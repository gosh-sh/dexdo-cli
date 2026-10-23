use super::*;
use proptest::prelude::*;

#[tokio::test]
async fn transient_read_2129_refuses_each_endpoint_then_recovers_the_same_deal() {
    let harness = weekly_route_harness(0, false).await;
    let (status, body) = harness.ask_full("/v1/chat/completions", 1, "", false).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    let deal = harness.deals.current().await.unwrap();
    let calls = harness.seller.calls();
    let remaining = harness.remaining().await;
    let delivered = harness.delivered().await;
    harness
        .chain
        .transient_deal_state
        .store(true, Ordering::SeqCst);
    // Even a reasoning capability query must wait for authoritative state.
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for stream in [false, true] {
            for disable_reasoning in [false, true] {
                if path == "/v1/messages" && disable_reasoning {
                    continue;
                }
                let mut input = if path == "/v1/responses" {
                    serde_json::json!({"model":"dexdo-mock", "input":"", "max_output_tokens":1, "stream":stream})
                } else {
                    serde_json::json!({"model":"dexdo-mock", "messages":[{"role":"user", "content":""}], "max_tokens":1, "stream":stream})
                };
                if disable_reasoning {
                    if path == "/v1/responses" {
                        input["reasoning"] = serde_json::json!({"effort":"none"});
                    } else {
                        input["reasoning_effort"] = serde_json::json!("none");
                    }
                }
                let response = reqwest::Client::new()
                    .post(format!("http://{}{path}", harness.addr))
                    .json(&input)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let body = response.text().await.unwrap();
                assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["error"]["code"], "deal_state_unavailable", "{body}");
                assert_eq!(body["error"]["recovery_action"], "retry_with_backoff");
                assert!(!deal.session.is_closed());
                assert_eq!(
                    harness.seller.calls(),
                    calls,
                    "no seller RPC during failed reads"
                );
                assert_eq!(harness.remaining().await, remaining);
                assert_eq!(harness.delivered().await, delivered);
                assert_eq!(harness.chain.foreign_money_calls(), 0);
                assert_eq!(harness.chain.settle_calls(), 0);
                assert_eq!(harness.chain.dispute_calls(), 0);
            }
        }
    }
    harness
        .chain
        .transient_deal_state
        .store(false, Ordering::SeqCst);
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        let current = harness.deals.current().await.unwrap();
        assert!(Arc::ptr_eq(&deal.session, &current.session));
    }
    assert_eq!(
        harness.chain.foreign_money_calls(),
        0,
        "no new BUY or recovery write"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn transient_read_2129_does_not_make_missing_or_invalid_facts_retryable() {
    for invalid in [false, true] {
        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_deal_state.store(invalid, Ordering::SeqCst);
        let session = SessionSettle::new(
            chain.clone(),
            "tc-2129-invalid".into(),
            Arc::new(dexdo_core::LocalNote::generate()),
        );
        let error = session.ensure_open_for_serving().await.unwrap_err();
        assert_eq!(error.code, ConsumerErrorCode::DealUnavailable);
        assert!(session.is_closed());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        session.disable_drop_backup();
    }
}

proptest! {
    #[test]
    fn transient_read_2129_never_authorizes_an_unreadable_deal(
        failures in prop::collection::vec(any::<bool>(), 1..24)
    ) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let chain = Arc::new(RecordingSettleChain::default());
            chain.set_deal_state(opened_deal_state(TEST_DEPOSIT, unix_now_secs()));
            let session = SessionSettle::new(chain.clone(), "tc-2129".into(),
                Arc::new(dexdo_core::LocalNote::generate()));
            for unavailable in failures {
                chain.transient_deal_state.store(unavailable, Ordering::SeqCst);
                assert_eq!(session.ensure_open_for_serving().await.is_err(), unavailable);
                assert!(!session.is_closed());
                assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
                assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
                assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
                assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
            }
            session.disable_drop_backup();
        });
    }
}

#[tokio::test]
async fn transient_read_2129_does_not_reopen_an_independently_closed_session() {
    let chain = Arc::new(RecordingSettleChain::default());
    chain.set_deal_state(opened_deal_state(TEST_DEPOSIT, unix_now_secs()));
    let session = SessionSettle::new(
        chain.clone(),
        "tc-2129-closed".into(),
        Arc::new(dexdo_core::LocalNote::generate()),
    );
    session.close_local_api(ClosureCategory::DeadGateway, Some("fail_closed"));
    chain.transient_deal_state.store(true, Ordering::SeqCst);
    assert!(session.ensure_open_for_serving().await.is_err());
    assert!(session.is_closed());
    assert!(session.closed_rejection("retry", false).is_some());
    session.disable_drop_backup();
}
