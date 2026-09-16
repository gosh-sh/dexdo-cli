//! OpenAI-compatible consumer endpoint: the primary unified interface, as in
//! OpenRouter. `POST /v1/chat/completions` (SSE when `stream:true`, otherwise a single JSON) and
//! `GET /v1/models`. The model is forced by the frame; the request's `model` field is not trusted.

use crate::buyer::api::stream::{CanonStreamDriver, CanonStreamNext};
use crate::buyer::api::{
    accounted_tokens, cap_canon_to_grant, handle_stream_error_policy, is_capacity_backpressure,
    report_request_delivery, ApiDeal, ApiState, ConsumerRequestGuard, DeadGatewayAction,
    DealInitError, DeliveryEvents, RequestDelivery, RouteBudget, StreamErrorPolicyAction,
};
use crate::buyer::render::{self, OpenAiChatRequest};
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

/// `GET /v1/models` (B19): lists the model id of the buyer's configured frame/market.
pub async fn models(State(state): State<ApiState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "object": "list",
        "data": [{
            "id": state.frame_model,
            "object": "model",
            "owned_by": "dexdo"
        }]
    }))
}

/// `POST /v1/chat/completions` (B19). `stream:true` -> SSE deltas + `[DONE]`; otherwise a single
/// aggregated `chat.completion` JSON.
pub async fn chat_completions(
    State(state): State<ApiState>,
    Json(req): Json<OpenAiChatRequest>,
) -> Response {
    // The model is forced by the market (B2/B19): a request outside the frame -- reject BEFORE opening the stream.
    if let Err(reason) = state.check_model(req.model.as_deref()) {
        return reject(StatusCode::BAD_REQUEST, &reason);
    }
    let deal = match state.current_deal().await {
        Ok(deal) => deal,
        Err(error) => return deal_init_rejection(&error),
    };
    let mut request_guard = deal.begin_request(now_secs());
    // Session-scoped lifecycle: once the local deal is closed (terminal settlement landed or policy
    // recovery is pending) no new request may open a stream on the closed deal.
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
    // one-per-deal content-identity gate (B8 + B7-full), run ONCE before the first paid stream. The inline
    // StreamVerifier below only runs B5/B6 + the cheap declared-NAME B7; a seller declaring the correct model
    // NAME while serving a cheaper model is caught only here. On a bail the gate closes the deal and attempts
    // policy recovery; a transport error is not cached, so a later request retries.

    // this runs after admission and inside the reservation it granted. Verification is
    // billable service on this deal - the seller serves it and claims its terminal input+output total
    // like any other request - so a request with no quota must not reach it, and its total comes out of the same grant as the
    // answer. Anything else lets an exhausted week keep buying probes.
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
    let mut canon = render::openai_to_canon(req);
    // the grant may still hold what verification did not spend. The answer is bounded by what
    // was actually asked for, on the wire and on the way back alike.
    let max_tokens = cap_canon_to_grant(&mut canon, billing_grant_tokens);
    let id = completion_id();
    let model = state.frame_model.clone();
    let created = now_secs();
    let reclaim_heartbeat = deal.accepted_output_guard();
    request_guard.arm_upstream_failure();

    // Open an authorized TLS gRPC stream to the (mock) seller with the canonical request (R1).
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

    // Session-scoped: the deal is NOT STOPped at this request's end -- it lives for the next
    // request and is settled once at session end (graceful shutdown) or on a verification-bail. The handler
    // settles the shared session ONLY on a bail (the seller cheated -> end the session, bail off B3/B10).
    let delivery_events = state.delivery_events.clone();
    let mock_model = state.mock_model;
    if stream {
        sse_response(
            upstream,
            id,
            model,
            created,
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
            created,
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

/// Re-render the canonical stream to OpenAI-SSE (B19, R6): `chat.completion.chunk` deltas ->
/// terminal chunk with `finish_reason` -> `data: [DONE]`. Canonical verification and the visible
/// output cap happen before re-rendering; monetary accounting waits for validated terminal usage
/// and clean EOF.
#[allow(clippy::too_many_arguments)]
fn sse_response(
    upstream: tonic::Streaming<CanonChunk>,
    id: String,
    model: String,
    created: u64,
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
        let mut first = true;
        let mut capped = false;
        let mut stream_error = None;
        let mut billing_usage: Option<BillingUsage> = None;
        loop {
            let chunk = match driver.next().await {
                CanonStreamNext::Chunk(c) => c,
                CanonStreamNext::Usage(usage) => {
                    billing_usage = Some(usage);
                    continue;
                }
                // upstream transport error -- do NOT pass it off as a clean stop (see finish_reason below).
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
                yield Ok(openai_content_event(&deal, &id, &model, created, &chunk.text, first));
                first = false;
            }
        }
        // Session-scoped: completion / max_tokens / upstream-error do NOT STOP -- the deal lives for
        // the next request and is settled once at session end. ONLY a verification-bail ends the session early
        // (the seller cheated -> STOP this deal + bail off, B3/B10). `errored` still drives the finish_reason below.
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
            if e.is_capacity()
                || handle_stream_error_policy(&deal, received, e.policy_message()).await
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
        let billing_exhausted = accepted_usage
            .is_some_and(|usage| usage.total_tokens == billing_grant_tokens);
        // the terminal chunk does NOT pass off a bail or an upstream error as a clean
        // `stop` -- bail -> `content_filter`, capacity refusal -> `capacity`, other transport error ->
        // `error`, otherwise an honest `stop`.

        // `length` used to require `received == 0`, so every truncation that had already
        // rendered a token fell through to `stop`. A stream cut at 1,972,000 of a 2,000,000 grant
        // was then byte-identical to a finished answer, and a consumer paying per token had no way
        // to tell that its answer stopped short. `length` is the one value in the OpenAI enum that
        // says "the cap ended this", and the cap ends it at any `received`, not only at zero.
        let finish_reason = if bailed {
            "content_filter"
        } else if capped {
            "length"
        } else if billing_exhausted
            || stream_error.as_ref().is_some_and(|error| error.is_capacity())
        {
            "capacity"
        } else if stream_error.is_some() || received == 0 {
            "error"
        } else {
            "stop"
        };
        report_request_delivery(
            delivery_events.as_ref(),
            RequestDelivery {
                token_contract: deal.route.token_contract.clone(),
                protocol: "openai",
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
                finish_reason,
                truncated_by_output_limit: capped,
                ended_before_output_limit: received < max_tokens,
            },
        );
        yield Ok(Event::default().data(render::openai_final_chunk(&id, &model, created, finish_reason)));
        yield Ok(Event::default().data("[DONE]"));
    };
    Sse::new(sse)
}

fn openai_content_event(
    deal: &ApiDeal,
    id: &str,
    model: &str,
    created: u64,
    text: &str,
    first: bool,
) -> Event {
    deal.record_accepted_output(now_secs());
    Event::default().data(render::openai_delta_chunk(id, model, created, text, first))
}

#[cfg(test)]
pub(super) fn heartbeat_poll_test_stream(deal: ApiDeal) -> impl Stream<Item = Event> {
    async_stream::stream! {
        yield openai_content_event(&deal, "id", "model", 1, "content", true);
    }
}

/// Non-streaming (B19): collect the entire canonical stream into a single `chat.completion`.
#[allow(clippy::too_many_arguments)]
async fn aggregate_response(
    upstream: tonic::Streaming<CanonChunk>,
    id: String,
    model: String,
    created: u64,
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
    // Session-scoped: a clean completion / max_tokens does NOT STOP -- the deal lives for the next
    // request. ONLY a verification-bail ends the session early (STOP + bail off, B3/B10).
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
    let billing_exhausted =
        accepted_usage.is_some_and(|usage| usage.total_tokens == billing_grant_tokens);
    // every terminal below records the same three figures, including the two that answer 502.
    // A request that failed after paying for output is exactly the one worth being able to attribute.
    let report = |finish_reason: &'static str| {
        report_request_delivery(
            delivery_events.as_ref(),
            RequestDelivery {
                token_contract: deal.route.token_contract.clone(),
                protocol: "openai",
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
                finish_reason,
                truncated_by_output_limit: capped,
                ended_before_output_limit: received < max_tokens,
            },
        );
    };
    if bailed {
        deal.session.settle_verification_bail("verify-bail").await;
    } else if let Some(e) = stream_error {
        if e.is_capacity() {
            request_guard.complete();
            report("capacity");
            let body = render::openai_completion(&id, &model, created, &content, "capacity");
            return ([(http::header::CONTENT_TYPE, "application/json")], body).into_response();
        }
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
    // verification bail -> `content_filter`; capacity refusal returned above ->
    // `capacity`. Neither can be mistaken for an honest completion.
    // `length` no longer requires `received == 0` -- a grant that cut the answer after some of
    // it had already been aggregated said `stop`, which is indistinguishable from a complete answer.
    let finish_reason = if bailed {
        "content_filter"
    } else if capped {
        "length"
    } else if billing_exhausted {
        "capacity"
    } else {
        "stop"
    };
    report(finish_reason);
    let body = render::openai_completion(&id, &model, created, &content, finish_reason);
    ([(http::header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn reject(status: StatusCode, message: &str) -> Response {
    let body =
        serde_json::json!({ "error": { "message": message, "type": "invalid_request_error" } });
    (status, Json(body)).into_response()
}

fn deal_init_rejection(error: &DealInitError) -> Response {
    let mut body = serde_json::json!({
        "error": {
            "message": error.message(),
            "type": "invalid_request_error"
        }
    });
    if let Some(reconciliation) = error.reconciliation() {
        body["error"]["submit_reconciliation"] = serde_json::json!(reconciliation);
    }
    (deal_init_error_status(error.message()), Json(body)).into_response()
}

fn deal_init_error_status(reason: &str) -> StatusCode {
    if reason.to_ascii_lowercase().contains("invalid buy ticks") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn completion_id() -> String {
    format!("chatcmpl-dexdo-{}", now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_on_demand_buy_ticks_are_bad_request() {
        assert_eq!(
            deal_init_error_status(
                "invalid buy ticks: --ticks 1 is below the 2-tick stream minimum"
            ),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            deal_init_error_status("on-demand purchase timed out before a deal became ready"),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
