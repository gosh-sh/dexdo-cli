//! OpenAI-compatible consumer endpoint: the primary unified interface, as in
//! OpenRouter. `POST /v1/chat/completions` (SSE when `stream:true`, otherwise a single JSON) and
//! `GET /v1/models`. The model is forced by the frame; the request's `model` field is not trusted.

use crate::buyer::api::stream::{CanonStreamDriver, CanonStreamNext};
use crate::buyer::api::{
    accounted_tokens, aggregate_body_response, attach_request_id, cap_canon_to_grant,
    capacity_rejection, consumer_request_id, handle_stream_error_policy, is_capacity_backpressure,
    report_pre_answer_failure, ApiDeal, ApiState, ClosureCategory, ConsumerError,
    ConsumerErrorCode, ConsumerRequestGuard, DeadGatewayAction, DealInitError, RequestDelivery,
    RouteBudget, StreamErrorPolicyAction,
};
use crate::buyer::render::{self, OpenAiChatRequest};
use axum::extract::{rejection::JsonRejection, State};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dexdo_core::params::BUYER_UPSTREAM_OPEN_RETRIES;
use dexdo_proto::{BillingUsage, CanonChunk, CanonRequest};
use futures::{Stream, StreamExt};
use http::{HeaderMap, StatusCode};
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
    headers: HeaderMap,
    req: Result<Json<OpenAiChatRequest>, JsonRejection>,
) -> Response {
    let req = match req {
        Ok(Json(req)) => req,
        Err(error) => return super::error::json_rejection(error, false),
    };
    if let Err(reason) = req.validate_reasoning() {
        return reject_unsupported_reasoning(&reason);
    }
    if !req.tools.is_empty() {
        return reject(
            StatusCode::BAD_REQUEST,
            "unsupported Chat Completions field: tools",
        );
    }
    let model = req.model.clone();
    let stream = req.stream;
    let include_usage = req
        .stream_options
        .as_ref()
        .is_some_and(|options| options.include_usage);
    let max_tokens = req.max_tokens;
    let mut canon = render::openai_to_canon(req);
    canon.session_id = match session_id(&headers) {
        Ok(id) => id,
        Err(reason) => return reject(StatusCode::BAD_REQUEST, reason),
    };
    handle_text_request(
        state,
        ConsumerInput {
            model,
            stream,
            include_usage,
            max_tokens,
            canon,
            wire: WireFormat::Chat,
        },
    )
    .await
}

/// Text-only Responses wire format on the same buyer deal and billing path.
pub async fn responses(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(error) => return super::error::json_rejection(error, false),
    };
    let mut req = match render::responses_text_request(body) {
        Ok(req) => req,
        Err(reason) => return reject(StatusCode::BAD_REQUEST, &reason),
    };
    req.canon.session_id = match session_id(&headers) {
        Ok(id) => id,
        Err(reason) => return reject(StatusCode::BAD_REQUEST, reason),
    };
    handle_text_request(
        state,
        ConsumerInput {
            model: req.model,
            stream: req.stream,
            include_usage: false,
            max_tokens: req.max_output_tokens,
            canon: req.canon,
            wire: WireFormat::Responses,
        },
    )
    .await
}

fn session_id(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    headers
        .get("session_id")
        .or_else(|| headers.get("conversation_id"))
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| "session_id or conversation_id must contain ASCII text")
        })
        .transpose()
}

#[derive(Clone, Copy)]
enum WireFormat {
    Chat,
    Responses,
}

impl WireFormat {
    fn protocol(self) -> &'static str {
        match self {
            Self::Chat => "openai",
            Self::Responses => "openai_responses",
        }
    }
}

struct ConsumerInput {
    model: Option<String>,
    stream: bool,
    include_usage: bool,
    max_tokens: Option<u32>,
    canon: CanonRequest,
    wire: WireFormat,
}

async fn handle_text_request(state: ApiState, req: ConsumerInput) -> Response {
    let request_id = consumer_request_id();
    let response = handle_text_request_with_id(state, req, request_id.clone()).await;
    attach_request_id(response, &request_id)
}

async fn handle_text_request_with_id(
    state: ApiState,
    req: ConsumerInput,
    request_id: String,
) -> Response {
    // The model is forced by the market (B2/B19): a request outside the frame -- reject BEFORE opening the stream.
    if let Err(reason) = state.check_model(req.model.as_deref()) {
        return reject(StatusCode::BAD_REQUEST, &reason);
    }
    let deal = match state.current_deal().await {
        Ok(deal) => deal,
        Err(error) => return deal_init_rejection(&error),
    };
    // Valid requests still record demand when a failed/closed route needs renewal.
    // Beginning the guard does not reserve capacity or run billable identity probes.
    let mut request_guard = deal.begin_request(now_secs());
    if let Some(response) = deal.session.closed_rejection(&request_id, false) {
        return response;
    }
    let serving_state = match deal.session.ensure_open_for_serving().await {
        Ok(state) => state,
        Err(error) => return error.response(StatusCode::BAD_GATEWAY, false),
    };
    let retry_limit = if deal.session.dead_gateway_action() == DeadGatewayAction::RetryThenReclaim {
        BUYER_UPSTREAM_OPEN_RETRIES
    } else {
        0
    };
    if req
        .canon
        .params
        .as_ref()
        .is_some_and(|p| p.disable_reasoning || p.reasoning_effort.is_some())
    {
        // Capability transport failures obey the same retry and guarded recovery
        // policy as a stream-open failure. An unsupported option remains request-scoped.
        let capability_heartbeat = deal.accepted_output_guard();
        request_guard.arm_upstream_failure();
        let mut retries = 0usize;
        loop {
            match state
                .buyer
                .check_reasoning(
                    &deal.route.handover,
                    &deal.route.token_contract,
                    req.canon.params.as_ref(),
                )
                .await
            {
                Ok(()) => break,
                Err(error) if is_unsupported_reasoning(&error) => {
                    request_guard.complete();
                    return reject_unsupported_reasoning(&error.to_string());
                }
                Err(error) if retries < retry_limit => {
                    retries += 1;
                    tracing::warn!(
                        error = %error,
                        token_contract = %super::display_token_contract(&deal.route.token_contract),
                        "consumer API: capability query failed; retrying once per dead_gateway=retry_then_reclaim"
                    );
                }
                Err(error) => {
                    request_guard.observe_policy_failure(ClosureCategory::DeadGateway);
                    let reason =
                        settle_upstream_open_failure(&deal, &error, retries, capability_heartbeat)
                            .await;
                    return reject(StatusCode::BAD_GATEWAY, &reason);
                }
            }
        }
        request_guard.complete();
    }
    // Session-scoped lifecycle: once the local deal is closed (terminal settlement landed or policy
    // recovery is pending) no new request may open a stream on the closed deal.
    if let Some(response) = deal.session.closed_rejection(&request_id, false) {
        return response;
    }
    let stream = req.stream;
    let include_usage = req.include_usage;
    let requested_max_tokens = req.max_tokens;
    // Authorize payment from the shared remainder; the seller enforces per-deal concurrency
    // and allocates actual charges atomically. Subscription boundary reconciliation stays here.
    match deal
        .admit_with_probe_observation(requested_max_tokens, serving_state.probe_accepted)
        .await
    {
        RouteBudget::Admitted(reservation) => {
            request_guard.hold(reservation);
        }
        RouteBudget::Exhausted(error) => return state.budget_rejection(error, false),
    }
    if let Err(error) = request_guard.arm_delivery(
        state.delivery_events.clone(),
        &deal,
        request_id.clone(),
        req.wire.protocol(),
        stream,
        requested_max_tokens.map(u64::from).unwrap_or(0),
    ) {
        tracing::error!(%error, "buyer usage: could not bind admitted request");
        return super::usage::unavailable("usage history could not be bound to this deal");
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
        .await
        .and_then(|()| {
            deal.session
                .ensure_paid_phase()
                .map_err(anyhow::Error::from)
        });
    request_guard.usage_phase = "answer";
    if let Err(reason) = verdict {
        let response = if let Some(error) = reason.downcast_ref::<ConsumerError>() {
            deal.session
                .closed_rejection(&request_id, false)
                .unwrap_or_else(|| error.response(StatusCode::SERVICE_UNAVAILABLE, false))
        } else {
            reject(
                StatusCode::BAD_GATEWAY,
                &format!("model identity verification failed (content check): {reason}"),
            )
        };
        report_pre_answer_failure(
            &deal,
            &mut request_guard,
            &request_id,
            req.wire.protocol(),
            stream,
            requested_max_tokens.map(u64::from).unwrap_or(0),
            response.status().as_u16(),
            "content_verification",
        );
        return response;
    }
    let billing_grant_tokens = request_guard.remaining_grant();
    if billing_grant_tokens == 0 {
        report_pre_answer_failure(
            &deal,
            &mut request_guard,
            &request_id,
            req.wire.protocol(),
            stream,
            requested_max_tokens.map(u64::from).unwrap_or(0),
            StatusCode::SERVICE_UNAVAILABLE.as_u16(),
            "content_verification_budget",
        );
        return ConsumerError::new(
            ConsumerErrorCode::VerificationGrantConsumed,
            "the identity verification this deal owed consumed the whole admitted grant; the \
             request was not sent and the next one starts from a verified deal",
        )
        .response(StatusCode::SERVICE_UNAVAILABLE, false);
    }
    let mut canon = req.canon;
    // the grant may still hold what verification did not spend. The answer is bounded by what
    // was actually asked for, on the wire and on the way back alike.
    let max_tokens = cap_canon_to_grant(&mut canon, billing_grant_tokens);
    request_guard.set_delivery_output_limit(max_tokens);
    let created = now_secs();
    let id = match req.wire {
        WireFormat::Chat => completion_id(),
        WireFormat::Responses => responses_id(created),
    };
    let model = state.frame_model.clone();
    let reclaim_heartbeat = deal.accepted_output_guard();
    request_guard.arm_upstream_failure();

    // Open an authorized TLS gRPC stream to the (mock) seller with the canonical request (R1).
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
            if is_unsupported_reasoning(&error) {
                request_guard.complete();
                return reject_unsupported_reasoning(&error.to_string());
            }
            // The seller answered, and its answer was "not yet" -- request-scoped, not a dead
            // gateway. The local caller retries; the deal is left alone.
            if is_capacity_backpressure(&error) {
                request_guard.complete();
                report_pre_answer_failure(
                    &deal,
                    &mut request_guard,
                    &request_id,
                    req.wire.protocol(),
                    stream,
                    max_tokens,
                    StatusCode::TOO_MANY_REQUESTS.as_u16(),
                    "upstream_open",
                );
                return capacity_rejection(&error).response(StatusCode::TOO_MANY_REQUESTS, false);
            }
            request_guard.observe_policy_failure(ClosureCategory::DeadGateway);
            let reason =
                settle_upstream_open_failure(&deal, &error, retries, reclaim_heartbeat).await;
            report_pre_answer_failure(
                &deal,
                &mut request_guard,
                &request_id,
                req.wire.protocol(),
                stream,
                max_tokens,
                StatusCode::BAD_GATEWAY.as_u16(),
                "upstream_open",
            );
            return reject(StatusCode::BAD_GATEWAY, &reason);
        }
    };

    // Session-scoped: the deal is NOT STOPped at this request's end -- it lives for the next
    // request and is settled once at session end (graceful shutdown) or on a verification-bail. The handler
    // settles the shared session ONLY on a bail (the seller cheated -> end the session, bail off B3/B10).
    let mock_model = state.mock_model;
    if stream {
        #[cfg(test)]
        let test_delivery_finalizer = request_guard.delivery_finalizer();
        request_guard.mark_response_started();
        let response = sse_response(
            upstream,
            id,
            model,
            created,
            max_tokens,
            billing_grant_tokens,
            mock_model,
            deal,
            request_guard,
            include_usage,
            req.wire,
            request_id,
        )
        .into_response();
        #[cfg(test)]
        let mut response = response;
        #[cfg(test)]
        if let Some(finalizer) = test_delivery_finalizer {
            response.extensions_mut().insert(finalizer);
        }
        response
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
            req.wire,
            request_id,
        )
        .await
        .into_response()
    }
}

// Capability queries and stream opens share the existing guarded dead-gateway
// policy and diagnostics; unsupported controls never enter this path.
async fn settle_upstream_open_failure(
    deal: &ApiDeal,
    error: &anyhow::Error,
    retries: usize,
    reclaim_heartbeat: dexdo_core::market::HeartbeatGuard,
) -> String {
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
    let detail = crate::buyer::tls::dial_failure_detail(error);
    if retries == 0 {
        format!("upstream open failed: {detail}")
    } else if retries == 1 {
        format!("upstream open failed after retry: {detail}")
    } else {
        format!("upstream open failed after {retries} retries: {detail}")
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
    include_usage: bool,
    wire: WireFormat,
    request_id: String,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let delivery_finalizer = request_guard.delivery_finalizer();
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
        let mut content = String::new();
        let mut visible_text_tokens = 0_u64;
        let message_id = format!("msg_{id}");
        if matches!(wire, WireFormat::Responses) {
            yield Ok((responses_event("response.created", serde_json::json!({
                "response": {"id": id, "object": "response", "created_at": created,
                    "status": "in_progress", "model": model, "output": [], "usage": null}
            })), false));
            yield Ok((responses_event("response.in_progress", serde_json::json!({
                "response": {"id": id, "object": "response", "created_at": created,
                    "status": "in_progress", "model": model, "output": [], "usage": null}
            })), false));
            yield Ok((responses_event("response.output_item.added", serde_json::json!({
                "output_index": 0, "item": {"id": message_id, "type": "message",
                    "status": "in_progress", "role": "assistant", "content": []}
            })), false));
            yield Ok((responses_event("response.content_part.added", serde_json::json!({
                "item_id": message_id, "output_index": 0, "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            })), false));
        }
        loop {
            let chunk = match driver.next().await {
                CanonStreamNext::Chunk(c) => c,
                CanonStreamNext::Usage(usage) => {
                    if let Err(error) = request_guard.observe_answer_usage(usage) {
                        stream_error = Some(error.into());
                        break;
                    }
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
            // Preserve the stable v2 route witness for every accepted canonical content chunk,
            // including hidden reasoning. The request-level visible lower bound remains text-only.
            let chunk_tokens = accounted_tokens(&chunk);
            if let Err(error) = deal.record_visible_output(chunk_tokens) {
                stream_error = Some(error.into());
                break;
            }
            if !chunk.text.is_empty() {
                visible_text_tokens = visible_text_tokens.saturating_add(chunk_tokens);
            }
            let before = driver.received();
            driver.account_rendered(&chunk);
            debug_assert!(driver.received() >= before);
            request_guard.record_delivery_progress(
                driver.received(),
                visible_text_tokens,
                driver.legacy_received(),
            );
            if !chunk.text.is_empty() {
                if matches!(wire, WireFormat::Responses) {
                    deal.record_accepted_output(now_secs());
                    content.push_str(&chunk.text);
                    yield Ok((responses_event("response.output_text.delta", serde_json::json!({
                        "item_id": message_id, "output_index": 0, "content_index": 0,
                        "delta": chunk.text
                    })), false));
                } else {
                    yield Ok((openai_content_event(
                        &deal,
                        &id,
                        &model,
                        created,
                        &chunk.text,
                        first,
                        include_usage,
                    ), false));
                }
                first = false;
            }
        }
        // Session-scoped: completion / max_tokens / upstream-error do NOT STOP -- the deal lives for
        // the next request and is settled once at session end. ONLY a verification-bail ends the session early
        // (the seller cheated -> STOP this deal + bail off, B3/B10). `errored` still drives the finish_reason below.
        let bailed = driver.bailed();
        let received = visible_text_tokens;
        let rendered_tokens = driver.legacy_received();
        let provider_output_tokens = driver.received();
        drop(driver);
        if !bailed && stream_error.is_none() {
            if let Some(usage) = billing_usage.as_ref() {
                if let Err(error) = request_guard.record_billing(&deal, *usage) {
                    stream_error = Some(error.into());
                }
            }
        }
        if bailed {
            request_guard.observe_policy_failure(ClosureCategory::VerificationBail);
            deal.session.settle_verification_bail("verify-bail").await;
        } else if let Some(e) = &stream_error {
            request_guard.observe_upstream_failure();
            if e.is_capacity()
                || handle_stream_error_policy(&deal, provider_output_tokens, e, &mut request_guard).await
                    == StreamErrorPolicyAction::RequestScoped
            {
                request_guard.complete();
            }
        } else if received == 0 && !capped {
            request_guard.observe_policy_failure(ClosureCategory::EmptyStream);
            let heartbeat = deal.accepted_output_guard();
            deal.session
                .settle_empty_stream("empty-output", &heartbeat)
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
            .is_some_and(|usage| usage.billable_tokens() == billing_grant_tokens || usage.billable_tokens() < usage.total_tokens);
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
        } else if stream_error.is_some() {
            "error"
        } else if received == 0 {
            "empty_output"
        } else {
            "stop"
        };
        let visible_output_tokens = received;
        let failure_stage = if bailed {
            Some("inline_verification")
        } else if stream_error.is_some() {
            Some("upstream_stream")
        } else if received == 0 && !capped {
            Some("empty_output")
        } else {
            None
        };
        let request_usage = request_guard.request_usage();
        request_guard.prepare_delivery(
            RequestDelivery {
                token_contract: deal.route.token_contract.clone(),
                request_id: request_id.clone(),
                protocol: wire.protocol(),
                streamed: true,
                http_status: StatusCode::OK.as_u16(),
                failure_stage,
                billing_grant_tokens,
                output_limit_tokens: max_tokens,
                visible_output_tokens,
                rendered_tokens,
                route_delivered_tokens: Some(deal.route_visible_output_tokens()),
                input_tokens: accepted_usage.map(|usage| usage.input_tokens),
                output_tokens: accepted_usage.map(|usage| usage.output_tokens),
                billable_tokens: accepted_usage.map(|usage| usage.total_tokens),
                request_input_tokens: request_usage.input_tokens,
                request_output_tokens: request_usage.output_tokens,
                request_billable_tokens: request_usage.total_tokens,
                route_billable_tokens: deal.session.route_billable_tokens(),
                finish_reason,
                truncated_by_output_limit: capped,
                ended_before_output_limit: provider_output_tokens < max_tokens,
            },
        );
        if received == 0 {
            if let Some(error) = stream_error.as_ref().filter(|error| error.is_retryable_upstream()) {
                let body = error.retryable_rejection().body(false);
                let event = match wire {
                    WireFormat::Responses => responses_event("error", body["error"].clone()),
                    WireFormat::Chat => Event::default().data(body.to_string()),
                };
                yield Ok((event, false));
            }
        }
        if matches!(wire, WireFormat::Responses) {
            yield Ok((responses_event("response.output_text.done", serde_json::json!({
                "item_id": message_id, "output_index": 0, "content_index": 0, "text": content
            })), false));
            yield Ok((responses_event("response.content_part.done", serde_json::json!({
                "item_id": message_id, "output_index": 0, "content_index": 0,
                "part": {"type": "output_text", "text": content, "annotations": []}
            })), false));
            let response = render::responses_response(&id, &model, created, &content, finish_reason, accepted_usage);
            yield Ok((responses_event("response.output_item.done", serde_json::json!({
                "output_index": 0, "item": response["output"][0].clone()
            })), false));
            let terminal = match response["status"].as_str() {
                Some("completed") => "response.completed",
                Some("incomplete") => "response.incomplete",
                _ => "response.failed",
            };
            yield Ok((responses_event(terminal, serde_json::json!({"response": response})), false));
        } else {
            yield Ok((Event::default().data(render::openai_final_chunk(
                &id, &model, created, finish_reason, include_usage,
            )), false));
            if include_usage {
                if let Some(usage) = accepted_usage {
                    yield Ok((Event::default().data(render::openai_usage_chunk(&id, &model, created, usage)), false));
                }
            }
        }
        yield Ok((Event::default().data("[DONE]"), true));
    };
    Sse::new(sse.map(move |item| {
        item.map(|(event, terminal)| {
            if terminal {
                if let Some(finalizer) = delivery_finalizer.as_ref() {
                    finalizer.report_prepared();
                }
            }
            event
        })
    }))
}

fn responses_event(kind: &'static str, mut body: serde_json::Value) -> Event {
    body["type"] = serde_json::Value::String(kind.into());
    Event::default().event(kind).data(body.to_string())
}

fn openai_content_event(
    deal: &ApiDeal,
    id: &str,
    model: &str,
    created: u64,
    text: &str,
    first: bool,
    include_usage: bool,
) -> Event {
    deal.record_accepted_output(now_secs());
    Event::default().data(render::openai_delta_chunk(
        id,
        model,
        created,
        text,
        first,
        include_usage,
    ))
}

#[cfg(test)]
pub(super) fn heartbeat_poll_test_stream(deal: ApiDeal) -> impl Stream<Item = Event> {
    async_stream::stream! {
        yield openai_content_event(&deal, "id", "model", 1, "content", true, false);
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
    wire: WireFormat,
    request_id: String,
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
    let mut visible_text_tokens = 0_u64;
    loop {
        let chunk = match driver.next().await {
            CanonStreamNext::Chunk(c) => c,
            CanonStreamNext::Usage(usage) => {
                if let Err(error) = request_guard.observe_answer_usage(usage) {
                    stream_error = Some(error.into());
                    break;
                }
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
        // Preserve the stable v2 route witness for every accepted canonical content chunk,
        // including hidden reasoning. The request-level visible lower bound remains text-only.
        let chunk_tokens = accounted_tokens(&chunk);
        if let Err(error) = deal.record_visible_output(chunk_tokens) {
            stream_error = Some(error.into());
            break;
        }
        if !chunk.text.is_empty() {
            visible_text_tokens = visible_text_tokens.saturating_add(chunk_tokens);
        }
        let before = driver.received();
        driver.account_rendered(&chunk);
        debug_assert!(driver.received() >= before);
        request_guard.record_delivery_progress(driver.received(), 0, driver.legacy_received());
        content.push_str(&chunk.text);
    }
    // Session-scoped: a clean completion / max_tokens does NOT STOP -- the deal lives for the next
    // request. ONLY a verification-bail ends the session early (STOP + bail off, B3/B10).
    let bailed = driver.bailed();
    let received = visible_text_tokens;
    let rendered_tokens = driver.legacy_received();
    let provider_output_tokens = driver.received();
    drop(driver);
    if !bailed && stream_error.is_none() {
        if let Some(usage) = billing_usage.as_ref() {
            if let Err(error) = request_guard.record_billing(&deal, *usage) {
                stream_error = Some(error.into());
            }
        }
    }
    let accepted_usage = if stream_error.is_none() && !bailed {
        billing_usage.as_ref()
    } else {
        None
    };
    let billing_exhausted = accepted_usage.is_some_and(|usage| {
        usage.billable_tokens() == billing_grant_tokens
            || usage.billable_tokens() < usage.total_tokens
    });
    // every terminal below records the same three figures, including the two that answer 502.
    // A request that failed after paying for output is exactly the one worth being able to attribute.
    let delivery = |request_guard: &ConsumerRequestGuard,
                    finish_reason: &'static str,
                    http_status: StatusCode,
                    failure_stage: Option<&'static str>,
                    visible_output_tokens: u64| {
        let request_usage = request_guard.request_usage();
        RequestDelivery {
            token_contract: deal.route.token_contract.clone(),
            request_id: request_id.clone(),
            protocol: wire.protocol(),
            streamed: false,
            http_status: http_status.as_u16(),
            failure_stage,
            billing_grant_tokens,
            output_limit_tokens: max_tokens,
            visible_output_tokens,
            rendered_tokens,
            route_delivered_tokens: Some(deal.route_visible_output_tokens()),
            input_tokens: accepted_usage.map(|usage| usage.input_tokens),
            output_tokens: accepted_usage.map(|usage| usage.output_tokens),
            billable_tokens: accepted_usage.map(|usage| usage.total_tokens),
            request_input_tokens: request_usage.input_tokens,
            request_output_tokens: request_usage.output_tokens,
            request_billable_tokens: request_usage.total_tokens,
            route_billable_tokens: deal.session.route_billable_tokens(),
            finish_reason,
            truncated_by_output_limit: capped,
            ended_before_output_limit: provider_output_tokens < max_tokens,
        }
    };
    if bailed {
        request_guard.observe_policy_failure(ClosureCategory::VerificationBail);
        deal.session.settle_verification_bail("verify-bail").await;
    } else if let Some(e) = stream_error {
        request_guard.observe_upstream_failure();
        if e.is_capacity() {
            request_guard.complete();
            let terminal = delivery(
                &request_guard,
                "capacity",
                StatusCode::OK,
                Some("upstream_capacity"),
                received,
            );
            let body = match wire {
                WireFormat::Chat => render::openai_completion(
                    &id,
                    &model,
                    created,
                    &content,
                    "capacity",
                    accepted_usage,
                ),
                WireFormat::Responses => render::responses_response(
                    &id,
                    &model,
                    created,
                    &content,
                    "capacity",
                    accepted_usage,
                )
                .to_string(),
            };
            return aggregate_body_response(request_guard, terminal, body);
        }
        if handle_stream_error_policy(&deal, received, &e, &mut request_guard).await
            == StreamErrorPolicyAction::RequestScoped
        {
            request_guard.complete();
        }
        let terminal = delivery(
            &request_guard,
            "error",
            StatusCode::BAD_GATEWAY,
            Some("upstream_stream"),
            0,
        );
        request_guard.report_delivery(terminal);
        if received == 0 && e.is_retryable_upstream() {
            return e
                .retryable_rejection()
                .response(StatusCode::BAD_GATEWAY, false);
        }
        return reject(StatusCode::BAD_GATEWAY, &format!("stream error: {e}"));
    } else if received == 0 && !capped {
        request_guard.observe_policy_failure(ClosureCategory::EmptyStream);
        let heartbeat = deal.accepted_output_guard();
        deal.session
            .settle_empty_stream("empty-output", &heartbeat)
            .await;
        let terminal = delivery(
            &request_guard,
            "empty_output",
            StatusCode::BAD_GATEWAY,
            Some("empty_output"),
            0,
        );
        request_guard.report_delivery(terminal);
        return reject(
            StatusCode::BAD_GATEWAY,
            "upstream produced no assistant text",
        );
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
    let terminal = delivery(
        &request_guard,
        finish_reason,
        StatusCode::OK,
        bailed.then_some("inline_verification"),
        received,
    );
    let body = match wire {
        WireFormat::Chat => render::openai_completion(
            &id,
            &model,
            created,
            &content,
            finish_reason,
            accepted_usage,
        ),
        WireFormat::Responses => render::responses_response(
            &id,
            &model,
            created,
            &content,
            finish_reason,
            accepted_usage,
        )
        .to_string(),
    };
    aggregate_body_response(request_guard, terminal, body)
}

fn reject(status: StatusCode, message: &str) -> Response {
    let body =
        serde_json::json!({ "error": { "message": message, "type": "invalid_request_error" } });
    (status, Json(body)).into_response()
}

fn is_unsupported_reasoning(error: &anyhow::Error) -> bool {
    error.downcast_ref::<tonic::Status>().is_some_and(|status| {
        status.code() == tonic::Code::InvalidArgument
            && status.message().starts_with("unsupported_parameter:")
    })
}

fn reject_unsupported_reasoning(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": "unsupported_parameter"
            }
        })),
    )
        .into_response()
}

fn deal_init_rejection(error: &DealInitError) -> Response {
    error.response(false)
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

fn responses_id(created: u64) -> String {
    format!("resp-dexdo-{created}-{:032x}", rand::random::<u128>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_2094_header_priority_and_invalid_text() {
        let mut headers = HeaderMap::new();
        assert_eq!(session_id(&headers).unwrap(), None);
        headers.insert("conversation_id", "conversation".parse().unwrap());
        assert_eq!(
            session_id(&headers).unwrap().as_deref(),
            Some("conversation")
        );
        headers.insert("session_id", "session".parse().unwrap());
        assert_eq!(session_id(&headers).unwrap().as_deref(), Some("session"));
        headers.insert(
            "session_id",
            http::HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(
            session_id(&headers).is_err(),
            "invalid primary header must not fall back"
        );
        headers.remove("session_id");
        headers.insert(
            "conversation_id",
            http::HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(session_id(&headers).is_err());
    }
    #[test]
    fn responses_created_in_the_same_second_have_distinct_response_and_message_ids() {
        let first = responses_id(42);
        let second = responses_id(42);
        assert_ne!(first, second);
        assert_ne!(format!("msg_{first}"), format!("msg_{second}"));
    }
}
