use super::*;
use axum::{response::IntoResponse, routing::post, Router};
use std::sync::atomic::AtomicU8;

/// Mode 0 throttles with `retry-after`; 2 answers; 3 answers HTTP 200 with an empty body; 4 sends
/// a Messages start and then drops the connection.
async fn provider(
    mode: Arc<AtomicU8>,
    native: bool,
) -> (crate::seller::UpstreamConfig, tokio::task::JoinHandle<()>) {
    let handler = move |uri: http::Uri, body: axum::body::Bytes| {
        let mode = mode.clone();
        async move {
            let stream = serde_json::from_slice::<serde_json::Value>(&body)
                .is_ok_and(|body| body["stream"] == true);
            let path = uri.path().trim_start_matches("/v1").to_string();
            match mode.load(Ordering::SeqCst) {
                0 => (
                    http::StatusCode::TOO_MANY_REQUESTS,
                    [
                        ("retry-after", RETRY_AFTER),
                        ("content-type", "application/json"),
                    ],
                    String::new(),
                )
                    .into_response(),
                3 => (
                    http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    String::new(),
                )
                    .into_response(),
                4 => {
                    let body = axum::body::Body::from_stream(async_stream::stream! {
                        yield Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                        ));
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        yield Err(std::io::Error::other("fixture body disconnected"));
                    });
                    ([("content-type", "text/event-stream")], body).into_response()
                }
                _ => {
                    let (content_type, body) = match (path.as_str(), stream) {
                        ("/chat/completions", true) => ("text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n"),
                        ("/chat/completions", false) => ("application/json", "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}"),
                        ("/responses", true) => ("text/event-stream", "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"),
                        ("/responses", false) => ("application/json", "{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}"),
                        (_, true) => ("text/event-stream", "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_stop\"}\n\n"),
                        (_, false) => ("application/json", "{\"type\":\"message\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}],\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}"),
                    };
                    ([("content-type", content_type)], body).into_response()
                }
            }
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/chat/completions", post(handler.clone()))
                .route("/responses", post(handler.clone()))
                .route("/v1/messages", post(handler)),
        )
        .await
        .unwrap();
    });
    let env = format!("DEXDO_2135_FIXTURE_{}", addr.port());
    std::env::set_var(&env, "fixture-key");
    let upstream = if native {
        crate::seller::UpstreamConfig::Anthropic(crate::seller::AnthropicConfig {
            base_url: format!("http://{addr}"),
            model: "dexdo-mock".into(),
            frame_model: "dexdo-mock".into(),
            api_key_env: env,
        })
    } else {
        crate::seller::UpstreamConfig::OpenAi(crate::seller::OpenAiConfig {
            base_url: format!("http://{addr}"),
            model: "dexdo-mock".into(),
            frame_model: "dexdo-mock".into(),
            api_key_env: env,
            ..Default::default()
        })
    };
    (upstream, task)
}

const RETRY_AFTER: &str = "7";

async fn ask(
    harness: &WeeklyRouteHarness,
    path: &str,
    stream: bool,
) -> (http::StatusCode, http::HeaderMap, String) {
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
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
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
    for _ in 0..2 {
        let (status, headers, body) = ask(&harness, path, stream).await;
        if initial_mode == 0 {
            // The provider's throttling reaches the client as sent; the client retries.
            assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS, "{body}");
            assert_eq!(headers["retry-after"], RETRY_AFTER, "{path}");
        } else {
            assert_eq!(
                status,
                if stream {
                    reqwest::StatusCode::OK
                } else {
                    reqwest::StatusCode::BAD_GATEWAY
                },
                "{body}"
            );
            assert!(body.contains("upstream_retryable"), "{path}: {body}");
            assert!(body.contains("retry_with_backoff"), "{path}: {body}");
        }
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
        if initial_mode == 4 {
            assert!(body.contains("upstream read failed:"), "{path}: {body}");
        }
    }
    mode.store(2, Ordering::SeqCst);
    let (status, _, body) = ask(&harness, path, stream).await;
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
    for (path, native) in [
        ("/v1/chat/completions", false),
        ("/v1/responses", false),
        ("/v1/messages", true),
    ] {
        for stream in [false, true] {
            assert_preserved(path, stream, 0, native).await;
        }
    }
}

#[tokio::test]
async fn issue_2135_native_pre_output_transport_failures_preserve_the_deal() {
    for stream in [false, true] {
        for failure_mode in [3, 4] {
            assert_preserved("/v1/messages", stream, failure_mode, true).await;
        }
    }
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
        let error = if from_gateway { stream::StreamError::Upstream(status) }
            else { stream::StreamError::Local("dexdo-error-code: upstream_retryable".into()) };
        let expected = if received > 0 { StreamErrorPolicyAction::SellerStallsMidStream }
            else if typed && from_gateway { StreamErrorPolicyAction::RequestScoped }
            else { StreamErrorPolicyAction::DeadGateway };
        proptest::prop_assert_eq!(error.policy_action(received), expected);
    }
}
