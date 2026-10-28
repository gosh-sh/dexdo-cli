//! Native Anthropic Messages API seller upstream: requests are relayed to the provider as received,
//! and its response returns as produced.

use super::forward::{forward, ProviderCall};
use super::{probe_body, send_event, ProviderRequest, UpstreamResult};
use crate::api_format::ApiFormat;
use crate::seller::models::ModelConfig;
use tokio::sync::mpsc;
use tonic::Status;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    pub base_url: String,
    /// Upstream model id (`served_model`): sent to the provider in place of the deal model.
    pub model: String,
    /// The market model a request must name.
    pub frame_model: String,
    pub api_key_env: String,
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
        }
    }
}

fn endpoint(cfg: &AnthropicConfig) -> String {
    format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'))
}

/// The seller's own readiness request: a streamed Messages call.
pub(super) fn probe_request(
    cfg: &AnthropicConfig,
    prompt: &str,
    max_tokens: Option<u32>,
) -> ProviderRequest {
    let mut body = serde_json::json!({
        "model": cfg.model,
        "messages": [{ "role": "user", "content": prompt }],
        "stream": true,
    });
    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = max_tokens.into();
    }
    ProviderRequest {
        format: ApiFormat::Messages,
        body: probe_body(body),
        headers: crate::api_format::own_request_headers(ApiFormat::Messages),
    }
}

pub async fn run(
    cfg: &AnthropicConfig,
    request: &ProviderRequest,
    tx: mpsc::Sender<UpstreamResult>,
) {
    let Some(key) = std::env::var(&cfg.api_key_env)
        .ok()
        .filter(|key| !key.is_empty())
    else {
        let _ = send_event(
            &tx,
            Err(Status::failed_precondition(format!(
                "real upstream key absent ({})",
                cfg.api_key_env
            ))),
        )
        .await;
        return;
    };
    let call = ProviderCall {
        url: endpoint(cfg),
        credentials: vec![("x-api-key", key)],
        request,
        served_model: None,
    };
    if let Err(status) = forward(call, &tx).await {
        let _ = send_event(&tx, Err(status)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_format::RequestBody;
    use crate::seller::upstream::UpstreamEvent;
    use dexdo_proto::{BillingUsage, Header};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One provider answer; writes are delivered as separate reads. Returns the raw request.
    async fn answer(
        status: &'static str,
        content_type: &'static str,
        writes: Vec<Vec<u8>>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut next = [0_u8; 4096];
            loop {
                let read = socket.read(&mut next).await.unwrap();
                assert_ne!(read, 0, "the provider received a truncated request");
                request.extend_from_slice(&next[..read]);
                let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let head = std::str::from_utf8(&request[..end]).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            let length: usize = writes.iter().map(Vec::len).sum();
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            for write in writes {
                socket.write_all(&write).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}"), task)
    }

    fn config(base_url: String) -> AnthropicConfig {
        AnthropicConfig {
            base_url,
            model: "claude-provider-model".to_string(),
            frame_model: "anthropic--claude--x".to_string(),
            api_key_env: "PATH".to_string(),
        }
    }

    fn messages(stream: bool, headers: Vec<Header>) -> ProviderRequest {
        let mut body = RequestBody::parse(
            &serde_json::to_vec(&serde_json::json!({
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": stream,
            }))
            .unwrap(),
        )
        .unwrap();
        body.set_model("claude-provider-model");
        ProviderRequest {
            format: ApiFormat::Messages,
            body,
            headers,
        }
    }

    async fn relay_writes(
        writes: Vec<Vec<u8>>,
        request: ProviderRequest,
    ) -> (Vec<crate::seller::upstream::UpstreamResult>, String) {
        let (base_url, provider) = answer("200 OK", "text/event-stream", writes).await;
        let (tx, mut rx) = mpsc::channel(256);
        run(&config(base_url), &request, tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (events, provider.await.unwrap())
    }

    async fn relay(sse: &str) -> Vec<crate::seller::upstream::UpstreamResult> {
        relay_writes(vec![sse.as_bytes().to_vec()], messages(true, Vec::new()))
            .await
            .0
    }

    fn forwarded(events: &[crate::seller::upstream::UpstreamResult]) -> Vec<u8> {
        events
            .iter()
            .filter_map(|event| match event {
                Ok(UpstreamEvent::Data { bytes, .. }) => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn billed(events: &[crate::seller::upstream::UpstreamResult]) -> Option<BillingUsage> {
        events.iter().find_map(|event| match event {
            Ok(UpstreamEvent::Usage(usage)) => Some(*usage),
            _ => None,
        })
    }

    fn error(events: &[crate::seller::upstream::UpstreamResult]) -> Option<&Status> {
        events.iter().find_map(|event| event.as_ref().err())
    }

    fn start(input: &str) -> String {
        format!("event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"model\":\"claude-provider-model\",\"usage\":{input}}}}}\n\n")
    }

    fn text(delta: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":delta}})
        )
    }

    fn output_usage(usage: &str) -> String {
        format!("event: message_delta\ndata: {{\"type\":\"message_delta\",\"usage\":{usage}}}\n\n")
    }

    const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    #[tokio::test]
    async fn the_request_carries_the_seller_key_and_only_the_clients_api_version() {
        for (client_version, expected) in [(None, 0), (Some("2099-01-01"), 1)] {
            let mut headers = vec![Header {
                name: "anthropic-beta".into(),
                value: b"tools-2024".to_vec(),
            }];
            if let Some(version) = client_version {
                headers.push(Header {
                    name: "anthropic-version".into(),
                    value: version.as_bytes().to_vec(),
                });
            }
            let sse = format!(
                "{}{}{}{STOP}",
                start(r#"{"input_tokens":1,"output_tokens":0}"#),
                text("hi"),
                output_usage(r#"{"output_tokens":1}"#)
            );
            let (events, request) =
                relay_writes(vec![sse.into_bytes()], messages(true, headers)).await;
            assert!(error(&events).is_none());
            let request = request.to_ascii_lowercase();
            assert!(request.starts_with("post /v1/messages "), "{request}");
            assert!(request.contains("x-api-key: "), "{request}");
            assert!(!request.contains("authorization:"), "{request}");
            assert!(request.contains("anthropic-beta: tools-2024"), "{request}");
            // The seller adds no API version of its own: the client's, if any, is the only one.
            assert_eq!(
                request.matches("anthropic-version: ").count(),
                expected,
                "{request}"
            );
            if let Some(version) = client_version {
                assert!(
                    request.contains(&format!("anthropic-version: {version}")),
                    "{request}"
                );
            }
        }
    }

    #[test]
    fn the_sellers_own_readiness_check_states_the_api_version() {
        let cfg = AnthropicConfig {
            base_url: DEFAULT_BASE_URL.into(),
            model: "claude-provider-model".into(),
            frame_model: "claude-provider-model".into(),
            api_key_env: "UNUSED".into(),
        };
        let request = probe_request(&cfg, "ping", Some(1));
        assert_eq!(
            request.headers,
            crate::api_format::own_request_headers(ApiFormat::Messages)
        );
        assert_eq!(
            request.headers[0].value,
            crate::api_format::ANTHROPIC_VERSION.as_bytes()
        );
    }

    #[tokio::test]
    async fn streams_the_provider_bytes_and_bills_its_cumulative_usage_with_cache_subsets() {
        let sse = format!(
            "{}{}{}{}{STOP}",
            start(
                r#"{"input_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":3,"output_tokens":1}"#
            ),
            text("Hello"),
            text(" world"),
            output_usage(r#"{"output_tokens":5}"#)
        );
        let events = relay(&sse).await;
        assert!(error(&events).is_none(), "{:?}", error(&events));
        assert_eq!(forwarded(&events), sse.as_bytes());
        let usage = billed(&events).unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (9, 5, 14)
        );
        assert_eq!((usage.cache_read_tokens, usage.cache_write_tokens), (4, 3));
    }

    #[tokio::test]
    async fn buffers_split_unicode_bytes_until_the_frame_is_complete() {
        let delta = "\u{41f}\u{440}\u{438}";
        let sse = format!(
            "{}{}{}{STOP}",
            start(r#"{"input_tokens":1,"output_tokens":0}"#),
            text(delta),
            output_usage(r#"{"output_tokens":3}"#)
        );
        let bytes = sse.clone().into_bytes();
        let split = (1..bytes.len())
            .find(|at| !sse.is_char_boundary(*at))
            .unwrap();
        let (events, _) = relay_writes(
            vec![bytes[..split].to_vec(), bytes[split..].to_vec()],
            messages(true, Vec::new()),
        )
        .await;
        assert!(error(&events).is_none());
        assert_eq!(forwarded(&events), bytes);
        assert_eq!(billed(&events).unwrap().output_tokens, 3);
    }

    /// E2E-ROW: E2E-UPS-18/L0
    #[tokio::test]
    async fn malformed_terminal_frame_fails_at_eof() {
        let complete = format!(
            "{}{}",
            start(r#"{"input_tokens":1,"output_tokens":0}"#),
            text("x")
        );
        let sse = format!("{complete}event: message_stop\ndata: {{\"type\":");
        let events = relay(&sse).await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
        assert_eq!(
            forwarded(&events),
            complete.as_bytes(),
            "the malformed unfinished tail is judged like any frame and never relayed"
        );
    }

    #[tokio::test]
    async fn missing_final_usage_fails_without_accounting_frames() {
        let sse = format!(
            "{}{}{STOP}",
            start(r#"{"input_tokens":1,"output_tokens":0}"#),
            text("x")
        );
        let events = relay(&sse).await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
    }

    #[tokio::test]
    async fn positive_initial_usage_without_final_usage_fails_without_accounting_frames() {
        let sse = format!(
            "{}{}{STOP}",
            start(r#"{"input_tokens":1,"output_tokens":4}"#),
            text("x")
        );
        let events = relay(&sse).await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
    }

    /// A provider-run tool adds its results to the input: the final cumulative counts are the bill.
    #[tokio::test]
    async fn a_provider_run_tool_is_billed_at_the_final_cumulative_input() {
        let sse = format!(
            "{}{}{}{STOP}",
            start(
                r#"{"input_tokens":2679,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":3}"#
            ),
            text("found"),
            output_usage(
                r#"{"input_tokens":10682,"cache_creation_input_tokens":2,"cache_read_input_tokens":5,"output_tokens":510,"server_tool_use":{"web_search_requests":1}}"#
            )
        );
        let events = relay(&sse).await;
        assert!(error(&events).is_none(), "{:?}", error(&events));
        assert_eq!(forwarded(&events), sse.as_bytes());
        let usage = billed(&events).unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens),
            (10682 + 5 + 2, 510)
        );
        assert_eq!((usage.cache_read_tokens, usage.cache_write_tokens), (5, 2));
    }

    /// A cumulative input or cache count that falls is a contradiction and bills nothing.
    #[tokio::test]
    async fn a_decreasing_cumulative_input_or_cache_count_bills_nothing() {
        for (initial, last) in [
            (
                r#"{"input_tokens":5,"output_tokens":0}"#,
                r#"{"input_tokens":4,"output_tokens":1}"#,
            ),
            (
                r#"{"input_tokens":5,"cache_read_input_tokens":3,"output_tokens":0}"#,
                r#"{"cache_read_input_tokens":2,"output_tokens":1}"#,
            ),
            (
                r#"{"input_tokens":5,"cache_creation_input_tokens":3,"output_tokens":0}"#,
                r#"{"cache_creation_input_tokens":2,"output_tokens":1}"#,
            ),
        ] {
            let sse = format!(
                "{}{}{}{STOP}",
                start(initial),
                text("output"),
                output_usage(last)
            );
            let events = relay(&sse).await;
            assert_eq!(
                error(&events).unwrap().code(),
                tonic::Code::DataLoss,
                "{last}"
            );
            assert!(billed(&events).is_none(), "{last}");
        }
        // A fall after growth is measured against the last stated count.
        let sse = format!(
            "{}{}{}{}{STOP}",
            start(r#"{"input_tokens":5,"output_tokens":0}"#),
            text("output"),
            output_usage(r#"{"input_tokens":10,"output_tokens":1}"#),
            output_usage(r#"{"input_tokens":8,"output_tokens":2}"#)
        );
        let events = relay(&sse).await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
    }

    /// E2E-ROW: E2E-UPS-21/L0
    #[tokio::test]
    async fn decreasing_cumulative_usage_fails_without_partial_accounting() {
        let sse = format!(
            "{}{}{}{}{STOP}",
            start(r#"{"input_tokens":3,"output_tokens":0}"#),
            text("output"),
            output_usage(r#"{"output_tokens":5}"#),
            output_usage(r#"{"output_tokens":4}"#)
        );
        let events = relay(&sse).await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(
            billed(&events).is_none(),
            "a contradictory cumulative sequence must not advance monetary usage"
        );
    }

    /// E2E-ROW: E2E-UPS-22/L0
    #[tokio::test]
    async fn overflowing_usage_value_fails_without_accounting() {
        for initial in [
            r#"{"input_tokens":18446744073709551615,"cache_read_input_tokens":1,"output_tokens":0}"#,
            r#"{"input_tokens":184467440737095516160,"output_tokens":0}"#,
            r#"{"input_tokens":null,"output_tokens":0}"#,
            r#"{"input_tokens":"3","output_tokens":0}"#,
        ] {
            let sse = format!(
                "{}{}{}{STOP}",
                start(initial),
                text("output"),
                output_usage(r#"{"output_tokens":1}"#)
            );
            let events = relay(&sse).await;
            assert!(error(&events).is_some(), "{initial}");
            assert!(billed(&events).is_none(), "{initial}");
        }
    }

    #[tokio::test]
    async fn content_before_message_start_or_a_repeated_start_bills_nothing() {
        for sse in [
            format!(
                "{}{}{}{STOP}",
                text("x"),
                start(r#"{"input_tokens":1}"#),
                output_usage(r#"{"output_tokens":1}"#)
            ),
            format!(
                "{}{}{}{}{STOP}",
                start(r#"{"input_tokens":1}"#),
                text("x"),
                start(r#"{"input_tokens":1}"#),
                output_usage(r#"{"output_tokens":1}"#)
            ),
        ] {
            let events = relay(&sse).await;
            assert_eq!(
                error(&events).unwrap().code(),
                tonic::Code::DataLoss,
                "{sse}"
            );
            assert!(billed(&events).is_none(), "{sse}");
        }
    }

    #[tokio::test]
    async fn thinking_is_billed_output_and_an_in_band_error_ends_unbilled() {
        let thinking = format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}})
        );
        let sse = format!(
            "{}{thinking}{}{STOP}",
            start(r#"{"input_tokens":1,"output_tokens":0}"#),
            output_usage(r#"{"output_tokens":2}"#)
        );
        let events = relay(&sse).await;
        assert!(error(&events).is_none());
        assert_eq!(billed(&events).unwrap().output_tokens, 2);

        let failed = format!(
            "{}{}event: error\ndata: {}\n\n",
            start(r#"{"input_tokens":1,"output_tokens":0}"#),
            text("x"),
            serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})
        );
        let events = relay(&failed).await;
        assert_eq!(forwarded(&events), failed.as_bytes());
        assert!(error(&events).is_some());
        assert!(billed(&events).is_none());
    }

    #[tokio::test]
    async fn an_unstreamed_message_is_relayed_whole_and_billed() {
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-provider-model",
            "content": [{"type": "text", "text": "whole"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "cache_read_input_tokens": 2, "output_tokens": 4},
        })
        .to_string();
        let (base_url, provider) = answer(
            "200 OK",
            "application/json",
            vec![body.clone().into_bytes()],
        )
        .await;
        let (tx, mut rx) = mpsc::channel(16);
        run(&config(base_url), &messages(false, Vec::new()), tx).await;
        provider.await.unwrap();
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(error(&events).is_none());
        assert_eq!(forwarded(&events), body.as_bytes());
        let usage = billed(&events).unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (5, 4));
        assert_eq!(usage.cache_read_tokens, 2);
    }
}
