use super::backends::{note_owner_mismatch_reason, MODEL_TICK_SIZE};
use super::book_events::{read_book_event_fold, BookEventFold};
use super::contracts_provision::*;
use super::stream_locks::{
    decode_note_stream_lock_call, NoteStreamLockEntry, NoteStreamLockFold, NoteStreamLockKind,
    NoteStreamLockSnapshot,
};
use crate::chain::{
    InferenceSubscriptionPlacement, MatchWatchCursor, MatchedFill, OrderBookSubscription,
};
use crate::manifest::{model_hash_for, MarketManifest};
use crate::onchain_diagnostics::{validate_onchain_submit_response, OnchainSubmitError};
use crate::oracle_manifest::OracleMarketManifest;
use anyhow::{anyhow, Context, Result};
#[cfg(feature = "test-giver")]
use base64::Engine as _;
use gosh_ackinacki::airegistry::calls::encode_external_call;
use gosh_ackinacki::airegistry::deploy::{build_deploy, local_context};
use gosh_ackinacki::config::AiRegistryConfig;
use gosh_ackinacki::sdk::{Address, ChainClient, ChainLiveness, KeyPair};
use gosh_ackinacki::wallet::query::{dest_account_id_hex, fetch_dapp_id};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;
#[cfg(feature = "test-giver")]
use tvm_block::Deserializable;

const FIXED_SUPERROOT_ACCOUNT_ID: &str =
    "0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c";
const MIN_PMP_INITIAL_STAKE: u128 = 10_000_000;
/// `PrivateNote.sol::STREAM_LOCK_MAX`; the owner escape hatch is accepted strictly after this delay.
pub const PRIVATE_NOTE_STREAM_LOCK_MAX_SECS: u64 = 7 * 24 * 60 * 60;
/// Pinned `tvm_client` default signed-message lifetime(`message_expiration_timeout`).
const SDK_MESSAGE_EXPIRY_SECS: u64 = 40;
/// Strict contract window: `block.timestamp < expireAt < block.timestamp + 300`.
const CONTRACT_MESSAGE_WINDOW_SECS: u64 = 300;
/// Keep ten seconds away from either strict boundary for observation/submit latency.
const CLOCK_SKEW_SAFETY_MARGIN_SECS: u64 = 10;
const MAX_CLOCK_BEHIND_SECS: u64 = SDK_MESSAGE_EXPIRY_SECS - CLOCK_SKEW_SAFETY_MARGIN_SECS;
const MAX_CLOCK_AHEAD_SECS: u64 =
    CONTRACT_MESSAGE_WINDOW_SECS - SDK_MESSAGE_EXPIRY_SECS - CLOCK_SKEW_SAFETY_MARGIN_SECS;

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

#[allow(dead_code)]
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

#[cfg(feature = "test-giver")]
#[path = "test_giver.rs"]
mod test_giver;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellnetDoctorStatus {
    Pass,
    Fail,
    Skip,
}

impl ShellnetDoctorStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellnetDoctorCheck {
    pub name: String,
    pub status: ShellnetDoctorStatus,
    pub address: Option<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellnetDoctorReport {
    pub network: String,
    pub versions: Vec<(String, String)>,
    pub checks: Vec<ShellnetDoctorCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteStreamLockStatus {
    pub stream_count: u32,
    pub dispute_count: u32,
    pub last_change_unix: u64,
    pub entries: Vec<NoteStreamLockEntry>,
    pub history_complete: bool,
}

impl NoteStreamLockStatus {
    /// Reconstruct a status from successful inbound calls ordered from oldest to newest.
    /// `internal_source` is the authoritative TokenContract deal address for 4.0.27
    /// `stream*Lock(sellerPubkey, nonce)` calls; `None` identifies an external owner clear call.
    pub fn from_successful_inbound_calls<'a>(
        stream_count: u32,
        dispute_count: u32,
        last_change_unix: u64,
        calls: impl IntoIterator<Item = (u64, &'a str, bool, Option<&'a str>)>,
    ) -> Result<Self> {
        let entries = reconstruct_note_stream_lock_entries(calls)?;
        Ok(Self::from_entries(
            stream_count,
            dispute_count,
            last_change_unix,
            entries,
        ))
    }

    fn from_entries(
        stream_count: u32,
        dispute_count: u32,
        last_change_unix: u64,
        entries: Vec<NoteStreamLockEntry>,
    ) -> Self {
        let folded_stream = entries
            .iter()
            .filter(|entry| entry.kind == NoteStreamLockKind::Stream)
            .count();
        let folded_dispute = entries
            .iter()
            .filter(|entry| entry.kind == NoteStreamLockKind::Dispute)
            .count();
        let history_complete =
            folded_stream == stream_count as usize && folded_dispute == dispute_count as usize;
        Self {
            stream_count,
            dispute_count,
            last_change_unix,
            entries,
            history_complete,
        }
    }
}

impl ShellnetDoctorReport {
    pub fn is_ok(&self) -> bool {
        self.checks
            .iter()
            .all(|c| c.status != ShellnetDoctorStatus::Fail)
    }

    pub fn fail_summary(&self) -> String {
        self.checks
            .iter()
            .filter(|c| c.status == ShellnetDoctorStatus::Fail)
            .map(|c| format!("{}: {}", c.name, c.message))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn normalize_code_hash(raw: &str) -> Option<String> {
    let h = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    if h.is_empty() || h.len() > 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{h:0>64}").to_lowercase())
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

type SuccessfulInboundLockCall<'a> = (u64, &'a str, bool, Option<&'a str>);

fn successful_inbound_lock_call(node: &Value) -> Result<Option<SuccessfulInboundLockCall<'_>>> {
    if !successful_inbound_call(node) {
        return Ok(None);
    }
    let Some(body) = node["body"].as_str() else {
        return Ok(None);
    };
    let created_at = node["created_at"]
        .as_u64()
        .or_else(|| {
            node["created_at"]
                .as_str()
                .and_then(|value| value.parse().ok())
        })
        .ok_or_else(|| anyhow!("successful PrivateNote inbound call has no created_at"))?;
    let internal_source = node["src"].as_str().filter(|source| !source.is_empty());
    Ok(Some((
        created_at,
        body,
        internal_source.is_some(),
        internal_source,
    )))
}

fn reconstruct_note_stream_lock_entries<'a>(
    calls: impl IntoIterator<Item = (u64, &'a str, bool, Option<&'a str>)>,
) -> Result<Vec<NoteStreamLockEntry>> {
    let mut fold = NoteStreamLockFold::default();
    for (created_at, body, internal, internal_source) in calls {
        if let Some(call) = decode_note_stream_lock_call(body, internal, internal_source)? {
            fold.apply(call, created_at);
        }
    }
    Ok(fold.into_entries())
}

fn details_has_withdrawn(details: &Value) -> Option<bool> {
    getter_bool(details, "hasWithdrawn")
}

fn note_withdrawn_sell_offer_message(note: &Address) -> String {
    format!(
        "seller post_offer aborted: this note has withdrawn and can no longer post sell offers -- deploy/use a \
         fresh note, re-provision the market, and retry. note={note}; postSellOffer would revert \
         ERR_INVALID_STATE 151 because PrivateNote._hasWithdrawn=true."
    )
}

fn note_withdrawn_buy_message(note: &Address) -> String {
    format!(
        "buyer place aborted: this note has withdrawn and can no longer place buys (deploy/use a fresh note); \
         the chain rejects it with ERR_INVALID_STATE 151 because PrivateNote._hasWithdrawn=true. note={note}"
    )
}

fn buyer_note_withdrawn_guard(note: &Address, details: Option<&Value>) -> Result<()> {
    match details.and_then(details_has_withdrawn) {
        Some(true) => Err(anyhow!(note_withdrawn_buy_message(note))),
        Some(false) => Ok(()),
        None => {
            eprintln!(
                "buyer place preflight note: PrivateNote.getDetails for note {note} did not expose \
                 hasWithdrawn; continuing without the withdrawn-state guard"
            );
            Ok(())
        }
    }
}

fn seller_note_withdrawn_check(note: &Address, actual: Option<bool>) -> ShellnetDoctorCheck {
    let (status, actual, message) = match actual {
        Some(false) => (
            ShellnetDoctorStatus::Pass,
            Some("hasWithdrawn=false".to_string()),
            "seller note has not withdrawn; postSellOffer is not blocked by _hasWithdrawn".to_string(),
        ),
        Some(true) => (
            ShellnetDoctorStatus::Fail,
            Some("hasWithdrawn=true".to_string()),
            note_withdrawn_sell_offer_message(note),
        ),
        None => (
            ShellnetDoctorStatus::Fail,
            Some("hasWithdrawn=<missing>".to_string()),
            "PrivateNote.getDetails did not expose hasWithdrawn; refusing to prove postSellOffer safety"
                .to_string(),
        ),
    };
    ShellnetDoctorCheck {
        name: "seller PrivateNote withdrawn state".to_string(),
        status,
        address: Some(note.with_workchain()),
        expected: Some("hasWithdrawn=false".to_string()),
        actual,
        message,
    }
}

pub(super) fn code_hash_check(
    name: &str,
    address: Option<&Address>,
    expected: &str,
    actual: Option<&str>,
) -> ShellnetDoctorCheck {
    let expected = normalize_code_hash(expected).unwrap_or_else(|| expected.to_string());
    let actual = actual.and_then(normalize_code_hash);
    let (status, message) = match actual.as_deref() {
        Some(a) if a == expected => (
            ShellnetDoctorStatus::Pass,
            "binary pin matches live shellnet".to_string(),
        ),
        Some(a) => (
            ShellnetDoctorStatus::Fail,
            format!(
                "dexdo build is STALE vs live shellnet - binary pins {expected}, live is {a}; rebuild from dev HEAD"
            ),
        ),
        None => (
            ShellnetDoctorStatus::Fail,
            "live account is missing, inactive, or exposes no code_hash".to_string(),
        ),
    };
    ShellnetDoctorCheck {
        name: name.to_string(),
        status,
        address: address.map(|a| a.with_workchain()),
        expected: Some(expected),
        actual,
        message,
    }
}

fn account_id_eq(addr: &Address, account_id: &str) -> bool {
    let addr = addr.with_workchain();
    let addr = addr.strip_prefix("0:").unwrap_or(&addr);
    addr.eq_ignore_ascii_case(account_id)
}

pub(super) fn active_check(name: &str, address: &Address, active: bool) -> ShellnetDoctorCheck {
    ShellnetDoctorCheck {
        name: name.to_string(),
        status: if active {
            ShellnetDoctorStatus::Pass
        } else {
            ShellnetDoctorStatus::Fail
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

fn pass_check(name: &str, message: &str) -> ShellnetDoctorCheck {
    ShellnetDoctorCheck {
        name: name.to_string(),
        status: ShellnetDoctorStatus::Pass,
        address: None,
        expected: None,
        actual: None,
        message: message.to_string(),
    }
}

fn skipped_check(name: &str, message: &str) -> ShellnetDoctorCheck {
    ShellnetDoctorCheck {
        name: name.to_string(),
        status: ShellnetDoctorStatus::Skip,
        address: None,
        expected: None,
        actual: None,
        message: message.to_string(),
    }
}

fn clock_skew_check(local_unix: u64, chain_unix: u64) -> ShellnetDoctorCheck {
    let (skew_secs, direction, permitted_secs) = if local_unix >= chain_unix {
        (local_unix - chain_unix, "ahead of", MAX_CLOCK_AHEAD_SECS)
    } else {
        (chain_unix - local_unix, "behind", MAX_CLOCK_BEHIND_SECS)
    };
    let status = if skew_secs <= permitted_secs {
        ShellnetDoctorStatus::Pass
    } else {
        ShellnetDoctorStatus::Fail
    };
    let message = if status == ShellnetDoctorStatus::Pass {
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
    ShellnetDoctorCheck {
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

async fn fetch_chain_time_secs(http: &reqwest::Client, endpoint: &str) -> Result<u64> {
    let (graphql_url, _) = endpoint_urls(endpoint)?;
    let body = json!({
        "query": "{ blockchain { blocks(last:1){ edges { node { gen_utime } } } } }"
    });
    let response: Value = http
        .post(&graphql_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {graphql_url} for chain time"))?
        .error_for_status()?
        .json()
        .await
        .context("parse GraphQL chain-time response")?;
    if let Some(errors) = response.get("errors").filter(|errors| !errors.is_null()) {
        return Err(anyhow!("GraphQL chain-time errors: {errors}"));
    }
    response
        .pointer("/data/blockchain/blocks/edges/0/node/gen_utime")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("chain time: latest block is missing gen_utime"))
}

/// Fail closed before a signed SDK write when the operator clock is unsafe for the contracts'
/// five-minute `expireAt` window.
pub async fn shellnet_clock_skew_preflight(endpoint: &str) -> Result<()> {
    let http = reqwest::Client::builder().user_agent(BROWSER_UA).build()?;
    let check = clock_skew_check(
        local_unix_secs()?,
        fetch_chain_time_secs(&http, endpoint).await?,
    );
    if check.status == ShellnetDoctorStatus::Fail {
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
}

/// Manifest of the deployed shellnet contracts(`contracts/deployed.shellnet.json`).
/// The address source for the adapter and e2e. `InferenceOrderBook`(per-model) and
/// `TokenContract`(per-deal) are derived/discovered on the fly, so they are not pinned here.
#[derive(Debug, Clone, Deserialize)]
pub struct Deployed {
    /// Network label(for shellnet, `"shellnet"`).
    pub network: String,
    /// `SuperRoot` airegistry -- the derivation point for `RootModel`/`InferenceOrderBook`.
    pub superroot: String,
    /// `DappConfig`(a DApp with unlimited credit for deploys).
    pub dapp_config: String,
    /// `dapp_id`(= account_id of `SuperRoot`).
    pub dapp_id: String,
    /// The seller's probe-tick commission in bps.
    pub seller_probe_commission_bps: u16,
    /// Optional Block Manager endpoint. `graphql` is accepted for deployed-manifest compatibility.
    #[serde(default, alias = "graphql")]
    pub endpoint: Option<String>,
}

impl Deployed {
    /// Read the manifest from a file(`contracts/deployed.shellnet.json`).
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path.as_ref())?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

pub const DEFAULT_SHELLNET_ENDPOINT: &str = "https://shellnet.ackinacki.org";

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

pub fn resolve_endpoint(explicit: Option<&str>, manifest: &Deployed) -> anyhow::Result<String> {
    normalize_endpoint(
        explicit
            .or(manifest.endpoint.as_deref())
            .unwrap_or(DEFAULT_SHELLNET_ENDPOINT),
    )
}

/// Real on-chain backend on top of `gosh.ackinacki` `ChainClient`.
/// Carries a live connection to shellnet and the root addresses from the manifest.
pub struct RealChainBackend {
    client: ChainClient,
    /// Browser-UA http client for reads, with reqwest's default redirect behavior.
    pub(super) http: reqwest::Client,
    /// Browser-UA client used only for one-shot money POSTs to `/v2/messages`.
    money_post_http: reqwest::Client,
    superroot: Address,
    deployed: Deployed,
}

/// True iff `e` is the BK REST `/v2/account` lookup 404 -- the destination account is not yet in the
/// block-manager index(a **funded-uninit deploy target**). Matched on the specific endpoint **and**
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
            "shellnet submit failure payload"
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CorrelatedActionReceipt {
    message_hash: String,
    transaction_hash: Option<String>,
    aborted: Option<bool>,
    action_success: Option<bool>,
    result_code: Option<i64>,
    no_funds: Option<bool>,
}

const EXACT_MESSAGE_RECEIPT_QUERY: &str = r#"
    query($hash: String!, $accountId: String!, $dappId: String!) {
      blockchain {
        message(hash: $hash) {
          id dst
          dst_transaction {
            id status aborted account_addr
            action { result_code success no_funds }
          }
        }
        account(account_id: $accountId, dapp_id: $dappId) {
          info { id dapp_id }
        }
      }
    }
"#;

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

fn build_money_post_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .redirect(reqwest::redirect::Policy::none())
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

async fn send_message_routed_checked(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
    account_id: &str,
    dapp_id: &str,
    thread_id: Option<&str>,
) -> Result<Value> {
    eprintln!(
        "DEXDO-SUBMIT-DBG account_id={} dapp_id={} thread_id={:?}",
        bare_hex(account_id),
        bare_hex(dapp_id),
        thread_id
    );
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
            "shellnet submit refused HTTP redirect {}",
            response.status()
        ));
    }
    let resp = response.error_for_status()?.json::<Value>().await?;
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

async fn prepare_reclaim_money_post_if<P, F, Fut>(
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

async fn query_exact_destination_receipt(
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    dapp_id: &str,
    expected_message_hash: &str,
) -> Result<Value> {
    let gql = format!("{}/graphql", endpoint.trim_end_matches('/'));
    let response: Value = http
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
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(response)
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
    let action_success = transaction["action"]["success"].as_bool().ok_or_else(|| {
        anyhow!("fundDeployShell finalized destination transaction has no action success fact")
    })?;
    let result_code = transaction["action"]["result_code"]
        .as_i64()
        .ok_or_else(|| {
            anyhow!("fundDeployShell finalized destination transaction has no action result code")
        })?;
    let no_funds = transaction["action"]["no_funds"].as_bool().ok_or_else(|| {
        anyhow!("fundDeployShell finalized destination transaction has no no_funds fact")
    })?;
    Ok(Some(CorrelatedActionReceipt {
        message_hash: message_hash.to_string(),
        transaction_hash: Some(transaction_hash.to_string()),
        aborted: Some(aborted),
        action_success: Some(action_success),
        result_code: Some(result_code),
        no_funds: Some(no_funds),
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
        std::time::Duration::from_secs(2),
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
    const ATTEMPTS: u32 = 12;
    for attempt in 0..ATTEMPTS {
        let response = query().await?;
        if let Some(errors) = response.get("errors") {
            return Err(anyhow!("fundDeployShell receipt GraphQL errors: {errors}"));
        }
        if let Some(receipt) =
            parse_exact_destination_receipt(&response, account_id, dapp_id, expected_message_hash)?
        {
            return Ok(Some(receipt));
        }
        if attempt + 1 < ATTEMPTS {
            tokio::time::sleep(retry_delay).await;
        }
    }
    Ok(None)
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

#[derive(Debug)]
pub(super) struct ExtOutMessage {
    pub id: String,
    pub created_at: u64,
    pub cursor: String,
    pub body: String,
}

pub(super) struct ExtOutPage {
    pub messages: Vec<ExtOutMessage>,
    pub previous_cursor: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct SellerOfferEvents {
    pub placed_order_id: Option<u128>,
    pub matched: bool,
    pub placement_value_returned: bool,
}

/// One successful owner-signed `PrivateNote.placeInferenceBuy` transaction, decoded from the note's
/// external-in message and backed by a non-aborted destination transaction.
#[cfg(feature = "test-giver")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceInferenceBuyReceipt {
    pub message_id: String,
    pub created_at: u64,
    pub max_price_per_tick: u128,
    pub ticks: u128,
    pub escrow: u128,
}

/// One ordered lifecycle/settlement event emitted by a `TokenContract`.
#[cfg(feature = "test-giver")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenContractSettlementReceipt {
    pub message_id: String,
    pub created_at: u64,
    pub event: TokenContractSettlementEvent,
}

/// Exact ABI payload of a lifecycle/settlement event used by the live money-path proof.
#[cfg(feature = "test-giver")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenContractSettlementEvent {
    ProbeAccepted {
        buyer: String,
        to_seller: u128,
        commission_returned: u128,
    },
    ProbeBurned {
        buyer: String,
        burned_probe: u128,
        burned_commission: u128,
        refund_to_buyer: u128,
    },
    TickFinalized {
        finalized_owed: u128,
        deposit: u128,
    },
    StreamStopped {
        buyer: String,
        to_seller: u128,
        refund_to_buyer: u128,
    },
}

/// Ordered lifecycle and settlement receipts emitted by one `TokenContract`.
#[cfg(feature = "test-giver")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenContractSettlementReceipts {
    pub events: Vec<TokenContractSettlementReceipt>,
}

pub(super) async fn fetch_ext_out_page(
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    dapp_id: &str,
    page_size: u32,
    before: Option<&str>,
) -> Result<ExtOutPage> {
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
    let response: Value = http
        .post(&gql)
        .json(&json!({
            "query": query,
            "variables": {
                "accountId": account_id,
                "dappId": dapp_id,
                "last": page_size,
                "before": before,
            },
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(errors) = response.get("errors") {
        return Err(anyhow!(
            "account {account_id} ext-out GraphQL errors: {errors}"
        ));
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
        let id = node["id"].as_str().unwrap_or(cursor);
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

async fn fetch_all_ext_out_messages(
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
) -> Result<Vec<ExtOutMessage>> {
    const PAGE_SIZE: u32 = 1_000;
    let dapp_id = fetch_dapp_id(http, endpoint, account_id).await?;
    let mut before: Option<String> = None;
    let mut seen = BTreeSet::new();
    let mut messages = Vec::new();
    loop {
        let page = fetch_ext_out_page(
            http,
            endpoint,
            account_id,
            &dapp_id,
            PAGE_SIZE,
            before.as_deref(),
        )
        .await?;
        for message in page.messages {
            if !seen.insert(message.id.clone()) {
                continue;
            }
            messages.push(message);
        }
        let Some(next) = page.previous_cursor else {
            break;
        };
        before = Some(next);
    }
    messages.sort_by(|left, right| {
        (left.created_at, &left.cursor).cmp(&(right.created_at, &right.cursor))
    });
    Ok(messages)
}

#[cfg(feature = "test-giver")]
fn decode_token_contract_settlement_receipts(
    mut messages: Vec<ExtOutMessage>,
) -> Result<TokenContractSettlementReceipts> {
    messages.sort_by(|left, right| {
        (left.created_at, &left.cursor).cmp(&(right.created_at, &right.cursor))
    });
    let mut receipts = TokenContractSettlementReceipts::default();
    for message in messages {
        let Some(decoded) = decode_external_abi_message(&message.body, TOKENCONTRACT_ABI, false)
        else {
            continue;
        };
        let required_u128 = |name| {
            decoded_u128(&decoded.tokens, name).ok_or_else(|| {
                anyhow!(
                    "{} event {} has no {name}",
                    decoded.function_name,
                    message.id
                )
            })
        };
        let required_address = |name| {
            decoded_address(&decoded.tokens, name).ok_or_else(|| {
                anyhow!(
                    "{} event {} has no {name}",
                    decoded.function_name,
                    message.id
                )
            })
        };
        let event = match decoded.function_name.as_str() {
            "ProbeAccepted" => TokenContractSettlementEvent::ProbeAccepted {
                buyer: required_address("buyer")?,
                to_seller: required_u128("toSeller")?,
                commission_returned: required_u128("commissionReturned")?,
            },
            "ProbeBurned" => TokenContractSettlementEvent::ProbeBurned {
                buyer: required_address("buyer")?,
                burned_probe: required_u128("burnedProbe")?,
                burned_commission: required_u128("burnedCommission")?,
                refund_to_buyer: required_u128("refundToBuyer")?,
            },
            "TickFinalized" => TokenContractSettlementEvent::TickFinalized {
                finalized_owed: required_u128("finalizedOwed")?,
                deposit: required_u128("deposit")?,
            },
            "StreamStopped" => TokenContractSettlementEvent::StreamStopped {
                buyer: required_address("buyer")?,
                to_seller: required_u128("toSeller")?,
                refund_to_buyer: required_u128("refundToBuyer")?,
            },
            _ => continue,
        };
        receipts.events.push(TokenContractSettlementReceipt {
            message_id: message.id,
            created_at: message.created_at,
            event,
        });
    }
    Ok(receipts)
}

#[cfg(feature = "test-giver")]
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

#[cfg(feature = "test-giver")]
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

#[cfg(feature = "test-giver")]
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

#[cfg(feature = "test-giver")]
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

impl RealChainBackend {
    /// Connect using an optional manifest endpoint, falling back to the canonical shellnet endpoint.
    pub fn connect(manifest_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::connect_with_endpoint(manifest_path, None)
    }

    /// Connect with an explicit endpoint override, then manifest endpoint, then the shellnet default.
    pub fn connect_with_endpoint(
        manifest_path: impl AsRef<Path>,
        endpoint: Option<&str>,
    ) -> anyhow::Result<Self> {
        let deployed = Deployed::load(manifest_path)?;
        let endpoint = resolve_endpoint(endpoint, &deployed)?;
        let client = ChainClient::connect_with_config(&endpoint, AiRegistryConfig::shellnet())?;
        let http = reqwest::Client::builder().user_agent(BROWSER_UA).build()?;
        let money_post_http = build_money_post_http_client()?;
        let superroot = Address::parse(&deployed.superroot)?;
        Ok(Self {
            client,
            http,
            money_post_http,
            superroot,
            deployed,
        })
    }

    /// Fold authoritative live orders from one `InferenceOrderBook` ext-out stream.
    pub async fn fold_order_book_events(
        &self,
        order_book: &str,
        previous: BookEventFold,
    ) -> Result<BookEventFold> {
        read_book_event_fold(&self.http, self.client.endpoint(), order_book, previous).await
    }

    /// Low-level chain client(for the trait adapter in the next step).
    pub fn client(&self) -> &ChainClient {
        &self.client
    }

    /// The `SuperRoot` address -- the derivation point for `RootModel`/`InferenceOrderBook`.
    pub fn superroot(&self) -> &Address {
        &self.superroot
    }

    /// Chain liveness check -- confirms a working connection to shellnet.
    pub async fn liveness(&self) -> Result<ChainLiveness> {
        self.client.chain_liveness().await
    }

    async fn clock_skew_preflight(&self) -> Result<()> {
        let check = clock_skew_check(
            local_unix_secs()?,
            fetch_chain_time_secs(&self.http, self.client.endpoint()).await?,
        );
        if check.status == ShellnetDoctorStatus::Fail {
            return Err(anyhow!(check.message));
        }
        Ok(())
    }

    async fn account_active_code_hash(&self, addr: &Address) -> Result<(bool, Option<String>)> {
        let Some(acc) = self.client.get_account(addr).await? else {
            return Ok((false, None));
        };
        Ok((
            acc.is_active(),
            acc.code_hash.as_deref().and_then(normalize_code_hash),
        ))
    }

    async fn code_hash_account_check(
        &self,
        name: &str,
        addr: &Address,
        expected: &str,
    ) -> Result<ShellnetDoctorCheck> {
        let (active, hash) = self.account_active_code_hash(addr).await?;
        if !active {
            return Ok(code_hash_check(name, Some(addr), expected, None));
        }
        Ok(code_hash_check(name, Some(addr), expected, hash.as_deref()))
    }

    async fn seller_note_withdrawn_check(&self, note: &Address) -> Result<ShellnetDoctorCheck> {
        match self.private_note_details(note).await {
            Ok(Some(details)) => Ok(seller_note_withdrawn_check(
                note,
                details_has_withdrawn(&details),
            )),
            Ok(None) => Ok(ShellnetDoctorCheck {
                name: "seller PrivateNote withdrawn state".to_string(),
                status: ShellnetDoctorStatus::Fail,
                address: Some(note.with_workchain()),
                expected: Some("hasWithdrawn=false".to_string()),
                actual: Some("getDetails=<none>".to_string()),
                message: "seller note returned no PrivateNote.getDetails; it is not active/current enough to prove postSellOffer safety"
                    .to_string(),
            }),
            Err(e) => Ok(ShellnetDoctorCheck {
                name: "seller PrivateNote withdrawn state".to_string(),
                status: ShellnetDoctorStatus::Fail,
                address: Some(note.with_workchain()),
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
            .run_getter(addr, abi, "getVersion", json!({}))
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

    /// Read-only shellnet readiness report: compare this binary's embedded/pinned contract images against
    /// live shellnet and, when supplied, verify that a market manifest still points at active IOB/TC accounts.
    pub async fn doctor(&self, market: Option<&MarketManifest>) -> Result<ShellnetDoctorReport> {
        let mut checks = Vec::new();
        self.liveness().await?;
        checks.push(pass_check("shellnet endpoint", "reachable"));
        checks.push(clock_skew_check(
            local_unix_secs()?,
            fetch_chain_time_secs(&self.http, self.client.endpoint()).await?,
        ));

        if account_id_eq(&self.superroot, FIXED_SUPERROOT_ACCOUNT_ID) {
            checks.push(skipped_check(
                "SuperRoot code hash",
                "fixed-superroot shellnet redeploy uses the 0:0c0c... zerostate anchor; old code-derived accounts are intentionally gone",
            ));
        } else {
            let superroot_hash = code_hash(SUPERROOT_TVC)?;
            checks.push(
                self.code_hash_account_check(
                    "SuperRoot code hash",
                    &self.superroot,
                    &superroot_hash,
                )
                .await?,
            );
        }

        if self.deployed.dapp_config.trim().is_empty() {
            checks.push(skipped_check(
                "DappConfig account",
                "fixed-superroot shellnet redeploy has no legacy DappConfig manifest account",
            ));
        } else {
            let dapp_config = Address::parse(&self.deployed.dapp_config)?;
            let (dapp_active, _) = self.account_active_code_hash(&dapp_config).await?;
            checks.push(active_check(
                "DappConfig account",
                &dapp_config,
                dapp_active,
            ));
        }

        let rootpn = Address::parse(ROOTPN_ADDR)?;
        checks.push(
            self.code_hash_account_check("RootPN code hash", &rootpn, SHELLNET_ROOTPN_V1_CODE_HASH)
                .await?,
        );
        let rootoracle = Address::parse(ROOTORACLE_ADDR)?;
        checks.push(
            self.code_hash_account_check(
                "RootOracle code hash",
                &rootoracle,
                &code_hash(ROOTORACLE_TVC)?,
            )
            .await?,
        );

        let rootpn_details = self
            .client
            .run_getter(&rootpn, ROOTPN_ABI, "getDetails", json!({}))
            .await?
            .ok_or_else(|| anyhow!("RootPN is not active"))?;
        checks.push(code_hash_check(
            "PrivateNote code hash (RootPN pin)",
            None,
            &code_hash(PRIVATENOTE_TVC)?,
            rootpn_details["privateNoteCodeHash"].as_str(),
        ));

        if let Some(market) = market {
            let rm = Address::parse(&market.root_model)?;
            checks.push(
                self.code_hash_account_check(
                    "RootModel code hash",
                    &rm,
                    SUPERROOT_PINNED_RM_CODE_HASH,
                )
                .await?,
            );
            let ob = Address::parse(&market.inference_order_book)?;
            checks.push(
                self.code_hash_account_check(
                    "InferenceOrderBook code hash",
                    &ob,
                    &code_hash(INFERENCE_ORDERBOOK_TVC)?,
                )
                .await?,
            );
            let tc = Address::parse(&market.token_contract)?;
            checks.push(
                self.code_hash_account_check(
                    "TokenContract code hash",
                    &tc,
                    ROOTMODEL_PINNED_TC_CODE_HASH,
                )
                .await?,
            );
            checks.push(active_check(
                "market TokenContract state",
                &tc,
                self.token_contract_state(&tc).await?.is_some(),
            ));
            let seller_note = Address::parse(&market.seller_note)?;
            checks.push(self.seller_note_withdrawn_check(&seller_note).await?);
        } else {
            checks.push(skipped_check(
                "RootModel code hash",
                "pass --market <manifest> to check the seller's deployed RootModel",
            ));
            checks.push(skipped_check(
                "InferenceOrderBook code hash",
                "pass --market <manifest> to check a deployed order book",
            ));
            checks.push(skipped_check(
                "TokenContract code hash",
                "pass --market <manifest> to check a deployed TokenContract",
            ));
            checks.push(skipped_check(
                "market TokenContract state",
                "pass --market <manifest> to check manifest freshness",
            ));
            checks.push(skipped_check(
                "seller PrivateNote withdrawn state",
                "pass --market <manifest> to check the seller note's hasWithdrawn flag",
            ));
        }

        let mut versions = Vec::new();
        if let Some(v) = self.version_of(&self.superroot, SUPERROOT_ABI).await? {
            versions.push(("SuperRoot".to_string(), v));
        }
        if let Some(v) = self.version_of(&rootpn, ROOTPN_ABI).await? {
            versions.push(("RootPN".to_string(), v));
        }
        if let Some(v) = self.version_of(&rootoracle, ROOTORACLE_ABI).await? {
            versions.push(("RootOracle".to_string(), v));
        }
        Ok(ShellnetDoctorReport {
            network: self.deployed.network.clone(),
            versions,
            checks,
        })
    }

    /// The `SuperRoot` owner pubkey(on-chain getter `getOwnerPubkey`).
    pub async fn superroot_owner_pubkey(&self) -> Result<Value> {
        let v = self
            .client
            .run_getter(&self.superroot, SUPERROOT_ABI, "getOwnerPubkey", json!({}))
            .await?
            .ok_or_else(|| anyhow!("SuperRoot is not active"))?;
        Ok(v["value0"].clone())
    }

    /// The `RootModel` address for a given owner pubkey -- the deterministic SuperRoot on-chain getter
    /// `getRootModelAddress(ownerPubkey)`. RootModel is per-owner: for the seller(model owner)
    /// it is derived from their pubkey(see [`Self::deploy_root_model`]).
    pub async fn root_model_address_for(&self, owner_pubkey: &Value) -> Result<Address> {
        let v = self
            .client
            .run_getter(
                &self.superroot,
                SUPERROOT_ABI,
                "getRootModelAddress",
                json!({ "ownerPubkey": owner_pubkey }),
            )
            .await?
            .ok_or_else(|| anyhow!("SuperRoot is not active"))?;
        Address::parse(v["value0"].as_str().ok_or_else(|| anyhow!("no address"))?)
    }

    /// Derive the `RootModel` address of the `SuperRoot` owner(part of address resolution for `ChainBackend`).
    pub async fn resolve_root_model(&self) -> Result<Address> {
        let owner = self.superroot_owner_pubkey().await?;
        self.root_model_address_for(&owner).await
    }

    async fn root_model_deploy_msg(&self, owner: &KeyPair) -> Result<(Address, String)> {
        let ctx = local_context()?;
        let tc_code = code_boc_b64(TOKENCONTRACT_TVC)?;
        let init_data = json!({
            "_ownerPubkey": format!("0x{}", owner.public_hex()),
            "_superRootAddress": self.superroot.with_workchain(),
        });
        let ctor = json!({ "tokenContractCode": tc_code });
        let msg = build_deploy(
            &ctx,
            ROOTMODEL_ABI,
            ROOTMODEL_TVC,
            init_data,
            ctor,
            owner.public_hex(),
            owner.secret_hex(),
        )
        .await?;
        Ok((Address::parse(&msg.address)?, msg.message_boc_b64))
    }

    /// Derive the per-deal `TokenContract` address from `RootModel`(`getTokenContractAddress`)
    /// by the seller's pubkey and the deal nonce -- a deterministic on-chain getter.
    pub async fn resolve_token_contract(
        &self,
        root_model: &Address,
        seller_pubkey: &Value,
        nonce: u64,
    ) -> Result<Address> {
        let v = self
            .client
            .run_getter(
                root_model,
                ROOTMODEL_ABI,
                "getTokenContractAddress",
                json!({ "sellerPubkey": seller_pubkey, "nonce": nonce }),
            )
            .await?
            .ok_or_else(|| anyhow!("RootModel is not active"))?;
        Address::parse(v["value0"].as_str().ok_or_else(|| anyhow!("no address"))?)
    }

    /// Derive the per-deal `TokenContract` address from the deploy **INIT-DATA(stateInit)** -- the
    /// getter-free, offline counterpart to [`resolve_token_contract`](Self::resolve_token_contract).
    /// `provision_market`'s idempotency check must NOT depend on the RootModel `getTokenContractAddress`
    /// network getter: on a fresh provision the RootModel deploy was just sent but is not yet `Active`, so the
    /// getter 404s and `resolve_token_contract`'s `"RootModel is not active"` error would abort the **entire**
    /// idempotent provision -- exactly the case the check exists to handle. The TC address is `hash(stateInit)`
    /// over `{code, varInit {_sellerPubkey,_rootModelAddress,_nonce,_pubkey}}`; it needs no RootModel account,
    /// no network, and cannot 404. (Bit-for-bit the address the deploy creates -- cross-checked against the
    /// getter only on the idempotent-skip branch, where the RootModel is guaranteed `Active`.)
    #[allow(clippy::too_many_arguments)]
    pub async fn token_contract_deploy_address(
        &self,
        seller: &KeyPair,
        root_model: &Address,
        nonce: u64,
        model_name: &str,
        _tick_size: u128,
        price_per_tick: u128,
        max_ticks: u128,
        seller_note: &Address,
    ) -> Result<Address> {
        Ok(self
            .token_contract_deploy_msg(
                seller,
                root_model,
                nonce,
                model_name,
                price_per_tick,
                max_ticks,
                seller_note,
            )
            .await?
            .0)
    }

    /// Read the endpoint ciphertext from `TokenContract` -- getter
    /// `getEndpointCipher`. The same `Handover` format as in (the buyer
    /// decrypts with the note key). `None` if the contract is not active or the endpoint is not yet written.
    pub async fn read_handover(&self, token_contract: &Address) -> Result<Option<Vec<u8>>> {
        let Some(v) = self
            .client
            .run_getter(
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
            .run_getter(tc, TOKENCONTRACT_ABI, "getModelHash", json!({}))
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
            .run_getter(tc, TOKENCONTRACT_ABI, "getModelName", json!({}))
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

    /// The TC's on-chain **price per tick** (`getDeal() ->(tickSize, pricePerTick, maxTicks)`, 4.0.6) -- the
    /// authoritative deal price for the accounting view, NOT the operator-supplied manifest value.
    /// `uint128` decimal string. `None` if the TC is not active.
    pub async fn token_contract_price_per_tick(&self, tc: &Address) -> Result<Option<u128>> {
        let Some(v) = self
            .client
            .run_getter(tc, TOKENCONTRACT_ABI, "getDeal", json!({}))
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
            .run_getter(tc, TOKENCONTRACT_ABI, "getDeal", json!({}))
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

    /// Read the **buyer's ed25519 pubkey** from `TokenContract`(`getBuyerPubkey`, uint256) -- the book
    /// records it on a match(`placeInferenceBuy`). From it the seller **reconstructs the x25519 handover**
    /// and encrypts the endpoint to
    /// the recovered pubkey -- no separate x25519 channel is needed. `None` if the TC is not active or the buyer
    /// is not yet recorded(zero pubkey). The pubkey round-trips as `0x`-hex(like `getOwnerPubkey`).
    pub async fn token_contract_buyer_pubkey(&self, tc: &Address) -> Result<Option<[u8; 32]>> {
        let Some(v) = self
            .client
            .run_getter(tc, TOKENCONTRACT_ABI, "getBuyerPubkey", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let raw = v["value0"].as_str().unwrap_or("");
        let hex = raw.strip_prefix("0x").unwrap_or(raw);
        if hex.is_empty() {
            return Ok(None);
        }
        // uint256 -> 32 bytes BE(the pubkey may have arrived without leading zeros -- left-pad to 64 hex).
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

    /// Read the buyer note address from `TokenContract.getParties()`. `None` means the TC is inactive
    /// or has not recorded a buyer yet.
    pub async fn token_contract_buyer_note(&self, tc: &Address) -> Result<Option<Address>> {
        let Some(v) = self
            .client
            .run_getter(tc, TOKENCONTRACT_ABI, "getParties", json!({}))
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
            .run_getter(tc, TOKENCONTRACT_ABI, "getSeller", json!({}))
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
    /// `.tvc`(StateInit -> `.code`), like `airegistry::abi::Contract::code_boc_b64` in the SDK.
    pub fn inference_orderbook_code_b64() -> Result<String> {
        code_boc_b64(INFERENCE_ORDERBOOK_TVC)
    }

    pub fn canonical_inference_orderbook_address(model_hash: &str) -> Result<Address> {
        inference_orderbook_address_from_model_hash(model_hash)
    }

    /// Deterministic `InferenceOrderBook` address for(model, tick size) -- the note's on-chain getter
    /// `getInferenceOrderBookAddress(code, modelHash, tickSize)`. Success = the note has this
    /// method(meaning it is an inference note). `model_hash` is `0x...` uint256, `tick_size` is uint128.
    pub async fn inference_orderbook_address(
        &self,
        note: &Address,
        model_hash: &str,
        tick_size: u128,
    ) -> Result<Address> {
        let code = Self::inference_orderbook_code_b64()?;
        let v = self
            .client
            .run_getter(
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
            .run_getter(ob, INFERENCE_ORDERBOOK_ABI, "getParams", json!({}))
            .await
    }

    /// A signed external contract call(write) through the backend's **browser-UA** http
    /// client: `encode_external_call`(the same codec as `ChainClient::call`) -> submit to
    /// `/v2/messages`. The ChainClient is not used for writes -- its default UA is blocked by
    /// Cloudflare(getters through it work fine). Returns the submit response.
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

    /// Submit `boc` to `/v2/messages`. `deploy` selects the routing:
    /// - `false` -- a regular write to an **existing** contract(call/fund): `send_message`, which
    /// reads the real `dapp_id` via the BK REST `/v2/account`. A 404 there is a real error -> propagates.
    /// - `true` -- a **deploy-message send** whose destination is a not-yet-deployed self-dapp address:
    /// read the real `dapp_id`, but on the **specific `/v2/account` uninit-404**([`is_uninit_account_404`])
    /// fall back to `dapp_id = account_id`(self-dapp) and submit via `send_message_routed` (which skips
    /// the `/v2/account` read). This lets one `dexdo provision` land a fresh deploy in a SINGLE shot
    /// instead of dying on the first attempt and forcing a cumulative re-funded retry.
    /// **Scoped:** only the deploy/fund submit sites pass `deploy = true`; every regular write keeps the
    /// unchanged `send_message` path. Any non-`/v2/account` 404(or other error) still propagates.
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

    /// Submit a message to shellnet with retry on **transient** infrastructure failures:
    /// (1) overflow of the block manager's write queue(`QUEUE_OVERFLOW` -- "message queue is full");
    /// (2) **transient gateway 5xx** (`502 Bad Gateway` / `503` / `504` from the reverse proxy, when
    /// the backend is briefly unavailable -- observed to flicker on shellnet under load). The node is alive and moving
    /// blocks; we wait(exponential backoff, cap 8s) and retry -- this is resilience to a real network,
    /// not a test crutch. Other(logical) errors propagate immediately. `deploy` is threaded to
    /// [`submit_once`] so only deploy-message sends get the funded-uninit `/v2/account` 404 tolerance.
    async fn retry_submit(&self, boc: &str, deploy: bool) -> Result<Value> {
        self.clock_skew_preflight().await?;
        // Transient marker: the queue is full OR a temporary gateway failure(5xx) that clears on its own.
        fn is_transient(msg: &str) -> bool {
            msg.contains("QUEUE_OVERFLOW")
                || msg.contains("502")
                || msg.contains("503")
                || msg.contains("504")
                || msg.contains("Bad Gateway")
                || msg.contains("Service Unavailable")
                || msg.contains("Gateway Time")
        }
        let mut delay = std::time::Duration::from_secs(2);
        for attempt in 1..=8u32 {
            match self.submit_once(boc, deploy).await {
                Ok(v) => return Ok(v),
                Err(e) if is_transient(&e.to_string()) => {
                    eprintln!(
                        "shellnet transient submit error (attempt {attempt}): {e}; waiting {delay:?} then retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(8));
                }
                Err(e) => return Err(e),
            }
        }
        // Final attempt -- pass the result through as-is(Ok or the final error).
        self.submit_once(boc, deploy).await
    }

    /// Regular write to an **existing** contract(call/fund) -- unchanged `send_message` routing.
    pub(super) async fn send_with_retry(&self, boc: &str) -> Result<Value> {
        self.retry_submit(boc, false).await
    }

    /// A **deploy-message** send(its destination is a not-yet-deployed self-dapp address): tolerates
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
        // `sha256(model_name)`(the canonical preimage). `inferenceOrderBookCode`/`tickSize` are not in
        // the 2-arg ABI(the OB code is stored on the note) -- harmless extra keys, the encoder ignores them.
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

    /// The book's `getBestBidAsk` getter(`hasBid`, `bid`, `hasAsk`, `ask`) -- a check that the offer landed
    /// in the order book as an ask. `None` if the book is not active.
    pub async fn inference_orderbook_best_bid_ask(&self, ob: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(ob, INFERENCE_ORDERBOOK_ABI, "getBestBidAsk", json!({}))
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
        let dapp_id = fetch_dapp_id(&self.http, endpoint, &account_id).await?;
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
        let resp: Value = self
            .http
            .post(&gql)
            .json(&json!({
                "query": query,
                "variables": { "accountId": account_id, "dappId": dapp_id, "last": 200 },
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
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
                                fill.token_contract
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
        let response: Value = self
            .http
            .post(&gql)
            .json(&json!({
                "query": query,
                "variables": { "accountId": account_id, "dappId": dapp_id, "last": 200 },
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
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
        let messages =
            fetch_all_ext_out_messages(&self.http, self.client.endpoint(), &account_id).await?;
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
                                fill.token_contract
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
            std::time::Duration::from_secs(2),
            &timeout_context,
        )
        .await
    }

    /// The book's `getStats` getter(`nextOrderId`, `orderCount`, `executedNotional`, `executedTicks`).
    pub async fn inference_orderbook_stats(&self, ob: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(ob, INFERENCE_ORDERBOOK_ABI, "getStats", json!({}))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn inference_subscription_placements_since(
        &self,
        ob: &Address,
        buyer_note: &Address,
        order_id_floor: u128,
        max_price_per_tick: u128,
        ticks: u128,
        cycle_budget: u128,
        auto_renew: bool,
    ) -> Result<Vec<InferenceSubscriptionPlacement>> {
        let account_id = ob.bare().to_string();
        let messages =
            fetch_all_ext_out_messages(&self.http, self.client.endpoint(), &account_id).await?;
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
                        placement.buyer_note
                    )
                })?
                .with_workchain();
            if placement.order_id < order_id_floor
                || !owner.eq_ignore_ascii_case(&buyer_note)
                || placement.max_price_per_tick != max_price_per_tick
                || placement.ticks != ticks
                || placement.cycle_budget != cycle_budget
                || placement.auto_renew != auto_renew
            {
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
        placements.sort_by_key(|placement| (placement.order_id, placement.created_at));
        placements.dedup_by_key(|placement| placement.order_id);
        Ok(placements)
    }

    /// The book's `getWeeklyMedianPrice` getter. `None` means the book is inactive; a live active
    /// book with no matched volume returns the contract's `ERR_NO_LIQUIDITY` through the TVM getter error.
    pub async fn inference_orderbook_weekly_median_price(
        &self,
        ob: &Address,
    ) -> Result<Option<u128>> {
        let Some(v) = self
            .client
            .run_getter(
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

    /// The book's `getOrder(id)` getter -- resolves a specific order/offer(note, `tokenContract`, price...).
    pub async fn inference_orderbook_order(&self, ob: &Address, id: u128) -> Result<Option<Value>> {
        self.client
            .run_getter(
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
            return Ok(false);
        };
        let Some(note) = order.get("note").and_then(Value::as_str) else {
            return Err(anyhow!("getOrder({order_id}) has no owner note: {order}"));
        };
        let note = Address::parse(note)
            .map_err(|error| anyhow!("getOrder({order_id}) owner note {note}: {error}"))?
            .with_workchain();
        let owner_note = Address::parse(owner_note)
            .map_err(|error| anyhow!("expected owner note {owner_note}: {error}"))?
            .with_workchain();
        let is_buy = order
            .get("isBuy")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("getOrder({order_id}) has no isBuy: {order}"))?;
        let amount = getter_u128(&order, "amount")
            .ok_or_else(|| anyhow!("getOrder({order_id}) has no amount: {order}"))?;
        Ok(is_buy && amount > 0 && note.eq_ignore_ascii_case(&owner_note))
    }

    /// The book's `getSubscription(orderId)` getter. `exists=false` means the order is not a live
    /// subscription(it may be a plain order, cancelled, filled, or expired).
    pub async fn inference_orderbook_subscription(
        &self,
        ob: &Address,
        order_id: u128,
    ) -> Result<Option<OrderBookSubscription>> {
        let Some(v) = self
            .client
            .run_getter(
                ob,
                INFERENCE_ORDERBOOK_ABI,
                "getSubscription",
                json!({ "orderId": order_id.to_string() }),
            )
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(OrderBookSubscription {
            order_id,
            exists: v["exists"].as_bool().unwrap_or(false),
            period_start: v.get("periodStart").and_then(value_u64).unwrap_or(0),
            cur_cycle: v
                .get("curCycle")
                .and_then(value_u64)
                .unwrap_or(0)
                .min(u8::MAX as u64) as u8,
            cycle_budget: getter_u128(&v, "cycleBudget").unwrap_or(0),
            cycle_spent: getter_u128(&v, "cycleSpent").unwrap_or(0),
            auto_renew: v["autoRenew"].as_bool().unwrap_or(false),
        }))
    }

    /// The book's `getForfeit(orderId, cycle)` getter. This is read-only evidence for the
    /// subscription full-fill path: the current-cycle unspent budget is recorded for that cycle's
    /// sellers, while future-cycle residual must not remain stranded in the order.
    pub async fn inference_orderbook_forfeit(
        &self,
        ob: &Address,
        order_id: u128,
        cycle: u8,
    ) -> Result<Option<(u128, u128)>> {
        let Some(v) = self
            .client
            .run_getter(
                ob,
                INFERENCE_ORDERBOOK_ABI,
                "getForfeit",
                json!({ "orderId": order_id.to_string(), "cycle": cycle }),
            )
            .await?
        else {
            return Ok(None);
        };
        Ok(Some((
            getter_u128(&v, "pool").unwrap_or(0),
            getter_u128(&v, "fundedTicks").unwrap_or(0),
        )))
    }

    /// The seller submits exactly one owner-signed external call to the note:
    /// `postSellOffer(flags, nonce)`. In 4.0.26 the note derives the canonical per-deal
    /// `TokenContract`, and the TC supplies its constructor-bound model, price, maximum ticks,
    /// and seller note when it posts the ask internally. `flags=0` is a plain resting limit.
    pub async fn post_sell_offer(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        flags: u8,
        nonce: u64,
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "postSellOffer",
            json!({
                "flags": flags,
                "nonce": nonce.to_string(),
            }),
            owner_keys,
        )
        .await
    }

    /// The buyer(note) places a limit buy for inference -- `placeInferenceBuy(modelHash,
    /// maxPricePerTick, ticks, escrow, flags, deadline)`(signed with the note's owner key). The escrow is ECC
    /// SHELL(currency 2): the note moves `escrow` from its ECC balance into the book. `deadline=0` = GTC.
    /// If `maxPricePerTick` >= the resting ask -- a match happens immediately(the book calls `fundFromOrderBook` on the TC).
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
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "placeInferenceBuy",
            json!({
                "modelHash": model_hash,
                "maxPricePerTick": max_price_per_tick.to_string(),
                "ticks": ticks.to_string(),
                "escrow": escrow.to_string(),
                "flags": flags,
                "deadline": deadline.to_string(),
            }),
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
        before_post: &mut (dyn FnMut(String, MatchWatchCursor) -> Result<()> + Send),
    ) -> Result<Value> {
        let (endpoint, boc, account_id, dapp_id) = self
            .prepare_money_post(
                note,
                PRIVATENOTE_ABI,
                "placeInferenceBuy",
                json!({
                    "modelHash": model_hash,
                    "maxPricePerTick": max_price_per_tick.to_string(),
                    "ticks": ticks.to_string(),
                    "escrow": escrow.to_string(),
                    "flags": flags,
                    "deadline": deadline.to_string(),
                }),
                owner_keys,
            )
            .await?;
        let mut final_cursor = MatchWatchCursor::new(0);
        self.poll_inference_filled_tcs(note, order_book, true, &mut final_cursor)
            .await
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        *cursor = final_cursor;
        before_post(money_submit_identity(&boc), cursor.clone())
            .map_err(|source| anyhow::Error::new(MoneySubmitError::Preparation { source }))?;
        send_message_routed_money_once(
            &self.money_post_http,
            &endpoint,
            &boc,
            &account_id,
            &dapp_id,
        )
        .await
    }

    /// The buyer(note) places a recurring inference subscription through
    /// `PrivateNote.placeInferenceSubscription`. The escrow is exact fee-inclusive SHELL selected by
    /// the CLI; surplus is intentionally not sent.
    #[allow(clippy::too_many_arguments)]
    pub async fn place_inference_subscription(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        model_hash: &str,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
        auto_renew: bool,
    ) -> Result<Value> {
        let order_book = self
            .inference_orderbook_address(note, model_hash, MODEL_TICK_SIZE)
            .await?;
        let mut fill_cursor = MatchWatchCursor::new(0);
        let mut ignore_identity =
            |_: String, _: u128, _: MatchWatchCursor, _: Vec<(u128, MatchedFill)>| Ok(());
        self.place_inference_subscription_with_identity_and_cursors(
            note,
            owner_keys,
            &order_book,
            model_hash,
            max_price_per_tick,
            ticks,
            escrow,
            auto_renew,
            &mut fill_cursor,
            &mut ignore_identity,
        )
        .await
    }

    /// Prepare one subscription money message, anchor its placement/fill cursors,
    /// persist its identity through `before_post`, then POST exactly once.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub async fn place_inference_subscription_with_identity_and_cursors(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        order_book: &Address,
        model_hash: &str,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
        auto_renew: bool,
        fill_cursor: &mut MatchWatchCursor,
        before_post: &mut (dyn FnMut(String, u128, MatchWatchCursor, Vec<(u128, MatchedFill)>) -> Result<()>
                  + Send),
    ) -> Result<Value> {
        let (endpoint, boc, account_id, dapp_id) = self
            .prepare_money_post(
                note,
                PRIVATENOTE_ABI,
                "placeInferenceSubscription",
                json!({
                    "modelHash": model_hash,
                    "maxPricePerTick": max_price_per_tick.to_string(),
                    "ticks": ticks.to_string(),
                    "escrow": escrow.to_string(),
                    "autoRenew": auto_renew,
                }),
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
                        order_book.with_workchain()
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
        send_message_routed_money_once(
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

    /// The `getState` getter of the `TokenContract` deal (`funded`, `opened`, `probeAccepted`, `disputed`,
    /// `deposit`, `prepaid`, `frozen`, `finalizedOwed`,...). After a match `funded` becomes `true`
    /// (the book funded the TC via `fundFromOrderBook`). `None` if the TC is not active.
    pub async fn token_contract_state(&self, tc: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(tc, TOKENCONTRACT_ABI, "getState", json!({}))
            .await
    }

    /// The `getProbe` getter of the deal(`probeFunded`, `probeLocked`, `probeCommission`).
    pub async fn token_contract_probe(&self, tc: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(tc, TOKENCONTRACT_ABI, "getProbe", json!({}))
            .await
    }

    /// The `getConfig` getter of the deal(`TokenContract`, 4.0.5 `view`): the per-deal
    /// `settleWindow`/`streamTimeout`(dynamic, scaled by `pricePerTick`) plus `platformFeeBps` and
    /// `disputeWindow`. The seller advance driver reads this per deal to time the stream cadence
    /// (`settleWindow`) and reclaim/timeout(`streamTimeout`); the fixed probe window is NOT in here
    /// . `None` if the TC is not active.
    pub async fn token_contract_config(&self, tc: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(tc, TOKENCONTRACT_ABI, "getConfig", json!({}))
            .await
    }

    /// The `getStreamLocks` getter of the `PrivateNote` note(`streamCount`, `disputeCount`, `lastChange`):
    /// direct proof that "the note is locked". After `TC.dispute()` both notes have
    /// `disputeCount > 0` -- until the dispute is resolved, a new offer/withdrawal from the note is rejected
    /// (`ERR_STREAM_LOCKED`). `None` if the note is not active.
    pub async fn note_stream_locks(&self, note: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(note, PRIVATENOTE_ABI, "getStreamLocks", json!({}))
            .await
    }

    /// Read only the authoritative `getStreamLocks` counters and `lastChange`, without the
    /// transaction-history reconstruction performed by [`Self::note_stream_lock_status`].
    pub async fn note_stream_lock_snapshot(
        &self,
        note: &Address,
    ) -> Result<Option<NoteStreamLockSnapshot>> {
        let Some(raw) = self.note_stream_locks(note).await? else {
            return Ok(None);
        };
        let stream_count: u32 = getter_u128(&raw, "streamCount")
            .ok_or_else(|| anyhow!("PrivateNote {note} getStreamLocks has no streamCount"))?
            .try_into()
            .map_err(|_| anyhow!("PrivateNote {note} streamCount exceeds u32"))?;
        let dispute_count: u32 = getter_u128(&raw, "disputeCount")
            .ok_or_else(|| anyhow!("PrivateNote {note} getStreamLocks has no disputeCount"))?
            .try_into()
            .map_err(|_| anyhow!("PrivateNote {note} disputeCount exceeds u32"))?;
        let last_change_unix: u64 = getter_u128(&raw, "lastChange")
            .ok_or_else(|| anyhow!("PrivateNote {note} getStreamLocks has no lastChange"))?
            .try_into()
            .map_err(|_| anyhow!("PrivateNote {note} lastChange exceeds u64"))?;
        Ok(Some(NoteStreamLockSnapshot {
            stream_count,
            dispute_count,
            last_change_unix,
        }))
    }

    /// Read the authoritative lock counters and reconstruct the active deal addresses from successful
    /// inbound `stream*Lock`/`stream*Unlock` calls. `forceClearStreamLocks` is folded as a reset.
    pub async fn note_stream_lock_status(
        &self,
        note: &Address,
    ) -> Result<Option<NoteStreamLockStatus>> {
        let Some(snapshot) = self.note_stream_lock_snapshot(note).await? else {
            return Ok(None);
        };
        let entries = if snapshot.stream_count == 0 && snapshot.dispute_count == 0 {
            Vec::new()
        } else {
            self.note_stream_lock_entries(note).await?
        };
        Ok(Some(NoteStreamLockStatus::from_entries(
            snapshot.stream_count,
            snapshot.dispute_count,
            snapshot.last_change_unix,
            entries,
        )))
    }

    async fn note_stream_lock_entries(&self, note: &Address) -> Result<Vec<NoteStreamLockEntry>> {
        const PAGE_SIZE: u32 = 1_000;
        let account_id = note.bare().to_string();
        let endpoint = self.client.endpoint().trim_end_matches('/');
        let dapp_id = fetch_dapp_id(&self.http, endpoint, &account_id).await?;
        let gql = format!("{endpoint}/graphql");
        let query = r#"
            query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
              blockchain {
                account(account_id: $accountId, dapp_id: $dappId) {
                  messages(msg_type: [ExtIn, IntIn], last: $last, before: $before) {
                    pageInfo { startCursor hasPreviousPage }
                    edges {
                      cursor
                      node {
                        id body src created_at
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
        let mut decoded = Vec::new();
        loop {
            let response: Value = self
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
                .await?
                .error_for_status()?
                .json()
                .await?;
            if let Some(errors) = response.get("errors") {
                return Err(anyhow!(
                    "PrivateNote {note} inbound-message GraphQL errors: {errors}"
                ));
            }
            let messages = response
                .pointer("/data/blockchain/account/messages")
                .ok_or_else(|| {
                    anyhow!("PrivateNote {note} inbound-message GraphQL shape changed: {response}")
                })?;
            let edges = messages["edges"].as_array().ok_or_else(|| {
                anyhow!("PrivateNote {note} inbound-message GraphQL edges missing: {response}")
            })?;
            for edge in edges {
                let cursor = edge["cursor"]
                    .as_str()
                    .ok_or_else(|| anyhow!("PrivateNote {note} inbound message has no cursor"))?;
                let node = &edge["node"];
                let id = node["id"].as_str().unwrap_or(cursor);
                if !seen.insert(id.to_string()) {
                    continue;
                }
                let Some((created_at, body, internal, internal_source)) =
                    successful_inbound_lock_call(node)?
                else {
                    continue;
                };
                decoded.push((
                    created_at,
                    cursor.to_string(),
                    body.to_string(),
                    internal,
                    internal_source.map(str::to_string),
                ));
            }
            let Some(next) = previous_page_cursor(
                &format!("PrivateNote {note} inbound-message"),
                messages,
                before.as_deref(),
            )?
            else {
                break;
            };
            before = Some(next);
        }
        decoded.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        reconstruct_note_stream_lock_entries(decoded.iter().map(
            |(created_at, _, body, internal, internal_source)| {
                (
                    *created_at,
                    body.as_str(),
                    *internal,
                    internal_source.as_deref(),
                )
            },
        ))
    }

    /// Read-only `PrivateNote.getDetails()`: public balance/lock maps and metadata, no key and no signed call.
    pub async fn private_note_details(&self, note: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await
    }

    /// Read every successful owner-signed `placeInferenceBuy` receipt for one note. This is intended
    /// for live by-fact verification: it counts destination transactions, not CLI log events. The
    /// shellnet indexer can omit `body` for external-in messages, so decode the authoritative full
    /// message BOC when that projection is absent.
    #[cfg(feature = "test-giver")]
    pub async fn successful_place_inference_buy_receipts(
        &self,
        note: &Address,
    ) -> Result<Vec<PlaceInferenceBuyReceipt>> {
        const PAGE_SIZE: u32 = 1_000;
        let account_id = note.bare().to_string();
        let endpoint = self.client.endpoint().trim_end_matches('/');
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
            let response: Value = self
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
                .await?
                .error_for_status()?
                .json()
                .await?;
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

    /// Read ordered lifecycle receipts for one deal. `StreamStopped` proves the clean
    /// post-probe-accept split; `ProbeBurned` proves the mutually exclusive BurnBoth path.
    #[cfg(feature = "test-giver")]
    pub async fn token_contract_settlement_receipts(
        &self,
        token_contract: &Address,
    ) -> Result<TokenContractSettlementReceipts> {
        let account_id = token_contract.bare().to_string();
        let messages =
            fetch_all_ext_out_messages(&self.http, self.client.endpoint(), &account_id).await?;
        decode_token_contract_settlement_receipts(messages)
    }

    /// Read-only buyer preflight for the final-withdrawal latch. A withdrawn PrivateNote cannot call
    /// `placeInferenceBuy`; detect that state before any money write and return the actionable chain
    /// refusal instead of the raw exit code. Read errors are retried because a transient getter failure
    /// is not evidence that the note withdrew. Older contract generations without `hasWithdrawn` remain
    /// usable: the guard records that it could not inspect the latch and fails open.
    pub async fn assert_note_can_place_inference_buy(&self, note: &Address) -> Result<()> {
        const ATTEMPTS: u32 = 3;
        let mut delay = std::time::Duration::from_millis(250);
        let mut details = None;
        for attempt in 1..=ATTEMPTS {
            match self.private_note_details(note).await {
                Ok(value) => {
                    details = Some(value);
                    break;
                }
                Err(error) if attempt < ATTEMPTS => {
                    eprintln!(
                        "buyer place preflight getDetails read failed (attempt {attempt}/{ATTEMPTS}): \
                         {error}; retrying after {delay:?}"
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "buyer place preflight could not read PrivateNote.getDetails for note {note} \
                             after {ATTEMPTS} attempts"
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
                "seller post_offer aborted: note {note} returned no PrivateNote.getDetails; cannot read \
                 hasWithdrawn before postSellOffer. Re-mint/deploy a fresh note against the current contracts."
            )
        })?;
        let withdrawn = details_has_withdrawn(&details).ok_or_else(|| {
            anyhow!(
                "seller post_offer aborted: PrivateNote.getDetails for note {note} has no hasWithdrawn field; \
                 refusing to submit postSellOffer without proving the note is not withdrawn"
            )
        })?;
        if withdrawn {
            return Err(anyhow!(note_withdrawn_sell_offer_message(note)));
        }
        Ok(())
    }

    /// Directive -- the note pre-funds its own RootModel + TC **uninit deploy addresses** from its ECC[2],
    /// via the `PrivateNote` owner-method `fundDeployShell(nonce, rootModelShell, tcShell)`(4.0.7). The note
    /// derives both targets internally from `(ephemeralPubkey, nonce)`, so no caller-supplied address -- this
    /// replaces the operator multisig's [`fund_deploy_from_wallet_ecc`](Self::fund_deploy_from_wallet_ecc) on the
    /// operate path. The RootModel/TC *deploys* stay external seller-signed; this call only pre-funds. The call is
    /// an external owner-signed message to the note, exactly like [`deploy_inference_orderbook`](Self::deploy_inference_orderbook).
    pub async fn note_fund_deploy_shell(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        nonce: u64,
        root_model_shell: u128,
        tc_shell: u128,
    ) -> Result<Value> {
        let boc = Self::encode_signed_call_boc(
            note,
            PRIVATENOTE_ABI,
            "fundDeployShell",
            json!({
                "nonce": nonce.to_string(),
                "rootModelShell": root_model_shell.to_string(),
                "tcShell": tc_shell.to_string(),
            }),
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

    pub(super) async fn active_native_balance(&self, addr: &Address) -> Result<u128> {
        let account = self
            .client
            .get_account(addr)
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

    async fn wait_native_balance_at_least(&self, addr: &Address, min: u128) -> Result<()> {
        for _ in 0..20 {
            if self.active_native_balance(addr).await? > min {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let balance = self.active_native_balance(addr).await?;
        Err(anyhow!(
            "contract {addr} native balance {balance} did not rise above gas-health floor {min}"
        ))
    }

    async fn account_snapshot(&self, addr: &Address) -> String {
        match self.client.get_account(addr).await {
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
            "deploy-prefund {stage}: note {note} [{}]; RootModel {rm} [{}]; TokenContract {tc} [{}]",
            self.account_snapshot(note).await,
            self.account_snapshot(rm).await,
            self.account_snapshot(tc).await,
        );
    }

    /// before an active RootModel / per-deal TC write, ensure the contract still has native
    /// vmshell gas. `fundDeployShell` is seller-note-owned and derives both targets from
    /// `(seller pubkey, nonce)`, so only call this from paths that hold the seller note/key/nonce.
    pub async fn ensure_deal_contract_gas(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        nonce: u64,
        root_model: Option<&Address>,
        token_contract: Option<&Address>,
    ) -> Result<()> {
        let mut rm_top_up = 0;
        let mut tc_top_up = 0;

        if let Some(rm) = root_model {
            let balance = self.active_native_balance(rm).await?;
            rm_top_up =
                gas_health_top_up_amount(balance, GAS_HEALTH_MIN, GAS_HEALTH_TARGET).unwrap_or(0);
        }
        if let Some(tc) = token_contract {
            let balance = self.active_native_balance(tc).await?;
            tc_top_up =
                gas_health_top_up_amount(balance, GAS_HEALTH_MIN, GAS_HEALTH_TARGET).unwrap_or(0);
        }

        if rm_top_up == 0 && tc_top_up == 0 {
            return Ok(());
        }

        eprintln!(
            "gas-health: topping up RootModel {rm_top_up} + TokenContract {tc_top_up} native nanotokens via note fundDeployShell"
        );
        self.note_fund_deploy_shell(note, owner_keys, nonce, rm_top_up, tc_top_up)
            .await?;

        if rm_top_up > 0 {
            if let Some(rm) = root_model {
                self.wait_native_balance_at_least(rm, GAS_HEALTH_MIN)
                    .await?;
            }
        }
        if tc_top_up > 0 {
            if let Some(tc) = token_contract {
                self.wait_native_balance_at_least(tc, GAS_HEALTH_MIN)
                    .await?;
            }
        }
        Ok(())
    }

    /// Directive -- the note posts the probe-commission to the nonce-derived `TokenContract` from its own
    /// ECC[2], via the `PrivateNote` owner-method `postProbeCommission(nonce, amount)`(4.0.7) -- replaces the
    /// operator multisig's [`fund_probe_commission`](Self::fund_probe_commission). External owner-signed message.
    pub async fn note_post_probe_commission(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        nonce: u64,
        amount: u128,
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "postProbeCommission",
            json!({
                "nonce": nonce.to_string(),
                "amount": amount.to_string(),
            }),
            owner_keys,
        )
        .await
    }

    /// The seller opens a stream session: `open(endpointCipher)`(external signature `_sellerPubkey`).
    /// Freezes a probe-tick from the deposit
    /// and writes the endpoint cipher -- handover(`RealNote::encrypt_to` to the buyer's x25519 pubkey).
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

    /// The seller advances the stream: `advance()`(external signature `_sellerPubkey`). The first call after
    /// `SETTLE_WINDOW`(180s) accepts the probe (probe-tick -> seller, commission is returned, sets the
    /// two-tick invariant); afterwards it finalizes the delivered tick.
    pub async fn advance_stream(&self, tc: &Address, seller_keys: &KeyPair) -> Result<Value> {
        self.submit(tc, TOKENCONTRACT_ABI, "advance", json!({}), seller_keys)
            .await
    }

    /// the seller CLOSES a STOPped deal's `TokenContract`. `destroy(payoutAddress)` is
    /// `onlyOwnerPubkey(_sellerPubkey)`, gated `!_opened && !_disputed` (the buyer's `stop()` clears
    /// `_opened` on close), and calls `selfdestruct(payoutAddress)`(`contracts/airegistry/TokenContract.sol:651`).
    /// External call, signed by the seller owner key(matches `_sellerPubkey`).
    /// **DESTRUCTIVE / BURNS(by-fact, 4.0.7):** the held ~`MIN_BALANCE` reserve does NOT recover to `payout`
    /// when `payout` is the cross-dapp note -- the note balance does not increase(reproduced x2). The deploy
    /// *funding* crossed dapps via `fundDeployShell` flag:16(credited); the raw `selfdestruct` *return* crossing
    /// the boundary is not credited -> the reserve is **burned at destroy**. So this closes the TC; reclaiming the
    /// reserve to the note would need a `TokenContract` flag:16/dapp-credit return fix(contract-side).
    /// NOT the dex/PMP oracle lifecycle.
    pub async fn destroy_token_contract(
        &self,
        tc: &Address,
        payout: &Address,
        seller_keys: &KeyPair,
    ) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "destroy",
            json!({ "payoutAddress": payout.with_workchain() }),
            seller_keys,
        )
        .await
    }

    /// The seller **concedes the dispute**: `releaseDispute()` on the TC (`onlyOwnerPubkey(_sellerPubkey)`) ->
    /// unlocks BOTH notes and **returns the tick to the buyer** (on the probe: probe+deposit to the buyer,
    /// commission to the seller, NO burn -- a concession is not a stop,/). Symmetric to `stream_dispute`,
    /// closing the anti-scam cycle of(lock -> resolution -> tick return).
    pub async fn release_dispute(&self, tc: &Address, seller_keys: &KeyPair) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "releaseDispute",
            json!({}),
            seller_keys,
        )
        .await
    }

    /// Withdraw finalized seller SHELL from a closed or still-open deal balance. This moves only the
    /// already-finalized `_finalizedOwed`; it is separate from `destroy`, which closes/selfdestructs the TC.
    pub async fn withdraw_shell(
        &self,
        tc: &Address,
        amount: u128,
        recipient: &Address,
        seller_keys: &KeyPair,
    ) -> Result<Value> {
        self.submit(
            tc,
            TOKENCONTRACT_ABI,
            "withdrawShell",
            json!({
                "amount": amount.to_string(),
                "recipient": recipient.with_workchain(),
            }),
            seller_keys,
        )
        .await
    }

    /// Submit owner-signed `PrivateNote.withdrawTokens(destWalletAddr, dapp_id)` for a note's available token
    /// balances. `dapp_id` is event metadata only(surfaced in `TokensWithdrawn`, drives no logic) -- taken from
    /// the deployed manifest. Fails on-chain if the note is stream-locked. Returns
    /// the submit result. Do not treat this helper as proof that every native/ECC balance is fully retired
    /// without by-fact evidence on the current deployed contract.
    pub async fn withdraw_note_tokens(
        &self,
        note: &Address,
        keys: &KeyPair,
        dest_wallet: &Address,
    ) -> Result<Value> {
        // One-shot guard: `withdrawTokens` sets `_hasWithdrawn=true` and reverts `ERR_INVALID_STATE` on a
        // re-call. Read `getDetails().hasWithdrawn` and fail
        // LOUD with a clear reason instead of the opaque `TVM_ERROR(compute phase)` the revert would produce.
        if let Some(d) = self
            .client
            .run_getter(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await?
        {
            let already = details_has_withdrawn(&d).unwrap_or(false);
            if already {
                return Err(anyhow!(
                    "note {note} was already withdrawn -- `withdrawTokens` is one-shot per note. Re-check the \
                     note/wallet on-chain before assuming any remaining balance is withdrawable."
                ));
            }
        }
        let dapp_id = format!("0x{}", self.deployed.dapp_id.trim_start_matches("0x"));
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "withdrawTokens",
            withdraw_note_tokens_payload(dest_wallet, &dapp_id),
            keys,
        )
        .await
    }

    /// The buyer stops the stream via their note: `streamStop(tokenContract)` -> `TC.stop()`
    /// (the TC checks `msg.sender == _buyer`). On the probe(before accept) -- burns the probe-tick and commission
    /// + returns the remaining deposit; in Streaming -- a standard split.
    pub async fn stream_stop(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
    ) -> Result<Value> {
        self.submit(
            buyer_note,
            PRIVATENOTE_ABI,
            "streamStop",
            json!({ "tokenContract": tc.with_workchain() }),
            buyer_keys,
        )
        .await
    }

    /// The buyer **opens a dispute** via their note: `streamDispute(tokenContract)` -> `TC.dispute()`
    /// (the TC checks `msg.sender == _buyer`). `TC.dispute()` locks **both** notes (`streamDisputeLock` on
    /// `_buyer` and `_sellerNote`,): until the dispute is resolved, new offers/withdrawals from a locked note
    /// are rejected(`ERR_STREAM_LOCKED`); `releaseDispute()` then returns the tick. The anti-scam `Dispute`
    /// of -- a real on-chain lock of the scammer's note(strictly stronger than `streamStop`).
    pub async fn stream_dispute(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
    ) -> Result<Value> {
        self.submit(
            buyer_note,
            PRIVATENOTE_ABI,
            "streamDispute",
            json!({ "tokenContract": tc.with_workchain() }),
            buyer_keys,
        )
        .await
    }

    /// The buyer reclaims the deal on a **seller-inactivity timeout**: the note sends
    /// `streamReclaim(tokenContract)` -> `TC.reclaimOnTimeout()`. Requires `block.timestamp >=
    /// _lastAdvance + STREAM_TIMEOUT`(600s) and `_opened`. On the probe(seller no-show) -- **no burn**:
    /// the probe and deposit are returned to the buyer, the commission to the seller.
    pub async fn reclaim_on_timeout(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
    ) -> Result<Value> {
        self.submit(
            buyer_note,
            PRIVATENOTE_ABI,
            "streamReclaim",
            json!({ "tokenContract": tc.with_workchain() }),
            buyer_keys,
        )
        .await
    }

    /// Prepare the signed reclaim and its route before synchronously checking whether accepted
    /// output changed. A changed heartbeat cancels before the single money POST.
    pub async fn reclaim_on_timeout_if(
        &self,
        buyer_note: &Address,
        buyer_keys: &KeyPair,
        tc: &Address,
        before_post: &mut (dyn FnMut() -> bool + Send),
    ) -> Result<Option<Value>> {
        prepare_reclaim_money_post_if(
            self.prepare_money_post(
                buyer_note,
                PRIVATENOTE_ABI,
                "streamReclaim",
                json!({ "tokenContract": tc.with_workchain() }),
                buyer_keys,
            ),
            before_post,
            |prepared| async move {
                let (endpoint, boc, account_id, dapp_id) = prepared;
                send_message_routed_money_once(
                    &self.money_post_http,
                    &endpoint,
                    &boc,
                    &account_id,
                    &dapp_id,
                )
                .await
            },
        )
        .await
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

    /// Directive -- `RootModel` deploy on the **note-funded** path: builds the same deploy message as
    /// [`deploy_root_model_from_wallet`](Self::deploy_root_model_from_wallet) but assumes the note has already
    /// pre-funded the uninit address with ECC[2] (via [`note_fund_deploy_shell`](Self::note_fund_deploy_shell));
    /// it only sends the external seller-signed deploy and waits for `Active`. No operator wallet.
    pub async fn deploy_root_model_note_funded(&self, owner: &KeyPair) -> Result<Address> {
        let (addr, message_boc_b64) = self.root_model_deploy_msg(owner).await?;
        // The note already pre-funded the uninit address(`fundDeployShell`); just send the deploy + wait.
        // Deploy-message send -> `send_deploy_with_retry` tolerates the funded-uninit `/v2/account` 404.
        let submit_err = self.send_deploy_with_retry(&message_boc_b64).await.err();
        if self.wait_active(&addr, 40).await {
            if let Some(e) = submit_err {
                eprintln!(
                    "deploy {addr} became Active after submit returned an error (treating as landed): {e}"
                );
            }
            Ok(addr)
        } else if let Some(e) = submit_err {
            Err(e)
        } else {
            Err(anyhow!(
                "deploy {addr} did not activate within the allotted time (note-funded)"
            ))
        }
    }

    /// Build the per-deal `TokenContract` deploy message **and its INIT-DATA(stateInit) address** -- offline,
    /// no send (`build_deploy` + `local_context()`). The single source of the per-deal TC derivation, shared by
    /// [`token_contract_deploy_address`](Self::token_contract_deploy_address) (the getter-free idempotency
    /// address,) and [`deploy_token_contract_note_funded`](Self::deploy_token_contract_note_funded) (the
    /// actual deploy) -- so the address checked for idempotency is bit-for-bit the one the deploy creates. The
    /// address is `hash(stateInit)` over `{code, varInit {_sellerPubkey,_rootModelAddress,_nonce,_pubkey}}`;
    /// the ctor args do **not** enter the address but `build_deploy` needs them to encode the message body.
    #[allow(clippy::too_many_arguments)]
    async fn token_contract_deploy_msg(
        &self,
        seller: &KeyPair,
        root_model: &Address,
        nonce: u64,
        model_name: &str,
        price_per_tick: u128,
        max_ticks: u128,
        seller_note: &Address,
    ) -> Result<(Address, String)> {
        let ctx = local_context()?;
        let init_data = json!({
            "_sellerPubkey": format!("0x{}", seller.public_hex()),
            "_rootModelAddress": root_model.with_workchain(),
            "_nonce": nonce.to_string(),
        });
        let ctor = json!({
            "modelName": model_name,
            "modelHash": model_hash_for(model_name),
            "pricePerTick": price_per_tick.to_string(),
            "maxTicks": max_ticks.to_string(),
            "sellerNote": seller_note.with_workchain(),
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
        .await?;
        Ok((Address::parse(&msg.address)?, msg.message_boc_b64))
    }

    /// Directive -- per-deal `TokenContract` deploy on the **note-funded** path: builds the deploy message
    /// (the note pre-funded the uninit address via `fundDeployShell`) and sends it, waiting for `Active`. No
    /// wallet. Shares [`token_contract_deploy_msg`](Self::token_contract_deploy_msg) with the idempotency
    /// derivation, so the deployed address equals the pre-derived one by construction.
    #[allow(clippy::too_many_arguments)]
    pub async fn deploy_token_contract_note_funded(
        &self,
        seller: &KeyPair,
        root_model: &Address,
        nonce: u64,
        model_name: &str,
        _tick_size: u128,
        price_per_tick: u128,
        max_ticks: u128,
        seller_note: &Address,
    ) -> Result<Address> {
        let (addr, message_boc_b64) = self
            .token_contract_deploy_msg(
                seller,
                root_model,
                nonce,
                model_name,
                price_per_tick,
                max_ticks,
                seller_note,
            )
            .await?;
        // The note already pre-funded the uninit address(`fundDeployShell`); just send the deploy + wait.
        // Deploy-message send -> `send_deploy_with_retry` tolerates the funded-uninit `/v2/account` 404.
        let submit_err = self.send_deploy_with_retry(&message_boc_b64).await.err();
        if self.wait_active(&addr, 40).await {
            if let Some(e) = submit_err {
                eprintln!(
                    "deploy {addr} became Active after submit returned an error (treating as landed): {e}"
                );
            }
            Ok(addr)
        } else if let Some(e) = submit_err {
            Err(e)
        } else {
            Err(anyhow!(
                "deploy {addr} did not activate within the allotted time (note-funded)"
            ))
        }
    }

    /// Provision a per-deal market for the seller (issue; **note-funded,** -- NO operator wallet, NO
    /// giver in the operate path): deploy-if-absent the per-model `InferenceOrderBook`, the per-owner
    /// `RootModel`, and the per-deal `TokenContract`, **all funded from the seller note's own ECC[2]**. Returns a
    /// [`MarketManifest`] whose `token_contract` is the **active** deployed address.
    /// The per-deal `TokenContract`(and `RootModel`) is a self-dapp contract whose uninit cross-dapp deploy
    /// address cannot be funded with privileged native gas(the 404). Instead the note pre-funds each uninit
    /// deploy address with **ECC[2] SHELL** via [`note_fund_deploy_shell`](Self::note_fund_deploy_shell)
    /// (`PrivateNote.fundDeployShell`, a single `flag:16` send so the ECC lands as spendable native balance), and
    /// the external seller-signed deploy then activates it -- the permission-free mechanism, no privileged giver,
    /// no separate operational wallet(the funding source is the anonymous note itself). `gas` is the ECC[2]
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
        // fail-closed up front if the seller note is orphaned by a contract redeploy -- a clear
        // "re-mint" error instead of a downstream bare TVM_ERROR(stale note) or "note is not active".
        self.assert_seller_note_current(note).await?;
        // 1) Per-model InferenceOrderBook -- note-funded(owner-method). Deploy-if-absent.
        let model_hash = model_hash_for(frame_model);
        let ob = self
            .inference_orderbook_address(note, &model_hash, MODEL_TICK_SIZE)
            .await?;
        if !self.wait_active(&ob, 1).await {
            self.deploy_inference_orderbook(
                note,
                seed_keys,
                &model_hash,
                frame_model,
                MODEL_TICK_SIZE,
            )
            .await?;
            if !self.wait_active(&ob, 40).await {
                return Err(anyhow!("InferenceOrderBook {ob} did not activate"));
            }
        }
        // 2) RootModel + per-deal TokenContract -- NOTE-FUNDED: no operator multisig. The note pre-funds
        // each uninit deploy address from its own ECC[2] (`fundDeployShell`, the note derives the targets from
        // `(ephemeralPubkey, nonce)`), then the external seller-signed deploy activates it. ORDER MATTERS: the
        // RootModel is deployed first so the per-deal TC registers into it in its ctor; the TC address itself is
        // derived **locally from the deploy INIT-DATA**, NOT by querying
        // the RootModel `getTokenContractAddress` getter -- so neither a fixed-superroot shellnet restart nor
        // a not-yet-`Active` RootModel can 404 the idempotency check. The getter is used only as a post-`Active`
        // cross-check below.
        let seller_pubkey = json!(format!("0x{}", seed_keys.public_hex()));
        let (rm, _) = self.root_model_deploy_msg(seed_keys).await?;
        let tc = self
            .token_contract_deploy_address(
                seed_keys,
                &rm,
                nonce,
                frame_model,
                MODEL_TICK_SIZE,
                price_per_tick,
                max_ticks,
                note,
            )
            .await?;
        let rm_absent = !self.wait_active(&rm, 1).await;
        if rm_absent {
            // Pre-fund the RootModel's(and the TC's -- same nonce) uninit deploy addresses, then deploy the RM.
            self.log_deploy_prefund_snapshot("before fundDeployShell", note, &rm, &tc)
                .await;
            let prefund = self
                .note_fund_deploy_shell(note, seed_keys, nonce, gas, gas)
                .await
                .context("note-funded provision: fundDeployShell ECC[2]/SHELL funding failed")?;
            eprintln!(
                "deploy-prefund submit: RootModel {gas} + TokenContract {gas} via fundDeployShell(nonce={nonce}) -> {prefund}"
            );
            self.log_deploy_prefund_snapshot("after fundDeployShell", note, &rm, &tc)
                .await;
            // Do not hard-gate on a visible balance at the uninit deploy address. On shellnet an uninit
            // pre-funded account can still read as absent/zero through account queries; the reliable proof is
            // fund -> deploy -> wait Active. If funding did not land, the deploy wait below fails with the
            // snapshots above in stderr.
            self.deploy_root_model_note_funded(seed_keys).await?;
        }
        self.ensure_deal_contract_gas(note, seed_keys, nonce, Some(&rm), None)
            .await?;
        // The per-deal TC address is derived from the deploy INIT-DATA(stateInit), NOT the RootModel
        // `getTokenContractAddress` network getter: on a fresh provision the RootModel deploy was just
        // sent(step above) but is not yet `Active`, so the getter would 404 and abort this idempotent check.
        if self.wait_active(&tc, 1).await {
            // Idempotent skip: the TC is already `Active` => the RootModel is guaranteed `Active`, so the getter
            // is safe here -- cross-check it agrees with the INIT-DATA derivation (catch a code-hash/derivation
            // divergence between the embedded TC image and the deployed RootModel).
            let getter_tc = self
                .resolve_token_contract(&rm, &seller_pubkey, nonce)
                .await?;
            if getter_tc.with_workchain() != tc.with_workchain() {
                return Err(anyhow!(
                    "RootModel getTokenContractAddress {getter_tc} != INIT-DATA-derived {tc} (TC derivation diverged)"
                ));
            }
        } else {
            // Deploy-if-absent. If the RootModel was already active(idempotent re-run), the TC was not
            // pre-funded above.
            if !rm_absent {
                self.log_deploy_prefund_snapshot("before fundDeployShell", note, &rm, &tc)
                    .await;
                let prefund = self
                    .note_fund_deploy_shell(note, seed_keys, nonce, 0, gas)
                    .await
                    .context(
                        "note-funded provision: fundDeployShell ECC[2]/SHELL funding failed",
                    )?;
                eprintln!(
                    "deploy-prefund submit: TokenContract {gas} via fundDeployShell(nonce={nonce}) -> {prefund}"
                );
                self.log_deploy_prefund_snapshot("after fundDeployShell", note, &rm, &tc)
                    .await;
            }
            let deployed = self
                .deploy_token_contract_note_funded(
                    seed_keys,
                    &rm,
                    nonce,
                    frame_model,
                    MODEL_TICK_SIZE,
                    price_per_tick,
                    max_ticks,
                    note,
                )
                .await?;
            // Post-deploy convergence guard: the deployed address must equal the INIT-DATA-derived one.
            if deployed.with_workchain() != tc.with_workchain() {
                return Err(anyhow!(
                    "deployed TC {deployed} != INIT-DATA-derived {tc} (derivation diverged)"
                ));
            }
        }
        self.ensure_deal_contract_gas(note, seed_keys, nonce, Some(&rm), Some(&tc))
            .await?;
        Ok(crate::MarketManifest {
            network: "shellnet".to_string(),
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

    pub async fn oracle_address(&self, oracle_name: &str) -> Result<Address> {
        let root = self.root_oracle_address().await?;
        let v = self
            .client
            .run_getter(
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
            .run_getter(
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

    pub async fn oracle_event_list_events(&self, oel: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(oel, ORACLEEVENTLIST_ABI, "_events", json!({}))
            .await
    }

    pub async fn oracle_range_data(&self, oel: &Address, event_id: &str) -> Result<Option<Value>> {
        self.client
            .run_getter(
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
            .run_getter(
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

    pub async fn pmp_details(&self, pmp: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(pmp, PMP_ABI, "getDetails", json!({}))
            .await
    }

    pub async fn pmp_order_book_address(&self, pmp: &Address) -> Result<Option<Address>> {
        let Some(v) = self
            .client
            .run_getter(pmp, PMP_ABI, "getOrderBookAddress", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let raw = v["orderBookAddress"]
            .as_str()
            .or_else(|| v["value0"].as_str());
        raw.map(Address::parse).transpose()
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
        if !self.wait_active(&oracle, 1).await {
            self.deploy_oracle(oracle_keys, oracle_name).await?;
            if !self.wait_active(&oracle, 40).await {
                return Err(anyhow!("Oracle {oracle} did not activate"));
            }
        }

        let oel = self
            .oracle_event_list_address(&oracle, event_list_index)
            .await?;
        if !self.wait_active(&oel, 1).await {
            self.deploy_oracle_event_list(
                &oracle,
                oracle_keys,
                event_list_index,
                event_list_description,
            )
            .await?;
            if !self.wait_active(&oel, 40).await {
                return Err(anyhow!("OracleEventList {oel} did not activate"));
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
        if !self.wait_active(&pmp, 1).await {
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
            if !self.wait_active(&pmp, 40).await {
                return Err(anyhow!("PMP {pmp} did not activate"));
            }
        }

        let details = self.wait_pmp_approved(&pmp).await?;
        let oracle_list_hash = value_to_uint256_hex(&details["oracleListHash"])
            .ok_or_else(|| anyhow!("PMP getDetails returned no oracleListHash"))?;
        let range = self
            .oracle_range_data(&oel, &event_id)
            .await?
            .ok_or_else(|| anyhow!("OracleEventList {oel} returned no range data"))?;
        if !range["exists"].as_bool().unwrap_or(false) {
            return Err(anyhow!(
                "OracleEventList {oel} has no range data for event {event_id}"
            ));
        }
        let range_ob = range["ob"].as_str().unwrap_or("");
        if normalize_addr(range_ob)? != normalize_addr(&market.inference_order_book)? {
            return Err(anyhow!(
                "range event OB {range_ob} != market inference_order_book {}",
                market.inference_order_book
            ));
        }
        let on_chain_bounds = range_bounds_to_uint256_hex(&range["bounds"]).ok_or_else(|| {
            anyhow!("OracleEventList {oel} returned invalid bounds for event {event_id}: {range:?}")
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
        for i in 0..20 {
            if let Some(id) = self
                .find_oracle_event_id(oel, event_name, deadline, describe, outcome_names)
                .await?
            {
                return Ok(id);
            }
            if i + 1 < 20 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
        Err(anyhow!(
            "range event `{event_name}` did not appear in OracleEventList {oel}"
        ))
    }

    async fn wait_pmp_approved(&self, pmp: &Address) -> Result<Value> {
        for i in 0..30 {
            if let Some(details) = self.pmp_details(pmp).await? {
                if details["approved"].as_bool().unwrap_or(false) {
                    return Ok(details);
                }
            }
            if i + 1 < 30 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
        let details = self.pmp_details(pmp).await?;
        Err(anyhow!(
            "PMP {pmp} did not become approved by oracle; last getDetails={details:?}"
        ))
    }

    /// fail-closed pre-flight: the seller note must be Active on-chain AND carry the **current**
    /// `PrivateNote` code(the embedded `PRIVATENOTE_TVC` hash). A `pn_pool` minted before a SuperRoot /
    /// PrivateNote redeploy is orphaned -- the note is either gone (a later getter 404s as "note is not
    /// active") or runs stale code whose deploy/registration into the rotated SuperRoot throws a bare
    /// `TVM_ERROR` in the compute phase. Catch both here with an actionable "re-mint your pool" message
    /// instead of letting provision fail opaquely downstream.
    pub async fn assert_seller_note_current(&self, note: &Address) -> Result<()> {
        let acc = self.client.get_account(note).await?.ok_or_else(|| {
            anyhow!(
                "seller note {note} is not on-chain -- the pn_pool is likely orphaned by a contract redeploy \
                 (SuperRoot/PrivateNote rotation). Re-mint against the current contracts (`mint_pn_pool`) and \
                 point DEXDO_PN_POOL at the fresh pool."
            )
        })?;
        if !acc.is_active() {
            return Err(anyhow!(
                "seller note {note} is {}, not Active -- re-mint the pn_pool against the current contracts \
                 (`mint_pn_pool`); a pool minted before a SuperRoot redeploy is orphaned.",
                acc.status
            ));
        }
        note_code_hash_current(note, acc.code_hash.as_deref())
    }

    /// Fund-safety guard for `note withdraw`. A PrivateNote deployed by a
    /// PREVIOUS contract generation -- its on-chain `code_hash` != the current
    /// `PRIVATENOTE_PINNED_CODE_HASH` -- still accepts the current-generation `withdrawTokens`
    /// message: it ZEROES the note's balance but does NOT credit the destination wallet, so the
    /// SHELL is lost. Refuse the withdraw BEFORE any on-chain write when the note is not the current
    /// generation. This does not recover funds already lost; it prevents zeroing a still-funded
    /// previous-generation note.
    pub async fn assert_note_withdraw_generation(&self, note: &Address) -> Result<()> {
        let acc = self
            .client
            .get_account(note)
            .await?
            .ok_or_else(|| anyhow!("note {note} is not on-chain; cannot withdraw"))?;
        if !acc.is_active() {
            return Err(anyhow!(
                "note {note} is {}, not Active; cannot withdraw",
                acc.status
            ));
        }
        note_withdraw_generation_ok(note, acc.code_hash.as_deref())
    }

    /// read the note's on-chain owner key (`getDetails().ephemeralPubkey`) and fail closed if it does not
    /// match the key the client will sign the owner-authenticated write with -- turning the opaque pre-accept
    /// `onlyOwnerPubkey` revert(branch 3: a non-conforming/orphaned note) into an actionable error. The buyer's
    /// `place_buy` calls it before `placeInferenceBuy`; the seller's `post_offer` before `postSellOffer`. An
    /// absent/empty `getDetails`(uninit/orphaned note) is itself a fail-closed re-mint case.
    pub async fn assert_note_owner_matches(
        &self,
        role: &str,
        note: &Address,
        signing_keys: &KeyPair,
    ) -> Result<()> {
        let details = self
            .client
            .run_getter(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "{role} aborted: note {note} returned no getDetails (not on-chain/active) -- the pn_pool is \
                     likely orphaned by a contract redeploy. Re-mint against the current contracts \
                     (`mint_pn_pool`) and point DEXDO_PN_POOL at the fresh pool."
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

    /// Poll `get_account(addr).is_active()` up to `tries` times(3s apart; `tries=1` = a single check).
    /// A query error or a not-yet-existent account(e.g. a self-dapp uninit address that 404s) counts
    /// as "not active" -- the caller then deploys or fails with a clear message.
    async fn wait_active(&self, addr: &Address, tries: u32) -> bool {
        for i in 0..tries {
            if let Ok(Some(a)) = self.client.get_account(addr).await {
                if a.is_active() {
                    return true;
                }
            }
            if i + 1 < tries {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn clock_skew_within_threshold_passes() {
        for local in [999_990, 1_000_030] {
            assert_eq!(
                clock_skew_check(local, 1_000_000).status,
                ShellnetDoctorStatus::Pass
            );
        }
    }

    #[test]
    fn clock_skew_real_boundaries_fail_closed_with_actionable_message() {
        for behind in [41, 60] {
            let check = clock_skew_check(1_000_000 - behind, 1_000_000);
            assert_eq!(check.status, ShellnetDoctorStatus::Fail);
            assert!(check.message.contains("CLOCK_SKEW"));
            assert!(check
                .message
                .contains(&format!("{behind}s behind chain time")));
        }
        let check = clock_skew_check(1_000_000 + MAX_CLOCK_AHEAD_SECS + 1, 1_000_000);
        assert_eq!(check.status, ShellnetDoctorStatus::Fail);
        assert!(check.message.contains("CLOCK_SKEW"));
        assert!(check.message.contains("251s ahead of chain time"));
        assert!(check.message.contains("Fix system time / NTP and retry"));

        let report = ShellnetDoctorReport {
            network: "shellnet".to_string(),
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

    async fn skew_fixture_backend(
        chain_offset: i64,
    ) -> (
        RealChainBackend,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let posts = Arc::new(AtomicUsize::new(0));
        let server_posts = Arc::clone(&posts);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let read = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                if request.starts_with("POST /v2/messages ") {
                    server_posts.fetch_add(1, Ordering::SeqCst);
                }
                let local = local_unix_secs().unwrap() as i64;
                let chain = (local + chain_offset) as u64;
                let body = json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":chain}}]}}}}).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let deployed = deployed("");
        let backend = RealChainBackend {
            client: ChainClient::connect(&endpoint).unwrap(),
            http: reqwest::Client::new(),
            money_post_http: build_money_post_http_client().unwrap(),
            superroot: Address::parse(&deployed.superroot).unwrap(),
            deployed,
        };
        (backend, posts, task)
    }

    #[tokio::test]
    async fn unsafe_clock_produces_zero_posts_in_regular_and_money_paths() {
        for chain_offset in [60, -300] {
            let (backend, posts, task) = skew_fixture_backend(chain_offset).await;
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
                        "action": {"result_code": 38, "success": false, "no_funds": true}
                    }
                },
                "account": {"info": {"id": TARGET, "dapp_id": DAPP}},
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
        assert_eq!(receipt.result_code, Some(38));
        assert_eq!(receipt.no_funds, Some(true));
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
                action_success: Some(false),
                result_code: Some(38),
                no_funds: Some(true),
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

    #[cfg(feature = "test-giver")]
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

    fn deployed(endpoint_field: &str) -> Deployed {
        serde_json::from_str(&format!(
            r#"{{
                "network": "shellnet",
                "superroot": "0:{zeros}",
                "dapp_config": "0:{zeros}",
                "dapp_id": "{zeros}",
                "seller_probe_commission_bps": 250
                {endpoint_field}
            }}"#,
            zeros = "0".repeat(64),
        ))
        .unwrap()
    }

    #[test]
    fn endpoint_default_is_shellnet_when_unset() {
        let endpoint = resolve_endpoint(None, &deployed("")).unwrap();
        assert_eq!(endpoint, DEFAULT_SHELLNET_ENDPOINT);
        assert_eq!(
            endpoint_urls(&endpoint).unwrap(),
            (
                "https://shellnet.ackinacki.org/graphql".into(),
                "https://shellnet.ackinacki.org/v2/account".into(),
            )
        );
    }

    #[cfg(feature = "test-giver")]
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

    #[cfg(feature = "test-giver")]
    #[test]
    fn settlement_receipts_decode_exact_payloads_in_chain_order() {
        let buyer = format!("0:{}", "44".repeat(32));
        let message = |id: &str, created_at: u64, event: &str, fields: Value| ExtOutMessage {
            id: id.to_string(),
            created_at,
            cursor: format!("{created_at:04}-{id}"),
            body: encode_token_contract_event(event, fields),
        };
        let receipts = decode_token_contract_settlement_receipts(vec![
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
            message(
                "tick-3",
                30,
                "TickFinalized",
                json!({"finalizedOwed": "3", "deposit": "0"}),
            ),
            message(
                "accepted",
                10,
                "ProbeAccepted",
                json!({
                    "buyer": buyer,
                    "toSeller": "1",
                    "commissionReturned": "0",
                }),
            ),
            message(
                "tick-2",
                20,
                "TickFinalized",
                json!({"finalizedOwed": "2", "deposit": "0"}),
            ),
        ])
        .expect("decode exact settlement lifecycle");

        assert_eq!(
            receipts.events,
            vec![
                TokenContractSettlementReceipt {
                    message_id: "accepted".to_string(),
                    created_at: 10,
                    event: TokenContractSettlementEvent::ProbeAccepted {
                        buyer: buyer.clone(),
                        to_seller: 1,
                        commission_returned: 0,
                    },
                },
                TokenContractSettlementReceipt {
                    message_id: "tick-2".to_string(),
                    created_at: 20,
                    event: TokenContractSettlementEvent::TickFinalized {
                        finalized_owed: 2,
                        deposit: 0,
                    },
                },
                TokenContractSettlementReceipt {
                    message_id: "tick-3".to_string(),
                    created_at: 30,
                    event: TokenContractSettlementEvent::TickFinalized {
                        finalized_owed: 3,
                        deposit: 0,
                    },
                },
                TokenContractSettlementReceipt {
                    message_id: "stop".to_string(),
                    created_at: 40,
                    event: TokenContractSettlementEvent::StreamStopped {
                        buyer,
                        to_seller: 0,
                        refund_to_buyer: 0,
                    },
                },
            ]
        );
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
            socket.read(&mut request).await.expect("read money POST");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write money POST response");
        });
        (format!("http://{address}"), task)
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
            socket.read(&mut request).await.expect("read money POST");
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
    async fn reclaim_heartbeat_change_after_prepare_skips_money_post() {
        use std::sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc,
        };

        let generation = Arc::new(AtomicU64::new(7));
        let heartbeat = crate::chain::HeartbeatGuard::new(Arc::clone(&generation));
        let sends = Arc::new(AtomicUsize::new(0));
        let prepare_generation = Arc::clone(&generation);
        let send_counter = Arc::clone(&sends);
        let mut before_post = || heartbeat.unchanged();

        let result = prepare_reclaim_money_post_if(
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
            socket.read(&mut request).await.expect("read money POST");
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
                    replay.read(&mut request).await.expect("read replayed POST");
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
            socket.read(&mut request).await.expect("read money POST");
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
    fn seller_note_withdrawn_check_fails_with_actionable_message() {
        let note =
            Address::parse("0:1111111111111111111111111111111111111111111111111111111111111111")
                .expect("address");
        let check = seller_note_withdrawn_check(&note, Some(true));
        assert_eq!(check.status, ShellnetDoctorStatus::Fail);
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

    async fn encoded_internal_note_call(method: &str) -> String {
        gosh_ackinacki::airegistry::calls::encode_internal_payload(
            &local_context().expect("local TVM context"),
            PRIVATENOTE_ABI,
            method,
            json!({
                "sellerPubkey": format!("0x{}", "1".repeat(64)),
                "nonce": "7",
            }),
        )
        .await
        .expect("encode PrivateNote internal call")
    }

    #[tokio::test]
    async fn stream_lock_fold_ignores_failed_encoded_inbound_calls() {
        const SUCCESSFUL_DEAL: &str =
            "0:1111111111111111111111111111111111111111111111111111111111111111";
        const FAILED_DEAL: &str =
            "0:2222222222222222222222222222222222222222222222222222222222222222";
        let successful_body = encoded_internal_note_call("streamLock").await;
        let failed_body = encoded_internal_note_call("streamLock").await;
        let successful = json!({
            "body": successful_body,
            "src": SUCCESSFUL_DEAL,
            "created_at": 10,
            "dst_transaction": {
                "aborted": false,
                "compute": {"exit_code": 0, "success": true},
                "action": {"result_code": 0, "success": true}
            }
        });
        let failed = json!({
            "body": failed_body,
            "src": FAILED_DEAL,
            "created_at": 11,
            "dst_transaction": {
                "aborted": true,
                "compute": {"exit_code": 151, "success": false},
                "action": {"result_code": 0, "success": true}
            }
        });

        let mut calls = Vec::new();
        for node in [&successful, &failed] {
            if let Some((created_at, body, internal, internal_source)) =
                successful_inbound_lock_call(node).expect("inspect inbound call")
            {
                calls.push((
                    created_at,
                    body.to_string(),
                    internal,
                    internal_source.map(str::to_string),
                ));
            }
        }
        let status = NoteStreamLockStatus::from_successful_inbound_calls(
            1,
            0,
            10,
            calls
                .iter()
                .map(|(created_at, body, internal, internal_source)| {
                    (
                        *created_at,
                        body.as_str(),
                        *internal,
                        internal_source.as_deref(),
                    )
                }),
        )
        .expect("decode and fold successful inbound calls");

        assert_eq!(
            calls.len(),
            1,
            "failed inbound call must not reach the fold"
        );
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].deal, SUCCESSFUL_DEAL);
        assert_ne!(status.entries[0].deal, FAILED_DEAL);
        assert!(status.history_complete);
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
}
