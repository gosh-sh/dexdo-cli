//! Consumer API formats. The buyer accepts a request on the path the client used and the seller
//! forwards it to its provider unchanged except for the model name, the usage opt-in of a streamed
//! Chat Completions request and the headers this module filters. Both sides read the provider
//! response through [`parse_frame`] / [`inspect_body`] without rewriting it.

use dexdo_core::params::UPSTREAM_SSE_FRAME_MAX_BYTES;
use dexdo_proto::Header;
use serde_json::{Map, Value};

pub const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub const RESPONSES_PATH: &str = "/v1/responses";
pub const MESSAGES_PATH: &str = "/v1/messages";

/// One consumer API wire format, named by the path it is served on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiFormat {
    ChatCompletions,
    Responses,
    Messages,
}

impl ApiFormat {
    pub fn from_path(path: &str) -> Option<Self> {
        match path {
            CHAT_COMPLETIONS_PATH => Some(Self::ChatCompletions),
            RESPONSES_PATH => Some(Self::Responses),
            MESSAGES_PATH => Some(Self::Messages),
            _ => None,
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => CHAT_COMPLETIONS_PATH,
            Self::Responses => RESPONSES_PATH,
            Self::Messages => MESSAGES_PATH,
        }
    }

    /// Usage-history protocol label.
    pub fn protocol(self) -> &'static str {
        match self {
            Self::ChatCompletions => "openai",
            Self::Responses => "openai_responses",
            Self::Messages => "anthropic",
        }
    }
}

/// A consumer request body: a JSON object, kept as received apart from the seller's edits.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestBody(pub Map<String, Value>);

impl RequestBody {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        match serde_json::from_slice::<Value>(bytes) {
            Ok(Value::Object(object)) => Ok(Self(object)),
            Ok(_) => Err("request body must be a JSON object".to_string()),
            Err(error) => Err(format!("request body is not valid JSON: {error}")),
        }
    }

    /// The requested model; `None` when the field is absent, empty or not a string.
    pub fn model(&self) -> Option<&str> {
        self.0
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
    }

    pub fn set_model(&mut self, model: &str) {
        self.0
            .insert("model".to_string(), Value::String(model.to_string()));
    }

    pub fn stream(&self) -> bool {
        self.0.get("stream").and_then(Value::as_bool) == Some(true)
    }

    /// Ask a streamed Chat Completions response for its one terminal usage record, keeping any
    /// other `stream_options` the client set. vLLM's `continuous_usage_stats` is removed: it puts
    /// usage on every output frame, which the seller's meter refuses.
    pub fn require_stream_usage(&mut self) {
        let options = self
            .0
            .entry("stream_options")
            .or_insert_with(|| Value::Object(Map::new()));
        if !options.is_object() {
            *options = Value::Object(Map::new());
        }
        let options = options
            .as_object_mut()
            .expect("stream_options is an object");
        options.remove("continuous_usage_stats");
        options.insert("include_usage".to_string(), Value::Bool(true));
    }

    pub fn to_vec(&self) -> Vec<u8> {
        serde_json::to_vec(&self.0).expect("a JSON object serializes")
    }
}

/// The Messages API version stated by the requests dexdo writes itself -- the seller's readiness
/// check and the buyer's identity probes. A consumer request carries its own; none is added to it.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The headers a request dexdo writes itself in `format` carries, as any client of that API would.
pub fn own_request_headers(format: ApiFormat) -> Vec<Header> {
    match format {
        ApiFormat::Messages => vec![Header {
            name: "anthropic-version".into(),
            value: ANTHROPIC_VERSION.as_bytes().to_vec(),
        }],
        ApiFormat::ChatCompletions | ApiFormat::Responses => Vec::new(),
    }
}

/// Request headers that never travel to the seller or its provider: the client's credentials and
/// account binding, cookies, and the transport headers the seller's HTTP client sets itself.
const NON_FORWARDED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "api-key",
    "openai-organization",
    "openai-project",
    "cookie",
    "host",
    "content-length",
    "content-type",
    "accept-encoding",
];

/// Response headers that describe delivery between the provider and the seller, plus provider
/// cookies. The buyer's HTTP server sets its own transport headers.
const NON_FORWARDED_RESPONSE_HEADERS: &[&str] =
    &["content-length", "content-encoding", "set-cookie"];

/// Hop-by-hop headers belong to one connection in either direction.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub fn forwards_request_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !NON_FORWARDED_REQUEST_HEADERS.contains(&name.as_str())
        && !HOP_BY_HOP_HEADERS.contains(&name.as_str())
}

pub fn forwards_response_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !NON_FORWARDED_RESPONSE_HEADERS.contains(&name.as_str())
        && !HOP_BY_HOP_HEADERS.contains(&name.as_str())
}

/// The header names a `Connection` header lists: hop-by-hop for that one connection
/// whatever their name.
fn connection_listed(headers: &[Header]) -> Vec<String> {
    headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("connection"))
        .flat_map(|header| {
            String::from_utf8_lossy(&header.value)
                .split(',')
                .map(|name| name.trim().to_ascii_lowercase())
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn forwarded(headers: Vec<Header>, forwards: fn(&str) -> bool) -> Vec<Header> {
    let listed = connection_listed(&headers);
    headers
        .into_iter()
        .filter(|header| {
            forwards(&header.name) && !listed.contains(&header.name.to_ascii_lowercase())
        })
        .collect()
}

/// The client's request headers that may travel to the seller and its provider.
pub fn forwarded_request_headers(headers: Vec<Header>) -> Vec<Header> {
    forwarded(headers, forwards_request_header)
}

/// The provider's response headers that may travel back to the buyer.
pub fn forwarded_response_headers(headers: Vec<Header>) -> Vec<Header> {
    forwarded(headers, forwards_response_header)
}

/// The prefix every provider response header carries on the buyer's answer, so that no provider
/// header -- `Location`, CORS, cookies -- acts on the client as the buyer's own.
pub const PROVIDER_HEADER_PREFIX: &str = "llm_provider-";

/// Provider headers a client retries by, which also keep their own names: `retry-after`,
/// `retry-after-ms` and the rate-limit headers `x-ratelimit-*` and `anthropic-ratelimit-*`.
fn steers_client_retries(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "retry-after"
        || name == "retry-after-ms"
        || name.starts_with("x-ratelimit-")
        || name.starts_with("anthropic-ratelimit-")
}

/// The provider's response headers as the buyer's client receives them: each under
/// [`PROVIDER_HEADER_PREFIX`], and the retry and rate-limit headers also under their own names.
pub fn client_response_headers(headers: &[Header]) -> Vec<Header> {
    let mut out = Vec::new();
    for header in forwarded_response_headers(headers.to_vec()) {
        if steers_client_retries(&header.name) {
            out.push(header.clone());
        }
        out.push(Header {
            name: format!(
                "{PROVIDER_HEADER_PREFIX}{}",
                header.name.to_ascii_lowercase()
            ),
            value: header.value,
        });
    }
    out
}

/// Splits a provider byte stream into complete Server-Sent Events frames (`\n\n`-terminated).
#[derive(Default)]
pub struct SseFrames {
    buffer: Vec<u8>,
    /// Leading buffered bytes already searched for a frame end, so a large frame arriving in many
    /// reads is scanned once.
    scanned: usize,
}

/// One complete frame: its exact bytes (terminator included) and its text.
pub struct SseFrame {
    pub bytes: Vec<u8>,
    pub text: String,
}

impl SseFrames {
    /// Append bytes and drain every complete frame. A frame that is not UTF-8, or an unfinished
    /// frame above [`UPSTREAM_SSE_FRAME_MAX_BYTES`], is refused.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, &'static str> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            // A frame end may straddle the previous read, so the search backs up one byte.
            let from = self.scanned.saturating_sub(1);
            let Some(found) = self.buffer[from..]
                .windows(2)
                .position(|window| window == b"\n\n")
            else {
                self.scanned = self.buffer.len();
                break;
            };
            let boundary = from + found;
            let bytes = self.buffer.drain(..boundary + 2).collect::<Vec<u8>>();
            self.scanned = 0;
            let text = std::str::from_utf8(&bytes[..boundary])
                .map_err(|_| "provider SSE frame is not valid UTF-8")?
                .to_string();
            frames.push(SseFrame { bytes, text });
        }
        if self.buffer.len() > UPSTREAM_SSE_FRAME_MAX_BYTES {
            return Err("provider SSE frame exceeds buffer cap");
        }
        Ok(frames)
    }

    /// Bytes of an unfinished trailing frame.
    pub fn take_rest(&mut self) -> Vec<u8> {
        self.scanned = 0;
        std::mem::take(&mut self.buffer)
    }

    /// No unfinished frame is buffered.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

/// The SSE event name and the joined `data:` payload of one frame.
pub fn frame_parts(frame: &str) -> (Option<&str>, String) {
    let mut name = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    (name, data.join("\n"))
}

/// What a provider frame or a whole non-streamed response says, beyond its billing record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseFacts {
    /// The model the provider says produced this output.
    pub model: Option<String>,
    /// Visible answer text.
    pub text: String,
    /// Provider-separated reasoning text.
    pub reasoning: String,
    /// A tool or function call was produced.
    pub tool_call: bool,
    /// Output that is neither text, reasoning nor a tool call: audio, images, the result of a tool
    /// the provider ran itself.
    pub media: bool,
    /// The provider's own end reason (`finish_reason`, `stop_reason` or response `status`).
    pub finish_reason: Option<String>,
    /// The frame ends the response.
    pub done: bool,
    /// The provider stated an error; carries its message (possibly empty).
    pub error: Option<String>,
}

impl ResponseFacts {
    /// Any output a client receives: answer text, reasoning, a tool call or media.
    pub fn has_output(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty() || self.tool_call || self.media
    }

    /// Output beyond reasoning: answer text, a tool call or media.
    pub fn has_visible_output(&self) -> bool {
        !self.text.is_empty() || self.tool_call || self.media
    }
}

/// One complete SSE frame of a provider response.
pub enum Frame {
    /// No `data:` payload (a comment, a keep-alive or a bare event name).
    Empty,
    /// The `[DONE]` sentinel.
    Done,
    /// A JSON payload and what it states.
    Event { value: Value, facts: ResponseFacts },
}

/// Read one complete SSE frame of `format`.
pub fn parse_frame(format: ApiFormat, frame: &str) -> Result<Frame, String> {
    let (name, data) = frame_parts(frame);
    if data.is_empty() {
        return Ok(Frame::Empty);
    }
    if data.trim() == "[DONE]" {
        return Ok(Frame::Done);
    }
    let value = serde_json::from_str::<Value>(&data)
        .map_err(|error| format!("malformed provider SSE JSON: {error}"))?;
    let mut facts = match format {
        ApiFormat::ChatCompletions => chat_chunk_facts(&value),
        ApiFormat::Responses => responses_event_facts(&value),
        ApiFormat::Messages => messages_event_facts(&value),
    };
    if name == Some("error") && facts.error.is_none() {
        facts.error = Some(error_detail(&value).unwrap_or_default());
    }
    Ok(Frame::Event { value, facts })
}

/// Read a whole non-streamed response body of `format`.
pub fn inspect_body(format: ApiFormat, value: &Value) -> ResponseFacts {
    let mut facts = match format {
        ApiFormat::ChatCompletions => chat_completion_facts(value),
        ApiFormat::Responses => responses_object_facts(value),
        ApiFormat::Messages => messages_object_facts(value),
    };
    facts.done = true;
    facts
}

fn string_at<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn reported_model(value: &Value) -> Option<String> {
    string_at(value, "model")
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// The error a JSON value states: an `error` member that is present and not null.
pub fn stated_error(value: &Value) -> Option<&Value> {
    value.get("error").filter(|error| !error.is_null())
}

/// A readable message of a stated error (or of the value itself when it is the error).
pub fn error_detail(value: &Value) -> Option<String> {
    let error = stated_error(value).unwrap_or(value);
    if let Some(detail) = error.as_str() {
        return Some(detail.to_string());
    }
    ["message", "detail", "code", "type"]
        .into_iter()
        .find_map(|field| string_at(error, field))
        .map(str::to_string)
}

fn push_text(out: &mut String, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        out.push_str(value);
    }
}

fn chat_reasoning(container: &Value) -> String {
    let mut reasoning = String::new();
    push_text(&mut reasoning, string_at(container, "reasoning"));
    push_text(&mut reasoning, string_at(container, "reasoning_content"));
    if let Some(details) = container.get("reasoning_details").and_then(Value::as_array) {
        for detail in details {
            push_text(&mut reasoning, string_at(detail, "text"));
            push_text(&mut reasoning, string_at(detail, "summary"));
        }
    }
    reasoning
}

fn chat_container_facts(container: &Value, facts: &mut ResponseFacts) {
    match container.get("content") {
        Some(Value::String(text)) => push_text(&mut facts.text, Some(text)),
        // Content as an array of parts: text parts are text, any other part is media.
        Some(Value::Array(parts)) => {
            for part in parts {
                match string_at(part, "text") {
                    Some(text) => push_text(&mut facts.text, Some(text)),
                    None => facts.media |= !part.is_null(),
                }
            }
        }
        _ => {}
    }
    push_text(&mut facts.text, string_at(container, "refusal"));
    facts.media |= container.get("audio").is_some_and(|audio| !audio.is_null())
        || container
            .get("images")
            .and_then(Value::as_array)
            .is_some_and(|images| !images.is_empty());
    facts.reasoning.push_str(&chat_reasoning(container));
    facts.tool_call |= container
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
        || container
            .get("function_call")
            .is_some_and(|call| !call.is_null());
}

fn chat_choices_facts(value: &Value, field: &str) -> ResponseFacts {
    let mut facts = ResponseFacts {
        model: reported_model(value),
        error: stated_error(value).map(|_| error_detail(value).unwrap_or_default()),
        ..ResponseFacts::default()
    };
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(container) = choice.get(field) {
                chat_container_facts(container, &mut facts);
            }
            if let Some(reason) = string_at(choice, "finish_reason") {
                facts.finish_reason = Some(reason.to_string());
            }
        }
    }
    facts
}

fn chat_chunk_facts(value: &Value) -> ResponseFacts {
    chat_choices_facts(value, "delta")
}

fn chat_completion_facts(value: &Value) -> ResponseFacts {
    chat_choices_facts(value, "message")
}

fn responses_event_facts(value: &Value) -> ResponseFacts {
    let kind = string_at(value, "type").unwrap_or_default();
    let mut facts = ResponseFacts::default();
    match kind {
        "response.output_text.delta" | "response.refusal.delta" => {
            push_text(&mut facts.text, string_at(value, "delta"));
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            push_text(&mut facts.reasoning, string_at(value, "delta"));
        }
        "response.function_call_arguments.delta" => {
            facts.tool_call = true;
        }
        "response.image_generation_call.partial_image" => facts.media = true,
        // Every other delta streams output too: a tool's input, or audio and its transcript.
        kind if kind.ends_with(".delta") => {
            if kind.contains("_call") {
                facts.tool_call = true;
            } else {
                facts.media = true;
            }
        }
        "response.output_item.added" => {
            facts.tool_call = value
                .get("item")
                .and_then(|item| string_at(item, "type"))
                .is_some_and(|kind| kind.ends_with("_call"));
        }
        "error" => {
            facts.error = Some(error_detail(value).unwrap_or_default());
        }
        _ => {}
    }
    if let Some(response) = value.get("response") {
        facts.model = reported_model(response);
        if matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.failed"
        ) {
            facts.done = true;
            facts.finish_reason = string_at(response, "status").map(str::to_string);
            if kind == "response.failed" {
                facts.error = Some(
                    stated_error(response)
                        .and_then(|_| error_detail(response))
                        .unwrap_or_default(),
                );
            }
        }
    }
    facts
}

fn responses_object_facts(value: &Value) -> ResponseFacts {
    let mut facts = ResponseFacts {
        model: reported_model(value),
        finish_reason: string_at(value, "status").map(str::to_string),
        error: stated_error(value).map(|_| error_detail(value).unwrap_or_default()),
        ..ResponseFacts::default()
    };
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match string_at(item, "type").unwrap_or_default() {
            "message" => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match (string_at(part, "text"), string_at(part, "refusal")) {
                        (None, None) => facts.media |= !part.is_null(),
                        (text, refusal) => {
                            push_text(&mut facts.text, text);
                            push_text(&mut facts.text, refusal);
                        }
                    }
                }
            }
            "reasoning" => {
                for field in ["summary", "content"] {
                    for part in item
                        .get(field)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        push_text(&mut facts.reasoning, string_at(part, "text"));
                    }
                }
            }
            kind if kind.ends_with("_call") => facts.tool_call = true,
            _ => {}
        }
    }
    facts
}

fn messages_event_facts(value: &Value) -> ResponseFacts {
    let kind = string_at(value, "type").unwrap_or_default();
    let mut facts = ResponseFacts::default();
    match kind {
        "message_start" => {
            if let Some(message) = value.get("message") {
                facts.model = reported_model(message);
            }
        }
        "content_block_start" => {
            match value
                .get("content_block")
                .and_then(|block| string_at(block, "type"))
                .unwrap_or_default()
            {
                kind if kind.ends_with("tool_use") => facts.tool_call = true,
                // A block that starts empty and streams its deltas.
                "" | "text" | "thinking" => {}
                // A provider-run tool's result, redacted reasoning, an image: output on arrival.
                _ => facts.media = true,
            }
        }
        "content_block_delta" => {
            if let Some(delta) = value.get("delta") {
                match string_at(delta, "type").unwrap_or_default() {
                    "text_delta" => push_text(&mut facts.text, string_at(delta, "text")),
                    "thinking_delta" => {
                        push_text(&mut facts.reasoning, string_at(delta, "thinking"))
                    }
                    "input_json_delta" => facts.tool_call = true,
                    // Metadata of the text or reasoning block it belongs to.
                    "signature_delta" | "citations_delta" | "" => {}
                    _ => facts.media = true,
                }
            }
        }
        "message_delta" => {
            facts.finish_reason = value
                .get("delta")
                .and_then(|delta| string_at(delta, "stop_reason"))
                .map(str::to_string);
        }
        "message_stop" => facts.done = true,
        "error" => facts.error = Some(error_detail(value).unwrap_or_default()),
        _ => {}
    }
    facts
}

fn messages_object_facts(value: &Value) -> ResponseFacts {
    let mut facts = ResponseFacts {
        model: reported_model(value),
        finish_reason: string_at(value, "stop_reason").map(str::to_string),
        error: (string_at(value, "type") == Some("error") || stated_error(value).is_some())
            .then(|| error_detail(value).unwrap_or_default()),
        ..ResponseFacts::default()
    };
    for block in value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match string_at(block, "type").unwrap_or_default() {
            "text" => push_text(&mut facts.text, string_at(block, "text")),
            "thinking" => push_text(&mut facts.reasoning, string_at(block, "thinking")),
            kind if kind.ends_with("tool_use") => facts.tool_call = true,
            "" => {}
            _ => facts.media = true,
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn paths_name_the_three_formats_and_nothing_else() {
        for format in [
            ApiFormat::ChatCompletions,
            ApiFormat::Responses,
            ApiFormat::Messages,
        ] {
            assert_eq!(ApiFormat::from_path(format.path()), Some(format));
        }
        for other in [
            "/v1/embeddings",
            "/v1/chat/completions/",
            "/chat/completions",
            "",
        ] {
            assert_eq!(ApiFormat::from_path(other), None, "{other}");
        }
    }

    #[test]
    fn client_credentials_cookies_and_transport_headers_never_travel() {
        for name in [
            "Authorization",
            "x-api-key",
            "API-Key",
            "OpenAI-Organization",
            "openai-project",
            "Cookie",
            "Host",
            "Content-Length",
            "content-type",
            "Accept-Encoding",
            "Connection",
            "Transfer-Encoding",
            "keep-alive",
            "TE",
            "Upgrade",
            "Proxy-Authorization",
        ] {
            assert!(!forwards_request_header(name), "{name}");
        }
        for name in [
            "session_id",
            "conversation_id",
            "User-Agent",
            "anthropic-beta",
            "anthropic-version",
            "OpenAI-Beta",
            "x-custom",
        ] {
            assert!(forwards_request_header(name), "{name}");
        }
        for name in [
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Content-Encoding",
            "Set-Cookie",
            "keep-alive",
        ] {
            assert!(!forwards_response_header(name), "{name}");
        }
        for name in [
            "Content-Type",
            "retry-after",
            "x-ratelimit-remaining-requests",
            "request-id",
            "openai-processing-ms",
        ] {
            assert!(forwards_response_header(name), "{name}");
        }
    }

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.into(),
            value: value.as_bytes().to_vec(),
        }
    }

    fn names(headers: &[Header]) -> Vec<String> {
        headers.iter().map(|header| header.name.clone()).collect()
    }

    #[test]
    fn a_frame_far_above_a_megabyte_passes_whole_across_many_reads() {
        let payload = "x".repeat(8 * 1024 * 1024);
        let frame = format!("data: {{\"partial_image_b64\":\"{payload}\"}}\n\n");
        let mut frames = SseFrames::default();
        let mut complete = Vec::new();
        for chunk in frame.as_bytes().chunks(16 * 1024) {
            complete.extend(
                frames
                    .push(chunk)
                    .expect("a frame under the cap is accepted"),
            );
        }
        assert_eq!(complete.len(), 1);
        assert_eq!(complete[0].bytes, frame.as_bytes());
        assert!(frames.is_empty());
    }

    #[test]
    fn headers_a_connection_header_lists_never_travel() {
        let headers = vec![
            header("Connection", "x-hop, X-Other-Hop"),
            header("x-hop", "1"),
            header("x-other-hop", "2"),
            header("x-end-to-end", "3"),
        ];
        assert_eq!(
            names(&forwarded_request_headers(headers.clone())),
            ["x-end-to-end"]
        );
        assert_eq!(
            names(&forwarded_response_headers(headers)),
            ["x-end-to-end"]
        );
    }

    #[test]
    fn the_client_sees_provider_headers_under_the_prefix_and_retry_headers_also_as_sent() {
        let headers = client_response_headers(&[
            header("Request-Id", "req_1"),
            header("Retry-After", "3"),
            header("retry-after-ms", "3000"),
            header("x-ratelimit-remaining-requests", "7"),
            header("anthropic-ratelimit-tokens-remaining", "9"),
            header("x-ms-ratelimit-remaining", "4"),
            header("Location", "http://elsewhere.invalid"),
            header("Set-Cookie", "provider-session=secret"),
        ]);
        assert_eq!(
            names(&headers),
            [
                "llm_provider-request-id",
                "Retry-After",
                "llm_provider-retry-after",
                "retry-after-ms",
                "llm_provider-retry-after-ms",
                "x-ratelimit-remaining-requests",
                "llm_provider-x-ratelimit-remaining-requests",
                "anthropic-ratelimit-tokens-remaining",
                "llm_provider-anthropic-ratelimit-tokens-remaining",
                "llm_provider-x-ms-ratelimit-remaining",
                "llm_provider-location",
            ]
        );
        assert_eq!(headers[0].value, b"req_1");
        assert_eq!(headers[2].value, b"3");
    }

    #[test]
    fn the_usage_opt_in_keeps_the_clients_other_stream_options() {
        let mut body = RequestBody::parse(
            br#"{"stream":true,"stream_options":{"include_obfuscation":false,"continuous_usage_stats":true}}"#,
        )
        .unwrap();
        body.require_stream_usage();
        assert_eq!(
            Value::Object(body.0.clone())["stream_options"],
            serde_json::json!({"include_obfuscation": false, "include_usage": true}),
            "per-frame usage is removed, the client's other options stay"
        );
        let mut replaced = RequestBody::parse(br#"{"stream_options":null}"#).unwrap();
        replaced.require_stream_usage();
        assert_eq!(
            Value::Object(replaced.0)["stream_options"],
            serde_json::json!({"include_usage": true})
        );
    }

    #[test]
    fn the_request_model_is_absent_unless_a_non_empty_string() {
        for (body, model) in [
            (r#"{"model":"m"}"#, Some("m")),
            (r#"{"model":""}"#, None),
            (r#"{"model":7}"#, None),
            (r#"{}"#, None),
        ] {
            assert_eq!(
                RequestBody::parse(body.as_bytes()).unwrap().model(),
                model,
                "{body}"
            );
        }
        assert!(RequestBody::parse(b"[]").is_err());
        assert!(RequestBody::parse(b"not json").is_err());
    }

    #[test]
    fn frames_of_each_format_state_model_output_end_and_errors() {
        let chat = parse_frame(
            ApiFormat::ChatCompletions,
            r#"data: {"model":"m","choices":[{"delta":{"content":"a","reasoning":"r"},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        let Frame::Event { facts, .. } = chat else {
            panic!()
        };
        assert_eq!(
            (
                facts.model.as_deref(),
                facts.text.as_str(),
                facts.reasoning.as_str()
            ),
            (Some("m"), "a", "r")
        );
        assert_eq!(facts.finish_reason.as_deref(), Some("stop"));
        assert!(matches!(
            parse_frame(ApiFormat::ChatCompletions, "data: [DONE]").unwrap(),
            Frame::Done
        ));
        let Frame::Event { facts, .. } = parse_frame(
            ApiFormat::ChatCompletions,
            r#"data: {"error":null,"choices":[{"delta":{"content":"kept"}}]}"#,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!((facts.text.as_str(), facts.error), ("kept", None));
        let Frame::Event { facts, .. } = parse_frame(
            ApiFormat::ChatCompletions,
            "event: error\ndata: {\"message\":\"overloaded\"}",
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(facts.error.as_deref(), Some("overloaded"));

        let Frame::Event { facts, .. } = parse_frame(
            ApiFormat::Responses,
            r#"data: {"type":"response.completed","response":{"model":"m","status":"incomplete"}}"#,
        )
        .unwrap() else {
            panic!()
        };
        assert!(facts.done);
        assert_eq!(facts.finish_reason.as_deref(), Some("incomplete"));
        let Frame::Event { facts, .. } = parse_frame(
            ApiFormat::Responses,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call"}}"#,
        )
        .unwrap() else {
            panic!()
        };
        assert!(facts.tool_call);

        for (frame, expect_text, expect_tool, expect_done) in [
            (
                r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"t"}}"#,
                "t",
                false,
                false,
            ),
            (
                r#"data: {"type":"content_block_start","content_block":{"type":"tool_use"}}"#,
                "",
                true,
                false,
            ),
            (
                r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}"#,
                "",
                true,
                false,
            ),
            (r#"data: {"type":"message_stop"}"#, "", false, true),
        ] {
            let Frame::Event { facts, .. } = parse_frame(ApiFormat::Messages, frame).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                (facts.text.as_str(), facts.tool_call, facts.done),
                (expect_text, expect_tool, expect_done),
                "{frame}"
            );
        }
        assert!(parse_frame(ApiFormat::Messages, "data: {").is_err());
    }

    #[test]
    fn whole_responses_of_each_format_state_their_output() {
        let chat = inspect_body(
            ApiFormat::ChatCompletions,
            &serde_json::json!({"model":"m","choices":[{"message":{"content":"","tool_calls":[{"id":"c"}]},"finish_reason":"tool_calls"}]}),
        );
        assert!(chat.tool_call && chat.has_output() && chat.text.is_empty());
        assert_eq!(chat.finish_reason.as_deref(), Some("tool_calls"));
        let responses = inspect_body(
            ApiFormat::Responses,
            &serde_json::json!({"model":"m","status":"completed","output":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"why"}]},
                {"type":"message","content":[{"type":"output_text","text":"answer"}]}]}),
        );
        assert_eq!(
            (responses.text.as_str(), responses.reasoning.as_str()),
            ("answer", "why")
        );
        let messages = inspect_body(
            ApiFormat::Messages,
            &serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        );
        assert_eq!(messages.error.as_deref(), Some("Overloaded"));
        assert!(!messages.has_output());
    }

    /// Output that is not plain text still counts as output, so a positive bill for it is never
    /// refused as "usage without output" after the client already received it.
    #[test]
    fn audio_parts_images_and_provider_tool_results_are_output() {
        let frame = |format, data: &str| match parse_frame(format, data).unwrap() {
            Frame::Event { facts, .. } => facts,
            _ => panic!("{data}"),
        };
        for (format, data) in [
            (
                ApiFormat::ChatCompletions,
                r#"data: {"choices":[{"delta":{"audio":{"data":"AAA"}}}]}"#,
            ),
            (
                ApiFormat::ChatCompletions,
                r#"data: {"choices":[{"delta":{"content":[{"type":"image_url","image_url":{"url":"x"}}]}}]}"#,
            ),
            (
                ApiFormat::Responses,
                r#"data: {"type":"response.audio.delta","delta":"AAA"}"#,
            ),
            (
                ApiFormat::Responses,
                r#"data: {"type":"response.audio.transcript.delta","delta":"hi"}"#,
            ),
            (
                ApiFormat::Responses,
                r#"data: {"type":"response.image_generation_call.partial_image","partial_image_b64":"AAA"}"#,
            ),
            (
                ApiFormat::Responses,
                r#"data: {"type":"response.code_interpreter_call_code.delta","delta":"print(1)"}"#,
            ),
            (
                ApiFormat::Messages,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","content":[]}}"#,
            ),
            (
                ApiFormat::Messages,
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"x"}}"#,
            ),
        ] {
            let facts = frame(format, data);
            assert!(facts.has_output() && facts.has_visible_output(), "{data}");
        }
        let parts = frame(
            ApiFormat::ChatCompletions,
            r#"data: {"choices":[{"delta":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}]}"#,
        );
        assert_eq!(parts.text, "ab");
        assert!(!parts.media);
        for data in [
            r#"data: {"type":"content_block_delta","delta":{"type":"signature_delta","signature":"s"}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ] {
            assert!(!frame(ApiFormat::Messages, data).has_output(), "{data}");
        }
        let whole = inspect_body(
            ApiFormat::Messages,
            &serde_json::json!({"type":"message","content":[{"type":"web_search_tool_result","content":[]}]}),
        );
        assert!(whole.has_visible_output());
        let whole = inspect_body(
            ApiFormat::ChatCompletions,
            &serde_json::json!({"choices":[{"message":{"content":null,"audio":{"id":"a","data":"AAA"}}}]}),
        );
        assert!(whole.has_visible_output());
    }

    proptest! {
        /// However the network splits a response, the frames read from it, their exact bytes and
        /// the unfinished tail are the same.
        #[test]
        fn frames_do_not_depend_on_read_boundaries(
            pieces in proptest::collection::vec("[a-z \\n\u{41f}\u{442}]{0,12}", 1..12),
            cuts in proptest::collection::vec(0usize..64, 0..8),
        ) {
            let body: String = pieces.concat();
            let bytes = body.as_bytes();
            let mut whole = SseFrames::default();
            let expected: Vec<Vec<u8>> = whole.push(bytes).unwrap().into_iter().map(|f| f.bytes).collect();
            let expected_rest = whole.take_rest();

            let mut cuts: Vec<usize> = cuts.into_iter().map(|cut| cut.min(bytes.len())).collect();
            cuts.sort_unstable();
            let mut split = SseFrames::default();
            let mut got = Vec::new();
            let mut start = 0;
            for cut in cuts.into_iter().chain(std::iter::once(bytes.len())) {
                for frame in split.push(&bytes[start..cut]).unwrap() {
                    got.push(frame.bytes);
                }
                start = cut;
            }
            prop_assert_eq!(got, expected);
            prop_assert_eq!(split.take_rest(), expected_rest);
        }
    }
}
