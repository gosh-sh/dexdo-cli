use super::*;
use crate::buyer::usage::{UsageQuery, UsageStore};
use chrono::Utc;
use tower::ServiceExt;

struct ReferenceKey {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl ReferenceKey {
    fn set(name: &'static str) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, "fixture-key");
        Self { name, previous }
    }
}

impl Drop for ReferenceKey {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

fn inference_request() -> http::Request<Body> {
    http::Request::post("/v1/chat/completions")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({
                "model":"dexdo-mock", "messages":[{"role":"user","content":"answer"}],
                "max_tokens":8, "stream":false
            })
            .to_string(),
        ))
        .unwrap()
}

fn snapshot(store: &UsageStore) -> serde_json::Value {
    store.query(&UsageQuery::default(), Utc::now()).unwrap()
}

fn assert_reopen_preserves_history(dir: &std::path::Path, store: Arc<UsageStore>) {
    let now = Utc::now();
    let query = UsageQuery::default();
    let expected = store.query(&query, now).unwrap();
    assert_eq!(store.query(&query, now).unwrap(), expected);
    drop(store);
    let reopened = UsageStore::open(dir.join("usage.sqlite3"), "shellnet").unwrap();
    assert_eq!(reopened.query(&query, now).unwrap(), expected);
}

async fn fault_harness(key: &'static str) -> WeeklyRouteHarness {
    weekly_route_harness_gated(
        0,
        true,
        SUB_WEEK_LEN.as_secs(),
        1,
        ContentGate::probe(
            "dexdo-mock".into(),
            probe_models("identity probe", "http://127.0.0.1:1/v1", key),
        ),
        crate::seller::UpstreamConfig::Mock,
    )
    .await
}

#[tokio::test]
async fn b8_and_b7_transport_failures_preserve_observations_without_accepting_payment() {
    const KEY: &str = "DEXDO_USAGE_HISTORY_FAULT_REFERENCE_KEY";
    let _key = ReferenceKey::set(KEY);
    for b7 in [false, true] {
        for after_terminal in [false, true] {
            let harness = fault_harness(KEY).await;
            harness.seller.set_probe_output_fixture(
                b7,
                if after_terminal {
                    "DEXDO_FIXTURE_USAGE_THEN_ERROR"
                } else {
                    "DEXDO_FIXTURE_ERROR_BEFORE_USAGE"
                },
            );
            let dir = tempfile::tempdir().unwrap();
            let store =
                Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let state = direct_state_with_deliveries(&harness, sender).with_usage(store.clone());
            let response = router(state).oneshot(inference_request()).await.unwrap();
            assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(String::from_utf8(body.to_vec())
                .unwrap()
                .contains("fixture transport error"));
            let result = snapshot(&store);
            let verification = &result["usage"]["total"]["verification"];
            let paid = if b7 { 3 } else { 0 };
            let observed = if after_terminal {
                if b7 {
                    14
                } else {
                    4
                }
            } else {
                paid
            };
            assert_eq!(
                verification["observed_total_tokens"], observed,
                "B7={b7} after_terminal={after_terminal}: {result}"
            );
            assert_eq!(verification["complete"], false);
            assert!(verification["total_tokens"].is_null());
            assert!(verification["buyer_charged_tokens"].is_null());
            assert_eq!(
                harness.delivered().await,
                paid,
                "unaccepted usage must not change route billing"
            );
            assert_eq!(result["usage"]["total"]["requests"]["failed"], 1);
            assert_eq!(
                result["usage"]["total"]["answer"]["observed_total_tokens"],
                0
            );
            assert_reopen_preserves_history(dir.path(), store);
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn b8_and_b7_cancellation_before_or_after_terminal_remains_incomplete_after_reopen() {
    const KEY: &str = "DEXDO_USAGE_HISTORY_CANCEL_REFERENCE_KEY";
    let _key = ReferenceKey::set(KEY);
    for b7 in [false, true] {
        for after_terminal in [false, true] {
            let harness = fault_harness(KEY).await;
            harness.seller.set_probe_output_fixture(
                b7,
                if after_terminal {
                    "DEXDO_FIXTURE_DELAY_EOF"
                } else {
                    "DEXDO_FIXTURE_HOLD_AFTER_CONTENT"
                },
            );
            let dir = tempfile::tempdir().unwrap();
            let store =
                Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let state = direct_state_with_deliveries(&harness, sender).with_usage(store.clone());
            let deal = harness.deals.current().await.unwrap();
            let request = tokio::spawn(router(state).oneshot(inference_request()));
            let paid = if b7 { 3 } else { 0 };
            let observed = if after_terminal {
                if b7 {
                    14
                } else {
                    4
                }
            } else {
                paid
            };
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let result = snapshot(&store);
                    let started = result["usage"]["total"]["verification"]["complete"] == false;
                    let seen = if after_terminal {
                        result["usage"]["total"]["verification"]["observed_total_tokens"]
                            == observed
                    } else {
                        deal.session.route_delivered_tokens().unwrap_or(0) > u64::from(b7)
                    };
                    if started && seen {
                        break;
                    }
                    assert!(
                        !request.is_finished(),
                        "probe ended before the fixture's held boundary"
                    );
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("probe must reach its observable held boundary");
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            harness.seller.release_delayed_eof();
            let result = snapshot(&store);
            let verification = &result["usage"]["total"]["verification"];
            assert_eq!(result["usage"]["total"]["requests"]["cancelled"], 1);
            assert_eq!(verification["complete"], false);
            assert!(verification["total_tokens"].is_null());
            assert_eq!(
                verification["observed_total_tokens"], observed,
                "B7={b7} after_terminal={after_terminal}: {result}"
            );
            assert_eq!(harness.delivered().await, paid);
            assert_reopen_preserves_history(dir.path(), store);
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn lost_phase_and_marker_writes_keep_b8_and_b7_unknown_after_recovery() {
    const KEY: &str = "DEXDO_USAGE_HISTORY_STORAGE_REFERENCE_KEY";
    let _key = ReferenceKey::set(KEY);
    for b7 in [false, true] {
        for terminal_preserved in [false, true] {
            let harness = fault_harness(KEY).await;
            harness.seller.set_probe_output_fixture(
                b7,
                if terminal_preserved {
                    "DEXDO_FIXTURE_DELAY_EOF"
                } else {
                    "DEXDO_FIXTURE_HOLD_AFTER_CONTENT"
                },
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("usage.sqlite3");
            let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
            let faults = rusqlite::Connection::open(&path).unwrap();
            let phase = if b7 {
                "verification_b7"
            } else {
                "verification_b8"
            };
            // Lose the call-start row and, in one case, every usage row for the phase.
            // Earlier B8 evidence and any successfully persisted terminal must survive.
            faults
                .execute_batch(&format!(
                    "CREATE TRIGGER lose_phase_history BEFORE INSERT ON request_usage
                 WHEN NEW.phase='{phase}' AND ({} OR NEW.call_id='in_progress')
                 BEGIN SELECT RAISE(FAIL,'fixture phase history write failed'); END;
                 CREATE TRIGGER lose_incomplete_marker BEFORE UPDATE OF history_complete ON requests
                 BEGIN SELECT RAISE(FAIL,'fixture history marker failed'); END;",
                    u8::from(!terminal_preserved),
                ))
                .unwrap();
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let state = direct_state_with_deliveries(&harness, sender).with_usage(store.clone());
            let deal = harness.deals.current().await.unwrap();
            let request = tokio::spawn(router(state).oneshot(inference_request()));
            let paid = if b7 { 3 } else { 0 };
            let observed = if terminal_preserved {
                if b7 {
                    14
                } else {
                    4
                }
            } else {
                paid
            };
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let seen = if terminal_preserved {
                        snapshot(&store)["usage"]["total"]["verification"]["observed_total_tokens"]
                            == observed
                    } else {
                        deal.session.route_delivered_tokens().unwrap_or(0) > u64::from(b7)
                    };
                    if seen {
                        break;
                    }
                    assert!(
                        !request.is_finished(),
                        "probe must remain at its held boundary"
                    );
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("probe must reach its observable held boundary");
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            harness.seller.release_delayed_eof();
            let before = snapshot(&store);
            assert_eq!(before["coverage"]["unfinished_requests"], 1);
            assert_eq!(before["usage"]["total"]["requests"]["in_progress"], 1);
            assert_eq!(
                harness.delivered().await,
                paid,
                "observation must not accept payment"
            );
            faults
                .execute_batch(
                    "DROP TRIGGER lose_phase_history; DROP TRIGGER lose_incomplete_marker;",
                )
                .unwrap();
            drop(faults);
            drop(store);
            let recovered = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
            assert_eq!(recovered.recover_interrupted_requests().unwrap(), 1);
            assert_eq!(recovered.recover_interrupted_requests().unwrap(), 0);
            let result = snapshot(&recovered);
            assert_eq!(result["coverage"]["unfinished_requests"], 1);
            assert_eq!(result["usage"]["total"]["requests"]["interrupted"], 1);
            for snapshot in [&before, &result] {
                let verification = &snapshot["usage"]["total"]["verification"];
                assert_eq!(
                    verification["observed_total_tokens"], observed,
                    "B7={b7} terminal_preserved={terminal_preserved}: {snapshot}"
                );
                assert_eq!(
                    verification["complete"], false,
                    "lost phase/marker evidence must stay unknown before and after recovery"
                );
                for field in [
                    "input_tokens",
                    "output_tokens",
                    "total_tokens",
                    "buyer_charged_tokens",
                ] {
                    assert!(verification[field].is_null(), "{field}: {snapshot}");
                }
            }
            assert_reopen_preserves_history(dir.path(), recovered);
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn skipped_and_cached_verification_keep_complete_zero_for_unperformed_calls() {
    for skip in [true, false] {
        let gate = if skip {
            ContentGate::skip()
        } else {
            ContentGate::probe(
                "dexdo-mock".into(),
                probe_models(
                    "identity probe",
                    "http://127.0.0.1:1/v1",
                    "DEXDO_USAGE_HISTORY_ABSENT_REFERENCE_KEY",
                ),
            )
        };
        let harness = weekly_route_harness_gated(
            0,
            true,
            SUB_WEEK_LEN.as_secs(),
            1,
            gate,
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let state = direct_state_with_deliveries(&harness, sender).with_usage(store.clone());
        for _ in 0..2 {
            let response = router(state.clone())
                .oneshot(inference_request())
                .await
                .unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
        }
        drop(state);
        let result = snapshot(&store);
        assert_eq!(result["usage"]["total"]["requests"]["succeeded"], 2);
        assert_eq!(result["usage"]["total"]["verification"]["complete"], true);
        assert_eq!(
            result["usage"]["total"]["verification"]["total_tokens"],
            if skip { 0 } else { 3 }
        );
        assert_reopen_preserves_history(dir.path(), store);
        harness.shutdown().await;
    }
}

#[tokio::test]
async fn review_2146_uncertain_verification_output_blocks_resume() {
    const KEY: &str = "DEXDO_REVIEW_2146_REFERENCE";
    let _key = ReferenceKey::set(KEY);
    for b7 in [false, true] {
        for reasoning in [false, true] {
            let harness = fault_harness(KEY).await;
            harness.seller.set_probe_output_fixture(
                b7,
                if reasoning {
                    "DEXDO_FIXTURE_ERROR_BEFORE_USAGE DEXDO_FIXTURE_REASONING_ONLY"
                } else {
                    "DEXDO_FIXTURE_ERROR_BEFORE_USAGE"
                },
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("usage.sqlite3");
            let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let state = direct_state_with_deliveries(&harness, sender).with_usage(store.clone());
            let response = router(state).oneshot(inference_request()).await.unwrap();
            assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(String::from_utf8(body.to_vec())
                .unwrap()
                .contains("fixture transport error"));
            assert_eq!(harness.delivered().await, if b7 { 3 } else { 0 });
            let deal = harness.deals.current().await.unwrap();
            assert!(
                matches!(deal.session.ensure_open_for_serving().await,Err(error) if error.code == ConsumerErrorCode::UpstreamUsageUnresolved)
            );
            let calls = harness.seller.calls();
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let response =
                router(direct_state_with_deliveries(&harness, sender).with_usage(store.clone()))
                    .oneshot(inference_request())
                    .await
                    .unwrap();
            assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(harness.seller.calls(), calls);
            assert!(!deal.session.settle_on_exit("shutdown").await.unwrap());
            assert_eq!(harness.chain.foreign_money_calls(), 0);
            let reopened = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
            reopened.recover_interrupted_requests().unwrap();
            let resumed = SessionSettle::new(
                harness.chain.clone(),
                deal.route.token_contract.clone(),
                deal.session.note.clone(),
            );
            resumed.usage_quota.set(reopened.clone()).unwrap();
            assert!(
                matches!(resumed.ensure_open_for_serving().await,Err(error) if error.code == ConsumerErrorCode::UpstreamUsageUnresolved)
            );
            assert!(!resumed.settle_on_exit("shutdown").await.unwrap());
            let phase = if b7 {
                "verification_b7"
            } else {
                "verification_b8"
            };
            let connection = rusqlite::Connection::open(&path).unwrap();
            let request: String = connection.query_row("SELECT request_id FROM request_usage WHERE phase=?1 AND call_id='in_progress' AND accepted=0",[phase],|row| row.get(0)).unwrap();
            reopened
                .record_call_usage(
                    &request,
                    "answer",
                    "answer",
                    BillingUsage::new(1, 1, None).unwrap(),
                    true,
                )
                .unwrap();
            assert!(
                reopened
                    .has_unresolved_output_usage(&deal.route.token_contract)
                    .unwrap(),
                "another phase cannot clear the failed verification"
            );
            reopened
                .record_call_usage(
                    &request,
                    phase,
                    phase,
                    BillingUsage::new(1, 1, None).unwrap(),
                    false,
                )
                .unwrap();
            assert!(
                reopened
                    .has_unresolved_output_usage(&deal.route.token_contract)
                    .unwrap(),
                "observation alone is insufficient"
            );
            reopened
                .record_call_usage(
                    &request,
                    phase,
                    phase,
                    BillingUsage::new(1, 1, None).unwrap(),
                    true,
                )
                .unwrap();
            assert!(!reopened
                .has_unresolved_output_usage(&deal.route.token_contract)
                .unwrap());
            let reconciled = SessionSettle::new(
                harness.chain.clone(),
                deal.route.token_contract.clone(),
                deal.session.note.clone(),
            );
            reconciled.usage_quota.set(reopened).unwrap();
            assert!(reconciled.ensure_open_for_serving().await.is_ok());
            reconciled.disable_drop_backup();
            assert!(!deal.session.is_closed());
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn review_2146_verification_provider_refusal_without_output_allows_retry() {
    const KEY: &str = "DEXDO_REVIEW_2146_NO_OUTPUT";
    let _key = ReferenceKey::set(KEY);
    for (b7, provider_origin) in [(false, false), (false, true), (true, false), (true, true)] {
        let harness = fault_harness(KEY).await;
        harness.seller.set_probe_output_fixture(
            b7,
            if provider_origin {
                "DEXDO_FIXTURE_ERROR_BEFORE_OUTPUT"
            } else {
                "DEXDO_FIXTURE_TRANSPORT_BEFORE_OUTPUT"
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
        for _ in 0..2 {
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let response =
                router(direct_state_with_deliveries(&harness, sender).with_usage(store.clone()))
                    .oneshot(inference_request())
                    .await
                    .unwrap();
            assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
            let deal = harness.deals.current().await.unwrap();
            assert!(deal.session.ensure_open_for_serving().await.is_ok());
            assert!(!store
                .has_unresolved_output_usage(&deal.route.token_contract)
                .unwrap());
            assert!(!UsageStore::open(&path, "shellnet")
                .unwrap()
                .has_unresolved_output_usage(&deal.route.token_contract)
                .unwrap());
        }
        harness.shutdown().await;
    }
}

#[tokio::test]
async fn review_2146_pause_between_paid_b8_and_b7_preserves_b8_without_starting_b7() {
    const KEY: &str = "DEXDO_2144_BETWEEN_PHASES_REFERENCE";
    let _key = ReferenceKey::set(KEY);
    let harness = fault_harness(KEY).await;
    harness
        .seller
        .set_probe_output_fixture(false, "DEXDO_FIXTURE_DELAY_EOF");
    let deal = harness.deals.current().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let state = direct_state_with_deliveries(&harness, sender).with_usage(store.clone());
    let request = tokio::spawn(router(state).oneshot(inference_request()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if snapshot(&store)["usage"]["total"]["verification"]["observed_total_tokens"] == 4 {
                break;
            }
            assert!(
                !request.is_finished(),
                "B8 must reach its held terminal-usage boundary"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let calls = harness.seller.calls();
    let mut running = deal.begin_request(unix_now_secs());
    running.pause_unresolved_usage();
    harness.seller.release_delayed_eof();
    let response = request.await.unwrap().unwrap();
    assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8(body.to_vec())
        .unwrap()
        .contains("upstream_usage_unresolved"));
    assert_eq!(
        harness.seller.calls(),
        calls,
        "B7 and answer must not start after pause"
    );
    assert_eq!(
        deal.billable_tokens(),
        4,
        "already started B8 retains its accepted usage"
    );
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    assert!(
        deal.content_gate.verdict.get().is_none(),
        "admission refusal must not cache a model verdict"
    );
    harness.shutdown().await;
}
