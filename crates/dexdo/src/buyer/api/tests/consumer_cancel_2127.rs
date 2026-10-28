use super::*;

proptest::proptest! {
    #[test]
    fn cancelled_request_preserves_usage_and_only_observed_failures_can_latch_recovery(
        prior_usage in 1u64..100,
        failure_observed in proptest::bool::ANY,
        new_output in proptest::bool::ANY,
    ) {
        let deal = heartbeat_test_deal();
        deal.record_billable(prior_usage).unwrap();
        deal.record_accepted_output(100);
        let mut request = deal.begin_request(101);
        request.arm_upstream_failure();
        if failure_observed {
            request.observe_upstream_failure();
        }
        if new_output {
            deal.record_accepted_output(102);
        }
        drop(request);
        proptest::prop_assert_eq!(deal.billable_tokens(), prior_usage);
        proptest::prop_assert!(!deal.has_active_request());
        proptest::prop_assert_eq!(deal.session.is_closed(), failure_observed && !new_output);
        proptest::prop_assert_eq!(deal.session.recovery_episode(),
            (failure_observed && !new_output).then_some(RecoveryKind::ReclaimOpened));
        proptest::prop_assert_eq!(deal.session.terminal_action(), None);
        deal.session.disable_drop_backup();
    }
}

#[tokio::test]
async fn cancelled_request_keeps_prior_usage_and_serves_the_next_request() {
    for lifetime in [
        SessionLifetimePolicy::SettleOnExit,
        SessionLifetimePolicy::Preserve,
    ] {
        for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
            for stream in [false, true] {
                let harness = weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
                    claimed: PROBE_CLAIM,
                    subscription: false,
                    expires_in: SUB_WEEK_LEN.as_secs(),
                    upstream_tokens: 8,
                    content_gate: ContentGate::skip(),
                    upstream: crate::seller::UpstreamConfig::Mock,
                    failure_policy: BuyerApiFailurePolicy::default(),
                    lifetime,
                })
                .await;
                let (status, body) = harness
                    .ask_full(path, 8, "before cancellation", false)
                    .await;
                assert_eq!(status, reqwest::StatusCode::OK, "{path}: {body}");
                let deal = harness.deals.current().await.unwrap();
                let prior_usage = deal.billable_tokens();
                let remaining = deal.remaining_tokens();
                assert!(prior_usage > 0);
                // Registered past the probe with no capacity file, the seller's claim counter starts at
                // the deal's `tokensPending`.
                let registered = u64::try_from(PROBE_CLAIM).unwrap();
                assert_eq!(
                    harness.seller.billed_tokens(&deal.route.token_contract),
                    registered + prior_usage
                );

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let state = direct_state_with_deliveries(&harness, tx);
                let handler = tokio::spawn(async move {
                    direct_consumer_response(
                        state,
                        path,
                        8,
                        "DEXDO_FIXTURE_HOLD_STREAM_OPEN",
                        stream,
                    )
                    .await
                });
                tokio::time::timeout(Duration::from_secs(5), harness.seller.wait_for_held_open())
                    .await
                    .expect("seller admitted the request but has not returned its stream");
                handler.abort();
                assert!(handler.await.unwrap_err().is_cancelled());
                let cancelled = rx.recv().await.expect("one cancellation record");
                assert_eq!(cancelled.http_status, CLIENT_CLOSED_REQUEST_STATUS);
                assert_eq!(cancelled.failure_stage, Some("consumer_disconnect"));
                assert_eq!(cancelled.finish_reason, "cancelled");
                assert_eq!(cancelled.visible_output_tokens, 0);
                assert_eq!(cancelled.billable_tokens, None);
                assert_eq!(cancelled.request_billable_tokens, 0);
                assert_eq!(cancelled.route_billable_tokens, Some(prior_usage));
                assert_eq!(deal.billable_tokens(), prior_usage);
                assert_eq!(deal.remaining_tokens(), remaining);
                assert!(!deal.has_active_request());
                assert!(
                    !deal.session.is_closed(),
                    "{path} stream={stream} {lifetime:?}: cancellation must stay request-scoped"
                );
                assert_eq!(deal.session.recovery_episode(), None);
                assert_eq!(deal.session.terminal_action(), None);
                assert_eq!(harness.chain.foreign_money_calls.load(Ordering::SeqCst), 0);
                assert!(
                    rx.try_recv().is_err(),
                    "cancellation must not be reported twice"
                );

                // The seller drains work independently of the cancelled consumer. Its accepted
                // terminal usage still spends capacity; the buyer never guesses missing usage.
                harness.seller.release_delayed_eof();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while harness.seller.billed_tokens(&deal.route.token_contract)
                        <= registered + prior_usage
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("cancelled request retains seller-side accounting");
                let seller_after_cancel = harness.seller.billed_tokens(&deal.route.token_contract);

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let response = direct_consumer_response(
                    direct_state_with_deliveries(&harness, tx),
                    path,
                    8,
                    "after cancellation",
                    stream,
                )
                .await;
                assert_eq!(response.status(), http::StatusCode::OK);
                axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let next = rx
                    .recv()
                    .await
                    .expect("next request completes on the same deal");
                assert_eq!(next.failure_stage, None);
                assert_eq!(next.token_contract, cancelled.token_contract);
                assert!(next.request_billable_tokens > 0);
                assert_eq!(
                    deal.billable_tokens(),
                    prior_usage + next.request_billable_tokens
                );
                assert_eq!(
                    harness.seller.billed_tokens(&deal.route.token_contract),
                    seller_after_cancel + next.request_billable_tokens
                );
                assert_eq!(harness.chain.foreign_money_calls.load(Ordering::SeqCst), 0);
                let chain = harness.chain.clone();
                harness.shutdown().await;
                assert_eq!(
                    chain.foreign_money_calls.load(Ordering::SeqCst),
                    usize::from(lifetime == SessionLifetimePolicy::SettleOnExit),
                    "only the explicit session shutdown may settle; preservation submits nothing"
                );
            }
        }
    }
}

#[tokio::test]
async fn dropping_an_unpolled_sse_body_does_not_close_the_deal() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let harness = weekly_route_harness(PROBE_CLAIM, false).await;
        assert_eq!(harness.ask(8).await.0, reqwest::StatusCode::OK);
        let deal = harness.deals.current().await.unwrap();
        let prior_usage = deal.billable_tokens();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let response = direct_consumer_response(
            direct_state_with_deliveries(&harness, tx),
            path,
            8,
            "cancel the SSE body",
            true,
        )
        .await;
        assert_eq!(response.status(), http::StatusCode::OK);
        drop(response);
        let cancelled = rx.recv().await.expect("cancelled SSE record");
        assert_eq!(cancelled.http_status, 200);
        assert_eq!(cancelled.failure_stage, Some("consumer_disconnect"));
        assert_eq!(cancelled.visible_output_tokens, 0);
        assert_eq!(cancelled.billable_tokens, None);
        assert_eq!(deal.billable_tokens(), prior_usage);
        assert!(
            !deal.session.is_closed(),
            "{path}: dropping a body is request-scoped"
        );
        assert_eq!(deal.session.recovery_episode(), None);
        assert_eq!(harness.ask_path(path, 8).await.0, reqwest::StatusCode::OK);
        harness.shutdown().await;
    }
}
