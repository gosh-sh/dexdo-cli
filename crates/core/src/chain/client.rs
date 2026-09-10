use super::backends::{is_absent_address, note_owner_mismatch_reason};
use super::book_events::{
    read_book_event_fold, read_book_fill_candidates, BookEventFold, BookFillCandidate,
};
use super::contracts_provision::*;
use crate::manifest::{model_hash_for, MarketManifest};
use crate::market::{
    check_seller_pubkey, check_subscription_buy_reserve, flags, BuyerOrderFact,
    BuyerStopTerminalFact, BuyerStopTerminalReceipt, DealBuyerBond, DealChainSnapshot,
    DealChainState, DealOfferLatch, DealRole, DealSellerBond, DealSubscription,
    InferenceSubscriptionPlacement, MatchWatchCursor, MatchedFill, SettlementAction,
    SettlementActionBondState, SettlementActionEvent, SettlementActionPostState,
    SettlementActionReceipt,
};
use crate::onchain_diagnostics::{validate_onchain_submit_response, OnchainSubmitError};
use crate::oracle_manifest::OracleMarketManifest;
use crate::params::TICK_SIZE;
use crate::params::{
    SellerLivenessParams, DEAL_SNAPSHOT_MAX_ATTEMPTS, PRICE_STEP, SUBSCRIPTION_MAX_TICKS,
    SUBSCRIPTION_WEEKS,
};
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use gosh_ackinacki::airegistry::calls::encode_external_call;
// `build_deploy` is no longer called in THIS file -- contracts 4.0.36 moved the deal's deploy into
// the note, and the address derivation that remains encodes a stateInit rather than a message.
// It stays imported because the `operator_wallet` submodule below globs this scope and deploys
// the operator multisig through it, which is an ordinary external deploy and unaffected.
use super::note_withdraw_gate_boc::note_withdraw_gate_from_account_boc;
use crate::note_withdraw_gate::{
    refusal_carries_a_withdraw_gate_code, withdraw_gate_line, NoteWithdrawGate,
};
use gosh_ackinacki::airegistry::deploy::{build_deploy, local_context};
use gosh_ackinacki::config::AiRegistryConfig;
use gosh_ackinacki::sdk::{Account, Address, ChainClient, ChainLiveness, KeyPair};
use gosh_ackinacki::wallet::query::{dest_account_id_hex, fetch_dapp_id};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tvm_block::Deserializable;

#[cfg(test)]
#[path = "client_issue_1120_tests.rs"]
mod client_issue_1120_tests;

#[cfg(test)]
#[path = "client_https_refusal_tests.rs"]
mod client_https_refusal_tests;

#[cfg(test)]
#[path = "client_issue_1348_tests.rs"]
mod client_issue_1348_tests;

#[cfg(test)]
#[path = "client_issue_1528_tests.rs"]
mod client_issue_1528_tests;

#[cfg(test)]
#[path = "client_issue_1597_tests.rs"]
mod client_issue_1597_tests;

#[cfg(test)]
#[path = "client_issue_1834_tests.rs"]
mod client_issue_1834_tests;

#[cfg(test)]
#[path = "client_issue_1599_tests.rs"]
mod client_issue_1599_tests;

#[cfg(test)]
#[path = "client_issue_1861_tests.rs"]
mod client_issue_1861_tests;

#[cfg(test)]
#[path = "client_issue_1998_tests.rs"]
mod client_issue_1998_tests;

// the fall-through step of `resolve_endpoint`. Declared beside the pair because it
// pins the other half of the same rule -- refuses a label the endpoint contradicts, this one
// refuses to invent an endpoint the label does not imply.

const MIN_PMP_INITIAL_STAKE: u128 = 10_000_000;
/// Pinned `tvm_client` default signed-message lifetime (`message_expiration_timeout`).
const SDK_MESSAGE_EXPIRY_SECS: u64 = 40;
/// Strict contract window: `block.timestamp < expireAt < block.timestamp + 300`.
const CONTRACT_MESSAGE_WINDOW_SECS: u64 = 300;
const MAX_CLOCK_BEHIND_SECS: u64 =
    SDK_MESSAGE_EXPIRY_SECS - crate::params::CHAIN_CLOCK_SKEW_SAFETY_MARGIN_SECS;
const MAX_CLOCK_AHEAD_SECS: u64 = CONTRACT_MESSAGE_WINDOW_SECS
    - SDK_MESSAGE_EXPIRY_SECS
    - crate::params::CHAIN_CLOCK_SKEW_SAFETY_MARGIN_SECS;

fn display_dexdo_address(address: impl ToString) -> String {
    crate::address::display(&address.to_string())
}

fn display_token_contract(address: impl ToString) -> String {
    crate::address::display_self_dapp(&address.to_string())
}

/// `PrivateNote.fundDeal` -- the seller-side funding door of contracts 4.0.33. It replaced
/// `postSellerBond(nonce, amount)`, which the deployed `PrivateNote` no longer declares at all.
pub(super) const NOTE_FUND_DEAL_METHOD: &str = "fundDeal";

/// `SuperRoot.deployRootModel` -- the door a seller's `RootModel` comes into existence through in
/// contracts 4.0.34. It replaced `registerRoot(uint256)`, which verified a *self-deployed* root's
/// address; SuperRoot performs the deploy now, so there is no claim left to verify and the entry is
/// gone from the ABI.
pub(super) const SUPERROOT_DEPLOY_ROOT_MODEL_METHOD: &str = "deployRootModel";

/// `SuperRoot.registerRoot` -- the superseded entry. Named so the offline shape pin can assert it is
/// absent (4.0.34) or, in the window before the artifacts are vendored, that the client is not
/// encoding it.
#[cfg(test)]
pub(super) const SUPERROOT_REGISTER_ROOT_METHOD: &str = "registerRoot";

/// The exact argument object [`RealChainBackend::request_root_model_deploy`] sends. Pure so the
/// encoded shape is pinned offline against the vendored `SuperRoot.abi.json` itself.
pub(super) fn super_root_deploy_root_model_params(owner_pubkey: &Value) -> Value {
    json!({ "ownerPubkey": owner_pubkey })
}

/// `TokenContract.fundDeal` -- the receiving half. 4.0.33 renamed `fundSellerBond()` to
/// `fundDeal(uint128 amount)` and turned the bond from attached currency into a figure argument.
/// Only the legacy operator-wallet giver addresses the deal directly; the production seller path
/// reaches it through `PrivateNote.fundDeal`.
pub(super) const DEAL_FUND_DEAL_METHOD: &str = "fundDeal";

/// The exact argument object [`RealChainBackend::note_fund_deal`] sends. Kept pure so the encoded
/// shape can be pinned offline against the vendored `PrivateNote.abi.json` itself rather than
/// against a hand-written copy of it.

/// **FOUR ARGUMENTS, NOT THREE.** Contracts 4.0.35 added `endpointCipher optional(bytes)` to both
/// halves of the funding door, so a seller may publish the endpoint together with the bond in one
/// message from the note. An `optional` argument is a different `functionId`, not an extra field on
/// the old one: the three-argument shape does not encode at all against 4.0.35, so the omission is a
/// refusal before send rather than a silently truncated call.

/// This client sends `null` -- no endpoint on this leg. It publishes the endpoint where it already
/// does, in [`RealChainBackend::open_stream`] (`TokenContract.open(endpointCipher)`), which 4.0.35
/// left untouched. Taking the new leg would move handover from the moment the stream opens to the
/// moment the bond is posted, and nothing in the 4.0.35 migration forces that.
pub(super) fn note_fund_deal_params(nonce: u64, gas_shell: u128, amount: u128) -> Value {
    json!({
        "nonce": nonce.to_string(),
        "gasShell": gas_shell.to_string(),
        "amount": amount.to_string(),
        "endpointCipher": Value::Null,
    })
}

/// The exact argument object a caller sends to `TokenContract.fundDeal`. Pure for the same reason,
/// and carrying the same 4.0.35 `endpointCipher optional(bytes)` leg, sent as `null` for the same
/// reason.
pub(super) fn deal_fund_deal_params(amount: u128) -> Value {
    json!({
        "amount": amount.to_string(),
        "endpointCipher": Value::Null,
    })
}

/// The exact argument object [`RealChainBackend::note_fund_deploy_shell`] sends.

/// **TWO ARGUMENTS, NOT THREE.** Contracts 4.0.34 removed the `rootModelShell` leg --
/// `fundDeployShell(uint64 nonce, uint128 tcShell)` (`contracts/dex/PrivateNote.sol:1143`). The leg
/// existed only because a `RootModel` used to be deployed by its owner as an external message, so
/// somebody had to place native gas at that uninit address first and the note was the somebody.
/// `SuperRoot.deployRootModel` performs an internal `new` now (`contracts/airegistry/SuperRoot.sol:193`)
/// and an internal deploy carries its own value (`ROOT_MODEL_DEPLOY_VALUE = 5 vmshell`,
/// `contracts/airegistry/SuperRoot.sol:58`), so there is nothing left to pre-fund. The note's private
/// `_rootModelAddr` helper was deleted with the leg.

/// Kept pure for the same reason as [`note_fund_deal_params`]: the encoded shape is what the offline
/// regression pins, so a third argument cannot come back unnoticed.
pub(super) fn note_fund_deploy_shell_params(nonce: u64, tc_shell: u128) -> Value {
    json!({
        "nonce": nonce.to_string(),
        "tcShell": tc_shell.to_string(),
    })
}

/// The exact argument object [`RealChainBackend::note_deploy_deal`] sends to
/// `PrivateNote.deployDeal` (contracts 4.0.36).

/// **THE DEAL IS DEPLOYED BY THE NOTE NOW, AND ONLY BY THE NOTE.** Until 4.0.36 this client signed
/// an EXTERNAL message carrying the whole contract code and sent it at a pre-funded uninit address.
/// The 4.0.36 constructor refuses that at the door: it requires `msg.sender` to BE the canonical
/// note for `depositIdentifierHash` (`contracts/airegistry/TokenContract.sol:285`), and an external
/// message has no sender to offer. So there is nothing left to sign and nothing left to pre-fund --
/// one owner call from the note replaces the pair.

/// `depositIdentifierHash` is deliberately NOT an argument: the note passes its own, which is what
/// makes the authentication mean anything. A caller-supplied one would let the caller name the note
/// the deal believes in.

/// `gasReserve` is ECC[2] SHELL and is the deal's whole reserve for life -- each entry burns its
/// measured charge out of it (`gosh.burnecc`, the `GAS_*` table in
/// `contracts/airegistry/modifiers/modifiers.sol`). It is NOT the old life-support budget: the deal
/// lands in the note's configured dapp now and mints its own native floor. No entry of this
/// generation refills the reserve, so the figure here is chosen once.

/// Kept pure for the same reason as [`note_fund_deploy_shell_params`]: the encoded shape is what the
/// offline regression pins, so an argument cannot appear or vanish unnoticed.
pub(super) fn note_deploy_deal_params(
    nonce: u64,
    model_name: &str,
    model_hash: &str,
    price_per_tick: u128,
    max_ticks: u128,
    gas_reserve: u128,
) -> Value {
    json!({
        "nonce": nonce.to_string(),
        "modelName": model_name,
        "modelHash": model_hash,
        "pricePerTick": price_per_tick.to_string(),
        "maxTicks": max_ticks.to_string(),
        "gasReserve": gas_reserve.to_string(),
    })
}

/// 0.34 -- the note has no `RootModel` funding leg any more, so a caller asking for one is
/// refused rather than silently served a message that funds only the deal.

/// A non-zero request here used to mean "put `rootModelShell` at the RootModel's uninit deploy
/// address". `PrivateNote.fundDeployShell` no longer has that argument, so honouring the call would
/// mean dropping the amount on the floor and reporting success -- the RootModel would stay unfunded
/// and the caller would never learn it. It cannot be silently mapped onto the deal's leg either:
/// that is a different contract at a different address.
pub(super) fn root_model_deploy_shell_unsupported(root_model_shell: u128) -> Option<String> {
    if root_model_shell == 0 {
        return None;
    }
    Some(format!(
        "fundDeployShell was asked for {root_model_shell} raw ECC[2] of RootModel gas, but contracts \
         4.0.34 removed that leg: PrivateNote.fundDeployShell takes (nonce, tcShell) only \
         (contracts/dex/PrivateNote.sol:1143). A RootModel is deployed by SuperRoot with its own \
         ROOT_MODEL_DEPLOY_VALUE = 5 vmshell (contracts/airegistry/SuperRoot.sol:58) and mints its own \
         gas from SuperRoot's configured dapp (RootModel.ensureBalance), so it needs no note funding. \
         Pass root_model_shell = 0."
    ))
}

fn money_submit_identity(signed_boc: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(signed_boc.as_bytes());
    format!(
        "boc-sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Stage-aware failure from a non-idempotent money write.
#[derive(Debug, thiserror::Error)]
pub enum MoneySubmitError {
    #[error("money write failed before any message POST: {source}")]
    Preparation {
        #[source]
        source: anyhow::Error,
    },
    #[error("money message POST outcome is ambiguous: {source}")]
    Ambiguous {
        #[source]
        source: anyhow::Error,
    },
    #[error("money message POST was rejected: {source}")]
    Rejected {
        #[source]
        source: anyhow::Error,
    },
}

/// The message POST received an HTTP success response, but its response body could not be decoded.

/// This is deliberately stage-specific: the signed message has already left the process, so callers that
/// submit a cumulative claim must reconcile the authoritative claim cursor before deciding whether to retry.
#[derive(Debug, thiserror::Error)]
#[error(
    "message POST received HTTP {status}, but its response JSON could not be decoded: {source}"
)]
pub(super) struct MessagePostResponseDecodeError {
    status: reqwest::StatusCode,
    #[source]
    source: reqwest::Error,
}

impl MoneySubmitError {
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Self::Ambiguous { .. })
    }

    /// Clearing an exactly-once journal is safe only when no POST was attempted or when the
    /// protocol returned a decoded rejection. Every other outcome may have landed.
    pub fn clears_journal(&self) -> bool {
        matches!(self, Self::Preparation { .. } | Self::Rejected { .. })
    }
}

fn consume_new_fill_batch(
    cursor: &mut MatchWatchCursor,
    mut fills: Vec<(i64, MatchedFill)>,
) -> Vec<MatchedFill> {
    fills.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.token_contract.cmp(&b.1.token_contract))
            .then_with(|| a.1.ticks.cmp(&b.1.ticks))
            .then_with(|| a.1.price_per_tick.cmp(&b.1.price_per_tick))
    });
    let mut out = Vec::new();
    let mut consumed = Vec::new();
    let mut unique_new = BTreeSet::new();
    for (created_at, fill) in fills {
        if cursor.has_seen(created_at, &fill.token_contract) {
            continue;
        }
        if unique_new.insert((
            created_at,
            fill.token_contract.clone(),
            fill.ticks,
            fill.price_per_tick,
        )) {
            consumed.push((created_at, fill.token_contract.clone()));
            out.push(fill);
        }
    }
    cursor.record_seen_batch(consumed);
    out
}

fn correlate_fill_batch(
    expected: Option<&MatchedFill>,
    fills: &[MatchedFill],
) -> Result<Option<MatchedFill>> {
    let Some(expected) = expected else {
        return Ok(fills.last().cloned());
    };
    if let Some(fill) = fills.iter().find(|fill| {
        fill.token_contract == expected.token_contract
            && fill.ticks == expected.ticks
            && fill.price_per_tick == expected.price_per_tick
    }) {
        return Ok(Some(fill.clone()));
    }
    let Some(fill) = fills.first() else {
        return Ok(None);
    };
    Err(anyhow!(
        "buyer fill correlation failed: expected tokenContract {} ticks {} price_per_tick {}, \
         got tokenContract {} ticks {} price_per_tick {}; refusing wrong-fill attribution",
        expected.token_contract,
        expected.ticks,
        expected.price_per_tick,
        fill.token_contract,
        fill.ticks,
        fill.price_per_tick
    ))
}

#[async_trait::async_trait]
pub(super) trait InferenceFillPoller: Send + Sync {
    async fn poll(&self, cursor: &mut MatchWatchCursor) -> Result<Vec<MatchedFill>>;
}

struct RealInferenceFillPoller<'a> {
    chain: &'a RealChainBackend,
    note: &'a Address,
    order_book: &'a Address,
}

#[async_trait::async_trait]
impl InferenceFillPoller for RealInferenceFillPoller<'_> {
    async fn poll(&self, cursor: &mut MatchWatchCursor) -> Result<Vec<MatchedFill>> {
        self.chain
            .poll_inference_filled_tcs(self.note, self.order_book, true, cursor)
            .await
    }
}

pub(super) async fn wait_correlated_inference_fill(
    poller: &dyn InferenceFillPoller,
    cursor: &mut MatchWatchCursor,
    expected: Option<&MatchedFill>,
    timeout: std::time::Duration,
    poll_interval: std::time::Duration,
    timeout_context: &str,
) -> Result<MatchedFill> {
    let start = std::time::Instant::now();
    loop {
        let fills = poller.poll(cursor).await?;
        if let Some(fill) = correlate_fill_batch(expected, &fills)? {
            return Ok(fill);
        }
        if start.elapsed() >= timeout {
            return Err(anyhow!(timeout_context.to_string()));
        }
        tokio::time::sleep(poll_interval).await;
    }
}

#[path = "operator_wallet.rs"]
mod operator_wallet;

#[path = "test_giver.rs"]
pub(crate) mod test_giver;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainDoctorStatus {
    Pass,
    Fail,
    Skip,
}

impl ChainDoctorStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainDoctorCheck {
    pub name: String,
    pub status: ChainDoctorStatus,
    pub address: Option<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainDoctorReport {
    pub network: String,
    pub endpoint: String,
    pub manifest_generation: Option<String>,
    pub chain_generation: Option<String>,
    /// Positive means the local clock is ahead of the chain; negative means it is behind.
    pub clock_skew_seconds: i64,
    pub versions: Vec<(String, String)>,
    pub checks: Vec<ChainDoctorCheck>,
}

impl ChainDoctorReport {
    pub fn is_ok(&self) -> bool {
        self.checks
            .iter()
            .all(|c| c.status != ChainDoctorStatus::Fail)
    }

    pub fn fail_summary(&self) -> String {
        self.checks
            .iter()
            .filter(|c| c.status == ChainDoctorStatus::Fail)
            .map(|c| format!("{}: {}", c.name, c.message))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

pub const CHAIN_DOCTOR_CHECK_COUNT: usize = 14;

fn record_doctor_check(
    checks: &mut Vec<ChainDoctorCheck>,
    observe: &mut impl FnMut(usize, &ChainDoctorCheck),
    check: ChainDoctorCheck,
) {
    observe(checks.len() + 1, &check);
    checks.push(check);
}

fn signed_clock_skew_seconds(local_unix: u64, chain_unix: u64) -> i64 {
    let difference = i128::from(local_unix) - i128::from(chain_unix);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn doctor_report_endpoint(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return "<invalid-endpoint>".to_string();
    };
    let _ = url.set_password(None);
    let _ = url.set_username("");
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

fn getter_u128(v: &Value, key: &str) -> Option<u128> {
    let raw = &v[key];
    if let Some(n) = raw.as_u64() {
        return Some(u128::from(n));
    }
    let s = raw.as_str()?.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u128>().ok()
    }
}

fn subscription_order_is_active_for_owner(
    order_id: u128,
    order: &Value,
    owner_note: &str,
) -> Result<bool> {
    let amount = getter_u128(order, "amount")
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no amount: {order}"))?;
    let canonical_empty = amount == 0
        && order
            .get("note")
            .and_then(Value::as_str)
            .is_some_and(is_absent_address)
        && order
            .get("tokenContract")
            .and_then(Value::as_str)
            .is_some_and(is_absent_address)
        && ["price", "escrow", "deadline", "flags", "ts"]
            .iter()
            .all(|field| getter_u128(order, field) == Some(0))
        && getter_bool(order, "isBuy") == Some(false);
    if canonical_empty {
        return Ok(false);
    }
    if amount == 0 {
        return Err(anyhow!(
            "getOrder({order_id}) has non-canonical zero-amount row: {order}"
        ));
    }
    let Some(note) = order.get("note").and_then(Value::as_str) else {
        return Err(anyhow!("getOrder({order_id}) has no owner note: {order}"));
    };
    let note = Address::parse(note)
        .map_err(|error| {
            anyhow!(
                "getOrder({order_id}) owner note {}: {error}",
                display_dexdo_address(note)
            )
        })?
        .with_workchain();
    let owner_note = Address::parse(owner_note)
        .map_err(|error| anyhow!("expected owner note {owner_note}: {error}"))?
        .with_workchain();
    let is_buy = getter_bool(order, "isBuy")
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no isBuy: {order}"))?;
    if !note.eq_ignore_ascii_case(&owner_note) {
        return Err(anyhow!(
            "getOrder({order_id}) owner {} contradicts expected subscription owner {}: {order}",
            display_dexdo_address(&note),
            display_dexdo_address(&owner_note)
        ));
    }
    if !is_buy {
        return Err(anyhow!(
            "getOrder({order_id}) is a SELL, not the expected subscription BUY: {order}"
        ));
    }
    let token_contract = order
        .get("tokenContract")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no tokenContract: {order}"))?;
    if !is_absent_address(token_contract) {
        return Err(anyhow!(
            "getOrder({order_id}) subscription BUY has non-zero tokenContract \
             {}: {order}",
            display_token_contract(token_contract)
        ));
    }
    let price = getter_u128(order, "price")
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no price: {order}"))?;
    if price == 0 || !price.is_multiple_of(PRICE_STEP) {
        return Err(anyhow!(
            "getOrder({order_id}) subscription price {price} is not a positive PRICE_STEP \
             multiple: {order}"
        ));
    }
    let minimum_ticks = u128::from(SUBSCRIPTION_WEEKS);
    if !(minimum_ticks..=SUBSCRIPTION_MAX_TICKS).contains(&amount)
        || !amount.is_multiple_of(minimum_ticks)
    {
        return Err(anyhow!(
            "getOrder({order_id}) subscription amount {amount} is outside the canonical \
             four-week shape: {order}"
        ));
    }
    let escrow = getter_u128(order, "escrow")
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no escrow: {order}"))?;
    check_subscription_buy_reserve(escrow, amount, price)
        .map_err(|error| anyhow!("getOrder({order_id}) subscription reserve: {error}; {order}"))?;
    let order_flags = getter_u128(order, "flags")
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no flags: {order}"))?;
    let expected_flags = u128::from(flags::AON | flags::SUBSCRIPTION);
    if order_flags != expected_flags {
        return Err(anyhow!(
            "getOrder({order_id}) flags 0x{order_flags:02x} contradict exact \
             AON|SUBSCRIPTION flags 0x{expected_flags:02x}: {order}"
        ));
    }
    let deadline = getter_u128(order, "deadline")
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no deadline: {order}"))?;
    let timestamp = getter_u128(order, "ts")
        .ok_or_else(|| anyhow!("getOrder({order_id}) has no ts: {order}"))?;
    if deadline == 0 || deadline <= timestamp || deadline > u128::from(u64::MAX) {
        return Err(anyhow!(
            "getOrder({order_id}) subscription deadline {deadline} is invalid for timestamp \
             {timestamp}: {order}"
        ));
    }
    Ok(true)
}

fn coalesce_correlated_subscription_placements(
    mut placements: Vec<InferenceSubscriptionPlacement>,
    expected_owner: &str,
    expected_price_per_tick: u128,
    expected_ticks: u128,
) -> Result<Vec<InferenceSubscriptionPlacement>> {
    placements.sort_by(|left, right| {
        left.order_id
            .cmp(&right.order_id)
            .then_with(|| left.created_at.cmp(&right.created_at))
    });
    let mut correlated = Vec::new();
    let mut start = 0;
    while start < placements.len() {
        let order_id = placements[start].order_id;
        let mut end = start + 1;
        while end < placements.len() && placements[end].order_id == order_id {
            end += 1;
        }
        let group = &placements[start..end];
        let has_expected = group.iter().any(|placement| {
            placement.buyer_note.eq_ignore_ascii_case(expected_owner)
                && placement.max_price_per_tick == expected_price_per_tick
                && placement.ticks == expected_ticks
        });
        if has_expected {
            let first = &group[0];
            if group.iter().any(|placement| placement != first) {
                return Err(anyhow!(
                    "InferenceSubscriptionPlaced order #{order_id} has conflicting authenticated \
                     placement facts: {group:?}"
                ));
            }
            correlated.push(first.clone());
        }
        start = end;
    }
    Ok(correlated)
}

fn getter_bool(v: &Value, key: &str) -> Option<bool> {
    let raw = &v[key];
    if let Some(b) = raw.as_bool() {
        return Some(b);
    }
    let s = raw.as_str()?.trim();
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn successful_inbound_call(node: &Value) -> bool {
    let transaction = &node["dst_transaction"];
    if transaction.is_null() || transaction["aborted"].as_bool() != Some(false) {
        return false;
    }
    let stage_succeeded = |stage: &Value, code: &str| {
        if stage.is_null() {
            return false;
        }
        let success = stage["success"].as_bool();
        let exit_code = getter_u128(stage, code);
        success != Some(false)
            && exit_code.is_none_or(|value| value == 0)
            && (success == Some(true) || exit_code == Some(0))
    };
    stage_succeeded(&transaction["compute"], "exit_code")
        && stage_succeeded(&transaction["action"], "result_code")
}

fn details_has_withdrawn(details: &Value) -> Option<bool> {
    getter_bool(details, "hasWithdrawn")
}

fn note_withdrawn_sell_offer_message(note: &Address) -> String {
    format!(
        "seller post_offer aborted: this note has withdrawn and can no longer post sell offers -- deploy/use a \
         fresh note, re-provision the market, and retry. note={}; postSellOffer would revert \
         ERR_INVALID_STATE 151 because PrivateNote._hasWithdrawn=true.",
        display_dexdo_address(note)
    )
}

fn note_withdrawn_buy_message(note: &Address) -> String {
    format!(
        "buyer place aborted: this note has withdrawn and can no longer place buys (deploy/use a fresh note); \
         the chain rejects it with ERR_INVALID_STATE 151 because PrivateNote._hasWithdrawn=true. note={}",
        display_dexdo_address(note)
    )
}

fn buyer_note_withdrawn_guard(note: &Address, details: Option<&Value>) -> Result<()> {
    match details.and_then(details_has_withdrawn) {
        Some(true) => Err(anyhow!(note_withdrawn_buy_message(note))),
        Some(false) => Ok(()),
        None => {
            eprintln!(
                "buyer place preflight note: PrivateNote.getDetails for note {} did not expose \
                 hasWithdrawn; continuing without the withdrawn-state guard",
                display_dexdo_address(note)
            );
            Ok(())
        }
    }
}

fn seller_note_withdrawn_check(note: &Address, actual: Option<bool>) -> ChainDoctorCheck {
    let (status, actual, message) = match actual {
        Some(false) => (
            ChainDoctorStatus::Pass,
            Some("hasWithdrawn=false".to_string()),
            "seller note has not withdrawn; postSellOffer is not blocked by _hasWithdrawn".to_string(),
        ),
        Some(true) => (
            ChainDoctorStatus::Fail,
            Some("hasWithdrawn=true".to_string()),
            note_withdrawn_sell_offer_message(note),
        ),
        None => (
            ChainDoctorStatus::Fail,
            Some("hasWithdrawn=<missing>".to_string()),
            "PrivateNote.getDetails did not expose hasWithdrawn; refusing to prove postSellOffer safety"
                .to_string(),
        ),
    };
    ChainDoctorCheck {
        name: "seller PrivateNote withdrawn state".to_string(),
        status,
        address: Some(display_dexdo_address(note)),
        expected: Some("hasWithdrawn=false".to_string()),
        actual,
        message,
    }
}

/// these verdicts named the chain build on every network, and `doctor` prints one of them thirteen
/// times over on a mainnet run. The pinned hashes are GENERATION pins -- they belong to a BUILD and
/// not to a chain, and any number of chains may serve one -- so naming a network here asserted
/// something the comparison never established,
/// and on mainnet it was simply false. The chain the run is actually on is named once, by
/// `endpoint_reachable_check` and the report header, both sourced from the deployment manifest.
pub(super) fn code_hash_check(
    name: &str,
    address: Option<&Address>,
    expected: &str,
    actual: Option<&str>,
) -> ChainDoctorCheck {
    let expected = normalize_code_hash(expected).unwrap_or_else(|| expected.to_string());
    let actual = actual.and_then(normalize_code_hash);
    let (status, message) = match actual.as_deref() {
        Some(a) if a == expected => (
            ChainDoctorStatus::Pass,
            "binary pin matches the live chain".to_string(),
        ),
        Some(a) => (
            ChainDoctorStatus::Fail,
            // NOT "rebuild from dev HEAD". That advice sends the operator to do the one thing that
            // cannot help: the pin is a source literal, not something the compiler derives, so
            // rebuilding the same tree carries the same number over and the refusal repeats word
            // for word. Measured on mainnet's 4.0.36 redeploy: a build made from dev HEAD,
            // carrying the right artifacts, was told it was stale and the rebuild changed nothing.

            // AND NOT "repin the build" either, which was the first replacement.
            // wants an action the reader can take FROM THIS SHELL, and editing a pin table is not
            // one for whoever is holding a released binary -- it is work for whoever ships the next
            // one. What the operator can do is get a build made for this chain, so that is what it
            // says. The numbers stay in the check's own `expected`/`actual` fields for whoever is
            // going to do the repinning.

            // NO FILE IS NAMED, deliberately. Five checks share this text and they do not share a
            // source: `SuperRoot/RootPN/RootOracle code hash` read `GENERATION_PINS`, while
            // `RootModel code hash` and `TokenContract code hash` read module constants
            // (`ROOTMODEL_PINNED_TC_CODE_HASH`). Naming one of them would be wrong for the others.

            // Two clauses and no more: what did not happen, then what to do. The
            // renderer prints this once per failing check and again inside `fail_summary()`, so
            // every extra sentence is paid for twice.
            format!(
                "the chain serves code this build does not pin: binary pins {expected}, live is \
                 {a}. Use a dexdo built for this chain's generation; rebuilding this one carries \
                 the same pin over."
            ),
        ),
        None => (
            ChainDoctorStatus::Fail,
            "live account is missing, inactive, or exposes no code_hash".to_string(),
        ),
    };
    ChainDoctorCheck {
        name: name.to_string(),
        status,
        address: address.map(display_dexdo_address),
        expected: Some(expected),
        actual,
        message,
    }
}

// The generation checks `doctor` runs against the fixed chain roots and the per-model book, split
// out so each can be driven with a chain-supplied value in a test. `live_code_hash` is the
// `code_hash` read from the live account.

// Every one of them is unconditional, and one `Fail` aborts `chain_doctor_preflight`, so the
// constants they name gate provision/seller/buyer/`note deploy`/`note withdraw` ahead of every note
// guard. None of them may take its expected value from a vendored `.tvc`.

/// The SuperRoot generation check.
pub(super) fn superroot_generation_check(
    superroot: &Address,
    expected: &str,
    live_code_hash: Option<&str>,
) -> ChainDoctorCheck {
    code_hash_check(
        "SuperRoot code hash",
        Some(superroot),
        expected,
        live_code_hash,
    )
}

/// The RootPN generation check.
pub(super) fn rootpn_generation_check(
    rootpn: &Address,
    expected: &str,
    live_code_hash: Option<&str>,
) -> ChainDoctorCheck {
    code_hash_check("RootPN code hash", Some(rootpn), expected, live_code_hash)
}

/// The RootOracle generation check.
pub(super) fn rootoracle_generation_check(
    rootoracle: &Address,
    expected: &str,
    live_code_hash: Option<&str>,
) -> ChainDoctorCheck {
    code_hash_check(
        "RootOracle code hash",
        Some(rootoracle),
        expected,
        live_code_hash,
    )
}

/// The per-model `InferenceOrderBook` generation check.
pub(super) fn inference_orderbook_generation_check(
    book: &Address,
    expected: &str,
    live_code_hash: Option<&str>,
) -> ChainDoctorCheck {
    code_hash_check(
        "InferenceOrderBook code hash",
        Some(book),
        expected,
        live_code_hash,
    )
}

/// The PrivateNote generation check `doctor` runs, split out for the same reason. `rootpn_details` is
/// the `RootPN.getDetails()` getter result; the PrivateNote code RootPN currently mints is its
/// `privateNoteCodeHash` field.

/// The comparison is deliberately against the active [`GenerationPins::private_note`] value the
/// money-path guards enforce -- and NOT against the embedded `PRIVATENOTE_TVC`, which this CLI never
/// deploys and which is therefore no evidence about the chain.
pub(super) fn private_note_pin_check(expected: &str, rootpn_details: &Value) -> ChainDoctorCheck {
    code_hash_check(
        "PrivateNote code hash (RootPN pin)",
        None,
        expected,
        rootpn_details["privateNoteCodeHash"].as_str(),
    )
}

pub(super) fn active_check(name: &str, address: &Address, active: bool) -> ChainDoctorCheck {
    ChainDoctorCheck {
        name: name.to_string(),
        status: if active {
            ChainDoctorStatus::Pass
        } else {
            ChainDoctorStatus::Fail
        },
        address: Some(address.with_workchain()),
        expected: None,
        actual: Some(if active { "active" } else { "inactive" }.to_string()),
        message: if active {
            "account is active".to_string()
        } else {
            "manifest points at an inactive/undeployed account".to_string()
        },
    }
}

fn pass_check(name: &str, message: &str) -> ChainDoctorCheck {
    ChainDoctorCheck {
        name: name.to_string(),
        status: ChainDoctorStatus::Pass,
        address: None,
        expected: None,
        actual: None,
        message: message.to_string(),
    }
}

fn skipped_check(name: &str, message: &str) -> ChainDoctorCheck {
    ChainDoctorCheck {
        name: name.to_string(),
        status: ChainDoctorStatus::Skip,
        address: None,
        expected: None,
        actual: None,
        message: message.to_string(),
    }
}

fn clock_skew_check(local_unix: u64, chain_unix: u64) -> ChainDoctorCheck {
    let (skew_secs, direction, permitted_secs) = if local_unix >= chain_unix {
        (local_unix - chain_unix, "ahead of", MAX_CLOCK_AHEAD_SECS)
    } else {
        (chain_unix - local_unix, "behind", MAX_CLOCK_BEHIND_SECS)
    };
    let status = if skew_secs <= permitted_secs {
        ChainDoctorStatus::Pass
    } else {
        ChainDoctorStatus::Fail
    };
    let message = if status == ChainDoctorStatus::Pass {
        format!(
            "local clock is within the signed-message safety threshold (skew={skew_secs}s, \
             local_unix={local_unix}, chain_unix={chain_unix})"
        )
    } else {
        format!(
            "CLOCK_SKEW: local clock is {skew_secs}s {direction} chain time \
             (local_unix={local_unix}, chain_unix={chain_unix}); refusing signed writes before \
             submit: the pinned SDK gives signed messages {SDK_MESSAGE_EXPIRY_SECS}s to expire and \
             contracts strictly require block.timestamp < expireAt < block.timestamp + \
             {CONTRACT_MESSAGE_WINDOW_SECS}. Fix system time / NTP and retry."
        )
    };
    ChainDoctorCheck {
        name: "local clock vs chain time".to_string(),
        status,
        address: None,
        expected: Some(format!(
            "behind<={MAX_CLOCK_BEHIND_SECS}s, ahead<={MAX_CLOCK_AHEAD_SECS}s"
        )),
        actual: Some(format!(
            "skew={skew_secs}s local_unix={local_unix} chain_unix={chain_unix}"
        )),
        message,
    }
}

fn local_unix_secs() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("local system clock is before the Unix epoch")?
        .as_secs())
}

/// A GraphQL response that answered **HTTP 200** and put its failure in the body's `errors` array.

/// mainnet signals its own connection-pool limit this way -- status 200, and
/// `errors[].message = "pool timed out while waiting for an open connection"`. Flattened into a
/// string, that is indistinguishable from a permanent GraphQL error, so the read retry declined it
/// and every signed-write preflight became a coin flip on mainnet.

/// Carried as a type so the decision is structural. `Display` is the errors array verbatim, which is
/// what the previous `anyhow!` rendered, so nothing that reads the message text changes.
#[derive(Debug)]
pub struct GraphQlBodyError {
    body: String,
    messages: Vec<String>,
}

impl GraphQlBodyError {
    fn from_errors(errors: &Value) -> Self {
        let messages = errors
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("message").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            body: errors.to_string(),
            messages,
        }
    }

    /// Did the server say its connection pool was exhausted?

    /// Deliberately narrow: this one condition and no other body error. A predicate that treated
    /// every `errors` entry as transient would start repeating reads the server refused on purpose.
    fn is_pool_exhaustion(&self) -> bool {
        self.messages
            .iter()
            .any(|message| message.to_ascii_lowercase().contains("pool timed out"))
    }
}

impl std::fmt::Display for GraphQlBodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.body)
    }
}

impl std::error::Error for GraphQlBodyError {}

async fn fetch_chain_time_secs(http: &reqwest::Client, endpoint: &str) -> Result<u64> {
    let (graphql_url, _) = endpoint_urls(endpoint)?;
    let body = json!({
        "query": "{ blockchain { blocks(last:1){ edges { node { gen_utime } } } } }"
    });
    let response = http
        .post(&graphql_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {graphql_url} for chain time"))?;
    let response: Value = chain_response_for_status(response)
        .await?
        .json()
        .await
        .context("parse GraphQL chain-time response")?;
    if let Some(errors) = response.get("errors").filter(|errors| !errors.is_null()) {
        // raised as a TYPE, not a flattened string, so the read-retry predicate can ask what
        // this was instead of sniffing text. `{:#}` still renders exactly as before -- the context
        // carries the old prefix and the body's `Display` is the same JSON.
        return Err(anyhow::Error::new(GraphQlBodyError::from_errors(errors))
            .context("GraphQL chain-time errors"));
    }
    response
        .pointer("/data/blockchain/blocks/edges/0/node/gen_utime")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("chain time: latest block is missing gen_utime"))
}

/// Compare the two clocks with BOTH readings taken inside one attempt, chain first.

/// Retrying only the chain read is the obvious repair and it is wrong. Rust evaluates arguments in
/// order, so the local clock was read before the chain call started; wrapping just that call moves
/// the second reading later by however long the retries took, and the whole drift lands in one
/// direction -- "local is behind".

/// The arithmetic decides it. `TRANSIENT_READ_TOTAL_BUDGET` is 45s while [`MAX_CLOCK_BEHIND_SECS`]
/// is 30s, and in the other direction [`MAX_CLOCK_AHEAD_SECS`] leaves 250s. A machine with a
/// perfectly synchronised clock that caught one slow attempt would be measured as more than thirty
/// seconds behind and refused, telling its operator to fix system time that was never wrong -- on
/// the money path, and once per note. That is worse than the dropped read it replaces: a transient
/// error is re-run, a confident wrong diagnosis is acted on.

/// Reading the chain first and the local clock immediately after leaves only the local call between
/// them, and pushes what remains toward "ahead", where the headroom is.

/// The two readings are injected so a regression can prove the measured skew does not grow with the
/// number of retries -- the property this exists for, and the one a later change to the budget would
/// otherwise silently break.
async fn clock_skew_check_from_one_attempt<Read, Fut, Now>(
    read_chain: Read,
    now: Now,
) -> Result<ChainDoctorCheck>
where
    Read: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<u64>>,
    Now: Fn() -> Result<u64>,
{
    retry_transient_read(|| async {
        let chain_unix = read_chain().await?;
        let local_unix = now()?;
        Ok(clock_skew_check(local_unix, chain_unix))
    })
    .await
}

/// Fail closed before a signed SDK write when the operator clock is unsafe for the contracts'
/// five-minute `expireAt` window.
pub async fn chain_clock_skew_preflight(endpoint: &str) -> Result<()> {
    let http = chain_http_client()?;
    let check = clock_skew_check_from_one_attempt(
        || fetch_chain_time_secs(&http, endpoint),
        local_unix_secs,
    )
    .await?;
    if check.status == ChainDoctorStatus::Fail {
        return Err(anyhow!(check.message));
    }
    Ok(())
}

fn dense_string_map(labels: &[String]) -> Value {
    let mut m = serde_json::Map::new();
    for (i, name) in labels.iter().enumerate() {
        m.insert(i.to_string(), Value::String(name.clone()));
    }
    Value::Object(m)
}

fn u128_array(values: &[u128]) -> Vec<String> {
    values.iter().map(u128::to_string).collect()
}

fn private_note_pmp_exit_payload(
    event_id: &str,
    oracle_list_hash: &str,
    token_type: u32,
) -> Result<Value> {
    Ok(json!({
        "eventId": normalize_uint256_hex(event_id)?,
        "oracleListHash": normalize_uint256_hex(oracle_list_hash)?,
        "tokenType": token_type,
    }))
}

fn oracle_withdraw_fees_payload(to: &str, amount: u128) -> Value {
    json!({
        "to": to,
        "amount": amount.to_string(),
    })
}

/// `_stakes` and `_openOrdersByEvent` use `tvm.hash(abi.encode(eventId,
/// oracleListHash, tokenType))`. Reproduce that exact ABI 2.4 cell here instead of matching a
/// stake by only the oracle/token suffix and risking a different event with the same pair.
fn pmp_stake_key(event_id: &str, oracle_list_hash: &str, token_type: u32) -> Result<String> {
    let params = [
        tvm_abi::Param::new("eventId", tvm_abi::ParamType::Uint(256)),
        tvm_abi::Param::new("oracleListHash", tvm_abi::ParamType::Uint(256)),
        tvm_abi::Param::new("tokenType", tvm_abi::ParamType::Uint(32)),
    ];
    let values = private_note_pmp_exit_payload(event_id, oracle_list_hash, token_type)?;
    let tokens = tvm_abi::token::Tokenizer::tokenize_all_params(&params, &values)
        .context("encode PrivateNote PMP stake tuple")?;
    let cell = tvm_abi::TokenValue::pack_values_into_chain(
        &tokens,
        Vec::new(),
        &tvm_abi::contract::ABI_VERSION_2_4,
    )
    .context("pack PrivateNote PMP stake tuple")?
    .into_cell()
    .context("build PrivateNote PMP stake tuple cell")?;
    Ok(format!("0x{}", cell.repr_hash().to_hex_string()))
}

fn pubkey_uint256(keys: &KeyPair) -> String {
    format!("0x{}", keys.public_hex().trim_start_matches("0x"))
}

fn decimal_to_hex(dec: &str) -> Option<String> {
    let mut digits = dec
        .trim_start_matches('0')
        .bytes()
        .map(|b| b.checked_sub(b'0'))
        .collect::<Option<Vec<_>>>()?;
    if digits.is_empty() {
        return Some("0".to_string());
    }
    let mut out = Vec::new();
    while !digits.is_empty() {
        let mut next = Vec::new();
        let mut rem = 0u8;
        for d in digits {
            let n = rem as u16 * 10 + d as u16;
            let q = (n / 16) as u8;
            rem = (n % 16) as u8;
            if q != 0 || !next.is_empty() {
                next.push(q);
            }
        }
        out.push(b"0123456789abcdef"[rem as usize] as char);
        digits = next;
    }
    Some(out.into_iter().rev().collect())
}

fn normalize_uint256_hex(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(anyhow!("empty uint256"));
    }
    let hex = if let Some(h) = s.strip_prefix("0x") {
        h.to_string()
    } else if s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b.is_ascii_alphabetic())
    {
        s.to_string()
    } else if s.bytes().all(|b| b.is_ascii_digit()) {
        decimal_to_hex(s).ok_or_else(|| anyhow!("invalid uint256 decimal `{s}`"))?
    } else {
        return Err(anyhow!("invalid uint256 `{s}`"));
    };
    if hex.is_empty() || hex.len() > 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(anyhow!("invalid uint256 `{s}`"));
    }
    Ok(format!("0x{hex:0>64}").to_lowercase())
}

fn value_to_uint256_hex(v: &Value) -> Option<String> {
    v.as_str()
        .and_then(|s| normalize_uint256_hex(s).ok())
        .or_else(|| {
            v.as_u64()
                .and_then(|n| normalize_uint256_hex(&n.to_string()).ok())
        })
}

fn uint256_map_entry<'a>(map: &'a Value, wanted: &str) -> Option<&'a Value> {
    if let Some(entries) = map.as_object() {
        return entries.iter().find_map(|(key, value)| {
            (normalize_uint256_hex(key).ok().as_deref() == Some(wanted)).then_some(value)
        });
    }
    map.as_array()?.iter().find_map(|entry| {
        if let Some(object) = entry.as_object() {
            let key = object.get("key").or_else(|| object.get("0"))?;
            let value = object.get("value").or_else(|| object.get("1"))?;
            return (value_to_uint256_hex(key).as_deref() == Some(wanted)).then_some(value);
        }
        let pair = entry.as_array()?;
        (pair.len() == 2 && value_to_uint256_hex(&pair[0]).as_deref() == Some(wanted))
            .then(|| &pair[1])
    })
}

fn uint32_map_entry(map: &Value, wanted: u32) -> Option<&Value> {
    if let Some(entries) = map.as_object() {
        return entries.iter().find_map(|(key, value)| {
            (parse_u128_literal(key) == Some(u128::from(wanted))).then_some(value)
        });
    }
    map.as_array()?.iter().find_map(|entry| {
        if let Some(object) = entry.as_object() {
            let key = object.get("key").or_else(|| object.get("0"))?;
            let value = object.get("value").or_else(|| object.get("1"))?;
            return (value_u128(key) == Some(u128::from(wanted))).then_some(value);
        }
        let pair = entry.as_array()?;
        (pair.len() == 2 && value_u128(&pair[0]) == Some(u128::from(wanted))).then(|| &pair[1])
    })
}

fn optional_address(value: &Value) -> Option<String> {
    if let Some(address) = value.as_str().filter(|address| !address.trim().is_empty()) {
        return Some(address.trim().to_string());
    }
    value
        .as_object()
        .and_then(|object| object.get("value").or_else(|| object.get("0")))
        .and_then(Value::as_str)
        .filter(|address| !address.trim().is_empty())
        .map(|address| address.trim().to_string())
}

fn requested_bounds_to_uint256_hex(bounds: &[String]) -> Result<Vec<String>> {
    bounds.iter().map(|b| normalize_uint256_hex(b)).collect()
}

fn range_bounds_to_uint256_hex(bounds: &Value) -> Option<Vec<String>> {
    bounds
        .as_array()?
        .iter()
        .map(value_to_uint256_hex)
        .collect()
}

fn normalize_addr(raw: &str) -> Result<String> {
    Ok(Address::parse(raw)?.with_workchain().to_ascii_lowercase())
}

fn value_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
}

fn parse_u128_literal(raw: &str) -> Option<u128> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        if hex.is_empty() {
            return None;
        }
        return u128::from_str_radix(hex, 16).ok();
    }
    s.parse::<u128>().ok()
}

fn value_u128(v: &Value) -> Option<u128> {
    v.as_u64()
        .map(u128::from)
        .or_else(|| v.as_str().and_then(parse_u128_literal))
}

fn private_note_balance_currency(details: &Value, currency_id: u32) -> Result<u128> {
    let balance = details
        .get("balance")
        .ok_or_else(|| anyhow!("PrivateNote.getDetails().balance is missing"))?;
    let raw = balance
        .as_object()
        .and_then(|entries| entries.get(&currency_id.to_string()))
        .or_else(|| {
            balance.as_array()?.iter().find_map(|entry| {
                let id = entry
                    .get("currency")
                    .or_else(|| entry.get("id"))
                    .and_then(value_u128)?;
                (id == u128::from(currency_id)).then(|| {
                    entry
                        .get("value")
                        .or_else(|| entry.get("amount"))
                        .unwrap_or(&Value::Null)
                })
            })
        })
        .ok_or_else(|| {
        anyhow!(
            "PrivateNote.getDetails().balance has no currency {currency_id}; refusing to infer a spendable balance"
        )
    })?;
    value_u128(raw).ok_or_else(|| {
        anyhow!("PrivateNote.getDetails().balance[{currency_id}] is not a uint128: {raw}")
    })
}

fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> &'a Value {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .unwrap_or(&Value::Null)
}

fn outcome_names_match(value: &Value, expected: &[String]) -> bool {
    if let Some(obj) = value.as_object() {
        return obj.len() == expected.len()
            && expected
                .iter()
                .enumerate()
                .all(|(i, want)| obj.get(&i.to_string()).and_then(Value::as_str) == Some(want));
    }
    if let Some(arr) = value.as_array() {
        let mut got = vec![None; expected.len()];
        let mut count = 0usize;
        for item in arr {
            if let Some(obj) = item.as_object() {
                let key = obj
                    .get("key")
                    .or_else(|| obj.get("0"))
                    .and_then(value_u64)
                    .map(|v| v as usize);
                let val = obj
                    .get("value")
                    .or_else(|| obj.get("1"))
                    .and_then(Value::as_str);
                if let (Some(k), Some(v)) = (key, val) {
                    if let Some(slot) = got.get_mut(k) {
                        if slot.is_some() {
                            return false;
                        }
                        *slot = Some(v);
                        count += 1;
                    } else {
                        return false;
                    }
                }
            }
        }
        return count == expected.len()
            && got
                .iter()
                .zip(expected)
                .all(|(got, want)| got == &Some(want.as_str()));
    }
    false
}

fn event_matches(
    event: &Value,
    event_name: &str,
    deadline: u64,
    describe: &str,
    outcome_names: &[String],
) -> bool {
    field(event, "eventName", "event_name").as_str() == Some(event_name)
        && event["describe"].as_str() == Some(describe)
        && value_u64(&event["deadline"]) == Some(deadline)
        && outcome_names_match(field(event, "outcomeNames", "outcome_names"), outcome_names)
}

fn find_event_id_in_getter_output(
    output: &Value,
    event_name: &str,
    deadline: u64,
    describe: &str,
    outcome_names: &[String],
) -> Option<String> {
    let events = output.get("_events").unwrap_or(output);
    if let Some(obj) = events.as_object() {
        for (key, event) in obj {
            if event_matches(event, event_name, deadline, describe, outcome_names) {
                if let Ok(id) = normalize_uint256_hex(key) {
                    return Some(id);
                }
            }
        }
    }
    if let Some(arr) = events.as_array() {
        for item in arr {
            if let Some(obj) = item.as_object() {
                let key = obj.get("key").or_else(|| obj.get("0"));
                let event = obj.get("value").or_else(|| obj.get("1"));
                if let (Some(key), Some(event)) = (key, event) {
                    if event_matches(event, event_name, deadline, describe, outcome_names) {
                        if let Some(id) = value_to_uint256_hex(key) {
                            return Some(id);
                        }
                    }
                }
            } else if let Some(pair) = item.as_array() {
                if pair.len() == 2
                    && event_matches(&pair[1], event_name, deadline, describe, outcome_names)
                {
                    if let Some(id) = value_to_uint256_hex(&pair[0]) {
                        return Some(id);
                    }
                }
            }
        }
    }
    None
}

fn event_from_getter_output<'a>(output: &'a Value, event_id: &str) -> Option<&'a Value> {
    let wanted = normalize_uint256_hex(event_id).ok()?;
    let events = output.get("_events").unwrap_or(output);
    if let Some(obj) = events.as_object() {
        return obj.iter().find_map(|(key, event)| {
            (normalize_uint256_hex(key).ok().as_deref() == Some(wanted.as_str())).then_some(event)
        });
    }
    events.as_array()?.iter().find_map(|item| {
        if let Some(obj) = item.as_object() {
            let key = obj.get("key").or_else(|| obj.get("0"))?;
            let event = obj.get("value").or_else(|| obj.get("1"))?;
            return (value_to_uint256_hex(key).as_deref() == Some(wanted.as_str()))
                .then_some(event);
        }
        let pair = item.as_array()?;
        (pair.len() == 2 && value_to_uint256_hex(&pair[0]).as_deref() == Some(wanted.as_str()))
            .then(|| &pair[1])
    })
}

/// `repr_hash` of a cell handed back as a base64 BOC by an ABI storage decode, as lowercase hex.

/// `None` for anything that is not a readable cell -- an absent field, an empty string, a truncated
/// BOC. Every one of those means the same thing to the caller ("the root does not carry this code"),
/// and none of them should look like a hash that merely disagrees.
fn cell_boc_repr_hash(cell_boc: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cell_boc.trim())
        .ok()?;
    let cell = tvm_types::read_single_root_boc(&bytes).ok()?;
    Some(encode_hex(cell.repr_hash().as_slice()))
}

fn account_storage_fields(account_boc: &str, abi_json: &str, contract_name: &str) -> Result<Value> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(account_boc)
        .map_err(|error| anyhow!("decode account BOC base64: {error}"))?;
    let cell = tvm_types::read_single_root_boc(&bytes)
        .map_err(|error| anyhow!("read account BOC: {error}"))?;
    let account = tvm_block::Account::construct_from_cell(cell)
        .map_err(|error| anyhow!("decode account: {error}"))?;
    let data = account
        .get_data()
        .ok_or_else(|| anyhow!("active account exposes no data cell"))?;
    let contract = tvm_abi::Contract::load(abi_json.as_bytes())
        .map_err(|error| anyhow!("load {contract_name} ABI: {error}"))?;
    let tokens = contract
        .decode_storage_fields(
            tvm_types::SliceData::load_cell(data)
                .map_err(|error| anyhow!("load account data slice: {error}"))?,
            true,
        )
        .map_err(|error| anyhow!("decode account storage: {error}"))?;
    tvm_abi::token::Detokenizer::detokenize_to_json_value(&tokens)
        .map_err(|error| anyhow!("detokenize account storage: {error}"))
}

fn oracle_event_list_storage_fields(account_boc: &str) -> Result<Value> {
    account_storage_fields(account_boc, ORACLEEVENTLIST_ABI, "OracleEventList")
}

fn oracle_pmp_confirmation_is_active(
    fields: &Value,
    pmp: &Address,
    event_id: &str,
) -> Result<bool> {
    let Some(confirmed_event) = fields["_pmpConfirmed"]
        .as_object()
        .ok_or_else(|| anyhow!("OracleEventList storage exposes no _pmpConfirmed map"))?
        .get(&format!("0x{}", pmp.bare()))
    else {
        return Ok(false);
    };
    if value_to_uint256_hex(confirmed_event).as_deref()
        != Some(normalize_uint256_hex(event_id)?.as_str())
    {
        return Err(anyhow!(
            "OracleEventList _pmpConfirmed entry for PMP {} belongs to another event",
            display_dexdo_address(pmp)
        ));
    }
    Ok(true)
}

fn validate_oracle_event_list_identity(
    fields: &Value,
    manifest: &OracleMarketManifest,
    signer: &KeyPair,
) -> Result<u128> {
    let live_oracle = fields["_oracle"]
        .as_str()
        .ok_or_else(|| anyhow!("OracleEventList storage exposes no _oracle"))?;
    if normalize_addr(live_oracle)? != normalize_addr(&manifest.oracle)? {
        return Err(anyhow!(
            "OracleEventList belongs to {live_oracle}, not manifest oracle {}",
            manifest.oracle
        ));
    }
    let index = getter_u128(fields, "_index")
        .ok_or_else(|| anyhow!("OracleEventList storage exposes no _index"))?;
    let live_key = value_to_uint256_hex(&fields["_oraclePubkey"])
        .ok_or_else(|| anyhow!("OracleEventList storage exposes no _oraclePubkey"))?;
    if live_key != normalize_uint256_hex(signer.public_hex())? {
        return Err(anyhow!(
            "oracle signer {} does not own OracleEventList",
            signer.public_hex()
        ));
    }
    Ok(index)
}

/// The identity a PMP exit actually needs: the triple the contract keys the stake by. The manifest
/// is one way to carry it and not the only one --, where the run that created a stake deleted
/// the file the only command able to cancel it demanded, leaving a live stake unmanageable by a
/// working client. `source` names where the caller got the triple so a mismatch says which side is
/// wrong.
fn validate_pmp_triple(
    details: &Value,
    event_id: &str,
    oracle_list_hash: &str,
    token_type: u32,
    source: &str,
) -> Result<()> {
    let live_event = value_to_uint256_hex(&details["eventId"])
        .ok_or_else(|| anyhow!("PMP getDetails exposes no eventId"))?;
    if live_event != normalize_uint256_hex(event_id)? {
        return Err(anyhow!("PMP eventId does not match {source}"));
    }
    let list_hash = value_to_uint256_hex(&details["oracleListHash"])
        .ok_or_else(|| anyhow!("PMP getDetails exposes no oracleListHash"))?;
    if list_hash != normalize_uint256_hex(oracle_list_hash)? {
        return Err(anyhow!("PMP oracleListHash does not match {source}"));
    }
    if getter_u128(details, "tokenType") != Some(u128::from(token_type)) {
        return Err(anyhow!("PMP tokenType does not match {source}"));
    }
    Ok(())
}

fn validate_pmp_manifest(details: &Value, manifest: &OracleMarketManifest) -> Result<()> {
    validate_pmp_triple(
        details,
        &manifest.event_id,
        &manifest.oracle_list_hash,
        manifest.token_type,
        "the manifest",
    )
}

fn pmp_deployer(details: &Value) -> Result<Address> {
    let raw = details["deployer"]
        .as_str()
        .ok_or_else(|| anyhow!("PMP getDetails exposes no deployer"))?;
    Address::parse(raw).context("PMP getDetails deployer")
}

fn validate_salted_pmp_identity(
    private_note_pin: &str,
    pmp: &Address,
    actual_pmp_code_hash: Option<&str>,
    deployer: &Address,
    deployer_account: Option<&Account>,
    pmp_code: Option<&Value>,
) -> Result<()> {
    let deployer_account = deployer_account.ok_or_else(|| {
        anyhow!(
            "PrivateNote account {} is not Active/not found (account snapshot absent)",
            display_dexdo_address(deployer)
        )
    })?;
    if deployer_account.address != *deployer {
        return Err(anyhow!(
            "PMP deployer account snapshot belongs to {} instead of {}",
            display_dexdo_address(&deployer_account.address),
            display_dexdo_address(deployer)
        ));
    }
    note_balance_private_note_account(private_note_pin, deployer, Some(deployer_account))?;
    let pmp_code = pmp_code.ok_or_else(|| {
        anyhow!(
            "PrivateNote {} getPMPCode unavailable",
            display_dexdo_address(deployer)
        )
    })?;
    let expected = value_to_uint256_hex(&pmp_code["pmpCodeHash"])
        .and_then(|hash| normalize_code_hash(&hash))
        .ok_or_else(|| {
            anyhow!(
                "PrivateNote {} getPMPCode exposes no pmpCodeHash",
                display_dexdo_address(deployer)
            )
        })?;
    let actual = actual_pmp_code_hash
        .and_then(normalize_code_hash)
        .ok_or_else(|| anyhow!("PMP {} exposes no code hash", display_dexdo_address(pmp)))?;
    if actual != expected {
        return Err(anyhow!(
            "PMP {} code hash does not match PrivateNote {} getPMPCode",
            display_dexdo_address(pmp),
            display_dexdo_address(deployer)
        ));
    }
    Ok(())
}

fn active_account_code_hash(
    contract: &str,
    address: &Address,
    account: &Account,
) -> Result<String> {
    let display_address = display_dexdo_address(address);
    if account.address != *address {
        return Err(anyhow!(
            "{contract} account snapshot belongs to {} instead of {display_address}",
            display_dexdo_address(&account.address)
        ));
    }
    if !account.is_active() {
        return Err(anyhow!("{contract} {display_address} is not Active"));
    }
    account
        .code_hash
        .as_deref()
        .and_then(normalize_code_hash)
        .ok_or_else(|| anyhow!("{contract} {display_address} exposes no valid code hash"))
}

fn active_account_code(
    contract: &str,
    address: &Address,
    account: &Account,
) -> Result<(String, tvm_types::Cell)> {
    let display_address = display_dexdo_address(address);
    let advertised_hash = active_account_code_hash(contract, address, account)?;
    let boc = account
        .boc
        .as_deref()
        .ok_or_else(|| anyhow!("{contract} {display_address} account BOC is unavailable"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(boc)
        .with_context(|| format!("decode {contract} {display_address} account BOC base64"))?;
    let root = tvm_types::read_single_root_boc(&bytes)
        .with_context(|| format!("read {contract} {display_address} account BOC"))?;
    let decoded = tvm_block::Account::construct_from_cell(root)
        .with_context(|| format!("decode {contract} {display_address} account"))?;
    let code = decoded.get_code().ok_or_else(|| {
        anyhow!("{contract} {display_address} active account BOC exposes no code")
    })?;
    let boc_hash = code.repr_hash().to_hex_string();
    if advertised_hash != boc_hash {
        return Err(anyhow!(
            "{contract} {display_address} account BOC code hash does not match its advertised code hash"
        ));
    }
    Ok((advertised_hash, code))
}

fn code_salt(contract: &str, address: &Address, code: &tvm_types::Cell) -> Result<tvm_types::Cell> {
    let display_address = display_dexdo_address(address);
    let code_boc = tvm_types::write_boc(code)
        .with_context(|| format!("serialize {contract} {display_address} code"))?;
    let context = std::sync::Arc::new(
        tvm_client::client::ClientContext::new(Default::default()).with_context(|| {
            format!("initialize TVM code-salt decoder for {contract} {display_address}")
        })?,
    );
    let result = tvm_client::boc::get_code_salt(
        context,
        tvm_client::boc::ParamsOfGetCodeSalt {
            code: base64::engine::general_purpose::STANDARD.encode(code_boc),
            boc_cache: None,
        },
    )
    .with_context(|| format!("extract {contract} {display_address} code salt"))?;
    let salt_boc = result
        .salt
        .ok_or_else(|| anyhow!("{contract} {display_address} code has no salt"))?;
    let salt_bytes = base64::engine::general_purpose::STANDARD
        .decode(salt_boc)
        .with_context(|| format!("decode {contract} {display_address} code salt BOC base64"))?;
    tvm_types::read_single_root_boc(&salt_bytes)
        .with_context(|| format!("read {contract} {display_address} code salt BOC"))
}

fn validate_salted_code_from_current_base(
    contract: &str,
    address: &Address,
    actual_hash: &str,
    actual_code: &tvm_types::Cell,
    base_tvc: &[u8],
) -> Result<tvm_types::Cell> {
    let display_address = display_dexdo_address(address);
    // The manifest used to supply an expected hash to check `base_tvc` against. Both sides come from
    // the same bytes once makes the image the source, and a comparison of a value with itself
    // is not a check -- it is a line that can only pass. What this function actually establishes is
    // below: that the live code IS the compiled base with the live salt applied.
    let base_code =
        code_cell(base_tvc).with_context(|| format!("load compiled {contract} code"))?;
    let salt = code_salt(contract, address, actual_code)?;
    let reconstructed = tvm_client::boc::set_code_salt_cell(base_code, salt.clone())
        .with_context(|| format!("apply live salt to compiled {contract} base code"))?;
    if reconstructed.repr_hash().to_hex_string() != actual_hash
        || reconstructed.repr_depth() != actual_code.repr_depth()
    {
        return Err(anyhow!(
            "{contract} {display_address} code was not produced from the current compiled base and its live salt"
        ));
    }
    Ok(salt)
}

fn decode_pmp_private_note_code(salt: tvm_types::Cell) -> Result<tvm_types::Cell> {
    let tokens = tvm_abi::TokenValue::decode_params(
        &[tvm_abi::Param::new(
            "privateNoteCode",
            tvm_abi::ParamType::Cell,
        )],
        tvm_types::SliceData::load_cell(salt).context("load PMP code salt")?,
        &tvm_abi::contract::ABI_VERSION_2_4,
        false,
    )
    .context("decode PMP code salt")?;
    tokens
        .into_iter()
        .find_map(|token| match token.value {
            tvm_abi::TokenValue::Cell(code) if token.name == "privateNoteCode" => Some(code),
            _ => None,
        })
        .ok_or_else(|| anyhow!("PMP code salt exposes no PrivateNote code"))
}

fn decode_order_book_salt(salt: tvm_types::Cell) -> Result<(tvm_types::Cell, String, u16)> {
    let tokens = tvm_abi::TokenValue::decode_params(
        &[
            tvm_abi::Param::new("privateNoteCode", tvm_abi::ParamType::Cell),
            tvm_abi::Param::new("pmpSaltedCodeHash", tvm_abi::ParamType::Uint(256)),
            tvm_abi::Param::new("pmpSaltedCodeDepth", tvm_abi::ParamType::Uint(16)),
        ],
        tvm_types::SliceData::load_cell(salt).context("load OrderBook code salt")?,
        &tvm_abi::contract::ABI_VERSION_2_4,
        false,
    )
    .context("decode OrderBook code salt")?;
    let private_note_code = tokens.iter().find_map(|token| match &token.value {
        tvm_abi::TokenValue::Cell(code) if token.name == "privateNoteCode" => Some(code.clone()),
        _ => None,
    });
    let pmp_hash = tokens.iter().find_map(|token| match &token.value {
        tvm_abi::TokenValue::Uint(value) if token.name == "pmpSaltedCodeHash" => {
            normalize_code_hash(&value.number.to_str_radix(16))
        }
        _ => None,
    });
    let pmp_depth = tokens.iter().find_map(|token| match &token.value {
        tvm_abi::TokenValue::Uint(value) if token.name == "pmpSaltedCodeDepth" => {
            value.number.to_string().parse::<u16>().ok()
        }
        _ => None,
    });
    Ok((
        private_note_code
            .ok_or_else(|| anyhow!("OrderBook code salt exposes no PrivateNote code"))?,
        pmp_hash.ok_or_else(|| anyhow!("OrderBook code salt exposes no PMP code hash"))?,
        pmp_depth.ok_or_else(|| anyhow!("OrderBook code salt exposes no PMP code depth"))?,
    ))
}

/// The `PrivateNote` generation a container (PMP, OrderBook) mints against, held to the generation
/// THIS NETWORK runs.

/// The expected value is the network's own pin, not the image this build compiles, and the
/// difference is load-bearing: the pin is what somebody read off that chain, while the image is what
/// this tree happens to have vendored. They coincide while a chain runs this build's generation and
/// part company during a staged rollout -- and during one, a container on the chain that has not
/// moved yet is correct, not stale. Answering from the image would refuse it.
fn validate_private_note_generation(
    expected: &str,
    container: &str,
    private_note_code: &tvm_types::Cell,
) -> Result<String> {
    let actual = private_note_code.repr_hash().to_hex_string();
    if actual != expected {
        return Err(anyhow!(
            "{container} code salt embeds a different PrivateNote generation"
        ));
    }
    Ok(actual)
}

fn getter_code_hash(details: &Value, field: &str) -> Option<String> {
    value_to_uint256_hex(&details[field]).and_then(|hash| normalize_code_hash(&hash))
}

fn validate_order_book_market_identity(pmp: &Value, order_book: &Value) -> Result<()> {
    let pmp_event = value_to_uint256_hex(&pmp["eventId"])
        .ok_or_else(|| anyhow!("PMP getDetails exposes no eventId"))?;
    let book_event = value_to_uint256_hex(&order_book["eventId"])
        .ok_or_else(|| anyhow!("OrderBook getDetails exposes no eventId"))?;
    let pmp_oracles = value_to_uint256_hex(&pmp["oracleListHash"])
        .ok_or_else(|| anyhow!("PMP getDetails exposes no oracleListHash"))?;
    let book_oracles = value_to_uint256_hex(&order_book["oracleListHash"])
        .ok_or_else(|| anyhow!("OrderBook getDetails exposes no oracleListHash"))?;
    if pmp_event != book_event
        || pmp_oracles != book_oracles
        || getter_u128(pmp, "tokenType") != getter_u128(order_book, "tokenType")
        || getter_u128(pmp, "tokenType").is_none()
    {
        return Err(anyhow!(
            "OrderBook getDetails market identity does not match its PMP"
        ));
    }
    Ok(())
}

fn validate_oracle_event_manifest(
    event: &Value,
    range: &Value,
    manifest: &OracleMarketManifest,
) -> Result<()> {
    if event["eventName"].as_str() != Some(manifest.event_name.as_str())
        || getter_u128(event, "deadline") != Some(u128::from(manifest.deadline))
        || !outcome_names_match(&event["outcomeNames"], &manifest.outcome_names)
    {
        return Err(anyhow!(
            "OracleEventList event identity does not match the manifest"
        ));
    }
    if getter_bool(range, "exists") != Some(true)
        || range["ob"].as_str().map(normalize_addr).transpose()?
            != Some(normalize_addr(&manifest.inference_order_book)?)
        || range_bounds_to_uint256_hex(&range["bounds"])
            != Some(requested_bounds_to_uint256_hex(&manifest.bounds)?)
    {
        return Err(anyhow!(
            "OracleEventList range identity does not match the manifest"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod oracle_getter_tests {
    use super::*;

    #[test]
    fn normalizes_uint256_getter_shapes() {
        assert_eq!(
            normalize_uint256_hex("15").unwrap(),
            "0x000000000000000000000000000000000000000000000000000000000000000f"
        );
        assert_eq!(
            normalize_uint256_hex("0xabc").unwrap(),
            "0x0000000000000000000000000000000000000000000000000000000000000abc"
        );
        assert!(normalize_uint256_hex("0xnothex").is_err());
    }

    #[test]
    fn parses_u128_getter_numbers_and_hex_strings() {
        assert_eq!(value_u128(&json!(10_000u64)), Some(10_000));
        assert_eq!(value_u128(&json!("10000")), Some(10_000));
        assert_eq!(
            value_u128(&json!(
                "0x0000000000000000000000000000000000000000000000000000000000002710"
            )),
            Some(10_000)
        );
        assert_eq!(value_u128(&json!("0xnothex")), None);
    }

    #[test]
    fn normalizes_range_bounds_for_idempotent_event_checks() {
        let live = json!(["0x0000000000000000000000000000000000000000000000000000000000002711"]);
        assert_eq!(
            range_bounds_to_uint256_hex(&live).unwrap(),
            requested_bounds_to_uint256_hex(&["10001".to_string()]).unwrap()
        );
        assert_ne!(
            range_bounds_to_uint256_hex(&live).unwrap(),
            requested_bounds_to_uint256_hex(&["10002".to_string()]).unwrap()
        );
        assert!(range_bounds_to_uint256_hex(&json!(["0xnothex"])).is_none());
    }

    #[test]
    fn finds_range_event_from_legacy_and_snake_getters() {
        let outcomes = vec!["below".to_string(), "above".to_string()];
        let legacy = json!({
            "_events": {
                "15": {
                    "eventName": "weekly",
                    "deadline": "1900000000",
                    "describe": "qwen",
                    "outcomeNames": {"0": "below", "1": "above"}
                }
            }
        });
        assert_eq!(
            find_event_id_in_getter_output(&legacy, "weekly", 1_900_000_000, "qwen", &outcomes)
                .unwrap(),
            "0x000000000000000000000000000000000000000000000000000000000000000f"
        );

        let snake = json!({
            "_events": [{
                "key": "0x10",
                "value": {
                    "event_name": "weekly",
                    "deadline": 1900000000u64,
                    "describe": "qwen",
                    "outcome_names": [
                        {"key": 0, "value": "below"},
                        {"key": 1, "value": "above"}
                    ]
                }
            }]
        });
        assert_eq!(
            find_event_id_in_getter_output(&snake, "weekly", 1_900_000_000, "qwen", &outcomes)
                .unwrap(),
            "0x0000000000000000000000000000000000000000000000000000000000000010"
        );
    }

    #[test]
    fn rejects_sparse_or_extra_outcome_getters() {
        let outcomes = vec!["below".to_string(), "above".to_string()];
        let extra = json!({
            "_events": {
                "15": {
                    "eventName": "weekly",
                    "deadline": "1900000000",
                    "describe": "qwen",
                    "outcomeNames": {"0": "below", "1": "above", "2": "extra"}
                }
            }
        });
        assert!(
            find_event_id_in_getter_output(&extra, "weekly", 1_900_000_000, "qwen", &outcomes)
                .is_none()
        );

        let sparse = json!({
            "_events": [{
                "key": "15",
                "value": {
                    "eventName": "weekly",
                    "deadline": "1900000000",
                    "describe": "qwen",
                    "outcomeNames": [
                        {"key": 0, "value": "below"},
                        {"key": 2, "value": "above"}
                    ]
                }
            }]
        });
        assert!(find_event_id_in_getter_output(
            &sparse,
            "weekly",
            1_900_000_000,
            "qwen",
            &outcomes
        )
        .is_none());
    }

    #[test]
    fn finds_exact_event_by_normalized_id() {
        let output = json!({
            "_events": [{
                "key": "15",
                "value": {"eventName": "weekly", "count": "2"}
            }]
        });
        let event = event_from_getter_output(&output, "0x0f").expect("event by id");
        assert_eq!(event["eventName"], "weekly");
        assert_eq!(getter_u128(event, "count"), Some(2));
        assert!(event_from_getter_output(&output, "0x10").is_none());
    }

    #[test]
    fn finds_only_the_exact_pmp_confirmation_in_oel_storage() {
        let pmp = Address::parse(&format!("0:{}", "1".repeat(64))).unwrap();
        let other_pmp = Address::parse(&format!("0:{}", "2".repeat(64))).unwrap();
        let mut confirmations = serde_json::Map::new();
        confirmations.insert(format!("0x{}", pmp.bare()), json!("0x16"));
        let fields = json!({"_pmpConfirmed": confirmations});

        assert!(oracle_pmp_confirmation_is_active(&fields, &pmp, "0x16").unwrap());
        assert!(!oracle_pmp_confirmation_is_active(&fields, &other_pmp, "0x16").unwrap());
        assert!(oracle_pmp_confirmation_is_active(&fields, &pmp, "0x17").is_err());
    }

    #[test]
    fn oracle_identity_validators_reject_wrong_signer_and_manifest() {
        let addr = |digit: char| format!("0:{}", digit.to_string().repeat(64));
        let manifest = OracleMarketManifest {
            network: "net-a".into(),
            root_oracle: addr('1'),
            oracle: addr('2'),
            oracle_event_list: addr('3'),
            oracle_list_hash: "0x15".into(),
            event_id: "0x16".into(),
            event_name: "event".into(),
            pmp: addr('4'),
            token_type: 1,
            inference_order_book: addr('5'),
            frame_model: "model".into(),
            deadline: 1_000,
            bounds: vec!["10".into()],
            outcome_names: vec!["below".into(), "above".into()],
        };
        let signer = KeyPair::from_secret_hex(&"22".repeat(32)).unwrap();
        let fields = json!({
            "_oracle": manifest.oracle,
            "_index": "7",
            "_oraclePubkey": signer.public_hex(),
        });
        assert_eq!(
            validate_oracle_event_list_identity(&fields, &manifest, &signer).unwrap(),
            7
        );
        assert!(validate_oracle_event_list_identity(
            &fields,
            &manifest,
            &KeyPair::from_secret_hex(&"33".repeat(32)).unwrap()
        )
        .is_err());

        let details = json!({
            "eventId": manifest.event_id,
            "oracleListHash": manifest.oracle_list_hash,
            "tokenType": "1",
        });
        assert!(validate_pmp_manifest(&details, &manifest).is_ok());
        assert!(validate_pmp_manifest(
            &json!({"eventId": "0x17", "oracleListHash": "0x15", "tokenType": "1"}),
            &manifest
        )
        .is_err());

        let event = json!({
            "eventName": manifest.event_name,
            "deadline": "1000",
            "outcomeNames": {"0": "below", "1": "above"},
        });
        let range = json!({"exists": true, "ob": manifest.inference_order_book, "bounds": ["10"]});
        assert!(validate_oracle_event_manifest(&event, &range, &manifest).is_ok());
        assert!(validate_oracle_event_manifest(
            &event,
            &json!({"exists": true, "ob": addr('6'), "bounds": ["10"]}),
            &manifest
        )
        .is_err());
    }

    #[test]
    fn salted_pmp_identity_validator_is_fail_closed() {
        const SALTED: &str = "893599247dee107d493507399985a0bb5a4396580b8693f03f62aa36a25737f3";
        const BASE: &str = "fbc1fb4fa83a623bed6f224ba9d9d0f0904012f5f98a94937ea79ff27ce679fb";

        let pmp = Address::parse(&format!("0:{}", "1".repeat(64))).unwrap();
        let deployer = Address::parse(&format!("0:{}", "2".repeat(64))).unwrap();
        let other = Address::parse(&format!("0:{}", "3".repeat(64))).unwrap();
        let cases = [
            ("canonical salted", 0),
            ("unsalted base", 1),
            ("wrong salt", 2),
            ("wrong deployer", 3),
            ("inactive deployer", 4),
            ("non-PrivateNote", 5),
            ("missing getter", 6),
        ];

        for (name, case) in cases {
            let mut actual = SALTED;
            let mut account_address = &deployer;
            let mut status = "Active";
            let mut code_hash = PRIVATENOTE_PINNED_CODE_HASH;
            let mut getter = Some(SALTED);
            match case {
                1 => actual = BASE,
                2 => getter = Some(BASE),
                3 => account_address = &other,
                4 => {
                    status = "Uninit";
                    getter = None;
                }
                5 => code_hash = BASE,
                6 => getter = None,
                _ => {}
            }
            let account = Account {
                address: account_address.clone(),
                status: status.into(),
                balance: 0,
                ecc: Vec::new(),
                code_hash: Some(code_hash.into()),
                boc: None,
            };
            let getter = getter.map(|hash| json!({"pmpCodeHash": format!("0x{hash}")}));
            let result = validate_salted_pmp_identity(
                PRIVATENOTE_PINNED_CODE_HASH,
                &pmp,
                Some(actual),
                &deployer,
                Some(&account),
                getter.as_ref(),
            );
            assert_eq!(result.is_ok(), case == 0, "{name}");
        }
    }
}

/// Manifest of the deployed contracts.
/// The address source for the adapter and e2e. `InferenceOrderBook` (per-model) and
/// `TokenContract` (per-deal) are derived/discovered on the fly, so they are not pinned here.
#[derive(Debug, Clone, Deserialize)]
pub struct Deployed {
    /// Contract generation declared by the deployment manifest.
    #[serde(default)]
    pub version: Option<String>,
    /// Network label, as the manifest declares it.
    pub network: String,
    /// `SuperRoot` airegistry -- the derivation point for `RootModel`/`InferenceOrderBook`.
    pub superroot: String,
    /// `DappConfig` (a DApp with unlimited credit for deploys).
    pub dapp_config: String,
    /// `dapp_id` (= account_id of `SuperRoot`).
    pub dapp_id: String,
    /// Optional Block Manager endpoint. `graphql` is accepted for deployed-manifest compatibility.
    #[serde(default, alias = "graphql")]
    pub endpoint: Option<String>,
    /// How many chain requests a second this chain tolerates from us, if it says.

    /// Absent means no ceiling of ours -- which is a claim about THIS chain, made by the document
    /// that describes it, not a default the client picked. The production manifest carries 3: that
    /// chain answers a burst with `pool timed out while waiting for an open connection` at HTTP 200,
    /// and a retry on top of a self-inflicted overload makes the overload worse.
    #[serde(default)]
    pub requests_per_second: Option<u32>,
    /// Where an operator gets a Gosh.ai wallet for THIS deployment, if they can get one at all.

    /// Absent means Gosh.ai issues no wallets here, and `wallet onboard gosh-ai` refuses instead of
    /// showing a link that cannot work -- which matters because the next thing that flow asks for
    /// is a recovery phrase. Same reasoning as `requests_per_second` above: a claim about THIS
    /// chain, made by the document that describes it, not a default the client picked.

    /// It lives here rather than in a constant because took the client's opinions about
    /// networks away, and "does this network have Gosh.ai" is such an opinion. Keeping the URL here
    /// also means changing it is a manifest edit and not a release.
    #[serde(default)]
    pub goshai_onboarding_url: Option<String>,
}

impl Deployed {
    /// Read the manifest at the path the caller was given. Exactly that path, always.


    /// `params::manifest_path`: `DEXDO_MANIFEST` when set, otherwise the sole per-user default.
    /// Everything this function can do when the file is not there is say WHICH file, because that
    /// is the operator's next move.

    /// What used to be here, and why it is gone: a missing default path fell back to
    /// scanning `manifest/` for a single `deployed.*.json`, and before that to a copy of the
    /// a manifest for one network compiled into the binary. Both were a network chosen without being asked
    /// for. Measured on 2026-08-25: an operator whose wallet is bound on MAINNET ran `note deploy`
    /// in a directory dedicated to mainnet, was told "no wallet is bound on this network yet", and watched
    /// a 750-second onboarding wait for a QR nobody needed to scan. The binding was there. The
    /// network came from the fallback.

    /// The `io::Error` stays in the chain rather than being formatted into text: `doctor` asks
    /// whether the cause was `NotFound` and answers with the actionable refusal, and a flattened
    /// cause turns that question false -- the operator then gets "No such file or directory" with
    /// no fix in it.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            anyhow::bail!(
                "the deployment manifest path must name a FILE, but {} is a directory",
                path.display()
            );
        }
        let bytes = std::fs::read(path).map_err(|error| {
            let missing = error.kind() == std::io::ErrorKind::NotFound;
            let refusal = anyhow::Error::new(error)
                .context(format!("read the deployment manifest {}", path.display()));
            if !missing {
                return refusal;
            }
            refusal.context(format!(
                "{} names no file at that path. It is what says which network this client talks \
                 to, which contracts it addresses and where to dial, and nothing is assumed in \
                 its place -- point it at a manifest that exists.",
                crate::params::MANIFEST_PATH_VAR,
            ))
        })?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Checks that the supplied manifest describes the generation read from the chain, and that each
    /// of its contract pins names the compiled artifact this binary carries.

    /// The pin half used to compare the manifest against the manifest embedded in the binary,
    /// and on an installed machine [`Deployed::load`] returns exactly that constant, so the two
    /// sides were one value read twice.
    pub fn validate(&self, live_versions: &[(String, String)]) -> Vec<ChainDoctorCheck> {
        let live_generations = live_versions
            .iter()
            .filter_map(|(_, version)| version.split_whitespace().next())
            .filter(|generation| !generation.is_empty())
            .collect::<BTreeSet<_>>();
        let chain_generation = live_generations
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .join(",");
        let manifest_generation = self
            .version
            .as_deref()
            .map(str::trim)
            .filter(|generation| !generation.is_empty());
        let generation_matches = live_generations.len() == 1
            && manifest_generation.is_some_and(|generation| generation == chain_generation);
        let generation_message = match (manifest_generation, live_generations.len()) {
            (Some(manifest), 1) if generation_matches => {
                format!("deployed manifest and live chain both report generation {manifest}")
            }
            (Some(manifest), 1) => format!(
                "deployed manifest generation {manifest} does not match live chain generation {chain_generation}"
            ),
            (None, 1) => format!(
                "deployed manifest has no generation, while the live chain reports {chain_generation}"
            ),
            (Some(manifest), 0) => format!(
                "live chain exposed no contract generation to compare with deployed manifest generation {manifest}"
            ),
            (None, 0) => {
                "deployed manifest has no generation and the live chain exposed none".to_string()
            }
            (Some(manifest), _) => format!(
                "live contracts report multiple generations ({chain_generation}); deployed manifest reports {manifest}"
            ),
            (None, _) => format!(
                "live contracts report multiple generations ({chain_generation}); deployed manifest has no generation"
            ),
        };
        let checks = vec![ChainDoctorCheck {
            name: "deployed manifest generation".to_string(),
            status: if generation_matches {
                ChainDoctorStatus::Pass
            } else {
                ChainDoctorStatus::Fail
            },
            address: None,
            expected: Some(format!(
                "chain={}",
                if chain_generation.is_empty() {
                    "<missing>"
                } else {
                    &chain_generation
                }
            )),
            actual: Some(format!(
                "manifest={}",
                manifest_generation.unwrap_or("<missing>")
            )),
            message: generation_message,
        }];

        checks
    }
}

/// Normalize a Block Manager host or URL to the base used by GraphQL and REST reads.
pub fn normalize_endpoint(endpoint: &str) -> anyhow::Result<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        anyhow::bail!("endpoint must not be empty");
    }
    let endpoint = endpoint.strip_suffix("/graphql").unwrap_or(endpoint);
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        Ok(endpoint.to_string())
    } else {
        Ok(format!("https://{endpoint}"))
    }
}

pub fn endpoint_urls(endpoint: &str) -> anyhow::Result<(String, String)> {
    let endpoint = normalize_endpoint(endpoint)?;
    Ok((
        format!("{endpoint}/graphql"),
        format!("{endpoint}/v2/account"),
    ))
}

/// The Block Manager this run dials, in descending order of authority: an explicit `--endpoint`,
/// then the manifest's own `endpoint` field, and only then the default the manifest's network LABEL
/// implies.

/// **The label is never itself an address.** It reaches this function only as the key of
/// that last lookup, so a manifest naming a network this build does not know is refused with the
/// networks it does know, rather than having a hostname assembled out of its name. The default used
/// to be one chain's constant whatever the label said, which is the same substitution seen from the
/// other side: a mainnet manifest that carried no `endpoint` of its own silently resolved to a
/// that chain's host.
pub fn resolve_endpoint(explicit: Option<&str>, manifest: &Deployed) -> anyhow::Result<String> {
    if let Some(endpoint) = explicit.or(manifest.endpoint.as_deref()) {
        return normalize_endpoint(endpoint);
    }
    anyhow::bail!(
        "the manifest declares network `{}` and carries no `endpoint`, so nothing says where to \
         dial. There is no table of known networks to fall back on and no default host: a network \
         NAME is not an address, and assembling one out of it is how a mainnet manifest ended up \
         answering from a test chain. Add an `endpoint` field to the manifest.",
        manifest.network
    )
}

/// The SDK profile this client uses, for every chain.

/// **One profile, chosen by nothing.** This used to `match` the manifest's label onto
/// one of two per-chain SDK presets, and refuse any other label -- so the client held
/// a list of chains it would work on, and a manifest naming a new one was rejected for no reason
/// except that this binary predated it.

/// Measured before the change, in the SDK the two presets come from: they differ ONLY in the giver
/// fields. one preset is literally the other with `giver_* = None`, and the code hashes --
/// the part that actually locks the vendored artifacts -- are identical. So the `match` selected
/// between "with the test faucet compiled in" and "without", not between two chains.

/// `custom()` is the neutral one: same embedded-artifact code hashes, and every network-specific
/// field failing CLOSED -- no SuperRoot, no giver. That is what this client wants everywhere,
/// because the giver is network-agnostic (one address, one ABI) and arrives from the environment
/// under `DEV`, never from a preset.

/// What it does NOT do is keep the faucet's key out of the shipped binary, and an earlier draft of
/// this comment claimed it did. Measured on the release artifact: the preset's secret and public
/// key each appear ONCE in `strings`, with a working control (`DEXDO_MANIFEST` appears 17 times).
/// They live in the pinned SDK's own constructor and survive as `.rodata` whether or not anything
/// calls it; no code in this repository can remove them. What this change buys is narrower and
/// still worth having: no path in this client READS them, so a run cannot spend from the faucet
/// because of which preset it happened to select. Removing the strings is the SDK's to do.
fn ai_registry_config_from_manifest(manifest: &Deployed) -> anyhow::Result<AiRegistryConfig> {
    if manifest.network.trim().is_empty() {
        anyhow::bail!(
            "the manifest's `network` field is empty, so nothing names the chain this run is on. \
             It keys the wallet binding and the generation pins, and an empty label cannot be told \
             apart from any other."
        );
    }
    Ok(AiRegistryConfig::custom())
}

/// Is this failure the HTTPS client refusing to be assembled, rather than anything else the
/// connector does?

/// Matched on text because nothing else is left to match on. `tvm_client` renders its typed
/// `HttpClientCreateError` through `Display` into a `String` (`client/errors.rs`,
/// `http_client_create_error<E: Display>`), so by the time the error has crossed two dependencies
/// into `anyhow` it carries no code, no source and no path -- only a sentence.

/// A marker that stops matching after an SDK bump makes the caller fall back to the underlying text
/// unchanged. That is a return to today's behaviour, not a wrong claim, and it is the only direction
/// in which a guess is allowed to fail here.
fn is_https_client_build_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let text = cause.to_string().to_ascii_lowercase();
        text.contains("create http client") || text.contains("builder error")
    })
}

/// The refusal an operator sees when the HTTPS client cannot be assembled.

/// It lists what can produce the failure and never asserts which one it is. Measured 2026-08-27:
/// four distinct causes yield the identical underlying sentence -- an absent root-certificate store,
/// and `SSL_CERT_FILE` / `SSL_CERT_DIR` naming a missing path or a file holding no certificate.
/// Three of those four had a complete certificate store installed, so a refusal that named
/// certificates as THE cause would have been wrong three times out of four. Replacing one
/// uninformative sentence with a confident wrong one is not an improvement.

/// The underlying text is kept verbatim rather than replaced: it is the only evidence that survived
/// the two layers below, and a reader who recognises it should still find it here.
fn https_client_refusal(underlying: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "could not build the HTTPS client, so no endpoint was contacted and nothing was sent: \
         {underlying}\n\
         This step only assembles the client and makes no network call, so an unreachable endpoint \
         or a chain that is down is not the cause. What can produce it, each one checkable:\n\
         \x20 - there is no root-certificate store to read: /etc/ssl/certs is the usual location \
         on Linux, and `ca-certificates` is the package that fills it\n\
         \x20 - SSL_CERT_FILE or SSL_CERT_DIR is set and names a path that does not exist, or a \
         file that holds no certificate\n\
         The library underneath reports the same sentence for every one of these and discards its \
         own cause, so this names what it can be rather than which it is."
    )
}

fn connect_client_from_manifest_with<T>(
    manifest_path: impl AsRef<Path>,
    endpoint_override: Option<&str>,
    connect: impl FnOnce(&str, AiRegistryConfig) -> anyhow::Result<T>,
) -> anyhow::Result<(Deployed, T)> {
    let manifest_path = manifest_path.as_ref();
    let deployed = Deployed::load(manifest_path)?;
    let config = ai_registry_config_from_manifest(&deployed)?;
    let endpoint = resolve_endpoint(endpoint_override, &deployed)?;
    // the declared `network` is only a string until it is checked against the chain actually
    // being dialled. Both are known here, and this sits ahead of the connector, so a contradiction
    // costs no chain traffic.

    // What that covers, exactly: every `RealChainBackend::connect*`, because each one passes through
    // this function. What it does NOT cover, and what the earlier wording here claimed it did
    // the production sites that reach a chain through `ChainClient::connect` with an
    // endpoint and no manifest. There is no declared network there, so there is nothing to check the
    // endpoint against. That set is frozen by name in
    // `crates/dexdo/src/cli/network_check_reach_1613.rs`, so it cannot grow unnoticed while it is
    // being closed.
    // No declared-network-against-endpoint check any more, and its absence is the point. Both
    // facts now come out of ONE file: `deployed.network` is that file's label and `endpoint` is
    // resolved from that same file. Comparing them compared the manifest with itself.

    // The check was written for `--endpoint`, which could name a host from another chain
    // while the manifest declared this one. That flag is gone, and with it the second
    // source the check existed to catch. Keeping it would also mean keeping a table of hosts to
    // recognise, which is the client having an opinion about which chains exist.

    // the connector can fail for reasons that have nothing to do with TLS -- a bad endpoint,
    // a config the SDK rejects -- so the guidance is attached only when the failure IS the client
    // refusing to be built. Anything else is passed through exactly as it arrived.
    let client = connect(&endpoint, config).map_err(|error| {
        if is_https_client_build_failure(&error) {
            https_client_refusal(error)
        } else {
            error
        }
    })?;
    Ok((deployed, client))
}

#[derive(Clone, Copy, Default)]
struct ReadFailureFacts {
    status: Option<reqwest::StatusCode>,
    edge_client_signature_ban: bool,
    connect: bool,
    timeout: bool,
    body: bool,
    decode: bool,
}

impl ReadFailureFacts {
    fn status(status: reqwest::StatusCode) -> Self {
        Self {
            status: Some(status),
            edge_client_signature_ban: false,
            connect: false,
            timeout: false,
            body: false,
            decode: false,
        }
    }

    fn reqwest(error: &reqwest::Error) -> Self {
        Self {
            status: error.status(),
            edge_client_signature_ban: false,
            connect: error.is_connect(),
            timeout: error.is_timeout(),
            body: error.is_body(),
            decode: error.is_decode(),
        }
    }
}

/// A non-success response whose 403 discriminator survived body consumption.
#[derive(Debug)]
pub(super) struct ChainHttpResponseError {
    url: String,
    cf_ray: bool,
    error_code_1010: bool,
    retry_after_seconds: Option<u64>,
    body_read_error: Option<reqwest::Error>,
}

fn content_type_is_plain_text(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/plain"))
}

impl ChainHttpResponseError {
    pub(super) fn forbidden(
        url: impl Into<String>,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> Self {
        Self {
            url: url.into(),
            cf_ray: headers.contains_key("cf-ray"),
            error_code_1010: content_type_is_plain_text(headers)
                && body.to_ascii_lowercase().contains("error code: 1010"),
            retry_after_seconds: headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok()),
            body_read_error: None,
        }
    }

    fn facts(&self) -> ReadFailureFacts {
        let mut facts = ReadFailureFacts::status(reqwest::StatusCode::FORBIDDEN);
        facts.edge_client_signature_ban = self.error_code_1010;
        facts
    }
}

impl std::fmt::Display for ChainHttpResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.error_code_1010 {
            let evidence = if self.cf_ray {
                "Cloudflare error code 1010; cf-ray header present"
            } else {
                "Cloudflare error code 1010"
            };
            write!(
                f,
                "chain HTTP 403 from {}: this client's HTTP signature is banned at the \
                 Cloudflare edge ({evidence}); use a different HTTP client, not a longer retry",
                self.url
            )
        } else {
            write!(
                f,
                "{} HTTP 403 Forbidden for {}",
                crate::params::current_network(),
                self.url
            )
        }
    }
}

impl std::error::Error for ChainHttpResponseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.body_read_error
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

async fn chain_response_for_status(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status() != reqwest::StatusCode::FORBIDDEN {
        return Ok(response.error_for_status()?);
    }

    let url = response.url().to_string();
    let headers = response.headers().clone();
    let (body, body_read_error) = if content_type_is_plain_text(&headers) {
        match response.text().await {
            Ok(body) => (body, None),
            Err(error) => (String::new(), Some(error)),
        }
    } else {
        (String::new(), None)
    };
    let mut error = ChainHttpResponseError::forbidden(url, &headers, &body);
    error.body_read_error = body_read_error;
    Err(anyhow::Error::new(error))
}

/// The one retry decision for chain transport failures.
fn read_failure_is_transient(facts: ReadFailureFacts) -> bool {
    !facts.edge_client_signature_ban
        && (facts.connect
            || facts.timeout
            || facts.body
            || facts.decode
            || facts.status.is_some_and(|status| {
                status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::FORBIDDEN
                    || status.is_server_error()
            }))
}

/// Compatibility wrapper for status-only callers and tests. The decision remains above.
#[cfg(test)]
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    read_failure_is_transient(ReadFailureFacts::status(status))
}

pub(super) fn reqwest_error_is_transient(error: &reqwest::Error) -> bool {
    read_failure_is_transient(ReadFailureFacts::reqwest(error))
}

pub(super) fn is_transient_transport_failure(error: &anyhow::Error) -> bool {
    if let Some(response) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ChainHttpResponseError>())
    {
        return read_failure_is_transient(response.facts());
    }
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest_error_is_transient)
    })
}

/// Transient for a READ, which is a strictly wider question than transient for a submit.

/// this exists so the GraphQL-body case can be honoured WITHOUT widening
/// [`is_transient_transport_failure`], which also feeds `is_transient_submit_failure` and therefore
/// the money-submit retry. Repeating a read is safe because nothing was sent; repeating a submit is
/// not the same question and is deliberately left exactly as it was.
/// Whether a failed chain READ is one that retrying can still fix.

/// Public for one reason, and it is worth stating so nobody widens it by accident: a caller that
/// turns a read failure into an operator-facing refusal has to say whether the operator should try
/// again, and it must answer that with the SAME test the retry loop already applied -- otherwise the
/// client's own two answers disagree. `retry_transient_read` uses this predicate to decide whether
/// to keep going; `dexdo note deploy` uses it to decide which failure it is reporting.

/// Answer it with this one and not with [`is_transient_transport_failure`]: that inner test misses
/// GraphQL pool exhaustion, which is how a rate-limited endpoint reports throttling, and calling
/// throttling permanent is exactly the error exists to stop.

/// This is NOT a judgement about submits, money, or contract results -- only about a read that came
/// back without an answer.
pub fn is_transient_read_failure(error: &anyhow::Error) -> bool {
    if is_transient_transport_failure(error) {
        return true;
    }
    if read_request_got_no_response(error) {
        return true;
    }
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<GraphQlBodyError>())
        .is_some_and(GraphQlBodyError::is_pool_exhaustion)
}

/// A request that ended before any response existed: the connection was reset, closed or broken
/// while the request was going out.

/// `reqwest` calls this `Kind::Request`, and none of the facts
/// [`ReadFailureFacts::reqwest`] collects covers it. `is_connect` answers `true` only for
/// `hyper_util`'s `ErrorKind::Connect`, and a connection reset in flight is `ErrorKind::SendRequest`
/// -- so a reset read was permanent to every retry this client has, measured on the endpoint the
/// buy path dials.

/// For a READ this is the plainest transient failure there is: no answer came back, so repeating the
/// request asks the same question again. It is deliberately NOT part of
/// [`is_transient_transport_failure`], which feeds the money-submit retry: a submit reset in flight
/// may already have reached the chain, and repeating it is a second spend, not a second read.
fn read_request_got_no_response(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_request)
    })
}

/// What the server asked us to wait, when it asked. Honouring it is the difference between backing
/// off and making the limit worse; ignoring a stated `Retry-After` and retrying on our own schedule
/// is what turns one rate-limited read into five.
fn retry_after_delay(error: &anyhow::Error) -> Option<std::time::Duration> {
    let seconds: u64 = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<RetryAfter>()
            .map(|retry_after| retry_after.seconds)
            .or_else(|| {
                cause
                    .downcast_ref::<ChainHttpResponseError>()
                    .and_then(|response| response.retry_after_seconds)
            })
    })?;
    let asked = std::time::Duration::from_secs(seconds);
    (asked <= crate::params::TRANSIENT_READ_MAX_RETRY_AFTER).then_some(asked)
}

/// A `Retry-After` seen on a rate-limited response, carried in the error chain so the retry loop
/// can read it without re-issuing the request.
#[derive(Debug)]
pub struct RetryAfter {
    pub seconds: u64,
}

impl std::fmt::Display for RetryAfter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server asked to retry after {}s", self.seconds)
    }
}

impl std::error::Error for RetryAfter {}

/// Chain reads, retried when they got no answer.

/// One policy for every read, whichever way it leaves the process -- the SDK's getter, our own
/// `reqwest` client for event pages and chain time, or the liveness probe. Keeping them separate is
/// how the first version of this missed `dexdo doctor`: it went out through a path the wrapper did
/// not cover, and a run stopped on exactly the failure the fix was written for.

/// Bounded twice, because the SDK's HTTP client carries no timeout of its own: each attempt has a
/// ceiling, and the whole call has a budget. Without both, a server that accepts and never answers
/// keeps attempt one alive forever and attempt two never happens.

/// Retrying a read is safe because it is a read: nothing was submitted, so nothing can be submitted
/// twice. That is why this wraps reads and not the calls that move money.
pub async fn retry_transient_read<T, F, Fut>(mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let started = tokio::time::Instant::now();
    let budget = crate::params::TRANSIENT_READ_TOTAL_BUDGET;
    let mut delay = crate::params::TRANSIENT_READ_INITIAL_BACKOFF;

    for attempt in 1..=crate::params::TRANSIENT_READ_ATTEMPTS {
        let attempt_result =
            tokio::time::timeout(crate::params::TRANSIENT_READ_ATTEMPT_TIMEOUT, call()).await;
        let error = match attempt_result {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(error)) => {
                if !is_transient_read_failure(&error) {
                    return Err(error);
                }
                error
            }
            Err(_) => anyhow::anyhow!(
                "chain read exceeded {:?} on attempt {attempt}",
                crate::params::TRANSIENT_READ_ATTEMPT_TIMEOUT
            ),
        };

        let wait = retry_after_delay(&error).unwrap_or(delay);
        let spent = started.elapsed();
        if attempt == crate::params::TRANSIENT_READ_ATTEMPTS || spent + wait >= budget {
            return Err(error.context(format!(
                "{}{attempt} attempt(s) in {spent:?}",
                crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX
            )));
        }
        tokio::time::sleep(wait).await;
        delay = (delay * 2).min(crate::params::TRANSIENT_READ_MAX_BACKOFF);
    }
    unreachable!("the loop returns on the last attempt")
}

/// How many chain requests a second a network tolerates from us.

/// Two explicit cases rather than `Option<u32>`, because `None` reads as "not configured" and that
/// is a different claim from "this network has no ceiling". The network is always known by the time
/// this is built, so "unset" is not a state that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainRequestCeiling {
    /// The network is known and applies no ceiling of ours.
    Unlimited,
    /// At most this many admissions in any one-second window.
    PerSecond(u32),
}

impl ChainRequestCeiling {
    /// The ceiling this manifest declares, or none.

    /// This was `for_network(&str)`, a `match` on the label with the production network's figure
    /// compiled in beside it. The figure is right and the reason for it is right -- a burst answered
    /// with `pool timed out while waiting for an open connection` at HTTP 200, and a retry on top of
    /// a self-inflicted overload makes the overload worse -- but a ceiling is a property of the CHAIN
    /// being dialled, and the manifest is the document that describes that chain. Reading it there
    /// puts it beside `endpoint` and `indexer`, where every other per-chain fact already lives, and
    /// costs the client its last opinion about which chains exist.

    /// Behaviour is unchanged: the production manifest carries `"requests_per_second": 3`, and a
    /// manifest naming no ceiling gets `Unlimited` -- exactly what the label match decided.

    /// A test sets the value by writing a manifest, which it already does for every other field; it
    /// does not have to impersonate a network.
    pub fn from_manifest(deployed: &Deployed) -> Self {
        match deployed.requests_per_second {
            Some(per_second) if per_second > 0 => Self::PerSecond(per_second),
            _ => Self::Unlimited,
        }
    }
}

/// A one-second sliding window over admissions this process granted.
#[derive(Debug)]
pub(super) struct RequestGate {
    ceiling: ChainRequestCeiling,
    granted: tokio::sync::Mutex<std::collections::VecDeque<tokio::time::Instant>>,
    /// Every grant this gate made, including under `Unlimited`.

    /// This exists so a test can assert that a production reader ACTUALLY passes through the gate,
    /// rather than assert on timing (which an `Unlimited` gate cannot show) or on the reader's
    /// return value (which is identical whether or not the admit is there). Without it, deleting an
    /// `admit()` from the pager broke nothing, which is how review finding 4 was found.
    #[cfg(test)]
    admissions: std::sync::atomic::AtomicUsize,
}

impl RequestGate {
    pub(super) fn new(ceiling: ChainRequestCeiling) -> Self {
        Self {
            ceiling,
            granted: tokio::sync::Mutex::new(std::collections::VecDeque::new()),
            #[cfg(test)]
            admissions: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// How many admissions this gate granted. Counts grants, not HTTP requests: how many requests
    /// the SDK makes out of one admission is not ours and is not counted here.
    #[cfg(test)]
    pub(super) fn admissions(&self) -> usize {
        self.admissions.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Wait until this request may go out. Returns immediately when the network has no ceiling.
    pub(super) async fn admit(&self) {
        #[cfg(test)]
        self.admissions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ChainRequestCeiling::PerSecond(ceiling) = self.ceiling else {
            return;
        };
        let ceiling = usize::try_from(ceiling.max(1)).unwrap_or(1);
        let window = std::time::Duration::from_secs(1);
        loop {
            let wait = {
                let mut granted = self.granted.lock().await;
                let now = tokio::time::Instant::now();
                while granted
                    .front()
                    .is_some_and(|at| now.saturating_duration_since(*at) >= window)
                {
                    granted.pop_front();
                }
                if granted.len() < ceiling {
                    granted.push_back(now);
                    return;
                }
                // The oldest admission in the window decides when a slot frees.
                let oldest = *granted
                    .front()
                    .expect("the window is full, so it is not empty");
                window.saturating_sub(now.saturating_duration_since(oldest))
            };
            tokio::time::sleep(wait.max(std::time::Duration::from_millis(1))).await;
        }
    }
}

/// The SDK's `ChainClient` behind our own admission ceiling.

/// # Why there is no `Deref`

/// `Deref` would let any call site reach the inner `ChainClient` and go out unmetered -- silently,
/// with no edit and no warning, so the ceiling would read as present and not apply. The methods are
/// therefore enumerated: the set is SIX, measured across 100 call sites, and enumerating it makes a
/// bypass a compile error instead of a quiet pass.

/// `endpoint` is exempt because it issues no request; the other five admit before delegating.
pub struct LimitedChainClient {
    inner: ChainClient,
    gate: RequestGate,
}

impl LimitedChainClient {
    fn new(inner: ChainClient, ceiling: ChainRequestCeiling) -> Self {
        Self {
            inner,
            gate: RequestGate::new(ceiling),
        }
    }

    /// The ceiling this instance was built with. Carried per instance, never global: a process that
    /// somehow held two networks would otherwise give the second one the first one's ceiling, and
    /// the failure would be silent on the money path.
    pub fn ceiling(&self) -> ChainRequestCeiling {
        self.gate.ceiling
    }

    /// The admission gate, so readers that dial through our own `reqwest` client can take a slot
    /// from the SAME budget as the SDK getters. One ceiling per backend, not one per
    /// transport -- the chain counts requests, not which client sent them.
    pub(super) fn gate(&self) -> &RequestGate {
        &self.gate
    }

    /// The raw SDK client, with NO ceiling applied.

    /// this is the counted bypass, not a convenience. It exists because `RealChainBackend::client()`
    /// still hands the raw client to ~100 existing call sites, and changing that type cascades through
    /// the CLI without a fixed point -- measured at 69, then 50, then 11 compile errors, each round
    /// uncovering another layer. So the ceiling is added ALONGSIDE rather than in place of it, the
    /// remaining unmetered sites are frozen by `ci/check_client_bypass_ratchet.sh`, and they are
    /// converted in batches. Anything reached through here is NOT rate limited.
    pub fn unmetered(&self) -> &ChainClient {
        &self.inner
    }

    /// No admission: reading the configured endpoint sends nothing.
    pub fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    pub async fn get_account(&self, address: &Address) -> Result<Option<Account>> {
        self.gate.admit().await;
        self.inner.get_account(address).await
    }

    pub async fn get_account_in_dapp(
        &self,
        address: &Address,
        dapp: &Address,
    ) -> Result<Option<Account>> {
        self.gate.admit().await;
        self.inner.get_account_in_dapp(address, dapp).await
    }

    pub async fn run_getter(
        &self,
        address: &Address,
        abi: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        self.gate.admit().await;
        self.inner.run_getter(address, abi, method, args).await
    }

    pub async fn chain_liveness(&self) -> Result<ChainLiveness> {
        self.gate.admit().await;
        self.inner.chain_liveness().await
    }
}

/// Chain reads that repeat when they got no answer. See [`retry_transient_read`] for the policy
/// and for why a read may be repeated at all.
#[allow(async_fn_in_trait)]
pub trait RetryingReads {
    async fn run_getter_retrying(
        &self,
        address: &Address,
        abi: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>>;

    async fn get_account_retrying(&self, address: &Address) -> Result<Option<Account>>;

    async fn chain_liveness_retrying(&self) -> Result<ChainLiveness>;
}

// the raw SDK client keeps this impl because six production sites still build a
// `ChainClient` directly and the CLI will not compile without it. Those six go out UNMETERED and are
// named in the PR body and frozen by a ratchet check; this impl is the reason they compile, not a
// second opinion about whether they should exist. Every path that goes through the manifest gets
// `LimitedChainClient` and its ceiling.
impl RetryingReads for ChainClient {
    async fn run_getter_retrying(
        &self,
        address: &Address,
        abi: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        retry_transient_read(|| self.run_getter(address, abi, method, args.clone())).await
    }

    async fn get_account_retrying(&self, address: &Address) -> Result<Option<Account>> {
        retry_transient_read(|| self.get_account(address)).await
    }

    async fn chain_liveness_retrying(&self) -> Result<ChainLiveness> {
        retry_transient_read(|| self.chain_liveness()).await
    }
}

impl RetryingReads for LimitedChainClient {
    async fn run_getter_retrying(
        &self,
        address: &Address,
        abi: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        retry_transient_read(|| self.run_getter(address, abi, method, args.clone())).await
    }

    async fn get_account_retrying(&self, address: &Address) -> Result<Option<Account>> {
        retry_transient_read(|| self.get_account(address)).await
    }

    async fn chain_liveness_retrying(&self) -> Result<ChainLiveness> {
        retry_transient_read(|| self.chain_liveness()).await
    }
}

/// Real on-chain backend on top of `gosh.ackinacki` `ChainClient`.
/// Carries a live connection to the chain and the root addresses from the manifest.
pub struct RealChainBackend {
    client: LimitedChainClient,
    /// `DEXDO_USER_AGENT` http client for reads, with reqwest's default redirect behavior.
    pub(super) http: reqwest::Client,
    /// `DEXDO_USER_AGENT` client used only for one-shot money POSTs to `/v2/messages`.
    money_post_http: reqwest::Client,
    superroot: Address,
    deployed: Deployed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DealAccountIdentity {
    code_hash: String,
    boc_hash: String,
}

fn account_boc_hash(boc: &str) -> String {
    use sha2::{Digest, Sha256};

    Sha256::digest(boc.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[async_trait::async_trait]
trait DealSnapshotSource {
    async fn account_identity(&mut self) -> Result<Option<DealAccountIdentity>>;
    async fn state(&mut self) -> Result<Option<DealChainState>>;
    async fn subscription(&mut self) -> Result<Option<DealSubscription>>;
    async fn seller_bond(&mut self) -> Result<Option<DealSellerBond>>;
    async fn buyer_bond(&mut self) -> Result<Option<DealBuyerBond>>;
}

struct LiveDealSnapshotSource<'a> {
    chain: &'a RealChainBackend,
    token_contract: &'a Address,
}

#[async_trait::async_trait]
impl DealSnapshotSource for LiveDealSnapshotSource<'_> {
    async fn account_identity(&mut self) -> Result<Option<DealAccountIdentity>> {
        let Some(account) = self
            .chain
            .client
            .get_account_retrying(self.token_contract)
            .await?
        else {
            return Ok(None);
        };
        if !account.is_active() {
            return Ok(None);
        }
        let code_hash = account
            .code_hash
            .as_deref()
            .and_then(normalize_code_hash)
            .ok_or_else(|| {
                anyhow!(
                    "TokenContract {} is active but has no code hash",
                    self.token_contract
                )
            })?;
        let boc = account.boc.as_deref().ok_or_else(|| {
            anyhow!(
                "TokenContract {} is active but has no account BOC",
                self.token_contract
            )
        })?;
        Ok(Some(DealAccountIdentity {
            code_hash,
            boc_hash: account_boc_hash(boc),
        }))
    }

    async fn state(&mut self) -> Result<Option<DealChainState>> {
        self.chain
            .token_contract_deal_state(self.token_contract)
            .await
    }

    async fn subscription(&mut self) -> Result<Option<DealSubscription>> {
        self.chain
            .token_contract_subscription(self.token_contract)
            .await
    }

    async fn seller_bond(&mut self) -> Result<Option<DealSellerBond>> {
        self.chain
            .token_contract_deal_seller_bond(self.token_contract)
            .await
    }

    async fn buyer_bond(&mut self) -> Result<Option<DealBuyerBond>> {
        self.chain
            .token_contract_deal_buyer_bond(self.token_contract)
            .await
    }
}

async fn read_deal_snapshot_round<S: DealSnapshotSource + Send>(
    source: &mut S,
) -> Result<Option<DealChainSnapshot>> {
    let Some(before) = source.account_identity().await? else {
        return Ok(None);
    };
    let state = source.state().await?;
    let subscription = source.subscription().await?;
    let seller_bond = source.seller_bond().await?;
    let buyer_bond = source.buyer_bond().await?;
    let after = source.account_identity().await?;

    if after.as_ref() != Some(&before) {
        return Err(anyhow!(
            "TokenContract changed or was destroyed while its accounting getters were read"
        ));
    }

    let state =
        state.ok_or_else(|| anyhow!("getState() returned no data for an active contract"))?;
    let subscription = subscription
        .ok_or_else(|| anyhow!("getSubscription() returned no data for an active contract"))?;
    let seller_bond = seller_bond
        .ok_or_else(|| anyhow!("getSellerBond() returned no data for an active contract"))?;
    let buyer_bond = buyer_bond
        .ok_or_else(|| anyhow!("getBuyerBond() returned no data for an active contract"))?;
    let snapshot = DealChainSnapshot {
        account_code_hash: before.code_hash,
        account_boc_hash: before.boc_hash,
        state,
        subscription,
        seller_bond,
        buyer_bond,
    };
    snapshot
        .validate_cross_getter_invariants()
        .map_err(anyhow::Error::msg)?;
    Ok(Some(snapshot))
}

async fn read_coherent_deal_snapshot<S: DealSnapshotSource + Send>(
    source: &mut S,
) -> Result<Option<DealChainSnapshot>> {
    let mut last_reason = "no snapshot attempt completed".to_string();
    for attempt in 1..=DEAL_SNAPSHOT_MAX_ATTEMPTS {
        match read_deal_snapshot_round(source).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => {
                last_reason = format!("attempt {attempt}: bracketed read failed: {error}");
            }
        }
    }
    Err(anyhow!(
        "could not obtain a coherent TokenContract snapshot after \
         {DEAL_SNAPSHOT_MAX_ATTEMPTS} attempts: {last_reason}"
    ))
}

#[cfg(test)]
mod coherent_deal_snapshot_tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum Script {
        Stable,
        DestroyAfter(usize),
        MutateAfter(usize),
        MutateEveryRound,
    }

    struct ScriptSource {
        script: Script,
        calls: usize,
    }

    impl ScriptSource {
        fn new(script: Script) -> Self {
            Self { script, calls: 0 }
        }

        fn observe(&mut self) -> (bool, u64) {
            self.calls += 1;
            match self.script {
                Script::Stable => (true, 0),
                Script::DestroyAfter(last_alive) => (self.calls <= last_alive, 0),
                Script::MutateAfter(last_old) => (true, u64::from(self.calls > last_old)),
                Script::MutateEveryRound => (true, ((self.calls - 1) / 5) as u64),
            }
        }

        fn state(generation: u64) -> DealChainState {
            DealChainState {
                funded: true,
                opened: true,
                probe_accepted: true,
                disputed: false,
                deposit: 100 + u128::from(generation),
                finalized_owed: 3,
                tokens_final: 10,
                tokens_pending: 30,
                probe_tick: 2,
                funded_time: Some(70),
                probe_time: 40,
                last_claim_time: 60,
                dispute_time: 0,
            }
        }

        fn subscription(generation: u64) -> DealSubscription {
            let funded_tokens = 2 * crate::params::TICK_SIZE + u128::from(generation);
            DealSubscription {
                deal_flags: 0,
                sub_weeks: 0,
                week_index: 0,
                tokens_per_week: funded_tokens,
                funded_tokens,
                tokens_paid: 0,
                period_start: 70,
                week_base_tokens: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl DealSnapshotSource for ScriptSource {
        async fn account_identity(&mut self) -> Result<Option<DealAccountIdentity>> {
            let (alive, generation) = self.observe();
            Ok(alive.then(|| DealAccountIdentity {
                code_hash: "code".to_string(),
                boc_hash: format!("boc-{generation}"),
            }))
        }

        async fn state(&mut self) -> Result<Option<DealChainState>> {
            let (alive, generation) = self.observe();
            Ok(alive.then(|| Self::state(generation)))
        }

        async fn subscription(&mut self) -> Result<Option<DealSubscription>> {
            let (alive, generation) = self.observe();
            Ok(alive.then(|| Self::subscription(generation)))
        }

        async fn seller_bond(&mut self) -> Result<Option<DealSellerBond>> {
            let (alive, generation) = self.observe();
            Ok(alive.then(|| DealSellerBond {
                bond_funded: true,
                bond_held: 200 + u128::from(generation),
                bond_required: 200 + u128::from(generation),
            }))
        }

        async fn buyer_bond(&mut self) -> Result<Option<DealBuyerBond>> {
            let (alive, _) = self.observe();
            Ok(alive.then_some(DealBuyerBond {
                bond_held: 0,
                bond_required: 0,
            }))
        }
    }

    struct FixedSource {
        snapshot: DealChainSnapshot,
    }

    #[async_trait::async_trait]
    impl DealSnapshotSource for FixedSource {
        async fn account_identity(&mut self) -> Result<Option<DealAccountIdentity>> {
            Ok(Some(DealAccountIdentity {
                code_hash: self.snapshot.account_code_hash.clone(),
                boc_hash: self.snapshot.account_boc_hash.clone(),
            }))
        }

        async fn state(&mut self) -> Result<Option<DealChainState>> {
            Ok(Some(self.snapshot.state))
        }

        async fn subscription(&mut self) -> Result<Option<DealSubscription>> {
            Ok(Some(self.snapshot.subscription))
        }

        async fn seller_bond(&mut self) -> Result<Option<DealSellerBond>> {
            Ok(Some(self.snapshot.seller_bond))
        }

        async fn buyer_bond(&mut self) -> Result<Option<DealBuyerBond>> {
            Ok(Some(self.snapshot.buyer_bond))
        }
    }

    fn valid_subscription_snapshot() -> DealChainSnapshot {
        DealChainSnapshot {
            account_code_hash: "code".to_string(),
            account_boc_hash: "boc".to_string(),
            state: ScriptSource::state(0),
            subscription: DealSubscription {
                deal_flags: flags::SUBSCRIPTION,
                sub_weeks: SUBSCRIPTION_WEEKS,
                week_index: 0,
                tokens_per_week: crate::params::TICK_SIZE,
                funded_tokens: u128::from(SUBSCRIPTION_WEEKS) * crate::params::TICK_SIZE,
                tokens_paid: 0,
                period_start: 70,
                week_base_tokens: 0,
            },
            seller_bond: DealSellerBond {
                bond_funded: true,
                bond_held: 200,
                bond_required: 200,
            },
            buyer_bond: DealBuyerBond {
                bond_held: 200,
                bond_required: 200,
            },
        }
    }

    #[tokio::test]
    async fn coherent_snapshot_accepts_one_complete_boc_bracketed_round() {
        let mut source = ScriptSource::new(Script::Stable);
        let snapshot = read_coherent_deal_snapshot(&mut source)
            .await
            .expect("stable snapshot")
            .expect("active contract");
        assert_eq!(source.calls, 6);
        assert_eq!(snapshot.buyer_locked().unwrap(), 102);
    }

    #[tokio::test]
    async fn one_round_rejects_destroy_between_every_getter_boundary() {
        // Calls are: account-before, state, subscription, seller bond, buyer
        // bond, account-after. Destroy after any of the first five must never
        // produce an active mixed snapshot.
        for boundary in 1..=5 {
            let mut source = ScriptSource::new(Script::DestroyAfter(boundary));
            assert!(
                read_deal_snapshot_round(&mut source).await.is_err(),
                "destroy after call {boundary} must fail the complete round"
            );
        }
    }

    #[tokio::test]
    async fn one_round_rejects_mutation_between_every_getter_boundary() {
        for boundary in 1..=5 {
            let mut source = ScriptSource::new(Script::MutateAfter(boundary));
            assert!(
                read_deal_snapshot_round(&mut source).await.is_err(),
                "mutation after call {boundary} must fail the complete round"
            );
        }
    }

    #[tokio::test]
    async fn coherent_reader_retries_one_bracketed_round_after_each_mutation() {
        let mut source = ScriptSource::new(Script::MutateEveryRound);
        let error = read_coherent_deal_snapshot(&mut source)
            .await
            .expect_err("every bracketed round mutates");
        assert_eq!(source.calls, DEAL_SNAPSHOT_MAX_ATTEMPTS * 6);
        assert!(error
            .to_string()
            .contains("could not obtain a coherent TokenContract snapshot"));
    }

    #[tokio::test]
    async fn coherent_snapshot_rejects_each_bond_tuple_contradiction() {
        let valid = valid_subscription_snapshot();
        let mut cases = Vec::new();

        let mut snapshot = valid.clone();
        snapshot.seller_bond.bond_held = 201;
        cases.push(("seller held above required", snapshot, "getSellerBond()"));

        let mut snapshot = valid.clone();
        snapshot.seller_bond.bond_funded = false;
        cases.push((
            "unfunded seller reports held value",
            snapshot,
            "bondFunded=false",
        ));

        let mut snapshot = valid.clone();
        snapshot.state.funded = false;
        cases.push((
            "opened state is not funded",
            snapshot,
            "opened=true with funded=false",
        ));

        let mut snapshot = valid.clone();
        snapshot.seller_bond.bond_funded = false;
        snapshot.seller_bond.bond_held = 0;
        cases.push((
            "opened seller bond is not funded",
            snapshot,
            "fully funded non-zero seller bond",
        ));

        let mut snapshot = valid.clone();
        snapshot.seller_bond.bond_held = 199;
        cases.push((
            "opened seller bond is under-held",
            snapshot,
            "fully funded non-zero seller bond",
        ));

        let mut snapshot = valid.clone();
        snapshot.buyer_bond.bond_held = 201;
        cases.push(("buyer held above required", snapshot, "getBuyerBond()"));

        let mut snapshot = valid.clone();
        snapshot.buyer_bond.bond_held = 199;
        cases.push((
            "live subscription buyer bond is under-held",
            snapshot,
            "fully held non-zero buyer bond",
        ));

        let mut snapshot = valid.clone();
        snapshot.buyer_bond.bond_held = 199;
        snapshot.buyer_bond.bond_required = 199;
        cases.push((
            "subscription required bonds differ",
            snapshot,
            "is not the shape getBuyerBond() can report",
        ));

        let mut snapshot = valid.clone();
        snapshot.subscription = ScriptSource::subscription(0);
        snapshot.buyer_bond = DealBuyerBond {
            bond_held: 1,
            bond_required: 1,
        };
        cases.push((
            "ordinary deal reports a non-zero buyer requirement",
            snapshot,
            "is not the shape getBuyerBond() can report",
        ));

        let mut snapshot = valid;
        snapshot.state.opened = false;
        snapshot.state.probe_accepted = false;
        snapshot.seller_bond = DealSellerBond {
            bond_funded: false,
            bond_held: 0,
            bond_required: 0,
        };
        snapshot.buyer_bond = DealBuyerBond {
            bond_held: 0,
            bond_required: 0,
        };
        cases.push((
            "live funded subscription has zero buyer bond",
            snapshot,
            "fully held non-zero buyer bond",
        ));

        for (name, snapshot, expected) in cases {
            let mut source = FixedSource { snapshot };
            let error = read_deal_snapshot_round(&mut source).await.expect_err(name);
            assert!(
                error.to_string().contains(expected),
                "{name}: expected `{expected}`, got `{error}`"
            );
        }
    }

    #[tokio::test]
    async fn coherent_snapshot_rejects_funded_ordinary_volume_below_two_ticks() {
        for funded_tokens in [0, crate::params::TICK_SIZE] {
            let mut snapshot = valid_subscription_snapshot();
            snapshot.subscription = DealSubscription {
                deal_flags: 0,
                sub_weeks: 0,
                week_index: 0,
                tokens_per_week: funded_tokens,
                funded_tokens,
                tokens_paid: 0,
                period_start: 0,
                week_base_tokens: 0,
            };
            snapshot.buyer_bond = DealBuyerBond {
                bond_held: 0,
                bond_required: 0,
            };

            let mut source = FixedSource { snapshot };
            let error = read_deal_snapshot_round(&mut source)
                .await
                .expect_err("funded ordinary deal below two ticks must fail closed");
            assert!(
                error.to_string().contains("requires at least two ticks"),
                "unexpected error for fundedTokens={funded_tokens}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn coherent_snapshot_allows_terminal_subscription_with_released_bonds() {
        let mut snapshot = valid_subscription_snapshot();
        snapshot.state.opened = false;
        snapshot.state.deposit = 0;
        snapshot.state.probe_tick = 0;
        snapshot.seller_bond.bond_held = 0;
        snapshot.buyer_bond.bond_held = 0;
        assert!(snapshot.state.is_stopped());

        let mut source = FixedSource { snapshot };
        let observed = read_deal_snapshot_round(&mut source)
            .await
            .expect("terminal subscription is coherent")
            .expect("active terminal contract");
        assert_eq!(observed.buyer_bond.bond_held, 0);
        assert_eq!(observed.buyer_bond.bond_required, 200);
    }
}

/// True iff `e` is the BK REST `/v2/account` lookup 404 -- the destination account is not yet in the
/// block-manager index (a **funded-uninit deploy target**). Matched on the specific endpoint **and**
/// status, NOT a blanket "contains 404": a 404 from any other URL/cause still propagates as a real
/// error, and this only ever flips routing for a deploy-message send (`submit_once(.., deploy=true)`)..
pub(super) fn is_uninit_account_404(e: &str) -> bool {
    e.contains("/v2/account") && e.contains("404")
}

fn bare_hex(s: &str) -> String {
    s.trim()
        .trim_start_matches("0:")
        .trim_start_matches("0x")
        .to_lowercase()
}

fn submit_message_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("dexdo-{}-{nanos}", std::process::id())
}

fn submit_failure_is_clock_related(payload: &Value) -> bool {
    const GIVER_ADDRESS: &str =
        "0:1111111111111111111111111111111111111111111111111111111111111111";

    fn contains_string(value: &Value, wanted: &str) -> bool {
        match value {
            Value::String(value) => value == wanted,
            Value::Array(items) => items.iter().any(|item| contains_string(item, wanted)),
            Value::Object(fields) => fields.values().any(|value| contains_string(value, wanted)),
            _ => false,
        }
    }

    fn contains_exit_code(value: &Value, wanted: impl Fn(u64) -> bool + Copy) -> bool {
        match value {
            Value::Array(items) => items.iter().any(|item| contains_exit_code(item, wanted)),
            Value::Object(fields) => fields.iter().any(|(key, value)| {
                (matches!(
                    key.as_str(),
                    "exit_code" | "exitCode" | "vm_exit_code" | "vmExitCode"
                ) && value.as_u64().is_some_and(wanted))
                    || contains_exit_code(value, wanted)
            }),
            _ => false,
        }
    }

    contains_exit_code(payload, |code| matches!(code, 401 | 402))
        || (contains_string(payload, GIVER_ADDRESS)
            && contains_exit_code(payload, |code| matches!(code, 102 | 103)))
}

fn checked_submit_response(resp: Value) -> Result<Value> {
    validate_onchain_submit_response(resp).map_err(|e| {
        tracing::debug!(
            payload = %e.sanitized_payload(),
            "chain submit failure payload"
        );
        let clock_related = submit_failure_is_clock_related(e.sanitized_payload());
        let error = anyhow!(e);
        if clock_related {
            error.context(
                "signed-write expiry/replay rejection: verify the operator clock/NTP; a preflight observation may have raced or gone stale",
            )
        } else {
            error
        }
    })
}

fn external_message_hash(boc_base64: &str) -> Result<String> {
    use base64::Engine;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(boc_base64)
        .context("decode signed external-message BOC")?;
    let cell = tvm_types::read_single_root_boc(&bytes)
        .map_err(|error| anyhow!("decode signed external-message cell: {error}"))?;
    Ok(cell.repr_hash().to_hex_string())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CorrelatedActionReceipt {
    message_hash: String,
    transaction_hash: Option<String>,
    aborted: Option<bool>,
    compute_exit_code: Option<i64>,
    action_success: Option<bool>,
    result_code: Option<i64>,
    no_funds: Option<bool>,
    outmsg_count: Option<u64>,
    account_latest_transaction_hash: Option<String>,
    account_ecc_balances: Option<Vec<(u32, u128)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDeployWalletActionObservation {
    pub transaction_hash: String,
    pub aborted: bool,
    pub action_result_code: i64,
    pub outmsg_count: u64,
    pub wallet_ecc_balances: Option<Vec<(u32, u128)>>,
}

const EXACT_MESSAGE_RECEIPT_QUERY: &str = r#"
    query($hash: String!, $accountId: String!, $dappId: String!) {
      blockchain {
        message(hash: $hash) {
          id dst
          dst_transaction {
            id status aborted account_addr outmsg_cnt
            compute { exit_code success }
            action { result_code success no_funds }
          }
        }
        account(account_id: $accountId, dapp_id: $dappId) {
          info {
            id dapp_id
            balance_other { currency value }
          }
          transactions(last: 1) {
            edges { node { id } }
          }
        }
      }
    }
"#;

/// Which of the two pockets a buy order is short on, or `None` when it can be placed.

/// **`placeInferenceBuy` refuses two different shortfalls with the SAME `ERR_LOW_VALUE`.** The escrow
/// comes out of the note's private `_balance[SHELL]` -- the figure the note keeps for its owner -- and
/// the placement charge comes out of the note's ACCOUNT `currencies[SHELL]`, physical ECC[2] burnt by
/// `gosh.burnecc(BUY_ORDER_GAS)`. They are different pockets, they are topped up differently, and the
/// contract cannot tell the buyer which one it meant.

/// The charge is new: a sell offer always paid `GAS_POST_FROM_NOTE` through the deal, a buy rested
/// for free. A buyer whose note has a full balance and no physical ECC now fails at a call that used
/// to work, with an error that points at the escrow.
fn buy_order_shortfall(private_balance: u128, account_ecc: u128, escrow: u128) -> Option<String> {
    let charge = crate::params::BUY_ORDER_GAS_RAW;
    if private_balance < escrow {
        return Some(format!(
            "this note holds {private_balance} raw SHELL and the order escrows {escrow}: the book \
             would refuse it with ERR_LOW_VALUE. Top the note's balance up, or place a smaller order."
        ));
    }
    if account_ecc < charge {
        return Some(format!(
            "this note holds {account_ecc} raw ECC[2] and placing a buy order burns {charge} of it \
             before the order reaches the book -- a separate pocket from the {escrow} escrow, which \
             this note does cover. The contract refuses both shortfalls with the same ERR_LOW_VALUE, \
             so read this as: the escrow is fine, the placement charge is not. Send SHELL to the \
             note's ACCOUNT. A buy used to rest for free; since 4.0.36 it pays what a sell offer \
             pays, filled or not."
        ));
    }
    None
}

/// The decision behind [`RealChainBackend::refuse_terminal_call_neither_side_can_pay`], kept pure so
/// it can be asserted without a chain.

/// `None` means go: one of the two can pay. `Some` names BOTH readings, because the operator's next
/// move depends on which is short and the chain's own answer -- an aborted action phase -- says
/// neither.
fn terminal_charge_refusal(
    call: &str,
    note: &Address,
    note_ecc: u128,
    deal: &Address,
    deal_ecc: u128,
    charge: u128,
) -> Option<String> {
    if note_ecc >= charge || deal_ecc >= charge {
        return None;
    }
    Some(format!(
        "{call} cannot be paid for by either side, so the deal would reject it in its action phase \
         and nothing would change. Your note {note} holds {note_ecc} raw ECC[2] and the charge is \
         {charge}, so it cannot attach one; deal {deal} holds {deal_ecc} in its own reserve, so it \
         cannot cover one either. Two different fixes: put SHELL on your note and retry, or ask the \
         SELLER to top the deal up -- only the seller's note can do that, `fundDeployShell` is \
         owner-only. Nothing was sent."
    ))
}

fn fund_deploy_shell_receipt_error(
    submit_error: anyhow::Error,
    expected_message_hash: &str,
    receipt: Option<&CorrelatedActionReceipt>,
) -> anyhow::Error {
    match receipt {
        Some(receipt)
            if bare_hex(&receipt.message_hash) == bare_hex(expected_message_hash)
                && receipt.transaction_hash.is_some()
                && receipt.aborted == Some(true)
                && receipt.action_success == Some(false)
                && receipt.result_code == Some(38)
                && receipt.no_funds == Some(true) =>
        {
            submit_error.context(format!(
                "fundDeployShell failed: insufficient ECC[2]/SHELL for note_fund_deploy_shell; \
                 correlated finalized receipt message_hash={} transaction_hash={} \
                 aborted=true action_success=false action_result_code=38 no_funds=true",
                expected_message_hash,
                receipt
                    .transaction_hash
                    .as_deref()
                    .expect("guard requires a transaction hash"),
            ))
        }
        Some(receipt) => submit_error.context(format!(
            "fundDeployShell aborted; correlated receipt message_hash={} transaction_hash={} \
             aborted={} action_success={} action_result_code={} no_funds={}; ECC[2] cause not proven",
            expected_message_hash,
            receipt
                .transaction_hash
                .as_deref()
                .unwrap_or("<unavailable>"),
            receipt
                .aborted
                .map_or_else(|| "<unavailable>".to_string(), |value| value.to_string()),
            receipt
                .action_success
                .map_or_else(|| "<unavailable>".to_string(), |value| value.to_string()),
            receipt
                .result_code
                .map_or_else(|| "<unavailable>".to_string(), |value| value.to_string()),
            receipt
                .no_funds
                .map_or_else(|| "<unavailable>".to_string(), |value| value.to_string()),
        )),
        None => submit_error.context(format!(
            "fundDeployShell aborted; no finalized destination receipt matched external \
             message_hash={expected_message_hash}; ECC[2] cause not proven"
        )),
    }
}

/// The single place `DEXDO_USER_AGENT` is attached to an outgoing client. Every http client that
/// talks to the chain edge is built from this - a header set in N places is a header the N+1th
/// site will not set, and the edge 403s a request that arrives without one.
pub(super) fn chain_http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().user_agent(DEXDO_USER_AGENT)
}

/// An http client for the chain edge, carrying our identifier. Callers outside this crate must
/// use this instead of `reqwest::Client::new()`, which sends no `User-Agent` at all.
pub fn chain_http_client() -> reqwest::Result<reqwest::Client> {
    chain_http_builder().build()
}

fn build_money_post_http_client() -> reqwest::Result<reqwest::Client> {
    chain_http_builder()
        .redirect(reqwest::redirect::Policy::none())
        // A signed BOC expires after this existing SDK window. Do not let a lost HTTP response
        // hold the caller forever; the settlement path reconciles the possibly-landed BOC from
        // immutable event history and never submits it again.
        .timeout(std::time::Duration::from_secs(SDK_MESSAGE_EXPIRY_SECS))
        .build()
}

async fn send_message_checked(
    http: &reqwest::Client,
    money_post_http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
) -> Result<Value> {
    let account_id = dest_account_id_hex(boc_base64)?;
    let dapp_id = fetch_dapp_id(http, endpoint, &account_id).await?;
    send_message_routed_checked(
        money_post_http,
        endpoint,
        boc_base64,
        &account_id,
        &dapp_id,
        None,
    )
    .await
}

pub(super) async fn send_message_routed_checked(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
    account_id: &str,
    dapp_id: &str,
    thread_id: Option<&str>,
) -> Result<Value> {
    let mut item = json!({
        "id": submit_message_id(),
        "body": boc_base64,
        "account_id": bare_hex(account_id),
        "dapp_id": bare_hex(dapp_id),
    });
    if let Some(thread_id) = thread_id {
        item["thread_id"] = json!(bare_hex(thread_id));
    }
    let response = http
        .post(format!("{}/v2/messages", endpoint.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .json(&json!([item]))
        .send()
        .await?;
    if response.status().is_redirection() {
        return Err(anyhow!(
            "chain submit refused HTTP redirect {}",
            response.status()
        ));
    }
    let response = chain_response_for_status(response).await?;
    let status = response.status();
    let resp = response
        .json::<Value>()
        .await
        .map_err(|source| MessagePostResponseDecodeError { status, source })?;
    checked_submit_response(resp)
}

async fn send_message_routed_money_once(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
    account_id: &str,
    dapp_id: &str,
) -> Result<Value> {
    let item = json!({
        "id": submit_message_id(),
        "body": boc_base64,
        "account_id": bare_hex(account_id),
        "dapp_id": bare_hex(dapp_id),
    });
    let response = http
        .post(format!("{}/v2/messages", endpoint.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .json(&json!([item]))
        .send()
        .await
        .map_err(|source| {
            let source = anyhow::Error::new(source);
            let before_post = source
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|error| error.is_builder() || error.is_connect());
            anyhow::Error::new(if before_post {
                MoneySubmitError::Preparation { source }
            } else {
                MoneySubmitError::Ambiguous { source }
            })
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow::Error::new(MoneySubmitError::Ambiguous {
            source: anyhow!(
                "money POST returned unvalidated HTTP status {status}; redirects are disabled and no fresh BOC is safe"
            ),
        }));
    }
    let response = response.json::<Value>().await.map_err(|source| {
        anyhow::Error::new(MoneySubmitError::Ambiguous {
            source: anyhow::Error::new(source),
        })
    })?;
    checked_submit_response(response)
        .map_err(|source| anyhow::Error::new(MoneySubmitError::Rejected { source }))
}

fn is_queue_overflow_submit(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("queue_overflow")
        || message.contains("queue overflow")
        || message.contains("message queue is full")
}

fn is_transient_submit_failure(error: &anyhow::Error) -> bool {
    is_queue_overflow_submit(error) || is_transient_transport_failure(error)
}

fn is_decoded_transient_money_rejection(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<MoneySubmitError>())
        .is_some_and(|outcome| matches!(outcome, MoneySubmitError::Rejected { .. }))
        && is_transient_submit_failure(error)
}

async fn retry_buyer_money_submit(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
    account_id: &str,
    dapp_id: &str,
) -> Result<Value> {
    let mut delay = crate::params::TRANSIENT_SUBMIT_INITIAL_BACKOFF;
    for attempt in 1..=crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL {
        match send_message_routed_money_once(http, endpoint, boc_base64, account_id, dapp_id).await
        {
            Ok(value) => return Ok(value),
            Err(error) if is_decoded_transient_money_rejection(&error) => {
                eprintln!(
                    "chain transient submit error (attempt {attempt}): {error}; waiting {delay:?} then retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * crate::params::TRANSIENT_SUBMIT_BACKOFF_MULTIPLIER)
                    .min(crate::params::TRANSIENT_SUBMIT_MAX_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
    send_message_routed_money_once(http, endpoint, boc_base64, account_id, dapp_id).await
}

/// Submit an explicit buyer STOP exactly once. A decoded queue-overflow response is still
/// outcome-ambiguous for this non-idempotent signed message and must never trigger a fresh POST.
async fn send_explicit_stop_money_once(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
    account_id: &str,
    dapp_id: &str,
) -> Result<Value> {
    match send_message_routed_money_once(http, endpoint, boc_base64, account_id, dapp_id).await {
        Err(error) if is_queue_overflow_submit(&error) => {
            Err(anyhow::Error::new(MoneySubmitError::Ambiguous {
                source: error,
            }))
        }
        result => result,
    }
}

#[cfg(test)]
async fn prepare_policy_stop_money_post_if<P, F, Fut>(
    prepare: P,
    before_post: &mut (dyn FnMut() -> bool + Send),
    send: F,
) -> Result<Option<Value>>
where
    P: std::future::Future<Output = Result<(String, String, String, String)>>,
    F: FnOnce((String, String, String, String)) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let prepared = prepare.await?;
    if !before_post() {
        return Ok(None);
    }
    send(prepared).await.map(Some)
}

/// The exact-hash destination receipt read, retried the way every other chain read is.

/// both money-path observers reached the request below directly -- the note-deploy wallet and
/// RootPN polls through [`poll_finalized_destination_receipt`], and the multisig delivery proof in
/// [`prove_multisig_delivery_message`]. So a single `502` from the edge ended `dexdo note deploy`
/// AFTER the wallet spend had been submitted, leaving the run to be reconciled by hand. The same
/// client already calls a `502` on a read repeatable -- [`read_failure_is_transient`] accepts any
/// `is_server_error()` -- and repeats it at sixteen other production reads in this file, plus two
/// in `dexdo`. This read was simply outside the wrapper.

/// Retrying it is safe for the reason written above [`retry_transient_read`], and it holds here
/// literally: [`EXACT_MESSAGE_RECEIPT_QUERY`] is a GraphQL **query** over `blockchain.message` and
/// `blockchain.account`. Nothing is submitted, so nothing can be submitted twice.

/// The retry sits on the read rather than on either caller because the poll loop around it repeats
/// for a DIFFERENT reason -- the receipt is not finalized yet -- and a request that came back
/// without an answer is not an unfinalized receipt.

/// The request stays INSIDE this function rather than moving to a `..._once` helper of its own. A
/// helper would take `http: &reqwest::Client`, and `ci/check_client_bypass_ratchet.sh` counts that
/// signature as one more unmetered read surface (its DIRECT ceiling is an equality, measured 28 vs
/// 27). One more reader is exactly what this change does NOT add: it is the same single request,
/// repeated when it comes back with nothing.
async fn query_exact_destination_receipt(
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    dapp_id: &str,
    expected_message_hash: &str,
) -> Result<Value> {
    let gql = format!("{}/graphql", endpoint.trim_end_matches('/'));
    retry_transient_read(|| async {
        let response = http
            .post(&gql)
            .json(&json!({
                "query": EXACT_MESSAGE_RECEIPT_QUERY,
                "variables": {
                    "hash": bare_hex(expected_message_hash),
                    "accountId": bare_hex(account_id),
                    "dappId": bare_hex(dapp_id),
                },
            }))
            .send()
            .await?;
        let response: Value = chain_response_for_status(response).await?.json().await?;
        Ok(response)
    })
    .await
}

fn parse_exact_destination_receipt(
    response: &Value,
    expected_account_id: &str,
    expected_dapp_id: &str,
    expected_message_hash: &str,
) -> Result<Option<CorrelatedActionReceipt>> {
    let node = response
        .pointer("/data/blockchain/message")
        .ok_or_else(|| anyhow!("fundDeployShell receipt GraphQL response shape changed"))?;
    if node.is_null() {
        return Ok(None);
    }
    let message_hash = node["id"]
        .as_str()
        .ok_or_else(|| anyhow!("fundDeployShell exact-hash receipt has no message id"))?;
    if bare_hex(message_hash) != bare_hex(expected_message_hash) {
        return Err(anyhow!(
            "fundDeployShell exact-hash lookup returned mismatched message id"
        ));
    }
    let transaction = &node["dst_transaction"];
    if transaction.is_null() {
        return Ok(None);
    }
    let finalized = transaction["status"].as_i64() == Some(3)
        || transaction["status"].as_str() == Some("Finalized");
    if !finalized {
        return Ok(None);
    }

    let expected_account = bare_hex(expected_account_id);
    let destination = node["dst"]
        .as_str()
        .ok_or_else(|| anyhow!("fundDeployShell exact-hash receipt has no destination"))?;
    let transaction_account = transaction["account_addr"]
        .as_str()
        .ok_or_else(|| anyhow!("fundDeployShell destination transaction has no account"))?;
    let account = response
        .pointer("/data/blockchain/account/info")
        .ok_or_else(|| anyhow!("fundDeployShell receipt has no target account/dapp proof"))?;
    let account_id = account["id"]
        .as_str()
        .ok_or_else(|| anyhow!("fundDeployShell receipt target account has no id"))?;
    let account_dapp = account["dapp_id"]
        .as_str()
        .ok_or_else(|| anyhow!("fundDeployShell receipt target account has no dapp_id"))?;
    if bare_hex(destination) != expected_account
        || bare_hex(transaction_account) != expected_account
        || bare_hex(account_id) != expected_account
        || bare_hex(account_dapp) != bare_hex(expected_dapp_id)
    {
        return Err(anyhow!(
            "fundDeployShell exact-hash receipt destination/account/dapp mismatch"
        ));
    }
    let transaction_hash = transaction["id"]
        .as_str()
        .ok_or_else(|| anyhow!("fundDeployShell finalized destination transaction has no id"))?;
    let aborted = transaction["aborted"].as_bool().ok_or_else(|| {
        anyhow!("fundDeployShell finalized destination transaction has no aborted fact")
    })?;
    let compute_exit_code = transaction["compute"]["exit_code"].as_i64();
    let action_success = transaction["action"]["success"].as_bool();
    let result_code = transaction["action"]["result_code"].as_i64();
    let no_funds = transaction["action"]["no_funds"].as_bool();
    let account_ecc_balances = account["balance_other"].as_array().and_then(|balances| {
        balances
            .iter()
            .map(|balance| {
                let currency = u32::try_from(value_u128(&balance["currency"])?).ok()?;
                let value = value_u128(&balance["value"])?;
                Some((currency, value))
            })
            .collect()
    });
    let account_latest_transaction_hash = response
        .pointer("/data/blockchain/account/transactions/edges/0/node/id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(CorrelatedActionReceipt {
        message_hash: message_hash.to_string(),
        transaction_hash: Some(transaction_hash.to_string()),
        aborted: Some(aborted),
        compute_exit_code,
        action_success,
        result_code,
        no_funds,
        outmsg_count: value_u64(&transaction["outmsg_cnt"]),
        account_latest_transaction_hash,
        account_ecc_balances,
    }))
}

async fn poll_finalized_destination_receipt(
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    dapp_id: &str,
    expected_message_hash: &str,
) -> Result<Option<CorrelatedActionReceipt>> {
    poll_finalized_destination_receipt_with(
        account_id,
        dapp_id,
        expected_message_hash,
        || {
            query_exact_destination_receipt(
                http,
                endpoint,
                account_id,
                dapp_id,
                expected_message_hash,
            )
        },
        crate::params::FINALIZED_DESTINATION_RECEIPT_POLL_INTERVAL,
    )
    .await
}

async fn poll_finalized_destination_receipt_with<F, Fut>(
    account_id: &str,
    dapp_id: &str,
    expected_message_hash: &str,
    mut query: F,
    retry_delay: std::time::Duration,
) -> Result<Option<CorrelatedActionReceipt>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    for attempt in 0..crate::params::FINALIZED_DESTINATION_RECEIPT_MAX_ATTEMPTS {
        let response = query().await?;
        if let Some(errors) = response.get("errors") {
            return Err(anyhow!("fundDeployShell receipt GraphQL errors: {errors}"));
        }
        if let Some(receipt) =
            parse_exact_destination_receipt(&response, account_id, dapp_id, expected_message_hash)?
        {
            return Ok(Some(receipt));
        }
        if attempt + 1 < crate::params::FINALIZED_DESTINATION_RECEIPT_MAX_ATTEMPTS {
            tokio::time::sleep(retry_delay).await;
        }
    }
    Ok(None)
}

pub async fn observe_note_deploy_wallet_action(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
    account_id: &str,
    dapp_id: &str,
) -> Result<Option<NoteDeployWalletActionObservation>> {
    let message_hash = external_message_hash(boc_base64)?;
    let Some(receipt) =
        poll_finalized_destination_receipt(http, endpoint, account_id, dapp_id, &message_hash)
            .await?
    else {
        return Ok(None);
    };
    note_deploy_wallet_action_observation(receipt).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDeployRootPnActionObservation {
    pub transaction_hash: String,
    pub compute_exit_code: i64,
    pub aborted: bool,
    pub action_result_code: Option<i64>,
}

pub async fn observe_note_deploy_rootpn_action(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
    account_id: &str,
    dapp_id: &str,
) -> Result<Option<NoteDeployRootPnActionObservation>> {
    let message_hash = external_message_hash(boc_base64)?;
    let Some(receipt) =
        poll_finalized_destination_receipt(http, endpoint, account_id, dapp_id, &message_hash)
            .await?
    else {
        return Ok(None);
    };
    note_deploy_rootpn_action_observation(receipt).map(Some)
}

fn note_deploy_rootpn_action_observation(
    receipt: CorrelatedActionReceipt,
) -> Result<NoteDeployRootPnActionObservation> {
    Ok(NoteDeployRootPnActionObservation {
        transaction_hash: receipt.transaction_hash.ok_or_else(|| {
            anyhow!("finalized note-deploy RootPN receipt has no transaction hash")
        })?,
        compute_exit_code: receipt.compute_exit_code.ok_or_else(|| {
            anyhow!("finalized note-deploy RootPN receipt has no compute exit code")
        })?,
        aborted: receipt
            .aborted
            .ok_or_else(|| anyhow!("finalized note-deploy RootPN receipt has no aborted fact"))?,
        action_result_code: receipt.result_code,
    })
}

fn note_deploy_wallet_action_observation(
    receipt: CorrelatedActionReceipt,
) -> Result<NoteDeployWalletActionObservation> {
    let transaction_hash = receipt
        .transaction_hash
        .ok_or_else(|| anyhow!("finalized note-deploy wallet receipt has no transaction hash"))?;
    let aborted = receipt
        .aborted
        .ok_or_else(|| anyhow!("finalized note-deploy wallet receipt has no aborted fact"))?;
    let result_code = receipt
        .result_code
        .ok_or_else(|| anyhow!("finalized note-deploy wallet receipt has no action result code"))?;
    let account_latest_transaction_hash =
        receipt.account_latest_transaction_hash.ok_or_else(|| {
            anyhow!("finalized note-deploy wallet receipt has no latest wallet transaction hash")
        })?;
    if bare_hex(&account_latest_transaction_hash) != bare_hex(&transaction_hash) {
        return Err(anyhow!(
            "note-deploy wallet state is stale or advanced: observed transaction hash={}, \
             latest wallet transaction hash={account_latest_transaction_hash}",
            transaction_hash
        ));
    }
    let outmsg_count = receipt.outmsg_count.ok_or_else(|| {
        anyhow!("finalized note-deploy wallet receipt has no outbound-message count")
    })?;
    Ok(NoteDeployWalletActionObservation {
        transaction_hash,
        aborted,
        action_result_code: result_code,
        outmsg_count,
        wallet_ecc_balances: receipt.account_ecc_balances,
    })
}

pub(super) fn previous_page_cursor(
    context: &str,
    page: &Value,
    before: Option<&str>,
) -> Result<Option<String>> {
    let page_info = page
        .get("pageInfo")
        .ok_or_else(|| anyhow!("{context} pageInfo missing"))?;
    let has_previous = page_info["hasPreviousPage"]
        .as_bool()
        .ok_or_else(|| anyhow!("{context} hasPreviousPage missing/invalid"))?;
    if !has_previous {
        return Ok(None);
    }
    let next = page_info["startCursor"]
        .as_str()
        .filter(|cursor| Some(*cursor) != before)
        .ok_or_else(|| anyhow!("{context} pagination made no progress"))?;
    Ok(Some(next.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExtOutMessage {
    pub id: String,
    pub created_at: u64,
    pub cursor: String,
    pub body: String,
}

#[derive(Debug)]
pub(super) struct ExtOutPage {
    pub messages: Vec<ExtOutMessage>,
    pub previous_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenContractParties {
    buyer: String,
    seller_note: String,
}

/// One `PrivateNote.getOutstanding().deals` address that the named `TokenContract` independently
/// confirms as currently funded and belonging to this note.

/// This remains a lead, not a complete recovery result: the note's best-effort fill callback can
/// fail before recording a real deal, and its best-effort close callback can leave a destroyed
/// address recorded until `touchDeal` clears it. Callers must preserve those caveats when exposing
/// this value to an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutstandingDealLead {
    pub token_contract: String,
    pub role: DealRole,
    pub state: DealChainState,
}

/// One address named by `getOutstanding` that its `TokenContract` did not independently confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutstandingDealLeadRefusal {
    pub token_contract: String,
    pub reason: String,
}

/// Read-only diagnostic result for one note's `getOutstanding` mirror.

/// `deal_leads` contains only independently confirmed current facts. `opaque_order_count` is a
/// count on purpose: the getter returns hashes that cannot recover `(modelHash, orderId)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivateNoteOutstandingReport {
    pub deal_leads: Vec<OutstandingDealLead>,
    pub refused_deal_leads: Vec<OutstandingDealLeadRefusal>,
    pub opaque_order_count: usize,
    /// Resting orders named the way the owner can act on them.
    pub resting_orders: Vec<RecoveredRestingOrder>,
    /// `getOutstanding()` keys no recovered pair accounts for -- the honest measure of what is
    /// resting that this run could NOT name. Money is in here; it must never be rendered as zero by
    /// omission.
    pub unexplained_order_keys: Vec<String>,
    /// Whether the inbound-history walk reached the beginning, measured rather than assumed.
    pub history: NoteHistoryCoverage,
}

/// One resting order named by the two values that release it.

/// `cancelInferenceOrder(uint256 modelHash, uint128 orderId)` takes exactly these. The book address
/// is carried too because it is what the membership key is composed from, so an operator can redo
/// the proof by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRestingOrder {
    pub model_hash: String,
    pub order_id: u128,
    pub order_book: String,
    /// `tvm.hash(abi.encode(order_book, order_id))`, found in `getOutstanding().orders`.
    pub key: String,
}

/// Decide, from one note's inbound history and its current `getOutstanding()` keys, which orders are
/// resting and which keys nothing explained.

/// Kept as a free function on purpose: this is the whole of's judgement, and it must be
/// testable without a chain. A recovered pair earns its place only by clearing both gates -- it was
/// never removed, and its composed key is in the set the note publishes RIGHT NOW.
fn resolve_resting_inference_orders(
    placed: &[super::note_events::InferenceOrderCall],
    removed: &[super::note_events::InferenceOrderCall],
    order_keys: &[String],
) -> Result<(Vec<RecoveredRestingOrder>, Vec<String>)> {
    let mut resting = Vec::new();
    let mut explained = std::collections::BTreeSet::new();
    for call in placed {
        if removed
            .iter()
            .any(|gone| gone.order_id == call.order_id && gone.model_hash == call.model_hash)
        {
            continue;
        }
        let order_book = RealChainBackend::canonical_inference_orderbook_address(&call.model_hash)?
            .with_workchain();
        let key = super::note_events::resting_inference_order_key(&order_book, call.order_id)?;
        if !order_keys.iter().any(|stored| stored == &key) {
            continue;
        }
        if !explained.insert(key.clone()) {
            continue;
        }
        resting.push(RecoveredRestingOrder {
            model_hash: call.model_hash.clone(),
            order_id: call.order_id,
            order_book,
            key,
        });
    }
    let unexplained = order_keys
        .iter()
        .filter(|key| !explained.contains(*key))
        .cloned()
        .collect();
    Ok((resting, unexplained))
}

/// How much of the note's inbound history this run actually read.

/// The node serves a bounded page and there is no way to ask it what it retains, so the run states
/// what it observed instead of what it hopes: it pages backwards until the node says there is no
/// previous page, and only then is the list complete. A run that stopped while pages remained says
/// so, because a list believed complete is acted on, and acting on a short list means leaving money
/// where the owner has been told there is none.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoteHistoryCoverage {
    pub messages_read: usize,
    pub reached_beginning: bool,
}

fn outstanding_deal_refusal(
    token_contract: &str,
    reason: impl Into<String>,
) -> OutstandingDealLeadRefusal {
    OutstandingDealLeadRefusal {
        token_contract: token_contract.to_string(),
        reason: reason.into(),
    }
}

fn classify_outstanding_deal_lead(
    note: &str,
    token_contract: &str,
    parties: Option<&TokenContractParties>,
    state: Option<&DealChainState>,
) -> std::result::Result<OutstandingDealLead, OutstandingDealLeadRefusal> {
    let Some(parties) = parties else {
        return Err(outstanding_deal_refusal(
            token_contract,
            "TokenContract.getParties returned no data; the address is absent or destroyed",
        ));
    };
    let Some(state) = state else {
        return Err(outstanding_deal_refusal(
            token_contract,
            "TokenContract.getState returned no data; the address is absent or destroyed",
        ));
    };
    if !state.funded {
        return Err(outstanding_deal_refusal(
            token_contract,
            "TokenContract.getState reports funded=false",
        ));
    }
    if state.is_stopped() {
        return Err(outstanding_deal_refusal(
            token_contract,
            "TokenContract.getState reports a terminal stopped deal",
        ));
    }
    let role = if parties.buyer.eq_ignore_ascii_case(note) {
        DealRole::Buyer
    } else if parties.seller_note.eq_ignore_ascii_case(note) {
        DealRole::Seller
    } else {
        return Err(outstanding_deal_refusal(
            token_contract,
            format!(
                "TokenContract.getParties names buyer {} and seller note {}, not queried note {note}",
                parties.buyer, parties.seller_note
            ),
        ));
    };
    Ok(OutstandingDealLead {
        token_contract: token_contract.to_string(),
        role,
        state: *state,
    })
}

fn decode_private_note_outstanding(value: &Value) -> Result<(Vec<Address>, Vec<String>)> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("PrivateNote.getOutstanding() returned a non-object: {value}"))?;
    if object.len() != 2 || !object.contains_key("deals") || !object.contains_key("orders") {
        return Err(anyhow!(
            "PrivateNote.getOutstanding() must return exactly deals and orders: {value}"
        ));
    }
    let raw_deals = object["deals"]
        .as_array()
        .ok_or_else(|| anyhow!("PrivateNote.getOutstanding().deals is not address[]"))?;
    let raw_orders = object["orders"]
        .as_array()
        .ok_or_else(|| anyhow!("PrivateNote.getOutstanding().orders is not uint256[]"))?;
    // the keys are kept, not counted. Each one is `tvm.hash(abi.encode(book, orderId))`, and
    // matching a key recovered from history against this set is the only thing that turns "an order
    // was placed once" into "this order is resting now". Counting them threw that away.
    let mut order_keys = Vec::with_capacity(raw_orders.len());
    for (index, order) in raw_orders.iter().enumerate() {
        let key = value_to_uint256_hex(order).ok_or_else(|| {
            anyhow!("PrivateNote.getOutstanding().orders[{index}] is not uint256: {order}")
        })?;
        order_keys.push(key);
    }
    let mut deals = raw_deals
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let raw = value.as_str().ok_or_else(|| {
                anyhow!(
                    "PrivateNote.getOutstanding().deals[{index}] is not an address string: {value}"
                )
            })?;
            Address::parse(raw).with_context(|| {
                format!("PrivateNote.getOutstanding().deals[{index}] address {raw}")
            })
        })
        .collect::<Result<Vec<_>>>()?;
    deals.sort_by_key(Address::with_workchain);
    deals.dedup_by(|left, right| {
        left.with_workchain()
            .eq_ignore_ascii_case(&right.with_workchain())
    });
    Ok((deals, order_keys))
}

/// One `InferenceFilled` the book really emitted whose named `sellerTC` did not confirm it.

/// A refusal is evidence, not noise. The book emits `InferenceFilled` in the match transaction
/// itself, so its existence is a fact about this note regardless of what happened afterwards. An
/// operator shown only an empty candidate list cannot tell "the book never named my note" from "the
/// book named it and that deal is already settled or was unwound" -- the two call for opposite
/// actions, and only one of them means there is nothing to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFillCandidateRefusal {
    /// The whole decoded fill, not just the address that failed.

    /// is about identities the client threw away, so a refusal keeps all of them: an operator
    /// whose deal is already settled still needs `sellerNote` and the order ids to reconcile it, and
    /// discarding them here would reintroduce the same loss one layer up.
    pub candidate: BookFillCandidate,
    /// What the TokenContract said, in its own terms, that stopped this fill being offered.
    pub reason: String,
}

/// Every `InferenceFilled` the book emitted for one buyer note, split by what its `sellerTC` says now.

/// `candidates` are the fills whose TokenContract still names both parties and reports
/// `funded=true`; `refusals` are the fills it does not, each with the reason it was withheld.
/// Neither list is a recovery: a candidate can settle after this read, and an empty report is not
/// proof no deal exists, because it can only report the ext-out history the node still serves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookFillCandidateReport {
    pub candidates: Vec<BookFillCandidate>,
    pub refusals: Vec<BookFillCandidateRefusal>,
}

impl BookFillCandidateReport {
    /// How many `InferenceFilled` events named this note at all, confirmed or refused.

    /// This is the figure that separates "the book never named you" from "the book named you and
    /// nothing survives verification", which a length check on `candidates` alone cannot do.
    pub fn fills_named(&self) -> usize {
        self.candidates.len() + self.refusals.len()
    }
}

/// Why the named `TokenContract` does not confirm this fill, or `None` when it does.

/// This is the single place the verification decision is made; the yes/no predicate below is this
/// function with the reason discarded, so the two can never drift apart.
fn book_fill_candidate_refusal_reason(
    candidate: &BookFillCandidate,
    requested_buyer_note: &str,
    parties: Option<&TokenContractParties>,
    state: Option<&DealChainState>,
) -> Option<String> {
    let Some(parties) = parties else {
        return Some(
            "TokenContract.getParties returned no data; the address is absent or destroyed"
                .to_string(),
        );
    };
    let Some(state) = state else {
        return Some(
            "TokenContract.getState returned no data; the address is absent or destroyed"
                .to_string(),
        );
    };
    if !state.funded {
        return Some("TokenContract.getState reports funded=false".to_string());
    }
    if !candidate
        .buyer_note
        .eq_ignore_ascii_case(requested_buyer_note)
    {
        return Some(format!(
            "InferenceFilled names buyer note {}, not queried note {requested_buyer_note}",
            candidate.buyer_note
        ));
    }
    if !parties.buyer.eq_ignore_ascii_case(requested_buyer_note) {
        return Some(format!(
            "TokenContract.getParties names buyer {}, not queried note {requested_buyer_note}",
            parties.buyer
        ));
    }
    if !parties
        .seller_note
        .eq_ignore_ascii_case(&candidate.seller_note)
    {
        return Some(format!(
            "TokenContract.getParties names seller note {}, but InferenceFilled named {}",
            parties.seller_note, candidate.seller_note
        ));
    }
    None
}

/// The classifier above as a yes/no, for the tests that assert the decision rather than its wording.

/// Production reads the reason, so this has no caller outside tests; it stays because it is what
/// the existing verification test exercises, and routing it through the same classifier keeps that
/// test asserting the shipped decision instead of a second copy of it.
#[cfg(test)]
fn book_fill_candidate_is_verified(
    candidate: &BookFillCandidate,
    requested_buyer_note: &str,
    parties: Option<&TokenContractParties>,
    state: Option<&DealChainState>,
) -> bool {
    book_fill_candidate_refusal_reason(candidate, requested_buyer_note, parties, state).is_none()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct SellerOfferEvents {
    pub placed_order_id: Option<u128>,
    pub matched: bool,
    pub placement_value_returned: bool,
}

/// One successful owner-signed `PrivateNote.placeInferenceBuy` transaction, decoded from the note's
/// external-in message and backed by a non-aborted destination transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceInferenceBuyReceipt {
    pub message_id: String,
    pub created_at: u64,
    pub max_price_per_tick: u128,
    pub ticks: u128,
    pub escrow: u128,
}

/// The call a `TokenContract` executed in the transaction that emitted one of its settlement
/// receipts, decoded from that transaction's internal inbound message. This is what distinguishes
/// the buyer's `stop()` from the seller's `sellerStop()`, which share one `StreamStopped` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenContractInboundCall {
    /// Message id of the internal message that carried the call.
    pub message_id: String,
    /// Sender of that internal message: `msg.sender` as the contract's own guard saw it.
    pub source: String,
    /// `TokenContract` ABI function the message body decodes to.
    pub function: String,
}

impl TokenContractInboundCall {
    pub(super) fn is_buyer_stop_from(&self, buyer_note: &Address) -> bool {
        self.function == "stop"
            && normalize_addr(&self.source)
                .is_ok_and(|source| source.eq_ignore_ascii_case(&buyer_note.with_workchain()))
    }
}

/// One buyer STOP receipt together with the exact external message the client submitted for this
/// invocation. The external identity is retained until it can be bound through the PrivateNote
/// transaction to the internal `TokenContract.stop` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedBuyerStopReceipt {
    pub receipt: SettlementActionReceipt,
    pub client_message_id: String,
}

const SUBMITTED_BUYER_STOP_QUERY: &str = r#"
    query($hash: String!) {
      blockchain {
        message(hash: $hash) {
          id dst
          dst_transaction {
            status aborted
            out_msgs
          }
        }
      }
    }
"#;

pub(super) fn parse_submitted_buyer_stop_out_message_ids(
    response: &Value,
    expected_client_message_id: &str,
    buyer_note: &str,
) -> Result<Option<Vec<String>>> {
    let message = response
        .pointer("/data/blockchain/message")
        .ok_or_else(|| anyhow!("submitted buyer STOP GraphQL response shape changed"))?;
    if message.is_null() {
        return Ok(None);
    }
    let observed_client_message_id = message["id"]
        .as_str()
        .ok_or_else(|| anyhow!("submitted buyer STOP external message has no id"))?;
    if bare_hex(observed_client_message_id) != bare_hex(expected_client_message_id) {
        return Err(anyhow!(
            "submitted buyer STOP exact-hash lookup returned mismatched message id"
        ));
    }
    let destination = message["dst"]
        .as_str()
        .ok_or_else(|| anyhow!("submitted buyer STOP external message has no destination"))?;
    if bare_hex(destination) != bare_hex(buyer_note) {
        return Err(anyhow!(
            "submitted buyer STOP external message targeted {destination}, expected buyer note {buyer_note}"
        ));
    }

    let transaction = &message["dst_transaction"];
    if transaction.is_null() {
        return Ok(None);
    }
    let finalized = transaction["status"].as_i64() == Some(3)
        || transaction["status"].as_str() == Some("Finalized");
    if !finalized {
        return Ok(None);
    }
    if transaction["aborted"].as_bool() != Some(false) {
        return Err(anyhow!(
            "submitted buyer STOP external message did not produce a successful PrivateNote transaction"
        ));
    }
    let out_messages = transaction["out_msgs"]
        .as_array()
        .ok_or_else(|| anyhow!("submitted buyer STOP PrivateNote transaction has no out_msgs"))?;
    let message_ids = out_messages
        .iter()
        .map(|message_id| {
            let message_id = message_id.as_str().ok_or_else(|| {
                anyhow!("submitted buyer STOP PrivateNote transaction has a malformed out_msg id")
            })?;
            if message_id.is_empty() {
                return Err(anyhow!(
                    "submitted buyer STOP PrivateNote transaction has an empty out_msg id"
                ));
            }
            Ok(message_id.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(message_ids))
}

/// Destination identity recovered from one ABI-decoded RootPN `TokensWithdrawn` event body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokensWithdrawnEvent {
    /// Destination account in the contract ABI's workchain form (`0:<account_id>`).
    pub to: String,
    /// Destination DApp as one lowercase, zero-padded 256-bit hex component.
    pub dapp_id: String,
}

/// One ordered lifecycle/settlement event emitted by a `TokenContract`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenContractSettlementReceipt {
    pub message_id: String,
    pub created_at: u64,
    pub cursor: String,
    pub event: TokenContractSettlementEvent,
}

/// Exact ABI payload of a known `TokenContract` lifecycle/settlement event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenContractSettlementEvent {
    ContractDeployed {
        token_contract: String,
    },
    StreamFunded {
        buyer: String,
        deposit: u128,
    },
    SellerBondFunded {
        amount: u128,
    },
    /// the BUYER's bond, which the deal has always emitted (`TokenContract.sol` event
    /// `BuyerBondFunded(uint128 amount)`) and this decoder never read. It is not a detail: on the
    /// never-opened path the bond is folded back into the deposit and refunded WITH it
    /// (`_releaseBuyerBond`), so a reader that knows the deposit and not the bond sees a refund
    /// larger than anything it can account for and calls a correct settlement a divergence.
    BuyerBondFunded {
        amount: u128,
    },
    StreamOpened {
        buyer: String,
        price_per_tick: u128,
    },
    ProbeAccepted {
        buyer: String,
        to_seller: u128,
        bond_returned: u128,
    },
    ProbeBurned {
        buyer: String,
        burned_probe: u128,
        burned_bond: u128,
        refund_to_buyer: u128,
    },
    TickFinalized {
        finalized_owed: u128,
        deposit: u128,
    },
    TicksClaimed {
        trusted: u128,
        claimed: u128,
    },
    StreamStopped {
        buyer: String,
        to_seller: u128,
        refund_to_buyer: u128,
    },
    StreamDisputed {
        buyer: String,
        at: u64,
    },
    DisputeResolved {
        to_seller: u128,
        refund_to_buyer: u128,
        released: bool,
    },
    StreamReclaimed {
        buyer: String,
        refund_to_buyer: u128,
    },
    ShellWithdrawn {
        recipient: String,
        amount: u128,
    },
    ContractDestroyed {
        token_contract: String,
    },
}

/// Ordered lifecycle and settlement receipts emitted by one `TokenContract`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenContractSettlementReceipts {
    pub events: Vec<TokenContractSettlementReceipt>,
}

#[derive(Debug, Clone)]
pub struct TokenContractCurrentFacts {
    pub state: Value,
    pub fees: Value,
    pub deal: Value,
    pub parties: Value,
    pub seller: Value,
    pub version: Value,
}

#[derive(Debug, Clone)]
pub struct TokenContractReceiptChainData {
    pub account_id: String,
    pub account_active: bool,
    pub code_hash: Option<String>,
    pub current: Option<TokenContractCurrentFacts>,
    pub receipts: TokenContractSettlementReceipts,
    /// what the MONEY-REPORTING side said about this deal. A deal that ends through
    /// `cleanupUnopened` emits no settlement event -- only `ContractDestroyed` -- so its own ext-out
    /// cannot answer whether the escrow came back. The note that received it announces the figure
    /// (`PrivateNote.DealCredited`), and these are that announcement, restricted to credits naming
    /// THIS deal.
    pub note_credits: Vec<NoteDealCreditReceipt>,
    /// The note accounts actually read, so an empty `note_credits` can be told apart from a note
    /// that was never identified. Absence of evidence and absence of a reader are different answers
    /// and the receipt must not merge them.
    pub notes_read: Vec<String>,
}

/// One `DealCredited` a note emitted for this deal, with the same provenance the deal's own
/// settlement receipts carry, so both sides of the receipt are cited the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDealCreditReceipt {
    /// The note that announced the credit -- the party whose record grew.
    pub note: String,
    /// The deal named in the event, already normalized; equal to the receipt's TokenContract.
    pub deal: String,
    /// Raw ECC[2] SHELL credited.
    pub amount: u128,
    pub message_id: String,
    pub created_at: u64,
    pub cursor: String,
}

/// Caller-selected terms for one `PrivateNote.deployDeal` signed call.
#[derive(Debug, Clone, Copy)]
pub struct NoteDeployDealRequest<'a> {
    pub nonce: u64,
    pub model_name: &'a str,
    pub price_per_tick: u128,
    pub max_ticks: u128,
    pub gas_reserve: u128,
}

pub(super) async fn fetch_ext_out_page(
    gate: &RequestGate,
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    dapp_id: &str,
    page_size: u32,
    before: Option<&str>,
) -> Result<ExtOutPage> {
    // batch 1: this pager is the half of the live path that stayed unmetered. It dials through
    // our own reqwest client, so no amount of `.client()` conversion reaches it -- the gate has to be
    // handed in. One page, one admission.
    gate.admit().await;
    let gql = format!("{}/graphql", endpoint.trim_end_matches('/'));
    let query = r#"
        query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
          blockchain {
            account(account_id: $accountId, dapp_id: $dappId) {
              messages(msg_type: [ExtOut], last: $last, before: $before) {
                pageInfo { startCursor hasPreviousPage }
                edges { cursor node { id body created_at } }
              }
            }
          }
        }
    "#;
    let response = http
        .post(&gql)
        .json(&json!({
            "query": query,
            "variables": {
                "accountId": bare_hex(account_id),
                "dappId": bare_hex(dapp_id),
                "last": page_size,
                "before": before,
            },
        }))
        .send()
        .await?;
    let response: Value = chain_response_for_status(response).await?.json().await?;
    if let Some(errors) = response.get("errors") {
        // typed, so the READ predicate can recognise a pool timeout here too. This is the
        // reader that stopped the live mainnet run: mainnet answered HTTP 200 with
        // `pool timed out... path:["blockchain","account","messages"]`, and the flattened string
        // was additionally reported as "GraphQL shape changed" -- a transient named as a schema
        // drift, which sends the next reader hunting for an ABI change that does not exist.
        return Err(anyhow::Error::new(GraphQlBodyError::from_errors(errors))
            .context(format!("account {account_id} ext-out GraphQL errors")));
    }
    let page = response
        .pointer("/data/blockchain/account/messages")
        .ok_or_else(|| anyhow!("account {account_id} ext-out GraphQL shape changed: {response}"))?;
    let edges = page["edges"]
        .as_array()
        .ok_or_else(|| anyhow!("account {account_id} ext-out GraphQL edges missing: {response}"))?;
    let mut messages = Vec::with_capacity(edges.len());
    for edge in edges {
        let cursor = edge["cursor"]
            .as_str()
            .ok_or_else(|| anyhow!("account {account_id} ext-out event has no cursor"))?;
        let node = &edge["node"];
        let id = node["id"].as_str().ok_or_else(|| {
            anyhow!("account {account_id} ext-out event at cursor {cursor} has no message id")
        })?;
        let body = node["body"]
            .as_str()
            .ok_or_else(|| anyhow!("account {account_id} ext-out event {id} has no body"))?;
        let created_at = node["created_at"]
            .as_u64()
            .or_else(|| {
                node["created_at"]
                    .as_str()
                    .and_then(|value| value.parse().ok())
            })
            .ok_or_else(|| anyhow!("account {account_id} ext-out event has no created_at"))?;
        messages.push(ExtOutMessage {
            id: id.to_string(),
            created_at,
            cursor: cursor.to_string(),
            body: body.to_string(),
        });
    }
    Ok(ExtOutPage {
        messages,
        previous_cursor: previous_page_cursor(
            &format!("account {account_id} ext-out"),
            page,
            before,
        )?,
    })
}

pub(super) async fn fetch_all_ext_out_messages<T, F>(
    gate: &RequestGate,
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    filter_map: F,
) -> Result<Vec<T>>
where
    F: FnMut(ExtOutMessage) -> Result<Option<T>>,
{
    // The dapp-id lookup is a request of its own, so it takes a slot of its own.
    gate.admit().await;
    let dapp_id = fetch_dapp_id(http, endpoint, account_id).await?;
    fetch_all_ext_out_messages_routed(gate, http, endpoint, account_id, &dapp_id, filter_map).await
}

async fn fetch_all_ext_out_messages_routed<T, F>(
    gate: &RequestGate,
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    dapp_id: &str,
    filter_map: F,
) -> Result<Vec<T>>
where
    F: FnMut(ExtOutMessage) -> Result<Option<T>>,
{
    // Existing PR689 reader bound. R20-10 reuses the reader rather than defining a second pager.
    let mut before: Option<String> = None;
    let mut pages = Vec::new();
    loop {
        let page = fetch_ext_out_page(
            gate,
            http,
            endpoint,
            account_id,
            dapp_id,
            crate::params::EXT_OUT_PAGE_SIZE,
            before.as_deref(),
        )
        .await?;
        pages.push(page.messages);
        let Some(next) = page.previous_cursor else {
            break;
        };
        before = Some(next);
    }
    // Pages are fetched newest first; edges inside each page already carry chain order. Reverse
    // pages only. Message ids/cursors stay byte-for-byte opaque and are never parsed or sorted.
    filter_map_ext_out_messages_in_order(
        pages.into_iter().rev().flat_map(|page| page.into_iter()),
        filter_map,
    )
}

fn filter_map_ext_out_messages_in_order<T, F>(
    messages: impl IntoIterator<Item = ExtOutMessage>,
    mut filter_map: F,
) -> Result<Vec<T>>
where
    F: FnMut(ExtOutMessage) -> Result<Option<T>>,
{
    let mut by_id = BTreeMap::<String, ExtOutMessage>::new();
    let mut retained = Vec::new();
    for message in messages {
        if let Some(previous) = by_id.get(&message.id) {
            if previous != &message {
                return Err(anyhow!(
                    "ext-out message {} changed across overlapping pages",
                    message.id
                ));
            }
            continue;
        }
        by_id.insert(message.id.clone(), message.clone());
        if let Some(candidate) = filter_map(message)? {
            retained.push(candidate);
        }
    }
    Ok(retained)
}

fn dedupe_ext_out_messages_in_order(
    messages: impl IntoIterator<Item = ExtOutMessage>,
) -> Result<Vec<ExtOutMessage>> {
    filter_map_ext_out_messages_in_order(messages, |message| Ok(Some(message)))
}

fn decode_token_contract_settlement_receipts(
    messages: Vec<ExtOutMessage>,
) -> Result<TokenContractSettlementReceipts> {
    let messages = dedupe_ext_out_messages_in_order(messages)?;
    let mut receipts = TokenContractSettlementReceipts::default();
    for message in messages {
        let Some(event) = decode_token_contract_settlement_event(&message.body)
            .with_context(|| format!("decode TokenContract event {}", message.id))?
        else {
            continue;
        };
        receipts.events.push(TokenContractSettlementReceipt {
            message_id: message.id,
            created_at: message.created_at,
            cursor: message.cursor,
            event,
        });
    }
    Ok(receipts)
}

fn decode_token_contract_settlement_event(
    body_b64: &str,
) -> Result<Option<TokenContractSettlementEvent>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match tvm_types::SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match tvm_abi::Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = tvm_abi::Contract::load(TOKENCONTRACT_ABI.as_bytes())
        .map_err(|error| anyhow!("load TokenContract ABI: {error}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    let name = event.name.clone();
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode {name} body: {error}"))?;
    let required_u128 = |field| {
        decoded_u128(&tokens, field)
            .ok_or_else(|| anyhow!("{name} body missing or invalid {field}"))
    };
    let required_u64 = |field| {
        decoded_u64(&tokens, field).ok_or_else(|| anyhow!("{name} body missing or invalid {field}"))
    };
    let required_address = |field| {
        decoded_address(&tokens, field)
            .ok_or_else(|| anyhow!("{name} body missing or invalid {field}"))
    };
    let required_bool = |field| {
        decoded_bool(&tokens, field)
            .ok_or_else(|| anyhow!("{name} body missing or invalid {field}"))
    };
    Ok(Some(match name.as_str() {
        "ContractDeployed" => TokenContractSettlementEvent::ContractDeployed {
            token_contract: required_address("self")?,
        },
        "StreamFunded" => TokenContractSettlementEvent::StreamFunded {
            buyer: required_address("buyer")?,
            deposit: required_u128("deposit")?,
        },
        "SellerBondFunded" => TokenContractSettlementEvent::SellerBondFunded {
            amount: required_u128("amount")?,
        },
        "BuyerBondFunded" => TokenContractSettlementEvent::BuyerBondFunded {
            amount: required_u128("amount")?,
        },
        "StreamOpened" => TokenContractSettlementEvent::StreamOpened {
            buyer: required_address("buyer")?,
            price_per_tick: required_u128("pricePerTick")?,
        },
        "ProbeAccepted" => TokenContractSettlementEvent::ProbeAccepted {
            buyer: required_address("buyer")?,
            to_seller: required_u128("toSeller")?,
            bond_returned: required_u128("bondReturned")?,
        },
        "ProbeBurned" => TokenContractSettlementEvent::ProbeBurned {
            buyer: required_address("buyer")?,
            burned_probe: required_u128("burnedProbe")?,
            burned_bond: required_u128("burnedBond")?,
            refund_to_buyer: required_u128("refundToBuyer")?,
        },
        "TickFinalized" => TokenContractSettlementEvent::TickFinalized {
            finalized_owed: required_u128("finalizedOwed")?,
            deposit: required_u128("deposit")?,
        },
        "TicksClaimed" => TokenContractSettlementEvent::TicksClaimed {
            trusted: required_u128("trusted")?,
            claimed: required_u128("claimed")?,
        },
        "StreamStopped" => TokenContractSettlementEvent::StreamStopped {
            buyer: required_address("buyer")?,
            to_seller: required_u128("toSeller")?,
            refund_to_buyer: required_u128("refundToBuyer")?,
        },
        "StreamDisputed" => TokenContractSettlementEvent::StreamDisputed {
            buyer: required_address("buyer")?,
            at: required_u64("at")?,
        },
        "DisputeResolved" => TokenContractSettlementEvent::DisputeResolved {
            to_seller: required_u128("toSeller")?,
            refund_to_buyer: required_u128("refundToBuyer")?,
            released: required_bool("released")?,
        },
        "StreamReclaimed" => TokenContractSettlementEvent::StreamReclaimed {
            buyer: required_address("buyer")?,
            refund_to_buyer: required_u128("refundToBuyer")?,
        },
        "ShellWithdrawn" => TokenContractSettlementEvent::ShellWithdrawn {
            recipient: required_address("recipient")?,
            amount: required_u128("amount")?,
        },
        "ContractDestroyed" => TokenContractSettlementEvent::ContractDestroyed {
            token_contract: required_address("self")?,
        },
        _ => return Ok(None),
    }))
}

/// Decode one ext-out body as the current compiled RootPN `TokensWithdrawn` event.

/// A body for another event is skipped. Once the selector identifies `TokensWithdrawn`, ABI drift
/// or a malformed body fails loud rather than returning a partial destination identity.
pub fn decode_tokens_withdrawn_event(body_b64: &str) -> Result<Option<TokensWithdrawnEvent>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match tvm_types::SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match tvm_abi::Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = tvm_abi::Contract::load(ROOTPN_ABI.as_bytes())
        .map_err(|error| anyhow!("load RootPN ABI: {error}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    if event.name != "TokensWithdrawn" {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode TokensWithdrawn body: {error}"))?;
    let to = decoded_address(&tokens, "to")
        .ok_or_else(|| anyhow!("TokensWithdrawn body missing or invalid to"))?;
    let dapp_id = tokens
        .iter()
        .find_map(|token| {
            if token.name != "dapp_id" {
                return None;
            }
            match &token.value {
                tvm_abi::token::TokenValue::Uint(value) => {
                    Some(format!("{:0>64}", value.number.to_str_radix(16)))
                }
                _ => None,
            }
        })
        .ok_or_else(|| anyhow!("TokensWithdrawn body missing or invalid dapp_id"))?;
    Ok(Some(TokensWithdrawnEvent { to, dapp_id }))
}

/// One finalized queue event from a canonical multisig.

/// Two of the wallet's events answer the only question that separates a funding request which moved
/// money from one which never did:

/// - `TransactionSubmitted` is emitted when a request is QUEUED. It carries the queue id and, on
/// the message that delivered it, the chain time it happened at - which is what makes an expiry
/// verdict a chain fact rather than a local timer, and what recovers the queue id for a client
/// whose own submit receipt was never observed.
/// - `TransactionSent` is emitted when a queued request is confirmed and actually sent. Its presence
/// in finalized history is a POSITIVE proof of execution.

/// Without both, a request that has left `getTransactions` is indistinguishable from one that
/// expired, and those two are opposite in money terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultisigQueueEvent {
    /// The request entered the queue.
    Submitted {
        transaction_id: u64,
        dest: String,
        value: u128,
        dapp_id: String,
    },
    /// The request left the queue by executing.
    Sent {
        transaction_id: u64,
        dest: String,
        value: u128,
        send_flags: u64,
        bounce: bool,
        dapp_id: String,
    },
}

impl MultisigQueueEvent {
    /// The queue id the event is about.
    pub fn transaction_id(&self) -> u64 {
        match self {
            Self::Submitted { transaction_id, .. } | Self::Sent { transaction_id, .. } => {
                *transaction_id
            }
        }
    }
}

/// One queue event as finalized history delivered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultisigQueueRecord {
    pub event: MultisigQueueEvent,
    /// The finalized ext-out message that carried it.
    pub message_id: String,
    /// The chain time that message was created at.
    pub created_at: u64,
}

fn decoded_uint256_hex(tokens: &[tvm_abi::Token], name: &str) -> Option<String> {
    tokens.iter().find_map(|token| {
        if token.name != name {
            return None;
        }
        match &token.value {
            tvm_abi::token::TokenValue::Uint(value) => {
                Some(format!("{:0>64}", value.number.to_str_radix(16)))
            }
            _ => None,
        }
    })
}

/// Decode one ext-out body as a canonical multisig queue event.

/// A body for any other event is skipped. Once the selector identifies one of the two, ABI drift or
/// a malformed body fails loud rather than returning a partial claim - a half-read execution proof
/// is worse than none, because it is the fact that forbids a second transfer.
pub fn decode_multisig_queue_event(body_b64: &str) -> Result<Option<MultisigQueueEvent>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match tvm_types::SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match tvm_abi::Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = tvm_abi::Contract::load(crate::canonical_multisig::MULTISIG_ABI_JSON.as_bytes())
        .map_err(|error| anyhow!("load canonical multisig ABI: {error}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    let name = event.name.clone();
    if name != "TransactionSubmitted" && name != "TransactionSent" {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode {name} body: {error}"))?;
    let transaction_id = decoded_u64(&tokens, "transactionId")
        .ok_or_else(|| anyhow!("{name} body missing or invalid transactionId"))?;
    let dest = decoded_address(&tokens, "dest")
        .ok_or_else(|| anyhow!("{name} body missing or invalid dest"))?;
    let value = decoded_u128(&tokens, "value")
        .ok_or_else(|| anyhow!("{name} body missing or invalid value"))?;
    let dapp_id = decoded_uint256_hex(&tokens, "dapp_id")
        .ok_or_else(|| anyhow!("{name} body missing or invalid dapp_id"))?;
    if name == "TransactionSubmitted" {
        return Ok(Some(MultisigQueueEvent::Submitted {
            transaction_id,
            dest,
            value,
            dapp_id,
        }));
    }
    let send_flags = decoded_u64(&tokens, "sendFlags")
        .ok_or_else(|| anyhow!("TransactionSent body missing or invalid sendFlags"))?;
    let bounce = decoded_bool(&tokens, "bounce")
        .ok_or_else(|| anyhow!("TransactionSent body missing or invalid bounce"))?;
    Ok(Some(MultisigQueueEvent::Sent {
        transaction_id,
        dest,
        value,
        send_flags,
        bounce,
        dapp_id,
    }))
}

/// Every queue event in a multisig's own finalized ext-out history, in chain order.

/// Read over the account's COMPLETE ext-out stream rather than a recent window, so "not found"
/// cannot quietly mean "not looked far enough back" - which, for an execution proof, would be the
/// difference between refusing a second transfer and making one.
pub async fn read_multisig_queue_history(
    ceiling: ChainRequestCeiling,
    http: &reqwest::Client,
    endpoint: &str,
    multisig_account_id: &str,
    multisig_dapp_id: &str,
) -> Result<Vec<MultisigQueueRecord>> {
    // review finding 1: this used to build its own `Unlimited` gate, which bypassed the
    // instance ceiling from inside core where nothing could see it. The ceiling is now the caller's
    // to state, so the decision is visible at the call site and greppable, instead of buried here.
    let gate = RequestGate::new(ceiling);
    fetch_all_ext_out_messages_routed(
        &gate,
        http,
        endpoint,
        multisig_account_id,
        multisig_dapp_id,
        |message| {
            let Some(event) = decode_multisig_queue_event(&message.body)? else {
                return Ok(None);
            };
            Ok(Some(MultisigQueueRecord {
                event,
                message_id: message.id,
                created_at: message.created_at,
            }))
        },
    )
    .await
}

/// The transaction that emitted one ext-out message, and every message that transaction produced.

/// `out_msgs` is a list of message ids. The richer `out_messages { id dst }` projection is NOT
/// reachable: the dexdo read surface rejects it, so each sibling's destination is resolved by its
/// own exact-hash lookup below.
const SOURCE_TRANSACTION_OUT_MESSAGES_QUERY: &str = r#"
    query($hash: String!) {
      blockchain {
        message(hash: $hash) {
          id
          src_transaction { id out_msgs }
        }
      }
    }
"#;

/// One message's destination, by exact hash.
const MESSAGE_DESTINATION_QUERY: &str = r#"
    query($hash: String!) {
      blockchain {
        message(hash: $hash) { id dst }
      }
    }
"#;

/// The ids of every message emitted by the transaction that emitted `expected_message_id`.

/// `None` means the anchor itself is not readable yet - the message is unknown to the index, or its
/// emitting transaction is not attached to it. That is absence of evidence, never evidence that the
/// transaction produced nothing.
pub fn parse_source_transaction_out_messages(
    response: &Value,
    expected_message_id: &str,
) -> Result<Option<Vec<String>>> {
    let message = response
        .pointer("/data/blockchain/message")
        .ok_or_else(|| anyhow!("multisig delivery anchor GraphQL response shape changed"))?;
    if message.is_null() {
        return Ok(None);
    }
    let observed = message["id"]
        .as_str()
        .ok_or_else(|| anyhow!("multisig delivery anchor message has no id"))?;
    if bare_hex(observed) != bare_hex(expected_message_id) {
        return Err(anyhow!(
            "multisig delivery anchor exact-hash lookup returned mismatched message id"
        ));
    }
    let transaction = &message["src_transaction"];
    if transaction.is_null() {
        return Ok(None);
    }
    let out_messages = transaction["out_msgs"]
        .as_array()
        .ok_or_else(|| anyhow!("multisig delivery anchor source transaction has no out_msgs"))?;
    out_messages
        .iter()
        .map(|id| {
            let id = id
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "multisig delivery anchor source transaction has a malformed out_msg id"
                    )
                })?;
            Ok(id.to_string())
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// One message's destination account, by exact hash. `None` when the index does not know it.
pub fn parse_message_destination(
    response: &Value,
    expected_message_id: &str,
) -> Result<Option<String>> {
    let message = response
        .pointer("/data/blockchain/message")
        .ok_or_else(|| anyhow!("multisig delivery sibling GraphQL response shape changed"))?;
    if message.is_null() {
        return Ok(None);
    }
    let observed = message["id"]
        .as_str()
        .ok_or_else(|| anyhow!("multisig delivery sibling message has no id"))?;
    if bare_hex(observed) != bare_hex(expected_message_id) {
        return Err(anyhow!(
            "multisig delivery sibling exact-hash lookup returned mismatched message id"
        ));
    }
    match message["dst"].as_str() {
        Some(destination) if !destination.trim().is_empty() => Ok(Some(destination.to_string())),
        _ => Ok(None),
    }
}

/// Which of an emitting transaction's sibling messages carried the transfer to `destination`.

/// Exactly one, or nothing. Two siblings to the same destination in one transaction do not identify
/// a delivery, and neither does none.
pub fn sole_delivery_sibling(
    siblings: &[(String, Option<String>)],
    anchor_message_id: &str,
    destination_account_id: &str,
) -> Option<String> {
    let expected = bare_hex(destination_account_id);
    let mut matched = siblings.iter().filter(|(id, destination)| {
        bare_hex(id) != bare_hex(anchor_message_id)
            && destination
                .as_deref()
                .is_some_and(|destination| bare_hex(destination) == expected)
    });
    let first = matched.next()?;
    match matched.next() {
        Some(_) => None,
        None => Some(first.0.clone()),
    }
}

/// The internal message that carried an EXECUTED multisig queue transfer to its destination, proven
/// by the destination's own finalized receipt.

/// # Why the event's own message id is not that proof

/// `TransactionSent` is an ext-out EVENT message. The wallet emits it on a hardcoded event channel,
/// so the event message's own `dst` is that channel and never the transfer's destination. What binds
/// the two is that the queued path performs `txn.dest.transfer(...)` and then
/// `emit TransactionSent(...)` inside ONE Vault transaction: the transfer and the event are two
/// out-messages of the same transaction. So the event is an anchor to that transaction, and the
/// delivery is the sibling out-message addressed to the destination.

/// # What `None` means

/// Not "no delivery". It means this client cannot yet name the delivery from chain fact: the anchor
/// is not indexed, its transaction is not attached, no sibling is addressed to the destination, more
/// than one is, or the destination's receipt is not finalized. A caller must treat every one of
/// those as unknown. An aggregated balance that grew by the expected amount can establish that funds
/// are sufficient to spend; it can never establish that THIS transfer is what delivered them,
/// because an unrelated incoming transfer produces exactly the same growth.
pub async fn prove_multisig_delivery_message(
    http: &reqwest::Client,
    endpoint: &str,
    sent_event_message_id: &str,
    destination_account_id: &str,
    destination_dapp_id: &str,
) -> Result<Option<String>> {
    let gql = format!("{}/graphql", endpoint.trim_end_matches('/'));
    let anchor = post_message_query(
        http,
        &gql,
        SOURCE_TRANSACTION_OUT_MESSAGES_QUERY,
        sent_event_message_id,
        "multisig delivery anchor",
    )
    .await?;
    let Some(out_messages) = parse_source_transaction_out_messages(&anchor, sent_event_message_id)?
    else {
        return Ok(None);
    };

    let mut siblings = Vec::with_capacity(out_messages.len());
    for id in out_messages {
        if bare_hex(&id) == bare_hex(sent_event_message_id) {
            siblings.push((id, None));
            continue;
        }
        let response = post_message_query(
            http,
            &gql,
            MESSAGE_DESTINATION_QUERY,
            &id,
            "multisig delivery sibling",
        )
        .await?;
        let destination = parse_message_destination(&response, &id)?;
        siblings.push((id, destination));
    }
    let Some(delivery) =
        sole_delivery_sibling(&siblings, sent_event_message_id, destination_account_id)
    else {
        return Ok(None);
    };

    // The exact-hash destination receipt reader this client already has, and the only one: it binds
    // the message to a FINALIZED destination transaction at the expected account and DApp, and
    // refuses a receipt whose destination, transaction account or DApp is anything else.
    let response = query_exact_destination_receipt(
        http,
        endpoint,
        destination_account_id,
        destination_dapp_id,
        &delivery,
    )
    .await?;
    if let Some(errors) = response.get("errors") {
        return Err(anyhow!(
            "multisig delivery receipt GraphQL errors for message {delivery}: {errors}"
        ));
    }
    let receipt = parse_exact_destination_receipt(
        &response,
        destination_account_id,
        destination_dapp_id,
        &delivery,
    )
    .map_err(|error| {
        error.context(format!(
            "prove the multisig delivery {delivery} landed on {destination_account_id}"
        ))
    })?;
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    // An aborted destination transaction delivered nothing. Reported as unproven rather than as an
    // error: the caller's only safe response to both is the same, and a bounced transfer is a fact
    // about this chain rather than about this client.
    if receipt.aborted != Some(false) {
        return Ok(None);
    }
    Ok(Some(delivery))
}

/// One exact-hash message read, retried the way every other chain read is.

/// second half. `prove_multisig_delivery_message` reaches its receipt read through TWO of
/// these -- the anchor (`:5552`) and one per sibling (`:5572`) -- and both posted directly. Putting
/// the retry only on the receipt read would have made the release note half true: the same money
/// path would still die on a `502`, one step earlier.

/// Pure, on the same terms as [`query_exact_destination_receipt`]: `query` is `&'static str` and its
/// only two arguments are [`SOURCE_TRANSACTION_OUT_MESSAGES_QUERY`] and
/// [`MESSAGE_DESTINATION_QUERY`], both GraphQL **queries** over `blockchain.message`. Nothing is
/// submitted, so nothing can be submitted twice.

/// The `errors` check below stays OUTSIDE the retry, deliberately. A GraphQL failure carried in a
/// 200 body is not typed here, so `is_transient_read_failure` cannot recognise it either way, and
/// this change is not the place to alter that -- it is a separate defect of a different shape.

/// The request stays inside this function rather than moving to a `..._once` helper: that helper
/// would take `http: &reqwest::Client` and `ci/check_client_bypass_ratchet.sh` counts the signature
/// against a ceiling that is an equality. Same reason as the receipt read above.
async fn post_message_query(
    http: &reqwest::Client,
    gql: &str,
    query: &'static str,
    message_id: &str,
    what: &str,
) -> Result<Value> {
    let response: Value = retry_transient_read(|| async {
        let response = http
            .post(gql)
            .json(&json!({
                "query": query,
                "variables": { "hash": bare_hex(message_id) },
            }))
            .send()
            .await?;
        let response: Value = chain_response_for_status(response).await?.json().await?;
        Ok(response)
    })
    .await?;
    if let Some(errors) = response.get("errors") {
        return Err(anyhow!("{what} GraphQL errors for {message_id}: {errors}"));
    }
    Ok(response)
}

/// The chain's own clock, in unix seconds.

/// Exposed because an expiry verdict has to be taken against chain time. A local clock is not chain
/// evidence: a machine whose clock runs fast would conclude that a request it can still confirm has
/// expired, and act on that by creating a second one.
pub async fn chain_time_secs(http: &reqwest::Client, endpoint: &str) -> Result<u64> {
    fetch_chain_time_secs(http, endpoint).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedSettlementEvent {
    BuyerStop,
    ProbeBurned,
    StreamStopped,
    StreamDisputed,
    DisputeResolved { released: bool },
}

impl ExpectedSettlementEvent {
    fn resolve(self, pre_state: DealChainState) -> Self {
        match self {
            Self::BuyerStop if pre_state.on_probe() => Self::ProbeBurned,
            Self::BuyerStop => Self::StreamStopped,
            exact => exact,
        }
    }
}

fn settlement_action_event_kind(event: &TokenContractSettlementEvent) -> Option<&'static str> {
    match event {
        TokenContractSettlementEvent::ProbeBurned { .. } => Some("ProbeBurned"),
        TokenContractSettlementEvent::StreamStopped { .. } => Some("StreamStopped"),
        TokenContractSettlementEvent::StreamDisputed { .. } => Some("StreamDisputed"),
        TokenContractSettlementEvent::DisputeResolved { released: true, .. } => {
            Some("DisputeResolved(released=true)")
        }
        TokenContractSettlementEvent::DisputeResolved {
            released: false, ..
        } => Some("DisputeResolved(released=false)"),
        TokenContractSettlementEvent::ProbeAccepted { .. }
        | TokenContractSettlementEvent::ContractDeployed { .. }
        | TokenContractSettlementEvent::StreamFunded { .. }
        | TokenContractSettlementEvent::SellerBondFunded { .. }
        | TokenContractSettlementEvent::BuyerBondFunded { .. }
        | TokenContractSettlementEvent::StreamOpened { .. }
        | TokenContractSettlementEvent::StreamReclaimed { .. }
        | TokenContractSettlementEvent::ShellWithdrawn { .. }
        | TokenContractSettlementEvent::ContractDestroyed { .. }
        | TokenContractSettlementEvent::TickFinalized { .. }
        | TokenContractSettlementEvent::TicksClaimed { .. } => None,
    }
}

fn select_prior_buyer_terminal_receipt(
    token_contract: &str,
    expected_buyer: &str,
    receipts: &TokenContractSettlementReceipts,
) -> Result<Option<BuyerStopTerminalReceipt>> {
    let actions = receipts
        .events
        .iter()
        .filter(|receipt| settlement_action_event_kind(&receipt.event).is_some())
        .collect::<Vec<_>>();
    let stops = actions
        .iter()
        .copied()
        .filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::ProbeBurned { .. }
                    | TokenContractSettlementEvent::StreamStopped { .. }
            )
        })
        .collect::<Vec<_>>();
    if stops.is_empty() {
        return Ok(None);
    }
    if actions.len() != 1 || stops.len() != 1 {
        return Err(anyhow!(
            "TokenContract {token_contract} prior settlement history contains a terminal close event mixed \
             with another action; refusing local terminal reconciliation"
        ));
    }

    let receipt = stops[0];
    let (buyer, event) = match &receipt.event {
        TokenContractSettlementEvent::ProbeBurned {
            buyer,
            burned_probe,
            burned_bond,
            refund_to_buyer,
        } => (
            buyer,
            SettlementActionEvent::ProbeBurned {
                buyer: buyer.clone(),
                burned_probe: (*burned_probe).into(),
                burned_bond: (*burned_bond).into(),
                refund_to_buyer: (*refund_to_buyer).into(),
            },
        ),
        TokenContractSettlementEvent::StreamStopped {
            buyer,
            to_seller,
            refund_to_buyer,
        } => (
            buyer,
            SettlementActionEvent::StreamStopped {
                buyer: buyer.clone(),
                to_seller: (*to_seller).into(),
                refund_to_buyer: (*refund_to_buyer).into(),
            },
        ),
        _ => unreachable!("stops contains only terminal buyer-bearing events"),
    };
    let observed = normalize_addr(buyer).with_context(|| {
        format!(
            "TokenContract {token_contract} prior terminal receipt has malformed buyer beneficiary {buyer}"
        )
    })?;
    let expected = normalize_addr(expected_buyer).with_context(|| {
        format!(
            "TokenContract {token_contract} has malformed expected buyer actor {expected_buyer}"
        )
    })?;
    if observed != expected {
        return Err(anyhow!(
            "TokenContract {token_contract} prior terminal receipt beneficiary {observed} does not \
             match expected buyer note {expected}; refusing local reconciliation"
        ));
    }

    Ok(Some(BuyerStopTerminalReceipt {
        token_contract: token_contract.to_string(),
        fact: BuyerStopTerminalFact::AlreadyClosed,
        stop_submitted: false,
        message_id: receipt.message_id.clone(),
        created_at: receipt.created_at,
        event,
        pre_bonds: None,
        post_state: None,
    }))
}

fn reject_prior_settlement_action(
    token_contract: &str,
    action: SettlementAction,
    expected_buyer: Option<&str>,
    receipts: &TokenContractSettlementReceipts,
) -> Result<()> {
    let token_contract = display_token_contract(token_contract);
    let actions = receipts
        .events
        .iter()
        .filter(|receipt| settlement_action_event_kind(&receipt.event).is_some())
        .collect::<Vec<_>>();

    let resolutions = actions
        .iter()
        .copied()
        .filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::DisputeResolved { .. }
            )
        })
        .collect::<Vec<_>>();
    let disputes = actions
        .iter()
        .copied()
        .filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::StreamDisputed { .. }
            )
        })
        .collect::<Vec<_>>();
    let stops = actions
        .iter()
        .copied()
        .filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::ProbeBurned { .. }
                    | TokenContractSettlementEvent::StreamStopped { .. }
            )
        })
        .collect::<Vec<_>>();

    if matches!(
        action,
        SettlementAction::ReleaseDispute | SettlementAction::ResolveDisputeTimeout
    ) {
        let canonical_open_dispute =
            actions.len() == 1 && disputes.len() == 1 && resolutions.is_empty() && stops.is_empty();
        let canonical_resolution = actions.len() == 2
            && disputes.len() == 1
            && resolutions.len() == 1
            && stops.is_empty()
            && matches!(
                actions[0].event,
                TokenContractSettlementEvent::StreamDisputed { .. }
            )
            && matches!(
                actions[1].event,
                TokenContractSettlementEvent::DisputeResolved { .. }
            );
        if !canonical_open_dispute && !canonical_resolution {
            return Err(anyhow!(
                "TokenContract {token_contract} action {action} has invalid prior settlement-action \
                 history: expected exactly StreamDisputed, optionally followed by one \
                 DisputeResolved in canonical chain order; refusing before any money POST"
            ));
        }
    }

    let ambiguous = resolutions.len() > 1
        || disputes.len() > 1
        || stops.len() > 1
        || (!matches!(
            action,
            SettlementAction::ReleaseDispute | SettlementAction::ResolveDisputeTimeout
        ) && actions.len() > 1);
    if ambiguous {
        return Err(anyhow!(
            "TokenContract {token_contract} has more than one prior settlement-action receipt; \
             refusing action {action} before any money POST"
        ));
    }

    let exact = match action {
        SettlementAction::BuyerStop => stops.first().copied(),
        SettlementAction::SellerStop => stops.first().copied().filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::StreamStopped { .. }
            )
        }),
        SettlementAction::Dispute => {
            if stops.is_empty() && resolutions.is_empty() {
                disputes.first().copied()
            } else {
                None
            }
        }
        SettlementAction::ReleaseDispute => resolutions.first().copied().filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::DisputeResolved { released: true, .. }
            )
        }),
        SettlementAction::ResolveDisputeTimeout => resolutions.first().copied().filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::DisputeResolved {
                    released: false,
                    ..
                }
            )
        }),
    };

    if let Some(receipt) = exact {
        if let Some(expected_buyer) = expected_buyer {
            let observed_buyer = match &receipt.event {
                TokenContractSettlementEvent::ProbeBurned { buyer, .. }
                | TokenContractSettlementEvent::StreamStopped { buyer, .. }
                | TokenContractSettlementEvent::StreamDisputed { buyer, .. } => Some(buyer),
                TokenContractSettlementEvent::DisputeResolved { .. }
                | TokenContractSettlementEvent::ContractDeployed { .. }
                | TokenContractSettlementEvent::StreamFunded { .. }
                | TokenContractSettlementEvent::SellerBondFunded { .. }
                | TokenContractSettlementEvent::BuyerBondFunded { .. }
                | TokenContractSettlementEvent::StreamOpened { .. }
                | TokenContractSettlementEvent::StreamReclaimed { .. }
                | TokenContractSettlementEvent::ShellWithdrawn { .. }
                | TokenContractSettlementEvent::ContractDestroyed { .. }
                | TokenContractSettlementEvent::ProbeAccepted { .. }
                | TokenContractSettlementEvent::TickFinalized { .. }
                | TokenContractSettlementEvent::TicksClaimed { .. } => None,
            };
            if let Some(observed_buyer) = observed_buyer {
                let expected = normalize_addr(expected_buyer)?;
                let observed = normalize_addr(observed_buyer)?;
                if expected != observed {
                    let expected = display_dexdo_address(expected);
                    let observed = display_dexdo_address(observed);
                    return Err(anyhow!(
                        "TokenContract {token_contract} has prior action {action} receipt with buyer \
                         actor {observed}, expected {expected}; refusing before any money POST"
                    ));
                }
            }
        }
        let kind = settlement_action_event_kind(&receipt.event)
            .expect("exact prior action is a settlement-action event");
        return Err(anyhow!(
            "TokenContract {token_contract} action {action} is already recorded by exact {kind} \
             receipt message_id={} created_at={}; treating this retry as an idempotent no-op and \
             refusing a duplicate money POST",
            receipt.message_id,
            receipt.created_at
        ));
    }

    let incompatible = match action {
        SettlementAction::ReleaseDispute | SettlementAction::ResolveDisputeTimeout
            if resolutions.is_empty() && stops.is_empty() =>
        {
            None
        }
        _ => actions.last().copied(),
    };
    if let Some(receipt) = incompatible {
        let kind = settlement_action_event_kind(&receipt.event)
            .expect("incompatible prior action is a settlement-action event");
        return Err(anyhow!(
            "TokenContract {token_contract} action {action} has incompatible prior {kind} receipt \
             message_id={} created_at={}; refusing before any money POST",
            receipt.message_id,
            receipt.created_at
        ));
    }

    Ok(())
}

fn validate_buyer_stop_pre_state(
    token_contract: &str,
    pre: Option<&DealChainSnapshot>,
    receipts: &TokenContractSettlementReceipts,
) -> Result<()> {
    let token_contract = display_token_contract(token_contract);
    // Immutable action history wins over a potentially stale-open getter. Checking the getter first
    // would let a restarted process POST a second STOP after the terminal event had already landed.
    reject_prior_settlement_action(&token_contract, SettlementAction::BuyerStop, None, receipts)?;

    if pre.is_some_and(|snapshot| snapshot.state.opened && !snapshot.state.disputed) {
        return Ok(());
    }

    match pre {
        None => Err(anyhow!(
            "TokenContract {token_contract} is inactive and has no exact terminal receipt; \
             refusing buyer STOP before any money POST"
        )),
        Some(snapshot) if snapshot.state.disputed => Err(anyhow!(
            "TokenContract {token_contract} is disputed; refusing buyer STOP before any money POST"
        )),
        Some(snapshot) => Err(anyhow!(
            "TokenContract {token_contract} is not an open, undisputed stream \
             (funded={} opened={} disputed={} deposit={} probeTick={}); refusing buyer STOP before \
             any money POST",
            snapshot.state.funded,
            snapshot.state.opened,
            snapshot.state.disputed,
            snapshot.state.deposit,
            snapshot.state.probe_tick
        )),
    }
}

/*
 * Keep settlement replay classification centralized above. Every caller performs it both before
 * signing/preparing a money message and again immediately before the coherent state/actor reads.
 */

fn settlement_confirmation_delay(
    elapsed: std::time::Duration,
    confirmation_timeout: std::time::Duration,
    confirmation_poll: std::time::Duration,
) -> Option<std::time::Duration> {
    let remaining = confirmation_timeout.checked_sub(elapsed)?;
    (!remaining.is_zero()).then(|| remaining.min(confirmation_poll))
}

fn validate_settlement_facts(token_contract: &str, facts: &DealChainSnapshot) -> Result<()> {
    let token_contract = display_token_contract(token_contract);
    if facts.seller_bond.bond_held > facts.seller_bond.bond_required {
        return Err(anyhow!(
            "TokenContract {token_contract} getSellerBond contradiction: held {} exceeds required {}",
            facts.seller_bond.bond_held,
            facts.seller_bond.bond_required
        ));
    }
    // `getBuyerBond()` returns `(_buyerBond, _isSubscription() ? _bondAmount(): 0)`
    // (`contracts/airegistry/TokenContract.sol:2119-2121`). The two halves answer DIFFERENT
    // questions: `bondHeld` is what is actually held, `bondRequired` is what this deal SHAPE
    // mandates. Comparing them is only meaningful where the second is a real requirement.

    // On a subscription it is: `_bondAmount()` is `2 * _pricePerTick` (`:554-556`), the 2P mirror,
    // so holding more than that is a genuine contradiction and stays checked here.

    // On an ordinary deal `bondRequired` is a hard `0` in that ternary -- not a balance, not a
    // residual, nothing that can ever be otherwise -- so `held > required` degenerates to
    // `held > 0` and asserts that ordinary buyers never post a bond. 4.0.35 contradicts that by
    // construction: an ordinary buyer's bond is real and non-zero, and this gate aborted the
    // settlement POST before `streamStop` was ever submitted.
    if facts.subscription.is_subscription()
        && facts.buyer_bond.bond_held > facts.buyer_bond.bond_required
    {
        return Err(anyhow!(
            "subscription TokenContract {token_contract} getBuyerBond contradiction: held {} exceeds required {}",
            facts.buyer_bond.bond_held,
            facts.buyer_bond.bond_required
        ));
    }
    // The ONLY incoherence `getBuyerBond()` can report on an ordinary deal: the contract cannot
    // return a non-zero requirement there, so a non-zero one is not this contract answering.
    // `bond_held` is deliberately NOT asserted -- it is unconstrained on this shape.
    if !facts.subscription.is_subscription() && facts.buyer_bond.bond_required != 0 {
        return Err(anyhow!(
            "ordinary TokenContract {token_contract} reports a non-zero buyer bond requirement: {}",
            facts.buyer_bond.bond_required
        ));
    }
    if facts.subscription.is_subscription() && facts.buyer_bond.bond_required == 0 {
        return Err(anyhow!(
            "subscription TokenContract {token_contract} exposes zero buyer bond requirement"
        ));
    }
    Ok(())
}

fn settlement_bond_state(facts: &DealChainSnapshot) -> SettlementActionBondState {
    SettlementActionBondState {
        seller_bond_held: facts.seller_bond.bond_held.into(),
        seller_bond_required: facts.seller_bond.bond_required.into(),
        buyer_bond_held: facts.buyer_bond.bond_held.into(),
        buyer_bond_required: facts.buyer_bond.bond_required.into(),
    }
}

fn settlement_action_post_state(
    token_contract: &str,
    pre: &DealChainSnapshot,
    post: &DealChainSnapshot,
    event: &TokenContractSettlementEvent,
) -> Result<SettlementActionPostState> {
    let token_contract = display_token_contract(token_contract);
    validate_settlement_facts(&token_contract, post)?;
    if pre.subscription.is_subscription() != post.subscription.is_subscription()
        || pre.seller_bond.bond_required != post.seller_bond.bond_required
        || pre.buyer_bond.bond_required != post.buyer_bond.bond_required
    {
        return Err(anyhow!(
            "TokenContract {token_contract} immutable deal/bond shape changed across settlement action"
        ));
    }

    let terminal = !matches!(event, TokenContractSettlementEvent::StreamDisputed { .. });
    if terminal {
        if !post.state.is_stopped() {
            return Err(anyhow!(
                "TokenContract {token_contract} terminal event contradicts post-state: funded={} \
                 opened={} disputed={} deposit={} probeTick={}",
                post.state.funded,
                post.state.opened,
                post.state.disputed,
                post.state.deposit,
                post.state.probe_tick
            ));
        }
        if post.seller_bond.bond_held != 0 || post.buyer_bond.bond_held != 0 {
            return Err(anyhow!(
                "TokenContract {token_contract} terminal event contradicts held collateral: sellerBondHeld={} buyerBondHeld={}",
                post.seller_bond.bond_held,
                post.buyer_bond.bond_held
            ));
        }
    } else if !post.state.opened || !post.state.disputed {
        return Err(anyhow!(
            "TokenContract {token_contract} StreamDisputed contradicts post-state: opened={} disputed={}",
            post.state.opened,
            post.state.disputed
        ));
    }

    match event {
        TokenContractSettlementEvent::StreamStopped { to_seller, .. }
        | TokenContractSettlementEvent::DisputeResolved { to_seller, .. }
            if *to_seller != post.state.finalized_owed =>
        {
            return Err(anyhow!(
                "TokenContract {token_contract} event/getter contradiction: toSeller={to_seller} finalizedOwed={}",
                post.state.finalized_owed
            ));
        }
        TokenContractSettlementEvent::StreamDisputed { at, .. } => {
            let mut expected_state = pre.state;
            expected_state.disputed = true;
            expected_state.dispute_time = *at;
            if post.state != expected_state
                || post.subscription != pre.subscription
                || post.seller_bond != pre.seller_bond
                || post.buyer_bond != pre.buyer_bond
            {
                return Err(anyhow!(
                    "TokenContract {token_contract} StreamDisputed changed settlement money, \
                     token pipeline, subscription, or held bonds; only disputed/disputeTime may change"
                ));
            }
        }
        _ => {}
    }

    Ok(SettlementActionPostState {
        tokens_final: post.state.tokens_final.into(),
        tokens_pending: post.state.tokens_pending.into(),
        seller_bond_held: post.seller_bond.bond_held.into(),
        seller_bond_required: post.seller_bond.bond_required.into(),
        buyer_bond_held: post.buyer_bond.bond_held.into(),
        buyer_bond_required: post.buyer_bond.bond_required.into(),
        opened: post.state.opened,
        disputed: post.state.disputed,
    })
}

fn attach_settlement_post_snapshot(
    token_contract: &str,
    receipt: &mut SettlementActionReceipt,
    pre: &DealChainSnapshot,
    event: &TokenContractSettlementEvent,
    post: Option<&DealChainSnapshot>,
) -> Result<()> {
    let token_contract = display_token_contract(token_contract);
    match post {
        Some(post) => {
            receipt.post_state = Some(settlement_action_post_state(
                &token_contract,
                pre,
                post,
                event,
            )?);
        }
        None if matches!(event, TokenContractSettlementEvent::StreamDisputed { .. }) => {
            return Err(anyhow!(
                "TokenContract {token_contract} was inactive after non-terminal StreamDisputed"
            ));
        }
        None => {
            receipt.post_state = None;
        }
    }
    Ok(())
}

fn select_new_settlement_action_receipt(
    token_contract: &str,
    action: SettlementAction,
    expected: ExpectedSettlementEvent,
    expected_buyer: Option<&str>,
    observed: &TokenContractSettlementReceipts,
    pre_bonds: SettlementActionBondState,
) -> Result<Option<SettlementActionReceipt>> {
    let display_tc = display_token_contract(token_contract);
    let action_events = observed
        .events
        .iter()
        .filter(|receipt| {
            matches!(
                receipt.event,
                TokenContractSettlementEvent::ProbeBurned { .. }
                    | TokenContractSettlementEvent::StreamStopped { .. }
                    | TokenContractSettlementEvent::StreamDisputed { .. }
                    | TokenContractSettlementEvent::DisputeResolved { .. }
            )
        })
        .collect::<Vec<_>>();
    if action_events.is_empty() {
        return Ok(None);
    }
    if action_events.len() != 1 {
        return Err(anyhow!(
            "TokenContract {display_tc} action {action} produced {} distinct new action events: {}",
            action_events.len(),
            action_events
                .iter()
                .map(|receipt| receipt.message_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let receipt = action_events[0];
    let preserve_buyer = |observed_buyer: &str| -> Result<String> {
        let expected_buyer = expected_buyer.ok_or_else(|| {
            anyhow!(
                "TokenContract {display_tc} action {action} has no independently known buyer actor"
            )
        })?;
        let observed = normalize_addr(observed_buyer).with_context(|| {
            format!(
                "TokenContract {display_tc} action {action} emitted malformed buyer actor {observed_buyer}"
            )
        })?;
        let expected = normalize_addr(expected_buyer).with_context(|| {
            format!(
                "TokenContract {display_tc} action {action} has malformed expected buyer actor {expected_buyer}"
            )
        })?;
        if observed != expected {
            let observed = display_dexdo_address(observed);
            let expected = display_dexdo_address(expected);
            return Err(anyhow!(
                "TokenContract {display_tc} action {action} emitted wrong buyer actor {observed}; expected {expected}"
            ));
        }
        Ok(observed_buyer.to_string())
    };
    let event = match (&expected, &receipt.event) {
        (
            ExpectedSettlementEvent::ProbeBurned,
            TokenContractSettlementEvent::ProbeBurned {
                buyer,
                burned_probe,
                burned_bond,
                refund_to_buyer,
            },
        ) => SettlementActionEvent::ProbeBurned {
            buyer: preserve_buyer(buyer)?,
            burned_probe: (*burned_probe).into(),
            burned_bond: (*burned_bond).into(),
            refund_to_buyer: (*refund_to_buyer).into(),
        },
        (
            ExpectedSettlementEvent::StreamStopped,
            TokenContractSettlementEvent::StreamStopped {
                buyer,
                to_seller,
                refund_to_buyer,
            },
        ) => SettlementActionEvent::StreamStopped {
            buyer: preserve_buyer(buyer)?,
            to_seller: (*to_seller).into(),
            refund_to_buyer: (*refund_to_buyer).into(),
        },
        (
            ExpectedSettlementEvent::StreamDisputed,
            TokenContractSettlementEvent::StreamDisputed { buyer, at },
        ) => SettlementActionEvent::StreamDisputed {
            buyer: preserve_buyer(buyer)?,
            at: *at,
        },
        (
            ExpectedSettlementEvent::DisputeResolved { released: expected },
            TokenContractSettlementEvent::DisputeResolved {
                to_seller,
                refund_to_buyer,
                released,
            },
        ) if released == expected => SettlementActionEvent::DisputeResolved {
            to_seller: (*to_seller).into(),
            refund_to_buyer: (*refund_to_buyer).into(),
            released: *released,
        },
        (ExpectedSettlementEvent::BuyerStop, _) => {
            return Err(anyhow!(
                "TokenContract {display_tc} action {action} retained unresolved buyer-stop event expectation"
            ));
        }
        _ => {
            return Err(anyhow!(
                "TokenContract {display_tc} action {action} observed incompatible new event {:?}; \
                 expected {expected:?}",
                receipt.event
            ));
        }
    };
    Ok(Some(SettlementActionReceipt {
        token_contract: token_contract.to_string(),
        action,
        message_id: receipt.message_id.clone(),
        created_at: receipt.created_at,
        event,
        pre_bonds,
        post_state: None,
    }))
}

fn settlement_receipts_after_snapshot(
    before: &TokenContractSettlementReceipts,
    current: TokenContractSettlementReceipts,
) -> Result<TokenContractSettlementReceipts> {
    if current.events.len() < before.events.len() {
        return Err(anyhow!(
            "post-submit TokenContract history changed and is not an append-only extension: \
             pre-submit history had {} events but the current history has {}",
            before.events.len(),
            current.events.len()
        ));
    }
    for (index, (previous, observed)) in before.events.iter().zip(&current.events).enumerate() {
        if previous != observed {
            return Err(anyhow!(
                "post-submit TokenContract history changed and is not an append-only extension at event \
                 {index}: expected pre-submit identity {}, observed {}",
                previous.message_id,
                observed.message_id
            ));
        }
    }
    Ok(TokenContractSettlementReceipts {
        events: current
            .events
            .into_iter()
            .skip(before.events.len())
            .collect(),
    })
}

fn ambiguous_settlement_action(
    token_contract: &str,
    action: SettlementAction,
    source: anyhow::Error,
) -> anyhow::Error {
    let token_contract = display_token_contract(token_contract);
    anyhow::Error::new(MoneySubmitError::Ambiguous {
        source: source.context(format!(
            "TokenContract {token_contract} action {action} may have landed; the BOC was not resubmitted"
        )),
    })
}

fn explicit_money_submit_outcome(error: &anyhow::Error) -> Option<&MoneySubmitError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<MoneySubmitError>())
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_settlement_action_after_post<
    Submit,
    SubmitFuture,
    Observe,
    ObserveFuture,
    ReadPost,
    ReadPostFuture,
>(
    token_contract: &str,
    action: SettlementAction,
    expected: ExpectedSettlementEvent,
    expected_buyer: Option<&str>,
    before: &TokenContractSettlementReceipts,
    pre: &DealChainSnapshot,
    confirmation_timeout: std::time::Duration,
    confirmation_poll: std::time::Duration,
    post_timeout: std::time::Duration,
    submit: Submit,
    mut observe: Observe,
    read_post: ReadPost,
) -> Result<SettlementActionReceipt>
where
    Submit: FnOnce() -> SubmitFuture,
    SubmitFuture: std::future::Future<Output = Result<()>>,
    Observe: FnMut() -> ObserveFuture,
    ObserveFuture: std::future::Future<Output = Result<TokenContractSettlementReceipts>>,
    ReadPost: FnOnce() -> ReadPostFuture,
    ReadPostFuture: std::future::Future<Output = Result<Option<DealChainSnapshot>>>,
{
    // This is deliberately before the one POST await. Both a response and every subsequent fact
    // read share one finite budget.
    let started = std::time::Instant::now();
    let submit_note = match tokio::time::timeout(post_timeout.min(confirmation_timeout), submit())
        .await
    {
        Ok(Ok(())) => None,
        Ok(Err(error))
            if matches!(
                explicit_money_submit_outcome(&error),
                Some(MoneySubmitError::Preparation { .. } | MoneySubmitError::Rejected { .. })
            ) =>
        {
            return Err(error);
        }
        Ok(Err(error)) => Some(format!("{error:#}")),
        Err(_) => Some(format!(
            "one-shot money POST response exceeded the existing signed-message expiry bound {post_timeout:?}"
        )),
    };

    let pending_error = || {
        ambiguous_settlement_action(
            token_contract,
            action,
            anyhow!(
                "no compatible immutable event became provable inside the canonical \
                 confirmation budget; {}",
                submit_note
                    .as_deref()
                    .unwrap_or("the POST response was accepted")
            ),
        )
    };
    loop {
        let Some(remaining) = confirmation_timeout.checked_sub(started.elapsed()) else {
            return Err(pending_error());
        };
        if remaining.is_zero() {
            return Err(pending_error());
        }
        let current = match tokio::time::timeout(remaining, observe()).await {
            Ok(Ok(current)) => current,
            Ok(Err(error)) => {
                return Err(ambiguous_settlement_action(
                    token_contract,
                    action,
                    error.context("post-submit TokenContract event read/decode failed"),
                ));
            }
            Err(_) => return Err(pending_error()),
        };
        let observed = settlement_receipts_after_snapshot(before, current).map_err(|error| {
            ambiguous_settlement_action(
                token_contract,
                action,
                error.context("post-submit TokenContract event snapshot contradicted its baseline"),
            )
        })?;
        let selected = select_new_settlement_action_receipt(
            token_contract,
            action,
            expected,
            expected_buyer,
            &observed,
            settlement_bond_state(pre),
        )
        .map_err(|error| {
            ambiguous_settlement_action(
                token_contract,
                action,
                error.context("post-submit action event was incompatible"),
            )
        })?;
        if let Some(mut receipt) = selected {
            let observed_event = observed
                .events
                .iter()
                .find(|event| event.message_id == receipt.message_id)
                .expect("selected receipt came from this observed event set")
                .event
                .clone();
            let remaining = confirmation_timeout
                .checked_sub(started.elapsed())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(&pending_error)?;
            let post = match tokio::time::timeout(remaining, read_post()).await {
                Ok(Ok(post)) => post,
                Ok(Err(error)) => {
                    return Err(ambiguous_settlement_action(
                        token_contract,
                        action,
                        error.context("post-submit coherent TokenContract snapshot failed"),
                    ));
                }
                Err(_) => return Err(pending_error()),
            };
            attach_settlement_post_snapshot(
                token_contract,
                &mut receipt,
                pre,
                &observed_event,
                post.as_ref(),
            )
            .map_err(|error| {
                ambiguous_settlement_action(
                    token_contract,
                    action,
                    error.context("post-submit event/getter facts contradicted"),
                )
            })?;
            return Ok(receipt);
        }
        let delay = settlement_confirmation_delay(
            started.elapsed(),
            confirmation_timeout,
            confirmation_poll,
        )
        .ok_or_else(&pending_error)?;
        tokio::time::sleep(delay).await;
    }
}

fn decode_external_abi_message(
    body_b64: &str,
    abi: &str,
    input: bool,
) -> Option<tvm_abi::contract::DecodedMessage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body_b64.trim())
        .ok()?;
    let cell = tvm_types::read_single_root_boc(&bytes).ok()?;
    let slice = tvm_types::SliceData::load_cell(cell).ok()?;
    let contract = tvm_abi::Contract::load(abi.as_bytes()).ok()?;
    if input {
        contract.decode_input(slice, false, true).ok()
    } else {
        contract.decode_output(slice, false, true).ok()
    }
}

/// Decode the ABI call an INTERNAL message body carries: no signature and no header, so the same
/// decoder has to be told the message is internal.
fn decode_internal_abi_call(
    body_b64: &str,
    abi: &str,
) -> Option<tvm_abi::contract::DecodedMessage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body_b64.trim())
        .ok()?;
    let cell = tvm_types::read_single_root_boc(&bytes).ok()?;
    let slice = tvm_types::SliceData::load_cell(cell).ok()?;
    let contract = tvm_abi::Contract::load(abi.as_bytes()).ok()?;
    contract.decode_input(slice, true, true).ok()
}

fn decode_external_abi_message_boc(
    message_b64: &str,
    abi: &str,
    input: bool,
) -> Option<tvm_abi::contract::DecodedMessage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(message_b64.trim())
        .ok()?;
    let cell = tvm_types::read_single_root_boc(&bytes).ok()?;
    let message = tvm_block::Message::construct_from_cell(cell).ok()?;
    let body = message.body()?;
    let contract = tvm_abi::Contract::load(abi.as_bytes()).ok()?;
    if input {
        contract.decode_input(body, false, true).ok()
    } else {
        contract.decode_output(body, false, true).ok()
    }
}

fn decoded_u128(tokens: &[tvm_abi::Token], name: &str) -> Option<u128> {
    tokens.iter().find_map(|token| {
        if token.name != name {
            return None;
        }
        match &token.value {
            tvm_abi::token::TokenValue::Uint(value) => value.number.to_string().parse().ok(),
            _ => None,
        }
    })
}

fn decoded_address(tokens: &[tvm_abi::Token], name: &str) -> Option<String> {
    tokens.iter().find_map(|token| {
        if token.name != name {
            return None;
        }
        match &token.value {
            tvm_abi::token::TokenValue::Address(value) => Some(format!("{value}")),
            _ => None,
        }
    })
}

fn decoded_u64(tokens: &[tvm_abi::Token], name: &str) -> Option<u64> {
    tokens.iter().find_map(|token| {
        if token.name != name {
            return None;
        }
        match &token.value {
            tvm_abi::token::TokenValue::Uint(value) => value.number.to_string().parse().ok(),
            _ => None,
        }
    })
}

fn decoded_bool(tokens: &[tvm_abi::Token], name: &str) -> Option<bool> {
    tokens.iter().find_map(|token| {
        if token.name != name {
            return None;
        }
        match &token.value {
            tvm_abi::token::TokenValue::Bool(value) => Some(*value),
            _ => None,
        }
    })
}

impl RealChainBackend {
    /// Decode storage from a caller-owned immutable account snapshot with its compiled ABI.
    pub fn decode_account_storage_fields(
        account_boc: &str,
        abi_json: &str,
        contract_name: &str,
    ) -> Result<Value> {
        account_storage_fields(account_boc, abi_json, contract_name)
    }

    /// Connect using an optional manifest endpoint, falling back to the canonical the chain endpoint.
    pub fn connect(manifest_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::connect_with_endpoint(manifest_path, None)
    }

    /// Connect with an explicit endpoint override, then manifest endpoint, then a refusal: there is no default.
    pub fn connect_with_endpoint(
        manifest_path: impl AsRef<Path>,
        endpoint: Option<&str>,
    ) -> anyhow::Result<Self> {
        let (deployed, client) =
            connect_client_from_manifest_with(manifest_path, endpoint, |endpoint, config| {
                ChainClient::connect_with_config(endpoint, config)
            })?;
        // the ceiling is chosen HERE, from the manifest, and lives exactly as long as this
        // backend. `connect_client_from_manifest_with` says of itself that it is "the one place both
        // are known", and it is also the only place a `RealChainBackend` is built. Per instance
        // rather than global on purpose: a global set once takes its value from whichever backend
        // came up first, so a client raised after another would silently inherit "no ceiling" --
        // the failure would be invisible on the money path.

        // the FIGURE comes from the manifest's own `requests_per_second`, not from a match on
        // the network's name. A ceiling is a property of the chain being dialled, and the manifest is
        // the one document that describes that chain -- the same place `endpoint` and `indexer` come
        // from. Behaviour is unchanged: the production manifest carries 3, and a manifest naming no
        // ceiling gets none, exactly as the label match decided before.
        let client = LimitedChainClient::new(client, ChainRequestCeiling::from_manifest(&deployed));
        let http = chain_http_client()?;
        let money_post_http = build_money_post_http_client()?;
        // The SuperRoot is a shared-DApp account and this field is the chain address the client
        // reads and writes with, so only the chain half is stored.
        let superroot = crate::address::parse_chain_address(&deployed.superroot)?.into_chain();
        Ok(Self {
            client,
            http,
            money_post_http,
            superroot,
            deployed,
        })
    }

    /// Network profile selected from the deployment manifest used by this client.
    pub fn network(&self) -> &str {
        &self.deployed.network
    }

    /// A read-only client that addresses the Shell Accumulator's DApp explicitly.

    /// The accumulator root does NOT live in the dexdo DApp: it reports `dapp_id` 1 on both
    /// networks, and a read addressed to DApp 4 returns null rather than an error. The default
    /// getter rule (`dapp_id == account_id`) happens to reach it today because two DApp ids can
    /// route to the same shard - but "it answered" is not "it lives there", and a client that
    /// depends on that coincidence reads nothing the day routing changes, which on a money path
    /// looks like an empty queue rather than a failure. So the DApp is stated rather than inferred.
    pub fn connect_accumulator_reader(
        manifest_path: impl AsRef<Path>,
        endpoint: Option<&str>,
    ) -> anyhow::Result<ChainClient> {
        let (_deployed, client) =
            connect_client_from_manifest_with(manifest_path, endpoint, |endpoint, mut config| {
                config.dapp_id_override = Some(crate::params::ACCUMULATOR_DAPP_ID.to_string());
                ChainClient::connect_with_config(endpoint, config)
            })?;
        Ok(client)
    }

    /// Fold authoritative live orders from one `InferenceOrderBook` ext-out stream.
    pub async fn fold_order_book_events(
        &self,
        order_book: &str,
        previous: BookEventFold,
    ) -> Result<BookEventFold> {
        retry_transient_read(|| {
            read_book_event_fold(
                self.client.gate(),
                &self.http,
                self.client.endpoint(),
                order_book,
                previous.clone(),
            )
        })
        .await
    }

    /// Report every `InferenceFilled` that names `buyer_note`, split into confirmed candidates and
    /// refusals.

    /// This is a verified candidate list, not a recovery operation: the book history is filtered
    /// by normalized buyer note while it is walked, then every retained event's TokenContract must
    /// still report the same buyer and seller note through `getParties` and `funded=true` through
    /// `getState`. Inactive, settled, unfunded, or identity-mismatched historical fills are
    /// reported as refusals rather than dropped, because a fill the book emitted is a fact about
    /// this note whether or not its deal survived, and an operator who is shown nothing cannot
    /// otherwise tell that case from a note the book never named. Transport or ABI failures remain
    /// errors because they prevent verification.
    pub async fn verified_book_fill_candidates(
        &self,
        order_book: &Address,
        buyer_note: &Address,
    ) -> Result<BookFillCandidateReport> {
        let account_id = order_book.bare().to_string();
        let buyer_note = normalize_addr(&buyer_note.with_workchain())?;
        let candidates = retry_transient_read(|| {
            read_book_fill_candidates(
                self.client.gate(),
                &self.http,
                self.client.endpoint(),
                &account_id,
                &buyer_note,
            )
        })
        .await?;
        let mut report = BookFillCandidateReport::default();
        for candidate in candidates {
            let token_contract =
                Address::parse(&candidate.seller_token_contract).with_context(|| {
                    format!(
                        "InferenceFilled sellerTC {}",
                        candidate.seller_token_contract
                    )
                })?;
            let parties = self.token_contract_parties(&token_contract).await?;
            let state = self.token_contract_deal_state(&token_contract).await?;
            match book_fill_candidate_refusal_reason(
                &candidate,
                &buyer_note,
                parties.as_ref(),
                state.as_ref(),
            ) {
                None => report.candidates.push(candidate),
                Some(reason) => report
                    .refusals
                    .push(BookFillCandidateRefusal { candidate, reason }),
            }
        }
        Ok(report)
    }

    /// Low-level chain client (for the trait adapter in the next step).
    /// The raw SDK client, unchanged and UNMETERED.

    /// Kept at its original type on purpose: retyping it is what cascaded through the CLI without a
    /// fixed point. New code should call the metered methods on this backend instead -- see
    /// [`RealChainBackend::get_account`] and its siblings -- and existing sites move over in batches.
    pub fn client(&self) -> &ChainClient {
        self.client.unmetered()
    }

    /// The ceilinged client. Every method on it admits before it dials.
    pub fn metered(&self) -> &LimitedChainClient {
        &self.client
    }

    /// Read an account through the ceiling.

    /// Additive: this does not replace `client().get_account(..)`, it is the metered way to do the
    /// same thing, and a batch conversion turns one into the other with no signature change.
    pub async fn get_account(&self, address: &Address) -> Result<Option<Account>> {
        self.client.get_account(address).await
    }

    /// Read an account in an explicit DApp, through the ceiling.
    pub async fn get_account_in_dapp(
        &self,
        address: &Address,
        dapp: &Address,
    ) -> Result<Option<Account>> {
        self.client.get_account_in_dapp(address, dapp).await
    }

    /// Run a getter through the ceiling.
    pub async fn run_getter(
        &self,
        address: &Address,
        abi: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        self.client.run_getter(address, abi, method, args).await
    }

    /// Chain liveness through the ceiling.
    pub async fn chain_liveness(&self) -> Result<ChainLiveness> {
        self.client.chain_liveness().await
    }

    /// The same reads, retried, through the ceiling. Each attempt admits separately, which is what
    /// keeps a retry storm from becoming the overload it is retrying against.
    pub async fn get_account_retrying(&self, address: &Address) -> Result<Option<Account>> {
        RetryingReads::get_account_retrying(&self.client, address).await
    }

    pub async fn run_getter_retrying(
        &self,
        address: &Address,
        abi: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        RetryingReads::run_getter_retrying(&self.client, address, abi, method, args).await
    }

    pub async fn chain_liveness_retrying(&self) -> Result<ChainLiveness> {
        RetryingReads::chain_liveness_retrying(&self.client).await
    }

    /// The `SuperRoot` address -- the derivation point for `RootModel`/`InferenceOrderBook`.
    pub fn superroot(&self) -> &Address {
        &self.superroot
    }

    /// Chain liveness check -- confirms a working connection to the chain.
    pub async fn liveness(&self) -> Result<ChainLiveness> {
        self.client.chain_liveness_retrying().await
    }

    async fn clock_skew_preflight(&self) -> Result<()> {
        let check = clock_skew_check(
            local_unix_secs()?,
            retry_transient_read(|| fetch_chain_time_secs(&self.http, self.client.endpoint()))
                .await?,
        );
        if check.status == ChainDoctorStatus::Fail {
            return Err(anyhow!(check.message));
        }
        Ok(())
    }

    pub async fn observed_chain_timestamp(&self) -> Result<u64> {
        retry_transient_read(|| fetch_chain_time_secs(&self.http, self.client.endpoint())).await
    }

    pub async fn account_active_code_hash(&self, addr: &Address) -> Result<(bool, Option<String>)> {
        let Some(acc) = self.client.get_account_retrying(addr).await? else {
            return Ok((false, None));
        };
        Ok((
            acc.is_active(),
            acc.code_hash.as_deref().and_then(normalize_code_hash),
        ))
    }

    /// Read an account's live `code_hash` and hand it to one of the generation-check builders. An
    /// account that is not Active reports `None`, which those builders fail closed on.
    async fn generation_check(
        &self,
        addr: &Address,
        expected: &str,
        build: fn(&Address, &str, Option<&str>) -> ChainDoctorCheck,
    ) -> Result<ChainDoctorCheck> {
        let (active, hash) = self.account_active_code_hash(addr).await?;
        Ok(build(
            addr,
            expected,
            active.then_some(hash).flatten().as_deref(),
        ))
    }

    /// The generation this build has READ for the network it is dialling.

    /// A network with no row is a refusal and never a fall-through to another chain's pins: that
    /// substitution is the whole defect this table closes -- a client dialling one chain would report
    /// another chain's hashes as "live" and fail every check for a reason that is not its own.
    /// Hash of the `TokenContract` code RootPN bakes into every note it mints, read out of its
    /// `_tokenContractCode` storage field.

    /// A STORAGE READ AND NOT A GETTER, because there is no getter: `RootPN.getDetails()` returns the
    /// PMP and PrivateNote hashes and stops there. The field is declared in the ABI's `data` section,
    /// so it decodes off the account like any other storage field.
    async fn root_pn_deal_code_check(
        &self,
        rootpn: &Address,
        expected: &str,
    ) -> Result<ChainDoctorCheck> {
        const NAME: &str = "RootPN TokenContract code (setTokenContractCode)";
        let boc = self
            .client
            .get_account_retrying(rootpn)
            .await?
            .and_then(|account| account.boc);
        let Some(boc) = boc else {
            // No account, or an account with no BOC: report it as the generation check does, by
            // failing closed with nothing on the live side.
            return Ok(code_hash_check(NAME, Some(rootpn), expected, None));
        };
        // A CHECK THAT CANNOT BE EVALUATED FAILS; IT DOES NOT ABORT THE REPORT. This used to
        // propagate, and `doctor()` calls it with `?` -- so one undecodable account produced no
        // report at all, not even the twenty checks that had already passed. The `None` BOC arm ten
        // lines above already fails closed; this arm disagreed with it inside the same function.

        // made that reachable on a second chain: mainnet's row carries a
        // `token_contract_code` now, so mainnet runs this check for the first time, against an
        // account whose storage shape this tree has only ever decoded on the development chain.
        let fields = match account_storage_fields(&boc, ROOTPN_ABI, "RootPN") {
            Ok(fields) => fields,
            Err(error) => {
                let mut check = code_hash_check(NAME, Some(rootpn), expected, None);
                check.message = format!(
                    "RootPN {} storage could not be decoded, so whether `setTokenContractCode` has \
                     run is unknown -- treated as not run, because minting on that assumption is \
                     the cheaper mistake: {error}",
                    display_dexdo_address(rootpn)
                );
                return Ok(check);
            }
        };
        let actual = fields["_tokenContractCode"]
            .as_str()
            .and_then(cell_boc_repr_hash);
        let mut check = code_hash_check(NAME, Some(rootpn), expected, actual.as_deref());
        if check.status == ChainDoctorStatus::Fail {
            // The generic "stale binary, rebuild" advice is wrong here and would send an operator
            // down the wrong road. Say what actually has to happen, and what it costs to skip it.
            check.message = format!(
                "RootPN {} does not carry the 4.0.36 TokenContract code (expected {expected}, \
                 found {}). Until an owner runs `setTokenContractCode`, every note minted here is \
                 born holding an empty cell and its `deployDeal` cannot create the canonical deal -- \
                 and nothing reports that until the deal deploy, on a note that has already been \
                 funded. Install the code BEFORE minting the first note of this generation.",
                display_dexdo_address(rootpn),
                actual.as_deref().unwrap_or("an empty or unreadable cell"),
            );
        }
        Ok(check)
    }

    /// The PrivateNote code the root on THIS network mints -- what every note guard is held to.
    fn private_note_pin(&self) -> Result<&'static str> {
        Ok(self.generation_pins_for_this_network()?.private_note)
    }

    /// What a deployed `contract` account on THIS network is supposed to be.

    /// **Two sources, and which one answers is not a detail.** Where the row of the generation this
    /// manifest declares carries the value, the row answers: it is what somebody READ OFF A CHAIN
    /// running that generation, so during a staged rollout a chain that has not moved yet is judged
    /// by the generation it actually runs and not by whatever this tree happens to have compiled.
    /// That covers `SuperRoot`, `RootPN`, `RootOracle`, `PrivateNote`, `TokenContract` and
    /// `InferenceOrderBook`.

    /// For the rest -- `Oracle`, `OracleEventList`, `PMP`, `OrderBook` and the registry contracts --
    /// no measured fact exists anywhere: nobody has read them off a chain and written them down. The
    /// compiled image is then the only source there is, and the answer it gives is honest about what
    /// it means: "the code this build knows how to talk to". A caller about to decode that account's
    /// storage with this build's ABI is asking exactly that.

    /// The asymmetry is stated rather than smoothed over, because smoothing it over is what the
    /// manifest's `contract_hashes` did: it made every contract look measured while being a copy of
    /// what the compiler had already produced.
    fn expected_contract_hash(&self, contract: &str) -> Result<String> {
        let row = self.generation_pins_for_this_network()?;
        let per_network = match contract {
            "SuperRoot" => Some(row.superroot.to_string()),
            "RootPN" => Some(row.rootpn.to_string()),
            "RootOracle" => Some(row.rootoracle.to_string()),
            "PrivateNote" => Some(row.private_note.to_string()),
            "TokenContract" => row.token_contract_code.map(str::to_string),
            "InferenceOrderBook" => row.inference_orderbook.map(str::to_string),
            _ => None,
        };
        match per_network {
            Some(hash) => Ok(hash),
            None => compiled_contract_hash(contract),
        }
    }

    fn generation_pins_for_this_network(&self) -> Result<&'static GenerationPins> {
        let version = self.deployed.version.as_deref().unwrap_or_default();
        if version.is_empty() {
            return Err(anyhow!(
                "the manifest for `{}` declares no `version`, so it does not say which contracts \
                 generation this run expects and there is nothing to hold the chain to. Add the \
                 `version` field.",
                self.network()
            ));
        }
        generation_pins(version).ok_or_else(|| {
            anyhow!(
                "no generation pins have been measured for contracts generation `{version}`, which \
                 the manifest for `{}` declares, so there is nothing to hold the chain to. Read that \
                 generation's four fixed roots and add a row to GENERATION_PINS; do not reuse \
                 another generation's.",
                self.network()
            )
        })
    }

    async fn code_hash_account_check(
        &self,
        name: &str,
        addr: &Address,
        expected: &str,
    ) -> Result<ChainDoctorCheck> {
        let (active, hash) = self.account_active_code_hash(addr).await?;
        if !active {
            return Ok(code_hash_check(name, Some(addr), expected, None));
        }
        Ok(code_hash_check(name, Some(addr), expected, hash.as_deref()))
    }

    async fn self_dapp_code_hash_account_check(
        &self,
        name: &str,
        addr: &Address,
        expected: &str,
    ) -> Result<ChainDoctorCheck> {
        let mut check = self.code_hash_account_check(name, addr, expected).await?;
        check.address = Some(display_token_contract(addr));
        Ok(check)
    }

    async fn seller_note_withdrawn_check(&self, note: &Address) -> Result<ChainDoctorCheck> {
        match self.private_note_details(note).await {
            Ok(Some(details)) => Ok(seller_note_withdrawn_check(
                note,
                details_has_withdrawn(&details),
            )),
            Ok(None) => Ok(ChainDoctorCheck {
                name: "seller PrivateNote withdrawn state".to_string(),
                status: ChainDoctorStatus::Fail,
                address: Some(display_dexdo_address(note)),
                expected: Some("hasWithdrawn=false".to_string()),
                actual: Some("getDetails=<none>".to_string()),
                message: "seller note returned no PrivateNote.getDetails; it is not active/current enough to prove postSellOffer safety"
                    .to_string(),
            }),
            Err(e) => Ok(ChainDoctorCheck {
                name: "seller PrivateNote withdrawn state".to_string(),
                status: ChainDoctorStatus::Fail,
                address: Some(display_dexdo_address(note)),
                expected: Some("hasWithdrawn=false".to_string()),
                actual: Some("getDetails=<error>".to_string()),
                message: format!(
                    "cannot read PrivateNote.getDetails.hasWithdrawn before seller postSellOffer: {e}"
                ),
            }),
        }
    }

    async fn version_of(&self, addr: &Address, abi: &str) -> Result<Option<String>> {
        let Some(v) = self
            .client
            .run_getter_retrying(addr, abi, "getVersion", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let left = v["value0"].as_str().unwrap_or("").trim();
        let right = v["value1"].as_str().unwrap_or("").trim();
        Ok(match (left.is_empty(), right.is_empty()) {
            (true, true) => None,
            (false, true) => Some(left.to_string()),
            (true, false) => Some(right.to_string()),
            (false, false) => Some(format!("{left} {right}")),
        })
    }

    /// The endpoint-reachability line `doctor` prints, split out so it can be read without a chain.

    /// this line said `the chain endpoint` on every network, so a mainnet run reported a chain
    /// it had never dialled. `doctor` is what an operator runs to answer "am I where I think I am",
    /// and on mainnet a wrong answer to that precedes spending real money. The name is taken from
    /// the manifest this client was built from -- the same value the report header carries -- so it
    /// cannot disagree with the endpoint the connect-time cross-check already proved.
    fn endpoint_reachable_check(&self) -> ChainDoctorCheck {
        pass_check(&format!("{} endpoint", self.network()), "reachable")
    }

    /// Read-only chain readiness report: compare this binary's embedded/pinned contract images against
    /// a live chain and, when supplied, verify that a market manifest still points at active IOB/TC accounts.
    pub async fn doctor(&self, market: Option<&MarketManifest>) -> Result<ChainDoctorReport> {
        self.doctor_observing(market, |_, _| {}).await
    }

    /// The same report with a notification after each check completes.
    pub async fn doctor_observing(
        &self,
        market: Option<&MarketManifest>,
        mut observe: impl FnMut(usize, &ChainDoctorCheck),
    ) -> Result<ChainDoctorReport> {
        let mut checks = Vec::new();
        self.liveness().await?;
        record_doctor_check(&mut checks, &mut observe, self.endpoint_reachable_check());
        let local_unix = local_unix_secs()?;
        let chain_unix =
            retry_transient_read(|| fetch_chain_time_secs(&self.http, self.client.endpoint()))
                .await?;
        let clock_skew_seconds = signed_clock_skew_seconds(local_unix, chain_unix);
        record_doctor_check(
            &mut checks,
            &mut observe,
            clock_skew_check(local_unix, chain_unix),
        );

        let pins = self.generation_pins_for_this_network()?;
        let superroot = self.superroot.clone();
        record_doctor_check(
            &mut checks,
            &mut observe,
            self.generation_check(&superroot, pins.superroot, superroot_generation_check)
                .await?,
        );

        if self.deployed.dapp_config.trim().is_empty() {
            record_doctor_check(
                &mut checks,
                &mut observe,
                skipped_check(
                    "DappConfig account",
                    "fixed-superroot redeploy has no legacy DappConfig manifest account",
                ),
            );
        } else {
            let dapp_config = Address::parse(&self.deployed.dapp_config)?;
            let (dapp_active, _) = self.account_active_code_hash(&dapp_config).await?;
            record_doctor_check(
                &mut checks,
                &mut observe,
                active_check("DappConfig account", &dapp_config, dapp_active),
            );
        }

        let rootpn = Address::parse(ROOTPN_ADDR)?;
        record_doctor_check(
            &mut checks,
            &mut observe,
            self.generation_check(&rootpn, pins.rootpn, rootpn_generation_check)
                .await?,
        );
        let rootoracle = Address::parse(ROOTORACLE_ADDR)?;
        record_doctor_check(
            &mut checks,
            &mut observe,
            self.generation_check(&rootoracle, pins.rootoracle, rootoracle_generation_check)
                .await?,
        );

        // SAME RULE AS THE DEAL-CODE CHECK BELOW: a check that cannot be evaluated FAILS, it does
        // not abort the report. `getDetails` is decoded with the vendored ABI, and a chain whose
        // getter has a shape this tree does not know would otherwise take the whole report with it
        // -- including the four checks already sitting in `checks`. Fixing that one call and
        // leaving this one was half a fix, and the half left over was two statements earlier.

        // The getter is called ONCE. An earlier draft asked again to word the refusal, which is a
        // second chain read on the failure path and can answer differently from the first -- the
        // refusal would then describe a state that no longer holds.

        // Nothing below reads the answer, so an unreadable one costs exactly this one check.
        match self
            .client
            .run_getter_retrying(&rootpn, ROOTPN_ABI, "getDetails", json!({}))
            .await
        {
            Ok(Some(details)) => record_doctor_check(
                &mut checks,
                &mut observe,
                private_note_pin_check(pins.private_note, &details),
            ),
            other => {
                let reason = match other {
                    Ok(None) => "the account is not active".to_string(),
                    Err(error) => {
                        format!("it did not answer in a shape this build can decode: {error}")
                    }
                    Ok(Some(_)) => unreachable!("the matching arm above takes this case"),
                };
                record_doctor_check(
                    &mut checks,
                    &mut observe,
                    ChainDoctorCheck {
                        name: "PrivateNote code hash (RootPN pin)".to_string(),
                        status: ChainDoctorStatus::Fail,
                        address: Some(display_dexdo_address(&rootpn)),
                        expected: Some(pins.private_note.to_string()),
                        actual: None,
                        message: format!(
                            "the PrivateNote code RootPN {} mints could not be read, so nothing holds \
                             the notes this chain issues: {reason}",
                            display_dexdo_address(&rootpn)
                        ),
                    },
                );
            }
        }

        // **THE PRECONDITION WHOSE FAILURE SURFACES ON SOMEONE ELSE'S MONEY** (contracts 4.0.36).

        // A 4.0.36 note is deployed with the `TokenContract` code baked in, because the note deploys
        // the deal now and a deploy needs the code cell rather than a pin. RootPN hands it over from
        // `_tokenContractCode`, which an owner must install with `setTokenContractCode` BEFORE the
        // first note of the generation is minted. A note issued before that call holds an empty cell
        // -- and nothing says so until `deployDeal`, on a note that has already been funded.

        // RootPN exposes no getter for it (`getDetails` returns the PMP and PrivateNote hashes and
        // nothing else), so this reads the storage field off the account and hashes the cell. That is
        // the whole reason the check is here rather than one line beside the others.
        let deal_code_check = match pins.token_contract_code {
            Some(expected) => self.root_pn_deal_code_check(&rootpn, expected).await?,
            // A generation whose RootPN has no such field: 4.0.35 and earlier deployed the deal
            // off-chain, so there was never anything to install.
            None => skipped_check(
                "RootPN TokenContract code (setTokenContractCode)",
                "this generation's RootPN does not carry the deal code; the deal was deployed off-chain",
            ),
        };
        record_doctor_check(&mut checks, &mut observe, deal_code_check);

        if let Some(market) = market {
            let rm = Address::parse(&market.root_model)?;
            record_doctor_check(
                &mut checks,
                &mut observe,
                self.code_hash_account_check(
                    "RootModel code hash",
                    &rm,
                    SUPERROOT_PINNED_RM_CODE_HASH,
                )
                .await?,
            );
            let ob = Address::parse(&market.inference_order_book)?;
            let order_book_check = match pins.inference_orderbook {
                Some(expected) => {
                    self.generation_check(&ob, expected, inference_orderbook_generation_check)
                        .await?
                }
                // A book is deployed per model, so on a freshly brought-up network there is none of
                // this generation to have read. Skipping says that; comparing against a neighbouring
                // chain's book, or against the vendored image, would not.
                None => skipped_check(
                    "InferenceOrderBook code hash",
                    "no book of this generation has been read on this network yet",
                ),
            };
            record_doctor_check(&mut checks, &mut observe, order_book_check);
            let tc = Address::parse(&market.token_contract)?;
            record_doctor_check(
                &mut checks,
                &mut observe,
                self.self_dapp_code_hash_account_check(
                    "TokenContract code hash",
                    &tc,
                    ROOTMODEL_PINNED_TC_CODE_HASH,
                )
                .await?,
            );
            let mut token_contract_state = active_check(
                "market TokenContract state",
                &tc,
                self.token_contract_state(&tc).await?.is_some(),
            );
            token_contract_state.address = Some(display_token_contract(&tc));
            record_doctor_check(&mut checks, &mut observe, token_contract_state);
            let seller_note = Address::parse(&market.seller_note)?;
            let seller_note_check = self.seller_note_withdrawn_check(&seller_note).await?;
            record_doctor_check(&mut checks, &mut observe, seller_note_check);
        } else {
            for check in [
                skipped_check(
                    "RootModel code hash",
                    "pass --market <manifest> to check the seller's deployed RootModel",
                ),
                skipped_check(
                    "InferenceOrderBook code hash",
                    "pass --market <manifest> to check a deployed order book",
                ),
                skipped_check(
                    "TokenContract code hash",
                    "pass --market <manifest> to check a deployed TokenContract",
                ),
                skipped_check(
                    "market TokenContract state",
                    "pass --market <manifest> to check manifest freshness",
                ),
                skipped_check(
                    "seller PrivateNote withdrawn state",
                    "pass --market <manifest> to check the seller note's hasWithdrawn flag",
                ),
            ] {
                record_doctor_check(&mut checks, &mut observe, check);
            }
        }

        // THE VERSION BANNER IS NOT WORTH THE REPORT. These three reads decorate the header; a
        // chain that will not answer `getVersion` in a shape this build decodes used to take every
        // check down with it through `?`. The banner simply goes short instead, and the check that
        // actually holds the generation -- `deployed manifest generation`, fed by `validate` below
        // -- keeps whatever was read.
        let mut versions = Vec::new();
        for (label, address, abi) in [
            ("SuperRoot", &self.superroot, SUPERROOT_ABI),
            ("RootPN", &rootpn, ROOTPN_ABI),
            ("RootOracle", &rootoracle, ROOTORACLE_ABI),
        ] {
            if let Ok(Some(v)) = self.version_of(address, abi).await {
                versions.push((label.to_string(), v));
            }
        }
        for check in self.deployed.validate(&versions) {
            record_doctor_check(&mut checks, &mut observe, check);
        }
        debug_assert_eq!(checks.len(), CHAIN_DOCTOR_CHECK_COUNT);
        let chain_generations = versions
            .iter()
            .filter_map(|(_, version)| version.split_whitespace().next())
            .filter(|generation| !generation.is_empty())
            .collect::<BTreeSet<_>>();
        let chain_generation = (chain_generations.len() == 1)
            .then(|| (*chain_generations.first().expect("one generation")).to_string());
        Ok(ChainDoctorReport {
            network: self.deployed.network.clone(),
            endpoint: doctor_report_endpoint(self.client.endpoint()),
            manifest_generation: self
                .deployed
                .version
                .as_deref()
                .map(str::trim)
                .filter(|generation| !generation.is_empty())
                .map(str::to_string),
            chain_generation,
            clock_skew_seconds,
            versions,
            checks,
        })
    }

    /// The `SuperRoot` owner pubkey (on-chain getter `getOwnerPubkey`).
    pub async fn superroot_owner_pubkey(&self) -> Result<Value> {
        let v = self
            .client
            .run_getter_retrying(&self.superroot, SUPERROOT_ABI, "getOwnerPubkey", json!({}))
            .await?
            .ok_or_else(|| anyhow!("SuperRoot is not active"))?;
        Ok(v["value0"].clone())
    }

    /// The `RootModel` address for a given owner pubkey -- the deterministic SuperRoot on-chain getter
    /// `getRootModelAddress(ownerPubkey)`. RootModel is per-owner: for the seller (model owner)
    /// it is derived from their pubkey (see [`Self::deploy_root_model`]).
    pub async fn root_model_address_for(&self, owner_pubkey: &Value) -> Result<Address> {
        let v = self
            .client
            .run_getter_retrying(
                &self.superroot,
                SUPERROOT_ABI,
                "getRootModelAddress",
                json!({ "ownerPubkey": owner_pubkey }),
            )
            .await?
            .ok_or_else(|| anyhow!("SuperRoot is not active"))?;
        Address::parse(v["value0"].as_str().ok_or_else(|| anyhow!("no address"))?)
    }

    /// Derive the `RootModel` address of the `SuperRoot` owner (part of address resolution for `ChainBackend`).
    pub async fn resolve_root_model(&self) -> Result<Address> {
        let owner = self.superroot_owner_pubkey().await?;
        self.root_model_address_for(&owner).await
    }

    /// Ask `SuperRoot` to deploy the `RootModel` for `owner_pubkey`: an external call to the fixed
    /// SuperRoot address, `deployRootModel(uint256 ownerPubkey)`
    /// (`contracts/airegistry/SuperRoot.sol:189`).

    /// **THE CLIENT NO LONGER DEPLOYS THE ROOT MODEL, IT ASKS FOR IT.** Under 4.0.34 an externally
    /// deployed `RootModel` is not merely malformed, it is refused on authority: the constructor's
    /// first statement is `require(msg.sender == _superRootAddress, ERR_INVALID_SENDER)`
    /// (`contracts/airegistry/RootModel.sol:67`, `ERR_INVALID_SENDER = 302`), and `msg.sender` on an
    /// external message is `addr_none`. The deploy therefore has to come from SuperRoot's own internal
    /// `new`, which is also the point of the change -- an internal `new` lands the root in SuperRoot's
    /// configured dapp, where `RootModel.ensureBalance() -> gosh.mintshellq` works, whereas an external
    /// deploy landed it in a dapp of its own with no configuration and that line did nothing.

    /// **NOTHING IS ATTACHED AND NOTHING IS PRE-FUNDED.** SuperRoot carries `ROOT_MODEL_DEPLOY_VALUE =
    /// 5 vmshell` on the deploy message itself (`contracts/airegistry/SuperRoot.sol:58`), so the seller
    /// note has no uninit address to fund first. The entry takes `tvm.accept()` on its first line and
    /// checks no key, so the signature this call carries is ignored by the contract; it is the seller's
    /// own key only because that is the key the caller already holds.

    /// Calling it twice is a no-op -- a `new` at an occupied address does not overwrite and, with
    /// `bounce: false`, does not revert. So this is safe to re-issue on an idempotent provision.
    async fn request_root_model_deploy(
        &self,
        owner: &KeyPair,
        owner_pubkey: &Value,
    ) -> Result<Value> {
        self.submit(
            &self.superroot,
            SUPERROOT_ABI,
            SUPERROOT_DEPLOY_ROOT_MODEL_METHOD,
            super_root_deploy_root_model_params(owner_pubkey),
            owner,
        )
        .await
    }

    /// Derive the per-deal `TokenContract` address from `RootModel` (`getTokenContractAddress`)
    /// by the seller's pubkey and the deal nonce -- a deterministic on-chain getter.
    pub async fn resolve_token_contract(
        &self,
        root_model: &Address,
        seller_pubkey: &Value,
        nonce: u64,
    ) -> Result<Address> {
        let v = self
            .client
            .run_getter_retrying(
                root_model,
                ROOTMODEL_ABI,
                "getTokenContractAddress",
                json!({ "sellerPubkey": seller_pubkey, "nonce": nonce }),
            )
            .await?
            .ok_or_else(|| anyhow!("RootModel is not active"))?;
        Address::parse(v["value0"].as_str().ok_or_else(|| anyhow!("no address"))?)
    }

    /// Derive the per-deal `TokenContract` address from the deploy **INIT-DATA (stateInit)** -- the
    /// getter-free, offline counterpart to [`resolve_token_contract`](Self::resolve_token_contract).

    /// `provision_market`'s idempotency check must NOT depend on the RootModel `getTokenContractAddress`
    /// network getter: on a fresh provision the RootModel deploy was just sent but is not yet `Active`, so the
    /// getter 404s and `resolve_token_contract`'s `"RootModel is not active"` error would abort the **entire**
    /// idempotent provision -- exactly the case the check exists to handle. The TC address is `hash(stateInit)`
    /// over `{code, varInit {_sellerPubkey,_rootModelAddress,_nonce,_pubkey}}`; it needs no RootModel account,
    /// no network, and cannot 404. (Bit-for-bit the address the deploy creates -- cross-checked against the
    /// getter only on the idempotent-skip branch, where the RootModel is guaranteed `Active`.)

    /// **The deal terms are no longer arguments here (4.0.36).** They never entered the address --
    /// only the three statics and the pubkey do -- but this used to take them because it shared a
    /// deploy-message builder with the deploy itself. The deploy moved into the note, so the sharing
    /// is gone and with it the arguments that only the message body ever needed. Passing terms to an
    /// address derivation invited the belief that terms are part of the address, which is the exact
    /// belief the constructor's sender check exists to correct.
    pub async fn token_contract_deploy_address(
        &self,
        seller: &KeyPair,
        root_model: &Address,
        nonce: u64,
    ) -> Result<Address> {
        self.token_contract_stateinit_address(seller.public_hex(), root_model, nonce)
            .await
    }

    /// Read the endpoint ciphertext from `TokenContract` -- getter
    /// `getEndpointCipher`. The same `Handover` format as in (the buyer
    /// decrypts with the note key). `None` if the contract is not active or the endpoint is not yet written.
    pub async fn read_handover(&self, token_contract: &Address) -> Result<Option<Vec<u8>>> {
        let Some(v) = self
            .client
            .run_getter_retrying(
                token_contract,
                TOKENCONTRACT_ABI,
                "getEndpointCipher",
                json!({}),
            )
            .await?
        else {
            return Ok(None);
        };
        let hex = v["value0"].as_str().unwrap_or("");
        let hex = hex.strip_prefix("0x").unwrap_or(hex);
        if hex.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_hex(hex)?))
    }

    /// The deployed TC's stored `_modelHash` (4.0.6 `getModelHash() = sha256(modelName)`), normalized to
    /// `0x` + 64 lowercase hex. Used to assert the deal TC is for the SAME model as the order book
    /// (`model_hash`) before posting -- the 4.0.6 end-to-end model-name invariant.
    pub async fn token_contract_model_hash(&self, tc: &Address) -> Result<Option<String>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getModelHash", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let raw = v["value0"].as_str().unwrap_or("");
        let hex = raw.strip_prefix("0x").unwrap_or(raw);
        if hex.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!("0x{}", format!("{hex:0>64}").to_lowercase())))
    }

    /// The TC's on-chain **model display name** (`getModelName() -> string`, 4.0.6) -- the authoritative name
    /// for the accounting view: the manifest's `frame_model` is operator-supplied and must NOT be
    /// trusted as chain truth. `None` if the TC is not active or the name is empty.
    pub async fn token_contract_model_name(&self, tc: &Address) -> Result<Option<String>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getModelName", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let name = v["value0"].as_str().unwrap_or("");
        if name.is_empty() {
            return Ok(None);
        }
        Ok(Some(name.to_string()))
    }

    /// The TC's on-chain **price per tick** (`getDeal() -> (tickSize, pricePerTick, maxTicks)`, 4.0.6) -- the
    /// authoritative deal price for the accounting view, NOT the operator-supplied manifest value.
    /// `uint128` decimal string. `None` if the TC is not active.
    pub async fn token_contract_price_per_tick(&self, tc: &Address) -> Result<Option<u128>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getDeal", json!({}))
            .await?
        else {
            return Ok(None);
        };
        Ok(getter_u128(&v, "pricePerTick"))
    }

    /// The TC's authoritative deal terms (`getDeal() -> tickSize, pricePerTick, maxTicks`).
    /// These are the values the seller must advertise in `postSellOffer`; CLI prompt/default values are not
    /// allowed to drift from this already-deployed per-deal contract.
    pub async fn token_contract_deal_terms(
        &self,
        tc: &Address,
    ) -> Result<Option<(u128, u128, u128)>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getDeal", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let Some(tick_size) = getter_u128(&v, "tickSize") else {
            return Ok(None);
        };
        let Some(price_per_tick) = getter_u128(&v, "pricePerTick") else {
            return Ok(None);
        };
        let Some(max_ticks) = getter_u128(&v, "maxTicks") else {
            return Ok(None);
        };
        Ok(Some((tick_size, price_per_tick, max_ticks)))
    }

    /// Read the **buyer's ed25519 pubkey** from `TokenContract` (`getBuyerPubkey`, uint256) -- the book
    /// records it on a match (`placeInferenceBuy`). From it the seller **reconstructs the x25519 handover**
    /// and encrypts the endpoint to
    /// the recovered pubkey -- no separate x25519 channel is needed. `None` if the TC is not active or the buyer
    /// is not yet recorded (zero pubkey). The pubkey round-trips as `0x`-hex (like `getOwnerPubkey`).
    pub async fn token_contract_buyer_pubkey(&self, tc: &Address) -> Result<Option<[u8; 32]>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getBuyerPubkey", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let raw = v["value0"].as_str().unwrap_or("");
        let hex = raw.strip_prefix("0x").unwrap_or(raw);
        if hex.is_empty() {
            return Ok(None);
        }
        // uint256 -> 32 bytes BE (the pubkey may have arrived without leading zeros -- left-pad to 64 hex).
        let bytes = decode_hex(&format!("{hex:0>64}"))?;
        if bytes.len() != 32 {
            return Err(anyhow!(
                "getBuyerPubkey: expected 32 bytes of ed25519, got {}",
                bytes.len()
            ));
        }
        if bytes.iter().all(|&b| b == 0) {
            return Ok(None); // buyer not yet recorded
        }
        let mut ed = [0u8; 32];
        ed.copy_from_slice(&bytes);
        Ok(Some(ed))
    }

    async fn token_contract_parties(&self, tc: &Address) -> Result<Option<TokenContractParties>> {
        let display_tc = display_token_contract(tc);
        let Some(value) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getParties", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let buyer = value["buyer"].as_str().ok_or_else(|| {
            anyhow!("TokenContract {display_tc} getParties() has no buyer address")
        })?;
        let seller_note = value["sellerNote"].as_str().ok_or_else(|| {
            anyhow!("TokenContract {display_tc} getParties() has no sellerNote address")
        })?;
        Ok(Some(TokenContractParties {
            buyer: normalize_addr(buyer).with_context(|| {
                format!(
                    "TokenContract {display_tc} getParties() buyer {}",
                    display_dexdo_address(buyer)
                )
            })?,
            seller_note: normalize_addr(seller_note).with_context(|| {
                format!(
                    "TokenContract {display_tc} getParties() sellerNote {}",
                    display_dexdo_address(seller_note)
                )
            })?,
        }))
    }

    /// Read the buyer note address from `TokenContract.getParties()`. `None` means the TC is inactive
    /// or has not recorded a buyer yet.
    pub async fn token_contract_buyer_note(&self, tc: &Address) -> Result<Option<Address>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getParties", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let raw = v["buyer"].as_str().unwrap_or("");
        if raw.is_empty() {
            return Ok(None);
        }
        let addr = Address::parse(raw)?;
        if addr
            .with_workchain()
            .ends_with(":0000000000000000000000000000000000000000000000000000000000000000")
        {
            return Ok(None);
        }
        Ok(Some(addr))
    }

    /// Read the seller pubkey from `TokenContract.getSeller()`. Returned as normalized bare lowercase hex
    /// (no `0x`, left-padding is not significant for the key comparison). `None` means the TC is inactive or
    /// the getter returned an empty/zero pubkey.
    pub async fn token_contract_seller_pubkey(&self, tc: &Address) -> Result<Option<String>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getSeller", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let raw = v["sellerPubkey"]
            .as_str()
            .or_else(|| v["value0"].as_str())
            .unwrap_or("");
        let hex = raw
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X")
            .to_ascii_lowercase();
        let hex = hex.trim_start_matches('0').to_string();
        if hex.is_empty() {
            return Ok(None);
        }
        Ok(Some(hex))
    }

    /// The `InferenceOrderBook` code-cell as base64-BOC -- the `code` argument for
    /// `deployInferenceOrderBook`/`getInferenceOrderBookAddress`. Extracted from the embedded
    /// `.tvc` (StateInit -> `.code`), like `airegistry::abi::Contract::code_boc_b64` in the SDK.
    pub fn inference_orderbook_code_b64() -> Result<String> {
        code_boc_b64(INFERENCE_ORDERBOOK_TVC)
    }

    pub fn canonical_inference_orderbook_address(model_hash: &str) -> Result<Address> {
        inference_orderbook_address_from_model_hash(model_hash)
    }

    /// Deterministic `InferenceOrderBook` address for (model, tick size) -- the note's on-chain getter
    /// `getInferenceOrderBookAddress(code, modelHash, tickSize)`. Success = the note has this
    /// method (meaning it is an inference note). `model_hash` is `0x...` uint256, `tick_size` is uint128.
    pub async fn inference_orderbook_address(
        &self,
        note: &Address,
        model_hash: &str,
        tick_size: u128,
    ) -> Result<Address> {
        let code = Self::inference_orderbook_code_b64()?;
        let v = self
            .client
            .run_getter_retrying(
                note,
                PRIVATENOTE_ABI,
                "getInferenceOrderBookAddress",
                json!({
                    "inferenceOrderBookCode": code,
                    "modelHash": model_hash,
                    "tickSize": tick_size.to_string(),
                }),
            )
            .await?
            .ok_or_else(|| anyhow!("note is not active"))?;
        Address::parse(v["value0"].as_str().ok_or_else(|| anyhow!("no address"))?)
    }

    /// Parameters of the deployed `InferenceOrderBook` -- getter `getParams` (`modelHash`, `tickSize`,
    /// `platformFeeBps`). Confirms that the book came up with the expected parameters. `None` if
    /// the book is not yet active.
    pub async fn inference_orderbook_params(&self, ob: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(ob, INFERENCE_ORDERBOOK_ABI, "getParams", json!({}))
            .await
    }

    /// A signed external contract call (write) through the backend's **`DEXDO_USER_AGENT`** http
    /// client: `encode_external_call` (the same codec as `ChainClient::call`) -> submit to
    /// `/v2/messages`. The ChainClient is not used for writes -- its default UA is blocked by
    /// Cloudflare (getters through it work fine). Returns the submit response.
    async fn encode_signed_call_boc(
        addr: &Address,
        abi_json: &str,
        method: &str,
        args: Value,
        keys: &KeyPair,
    ) -> Result<String> {
        let ctx = local_context()?;
        encode_external_call(
            &ctx,
            abi_json,
            &addr.with_workchain(),
            method,
            args,
            keys.public_hex(),
            keys.secret_hex(),
        )
        .await
    }

    async fn submit(
        &self,
        addr: &Address,
        abi_json: &str,
        method: &str,
        args: Value,
        keys: &KeyPair,
    ) -> Result<Value> {
        let boc = Self::encode_signed_call_boc(addr, abi_json, method, args, keys).await?;
        self.send_with_retry(&boc).await
    }

    async fn prepare_money_post(
        &self,
        addr: &Address,
        abi_json: &str,
        method: &str,
        args: Value,
        keys: &KeyPair,
    ) -> Result<(String, String, String, String)> {
        self.clock_skew_preflight()
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        let boc = Self::encode_signed_call_boc(addr, abi_json, method, args, keys)
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        let endpoint = self.client.endpoint().to_string();
        let account_id = dest_account_id_hex(&boc)
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        let dapp_id = fetch_dapp_id(&self.http, &endpoint, &account_id)
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        Ok((endpoint, boc, account_id, dapp_id))
    }

    async fn submit_money_call_once(
        &self,
        addr: &Address,
        abi_json: &str,
        method: &str,
        args: Value,
        keys: &KeyPair,
    ) -> Result<Value> {
        let (endpoint, boc, account_id, dapp_id) = self
            .prepare_money_post(addr, abi_json, method, args, keys)
            .await?;
        send_message_routed_money_once(
            &self.money_post_http,
            &endpoint,
            &boc,
            &account_id,
            &dapp_id,
        )
        .await
    }

    /// Submit `boc` to `/v2/messages`. `deploy` selects the routing:
    /// - `false` -- a regular write to an **existing** contract (call/fund): `send_message`, which
    /// reads the real `dapp_id` via the BK REST `/v2/account`. A 404 there is a real error -> propagates.
    /// - `true` -- a **deploy-message send** whose destination is a not-yet-deployed self-dapp address:
    /// read the real `dapp_id`, but on the **specific `/v2/account` uninit-404** ([`is_uninit_account_404`])
    /// fall back to `dapp_id = account_id` (self-dapp) and submit via `send_message_routed` (which skips
    /// the `/v2/account` read). This lets one `dexdo provision` land a fresh deploy in a SINGLE shot
    /// instead of dying on the first attempt and forcing a cumulative re-funded retry.
    /// **Scoped:** only the deploy/fund submit sites pass `deploy = true`; every regular write keeps the
    /// unchanged `send_message` path. Any non-`/v2/account` 404 (or other error) still propagates.
    async fn submit_once(&self, boc: &str, deploy: bool) -> Result<Value> {
        let endpoint = self.client.endpoint();
        if !deploy {
            return send_message_checked(&self.http, &self.money_post_http, endpoint, boc).await;
        }
        let account_id = dest_account_id_hex(boc)?;
        let dapp_id = match fetch_dapp_id(&self.http, endpoint, &account_id).await {
            Ok(d) => d,
            Err(e) if is_uninit_account_404(&e.to_string()) => account_id.clone(),
            Err(e) => return Err(e),
        };
        send_message_routed_checked(
            &self.money_post_http,
            endpoint,
            boc,
            &account_id,
            &dapp_id,
            None,
        )
        .await
    }

    /// Submit a message to the chain with retry on the shared transient transport classification or
    /// the block manager's explicit `QUEUE_OVERFLOW` answer. `deploy` is threaded to [`submit_once`]
    /// so only deploy-message sends get the funded-uninit `/v2/account` 404 tolerance.
    async fn retry_submit(&self, boc: &str, deploy: bool) -> Result<Value> {
        self.clock_skew_preflight().await?;
        let mut delay = crate::params::TRANSIENT_SUBMIT_INITIAL_BACKOFF;
        for attempt in 1..=crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL {
            match self.submit_once(boc, deploy).await {
                Ok(v) => return Ok(v),
                Err(e) if is_transient_submit_failure(&e) => {
                    eprintln!(
                        "chain transient submit error (attempt {attempt}): {e}; waiting {delay:?} then retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * crate::params::TRANSIENT_SUBMIT_BACKOFF_MULTIPLIER)
                        .min(crate::params::TRANSIENT_SUBMIT_MAX_BACKOFF);
                }
                Err(e) => return Err(e),
            }
        }
        // Final attempt -- pass the result through as-is (Ok or the final error).
        self.submit_once(boc, deploy).await
    }

    /// Regular write to an **existing** contract (call/fund) -- unchanged `send_message` routing.
    pub(super) async fn send_with_retry(&self, boc: &str) -> Result<Value> {
        self.retry_submit(boc, false).await
    }

    /// A **deploy-message** send (its destination is a not-yet-deployed self-dapp address): tolerates
    /// the funded-uninit `/v2/account` 404 via self-dapp routing. Use ONLY for deploy submits.
    async fn send_deploy_with_retry(&self, boc: &str) -> Result<Value> {
        self.retry_submit(boc, true).await
    }

    /// The owner note deploys `InferenceOrderBook` (`deployInferenceOrderBook(code, modelHash,
    /// tickSize)`, signed with the note's owner key). The book is deployed by the note itself: it passes its
    /// `depositIdentifierHash`, and the book's ctor checks that the deployer is a genuine note. Returns
    /// the submit result; wait for book activation by polling `inference_orderbook_address`.
    pub async fn deploy_inference_orderbook(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        model_hash: &str,
        model_name: &str,
        tick_size: u128,
    ) -> Result<Value> {
        let code = Self::inference_orderbook_code_b64()?;
        // 4.0.6: the book's ctor verifies `sha256(modelName) == modelHash`, so `model_hash` MUST be
        // `sha256(model_name)` (the canonical preimage). `inferenceOrderBookCode`/`tickSize` are not in
        // the 2-arg ABI (the OB code is stored on the note) -- harmless extra keys, the encoder ignores them.
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "deployInferenceOrderBook",
            json!({
                "inferenceOrderBookCode": code,
                "modelHash": model_hash,
                "modelName": model_name,
                "tickSize": tick_size.to_string(),
            }),
            owner_keys,
        )
        .await
    }

    /// The book's `getBestBidAsk` getter (`hasBid`, `bid`, `hasAsk`, `ask`) -- a check that the offer landed
    /// in the order book as an ask. `None` if the book is not active.
    pub async fn inference_orderbook_best_bid_ask(&self, ob: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(ob, INFERENCE_ORDERBOOK_ABI, "getBestBidAsk", json!({}))
            .await
    }

    /// Poll THIS note's owner-facing `InferenceFilledConfirmed` ext-out
    /// and advance a durable cursor. The side is owner-relative: `want_is_buy=true` for the buyer's note,
    /// `false` for the seller's note. The caller decides whether/how long to sleep between polls.
    pub async fn poll_inference_filled_tcs(
        &self,
        note: &Address,
        order_book: &Address,
        want_is_buy: bool,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>> {
        let acct = note.with_workchain();
        let account_id = acct.strip_prefix("0:").unwrap_or(&acct).to_string();
        let want_ob = Address::parse(&order_book.with_workchain())
            .map(|a| a.with_workchain())
            .unwrap_or_else(|_| order_book.with_workchain());
        let endpoint = self.client.endpoint();
        let gql = format!("{}/graphql", endpoint.trim_end_matches('/'));
        let query = r#"
            query($accountId: String!, $dappId: String!, $last: Int!) {
              blockchain {
                account(account_id: $accountId, dapp_id: $dappId) {
                  messages(msg_type: [ExtOut], last: $last) {
                    edges { node { body created_at } }
                  }
                }
              }
            }
        "#;
        // both requests go through the one read retry policy, like every other reader in
        // this file. Without it this poll was the buyer's only account of money already POSTed and
        // a single connection reset on the dapp-id lookup ended the purchase with `ambiguous
        // submit` -- measured on live shellnet, ten seconds into a 300-second wait. The retry is
        // bounded by the policy: an exhausted read still leaves the loop, it just is not
        // one unanswered request any more.
        let resp: Value = retry_transient_read(|| async {
            // the dapp-id lookup is a request of its own, so it takes a slot of its own, and
            // so does the ext-out query below -- this reader has its own inline GraphQL and never
            // reached the pager's gate.
            self.client.gate().admit().await;
            let dapp_id = fetch_dapp_id(&self.http, endpoint, &account_id).await?;
            self.client.gate().admit().await;
            let response = self
                .http
                .post(&gql)
                .json(&json!({
                    "query": query,
                    "variables": { "accountId": account_id, "dappId": dapp_id, "last": 200 },
                }))
                .send()
                .await?;
            Ok(chain_response_for_status(response).await?.json().await?)
        })
        .await?;
        let edges = resp["data"]["blockchain"]["account"]["messages"]["edges"]
            .as_array()
            .ok_or_else(|| anyhow!("note ext-out GraphQL shape changed: {resp}"))?;
        let mut matches = Vec::<(i64, MatchedFill)>::new();
        for edge in edges {
            let node = &edge["node"];
            let created = node["created_at"]
                .as_i64()
                .or_else(|| node["created_at"].as_str().and_then(|s| s.parse().ok()));
            let Some(body) = node["body"].as_str() else {
                continue;
            };
            match super::note_events::decode_inference_filled(body) {
                Ok(Some(fill)) => {
                    if fill.is_buy != want_is_buy {
                        continue;
                    }
                    let got_ob = Address::parse(&fill.order_book)
                        .map(|a| a.with_workchain())
                        .unwrap_or(fill.order_book.clone());
                    if got_ob != want_ob {
                        continue;
                    }
                    let created_at = created.ok_or_else(|| {
                        anyhow!(
                            "InferenceFilledConfirmed ext-out on note {account_id} has no created_at cursor"
                        )
                    })?;
                    let tc = Address::parse(&fill.token_contract)
                        .map_err(|e| {
                            anyhow!(
                                "InferenceFilledConfirmed tokenContract {}: {e}",
                                display_token_contract(&fill.token_contract)
                            )
                        })?
                        .with_workchain();
                    matches.push((
                        created_at,
                        MatchedFill {
                            order_id: fill.order_id,
                            token_contract: tc,
                            ticks: fill.ticks,
                            price_per_tick: fill.price_per_tick,
                        },
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(anyhow!(
                        "decode InferenceFilledConfirmed ext-out on note {account_id}: {e}"
                    ));
                }
            }
        }
        Ok(consume_new_fill_batch(cursor, matches))
    }

    pub(super) async fn seller_offer_events_since(
        &self,
        note: &Address,
        order_book: &Address,
        token_contract: &Address,
        since: u64,
    ) -> Result<SellerOfferEvents> {
        let acct = note.with_workchain();
        let account_id = acct.strip_prefix("0:").unwrap_or(&acct).to_string();
        let want_ob = order_book.with_workchain();
        let want_tc = token_contract.with_workchain();
        let endpoint = self.client.endpoint();
        let gql = format!("{}/graphql", endpoint.trim_end_matches('/'));
        // the dapp-id lookup is a request of its own, so it takes a slot of its own.
        self.client.gate().admit().await;
        let dapp_id = fetch_dapp_id(&self.http, endpoint, &account_id).await?;
        let query = r#"
            query($accountId: String!, $dappId: String!, $last: Int!) {
              blockchain {
                account(account_id: $accountId, dapp_id: $dappId) {
                  messages(msg_type: [ExtOut, IntIn], last: $last) {
                    edges { node { body src value created_at } }
                  }
                }
              }
            }
        "#;
        // this reader queries the ext-out surface with its own inline GraphQL, so it never
        // reached the pager's gate. One request, one admission.
        self.client.gate().admit().await;
        let response = self
            .http
            .post(&gql)
            .json(&json!({
                "query": query,
                "variables": { "accountId": account_id, "dappId": dapp_id, "last": 200 },
            }))
            .send()
            .await?;
        let response: Value = chain_response_for_status(response).await?.json().await?;
        let edges = response["data"]["blockchain"]["account"]["messages"]["edges"]
            .as_array()
            .ok_or_else(|| anyhow!("seller offer outcome GraphQL shape changed: {response}"))?;
        let mut outcome = SellerOfferEvents::default();
        for edge in edges {
            let node = &edge["node"];
            let created_at = node["created_at"].as_u64().or_else(|| {
                node["created_at"]
                    .as_str()
                    .and_then(|value| value.parse().ok())
            });
            if created_at.is_none_or(|created_at| created_at < since) {
                continue;
            }
            if let Some(body) = node["body"].as_str().filter(|body| !body.is_empty()) {
                if let Some(placed) = super::note_events::decode_inference_placed(body)? {
                    if !placed.is_buy
                        && placed.order_book.eq_ignore_ascii_case(&want_ob)
                        && placed.token_contract.eq_ignore_ascii_case(&want_tc)
                    {
                        outcome.placed_order_id = Some(placed.order_id);
                    }
                }
                if let Some(fill) = super::note_events::decode_inference_filled(body)? {
                    if !fill.is_buy
                        && fill.order_book.eq_ignore_ascii_case(&want_ob)
                        && fill.token_contract.eq_ignore_ascii_case(&want_tc)
                    {
                        outcome.matched = true;
                    }
                }
            }
            let source_matches = node["src"]
                .as_str()
                .is_some_and(|source| source.eq_ignore_ascii_case(&want_ob));
            let empty_body = node["body"].as_str().is_none_or(str::is_empty);
            if source_matches && empty_body && value_u128(&node["value"]) == Some(1_000_000_000) {
                outcome.placement_value_returned = true;
            }
        }
        Ok(outcome)
    }

    /// Scan paginated fill history with order-id attribution for the inert subscription journal.
    pub async fn poll_inference_attributed_fills(
        &self,
        note: &Address,
        order_book: &Address,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<(u128, MatchedFill)>> {
        let acct = note.with_workchain();
        let account_id = acct.strip_prefix("0:").unwrap_or(&acct).to_string();
        let want_ob = Address::parse(&order_book.with_workchain())
            .map(|a| a.with_workchain())
            .unwrap_or_else(|_| order_book.with_workchain());
        let messages = retry_transient_read(|| {
            fetch_all_ext_out_messages(
                self.client.gate(),
                &self.http,
                self.client.endpoint(),
                &account_id,
                |message| Ok(Some(message)),
            )
        })
        .await?;
        let mut matches = Vec::<(i64, u128, MatchedFill)>::new();
        for message in messages {
            match super::note_events::decode_attributed_inference_filled(&message.body) {
                Ok(Some(fill)) => {
                    if !fill.is_buy {
                        continue;
                    }
                    let got_ob = Address::parse(&fill.order_book)
                        .map(|a| a.with_workchain())
                        .unwrap_or(fill.order_book.clone());
                    if got_ob != want_ob {
                        continue;
                    }
                    let created_at = message.created_at.try_into().map_err(|_| {
                        anyhow!("InferenceFilledConfirmed ext-out on note {account_id} has created_at above i64")
                    })?;
                    let tc = Address::parse(&fill.token_contract)
                        .map_err(|e| {
                            anyhow!(
                                "InferenceFilledConfirmed tokenContract {}: {e}",
                                display_token_contract(&fill.token_contract)
                            )
                        })?
                        .with_workchain();
                    matches.push((
                        created_at,
                        fill.order_id,
                        MatchedFill {
                            order_id: fill.order_id,
                            token_contract: tc,
                            ticks: fill.ticks,
                            price_per_tick: fill.price_per_tick,
                        },
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(anyhow!(
                        "decode InferenceFilledConfirmed ext-out on note {account_id}: {e}"
                    ));
                }
            }
        }
        matches.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.2.token_contract.cmp(&right.2.token_contract))
                .then_with(|| left.1.cmp(&right.1))
        });
        let mut out = Vec::new();
        let mut consumed = Vec::new();
        let mut unique_new = BTreeSet::new();
        for (created_at, order_id, fill) in matches {
            if cursor.has_seen(created_at, &fill.token_contract) {
                continue;
            }
            if unique_new.insert((created_at, fill.token_contract.clone(), order_id)) {
                consumed.push((created_at, fill.token_contract.clone()));
                out.push((order_id, fill));
            }
        }
        cursor.record_seen_batch(consumed);
        Ok(out)
    }

    /// Wait for THIS note's owner-facing `InferenceFilledConfirmed` ext-out
    /// and return the matched per-deal `TokenContract`. The buyer learns its deal from JUST its own note --
    /// no shared-book index. Polls the note's ext-out via the chain GraphQL (the same `messages(ExtOut)`
    /// surface the live giver diag uses), decodes each body, and returns the first fill that is this note's
    /// BUY side on the derived `order_book`, ignoring events older than `since_unix` (a note may carry a
    /// prior deal's fill). Fails closed on timeout -- never a silent empty.
    pub async fn wait_inference_filled_tc(
        &self,
        note: &Address,
        order_book: &Address,
        _since_unix: i64,
        timeout: std::time::Duration,
        cursor: &mut MatchWatchCursor,
        expected: Option<&MatchedFill>,
    ) -> Result<MatchedFill> {
        let acct = note.with_workchain();
        let account_id = acct.strip_prefix("0:").unwrap_or(&acct).to_string();
        let want_ob = Address::parse(&order_book.with_workchain())
            .map(|a| a.with_workchain())
            .unwrap_or_else(|_| order_book.with_workchain());
        let timeout_context = format!(
            "timed out waiting for InferenceFilledConfirmed on note {account_id} (no buy match \
             on book {want_ob} yet for tokenContract {} ticks {} price_per_tick {}) -- the seller's offer \
             may not be resting, or the match didn't go through",
            expected
                .map(|fill| fill.token_contract.as_str())
                .unwrap_or("<resume-any>"),
            expected.map(|fill| fill.ticks).unwrap_or(0),
            expected.map(|fill| fill.price_per_tick).unwrap_or(0)
        );
        wait_correlated_inference_fill(
            &RealInferenceFillPoller {
                chain: self,
                note,
                order_book,
            },
            cursor,
            expected,
            timeout,
            crate::params::INFERENCE_FILL_POLL_INTERVAL,
            &timeout_context,
        )
        .await
    }

    /// The book's `getStats` getter (`nextOrderId`, `orderCount`, `executedNotional`, `executedTicks`).
    pub async fn inference_orderbook_stats(&self, ob: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(ob, INFERENCE_ORDERBOOK_ABI, "getStats", json!({}))
            .await
    }

    /// Reconcile accepted subscription placements at or above a pre-POST order-id floor.

    /// A subscription is an ordinary flagged BUY order now, so the identifying terms are the order's own
    /// price and volume; there are no cycle budgets or auto-renewal to match on any more. The term is not
    /// matched either -- it is [`SUBSCRIPTION_WEEKS`] for every subscription in the protocol.
    pub async fn inference_subscription_placements_since(
        &self,
        ob: &Address,
        buyer_note: &Address,
        order_id_floor: u128,
        max_price_per_tick: u128,
        ticks: u128,
    ) -> Result<Vec<InferenceSubscriptionPlacement>> {
        let account_id = ob.bare().to_string();
        let messages = retry_transient_read(|| {
            fetch_all_ext_out_messages(
                self.client.gate(),
                &self.http,
                self.client.endpoint(),
                &account_id,
                |message| Ok(Some(message)),
            )
        })
        .await?;
        let buyer_note = buyer_note.with_workchain();
        let mut placements = Vec::new();
        for message in messages {
            let Some(mut placement) =
                super::order_events::decode_subscription_placement(&message.body)?
            else {
                continue;
            };
            let owner = Address::parse(&placement.buyer_note)
                .map_err(|error| {
                    anyhow!(
                        "InferenceSubscriptionPlaced buyerNote {}: {error}",
                        display_dexdo_address(&placement.buyer_note)
                    )
                })?
                .with_workchain();
            if placement.order_id < order_id_floor {
                continue;
            }
            placement.buyer_note = owner;
            placement.created_at = message.created_at.try_into().map_err(|_| {
                anyhow!(
                    "InferenceSubscriptionPlaced order #{} created_at exceeds i64",
                    placement.order_id
                )
            })?;
            placements.push(placement);
        }
        coalesce_correlated_subscription_placements(
            placements,
            &buyer_note,
            max_price_per_tick,
            ticks,
        )
    }

    /// Owner-facing BUY outcome facts one note has in this book, oldest first.

    /// The book emits a distinct event per outcome and names the owning note on each, so the durable buyer
    /// submit record is resolved by the outcome that happened rather than by the order's absence.
    pub async fn inference_buyer_order_facts(
        &self,
        ob: &Address,
        buyer_note: &Address,
    ) -> Result<Vec<BuyerOrderFact>> {
        let account_id = ob.bare().to_string();
        let messages = retry_transient_read(|| {
            fetch_all_ext_out_messages(
                self.client.gate(),
                &self.http,
                self.client.endpoint(),
                &account_id,
                |message| Ok(Some(message)),
            )
        })
        .await?;
        let buyer_note = buyer_note.with_workchain();
        let mut facts = Vec::new();
        for message in messages {
            let created_at = i64::try_from(message.created_at).map_err(|_| {
                anyhow!(
                    "InferenceOrderBook ext-out {} created_at exceeds i64",
                    message.id
                )
            })?;
            let Some(fact) =
                super::book_events::decode_buyer_order_fact(&message.body, created_at)?
            else {
                continue;
            };
            let owner = Address::parse(&fact.note)
                .map_err(|error| {
                    anyhow!(
                        "InferenceOrderBook event note {}: {error}",
                        display_dexdo_address(&fact.note)
                    )
                })?
                .with_workchain();
            if !owner.eq_ignore_ascii_case(&buyer_note) {
                continue;
            }
            facts.push(BuyerOrderFact {
                note: owner,
                ..fact
            });
        }
        Ok(facts)
    }

    /// The book's `getWeeklyMedianPrice` getter. `None` means the book is inactive; a live active
    /// book with no matched volume returns the contract's `ERR_NO_LIQUIDITY` through the TVM getter error.
    pub async fn inference_orderbook_weekly_median_price(
        &self,
        ob: &Address,
    ) -> Result<Option<u128>> {
        let Some(v) = self
            .client
            .run_getter_retrying(
                ob,
                INFERENCE_ORDERBOOK_ABI,
                "getWeeklyMedianPrice",
                json!({}),
            )
            .await?
        else {
            return Ok(None);
        };
        let raw = v
            .get("price")
            .or_else(|| v.get("value0"))
            .ok_or_else(|| anyhow!("getWeeklyMedianPrice returned unexpected shape: {v:?}"))?;
        value_u128(raw)
            .ok_or_else(|| anyhow!("getWeeklyMedianPrice returned non-u128 price: {v:?}"))
            .map(Some)
    }

    /// The book's `getOrder(id)` getter -- resolves a specific order/offer (note, `tokenContract`, price...).
    pub async fn inference_orderbook_order(&self, ob: &Address, id: u128) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(
                ob,
                INFERENCE_ORDERBOOK_ABI,
                "getOrder",
                json!({ "id": id.to_string() }),
            )
            .await
    }

    pub async fn inference_buyer_order_is_active_for_owner(
        &self,
        ob: &Address,
        order_id: u128,
        owner_note: &str,
    ) -> Result<bool> {
        let Some(order) = self.inference_orderbook_order(ob, order_id).await? else {
            return Err(anyhow!(
                "getOrder({order_id}) returned no fixed-id row; only an explicit all-zero \
                 tombstone proves that the expected subscription order is absent"
            ));
        };
        subscription_order_is_active_for_owner(order_id, &order, owner_note)
    }

    /// The deal's `getSubscription()` getter on the `TokenContract`.

    /// A subscription is no longer a book-side primitive with cycles and auto-renewal: the book matches a
    /// flagged AON buy order and the resulting deal carries the whole term. So the authoritative
    /// subscription state lives on the per-deal TC, and `sub_weeks == 0` simply means an ordinary deal.
    pub async fn token_contract_subscription(
        &self,
        tc: &Address,
    ) -> Result<Option<DealSubscription>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getSubscription", json!({}))
            .await?
        else {
            return Ok(None);
        };
        DealSubscription::decode_getter(&v)
            .map(Some)
            .map_err(|reason| anyhow!("TokenContract {}: {reason}", display_token_contract(tc)))
    }

    /// The deal's `getOffer()` getter on the `TokenContract`.

    /// `offerPosted` is the `_offerPosted` latch: a TC with it set drops `postFromNote` on the floor
    /// (`contracts/airegistry/TokenContract.sol:713`), so it is the fact a seller must read before
    /// believing a successor ask can rest. The book clears it through `onSellClosed` when the ask
    /// leaves WITHOUT a fill -- cancel or expiry -- which is what keeps the same live TC re-listable
    /// (`contracts/airegistry/TokenContract.sol:729-736`).
    pub async fn token_contract_offer(&self, tc: &Address) -> Result<Option<DealOfferLatch>> {
        let Some(v) = self
            .client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getOffer", json!({}))
            .await?
        else {
            return Ok(None);
        };
        DealOfferLatch::decode_getter(&v)
            .map(Some)
            .map_err(|reason| anyhow!("TokenContract {}: {reason}", display_token_contract(tc)))
    }

    /// Permissionlessly clear an order whose deadline has passed, refunding its escrow.

    /// SELL deadlines are contract-mandatory; BUY deadline 0/GTC is contract-permitted, while the dexdo CLI
    /// deliberately enforces a finite BUY deadline. Lazy expiry-on-match cannot reach an order that never
    /// crosses, so a stale order at an untouched price level needs this external sweep to leave the book at
    /// all. The deployed book owns the time and refund semantics; the caller supplies only the order id and
    /// is not paid for the work.
    pub async fn expire_inference_order(&self, ob: &Address, order_id: u128) -> Result<Value> {
        self.submit(
            ob,
            INFERENCE_ORDERBOOK_ABI,
            "expireOrder",
            json!({ "orderId": order_id.to_string() }),
            &KeyPair::generate(),
        )
        .await
    }

    /// The seller submits exactly one owner-signed external call to the note:
    /// `postSellOffer(flags, nonce, ttl)`. In 4.0.26 the note derives the canonical per-deal
    /// `TokenContract`, and the TC supplies its constructor-bound model, price, maximum ticks,
    /// and seller note when it posts the ask internally. `flags=0` is a plain resting limit.

    /// `ttl` is the offer's lifetime in SECONDS and is MANDATORY: a sell offer
    /// commits no collateral at post time, so it must auto-expire. The note rejects `ttl == 0`
    /// (no GTC asks) and `ttl > MAX_SELL_TTL` (1 hour) with `ERR_SELL_DEADLINE_TOO_LONG`, then
    /// converts it to an absolute deadline anchored at the seller's call -- so time spent reaching
    /// the book counts against the offer's life rather than extending it.
    pub async fn post_sell_offer(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        flags: u8,
        nonce: u64,
        ttl: u64,
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "postSellOffer",
            json!({
                "flags": flags,
                "nonce": nonce.to_string(),
                "ttl": ttl.to_string(),
            }),
            owner_keys,
        )
        .await
    }

    /// Say WHICH pocket is short before the book answers `ERR_LOW_VALUE` for either.

    /// Read-only, and it fails OPEN: a balance that will not read is not evidence of a shortfall,
    /// and refusing a buy on a failed read would cost the buyer an order for the chain's silence.
    async fn refuse_buy_order_the_note_cannot_place(
        &self,
        note: &Address,
        escrow: u128,
    ) -> Result<()> {
        let Ok(Some(account)) = self.client.get_account_retrying(note).await else {
            return Ok(());
        };
        let Ok(private_balance) = self.private_note_shell_balance(note).await else {
            return Ok(());
        };
        let account_ecc = account.ecc_balance(crate::params::SHELL_CURRENCY_ID);
        match buy_order_shortfall(private_balance, account_ecc, escrow) {
            Some(reason) => Err(anyhow!("placeInferenceBuy refused before submit: {reason}")),
            None => Ok(()),
        }
    }

    /// The buyer (note) places a limit buy for inference -- `placeInferenceBuy(modelHash,
    /// maxPricePerTick, ticks, escrow, flags, deadline)` (signed with the note's owner key). The
    /// escrow is SHELL currency 2: the note moves `escrow` from `getDetails().balance[2]` into the book.
    /// If `maxPricePerTick` >= the resting ask -- a match happens immediately (the book calls
    /// `fundFromOrderBook` on the TC).

    /// `deadline` is an absolute unix timestamp. The contract permits zero as GTC, but the dexdo CLI applies
    /// a stricter policy and always submits a finite future deadline. Past it the order is expirable by anyone
    /// (`expire_inference_order`), which refunds the escrow.

    /// `flags::SUBSCRIPTION` selects the fixed four-week take-or-pay term. The book also requires
    /// `flags::AON` plus a volume divisible by that term -- a subscription must come whole from a single
    /// seller, since a half-filled reservation reserves nothing.
    #[allow(clippy::too_many_arguments)]
    pub async fn place_inference_buy(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        model_hash: &str,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
        flags: u8,
        deadline: u64,
    ) -> Result<Value> {
        self.refuse_buy_order_the_note_cannot_place(note, escrow)
            .await?;
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "placeInferenceBuy",
            place_inference_buy_payload(
                model_hash,
                max_price_per_tick,
                ticks,
                escrow,
                flags,
                deadline,
            ),
            owner_keys,
        )
        .await
    }

    /// Prepare the exact signed buy BOC and route, prime the owner-fill cursor, persist its
    /// identity through `before_post`, then use the existing no-redirect money client once.
    #[allow(clippy::too_many_arguments)]
    pub async fn place_inference_buy_with_submit_identity(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        order_book: &Address,
        model_hash: &str,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
        flags: u8,
        deadline: u64,
        cursor: &mut MatchWatchCursor,
        before_post: &mut (dyn FnMut(String, MatchWatchCursor, u128) -> Result<()> + Send),
    ) -> Result<Value> {
        let (endpoint, boc, account_id, dapp_id) = self
            .prepare_money_post(
                note,
                PRIVATENOTE_ABI,
                "placeInferenceBuy",
                place_inference_buy_payload(
                    model_hash,
                    max_price_per_tick,
                    ticks,
                    escrow,
                    flags,
                    deadline,
                ),
                owner_keys,
            )
            .await?;
        let mut final_cursor = MatchWatchCursor::new(0);
        self.poll_inference_filled_tcs(note, order_book, true, &mut final_cursor)
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        *cursor = final_cursor;
        let note_shell_balance = self
            .private_note_shell_balance(note)
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        before_post(
            money_submit_identity(&boc),
            cursor.clone(),
            note_shell_balance,
        )
        .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        retry_buyer_money_submit(
            &self.money_post_http,
            &endpoint,
            &boc,
            &account_id,
            &dapp_id,
        )
        .await
    }

    /// Prepare one BUY money message, anchor its placement/fill cursors,
    /// persist its identity through `before_post`, then POST exactly once.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub async fn place_inference_buy_with_identity_and_cursors(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        order_book: &Address,
        model_hash: &str,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
        flags: u8,
        deadline: u64,
        fill_cursor: &mut MatchWatchCursor,
        before_post: &mut (dyn FnMut(String, u128, MatchWatchCursor, Vec<(u128, MatchedFill)>) -> Result<()>
                  + Send),
    ) -> Result<Value> {
        if flags & crate::market::flags::SUBSCRIPTION != 0 {
            check_subscription_buy_reserve(escrow, ticks, max_price_per_tick)
                .map_err(|error| anyhow!("subscription money preflight: {error}"))?;
            let weeks = u128::from(SUBSCRIPTION_WEEKS);
            if ticks == 0 || !ticks.is_multiple_of(weeks) {
                return Err(anyhow!(
                    "subscription volume {ticks} ticks must be a non-zero multiple of \
                     {SUBSCRIPTION_WEEKS} weeks -- pick e.g. {} or {} ticks",
                    ticks.next_multiple_of(weeks).max(weeks),
                    (ticks / weeks).max(1) * weeks,
                ));
            }
        }
        let (endpoint, boc, account_id, dapp_id) = self
            .prepare_money_post(
                note,
                PRIVATENOTE_ABI,
                "placeInferenceBuy",
                place_inference_buy_payload(
                    model_hash,
                    max_price_per_tick,
                    ticks,
                    escrow,
                    flags,
                    deadline,
                ),
                owner_keys,
            )
            .await?;
        let stats = self
            .inference_orderbook_stats(order_book)
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?
            .ok_or_else(|| {
                anyhow::Error::new(MoneySubmitError::Preparation {
                    source: anyhow!(
                        "InferenceOrderBook {} is not active before subscription POST",
                        display_dexdo_address(order_book)
                    ),
                })
            })?;
        let order_id_floor = stats
            .get("nextOrderId")
            .and_then(value_u128)
            .ok_or_else(|| {
                anyhow::Error::new(MoneySubmitError::Preparation {
                    source: anyhow!("getStats returned no valid nextOrderId: {stats}"),
                })
            })?;
        let pre_post_fills = self
            .poll_inference_attributed_fills(note, order_book, fill_cursor)
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        before_post(
            money_submit_identity(&boc),
            order_id_floor,
            fill_cursor.clone(),
            pre_post_fills,
        )
        .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        retry_buyer_money_submit(
            &self.money_post_http,
            &endpoint,
            &boc,
            &account_id,
            &dapp_id,
        )
        .await
    }

    /// Cancel one resting inference order owned by `note` through `PrivateNote.cancelInferenceOrder`.
    pub async fn cancel_inference_order(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        model_hash: &str,
        order_id: u128,
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "cancelInferenceOrder",
            json!({
                "modelHash": model_hash,
                "orderId": order_id.to_string(),
            }),
            owner_keys,
        )
        .await
    }

    /// Cancel all resting inference orders owned by `note` for one model through the note owner method.
    pub async fn cancel_all_inference_orders(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        model_hash: &str,
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "cancelAllInferenceOrders",
            json!({ "modelHash": model_hash }),
            owner_keys,
        )
        .await
    }

    /// The raw `getState` getter of the `TokenContract`. Production lifecycle consumers must use
    /// [`Self::token_contract_deal_state`] so malformed or incomplete ABI output cannot silently become
    /// an ordinary zero-valued deal.
    pub async fn token_contract_state(&self, tc: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getState", json!({}))
            .await
    }

    /// Strict typed `getState` read.
    pub async fn token_contract_deal_state(&self, tc: &Address) -> Result<Option<DealChainState>> {
        let Some(value) = self.token_contract_state(tc).await? else {
            return Ok(None);
        };
        DealChainState::decode_getter(&value)
            .map(Some)
            .map_err(|reason| anyhow!("TokenContract {}: {reason}", display_token_contract(tc)))
    }

    /// The raw `getSellerBond` getter of the deal. Production lifecycle consumers must use
    /// [`Self::token_contract_deal_seller_bond`].
    pub async fn token_contract_seller_bond(&self, tc: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getSellerBond", json!({}))
            .await
    }

    /// Strict typed `getSellerBond` read.
    pub async fn token_contract_deal_seller_bond(
        &self,
        tc: &Address,
    ) -> Result<Option<DealSellerBond>> {
        let Some(value) = self.token_contract_seller_bond(tc).await? else {
            return Ok(None);
        };
        DealSellerBond::decode_getter(&value)
            .map(Some)
            .map_err(|reason| anyhow!("TokenContract {}: {reason}", display_token_contract(tc)))
    }

    /// The raw `getBuyerBond` getter of the deal. Production accounting consumers must use
    /// [`Self::token_contract_deal_buyer_bond`].
    pub async fn token_contract_buyer_bond(&self, tc: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getBuyerBond", json!({}))
            .await
    }

    /// Strict typed `getBuyerBond` read.
    pub async fn token_contract_deal_buyer_bond(
        &self,
        tc: &Address,
    ) -> Result<Option<DealBuyerBond>> {
        let Some(value) = self.token_contract_buyer_bond(tc).await? else {
            return Ok(None);
        };
        DealBuyerBond::decode_getter(&value)
            .map(Some)
            .map_err(|reason| anyhow!("TokenContract {}: {reason}", display_token_contract(tc)))
    }

    /// Read one coherent strict accounting/lifecycle snapshot.

    /// Each bounded attempt reads one complete four-getter set bracketed by the
    /// account BOC identity. A mutation or destroy between any getters rejects
    /// that attempt; only a new bracketed attempt may be retried, and missing
    /// fields are never filled with defaults.
    pub async fn token_contract_deal_snapshot(
        &self,
        tc: &Address,
    ) -> Result<Option<DealChainSnapshot>> {
        let mut source = LiveDealSnapshotSource {
            chain: self,
            token_contract: tc,
        };
        read_coherent_deal_snapshot(&mut source)
            .await
            .map_err(|error| anyhow!("TokenContract {}: {error}", display_token_contract(tc)))
    }
    /// The `getConfig` getter of the deal (`TokenContract`, 4.0.31 `view`):
    /// `platformFeeBps`, `minClaimInterval`, `minSecondsPerTick`, and `disputeWindow`.
    /// The seller claim driver reads the two claim cadence bounds per deal; the fixed probe and claim
    /// promotion windows are not returned here. `None` if the TC is not active.
    pub async fn token_contract_config(&self, tc: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(tc, TOKENCONTRACT_ABI, "getConfig", json!({}))
            .await
    }

    /// Read-only `PrivateNote.getDetails()`: public balance/lock maps and metadata, no key and no signed call.
    pub async fn private_note_details(&self, note: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await
    }

    /// Name this note's resting inference orders from its own inbound history, and prove each one.

    /// The owner's obligation is to get the money back holding only the note and the key, and
    /// for a resting order that was not possible: `cancelInferenceOrder` is keyed on `modelHash`,
    /// which nothing the note publishes carries. `getOutstanding()` publishes
    /// `tvm.hash(abi.encode(book, orderId))`, one way. The ext-out mirrors publish the book, and a
    /// book address is `computeInferenceOrderBookAddress(code, modelHash)`, also one way. The one
    /// place `modelHash` survives is the book's own inbound calls into this note, so that is what is
    /// walked.

    /// Three things make the answer trustworthy rather than plausible:

    /// - **Removals are subtracted.** `onInferenceOrderRemoved` carries the same pair. A note that
    /// placed five orders and had five removed shows five placements in its history, and reporting
    /// those as resting would send the owner to cancel five orders that no longer exist, paying
    /// gas for each. Measured on the chain note `0:29f4223b...4e`, which has exactly that shape.
    /// - **Each survivor is proved against the note as it is now.** History says what happened; the
    /// getter says what is true. Composing the key from the recovered pair and finding it in
    /// `getOutstanding().orders` joins the two, and a pair that does not appear there is dropped
    /// rather than offered.
    /// - **What is left over is reported, not hidden.** Keys the walk could not explain are money
    /// that is resting under a name this run failed to recover, which is precisely the case the
    /// operator must be told about.
    async fn recover_resting_inference_orders(
        &self,
        note: &Address,
        order_keys: &[String],
    ) -> Result<(Vec<RecoveredRestingOrder>, Vec<String>, NoteHistoryCoverage)> {
        let (placed, removed, history) = self.note_inbound_inference_order_calls(note).await?;
        let (resting, unexplained) =
            resolve_resting_inference_orders(&placed, &removed, order_keys)?;
        Ok((resting, unexplained, history))
    }

    /// Page this note's inbound messages back to the beginning, decoding the two calls that name an
    /// inference order.

    /// The page limit is the node's, not ours, and there is no way to ask what it retains. So the
    /// walk does not guess: it follows `pageInfo.hasPreviousPage` until the node says there is
    /// nothing earlier, and reports whether it got there. That makes completeness a fact measured on
    /// this run rather than a belief about node policy.
    async fn note_inbound_inference_order_calls(
        &self,
        note: &Address,
    ) -> Result<(
        Vec<super::note_events::InferenceOrderCall>,
        Vec<super::note_events::InferenceOrderCall>,
        NoteHistoryCoverage,
    )> {
        let acct = note.with_workchain();
        let account_id = acct.strip_prefix("0:").unwrap_or(&acct).to_string();
        let endpoint = self.client.endpoint();
        let gql = format!("{}/graphql", endpoint.trim_end_matches('/'));
        // the dapp-id lookup is a request of its own, so it takes a slot of its own.
        self.client.gate().admit().await;
        let dapp_id = fetch_dapp_id(&self.http, endpoint, &account_id).await?;
        let query = r#"
            query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
              blockchain {
                account(account_id: $accountId, dapp_id: $dappId) {
                  messages(msg_type: [IntIn], last: $last, before: $before) {
                    pageInfo { hasPreviousPage startCursor }
                    edges { node { body } }
                  }
                }
              }
            }
        "#;

        let mut placed = Vec::new();
        let mut removed = Vec::new();
        let mut history = NoteHistoryCoverage::default();
        let mut before: Option<String> = None;
        loop {
            // this reader queries the ext-out surface with its own inline GraphQL, so it never
            // reached the pager's gate. One request, one admission.
            self.client.gate().admit().await;
            let response = self
                .http
                .post(&gql)
                .json(&json!({
                    "query": query,
                    "variables": {
                        "accountId": account_id,
                        "dappId": dapp_id,
                        "last": crate::params::INT_IN_PAGE_SIZE,
                        "before": before,
                    },
                }))
                .send()
                .await?;
            let response: Value = chain_response_for_status(response).await?.json().await?;
            let messages = &response["data"]["blockchain"]["account"]["messages"];
            let edges = messages["edges"]
                .as_array()
                .ok_or_else(|| anyhow!("note inbound-history GraphQL shape changed: {response}"))?;
            for edge in edges {
                let Some(body) = edge["node"]["body"].as_str().filter(|b| !b.is_empty()) else {
                    continue;
                };
                history.messages_read += 1;
                if let Some(call) = super::note_events::decode_inference_placed_call(body)? {
                    placed.push(call);
                } else if let Some(call) =
                    super::note_events::decode_inference_order_removed_call(body)?
                {
                    removed.push(call);
                }
            }
            let has_previous = messages["pageInfo"]["hasPreviousPage"]
                .as_bool()
                .unwrap_or(false);
            if !has_previous {
                history.reached_beginning = true;
                break;
            }
            // Terminate on a real condition, never on a page budget. The node says whether an
            // earlier page exists; if it claims one but hands back a cursor that does not move, the
            // walk is not advancing and stops -- reported as not having reached the beginning,
            // which is the truth, rather than spinning on a node that contradicts itself.
            let Some(cursor) = messages["pageInfo"]["startCursor"].as_str() else {
                break;
            };
            if before.as_deref() == Some(cursor) {
                break;
            }
            before = Some(cursor.to_string());
        }
        Ok((placed, removed, history))
    }

    /// Read the note's best-effort outstanding mirror and independently check every deal address.

    /// The mirror has two opposite blind spots. Its `bounce:false` fill callback can fail before a
    /// live deal is recorded, so absence here proves nothing. Its `bounce:false` close callback can
    /// also fail, so a destroyed deal can remain recorded. For that reason only a candidate whose
    /// own `getParties` names this note and whose own `getState` reports `funded=true` is returned as
    /// a lead; inactive, destroyed, unfunded, terminal, and identity-mismatched addresses are
    /// retained as explicit refusals.
    pub async fn private_note_outstanding(
        &self,
        note: &Address,
    ) -> Result<PrivateNoteOutstandingReport> {
        let note = normalize_addr(&note.with_workchain())?;
        let note_address = Address::parse(&note)?;
        let value = self
            .client
            .run_getter_retrying(&note_address, PRIVATENOTE_ABI, "getOutstanding", json!({}))
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "PrivateNote {note} getOutstanding returned no data; no deal address is proven"
                )
            })?;
        let (deals, order_keys) = decode_private_note_outstanding(&value)?;
        let (resting_orders, unexplained_order_keys, history) = self
            .recover_resting_inference_orders(&note_address, &order_keys)
            .await
            .with_context(|| format!("recover resting inference orders for PrivateNote {note}"))?;
        let mut report = PrivateNoteOutstandingReport {
            opaque_order_count: order_keys.len(),
            resting_orders,
            unexplained_order_keys,
            history,
            ..Default::default()
        };
        for token_contract in deals {
            let token_contract = token_contract.with_workchain();
            let address = Address::parse(&token_contract)?;
            let parties = self
                .token_contract_parties(&address)
                .await
                .with_context(|| {
                    format!(
                        "validate PrivateNote {note} getOutstanding lead {token_contract} through TokenContract.getParties"
                    )
                })?;
            let state = self
                .token_contract_deal_state(&address)
                .await
                .with_context(|| {
                    format!(
                        "validate PrivateNote {note} getOutstanding lead {token_contract} through TokenContract.getState"
                    )
                })?;
            match classify_outstanding_deal_lead(
                &note,
                &token_contract,
                parties.as_ref(),
                state.as_ref(),
            ) {
                Ok(lead) => report.deal_leads.push(lead),
                Err(refusal) => report.refused_deal_leads.push(refusal),
            }
        }
        Ok(report)
    }

    /// Spendable SHELL recorded by the note contract. Physical account ECC[2] is deployment gas,
    /// not order or seller-bond money on 4.0.33.
    pub async fn private_note_shell_balance(&self, note: &Address) -> Result<u128> {
        let display_note = display_dexdo_address(note);
        let details = self.private_note_details(note).await?.ok_or_else(|| {
            anyhow!(
                "PrivateNote {display_note} getDetails returned no data; spendable balance is unknown"
            )
        })?;
        private_note_balance_currency(&details, crate::params::SHELL_CURRENCY_ID)
            .with_context(|| format!("PrivateNote {display_note} spendable SHELL balance"))
    }

    /// Read every successful owner-signed `placeInferenceBuy` receipt for one note. This is intended
    /// for live by-fact verification: it counts destination transactions, not CLI log events. The
    /// the indexer can omit `body` for external-in messages, so decode the authoritative full
    /// message BOC when that projection is absent.
    pub async fn successful_place_inference_buy_receipts(
        &self,
        note: &Address,
    ) -> Result<Vec<PlaceInferenceBuyReceipt>> {
        const PAGE_SIZE: u32 = 1_000;
        let account_id = note.bare().to_string();
        let endpoint = self.client.endpoint().trim_end_matches('/');
        // the dapp-id lookup is a request of its own, so it takes a slot of its own.
        self.client.gate().admit().await;
        let dapp_id = fetch_dapp_id(&self.http, endpoint, &account_id).await?;
        let gql = format!("{endpoint}/graphql");
        let query = r#"
            query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
              blockchain {
                account(account_id: $accountId, dapp_id: $dappId) {
                  messages(msg_type: [ExtIn], last: $last, before: $before) {
                    pageInfo { startCursor hasPreviousPage }
                    edges {
                      cursor
                      node {
                        id boc body created_at
                        dst_transaction {
                          aborted
                          compute { exit_code success }
                          action { result_code success }
                        }
                      }
                    }
                  }
                }
              }
            }
        "#;
        let mut before: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut receipts = Vec::new();
        loop {
            // this reader queries the ext-out surface with its own inline GraphQL, so it never
            // reached the pager's gate. One request, one admission.
            self.client.gate().admit().await;
            let response = self
                .http
                .post(&gql)
                .json(&json!({
                    "query": query,
                    "variables": {
                        "accountId": account_id.as_str(),
                        "dappId": dapp_id.as_str(),
                        "last": PAGE_SIZE,
                        "before": before.as_deref(),
                    },
                }))
                .send()
                .await?;
            let response: Value = chain_response_for_status(response).await?.json().await?;
            if let Some(errors) = response.get("errors") {
                return Err(anyhow!(
                    "PrivateNote {note} owner-call GraphQL errors: {errors}"
                ));
            }
            let messages = response
                .pointer("/data/blockchain/account/messages")
                .ok_or_else(|| {
                    anyhow!("PrivateNote {note} owner-call GraphQL shape changed: {response}")
                })?;
            let edges = messages["edges"].as_array().ok_or_else(|| {
                anyhow!("PrivateNote {note} owner-call GraphQL edges missing: {response}")
            })?;
            for edge in edges {
                let cursor = edge["cursor"]
                    .as_str()
                    .ok_or_else(|| anyhow!("PrivateNote {note} owner call has no cursor"))?;
                let node = &edge["node"];
                let message_id = node["id"].as_str().unwrap_or(cursor);
                if !seen.insert(message_id.to_string()) || !successful_inbound_call(node) {
                    continue;
                }
                let decoded = node["body"]
                    .as_str()
                    .and_then(|body| decode_external_abi_message(body, PRIVATENOTE_ABI, true))
                    .or_else(|| {
                        node["boc"].as_str().and_then(|boc| {
                            decode_external_abi_message_boc(boc, PRIVATENOTE_ABI, true)
                        })
                    });
                let Some(decoded) = decoded else {
                    continue;
                };
                if decoded.function_name != "placeInferenceBuy" {
                    continue;
                }
                let created_at = node["created_at"]
                    .as_u64()
                    .or_else(|| {
                        node["created_at"]
                            .as_str()
                            .and_then(|value| value.parse().ok())
                    })
                    .ok_or_else(|| {
                        anyhow!("successful PrivateNote placeInferenceBuy has no created_at")
                    })?;
                receipts.push(PlaceInferenceBuyReceipt {
                    message_id: message_id.to_string(),
                    created_at,
                    max_price_per_tick: decoded_u128(&decoded.tokens, "maxPricePerTick")
                        .ok_or_else(|| {
                            anyhow!("placeInferenceBuy receipt has no maxPricePerTick")
                        })?,
                    ticks: decoded_u128(&decoded.tokens, "ticks")
                        .ok_or_else(|| anyhow!("placeInferenceBuy receipt has no ticks"))?,
                    escrow: decoded_u128(&decoded.tokens, "escrow")
                        .ok_or_else(|| anyhow!("placeInferenceBuy receipt has no escrow"))?,
                });
            }
            let Some(next) = previous_page_cursor(
                &format!("PrivateNote {note} owner-call"),
                messages,
                before.as_deref(),
            )?
            else {
                break;
            };
            before = Some(next);
        }
        receipts.sort_by(|left, right| {
            (left.created_at, &left.message_id).cmp(&(right.created_at, &right.message_id))
        });
        Ok(receipts)
    }

    /// Resolve every outbound message produced by this invocation's exact external
    /// `PrivateNote.streamStop` message. These ids can be matched directly against the terminal
    /// TokenContract transaction's inbound message id.
    pub async fn submitted_buyer_stop_out_message_ids(
        &self,
        client_message_id: &str,
        buyer_note: &Address,
    ) -> Result<Option<Vec<String>>> {
        let gql = format!("{}/graphql", self.client.endpoint().trim_end_matches('/'));
        let response: Value = self
            .http
            .post(&gql)
            .json(&json!({
                "query": SUBMITTED_BUYER_STOP_QUERY,
                "variables": { "hash": bare_hex(client_message_id) },
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(errors) = response.get("errors") {
            return Err(anyhow!(
                "submitted buyer STOP message GraphQL errors: {errors}"
            ));
        }
        parse_submitted_buyer_stop_out_message_ids(
            &response,
            client_message_id,
            &buyer_note.with_workchain(),
        )
    }

    /// Resolve which call produced one settlement receipt, from the inbound message of the
    /// transaction that emitted it rather than from the event body.

    /// `StreamStopped` has a single emit site, in `_closeClean()`, which both the buyer's `stop()`
    /// and the seller's `sellerStop()` reach, and its payload names the buyer beneficiary and
    /// never the actor. The discriminator is one level up, in the transaction's inbound message:
    /// `stop()` is `require(msg.sender == _buyer)`, so only an internal message from the recorded
    /// buyer note reaches it, while `sellerStop()` is `onlyOwnerPubkey(_sellerPubkey) accept` and
    /// arrives as an external message. Only internal inbound messages are read here, so a receipt
    /// an external close produced has no call to bind to and fails instead of being reported as a
    /// buyer stop.

    /// Introduced for's live verifier and reused by's production STOP attribution: both
    /// need the same transaction-bound fact, so the client keeps one reader rather than a second
    /// event heuristic.
    pub async fn token_contract_settlement_inbound_call(
        &self,
        token_contract: &Address,
        settlement_message_id: &str,
    ) -> Result<TokenContractInboundCall> {
        let account_id = token_contract.bare().to_string();
        let endpoint = self.client.endpoint().trim_end_matches('/');
        let gql = format!("{endpoint}/graphql");
        // The same account and the same post-terminal `/v2/account` 404 as
        // `token_contract_settlement_receipts`, which reads this deal's ext-out side.
        // the dapp-id lookup is a request of its own, so it takes a slot of its own.
        self.client.gate().admit().await;
        let dapp_id = match fetch_dapp_id(&self.http, endpoint, &account_id).await {
            Ok(dapp_id) => dapp_id,
            Err(error) if is_uninit_account_404(&error.to_string()) => account_id.clone(),
            Err(error) => return Err(error),
        };
        // One projection for both sides, so the receipt id and the inbound message id are compared
        // as the same field of the same type: the ext-out receipt names its emitting transaction
        // through `src_transaction`, and the message that transaction consumed names it back
        // through `dst_transaction`. A transaction executes exactly one inbound message, so that
        // pair is an exact binding rather than a search for a compatible-looking record.
        let query = r#"
            query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
              blockchain {
                account(account_id: $accountId, dapp_id: $dappId) {
                  messages(msg_type: [IntIn, ExtOut], last: $last, before: $before) {
                    pageInfo { startCursor hasPreviousPage }
                    edges {
                      node {
                        id src body
                        src_transaction { id }
                        dst_transaction { id }
                      }
                    }
                  }
                }
              }
            }
        "#;
        let mut before: Option<String> = None;
        let mut nodes = Vec::new();
        loop {
            // this reader queries the ext-out surface with its own inline GraphQL, so it never
            // reached the pager's gate. One request, one admission.
            self.client.gate().admit().await;
            let response = self
                .http
                .post(&gql)
                .json(&json!({
                    "query": query,
                    "variables": {
                        "accountId": bare_hex(&account_id),
                        "dappId": bare_hex(&dapp_id),
                        // The existing account-message pager bound; not a second copy of it.
                        "last": crate::params::EXT_OUT_PAGE_SIZE,
                        "before": before.as_deref(),
                    },
                }))
                .send()
                .await?;
            let response: Value = chain_response_for_status(response).await?.json().await?;
            if let Some(errors) = response.get("errors") {
                return Err(anyhow!(
                    "TokenContract {token_contract} inbound-call GraphQL errors: {errors}"
                ));
            }
            let messages = response
                .pointer("/data/blockchain/account/messages")
                .ok_or_else(|| {
                    anyhow!(
                        "TokenContract {token_contract} inbound-call GraphQL shape changed: \
                         {response}"
                    )
                })?;
            let edges = messages["edges"].as_array().ok_or_else(|| {
                anyhow!("TokenContract {token_contract} inbound-call GraphQL edges missing")
            })?;
            nodes.extend(edges.iter().map(|edge| edge["node"].clone()));
            let Some(next) = previous_page_cursor(
                &format!("TokenContract {token_contract} inbound-call"),
                messages,
                before.as_deref(),
            )?
            else {
                break;
            };
            before = Some(next);
        }
        let emitting = nodes
            .iter()
            .filter(|node| node["id"].as_str() == Some(settlement_message_id))
            .filter_map(|node| node["src_transaction"]["id"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        if emitting.len() != 1 {
            return Err(anyhow!(
                "TokenContract {token_contract} settlement receipt {settlement_message_id} names \
                 {} emitting transactions of this deal, expected exactly one",
                emitting.len()
            ));
        }
        let inbound = nodes
            .iter()
            .filter(|node| node["dst_transaction"]["id"].as_str() == Some(emitting[0].as_str()))
            .collect::<Vec<_>>();
        if inbound.len() != 1 {
            return Err(anyhow!(
                "TokenContract {token_contract} settlement receipt {settlement_message_id} was \
                 emitted by a transaction carrying {} internal inbound messages, expected exactly \
                 one; a close with none was submitted externally, which `stop()` cannot be because \
                 it requires `msg.sender == _buyer`",
                inbound.len()
            ));
        }
        let executed = inbound[0];
        let message_id = executed["id"].as_str().ok_or_else(|| {
            anyhow!("TokenContract {token_contract} inbound call has no message id")
        })?;
        let source = executed["src"].as_str().ok_or_else(|| {
            anyhow!("TokenContract {token_contract} inbound call {message_id} has no sender")
        })?;
        let function = executed["body"]
            .as_str()
            .and_then(|body| decode_internal_abi_call(body, TOKENCONTRACT_ABI))
            .map(|decoded| decoded.function_name)
            .ok_or_else(|| {
                anyhow!(
                    "TokenContract {token_contract} inbound call {message_id} does not decode \
                     against the TokenContract ABI"
                )
            })?;
        Ok(TokenContractInboundCall {
            message_id: message_id.to_string(),
            source: source.to_string(),
            function,
        })
    }

    /// Confirm that the transaction which emitted `settlement_message_id` consumed the buyer
    /// note's internal `TokenContract.stop()` call. Event payload alone cannot prove this because
    /// buyer STOP, seller STOP, and permissionless finalization share terminal event shapes.
    pub async fn settlement_receipt_confirms_buyer_stop(
        &self,
        token_contract: &Address,
        settlement_message_id: &str,
        buyer_note: &Address,
    ) -> Result<bool> {
        let call = self
            .token_contract_settlement_inbound_call(token_contract, settlement_message_id)
            .await?;
        if call.function != "stop" {
            return Ok(false);
        }
        Ok(normalize_addr(&call.source)? == normalize_addr(&buyer_note.with_workchain())?)
    }

    /// Read ordered lifecycle receipts for one deal. `StreamStopped` proves the clean
    /// post-probe-accept split; `ProbeBurned` proves the mutually exclusive probe-burn path.
    pub async fn token_contract_settlement_receipts(
        &self,
        token_contract: &Address,
    ) -> Result<TokenContractSettlementReceipts> {
        let account_id = token_contract.bare().to_string();
        // A TokenContract is its own dapp. Its immutable ext-out history remains queryable after
        // terminal withdrawal destroys the account and `/v2/account` starts returning 404.
        // and it goes through the read retry policy, because this is what the settlement
        // confirmation loop polls after the money POST. A 404 is a real answer and is not retried
        // by the policy, so the uninit branch below keeps seeing it.
        let dapp_id = match retry_transient_read(|| async {
            // the dapp-id lookup is a request of its own, so it takes a slot of its own.
            self.client.gate().admit().await;
            fetch_dapp_id(&self.http, self.client.endpoint(), &account_id).await
        })
        .await
        {
            Ok(dapp_id) => dapp_id,
            Err(error) if is_uninit_account_404(&error.to_string()) => account_id.clone(),
            Err(error) => return Err(error),
        };
        let messages = retry_transient_read(|| {
            fetch_all_ext_out_messages_routed(
                self.client.gate(),
                &self.http,
                self.client.endpoint(),
                &account_id,
                &dapp_id,
                |message| Ok(Some(message)),
            )
        })
        .await?;
        decode_token_contract_settlement_receipts(messages)
    }

    /// Read current getters when active and immutable ext-out history for one TokenContract.
    pub async fn token_contract_receipt_chain_data(
        &self,
        token_contract: &Address,
    ) -> Result<TokenContractReceiptChainData> {
        let account = self.client.get_account(token_contract).await?;
        let account_active = account.as_ref().is_some_and(Account::is_active);
        let code_hash = account
            .as_ref()
            .and_then(|account| account.code_hash.as_deref())
            .and_then(normalize_code_hash);
        let current = if account_active {
            let getter = |name: &'static str| async move {
                self.client
                    .run_getter(token_contract, TOKENCONTRACT_ABI, name, json!({}))
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "active TokenContract {} returned no {name}",
                            token_contract.with_workchain()
                        )
                    })
            };
            Some(TokenContractCurrentFacts {
                state: getter("getState").await?,
                fees: getter("getFees").await?,
                deal: getter("getDeal").await?,
                parties: getter("getParties").await?,
                seller: getter("getSeller").await?,
                version: getter("getVersion").await?,
            })
        } else {
            None
        };
        let receipts = self
            .token_contract_settlement_receipts(token_contract)
            .await?;
        // read the side that reports the money. The deal's own ext-out is silent about the
        // never-opened refund by construction, so the notes party to it are asked what they were
        // credited. Which notes: the current getters when the deal still exists, and -- because the
        // silent path is exactly the one that destroys the deal -- the buyer named in the deal's own
        // funding event, which survives the destruction in the archive.
        let mut notes = Vec::<String>::new();
        let push_note = |candidate: Option<String>, notes: &mut Vec<String>| {
            if let Some(address) = candidate.filter(|address| !address.is_empty()) {
                let normalized = Address::parse(&address)
                    .map(|parsed| parsed.with_workchain())
                    .unwrap_or(address);
                if !notes.contains(&normalized) {
                    notes.push(normalized);
                }
            }
        };
        if let Some(facts) = current.as_ref() {
            push_note(
                facts.parties["buyerNote"].as_str().map(str::to_string),
                &mut notes,
            );
            push_note(
                facts.parties["sellerNote"].as_str().map(str::to_string),
                &mut notes,
            );
        }
        for receipt in &receipts.events {
            if let TokenContractSettlementEvent::StreamFunded { buyer, .. } = &receipt.event {
                push_note(Some(buyer.clone()), &mut notes);
            }
        }
        let mut note_credits = Vec::new();
        for note in &notes {
            note_credits.extend(self.note_deal_credits(note, token_contract).await?);
        }
        Ok(TokenContractReceiptChainData {
            account_id: token_contract.with_workchain(),
            account_active,
            code_hash,
            current,
            receipts,
            note_credits,
            notes_read: notes,
        })
    }

    /// Every `DealCredited` one note emitted that names `token_contract`.

    /// the reader is the existing ext-out pager -- the same one that already scans notes for
    /// order events -- so this adds a note account to what the receipt reads, not a second way of
    /// reading the chain. The filter is the deal address the note itself put in the event, and the
    /// note only emits it after re-deriving that address, so a credit reaches this list only when
    /// the chain has already tied it to this deal.
    async fn note_deal_credits(
        &self,
        note: &str,
        token_contract: &Address,
    ) -> Result<Vec<NoteDealCreditReceipt>> {
        let account_id = note.strip_prefix("0:").unwrap_or(note);
        let account_id = account_id
            .rsplit_once("::")
            .map_or(account_id, |(_, account)| account)
            .to_string();
        let want = token_contract.with_workchain();
        let note_display = note.to_string();
        let messages = retry_transient_read(|| {
            fetch_all_ext_out_messages(
                self.client.gate(),
                &self.http,
                self.client.endpoint(),
                &account_id,
                |message| Ok(Some(message)),
            )
        })
        .await?;
        let mut credits = Vec::new();
        for message in messages {
            let Some(credit) = super::note_events::decode_deal_credited(&message.body)
                .with_context(|| format!("decode PrivateNote event {}", message.id))?
            else {
                continue;
            };
            let deal = Address::parse(&credit.deal)
                .map(|parsed| parsed.with_workchain())
                .unwrap_or_else(|_| credit.deal.clone());
            if deal != want {
                continue;
            }
            credits.push(NoteDealCreditReceipt {
                note: note_display.clone(),
                deal,
                amount: credit.amount,
                message_id: message.id,
                created_at: message.created_at,
                cursor: message.cursor,
            });
        }
        Ok(credits)
    }

    /// Read immutable terminal history before a buyer STOP path prepares or submits money. A
    /// matching terminal event is a successful already-closed observation, not a STOP error and
    /// not proof that this client closed the deal.
    pub async fn buyer_terminal_before_stop(
        &self,
        buyer_note: &Address,
        token_contract: &Address,
    ) -> Result<Option<BuyerStopTerminalReceipt>> {
        let timeout = SellerLivenessParams::canonical().cancel_confirmation_timeout;
        let receipts = tokio::time::timeout(
            timeout,
            self.token_contract_settlement_receipts(token_contract),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "TokenContract {token_contract} terminal event read exceeded the existing canonical \
                 confirmation/read timeout"
            )
        })??;
        select_prior_buyer_terminal_receipt(
            &token_contract.with_workchain(),
            &buyer_note.with_workchain(),
            &receipts,
        )
    }

    async fn reject_prior_settlement_action_before_prepare(
        &self,
        token_contract: &Address,
        action: SettlementAction,
        buyer_actor: Option<&Address>,
    ) -> Result<()> {
        let display_tc = display_token_contract(token_contract);
        let confirmation_timeout = SellerLivenessParams::canonical().cancel_confirmation_timeout;
        let receipts = tokio::time::timeout(
            confirmation_timeout,
            self.token_contract_settlement_receipts(token_contract),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "TokenContract {display_tc} pre-prepare event snapshot exceeded the existing \
                 canonical confirmation/read timeout"
            )
        })??;
        let buyer_actor = buyer_actor.map(Address::with_workchain);
        reject_prior_settlement_action(
            &token_contract.with_workchain(),
            action,
            buyer_actor.as_deref(),
            &receipts,
        )
    }

    async fn submit_settlement_action_once(
        &self,
        token_contract: &Address,
        action: SettlementAction,
        expected: ExpectedSettlementEvent,
        buyer_actor: Option<&Address>,
        prepared: (String, String, String, String),
    ) -> Result<SettlementActionReceipt> {
        let mut unconditional = || true;
        self.submit_settlement_action_once_if(
            token_contract,
            action,
            expected,
            buyer_actor,
            prepared,
            &mut unconditional,
        )
        .await?
        .ok_or_else(|| anyhow!("unconditional settlement action was unexpectedly cancelled"))
    }

    async fn submit_settlement_action_once_if(
        &self,
        token_contract: &Address,
        action: SettlementAction,
        expected: ExpectedSettlementEvent,
        buyer_actor: Option<&Address>,
        prepared: (String, String, String, String),
        before_post: &mut (dyn FnMut() -> bool + Send),
    ) -> Result<Option<SettlementActionReceipt>> {
        let display_tc = display_token_contract(token_contract);
        let timing = SellerLivenessParams::canonical();
        let confirmation_timeout = timing.cancel_confirmation_timeout;
        let confirmation_poll = timing.cancel_confirmation_poll;
        let before = tokio::time::timeout(
            confirmation_timeout,
            self.token_contract_settlement_receipts(token_contract),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "TokenContract {display_tc} pre-submit event snapshot exceeded the existing \
                 canonical confirmation/read timeout"
            )
        })??;
        let buyer_actor_string = buyer_actor.map(Address::with_workchain);
        reject_prior_settlement_action(
            &token_contract.with_workchain(),
            action,
            buyer_actor_string.as_deref(),
            &before,
        )?;
        let pre = tokio::time::timeout(
            confirmation_timeout,
            self.token_contract_deal_snapshot(token_contract),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "TokenContract {display_tc} pre-submit coherent snapshot exceeded the existing \
                 canonical confirmation/read timeout"
            )
        })??;
        if action == SettlementAction::BuyerStop {
            validate_buyer_stop_pre_state(&token_contract.with_workchain(), pre.as_ref(), &before)?;
        }
        let pre = pre.ok_or_else(|| {
            anyhow!("TokenContract {display_tc} was inactive before the settlement action POST")
        })?;
        validate_settlement_facts(&token_contract.with_workchain(), &pre)?;
        let expected = expected.resolve(pre.state);
        let expected_buyer = if matches!(
            action,
            SettlementAction::BuyerStop | SettlementAction::SellerStop | SettlementAction::Dispute
        ) {
            let recorded = tokio::time::timeout(
                confirmation_timeout,
                self.token_contract_buyer_note(token_contract),
            )
            .await
            .map_err(|_| {
                anyhow!(
                    "TokenContract {display_tc} buyer-actor preflight exceeded the existing \
                     canonical confirmation/read timeout"
                )
            })??
            .ok_or_else(|| {
                anyhow!(
                    "TokenContract {display_tc} has no authoritative buyer actor in getParties; \
                     refusing settlement action before any money POST"
                )
            })?;
            if let Some(actor) = buyer_actor {
                let recorded = normalize_addr(&recorded.with_workchain())?;
                let actor = normalize_addr(&actor.with_workchain())?;
                if recorded != actor {
                    return Err(anyhow!(
                        "TokenContract {display_tc} recorded buyer actor {} does not match requested \
                         buyer note {}; refusing settlement action before any money POST",
                        display_dexdo_address(&recorded),
                        display_dexdo_address(&actor)
                    ));
                }
            }
            Some(recorded.with_workchain())
        } else {
            None
        };

        let (endpoint, boc, account_id, dapp_id) = prepared;
        if !before_post() {
            return Ok(None);
        }
        reconcile_settlement_action_after_post(
            &token_contract.with_workchain(),
            action,
            expected,
            expected_buyer.as_deref(),
            &before,
            &pre,
            confirmation_timeout,
            confirmation_poll,
            std::time::Duration::from_secs(SDK_MESSAGE_EXPIRY_SECS),
            || async {
                let submitted = if action == SettlementAction::BuyerStop {
                    send_explicit_stop_money_once(
                        &self.money_post_http,
                        &endpoint,
                        &boc,
                        &account_id,
                        &dapp_id,
                    )
                    .await
                } else {
                    send_message_routed_money_once(
                        &self.money_post_http,
                        &endpoint,
                        &boc,
                        &account_id,
                        &dapp_id,
                    )
                    .await
                };
                submitted.map(|_| ())
            },
            || self.token_contract_settlement_receipts(token_contract),
            || self.token_contract_deal_snapshot(token_contract),
        )
        .await
        .map(Some)
    }

    /// Read-only buyer preflight for the final-withdrawal latch. A withdrawn PrivateNote cannot call
    /// `placeInferenceBuy`; detect that state before any money write and return the actionable chain
    /// refusal instead of the raw exit code. Read errors are retried because a transient getter failure
    /// is not evidence that the note withdrew. Older contract generations without `hasWithdrawn` remain
    /// usable: the guard records that it could not inspect the latch and fails open.
    pub async fn assert_note_can_place_inference_buy(&self, note: &Address) -> Result<()> {
        let mut delay = crate::params::BUYER_NOTE_PREFLIGHT_INITIAL_BACKOFF;
        let mut details = None;
        for attempt in 1..=crate::params::BUYER_NOTE_PREFLIGHT_MAX_ATTEMPTS {
            match self.private_note_details(note).await {
                Ok(value) => {
                    details = Some(value);
                    break;
                }
                Err(error) if attempt < crate::params::BUYER_NOTE_PREFLIGHT_MAX_ATTEMPTS => {
                    eprintln!(
                        "buyer place preflight getDetails read failed (attempt {attempt}/{}): \
                         {error}; retrying after {delay:?}",
                        crate::params::BUYER_NOTE_PREFLIGHT_MAX_ATTEMPTS
                    );
                    tokio::time::sleep(delay).await;
                    delay *= crate::params::BUYER_NOTE_PREFLIGHT_BACKOFF_MULTIPLIER;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "buyer place preflight could not read PrivateNote.getDetails for note {} \
                             after {} attempts",
                            display_dexdo_address(note),
                            crate::params::BUYER_NOTE_PREFLIGHT_MAX_ATTEMPTS
                        )
                    });
                }
            }
        }
        let details =
            details.expect("buyer note details retry loop must return or record a result");
        buyer_note_withdrawn_guard(note, details.as_ref())
    }

    /// read-only seller preflight for the contract's final-withdrawal latch. `withdrawTokens` sets
    /// `_hasWithdrawn=true`; after that `PrivateNote.postSellOffer` is permanently blocked by
    /// `ERR_INVALID_STATE` 151. Keep that semantics and fail before any seller write.
    pub async fn assert_note_can_post_sell_offer(&self, note: &Address) -> Result<()> {
        let details = self.private_note_details(note).await?.ok_or_else(|| {
            anyhow!(
                "seller post_offer aborted: note {} returned no PrivateNote.getDetails; cannot read \
                 hasWithdrawn before postSellOffer. Re-mint/deploy a fresh note against the current contracts.",
                display_dexdo_address(note)
            )
        })?;
        let withdrawn = details_has_withdrawn(&details).ok_or_else(|| {
            anyhow!(
                "seller post_offer aborted: PrivateNote.getDetails for note {} has no hasWithdrawn field; \
                 refusing to submit postSellOffer without proving the note is not withdrawn",
                display_dexdo_address(note)
            )
        })?;
        if withdrawn {
            return Err(anyhow!(note_withdrawn_sell_offer_message(note)));
        }
        Ok(())
    }

    /// Directive -- the note pre-funds its per-deal TC **uninit deploy address** from its ECC[2],
    /// via the `PrivateNote` owner-method `fundDeployShell(nonce, tcShell)`. The note derives the target
    /// internally from `(ephemeralPubkey, nonce)`, so no caller-supplied address -- this replaces the
    /// operator multisig's [`fund_deploy_from_wallet_ecc`](Self::fund_deploy_from_wallet_ecc) on the
    /// operate path. The TC *deploy* stays external seller-signed; this call only pre-funds. The call is
    /// an external owner-signed message to the note, exactly like [`deploy_inference_orderbook`](Self::deploy_inference_orderbook).

    /// **ONE LEG SINCE 4.0.34.** `rootModelShell` is gone from the contract signature
    /// (`contracts/dex/PrivateNote.sol:1143`) because `SuperRoot` deploys the `RootModel` itself and
    /// carries its own value. The Rust parameter is kept so a caller that still passes one is REFUSED
    /// rather than served a message that quietly funds only the deal -- see
    /// [`root_model_deploy_shell_unsupported`].
    pub async fn note_fund_deploy_shell(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        nonce: u64,
        root_model_shell: u128,
        tc_shell: u128,
    ) -> Result<Value> {
        if let Some(reason) = root_model_deploy_shell_unsupported(root_model_shell) {
            return Err(anyhow!(reason));
        }
        let boc = Self::encode_signed_call_boc(
            note,
            PRIVATENOTE_ABI,
            "fundDeployShell",
            note_fund_deploy_shell_params(nonce, tc_shell),
            owner_keys,
        )
        .await?;
        let message_hash = external_message_hash(&boc)?;
        let endpoint = self.client.endpoint();
        let account_id = dest_account_id_hex(&boc)?;
        let dapp_id = fetch_dapp_id(&self.http, endpoint, &account_id).await?;
        match self.send_with_retry(&boc).await {
            Ok(value) => Ok(value),
            Err(error) => {
                let aborted = error
                    .downcast_ref::<OnchainSubmitError>()
                    .and_then(|submit| submit.sanitized_payload().pointer("/result/aborted"))
                    .and_then(Value::as_bool)
                    == Some(true);
                if !aborted {
                    return Err(error);
                }
                let receipt = match poll_finalized_destination_receipt(
                    &self.http,
                    endpoint,
                    &account_id,
                    &dapp_id,
                    &message_hash,
                )
                .await
                {
                    Ok(receipt) => receipt,
                    Err(receipt_error) => {
                        return Err(error.context(format!(
                            "fundDeployShell aborted; failed to resolve finalized destination receipt \
                             for message_hash={message_hash}: {receipt_error}; ECC[2] cause not proven"
                        )));
                    }
                };
                Err(fund_deploy_shell_receipt_error(
                    error,
                    &message_hash,
                    receipt.as_ref(),
                ))
            }
        }
    }

    /// Deploy the per-deal `TokenContract` FROM THE SELLER'S NOTE (contracts 4.0.36).

    /// Replaces the pair this client used before -- `note_fund_deploy_shell` placing ECC[2] at an
    /// uninit address, then a seller-signed external deploy carrying the code. The 4.0.36
    /// constructor authenticates its sender against the canonical note for `depositIdentifierHash`
    /// and runs that check BEFORE `accept`, so the external form is refused and paid for by whoever
    /// sent it. One owner call does both halves now, and the note supplies its own deposit hash.

    /// The deal's ADDRESS is unchanged in construction -- same three statics, same `pubkey` -- so it
    /// still comes from [`token_contract_deploy_address`](Self::token_contract_deploy_address) and
    /// every other party derives it exactly as before. What moved is the DAPP: the deal lands in the
    /// note's, which is configured, which is why it can hold its own native floor at all.

    pub async fn note_deploy_deal(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        request: NoteDeployDealRequest<'_>,
    ) -> Result<Value> {
        let boc = Self::encode_signed_call_boc(
            note,
            PRIVATENOTE_ABI,
            "deployDeal",
            note_deploy_deal_params(
                request.nonce,
                request.model_name,
                &model_hash_for(request.model_name),
                request.price_per_tick,
                request.max_ticks,
                request.gas_reserve,
            ),
            owner_keys,
        )
        .await?;
        self.send_with_retry(&boc).await
    }

    /// A deal's ECC[2] RESERVE -- the plane its charges come out of.

    /// Distinct from [`active_native_balance`](Self::active_native_balance) in the way that matters:
    /// a deal mints its own native floor and cannot mint this. Confusing the two is what made the
    /// pre-`fundDeal` reserve gate unable to fire.
    pub(super) async fn deal_reserve_ecc(&self, addr: &Address) -> Result<u128> {
        let account = self
            .client
            .get_account_retrying(addr)
            .await?
            .ok_or_else(|| anyhow!("contract {addr} is missing; cannot read its ECC[2] reserve"))?;
        Ok(account.ecc_balance(crate::params::SHELL_CURRENCY_ID))
    }

    pub(super) async fn active_native_balance(&self, addr: &Address) -> Result<u128> {
        let account = self
            .client
            .get_account_retrying(addr)
            .await?
            .ok_or_else(|| anyhow!("contract {addr} is missing; cannot gas-health check"))?;
        if !account.is_active() {
            return Err(anyhow!(
                "contract {addr} is {}, not Active; cannot gas-health check",
                account.status
            ));
        }
        Ok(account.balance)
    }

    async fn account_snapshot(&self, addr: &Address) -> String {
        match self.client.get_account_retrying(addr).await {
            Ok(Some(a)) => format!(
                "status={} native={} ecc2={} code_hash={}",
                a.status,
                a.balance,
                a.ecc_balance(2),
                a.code_hash.as_deref().unwrap_or("<none>")
            ),
            Ok(None) => "not found".to_string(),
            Err(e) => format!("query error: {e}"),
        }
    }

    async fn log_deploy_prefund_snapshot(
        &self,
        stage: &str,
        note: &Address,
        rm: &Address,
        tc: &Address,
    ) {
        eprintln!(
            "deploy-prefund {stage}: note {} [{}]; RootModel {} [{}]; TokenContract {} [{}]",
            display_dexdo_address(note),
            self.account_snapshot(note).await,
            display_dexdo_address(rm),
            self.account_snapshot(rm).await,
            display_token_contract(tc),
            self.account_snapshot(tc).await,
        );
    }

    /// before an active per-deal TC write, ensure the deal still has native vmshell gas.
    /// `fundDeployShell` is seller-note-owned and derives the target from `(seller pubkey, nonce)`, so
    /// only call this from paths that hold the seller note/key/nonce.

    /// **THE DEAL ONLY.** This used to top the `RootModel` up as well, on the same message. Both halves
    /// of that are gone in 4.0.34 and for the same reason: `fundDeployShell` no longer has a RootModel
    /// leg (`contracts/dex/PrivateNote.sol:1143`), and the RootModel no longer needs one -- deployed by
    /// SuperRoot it lives in SuperRoot's configured dapp, where its own `ensureBalance() ->
    /// gosh.mintshellq` mints what it needs. A deal cannot do that: it is deployed by an external
    /// message into a dapp of its own with no configuration, which is why this step survives for it.
    /// ** -- SIZED TO THE DEAL, NOT TO A CONSTANT.** The floor and target used to be the flat
    /// `ACTIVE_CONTRACT_GAS_HEALTH_*` pair, 5 and 10 vmshell. That made the deposit decision
    /// meaningless: whatever the seller funded, a deal below 5 was refilled to 10 out of the same
    /// note, so a cheap deal spent ten SHELL either way and the surplus burnt at `destroy`. The
    /// figures here are the deal's own, read from the deal -- which states its `maxTicks`.
    pub async fn ensure_deal_contract_gas(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        nonce: u64,
        token_contract: Option<&Address>,
    ) -> Result<()> {
        let deal_gas_overhead_raw =
            crate::params::resolve_deal_gas_overhead_raw(self.network(), None)
                .map_err(anyhow::Error::msg)?;
        self.ensure_deal_contract_gas_with_overhead(
            note,
            owner_keys,
            nonce,
            token_contract,
            deal_gas_overhead_raw,
        )
        .await
    }

    /// Top the deal's ECC[2] RESERVE up to what its whole life is charged, out of the seller's note.

    /// **This was a no-op for one build, and the contracts answered the question that made it one.**
    /// It used to read the deal's NATIVE balance and top that up, which was right while a deal could
    /// not mint: deployed by an external message into a dapp with no config, every vmshell it would
    /// spend had to be put there. Since 4.0.36 the note deploys it into the note's dapp and
    /// `ensureBalance` mints its own native floor, so the native plane needs nothing -- and for one
    /// build there was no other plane to fund either, because every leg pointed at the deal
    /// converted its ECC to native on arrival.

    /// `PrivateNote.fundDeployShell` sends ECC[2] under `flag: 1` now, so the money arrives as
    /// ECC[2] and lands in the reserve the burns come out of. That is the leg this function was
    /// waiting for.

    /// **Why top up at all when `deployDeal` already seeded the reserve.** The seed is one figure
    /// chosen before the deal exists; the charges are real and arrive over its life. A deal whose
    /// reserve runs dry does not stall quietly -- `gosh.burnecc` fails the ACTION phase
    /// (`RESULT_CODE_NOT_ENOUGH_EXTRA`) and the whole transaction reverts, so the entry simply does
    /// not happen. The entries that cannot be rescued any other way are the seller's own external
    /// ones: `onlyOwnerPubkey(_sellerPubkey) accept` takes an external message and an external
    /// message carries no currency, so `claimTokens`, `sellerStop`, `withdrawShell` and `destroy`
    /// have no way to pay for themselves, ever. Topping up here is what keeps them reachable.

    /// Reads before it writes and sends nothing when the reserve already covers the requirement: a
    /// top-up is a spend from the seller's note, and spending to restore a balance that is already
    /// there is the kind of thing an operator only finds out about from a receipt.

    /// `deal_gas_overhead_raw` is the operator's EXTRA on top of the contract's own figure, honoured
    /// exactly as supplied -- see [`crate::params::resolve_deal_gas_overhead_raw`].
    pub async fn ensure_deal_contract_gas_with_overhead(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        nonce: u64,
        token_contract: Option<&Address>,
        deal_gas_overhead_raw: u128,
    ) -> Result<()> {
        let Some(deal) = token_contract else {
            // No deal address yet means nothing to top up: `deployDeal` seeds the reserve, and it
            // has not run. Not an error -- the seller's funding order reaches here before and after.
            return Ok(());
        };
        let Some(account) = self.client.get_account_retrying(deal).await? else {
            return Ok(());
        };
        if !account.is_active() {
            // An inactive deal is either not deployed yet or already destroyed. Both are somebody
            // else's decision, and sending ECC[2] to either is money that lands where nothing spends it.
            return Ok(());
        }
        // `maxTicks` comes from the DEAL, not from a caller: the deal states its own terms, and a
        // figure passed in could disagree with the contract that will do the charging. A deal that
        // does not answer its terms is not one this can size, so it is left alone rather than
        // funded against a guess.
        let Some((_, _, max_ticks)) = self.token_contract_deal_terms(deal).await? else {
            return Ok(());
        };
        let have = account.ecc_balance(crate::params::SHELL_CURRENCY_ID);
        // HYSTERESIS, and it is not a nicety. This runs before EVERY write into the deal -- including
        // `claimTokens`, which is once per tick -- so a rule of "top up to the whole life's charge
        // whenever anything is missing" tops up after every burn. On a 1024-tick deal that is a
        // thousand external messages on the critical path of delivering ticks, and the seller ends
        // up paying about twice what the deal burns, because the reserve is held full right up to
        // `destroy`, where the remainder burns. Below the floor, refill to the target; above it,
        // send nothing.
        let Some(short) = crate::chain::contracts_provision::gas_health_top_up_amount(
            have,
            crate::params::deal_gas_health_floor_raw_with_overhead(
                max_ticks,
                deal_gas_overhead_raw,
            ),
            crate::params::deal_gas_health_target_raw_with_overhead(
                max_ticks,
                deal_gas_overhead_raw,
            ),
        ) else {
            return Ok(());
        };
        self.note_fund_deploy_shell(note, owner_keys, nonce, 0, short)
            .await
            .map_err(|error| {
                anyhow!(
                    "deal {deal} holds {have} raw ECC[2], below the floor its life is charged; \
                     topping {short} up from note {note} failed: {error}"
                )
            })?;
        // Wait for the credit before returning. Without this the next entry -- and there is one
        // immediately, this runs before every write -- reads the balance from before the transfer
        // landed and tops up again for the same shortfall.
        self.wait_ecc_balance_at_least(deal, have.saturating_add(short))
            .await
    }

    /// Poll until an account's ECC[2] reaches `min`, or say it did not.

    /// The native twin of this exists for the plane a deal no longer needs funded; this is the one
    /// that matters now, because ECC[2] is what the burns come out of and what a top-up moves.
    async fn wait_ecc_balance_at_least(&self, addr: &Address, min: u128) -> Result<()> {
        for _ in 0..crate::params::GAS_BALANCE_CONFIRM_MAX_READS {
            if let Ok(Some(account)) = self.client.get_account_retrying(addr).await {
                if account.ecc_balance(crate::params::SHELL_CURRENCY_ID) >= min {
                    return Ok(());
                }
            }
            tokio::time::sleep(crate::params::GAS_BALANCE_CONFIRM_POLL_INTERVAL).await;
        }
        Err(anyhow!(
            "deal {addr} ECC[2] reserve did not reach {min} after the top-up was submitted; \
             a second entry would top up again for the same shortfall"
        ))
    }

    /// Directive -- the note funds its own nonce-derived per-deal `TokenContract` with the exact
    /// `2P` seller bond, with no operator multisig in the path (replaces the operator
    /// multisig's [`fund_seller_bond`](Self::fund_seller_bond)). External owner-signed message.

    /// Contracts 4.0.33 replaced `postSellerBond(nonce, amount)` with the single funding door
    /// `fundDeal(nonce, gasShell, amount)` (`contracts/dex/PrivateNote.sol`), and 4.0.35 added a
    /// fourth argument, `endpointCipher optional(bytes)`, which this client sends as `null` -- see
    /// [`note_fund_deal_params`]. The door carries the two things a deal needs from two different
    /// pockets in one message:

    /// * `gasShell` rides in `currencies` as physical ECC[2] and ARRIVES as ECC[2] (`bab4bab3`
    /// dropped the conversion flag), so it lands in the reserve every entry burns from. It used to
    /// be sent under flag 17, which converted it to native -- the one plane a deal mints for itself
    /// -- so the leg moved money into the pocket that did not need it;
    /// * `amount` is a **figure**, subtracted from this note's private `_balance[CURRENCIES_ID_SHELL]`
    /// and passed as a call argument, which `TokenContract.fundDeal(amount)` adds to the deal's own
    /// record after re-deriving the caller as the canonical seller note. The bond is that figure.

    /// The gas leg is left where it already is: [`ensure_deal_contract_gas`](Self::ensure_deal_contract_gas)
    /// tops the `TokenContract` up through `fundDeployShell` before this call, so the production
    /// seller path passes `gas_shell = 0` and this message moves money only.
    pub async fn note_fund_deal(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        nonce: u64,
        gas_shell: u128,
        amount: u128,
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            NOTE_FUND_DEAL_METHOD,
            note_fund_deal_params(nonce, gas_shell, amount),
            owner_keys,
        )
        .await
    }

    /// The seller opens a stream session: `open(endpointCipher)` (external signature `_sellerPubkey`).
    /// Freezes a probe tick from the deposit
    /// and writes the endpoint cipher -- handover (`RealNote::encrypt_to` to the buyer's x25519 pubkey).
    pub async fn open_stream(
        &self,
        tc: &Address,
        seller_keys: &KeyPair,
        endpoint_cipher: &[u8],
    ) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "open",
            json!({ "endpointCipher": encode_hex(endpoint_cipher) }),
            seller_keys,
        )
        .await
    }

    /// Seller-only: `acceptProbe()` on the TC. Requires `block.timestamp >= probeTime + PROBE_WINDOW` and an
    /// unaccepted probe. Credits the trial tick to the seller, takes its fee by-fact, and only then does the
    /// deal become claimable at all.
    pub async fn accept_probe(&self, tc: &Address, seller_keys: &KeyPair) -> Result<Value> {
        self.submit(tc, TOKENCONTRACT_ABI, "acceptProbe", json!({}), seller_keys)
            .await
    }

    /// The seller claims CUMULATIVE consumption: `claimTokens(cumulativeTokens)` (external signature
    /// `_sellerPubkey`).

    /// The value is an absolute running total in tokens, never a delta. The contract REJECTS rather than
    /// trims a claim that breaks any of its bounds, so the caller must pre-clamp:
    /// - not below the previous claim (cumulative, never decreasing);
    /// - not above the claim cap (`getSubscription`: whole volume for an ordinary deal, one weekly quota
    /// per started week for a subscription);
    /// - at least `minClaimInterval` since the previous claim;
    /// - within the rate bound `delta * minSecondsPerTick <= elapsed * TICK_SIZE`;
    /// - and no larger than the hard per-call `MAX_CLAIM_DELTA == TICK_SIZE`, regardless of elapsed time.

    /// Landing a claim promotes the PREVIOUS one to trusted -- nobody contested it, since an open dispute
    /// blocks this path entirely -- so the newest claim always remains contestable.
    pub async fn claim_tokens(
        &self,
        tc: &Address,
        seller_keys: &KeyPair,
        cumulative_tokens: u128,
    ) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "claimTokens",
            json!({ "cumulativeTokens": cumulative_tokens.to_string() }),
            seller_keys,
        )
        .await
    }

    /// Permissionless: `finalize()` promotes the pending claims once `CLAIM_PROMOTE_WINDOW` has passed with
    /// no dispute, and settles/closes an ordinary deal whose funded volume is exhausted.

    /// This is what makes the LAST claim of a deal payable at all -- nothing supersedes it, so without the
    /// window it would stay contestable forever. Unsigned-equivalent (a throwaway key): the contract takes
    /// no caller-chosen parameters and pays the caller nothing.
    pub async fn finalize_claims(&self, tc: &Address) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "finalize",
            json!({}),
            &KeyPair::generate(),
        )
        .await
    }

    /// Permissionless: `settleWeek()` credits the seller ONE crossed subscription week at the full weekly
    /// quota, take-or-pay -- independently of how much the buyer actually drew, because a subscription buys
    /// reserved availability rather than delivered volume. Idempotent per boundary; the final week closes
    /// the deal.
    pub async fn settle_week(&self, tc: &Address) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "settleWeek",
            json!({}),
            &KeyPair::generate(),
        )
        .await
    }

    /// The seller abandons the deal: `sellerStop()` (external signature `_sellerPubkey`). Settles by FACT on
    /// every deal shape -- a seller who walks out mid-week has stopped reserving capacity, so take-or-pay
    /// does not apply to him and the buyer keeps the remaining escrow. He forfeits the pending tail exactly
    /// as the buyer would, so quitting never pays better than delivering.
    pub async fn seller_stop(
        &self,
        tc: &Address,
        seller_keys: &KeyPair,
    ) -> Result<SettlementActionReceipt> {
        self.reject_prior_settlement_action_before_prepare(tc, SettlementAction::SellerStop, None)
            .await?;
        let prepared = self
            .prepare_money_post(tc, TOKENCONTRACT_ABI, "sellerStop", json!({}), seller_keys)
            .await?;
        self.submit_settlement_action_once(
            tc,
            SettlementAction::SellerStop,
            ExpectedSettlementEvent::StreamStopped,
            None,
            prepared,
        )
        .await
    }

    /// the seller CLOSES a STOPped deal's `TokenContract`. `destroy()` is
    /// `onlyOwnerPubkey(_sellerPubkey)`, gated `!_opened && !_disputed && !_offerPosted` (the buyer's
    /// `stop()` clears `_opened` on close), and calls `selfdestruct` to the deal's own stored
    /// `_sellerNote` (`contracts/airegistry/TokenContract.sol:1844`).
    /// External call, signed by the seller owner key (matches `_sellerPubkey`).
    /// **DESTRUCTIVE / BURNS (by-fact, 4.0.7):** the held ~`MIN_BALANCE` reserve does NOT recover when the
    /// stored `_sellerNote` is the cross-dapp note -- the note balance does not increase (reproduced x2). The
    /// deploy *funding* crossed dapps via `fundDeployShell` flag:16 (credited); the raw `selfdestruct` *return*
    /// crossing the boundary is not credited -> the reserve is **burned at destroy**. So this closes the TC;
    /// reclaiming the reserve to the note would need a `TokenContract` flag:16/dapp-credit return fix
    /// (contract-side). NOT the dex/PMP oracle lifecycle.

    /// **4.0.33 signature.** Task O removed the caller-named payee: the contract pays its own
    /// stored `_sellerNote` (`TokenContract.sol` `_die`/`_payOwedAndDie`), and `destroy()` takes no
    /// inputs. A function id is derived from the whole signature, so sending the old
    /// `destroy(payoutAddress)` addresses a method this generation does not have -- the message
    /// never reaches a guard at all. `_payout` is kept only so the CLI surface is unchanged; it is
    /// deliberately not encoded, and it does not decide the payee.
    pub async fn destroy_token_contract(
        &self,
        tc: &Address,
        _payout: &Address,
        seller_keys: &KeyPair,
    ) -> Result<Value> {
        let seller_pubkey = self.token_contract_seller_pubkey(tc).await?;
        check_seller_pubkey(
            "destroy",
            seller_pubkey.as_deref(),
            seller_keys.public_hex(),
        )
        .map_err(anyhow::Error::msg)?;
        self.submit(tc, TOKENCONTRACT_ABI, "destroy", json!({}), seller_keys)
            .await
    }

    /// the seller winds down an **UNSOLD** deal -- one that never matched, so it was never
    /// funded and never opened. `destroy()` cannot reach that shape: it is gated on a STOPped deal,
    /// and a deal nobody bought was never stopped because it never started. `close()` is its door
    /// (`contracts/airegistry/TokenContract.sol:803`), `onlyOwnerPubkey(_sellerPubkey)` exactly like
    /// `destroy()`, and it takes no inputs after Task O.

    /// **TWO BRANCHES, AND ONLY ONE OF THEM ENDS THE DEAL.** With no ask resting, the contract hands
    /// the bond back to its own stored `_sellerNote` and self-destructs in this same transaction
    /// (`TokenContract.sol:816-820`). With an ask still resting it records INTENT only --
    /// `_closing = true`, then returns (`TokenContract.sol:805-810`) -- leaving the deal alive and
    /// still matchable, and the destruct deferred to whenever the book announces the ask left. So
    /// the caller must establish `getOffer().offerPosted == false` before sending this, or it will
    /// report a close that did not happen; `dexdo close` refuses instead.

    /// **It could not be sent at all before 4.0.34.** `_buyer` was declared and never assigned, and
    /// `addr_none.value` compiles to `PARSEMSGADDR` + `INDEX 3`, an out-of-range read (`exit_code 5`)
    /// on the one-element representation -- thrown inside `_die` at `TokenContract.sol:434`, on every
    /// path through it. The constructor now writes `_buyer = address(0)`
    /// (`contracts/airegistry/TokenContract.sol:318`), whose `.value` is a readable zero; that one
    /// assignment is what lets this call reach `selfdestruct`.
    pub async fn close_unsold_deal(&self, tc: &Address, seller_keys: &KeyPair) -> Result<Value> {
        let seller_pubkey = self.token_contract_seller_pubkey(tc).await?;
        check_seller_pubkey("close", seller_pubkey.as_deref(), seller_keys.public_hex())
            .map_err(anyhow::Error::msg)?;
        self.submit(tc, TOKENCONTRACT_ABI, "close", json!({}), seller_keys)
            .await
    }

    /// The seller **concedes the dispute** through `releaseDispute()`
    /// (`onlyOwnerPubkey(_sellerPubkey)`). The exact terminal movement is reported only by the
    /// resulting `DisputeResolved(released=true)` event and strict getters. In the current
    /// subscription contract the buyer's stake and the unearned disputed-week remainder burn;
    /// no client-side amount is reconstructed.
    pub async fn release_dispute(
        &self,
        tc: &Address,
        seller_keys: &KeyPair,
    ) -> Result<SettlementActionReceipt> {
        self.reject_prior_settlement_action_before_prepare(
            tc,
            SettlementAction::ReleaseDispute,
            None,
        )
        .await?;
        let prepared = self
            .prepare_money_post(
                tc,
                TOKENCONTRACT_ABI,
                "releaseDispute",
                json!({}),
                seller_keys,
            )
            .await?;
        self.submit_settlement_action_once(
            tc,
            SettlementAction::ReleaseDispute,
            ExpectedSettlementEvent::DisputeResolved { released: true },
            None,
            prepared,
        )
        .await
    }

    /// Permissionless expiry resolution for an already-disputed deal. The caller does not choose
    /// payouts; `TokenContract.resolveDisputeTimeout()` applies the deployed settlement rules.
    pub async fn resolve_dispute_timeout(&self, tc: &Address) -> Result<SettlementActionReceipt> {
        self.reject_prior_settlement_action_before_prepare(
            tc,
            SettlementAction::ResolveDisputeTimeout,
            None,
        )
        .await?;
        let prepared = self
            .prepare_money_post(
                tc,
                TOKENCONTRACT_ABI,
                "resolveDisputeTimeout",
                json!({}),
                &KeyPair::generate(),
            )
            .await?;
        self.submit_settlement_action_once(
            tc,
            SettlementAction::ResolveDisputeTimeout,
            ExpectedSettlementEvent::DisputeResolved { released: false },
            None,
            prepared,
        )
        .await
    }

    /// Withdraw finalized seller SHELL from a closed or still-open deal balance. This moves only the
    /// already-finalized `_finalizedOwed`; it is separate from `destroy`, which closes/selfdestructs the TC.

    /// **4.0.33 signature.** Like `destroy`, this lost its caller-named payee to Task O:
    /// `withdrawShell(uint128 amount)` pays the contract's stored `_sellerNote`.
    pub async fn withdraw_shell(
        &self,
        tc: &Address,
        amount: u128,
        seller_keys: &KeyPair,
    ) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "withdrawShell",
            json!({ "amount": amount.to_string() }),
            seller_keys,
        )
        .await
    }

    /// Submit owner-signed `PrivateNote.withdrawTokens(destWalletAddr, dapp_id)` for a note's available token
    /// balances. `destination_dapp_id` is event metadata only (surfaced in `TokensWithdrawn`, drives no logic)
    /// and names the destination wallet's DApp, not the dexdo deployment. Returns the submit result. Do not
    /// treat this helper as proof that every native/ECC balance is fully retired
    /// without by-fact evidence on the current deployed contract.
    pub async fn withdraw_note_tokens(
        &self,
        note: &Address,
        keys: &KeyPair,
        dest_wallet: &Address,
        destination_dapp_id: &str,
    ) -> Result<Value> {
        // One-shot guard: `withdrawTokens` sets `_hasWithdrawn=true` and reverts `ERR_INVALID_STATE` on a
        // re-call. Read `getDetails().hasWithdrawn` and fail
        // LOUD with a clear reason instead of the opaque `TVM_ERROR (compute phase)` the revert would produce.
        if let Some(d) = self
            .client
            .run_getter_retrying(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await?
        {
            let already = details_has_withdrawn(&d).unwrap_or(false);
            if already {
                return Err(anyhow!(
                    "note {} was already withdrawn -- `withdrawTokens` is one-shot per note. Re-check the \
                     note/wallet on-chain before assuming any remaining balance is withdrawable.",
                    display_dexdo_address(note)
                ));
            }
        }
        let submitted = self
            .submit(
                note,
                PRIVATENOTE_ABI,
                "withdrawTokens",
                self.withdraw_note_tokens_payload_for_destination(dest_wallet, destination_dapp_id),
                keys,
            )
            .await;
        match submitted {
            Ok(value) => Ok(value),
            Err(error) => {
                let line = self.refusal_gate_line(note, &error.to_string()).await;
                Err(match line {
                    Some(line) => anyhow!("{error}\n{line}"),
                    None => error,
                })
            }
        }
    }

    /// The first unclosed `withdrawTokens` gate for a note, decoded from its own account storage.

    /// nine of the eleven gates have no getter, and `_stakes` has none at all -- which is why
    /// an operator can read `busyAddress: not busy` off `getDetails()` and still be refused 121. The
    /// storage decode answers all eleven without a contract change.
    pub async fn note_withdraw_gate(&self, note: &Address) -> Result<NoteWithdrawGate> {
        let account = self
            .client
            .get_account_retrying(note)
            .await?
            .ok_or_else(|| anyhow!("PrivateNote {note} account is not found"))?;
        let boc = account
            .boc
            .as_deref()
            .ok_or_else(|| anyhow!("PrivateNote {note} account BOC is unavailable"))?;
        note_withdraw_gate_from_account_boc(boc)
    }

    /// The gate line for a refusal one of the eleven gates could explain -- or nothing at all.

    /// `None` when no gate raises this refusal's code. A withdrawal that failed on gas is not a
    /// note-state problem, and "all eleven closed" printed beside it would be a true sentence
    /// answering a question the operator did not ask, which reads as an explanation and is not one.

    /// When the diagnostic read itself fails, that is reported as a failed diagnostic and the
    /// original refusal stands unchanged. The refusal is the fact the operator came with; one
    /// failure is never turned into two, and a refusal is never swapped for the error raised while
    /// explaining it.
    async fn refusal_gate_line(&self, note: &Address, error_text: &str) -> Option<String> {
        if !refusal_carries_a_withdraw_gate_code(error_text) {
            return None;
        }
        Some(match self.note_withdraw_gate(note).await {
            Ok(reading) => withdraw_gate_line(&reading),
            Err(read_error) => format!(
                "the note state could not be read ({read_error:#}), so the refusal above stands as \
                 the only fact -- this is NOT evidence about which condition stopped it"
            ),
        })
    }

    fn withdraw_note_tokens_payload_for_destination(
        &self,
        dest_wallet: &Address,
        destination_dapp_id: &str,
    ) -> Value {
        let dapp_id = crate::address::to_dapp_id_param(destination_dapp_id);
        withdraw_note_tokens_payload(dest_wallet, &dapp_id)
    }

    /// Submit owner-signed `PrivateNote.sweepShell(destWalletAddr, dapp_id)`: move the note's
    /// PHYSICAL `ECC[2]` pocket to a wallet after the note has already withdrawn.

    /// # Which money this is, because the note holds two kinds

    /// A note carries a trading RECORD (`_balance`, what `getDetails().balance` reports) and a
    /// physical ECC[2] POCKET on the account itself (`balance_other`). `withdrawTokens` releases
    /// the record and drains the pocket once, then latches `_hasWithdrawn` forever. `sweepShell`
    /// moves the POCKET and never touches the record -- so it collects only what arrived after the
    /// withdrawal, which is exactly the tail of commitments the note made before it.

    /// The contract sends under `flag: 1`, so the SHELL lands at the destination as ECC[2] -- the
    /// traded asset, in `balance_other`. It does NOT become spendable gas there, and a sweep
    /// therefore cannot rescue a wallet that is out of native `vmshell`. The note pays this call's
    /// execution out of its own native balance, so the destination needs nothing to receive it.

    /// # Why the two guards are here and not left to the chain

    /// Both conditions the contract enforces are readable before spending anything, and the chain's
    /// refusal for either is an opaque `TVM_ERROR (compute phase)` exit code. Reading first turns
    /// two dead ends into two sentences that say what to do instead.

    /// The `_hasWithdrawn` guard refuses only on a DEFINITE `false`. When `getDetails()` cannot be
    /// read the call proceeds and the chain decides, matching [`withdraw_note_tokens`]: failing
    /// closed on an unreadable getter would block a legitimate sweep during a read hiccup, and this
    /// is a recovery path whose whole point is being available when things have gone wrong.
    pub async fn sweep_note_shell(
        &self,
        note: &Address,
        keys: &KeyPair,
        dest_wallet: &Address,
        destination_dapp_id: &str,
    ) -> Result<Value> {
        if let Some(details) = self
            .client
            .run_getter_retrying(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await?
        {
            if details_has_withdrawn(&details) == Some(false) {
                return Err(anyhow!(
                    "note {} has not withdrawn yet, so there is nothing for `sweepShell` to \
                     collect: it moves only the physical ECC[2] that arrives AFTER a withdrawal. \
                     Run `dexdo note withdraw` instead -- that releases the trading record AND \
                     takes the physical pocket in the same message.",
                    display_dexdo_address(note)
                ));
            }
        }

        let before = self.note_ecc_shell_pocket(note).await?;
        if before == 0 {
            return Err(anyhow!(
                "note {} holds no physical ECC[2], so there is nothing to sweep. This reads the \
                 account pocket (`balance_other`), not the trading record `getDetails().balance` \
                 -- a note can show a balance there and still have an empty pocket, and \
                 `sweepShell` moves only the pocket.",
                display_dexdo_address(note)
            ));
        }

        self.submit_money_call_once(
            note,
            PRIVATENOTE_ABI,
            "sweepShell",
            self.withdraw_note_tokens_payload_for_destination(dest_wallet, destination_dapp_id),
            keys,
        )
        .await?;

        // A RESULT, not a receipt for a message. The submit says the call was accepted; only the
        // pocket going down says the money moved, and those are different facts -- a submitted
        // sweep that left the balance where it was is the failure this waits to rule out.
        let after = self.wait_ecc_shell_pocket_drained(note, before).await?;
        Ok(json!({
            "note": display_dexdo_address(note),
            "destination": display_dexdo_address(dest_wallet),
            "pocket_before": before.to_string(),
            "pocket_after": after.to_string(),
            "swept": before.saturating_sub(after).to_string(),
            "confirmed": after < before,
        }))
    }

    /// The note's PHYSICAL ECC[2] pocket -- the account's own currency, not the trading record.
    async fn note_ecc_shell_pocket(&self, note: &Address) -> Result<u128> {
        let account = self
            .client
            .get_account_retrying(note)
            .await?
            .ok_or_else(|| anyhow!("PrivateNote {note} account is not found"))?;
        Ok(account.ecc_balance(crate::params::SHELL_CURRENCY_ID))
    }

    /// Poll until the note's ECC[2] pocket falls below `before`, and report where it settled.

    /// The mirror of [`wait_ecc_balance_at_least`](Self::wait_ecc_balance_at_least) and reusing its
    /// two bounds rather than adding a third pair: the question is the same one asked in the other
    /// direction, and a second set of numbers for it would be another timer.

    /// A pocket that has not moved after the bound is NOT reported as success. It returns the last
    /// reading and the caller states plainly that nothing was confirmed, because "submitted" and
    /// "arrived" are the two facts this whole method exists to keep apart.
    async fn wait_ecc_shell_pocket_drained(&self, note: &Address, before: u128) -> Result<u128> {
        let mut last = before;
        for _ in 0..crate::params::GAS_BALANCE_CONFIRM_MAX_READS {
            tokio::time::sleep(crate::params::GAS_BALANCE_CONFIRM_POLL_INTERVAL).await;
            if let Ok(current) = self.note_ecc_shell_pocket(note).await {
                last = current;
                if current < before {
                    return Ok(current);
                }
            }
        }
        Ok(last)
    }

    /// Submit owner-signed `PrivateNote.initTransfer(destDepositHash, tokenType, amount, eccAmount)`:
    /// move part of THIS note's spendable trading record (`_balance`) into another note's.

    /// This is the only credit into `_balance` a user can originate. The record is written once by
    /// the constructor and otherwise credited only by a deal, by the book, or -- here -- by another
    /// note, so a note whose trading balance has run down has no other refill: `note topup` reaches
    /// the ECC[2] pocket and cannot touch this plane at all.

    /// The destination is named by its `depositIdentifierHash`, NOT by its address, because that is
    /// what the contract takes: it derives `dest` itself with
    /// `DexLib.computePrivateNoteAddress(_privateNoteCode, destDepositHash)`. Callers should read
    /// that hash off the destination note's own `getDetails()` rather than carry it separately --
    /// `_depositIdentifierHash` is a `static` StateInit field, so for any genuine note of the
    /// pinned generation the hash and the address determine each other, and reading it from the
    /// account whose balance was checked is what makes "the note I inspected" and "the note the
    /// contract will credit" the same note by construction.

    /// `ecc_amount` rides the physical ECC pocket along with the record and is a separate figure;
    /// pass 0 to move the record only.

    /// The refusals this can come back with are not generic failures and are re-stated by name --
    /// see [`note_transfer_submit_hint`].
    pub async fn init_note_transfer(
        &self,
        note: &Address,
        keys: &KeyPair,
        dest_deposit_hash: &str,
        token_type: u32,
        amount: u128,
        ecc_amount: u128,
    ) -> Result<Value> {
        let submitted = self
            .submit(
                note,
                PRIVATENOTE_ABI,
                "initTransfer",
                init_note_transfer_payload(dest_deposit_hash, token_type, amount, ecc_amount),
                keys,
            )
            .await;
        // `initTransfer` carries the SAME eleven gates in the same order as `withdrawTokens`,
        // so an operator refused 121 here is in the identical position -- told a state their client
        // has just denied. The hint already explains the code; this names which gate produced it.

        // Only where the hint already fires, so the boundary stays the two refusals that already
        // explain 121/167 rather than becoming a chain read on every failed transfer.
        match submitted {
            Ok(value) => Ok(value),
            Err(error) => {
                let text = error.to_string();
                let hint = note_transfer_submit_hint(&text);
                let gate = match hint {
                    Some(_) => self.refusal_gate_line(note, &text).await,
                    None => None,
                };
                Err(match (hint, gate) {
                    (Some(hint), Some(gate)) => anyhow!("{error}\n{hint}\n{gate}"),
                    (Some(hint), None) => anyhow!("{error}\n{hint}"),
                    (None, _) => error,
                })
            }
        }
    }

    /// Name WHICH side cannot pay for a buyer's terminal call, before the chain answers with a
    /// code the operator cannot act on.

    /// **The two failures look identical and require opposite actions.** Since contracts 4.0.36 the
    /// note attaches `DEAL_GAS_TERMINAL` to `stop` / `dispute` / `cleanupUnopened` -- but attaches it
    /// CONDITIONALLY (`PrivateNote.sol`: `if (currencies[SHELL] >= DEAL_GAS_TERMINAL)`). A note
    /// without that ECC sends the call anyway, with an empty map, and the deal falls back to its own
    /// reserve. So the call fails only when NEITHER can pay, and when it does the chain reports one
    /// thing -- an aborted action phase, `RESULT_CODE_NOT_ENOUGH_EXTRA` -- for two situations that the
    /// operator resolves differently: top up the note, or ask the seller to top up the deal (only
    /// the seller's note can, `fundDeployShell` is `onlyOwnerPubkey`).

    /// **Empty note alone is not a refusal**, and that is the point of reading both. It is the
    /// ordinary case for a buyer who spent everything on the deal, and the deal's reserve is exactly
    /// what it is for.

    /// Read-only, and it fails OPEN on an unreadable balance: a chain that will not answer is not
    /// evidence that money is missing, and refusing a buyer's exit on a failed read would take the
    /// exit away for the wrong reason.
    async fn refuse_terminal_call_neither_side_can_pay(
        &self,
        buyer_note: &Address,
        tc: &Address,
        call: &str,
    ) -> Result<()> {
        let charge = crate::params::DEAL_BURN_TERMINAL_RAW;
        let Ok(Some(note_account)) = self.client.get_account_retrying(buyer_note).await else {
            return Ok(());
        };
        let Ok(Some(deal_account)) = self.client.get_account_retrying(tc).await else {
            return Ok(());
        };
        let note_ecc = note_account.ecc_balance(crate::params::SHELL_CURRENCY_ID);
        let deal_ecc = deal_account.ecc_balance(crate::params::SHELL_CURRENCY_ID);
        match terminal_charge_refusal(call, buyer_note, note_ecc, tc, deal_ecc, charge) {
            Some(refusal) => Err(anyhow!(refusal)),
            None => Ok(()),
        }
    }

    /// The buyer stops the stream via their note: `streamStop(tokenContract)` -> `TC.stop()`
    /// (the TC checks `msg.sender == _buyer`). On the probe (before accept), buyer and seller each burn `P`,
    /// the remaining seller-bond `P` and buyer deposit return; in Streaming -- a standard split.
    pub async fn stream_stop(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
    ) -> Result<SubmittedBuyerStopReceipt> {
        self.refuse_terminal_call_neither_side_can_pay(buyer_note, tc, "streamStop")
            .await?;
        self.reject_prior_settlement_action_before_prepare(
            tc,
            SettlementAction::BuyerStop,
            Some(buyer_note),
        )
        .await?;
        let prepared = self
            .prepare_money_post(
                buyer_note,
                PRIVATENOTE_ABI,
                "streamStop",
                json!({ "tokenContract": tc.with_workchain() }),
                buyer_keys,
            )
            .await?;
        let client_message_id = external_message_hash(&prepared.1)
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        let receipt = self
            .submit_settlement_action_once(
                tc,
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::BuyerStop,
                Some(buyer_note),
                prepared,
            )
            .await?;
        Ok(SubmittedBuyerStopReceipt {
            receipt,
            client_message_id,
        })
    }

    /// The buyer **opens a dispute** via their note: `streamDispute(tokenContract)` -> `TC.dispute()`
    /// (the TC checks `msg.sender == _buyer`). This produces only `StreamDisputed`: funds remain
    /// frozen and there is no terminal split to project. A later concession or timeout has its own
    /// authoritative `DisputeResolved` receipt.
    pub async fn stream_dispute(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
    ) -> Result<SettlementActionReceipt> {
        self.refuse_terminal_call_neither_side_can_pay(buyer_note, tc, "streamDispute")
            .await?;
        self.reject_prior_settlement_action_before_prepare(
            tc,
            SettlementAction::Dispute,
            Some(buyer_note),
        )
        .await?;
        let prepared = self
            .prepare_money_post(
                buyer_note,
                PRIVATENOTE_ABI,
                "streamDispute",
                json!({ "tokenContract": tc.with_workchain() }),
                buyer_keys,
            )
            .await?;
        self.submit_settlement_action_once(
            tc,
            SettlementAction::Dispute,
            ExpectedSettlementEvent::StreamDisputed,
            Some(buyer_note),
            prepared,
        )
        .await
    }

    /// Prepare an automatic/inactivity-policy buyer STOP and its route before synchronously checking whether
    /// accepted output changed. A changed heartbeat cancels before the single money POST.

    /// Explicit operator/user STOP uses [`Self::stream_stop`] and remains unconditional after its normal
    /// actor/state/dispute preflight. This seam is only for a configured automatic failure policy: output
    /// resuming while its signed message is prepared cancels that stale policy decision.
    pub async fn stop_if_heartbeat(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
        before_post: &mut (dyn FnMut() -> bool + Send),
    ) -> Result<Option<SubmittedBuyerStopReceipt>> {
        self.reject_prior_settlement_action_before_prepare(
            tc,
            SettlementAction::BuyerStop,
            Some(buyer_note),
        )
        .await?;
        let prepared = self
            .prepare_money_post(
                buyer_note,
                PRIVATENOTE_ABI,
                "streamStop",
                json!({ "tokenContract": tc.with_workchain() }),
                buyer_keys,
            )
            .await?;
        let client_message_id = external_message_hash(&prepared.1)
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        let receipt = self
            .submit_settlement_action_once_if(
                tc,
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::BuyerStop,
                Some(buyer_note),
                prepared,
                before_post,
            )
            .await?;
        Ok(receipt.map(|receipt| SubmittedBuyerStopReceipt {
            receipt,
            client_message_id,
        }))
    }

    /// The buyer cleans up a funded-but-never-opened deal via their note:
    /// `streamCleanup(tokenContract)` -> `TC.cleanupUnopened()`. Requires
    /// `block.timestamp >= _fundedTime + MATCH_OPEN_TIMEOUT` and `!_opened`.
    pub async fn stream_cleanup(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
    ) -> Result<Value> {
        self.submit(
            buyer_note,
            PRIVATENOTE_ABI,
            "streamCleanup",
            json!({ "tokenContract": tc.with_workchain() }),
            buyer_keys,
        )
        .await
    }

    /// Bring the seller's `RootModel` into existence and wait for it to be `Active`.

    /// **NO LONGER A DEPLOY THIS PROCESS SENDS.** It is one call to
    /// [`request_root_model_deploy`](Self::request_root_model_deploy) -- `SuperRoot.deployRootModel` --
    /// followed by the same activation wait as before. The note pre-funding step that used to precede
    /// this is gone with the leg that performed it: SuperRoot attaches the deploy value itself.

    /// The name is kept because the callers' contract is unchanged: hand it the owner key, get back
    /// the address of an `Active` RootModel.
    pub async fn deploy_root_model_note_funded(&self, owner: &KeyPair) -> Result<Address> {
        let owner_pubkey = json!(format!("0x{}", owner.public_hex()));
        let addr = self.root_model_address_for(&owner_pubkey).await?;
        // `deployRootModel` is idempotent at the contract (a `new` at an occupied address does not
        // overwrite and does not revert), so a re-issued request on an already-Active root is a no-op.
        let submit_err = self
            .request_root_model_deploy(owner, &owner_pubkey)
            .await
            .err();
        if self
            .wait_active(&addr, crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS)
            .await
        {
            if let Some(e) = submit_err {
                eprintln!(
                    "RootModel {} became Active after SuperRoot.deployRootModel returned an error \
                     (treating as landed): {e}",
                    display_dexdo_address(&addr)
                );
            }
            Ok(addr)
        } else if let Some(e) = submit_err {
            Err(e)
        } else {
            Err(anyhow!(
                "RootModel {} did not activate within the allotted time after \
                 SuperRoot.deployRootModel",
                display_dexdo_address(&addr)
            ))
        }
    }

    /// The per-deal `TokenContract` address, from its INIT-DATA (stateInit) alone -- offline, no
    /// network, no signature, NO DEPLOY MESSAGE.

    /// **It stopped building a deploy message in 4.0.36, and the reason is the point.** This used to
    /// call `build_deploy` and return the message beside the address, because this client deployed
    /// the deal itself with an external signed message and wanted the checked address to be
    /// bit-for-bit the one that deploy would create. That deploy is gone: the 4.0.36 constructor
    /// requires `msg.sender` to be the canonical note, so the deal is deployed by
    /// `PrivateNote.deployDeal` and there is no external message for this to build.

    /// Keeping `build_deploy` here would have been worse than useless. Its `call_set` must satisfy
    /// the constructor's ABI, and the 4.0.36 constructor takes a sixth argument
    /// (`depositIdentifierHash`) that this client does not hold and must not invent -- the note
    /// passes its OWN, which is exactly what makes the deal's authentication mean anything. It would
    /// also hand back a signed deploy message that must never be sent.

    /// The address never depended on any of that. It is `hash(stateInit)` over
    /// `{code, varInit {_sellerPubkey, _rootModelAddress, _nonce}, pubkey}` -- constructor arguments
    /// do not enter it, which is precisely why deal terms have to be authenticated separately. So
    /// this encodes the stateInit and nothing else, and `_pubkey` is supplied explicitly rather than
    /// injected from a keypair, because there is no keypair in this call any more.

    /// The on-chain counterpart is `DexLib.buildTokenContractStateInit` (the note deploys through
    /// it) and `DexLib.computeCanonicalTokenContractAddress` (everyone else derives through that).
    /// All three MUST agree on the same three statics; they are the same three here.
    async fn token_contract_stateinit_address(
        &self,
        seller_pubkey_hex: &str,
        root_model: &Address,
        nonce: u64,
    ) -> Result<Address> {
        use tvm_client::abi::{Abi, DeploySet, ParamsOfEncodeMessage, Signer};
        let pubkey = format!("0x{seller_pubkey_hex}");
        let encoded = tvm_client::abi::encode_message(
            local_context()?,
            ParamsOfEncodeMessage {
                abi: Abi::Json(TOKENCONTRACT_ABI.to_string()),
                address: None,
                deploy_set: Some(DeploySet {
                    tvc: Some(base64::engine::general_purpose::STANDARD.encode(TOKENCONTRACT_TVC)),
                    code: None,
                    state_init: None,
                    workchain_id: Some(0),
                    initial_data: Some(json!({
                        "_sellerPubkey": pubkey,
                        "_rootModelAddress": root_model.with_workchain(),
                        "_nonce": nonce.to_string(),
                        // ABI >= 2.4 carries the tvm pubkey as an `init` storage field, so it is
                        // part of the stateInit and therefore part of the address. `build_deploy`
                        // used to inject it from the signing keypair; with no signer here it is
                        // stated outright, and it is the seller key either way.
                        "_pubkey": pubkey,
                    })),
                    initial_pubkey: None,
                }),
                // NO `call_set`: a constructor body is what a deploy message needs, and this builds
                // no deploy message. Address only.
                call_set: None,
                signer: Signer::None,
                processing_try_index: None,
                signature_id: None,
            },
        )
        .await
        .map_err(|e| anyhow!("encode TokenContract stateInit: {e}"))?;
        Address::parse(&encoded.address)
    }

    // `deploy_token_contract_note_funded` USED TO BE HERE, and it is gone rather than deprecated.

    // It was directive's path: the note pre-funded the deal's uninit address with ECC[2]
    // (`fundDeployShell`), then this client sent a seller-signed EXTERNAL message carrying the whole
    // contract code, and waited for the account to go Active. Contracts 4.0.36 refuse that message
    // at the constructor -- `msg.sender` must BE the canonical note for `depositIdentifierHash`
    // (`contracts/airegistry/TokenContract.sol:285`), and an external message has no sender to
    // offer. There is no version of this function that works against this generation, so leaving a
    // shell of it behind would only offer a caller a path that always fails.

    // Its replacement is one owner call: `note_deploy_deal` -> `PrivateNote.deployDeal`, which
    // carries the deal's ECC[2] reserve on the same message. `provision_market` waits for the
    // derived address to activate, because an internal deploy lands after the call returns.

    /// Provision a per-deal market for the seller (issue; **note-funded,** -- NO operator wallet, NO
    /// giver in the operate path): deploy-if-absent the per-model `InferenceOrderBook`, the per-owner
    /// `RootModel`, and the per-deal `TokenContract`, **all funded from the seller note's own ECC[2]**. Returns a
    /// [`MarketManifest`] whose `token_contract` is the **active** deployed address.

    /// The per-deal `TokenContract` (and `RootModel`) is a self-dapp contract whose uninit cross-dapp deploy
    /// address cannot be funded with privileged native gas (the 404). Instead the note pre-funds each uninit
    /// deploy address with **ECC[2] SHELL** via [`note_fund_deploy_shell`](Self::note_fund_deploy_shell)
    /// (`PrivateNote.fundDeployShell`, a single `flag:16` send so the ECC lands as spendable native balance), and
    /// the external seller-signed deploy then activates it -- the permission-free mechanism, no privileged giver,
    /// no separate operational wallet (the funding source is the anonymous note itself). `gas` is the ECC[2]
    /// SHELL pre-funded per uninit deploy address.
    #[allow(clippy::too_many_arguments)]
    pub async fn provision_market(
        &self,
        seed_keys: &KeyPair,
        note: &Address,
        frame_model: &str,
        nonce: u64,
        price_per_tick: u128,
        max_ticks: u128,
        gas: u128,
    ) -> Result<crate::MarketManifest> {
        let deal_gas_overhead_raw =
            crate::params::resolve_deal_gas_overhead_raw(self.network(), None)
                .map_err(anyhow::Error::msg)?;
        self.provision_market_with_deal_gas_overhead(
            seed_keys,
            note,
            frame_model,
            nonce,
            price_per_tick,
            max_ticks,
            gas,
            deal_gas_overhead_raw,
        )
        .await
    }

    /// Provision using the measured remainder selected for the runtime network.
    #[allow(clippy::too_many_arguments)]
    pub async fn provision_market_with_deal_gas_overhead(
        &self,
        seed_keys: &KeyPair,
        note: &Address,
        frame_model: &str,
        nonce: u64,
        price_per_tick: u128,
        max_ticks: u128,
        gas: u128,
        deal_gas_overhead_raw: u128,
    ) -> Result<crate::MarketManifest> {
        // fail-closed up front if the seller note is orphaned by a contract redeploy -- a clear
        // "re-mint" error instead of a downstream bare TVM_ERROR (stale note) or "note is not active".
        self.assert_seller_note_current(note).await?;
        // 1) Per-model InferenceOrderBook -- note-funded (owner-method). Deploy-if-absent.
        let model_hash = model_hash_for(frame_model);
        let ob = self
            .inference_orderbook_address(note, &model_hash, TICK_SIZE)
            .await?;
        if !self
            .wait_active(&ob, crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS)
            .await
        {
            self.deploy_inference_orderbook(note, seed_keys, &model_hash, frame_model, TICK_SIZE)
                .await?;
            if !self
                .wait_active(&ob, crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS)
                .await
            {
                return Err(anyhow!(
                    "InferenceOrderBook {} did not activate",
                    display_dexdo_address(&ob)
                ));
            }
        }
        // 2) RootModel + per-deal TokenContract. ORDER MATTERS: the RootModel exists first so the per-deal
        // TC registers into it in its ctor.

        // THE TWO ARE NO LONGER PROVISIONED THE SAME WAY (4.0.34). The RootModel is asked for --
        // `SuperRoot.deployRootModel`, an internal `new` that carries its own 5 vmshell -- so there is
        // nothing for the note to pre-fund and no external deploy to send; an external one would be
        // refused, `ERR_INVALID_SENDER = 302` (`contracts/airegistry/RootModel.sol:67`). The per-deal TC
        // is still NOTE-FUNDED: the note pre-funds its uninit deploy address from its own ECC[2]
        // (`fundDeployShell`, the note derives the target from `(ephemeralPubkey, nonce)`), then the
        // external seller-signed deploy activates it. That is why only one `gas` allocation is spent here
        // where two used to be.

        // The RootModel address comes from `SuperRoot.getRootModelAddress` rather than from a locally
        // built deploy message: SuperRoot derives the child from its OWN pinned `_rootModelCode`, which
        // is the code that actually lands, and SuperRoot sits at a fixed zerostate address that is always
        // `Active`, so this getter cannot 404 the way a not-yet-`Active` RootModel's would. The TC address
        // stays derived **locally from the deploy INIT-DATA**, NOT
        // from the RootModel `getTokenContractAddress` getter, so a not-yet-`Active` RootModel cannot
        // abort the idempotency check; that getter is used only as a post-`Active` cross-check below.
        let seller_pubkey = json!(format!("0x{}", seed_keys.public_hex()));
        let rm = self.root_model_address_for(&seller_pubkey).await?;
        let tc = self
            .token_contract_deploy_address(seed_keys, &rm, nonce)
            .await?;
        let rm_absent = !self
            .wait_active(&rm, crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS)
            .await;
        if rm_absent {
            self.deploy_root_model_note_funded(seed_keys).await?;
        }
        // The per-deal TC address is derived from the deploy INIT-DATA (stateInit), NOT the RootModel
        // `getTokenContractAddress` network getter: on a fresh provision the RootModel deploy was just
        // sent (step above) but is not yet `Active`, so the getter would 404 and abort this idempotent check.
        if self
            .wait_active(&tc, crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS)
            .await
        {
            // Idempotent skip: the TC is already `Active` => the RootModel is guaranteed `Active`, so the getter
            // is safe here -- cross-check it agrees with the INIT-DATA derivation (catch a code-hash/derivation
            // divergence between the embedded TC image and the deployed RootModel).
            let getter_tc = self
                .resolve_token_contract(&rm, &seller_pubkey, nonce)
                .await?;
            if getter_tc.with_workchain() != tc.with_workchain() {
                return Err(anyhow!(
                    "RootModel getTokenContractAddress {} != INIT-DATA-derived {} (TC derivation diverged)",
                    display_token_contract(&getter_tc),
                    display_token_contract(&tc)
                ));
            }
        } else {
            // Deploy-if-absent, FROM THE NOTE (contracts 4.0.36). One owner call replaces what used
            // to be two steps: `fundDeployShell` placing ECC[2] at the uninit address, then a
            // seller-signed external deploy carrying the code. The constructor refuses an external
            // deploy now -- it requires `msg.sender` to be the canonical note for its
            // `depositIdentifierHash` -- so there is no longer a form of this that pre-funds.

            // `gas` stops being a life-support budget and becomes the deal's ECC[2] RESERVE, which
            // each entry burns its measured charge out of. It rides on this same message.
            self.log_deploy_prefund_snapshot("before deployDeal", note, &rm, &tc)
                .await;
            self.note_deploy_deal(
                note,
                seed_keys,
                NoteDeployDealRequest {
                    nonce,
                    model_name: frame_model,
                    price_per_tick,
                    max_ticks,
                    gas_reserve: gas,
                },
            )
            .await
            .context("note-deployed provision: PrivateNote.deployDeal failed")?;
            self.log_deploy_prefund_snapshot("after deployDeal", note, &rm, &tc)
                .await;
            // The deploy is an INTERNAL message the note emits, so the owner call returning is not
            // the deal existing: wait for the address this provision already derived to go Active.
            // That derived address is also the convergence guard the old external path needed a
            // separate check for -- nothing here reports an address of its own to disagree with it.
            if !self
                .wait_active(&tc, crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS)
                .await
            {
                return Err(anyhow!(
                    "TokenContract {} did not activate after PrivateNote.deployDeal (nonce {nonce})",
                    display_token_contract(&tc)
                ));
            }
        }
        // The deal only. The RootModel used to be topped up here too; it mints its own gas now that
        // SuperRoot's deploy puts it in SuperRoot's configured dapp.
        self.ensure_deal_contract_gas_with_overhead(
            note,
            seed_keys,
            nonce,
            Some(&tc),
            deal_gas_overhead_raw,
        )
        .await?;
        Ok(crate::MarketManifest {
            network: self.network().to_string(),
            frame_model: frame_model.to_string(),
            model_hash,
            inference_order_book: ob.with_workchain(),
            root_model: rm.with_workchain(),
            token_contract: tc.with_workchain(),
            seller_note: note.with_workchain(),
            nonce,
            price_per_tick,
            max_ticks,
        })
    }

    pub async fn root_oracle_address(&self) -> Result<Address> {
        Address::parse(ROOTORACLE_ADDR)
    }

    pub async fn root_pn_address(&self) -> Result<Address> {
        Address::parse(ROOTPN_ADDR)
    }

    async fn assert_deployed_contract_read_identity(
        &self,
        contract: &str,
        address: &Address,
    ) -> Result<()> {
        let display_address = display_dexdo_address(address);
        let account = self
            .client
            .get_account_retrying(address)
            .await?
            .ok_or_else(|| anyhow!("{contract} {display_address} account is not found"))?;
        let actual = active_account_code_hash(contract, address, &account)?;
        let expected = self.expected_contract_hash(contract)?;
        if actual != expected {
            let network = self.network();
            return Err(anyhow!(
                "{contract} {display_address} serves code this build does not expect for \
                 {network}: expected {expected}, live is {actual}. Use a dexdo built for this \
                 chain's generation."
            ));
        }
        Ok(())
    }

    pub async fn assert_root_oracle_read_identity(&self) -> Result<()> {
        let root = self.root_oracle_address().await?;
        self.assert_deployed_contract_read_identity("RootOracle", &root)
            .await
    }

    pub async fn assert_root_pn_read_identity(&self) -> Result<()> {
        let root = self.root_pn_address().await?;
        self.assert_deployed_contract_read_identity("RootPN", &root)
            .await
    }

    pub async fn assert_oracle_read_identity(&self, oracle: &Address) -> Result<()> {
        self.assert_deployed_contract_read_identity("Oracle", oracle)
            .await
    }

    /// Read the Oracle's factual ECC[2] fee balance and bind `signer` to the owner key stored in
    /// the same active/current account snapshot. `Oracle` exposes no fee-balance getter.
    pub async fn oracle_fee_balance_for_owner(
        &self,
        oracle: &Address,
        signer: &KeyPair,
    ) -> Result<u128> {
        let account = self
            .client
            .get_account_retrying(oracle)
            .await?
            .ok_or_else(|| anyhow!("Oracle {oracle} account is not found"))?;
        let actual = active_account_code_hash("Oracle", oracle, &account)?;
        let expected = self.expected_contract_hash("Oracle")?;
        if actual != expected {
            let network = self.network();
            return Err(anyhow!(
                "Oracle {oracle} serves code this build does not expect for {network}: expected \
                 {expected}, live is {actual}. Use a dexdo built for this chain's generation."
            ));
        }
        let fields = account_storage_fields(
            account
                .boc
                .as_deref()
                .ok_or_else(|| anyhow!("Oracle {oracle} account BOC is unavailable"))?,
            ORACLE_ABI,
            "Oracle",
        )?;
        let owner = value_to_uint256_hex(&fields["_oraclePubkey"])
            .ok_or_else(|| anyhow!("Oracle {oracle} storage exposes no _oraclePubkey"))?;
        let signer = normalize_uint256_hex(&pubkey_uint256(signer))?;
        if owner != signer {
            return Err(anyhow!(
                "oracle signer {} does not own Oracle {oracle}",
                signer
            ));
        }
        Ok(account.ecc_balance(crate::params::SHELL_CURRENCY_ID))
    }

    pub async fn withdraw_oracle_fees(
        &self,
        oracle: &Address,
        signer: &KeyPair,
        to: &Address,
        amount: u128,
    ) -> Result<Value> {
        self.submit_money_call_once(
            oracle,
            ORACLE_ABI,
            "withdrawFees",
            oracle_withdraw_fees_payload(&to.with_workchain(), amount),
            signer,
        )
        .await
    }

    pub async fn oracle_address(&self, oracle_name: &str) -> Result<Address> {
        let root = self.root_oracle_address().await?;
        let v = self
            .client
            .run_getter_retrying(
                &root,
                ROOTORACLE_ABI,
                "getOracleAddress",
                json!({ "name": oracle_name }),
            )
            .await?
            .ok_or_else(|| anyhow!("RootOracle is not active"))?;
        Address::parse(
            v["oracleAddress"]
                .as_str()
                .ok_or_else(|| anyhow!("no address"))?,
        )
    }

    pub async fn deploy_oracle(&self, oracle_keys: &KeyPair, oracle_name: &str) -> Result<Value> {
        let root = self.root_oracle_address().await?;
        self.submit(
            &root,
            ROOTORACLE_ABI,
            "deployOracle",
            json!({
                "oraclePubkey": pubkey_uint256(oracle_keys),
                "oracleName": oracle_name,
            }),
            oracle_keys,
        )
        .await
    }

    pub async fn oracle_event_list_address(
        &self,
        oracle: &Address,
        index: u128,
    ) -> Result<Address> {
        let v = self
            .client
            .run_getter_retrying(
                oracle,
                ORACLE_ABI,
                "getEventListAddress",
                json!({ "index": index.to_string() }),
            )
            .await?
            .ok_or_else(|| anyhow!("Oracle is not active"))?;
        Address::parse(v["value0"].as_str().ok_or_else(|| anyhow!("no address"))?)
    }

    pub async fn deploy_oracle_event_list(
        &self,
        oracle: &Address,
        oracle_keys: &KeyPair,
        index: u128,
        description: &str,
    ) -> Result<Value> {
        self.submit(
            oracle,
            ORACLE_ABI,
            "deployEventList",
            json!({
                "index": index.to_string(),
                "description": description,
            }),
            oracle_keys,
        )
        .await
    }

    pub async fn assert_oracle_event_list_read_identity(&self, event_list: &Address) -> Result<()> {
        self.assert_deployed_contract_read_identity("OracleEventList", event_list)
            .await
    }

    pub async fn oracle_event_list_events(&self, oel: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(oel, ORACLEEVENTLIST_ABI, "_events", json!({}))
            .await
    }

    pub async fn oracle_event_info(&self, oel: &Address, event_id: &str) -> Result<Option<Value>> {
        let events = self.oracle_event_list_events(oel).await?.ok_or_else(|| {
            anyhow!(
                "OracleEventList {} _events getter unavailable",
                display_dexdo_address(oel)
            )
        })?;
        Ok(event_from_getter_output(&events, event_id).cloned())
    }

    pub async fn oracle_range_data(&self, oel: &Address, event_id: &str) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(
                oel,
                ORACLEEVENTLIST_ABI,
                "getRangeData",
                json!({ "eventId": normalize_uint256_hex(event_id)? }),
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_range_event(
        &self,
        oel: &Address,
        oracle_keys: &KeyPair,
        event_name: &str,
        oracle_fee: u128,
        deadline: u64,
        describe: &str,
        bounds: &[String],
        outcome_names: &[String],
        order_book: &Address,
    ) -> Result<Value> {
        self.submit(
            oel,
            ORACLEEVENTLIST_ABI,
            "addRangeEvent",
            json!({
                "eventName": event_name,
                "oracleFee": oracle_fee.to_string(),
                "deadline": deadline.to_string(),
                "describe": describe,
                "bounds": bounds,
                "outcomeNames": dense_string_map(outcome_names),
                "ob": order_book.with_workchain(),
            }),
            oracle_keys,
        )
        .await
    }

    pub async fn pmp_address(
        &self,
        event_id: &str,
        oracle_names: &[String],
        token_type: u32,
    ) -> Result<Address> {
        let root = self.root_pn_address().await?;
        let v = self
            .client
            .run_getter_retrying(
                &root,
                ROOTPN_ABI,
                "getPMPAddress",
                json!({
                    "eventId": normalize_uint256_hex(event_id)?,
                    "names": oracle_names,
                    "tokenType": token_type,
                }),
            )
            .await?
            .ok_or_else(|| anyhow!("RootPN is not active"))?;
        Address::parse(
            v["pmpAddress"]
                .as_str()
                .ok_or_else(|| anyhow!("no address"))?,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn deploy_pmp(
        &self,
        note: &Address,
        note_keys: &KeyPair,
        event_id: &str,
        oracle_fees: &[u128],
        token_type: u32,
        oracle_names: &[String],
        oracle_indexes: &[u128],
        initial_stakes: &[u128],
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "deployPMP",
            json!({
                "eventId": normalize_uint256_hex(event_id)?,
                "oracleFee": u128_array(oracle_fees),
                "tokenType": token_type,
                "names": oracle_names,
                "index": u128_array(oracle_indexes),
                "initialStakes": u128_array(initial_stakes),
            }),
            note_keys,
        )
        .await
    }

    /// Validate the manifest tuple against the current compiled PMP and its live `getDetails`.
    /// Participant exits do not have an Oracle signer, so they cannot reuse the Oracle-owned
    /// cancellation preflight.
    pub async fn assert_pmp_market_identity(
        &self,
        manifest: &OracleMarketManifest,
    ) -> Result<(Address, Value)> {
        manifest.validate().map_err(anyhow::Error::msg)?;
        let pmp = Address::parse(&manifest.pmp).context("oracle manifest pmp")?;
        let details = self
            .assert_pmp_identity_for_triple(
                &pmp,
                &manifest.event_id,
                &manifest.oracle_list_hash,
                manifest.token_type,
                "the manifest",
            )
            .await?;
        Ok((pmp, details))
    }

    /// The same identity proof without a manifest file: the contract keys a stake by the triple,
    /// so the triple is what has to be proven against the live PMP. Code identity is checked by
    /// exactly the same reader, so this is not a weaker route -- only one that does not require an
    /// artefact the run that created the stake is free to delete.
    pub async fn assert_pmp_identity_for_triple(
        &self,
        pmp: &Address,
        event_id: &str,
        oracle_list_hash: &str,
        token_type: u32,
        source: &str,
    ) -> Result<Value> {
        let (_, details) = self.validated_pmp_read_identity(pmp).await?;
        validate_pmp_triple(&details, event_id, oracle_list_hash, token_type, source)?;
        Ok(details)
    }

    /// One coherent PrivateNote storage snapshot for a PMP participant exit. The exact stake key
    /// is the contract's `tvm.hash(abi.encode(eventId, oracleListHash, tokenType))`; private
    /// `_openOrdersByEvent` is decoded from the same account BOC because `getDetails` does not expose
    /// it. The returned JSON has a stable client-owned shape consumed by the CLI pre/post checks.
    pub async fn private_note_pmp_exit_state(
        &self,
        note: &Address,
        event_id: &str,
        oracle_list_hash: &str,
        token_type: u32,
    ) -> Result<Value> {
        let account = self
            .client
            .get_account_retrying(note)
            .await?
            .ok_or_else(|| anyhow!("PrivateNote {note} account is not found"))?;
        note_balance_private_note_account(self.private_note_pin()?, note, Some(&account))?;
        let fields = account_storage_fields(
            account
                .boc
                .as_deref()
                .ok_or_else(|| anyhow!("PrivateNote {note} account BOC is unavailable"))?,
            PRIVATENOTE_ABI,
            "PrivateNote",
        )?;
        let stake_key = pmp_stake_key(event_id, oracle_list_hash, token_type)?;
        let stake = uint256_map_entry(&fields["_stakes"], &stake_key).cloned();
        if let Some(stake) = stake.as_ref() {
            let live_oracles = value_to_uint256_hex(&stake["oracleListHash"])
                .ok_or_else(|| anyhow!("PrivateNote {note} stake exposes no oracleListHash"))?;
            if live_oracles != normalize_uint256_hex(oracle_list_hash)? {
                return Err(anyhow!(
                    "PrivateNote {note} stake oracleListHash does not match the manifest"
                ));
            }
            if value_u128(&stake["tokenType"]) != Some(u128::from(token_type)) {
                return Err(anyhow!(
                    "PrivateNote {note} stake tokenType does not match the manifest"
                ));
            }
        }
        let candidate_amount = match stake.as_ref() {
            Some(stake) => value_u128(&stake["candidateAmount"])
                .ok_or_else(|| anyhow!("PrivateNote {note} stake exposes no candidateAmount"))?,
            None => 0,
        };
        let amount_slots = match stake.as_ref() {
            Some(stake) => stake["amount"]
                .as_array()
                .ok_or_else(|| anyhow!("PrivateNote {note} stake exposes no amount array"))?
                .len(),
            None => 0,
        };
        let open_orders_by_event = fields
            .get("_openOrdersByEvent")
            .filter(|map| map.is_object() || map.is_array())
            .ok_or_else(|| {
                anyhow!("PrivateNote {note} storage exposes no _openOrdersByEvent map")
            })?;
        let open_orders = uint256_map_entry(open_orders_by_event, &stake_key)
            .and_then(value_u128)
            .unwrap_or(0);
        let open_orders = u32::try_from(open_orders).map_err(|_| {
            anyhow!("PrivateNote {note} _openOrdersByEvent[{stake_key}] exceeds uint32")
        })?;
        let has_withdrawn = getter_bool(&fields, "_hasWithdrawn")
            .ok_or_else(|| anyhow!("PrivateNote {note} storage exposes no _hasWithdrawn"))?;
        let balance = fields
            .get("_balance")
            .filter(|map| map.is_object() || map.is_array())
            .ok_or_else(|| anyhow!("PrivateNote {note} storage exposes no _balance map"))?;
        let note_balance = uint32_map_entry(balance, token_type)
            .and_then(value_u128)
            .unwrap_or(0);
        let coupons_value = value_u128(&fields["_couponsValue"])
            .ok_or_else(|| anyhow!("PrivateNote {note} storage exposes no _couponsValue"))?;
        let busy = fields
            .get("_busy")
            .ok_or_else(|| anyhow!("PrivateNote {note} storage exposes no _busy"))?;
        Ok(json!({
            "stake_key": stake_key,
            "stake_present": stake.is_some(),
            "candidate_amount": candidate_amount.to_string(),
            "amount_slots": amount_slots,
            "open_orders": open_orders,
            "busy_address": optional_address(busy),
            "has_withdrawn": has_withdrawn,
            "note_balance": note_balance.to_string(),
            "coupons_value": coupons_value.to_string(),
        }))
    }

    pub async fn cancel_pmp_stake(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        event_id: &str,
        oracle_list_hash: &str,
        token_type: u32,
    ) -> Result<Value> {
        self.submit_money_call_once(
            note,
            PRIVATENOTE_ABI,
            "cancelStake",
            private_note_pmp_exit_payload(event_id, oracle_list_hash, token_type)?,
            owner_keys,
        )
        .await
    }

    pub async fn claim_pmp_stake(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        event_id: &str,
        oracle_list_hash: &str,
        token_type: u32,
    ) -> Result<Value> {
        self.submit_money_call_once(
            note,
            PRIVATENOTE_ABI,
            "claim",
            private_note_pmp_exit_payload(event_id, oracle_list_hash, token_type)?,
            owner_keys,
        )
        .await
    }

    /// Submit owner-signed `PrivateNote.deleteStake(eventId, oracleListHash, tokenType)`:
    /// ABANDON this note's stake so the record clears and the note stops being frozen.

    /// The third exit, and the only one with no lifecycle gate on the PMP side --
    /// `PMP.forfeitStake` checks the sender and nothing else, where `cancelStake` needs the event
    /// cancelled, the book drained, and (for the deployer, which any stake this client holds
    /// belongs to) the freeze-time clean refund acknowledged. That is why it exists: three
    /// reachable states leave `cancelStake` permanently refusing while `_stakes` keeps
    /// `withdrawTokens` and `initTransfer` shut, freezing the note's WHOLE balance.

    /// WHAT IT COSTS. The abandoned mass goes into `PMP._forfeited`, which is never paid to the
    /// forfeiter. It leaves at the market's close, to `PrivateNote(_deployer).acceptFee` -- and for
    /// any stake reachable from this client the deployer IS this note, because `deployPMP` is its
    /// only stake-creating path. So it can come home, at a time other parties choose, and only if
    /// this note has not withdrawn by then: `acceptFee` returns without crediting when
    /// `_hasWithdrawn`, and that money is then beyond every command we have, sweep included.

    /// The caller is responsible for the consent gate; this is the transport.
    pub async fn forfeit_pmp_stake(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        event_id: &str,
        oracle_list_hash: &str,
        token_type: u32,
    ) -> Result<Value> {
        self.submit_money_call_once(
            note,
            PRIVATENOTE_ABI,
            "deleteStake",
            private_note_pmp_exit_payload(event_id, oracle_list_hash, token_type)?,
            owner_keys,
        )
        .await
    }

    pub async fn pmp_details(&self, pmp: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(pmp, PMP_ABI, "getDetails", json!({}))
            .await
    }

    async fn validated_pmp_read_identity(&self, pmp: &Address) -> Result<(tvm_types::Cell, Value)> {
        let display_pmp = display_dexdo_address(pmp);
        let account = self
            .client
            .get_account_retrying(pmp)
            .await?
            .ok_or_else(|| anyhow!("PMP {display_pmp} account is not found"))?;
        let (actual_hash, actual_code) = active_account_code("PMP", pmp, &account)?;
        let salt = validate_salted_code_from_current_base(
            "PMP",
            pmp,
            &actual_hash,
            &actual_code,
            PMP_TVC,
        )?;
        let private_note_code = decode_pmp_private_note_code(salt)?;
        let private_note_hash = validate_private_note_generation(
            self.private_note_pin()?,
            &format!("PMP {display_pmp}"),
            &private_note_code,
        )?;
        let details = self
            .pmp_details(pmp)
            .await?
            .ok_or_else(|| anyhow!("PMP {display_pmp} getDetails unavailable"))?;
        if getter_code_hash(&details, "privateNoteCodeHash").as_deref()
            != Some(private_note_hash.as_str())
        {
            return Err(anyhow!(
                "PMP {display_pmp} getDetails PrivateNote generation does not match its code salt"
            ));
        }
        Ok((actual_code, details))
    }

    pub async fn assert_pmp_read_identity(&self, pmp: &Address) -> Result<()> {
        self.validated_pmp_read_identity(pmp).await.map(|_| ())
    }

    pub async fn pmp_shutdown_state(&self, pmp: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(pmp, PMP_ABI, "getShutdownState", json!({}))
            .await
    }

    pub async fn pmp_unclaimed_balance(&self, pmp: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(pmp, PMP_ABI, "getUnclaimedBalance", json!({}))
            .await
    }

    pub async fn pmp_version(&self, pmp: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(pmp, PMP_ABI, "getVersion", json!({}))
            .await
    }

    /// Fail-closed OEL/signer/event identity preflight. It intentionally does not require a live
    /// PMP, because a deletable event outlives the PMP that released its confirmation.
    pub async fn assert_oracle_event_identity(
        &self,
        manifest: &OracleMarketManifest,
        signer: &KeyPair,
    ) -> Result<(Address, Value)> {
        manifest.validate().map_err(anyhow::Error::msg)?;
        let oel = Address::parse(&manifest.oracle_event_list)
            .context("oracle manifest oracle_event_list")?;
        let oracle = Address::parse(&manifest.oracle).context("oracle manifest oracle")?;
        let display_oel = display_dexdo_address(&oel);

        let oel_account = self
            .client
            .get_account_retrying(&oel)
            .await?
            .filter(Account::is_active)
            .ok_or_else(|| anyhow!("OracleEventList {display_oel} is not Active"))?;
        let expected_oel_hash = self.expected_contract_hash("OracleEventList")?;
        let actual_oel_hash = oel_account
            .code_hash
            .as_deref()
            .and_then(normalize_code_hash)
            .ok_or_else(|| anyhow!("OracleEventList {display_oel} exposes no code hash"))?;
        if actual_oel_hash != expected_oel_hash {
            return Err(anyhow!(
                "OracleEventList {display_oel} code hash does not match the deployed manifest"
            ));
        }
        let oel_fields =
            oracle_event_list_storage_fields(oel_account.boc.as_deref().ok_or_else(|| {
                anyhow!("OracleEventList {display_oel} account BOC is unavailable")
            })?)?;
        let index = validate_oracle_event_list_identity(&oel_fields, manifest, signer)?;
        let canonical_oel = self.oracle_event_list_address(&oracle, index).await?;
        if canonical_oel.with_workchain() != oel.with_workchain() {
            return Err(anyhow!(
                "OracleEventList {display_oel} is not canonical oracle {} index {index}",
                display_dexdo_address(&manifest.oracle)
            ));
        }
        let event = self
            .oracle_event_info(&oel, &manifest.event_id)
            .await?
            .ok_or_else(|| anyhow!("event {} is absent from {display_oel}", manifest.event_id))?;
        let range = self
            .oracle_range_data(&oel, &manifest.event_id)
            .await?
            .ok_or_else(|| anyhow!("event {} has no range data", manifest.event_id))?;
        validate_oracle_event_manifest(&event, &range, manifest)?;
        Ok((oel, event))
    }

    /// Full fail-closed identity preflight for PMP cancellation.
    pub async fn assert_oracle_market_identity(
        &self,
        manifest: &OracleMarketManifest,
        signer: &KeyPair,
    ) -> Result<(Address, Address, Value, Value)> {
        let (oel, event) = self.assert_oracle_event_identity(manifest, signer).await?;
        let pmp = Address::parse(&manifest.pmp).context("oracle manifest pmp")?;
        let display_pmp = display_dexdo_address(&pmp);
        let pmp_account = self
            .client
            .get_account_retrying(&pmp)
            .await?
            .filter(Account::is_active)
            .ok_or_else(|| anyhow!("PMP {display_pmp} is not Active"))?;
        let details = self
            .pmp_details(&pmp)
            .await?
            .ok_or_else(|| anyhow!("PMP {display_pmp} getDetails unavailable"))?;
        validate_pmp_manifest(&details, manifest)?;
        let deployer = pmp_deployer(&details)?;
        let deployer_account = self.client.get_account_retrying(&deployer).await?;
        note_balance_private_note_account(
            self.private_note_pin()?,
            &deployer,
            deployer_account.as_ref(),
        )?;
        let pmp_code = self
            .client
            .run_getter_retrying(&deployer, PRIVATENOTE_ABI, "getPMPCode", json!({}))
            .await?;
        validate_salted_pmp_identity(
            self.private_note_pin()?,
            &pmp,
            pmp_account.code_hash.as_deref(),
            &deployer,
            deployer_account.as_ref(),
            pmp_code.as_ref(),
        )?;
        if !self
            .oracle_event_list_has_pmp_confirmation(&oel, &pmp, &manifest.event_id)
            .await?
        {
            return Err(anyhow!(
                "PMP {display_pmp} has no active confirmation for event {}",
                manifest.event_id
            ));
        }
        Ok((oel, pmp, details, event))
    }

    pub async fn oracle_event_list_has_pmp_confirmation(
        &self,
        oel: &Address,
        pmp: &Address,
        event_id: &str,
    ) -> Result<bool> {
        let display_oel = display_dexdo_address(oel);
        let account = self
            .client
            .get_account_retrying(oel)
            .await?
            .filter(Account::is_active)
            .ok_or_else(|| anyhow!("OracleEventList {display_oel} is not Active"))?;
        let fields =
            oracle_event_list_storage_fields(account.boc.as_deref().ok_or_else(|| {
                anyhow!("OracleEventList {display_oel} account BOC is unavailable")
            })?)?;
        oracle_pmp_confirmation_is_active(&fields, pmp, event_id)
    }

    pub async fn submit_pmp_cancel_event(
        &self,
        pmp: &Address,
        oracle_keys: &KeyPair,
    ) -> Result<Value> {
        self.submit(pmp, PMP_ABI, "submitCancelEvent", json!({}), oracle_keys)
            .await
    }

    pub async fn delete_oracle_event(
        &self,
        oel: &Address,
        oracle_keys: &KeyPair,
        event_id: &str,
    ) -> Result<Value> {
        self.submit(
            oel,
            ORACLEEVENTLIST_ABI,
            "deleteEvent",
            json!({ "eventId": normalize_uint256_hex(event_id)? }),
            oracle_keys,
        )
        .await
    }

    pub async fn pmp_order_book_address(&self, pmp: &Address) -> Result<Option<Address>> {
        let Some(v) = self
            .client
            .run_getter_retrying(pmp, PMP_ABI, "getOrderBookAddress", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let raw = v["orderBookAddress"]
            .as_str()
            .or_else(|| v["value0"].as_str());
        raw.map(Address::parse).transpose()
    }

    pub async fn order_book_details(&self, order_book: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(order_book, ORDERBOOK_ABI, "getDetails", json!({}))
            .await
    }

    pub async fn order_book_queue_size(&self, order_book: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(order_book, ORDERBOOK_ABI, "getQueueSize", json!({}))
            .await
    }

    pub async fn order_book_shutdown_state(&self, order_book: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(order_book, ORDERBOOK_ABI, "getShutdownState", json!({}))
            .await
    }

    pub async fn order_book_order(
        &self,
        order_book: &Address,
        order_id: u128,
    ) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(
                order_book,
                ORDERBOOK_ABI,
                "getOrder",
                json!({ "orderId": order_id.to_string() }),
            )
            .await
    }

    pub async fn order_book_orders_by_owner(
        &self,
        order_book: &Address,
        deposit_hash: &str,
    ) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(
                order_book,
                ORDERBOOK_ABI,
                "getOrdersByOwner",
                json!({ "depositHash": normalize_uint256_hex(deposit_hash)? }),
            )
            .await
    }

    pub async fn order_book_version(&self, order_book: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter_retrying(order_book, ORDERBOOK_ABI, "getVersion", json!({}))
            .await
    }

    pub async fn assert_order_book_read_identity(
        &self,
        pmp: &Address,
        order_book: &Address,
    ) -> Result<()> {
        let display_pmp = display_dexdo_address(pmp);
        let display_order_book = display_dexdo_address(order_book);
        let (pmp_code, pmp_details) = self.validated_pmp_read_identity(pmp).await?;
        let bound_order_book = self
            .pmp_order_book_address(pmp)
            .await?
            .ok_or_else(|| anyhow!("PMP {display_pmp} getOrderBookAddress unavailable"))?;
        if bound_order_book != *order_book {
            return Err(anyhow!(
                "OrderBook {display_order_book} is not the book bound to PMP {display_pmp}"
            ));
        }

        let account = self
            .client
            .get_account_retrying(order_book)
            .await?
            .ok_or_else(|| anyhow!("OrderBook {display_order_book} account is not found"))?;
        let (actual_hash, actual_code) = active_account_code("OrderBook", order_book, &account)?;
        let salt = validate_salted_code_from_current_base(
            "OrderBook",
            order_book,
            &actual_hash,
            &actual_code,
            ORDERBOOK_TVC,
        )?;
        let (private_note_code, salted_pmp_hash, salted_pmp_depth) = decode_order_book_salt(salt)?;
        validate_private_note_generation(
            self.private_note_pin()?,
            &format!("OrderBook {display_order_book}"),
            &private_note_code,
        )?;
        if salted_pmp_hash != pmp_code.repr_hash().to_hex_string()
            || salted_pmp_depth != pmp_code.repr_depth()
        {
            return Err(anyhow!(
                "OrderBook {display_order_book} code salt does not bind the live PMP {display_pmp} code"
            ));
        }
        let order_book_details = self
            .order_book_details(order_book)
            .await?
            .ok_or_else(|| anyhow!("OrderBook {display_order_book} getDetails unavailable"))?;
        validate_order_book_market_identity(&pmp_details, &order_book_details)
    }

    pub async fn resolve_oracle_range(
        &self,
        oel: &Address,
        signer: &KeyPair,
        event_id: &str,
        oracle_list_hash: &str,
        token_type: u32,
    ) -> Result<Value> {
        self.submit(
            oel,
            ORACLEEVENTLIST_ABI,
            "resolveRange",
            json!({
                "eventId": normalize_uint256_hex(event_id)?,
                "oracleListHash": normalize_uint256_hex(oracle_list_hash)?,
                "tokenType": token_type,
            }),
            signer,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn provision_oracle_market(
        &self,
        note_keys: &KeyPair,
        note: &Address,
        oracle_keys: &KeyPair,
        oracle_name: &str,
        event_list_index: u128,
        event_list_description: &str,
        event_name: &str,
        oracle_fee: u128,
        deadline: u64,
        describe: &str,
        bounds: &[String],
        outcome_names: &[String],
        market: &MarketManifest,
        token_type: u32,
        initial_stakes: &[u128],
    ) -> Result<OracleMarketManifest> {
        if oracle_name.trim().is_empty() {
            return Err(anyhow!("oracle_name is empty"));
        }
        if initial_stakes.len() != outcome_names.len() {
            return Err(anyhow!(
                "initial_stakes must cover every outcome (got {}, expected {})",
                initial_stakes.len(),
                outcome_names.len()
            ));
        }
        if let Some((i, _)) = initial_stakes
            .iter()
            .enumerate()
            .find(|(_, v)| **v < MIN_PMP_INITIAL_STAKE)
        {
            return Err(anyhow!(
                "initial_stakes[{i}] is below the contract minimum {MIN_PMP_INITIAL_STAKE}"
            ));
        }

        self.assert_seller_note_current(note).await?;
        self.assert_note_owner_matches("oracle provision", note, note_keys)
            .await?;

        let order_book = Address::parse(&market.inference_order_book)?;
        let oracle = self.oracle_address(oracle_name).await?;
        if !self
            .wait_active(&oracle, crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS)
            .await
        {
            self.deploy_oracle(oracle_keys, oracle_name).await?;
            if !self
                .wait_active(&oracle, crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS)
                .await
            {
                return Err(anyhow!(
                    "Oracle {} did not activate",
                    display_dexdo_address(&oracle)
                ));
            }
        }

        let oel = self
            .oracle_event_list_address(&oracle, event_list_index)
            .await?;
        if !self
            .wait_active(&oel, crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS)
            .await
        {
            self.deploy_oracle_event_list(
                &oracle,
                oracle_keys,
                event_list_index,
                event_list_description,
            )
            .await?;
            if !self
                .wait_active(&oel, crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS)
                .await
            {
                return Err(anyhow!(
                    "OracleEventList {} did not activate",
                    display_dexdo_address(&oel)
                ));
            }
        }

        let event_id = match self
            .find_oracle_event_id(&oel, event_name, deadline, describe, outcome_names)
            .await?
        {
            Some(id) => id,
            None => {
                self.add_range_event(
                    &oel,
                    oracle_keys,
                    event_name,
                    oracle_fee,
                    deadline,
                    describe,
                    bounds,
                    outcome_names,
                    &order_book,
                )
                .await?;
                self.wait_oracle_event_id(&oel, event_name, deadline, describe, outcome_names)
                    .await?
            }
        };

        let oracle_names = vec![oracle_name.to_string()];
        let oracle_indexes = vec![event_list_index];
        let oracle_fees = vec![oracle_fee];
        let pmp = self
            .pmp_address(&event_id, &oracle_names, token_type)
            .await?;
        if !self
            .wait_active(&pmp, crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS)
            .await
        {
            self.deploy_pmp(
                note,
                note_keys,
                &event_id,
                &oracle_fees,
                token_type,
                &oracle_names,
                &oracle_indexes,
                initial_stakes,
            )
            .await?;
            if !self
                .wait_active(&pmp, crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS)
                .await
            {
                return Err(anyhow!(
                    "PMP {} did not activate",
                    display_dexdo_address(&pmp)
                ));
            }
        }

        let details = self.wait_pmp_approved(&pmp).await?;
        let oracle_list_hash = value_to_uint256_hex(&details["oracleListHash"])
            .ok_or_else(|| anyhow!("PMP getDetails returned no oracleListHash"))?;
        let range = self
            .oracle_range_data(&oel, &event_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "OracleEventList {} returned no range data",
                    display_dexdo_address(&oel)
                )
            })?;
        if !range["exists"].as_bool().unwrap_or(false) {
            return Err(anyhow!(
                "OracleEventList {} has no range data for event {event_id}",
                display_dexdo_address(&oel)
            ));
        }
        let range_ob = range["ob"].as_str().unwrap_or("");
        if normalize_addr(range_ob)? != normalize_addr(&market.inference_order_book)? {
            return Err(anyhow!(
                "range event OB {} != market inference_order_book {}",
                display_dexdo_address(range_ob),
                display_dexdo_address(&market.inference_order_book)
            ));
        }
        let on_chain_bounds = range_bounds_to_uint256_hex(&range["bounds"]).ok_or_else(|| {
            anyhow!(
                "OracleEventList {} returned invalid bounds for event {event_id}: {range:?}",
                display_dexdo_address(&oel)
            )
        })?;
        let requested_bounds = requested_bounds_to_uint256_hex(bounds)?;
        if on_chain_bounds != requested_bounds {
            return Err(anyhow!(
                "range event bounds {:?} != requested {:?} for event {event_id}",
                on_chain_bounds,
                requested_bounds
            ));
        }

        let manifest = OracleMarketManifest {
            network: self.deployed.network.clone(),
            root_oracle: self.root_oracle_address().await?.with_workchain(),
            oracle: oracle.with_workchain(),
            oracle_event_list: oel.with_workchain(),
            oracle_list_hash,
            event_id,
            event_name: event_name.to_string(),
            pmp: pmp.with_workchain(),
            token_type,
            inference_order_book: market.inference_order_book.clone(),
            frame_model: market.frame_model.clone(),
            deadline,
            bounds: bounds.to_vec(),
            outcome_names: outcome_names.to_vec(),
        };
        manifest
            .validate()
            .map_err(|e| anyhow!("oracle market manifest: {e}"))?;
        Ok(manifest)
    }

    async fn find_oracle_event_id(
        &self,
        oel: &Address,
        event_name: &str,
        deadline: u64,
        describe: &str,
        outcome_names: &[String],
    ) -> Result<Option<String>> {
        let Some(events) = self.oracle_event_list_events(oel).await? else {
            return Ok(None);
        };
        Ok(find_event_id_in_getter_output(
            &events,
            event_name,
            deadline,
            describe,
            outcome_names,
        ))
    }

    async fn wait_oracle_event_id(
        &self,
        oel: &Address,
        event_name: &str,
        deadline: u64,
        describe: &str,
        outcome_names: &[String],
    ) -> Result<String> {
        for i in 0..crate::params::ORACLE_EVENT_ID_MAX_READS {
            if let Some(id) = self
                .find_oracle_event_id(oel, event_name, deadline, describe, outcome_names)
                .await?
            {
                return Ok(id);
            }
            if i + 1 < crate::params::ORACLE_EVENT_ID_MAX_READS {
                tokio::time::sleep(crate::params::ORACLE_EVENT_ID_POLL_INTERVAL).await;
            }
        }
        Err(anyhow!(
            "range event `{event_name}` did not appear in OracleEventList {}",
            display_dexdo_address(oel)
        ))
    }

    async fn wait_pmp_approved(&self, pmp: &Address) -> Result<Value> {
        let display_pmp = display_dexdo_address(pmp);
        for i in 0..crate::params::PMP_APPROVAL_MAX_READS {
            if let Some(details) = self.pmp_details(pmp).await? {
                if details["approved"].as_bool().unwrap_or(false) {
                    return Ok(details);
                }
            }
            if i + 1 < crate::params::PMP_APPROVAL_MAX_READS {
                tokio::time::sleep(crate::params::PMP_APPROVAL_POLL_INTERVAL).await;
            }
        }
        let details = self.pmp_details(pmp).await?;
        Err(anyhow!(
            "PMP {display_pmp} did not become approved by oracle; last getDetails={details:?}"
        ))
    }

    /// fail-closed pre-flight: the seller note must be Active on-chain AND carry the **current**
    /// `PrivateNote` code (the embedded `PRIVATENOTE_TVC` hash). A `pn_pool` minted before a SuperRoot /
    /// PrivateNote redeploy is orphaned -- the note is either gone (a later getter 404s as "note is not
    /// active") or runs stale code whose deploy/registration into the rotated SuperRoot throws a bare
    /// `TVM_ERROR` in the compute phase. Catch both here with an actionable "re-mint your pool" message
    /// instead of letting provision fail opaquely downstream.
    pub async fn assert_seller_note_current(&self, note: &Address) -> Result<()> {
        let account = self.client.get_account_retrying(note).await?;
        seller_note_account_current(self.private_note_pin()?, note, account.as_ref())
    }

    /// Validate the account snapshot read by `dexdo note balance`.
    pub fn assert_note_balance_private_note_account(
        &self,
        note: &Address,
        account: Option<&Account>,
    ) -> Result<()> {
        note_balance_private_note_account(self.private_note_pin()?, note, account)
    }

    /// Fund-safety guard for `note withdraw`. A PrivateNote deployed by a
    /// PREVIOUS contract generation -- its on-chain `code_hash` differs from the current generation's
    /// `private_note` pin -- still accepts the current-generation `withdrawTokens`
    /// message: it ZEROES the note's balance but does NOT credit the destination wallet, so the
    /// SHELL is lost. Refuse the withdraw BEFORE any on-chain write when the note is not the current
    /// generation. This does not recover funds already lost; it prevents zeroing a still-funded
    /// previous-generation note.
    pub async fn assert_note_withdraw_generation(&self, note: &Address) -> Result<()> {
        let acc = self
            .client
            .get_account_retrying(note)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "note {} is not on-chain; cannot withdraw",
                    display_dexdo_address(note)
                )
            })?;
        if !acc.is_active() {
            return Err(anyhow!(
                "note {} is {}, not Active; cannot withdraw",
                display_dexdo_address(note),
                acc.status
            ));
        }
        note_withdraw_generation_ok(self.private_note_pin()?, note, acc.code_hash.as_deref())
    }

    /// read the note's on-chain owner key (`getDetails().ephemeralPubkey`) and fail closed if it does not
    /// match the key the client will sign the owner-authenticated write with -- turning the opaque pre-accept
    /// `onlyOwnerPubkey` revert (branch 3: a non-conforming/orphaned note) into an actionable error. The buyer's
    /// `place_buy` calls it before `placeInferenceBuy`; the seller's `post_offer` before `postSellOffer`. An
    /// absent/empty `getDetails` (uninit/orphaned note) is itself a fail-closed re-mint case.
    pub async fn assert_note_owner_matches(
        &self,
        role: &str,
        note: &Address,
        signing_keys: &KeyPair,
    ) -> Result<()> {
        let details = self
            .client
            .run_getter_retrying(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "{role} aborted: note {} returned no getDetails (not on-chain/active) -- the pn_pool is \
                     likely orphaned by a contract redeploy. Re-mint against the current contracts \
                     (`mint_pn_pool`) and point DEXDO_PN_POOL at the fresh pool.",
                    display_dexdo_address(note)
                )
            })?;
        match note_owner_mismatch_reason(
            role,
            note,
            details["ephemeralPubkey"].as_str(),
            signing_keys.public_hex(),
        ) {
            Some(reason) => Err(anyhow!(reason)),
            None => Ok(()),
        }
    }

    /// Poll `get_account(addr).is_active()` up to `tries` times (3s apart; `tries=1` = a single check).
    /// A query error or a not-yet-existent account (e.g. a self-dapp uninit address that 404s) counts
    /// as "not active" -- the caller then deploys or fails with a clear message.
    async fn wait_active(&self, addr: &Address, tries: u32) -> bool {
        for i in 0..tries {
            if let Ok(Some(a)) = self.client.get_account_retrying(addr).await {
                if a.is_active() {
                    return true;
                }
            }
            if i + 1 < tries {
                tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
            }
        }
        false
    }
}

fn withdraw_note_tokens_payload(dest_wallet: &Address, dapp_id: &str) -> Value {
    json!({
        "destWalletAddr": dest_wallet.with_workchain(),
        "dapp_id": dapp_id,
    })
}

fn init_note_transfer_payload(
    dest_deposit_hash: &str,
    token_type: u32,
    amount: u128,
    ecc_amount: u128,
) -> Value {
    json!({
        "destDepositHash": dest_deposit_hash,
        "tokenType": token_type,
        "amount": amount.to_string(),
        "eccAmount": ecc_amount.to_string(),
    })
}

/// A refusal the note-to-note transfer would meet, recognised from public `getDetails()` reads
/// before anything is signed.

/// Every variant mirrors a `require` in `contracts/dex/PrivateNote.sol` and carries that
/// requirement's error constant, because the point of naming them here is that the operator is told
/// the same thing the contract would have told them -- only for free. `initTransfer` runs
/// `tvm.accept()` before all of these (`onlyOwnerPubkey... accept`, PrivateNote.sol), so meeting
/// one on chain is not a cheap bounce: it spends the sending note's gas to be refused.

/// This list is what `getDetails()` can see, and it is deliberately not claimed to be all of them.
/// `_openOrderCount`, `_restingInf`, `_pendingInf` and `_liveDeals` are not in the getter, so a
/// sender carrying leftover inference state passes every check here and is still refused on chain
/// with `ERR_OPEN_ORDERS_EXIST` -- which is why that code is given its own explanation in
/// [`note_transfer_submit_hint`] rather than left to arrive as a bare number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteTransferRefusal {
    /// `initTransfer` requires `!_hasWithdrawn` (`ERR_INVALID_STATE`).
    SenderWithdrawn,
    /// `offerTransfer` requires `!_hasWithdrawn` on the RECEIVING note (`ERR_INVALID_STATE`). This
    /// is the destination's only state gate, and it is the one that would silently cost money: the
    /// sender debits its record, the destination refuses, and the value comes home only through the
    /// bounce path.
    DestWithdrawn,
    /// `initTransfer` requires `!_busy.hasValue()` (`ERR_NOTE_BUSY`).
    SenderBusy { with: String },
    /// `initTransfer` requires `_couponsValue == 0` (`ERR_COUPON_ACTIVE`).
    SenderCouponActive { value: u128 },
    /// `initTransfer` requires every `_lockedInOrders` entry to be zero (`ERR_NON_ZERO_BALANCE`).
    SenderLockedInOrders { token_type: u32, locked: u128 },
    /// `initTransfer` requires `_balance[tokenType] >= amount` (`ERR_LOW_VALUE`).
    SenderRecordShort { have: u128, want: u128 },
    /// `initTransfer` requires `amount >= minStakeValue(tokenType)` (`ERR_LOW_VALUE`).
    AmountBelowMinimum { amount: u128, minimum: u128 },
    /// `initTransfer` requires `destDepositHash != _depositIdentifierHash` (`ERR_INVALID_PARAMS`).
    SelfTransfer { note: String },
}

impl std::fmt::Display for NoteTransferRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SenderWithdrawn => f.write_str(
                "the sending note has already been withdrawn (`withdrawTokens` is one-shot), so \
                 `initTransfer` would revert dex::ERR_INVALID_STATE (151)",
            ),
            Self::DestWithdrawn => f.write_str(
                "the destination note has already been withdrawn, so it would refuse the incoming \
                 transfer with dex::ERR_INVALID_STATE (151) -- the sending note debits its record \
                 first and only gets it back through the bounce path",
            ),
            Self::SenderBusy { with } => write!(
                f,
                "the sending note is busy with {with}, so `initTransfer` would revert \
                 dex::ERR_NOTE_BUSY (121). `_busy` is a latch cleared only by the acknowledgement \
                 of whatever set it (or by that message bouncing); it is not a wait-and-retry state \
                 and may not clear on its own"
            ),
            Self::SenderCouponActive { value } => write!(
                f,
                "the sending note holds {value} raw in active coupons, so `initTransfer` would \
                 revert dex::ERR_COUPON_ACTIVE (149)"
            ),
            Self::SenderLockedInOrders { token_type, locked } => write!(
                f,
                "the sending note still has {locked} raw of token type {token_type} locked in \
                 orders, so `initTransfer` would revert dex::ERR_NON_ZERO_BALANCE (144)"
            ),
            Self::SenderRecordShort { have, want } => write!(
                f,
                "the sending note's spendable trading record is {have} raw and the transfer needs \
                 {want} raw, so `initTransfer` would revert dex::ERR_LOW_VALUE (102). This is the \
                 note's `_balance`, not its ECC[2] pocket -- `dexdo note topup` cannot raise it"
            ),
            Self::AmountBelowMinimum { amount, minimum } => write!(
                f,
                "{amount} raw is below the contract's minimum transfer of {minimum} raw \
                 (`minStakeValue`), so `initTransfer` would revert dex::ERR_LOW_VALUE (102)"
            ),
            Self::SelfTransfer { note } => write!(
                f,
                "source and destination are both {}; `initTransfer` refuses a transfer to the \
                 sending note's own deposit hash with dex::ERR_INVALID_PARAMS (129)",
                display_dexdo_address(note)
            ),
        }
    }
}

/// The destination's `depositIdentifierHash` as `initTransfer` wants it, read off the destination
/// note's own `getDetails()`.
pub fn note_transfer_deposit_identifier_hash(details: &Value) -> Result<String> {
    let raw = field(details, "depositIdentifierHash", "deposit_identifier_hash");
    match raw {
        Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(anyhow!(
            "PrivateNote.getDetails() carries no usable depositIdentifierHash; refusing to guess \
             the destination the contract would derive from it"
        )),
    }
}

/// The `initTransfer` refusals visible in the SENDING note's `getDetails()`.
pub fn note_transfer_sender_refusal(details: &Value) -> Option<NoteTransferRefusal> {
    if details_has_withdrawn(details).unwrap_or(false) {
        return Some(NoteTransferRefusal::SenderWithdrawn);
    }
    if let Some(with) = busy_with(details) {
        return Some(NoteTransferRefusal::SenderBusy { with });
    }
    let coupons = value_u128(field(details, "couponsValue", "coupons_value")).unwrap_or_default();
    if coupons > 0 {
        return Some(NoteTransferRefusal::SenderCouponActive { value: coupons });
    }
    currency_map_entries(field(details, "lockedInOrders", "locked_in_orders"))
        .into_iter()
        .find(|(_, locked)| *locked > 0)
        .map(
            |(token_type, locked)| NoteTransferRefusal::SenderLockedInOrders { token_type, locked },
        )
}

/// The `offerTransfer` refusal visible in the RECEIVING note's `getDetails()`. The receiving side
/// has exactly one state gate, so this is the whole of it.
pub fn note_transfer_dest_refusal(details: &Value) -> Option<NoteTransferRefusal> {
    details_has_withdrawn(details)
        .unwrap_or(false)
        .then_some(NoteTransferRefusal::DestWithdrawn)
}

/// The two amount refusals, which depend on the figure rather than on either note's state.
pub fn note_transfer_amount_refusal(
    sender_record: u128,
    amount: u128,
    minimum: u128,
) -> Option<NoteTransferRefusal> {
    if amount < minimum {
        return Some(NoteTransferRefusal::AmountBelowMinimum { amount, minimum });
    }
    (sender_record < amount).then_some(NoteTransferRefusal::SenderRecordShort {
        have: sender_record,
        want: amount,
    })
}

/// What the two transfer-specific on-chain refusals actually mean, keyed on the exit code an
/// `initTransfer` submit came back with.

/// Both already arrive with their constant's name attached, because every exit code goes through
/// `contract_error_label`. A name is not yet an answer, though: `ERR_OPEN_ORDERS_EXIST` on a note
/// the operator believes is idle reads as a contradiction unless someone says which state it means,
/// and `ERR_NOTE_BUSY` reads as "try again shortly" when it is a latch that may never clear on its
/// own. Neither is preflightable -- `getDetails()` does not expose `_openOrderCount`,
/// `_restingInf`, `_pendingInf` or `_liveDeals` -- so the explanation has to be attached here, on
/// the way out, rather than raised before the send.
pub fn note_transfer_submit_hint(error_text: &str) -> Option<&'static str> {
    // The exact fragment `exit_code_fragment` writes, so this keys on the code rather than on the
    // wording of the sentence around it.
    if error_text.contains("exit_code=167 (") {
        return Some(
            "dex::ERR_OPEN_ORDERS_EXIST (167): the SENDING note still carries inference state -- an \
             open order, a resting or pending inference, or a live deal. None of those are in \
             `getDetails()`, so this could not be refused before the send. A note in this state can \
             still RECEIVE a transfer; it cannot send one. Settle or cancel what the note is \
             holding (`dexdo status`, then cancel/stop the deals it names) and re-run the same \
             `--to`, which submits nothing if the destination has meanwhile reached the level.",
        );
    }
    if error_text.contains("exit_code=121 (") {
        return Some(
            "dex::ERR_NOTE_BUSY (121): the SENDING note's `_busy` latch is set. It is cleared only \
             by the acknowledgement of the operation that set it, or by that operation's message \
             bouncing -- so this is not a wait-and-retry state and it may not clear on its own. \
             Check what the note is busy with (`dexdo note balance` on the SENDING note reports \
             `busyAddress`) before re-running; a latch left by a lost acknowledgement needs that \
             counterparty resolved, not another transfer.",
        );
    }
    None
}

/// `_busy` as `getDetails()` renders it: `optional(address)`, so absent, null, or the address.
fn busy_with(details: &Value) -> Option<String> {
    match field(details, "busyAddress", "busy_address") {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Both shapes a `map(uint32,uint128)` getter output arrives in, as `(currency, value)` pairs.
/// Shared with nothing on purpose: `private_note_balance_currency` answers about ONE currency and
/// this needs every entry, since `initTransfer` requires them all to be zero.
fn currency_map_entries(value: &Value) -> Vec<(u32, u128)> {
    if let Some(entries) = value.as_object() {
        return entries
            .iter()
            .filter_map(|(id, raw)| Some((id.parse::<u32>().ok()?, value_u128(raw)?)))
            .collect();
    }
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry
                .get("currency")
                .or_else(|| entry.get("id"))
                .and_then(value_u128)?;
            let amount = value_u128(entry.get("value").or_else(|| entry.get("amount"))?)?;
            Some((u32::try_from(id).ok()?, amount))
        })
        .collect()
}

/// One shape for every `PrivateNote.placeInferenceBuy` payload -- ordinary buys and subscriptions alike,
/// since they are the same on-chain call and differ only in `flags`. Keeping the encoding in one place is
/// what stops the plain and subscription paths from drifting into two argument orders.
fn place_inference_buy_payload(
    model_hash: &str,
    max_price_per_tick: u128,
    ticks: u128,
    escrow: u128,
    flags: u8,
    deadline: u64,
) -> Value {
    json!({
        "modelHash": model_hash,
        "maxPricePerTick": max_price_per_tick.to_string(),
        "ticks": ticks.to_string(),
        "escrow": escrow.to_string(),
        "flags": flags,
        "deadline": deadline.to_string(),
    })
}

#[cfg(test)]
mod tests {
    /// regression 1: a resting order is NAMED and PROVED.

    /// The pair comes out of the book's inbound `onInferencePlaced`, which is the only record that
    /// carries `modelHash`; the proof is that the key composed from it is one the note publishes
    /// right now. Both halves have to hold -- history alone says what happened, the getter alone
    /// says what is true but not under what name.
    mod issue_1522_resting_orders_are_named_and_proved {
        use super::super::{resolve_resting_inference_orders, RealChainBackend};
        use crate::chain::note_events::{resting_inference_order_key, InferenceOrderCall};

        const MODEL_HASH: &str =
            "0x0000000000000000000000000000000000000000000000000000000000000abc";

        fn call(order_id: u128) -> InferenceOrderCall {
            InferenceOrderCall {
                model_hash: MODEL_HASH.to_string(),
                order_id,
            }
        }

        fn key_for(order_id: u128) -> String {
            let book = RealChainBackend::canonical_inference_orderbook_address(MODEL_HASH)
                .expect("book derives from the model hash")
                .with_workchain();
            resting_inference_order_key(&book, order_id).expect("key composes")
        }

        #[test]
        fn a_placed_order_still_in_getoutstanding_is_recovered_with_its_model_hash() {
            let keys = vec![key_for(7)];
            let (resting, unexplained) =
                resolve_resting_inference_orders(&[call(7)], &[], &keys).expect("resolve");

            assert_eq!(resting.len(), 1, "the placed order is recovered");
            assert_eq!(resting[0].model_hash, MODEL_HASH);
            assert_eq!(resting[0].order_id, 7);
            assert_eq!(
                resting[0].key, keys[0],
                "the recovered order is the one the note publishes, proved by its key"
            );
            assert!(
                unexplained.is_empty(),
                "every published key was explained: {unexplained:?}"
            );
        }

        /// A pair history offers that the note no longer publishes is NOT offered to the owner. The
        /// getter is what is true now; history is only how the name was learned.
        #[test]
        fn a_pair_absent_from_getoutstanding_is_not_offered() {
            let (resting, unexplained) =
                resolve_resting_inference_orders(&[call(7)], &[], &[]).expect("resolve");
            assert!(resting.is_empty(), "nothing rests, so nothing is offered");
            assert!(unexplained.is_empty());
        }

        /// regression 2: a removed order never reaches the list.

        /// Measured shape, from a chain note `0:29f4223b...4e`: five placements and five removals.
        /// Without subtracting the removals the owner is handed five orders that do not exist and
        /// pays gas cancelling each.
        #[test]
        fn removed_orders_do_not_appear_as_resting() {
            let placed: Vec<_> = (1..=5).map(call).collect();
            let removed: Vec<_> = (1..=5).map(call).collect();
            let (resting, unexplained) =
                resolve_resting_inference_orders(&placed, &removed, &[]).expect("resolve");

            assert!(
                resting.is_empty(),
                "five placed and five removed leaves nothing resting: {resting:?}"
            );
            assert!(unexplained.is_empty());
        }

        /// A key the walk could not explain is reported, never dropped. That key is an order that IS
        /// resting -- money -- under a name this run failed to recover, and silence would read as an
        /// all-clear.
        #[test]
        fn an_unexplained_key_is_reported_rather_than_dropped() {
            let stranger =
                "0x00000000000000000000000000000000000000000000000000000000deadbeef".to_string();
            let keys = vec![key_for(7), stranger.clone()];
            let (resting, unexplained) =
                resolve_resting_inference_orders(&[call(7)], &[], &keys).expect("resolve");

            assert_eq!(resting.len(), 1);
            assert_eq!(
                unexplained,
                vec![stranger],
                "the key nothing explained must survive into the report"
            );
        }
    }

    /// `Deployed::load` maps the file's fields onto the struct's, for every committed manifest.

    /// This used to name one manifest and assert that chain's host, DApp id and roots as literals.
    /// As a check that was a copy of the file it read -- it says nothing that reading the file twice
    /// would not -- and it went red on a legitimate redeployment. What it is actually for is that
    /// the LOADER does not drop or transpose a field, and that holds against the file's own JSON,
    /// whichever chain it describes.
    #[test]
    fn the_loader_maps_every_committed_manifest_onto_its_own_fields() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../manifest");
        let mut checked = 0;

        for entry in std::fs::read_dir(&dir).expect("read the committed manifest directory") {
            let path = entry.expect("read a manifest directory entry").path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".manifest.json") {
                continue;
            }
            let raw: Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read a committed manifest"))
                    .expect("a committed manifest is JSON");
            let manifest = Deployed::load(&path).unwrap_or_else(|error| panic!("{name}: {error}"));

            assert_eq!(
                Some(manifest.network.as_str()),
                raw["network"].as_str(),
                "{name}: network"
            );
            assert_eq!(
                Some(manifest.dapp_id.as_str()),
                raw["dapp_id"].as_str(),
                "{name}: dapp_id"
            );
            assert_eq!(
                Some(manifest.superroot.as_str()),
                raw["superroot"].as_str(),
                "{name}: superroot"
            );
            assert_eq!(
                resolve_endpoint(None, &manifest)
                    .unwrap_or_else(|error| panic!("{name}: {error:#}")),
                raw["endpoint"]
                    .as_str()
                    .expect("a committed manifest declares its endpoint"),
                "{name}: endpoint"
            );
            checked += 1;
        }

        assert!(
            checked >= 1,
            "no committed manifest was found in {}",
            dir.display()
        );
    }

    /// Every committed manifest is internally consistent, whichever chain it names.

    /// This replaced a test that loaded ONE manifest by name and asserted that chain's addresses as
    /// literals. Two problems with that. It named a network in the client's own tests, which is the
    /// coupling this change exists to remove; and as a check it was a change-detector on a data file
    /// that humans edit on purpose -- it went red on the day of a legitimate deployment and told
    /// nobody anything, exactly as its neighbour above describes.

    /// What holds on every day, for every manifest, present and future, is the shape: the file's
    /// name and its `network` field must agree, it must pin its own generation, and every address
    /// it carries must be an address rather than a placeholder somebody left behind.
    #[test]
    fn every_committed_manifest_agrees_with_its_own_name_and_pins() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../manifest");
        let mut checked = 0;

        for entry in std::fs::read_dir(&dir).expect("read the committed manifest directory") {
            let path = entry.expect("read a manifest directory entry").path();
            let Some(stem) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".manifest.json"))
            else {
                continue;
            };
            let manifest =
                Deployed::load(&path).unwrap_or_else(|error| panic!("load {stem}: {error}"));

            assert_eq!(
                manifest.network, stem,
                "{stem}.manifest.json declares network `{}`; a file whose name and contents \
                 disagree is read as one chain and dialled as another",
                manifest.network
            );
            assert_eq!(
                manifest.version.as_deref(),
                Some(
                    super::super::contracts_provision::generation_pins(
                        manifest.version.as_deref().unwrap_or_default(),
                    )
                    .unwrap_or_else(|| {
                        panic!("{stem} declares a generation this build has not measured")
                    })
                    .version
                ),
                "{stem}: the manifest and the pin row for its generation must agree"
            );
            resolve_endpoint(None, &manifest)
                .unwrap_or_else(|error| panic!("{stem} must say where to dial: {error:#}"));

            let raw: Value = serde_json::from_slice(
                &std::fs::read(&path).unwrap_or_else(|error| panic!("read {stem}: {error}")),
            )
            .unwrap_or_else(|error| panic!("parse {stem} as JSON: {error}"));
            for field in ["superroot", "model_registry", "rootpn", "rootoracle"] {
                let value = raw[field]
                    .as_str()
                    .unwrap_or_else(|| panic!("{stem}.{field} is missing or not a string"));
                let hex = value
                    .strip_prefix("0:")
                    .unwrap_or_else(|| panic!("{stem}.{field} is not an address: {value:?}"));
                assert!(
                    hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()),
                    "{stem}.{field} is not a 64-hex address: {value:?}"
                );
            }
            checked += 1;
        }

        assert!(
            checked >= 1,
            "no committed manifest was found in {}",
            dir.display()
        );
    }

    #[test]
    fn committed_manifest_missing_required_field_refuses_before_connection() {
        use std::cell::Cell;

        // Through the pointer, not by name: this test only needs a WELL-FORMED committed manifest
        // to break, and naming one here is the second answer to "which manifest do tests read"
        // that `manifest/for-tests` exists to prevent -- mine, left behind by my own sweep.
        let path = crate::params::committed_manifest_for_tests();
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read the committed manifest"))
                .expect("parse the committed manifest as JSON");
        manifest
            .as_object_mut()
            .expect("deployment manifest is an object")
            .remove("superroot");
        let root = tempfile::tempdir().expect("manifest fixture directory");
        let incomplete = root.path().join("deployed.missing-superroot.json");
        std::fs::write(
            &incomplete,
            serde_json::to_vec(&manifest).expect("serialize incomplete manifest"),
        )
        .expect("write incomplete manifest");
        let connector_called = Cell::new(false);

        let error = super::connect_client_from_manifest_with(&incomplete, None, |_, _| {
            connector_called.set(true);
            Ok(())
        })
        .expect_err("a manifest without superroot must be refused");

        assert!(!connector_called.get(), "no connection attempt is allowed");
        assert!(format!("{error:#}").contains("superroot"), "{error:#}");
    }

    /// The profile does not depend on which chain the manifest names.

    /// These three tests used to assert the opposite: that one label produced the SDK's first
    /// preset byte-for-byte, another produced the second, and any third label was refused outright.
    /// That is a list of chains this binary consents to work on, and a manifest naming a chain
    /// deployed after the binary was built was rejected for no reason except its own age.

    /// The presets differ ONLY in the giver fields -- the second is literally the first with the
    /// giver set to `None` -- and the giver is reached from the environment now, under `DEV`, not
    /// from a preset. So there is nothing left for a label to select.
    #[test]
    fn the_profile_is_the_same_whichever_chain_the_manifest_names() {
        let profile = |label: &str| {
            let mut manifest = deployed("");
            manifest.network = label.to_string();
            serde_json::to_value(
                super::ai_registry_config_from_manifest(&manifest)
                    .unwrap_or_else(|error| panic!("`{label}` must be supported: {error:#}")),
            )
            .expect("serialize the manifest-selected config")
        };

        // Two labels this binary has heard of, and one deployed after it was built.
        assert_eq!(profile("net-a"), profile("net-b"));
        assert_eq!(
            profile("net-a"),
            profile("a-chain-that-did-not-exist-at-build-time")
        );
    }

    #[test]
    fn the_profile_carries_no_giver_of_its_own() {
        // The giver used to arrive inside a preset, which is how a test chain's funding key rode
        // into a binary that had no business holding one. It comes from the environment now, so the
        // profile itself must carry none, on every chain.
        let config = super::ai_registry_config_from_manifest(&deployed("")).expect("build profile");
        assert!(config.giver_address.is_none());
        assert!(config.giver_pubkey.is_none());
        assert!(config.giver_secret.is_none());
    }

    #[test]
    fn a_manifest_naming_no_chain_at_all_is_refused() {
        let mut manifest = deployed("");
        manifest.network = "   ".to_string();

        let error = super::ai_registry_config_from_manifest(&manifest)
            .expect_err("a manifest that names no chain must be refused");
        assert!(
            error.to_string().contains("`network` field is empty"),
            "{error:#}"
        );
    }

    #[test]
    fn missing_manifest_network_refuses_before_the_connector_is_called() {
        use std::cell::Cell;

        let root = tempfile::tempdir().expect("manifest fixture directory");
        let manifest = root.path().join("deployed.missing-network.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(&serde_json::json!({
                "superroot": format!("0:{}", "0".repeat(64)),
                "dapp_config": "",
                "dapp_id": "0".repeat(64)
            }))
            .expect("serialize incomplete manifest"),
        )
        .expect("write incomplete manifest");
        let connector_called = Cell::new(false);

        let error = super::connect_client_from_manifest_with(&manifest, None, |_, _| {
            connector_called.set(true);
            Ok(())
        })
        .expect_err("a manifest without network must be refused");

        assert!(!connector_called.get(), "no connection attempt is allowed");
        assert!(format!("{error:#}").contains("network"), "{error:#}");
    }

    /// A read that got no answer is repeated, and the answer that follows is the result.

    /// Drives the retry the way callers reach it -- through the wrapper, on a call that fails the
    /// way the endpoint fails -- rather than asserting on the classifier alone: the defect this
    /// exists for was a path that never entered the wrapper at all.
    #[tokio::test]
    async fn transient_read_retries_and_returns_the_answer() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);

        let value = super::retry_transient_read(|| async {
            let seen = calls.fetch_add(1, Ordering::SeqCst);
            if seen < 2 {
                Err(anyhow::Error::new(connect_failure().await))
            } else {
                Ok(42u32)
            }
        })
        .await
        .expect("the third attempt answers");

        assert_eq!(value, 42);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two failures then the answer: the read must be repeated, not abandoned"
        );
    }

    /// The retry stops. A read that never answers costs a bounded number of attempts and returns
    /// the failure -- it does not spin, and it does not hide what happened.
    #[tokio::test]
    async fn transient_read_gives_up_after_the_bounded_attempts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);

        let outcome: Result<u32> = super::retry_transient_read(|| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::Error::new(connect_failure().await))
        })
        .await;

        assert!(outcome.is_err(), "a read that never answers must fail");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            crate::params::TRANSIENT_READ_ATTEMPTS,
            "exactly the configured attempts, no more"
        );
    }

    /// An answer is an answer. A refusal the node actually spoke is returned at once, so a wrong
    /// key or a rejected argument does not cost five round trips before the operator sees it.
    #[tokio::test]
    async fn an_answered_refusal_is_not_repeated() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);

        let outcome: Result<u32> = super::retry_transient_read(|| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("contract refused: insufficient funds"))
        })
        .await;

        assert!(outcome.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the node answered; repeating it only delays the refusal"
        );
    }

    #[tokio::test]
    async fn issue_1185_cloudflare_signature_ban_is_permanent_and_actionable() {
        for response in [
            http::Response::builder()
                .status(reqwest::StatusCode::FORBIDDEN)
                .header("cf-ray", "a290bd7b-ATH")
                .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=UTF-8")
                .body(b"error code: 1010".to_vec())
                .expect("build Cloudflare edge 1010 response")
                .into(),
            http::Response::builder()
                .status(reqwest::StatusCode::FORBIDDEN)
                .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=UTF-8")
                .body(b"error code: 1010".to_vec())
                .expect("build Cloudflare 1010 response")
                .into(),
        ] {
            let error = super::chain_response_for_status(response)
                .await
                .expect_err("HTTP 403 must fail");
            assert!(
                !super::is_transient_transport_failure(&error),
                "a client-signature ban cannot clear on retry: {error:#}"
            );
            let message = format!("{error:#}");
            assert!(
                message.contains("client's HTTP signature is banned"),
                "{message}"
            );
            assert!(message.contains("Cloudflare edge"), "{message}");
            assert!(message.contains("different HTTP client"), "{message}");
            assert!(!message.contains("rate limit"), "{message}");
        }
    }

    #[tokio::test]
    async fn issue_1185_other_forbidden_is_transient() {
        let response: reqwest::Response = http::Response::builder()
            .status(reqwest::StatusCode::FORBIDDEN)
            .header("cf-ray", "a290bd7b-ATH")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(br#"{"error":"try later"}"#.to_vec())
            .expect("build ordinary forbidden response")
            .into();
        let error = super::chain_response_for_status(response)
            .await
            .expect_err("HTTP 403 must fail");

        assert!(
            super::is_transient_transport_failure(&error),
            "an unproven non-1010 HTTP 403 retains the existing retry policy: {error:#}"
        );
    }

    #[test]
    fn issue_1185_single_predicate_classifies_all_transient_shapes() {
        for (label, facts) in [
            (
                "connect",
                super::ReadFailureFacts {
                    connect: true,
                    ..Default::default()
                },
            ),
            (
                "timeout",
                super::ReadFailureFacts {
                    timeout: true,
                    ..Default::default()
                },
            ),
            (
                "body",
                super::ReadFailureFacts {
                    body: true,
                    ..Default::default()
                },
            ),
            (
                "decode",
                super::ReadFailureFacts {
                    decode: true,
                    ..Default::default()
                },
            ),
            (
                "429",
                super::ReadFailureFacts::status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            ),
            (
                "500",
                super::ReadFailureFacts::status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            ),
            (
                "502",
                super::ReadFailureFacts::status(reqwest::StatusCode::BAD_GATEWAY),
            ),
        ] {
            assert!(
                super::read_failure_is_transient(facts),
                "{label} must be transient through the unified predicate"
            );
        }
    }

    /// Every shape of "no answer" is classified as one, including the two the first version missed:
    /// a 5xx, and a body that died mid-transfer.
    #[test]
    fn no_answer_is_classified_by_shape_not_by_luck() {
        assert!(super::is_retryable_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(super::is_retryable_status(reqwest::StatusCode::FORBIDDEN));
        assert!(super::is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(super::is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(super::is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));

        assert!(!super::is_retryable_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!super::is_retryable_status(
            reqwest::StatusCode::BAD_REQUEST
        ));
        assert!(!super::is_retryable_status(
            reqwest::StatusCode::UNAUTHORIZED
        ));
    }

    /// A `Retry-After` the server stated is honoured; one that asks for longer than a command may
    /// wait is not, and the read fails instead of sleeping past its purpose.
    #[test]
    fn retry_after_is_honoured_within_its_bound() {
        let short = anyhow::Error::new(super::RetryAfter { seconds: 3 });
        assert_eq!(
            super::retry_after_delay(&short),
            Some(std::time::Duration::from_secs(3))
        );

        let too_long = anyhow::Error::new(super::RetryAfter {
            seconds: crate::params::TRANSIENT_READ_MAX_RETRY_AFTER.as_secs() + 1,
        });
        assert_eq!(super::retry_after_delay(&too_long), None);
    }

    /// A `reqwest` connect failure, built the only way the crate allows: by making a request that
    /// cannot connect. Used by the retry tests above as a stand-in for a dropped connection.
    async fn connect_failure() -> reqwest::Error {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(1))
            .build()
            .expect("client")
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("connecting to a closed port cannot succeed")
    }

    use super::*;
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn model_registry_account_boc(names: &[String], abi: &str) -> String {
        use tvm_block::Serializable as _;

        let mut models = serde_json::Map::new();
        for name in names {
            assert!(
                models
                    .insert(crate::model_hash_for(name), Value::String(name.clone()))
                    .is_none(),
                "fixture model hashes must be unique"
            );
        }
        let storage = json!({
            "_pubkey": "0x0",
            "_timestamp": "0",
            "_constructorFlag": true,
            "_models": models,
            "_count": names.len().to_string(),
            "_ownerPubkey": "0x0"
        });
        let contract = tvm_abi::Contract::load(abi.as_bytes()).expect("load ModelRegistry ABI");
        let tokens = tvm_abi::token::Tokenizer::tokenize_all_params(contract.fields(), &storage)
            .expect("tokenize ModelRegistry storage fixture");
        let data = tvm_abi::token::TokenValue::pack_values_into_chain(
            &tokens,
            Vec::new(),
            contract.version(),
        )
        .expect("encode ModelRegistry storage fixture")
        .into_cell()
        .expect("build ModelRegistry data cell");
        let mut state_init = tvm_block::StateInit::default();
        state_init.set_data(data);
        let account_storage = tvm_block::AccountStorage::active_by_init_code_hash(
            0,
            tvm_block::CurrencyCollection::default(),
            state_init,
            false,
        );
        let address =
            tvm_block::MsgAddressInt::with_standart(None, 0, tvm_types::AccountId::from([0u8; 32]))
                .expect("fixture address");
        let mut account = tvm_block::Account::with_storage(
            &address,
            &tvm_block::StorageInfo::default(),
            &account_storage,
        );
        account.update_storage_stat().expect("fixture storage stat");
        let boc = tvm_types::write_boc(&account.serialize().expect("serialize fixture account"))
            .expect("write fixture account BOC");
        base64::engine::general_purpose::STANDARD.encode(boc)
    }

    #[test]
    fn model_registry_account_storage_decoder_reads_models_map_fixture() {
        let abi = include_str!("../../../../contracts/compiled/airegistry/ModelRegistry.abi.json");
        let account_boc =
            model_registry_account_boc(&["alpha-model".to_string(), "beta-model".to_string()], abi);

        let decoded = super::account_storage_fields(&account_boc, abi, "ModelRegistry")
            .expect("decode ModelRegistry account fixture");

        let mut names = decoded["_models"]
            .as_object()
            .expect("decoded _models map")
            .values()
            .map(|value| value.as_str().expect("decoded model name"))
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["alpha-model", "beta-model"]);
        assert_eq!(decoded["_count"], "2");
    }

    /// The `(name, type)` input list a compiled ABI declares for `function`, or `None` when the
    /// bundle does not declare it at all. Reads the ABI the crate actually embeds, never a copy.
    fn compiled_abi_inputs(abi_json: &str, function: &str) -> Option<Vec<(String, String)>> {
        let abi: Value = serde_json::from_str(abi_json).expect("parse compiled ABI");
        abi["functions"]
            .as_array()
            .expect("compiled ABI functions[]")
            .iter()
            .find(|f| f["name"] == function)
            .map(|f| {
                f["inputs"]
                    .as_array()
                    .expect("compiled ABI inputs[]")
                    .iter()
                    .map(|i| {
                        (
                            i["name"].as_str().unwrap_or_default().to_string(),
                            i["type"].as_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect()
            })
    }

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, t)| ((*n).to_string(), (*t).to_string()))
            .collect()
    }

    /// Shape pin for the seller-bond funding door, decoded against the ABI this crate itself embeds
    /// (`PRIVATENOTE_ABI` / `TOKENCONTRACT_ABI`, `include_str!` of `contracts/compiled/`) -- never
    /// against a hand-written or hand-copied signature, because a frozen copy of a proposed ABI
    /// passes every offline gate while being dead against the deployed contract.

    /// Contracts 4.0.33 removed `PrivateNote.postSellerBond(nonce, amount)` in favour of
    /// `PrivateNote.fundDeal(nonce, gasShell, amount)`, and renamed `TokenContract.fundSellerBond()`
    /// to `TokenContract.fundDeal(amount)`. Contracts 4.0.35 then added `endpointCipher
    /// optional(bytes)` to BOTH halves, which is a new `functionId` rather than an added field, so
    /// the previous shape does not encode at all. The client is adapted here; the compiled artifacts
    /// are vendored by the contracts merge, so for one window the two may be a generation apart. The
    /// invariant is therefore stated over both states, and it is fail-closed in both:

    /// * artifacts at 4.0.33 or later -- `fundDeal` must be declared with exactly the argument names
    /// and types this client sends, on both contracts, and the superseded entry points must be
    /// gone;
    /// * artifacts still pre-4.0.33 -- the superseded entry points must be the only ones declared,
    /// and the client must NOT be encoding them. Putting the client back on
    /// `postSellerBond`/`fundSellerBond` turns this red in either state, which is the regression.
    #[test]
    fn seller_bond_encodes_the_funding_door_the_compiled_abi_declares() {
        let sent_note_keys: Vec<String> = note_fund_deal_params(7, 0, 2_000)
            .as_object()
            .expect("note fundDeal params object")
            .keys()
            .cloned()
            .collect();
        let sent_deal_keys: Vec<String> = deal_fund_deal_params(2_000)
            .as_object()
            .expect("deal fundDeal params object")
            .keys()
            .cloned()
            .collect();

        let note_fund = compiled_abi_inputs(PRIVATENOTE_ABI, NOTE_FUND_DEAL_METHOD);
        let note_legacy = compiled_abi_inputs(PRIVATENOTE_ABI, "postSellerBond");
        let deal_fund = compiled_abi_inputs(TOKENCONTRACT_ABI, DEAL_FUND_DEAL_METHOD);
        let deal_legacy = compiled_abi_inputs(TOKENCONTRACT_ABI, "fundSellerBond");

        match (note_fund, note_legacy) {
            (Some(fund), legacy) => {
                assert!(
                    legacy.is_none(),
                    "4.0.33 removed PrivateNote.postSellerBond when it added fundDeal; a bundle \
                     declaring both is not a generation this client can encode for"
                );
                assert_eq!(
                    fund,
                    owned(&[
                        ("nonce", "uint64"),
                        ("gasShell", "uint128"),
                        ("amount", "uint128"),
                        ("endpointCipher", "optional(bytes)"),
                    ]),
                    "PrivateNote.fundDeal takes (nonce, gasShell, amount, endpointCipher): gas as \
                     attached ECC[2], money as a figure off the note's own _balance, and the 4.0.35 \
                     optional endpoint leg this client sends as null"
                );
                let declared: Vec<String> = fund.iter().map(|(name, _)| name.clone()).collect();
                assert_eq!(
                    sent_note_keys, declared,
                    "the client must send exactly the arguments the compiled PrivateNote ABI declares"
                );

                let deal_fund = deal_fund.expect(
                    "TokenContract.fundDeal must be declared alongside PrivateNote.fundDeal -- the \
                     note's call has no receiver otherwise",
                );
                assert!(
                    deal_legacy.is_none(),
                    "4.0.33 renamed TokenContract.fundSellerBond to fundDeal; both cannot be declared"
                );
                assert_eq!(
                    deal_fund,
                    owned(&[("amount", "uint128"), ("endpointCipher", "optional(bytes)"),]),
                    "TokenContract.fundDeal takes the bond as a figure and the 4.0.35 optional \
                     endpoint leg; the ECC that arrives with it is the deal's gas, not the bond"
                );
                let declared: Vec<String> =
                    deal_fund.iter().map(|(name, _)| name.clone()).collect();
                assert_eq!(
                    sent_deal_keys, declared,
                    "the client must send exactly the arguments the compiled TokenContract ABI declares"
                );
            }
            (None, Some(legacy)) => {
                assert_eq!(
                    legacy,
                    owned(&[("nonce", "uint64"), ("amount", "uint128")]),
                    "a bundle without fundDeal must be the pre-4.0.33 PrivateNote, whose funding \
                     door is postSellerBond(nonce, amount)"
                );
                assert!(
                    deal_fund.is_none() && deal_legacy.is_some(),
                    "the two compiled ABIs must be the same generation: PrivateNote is pre-4.0.33 \
                     while TokenContract is not"
                );
                let declared: Vec<String> = legacy.iter().map(|(name, _)| name.clone()).collect();
                assert_ne!(
                    NOTE_FUND_DEAL_METHOD, "postSellerBond",
                    "the client targets the deployed 4.0.33 contracts, which no longer declare \
                     postSellerBond; the vendored artifacts are one generation behind the chain"
                );
                assert_ne!(
                    sent_note_keys, declared,
                    "the client must not fall back to the pre-4.0.33 (nonce, amount) shape"
                );
                assert_ne!(
                    DEAL_FUND_DEAL_METHOD, "fundSellerBond",
                    "the client targets TokenContract.fundDeal, not the renamed-away fundSellerBond"
                );
            }
            (None, None) => panic!(
                "the compiled PrivateNote ABI declares neither fundDeal nor postSellerBond -- it \
                 carries no seller-side funding door at all"
            ),
        }
    }

    /// Shape pin for the 4.0.34 deploy-gas door, decoded against the `PrivateNote.abi.json` this
    /// crate embeds -- the same discipline as
    /// [`seller_bond_encodes_the_funding_door_the_compiled_abi_declares`], and for the same reason:
    /// a client written against a proposed ABI passes every offline gate while being dead on chain.

    /// 4.0.34 removed the `rootModelShell` leg -- `fundDeployShell(uint64 nonce, uint128 tcShell)`
    /// (`contracts/dex/PrivateNote.sol:1143`) -- because `SuperRoot` deploys the `RootModel` and
    /// attaches its own value, leaving the note nothing to pre-fund. The client is adapted here; the
    /// compiled artifacts arrive with the contracts merge, so for one window the two are a generation
    /// apart. The invariant is stated over both states and is fail-closed in both:

    /// * artifacts at 4.0.34 -- the client must send exactly the two arguments the ABI declares;
    /// * artifacts still at 4.0.33 -- the ABI declares three, and the client must NOT be sending the
    /// third. Putting `rootModelShell` back turns this red in either state, which is the regression.

    /// The same invariant for the call that REPLACED the external deploy (contracts 4.0.36): the
    /// client sends exactly the arguments `PrivateNote.deployDeal` declares, in that order.

    /// It is worth its own test rather than trust, because two of the six are easy to get wrong in
    /// ways nothing else would catch. `modelHash` must be `sha256(modelName)` -- the constructor
    /// re-derives it on chain and refuses a pair that disagrees, so a client that sent the hash of
    /// something else would fail at deploy time with a bare TVM error. And `depositIdentifierHash`
    /// must NOT be here at all: the note supplies its own, and a client-supplied one is exactly the
    /// hole the constructor's sender check exists to close.
    #[test]
    fn deploy_deal_encodes_the_shape_the_compiled_abi_declares() {
        let model = "qwen--qwen3--32b";
        let params = note_deploy_deal_params(
            7,
            model,
            &crate::manifest::model_hash_for(model),
            1_000_000_000,
            1024,
            20_000_000_000,
        );
        let sent: Vec<String> = params
            .as_object()
            .expect("deployDeal params object")
            .keys()
            .cloned()
            .collect();
        let declared = compiled_abi_inputs(PRIVATENOTE_ABI, "deployDeal")
            .expect("compiled PrivateNote ABI declares deployDeal");

        assert_eq!(
            declared,
            owned(&[
                ("nonce", "uint64"),
                ("modelName", "string"),
                ("modelHash", "uint256"),
                ("pricePerTick", "uint128"),
                ("maxTicks", "uint128"),
                ("gasReserve", "uint128"),
            ]),
            "4.0.36 PrivateNote.deployDeal takes the deal terms plus its ECC[2] reserve"
        );
        assert_eq!(
            sent,
            declared
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            "the client must send exactly the arguments the compiled PrivateNote ABI declares"
        );
        assert!(
            !sent.contains(&"depositIdentifierHash".to_string()),
            "the note supplies its OWN deposit hash; a client-supplied one would let the caller name \
             the note the deal believes in"
        );
        assert_eq!(
            params["modelHash"],
            crate::manifest::model_hash_for(model),
            "modelHash must be sha256(modelName) -- the constructor re-derives it and refuses a \
             disagreeing pair"
        );
    }

    #[test]
    fn deploy_gas_encodes_the_fund_deploy_shell_shape_the_compiled_abi_declares() {
        let sent: Vec<String> = note_fund_deploy_shell_params(7, 10_000_000_000)
            .as_object()
            .expect("fundDeployShell params object")
            .keys()
            .cloned()
            .collect();
        let declared = compiled_abi_inputs(PRIVATENOTE_ABI, "fundDeployShell")
            .expect("compiled PrivateNote ABI declares fundDeployShell");
        let declared_names: Vec<String> = declared.iter().map(|(name, _)| name.clone()).collect();

        assert_eq!(
            sent,
            owned(&[("nonce", ""), ("tcShell", "")])
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            "the client sends the 4.0.34 shape: the deal's leg only"
        );

        if declared.len() == 2 {
            assert_eq!(
                declared,
                owned(&[("nonce", "uint64"), ("tcShell", "uint128")]),
                "4.0.34 PrivateNote.fundDeployShell funds the per-deal TokenContract only"
            );
            assert_eq!(
                sent, declared_names,
                "the client must send exactly the arguments the compiled PrivateNote ABI declares"
            );
        } else {
            assert_eq!(
                declared,
                owned(&[
                    ("nonce", "uint64"),
                    ("rootModelShell", "uint128"),
                    ("tcShell", "uint128"),
                ]),
                "a three-argument fundDeployShell must be the pre-4.0.34 shape; anything else is a \
                 generation this client cannot encode for"
            );
            assert!(
                !sent.contains(&"rootModelShell".to_string()),
                "the vendored artifacts are one generation behind the chain this client targets; \
                 the client must not fall back to funding a RootModel the super root now deploys"
            );
        }
    }

    /// Shape pin for how a `RootModel` comes into existence, over the same two states.

    /// 4.0.34 replaced `SuperRoot.registerRoot(uint256)` -- which verified a self-deployed root's
    /// address -- with `SuperRoot.deployRootModel(uint256)`, which performs the deploy
    /// (`contracts/airegistry/SuperRoot.sol:189`). The client stops deploying and starts asking. A
    /// method the deployed code does not declare is not refused, it is simply never executed, so the
    /// name is pinned against the ABI rather than trusted.
    #[test]
    fn root_model_deploy_targets_the_superroot_entry_the_compiled_abi_declares() {
        let sent: Vec<String> = super_root_deploy_root_model_params(&json!("0xabc"))
            .as_object()
            .expect("deployRootModel params object")
            .keys()
            .cloned()
            .collect();
        let deploy = compiled_abi_inputs(SUPERROOT_ABI, SUPERROOT_DEPLOY_ROOT_MODEL_METHOD);
        let legacy = compiled_abi_inputs(SUPERROOT_ABI, SUPERROOT_REGISTER_ROOT_METHOD);

        match (deploy, legacy) {
            (Some(deploy), legacy) => {
                assert!(
                    legacy.is_none(),
                    "4.0.34 removed SuperRoot.registerRoot when it added deployRootModel; a bundle \
                     declaring both is not a generation this client can encode for"
                );
                assert_eq!(
                    deploy,
                    owned(&[("ownerPubkey", "uint256")]),
                    "SuperRoot.deployRootModel takes the owner pubkey alone -- the address derives \
                     from it and the code comes from SuperRoot's own pin, so a caller can neither \
                     aim the deploy nor choose what lands there"
                );
                let declared: Vec<String> = deploy.iter().map(|(name, _)| name.clone()).collect();
                assert_eq!(
                    sent, declared,
                    "the client must send exactly the arguments the compiled SuperRoot ABI declares"
                );
            }
            (None, Some(legacy)) => {
                assert_eq!(
                    legacy,
                    owned(&[("ownerPubkey", "uint256")]),
                    "a bundle without deployRootModel must be the pre-4.0.34 SuperRoot, whose entry \
                     is registerRoot(ownerPubkey)"
                );
                assert_ne!(
                    SUPERROOT_DEPLOY_ROOT_MODEL_METHOD, SUPERROOT_REGISTER_ROOT_METHOD,
                    "the client targets the 4.0.34 SuperRoot, which no longer declares \
                     registerRoot; the vendored artifacts are one generation behind"
                );
                assert!(
                    compiled_abi_inputs(ROOTMODEL_ABI, "constructor")
                        .is_some_and(|ctor| !ctor.is_empty()),
                    "a pre-4.0.34 bundle still declares RootModel's constructor argument; once it \
                     is empty the SuperRoot ABI must have moved too"
                );
            }
            (None, None) => panic!(
                "the compiled SuperRoot ABI declares neither deployRootModel nor registerRoot -- it \
                 carries no way to bring a RootModel into existence at all"
            ),
        }
    }

    /// 0.34 -- a caller asking the note to fund a `RootModel` is refused, not quietly served a
    /// message that funds only the deal. The contract has no such leg any more, so honouring the
    /// request would drop the amount and report success while the RootModel stayed unfunded.
    #[test]
    fn note_refuses_to_fund_a_root_model_it_can_no_longer_reach() {
        assert!(
            root_model_deploy_shell_unsupported(0).is_none(),
            "zero is the only figure the removed leg can carry; it must not be an error"
        );
        let refusal = root_model_deploy_shell_unsupported(10_000_000_000)
            .expect("a non-zero RootModel funding request must be refused");
        assert!(
            refusal.contains("10000000000"),
            "the refusal must name the amount that was not sent: {refusal}"
        );
        assert!(
            refusal.contains("contracts/dex/PrivateNote.sol:1143"),
            "the refusal must cite the signature that removed the leg: {refusal}"
        );
    }

    /// Task O (4.0.33) removed the caller-named payee from the TokenContract's terminal doors:
    /// `close(payoutAddress) -> close()`, `destroy(payoutAddress) -> destroy()`,
    /// `withdrawShell(amount, recipient) -> withdrawShell(amount)`. A function id is derived from
    /// the whole signature, so an extra argument does not fail a guard -- it addresses a method the
    /// deployed code does not have, and the call is simply never executed. Pin the encoded shape
    /// against the compiled ABI so the next signature change is caught here, not on chain.
    #[test]
    fn token_contract_terminal_calls_match_the_compiled_4033_abi() {
        for (method, expected) in [
            ("destroy", owned(&[])),
            ("withdrawShell", owned(&[("amount", "uint128")])),
            ("close", owned(&[])),
        ] {
            let declared = compiled_abi_inputs(TOKENCONTRACT_ABI, method)
                .unwrap_or_else(|| panic!("compiled TokenContract ABI declares {method}"));
            assert_eq!(
                declared, expected,
                "TokenContract.{method} changed shape in the compiled bundle; the client's \
                 encoded arguments must move with it"
            );
        }

        // What the client actually puts on the wire for the two terminal doors it encodes.
        let destroy_keys: Vec<String> = json!({})
            .as_object()
            .expect("destroy params object")
            .keys()
            .cloned()
            .collect();
        assert!(
            destroy_keys.is_empty(),
            "destroy() takes no inputs on 4.0.33; the pre-Task-O payoutAddress must not be encoded"
        );
        let withdraw_keys: Vec<String> = json!({ "amount": 1u128.to_string() })
            .as_object()
            .expect("withdrawShell params object")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            withdraw_keys,
            vec!["amount".to_string()],
            "withdrawShell(uint128 amount) takes no recipient on 4.0.33"
        );
    }

    #[test]
    fn buyer_note_and_voucher_calls_match_the_compiled_4033_abis() {
        for (abi, method, expected) in [
            (
                PRIVATENOTE_ABI,
                "placeInferenceBuy",
                owned(&[
                    ("modelHash", "uint256"),
                    ("maxPricePerTick", "uint128"),
                    ("ticks", "uint128"),
                    ("escrow", "uint128"),
                    ("flags", "uint8"),
                    ("deadline", "uint64"),
                ]),
            ),
            (
                PRIVATENOTE_ABI,
                "cancelOrder",
                owned(&[
                    ("eventId", "uint256"),
                    ("oracleListHash", "uint256"),
                    ("tokenType", "uint32"),
                    ("orderId", "uint128"),
                ]),
            ),
            (
                PRIVATENOTE_ABI,
                "withdrawTokens",
                owned(&[("destWalletAddr", "address"), ("dapp_id", "uint256")]),
            ),
            (
                ROOTPN_ABI,
                "generateVoucher",
                owned(&[("skUCommit", "uint256"), ("isFee", "bool")]),
            ),
            (
                ROOTPN_ABI,
                "deployPrivateNote",
                owned(&[
                    ("zkproof", "bytes"),
                    ("depositIdentifierHash", "uint256"),
                    ("finalLayerHistoricalHashRoot", "uint256"),
                    ("voucherNominalFr", "uint256"),
                    ("tokenTypeFr", "uint256"),
                    ("ephemeralPubkey", "uint256"),
                    ("value", "uint64"),
                    ("tokenType", "uint32"),
                    ("layerNumber", "uint8"),
                ]),
            ),
        ] {
            assert_eq!(
                compiled_abi_inputs(abi, method),
                Some(expected),
                "{method} input shape drifted from the 4.0.33 artifact"
            );
        }

        let buy_payload = place_inference_buy_payload("0x1", 2, 3, 4, 5, 6);
        let buy_keys: BTreeSet<_> = buy_payload
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            buy_keys,
            BTreeSet::from([
                "deadline",
                "escrow",
                "flags",
                "maxPricePerTick",
                "modelHash",
                "ticks",
            ])
        );
        let dest =
            Address::parse("0:1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();
        let withdraw_payload = withdraw_note_tokens_payload(&dest, "0x4");
        let withdraw_keys: BTreeSet<_> = withdraw_payload
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(withdraw_keys, BTreeSet::from(["dapp_id", "destWalletAddr"]));
    }

    fn zero_address() -> String {
        format!("0:{}", "0".repeat(64))
    }

    fn valid_subscription_order(owner: &str) -> Value {
        let ticks = u128::from(SUBSCRIPTION_WEEKS);
        let reserve = crate::market::subscription_buy_reserve(ticks, PRICE_STEP)
            .expect("canonical subscription reserve");
        json!({
            "note": owner,
            "tokenContract": zero_address(),
            "price": PRICE_STEP.to_string(),
            "amount": ticks.to_string(),
            "escrow": reserve.total_escrow.to_string(),
            "deadline": "200",
            "flags": (flags::AON | flags::SUBSCRIPTION).to_string(),
            "ts": "100",
            "isBuy": true
        })
    }

    fn subscription_placement(order_id: u128, owner: &str) -> InferenceSubscriptionPlacement {
        InferenceSubscriptionPlacement {
            order_id,
            buyer_note: owner.to_string(),
            max_price_per_tick: PRICE_STEP,
            ticks: u128::from(SUBSCRIPTION_WEEKS),
            sub_weeks: SUBSCRIPTION_WEEKS,
            deadline: 200,
            created_at: 100,
        }
    }

    #[test]
    fn subscription_history_treats_cancelled_empty_order_as_absent() {
        let owner = format!("0:{}", "1".repeat(64));
        let tombstone = json!({
            "note": zero_address(),
            "tokenContract": zero_address(),
            "price": "0",
            "amount": "0",
            "escrow": "0",
            "deadline": "0",
            "flags": "0",
            "ts": "0",
            "isBuy": false
        });

        assert!(
            !subscription_order_is_active_for_owner(1, &tombstone, &owner)
                .expect("canonical cancelled tombstone is absent")
        );

        for field in ["note", "tokenContract"] {
            for malformed in ["x", ":", "0x"] {
                let mut mutated = tombstone.clone();
                mutated[field] = json!(malformed);
                let error = subscription_order_is_active_for_owner(1, &mutated, &owner)
                    .expect_err("strip-only pseudo-zero address must fail closed");
                assert!(
                    error.to_string().contains("non-canonical zero-amount"),
                    "{field}={malformed:?}: {error:#}"
                );
            }
        }
    }

    /// The shape the live chain actually returns for a cancelled order.

    /// `getOrder` is `Order o = _orders[id]` on a plain mapping
    /// (`contracts/airegistry/InferenceOrderBook.sol:1775`), so after `delete _orders[orderId]`
    /// (`:716`) every field comes back default-constructed -- and a default TVM `address` is
    /// `addr_none`, which the ABI decoder renders as `""`, not as the `addr_std` `0:`+64-zeros
    /// form a field explicitly written as `address(0)` produces. Reading only the second shape
    /// made `dexdo subscription cancel` report its own successful cancellation as a corrupt row.
    #[test]
    fn subscription_history_treats_addr_none_cancelled_order_as_absent() {
        let owner = format!("0:{}", "1".repeat(64));
        let tombstone = json!({
            "note": "",
            "tokenContract": "",
            "price": format!("0x{}", "0".repeat(64)),
            "amount": "0",
            "escrow": "0",
            "deadline": "0",
            "flags": "0",
            "ts": "0",
            "isBuy": false
        });

        assert!(
            !subscription_order_is_active_for_owner(4, &tombstone, &owner)
                .expect("an addr_none deletion tombstone is absent, not malformed")
        );
    }

    /// A resting subscription BUY carries no `TokenContract` and the chain may say so in either
    /// TVM shape; both mean "no deal yet" and neither may be read as a foreign deal.
    #[test]
    fn subscription_buy_accepts_either_absent_token_contract_shape() {
        let owner = format!("0:{}", "1".repeat(64));
        for absent in ["", &zero_address()] {
            let mut resting = valid_subscription_order(&owner);
            resting["tokenContract"] = json!(absent);

            assert!(
                subscription_order_is_active_for_owner(5, &resting, &owner)
                    .unwrap_or_else(|e| panic!("tokenContract={absent:?}: {e:#}")),
                "tokenContract={absent:?} must read as a resting subscription BUY"
            );
        }
    }

    #[test]
    fn subscription_history_rejects_non_empty_order_without_owner() {
        let owner = format!("0:{}", "1".repeat(64));
        let malformed = json!({
            "note": "",
            "amount": "1",
            "isBuy": true
        });

        let error = subscription_order_is_active_for_owner(2, &malformed, &owner)
            .expect_err("non-empty ownerless order must fail closed");
        assert!(error.to_string().contains("owner note"), "{error:#}");
    }

    #[test]
    fn subscription_history_rejects_nonempty_zero_amount_row() {
        let owner = format!("0:{}", "1".repeat(64));
        let malformed = json!({
            "note": owner.clone(),
            "tokenContract": zero_address(),
            "price": "0",
            "amount": "0",
            "escrow": "0",
            "deadline": "0",
            "flags": "0",
            "ts": "0",
            "isBuy": true
        });

        let error = subscription_order_is_active_for_owner(3, &malformed, &owner)
            .expect_err("zero amount alone must not classify a non-empty row as absent");
        assert!(
            error.to_string().contains("non-canonical zero-amount"),
            "{error:#}"
        );
    }

    #[test]
    fn subscription_placement_history_coalesces_only_identical_duplicates() {
        let owner = format!("0:{}", "1".repeat(64));
        let placement = subscription_placement(9, &owner);
        let placements = coalesce_correlated_subscription_placements(
            vec![placement.clone(), placement.clone()],
            &owner,
            PRICE_STEP,
            u128::from(SUBSCRIPTION_WEEKS),
        )
        .expect("byte-for-byte semantic duplicates coalesce");
        assert_eq!(placements, vec![placement]);
    }

    #[test]
    fn subscription_placement_history_rejects_conflicting_duplicate_order_id() {
        let owner = format!("0:{}", "1".repeat(64));
        let placement = subscription_placement(9, &owner);
        let mut conflicting = placement.clone();
        conflicting.deadline += 1;
        let error = coalesce_correlated_subscription_placements(
            vec![placement, conflicting],
            &owner,
            PRICE_STEP,
            u128::from(SUBSCRIPTION_WEEKS),
        )
        .expect_err("same order id with conflicting authenticated facts must fail closed");
        assert!(
            error.to_string().contains("conflicting authenticated"),
            "{error:#}"
        );
    }

    #[test]
    fn subscription_cancel_race_rejects_every_nonempty_fixed_id_mutation() {
        let owner = format!("0:{}", "1".repeat(64));
        let valid = valid_subscription_order(&owner);
        assert!(subscription_order_is_active_for_owner(10, &valid, &owner)
            .expect("canonical live subscription is active"));

        let reserve =
            crate::market::subscription_buy_reserve(u128::from(SUBSCRIPTION_WEEKS), PRICE_STEP)
                .expect("canonical subscription reserve");
        let mut mutations = Vec::new();

        let mut wrong_owner = valid.clone();
        wrong_owner["note"] = json!(format!("0:{}", "2".repeat(64)));
        mutations.push(("owner", wrong_owner));

        let mut wrong_side = valid.clone();
        wrong_side["isBuy"] = json!(false);
        mutations.push(("side", wrong_side));

        let mut wrong_flags = valid.clone();
        wrong_flags["flags"] = json!(flags::SUBSCRIPTION.to_string());
        mutations.push(("flags", wrong_flags));

        let mut wrong_token_contract = valid.clone();
        wrong_token_contract["tokenContract"] = json!(format!("0:{}", "3".repeat(64)));
        mutations.push(("tokenContract", wrong_token_contract));

        let mut zero_amount = valid.clone();
        zero_amount["amount"] = json!("0");
        mutations.push(("zero amount", zero_amount));

        let mut wrong_amount = valid.clone();
        wrong_amount["amount"] = json!("3");
        mutations.push(("amount shape", wrong_amount));

        let mut wrong_escrow = valid.clone();
        wrong_escrow["escrow"] = json!((reserve.total_escrow + 1).to_string());
        mutations.push(("escrow", wrong_escrow));

        let mut wrong_price = valid.clone();
        wrong_price["price"] = json!("1");
        mutations.push(("price", wrong_price));

        let mut wrong_deadline = valid.clone();
        wrong_deadline["deadline"] = json!("100");
        mutations.push(("deadline", wrong_deadline));

        let mut missing_shape = valid;
        missing_shape
            .as_object_mut()
            .expect("fixture is object")
            .remove("flags");
        mutations.push(("missing shape", missing_shape));

        for (label, mutation) in mutations {
            assert!(
                subscription_order_is_active_for_owner(10, &mutation, &owner).is_err(),
                "{label} mutation must be a contradiction, never inactive: {mutation}"
            );
        }

        let valid = valid_subscription_order(&owner);
        for field in ["note", "tokenContract"] {
            for malformed in ["x", ":", "0x"] {
                let mut mutation = valid.clone();
                mutation[field] = json!(malformed);
                assert!(
                    subscription_order_is_active_for_owner(10, &mutation, &owner).is_err(),
                    "{field}={malformed:?} must be a contradiction, never inactive: {mutation}"
                );
            }
        }
    }

    #[tokio::test]
    async fn fetch_ext_out_page_sends_bare_graphql_ids() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind GraphQL fixture");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept GraphQL request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let read = socket.read(&mut chunk).await.expect("read GraphQL request");
                request.extend_from_slice(&chunk[..read]);
                let Some(headers_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers_end = headers_end + 4;
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .expect("GraphQL request content length");
                if request.len() < headers_end + content_length {
                    continue;
                }
                let body: Value =
                    serde_json::from_slice(&request[headers_end..headers_end + content_length])
                        .expect("GraphQL request JSON");
                assert_eq!(
                    body["variables"]["accountId"],
                    "1111111111111111111111111111111111111111111111111111111111111111"
                );
                assert_eq!(
                    body["variables"]["dappId"],
                    "2222222222222222222222222222222222222222222222222222222222222222"
                );
                let response_body = json!({
                    "data": {"blockchain": {"account": {"messages": {
                        "pageInfo": {"startCursor": null, "hasPreviousPage": false},
                        "edges": []
                    }}}}
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write GraphQL response");
                break;
            }
        });

        let page = fetch_ext_out_page(
            &RequestGate::new(ChainRequestCeiling::Unlimited),
            &reqwest::Client::new(),
            &endpoint,
            &format!("0:{}", "1".repeat(64)),
            &format!("0:{}", "2".repeat(64)),
            100,
            None,
        )
        .await
        .expect("fetch ext-out page");
        assert!(page.messages.is_empty());
        assert_eq!(page.previous_cursor, None);
        task.await.expect("GraphQL fixture task");
    }

    #[tokio::test]
    async fn settlement_receipts_fail_closed_when_message_id_is_missing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind GraphQL fixture");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept GraphQL request");
            let _ = read_fixture_http_request(&mut socket).await;
            let response_body = json!({
                "data": {"blockchain": {"account": {"messages": {
                    "pageInfo": {"startCursor": null, "hasPreviousPage": false},
                    "edges": [{
                        "cursor": "opaque-cursor",
                        "node": {"body": "ignored", "created_at": 1}
                    }]
                }}}}
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write GraphQL response");
        });

        let error = fetch_ext_out_page(
            &RequestGate::new(ChainRequestCeiling::Unlimited),
            &reqwest::Client::new(),
            &endpoint,
            &format!("0:{}", "1".repeat(64)),
            &format!("0:{}", "2".repeat(64)),
            100,
            None,
        )
        .await
        .expect_err("cursor must never stand in for a missing message id");
        task.await.expect("GraphQL fixture task");
        let message = format!("{error:#}");
        assert!(message.contains("no message id"), "{message}");
        assert!(message.contains("opaque-cursor"), "{message}");
    }

    #[test]
    fn clock_skew_within_threshold_passes() {
        for local in [999_990, 1_000_030] {
            assert_eq!(
                clock_skew_check(local, 1_000_000).status,
                ChainDoctorStatus::Pass
            );
        }
    }

    #[test]
    fn doctor_report_endpoint_omits_url_credentials_query_and_fragment() {
        let endpoint = doctor_report_endpoint(
            "https://operator:secret@example.invalid:8443/rpc/graphql?access_token=private#trace",
        );
        assert_eq!(endpoint, "https://example.invalid:8443/rpc/graphql");
        for secret in ["operator", "secret", "access_token", "private", "trace"] {
            assert!(
                !endpoint.contains(secret),
                "doctor endpoint leaked {secret}: {endpoint}"
            );
        }
    }

    #[test]
    fn clock_skew_real_boundaries_fail_closed_with_actionable_message() {
        for behind in [41, 60] {
            let check = clock_skew_check(1_000_000 - behind, 1_000_000);
            assert_eq!(check.status, ChainDoctorStatus::Fail);
            assert!(check.message.contains("CLOCK_SKEW"));
            assert!(check
                .message
                .contains(&format!("{behind}s behind chain time")));
        }
        let check = clock_skew_check(1_000_000 + MAX_CLOCK_AHEAD_SECS + 1, 1_000_000);
        assert_eq!(check.status, ChainDoctorStatus::Fail);
        assert!(check.message.contains("CLOCK_SKEW"));
        assert!(check.message.contains("251s ahead of chain time"));
        assert!(check.message.contains("Fix system time / NTP and retry"));

        let report = ChainDoctorReport {
            network: "net-a".to_string(),
            endpoint: "https://net-a.example".to_string(),
            manifest_generation: Some("1.0.0".to_string()),
            chain_generation: Some("1.0.0".to_string()),
            clock_skew_seconds: i64::try_from(MAX_CLOCK_AHEAD_SECS + 1).unwrap(),
            versions: Vec::new(),
            checks: vec![check],
        };
        assert!(!report.is_ok(), "write preflight must fail closed");
        assert!(report.fail_summary().contains("CLOCK_SKEW"));
    }

    #[test]
    fn signed_write_expiry_codes_get_clock_hint_without_hiding_dex_errors() {
        let error = checked_submit_response(json!({
            "error": {
                "code": "TVM_ERROR",
                "message": "Failed to execute the message. Error occurred during the compute phase.",
                "exit_code": 103,
                "address": "0:1111111111111111111111111111111111111111111111111111111111111111"
            }
        }))
        .expect_err("nested giver replay rejection");
        assert!(format!("{error:#}").contains("verify the operator clock/NTP"));

        for (code, diagnosis) in [
            (102, "dex::ERR_LOW_VALUE"),
            (103, "dex::ERR_ALREADY_RESOLVED"),
        ] {
            let error = checked_submit_response(json!({
                "result": {
                    "exit_code": code,
                    "address": "0:2222222222222222222222222222222222222222222222222222222222222222"
                }
            }))
            .expect_err("ordinary dex rejection");
            let displayed = format!("{error:#}");
            assert!(displayed.contains(diagnosis), "{displayed}");
            assert!(!displayed.contains("operator clock"), "{displayed}");
        }

        for code in [401, 402] {
            let error = checked_submit_response(json!({"result": {"exit_code": code}}))
                .expect_err("dex expiry/replay rejection");
            assert!(format!("{error:#}").contains("verify the operator clock/NTP"));
        }
        let error = checked_submit_response(json!({"result": {"exit_code": 151}}))
            .expect_err("other rejection");
        assert!(!format!("{error:#}").contains("operator clock"));
    }

    struct SkewFixtureServer {
        task: tokio::task::JoinHandle<()>,
        unrecognized: Arc<Mutex<Option<String>>>,
    }

    impl SkewFixtureServer {
        fn abort(&self) {
            let unrecognized = self.unrecognized.lock().unwrap().clone();
            self.task.abort();
            if let Some(request) = unrecognized {
                panic!("unrecognized skew fixture request: {request}");
            }
        }
    }

    fn skew_fixture_disputed_token_contract_account() -> (String, String) {
        use tvm_block::{
            Account as TvmAccount, CurrencyCollection, Deserializable, MsgAddressInt, Serializable,
            StateInit,
        };

        let model_name = "fixture--model--v1";
        let buyer = format!("0:{}", "2".repeat(64));
        let funded_tokens = 2 * TICK_SIZE;
        let mut fields = json!({
            "_pubkey": "0x0",
            "_timestamp": "0",
            "_constructorFlag": true,
            "_sellerPubkey": "0x0",
            "_rootModelAddress": format!("0:{}", "0".repeat(64)),
            "_nonce": "0",
            "_iobHash": "0x1",
            "_iobDepth": "1",
            "_noteAuthorized": true,
            "_offerPosted": false,
            "_modelName": model_name,
            "_modelHash": model_hash_for(model_name),
            "_pricePerTick": "10",
            "_maxTicks": "2",
            "_buyer": buyer,
            "_buyerPubkey": "0x0",
            "_sellerNote": format!("0:{}", "3".repeat(64)),
            "_endpointCipher": "",
        });
        let lifecycle = json!({
            "_funded": true,
            "_opened": true,
            "_everOpened": true,
            "_disputed": true,
            "_probeAccepted": false,
            "_probeTick": "10",
            "_probeTime": "2",
            "_sellerBondFunded": true,
            "_buyerBondFunded": true,
            "_sellerBond": "20",
            "_buyerBond": "20",
            "_balance": "60",
            "_deposit": "10",
            "_finalizedOwed": "0",
            "_feeAccrued": "0",
            "_ticksFinalized": "0",
            "_everDisputed": true,
        });
        let subscription = json!({
            "_fundedTime": "1",
            "_disputeTime": "3",
            "_dealFlags": "0",
            "_subWeeks": "0",
            "_weekIndex": "0",
            "_tokensPerWeek": funded_tokens.to_string(),
            "_fundedTokens": funded_tokens.to_string(),
            "_tokensPaid": "0",
            "_periodStart": "1",
            "_weekBaseTokens": "0",
            "_tokensFinal": "0",
            "_tokensPend": "0",
            "_lastClaimTime": "2",
        });
        for part in [lifecycle, subscription] {
            fields
                .as_object_mut()
                .expect("TokenContract fixture storage object")
                .extend(
                    part.as_object()
                        .expect("TokenContract fixture storage part")
                        .clone(),
                );
        }
        let root = tvm_types::read_single_root_boc(TOKENCONTRACT_TVC)
            .expect("read TokenContract fixture TVC");
        let mut state_init =
            StateInit::construct_from_cell(root).expect("parse TokenContract fixture StateInit");
        let contract = tvm_abi::Contract::load(TOKENCONTRACT_ABI.as_bytes())
            .expect("load TokenContract fixture ABI");
        let tokens = tvm_abi::token::Tokenizer::tokenize_all_params(contract.fields(), &fields)
            .expect("tokenize TokenContract fixture storage");
        state_init.data = Some(
            tvm_abi::TokenValue::pack_values_into_chain(&tokens, Vec::new(), contract.version())
                .expect("encode TokenContract fixture storage")
                .into_cell()
                .expect("build TokenContract fixture data cell"),
        );
        let address = MsgAddressInt::with_standart(None, 0, [0x11; 32].into())
            .expect("TokenContract fixture address");
        let account = TvmAccount::active_by_init_code_hash(
            address,
            CurrencyCollection::from(100_000_000_000u64),
            0,
            state_init,
            false,
        )
        .expect("activate TokenContract fixture account");
        let account_cell = account
            .serialize()
            .expect("serialize TokenContract fixture account");
        let account_boc = base64::engine::general_purpose::STANDARD.encode(
            tvm_types::write_boc(&account_cell).expect("write TokenContract fixture account BOC"),
        );
        let code_hash = code_hash(TOKENCONTRACT_TVC).expect("TokenContract fixture code hash");
        (account_boc, code_hash)
    }

    fn skew_fixture_settlement_history(resolved: bool) -> Value {
        let buyer = format!("0:{}", "2".repeat(64));
        let mut edges = vec![json!({
            "cursor": "stream-disputed-cursor",
            "node": {
                "id": "stream-disputed-message",
                "body": encode_token_contract_event(
                    "StreamDisputed",
                    json!({"buyer": buyer, "at": "3"}),
                ),
                "created_at": 3,
            },
        })];
        if resolved {
            edges.push(json!({
                "cursor": "dispute-resolved-cursor",
                "node": {
                    "id": "dispute-resolved-message",
                    "body": encode_token_contract_event(
                        "DisputeResolved",
                        json!({
                            "toSeller": "0",
                            "refundToBuyer": "20",
                            "released": false,
                        }),
                    ),
                    "created_at": 4,
                },
            }));
        }
        json!({
            "data": {"blockchain": {"account": {"messages": {
                "pageInfo": {"startCursor": null, "hasPreviousPage": false},
                "edges": edges,
            }}}}
        })
    }

    fn skew_fixture_unrecognized_response(
        unrecognized: &Mutex<Option<String>>,
        request: &str,
    ) -> String {
        unrecognized
            .lock()
            .unwrap()
            .get_or_insert_with(|| request.to_string());
        let message = format!("unrecognized skew fixture request: {request}");
        json!({
            "error": {"code": "FIXTURE_UNRECOGNIZED", "message": message},
            "errors": [{"message": message}],
        })
        .to_string()
    }

    async fn skew_fixture_backend(
        chain_offset: i64,
    ) -> (
        RealChainBackend,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
        SkewFixtureServer,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let fixture_account_id = "1".repeat(64);
        let fixture_account_route = format!(
            "GET /v2/account?account_id={fixture_account_id}&dapp_id={fixture_account_id} "
        );
        let (fixture_account_boc, fixture_code_hash) =
            skew_fixture_disputed_token_contract_account();
        let posts = Arc::new(AtomicUsize::new(0));
        let server_posts = Arc::clone(&posts);
        let posted_bocs = Arc::new(Mutex::new(Vec::new()));
        let server_bocs = Arc::clone(&posted_bocs);
        let unrecognized = Arc::new(Mutex::new(None));
        let server_unrecognized = Arc::clone(&unrecognized);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_fixture_http_request(&mut socket).await;
                let request_body = request
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| body)
                    .unwrap_or_default();
                let body = if request.starts_with(&fixture_account_route) {
                    json!({"dapp_id": fixture_account_id}).to_string()
                } else if request.starts_with("POST /graphql ") {
                    let payload = serde_json::from_str::<Value>(request_body).ok();
                    let query = payload
                        .as_ref()
                        .and_then(|payload| payload["query"].as_str());
                    if query.is_some_and(|query| query.contains("blocks(last:1)")) {
                        let local = local_unix_secs().unwrap() as i64;
                        let chain = (local + chain_offset) as u64;
                        json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":chain}}]}}}}).to_string()
                    } else if query
                        .is_some_and(|query| query.contains("messages(msg_type: [ExtOut]"))
                    {
                        skew_fixture_settlement_history(server_posts.load(Ordering::SeqCst) > 0)
                            .to_string()
                    } else if query.is_some_and(|query| query.contains("info {")) {
                        let info = if server_posts.load(Ordering::SeqCst) == 0 {
                            json!({
                                "address": fixture_account_id,
                                "acc_type_name": "Active",
                                "boc": fixture_account_boc,
                                "code_hash": fixture_code_hash,
                                "balance": "0x174876e800",
                                "balance_other": [],
                            })
                        } else {
                            Value::Null
                        };
                        json!({"data":{"blockchain":{"account":{"info":info}}}}).to_string()
                    } else {
                        skew_fixture_unrecognized_response(&server_unrecognized, &request)
                    }
                } else if request.starts_with("POST /v2/messages ") {
                    let payload = serde_json::from_str::<Value>(request_body).ok();
                    let routed_boc = payload
                        .as_ref()
                        .and_then(Value::as_array)
                        .and_then(|entries| entries.first())
                        .filter(|entry| entry["account_id"] == fixture_account_id)
                        .filter(|entry| entry["dapp_id"] == fixture_account_id)
                        .and_then(|entry| entry["body"].as_str());
                    if let Some(boc) = routed_boc {
                        server_bocs.lock().unwrap().push(boc.to_string());
                        server_posts.fetch_add(1, Ordering::SeqCst);
                        json!({"result":{"exit_code":0}}).to_string()
                    } else {
                        skew_fixture_unrecognized_response(&server_unrecognized, &request)
                    }
                } else {
                    skew_fixture_unrecognized_response(&server_unrecognized, &request)
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let deployed = deployed("");
        let backend = RealChainBackend {
            client: LimitedChainClient::new(
                ChainClient::connect(&endpoint).unwrap(),
                ChainRequestCeiling::Unlimited,
            ),
            http: reqwest::Client::new(),
            money_post_http: build_money_post_http_client().unwrap(),
            superroot: Address::parse(&deployed.superroot).unwrap(),
            deployed,
        };
        (
            backend,
            posts,
            posted_bocs,
            SkewFixtureServer { task, unrecognized },
        )
    }

    #[tokio::test]
    async fn unsafe_clock_produces_zero_posts_in_regular_and_money_paths() {
        for chain_offset in [60, -300] {
            let (backend, posts, _, task) = skew_fixture_backend(chain_offset).await;
            let regular = backend.retry_submit("not-posted", false).await.unwrap_err();
            assert!(format!("{regular:#}").contains("CLOCK_SKEW"));

            let address = Address::parse(&format!("0:{}", "1".repeat(64))).unwrap();
            let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
            let money = backend
                .prepare_money_post(&address, "{}", "unused", json!({}), &keys)
                .await
                .unwrap_err();
            assert!(format!("{money:#}").contains("CLOCK_SKEW"));
            assert_eq!(
                posts.load(Ordering::SeqCst),
                0,
                "no message POST is permitted"
            );
            task.abort();
        }
    }

    fn aborted_submit_error() -> anyhow::Error {
        checked_submit_response(json!({
            "result": {
                "exit_code": 0,
                "aborted": true
            }
        }))
        .expect_err("aborted submit must fail")
    }

    /// the two pins that name a CODE IMAGE must equal the image this tree ships.

    /// A mutation caught this: `GENERATION_PINS.token_contract_code` -> `deadbeef...` left 619 tests
    /// passing and the three that failed failed byte-identically. Nothing read that pin offline, so
    /// a wrong value would have travelled to a live `doctor` and been reported as the chain's fault.

    /// **These two are not like the root pins beside them, and that is why they can be asserted.**
    /// `superroot` / `rootpn` / `rootoracle` are statements about accounts on a chain: this tree has
    /// no image of them, they move when somebody redeploys, and there is nothing local to compare
    /// against. `token_contract_code` and `inference_orderbook` name code that RootPN deploys FROM --
    /// the vendored `.tvc`s -- so the tree does know the answer, and a pin that disagrees with the
    /// image beside it is wrong before any chain is dialled.
    #[test]
    fn the_book_and_deal_pins_are_the_images_this_tree_ships() {
        let row = super::super::contracts_provision::generation_pins("4.0.36")
            .expect("the committed network has a row");

        let deal_code = super::super::contracts_provision::code_boc_b64(
            super::super::contracts_provision::TOKENCONTRACT_TVC,
        )
        .expect("the vendored TokenContract image carries code");
        assert_eq!(
            super::cell_boc_repr_hash(&deal_code).as_deref(),
            row.token_contract_code,
            "the deal-code pin and the vendored TokenContract disagree; RootPN mints the vendored \
             one, so the pin is what is wrong",
        );

        let book_code = super::super::contracts_provision::code_boc_b64(
            super::super::contracts_provision::INFERENCE_ORDERBOOK_TVC,
        )
        .expect("the vendored InferenceOrderBook image carries code");
        assert_eq!(
            super::cell_boc_repr_hash(&book_code).as_deref(),
            row.inference_orderbook,
            "the book pin and the vendored InferenceOrderBook disagree",
        );

        // And it must not be absent: a `None` becomes a `Skip`, and `is_ok()` counts a `Skip` as
        // passing -- the shape that let this pin go unread in the first place.
        assert!(
            row.inference_orderbook.is_some(),
            "an absent book pin is a check that cannot fail",
        );
    }

    /// a buy order is refused for two different shortfalls under ONE error code.

    /// Since 4.0.36 `placeInferenceBuy` burns `BUY_ORDER_GAS` out of the note's ACCOUNT ECC[2] before
    /// the order reaches the book, while the escrow still comes from the note's PRIVATE balance. Both
    /// shortfalls answer `ERR_LOW_VALUE`, they are different pockets, and they are topped up
    /// differently -- so a buyer whose escrow is fine and whose account is empty reads an error that
    /// points at the escrow and tops up the wrong thing.

    /// The charge is new: a sell offer always paid, a buy rested for free.
    #[test]
    fn a_buy_order_names_which_pocket_is_short() {
        let charge = crate::params::BUY_ORDER_GAS_RAW;
        let escrow = 10 * crate::params::SHELL_UNIT;

        // Both pockets covered: nothing to say.
        assert!(super::buy_order_shortfall(escrow, charge, escrow).is_none());

        // Escrow short -- the older of the two failures, and still the first one checked.
        let low_escrow = super::buy_order_shortfall(escrow - 1, charge, escrow)
            .expect("an escrow the note cannot cover is refused");
        assert!(low_escrow.contains("escrows"), "{low_escrow}");

        // The NEW failure: escrow fine, account empty. This is the one that used to work.
        let low_charge = super::buy_order_shortfall(escrow, charge - 1, escrow)
            .expect("a placement charge the note cannot burn is refused");
        assert!(low_charge.contains("ECC[2]"), "{low_charge}");
        assert!(low_charge.contains("separate pocket"), "{low_charge}");
        assert!(low_charge.contains("the escrow is fine"), "{low_charge}");
        assert!(
            low_charge.contains("rest for free"),
            "the refusal has to say the charge is new, or a buyer who placed orders yesterday reads \
             it as a broken client: {low_charge}",
        );

        // The two must not be confusable: neither text may read as the other's fix.
        assert!(!low_escrow.contains("ECC[2]"), "{low_escrow}");
    }

    /// a buyer's terminal call has TWO ways to be unpayable and the chain reports one.

    /// Since 4.0.36 the note attaches `DEAL_GAS_TERMINAL` to `stop`/`dispute`/`cleanupUnopened`, but
    /// only `if (currencies[SHELL] >= DEAL_GAS_TERMINAL)`. So an empty note is NOT a failure -- the
    /// deal's own reserve covers it -- and a dry reserve is not one either while the note can attach.
    /// Only both at once is, and then `gosh.burnecc` fails the action phase with
    /// `RESULT_CODE_NOT_ENOUGH_EXTRA`: one code for two situations whose fixes are opposite, since
    /// only the SELLER's note can refill a deal (`fundDeployShell` is owner-only).

    /// Without this the buyer reads "aborted, code 38" and cannot tell whose money is missing.
    #[test]
    fn a_terminal_call_is_refused_only_when_neither_side_can_pay() {
        let note = Address::parse(&format!("0:{}", "a".repeat(64))).expect("note");
        let deal = Address::parse(&format!("0:{}", "b".repeat(64))).expect("deal");
        let charge = crate::params::DEAL_BURN_TERMINAL_RAW;

        // The ordinary case for a spent-out buyer: empty note, funded deal. Goes.
        assert!(
            super::terminal_charge_refusal("streamStop", &note, 0, &deal, charge, charge).is_none(),
            "an empty note is what the deal's reserve is for; refusing here takes the buyer's exit \
             away for no reason",
        );
        // The other single-sided case: funded note, dry deal. Also goes -- the note attaches.
        assert!(
            super::terminal_charge_refusal("streamStop", &note, charge, &deal, 0, charge).is_none(),
        );
        // One short of the charge on both sides is the only refusal.
        let refusal = super::terminal_charge_refusal(
            "streamDispute",
            &note,
            charge - 1,
            &deal,
            charge - 1,
            charge,
        )
        .expect("neither side can pay, so the call would revert and change nothing");

        // It has to name BOTH readings and both fixes, or it is the same unactionable answer the
        // chain already gives.
        assert!(refusal.contains("streamDispute"), "{refusal}");
        assert!(refusal.contains(&note.to_string()), "{refusal}");
        assert!(refusal.contains(&deal.to_string()), "{refusal}");
        assert!(refusal.contains(&(charge - 1).to_string()), "{refusal}");
        assert!(refusal.contains("SELLER"), "{refusal}");
        assert!(refusal.contains("Nothing was sent"), "{refusal}");
    }

    #[test]
    fn fund_deploy_shell_correlated_no_funds_receipt_adds_ecc2_context() {
        let contextual = fund_deploy_shell_receipt_error(
            aborted_submit_error(),
            "abcd",
            Some(&CorrelatedActionReceipt {
                message_hash: "abcd".to_string(),
                transaction_hash: Some("tx38".to_string()),
                aborted: Some(true),
                action_success: Some(false),
                result_code: Some(38),
                no_funds: Some(true),
                ..Default::default()
            }),
        );
        let displayed = format!("{contextual:#}");
        assert!(
            displayed.contains("insufficient ECC[2]/SHELL"),
            "{displayed}"
        );
        assert!(displayed.contains("note_fund_deploy_shell"), "{displayed}");
        assert!(displayed.contains("aborted=true"), "{displayed}");
        assert!(displayed.contains("action_success=false"), "{displayed}");
        assert!(displayed.contains("action_result_code=38"), "{displayed}");
        assert!(displayed.contains("no_funds=true"), "{displayed}");
    }

    #[test]
    fn fund_deploy_shell_non_38_receipt_is_factual_without_ecc2_claim() {
        let contextual = fund_deploy_shell_receipt_error(
            aborted_submit_error(),
            "abcd",
            Some(&CorrelatedActionReceipt {
                message_hash: "abcd".to_string(),
                transaction_hash: Some("tx401".to_string()),
                aborted: Some(true),
                action_success: Some(false),
                result_code: Some(401),
                no_funds: Some(false),
                ..Default::default()
            }),
        );
        let displayed = format!("{contextual:#}");
        assert!(!displayed.contains("insufficient ECC[2]"), "{displayed}");
        assert!(displayed.contains("action_result_code=401"), "{displayed}");
        assert!(displayed.contains("aborted=true"), "{displayed}");
        assert!(displayed.contains("ECC[2] cause not proven"), "{displayed}");
    }

    #[test]
    fn fund_deploy_shell_missing_or_mismatched_receipt_fails_closed() {
        let missing = fund_deploy_shell_receipt_error(aborted_submit_error(), "abcd", None);
        let missing = format!("{missing:#}");
        assert!(!missing.contains("insufficient ECC[2]"), "{missing}");
        assert!(
            missing.contains("no finalized destination receipt matched"),
            "{missing}"
        );

        let mismatched = fund_deploy_shell_receipt_error(
            aborted_submit_error(),
            "abcd",
            Some(&CorrelatedActionReceipt {
                message_hash: "ffff".to_string(),
                transaction_hash: Some("tx38".to_string()),
                aborted: Some(true),
                action_success: Some(false),
                result_code: Some(38),
                no_funds: Some(true),
                ..Default::default()
            }),
        );
        let mismatched = format!("{mismatched:#}");
        assert!(!mismatched.contains("insufficient ECC[2]"), "{mismatched}");
        assert!(
            mismatched.contains("ECC[2] cause not proven"),
            "{mismatched}"
        );
    }

    #[test]
    fn fund_deploy_shell_incomplete_or_inconsistent_38_receipt_fails_closed() {
        for (case, receipt) in [
            (
                "action success",
                CorrelatedActionReceipt {
                    message_hash: "abcd".to_string(),
                    transaction_hash: Some("tx38".to_string()),
                    aborted: Some(true),
                    action_success: Some(true),
                    result_code: Some(38),
                    no_funds: Some(true),
                    ..Default::default()
                },
            ),
            (
                "missing no_funds",
                CorrelatedActionReceipt {
                    message_hash: "abcd".to_string(),
                    transaction_hash: Some("tx38".to_string()),
                    aborted: Some(true),
                    action_success: Some(false),
                    result_code: Some(38),
                    no_funds: None,
                    ..Default::default()
                },
            ),
            (
                "missing aborted",
                CorrelatedActionReceipt {
                    message_hash: "abcd".to_string(),
                    transaction_hash: Some("tx38".to_string()),
                    aborted: None,
                    action_success: Some(false),
                    result_code: Some(38),
                    no_funds: Some(true),
                    ..Default::default()
                },
            ),
            (
                "missing transaction id",
                CorrelatedActionReceipt {
                    message_hash: "abcd".to_string(),
                    transaction_hash: None,
                    aborted: Some(true),
                    action_success: Some(false),
                    result_code: Some(38),
                    no_funds: Some(true),
                    ..Default::default()
                },
            ),
        ] {
            let contextual =
                fund_deploy_shell_receipt_error(aborted_submit_error(), "abcd", Some(&receipt));
            let displayed = format!("{contextual:#}");
            assert!(
                !displayed.contains("insufficient ECC[2]"),
                "{case}: {displayed}"
            );
            assert!(
                displayed.contains("ECC[2] cause not proven"),
                "{case}: {displayed}"
            );
            assert!(
                displayed.contains("action_result_code=38"),
                "{case}: {displayed}"
            );
        }
    }

    #[test]
    fn exact_message_receipt_keeps_polling_without_destination_transaction() {
        let raw = json!({
            "data": {"blockchain": {
                "message": {
                    "id": "abcd",
                    "dst": "11",
                    "dst_transaction": null
                },
                "account": {"info": null}
            }}
        });

        assert_eq!(
            parse_exact_destination_receipt(&raw, "11", "04", "abcd")
                .expect("pending exact message must not fail"),
            None
        );
    }

    #[test]
    fn exact_message_receipt_keeps_polling_non_finalized_destination_transaction() {
        let raw = json!({
            "data": {"blockchain": {
                "message": {
                    "id": "abcd",
                    "dst": "11",
                    "dst_transaction": {"status": 1}
                },
                "account": {"info": null}
            }}
        });

        assert_eq!(
            parse_exact_destination_receipt(&raw, "11", "04", "abcd")
                .expect("non-finalized destination transaction must not fail"),
            None
        );
    }

    #[test]
    fn exact_message_receipt_rejects_malformed_finalized_destination_transaction() {
        let raw = json!({
            "data": {"blockchain": {
                "message": {
                    "id": "abcd",
                    "dst": "11",
                    "dst_transaction": {"status": 3}
                },
                "account": {"info": {"id": "11", "dapp_id": "04"}}
            }}
        });

        let error = parse_exact_destination_receipt(&raw, "11", "04", "abcd")
            .expect_err("malformed finalized receipt must fail closed");
        assert!(
            error.to_string().contains("has no account"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn exact_message_receipt_query_and_raw_shape_ignore_unrelated_newer_messages() {
        const TARGET: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        const DAPP: &str = "04";
        const HASH: &str = "abcd";
        assert!(
            EXACT_MESSAGE_RECEIPT_QUERY.contains("message(hash: $hash)"),
            "receipt lookup must address the submitted message directly"
        );
        assert!(
            !EXACT_MESSAGE_RECEIPT_QUERY.contains("messages("),
            "receipt lookup must not scan a bounded account-message window"
        );
        assert!(
            !EXACT_MESSAGE_RECEIPT_QUERY.contains("last_trans_lt"),
            "wallet freshness must use exact transaction identity, not incompatible LT encodings"
        );
        for field in ["outmsg_cnt", "balance_other", "transactions(last: 1)"] {
            assert!(
                EXACT_MESSAGE_RECEIPT_QUERY.contains(field),
                "receipt lookup must include {field}"
            );
        }

        let raw = json!({
            "data": {"blockchain": {
                "message": {
                    "id": HASH,
                    "dst": format!("0:{TARGET}"),
                    "dst_transaction": {
                        "id": "tx38",
                        "status": 3,
                        "aborted": true,
                        "account_addr": TARGET,
                        "lt": "0x62b9b6",
                        "outmsg_cnt": 0,
                        "compute": {"exit_code": 0, "success": true},
                        "action": {"result_code": 38, "success": false, "no_funds": true}
                    }
                },
                "account": {"info": {
                    "id": TARGET,
                    "dapp_id": DAPP,
                    "last_trans_lt": "0x62b9b7",
                    "balance_other": [
                        {"currency": 1, "value": "0x64"},
                        {"currency": 2, "value": "0xc8"}
                    ]
                }, "transactions": {"edges": [{"node": {"id": "tx38"}}]}},
                "messages": {"edges": [{"node": {
                    "id": "unrelated-newer-message",
                    "dst": format!("0:{TARGET}")
                }}]}
            }}
        });
        let receipt = parse_exact_destination_receipt(&raw, TARGET, DAPP, HASH)
            .expect("raw exact-hash GraphQL shape")
            .expect("finalized destination receipt");
        assert_eq!(receipt.message_hash, HASH);
        assert_eq!(receipt.transaction_hash.as_deref(), Some("tx38"));
        assert_eq!(receipt.compute_exit_code, Some(0));
        assert_eq!(receipt.result_code, Some(38));
        assert_eq!(receipt.no_funds, Some(true));
        assert_eq!(receipt.outmsg_count, Some(0));
        assert_eq!(
            receipt.account_latest_transaction_hash.as_deref(),
            Some("tx38")
        );
        assert_eq!(receipt.account_ecc_balances, Some(vec![(1, 100), (2, 200)]));
        assert_eq!(
            note_deploy_wallet_action_observation(receipt)
                .expect("exact current wallet state must be usable"),
            NoteDeployWalletActionObservation {
                transaction_hash: "tx38".to_string(),
                aborted: true,
                action_result_code: 38,
                outmsg_count: 0,
                wallet_ecc_balances: Some(vec![(1, 100), (2, 200)]),
            }
        );
    }

    #[test]
    fn note_deploy_wallet_receipt_rejects_absent_or_stale_effect_state() {
        let complete = CorrelatedActionReceipt {
            message_hash: "abcd".to_string(),
            transaction_hash: Some("tx38".to_string()),
            aborted: Some(true),
            compute_exit_code: Some(0),
            action_success: Some(false),
            result_code: Some(38),
            no_funds: Some(true),
            outmsg_count: Some(0),
            account_latest_transaction_hash: Some("tx38".to_string()),
            account_ecc_balances: Some(vec![(1, 100), (2, 200)]),
        };
        let mut missing_latest_transaction = complete.clone();
        missing_latest_transaction.account_latest_transaction_hash = None;
        let mut advanced_account = complete.clone();
        advanced_account.account_latest_transaction_hash = Some("tx39".to_string());
        let mut missing_outmsg_count = complete.clone();
        missing_outmsg_count.outmsg_count = None;
        for (case, receipt, expected) in [
            (
                "missing latest transaction",
                missing_latest_transaction,
                "no latest wallet transaction hash",
            ),
            ("advanced account", advanced_account, "stale or advanced"),
            (
                "missing outmsg count",
                missing_outmsg_count,
                "no outbound-message count",
            ),
        ] {
            let error = note_deploy_wallet_action_observation(receipt)
                .expect_err("incomplete or non-current effect state must fail closed");
            assert!(error.to_string().contains(expected), "{case}: {error:#}");
        }
    }

    #[test]
    fn note_deploy_rootpn_receipt_surfaces_exact_compute_403_without_action_phase() {
        let receipt = note_deploy_rootpn_action_observation(CorrelatedActionReceipt {
            message_hash: "abcd".to_string(),
            transaction_hash: Some("tx403".to_string()),
            aborted: Some(true),
            compute_exit_code: Some(403),
            ..Default::default()
        })
        .expect("compute abort is an exact finalized RootPN observation");

        assert_eq!(
            receipt,
            NoteDeployRootPnActionObservation {
                transaction_hash: "tx403".to_string(),
                compute_exit_code: 403,
                aborted: true,
                action_result_code: None,
            }
        );
    }

    #[test]
    fn note_deploy_success_observation_allows_eventually_indexed_ecc_state() {
        let receipt = CorrelatedActionReceipt {
            message_hash: "abcd".to_string(),
            transaction_hash: Some("tx38".to_string()),
            aborted: Some(false),
            compute_exit_code: Some(0),
            action_success: Some(true),
            result_code: Some(0),
            no_funds: Some(false),
            outmsg_count: Some(3),
            account_latest_transaction_hash: Some("tx38".to_string()),
            account_ecc_balances: None,
        };
        let observation = note_deploy_wallet_action_observation(receipt)
            .expect("successful action continues to downstream VoucherGenerated confirmation");

        assert_eq!(observation.transaction_hash, "tx38");
        assert_eq!(observation.wallet_ecc_balances, None);
    }

    #[test]
    fn exact_message_receipt_rejects_wrong_destination_account_or_dapp() {
        const TARGET: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        let base = json!({
            "data": {"blockchain": {
                "message": {
                    "id": "abcd",
                    "dst": TARGET,
                    "dst_transaction": {
                        "id": "tx38", "status": 3, "aborted": true,
                        "account_addr": TARGET,
                        "compute": {"exit_code": 0, "success": true},
                        "action": {"result_code": 38, "success": false, "no_funds": true}
                    }
                },
                "account": {"info": {"id": TARGET, "dapp_id": "04"}}
            }}
        });
        for (case, mut raw) in [
            ("destination", base.clone()),
            ("transaction account", base.clone()),
            ("account", base.clone()),
            ("dapp", base),
        ] {
            let replacement = Value::String("22".repeat(32));
            match case {
                "destination" => raw["data"]["blockchain"]["message"]["dst"] = replacement,
                "transaction account" => {
                    raw["data"]["blockchain"]["message"]["dst_transaction"]["account_addr"] =
                        replacement
                }
                "account" => raw["data"]["blockchain"]["account"]["info"]["id"] = replacement,
                "dapp" => {
                    raw["data"]["blockchain"]["account"]["info"]["dapp_id"] =
                        Value::String("05".to_string())
                }
                _ => unreachable!(),
            }
            let error = parse_exact_destination_receipt(&raw, TARGET, "04", "abcd")
                .expect_err("mismatched receipt must fail closed");
            assert!(error.to_string().contains("mismatch"), "{case}: {error:#}");
        }
    }

    #[tokio::test]
    async fn exact_message_receipt_poller_crosses_pending_states_to_one_final_receipt() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        const TARGET: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        const DAPP: &str = "04";
        const HASH: &str = "abcd";
        let responses = Arc::new(Mutex::new(VecDeque::from([
            json!({"data": {"blockchain": {
                "message": {"id": HASH, "dst": TARGET, "dst_transaction": null},
                "account": {"info": null}
            }}}),
            json!({"data": {"blockchain": {
                "message": {
                    "id": HASH, "dst": TARGET,
                    "dst_transaction": {"status": 1}
                },
                "account": {"info": null}
            }}}),
            json!({"data": {"blockchain": {
                "message": {
                    "id": HASH,
                    "dst": TARGET,
                    "dst_transaction": {
                        "id": "tx38", "status": 3, "aborted": true,
                        "account_addr": TARGET,
                        "compute": {"exit_code": 0, "success": true},
                        "action": {"result_code": 38, "success": false, "no_funds": true}
                    }
                },
                "account": {"info": {"id": TARGET, "dapp_id": DAPP}}
            }}}),
        ])));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let query = {
            let responses = Arc::clone(&responses);
            let reads = Arc::clone(&reads);
            move || {
                let responses = Arc::clone(&responses);
                let reads = Arc::clone(&reads);
                async move {
                    reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(responses
                        .lock()
                        .expect("scripted response lock")
                        .pop_front()
                        .expect("poller must not exceed the three scripted reads"))
                }
            }
        };

        let receipt = poll_finalized_destination_receipt_with(
            TARGET,
            DAPP,
            HASH,
            query,
            std::time::Duration::ZERO,
        )
        .await
        .expect("pending reads must not abort the poller")
        .expect("third read must return the finalized correlated receipt");

        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(responses.lock().expect("scripted response lock").is_empty());
        assert_eq!(
            receipt,
            CorrelatedActionReceipt {
                message_hash: HASH.to_string(),
                transaction_hash: Some("tx38".to_string()),
                aborted: Some(true),
                compute_exit_code: Some(0),
                action_success: Some(false),
                result_code: Some(38),
                no_funds: Some(true),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn exact_message_receipt_poller_does_not_retry_malformed_finalized_receipt() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let reads = Arc::new(AtomicUsize::new(0));
        let query = {
            let reads = Arc::clone(&reads);
            move || {
                let reads = Arc::clone(&reads);
                async move {
                    reads.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"data": {"blockchain": {
                        "message": {
                            "id": "abcd", "dst": "11",
                            "dst_transaction": {"status": 3}
                        },
                        "account": {"info": {"id": "11", "dapp_id": "04"}}
                    }}}))
                }
            }
        };

        let error = poll_finalized_destination_receipt_with(
            "11",
            "04",
            "abcd",
            query,
            std::time::Duration::ZERO,
        )
        .await
        .expect_err("malformed finalized receipt must terminate fail-closed");

        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(error.to_string().contains("has no account"), "{error:#}");
    }

    fn encode_token_contract_event(name: &str, fields: Value) -> String {
        use tvm_abi::token::Tokenizer;
        use tvm_abi::{Contract, TokenValue};
        use tvm_types::{BuilderData, IBitstring as _};

        let contract =
            Contract::load(TOKENCONTRACT_ABI.as_bytes()).expect("load TokenContract ABI");
        let event = contract.event(name).expect("TokenContract event by name");
        let tokens =
            Tokenizer::tokenize_all_params(&event.inputs, &fields).expect("tokenize event");
        let mut prefix = BuilderData::new();
        prefix.append_u32(event.get_id()).expect("event selector");
        let builder =
            TokenValue::pack_values_into_chain(&tokens, vec![prefix.into()], &event.abi_version)
                .expect("encode event body");
        let cell = builder.into_cell().expect("event cell");
        base64::engine::general_purpose::STANDARD
            .encode(tvm_types::write_boc(&cell).expect("event BOC"))
    }

    fn encode_event_selector_only(name: &str) -> String {
        use tvm_abi::Contract;
        use tvm_types::{BuilderData, IBitstring as _};

        let contract =
            Contract::load(TOKENCONTRACT_ABI.as_bytes()).expect("load TokenContract ABI");
        let event = contract.event(name).expect("TokenContract event by name");
        let mut body = BuilderData::new();
        body.append_u32(event.get_id()).expect("event selector");
        base64::engine::general_purpose::STANDARD.encode(
            tvm_types::write_boc(&body.into_cell().expect("event cell")).expect("event BOC"),
        )
    }

    fn encode_unknown_event() -> String {
        use tvm_abi::Contract;
        use tvm_types::{BuilderData, IBitstring as _};

        let contract =
            Contract::load(TOKENCONTRACT_ABI.as_bytes()).expect("load TokenContract ABI");
        let id = (0..u32::MAX)
            .find(|id| contract.event_by_id(*id).is_err())
            .expect("an unknown event id");
        let mut body = BuilderData::new();
        body.append_u32(id).expect("unknown event selector");
        base64::engine::general_purpose::STANDARD.encode(
            tvm_types::write_boc(&body.into_cell().expect("event cell")).expect("event BOC"),
        )
    }

    fn deployed(endpoint_field: &str) -> Deployed {
        serde_json::from_str(&format!(
            r#"{{
                "network": "net-a",
                "superroot": "0:{zeros}",
                "dapp_config": "0:{zeros}",
                "dapp_id": "{zeros}"
                {endpoint_field}
            }}"#,
            zeros = "0".repeat(64),
        ))
        .unwrap()
    }

    #[test]
    fn a_manifest_without_an_endpoint_is_refused_instead_of_guessed_at() {
        // This used to assert a built-in host for a manifest that names no endpoint. That default
        // is what let a manifest for one chain answer from another: the label was read, a host was
        // assembled out of it, and the run proceeded against whatever answered. A name is not an
        // address, so the absence of one is now a refusal.
        let error = resolve_endpoint(None, &deployed("")).expect_err(
            "a manifest carrying no endpoint says nothing about where to dial, and must refuse",
        );
        let error = format!("{error:#}");
        assert!(error.contains("carries no `endpoint`"), "{error}");
        assert!(error.contains("NAME is not an address"), "{error}");
    }

    #[tokio::test]
    async fn full_ext_in_boc_decodes_place_inference_buy_when_body_projection_is_absent() {
        let note =
            Address::parse("0:1111111111111111111111111111111111111111111111111111111111111111")
                .expect("note");
        let keys = KeyPair::from_secret_hex(&"22".repeat(32)).expect("owner key");
        let boc = RealChainBackend::encode_signed_call_boc(
            &note,
            PRIVATENOTE_ABI,
            "placeInferenceBuy",
            json!({
                "modelHash": format!("0x{}", "33".repeat(32)),
                "maxPricePerTick": "7",
                "ticks": "2",
                "escrow": "14",
                "flags": 0,
                "deadline": "0",
            }),
            &keys,
        )
        .await
        .expect("encode owner-signed placeInferenceBuy");

        let decoded = decode_external_abi_message_boc(&boc, PRIVATENOTE_ABI, true)
            .expect("decode placeInferenceBuy from the full indexed message BOC");
        assert_eq!(decoded.function_name, "placeInferenceBuy");
        assert_eq!(decoded_u128(&decoded.tokens, "maxPricePerTick"), Some(7));
        assert_eq!(decoded_u128(&decoded.tokens, "ticks"), Some(2));
        assert_eq!(decoded_u128(&decoded.tokens, "escrow"), Some(14));
    }

    async fn read_fixture_http_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            socket.read_buf(&mut request).await.unwrap();
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .to_ascii_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    #[tokio::test]
    async fn live_acceptance_submit_payloads_match_vendored_abis_exactly() {
        let address =
            Address::parse("0:1111111111111111111111111111111111111111111111111111111111111111")
                .expect("address");
        let keys = KeyPair::from_secret_hex(&"22".repeat(32)).expect("signer");
        let (backend, posts, posted_bocs, server) = skew_fixture_backend(0).await;

        backend.resolve_dispute_timeout(&address).await.unwrap();
        backend
            .submit_pmp_cancel_event(&address, &keys)
            .await
            .unwrap();
        backend
            .delete_oracle_event(&address, &keys, "22")
            .await
            .unwrap();

        assert_eq!(posts.load(Ordering::SeqCst), 3);
        let posted_bocs = posted_bocs.lock().unwrap().clone();
        assert_eq!(posted_bocs.len(), 3, "one POST per production method");
        for (boc, (abi, method, expected_field)) in posted_bocs.iter().zip([
            (TOKENCONTRACT_ABI, "resolveDisputeTimeout", None),
            (PMP_ABI, "submitCancelEvent", None),
            (ORACLEEVENTLIST_ABI, "deleteEvent", Some(("eventId", 22))),
        ]) {
            let decoded = decode_external_abi_message_boc(boc, abi, true)
                .unwrap_or_else(|| panic!("decode {method}"));
            assert_eq!(decoded.function_name, method);
            if let Some((field, value)) = expected_field {
                assert_eq!(decoded_u128(&decoded.tokens, field), Some(value));
            } else {
                assert!(decoded.tokens.is_empty(), "{method} must have no inputs");
            }
        }
        assert!(backend.oracle_event_info(&address, "22").await.is_err());
        server.abort();
    }

    #[test]
    fn settlement_receipts_decode_current_ordinary_claim_sequence_in_chain_order() {
        let buyer = format!("0:{}", "44".repeat(32));
        let message = |id: &str, created_at: u64, event: &str, fields: Value| ExtOutMessage {
            id: id.to_string(),
            created_at,
            cursor: format!("opaque-{id}"),
            body: encode_token_contract_event(event, fields),
        };
        let receipts = decode_token_contract_settlement_receipts(vec![
            message(
                "accepted",
                10,
                "ProbeAccepted",
                json!({
                    "buyer": buyer,
                    "toSeller": "1",
                    "bondReturned": "0",
                }),
            ),
            message(
                "claim-z",
                20,
                "TicksClaimed",
                json!({"trusted": "1", "claimed": "2"}),
            ),
            // Same-second events deliberately use lexically descending opaque cursors. Their
            // supplied chain order must survive unchanged.
            message(
                "claim-a",
                20,
                "TicksClaimed",
                json!({"trusted": "1", "claimed": "3"}),
            ),
            message(
                "stop",
                40,
                "StreamStopped",
                json!({
                    "buyer": buyer,
                    "toSeller": "0",
                    "refundToBuyer": "0",
                }),
            ),
        ])
        .expect("decode exact settlement lifecycle");

        assert_eq!(
            receipts.events,
            vec![
                TokenContractSettlementReceipt {
                    message_id: "accepted".to_string(),
                    created_at: 10,
                    cursor: "opaque-accepted".to_string(),
                    event: TokenContractSettlementEvent::ProbeAccepted {
                        buyer: buyer.clone(),
                        to_seller: 1,
                        bond_returned: 0,
                    },
                },
                TokenContractSettlementReceipt {
                    message_id: "claim-z".to_string(),
                    created_at: 20,
                    cursor: "opaque-claim-z".to_string(),
                    event: TokenContractSettlementEvent::TicksClaimed {
                        trusted: 1,
                        claimed: 2,
                    },
                },
                TokenContractSettlementReceipt {
                    message_id: "claim-a".to_string(),
                    created_at: 20,
                    cursor: "opaque-claim-a".to_string(),
                    event: TokenContractSettlementEvent::TicksClaimed {
                        trusted: 1,
                        claimed: 3,
                    },
                },
                TokenContractSettlementReceipt {
                    message_id: "stop".to_string(),
                    created_at: 40,
                    cursor: "opaque-stop".to_string(),
                    event: TokenContractSettlementEvent::StreamStopped {
                        buyer,
                        to_seller: 0,
                        refund_to_buyer: 0,
                    },
                },
            ]
        );
    }

    fn test_action_receipt(
        id: &str,
        created_at: u64,
        event: TokenContractSettlementEvent,
    ) -> TokenContractSettlementReceipt {
        TokenContractSettlementReceipt {
            message_id: id.to_string(),
            created_at,
            cursor: format!("opaque-{id}"),
            event,
        }
    }

    fn test_pre_bonds() -> SettlementActionBondState {
        SettlementActionBondState {
            seller_bond_held: 20u128.into(),
            seller_bond_required: 20u128.into(),
            buyer_bond_held: 0u128.into(),
            buyer_bond_required: 0u128.into(),
        }
    }

    #[test]
    fn action_selector_accepts_each_exact_action_event_and_rejects_other_action_events() {
        let buyer = format!("0:{}", "44".repeat(32));
        let candidates = [
            TokenContractSettlementEvent::ProbeBurned {
                buyer: buyer.clone(),
                burned_probe: 1,
                burned_bond: 2,
                refund_to_buyer: 3,
            },
            TokenContractSettlementEvent::StreamStopped {
                buyer: buyer.clone(),
                to_seller: 4,
                refund_to_buyer: 5,
            },
            TokenContractSettlementEvent::StreamDisputed {
                buyer: buyer.clone(),
                at: 6,
            },
            TokenContractSettlementEvent::DisputeResolved {
                to_seller: 7,
                refund_to_buyer: 8,
                released: true,
            },
            TokenContractSettlementEvent::DisputeResolved {
                to_seller: 9,
                refund_to_buyer: 10,
                released: false,
            },
        ];
        let cases = [
            (
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::ProbeBurned,
                0,
            ),
            (
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::StreamStopped,
                1,
            ),
            (
                SettlementAction::SellerStop,
                ExpectedSettlementEvent::StreamStopped,
                1,
            ),
            (
                SettlementAction::Dispute,
                ExpectedSettlementEvent::StreamDisputed,
                2,
            ),
            (
                SettlementAction::ReleaseDispute,
                ExpectedSettlementEvent::DisputeResolved { released: true },
                3,
            ),
            (
                SettlementAction::ResolveDisputeTimeout,
                ExpectedSettlementEvent::DisputeResolved { released: false },
                4,
            ),
        ];

        for (action, expected, accepted_index) in cases {
            for (candidate_index, candidate) in candidates.iter().cloned().enumerate() {
                let observed = TokenContractSettlementReceipts {
                    events: vec![test_action_receipt("new-action", 1, candidate)],
                };
                let selected = select_new_settlement_action_receipt(
                    "0:tc",
                    action,
                    expected,
                    matches!(
                        action,
                        SettlementAction::BuyerStop
                            | SettlementAction::SellerStop
                            | SettlementAction::Dispute
                    )
                    .then_some(buyer.as_str()),
                    &observed,
                    test_pre_bonds(),
                );
                if candidate_index == accepted_index {
                    let receipt = selected
                        .unwrap_or_else(|error| panic!("{action}: {error:#}"))
                        .unwrap();
                    assert_eq!(receipt.message_id, "new-action");
                    assert_eq!(receipt.pre_bonds, test_pre_bonds());
                    match &receipt.event {
                        SettlementActionEvent::ProbeBurned { buyer: actor, .. }
                        | SettlementActionEvent::StreamStopped { buyer: actor, .. }
                        | SettlementActionEvent::StreamDisputed { buyer: actor, .. } => {
                            assert_eq!(actor, &buyer, "receipt must preserve the decoded actor")
                        }
                        SettlementActionEvent::DisputeResolved { .. } => {}
                    }
                } else {
                    assert!(
                        selected.is_err(),
                        "{action} must reject incompatible action event {candidate_index}"
                    );
                }
            }
        }
    }

    #[test]
    fn action_selector_rejects_wrong_buyer_actor_for_every_actor_event() {
        let buyer = format!("0:{}", "44".repeat(32));
        let wrong = format!("0:{}", "55".repeat(32));
        let cases = [
            (
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::ProbeBurned,
                TokenContractSettlementEvent::ProbeBurned {
                    buyer: wrong.clone(),
                    burned_probe: 1,
                    burned_bond: 2,
                    refund_to_buyer: 3,
                },
            ),
            (
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::StreamStopped,
                TokenContractSettlementEvent::StreamStopped {
                    buyer: wrong.clone(),
                    to_seller: 4,
                    refund_to_buyer: 5,
                },
            ),
            (
                SettlementAction::SellerStop,
                ExpectedSettlementEvent::StreamStopped,
                TokenContractSettlementEvent::StreamStopped {
                    buyer: wrong.clone(),
                    to_seller: 6,
                    refund_to_buyer: 7,
                },
            ),
            (
                SettlementAction::Dispute,
                ExpectedSettlementEvent::StreamDisputed,
                TokenContractSettlementEvent::StreamDisputed {
                    buyer: wrong,
                    at: 8,
                },
            ),
        ];

        for (action, expected, event) in cases {
            let error = select_new_settlement_action_receipt(
                "0:tc",
                action,
                expected,
                Some(&buyer),
                &TokenContractSettlementReceipts {
                    events: vec![test_action_receipt("wrong-actor", 1, event)],
                },
                test_pre_bonds(),
            )
            .expect_err("wrong buyer actor must fail closed before receipt success");
            assert!(
                error.to_string().contains("wrong buyer actor"),
                "{action}: {error:#}"
            );
        }
    }

    #[test]
    fn action_selector_allows_ordered_auxiliary_tick_before_one_terminal_event() {
        let buyer = format!("0:{}", "44".repeat(32));
        let observed = TokenContractSettlementReceipts {
            events: vec![
                test_action_receipt(
                    "tick",
                    1,
                    TokenContractSettlementEvent::TickFinalized {
                        finalized_owed: 11,
                        deposit: 12,
                    },
                ),
                test_action_receipt(
                    "stop",
                    2,
                    TokenContractSettlementEvent::StreamStopped {
                        buyer: buyer.clone(),
                        to_seller: 11,
                        refund_to_buyer: 12,
                    },
                ),
            ],
        };
        let receipt = select_new_settlement_action_receipt(
            "0:tc",
            SettlementAction::BuyerStop,
            ExpectedSettlementEvent::StreamStopped,
            Some(&buyer),
            &observed,
            test_pre_bonds(),
        )
        .expect("auxiliary event is allowed")
        .expect("one exact action event");
        assert_eq!(receipt.message_id, "stop");
    }

    #[test]
    fn action_selector_rejects_concurrent_duplicate_or_conflicting_action_events() {
        let buyer = format!("0:{}", "44".repeat(32));
        for second in [
            TokenContractSettlementEvent::StreamStopped {
                buyer: buyer.clone(),
                to_seller: 1,
                refund_to_buyer: 2,
            },
            TokenContractSettlementEvent::DisputeResolved {
                to_seller: 1,
                refund_to_buyer: 2,
                released: true,
            },
        ] {
            let observed = TokenContractSettlementReceipts {
                events: vec![
                    test_action_receipt(
                        "stop-one",
                        1,
                        TokenContractSettlementEvent::StreamStopped {
                            buyer: buyer.clone(),
                            to_seller: 1,
                            refund_to_buyer: 2,
                        },
                    ),
                    test_action_receipt("action-two", 2, second),
                ],
            };
            let error = select_new_settlement_action_receipt(
                "0:tc",
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::StreamStopped,
                Some(&buyer),
                &observed,
                test_pre_bonds(),
            )
            .expect_err("two new action events are ambiguous");
            assert!(error.to_string().contains("2 distinct new action events"));
        }
    }

    #[test]
    fn receipt_snapshot_excludes_old_terminal_and_keeps_new_chain_order() {
        let buyer = "0:buyer".to_string();
        let old = test_action_receipt(
            "old-stop",
            1,
            TokenContractSettlementEvent::StreamStopped {
                buyer: buyer.clone(),
                to_seller: 1,
                refund_to_buyer: 2,
            },
        );
        let tick = test_action_receipt(
            "tick",
            2,
            TokenContractSettlementEvent::TickFinalized {
                finalized_owed: 3,
                deposit: 4,
            },
        );
        let new = test_action_receipt(
            "new-stop",
            3,
            TokenContractSettlementEvent::StreamStopped {
                buyer,
                to_seller: 3,
                refund_to_buyer: 4,
            },
        );
        let after = settlement_receipts_after_snapshot(
            &TokenContractSettlementReceipts {
                events: vec![old.clone()],
            },
            TokenContractSettlementReceipts {
                events: vec![old, tick.clone(), new.clone()],
            },
        )
        .expect("immutable pre-submit identities exclude old events");
        assert_eq!(after.events, vec![tick, new]);
    }

    #[test]
    fn receipt_snapshot_rejects_changed_pre_submit_identity() {
        let before = test_action_receipt(
            "same-id",
            1,
            TokenContractSettlementEvent::TickFinalized {
                finalized_owed: 1,
                deposit: 2,
            },
        );
        let mut changed = before.clone();
        changed.cursor = "changed-opaque-cursor".to_string();
        let error = settlement_receipts_after_snapshot(
            &TokenContractSettlementReceipts {
                events: vec![before],
            },
            TokenContractSettlementReceipts {
                events: vec![changed],
            },
        )
        .expect_err("an old event identity cannot mutate across the POST");
        assert!(error.to_string().contains("changed"));
    }

    #[test]
    fn receipt_snapshot_rejects_disappearance_reorder_and_late_old_event() {
        let buyer = "0:buyer".to_string();
        let first = test_action_receipt(
            "baseline-first",
            10,
            TokenContractSettlementEvent::TickFinalized {
                finalized_owed: 1,
                deposit: 2,
            },
        );
        let second = test_action_receipt(
            "baseline-second",
            11,
            TokenContractSettlementEvent::ProbeAccepted {
                buyer: buyer.clone(),
                to_seller: 1,
                bond_returned: 2,
            },
        );
        let late_old = test_action_receipt(
            "late-indexed-old-stop",
            1,
            TokenContractSettlementEvent::StreamStopped {
                buyer,
                to_seller: 1,
                refund_to_buyer: 2,
            },
        );
        let before = TokenContractSettlementReceipts {
            events: vec![first.clone(), second.clone()],
        };

        for (case, current) in [
            ("disappearance", vec![first.clone()]),
            ("reorder", vec![second.clone(), first.clone()]),
            (
                "late-old insertion",
                vec![late_old, first.clone(), second.clone()],
            ),
        ] {
            let error = settlement_receipts_after_snapshot(
                &before,
                TokenContractSettlementReceipts { events: current },
            )
            .expect_err("post history must preserve the exact baseline as its prefix");
            assert!(
                error.to_string().contains("append-only extension"),
                "{case} escaped append-only history validation: {error:#}"
            );
        }
    }

    #[test]
    fn confirmation_delay_never_exceeds_the_shared_remaining_budget() {
        let timeout = std::time::Duration::from_secs(5);
        let poll = std::time::Duration::from_secs(2);
        assert_eq!(
            settlement_confirmation_delay(std::time::Duration::ZERO, timeout, poll),
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(
            settlement_confirmation_delay(std::time::Duration::from_secs(2), timeout, poll),
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(
            settlement_confirmation_delay(std::time::Duration::from_secs(4), timeout, poll),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            settlement_confirmation_delay(std::time::Duration::from_secs(5), timeout, poll),
            None
        );
        assert_eq!(
            settlement_confirmation_delay(std::time::Duration::from_secs(6), timeout, poll),
            None
        );
    }

    #[test]
    fn settlement_receipts_fail_closed_on_malformed_known_event_and_skip_unknown() {
        let error = decode_token_contract_settlement_receipts(vec![ExtOutMessage {
            id: "malformed-stop".to_string(),
            created_at: 1,
            cursor: "cursor-malformed".to_string(),
            body: encode_event_selector_only("StreamStopped"),
        }])
        .expect_err("known selector with malformed payload must fail closed");
        let message = format!("{error:#}");
        assert!(message.contains("StreamStopped"), "{message}");
        assert!(message.contains("malformed-stop"), "{message}");

        let receipts = decode_token_contract_settlement_receipts(vec![
            ExtOutMessage {
                id: "unknown".to_string(),
                created_at: 1,
                cursor: "cursor-unknown".to_string(),
                body: encode_unknown_event(),
            },
            ExtOutMessage {
                id: "not-an-event".to_string(),
                created_at: 2,
                cursor: "cursor-other".to_string(),
                body: "not-base64".to_string(),
            },
        ])
        .expect("unknown/non-event bodies are not lifecycle claims");
        assert!(receipts.events.is_empty());
    }

    #[test]
    fn settlement_receipts_deduplicate_identical_overlap_and_reject_conflict() {
        let buyer = format!("0:{}", "44".repeat(32));
        let message = ExtOutMessage {
            id: "same-message".to_string(),
            created_at: 1,
            cursor: "same-cursor".to_string(),
            body: encode_token_contract_event(
                "StreamStopped",
                json!({"buyer": buyer, "toSeller": "1", "refundToBuyer": "2"}),
            ),
        };
        let receipts =
            decode_token_contract_settlement_receipts(vec![message.clone(), message.clone()])
                .expect("identical overlapping pages deduplicate");
        assert_eq!(receipts.events.len(), 1);

        let mut conflicting = message.clone();
        conflicting.cursor = "different-cursor".to_string();
        let error = decode_token_contract_settlement_receipts(vec![message, conflicting])
            .expect_err("same id with changed order/body must fail closed");
        assert!(
            error
                .to_string()
                .contains("changed across overlapping pages"),
            "{error:#}"
        );
    }

    #[test]
    fn ext_out_walk_filter_map_retains_only_candidates_and_rejects_conflicts() {
        let first = ExtOutMessage {
            id: "first".to_string(),
            created_at: 1,
            cursor: "cursor-first".to_string(),
            body: "unrelated".to_string(),
        };
        let candidate = ExtOutMessage {
            id: "candidate".to_string(),
            created_at: 2,
            cursor: "cursor-candidate".to_string(),
            body: "fill".to_string(),
        };
        let mut visited = Vec::new();
        let retained = filter_map_ext_out_messages_in_order(
            vec![first, candidate.clone(), candidate.clone()],
            |message| {
                visited.push(message.id.clone());
                Ok((message.body == "fill").then_some(message.id))
            },
        )
        .expect("filter the deduplicated history walk");
        assert_eq!(visited, ["first", "candidate"]);
        assert_eq!(retained, ["candidate"]);

        let mut conflicting = candidate.clone();
        conflicting.body = "changed-fill".to_string();
        let error = filter_map_ext_out_messages_in_order(vec![candidate, conflicting], |message| {
            Ok(Some(message.id))
        })
        .expect_err("a reused message id with changed content must fail closed");
        assert!(
            error
                .to_string()
                .contains("changed across overlapping pages"),
            "{error:#}"
        );
    }

    #[test]
    fn book_fill_candidate_verification_requires_both_parties_and_current_funding() {
        let candidate = BookFillCandidate {
            maker_id: 17,
            taker_id: 18,
            ticks: 4,
            clearing_price: "700".to_string(),
            seller_token_contract: format!("0:{}", "aa".repeat(32)),
            buyer_note: format!("0:{}", "bb".repeat(32)),
            seller_note: format!("0:{}", "cc".repeat(32)),
        };
        let requested_buyer = candidate.buyer_note.clone();
        let parties = TokenContractParties {
            buyer: candidate.buyer_note.clone(),
            seller_note: candidate.seller_note.clone(),
        };
        let state = test_deal_state(false, false, 0);
        assert!(book_fill_candidate_is_verified(
            &candidate,
            &requested_buyer,
            Some(&parties),
            Some(&state),
        ));
        assert!(!book_fill_candidate_is_verified(
            &candidate,
            &requested_buyer,
            None,
            Some(&state),
        ));
        assert!(!book_fill_candidate_is_verified(
            &candidate,
            &requested_buyer,
            Some(&parties),
            None,
        ));

        let mut unfunded = state;
        unfunded.funded = false;
        assert!(!book_fill_candidate_is_verified(
            &candidate,
            &requested_buyer,
            Some(&parties),
            Some(&unfunded),
        ));

        let wrong_buyer = TokenContractParties {
            buyer: format!("0:{}", "dd".repeat(32)),
            ..parties.clone()
        };
        assert!(!book_fill_candidate_is_verified(
            &candidate,
            &requested_buyer,
            Some(&wrong_buyer),
            Some(&state),
        ));

        let wrong_seller = TokenContractParties {
            seller_note: format!("0:{}", "ee".repeat(32)),
            ..parties
        };
        assert!(!book_fill_candidate_is_verified(
            &candidate,
            &requested_buyer,
            Some(&wrong_seller),
            Some(&state),
        ));
    }

    #[test]
    fn outstanding_candidate_confirmed_by_its_token_contract_is_offered_as_a_lead() {
        let note = format!("0:{}", "bb".repeat(32));
        let token_contract = format!("0:{}", "aa".repeat(32));
        let parties = TokenContractParties {
            buyer: note.clone(),
            seller_note: format!("0:{}", "cc".repeat(32)),
        };
        let state = test_deal_state(true, false, 0);

        let lead =
            classify_outstanding_deal_lead(&note, &token_contract, Some(&parties), Some(&state))
                .expect("matching getParties plus funded getState offers a lead");

        assert_eq!(lead.token_contract, token_contract);
        assert_eq!(lead.role, DealRole::Buyer);
        assert_eq!(lead.state, state);
    }

    #[test]
    fn outstanding_candidate_whose_token_contract_is_not_funded_is_refused() {
        let note = format!("0:{}", "bb".repeat(32));
        let token_contract = format!("0:{}", "aa".repeat(32));
        let parties = TokenContractParties {
            buyer: note.clone(),
            seller_note: format!("0:{}", "cc".repeat(32)),
        };
        let mut state = test_deal_state(false, false, 0);
        state.funded = false;

        let refusal =
            classify_outstanding_deal_lead(&note, &token_contract, Some(&parties), Some(&state))
                .expect_err("getOutstanding cannot promote an unfunded address to a deal lead");

        assert_eq!(refusal.token_contract, token_contract);
        assert!(refusal.reason.contains("funded=false"), "{refusal:?}");
    }

    #[test]
    fn outstanding_candidate_whose_token_contract_is_absent_or_destroyed_is_refused() {
        let note = format!("0:{}", "bb".repeat(32));
        let token_contract = format!("0:{}", "aa".repeat(32));

        let refusal = classify_outstanding_deal_lead(&note, &token_contract, None, None)
            .expect_err("a stale pointer to a destroyed TokenContract is never a deal lead");

        assert_eq!(refusal.token_contract, token_contract);
        assert!(
            refusal.reason.contains("absent or destroyed"),
            "{refusal:?}"
        );
    }

    fn test_deal_state(opened: bool, disputed: bool, finalized_owed: u128) -> DealChainState {
        DealChainState {
            funded: true,
            opened,
            probe_accepted: true,
            disputed,
            deposit: if opened { 100 } else { 0 },
            finalized_owed,
            tokens_final: 1_000_001,
            tokens_pending: 1_000_003,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 2,
            last_claim_time: 4,
            dispute_time: if disputed { 5 } else { 0 },
        }
    }

    fn test_subscription(is_subscription: bool) -> DealSubscription {
        DealSubscription {
            deal_flags: if is_subscription {
                crate::market::flags::SUBSCRIPTION
            } else {
                0
            },
            sub_weeks: if is_subscription {
                SUBSCRIPTION_WEEKS
            } else {
                0
            },
            week_index: 0,
            tokens_per_week: 4_000_000,
            funded_tokens: 4_000_000,
            tokens_paid: 0,
            period_start: 1,
            week_base_tokens: 0,
        }
    }

    fn test_deal_snapshot(
        boc: &str,
        opened: bool,
        disputed: bool,
        finalized_owed: u128,
        seller_bond_held: u128,
    ) -> DealChainSnapshot {
        test_deal_snapshot_with_buyer_bond(
            boc,
            opened,
            disputed,
            finalized_owed,
            seller_bond_held,
            0,
            0,
        )
    }

    fn test_deal_snapshot_with_buyer_bond(
        boc: &str,
        opened: bool,
        disputed: bool,
        finalized_owed: u128,
        seller_bond_held: u128,
        buyer_bond_held: u128,
        buyer_bond_required: u128,
    ) -> DealChainSnapshot {
        DealChainSnapshot {
            account_code_hash: "code".to_string(),
            account_boc_hash: boc.to_string(),
            state: test_deal_state(opened, disputed, finalized_owed),
            subscription: test_subscription(buyer_bond_required != 0),
            seller_bond: DealSellerBond {
                bond_funded: true,
                bond_held: seller_bond_held,
                bond_required: 20,
            },
            buyer_bond: DealBuyerBond {
                bond_held: buyer_bond_held,
                bond_required: buyer_bond_required,
            },
        }
    }

    #[test]
    fn action_post_state_rejects_event_getter_contradiction_and_preserves_raw_tokens() {
        let pre = test_deal_snapshot("pre", true, false, 0, 20);
        let post = test_deal_snapshot("post", false, false, u64::MAX as u128 + 1, 0);
        let contradiction = TokenContractSettlementEvent::StreamStopped {
            buyer: "0:buyer".to_string(),
            to_seller: u64::MAX as u128 + 2,
            refund_to_buyer: u128::MAX,
        };
        let error = settlement_action_post_state("0:tc", &pre, &post, &contradiction)
            .expect_err("event/getter mismatch fails closed");
        assert!(error.to_string().contains("event/getter contradiction"));

        let exact = TokenContractSettlementEvent::StreamStopped {
            buyer: "0:buyer".to_string(),
            to_seller: u64::MAX as u128 + 1,
            refund_to_buyer: u128::MAX,
        };
        let state = settlement_action_post_state("0:tc", &pre, &post, &exact)
            .expect("exact raw uint128 facts agree");
        assert_eq!(state.tokens_final.0, 1_000_001);
    }

    #[test]
    fn terminal_post_state_rejects_funded_deposit_and_probe_mutations() {
        let pre = test_deal_snapshot("pre", true, false, 0, 20);
        let post = test_deal_snapshot("post", false, false, 7, 0);
        let stopped = TokenContractSettlementEvent::StreamStopped {
            buyer: "0:buyer".to_string(),
            to_seller: 7,
            refund_to_buyer: 8,
        };
        settlement_action_post_state("0:tc", &pre, &post, &stopped)
            .expect("canonical active terminal getter state is stopped");

        let mut mutations = Vec::new();
        let mut changed = post.clone();
        changed.state.funded = false;
        mutations.push(("funded", changed));
        let mut changed = post.clone();
        changed.state.deposit = 1;
        mutations.push(("deposit", changed));
        let mut changed = post;
        changed.state.probe_tick = 1;
        mutations.push(("probeTick", changed));

        for (field, changed) in mutations {
            let error = settlement_action_post_state("0:tc", &pre, &changed, &stopped).unwrap_err();
            assert!(
                error.to_string().contains("terminal event contradicts"),
                "{field} mutation escaped: {error:#}"
            );
        }
    }

    #[test]
    fn stream_disputed_proves_all_money_tokens_and_separate_bonds_unchanged() {
        let pre = test_deal_snapshot_with_buyer_bond("pre", true, false, 17, 20, 20, 20);
        let post = test_deal_snapshot_with_buyer_bond("post", true, true, 17, 20, 20, 20);
        let disputed = TokenContractSettlementEvent::StreamDisputed {
            buyer: "0:buyer".to_string(),
            at: 5,
        };
        settlement_action_post_state("0:tc", &pre, &post, &disputed)
            .expect("only disputed/disputeTime changed");

        let mut mutations = Vec::new();
        let mut changed = post.clone();
        changed.state.deposit += 1;
        mutations.push(("deposit", changed));
        let mut changed = post.clone();
        changed.state.finalized_owed += 1;
        mutations.push(("finalizedOwed", changed));
        let mut changed = post.clone();
        changed.state.tokens_final += 1;
        mutations.push(("tokensFinal", changed));
        let mut changed = post.clone();
        changed.state.tokens_pending += 1;
        mutations.push(("tokensPending", changed));
        let mut changed = post.clone();
        changed.seller_bond.bond_held -= 1;
        mutations.push(("sellerBondHeld", changed));
        let mut changed = post.clone();
        changed.buyer_bond.bond_held -= 1;
        mutations.push(("buyerBondHeld", changed));
        let mut changed = post.clone();
        changed.subscription.tokens_paid += 1;
        mutations.push(("subscription", changed));

        for (field, changed) in mutations {
            let error =
                settlement_action_post_state("0:tc", &pre, &changed, &disputed).unwrap_err();
            assert!(
                error.to_string().contains("only disputed/disputeTime"),
                "{field} mutation escaped: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn lost_post_response_reconciles_one_landed_event_without_second_post() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let posts = Arc::new(AtomicUsize::new(0));
        let posts_for_submit = posts.clone();
        let pre = test_deal_snapshot("pre", true, false, 0, 20);
        let post = test_deal_snapshot("post", false, false, 7, 0);
        let observed = TokenContractSettlementReceipts {
            events: vec![test_action_receipt(
                "landed-stop",
                7,
                TokenContractSettlementEvent::StreamStopped {
                    buyer: format!("0:{}", "44".repeat(32)),
                    to_seller: 7,
                    refund_to_buyer: 8,
                },
            )],
        };
        let observed_for_read = observed.clone();
        let post_for_read = post.clone();

        let receipt = reconcile_settlement_action_after_post(
            "0:tc",
            SettlementAction::BuyerStop,
            ExpectedSettlementEvent::StreamStopped,
            Some(&format!("0:{}", "44".repeat(32))),
            &TokenContractSettlementReceipts::default(),
            &pre,
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
            move || async move {
                posts_for_submit.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                Ok(())
            },
            move || {
                let observed = observed_for_read.clone();
                async move { Ok(observed) }
            },
            move || async move { Ok(Some(post_for_read)) },
        )
        .await
        .expect("lost response must reconcile the landed event");
        assert_eq!(receipt.message_id, "landed-stop");
        assert_eq!(posts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn no_event_until_confirmation_budget_is_ambiguous_and_posts_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let posts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let posts_for_submit = posts.clone();
        let reads_for_observe = reads.clone();
        let pre = test_deal_snapshot("pre", true, false, 0, 20);
        let error = reconcile_settlement_action_after_post(
            "0:tc",
            SettlementAction::BuyerStop,
            ExpectedSettlementEvent::StreamStopped,
            Some(&format!("0:{}", "44".repeat(32))),
            &TokenContractSettlementReceipts::default(),
            &pre,
            std::time::Duration::from_millis(25),
            std::time::Duration::from_millis(2),
            std::time::Duration::from_millis(5),
            move || async move {
                posts_for_submit.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            move || {
                reads_for_observe.fetch_add(1, Ordering::SeqCst);
                async { Ok(TokenContractSettlementReceipts::default()) }
            },
            || async { Ok(None) },
        )
        .await
        .expect_err("absence inside the bounded wait is never success");
        assert!(matches!(
            explicit_money_submit_outcome(&error),
            Some(MoneySubmitError::Ambiguous { .. })
        ));
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert!(reads.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn post_event_read_wrong_or_multiple_event_is_ambiguous_and_never_reposts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        async fn assert_ambiguous(
            observed: Result<TokenContractSettlementReceipts>,
            expected_error: &str,
        ) {
            let posts = Arc::new(AtomicUsize::new(0));
            let posts_for_submit = posts.clone();
            let pre = test_deal_snapshot("pre", true, false, 0, 20);
            let mut observed = Some(observed);
            let error = reconcile_settlement_action_after_post(
                "0:tc",
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::StreamStopped,
                Some(&format!("0:{}", "44".repeat(32))),
                &TokenContractSettlementReceipts::default(),
                &pre,
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(5),
                move || async move {
                    posts_for_submit.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                move || {
                    let observed = observed
                        .take()
                        .expect("failure must stop after one event read");
                    async move { observed }
                },
                || async { Ok(None) },
            )
            .await
            .expect_err("post-submit fact failure cannot be a successful receipt");
            assert!(matches!(
                explicit_money_submit_outcome(&error),
                Some(MoneySubmitError::Ambiguous { .. })
            ));
            assert_eq!(posts.load(Ordering::SeqCst), 1);
            assert!(
                format!("{error:#}").contains(expected_error),
                "missing {expected_error:?} in {error:#}"
            );
        }

        assert_ambiguous(
            Err(anyhow!("malformed TokenContract ext-out")),
            "event read/decode failed",
        )
        .await;
        assert_ambiguous(
            Ok(TokenContractSettlementReceipts {
                events: vec![test_action_receipt(
                    "wrong",
                    1,
                    TokenContractSettlementEvent::StreamDisputed {
                        buyer: "0:buyer".to_string(),
                        at: 1,
                    },
                )],
            }),
            "incompatible",
        )
        .await;
        assert_ambiguous(
            Ok(TokenContractSettlementReceipts {
                events: vec![
                    test_action_receipt(
                        "first",
                        1,
                        TokenContractSettlementEvent::StreamStopped {
                            buyer: "0:buyer".to_string(),
                            to_seller: 1,
                            refund_to_buyer: 2,
                        },
                    ),
                    test_action_receipt(
                        "second",
                        2,
                        TokenContractSettlementEvent::StreamStopped {
                            buyer: "0:buyer".to_string(),
                            to_seller: 1,
                            refund_to_buyer: 2,
                        },
                    ),
                ],
            }),
            "2 distinct new action events",
        )
        .await;
    }

    #[tokio::test]
    async fn wrong_buyer_actor_never_becomes_success_or_reaches_post_state_read() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let buyer = format!("0:{}", "44".repeat(32));
        let wrong = format!("0:{}", "55".repeat(32));
        let cases = [
            (
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::ProbeBurned,
                TokenContractSettlementEvent::ProbeBurned {
                    buyer: wrong.clone(),
                    burned_probe: 1,
                    burned_bond: 2,
                    refund_to_buyer: 3,
                },
            ),
            (
                SettlementAction::BuyerStop,
                ExpectedSettlementEvent::StreamStopped,
                TokenContractSettlementEvent::StreamStopped {
                    buyer: wrong.clone(),
                    to_seller: 4,
                    refund_to_buyer: 5,
                },
            ),
            (
                SettlementAction::SellerStop,
                ExpectedSettlementEvent::StreamStopped,
                TokenContractSettlementEvent::StreamStopped {
                    buyer: wrong.clone(),
                    to_seller: 6,
                    refund_to_buyer: 7,
                },
            ),
            (
                SettlementAction::Dispute,
                ExpectedSettlementEvent::StreamDisputed,
                TokenContractSettlementEvent::StreamDisputed {
                    buyer: wrong,
                    at: 8,
                },
            ),
        ];

        for (action, expected, event) in cases {
            let observed = TokenContractSettlementReceipts {
                events: vec![test_action_receipt("wrong-actor", 1, event)],
            };
            let observed_for_read = observed.clone();
            let post_reads = Arc::new(AtomicUsize::new(0));
            let post_reads_for_read = post_reads.clone();
            let pre = test_deal_snapshot("pre", true, false, 0, 20);
            let error = reconcile_settlement_action_after_post(
                "0:tc",
                action,
                expected,
                Some(&buyer),
                &TokenContractSettlementReceipts::default(),
                &pre,
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(5),
                || async { Ok(()) },
                move || {
                    let observed = observed_for_read.clone();
                    async move { Ok(observed) }
                },
                move || async move {
                    post_reads_for_read.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                },
            )
            .await
            .expect_err("wrong actor cannot produce an authoritative receipt");
            assert!(matches!(
                explicit_money_submit_outcome(&error),
                Some(MoneySubmitError::Ambiguous { .. })
            ));
            assert!(format!("{error:#}").contains("wrong buyer actor"));
            assert_eq!(
                post_reads.load(Ordering::SeqCst),
                0,
                "{action}: selector must fail before any post-state read"
            );
        }
    }

    #[tokio::test]
    async fn terminal_post_state_contradiction_is_ambiguous_and_never_reposts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let posts = Arc::new(AtomicUsize::new(0));
        let posts_for_submit = posts.clone();
        let pre = test_deal_snapshot("pre", true, false, 0, 20);
        let mut contradicted = test_deal_snapshot("post", false, false, 7, 0);
        contradicted.state.deposit = 1;
        let observed = TokenContractSettlementReceipts {
            events: vec![test_action_receipt(
                "stop",
                5,
                TokenContractSettlementEvent::StreamStopped {
                    buyer: format!("0:{}", "44".repeat(32)),
                    to_seller: 7,
                    refund_to_buyer: 8,
                },
            )],
        };
        let observed_for_read = observed.clone();

        let error = reconcile_settlement_action_after_post(
            "0:tc",
            SettlementAction::BuyerStop,
            ExpectedSettlementEvent::StreamStopped,
            Some(&format!("0:{}", "44".repeat(32))),
            &TokenContractSettlementReceipts::default(),
            &pre,
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
            move || async move {
                posts_for_submit.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            move || {
                let observed = observed_for_read.clone();
                async move { Ok(observed) }
            },
            move || async move { Ok(Some(contradicted)) },
        )
        .await
        .expect_err("active terminal account with retained deposit must fail closed");
        assert!(matches!(
            explicit_money_submit_outcome(&error),
            Some(MoneySubmitError::Ambiguous { .. })
        ));
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert!(format!("{error:#}").contains("terminal event contradicts"));
    }

    #[tokio::test]
    async fn post_event_getter_contradiction_is_ambiguous_and_never_reposts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let posts = Arc::new(AtomicUsize::new(0));
        let posts_for_submit = posts.clone();
        let pre = test_deal_snapshot_with_buyer_bond("pre", true, false, 17, 20, 20, 20);
        let mut contradicted =
            test_deal_snapshot_with_buyer_bond("post", true, true, 17, 20, 20, 20);
        contradicted.buyer_bond.bond_held = 19;
        let observed = TokenContractSettlementReceipts {
            events: vec![test_action_receipt(
                "dispute",
                5,
                TokenContractSettlementEvent::StreamDisputed {
                    buyer: format!("0:{}", "44".repeat(32)),
                    at: 5,
                },
            )],
        };
        let observed_for_read = observed.clone();

        let error = reconcile_settlement_action_after_post(
            "0:tc",
            SettlementAction::Dispute,
            ExpectedSettlementEvent::StreamDisputed,
            Some(&format!("0:{}", "44".repeat(32))),
            &TokenContractSettlementReceipts::default(),
            &pre,
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
            move || async move {
                posts_for_submit.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            move || {
                let observed = observed_for_read.clone();
                async move { Ok(observed) }
            },
            move || async move { Ok(Some(contradicted)) },
        )
        .await
        .expect_err("changed subscription buyer bond must fail closed");
        assert!(matches!(
            explicit_money_submit_outcome(&error),
            Some(MoneySubmitError::Ambiguous { .. })
        ));
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert!(format!("{error:#}").contains("only disputed/disputeTime"));
    }

    #[test]
    fn terminal_destroy_keeps_event_receipt_but_dispute_requires_active_snapshot() {
        let pre = test_deal_snapshot("pre", true, false, 0, 20);
        let stopped = TokenContractSettlementEvent::StreamStopped {
            buyer: "0:buyer".to_string(),
            to_seller: 7,
            refund_to_buyer: 8,
        };
        let mut receipt = select_new_settlement_action_receipt(
            "0:tc",
            SettlementAction::BuyerStop,
            ExpectedSettlementEvent::StreamStopped,
            Some(&format!("0:{}", "44".repeat(32))),
            &TokenContractSettlementReceipts {
                events: vec![test_action_receipt(
                    "stop",
                    1,
                    TokenContractSettlementEvent::StreamStopped {
                        buyer: format!("0:{}", "44".repeat(32)),
                        to_seller: 7,
                        refund_to_buyer: 8,
                    },
                )],
            },
            test_pre_bonds(),
        )
        .unwrap()
        .unwrap();
        attach_settlement_post_snapshot("0:tc", &mut receipt, &pre, &stopped, None)
            .expect("terminal event remains authoritative after account destruction");
        assert_eq!(receipt.post_state, None);
        assert!(matches!(
            receipt.event,
            SettlementActionEvent::StreamStopped { .. }
        ));

        let disputed = TokenContractSettlementEvent::StreamDisputed {
            buyer: "0:buyer".to_string(),
            at: 5,
        };
        let error = attach_settlement_post_snapshot("0:tc", &mut receipt, &pre, &disputed, None)
            .expect_err("non-terminal dispute cannot destroy its TokenContract");
        assert!(error.to_string().contains("inactive after non-terminal"));
    }

    #[test]
    fn explicit_endpoint_flag_overrides_default() {
        let endpoint = resolve_endpoint(Some("some-host"), &deployed("")).unwrap();
        assert_eq!(
            endpoint_urls(&endpoint).unwrap(),
            (
                "https://some-host/graphql".into(),
                "https://some-host/v2/account".into(),
            )
        );
    }

    #[test]
    fn manifest_graphql_field_supplies_endpoint() {
        let manifest = deployed(r#", "graphql": "https://manifest-host/graphql/""#);
        assert_eq!(
            resolve_endpoint(None, &manifest).unwrap(),
            "https://manifest-host"
        );
        assert_eq!(
            resolve_endpoint(Some("explicit-host"), &manifest).unwrap(),
            "https://explicit-host"
        );
    }

    #[test]
    fn endpoint_url_normalization() {
        let expected = (
            "https://host/graphql".to_string(),
            "https://host/v2/account".to_string(),
        );
        for endpoint in ["host", "https://host", "https://host/"] {
            assert_eq!(endpoint_urls(endpoint).unwrap(), expected);
        }
    }

    fn fill(token_contract: &str, ticks: u128, price_per_tick: u128) -> MatchedFill {
        MatchedFill {
            order_id: 1,
            token_contract: token_contract.to_string(),
            ticks,
            price_per_tick,
        }
    }

    struct CountingFillSource {
        batches: Mutex<VecDeque<Vec<(i64, MatchedFill)>>>,
    }

    impl CountingFillSource {
        fn new(batches: Vec<Vec<(i64, MatchedFill)>>) -> Self {
            Self {
                batches: Mutex::new(batches.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl InferenceFillPoller for CountingFillSource {
        async fn poll(&self, cursor: &mut MatchWatchCursor) -> Result<Vec<MatchedFill>> {
            let batch = self
                .batches
                .lock()
                .expect("fill batches lock")
                .pop_front()
                .unwrap_or_default();
            Ok(consume_new_fill_batch(cursor, batch))
        }
    }

    async fn wait_for_test_fill(
        source: &CountingFillSource,
        cursor: &mut MatchWatchCursor,
        expected: &MatchedFill,
    ) -> Result<MatchedFill> {
        wait_correlated_inference_fill(
            source,
            cursor,
            Some(expected),
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            "test fill timeout",
        )
        .await
    }

    fn money_submit_stage(error: &anyhow::Error) -> &MoneySubmitError {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<MoneySubmitError>())
            .expect("stage-aware money submit error")
    }

    async fn serve_money_post_response(
        status: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind money POST fixture");
        let address = listener.local_addr().expect("money POST fixture address");
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept money POST");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read money POST");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write money POST response");
        });
        (format!("http://{address}"), task)
    }

    async fn serve_counted_money_post_response(
        status: &str,
        body: &str,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind counted money POST fixture");
        let address = listener
            .local_addr()
            .expect("counted money POST fixture address");
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let posts = Arc::new(AtomicUsize::new(0));
        let task_posts = Arc::clone(&posts);
        let task = tokio::spawn(async move {
            loop {
                let wait = if task_posts.load(Ordering::SeqCst) == 0 {
                    std::time::Duration::from_secs(1)
                } else {
                    std::time::Duration::from_millis(150)
                };
                let Ok(Ok((mut socket, _))) = tokio::time::timeout(wait, listener.accept()).await
                else {
                    break;
                };
                task_posts.fetch_add(1, Ordering::SeqCst);
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await.expect("read money POST");
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write counted money POST response");
            }
        });
        (format!("http://{address}"), posts, task)
    }

    #[tokio::test]
    async fn money_post_outcomes_only_clear_for_preparation_or_decoded_rejection() {
        let account = "1".repeat(64);
        let client = build_money_post_http_client().expect("money POST client");

        for status in ["408 Request Timeout", "409 Conflict"] {
            let (endpoint, task) =
                serve_money_post_response(status, r#"{"error":"fixture"}"#).await;
            let error = send_message_routed_money_once(
                &client,
                &endpoint,
                "signed-boc",
                &account,
                &account,
            )
            .await
            .expect_err("unvalidated HTTP status must be ambiguous");
            assert!(matches!(
                money_submit_stage(&error),
                MoneySubmitError::Ambiguous { .. }
            ));
            assert!(!money_submit_stage(&error).clears_journal());
            task.await.expect("money POST fixture task");
        }

        let (endpoint, task) = serve_money_post_response("200 OK", "not-json").await;
        let error =
            send_message_routed_money_once(&client, &endpoint, "signed-boc", &account, &account)
                .await
                .expect_err("undecodable response must be ambiguous");
        assert!(matches!(
            money_submit_stage(&error),
            MoneySubmitError::Ambiguous { .. }
        ));
        assert!(!money_submit_stage(&error).clears_journal());
        task.await.expect("invalid-body fixture task");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind transport-after-send fixture");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept money POST");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read money POST");
            drop(socket);
        });
        let error =
            send_message_routed_money_once(&client, &endpoint, "signed-boc", &account, &account)
                .await
                .expect_err("transport failure after send must be ambiguous");
        assert!(matches!(
            money_submit_stage(&error),
            MoneySubmitError::Ambiguous { .. }
        ));
        assert!(!money_submit_stage(&error).clears_journal());
        task.await.expect("transport-after-send fixture task");

        let (endpoint, task) =
            serve_money_post_response("200 OK", r#"{"result":{"exit_code":151}}"#).await;
        let error =
            send_message_routed_money_once(&client, &endpoint, "signed-boc", &account, &account)
                .await
                .expect_err("decoded contract rejection must be terminal");
        assert!(matches!(
            money_submit_stage(&error),
            MoneySubmitError::Rejected { .. }
        ));
        assert!(money_submit_stage(&error).clears_journal());
        assert!(format!("{error:#}").contains("exit_code=151"));
        task.await.expect("contract rejection fixture task");

        let error = send_message_routed_money_once(
            &client,
            "not a valid URL",
            "signed-boc",
            &account,
            &account,
        )
        .await
        .expect_err("request builder failure must be pre-POST");
        assert!(matches!(
            money_submit_stage(&error),
            MoneySubmitError::Preparation { .. }
        ));
        assert!(money_submit_stage(&error).clears_journal());
    }

    #[tokio::test]
    async fn explicit_stop_posts_once_for_gateway_queue_and_ambiguous_outcomes() {
        let account = "1".repeat(64);
        let client = build_money_post_http_client().expect("money POST client");
        for (status, body) in [
            ("502 Bad Gateway", r#"{"error":"gateway"}"#),
            ("503 Service Unavailable", r#"{"error":"service"}"#),
            ("504 Gateway Timeout", r#"{"error":"timeout"}"#),
            (
                "200 OK",
                r#"{"error":"QUEUE_OVERFLOW: message queue is full"}"#,
            ),
            ("200 OK", "not-json"),
        ] {
            let (endpoint, posts, task) = serve_counted_money_post_response(status, body).await;
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                send_explicit_stop_money_once(&client, &endpoint, "signed-boc", &account, &account),
            )
            .await
            .expect("explicit STOP must not enter a retry/backoff loop")
            .expect_err("fixture response is not a confirmed submit");
            assert!(
                matches!(
                    money_submit_stage(&error),
                    MoneySubmitError::Ambiguous { .. }
                ),
                "{status}: {error:#}"
            );
            task.await.expect("counted money POST fixture task");
            assert_eq!(
                posts.load(Ordering::SeqCst),
                1,
                "{status} must not resend the signed STOP BOC"
            );
        }
    }

    #[test]
    fn explicit_stream_stop_uses_the_authoritative_one_shot_receipt_path() {
        let source = include_str!("client.rs");
        let start = source
            .find("pub async fn stream_stop(")
            .expect("explicit stream_stop implementation");
        let end = source[start..]
            .find("pub async fn stream_dispute(")
            .map(|offset| start + offset)
            .expect("method after explicit stream_stop");
        let body = &source[start..end];
        let compact_body: String = body.chars().filter(|ch| !ch.is_whitespace()).collect();

        assert!(body.contains(".prepare_money_post("));
        assert!(compact_body.contains("self.submit_settlement_action_once("));
        assert!(body.contains("SettlementAction::BuyerStop"));
        assert!(body.contains("ExpectedSettlementEvent::BuyerStop"));
        assert!(!body.contains("send_explicit_stop_money_once("));
        assert!(!body.contains("self.submit("));
        assert!(!body.contains("send_with_retry("));
    }

    #[test]
    fn restart_after_exact_stream_stopped_is_an_idempotent_no_post() {
        let receipts = TokenContractSettlementReceipts {
            events: vec![TokenContractSettlementReceipt {
                message_id: "receipt-stop".to_string(),
                created_at: 77,
                cursor: "cursor-stop".to_string(),
                event: TokenContractSettlementEvent::StreamStopped {
                    buyer: "0:buyer".to_string(),
                    to_seller: 10,
                    refund_to_buyer: 90,
                },
            }],
        };
        let closed = test_deal_snapshot("closed", false, false, 10, 0);
        let mut simulated_posts = 0;

        for pre in [Some(&closed), None] {
            let result = validate_buyer_stop_pre_state("0:tc", pre, &receipts);
            if result.is_ok() {
                simulated_posts += 1;
            }
            let message = result.expect_err("terminal retry must not reach the money POST");
            let message = message.to_string();
            assert!(message.contains("exact StreamStopped receipt"), "{message}");
            assert!(message.contains("message_id=receipt-stop"), "{message}");
            assert!(message.contains("idempotent no-op"), "{message}");
        }
        assert_eq!(
            simulated_posts, 0,
            "active-closed and destroyed restart retries must issue no money POST"
        );

        let live = test_deal_snapshot("live", true, false, 0, 20);
        validate_buyer_stop_pre_state(
            "0:tc",
            Some(&live),
            &TokenContractSettlementReceipts::default(),
        )
        .expect("one open undisputed stream may proceed to its first STOP");
    }

    #[test]
    fn prior_stream_stopped_is_recorded_as_already_closed_without_a_submit() {
        let buyer = format!("0:{}", "44".repeat(32));
        let receipt = select_prior_buyer_terminal_receipt(
            "0:tc",
            &buyer,
            &TokenContractSettlementReceipts {
                events: vec![TokenContractSettlementReceipt {
                    message_id: "permissionless-finalize".to_string(),
                    created_at: 81,
                    cursor: "cursor-finalize".to_string(),
                    event: TokenContractSettlementEvent::StreamStopped {
                        buyer: buyer.clone(),
                        to_seller: 10,
                        refund_to_buyer: 90,
                    },
                }],
            },
        )
        .expect("valid prior terminal history")
        .expect("terminal observation");

        assert_eq!(
            receipt.fact,
            crate::market::BuyerStopTerminalFact::AlreadyClosed
        );
        assert!(!receipt.stop_submitted);
        assert_eq!(receipt.message_id, "permissionless-finalize");
    }

    #[test]
    fn submitted_stream_stop_message_is_bound_to_the_exact_client_transaction() {
        let buyer = format!("0:{}", "44".repeat(32));
        let response = json!({
            "data": {
                "blockchain": {
                    "message": {
                        "id": "client-stream-stop",
                        "dst": buyer,
                        "dst_transaction": {
                            "status": 3,
                            "aborted": false,
                            "out_msgs": [
                                "unrelated-ensure-balance",
                                "our-internal-stop"
                            ]
                        }
                    }
                }
            }
        });

        assert_eq!(
            parse_submitted_buyer_stop_out_message_ids(&response, "client-stream-stop", &buyer,)
                .expect("exact submitted streamStop trace"),
            Some(vec![
                "unrelated-ensure-balance".to_string(),
                "our-internal-stop".to_string(),
            ])
        );
    }

    #[test]
    fn submitted_stop_graphql_query_matches_the_response_shape_parser_consumes() {
        for field in [
            "message(hash: $hash)",
            "id dst",
            "dst_transaction",
            "status aborted",
            "out_msgs",
        ] {
            assert!(
                SUBMITTED_BUYER_STOP_QUERY.contains(field),
                "submitted STOP query must request {field}"
            );
        }
    }

    #[test]
    fn destroyed_prior_terminal_receipt_wins_before_state_actor_or_post() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let receipts = TokenContractSettlementReceipts {
            events: vec![TokenContractSettlementReceipt {
                message_id: "destroyed-stop".to_string(),
                created_at: 77,
                cursor: "destroyed-cursor".to_string(),
                event: TokenContractSettlementEvent::StreamStopped {
                    buyer: "0:buyer".to_string(),
                    to_seller: 10,
                    refund_to_buyer: 90,
                },
            }],
        };
        let state_reads = AtomicUsize::new(0);
        let actor_reads = AtomicUsize::new(0);
        let posts = AtomicUsize::new(0);
        let result = (|| -> Result<()> {
            reject_prior_settlement_action("0:tc", SettlementAction::BuyerStop, None, &receipts)?;
            state_reads.fetch_add(1, Ordering::SeqCst);
            actor_reads.fetch_add(1, Ordering::SeqCst);
            posts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })();
        let message = result
            .expect_err("destroyed terminal retry must stop at immutable history")
            .to_string();
        assert!(message.contains("exact StreamStopped receipt"), "{message}");
        assert_eq!(state_reads.load(Ordering::SeqCst), 0);
        assert_eq!(actor_reads.load(Ordering::SeqCst), 0);
        assert_eq!(posts.load(Ordering::SeqCst), 0);

        let source = include_str!("client.rs");
        let start = source
            .find("async fn submit_settlement_action_once_if(")
            .expect("settlement submit helper");
        let end = source[start..]
            .find("/// Read-only buyer preflight")
            .map(|offset| start + offset)
            .expect("method after settlement submit helper");
        let body = &source[start..end];
        let history = body
            .find("self.token_contract_settlement_receipts(")
            .expect("immutable event snapshot");
        let terminal = body
            .find("reject_prior_settlement_action(")
            .expect("prior terminal guard");
        let state = body
            .find("self.token_contract_deal_snapshot(")
            .expect("live state getter");
        let actor = body
            .find("self.token_contract_buyer_note(")
            .expect("live actor getter");
        let post = body.find("if !before_post()").expect("only POST guard");
        assert!(history < terminal && terminal < state && state < actor && actor < post);
    }

    #[test]
    fn every_settlement_action_retry_is_classified_before_getters_or_second_money() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let buyer = format!("0:{}", "44".repeat(32));
        let disputed = || TokenContractSettlementEvent::StreamDisputed {
            buyer: buyer.clone(),
            at: 70,
        };
        let exact_cases = vec![
            (
                SettlementAction::BuyerStop,
                vec![TokenContractSettlementEvent::StreamStopped {
                    buyer: buyer.clone(),
                    to_seller: 10,
                    refund_to_buyer: 90,
                }],
                "StreamStopped",
            ),
            (
                SettlementAction::SellerStop,
                vec![TokenContractSettlementEvent::StreamStopped {
                    buyer: buyer.clone(),
                    to_seller: 10,
                    refund_to_buyer: 90,
                }],
                "StreamStopped",
            ),
            (
                SettlementAction::Dispute,
                vec![disputed()],
                "StreamDisputed",
            ),
            (
                SettlementAction::ReleaseDispute,
                vec![
                    disputed(),
                    TokenContractSettlementEvent::DisputeResolved {
                        to_seller: 10,
                        refund_to_buyer: 90,
                        released: true,
                    },
                ],
                "DisputeResolved(released=true)",
            ),
            (
                SettlementAction::ResolveDisputeTimeout,
                vec![
                    disputed(),
                    TokenContractSettlementEvent::DisputeResolved {
                        to_seller: 10,
                        refund_to_buyer: 90,
                        released: false,
                    },
                ],
                "DisputeResolved(released=false)",
            ),
        ];

        for (action, events, expected_kind) in exact_cases {
            let receipts = TokenContractSettlementReceipts {
                events: events
                    .into_iter()
                    .enumerate()
                    .map(|(index, event)| {
                        test_action_receipt(
                            &format!("prior-{action}-{index}"),
                            77 + index as u64,
                            event,
                        )
                    })
                    .collect(),
            };
            let state_reads = AtomicUsize::new(0);
            let actor_reads = AtomicUsize::new(0);
            let prepares = AtomicUsize::new(0);
            let posts = AtomicUsize::new(0);
            let expected_buyer = matches!(
                action,
                SettlementAction::BuyerStop | SettlementAction::Dispute
            )
            .then_some(buyer.as_str());
            let result = (|| -> Result<()> {
                reject_prior_settlement_action("0:tc", action, expected_buyer, &receipts)?;
                state_reads.fetch_add(1, Ordering::SeqCst);
                actor_reads.fetch_add(1, Ordering::SeqCst);
                prepares.fetch_add(1, Ordering::SeqCst);
                posts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })();
            let message = result
                .expect_err("an exact prior action must classify the retry without live state")
                .to_string();
            assert!(message.contains(expected_kind), "{action}: {message}");
            assert!(message.contains("idempotent no-op"), "{action}: {message}");
            assert_eq!(state_reads.load(Ordering::SeqCst), 0, "{action}");
            assert_eq!(actor_reads.load(Ordering::SeqCst), 0, "{action}");
            assert_eq!(prepares.load(Ordering::SeqCst), 0, "{action}");
            assert_eq!(posts.load(Ordering::SeqCst), 0, "{action}");
        }
    }

    #[test]
    fn every_settlement_action_rejects_incompatible_history_before_second_money() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let buyer = format!("0:{}", "44".repeat(32));
        let disputed = || TokenContractSettlementEvent::StreamDisputed {
            buyer: buyer.clone(),
            at: 70,
        };
        let mismatch_cases = vec![
            (SettlementAction::BuyerStop, vec![disputed()]),
            (
                SettlementAction::SellerStop,
                vec![TokenContractSettlementEvent::ProbeBurned {
                    buyer: buyer.clone(),
                    burned_probe: 1,
                    burned_bond: 1,
                    refund_to_buyer: 98,
                }],
            ),
            (
                SettlementAction::Dispute,
                vec![TokenContractSettlementEvent::StreamStopped {
                    buyer: buyer.clone(),
                    to_seller: 10,
                    refund_to_buyer: 90,
                }],
            ),
            (
                SettlementAction::ReleaseDispute,
                vec![
                    disputed(),
                    TokenContractSettlementEvent::DisputeResolved {
                        to_seller: 10,
                        refund_to_buyer: 90,
                        released: false,
                    },
                ],
            ),
            (
                SettlementAction::ResolveDisputeTimeout,
                vec![
                    disputed(),
                    TokenContractSettlementEvent::DisputeResolved {
                        to_seller: 10,
                        refund_to_buyer: 90,
                        released: true,
                    },
                ],
            ),
        ];

        for (action, events) in mismatch_cases {
            let receipts = TokenContractSettlementReceipts {
                events: events
                    .into_iter()
                    .enumerate()
                    .map(|(index, event)| {
                        test_action_receipt(
                            &format!("wrong-{action}-{index}"),
                            77 + index as u64,
                            event,
                        )
                    })
                    .collect(),
            };
            let prepares = AtomicUsize::new(0);
            let posts = AtomicUsize::new(0);
            let result = (|| -> Result<()> {
                reject_prior_settlement_action("0:tc", action, None, &receipts)?;
                prepares.fetch_add(1, Ordering::SeqCst);
                posts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })();
            let message = result
                .expect_err("incompatible old action history must fail closed")
                .to_string();
            assert!(
                message.contains("incompatible prior"),
                "{action}: {message}"
            );
            assert!(
                message.contains("before any money POST"),
                "{action}: {message}"
            );
            assert_eq!(prepares.load(Ordering::SeqCst), 0, "{action}");
            assert_eq!(posts.load(Ordering::SeqCst), 0, "{action}");
        }
    }

    #[test]
    fn dispute_resolution_replays_require_canonical_event_order_and_shape() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let buyer = format!("0:{}", "44".repeat(32));
        for action in [
            SettlementAction::ReleaseDispute,
            SettlementAction::ResolveDisputeTimeout,
        ] {
            let released = action == SettlementAction::ReleaseDispute;
            let dispute = || TokenContractSettlementEvent::StreamDisputed {
                buyer: buyer.clone(),
                at: 70,
            };
            let resolution = || TokenContractSettlementEvent::DisputeResolved {
                to_seller: 10,
                refund_to_buyer: 90,
                released,
            };
            let stop = || TokenContractSettlementEvent::StreamStopped {
                buyer: buyer.clone(),
                to_seller: 10,
                refund_to_buyer: 90,
            };
            for (label, events) in [
                ("lone resolution", vec![resolution()]),
                ("reversed order", vec![resolution(), dispute()]),
                ("extra terminal", vec![dispute(), resolution(), stop()]),
            ] {
                let receipts = TokenContractSettlementReceipts {
                    events: events
                        .into_iter()
                        .enumerate()
                        .map(|(index, event)| {
                            test_action_receipt(
                                &format!("tampered-{action}-{index}"),
                                77 + index as u64,
                                event,
                            )
                        })
                        .collect(),
                };
                let state_reads = AtomicUsize::new(0);
                let prepares = AtomicUsize::new(0);
                let posts = AtomicUsize::new(0);
                let result = (|| -> Result<()> {
                    reject_prior_settlement_action("0:tc", action, None, &receipts)?;
                    state_reads.fetch_add(1, Ordering::SeqCst);
                    prepares.fetch_add(1, Ordering::SeqCst);
                    posts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })();
                let message = result
                    .expect_err("tampered resolution history must fail before live state or money")
                    .to_string();
                assert!(
                    message.contains("invalid prior settlement-action history"),
                    "{action}/{label}: {message}"
                );
                assert!(
                    message.contains("canonical chain order"),
                    "{action}/{label}: {message}"
                );
                assert_eq!(state_reads.load(Ordering::SeqCst), 0, "{action}/{label}");
                assert_eq!(prepares.load(Ordering::SeqCst), 0, "{action}/{label}");
                assert_eq!(posts.load(Ordering::SeqCst), 0, "{action}/{label}");
            }
        }
    }

    #[test]
    fn every_public_settlement_action_checks_history_before_money_preparation() {
        let source = include_str!("client.rs");
        for (method, next_method) in [
            (
                "pub async fn seller_stop(",
                "pub async fn destroy_token_contract(",
            ),
            (
                "pub async fn release_dispute(",
                "pub async fn resolve_dispute_timeout(",
            ),
            (
                "pub async fn resolve_dispute_timeout(",
                "pub async fn withdraw_shell(",
            ),
            ("pub async fn stream_stop(", "pub async fn stream_dispute("),
            (
                "pub async fn stream_dispute(",
                "pub async fn stop_if_heartbeat(",
            ),
            (
                "pub async fn stop_if_heartbeat(",
                "pub async fn stream_cleanup(",
            ),
        ] {
            let start = source
                .find(method)
                .unwrap_or_else(|| panic!("missing {method}"));
            let end = source[start..]
                .find(next_method)
                .map(|offset| start + offset)
                .unwrap_or_else(|| panic!("missing {next_method}"));
            let body = &source[start..end];
            let history = body
                .find("reject_prior_settlement_action_before_prepare(")
                .unwrap_or_else(|| panic!("{method} lacks immutable-history preflight"));
            let prepare = body
                .find(".prepare_money_post(")
                .unwrap_or_else(|| panic!("{method} lacks money preparation"));
            assert!(
                history < prepare,
                "{method} prepares money before replay classification"
            );
        }
    }

    #[test]
    fn stale_open_getter_never_overrides_prior_terminal_receipt() {
        let live = test_deal_snapshot("stale-open", true, false, 0, 20);
        let terminal_events = [
            (
                "ProbeBurned",
                TokenContractSettlementEvent::ProbeBurned {
                    buyer: "0:buyer".to_string(),
                    burned_probe: 1,
                    burned_bond: 1,
                    refund_to_buyer: 98,
                },
            ),
            (
                "StreamStopped",
                TokenContractSettlementEvent::StreamStopped {
                    buyer: "0:buyer".to_string(),
                    to_seller: 10,
                    refund_to_buyer: 90,
                },
            ),
            (
                "DisputeResolved",
                TokenContractSettlementEvent::DisputeResolved {
                    to_seller: 10,
                    refund_to_buyer: 90,
                    released: true,
                },
            ),
        ];
        let mut simulated_posts = 0;

        for (kind, event) in terminal_events {
            let receipts = TokenContractSettlementReceipts {
                events: vec![TokenContractSettlementReceipt {
                    message_id: format!("receipt-{kind}"),
                    created_at: 77,
                    cursor: format!("cursor-{kind}"),
                    event,
                }],
            };
            let result = validate_buyer_stop_pre_state("0:tc", Some(&live), &receipts);
            if result.is_ok() {
                simulated_posts += 1;
            }
            let message =
                result.expect_err("immutable terminal history must beat stale-open state");
            assert!(message.to_string().contains(kind), "{message}");
        }
        assert_eq!(
            simulated_posts, 0,
            "no prior terminal receipt may reach a duplicate STOP POST"
        );
    }

    #[test]
    fn probe_stop_restart_is_an_idempotent_no_post() {
        let receipts = TokenContractSettlementReceipts {
            events: vec![TokenContractSettlementReceipt {
                message_id: "receipt-probe-burn".to_string(),
                created_at: 77,
                cursor: "cursor-probe-burn".to_string(),
                event: TokenContractSettlementEvent::ProbeBurned {
                    buyer: "0:buyer".to_string(),
                    burned_probe: 1,
                    burned_bond: 1,
                    refund_to_buyer: 98,
                },
            }],
        };
        let closed = test_deal_snapshot("closed-probe", false, false, 0, 0);
        for pre in [Some(&closed), None] {
            let message = validate_buyer_stop_pre_state("0:tc", pre, &receipts)
                .expect_err("probe STOP retry must not reach a second POST")
                .to_string();
            assert!(message.contains("exact ProbeBurned receipt"), "{message}");
            assert!(message.contains("idempotent no-op"), "{message}");
        }
    }

    #[test]
    fn illegal_buyer_stop_state_without_exact_receipt_fails_before_post() {
        for (label, pre) in [
            (
                "closed without receipt",
                Some(test_deal_snapshot("closed", false, false, 10, 0)),
            ),
            (
                "disputed",
                Some(test_deal_snapshot("disputed", true, true, 0, 20)),
            ),
        ] {
            let error = validate_buyer_stop_pre_state(
                "0:tc",
                pre.as_ref(),
                &TokenContractSettlementReceipts::default(),
            )
            .expect_err(label);
            assert!(
                error.to_string().contains("before any money POST"),
                "{error}"
            );
        }

        let live = test_deal_snapshot("live", true, false, 0, 20);
        let disputed = TokenContractSettlementReceipts {
            events: vec![TokenContractSettlementReceipt {
                message_id: "receipt-disputed".to_string(),
                created_at: 77,
                cursor: "cursor-disputed".to_string(),
                event: TokenContractSettlementEvent::StreamDisputed {
                    buyer: "0:buyer".to_string(),
                    at: 76,
                },
            }],
        };
        let error = validate_buyer_stop_pre_state("0:tc", Some(&live), &disputed)
            .expect_err("prior dispute receipt must beat a stale-undisputed getter");
        assert!(error.to_string().contains("StreamDisputed"), "{error}");

        let mut multiple = disputed;
        multiple.events.push(TokenContractSettlementReceipt {
            message_id: "receipt-resolved".to_string(),
            created_at: 78,
            cursor: "cursor-resolved".to_string(),
            event: TokenContractSettlementEvent::DisputeResolved {
                to_seller: 10,
                refund_to_buyer: 90,
                released: false,
            },
        });
        let error = validate_buyer_stop_pre_state("0:tc", Some(&live), &multiple)
            .expect_err("multiple prior action receipts must fail closed");
        assert!(
            error
                .to_string()
                .contains("more than one prior settlement-action receipt"),
            "{error}"
        );
    }

    #[test]
    fn policy_stop_checks_heartbeat_after_receipt_preflight_and_before_the_only_post() {
        let source = include_str!("client.rs");
        let start = source
            .find("async fn submit_settlement_action_once_if(")
            .expect("guarded settlement helper");
        let end = source[start..]
            .find("/// Read-only buyer preflight")
            .map(|offset| start + offset)
            .expect("method after guarded settlement helper");
        let body = &source[start..end];
        let legality = body
            .find("validate_buyer_stop_pre_state(")
            .expect("strict buyer STOP legality gate");
        let pre_snapshot = body
            .find("validate_settlement_facts(")
            .expect("authoritative pre-submit validation");
        let guard = body
            .find("if !before_post()")
            .expect("final heartbeat guard");
        let reconcile = body
            .find("reconcile_settlement_action_after_post(")
            .expect("one-shot receipt reconciliation");
        let after_guard = &body[guard..reconcile];

        assert!(legality < pre_snapshot && pre_snapshot < guard && guard < reconcile);
        assert!(
            !after_guard.contains(".await"),
            "no async gap may reopen the heartbeat race before the only POST"
        );
        assert!(body.contains("send_explicit_stop_money_once("));
    }

    #[tokio::test]
    async fn policy_stop_heartbeat_change_after_prepare_skips_money_post() {
        use std::sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc,
        };

        let generation = Arc::new(AtomicU64::new(7));
        let heartbeat = crate::market::HeartbeatGuard::new(Arc::clone(&generation));
        let sends = Arc::new(AtomicUsize::new(0));
        let prepare_generation = Arc::clone(&generation);
        let send_counter = Arc::clone(&sends);
        let mut before_post = || heartbeat.unchanged();

        let result = prepare_policy_stop_money_post_if(
            async move {
                prepare_generation.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "endpoint".to_string(),
                    "signed-boc".to_string(),
                    "account".to_string(),
                    "dapp".to_string(),
                ))
            },
            &mut before_post,
            move |_| async move {
                send_counter.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"ok": true}))
            },
        )
        .await
        .expect("changed heartbeat must cancel without a send");

        assert!(result.is_none());
        assert_eq!(sends.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn wait_reconciliation_logic_ignores_stale_rejects_wrong_and_selects_intended() {
        let expected = fill("0:intended", 2, 700);
        let stale = fill("0:stale", 9, 999);
        let unrelated = fill("0:unrelated", 3, 701);

        let wrong_source = CountingFillSource::new(vec![
            vec![(100, stale.clone())],
            vec![(100, stale.clone()), (101, unrelated.clone())],
        ]);
        let mut wrong_cursor = MatchWatchCursor::new(0);
        wrong_source
            .poll(&mut wrong_cursor)
            .await
            .expect("prime cursor past stale fill");
        let error = wait_for_test_fill(&wrong_source, &mut wrong_cursor, &expected)
            .await
            .expect_err("post-submit unrelated fill must fail closed");
        assert!(error
            .to_string()
            .contains("refusing wrong-fill attribution"));

        let intended_source = CountingFillSource::new(vec![
            vec![(100, stale.clone())],
            vec![(100, stale), (101, unrelated), (101, expected.clone())],
        ]);
        let mut intended_cursor = MatchWatchCursor::new(0);
        intended_source
            .poll(&mut intended_cursor)
            .await
            .expect("prime cursor past stale fill");
        let selected = wait_for_test_fill(&intended_source, &mut intended_cursor, &expected)
            .await
            .expect("intended fill must let the deal proceed");
        assert_eq!(selected, expected);
    }

    #[tokio::test]
    async fn money_post_refuses_307_without_replaying_signed_boc() {
        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirect server");
        let redirect_addr = redirect_listener.local_addr().expect("redirect address");
        let redirect_task = tokio::spawn(async move {
            let (mut socket, _) = redirect_listener.accept().await.expect("redirect request");
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.expect("read money POST");
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{redirect_addr}/replayed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write redirect");
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                redirect_listener.accept(),
            )
            .await
            {
                Ok(Ok((mut replay, _))) => {
                    let _ = replay.read(&mut request).await.expect("read replayed POST");
                    replay
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
                        )
                        .await
                        .expect("write replay response");
                    true
                }
                _ => false,
            }
        });

        let client = build_money_post_http_client().expect("money POST client");
        let error = send_message_routed_checked(
            &client,
            &format!("http://{redirect_addr}"),
            "signed-boc",
            "0:11",
            "0:22",
            None,
        )
        .await
        .expect_err("307 must fail instead of replaying the signed BOC");

        let replayed = redirect_task.await.expect("redirect server task");
        assert!(
            error.to_string().contains("refused HTTP redirect 307"),
            "{error:#}"
        );
        assert!(!replayed, "signed BOC was replayed at redirect target");

        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind money redirect server");
        let redirect_addr = redirect_listener.local_addr().expect("redirect address");
        let redirect_task = tokio::spawn(async move {
            let (mut socket, _) = redirect_listener.accept().await.expect("redirect request");
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.expect("read money POST");
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{redirect_addr}/replayed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write redirect");
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                redirect_listener.accept(),
            )
            .await
            .is_ok()
        });
        let error = send_message_routed_money_once(
            &client,
            &format!("http://{redirect_addr}"),
            "signed-boc",
            "0:11",
            "0:22",
        )
        .await
        .expect_err("money redirect must remain ambiguous");
        assert!(matches!(
            money_submit_stage(&error),
            MoneySubmitError::Ambiguous { .. }
        ));
        assert!(!money_submit_stage(&error).clears_journal());
        assert!(!redirect_task.await.expect("money redirect task"));
    }


    /// an explicit one on the wire -- and per `DEXDO_USER_AGENT` it must be OUR `name/version`
    /// identifier, never a browser impersonation. Observed on the wire, not read back off the
    /// builder, because only the sent header is what the edge sees.
    #[tokio::test]
    async fn chain_http_clients_send_our_own_user_agent() {
        /// Serve exactly one request with `body`, and hand back its raw request head.
        async fn capture_one_request(
            body: &'static str,
        ) -> (String, tokio::task::JoinHandle<String>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind user-agent capture server");
            let addr = listener.local_addr().expect("capture server address");
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("capture request");
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let read = socket.read(&mut chunk).await.expect("read request");
                    if read == 0 {
                        break;
                    }
                    head.extend_from_slice(&chunk[..read]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write canned response");
                String::from_utf8_lossy(&head).into_owned()
            });
            (format!("http://{addr}"), task)
        }

        /// The `User-Agent` value the client actually put on the wire, or `None` when it sent none.
        fn sent_user_agent(request_head: &str) -> Option<String> {
            request_head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("user-agent")
                    .then(|| value.trim().to_string())
            })
        }

        let expected = concat!("dexdo/", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            DEXDO_USER_AGENT, expected,
            "the identifier must be our product name and the crate's own version"
        );

        // The read/getter client (`chain_clock_skew_preflight`, and the same builder backs
        // `RealChainBackend::connect_with_endpoint`). The preflight's own verdict is irrelevant
        // here -- the request is already on the wire by the time it fails on the canned body.
        let (endpoint, captured) = capture_one_request("{}").await;
        let _ = chain_clock_skew_preflight(&endpoint).await;
        let read_ua = sent_user_agent(&captured.await.expect("capture task"))
            .expect("the read client must send an explicit User-Agent; the edge 403s the default");

        // The shared seam every chain-facing caller (including `crates/dexdo`) builds from.
        let (endpoint, captured) = capture_one_request("{}").await;
        let _ = chain_http_client()
            .expect("shared chain client")
            .post(&endpoint)
            .send()
            .await;
        let shared_ua = sent_user_agent(&captured.await.expect("capture task"))
            .expect("chain_http_client must send an explicit User-Agent");

        // The money-POST client (`build_money_post_http_client`), used for `/v2/messages`.
        let (endpoint, captured) = capture_one_request("[]").await;
        let money_client = build_money_post_http_client().expect("money POST client");
        let _ = send_message_routed_checked(
            &money_client,
            &endpoint,
            "signed-boc",
            "0:11",
            "0:22",
            None,
        )
        .await;
        let money_ua = sent_user_agent(&captured.await.expect("capture task"))
            .expect("the money POST client must send an explicit User-Agent");

        for (label, sent) in [
            ("read", &read_ua),
            ("shared", &shared_ua),
            ("money POST", &money_ua),
        ] {
            assert_eq!(
                sent.as_str(),
                DEXDO_USER_AGENT,
                "the {label} client sent {sent:?} instead of our own identifier"
            );
            assert!(
                !sent.to_ascii_lowercase().contains("mozilla"),
                "the {label} client impersonated a browser: {sent:?}"
            );
            let (name, version) = sent
                .split_once('/')
                .expect("an honest identifier is name/version");
            assert_eq!(name, "dexdo", "the {label} client must name this product");
            assert!(
                version.split('.').count() >= 2 && version.starts_with(char::is_numeric),
                "the {label} client must carry the crate version, got {version:?}"
            );
        }
    }

    #[test]
    fn details_has_withdrawn_accepts_bool_and_string_forms() {
        assert_eq!(
            details_has_withdrawn(&json!({"hasWithdrawn": false})),
            Some(false)
        );
        assert_eq!(
            details_has_withdrawn(&json!({"hasWithdrawn": true})),
            Some(true)
        );
        assert_eq!(
            details_has_withdrawn(&json!({"hasWithdrawn": "0"})),
            Some(false)
        );
        assert_eq!(
            details_has_withdrawn(&json!({"hasWithdrawn": "true"})),
            Some(true)
        );
        assert_eq!(details_has_withdrawn(&json!({"hasWithdrawn": "wat"})), None);
    }

    #[test]
    fn private_note_spendable_shell_is_get_details_balance_record() {
        let details = json!({
            "balance": {"2": "1000000000000"},
            "lockedInOrders": {"2": "250000000000"}
        });
        assert_eq!(
            private_note_balance_currency(&details, crate::params::SHELL_CURRENCY_ID).unwrap(),
            1_000_000_000_000
        );
        assert_eq!(
            private_note_balance_currency(
                &json!({"balance": [{"currency": "2", "value": "7"}]}),
                crate::params::SHELL_CURRENCY_ID,
            )
            .unwrap(),
            7
        );
        let missing = private_note_balance_currency(
            &json!({"balance": {}, "lockedInOrders": {"2": "1000000000000"}}),
            crate::params::SHELL_CURRENCY_ID,
        )
        .expect_err("locked funds are not a substitute for spendable balance")
        .to_string();
        assert!(
            missing.contains("refusing to infer a spendable balance"),
            "{missing}"
        );
    }

    #[test]
    fn seller_note_withdrawn_check_fails_with_actionable_message() {
        let note =
            Address::parse("0:1111111111111111111111111111111111111111111111111111111111111111")
                .expect("address");
        let check = seller_note_withdrawn_check(&note, Some(true));
        assert_eq!(check.status, ChainDoctorStatus::Fail);
        assert_eq!(check.expected.as_deref(), Some("hasWithdrawn=false"));
        assert_eq!(check.actual.as_deref(), Some("hasWithdrawn=true"));
        assert!(
            check.message.contains("this note has withdrawn"),
            "{}",
            check.message
        );
        assert!(
            check.message.contains("can no longer post sell offers"),
            "{}",
            check.message
        );
        assert!(
            check.message.contains("ERR_INVALID_STATE 151"),
            "{}",
            check.message
        );
        assert!(!check.message.contains("TVM_ERROR"), "{}", check.message);
    }

    #[test]
    fn buyer_note_withdrawn_guard_aborts_with_actionable_message() {
        let note =
            Address::parse("0:2222222222222222222222222222222222222222222222222222222222222222")
                .expect("address");
        let error = buyer_note_withdrawn_guard(&note, Some(&json!({"hasWithdrawn": true})))
            .expect_err("a withdrawn buyer note must be rejected before submit");
        let message = error.to_string();
        assert!(message.contains("buyer place aborted"), "{message}");
        assert!(message.contains("can no longer place buys"), "{message}");
        assert!(message.contains("deploy/use a fresh note"), "{message}");
        assert!(message.contains("ERR_INVALID_STATE 151"), "{message}");
        assert!(
            message.contains("PrivateNote._hasWithdrawn=true"),
            "{message}"
        );
        assert!(!message.contains("CHAIN_TRANSPORT"), "{message}");
    }

    #[test]
    fn buyer_note_withdrawn_guard_allows_not_withdrawn_note() {
        let note =
            Address::parse("0:2222222222222222222222222222222222222222222222222222222222222222")
                .expect("address");
        buyer_note_withdrawn_guard(&note, Some(&json!({"hasWithdrawn": false})))
            .expect("a note that has not withdrawn must not be blocked");
    }

    #[test]
    fn buyer_note_withdrawn_guard_fails_open_when_field_is_missing() {
        let note =
            Address::parse("0:2222222222222222222222222222222222222222222222222222222222222222")
                .expect("address");
        buyer_note_withdrawn_guard(&note, Some(&json!({"ephemeralPubkey": "0x1234"})))
            .expect("a contract generation without hasWithdrawn must remain usable");
        buyer_note_withdrawn_guard(&note, None)
            .expect("an empty getter result must not be reported as withdrawn");
    }

    #[test]
    fn lock_history_requires_typed_complete_pagination_metadata() {
        for page_info in [
            None,
            Some(json!({"startCursor": "c1"})),
            Some(json!({"startCursor": "c1", "hasPreviousPage": "false"})),
            Some(json!({"hasPreviousPage": true})),
        ] {
            let mut page = json!({"edges": []});
            if let Some(page_info) = page_info {
                page["pageInfo"] = page_info;
            }
            let error = previous_page_cursor("PrivateNote fixture inbound-message", &page, None)
                .expect_err("truncated lock-history pagination must fail closed");
            assert!(error.to_string().contains("inbound-message"), "{error:#}");
        }

        let complete = json!({
            "pageInfo": {"startCursor": null, "hasPreviousPage": false},
            "edges": []
        });
        assert_eq!(
            previous_page_cursor("PrivateNote fixture inbound-message", &complete, None).unwrap(),
            None
        );
    }

    #[test]
    fn withdraw_note_tokens_payload_shape_is_pinned() {
        let dest =
            Address::parse("0:1111111111111111111111111111111111111111111111111111111111111111")
                .expect("address");
        let payload = withdraw_note_tokens_payload(&dest, "0x0");
        assert_eq!(
            payload,
            json!({
                "destWalletAddr": "0:1111111111111111111111111111111111111111111111111111111111111111",
                "dapp_id": "0x0",
            })
        );
    }

    /// the DApp carried in `withdrawTokens` is the destination wallet's identity, not the
    /// DApp from the dexdo deployment manifest. The backend fixture deliberately disagrees with
    /// the destination so reaching for `self.deployed.dapp_id` makes this regression fail.
    #[test]
    fn withdraw_note_tokens_payload_uses_destination_dapp_not_deployment_dapp() {
        let deployment_dapp = format!("{}4", "0".repeat(63));
        let destination_dapp = "d".repeat(64);
        let mut deployment = deployed("");
        deployment.dapp_id = deployment_dapp.clone();
        let backend = RealChainBackend {
            client: LimitedChainClient::new(
                ChainClient::connect("http://127.0.0.1:9").expect("offline fixture client"),
                ChainRequestCeiling::Unlimited,
            ),
            http: reqwest::Client::new(),
            money_post_http: build_money_post_http_client().expect("money POST client"),
            superroot: Address::parse(&deployment.superroot).expect("fixture SuperRoot"),
            deployed: deployment,
        };
        let destination_account = "a".repeat(64);
        let supplied_destination =
            crate::CanonicalAddress::parse(&format!("{destination_dapp}::{destination_account}"))
                .expect("canonical --to destination");
        let dest = Address::parse(&supplied_destination.legacy()).expect("destination account");

        assert_ne!(backend.deployed.dapp_id, supplied_destination.dapp_id());
        let payload = backend
            .withdraw_note_tokens_payload_for_destination(&dest, supplied_destination.dapp_id());
        assert_eq!(
            payload,
            json!({
                "destWalletAddr": "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "dapp_id": format!("0x{destination_dapp}"),
            })
        );
    }

    #[test]
    fn submit_path_has_no_raw_debug_console_output() {
        let source = include_str!("client.rs");
        assert!(!source.contains(concat!("DEXDO-SUBMIT", "-DBG")));
        assert!(!source.contains(concat!("deploy-prefund", " submit:")));
    }

    /// The note-to-note payload is encoded against the ABI the client actually ships, not against a
    /// signature copied out of the contract source or an issue comment. Argument ORDER is the reason
    /// this matters more here than usual: `initTransfer` takes two `uint128`s in a row, `amount` then
    /// `eccAmount`, so a transposition compiles, encodes, submits, and moves the gas pocket instead
    /// of the trading record.
    #[test]
    fn init_transfer_payload_matches_the_compiled_private_note_abi() {
        let inputs = compiled_abi_inputs(PRIVATENOTE_ABI, "initTransfer")
            .expect("compiled PrivateNote ABI declares initTransfer");
        assert_eq!(
            inputs,
            owned(&[
                ("destDepositHash", "uint256"),
                ("tokenType", "uint32"),
                ("amount", "uint128"),
                ("eccAmount", "uint128"),
            ])
        );
        let payload = init_note_transfer_payload("12345", 2, 81_000_000_000, 0);
        assert_eq!(
            payload,
            json!({
                "destDepositHash": "12345",
                "tokenType": 2,
                "amount": "81000000000",
                "eccAmount": "0",
            })
        );
        // Every declared input is supplied, by the name the ABI declares it under.
        for (name, _) in &inputs {
            assert!(
                payload.get(name).is_some(),
                "initTransfer payload is missing the declared input {name}"
            );
        }
        assert_eq!(payload.as_object().expect("object").len(), inputs.len());
    }

    /// The refusals that can be seen for free are seen, and the ones that cannot are NOT claimed.

    /// `initTransfer` accepts before it checks, so each of these met on chain costs the sending
    /// note's gas to be refused. The last case is the important one: a note carrying open inference
    /// state looks perfectly transferable through `getDetails()`, which is exactly why 167 has to be
    /// explained on the way out instead.
    #[test]
    fn note_transfer_refusals_are_read_from_get_details() {
        let clean = json!({
            "depositIdentifierHash": "42",
            "balance": { "2": "350000000000" },
            "lockedInOrders": { "2": "0" },
            "busyAddress": null,
            "couponsValue": "0",
            "hasWithdrawn": false,
        });
        assert_eq!(note_transfer_sender_refusal(&clean), None);
        assert_eq!(note_transfer_dest_refusal(&clean), None);
        assert_eq!(
            note_transfer_deposit_identifier_hash(&clean).expect("hash"),
            "42"
        );

        let mut withdrawn = clean.clone();
        withdrawn["hasWithdrawn"] = json!(true);
        assert_eq!(
            note_transfer_sender_refusal(&withdrawn),
            Some(NoteTransferRefusal::SenderWithdrawn)
        );
        // The destination's ONLY state gate, and the one that costs money if it is not checked: the
        // sender debits `_balance` before the far side ever refuses.
        assert_eq!(
            note_transfer_dest_refusal(&withdrawn),
            Some(NoteTransferRefusal::DestWithdrawn)
        );

        let mut busy = clean.clone();
        busy["busyAddress"] =
            json!("0:2222222222222222222222222222222222222222222222222222222222222222");
        assert_eq!(
            note_transfer_sender_refusal(&busy),
            Some(NoteTransferRefusal::SenderBusy {
                with: "0:2222222222222222222222222222222222222222222222222222222222222222"
                    .to_string()
            })
        );

        let mut coupon = clean.clone();
        coupon["couponsValue"] = json!("7");
        assert_eq!(
            note_transfer_sender_refusal(&coupon),
            Some(NoteTransferRefusal::SenderCouponActive { value: 7 })
        );

        let mut locked = clean.clone();
        locked["lockedInOrders"] = json!({ "2": "5000" });
        assert_eq!(
            note_transfer_sender_refusal(&locked),
            Some(NoteTransferRefusal::SenderLockedInOrders {
                token_type: 2,
                locked: 5000
            })
        );
        // The array rendering of the same map is read too, so a getter that answers in the other
        // shape does not silently report a locked note as clean.
        let mut locked_array = clean.clone();
        locked_array["lockedInOrders"] = json!([{ "currency": 2, "value": "5000" }]);
        assert_eq!(
            note_transfer_sender_refusal(&locked_array),
            Some(NoteTransferRefusal::SenderLockedInOrders {
                token_type: 2,
                locked: 5000
            })
        );

        // A note with open orders / resting inference / a live deal is INVISIBLE here: none of
        // `_openOrderCount`, `_restingInf`, `_pendingInf`, `_liveDeals` is in `getDetails()`. This
        // asserts the gap rather than pretending it is covered.
        let details_keys: Vec<&str> = clean
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        for absent in ["openOrderCount", "restingInf", "pendingInf", "liveDeals"] {
            assert!(
                !details_keys.contains(&absent),
                "getDetails() now exposes {absent}; the ERR_OPEN_ORDERS_EXIST refusal became \
                 preflightable and should be raised before spending instead of explained after"
            );
        }
    }

    /// Amount refusals, including the one that is easy to get backwards: below the contract minimum
    /// is refused even when the sender is rich, because `minStakeValue` is checked before the
    /// balance is.
    #[test]
    fn note_transfer_amount_refusals_match_the_contract_requires() {
        let minimum = crate::params::MIN_NOTE_TRANSFER_SHELL_RAW;
        assert_eq!(
            note_transfer_amount_refusal(350_000_000_000, 81_000_000_000, minimum),
            None
        );
        assert_eq!(
            note_transfer_amount_refusal(350_000_000_000, minimum, minimum),
            None
        );
        assert_eq!(
            note_transfer_amount_refusal(350_000_000_000, minimum - 1, minimum),
            Some(NoteTransferRefusal::AmountBelowMinimum {
                amount: minimum - 1,
                minimum
            })
        );
        assert_eq!(
            note_transfer_amount_refusal(40_000_000_000, 81_000_000_000, minimum),
            Some(NoteTransferRefusal::SenderRecordShort {
                have: 40_000_000_000,
                want: 81_000_000_000
            })
        );
        // Exactly the whole record is allowed: the contract's check is `>=`.
        assert_eq!(
            note_transfer_amount_refusal(81_000_000_000, 81_000_000_000, minimum),
            None
        );
    }

    /// The contract minimum is taken FROM the contract, not from folklore. If `MIN_VALUE_SHELL`
    /// moves, this fails and the constant has to move with it rather than quietly under-refusing.
    #[test]
    fn min_note_transfer_matches_the_contract_min_stake_value() {
        const MODIFIERS: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/dex/modifiers/modifiers.sol"
        ));
        let at = MODIFIERS
            .find("MIN_VALUE_SHELL")
            .expect("modifiers.sol declares MIN_VALUE_SHELL");
        let rest = &MODIFIERS[at..];
        let eq = rest.find('=').expect("MIN_VALUE_SHELL is assigned");
        let end = rest.find(';').expect("MIN_VALUE_SHELL terminator");
        let declared: u128 = rest[eq + 1..end]
            .replace('_', "")
            .trim()
            .parse()
            .expect("numeric MIN_VALUE_SHELL");
        assert_eq!(declared, crate::params::MIN_NOTE_TRANSFER_SHELL_RAW);
    }

    /// The two refusals no preflight can reach are re-stated by name and by what to do, keyed on the
    /// exit code rather than on the wording around it. `exit_code=<n> (` is the exact fragment
    /// `exit_code_fragment` writes, so the label already being there is what this hooks onto.
    #[test]
    fn note_transfer_names_the_refusals_a_preflight_cannot_reach() {
        let open_orders = note_transfer_submit_hint(
            "on-chain submit failed: exit_code=167 (dex::ERR_OPEN_ORDERS_EXIST) stage=compute",
        )
        .expect("167 is explained");
        assert!(open_orders.contains("ERR_OPEN_ORDERS_EXIST"));
        // The asymmetry is the actionable part: such a note can still RECEIVE.
        assert!(open_orders.contains("still RECEIVE"));

        let busy = note_transfer_submit_hint(
            "on-chain submit failed: exit_code=121 (dex::ERR_NOTE_BUSY) stage=compute",
        )
        .expect("121 is explained");
        assert!(busy.contains("ERR_NOTE_BUSY"));
        // A latch, not a wait: telling an operator to retry would be the wrong instruction.
        assert!(busy.contains("not a wait-and-retry state"));

        // Numbers that merely CONTAIN these digits are not these codes.
        assert_eq!(
            note_transfer_submit_hint(
                "on-chain submit failed: exit_code=1670 (unknown contract error code) stage=compute"
            ),
            None
        );
        assert_eq!(
            note_transfer_submit_hint(
                "on-chain submit failed: exit_code=102 (dex::ERR_LOW_VALUE) stage=compute"
            ),
            None
        );
        // And the codes are the ones the vendored table already knows by these names, so the hint
        // and the label can never disagree about which constant a number is.
        assert!(crate::onchain_diagnostics::contract_error_names(167)
            .contains(&"dex::ERR_OPEN_ORDERS_EXIST"));
        assert!(
            crate::onchain_diagnostics::contract_error_names(121).contains(&"dex::ERR_NOTE_BUSY")
        );
    }
}
