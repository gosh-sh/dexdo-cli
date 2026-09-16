//! Native Anthropic Messages API seller upstream.

use super::{
    annotate_seller_config_fault, resolve_model_output_cap, UpstreamEvent, UpstreamResult,
};
use crate::seller::models::ModelConfig;
use dexdo_core::params::UPSTREAM_SSE_FRAME_MAX_BYTES;
use dexdo_proto::{BillingUsage, CanonChunk, CanonRequest, SignalManifest};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::mpsc;
use tonic::Status;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub model: String,
    pub frame_model: String,
    pub api_key_env: String,
    pub tokenizer_family: String,
    /// The model's own maximum output length. `None` = unknown ->
    /// serving fails closed before the provider is contacted. The Messages API always requires `max_tokens`
    /// and rejects anything above the model's limit, so a deal-budget-only bound never delivered.
    pub max_output_tokens: Option<u32>,
}

impl AnthropicConfig {
    pub fn supports(model: &ModelConfig) -> bool {
        model
            .base_url
            .to_ascii_lowercase()
            .contains("api.anthropic.com")
    }

    pub fn from_model(model: &ModelConfig, registry_frame_model: Option<&str>) -> Self {
        Self {
            base_url: model.base_url.clone(),
            model: model.served_model.clone(),
            frame_model: registry_frame_model
                .unwrap_or(&model.frame_model)
                .to_string(),
            api_key_env: model.api_key_env.clone(),
            tokenizer_family: model.tokenizer_family.clone(),
            max_output_tokens: model.capabilities.max_output_tokens,
        }
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    stream: bool,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

#[derive(Serialize)]
struct WireMessage {
    role: String,
    content: String,
}

fn build_request<'a>(
    cfg: &'a AnthropicConfig,
    req: &CanonRequest,
    count: u64,
    model_output_cap: u32,
) -> MessagesRequest<'a> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &req.messages {
        if message.role == "system" {
            system.push(message.content.clone());
        } else {
            messages.push(WireMessage {
                role: message.role.clone(),
                content: message.content.clone(),
            });
        }
    }
    let requested_max = req
        .params
        .as_ref()
        .and_then(|p| (p.max_tokens != 0).then_some(p.max_tokens));
    // bounded by ALL THREE of the buyer's request, the deal budget and the model's own output cap.
    let deal_max_tokens = u32::try_from(count).unwrap_or(u32::MAX);
    let max_tokens = requested_max
        .unwrap_or(u32::MAX)
        .min(deal_max_tokens)
        .min(model_output_cap)
        .max(1);
    let (temperature, stop_sequences) = req.params.as_ref().map_or((None, Vec::new()), |p| {
        (
            if p.greedy {
                Some(0.0)
            } else {
                (p.temperature != 0.0).then_some(p.temperature)
            },
            p.stop.clone(),
        )
    });
    MessagesRequest {
        model: &cfg.model,
        messages,
        stream: true,
        max_tokens,
        system: (!system.is_empty()).then(|| system.join("\n")),
        temperature,
        stop_sequences,
    }
}

fn endpoint(cfg: &AnthropicConfig) -> String {
    format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'))
}

fn http_request(
    client: &reqwest::Client,
    cfg: &AnthropicConfig,
    key: &str,
    body: &MessagesRequest<'_>,
) -> reqwest::RequestBuilder {
    client
        .post(endpoint(cfg))
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(body)
}

pub async fn run(
    cfg: &AnthropicConfig,
    count: u64,
    req: Option<CanonRequest>,
    tx: mpsc::Sender<UpstreamResult>,
) {
    // an unknown model output cap fails closed FIRST, before any provider connection.
    let model_output_cap =
        match resolve_model_output_cap(cfg.max_output_tokens, &cfg.frame_model, &cfg.model) {
            Ok(cap) => cap,
            Err(status) => {
                let _ = tx.send(Err(status)).await;
                return;
            }
        };
    let Some(key) = std::env::var(&cfg.api_key_env)
        .ok()
        .filter(|key| !key.is_empty())
    else {
        let _ = tx
            .send(Err(Status::failed_precondition(format!(
                "real upstream key absent ({})",
                cfg.api_key_env
            ))))
            .await;
        return;
    };
    let Some(req) = req else {
        let _ = tx
            .send(Err(Status::invalid_argument(
                "real upstream requires a canonical request",
            )))
            .await;
        return;
    };
    if let Err(status) = stream_upstream(cfg, &key, count, &req, &tx, model_output_cap).await {
        let _ = tx.send(Err(*status)).await;
    }
}

async fn stream_upstream(
    cfg: &AnthropicConfig,
    key: &str,
    count: u64,
    req: &CanonRequest,
    tx: &mpsc::Sender<UpstreamResult>,
    model_output_cap: u32,
) -> Result<(), Box<Status>> {
    use futures::StreamExt;

    let client = reqwest::Client::new();
    let body = build_request(cfg, req, count, model_output_cap);
    let response = http_request(&client, cfg, key, &body)
        .send()
        .await
        .map_err(|e| Status::unavailable(format!("upstream connect failed: {e}")))?;
    if !response.status().is_success() {
        // a `4xx` rejects a request the seller built end to end -- name the model and the sent limit.
        return Err(Box::new(annotate_seller_config_fault(
            Status::unavailable(format!("upstream HTTP {}", response.status())),
            response.status().as_u16(),
            &cfg.model,
            body.max_tokens,
            model_output_cap,
            None,
        )));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut seq = 0_u64;
    // Usage is cumulative and untrusted until the stream terminates consistently. Hold it locally so a
    // later decrease/malformed terminal event cannot leave a partially advanced monetary high-water.
    let mut reported_output: Option<u64> = None;
    let mut post_output_reported: Option<u64> = None;
    let mut reported_input: Option<u64> = None;
    let mut message_started = false;
    while let Some(part) = stream.next().await {
        let bytes = part.map_err(|e| Status::unavailable(format!("upstream read failed: {e}")))?;
        buffer.extend_from_slice(&bytes);
        for event in drain_complete_events(&mut buffer)? {
            match parse_event(&event)? {
                ParsedEvent::Delta { text, reasoning } => {
                    if !message_started {
                        return Err(Box::new(Status::data_loss(
                            "Anthropic content arrived before message_start usage",
                        )));
                    }
                    if text.is_empty() && reasoning.is_empty() {
                        continue;
                    }
                    post_output_reported = None;
                    let chunk = CanonChunk {
                        text,
                        reasoning,
                        token_ids: Vec::new(),
                        seq,
                        manifest: (seq == 0).then(|| SignalManifest {
                            tokenizer_family: cfg.tokenizer_family.clone(),
                            has_token_ids: false,
                            claimed_model: cfg.frame_model.clone(),
                        }),
                        usage: None,
                    };
                    seq += 1;
                    if tx.send(Ok(UpstreamEvent::Chunk(chunk))).await.is_err() {
                        return Ok(());
                    }
                }
                ParsedEvent::InitialUsage {
                    input_tokens,
                    output_tokens,
                } => {
                    if message_started {
                        return Err(Box::new(Status::data_loss(
                            "Anthropic message_start usage repeated within one stream",
                        )));
                    }
                    let input_tokens = input_tokens.ok_or_else(|| {
                        Status::data_loss("Anthropic initial usage is missing input_tokens")
                    })?;
                    message_started = true;
                    reported_input = Some(input_tokens);
                    let output_tokens = output_tokens.unwrap_or(0);
                    if reported_output.is_some_and(|previous| output_tokens < previous) {
                        return Err(Box::new(Status::data_loss(
                            "Anthropic cumulative output_tokens decreased",
                        )));
                    }
                    reported_output = Some(output_tokens);
                }
                ParsedEvent::CumulativeUsage {
                    input_tokens,
                    output_tokens,
                } => {
                    if !message_started {
                        return Err(Box::new(Status::data_loss(
                            "Anthropic message_delta usage arrived before message_start usage",
                        )));
                    }
                    if let Some(input_tokens) = input_tokens {
                        if reported_input != Some(input_tokens) {
                            return Err(Box::new(Status::data_loss(
                                "Anthropic cumulative usage input_tokens disagrees with initial usage",
                            )));
                        }
                    }
                    let output_tokens = output_tokens.ok_or_else(|| {
                        Status::data_loss("Anthropic cumulative usage is missing output_tokens")
                    })?;
                    if reported_output.is_some_and(|previous| output_tokens < previous) {
                        return Err(Box::new(Status::data_loss(
                            "Anthropic cumulative output_tokens decreased",
                        )));
                    }
                    reported_output = Some(output_tokens);
                    post_output_reported = Some(output_tokens);
                }
                ParsedEvent::Other => {}
                ParsedEvent::Stop => {
                    let input_tokens = reported_input.ok_or_else(|| {
                        Status::data_loss("Anthropic output ended without initial input_tokens")
                    })?;
                    if seq == 0 {
                        let output_tokens = post_output_reported.ok_or_else(|| {
                            Status::data_loss(
                                "Anthropic output ended without final cumulative output_tokens",
                            )
                        })?;
                        if output_tokens != 0 {
                            return Err(Box::new(Status::data_loss(
                                "Anthropic usage reports output tokens without delivered output",
                            )));
                        }
                        return Ok(());
                    }
                    let output_tokens = post_output_reported
                        .filter(|tokens| *tokens > 0)
                        .ok_or_else(|| {
                            Status::data_loss(
                                "Anthropic output ended without positive post-output cumulative output_tokens",
                            )
                        })?;
                    let usage = BillingUsage::new(input_tokens, output_tokens, None)
                        .map_err(Status::data_loss)?;
                    if tx.send(Ok(UpstreamEvent::Usage(usage))).await.is_err() {
                        return Ok(());
                    }
                    return Ok(());
                }
                ParsedEvent::Error(message) => {
                    return Err(Box::new(Status::unavailable(format!(
                        "Anthropic stream error: {message}"
                    ))));
                }
            }
        }
    }
    if !buffer.is_empty() {
        return Err(Box::new(Status::data_loss(
            "incomplete Anthropic SSE frame at EOF",
        )));
    }
    Err(Box::new(Status::data_loss(
        "Anthropic SSE ended without message_stop",
    )))
}

#[allow(clippy::result_large_err)]
fn drain_complete_events(buffer: &mut Vec<u8>) -> Result<Vec<String>, Status> {
    let mut events = Vec::new();
    while let Some(boundary) = buffer.windows(2).position(|window| window == b"\n\n") {
        let frame = buffer.drain(..boundary).collect::<Vec<_>>();
        buffer.drain(..2);
        let frame = String::from_utf8(frame)
            .map_err(|_| Status::data_loss("Anthropic SSE frame is not valid UTF-8"))?;
        events.push(frame);
    }
    if buffer.len() > UPSTREAM_SSE_FRAME_MAX_BYTES {
        return Err(Status::resource_exhausted(
            "Anthropic SSE frame exceeds buffer cap",
        ));
    }
    Ok(events)
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    delta: EventDelta,
    #[serde(default)]
    message: Option<EventMessage>,
    #[serde(default)]
    usage: Option<EventUsage>,
    #[serde(default)]
    error: Option<EventError>,
}

#[derive(Deserialize)]
struct EventError {
    #[serde(default)]
    message: String,
}

#[derive(Default, Deserialize)]
struct EventDelta {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    thinking: String,
}

#[derive(Deserialize)]
struct EventMessage {
    #[serde(default)]
    usage: Option<EventUsage>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum UsageCount {
    #[default]
    Missing,
    Value(u64),
}

impl UsageCount {
    fn into_option(self) -> Option<u64> {
        match self {
            Self::Missing => None,
            Self::Value(value) => Some(value),
        }
    }
}

fn deserialize_usage_count<'de, D>(deserializer: D) -> Result<UsageCount, D::Error>
where
    D: Deserializer<'de>,
{
    u64::deserialize(deserializer).map(UsageCount::Value)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
struct EventUsage {
    #[serde(default, deserialize_with = "deserialize_usage_count")]
    input_tokens: UsageCount,
    #[serde(default, deserialize_with = "deserialize_usage_count")]
    output_tokens: UsageCount,
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedEvent {
    Delta {
        text: String,
        reasoning: String,
    },
    InitialUsage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    CumulativeUsage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    Stop,
    Error(String),
    Other,
}

#[allow(clippy::result_large_err)]
fn parse_event(event: &str) -> Result<ParsedEvent, Status> {
    let event_name = event
        .lines()
        .find_map(|line| line.strip_prefix("event:").map(str::trim));
    let data = event
        .lines()
        .find_map(|line| line.strip_prefix("data:").map(str::trim));
    let Some(data) = data else {
        return Ok(ParsedEvent::Other);
    };
    let event: Event = serde_json::from_str(data)
        .map_err(|e| Status::data_loss(format!("malformed Anthropic SSE JSON: {e}")))?;
    if event_name == Some("error") || event.kind == "error" {
        return Ok(ParsedEvent::Error(
            event
                .error
                .map(|error| error.message)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown error".into()),
        ));
    }
    Ok(match (event.kind.as_str(), event.delta.kind.as_str()) {
        ("content_block_delta", "text_delta") => ParsedEvent::Delta {
            text: event.delta.text,
            reasoning: String::new(),
        },
        ("content_block_delta", "thinking_delta") => ParsedEvent::Delta {
            text: String::new(),
            reasoning: event.delta.thinking,
        },
        ("message_start", _) => event.message.and_then(|message| message.usage).map_or(
            ParsedEvent::InitialUsage {
                input_tokens: None,
                output_tokens: None,
            },
            |usage| ParsedEvent::InitialUsage {
                input_tokens: usage.input_tokens.into_option(),
                output_tokens: usage.output_tokens.into_option(),
            },
        ),
        ("message_delta", _) => event.usage.map_or(
            ParsedEvent::CumulativeUsage {
                input_tokens: None,
                output_tokens: None,
            },
            |usage| ParsedEvent::CumulativeUsage {
                input_tokens: usage.input_tokens.into_option(),
                output_tokens: usage.output_tokens.into_option(),
            },
        ),
        ("message_stop", _) => ParsedEvent::Stop,
        _ => ParsedEvent::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller::models::Capabilities;
    use dexdo_proto::{ChatMessage, SamplingParams};

    /// A model output cap high enough that these fixtures are bounded by the request/deal, as before.
    const TEST_OUTPUT_CAP: u32 = 64_000;

    fn config() -> AnthropicConfig {
        AnthropicConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            frame_model: "anthropic--claude--sonnet-4".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            tokenizer_family: "claude".to_string(),
            max_output_tokens: Some(TEST_OUTPUT_CAP),
        }
    }

    async fn run_test_stream(body: Vec<u8>) -> (Result<(), Box<Status>>, Vec<UpstreamEvent>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        let cfg = AnthropicConfig {
            base_url: format!("http://{address}"),
            ..config()
        };
        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            params: None,
        };
        let (tx, mut rx) = mpsc::channel(16);
        let result = stream_upstream(&cfg, "secret", 8, &request, &tx, TEST_OUTPUT_CAP).await;
        drop(tx);
        server.await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        (result, events)
    }

    #[test]
    fn builds_native_messages_request_with_endpoint_and_headers() {
        let cfg = config();
        let request = CanonRequest {
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: "Be concise".into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: "Hello".into(),
                },
            ],
            params: Some(SamplingParams {
                temperature: 0.4,
                max_tokens: 64,
                stop: vec!["STOP".into()],
                greedy: false,
            }),
        };
        let body = build_request(&cfg, &request, 32, TEST_OUTPUT_CAP);
        let built = http_request(&reqwest::Client::new(), &cfg, "secret", &body)
            .build()
            .unwrap();
        assert_eq!(
            built.url().as_str(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(built.headers()["x-api-key"], "secret");
        assert_eq!(built.headers()["anthropic-version"], ANTHROPIC_VERSION);
        let json: serde_json::Value =
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-20250514");
        assert_eq!(json["system"], "Be concise");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "Hello");
        assert_eq!(json["max_tokens"], 32);
        assert_eq!(json["stream"], true);
        assert_eq!(json["stop_sequences"][0], "STOP");
    }

    #[test]
    fn build_request_clamps_generation_to_request_deal_count_and_model_output_cap() {
        // the Messages API always requires `max_tokens` and rejects anything above the model's own
        // limit, so the same three-way clamp applies here.
        let cfg = config();
        let with_request_limit = |max_tokens| CanonRequest {
            messages: vec![],
            params: Some(SamplingParams {
                max_tokens,
                ..Default::default()
            }),
        };
        let no_request_limit = CanonRequest {
            messages: vec![],
            params: None,
        };

        assert_eq!(
            build_request(&cfg, &with_request_limit(12), 5, TEST_OUTPUT_CAP).max_tokens,
            5
        );
        assert_eq!(
            build_request(&cfg, &with_request_limit(3), 5, TEST_OUTPUT_CAP).max_tokens,
            3
        );
        assert_eq!(
            build_request(&cfg, &no_request_limit, 7, TEST_OUTPUT_CAP).max_tokens,
            7
        );
        // No client value and a deal budget of whole ticks -> the model cap, never `u32::MAX`.
        assert_eq!(
            build_request(&cfg, &no_request_limit, u64::MAX, TEST_OUTPUT_CAP).max_tokens,
            TEST_OUTPUT_CAP
        );
        assert_eq!(
            build_request(
                &cfg,
                &with_request_limit(100_000),
                2_000_000,
                TEST_OUTPUT_CAP
            )
            .max_tokens,
            TEST_OUTPUT_CAP
        );
        assert_eq!(
            build_request(&cfg, &no_request_limit, 0, TEST_OUTPUT_CAP).max_tokens,
            1
        );
    }

    #[tokio::test]
    async fn unknown_model_output_cap_refuses_before_contacting_the_provider() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cfg = AnthropicConfig {
            base_url: format!("http://{address}"),
            // `PATH` is always set, so the key precondition cannot mask the capability refusal.
            api_key_env: "PATH".to_string(),
            max_output_tokens: None,
            ..config()
        };
        let (tx, mut rx) = mpsc::channel(4);
        // Bounded: a regression that only refuses AFTER connecting would otherwise block on a listener that
        // never answers, and a hung test is not a failing test.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run(
                &cfg,
                8,
                Some(CanonRequest {
                    messages: vec![ChatMessage {
                        role: "user".into(),
                        content: "hello".into(),
                    }],
                    params: None,
                }),
                tx,
            ),
        )
        .await
        .expect("an unknown output cap must be refused without waiting on the provider");

        let Some(Err(status)) = rx.recv().await else {
            panic!("an unknown output cap must be reported as a refusal");
        };
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(
            status.message().contains("max_output_tokens"),
            "{}",
            status.message()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept())
                .await
                .is_err(),
            "an unknown output cap must not reach the provider at all"
        );
    }

    #[test]
    fn from_model_carries_the_declared_model_output_cap() {
        let mut model = ModelConfig {
            frame_model: "anthropic--claude-sonnet--4".into(),
            base_url: DEFAULT_BASE_URL.into(),
            served_model: "claude-sonnet-4-5-20250929".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            tokenizer_family: "claude".into(),
            price_per_tick: 1,
            capabilities: Capabilities::default(),
            identity_aliases: vec![],
            vocab_size: None,
            fingerprints: vec![],
        };
        assert_eq!(
            AnthropicConfig::from_model(&model, None).max_output_tokens,
            None,
            "an undeclared cap stays unknown (fail closed), never a fabricated default"
        );
        model.capabilities.max_output_tokens = Some(64_000);
        assert_eq!(
            AnthropicConfig::from_model(&model, None).max_output_tokens,
            Some(64_000)
        );
    }

    #[test]
    fn selects_anthropic_models_and_parses_stream_deltas() {
        let model = ModelConfig {
            frame_model: "anthropic--claude--sonnet-4".into(),
            base_url: DEFAULT_BASE_URL.into(),
            served_model: "claude-sonnet-4-20250514".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            tokenizer_family: "claude".into(),
            price_per_tick: 1,
            capabilities: Capabilities::default(),
            identity_aliases: vec![],
            vocab_size: None,
            fingerprints: vec![],
        };
        assert!(AnthropicConfig::supports(&model));
        assert_eq!(
            parse_event("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}").unwrap(),
            ParsedEvent::Delta { text: "hi".into(), reasoning: String::new() }
        );
        assert_eq!(
            parse_event("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"why\"}}").unwrap(),
            ParsedEvent::Delta { text: String::new(), reasoning: "why".into() }
        );
    }

    #[test]
    fn handles_message_usage_and_stop_events() {
        assert_eq!(
            parse_event("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}").unwrap(),
            ParsedEvent::InitialUsage { input_tokens: Some(12), output_tokens: Some(1) }
        );
        assert_eq!(
            parse_event("event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}").unwrap(),
            ParsedEvent::CumulativeUsage { input_tokens: None, output_tokens: Some(7) }
        );
        assert_eq!(
            parse_event("event: message_stop\ndata: {\"type\":\"message_stop\"}").unwrap(),
            ParsedEvent::Stop
        );
    }

    #[test]
    fn explicit_null_or_non_count_anthropic_usage_fields_fail_closed() {
        for data in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":null,"output_tokens":0}}}"#,
            r#"{"type":"message_start","message":{"usage":{"input_tokens":-1,"output_tokens":0}}}"#,
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1.5,"output_tokens":0}}}"#,
            r#"{"type":"message_delta","usage":{"input_tokens":null,"output_tokens":1}}"#,
            r#"{"type":"message_delta","usage":{"input_tokens":"1","output_tokens":1}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":null}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":18446744073709551616}}"#,
        ] {
            let event_name = if data.contains("message_start") {
                "message_start"
            } else {
                "message_delta"
            };
            let status =
                parse_event(&format!("event: {event_name}\ndata: {data}")).expect_err(data);
            assert_eq!(status.code(), tonic::Code::DataLoss, "{data}");
        }
    }

    #[test]
    fn optional_repeated_anthropic_input_may_be_absent_but_must_match_when_present() {
        assert_eq!(
            parse_event(
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}"
            )
            .unwrap(),
            ParsedEvent::CumulativeUsage {
                input_tokens: None,
                output_tokens: Some(7)
            }
        );
        assert_eq!(
            parse_event(
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":7}}"
            )
            .unwrap(),
            ParsedEvent::CumulativeUsage {
                input_tokens: Some(12),
                output_tokens: Some(7)
            }
        );
    }

    #[tokio::test]
    async fn streams_two_deltas_but_accounts_five_reported_model_tokens() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let cfg = AnthropicConfig {
            base_url: format!("http://{address}"),
            ..config()
        };
        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            params: None,
        };
        let (tx, mut rx) = mpsc::channel(8);
        stream_upstream(&cfg, "secret", 8, &request, &tx, TEST_OUTPUT_CAP)
            .await
            .unwrap();
        drop(tx);
        server.await.unwrap();

        let mut chunks = Vec::new();
        let mut accounted = 0;
        while let Some(event) = rx.recv().await {
            match event.unwrap() {
                UpstreamEvent::Chunk(chunk) => chunks.push(chunk),
                UpstreamEvent::Usage(usage) => accounted += usage.total_tokens,
            }
        }
        assert_eq!(chunks.len(), 2, "SSE delta count remains a framing detail");
        assert_eq!(
            accounted, 8,
            "billing follows input plus cumulative output usage"
        );
        assert_eq!(chunks[0].seq, 0);
        assert_eq!(chunks[0].text, "Hello");
        assert!(chunks[0].manifest.is_some());
        assert_eq!(chunks[1].seq, 1);
        assert_eq!(chunks[1].text, " world");
        assert!(chunks[1].manifest.is_none());
    }

    #[tokio::test]
    async fn terminal_billing_output_above_the_requested_limit_is_preserved() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning and answer\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        result.expect("reasoning-inclusive billing usage is not the visible-output limit");
        assert!(matches!(
            &events[0],
            UpstreamEvent::Chunk(chunk) if chunk.reasoning == "reasoning and answer"
        ));
        assert!(matches!(
            &events[1],
            UpstreamEvent::Usage(usage)
                if usage.input_tokens == 3
                    && usage.output_tokens == 9
                    && usage.total_tokens == 12
        ));
    }

    #[test]
    fn buffers_split_unicode_bytes_until_the_frame_is_complete() {
        let unicode = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}";
        let frame = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{unicode}\"}}}}\n\n"
        );
        let bytes = frame.as_bytes();
        let split = bytes.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
        let mut buffer = bytes[..split].to_vec();
        assert!(drain_complete_events(&mut buffer).unwrap().is_empty());
        buffer.extend_from_slice(&bytes[split..]);
        let events = drain_complete_events(&mut buffer).unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(parse_event(&events[0]).unwrap(), ParsedEvent::Delta { text, .. } if text == unicode)
        );
    }

    #[test]
    fn rejects_oversized_unterminated_frame_and_malformed_json() {
        let mut oversized = vec![b'x'; UPSTREAM_SSE_FRAME_MAX_BYTES + 1];
        assert_eq!(
            drain_complete_events(&mut oversized).unwrap_err().code(),
            tonic::Code::ResourceExhausted
        );
        assert_eq!(
            parse_event("event: message_stop\ndata: {")
                .unwrap_err()
                .code(),
            tonic::Code::DataLoss
        );
    }

    #[test]
    fn recognizes_http_200_error_event() {
        assert!(matches!(
            parse_event("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}").unwrap(),
            ParsedEvent::Error(message) if message == "busy"
        ));
    }

    #[tokio::test]
    async fn http_200_midstream_error_fails_instead_of_clean_completion() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
        assert!(matches!(events.as_slice(), [UpstreamEvent::Chunk(_)]));
    }

    #[tokio::test]
    async fn content_before_message_start_is_rejected_without_authoritative_usage() {
        let sse = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        let status = result.expect_err("content before message_start must fail closed");
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(status.message().contains("before message_start"));
        assert!(events.is_empty(), "the malformed order authorizes no event");
    }

    #[tokio::test]
    async fn message_delta_before_message_start_is_rejected_without_authoritative_usage() {
        let sse = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        let status = result.expect_err("message_delta before message_start must fail closed");
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(status.message().contains("before message_start"));
        assert!(events.is_empty(), "the malformed order authorizes no event");
    }

    #[tokio::test]
    async fn duplicate_or_late_message_start_is_rejected_without_authoritative_usage() {
        for (sse, expected_chunks) in [
            (
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n"
                ),
                0,
            ),
            (
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                    "event: message_delta\n",
                    "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                ),
                1,
            ),
        ] {
            let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
            let status = result.expect_err("message_start may occur exactly once");
            assert_eq!(status.code(), tonic::Code::DataLoss);
            assert!(status.message().contains("message_start usage repeated"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, UpstreamEvent::Chunk(_)))
                    .count(),
                expected_chunks
            );
            assert!(
                events
                    .iter()
                    .all(|event| matches!(event, UpstreamEvent::Chunk(_))),
                "a duplicate start authorizes no usage"
            );
        }
    }

    /// E2E-ROW: E2E-UPS-18/L0
    #[tokio::test]
    async fn malformed_terminal_frame_fails_at_eof() {
        let malformed = b"event: message_stop\ndata: {\"type\":".to_vec();
        let (result, _) = run_test_stream(malformed).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DataLoss);
    }

    #[tokio::test]
    async fn one_delta_with_usage_one_accounts_exactly_one() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        result.unwrap();
        let accounted: u64 = events
            .iter()
            .filter_map(|event| match event {
                UpstreamEvent::Usage(usage) => Some(usage.total_tokens),
                UpstreamEvent::Chunk(_) => None,
            })
            .sum();
        assert_eq!(accounted, 4);
    }

    #[tokio::test]
    async fn anthropic_input_is_billed_without_becoming_part_of_the_output_cap() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        result.expect("input tokens do not consume the output-only cap");
        assert!(events.iter().any(|event| matches!(
            event,
            UpstreamEvent::Usage(usage)
                if usage.input_tokens == 100
                    && usage.output_tokens == 1
                    && usage.total_tokens == 101
        )));
    }

    #[tokio::test]
    async fn repeated_anthropic_input_must_equal_the_initial_count() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        let status = result.expect_err("conflicting repeated input must fail");
        assert!(status.message().contains("disagrees with initial usage"));
        assert!(events
            .iter()
            .all(|event| matches!(event, UpstreamEvent::Chunk(_))));
    }

    #[tokio::test]
    async fn missing_final_usage_fails_without_accounting_frames() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"uncounted\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DataLoss);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, UpstreamEvent::Chunk(_))),
            "no guessed usage may be emitted"
        );
    }

    #[tokio::test]
    async fn message_stop_without_input_usage_fails_closed() {
        let sse = concat!(
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        let status = result.expect_err("a terminator is not an input-usage record");
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(status.message().contains("without initial input_tokens"));
        assert!(
            events.is_empty(),
            "missing input usage authorizes no accounting"
        );
    }

    #[tokio::test]
    async fn positive_initial_usage_without_final_usage_fails_without_accounting_frames() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"uncounted\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DataLoss);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, UpstreamEvent::Chunk(_))),
            "initial usage is not final usage and must not advance monetary accounting"
        );
    }

    /// E2E-ROW: E2E-UPS-21/L0
    #[tokio::test]
    async fn decreasing_cumulative_usage_fails_without_partial_accounting() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"output\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DataLoss);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, UpstreamEvent::Chunk(_))),
            "a contradictory cumulative sequence must not advance monetary usage"
        );
    }

    /// E2E-ROW: E2E-UPS-22/L0
    #[tokio::test]
    async fn overflowing_usage_value_fails_without_accounting() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"output\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":18446744073709551616}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DataLoss);
        assert!(events
            .iter()
            .all(|event| matches!(event, UpstreamEvent::Chunk(_))));
    }

    /// UPS-B5, the native-Anthropic half of the row: the first text delta is EMPTY and no
    /// later one carries text at all, because the whole answer is streamed on the thinking channel.

    /// The OpenAI-compatible side of this shape is what was: a thinking model delivered its
    /// answer on a channel the accounting treated as absent, readiness read "billed without
    /// delivery", and the family became unsellable. This adapter must give the opposite answer --
    /// thinking IS delivered output -- and the empty delta must not be forwarded or consume `seq 0`,
    /// because that is the slot the `SignalManifest` rides in.
    #[tokio::test]
    async fn an_empty_first_text_delta_delivers_nothing_and_thinking_is_billed() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"the whole\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" answer\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        result
            .expect("a thinking-only Anthropic stream is delivered output, not an empty response");

        let mut chunks = Vec::new();
        let mut accounted = Vec::new();
        for event in events {
            match event {
                UpstreamEvent::Chunk(chunk) => chunks.push(chunk),
                UpstreamEvent::Usage(usage) => accounted.push(usage.total_tokens),
            }
        }
        assert_eq!(
            chunks.iter().map(|chunk| chunk.seq).collect::<Vec<_>>(),
            vec![0, 1],
            "the empty text delta forwards nothing and consumes no sequence number"
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.reasoning.as_str())
                .collect::<Vec<_>>(),
            vec!["the whole", " answer"]
        );
        assert!(
            chunks.iter().all(|chunk| chunk.text.is_empty()),
            "nothing may be invented into the content channel"
        );
        assert_eq!(
            chunks[0]
                .manifest
                .as_ref()
                .expect("the first delivered chunk declares the manifest")
                .claimed_model,
            config().frame_model,
            "the manifest rides the first DELIVERED chunk"
        );
        assert!(chunks[1].manifest.is_none());
        assert_eq!(
            accounted,
            vec![7],
            "the bill is initial input plus final cumulative output"
        );
    }
}
