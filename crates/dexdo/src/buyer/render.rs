//! Two consumer-interface translation layers:
//! - **consumer request -> `CanonRequest`**(canonical, OpenAI chat-completions shape);
//! - **`CanonChunk` stream -> OpenAI-SSE** and **-> Anthropic-SSE**.
//! Transcode happens **off-chain, on the buyer side**(B20): the wire(gRPC) and the canonical
//! format are not touched. Model verification runs on the canonical stream BEFORE re-rendering --
//! [`crate::buyer::verify::StreamVerifier`]; chunks arrive here already
//! past it(the verdict affects accept/STOP, B10).

use dexdo_proto::{CanonRequest, ChatMessage, SamplingParams};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// OpenAI chat-completions: consumer request.
// ---------------------------------------------------------------------------

/// Body of `POST /v1/chat/completions`(the OpenAI subset needed for B19).
#[derive(Debug, Deserialize)]
pub struct OpenAiChatRequest {
    /// The `model` field is NOT trusted -- the model is forced by the market/frame(B2, B19).
    /// Kept for the "outside the frame -> reject" check.
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Option<StringOrVec>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,
}

/// OpenAI allows `stop` as a string or an array of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl StringOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            StringOrVec::One(s) => vec![s],
            StringOrVec::Many(v) => v,
        }
    }
}

/// Consumer request(OpenAI) -> `CanonRequest`(B19, R1). The `model` field is NOT carried over:
/// the model is forced by the market on the gateway side.
pub fn openai_to_canon(req: OpenAiChatRequest) -> CanonRequest {
    let messages = req
        .messages
        .into_iter()
        .map(|m| ChatMessage {
            role: m.role,
            content: m.content,
        })
        .collect();
    let params = SamplingParams {
        temperature: req.temperature.unwrap_or(0.0),
        max_tokens: req.max_tokens.unwrap_or(0),
        stop: req.stop.map(StringOrVec::into_vec).unwrap_or_default(),
        greedy: false,
    };
    CanonRequest {
        messages,
        params: Some(params),
    }
}

// ---------------------------------------------------------------------------
// OpenAI chat-completions: response re-render(SSE chunk + non-streaming).
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct OpenAiChunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAiChunkChoice>,
}

#[derive(Debug, Serialize)]
pub struct OpenAiChunkChoice {
    pub index: u32,
    pub delta: OpenAiDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct OpenAiDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// `chat.completion.chunk` with the text delta from `CanonChunk`(re-render B19, R6).
pub fn openai_delta_chunk(id: &str, model: &str, created: u64, text: &str, first: bool) -> String {
    let chunk = OpenAiChunk {
        id: id.to_string(),
        object: "chat.completion.chunk",
        created,
        model: model.to_string(),
        choices: vec![OpenAiChunkChoice {
            index: 0,
            delta: OpenAiDelta {
                role: if first {
                    Some("assistant".into())
                } else {
                    None
                },
                content: Some(text.to_string()),
            },
            finish_reason: None,
        }],
    };
    serde_json::to_string(&chunk).expect("serialize openai chunk")
}

/// Terminal `chat.completion.chunk` with the given `finish_reason`(before `[DONE]`). The normal
/// end is `"stop"`; a verification bail(B10, model substitution) is `"content_filter"`, so the
/// consumer can DISTINGUISH a scam-aborted stream from an honest complete response.
pub fn openai_final_chunk(id: &str, model: &str, created: u64, finish_reason: &str) -> String {
    let chunk = OpenAiChunk {
        id: id.to_string(),
        object: "chat.completion.chunk",
        created,
        model: model.to_string(),
        choices: vec![OpenAiChunkChoice {
            index: 0,
            delta: OpenAiDelta::default(),
            finish_reason: Some(finish_reason.to_string()),
        }],
    };
    serde_json::to_string(&chunk).expect("serialize openai final chunk")
}

/// Full `chat.completion` for a non-streaming response(text aggregate, B19). `finish_reason` is
/// `"stop"` on an honest end / `"content_filter"` on a verification bail.
pub fn openai_completion(
    id: &str,
    model: &str,
    created: u64,
    content: &str,
    finish_reason: &str,
) -> String {
    let body = serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish_reason
        }]
    });
    serde_json::to_string(&body).expect("serialize openai completion")
}

// ---------------------------------------------------------------------------
// Anthropic Messages: request + re-render(local transcode, B20).
// ---------------------------------------------------------------------------

/// Body of `POST /v1/messages`(the Anthropic subset needed for B20).
#[derive(Debug, Deserialize)]
pub struct AnthropicRequest {
    #[serde(default)]
    pub model: Option<String>,
    /// Anthropic system prompt -- a separate top-level field.
    #[serde(default)]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    /// Anthropic content -- a string or an array of blocks; we take the string or the text blocks.
    pub content: AnthropicContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Deserialize)]
pub struct AnthropicContentBlock {
    #[serde(default)]
    pub text: String,
}

impl AnthropicContent {
    fn into_text(self) -> String {
        match self {
            AnthropicContent::Text(s) => s,
            AnthropicContent::Blocks(blocks) => blocks
                .into_iter()
                .map(|b| b.text)
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// Anthropic request -> the same `CanonRequest`(OpenAI shape), local transcode(B20, R1).
/// `system` maps to a `system`-role message; `model` is not trusted.
pub fn anthropic_to_canon(req: AnthropicRequest) -> CanonRequest {
    let mut messages = Vec::new();
    if let Some(sys) = req.system {
        messages.push(ChatMessage {
            role: "system".into(),
            content: sys,
        });
    }
    for m in req.messages {
        messages.push(ChatMessage {
            role: m.role,
            content: m.content.into_text(),
        });
    }
    let params = SamplingParams {
        temperature: req.temperature.unwrap_or(0.0),
        max_tokens: req.max_tokens.unwrap_or(0),
        stop: req.stop_sequences.unwrap_or_default(),
        greedy: false,
    };
    CanonRequest {
        messages,
        params: Some(params),
    }
}

/// Anthropic SSE event: `(event name, JSON payload)`. The handler builds the SSE frame
/// (`event:`/`data:`) from this via the HTTP layer -- no manual line breaks.
pub type AnthropicEvent = (&'static str, String);

/// Anthropic `message_start` event.
pub fn anthropic_message_start(id: &str, model: &str) -> AnthropicEvent {
    let data = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": { "input_tokens": 0, "output_tokens": 0 }
        }
    });
    ("message_start", data.to_string())
}

/// Anthropic `content_block_start` event(a single text block, index 0).
pub fn anthropic_content_block_start() -> AnthropicEvent {
    let data = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": { "type": "text", "text": "" }
    });
    ("content_block_start", data.to_string())
}

/// Anthropic `content_block_delta` event with the text delta from `CanonChunk`(B20, R6).
pub fn anthropic_content_block_delta(text: &str) -> AnthropicEvent {
    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "text_delta", "text": text }
    });
    ("content_block_delta", data.to_string())
}

/// Anthropic `content_block_stop` event.
pub fn anthropic_content_block_stop() -> AnthropicEvent {
    let data = serde_json::json!({ "type": "content_block_stop", "index": 0 });
    ("content_block_stop", data.to_string())
}

/// Anthropic `message_delta` event with the given `stop_reason`. The honest end is `"end_turn"`;
/// a verification bail(B10, substitution) is `"refusal"`, so the consumer can tell a
/// scam-aborted response from a normal completion (review; `refusal` is Anthropic's standard
/// stop_reason for a non-normal completion).
pub fn anthropic_message_delta(stop_reason: &str) -> AnthropicEvent {
    let data = serde_json::json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason, "stop_sequence": null },
        "usage": { "output_tokens": 0 }
    });
    ("message_delta", data.to_string())
}

/// Anthropic `message_stop` event.
pub fn anthropic_message_stop() -> AnthropicEvent {
    (
        "message_stop",
        serde_json::json!({ "type": "message_stop" }).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_request_maps_to_canon_without_model() {
        let req = OpenAiChatRequest {
            model: Some("untrusted-model".into()),
            messages: vec![OpenAiMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            temperature: Some(0.5),
            max_tokens: Some(16),
            stop: Some(StringOrVec::One("END".into())),
            stream: true,
        };
        let canon = openai_to_canon(req);
        assert_eq!(canon.messages.len(), 1);
        assert_eq!(canon.messages[0].role, "user");
        let params = canon.params.unwrap();
        assert_eq!(params.max_tokens, 16);
        assert_eq!(params.stop, vec!["END".to_string()]);
    }

    #[test]
    fn anthropic_request_maps_system_and_blocks() {
        let req = AnthropicRequest {
            model: Some("model-x".into()),
            system: Some("be brief".into()),
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Blocks(vec![AnthropicContentBlock {
                    text: "hello".into(),
                }]),
            }],
            temperature: None,
            max_tokens: Some(32),
            stop_sequences: None,
            stream: true,
        };
        let canon = anthropic_to_canon(req);
        assert_eq!(canon.messages[0].role, "system");
        assert_eq!(canon.messages[0].content, "be brief");
        assert_eq!(canon.messages[1].content, "hello");
    }

    #[test]
    fn anthropic_events_are_well_formed() {
        let (name, data) = anthropic_content_block_delta("tok ");
        assert_eq!(name, "content_block_delta");
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["delta"]["text"], "tok ");
    }
}
