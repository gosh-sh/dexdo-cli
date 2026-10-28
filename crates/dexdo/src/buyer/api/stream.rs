//! The buyer's reading of the seller's frames: the provider's head, its bytes and the terminal
//! usage. Streamed bytes are read frame by frame before they are released, so the model check
//! ([`ModelCheck`]) runs ahead of the output it judges.

use crate::api_format::{parse_frame, ApiFormat, Frame, ResponseFacts, SseFrames};
use crate::buyer::verify::{ModelCheck, Verdict};
use dexdo_core::params::UPSTREAM_RESPONSE_BODY_MAX_BYTES;
use dexdo_proto::{BillingUsage, ResponseHead, StreamFrame};
use tokio_stream::StreamExt;

pub(super) enum StreamError {
    Upstream(tonic::Status),
    Local(String),
}

impl StreamError {
    pub(super) fn policy_action(&self, received: u64) -> super::StreamErrorPolicyAction {
        if let Self::Upstream(status) = self {
            if let Some(started) = dexdo_proto::upstream_output_started(status) {
                return if started || received > 0 {
                    super::StreamErrorPolicyAction::ReconcileUsage
                } else {
                    super::StreamErrorPolicyAction::RequestScoped
                };
            }
        }
        if received == 0 && self.is_retryable_upstream() {
            super::StreamErrorPolicyAction::RequestScoped
        } else {
            super::stream_error_policy_action(self.policy_message(), received)
        }
    }

    pub(super) fn is_retryable_upstream(&self) -> bool {
        matches!(self, Self::Upstream(status) if dexdo_proto::is_retryable_upstream(status))
    }

    pub(super) fn retryable_rejection(&self) -> super::ConsumerError {
        super::ConsumerError::new(
            super::ConsumerErrorCode::UpstreamRetryable,
            format!("temporary provider failure; retry this request on the same deal: {self}"),
        )
    }

    pub(super) fn is_capacity(&self) -> bool {
        matches!(self, Self::Upstream(status) if status.code() == tonic::Code::ResourceExhausted
            && dexdo_proto::upstream_output_started(status).is_none())
    }

    pub(super) fn policy_message(&self) -> &str {
        match self {
            Self::Upstream(status) => status.message(),
            Self::Local(message) => message,
        }
    }
}

impl From<String> for StreamError {
    fn from(message: String) -> Self {
        Self::Local(message)
    }
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upstream(status) => status.fmt(formatter),
            Self::Local(message) => formatter.write_str(message),
        }
    }
}

pub(super) enum FrameNext {
    /// Provider bytes to release, with how many output-carrying frames and how many frames with
    /// visible output (answer text or a tool call) they hold.
    Data {
        bytes: Vec<u8>,
        output: u64,
        text: u64,
    },
    Usage(BillingUsage),
    End,
    Bailed(String),
    Errored(StreamError),
}

/// What the streamed answer stated so far, for ending it in the client's format.
#[derive(Clone, Debug, Default)]
pub(super) struct StreamFacts {
    pub(super) id: Option<String>,
    pub(super) model: Option<String>,
    pub(super) finish_reason: Option<String>,
    /// The Messages content block that is open, if any.
    pub(super) open_block: Option<u64>,
    /// The bytes forwarded so far end inside an unfinished SSE frame.
    pub(super) unterminated: bool,
    /// The client has received the provider's first frame, which opens the answer
    /// (`message_start`, `response.created`).
    pub(super) opened: bool,
}

pub(super) struct FrameDriver {
    upstream: tonic::Streaming<StreamFrame>,
    session_closed: tokio::sync::watch::Receiver<bool>,
    format: ApiFormat,
    streamed: bool,
    check: ModelCheck,
    frames: SseFrames,
    billing_grant: u64,
    head_status: Option<u32>,
    saw_usage: bool,
    facts: StreamFacts,
    /// The provider's terminal frames (from the one stating its end reason on), held until the
    /// seller's bill is recorded so a client that closes on them cannot leave the bill unrecorded.
    held: Vec<u8>,
}

impl FrameDriver {
    pub(super) fn new(
        upstream: tonic::Streaming<StreamFrame>,
        session_closed: tokio::sync::watch::Receiver<bool>,
        format: ApiFormat,
        streamed: bool,
        check: ModelCheck,
        billing_grant: u64,
    ) -> Self {
        Self {
            upstream,
            session_closed,
            format,
            streamed,
            check,
            frames: SseFrames::default(),
            billing_grant,
            head_status: None,
            saw_usage: false,
            facts: StreamFacts::default(),
            held: Vec::new(),
        }
    }

    #[allow(clippy::result_large_err)]
    async fn recv(&mut self) -> Result<Option<StreamFrame>, StreamError> {
        let closed = || {
            Err(StreamError::from(
                "deal session closed after accepted-output deadline".to_string(),
            ))
        };
        if *self.session_closed.borrow() {
            return closed();
        }
        let next = tokio::select! {
            next = self.upstream.next() => next,
            changed = self.session_closed.changed() => {
                if changed.is_ok() && *self.session_closed.borrow() {
                    return closed();
                }
                self.upstream.next().await
            }
        };
        next.transpose().map_err(StreamError::Upstream)
    }

    /// The provider's response head, which the seller sends first.
    #[allow(clippy::result_large_err)]
    pub(super) async fn head(&mut self) -> Result<ResponseHead, StreamError> {
        match self.recv().await? {
            Some(StreamFrame {
                head: Some(head), ..
            }) if (200..=299).contains(&head.status) || (400..=599).contains(&head.status) => {
                self.head_status = Some(head.status);
                Ok(head)
            }
            Some(StreamFrame {
                head: Some(head), ..
            }) => Err(format!(
                "the seller stated response status {}, which is not an answer or an error",
                head.status
            )
            .into()),
            Some(_) => Err("the seller sent content before the response head"
                .to_string()
                .into()),
            None => Err("the seller's stream ended before the response head"
                .to_string()
                .into()),
        }
    }

    fn success(&self) -> bool {
        self.head_status
            .is_some_and(|status| (200..300).contains(&status))
    }

    pub(super) async fn next(&mut self) -> FrameNext {
        let frame = match self.recv().await {
            Ok(Some(frame)) => frame,
            Ok(None) if self.saw_usage || !self.success() => return FrameNext::End,
            Ok(None) => {
                return FrameNext::Errored(
                    "response stream ended without terminal billing usage"
                        .to_string()
                        .into(),
                )
            }
            Err(error) => return FrameNext::Errored(error),
        };
        if self.saw_usage {
            return FrameNext::Errored(
                "response content followed terminal billing usage"
                    .to_string()
                    .into(),
            );
        }
        if frame.head.is_some() {
            return FrameNext::Errored(
                "the seller stated a second response head"
                    .to_string()
                    .into(),
            );
        }
        if let Some(usage) = frame.usage {
            if let Err(error) = usage.validate() {
                return FrameNext::Errored(error.to_string().into());
            }
            if !self.success() {
                return FrameNext::Errored(
                    "terminal billing usage follows no successful response"
                        .to_string()
                        .into(),
                );
            }
            if usage.billable_tokens() > self.billing_grant {
                return FrameNext::Errored(
                    format!(
                        "terminal billable usage {} exceeds billing grant {}",
                        usage.billable_tokens(),
                        self.billing_grant
                    )
                    .into(),
                );
            }
            if usage.output_tokens == 0 {
                return FrameNext::Errored(
                    "terminal billing usage has no preceding delivered output"
                        .to_string()
                        .into(),
                );
            }
            self.saw_usage = true;
            return FrameNext::Usage(usage);
        }
        if !self.streamed || !self.success() {
            return FrameNext::Data {
                bytes: frame.data,
                output: 0,
                text: 0,
            };
        }
        let complete = match self.frames.push(&frame.data) {
            Ok(complete) => complete,
            Err(error) => return FrameNext::Errored(error.to_string().into()),
        };
        let (mut output, mut text) = (0, 0);
        let mut terminal = !self.held.is_empty();
        for complete in complete {
            let parsed = match parse_frame(self.format, &complete.text) {
                Ok(parsed) => parsed,
                Err(error) => return FrameNext::Errored(error.into()),
            };
            terminal |= matches!(parsed, Frame::Done);
            let Frame::Event { value, facts } = parsed else {
                continue;
            };
            terminal |= facts.done || facts.finish_reason.is_some();
            if let Some(reported) = facts.model.as_deref() {
                if let Verdict::Bail(reason) = self.check.verify(reported) {
                    return FrameNext::Bailed(reason);
                }
            }
            self.observe(&value, &facts);
            output += u64::from(facts.has_output());
            text += u64::from(facts.has_visible_output());
        }
        if terminal {
            if self.held.len() + frame.data.len() > UPSTREAM_RESPONSE_BODY_MAX_BYTES {
                return FrameNext::Errored(
                    "the provider's terminal frames exceed the buyer's response size limit"
                        .to_string()
                        .into(),
                );
            }
            self.held.extend_from_slice(&frame.data);
            return FrameNext::Data {
                bytes: Vec::new(),
                output,
                text,
            };
        }
        self.facts.opened |= !frame.data.is_empty();
        FrameNext::Data {
            bytes: frame.data,
            output,
            text,
        }
    }

    /// The provider's held terminal frames, to release once the bill is recorded.
    pub(super) fn take_held(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    fn observe(&mut self, value: &serde_json::Value, facts: &ResponseFacts) {
        let id = value
            .get("id")
            .or_else(|| value.pointer("/message/id"))
            .or_else(|| value.pointer("/response/id"))
            .and_then(serde_json::Value::as_str);
        if let Some(id) = id {
            self.facts.id = Some(id.to_string());
        }
        if let Some(model) = &facts.model {
            self.facts.model = Some(model.clone());
        }
        if let Some(reason) = &facts.finish_reason {
            self.facts.finish_reason = Some(reason.clone());
        }
        if self.format == ApiFormat::Messages {
            match value.get("type").and_then(serde_json::Value::as_str) {
                Some("content_block_start") => {
                    self.facts.open_block = value.get("index").and_then(serde_json::Value::as_u64)
                }
                Some("content_block_stop") => self.facts.open_block = None,
                _ => {}
            }
        }
    }

    pub(super) fn facts(&self) -> StreamFacts {
        StreamFacts {
            unterminated: !self.frames.is_empty(),
            ..self.facts.clone()
        }
    }
}

fn sse(name: Option<&str>, data: &serde_json::Value) -> String {
    match name {
        Some(name) => format!("event: {name}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

/// The end of a broken or bailed streamed answer, in the client's format: an `upstream_retryable`
/// error first when the provider failed before its first word on a temporary fault, the format's
/// opening event when the client has received none, then the format's terminal events carrying
/// `finish_reason`.
pub(super) fn break_frames(
    format: ApiFormat,
    facts: &StreamFacts,
    frame_model: &str,
    finish_reason: &str,
    retryable: Option<&super::ConsumerError>,
) -> Vec<u8> {
    let anthropic = format == ApiFormat::Messages;
    let id = facts.id.as_deref().unwrap_or("dexdo-buyer");
    let model = facts.model.as_deref().unwrap_or(frame_model);
    let mut out = String::new();
    if let Some(error) = retryable {
        let body = error.body(anthropic);
        out.push_str(&match format {
            ApiFormat::ChatCompletions => sse(None, &body),
            ApiFormat::Responses => {
                let mut event = body["error"].clone();
                event["type"] = "error".into();
                sse(Some("error"), &event)
            }
            ApiFormat::Messages => sse(Some("error"), &body),
        });
    }
    if !facts.opened {
        match format {
            ApiFormat::ChatCompletions => {}
            ApiFormat::Responses => out.push_str(&sse(
                Some("response.created"),
                &serde_json::json!({
                    "type": "response.created",
                    "response": {
                        "id": id,
                        "object": "response",
                        "status": "in_progress",
                        "model": model,
                        "output": [],
                    },
                }),
            )),
            ApiFormat::Messages => out.push_str(&sse(
                Some("message_start"),
                &serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": id,
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    },
                }),
            )),
        }
    }
    match format {
        ApiFormat::ChatCompletions => {
            out.push_str(&sse(
                None,
                &serde_json::json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
                }),
            ));
            out.push_str("data: [DONE]\n\n");
        }
        ApiFormat::Responses => {
            // Every Responses break, a substitution bail included, ends as `response.failed`.
            let kind = "response.failed";
            let response = serde_json::json!({
                "id": id,
                "object": "response",
                "status": "failed",
                "model": model,
                "output": [],
                "error": {
                    "code": finish_reason,
                    "message": format!("buyer stream ended: {finish_reason}"),
                },
            });
            out.push_str(&sse(
                Some(kind),
                &serde_json::json!({ "type": kind, "response": response }),
            ));
        }
        ApiFormat::Messages => {
            if let Some(index) = facts.open_block {
                out.push_str(&sse(
                    Some("content_block_stop"),
                    &serde_json::json!({ "type": "content_block_stop", "index": index }),
                ));
            }
            out.push_str(&sse(
                Some("message_delta"),
                &serde_json::json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": finish_reason, "stop_sequence": null },
                    "usage": { "output_tokens": 0 },
                }),
            ));
            out.push_str(&sse(
                Some("message_stop"),
                &serde_json::json!({ "type": "message_stop" }),
            ));
        }
    }
    out.into_bytes()
}
