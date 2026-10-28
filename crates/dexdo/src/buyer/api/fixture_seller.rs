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
    Challenge, ChallengeRequest, Gateway, GatewayServer, StreamFrame, StreamRequest,
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
    pub(super) tls_fingerprint: String,
    delayed_eof_release: Arc<Semaphore>,
    held_open: Arc<Notify>,
    legacy_challenge: Arc<AtomicBool>,
    probe_output_fixture: Arc<Mutex<Option<(bool, &'static str)>>>,
    calls: Arc<AtomicUsize>,
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

    pub(super) fn set_probe_output_fixture(&self, greedy: bool, marker: &'static str) {
        *self.probe_output_fixture.lock().unwrap() = Some((greedy, marker));
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
            let response = self.production.open_stream(request).await?;
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
    let tls_fingerprint = tls.fingerprint.clone();
    let identity = Identity::from_pem(tls.cert_pem, tls.key_pem);
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
        .map_err(|error| anyhow!("configure fixture seller gateway TLS: {error}"))?;
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
        tls_fingerprint,
        delayed_eof_release,
        held_open,
        legacy_challenge,
        probe_output_fixture,
        calls,
    })
}
