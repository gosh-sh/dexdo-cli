//! Test-only seller seam for buyer-side adversarial stream fixtures.

//! Ordinary requests delegate to the production gateway unchanged. A request carrying an existing
//! adversarial-output marker takes a bare gRPC branch after authorization that deliberately skips
//! the seller reservation: it models a third-party seller whose stream misbehaves around its terminal
//! usage.

use crate::api_format::{ApiFormat, RequestBody};
use crate::seller::auth::AuthRegistry;
use crate::seller::gateway::{GatewayService, GatewayState};
use crate::seller::tls::GatewayTls;
use crate::seller::upstream::{mock, ProviderRequest, UpstreamConfig, UpstreamEvent};
use anyhow::{anyhow, Result};
use dexdo_core::note::{NotePubkey, Signature};
use dexdo_core::params::{GATEWAY_CLIENT_CHANNEL_CAPACITY, GATEWAY_UPSTREAM_CHANNEL_CAPACITY};
use dexdo_core::{DealChainState, DealSubscription};
use dexdo_proto::{
    BillingUsage, Challenge, ChallengeRequest, Gateway, GatewayServer, StreamFrame, StreamRequest,
};
use serde_json::Value;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::{Stream, StreamExt};
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

pub(super) struct RunningFixtureSeller {
    state: Arc<GatewayState>,
    fixture_auth: Arc<AuthRegistry>,
    pub(super) server_task: tokio::task::JoinHandle<()>,
    pub(super) listen_addr: SocketAddr,
    pub(super) tls: GatewayTls,
    delayed_eof_release: Arc<Semaphore>,
    held_open: Arc<Notify>,
    legacy_challenge: Arc<AtomicBool>,
    probe_output_fixture: Arc<Mutex<Option<(bool, &'static str)>>>,
    calls: Arc<AtomicUsize>,
    recovery_calls: Arc<AtomicUsize>,
}

impl RunningFixtureSeller {
    pub(super) async fn recv_upstream_failure(&self) -> crate::seller::gateway::UpstreamFailure {
        self.state.recv_upstream_failure().await.unwrap()
    }

    pub(super) async fn wait_for_held_open(&self) {
        self.held_open.notified().await;
    }

    pub(super) fn billed_tokens(&self, token_contract: &str) -> u64 {
        self.state
            .delivery(token_contract)
            .count
            .load(Ordering::SeqCst)
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(super) fn recovery_calls(&self) -> usize {
        self.recovery_calls.load(Ordering::SeqCst)
    }

    pub(super) fn set_probe_output_fixture(&self, greedy: bool, marker: &'static str) {
        *self.probe_output_fixture.lock().unwrap() = Some((greedy, marker));
    }
    pub(super) fn clear_probe_output_fixture(&self) {
        *self.probe_output_fixture.lock().unwrap() = None;
    }
    pub(super) fn set_concurrency(&self, limit: std::num::NonZeroU32) {
        self.state.set_max_concurrent_requests_per_deal(limit);
    }
    /// Answer challenges the way a seller that predates request forwarding does.
    pub(super) fn use_legacy_challenge(&self) {
        self.legacy_challenge.store(true, Ordering::SeqCst);
    }
    pub(super) fn register_stream(
        &self,
        token_contract: &str,
        buyer_pubkey: NotePubkey,
        mock_token_count: u64,
        state: DealChainState,
        deal: DealSubscription,
    ) -> Result<()> {
        self.state.register_stream(
            token_contract,
            buyer_pubkey.clone(),
            mock_token_count,
            state,
            deal,
        )?;
        self.fixture_auth.register(token_contract, buyer_pubkey);
        Ok(())
    }

    pub(super) fn release_delayed_eof(&self) {
        self.delayed_eof_release.add_permits(1);
    }
}

struct FixtureGateway {
    production: GatewayService,
    fixture_auth: Arc<AuthRegistry>,
    delayed_eof_release: Arc<Semaphore>,
    held_open: Arc<Notify>,
    legacy_challenge: Arc<AtomicBool>,
    probe_output_fixture: Arc<Mutex<Option<(bool, &'static str)>>>,
    calls: Arc<AtomicUsize>,
}

type FrameStream = Pin<Box<dyn Stream<Item = Result<StreamFrame, Status>> + Send>>;

fn body_text(request: &StreamRequest) -> String {
    String::from_utf8_lossy(&request.body).into_owned()
}

#[tonic::async_trait]
impl Gateway for FixtureGateway {
    async fn get_challenge(
        &self,
        mut request: Request<ChallengeRequest>,
    ) -> Result<Response<Challenge>, Status> {
        // The money-free capability query is not work the seller serves; `calls` counts the rest.
        if !request.get_ref().capabilities_only {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        let legacy = self.legacy_challenge.load(Ordering::SeqCst);
        if legacy {
            // An old seller ignores the additive request flag and still issues a normal nonce.
            request.get_mut().capabilities_only = false;
        }
        let token_contract = request.get_ref().token_contract.clone();
        let capabilities_only = request.get_ref().capabilities_only;
        let mut response = self.production.get_challenge(request).await?;
        if legacy {
            // An empty list is omitted by protobuf encoding, exactly the older seller's reply.
            response.get_mut().api_paths.clear();
        }
        if !capabilities_only {
            self.fixture_auth
                .issue_challenge(&token_contract, response.get_ref().nonce.clone());
        }
        Ok(response)
    }

    type OpenStreamStream = FrameStream;

    async fn open_stream(
        &self,
        request: Request<StreamRequest>,
    ) -> Result<Response<Self::OpenStreamStream>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // The B7 reference probe asks for a greedy decode; B8 and consumer requests do not.
        let greedy = RequestBody::parse(&request.get_ref().body)
            .is_ok_and(|body| body.0.get("temperature").and_then(Value::as_f64) == Some(0.0));
        let probe_marker = self
            .probe_output_fixture
            .lock()
            .unwrap()
            .filter(|(target, _)| *target == greedy)
            .map(|(_, marker)| marker);
        let adversarial_output =
            probe_marker.is_some() || is_adversarial_output_request(request.get_ref());
        authorize_fixture_request(&self.fixture_auth, request.get_ref())
            .map_err(|status| *status)?;
        if matches!(
            probe_marker,
            Some("DEXDO_FIXTURE_ERROR_BEFORE_OUTPUT" | "DEXDO_FIXTURE_TRANSPORT_BEFORE_OUTPUT")
        ) {
            let status = Status::unavailable("fixture failure before output");
            let status = if probe_marker == Some("DEXDO_FIXTURE_ERROR_BEFORE_OUTPUT") {
                dexdo_proto::upstream_failure_status(status, false)
            } else {
                status
            };
            return Ok(Response::new(Box::pin(tokio_stream::once(Err(status)))));
        }
        if !adversarial_output {
            let hold_open = body_text(request.get_ref()).contains("DEXDO_FIXTURE_HOLD_STREAM_OPEN");
            let hold_response =
                body_text(request.get_ref()).contains("DEXDO_FIXTURE_HOLD_SHARED_RESPONSE");
            let lose_usage =
                body_text(request.get_ref()).contains("DEXDO_FIXTURE_LOSE_BILLING_FRAME");
            let lost_open = body_text(request.get_ref()).contains("DEXDO_FIXTURE_LOSE_STREAM_OPEN");
            let deny_output = body_text(request.get_ref()).contains("DEXDO_FIXTURE_DENY_OUTPUT");
            let duplicate_open =
                body_text(request.get_ref()).contains("DEXDO_FIXTURE_DUPLICATE_STREAM_OPEN");
            let lose_first_frame =
                body_text(request.get_ref()).contains("DEXDO_FIXTURE_LOSE_FIRST_FRAME");
            let hold_usage =
                body_text(request.get_ref()).contains("DEXDO_FIXTURE_HOLD_BILLING_FRAME");
            if body_text(request.get_ref()).contains("DEXDO_FIXTURE_REFUSE_OPEN") {
                return Err(Status::resource_exhausted(
                    "fixture refused before inference",
                ));
            }
            let prompt = body_text(request.get_ref());
            let committed_response = prompt.contains("DEXDO_FIXTURE_COMMITTED_RESPONSE");
            let split_json = prompt.contains("DEXDO_FIXTURE_SPLIT_JSON");
            let bad_status = prompt.contains("DEXDO_FIXTURE_COMMITTED_BAD_STATUS");
            let data_before_head = prompt.contains("DEXDO_FIXTURE_COMMITTED_DATA_BEFORE_HEAD");
            let duplicate_head = prompt.contains("DEXDO_FIXTURE_COMMITTED_DUPLICATE_HEAD");
            let invalid_usage = prompt.contains("DEXDO_FIXTURE_COMMITTED_INVALID_USAGE");
            let omit_charge = prompt.contains("DEXDO_FIXTURE_OMIT_CHARGED_TOKENS");
            let omit_cumulative = prompt.contains("DEXDO_FIXTURE_OMIT_CUMULATIVE_TOKENS");
            if prompt.contains("DEXDO_FIXTURE_CUT_AT_OPEN") {
                return Err(Status::unavailable(
                    "gateway restarted before opening stream",
                ));
            }
            if prompt.contains("DEXDO_FIXTURE_CUT_BEFORE_HEAD") {
                let status = if prompt.contains("BEFORE_HEAD_UNKNOWN") {
                    Status::unknown("gateway interrupted before response head")
                } else {
                    Status::unavailable("gateway restarted before response head")
                };
                let stream = async_stream::stream! {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    yield Err(status);
                };
                return Ok(Response::new(Box::pin(stream)));
            }
            if prompt.contains("DEXDO_FIXTURE_CUT_AFTER_ERROR_HEAD") {
                let frames = [
                    Ok(StreamFrame {
                        head: Some(dexdo_proto::ResponseHead {
                            status: 503,
                            headers: Vec::new(),
                        }),
                        ..Default::default()
                    }),
                    Err(Status::unavailable(
                        "gateway interrupted the provider error body",
                    )),
                ];
                return Ok(Response::new(Box::pin(tokio_stream::iter(frames))));
            }
            let cut_output = prompt.contains("DEXDO_FIXTURE_CUT_AFTER_OUTPUT");
            let cut_billing = prompt.contains("DEXDO_FIXTURE_CUT_BEFORE_BILLING");
            let cut_eof = prompt.contains("DEXDO_FIXTURE_CUT_BEFORE_EOF");
            let response = self.production.open_stream(request).await?;
            if cut_output || cut_billing || cut_eof {
                let mut upstream = response.into_inner();
                let stream = async_stream::try_stream! {
                    while let Some(frame) = upstream.next().await {
                        let frame = frame?;
                        if cut_billing && frame.usage.is_some() {
                            Err(Status::unavailable("gateway restarted before billing"))?;
                        }
                        let cut = cut_output && !frame.data.is_empty() || cut_eof && frame.usage.is_some();
                        yield frame;
                        if cut {
                            Err(Status::unavailable("gateway restarted before EOF"))?;
                        }
                    }
                };
                return Ok(Response::new(Box::pin(stream)));
            }
            if lost_open || duplicate_open {
                let mut stream = response.into_inner();
                while let Some(frame) = stream.next().await {
                    frame?;
                }
                return Err(if duplicate_open {
                    Status::already_exists("fixture found the committed stream response")
                } else {
                    Status::unknown("fixture lost the committed stream response")
                });
            }
            if committed_response
                || bad_status
                || data_before_head
                || duplicate_head
                || invalid_usage
                || split_json
                || omit_charge
                || omit_cumulative
            {
                let mut stream = response.into_inner();
                let mut frames = Vec::new();
                while let Some(frame) = stream.next().await {
                    frames.push(frame?);
                }
                if bad_status {
                    frames[0].head.as_mut().unwrap().status = 42;
                } else if data_before_head {
                    frames[0] = StreamFrame {
                        data: b"content before head".to_vec(),
                        ..StreamFrame::default()
                    };
                } else if duplicate_head {
                    frames[1] = frames[0].clone();
                } else if invalid_usage {
                    frames[1] = StreamFrame {
                        usage: Some(BillingUsage {
                            input_tokens: 1,
                            output_tokens: 1,
                            ..BillingUsage::default()
                        }),
                        ..StreamFrame::default()
                    };
                } else if split_json {
                    let midpoint = frames[1].data.len() / 2;
                    let tail = frames[1].data.split_off(midpoint);
                    frames.insert(
                        2,
                        StreamFrame {
                            data: tail,
                            ..StreamFrame::default()
                        },
                    );
                    if lose_usage || deny_output {
                        assert!(frames.pop().unwrap().usage.is_some());
                    }
                }
                if omit_charge || omit_cumulative {
                    let usage = frames.last_mut().unwrap().usage.as_mut().unwrap();
                    if omit_charge {
                        usage.charged_tokens = None;
                    }
                    if omit_cumulative {
                        usage.cumulative_billable_tokens = None;
                    }
                }
                let mut frames: Vec<_> = frames.into_iter().map(Ok).collect();
                if split_json && deny_output {
                    frames.push(Err(dexdo_proto::upstream_failure_status(
                        Status::unavailable("fixture incorrectly denied output"),
                        false,
                    )));
                }
                return Ok(Response::new(Box::pin(tokio_stream::iter(frames))));
            }
            if lose_first_frame || hold_usage || deny_output {
                let mut stream = response.into_inner();
                let committed = self.held_open.clone();
                let release = self.delayed_eof_release.clone();
                let stream = async_stream::try_stream! {
                    while let Some(frame) = stream.next().await {
                        let frame = frame?;
                        if frame.usage.is_some() {
                            if deny_output {
                                Err(dexdo_proto::upstream_failure_status(
                                    Status::unavailable("fixture incorrectly denied output"), false,
                                ))?;
                            }
                            committed.notify_one();
                            release.acquire().await
                                .map_err(|_| Status::cancelled("fixture release closed"))?.forget();
                            if lose_first_frame {
                                Err(Status::resource_exhausted("fixture lost the first stream frame"))?;
                            }
                        }
                        if !lose_first_frame {
                            yield frame;
                        }
                    }
                };
                return Ok(Response::new(Box::pin(stream)));
            }
            if lose_usage {
                let stream = response
                    .into_inner()
                    .take_while(|frame| !frame.as_ref().is_ok_and(|frame| frame.usage.is_some()));
                return Ok(Response::new(Box::pin(stream)));
            }
            if hold_open {
                self.held_open.notify_one();
                self.delayed_eof_release
                    .acquire()
                    .await
                    .map_err(|_| Status::cancelled("fixture release closed"))?
                    .forget();
            }
            if hold_response {
                let mut stream = response.into_inner();
                let release = self.delayed_eof_release.clone();
                let (tx, rx) = mpsc::channel(GATEWAY_CLIENT_CHANNEL_CAPACITY);
                tokio::spawn(async move {
                    let mut first = true;
                    while let Some(frame) = stream.next().await {
                        let output = frame.as_ref().is_ok_and(|frame| !frame.data.is_empty());
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                        if first && output {
                            first = false;
                            match release.acquire().await {
                                Ok(permit) => permit.forget(),
                                Err(_) => break,
                            }
                        }
                    }
                });
                return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
            }
            return Ok(Response::new(Box::pin(response.into_inner())));
        }

        let request = request.into_inner();
        if body_text(&request).contains("DEXDO_FIXTURE_INVALID_STATUS") {
            // A seller that states a status no HTTP response can carry.
            let head = StreamFrame {
                head: Some(dexdo_proto::ResponseHead {
                    status: 42,
                    headers: Vec::new(),
                }),
                ..StreamFrame::default()
            };
            return Ok(Response::new(Box::pin(tokio_stream::once(Ok(head)))));
        }
        let format = ApiFormat::from_path(&request.path)
            .ok_or_else(|| Status::invalid_argument("output fixture has no consumer path"))?;
        let body = RequestBody::parse(&request.body).map_err(Status::invalid_argument)?;
        let prompt = body_text(&request);
        let prompt = probe_marker.unwrap_or(&prompt);
        let zero_charge = prompt.contains("DEXDO_FIXTURE_ZERO_CHARGE");
        let partial_charge = prompt.contains("DEXDO_FIXTURE_PARTIAL_CHARGE");
        let reasoning_only = prompt.contains("DEXDO_FIXTURE_REASONING_ONLY");
        let mixed_reasoning = prompt.contains("DEXDO_FIXTURE_MIXED_REASONING");
        let delayed_eof = prompt.contains("DEXDO_FIXTURE_DELAY_EOF");
        let usage_then_error = prompt.contains("DEXDO_FIXTURE_USAGE_THEN_ERROR");
        let error_before_usage = prompt.contains("DEXDO_FIXTURE_ERROR_BEFORE_USAGE");
        let hold_after_content = prompt.contains("DEXDO_FIXTURE_HOLD_AFTER_CONTENT");
        let words = 2;
        let stream = body.stream();
        let provider_request = ProviderRequest {
            format,
            body,
            headers: Vec::new(),
        };

        let (up_tx, mut up_rx) = mpsc::channel(GATEWAY_UPSTREAM_CHANNEL_CAPACITY);
        let (client_tx, client_rx) = mpsc::channel(GATEWAY_CLIENT_CHANNEL_CAPACITY);
        let delayed_eof_release = self.delayed_eof_release.clone();
        tokio::spawn(async move {
            let upstream = tokio::spawn(async move {
                mock::run(&provider_request, words, None, up_tx).await;
            });
            let mut first_output = true;
            while let Some(event) = up_rx.recv().await {
                match event {
                    Ok(UpstreamEvent::Head(head)) => {
                        let frame = StreamFrame {
                            head: Some(head),
                            ..StreamFrame::default()
                        };
                        if client_tx.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                    Ok(UpstreamEvent::Data { mut bytes, output }) => {
                        if output && stream && (reasoning_only || mixed_reasoning && first_output) {
                            bytes = reasoning_frame(format, &bytes);
                        } else if output && !stream && (reasoning_only || mixed_reasoning) {
                            bytes = reasoning_body(format, &bytes, mixed_reasoning);
                        }
                        first_output &= !output;
                        let frame = StreamFrame {
                            data: bytes,
                            ..StreamFrame::default()
                        };
                        if client_tx.send(Ok(frame)).await.is_err() {
                            break;
                        }
                        if output && error_before_usage {
                            let _ = client_tx
                                .send(Err(dexdo_proto::upstream_failure_status(
                                    Status::unavailable(
                                        "fixture transport error before terminal usage",
                                    ),
                                    true,
                                )))
                                .await;
                            break;
                        }
                        if output && hold_after_content {
                            match delayed_eof_release.acquire().await {
                                Ok(permit) => permit.forget(),
                                Err(_) => break,
                            }
                        }
                    }
                    Ok(UpstreamEvent::Usage(mut usage)) => {
                        if zero_charge {
                            usage.charged_tokens = Some(0);
                        } else if partial_charge {
                            usage.charged_tokens = Some(1);
                        }
                        let terminal = StreamFrame {
                            usage: Some(usage),
                            ..StreamFrame::default()
                        };
                        if client_tx.send(Ok(terminal)).await.is_err() {
                            break;
                        }
                        if usage_then_error {
                            let _ = client_tx
                                .send(Err(Status::unavailable(
                                    "fixture transport error after terminal usage",
                                )))
                                .await;
                            break;
                        }
                        if delayed_eof {
                            match delayed_eof_release.acquire().await {
                                Ok(permit) => permit.forget(),
                                Err(_) => break,
                            }
                        }
                    }
                    Err(status) => {
                        let _ = client_tx.send(Err(status)).await;
                        break;
                    }
                }
            }
            drop(up_rx);
            let _ = upstream.await;
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(client_rx))))
    }
}

/// One streamed output frame as the provider would send it for reasoning instead of answer text.
fn reasoning_frame(format: ApiFormat, bytes: &[u8]) -> Vec<u8> {
    let text = std::str::from_utf8(bytes).expect("mock frames are UTF-8");
    let (event, data) = match text.strip_prefix("event: ") {
        Some(rest) => {
            let (event, data) = rest.split_once('\n').expect("an event line ends");
            (Some(event), data)
        }
        None => (None, text),
    };
    let data = data
        .strip_prefix("data: ")
        .and_then(|data| data.strip_suffix("\n\n"))
        .expect("one mock data frame");
    let mut value: Value = serde_json::from_str(data).expect("mock frames are JSON");
    let event = match format {
        ApiFormat::ChatCompletions => {
            let delta = &mut value["choices"][0]["delta"];
            let content = delta.as_object_mut().unwrap().remove("content").unwrap();
            delta["reasoning"] = content;
            None
        }
        ApiFormat::Responses => {
            value["type"] = "response.reasoning_text.delta".into();
            event.map(|_| "response.reasoning_text.delta")
        }
        ApiFormat::Messages => {
            let delta = &mut value["delta"];
            let text = delta.as_object_mut().unwrap().remove("text").unwrap();
            *delta = serde_json::json!({ "type": "thinking_delta", "thinking": text });
            event
        }
    };
    let mut frame = String::new();
    if let Some(event) = event {
        frame.push_str(&format!("event: {event}\n"));
    }
    frame.push_str(&format!("data: {value}\n\n"));
    frame.into_bytes()
}

/// A whole answer whose text is reasoning; `keep_text` adds the reasoning beside the text instead.
fn reasoning_body(format: ApiFormat, bytes: &[u8], keep_text: bool) -> Vec<u8> {
    const REASONING: &str = "fixture reasoning";
    let mut value: Value = serde_json::from_slice(bytes).expect("mock bodies are JSON");
    match format {
        ApiFormat::ChatCompletions => {
            let message = &mut value["choices"][0]["message"];
            let text = message["content"].clone();
            if keep_text {
                message["reasoning"] = REASONING.into();
            } else {
                message.as_object_mut().unwrap().remove("content");
                message["reasoning"] = text;
            }
        }
        ApiFormat::Responses => {
            let output = value["output"].as_array_mut().unwrap();
            let text = output[0]["content"][0]["text"].clone();
            let reasoning = serde_json::json!({
                "type": "reasoning",
                "id": "rs_fixture",
                "summary": [],
                "content": [{
                    "type": "reasoning_text",
                    "text": if keep_text { Value::from(REASONING) } else { text },
                }],
            });
            if keep_text {
                output.insert(0, reasoning);
            } else {
                output[0] = reasoning;
            }
        }
        ApiFormat::Messages => {
            let content = value["content"].as_array_mut().unwrap();
            let text = content[0]["text"].clone();
            let thinking = serde_json::json!({
                "type": "thinking",
                "thinking": if keep_text { Value::from(REASONING) } else { text },
                "signature": "fixture",
            });
            if keep_text {
                content.insert(0, thinking);
            } else {
                content[0] = thinking;
            }
        }
    }
    serde_json::to_vec(&value).expect("a JSON value serializes")
}

fn is_adversarial_output_request(request: &StreamRequest) -> bool {
    let body = body_text(request);
    [
        "DEXDO_FIXTURE_ZERO_CHARGE",
        "DEXDO_FIXTURE_PARTIAL_CHARGE",
        "DEXDO_FIXTURE_REASONING_ONLY",
        "DEXDO_FIXTURE_MIXED_REASONING",
        "DEXDO_FIXTURE_DELAY_EOF",
        "DEXDO_FIXTURE_USAGE_THEN_ERROR",
        "DEXDO_FIXTURE_ERROR_BEFORE_USAGE",
        "DEXDO_FIXTURE_HOLD_AFTER_CONTENT",
        "DEXDO_FIXTURE_INVALID_STATUS",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

fn authorize_fixture_request(
    auth: &AuthRegistry,
    request: &StreamRequest,
) -> Result<(), Box<Status>> {
    let signature: [u8; 64] = request
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| Box::new(Status::unauthenticated("bad signature length")))?;
    if !auth.verify_response(
        &request.token_contract,
        &request.nonce,
        &Signature(signature),
    ) {
        return Err(Box::new(Status::unauthenticated(
            "challenge-response failed",
        )));
    }
    Ok(())
}

#[derive(Clone)]
struct CountRecoveryCalls<S> {
    service: S,
    calls: Arc<AtomicUsize>,
}

impl<S, B> tower::Service<http::Request<B>> for CountRecoveryCalls<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if request.uri().path() == "/dexdo.v1.Gateway/ReadUsageOutcome" {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        self.service.call(request)
    }
}

pub(super) async fn start_gateway_with(
    addr: SocketAddr,
    upstream: UpstreamConfig,
) -> Result<RunningFixtureSeller> {
    let state = Arc::new(GatewayState::with_upstream(upstream));
    let fixture_auth = Arc::new(AuthRegistry::new());
    let delayed_eof_release = Arc::new(Semaphore::new(0));
    let held_open = Arc::new(Notify::new());
    let legacy_challenge = Arc::new(AtomicBool::new(false));
    let probe_output_fixture = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let recovery_calls = Arc::new(AtomicUsize::new(0));
    let service = GatewayServer::new(FixtureGateway {
        production: GatewayService::new(state.clone()),
        fixture_auth: fixture_auth.clone(),
        delayed_eof_release: delayed_eof_release.clone(),
        held_open: held_open.clone(),
        legacy_challenge: legacy_challenge.clone(),
        probe_output_fixture: probe_output_fixture.clone(),
        calls: calls.clone(),
    })
    .max_decoding_message_size(dexdo_core::params::GATEWAY_REQUEST_MAX_BYTES);
    let tls = GatewayTls::generate()?;

    crate::seller::tls::ensure_crypto_provider();
    let identity = Identity::from_pem(&tls.cert_pem, &tls.key_pem);
    let tls_config = ServerTlsConfig::new().identity(identity);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| anyhow!("bind fixture seller gateway {addr}: {error}"))?;
    let listen_addr = listener
        .local_addr()
        .map_err(|error| anyhow!("read bound fixture seller gateway address: {error}"))?;
    let incoming = TcpListenerStream::new(listener);
    let mut builder = Server::builder()
        .tls_config(tls_config)
        .map_err(|error| anyhow!("configure fixture seller gateway TLS: {error}"))?
        .layer({
            let calls = recovery_calls.clone();
            tower::layer::layer_fn(move |service| CountRecoveryCalls {
                service,
                calls: calls.clone(),
            })
        });
    let server_task = tokio::spawn(async move {
        if let Err(error) = builder
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
        {
            tracing::error!("fixture seller gateway stopped: {error}");
        }
    });

    Ok(RunningFixtureSeller {
        state,
        fixture_auth,
        server_task,
        listen_addr,
        tls,
        delayed_eof_release,
        held_open,
        legacy_challenge,
        probe_output_fixture,
        calls,
        recovery_calls,
    })
}
