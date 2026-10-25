//! Optional Anthropic-compatible endpoint: `POST /v1/messages`,
//! a **local transcode** to the same `CanonRequest` (OpenAI shape) and back `CanonChunk` ->
//! Anthropic-SSE, for Anthropic-native clients. The transcode is off-chain, on the
//! buyer side: the wire (gRPC) and the canonical format are not touched.

use crate::buyer::api::stream::{CanonStreamDriver, CanonStreamNext};
use crate::buyer::api::{
    accounted_tokens, aggregate_body_response, attach_request_id, cap_canon_to_grant,
    capacity_rejection, consumer_request_id, handle_stream_error_policy, is_capacity_backpressure,
    report_pre_answer_failure, ApiDeal, ApiState, ClosureCategory, ConsumerError,
    ConsumerErrorCode, ConsumerRequestGuard, DeadGatewayAction, DealInitError, RequestDelivery,
    RouteBudget, StreamErrorPolicyAction,
};
use crate::buyer::render::{self, AnthropicRequest};
use axum::extract::{rejection::JsonRejection, State};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dexdo_core::params::BUYER_UPSTREAM_OPEN_RETRIES;
use dexdo_proto::{BillingUsage, CanonChunk};
use futures::{Stream, StreamExt};
use http::StatusCode;
use std::convert::Infallible;
use std::time::{SystemTime, UNIX_EPOCH};

/// `POST /v1/messages` (B20). Transcodes the request -> `CanonRequest`, opens the same
/// authorized TLS gRPC stream and re-renders `CanonChunk` into Anthropic-SSE
/// (`message_start` -> `content_block_*` -> `message_delta` -> `message_stop`).
pub async fn messages(
    State(state): State<ApiState>,
    req: Result<Json<AnthropicRequest>, JsonRejection>,
) -> Response {
    let req = match req {
        Ok(Json(req)) => req,
        Err(error) => return super::error::json_rejection(error, true),
    };
    let request_id = consumer_request_id();
    let response = messages_with_id(state, req, request_id.clone()).await;
    attach_request_id(response, &request_id)
}

async fn messages_with_id(state: ApiState, req: AnthropicRequest, request_id: String) -> Response {
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
    if let Some(response) = deal.session.closed_rejection(&request_id, true) {
        return response;
    }
    let serving_state = match deal.session.ensure_open_for_serving().await {
        Ok(state) => state,
        Err(error) => return error.response(StatusCode::BAD_GATEWAY, true),
    };
    let stream = req.stream;
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
        RouteBudget::Exhausted(error) => return state.budget_rejection(error, true),
    }
    if let Err(error) = request_guard.arm_delivery(
        state.delivery_events.clone(),
        &deal,
        request_id.clone(),
        "anthropic",
        stream,
        requested_max_tokens.map(u64::from).unwrap_or(0),
    ) {
        tracing::error!(%error, "buyer usage: could not bind admitted request");
        return super::usage::unavailable("usage history could not be bound to this deal");
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
                .closed_rejection(&request_id, true)
                .unwrap_or_else(|| error.response(StatusCode::SERVICE_UNAVAILABLE, true))
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
            "anthropic",
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
            "anthropic",
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
        .response(StatusCode::SERVICE_UNAVAILABLE, true);
    }
    let mut canon = render::anthropic_to_canon(req);
    // the grant may still hold what verification did not spend. The answer is bounded by what
    // was actually asked for, on the wire and on the way back alike.
    let max_tokens = cap_canon_to_grant(&mut canon, billing_grant_tokens);
    request_guard.set_delivery_output_limit(max_tokens);
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
                report_pre_answer_failure(
                    &deal,
                    &mut request_guard,
                    &request_id,
                    "anthropic",
                    stream,
                    max_tokens,
                    StatusCode::TOO_MANY_REQUESTS.as_u16(),
                    "upstream_open",
                );
                return capacity_rejection(&error).response(StatusCode::TOO_MANY_REQUESTS, true);
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
            request_guard.observe_policy_failure(ClosureCategory::DeadGateway);
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
            report_pre_answer_failure(
                &deal,
                &mut request_guard,
                &request_id,
                "anthropic",
                stream,
                max_tokens,
                StatusCode::BAD_GATEWAY.as_u16(),
                "upstream_open",
            );
            return reject(StatusCode::BAD_GATEWAY, &reason);
        }
    };

    // Session-scoped: no per-request STOP -- the shared session settles once at session end / on a
    // verification-bail (as in the OpenAI path).
    let mock_model = state.mock_model;
    if stream {
        #[cfg(test)]
        let test_delivery_finalizer = request_guard.delivery_finalizer();
        request_guard.mark_response_started();
        let response = sse_response(
            upstream,
            id,
            model,
            max_tokens,
            billing_grant_tokens,
            mock_model,
            deal,
            request_guard,
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
            max_tokens,
            billing_grant_tokens,
            mock_model,
            deal,
            request_guard,
            request_id,
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
        let mut capped = false;
        let mut stream_error = None;
        let mut billing_usage: Option<BillingUsage> = None;
        let mut visible_text_tokens = 0_u64;
        yield Ok((event(render::anthropic_message_start(&id, &model)), false));
        yield Ok((event(render::anthropic_content_block_start()), false));
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
                yield Ok((anthropic_content_event(&deal, &chunk.text), false));
            }
        }
        // Session-scoped: completion / max_tokens / upstream-error do NOT STOP -- only a
        // verification-bail ends the session early (STOP + bail off). `errored` still drives stop_reason below.
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
            if handle_stream_error_policy(&deal, provider_output_tokens, e, &mut request_guard).await
                == StreamErrorPolicyAction::RequestScoped
            {
                request_guard.complete();
            }
        } else if received == 0 && !capped {
            request_guard.observe_policy_failure(ClosureCategory::EmptyStream);
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
        // Legacy sellers use their full grant as the exhaustion signal. Shared-budget payment
        // allocation does not describe whether generation ended or hit its output cap.
        let billing_exhausted = accepted_usage.is_some_and(|usage| {
            usage.charged_tokens.is_none() && usage.total_tokens == billing_grant_tokens
        });
        // stop_reason does NOT pass off a bail/error as an honest `end_turn` -- bail -> `refusal`,
        // transport error -> `error`, otherwise `end_turn`.

        // `max_tokens` used to require `received == 0`, so a grant that cut the answer after
        // some of it had already been rendered reported `end_turn` -- indistinguishable from a model
        // that finished. The cap ends the answer at any `received`, not only at zero.
        let stop_reason = if bailed {
            "refusal"
        } else if capped || billing_exhausted {
            "max_tokens"
        } else if stream_error.is_some() || received == 0 {
            "error"
        } else {
            "end_turn"
        };
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
                protocol: "anthropic",
                streamed: true,
                http_status: StatusCode::OK.as_u16(),
                failure_stage,
                billing_grant_tokens,
                output_limit_tokens: max_tokens,
                visible_output_tokens: received,
                rendered_tokens,
                route_delivered_tokens: Some(deal.route_visible_output_tokens()),
                input_tokens: accepted_usage.map(|usage| usage.input_tokens),
                output_tokens: accepted_usage.map(|usage| usage.output_tokens),
                billable_tokens: accepted_usage.map(|usage| usage.total_tokens),
                request_input_tokens: request_usage.input_tokens,
                request_output_tokens: request_usage.output_tokens,
                request_billable_tokens: request_usage.total_tokens,
                route_billable_tokens: deal.session.route_billable_tokens(),
                finish_reason: stop_reason,
                truncated_by_output_limit: capped,
                ended_before_output_limit: provider_output_tokens < max_tokens,
            },
        );
        if received == 0 {
            if let Some(error) = stream_error.as_ref().filter(|error| error.is_retryable_upstream()) {
                yield Ok((Event::default().event("error").data(error.retryable_rejection().body(true).to_string()), false));
            }
        }
        yield Ok((event(render::anthropic_content_block_stop()), false));
        yield Ok((event(render::anthropic_message_delta(stop_reason)), false));
        yield Ok((event(render::anthropic_message_stop()), true));
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
    // Session-scoped: a clean completion / max_tokens does NOT STOP -- only a verification-bail ends
    // the session early (STOP + bail off).
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
        usage.charged_tokens.is_none() && usage.total_tokens == billing_grant_tokens
    });
    // every terminal below records the same three figures, including the two that answer 502.
    let delivery = |request_guard: &ConsumerRequestGuard,
                    stop_reason: &'static str,
                    http_status: StatusCode,
                    failure_stage: Option<&'static str>,
                    visible_output_tokens: u64| {
        let request_usage = request_guard.request_usage();
        RequestDelivery {
            token_contract: deal.route.token_contract.clone(),
            request_id: request_id.clone(),
            protocol: "anthropic",
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
            finish_reason: stop_reason,
            truncated_by_output_limit: capped,
            ended_before_output_limit: provider_output_tokens < max_tokens,
        }
    };
    if bailed {
        request_guard.observe_policy_failure(ClosureCategory::VerificationBail);
        deal.session.settle_verification_bail("verify-bail").await;
    } else if let Some(e) = stream_error {
        request_guard.observe_upstream_failure();
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
                .response(StatusCode::BAD_GATEWAY, true);
        }
        return reject(StatusCode::BAD_GATEWAY, &format!("stream error: {e}"));
    } else if received == 0 && !capped {
        request_guard.observe_policy_failure(ClosureCategory::EmptyStream);
        let heartbeat = deal.accepted_output_guard();
        deal.session
            .settle_empty_stream("empty-stream", &heartbeat)
            .await;
        let terminal = delivery(
            &request_guard,
            "error",
            StatusCode::BAD_GATEWAY,
            Some("empty_output"),
            0,
        );
        request_guard.report_delivery(terminal);
        return reject(StatusCode::BAD_GATEWAY, "upstream produced an empty stream");
    }
    request_guard.complete();
    // verification bail -> `refusal` (distinguishable from an honest `end_turn`).
    // `max_tokens` no longer requires `received == 0` -- a grant that cut the answer after
    // part of it had been aggregated reported `end_turn`, which is what a finished model reports.
    let stop_reason = if bailed {
        "refusal"
    } else if capped || billing_exhausted {
        "max_tokens"
    } else {
        "end_turn"
    };
    let terminal = delivery(
        &request_guard,
        stop_reason,
        StatusCode::OK,
        bailed.then_some("inline_verification"),
        received,
    );
    let body = serde_json::json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{ "type": "text", "text": content }],
        "stop_reason": stop_reason,
        "stop_sequence": null
    });
    aggregate_body_response(
        request_guard,
        terminal,
        serde_json::to_string(&body).unwrap(),
    )
}

fn reject(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "type": "error", "error": { "type": "invalid_request_error", "message": message } });
    (status, Json(body)).into_response()
}

fn deal_init_rejection(error: &DealInitError) -> Response {
    error.response(true)
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
