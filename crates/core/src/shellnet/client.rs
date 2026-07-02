use super::backends::{note_owner_mismatch_reason, MODEL_TICK_SIZE};
use super::contracts_provision::*;
use crate::chain::{MatchWatchCursor, OrderBookSubscription};
use crate::manifest::{model_hash_for, MarketManifest};
use crate::onchain_diagnostics::validate_onchain_submit_response;
use crate::oracle_manifest::OracleMarketManifest;
use anyhow::{anyhow, Result};
use gosh_ackinacki::airegistry::calls::encode_external_call;
use gosh_ackinacki::airegistry::deploy::{build_deploy, local_context};
use gosh_ackinacki::sdk::{Address, ChainClient, ChainLiveness, KeyPair};
use gosh_ackinacki::wallet::query::{dest_account_id_hex, fetch_dapp_id};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

const FIXED_SUPERROOT_ACCOUNT_ID: &str =
    "0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c";
const MIN_PMP_INITIAL_STAKE: u128 = 10_000_000;

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
    /// Network(for shellnet -- `"shellnet"`; the connection goes through [`ChainClient::shellnet`]).
    pub network: String,
    /// `SuperRoot` airegistry -- the derivation point for `RootModel`/`InferenceOrderBook`.
    pub superroot: String,
    /// `DappConfig`(a DApp with unlimited credit for deploys).
    pub dapp_config: String,
    /// `dapp_id`(= account_id of `SuperRoot`).
    pub dapp_id: String,
    /// The seller's probe-tick commission in bps.
    pub seller_probe_commission_bps: u16,
}

impl Deployed {
    /// Read the manifest from a file(`contracts/deployed.shellnet.json`).
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path.as_ref())?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Real on-chain backend on top of `gosh.ackinacki` `ChainClient`.
/// Carries a live connection to shellnet and the root addresses from the manifest.
pub struct RealChainBackend {
    client: ChainClient,
    /// Browser-UA http client for writes(submitting messages to `/v2/messages`).
    pub(super) http: reqwest::Client,
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

fn checked_submit_response(resp: Value) -> Result<Value> {
    validate_onchain_submit_response(resp).map_err(|e| {
        tracing::debug!(
            payload = %e.sanitized_payload(),
            "shellnet submit failure payload"
        );
        anyhow!(e)
    })
}

async fn send_message_checked(
    http: &reqwest::Client,
    endpoint: &str,
    boc_base64: &str,
) -> Result<Value> {
    let account_id = dest_account_id_hex(boc_base64)?;
    let dapp_id = fetch_dapp_id(http, endpoint, &account_id).await?;
    send_message_routed_checked(http, endpoint, boc_base64, &account_id, &dapp_id, None).await
}

async fn send_message_routed_checked(
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
    let resp = http
        .post(format!("{}/v2/messages", endpoint.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .json(&json!([item]))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    checked_submit_response(resp)
}

impl RealChainBackend {
    /// Connect to shellnet (`ChainClient::shellnet()`) and read the manifest
    /// of deployed contracts. The endpoint is taken from the SDK -- no URL is needed in the manifest.
    pub fn connect(manifest_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let deployed = Deployed::load(manifest_path)?;
        let client = ChainClient::shellnet()?;
        let http = reqwest::Client::builder().user_agent(BROWSER_UA).build()?;
        let superroot = Address::parse(&deployed.superroot)?;
        Ok(Self {
            client,
            http,
            superroot,
            deployed,
        })
    }

    /// Low-level chain client(for the trait adapter in the next step).
    pub fn client(&self) -> &ChainClient {
        &self.client
    }

    /// The `SuperRoot` address -- the derivation point for `RootModel`/`InferenceOrderBook`.
    pub fn superroot(&self) -> &Address {
        &self.superroot
    }

    /// Manifest of the deployed contracts.
    pub fn deployed(&self) -> &Deployed {
        &self.deployed
    }

    /// Chain liveness check -- confirms a working connection to shellnet.
    pub async fn liveness(&self) -> Result<ChainLiveness> {
        self.client.chain_liveness().await
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
            self.code_hash_account_check("RootPN code hash", &rootpn, &code_hash(ROOTPN_TVC)?)
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
    async fn submit(
        &self,
        addr: &Address,
        abi_json: &str,
        method: &str,
        args: Value,
        keys: &KeyPair,
    ) -> Result<Value> {
        let ctx = local_context()?;
        let boc = encode_external_call(
            &ctx,
            abi_json,
            &addr.with_workchain(),
            method,
            args,
            keys.public_hex(),
            keys.secret_hex(),
        )
        .await?;
        self.send_with_retry(&boc).await
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
            return send_message_checked(&self.http, endpoint, boc).await;
        }
        let account_id = dest_account_id_hex(boc)?;
        let dapp_id = match fetch_dapp_id(&self.http, endpoint, &account_id).await {
            Ok(d) => d,
            Err(e) if is_uninit_account_404(&e.to_string()) => account_id.clone(),
            Err(e) => return Err(e),
        };
        send_message_routed_checked(&self.http, endpoint, boc, &account_id, &dapp_id, None).await
    }

    /// Submit a message to shellnet with retry on **transient** infrastructure failures:
    /// (1) overflow of the block manager's write queue(`QUEUE_OVERFLOW` -- "message queue is full");
    /// (2) **transient gateway 5xx** (`502 Bad Gateway` / `503` / `504` from the reverse proxy, when
    /// the backend is briefly unavailable -- observed to flicker on shellnet under load). The node is alive and moving
    /// blocks; we wait(exponential backoff, cap 8s) and retry -- this is resilience to a real network,
    /// not a test crutch. Other(logical) errors propagate immediately. `deploy` is threaded to
    /// [`submit_once`] so only deploy-message sends get the funded-uninit `/v2/account` 404 tolerance.
    async fn retry_submit(&self, boc: &str, deploy: bool) -> Result<Value> {
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
    ) -> Result<Vec<Address>> {
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
        let mut matches = Vec::<(i64, String)>::new();
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
                    matches.push((created_at, tc));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(anyhow!(
                        "decode InferenceFilledConfirmed ext-out on note {account_id}: {e}"
                    ));
                }
            }
        }
        matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut out = Vec::new();
        let mut consumed = Vec::new();
        let mut unique_new = BTreeSet::new();
        for (created_at, tc) in matches {
            if cursor.has_seen(created_at, &tc) {
                continue;
            }
            if unique_new.insert((created_at, tc.clone())) {
                out.push(
                    Address::parse(&tc)
                        .map_err(|e| anyhow!("InferenceFilledConfirmed tokenContract {tc}: {e}"))?,
                );
                consumed.push((created_at, tc));
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
        since_unix: i64,
        timeout: std::time::Duration,
    ) -> Result<Address> {
        let acct = note.with_workchain();
        let account_id = acct.strip_prefix("0:").unwrap_or(&acct).to_string();
        let want_ob = Address::parse(&order_book.with_workchain())
            .map(|a| a.with_workchain())
            .unwrap_or_else(|_| order_book.with_workchain());
        let start = std::time::Instant::now();
        let mut cursor = MatchWatchCursor::new(since_unix);
        loop {
            let mut tcs = self
                .poll_inference_filled_tcs(note, order_book, true, &mut cursor)
                .await?;
            if let Some(tc) = tcs.pop() {
                return Ok(tc);
            }
            if start.elapsed() >= timeout {
                return Err(anyhow!(
                    "timed out waiting for InferenceFilledConfirmed on note {account_id} (no buy match \
                     on book {want_ob} yet) -- the seller's offer may not be resting, or the match didn't go through"
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    /// The book's `getStats` getter(`nextOrderId`, `orderCount`, `executedNotional`, `executedTicks`).
    pub async fn inference_orderbook_stats(&self, ob: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(ob, INFERENCE_ORDERBOOK_ABI, "getStats", json!({}))
            .await
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

    /// The seller(note) posts an offer into the on-chain book -- `postSellOffer(modelHash, pricePerTick,
    /// maxTicks, tokenContract, flags, nonce)` (the note derives the book address from `modelHash`; owner =
    /// `msg.sender` = note). `flags=0` -- a plain limit: the offer rests in the order book as an ask.
    /// The deployed 4.0.6 `PrivateNote`(`c8a81f54`, live-verified -- RootPN `fc1d445d` updateCode'd to it)
    /// `postSellOffer` **takes `nonce`**: the IOB re-derives the per-deal `TokenContract` address as
    /// `_tokenContractAddr(sellerPubkey, nonce)` and rejects the offer(no ask rests) unless the supplied
    /// `tokenContract` matches. The same `nonce` is the `_nonce` static the
    /// TC is deployed with.
    #[allow(clippy::too_many_arguments)]
    pub async fn post_sell_offer(
        &self,
        note: &Address,
        owner_keys: &KeyPair,
        model_hash: &str,
        price_per_tick: u128,
        max_ticks: u128,
        token_contract: &Address,
        flags: u8,
        nonce: u64,
    ) -> Result<Value> {
        self.submit(
            note,
            PRIVATENOTE_ABI,
            "postSellOffer",
            json!({
                "modelHash": model_hash,
                "pricePerTick": price_per_tick.to_string(),
                "maxTicks": max_ticks.to_string(),
                "tokenContract": token_contract.with_workchain(),
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
        self.submit(
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

    /// Read-only `PrivateNote.getDetails()`: public balance/lock maps and metadata, no key and no signed call.
    pub async fn private_note_details(&self, note: &Address) -> Result<Option<Value>> {
        self.client
            .run_getter(note, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await
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
        self.submit(
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
        .await
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
            let already = d["hasWithdrawn"]
                .as_bool()
                .or_else(|| d["hasWithdrawn"].as_str().map(|s| s == "true" || s == "1"))
                .unwrap_or(false);
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
                .await?;
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
                    .await?;
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
