//! Two consumer-interface translation layers:
//! - **consumer request -> `CanonRequest`** (canonical, OpenAI chat-completions shape);
//! - **`CanonChunk` stream -> OpenAI-SSE** and **-> Anthropic-SSE**.

//! Transcode happens **off-chain, on the buyer side** (B20): the wire (gRPC) and the canonical
//! format are not touched. Model verification runs on the canonical stream BEFORE re-rendering --
//! [`crate::buyer::verify::StreamVerifier`]; chunks arrive here already
//! past it (the verdict affects accept/STOP, B10).

use dexdo_proto::{BillingUsage, CanonRequest, ChatMessage, SamplingParams};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// OpenAI chat-completions: consumer request.
// ---------------------------------------------------------------------------

/// Body of `POST /v1/chat/completions` (the OpenAI subset needed for B19).
#[derive(Debug, Deserialize)]
pub struct OpenAiChatRequest {
    /// The `model` field is NOT trusted -- the model is forced by the market/frame (B2, B19).
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
    #[serde(default)]
    pub stream_options: Option<OpenAiStreamOptions>,
    #[serde(default)]
    pub tools: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiStreamOptions {
    #[serde(default)]
    pub include_usage: bool,
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

/// Consumer request (OpenAI) -> `CanonRequest` (B19, R1). The `model` field is NOT carried over:
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
// OpenAI Responses: the text subset supported by the buyer's canonical wire.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesRequest {
    model: Option<String>,
    input: Value,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    store: Option<bool>,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    tool_choice: Option<String>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    text: Option<Value>,
}

pub struct ResponsesTextRequest {
    pub model: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub stream: bool,
    pub canon: CanonRequest,
}

/// Reject every feature the text-only canonical request cannot represent before opening a deal.
pub fn responses_text_request(body: Value) -> Result<ResponsesTextRequest, String> {
    let request: ResponsesRequest = serde_json::from_value(body)
        .map_err(|error| format!("invalid or unsupported Responses field: {error}"))?;
    if request.store == Some(true) {
        return Err("unsupported Responses field: store=true".into());
    }
    if !request.tools.is_empty() {
        return Err("unsupported Responses field: tools".into());
    }
    if request
        .tool_choice
        .as_deref()
        .is_some_and(|choice| choice != "auto" && choice != "none")
    {
        return Err("unsupported Responses field: tool_choice".into());
    }
    if request.parallel_tool_calls == Some(true) {
        return Err("unsupported Responses field: parallel_tool_calls".into());
    }
    if let Some(text) = request.text {
        if text != serde_json::json!({"format": {"type": "text"}}) {
            return Err("unsupported Responses field: text".into());
        }
    }
    if request.max_output_tokens == Some(0) {
        return Err("max_output_tokens must be greater than zero".into());
    }

    let mut messages = Vec::new();
    if let Some(instructions) = request.instructions {
        messages.push(ChatMessage {
            role: "system".into(),
            content: instructions,
        });
    }
    match request.input {
        Value::String(content) => messages.push(ChatMessage {
            role: "user".into(),
            content,
        }),
        Value::Array(items) if !items.is_empty() => {
            for (index, item) in items.into_iter().enumerate() {
                messages.push(responses_message(item, index)?);
            }
        }
        _ => {
            return Err(
                "unsupported Responses field: input (expected text or text messages)".into(),
            )
        }
    }
    Ok(ResponsesTextRequest {
        model: request.model,
        max_output_tokens: request.max_output_tokens,
        stream: request.stream,
        canon: CanonRequest {
            messages,
            params: Some(SamplingParams {
                max_tokens: request.max_output_tokens.unwrap_or(0),
                ..Default::default()
            }),
        },
    })
}

fn responses_message(value: Value, index: usize) -> Result<ChatMessage, String> {
    let mut item = value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("unsupported Responses field: input[{index}]"))?;
    if let Some(kind) = item.remove("type") {
        if kind != "message" {
            return Err(format!(
                "unsupported Responses field: input[{index}].type={kind}"
            ));
        }
    }
    let role = item
        .remove("role")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| format!("input[{index}].role must be a string"))?;
    if !matches!(role.as_str(), "system" | "developer" | "user" | "assistant") {
        return Err(format!(
            "unsupported Responses field: input[{index}].role={role}"
        ));
    }
    let content = item
        .remove("content")
        .ok_or_else(|| format!("input[{index}].content is required"))?;
    if let Some(field) = item.keys().next() {
        return Err(format!(
            "unsupported Responses field: input[{index}].{field}"
        ));
    }
    let content = match content {
        Value::String(text) => text,
        Value::Array(parts) if !parts.is_empty() => {
            let mut text = String::new();
            for (part_index, part) in parts.into_iter().enumerate() {
                let mut part = part.as_object().cloned().ok_or_else(|| {
                    format!("unsupported Responses field: input[{index}].content[{part_index}]")
                })?;
                let expected = if role == "assistant" {
                    "output_text"
                } else {
                    "input_text"
                };
                if part.remove("type") != Some(Value::String(expected.into())) {
                    return Err(format!(
                        "unsupported Responses field: input[{index}].content[{part_index}].type"
                    ));
                }
                let fragment = part
                    .remove("text")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .ok_or_else(|| {
                        format!("input[{index}].content[{part_index}].text is required")
                    })?;
                if let Some(field) = part.keys().next() {
                    return Err(format!(
                        "unsupported Responses field: input[{index}].content[{part_index}].{field}"
                    ));
                }
                text.push_str(&fragment);
            }
            text
        }
        _ => {
            return Err(format!(
                "unsupported Responses field: input[{index}].content"
            ))
        }
    };
    Ok(ChatMessage {
        role: if role == "developer" {
            "system".into()
        } else {
            role
        },
        content,
    })
}

pub fn responses_response(
    id: &str,
    model: &str,
    created: u64,
    content: &str,
    finish_reason: &str,
    usage: Option<&BillingUsage>,
) -> Value {
    let (status, incomplete_details, error) = match finish_reason {
        "stop" => ("completed", Value::Null, Value::Null),
        // A clean terminal usage can spend the exact route grant while still completing
        // the text response. The buyer's delivery record retains the capacity verdict.
        "capacity" if usage.is_some() => ("completed", Value::Null, Value::Null),
        "length" => (
            "incomplete",
            serde_json::json!({"reason": "max_output_tokens"}),
            Value::Null,
        ),
        "content_filter" => (
            "incomplete",
            serde_json::json!({"reason": "content_filter"}),
            Value::Null,
        ),
        other => (
            "failed",
            Value::Null,
            serde_json::json!({"code": other, "message": format!("buyer stream ended: {other}")}),
        ),
    };
    let item_status = if status == "completed" {
        "completed"
    } else {
        "incomplete"
    };
    serde_json::json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "model": model,
        "output": [{
            "id": format!("msg_{id}"),
            "type": "message",
            "status": item_status,
            "role": "assistant",
            "content": [{"type": "output_text", "text": content, "annotations": []}]
        }],
        "usage": usage.map(|usage| serde_json::json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
        })),
        "incomplete_details": incomplete_details,
        "error": error,
    })
}

// ---------------------------------------------------------------------------
// OpenAI chat-completions: response re-render (SSE chunk + non-streaming).
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct OpenAiChunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAiChunkChoice>,
    /// OpenAI includes this field on streamed chunks only when the request opted into usage.
    /// The ordinary chunks carry `null`; the additional terminal usage chunk carries the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Option<OpenAiUsage>>,
}

#[derive(Debug, Serialize)]
pub struct OpenAiUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl From<&BillingUsage> for OpenAiUsage {
    fn from(usage: &BillingUsage) -> Self {
        Self {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        }
    }
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

/// `chat.completion.chunk` with the text delta from `CanonChunk` (re-render B19, R6).
pub fn openai_delta_chunk(
    id: &str,
    model: &str,
    created: u64,
    text: &str,
    first: bool,
    include_usage: bool,
) -> String {
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
        usage: include_usage.then_some(None),
    };
    serde_json::to_string(&chunk).expect("serialize openai chunk")
}

/// Terminal `chat.completion.chunk` with the given `finish_reason` (before `[DONE]`). The caller
/// supplies the classified terminal: `stop`, `length`, `capacity`, `error`, or `content_filter`.
pub fn openai_final_chunk(
    id: &str,
    model: &str,
    created: u64,
    finish_reason: &str,
    include_usage: bool,
) -> String {
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
        usage: include_usage.then_some(None),
    };
    serde_json::to_string(&chunk).expect("serialize openai final chunk")
}

/// Additional terminal chunk requested by `stream_options.include_usage`, immediately before
/// `[DONE]`. It has no choices; the counts are the accepted provider-native terminal record.
pub fn openai_usage_chunk(id: &str, model: &str, created: u64, usage: &BillingUsage) -> String {
    let chunk = OpenAiChunk {
        id: id.to_string(),
        object: "chat.completion.chunk",
        created,
        model: model.to_string(),
        choices: Vec::new(),
        usage: Some(Some(usage.into())),
    };
    serde_json::to_string(&chunk).expect("serialize openai usage chunk")
}

/// Full `chat.completion` for a non-streaming response (text aggregate, B19), including the
/// caller-classified `finish_reason`.
pub fn openai_completion(
    id: &str,
    model: &str,
    created: u64,
    content: &str,
    finish_reason: &str,
    usage: Option<&BillingUsage>,
) -> String {
    let mut body = serde_json::json!({
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
    if let Some(usage) = usage {
        body.as_object_mut()
            .expect("OpenAI completion is an object")
            .insert(
                "usage".to_string(),
                serde_json::json!({
                    "prompt_tokens": usage.input_tokens,
                    "completion_tokens": usage.output_tokens,
                    "total_tokens": usage.total_tokens,
                }),
            );
    }
    serde_json::to_string(&body).expect("serialize openai completion")
}

// ---------------------------------------------------------------------------
// Anthropic Messages: request + re-render (local transcode, B20).
// ---------------------------------------------------------------------------

/// Body of `POST /v1/messages` (the Anthropic subset needed for B20).
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

/// Anthropic request -> the same `CanonRequest` (OpenAI shape), local transcode (B20, R1).
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

/// Anthropic `content_block_start` event (a single text block, index 0).
pub fn anthropic_content_block_start() -> AnthropicEvent {
    let data = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": { "type": "text", "text": "" }
    });
    ("content_block_start", data.to_string())
}

/// Anthropic `content_block_delta` event with the text delta from `CanonChunk` (B20, R6).
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
/// a verification bail (B10, substitution) is `"refusal"`, so the consumer can tell a
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
    use proptest::prelude::*;

    #[test]
    fn responses_text_maps_to_the_canonical_request_and_renders_native_usage() {
        let request = responses_text_request(serde_json::json!({
            "model": "dexdo-mock",
            "instructions": "be brief",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "hello"},
                {"type": "input_text", "text": " world"}
            ]}],
            "max_output_tokens": 8,
            "stream": true,
            "store": false,
            "tools": [],
            "text": {"format": {"type": "text"}}
        }))
        .unwrap();
        assert_eq!(request.model.as_deref(), Some("dexdo-mock"));
        assert!(request.stream);
        assert_eq!(request.canon.messages[0].role, "system");
        assert_eq!(request.canon.messages[0].content, "be brief");
        assert_eq!(request.canon.messages[1].content, "hello world");
        assert_eq!(request.canon.params.unwrap().max_tokens, 8);

        let usage = BillingUsage {
            input_tokens: 3,
            output_tokens: 7,
            total_tokens: 10,
        };
        let response =
            responses_response("resp_1", "dexdo-mock", 1, "answer", "stop", Some(&usage));
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["status"], "completed");
        assert_eq!(response["output"][0]["content"][0]["text"], "answer");
        assert_eq!(response["usage"]["input_tokens"], 3);
        assert_eq!(response["usage"]["output_tokens"], 7);
        assert_eq!(response["usage"]["total_tokens"], 10);

        let exact_grant = responses_response(
            "resp_2",
            "dexdo-mock",
            1,
            "answer",
            "capacity",
            Some(&usage),
        );
        assert_eq!(exact_grant["status"], "completed");
        assert_eq!(exact_grant["output"][0]["status"], "completed");
        let refused = responses_response("resp_3", "dexdo-mock", 1, "partial", "capacity", None);
        assert_eq!(refused["status"], "failed");
        assert_eq!(refused["output"][0]["status"], "incomplete");
    }

    #[test]
    fn responses_partial_or_failed_text_never_completes_its_output_message() {
        for (reason, response_status) in [
            ("length", "incomplete"),
            ("content_filter", "incomplete"),
            ("error", "failed"),
        ] {
            let response = responses_response("resp_1", "dexdo-mock", 1, "partial", reason, None);
            assert_eq!(response["status"], response_status, "{reason}");
            assert_eq!(response["output"][0]["status"], "incomplete", "{reason}");
            assert_eq!(response["output"][0]["content"][0]["text"], "partial");
        }
    }

    #[test]
    fn responses_unsupported_features_are_named() {
        for (body, field) in [
            (
                serde_json::json!({"input": "hi", "tools": [{"type": "function", "name": "run"}]}),
                "tools",
            ),
            (
                serde_json::json!({"input": [{"type": "function_call_output", "call_id": "c", "output": "ok"}]}),
                "input[0].type",
            ),
            (
                serde_json::json!({"input": "hi", "previous_response_id": "resp_1"}),
                "previous_response_id",
            ),
            (serde_json::json!({"input": "hi", "store": true}), "store"),
            (
                serde_json::json!({"input": "hi", "reasoning": {"effort": "high"}}),
                "reasoning",
            ),
            (
                serde_json::json!({"input": [{"role": "user", "content": [{"type": "input_image", "image_url": "data:"}]}]}),
                "input[0].content[0].type",
            ),
        ] {
            let error = responses_text_request(body)
                .err()
                .expect("feature must be rejected");
            assert!(error.contains(field), "{error}");
        }
    }

    proptest! {
        #[test]
        fn responses_never_ignores_an_unknown_top_level_field(field in "[a-z]{1,16}") {
            let field = format!("unsupported_{field}");
            let mut body = serde_json::json!({"input": "hi"});
            body[field.clone()] = serde_json::json!(true);
            let error = responses_text_request(body).err().expect("unknown field must be rejected");
            prop_assert!(error.contains(&field), "{error}");
        }
    }

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
            stream_options: None,
            tools: vec![],
        };
        let canon = openai_to_canon(req);
        assert_eq!(canon.messages.len(), 1);
        assert_eq!(canon.messages[0].role, "user");
        let params = canon.params.unwrap();
        assert_eq!(params.max_tokens, 16);
        assert_eq!(params.stop, vec!["END".to_string()]);
    }

    #[test]
    fn openai_usage_is_provider_native_for_aggregated_and_streamed_responses() {
        let usage = BillingUsage {
            input_tokens: 7,
            output_tokens: 57,
            total_tokens: 64,
        };
        let completion: serde_json::Value = serde_json::from_str(&openai_completion(
            "id",
            "model",
            1,
            "4",
            "stop",
            Some(&usage),
        ))
        .unwrap();
        assert_eq!(completion["usage"]["prompt_tokens"], 7);
        assert_eq!(completion["usage"]["completion_tokens"], 57);
        assert_eq!(completion["usage"]["total_tokens"], 64);
        assert_ne!(completion["usage"]["completion_tokens"], 1);

        let streamed: serde_json::Value =
            serde_json::from_str(&openai_usage_chunk("id", "model", 1, &usage)).unwrap();
        assert_eq!(streamed["choices"], serde_json::json!([]));
        assert_eq!(streamed["usage"], completion["usage"]);

        let unavailable: serde_json::Value = serde_json::from_str(&openai_completion(
            "id", "model", 1, "partial", "error", None,
        ))
        .unwrap();
        assert!(
            !unavailable.as_object().unwrap().contains_key("usage"),
            "missing terminal usage must not be fabricated as zero"
        );
    }

    #[test]
    fn openai_stream_usage_opt_in_is_parsed_and_marks_regular_chunks_null() {
        let request: OpenAiChatRequest = serde_json::from_value(serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        }))
        .unwrap();
        assert!(request
            .stream_options
            .as_ref()
            .is_some_and(|options| options.include_usage));

        let with_usage: serde_json::Value =
            serde_json::from_str(&openai_delta_chunk("id", "model", 1, "hi", true, true)).unwrap();
        assert!(with_usage["usage"].is_null());
        assert!(with_usage.as_object().unwrap().contains_key("usage"));

        let without_usage: serde_json::Value =
            serde_json::from_str(&openai_delta_chunk("id", "model", 1, "hi", true, false)).unwrap();
        assert!(!without_usage.as_object().unwrap().contains_key("usage"));
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
