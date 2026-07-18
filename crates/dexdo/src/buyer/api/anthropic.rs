//! Optional Anthropic-compatible endpoint: `POST /v1/messages`,
//! a **local transcode** to the same `CanonRequest`(OpenAI shape) and back `CanonChunk` ->
//! Anthropic-SSE, for Anthropic-native clients. The transcode is off-chain, on the
//! buyer side: the wire(gRPC) and the canonical format are not touched.

use crate::buyer::api::stream::{CanonStreamDriver, CanonStreamNext};
use crate::buyer::api::{
    handle_stream_error_policy, request_token_limit, ApiDeal, ApiState, ConsumerRequestGuard,
    DeadGatewayAction, DealInitError,
};
use crate::buyer::render::{self, AnthropicRequest};
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dexdo_proto::CanonChunk;
use futures::Stream;
use http::StatusCode;
use std::convert::Infallible;
use std::time::{SystemTime, UNIX_EPOCH};

/// `POST /v1/messages`(B20). Transcodes the request -> `CanonRequest`, opens the same
/// authorized TLS gRPC stream and re-renders `CanonChunk` into Anthropic-SSE
/// (`message_start` -> `content_block_*` -> `message_delta` -> `message_stop`).
pub async fn messages(
    State(state): State<ApiState>,
    Json(req): Json<AnthropicRequest>,
) -> Response {
    // The model is forced by the market(B2/B19) -- the same check as for the OpenAI path.
    if let Err(reason) = state.check_model(req.model.as_deref()) {
        return reject(StatusCode::BAD_REQUEST, &reason);
    }
    let deal = match state.current_deal().await {
        Ok(deal) => deal,
        Err(error) => return deal_init_rejection(&error),
    };
    let request_guard = deal.begin_request(message_started_secs());
    // Session-scoped lifecycle: no new request once the local deal is closed.
    if deal.session.is_closed() {
        return reject(StatusCode::GONE, "deal session closed; open a new session");
    }
    if deal.remaining_tokens() == 0 {
        return reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "active deal budget exhausted; waiting for renewal handover",
        );
    }
    if let Err(reason) = deal.session.ensure_open_for_serving().await {
        return reject(StatusCode::BAD_GATEWAY, &reason);
    }
    // one-per-deal content-identity gate(B8 + B7-full), run ONCE before the first paid stream -- the same
    // gate as the OpenAI path. The inline StreamVerifier only runs B5/B6 + the cheap declared-NAME B7; a seller
    // serving a cheaper model under the correct NAME is caught only here. On a bail the gate closes the deal and
    // attempts policy recovery; a transport error is not cached, so a later request retries.
    if let Err(reason) = deal
        .content_gate
        .ensure_verified(&state.buyer, &deal.route, &deal.session)
        .await
    {
        return reject(
            StatusCode::BAD_GATEWAY,
            &format!("model identity verification failed (content check): {reason}"),
        );
    }

    let stream = req.stream;
    let requested_max_tokens = req.max_tokens;
    let canon = render::anthropic_to_canon(req);
    let id = message_id();
    let model = state.frame_model.clone();

    let upstream = match state
        .buyer
        .open_canon_stream(
            &deal.route.handover,
            &deal.route.token_contract,
            canon.clone(),
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            if deal.session.dead_gateway_action() == DeadGatewayAction::RetryThenReclaim {
                tracing::warn!(
                    error = %e,
                    token_contract = %deal.route.token_contract,
                    "consumer API: upstream open failed; retrying once per dead_gateway=retry_then_reclaim"
                );
                match state
                    .buyer
                    .open_canon_stream(&deal.route.handover, &deal.route.token_contract, canon)
                    .await
                {
                    Ok(s) => s,
                    Err(second) => {
                        deal.session.settle_dead_gateway("dead-gateway").await;
                        return reject(
                            StatusCode::BAD_GATEWAY,
                            &format!("upstream open failed after retry: {second}"),
                        );
                    }
                }
            } else {
                deal.session.settle_dead_gateway("dead-gateway").await;
                return reject(
                    StatusCode::BAD_GATEWAY,
                    &format!("upstream open failed: {e}"),
                );
            }
        }
    };

    let max_tokens = request_token_limit(requested_max_tokens, deal.remaining_tokens());
    // Session-scoped: no per-request STOP -- the shared session settles once at session end / on a
    // verification-bail(as in the OpenAI path).
    if stream {
        sse_response(upstream, id, model, max_tokens, deal, request_guard).into_response()
    } else {
        aggregate_response(upstream, id, model, max_tokens, deal, request_guard)
            .await
            .into_response()
    }
}

/// Re-render the canonical stream to Anthropic-SSE(B20, R6). Accounting/verification happen before re-rendering.
fn sse_response(
    upstream: tonic::Streaming<CanonChunk>,
    id: String,
    model: String,
    max_tokens: u64,
    deal: ApiDeal,
    request_guard: ConsumerRequestGuard,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let sse = async_stream::stream! {
        let _request_guard = request_guard;
        let mut driver = CanonStreamDriver::new(upstream, model.clone(), max_tokens);
        let mut stream_error = None;
        yield Ok(event(render::anthropic_message_start(&id, &model)));
        yield Ok(event(render::anthropic_content_block_start()));
        loop {
            let chunk = match driver.next().await {
                CanonStreamNext::Chunk(c) => c,
                // upstream transport error -- do not pass it off as a clean `end_turn`.
                CanonStreamNext::Errored(e) => {
                    stream_error = Some(e);
                    break;
                }
                CanonStreamNext::Bailed | CanonStreamNext::End => break,
            };
            if !chunk.text.is_empty() {
                yield Ok(event(render::anthropic_content_block_delta(&chunk.text)));
            }
            let before = driver.received();
            if driver.account_rendered(&chunk) {
                deal.record_delivered(driver.received().saturating_sub(before));
                break; // request/deal token budget reached
            }
            deal.record_delivered(driver.received().saturating_sub(before));
        }
        // Session-scoped: completion / max_tokens / upstream-error do NOT STOP -- only a
        // verification-bail ends the session early(STOP + bail off). `errored` still drives stop_reason below.
        let bailed = driver.bailed();
        let received = driver.received();
        drop(driver);
        if bailed {
            deal.session.settle_verification_bail("verify-bail").await;
        } else if let Some(e) = &stream_error {
            handle_stream_error_policy(&deal, received, e).await;
        } else if received == 0 {
            deal.session.settle_empty_stream("empty-stream").await;
        }
        // stop_reason does NOT pass off a bail/error as an honest `end_turn` -- bail -> `refusal`,
        // transport error -> `error`, otherwise `end_turn`.
        let stop_reason = if bailed {
            "refusal"
        } else if stream_error.is_some() || received == 0 {
            "error"
        } else {
            "end_turn"
        };
        yield Ok(event(render::anthropic_content_block_stop()));
        yield Ok(event(render::anthropic_message_delta(stop_reason)));
        yield Ok(event(render::anthropic_message_stop()));
    };
    Sse::new(sse)
}

/// Build an axum SSE `Event` from `(name, JSON data)`(B20). A single source of truth for the
/// frame -- the HTTP layer adds `event:`/`data:`.
fn event((name, data): render::AnthropicEvent) -> Event {
    Event::default().event(name).data(data)
}

/// Non-streaming Anthropic response(B20): a single `message` JSON with aggregated text.
async fn aggregate_response(
    upstream: tonic::Streaming<CanonChunk>,
    id: String,
    model: String,
    max_tokens: u64,
    deal: ApiDeal,
    _request_guard: ConsumerRequestGuard,
) -> Response {
    let mut content = String::new();
    let mut driver = CanonStreamDriver::new(upstream, model.clone(), max_tokens);
    let mut stream_error = None;
    loop {
        let chunk = match driver.next().await {
            CanonStreamNext::Chunk(c) => c,
            CanonStreamNext::Errored(e) => {
                stream_error = Some(e);
                break;
            }
            CanonStreamNext::Bailed | CanonStreamNext::End => break,
        };
        content.push_str(&chunk.text);
        if driver.account_rendered(&chunk) {
            break;
        }
    }
    // Session-scoped: a clean completion / max_tokens does NOT STOP -- only a verification-bail ends
    // the session early(STOP + bail off).
    let bailed = driver.bailed();
    let received = driver.received();
    drop(driver);
    deal.record_delivered(received);
    if bailed {
        deal.session.settle_verification_bail("verify-bail").await;
    } else if let Some(e) = stream_error {
        handle_stream_error_policy(&deal, received, &e).await;
        return reject(StatusCode::BAD_GATEWAY, &format!("stream error: {e}"));
    } else if received == 0 {
        deal.session.settle_empty_stream("empty-stream").await;
        return reject(StatusCode::BAD_GATEWAY, "upstream produced an empty stream");
    }
    // verification bail -> `refusal`(distinguishable from an honest `end_turn`).
    let stop_reason = if bailed { "refusal" } else { "end_turn" };
    let body = serde_json::json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{ "type": "text", "text": content }],
        "stop_reason": stop_reason,
        "stop_sequence": null
    });
    (
        [(http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap(),
    )
        .into_response()
}

fn reject(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "type": "error", "error": { "type": "invalid_request_error", "message": message } });
    (status, Json(body)).into_response()
}

fn deal_init_rejection(error: &DealInitError) -> Response {
    let mut body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": error.message()
        }
    });
    if let Some(reconciliation) = error.reconciliation() {
        body["error"]["submit_reconciliation"] = serde_json::json!(reconciliation);
    }
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

fn message_id() -> String {
    let n = message_started_secs();
    format!("msg-dexdo-{n}")
}

fn message_started_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
