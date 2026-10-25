use super::*;
use crate::seller::models::{Capabilities, ReasoningDisableControl};
use crate::seller::upstream::openai::OpenAiConfig;
use crate::seller::UpstreamConfig;
use axum::{extract::OriginalUri, routing::post, Json, Router};
use serde_json::{json, Value};

async fn provider(
    control: ReasoningDisableControl,
    native: bool,
) -> (
    UpstreamConfig,
    Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let handler = move |OriginalUri(uri): OriginalUri, Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            captured.lock().unwrap().push(body.clone());
            let disabled = body["chat_template_kwargs"]["enable_thinking"] == false
                || body["chat_template_kwargs"]["thinking"] == false
                || body["reasoning_effort"] == "none"
                || body["reasoning"]["enabled"] == false
                || body["thinking"]["type"] == "disabled";
            let delta = if disabled {
                json!({"content": "OK"})
            } else {
                json!({"reasoning_content": "thinking", "content": "OK"})
            };
            let finish = "stop";
            let output = if disabled { 2 } else { 3 };
            let frames = if uri.path() == "/v1/messages" {
                let mut frames = vec![
                    json!({"type":"message_start","message":{"model":"provider-model",
                        "usage":{"input_tokens":3,"output_tokens":0}}}),
                ];
                if !disabled {
                    frames.push(json!({"type":"content_block_delta",
                        "delta":{"type":"thinking_delta","thinking":"thinking"}}));
                }
                frames.extend([
                    json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"OK"}}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
                        "usage":{"output_tokens":output}}),
                    json!({"type":"message_stop"}),
                ]);
                frames
            } else {
                vec![
                    json!({"model":"provider-model","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                    json!({"model":"provider-model","choices":[{"index":0,"delta":{},"finish_reason":finish}],
                    "usage":{"prompt_tokens":3,"completion_tokens":output,"total_tokens":3+output}}),
                ]
            };
            let mut sse = frames
                .iter()
                .map(|v| format!("data: {v}\n\n"))
                .collect::<String>();
            if uri.path() != "/v1/messages" {
                sse += "data: [DONE]\n\n";
            }
            ([("content-type", "text/event-stream")], sse)
        }
    };
    let app = Router::new()
        .route("/chat/completions", post(handler.clone()))
        .route("/v1/messages", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Unique fixture-only variable, never an operational provider key.
    let key_env = format!("DEXDO_REASONING_2090_FIXTURE_{}", addr.port());
    std::env::set_var(&key_env, "fixture");
    let cfg = OpenAiConfig {
        base_url: format!("http://{addr}"),
        model: "provider-model".into(),
        frame_model: "dexdo-mock".into(),
        api_key_env: key_env,
        capabilities: Capabilities {
            max_output_tokens: Some(8),
            reasoning_disable: control,
            ..Default::default()
        },
        ..OpenAiConfig::default()
    };
    let upstream = if native {
        UpstreamConfig::Anthropic(crate::seller::AnthropicConfig {
            base_url: cfg.base_url,
            model: cfg.model,
            frame_model: cfg.frame_model,
            api_key_env: cfg.api_key_env,
            tokenizer_family: cfg.tokenizer_family,
            max_output_tokens: cfg.capabilities.max_output_tokens,
            reasoning_disable: control,
        })
    } else {
        UpstreamConfig::OpenAi(cfg)
    };
    (upstream, requests, task)
}

async fn harness(upstream: UpstreamConfig, gate: ContentGate) -> WeeklyRouteHarness {
    weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
        claimed: PROBE_CLAIM,
        subscription: false,
        expires_in: SUB_WEEK_LEN.as_secs(),
        upstream_tokens: UNCONSTRAINED_UPSTREAM,
        content_gate: gate,
        upstream,
        failure_policy: BuyerApiFailurePolicy::default(),
        lifetime: SessionLifetimePolicy::Preserve,
    })
    .await
}

fn identity_gate() -> ContentGate {
    ContentGate::probe(
        "dexdo-mock".into(),
        Arc::new(
            ModelsConfig::from_json(
                r#"{"models":{"fixture":{"frame_model":"dexdo-mock","base_url":"http://invalid",
        "served_model":"provider-model","api_key_env":"unused","tokenizer_family":"qwen",
        "price_per_tick":1,"fingerprints":[{"probe_prompt":"probe","expected_contains":"OK"}]}}}"#,
            )
            .unwrap(),
        ),
    )
}

fn request(path: &str, stream: bool, disabled: bool, max_tokens: u32) -> Value {
    let mut body = if path == "/v1/responses" {
        json!({"model":"dexdo-mock","input":"answer","max_output_tokens":max_tokens,"stream":stream})
    } else {
        json!({"model":"dexdo-mock","messages":[{"role":"user","content":"answer"}],
            "max_tokens":max_tokens,"stream":stream,"stream_options":{"include_usage":true}})
    };
    body["prompt_cache_key"] = json!("reasoning-2090-fixture");
    if disabled {
        if path == "/v1/responses" {
            body["reasoning"] = json!({"effort":"none"});
        } else {
            body["reasoning_effort"] = json!("none");
        }
    }
    body
}

async fn send(
    harness: &WeeklyRouteHarness,
    path: &str,
    body: Value,
) -> (reqwest::StatusCode, String) {
    let response = reqwest::Client::new()
        .post(format!("http://{}{path}", harness.addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    (response.status(), response.text().await.unwrap())
}

#[tokio::test]
async fn supported_disable_reaches_provider_and_preserves_usage_and_terminal_status() {
    for (control, native, expected) in [
        (
            ReasoningDisableControl::ChatTemplate,
            false,
            json!({"chat_template_kwargs":{"enable_thinking":false}}),
        ),
        (
            ReasoningDisableControl::ChatTemplateThinking,
            false,
            json!({"chat_template_kwargs":{"thinking":false}}),
        ),
        (
            ReasoningDisableControl::ReasoningEffort,
            false,
            json!({"reasoning_effort":"none"}),
        ),
        (
            ReasoningDisableControl::OpenRouter,
            false,
            json!({"reasoning":{"enabled":false,"exclude":false},"provider":{"require_parameters":true}}),
        ),
        (
            ReasoningDisableControl::ThinkingDisabled,
            false,
            json!({"thinking":{"type":"disabled"}}),
        ),
        (
            ReasoningDisableControl::ThinkingDisabled,
            true,
            json!({"thinking":{"type":"disabled"}}),
        ),
    ] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for stream in [false, true] {
                for cap in [8, 2] {
                    let (upstream, captured, task) = provider(control, native).await;
                    let h = harness(upstream, ContentGate::skip()).await;
                    let before = h.remaining().await;
                    let (status, body) = send(&h, path, request(path, stream, true, cap)).await;
                    assert_eq!(status, reqwest::StatusCode::OK, "{path}: {body}");
                    assert!(body.contains("OK"), "{body}");
                    let requests = captured.lock().unwrap().clone();
                    assert_eq!(requests.len(), 1);
                    let controls = |body: &Value| -> Value {
                        let fields = [
                            "chat_template_kwargs",
                            "reasoning_effort",
                            "reasoning",
                            "thinking",
                            "provider",
                        ];
                        Value::Object(
                            body.as_object()
                                .unwrap()
                                .iter()
                                .filter(|(key, _)| fields.contains(&key.as_str()))
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect(),
                        )
                    };
                    assert_eq!(
                        controls(&requests[0]),
                        expected,
                        "{control:?}, native={native}"
                    );
                    assert_eq!(requests[0]["model"], "provider-model");
                    assert_eq!(requests[0]["max_tokens"], cap);
                    if !native {
                        assert_eq!(requests[0]["stream_options"]["include_usage"], true);
                        assert_eq!(requests[0]["prompt_cache_key"], "reasoning-2090-fixture");
                    }
                    let delivery = h.last_delivery().await;
                    assert_eq!(delivery.request_input_tokens, 3);
                    assert_eq!(delivery.request_output_tokens, 2);
                    assert_eq!(delivery.request_billable_tokens, 5);
                    assert_eq!(h.remaining().await, before - 5);
                    assert!(
                        !delivery.truncated_by_output_limit,
                        "a clean exact fit is not local truncation"
                    );
                    if path == "/v1/responses" {
                        assert!(body.contains("completed"), "{body}");
                    } else if stream {
                        assert_eq!(terminal_reason(&body).as_deref(), Some("stop"));
                    } else {
                        let value: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(value["choices"][0]["finish_reason"], "stop");
                    }
                    assert!(!h.deals.current().await.unwrap().session.is_closed());
                    let (status, body) = send(&h, path, request(path, stream, false, 8)).await;
                    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
                    assert_eq!(controls(&captured.lock().unwrap()[1]), json!({}));
                    h.shutdown().await;
                    task.abort();
                }
            }
        }
    }
}

#[tokio::test]
async fn unsupported_and_legacy_sellers_reject_only_the_controlled_request() {
    for native in [false, true] {
        for legacy in [false, true] {
            for path in ["/v1/chat/completions", "/v1/responses"] {
                for stream in [false, true] {
                    let control = if !legacy {
                        ReasoningDisableControl::Unsupported
                    } else if native {
                        ReasoningDisableControl::ThinkingDisabled
                    } else {
                        ReasoningDisableControl::ChatTemplate
                    };
                    let (upstream, captured, task) = provider(control, native).await;
                    let h = harness(upstream, ContentGate::skip()).await;
                    if legacy {
                        h.seller.use_legacy_challenge();
                    }
                    let before = h.remaining().await;
                    let (status, body) = send(&h, path, request(path, stream, true, 8)).await;
                    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
                    let value: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(value["error"]["code"], "unsupported_parameter");
                    assert!(
                        captured.lock().unwrap().is_empty(),
                        "no provider call, including probes"
                    );
                    assert_eq!(h.remaining().await, before);
                    assert_eq!(h.delivered().await, 0);
                    assert_eq!(h.chain.foreign_money_calls.load(Ordering::SeqCst), 0);
                    assert!(!h.deals.current().await.unwrap().session.is_closed());
                    let (status, body) = send(&h, path, request(path, stream, false, 8)).await;
                    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
                    assert!(body.contains("OK"), "{body}");
                    let calls = captured.lock().unwrap().clone();
                    assert_eq!(calls.len(), 1);
                    assert!(
                        calls[0].get("chat_template_kwargs").is_none(),
                        "omission preserves the old request"
                    );
                    assert_eq!(
                        h.last_delivery().await.request_billable_tokens,
                        6,
                        "mixed reasoning/text stays billable"
                    );
                    h.shutdown().await;
                    task.abort();
                }
            }
        }
    }
}

#[tokio::test]
async fn incompatible_native_profiles_reject_only_the_controlled_request() {
    for control in [
        ReasoningDisableControl::ChatTemplate,
        ReasoningDisableControl::ChatTemplateThinking,
        ReasoningDisableControl::ReasoningEffort,
        ReasoningDisableControl::OpenRouter,
    ] {
        let (upstream, captured, task) = provider(control, true).await;
        let h = harness(upstream, ContentGate::skip()).await;
        let before = h.remaining().await;
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let (status, body) = send(&h, path, request(path, false, true, 8)).await;
            assert_eq!(
                status,
                reqwest::StatusCode::BAD_REQUEST,
                "{control:?}: {body}"
            );
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
                "unsupported_parameter"
            );
        }
        assert!(captured.lock().unwrap().is_empty());
        assert_eq!(h.remaining().await, before);
        assert_eq!(h.chain.foreign_money_calls.load(Ordering::SeqCst), 0);
        assert!(!h.deals.current().await.unwrap().session.is_closed());
        let (status, body) = send(
            &h,
            "/v1/chat/completions",
            request("/v1/chat/completions", false, false, 8),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(h.last_delivery().await.request_billable_tokens, 6);
        assert!(captured.lock().unwrap()[0].get("thinking").is_none());
        h.shutdown().await;
        task.abort();
    }
}

#[tokio::test]
async fn unsupported_disable_is_checked_before_billable_identity_verification() {
    let (upstream, captured, task) = provider(ReasoningDisableControl::Unsupported, false).await;
    let h = harness(upstream, identity_gate()).await;
    let before = h.remaining().await;
    let (status, body) = send(
        &h,
        "/v1/chat/completions",
        request("/v1/chat/completions", false, true, 8),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
    assert!(captured.lock().unwrap().is_empty());
    assert_eq!(h.remaining().await, before);
    assert!(h
        .deals
        .current()
        .await
        .unwrap()
        .content_gate
        .verdict
        .get()
        .is_none());
    assert_eq!(h.chain.foreign_money_calls.load(Ordering::SeqCst), 0);
    h.shutdown().await;
    task.abort();
}

#[tokio::test]
async fn unsupported_reasoning_spellings_and_values_never_reach_the_provider() {
    let (upstream, captured, task) = provider(ReasoningDisableControl::ChatTemplate, false).await;
    let h = harness(upstream, identity_gate()).await;
    let before = h.remaining().await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for control in [
            json!({"reasoning_effort":"low"}),
            json!({"reasoning_effort":false}),
            json!({"reasoning":{"effort":"low"}}),
            json!({"reasoning":{"enabled":false}}),
            json!({"reasoning":{"effort":"none","summary":"auto"}}),
            json!({"enable_thinking":false}),
            json!({"chat_template_kwargs":{"enable_thinking":false}}),
            json!({"reasoning_format":"hidden"}),
            json!({"think":false}),
            json!({"thinking_token_budget":0}),
            json!({"reasoning_control":"off"}),
            json!({"separate_reasoning":false}),
            json!({"stream_reasoning":false}),
        ] {
            let mut body = request(path, false, false, 8);
            body.as_object_mut()
                .unwrap()
                .extend(control.as_object().unwrap().clone());
            let (status, body) = send(&h, path, body).await;
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{path}: {body}");
            if path == "/v1/chat/completions" {
                assert_eq!(
                    serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
                    "unsupported_parameter"
                );
            }
        }
    }
    assert!(captured.lock().unwrap().is_empty());
    assert_eq!(h.remaining().await, before);
    assert_eq!(h.chain.foreign_money_calls.load(Ordering::SeqCst), 0);
    assert!(!h.deals.current().await.unwrap().session.is_closed());
    assert!(h
        .deals
        .current()
        .await
        .unwrap()
        .content_gate
        .verdict
        .get()
        .is_none());
    let (status, body) = send(
        &h,
        "/v1/chat/completions",
        request("/v1/chat/completions", false, false, 8),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    assert_eq!(
        captured.lock().unwrap().len(),
        2,
        "identity probe and answer"
    );
    h.shutdown().await;
    task.abort();
}

#[tokio::test]
async fn closed_session_preserves_valid_consumer_demand_with_or_without_disable() {
    use dexdo_core::params::CONSUMER_DEMAND_RECENT_SECS;
    let mut missed = Vec::new();
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for disabled in [false, true] {
            let (upstream, captured, task) =
                provider(ReasoningDisableControl::ReasoningEffort, false).await;
            let h = harness(upstream, ContentGate::skip()).await;
            let before = h.remaining().await;
            let deal = h.deals.current().await.unwrap();
            deal.session
                .close_recovery_episode(RecoveryKind::ReclaimOpened, "failed session fixture");
            for invalid_model in [true, false] {
                let mut body = request(path, false, disabled, 8);
                if invalid_model {
                    body["model"] = json!("foreign-model");
                } else if path == "/v1/responses" {
                    body["reasoning"] = json!({"effort":"invalid"});
                } else {
                    body["reasoning_effort"] = json!("invalid");
                }
                let (status, body) = send(&h, path, body).await;
                assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
                assert!(!deal
                    .has_active_or_recent_request(unix_now_secs(), CONSUMER_DEMAND_RECENT_SECS));
            }
            let (status, body) = send(&h, path, request(path, false, disabled, 8)).await;
            assert_eq!(status, reqwest::StatusCode::GONE, "{body}");
            if !deal.has_active_or_recent_request(unix_now_secs(), CONSUMER_DEMAND_RECENT_SECS) {
                missed.push(format!("{path}, disable={disabled}"));
            }
            assert!(captured.lock().unwrap().is_empty());
            assert_eq!(h.remaining().await, before);
            assert_eq!(h.chain.foreign_money_calls(), 0);
            h.shutdown().await;
            task.abort();
        }
    }
    assert!(
        missed.is_empty(),
        "valid closed-session requests lost demand: {missed:?}"
    );
}

#[tokio::test]
async fn capability_transport_failure_preserves_dead_gateway_policy() {
    use dexdo_core::params::{BUYER_UPSTREAM_OPEN_RETRIES, CONSUMER_DEMAND_RECENT_SECS};
    for path in ["/v1/chat/completions", "/v1/responses"] {
        for action in [
            DeadGatewayAction::FailClosed,
            DeadGatewayAction::RetryThenReclaim,
            DeadGatewayAction::NextSeller,
        ] {
            for lifetime in [
                SessionLifetimePolicy::Preserve,
                SessionLifetimePolicy::SettleOnExit,
            ] {
                let mut outcomes = Vec::new();
                for disabled in [false, true] {
                    let (upstream, captured, task) =
                        provider(ReasoningDisableControl::ReasoningEffort, false).await;
                    let mut h = weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
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
                    let before = h.remaining().await;
                    h.seller.server_task.abort();
                    let _ = (&mut h.seller.server_task).await;
                    let (status, body) = send(&h, path, request(path, false, disabled, 8)).await;
                    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
                    if action == DeadGatewayAction::RetryThenReclaim
                        && BUYER_UPSTREAM_OPEN_RETRIES > 0
                    {
                        assert!(body.contains("after retry"), "{body}");
                    }
                    let deal = h.deals.current().await.unwrap();
                    outcomes.push((
                        deal.session.is_closed(),
                        deal.session.is_settled(),
                        deal.session.recovery_episode(),
                        deal.session.terminal_action(),
                        h.chain.foreign_money_calls(),
                    ));
                    assert!(deal.has_active_or_recent_request(
                        unix_now_secs(),
                        CONSUMER_DEMAND_RECENT_SECS
                    ));
                    assert!(captured.lock().unwrap().is_empty());
                    assert_eq!(h.remaining().await, before);
                    if lifetime == SessionLifetimePolicy::Preserve {
                        assert_eq!(h.chain.foreign_money_calls(), 0);
                    }
                    h.shutdown().await;
                    task.abort();
                }
                assert!(outcomes[0].0, "ordinary dead gateway must close locally");
                assert_eq!(outcomes[0], outcomes[1], "{path}, {action:?}, {lifetime:?}");
            }
        }
    }
}

#[tokio::test]
async fn direct_adapter_rejects_undeclared_disable_without_a_provider_call() {
    for (control, native) in [
        (ReasoningDisableControl::Unsupported, false),
        (ReasoningDisableControl::Unsupported, true),
        (ReasoningDisableControl::ReasoningEffort, true),
    ] {
        let (upstream, captured, task) = provider(control, native).await;
        let request = dexdo_proto::CanonRequest {
            messages: vec![dexdo_proto::ChatMessage {
                role: "user".into(),
                content: "answer".into(),
            }],
            params: Some(dexdo_proto::SamplingParams {
                max_tokens: 8,
                disable_reasoning: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        match upstream {
            UpstreamConfig::OpenAi(cfg) => {
                crate::seller::upstream::openai::run(&cfg, None, 8, Some(request), tx).await
            }
            UpstreamConfig::Anthropic(cfg) => {
                crate::seller::upstream::anthropic::run(&cfg, 8, Some(request), tx).await
            }
            _ => unreachable!(),
        }
        let error = match rx.recv().await.unwrap() {
            Err(error) => error,
            Ok(_) => panic!("unsupported control must not produce output or usage"),
        };
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(error.message().starts_with("unsupported_parameter:"));
        assert!(rx.recv().await.is_none());
        assert!(captured.lock().unwrap().is_empty());
        task.abort();
    }
}

// effort levels and disable are separate capabilities on the same seller.
fn with_reasoning_efforts(
    mut upstream: UpstreamConfig,
    control: &str,
    supported: &[&str],
) -> UpstreamConfig {
    let UpstreamConfig::OpenAi(cfg) = &mut upstream else {
        unreachable!()
    };
    cfg.capabilities.reasoning_effort = Some(
        serde_json::from_value(json!({
            "control": control, "supported": supported
        }))
        .unwrap(),
    );
    upstream
}

fn effort_request(path: &str, stream: bool, effort: Value) -> Value {
    let mut body = request(path, stream, false, 8);
    if path == "/v1/responses" {
        body["reasoning"] = json!({"effort": effort});
    } else {
        body["reasoning_effort"] = effort;
    }
    body
}

#[tokio::test]
async fn reasoning_2172_levels_reach_both_apis_and_upstream_shapes_unchanged() {
    for control in ["reasoning_effort", "reasoning_effort_nested"] {
        for effort in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            for path in ["/v1/chat/completions", "/v1/responses"] {
                for stream in [false, true] {
                    let (upstream, captured, task) =
                        provider(ReasoningDisableControl::ReasoningEffort, false).await;
                    let upstream = with_reasoning_efforts(
                        upstream,
                        control,
                        &["minimal", "low", "medium", "high", "xhigh", "max"],
                    );
                    let h = harness(upstream, identity_gate()).await;
                    let (status, body) =
                        send(&h, path, effort_request(path, stream, json!(effort))).await;
                    assert_eq!(
                        status,
                        reqwest::StatusCode::OK,
                        "{control} {effort} {path} {stream}: {body}"
                    );
                    {
                        let requests = captured.lock().unwrap();
                        assert_eq!(requests.len(), 2, "identity probe and generation");
                        assert!(requests[0].get("reasoning_effort").is_none());
                        assert!(requests[0].get("reasoning").is_none());
                        let actual = &requests[1];
                        if control == "reasoning_effort" {
                            assert_eq!(actual["reasoning_effort"], effort);
                            assert!(actual.get("reasoning").is_none());
                        } else {
                            assert_eq!(actual["reasoning"], json!({"effort": effort}));
                            assert!(actual.get("reasoning_effort").is_none());
                        }
                        assert!(actual.get("thinking").is_none());
                        assert!(actual.get("chat_template_kwargs").is_none());
                    }
                    assert_eq!(h.chain.foreign_money_calls(), 0);
                    h.shutdown().await;
                    task.abort();
                }
            }
        }
    }
}

#[tokio::test]
async fn reasoning_2172_both_capabilities_keep_none_and_omission_independent() {
    for control in ["reasoning_effort", "reasoning_effort_nested"] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for disabled in [false, true] {
                let (upstream, captured, task) =
                    provider(ReasoningDisableControl::ChatTemplate, false).await;
                let upstream = with_reasoning_efforts(upstream, control, &["high"]);
                let h = harness(upstream, ContentGate::skip()).await;
                let (status, body) = send(&h, path, request(path, false, disabled, 8)).await;
                assert_eq!(status, reqwest::StatusCode::OK, "{body}");
                {
                    let requests = captured.lock().unwrap();
                    assert_eq!(requests.len(), 1);
                    assert!(requests[0].get("reasoning_effort").is_none());
                    assert!(requests[0].get("reasoning").is_none());
                    if disabled {
                        assert_eq!(
                            requests[0]["chat_template_kwargs"],
                            json!({"enable_thinking":false})
                        );
                    } else {
                        assert!(requests[0].get("chat_template_kwargs").is_none());
                    }
                }
                h.shutdown().await;
                task.abort();
            }
        }
    }
}

#[tokio::test]
async fn reasoning_2172_invalid_and_undeclared_levels_have_no_billable_or_chain_effects() {
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let (upstream, captured, task) =
            provider(ReasoningDisableControl::ChatTemplate, false).await;
        let upstream = with_reasoning_efforts(upstream, "reasoning_effort", &["high"]);
        let h = harness(upstream, identity_gate()).await;
        let before = h.remaining().await;
        for effort in [
            json!("minimal"),
            json!("low"),
            json!("medium"),
            json!("xhigh"),
            json!("max"),
            json!("ultra"),
            json!("invalid"),
            json!("NONE"),
            json!(""),
            json!(false),
            json!(0),
            json!([]),
            json!({}),
        ] {
            let (status, body) = send(&h, path, effort_request(path, false, effort)).await;
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
        }
        assert!(captured.lock().unwrap().is_empty());
        assert_eq!(h.remaining().await, before);
        assert_eq!(h.chain.foreign_money_calls(), 0);
        assert!(!h.deals.current().await.unwrap().session.is_closed());
        assert!(h
            .deals
            .current()
            .await
            .unwrap()
            .content_gate
            .verdict
            .get()
            .is_none());
        h.shutdown().await;
        task.abort();
    }
}

#[tokio::test]
async fn reasoning_2172_direct_adapters_reject_unsupported_and_conflicting_canonical_controls() {
    for native in [false, true] {
        for (effort, disabled, declared) in [
            ("high", false, false),
            ("invalid", false, true),
            ("low", false, true),
            ("minimal", false, true),
            ("xhigh", false, true),
            ("max", false, true),
            ("high", true, true),
        ] {
            let (mut upstream, captured, task) =
                provider(ReasoningDisableControl::ChatTemplate, native).await;
            if !native && declared {
                upstream = with_reasoning_efforts(upstream, "reasoning_effort", &["high"]);
            }
            let request = dexdo_proto::CanonRequest {
                messages: vec![dexdo_proto::ChatMessage {
                    role: "user".into(),
                    content: "answer".into(),
                }],
                params: Some(dexdo_proto::SamplingParams {
                    max_tokens: 8,
                    disable_reasoning: disabled,
                    reasoning_effort: Some(effort.into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let (tx, mut rx) = tokio::sync::mpsc::channel(2);
            match upstream {
                UpstreamConfig::OpenAi(cfg) => {
                    crate::seller::upstream::openai::run(&cfg, None, 8, Some(request), tx).await
                }
                UpstreamConfig::Anthropic(cfg) => {
                    crate::seller::upstream::anthropic::run(&cfg, 8, Some(request), tx).await
                }
                _ => unreachable!(),
            }
            let error = match rx.recv().await.unwrap() {
                Err(error) => error,
                Ok(_) => panic!("invalid control must not produce output or usage"),
            };
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(error.message().starts_with("unsupported_parameter:"));
            assert!(rx.recv().await.is_none());
            assert!(captured.lock().unwrap().is_empty());
            task.abort();
        }
    }
}
