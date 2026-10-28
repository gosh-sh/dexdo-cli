//! The consumer endpoints (B19/B20): `POST /v1/chat/completions`, `/v1/responses` and
//! `/v1/messages` reach the deal's seller as received, and the provider's answer -- status, headers
//! and bytes -- returns to the client as produced. `GET /v1/models` names the frame model.

//! The buyer reads its copy of the answer only to account it and to check the model it names
//! ([`ModelCheck`]). It writes into a response only when the seller's stream breaks or the check
//! bails mid-answer: then it ends the answer in the client's format.

use super::stream::{FrameDriver, FrameNext, StreamError, StreamFacts};
use super::{
    aggregate_body_response, attach_request_id, capacity_rejection, consumer_request_id,
    handle_stream_error_policy, is_capacity_backpressure, report_pre_answer_failure, ApiDeal,
    ApiState, ClosureCategory, ConsumerError, ConsumerErrorCode, ConsumerRequestGuard,
    DeadGatewayAction, DealInitError, RequestDelivery, RouteBudget, StreamErrorPolicyAction,
};
use crate::api_format::{
    client_response_headers, forwarded_request_headers, inspect_body, ApiFormat, RequestBody,
    ResponseFacts,
};
use crate::buyer::verify::{ModelCheck, Verdict};
use crate::buyer::SellerApiMismatch;
use axum::body::{Body, Bytes};
use axum::extract::{rejection::BytesRejection, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dexdo_core::params::{BUYER_UPSTREAM_OPEN_RETRIES, UPSTREAM_RESPONSE_BODY_MAX_BYTES};
use dexdo_proto::{BillingUsage, Header, ResponseHead};
use http::{HeaderMap, StatusCode};
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

/// `POST /v1/chat/completions`.
pub async fn chat_completions(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(state, ApiFormat::ChatCompletions, headers, body).await
}

/// `POST /v1/responses`.
pub async fn responses(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(state, ApiFormat::Responses, headers, body).await
}

/// `POST /v1/messages`.
pub async fn messages(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(state, ApiFormat::Messages, headers, body).await
}

async fn handle(
    state: ApiState,
    format: ApiFormat,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let request_id = consumer_request_id();
    let response = handle_with_id(state, format, headers, body, request_id.clone()).await;
    attach_request_id(response, &request_id)
}

/// `application/json` or an `application/*+json` type, the media types the JSON extractor accepts.
fn json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || essence
            .strip_prefix("application/")
            .is_some_and(|subtype| subtype.ends_with("+json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// An error answer in the client's format (`anthropic` = the Messages shape).
pub(super) fn reject(status: StatusCode, message: &str, anthropic: bool) -> Response {
    let mut body =
        serde_json::json!({ "error": { "message": message, "type": "invalid_request_error" } });
    if anthropic {
        body["type"] = "error".into();
    }
    (status, Json(body)).into_response()
}

fn deal_init_rejection(error: &DealInitError, anthropic: bool) -> Response {
    error.response(anthropic)
}

fn mismatch_rejection(mismatch: &SellerApiMismatch, anthropic: bool) -> Response {
    match mismatch {
        SellerApiMismatch::Outdated => ConsumerError::new(
            ConsumerErrorCode::SellerUpdateRequired,
            mismatch.to_string(),
        )
        .response(StatusCode::BAD_GATEWAY, anthropic),
        SellerApiMismatch::PathUnsupported { .. } => {
            ConsumerError::new(ConsumerErrorCode::UnsupportedApiPath, mismatch.to_string())
                .response(StatusCode::BAD_REQUEST, anthropic)
        }
    }
}

/// The seller refused the request itself (`InvalidArgument`): it is reachable and correct, so the
/// refusal is request-scoped.
fn seller_request_refusal(error: &anyhow::Error) -> Option<&tonic::Status> {
    error
        .downcast_ref::<tonic::Status>()
        .filter(|status| status.code() == tonic::Code::InvalidArgument)
}

async fn handle_with_id(
    state: ApiState,
    format: ApiFormat,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
    request_id: String,
) -> Response {
    let anthropic = format == ApiFormat::Messages;
    // A JSON content type is what makes a browser preflight a cross-site request to this loopback
    // endpoint; a simple `text/plain` POST from a web page must not reach the deal.
    if !json_content_type(&headers) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected request with `Content-Type: application/json`",
        )
            .into_response();
    }
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(error) => return super::error::bytes_rejection(error, anthropic),
    };
    let request = match RequestBody::parse(&bytes) {
        Ok(request) => request,
        Err(reason) => return reject(StatusCode::BAD_REQUEST, &reason, anthropic),
    };
    // The model is forced by the market (B2/B19): a request must name the frame's model.
    let Some(model) = request.model() else {
        return reject(
            StatusCode::BAD_REQUEST,
            "request model is required",
            anthropic,
        );
    };
    if let Err(reason) = state.check_model(model) {
        return reject(StatusCode::BAD_REQUEST, &reason, anthropic);
    }
    let stream = request.stream();
    let forwarded_headers: Vec<Header> = forwarded_request_headers(
        headers
            .iter()
            .map(|(name, value)| Header {
                name: name.as_str().to_string(),
                value: value.as_bytes().to_vec(),
            })
            .collect(),
    );
    let deal = match state.current_deal().await {
        Ok(deal) => deal,
        Err(error) => return deal_init_rejection(&error, anthropic),
    };
    // Valid requests still record demand when a failed/closed route needs renewal.
    // Beginning the guard does not reserve capacity or run billable identity probes.
    let mut request_guard = deal.begin_request(now_secs());
    if let Some(response) = deal.session.closed_rejection(&request_id, anthropic) {
        return response;
    }
    // No chain read on the request path: the handover routing this request was written for
    // this deal alone, and a seller that no longer serves it fails the capability query below.
    if let Some(error) = deal.session.unresolved_usage_rejection() {
        return error.response(StatusCode::BAD_GATEWAY, anthropic);
    }
    let retry_limit = if deal.session.dead_gateway_action() == DeadGatewayAction::RetryThenReclaim {
        BUYER_UPSTREAM_OPEN_RETRIES
    } else {
        0
    };
    // Money-free capability check, before admission and the billable identity probes: the seller
    // must serve this path. Transport failures obey the same retry and guarded recovery policy as a
    // stream-open failure; a seller that cannot serve the path leaves the deal untouched.
    let capability_heartbeat = deal.accepted_output_guard();
    request_guard.arm_upstream_failure();
    let mut retries = 0usize;
    loop {
        let error = match state
            .buyer
            .check_seller_serves(
                &deal.route.handover,
                &deal.route.token_contract,
                format.path(),
            )
            .await
        {
            Ok(()) => break,
            Err(error) => error,
        };
        if let Some(mismatch) = error.downcast_ref::<SellerApiMismatch>() {
            request_guard.complete();
            return mismatch_rejection(mismatch, anthropic);
        }
        if retries < retry_limit {
            retries += 1;
            tracing::warn!(
                error = %error,
                token_contract = %super::display_token_contract(&deal.route.token_contract),
                "consumer API: capability query failed; retrying once per dead_gateway=retry_then_reclaim"
            );
            continue;
        }
        request_guard.observe_policy_failure(ClosureCategory::DeadGateway);
        let reason =
            settle_upstream_open_failure(&deal, &error, retries, capability_heartbeat).await;
        return reject(StatusCode::BAD_GATEWAY, &reason, anthropic);
    }
    request_guard.complete();
    // Session-scoped lifecycle: once the local deal is closed (terminal settlement landed or policy
    // recovery is pending) no new request may open a stream on the closed deal.
    if let Some(response) = deal.session.closed_rejection(&request_id, anthropic) {
        return response;
    }
    // Authorize payment from the shared remainder; the seller enforces per-deal concurrency
    // and allocates actual charges atomically. Subscription boundary reconciliation stays here.
    match deal
        .admit_with_probe_observation(deal.probe_accepted())
        .await
    {
        RouteBudget::Admitted(reservation) => {
            request_guard.hold(reservation);
        }
        RouteBudget::Exhausted(error) => return state.budget_rejection(error, anthropic),
    }
    if let Err(error) = request_guard.arm_delivery(
        state.delivery_events.clone(),
        &deal,
        request_id.clone(),
        format.protocol(),
        stream,
    ) {
        tracing::error!(%error, "buyer usage: could not bind admitted request");
        return super::usage::unavailable("usage history could not be bound to this deal");
    }
    // one-per-deal content-identity gate (B8 + B7-full), run ONCE before the first paid stream. The
    // model check of the forwarded answer below only reads the model the provider names; a seller naming
    // the correct model while serving a cheaper one is caught only here. On a bail the gate closes the
    // deal and attempts policy recovery; a transport error is not cached, so a later request retries.

    // this runs after admission and inside the reservation it granted. Verification is
    // billable service on this deal, so a request with no quota must not reach it, and its total comes
    // out of the same grant as the answer.
    let verdict = deal
        .content_gate
        .ensure_verified(&state.buyer, &deal, &mut request_guard, &state.frame_model)
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
                .closed_rejection(&request_id, anthropic)
                .unwrap_or_else(|| error.response(StatusCode::SERVICE_UNAVAILABLE, anthropic))
        } else if let Some(mismatch) = reason.downcast_ref::<SellerApiMismatch>() {
            mismatch_rejection(mismatch, anthropic)
        } else {
            reject(
                StatusCode::BAD_GATEWAY,
                &format!("model identity verification failed (content check): {reason}"),
                anthropic,
            )
        };
        report_pre_answer_failure(
            &deal,
            &mut request_guard,
            &request_id,
            format.protocol(),
            stream,
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
            format.protocol(),
            stream,
            StatusCode::SERVICE_UNAVAILABLE.as_u16(),
            "content_verification_budget",
        );
        return ConsumerError::new(
            ConsumerErrorCode::VerificationGrantConsumed,
            "the identity verification this deal owed consumed the whole admitted grant; the \
             request was not sent and the next one starts from a verified deal",
        )
        .response(StatusCode::SERVICE_UNAVAILABLE, anthropic);
    }
    let reclaim_heartbeat = deal.accepted_output_guard();
    request_guard.arm_upstream_failure();

    // Open an authorized TLS gRPC stream to the seller carrying the request as received.
    let mut retries = 0usize;
    let upstream = loop {
        match state
            .buyer
            .open_request_stream(
                &deal.route.handover,
                &deal.route.token_contract,
                format.path(),
                bytes.to_vec(),
                forwarded_headers.clone(),
                billing_grant_tokens,
            )
            .await
        {
            Ok(stream) => break Ok(stream),
            Err(error)
                if retries < retry_limit
                    && seller_request_refusal(&error).is_none()
                    && error.downcast_ref::<SellerApiMismatch>().is_none()
                    && !is_capacity_backpressure(&error) =>
            {
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
            let (status, response) = if let Some(refusal) = seller_request_refusal(&error) {
                // The seller answered, and refused this request: request-scoped.
                request_guard.complete();
                (
                    StatusCode::BAD_REQUEST,
                    reject(StatusCode::BAD_REQUEST, refusal.message(), anthropic),
                )
            } else if let Some(mismatch) = error.downcast_ref::<SellerApiMismatch>() {
                request_guard.complete();
                let response = mismatch_rejection(mismatch, anthropic);
                (response.status(), response)
            } else if is_capacity_backpressure(&error) {
                // The seller answered, and its answer was "not yet" -- request-scoped, not a dead
                // gateway. The local caller retries; the deal is left alone.
                request_guard.complete();
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    capacity_rejection(&error).response(StatusCode::TOO_MANY_REQUESTS, anthropic),
                )
            } else {
                request_guard.observe_policy_failure(ClosureCategory::DeadGateway);
                let reason =
                    settle_upstream_open_failure(&deal, &error, retries, reclaim_heartbeat).await;
                (
                    StatusCode::BAD_GATEWAY,
                    reject(StatusCode::BAD_GATEWAY, &reason, anthropic),
                )
            };
            report_pre_answer_failure(
                &deal,
                &mut request_guard,
                &request_id,
                format.protocol(),
                stream,
                status.as_u16(),
                "upstream_open",
            );
            return response;
        }
    };

    let check = ModelCheck::new(&state.frame_model, deal.content_gate.models());
    let mut driver = FrameDriver::new(
        upstream,
        deal.session.closed_receiver(),
        format,
        stream,
        check,
        billing_grant_tokens,
    );
    // The provider's status and headers come first; nothing reaches the client before them.
    let head = match driver.head().await {
        Ok(head) => head,
        Err(error) => {
            return pre_head_failure(&deal, request_guard, &request_id, format, stream, error)
                .await;
        }
    };
    let context = AnswerContext {
        deal,
        request_id,
        format,
        stream,
        billing_grant_tokens,
        frame_model: state.frame_model.clone(),
    };
    if stream && (200..300).contains(&head.status) {
        #[cfg(test)]
        let test_delivery_finalizer = request_guard.delivery_finalizer();
        request_guard.mark_response_started();
        let response = streamed_answer(context, head, driver, request_guard);
        #[cfg(test)]
        let mut response = response;
        #[cfg(test)]
        if let Some(finalizer) = test_delivery_finalizer {
            response.extensions_mut().insert(finalizer);
        }
        response
    } else {
        whole_answer(context, head, driver, request_guard).await
    }
}

// Capability queries and stream opens share the existing guarded dead-gateway
// policy and diagnostics.
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

struct AnswerContext {
    deal: ApiDeal,
    request_id: String,
    format: ApiFormat,
    stream: bool,
    billing_grant_tokens: u64,
    frame_model: String,
}

impl AnswerContext {
    #[allow(clippy::too_many_arguments)]
    fn delivery(
        &self,
        request_guard: &ConsumerRequestGuard,
        accepted_usage: Option<&BillingUsage>,
        finish_reason: String,
        http_status: u16,
        failure_stage: Option<&'static str>,
        visible_output_tokens: u64,
        rendered_tokens: u64,
    ) -> RequestDelivery {
        let request_usage = request_guard.request_usage();
        RequestDelivery {
            token_contract: self.deal.route.token_contract.clone(),
            request_id: self.request_id.clone(),
            protocol: self.format.protocol(),
            streamed: self.stream,
            http_status,
            failure_stage,
            billing_grant_tokens: self.billing_grant_tokens,
            visible_output_tokens,
            rendered_tokens,
            route_delivered_tokens: Some(self.deal.route_visible_output_tokens()),
            input_tokens: accepted_usage.map(|usage| usage.input_tokens),
            output_tokens: accepted_usage.map(|usage| usage.output_tokens),
            billable_tokens: accepted_usage.map(|usage| usage.total_tokens),
            request_input_tokens: request_usage.input_tokens,
            request_output_tokens: request_usage.output_tokens,
            request_billable_tokens: request_usage.total_tokens,
            route_billable_tokens: self.deal.session.route_billable_tokens(),
            finish_reason,
        }
    }

    /// The end reason a bail writes in the client's format.
    fn bail_reason(&self) -> &'static str {
        if self.format == ApiFormat::Messages {
            "refusal"
        } else {
            "content_filter"
        }
    }
}

/// The seller's stream failed before the provider answered: the client receives an error, as the
/// seller had no answer to forward.
async fn pre_head_failure(
    deal: &ApiDeal,
    mut request_guard: ConsumerRequestGuard,
    request_id: &str,
    format: ApiFormat,
    stream: bool,
    error: StreamError,
) -> Response {
    let anthropic = format == ApiFormat::Messages;
    request_guard.observe_upstream_failure();
    if error.is_capacity() {
        request_guard.complete();
        report_pre_answer_failure(
            deal,
            &mut request_guard,
            request_id,
            format.protocol(),
            stream,
            StatusCode::TOO_MANY_REQUESTS.as_u16(),
            "upstream_capacity",
        );
        return ConsumerError::new(
            ConsumerErrorCode::DealCapacityUnavailable,
            format!("the deal has no delivery capacity for this request yet; retry after seller capacity reconciliation: {error}"),
        )
        .response(StatusCode::TOO_MANY_REQUESTS, anthropic);
    }
    if handle_stream_error_policy(deal, 0, &error, &mut request_guard).await
        == StreamErrorPolicyAction::RequestScoped
    {
        request_guard.complete();
    }
    report_pre_answer_failure(
        deal,
        &mut request_guard,
        request_id,
        format.protocol(),
        stream,
        StatusCode::BAD_GATEWAY.as_u16(),
        "upstream_stream",
    );
    if error.is_retryable_upstream() {
        return error
            .retryable_rejection()
            .response(StatusCode::BAD_GATEWAY, anthropic);
    }
    reject(
        StatusCode::BAD_GATEWAY,
        &format!("stream error: {error}"),
        anthropic,
    )
}

/// The provider's response headers, as the client receives them.
pub(super) fn response_headers(head_headers: &[Header], content_type: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for header in client_response_headers(head_headers) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(header.name.as_bytes()),
            http::HeaderValue::from_bytes(&header.value),
        ) {
            headers.append(name, value);
        }
    }
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(content_type),
    );
    headers
}

/// A streamed answer: provider frames reach the client as they arrive.
fn streamed_answer(
    context: AnswerContext,
    head: ResponseHead,
    mut driver: FrameDriver,
    mut request_guard: ConsumerRequestGuard,
) -> Response {
    let delivery_finalizer = request_guard.delivery_finalizer();
    let status =
        StatusCode::from_u16(u16::try_from(head.status).unwrap_or(500)).unwrap_or(StatusCode::OK);
    let headers = response_headers(&head.headers, "text/event-stream");
    let body = async_stream::stream! {
        let deal = &context.deal;
        let mut stream_error: Option<StreamError> = None;
        let mut bailed = None;
        let mut billing_usage: Option<BillingUsage> = None;
        let mut visible = 0_u64;
        let mut rendered = 0_u64;
        loop {
            match driver.next().await {
                FrameNext::Data { bytes, output, text } => {
                    if output > 0 {
                        if let Err(error) = deal.record_visible_output(output) {
                            stream_error = Some(error.into());
                            break;
                        }
                        deal.record_accepted_output(now_secs());
                    }
                    rendered = rendered.saturating_add(output);
                    visible = visible.saturating_add(text);
                    request_guard.record_delivery_progress(rendered, visible, rendered);
                    if !bytes.is_empty() {
                        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(bytes));
                    }
                }
                FrameNext::Usage(usage) => {
                    if rendered == 0 {
                        stream_error = Some(
                            "terminal billing usage has no preceding delivered output"
                                .to_string()
                                .into(),
                        );
                        break;
                    }
                    if let Err(error) = request_guard.observe_answer_usage(usage) {
                        stream_error = Some(error.into());
                        break;
                    }
                    billing_usage = Some(usage);
                }
                FrameNext::Bailed(reason) => {
                    tracing::warn!(%reason, "verify: bail -- bailing off the stream (B10)");
                    bailed = Some(reason);
                    break;
                }
                FrameNext::Errored(error) => {
                    stream_error = Some(error);
                    break;
                }
                FrameNext::End => break,
            }
        }
        let facts: StreamFacts = driver.facts();
        let held = driver.take_held();
        drop(driver);
        if bailed.is_none() && stream_error.is_none() {
            if let Some(usage) = billing_usage.as_ref() {
                if let Err(error) = request_guard.record_billing(deal, *usage) {
                    stream_error = Some(error.into());
                }
            }
        }
        if bailed.is_some() {
            request_guard.observe_policy_failure(ClosureCategory::VerificationBail);
            deal.session.settle_verification_bail("verify-bail").await;
        } else if let Some(error) = &stream_error {
            request_guard.observe_upstream_failure();
            if error.is_capacity()
                || handle_stream_error_policy(deal, rendered, error, &mut request_guard).await
                    == StreamErrorPolicyAction::RequestScoped
            {
                request_guard.complete();
            }
        } else if visible == 0 {
            request_guard.observe_policy_failure(ClosureCategory::EmptyStream);
            let heartbeat = deal.accepted_output_guard();
            deal.session.settle_empty_stream("empty-output", &heartbeat).await;
        } else {
            request_guard.complete();
        }
        let accepted_usage = if stream_error.is_none() && bailed.is_none() {
            billing_usage.as_ref()
        } else {
            None
        };
        // A capacity refusal ends Chat Completions and Responses as `capacity`; Messages has no such
        // reason and ends as `error`.
        let capacity = stream_error.as_ref().is_some_and(StreamError::is_capacity)
            && context.format != ApiFormat::Messages;
        let finish_reason = if bailed.is_some() {
            context.bail_reason().to_string()
        } else if capacity {
            "capacity".to_string()
        } else if stream_error.is_some() {
            "error".to_string()
        } else if visible == 0 {
            "empty_output".to_string()
        } else {
            facts.finish_reason.clone().unwrap_or_else(|| "stop".to_string())
        };
        let failure_stage = if bailed.is_some() {
            Some("inline_verification")
        } else if stream_error.is_some() {
            Some("upstream_stream")
        } else if visible == 0 {
            Some("empty_output")
        } else {
            None
        };
        request_guard.prepare_delivery(context.delivery(
            &request_guard,
            accepted_usage,
            finish_reason.clone(),
            StatusCode::OK.as_u16(),
            failure_stage,
            visible,
            rendered,
        ));
        // A broken or bailed answer is ended in the client's format; a clean one ends with the
        // provider's own terminal frames, released once its bill is recorded.
        if bailed.is_none() && stream_error.is_none() {
            if let Some(finalizer) = delivery_finalizer.as_ref() {
                finalizer.report_prepared();
            }
            if !held.is_empty() {
                yield Ok(Bytes::from(held));
            }
        } else {
            let retryable = stream_error
                .as_ref()
                .filter(|error| rendered == 0 && error.is_retryable_upstream())
                .map(StreamError::retryable_rejection);
            let mut ending = if facts.unterminated {
                // Close the seller's unfinished frame so the ending parses as its own events.
                b"\n\n".to_vec()
            } else {
                Vec::new()
            };
            ending.extend(super::stream::break_frames(
                context.format,
                &facts,
                &context.frame_model,
                &finish_reason,
                retryable.as_ref(),
            ));
            yield Ok(Bytes::from(ending));
            if let Some(finalizer) = delivery_finalizer.as_ref() {
                finalizer.report_prepared();
            }
        }
        drop(request_guard);
    };
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// A non-streamed answer, or any provider error answer: collected whole, then returned as it is.
async fn whole_answer(
    context: AnswerContext,
    head: ResponseHead,
    mut driver: FrameDriver,
    mut request_guard: ConsumerRequestGuard,
) -> Response {
    let anthropic = context.format == ApiFormat::Messages;
    let deal = &context.deal;
    let success = (200..300).contains(&head.status);
    let check = ModelCheck::new(&context.frame_model, deal.content_gate.models());
    let mut body = Vec::new();
    let mut frames = 0usize;
    let mut facts: Option<ResponseFacts> = None;
    let mut output = 0;
    let mut stream_error: Option<StreamError> = None;
    let mut billing_usage: Option<BillingUsage> = None;
    loop {
        match driver.next().await {
            FrameNext::Data { bytes, .. } => {
                body.extend_from_slice(&bytes);
                frames += 1;
                if body.len() > UPSTREAM_RESPONSE_BODY_MAX_BYTES {
                    stream_error = Some(
                        "the seller's answer exceeds the buyer's response size limit"
                            .to_string()
                            .into(),
                    );
                    break;
                }
                // An honest seller sends a whole answer as one frame, so it is inspected on
                // arrival: it counts as received output even when the stream fails afterwards.
                // Any other body is inspected once, when the stream ends.
                if !success || frames > 1 {
                    continue;
                }
                match accept_whole_body(&context, &check, &body, &mut request_guard).await {
                    Ok(Some((complete, received))) => {
                        facts = Some(complete);
                        output = received;
                    }
                    Ok(None) => {}
                    Err(WholeBodyStop::Refused(response)) => return response,
                    Err(WholeBodyStop::Failed(error)) => {
                        stream_error = Some(error);
                        break;
                    }
                }
            }
            FrameNext::Usage(usage) => billing_usage = Some(usage),
            FrameNext::Errored(error) => {
                stream_error = Some(error);
                break;
            }
            FrameNext::Bailed(_) | FrameNext::End => break,
        }
    }
    drop(driver);
    let headers = head.headers.clone();
    // The provider's own error answer, relayed as it is. Nothing was delivered or billed. An error
    // body cut short or over the size limit is not the provider's answer and fails below.
    if !success && stream_error.is_none() {
        request_guard.complete();
        let delivery = context.delivery(
            &request_guard,
            None,
            "error".to_string(),
            u16::try_from(head.status).unwrap_or(502),
            Some("provider_error"),
            0,
            0,
        );
        return aggregate_body_response(request_guard, delivery, headers, body);
    }
    if success && facts.is_none() && frames > 1 && body.len() <= UPSTREAM_RESPONSE_BODY_MAX_BYTES {
        match accept_whole_body(&context, &check, &body, &mut request_guard).await {
            Ok(Some((complete, received))) => {
                facts = Some(complete);
                output = received;
            }
            Ok(None) => {}
            Err(WholeBodyStop::Refused(response)) => return response,
            Err(WholeBodyStop::Failed(error)) => {
                stream_error.get_or_insert(error);
            }
        }
    }
    // Usage the seller stated for that output is the seller's claim even when the stream then fails.
    if let Some(usage) = billing_usage.filter(|_| output > 0) {
        if let Err(error) = request_guard.observe_answer_usage(usage) {
            stream_error.get_or_insert(error.into());
        }
    }
    let facts = match (stream_error, facts) {
        (Some(error), _) => {
            return whole_answer_failure(&context, request_guard, error, output, anthropic).await;
        }
        (None, Some(facts)) => facts,
        (None, None) => {
            let error = match serde_json::from_slice::<serde_json::Value>(&body) {
                Err(error) => format!("provider response is not JSON: {error}"),
                Ok(_) => "provider response is not a complete answer".to_string(),
            };
            return whole_answer_failure(&context, request_guard, error.into(), 0, anthropic).await;
        }
    };
    let visible = u64::from(facts.has_visible_output());
    let usage = match billing_usage {
        Some(usage) if output > 0 => usage,
        Some(_) => {
            let error = "terminal billing usage has no preceding delivered output"
                .to_string()
                .into();
            return whole_answer_failure(&context, request_guard, error, output, anthropic).await;
        }
        None => {
            let error = "response stream ended without terminal billing usage"
                .to_string()
                .into();
            return whole_answer_failure(&context, request_guard, error, output, anthropic).await;
        }
    };
    if let Err(error) = request_guard.record_billing(deal, usage) {
        return whole_answer_failure(&context, request_guard, error.into(), output, anthropic)
            .await;
    }
    if visible == 0 {
        request_guard.observe_policy_failure(ClosureCategory::EmptyStream);
        let heartbeat = deal.accepted_output_guard();
        deal.session
            .settle_empty_stream("empty-output", &heartbeat)
            .await;
        let delivery = context.delivery(
            &request_guard,
            Some(&usage),
            "empty_output".to_string(),
            StatusCode::BAD_GATEWAY.as_u16(),
            Some("empty_output"),
            0,
            output,
        );
        request_guard.report_delivery(delivery);
        return reject(
            StatusCode::BAD_GATEWAY,
            "upstream produced no assistant text",
            anthropic,
        );
    }
    request_guard.complete();
    let delivery = context.delivery(
        &request_guard,
        Some(&usage),
        facts.finish_reason.unwrap_or_else(|| "stop".to_string()),
        u16::try_from(head.status).unwrap_or(200),
        None,
        visible,
        output,
    );
    aggregate_body_response(request_guard, delivery, headers, body)
}

/// Why inspecting a whole body ended the answer.
enum WholeBodyStop {
    /// The body named another model: the answer is refused and the deal settled.
    Refused(Response),
    /// The body's output could not be recorded.
    Failed(StreamError),
}

/// What a complete whole body states, once its model is checked and its output recorded; `None`
/// while the body is not yet a JSON value.
#[allow(clippy::result_large_err)]
async fn accept_whole_body(
    context: &AnswerContext,
    check: &ModelCheck,
    body: &[u8],
    request_guard: &mut ConsumerRequestGuard,
) -> Result<Option<(ResponseFacts, u64)>, WholeBodyStop> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Ok(None);
    };
    let deal = &context.deal;
    let facts = inspect_body(context.format, &value);
    if let Some(Verdict::Bail(reason)) = facts.model.as_deref().map(|model| check.verify(model)) {
        tracing::warn!(%reason, "verify: bail -- refusing the answer (B10)");
        request_guard.observe_policy_failure(ClosureCategory::VerificationBail);
        deal.session.settle_verification_bail("verify-bail").await;
        let delivery = context.delivery(
            request_guard,
            None,
            context.bail_reason().to_string(),
            StatusCode::BAD_GATEWAY.as_u16(),
            Some("inline_verification"),
            0,
            0,
        );
        request_guard.report_delivery(delivery);
        return Err(WholeBodyStop::Refused(reject(
            StatusCode::BAD_GATEWAY,
            &format!("model identity verification failed: {reason}"),
            context.format == ApiFormat::Messages,
        )));
    }
    let output = u64::from(facts.has_output());
    if output > 0 {
        deal.record_visible_output(output)
            .map_err(|error| WholeBodyStop::Failed(error.into()))?;
        deal.record_accepted_output(now_secs());
        request_guard.record_delivery_progress(output, 0, output);
    }
    Ok(Some((facts, output)))
}

async fn whole_answer_failure(
    context: &AnswerContext,
    mut request_guard: ConsumerRequestGuard,
    error: StreamError,
    received: u64,
    anthropic: bool,
) -> Response {
    let deal = &context.deal;
    request_guard.observe_upstream_failure();
    if error.is_capacity() {
        request_guard.complete();
        let delivery = context.delivery(
            &request_guard,
            None,
            "error".to_string(),
            StatusCode::TOO_MANY_REQUESTS.as_u16(),
            Some("upstream_capacity"),
            0,
            received,
        );
        request_guard.report_delivery(delivery);
        return ConsumerError::new(
            ConsumerErrorCode::DealCapacityUnavailable,
            format!("the deal has no delivery capacity for this request yet; retry after seller capacity reconciliation: {error}"),
        )
        .response(StatusCode::TOO_MANY_REQUESTS, anthropic);
    }
    if handle_stream_error_policy(deal, received, &error, &mut request_guard).await
        == StreamErrorPolicyAction::RequestScoped
    {
        request_guard.complete();
    }
    let delivery = context.delivery(
        &request_guard,
        None,
        "error".to_string(),
        StatusCode::BAD_GATEWAY.as_u16(),
        Some("upstream_stream"),
        0,
        received,
    );
    request_guard.report_delivery(delivery);
    if received == 0 && error.is_retryable_upstream() {
        return error
            .retryable_rejection()
            .response(StatusCode::BAD_GATEWAY, anthropic);
    }
    reject(
        StatusCode::BAD_GATEWAY,
        &format!("stream error: {error}"),
        anthropic,
    )
}
