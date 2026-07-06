use super::client::RealChainBackend;
#[cfg(test)]
use super::client::{active_check, code_hash_check, is_uninit_account_404, ShellnetDoctorStatus};
use super::contracts_provision::*;
use crate::chain::{
    check_buy_deposit_headroom, coalesce_equivalent_resting_asks, ChainBackend, ChainError,
    DealChainState, DealRole, DealView, Match, MatchWatchCursor, OrderBookOrder, OrderBookSnapshot,
    OrderBookStats, SellOffer, StreamSnapshot, TokenContract, MATCH_OPEN_TIMEOUT_SECS,
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
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

    /// The ed25519 `KeyPair` -- `RealChainBackend` signs chain calls with it(`ChainClient::call`).
    pub fn keypair(&self) -> &KeyPair {
        &self.keypair
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
    /// `modelHash`(uint256 hex) -- the note derives the book address from it; `postSellOffer`/
    /// `placeInferenceBuy`/`deployInferenceOrderBook`/getter accept only `modelHash`.
    pub model_hash: String,
    /// The seller's deal nonce: the `_nonce` static the per-deal `TokenContract` is deployed with (it
    /// parameterises the TC address derivation) AND forwarded into `postSellOffer(...nonce)` so the 4.0.6
    /// IOB's canonical-TC check passes. The deployed 4.0.6 note(`c8a81f54`) takes the nonce.
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

    /// Low-level backend(provisioning/getters).
    pub fn chain(&self) -> &RealChainBackend {
        &self.chain
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

fn map_err(e: anyhow::Error) -> ChainError {
    ChainError::Chain(e.to_string())
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

/// does this resting `InferenceOrderBook` order belong to OUR deal? Matches by canonical `tokenContract`
/// (case-insensitive) -- `getBestBidAsk.hasAsk` alone is a false-green (a shared/non-empty model book carries
/// unrelated asks). Pure + offline-testable; an order without a `tokenContract`(cancelled/buy) never matches.
fn ask_matches_deal(order: &Value, want_tc_lower: &str) -> bool {
    order["tokenContract"]
        .as_str()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
        == Some(want_tc_lower)
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

fn order_u64(order: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| {
            order[*k]
                .as_str()
                .and_then(|s| parse_order_u128(s).and_then(|v| v.try_into().ok()))
                .or_else(|| order[*k].as_u64())
        })
        .unwrap_or(0)
}

fn zero_address_like(addr: &str) -> bool {
    addr.trim_start_matches(['0', ':', 'x']).is_empty()
}

fn orderbook_order_from_getter(order_id: u128, order: &Value) -> Option<OrderBookOrder> {
    let is_buy = order["isBuy"].as_bool().unwrap_or(false);
    let owner_note = order["note"]
        .as_str()
        .or_else(|| order["owner"].as_str())
        .unwrap_or("")
        .to_string();
    if owner_note.is_empty() || zero_address_like(&owner_note) {
        return None;
    }
    let token_contract = order["tokenContract"].as_str().and_then(|s| {
        if zero_address_like(s) {
            None
        } else {
            Some(s.to_string())
        }
    });
    let ticks = order_u128(order, &["amount", "maxTicks"])?;
    if ticks == 0 {
        return None;
    }
    Some(OrderBookOrder {
        order_id,
        owner_note,
        token_contract,
        is_buy,
        price_per_tick: order_u128(order, &["price", "pricePerTick"])?,
        ticks,
        escrow: order_u128(order, &["escrow"]).unwrap_or(0),
        deadline: order_u64(order, &["deadline"]),
        flags: order_u64(order, &["flags"]).min(u8::MAX as u64) as u8,
        timestamp: order_u64(order, &["ts", "timestamp"]),
    })
}

#[cfg(test)]
fn resting_ask_from_order(order_id: u128, order: &Value) -> Option<OrderBookOrder> {
    orderbook_order_from_getter(order_id, order).filter(|o| o.is_resting_ask())
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

fn check_full_fill_only(ask: &OrderBookOrder, ticks: u128) -> Result<(), String> {
    if ask.ticks == ticks {
        return Ok(());
    }
    Err(format!(
        "refusing partial/multi-ask fill: order #{} tokenContract {} has {} ticks, buyer requested {ticks}. \
         Current shellnet must be bought as a whole ask; live  reproduced ticks < ask ticks removing the \
         ask without funding the TokenContract.",
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
        return Err(format!(
            "no matchable ask for max_price_per_tick {max_price_per_tick}, requested ticks {ticks}"
        ));
    };
    check_full_fill_only(best, ticks)?;
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
    let raw_selected = selected_model_buy_ask(&raw_asks, max_price_per_tick, ticks).ok();
    selected_model_buy_ask(executable_asks, max_price_per_tick, ticks).map_err(|e| {
        let raw = raw_selected
            .as_ref()
            .map(describe_buy_ask)
            .unwrap_or_else(|| "no raw matching ask".to_string());
        format!(
            "no executable matching ask after skipping unreadable or already-used TokenContracts: {e}. \
             raw order-book head candidate: {raw}. Retry after the seller posts a fresh ask, or clean/cancel \
             the stale order-book rows if you operate this market"
        )
    })
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
        check_full_fill_only(best, ticks)?;
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

fn active_sell_order_ids_for_tc(orders: &[OrderBookOrder], want_tc_lower: &str) -> Vec<u128> {
    orders
        .iter()
        .filter(|order| {
            order.is_resting_ask()
                && order
                    .token_contract
                    .as_deref()
                    .is_some_and(|tc| tc.eq_ignore_ascii_case(want_tc_lower))
        })
        .map(|order| order.order_id)
        .collect()
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
        ask_matches_deal, check_expected_buy_target, check_model_buy_full_fill,
        executable_resting_asks_by_state, next_matching_ask, orderbook_order_from_getter,
        resting_ask_from_order, selected_model_buy_ask,
        selected_model_buy_ask_matching_executable_depth,
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

    /// the rest-guard must match THIS deal's `tokenContract` -- `getBestBidAsk.hasAsk` (any ask
    /// in a shared book) is a false-green. Unrelated ask => reject; our ask(case-insensitive TC) => accept.
    #[test]
    fn rest_guard_matches_only_our_token_contract() {
        let want = "0:dead".to_ascii_lowercase();
        // an unrelated seller's ask resting in the same model book must NOT satisfy our guard
        assert!(!ask_matches_deal(
            &json!({ "tokenContract": "0:beef" }),
            &want
        ));
        // our ask(canonical TC, matched case-insensitively) does
        assert!(ask_matches_deal(
            &json!({ "tokenContract": "0:DEAD" }),
            &want
        ));
        // a cancelled/buy order(no tokenContract) never matches
        assert!(!ask_matches_deal(&json!({ "pricePerTick": "1" }), &want));
    }

    #[test]
    fn order_parser_accepts_current_get_order_abi_names() {
        let ask = resting_ask_from_order(
            7,
            &json!({
                "note": "0:seller",
                "tokenContract": "0:tc",
                "price": "1000",
                "amount": "1024",
                "isBuy": false
            }),
        )
        .expect("current getOrder ABI fields should parse");
        assert_eq!(ask.order_id, 7);
        assert_eq!(ask.owner_note, "0:seller");
        assert_eq!(ask.price_per_tick, 1000);
        assert_eq!(ask.ticks, 1024);
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
                "isBuy": false
            }),
        )
        .expect("live getter hex numeric fields should parse");
        assert_eq!(ask.price_per_tick, 10_000);
        assert_eq!(ask.ticks, 1024);
    }

    #[test]
    fn order_parser_still_accepts_legacy_offer_field_names() {
        let ask = resting_ask_from_order(
            9,
            &json!({
                "owner": "0:seller",
                "tokenContract": "0:tc",
                "pricePerTick": "55",
                "maxTicks": "8"
            }),
        )
        .expect("legacy field names should parse");
        assert_eq!(ask.owner_note, "0:seller");
        assert_eq!(ask.price_per_tick, 55);
        assert_eq!(ask.ticks, 8);
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
                "isBuy": true
            }),
        )
        .expect("resting buy order should parse");
        assert_eq!(order.owner_note, "0:buyer");
        assert!(order.token_contract.is_none());
        assert!(order.is_buy);
        assert!(!order.is_resting_ask());
        assert_eq!(order.escrow, 3075);
    }

    #[test]
    fn order_parser_skips_buy_cancelled_and_zero_tc_orders() {
        assert!(resting_ask_from_order(
            1,
            &json!({ "tokenContract": "0:tc", "price": "1", "amount": "1", "isBuy": true })
        )
        .is_none());
        assert!(resting_ask_from_order(2, &json!({ "price": "1", "amount": "1" })).is_none());
        assert!(resting_ask_from_order(
            3,
            &json!({ "tokenContract": "0:000000", "price": "1", "amount": "1" })
        )
        .is_none());
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
    fn buyer_target_preflight_rejects_expected_partial_fill() {
        let asks = vec![parsed_ask(1, "0:expected", 1000, 10)];
        let err = check_expected_buy_target(&asks, "0:expected", 1000, 2).unwrap_err();
        assert!(err.contains("refusing partial/multi-ask fill"), "{err}");
        assert!(err.contains("has 10 ticks"), "{err}");
        assert!(err.contains("buyer requested 2"), "{err}");
    }

    #[test]
    fn model_only_preflight_rejects_partial_fill_before_submit() {
        let asks = vec![parsed_ask(1, "0:best", 1000, 2)];
        let err = check_model_buy_full_fill(&asks, 1000, 1).unwrap_err();
        assert!(err.contains("refusing partial/multi-ask fill"), "{err}");
        assert!(err.contains("tokenContract 0:best"), "{err}");
    }

    #[test]
    fn model_only_preflight_accepts_whole_best_ask() {
        let asks = vec![parsed_ask(1, "0:best", 1000, 1)];
        assert!(check_model_buy_full_fill(&asks, 1000, 1).is_ok());
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
    fn model_only_buy_preflight_accepts_live_ask_after_stale_head() {
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

        let selected =
            selected_model_buy_ask_matching_executable_depth(&asks, &executable, 100, 1024)
                .expect("later executable ask remains buyable despite stale raw rows");
        assert_eq!(selected.order_id, 35);
        assert_eq!(selected.token_contract.as_deref(), Some(live));
    }

    #[test]
    fn quote_selection_skips_unreadable_raw_row_and_fills_later_live_ask() {
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

        let selected =
            selected_model_buy_ask_matching_executable_depth(&raw_asks, &executable, 100, 1024)
                .expect(
                    "quote selection follows executable depth after skipping unreadable raw rows",
                );
        assert_eq!(selected.order_id, 11);
        assert_eq!(selected.token_contract.as_deref(), Some(live));
    }

    #[test]
    fn model_only_buy_preflight_accepts_skip_only_later_quote_selection() {
        let closed = "0:5701d680491b6ff787c18db8e3a2ecde799e039c595bee495d14c1a78cb4de57";
        let live = "0:7969c6c6012dce3575c0547857ce83bf8001e3deedd7ea0425af3b13d5b24704";
        let asks = vec![
            parsed_ask(14, closed, 100, 1024),
            parsed_ask(15, closed, 100, 1024),
            parsed_ask(35, live, 100, 1024),
        ];
        let skip_only_executable = vec![parsed_ask(35, live, 100, 1024)];

        let selected = selected_model_buy_ask_matching_executable_depth(
            &asks,
            &skip_only_executable,
            100,
            1024,
        )
        .expect("model-only preflight follows executable quote selection");
        assert_eq!(selected.order_id, 35);
        assert_eq!(selected.token_contract.as_deref(), Some(live));
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
        assert!(err.contains("refusing partial/multi-ask fill"), "{err}");
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
fn token_contract_resume_blocker(state: &Value, handover_present: bool) -> Option<String> {
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
        if handover_present {
            blockers.push("endpoint handover already written before opened=true".to_string());
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
        let mut orders = Vec::new();
        for id in 1..stats.next_order_id {
            let Some(order) = self.inference_orderbook_order(order_book, id).await? else {
                continue;
            };
            if let Some(parsed) = orderbook_order_from_getter(id, &order) {
                orders.push(parsed);
            }
        }
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
                executable.push(ask);
            }
        }
        Ok(executable)
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

    async fn post_offer(&self, offer: SellOffer, _note: &dyn Note) -> Result<(), ChainError> {
        let tc = parse_tc(&offer.token_contract)?;
        self.chain
            .post_sell_offer(
                &self.ctx.seller_note,
                &self.ctx.seller_keys,
                &self.ctx.model_hash,
                offer.price_per_tick as u128,
                offer.max_ticks as u128,
                &tc,
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
                let handover_present = self
                    .chain
                    .read_handover(&tc)
                    .await
                    .map_err(map_err)?
                    .is_some();
                if let Some(reason) = token_contract_resume_blocker(state, handover_present) {
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
    /// Deal nonce for the per-deal `TokenContract`: the `_nonce` static the TC is deployed
    /// with AND forwarded into `postSellOffer(...nonce)`, so the 4.0.6 IOB's canonical-TC check
    /// (`tokenContract == _tokenContractAddr(sellerPubkey, nonce)`) passes. (The no-nonce selector PR
    /// observed was the superseded 4.0.5 deployment; the live roots are now updateCode'd to 4.0.6.)
    nonce: u64,
    tick_size: u128,
    probe_shell: u128,
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

    /// Low-level backend(getters/address derivation).
    pub fn chain(&self) -> &RealChainBackend {
        &self.chain
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
        let handover_present = self
            .chain
            .read_handover(&tc)
            .await
            .map_err(map_err)?
            .is_some();
        if let Some(reason) = token_contract_resume_blocker(&state, handover_present) {
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
}

#[async_trait]
impl ChainBackend for RealSellerBackend {
    /// the seller daemon publishes offers without `provision_market`'s note-current gate; enforce it here
    /// so a note orphaned by a contract redeploy(stale code_hash) fails closed with an actionable "re-mint"
    /// message instead of a raw `TVM_ERROR` from `postSellOffer`.
    async fn assert_note_current(&self) -> Result<(), ChainError> {
        self.chain
            .assert_seller_note_current(&self.note)
            .await
            .map_err(map_err)
    }
    /// the per-deal TC(sellerPubkey + nonce) is single-use; before resting an ask, fail closed if it is
    /// already USED(a prior deal opened/funded/disputed it or left residual), so the operator gets an
    /// actionable message instead of a raw `TVM_ERROR`(`ERR_ALREADY_OPEN` 321) from the pre-stream steps. A
    /// not-yet-active(undeployed) TC is not "used" -- let the deploy path handle it.
    async fn assert_token_contract_fresh(&self, tc: &TokenContract) -> Result<(), ChainError> {
        let addr = parse_tc(tc)?;
        let Some(state) = self
            .chain
            .token_contract_state(&addr)
            .await
            .map_err(map_err)?
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
        // (symmetric branch-3 guard): fail closed if this note's on-chain owner key
        // (`getDetails().ephemeralPubkey`) is not the key we sign `postSellOffer` with -- otherwise
        // `onlyOwnerPubkey` reverts pre-accept(ERR_INVALID_SENDER 101) and the ask never rests (only an
        // opaque TVM_ERROR). Run it before the IOB deploy / offer write.
        self.chain
            .assert_note_owner_matches("seller post_offer", &self.note, &self.keys)
            .await
            .map_err(map_err)?;
        // An operate exception: if the per-model `InferenceOrderBook` is not yet deployed --
        // deploy it(model listing; the address is derived from `model_hash`). This is operate, NOT actor provisioning.
        let ob = self
            .chain
            .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
            .await
            .map_err(map_err)?;
        if self
            .chain
            .inference_orderbook_stats(&ob)
            .await
            .map_err(map_err)?
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
        self.assert_no_active_sell_order(&offer.token_contract)
            .await?;
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
        self.chain
            .post_sell_offer(
                &self.note,
                &self.keys,
                &self.model_hash,
                price_per_tick as u128,
                max_ticks as u128,
                &tc,
                0,
                self.nonce,
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn assert_no_active_sell_order(&self, tc: &TokenContract) -> Result<(), ChainError> {
        let ob = self
            .chain
            .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
            .await
            .map_err(map_err)?;
        let snapshot = self
            .chain
            .inference_orderbook_snapshot(&ob, &self.model_name, &self.model_hash)
            .await
            .map_err(map_err)?;
        let want = parse_tc(tc)?.with_workchain().to_ascii_lowercase();
        let order_ids = active_sell_order_ids_for_tc(&snapshot.orders, &want);
        if order_ids.is_empty() {
            return Ok(());
        }
        Err(ChainError::Chain(format!(
            "duplicate active sell order rejected for TokenContract {want}: InferenceOrderBook {} already has \
             active sell order id(s) {}. Cancel/fill/cleanup the old order before reposting this TC.",
            snapshot.order_book,
            order_ids
                .iter()
                .map(u128::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )))
    }

    /// confirm THIS deal's ask actually rested in the per-model `InferenceOrderBook` after `post_offer`.
    /// `getBestBidAsk.hasAsk` alone is a false-green -- a shared/non-empty model book carries unrelated asks -- so
    /// this scans the book's orders (`getStats.nextOrderId` + `getOrder(id)`, like the buyer's `discover_offers`)
    /// for ~16s and requires an order whose `tokenContract` == ours. If our ask never rests, fails closed with the
    /// IOB stats so the operator can route(canonical-TC / note-pairing mismatch) instead of a silent 300s
    /// `read_match` timeout.
    async fn assert_offer_rested(&self, tc: &TokenContract) -> Result<(), ChainError> {
        let ob = self
            .chain
            .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
            .await
            .map_err(map_err)?;
        let want = parse_tc(tc)?.with_workchain().to_ascii_lowercase();
        for _ in 0..8 {
            if let Some(stats) = self
                .chain
                .inference_orderbook_stats(&ob)
                .await
                .map_err(map_err)?
            {
                let next_id = stats["nextOrderId"]
                    .as_str()
                    .and_then(|x| x.parse::<u128>().ok())
                    .unwrap_or(0);
                for id in 1..next_id {
                    if let Some(o) = self
                        .chain
                        .inference_orderbook_order(&ob, id)
                        .await
                        .map_err(map_err)?
                    {
                        if ask_matches_deal(&o, &want) {
                            eprintln!(
                                "offer_rested evidence: InferenceOrderBook {ob} order_id={id} token_contract={want}"
                            );
                            return Ok(());
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let stats = self
            .chain
            .inference_orderbook_stats(&ob)
            .await
            .map_err(map_err)?
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unreadable>".to_string());
        Err(ChainError::Chain(format!(
            "seller offer did not rest in the InferenceOrderBook {ob} after posting (no resting order with our \
             tokenContract {want} after ~16s) -- model_hash {}, nonce {}, seller_note {}, IOB stats {stats}. \
             `postSellOffer` submitted but the book did not accept THIS deal's ask: most likely the offer's \
             tokenContract is not the canonical `(sellerPubkey, nonce)` TC for this note (the IOB rejects a \
             mismatched TC), or the market/note pairing differs from what was provisioned. Re-provision a market \
             for THIS note + nonce and offer that, instead of waiting out the match ().",
            self.model_hash, self.nonce, self.note,
        )))
    }

    async fn sell_offer_terms(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(Shell, u64)>, ChainError> {
        let tc = parse_tc(token_contract)?;
        let Some((tick_size, price_per_tick, max_ticks)) = self
            .chain
            .token_contract_deal_terms(&tc)
            .await
            .map_err(map_err)?
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
        self.read_openable_match_once(token_contract).await
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
            .any(|tc| tc.with_workchain().eq_ignore_ascii_case(&want))
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
}

impl RealBuyerBackend {
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

    /// Low-level backend(getters/address derivation).
    pub fn chain(&self) -> &RealChainBackend {
        &self.chain
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

    async fn model_buy_preflight_selection(
        &self,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<(String, OrderBookOrder), ChainError> {
        let snapshot = self.orderbook_snapshot().await?;
        let raw_asks: Vec<OrderBookOrder> = snapshot.resting_asks().cloned().collect();
        let executable_asks = self
            .chain
            .executable_resting_asks(&snapshot)
            .await
            .map_err(|e| {
                ChainError::Chain(format!(
                    "buyer model-only executable-depth preflight failed for InferenceOrderBook {}: {e}. IOB stats {}",
                    snapshot.order_book,
                    orderbook_stats_for_error(&snapshot)
                ))
            })?;
        let selected = selected_model_buy_ask_matching_executable_depth(
            &raw_asks,
            &executable_asks,
            max_price_per_tick,
            ticks,
        )
        .map_err(|e| {
            ChainError::Chain(format!(
                "buyer model-only preflight failed for InferenceOrderBook {}: {e}. IOB stats {}",
                snapshot.order_book,
                orderbook_stats_for_error(&snapshot)
            ))
        })?;
        Ok((snapshot.order_book, selected))
    }

    async fn assert_expected_buy_target(&self, tc: &Address) -> Result<String, ChainError> {
        let snapshot = self.orderbook_snapshot().await?;
        let asks: Vec<OrderBookOrder> = snapshot.resting_asks().cloned().collect();
        let want = tc.with_workchain().to_ascii_lowercase();
        check_expected_buy_target(&asks, &want, self.max_price_per_tick, self.ticks).map_err(
            |e| {
                ChainError::Chain(format!(
                    "buyer target preflight failed for InferenceOrderBook {}: {e}. IOB stats {}",
                    snapshot.order_book,
                    snapshot
                        .stats
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_else(|| "<book not active>".to_string())
                ))
            },
        )?;
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
        // `placeInferenceBuy` is model-book-wide and cannot name a target TC. Fail before moving escrow
        // unless the book's price->time matcher would fund the TC from this market manifest.
        let order_book = self.assert_expected_buy_target(&expected).await?;
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
        self.model_buy_preflight_selection(ticks, max_price_per_tick)
            .await
            .map(|_| ())
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
        let (order_book, selected) = self
            .model_buy_preflight_selection(ticks, max_price_per_tick)
            .await?;
        let selected_tc = selected.token_contract.as_deref().ok_or_else(|| {
            ChainError::Chain(format!(
                "buyer model-only preflight failed for InferenceOrderBook {}: selected order #{} has no TokenContract",
                order_book, selected.order_id
            ))
        })?;
        self.assert_selected_tc_unused(selected_tc, &order_book)
            .await?;
        // The order the buyer chose after seeing the book -- NOT the backend's construction-time defaults.
        self.chain
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
            .map_err(map_err)?;
        Ok(())
    }

    /// Learn the matched per-deal `TokenContract` from THIS note's owner-facing `InferenceFilledConfirmed`
    /// ext-out: derive the per-model book from `model_hash`, then read the note's own
    /// fill event for this book's BUY side. No shared-book index.
    async fn wait_matched_token_contract(
        &self,
        since_unix: i64,
        timeout: std::time::Duration,
    ) -> Result<TokenContract, ChainError> {
        let ob = self
            .chain
            .inference_orderbook_address(&self.note, &self.model_hash, self.tick_size)
            .await
            .map_err(map_err)?;
        let tc = self
            .chain
            .wait_inference_filled_tc(&self.note, &ob, since_unix, timeout)
            .await
            .map_err(map_err)?;
        Ok(tc.with_workchain())
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
        assert_eq!(token_contract_resume_blocker(&pre_open, false), None);

        let handover =
            token_contract_resume_blocker(&pre_open, true).expect("handover blocks resume");
        assert!(
            handover.contains("endpoint handover already written before opened=true"),
            "{handover}"
        );

        let opened = json!({"funded": true, "opened": true, "probeAccepted": false, "disputed": false,
            "deposit": "0", "prepaid": "0", "frozen": "1000", "finalizedOwed": "0"});
        assert_eq!(token_contract_resume_blocker(&opened, true), None);

        let stopped = json!({"funded": true, "opened": false, "probeAccepted": true, "disputed": false,
            "deposit": "0", "prepaid": "0", "frozen": "0", "finalizedOwed": "2000"});
        let r =
            token_contract_resume_blocker(&stopped, false).expect("stopped state blocks resume");
        assert!(r.contains("probeAccepted without opened"), "{r}");

        let disputed = json!({"funded": true, "opened": true, "probeAccepted": false, "disputed": true,
            "deposit": "0", "prepaid": "0", "frozen": "1000", "finalizedOwed": "0"});
        let r =
            token_contract_resume_blocker(&disputed, true).expect("disputed state blocks resume");
        assert!(r.contains("disputed"), "{r}");
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

    /// Offline selector-agreement guard. The deployed **4.0.7** `postSellOffer` is
    /// `postSellOffer(modelHash, pricePerTick, maxTicks, tokenContract, flags, nonce)` -- the trailing
    /// `nonce` is REQUIRED: the IOB re-derives the per-deal `TokenContract` as
    /// `_tokenContractAddr(sellerPubkey, nonce)` and rejects the offer unless the supplied `tokenContract`
    /// matches(canonical-TC check). The vendored `PrivateNote` (`26425eed`, 4.0.15: owner-facing
    /// onInferenceFilled/onInferencePlaced mirrors pushing the deal TC into the note; carries
    /// streamCleanup +-F2/P1) is
    /// live (RootPN `9ab11582` carries it via setPrivateNoteCode -- the PN code is too big for the
    /// updateCode cell now, so it ships in its own message). This guard pins the exact signature -- name + ordered input types, which is what
    /// the TVM function ID is derived from -- so the Rust client's `nonce`-forwarding(above) cannot silently
    /// drift from the deployed selector.
    #[test]
    fn post_sell_offer_abi_selector_requires_nonce() {
        let abi: Value = serde_json::from_str(PRIVATENOTE_ABI).expect("parse PrivateNote ABI");
        let func = abi["functions"]
            .as_array()
            .expect("functions[]")
            .iter()
            .find(|f| f["name"] == "postSellOffer")
            .expect("postSellOffer present in the 4.0.6 ABI");
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
            vec![
                ("modelHash", "uint256"),
                ("pricePerTick", "uint128"),
                ("maxTicks", "uint128"),
                ("tokenContract", "address"),
                ("flags", "uint8"),
                ("nonce", "uint64"),
            ],
            "deployed 4.0.6 postSellOffer selector (c8a81f54) REQUIRES the trailing `nonce` ( canonical-TC check)"
        );
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
            .find("model_buy_preflight_selection")
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
        let terms = body
            .find("sell_offer_terms(&offer.token_contract)")
            .expect("post_offer reads on-chain deal terms");
        let submit = body
            .find(".post_sell_offer(")
            .expect("post_offer submits to shellnet");
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
