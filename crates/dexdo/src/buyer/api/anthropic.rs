//! Optional Anthropic-compatible endpoint: `POST /v1/messages`,
//! a **local transcode** to the same `CanonRequest` (OpenAI shape) and back `CanonChunk` ->
//! Anthropic-SSE, for Anthropic-native clients. The transcode is off-chain, on the
//! buyer side: the wire (gRPC) and the canonical format are not touched.

use crate::buyer::api::stream::{CanonStreamDriver, CanonStreamNext};
use crate::buyer::api::{
    accounted_tokens, cap_canon_to_grant, handle_stream_error_policy, is_capacity_backpressure,
    report_request_delivery, ApiDeal, ApiState, ConsumerRequestGuard, DeadGatewayAction,
    DealInitError, DeliveryEvents, RequestDelivery, RouteBudget, StreamErrorPolicyAction,
};
use crate::buyer::render::{self, AnthropicRequest};
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dexdo_core::params::BUYER_UPSTREAM_OPEN_RETRIES;
use dexdo_proto::{BillingUsage, CanonChunk};
use futures::Stream;
use http::StatusCode;
use std::convert::Infallible;
use std::time::{SystemTime, UNIX_EPOCH};

/// `POST /v1/messages` (B20). Transcodes the request -> `CanonRequest`, opens the same
/// authorized TLS gRPC stream and re-renders `CanonChunk` into Anthropic-SSE
/// (`message_start` -> `content_block_*` -> `message_delta` -> `message_stop`).
pub async fn messages(
    State(state): State<ApiState>,
    Json(req): Json<AnthropicRequest>,
) -> Response {
    // The model is forced by the market (B2/B19) -- the same check as for the OpenAI path.
    if let Err(reason) = state.check_model(req.model.as_deref()) {
        return reject(StatusCode::BAD_REQUEST, &reason);
    }
    let deal = match state.current_deal().await {
        Ok(deal) => deal,
        Err(error) => return deal_init_rejection(&error),
    };
    let mut request_guard = deal.begin_request(message_started_secs());
    // Session-scoped lifecycle: no new request once the local deal is closed.
    if deal.session.is_closed() {
        return reject(StatusCode::GONE, "deal session closed; open a new session");
    }
    let serving_state = match deal.session.ensure_open_for_serving().await {
        Ok(state) => state,
        Err(reason) => return reject(StatusCode::BAD_GATEWAY, &reason),
    };
    let stream = req.stream;
    let requested_max_tokens = req.max_tokens;
    // admission reserves the route's entire free billable remainder before the model is
    // contacted, so two concurrent requests can never share an unknown provider-native input bill. For a
    // subscription it first books any due week boundary through the permissionless path and recomputes
    // from the coherent state that comes back -- an under-used week is never carried across it, and a
    // finished term is never served from a stale positive remainder.
    match deal
        .admit_with_probe_observation(requested_max_tokens, serving_state.probe_accepted)
        .await
    {
        RouteBudget::Admitted(reservation) => {
            request_guard.hold(reservation);
        }
        RouteBudget::Exhausted(reason) => return reject(StatusCode::SERVICE_UNAVAILABLE, &reason),
    }
    // one-per-deal content-identity gate (B8 + B7-full), run ONCE before the first paid stream -- the same
    // gate as the OpenAI path. The inline StreamVerifier only runs B5/B6 + the cheap declared-NAME B7; a seller
    // serving a cheaper model under the correct NAME is caught only here. On a bail the gate closes the deal and
    // attempts policy recovery; a transport error is not cached, so a later request retries.

    // this runs after admission and inside the reservation it granted. Verification is
    // billable service on this deal, so an exhausted week must not reach it and its terminal
    // input+output total comes out of the same grant as the answer.
    let verdict = deal
        .content_gate
        .ensure_verified(&state.buyer, &deal, &mut request_guard)
        .await;
    if let Err(reason) = verdict {
        return reject(
            StatusCode::BAD_GATEWAY,
            &format!("model identity verification failed (content check): {reason}"),
        );
    }
    let billing_grant_tokens = request_guard.remaining_grant();
    if billing_grant_tokens == 0 {
        return reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "the identity verification this deal owed consumed the whole admitted grant; the \
             request was not sent and the next one starts from a verified deal",
        );
    }
    let mut canon = render::anthropic_to_canon(req);
    // the grant may still hold what verification did not spend. The answer is bounded by what
    // was actually asked for, on the wire and on the way back alike.
    let max_tokens = cap_canon_to_grant(&mut canon, billing_grant_tokens);
    let id = message_id();
    let model = state.frame_model.clone();
    let reclaim_heartbeat = deal.accepted_output_guard();
    request_guard.arm_upstream_failure();

    let retry_limit = if deal.session.dead_gateway_action() == DeadGatewayAction::RetryThenReclaim {
        BUYER_UPSTREAM_OPEN_RETRIES
    } else {
        0
    };
    let mut retries = 0usize;
    let upstream = loop {
        match state
            .buyer
            .open_canon_stream(
                &deal.route.handover,
                &deal.route.token_contract,
                canon.clone(),
                billing_grant_tokens,
            )
            .await
        {
            Ok(stream) => break Ok(stream),
            Err(error) if retries < retry_limit => {
                retries += 1;
                tracing::warn!(
                    error = %error,
                    token_contract = %super::display_token_contract(&deal.route.token_contract),
                    "consumer API: upstream open failed; retrying once per dead_gateway=retry_then_reclaim"
                );
            }
            Err(error) => break Err(error),
        }
    };
    let upstream = match upstream {
        Ok(stream) => stream,
        Err(error) => {
            // The seller answered, and its answer was "not yet" -- request-scoped, not a dead
            // gateway. The local caller retries; the deal is left alone.
            if is_capacity_backpressure(&error) {
                request_guard.complete();
                return reject(
                    StatusCode::TOO_MANY_REQUESTS,
                    &format!(
                        "the deal has no delivery capacity for this request yet; retry after seller \
                         capacity reconciliation: {error}"
                    ),
                );
            }
            // the retry above logs at WARN, and until now the DEFINITIVE refusal logged
            // nothing at all -- a run that ended in a dead gateway had no ERROR line for an
            // operator filtering by level, and the address that was tried appeared nowhere in
            // the log at any verbosity. Retries stay WARN; this one is terminal.
            tracing::error!(
                error = %error,
                endpoint = %deal.route.handover.endpoint,
                token_contract = %super::display_token_contract(&deal.route.token_contract),
                retries,
                "consumer API: upstream open failed definitively; settling the deal as a dead gateway"
            );
            deal.session
                .settle_dead_gateway("dead-gateway", &reclaim_heartbeat)
                .await;
            // The bare `{error}` here rendered `transport error` and nothing else: the stage and
            // every cause under it were dropped, leaving the operator with a dead gateway and no
            // way to tell a refused connect from a TLS alert or an h2 rejection.
            let detail = crate::buyer::tls::dial_failure_detail(&error);
            let reason = if retries == 0 {
                format!("upstream open failed: {detail}")
            } else if retries == 1 {
                format!("upstream open failed after retry: {detail}")
            } else {
                format!("upstream open failed after {retries} retries: {detail}")
            };
            return reject(StatusCode::BAD_GATEWAY, &reason);
        }
    };

    // Session-scoped: no per-request STOP -- the shared session settles once at session end / on a
    // verification-bail (as in the OpenAI path).
    let delivery_events = state.delivery_events.clone();
    let mock_model = state.mock_model;
    if stream {
        sse_response(
            upstream,
            id,
            model,
            max_tokens,
            billing_grant_tokens,
            mock_model,
            deal,
            request_guard,
            delivery_events,
        )
        .into_response()
    } else {
        aggregate_response(
            upstream,
            id,
            model,
            max_tokens,
            billing_grant_tokens,
            mock_model,
            deal,
            request_guard,
            delivery_events,
        )
        .await
        .into_response()
    }
}

/// Re-render the canonical stream to Anthropic-SSE (B20, R6). Verification and output-cap tracking
/// happen before re-rendering; monetary accounting waits for validated terminal usage and clean EOF.
#[allow(clippy::too_many_arguments)]
fn sse_response(
    upstream: tonic::Streaming<CanonChunk>,
    id: String,
    model: String,
    max_tokens: u64,
    billing_grant_tokens: u64,
    mock_model: bool,
    deal: ApiDeal,
    mut request_guard: ConsumerRequestGuard,
    delivery_events: Option<DeliveryEvents>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let sse = async_stream::stream! {
        let mut driver = CanonStreamDriver::new(
            upstream,
            deal.session.closed_receiver(),
            model.clone(),
            max_tokens,
            billing_grant_tokens,
            mock_model,
        );
        let mut capped = false;
        let mut stream_error = None;
        let mut billing_usage: Option<BillingUsage> = None;
        yield Ok(event(render::anthropic_message_start(&id, &model)));
        yield Ok(event(render::anthropic_content_block_start()));
        loop {
            let chunk = match driver.next().await {
                CanonStreamNext::Chunk(c) => c,
                CanonStreamNext::Usage(usage) => {
                    billing_usage = Some(usage);
                    continue;
                }
                // upstream transport error -- do not pass it off as a clean `end_turn`.
                CanonStreamNext::Errored(e) => {
                    stream_error = Some(e);
                    break;
                }
                CanonStreamNext::Bailed | CanonStreamNext::End => break,
            };
            // exact token IDs are capped before rendering. Provider text fragments
            // are transport units, so their output-only cap is checked against terminal usage.
            if !driver.admits(&chunk) {
                capped = true;
                break;
            }
            // Track the visible output lower bound before its bytes leave so an immediate consumer
            // disconnect cannot erase evidence of what was rendered. This is not a monetary update:
            // the held billing reservation commits only after validated terminal usage and clean EOF.
            if let Err(error) = deal.record_visible_output(accounted_tokens(&chunk)) {
                stream_error = Some(error.into());
                break;
            }
            let before = driver.received();
            driver.account_rendered(&chunk);
            debug_assert!(driver.received() >= before);
            if !chunk.text.is_empty() {
                yield Ok(anthropic_content_event(&deal, &chunk.text));
            }
        }
        // Session-scoped: completion / max_tokens / upstream-error do NOT STOP -- only a
        // verification-bail ends the session early (STOP + bail off). `errored` still drives stop_reason below.
        let bailed = driver.bailed();
        let received = driver.received();
        let rendered_tokens = driver.legacy_received();
        drop(driver);
        if !bailed && stream_error.is_none() {
            if let Some(usage) = billing_usage.as_ref() {
                if let Err(error) = request_guard.record_billing(&deal, usage.total_tokens) {
                    stream_error = Some(error.into());
                }
            }
        }
        if bailed {
            deal.session.settle_verification_bail("verify-bail").await;
        } else if let Some(e) = &stream_error {
            if handle_stream_error_policy(&deal, received, e.policy_message()).await
                == StreamErrorPolicyAction::RequestScoped
            {
                request_guard.complete();
            }
        } else if received == 0 && !capped {
            let heartbeat = deal.accepted_output_guard();
            deal.session
                .settle_empty_stream("empty-stream", &heartbeat)
                .await;
        } else {
            request_guard.complete();
        }
        let accepted_usage = if stream_error.is_none() && !bailed {
            billing_usage.as_ref()
        } else {
            None
        };
        // stop_reason does NOT pass off a bail/error as an honest `end_turn` -- bail -> `refusal`,
        // transport error -> `error`, otherwise `end_turn`.

        // `max_tokens` used to require `received == 0`, so a grant that cut the answer after
        // some of it had already been rendered reported `end_turn` -- indistinguishable from a model
        // that finished. The cap ends the answer at any `received`, not only at zero.
        let stop_reason = if bailed {
            "refusal"
        } else if capped {
            "max_tokens"
        } else if stream_error.is_some() || received == 0 {
            "error"
        } else {
            "end_turn"
        };
        report_request_delivery(
            delivery_events.as_ref(),
            RequestDelivery {
                token_contract: deal.route.token_contract.clone(),
                protocol: "anthropic",
                streamed: true,
                billing_grant_tokens,
                output_limit_tokens: max_tokens,
                visible_output_tokens: received,
                rendered_tokens,
                route_delivered_tokens: Some(deal.route_visible_output_tokens()),
                input_tokens: accepted_usage.map(|usage| usage.input_tokens),
                output_tokens: accepted_usage.map(|usage| usage.output_tokens),
                billable_tokens: accepted_usage.map(|usage| usage.total_tokens),
                route_billable_tokens: deal.session.route_billable_tokens(),
                finish_reason: stop_reason,
                truncated_by_output_limit: capped,
                ended_before_output_limit: received < max_tokens,
            },
        );
        yield Ok(event(render::anthropic_content_block_stop()));
        yield Ok(event(render::anthropic_message_delta(stop_reason)));
        yield Ok(event(render::anthropic_message_stop()));
    };
    Sse::new(sse)
}

fn anthropic_content_event(deal: &ApiDeal, text: &str) -> Event {
    deal.record_accepted_output(message_started_secs());
    event(render::anthropic_content_block_delta(text))
}

#[cfg(test)]
pub(super) fn heartbeat_poll_test_stream(deal: ApiDeal) -> impl Stream<Item = Event> {
    async_stream::stream! {
        yield anthropic_content_event(&deal, "content");
    }
}

/// Build an axum SSE `Event` from `(name, JSON data)` (B20). A single source of truth for the
/// frame -- the HTTP layer adds `event:`/`data:`.
fn event((name, data): render::AnthropicEvent) -> Event {
    Event::default().event(name).data(data)
}

/// Non-streaming Anthropic response (B20): a single `message` JSON with aggregated text.
#[allow(clippy::too_many_arguments)]
async fn aggregate_response(
    upstream: tonic::Streaming<CanonChunk>,
    id: String,
    model: String,
    max_tokens: u64,
    billing_grant_tokens: u64,
    mock_model: bool,
    deal: ApiDeal,
    mut request_guard: ConsumerRequestGuard,
    delivery_events: Option<DeliveryEvents>,
) -> Response {
    let mut content = String::new();
    let mut capped = false;
    let mut driver = CanonStreamDriver::new(
        upstream,
        deal.session.closed_receiver(),
        model.clone(),
        max_tokens,
        billing_grant_tokens,
        mock_model,
    );
    let mut stream_error = None;
    let mut billing_usage: Option<BillingUsage> = None;
    loop {
        let chunk = match driver.next().await {
            CanonStreamNext::Chunk(c) => c,
            CanonStreamNext::Usage(usage) => {
                billing_usage = Some(usage);
                continue;
            }
            CanonStreamNext::Errored(e) => {
                stream_error = Some(e);
                break;
            }
            CanonStreamNext::Bailed | CanonStreamNext::End => break,
        };
        // the output limit is a HARD cap. A chunk that does not fit inside what is
        // left of it is never rendered, independently of the larger total billing grant.
        // Stopping here is the REQUEST hitting its own cap, not the seller failing: it must not be
        // mistaken for an empty stream, which is a settlement action against the counterparty.
        if !driver.admits(&chunk) {
            capped = true;
            break;
        }
        // Track the visible output lower bound before it joins the answer. Money still waits for a
        // validated terminal input-plus-output record and clean EOF.
        if let Err(error) = deal.record_visible_output(accounted_tokens(&chunk)) {
            stream_error = Some(error.into());
            break;
        }
        let before = driver.received();
        driver.account_rendered(&chunk);
        debug_assert!(driver.received() >= before);
        content.push_str(&chunk.text);
    }
    // Session-scoped: a clean completion / max_tokens does NOT STOP -- only a verification-bail ends
    // the session early (STOP + bail off).
    let bailed = driver.bailed();
    let received = driver.received();
    let rendered_tokens = driver.legacy_received();
    drop(driver);
    if !bailed && stream_error.is_none() {
        if let Some(usage) = billing_usage.as_ref() {
            if let Err(error) = request_guard.record_billing(&deal, usage.total_tokens) {
                stream_error = Some(error.into());
            }
        }
    }
    let accepted_usage = if stream_error.is_none() && !bailed {
        billing_usage.as_ref()
    } else {
        None
    };
    // every terminal below records the same three figures, including the two that answer 502.
    let report = |stop_reason: &'static str| {
        report_request_delivery(
            delivery_events.as_ref(),
            RequestDelivery {
                token_contract: deal.route.token_contract.clone(),
                protocol: "anthropic",
                streamed: false,
                billing_grant_tokens,
                output_limit_tokens: max_tokens,
                visible_output_tokens: received,
                rendered_tokens,
                route_delivered_tokens: Some(deal.route_visible_output_tokens()),
                input_tokens: accepted_usage.map(|usage| usage.input_tokens),
                output_tokens: accepted_usage.map(|usage| usage.output_tokens),
                billable_tokens: accepted_usage.map(|usage| usage.total_tokens),
                route_billable_tokens: deal.session.route_billable_tokens(),
                finish_reason: stop_reason,
                truncated_by_output_limit: capped,
                ended_before_output_limit: received < max_tokens,
            },
        );
    };
    if bailed {
        deal.session.settle_verification_bail("verify-bail").await;
    } else if let Some(e) = stream_error {
        if handle_stream_error_policy(&deal, received, e.policy_message()).await
            == StreamErrorPolicyAction::RequestScoped
        {
            request_guard.complete();
        }
        report("error");
        return reject(StatusCode::BAD_GATEWAY, &format!("stream error: {e}"));
    } else if received == 0 && !capped {
        let heartbeat = deal.accepted_output_guard();
        deal.session
            .settle_empty_stream("empty-stream", &heartbeat)
            .await;
        report("error");
        return reject(StatusCode::BAD_GATEWAY, "upstream produced an empty stream");
    }
    request_guard.complete();
    // verification bail -> `refusal` (distinguishable from an honest `end_turn`).
    // `max_tokens` no longer requires `received == 0` -- a grant that cut the answer after
    // part of it had been aggregated reported `end_turn`, which is what a finished model reports.
    let stop_reason = if bailed {
        "refusal"
    } else if capped {
        "max_tokens"
    } else {
        "end_turn"
    };
    report(stop_reason);
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
