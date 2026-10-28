//! One consumer request to the provider and its response back, unchanged.

//! The provider's status, headers and bytes are relayed as they arrive. Each streamed frame is read
//! before it is forwarded, so the served-model check and the billing rules run ahead of the output
//! they judge. Provider errors are relayed as received, except a refused seller credential
//! (`401`/`403`). An answer whose headers, frames or body name the seller's key is never relayed:
//! like a refused key, it stays the seller's own final error.

use super::meter::UsageMeter;
use super::{send_event, ProviderRequest, UpstreamEvent, UpstreamResult};
use crate::api_format::{forwarded_response_headers, inspect_body, parse_frame, Frame, SseFrames};
use dexdo_core::params::{
    SellerLivenessParams, UPSTREAM_ERROR_KEY_ECHO_PREFIX_CHARS, UPSTREAM_RESPONSE_BODY_MAX_BYTES,
    UPSTREAM_UNSTREAMED_RESPONSE_TIMEOUT,
};
use dexdo_proto::{Header, ResponseHead};
use futures::StreamExt;
use tokio::sync::mpsc;
use tonic::Status;

/// A served-model verdict: `Some(reason)` refuses the model the provider says answered.
pub(super) type ServedModelCheck<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

pub(super) struct ProviderCall<'a> {
    pub url: String,
    /// Seller credentials; the client's own never reach this request.
    pub credentials: Vec<(&'static str, String)>,
    pub request: &'a ProviderRequest,
    pub served_model: Option<ServedModelCheck<'a>>,
}

/// A transport that produced no response, or whose body decoder died, did not answer. Request
/// construction and redirect policy are local answers.
fn retryable_transport_error(error: &reqwest::Error) -> bool {
    !error.is_builder()
        && !error.is_redirect()
        && (error.is_connect()
            || error.is_timeout()
            || error.is_request()
            || error.is_body()
            || error.is_decode())
}

/// A provider HTTP refusal as the seller's own error. Throttling and a momentarily unavailable
/// provider are marked retryable.
pub(super) fn http_failure(status: reqwest::StatusCode) -> Status {
    let failure = Status::unavailable(format!("upstream HTTP {status}"));
    if matches!(status.as_u16(), 429 | 502 | 503) {
        dexdo_proto::retryable_upstream_status(failure)
    } else {
        failure
    }
}

fn no_output_before_deadline() -> Status {
    dexdo_proto::retryable_upstream_status(Status::deadline_exceeded(
        "upstream produced no output before the seller deadline",
    ))
}

fn read_failure(error: reqwest::Error, answered: bool) -> Status {
    let retryable = retryable_transport_error(&error);
    let status = Status::unavailable(format!("upstream read failed: {}", error.without_url()));
    if !answered && retryable {
        dexdo_proto::retryable_upstream_status(status)
    } else {
        status
    }
}

#[allow(clippy::result_large_err)]
fn request_headers(call: &ProviderCall<'_>) -> Result<reqwest::header::HeaderMap, Status> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
    let mut headers = HeaderMap::new();
    for header in &call.request.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| Status::invalid_argument("request header name is not valid HTTP"))?;
        let value = HeaderValue::from_bytes(&header.value)
            .map_err(|_| Status::invalid_argument("request header value is not valid HTTP"))?;
        headers.append(name, value);
    }
    for (name, value) in &call.credentials {
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| Status::failed_precondition("upstream key is not a valid header value"))?;
        value.set_sensitive(true);
        headers.insert(*name, value);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

/// The provider error body names the seller's key: the key itself or, for a long key, its first
/// [`UPSTREAM_ERROR_KEY_ECHO_PREFIX_CHARS`] characters, so a cut-short echo counts too.
fn echoes_seller_key(body: &[u8], credentials: &[(&'static str, String)]) -> bool {
    credentials.iter().any(|(_, value)| {
        let key = value.strip_prefix("Bearer ").unwrap_or(value).trim();
        let echo = key
            .char_indices()
            .nth(UPSTREAM_ERROR_KEY_ECHO_PREFIX_CHARS)
            .map_or(key, |(end, _)| &key[..end]);
        !echo.is_empty()
            && body
                .windows(echo.len())
                .any(|window| window == echo.as_bytes())
    })
}

/// The response head names the seller's key in a header name or value.
fn head_names_seller_key(head: &ResponseHead, credentials: &[(&'static str, String)]) -> bool {
    head.headers.iter().any(|header| {
        echoes_seller_key(header.name.as_bytes(), credentials)
            || echoes_seller_key(&header.value, credentials)
    })
}

/// The seller's own error for a provider answer that names the seller's key: nothing of that answer
/// reaches the buyer, and the error is final, as for a refused key.
fn key_named_failure() -> Status {
    Status::unavailable("the provider's answer named the seller's key and is not relayed")
}

/// A frame without `data:` that is a whole JSON object stating an error: the provider answered
/// with an error object instead of a stream. Returns the provider's reason.
fn unframed_error(text: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(text.trim()).ok()?;
    crate::api_format::stated_error(&value)?;
    Some(crate::api_format::error_detail(&value).unwrap_or_default())
}

fn response_head(response: &reqwest::Response) -> ResponseHead {
    ResponseHead {
        status: u32::from(response.status().as_u16()),
        headers: forwarded_response_headers(
            response
                .headers()
                .iter()
                .map(|(name, value)| Header {
                    name: name.as_str().to_string(),
                    value: value.as_bytes().to_vec(),
                })
                .collect(),
        ),
    }
}

#[allow(clippy::result_large_err)]
pub(super) async fn forward(
    call: ProviderCall<'_>,
    tx: &mpsc::Sender<UpstreamResult>,
) -> Result<(), Status> {
    let stream = call.request.body.stream();
    let deadline = tokio::time::Instant::now()
        + if stream {
            SellerLivenessParams::canonical().health_cycle_timeout
        } else {
            UPSTREAM_UNSTREAMED_RESPONSE_TIMEOUT
        };
    // A provider redirect is never followed: it would carry the seller's key to another host.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Status::internal("provider HTTP client could not be built"))?;
    let send = client
        .post(&call.url)
        .headers(request_headers(&call)?)
        .body(call.request.body.to_vec())
        .send();
    crate::telemetry::attempt(None);
    let response = tokio::select! {
        _ = tx.closed() => return Ok(()),
        response = tokio::time::timeout_at(deadline, send) => response,
    };
    let response = match response {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            let retryable = retryable_transport_error(&error);
            let status =
                Status::unavailable(format!("upstream connect failed: {}", error.without_url()));
            return Err(if retryable {
                dexdo_proto::retryable_upstream_status(status)
            } else {
                status
            });
        }
        Err(_) => return Err(no_output_before_deadline()),
    };
    crate::telemetry::attempt(Some(response.status().as_u16()));
    // A refused seller key, and a redirect the seller does not follow, stay the seller's own
    // error: the provider's body and `Location` never reach the buyer.
    if matches!(response.status().as_u16(), 401 | 403) || response.status().is_redirection() {
        return Err(http_failure(response.status()));
    }
    let head = response_head(&response);
    if head_names_seller_key(&head, &call.credentials) {
        return Err(key_named_failure());
    }
    if !response.status().is_success() {
        // The provider's error answer, under the same deadline and size limit as a whole answer.
        // It is read whole first: one that names the seller's key never leaves the seller, and the
        // buyer gets the seller's own error instead.
        let mut body = response.bytes_stream();
        let mut error = Vec::new();
        loop {
            let item = tokio::select! {
                _ = tx.closed() => return Ok(()),
                item = tokio::time::timeout_at(deadline, body.next()) => {
                    item.map_err(|_| no_output_before_deadline())?
                }
            };
            let Some(item) = item else { break };
            let bytes = item.map_err(|error| read_failure(error, true))?;
            error.extend_from_slice(&bytes);
            if error.len() > UPSTREAM_RESPONSE_BODY_MAX_BYTES {
                return Err(Status::resource_exhausted(
                    "provider error answer exceeds the seller's response size limit",
                ));
            }
        }
        if echoes_seller_key(&error, &call.credentials) {
            return Err(key_named_failure());
        }
        if send_event(tx, Ok(UpstreamEvent::Head(head))).await.is_err() {
            return Ok(());
        }
        if !error.is_empty() {
            let data = UpstreamEvent::Data {
                bytes: error,
                output: false,
            };
            let _ = send_event(tx, Ok(data)).await;
        }
        return Ok(());
    }
    if send_event(tx, Ok(UpstreamEvent::Head(head))).await.is_err() {
        return Ok(());
    }
    if stream {
        relay_stream(&call, response, deadline, tx).await
    } else {
        relay_body(&call, response, deadline, tx).await
    }
}

#[allow(clippy::result_large_err)]
async fn relay_stream(
    call: &ProviderCall<'_>,
    response: reqwest::Response,
    deadline: tokio::time::Instant,
    tx: &mpsc::Sender<UpstreamResult>,
) -> Result<(), Status> {
    let format = call.request.format;
    let mut body = response.bytes_stream();
    let mut frames = SseFrames::default();
    let mut meter = UsageMeter::new(format);
    let mut saw_output = false;
    let mut stated_error = false;
    let mut model_settled = call.served_model.is_none();
    let finished = 'stream: loop {
        let item = if saw_output {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                item = body.next() => item,
            }
        } else {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                item = tokio::time::timeout_at(deadline, body.next()) => {
                    item.map_err(|_| no_output_before_deadline())?
                }
            }
        };
        // At end of body an unterminated last frame is closed and judged like any other: nothing
        // reaches the buyer without the model check and output classification.
        let (bytes, eof) = match item {
            Some(item) => (
                item.map_err(|error| read_failure(error, saw_output || stated_error))?
                    .to_vec(),
                false,
            ),
            None if frames.is_empty() => break false,
            None => (b"\n\n".to_vec(), true),
        };
        for frame in frames.push(&bytes).map_err(Status::data_loss)? {
            if echoes_seller_key(&frame.bytes, &call.credentials) {
                return Err(key_named_failure());
            }
            let mut output = false;
            let mut done = false;
            match parse_frame(format, &frame.text).map_err(Status::data_loss)? {
                Frame::Empty => {
                    // The provider answered with an error object, not a stream: that is its
                    // answer, final and not retryable, and it is no stream frame to relay.
                    if let Some(reason) = unframed_error(&frame.text) {
                        return Err(Status::failed_precondition(format!(
                            "provider error object instead of a stream: {reason}"
                        )));
                    }
                }
                Frame::Done => done = true,
                Frame::Event { value, facts } => {
                    if !model_settled {
                        if let Some(reported) = facts.model.as_deref() {
                            model_settled = true;
                            check_served_model(call, reported, saw_output)?;
                        }
                    }
                    meter.observe(&value, &facts)?;
                    output = facts.has_output();
                    stated_error |= facts.error.is_some();
                    done = facts.done;
                }
            }
            let data = UpstreamEvent::Data {
                bytes: frame.bytes,
                output,
            };
            if send_event(tx, Ok(data)).await.is_err() {
                return Ok(());
            }
            saw_output |= output;
            if done {
                break 'stream true;
            }
        }
        if eof {
            break false;
        }
    };
    if finished {
        if let Some(usage) = meter.finish(saw_output)? {
            let _ = send_event(tx, Ok(UpstreamEvent::Usage(usage))).await;
        }
        return Ok(());
    }
    let status = Status::data_loss("provider stream ended before its final event");
    Err(if saw_output || stated_error {
        status
    } else {
        dexdo_proto::retryable_upstream_status(status)
    })
}

#[allow(clippy::result_large_err)]
async fn relay_body(
    call: &ProviderCall<'_>,
    response: reqwest::Response,
    deadline: tokio::time::Instant,
    tx: &mpsc::Sender<UpstreamResult>,
) -> Result<(), Status> {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        let item = tokio::select! {
            _ = tx.closed() => return Ok(()),
            item = tokio::time::timeout_at(deadline, body.next()) => {
                item.map_err(|_| no_output_before_deadline())?
            }
        };
        let Some(item) = item else { break };
        bytes.extend_from_slice(&item.map_err(|error| read_failure(error, false))?);
        if bytes.len() > UPSTREAM_RESPONSE_BODY_MAX_BYTES {
            return Err(Status::resource_exhausted(
                "provider response exceeds the seller's response size limit",
            ));
        }
    }
    // An empty answer is the stream that ended before its final event, in whole form.
    if bytes.is_empty() {
        return Err(dexdo_proto::retryable_upstream_status(Status::data_loss(
            "provider response ended before its answer",
        )));
    }
    if echoes_seller_key(&bytes, &call.credentials) {
        return Err(key_named_failure());
    }
    let value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|error| Status::data_loss(format!("provider response is not JSON: {error}")))?;
    let facts = inspect_body(call.request.format, &value);
    if let Some(reported) = facts.model.as_deref() {
        check_served_model(call, reported, false)?;
    }
    let usage = UsageMeter::body(call.request.format, &value, &facts)?;
    let data = UpstreamEvent::Data {
        bytes,
        output: facts.has_output(),
    };
    if send_event(tx, Ok(data)).await.is_err() {
        return Ok(());
    }
    if let Some(usage) = usage {
        let _ = send_event(tx, Ok(UpstreamEvent::Usage(usage))).await;
    }
    Ok(())
}

/// E2E-ADV-02: refuse a provider that answers as another model, but only before output was
/// forwarded. Afterwards a refusal would strand both sides' money: the divergence is logged
/// and the response is carried to its terminal.
#[allow(clippy::result_large_err)]
fn check_served_model(
    call: &ProviderCall<'_>,
    reported: &str,
    saw_output: bool,
) -> Result<(), Status> {
    let Some(mismatch) = call.served_model.and_then(|check| check(reported)) else {
        return Ok(());
    };
    if !saw_output {
        return Err(Status::failed_precondition(mismatch));
    }
    tracing::error!(
        "{mismatch}; output was already delivered, so the response is carried to its terminal \
         instead of stranding the buyer's paid capacity"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a forwarded header value cannot smuggle another header into the provider request.
    /// The request is refused before any connection is attempted.
    #[tokio::test]
    async fn a_header_value_cannot_inject_a_provider_header() {
        let request = ProviderRequest {
            format: crate::api_format::ApiFormat::ChatCompletions,
            body: crate::api_format::RequestBody::parse(br#"{"model":"m"}"#).unwrap(),
            headers: vec![Header {
                name: "session_id".into(),
                value: b"session\r\nAuthorization: attacker".to_vec(),
            }],
        };
        let call = ProviderCall {
            // Nothing listens here: reaching it would fail as a connection error instead.
            url: "http://127.0.0.1:9/v1/chat/completions".into(),
            credentials: vec![("authorization", "Bearer seller-key".into())],
            request: &request,
            served_model: None,
        };
        let (tx, _rx) = mpsc::channel(4);
        let status = forward(call, &tx)
            .await
            .expect_err("the request is refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status:?}");
        assert!(status.message().contains("not valid HTTP"), "{status:?}");
    }

    #[test]
    fn a_provider_error_that_names_the_sellers_key_is_recognized() {
        let key = "sk-proj-0123456789abcdefghijklmnopqrstuvwxyz";
        let bearer = [("authorization", format!("Bearer {key}"))];
        let cut_short = &key[..UPSTREAM_ERROR_KEY_ECHO_PREFIX_CHARS];
        let shorter = &key[..UPSTREAM_ERROR_KEY_ECHO_PREFIX_CHARS - 1];
        for (body, named) in [
            (format!(r#"{{"error":"bad key {key}"}}"#), true),
            (format!(r#"{{"error":"bad key {cut_short}..."}}"#), true),
            (format!(r#"{{"error":"bad key {shorter}..."}}"#), false),
            (r#"{"error":"overloaded"}"#.to_string(), false),
        ] {
            assert_eq!(echoes_seller_key(body.as_bytes(), &bearer), named, "{body}");
        }
        let anthropic = [("x-api-key", "short-key".to_string())];
        assert!(echoes_seller_key(b"bad short-key", &anthropic));
        assert!(!echoes_seller_key(b"bad short", &anthropic));
    }
}
