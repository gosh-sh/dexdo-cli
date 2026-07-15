//! Live #214 policy proofs through the `dexdo` binary.
//!
//! These tests are ignored and require a freshly minted `DEXDO_PN_POOL`.
//! They do not mint notes or use a giver in the operate path.

#![cfg(feature = "shellnet")]

use dexdo_core::{
    required_escrow_for_buy, Address, KeyPair, OrderBookOrder, RealChainBackend,
    MATCH_OPEN_TIMEOUT_SECS,
};
use serde_json::json;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contracts/deployed.shellnet.json"
);
const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FRAME_MODEL: &str = "qwen--qwen3--32b";
const DEPLOY_GAS: u128 = 10_000_000_000;

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

fn write_private_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path).expect("create private file");
    file.write_all(contents.as_bytes())
        .expect("write private file");
    path
}

fn live_pool_or_skip(test_name: &str, min_notes: usize) -> Option<serde_json::Value> {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("{test_name}: DEXDO_PN_POOL not set - skipping live shellnet test");
        return None;
    };
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read DEXDO_PN_POOL"))
            .expect("parse DEXDO_PN_POOL");
    let notes = pool["notes"].as_array().expect("notes[]");
    if notes.len() < min_notes {
        eprintln!(
            "{test_name}: DEXDO_PN_POOL has {} note(s), need {min_notes} - skipping",
            notes.len()
        );
        return None;
    }
    Some(pool)
}

async fn ecc_shell_balance(be: &RealChainBackend, addr: &Address) -> u128 {
    be.client()
        .get_account(addr)
        .await
        .expect("get account")
        .expect("account exists")
        .ecc_balance(2)
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

async fn wait_resting_ask_gone(
    be: &RealChainBackend,
    ob: &Address,
    frame_model: &str,
    model_hash: &str,
    token_contract: &str,
) -> bool {
    for _ in 0..20 {
        let snapshot = be
            .inference_orderbook_snapshot(ob, frame_model, model_hash)
            .await
            .expect("read order book snapshot");
        let visible = snapshot.resting_asks().any(|o| {
            o.token_contract
                .as_deref()
                .is_some_and(|tc| tc.eq_ignore_ascii_case(token_contract))
        });
        if !visible {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    false
}

fn json_u64_field(v: &serde_json::Value, key: &str) -> Option<u64> {
    if let Some(n) = v[key].as_u64() {
        return Some(n);
    }
    v[key].as_str()?.parse().ok()
}

fn json_u128_field(v: &serde_json::Value, key: &str) -> Option<u128> {
    if let Some(n) = v[key].as_u64() {
        return Some(n as u128);
    }
    v[key].as_str()?.parse().ok()
}

fn tail_file(path: &Path, n: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(n);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

async fn wait_child_exit(
    child: &mut std::process::Child,
    deadline: Instant,
    stdout_log: &Path,
    stderr_log: &Path,
    label: &str,
) -> ExitStatus {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {}
            Err(e) => panic!("{label} status check failed: {e}"),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{label} did not exit before deadline\n--- stdout tail ---\n{}\n--- stderr tail ---\n{}",
                tail_file(stdout_log, 5000),
                tail_file(stderr_log, 5000)
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn write_buyer_policy_with_no_handover_action(
    dir: &Path,
    escrow_cap: u128,
    no_handover_after_match: &str,
) -> PathBuf {
    let cap = u64::try_from(escrow_cap.saturating_mul(2)).expect("policy cap fits u64");
    let policy = json!({
        "version": 1,
        "buyer": {
            "on": {
                "no_handover_after_match": no_handover_after_match,
                "malformed_handover": "fail_closed",
                "dead_gateway": "retry_then_reclaim",
                "empty_stream": "fail_closed",
                "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                "bad_output_scam": "stop"
            },
            "failover": {
                "max_sellers_to_try": 1,
                "total_spend_cap_shells": cap
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
    write_private_file(
        dir,
        "policy.json",
        &serde_json::to_string_pretty(&policy).expect("policy serializes"),
    )
}

fn write_buyer_policy(dir: &Path, escrow_cap: u128) -> PathBuf {
    write_buyer_policy_with_no_handover_action(dir, escrow_cap, "wait_then_reclaim")
}

/// #214 live proof: configured `no_handover_after_match=wait_then_reclaim` executes
/// through the real CLI buyer, submits `streamCleanup` on shellnet, and leaves no
/// escrow locked in the matched TokenContract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live shellnet #214; consumes notes and waits MATCH_OPEN_TIMEOUT before policy cleanup"]
async fn live_214_no_handover_policy_wait_then_reclaim_cleanup_by_fact() {
    let test_name = "live_214_no_handover_policy_wait_then_reclaim_cleanup_by_fact";
    let Some(pool) = live_pool_or_skip(test_name, 2) else {
        return;
    };

    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &pool, 2).await;
    let (seller_addr, seller_secret, seller_ecc2) = role_notes[0].clone();
    let (buyer_addr, buyer_secret, _buyer_ecc2) = role_notes[1].clone();
    assert!(
        seller_ecc2 >= DEPLOY_GAS * 2,
        "seller note needs deploy headroom, got {seller_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&seller_secret).expect("seller kp");
    let seller_note = Address::parse(&seller_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs();
    let frame_model = format!("{FRAME_MODEL}-issue214-nohandover-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 2;
    let escrow = required_escrow_for_buy(max_ticks, price);

    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &frame_model,
            nonce,
            price,
            max_ticks,
            DEPLOY_GAS,
        )
        .await
        .expect("provision market");
    let order_book = Address::parse(&manifest.inference_order_book).expect("order book addr");
    let token_contract = Address::parse(&manifest.token_contract).expect("tc addr");
    let token_contract_arg = token_contract.with_workchain();
    assert!(
        wait_active(&be, &token_contract).await,
        "TC active after note-funded deploy"
    );

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post one sell offer");
    let order = wait_resting_ask(
        &be,
        &order_book,
        &frame_model,
        &manifest.model_hash,
        &token_contract_arg,
    )
    .await
    .expect("seller ask rests before buyer starts");

    let dir = create_private_temp_dir(&format!("dexdo_live_214_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let buyer_key = write_private_file(&dir, "buyer.key", &buyer_secret);
    let policy_path = write_buyer_policy(&dir, escrow);
    let stdout_log = dir.join("buyer.stdout.log");
    let stderr_log = dir.join("buyer.stderr.log");
    let stdout = std::fs::File::create(&stdout_log).expect("create buyer stdout log");
    let stderr = std::fs::File::create(&stderr_log).expect("create buyer stderr log");

    let mut buyer = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "buyer",
            "--mock-model",
            "--note-key",
            buyer_key.to_str().unwrap(),
            "--note-addr",
            &buyer_addr,
            "--frame-model",
            &frame_model,
            "--ticks",
            &max_ticks.to_string(),
            "--max-price-per-tick",
            &price.to_string(),
            "--max-tokens",
            "2",
            "--policy",
            policy_path.to_str().unwrap(),
            "--contracts",
            MANIFEST,
        ])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        .spawn()
        .expect("spawn dexdo buyer");

    let deadline = Instant::now() + Duration::from_secs(MATCH_OPEN_TIMEOUT_SECS + 480);
    let mut funded_state = None;
    let mut opened_seen = false;
    let status = loop {
        if let Ok(Some(state)) = be.token_contract_state(&token_contract).await {
            let funded = state["funded"].as_bool().unwrap_or(false);
            let opened = state["opened"].as_bool().unwrap_or(false);
            opened_seen |= opened;
            if funded && funded_state.is_none() {
                funded_state = Some(state);
            }
        }
        match buyer.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => panic!("buyer status check failed: {e}"),
        }
        if Instant::now() >= deadline {
            break wait_child_exit(
                &mut buyer,
                Instant::now(),
                &stdout_log,
                &stderr_log,
                "dexdo buyer",
            )
            .await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };

    let stdout = tail_file(&stdout_log, 20_000);
    let stderr = tail_file(&stderr_log, 20_000);
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        !status.success(),
        "policy cleanup path should exit non-zero with the policy diagnostic\n{combined}"
    );
    let funded_state = funded_state.expect("buyer match funded the TC before policy cleanup");
    let funded_time = json_u64_field(&funded_state, "fundedTime");
    assert!(
        funded_state["funded"].as_bool().unwrap_or(false),
        "matched TC reached funded=true before policy cleanup: {funded_state}"
    );
    assert!(
        !funded_state["opened"].as_bool().unwrap_or(false),
        "no seller process should leave TC funded but unopened before cleanup: {funded_state}"
    );
    assert!(
        !opened_seen,
        "no_handover_after_match proof must not open a stream before cleanup"
    );
    assert!(
        combined.contains("failure_class=no_handover_after_match"),
        "buyer output must report the #214 failure class\n{combined}"
    );
    assert!(
        combined.contains("action=wait_then_reclaim"),
        "buyer output must report the configured policy action\n{combined}"
    );
    assert!(
        combined.contains("result=cleanup_unopened_submitted"),
        "buyer output must show the streamCleanup submission\n{combined}"
    );
    assert!(
        combined.contains("result=money_reclaimed"),
        "buyer output must show the terminal money-reclaimed policy result\n{combined}"
    );

    let post = be
        .token_contract_state(&token_contract)
        .await
        .expect("read post-cleanup TC state");
    if let Some(state) = &post {
        let funded = state["funded"].as_bool().unwrap_or(false);
        let opened = state["opened"].as_bool().unwrap_or(false);
        let frozen = json_u128_field(state, "frozen").unwrap_or(0);
        let deposit = json_u128_field(state, "deposit").unwrap_or(0);
        assert!(!funded, "post-cleanup TC must not remain funded: {state}");
        assert!(!opened, "post-cleanup TC must not remain opened: {state}");
        assert_eq!(
            frozen, 0,
            "post-cleanup TC frozen funds must be zero: {state}"
        );
        assert_eq!(deposit, 0, "post-cleanup TC deposit must be zero: {state}");
    }
    assert!(
        wait_resting_ask_gone(
            &be,
            &order_book,
            &frame_model,
            &manifest.model_hash,
            &token_contract_arg,
        )
        .await,
        "filled ask must not reappear after policy cleanup"
    );

    eprintln!(
        "issue214 no-handover policy cleanup by fact: order_id={} tc={} fundedTime={funded_time:?} final_state={post:?}",
        order.order_id, token_contract_arg
    );
}

/// #338 live proof: the matched buyer TC is persisted into DEXDO_PN_POOL and the recovery command can reclaim
/// using only that pool file, without note-key/note-addr/token-contract flags or scraped logs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live shellnet #338; consumes notes and waits MATCH_OPEN_TIMEOUT before pool-only reclaim"]
async fn live_338_pool_only_reclaim_uses_recorded_token_contract_by_fact() {
    let test_name = "live_338_pool_only_reclaim_uses_recorded_token_contract_by_fact";
    let Some(source_pool) = live_pool_or_skip(test_name, 2) else {
        return;
    };

    let be = RealChainBackend::connect(MANIFEST).expect("connect shellnet");
    let role_notes = live_role_notes_by_ecc(&be, &source_pool, 2).await;
    let (seller_addr, seller_secret, seller_ecc2) = role_notes[0].clone();
    let (buyer_addr, buyer_secret, _buyer_ecc2) = role_notes[1].clone();
    assert!(
        seller_ecc2 >= DEPLOY_GAS * 2,
        "seller note needs deploy headroom, got {seller_ecc2}"
    );

    let seller_kp = KeyPair::from_secret_hex(&seller_secret).expect("seller kp");
    let seller_note = Address::parse(&seller_addr).expect("seller note addr");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs();
    let frame_model = format!("{FRAME_MODEL}-issue338-pool-reclaim-{nonce}");
    let price: u128 = 10_000;
    let max_ticks: u128 = 2;
    let escrow = required_escrow_for_buy(max_ticks, price);

    let manifest = be
        .provision_market(
            &seller_kp,
            &seller_note,
            &frame_model,
            nonce,
            price,
            max_ticks,
            DEPLOY_GAS,
        )
        .await
        .expect("provision market");
    let order_book = Address::parse(&manifest.inference_order_book).expect("order book addr");
    let token_contract = Address::parse(&manifest.token_contract).expect("tc addr");
    let token_contract_arg = token_contract.with_workchain();
    assert!(
        wait_active(&be, &token_contract).await,
        "TC active after note-funded deploy"
    );

    be.post_sell_offer(&seller_note, &seller_kp, 0, nonce)
        .await
        .expect("post one sell offer");
    let order = wait_resting_ask(
        &be,
        &order_book,
        &frame_model,
        &manifest.model_hash,
        &token_contract_arg,
    )
    .await
    .expect("seller ask rests before buyer starts");

    let dir = create_private_temp_dir(&format!("dexdo_live_338_{nonce}"));
    let _cleanup = TempDirCleanup(dir.clone());
    let pool_path = write_private_file(
        &dir,
        "pn_pool.json",
        &serde_json::to_string_pretty(&source_pool).expect("pool serializes"),
    );
    let buyer_key = write_private_file(&dir, "buyer.key", &buyer_secret);
    let policy_path = write_buyer_policy_with_no_handover_action(&dir, escrow, "fail_closed");
    let stdout_log = dir.join("buyer.stdout.log");
    let stderr_log = dir.join("buyer.stderr.log");
    let stdout = std::fs::File::create(&stdout_log).expect("create buyer stdout log");
    let stderr = std::fs::File::create(&stderr_log).expect("create buyer stderr log");

    let mut buyer = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "buyer",
            "--mock-model",
            "--note-key",
            buyer_key.to_str().unwrap(),
            "--note-addr",
            &buyer_addr,
            "--frame-model",
            &frame_model,
            "--ticks",
            &max_ticks.to_string(),
            "--max-price-per-tick",
            &price.to_string(),
            "--max-tokens",
            "2",
            "--policy",
            policy_path.to_str().unwrap(),
            "--contracts",
            MANIFEST,
        ])
        .env("DEXDO_PN_POOL", &pool_path)
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        .spawn()
        .expect("spawn dexdo buyer");

    let deadline = Instant::now() + Duration::from_secs(MATCH_OPEN_TIMEOUT_SECS + 480);
    let mut funded_state = None;
    let mut opened_seen = false;
    let status = loop {
        if let Ok(Some(state)) = be.token_contract_state(&token_contract).await {
            let funded = state["funded"].as_bool().unwrap_or(false);
            let opened = state["opened"].as_bool().unwrap_or(false);
            opened_seen |= opened;
            if funded && funded_state.is_none() {
                funded_state = Some(state);
            }
        }
        match buyer.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => panic!("buyer status check failed: {e}"),
        }
        if Instant::now() >= deadline {
            break wait_child_exit(
                &mut buyer,
                Instant::now(),
                &stdout_log,
                &stderr_log,
                "dexdo buyer",
            )
            .await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };

    let stdout = tail_file(&stdout_log, 20_000);
    let stderr = tail_file(&stderr_log, 20_000);
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        !status.success(),
        "fail_closed path should exit non-zero without cleanup\n{combined}"
    );
    assert!(
        combined.contains("failure_class=no_handover_after_match"),
        "buyer output must report no_handover_after_match\n{combined}"
    );
    assert!(
        combined.contains("action=fail_closed"),
        "buyer output must report fail_closed\n{combined}"
    );
    assert!(
        combined.contains("result=no_recovery_submitted"),
        "buyer output must leave recovery to the operator\n{combined}"
    );
    let funded_state = funded_state.expect("buyer match funded the TC before fail_closed");
    let funded_time =
        json_u64_field(&funded_state, "fundedTime").expect("funded matched TC exposes fundedTime");
    assert!(
        funded_state["funded"].as_bool().unwrap_or(false),
        "matched TC reached funded=true before fail_closed: {funded_state}"
    );
    assert!(
        !funded_state["opened"].as_bool().unwrap_or(false),
        "no seller process should leave TC funded but unopened before reclaim: {funded_state}"
    );
    assert!(
        !opened_seen,
        "pool-only reclaim proof must not open a stream"
    );

    let pool_after_buyer: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read updated pool"))
            .expect("parse updated pool");
    let buyer_entry = pool_after_buyer["notes"]
        .as_array()
        .expect("notes[]")
        .iter()
        .find(|note| {
            note["address"]
                .as_str()
                .is_some_and(|addr| addr.eq_ignore_ascii_case(&buyer_addr))
        })
        .expect("buyer note entry in pool");
    assert_eq!(
        buyer_entry["token_contract"]
            .as_str()
            .map(str::to_ascii_lowercase),
        Some(token_contract_arg.to_ascii_lowercase()),
        "buyer pool entry must record the matched TC"
    );
    assert_eq!(
        buyer_entry["token_contract_role"].as_str(),
        Some("buyer"),
        "buyer pool entry must identify the TC role"
    );
    assert!(
        buyer_entry["token_contract_updated_at_unix"]
            .as_u64()
            .is_some(),
        "buyer pool entry must timestamp the TC recovery metadata"
    );

    let reclaim_ready_at = funded_time
        .saturating_add(MATCH_OPEN_TIMEOUT_SECS)
        .saturating_add(5);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs();
    if now < reclaim_ready_at {
        let wait = reclaim_ready_at - now;
        eprintln!(
            "issue338 waiting {wait}s for MATCH_OPEN_TIMEOUT before pool-only reclaim tc={token_contract_arg}"
        );
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }

    let reclaim = Command::new(env!("CARGO_BIN_EXE_dexdo"))
        .current_dir(WORKSPACE)
        .args([
            "reclaim",
            "--pool",
            pool_path.to_str().unwrap(),
            "--contracts",
            MANIFEST,
        ])
        .env_remove("DEXDO_PN_POOL")
        .output()
        .expect("run dexdo reclaim --pool");
    let reclaim_stdout = String::from_utf8_lossy(&reclaim.stdout);
    let reclaim_stderr = String::from_utf8_lossy(&reclaim.stderr);
    let reclaim_combined = format!("{reclaim_stdout}\n{reclaim_stderr}");
    assert!(
        reclaim.status.success(),
        "dexdo reclaim --pool must succeed using only pool metadata\n{reclaim_combined}"
    );
    assert!(
        reclaim_combined.contains("streamCleanup") || reclaim_combined.contains("cleanupUnopened"),
        "pool-only reclaim must use the never-opened cleanup path\n{reclaim_combined}"
    );
    assert!(
        !reclaim_combined.contains("owner_secret_key_hex"),
        "pool-only reclaim output must not leak pool secrets\n{reclaim_combined}"
    );

    let mut post = None;
    let mut cleaned = false;
    for _ in 0..60 {
        post = be
            .token_contract_state(&token_contract)
            .await
            .expect("read post-reclaim TC state");
        match &post {
            None => {
                cleaned = true;
                break;
            }
            Some(state) => {
                let funded = state["funded"].as_bool().unwrap_or(false);
                let opened = state["opened"].as_bool().unwrap_or(false);
                let frozen = json_u128_field(state, "frozen").unwrap_or(0);
                let deposit = json_u128_field(state, "deposit").unwrap_or(0);
                if !funded && !opened && frozen == 0 && deposit == 0 {
                    cleaned = true;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(
        cleaned,
        "post-reclaim TC must clear funded/opened/frozen/deposit; last_state={post:?}"
    );
    assert!(
        wait_resting_ask_gone(
            &be,
            &order_book,
            &frame_model,
            &manifest.model_hash,
            &token_contract_arg,
        )
        .await,
        "filled ask must not reappear after pool-only reclaim"
    );

    eprintln!(
        "issue338 pool-only reclaim by fact: order_id={} tc={} pool_note={} final_state={post:?}",
        order.order_id, token_contract_arg, buyer_addr
    );
}
