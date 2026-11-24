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
            let response = router(state.clone())
                .oneshot(inference_request())
                .await
                .unwrap();
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
            assert_eq!(verification["complete"], !after_terminal);
            assert_eq!(verification["total_tokens"].is_null(), after_terminal);
            assert_eq!(
                verification["buyer_charged_tokens"].is_null(),
                after_terminal
            );
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
            let calls = harness.seller.calls();
            let tc = harness.deals.current().await.unwrap().route.token_contract;
            harness.seller.clear_probe_output_fixture();
            let retry = router(state).oneshot(inference_request()).await.unwrap();
            assert_eq!(retry.status(), http::StatusCode::OK);
            let retry = axum::body::to_bytes(retry.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&retry).contains("mock-reply"));
            assert_eq!(
                harness.seller.calls() - calls,
                6,
                "retry runs B8, B7 and answer"
            );
            assert_eq!(
                harness.deals.current().await.unwrap().route.token_contract,
                tc
            );
            let database = rusqlite::Connection::open(dir.path().join("usage.sqlite3")).unwrap();
            let mut rows = database
                .prepare(
                    "SELECT r.outcome,u.phase,CAST(u.charged_tokens AS INTEGER)
                 FROM request_usage u JOIN requests r USING(network,request_id)
                 WHERE u.accepted=1 AND u.call_id=u.phase ORDER BY r.outcome,u.phase",
                )
                .unwrap();
            let charges = rows
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let mut expected = Vec::new();
            if b7 {
                expected.push(("failed", "verification_b8", 3));
            }
            expected.extend([
                ("succeeded", "answer", 2),
                ("succeeded", "verification_b7", 10),
                ("succeeded", "verification_b8", 3),
            ]);
            assert_eq!(
                charges
                    .iter()
                    .map(|(outcome, phase, tokens)| (outcome.as_str(), phase.as_str(), *tokens))
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(harness.delivered().await, paid + 15);
            assert_reopen_preserves_history(dir.path(), store);
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn b8_and_b7_cancellation_before_or_after_terminal_preserves_history() {
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
            assert_eq!(verification["complete"], !after_terminal);
            assert_eq!(verification["total_tokens"].is_null(), after_terminal);
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
        }
        harness.shutdown().await;
    }
}
