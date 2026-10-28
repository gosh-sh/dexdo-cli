use super::*;
use crate::buyer::usage::{AccrualFact, UsageFactsQuery, UsageQuery, UsageStore};
use dexdo_core::params::{SHELL_UNIT, TICK_SIZE};
use serde_json::{json, Value};

struct Harness {
    state: ApiState,
    addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start(state: ApiState) -> Self {
        let (shutdown, receiver) = tokio::sync::oneshot::channel();
        let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), state.clone(), async {
            let _ = receiver.await;
        })
        .await
        .unwrap();
        Self {
            state,
            addr,
            shutdown,
            task,
        }
    }

    async fn usage(&self) -> (reqwest::StatusCode, Value) {
        let response = reqwest::get(format!("http://{}/v1/usage", self.addr))
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn assert_quota(&self, limit: u64, used: u64, remaining: u64) {
        let (status, body) = self.usage().await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(
            body["quota"],
            json!({"limit":limit,"used":used,"remaining":remaining,"unit":"SHELL"})
        );
        assert_eq!(body["remaining"], remaining);
    }

    async fn assert_unavailable(&self) {
        let (status, body) = self.usage().await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["type"], "usage_unavailable");
    }

    async fn serving_check(&self) -> Value {
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", self.addr))
            .json(&json!({"model":"dexdo-mock", "messages":[{"role":"user","content":""}], "max_tokens":1,"stream":false}))
            .send().await.unwrap();
        assert!(!response.status().is_success());
        response.json().await.unwrap()
    }

    async fn finish(self) {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap();
    }
}

fn purchase(
    buyer: &Buyer,
    id: &str,
    ticks: u128,
    price: u128,
) -> (ApiDeal, Arc<RecordingSettleChain>) {
    let chain = Arc::new(RecordingSettleChain::default());
    chain.set_deal_state(opened_deal_state(ticks * price, unix_now_secs()));
    *chain.purchase_terms.lock().unwrap() = Some((ticks, price));
    let session = Arc::new(SessionSettle::new_with_failure_policy_and_lifetime(
        chain.clone(),
        id.into(),
        buyer.note.clone(),
        BuyerApiFailurePolicy::default(),
        SessionLifetimePolicy::Preserve,
    ));
    session.disable_drop_backup();
    let deal = ApiDeal::new(
        Route {
            handover: Handover {
                endpoint: "https://127.0.0.1:1".into(),
                tls_fingerprint: "00".repeat(32),
            },
            token_contract: id.into(),
            max_tokens: u64::try_from(dexdo_core::TICK_SIZE).unwrap(),
        },
        session,
        Arc::new(ContentGate::skip()),
    );
    (deal, chain)
}

fn record_work_and_accrual(store: &UsageStore, id: &str, price_raw: u128) {
    store.register_deal(id, "dexdo-mock").unwrap();
    let spent_raw = 30 * SHELL_UNIT;
    let request_id = format!("request:{id}");
    store
        .start_request(&request_id, "dexdo-mock", "openai", false)
        .unwrap();
    store.link_deal(&request_id, id).unwrap();
    // Seed accepted work independently of payment. Price publication remains owned by
    // the purchase/journal path under test, including when its getter or SQLite write fails.
    let charged_tokens = u64::try_from(spent_raw * TICK_SIZE / price_raw).unwrap();
    store
        .record_call_usage(
            &request_id,
            "answer",
            "answer",
            BillingUsage::new(charged_tokens, 0, None).unwrap(),
            true,
        )
        .unwrap();
    store
        .finish_request(&request_id, 200, Some("stop"), None)
        .unwrap();
    store
        .apply_deal_expenses(
            id,
            &[AccrualFact {
                source_id: format!("accrual:{id}"),
                occurred_at: 1,
                spent_raw,
            }],
            None,
        )
        .unwrap();
}

fn buyer() -> Arc<Buyer> {
    Arc::new(Buyer::from_note(
        Arc::new(dexdo_core::LocalNote::generate()),
    ))
}

fn facts(store: &UsageStore) -> Value {
    serde_json::to_value(store.facts(&UsageFactsQuery::default()).unwrap()).unwrap()
}

fn state(buyer: Arc<Buyer>, deal: ApiDeal, store: Arc<UsageStore>) -> ApiState {
    ApiState::single_deal(buyer, "dexdo-mock".into(), deal)
        .with_mock_model(true)
        .with_usage(store)
}

fn read_only_state(store: Arc<UsageStore>) -> ApiState {
    ApiState::lazy(
        buyer(),
        "dexdo-mock".into(),
        Arc::new(|| Box::pin(async { panic!("GET usage must never BUY") })),
        Duration::from_secs(1),
    )
    .with_usage(store)
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn observed_external_terminal_closes_quota_before_financial_reconciliation() {
    for disappeared in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
        let buyer = buyer();
        let (deal, chain) = purchase(&buyer, "A", 100, SHELL_UNIT);
        let session = deal.session.clone();
        record_work_and_accrual(&store, "A", SHELL_UNIT);
        let before = facts(&store);
        let harness = Harness::start(state(buyer, deal, store.clone())).await;
        harness.assert_quota(100, 30, 70).await;
        *chain.deal_state.lock().unwrap() = if disappeared {
            None
        } else {
            Some(never_opened_deal_state(0, unix_now_secs()))
        };
        observe_deal(&harness.state.deals).await;
        let body = harness.serving_check().await;
        assert_eq!(body["error"]["code"], "deal_session_closed", "{body}");
        assert!(!session.is_settled(), "no financial reconciliation yet");
        harness.assert_quota(100, 30, 0).await;
        assert_eq!(
            facts(&store),
            before,
            "terminal observation adds no expense facts"
        );
        let reopened = UsageStore::open(&path, "shellnet").unwrap();
        assert_eq!(
            reopened
                .query(&UsageQuery::default(), chrono::Utc::now())
                .unwrap()["quota"]["remaining"],
            "0"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        harness.finish().await;
    }
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn transport_failure_keeps_active_quota_and_late_old_terminal_cannot_close_new_purchase() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
    let buyer = buyer();
    let (old, chain) = purchase(&buyer, "A", 100, SHELL_UNIT);
    let old_session = old.session.clone();
    record_work_and_accrual(&store, "A", SHELL_UNIT);
    let harness = Harness::start(state(buyer.clone(), old, store)).await;
    chain.transient_deal_state.store(true, Ordering::SeqCst);
    observe_deal(&harness.state.deals).await;
    harness.assert_quota(100, 30, 70).await;
    assert!(!old_session.is_closed());
    chain.transient_deal_state.store(false, Ordering::SeqCst);
    let (new, _) = purchase(&buyer, "B", 80, 2 * SHELL_UNIT);
    harness
        .state
        .deals
        .replace_active(|| new, "quota-review")
        .await
        .unwrap();
    chain.set_deal_state(never_opened_deal_state(0, unix_now_secs()));
    assert!(old_session.ensure_open_for_serving().await.is_err());
    harness.assert_quota(160, 0, 160).await;
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn failed_sqlite_publication_recovers_actual_terms_after_contract_disappears_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    let buyer = buyer();
    let (old, _) = purchase(&buyer, "A", 100, SHELL_UNIT);
    record_work_and_accrual(&store, "A", SHELL_UNIT);
    let harness = Harness::start(state(buyer.clone(), old, store.clone())).await;
    harness.assert_quota(100, 30, 70).await;
    let faults = rusqlite::Connection::open(&path).unwrap();
    faults
        .execute_batch(
            "CREATE TRIGGER fail_quota BEFORE INSERT ON deal_quota
        WHEN NEW.deal_id='B' BEGIN SELECT RAISE(FAIL,'injected quota write failure'); END;",
        )
        .unwrap();
    let (new, chain) = purchase(&buyer, "B", 80, 2 * SHELL_UNIT);
    record_work_and_accrual(&store, "B", 2 * SHELL_UNIT);
    let before = facts(&store);
    let publication = harness
        .state
        .deals
        .replace_active(|| new, "quota-review")
        .await;
    assert_eq!(
        harness
            .state
            .deals
            .current()
            .await
            .unwrap()
            .route
            .token_contract,
        "B"
    );
    harness.assert_unavailable().await;
    *chain.deal_state.lock().unwrap() = None;
    observe_deal(&harness.state.deals).await;
    assert_eq!(
        harness.serving_check().await["error"]["code"],
        "deal_session_closed"
    );
    harness.assert_unavailable().await;
    assert!(publication.is_err());
    harness.finish().await;
    drop(store);
    faults.execute_batch("DROP TRIGGER fail_quota;").unwrap();
    drop(faults);
    // Restart with no active route and no contract getter: the original B terms must be durable.
    let reopened = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    assert_eq!(
        facts(&reopened),
        before,
        "history and financial completeness survive quota recovery"
    );
    let history = reopened
        .query(&UsageQuery::default(), chrono::Utc::now())
        .unwrap();
    assert_eq!(history["spend"]["total"]["actual_cost_shell"], "60");
    let restarted = Harness::start(read_only_state(reopened)).await;
    restarted.assert_quota(160, 30, 0).await;
    restarted.finish().await;
}

#[tokio::test]
async fn failed_terms_getter_never_reports_previous_or_invented_purchase_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    let buyer = buyer();
    let (old, _) = purchase(&buyer, "A", 100, SHELL_UNIT);
    let harness = Harness::start(state(buyer.clone(), old, store.clone())).await;
    let (new, chain) = purchase(&buyer, "B", 80, 2 * SHELL_UNIT);
    chain.fail_purchase_terms.store(true, Ordering::SeqCst);
    assert!(harness
        .state
        .deals
        .replace_active(|| new, "quota-review")
        .await
        .is_err());
    harness.assert_unavailable().await;
    *chain.deal_state.lock().unwrap() = None;
    harness.assert_unavailable().await;
    harness.finish().await;
    drop(store);
    let reopened = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    let restarted = Harness::start(read_only_state(reopened)).await;
    restarted.assert_unavailable().await;
    restarted.finish().await;
}

#[tokio::test]
async fn failed_terms_getter_retries_same_purchase_without_buy_or_history_reset() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
    let buyer = buyer();
    let (old, _) = purchase(&buyer, "A", 100, SHELL_UNIT);
    let harness = Harness::start(state(buyer.clone(), old, store.clone())).await;
    let (new, chain) = purchase(&buyer, "B", 80, 2 * SHELL_UNIT);
    record_work_and_accrual(&store, "B", 2 * SHELL_UNIT);
    chain.fail_purchase_terms.store(true, Ordering::SeqCst);
    assert!(harness
        .state
        .deals
        .replace_active(|| new, "quota-review")
        .await
        .is_err());
    harness.assert_unavailable().await;
    chain.fail_purchase_terms.store(false, Ordering::SeqCst);
    harness.assert_quota(160, 30, 130).await;
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
}

async fn initial_harness(store: Arc<UsageStore>, previous: bool) -> Harness {
    if previous {
        let buyer = buyer();
        let (old, _) = purchase(&buyer, "A", 100, SHELL_UNIT);
        record_work_and_accrual(&store, "A", SHELL_UNIT);
        let harness = Harness::start(state(buyer, old, store)).await;
        harness.assert_quota(100, 30, 70).await;
        harness
    } else {
        let harness = Harness::start(read_only_state(store)).await;
        harness.assert_quota(0, 0, 0).await;
        harness
    }
}

async fn early_state_failure_after_restart(previous: bool) {
    for disappeared in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
        let harness = initial_harness(store.clone(), previous).await;
        let (new, chain) = purchase(&harness.state.buyer, "B", 80, 2 * SHELL_UNIT);
        record_work_and_accrual(&store, "B", 2 * SHELL_UNIT);
        let before = facts(&store);
        chain.transient_deal_state.store(true, Ordering::SeqCst);
        assert!(harness
            .state
            .deals
            .replace_active(|| new, "quota-review")
            .await
            .is_err());
        harness.assert_unavailable().await;
        chain.transient_deal_state.store(false, Ordering::SeqCst);
        *chain.deal_state.lock().unwrap() = if disappeared {
            None
        } else {
            Some(never_opened_deal_state(0, unix_now_secs()))
        };
        harness.assert_unavailable().await;
        assert_eq!(facts(&store), before);
        harness.finish().await;
        drop(store);
        let reopened = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
        assert_eq!(facts(&reopened), before);
        let restarted = Harness::start(read_only_state(reopened)).await;
        // B opened but its first state read failed before its actual terms were obtained.
        // Neither A nor a fabricated zero is a successful answer for this purchase.
        restarted.assert_unavailable().await;
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        restarted.finish().await;
    }
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn early_state_failure_does_not_restore_previous_purchase_after_restart() {
    early_state_failure_after_restart(true).await;
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn early_state_failure_does_not_invent_zero_purchase_after_restart() {
    early_state_failure_after_restart(false).await;
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn early_state_failure_retries_same_lazy_purchase_without_buy_or_history_reset() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
    let buyer = buyer();
    let (new, chain) = purchase(&buyer, "B", 80, 2 * SHELL_UNIT);
    let expected_session = new.session.clone();
    record_work_and_accrual(&store, "B", 2 * SHELL_UNIT);
    let before = facts(&store);
    let initializations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = initializations.clone();
    let state = ApiState::lazy(
        buyer,
        "dexdo-mock".into(),
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            let new = new.clone();
            Box::pin(async move { Ok(new) })
        }),
        Duration::from_secs(1),
    )
    .with_mock_model(true)
    .with_usage(store.clone());
    let harness = Harness::start(state).await;
    assert_eq!(initializations.load(Ordering::SeqCst), 0);
    chain.transient_deal_state.store(true, Ordering::SeqCst);
    let _ = harness.serving_check().await;
    harness.assert_unavailable().await;
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    chain.transient_deal_state.store(false, Ordering::SeqCst);
    harness.assert_quota(160, 30, 130).await;
    let recovered = harness.state.current_deal().await.unwrap();
    assert!(Arc::ptr_eq(&recovered.session, &expected_session));
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    assert_eq!(facts(&store), before);
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn unopened_candidate_preserves_previous_quota_and_history_after_restart() {
    for previous in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
        let harness = initial_harness(store.clone(), previous).await;
        let (new, chain) = purchase(&harness.state.buyer, "unopened", 80, 2 * SHELL_UNIT);
        chain.set_deal_state(never_opened_deal_state(160 * SHELL_UNIT, unix_now_secs()));
        let before = facts(&store);
        assert!(harness
            .state
            .deals
            .replace_active(|| new, "quota-review")
            .await
            .is_err());
        for _ in 0..2 {
            if previous {
                harness.assert_quota(100, 30, 70).await;
            } else {
                harness.assert_quota(0, 0, 0).await;
            }
            assert_eq!(facts(&store), before);
        }
        let _ = harness.serving_check().await;
        assert_eq!(facts(&store), before);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        harness.finish().await;
        drop(store);
        let reopened = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
        assert_eq!(facts(&reopened), before);
        let restarted = Harness::start(read_only_state(reopened)).await;
        if previous {
            restarted.assert_quota(100, 30, 70).await;
        } else {
            restarted.assert_quota(0, 0, 0).await;
        }
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        restarted.finish().await;
    }
}

#[tokio::test]
async fn unopened_candidate_does_not_clear_an_earlier_unresolved_purchase() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    let harness = initial_harness(store.clone(), true).await;
    let (opened, opened_chain) = purchase(&harness.state.buyer, "B", 80, 2 * SHELL_UNIT);
    opened_chain
        .transient_deal_state
        .store(true, Ordering::SeqCst);
    assert!(harness
        .state
        .deals
        .replace_active(|| opened, "quota-review")
        .await
        .is_err());
    let (unopened, unopened_chain) = purchase(&harness.state.buyer, "C", 80, 2 * SHELL_UNIT);
    unopened_chain.set_deal_state(never_opened_deal_state(160 * SHELL_UNIT, unix_now_secs()));
    let before = facts(&store);
    assert!(harness
        .state
        .deals
        .replace_active(|| unopened, "quota-review")
        .await
        .is_err());
    harness.assert_unavailable().await;
    assert_eq!(facts(&store), before);
    assert_eq!(opened_chain.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(unopened_chain.stop_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
    drop(store);
    let reopened = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    assert_eq!(facts(&reopened), before);
    let restarted = Harness::start(read_only_state(reopened)).await;
    restarted.assert_unavailable().await;
    restarted.finish().await;
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn unopened_candidate_switches_to_actual_terms_when_it_opens() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
    let harness = initial_harness(store.clone(), true).await;
    let (new, chain) = purchase(&harness.state.buyer, "B", 80, 2 * SHELL_UNIT);
    let expected_session = new.session.clone();
    chain.set_deal_state(never_opened_deal_state(160 * SHELL_UNIT, unix_now_secs()));
    let before = facts(&store);
    assert!(harness
        .state
        .deals
        .replace_active(|| new, "quota-review")
        .await
        .is_err());
    harness.assert_quota(100, 30, 70).await;
    let _ = harness.serving_check().await;
    assert_eq!(facts(&store), before);
    chain.set_deal_state(opened_deal_state(160 * SHELL_UNIT, unix_now_secs()));
    record_work_and_accrual(&store, "B", 2 * SHELL_UNIT);
    let opened_facts = facts(&store);
    harness.assert_quota(160, 30, 130).await;
    let recovered = harness.state.current_deal().await.unwrap();
    assert!(Arc::ptr_eq(&recovered.session, &expected_session));
    recovered.session.ensure_open_for_serving().await.unwrap();
    assert_eq!(facts(&store), opened_facts);
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
}

/// E2E-ROW: E2E-USAGE-03/L0
#[tokio::test]
async fn unopened_usage_does_not_hide_storage_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    let harness = initial_harness(store.clone(), true).await;
    let (new, chain) = purchase(&harness.state.buyer, "B", 80, 2 * SHELL_UNIT);
    chain.set_deal_state(never_opened_deal_state(160 * SHELL_UNIT, unix_now_secs()));
    let before = facts(&store);
    assert!(harness
        .state
        .deals
        .replace_active(|| new, "quota-review")
        .await
        .is_err());
    let faults = rusqlite::Connection::open(&path).unwrap();
    faults.execute_batch("DROP TABLE deal_quota;").unwrap();
    harness.assert_unavailable().await;
    assert_eq!(facts(&store), before);
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
    drop(faults);
    drop(store);
    let reopened = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    assert_eq!(facts(&reopened), before);
    let restarted = Harness::start(read_only_state(reopened)).await;
    restarted.assert_quota(100, 30, 70).await;
    restarted.finish().await;
}
