//! `keep_open` on every consumer-API `buyer.on.*` field: the failing request gets its error, no
//! chain write is submitted, the local API is not closed, and the next request is served on the
//! same deal. Every scenario is also run under a closing answer for the same field, so a scenario
//! that never reached its policy cannot pass as "kept open".
use super::*;

const PATHS: [&str; 3] = ["/v1/chat/completions", "/v1/responses", "/v1/messages"];

fn keep_open() -> BuyerApiFailurePolicy {
    BuyerApiFailurePolicy {
        verification_bail: VerificationBailAction::KeepOpen,
        dead_gateway: DeadGatewayAction::KeepOpen,
        empty_stream: EmptyStreamAction::KeepOpen,
        seller_stalls_mid_stream: SellerStallsMidStreamAction::KeepOpen,
    }
}

/// Answers that close the local API for each field, so the control run shows the failure did
/// reach the policy.
fn closing() -> BuyerApiFailurePolicy {
    BuyerApiFailurePolicy {
        verification_bail: VerificationBailAction::StopAndBlacklist,
        dead_gateway: DeadGatewayAction::FailClosed,
        empty_stream: EmptyStreamAction::FailClosed,
        seller_stalls_mid_stream: SellerStallsMidStreamAction::Dispute,
    }
}

async fn harness(
    upstream: crate::seller::UpstreamConfig,
    content_gate: ContentGate,
    failure_policy: BuyerApiFailurePolicy,
    lifetime: SessionLifetimePolicy,
) -> WeeklyRouteHarness {
    weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
        claimed: PROBE_CLAIM,
        subscription: false,
        expires_in: SUB_WEEK_LEN.as_secs(),
        upstream_tokens: UNCONSTRAINED_UPSTREAM,
        content_gate,
        upstream,
        failure_policy,
        lifetime,
    })
    .await
}

/// The failing request was answered as a failure, and nothing outlived it.
fn assert_kept_open(
    harness: &WeeklyRouteHarness,
    deal: &ApiDeal,
    failed: (http::StatusCode, &str),
    case: &str,
) {
    let (status, body) = failed;
    let reported = status != http::StatusCode::OK
        || [
            "error",
            "content_filter",
            "refusal",
            "empty_output",
            "failed",
        ]
        .iter()
        .any(|marker| body.contains(marker));
    assert!(
        reported,
        "{case}: the failing request must report its failure: {status} {body}"
    );
    assert_ne!(status, http::StatusCode::GONE, "{case}: {body}");
    assert!(
        !deal.session.is_closed(),
        "{case}: local API latched closed: {body}"
    );
    assert_eq!(deal.session.recovery_episode(), None, "{case}");
    assert_eq!(deal.session.terminal_action(), None, "{case}");
    assert!(!deal.session.is_settled(), "{case}");
    assert_eq!(
        harness.chain.foreign_money_calls(),
        0,
        "{case}: STOP submitted"
    );
    assert_eq!(
        harness.chain.dispute_calls(),
        0,
        "{case}: dispute submitted"
    );
    assert!(
        !harness.task.as_ref().unwrap().is_finished(),
        "{case}: the local API stopped serving"
    );
}

/// The next request runs the normal path again on the same deal and is served.
async fn assert_next_served(
    harness: &WeeklyRouteHarness,
    tc: &str,
    path: &str,
    stream: bool,
    case: &str,
) {
    let (status, body) = harness.ask_full(path, 4, "question", stream).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{case}: {body}");
    let finish = terminal_reason(&body);
    assert!(
        matches!(
            finish.as_deref(),
            Some("stop" | "end_turn" | "length" | "max_tokens")
        ) || (path == "/v1/responses" && body.contains("completed")),
        "{case}: next request was not served: {body}"
    );
    let current = harness.deals.current().await.unwrap();
    assert_eq!(current.route.token_contract, tc, "{case}: deal replaced");
    assert!(!current.session.is_closed(), "{case}");
    assert_eq!(harness.chain.foreign_money_calls(), 0, "{case}");
    assert_eq!(harness.chain.dispute_calls(), 0, "{case}");
}

/// Under the closing control answer the same failure latches the local API: the next request is
/// refused with 410.
async fn assert_control_closes(harness: &WeeklyRouteHarness, path: &str, stream: bool, case: &str) {
    let deal = harness.deals.current().await.unwrap();
    assert!(
        deal.session.is_closed(),
        "{case}: control never reached the policy"
    );
    let (status, body) = harness.ask_full(path, 4, "question", stream).await;
    assert_eq!(status, reqwest::StatusCode::GONE, "{case}: {body}");
}

/// A gateway failure before the first token. Since a provider failure is request-scoped by
/// itself, so this is the gateway's own untagged transport failure, the one `dead_gateway` owns.
#[tokio::test]
async fn keep_open_dead_gateway_before_the_first_token_keeps_serving_the_deal() {
    for path in PATHS {
        for stream in [false, true] {
            for (policy, kept) in [(keep_open(), true), (closing(), false)] {
                let case = format!("dead_gateway {path} stream={stream} keep_open={kept}");
                let harness = harness(
                    crate::seller::UpstreamConfig::Mock,
                    ContentGate::skip(),
                    policy,
                    SessionLifetimePolicy::SettleOnExit,
                )
                .await;
                let deal = harness.deals.current().await.unwrap();
                let tc = deal.route.token_contract.clone();
                harness
                    .seller
                    .set_probe_output_fixture(false, "DEXDO_FIXTURE_TRANSPORT_BEFORE_OUTPUT");
                let (status, body) = harness.ask_full(path, 4, "question", stream).await;
                if kept {
                    assert_kept_open(&harness, &deal, (status, &body), &case);
                    // The gateway answers again; this marker selects no failure.
                    harness
                        .seller
                        .set_probe_output_fixture(false, "DEXDO_2148_GATEWAY_BACK");
                    assert_next_served(&harness, &tc, path, stream, &case).await;
                } else {
                    assert_control_closes(&harness, path, stream, &case).await;
                }
                harness.shutdown().await;
            }
        }
    }
}

/// A gateway that does not accept connections at all fails the stream open, the second dead
/// gateway path. Each request reconnects: the second one reaches the dead gateway again instead of
/// being refused by a latched local API.
#[tokio::test]
async fn keep_open_dead_gateway_at_stream_open_reconnects_on_every_request() {
    for path in PATHS {
        for (policy, kept) in [(keep_open(), true), (closing(), false)] {
            let case = format!("dead_gateway open {path} keep_open={kept}");
            let harness = harness(
                crate::seller::UpstreamConfig::Mock,
                ContentGate::skip(),
                policy,
                SessionLifetimePolicy::SettleOnExit,
            )
            .await;
            let deal = harness.deals.current().await.unwrap();
            harness.seller.server_task.abort();
            for _ in 0..100 {
                if tokio::net::TcpStream::connect(harness.seller.listen_addr)
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let (status, body) = harness.ask_full(path, 4, "question", false).await;
            assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{case}: {body}");
            if kept {
                assert_kept_open(&harness, &deal, (status, &body), &case);
                for _ in 0..2 {
                    let (status, body) = harness.ask_full(path, 4, "question", false).await;
                    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{case}: {body}");
                    assert!(body.contains("upstream open failed"), "{case}: {body}");
                    assert_kept_open(&harness, &deal, (status, &body), &case);
                }
            } else {
                assert_control_closes(&harness, path, false, &case).await;
            }
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn keep_open_empty_stream_keeps_serving_the_deal() {
    for path in PATHS {
        for stream in [false, true] {
            for (policy, kept) in [(keep_open(), true), (closing(), false)] {
                let case = format!("empty_stream {path} stream={stream} keep_open={kept}");
                let harness = harness(
                    crate::seller::UpstreamConfig::Mock,
                    ContentGate::skip(),
                    policy,
                    SessionLifetimePolicy::SettleOnExit,
                )
                .await;
                let deal = harness.deals.current().await.unwrap();
                let tc = deal.route.token_contract.clone();
                let (status, body) = harness
                    .ask_full(path, 8, "DEXDO_FIXTURE_REASONING_ONLY", stream)
                    .await;
                if kept {
                    assert_kept_open(&harness, &deal, (status, &body), &case);
                    // Accounting is unchanged: the reasoning-only answer stays billed.
                    assert!(deal.billable_tokens() > 0, "{case}");
                    assert_next_served(&harness, &tc, path, stream, &case).await;
                } else {
                    assert_control_closes(&harness, path, stream, &case).await;
                }
                harness.shutdown().await;
            }
        }
    }
}

/// A gateway failure after output was accepted. The provider-tagged variant of the same shape is
/// 's unresolved-usage pause, checked separately below.
#[tokio::test]
async fn keep_open_seller_stall_mid_stream_keeps_serving_the_deal() {
    for path in PATHS {
        for stream in [false, true] {
            for (policy, kept) in [(keep_open(), true), (closing(), false)] {
                let case =
                    format!("seller_stalls_mid_stream {path} stream={stream} keep_open={kept}");
                let harness = harness(
                    crate::seller::UpstreamConfig::Mock,
                    ContentGate::skip(),
                    policy,
                    SessionLifetimePolicy::SettleOnExit,
                )
                .await;
                let deal = harness.deals.current().await.unwrap();
                let tc = deal.route.token_contract.clone();
                let (status, body) = harness
                    .ask_full(path, 8, "DEXDO_FIXTURE_USAGE_THEN_ERROR", stream)
                    .await;
                if kept {
                    assert_kept_open(&harness, &deal, (status, &body), &case);
                    assert_next_served(&harness, &tc, path, stream, &case).await;
                } else {
                    assert_eq!(harness.chain.dispute_calls(), 1, "{case}");
                    assert_control_closes(&harness, path, stream, &case).await;
                }
                harness.shutdown().await;
            }
        }
    }
}

/// Provider output that ended without terminal usage pauses the deal until that request is
/// reconciled. `keep_open` writes nothing and closes nothing, and it does not lift the
/// pause either: what is refused for lack of authoritative usage stays refused.
#[tokio::test]
async fn keep_open_does_not_lift_the_unresolved_usage_pause() {
    for path in PATHS {
        for stream in [false, true] {
            let case = format!("unresolved usage {path} stream={stream}");
            let harness = harness(
                crate::seller::UpstreamConfig::Mock,
                ContentGate::skip(),
                keep_open(),
                SessionLifetimePolicy::SettleOnExit,
            )
            .await;
            let deal = harness.deals.current().await.unwrap();
            let (status, body) = harness
                .ask_full(
                    path,
                    8,
                    "DEXDO_FIXTURE_STRADDLE DEXDO_FIXTURE_ERROR_BEFORE_USAGE",
                    stream,
                )
                .await;
            assert_kept_open(&harness, &deal, (status, &body), &case);
            let (status, body) = harness.ask_full(path, 4, "question", stream).await;
            assert_eq!(
                status,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "{case}: {body}"
            );
            assert!(body.contains("upstream_usage_unresolved"), "{case}: {body}");
            assert!(!deal.session.is_closed(), "{case}");
            assert_eq!(harness.chain.foreign_money_calls(), 0, "{case}");
            assert_eq!(harness.chain.dispute_calls(), 0, "{case}");
            harness.shutdown().await;
        }
    }
}

/// The inline verifier bails on a substituted model. Under `keep_open` that request ends as the
/// bail it is, and the deal keeps serving -- under both lifetimes, because a preserved deal used
/// to close its local API on the way to vetoing the chain write.
#[tokio::test]
async fn keep_open_inline_verification_bail_keeps_serving_the_deal() {
    for lifetime in [
        SessionLifetimePolicy::SettleOnExit,
        SessionLifetimePolicy::Preserve,
    ] {
        for path in PATHS {
            for stream in [false, true] {
                for (policy, kept) in [(keep_open(), true), (closing(), false)] {
                    let case = format!(
                        "bad_output_scam {path} stream={stream} {lifetime:?} keep_open={kept}"
                    );
                    let harness = harness(
                        crate::seller::UpstreamConfig::Mock,
                        ContentGate::skip(),
                        policy,
                        lifetime,
                    )
                    .await;
                    let deal = harness.deals.current().await.unwrap();
                    let tc = deal.route.token_contract.clone();
                    let (status, body) = harness
                        .ask_full(path, 4, "DEXDO_FIXTURE_SUBSTITUTE", stream)
                        .await;
                    if kept {
                        assert_kept_open(&harness, &deal, (status, &body), &case);
                        assert_next_served(&harness, &tc, path, stream, &case).await;
                    } else {
                        assert_control_closes(&harness, path, stream, &case).await;
                    }
                    harness.shutdown().await;
                }
            }
        }
    }
}

/// The per-deal content check bails. Under `keep_open` the request that ran it is refused with
/// the reason, and the next request is served without probing again.
#[tokio::test]
async fn keep_open_content_identity_bail_keeps_serving_the_deal() {
    for (policy, kept) in [(keep_open(), true), (closing(), false)] {
        let case = format!("content gate keep_open={kept}");
        let harness = harness(
            crate::seller::UpstreamConfig::Mock,
            ContentGate::probe(
                "dexdo-mock".to_string(),
                probe_models_expecting(
                    "identity probe",
                    "this fingerprint cannot match the mock",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
            ),
            policy,
            SessionLifetimePolicy::SettleOnExit,
        )
        .await;
        let deal = harness.deals.current().await.unwrap();
        let tc = deal.route.token_contract.clone();
        let (status, body) = harness
            .ask_full("/v1/chat/completions", 4, "question", false)
            .await;
        assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{case}: {body}");
        assert!(
            body.contains("model identity verification failed"),
            "{case}: {body}"
        );
        if kept {
            assert_kept_open(&harness, &deal, (status, &body), &case);
            let probes = harness.seller.calls();
            assert_next_served(&harness, &tc, "/v1/chat/completions", false, &case).await;
            assert_eq!(
                deal.content_gate.outstanding_verification_tokens(),
                0,
                "{case}: the content check is not owed again"
            );
            // One challenge and one stream for the answer; no second probe.
            assert_eq!(harness.seller.calls(), probes + 2, "{case}");
        } else {
            assert_control_closes(&harness, "/v1/chat/completions", false, &case).await;
        }
        harness.shutdown().await;
    }
}

/// Any closure category that is not one of the four consumer-API failure fields is never kept
/// open, whatever the policy says.
#[test]
fn keeps_open_only_names_the_four_policy_fields() {
    let chain = Arc::new(RecordingSettleChain::default());
    let session = SessionSettle::new_with_failure_policy(
        chain,
        "tc-2148".to_string(),
        Arc::new(dexdo_core::LocalNote::generate()),
        keep_open(),
    );
    for category in [
        ClosureCategory::DeadGateway,
        ClosureCategory::EmptyStream,
        ClosureCategory::SellerStallsMidStream,
        ClosureCategory::VerificationBail,
    ] {
        assert!(session.keeps_open(category), "{category:?}");
    }
    for category in [
        ClosureCategory::PlannedShutdown,
        ClosureCategory::NoDelivery,
        ClosureCategory::ChainStateUnavailable,
        ClosureCategory::DealNotOpen,
        ClosureCategory::TerminalObserved,
        ClosureCategory::SessionStop,
        ClosureCategory::Recovery,
    ] {
        assert!(!session.keeps_open(category), "{category:?}");
    }
}

/// Each settle entry point under `keep_open` writes nothing and closes nothing, including a
/// preserved deal.
#[tokio::test]
async fn keep_open_settle_entry_points_make_no_chain_write() {
    for lifetime in [
        SessionLifetimePolicy::SettleOnExit,
        SessionLifetimePolicy::Preserve,
    ] {
        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_failure_policy_and_lifetime(
            chain.clone(),
            "tc-2148".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            keep_open(),
            lifetime,
        );
        assert!(!session.settle_verification_bail("bail").await);
        assert!(
            !session
                .settle_dead_gateway("dead", &unchanged_heartbeat())
                .await
        );
        assert!(
            !session
                .settle_empty_stream("empty", &unchanged_heartbeat())
                .await
        );
        assert!(
            !session
                .settle_seller_stalls_mid_stream("stall", &unchanged_heartbeat())
                .await
        );
        assert!(!session.is_closed(), "{lifetime:?}");
        assert_eq!(session.recovery_episode(), None);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
    }
}
