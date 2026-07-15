//! Live CLI e2e (Directive 10, gate F1) — **through the `dexdo` binary itself**, not just the trait.
//!
//! Provisions a deal **note-funded** (#58: OB + RootModel + per-deal TokenContract from the seller note's own
//! ECC[2] via `provision_market` — no operator wallet, no giver in the operate path) and runs TWO processes
//! `dexdo seller` / `dexdo buyer` against real shellnet (4.0.7). Verifies the key security-review condition F1:
//! the seller **reconstructs the buyer's pubkey from chain** (`getBuyerPubkey` → x25519) and encrypts the
//! handover with it — the buyer **decrypts** that handover. If the reconstruction is wrong, decryption fails and
//! the buyer does not receive the endpoint.
//!
//! Behind `#[ignore]` + the `shellnet` feature: requires `DEXDO_PN_POOL` (≥2 notes from `mint_pn_pool`) and the
//! network; not part of the offline suite. The pool is minted EXTERNALLY — the giver (test faucet, D13) lives only
//! in `mint_pn_pool`, never in this harness or production-`dexdo`; here the notes self-fund their own provisioning:
//!   export DEXDO_PN_POOL=$PWD/pn_pool.json           # ≥2 minted notes (seller + buyer)
//!   cargo test -p dexdo --features shellnet --test live_cli -- --ignored --nocapture
// The operate/provision path is note-funded (no giver) → the deal-flow tests need only `shellnet`. The #137
// note-deploy §8 (`live_note_deploy_via_giver_funded_wallet`) additionally uses `test-giver` to SELF-FUND a fresh
// wallet from the faucet (the executor provisions its own wallet — nothing is financed externally); the giver is
// still test-only (behind `test-giver`), never in production-`dexdo`.
#![cfg(feature = "shellnet")]

use dexdo_core::{
    required_escrow_for_buy, Address, ChainBackend, KeyPair, Note, OrderBookOrder,
    RealBuyerBackend, RealChainBackend, RealSellerBackend, SellOffer, Settlement,
    MATCH_OPEN_TIMEOUT_SECS, MODEL_TICK_SIZE,
};
use std::io::{BufRead, BufReader, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicU64},
    mpsc, Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Manifest of deployed contracts (the same one the binary reads).
const MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contracts/deployed.shellnet.json"
);
/// Workspace root — the working directory for the CLI (where `models.json` lives).
const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
/// Frame model name: the seller's config key (`--model qwen`) resolves to this `frame_model`, and the buyer
/// passes the same one via `--frame-model` — both sides converge on a single `model_hash` (sha256).
const FRAME_MODEL: &str = "qwen--qwen3--32b";
const DEFAULT_LATE_BUYER_DELAY_SECS: u64 = 35;
const ISSUE208_DEPLOY_GAS: u128 = 10_000_000_000;
const ISSUE335_PRICE: u128 = 10_000;
const ISSUE335_MAX_TICKS: u128 = 3;
const CURRENT_PRIVATE_NOTE_CODE_HASH: &str =
    "1d2fcae0a1a7bc8af4e39992fbf0eda7bc2e7ff3397e44500ff03b57247d732f";

async fn is_active_once(be: &RealChainBackend, addr: &Address) -> bool {
    be.client()
        .get_account(addr)
        .await
        .ok()
        .flatten()
        .map(|a| a.is_active())
        .unwrap_or(false)
}

async fn wait_active(be: &RealChainBackend, addr: &Address) -> bool {
    for _ in 0..40 {
        if is_active_once(be, addr).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    false
}

async fn ecc_shell_balance(be: &RealChainBackend, addr: &Address) -> u128 {
    be.client()
        .get_account(addr)
        .await
        .expect("get account")
        .expect("account exists")
        .ecc_balance(2)
}

fn masked_addr(addr: &str) -> String {
    let trimmed = addr.trim();
    if trimmed.len() <= 18 {
        return trimmed.to_string();
    }
    format!("{}...{}", &trimmed[..10], &trimmed[trimmed.len() - 6..])
}

fn json_bool_field(v: &serde_json::Value, key: &str) -> Option<bool> {
    if let Some(b) = v[key].as_bool() {
        return Some(b);
    }
    match v[key].as_str()?.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn json_u128_value(v: &serde_json::Value) -> Option<u128> {
    if let Some(n) = v.as_u64() {
        return Some(n as u128);
    }
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    None
}

fn json_map_u128(map: &serde_json::Value, key: &str) -> Option<u128> {
    if let Some(obj) = map.as_object() {
        return obj.get(key).and_then(json_u128_value);
    }
    let arr = map.as_array()?;
    for entry in arr {
        if let Some(pair) = entry.as_array() {
            if pair.len() >= 2
                && pair[0]
                    .as_str()
                    .map_or_else(|| pair[0].to_string(), ToString::to_string)
                    .trim_matches('"')
                    == key
            {
                return json_u128_value(&pair[1]);
            }
        }
        if let Some(obj) = entry.as_object() {
            let entry_key = obj
                .get("key")
                .or_else(|| obj.get("id"))
                .and_then(|v| {
                    v.as_str()
                        .map(ToString::to_string)
                        .or_else(|| Some(v.to_string()))
                })
                .unwrap_or_default();
            if entry_key.trim_matches('"') == key {
                if let Some(v) = obj.get("value").or_else(|| obj.get("amount")) {
                    return json_u128_value(v);
                }
            }
        }
    }
    None
}

fn pool_raw_value_per_pn(pool: &serde_json::Value) -> u128 {
    pool["raw_value_per_pn"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| pool["raw_value_per_pn"].as_u64().map(u128::from))
        .unwrap_or(1)
}

fn pool_note_identity(pool: &serde_json::Value, index: usize) -> (String, String) {
    let note = &pool["notes"].as_array().expect("notes[]")[index];
    (
        note["address"].as_str().expect("note address").to_string(),
        note["owner_secret_key_hex"]
            .as_str()
            .expect("note owner secret")
            .to_string(),
    )
}

#[derive(Debug, Clone)]
struct Issue335NoteFacts {
    address: String,
    code_hash: String,
    native_vmshell: u128,
    shell_ecc2: u128,
    nackl_balance: u128,
    has_withdrawn: bool,
}

async fn issue335_note_facts(
    be: &RealChainBackend,
    note: &Address,
    stage: &str,
) -> Issue335NoteFacts {
    let account = be
        .client()
        .get_account(note)
        .await
        .expect("get PrivateNote account")
        .expect("PrivateNote account exists");
    assert!(
        account.is_active(),
        "#335 {stage}: note {note} is {}, not Active",
        account.status
    );
    let details = be
        .private_note_details(note)
        .await
        .expect("get PrivateNote.getDetails")
        .expect("PrivateNote.getDetails exists");
    let has_withdrawn =
        json_bool_field(&details, "hasWithdrawn").expect("getDetails.hasWithdrawn bool");
    let nackl_balance = json_map_u128(&details["balance"], "1").unwrap_or(0);
    let code_hash = account.code_hash.clone().unwrap_or_default();
    let facts = Issue335NoteFacts {
        address: note.with_workchain(),
        code_hash,
        native_vmshell: account.balance,
        shell_ecc2: account.ecc_balance(2),
        nackl_balance,
        has_withdrawn,
    };
    eprintln!(
        "#335 note_facts stage={stage} note={} code_hash={} hasWithdrawn={} native_vmshell={} shell_ecc2={} nackl_balance={}",
        masked_addr(&facts.address),
        facts.code_hash,
        facts.has_withdrawn,
        facts.native_vmshell,
        facts.shell_ecc2,
        facts.nackl_balance
    );
    facts
}

async fn issue335_validate_pool_note(
    be: &RealChainBackend,
    pool: &serde_json::Value,
    index: usize,
    stage: &str,
) -> (String, String, Issue335NoteFacts) {
    let (addr, secret) = pool_note_identity(pool, index);
    let note = Address::parse(&addr).expect("pool note address parses");
    let keys = KeyPair::from_secret_hex(&secret).expect("pool note owner key parses");
    be.assert_seller_note_current(&note)
        .await
        .expect("pool note code_hash/current PrivateNote pin");
    be.assert_note_owner_matches("#335 pool validation", &note, &keys)
        .await
        .expect("pool note owner key matches getDetails.ephemeralPubkey");
    let facts = issue335_note_facts(be, &note, stage).await;
    assert_eq!(
        facts.code_hash, CURRENT_PRIVATE_NOTE_CODE_HASH,
        "#335 {stage}: on-chain PrivateNote code_hash must match 4.0.18 pin"
    );
    assert!(
        !facts.has_withdrawn,
        "#335 {stage}: note {} is already withdrawn",
        masked_addr(&facts.address)
    );
    let expected_nackl = pool_raw_value_per_pn(pool);
    assert!(
        facts.nackl_balance >= expected_nackl,
        "#335 {stage}: note {} NACKL balance {} < pool raw_value_per_pn {}",
        masked_addr(&facts.address),
        facts.nackl_balance,
        expected_nackl
    );
    (addr, secret, facts)
}

fn issue335_policy_json() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "buyer": {
            "on": {
                "no_handover_after_match": "wait_then_reclaim",
                "malformed_handover": "reclaim",
                "dead_gateway": "retry_then_reclaim",
                "empty_stream": "reclaim",
                "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                "bad_output_scam": "dispute"
            },
            "failover": {
                "max_sellers_to_try": 1,
                "total_spend_cap_shells": 1
            }
        },
        "seller": {
            "on": {
                "after_deal_done": "retire",
                "buyer_no_show": "retire_gateway",
                "dispute_against_me": "release_if_clean"
            },
            "max_open_deals": 1
        }
    })
}

fn write_json_private(dir: &Path, name: &str, value: &serde_json::Value) -> PathBuf {
    let p = dir.join(name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&p).expect("create private json file");
    let text = serde_json::to_vec_pretty(value).expect("serialize json");
    f.write_all(&text).expect("write json");
    p
}

struct TempDirCleanup(PathBuf);

impl Drop for TempDirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create_private_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).expect("create private temp dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod private temp dir");
    }
    dir
}

/// Write the key's hex secret to a temporary file (custody is external §5; local, not committed).
fn write_key(dir: &Path, name: &str, hex: &str) -> PathBuf {
    let p = dir.join(name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&p).expect("create key file");
    f.write_all(hex.trim().as_bytes()).expect("write key");
    p
}

fn successful_stdout(out: std::process::Output, label: &str) -> String {
    assert!(
        out.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout utf8")
}

fn failed_output(out: std::process::Output, label: &str) -> (String, String) {
    assert!(
        !out.status.success(),
        "{label} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

#[cfg(unix)]
#[cfg_attr(not(feature = "test-giver"), allow(dead_code))]
fn assert_private_file_mode(path: &Path, label: &str) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).expect(label).permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{label} must be 0600");
}

#[cfg(not(unix))]
#[cfg_attr(not(feature = "test-giver"), allow(dead_code))]
fn assert_private_file_mode(_path: &Path, _label: &str) {}

fn first_order_id(stdout: &str) -> u128 {
    stdout
        .lines()
        .flat_map(|line| line.split_whitespace())
        .find_map(|part| part.strip_prefix("order_id=")?.parse().ok())
        .expect("stdout contains order_id=")
}

fn tail(s: &str, n: usize) -> String {
    let b = s.as_bytes();
    String::from_utf8_lossy(&b[b.len().saturating_sub(n)..]).into_owned()
}

fn stdout_lines(child: &mut std::process::Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().expect("child stdout piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn free_loopback_addr() -> String {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind free loopback port")
        .local_addr()
        .expect("local addr")
        .to_string()
}

fn gateway_accepting(addr: &str) -> bool {
    let Ok(addr) = addr.parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

struct MarkRecoveredOnDrop {
    session: Arc<dexdo::buyer::api::SessionSettle>,
    reason: &'static str,
}

impl Drop for MarkRecoveredOnDrop {
    fn drop(&mut self) {
        self.session.mark_recovered(self.reason);
    }
}

/// #205 live/manual check: address-only balance read for a known pool note. Read-only; no note key is passed.
#[tokio::test]
#[ignore = "live read-only: requires DEXDO_PN_POOL with a current shellnet PrivateNote"]
async fn live_note_balance_reads_pool_note_address_only() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set - skipping (minted notes pn_pool.json are required)");
        return;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read DEXDO_PN_POOL"))
            .expect("DEXDO_PN_POOL json");
    let note_addr = pool["notes"][0]["address"]
        .as_str()
        .expect("pool note address");
    let out = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "note",
            "balance",
            "--note-addr",
            note_addr,
            "--contracts",
            MANIFEST,
        ])
        .output()
        .expect("run dexdo note balance");
    let stdout = successful_stdout(out, "dexdo note balance");
    assert!(stdout.contains("SHELL ECC[2]"), "{stdout}");
    assert!(stdout.contains("VMSHELL native gas"), "{stdout}");
    assert!(!stdout.contains("owner_secret_key_hex"), "{stdout}");
}

fn assert_child_still_running(child: &mut std::process::Child, log: &Path, label: &str) {
    match child.try_wait() {
        Ok(Some(status)) => {
            let log_txt = std::fs::read_to_string(log).unwrap_or_default();
            panic!(
                "{label} exited before buyer started: success={}\n{}",
                status.success(),
                tail(&log_txt, 3000)
            );
        }
        Ok(None) => {}
        Err(e) => panic!("{label} status check failed: {e}"),
    }
}

fn configured_late_buyer_delay_secs() -> u64 {
    match std::env::var("DEXDO_LIVE_LATE_BUYER_DELAY_SECS") {
        Ok(raw) => raw
            .parse::<u64>()
            .expect("DEXDO_LIVE_LATE_BUYER_DELAY_SECS must be a u64"),
        Err(_) => DEFAULT_LATE_BUYER_DELAY_SECS,
    }
}

fn live_pool_or_skip(test_name: &str, min_notes: usize) -> Option<serde_json::Value> {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("{test_name}: DEXDO_PN_POOL not set — skipping live shellnet test");
        return None;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read DEXDO_PN_POOL"))
            .expect("parse DEXDO_PN_POOL");
    let notes = pool["notes"].as_array().expect("notes[]");
    if notes.len() < min_notes {
        eprintln!(
            "{test_name}: DEXDO_PN_POOL has {} note(s), need {min_notes} — skipping",
            notes.len()
        );
        return None;
    }
    Some(pool)
}

async fn live_role_notes_by_ecc(
    be: &RealChainBackend,
    pool: &serde_json::Value,
    min_notes: usize,
) -> Vec<(String, String, u128)> {
    let notes = pool["notes"].as_array().expect("notes[]");
    assert!(
        notes.len() >= min_notes,
        "live pool must have at least {min_notes} note(s)"
    );
    let mut role_notes = Vec::new();
    for note in notes {
        let addr = note["address"].as_str().expect("note addr").to_string();
        let secret = note["owner_secret_key_hex"]
            .as_str()
            .expect("note secret")
            .to_string();
        let address = Address::parse(&addr).expect("note addr parses");
        let ecc2 = ecc_shell_balance(be, &address).await;
        role_notes.push((addr, secret, ecc2));
    }
    role_notes.sort_by(|a, b| b.2.cmp(&a.2));
    role_notes
}

async fn live_role_notes_in_pool_order(
    be: &RealChainBackend,
    pool: &serde_json::Value,
    min_notes: usize,
) -> Vec<(String, String, u128)> {
    let notes = pool["notes"].as_array().expect("notes[]");
    assert!(
        notes.len() >= min_notes,
        "live pool must have at least {min_notes} note(s)"
    );
    let mut role_notes = Vec::new();
    for note in notes.iter().take(min_notes) {
        let addr = note["address"].as_str().expect("note addr").to_string();
        let secret = note["owner_secret_key_hex"]
            .as_str()
            .expect("note secret")
            .to_string();
        let address = Address::parse(&addr).expect("note addr parses");
        let ecc2 = ecc_shell_balance(be, &address).await;
        role_notes.push((addr, secret, ecc2));
    }
    role_notes
}

async fn wait_resting_ask(
    be: &RealChainBackend,
    ob: &Address,
    frame_model: &str,
    model_hash: &str,
    token_contract: &str,
) -> Option<OrderBookOrder> {
    for _ in 0..40 {
        let snapshot = be
            .inference_orderbook_snapshot(ob, frame_model, model_hash)
            .await
            .expect("read order book snapshot");
        if let Some(order) = snapshot.resting_asks().find(|o| {
            o.token_contract
                .as_deref()
                .is_some_and(|tc| tc.eq_ignore_ascii_case(token_contract))
        }) {
            return Some(order.clone());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    None
}

async fn fresh_tc_resting_ask_once(
    be: &RealChainBackend,
    ob: &Address,
    frame_model: &str,
    model_hash: &str,
    token_contract: &str,
) -> std::result::Result<bool, String> {
    let snapshot = tokio::time::timeout(
        Duration::from_secs(90),
        be.inference_orderbook_snapshot(ob, frame_model, model_hash),
    )
    .await
    .map_err(|_| "order book snapshot timed out after 90s".to_string())?
    .map_err(|e| format!("order book snapshot failed: {e}"))?;
    let found = snapshot.resting_asks().any(|o| {
        o.token_contract
            .as_deref()
            .is_some_and(|tc| tc.eq_ignore_ascii_case(token_contract))
    });
    Ok(found)
}

fn json_u64_field(v: &serde_json::Value, key: &str) -> Option<u64> {
    if let Some(n) = v[key].as_u64() {
        return Some(n);
    }
    v[key].as_str()?.parse().ok()
}

async fn wait_funded_token_contract_state(
    be: &RealChainBackend,
    tc: &Address,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    let mut last = "no state read attempted".to_string();
    while Instant::now() < deadline {
        match be.token_contract_state(tc).await {
            Ok(Some(st)) => {
                let funded = st["funded"].as_bool().unwrap_or(false);
                let opened = st["opened"].as_bool().unwrap_or(false);
                let funded_time = json_u64_field(&st, "fundedTime");
                last = format!("funded={funded} opened={opened} fundedTime={funded_time:?}");
                if funded {
                    return st;
                }
            }
            Ok(None) => last = "TokenContract inactive or unreadable".to_string(),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    panic!("TC {tc} did not reach funded=true within {timeout:?}; last={last}");
}

/// #335 read-only live validation for a candidate pool before using it in the seller/buyer money path.
///
/// Run:
///   export DEXDO_PN_POOL=/media/futurizt/BIG/dexdo-operator-226-run/pn_pool.operator-418.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_335_validate_pool_notes_by_fact -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live shellnet #335; validates pool notes by fact without consuming them"]
async fn live_335_validate_pool_notes_by_fact() {
    let Some(pool) = live_pool_or_skip("live_335_validate_pool_notes_by_fact", 2) else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let (_, _, seller) = issue335_validate_pool_note(&be, &pool, 0, "pool-validation-seller").await;
    let (_, _, buyer) = issue335_validate_pool_note(&be, &pool, 1, "pool-validation-buyer").await;
    let seller_min_shell = ISSUE208_DEPLOY_GAS * 2;
    assert!(
        seller.shell_ecc2 >= seller_min_shell,
        "#335 pool seller note needs >= {seller_min_shell} SHELL raw units for RootModel+TC provision; got {}",
        seller.shell_ecc2
    );
    let buyer_min_shell = required_escrow_for_buy(ISSUE335_MAX_TICKS, ISSUE335_PRICE);
    assert!(
        buyer.shell_ecc2 >= buyer_min_shell,
        "#335 pool buyer note needs >= {buyer_min_shell} SHELL raw units for buy escrow; got {}",
        buyer.shell_ecc2
    );
}

/// #335 happy live lifecycle proof:
/// fresh note hasWithdrawn=false immediately, still false after provision and before seller post,
/// the real ask rests in the IOB, buyer matches, seller opens, accepts probe, advances twice, and
/// buyer STOP settles as AmicableSplit.
///
/// Run:
///   export DEXDO_PN_POOL=/media/futurizt/BIG/dexdo-operator-226-run/pn_pool.operator-418.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_335_happy_note_lifecycle_rest_match_and_settle -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live shellnet #335; consumes two validated notes and waits contract settle windows"]
async fn live_335_happy_note_lifecycle_rest_match_and_settle() {
    let Some(pool) = live_pool_or_skip("live_335_happy_note_lifecycle_rest_match_and_settle", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let (s_addr, s_sec, seller_immediate) =
        issue335_validate_pool_note(&be, &pool, 0, "immediate").await;
    let (b_addr, b_sec, buyer_immediate) =
        issue335_validate_pool_note(&be, &pool, 1, "buyer-immediate").await;
    let seller_min_shell = ISSUE208_DEPLOY_GAS * 2;
    assert!(
        seller_immediate.shell_ecc2 >= seller_min_shell,
        "seller note needs >= {seller_min_shell} SHELL raw units, got {}",
        seller_immediate.shell_ecc2
    );
    let buyer_min_shell = required_escrow_for_buy(ISSUE335_MAX_TICKS, ISSUE335_PRICE);
    assert!(
        buyer_immediate.shell_ecc2 >= buyer_min_shell,
        "buyer note needs >= {buyer_min_shell} SHELL raw units, got {}",
        buyer_immediate.shell_ecc2
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let seller_note_addr = Address::parse(&s_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let frame_model = format!("{FRAME_MODEL}-issue335-{nonce}");
    let market = be
        .provision_market(
            &seller_kp,
            &seller_note_addr,
            &frame_model,
            nonce,
            ISSUE335_PRICE,
            ISSUE335_MAX_TICKS,
            ISSUE208_DEPLOY_GAS,
        )
        .await
        .expect("provision market from seller note");
    let ob = Address::parse(&market.inference_order_book).expect("OB addr");
    let tc = Address::parse(&market.token_contract).expect("TC addr");
    let tc_arg = tc.with_workchain();
    assert!(wait_active(&be, &tc).await, "TC active after provision");

    let after_provision = issue335_note_facts(&be, &seller_note_addr, "after-provision").await;
    assert!(
        !after_provision.has_withdrawn,
        "fresh seller note became withdrawn during provision"
    );

    let (seller_backend, seller_note) = RealSellerBackend::from_provisioned(
        MANIFEST,
        &s_addr,
        &s_sec,
        &frame_model,
        nonce,
        1_000_000,
    )
    .expect("seller backend");
    let escrow = required_escrow_for_buy(ISSUE335_MAX_TICKS, ISSUE335_PRICE);
    let (buyer_backend, buyer_note) = RealBuyerBackend::from_provisioned(
        MANIFEST,
        &b_addr,
        &b_sec,
        &frame_model,
        ISSUE335_PRICE,
        ISSUE335_MAX_TICKS,
        escrow,
    )
    .expect("buyer backend");

    let before_seller = issue335_note_facts(&be, &seller_note_addr, "before-seller").await;
    assert!(
        !before_seller.has_withdrawn,
        "fresh seller note is withdrawn before seller post"
    );
    seller_backend
        .post_offer(
            SellOffer {
                price_per_tick: ISSUE335_PRICE as u64,
                max_ticks: ISSUE335_MAX_TICKS as u64,
                token_contract: tc_arg.clone(),
            },
            &seller_note,
        )
        .await
        .expect("post sell offer with hasWithdrawn=false note");
    seller_backend
        .confirm_offer_outcome(&tc_arg)
        .await
        .expect("offer rested by exact TC");
    let order = wait_resting_ask(&be, &ob, &frame_model, &market.model_hash, &tc_arg)
        .await
        .expect("resting ask visible in IOB by exact TC");
    eprintln!(
        "#335 resting_ask order_id={} tc={} frame_model={} price={} ticks={}",
        order.order_id, tc_arg, frame_model, ISSUE335_PRICE, ISSUE335_MAX_TICKS
    );

    let since_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    buyer_backend
        .place_buy(&tc_arg, &buyer_note)
        .await
        .expect("buyer matches exact resting ask");
    let matched_from_buyer = buyer_backend
        .wait_matched_token_contract(since_unix, Duration::from_secs(240))
        .await
        .expect("buyer fill mirror records matched TC")
        .expect("buyer fill mirror returns a match")
        .token_contract;
    assert_eq!(
        matched_from_buyer.to_ascii_lowercase(),
        tc_arg.to_ascii_lowercase()
    );

    let mut matched = None;
    for _ in 0..60 {
        if let Some(m) = seller_backend
            .read_openable_match_now(&tc_arg)
            .await
            .expect("read openable match")
        {
            matched = Some(m);
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let matched = matched.expect("seller observed funded/openable match");
    let funded_state = be
        .token_contract_state(&tc)
        .await
        .expect("getState after match")
        .expect("TC state after match");
    eprintln!("#335 matched tc={tc_arg} getState={funded_state:?}");
    assert_eq!(funded_state["funded"].as_bool(), Some(true));

    let endpoint = b"https://seller.example/335|happy-lifecycle";
    let encrypted = seller_note.encrypt_to(&matched.buyer_pubkey, endpoint);
    seller_backend
        .open_stream(&tc_arg, encrypted, &seller_note)
        .await
        .expect("seller opens stream/probe");
    let opened_state = be
        .token_contract_state(&tc)
        .await
        .expect("getState after open")
        .expect("TC state after open");
    eprintln!("#335 opened tc={tc_arg} getState={opened_state:?}");
    assert_eq!(opened_state["opened"].as_bool(), Some(true));

    let settle = seller_backend
        .deal_settle_window(&tc_arg)
        .await
        .expect("read real deal settle window");
    let settle_wait = settle + Duration::from_secs(5);
    eprintln!(
        "#335 waiting {:?} before accept_probe on real settle cadence",
        settle_wait
    );
    tokio::time::sleep(settle_wait).await;
    seller_backend
        .accept_probe(&tc_arg)
        .await
        .expect("seller accepts probe");
    let accepted_state = be
        .token_contract_state(&tc)
        .await
        .expect("getState after accept_probe")
        .expect("TC state after accept_probe");
    eprintln!("#335 accept_probe tc={tc_arg} getState={accepted_state:?}");
    assert_eq!(accepted_state["probeAccepted"].as_bool(), Some(true));

    for advance_index in 1..=2 {
        eprintln!(
            "#335 waiting {:?} before advance_tick #{}",
            settle_wait, advance_index
        );
        tokio::time::sleep(settle_wait).await;
        seller_backend
            .advance_tick(&tc_arg, &seller_note)
            .await
            .unwrap_or_else(|e| panic!("seller advance_tick #{advance_index}: {e}"));
        let state = be
            .token_contract_state(&tc)
            .await
            .expect("getState after advance_tick")
            .expect("TC state after advance_tick");
        eprintln!("#335 advance_tick#{advance_index} tc={tc_arg} getState={state:?}");
    }

    let settlement = buyer_backend
        .stop(&tc_arg, &buyer_note)
        .await
        .expect("buyer stop settles opened stream");
    assert!(
        matches!(settlement, Settlement::AmicableSplit { .. }),
        "expected AmicableSplit, got {settlement:?}"
    );
    let post_stop = be
        .token_contract_state(&tc)
        .await
        .expect("getState after stop")
        .expect("TC state after stop");
    eprintln!("#335 settled tc={tc_arg} settlement={settlement:?} getState={post_stop:?}");
    assert_eq!(post_stop["opened"].as_bool(), Some(false));
}

/// #335 destructive negative proof. This test intentionally withdraws a sacrificial note from
/// DEXDO_335_NEGATIVE_PN_POOL, then invokes the real CLI seller with that note and asserts the new
/// precheck fails before sell-offer terms/postSellOffer.
///
/// Run:
///   export DEXDO_335_NEGATIVE_PN_POOL=/tmp/dexdo-335-negative-pn_pool.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_335_withdrawn_note_precheck_blocks_cli_seller -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live shellnet #335; destructively withdraws one sacrificial note"]
async fn live_335_withdrawn_note_precheck_blocks_cli_seller() {
    let Ok(pool_path) = std::env::var("DEXDO_335_NEGATIVE_PN_POOL") else {
        eprintln!(
            "live_335_withdrawn_note_precheck_blocks_cli_seller: DEXDO_335_NEGATIVE_PN_POOL not set - skipping destructive negative proof"
        );
        return;
    };
    if let Ok(primary) = std::env::var("DEXDO_PN_POOL") {
        if std::fs::canonicalize(&primary).ok() == std::fs::canonicalize(&pool_path).ok() {
            panic!(
                "DEXDO_335_NEGATIVE_PN_POOL must be a sacrificial pool distinct from DEXDO_PN_POOL"
            );
        }
    }
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read negative pool"))
            .expect("parse negative pool");
    assert!(
        pool["notes"].as_array().map_or(0, Vec::len) >= 1,
        "negative pool needs at least one sacrificial note"
    );
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let (addr, secret) = pool_note_identity(&pool, 0);
    let note = Address::parse(&addr).expect("negative note address");
    let keys = KeyPair::from_secret_hex(&secret).expect("negative note key");
    be.assert_seller_note_current(&note)
        .await
        .expect("negative note code_hash/current PrivateNote pin");
    be.assert_note_owner_matches("#335 negative owner validation", &note, &keys)
        .await
        .expect("negative note owner key matches getDetails.ephemeralPubkey");
    let before = issue335_note_facts(&be, &note, "negative-before-withdraw").await;
    let withdrawn = if before.has_withdrawn {
        before
    } else {
        assert!(
            before.nackl_balance > 0 || before.shell_ecc2 > 0,
            "sacrificial note has no token/SHELL balance to withdraw"
        );
        let dest =
            Address::parse("0:3335333533353335333533353335333533353335333533353335333533353335")
                .expect("dummy withdraw destination");
        let submit = be
            .withdraw_note_tokens(&note, &keys, &dest)
            .await
            .expect("submit withdrawTokens for sacrificial note");
        eprintln!(
            "#335 negative withdraw submitted note={} result={submit}",
            masked_addr(&addr)
        );
        let mut withdrawn = None;
        for _ in 0..60 {
            let facts = issue335_note_facts(&be, &note, "negative-after-withdraw-poll").await;
            if facts.has_withdrawn {
                withdrawn = Some(facts);
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        withdrawn.expect("sacrificial note reached hasWithdrawn=true by fact")
    };
    eprintln!(
        "#335 negative note withdrawn note={} hasWithdrawn={} shell_ecc2={} nackl_balance={}",
        masked_addr(&withdrawn.address),
        withdrawn.has_withdrawn,
        withdrawn.shell_ecc2,
        withdrawn.nackl_balance
    );

    let dir = create_private_temp_dir("dexdo_335_negative");
    let _cleanup = TempDirCleanup(dir.clone());
    let key = write_key(&dir, "seller.key", &secret);
    let policy = write_json_private(&dir, "policy.json", &issue335_policy_json());
    let endpoints = write_json_private(&dir, "endpoints.json", &serde_json::json!({}));
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let dummy_tc = "0:3353353353353353353353353353353353353353353353353353353353353335";
    let mut seller = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "seller",
            "--mock-model",
            "--note-key",
            key.to_str().unwrap(),
            "--note-addr",
            &addr,
            "--model",
            "qwen",
            "--models",
            "models.json",
            "--token-contract",
            dummy_tc,
            "--nonce",
            "335",
            "--contracts",
            MANIFEST,
            "--policy",
            policy.to_str().unwrap(),
            "--endpoints-file",
            endpoints.to_str().unwrap(),
            "--deals-dir",
            deals_dir.to_str().unwrap(),
            "--gateway-listen",
            "127.0.0.1:0",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn dexdo seller negative precheck");
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        match seller.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
            Err(e) => panic!("dexdo seller negative precheck status check failed: {e}"),
        }
    }
    if seller
        .try_wait()
        .expect("dexdo seller negative precheck final status")
        .is_none()
    {
        terminate_child(&mut seller);
        let out = seller
            .wait_with_output()
            .expect("collect timed-out dexdo seller output");
        panic!(
            "dexdo seller did not fail the withdrawn-note precheck within 90s\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = seller
        .wait_with_output()
        .expect("collect dexdo seller negative precheck output");
    let (stdout, stderr) = failed_output(out, "dexdo seller withdrawn-note negative precheck");
    let combined = format!("{stdout}\n{stderr}");
    eprintln!("#335 negative CLI stderr_tail=\n{}", tail(&combined, 3000));
    assert!(combined.contains("seller post_offer aborted"), "{combined}");
    assert!(combined.contains("this note has withdrawn"), "{combined}");
    assert!(
        combined.contains("postSellOffer would revert ERR_INVALID_STATE 151"),
        "{combined}"
    );
    assert!(
        !combined.contains("TVM_ERROR"),
        "negative precheck must not leak raw TVM_ERROR: {combined}"
    );
    assert!(
        !combined.contains("sell-offer terms"),
        "seller must fail before token-contract terms/postSellOffer: {combined}"
    );
}

/// #208 live by-fact probe: no seller process is running when the buyer crosses the ask. The matched TC must
/// become funded=true/opened=false, and streamCleanup must recover it after MATCH_OPEN_TIMEOUT.
///
/// Run:
///   export DEXDO_PN_POOL=$PWD/pn_pool.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_208_inactive_seller_model_buy_funds_then_cleanup -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live shellnet #208; consumes notes and waits MATCH_OPEN_TIMEOUT before cleanup"]
async fn live_208_inactive_seller_model_buy_funds_then_cleanup() {
    let Some(pool) = live_pool_or_skip("live_208_inactive_seller_model_buy_funds_then_cleanup", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= ISSUE208_DEPLOY_GAS * 2,
        "seller note needs deploy headroom, got {s_ecc2}"
    );
    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}-issue208-inactive-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 2;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &unique_frame,
            nonce,
            price,
            max_ticks,
            ISSUE208_DEPLOY_GAS,
        )
        .await
        .expect("provision market");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    assert!(wait_active(&be, &tc).await, "TC active");

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post one sell offer");
    let order = wait_resting_ask(&be, &ob, &unique_frame, &manifest.model_hash, &tc_arg)
        .await
        .expect("one seller ask rests");

    let escrow = required_escrow_for_buy(max_ticks, price);
    let (buyer_backend, buyer_note) = RealBuyerBackend::from_provisioned(
        MANIFEST,
        &b_addr,
        &b_sec,
        &unique_frame,
        price,
        max_ticks,
        escrow,
    )
    .expect("buyer backend");
    let since_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    buyer_backend
        .place_buy_by_model(&buyer_note, max_ticks, price, escrow)
        .await
        .expect("place model-only buy");
    let matched = buyer_backend
        .wait_matched_token_contract(since_unix, Duration::from_secs(240))
        .await
        .expect("matched fill event")
        .expect("matched fill")
        .token_contract;
    assert_eq!(matched.to_ascii_lowercase(), tc_arg.to_ascii_lowercase());

    let state = wait_funded_token_contract_state(&be, &tc, Duration::from_secs(240)).await;
    let buyer_note_onchain = be
        .token_contract_buyer_note(&tc)
        .await
        .expect("buyer note")
        .map(|a| a.with_workchain())
        .unwrap_or_else(|| "<none>".to_string());
    let funded = state["funded"].as_bool().unwrap_or(false);
    let opened = state["opened"].as_bool().unwrap_or(false);
    let funded_time = json_u64_field(&state, "fundedTime");
    eprintln!(
        "issue208 inactive-seller matched: order_id={} tc={} funded={} opened={} fundedTime={:?} buyer_note={} post_recovery_order_visible=pending",
        order.order_id,
        matched,
        funded,
        opened,
        funded_time,
        buyer_note_onchain
    );
    assert!(funded, "matched TC must be funded");
    assert!(
        !opened,
        "inactive seller should leave funded-never-opened state"
    );

    let cleanup_at = funded_time
        .expect("fundedTime must be exposed for cleanup")
        .saturating_add(MATCH_OPEN_TIMEOUT_SECS);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now < cleanup_at {
        tokio::time::sleep(Duration::from_secs(cleanup_at - now + 5)).await;
    }
    let settlement = buyer_backend
        .cleanup_unopened(&matched)
        .await
        .expect("streamCleanup funded-never-opened");
    let visible_after = wait_resting_ask(&be, &ob, &unique_frame, &manifest.model_hash, &tc_arg)
        .await
        .is_some();
    eprintln!(
        "issue208 inactive-seller cleanup: order_id={} tc={} settlement={settlement:?} post_recovery_order_visible={visible_after}",
        order.order_id, matched
    );
    assert!(
        !visible_after,
        "filled ask must not reappear after funded-never-opened cleanup"
    );
}

/// #264 live replay harness: a stale local handover must not let the OpenAI-compatible buyer API render a
/// user-visible response while the live TC is still funded-never-opened. The historical test name is kept to
/// match the PR271 sidecar command and reviewer grep.
///
/// Run:
///   export DEXDO_PN_POOL=$PWD/pn_pool.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_264_err_not_open_after_probe_stop_is_graceful -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live shellnet #264; consumes notes, rejects stale local API before upstream, then waits MATCH_OPEN_TIMEOUT for cleanup"]
async fn live_264_err_not_open_after_probe_stop_is_graceful() {
    let Some(pool) = live_pool_or_skip("live_264_err_not_open_after_probe_stop_is_graceful", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= ISSUE208_DEPLOY_GAS * 2,
        "seller note needs deploy headroom, got {s_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}-issue264-failclosed-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 2;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &unique_frame,
            nonce,
            price,
            max_ticks,
            ISSUE208_DEPLOY_GAS,
        )
        .await
        .expect("provision market");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    assert!(wait_active(&be, &tc).await, "TC active");

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post one sell offer");
    let order = wait_resting_ask(&be, &ob, &unique_frame, &manifest.model_hash, &tc_arg)
        .await
        .expect("one seller ask rests");

    let escrow = required_escrow_for_buy(max_ticks, price);
    let (buyer_backend, buyer_note) = RealBuyerBackend::from_provisioned(
        MANIFEST,
        &b_addr,
        &b_sec,
        &unique_frame,
        price,
        max_ticks,
        escrow,
    )
    .expect("buyer backend");
    let since_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    buyer_backend
        .place_buy_by_model(&buyer_note, max_ticks, price, escrow)
        .await
        .expect("place model-only buy");
    let matched = buyer_backend
        .wait_matched_token_contract(since_unix, Duration::from_secs(240))
        .await
        .expect("matched fill event")
        .expect("matched fill")
        .token_contract;
    assert_eq!(matched.to_ascii_lowercase(), tc_arg.to_ascii_lowercase());

    let state = wait_funded_token_contract_state(&be, &tc, Duration::from_secs(240)).await;
    assert_eq!(state["funded"].as_bool(), Some(true), "{state:?}");
    assert_eq!(state["opened"].as_bool(), Some(false), "{state:?}");
    assert_eq!(state["probeAccepted"].as_bool(), Some(false), "{state:?}");
    let funded_time = json_u64_field(&state, "fundedTime").expect("fundedTime exposed");
    let cleanup_at = funded_time.saturating_add(MATCH_OPEN_TIMEOUT_SECS);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        now < cleanup_at,
        "freshly funded TC should not be cleanup-ready yet: now={now} cleanup_at={cleanup_at}"
    );

    let buyer_note: Arc<dyn Note> = Arc::new(buyer_note);
    let chain: Arc<dyn ChainBackend> = Arc::new(buyer_backend);
    let buyer = Arc::new(dexdo::buyer::Buyer::from_note(buyer_note.clone()));
    let session = Arc::new(dexdo::buyer::api::SessionSettle::new_with_failure_policy(
        chain.clone(),
        matched.clone(),
        buyer_note,
        dexdo::buyer::api::BuyerApiFailurePolicy::default(),
    ));
    let route = dexdo::buyer::api::Route {
        handover: dexdo_core::Handover {
            endpoint: "https://127.0.0.1:1".to_string(),
            tls_fingerprint: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
        },
        token_contract: matched.clone(),
        max_tokens: MODEL_TICK_SIZE as u64,
    };
    let state = dexdo::buyer::api::ApiState::single(
        buyer,
        route,
        unique_frame.clone(),
        session.clone(),
        Arc::new(dexdo::buyer::api::ContentGate::skip()),
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (api_addr, api_task) =
        dexdo::buyer::api::serve("127.0.0.1:0".parse().unwrap(), state, false, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("start buyer API");
    let _session_guard = MarkRecoveredOnDrop {
        session: session.clone(),
        reason: "issue264-live-test-end",
    };

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
        .post(format!("http://{api_addr}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": unique_frame,
            "messages": [{"role": "user", "content": "prove #264 fail-closed"}],
            "max_tokens": 1,
            "stream": false
        }))
        .send()
        .await
        .expect("send OpenAI-compatible request");
    let status = response.status();
    let body = response.text().await.expect("response body");
    eprintln!(
        "issue264 fail-closed response: order_id={} tc={} status={} body={}",
        order.order_id, matched, status, body
    );
    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("opened=false"), "{body}");
    assert!(body.contains("cleanup_ready=false"), "{body}");
    assert!(
        !body.contains("upstream open failed"),
        "request must fail before stale handover/upstream is used: {body}"
    );
    assert!(
        !body.contains("\"choices\""),
        "fail-closed response must not contain user-visible completion choices: {body}"
    );
    assert!(session.is_closed(), "API session closes immediately");
    assert!(
        !session.is_settled(),
        "before MATCH_OPEN_TIMEOUT there is no cleanup write and the session remains recoverable"
    );

    let before_timeout_state = be
        .token_contract_state(&tc)
        .await
        .expect("getState after fail-closed request")
        .expect("TC still active before cleanup");
    assert_eq!(
        before_timeout_state["funded"].as_bool(),
        Some(true),
        "{before_timeout_state:?}"
    );
    assert_eq!(
        before_timeout_state["opened"].as_bool(),
        Some(false),
        "{before_timeout_state:?}"
    );

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now < cleanup_at {
        tokio::time::sleep(Duration::from_secs(cleanup_at - now + 5)).await;
    }
    let settlement = chain
        .cleanup_unopened(&matched)
        .await
        .expect("timeout-ready cleanup_unopened");
    session.mark_recovered("issue264-live-cleanup-complete");
    let _ = shutdown_tx.send(());
    api_task.await.expect("buyer API task joins after shutdown");
    let after_cleanup_state = be
        .token_contract_state(&tc)
        .await
        .expect("getState after cleanup");
    eprintln!(
        "issue264 cleanup evidence: tc={} settlement={settlement:?} state_after={after_cleanup_state:?}",
        matched
    );
    assert!(
        after_cleanup_state
            .as_ref()
            .map_or(true, |st| st["funded"].as_bool() == Some(false)),
        "cleanup must clear funded-never-opened state: {after_cleanup_state:?}"
    );
}

/// #208 live duplicate-post probe: with one active ask for a TC, bypass the seller backend preflight and send
/// a second direct `postSellOffer` through `RealChainBackend`. Acceptance evidence is the direct submit
/// result/error plus a live book reread proving the TC still has exactly one resting ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live shellnet #208; posts and cancels a real resting ask"]
async fn live_208_duplicate_post_rejected_by_direct_contract_path() {
    let Some(pool) = live_pool_or_skip(
        "live_208_duplicate_post_rejected_by_direct_contract_path",
        1,
    ) else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 1).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    assert!(
        s_ecc2 >= ISSUE208_DEPLOY_GAS * 2,
        "seller note needs deploy headroom, got {s_ecc2}"
    );
    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let seller_note_addr = Address::parse(&s_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}-issue208-dup-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 2;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note_addr,
            &unique_frame,
            nonce,
            price,
            max_ticks,
            ISSUE208_DEPLOY_GAS,
        )
        .await
        .expect("provision market");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    assert!(wait_active(&be, &tc).await, "TC active");

    let first_submit = be
        .post_sell_offer(&seller_note_addr, &seller_kp, 0, nonce)
        .await
        .expect("first direct postSellOffer");
    let order = wait_resting_ask(&be, &ob, &unique_frame, &manifest.model_hash, &tc_arg)
        .await
        .expect("first ask rests");

    let duplicate_submit = be
        .post_sell_offer(&seller_note_addr, &seller_kp, 0, nonce)
        .await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let snapshot = be
        .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
        .await
        .expect("reread order book after duplicate direct post");
    let matching: Vec<_> = snapshot
        .resting_asks()
        .filter(|o| {
            o.token_contract
                .as_deref()
                .is_some_and(|tc| tc.eq_ignore_ascii_case(&tc_arg))
        })
        .collect();
    let matching_order_ids = matching
        .iter()
        .map(|o| o.order_id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let duplicate_result = match &duplicate_submit {
        Ok(v) => format!("submit_ok={v}"),
        Err(e) => format!("submit_err={e}"),
    };
    eprintln!(
        "issue208 duplicate direct-post proof: first_order_id={} tc={} first_submit={} \
         duplicate_result={} matching_ask_count={} matching_order_ids=[{}]",
        order.order_id,
        tc_arg,
        first_submit,
        duplicate_result,
        matching.len(),
        matching_order_ids
    );
    be.cancel_all_inference_orders(&seller_note_addr, &seller_kp, &manifest.model_hash)
        .await
        .expect("cancel live ask(s) after duplicate-post test");
    assert_eq!(
        matching.len(),
        1,
        "direct duplicate postSellOffer must not create a second resting ask"
    );
    assert_eq!(
        matching[0].order_id, order.order_id,
        "the original ask must remain the only active ask for the TC"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: #75 fresh-note dexdo provision regression on shellnet"]
async fn live_cli_fresh_note_provision_deploys_root_model() {
    let Ok(pool_path) = std::env::var("DEXDO_FRESH_PN_POOL") else {
        eprintln!(
            "DEXDO_FRESH_PN_POOL not set — skipping (requires a fresh one-shot pn_pool.json)"
        );
        return;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes[]");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s sec");
    let s_addr = notes[0]["address"].as_str().expect("s addr").to_string();

    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let seller_kp = KeyPair::from_secret_hex(s_sec).expect("seller kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let seller_pubkey = serde_json::json!(format!("0x{}", seller_kp.public_hex()));
    let rm = be
        .root_model_address_for(&seller_pubkey)
        .await
        .expect("root model address");
    assert!(
        !is_active_once(&be, &rm).await,
        "DEXDO_FRESH_PN_POOL note is not fresh: RootModel {rm} is already active"
    );

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}-issue75-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 1024;
    let tc = be
        .token_contract_deploy_address(
            &seller_kp,
            &rm,
            nonce,
            &unique_frame,
            MODEL_TICK_SIZE,
            price,
            max_ticks,
            &seller_note,
        )
        .await
        .expect("token contract deploy address");
    assert!(
        !is_active_once(&be, &tc).await,
        "DEXDO_FRESH_PN_POOL note is not fresh for nonce {nonce}: TC {tc} is already active"
    );

    let dir = create_private_temp_dir(&format!("dexdo_issue75_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let s_key = write_key(&dir, "seller.key", s_sec);
    let market_path = dir.join("market.json");
    let nonce_s = nonce.to_string();
    let price_s = price.to_string();
    let max_ticks_s = max_ticks.to_string();
    let market_s = market_path.to_string_lossy().to_string();

    let output = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "provision",
            "--note-key",
            s_key.to_str().unwrap(),
            "--note-addr",
            &s_addr,
            "--frame-model",
            &unique_frame,
            "--contracts",
            MANIFEST,
            "--nonce",
            &nonce_s,
            "--price-per-tick",
            &price_s,
            "--max-ticks",
            &max_ticks_s,
            "--deposit-shells",
            "20",
            "--output",
            &market_s,
        ])
        .output()
        .expect("run dexdo provision");
    assert!(
        output.status.success(),
        "dexdo provision failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let market: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&market_path).expect("read market"))
            .expect("parse market");
    assert_eq!(market["root_model"].as_str().unwrap(), rm.with_workchain());
    assert_eq!(
        market["token_contract"].as_str().unwrap(),
        tc.with_workchain()
    );
    assert!(
        wait_active(&be, &rm).await,
        "RootModel active after CLI provision"
    );
    assert!(wait_active(&be, &tc).await, "TC active after CLI provision");
}

/// #163: literal subscription semantics proof without waiting a week.
///
/// A subscription rests as maker BUY at a high max price. A cheaper incoming SELL fully fills it in
/// cycle 0. By contract, the current-cycle unspent budget is recorded in `getForfeit`, while the
/// buyer's ECC delta must stay inside the current-cycle budget instead of losing future cycles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live shellnet; proves subscription full-fill current-cycle forfeit + future-cycle refund"]
async fn live_subscription_full_fill_forfeits_current_cycle_and_refunds_future_cycles() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set - skipping (minted notes pn_pool.json are required)");
        return;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let notes = pool["notes"].as_array().expect("notes[]");
    assert!(notes.len() >= 2, "need >=2 minted notes (seller+buyer)");
    let mut role_notes = Vec::new();
    for note in notes.iter().take(2) {
        let addr = note["address"].as_str().expect("note addr").to_string();
        let secret = note["owner_secret_key_hex"]
            .as_str()
            .expect("note secret")
            .to_string();
        let address = Address::parse(&addr).expect("note addr parses");
        let ecc2 = ecc_shell_balance(&be, &address).await;
        role_notes.push((addr, secret, ecc2));
    }
    role_notes.sort_by(|a, b| b.2.cmp(&a.2));
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= 20_000_000_000,
        "seller note needs deploy headroom, got {s_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let buyer_kp = KeyPair::from_secret_hex(&b_sec).expect("buyer kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let buyer_note = Address::parse(&b_addr).expect("buyer note addr");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}#subfill{nonce}");
    let seller_price: u128 = 1_000;
    let subscription_max_price: u128 = 10_000;
    let ticks: u128 = 1;
    let seller_max_ticks: u128 = 4;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &unique_frame,
            nonce,
            seller_price,
            seller_max_ticks,
            10_000_000_000,
        )
        .await
        .expect("provision market (note-funded)");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    let initial = be
        .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
        .await
        .expect("initial book snapshot");
    let sub_order_id = initial.stats.as_ref().expect("active book").next_order_id;
    let unit = required_escrow_for_buy(1, subscription_max_price);
    let sub_escrow = unit * 8;
    let seller_cost = required_escrow_for_buy(ticks, seller_price);
    let clearing_cost = required_escrow_for_buy(ticks, subscription_max_price);
    let cycle_budget = sub_escrow / 4;
    let expected_forfeit = cycle_budget - clearing_cost;
    let expected_future_refund = sub_escrow - clearing_cost - expected_forfeit;
    let max_current_cycle_debit = clearing_cost + expected_forfeit;
    assert!(
        clearing_cost < cycle_budget,
        "test must leave current-cycle unspent"
    );
    assert!(expected_forfeit > 0, "test must prove a non-zero forfeit");
    assert!(
        expected_future_refund > 0,
        "test must prove a non-zero future refund"
    );

    let buyer_before = ecc_shell_balance(&be, &buyer_note).await;
    assert!(
        buyer_before >= sub_escrow,
        "buyer note needs enough SHELL for subscription escrow (before={buyer_before}, initial={b_ecc2})"
    );
    be.place_inference_subscription(
        &buyer_note,
        &buyer_kp,
        &manifest.model_hash,
        subscription_max_price,
        ticks,
        sub_escrow,
        false,
    )
    .await
    .expect("place subscription");

    let mut placed = None;
    for _ in 0..40 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("book snapshot after subscription");
        let order_found = snapshot.orders.iter().any(|o| o.order_id == sub_order_id);
        let sub = be
            .inference_orderbook_subscription(&ob, sub_order_id)
            .await
            .expect("subscription getter");
        if order_found && sub.as_ref().map(|s| s.exists).unwrap_or(false) {
            placed = sub;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let placed = placed.expect("subscription rested in the book");
    assert_eq!(placed.cycle_budget, cycle_budget);
    assert_eq!(placed.cycle_spent, 0);

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post crossing sell offer");

    let mut evidence = None;
    let mut last_observed = String::new();
    for _ in 0..60 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("book snapshot after fill");
        let executed = snapshot
            .stats
            .as_ref()
            .map(|s| s.executed_ticks >= ticks)
            .unwrap_or(false);
        let order_gone = !snapshot.orders.iter().any(|o| o.order_id == sub_order_id);
        let sub = be
            .inference_orderbook_subscription(&ob, sub_order_id)
            .await
            .expect("subscription getter after fill");
        let forfeit = be
            .inference_orderbook_forfeit(&ob, sub_order_id, 0)
            .await
            .expect("forfeit getter after fill");
        let buyer_after = ecc_shell_balance(&be, &buyer_note).await;
        let buyer_paid = buyer_before.saturating_sub(buyer_after);
        last_observed = format!(
            "executed={} order_gone={} sub={:?} forfeit={:?} buyer_ecc_delta={} min_delta={} max_current_cycle_delta={} stats={:?} orders={}",
            executed,
            order_gone,
            sub,
            forfeit,
            buyer_paid,
            clearing_cost,
            max_current_cycle_debit,
            snapshot.stats,
            snapshot.orders.len()
        );
        if let (Some(sub), Some((pool, funded_ticks))) = (sub, forfeit) {
            if executed
                && order_gone
                && !sub.exists
                && pool == expected_forfeit
                && funded_ticks == ticks
                && buyer_paid >= clearing_cost
                && buyer_paid <= max_current_cycle_debit
            {
                evidence = Some((pool, funded_ticks, buyer_after));
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let (pool, funded_ticks, buyer_after) = evidence.unwrap_or_else(|| {
        panic!("subscription full-fill forfeit/refund evidence; last_observed={last_observed}")
    });
    assert_eq!(pool, expected_forfeit);
    assert_eq!(funded_ticks, ticks);
    let buyer_ecc_delta = buyer_before - buyer_after;
    assert!(buyer_ecc_delta >= clearing_cost);
    assert!(buyer_ecc_delta <= max_current_cycle_debit);
    println!(
        "=== #163 subscription full-fill proof order_id={sub_order_id} ob={} tc={} ticks={ticks} \
         sub_escrow={sub_escrow} seller_cost={seller_cost} clearing_cost={clearing_cost} cycle_budget={cycle_budget} \
         forfeit_pool={pool} contract_future_refund={expected_future_refund} buyer_ecc_delta={buyer_ecc_delta} ===",
        ob.with_workchain(),
        tc.with_workchain()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: provisions a deal + spawns dexdo seller/buyer against shellnet (real submits + network)"]
async fn live_cli_deal_flow_handover() {
    run_live_cli_deal_flow_handover(0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live #198: seller waits after readiness for a late buyer, then opens handover"]
async fn live_cli_late_buyer_handover() {
    run_live_cli_deal_flow_handover(configured_late_buyer_delay_secs()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live #309/#328/#18: qwen on-demand binds first, rejects one-tick buy, then valid 4-tick buy streams and STOPs"]
async fn live_cli_on_demand_binds_before_buy_then_streams() {
    let Some(pool) = live_pool_or_skip("live_cli_on_demand_binds_before_buy_then_streams", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_in_pool_order(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    let deploy_gas: u128 = ISSUE208_DEPLOY_GAS;
    assert!(
        s_ecc2 >= deploy_gas * 2,
        "seller note needs deploy headroom, got {s_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Representative #328 shape: the real public qwen frame, but an explicit freshly provisioned TC so the proof
    // exercises the user's model/input shape without letting stale public-book asks outrank this run's fresh ask.
    let frame_model = FRAME_MODEL.to_string();
    let price: u128 = 1;
    let max_ticks: u128 = 4;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &frame_model,
            nonce,
            price,
            max_ticks,
            deploy_gas,
        )
        .await
        .expect("provision market (note-funded)");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    let dir = create_private_temp_dir(&format!("dexdo_live_309_328_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let s_key = write_key(&dir, "seller.key", &s_sec);
    let b_key = write_key(&dir, "buyer.key", &b_sec);
    let seller_deals_dir = dir.join("deals-seller");
    let invalid_deals_dir = dir.join("deals-invalid-buyer");
    let valid_deals_dir = dir.join("deals-valid-buyer");
    let policy_path = dir.join("policy.json");
    std::fs::write(
        &policy_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "buyer": {
                "on": {
                    "no_handover_after_match": "wait_then_reclaim",
                    "malformed_handover": "fail_closed",
                    "dead_gateway": "retry_then_reclaim",
                    "empty_stream": "fail_closed",
                    "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                    "bad_output_scam": "stop"
                },
                "failover": {
                    "max_sellers_to_try": 1,
                    "total_spend_cap_shells": 1
                }
            },
            "seller": {
                "on": {
                    "after_deal_done": "retire",
                    "buyer_no_show": "retire_gateway",
                    "dispute_against_me": "release_if_clean"
                },
                "max_open_deals": 1
            }
        }))
        .expect("policy serializes"),
    )
    .expect("write policy");
    let models_path = dir.join("models.json");
    std::fs::write(
        &models_path,
        format!(
            r#"{{"models":{{"livetest":{{"frame_model":"{frame_model}","base_url":"http://localhost:1","served_model":"x","api_key_env":"NONE_LIVE","tokenizer_family":"qwen","price_per_tick":{price},"capabilities":{{"logprobs":false,"top_logprobs":0}}}}}}}}"#
        ),
    )
    .expect("write temp models.json");

    let bin = env!("CARGO_BIN_EXE_dexdo");
    let gateway = free_loopback_addr();
    let seller_log = dir.join("seller.log");
    let invalid_buyer_log = dir.join("buyer-invalid.log");
    let buyer_log = dir.join("buyer.log");
    let slog = std::fs::File::create(&seller_log).expect("seller log");
    let mut seller = Command::new(bin)
        .current_dir(WORKSPACE)
        .args([
            "seller",
            "--mock-model",
            "--note-key",
            s_key.to_str().unwrap(),
            "--note-addr",
            &s_addr,
            "--model",
            "livetest",
            "--models",
            models_path.to_str().unwrap(),
            "--token-contract",
            &tc_arg,
            "--nonce",
            &nonce.to_string(),
            "--gateway-listen",
            &gateway,
            "--contracts",
            MANIFEST,
            "--price-per-tick",
            &price.to_string(),
            "--probe-shell",
            "1000000",
            "--mock-token-count",
            "4",
            "--deals-dir",
            seller_deals_dir.to_str().unwrap(),
            "--policy",
            policy_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::from(slog.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(slog))
        .spawn()
        .expect("spawn seller");

    let (mut offer_rested, mut gateway_ready) = (false, false);
    let mut last_offer_probe = "not checked yet".to_string();
    for _ in 0..6 {
        assert_child_still_running(&mut seller, &seller_log, "seller readiness wait");
        if !offer_rested {
            match fresh_tc_resting_ask_once(&be, &ob, &frame_model, &manifest.model_hash, &tc_arg)
                .await
            {
                Ok(true) => offer_rested = true,
                Ok(false) => last_offer_probe = "fresh TC not visible in resting asks".to_string(),
                Err(e) => last_offer_probe = e,
            }
        }
        gateway_ready |= gateway_accepting(&gateway);
        if offer_rested && gateway_ready {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(
        offer_rested,
        "fresh qwen seller offer for {tc_arg} rested before buyer starts; last={last_offer_probe}; seller_tail={}",
        tail(&std::fs::read_to_string(&seller_log).unwrap_or_default(), 3000)
    );
    if !gateway_ready {
        eprintln!("seller gateway was not accepting before buyer start; continuing to prove it through the first chat request");
    }

    let invalid_blog = std::fs::File::create(&invalid_buyer_log).expect("invalid buyer log");
    let mut invalid_buyer = Command::new(bin)
        .current_dir(WORKSPACE)
        .args([
            "buyer",
            "--json",
            "--mock-model",
            "--allow-unverified-model",
            "--note-key",
            b_key.to_str().unwrap(),
            "--note-addr",
            &b_addr,
            "--frame-model",
            &frame_model,
            "--token-contract",
            &tc_arg,
            "--ticks",
            "1",
            "--contracts",
            MANIFEST,
            "--max-tokens",
            "1",
            "--local-listen",
            "127.0.0.1:0",
            "--continuity-mode",
            "on-demand",
            "--deals-dir",
            invalid_deals_dir.to_str().unwrap(),
            "--policy",
            policy_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(invalid_blog))
        .spawn()
        .expect("spawn one-tick buyer on-demand API");
    let invalid_buyer_lines = stdout_lines(&mut invalid_buyer);
    let mut invalid_events = Vec::new();
    let invalid_ready = loop {
        let line = invalid_buyer_lines
            .recv_timeout(Duration::from_secs(15))
            .expect("one-tick buyer endpoint_ready within seconds");
        let event: serde_json::Value = serde_json::from_str(&line).expect("buyer JSONL");
        eprintln!("one-tick buyer event before first chat: {event}");
        invalid_events.push(event.clone());
        if event["event"] == "endpoint_ready" {
            break event;
        }
    };
    let invalid_names = invalid_events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert_eq!(
        invalid_names,
        vec!["starting", "endpoint_binding", "endpoint_ready"],
        "one-tick on-demand buyer must bind before purchase work"
    );
    assert_eq!(invalid_ready["token_contract"], "pending:on-demand");

    let invalid_base_url = invalid_ready["base_url"].as_str().expect("base_url");
    let invalid_chat_url = format!("{invalid_base_url}/chat/completions");
    let invalid_response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
        .post(&invalid_chat_url)
        .json(&serde_json::json!({
            "model": frame_model,
            "messages": [{"role": "user", "content": "prove directive 328 invalid ticks"}],
            "max_tokens": 1,
            "stream": false
        }))
        .send()
        .await
        .expect("POST /v1/chat/completions triggers one-tick validation");
    let invalid_status = invalid_response.status();
    let invalid_body = invalid_response.text().await.expect("chat body");
    eprintln!("directive328 one-tick chat status={invalid_status} body={invalid_body}");
    assert_eq!(
        invalid_status,
        reqwest::StatusCode::BAD_REQUEST,
        "{invalid_body}"
    );
    assert!(invalid_body.contains("invalid buy ticks"), "{invalid_body}");
    assert!(invalid_body.contains("--ticks 1"), "{invalid_body}");
    assert!(
        invalid_body.contains("2-tick stream minimum"),
        "{invalid_body}"
    );
    assert!(
        !invalid_body.contains("internal invariant failed"),
        "{invalid_body}"
    );

    let mut post_invalid_events = Vec::new();
    for _ in 0..20 {
        match invalid_buyer_lines.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                let event: serde_json::Value = serde_json::from_str(&line).expect("buyer JSONL");
                eprintln!("one-tick buyer event after first chat: {event}");
                assert_ne!(event["schema"], "dexdo.error.v1", "{event}");
                post_invalid_events.push(event);
                if post_invalid_events
                    .iter()
                    .any(|event| event["event"] == "purchase_progress")
                {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let post_invalid_names = post_invalid_events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert!(
        post_invalid_names.contains(&"purchase_progress"),
        "first chat must reach lazy purchase validation: {post_invalid_names:?}"
    );
    assert!(
        !post_invalid_names.contains(&"quote_selected")
            && !post_invalid_names.contains(&"buy_submitted")
            && !post_invalid_names.contains(&"matched"),
        "one-tick validation must fail before quote/buy/match: {post_invalid_names:?}"
    );
    let invalid_state = be
        .token_contract_state(&tc)
        .await
        .expect("read TC state")
        .expect("TC state exists");
    assert_eq!(
        invalid_state["funded"].as_bool(),
        Some(false),
        "{invalid_state:?}"
    );
    assert_child_still_running(
        &mut seller,
        &seller_log,
        "seller must remain available after one-tick rejection",
    );
    assert!(
        fresh_tc_resting_ask_once(&be, &ob, &frame_model, &manifest.model_hash, &tc_arg)
            .await
            .expect("fresh TC ask still readable after one-tick rejection"),
        "fresh ask must remain after one-tick rejection"
    );
    terminate_child(&mut invalid_buyer);
    let mut invalid_buyer_status = None;
    for _ in 0..30 {
        if let Some(status) = invalid_buyer.try_wait().expect("invalid buyer try_wait") {
            invalid_buyer_status = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let invalid_buyer_status =
        invalid_buyer_status.expect("one-tick buyer graceful SIGTERM must finish within 30s");
    assert!(
        invalid_buyer_status.success(),
        "one-tick buyer graceful shutdown failed; status={invalid_buyer_status:?}\n--- buyer-invalid.log ---\n{}",
        tail(
            &std::fs::read_to_string(&invalid_buyer_log).unwrap_or_default(),
            4000
        )
    );

    let blog = std::fs::File::create(&buyer_log).expect("buyer log");
    let mut buyer = Command::new(bin)
        .current_dir(WORKSPACE)
        .args([
            "buyer",
            "--json",
            "--mock-model",
            "--allow-unverified-model",
            "--note-key",
            b_key.to_str().unwrap(),
            "--note-addr",
            &b_addr,
            "--frame-model",
            &frame_model,
            "--token-contract",
            &tc_arg,
            "--ticks",
            "4",
            "--contracts",
            MANIFEST,
            "--max-tokens",
            "4",
            "--local-listen",
            "127.0.0.1:0",
            "--continuity-mode",
            "on-demand",
            "--deals-dir",
            valid_deals_dir.to_str().unwrap(),
            "--policy",
            policy_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(blog))
        .spawn()
        .expect("spawn buyer on-demand API");
    let buyer_lines = stdout_lines(&mut buyer);
    let ready_start = Instant::now();
    let mut events = Vec::new();
    let ready = loop {
        let line = buyer_lines
            .recv_timeout(Duration::from_secs(15))
            .expect("buyer endpoint_ready within seconds");
        let event: serde_json::Value = serde_json::from_str(&line).expect("buyer JSONL");
        eprintln!("buyer event before first chat: {event}");
        events.push(event.clone());
        if event["event"] == "endpoint_ready" {
            break event;
        }
    };
    let ready_elapsed = ready_start.elapsed();
    assert!(
        ready_elapsed <= Duration::from_secs(15),
        "endpoint_ready must happen within seconds, got {ready_elapsed:?}"
    );
    let names = events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["starting", "endpoint_binding", "endpoint_ready"],
        "no quote/place_buy/matched events may run before endpoint_ready"
    );
    assert_eq!(ready["token_contract"], "pending:on-demand");
    let models_url = ready["models_url"].as_str().expect("models_url");
    let models: serde_json::Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .get(models_url)
        .send()
        .await
        .expect("GET /v1/models")
        .error_for_status()
        .expect("models status")
        .json()
        .await
        .expect("models json");
    assert_eq!(models["data"][0]["id"], frame_model);

    let base_url = ready["base_url"].as_str().expect("base_url");
    let chat_url = format!("{base_url}/chat/completions");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(260))
        .build()
        .unwrap()
        .post(&chat_url)
        .json(&serde_json::json!({
            "model": frame_model,
            "messages": [{"role": "user", "content": "prove directive 328 and public issue 18 happy path"}],
            "max_tokens": 4,
            "stream": false
        }))
        .send()
        .await
        .expect("POST /v1/chat/completions triggers on-demand buy");
    let status = response.status();
    let body = response.text().await.expect("chat body");
    eprintln!("directive309_328 happy chat status={status} body={body}");
    if !status.is_success() {
        let s_txt = std::fs::read_to_string(&seller_log).unwrap_or_default();
        let b_txt = std::fs::read_to_string(&buyer_log).unwrap_or_default();
        panic!(
            "chat failed status={status} body={body}\n--- seller.log ---\n{}\n--- buyer.log ---\n{}",
            tail(&s_txt, 4000),
            tail(&b_txt, 4000)
        );
    }
    assert!(body.contains("\"choices\""), "{body}");

    let state = be
        .token_contract_state(&tc)
        .await
        .expect("read TC state")
        .expect("TC state exists");
    eprintln!("directive309 pre-shutdown TC state={state:?}");
    assert_eq!(state["funded"].as_bool(), Some(true), "{state:?}");
    assert_eq!(state["opened"].as_bool(), Some(true), "{state:?}");

    terminate_child(&mut buyer);
    let mut buyer_status = None;
    for _ in 0..90 {
        if let Some(status) = buyer.try_wait().expect("buyer try_wait") {
            buyer_status = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let buyer_status =
        buyer_status.expect("buyer graceful SIGTERM must finish and await STOP within 90s");
    assert!(
        buyer_status.success(),
        "buyer graceful shutdown failed; status={buyer_status:?}\n--- buyer.log ---\n{}",
        tail(
            &std::fs::read_to_string(&buyer_log).unwrap_or_default(),
            4000
        )
    );
    let mut final_state = None;
    for _ in 0..30 {
        if let Ok(Some(st)) = be.token_contract_state(&tc).await {
            if st["opened"].as_bool() == Some(false) {
                final_state = Some(st);
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let final_state = final_state.expect("post-STOP TC state reaches opened=false");
    assert_eq!(
        json_u64_field(&final_state, "deposit"),
        Some(0),
        "{final_state:?}"
    );
    assert_eq!(
        json_u64_field(&final_state, "prepaid"),
        Some(0),
        "{final_state:?}"
    );
    assert_eq!(
        json_u64_field(&final_state, "frozen"),
        Some(0),
        "{final_state:?}"
    );
    assert_eq!(
        json_u64_field(&final_state, "finalizedOwed"),
        Some(0),
        "{final_state:?}"
    );
    let _ = seller.kill();
    let _ = seller.wait();
    eprintln!(
        "=== #309/#328/#18 live proof: frame={FRAME_MODEL} endpoint_ready_elapsed={ready_elapsed:?} tc={} invalid_status={} invalid_state={invalid_state:?} happy_status={} post_stop_state={final_state:?} ===",
        tc.with_workchain(),
        invalid_status,
        status
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live #9: contract by-fact for partial-taking a 1024-tick ask with an 8-tick buy"]
async fn live_9_contract_partial_take_buy_funds_8_from_1024_ask() {
    let Some(pool) = live_pool_or_skip("live_9_contract_partial_take_buy_funds_8_from_1024_ask", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= 15_000_000_000,
        "seller note needs deploy headroom, got {s_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let buyer_kp = KeyPair::from_secret_hex(&b_sec).expect("buyer kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let buyer_note = Address::parse(&b_addr).expect("buyer note addr");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let frame_model = format!("{FRAME_MODEL}-partial-take-{nonce}");
    let price: u128 = 1;
    let ask_ticks: u128 = 1024;
    let buy_ticks: u128 = 8;
    let escrow = required_escrow_for_buy(buy_ticks, price);

    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &frame_model,
            nonce,
            price,
            ask_ticks,
            10_000_000_000,
        )
        .await
        .expect("provision market for partial-take by-fact");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post 1024-tick ask");

    let mut ask_snapshot = None;
    for _ in 0..40 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &frame_model, &manifest.model_hash)
            .await
            .expect("read orderbook snapshot");
        if snapshot.orders.iter().any(|o| {
            o.is_resting_ask()
                && o.token_contract
                    .as_deref()
                    .map(|addr| addr.eq_ignore_ascii_case(&tc.with_workchain()))
                    .unwrap_or(false)
        }) {
            ask_snapshot = Some(snapshot);
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let ask_snapshot = ask_snapshot.expect("1024-tick ask rested before partial buy");
    let live_quote = be
        .submit_safe_single_ask_quote(&ask_snapshot, Some(buy_ticks), None)
        .await
        .expect("live quote 8 from 1024");
    assert!(live_quote.complete, "{live_quote:?}");
    assert_eq!(live_quote.filled_ticks, buy_ticks, "{live_quote:?}");
    assert_eq!(live_quote.fills.len(), 1, "{live_quote:?}");
    assert_eq!(live_quote.fills[0].ticks, buy_ticks, "{live_quote:?}");
    assert_eq!(
        live_quote.total_with_fee, escrow,
        "live quote must escrow exactly the 8-tick buy"
    );

    be.place_inference_buy(
        &buyer_note,
        &buyer_kp,
        &manifest.model_hash,
        price,
        buy_ticks,
        escrow,
        0,
        0,
    )
    .await
    .expect("place 8-tick model buy against 1024-tick ask");

    let mut funded_state = None;
    for _ in 0..40 {
        let state = be.token_contract_state(&tc).await.expect("read TC state");
        if state
            .as_ref()
            .and_then(|s| s["funded"].as_bool())
            .unwrap_or(false)
        {
            funded_state = state;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let state = funded_state.expect("partial-take buy funded the seller TC");
    let deposit = state["deposit"]
        .as_str()
        .and_then(|raw| raw.parse::<u128>().ok())
        .expect("deposit");
    let frozen = state["frozen"]
        .as_str()
        .and_then(|raw| raw.parse::<u128>().ok())
        .expect("frozen");
    assert_eq!(deposit, buy_ticks, "{state:?}");
    assert_eq!(frozen, 0, "{state:?}");

    let after = be
        .inference_orderbook_snapshot(&ob, &frame_model, &manifest.model_hash)
        .await
        .expect("read post-match orderbook snapshot");
    let matching_ask_count = after
        .orders
        .iter()
        .filter(|o| {
            o.is_resting_ask()
                && o.token_contract
                    .as_deref()
                    .map(|addr| addr.eq_ignore_ascii_case(&tc.with_workchain()))
                    .unwrap_or(false)
        })
        .count();
    assert_eq!(
        matching_ask_count, 0,
        "partial-taken SELL slot must be consumed whole"
    );
    eprintln!(
        "=== #9 live by-fact: ask_ticks={ask_ticks} buy_ticks={buy_ticks} escrow={escrow} tc={} funded=true deposit={deposit} frozen={frozen} matching_ask_count={matching_ask_count} ===",
        tc.with_workchain()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live #9/#8: stale duplicate TC row must quote no_liquidity after one duplicate is matched"]
async fn live_9_stale_duplicate_tc_row_quotes_no_liquidity() {
    let Some(pool) = live_pool_or_skip("live_9_stale_duplicate_tc_row_quotes_no_liquidity", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= 15_000_000_000,
        "seller note needs deploy headroom, got {s_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let buyer_kp = KeyPair::from_secret_hex(&b_sec).expect("buyer kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let buyer_note = Address::parse(&b_addr).expect("buyer note addr");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let frame_model = format!("{FRAME_MODEL}-stale-duplicate-{nonce}");
    let price: u128 = 1;
    let ask_ticks: u128 = 1024;
    let buy_ticks: u128 = 8;
    let escrow = required_escrow_for_buy(buy_ticks, price);

    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &frame_model,
            nonce,
            price,
            ask_ticks,
            10_000_000_000,
        )
        .await
        .expect("provision market for stale duplicate by-fact");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    for idx in 0..2 {
        be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
            .await
            .unwrap_or_else(|e| panic!("post duplicate {idx} 1024-tick ask: {e}"));
    }

    let mut matching_ask_count = 0usize;
    for _ in 0..40 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &frame_model, &manifest.model_hash)
            .await
            .expect("read orderbook snapshot");
        matching_ask_count = snapshot
            .orders
            .iter()
            .filter(|o| {
                o.is_resting_ask()
                    && o.token_contract
                        .as_deref()
                        .map(|addr| addr.eq_ignore_ascii_case(&tc.with_workchain()))
                        .unwrap_or(false)
            })
            .count();
        if matching_ask_count >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(
        matching_ask_count >= 1,
        "at least one TC ask must rest before stale-row proof"
    );
    if matching_ask_count < 2 {
        eprintln!(
            "=== #9/#8 live stale-row setup: duplicate post did not produce duplicate rows on current contracts (matching_ask_count={matching_ask_count}); continuing with post-used-TC fallback ==="
        );
    }

    be.place_inference_buy(
        &buyer_note,
        &buyer_kp,
        &manifest.model_hash,
        price,
        buy_ticks,
        escrow,
        0,
        0,
    )
    .await
    .expect("place 8-tick buy against duplicate ask");

    let mut funded = false;
    for _ in 0..40 {
        let state = be.token_contract_state(&tc).await.expect("read TC state");
        if state
            .as_ref()
            .and_then(|s| s["funded"].as_bool())
            .unwrap_or(false)
        {
            funded = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(funded, "duplicate-row buy funded the TC");

    let mut after = be
        .inference_orderbook_snapshot(&ob, &frame_model, &manifest.model_hash)
        .await
        .expect("read post-match orderbook snapshot");
    let mut stale_count = after
        .orders
        .iter()
        .filter(|o| {
            o.is_resting_ask()
                && o.token_contract
                    .as_deref()
                    .map(|addr| addr.eq_ignore_ascii_case(&tc.with_workchain()))
                    .unwrap_or(false)
        })
        .count();
    if stale_count == 0 {
        let repost = be.post_sell_offer(&seller_note, &seller_kp, 0, nonce).await;
        eprintln!(
            "=== #9/#8 live stale-row fallback: post-used-TC returned ok={} ===",
            repost.is_ok()
        );
        for _ in 0..20 {
            after = be
                .inference_orderbook_snapshot(&ob, &frame_model, &manifest.model_hash)
                .await
                .expect("read fallback stale orderbook snapshot");
            stale_count = after
                .orders
                .iter()
                .filter(|o| {
                    o.is_resting_ask()
                        && o.token_contract
                            .as_deref()
                            .map(|addr| addr.eq_ignore_ascii_case(&tc.with_workchain()))
                            .unwrap_or(false)
                })
                .count();
            if stale_count >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    if stale_count == 0 {
        eprintln!(
            "=== #9/#8 live stale-row evidence: current contracts did not leave/recreate a stale TC row for {}; stale-row quote path not live-reproducible on this fresh market ===",
            tc.with_workchain()
        );
        return;
    }
    assert!(
        stale_count >= 1,
        "duplicate row should leave at least one stale resting ask for the used TC"
    );

    let quote = be
        .submit_safe_single_ask_quote(&after, Some(buy_ticks), None)
        .await
        .expect("stale duplicate quote check");
    assert!(!quote.complete, "{quote:?}");
    assert_eq!(quote.filled_ticks, 0, "{quote:?}");
    assert!(quote.fills.is_empty(), "{quote:?}");
    eprintln!(
        "=== #9/#8 live stale-row evidence: buy_ticks={buy_ticks} tc={} funded=true stale_matching_ask_count={stale_count} quote_complete={} quote_filled={} ===",
        tc.with_workchain(),
        quote.complete,
        quote.filled_ticks
    );
}

struct Live10BuyerApiResult {
    status: reqwest::StatusCode,
    body: String,
    events: Vec<serde_json::Value>,
}

fn live10_push_buyer_event(
    label: &str,
    line: String,
    events: &mut Vec<serde_json::Value>,
) -> serde_json::Value {
    let event: serde_json::Value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("#10 {label}: buyer JSONL: {e}; line={line}"));
    eprintln!("#10 {label}: buyer event {event}");
    events.push(event.clone());
    event
}

fn live10_event_names(events: &[serde_json::Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or(""))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn run_live10_real_model_buyer_api(
    bin: &str,
    label: &str,
    b_key: &Path,
    b_addr: &str,
    frame_model: &str,
    token_contract: Option<&str>,
    ticks: u128,
    max_price_per_tick: u128,
    buyer_deals_dir: &Path,
    policy_path: &Path,
    buyer_log: &Path,
) -> Live10BuyerApiResult {
    let blog = std::fs::File::create(buyer_log).expect("#10 buyer log");
    let mut buyer_args = vec![
        "buyer".to_string(),
        "--json".to_string(),
        "--note-key".to_string(),
        b_key.to_str().unwrap().to_string(),
        "--note-addr".to_string(),
        b_addr.to_string(),
        "--frame-model".to_string(),
        frame_model.to_string(),
        "--models".to_string(),
        "models.json".to_string(),
        "--ticks".to_string(),
        ticks.to_string(),
        "--max-price-per-tick".to_string(),
        max_price_per_tick.to_string(),
        "--contracts".to_string(),
        MANIFEST.to_string(),
        "--max-tokens".to_string(),
        ticks.to_string(),
        "--local-listen".to_string(),
        "127.0.0.1:0".to_string(),
        "--deals-dir".to_string(),
        buyer_deals_dir.to_str().unwrap().to_string(),
        "--policy".to_string(),
        policy_path.to_str().unwrap().to_string(),
    ];
    if let Some(tc) = token_contract {
        buyer_args.push("--token-contract".to_string());
        buyer_args.push(tc.to_string());
    }
    assert!(
        !buyer_args
            .iter()
            .any(|arg| arg == "--mock-model" || arg == "--allow-unverified-model"),
        "#10 {label}: real-model buyer proof must not bypass content identity"
    );

    let mut buyer = Command::new(bin)
        .current_dir(WORKSPACE)
        .args(&buyer_args)
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(blog))
        .spawn()
        .unwrap_or_else(|e| panic!("#10 {label}: spawn real-model buyer: {e}"));
    let buyer_lines = stdout_lines(&mut buyer);
    let mut events = Vec::new();
    let ready = loop {
        let line = buyer_lines
            .recv_timeout(Duration::from_secs(900))
            .unwrap_or_else(|e| panic!("#10 {label}: endpoint_ready within 900s: {e}"));
        let event = live10_push_buyer_event(label, line, &mut events);
        assert_ne!(
            event["schema"],
            "dexdo.error.v1",
            "#10 {label}: buyer failed before endpoint_ready; log={}",
            tail(
                &std::fs::read_to_string(buyer_log).unwrap_or_default(),
                4000
            )
        );
        if event["event"] == "endpoint_ready" {
            break event;
        }
    };
    assert!(
        ready["served_models"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(frame_model))),
        "#10 {label}: endpoint_ready did not serve {frame_model}: {ready}"
    );

    let base_url = ready["base_url"].as_str().expect("#10 buyer base_url");
    let chat_url = format!("{base_url}/chat/completions");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap()
        .post(&chat_url)
        .json(&serde_json::json!({
            "model": frame_model,
            "messages": [{"role": "user", "content": format!("directive 10 canonical qwen proof: {label}")}],
            "max_tokens": ticks,
            "stream": false
        }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("#10 {label}: POST /v1/chat/completions: {e}"));
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|e| panic!("#10 {label}: chat body: {e}"));
    eprintln!("#10 {label}: chat status={status} body={body}");

    let drain_deadline =
        Instant::now() + Duration::from_secs(if status.is_success() { 45 } else { 10 });
    while Instant::now() < drain_deadline {
        match buyer_lines.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                let event = live10_push_buyer_event(label, line, &mut events);
                if status.is_success() && event["event"] == "handover_received" {
                    break;
                }
                if !status.is_success() && event["event"] == "purchase_progress" {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if status.is_success()
                    && events
                        .iter()
                        .any(|event| event["event"] == "handover_received")
                {
                    break;
                }
                if !status.is_success()
                    && events
                        .iter()
                        .any(|event| event["event"] == "purchase_progress")
                {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    terminate_child(&mut buyer);
    let shutdown_deadline = Instant::now() + Duration::from_secs(120);
    let mut buyer_status = None;
    while Instant::now() < shutdown_deadline {
        while let Ok(line) = buyer_lines.try_recv() {
            live10_push_buyer_event(label, line, &mut events);
        }
        if let Some(status) = buyer.try_wait().expect("#10 buyer try_wait") {
            buyer_status = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let buyer_status = buyer_status.unwrap_or_else(|| {
        let _ = buyer.kill();
        panic!(
            "#10 {label}: buyer did not exit after SIGTERM; log={}",
            tail(
                &std::fs::read_to_string(buyer_log).unwrap_or_default(),
                4000
            )
        )
    });
    for _ in 0..40 {
        match buyer_lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                live10_push_buyer_event(label, line, &mut events);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(
        buyer_status.success(),
        "#10 {label}: buyer shutdown failed status={buyer_status:?}; log={}",
        tail(
            &std::fs::read_to_string(buyer_log).unwrap_or_default(),
            6000
        )
    );

    Live10BuyerApiResult {
        status,
        body,
        events,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live #10/#311: executable-book, model auto-match, explicit select, stale-row empty view"]
async fn live_10_executable_book_auto_match_and_manual_select() {
    let Some(pool) = live_pool_or_skip("live_10_executable_book_auto_match_and_manual_select", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    let deploy_gas: u128 = ISSUE208_DEPLOY_GAS;
    assert!(
        s_ecc2 >= deploy_gas * 2,
        "seller note needs two TC deploys for #10 live proof, got {s_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let price: u128 = 1;
    let ask_ticks: u128 = 4;
    let buy_ticks: u128 = 2;

    let dir = create_private_temp_dir(&format!("dexdo_live_10_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let s_key = write_key(&dir, "seller.key", &s_sec);
    let b_key = write_key(&dir, "buyer.key", &b_sec);
    let policy_json = serde_json::json!({
        "version": 1,
        "buyer": {
            "on": {
                "no_handover_after_match": "wait_then_reclaim",
                "malformed_handover": "fail_closed",
                "dead_gateway": "retry_then_reclaim",
                "empty_stream": "fail_closed",
                "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                "bad_output_scam": "stop"
            },
            "failover": {
                "max_sellers_to_try": 1,
                "total_spend_cap_shells": 1
            }
        },
        "seller": {
            "on": {
                "after_deal_done": "retire",
                "buyer_no_show": "retire_gateway",
                "dispute_against_me": "release_if_clean"
            },
            "max_open_deals": 1
        }
    });
    let policy_path = write_json_private(&dir, "policy.json", &policy_json);
    let bin = env!("CARGO_BIN_EXE_dexdo");

    for (label, explicit_select) in [("automatch", false), ("manual", true)] {
        let case_nonce = nonce + if explicit_select { 1 } else { 0 };
        let frame_model = FRAME_MODEL.to_string();
        let market = be
            .provision_market(
                &seller_kp,
                &seller_note,
                &frame_model,
                case_nonce,
                price,
                ask_ticks,
                deploy_gas,
            )
            .await
            .unwrap_or_else(|e| panic!("#10 {label}: provision market: {e}"));
        let tc = Address::parse(&market.token_contract).expect("tc addr");
        let tc_arg = tc.with_workchain();
        assert!(wait_active(&be, &tc).await, "#10 {label}: TC active");

        let market_path = dir.join(format!("{label}.market.json"));
        std::fs::write(&market_path, market.to_json().expect("market json")).expect("write market");
        let models_path = Path::new(WORKSPACE).join("models.json");
        assert!(models_path.exists(), "workspace models.json exists");
        let seller_deals_dir = dir.join(format!("{label}-seller-deals"));
        let buyer_deals_dir = dir.join(format!("{label}-buyer-deals"));
        let gateway = free_loopback_addr();
        let seller_log = dir.join(format!("{label}.seller.log"));
        let slog = std::fs::File::create(&seller_log).expect("seller log");
        let mut seller = Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "seller",
                "--note-key",
                s_key.to_str().unwrap(),
                "--note-addr",
                &s_addr,
                "--model",
                "qwen",
                "--models",
                models_path.to_str().unwrap(),
                "--market",
                market_path.to_str().unwrap(),
                "--gateway-listen",
                &gateway,
                "--contracts",
                MANIFEST,
                "--price-per-tick",
                &price.to_string(),
                "--probe-shell",
                "1000000",
                "--deals-dir",
                seller_deals_dir.to_str().unwrap(),
                "--policy",
                policy_path.to_str().unwrap(),
            ])
            .env("RUST_LOG", "info")
            .stdout(std::process::Stdio::from(slog.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(slog))
            .spawn()
            .unwrap_or_else(|e| panic!("#10 {label}: spawn seller: {e}"));

        let book_deadline = Instant::now() + Duration::from_secs(360);
        let book_stdout = loop {
            assert_child_still_running(&mut seller, &seller_log, "#10 seller offer wait");
            let out = Command::new(bin)
                .current_dir(WORKSPACE)
                .args([
                    "executable-book",
                    "--note-addr",
                    &b_addr,
                    "--models",
                    models_path.to_str().unwrap(),
                    "--ticks",
                    &buy_ticks.to_string(),
                    "--max-price-per-tick",
                    &price.to_string(),
                    "--read-timeout-secs",
                    "120",
                    "--contracts",
                    MANIFEST,
                    "qwen",
                ])
                .output()
                .expect("run dexdo executable-book");
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            if out.status.success()
                && stdout.contains("executable_ask ")
                && stdout.contains(&format!("token_contract={tc_arg}"))
                && stdout.contains(&format!("price_per_tick={price}"))
                && stdout.contains(&format!("ticks={ask_ticks}"))
                && stdout.contains(&format!("requested_ticks={buy_ticks}"))
            {
                break stdout;
            }
            let last_book_stdout = if out.status.success() {
                stdout
            } else {
                format!(
                    "stdout:\n{}\nstderr:\n{}",
                    stdout,
                    String::from_utf8_lossy(&out.stderr)
                )
            };
            assert!(
                Instant::now() < book_deadline,
                "#10 {label}: executable-book did not list TC {tc_arg}; last output={}; seller log={}",
                last_book_stdout,
                tail(
                    &std::fs::read_to_string(&seller_log).unwrap_or_default(),
                    4000
                )
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        };
        assert!(book_stdout.contains("executable_ask "), "{book_stdout}");
        assert!(
            book_stdout.contains(&format!("token_contract={tc_arg}")),
            "{book_stdout}"
        );
        assert!(
            book_stdout.contains(&format!("price_per_tick={price}")),
            "{book_stdout}"
        );
        assert!(
            book_stdout.contains(&format!("ticks={ask_ticks}")),
            "{book_stdout}"
        );
        assert!(
            book_stdout.contains(&format!("requested_ticks={buy_ticks}")),
            "{book_stdout}"
        );

        let buyer_log = dir.join(format!("{label}.buyer.log"));
        let buyer_result = run_live10_real_model_buyer_api(
            bin,
            label,
            &b_key,
            &b_addr,
            &frame_model,
            explicit_select.then_some(tc_arg.as_str()),
            buy_ticks,
            price,
            &buyer_deals_dir,
            &policy_path,
            &buyer_log,
        )
        .await;
        assert!(
            buyer_result.status.is_success(),
            "#10 {label}: buyer chat failed status={} body={} log={}",
            buyer_result.status,
            buyer_result.body,
            tail(
                &std::fs::read_to_string(&buyer_log).unwrap_or_default(),
                6000
            )
        );
        assert!(
            buyer_result.body.contains("\"choices\""),
            "{}",
            buyer_result.body
        );
        let event_names = live10_event_names(&buyer_result.events);
        assert!(
            event_names.contains(&"quote_selected"),
            "#10 {label}: missing quote_selected in {event_names:?}"
        );
        assert!(
            event_names.contains(&"buy_submitted"),
            "#10 {label}: missing buy_submitted in {event_names:?}"
        );
        assert!(
            event_names.contains(&"matched"),
            "#10 {label}: missing matched in {event_names:?}"
        );
        assert!(
            event_names.contains(&"handover_received"),
            "#10 {label}: missing handover_received in {event_names:?}"
        );
        assert!(
            event_names.contains(&"settlement_submitted") && event_names.contains(&"settled"),
            "#10 {label}: missing graceful settlement events in {event_names:?}"
        );
        assert!(
            buyer_result
                .events
                .iter()
                .any(|event| event.to_string().contains(&tc_arg)),
            "#10 {label}: buyer events do not mention expected TC {tc_arg}: {:?}",
            buyer_result.events
        );

        let final_state = be
            .token_contract_state(&tc)
            .await
            .expect("read #10 final TC state");
        eprintln!(
            "=== #10 live {label}: listed_tc={} explicit_select={} book_line={} buyer_events={:?} chat_status={} final_state={final_state:?} ===",
            tc_arg,
            explicit_select,
            book_stdout.trim(),
            event_names,
            buyer_result.status
        );
        let _ = seller.kill();
        let _ = seller.wait();
    }

    let frame_model = FRAME_MODEL.to_string();
    let models_path = Path::new(WORKSPACE).join("models.json");
    let low_ceiling = price - 1;
    let low_book_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "executable-book",
                "--note-addr",
                &b_addr,
                "--models",
                models_path.to_str().unwrap(),
                "--ticks",
                &buy_ticks.to_string(),
                "--max-price-per-tick",
                &low_ceiling.to_string(),
                "--read-timeout-secs",
                "120",
                "--contracts",
                MANIFEST,
                "qwen",
            ])
            .output()
            .expect("run low-ceiling executable-book"),
        "dexdo executable-book low ceiling",
    );
    assert!(low_book_stdout.contains("none=true"), "{low_book_stdout}");
    assert!(
        low_book_stdout.contains("no_executable_ask=true"),
        "{low_book_stdout}"
    );

    let negative_deals_dir = dir.join("negative-buyer-deals");
    let (low_stdout, low_stderr) = failed_output(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "buyer",
                "--json",
                "--note-key",
                b_key.to_str().unwrap(),
                "--note-addr",
                &b_addr,
                "--frame-model",
                &frame_model,
                "--models",
                models_path.to_str().unwrap(),
                "--ticks",
                &buy_ticks.to_string(),
                "--max-price-per-tick",
                &low_ceiling.to_string(),
                "--contracts",
                MANIFEST,
                "--max-tokens",
                &buy_ticks.to_string(),
                "--local-listen",
                "127.0.0.1:0",
                "--deals-dir",
                negative_deals_dir.to_str().unwrap(),
                "--policy",
                policy_path.to_str().unwrap(),
            ])
            .output()
            .expect("run low-ceiling buyer"),
        "dexdo buyer low ceiling",
    );
    let low_combined = format!("{low_stdout}\n{low_stderr}");
    assert!(low_combined.contains("no_executable_ask"), "{low_combined}");
    eprintln!(
        "=== #10 live low-ceiling negative: frame_model={FRAME_MODEL} output={} buyer_error={} ===",
        low_book_stdout.trim(),
        low_combined.lines().collect::<Vec<_>>().join(" | ")
    );

    eprintln!(
        "=== #10 live canonical model proof complete: frame_model={FRAME_MODEL} stale/non-executable row safety is covered by offline submit-safe executable-book regressions ==="
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live #316: buyer-abandoned Probe is recovered by seller advance path"]
async fn live_316_seller_advance_recovers_buyer_abandoned_probe() {
    let Some(pool) = live_pool_or_skip("live_316_seller_advance_recovers_buyer_abandoned_probe", 2)
    else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= 20_000_000_000,
        "seller note needs deploy + probe headroom, got {s_ecc2}"
    );

    let seller_addr = Address::parse(&s_addr).expect("seller note addr");
    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller key");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let frame_model = format!("{FRAME_MODEL}-seller-abandon-{nonce}");
    let price: u128 = 10_000;
    let ticks: u128 = 2;
    let escrow = required_escrow_for_buy(ticks, price);

    let market = be
        .provision_market(
            &seller_kp,
            &seller_addr,
            &frame_model,
            nonce,
            price,
            ticks,
            10_000_000_000,
        )
        .await
        .expect("provision market (note-funded)");
    let tc = Address::parse(&market.token_contract).expect("TC addr");
    assert!(
        wait_active(&be, &tc).await,
        "TC active after note-funded provision"
    );
    let tc_arg = tc.with_workchain();

    let (seller_backend, seller_note) = RealSellerBackend::from_provisioned(
        MANIFEST,
        &s_addr,
        &s_sec,
        &frame_model,
        nonce,
        1_000_000,
    )
    .expect("seller backend");
    let (buyer_backend, buyer_note) = RealBuyerBackend::from_provisioned(
        MANIFEST,
        &b_addr,
        &b_sec,
        &frame_model,
        price,
        ticks,
        escrow,
    )
    .expect("buyer backend");

    seller_backend
        .post_offer(
            SellOffer {
                price_per_tick: price as u64,
                max_ticks: ticks as u64,
                token_contract: tc_arg.clone(),
            },
            &seller_note,
        )
        .await
        .expect("post sell offer");
    seller_backend
        .confirm_offer_outcome(&tc_arg)
        .await
        .expect("offer rested");
    buyer_backend
        .place_buy(&tc_arg, &buyer_note)
        .await
        .expect("buyer funds deal");

    let mut matched = None;
    for _ in 0..40 {
        if let Some(m) = seller_backend
            .read_openable_match_now(&tc_arg)
            .await
            .expect("read openable match")
        {
            matched = Some(m);
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let matched = matched.expect("seller observed funded/openable match");
    let endpoint = b"https://seller.example/316|buyer-abandoned";
    let encrypted = seller_note.encrypt_to(&matched.buyer_pubkey, endpoint);
    seller_backend
        .open_stream(&tc_arg, encrypted, &seller_note)
        .await
        .expect("seller opens stream/probe");

    let state_opened = be
        .token_contract_state(&tc)
        .await
        .expect("getState after open")
        .expect("state after open");
    let locks_opened = be
        .note_stream_locks(&seller_addr)
        .await
        .expect("seller getStreamLocks after open")
        .expect("seller locks after open");
    eprintln!(
        "#316 opened buyer-abandoned probe: tc={tc_arg} getState={state_opened:?} seller_locks={locks_opened:?}"
    );
    assert_eq!(state_opened["opened"].as_bool(), Some(true));
    assert_eq!(state_opened["probeAccepted"].as_bool(), Some(false));

    let tick_size = u64::try_from(MODEL_TICK_SIZE).expect("canonical tick size fits u64");
    let delivered = Arc::new(AtomicU64::new(tick_size));
    let done = Arc::new(AtomicBool::new(true));
    let finalized = dexdo::seller::drive_advance(
        &seller_backend,
        &tc_arg,
        &seller_note,
        dexdo::seller::AdvanceWindows::from_settle_window(Duration::from_secs(3)),
        ticks,
        tick_size,
        delivered,
        done,
    )
    .await
    .expect("seller advance recovers buyer-abandoned probe");
    assert!(finalized >= 1);

    let state_after = be
        .token_contract_state(&tc)
        .await
        .expect("getState after seller advance")
        .expect("state after seller advance");
    let locks_after = be
        .note_stream_locks(&seller_addr)
        .await
        .expect("seller getStreamLocks after advance")
        .expect("seller locks after advance");
    eprintln!(
        "#316 after seller advance: finalized={finalized} getState={state_after:?} seller_locks={locks_after:?}"
    );
    assert_eq!(state_after["probeAccepted"].as_bool(), Some(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live PR233: postProbeCommission -> probeFunded=true -> open -> opened=true"]
async fn live_probe_commission_then_open_by_fact() {
    let Some(pool) = live_pool_or_skip("live_probe_commission_then_open_by_fact", 2) else {
        return;
    };
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= 80_001_000_000,
        "seller note needs deploy + probe headroom, got {s_ecc2}"
    );

    let seller_addr = Address::parse(&s_addr).expect("seller note addr");
    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller key");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let frame_model = format!("{FRAME_MODEL}-pr233-probe-open-{nonce}");
    let price: u128 = 10_000;
    let ticks: u128 = 2;
    let max_ticks: u128 = ticks;
    let escrow = required_escrow_for_buy(ticks, price);

    let market = be
        .provision_market(
            &seller_kp,
            &seller_addr,
            &frame_model,
            nonce,
            price,
            max_ticks,
            40_000_000_000,
        )
        .await
        .expect("provision market (note-funded)");
    let tc = Address::parse(&market.token_contract).expect("TC addr");
    assert!(
        wait_active(&be, &tc).await,
        "TC active after note-funded provision"
    );
    let tc_arg = tc.with_workchain();

    let (seller_backend, seller_note) = RealSellerBackend::from_provisioned(
        MANIFEST,
        &s_addr,
        &s_sec,
        &frame_model,
        nonce,
        1_000_000,
    )
    .expect("seller backend");
    let (buyer_backend, buyer_note) = RealBuyerBackend::from_provisioned(
        MANIFEST,
        &b_addr,
        &b_sec,
        &frame_model,
        price,
        ticks,
        escrow,
    )
    .expect("buyer backend");

    seller_backend
        .post_offer(
            SellOffer {
                price_per_tick: price as u64,
                max_ticks: max_ticks as u64,
                token_contract: tc_arg.clone(),
            },
            &seller_note,
        )
        .await
        .expect("post sell offer");
    seller_backend
        .confirm_offer_outcome(&tc_arg)
        .await
        .expect("offer rested");
    buyer_backend
        .place_buy(&tc_arg, &buyer_note)
        .await
        .expect("place buy");
    let mut matched = None;
    for _ in 0..40 {
        if let Some(m) = seller_backend
            .read_openable_match_now(&tc_arg)
            .await
            .expect("read openable match")
        {
            matched = Some(m);
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let matched = matched.expect("seller observed funded/openable match");

    let state_before = be
        .token_contract_state(&tc)
        .await
        .expect("getState before open")
        .expect("state before open");
    let probe_before = be
        .token_contract_probe(&tc)
        .await
        .expect("getProbe before open")
        .expect("probe before open");
    assert_eq!(state_before["funded"].as_bool(), Some(true));
    assert_eq!(state_before["opened"].as_bool(), Some(false));
    assert_eq!(probe_before["probeFunded"].as_bool(), Some(false));
    assert!(
        probe_before["probeCommission"]
            .as_str()
            .and_then(|raw| raw.parse::<u128>().ok())
            .unwrap_or(0)
            > 0,
        "probeCommission must be explicit and non-zero: {probe_before:?}"
    );
    eprintln!(
        "PR233 before postProbeCommission: tc={tc_arg} getState={state_before:?} getProbe={probe_before:?}"
    );

    let endpoint = b"https://seller.example/pr233|probe-open";
    let encrypted = seller_note.encrypt_to(&matched.buyer_pubkey, endpoint);
    seller_backend
        .open_stream(&tc_arg, encrypted, &seller_note)
        .await
        .expect("postProbeCommission then open_stream");

    let probe_after = be
        .token_contract_probe(&tc)
        .await
        .expect("getProbe after open")
        .expect("probe after open");
    let state_after = be
        .token_contract_state(&tc)
        .await
        .expect("getState after open")
        .expect("state after open");
    eprintln!(
        "PR233 live evidence: postProbeCommission -> getProbe={probe_after:?}; TokenContract.open -> getState={state_after:?}"
    );
    assert_eq!(probe_after["probeFunded"].as_bool(), Some(true));
    assert_eq!(state_after["opened"].as_bool(), Some(true));

    eprintln!("PR233 live cleanup: waiting 185s for probe acceptance window");
    tokio::time::sleep(Duration::from_secs(185)).await;
    seller_backend
        .accept_probe(&tc_arg)
        .await
        .expect("accept probe before cleanup stop");
    let state_accepted = be
        .token_contract_state(&tc)
        .await
        .expect("getState after accept_probe")
        .expect("state after accept_probe");
    eprintln!("PR233 after accept_probe: getState={state_accepted:?}");
    assert_eq!(state_accepted["probeAccepted"].as_bool(), Some(true));

    let settlement = buyer_backend
        .stop(&tc_arg, &buyer_note)
        .await
        .expect("cleanup stop opened deal");
    assert!(
        matches!(settlement, Settlement::AmicableSplit { .. }),
        "cleanup must be AmicableSplit, got {settlement:?}"
    );
    let post_stop = be
        .token_contract_state(&tc)
        .await
        .expect("getState after cleanup stop")
        .expect("state after cleanup stop");
    eprintln!("PR233 cleanup: settlement={settlement:?}; post_stop={post_stop:?}");
}

async fn run_live_cli_deal_flow_handover(late_buyer_delay_secs: u64) {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set — skipping (minted notes pn_pool.json are required)");
        return;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes[]");
    assert!(notes.len() >= 2, "need ≥2 minted notes (seller+buyer)");
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let mut role_notes = Vec::new();
    for note in notes {
        let addr = note["address"].as_str().expect("note addr").to_string();
        let secret = note["owner_secret_key_hex"]
            .as_str()
            .expect("note secret")
            .to_string();
        let address = Address::parse(&addr).expect("note addr parses");
        let ecc2 = ecc_shell_balance(&be, &address).await;
        role_notes.push((addr, secret, ecc2));
    }
    role_notes.sort_by(|a, b| b.2.cmp(&a.2));
    let (s_addr, s_sec, s_ecc2) = role_notes[0].clone();
    let (b_addr, b_sec, _b_ecc2) = role_notes[1].clone();
    assert!(
        s_ecc2 >= 80_000_000_000,
        "seller note needs deploy headroom, got {s_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&s_sec).expect("seller kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // A UNIQUE frame_model per run → a FRESH (empty) OB (it is keyed by model_hash=sha256(frame_model))
    // → the buy matches ONLY our offer, not stale asks from previous/killed runs. tick_size=MODEL_TICK_SIZE.
    let scenario = if late_buyer_delay_secs > 0 {
        "late"
    } else {
        "live"
    };
    let unique_frame = format!("{FRAME_MODEL}-{scenario}-{nonce}");
    let price: u128 = 10_000;
    // offer.maxTicks (CLI SellerConfig=1024) must match TC.maxTicks (otherwise the match/funding does not reconcile —
    // in the working in-process setup_funded_deal they are equal). We deploy the TC with 1024.
    let max_ticks: u128 = 1024;

    // ── Provisioning (note-funded, #58): OB + RootModel + per-deal TokenContract, ALL from the seller note's
    //    own ECC[2] (`fundDeployShell`) — no operator wallet, no giver in the operate path. The note pre-funds the
    //    RootModel/TC uninit deploy addresses and the external seller-signed deploys activate them, exactly as the
    //    `dexdo provision` command does. `--gas` per uninit address: 4·10¹⁰ SHELL (two of them ≤ the note's ECC).
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &unique_frame,
            nonce,
            price,
            max_ticks,
            40_000_000_000,
        )
        .await
        .expect("provision market (note-funded)");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    let nonce_arg = nonce.to_string();
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    // ── Key files for the CLI (local, not committed) ──────────────────────────────────────────
    let dir = create_private_temp_dir(&format!("dexdo_live_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let s_key = write_key(&dir, "seller.key", &s_sec);
    let b_key = write_key(&dir, "buyer.key", &b_sec);
    let policy_path = dir.join("policy.json");
    std::fs::write(
        &policy_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "buyer": {
                "on": {
                    "no_handover_after_match": "wait_then_reclaim",
                    "malformed_handover": "fail_closed",
                    "dead_gateway": "retry_then_reclaim",
                    "empty_stream": "fail_closed",
                    "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                    "bad_output_scam": "stop"
                },
                "failover": {
                    "max_sellers_to_try": 1,
                    "total_spend_cap_shells": 1
                }
            },
            "seller": {
                "on": {
                    "after_deal_done": "retire",
                    "buyer_no_show": "retire_gateway",
                    "dispute_against_me": "release_if_clean"
                },
                "max_open_deals": 1
            }
        }))
        .expect("policy serializes"),
    )
    .expect("write policy");
    // Temporary models.json: `--model livetest` → unique_frame. The upstream is mocked (`--mock-model`), so
    // base_url/api_key are stubs (only frame_model matters, for model_hash).
    let models_path = dir.join("models.json");
    std::fs::write(
        &models_path,
        format!(
            r#"{{"models":{{"livetest":{{"frame_model":"{unique_frame}","base_url":"http://localhost:1","served_model":"x","api_key_env":"NONE_LIVE","tokenizer_family":"qwen","price_per_tick":10000,"capabilities":{{"logprobs":false,"top_logprobs":0}}}}}}}}"#
        ),
    )
    .expect("write temp models.json");

    // ── Two CLI processes against real shellnet (logs → files; the test tracks on-chain state itself) ────────
    let bin = env!("CARGO_BIN_EXE_dexdo");
    let gateway = "127.0.0.1:18443";
    let seller_log = dir.join("seller.log");
    let buyer_log = dir.join("buyer.log");

    // Seller: mock upstream (fake tokens — we are testing the CHAIN), real chain. Posts an offer, waits for a match,
    // on the match reconstructs the buyer's pubkey from chain (F1) and opens the stream (writes the encrypted endpoint).
    let slog = std::fs::File::create(&seller_log).expect("seller log");
    let mut seller = Command::new(bin)
        .current_dir(WORKSPACE)
        .args([
            "seller",
            "--mock-model",
            "--note-key",
            s_key.to_str().unwrap(),
            "--note-addr",
            &s_addr,
            "--model",
            "livetest",
            "--models",
            models_path.to_str().unwrap(),
            "--token-contract",
            &tc_arg,
            "--nonce",
            &nonce_arg,
            "--gateway-listen",
            gateway,
            "--contracts",
            MANIFEST,
            "--price-per-tick",
            &price.to_string(),
            "--probe-shell",
            "1000000",
            "--mock-token-count",
            "2",
            "--policy",
            policy_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::from(slog.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(slog))
        .spawn()
        .expect("spawn seller");

    // The offer rested in the book and the gateway is listening — then the buyer's match will fire.
    let (mut offer_rested, mut gateway_ready) = (false, false);
    for _ in 0..30 {
        assert_child_still_running(&mut seller, &seller_log, "seller readiness wait");
        let oc = be
            .inference_orderbook_stats(&ob)
            .await
            .ok()
            .flatten()
            .and_then(|s| {
                s["orderCount"]
                    .as_str()
                    .and_then(|x| x.parse::<u128>().ok())
            })
            .unwrap_or(0);
        if oc >= 1 {
            offer_rested = true;
        }
        gateway_ready |= gateway_accepting(gateway);
        if offer_rested && gateway_ready {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    eprintln!("offer_rested={offer_rested} gateway_ready={gateway_ready}");

    if late_buyer_delay_secs > 0 {
        eprintln!(
            "late_buyer_delay_secs={late_buyer_delay_secs}: keeping seller alive before buyer spawn"
        );
        let delay = Duration::from_secs(late_buyer_delay_secs);
        let start = Instant::now();
        while start.elapsed() < delay {
            assert_child_still_running(&mut seller, &seller_log, "seller late-buyer wait");
            let remaining = delay.saturating_sub(start.elapsed());
            tokio::time::sleep(remaining.min(Duration::from_secs(3))).await;
        }
        assert_child_still_running(&mut seller, &seller_log, "seller after late-buyer wait");
        eprintln!("seller survived late buyer delay; spawning buyer");
    }

    // Buyer: place_buy (the match funds the TC) → resolve_endpoint (handover DECRYPTION = F1) →
    // stream → STOP. We spawn it (no blocking) — the test tracks the deal's progress on-chain itself.
    let blog = std::fs::File::create(&buyer_log).expect("buyer log");
    let mut buyer = Command::new(bin)
        .current_dir(WORKSPACE)
        .args([
            // max_price_per_tick = default 1_000_000 (≥ ask 10000); --escrow omitted → the computed default
            // = exactly ticks 2 × 1M × (1 + 2.5% fee) = 2_050_000 (issue #20/#116: no over-funding).
            "buyer",
            "--mock-model",
            "--note-key",
            b_key.to_str().unwrap(),
            "--note-addr",
            &b_addr,
            "--frame-model",
            &unique_frame,
            "--token-contract",
            &tc_arg,
            "--ticks",
            "2",
            "--contracts",
            MANIFEST,
            "--max-tokens",
            "2",
            "--policy",
            policy_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::from(blog.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(blog))
        .spawn()
        .expect("spawn buyer");

    // DIAGNOSTICS: the test tracks the TC's on-chain state (funded → opened) + the buyer's completion.
    let (mut funded, mut opened) = (false, false);
    let mut buyer_status = None;
    for i in 0..90 {
        if let Ok(Some(st)) = be.token_contract_state(&tc).await {
            let f = st["funded"].as_bool().unwrap_or(false);
            let o = st["opened"].as_bool().unwrap_or(false);
            if f && !funded {
                funded = true;
                eprintln!("[{i}] TC funded=true (place_buy crossed with the offer)");
            }
            if o && !opened {
                opened = true;
                eprintln!(
                    "[{i}] TC opened=true (seller opened the stream — handover written, F1 path)"
                );
            }
        }
        // First iterations: the book's state — whether the buy crossed with the ask (match diagnostics).
        if i < 6 {
            let oc = be
                .inference_orderbook_stats(&ob)
                .await
                .ok()
                .flatten()
                .and_then(|s| s["orderCount"].as_str().map(String::from));
            let bba = be
                .inference_orderbook_best_bid_ask(&ob)
                .await
                .ok()
                .flatten();
            eprintln!("[{i}] OB orderCount={oc:?} bestBidAsk={bba:?}");
        }
        match buyer.try_wait() {
            Ok(Some(st)) => {
                buyer_status = Some(st);
                eprintln!("[{i}] buyer finished: success={}", st.success());
                break;
            }
            Ok(None) => {}
            Err(e) => eprintln!("[{i}] buyer try_wait err: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let _ = buyer.kill();
    let _ = buyer.wait();
    let _ = seller.kill();
    let _ = seller.wait();
    let s_txt = std::fs::read_to_string(&seller_log).unwrap_or_default();
    let b_txt = std::fs::read_to_string(&buyer_log).unwrap_or_default();
    eprintln!(
        "=== TC state: funded={funded} opened={opened}; buyer_done={} ===",
        buyer_status.is_some()
    );
    eprintln!("--- seller.log (tail) ---\n{}", tail(&s_txt, 3000));
    eprintln!("--- buyer.log (tail) ---\n{}", tail(&b_txt, 3000));

    // Staged asserts. `opened` is INFORMATIONAL (the on-chain `opened` flag is transient: open→stream→STOP
    // may fit between the 3-sec polls, especially when the stream is short). The DEFINITIVE proof of
    // F1 is that the buyer ACTUALLY received tokens: for that it had to decrypt the handover
    // (the pubkey reconstructed by the seller from chain), pass challenge-response, and open the stream.
    assert!(
        offer_rested,
        "CLI seller posted the offer to the book (orderCount>=1)"
    );
    assert!(gateway_ready, "CLI seller gateway accepted connections");
    assert!(
        funded,
        "the match funded the TC (place_buy crossed with the offer)"
    );
    let status = buyer_status.expect("buyer finished within the deadline (did not hang)");
    assert!(
        status.success(),
        "buyer completed the deal via CLI: handover DECRYPT (F1) + challenge-response + stream + STOP"
    );
    assert!(
        b_txt.contains("received"),
        "buyer received tokens after the decrypted handover — F1 proven live via per-role CLI"
    );
    if late_buyer_delay_secs > 0 {
        assert!(
            opened || b_txt.contains("received"),
            "#198 late-buyer proof: seller watcher stayed alive after the delayed buyer and the handover/stream path completed"
        );
    }
    if !opened {
        eprintln!("(opened=false in the polls — the transient window was missed; the buyer's token receipt covers this)");
    }

    // F3 (review): prove the SETTLEMENT, not just token delivery. After the buyer exits we re-read
    // the TC FROM CHAIN and check the POST-STOP state: the deal is CLOSED (opened=false), not hanging with funds
    // locked. This is by-fact (not the transient `opened` from polls): the one-shot CLI buyer calls chain.stop,
    // and RealBuyerBackend.stop waits for opened=false BEFORE exiting — meaning the settlement is applied by this point.
    let post = be
        .token_contract_state(&tc)
        .await
        .expect("getState after STOP")
        .expect("TC active after STOP");
    eprintln!(
        "post-STOP getState: opened={:?} probeAccepted={:?} finalizedOwed={:?} frozen={:?} deposit={:?}",
        post["opened"], post["probeAccepted"], post["finalizedOwed"], post["frozen"], post["deposit"]
    );
    assert_eq!(
        post["opened"].as_bool(),
        Some(false),
        "post-STOP: the deal is CLOSED (opened=false) — funds settled, not locked (F3, issue #18)"
    );
}

/// #157/#158 §8 (LIVE — AGENTS.md §8 merge gate): real market read/quote + own order cancel.
///
/// Run:
///   export DEXDO_PN_POOL=$PWD/pn_pool.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_cli_market_quote_and_orders_cancel -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live shellnet; reads a real book, places a resting buy, then cancels it"]
async fn live_cli_market_quote_and_orders_cancel() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set — skipping (minted notes pn_pool.json are required)");
        return;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes[]");
    assert!(notes.len() >= 2, "need >=2 minted notes (seller+buyer)");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s sec");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("b sec");
    let s_addr = notes[0]["address"].as_str().expect("s addr").to_string();
    let b_addr = notes[1]["address"].as_str().expect("b addr").to_string();

    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let seller_kp = KeyPair::from_secret_hex(s_sec).expect("seller kp");
    let buyer_kp = KeyPair::from_secret_hex(b_sec).expect("buyer kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let buyer_note = Address::parse(&b_addr).expect("buyer note addr");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}-book-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 1024;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &unique_frame,
            nonce,
            price,
            max_ticks,
            40_000_000_000,
        )
        .await
        .expect("provision market (note-funded)");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    let dir = create_private_temp_dir(&format!("dexdo_book_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let s_key = write_key(&dir, "seller.key", s_sec);
    let b_key = write_key(&dir, "buyer.key", b_sec);
    let market_path = dir.join("market.json");
    std::fs::write(&market_path, manifest.to_json().expect("market json")).expect("write market");
    let models_path = dir.join("models.json");
    std::fs::write(
        &models_path,
        format!(
            r#"{{"models":{{"livetest":{{"frame_model":"{unique_frame}","base_url":"http://localhost:1","served_model":"x","api_key_env":"NONE_LIVE","tokenizer_family":"qwen","price_per_tick":10000,"capabilities":{{"logprobs":false,"top_logprobs":0}}}}}}}}"#
        ),
    )
    .expect("write temp models.json");

    let bin = env!("CARGO_BIN_EXE_dexdo");
    let seller_log = dir.join("seller.log");
    let slog = std::fs::File::create(&seller_log).expect("seller log");
    let mut seller = Command::new(bin)
        .current_dir(WORKSPACE)
        .args([
            "seller",
            "--mock-model",
            "--note-key",
            s_key.to_str().unwrap(),
            "--note-addr",
            &s_addr,
            "--model",
            "livetest",
            "--models",
            models_path.to_str().unwrap(),
            "--market",
            market_path.to_str().unwrap(),
            "--gateway-listen",
            "127.0.0.1:0",
            "--contracts",
            MANIFEST,
            "--price-per-tick",
            &price.to_string(),
            "--probe-shell",
            "1000000",
            "--mock-token-count",
            "2",
        ])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::from(slog.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(slog))
        .spawn()
        .expect("spawn seller");

    let want_tc = tc_arg.to_ascii_lowercase();
    let mut ask_order_id = None;
    for _ in 0..40 {
        if let Ok(Some(status)) = seller.try_wait() {
            panic!(
                "seller exited before ask rested: success={}\n{}",
                status.success(),
                std::fs::read_to_string(&seller_log).unwrap_or_default()
            );
        }
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("read book snapshot");
        ask_order_id = snapshot
            .resting_asks()
            .find(|o| {
                o.token_contract
                    .as_deref()
                    .map(|tc| tc.eq_ignore_ascii_case(&want_tc))
                    .unwrap_or(false)
            })
            .map(|o| o.order_id);
        if ask_order_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let ask_order_id = ask_order_id.expect("seller ask rested in real order book");
    println!("=== #157 live ask order_id={ask_order_id} tc={tc_arg} ===");

    let market_s = market_path.to_str().unwrap();
    let markets_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args(["markets", "--market", market_s, "--contracts", MANIFEST])
            .output()
            .expect("run dexdo markets"),
        "dexdo markets",
    );
    println!("--- markets stdout ---\n{markets_stdout}");
    assert!(markets_stdout.contains("active=true"), "{markets_stdout}");
    assert!(markets_stdout.contains("ask_count=1"), "{markets_stdout}");
    assert!(
        markets_stdout.contains("best_ask=10000"),
        "{markets_stdout}"
    );

    let quote_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "quote",
                "--market",
                market_s,
                "--ticks",
                "2",
                "--contracts",
                MANIFEST,
            ])
            .output()
            .expect("run dexdo quote"),
        "dexdo quote",
    );
    println!("--- quote stdout ---\n{quote_stdout}");
    let expected_quote = required_escrow_for_buy(2, price);
    assert!(quote_stdout.contains("filled_ticks=2"), "{quote_stdout}");
    assert!(
        quote_stdout.contains(&format!("total_with_fee={expected_quote}")),
        "{quote_stdout}"
    );
    assert!(
        quote_stdout.contains(&format!("order_id={ask_order_id}")),
        "{quote_stdout}"
    );

    let bid_price = price - 1;
    let bid_ticks = 3;
    let bid_escrow = required_escrow_for_buy(bid_ticks, bid_price);
    be.place_inference_buy(
        &buyer_note,
        &buyer_kp,
        &manifest.model_hash,
        bid_price,
        bid_ticks,
        bid_escrow,
        0,
        0,
    )
    .await
    .expect("place resting maker buy");

    let mut buy_order_id = None;
    for _ in 0..30 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("read book snapshot after bid");
        buy_order_id = snapshot
            .orders
            .iter()
            .find(|o| {
                o.is_buy
                    && o.owner_note.eq_ignore_ascii_case(&b_addr)
                    && o.price_per_tick == bid_price
                    && o.ticks == bid_ticks
                    && o.escrow == bid_escrow
            })
            .map(|o| o.order_id);
        if buy_order_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let buy_order_id = buy_order_id.expect("resting buy order visible in real book");
    println!(
        "=== #158 live maker buy order_id={buy_order_id} escrow={bid_escrow} rested in real book ==="
    );

    let b_key_s = b_key.to_str().unwrap();
    let orders_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "orders",
                "--note-addr",
                &b_addr,
                "--market",
                market_s,
                "--contracts",
                MANIFEST,
                "list",
            ])
            .output()
            .expect("run dexdo orders list"),
        "dexdo orders list",
    );
    println!("--- orders list stdout ---\n{orders_stdout}");
    assert_eq!(first_order_id(&orders_stdout), buy_order_id);
    assert!(orders_stdout.contains("side=buy"), "{orders_stdout}");
    assert!(
        orders_stdout.contains(&format!("escrow={bid_escrow}")),
        "{orders_stdout}"
    );

    let cancel_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "orders",
                "--note-addr",
                &b_addr,
                "--note-key",
                b_key_s,
                "--market",
                market_s,
                "--contracts",
                MANIFEST,
                "cancel",
                &buy_order_id.to_string(),
            ])
            .output()
            .expect("run dexdo orders cancel"),
        "dexdo orders cancel",
    );
    println!("--- orders cancel stdout ---\n{cancel_stdout}");
    assert!(
        cancel_stdout.contains(&format!("order_id={buy_order_id}")),
        "{cancel_stdout}"
    );

    for _ in 0..30 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("read book snapshot after cancel");
        if snapshot.orders.iter().all(|o| o.order_id != buy_order_id) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let after_orders_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "orders",
                "--note-addr",
                &b_addr,
                "--market",
                market_s,
                "--contracts",
                MANIFEST,
                "list",
            ])
            .output()
            .expect("run dexdo orders list after cancel"),
        "dexdo orders list after cancel",
    );
    assert!(
        after_orders_stdout.contains("none=true"),
        "{after_orders_stdout}"
    );
    println!(
        "=== #157/#158 §8 PASS: quote real ask + list/cancel real maker buy; order removed after cancel ==="
    );

    let _ = seller.kill();
    let _ = seller.wait();
}

/// #26 §8 (LIVE — AGENTS.md §8 merge gate): OracleEventList range PMP from an inference OB.
///
/// This is intentionally end-to-end through the CLI for the #26 surface:
///   1. fresh inference market + real crossing buy, so `getWeeklyMedianPrice` has MIN_LIQUIDITY;
///   2. `dexdo oracle provision` deploys/approves OracleEventList + PMP;
///   3. after the contract result gap, `dexdo oracle resolve` resolves the PMP from the OB median.
///
/// Run:
///   export DEXDO_PN_POOL=$PWD/pn_pool.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_cli_oracle_range_pmp_resolves_from_inference_book -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live shellnet; provisions OracleEventList + PMP and waits for range resolve"]
async fn live_cli_oracle_range_pmp_resolves_from_inference_book() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set — skipping (minted notes pn_pool.json are required)");
        return;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes[]");
    assert!(notes.len() >= 2, "need >=2 minted notes (seller+buyer)");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s sec");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("b sec");
    let s_addr = notes[0]["address"].as_str().expect("s addr").to_string();
    let b_addr = notes[1]["address"].as_str().expect("b addr").to_string();

    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let seller_kp = KeyPair::from_secret_hex(s_sec).expect("seller kp");
    let buyer_kp = KeyPair::from_secret_hex(b_sec).expect("buyer kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");
    let buyer_note = Address::parse(&b_addr).expect("buyer note addr");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}-oracle-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 1024;
    let fill_ticks: u128 = 2;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &unique_frame,
            nonce,
            price,
            max_ticks,
            40_000_000_000,
        )
        .await
        .expect("provision market (note-funded)");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post sell offer");

    let want_tc = tc_arg.to_ascii_lowercase();
    let mut ask_order_id = None;
    for _ in 0..40 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("read book snapshot");
        ask_order_id = snapshot
            .resting_asks()
            .find(|o| {
                o.token_contract
                    .as_deref()
                    .map(|tc| tc.eq_ignore_ascii_case(&want_tc))
                    .unwrap_or(false)
            })
            .map(|o| o.order_id);
        if ask_order_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let ask_order_id = ask_order_id.expect("seller ask rested in real order book");
    println!("=== #26 live seller ask order_id={ask_order_id} tc={tc_arg} ===");

    let fill_escrow = required_escrow_for_buy(fill_ticks, price);
    be.place_inference_buy(
        &buyer_note,
        &buyer_kp,
        &manifest.model_hash,
        price,
        fill_ticks,
        fill_escrow,
        0,
        0,
    )
    .await
    .expect("place crossing buy");

    let mut filled_by_stats = false;
    for _ in 0..40 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("read book snapshot after crossing buy");
        if snapshot
            .stats
            .as_ref()
            .map(|s| s.executed_ticks >= fill_ticks)
            .unwrap_or(false)
        {
            filled_by_stats = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(
        filled_by_stats,
        "InferenceOrderBook stats did not record the crossing fill"
    );
    let mut median = None;
    for _ in 0..40 {
        match be.inference_orderbook_weekly_median_price(&ob).await {
            Ok(Some(price)) => {
                median = Some(price);
                break;
            }
            Ok(None) => {}
            Err(e) => eprintln!("weekly median not ready yet: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let median = median.expect("weekly median did not become available after the live fill");
    assert_eq!(median, price, "weekly median follows the live fill price");
    println!("=== #26 live inference OB median={median} after fill_ticks={fill_ticks} ===");

    let dir = create_private_temp_dir(&format!("dexdo_oracle_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let note_key = write_key(&dir, "seller-note.key", s_sec);
    let oracle_keys = KeyPair::generate();
    let oracle_key = write_key(&dir, "oracle.key", oracle_keys.secret_hex());
    let market_path = dir.join("market.json");
    std::fs::write(&market_path, manifest.to_json().expect("market json")).expect("write market");
    let oracle_market_path = dir.join("oracle-market.json");

    let bin = env!("CARGO_BIN_EXE_dexdo");
    let deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 300;
    let bound = (price + 1).to_string();
    let oracle_name = format!("dexdo26-{nonce}");
    let event_name = format!("weekly-{nonce}");

    let provision_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "oracle",
                "provision",
                "--note-key",
                note_key.to_str().unwrap(),
                "--note-addr",
                &s_addr,
                "--oracle-key",
                oracle_key.to_str().unwrap(),
                "--oracle-name",
                &oracle_name,
                "--event-list-description",
                "dexdo #26 live range list",
                "--market",
                market_path.to_str().unwrap(),
                "--event-name",
                &event_name,
                "--deadline",
                &deadline.to_string(),
                "--describe",
                "dexdo #26 live weekly median range",
                "--bound",
                &bound,
                "--outcome",
                "below-or-at-fill",
                "--outcome",
                "above-fill",
                "--initial-stake",
                "10000000",
                "--initial-stake",
                "10000000",
                "--contracts",
                MANIFEST,
                "--output",
                oracle_market_path.to_str().unwrap(),
            ])
            .output()
            .expect("run dexdo oracle provision"),
        "dexdo oracle provision",
    );
    println!("--- oracle provision stdout ---\n{provision_stdout}");
    assert!(
        provision_stdout.contains("oracle market provisioned"),
        "{provision_stdout}"
    );

    let state_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "oracle",
                "state",
                "--manifest",
                oracle_market_path.to_str().unwrap(),
                "--contracts",
                MANIFEST,
            ])
            .output()
            .expect("run dexdo oracle state"),
        "dexdo oracle state",
    );
    println!("--- oracle state stdout ---\n{state_stdout}");
    assert!(state_stdout.contains("approved=true"), "{state_stdout}");
    assert!(
        state_stdout.contains("resolved_outcome=none"),
        "{state_stdout}"
    );

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let wait_secs = deadline.saturating_sub(now).saturating_add(8);
    println!("=== #26 waiting {wait_secs}s for deadline {deadline} before resolve ===");
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;

    let resolve_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "oracle",
                "resolve",
                "--manifest",
                oracle_market_path.to_str().unwrap(),
                "--oracle-key",
                oracle_key.to_str().unwrap(),
                "--contracts",
                MANIFEST,
            ])
            .output()
            .expect("run dexdo oracle resolve"),
        "dexdo oracle resolve",
    );
    println!("--- oracle resolve stdout ---\n{resolve_stdout}");
    assert!(resolve_stdout.contains("pmp resolved"), "{resolve_stdout}");
    assert!(resolve_stdout.contains("outcome=0"), "{resolve_stdout}");
    println!("=== #26 §8 PASS: range PMP resolved from live inference OB median ===");
}

/// #26 §8 negative live gate: a PMP bound to an inference order book with no matched volume must NOT fake-resolve.
///
/// Run:
///   export DEXDO_PN_POOL=$PWD/pn_pool.json
///   cargo test -p dexdo --features shellnet --test live_cli \
///     live_cli_oracle_range_pmp_no_liquidity_stays_unresolved -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live shellnet; provisions OracleEventList + PMP and waits for no-liquidity resolve failure"]
async fn live_cli_oracle_range_pmp_no_liquidity_stays_unresolved() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set — skipping (minted notes pn_pool.json are required)");
        return;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes[]");
    assert!(!notes.is_empty(), "need >=1 minted note");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s sec");
    let s_addr = notes[0]["address"].as_str().expect("s addr").to_string();

    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let seller_kp = KeyPair::from_secret_hex(s_sec).expect("seller kp");
    let seller_note = Address::parse(&s_addr).expect("seller note addr");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let unique_frame = format!("{FRAME_MODEL}-oracle-noliquidity-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 1024;
    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &unique_frame,
            nonce,
            price,
            max_ticks,
            40_000_000_000,
        )
        .await
        .expect("provision market (note-funded)");
    let ob = Address::parse(&manifest.inference_order_book).expect("ob addr");
    let tc = Address::parse(&manifest.token_contract).expect("tc addr");
    let tc_arg = tc.with_workchain();
    assert!(
        wait_active(&be, &tc).await,
        "TC active (note-funded deploy)"
    );

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post sell offer");

    let want_tc = tc_arg.to_ascii_lowercase();
    let mut ask_rested = false;
    for _ in 0..40 {
        let snapshot = be
            .inference_orderbook_snapshot(&ob, &unique_frame, &manifest.model_hash)
            .await
            .expect("read book snapshot");
        ask_rested = snapshot.resting_asks().any(|o| {
            o.token_contract
                .as_deref()
                .map(|tc| tc.eq_ignore_ascii_case(&want_tc))
                .unwrap_or(false)
        });
        if ask_rested {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(ask_rested, "seller ask rested in real order book");
    println!(
        "=== #26 no-liquidity live ask rested tc={tc_arg}; no crossing buy will be placed ==="
    );

    let dir = create_private_temp_dir(&format!("dexdo_oracle_noliquidity_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let note_key = write_key(&dir, "seller-note.key", s_sec);
    let oracle_keys = KeyPair::generate();
    let oracle_key = write_key(&dir, "oracle.key", oracle_keys.secret_hex());
    let market_path = dir.join("market.json");
    std::fs::write(&market_path, manifest.to_json().expect("market json")).expect("write market");
    let oracle_market_path = dir.join("oracle-market.json");

    let bin = env!("CARGO_BIN_EXE_dexdo");
    let deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 140;
    let bound = (price + 1).to_string();
    let oracle_name = format!("dexdo26-noliquidity-{nonce}");
    let event_name = format!("weekly-noliquidity-{nonce}");

    let provision_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "oracle",
                "provision",
                "--note-key",
                note_key.to_str().unwrap(),
                "--note-addr",
                &s_addr,
                "--oracle-key",
                oracle_key.to_str().unwrap(),
                "--oracle-name",
                &oracle_name,
                "--event-list-description",
                "dexdo #26 no-liquidity range list",
                "--market",
                market_path.to_str().unwrap(),
                "--event-name",
                &event_name,
                "--deadline",
                &deadline.to_string(),
                "--describe",
                "dexdo #26 no-liquidity weekly median range",
                "--bound",
                &bound,
                "--outcome",
                "below-or-at-fill",
                "--outcome",
                "above-fill",
                "--initial-stake",
                "10000000",
                "--initial-stake",
                "10000000",
                "--contracts",
                MANIFEST,
                "--output",
                oracle_market_path.to_str().unwrap(),
            ])
            .output()
            .expect("run dexdo oracle provision"),
        "dexdo oracle provision no-liquidity",
    );
    println!("--- no-liquidity oracle provision stdout ---\n{provision_stdout}");

    let state_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "oracle",
                "state",
                "--manifest",
                oracle_market_path.to_str().unwrap(),
                "--contracts",
                MANIFEST,
            ])
            .output()
            .expect("run dexdo oracle state"),
        "dexdo oracle state no-liquidity",
    );
    println!("--- no-liquidity oracle state stdout ---\n{state_stdout}");
    assert!(state_stdout.contains("approved=true"), "{state_stdout}");
    assert!(
        state_stdout.contains("resolved_outcome=none"),
        "{state_stdout}"
    );

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let wait_secs = deadline.saturating_sub(now).saturating_add(8);
    println!(
        "=== #26 no-liquidity waiting {wait_secs}s for deadline {deadline} before resolve ==="
    );
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;

    let (resolve_stdout, resolve_stderr) = failed_output(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "oracle",
                "resolve",
                "--manifest",
                oracle_market_path.to_str().unwrap(),
                "--oracle-key",
                oracle_key.to_str().unwrap(),
                "--contracts",
                MANIFEST,
            ])
            .output()
            .expect("run dexdo oracle resolve no-liquidity"),
        "dexdo oracle resolve no-liquidity",
    );
    println!("--- no-liquidity oracle resolve stdout ---\n{resolve_stdout}");
    println!("--- no-liquidity oracle resolve stderr ---\n{resolve_stderr}");
    assert!(
        resolve_stdout.contains("resolveRange submitted"),
        "{resolve_stdout}"
    );
    assert!(
        resolve_stderr.contains("no-liquidity stuck case"),
        "{resolve_stderr}"
    );

    let final_state_stdout = successful_stdout(
        Command::new(bin)
            .current_dir(WORKSPACE)
            .args([
                "oracle",
                "state",
                "--manifest",
                oracle_market_path.to_str().unwrap(),
                "--contracts",
                MANIFEST,
            ])
            .output()
            .expect("run dexdo oracle state after no-liquidity resolve"),
        "dexdo oracle state after no-liquidity resolve",
    );
    println!("--- no-liquidity final oracle state stdout ---\n{final_state_stdout}");
    assert!(
        final_state_stdout.contains("resolved_outcome=none"),
        "{final_state_stdout}"
    );
    println!("=== #26 §8 PASS: no-liquidity PMP stayed unresolved with explicit CLI failure ===");
}

/// #137 §8 (LIVE — AGENTS.md §8 merge gate): `dexdo note deploy` wallet-funded, no giver in the deploy.
///
/// The **giver faucet** (behind `test-giver`) self-funds a fresh multisig WALLET; then `dexdo note deploy` mints a
/// real `PrivateNote` FROM THAT WALLET in-process through `gosh.ackinacki`, folding it into a `DEXDO_PN_POOL`.
/// By-fact: the pooled note is ACTIVE + CANONICAL (code_hash == the live pool's PrivateNotes) on real shellnet.
/// The executor provisions its OWN wallet from the faucet — nothing is financed externally.
///
/// Run:
///   cargo test -p dexdo --features shellnet,test-giver --test live_cli \
///     live_note_deploy_via_giver_funded_wallet -- --ignored --nocapture
#[cfg(feature = "test-giver")]
async fn issue344_fund_disposable_wallet(be: &RealChainBackend, label: &str) -> (KeyPair, String) {
    let keys = KeyPair::generate();
    let wallet = be
        .deploy_multisig(&keys)
        .await
        .expect("giver-funded multisig deploy");
    let wallet_s = wallet.with_workchain();
    println!(
        "=== #344 {label}: self-funded wallet {} ===",
        masked_addr(&wallet_s)
    );
    be.giver_send_shell(&wallet_s, 400_000_000_000)
        .await
        .expect("giver SHELL (deposit + voucher)");
    be.giver_fund(&wallet_s, 200_000_000_000)
        .await
        .expect("giver native gas top-up");
    (keys, wallet_s)
}

#[cfg(feature = "test-giver")]
async fn issue344_prove_note_usable_by_provision(
    be: &RealChainBackend,
    dir: &Path,
    note_addr: &str,
    owner_secret: &str,
    label: &str,
) {
    let note = Address::parse(note_addr).expect("note addr");
    assert!(
        wait_active(be, &note).await,
        "{label}: recovered note must be active"
    );
    let key_path = write_key(dir, &format!("{label}.note.key"), owner_secret);
    let market_path = dir.join(format!("{label}.market.json"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos() as u64;
    let frame_model = format!("{FRAME_MODEL}-issue344-{label}-{nonce}");
    let stdout = successful_stdout(
        Command::new(env!("CARGO_BIN_EXE_dexdo"))
            .current_dir(WORKSPACE)
            .args([
                "provision",
                "--note-key",
                key_path.to_str().unwrap(),
                "--note-addr",
                note_addr,
                "--frame-model",
                &frame_model,
                "--contracts",
                MANIFEST,
                "--nonce",
                &nonce.to_string(),
                "--price-per-tick",
                "10000",
                "--max-ticks",
                "3",
                "--deposit-shells",
                "20",
                "--output",
                market_path.to_str().unwrap(),
            ])
            .output()
            .expect("run dexdo provision"),
        &format!("#344 {label} dexdo provision"),
    );
    println!("--- #344 {label} provision stdout ---\n{stdout}");
    let market: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&market_path).expect("read market"))
            .expect("parse market");
    let root_model = Address::parse(market["root_model"].as_str().expect("root_model"))
        .expect("root model address");
    let token_contract = Address::parse(market["token_contract"].as_str().expect("token_contract"))
        .expect("token contract address");
    assert!(
        wait_active(be, &root_model).await,
        "{label}: provisioned RootModel must be active"
    );
    assert!(
        wait_active(be, &token_contract).await,
        "{label}: provisioned TokenContract must be active"
    );
    println!(
        "=== #344 {label}: provision usable note={} root_model={} token_contract={} ===",
        masked_addr(note_addr),
        masked_addr(&root_model.with_workchain()),
        masked_addr(&token_contract.with_workchain())
    );
}

#[cfg(feature = "test-giver")]
async fn issue344_voucher_submit_recovery_case(
    be: &RealChainBackend,
    dir: &Path,
    label: &str,
    fail_flag: &str,
    voucher_field: &str,
    expect_note_recorded_after_interrupt: bool,
) {
    let (wallet_keys, wallet) = issue344_fund_disposable_wallet(be, label).await;
    let wallet_key_path = write_key(
        dir,
        &format!("{label}.wallet.secret.hex"),
        wallet_keys.secret_hex(),
    );
    let pool = dir.join(format!("{label}.pn_pool.json"));
    let recovery_path = dir.join(format!("{label}.recovery.json"));
    let interrupted = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "note",
            "deploy",
            "--multisig-address",
            &wallet,
            "--multisig-key",
            wallet_key_path.to_str().unwrap(),
            "--pool",
            pool.to_str().unwrap(),
            "--recovery",
            recovery_path.to_str().unwrap(),
            "--token-type",
            "shell",
            "--nominal",
            "N100",
            "--endpoint",
            "shellnet.ackinacki.org",
        ])
        .arg(fail_flag)
        .output()
        .expect("run interrupted voucher dexdo note deploy");
    let (interrupted_stdout, interrupted_stderr) = failed_output(
        interrupted,
        &format!("#344 {label} interrupted voucher deploy"),
    );
    println!("--- #344 {label} interrupted deploy stdout ---\n{interrupted_stdout}");
    println!("--- #344 {label} interrupted deploy stderr ---\n{interrupted_stderr}");
    assert!(
        interrupted_stderr.contains("voucher wallet submit"),
        "{interrupted_stderr}"
    );
    assert!(
        interrupted_stderr.contains("without a second wallet spend"),
        "{interrupted_stderr}"
    );
    assert!(
        !pool.exists(),
        "{label}: interrupted deploy must not write pool"
    );
    assert_private_file_mode(&recovery_path, &format!("{label} recovery state"));
    let interrupted_state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recovery_path).expect("read recovery state"))
            .expect("parse recovery state");
    assert_eq!(
        interrupted_state["pn_address"].is_string(),
        expect_note_recorded_after_interrupt,
        "{label}: unexpected pn_address state after failpoint"
    );
    assert_eq!(
        interrupted_state[voucher_field]["submit_maybe_sent"], true,
        "{label}: voucher submit checkpoint missing"
    );
    assert!(
        interrupted_state[voucher_field]["event"].is_null(),
        "{label}: submit failpoint should stop before event checkpoint"
    );
    let owner_secret = interrupted_state["owner_secret_key_hex"]
        .as_str()
        .expect("owner secret")
        .to_string();
    let voucher_secret = interrupted_state[voucher_field]["sk_u_hex"]
        .as_str()
        .expect("voucher sk_u_hex");
    assert!(
        !interrupted_stdout.contains(&owner_secret)
            && !interrupted_stderr.contains(&owner_secret)
            && !interrupted_stdout.contains(wallet_keys.secret_hex())
            && !interrupted_stderr.contains(wallet_keys.secret_hex())
            && !interrupted_stdout.contains(voucher_secret)
            && !interrupted_stderr.contains(voucher_secret),
        "{label}: interrupted output leaked wallet/note/voucher secret"
    );

    let resumed = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "note",
            "deploy",
            "--multisig-address",
            &wallet,
            "--multisig-key",
            wallet_key_path.to_str().unwrap(),
            "--pool",
            pool.to_str().unwrap(),
            "--recovery",
            recovery_path.to_str().unwrap(),
            "--token-type",
            "shell",
            "--nominal",
            "N100",
            "--endpoint",
            "shellnet.ackinacki.org",
        ])
        .output()
        .expect("rerun dexdo note deploy");
    assert!(
        resumed.status.success(),
        "#344 {label} resumed deploy failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed_stdout = String::from_utf8(resumed.stdout).expect("stdout utf8");
    let resumed_stderr = String::from_utf8(resumed.stderr).expect("stderr utf8");
    println!("--- #344 {label} resumed deploy stdout ---\n{resumed_stdout}");
    println!("--- #344 {label} resumed deploy stderr ---\n{resumed_stderr}");
    assert!(
        resumed_stderr.contains("without submitting another wallet spend")
            || resumed_stderr.contains("no wallet spend will be submitted"),
        "{resumed_stderr}"
    );
    assert!(
        !resumed_stdout.contains(&owner_secret)
            && !resumed_stderr.contains(&owner_secret)
            && !resumed_stdout.contains(wallet_keys.secret_hex())
            && !resumed_stderr.contains(wallet_keys.secret_hex())
            && !resumed_stdout.contains(voucher_secret)
            && !resumed_stderr.contains(voucher_secret),
        "{label}: resumed output leaked wallet/note/voucher secret"
    );
    assert_private_file_mode(&pool, &format!("{label} pool"));
    let pool_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool).expect("read resumed pool"))
            .expect("parse resumed pool");
    let (note_addr, pool_secret) = pool_note_identity(&pool_json, 0);
    assert_eq!(pool_secret, owner_secret);
    let final_state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recovery_path).expect("read final recovery state"))
            .expect("parse final recovery state");
    assert!(final_state[voucher_field]["proof"].is_object());
    assert_eq!(final_state["shell_funded"], true);
    assert_eq!(final_state["sanity_checked"], true);
    issue344_prove_note_usable_by_provision(be, dir, &note_addr, &pool_secret, label).await;
}

/// #344 §8 money-safety gate: interruption immediately after the wallet-funded VoucherGenerated submit is
/// recovered by finishing the persisted voucher, not by minting a second voucher. Covers both the deposit
/// voucher and the SHELL gas voucher.
#[cfg(feature = "test-giver")]
#[tokio::test]
#[ignore = "live shellnet + giver faucet; explicit run only"]
async fn live_note_deploy_recovery_after_interrupted_voucher_submit() {
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let dir = create_private_temp_dir("dexdo-note-voucher-recovery-live");
    let _cleanup = TempDirCleanup(dir.clone());

    issue344_voucher_submit_recovery_case(
        &be,
        &dir,
        "deposit-submit",
        "--simulate-interrupt-after-deposit-voucher-submit",
        "deposit_voucher",
        false,
    )
    .await;
    issue344_voucher_submit_recovery_case(
        &be,
        &dir,
        "shell-submit",
        "--simulate-interrupt-after-shell-voucher-submit",
        "shell_voucher",
        true,
    )
    .await;

    println!(
        "=== #344 §8 PASS: deposit and SHELL voucher submits recovered without second wallet spends ==="
    );
}

/// #344 §8 money-safety gate: interruption after `deployPrivateNote` made the PrivateNote active but before
/// recovery recorded `pn_address`/`deposit_identifier_hash` is recovered by discovering the active note from the
/// persisted deposit proof, not by submitting `deployPrivateNote` again.
#[cfg(feature = "test-giver")]
#[tokio::test]
#[ignore = "live shellnet + giver faucet; explicit run only"]
async fn live_note_deploy_recovery_after_interrupted_deploy_before_note_record() {
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let dir = create_private_temp_dir("dexdo-note-deploy-record-recovery-live");
    let _cleanup = TempDirCleanup(dir.clone());

    let (wallet_keys, wallet) = issue344_fund_disposable_wallet(&be, "deploy-record").await;
    let wallet_key_path = write_key(
        &dir,
        "deploy-record.wallet.secret.hex",
        wallet_keys.secret_hex(),
    );
    let pool = dir.join("deploy-record.pn_pool.json");
    let recovery_path = dir.join("deploy-record.recovery.json");
    let interrupted = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "note",
            "deploy",
            "--multisig-address",
            &wallet,
            "--multisig-key",
            wallet_key_path.to_str().unwrap(),
            "--pool",
            pool.to_str().unwrap(),
            "--recovery",
            recovery_path.to_str().unwrap(),
            "--token-type",
            "shell",
            "--nominal",
            "N100",
            "--endpoint",
            "shellnet.ackinacki.org",
            "--simulate-interrupt-after-deploy-before-note-record",
        ])
        .output()
        .expect("run interrupted deploy-record dexdo note deploy");
    let (interrupted_stdout, interrupted_stderr) = failed_output(
        interrupted,
        "#344 deploy-record interrupted dexdo note deploy",
    );
    println!("--- #344 deploy-record interrupted stdout ---\n{interrupted_stdout}");
    println!("--- #344 deploy-record interrupted stderr ---\n{interrupted_stderr}");
    assert!(
        interrupted_stderr.contains("simulated interruption after deployPrivateNote active"),
        "{interrupted_stderr}"
    );
    assert!(
        interrupted_stderr.contains("without repeating deployPrivateNote"),
        "{interrupted_stderr}"
    );
    assert!(
        !pool.exists(),
        "interrupted deploy-record path must not write pool"
    );
    assert_private_file_mode(&recovery_path, "deploy-record recovery state");
    let interrupted_state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recovery_path).expect("read recovery state"))
            .expect("parse recovery state");
    assert!(
        interrupted_state["pn_address"].is_null(),
        "failpoint must stop before pn_address record"
    );
    assert!(
        interrupted_state["deposit_identifier_hash"].is_null(),
        "failpoint must stop before deposit hash record"
    );
    assert!(interrupted_state["deposit_voucher"]["proof"].is_object());
    assert!(interrupted_state["shell_voucher"].is_null());
    let owner_secret = interrupted_state["owner_secret_key_hex"]
        .as_str()
        .expect("owner secret")
        .to_string();
    let voucher_secret = interrupted_state["deposit_voucher"]["sk_u_hex"]
        .as_str()
        .expect("deposit voucher sk_u_hex");
    assert!(
        !interrupted_stdout.contains(&owner_secret)
            && !interrupted_stderr.contains(&owner_secret)
            && !interrupted_stdout.contains(wallet_keys.secret_hex())
            && !interrupted_stderr.contains(wallet_keys.secret_hex())
            && !interrupted_stdout.contains(voucher_secret)
            && !interrupted_stderr.contains(voucher_secret),
        "deploy-record interrupted output leaked wallet/note/voucher secret"
    );

    let resumed = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "note",
            "deploy",
            "--multisig-address",
            &wallet,
            "--multisig-key",
            wallet_key_path.to_str().unwrap(),
            "--pool",
            pool.to_str().unwrap(),
            "--recovery",
            recovery_path.to_str().unwrap(),
            "--token-type",
            "shell",
            "--nominal",
            "N100",
            "--endpoint",
            "shellnet.ackinacki.org",
        ])
        .output()
        .expect("rerun deploy-record dexdo note deploy");
    assert!(
        resumed.status.success(),
        "#344 deploy-record resumed deploy failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed_stdout = String::from_utf8(resumed.stdout).expect("stdout utf8");
    let resumed_stderr = String::from_utf8(resumed.stderr).expect("stderr utf8");
    println!("--- #344 deploy-record resumed stdout ---\n{resumed_stdout}");
    println!("--- #344 deploy-record resumed stderr ---\n{resumed_stderr}");
    assert!(
        resumed_stderr.contains("recovered active PrivateNote")
            && resumed_stderr.contains("skipping repeat deployPrivateNote submit"),
        "{resumed_stderr}"
    );
    assert!(
        !resumed_stdout.contains(&owner_secret)
            && !resumed_stderr.contains(&owner_secret)
            && !resumed_stdout.contains(wallet_keys.secret_hex())
            && !resumed_stderr.contains(wallet_keys.secret_hex())
            && !resumed_stdout.contains(voucher_secret)
            && !resumed_stderr.contains(voucher_secret),
        "deploy-record resumed output leaked wallet/note/voucher secret"
    );
    assert_private_file_mode(&pool, "deploy-record pool");
    let pool_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool).expect("read resumed pool"))
            .expect("parse resumed pool");
    let (note_addr, pool_secret) = pool_note_identity(&pool_json, 0);
    assert_eq!(pool_secret, owner_secret);
    let final_state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recovery_path).expect("read final recovery state"))
            .expect("parse final recovery state");
    assert_eq!(final_state["pn_address"], note_addr);
    assert!(final_state["deposit_identifier_hash"].is_string());
    assert!(final_state["shell_voucher"]["proof"].is_object());
    assert_eq!(final_state["shell_funded"], true);
    assert_eq!(final_state["sanity_checked"], true);

    issue344_prove_note_usable_by_provision(&be, &dir, &note_addr, &pool_secret, "deploy-record")
        .await;
    println!(
        "=== #344 §8 PASS: deployPrivateNote active-before-record recovered without repeat deploy submit ==="
    );
}

/// #344 §8 money-safety gate: owner key is persisted before spend; an interruption after spend but before pool
/// write is recoverable without re-spending the completed note deploy, and both recovered + happy notes can
/// provision live shellnet contracts.
#[cfg(feature = "test-giver")]
#[tokio::test]
#[ignore = "live shellnet + giver faucet; explicit run only"]
async fn live_note_deploy_recovery_after_interrupted_pool_write() {
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let dir = create_private_temp_dir("dexdo-note-recovery-live");
    let _cleanup = TempDirCleanup(dir.clone());

    let (recovery_wallet_keys, recovery_wallet) =
        issue344_fund_disposable_wallet(&be, "negative").await;
    let recovery_wallet_key_path = write_key(
        &dir,
        "negative.wallet.secret.hex",
        recovery_wallet_keys.secret_hex(),
    );
    let recovery_pool = dir.join("negative.pn_pool.json");
    let recovery_state_path = dir.join("negative.recovery.json");
    let interrupted = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "note",
            "deploy",
            "--multisig-address",
            &recovery_wallet,
            "--multisig-key",
            recovery_wallet_key_path.to_str().unwrap(),
            "--pool",
            recovery_pool.to_str().unwrap(),
            "--recovery",
            recovery_state_path.to_str().unwrap(),
            "--token-type",
            "shell",
            "--nominal",
            "N100",
            "--endpoint",
            "shellnet.ackinacki.org",
            "--simulate-interrupt-after-spend-before-pool",
        ])
        .output()
        .expect("run interrupted dexdo note deploy");
    let (interrupted_stdout, interrupted_stderr) =
        failed_output(interrupted, "#344 interrupted dexdo note deploy");
    println!("--- #344 interrupted deploy stdout ---\n{interrupted_stdout}");
    println!("--- #344 interrupted deploy stderr ---\n{interrupted_stderr}");
    assert!(
        interrupted_stderr.contains("simulated interruption after on-chain spend"),
        "{interrupted_stderr}"
    );
    assert!(
        interrupted_stderr.contains("dexdo note recover"),
        "{interrupted_stderr}"
    );
    assert!(
        !recovery_pool.exists(),
        "interrupted deploy must not leave a final pool file"
    );
    assert_private_file_mode(&recovery_state_path, "negative recovery state");
    let recovery_state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recovery_state_path).expect("read recovery state"))
            .expect("parse recovery state");
    let recovered_secret = recovery_state["owner_secret_key_hex"]
        .as_str()
        .expect("recovery owner secret");
    assert!(
        !interrupted_stdout.contains(recovered_secret)
            && !interrupted_stderr.contains(recovered_secret)
            && !interrupted_stdout.contains(recovery_wallet_keys.secret_hex())
            && !interrupted_stderr.contains(recovery_wallet_keys.secret_hex()),
        "interrupted deploy output must not leak wallet or note secrets"
    );
    assert_eq!(recovery_state["shell_funded"], true);
    assert_eq!(recovery_state["sanity_checked"], true);
    let recovered_note_addr = recovery_state["pn_address"]
        .as_str()
        .expect("recovery pn_address")
        .to_string();

    let recover_stdout = successful_stdout(
        Command::new(env!("CARGO_BIN_EXE_dexdo"))
            .current_dir(WORKSPACE)
            .args([
                "note",
                "recover",
                "--recovery",
                recovery_state_path.to_str().unwrap(),
                "--pool",
                recovery_pool.to_str().unwrap(),
            ])
            .output()
            .expect("run dexdo note recover"),
        "#344 dexdo note recover",
    );
    println!("--- #344 note recover stdout ---\n{recover_stdout}");
    assert!(
        recover_stdout.contains("No wallet spend was submitted"),
        "{recover_stdout}"
    );
    assert_private_file_mode(&recovery_pool, "negative recovered pool");
    let recovered_pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recovery_pool).expect("read recovered pool"))
            .expect("parse recovered pool");
    assert_eq!(recovered_pool["notes"][0]["address"], recovered_note_addr);
    let (_, recovered_pool_secret) = pool_note_identity(&recovered_pool, 0);
    assert_eq!(recovered_pool_secret, recovered_secret);
    issue344_prove_note_usable_by_provision(
        &be,
        &dir,
        &recovered_note_addr,
        &recovered_pool_secret,
        "recovered",
    )
    .await;

    let (happy_wallet_keys, happy_wallet) = issue344_fund_disposable_wallet(&be, "happy").await;
    let happy_wallet_key_path = write_key(
        &dir,
        "happy.wallet.secret.hex",
        happy_wallet_keys.secret_hex(),
    );
    let happy_pool = dir.join("happy.pn_pool.json");
    let happy_recovery = dir.join("happy.recovery.json");
    let happy_stdout = successful_stdout(
        Command::new(env!("CARGO_BIN_EXE_dexdo"))
            .current_dir(WORKSPACE)
            .args([
                "note",
                "deploy",
                "--multisig-address",
                &happy_wallet,
                "--multisig-key",
                happy_wallet_key_path.to_str().unwrap(),
                "--pool",
                happy_pool.to_str().unwrap(),
                "--recovery",
                happy_recovery.to_str().unwrap(),
                "--token-type",
                "shell",
                "--nominal",
                "N100",
                "--endpoint",
                "shellnet.ackinacki.org",
            ])
            .output()
            .expect("run happy dexdo note deploy"),
        "#344 happy dexdo note deploy",
    );
    println!("--- #344 happy note deploy stdout ---\n{happy_stdout}");
    assert_private_file_mode(&happy_pool, "happy pool");
    assert_private_file_mode(&happy_recovery, "happy recovery state");
    let happy_pool_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&happy_pool).expect("read happy pool"))
            .expect("parse happy pool");
    let (happy_note_addr, happy_secret) = pool_note_identity(&happy_pool_json, 0);
    assert!(
        !happy_stdout.contains(&happy_secret)
            && !happy_stdout.contains(happy_wallet_keys.secret_hex()),
        "happy deploy stdout must not leak wallet or note secrets"
    );
    issue344_prove_note_usable_by_provision(&be, &dir, &happy_note_addr, &happy_secret, "happy")
        .await;

    println!(
        "=== #344 §8 PASS: interrupted note deploy recovered+provisioned and happy note deploy provisioned ==="
    );
}

#[cfg(feature = "test-giver")]
#[tokio::test]
#[ignore = "live shellnet + giver faucet; explicit run only"]
async fn live_note_deploy_via_giver_funded_wallet() {
    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");

    // 1. Self-fund: the giver deploys + activates a fresh multisig wallet, then tops it up with ECC[2] SHELL
    //    (the N100 deposit 1e11 + the ~1e11 gas voucher + buffer) and extra native gas for the queued messages.
    let keys = KeyPair::generate();
    let wallet = be
        .deploy_multisig(&keys)
        .await
        .expect("giver-funded multisig deploy");
    let wallet_s = wallet.with_workchain();
    println!("=== #137 §8: self-funded wallet {wallet_s} ===");
    be.giver_send_shell(&wallet_s, 400_000_000_000)
        .await
        .expect("giver SHELL (deposit + voucher)");
    be.giver_fund(&wallet_s, 200_000_000_000)
        .await
        .expect("giver native gas top-up");

    // 2. `--multisig-key` is a BARE secret-hex file, same as the other note-key CLI paths.
    let dir = create_private_temp_dir("dexdo-note-live");
    let _cleanup = TempDirCleanup(dir.clone());
    let keys_path = write_key(&dir, "wallet.secret.hex", keys.secret_hex());
    let pool_path = dir.join("pn_pool.json");

    // 3. `dexdo note deploy` FROM the funded wallet — in-process through gosh.ackinacki (no subprocess/giver).
    let out = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "note",
            "deploy",
            "--multisig-address",
            &wallet_s,
            "--multisig-key",
            keys_path.to_str().unwrap(),
            "--pool",
            pool_path.to_str().unwrap(),
            "--token-type",
            "shell",
            "--nominal",
            "N100",
            "--endpoint",
            "shellnet.ackinacki.org",
        ])
        .output()
        .expect("run dexdo note deploy");
    println!(
        "--- dexdo note deploy stdout ---\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    println!("--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "dexdo note deploy must succeed against the funded wallet"
    );

    // 4. By-fact: the pool carries the deployed note, and it is LIVE + CANONICAL on shellnet.
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("pool written"))
            .expect("pool json");
    assert_eq!(pool["nominal"], "N100");
    let note_addr = pool["notes"][0]["address"]
        .as_str()
        .expect("pool has note address")
        .to_string();
    println!("=== #137 §8: deployed PrivateNote {note_addr} ===");
    let na = Address::parse(&note_addr).expect("note addr");
    let acc = be
        .client()
        .get_account(&na)
        .await
        .expect("probe note")
        .expect("note exists on-chain");
    assert!(
        acc.is_active(),
        "the deployed note must be active on shellnet"
    );
    let minted_hash = acc.code_hash.clone().expect("note code_hash");
    println!("=== minted note code_hash = {minted_hash} ===");

    // Pin-match (Gate 1): the freshly-minted note's code matches the live pool's canonical PrivateNotes. Compare
    // against an existing DEXDO_PN_POOL note (known-canonical) when available — a by-fact pin-match, no offline pin.
    if let Ok(env_pool) = std::env::var("DEXDO_PN_POOL") {
        if let Ok(bytes) = std::fs::read(&env_pool) {
            if let Ok(p) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(ref_addr) = p["notes"][0]["address"].as_str() {
                    let ra = Address::parse(ref_addr).expect("ref note addr");
                    if let Some(ref_hash) = be
                        .client()
                        .get_account(&ra)
                        .await
                        .expect("probe ref")
                        .and_then(|a| a.code_hash)
                    {
                        assert_eq!(
                            minted_hash, ref_hash,
                            "Gate 1: minted note code_hash must equal the canonical PrivateNote (live pool)"
                        );
                        println!("=== #137 Gate 1 pin-match OK: minted == canonical pool note code_hash ===");
                    }
                }
            }
        }
    }
    println!("=== #137 §8 PASS: giver-funded wallet -> dexdo note deploy -> live canonical PrivateNote ===");
}
