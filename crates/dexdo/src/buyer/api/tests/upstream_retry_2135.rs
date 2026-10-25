use super::*;
use axum::{response::IntoResponse, routing::post, Router};
use std::sync::atomic::{AtomicU8, AtomicUsize};

// 429 exhausts the existing retry budget immediately when Retry-After exceeds it.
// Two 429 responses followed by empty HTTP 200 responses exercise the DataLoss retry ladder.
async fn provider(
    mode: Arc<AtomicU8>,
    native: bool,
) -> (crate::seller::UpstreamConfig, tokio::task::JoinHandle<()>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler = move || {
        let mode = mode.clone();
        let attempts = attempts.clone();
        async move {
            if mode.load(Ordering::SeqCst) == 4 {
                let body = axum::body::Body::from_stream(async_stream::stream! {
                    yield Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                        b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    ));
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    yield Err(std::io::Error::other("fixture body disconnected"));
                });
                return ([("content-type", "text/event-stream")], body).into_response();
            }
            match mode.load(Ordering::SeqCst) {
                0 => (
                    http::StatusCode::TOO_MANY_REQUESTS,
                    [
                        (
                            "retry-after",
                            dexdo_core::params::SellerLivenessParams::canonical()
                                .health_cycle_timeout
                                .as_secs()
                                .to_string(),
                        ),
                        ("content-type", "text/event-stream".to_string()),
                    ],
                    String::new(),
                ),
                1 => (
                    if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                        http::StatusCode::TOO_MANY_REQUESTS
                    } else {
                        http::StatusCode::OK
                    },
                    [
                        ("retry-after", "0".to_string()),
                        ("content-type", "text/event-stream".to_string()),
                    ],
                    String::new(),
                ),
                3 => (
                    http::StatusCode::OK,
                    [
                        ("retry-after", "0".to_string()),
                        ("content-type", "text/event-stream".to_string()),
                    ],
                    String::new(),
                ),
                _ => (
                    http::StatusCode::OK,
                    [
                        ("retry-after", "0".to_string()),
                        ("content-type", "text/event-stream".to_string()),
                    ],
                    if native {
                        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_stop\"}\n\n".to_string()
                    } else {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n".to_string()
                    },
                ),
            }
            .into_response()
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
    let env = format!("DEXDO_2135_FIXTURE_{}", addr.port());
    std::env::set_var(&env, "fixture-key");
    let cfg = crate::seller::OpenAiConfig {
        base_url: format!("http://{addr}"),
        model: "dexdo-mock".into(),
        frame_model: "dexdo-mock".into(),
        api_key_env: env,
        capabilities: crate::seller::Capabilities {
            max_output_tokens: Some(8),
            ..Default::default()
        },
        ..Default::default()
    };
    let upstream = if native {
        crate::seller::UpstreamConfig::Anthropic(crate::seller::AnthropicConfig {
            base_url: cfg.base_url,
            model: cfg.model,
            frame_model: cfg.frame_model,
            api_key_env: cfg.api_key_env,
            tokenizer_family: cfg.tokenizer_family,
            max_output_tokens: cfg.capabilities.max_output_tokens,
            reasoning_disable: Default::default(),
        })
    } else {
        crate::seller::UpstreamConfig::OpenAi(cfg)
    };
    (upstream, task)
}

async fn ask(harness: &WeeklyRouteHarness, path: &str, stream: bool) -> (http::StatusCode, String) {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = direct_consumer_response(
        direct_state_with_deliveries(harness, tx),
        path,
        4,
        "question",
        stream,
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn assert_preserved(path: &str, stream: bool, initial_mode: u8, native: bool) {
    for lifetime in [
        SessionLifetimePolicy::SettleOnExit,
        SessionLifetimePolicy::Preserve,
    ] {
        assert_preserved_with_lifetime(path, stream, initial_mode, native, lifetime).await;
    }
}

async fn assert_preserved_with_lifetime(
    path: &str,
    stream: bool,
    initial_mode: u8,
    native: bool,
    lifetime: SessionLifetimePolicy,
) {
    let mode = Arc::new(AtomicU8::new(initial_mode));
    let (upstream, task) = provider(mode.clone(), native).await;
    let harness = weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
        claimed: PROBE_CLAIM,
        subscription: false,
        expires_in: SUB_WEEK_LEN.as_secs(),
        upstream_tokens: UNCONSTRAINED_UPSTREAM,
        content_gate: ContentGate::skip(),
        upstream,
        failure_policy: BuyerApiFailurePolicy::default(),
        lifetime,
    })
    .await;
    let deal = harness.deals.current().await.unwrap();
    let tc = deal.route.token_contract.clone();
    // A deal with earlier paid usage must also survive the failed request. Under
    // SettleOnExit this exercises the real recovery lever, rather than a zero-usage veto.
    mode.store(2, Ordering::SeqCst);
    assert_eq!(ask(&harness, path, stream).await.0, reqwest::StatusCode::OK);
    let prior_usage = deal.billable_tokens();
    assert_eq!(prior_usage, 2);
    mode.store(initial_mode, Ordering::SeqCst);
    let failures = if initial_mode == 0 { 2 } else { 1 };
    for _ in 0..failures {
        let (status, body) = ask(&harness, path, stream).await;
        assert_eq!(
            status,
            if stream {
                reqwest::StatusCode::OK
            } else {
                reqwest::StatusCode::BAD_GATEWAY
            },
            "{body}"
        );
        assert!(!deal.session.is_closed(), "{path}: {body}");
        assert_eq!(deal.session.recovery_episode(), None);
        assert_eq!(deal.session.terminal_action(), None);
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        assert_eq!(deal.billable_tokens(), prior_usage);
        assert!(!deal.has_active_request());
        let failure = harness.seller.recv_upstream_failure().await;
        assert!(failure.retryable, "{failure:?}");
        assert_eq!(failure.http_status, (initial_mode == 0).then_some(429));
        assert_eq!(
            failure.grpc_status,
            if matches!(initial_mode, 0 | 4) {
                "Unavailable"
            } else {
                "DataLoss"
            }
        );
        assert!(body.contains("upstream_retryable"), "{path}: {body}");
        assert!(body.contains("retry_with_backoff"), "{path}: {body}");
        if initial_mode == 4 {
            assert!(body.contains("upstream read failed:"), "{path}: {body}");
        }
    }
    mode.store(2, Ordering::SeqCst);
    let (status, body) = ask(&harness, path, stream).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    assert_eq!(
        harness.deals.current().await.unwrap().route.token_contract,
        tc
    );
    assert!(!deal.session.is_closed());
    assert_eq!(deal.billable_tokens(), prior_usage + 2);
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    assert!(!harness.seller.server_task.is_finished());
    harness.shutdown().await;
    task.abort();
}

#[tokio::test]
async fn issue_2135_throttled_request_preserves_the_deal_on_every_api() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for stream in [false, true] {
            for native in [false, true] {
                assert_preserved(path, stream, 0, native).await;
            }
        }
    }
}

#[tokio::test]
async fn issue_2135_native_pre_output_transport_failures_preserve_the_deal_on_every_api() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for stream in [false, true] {
            for failure_mode in [3, 4] {
                assert_preserved(path, stream, failure_mode, true).await;
            }
        }
    }
}

#[tokio::test]
async fn issue_2135_exhausted_pre_output_dataloss_retries_preserve_the_deal() {
    assert_preserved("/v1/chat/completions", false, 1, false).await;
}

proptest::proptest! {
    #[test]
    fn issue_2135_only_typed_pre_output_failures_bypass_terminal_policy(
        received in 0u64..100,
        typed in proptest::bool::ANY,
        from_gateway in proptest::bool::ANY,
        dataloss in proptest::bool::ANY,
    ) {
        let status = if dataloss { tonic::Status::data_loss("provider stream ended") }
            else { tonic::Status::unavailable("connection failed") };
        let status = if typed { dexdo_proto::retryable_upstream_status(status) } else { status };
        let error = if from_gateway { stream::CanonStreamError::Upstream(status) }
            else { stream::CanonStreamError::Local("dexdo-error-code: upstream_retryable".into()) };
        let expected = if received > 0 { StreamErrorPolicyAction::SellerStallsMidStream }
            else if typed && from_gateway { StreamErrorPolicyAction::RequestScoped }
            else { StreamErrorPolicyAction::DeadGateway };
        proptest::prop_assert_eq!(error.policy_action(received), expected);
    }
}
