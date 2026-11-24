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
    assert_deal_kept_open(harness, deal, case);
}

/// Nothing outlived the failing request: the deal is open and unsettled, and no money moved.
fn assert_deal_kept_open(harness: &WeeklyRouteHarness, deal: &ApiDeal, case: &str) {
    assert!(
        !deal.session.is_closed(),
        "{case}: local API latched closed"
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

/// A temporary gRPC failure before the provider head fails only the current request.
#[tokio::test]
async fn issue_2266_temporary_gateway_status_before_head_keeps_serving() {
    for path in PATHS {
        for stream in [false, true] {
            for policy in [keep_open(), closing()] {
                let case = format!("dead_gateway {path} stream={stream} policy={policy:?}");
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
                assert_kept_open(&harness, &deal, (status, &body), &case);
                harness
                    .seller
                    .set_probe_output_fixture(false, "DEXDO_2148_GATEWAY_BACK");
                assert_next_served(&harness, &tc, path, stream, &case).await;
                harness.shutdown().await;
            }
        }
    }
}

/// A gateway that does not accept connections at all fails the stream open, the second dead
/// gateway path. Each request reconnects: the second one reaches the dead gateway again instead of
/// being refused by a latched local API.
#[tokio::test]
async fn issue_2266_unreachable_gateway_is_request_scoped() {
    for path in PATHS {
        for policy in [keep_open(), closing()] {
            let case = format!("dead_gateway open {path} policy={policy:?}");
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
            assert_kept_open(&harness, &deal, (status, &body), &case);
            for _ in 0..2 {
                let (status, body) = harness.ask_full(path, 4, "question", false).await;
                assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{case}: {body}");
                assert!(body.contains("upstream open failed"), "{case}: {body}");
                assert_kept_open(&harness, &deal, (status, &body), &case);
            }
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn issue_2275_stalled_gateway_returns_502_and_the_next_request_recovers() {
    issue_2275_gateway_returns_502_and_recovers(false).await;
}

#[tokio::test]
async fn issue_2275_closed_ready_gateway_returns_502_without_redial_and_recovers() {
    issue_2275_gateway_returns_502_and_recovers(true).await;
}

async fn issue_2275_gateway_returns_502_and_recovers(close_ready_channel: bool) {
    use crate::buyer::tls::{
        test_tls_acceptor, wait_for_client_settings_ack, EMPTY_SERVER_SETTINGS,
    };
    use dexdo_core::params::BUYER_GATEWAY_DIAL_TIMEOUT;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let harness = weekly_route_harness_with_seller_shape(
        WeeklyRouteScenario {
            claimed: PROBE_CLAIM,
            subscription: false,
            expires_in: SUB_WEEK_LEN.as_secs(),
            upstream_tokens: UNCONSTRAINED_UPSTREAM,
            content_gate: ContentGate::skip(),
            upstream: crate::seller::UpstreamConfig::Mock,
            failure_policy: keep_open(),
            lifetime: SessionLifetimePolicy::SettleOnExit,
        },
        None,
        Some(listener.local_addr().unwrap()),
    )
    .await;
    let deal = harness.deals.current().await.unwrap();
    let path = "/v1/chat/completions";
    let started = tokio::time::Instant::now();
    let fault = async {
        let (socket, _) = listener.accept().await.unwrap();
        if !close_ready_channel {
            // Keep the socket open and silent until the request has failed.
            return Some(socket);
        }
        let acceptor = test_tls_acceptor(&harness.seller.tls);
        let mut stream = acceptor.accept(socket).await.unwrap();
        stream.write_all(&EMPTY_SERVER_SETTINGS).await.unwrap();
        stream.flush().await.unwrap();
        wait_for_client_settings_ack(&mut stream).await;
        stream.shutdown().await.unwrap();
        None
    };
    let ((status, body), stalled) = tokio::time::timeout(2 * BUYER_GATEWAY_DIAL_TIMEOUT, async {
        tokio::join!(harness.ask_full(path, 4, "question", false), fault)
    })
    .await
    .expect("keep_open must answer after one dial timeout");
    drop(stalled);
    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
    if !close_ready_channel {
        assert!(started.elapsed() >= BUYER_GATEWAY_DIAL_TIMEOUT);
        assert!(body.contains("E_GATEWAY_UNREACHABLE"), "{body}");
        assert!(body.contains("tls_handshake"), "{body}");
    }
    assert!(futures::poll!(Box::pin(listener.accept())).is_pending());
    assert_deal_kept_open(&harness, &deal, "gateway connection failed");

    // Restore the route at the advertised address, preserving the pinned TLS identity.
    let seller_addr = harness.seller.listen_addr;
    let forward = tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut seller = TcpStream::connect(seller_addr).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut seller).await;
            });
        }
    });
    assert_next_served(
        &harness,
        &deal.route.token_contract,
        path,
        false,
        "gateway restored",
    )
    .await;
    forward.abort();
    harness.shutdown().await;
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
                    if stream {
                        // A streamed answer reaches the client with the provider's own end; the
                        // buyer records it as `empty_output` and applies the policy.
                        assert_eq!(status, http::StatusCode::OK, "{case}: {body}");
                        assert!(
                            body.contains("DEXDO_FIXTURE_REASONING_ONLY"),
                            "{case}: {body}"
                        );
                        assert_deal_kept_open(&harness, &deal, &case);
                    } else {
                        assert_kept_open(&harness, &deal, (status, &body), &case);
                    }
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

/// An untagged transport failure after partial output leaves the deal open.
#[tokio::test]
async fn issue_2266_gateway_interruption_keeps_serving_the_same_deal() {
    for path in PATHS {
        for policy in [keep_open(), closing()] {
            let case = format!("seller_stalls_mid_stream {path} SSE policy={policy:?}");
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
                .ask_full(path, 8, "DEXDO_FIXTURE_CUT_AFTER_OUTPUT", true)
                .await;
            assert_kept_open(&harness, &deal, (status, &body), &case);
            assert!(body.contains("upstream_retryable"), "{case}: {body}");
            assert_eq!(deal.billable_tokens(), 0, "{case}");
            assert_next_served(&harness, &tc, path, true, &case).await;
            harness.shutdown().await;
        }
    }
}

/// A provider-tagged interruption without terminal billing also keeps the deal open.
#[tokio::test]
async fn issue_2266_provider_interruption_keeps_serving_the_same_deal() {
    for path in PATHS {
        let case = format!("interrupted answer {path} SSE");
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
                true,
            )
            .await;
        assert_kept_open(&harness, &deal, (status, &body), &case);
        assert!(body.contains("upstream_retryable"), "{case}: {body}");
        assert_eq!(deal.billable_tokens(), 0, "{case}");
        assert_next_served(&harness, &deal.route.token_contract, path, true, &case).await;
        harness.shutdown().await;
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
