//! Gateway mock model: fake output instead of calling a real model
//! . A standard debug mode in production code, not a
//! `#[cfg(test)]` crutch. It answers in the format of the requested path, the way a provider
//! serving that path would, so the whole forwarding path runs unchanged in mock mode.

use super::{send_event, ProviderRequest, UpstreamEvent, UpstreamResult};
use crate::api_format::ApiFormat;
use dexdo_proto::{BillingUsage, Header, ResponseHead};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tonic::Status;

/// The model a substituting mock answers as.
pub const SUBSTITUTED_MODEL: &str = "substituted/cheap-model";

/// Run the mock upstream: build up to `mock_token_count` deterministic fake words **from the prompt**
/// and send them incrementally into `tx` (R6, word by word), followed by the same terminal
/// input/output/total record used by real adapters. Both sides know the output is fake
/// . `answer_as` overrides the model the mock says produced the output.
pub async fn run(
    request: &ProviderRequest,
    mock_token_count: u64,
    answer_as: Option<&str>,
    tx: mpsc::Sender<UpstreamResult>,
) {
    crate::telemetry::attempt(None);
    let format = request.format;
    let body = &request.body.0;
    let prompt = last_user_text(format, body);

    // fixture -- the mock answers as a DIFFERENT model than the market's -> the buyer's check rejects it.
    let model = if prompt.contains("DEXDO_FIXTURE_SUBSTITUTE") {
        SUBSTITUTED_MODEL
    } else {
        answer_as.or_else(|| request.body.model()).unwrap_or("mock")
    };
    let count = requested_output_limit(format, body)
        .map_or(mock_token_count, |limit| limit.min(mock_token_count));
    let words = derive_words(&prompt, count);
    let input_tokens = match input_words(format, body) {
        Ok(tokens) => tokens,
        Err(error) => {
            let _ = send_event(&tx, Err(Status::data_loss(error))).await;
            return;
        }
    };
    let usage = match BillingUsage::new(input_tokens, words.len() as u64, None) {
        Ok(usage) => usage,
        Err(error) => {
            let _ = send_event(&tx, Err(Status::data_loss(error))).await;
            return;
        }
    };
    let stream = request.body.stream();
    let head = ResponseHead {
        status: 200,
        headers: vec![Header {
            name: "content-type".to_string(),
            value: if stream {
                b"text/event-stream".to_vec()
            } else {
                b"application/json".to_vec()
            },
        }],
    };
    if send_event(&tx, Ok(UpstreamEvent::Head(head)))
        .await
        .is_err()
    {
        return;
    }
    let events = if stream {
        stream_events(format, model, &words, &usage)
    } else {
        vec![(
            serde_json::to_vec(&whole_response(format, model, &words.concat(), &usage))
                .expect("mock response serializes"),
            !words.is_empty(),
        )]
    };
    for (bytes, output) in events {
        if send_event(&tx, Ok(UpstreamEvent::Data { bytes, output }))
            .await
            .is_err()
        {
            return; // buyer disconnected before terminal usage
        }
    }
    let _ = send_event(&tx, Ok(UpstreamEvent::Usage(usage))).await;
}

fn sse(name: Option<&str>, data: &Value) -> Vec<u8> {
    let mut frame = String::new();
    if let Some(name) = name {
        frame.push_str("event: ");
        frame.push_str(name);
        frame.push('\n');
    }
    frame.push_str("data: ");
    frame.push_str(&data.to_string());
    frame.push_str("\n\n");
    frame.into_bytes()
}

fn chat_usage(usage: &BillingUsage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
    })
}

fn responses_usage(usage: &BillingUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
    })
}

fn whole_response(format: ApiFormat, model: &str, text: &str, usage: &BillingUsage) -> Value {
    match format {
        ApiFormat::ChatCompletions => json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop",
            }],
            "usage": chat_usage(usage),
        }),
        ApiFormat::Responses => json!({
            "id": "resp_mock",
            "object": "response",
            "model": model,
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_mock",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            }],
            "usage": responses_usage(usage),
        }),
        ApiFormat::Messages => json!({
            "id": "msg_mock",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{ "type": "text", "text": text }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens },
        }),
    }
}

/// The frames a provider streams for `words`, each paired with whether it carries output.
fn stream_events(
    format: ApiFormat,
    model: &str,
    words: &[String],
    usage: &BillingUsage,
) -> Vec<(Vec<u8>, bool)> {
    let mut events = Vec::new();
    match format {
        ApiFormat::ChatCompletions => {
            for (index, word) in words.iter().enumerate() {
                let mut delta = json!({ "content": word });
                if index == 0 {
                    delta["role"] = "assistant".into();
                }
                let chunk = json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": null }],
                });
                events.push((sse(None, &chunk), true));
            }
            let finish = json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            });
            events.push((sse(None, &finish), false));
            let usage = json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [],
                "usage": chat_usage(usage),
            });
            events.push((sse(None, &usage), false));
            events.push((b"data: [DONE]\n\n".to_vec(), false));
        }
        ApiFormat::Responses => {
            let created = json!({
                "type": "response.created",
                "response": { "id": "resp_mock", "object": "response", "model": model, "status": "in_progress" },
            });
            events.push((sse(Some("response.created"), &created), false));
            for word in words {
                let delta = json!({
                    "type": "response.output_text.delta",
                    "item_id": "msg_mock",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": word,
                });
                events.push((sse(Some("response.output_text.delta"), &delta), true));
            }
            let completed = json!({
                "type": "response.completed",
                "response": whole_response(ApiFormat::Responses, model, &words.concat(), usage),
            });
            events.push((sse(Some("response.completed"), &completed), false));
        }
        ApiFormat::Messages => {
            let start = json!({
                "type": "message_start",
                "message": {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "usage": { "input_tokens": usage.input_tokens, "output_tokens": 0 },
                },
            });
            events.push((sse(Some("message_start"), &start), false));
            let block = json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            });
            events.push((sse(Some("content_block_start"), &block), false));
            for word in words {
                let delta = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": word },
                });
                events.push((sse(Some("content_block_delta"), &delta), true));
            }
            let stop_block = json!({ "type": "content_block_stop", "index": 0 });
            events.push((sse(Some("content_block_stop"), &stop_block), false));
            let delta = json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": usage.output_tokens },
            });
            events.push((sse(Some("message_delta"), &delta), false));
            events.push((
                sse(Some("message_stop"), &json!({ "type": "message_stop" })),
                false,
            ));
        }
    }
    events
}

/// The output limit the request asks for, in whichever field its format uses.
fn requested_output_limit(format: ApiFormat, body: &serde_json::Map<String, Value>) -> Option<u64> {
    let fields: &[&str] = match format {
        ApiFormat::ChatCompletions => &["max_completion_tokens", "max_tokens"],
        ApiFormat::Responses => &["max_output_tokens"],
        ApiFormat::Messages => &["max_tokens"],
    };
    fields
        .iter()
        .filter_map(|field| body.get(*field).and_then(Value::as_u64))
        .min()
}

/// Text of one message `content`: a string or an array of text parts.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// `(role, text)` of every message in the request, in order, including system instructions.
fn messages(format: ApiFormat, body: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let system_field = match format {
        ApiFormat::Responses => Some("instructions"),
        ApiFormat::Messages => Some("system"),
        ApiFormat::ChatCompletions => None,
    };
    if let Some(system) = system_field.and_then(|field| body.get(field)) {
        out.push(("system".to_string(), content_text(Some(system))));
    }
    let list_field = if format == ApiFormat::Responses {
        "input"
    } else {
        "messages"
    };
    match body.get(list_field) {
        Some(Value::String(text)) => out.push(("user".to_string(), text.clone())),
        Some(Value::Array(items)) => {
            for item in items {
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                out.push((role, content_text(item.get("content"))));
            }
        }
        _ => {}
    }
    out
}

fn last_user_text(format: ApiFormat, body: &serde_json::Map<String, Value>) -> String {
    messages(format, body)
        .into_iter()
        .rev()
        .find(|(role, _)| role == "user")
        .map(|(_, text)| text)
        .unwrap_or_default()
}

fn input_words(
    format: ApiFormat,
    body: &serde_json::Map<String, Value>,
) -> Result<u64, &'static str> {
    messages(format, body)
        .iter()
        .try_fold(0u64, |total, (_, text)| {
            let count = u64::try_from(text.split_whitespace().count())
                .map_err(|_| "mock input-token count does not fit u64")?;
            total
                .checked_add(count)
                .ok_or("mock input-token total overflows u64")
        })
}

/// Deterministic fake output from the prompt: a prefix marker + echo of the words of the last
/// user message, one word per delta, truncated to `count` (mock model).
fn derive_words(prompt: &str, count: u64) -> Vec<String> {
    let mut seed: Vec<String> = vec!["mock-reply: ".to_string()];
    for word in prompt.split_whitespace() {
        seed.push(format!("{word} "));
    }
    if seed.len() == 1 {
        // Empty prompt -- still yield something deterministic.
        seed.push("(empty) ".to_string());
    }
    (0..count)
        .map(|index| seed[(index % seed.len() as u64) as usize].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_format::{inspect_body, parse_frame, Frame, RequestBody, SseFrames};

    fn request(format: ApiFormat, body: Value) -> ProviderRequest {
        ProviderRequest {
            format,
            body: RequestBody::parse(&serde_json::to_vec(&body).unwrap()).unwrap(),
            headers: Vec::new(),
        }
    }

    async fn events(
        request: &ProviderRequest,
        count: u64,
        answer_as: Option<&str>,
    ) -> Vec<UpstreamEvent> {
        let (tx, mut rx) = mpsc::channel(256);
        run(request, count, answer_as, tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event.unwrap());
        }
        events
    }

    /// What a client of `format` reads from the mock's bytes: text, the model it names, and whether
    /// the response reached its final event.
    fn read(
        format: ApiFormat,
        stream: bool,
        events: &[UpstreamEvent],
    ) -> (String, Option<String>, bool) {
        let bytes: Vec<u8> = events
            .iter()
            .filter_map(|event| match event {
                UpstreamEvent::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        if !stream {
            let facts = inspect_body(format, &serde_json::from_slice(&bytes).unwrap());
            return (facts.text, facts.model, facts.done);
        }
        let mut frames = SseFrames::default();
        let (mut text, mut model, mut done) = (String::new(), None, false);
        for frame in frames.push(&bytes).unwrap() {
            match parse_frame(format, &frame.text).unwrap() {
                Frame::Done => done = true,
                Frame::Event { facts, .. } => {
                    text.push_str(&facts.text);
                    model = model.or(facts.model);
                    done |= facts.done;
                }
                Frame::Empty => {}
            }
        }
        assert!(frames.take_rest().is_empty());
        (text, model, done)
    }

    #[tokio::test]
    async fn the_mock_answers_every_format_streamed_or_whole() {
        for (format, body) in [
            (
                ApiFormat::ChatCompletions,
                json!({"model":"m","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"say two words"}]}),
            ),
            (
                ApiFormat::Responses,
                json!({"model":"m","instructions":"be brief","input":[{"role":"user","content":[{"type":"input_text","text":"say two words"}]}]}),
            ),
            (
                ApiFormat::Messages,
                json!({"model":"m","system":"be brief","messages":[{"role":"user","content":[{"type":"text","text":"say two words"}]}]}),
            ),
        ] {
            for stream in [true, false] {
                let mut body = body.clone();
                body["stream"] = stream.into();
                let events = events(&request(format, body), 4, None).await;
                let UpstreamEvent::Head(head) = &events[0] else {
                    panic!("{format:?}: the head comes first");
                };
                assert_eq!(head.status, 200);
                let (text, model, done) = read(format, stream, &events);
                assert_eq!(
                    text, "mock-reply: say two words ",
                    "{format:?} stream={stream}"
                );
                assert_eq!(model.as_deref(), Some("m"), "{format:?} stream={stream}");
                assert!(done, "{format:?} stream={stream}");
                let UpstreamEvent::Usage(usage) = events.last().unwrap() else {
                    panic!("{format:?}: the usage record comes last");
                };
                assert_eq!(
                    (usage.input_tokens, usage.output_tokens),
                    (5, 4),
                    "{format:?}: system plus user words in, four words out"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_mock_honours_the_requested_limit_its_count_and_its_identity() {
        let body = |limit: Value| json!({"model":"requested","messages":[{"role":"user","content":"one two three"}],"max_tokens":limit,"stream":true});
        let limited = events(
            &request(ApiFormat::ChatCompletions, body(json!(2))),
            8,
            None,
        )
        .await;
        let UpstreamEvent::Usage(usage) = limited.last().unwrap() else {
            panic!()
        };
        assert_eq!(usage.output_tokens, 2);
        let counted = events(
            &request(ApiFormat::ChatCompletions, body(json!(100))),
            3,
            None,
        )
        .await;
        let UpstreamEvent::Usage(usage) = counted.last().unwrap() else {
            panic!()
        };
        assert_eq!(usage.output_tokens, 3);

        let claimed = events(
            &request(ApiFormat::ChatCompletions, body(json!(2))),
            8,
            Some("Qwen/Qwen3-32B"),
        )
        .await;
        assert_eq!(
            read(ApiFormat::ChatCompletions, true, &claimed)
                .1
                .as_deref(),
            Some("Qwen/Qwen3-32B")
        );
        let substituted = events(
            &request(
                ApiFormat::ChatCompletions,
                json!({"model":"requested","messages":[{"role":"user","content":"DEXDO_FIXTURE_SUBSTITUTE"}],"stream":true}),
            ),
            2,
            Some("Qwen/Qwen3-32B"),
        )
        .await;
        assert_eq!(
            read(ApiFormat::ChatCompletions, true, &substituted)
                .1
                .as_deref(),
            Some(SUBSTITUTED_MODEL)
        );
    }
}
