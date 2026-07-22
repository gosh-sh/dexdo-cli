//! Native Anthropic Messages API seller upstream.

use super::{UpstreamEvent, UpstreamResult};
use crate::seller::models::ModelConfig;
use dexdo_proto::{CanonChunk, CanonRequest, SignalManifest};
use serde::{Deserialize, Serialize};
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
}

impl AnthropicConfig {
    pub fn supports(model: &ModelConfig) -> bool {
        model
            .base_url
            .to_ascii_lowercase()
            .contains("api.anthropic.com")
    }

    pub fn from_model(model: &ModelConfig) -> Self {
        Self {
            base_url: model.base_url.clone(),
            model: model.served_model.clone(),
            frame_model: model.frame_model.clone(),
            api_key_env: model.api_key_env.clone(),
            tokenizer_family: model.tokenizer_family.clone(),
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
        .and_then(|p| (p.max_tokens != 0).then_some(p.max_tokens))
        .unwrap_or_else(|| count.min(u32::MAX as u64) as u32);
    let max_tokens = requested_max.min(count.min(u32::MAX as u64) as u32).max(1);
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
    if let Err(status) = stream_upstream(cfg, &key, count, &req, &tx).await {
        let _ = tx.send(Err(status)).await;
    }
}

async fn stream_upstream(
    cfg: &AnthropicConfig,
    key: &str,
    count: u64,
    req: &CanonRequest,
    tx: &mpsc::Sender<UpstreamResult>,
) -> Result<(), Status> {
    use futures::StreamExt;

    let client = reqwest::Client::new();
    let body = build_request(cfg, req, count);
    let response = http_request(&client, cfg, key, &body)
        .send()
        .await
        .map_err(|e| Status::unavailable(format!("upstream connect failed: {e}")))?;
    if !response.status().is_success() {
        return Err(Status::unavailable(format!(
            "upstream HTTP {}",
            response.status()
        )));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut seq = 0_u64;
    let mut accounted_output = 0_u64;
    let mut saw_stop = false;
    while let Some(part) = stream.next().await {
        let bytes = part.map_err(|e| Status::unavailable(format!("upstream read failed: {e}")))?;
        buffer.extend_from_slice(&bytes);
        for event in drain_complete_events(&mut buffer)? {
            match parse_event(&event)? {
                ParsedEvent::Delta { text, reasoning }
                    if !text.is_empty() || !reasoning.is_empty() =>
                {
                    let chunk = CanonChunk {
                        text,
                        reasoning,
                        token_ids: Vec::new(),
                        seq,
                        logprobs: Vec::new(),
                        manifest: (seq == 0).then(|| SignalManifest {
                            tokenizer_family: cfg.tokenizer_family.clone(),
                            has_token_ids: false,
                            has_logprobs: false,
                            claimed_model: cfg.frame_model.clone(),
                        }),
                    };
                    seq += 1;
                    if tx
                        .send(Ok(UpstreamEvent::Chunk {
                            chunk,
                            accounted_tokens: 0,
                        }))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                ParsedEvent::Usage {
                    input_tokens: _,
                    output_tokens,
                } => {
                    if output_tokens < accounted_output {
                        return Err(Status::data_loss(
                            "Anthropic cumulative output_tokens decreased",
                        ));
                    }
                    let capped = output_tokens.min(count);
                    let delta = capped.saturating_sub(accounted_output);
                    if delta != 0 {
                        if tx.send(Ok(UpstreamEvent::Accounted(delta))).await.is_err() {
                            return Ok(());
                        }
                        accounted_output = capped;
                    }
                }
                ParsedEvent::Delta { .. } | ParsedEvent::Other => {}
                ParsedEvent::Stop => {
                    saw_stop = true;
                    break;
                }
                ParsedEvent::Error(message) => {
                    return Err(Status::unavailable(format!(
                        "Anthropic stream error: {message}"
                    )));
                }
            }
        }
        if saw_stop {
            return Ok(());
        }
    }
    if !buffer.is_empty() {
        return Err(Status::data_loss("incomplete Anthropic SSE frame at EOF"));
    }
    Err(Status::data_loss(
        "Anthropic SSE ended without message_stop",
    ))
}

const MAX_SSE_FRAME_BYTES: usize = 1 << 20;

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
    if buffer.len() > MAX_SSE_FRAME_BYTES {
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

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
struct EventUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedEvent {
    Delta {
        text: String,
        reasoning: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
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
        ("message_start", _) => {
            event
                .message
                .and_then(|message| message.usage)
                .map_or(ParsedEvent::Other, |usage| ParsedEvent::Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                })
        }
        ("message_delta", _) => {
            event
                .usage
                .map_or(ParsedEvent::Other, |usage| ParsedEvent::Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                })
        }
        ("message_stop", _) => ParsedEvent::Stop,
        _ => ParsedEvent::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller::models::Capabilities;
    use dexdo_proto::{ChatMessage, SamplingParams};

    fn config() -> AnthropicConfig {
        AnthropicConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            frame_model: "anthropic--claude--sonnet-4".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            tokenizer_family: "claude".to_string(),
        }
    }

    async fn run_test_stream(body: Vec<u8>) -> (Result<(), Status>, Vec<UpstreamEvent>) {
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
        let result = stream_upstream(&cfg, "secret", 8, &request, &tx).await;
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
        let body = build_request(&cfg, &request, 32);
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
            ParsedEvent::Usage { input_tokens: 12, output_tokens: 1 }
        );
        assert_eq!(
            parse_event("event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}").unwrap(),
            ParsedEvent::Usage { input_tokens: 0, output_tokens: 7 }
        );
        assert_eq!(
            parse_event("event: message_stop\ndata: {\"type\":\"message_stop\"}").unwrap(),
            ParsedEvent::Stop
        );
    }

    #[tokio::test]
    async fn streams_two_deltas_but_accounts_five_reported_model_tokens() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
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
        stream_upstream(&cfg, "secret", 8, &request, &tx)
            .await
            .unwrap();
        drop(tx);
        server.await.unwrap();

        let mut chunks = Vec::new();
        let mut accounted = 0;
        while let Some(event) = rx.recv().await {
            match event.unwrap() {
                UpstreamEvent::Chunk {
                    chunk,
                    accounted_tokens,
                } => {
                    assert_eq!(accounted_tokens, 0);
                    chunks.push(chunk);
                }
                UpstreamEvent::Accounted(tokens) => accounted += tokens,
            }
        }
        assert_eq!(chunks.len(), 2, "SSE delta count remains a framing detail");
        assert_eq!(accounted, 5, "billing follows cumulative model usage");
        assert_eq!(chunks[0].seq, 0);
        assert_eq!(chunks[0].text, "Hello");
        assert!(chunks[0].manifest.is_some());
        assert_eq!(chunks[1].seq, 1);
        assert_eq!(chunks[1].text, " world");
        assert!(chunks[1].manifest.is_none());
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
        let mut oversized = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
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
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n"
        );
        let (result, events) = run_test_stream(sse.as_bytes().to_vec()).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
        assert!(matches!(events.as_slice(), [UpstreamEvent::Chunk { .. }]));
    }

    #[tokio::test]
    async fn malformed_terminal_frame_fails_at_eof() {
        let malformed = b"event: message_stop\ndata: {\"type\":".to_vec();
        let (result, _) = run_test_stream(malformed).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DataLoss);
    }

    #[tokio::test]
    async fn one_delta_with_usage_one_accounts_exactly_one() {
        let sse = concat!(
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
                UpstreamEvent::Accounted(tokens) => Some(*tokens),
                UpstreamEvent::Chunk { .. } => None,
            })
            .sum();
        assert_eq!(accounted, 1);
    }
}
