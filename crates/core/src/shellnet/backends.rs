#[cfg(test)]
use super::client::{active_check, code_hash_check, is_uninit_account_404, ShellnetDoctorStatus};
use super::client::{RealChainBackend, SellerOfferEvents};
use super::contracts_provision::*;
use crate::chain::{
    check_buy_deposit_headroom, coalesce_equivalent_resting_asks, ChainBackend, ChainError,
    DealChainState, DealRole, DealView, ExecutableQuote, Match, MatchWatchCursor, MatchedFill,
    OrderBookOrder, OrderBookSnapshot, OrderBookStats, SellOffer, SellOfferOutcome, StreamSnapshot,
    TokenContract, MATCH_OPEN_TIMEOUT_SECS,
};
use crate::machine::Settlement;
use crate::manifest::model_hash_for;
use crate::note::{LocalNote, Note, NoteError, NotePubkey, Signature};
use crate::params::Shell;
use crate::settle::ProbeBurn;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
#[cfg(test)]
use gosh_ackinacki::airegistry::deploy::{build_deploy, local_context};
use gosh_ackinacki::sdk::{Address, KeyPair};
use serde_json::{json, Value};

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const POST_SELL_OFFER_SUBMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const OFFER_ACCEPTANCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const SELLER_READ_BACKOFF: [std::time::Duration; 4] = [
    std::time::Duration::from_millis(250),
    std::time::Duration::from_millis(500),
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(2),
];
const DUPLICATE_SELL_MESSAGE: &str = "this TokenContract already has a live resting SELL";

fn classify_seller_offer_outcome(
    events: SellerOfferEvents,
    matched_state: bool,
) -> Result<Option<SellOfferOutcome>, ChainError> {
    if events.matched || matched_state {
        return Ok(Some(SellOfferOutcome::Matched));
    }
    if let Some(order_id) = events.placed_order_id {
        return Ok(Some(SellOfferOutcome::Rested { order_id }));
    }
    if events.placement_value_returned {
        return Err(ChainError::DuplicateSell(
            DUPLICATE_SELL_MESSAGE.to_string(),
        ));
    }
    Ok(None)
}

fn normalized_hash_eq(left: &str, right: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .strip_prefix("0x")
            .or_else(|| s.trim().strip_prefix("0X"))
            .unwrap_or(s.trim())
            .to_ascii_lowercase()
    };
    norm(left) == norm(right)
}

struct ModelOnlyResumeFacts<'a> {
    state: Option<DealChainState>,
    model_name: Option<&'a str>,
    model_hash: Option<&'a str>,
    buyer_note: Option<&'a str>,
    buyer_pubkey: Option<&'a [u8; 32]>,
    order_book: Option<&'a str>,
}

fn validate_model_only_resume_facts(
    token_contract: &str,
    facts: ModelOnlyResumeFacts<'_>,
    expected_model_hash: &str,
    expected_buyer_note: &str,
    expected_buyer_pubkey: &[u8; 32],
    now: u64,
) -> Result<(), ChainError> {
    let state = facts.state.ok_or_else(|| {
        ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} is not active on-chain"
        ))
    })?;
    if !state.funded {
        return Err(ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} is not funded by-fact (funded=false)"
        )));
    }
    if state.disputed {
        return Err(ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} is disputed by-fact"
        )));
    }
    if state.probe_accepted && !state.opened {
        return Err(ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} has probeAccepted=true while opened=false"
        )));
    }
    if !state.opened {
        let funded_time = state.funded_time.ok_or_else(|| {
            ChainError::Chain(format!(
                "model-only resume: TokenContract {token_contract} is funded but getState has no fundedTime"
            ))
        })?;
        let cleanup_at = funded_time.saturating_add(MATCH_OPEN_TIMEOUT_SECS);
        if now >= cleanup_at {
            return Err(ChainError::Chain(format!(
                "model-only resume: TokenContract {token_contract} is stale never-opened by-fact \
                 (fundedTime {funded_time} + MATCH_OPEN_TIMEOUT {MATCH_OPEN_TIMEOUT_SECS} <= now {now}); \
                 run buyer recovery/cleanup instead of waiting for handover"
            )));
        }
    }

    let model_name = facts.model_name.ok_or_else(|| {
        ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} exposes no on-chain model name"
        ))
    })?;
    if !normalized_hash_eq(&model_hash_for(model_name), expected_model_hash) {
        return Err(ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} is for wrong model `{model_name}`"
        )));
    }
    let model_hash = facts.model_hash.ok_or_else(|| {
        ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} exposes no on-chain model hash"
        ))
    })?;
    if !normalized_hash_eq(model_hash, expected_model_hash) {
        return Err(ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} model_hash {model_hash} does not match \
             expected {expected_model_hash}"
        )));
    }

    let buyer_note = facts.buyer_note.ok_or_else(|| {
        ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} has no recorded buyer note"
        ))
    })?;
    let norm =
        |s: &str| crate::normalize_wallet_address(s).unwrap_or_else(|_| s.trim().to_string());
    if norm(buyer_note) != norm(expected_buyer_note) {
        return Err(ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} buyer note {buyer_note} is not this buyer note \
             {expected_buyer_note}"
        )));
    }
    let buyer_pubkey = facts.buyer_pubkey.ok_or_else(|| {
        ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} has no recorded buyer pubkey"
        ))
    })?;
    if buyer_pubkey != expected_buyer_pubkey {
        return Err(ChainError::Chain(format!(
            "model-only resume: TokenContract {token_contract} buyer pubkey is not this buyer key"
        )));
    }
    if facts.order_book.is_none() {
        return Err(ChainError::Chain(format!(
            "model-only resume: current model order book is not active for TokenContract {token_contract}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod model_only_resume_tests {
    use super::*;

    fn open_state() -> DealChainState {
        DealChainState {
            funded: true,
            opened: true,
            disputed: false,
            probe_accepted: false,
            funded_time: Some(1_000),
            last_advance: 1_010,
        }
    }

    fn facts<'a>(
        state: Option<DealChainState>,
        model_name: &'a str,
        model_hash: &'a str,
        buyer_note: &'a str,
        buyer_pubkey: &'a [u8; 32],
    ) -> ModelOnlyResumeFacts<'a> {
        ModelOnlyResumeFacts {
            state,
            model_name: Some(model_name),
            model_hash: Some(model_hash),
            buyer_note: Some(buyer_note),
            buyer_pubkey: Some(buyer_pubkey),
            order_book: Some("0:book"),
        }
    }

    #[test]
    fn model_only_resume_rejects_wrong_model() {
        let pk = [7u8; 32];
        let expected_hash = model_hash_for("qwen--qwen3--32b");
        let wrong_hash = model_hash_for("llama--llama3--8b");

        let err = validate_model_only_resume_facts(
            "0:tc",
            facts(
                Some(open_state()),
                "llama--llama3--8b",
                &wrong_hash,
                "0:buyer",
                &pk,
            ),
            &expected_hash,
            "0:buyer",
            &pk,
            1_100,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("wrong model"), "{err}");
    }

    #[test]
    fn model_only_resume_rejects_wrong_market_buyer_note() {
        let pk = [7u8; 32];
        let expected_hash = model_hash_for("qwen--qwen3--32b");

        let err = validate_model_only_resume_facts(
            "0:tc",
            facts(
                Some(open_state()),
                "qwen--qwen3--32b",
                &expected_hash,
                "0:other",
                &pk,
            ),
            &expected_hash,
            "0:buyer",
            &pk,
            1_100,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("buyer note"), "{err}");
        assert!(err.contains("not this buyer note"), "{err}");
    }

    #[test]
    fn model_only_resume_rejects_wrong_token_contract_state() {
        let pk = [7u8; 32];
        let expected_hash = model_hash_for("qwen--qwen3--32b");
        let mut unfunded = open_state();
        unfunded.funded = false;

        let err = validate_model_only_resume_facts(
            "0:tc",
            facts(
                Some(unfunded),
                "qwen--qwen3--32b",
                &expected_hash,
                "0:buyer",
                &pk,
            ),
            &expected_hash,
            "0:buyer",
            &pk,
            1_100,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("funded=false"), "{err}");
    }

    #[test]
    fn model_only_resume_rejects_stale_never_opened_match() {
        let pk = [7u8; 32];
        let expected_hash = model_hash_for("qwen--qwen3--32b");
        let mut stale = open_state();
        stale.opened = false;
        stale.funded_time = Some(1_000);

        let err = validate_model_only_resume_facts(
            "0:tc",
            facts(
                Some(stale),
                "qwen--qwen3--32b",
                &expected_hash,
                "0:buyer",
                &pk,
            ),
            &expected_hash,
            "0:buyer",
            &pk,
            1_000 + MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("stale never-opened"), "{err}");
    }
}

/// A note on top of `gosh.ackinacki`. The **ed25519 signature** of the chain/challenge -- from
/// the SDK [`KeyPair`] (the same pubkey is registered on-chain as `buyerPubkey`, against which the gateway checks
/// the signature). The **x25519 handover** -- on the dexdo crypto of([`LocalNote`]): the SDK
/// by-design does not expose X25519(the agent's root identity is a different layer). `pubkey()` carries both pubkeys, as in the mock.
pub struct RealNote {
    handover: LocalNote,
    keypair: KeyPair,
}

impl RealNote {
    /// A fresh note: ed25519 SDK `KeyPair`(signature/chain) + x25519 handover, **derived from
    /// it**(see `from_keypair`). A freshly generated `KeyPair` always carries a valid
    /// 32-byte ed25519 seed(the `KeyPair::generate` invariant), so the reconstruction does not fail.
    pub fn generate() -> Self {
        Self::from_keypair(KeyPair::generate())
            .expect("freshly generated SDK KeyPair carries a valid 32-byte ed25519 seed")
    }

    /// A note on a given ed25519 key(an on-chain actor).: the x25519 handover **is derived from
    /// ed25519**(Montgomery form), so that the seller reconstructs the buyer's pubkey from on-chain
    /// `getBuyerPubkey`(ed25519) -- no separate x25519 channel is needed. Requires a standard ed25519 seed
    /// for the SDK key(the invariant is pinned by the test `realnote_x25519_handover_derives_from_ed25519`).
    /// This is the **production path**(the actor loads the key from `--note-key`): the external secret may be malformed,
    /// so we return a typed [`NoteError::BadKey`] rather than panic.
    pub fn from_keypair(keypair: KeyPair) -> Result<Self, NoteError> {
        let secret = keypair.secret_hex();
        let bytes = decode_hex(secret.trim_start_matches("0x")).map_err(|_| NoteError::BadKey)?;
        if bytes.len() < 32 {
            return Err(NoteError::BadKey);
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes[..32]);
        let handover =
            LocalNote::from_ed25519_signing(ed25519_dalek::SigningKey::from_bytes(&seed));
        Ok(Self { handover, keypair })
    }

    /// A note from an SDK key's hex secret -- a convenience constructor for the CLI: the actor loads the owner key
    /// of the minted `PrivateNote` from `--note-key`. Builds a `KeyPair` from hex and derives the handover from ed25519.
    /// Malformed hex / non-ed25519 seed -> a typed [`NoteError::BadKey`](not a panic).
    pub fn from_secret_hex(secret_hex: &str) -> Result<Self, NoteError> {
        let keypair = KeyPair::from_secret_hex(secret_hex.trim()).map_err(|_| NoteError::BadKey)?;
        Self::from_keypair(keypair)
    }
}

impl Note for RealNote {
    fn pubkey(&self) -> NotePubkey {
        let bytes = decode_hex(self.keypair.public_hex().trim_start_matches("0x"))
            .expect("ed25519 public hex from SDK");
        let mut ed = [0u8; 32];
        ed.copy_from_slice(&bytes);
        NotePubkey {
            x: self.handover.pubkey().x,
            ed,
        }
    }

    fn encrypt_to(&self, peer: &NotePubkey, msg: &[u8]) -> Vec<u8> {
        self.handover.encrypt_to(peer, msg)
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, NoteError> {
        self.handover.decrypt(ciphertext)
    }

    fn sign(&self, msg: &[u8]) -> Signature {
        let sig = self.keypair.sign(msg).expect("ed25519 sign");
        let bytes = decode_hex(sig.hex().trim_start_matches("0x")).expect("signature hex from SDK");
        let mut arr = [0u8; 64];
        arr.copy_from_slice(&bytes);
        Signature(arr)
    }
}

/// The context of a SINGLE deal on the real chain for the [`RealDealBackend`] adapter: everything not present in
/// the mock form of the `ChainBackend` trait -- the book address, the actors' notes+keys(+ nonce), the buyer's
/// x25519 pubkey and the deal terms. The seller side is **note-funded**: the probe-commission is posted by
/// the seller note itself(`postProbeCommission` from its own ECC[2]) -- no operator wallet. Provisioned ahead of
/// time by [`RealChainBackend::provision_market`](note-funded), then placed here.
pub struct DealContext {
    pub order_book: Address,
    /// `modelHash`(uint256 hex) - buyer placement, book deployment, and getters use it.
    /// The 4.0.26 seller note instead derives the model and book from its runtime fields.
    pub model_hash: String,
    /// The seller's deal nonce: the `_nonce` static the per-deal `TokenContract` is deployed with and the
    /// only deal identifier forwarded by the client in 4.0.26 `note.postSellOffer(flags, nonce)`.
    pub nonce: u64,
    pub seller_note: Address,
    pub seller_keys: KeyPair,
    pub buyer_note: Address,
    pub buyer_keys: KeyPair,
    pub buyer_pubkey: NotePubkey,
    pub price_per_tick: u128,
    pub max_ticks: u128,
    /// How many ticks the buyer buys(budget/escrow for `placeInferenceBuy`).
    pub ticks: u128,
    pub escrow: u128,
    /// SHELL(ECC[2]) attached to `fundProbeCommission`(>= probe-commission; the excess is returned).
    pub probe_shell: u128,
}

/// A `ChainBackend` trait adapter on top of [`RealChainBackend`] for a SINGLE deal on shellnet
/// . The trait `token_contract: String` = the on-chain `TokenContract` address.
/// **Impedance**:
/// - `Shell`(u64) <- the raw on-chain value(testnet magnitudes fit);
/// - `Settlement`(`stop`/`seller_timeout`) is computed from the TC state **before** the call -- without events;
/// - `snapshot.burned`/`buyer_refunded` are not in `getState`(payout/burn are outside the getter) -- `0` in the snapshot;
/// the actual magnitudes are carried by `Settlement` from `stop`/`seller_timeout`.
pub struct RealDealBackend {
    chain: RealChainBackend,
    ctx: DealContext,
}

impl RealDealBackend {
    /// Assemble the adapter from an(already connected) low-level backend and a provisioned deal context.
    pub fn new(chain: RealChainBackend, ctx: DealContext) -> Self {
        Self { chain, ctx }
    }

    /// Wait for a boolean TC state flag. `submit` is asynchronous(the contract executes across blocks),
    /// so the trait's transition methods wait for the effect to be applied before returning(the trait's synchronous semantics).
    async fn wait_state_bool(&self, tc: &Address, key: &str, want: bool) -> Result<(), ChainError> {
        for _ in 0..40 {
            if let Some(st) = self.chain.token_contract_state(tc).await.map_err(map_err)? {
                if st[key].as_bool().unwrap_or(!want) == want {
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Err(ChainError::Chain(format!(
            "TC {tc}: field {key} != {want} within the allotted time"
        )))
    }

    async fn ensure_tc_gas(&self, tc: &Address) -> Result<(), ChainError> {
        self.chain
            .ensure_deal_contract_gas(
                &self.ctx.seller_note,
                &self.ctx.seller_keys,
                self.ctx.nonce,
                None,
                Some(tc),
            )
            .await
            .map_err(map_err)
    }
}

/// `(opened, probeAccepted, prepaid, frozen, deposit, probeLocked)` from the TC -- for computing
/// `Settlement`/the snapshot.
async fn tc_settle_state(
    chain: &RealChainBackend,
    tc: &Address,
) -> Result<(bool, bool, u128, u128, u128, u128)> {
    let st = chain
        .token_contract_state(tc)
        .await?
        .ok_or_else(|| anyhow!("TC is not active"))?;
    let pr = chain.token_contract_probe(tc).await?;
    let g = |s: &Value, k: &str| {
        s[k].as_str()
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0)
    };
    Ok((
        st["opened"].as_bool().unwrap_or(false),
        st["probeAccepted"].as_bool().unwrap_or(false),
        g(&st, "prepaid"),
        g(&st, "frozen"),
        g(&st, "deposit"),
        pr.as_ref().map(|p| g(p, "probeLocked")).unwrap_or(0),
    ))
}

fn reqwest_error_is_transport(error: &reqwest::Error) -> bool {
    error.is_connect()
        || error.is_timeout()
        || error.is_body()
        || error
            .status()
            .is_some_and(|status| status.is_server_error() || status.as_u16() == 429)
}

fn message_is_contract_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let has_exit_code = lower.contains("exit_code=")
        || lower.contains("exit_code ")
        || lower.contains("exit code:")
        || lower.contains("exit code ");
    let block_manager_compute_revert = lower.contains("block manager rejected message")
        && (lower.contains("tvm_error") || lower.contains("compute phase"));
    let explicit_onchain_failure = lower.contains("on-chain revert")
        || lower.contains("on-chain submit failed")
        || lower.contains("action_result_code=");

    // Some SDK paths expose only the named contract error. Do not turn our own fail-closed
    // preflight explanations(which describe what *would* revert) into chain results.
    let named_contract_error = message.contains("ERR_")
        && !lower.contains("would revert")
        && !lower.contains("would fail")
        && !lower.contains("pre-accept")
        && !lower.contains("refusing")
        && (lower.trim_start().starts_with("err_")
            || lower.contains("revert")
            || lower.contains("rejected")
            || lower.contains("failed"));

    has_exit_code
        || block_manager_compute_revert
        || explicit_onchain_failure
        || named_contract_error
}

fn map_err(error: anyhow::Error) -> ChainError {
    // Alternate Display preserves every anyhow context and source, including the reqwest
    // cause or the contract exit code, in the user-visible machine error.
    let message = format!("{error:#}");
    if let Some(outcome) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::MoneySubmitError>())
    {
        return match outcome {
            crate::MoneySubmitError::Preparation { .. } => {
                ChainError::MoneySubmitPreparation(message)
            }
            crate::MoneySubmitError::Ambiguous { .. } => ChainError::AmbiguousSubmit(format!(
                "{message}; the BOC was not retried; reconcile from chain facts before any resubmit"
            )),
            crate::MoneySubmitError::Rejected { .. } => ChainError::MoneySubmitRejected(message),
        };
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest_error_is_transport)
    }) {
        ChainError::Transport(message)
    } else if message_is_contract_failure(&message) {
        ChainError::Contract(message)
    } else {
        ChainError::Chain(message)
    }
}

async fn retry_seller_read<T, F, Fut>(label: &str, mut read: F) -> Result<T, ChainError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ChainError>>,
{
    for (attempt, delay) in SELLER_READ_BACKOFF.iter().enumerate() {
        match read().await {
            Ok(value) => return Ok(value),
            Err(ChainError::Transport(error)) => {
                tracing::warn!(
                    read = label,
                    attempt = attempt + 1,
                    backoff_ms = delay.as_millis(),
                    error,
                    "transient seller chain read failed; retrying"
                );
                tokio::time::sleep(*delay).await;
            }
            Err(error) => return Err(error),
        }
    }
    read().await
}

#[cfg(test)]
mod shellnet_error_mapping_tests {
    use super::*;

    fn http_status_error(status: reqwest::StatusCode) -> anyhow::Error {
        let response: reqwest::Response = http::Response::builder()
            .status(status)
            .body(Vec::<u8>::new())
            .expect("build HTTP response")
            .into();
        anyhow::Error::new(
            response
                .error_for_status()
                .expect_err("status must produce reqwest error"),
        )
    }

    #[test]
    fn buyer_adapter_preserves_transport_and_contract_causes() {
        for status in [
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let mapped = map_err(http_status_error(status));
            let ChainError::Transport(cause) = mapped else {
                panic!("HTTP {status} must map to ChainError::Transport");
            };
            assert!(cause.contains(status.as_str()), "{cause}");
            assert!(!cause.contains("CHAIN_TRANSPORT"), "{cause}");
        }

        let submit_error = crate::onchain_diagnostics::validate_onchain_submit_response(json!({
            "error": {
                "code": "TVM_ERROR",
                "message": "compute phase failed",
                "data": { "exit_code": 321 }
            }
        }))
        .expect_err("contract revert must fail submit validation");
        let mapped = map_err(anyhow::Error::new(submit_error));
        let ChainError::Contract(cause) = mapped else {
            panic!("contract revert must map to ChainError::Contract");
        };
        assert!(cause.contains("compute phase failed"), "{cause}");
        assert!(cause.contains("exit_code=321"), "{cause}");
        assert!(cause.contains("ERR_ALREADY_OPEN"), "{cause}");

        let unknown = map_err(anyhow!("buyer adapter invariant failed"));
        assert!(matches!(unknown, ChainError::Chain(_)));

        let preflight = map_err(anyhow!(
            "buyer aborted pre-accept: placeInferenceBuy would revert ERR_INVALID_SENDER 101"
        ));
        assert!(matches!(preflight, ChainError::Chain(_)));
    }
}

fn parse_tc(tc: &TokenContract) -> Result<Address, ChainError> {
    Address::parse(tc).map_err(|e| ChainError::Chain(format!("bad token_contract {tc}: {e}")))
}

fn probe_is_funded(probe: &Value) -> bool {
    probe["probeFunded"].as_bool().unwrap_or(false)
}

fn probe_required_commission(
    token_contract: &TokenContract,
    probe: &Value,
) -> Result<u128, ChainError> {
    let raw = probe.get("probeCommission").ok_or_else(|| {
        ChainError::Chain(format!(
            "TokenContract {token_contract}: getProbe().probeCommission is missing while \
             probeFunded=false; refusing postProbeCommission before money moves because a missing \
             contract getter value must not be inferred as 0"
        ))
    })?;
    let raw = raw.as_str().ok_or_else(|| {
        ChainError::Chain(format!(
            "TokenContract {token_contract}: getProbe().probeCommission is not a string ({raw:?}) \
             while probeFunded=false; refusing postProbeCommission before money moves because a malformed \
             contract getter value must not be inferred as 0"
        ))
    })?;
    raw.parse::<u128>().map_err(|e| {
        ChainError::Chain(format!(
            "TokenContract {token_contract}: getProbe().probeCommission value {raw:?} is malformed: {e}; \
             refusing postProbeCommission before money moves because a malformed contract getter value \
             must not be inferred as 0"
        ))
    })
}

fn probe_post_amount(
    token_contract: &TokenContract,
    probe: &Value,
    max_post_amount: u128,
) -> Result<u128, ChainError> {
    let required = probe_required_commission(token_contract, probe)?;
    if max_post_amount < required {
        return Err(ChainError::Chain(format!(
            "TokenContract {token_contract}: --probe-shell raw ECC[2] limit {max_post_amount} is below \
             getProbe().probeCommission {required}; refusing TokenContract.open because open() would revert with \
             airegistry::ERR_PROBE_NOT_FUNDED (332)"
        )));
    }
    Ok(required)
}

fn probe_not_funded_after_post_reason(
    token_contract: &TokenContract,
    seller_note: &Address,
    post_amount: u128,
    note_physical_shell: u128,
    state: Option<&Value>,
    probe: Option<&Value>,
) -> String {
    format!(
        "TokenContract {token_contract}: postProbeCommission submitted but getProbe().probeFunded stayed false; \
         refusing TokenContract.open because open() would revert with airegistry::ERR_PROBE_NOT_FUNDED (332). \
         seller note {seller_note} physical ECC[2] SHELL after submit={note_physical_shell}, \
         posted_amount={post_amount}, state={state:?}, probe={probe:?}. \
         Re-mint/fund the seller note with physical SHELL ECC[2] (`mint_pn_pool --deposit-shells ...` / current \
         onboarding) or lower --probe-shell only if it still covers getProbe().probeCommission."
    )
}

async fn seller_note_physical_shell(
    chain: &RealChainBackend,
    seller_note: &Address,
) -> Result<u128, ChainError> {
    let acc = chain
        .client()
        .get_account(seller_note)
        .await
        .map_err(map_err)?
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "seller note {seller_note} disappeared before postProbeCommission"
            ))
        })?;
    Ok(acc.ecc_balance(2))
}

async fn post_probe_commission_and_wait(
    chain: &RealChainBackend,
    seller_note: &Address,
    seller_keys: &KeyPair,
    nonce: u64,
    token_contract: &TokenContract,
    tc: &Address,
    max_post_amount: u128,
) -> Result<(), ChainError> {
    let probe_before = chain
        .token_contract_probe(tc)
        .await
        .map_err(map_err)?
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "TokenContract {token_contract}: getProbe() returned no data before postProbeCommission"
            ))
        })?;
    if probe_is_funded(&probe_before) {
        return Ok(());
    }

    let post_amount = probe_post_amount(token_contract, &probe_before, max_post_amount)?;

    let note_physical_shell = seller_note_physical_shell(chain, seller_note).await?;
    if note_physical_shell < post_amount {
        return Err(ChainError::Chain(format!(
            "seller note {seller_note} has physical ECC[2] SHELL raw units {note_physical_shell}, below \
             required probe commission {post_amount} for TokenContract {token_contract} \
             (--probe-shell raw limit={max_post_amount}, getProbe().probeCommission={post_amount}); \
             postProbeCommission cannot fund the probe, and TokenContract.open would revert with \
             airegistry::ERR_PROBE_NOT_FUNDED (332). Re-mint/fund the seller note with physical SHELL ECC[2] \
             (`mint_pn_pool` / current onboarding) and keep provision --deposit-shells low enough to leave this \
             exact probe reserve."
        )));
    }

    chain
        .note_post_probe_commission(seller_note, seller_keys, nonce, post_amount)
        .await
        .map_err(map_err)?;
    for _ in 0..20 {
        if chain
            .token_contract_probe(tc)
            .await
            .map_err(map_err)?
            .as_ref()
            .map(probe_is_funded)
            .unwrap_or(false)
        {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    let state = chain.token_contract_state(tc).await.map_err(map_err)?;
    let probe = chain.token_contract_probe(tc).await.map_err(map_err)?;
    let note_physical_shell = seller_note_physical_shell(chain, seller_note)
        .await
        .unwrap_or(0);
    Err(ChainError::Chain(probe_not_funded_after_post_reason(
        token_contract,
        seller_note,
        post_amount,
        note_physical_shell,
        state.as_ref(),
        probe.as_ref(),
    )))
}

#[cfg(test)]
mod probe_open_guard_tests {
    use super::*;

    #[test]
    fn probe_getter_helpers_parse_funded_and_commission() {
        let probe = json!({
            "probeFunded": false,
            "probeLocked": "0",
            "probeCommission": "25"
        });
        assert!(!probe_is_funded(&probe));
        assert_eq!(
            probe_required_commission(&"0:tc".to_string(), &probe).unwrap(),
            25
        );

        let funded = json!({"probeFunded": true, "probeCommission": "25"});
        assert!(probe_is_funded(&funded));
    }

    #[test]
    fn probe_post_amount_uses_exact_contract_commission_under_limit() {
        let tc = "0:9754c903354dfba45c66898e5fcb840c23a892e0829906bea1b554c15b6d7c8c".to_string();
        let probe = json!({
            "probeFunded": false,
            "probeLocked": "0",
            "probeCommission": "25"
        });

        assert_eq!(
            probe_post_amount(&tc, &probe, 1_000_000).expect("default limit covers commission"),
            25
        );
        let err = probe_post_amount(&tc, &probe, 24).expect_err("below contract commission");
        let reason = err.to_string();
        assert!(
            reason.contains("--probe-shell raw ECC[2] limit 24 is below"),
            "{reason}"
        );
        assert!(reason.contains("ERR_PROBE_NOT_FUNDED (332)"), "{reason}");
    }

    #[test]
    fn probe_post_amount_allows_explicit_zero_commission_only() {
        let tc = "0:9754c903354dfba45c66898e5fcb840c23a892e0829906bea1b554c15b6d7c8c".to_string();
        let probe = json!({
            "probeFunded": false,
            "probeLocked": "0",
            "probeCommission": "0"
        });

        assert_eq!(
            probe_post_amount(&tc, &probe, 1_000_000).expect("explicit zero is a contract value"),
            0
        );
    }

    #[test]
    fn probe_post_amount_fails_closed_when_commission_missing() {
        let tc = "0:9754c903354dfba45c66898e5fcb840c23a892e0829906bea1b554c15b6d7c8c".to_string();
        let probe = json!({
            "probeFunded": false,
            "probeLocked": "0"
        });

        let err = probe_post_amount(&tc, &probe, 1_000_000).expect_err("missing commission");
        let reason = err.to_string();
        assert!(reason.contains("probeCommission is missing"), "{reason}");
        assert!(reason.contains("must not be inferred as 0"), "{reason}");
        assert!(reason.contains("refusing postProbeCommission"), "{reason}");
    }

    #[test]
    fn probe_post_amount_fails_closed_when_commission_non_string() {
        let tc = "0:9754c903354dfba45c66898e5fcb840c23a892e0829906bea1b554c15b6d7c8c".to_string();
        let probe = json!({
            "probeFunded": false,
            "probeLocked": "0",
            "probeCommission": 25
        });

        let err = probe_post_amount(&tc, &probe, 1_000_000).expect_err("non-string commission");
        let reason = err.to_string();
        assert!(
            reason.contains("probeCommission is not a string"),
            "{reason}"
        );
        assert!(reason.contains("must not be inferred as 0"), "{reason}");
        assert!(reason.contains("refusing postProbeCommission"), "{reason}");
    }

    #[test]
    fn probe_post_amount_fails_closed_when_commission_malformed() {
        let tc = "0:9754c903354dfba45c66898e5fcb840c23a892e0829906bea1b554c15b6d7c8c".to_string();
        let probe = json!({
            "probeFunded": false,
            "probeLocked": "0",
            "probeCommission": "not-a-number"
        });

        let err = probe_post_amount(&tc, &probe, 1_000_000).expect_err("malformed commission");
        let reason = err.to_string();
        assert!(reason.contains("probeCommission value"), "{reason}");
        assert!(reason.contains("malformed"), "{reason}");
        assert!(reason.contains("must not be inferred as 0"), "{reason}");
        assert!(reason.contains("refusing postProbeCommission"), "{reason}");
    }

    #[test]
    fn probe_not_funded_reason_names_open_revert_code() {
        let seller_note =
            Address::parse("0:d154e18f92f422b3879ee860842f3bbe634fc95be8e595bce009de00acdb61d2")
                .expect("seller note");
        let state = json!({
            "funded": true,
            "opened": false,
            "deposit": "2050"
        });
        let probe = json!({
            "probeFunded": false,
            "probeLocked": "0",
            "probeCommission": "25"
        });
        let reason = probe_not_funded_after_post_reason(
            &"0:9754c903354dfba45c66898e5fcb840c23a892e0829906bea1b554c15b6d7c8c".to_string(),
            &seller_note,
            1_000_000,
            0,
            Some(&state),
            Some(&probe),
        );
        assert!(reason.contains("ERR_PROBE_NOT_FUNDED (332)"), "{reason}");
        assert!(reason.contains("probeFunded"), "{reason}");
        assert!(
            reason.contains("physical ECC[2] SHELL after submit=0"),
            "{reason}"
        );
    }
}

fn u64_json_field(state: &Value, key: &str) -> Option<u64> {
    state[key].as_str().and_then(|x| x.parse::<u64>().ok())
}

fn deal_chain_state_from_json(state: &Value) -> DealChainState {
    DealChainState {
        funded: state["funded"].as_bool().unwrap_or(false),
        opened: state["opened"].as_bool().unwrap_or(false),
        disputed: state["disputed"].as_bool().unwrap_or(false),
        probe_accepted: state["probeAccepted"].as_bool().unwrap_or(false),
        funded_time: u64_json_field(state, "fundedTime").filter(|v| *v > 0),
        last_advance: u64_json_field(state, "lastAdvance").unwrap_or(0),
    }
}

fn parse_order_u128(s: &str) -> Option<u128> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u128>().ok()
    }
}

fn order_u128(order: &Value, keys: &[&str]) -> Option<u128> {
    keys.iter().find_map(|k| {
        order[*k]
            .as_str()
            .and_then(parse_order_u128)
            .or_else(|| order[*k].as_u64().map(u128::from))
    })
}

fn order_u64(order: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        order[*k]
            .as_str()
            .and_then(|s| parse_order_u128(s).and_then(|v| v.try_into().ok()))
            .or_else(|| order[*k].as_u64())
    })
}

fn zero_address_like(addr: &str) -> bool {
    addr.trim_start_matches(['0', ':', 'x']).is_empty()
}

enum Uint256ToU128 {
    Value(u128),
    ExceedsU128,
    Invalid,
}

fn parse_uint256_to_u128(value: &str) -> Uint256ToU128 {
    const U256_MAX_DECIMAL: &str =
        "115792089237316195423570985008687907853269984665640564039457584007913129639935";

    let value = value.trim();
    let (digits, radix, max_digits) = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (hex, 16, 64)
    } else {
        (value, 10, U256_MAX_DECIMAL.len())
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Uint256ToU128::Invalid;
    }

    let significant = digits.trim_start_matches('0');
    let significant = if significant.is_empty() {
        "0"
    } else {
        significant
    };
    let within_uint256 = if significant.len() < max_digits {
        true
    } else if significant.len() > max_digits {
        false
    } else if radix == 16 {
        true
    } else {
        significant <= U256_MAX_DECIMAL
    };
    if !within_uint256 {
        return Uint256ToU128::Invalid;
    }

    match u128::from_str_radix(significant, radix) {
        Ok(value) => Uint256ToU128::Value(value),
        Err(_) => Uint256ToU128::ExceedsU128,
    }
}

fn orderbook_order_from_getter(order_id: u128, order: &Value) -> Result<Option<OrderBookOrder>> {
    let ticks = order_u128(order, &["amount"])
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid amount: {order}"))?;
    // A zero-tick slot is not a live, matchable order: either a cleanly removed slot
    // (`_removeFromBook` -> `delete _orders[id]`, all fields zero) or an order filled /
    // consumed to zero remaining ticks but not yet swept from the book (its owner note can
    // linger until a `cancelInferenceOrder`). Neither can be matched, so skip it rather than
    // letting a strict parse of a lingering filled order abort the whole book scan.
    if ticks == 0 {
        return Ok(None);
    }
    let owner_note = order["note"]
        .as_str()
        .filter(|note| !note.trim().is_empty())
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid note: {order}"))?
        .to_string();
    // A non-zero amount with a zero/absent owner note is genuinely malformed (ticks with no
    // owner) -- keep it fail-loud.
    if zero_address_like(&owner_note) {
        return Err(anyhow!(
            "getOrder({order_id}) malformed: non-zero amount with zero owner note: {order}"
        ));
    }
    let is_buy = order["isBuy"]
        .as_bool()
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid isBuy: {order}"))?;
    let token_contract = order["tokenContract"]
        .as_str()
        .filter(|token_contract| !token_contract.trim().is_empty())
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid tokenContract: {order}"))?;
    let token_contract = if zero_address_like(token_contract) {
        None
    } else {
        Some(token_contract.to_string())
    };
    let price_per_tick = match order["price"].as_str() {
        Some(price) => match parse_uint256_to_u128(price) {
            Uint256ToU128::Value(price) => price,
            Uint256ToU128::ExceedsU128 => {
                return Err(anyhow!(
                    "getOrder({order_id}) price exceeds downstream u128: {order}"
                ));
            }
            Uint256ToU128::Invalid => {
                return Err(anyhow!(
                    "getOrder({order_id}) missing/invalid price: {order}"
                ));
            }
        },
        None => order["price"]
            .as_u64()
            .map(u128::from)
            .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid price: {order}"))?,
    };
    let escrow = order_u128(order, &["escrow"])
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid escrow: {order}"))?;
    let deadline = order_u64(order, &["deadline"])
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid deadline: {order}"))?;
    let flags = order_u64(order, &["flags"])
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid flags: {order}"))?
        .try_into()
        .map_err(|_| anyhow!("getOrder({order_id}) flags exceed uint8: {order}"))?;
    let timestamp = order_u64(order, &["ts"])
        .ok_or_else(|| anyhow!("getOrder({order_id}) missing/invalid ts: {order}"))?;
    Ok(Some(OrderBookOrder {
        order_id,
        owner_note,
        token_contract,
        is_buy,
        price_per_tick,
        ticks,
        escrow,
        deadline,
        flags,
        timestamp,
    }))
}

/// Build the live-order list from raw per-id `getOrder` reads, skipping empty/filled slots
/// (`Ok(None)`) and lingering/unparseable slots(`Err`, logged) so one non-live or corrupt
/// order never blinds the whole book scan. Transport/chain read errors are surfaced by
/// the caller before the raw values reach here.
fn collect_live_orders(raw: impl IntoIterator<Item = (u128, Value)>) -> Vec<OrderBookOrder> {
    let mut orders = Vec::new();
    for (id, order) in raw {
        match orderbook_order_from_getter(id, &order) {
            Ok(Some(parsed)) => orders.push(parsed),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(order_id = id, error = %format!("{error:#}"), "skipping unparseable order in book scan");
            }
        }
    }
    orders
}

#[cfg(test)]
fn resting_ask_from_order(order_id: u128, order: &Value) -> Option<OrderBookOrder> {
    orderbook_order_from_getter(order_id, order)
        .expect("valid getOrder fixture")
        .filter(|o| o.is_resting_ask())
}

fn next_matching_ask(
    asks: &[OrderBookOrder],
    max_price_per_tick: u128,
    _ticks: u128,
) -> Option<&OrderBookOrder> {
    asks.iter()
        .filter(|ask| ask.price_per_tick <= max_price_per_tick)
        .min_by_key(|ask| (ask.price_per_tick, ask.order_id))
}

fn no_matching_ask_reason(
    asks: &[OrderBookOrder],
    max_price_per_tick: u128,
    ticks: u128,
) -> String {
    match asks.iter().min_by_key(|ask| (ask.price_per_tick, ask.order_id)) {
        Some(best) if best.price_per_tick > max_price_per_tick => format!(
            "best ask price {} is above buyer max_price_per_tick {max_price_per_tick}; requested ticks {ticks}. \
             Raise --max-price-per-tick to at least {} or wait for a cheaper ask",
            best.price_per_tick, best.price_per_tick
        ),
        Some(best) => format!(
            "no matchable ask for max_price_per_tick {max_price_per_tick}, requested ticks {ticks}. \
             Best ask is order #{} tokenContract {} (price {}, ticks {})",
            best.order_id,
            best.token_contract.as_deref().unwrap_or("<none>"),
            best.price_per_tick,
            best.ticks
        ),
        None => format!(
            "no resting asks in this model book for max_price_per_tick {max_price_per_tick}, requested ticks {ticks}"
        ),
    }
}

fn check_single_head_capacity(ask: &OrderBookOrder, ticks: u128) -> Result<(), String> {
    if ask.ticks >= ticks {
        return Ok(());
    }
    Err(format!(
        "refusing multi-ask fill: order #{} tokenContract {} has only {} ticks, buyer requested {ticks}. \
         Current shellnet submit is accepted only when the price/time head ask alone covers the request.",
        ask.order_id,
        ask.token_contract.as_deref().unwrap_or("<none>"),
        ask.ticks,
    ))
}

#[cfg(test)]
fn check_model_buy_full_fill(
    asks: &[OrderBookOrder],
    max_price_per_tick: u128,
    ticks: u128,
) -> Result<(), String> {
    selected_model_buy_ask(asks, max_price_per_tick, ticks).map(|_| ())
}

fn selected_model_buy_ask(
    asks: &[OrderBookOrder],
    max_price_per_tick: u128,
    ticks: u128,
) -> Result<OrderBookOrder, String> {
    let asks = coalesce_equivalent_resting_asks(asks)?;
    let Some(best) = next_matching_ask(&asks, max_price_per_tick, ticks) else {
        return Err(no_matching_ask_reason(&asks, max_price_per_tick, ticks));
    };
    check_single_head_capacity(best, ticks)?;
    Ok(best.clone())
}

fn describe_buy_ask(ask: &OrderBookOrder) -> String {
    format!(
        "order #{} tokenContract {} (price {}, ticks {})",
        ask.order_id,
        ask.token_contract.as_deref().unwrap_or("<none>"),
        ask.price_per_tick,
        ask.ticks
    )
}

fn selected_model_buy_ask_matching_executable_depth(
    raw_asks: &[OrderBookOrder],
    executable_asks: &[OrderBookOrder],
    max_price_per_tick: u128,
    ticks: u128,
) -> Result<OrderBookOrder, String> {
    let raw_asks = coalesce_equivalent_resting_asks(raw_asks)?;
    let raw_selected = selected_model_buy_ask(&raw_asks, max_price_per_tick, ticks).map_err(|e| {
        format!(
            "raw order-book matcher has no submit-safe ask: {e}. Retry after the seller posts a fresh ask with enough ticks, \
             or clean/cancel stale order-book rows if you operate this market"
        )
    })?;
    let executable_selected =
        selected_model_buy_ask(executable_asks, max_price_per_tick, ticks).map_err(|e| {
            format!(
                "raw order-book matcher would select {}, but executable-depth check has no matching ask: {e}. \
                 Refusing to send escrow while stale/unreadable rows block the real matcher",
                describe_buy_ask(&raw_selected)
            )
        })?;
    let same_tc = raw_selected
        .token_contract
        .as_deref()
        .zip(executable_selected.token_contract.as_deref())
        .is_some_and(|(raw, executable)| raw.eq_ignore_ascii_case(executable));
    if !same_tc || raw_selected.order_id != executable_selected.order_id {
        return Err(format!(
            "raw order-book matcher would select {}, but executable quote selected {}. \
             Refusing to send escrow while stale/unreadable rows block the real matcher",
            describe_buy_ask(&raw_selected),
            describe_buy_ask(&executable_selected)
        ));
    }
    Ok(executable_selected)
}

fn same_token_contract(left: &OrderBookOrder, right: &OrderBookOrder) -> bool {
    left.token_contract
        .as_deref()
        .zip(right.token_contract.as_deref())
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn submit_safe_executable_book_asks(
    raw_asks: &[OrderBookOrder],
    executable_asks: &[OrderBookOrder],
    max_price_per_tick: u128,
    ticks: u128,
) -> Result<(Vec<OrderBookOrder>, Option<String>), String> {
    enum ListingBlocker {
        NonExecutable(OrderBookOrder),
        InsufficientHead(OrderBookOrder),
    }

    let raw_asks = coalesce_equivalent_resting_asks(raw_asks)?;
    let executable_asks = coalesce_equivalent_resting_asks(executable_asks)?;
    let mut rows = Vec::new();
    let mut blocker = None;

    for raw in raw_asks
        .iter()
        .filter(|ask| ask.price_per_tick <= max_price_per_tick)
    {
        let Some(executable) = executable_asks
            .iter()
            .find(|executable| same_token_contract(raw, executable))
        else {
            blocker = Some(ListingBlocker::NonExecutable(raw.clone()));
            break;
        };
        if executable.ticks >= ticks {
            rows.push(executable.clone());
        } else {
            blocker = Some(ListingBlocker::InsufficientHead(executable.clone()));
            break;
        }
    }

    if !rows.is_empty() {
        return Ok((rows, None));
    }

    let reason = if let Some(blocker) = blocker {
        match blocker {
            ListingBlocker::NonExecutable(blocker) => format!(
                "raw order-book matcher would hit non-executable {} before any later executable ask. \
                 Refusing to list stale/unreadable-blocked rows",
                describe_buy_ask(&blocker)
            ),
            ListingBlocker::InsufficientHead(blocker) => {
                let capacity = check_single_head_capacity(&blocker, ticks)
                    .expect_err("insufficient head blocker was checked before listing");
                format!(
                    "raw order-book matcher would hit executable but insufficient head {} before any later \
                     executable ask: {capacity}. Refusing to list rows the model-wide matcher cannot reach",
                    describe_buy_ask(&blocker)
                )
            }
        }
    } else if raw_asks
        .iter()
        .all(|ask| ask.price_per_tick > max_price_per_tick)
    {
        no_matching_ask_reason(&raw_asks, max_price_per_tick, ticks)
    } else if let Some(best) = executable_asks
        .iter()
        .filter(|ask| ask.price_per_tick <= max_price_per_tick)
        .min_by_key(|ask| (ask.price_per_tick, ask.order_id))
    {
        format!(
            "no executable ask has at least requested ticks {ticks}. Best executable ask is {}",
            describe_buy_ask(best)
        )
    } else {
        format!(
            "no executable matching ask for max_price_per_tick {max_price_per_tick}, requested ticks {ticks}. \
             The raw book has crossing rows, but none are live, funded, fresh, and unblocked"
        )
    };
    Ok((Vec::new(), Some(reason)))
}

fn orderbook_stats_for_error(snapshot: &OrderBookSnapshot) -> String {
    snapshot
        .stats
        .as_ref()
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "<book not active>".to_string())
}

fn check_expected_buy_target(
    asks: &[OrderBookOrder],
    expected_tc_lower: &str,
    max_price_per_tick: u128,
    ticks: u128,
) -> Result<(), String> {
    let asks = coalesce_equivalent_resting_asks(asks)?;
    let expected = asks.iter().find(|ask| {
        ask.token_contract
            .as_deref()
            .is_some_and(|tc| tc.eq_ignore_ascii_case(expected_tc_lower))
    });
    let Some(best) = next_matching_ask(&asks, max_price_per_tick, ticks) else {
        return Err(match expected {
            Some(ask) => format!(
                "the expected ask exists but is not matchable by this buy: tokenContract {}, price {}, ticks {}, \
                 buyer max_price_per_tick {max_price_per_tick}, requested ticks {ticks}",
                ask.token_contract.as_deref().unwrap_or("<none>"), ask.price_per_tick, ask.ticks,
            ),
            None => format!(
                "no resting ask for expected tokenContract {expected_tc_lower}, and no matchable ask for \
                 max_price_per_tick {max_price_per_tick}, requested ticks {ticks}"
            ),
        });
    };
    if best
        .token_contract
        .as_deref()
        .is_some_and(|tc| tc.eq_ignore_ascii_case(expected_tc_lower))
    {
        check_single_head_capacity(best, ticks)?;
        return Ok(());
    }
    Err(match expected {
        Some(ask) => format!(
            "placeInferenceBuy cannot target a TokenContract; the shared model book would match order #{} \
             tokenContract {} (price {}, ticks {}) before expected tokenContract {} (order #{}, price {}, ticks {}). \
             Refusing to send escrow into the wrong deal; buy the best matching market or clear/cancel the \
             earlier ask first",
            best.order_id,
            best.token_contract.as_deref().unwrap_or("<none>"),
            best.price_per_tick,
            best.ticks,
            ask.token_contract.as_deref().unwrap_or("<none>"),
            ask.order_id,
            ask.price_per_tick,
            ask.ticks,
        ),
        None => format!(
            "no resting ask for expected tokenContract {expected_tc_lower}; the shared model book would match \
             order #{} tokenContract {} (price {}, ticks {}) instead. Refusing to send escrow into the wrong deal",
            best.order_id, best.token_contract.as_deref().unwrap_or("<none>"), best.price_per_tick, best.ticks,
        ),
    })
}

#[allow(clippy::too_many_arguments)]
fn seller_post_sell_offer_timeout_message(
    ob: &Address,
    token_contract: &str,
    model_hash: &str,
    nonce: u64,
    seller_note: &Address,
    timeout: std::time::Duration,
    canonical_evidence: &str,
    tc_state_evidence: &str,
) -> String {
    format!(
        "seller postSellOffer submit timed out after {}s before shellnet returned an accepted/rejected \
         /v2/messages response; no message_hash/tx_hash is available. InferenceOrderBook {ob} model_hash={model_hash} \
         nonce={nonce} seller_note={seller_note} token_contract={token_contract}. {canonical_evidence}. \
         {tc_state_evidence}. This is  submit-timeout evidence; retry may be safe only after checking \
         whether the chain later shows a matching message/order for this exact TC.",
        timeout.as_secs()
    )
}

fn orderbook_stats_from_getter(stats: &Value) -> OrderBookStats {
    OrderBookStats {
        next_order_id: order_u128(stats, &["nextOrderId"]).unwrap_or(0),
        order_count: order_u128(stats, &["orderCount"]).unwrap_or(0),
        executed_notional: order_u128(stats, &["executedNotional"]).unwrap_or(0),
        executed_ticks: order_u128(stats, &["executedTicks"]).unwrap_or(0),
    }
}

#[cfg(test)]
mod offer_rested_match_tests {
    use super::{
        check_expected_buy_target, check_model_buy_full_fill, collect_live_orders,
        executable_resting_asks_by_state, next_matching_ask, orderbook_order_from_getter,
        resting_ask_from_order, selected_model_buy_ask,
        selected_model_buy_ask_matching_executable_depth, submit_safe_executable_book_asks,
    };
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    fn parsed_ask(
        order_id: u128,
        token_contract: &str,
        price: u128,
        amount: u128,
    ) -> crate::chain::OrderBookOrder {
        resting_ask_from_order(
            order_id,
            &json!({
                "note": "0:seller",
                "tokenContract": token_contract,
                "price": price.to_string(),
                "amount": amount.to_string(),
                "escrow": "0",
                "deadline": "0",
                "flags": "0",
                "ts": "0",
                "isBuy": false
            }),
        )
        .unwrap()
    }

    fn fresh_tc_state() -> Value {
        json!({"funded": false, "opened": false, "probeAccepted": false, "disputed": false,
            "deposit": "0", "prepaid": "0", "frozen": "0", "finalizedOwed": "0"})
    }

    fn used_tc_state() -> Value {
        json!({"funded": true, "opened": false, "probeAccepted": false, "disputed": false,
            "deposit": "104448", "prepaid": "0", "frozen": "0", "finalizedOwed": "0"})
    }

    #[test]
    fn order_parser_decodes_every_complete_get_order_abi_field() {
        let order = orderbook_order_from_getter(
            7,
            &json!({
                "note": "0:seller",
                "tokenContract": "0:tc",
                "price": "1000",
                "amount": "1024",
                "escrow": "2048",
                "deadline": "1712345678",
                "flags": "3",
                "ts": "1712000000",
                "isBuy": false
            }),
        )
        .expect("complete getOrder ABI fixture should decode")
        .expect("complete order should be present");
        assert_eq!(order.order_id, 7);
        assert_eq!(order.owner_note, "0:seller");
        assert_eq!(order.token_contract.as_deref(), Some("0:tc"));
        assert!(!order.is_buy);
        assert_eq!(order.price_per_tick, 1000);
        assert_eq!(order.ticks, 1024);
        assert_eq!(order.escrow, 2048);
        assert_eq!(order.deadline, 1_712_345_678);
        assert_eq!(order.flags, 3);
        assert_eq!(order.timestamp, 1_712_000_000);
    }

    #[test]
    fn order_parser_accepts_live_hex_numeric_getter_values() {
        let ask = resting_ask_from_order(
            8,
            &json!({
                "note": "0:seller",
                "tokenContract": "0:tc",
                "price": "0x2710",
                "amount": "0x400",
                "escrow": "0x0",
                "deadline": "0x0",
                "flags": "0x0",
                "ts": "0x0",
                "isBuy": false
            }),
        )
        .expect("live getter hex numeric fields should parse");
        assert_eq!(ask.price_per_tick, 10_000);
        assert_eq!(ask.ticks, 1024);
    }

    #[test]
    fn generic_order_parser_keeps_resting_buy_orders_for_orders_cli() {
        let order = orderbook_order_from_getter(
            11,
            &json!({
                "note": "0:buyer",
                "tokenContract": "0:000000",
                "price": "1000",
                "amount": "3",
                "escrow": "3075",
                "deadline": "0",
                "flags": "0",
                "ts": "0",
                "isBuy": true
            }),
        )
        .expect("valid getOrder ABI fields")
        .expect("resting buy order should parse");
        assert_eq!(order.owner_note, "0:buyer");
        assert!(order.token_contract.is_none());
        assert!(order.is_buy);
        assert!(!order.is_resting_ask());
        assert_eq!(order.escrow, 3075);
    }

    #[test]
    fn order_parser_rejects_each_missing_required_get_order_abi_field() {
        let valid = json!({
            "note": "0:seller",
            "tokenContract": "0:tc",
            "price": "1",
            "amount": "1",
            "escrow": "0",
            "deadline": "0",
            "flags": "0",
            "ts": "0",
            "isBuy": false
        });

        for field in [
            "isBuy",
            "note",
            "tokenContract",
            "amount",
            "price",
            "escrow",
            "deadline",
            "flags",
            "ts",
        ] {
            let mut malformed = valid.clone();
            malformed
                .as_object_mut()
                .expect("order fixture is an object")
                .remove(field);
            let error = orderbook_order_from_getter(382, &malformed)
                .expect_err("required getOrder ABI field must fail closed");
            assert!(error.to_string().contains(field), "{error:#}");
        }

        let mut legacy_timestamp_only = valid.clone();
        legacy_timestamp_only
            .as_object_mut()
            .expect("order fixture is an object")
            .remove("ts");
        legacy_timestamp_only["timestamp"] = json!(0);
        let error = orderbook_order_from_getter(382, &legacy_timestamp_only)
            .expect_err("legacy timestamp alias must not replace deployed ts field");
        assert!(error.to_string().contains("ts"), "{error:#}");

        let mut wide_flags = valid.clone();
        wide_flags["flags"] = json!("256");
        let error = orderbook_order_from_getter(382, &wide_flags)
            .expect_err("flags wider than uint8 must fail closed");
        assert!(
            error.to_string().contains("flags exceed uint8"),
            "{error:#}"
        );

        let mut wide_price = valid;
        wide_price["price"] =
            json!("115792089237316195423570985008687907853269984665640564039457584007913129639935");
        let error = orderbook_order_from_getter(382, &wide_price)
            .expect_err("uint256 price wider than downstream u128 must fail closed");
        assert!(
            error.to_string().contains("price exceeds downstream u128"),
            "{error:#}"
        );
    }

    #[test]
    fn order_parser_skips_buy_cancelled_and_zero_tc_orders() {
        assert!(resting_ask_from_order(
            1,
            &json!({
                "note": "0:buyer", "tokenContract": "0:tc", "price": "1", "amount": "1",
                "escrow": "1", "deadline": "0", "flags": "0", "ts": "0", "isBuy": true
            })
        )
        .is_none());
        assert!(orderbook_order_from_getter(
            2,
            &json!({
                "note": "0:000000", "tokenContract": "0:000000", "price": "0", "amount": "0",
                "escrow": "0", "deadline": "0", "flags": "0", "ts": "0", "isBuy": false
            })
        )
        .expect("complete empty getOrder sentinel should decode")
        .is_none());
        assert!(resting_ask_from_order(
            3,
            &json!({
                "note": "0:seller", "tokenContract": "0:000000", "price": "1", "amount": "1",
                "escrow": "0", "deadline": "0", "flags": "0", "ts": "0", "isBuy": false
            })
        )
        .is_none());
    }

    #[test]
    fn order_parser_skips_filled_zero_tick_order_and_rejects_ownerless_amount() {
        // a filled / consumed order lingers in the book as a real owner note with ZERO
        // remaining ticks until a `cancelInferenceOrder` sweeps it. It is not matchable, so the
        // parser SKIPS it (Ok(None)) instead of erroring -- otherwise a single filled order at a
        // low id would abort the whole book scan before it reaches the live orders behind it.
        let filled_zero_tick = json!({
            "note": "0:seller", "tokenContract": "0:tc", "price": "1", "amount": "0",
            "escrow": "0", "deadline": "0", "flags": "0", "ts": "0", "isBuy": false
        });
        assert!(
            orderbook_order_from_getter(382, &filled_zero_tick)
                .expect("a filled zero-tick order is skipped, not an error")
                .is_none(),
            "a filled (zero-tick) order must be skipped so the scan reaches the live orders"
        );

        // A non-zero amount with a zero owner note is genuinely malformed(ticks with no owner)
        // and stays fail-loud.
        let ownerless_amount = json!({
            "note": "0:000000", "tokenContract": "0:tc", "price": "1", "amount": "1",
            "escrow": "0", "deadline": "0", "flags": "0", "ts": "0", "isBuy": false
        });
        let error = orderbook_order_from_getter(382, &ownerless_amount)
            .expect_err("nonzero amount with zero owner note is malformed, not absent");
        assert!(error.to_string().contains("zero owner note"), "{error:#}");
    }

    #[test]
    fn book_scan_skips_filled_and_unparseable_orders_and_keeps_live_ones() {
        // end to end at the scan layer: a book with a filled order at id 1 and an
        // unparseable/corrupt slot at id 2 must still surface the live order at id 3, in order,
        // rather than aborting on the first non-live id.
        let raw = vec![
            (
                1u128,
                json!({
                    "note": "0:seller", "tokenContract": "0:tc1", "price": "1", "amount": "0",
                    "escrow": "0", "deadline": "0", "flags": "0", "ts": "0", "isBuy": false
                }),
            ),
            (
                2u128,
                json!({
                    "note": "0:000000", "tokenContract": "0:tc2", "price": "1", "amount": "5",
                    "escrow": "0", "deadline": "0", "flags": "0", "ts": "0", "isBuy": false
                }),
            ),
            (
                3u128,
                json!({
                    "note": "0:seller", "tokenContract": "0:tc3", "price": "7", "amount": "9",
                    "escrow": "0", "deadline": "0", "flags": "0", "ts": "0", "isBuy": false
                }),
            ),
        ];
        let live = collect_live_orders(raw);
        assert_eq!(
            live.len(),
            1,
            "only the live order should survive: {live:?}"
        );
        assert_eq!(live[0].order_id, 3);
        assert_eq!(live[0].ticks, 9);
    }

    #[test]
    fn buyer_target_preflight_accepts_expected_best_ask() {
        let asks = vec![
            parsed_ask(1, "0:expected", 1000, 2),
            parsed_ask(2, "0:later", 1200, 10),
        ];
        assert_eq!(
            next_matching_ask(&asks, 1000, 2)
                .unwrap()
                .token_contract
                .as_deref(),
            Some("0:expected")
        );
        assert!(check_expected_buy_target(&asks, "0:expected", 1000, 2).is_ok());
    }

    #[test]
    fn buyer_target_preflight_accepts_expected_partial_fill() {
        let asks = vec![parsed_ask(1, "0:expected", 1000, 10)];
        assert!(check_expected_buy_target(&asks, "0:expected", 1000, 2).is_ok());
    }

    #[test]
    fn model_only_preflight_accepts_partial_fill_before_submit() {
        let asks = vec![parsed_ask(1, "0:best", 1000, 2)];
        assert!(check_model_buy_full_fill(&asks, 1000, 1).is_ok());
    }

    #[test]
    fn model_only_preflight_accepts_whole_best_ask() {
        let asks = vec![parsed_ask(1, "0:best", 1000, 1)];
        assert!(check_model_buy_full_fill(&asks, 1000, 1).is_ok());
    }

    #[test]
    fn model_only_preflight_reports_price_ceiling_below_best_ask() {
        let asks = vec![parsed_ask(199, "0:best", 11, 1)];
        let quote = crate::chain::executable_quote(&asks, Some(1), None)
            .expect("the same book is quoteable without the buyer ceiling");
        assert!(quote.complete);

        let err = check_model_buy_full_fill(&asks, 10, 1).unwrap_err();

        assert!(err.contains("best ask price 11"), "{err}");
        assert!(err.contains("above buyer max_price_per_tick 10"), "{err}");
        assert!(
            err.contains("Raise --max-price-per-tick to at least 11"),
            "{err}"
        );
    }

    #[test]
    fn model_only_preflight_accepts_equivalent_duplicate_active_tc_asks() {
        let asks = vec![
            parsed_ask(2, "0:DUP", 1000, 1),
            parsed_ask(1, "0:dup", 1000, 1),
        ];
        assert!(check_model_buy_full_fill(&asks, 1000, 1).is_ok());
        let selected = selected_model_buy_ask(&asks, 1000, 1).expect("selected representative ask");
        assert_eq!(selected.order_id, 1);
        assert_eq!(selected.token_contract.as_deref(), Some("0:dup"));
    }

    #[test]
    fn model_only_preflight_rejects_conflicting_duplicate_active_tc_asks() {
        let asks = vec![
            parsed_ask(1, "0:dup", 900, 1),
            parsed_ask(2, "0:DUP", 1000, 1),
        ];
        let err = check_model_buy_full_fill(&asks, 1000, 1).unwrap_err();
        assert!(err.contains("conflicting terms/state"), "{err}");
        assert!(err.contains("0:dup"), "{err}");
    }

    #[test]
    fn executable_filter_skips_closed_duplicate_head() {
        let closed = "0:5701d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb4de57";
        let live = "0:7969d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb44704";
        let asks = vec![
            parsed_ask(14, closed, 100, 1024),
            parsed_ask(15, closed, 100, 1024),
            parsed_ask(19, live, 100, 1024),
        ];
        let mut states = BTreeMap::new();
        states.insert(live.to_ascii_lowercase(), fresh_tc_state());

        let executable =
            executable_resting_asks_by_state(&asks, |tc| states.get(&tc.to_ascii_lowercase()))
                .expect("equivalent stale duplicates are safe to filter");
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].order_id, 19);
        assert_eq!(executable[0].token_contract.as_deref(), Some(live));

        let q = crate::chain::executable_quote(&executable, Some(1024), None)
            .expect("later live ask should remain executable despite stale head");
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 1024);
        assert_eq!(q.fills.len(), 1);
        assert_eq!(q.fills[0].order_id, 19);
    }

    #[test]
    fn executable_filter_keeps_live_prefix_before_stale_tail() {
        let live = "0:7969c6c6012dce3575c0547857ce83bf8001e3deedd7ea0425af3b13d5b24704";
        let stale = "0:5701d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb4de57";
        let after = "0:236cd482607c8ca4690d15cbd95b511f84a8e68bf7eb81cbc0dbe3362bd4c688";
        let asks = vec![
            parsed_ask(35, live, 100, 1024),
            parsed_ask(36, stale, 101, 1024),
            parsed_ask(37, after, 102, 1024),
        ];
        let mut states = BTreeMap::new();
        states.insert(live.to_ascii_lowercase(), fresh_tc_state());
        states.insert(stale.to_ascii_lowercase(), used_tc_state());
        states.insert(after.to_ascii_lowercase(), fresh_tc_state());

        let executable =
            executable_resting_asks_by_state(&asks, |tc| states.get(&tc.to_ascii_lowercase()))
                .expect("live prefix before a stale tail remains executable");
        assert_eq!(executable.len(), 2);
        assert_eq!(executable[0].order_id, 35);
        assert_eq!(executable[0].token_contract.as_deref(), Some(live));
        assert_eq!(executable[1].order_id, 37);
        assert_eq!(executable[1].token_contract.as_deref(), Some(after));
    }

    #[test]
    fn model_only_buy_preflight_rejects_live_ask_after_stale_head() {
        let closed = "0:5701d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb4de57";
        let live = "0:7969c6c6012dce3575c0547857ce83bf8001e3deedd7ea0425af3b13d5b24704";
        let asks = vec![
            parsed_ask(14, closed, 100, 1024),
            parsed_ask(15, closed, 100, 1024),
            parsed_ask(35, live, 100, 1024),
        ];
        let mut states = BTreeMap::new();
        states.insert(live.to_ascii_lowercase(), fresh_tc_state());
        let executable =
            executable_resting_asks_by_state(&asks, |tc| states.get(&tc.to_ascii_lowercase()))
                .expect("stale raw rows are skipped in executable depth");
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].order_id, 35);
        let q = crate::chain::executable_quote(&executable, Some(1024), None)
            .expect("later live ask should quote");
        assert!(q.complete);
        assert_eq!(q.fills.len(), 1);
        assert_eq!(q.fills[0].order_id, 35);

        let err = selected_model_buy_ask_matching_executable_depth(&asks, &executable, 100, 1024)
            .expect_err("raw head blocks later executable ask for submit");
        assert!(err.contains("raw order-book matcher would select"), "{err}");
        assert!(err.contains("order "), "{err}");
        assert!(err.contains("executable quote selected order "), "{err}");
        assert!(err.contains("Refusing to send escrow"), "{err}");
    }

    #[test]
    fn executable_filter_skips_unreadable_raw_row_but_preflight_rejects_mismatch() {
        let unreadable = "0:1111000000000000000000000000000000000000000000000000000000000000";
        let live = "0:2222000000000000000000000000000000000000000000000000000000000000";
        let raw_asks = vec![
            parsed_ask(10, unreadable, 100, 1024),
            parsed_ask(11, live, 100, 1024),
        ];
        let raw_depth_ticks: u128 = raw_asks.iter().map(|ask| ask.ticks).sum();
        assert_eq!(raw_asks.len(), 2);
        assert_eq!(raw_depth_ticks, 2048);

        let mut states = BTreeMap::new();
        states.insert(live.to_ascii_lowercase(), fresh_tc_state());
        let executable =
            executable_resting_asks_by_state(&raw_asks, |tc| states.get(&tc.to_ascii_lowercase()))
                .expect("unreadable raw rows are skipped in quote executable depth");
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].order_id, 11);
        assert_eq!(executable[0].token_contract.as_deref(), Some(live));

        let quote = crate::chain::executable_quote(&executable, Some(1024), None)
            .expect("quote still fills the later live ask");
        assert!(quote.complete);
        assert_eq!(quote.filled_ticks, 1024);
        assert_eq!(quote.fills.len(), 1);
        assert_eq!(quote.fills[0].order_id, 11);
        assert_eq!(quote.fills[0].token_contract, live);

        let err =
            selected_model_buy_ask_matching_executable_depth(&raw_asks, &executable, 100, 1024)
                .expect_err("raw unreadable head blocks later executable ask for submit");
        assert!(err.contains("raw order-book matcher would select"), "{err}");
        assert!(err.contains("order "), "{err}");
        assert!(err.contains("executable quote selected order "), "{err}");
    }

    #[test]
    fn model_only_buy_preflight_rejects_skip_only_later_quote_selection() {
        let closed = "0:5701d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb4de57";
        let live = "0:7969c6c6012dce3575c0547857ce83bf8001e3deedd7ea0425af3b13d5b24704";
        let asks = vec![
            parsed_ask(14, closed, 100, 1024),
            parsed_ask(15, closed, 100, 1024),
            parsed_ask(35, live, 100, 1024),
        ];
        let skip_only_executable = vec![parsed_ask(35, live, 100, 1024)];

        let err = selected_model_buy_ask_matching_executable_depth(
            &asks,
            &skip_only_executable,
            100,
            1024,
        )
        .expect_err("model-only preflight must not follow skip-only executable depth");
        assert!(err.contains("raw order-book matcher would select"), "{err}");
        assert!(err.contains("order "), "{err}");
        assert!(err.contains("executable quote selected order "), "{err}");
    }

    #[test]
    fn model_only_buy_preflight_accepts_when_raw_head_matches_quote() {
        let live = "0:7969c6c6012dce3575c0547857ce83bf8001e3deedd7ea0425af3b13d5b24704";
        let asks = vec![parsed_ask(35, live, 100, 1024)];
        let mut states = BTreeMap::new();
        states.insert(live.to_ascii_lowercase(), fresh_tc_state());
        let executable =
            executable_resting_asks_by_state(&asks, |tc| states.get(&tc.to_ascii_lowercase()))
                .expect("fresh ask remains executable");

        let selected =
            selected_model_buy_ask_matching_executable_depth(&asks, &executable, 100, 1024)
                .expect("raw matcher and executable quote select the same ask");
        assert_eq!(selected.order_id, 35);
        assert_eq!(selected.token_contract.as_deref(), Some(live));
    }

    #[test]
    fn executable_book_listing_returns_multiple_fresh_rows() {
        let first = "0:1111000000000000000000000000000000000000000000000000000000000000";
        let second = "0:2222000000000000000000000000000000000000000000000000000000000000";
        let asks = vec![
            parsed_ask(11, first, 100, 10),
            parsed_ask(12, second, 101, 12),
        ];

        let (rows, reason) =
            submit_safe_executable_book_asks(&asks, &asks, 101, 8).expect("listing is safe");

        assert!(reason.is_none(), "{reason:?}");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].token_contract.as_deref(), Some(first));
        assert_eq!(rows[1].token_contract.as_deref(), Some(second));
    }

    #[test]
    fn executable_book_listing_hides_rows_after_stale_cheaper_blocker() {
        let stale = "0:1111000000000000000000000000000000000000000000000000000000000000";
        let live = "0:2222000000000000000000000000000000000000000000000000000000000000";
        let raw_asks = vec![
            parsed_ask(11, stale, 100, 10),
            parsed_ask(12, live, 101, 12),
        ];
        let executable_asks = vec![parsed_ask(12, live, 101, 12)];

        let (rows, reason) = submit_safe_executable_book_asks(&raw_asks, &executable_asks, 101, 8)
            .expect("stale blocker is an empty executable book, not a duplicate-book error");

        assert!(rows.is_empty(), "{rows:?}");
        let reason = reason.expect("empty stale-blocked list carries reason");
        assert!(reason.contains("non-executable order "), "{reason}");
        assert!(reason.contains("Refusing to list"), "{reason}");
    }

    #[test]
    fn executable_book_listing_keeps_safe_prefix_before_stale_tail() {
        let first = "0:1111000000000000000000000000000000000000000000000000000000000000";
        let stale = "0:2222000000000000000000000000000000000000000000000000000000000000";
        let hidden = "0:3333000000000000000000000000000000000000000000000000000000000000";
        let raw_asks = vec![
            parsed_ask(11, first, 100, 10),
            parsed_ask(12, stale, 101, 12),
            parsed_ask(13, hidden, 102, 12),
        ];
        let executable_asks = vec![
            parsed_ask(11, first, 100, 10),
            parsed_ask(13, hidden, 102, 12),
        ];

        let (rows, reason) = submit_safe_executable_book_asks(&raw_asks, &executable_asks, 102, 8)
            .expect("safe prefix can still be listed");

        assert!(reason.is_none(), "{reason:?}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token_contract.as_deref(), Some(first));
    }

    #[test]
    fn executable_book_listing_blocks_later_rows_after_insufficient_head() {
        let short_head = "0:1111000000000000000000000000000000000000000000000000000000000000";
        let hidden = "0:2222000000000000000000000000000000000000000000000000000000000000";
        let asks = vec![
            parsed_ask(11, short_head, 100, 1),
            parsed_ask(12, hidden, 101, 8),
        ];

        let (rows, reason) =
            submit_safe_executable_book_asks(&asks, &asks, 101, 8).expect("listing is safe");

        assert!(
            rows.is_empty(),
            "later row must not be listed because model-wide matcher fails on the insufficient head: {rows:?}"
        );
        let reason = reason.expect("empty insufficient-head-blocked list carries reason");
        assert!(reason.contains("insufficient head"), "{reason}");
        assert!(reason.contains("refusing multi-ask fill"), "{reason}");
        assert!(reason.contains("order "), "{reason}");
    }

    #[test]
    fn model_only_buy_preflight_preserves_conflicting_duplicate_fail_closed() {
        let dup = "0:5701d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb4de57";
        let asks = vec![
            parsed_ask(14, dup, 100, 1024),
            parsed_ask(15, dup, 101, 1024),
        ];
        let err = selected_model_buy_ask_matching_executable_depth(&asks, &[], 101, 1024)
            .expect_err("conflicting duplicates fail before executable-depth fallback");
        assert!(err.contains("conflicting terms/state"), "{err}");
        assert!(err.contains("order_ids [14,15]"), "{err}");
    }

    #[test]
    fn executable_filter_skips_used_duplicate_head() {
        let used = "0:1111000000000000000000000000000000000000000000000000000000000000";
        let live = "0:2222000000000000000000000000000000000000000000000000000000000000";
        let asks = vec![
            parsed_ask(1, used, 100, 1024),
            parsed_ask(2, used, 100, 1024),
            parsed_ask(3, live, 101, 1024),
        ];
        let mut states = BTreeMap::new();
        states.insert(used.to_ascii_lowercase(), used_tc_state());
        states.insert(live.to_ascii_lowercase(), fresh_tc_state());

        let executable =
            executable_resting_asks_by_state(&asks, |tc| states.get(&tc.to_ascii_lowercase()))
                .expect("used duplicate rows are non-executable depth");
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].order_id, 3);
        assert_eq!(executable[0].token_contract.as_deref(), Some(live));
    }

    #[test]
    fn executable_filter_rejects_conflicting_duplicate_before_state_skip() {
        let closed = "0:5701d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb4de57";
        let live = "0:7969d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb44704";
        let asks = vec![
            parsed_ask(14, closed, 100, 1024),
            parsed_ask(15, closed, 101, 1024),
            parsed_ask(19, live, 100, 1024),
        ];
        let mut states = BTreeMap::new();
        states.insert(live.to_ascii_lowercase(), fresh_tc_state());

        let err =
            executable_resting_asks_by_state(&asks, |tc| states.get(&tc.to_ascii_lowercase()))
                .expect_err("conflicting duplicates must fail closed even if their TC is stale");
        assert!(err.contains("conflicting terms/state"), "{err}");
        assert!(err.contains("order_ids [14,15]"), "{err}");
    }

    #[test]
    fn buyer_target_preflight_rejects_foreign_better_ask() {
        let asks = vec![
            parsed_ask(1, "0:foreign", 900, 10),
            parsed_ask(2, "0:expected", 1000, 10),
        ];
        let err = check_expected_buy_target(&asks, "0:expected", 1000, 2).unwrap_err();
        assert!(err.contains("would match order "), "{err}");
        assert!(
            err.contains("before expected tokenContract 0:expected"),
            "{err}"
        );
    }

    #[test]
    fn buyer_target_preflight_rejects_foreign_partial_fill_before_expected() {
        let asks = vec![
            parsed_ask(1, "0:foreign", 900, 1),
            parsed_ask(2, "0:expected", 1000, 10),
        ];
        let err = check_expected_buy_target(&asks, "0:expected", 1000, 2).unwrap_err();
        assert!(err.contains("would match order "), "{err}");
        assert!(err.contains("tokenContract 0:foreign"), "{err}");
    }

    #[test]
    fn buyer_target_preflight_rejects_missing_expected_ask() {
        let asks = vec![parsed_ask(4, "0:foreign", 1000, 10)];
        let err = check_expected_buy_target(&asks, "0:expected", 1000, 2).unwrap_err();
        assert!(
            err.contains("no resting ask for expected tokenContract 0:expected"),
            "{err}"
        );
        assert!(err.contains("would match"), "{err}");
    }

    #[test]
    fn buyer_target_preflight_rejects_unmatchable_expected_ask() {
        let asks = vec![parsed_ask(5, "0:expected", 1000, 1)];
        let err = check_expected_buy_target(&asks, "0:expected", 1000, 2).unwrap_err();
        assert!(err.contains("refusing multi-ask fill"), "{err}");
        assert!(err.contains("has only 1 ticks"), "{err}");
    }

    #[test]
    fn buyer_target_preflight_accepts_equivalent_duplicate_active_tc_asks() {
        let asks = vec![
            parsed_ask(1, "0:expected", 1000, 2),
            parsed_ask(2, "0:EXPECTED", 1000, 2),
        ];
        assert!(check_expected_buy_target(&asks, "0:expected", 1000, 2).is_ok());
    }

    #[test]
    fn buyer_target_preflight_rejects_conflicting_duplicate_active_tc_asks() {
        let asks = vec![
            parsed_ask(1, "0:expected", 1000, 2),
            parsed_ask(2, "0:EXPECTED", 1000, 3),
        ];
        let err = check_expected_buy_target(&asks, "0:expected", 1000, 2).unwrap_err();
        assert!(err.contains("conflicting terms/state"), "{err}");
    }
}

/// (pure, offline-testable): is a per-deal `TokenContract` already USED(not fresh/reusable)? A fresh
/// active TC is unfunded/unopened -- all `getState` flags false, all amounts 0 -> `None`. Any of
/// `opened`/`funded`/`disputed`/`probeAccepted`, or a non-zero `deposit`/`prepaid`/`frozen`/`finalizedOwed`,
/// means a prior deal used this `(sellerPubkey, nonce)` TC; resting a new ask reverts the seller's pre-stream
/// steps(`fundProbeCommission`/`open`) with a raw `TVM_ERROR`(`ERR_ALREADY_OPEN` 321 and kin). Returns
/// `Some(reason)`(the offending flags/amounts) when used. Numeric fields are `getState`'s uint128 strings.
fn token_contract_used_reason(state: &Value) -> Option<String> {
    let flag = |k: &str| state[k].as_bool().unwrap_or(false);
    let amount = |k: &str| state[k].as_str().and_then(parse_order_u128).unwrap_or(0);
    let mut used = Vec::new();
    for k in ["opened", "funded", "disputed", "probeAccepted"] {
        if flag(k) {
            used.push(k.to_string());
        }
    }
    for k in ["deposit", "prepaid", "frozen", "finalizedOwed"] {
        let v = amount(k);
        if v > 0 {
            used.push(format!("{k}={v}"));
        }
    }
    (!used.is_empty()).then(|| used.join(", "))
}

fn check_selected_token_contract_unused(
    token_contract: &str,
    state: Option<&Value>,
) -> Result<(), String> {
    if let Some(reason) = token_contract_non_executable_reason(state) {
        return Err(format!(
            "selected TokenContract {token_contract} is {reason}; refusing to move escrow"
        ));
    }
    Ok(())
}

fn token_contract_non_executable_reason(state: Option<&Value>) -> Option<String> {
    let Some(state) = state else {
        return Some("not readable by getState".to_string());
    };
    token_contract_used_reason(state)
        .map(|reason| format!("already used by chain state ({reason})"))
}

#[cfg(test)]
fn executable_resting_asks_by_state<'a, F>(
    orders: &[OrderBookOrder],
    mut state_for_tc: F,
) -> Result<Vec<OrderBookOrder>, String>
where
    F: FnMut(&str) -> Option<&'a Value>,
{
    let asks = coalesce_equivalent_resting_asks(orders)?;
    let mut executable = Vec::new();
    for ask in asks {
        let Some(tc) = ask.token_contract.as_deref() else {
            continue;
        };
        if token_contract_non_executable_reason(state_for_tc(tc)).is_some() {
            continue;
        }
        executable.push(ask);
    }
    Ok(executable)
}

/// A seller gateway may resume either before first handover write(`funded` but unopened) or after a live
/// `open_stream`(`opened=true`) so restart can rebuild in-memory gateway authorization. Completed/abandoned
/// deals can keep `funded=true`, so terminal/disputed states still must not bypass the single-use TC freshness
/// guard.
fn token_contract_resume_blocker(state: &Value) -> Option<String> {
    let flag = |k: &str| state[k].as_bool().unwrap_or(false);
    let amount = |k: &str| {
        state[k]
            .as_str()
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0)
    };
    let mut blockers = Vec::new();
    if !flag("funded") {
        blockers.push("funded=false".to_string());
    }
    for k in ["disputed"] {
        if flag(k) {
            blockers.push(k.to_string());
        }
    }
    if flag("probeAccepted") && !flag("opened") {
        blockers.push("probeAccepted without opened".to_string());
    }
    if !flag("opened") {
        for k in ["prepaid", "frozen", "finalizedOwed"] {
            let v = amount(k);
            if v > 0 {
                blockers.push(format!("{k}={v}"));
            }
        }
    }
    (!blockers.is_empty()).then(|| blockers.join(", "))
}

/// (pure, offline-testable): the note's on-chain owner key (`getDetails().ephemeralPubkey` -- what the
/// `onlyOwnerPubkey(_ephemeralPubkey)` gate checks `msg.pubkey()` against) must equal the key the client signs
/// the owner-authenticated write(`placeInferenceBuy` / `postSellOffer`) with. If the note's `_ephemeralPubkey`
/// was rotated(`changeOwner`, `PrivateNote.sol:381`) or the pool records a different/orphaned owner, that gate
/// rejects the write PRE-accept(`ERR_INVALID_SENDER` 101, dex table -- `contracts/dex/modifiers/errors.sol`) ->
/// no tx commits -> the buyer silently 300s-times out in `read_match`.
/// Returns the actionable fail-closed reason, or `None` when they match. Both keys are normalized
/// (lower-case, strip `0x`) before comparing -- the getter returns `0x...`(possibly upper-case), `public_hex()`
/// has no prefix. This is the branch-3(non-conforming/orphaned note) guard; the async
/// [`RealChainBackend::assert_note_owner_matches`] wraps it with the on-chain `getDetails` read.
pub(super) fn note_owner_mismatch_reason(
    role: &str,
    note: &Address,
    ephemeral_onchain: Option<&str>,
    signing_pubkey_hex: &str,
) -> Option<String> {
    let norm = |s: &str| s.to_ascii_lowercase().trim_start_matches("0x").to_string();
    let signing = norm(signing_pubkey_hex);
    let onchain = ephemeral_onchain.unwrap_or("<none>");
    if !signing.is_empty() && norm(onchain) == signing {
        return None;
    }
    Some(format!(
        "{role} aborted: --note-key pubkey 0x{signing} does not match note {note}'s on-chain owner key \
         _ephemeralPubkey {onchain} (ownership rotated via changeOwner, or a stale/wrong/orphaned pool). The \
         note's onlyOwnerPubkey gate rejects msg.pubkey() pre-accept (ERR_INVALID_SENDER 101, dex table) -- the \
         write never commits (no order rests; the buyer then 300s-times out in read_match). Re-mint the note \
         against the current contracts (`mint_pn_pool`) and point DEXDO_PN_POOL at the fresh pool, or use the \
         correct --note-key."
    ))
}

// --- Shared helpers for the per-role CLI backends ---------------------------------------
// Free functions reused by `RealSellerBackend`/`RealBuyerBackend`. `RealDealBackend`
// (the in-process form of D2) is intentionally NOT touched(a 10/10 do-not-break) -- it has its own inline bodies; the small
// duplication of formulas here is the deliberate price of "leaving D2 as is".

/// Wait for a boolean TC `getState` flag(the trait's transitions are synchronous -- they wait for `submit` to apply).
async fn wait_tc_bool(
    chain: &RealChainBackend,
    tc: &Address,
    key: &str,
    want: bool,
) -> Result<(), ChainError> {
    for _ in 0..40 {
        if let Some(st) = chain.token_contract_state(tc).await.map_err(map_err)? {
            if st[key].as_bool().unwrap_or(!want) == want {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    Err(ChainError::Chain(format!(
        "TC {tc}: field {key} != {want} within the allotted time"
    )))
}

/// A snapshot of the locks/burned amounts for a TC -- the same reads as in `RealDealBackend::snapshot`.
async fn real_tc_snapshot(
    chain: &RealChainBackend,
    token_contract: &TokenContract,
) -> Option<StreamSnapshot> {
    let tc = Address::parse(token_contract).ok()?;
    let st = chain.token_contract_state(&tc).await.ok()??;
    let pr = chain.token_contract_probe(&tc).await.ok().flatten();
    let lifecycle = deal_chain_state_from_json(&st);
    let g = |s: &Value, k: &str| {
        s[k].as_str()
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0)
    };
    Some(StreamSnapshot {
        seller_locked: pr.as_ref().map(|p| g(p, "probeLocked")).unwrap_or(0) as Shell,
        buyer_locked: (g(&st, "prepaid") + g(&st, "frozen") + g(&st, "deposit")) as Shell,
        // the at-risk lead is `prepaid + frozen` only -- the unspent `deposit` is not part of the
        // two-tick bound(it funds the remaining ticks of a multi-tick deal).
        buyer_lead: (g(&st, "prepaid") + g(&st, "frozen")) as Shell,
        seller_received: g(&st, "finalizedOwed") as Shell,
        buyer_refunded: 0,
        burned: 0,
        closed: lifecycle.is_stopped(),
    })
}

/// Read one market's deal into a monitor [`DealView`] from the **authoritative on-chain getters** (issue,
/// real-chain reader). The operator's [`crate::MarketManifest`] supplies only the `TokenContract` ADDRESS to
/// read + the `model_hash` to integrity-check against; every accounting field comes from the CHAIN -- model from
/// `getModelName`, price from `getDeal().pricePerTick`, by-fact from `getState`/`getProbe`, counterparty from
/// `getBuyerPubkey`. The manifest is operator-supplied and is NOT trusted as chain truth: this **fails loud**
/// rather than rendering a stale/hand-edited manifest or hiding a broken/undeployed TC as empty data. Errors on:
/// a `token_contract` that does not parse; an undeployed/inactive TC(no `getState`); unreadable
/// `getModelName`/`getDeal`; or an on-chain `getModelHash` that does NOT match the manifest's `model_hash` (the
/// manifest points at a TC for a different model). The operator is the SELLER of their own market, so
/// `role = Seller`. The view feeds `print_tree_snapshot` + `deal_anomalies` like the mock path. (Refund/burn are
/// not live-readable -- `real_tc_snapshot` leaves them `0`.) The caller adds the `--market <path>` context.
pub async fn real_market_deal_view(
    chain: &RealChainBackend,
    manifest: &crate::MarketManifest,
) -> Result<DealView> {
    let tc = manifest.token_contract.as_str();
    let addr =
        Address::parse(tc).map_err(|e| anyhow!("token_contract {tc}: invalid address: {e}"))?;
    // Fail loud: an undeployed / inactive TC is NOT a valid accounting row -- never render it as empty data.
    let snapshot = real_tc_snapshot(chain, &manifest.token_contract)
        .await
        .ok_or_else(|| {
            anyhow!("TokenContract {tc} is not readable (undeployed/inactive/getState failed)")
        })?;
    // Model: authoritative on-chain getModelName(NOT the manifest's frame_model).
    let model = chain
        .token_contract_model_name(&addr)
        .await?
        .ok_or_else(|| anyhow!("TokenContract {tc}: getModelName empty/unreadable"))?;
    // Integrity: the on-chain modelHash MUST match the manifest's -- else the manifest points at the wrong TC.
    let on_chain_hash = chain
        .token_contract_model_hash(&addr)
        .await?
        .ok_or_else(|| anyhow!("TokenContract {tc}: getModelHash empty/unreadable"))?;
    if on_chain_hash != manifest.model_hash {
        return Err(anyhow!(
            "TokenContract {tc}: on-chain modelHash {on_chain_hash} != manifest model_hash {} \
             (the manifest points at a TC for a different model)",
            manifest.model_hash
        ));
    }
    // Price: authoritative on-chain getDeal().pricePerTick(NOT the manifest's).
    let price = chain
        .token_contract_price_per_tick(&addr)
        .await?
        .ok_or_else(|| anyhow!("TokenContract {tc}: getDeal/pricePerTick unreadable"))?;
    // Counterparty: the matched buyer's anonymous pubkey(none before a match).
    let counterparty = chain
        .token_contract_buyer_pubkey(&addr)
        .await?
        .map(|pk| pk.iter().map(|b| format!("{b:02x}")).collect::<String>());
    Ok(DealView {
        token_contract: manifest.token_contract.clone(),
        role: DealRole::Seller,
        counterparty,
        // SHELL price fits u64 for any real market; saturate rather than silently wrap a bogus huge value.
        price_per_tick: price.min(Shell::MAX as u128) as Shell,
        model: Some(model),
        snapshot: Some(snapshot),
    })
}

/// The STOP outcome from the TC state BEFORE the call: on the probe -- `BurnBoth`, otherwise `AmicableSplit`.
fn settle_stop(
    accepted: bool,
    prepaid: u128,
    frozen: u128,
    deposit: u128,
    commission: u128,
) -> Settlement {
    if !accepted {
        Settlement::BurnBoth(ProbeBurn {
            buyer: frozen as Shell,
            seller: commission as Shell,
        })
    } else {
        Settlement::AmicableSplit {
            to_seller_ticks: if prepaid > 0 { 1 } else { 0 },
            to_buyer_refund: (frozen + deposit) as Shell,
        }
    }
}

/// The expected post-release outcome for the buyer: a dispute/concession/timeout returns the tick to the buyer
/// without burn -- on the probe, probe+deposit to the buyer, commission to the seller; otherwise a split with a refund.
fn settle_release(accepted: bool, frozen: u128, deposit: u128, commission: u128) -> Settlement {
    if !accepted {
        Settlement::SellerNoShow {
            to_buyer_refund: (frozen + deposit) as Shell,
            seller_commission_returned: commission as Shell,
        }
    } else {
        Settlement::AmicableSplit {
            to_seller_ticks: 0,
            to_buyer_refund: (frozen + deposit) as Shell,
        }
    }
}

/// A "wrong role" error: a counterparty's method was called on a per-role backend.
fn wrong_role(method: &str, want: &str) -> ChainError {
    ChainError::Chain(format!(
        "{method}: a `{want}` role action on a backend of a different role -- run `dexdo {want}`"
    ))
}

/// The canonical `tickSize`(uint128) for CLI derivation of the `InferenceOrderBook` address: both sides
/// derive the book address from `(model_hash, tick_size)`, so the tick size is fixed in code (a single source
/// of truth, not a flag -- otherwise the sides desync).: a tick is the canonical `TICK_SIZE` =
/// **1,000,000 delivered tokens** (`params::DobParams::canonical().tick_size`, spec), NOT an ad-hoc `1000`.
/// Both the book-address derivation here and the seller's tick-finalization cadence read this one value.
pub const MODEL_TICK_SIZE: u128 = crate::params::DobParams::canonical().tick_size as u128;

impl RealChainBackend {
    pub async fn inference_orderbook_snapshot(
        &self,
        order_book: &Address,
        frame_model: &str,
        model_hash: &str,
    ) -> Result<OrderBookSnapshot> {
        let Some(stats_value) = self.inference_orderbook_stats(order_book).await? else {
            return Ok(OrderBookSnapshot {
                frame_model: frame_model.to_string(),
                model_hash: model_hash.to_string(),
                order_book: order_book.with_workchain(),
                stats: None,
                orders: Vec::new(),
            });
        };
        let stats = orderbook_stats_from_getter(&stats_value);
        let mut raw = Vec::new();
        for id in 1..stats.next_order_id {
            // A per-id transport/chain read failure is a real error and surfaces here; a
            // `None` is an absent slot. Parsing (and the skip of filled/unparseable
            // orders) happens in `collect_live_orders`.
            if let Some(order) = self.inference_orderbook_order(order_book, id).await? {
                raw.push((id, order));
            }
        }
        let orders = collect_live_orders(raw);
        Ok(OrderBookSnapshot {
            frame_model: frame_model.to_string(),
            model_hash: model_hash.to_string(),
            order_book: order_book.with_workchain(),
            stats: Some(stats),
            orders,
        })
    }

    pub async fn inference_orderbook_snapshot_for_note(
        &self,
        note: &Address,
        frame_model: &str,
        model_hash: &str,
        tick_size: u128,
    ) -> Result<OrderBookSnapshot> {
        let order_book = self
            .inference_orderbook_address(note, model_hash, tick_size)
            .await?;
        self.inference_orderbook_snapshot(&order_book, frame_model, model_hash)
            .await
    }

    pub async fn executable_resting_asks(
        &self,
        snapshot: &OrderBookSnapshot,
    ) -> Result<Vec<OrderBookOrder>> {
        let asks = coalesce_equivalent_resting_asks(&snapshot.orders).map_err(|e| {
            anyhow!(
                "InferenceOrderBook {} exposes unsafe duplicate active sell orders: {e}",
                snapshot.order_book
            )
        })?;
        let mut executable = Vec::with_capacity(asks.len());
        for ask in asks {
            let Some(token_contract) = ask.token_contract.as_deref() else {
                continue;
            };
            let Ok(tc) = Address::parse(token_contract) else {
                continue;
            };
            let state = self.token_contract_state(&tc).await?;
            if token_contract_non_executable_reason(state.as_ref()).is_none() {
                let balance = self.active_native_balance(&tc).await?;
                if balance > GAS_HEALTH_MIN {
                    executable.push(ask);
                }
            }
        }
        Ok(executable)
    }

    pub async fn submit_safe_single_ask_quote(
        &self,
        snapshot: &OrderBookSnapshot,
        wanted_ticks: Option<u128>,
        budget: Option<u128>,
    ) -> Result<ExecutableQuote> {
        let asks = coalesce_equivalent_resting_asks(&snapshot.orders).map_err(|e| {
            anyhow!(
                "InferenceOrderBook {} exposes unsafe duplicate active sell orders: {e}",
                snapshot.order_book
            )
        })?;
        let quote = crate::chain::submit_safe_single_ask_quote(&asks, wanted_ticks, budget)
            .map_err(|e| anyhow!("quote: {e}"))?;
        if !quote.complete {
            return Ok(quote);
        }
        for fill in &quote.fills {
            let Ok(tc) = Address::parse(&fill.token_contract) else {
                return Ok(ExecutableQuote {
                    filled_ticks: 0,
                    total_with_fee: 0,
                    complete: false,
                    fills: Vec::new(),
                });
            };
            let state = self.token_contract_state(&tc).await?;
            if token_contract_non_executable_reason(state.as_ref()).is_some() {
                return Ok(ExecutableQuote {
                    filled_ticks: 0,
                    total_with_fee: 0,
                    complete: false,
                    fills: Vec::new(),
                });
            }
            if self.active_native_balance(&tc).await? <= GAS_HEALTH_MIN {
                return Ok(ExecutableQuote {
                    filled_ticks: 0,
                    total_with_fee: 0,
                    complete: false,
                    fills: Vec::new(),
                });
            }
        }
        Ok(quote)
    }

    pub async fn submit_safe_model_buy_ask(
        &self,
        snapshot: &OrderBookSnapshot,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<OrderBookOrder> {
        let raw_asks: Vec<OrderBookOrder> = snapshot.resting_asks().cloned().collect();
        let executable_asks = self.executable_resting_asks(snapshot).await?;
        selected_model_buy_ask_matching_executable_depth(
            &raw_asks,
            &executable_asks,
            max_price_per_tick,
            ticks,
        )
        .map_err(|e| {
            anyhow!(
                "no_executable_ask: no executable matching ask for InferenceOrderBook {} at max_price_per_tick {}, \
                 requested ticks {}: {e}. IOB stats {}",
                snapshot.order_book,
                max_price_per_tick,
                ticks,
                orderbook_stats_for_error(snapshot)
            )
        })
    }

    pub async fn submit_safe_executable_book_asks(
        &self,
        snapshot: &OrderBookSnapshot,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<(Vec<OrderBookOrder>, Option<String>)> {
        let raw_asks: Vec<OrderBookOrder> = snapshot.resting_asks().cloned().collect();
        let executable_asks = self.executable_resting_asks(snapshot).await?;
        submit_safe_executable_book_asks(&raw_asks, &executable_asks, max_price_per_tick, ticks)
            .map_err(|e| {
                anyhow!(
                    "InferenceOrderBook {} exposes unsafe executable-book depth: {e}",
                    snapshot.order_book
                )
            })
    }
}

#[async_trait]
impl ChainBackend for RealDealBackend {
    async fn discover_offers(&self) -> Result<Vec<crate::chain::OfferListing>, ChainError> {
        // The adapter is configured for a SINGLE deal: book discovery (many offers,
        // B1) is a read of `InferenceOrderBook` via the low-level `RealChainBackend`
        // (getStats/getOrder), not the job of the single-deal wrapper. Here -- empty.
        Ok(Vec::new())
    }

    async fn post_offer(&self, _offer: SellOffer, _note: &dyn Note) -> Result<(), ChainError> {
        // 4.0.25: one seller call. PrivateNote.postSellOffer(flags, nonce) derives the canonical per-deal
        // TokenContract locally and hands it the baked InferenceOrderBook hash; the TC posts its own resting
        // ask(msg.sender == TC). No RootPN round-trip.
        self.chain
            .post_sell_offer(
                &self.ctx.seller_note,
                &self.ctx.seller_keys,
                0,
                self.ctx.nonce,
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn place_buy(
        &self,
        _token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        // the shared order book may contain valid asks from many sellers, each with its own canonical TC.
        // The IOB itself enforces `tokenContract == _tokenContractAddr(sellerPubkey, nonce)` at
        // `placeSellOffer`, so a client-side scan against this buyer's single expected TC is both redundant and
        // wrong for shared books.
        self.chain
            .place_inference_buy(
                &self.ctx.buyer_note,
                &self.ctx.buyer_keys,
                &self.ctx.model_hash,
                self.ctx.price_per_tick,
                self.ctx.ticks,
                self.ctx.escrow,
                0,
                0,
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        let tc = parse_tc(token_contract)?;
        for _ in 0..40 {
            let state = self
                .chain
                .token_contract_state(&tc)
                .await
                .map_err(map_err)?;
            if let Some(state) = state
                .as_ref()
                .filter(|s| s["funded"].as_bool().unwrap_or(false))
            {
                if let Some(reason) = token_contract_resume_blocker(state) {
                    return Err(ChainError::Chain(format!(
                        "TokenContract {token_contract} is matched but not openable for seller resume \
                         ({reason}) -- use a fresh --nonce / --market, or close/recover the previous deal"
                    )));
                }
                return Ok(Match {
                    token_contract: token_contract.clone(),
                    buyer_pubkey: self.ctx.buyer_pubkey.clone(),
                    price_per_tick: self.ctx.price_per_tick as Shell,
                });
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Err(ChainError::NoMatch(token_contract.clone()))
    }

    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        enc_endpoint: Vec<u8>,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let tc = parse_tc(token_contract)?;
        self.ensure_tc_gas(&tc).await?;
        // +: the note posts the probe-commission to the nonce-derived TC from its own ECC[2]
        // (`postProbeCommission`) -- no operator wallet.
        post_probe_commission_and_wait(
            &self.chain,
            &self.ctx.seller_note,
            &self.ctx.seller_keys,
            self.ctx.nonce,
            token_contract,
            &tc,
            self.ctx.probe_shell,
        )
        .await?;
        // the enc endpoint(handover) is written to the TC. Wait for open() to apply(opened==true).
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .open_stream(&tc, &self.ctx.seller_keys, &enc_endpoint)
            .await
            .map_err(map_err)?;
        self.wait_state_bool(&tc, "opened", true).await
    }

    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        let tc = parse_tc(token_contract)?;
        self.chain.read_handover(&tc).await.map_err(map_err)
    }

    async fn advance_tick(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let tc = parse_tc(token_contract)?;
        self.ensure_tc_gas(&tc).await?;
        // Finalizing the streaming tick: wait until `lastAdvance` moves (the advance() effect is applied).
        let read_la = |st: &Option<Value>| -> u128 {
            st.as_ref()
                .and_then(|s| s["lastAdvance"].as_str())
                .and_then(|x| x.parse::<u128>().ok())
                .unwrap_or(0)
        };
        let pre = read_la(
            &self
                .chain
                .token_contract_state(&tc)
                .await
                .map_err(map_err)?,
        );
        self.chain
            .advance_stream(&tc, &self.ctx.seller_keys)
            .await
            .map_err(map_err)?;
        for _ in 0..40 {
            if read_la(
                &self
                    .chain
                    .token_contract_state(&tc)
                    .await
                    .map_err(map_err)?,
            ) > pre
            {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Err(ChainError::Chain(format!("TC {tc}: advance did not apply")))
    }

    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        // On the real chain the probe is accepted by the same `advance()`(the first call after SETTLE_WINDOW).
        let tc = parse_tc(token_contract)?;
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .advance_stream(&tc, &self.ctx.seller_keys)
            .await
            .map_err(map_err)?;
        self.wait_state_bool(&tc, "probeAccepted", true).await
    }

    async fn stop(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, accepted, prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .stream_stop(&self.ctx.buyer_note, &self.ctx.buyer_keys, &tc)
            .await
            .map_err(map_err)?;
        self.wait_state_bool(&tc, "opened", false).await?;
        // the outcome is computed from the state BEFORE stop.
        if !accepted {
            Ok(Settlement::BurnBoth(ProbeBurn {
                buyer: frozen as Shell,
                seller: commission as Shell,
            }))
        } else {
            Ok(Settlement::AmicableSplit {
                to_seller_ticks: if prepaid > 0 { 1 } else { 0 },
                to_buyer_refund: (frozen + deposit) as Shell,
            })
        }
    }

    async fn dispute(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.ensure_tc_gas(&tc).await?;
        // the dispute locks BOTH notes (`streamDispute`->`TC.dispute()`->`streamDisputeLock`) and
        // FREEZES the deal(does not close it -- `_opened` stays true, we wait for `disputed==true`).
        self.chain
            .stream_dispute(&self.ctx.buyer_note, &self.ctx.buyer_keys, &tc)
            .await
            .map_err(map_err)?;
        self.wait_state_bool(&tc, "disputed", true).await?;
        // The final settlement is on `releaseDispute`(the seller concedes / dispute timeout). We return
        // the EXPECTED post-release outcome for the buyer: on the probe the tick and deposit are returned, the commission
        // to the seller, NO burn.
        if !accepted {
            Ok(Settlement::SellerNoShow {
                to_buyer_refund: (frozen + deposit) as Shell,
                seller_commission_returned: commission as Shell,
            })
        } else {
            Ok(Settlement::AmicableSplit {
                to_seller_ticks: 0, // the disputed ticks are returned to the buyer on release
                to_buyer_refund: (frozen + deposit) as Shell,
            })
        }
    }

    async fn release_dispute(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.ensure_tc_gas(&tc).await?;
        // the seller concedes -- `TC.releaseDispute()` unlocks both notes and returns the tick
        // to the buyer(on the probe: probe+deposit to the buyer, commission to the seller, no burn).
        self.chain
            .release_dispute(&tc, &self.ctx.seller_keys)
            .await
            .map_err(map_err)?;
        self.wait_state_bool(&tc, "disputed", false).await?;
        if !accepted {
            Ok(Settlement::SellerNoShow {
                to_buyer_refund: (frozen + deposit) as Shell,
                seller_commission_returned: commission as Shell,
            })
        } else {
            Ok(Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: (frozen + deposit) as Shell,
            })
        }
    }

    async fn seller_timeout(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, _accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .reclaim_on_timeout(&self.ctx.buyer_note, &self.ctx.buyer_keys, &tc)
            .await
            .map_err(map_err)?;
        self.wait_state_bool(&tc, "opened", false).await?;
        Ok(Settlement::SellerNoShow {
            to_buyer_refund: (frozen + deposit) as Shell,
            seller_commission_returned: commission as Shell,
        })
    }

    async fn cleanup_unopened(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, _accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .stream_cleanup(&self.ctx.buyer_note, &self.ctx.buyer_keys, &tc)
            .await
            .map_err(map_err)?;
        for _ in 0..40 {
            match self
                .chain
                .token_contract_state(&tc)
                .await
                .map_err(map_err)?
            {
                None => {
                    return Ok(Settlement::SellerNoShow {
                        to_buyer_refund: (frozen + deposit) as Shell,
                        seller_commission_returned: commission as Shell,
                    })
                }
                Some(st) if !st["funded"].as_bool().unwrap_or(true) => {
                    return Ok(Settlement::SellerNoShow {
                        to_buyer_refund: (frozen + deposit) as Shell,
                        seller_commission_returned: commission as Shell,
                    })
                }
                Some(_) => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
            }
        }
        Err(ChainError::Chain(format!(
            "TC {tc}: cleanupUnopened did not clear funded state within the allotted time"
        )))
    }

    async fn deal_state(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        let tc = parse_tc(token_contract)?;
        Ok(self
            .chain
            .token_contract_state(&tc)
            .await
            .map_err(map_err)?
            .as_ref()
            .map(deal_chain_state_from_json))
    }

    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot> {
        let tc = Address::parse(token_contract).ok()?;
        let st = self.chain.token_contract_state(&tc).await.ok()??;
        let pr = self.chain.token_contract_probe(&tc).await.ok().flatten();
        let lifecycle = deal_chain_state_from_json(&st);
        let g = |s: &Value, k: &str| {
            s[k].as_str()
                .and_then(|x| x.parse::<u128>().ok())
                .unwrap_or(0)
        };
        Some(StreamSnapshot {
            seller_locked: pr.as_ref().map(|p| g(p, "probeLocked")).unwrap_or(0) as Shell,
            buyer_locked: (g(&st, "prepaid") + g(&st, "frozen") + g(&st, "deposit")) as Shell,
            // the at-risk lead is `prepaid + frozen` only -- the unspent `deposit` is not part of the
            // two-tick bound(it funds the remaining ticks of a multi-tick deal).
            buyer_lead: (g(&st, "prepaid") + g(&st, "frozen")) as Shell,
            seller_received: g(&st, "finalizedOwed") as Shell,
            buyer_refunded: 0, // not in getState -- the actual magnitude is carried by Settlement from stop/seller_timeout
            burned: 0, // not in getState -- the net burn is outside the getter
            closed: lifecycle.is_stopped(),
        })
    }
}

/// The per-role CLI backend of the **SELLER**: the `ChainBackend` trait for the `dexdo seller` process. Unlike
/// [`RealDealBackend`](both sides in-process, D2) it holds ONLY the seller's identity (note+keys +
/// `model_hash` from) and **reads the counterparty/state from the chain** -- the buyer's
/// pubkey is taken from on-chain `getBuyerPubkey` after the match(F1), not from arguments. The seller side is
/// **note-funded**: no operator wallet -- the note self-funds the deploy pre-fund + probe-commission from its
/// own ECC[2]. It reuses [`RealChainBackend`] helpers(deploy OB/offer/probe/advance) -- it does not duplicate
/// submit/provisioning. Provisioning(note/keys) is NOT here: the backend only reads/signs.
pub struct RealSellerBackend {
    chain: RealChainBackend,
    note: Address,
    keys: KeyPair,
    model_hash: String,
    /// Canonical model name(4.0.6): forwarded into `deployInferenceOrderBook(modelHash, modelName)`
    /// so the book verifies `sha256(modelName)==modelHash`(`ERR_BAD_MODEL_NAME`).
    model_name: String,
    /// Deal nonce for the per-deal `TokenContract`: the `_nonce` static the TC is deployed with and the
    /// nonce passed to the 4.0.26 `note.postSellOffer(flags, nonce)` call.
    nonce: u64,
    tick_size: u128,
    probe_shell: u128,
    offer_post_started_at: std::sync::Mutex<Option<u64>>,
}

impl RealSellerBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: RealChainBackend,
        note: Address,
        keys: KeyPair,
        model_hash: String,
        model_name: String,
        nonce: u64,
        tick_size: u128,
        probe_shell: u128,
    ) -> Self {
        Self {
            chain,
            note,
            keys,
            model_hash,
            model_name,
            nonce,
            tick_size,
            probe_shell,
            offer_post_started_at: std::sync::Mutex::new(None),
        }
    }

    /// Assemble the seller backend and the seller's note from the `--note-key` seed. Directive: the
    /// seller has **no operator multisig** -- the note self-funds its seller side (RootModel/TC deploy pre-fund
    /// via `fundDeployShell`, probe-commission via `postProbeCommission`) from its own ECC[2], so there is no
    /// wallet to derive. The note address `note_addr` is mint-specific(`depositIdentifier`), not derivable, so
    /// it is passed in. dexdo does NOT create keys and does NOT fund from
    /// the giver. `model_hash` -- from `frame_model`. Returns the backend + a
    /// `RealNote` for the gateway. All SDK types stay in the core -- the CLI passes strings.
    pub fn from_provisioned(
        manifest_path: &str,
        note_addr: &str,
        note_secret_hex: &str,
        frame_model: &str,
        nonce: u64,
        probe_shell: u128,
    ) -> Result<(Self, RealNote)> {
        let chain = RealChainBackend::connect(manifest_path)?;
        let note =
            Address::parse(note_addr).map_err(|e| anyhow!("--note-addr {note_addr}: {e}"))?;
        let keys = KeyPair::from_secret_hex(note_secret_hex.trim())
            .map_err(|e| anyhow!("--note-key (SDK secret hex): {e:?}"))?;
        let rn = RealNote::from_secret_hex(note_secret_hex)
            .map_err(|e| anyhow!("--note-key invalid ed25519 seed: {e}"))?;
        let backend = Self::new(
            chain,
            note,
            keys,
            model_hash_for(frame_model),
            frame_model.to_string(),
            nonce,
            MODEL_TICK_SIZE,
            probe_shell,
        );
        Ok((backend, rn))
    }

    async fn ensure_tc_gas(&self, tc: &Address) -> Result<(), ChainError> {
        self.chain
            .ensure_deal_contract_gas(&self.note, &self.keys, self.nonce, None, Some(tc))
            .await
            .map_err(map_err)
    }

    async fn read_openable_match_once(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        let tc = parse_tc(token_contract)?;
        let Some(state) = self
            .chain
            .token_contract_state(&tc)
            .await
            .map_err(map_err)?
        else {
            return Ok(None);
        };
        if !state["funded"].as_bool().unwrap_or(false) {
            return Ok(None);
        }
        if let Some(reason) = token_contract_resume_blocker(&state) {
            return Err(ChainError::Chain(format!(
                "TokenContract {token_contract} is matched but not openable for seller resume \
                 ({reason}) -- use a fresh --nonce / --market, or close/recover the previous deal"
            )));
        }
        // F1: the buyer's pubkey is FROM THE CHAIN(`getBuyerPubkey`, ed25519),
        // not from arguments. Reconstruct the x25519 handover key from it.
        let ed = self
            .chain
            .token_contract_buyer_pubkey(&tc)
            .await
            .map_err(map_err)?
            .ok_or_else(|| {
                ChainError::Chain(format!("TC {tc}: funded, but buyerPubkey is empty"))
            })?;
        let x = crate::note::x25519_pub_from_ed25519_pub(&ed).ok_or_else(|| {
            ChainError::Chain(format!("TC {tc}: buyerPubkey is an invalid ed25519 point"))
        })?;
        let price_per_tick = self
            .sell_offer_terms(token_contract)
            .await?
            .map(|(price, _ticks)| price)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "TokenContract {token_contract} getDeal unavailable after match"
                ))
            })?;
        Ok(Some(Match {
            token_contract: token_contract.clone(),
            buyer_pubkey: NotePubkey { x, ed },
            price_per_tick,
        }))
    }

    async fn post_offer_failure_evidence(&self, tc: &Address) -> (String, String) {
        let tc_state_evidence = match retry_seller_read("seller failure TC state", || async {
            self.chain.token_contract_state(tc).await.map_err(map_err)
        })
        .await
        {
            Ok(Some(_)) => format!(
                "TokenContract {} state evidence: Active/getState readable",
                tc.with_workchain()
            ),
            Ok(None) => format!(
                "TokenContract {} state evidence: not Active or getState unreadable",
                tc.with_workchain()
            ),
            Err(e) => format!(
                "TokenContract {} state evidence: getState error: {e}",
                tc.with_workchain()
            ),
        };
        let seller_pubkey = json!(format!("0x{}", self.keys.public_hex()));
        let canonical_evidence = match retry_seller_read("seller failure RootModel", || async {
            self.chain
                .root_model_address_for(&seller_pubkey)
                .await
                .map_err(map_err)
        })
        .await
        {
            Ok(root_model) => match retry_seller_read("seller failure canonical TC", || async {
                self.chain
                    .resolve_token_contract(&root_model, &seller_pubkey, self.nonce)
                    .await
                    .map_err(map_err)
            })
            .await
            {
                Ok(expected) => format!(
                    "RootModel expected TokenContract for (sellerPubkey, nonce) is {} and offered token_contract is {}; match={}",
                    expected.with_workchain(),
                    tc.with_workchain(),
                    expected.with_workchain().eq_ignore_ascii_case(&tc.with_workchain())
                ),
                Err(e) => format!(
                    "RootModel expected TokenContract for (sellerPubkey, nonce) could not be read from {root_model}: {e}"
                ),
            },
            Err(e) => format!(
                "RootModel address for sellerPubkey could not be read from SuperRoot: {e}"
            ),
        };
        (canonical_evidence, tc_state_evidence)
    }
}

#[async_trait]
impl ChainBackend for RealSellerBackend {
    /// the seller daemon publishes offers without `provision_market`'s note-current gate; enforce it here
    /// so a note orphaned by a contract redeploy(stale code_hash) fails closed with an actionable "re-mint"
    /// message instead of a raw `TVM_ERROR` from `postSellOffer`.
    async fn assert_note_current(&self) -> Result<(), ChainError> {
        retry_seller_read("seller note code", || async {
            self.chain
                .assert_seller_note_current(&self.note)
                .await
                .map_err(map_err)
        })
        .await
    }
    /// `PrivateNote._hasWithdrawn=true` permanently blocks `postSellOffer`. Read it before seller writes
    /// so users get the fresh-note action instead of raw `ERR_INVALID_STATE` 151.
    async fn assert_note_can_post_sell_offer(&self) -> Result<(), ChainError> {
        retry_seller_read("seller note post eligibility", || async {
            self.chain
                .assert_note_can_post_sell_offer(&self.note)
                .await
                .map_err(map_err)
        })
        .await
    }
    /// the per-deal TC(sellerPubkey + nonce) is single-use; before resting an ask, fail closed if it is
    /// already USED(a prior deal opened/funded/disputed it or left residual), so the operator gets an
    /// actionable message instead of a raw `TVM_ERROR`(`ERR_ALREADY_OPEN` 321) from the pre-stream steps. A
    /// not-yet-active(undeployed) TC is not "used" -- let the deploy path handle it.
    async fn assert_token_contract_fresh(&self, tc: &TokenContract) -> Result<(), ChainError> {
        let addr = parse_tc(tc)?;
        let Some(state) = retry_seller_read("seller TokenContract freshness", || async {
            self.chain
                .token_contract_state(&addr)
                .await
                .map_err(map_err)
        })
        .await?
        else {
            return Ok(());
        };
        if let Some(reason) = token_contract_used_reason(&state) {
            return Err(ChainError::Chain(format!(
                "deal TokenContract {tc} is already USED ({reason}) -- a per-deal TC (sellerPubkey + nonce) is \
                 single-use, not reusable capacity. Use a fresh --nonce / fresh --market, or close the prior \
                 deal (`dexdo recover` as the buyer, then `dexdo destroy` as the seller) before re-offering ()."
            )));
        }
        Ok(())
    }
    /// the deal's dynamic stream-phase cadence from the on-chain `getConfig().settleWindow` (the seller
    /// driver pairs it with the fixed `PROBE_WINDOW`). Fail-loud if the deal exposes no `settleWindow` -- the
    /// driver must not silently use a wrong cadence.
    async fn deal_settle_window(
        &self,
        token_contract: &TokenContract,
    ) -> Result<std::time::Duration, ChainError> {
        let tc = parse_tc(token_contract)?;
        let cfg = self
            .chain
            .token_contract_config(&tc)
            .await
            .map_err(map_err)?
            .ok_or_else(|| map_err(anyhow!("getConfig() empty for {token_contract}")))?;
        let settle = cfg["settleWindow"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                map_err(anyhow!(
                    "getConfig().settleWindow missing for {token_contract}"
                ))
            })?;
        Ok(std::time::Duration::from_secs(settle))
    }

    async fn discover_offers(&self) -> Result<Vec<crate::chain::OfferListing>, ChainError> {
        // Book discovery is the buyer's/monitor's job; the seller does not scan the listing.
        Ok(Vec::new())
    }

    async fn post_offer(&self, offer: SellOffer, _note: &dyn Note) -> Result<(), ChainError> {
        let tc = parse_tc(&offer.token_contract)?;
        self.assert_note_can_post_sell_offer().await?;
        // (symmetric branch-3 guard): fail closed if this note's on-chain owner key
        // (`getDetails().ephemeralPubkey`) is not the key we sign `postSellOffer` with -- otherwise
        // `onlyOwnerPubkey` reverts pre-accept(ERR_INVALID_SENDER 101) and the ask never rests (only an
        // opaque TVM_ERROR). Run it before the IOB deploy / offer write.
        retry_seller_read("seller note owner", || async {
            self.chain
                .assert_note_owner_matches("seller post_offer", &self.note, &self.keys)
                .await
                .map_err(map_err)
        })
        .await?;
        // An operate exception: if the per-model `InferenceOrderBook` is not yet deployed --
        // deploy it(model listing; the address is derived from `model_hash`). This is operate, NOT actor provisioning.
        let ob = retry_seller_read("seller order-book address", || async {
            self.chain
                .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
                .await
                .map_err(map_err)
        })
        .await?;
        if retry_seller_read("seller order-book state", || async {
            self.chain
                .inference_orderbook_stats(&ob)
                .await
                .map_err(map_err)
        })
        .await?
        .is_none()
        {
            self.chain
                .deploy_inference_orderbook(
                    &self.note,
                    &self.keys,
                    &self.model_hash,
                    &self.model_name,
                    self.tick_size,
                )
                .await
                .map_err(map_err)?;
        }
        let (price_per_tick, max_ticks) = self
            .sell_offer_terms(&offer.token_contract)
            .await?
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "TokenContract {} getDeal unavailable: run `dexdo provision` for a deployed per-deal TC \
                     or pass --market for the provisioned manifest",
                    offer.token_contract
                ))
        })?;
        if offer.price_per_tick != price_per_tick || offer.max_ticks != max_ticks {
            eprintln!(
                "seller offer terms are bound to TokenContract.getDeal; ignoring drifted CLI values: \
                 token_contract={} requested_price_per_tick={} requested_max_ticks={} \
                 onchain_price_per_tick={} onchain_max_ticks={}",
                offer.token_contract,
                offer.price_per_tick,
                offer.max_ticks,
                price_per_tick,
                max_ticks
            );
        }
        *self.offer_post_started_at.lock().map_err(|_| {
            ChainError::Chain("seller offer submission marker lock poisoned".to_string())
        })? = Some(now_secs().saturating_sub(1));
        match tokio::time::timeout(
            POST_SELL_OFFER_SUBMIT_TIMEOUT,
            // One seller call: postSellOffer(flags, nonce). The note derives the canonical TC and
            // hands it the baked book hash; the TC posts its own ask. The on-chain terms read + drift check
            // above stay as a pre-post sanity check.
            self.chain
                .post_sell_offer(&self.note, &self.keys, 0, self.nonce),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(map_err(e)),
            Err(_) => {
                let (canonical_evidence, tc_state_evidence) =
                    self.post_offer_failure_evidence(&tc).await;
                return Err(ChainError::Chain(seller_post_sell_offer_timeout_message(
                    &ob,
                    &offer.token_contract,
                    &self.model_hash,
                    self.nonce,
                    &self.note,
                    POST_SELL_OFFER_SUBMIT_TIMEOUT,
                    &canonical_evidence,
                    &tc_state_evidence,
                )));
            }
        }
        Ok(())
    }

    async fn confirm_offer_outcome(
        &self,
        tc: &TokenContract,
    ) -> Result<Option<SellOfferOutcome>, ChainError> {
        let ob = retry_seller_read("seller outcome order-book address", || async {
            self.chain
                .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
                .await
                .map_err(map_err)
        })
        .await?;
        let tc_addr = parse_tc(tc)?;
        let since = self
            .offer_post_started_at
            .lock()
            .map_err(|_| {
                ChainError::Chain("seller offer submission marker lock poisoned".to_string())
            })?
            .ok_or_else(|| {
                ChainError::Chain("seller offer submission marker is missing".to_string())
            })?;
        let started = std::time::Instant::now();
        while started.elapsed() < OFFER_ACCEPTANCE_TIMEOUT {
            let events = retry_seller_read("seller offer outcome events", || async {
                self.chain
                    .seller_offer_events_since(&self.note, &ob, &tc_addr, since)
                    .await
                    .map_err(map_err)
            })
            .await?;
            let matched_state = retry_seller_read("seller immediate-match state", || async {
                self.read_openable_match_once(tc).await
            })
            .await?
            .is_some();
            if let Some(outcome) = classify_seller_offer_outcome(events, matched_state)? {
                return Ok(Some(outcome));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        Err(ChainError::Chain(format!(
            "seller postSellOffer outcome is not yet confirmed for TokenContract {tc}; no placement, match, or returned placement value was observed"
        )))
    }

    async fn sell_offer_terms(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(Shell, u64)>, ChainError> {
        let tc = parse_tc(token_contract)?;
        let Some((tick_size, price_per_tick, max_ticks)) =
            retry_seller_read("seller TokenContract terms", || async {
                self.chain
                    .token_contract_deal_terms(&tc)
                    .await
                    .map_err(map_err)
            })
            .await?
        else {
            return Ok(None);
        };
        if tick_size != self.tick_size {
            return Err(ChainError::Chain(format!(
                "TokenContract {token_contract} tickSize {tick_size} != canonical {}",
                self.tick_size
            )));
        }
        let price = price_per_tick.try_into().map_err(|_| {
            ChainError::Chain(format!(
                "TokenContract {token_contract} pricePerTick {price_per_tick} exceeds CLI Shell range"
            ))
        })?;
        let ticks = max_ticks.try_into().map_err(|_| {
            ChainError::Chain(format!(
                "TokenContract {token_contract} maxTicks {max_ticks} exceeds CLI range"
            ))
        })?;
        Ok(Some((price, ticks)))
    }

    async fn read_openable_match_now(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        retry_seller_read("seller existing-match preflight", || async {
            self.read_openable_match_once(token_contract).await
        })
        .await
    }

    async fn poll_openable_match(
        &self,
        token_contract: &TokenContract,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Option<Match>, ChainError> {
        if let Some(m) = self.read_openable_match_once(token_contract).await? {
            return Ok(Some(m));
        }
        let ob = self
            .chain
            .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
            .await
            .map_err(map_err)?;
        let want = parse_tc(token_contract)?.with_workchain();
        let fills = self
            .chain
            .poll_inference_filled_tcs(&self.note, &ob, false, cursor)
            .await
            .map_err(map_err)?;
        if fills
            .iter()
            .any(|fill| fill.token_contract.eq_ignore_ascii_case(&want))
        {
            return self.read_openable_match_once(token_contract).await;
        }
        Ok(None)
    }

    async fn place_buy(&self, tc: &TokenContract, _note: &dyn Note) -> Result<(), ChainError> {
        let _ = tc;
        Err(wrong_role("place_buy", "buyer"))
    }

    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        self.read_openable_match_once(token_contract)
            .await?
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))
    }

    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        enc_endpoint: Vec<u8>,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let tc = parse_tc(token_contract)?;
        self.ensure_tc_gas(&tc).await?;
        // +: the note posts the probe-commission to the nonce-derived TC from its own ECC[2]
        // (`postProbeCommission`) -- no operator wallet.
        post_probe_commission_and_wait(
            &self.chain,
            &self.note,
            &self.keys,
            self.nonce,
            token_contract,
            &tc,
            self.probe_shell,
        )
        .await?;
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .open_stream(&tc, &self.keys, &enc_endpoint)
            .await
            .map_err(map_err)?;
        wait_tc_bool(&self.chain, &tc, "opened", true).await
    }

    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        let tc = parse_tc(token_contract)?;
        self.chain.read_handover(&tc).await.map_err(map_err)
    }

    async fn advance_tick(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let tc = parse_tc(token_contract)?;
        self.ensure_tc_gas(&tc).await?;
        let read_la = |st: &Option<Value>| -> u128 {
            st.as_ref()
                .and_then(|s| s["lastAdvance"].as_str())
                .and_then(|x| x.parse::<u128>().ok())
                .unwrap_or(0)
        };
        let pre = read_la(
            &self
                .chain
                .token_contract_state(&tc)
                .await
                .map_err(map_err)?,
        );
        self.chain
            .advance_stream(&tc, &self.keys)
            .await
            .map_err(map_err)?;
        for _ in 0..40 {
            if read_la(
                &self
                    .chain
                    .token_contract_state(&tc)
                    .await
                    .map_err(map_err)?,
            ) > pre
            {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Err(ChainError::Chain(format!("TC {tc}: advance did not apply")))
    }

    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        let tc = parse_tc(token_contract)?;
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .advance_stream(&tc, &self.keys)
            .await
            .map_err(map_err)?;
        wait_tc_bool(&self.chain, &tc, "probeAccepted", true).await
    }

    async fn stop(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let _ = token_contract;
        Err(wrong_role("stop", "buyer"))
    }

    async fn release_dispute(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.ensure_tc_gas(&tc).await?;
        self.chain
            .release_dispute(&tc, &self.keys)
            .await
            .map_err(map_err)?;
        wait_tc_bool(&self.chain, &tc, "disputed", false).await?;
        Ok(settle_release(accepted, frozen, deposit, commission))
    }

    async fn seller_timeout(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let _ = token_contract;
        Err(wrong_role("seller_timeout", "buyer"))
    }

    async fn deal_state(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        let tc = parse_tc(token_contract)?;
        Ok(self
            .chain
            .token_contract_state(&tc)
            .await
            .map_err(map_err)?
            .as_ref()
            .map(deal_chain_state_from_json))
    }

    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot> {
        real_tc_snapshot(&self.chain, token_contract).await
    }
}

/// The per-role CLI backend of the **BUYER**: the `ChainBackend` trait for the `dexdo buyer` process. It holds
/// the buyer's identity and **reads
/// the book/state from the chain**(`discover_offers` scans `InferenceOrderBook`). Seller actions
/// (`post_offer`/`read_match`/`open_stream`/`advance_tick`/`accept_probe`/`release_dispute`) are an explicit error.
pub struct RealBuyerBackend {
    chain: RealChainBackend,
    note: Address,
    keys: KeyPair,
    model_hash: String,
    tick_size: u128,
    max_price_per_tick: u128,
    ticks: u128,
    escrow: u128,
    pending_fill: std::sync::Mutex<Option<PendingBuyerFill>>,
}

#[derive(Debug, Clone)]
struct PendingBuyerFill {
    cursor: MatchWatchCursor,
    expected: MatchedFill,
}

impl RealBuyerBackend {
    fn set_pending_fill(&self, pending: Option<PendingBuyerFill>) -> Result<(), ChainError> {
        *self.pending_fill.lock().map_err(|_| {
            ChainError::Chain("buyer fill reconciliation state lock poisoned".to_string())
        })? = pending;
        Ok(())
    }

    fn take_pending_fill(&self) -> Result<Option<PendingBuyerFill>, ChainError> {
        Ok(self
            .pending_fill
            .lock()
            .map_err(|_| {
                ChainError::Chain("buyer fill reconciliation state lock poisoned".to_string())
            })?
            .take())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: RealChainBackend,
        note: Address,
        keys: KeyPair,
        model_hash: String,
        tick_size: u128,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
    ) -> Self {
        Self {
            chain,
            note,
            keys,
            model_hash,
            tick_size,
            max_price_per_tick,
            ticks,
            escrow,
            pending_fill: std::sync::Mutex::new(None),
        }
    }

    /// Assemble the buyer backend + the buyer's note from an **already provisioned** actor: a minted
    /// `PrivateNote`(`note_addr` + owner key). The buyer needs no wallet(the escrow is the note's ECC).
    /// `model_hash` is derived from `frame_model`. Returns the backend and a `RealNote`(handover decryption).
    #[allow(clippy::too_many_arguments)]
    pub fn from_provisioned(
        manifest_path: &str,
        note_addr: &str,
        note_secret_hex: &str,
        frame_model: &str,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
    ) -> Result<(Self, RealNote)> {
        // Issue(track 1): reject an insufficient escrow BEFORE any network call -- otherwise the book
        // accepts the SHELL and orphans it(no match, no bid, no refund). Fail-fast instead of a silent loss.
        check_buy_deposit_headroom(escrow, ticks, max_price_per_tick)
            .map_err(|e| anyhow!("{e}"))?;
        let chain = RealChainBackend::connect(manifest_path)?;
        let note =
            Address::parse(note_addr).map_err(|e| anyhow!("--note-addr {note_addr}: {e}"))?;
        let keys = KeyPair::from_secret_hex(note_secret_hex.trim())
            .map_err(|e| anyhow!("--note-key (SDK secret hex): {e:?}"))?;
        let rn = RealNote::from_secret_hex(note_secret_hex)
            .map_err(|e| anyhow!("--note-key invalid ed25519 seed: {e}"))?;
        let backend = Self::new(
            chain,
            note,
            keys,
            model_hash_for(frame_model),
            MODEL_TICK_SIZE,
            max_price_per_tick,
            ticks,
            escrow,
        );
        Ok((backend, rn))
    }

    async fn require_tc_gas(&self, tc: &Address) -> Result<(), ChainError> {
        let balance = self
            .chain
            .active_native_balance(tc)
            .await
            .map_err(map_err)?;
        if balance <= GAS_HEALTH_MIN {
            return Err(ChainError::Chain(format!(
                "TokenContract {tc} native balance {balance} is at/below gas-health floor \
                 {GAS_HEALTH_MIN}; seller-side top-up is required before this buyer-only write"
            )));
        }
        Ok(())
    }

    async fn orderbook_snapshot(&self) -> Result<OrderBookSnapshot, ChainError> {
        self.chain
            .inference_orderbook_snapshot_for_note(
                &self.note,
                &self.model_hash,
                &self.model_hash,
                self.tick_size,
            )
            .await
            .map_err(map_err)
    }

    /// One complete model-buy read/preflight attempt. Retry ownership belongs to the CLI's
    /// `buyer_quote_selection` boundary; this backend seam must never add another retry loop.
    async fn model_buy_preflight_selection_once(
        &self,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<(String, OrderBookOrder), ChainError> {
        let snapshot = self.orderbook_snapshot().await?;
        let selected = self
            .chain
            .submit_safe_model_buy_ask(&snapshot, ticks, max_price_per_tick)
            .await
            .map_err(map_err)?;
        Ok((snapshot.order_book, selected))
    }

    async fn assert_expected_buy_target(
        &self,
        tc: &Address,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<String, ChainError> {
        let snapshot = self.orderbook_snapshot().await?;
        let asks: Vec<OrderBookOrder> = snapshot.resting_asks().cloned().collect();
        let want = tc.with_workchain().to_ascii_lowercase();
        check_expected_buy_target(&asks, &want, max_price_per_tick, ticks).map_err(|e| {
            ChainError::Chain(format!(
                "buyer target preflight failed for InferenceOrderBook {}: {e}. IOB stats {}",
                snapshot.order_book,
                snapshot
                    .stats
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_else(|| "<book not active>".to_string())
            ))
        })?;
        Ok(snapshot.order_book)
    }

    async fn assert_selected_tc_unused(
        &self,
        token_contract: &str,
        order_book: &str,
    ) -> Result<(), ChainError> {
        let tc = parse_tc(&token_contract.to_string())?;
        let state = self
            .chain
            .token_contract_state(&tc)
            .await
            .map_err(map_err)?;
        check_selected_token_contract_unused(token_contract, state.as_ref()).map_err(|e| {
            ChainError::Chain(format!(
                "buyer selected-TC preflight failed for InferenceOrderBook {order_book}: {e}"
            ))
        })
    }
}

#[async_trait]
impl ChainBackend for RealBuyerBackend {
    fn model_buy_order_book_identity(&self) -> Option<String> {
        RealChainBackend::canonical_inference_orderbook_address(&self.model_hash)
            .ok()
            .map(|address| address.with_workchain())
    }

    async fn discover_offers(&self) -> Result<Vec<crate::chain::OfferListing>, ChainError> {
        // Reading the per-model book from the chain: the address is derived from `model_hash`,
        // each offer carries its own `tokenContract`. The book is not active -> no offers.
        let snapshot = self.orderbook_snapshot().await?;
        let asks = self
            .chain
            .executable_resting_asks(&snapshot)
            .await
            .map_err(map_err)?;
        Ok(asks
            .iter()
            .map(|ask| crate::chain::OfferListing {
                seller_id: ask.owner_note.clone(),
                token_contract: ask.token_contract.clone().unwrap_or_default(),
                price_per_tick: ask.price_per_tick.min(Shell::MAX as u128) as Shell,
                max_ticks: ask.ticks.min(u64::MAX as u128) as u64,
            })
            .collect())
    }

    async fn post_offer(&self, offer: SellOffer, _note: &dyn Note) -> Result<(), ChainError> {
        Err(wrong_role("post_offer", "seller"))
            .map_err(|e| ChainError::Chain(format!("{e} (TC {})", offer.token_contract)))
    }

    async fn place_buy(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let expected = parse_tc(token_contract)?;
        self.require_tc_gas(&expected).await?;
        // (branch-3 guard): fail closed if this note's on-chain owner key (`getDetails().ephemeralPubkey`)
        // is not the `--note-key` we sign `placeInferenceBuy` with -- otherwise `onlyOwnerPubkey` reverts
        // pre-accept(ERR_INVALID_SENDER 101) and the buyer silently 300s-times out in `read_match`.
        self.chain
            .assert_note_owner_matches("buyer place_buy", &self.note, &self.keys)
            .await
            .map_err(map_err)?;
        self.chain
            .assert_note_can_place_inference_buy(&self.note)
            .await
            .map_err(map_err)?;
        // `placeInferenceBuy` is model-book-wide and cannot name a target TC. Fail before moving escrow
        // unless the book's price->time matcher would fund the TC from this market manifest.
        let order_book = self
            .assert_expected_buy_target(&expected, self.ticks, self.max_price_per_tick)
            .await?;
        let expected_tc = expected.with_workchain();
        self.assert_selected_tc_unused(&expected_tc, &order_book)
            .await?;
        // A limit buy by `model_hash`; the book matches the preflighted ask and funds the seller's TC
        // (`fundFromOrderBook`).
        self.chain
            .place_inference_buy(
                &self.note,
                &self.keys,
                &self.model_hash,
                self.max_price_per_tick,
                self.ticks,
                self.escrow,
                0,
                0,
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn assert_model_buy_matches_executable_quote(
        &self,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<(), ChainError> {
        self.model_buy_preflight_selection_once(ticks, max_price_per_tick)
            .await
            .map(|_| ())
    }

    async fn assert_explicit_buy_matches_executable_quote(
        &self,
        token_contract: &TokenContract,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<(), ChainError> {
        let expected = parse_tc(token_contract)?;
        self.require_tc_gas(&expected).await?;
        self.chain
            .assert_note_owner_matches(
                "buyer explicit-token quote preflight",
                &self.note,
                &self.keys,
            )
            .await
            .map_err(map_err)?;
        let order_book = self
            .assert_expected_buy_target(&expected, ticks, max_price_per_tick)
            .await?;
        let expected_tc = expected.with_workchain();
        self.assert_selected_tc_unused(&expected_tc, &order_book)
            .await
    }

    async fn submit_safe_explicit_buy_quote_order(
        &self,
        token_contract: &TokenContract,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<Option<OrderBookOrder>, ChainError> {
        let expected = parse_tc(token_contract)?;
        self.require_tc_gas(&expected).await?;
        self.chain
            .assert_note_owner_matches(
                "buyer explicit-token quote preflight",
                &self.note,
                &self.keys,
            )
            .await
            .map_err(map_err)?;
        let snapshot = self.orderbook_snapshot().await?;
        let asks: Vec<OrderBookOrder> = snapshot.resting_asks().cloned().collect();
        let expected_tc = expected.with_workchain();
        let want = expected_tc.to_ascii_lowercase();
        check_expected_buy_target(&asks, &want, max_price_per_tick, ticks).map_err(|e| {
            ChainError::Chain(format!(
                "buyer target preflight failed for InferenceOrderBook {}: {e}. IOB stats {}",
                snapshot.order_book,
                orderbook_stats_for_error(&snapshot)
            ))
        })?;
        let selected = self
            .chain
            .submit_safe_model_buy_ask(&snapshot, ticks, max_price_per_tick)
            .await
            .map_err(map_err)?;
        if !selected
            .token_contract
            .as_deref()
            .is_some_and(|tc| tc.eq_ignore_ascii_case(&expected_tc))
        {
            return Err(ChainError::Chain(format!(
                "buyer target preflight failed for InferenceOrderBook {}: submit-safe executable quote selected {}, \
                 not expected tokenContract {}. IOB stats {}",
                snapshot.order_book,
                describe_buy_ask(&selected),
                expected_tc,
                orderbook_stats_for_error(&snapshot)
            )));
        }
        self.assert_selected_tc_unused(&expected_tc, &snapshot.order_book)
            .await?;
        Ok(Some(selected))
    }

    fn requires_submit_safe_single_ask_quote(&self) -> bool {
        true
    }

    /// Model-only buy(no pre-known TC): the buyer derives the book from `--frame-model` and places a limit
    /// buy by `model_hash`, accepting whatever resting ask the book's price->time matcher fills. The matched
    /// per-deal `TokenContract` is learned afterwards from this note's own fill event
    /// ([`Self::wait_matched_token_contract`]) -- so the buyer needs only the model name, no seller hand-off.
    async fn place_buy_by_model(
        &self,
        _note: &dyn Note,
        ticks: u128,
        max_price_per_tick: u128,
        escrow: u128,
    ) -> Result<(), ChainError> {
        check_buy_deposit_headroom(escrow, ticks, max_price_per_tick).map_err(ChainError::Chain)?;
        // Same owner-key guard as `place_buy`: the on-chain note owner must be the `--note-key` we sign
        // `placeInferenceBuy` with, else `onlyOwnerPubkey` reverts pre-accept(ERR_INVALID_SENDER 101).
        self.chain
            .assert_note_owner_matches("buyer place_buy_by_model", &self.note, &self.keys)
            .await
            .map_err(map_err)?;
        self.chain
            .assert_note_can_place_inference_buy(&self.note)
            .await
            .map_err(map_err)?;
        // This fresh pre-submit safety check is one attempt only. In particular it must not
        // multiply the CLI's bounded quote retry, and the money-moving call below is never retried.
        let (order_book, selected) = self
            .model_buy_preflight_selection_once(ticks, max_price_per_tick)
            .await?;
        let selected_tc = selected.token_contract.as_deref().ok_or_else(|| {
            ChainError::Chain(format!(
                "buyer model-only preflight failed for InferenceOrderBook {}: selected order #{} has no TokenContract",
                order_book, selected.order_id
            ))
        })?;
        self.assert_selected_tc_unused(selected_tc, &order_book)
            .await?;
        let expected = MatchedFill {
            token_contract: parse_tc(&selected_tc.to_string())?.with_workchain(),
            ticks,
            price_per_tick: selected.price_per_tick,
        };
        let ob = Address::parse(&order_book).map_err(|e| {
            ChainError::Chain(format!(
                "buyer model-only preflight returned invalid InferenceOrderBook {order_book}: {e}"
            ))
        })?;
        // Prime the durable cursor immediately before the one money-moving submit. This consumes every
        // already-visible fill, including stale fills created in the same wall-clock second.
        let mut cursor = MatchWatchCursor::new(0);
        self.chain
            .poll_inference_filled_tcs(&self.note, &ob, true, &mut cursor)
            .await
            .map_err(map_err)?;
        self.set_pending_fill(Some(PendingBuyerFill { cursor, expected }))?;
        // The order the buyer chose after seeing the book -- NOT the backend's construction-time defaults.
        let submit = self
            .chain
            .place_inference_buy(
                &self.note,
                &self.keys,
                &self.model_hash,
                max_price_per_tick,
                ticks,
                escrow,
                0,
                0,
            )
            .await
            .map_err(map_err);
        if submit.is_err() {
            self.set_pending_fill(None)?;
        }
        submit?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn place_buy_by_model_with_submit_identity(
        &self,
        _note: &dyn Note,
        quoted_order: Option<&OrderBookOrder>,
        ticks: u128,
        max_price_per_tick: u128,
        escrow: u128,
        cursor: &mut MatchWatchCursor,
        before_post: &mut (dyn FnMut(String, MatchWatchCursor) -> Result<(), ChainError> + Send),
    ) -> Result<(), ChainError> {
        check_buy_deposit_headroom(escrow, ticks, max_price_per_tick).map_err(ChainError::Chain)?;
        self.chain
            .assert_note_owner_matches("buyer place_buy_by_model", &self.note, &self.keys)
            .await
            .map_err(map_err)?;
        self.chain
            .assert_note_can_place_inference_buy(&self.note)
            .await
            .map_err(map_err)?;
        let (order_book, selected) = self
            .model_buy_preflight_selection_once(ticks, max_price_per_tick)
            .await?;
        if quoted_order != Some(&selected) {
            return Err(ChainError::Chain(
                "buyer pre-submit matcher head differs from the rendered quote; no escrow was sent"
                    .to_string(),
            ));
        }
        let selected_tc = selected.token_contract.as_deref().ok_or_else(|| {
            ChainError::Chain(format!(
                "buyer model-only preflight failed for InferenceOrderBook {order_book}: selected order #{} has no TokenContract",
                selected.order_id
            ))
        })?;
        self.assert_selected_tc_unused(selected_tc, &order_book)
            .await?;
        let expected = MatchedFill {
            token_contract: parse_tc(&selected_tc.to_string())?.with_workchain(),
            ticks,
            price_per_tick: selected.price_per_tick,
        };
        let expected_for_callback = expected.clone();
        let order_book = Address::parse(&order_book).map_err(|error| {
            ChainError::Chain(format!(
                "buyer model-only preflight returned invalid InferenceOrderBook {order_book}: {error}"
            ))
        })?;
        self.set_pending_fill(Some(PendingBuyerFill {
            cursor: MatchWatchCursor::default(),
            expected,
        }))?;
        let mut callback = |identity: String, final_cursor: MatchWatchCursor| {
            self.set_pending_fill(Some(PendingBuyerFill {
                cursor: final_cursor.clone(),
                expected: expected_for_callback.clone(),
            }))?;
            before_post(identity, final_cursor).map_err(anyhow::Error::new)
        };
        let result = self
            .chain
            .place_inference_buy_with_submit_identity(
                &self.note,
                &self.keys,
                &order_book,
                &self.model_hash,
                max_price_per_tick,
                ticks,
                escrow,
                0,
                0,
                cursor,
                &mut callback,
            )
            .await
            .map_err(map_err);
        if result
            .as_ref()
            .is_err_and(|error| !matches!(error, ChainError::AmbiguousSubmit(_)))
        {
            self.set_pending_fill(None)?;
        }
        result.map(|_| ())
    }

    async fn poll_matched_model_buys_for_order_book(
        &self,
        order_book: &str,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>, ChainError> {
        let order_book = Address::parse(order_book).map_err(|error| {
            ChainError::Chain(format!(
                "buyer recovery has invalid InferenceOrderBook address {order_book}: {error}"
            ))
        })?;
        self.chain
            .poll_inference_filled_tcs(&self.note, &order_book, true, cursor)
            .await
            .map_err(map_err)
    }

    async fn poll_attributed_model_buys_for_order_book(
        &self,
        order_book: &str,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<(u128, MatchedFill)>, ChainError> {
        let order_book = Address::parse(order_book).map_err(|error| {
            ChainError::Chain(format!(
                "subscription recovery has invalid InferenceOrderBook address {order_book}: {error}"
            ))
        })?;
        self.chain
            .poll_inference_attributed_fills(&self.note, &order_book, cursor)
            .await
            .map_err(map_err)
    }

    async fn subscription_placements_since(
        &self,
        order_book: &str,
        buyer_note: &str,
        order_id_floor: u128,
        max_price_per_tick: u128,
        ticks: u128,
        cycle_budget: u128,
        auto_renew: bool,
    ) -> Result<Vec<crate::chain::InferenceSubscriptionPlacement>, ChainError> {
        let order_book = Address::parse(order_book).map_err(|error| {
            ChainError::Chain(format!(
                "subscription recovery has invalid InferenceOrderBook address {order_book}: {error}"
            ))
        })?;
        let buyer_note = Address::parse(buyer_note).map_err(|error| {
            ChainError::Chain(format!(
                "subscription recovery has invalid buyer note address {buyer_note}: {error}"
            ))
        })?;
        self.chain
            .inference_subscription_placements_since(
                &order_book,
                &buyer_note,
                order_id_floor,
                max_price_per_tick,
                ticks,
                cycle_budget,
                auto_renew,
            )
            .await
            .map_err(map_err)
    }

    async fn buyer_order_is_active_for_owner(
        &self,
        order_book: &str,
        order_id: u128,
        buyer_note: &str,
    ) -> Result<bool, ChainError> {
        let order_book = Address::parse(order_book).map_err(|error| {
            ChainError::Chain(format!(
                "buyer order recovery has invalid InferenceOrderBook address {order_book}: {error}"
            ))
        })?;
        self.chain
            .inference_buyer_order_is_active_for_owner(&order_book, order_id, buyer_note)
            .await
            .map_err(map_err)
    }

    /// Learn the matched per-deal `TokenContract` from THIS note's owner-facing `InferenceFilledConfirmed`
    /// ext-out: derive the per-model book from `model_hash`, then read the note's own
    /// fill event for this book's BUY side. No shared-book index.
    async fn wait_matched_token_contract(
        &self,
        since_unix: i64,
        timeout: std::time::Duration,
    ) -> Result<Option<MatchedFill>, ChainError> {
        let ob = self
            .chain
            .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
            .await
            .map_err(map_err)?;
        let pending = self.take_pending_fill()?;
        let mut cursor = pending
            .as_ref()
            .map(|pending| pending.cursor.clone())
            .unwrap_or_else(|| MatchWatchCursor::new(since_unix));
        let fill = self
            .chain
            .wait_inference_filled_tc(
                &self.note,
                &ob,
                since_unix,
                timeout,
                &mut cursor,
                pending.as_ref().map(|pending| &pending.expected),
            )
            .await
            .map_err(map_err)?;
        Ok(Some(fill))
    }

    async fn assert_model_only_resume_target(
        &self,
        token_contract: &TokenContract,
    ) -> Result<(), ChainError> {
        let tc = parse_tc(token_contract)?;
        let snapshot = self.orderbook_snapshot().await?;
        let state = self
            .chain
            .token_contract_state(&tc)
            .await
            .map_err(map_err)?
            .as_ref()
            .map(deal_chain_state_from_json);
        let model_name = self
            .chain
            .token_contract_model_name(&tc)
            .await
            .map_err(map_err)?;
        let model_hash = self
            .chain
            .token_contract_model_hash(&tc)
            .await
            .map_err(map_err)?;
        let buyer_note = self
            .chain
            .token_contract_buyer_note(&tc)
            .await
            .map_err(map_err)?
            .map(|a| a.with_workchain());
        let buyer_pubkey = self
            .chain
            .token_contract_buyer_pubkey(&tc)
            .await
            .map_err(map_err)?;
        let active_order_book = snapshot.active().then_some(snapshot.order_book.as_str());
        let expected_buyer_pubkey = keypair_ed_pubkey(&self.keys).map_err(map_err)?;
        validate_model_only_resume_facts(
            token_contract,
            ModelOnlyResumeFacts {
                state,
                model_name: model_name.as_deref(),
                model_hash: model_hash.as_deref(),
                buyer_note: buyer_note.as_deref(),
                buyer_pubkey: buyer_pubkey.as_ref(),
                order_book: active_order_book,
            },
            &self.model_hash,
            &self.note.with_workchain(),
            &expected_buyer_pubkey,
            now_secs(),
        )
    }

    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        let _ = token_contract;
        Err(wrong_role("read_match", "seller"))
    }

    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        _enc_endpoint: Vec<u8>,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let _ = token_contract;
        Err(wrong_role("open_stream", "seller"))
    }

    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        let tc = parse_tc(token_contract)?;
        self.chain.read_handover(&tc).await.map_err(map_err)
    }

    async fn advance_tick(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let _ = token_contract;
        Err(wrong_role("advance_tick", "seller"))
    }

    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        let _ = token_contract;
        Err(wrong_role("accept_probe", "seller"))
    }

    async fn stop(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, accepted, prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.require_tc_gas(&tc).await?;
        self.chain
            .stream_stop(&self.note, &self.keys, &tc)
            .await
            .map_err(map_err)?;
        wait_tc_bool(&self.chain, &tc, "opened", false).await?;
        Ok(settle_stop(accepted, prepaid, frozen, deposit, commission))
    }

    async fn dispute(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.require_tc_gas(&tc).await?;
        self.chain
            .stream_dispute(&self.note, &self.keys, &tc)
            .await
            .map_err(map_err)?;
        wait_tc_bool(&self.chain, &tc, "disputed", true).await?;
        Ok(settle_release(accepted, frozen, deposit, commission))
    }

    async fn release_dispute(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let _ = token_contract;
        Err(wrong_role("release_dispute", "seller"))
    }

    async fn seller_timeout(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, _accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.require_tc_gas(&tc).await?;
        self.chain
            .reclaim_on_timeout(&self.note, &self.keys, &tc)
            .await
            .map_err(map_err)?;
        wait_tc_bool(&self.chain, &tc, "opened", false).await?;
        Ok(Settlement::SellerNoShow {
            to_buyer_refund: (frozen + deposit) as Shell,
            seller_commission_returned: commission as Shell,
        })
    }

    async fn cleanup_unopened(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let tc = parse_tc(token_contract)?;
        let (_opened, _accepted, _prepaid, frozen, deposit, commission) =
            tc_settle_state(&self.chain, &tc).await.map_err(map_err)?;
        self.require_tc_gas(&tc).await?;
        self.chain
            .stream_cleanup(&self.note, &self.keys, &tc)
            .await
            .map_err(map_err)?;
        for _ in 0..40 {
            match self
                .chain
                .token_contract_state(&tc)
                .await
                .map_err(map_err)?
            {
                None => {
                    return Ok(Settlement::SellerNoShow {
                        to_buyer_refund: (frozen + deposit) as Shell,
                        seller_commission_returned: commission as Shell,
                    });
                }
                Some(st) if !st["funded"].as_bool().unwrap_or(true) => {
                    return Ok(Settlement::SellerNoShow {
                        to_buyer_refund: (frozen + deposit) as Shell,
                        seller_commission_returned: commission as Shell,
                    });
                }
                Some(_) => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
            }
        }
        Err(ChainError::Chain(format!(
            "TC {tc}: cleanupUnopened did not clear funded state within the allotted time"
        )))
    }

    async fn deal_state(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        let tc = parse_tc(token_contract)?;
        Ok(self
            .chain
            .token_contract_state(&tc)
            .await
            .map_err(map_err)?
            .as_ref()
            .map(deal_chain_state_from_json))
    }

    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot> {
        real_tc_snapshot(&self.chain, token_contract).await
    }
}

#[cfg(test)]
mod note_tests {
    use super::*;
    use crate::note::verify;

    /// Offline(no chain/keys): the SDK `KeyPair`(ed25519) signature is verified by dexdo `verify`
    /// and the x25519 handover round-trips.
    #[test]
    fn real_note_sign_verifies_and_handover_roundtrips() {
        let note = RealNote::generate();
        let msg = b"stream-session-challenge";
        let sig = note.sign(msg);
        assert!(
            verify(&note.pubkey(), msg, &sig),
            "the SDK KeyPair ed25519 signature is verified by dexdo-verify"
        );

        let buyer = RealNote::generate();
        let ct = note.encrypt_to(&buyer.pubkey(), b"https://gw:443|fingerprint");
        assert_eq!(buyer.decrypt(&ct).unwrap(), b"https://gw:443|fingerprint");
    }
}

#[cfg(test)]
mod codecell_tests {
    use super::*;

    /// Offline(no network): extracting the code-cell from the embedded `.tvc` works -- `InferenceOrderBook`
    /// yields a non-empty base64-BOC(the `code` argument for deploying the book) and a stable 32-byte
    /// code-hash; `PrivateNote.tvc` also parses. Meaning a book deploy will not hit the chain codec.
    #[test]
    fn tvc_code_cell_extraction() {
        let ob_code = RealChainBackend::inference_orderbook_code_b64().expect("OB code b64");
        assert!(!ob_code.is_empty(), "OB code-cell base64 is non-empty");
        let ob_hash = code_hash(INFERENCE_ORDERBOOK_TVC).expect("OB code hash");
        assert_eq!(ob_hash.len(), 64, "code-hash -- 32 bytes in hex");
        let pn_hash = code_hash(PRIVATENOTE_TVC).expect("PN code hash");
        assert_eq!(pn_hash.len(), 64);
        println!("InferenceOrderBook code_hash = {ob_hash}");
        println!("PrivateNote        code_hash = {pn_hash}");
    }

    /// pure regression: stale binary pins fail loud with actionable text, while matching pins pass.
    #[test]
    fn doctor_code_hash_compare_flags_stale_binary() {
        let ok = code_hash_check(
            "TokenContract code hash",
            None,
            ROOTMODEL_PINNED_TC_CODE_HASH,
            Some(ROOTMODEL_PINNED_TC_CODE_HASH),
        );
        assert_eq!(ok.status, ShellnetDoctorStatus::Pass);

        let stale = code_hash_check(
            "TokenContract code hash",
            None,
            ROOTMODEL_PINNED_TC_CODE_HASH,
            Some("0000000000000000000000000000000000000000000000000000000000000001"),
        );
        assert_eq!(stale.status, ShellnetDoctorStatus::Fail);
        assert!(stale.message.contains("STALE"), "{}", stale.message);
        assert!(
            stale.message.contains("rebuild from dev HEAD"),
            "{}",
            stale.message
        );
    }

    /// pure regression: manifest freshness is a fail-closed active-account check.
    #[test]
    fn doctor_manifest_active_check_fails_stale_manifest() {
        let addr =
            Address::parse("0:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .expect("addr");
        let fresh = active_check("market TokenContract state", &addr, true);
        assert_eq!(fresh.status, ShellnetDoctorStatus::Pass);
        let stale = active_check("market TokenContract state", &addr, false);
        assert_eq!(stale.status, ShellnetDoctorStatus::Fail);
        assert!(stale.message.contains("inactive"), "{}", stale.message);
    }

    /// regression: the deploy-send 404 tolerance is SPECIFIC to the BK `/v2/account` lookup (a
    /// funded-uninit deploy target), NOT a blanket "contains 404". A 404 from any other URL/cause, or
    /// any non-404 error, must classify as NOT-uninit so it propagates as a real error -- and so the
    /// self-dapp fallback can only ever flip routing for the funded-uninit deploy case.
    #[test]
    fn uninit_account_404_is_specific() {
        // The exact reqwest error `fetch_dapp_id` produces on a funded-uninit deploy target -> uninit.
        assert!(is_uninit_account_404(
            "HTTP status client error (404 Not Found) for url \
             (https://shellnet.ackinacki.org/v2/account?account_id=6606&dapp_id=6606)"
        ));
        // A 404 from a DIFFERENT endpoint is NOT the uninit-account case -> must propagate.
        assert!(!is_uninit_account_404(
            "HTTP status client error (404 Not Found) for url (https://shellnet.ackinacki.org/v2/messages)"
        ));
        // A non-404 error on `/v2/account`(transport/5xx) is NOT uninit -> must propagate.
        assert!(!is_uninit_account_404(
            "HTTP status server error (502 Bad Gateway) for url \
             (https://shellnet.ackinacki.org/v2/account?account_id=x&dapp_id=x)"
        ));
        assert!(!is_uninit_account_404(
            "transport error: connection refused"
        ));
        assert!(!is_uninit_account_404(""));
    }

    /// regression: an active contract at or below the gas-health floor must get topped up before the
    /// next RootModel/TC poke; above the floor it is left alone.
    #[test]
    fn gas_health_top_up_is_thresholded_and_targets_working_level() {
        assert_eq!(
            gas_health_top_up_amount(GAS_HEALTH_MIN - 1, GAS_HEALTH_MIN, GAS_HEALTH_TARGET),
            Some(GAS_HEALTH_TARGET - (GAS_HEALTH_MIN - 1))
        );
        assert_eq!(
            gas_health_top_up_amount(GAS_HEALTH_MIN, GAS_HEALTH_MIN, GAS_HEALTH_TARGET),
            Some(GAS_HEALTH_TARGET - GAS_HEALTH_MIN)
        );
        assert_eq!(
            gas_health_top_up_amount(GAS_HEALTH_MIN + 1, GAS_HEALTH_MIN, GAS_HEALTH_TARGET),
            None
        );
    }

    /// (offline): the `RealNote` x25519 handover is derived from its ed25519 -- the seller **reconstructs
    /// the buyer's pubkey from on-chain `getBuyerPubkey`(ed25519)**, no separate x25519 channel is needed.
    /// This removes the per-role blocker: the seller encrypts the handover to the pubkey recovered from the chain.
    #[test]
    fn realnote_x25519_handover_derives_from_ed25519() {
        use crate::note::{verify, x25519_pub_from_ed25519_pub, NotePubkey};
        // F2: pin the SDK-seed INVARIANT -- `from_keypair` slices
        // `secret_hex()[..32]`, assuming it is the ed25519 seed. If the SDK changes the secret format,
        // the handover derivation from ed will become silently incorrect. So we check explicitly that
        // `SigningKey::from_bytes(seed).verifying_key() == public_hex()` BEFORE building the note --
        // the invariant must survive any refactor of the test.
        let keypair = KeyPair::generate();
        let seed_bytes =
            decode_hex(keypair.secret_hex().trim_start_matches("0x")).expect("secret hex");
        assert!(seed_bytes.len() >= 32, "SDK ed25519 secret >= 32 bytes");
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&seed_bytes[..32]);
        let sdk_pub =
            decode_hex(keypair.public_hex().trim_start_matches("0x")).expect("public hex");
        assert_eq!(
            ed25519_dalek::SigningKey::from_bytes(&seed)
                .verifying_key()
                .to_bytes()[..],
            sdk_pub[..],
            "SDK invariant: secret_hex()[..32] is the ed25519 seed (its verifying_key == public_hex())"
        );

        let note = RealNote::from_keypair(keypair).expect("real note from valid SDK keypair");
        let pk = note.pubkey();
        // The handover's x25519 == Montgomery(ed25519) -> reconstructible from on-chain ed25519.
        assert_eq!(
            x25519_pub_from_ed25519_pub(&pk.ed),
            Some(pk.x),
            "x25519 is derived from the note's ed25519"
        );
        // Round-trip through the pubkey RECONSTRUCTED from ed(the seller's path from getBuyerPubkey):
        let recon_x = x25519_pub_from_ed25519_pub(&pk.ed).unwrap();
        let seller = RealNote::generate();
        let ct = seller.encrypt_to(
            &NotePubkey {
                x: recon_x,
                ed: pk.ed,
            },
            b"endpoint|fp",
        );
        assert_eq!(
            note.decrypt(&ct).unwrap(),
            b"endpoint|fp",
            "round-trip through the x25519 reconstructed from ed25519"
        );
        // The challenge on the same ed25519 note key -- the signature verifies.
        let sig = note.sign(b"challenge");
        assert!(
            verify(&pk, b"challenge", &sig),
            "the note's ed25519 signature"
        );
    }

    /// Offline guard for step 2: the embedded `TokenContract.tvc` code-hash matches the
    /// `RootModel.TOKEN_CONTRACT_CODE_HASH` pin. Otherwise the TC deploy is useless -- RootModel rejects
    /// the deal registration(the derived address won't match `msg.sender`). Catches a desync between the
    /// embedded image and the RootModel deployed on shellnet BEFORE any write.
    #[test]
    fn token_contract_code_hash_matches_rootmodel_pin() {
        let tc_hash = code_hash(TOKENCONTRACT_TVC).expect("TC code hash");
        println!("TokenContract code_hash = {tc_hash}");
        println!("RootModel pinned        = {ROOTMODEL_PINNED_TC_CODE_HASH}");
        assert_eq!(
            tc_hash, ROOTMODEL_PINNED_TC_CODE_HASH,
            "TokenContract.tvc code-hash must == RootModel.TOKEN_CONTRACT_CODE_HASH"
        );

        // RootModel.tvc code-hash == SuperRoot.ROOT_MODEL_CODE_HASH -- otherwise SuperRoot rejects
        // registerRoot and the seller cannot provision their RootModel.
        let rm_hash = code_hash(ROOTMODEL_TVC).expect("RM code hash");
        println!("RootModel code_hash = {rm_hash}");
        println!("SuperRoot pinned    = {SUPERROOT_PINNED_RM_CODE_HASH}");
        assert_eq!(
            rm_hash, SUPERROOT_PINNED_RM_CODE_HASH,
            "RootModel.tvc code-hash must == SuperRoot.ROOT_MODEL_CODE_HASH"
        );
    }

    /// offline guard for `assert_seller_note_current`: the embedded `PrivateNote.tvc` code-hash matches
    /// the deployed 4.0.15 PrivateNote. The runtime guard rejects a note whose on-chain `code_hash` !=
    /// `code_hash(PRIVATENOTE_TVC)`; if the embedded image drifted from the deployed code this pin breaks,
    /// warning that the guard would false-reject valid fresh notes. Keeps the guard's baseline honest.
    #[test]
    fn private_note_code_hash_matches_deployed_pin() {
        let pn_hash = code_hash(PRIVATENOTE_TVC).expect("PN code hash");
        println!("PrivateNote code_hash = {pn_hash}");
        println!("deployed pinned       = {PRIVATENOTE_PINNED_CODE_HASH}");
        assert_eq!(
            pn_hash, PRIVATENOTE_PINNED_CODE_HASH,
            "embedded PrivateNote.tvc code-hash must == deployed 4.0.15 PrivateNote (else the  guard false-rejects)"
        );
    }

    /// the offline code_hash gate behind `assert_seller_note_current` (now also enforced
    /// on the seller-daemon offer-publish path via `RealSellerBackend::assert_note_current`). A note whose
    /// on-chain code_hash != the pinned current PrivateNote -- orphaned by a redeploy, e.g. the live `00028b50...`
    /// 4.0.8-era note probed for -- is rejected with an actionable "re-mint" message; the current pin passes.
    #[test]
    fn note_code_hash_current_rejects_stale_with_remint() {
        let note =
            Address::parse("0:988322d9cbffc133b491ef09885d3811ce03a54ef5ae8ac94019bddea4d3736e")
                .expect("parse note address");
        let err = note_code_hash_current(
            &note,
            Some("00028b507121895f02de742cf6aa966af106e0e430f20a75b288f62aa068a8f6"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Re-mint"), "{err}");
        assert!(err.contains("code_hash"), "{err}");
        // A missing code_hash(uninit/none) is also rejected fail-closed.
        assert!(note_code_hash_current(&note, None).is_err());
        // The current pinned hash passes.
        assert!(note_code_hash_current(&note, Some(PRIVATENOTE_PINNED_CODE_HASH)).is_ok());
    }

    /// the per-deal TC freshness gate behind `RealSellerBackend::assert_token_contract_fresh`.
    /// A fresh active-but-unfunded TC(all flags false, all amounts "0") is reusable -> `None`; a TC already used
    /// by a prior deal -- opened(the live 321 case) / funded / disputed / residual deposit/prepaid/frozen/finalized
    /// -- is rejected with the offending reason so the seller fails closed before `postSellOffer`.
    #[test]
    fn token_contract_used_reason_flags_used_states() {
        let fresh = json!({"funded": false, "opened": false, "probeAccepted": false, "disputed": false,
            "deposit": "0", "prepaid": "0", "frozen": "0", "finalizedOwed": "0"});
        assert_eq!(token_contract_used_reason(&fresh), None);
        assert!(check_selected_token_contract_unused("0:fresh", Some(&fresh)).is_ok());
        let unreadable = check_selected_token_contract_unused("0:missing", None)
            .expect_err("unreadable selected TC must fail closed");
        assert!(
            unreadable.contains("not readable by getState"),
            "{unreadable}"
        );
        // The live case: opened(+ funded + a frozen probe tick) -> used, reason names each.
        let opened = json!({"funded": true, "opened": true, "probeAccepted": false, "disputed": false,
            "deposit": "0", "prepaid": "0", "frozen": "1000", "finalizedOwed": "0"});
        let r = token_contract_used_reason(&opened).expect("opened TC must be flagged used");
        assert!(r.contains("opened"), "{r}");
        assert!(r.contains("funded"), "{r}");
        assert!(r.contains("frozen=1000"), "{r}");
        let selected = check_selected_token_contract_unused("0:used", Some(&opened))
            .expect_err("used selected TC must fail closed");
        assert!(
            selected.contains("already used by chain state"),
            "{selected}"
        );
        assert!(selected.contains("funded"), "{selected}");
        // Residual deposit alone(a closed-but-not-destroyed deal) -> used.
        assert_eq!(
            token_contract_used_reason(&json!({"funded": false, "opened": false, "probeAccepted": false,
                "disputed": false, "deposit": "500", "prepaid": "0", "frozen": "0", "finalizedOwed": "0"}))
                .as_deref(),
            Some("deposit=500")
        );
        assert!(check_selected_token_contract_unused(
            "0:residual",
            Some(&json!({"funded": false, "opened": false, "probeAccepted": false,
                "disputed": false, "deposit": "0x1f4", "prepaid": "0", "frozen": "0", "finalizedOwed": "0"}))
        )
        .expect_err("residual selected TC must fail closed")
        .contains("deposit=500"));
        // Disputed alone -> used.
        assert!(token_contract_used_reason(&json!({"opened": false, "funded": false, "disputed": true,
            "probeAccepted": false, "deposit": "0", "prepaid": "0", "frozen": "0", "finalizedOwed": "0"}))
            .unwrap()
            .contains("disputed"));
    }

    /// resume regression: seller resume may skip `postSellOffer` for a funded pre-stream TC and for
    /// an active already-opened stream(gateway restart must rebuild auth). Terminal/disputed states still block.
    #[test]
    fn token_contract_resume_blocker_rejects_used_stream_state() {
        let pre_open = json!({"funded": true, "opened": false, "probeAccepted": false, "disputed": false,
            "deposit": "10000", "prepaid": "0", "frozen": "0", "finalizedOwed": "0"});
        assert_eq!(token_contract_resume_blocker(&pre_open), None);

        let opened = json!({"funded": true, "opened": true, "probeAccepted": false, "disputed": false,
            "deposit": "0", "prepaid": "0", "frozen": "1000", "finalizedOwed": "0"});
        assert_eq!(token_contract_resume_blocker(&opened), None);

        let stopped = json!({"funded": true, "opened": false, "probeAccepted": true, "disputed": false,
            "deposit": "0", "prepaid": "0", "frozen": "0", "finalizedOwed": "2000"});
        let r = token_contract_resume_blocker(&stopped).expect("stopped state blocks resume");
        assert!(r.contains("probeAccepted without opened"), "{r}");

        let disputed = json!({"funded": true, "opened": true, "probeAccepted": false, "disputed": true,
            "deposit": "0", "prepaid": "0", "frozen": "1000", "finalizedOwed": "0"});
        let r = token_contract_resume_blocker(&disputed).expect("disputed state blocks resume");
        assert!(r.contains("disputed"), "{r}");
    }

    #[test]
    fn outcome_confirmation_distinguishes_rested_matched_and_duplicate() {
        let rested = classify_seller_offer_outcome(
            SellerOfferEvents {
                placed_order_id: Some(835),
                ..Default::default()
            },
            false,
        )
        .expect("rested outcome");
        assert_eq!(rested, Some(SellOfferOutcome::Rested { order_id: 835 }));

        let matched = classify_seller_offer_outcome(SellerOfferEvents::default(), true)
            .expect("matched outcome");
        assert_eq!(matched, Some(SellOfferOutcome::Matched));

        let duplicate = classify_seller_offer_outcome(
            SellerOfferEvents {
                placement_value_returned: true,
                ..Default::default()
            },
            false,
        )
        .expect_err("returned placement value is a duplicate");
        assert_eq!(duplicate.to_string(), DUPLICATE_SELL_MESSAGE);
        assert!(!duplicate.to_string().contains("CHAIN_TRANSPORT"));
    }

    #[tokio::test]
    async fn transient_read_failure_retries_with_backoff() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let started = std::time::Instant::now();
        let outcome = retry_seller_read("test seller outcome", {
            let attempts = attempts.clone();
            move || {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(ChainError::Transport("timed out".to_string()))
                    } else {
                        Ok(Some(SellOfferOutcome::Rested { order_id: 7 }))
                    }
                }
            }
        })
        .await
        .expect("second read succeeds");
        assert_eq!(outcome, Some(SellOfferOutcome::Rested { order_id: 7 }));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(started.elapsed() >= SELLER_READ_BACKOFF[0]);
    }

    #[tokio::test]
    async fn empty_book_slow_read_does_not_report_duplicate_tc() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = retry_seller_read("empty order book", {
            let attempts = attempts.clone();
            move || {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(ChainError::Transport("slow read timed out".to_string()))
                    } else {
                        Ok(SellerOfferEvents {
                            placed_order_id: Some(1),
                            ..Default::default()
                        })
                    }
                }
            }
        })
        .await
        .and_then(|events| classify_seller_offer_outcome(events, false));
        assert_eq!(
            result.unwrap(),
            Some(SellOfferOutcome::Rested { order_id: 1 })
        );
    }

    /// negative regression: a shellnet submit stall must not leave `dexdo seller` hanging forever or
    /// pretend a message hash exists. The operator gets exact TC/book context and the by-fact derivation state.
    #[test]
    fn seller_post_sell_offer_timeout_message_is_precise_and_hash_free() {
        let ob =
            Address::parse("0:6330b82c9d866f68e989d4f71c79e6f4757602c065933b7e63179b00acd9aa0e")
                .expect("ob");
        let note =
            Address::parse("0:c60ff3783e78ce3feba2236b35403639a2a434ba9f3c6c351813a87ab98c9331")
                .expect("note");
        let tc = "0:9aff5b8520caf32dbb91390134a946fc9c2896830d96b86cb0f1fbd2262dbe36";
        let msg = seller_post_sell_offer_timeout_message(
            &ob,
            tc,
            "0xe3cc0b0b5cdadfaee3d9b9adf50b489a09f2d7540cb9436ef15423fe27b91a09",
            1783558097,
            &note,
            std::time::Duration::from_secs(120),
            "RootModel expected TokenContract for (sellerPubkey, nonce) is 0:9aff5b8520caf32dbb91390134a946fc9c2896830d96b86cb0f1fbd2262dbe36 and offered token_contract is 0:9aff5b8520caf32dbb91390134a946fc9c2896830d96b86cb0f1fbd2262dbe36; match=true",
            "TokenContract 0:9aff5b8520caf32dbb91390134a946fc9c2896830d96b86cb0f1fbd2262dbe36 state evidence: Active/getState readable",
        );
        assert!(msg.contains("timed out after 120s"), "{msg}");
        assert!(
            msg.contains("no message_hash/tx_hash is available"),
            "{msg}"
        );
        assert!(msg.contains(tc), "{msg}");
        assert!(msg.contains("nonce=1783558097"), "{msg}");
        assert!(msg.contains("match=true"), "{msg}");
        assert!(msg.contains("Active/getState readable"), "{msg}");
        assert!(!msg.contains("seller offer did not rest"), "{msg}");
    }

    /// the buyer/seller pre-write owner-key gate behind `assert_note_owner_matches`. A note
    /// whose on-chain `_ephemeralPubkey` equals the client's signing pubkey (case- and `0x`-insensitive -- the
    /// getter returns `0x...`, `public_hex()` has no prefix) passes; a rotated/orphaned note (different or absent
    /// `ephemeralPubkey`) is rejected fail-closed with an actionable re-mint message naming both keys and the
    /// pre-accept `ERR_INVALID_SENDER 101` cause -- instead of the opaque pre-accept revert + silent 300s
    /// `read_match` timeout. Pure/offline(no chain, no giver).
    #[test]
    fn note_owner_mismatch_reason_flags_rotated_note() {
        let note =
            Address::parse("0:988322d9cbffc133b491ef09885d3811ce03a54ef5ae8ac94019bddea4d3736e")
                .expect("parse note address");
        let signing = "10b129e8000000000000000000000000000000000000000000000000000006a9";
        // A healthy note: the match is case- and `0x`-insensitive(getter yields `0x...`, possibly upper-case).
        let onchain_match = "0x10B129E8000000000000000000000000000000000000000000000000000006A9";
        assert_eq!(
            note_owner_mismatch_reason("buyer place_buy", &note, Some(onchain_match), signing),
            None
        );
        // A rotated/wrong owner key -> rejected fail-closed, naming both keys + the pre-accept cause + remedy.
        let rotated = "0xdeadbeef00000000000000000000000000000000000000000000000000000000";
        let err = note_owner_mismatch_reason("buyer place_buy", &note, Some(rotated), signing)
            .expect("a rotated note must be flagged");
        assert!(err.contains("Re-mint"), "{err}");
        assert!(err.contains("ERR_INVALID_SENDER 101"), "{err}");
        assert!(err.contains("_ephemeralPubkey"), "{err}");
        assert!(err.contains(signing), "{err}");
        // An absent on-chain `ephemeralPubkey`(uninit/orphaned note) is rejected fail-closed too, by role.
        let none = note_owner_mismatch_reason("seller post_offer", &note, None, signing)
            .expect("an absent ephemeralPubkey must be flagged");
        assert!(none.contains("<none>"), "{none}");
        assert!(none.contains("seller post_offer"), "{none}");
    }

    /// Offline regression for ****: the per-deal TC address is derived from the deploy INIT-DATA
    /// (stateInit), NOT the RootModel `getTokenContractAddress` getter -- so `provision_market`'s idempotency
    /// check works on a fresh provision where the RootModel is still uninit (the getter would 404 and abort
    /// the whole provision). No network, no giver. Two properties, exactly:
    /// **(a)** `token_contract_deploy_address` == `build_deploy(...).address` bit-for-bit (it IS the address the
    /// deploy creates); **(b)** it returns `Ok` against a RootModel address whose account does **not** exist
    /// on-chain -- proving the getter(and any account query) is never called.
    #[tokio::test]
    async fn token_contract_deploy_address_is_init_data_derived_and_getter_free() {
        let manifest = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/deployed.shellnet.json"
        );
        // `connect` is offline: it loads the manifest + builds client config -- no network call.
        let be = RealChainBackend::connect(manifest)
            .expect("offline connect (manifest load, no network)");
        let seller = KeyPair::generate();
        // (b) A RootModel address whose account does NOT exist on-chain -- the derivation must NOT query it.
        let never_deployed_rm =
            Address::parse("0:00000000000000000000000000000000000000000000000000000000deadbeef")
                .expect("rm addr");
        let note =
            Address::parse("0:1111111111111111111111111111111111111111111111111111111111111111")
                .expect("note addr");
        let nonce = 68u64;
        let model = "dexdo-d68-init-data-derivation";
        let (tick, price, max_ticks) = (1000u128, 100u128, 10u128);
        let abi: Value = serde_json::from_str(TOKENCONTRACT_ABI).expect("parse TokenContract ABI");
        let constructor = abi["functions"]
            .as_array()
            .expect("TokenContract functions[]")
            .iter()
            .find(|f| f["name"] == "constructor")
            .expect("TokenContract constructor present");
        let inputs: Vec<(&str, &str)> = constructor["inputs"]
            .as_array()
            .expect("constructor inputs[]")
            .iter()
            .map(|i| {
                (
                    i["name"].as_str().unwrap_or(""),
                    i["type"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            inputs,
            vec![
                ("modelName", "string"),
                ("modelHash", "uint256"),
                ("pricePerTick", "uint128"),
                ("maxTicks", "uint128"),
                ("sellerNote", "address"),
            ],
            "4.0.18 TokenContract constructor is 5-arg; tickSize is a fixed getDeal() constant"
        );

        // (b) getter-free / 404-proof: succeeds against a never-deployed RootModel(no account query).
        let derived = be
            .token_contract_deploy_address(
                &seller,
                &never_deployed_rm,
                nonce,
                model,
                tick,
                price,
                max_ticks,
                &note,
            )
            .await
            .expect("Ok -- INIT-DATA derivation needs no RootModel account, no network, cannot 404");

        // (a) bit-for-bit == build_deploy(...).address -- the exact address the deploy will create.
        let ctx = local_context().expect("local ctx");
        let init_data = json!({
            "_sellerPubkey": format!("0x{}", seller.public_hex()),
            "_rootModelAddress": never_deployed_rm.with_workchain(),
            "_nonce": nonce.to_string(),
        });
        let ctor = json!({
            "modelName": model,
            "modelHash": model_hash_for(model),
            "pricePerTick": price.to_string(),
            "maxTicks": max_ticks.to_string(),
            "sellerNote": note.with_workchain(),
        });
        let msg = build_deploy(
            &ctx,
            TOKENCONTRACT_ABI,
            TOKENCONTRACT_TVC,
            init_data,
            ctor,
            seller.public_hex(),
            seller.secret_hex(),
        )
        .await
        .expect("build_deploy");
        assert_eq!(
            derived.with_workchain(),
            Address::parse(&msg.address).expect("addr").with_workchain(),
            "token_contract_deploy_address is bit-for-bit the deploy stateInit address"
        );
    }

    /// Offline selector-agreement guard. In the 4.0.25 flow the seller posts its deal in ONE call:
    /// `PrivateNote.postSellOffer(flags, nonce)`. The note derives the canonical per-deal `TokenContract`
    /// locally and hands it the baked `InferenceOrderBook` hash via `TokenContract.postFromNote`; the TC
    /// posts the resting ask itself via `InferenceOrderBook.placeSellOffer(...)` (`msg.sender == TC`, so the
    /// book proves canonical-TC ownership without a caller-supplied `tokenContract`). No RootPN round-trip.
    /// This guard pins the `postSellOffer` selector -- name + ordered input types, which is what the TVM
    /// function ID is derived from -- so the Rust client's `post_sell_offer` submit cannot silently drift
    /// from the deployed ABI, and asserts the superseded `confirmDeal` is gone.
    #[test]
    fn post_sell_offer_abi_selector_is_flags_nonce() {
        let abi: Value = serde_json::from_str(PRIVATENOTE_ABI).expect("parse PrivateNote ABI");
        let funcs = abi["functions"].as_array().expect("functions[]");
        let func = funcs
            .iter()
            .find(|f| f["name"] == "postSellOffer")
            .expect("postSellOffer present in the 4.0.25 PrivateNote ABI");
        let inputs: Vec<(&str, &str)> = func["inputs"]
            .as_array()
            .expect("inputs[]")
            .iter()
            .map(|i| {
                (
                    i["name"].as_str().unwrap_or(""),
                    i["type"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            inputs,
            vec![("flags", "uint8"), ("nonce", "uint64")],
            "4.0.25 PrivateNote.postSellOffer takes (flags, nonce); the TC posts the offer itself"
        );
        assert!(
            funcs.iter().all(|f| f["name"] != "confirmDeal"),
            "confirmDeal must be gone from the 4.0.25 PrivateNote ABI (superseded by postSellOffer + TC.postFromNote)"
        );
    }

    #[test]
    fn post_sell_offer_client_emits_only_the_single_note_call() {
        let source = include_str!("client.rs");
        let start = source
            .find("pub async fn post_sell_offer(")
            .expect("post_sell_offer client helper present");
        let body = &source[start
            ..source[start..]
                .find("/// The buyer (note) places a limit buy")
                .map(|offset| start + offset)
                .expect("post_sell_offer helper boundary present")];

        assert_eq!(
            body.matches("self.submit(").count(),
            1,
            "one external submit"
        );
        assert!(
            body.contains("self.submit(\n            note,"),
            "target is the seller note"
        );
        assert!(
            body.contains("\"postSellOffer\""),
            "target method is postSellOffer"
        );
        assert!(
            body.contains("\"flags\": flags"),
            "flags argument is emitted"
        );
        assert!(
            body.contains("\"nonce\": nonce.to_string()"),
            "nonce argument is emitted"
        );
        for obsolete in [
            "\"modelHash\"",
            "\"pricePerTick\"",
            "\"maxTicks\"",
            "\"tokenContract\"",
            "confirmDeal",
            "placeSellOffer",
            "postFromNote",
        ] {
            assert!(
                !body.contains(obsolete),
                "seller client must not emit obsolete submission field or handshake: {obsolete}"
            );
        }
    }

    /// offline selector guard: the live 4.0.14 client path depends on the note-level
    /// `streamCleanup(address)` wrapper and `TokenContract.getState().fundedTime` timer field.
    #[test]
    fn never_opened_cleanup_abi_surface_is_present() {
        let note_abi: Value = serde_json::from_str(PRIVATENOTE_ABI).expect("parse PrivateNote ABI");
        let cleanup = note_abi["functions"]
            .as_array()
            .expect("PrivateNote functions[]")
            .iter()
            .find(|f| f["name"] == "streamCleanup")
            .expect("PrivateNote.streamCleanup present");
        let inputs: Vec<(&str, &str)> = cleanup["inputs"]
            .as_array()
            .expect("streamCleanup inputs[]")
            .iter()
            .map(|i| {
                (
                    i["name"].as_str().unwrap_or(""),
                    i["type"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            inputs,
            vec![("tokenContract", "address")],
            "PrivateNote.streamCleanup selector must stay tokenContract-only"
        );

        let tc_abi: Value =
            serde_json::from_str(TOKENCONTRACT_ABI).expect("parse TokenContract ABI");
        let cleanup = tc_abi["functions"]
            .as_array()
            .expect("TokenContract functions[]")
            .iter()
            .find(|f| f["name"] == "cleanupUnopened")
            .expect("TokenContract.cleanupUnopened present");
        assert!(
            cleanup["inputs"]
                .as_array()
                .expect("cleanupUnopened inputs[]")
                .is_empty(),
            "cleanupUnopened must have no caller-chosen payout argument"
        );
        let state = tc_abi["functions"]
            .as_array()
            .expect("TokenContract functions[]")
            .iter()
            .find(|f| f["name"] == "getState")
            .expect("TokenContract.getState present");
        let outputs: Vec<(&str, &str)> = state["outputs"]
            .as_array()
            .expect("getState outputs[]")
            .iter()
            .map(|o| {
                (
                    o["name"].as_str().unwrap_or(""),
                    o["type"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert!(
            outputs.contains(&("fundedTime", "uint64")),
            "getState must expose fundedTime for the MATCH_OPEN_TIMEOUT preflight"
        );
    }

    /// review regression: a model-only buyer chooses ticks/price after seeing the book. The real backend must
    /// re-run the escrow invariant on that final tuple immediately before the shellnet write.
    #[test]
    fn model_only_buy_revalidates_chosen_escrow_before_submit() {
        let source = include_str!("backends.rs");
        let model_only = source
            .find("async fn place_buy_by_model")
            .expect("model-only buy implementation present");
        let body = &source[model_only..];
        let check = body
            .find("check_buy_deposit_headroom(escrow, ticks, max_price_per_tick)")
            .expect("final chosen escrow is checked");
        let submit = body
            .find(".place_inference_buy(")
            .expect("model-only buy submits placeInferenceBuy");
        assert!(
            check < submit,
            "final chosen escrow/headroom must be validated before placeInferenceBuy"
        );
    }

    #[test]
    fn buyer_withdrawn_preflight_precedes_every_place_inference_buy_write() {
        let source = include_str!("backends.rs");
        let buyer_impl = source
            .find("impl ChainBackend for RealBuyerBackend")
            .expect("real buyer implementation present");
        let buyer = &source[buyer_impl..];

        for (method, submit) in [
            ("async fn place_buy(", ".place_inference_buy("),
            ("async fn place_buy_by_model(", ".place_inference_buy("),
            (
                "async fn place_buy_by_model_with_submit_identity(",
                ".place_inference_buy_with_submit_identity(",
            ),
        ] {
            let start = buyer.find(method).expect("buyer submit method present");
            let body = &buyer[start..];
            let guard = body
                .find(".assert_note_can_place_inference_buy(&self.note)")
                .expect("buyer withdrawn-state preflight present");
            let write = body.find(submit).expect("buyer money write present");
            assert!(
                guard < write,
                "{method} must reject a withdrawn note before {submit}"
            );
        }
    }

    #[test]
    fn model_buy_preflight_selection_once_performs_one_underlying_preflight() {
        let source = include_str!("backends.rs");
        let start = source
            .find("async fn model_buy_preflight_selection_once")
            .expect("model preflight seam present");
        let body = &source[start..];
        let end = body
            .find("async fn assert_expected_buy_target")
            .expect("next backend method present");
        let body = &body[..end];

        assert_eq!(body.matches("self.orderbook_snapshot().await").count(), 1);
        assert_eq!(body.matches(".submit_safe_model_buy_ask(").count(), 1);
        assert!(
            !body.contains("for ") && !body.contains("while "),
            "the one-shot backend seam must not contain an inner retry loop"
        );
    }

    /// review regression: after duplicate-TC coalescing chooses one representative ask, the real buyer must
    /// read that TC's state and fail closed on funded/opened/disputed/residual states before moving escrow.
    #[test]
    fn buyer_checks_selected_tc_state_before_submit() {
        let source = include_str!("backends.rs");
        let buyer_impl = source
            .find("impl ChainBackend for RealBuyerBackend")
            .expect("real buyer impl present");
        let buyer = &source[buyer_impl..];

        let explicit = buyer
            .find("async fn place_buy(")
            .expect("explicit TC buy implementation present");
        let explicit_body = &buyer[explicit..];
        let explicit_guard = explicit_body
            .find("assert_selected_tc_unused")
            .expect("explicit TC buy checks selected TC state");
        let explicit_submit = explicit_body
            .find(".place_inference_buy(")
            .expect("explicit TC buy submits placeInferenceBuy");
        assert!(
            explicit_guard < explicit_submit,
            "explicit TC buy must check selected TC state before placeInferenceBuy"
        );

        let model_only = buyer
            .find("async fn place_buy_by_model")
            .expect("model-only buy implementation present");
        let model_body = &buyer[model_only..];
        let selected = model_body
            .find("model_buy_preflight_selection_once")
            .expect("model-only buy records a submit-safe selected representative ask");
        let model_guard = model_body
            .find("assert_selected_tc_unused")
            .expect("model-only buy checks selected TC state");
        let model_submit = model_body
            .find(".place_inference_buy(")
            .expect("model-only buy submits placeInferenceBuy");
        assert!(
            selected < model_guard && model_guard < model_submit,
            "model-only buy must check the selected representative TC state before placeInferenceBuy"
        );
    }

    /// review regression: seller offers must be bound to the deployed TC's `getDeal` terms, not interactive
    /// or stale CLI defaults, so advertised IOB terms cannot diverge from settlement config.
    #[test]
    fn real_seller_post_offer_uses_onchain_deal_terms() {
        let source = include_str!("backends.rs");
        let seller_impl = source
            .find("impl ChainBackend for RealSellerBackend")
            .expect("real seller impl present");
        let seller = &source[seller_impl..];
        let post_offer = seller
            .find("async fn post_offer(&self, offer: SellOffer")
            .expect("real seller post_offer present");
        let body = &seller[post_offer..];
        let withdrawn = body
            .find("assert_note_can_post_sell_offer")
            .expect("post_offer checks PrivateNote hasWithdrawn");
        let terms = body
            .find("sell_offer_terms(&offer.token_contract)")
            .expect("post_offer reads on-chain deal terms");
        let submit = body
            .find(".post_sell_offer(")
            .expect("post_offer submits to shellnet");
        assert!(
            withdrawn < submit,
            "PrivateNote.hasWithdrawn must be checked before postSellOffer"
        );
        assert!(
            terms < submit,
            "TokenContract.getDeal terms must be read before postSellOffer"
        );
        assert!(
            body.contains("seller offer terms are bound to TokenContract.getDeal"),
            "drifted CLI terms must be visibly ignored"
        );
    }

    /// live regression: the gateway-owned seller watcher calls `read_handover` while provisioning
    /// or restoring a match. Real seller backends must read the TC handover instead of failing as the buyer role.
    #[test]
    fn real_seller_backend_allows_handover_read_for_watcher_resume() {
        let source = include_str!("backends.rs");
        let seller_impl = source
            .find("impl ChainBackend for RealSellerBackend")
            .expect("real seller impl present");
        let seller = &source[seller_impl..];
        let read_handover = seller
            .find("async fn read_handover(")
            .expect("real seller read_handover present");
        let advance_tick = seller[read_handover..]
            .find("async fn advance_tick(")
            .map(|offset| read_handover + offset)
            .expect("next real seller method present");
        let body = &seller[read_handover..advance_tick];

        assert!(
            body.contains("self.chain.read_handover(&tc).await.map_err(map_err)"),
            "real seller watcher must read existing TC handover for idempotent provisioning"
        );
        assert!(
            !body.contains("wrong_role(\"read_handover\""),
            "real seller read_handover must not fail as a buyer-only action"
        );
    }

    #[test]
    fn real_seller_backend_exposes_deal_state_for_policy_watcher() {
        let source = include_str!("backends.rs");
        let seller_impl = source
            .find("impl ChainBackend for RealSellerBackend")
            .expect("real seller impl present");
        let seller = &source[seller_impl..];
        let deal_state = seller
            .find("async fn deal_state(")
            .expect("real seller deal_state present");
        let snapshot = seller[deal_state..]
            .find("async fn snapshot(")
            .map(|offset| deal_state + offset)
            .expect("next real seller method present");
        let body = &seller[deal_state..snapshot];

        assert!(
            body.contains("token_contract_state(&tc)")
                && body.contains("deal_chain_state_from_json"),
            "real seller policy watcher must read TokenContract lifecycle flags"
        );
    }
}

// the live shellnet tests drive the giver(test faucet), which is gated behind `test-giver`.
// Run with `--features shellnet,test-giver -- --ignored`. Without `test-giver` they are compiled out,
// so a default/`shellnet` build(and its test compile) contains no giver.
