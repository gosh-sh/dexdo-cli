use super::*;
use crate::buyer::usage::UsageStore;
use axum::{response::IntoResponse, routing::post, Router};
use std::sync::atomic::AtomicU8;
use tower::ServiceExt;

async fn provider(
    mode: Arc<AtomicU8>,
    native: bool,
) -> (crate::seller::UpstreamConfig, tokio::task::JoinHandle<()>) {
    let concurrent_failures = Arc::new(tokio::sync::Barrier::new(3));
    let handler = move || {
        let mode = mode.clone();
        let concurrent_failures = concurrent_failures.clone();
        async move {
            let mode = mode.load(Ordering::SeqCst);
            if mode == 4 {
                concurrent_failures.wait().await;
            }
            if mode == 5 {
                let body = axum::body::Body::from_stream(async_stream::stream! {
                    yield Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    yield Ok(axum::body::Bytes::from(vec![b'x'; dexdo_core::params::UPSTREAM_SSE_FRAME_MAX_BYTES + 1]));
                });
                return ([("content-type", "text/event-stream")], body).into_response();
            }
            let body = match (mode, native) {
                (0 | 4, _) => "event: error\ndata: {\"error\":{\"message\":\"provider overloaded\"}}\n\n",
                (1, false) => "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                (1, true) => "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                (2, false) => "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hidden\"}}]}\n\n",
                (2, true) => "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hidden\"}}\n\n",
                (_, false) => "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n",
                (_, true) => "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            };
            ([("content-type", "text/event-stream")], body).into_response()
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/chat/completions", post(handler.clone()))
                .route("/v1/messages", post(handler)),
        )
        .await
        .unwrap();
    });
    let key = format!("DEXDO_2144_PROVIDER_{}", addr.port());
    std::env::set_var(&key, "fixture-key");
    let upstream = if native {
        crate::seller::UpstreamConfig::Anthropic(crate::seller::AnthropicConfig {
            base_url: format!("http://{addr}"),
            model: "dexdo-mock".into(),
            frame_model: "dexdo-mock".into(),
            api_key_env: key,
            max_output_tokens: Some(8),
            tokenizer_family: String::new(),
            reasoning_disable: Default::default(),
        })
    } else {
        crate::seller::UpstreamConfig::OpenAi(crate::seller::OpenAiConfig {
            base_url: format!("http://{addr}"),
            model: "dexdo-mock".into(),
            frame_model: "dexdo-mock".into(),
            api_key_env: key,
            capabilities: crate::seller::Capabilities {
                max_output_tokens: Some(8),
                ..Default::default()
            },
            ..Default::default()
        })
    };
    (upstream, task)
}

async fn harness(
    upstream: crate::seller::UpstreamConfig,
    lifetime: SessionLifetimePolicy,
) -> WeeklyRouteHarness {
    weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
        claimed: 0,
        subscription: false,
        expires_in: SUB_WEEK_LEN.as_secs(),
        upstream_tokens: UNCONSTRAINED_UPSTREAM,
        content_gate: ContentGate::skip(),
        upstream,
        failure_policy: BuyerApiFailurePolicy::default(),
        lifetime,
    })
    .await
}

async fn ask(
    harness: &WeeklyRouteHarness,
    store: &Arc<UsageStore>,
    path: &str,
    stream: bool,
) -> (http::StatusCode, String) {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let state = direct_state_with_deliveries(harness, tx).with_usage(store.clone());
    let body = if path == "/v1/responses" {
        serde_json::json!({"model":"dexdo-mock","input":"question","max_output_tokens":4,"stream":stream})
    } else {
        serde_json::json!({"model":"dexdo-mock","messages":[{"role":"user","content":"question"}],"max_tokens":4,"stream":stream})
    };
    let response = router(state, true)
        .oneshot(
            http::Request::post(path)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn issue_2144_provider_error_preserves_prior_usage_and_concurrent_requests() {
    for native in [false, true] {
        for lifetime in [
            SessionLifetimePolicy::SettleOnExit,
            SessionLifetimePolicy::Preserve,
        ] {
            let mode = Arc::new(AtomicU8::new(3));
            let (upstream, task) = provider(mode.clone(), native).await;
            let harness = harness(upstream, lifetime).await;
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(
                UsageStore::open(dir.path().join("usage.sqlite3"), "test-network").unwrap(),
            );
            let deal = harness.deals.current().await.unwrap();
            assert_eq!(
                ask(&harness, &store, "/v1/responses", true).await.0,
                http::StatusCode::OK
            );
            let before = deal.billable_tokens();
            assert_eq!(before, 2);
            harness
                .seller
                .set_concurrency(std::num::NonZeroU32::new(3).unwrap());
            mode.store(4, Ordering::SeqCst);
            let failures = tokio::time::timeout(
                Duration::from_secs(5),
                futures::future::join_all(
                    ["/v1/chat/completions", "/v1/responses", "/v1/messages"]
                        .into_iter()
                        .map(|path| ask(&harness, &store, path, true)),
                ),
            )
            .await
            .expect("all three requests reach the provider concurrently");
            for (status, body) in failures {
                assert_eq!(status, http::StatusCode::OK, "{body}");
                assert!(
                    !deal.session.is_closed(),
                    "provider error closed a live deal: {body}"
                );
            }
            assert_eq!(deal.session.recovery_episode(), None);
            assert_eq!(deal.session.terminal_action(), None);
            assert_eq!(harness.chain.foreign_money_calls(), 0);
            assert_eq!(deal.billable_tokens(), before);
            mode.store(0, Ordering::SeqCst);
            for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
                let (status, body) = ask(&harness, &store, path, false).await;
                assert_eq!(status, http::StatusCode::BAD_GATEWAY, "{body}");
                assert!(!deal.session.is_closed());
            }
            mode.store(3, Ordering::SeqCst);
            for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
                assert_eq!(
                    ask(&harness, &store, path, false).await.0,
                    http::StatusCode::OK
                );
            }
            assert_eq!(deal.billable_tokens(), before + 6);
            harness.shutdown().await;
            task.abort();
        }
    }
}

#[tokio::test]
async fn issue_2144_partial_output_without_usage_blocks_every_api_and_implicit_stop() {
    for native in [false, true] {
        for stream in [false, true] {
            for failure in [1, 2] {
                let mode = Arc::new(AtomicU8::new(3));
                let (upstream, task) = provider(mode.clone(), native).await;
                let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
                let dir = tempfile::tempdir().unwrap();
                let store = Arc::new(
                    UsageStore::open(dir.path().join("usage.sqlite3"), "test-network").unwrap(),
                );
                let deal = harness.deals.current().await.unwrap();
                assert_eq!(
                    ask(&harness, &store, "/v1/responses", false).await.0,
                    http::StatusCode::OK
                );
                mode.store(failure, Ordering::SeqCst);
                let (status, body) = ask(&harness, &store, "/v1/responses", stream).await;
                assert_eq!(
                    status,
                    if stream {
                        http::StatusCode::OK
                    } else {
                        http::StatusCode::BAD_GATEWAY
                    },
                    "{body}"
                );
                assert!(
                    !deal.session.is_closed(),
                    "uncertain usage is not terminal: {body}"
                );
                assert_eq!(deal.session.terminal_action(), None);
                assert_eq!(deal.billable_tokens(), 2);
                assert_eq!(harness.chain.foreign_money_calls(), 0);
                mode.store(3, Ordering::SeqCst);
                let calls = harness.seller.calls();
                for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
                    let (status, body) = ask(&harness, &store, path, false).await;
                    assert_eq!(
                        status,
                        http::StatusCode::SERVICE_UNAVAILABLE,
                        "{path}: {body}"
                    );
                    assert!(body.contains("upstream_usage_unresolved"), "{body}");
                    assert!(body.contains("read_only_reconciliation"), "{body}");
                }
                assert_eq!(
                    harness.seller.calls(),
                    calls,
                    "paused requests must not reach the gateway"
                );
                assert!(!deal.session.settle_on_exit("test-shutdown").await.unwrap());
                assert_eq!(harness.chain.foreign_money_calls(), 0);
                harness.shutdown().await;
                task.abort();
            }
        }
    }
}

#[tokio::test]
async fn issue_2144_unresolved_usage_survives_restart_and_only_exact_usage_can_reconcile_it() {
    let mode = Arc::new(AtomicU8::new(1));
    let (upstream, task) = provider(mode, false).await;
    let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
    let deal = harness.deals.current().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = Arc::new(UsageStore::open(&path, "test-network").unwrap());
    let (_, body) = ask(&harness, &store, "/v1/responses", true).await;
    assert!(
        body.contains("partial"),
        "the failed request delivered client-visible output"
    );
    assert!(store
        .has_unresolved_output_usage(&deal.route.token_contract)
        .unwrap());
    let reopened = Arc::new(UsageStore::open(&path, "test-network").unwrap());
    reopened.recover_interrupted_requests().unwrap();
    let resumed = SessionSettle::new(
        harness.chain.clone(),
        deal.route.token_contract.clone(),
        deal.session.note.clone(),
    );
    resumed.usage_quota.set(reopened.clone()).unwrap();
    assert_eq!(
        resumed.ensure_open_for_serving().await.unwrap_err().code,
        ConsumerErrorCode::UpstreamUsageUnresolved
    );
    assert!(!resumed.is_closed());
    assert!(!resumed
        .settle_on_exit("restarted-paused-session")
        .await
        .unwrap());
    // A later successful concurrent request or a different network cannot erase this request.
    reopened
        .start_request("other", "dexdo-mock", "openai", false)
        .unwrap();
    reopened
        .link_deal("other", &deal.route.token_contract)
        .unwrap();
    reopened
        .record_call_usage(
            "other",
            "answer",
            "answer",
            BillingUsage::new(1, 1, None).unwrap(),
            true,
        )
        .unwrap();
    assert!(reopened
        .has_unresolved_output_usage(&deal.route.token_contract)
        .unwrap());
    assert!(!UsageStore::open(&path, "other-network")
        .unwrap()
        .has_unresolved_output_usage(&deal.route.token_contract)
        .unwrap());
    // Reconciliation needs this exact request's validated and accepted terminal usage.
    let connection = rusqlite::Connection::open(&path).unwrap();
    let request: String = connection.query_row("SELECT request_id FROM request_usage WHERE phase='answer' AND call_id='in_progress' AND accepted=0", [], |row| row.get(0)).unwrap();
    let malformed = BillingUsage {
        total_tokens: 99,
        ..BillingUsage::new(1, 1, None).unwrap()
    };
    assert!(reopened
        .record_call_usage(&request, "answer", "answer", malformed, true)
        .is_err());
    assert!(reopened
        .has_unresolved_output_usage(&deal.route.token_contract)
        .unwrap());
    let valid = BillingUsage::new(1, 1, None).unwrap();
    reopened
        .record_call_usage(&request, "answer", "answer", valid, false)
        .unwrap();
    assert!(reopened
        .has_unresolved_output_usage(&deal.route.token_contract)
        .unwrap());
    reopened
        .record_call_usage(&request, "answer", "answer", valid, true)
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
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    harness.shutdown().await;
    task.abort();
}

proptest::proptest! {
    #[test]
    fn issue_2144_provider_origin_and_output_evidence_are_independent_from_retryability(
        received in 0u64..100,
        started in proptest::bool::ANY,
        retryable in proptest::bool::ANY,
        typed in proptest::bool::ANY,
    ) {
        let status = tonic::Status::data_loss("provider stream ended");
        let status = if retryable { dexdo_proto::retryable_upstream_status(status) } else { status };
        let status = dexdo_proto::upstream_failure_status(status, started);
        proptest::prop_assert_eq!(status.code(), tonic::Code::DataLoss);
        proptest::prop_assert_eq!(dexdo_proto::is_retryable_upstream(&status), retryable);
        let error = if typed { stream::CanonStreamError::Upstream(status) } else { stream::CanonStreamError::Local("dexdo-upstream-output: none".into()) };
        let expected = if typed {
            if started || received > 0 { StreamErrorPolicyAction::ReconcileUsage } else { StreamErrorPolicyAction::RequestScoped }
        } else if received == 0 { StreamErrorPolicyAction::DeadGateway } else { StreamErrorPolicyAction::SellerStallsMidStream };
        proptest::prop_assert_eq!(error.policy_action(received), expected);
    }
}

#[tokio::test]
async fn issue_2144_uncertain_usage_cancels_an_automatic_stop_already_in_preflight() {
    let deal = heartbeat_test_deal();
    let guard = deal.accepted_output_guard();
    assert!(guard.unchanged());
    deal.session
        .unresolved_output_usage
        .store(true, Ordering::SeqCst);
    assert!(
        !guard.unchanged(),
        "the final pre-submit guard must see the accounting veto without new output"
    );
    assert!(
        !deal
            .session
            .settle_dead_gateway("concurrent-gateway-failure", &guard)
            .await
    );
    assert!(!deal.session.is_closed());
    assert_eq!(deal.session.terminal_action(), None);
}

#[tokio::test]
async fn review_2146_failed_pause_write_survives_restart() {
    for fail_fallback in [false, true] {
        let mode = Arc::new(AtomicU8::new(3));
        let (upstream, task) = provider(mode.clone(), false).await;
        let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
        let deal = harness.deals.current().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = Arc::new(UsageStore::open(&path, "test-network").unwrap());
        assert_eq!(
            ask(&harness, &store, "/v1/responses", false).await.0,
            http::StatusCode::OK
        );
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TRIGGER lose_pause BEFORE INSERT ON request_usage WHEN NEW.phase='answer' AND NEW.call_id='in_progress' BEGIN SELECT RAISE(FAIL,'pause write failed'); END;").unwrap();
        if fail_fallback {
            connection.execute_batch("CREATE TRIGGER lose_fallback BEFORE UPDATE OF history_complete ON requests WHEN NEW.history_complete=0 BEGIN SELECT RAISE(FAIL,'fallback write failed'); END;").unwrap();
        }
        mode.store(1, Ordering::SeqCst);
        let (_, body) = ask(&harness, &store, "/v1/responses", true).await;
        assert!(body.contains("partial"));
        assert!(deal.session.unresolved_usage_rejection().is_some());
        let (complete, finished): (bool, Option<i64>) = connection.query_row(
            "SELECT history_complete,finished_at FROM requests WHERE deal_id=?1 ORDER BY started_at DESC,rowid DESC LIMIT 1",
            [&deal.route.token_contract], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        if fail_fallback {
            assert!(finished.is_none());
        } else {
            assert!(!complete);
        }
        connection
            .execute_batch("DROP TRIGGER lose_pause; DROP TRIGGER IF EXISTS lose_fallback;")
            .unwrap();
        let reopened = Arc::new(UsageStore::open(&path, "test-network").unwrap());
        reopened.recover_interrupted_requests().unwrap();
        let resumed = Arc::new(SessionSettle::new(
            harness.chain.clone(),
            deal.route.token_contract.clone(),
            deal.session.note.clone(),
        ));
        resumed.usage_quota.set(reopened.clone()).unwrap();
        let result = resumed.ensure_open_for_serving().await;
        assert!(
            matches!(result, Err(ref error) if error.code == ConsumerErrorCode::UpstreamUsageUnresolved),
            "lost pause write allowed resumed serving: {result:?}"
        );
        *harness.deals.active.write().await = Some(ApiDeal::new(
            deal.route.clone(),
            resumed.clone(),
            deal.content_gate.clone(),
        ));
        let calls = harness.seller.calls();
        mode.store(3, Ordering::SeqCst);
        assert_eq!(
            ask(&harness, &reopened, "/v1/chat/completions", false)
                .await
                .0,
            http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(harness.seller.calls(), calls);
        assert!(!resumed.settle_on_exit("shutdown").await.unwrap());
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        assert!(!resumed.is_closed());
        harness.shutdown().await;
        task.abort();
    }
}

#[tokio::test]
async fn review_2146_continuity_swap_respects_usage_pause() {
    let mode = Arc::new(AtomicU8::new(3));
    let (upstream, task) = provider(mode.clone(), false).await;
    let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
    let deal = harness.deals.current().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "test-network").unwrap());
    assert_eq!(
        ask(&harness, &store, "/v1/responses", false).await.0,
        http::StatusCode::OK
    );
    mode.store(1, Ordering::SeqCst);
    let (_, body) = ask(&harness, &store, "/v1/responses", true).await;
    assert!(body.contains("partial"));
    assert!(deal.session.unresolved_usage_rejection().is_some());
    let result = harness
        .deals
        .replace_active(
            || panic!("paused route must stay active"),
            "continuity-renewal",
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        harness.deals.current().await.unwrap().route.token_contract,
        deal.route.token_contract
    );
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    assert!(!deal.session.is_closed());
    // An operator's explicit STOP still uses the unguarded path.
    assert!(deal.session.settle("explicit-user-stop").await.unwrap());
    assert_eq!(harness.chain.foreign_money_calls(), 1);
    harness.shutdown().await;
    task.abort();
}

#[tokio::test]
async fn review_2146_continuity_pause_during_stop_preflight_keeps_previous_route() {
    let chain = Arc::new(RecordingSettleChain::default());
    let prototype = heartbeat_test_deal();
    let session = Arc::new(SessionSettle::new(
        chain.clone(),
        prototype.route.token_contract.clone(),
        prototype.session.note.clone(),
    ));
    let deal = ApiDeal::new(
        prototype.route.clone(),
        session.clone(),
        Arc::new(ContentGate::skip()),
    );
    let routes = Arc::new(RouteManager::new(deal));
    deliver_one_request(&routes, 1).await;
    let (started, ready) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
    let swap = {
        let routes = routes.clone();
        tokio::spawn(async move {
            routes
                .replace_active(
                    || panic!("veto must retain the old route"),
                    "continuity-renewal",
                )
                .await
        })
    };
    ready.await.unwrap();
    session
        .unresolved_output_usage
        .store(true, Ordering::SeqCst);
    release.notify_one();
    assert!(swap.await.unwrap().is_err());
    assert!(Arc::ptr_eq(
        &routes.current().await.unwrap().session,
        &session
    ));
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
    assert!(!session.is_closed());
    assert!(!session.is_settled());
}

#[tokio::test]
async fn review_2146_upstream_resource_exhausted_after_output_must_pause() {
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for streamed in [false, true] {
            let mode = Arc::new(AtomicU8::new(3));
            let (upstream, task) = provider(mode.clone(), false).await;
            let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
            let deal = harness.deals.current().await.unwrap();
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(
                UsageStore::open(dir.path().join("usage.sqlite3"), "test-network").unwrap(),
            );
            assert_eq!(
                ask(&harness, &store, path, false).await.0,
                http::StatusCode::OK
            );
            mode.store(5, Ordering::SeqCst);
            let (status, body) = ask(&harness, &store, path, streamed).await;
            if streamed {
                assert!(
                    body.contains("partial"),
                    "must render before oversized frame: {body}"
                );
                assert!(body.contains("error") || body.contains("response.failed"));
            } else {
                assert_eq!(status, http::StatusCode::BAD_GATEWAY, "{body}");
            }
            assert!(deal.session.unresolved_usage_rejection().is_some());
            assert!(!body.contains("\"capacity\""), "{body}");
            let calls = harness.seller.calls();
            mode.store(3, Ordering::SeqCst);
            let (status, body) = ask(&harness, &store, path, streamed).await;
            assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert!(body.contains("upstream_usage_unresolved"));
            assert_eq!(harness.seller.calls(), calls);
            assert_eq!(harness.chain.foreign_money_calls(), 0);
            assert!(!deal.session.is_closed());
            harness.shutdown().await;
            task.abort();
        }
    }
    let capacity =
        stream::CanonStreamError::Upstream(tonic::Status::resource_exhausted("seller busy"));
    assert!(
        capacity.is_capacity(),
        "genuine seller capacity remains request scoped"
    );
}

#[tokio::test]
async fn review_2146_remaining_exit_pause_race() {
    let chain = Arc::new(RecordingSettleChain::default());
    let prototype = heartbeat_test_deal();
    let session = Arc::new(SessionSettle::new(
        chain.clone(),
        prototype.route.token_contract.clone(),
        prototype.session.note.clone(),
    ));
    let deal = ApiDeal::new(
        prototype.route.clone(),
        session.clone(),
        Arc::new(ContentGate::skip()),
    );
    let routes = RouteManager::new(deal);
    deliver_one_request(&routes, 1).await;
    let lock = session.settle_lock.lock().await;
    let mut exit = Box::pin(session.settle_on_exit("shutdown"));
    assert!(matches!(
        futures::poll!(exit.as_mut()),
        std::task::Poll::Pending
    ));
    session
        .unresolved_output_usage
        .store(true, Ordering::SeqCst);
    drop(lock);
    let settled = exit.await.unwrap();
    assert!(
        !settled,
        "shutdown STOP landed after unresolved pause: stop_calls={}",
        chain.stop_calls.load(Ordering::SeqCst)
    );
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    assert!(!session.is_closed());
}

#[tokio::test]
async fn review_2146_remaining_verification_bail_pause_race() {
    let chain = Arc::new(RecordingSettleChain::default());
    let prototype = heartbeat_test_deal();
    let session = Arc::new(SessionSettle::new(
        chain.clone(),
        prototype.route.token_contract.clone(),
        prototype.session.note.clone(),
    ));
    let lock = session.settle_lock.lock().await;
    let mut bail = Box::pin(session.settle_verification_bail("verify-bail"));
    assert!(matches!(
        futures::poll!(bail.as_mut()),
        std::task::Poll::Pending
    ));
    session
        .unresolved_output_usage
        .store(true, Ordering::SeqCst);
    drop(lock);
    let settled = bail.await;
    assert!(
        !settled,
        "verification bail STOP landed after unresolved pause: stop_calls={}",
        chain.stop_calls.load(Ordering::SeqCst)
    );
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    assert!(
        session.is_closed(),
        "known verification bail must still block admission"
    );
    assert!(!session.is_settled());
    assert_eq!(session.terminal_action(), None);
}

#[tokio::test]
async fn review_2146_remaining_automatic_stop_preflight_and_control() {
    for verification in [false, true] {
        for pause in [false, true] {
            let chain = Arc::new(RecordingSettleChain::default());
            let prototype = heartbeat_test_deal();
            let session = Arc::new(SessionSettle::new(
                chain.clone(),
                prototype.route.token_contract.clone(),
                prototype.session.note.clone(),
            ));
            let deal = ApiDeal::new(
                prototype.route.clone(),
                session.clone(),
                Arc::new(ContentGate::skip()),
            );
            let routes = RouteManager::new(deal);
            deliver_one_request(&routes, 1).await;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("usage.sqlite3");
            let store = Arc::new(UsageStore::open(&path, "test-network").unwrap());
            store
                .open_deal_quota(&session.token_contract, 3, 1)
                .unwrap();
            session.usage_quota.set(store).unwrap();
            let (started, mut ready) = tokio::sync::oneshot::channel();
            let release = Arc::new(tokio::sync::Notify::new());
            *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
            let mut stop = Box::pin(async {
                if verification {
                    session.settle_verification_bail("verification-bail").await
                } else {
                    session.settle_on_exit("shutdown").await.unwrap()
                }
            });
            assert!(matches!(
                futures::poll!(stop.as_mut()),
                std::task::Poll::Pending
            ));
            ready
                .try_recv()
                .expect("automatic STOP entered transaction preflight");
            session
                .unresolved_output_usage
                .store(pause, Ordering::SeqCst);
            release.notify_one();
            assert_eq!(stop.await, !pause);
            assert_eq!(
                chain.stop_calls.load(Ordering::SeqCst)
                    + chain.recovery_stop_calls.load(Ordering::SeqCst),
                usize::from(!pause)
            );
            assert_eq!(session.is_closed(), verification || !pause);
            assert_eq!(session.is_settled(), !pause);
            if pause {
                assert_eq!(session.terminal_action(), None);
                assert_eq!(session.recovery_episode(), None);
                let closed: bool = rusqlite::Connection::open(&path)
                    .unwrap()
                    .query_row(
                        "SELECT closed FROM deal_quota WHERE deal_id=?1",
                        [&session.token_contract],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(!closed, "a veto must preserve the durable quota");
            }
        }
    }
}

#[tokio::test]
async fn review_2146_remaining_binding_failure_rejects_before_paid_work() {
    for verification in [false, true] {
        for endpoint in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
            let mode = Arc::new(AtomicU8::new(1));
            let (upstream, task) = provider(mode, false).await;
            let harness = if verification {
                weekly_route_harness_gated(
                    0,
                    true,
                    SUB_WEEK_LEN.as_secs(),
                    UNCONSTRAINED_UPSTREAM,
                    ContentGate::probe(
                        "dexdo-mock".into(),
                        probe_models(
                            "identity probe",
                            "http://127.0.0.1:1/v1",
                            "DEXDO_2144_UNUSED_REFERENCE",
                        ),
                    ),
                    upstream,
                )
                .await
            } else {
                harness(upstream, SessionLifetimePolicy::SettleOnExit).await
            };
            let deal = harness.deals.current().await.unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("usage.sqlite3");
            let store = Arc::new(UsageStore::open(&path, "test-network").unwrap());
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection.execute_batch("CREATE TRIGGER lose_binding BEFORE UPDATE OF deal_id ON requests BEGIN SELECT RAISE(FAIL,'binding write failed'); END;").unwrap();
            let (status, body) = ask(&harness, &store, endpoint, true).await;
            assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert!(body.contains("usage_unavailable"), "{body}");
            assert_eq!(
                harness.seller.calls(),
                0,
                "binding must precede paid verification and answer"
            );
            assert_eq!(deal.billable_tokens(), 0);
            assert_eq!(harness.chain.foreign_money_calls(), 0);
            assert!(!deal.session.is_closed());
            let (bound, finished, complete): (Option<String>, Option<i64>, bool) = connection
                .query_row(
                    "SELECT deal_id,finished_at,history_complete FROM requests",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert!(bound.is_none());
            assert!(
                finished.is_some(),
                "rejected admission has no uncertain work"
            );
            assert!(complete);
            let reopened = Arc::new(UsageStore::open(&path, "test-network").unwrap());
            reopened.recover_interrupted_requests().unwrap();
            assert!(!reopened
                .has_unresolved_output_usage(&deal.route.token_contract)
                .unwrap());
            // A real restart binds the resumed session to the reopened store's runtime.
            let resumed = Arc::new(SessionSettle::new(
                harness.chain.clone(),
                deal.route.token_contract.clone(),
                deal.session.note.clone(),
            ));
            resumed.usage_quota.set(reopened.clone()).unwrap();
            *harness.deals.active.write().await = Some(ApiDeal::new(
                deal.route.clone(),
                resumed,
                deal.content_gate.clone(),
            ));
            // Successful binding restores admission, and partial work is now visible after restart.
            connection
                .execute_batch("DROP TRIGGER lose_binding;")
                .unwrap();
            let (status, body) = ask(&harness, &reopened, endpoint, true).await;
            if verification {
                assert_eq!(status, http::StatusCode::BAD_GATEWAY, "{body}");
                assert!(
                    body.contains("model identity verification failed"),
                    "{body}"
                );
            } else {
                assert!(body.contains("partial"), "{body}");
            }
            assert!(harness.seller.calls() > 0);
            let recovered = Arc::new(UsageStore::open(&path, "test-network").unwrap());
            recovered.recover_interrupted_requests().unwrap();
            assert!(recovered
                .has_unresolved_output_usage(&deal.route.token_contract)
                .unwrap());
            let resumed = Arc::new(SessionSettle::new(
                harness.chain.clone(),
                deal.route.token_contract.clone(),
                deal.session.note.clone(),
            ));
            resumed.usage_quota.set(recovered.clone()).unwrap();
            *harness.deals.active.write().await = Some(ApiDeal::new(
                deal.route.clone(),
                resumed.clone(),
                deal.content_gate.clone(),
            ));
            let calls = harness.seller.calls();
            assert_eq!(
                ask(&harness, &recovered, endpoint, false).await.0,
                http::StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(harness.seller.calls(), calls);
            assert!(!resumed.settle_on_exit("shutdown").await.unwrap());
            assert!(!resumed.settle_verification_bail("verification-bail").await);
            assert_eq!(harness.chain.foreign_money_calls(), 0);
            assert!(!resumed.is_closed());
            harness.shutdown().await;
            task.abort();
        }
    }
}

#[tokio::test]
async fn review_2146_remaining_automatic_terminal_lock_waits_recheck_pause() {
    for kind in 0..4 {
        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_verification_bail_action(
            chain.clone(),
            "tc-automatic".into(),
            Arc::new(dexdo_core::LocalNote::generate()),
            VerificationBailAction::Dispute,
        );
        let lock = session.settle_lock.lock().await;
        let heartbeat = unchanged_heartbeat();
        let mut terminal = Box::pin(async {
            match kind {
                0 => session
                    .recover_cleanup_unopened(true)
                    .await
                    .unwrap()
                    .is_some(),
                1 => session
                    .recover_reclaim_opened(&heartbeat, true)
                    .await
                    .unwrap()
                    .is_some(),
                2 => {
                    session
                        .policy_dispute("seller_stalls_mid_stream", "dispute", "test")
                        .await
                }
                _ => session.settle_verification_bail("test").await,
            }
        });
        assert!(matches!(
            futures::poll!(terminal.as_mut()),
            std::task::Poll::Pending
        ));
        session
            .unresolved_output_usage
            .store(true, Ordering::SeqCst);
        drop(lock);
        assert!(!terminal.await);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
        assert_eq!(session.is_closed(), kind == 3);
        assert!(!session.is_settled());
        assert_eq!(session.recovery_episode(), None);
    }
}

#[tokio::test]
async fn review_2146_bail_preflight_must_block_further_paid_answers() {
    let mode = Arc::new(AtomicU8::new(3));
    let (upstream, task) = provider(mode, false).await;
    let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
    let original = harness.deals.current().await.unwrap();
    let chain = Arc::new(RecordingSettleChain::default());
    *chain.deal_state.lock().unwrap() = Some(opened_deal_state(1_000_000_000, 0));
    *chain.purchase_terms.lock().unwrap() = Some((3, 1_000_000_000));
    let session = Arc::new(SessionSettle::new(
        chain.clone(),
        original.route.token_contract.clone(),
        original.session.note.clone(),
    ));
    let deal = ApiDeal::new(
        original.route.clone(),
        session.clone(),
        Arc::new(ContentGate::skip()),
    );
    *harness.deals.active.write().await = Some(deal.clone());
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "test-network").unwrap());
    let (warm, body) = ask(&harness, &store, "/v1/chat/completions", false).await;
    assert_eq!(warm, http::StatusCode::OK, "{body}");
    let before_calls = harness.seller.calls();
    let before_paid = deal.billable_tokens();
    let (started, mut ready) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
    let mut bail = Box::pin(session.settle_verification_bail("verify-bail"));
    assert!(matches!(
        futures::poll!(bail.as_mut()),
        std::task::Poll::Pending
    ));
    ready.try_recv().expect("verification STOP is in preflight");
    let (status, body) = ask(&harness, &store, "/v1/chat/completions", false).await;
    let added_calls = harness.seller.calls() - before_calls;
    let added_paid = deal.billable_tokens() - before_paid;
    release.notify_one();
    assert!(bail.await);
    harness.shutdown().await;
    task.abort();
    assert_ne!(status, http::StatusCode::OK, "known verification bail still served paid answer during STOP preflight: upstream_calls={added_calls}, billable_tokens={added_paid}, body={}",body);
    assert_eq!(added_calls, 0);
    assert_eq!(added_paid, 0);
}

#[tokio::test]
async fn review_2146_verification_admission_across_lock_preflight_and_result() {
    for (boundary, outcome, cached_pass) in [
        ("lock", "stop", false),
        ("preflight", "stop", false),
        ("result", "stop", false),
        ("lock", "pause", false),
        ("preflight", "pause", true),
        ("preflight", "failure", false),
        ("preflight", "ambiguous", false),
        ("preflight", "cancel", false),
    ] {
        let (upstream, task) = provider(Arc::new(AtomicU8::new(3)), false).await;
        let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
        let original = harness.deals.current().await.unwrap();
        let chain = Arc::new(RecordingSettleChain::default());
        chain.set_deal_state(opened_deal_state(3_000_000_000, 0));
        *chain.purchase_terms.lock().unwrap() = Some((3, 1_000_000_000));
        let session = Arc::new(SessionSettle::new(
            chain.clone(),
            original.route.token_contract.clone(),
            original.session.note.clone(),
        ));
        let gate = if cached_pass {
            let gate = ContentGate::probe(
                "dexdo-mock".into(),
                probe_models("probe", "http://127.0.0.1:1/v1", "DEXDO_2144_NO_REFERENCE"),
            );
            gate.verdict.set(Ok(())).unwrap();
            gate
        } else {
            ContentGate::skip()
        };
        let deal = ApiDeal::new(original.route.clone(), session.clone(), Arc::new(gate));
        *harness.deals.active.write().await = Some(deal.clone());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = Arc::new(UsageStore::open(&path, "test-network").unwrap());
        assert_eq!(
            ask(&harness, &store, "/v1/chat/completions", false).await.0,
            http::StatusCode::OK
        );
        let before_calls = harness.seller.calls();
        let before_paid = deal.billable_tokens();
        let (started, mut ready) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        if boundary == "result" {
            *chain.stop_result_gate.lock().unwrap() = Some((started, release.clone()));
        } else {
            *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
        }
        chain
            .fail_recovery_stop
            .store(outcome == "failure", Ordering::SeqCst);
        chain
            .ambiguous_recovery_stop
            .store(outcome == "ambiguous", Ordering::SeqCst);
        let mut lock = if boundary == "lock" {
            Some(session.settle_lock.lock().await)
        } else {
            None
        };
        let mut bail = Box::pin(session.settle_verification_bail("verify-bail"));
        assert!(matches!(
            futures::poll!(bail.as_mut()),
            std::task::Poll::Pending
        ));
        if boundary != "lock" {
            ready.try_recv().expect("requested STOP boundary reached");
        }
        let first = session.diagnostics.lock().unwrap().closure.clone().unwrap();
        assert_eq!(first.category, ClosureCategory::VerificationBail);
        for stream in [false, true] {
            for endpoint in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
                let (status, body) = ask(&harness, &store, endpoint, stream).await;
                assert_eq!(
                    status,
                    http::StatusCode::GONE,
                    "{boundary}/{outcome}: {body}"
                );
                let error: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(error["error"]["closure"]["category"], "verification_bail");
                assert!(error["error"]["terminal_action"].is_null());
            }
        }
        assert_eq!(harness.seller.calls(), before_calls);
        assert_eq!(deal.billable_tokens(), before_paid);
        assert!(!session.is_settled());
        if outcome == "pause" {
            // Same production pause setter, modelling uncertainty from already admitted work.
            let mut running = deal.begin_request(unix_now_secs());
            running.pause_unresolved_usage();
        } else if outcome == "stop" {
            // New accepted output alone must not cancel a known verification verdict.
            deal.record_accepted_output(unix_now_secs());
        }
        drop(lock.take());
        let settled = if outcome == "cancel" {
            drop(bail);
            false
        } else {
            release.notify_one();
            bail.await
        };
        assert_eq!(settled, outcome == "stop");
        assert_eq!(
            chain.recovery_stop_calls.load(Ordering::SeqCst),
            usize::from(!matches!(outcome, "pause" | "cancel"))
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(session.is_settled(), settled);
        let quota_closed: bool = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT closed FROM deal_quota WHERE deal_id=?1",
                [&session.token_contract],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(quota_closed, settled);
        assert_eq!(
            session.diagnostics.lock().unwrap().closure.as_ref(),
            Some(&first)
        );
        for stream in [false, true] {
            for endpoint in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
                let (status, body) = ask(&harness, &store, endpoint, stream).await;
                assert_eq!(
                    status,
                    if outcome == "pause" {
                        http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        http::StatusCode::GONE
                    },
                    "{body}"
                );
                if outcome == "pause" {
                    assert!(body.contains("upstream_usage_unresolved"));
                    assert!(body.contains("read_only_reconciliation"));
                }
            }
        }
        assert_eq!(harness.seller.calls(), before_calls);
        assert_eq!(deal.billable_tokens(), before_paid);
        if outcome == "ambiguous" {
            assert!(!session.settle_verification_bail("repeat-bail").await);
            assert!(session.settle("operator-after-ambiguous").await.is_err());
            assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 1);
            assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        } else if !settled {
            assert_ne!(
                session.diagnostics.lock().unwrap().recovery_state,
                RecoveryState::Pending
            );
            assert!(session.settle("explicit-operator-stop").await.unwrap());
            assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
            // A proven terminal fact remains stronger than the earlier unresolved local pause.
            let (status, body) = ask(&harness, &store, "/v1/responses", false).await;
            assert_eq!(status, http::StatusCode::GONE, "{body}");
            assert!(body.contains("terminal_confirmed"));
        }
        harness.shutdown().await;
        task.abort();
    }
}

#[tokio::test]
async fn review_2146_admitted_verification_waiters_recheck_before_paid_work() {
    for pause in [false, true] {
        for cached_pass in [false, true] {
            let (upstream, task) = provider(Arc::new(AtomicU8::new(3)), false).await;
            let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
            let original = harness.deals.current().await.unwrap();
            let chain = Arc::new(RecordingSettleChain::default());
            chain.set_deal_state(opened_deal_state(3_000_000_000, 0));
            *chain.purchase_terms.lock().unwrap() = Some((3, 1_000_000_000));
            let session = Arc::new(SessionSettle::new(
                chain.clone(),
                original.route.token_contract.clone(),
                original.session.note.clone(),
            ));
            let mut deal = ApiDeal::new(
                original.route.clone(),
                session.clone(),
                Arc::new(ContentGate::skip()),
            );
            *harness.deals.active.write().await = Some(deal.clone());
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(
                UsageStore::open(dir.path().join("usage.sqlite3"), "test-network").unwrap(),
            );
            assert_eq!(
                ask(&harness, &store, "/v1/chat/completions", false).await.0,
                http::StatusCode::OK
            );
            let gate = Arc::new(ContentGate::probe(
                "dexdo-mock".into(),
                probe_models_expecting(
                    "probe",
                    "OK",
                    "http://127.0.0.1:1/v1",
                    "DEXDO_2144_NO_REFERENCE",
                ),
            ));
            deal.content_gate = gate.clone();
            *harness.deals.active.write().await = Some(deal.clone());
            let before_calls = harness.seller.calls();
            let before_paid = deal.billable_tokens();
            // Hold the initializer permit so the real HTTP request is admitted but cannot start B8.
            let (initialize, completed) = tokio::sync::oneshot::channel::<()>();
            let mut initializer = Box::pin(gate.verdict.get_or_try_init(|| async {
                completed.await.unwrap();
                Ok::<Result<(), String>, String>(Ok(()))
            }));
            assert!(matches!(
                futures::poll!(initializer.as_mut()),
                std::task::Poll::Pending
            ));
            let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let state = direct_state_with_deliveries(&harness, sender).with_usage(store.clone());
            let mut waiter = Box::pin(router(state, true).oneshot(http::Request::post("/v1/chat/completions")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({"model":"dexdo-mock","messages":[{"role":"user","content":"question"}],"max_tokens":4}).to_string())).unwrap()));
            assert!(matches!(
                futures::poll!(waiter.as_mut()),
                std::task::Poll::Pending
            ));
            assert_eq!(deal.active_requests.load(Ordering::SeqCst), 1);
            assert_eq!(harness.seller.calls(), before_calls);
            let release = Arc::new(tokio::sync::Notify::new());
            let (started, mut ready) = tokio::sync::oneshot::channel();
            *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
            let mut bail = Box::pin(session.settle_verification_bail("verify-bail"));
            if pause {
                let mut running = deal.begin_request(unix_now_secs());
                running.pause_unresolved_usage();
            } else {
                assert!(matches!(
                    futures::poll!(bail.as_mut()),
                    std::task::Poll::Pending
                ));
                ready
                    .try_recv()
                    .expect("verification STOP is held in preflight");
            }
            if cached_pass {
                initialize.send(()).unwrap();
                assert!(initializer.await.unwrap().is_ok());
            } else {
                drop(initializer);
                drop(initialize);
            }
            let response = tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("waiter must reject without upstream/settlement waits")
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(
                status,
                if pause {
                    http::StatusCode::SERVICE_UNAVAILABLE
                } else {
                    http::StatusCode::GONE
                },
                "{body}"
            );
            if pause {
                assert!(body.contains("upstream_usage_unresolved"), "{body}");
            }
            assert_eq!(harness.seller.calls(), before_calls);
            assert_eq!(deal.billable_tokens(), before_paid);
            if pause {
                drop(bail);
            } else {
                release.notify_one();
                assert!(bail.await);
            }
            harness.shutdown().await;
            task.abort();
        }
    }
}

#[tokio::test]
async fn review_2146_cancelled_recovery_cannot_reopen_verification_admission() {
    let (upstream, task) = provider(Arc::new(AtomicU8::new(3)), false).await;
    let harness = harness(upstream, SessionLifetimePolicy::SettleOnExit).await;
    let original = harness.deals.current().await.unwrap();
    let chain = Arc::new(RecordingSettleChain::default());
    chain.set_deal_state(opened_deal_state(3_000_000_000, 0));
    *chain.purchase_terms.lock().unwrap() = Some((3, 1_000_000_000));
    let session = Arc::new(SessionSettle::new(
        chain.clone(),
        original.route.token_contract.clone(),
        original.session.note.clone(),
    ));
    let deal = ApiDeal::new(
        original.route.clone(),
        session.clone(),
        Arc::new(ContentGate::skip()),
    );
    *harness.deals.active.write().await = Some(deal.clone());
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "test-network").unwrap());
    assert_eq!(
        ask(&harness, &store, "/v1/chat/completions", false).await.0,
        http::StatusCode::OK
    );
    let before_calls = harness.seller.calls();
    let before_paid = deal.billable_tokens();
    let (started, mut ready) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
    let heartbeat = deal.accepted_output_guard();
    let mut recovery = Box::pin(session.recover_reclaim_opened(&heartbeat, true));
    assert!(matches!(
        futures::poll!(recovery.as_mut()),
        std::task::Poll::Pending
    ));
    ready
        .try_recv()
        .expect("recovery holds settle_lock in preflight");
    let first = session.diagnostics.lock().unwrap().closure.clone().unwrap();
    let mut bail = Box::pin(session.settle_verification_bail("verify-bail"));
    assert!(matches!(
        futures::poll!(bail.as_mut()),
        std::task::Poll::Pending
    ));
    // Cancel stale recovery through its normal heartbeat path, without accounting uncertainty.
    deal.record_accepted_output(unix_now_secs());
    release.notify_one();
    assert!(recovery.await.unwrap().is_none());
    // Intentionally leave the verification waiter unpolled: cancellation must not reopen admission.
    let (status, body) = ask(&harness, &store, "/v1/responses", false).await;
    let calls = harness.seller.calls();
    let paid = deal.billable_tokens();
    assert!(bail.await);
    assert_eq!(
        status,
        http::StatusCode::GONE,
        "cancelled recovery reopened a known verification failure: {body}"
    );
    assert_eq!(calls, before_calls);
    assert_eq!(paid, before_paid);
    assert_eq!(
        chain.recovery_stop_calls.load(Ordering::SeqCst),
        1,
        "only verification STOP may submit"
    );
    assert_eq!(
        session.diagnostics.lock().unwrap().closure.as_ref(),
        Some(&first)
    );
    harness.shutdown().await;
    task.abort();
}
