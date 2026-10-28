use super::*;
use proptest::prelude::*;

const PATHS: [&str; 3] = ["/v1/chat/completions", "/v1/responses", "/v1/messages"];

/// a request on an open deal reaches the seller without a chain read of the deal. Each such
/// read is six chain requests behind the per-process ceiling, so it cost every request ~1.5 s on
/// shellnet and held mainnet's three requests a second to half a request a second.
#[tokio::test]
async fn issue_2187_requests_on_an_open_deal_read_no_deal_state() {
    for subscription in [false, true] {
        let harness = weekly_route_harness(PROBE_CLAIM, subscription).await;
        let snapshot_reads = harness.chain.reads();
        for path in PATHS {
            for stream in [false, true] {
                let (status, body) = harness.ask_full(path, 1, "", stream).await;
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{path} stream={stream}: {body}"
                );
            }
        }
        assert_eq!(
            harness.chain.deal_state_reads(),
            0,
            "subscription={subscription}"
        );
        assert_eq!(
            harness.chain.reads(),
            snapshot_reads,
            "a mid-week request books nothing, so it reads nothing: subscription={subscription}"
        );
        harness.shutdown().await;
    }
}

/// negative: an unreadable chain no longer refuses a request. The old gate answered
/// `deal_state_unavailable` here without asking the seller.
#[tokio::test]
async fn issue_2187_an_unreadable_chain_does_not_refuse_requests() {
    let harness = weekly_route_harness(PROBE_CLAIM, false).await;
    harness
        .chain
        .transient_deal_state
        .store(true, Ordering::SeqCst);
    let deal = harness.deals.current().await.unwrap();
    let calls = harness.seller.calls();
    for path in PATHS {
        for stream in [false, true] {
            let (status, body) = harness.ask_full(path, 1, "", stream).await;
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "{path} stream={stream}: {body}"
            );
        }
    }
    assert!(
        harness.seller.calls() > calls,
        "every request reached the seller"
    );
    assert!(!deal.session.is_closed());
    assert_eq!(harness.chain.deal_state_reads(), 0);
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    assert_eq!(harness.chain.settle_calls(), 0);
    assert_eq!(harness.chain.dispute_calls(), 0);
    harness.shutdown().await;
}

/// the deal's end: the background read, not a request, finds the deal stopped and closes
/// serving. The next request is refused with the recorded closure before it reaches the seller,
/// and reads nothing itself.
#[tokio::test]
async fn issue_2187_the_observed_end_of_the_deal_closes_serving() {
    let harness = weekly_route_harness(PROBE_CLAIM, false).await;
    {
        let mut snapshot = harness.chain.snapshot.lock().unwrap();
        snapshot.state.opened = false;
        snapshot.state.deposit = 0;
        snapshot.state.probe_tick = 0;
        assert!(snapshot.state.is_stopped());
    }
    observe_deal(&harness.deals).await;
    assert_eq!(harness.chain.deal_state_reads(), 1);
    let calls = harness.seller.calls();
    for path in PATHS {
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::GONE, "{path}: {body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"]["code"], "deal_session_closed", "{body}");
        assert_eq!(body["error"]["closure"]["category"], "terminal_observed");
    }
    assert_eq!(
        harness.seller.calls(),
        calls,
        "no request reached the seller"
    );
    assert_eq!(
        harness.chain.deal_state_reads(),
        1,
        "only the observer read"
    );
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    harness.shutdown().await;
}

proptest! {
    /// invariant: the route's trial-tick acceptance is the OR of every read observed so far,
    /// so a later read showing the probe unaccepted never puts the one-tick ceiling back.
    #[test]
    fn issue_2187_observed_probe_acceptance_is_never_cleared(
        reads in prop::collection::vec(any::<bool>(), 1..24)
    ) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let session = Arc::new(SessionSettle::new(
                Arc::new(RecordingSettleChain::default()),
                "tc-2187".into(),
                Arc::new(dexdo_core::LocalNote::generate()),
            ));
            let deal = ApiDeal::new(
                Route {
                    handover: Handover {
                        endpoint: "https://127.0.0.1:1".to_string(),
                        tls_fingerprint: "00".repeat(32),
                    },
                    token_contract: "tc-2187".to_string(),
                    max_tokens: 1,
                },
                session.clone(),
                Arc::new(ContentGate::skip()),
            );
            let mut seen = false;
            for accepted in reads {
                let mut state = opened_deal_state(TEST_DEPOSIT, unix_now_secs());
                state.probe_accepted = accepted;
                deal.observe_deal_state(&Ok(Some(state))).await;
                seen |= accepted;
                assert_eq!(deal.probe_accepted(), seen);
                assert!(!session.is_closed(), "an open deal keeps serving");
            }
            session.disable_drop_backup();
        });
    }
}
