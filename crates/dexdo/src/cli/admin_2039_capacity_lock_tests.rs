//! review regression: one seller note has one atomic provision capacity boundary.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHILD_ROOT_VAR: &str = "DEXDO_TEST_2039_PROVISION_ROOT";
const CHILD_RESULT_VAR: &str = "DEXDO_TEST_2039_PROVISION_RESULT";
const CHILD_START_VAR: &str = "DEXDO_TEST_2039_PROVISION_START";
const CHILD_TEST_NAME: &str = "child_attempts_one_provision_slot_2039";
const NOTE: &str = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_CONTRACT: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn child_test_filter() -> String {
    let module = module_path!();
    let path = module
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or(module);
    format!("{path}::{CHILD_TEST_NAME}")
}

fn wait_for(path: &Path, deadline: Instant) {
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn market() -> dexdo_core::MarketManifest {
    dexdo_core::MarketManifest {
        network: "mock".to_string(),
        frame_model: "model-a".to_string(),
        model_hash: dexdo_core::model_hash_for("model-a"),
        inference_order_book: format!("0:{}", "c".repeat(64)),
        root_model: format!("0:{}", "d".repeat(64)),
        token_contract: TOKEN_CONTRACT.to_string(),
        seller_note: NOTE.to_string(),
        nonce: 1,
        price_per_tick: dexdo_core::PRICE_STEP,
        max_ticks: 4,
    }
}

fn capacity(root: &Path, deals_dir: &Path) -> crate::cli::seller::SellerDealCapacity {
    use dexdo_core::{DobParams, MockChainBackend, ProtocolConsts};

    let chain = std::sync::Arc::new(MockChainBackend::new(
        root.join(format!("mock-{}.json", std::process::id())),
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    ));
    tokio::runtime::Runtime::new()
        .expect("provision child runtime")
        .block_on(crate::cli::seller::seller_deal_capacity(
            Some(deals_dir),
            NOTE,
            |_| Ok(chain.clone() as std::sync::Arc<dyn dexdo_core::ChainBackend>),
        ))
        .expect("count durable provision capacity")
}

/// A real child process takes the production seller-pool lock and rechecks the real durable deal
/// records. The winner models an accepted deploy POST whose response is lost: it writes the exact
/// pending record production writes, then exits without clearing it while activation is delayed.
#[test]
fn child_attempts_one_provision_slot_2039() {
    let Ok(root) = std::env::var(CHILD_ROOT_VAR) else {
        return;
    };
    let result = PathBuf::from(
        std::env::var(CHILD_RESULT_VAR).expect("parent supplies the child result path"),
    );
    let start = PathBuf::from(
        std::env::var(CHILD_START_VAR).expect("parent supplies the shared start path"),
    );
    wait_for(&start, Instant::now() + Duration::from_secs(30));

    let root = PathBuf::from(root);
    let deals_dir = root.join("deals");
    let pool_dir = crate::cli::seller::seller_pool_dir(Some(&deals_dir), NOTE)
        .expect("derive the production seller-pool path");
    let outcome = match crate::cli::seller::acquire_seller_pool_lock(&pool_dir) {
        Err(error) if error.to_string().contains("lock") && error.to_string().contains("held") => {
            "locked"
        }
        Err(error) => panic!("unexpected seller-pool lock error: {error:#}"),
        Ok(_lock) => {
            let capacity = capacity(&root, &deals_dir);
            if super::ensure_provision_capacity(capacity, 1).is_err() {
                assert_eq!(
                    capacity.pending, 1,
                    "the unresolved POST must be the blocker"
                );
                "pending"
            } else {
                crate::cli::support::persist_provision_deal_handle(
                    Some(&deals_dir),
                    &market(),
                    &root.join("contracts.json"),
                    true,
                )
                .expect("persist the before-deploy reservation");
                std::thread::sleep(Duration::from_millis(250));
                "submitted_ambiguous"
            }
        }
    };
    std::fs::write(result, outcome).expect("record child provision outcome");
}

fn spawn_child(root: &Path, start: &Path, result: &Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            &child_test_filter(),
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD_ROOT_VAR, root)
        .env(CHILD_START_VAR, start)
        .env(CHILD_RESULT_VAR, result)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn concurrent provision child")
}

fn wait_child(child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("poll provision child") {
            assert!(status.success(), "provision child failed: {status:?}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("concurrent provision child did not finish within 30s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn concurrent_provision_processes_cross_only_one_slot_at_limit_one_2039() {
    let root = tempfile::tempdir().expect("concurrent provision directory");
    let start = root.path().join("start");
    let first_result = root.path().join("first-result");
    let second_result = root.path().join("second-result");
    let mut first = spawn_child(root.path(), &start, &first_result);
    let mut second = spawn_child(root.path(), &start, &second_result);
    std::fs::write(&start, "go").expect("release both provision children");

    wait_child(&mut first);
    wait_child(&mut second);

    let outcomes = [first_result, second_result]
        .map(|path| std::fs::read_to_string(path).expect("child recorded its provision outcome"));
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| *outcome == "submitted_ambiguous")
            .count(),
        1,
        "exactly one concurrent process may cross the max_open_deals=1 boundary: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome == "locked" || outcome == "pending"),
        "the other process must fail closed on the shared lock or the refreshed capacity: {outcomes:?}"
    );

    // Both original processes are gone, so the advisory lock is definitely released. A fresh
    // process still cannot cross: the accepted/lost POST remains a durable occupied slot while the
    // expected TokenContract is deliberately unreadable (delayed activation).
    let retry_result = root.path().join("retry-result");
    let mut retry = spawn_child(root.path(), &start, &retry_result);
    wait_child(&mut retry);
    assert_eq!(
        std::fs::read_to_string(retry_result).unwrap(),
        "pending",
        "a process restart must not turn an ambiguous deploy into a fresh paid TokenContract"
    );
}

#[test]
fn custom_deals_dir_shares_the_seller_note_lock_with_provision_2039() {
    let root = tempfile::tempdir().expect("custom deals directory");
    let custom = root.path().join("custom-deals");
    let seller_pool = crate::cli::seller::seller_pool_dir(Some(&custom), NOTE)
        .expect("seller derives its custom pool directory");
    let _seller_lock = crate::cli::seller::acquire_seller_pool_lock(&seller_pool)
        .expect("seller holds the custom note lock");
    let provision_pool = crate::cli::seller::seller_pool_dir(Some(&custom), NOTE)
        .expect("provision derives its custom pool directory");
    assert_eq!(seller_pool, provision_pool);
    let error = match crate::cli::seller::acquire_seller_pool_lock(&provision_pool) {
        Ok(_) => panic!("standalone provision must contend with the custom-dir seller lock"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("already running or being provisioned"),
        "{error:#}"
    );

    let source = include_str!("admin.rs");
    let body = source
        .split_once("fn run_provision_with_deal_gas_overhead")
        .expect("provision implementation present")
        .1
        .split_once("#[cfg(test)]")
        .expect("provision implementation has a bounded source section")
        .0;
    let compact = body.split_whitespace().collect::<String>();
    assert!(compact.contains("seller_pool_dir(args.deals_dir.as_deref(),&note_addr)"));
    assert!(compact.contains("seller_deal_capacity(args.deals_dir.as_deref(),&note_addr"));
    assert!(compact.contains("persist_provision_deal_handle(args.deals_dir.as_deref()"));
    assert!(compact.contains("clear_provision_pending_if_present(args.deals_dir.as_deref()"));
}

#[test]
fn ambiguous_residual_submit_blocks_restart_before_a_second_post_2039() {
    let root = tempfile::tempdir().expect("ambiguous residual restart directory");
    let start = root.path().join("start");
    let result = root.path().join("result");
    let mut submitter = spawn_child(root.path(), &start, &result);
    std::fs::write(&start, "go").expect("release ambiguous residual submit child");
    wait_child(&mut submitter);
    assert_eq!(
        std::fs::read_to_string(result).unwrap(),
        "submitted_ambiguous",
        "the first process must exit with the deploy outcome unresolved"
    );

    let deals_dir = root.path().join("deals");
    let error = tokio::runtime::Runtime::new()
        .expect("residual restart runtime")
        .block_on(
            crate::cli::support::ensure_no_unresolved_provision_reservation(
                Some(&deals_dir),
                TOKEN_CONTRACT,
                std::future::ready(Ok(false)),
            ),
        )
        .expect_err("a restart must not repeat an unresolved residual deploy");
    assert!(
        error.to_string().contains("refusing to submit it again"),
        "{error:#}"
    );
    assert_eq!(
        capacity(root.path(), &deals_dir).pending,
        1,
        "the residual reservation must also remain visible to note-wide capacity"
    );
}

#[test]
fn residual_money_path_uses_the_durable_before_submit_boundary_2039() {
    let source = include_str!("support.rs");
    let body = source
        .split_once("pub(crate) async fn provision_replacement_seller_with_deal_gas_overhead")
        .expect("residual provision implementation exists")
        .1
        .split_once("pub(crate) fn buyer_real_backend")
        .expect("residual provision implementation has a bounded source section")
        .0;
    let compact = body.split_whitespace().collect::<String>();

    let restart_guard = compact
        .find("ensure_no_unresolved_provision_reservation(")
        .expect("residual restart checks the durable reservation");
    let provision = compact
        .find("provision_market_with_deal_gas_overhead_and_before_deploy(")
        .expect("residual provision uses the before-submit callback");
    let reservation = compact
        .find("persist_provision_deal_handle(args.deals_dir.as_deref(),market,&manifest_path_path,true")
        .expect("residual deploy persists its pending handle in the callback");
    let reconciliation = compact
        .find("provision_failure_clears_pending(&error,false)")
        .expect("residual failure clears pending only for a proven pre-submit outcome");

    assert!(restart_guard < provision && provision < reservation);
    assert!(reservation < reconciliation);
}
