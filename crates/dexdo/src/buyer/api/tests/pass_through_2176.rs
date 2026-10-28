use super::*;
use crate::seller::upstream::anthropic::AnthropicConfig;
use crate::seller::upstream::openai::OpenAiConfig;
use crate::seller::UpstreamConfig;
use axum::{extract::OriginalUri, routing::post, Router};
use serde_json::{json, Value};

/// One request the provider received.
struct Captured {
    path: String,
    headers: http::HeaderMap,
    body: Value,
}

type Captures = Arc<std::sync::Mutex<Vec<Captured>>>;

/// The provider's error answer for a request that sets `x_fixture_status`.
const PROVIDER_ERROR: &str = "provider refused sk-fixture-provider-secret";

fn provider_answer(path: &str, stream: bool) -> (&'static str, String) {
    let frames: Vec<(Option<&str>, Value)> = match (path, stream) {
        ("/chat/completions", true) => vec![
            (
                None,
                json!({"id":"c1","model":"dexdo-mock","choices":[{"index":0,
                "delta":{"role":"assistant","content":"OK"},"finish_reason":null}]}),
            ),
            (
                None,
                json!({"id":"c1","model":"dexdo-mock","choices":[{"index":0,
                "delta":{},"finish_reason":"stop"}]}),
            ),
            (
                None,
                json!({"id":"c1","model":"dexdo-mock","choices":[],
                "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
            ),
        ],
        ("/chat/completions", false) => {
            return (
                "application/json",
                json!({"id":"c1","object":"chat.completion","model":"dexdo-mock",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"OK"},
                    "finish_reason":"stop"}],
                    "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})
                .to_string(),
            )
        }
        ("/responses", true) => vec![
            (
                Some("response.created"),
                json!({"type":"response.created",
                "response":{"id":"r1","model":"dexdo-mock","status":"in_progress"}}),
            ),
            (
                Some("response.output_text.delta"),
                json!({"type":"response.output_text.delta",
                "delta":"OK"}),
            ),
            (
                Some("response.completed"),
                json!({"type":"response.completed",
                "response":{"id":"r1","model":"dexdo-mock","status":"completed",
                "output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],
                "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}),
            ),
        ],
        ("/responses", false) => {
            return (
                "application/json",
                json!({"id":"r1","object":"response","model":"dexdo-mock","status":"completed",
                    "output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],
                    "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}})
                .to_string(),
            )
        }
        (_, true) => vec![
            (
                Some("message_start"),
                json!({"type":"message_start","message":{"id":"m1",
                "model":"dexdo-mock","usage":{"input_tokens":3,"output_tokens":0}}}),
            ),
            (
                Some("content_block_start"),
                json!({"type":"content_block_start","index":0,
                "content_block":{"type":"text","text":""}}),
            ),
            (
                Some("content_block_delta"),
                json!({"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":"OK"}}),
            ),
            (
                Some("content_block_stop"),
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                Some("message_delta"),
                json!({"type":"message_delta",
                "delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ),
            (Some("message_stop"), json!({"type":"message_stop"})),
        ],
        (_, false) => {
            return (
                "application/json",
                json!({"id":"m1","type":"message","role":"assistant","model":"dexdo-mock",
                    "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn",
                    "usage":{"input_tokens":3,"output_tokens":2}})
                .to_string(),
            )
        }
    };
    let mut sse = String::new();
    for (event, data) in frames {
        if let Some(event) = event {
            sse.push_str(&format!("event: {event}\n"));
        }
        sse.push_str(&format!("data: {data}\n\n"));
    }
    if path == "/chat/completions" {
        sse.push_str("data: [DONE]\n\n");
    }
    ("text/event-stream", sse)
}

/// A provider that records every request and answers it in the format of its path, with headers a
/// real provider sends.
async fn provider(native: bool) -> (UpstreamConfig, Captures, tokio::task::JoinHandle<()>) {
    let captures: Captures = Arc::default();
    let captured = captures.clone();
    let handler = move |OriginalUri(uri): OriginalUri,
                        headers: http::HeaderMap,
                        body: axum::body::Bytes| {
        let captured = captured.clone();
        async move {
            let body: Value = serde_json::from_slice(&body).expect("the provider receives JSON");
            let path = uri.path().trim_start_matches("/v1").to_string();
            let stream = body["stream"] == true;
            let status = body["x_fixture_status"].as_u64();
            let answer_model = body["x_fixture_model"].as_str().map(str::to_string);
            let truncate = body["x_fixture_truncate"] == true;
            let echo_key = body["x_fixture_echo_key"] == true;
            captured.lock().unwrap().push(Captured {
                path: path.clone(),
                headers,
                body,
            });
            let mut response = match status {
                Some(status) if truncate => {
                    // The provider's error body breaks off: half of it, then a reset.
                    let error = json!({"error":{"message":PROVIDER_ERROR}}).to_string();
                    let half = error[..error.len() / 2].to_string();
                    let body = axum::body::Body::from_stream(async_stream::stream! {
                        yield Ok::<_, std::io::Error>(axum::body::Bytes::from(half));
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        yield Err(std::io::Error::other("connection reset"));
                    });
                    let mut response = axum::response::IntoResponse::into_response((
                        [(http::header::CONTENT_TYPE, "application/json")],
                        body,
                    ));
                    *response.status_mut() =
                        http::StatusCode::from_u16(u16::try_from(status).unwrap()).unwrap();
                    response
                }
                Some(status) => {
                    let mut error = json!({"error":{"message":PROVIDER_ERROR}});
                    if echo_key {
                        // A provider that names the key it was called with.
                        error["error"]["key"] = json!("seller-fixture-key");
                    }
                    let mut response =
                        axum::response::IntoResponse::into_response(axum::Json(error));
                    *response.status_mut() =
                        http::StatusCode::from_u16(u16::try_from(status).unwrap()).unwrap();
                    if response.status().is_redirection() {
                        response.headers_mut().insert(
                            http::header::LOCATION,
                            http::HeaderValue::from_static("http://elsewhere.invalid/v1"),
                        );
                    }
                    response
                }
                None => {
                    let (content_type, mut body) = provider_answer(&path, stream);
                    if let Some(model) = &answer_model {
                        body = body.replace("dexdo-mock", model);
                    }
                    axum::response::IntoResponse::into_response((
                        [(http::header::CONTENT_TYPE, content_type)],
                        body,
                    ))
                }
            };
            let headers = response.headers_mut();
            for (name, value) in [
                ("x-ratelimit-remaining-requests", "7"),
                ("retry-after", "3"),
                ("request-id", "req_fixture"),
                ("set-cookie", "provider-session=secret"),
            ] {
                headers.insert(name, http::HeaderValue::from_static(value));
            }
            response
        }
    };
    let app = Router::new()
        .route("/chat/completions", post(handler.clone()))
        .route("/responses", post(handler.clone()))
        .route("/v1/messages", post(handler))
        .layer(axum::extract::DefaultBodyLimit::disable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Unique fixture-only variable, never an operational provider key.
    let key_env = format!("DEXDO_PASS_THROUGH_2176_FIXTURE_{}", addr.port());
    std::env::set_var(&key_env, "seller-fixture-key");
    let upstream = if native {
        UpstreamConfig::Anthropic(AnthropicConfig {
            base_url: format!("http://{addr}"),
            model: "dexdo-mock".into(),
            frame_model: "dexdo-mock".into(),
            api_key_env: key_env,
        })
    } else {
        UpstreamConfig::OpenAi(OpenAiConfig {
            base_url: format!("http://{addr}"),
            model: "dexdo-mock".into(),
            frame_model: "dexdo-mock".into(),
            api_key_env: key_env,
            identity_aliases: Vec::new(),
        })
    };
    (upstream, captures, task)
}

async fn harness(upstream: UpstreamConfig) -> WeeklyRouteHarness {
    weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
        claimed: PROBE_CLAIM,
        subscription: false,
        expires_in: SUB_WEEK_LEN.as_secs(),
        upstream_tokens: UNCONSTRAINED_UPSTREAM,
        content_gate: ContentGate::skip(),
        upstream,
        failure_policy: BuyerApiFailurePolicy::default(),
        lifetime: SessionLifetimePolicy::Preserve,
    })
    .await
}

fn client_request(path: &str, stream: bool) -> Value {
    let mut body = if path == "/v1/responses" {
        json!({"model":"dexdo-mock","input":"answer","max_output_tokens":4,"stream":stream,
            "reasoning":{"effort":"high"}})
    } else {
        json!({"model":"dexdo-mock","messages":[{"role":"user","content":"answer"}],
            "max_tokens":4,"stream":stream})
    };
    if path == "/v1/chat/completions" {
        body["reasoning_effort"] = json!("medium");
    }
    body["temperature"] = json!(0.3);
    body["x_vendor_field"] = json!({"nested":[1, "two", {"three": null}]});
    body
}

async fn send(
    harness: &WeeklyRouteHarness,
    path: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("http://{}{path}", harness.addr))
        .json(body);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.expect("the local endpoint answers")
}

const ROUTES: [(&str, bool); 3] = [
    ("/v1/chat/completions", false),
    ("/v1/responses", false),
    ("/v1/messages", true),
];

/// Every field the client sent reaches the provider unchanged, unknown ones included. The only
/// difference is the seller's own: a streamed Chat Completions request asks for its usage record.
#[tokio::test]
async fn unknown_fields_and_reasoning_controls_reach_the_provider_unchanged() {
    for (path, native) in ROUTES {
        for stream in [false, true] {
            let (upstream, captures, task) = provider(native).await;
            let harness = harness(upstream).await;
            let sent = client_request(path, stream);
            let response = send(&harness, path, &[], &sent).await;
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "{path} stream={stream}: {body}"
            );
            assert!(body.contains("OK"), "{path} stream={stream}: {body}");

            let mut expected = sent.clone();
            if path == "/v1/chat/completions" && stream {
                expected["stream_options"] = json!({"include_usage": true});
            }
            {
                let captures = captures.lock().unwrap();
                assert_eq!(
                    captures.len(),
                    1,
                    "{path} stream={stream}: one provider call"
                );
                assert_eq!(captures[0].body, expected, "{path} stream={stream}");
                assert_eq!(
                    captures[0].path,
                    path.trim_start_matches("/v1"),
                    "{path} reaches the same provider path"
                );
            }
            harness.shutdown().await;
            task.abort();
        }
    }
}

/// The client's own `stream_options` survive; the seller asks for one terminal usage record and
/// removes vLLM's per-frame usage, which its meter would refuse on the first output frame.
#[tokio::test]
async fn the_seller_adds_usage_to_a_streamed_chat_request_and_keeps_the_rest() {
    let (upstream, captures, task) = provider(false).await;
    let harness = harness(upstream).await;
    let mut sent = client_request("/v1/chat/completions", true);
    sent["stream_options"] = json!({
        "include_usage": false,
        "continuous_usage_stats": true,
        "include_obfuscation": false
    });
    let response = send(&harness, "/v1/chat/completions", &[], &sent).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let _ = response.text().await;
    assert_eq!(
        captures.lock().unwrap()[0].body["stream_options"],
        json!({"include_usage": true, "include_obfuscation": false})
    );
    harness.shutdown().await;
    task.abort();
}

/// The client's credentials, account binding and cookies never reach the provider, which sees the
/// seller's key instead; every other client header arrives.
#[tokio::test]
async fn client_credentials_never_reach_the_provider_and_other_headers_do() {
    for (path, native) in ROUTES {
        let (upstream, captures, task) = provider(native).await;
        let harness = harness(upstream).await;
        let response = send(
            &harness,
            path,
            &[
                ("authorization", "Bearer client-secret"),
                ("x-api-key", "client-secret"),
                ("api-key", "client-secret"),
                ("openai-organization", "org-client"),
                ("openai-project", "proj-client"),
                ("cookie", "client-session=secret"),
                ("user-agent", "fixture-agent/1.0"),
                ("session_id", "session-fixture"),
                ("x-client-trace", "trace-fixture"),
                ("anthropic-beta", "fixture-beta"),
            ],
            &client_request(path, true),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
        let _ = response.text().await;

        let headers = captures.lock().unwrap()[0].headers.clone();
        let headers = &headers;
        let all = format!("{headers:?}");
        assert!(!all.contains("client-secret"), "{path}: {all}");
        assert!(
            !all.contains("org-client") && !all.contains("proj-client"),
            "{path}: {all}"
        );
        assert!(!all.contains("client-session"), "{path}: {all}");
        if native {
            assert_eq!(headers["x-api-key"], "seller-fixture-key", "{path}");
            assert!(headers.get("authorization").is_none(), "{path}");
        } else {
            assert_eq!(
                headers["authorization"], "Bearer seller-fixture-key",
                "{path}"
            );
        }
        assert_eq!(headers["user-agent"], "fixture-agent/1.0", "{path}");
        assert_eq!(headers["session_id"], "session-fixture", "{path}");
        assert_eq!(headers["x-client-trace"], "trace-fixture", "{path}");
        assert_eq!(headers["anthropic-beta"], "fixture-beta", "{path}");
        harness.shutdown().await;
        task.abort();
    }
}

/// The provider's response headers reach the client under the `llm_provider-` prefix, except its
/// cookies and the headers that describe the provider-to-seller delivery. The retry and rate-limit
/// headers also keep their own names, and the content type is the buyer's.
#[tokio::test]
async fn provider_response_headers_reach_the_client_under_the_provider_prefix() {
    for (path, native) in ROUTES {
        for stream in [false, true] {
            let (upstream, _captures, task) = provider(native).await;
            let harness = harness(upstream).await;
            let response = send(&harness, path, &[], &client_request(path, stream)).await;
            assert_eq!(
                response.status(),
                reqwest::StatusCode::OK,
                "{path} stream={stream}"
            );
            let headers = response.headers().clone();
            let _ = response.text().await;
            for name in ["x-ratelimit-remaining-requests", "retry-after"] {
                let value = if name == "retry-after" { "3" } else { "7" };
                assert_eq!(headers[name], value, "{path} stream={stream} {name}");
                assert_eq!(
                    headers[format!("llm_provider-{name}").as_str()],
                    value,
                    "{path} stream={stream} {name}"
                );
            }
            assert_eq!(
                headers["llm_provider-request-id"], "req_fixture",
                "{path} stream={stream}"
            );
            assert!(
                headers.get("request-id").is_none(),
                "{path} stream={stream}: other provider headers carry only the prefix"
            );
            assert!(
                headers.get("set-cookie").is_none()
                    && headers.get("llm_provider-set-cookie").is_none(),
                "{path} stream={stream}"
            );
            let content_type = if stream {
                "text/event-stream"
            } else {
                "application/json"
            };
            assert_eq!(
                headers["content-type"], content_type,
                "{path} stream={stream}"
            );
            assert!(
                headers.contains_key("x-dexdo-request-id"),
                "{path} stream={stream}: the buyer's own correlation header stays"
            );
            harness.shutdown().await;
            task.abort();
        }
    }
}

/// A provider error reaches the client with its status, body and `retry-after`, unbilled and sent
/// once, for a streamed request too; a refused seller key stays the seller's 502 and never shows
/// the provider's body.
#[tokio::test]
async fn provider_errors_reach_the_client_as_sent_except_a_refused_seller_key() {
    for ((path, native), stream) in ROUTES
        .into_iter()
        .flat_map(|route| [(route, false), (route, true)])
    {
        for (provider_status, client_status) in [
            (429, reqwest::StatusCode::TOO_MANY_REQUESTS),
            (400, reqwest::StatusCode::BAD_REQUEST),
            (503, reqwest::StatusCode::SERVICE_UNAVAILABLE),
            (401, reqwest::StatusCode::BAD_GATEWAY),
            (403, reqwest::StatusCode::BAD_GATEWAY),
        ] {
            let (upstream, captures, task) = provider(native).await;
            let harness = harness(upstream).await;
            let delivered_before = harness.delivered().await;
            let mut sent = client_request(path, stream);
            sent["x_fixture_status"] = json!(provider_status);
            let response = send(&harness, path, &[], &sent).await;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.unwrap();
            assert_eq!(
                status, client_status,
                "{path} stream={stream} {provider_status}: {body}"
            );
            if matches!(provider_status, 401 | 403) {
                assert!(
                    !body.contains("sk-fixture"),
                    "{path} stream={stream} {provider_status}: {body}"
                );
            } else {
                assert!(
                    body.contains(PROVIDER_ERROR),
                    "{path} stream={stream} {provider_status}: {body}"
                );
                assert_eq!(
                    headers["retry-after"], "3",
                    "{path} stream={stream} {provider_status}"
                );
            }
            assert_eq!(
                captures.lock().unwrap().len(),
                1,
                "{path} stream={stream} {provider_status}: the seller does not repeat the request"
            );
            assert_eq!(
                harness.delivered().await,
                delivered_before,
                "{path} stream={stream} {provider_status}: an error answer bills nothing"
            );
            assert_eq!(
                harness.chain.foreign_money_calls(),
                0,
                "{path} stream={stream} {provider_status}"
            );
            harness.shutdown().await;
            task.abort();
        }
    }
}

/// A provider redirect is the seller's 502: the client never sees the provider's `Location`, and
/// the seller does not follow it.
#[tokio::test]
async fn a_provider_redirect_is_the_sellers_bad_gateway() {
    for (path, native) in ROUTES {
        for (provider_status, stream) in [(302, false), (307, true), (308, false)] {
            let (upstream, captures, task) = provider(native).await;
            let harness = harness(upstream).await;
            let delivered_before = harness.delivered().await;
            let mut sent = client_request(path, stream);
            sent["x_fixture_status"] = json!(provider_status);
            let response = send(&harness, path, &[], &sent).await;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::BAD_GATEWAY,
                "{path} {provider_status}: {body}"
            );
            assert!(
                headers.get("location").is_none() && headers.get("llm_provider-location").is_none(),
                "{path} {provider_status}: {headers:?}"
            );
            assert!(
                !body.contains(PROVIDER_ERROR) && !body.contains("elsewhere.invalid"),
                "{path} {provider_status}: {body}"
            );
            assert_eq!(
                captures.lock().unwrap().len(),
                1,
                "{path} {provider_status}: the seller does not follow the redirect"
            );
            assert_eq!(
                harness.delivered().await,
                delivered_before,
                "{path} {provider_status}: a redirect bills nothing"
            );
            harness.shutdown().await;
            task.abort();
        }
    }
}

/// The SSE events of a streamed answer, in order: each event's name or, without one, its type.
fn sse_events(body: &str) -> Vec<String> {
    body.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap_or_default();
            frame
                .lines()
                .find_map(|line| line.strip_prefix("event: "))
                .map(str::to_string)
                .or_else(|| {
                    serde_json::from_str::<Value>(data)
                        .ok()
                        .and_then(|value| value["type"].as_str().map(str::to_string))
                })
                .unwrap_or_else(|| data.to_string())
        })
        .collect()
}

/// A streamed answer refused on the provider's first frame reaches the client as a whole answer of
/// its format: the buyer opens it when the client has received nothing, so an SDK reads the refusal.
#[tokio::test]
async fn a_refusal_on_the_first_frame_opens_the_answer_in_the_clients_format() {
    for (path, native, opening, closing) in [
        ("/v1/messages", true, "message_start", "message_stop"),
        (
            "/v1/responses",
            false,
            "response.created",
            "response.failed",
        ),
        (
            "/v1/chat/completions",
            false,
            "chat.completion.chunk",
            "[DONE]",
        ),
    ] {
        let (upstream, _captures, task) = provider(native).await;
        let harness = harness(upstream).await;
        let mut sent = client_request(path, true);
        sent["x_fixture_model"] = json!("another/model");
        let response = send(&harness, path, &[], &sent).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
        let body = response.text().await.unwrap();
        let events = sse_events(&body);
        assert!(
            events.first().is_some_and(|event| event.contains(opening)),
            "{path}: {events:?}\n{body}"
        );
        assert!(
            events
                .last()
                .is_some_and(|last| closing.split('|').any(|end| end == last)),
            "{path}: {body}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.contains(opening))
                .count(),
            1,
            "{path}: one opening: {body}"
        );
        assert!(
            !body.contains("\"OK\""),
            "{path}: no provider output: {body}"
        );
        harness.shutdown().await;
        task.abort();
    }
}

/// A provider error body that breaks off is not the provider's answer: the client gets the
/// seller's 502 instead of a truncated body under the provider's status, and nothing is billed.
#[tokio::test]
async fn a_cut_off_provider_error_body_is_a_bad_gateway() {
    for (path, native) in ROUTES {
        let (upstream, captures, task) = provider(native).await;
        let harness = harness(upstream).await;
        let delivered_before = harness.delivered().await;
        let mut sent = client_request(path, false);
        sent["x_fixture_status"] = json!(500);
        sent["x_fixture_truncate"] = json!(true);
        let response = send(&harness, path, &[], &sent).await;
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{path}: {body}");
        assert!(!body.contains("provider refused"), "{path}: {body}");
        assert_eq!(captures.lock().unwrap().len(), 1, "{path}");
        assert_eq!(harness.delivered().await, delivered_before, "{path}");
        harness.shutdown().await;
        task.abort();
    }
}

/// A provider error that names the seller's key never leaves the seller: the client gets the
/// seller's 502 without the provider's body, whatever the provider's status.
#[tokio::test]
async fn a_provider_error_naming_the_sellers_key_never_reaches_the_client() {
    for (path, native) in ROUTES {
        for provider_status in [400, 429, 500] {
            let (upstream, captures, task) = provider(native).await;
            let harness = harness(upstream).await;
            let mut sent = client_request(path, false);
            sent["x_fixture_status"] = json!(provider_status);
            sent["x_fixture_echo_key"] = json!(true);
            let response = send(&harness, path, &[], &sent).await;
            let status = response.status();
            let headers = format!("{:?}", response.headers());
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::BAD_GATEWAY,
                "{path} {provider_status}"
            );
            for seen in [&body, &headers] {
                assert!(
                    !seen.contains("seller-fixture-key"),
                    "{path} {provider_status}"
                );
                assert!(!seen.contains(PROVIDER_ERROR), "{path} {provider_status}");
            }
            assert_eq!(
                captures.lock().unwrap().len(),
                1,
                "{path} {provider_status}"
            );
            harness.shutdown().await;
            task.abort();
        }
    }
}

/// A request without the deal's model, or for a path the seller's provider does not serve, is
/// refused before the provider is called and before any money moves.
#[tokio::test]
async fn requests_the_seller_must_not_forward_never_reach_the_provider() {
    type Case = (
        &'static str,
        bool,
        Box<dyn Fn(&mut Value)>,
        reqwest::StatusCode,
        &'static str,
    );
    let cases: [Case; 5] = [
        (
            "/v1/chat/completions",
            false,
            Box::new(|body| body["model"] = json!("another/model")),
            reqwest::StatusCode::BAD_REQUEST,
            "outside the configured frame",
        ),
        (
            "/v1/chat/completions",
            false,
            Box::new(|body| {
                body.as_object_mut().unwrap().remove("model");
            }),
            reqwest::StatusCode::BAD_REQUEST,
            "model",
        ),
        (
            "/v1/messages",
            true,
            Box::new(|body| body["model"] = json!("  ")),
            reqwest::StatusCode::BAD_REQUEST,
            "model",
        ),
        (
            "/v1/chat/completions",
            true,
            Box::new(|_| {}),
            reqwest::StatusCode::BAD_REQUEST,
            "unsupported_api_path",
        ),
        (
            "/v1/messages",
            false,
            Box::new(|_| {}),
            reqwest::StatusCode::BAD_REQUEST,
            "unsupported_api_path",
        ),
    ];
    for (path, native, edit, expected_status, expected_text) in cases {
        let (upstream, captures, task) = provider(native).await;
        let harness = harness(upstream).await;
        let delivered_before = harness.delivered().await;
        let mut sent = client_request(path, true);
        edit(&mut sent);
        let response = send(&harness, path, &[], &sent).await;
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, expected_status, "{path} native={native}: {body}");
        assert!(
            body.contains(expected_text),
            "{path} native={native}: {body}"
        );
        assert!(
            captures.lock().unwrap().is_empty(),
            "{path} native={native}: the provider is never called"
        );
        assert_eq!(
            harness.delivered().await,
            delivered_before,
            "{path} native={native}"
        );
        assert_eq!(
            harness.chain.foreign_money_calls(),
            0,
            "{path} native={native}"
        );
        harness.shutdown().await;
        task.abort();
    }
}

/// A body larger than gRPC's default message limit still crosses buyer and seller unchanged.
#[tokio::test]
async fn a_large_request_body_reaches_the_provider_unchanged() {
    let (upstream, captures, task) = provider(false).await;
    let harness = harness(upstream).await;
    let mut sent = client_request("/v1/chat/completions", false);
    sent["messages"][0]["content"] = json!("x".repeat(6 * 1024 * 1024));
    let response = send(&harness, "/v1/chat/completions", &[], &sent).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        &body[..body.len().min(500)]
    );
    assert_eq!(captures.lock().unwrap()[0].body, sent);
    harness.shutdown().await;
    task.abort();
}

/// A closed session records demand for a valid request and never for one it would refuse anyway.
#[tokio::test]
async fn closed_session_records_demand_only_for_valid_requests() {
    use dexdo_core::params::CONSUMER_DEMAND_RECENT_SECS;
    for (path, native) in ROUTES {
        let (upstream, captures, task) = provider(native).await;
        let harness = harness(upstream).await;
        let before = harness.remaining().await;
        let deal = harness.deals.current().await.unwrap();
        deal.session
            .close_recovery_episode(RecoveryKind::ReclaimOpened, "failed session fixture");
        for invalid in [json!("foreign-model"), Value::Null] {
            let mut body = client_request(path, false);
            if invalid.is_null() {
                body.as_object_mut().unwrap().remove("model");
            } else {
                body["model"] = invalid;
            }
            let response = send(&harness, path, &[], &body).await;
            assert_eq!(
                response.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{path}"
            );
            assert!(
                !deal.has_active_or_recent_request(unix_now_secs(), CONSUMER_DEMAND_RECENT_SECS)
            );
        }
        let response = send(&harness, path, &[], &client_request(path, false)).await;
        assert_eq!(response.status(), reqwest::StatusCode::GONE, "{path}");
        assert!(
            deal.has_active_or_recent_request(unix_now_secs(), CONSUMER_DEMAND_RECENT_SECS),
            "{path}: a valid closed-session request keeps its demand"
        );
        assert!(captures.lock().unwrap().is_empty());
        assert_eq!(harness.remaining().await, before);
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        harness.shutdown().await;
        task.abort();
    }
}

/// A seller that cannot answer the capability query is a dead gateway, under every policy, and
/// the provider is never called.
#[tokio::test]
async fn capability_transport_failure_preserves_dead_gateway_policy() {
    use dexdo_core::params::{BUYER_UPSTREAM_OPEN_RETRIES, CONSUMER_DEMAND_RECENT_SECS};
    for (path, native) in ROUTES {
        for action in [
            DeadGatewayAction::FailClosed,
            DeadGatewayAction::RetryThenReclaim,
            DeadGatewayAction::NextSeller,
        ] {
            for lifetime in [
                SessionLifetimePolicy::Preserve,
                SessionLifetimePolicy::SettleOnExit,
            ] {
                let (upstream, captures, task) = provider(native).await;
                let mut harness = weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
                    claimed: PROBE_CLAIM,
                    subscription: false,
                    expires_in: SUB_WEEK_LEN.as_secs(),
                    upstream_tokens: UNCONSTRAINED_UPSTREAM,
                    content_gate: ContentGate::skip(),
                    upstream,
                    failure_policy: BuyerApiFailurePolicy {
                        dead_gateway: action,
                        ..Default::default()
                    },
                    lifetime,
                })
                .await;
                let before = harness.remaining().await;
                harness.seller.server_task.abort();
                let _ = (&mut harness.seller.server_task).await;
                let response = send(&harness, path, &[], &client_request(path, false)).await;
                let status = response.status();
                let body = response.text().await.unwrap();
                assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
                if action == DeadGatewayAction::RetryThenReclaim && BUYER_UPSTREAM_OPEN_RETRIES > 0
                {
                    assert!(body.contains("after retry"), "{body}");
                }
                let deal = harness.deals.current().await.unwrap();
                assert!(
                    deal.session.is_closed(),
                    "{path}, {action:?}, {lifetime:?}: an ordinary dead gateway closes locally"
                );
                assert!(
                    deal.has_active_or_recent_request(unix_now_secs(), CONSUMER_DEMAND_RECENT_SECS)
                );
                assert!(captures.lock().unwrap().is_empty());
                assert_eq!(harness.remaining().await, before);
                if lifetime == SessionLifetimePolicy::Preserve {
                    assert_eq!(harness.chain.foreign_money_calls(), 0);
                }
                harness.shutdown().await;
                task.abort();
            }
        }
    }
}

/// A seller from before raw forwarding declares no paths: the buyer refuses the client's request
/// before any paid work and keeps the deal open, so updating the seller is all it takes.
#[tokio::test]
async fn an_outdated_seller_refuses_only_the_request_and_keeps_the_deal() {
    let harness = weekly_route_harness(PROBE_CLAIM, false).await;
    harness.seller.use_legacy_challenge();
    let deal = harness.deals.current().await.unwrap();
    let calls = harness.seller.calls();
    let delivered = harness.delivered().await;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let response = send(&harness, path, &[], &client_request(path, false)).await;
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{path}: {body}");
        let error: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(error["error"]["code"], "seller_update_required", "{body}");
        assert_eq!(error["error"]["recovery_action"], "wait_for_seller_update");
        assert!(!deal.session.is_closed(), "{path}");
        assert_eq!(deal.session.terminal_action(), None, "{path}");
    }
    assert_eq!(
        harness.seller.calls(),
        calls,
        "no paid work reaches an outdated seller"
    );
    assert_eq!(harness.delivered().await, delivered);
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    harness.shutdown().await;
}

/// A request without a JSON content type is refused before it can reach the deal: a web page can
/// send a `text/plain` POST to this loopback endpoint without a CORS preflight.
#[tokio::test]
async fn a_request_without_a_json_content_type_never_reaches_the_deal() {
    let (upstream, captures, task) = provider(false).await;
    let harness = harness(upstream).await;
    let delivered = harness.delivered().await;
    let calls = harness.seller.calls();
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let mut request = reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", harness.addr))
            .body(client_request("/v1/chat/completions", false).to_string());
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}"
        );
    }
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", harness.addr))
        .header("content-type", "application/json; charset=utf-8")
        .body(client_request("/v1/chat/completions", false).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let _ = response.text().await;
    assert_eq!(
        captures.lock().unwrap().len(),
        1,
        "only the JSON request was forwarded"
    );
    assert_eq!(
        harness.seller.calls(),
        calls + 2,
        "one challenge and one stream"
    );
    assert!(harness.delivered().await > delivered);
    harness.shutdown().await;
    task.abort();
}

/// A seller that states a status no HTTP response can carry gets a refusal, not a buyer panic.
#[tokio::test]
async fn an_invalid_seller_status_is_refused_without_a_panic() {
    for stream in [false, true] {
        let harness = weekly_route_harness(PROBE_CLAIM, false).await;
        let (status, body) = harness
            .ask_full(
                "/v1/chat/completions",
                4,
                "DEXDO_FIXTURE_INVALID_STATUS",
                stream,
            )
            .await;
        assert_eq!(
            status,
            reqwest::StatusCode::BAD_GATEWAY,
            "stream={stream}: {body}"
        );
        assert!(
            body.contains("not an answer or an error"),
            "stream={stream}: {body}"
        );
        harness.shutdown().await;
    }
}
