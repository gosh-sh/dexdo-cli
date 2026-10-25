//! Test-only seller seam for buyer-side adversarial stream fixtures.

//! Ordinary requests delegate to the production gateway unchanged. A request carrying an existing
//! adversarial-output marker takes a bare gRPC branch after authorization: that branch observes the
//! output cap on the wire but deliberately does not turn it into a seller reservation. It therefore
//! models a third-party seller that can send a chunk crossing the remaining output allowance.

use crate::seller::auth::AuthRegistry;
use crate::seller::gateway::{GatewayService, GatewayState};
use crate::seller::tls::GatewayTls;
use crate::seller::upstream::{mock, UpstreamConfig, UpstreamEvent};
use anyhow::{anyhow, Result};
use dexdo_core::note::{NotePubkey, Signature};
use dexdo_core::params::{GATEWAY_CLIENT_CHANNEL_CAPACITY, GATEWAY_UPSTREAM_CHANNEL_CAPACITY};
use dexdo_core::{DealChainState, DealSubscription};
use dexdo_proto::{CanonChunk, Challenge, ChallengeRequest, Gateway, GatewayServer, StreamRequest};
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
    hide_reasoning_capability: Arc<AtomicBool>,
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
    pub(super) fn use_legacy_challenge(&self) {
        self.hide_reasoning_capability.store(true, Ordering::SeqCst);
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
    hide_reasoning_capability: Arc<AtomicBool>,
    probe_output_fixture: Arc<Mutex<Option<(bool, &'static str)>>>,
    calls: Arc<AtomicUsize>,
}

type ChunkStream = Pin<Box<dyn Stream<Item = Result<CanonChunk, Status>> + Send>>;

#[tonic::async_trait]
impl Gateway for FixtureGateway {
    async fn get_challenge(
        &self,
        mut request: Request<ChallengeRequest>,
    ) -> Result<Response<Challenge>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let legacy = self.hide_reasoning_capability.load(Ordering::SeqCst);
        if legacy {
            // An old seller ignores the additive request flag and still issues a normal nonce.
            request.get_mut().capabilities_only = false;
        }
        let token_contract = request.get_ref().token_contract.clone();
        let capabilities_only = request.get_ref().capabilities_only;
        let mut response = self.production.get_challenge(request).await?;
        if legacy {
            // False is omitted by protobuf encoding, exactly the legacy two-field reply.
            response.get_mut().supports_reasoning_disable = false;
        }
        if !capabilities_only {
            self.fixture_auth
                .issue_challenge(&token_contract, response.get_ref().nonce.clone());
        }
        Ok(response)
    }

    type OpenStreamStream = ChunkStream;

    async fn open_stream(
        &self,
        request: Request<StreamRequest>,
    ) -> Result<Response<Self::OpenStreamStream>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let greedy = request
            .get_ref()
            .request
            .as_ref()
            .and_then(|canon| canon.params.as_ref())
            .is_some_and(|params| params.greedy);
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
            let hold_open = request.get_ref().request.as_ref().is_some_and(|canon| {
                canon
                    .messages
                    .iter()
                    .any(|message| message.content.contains("DEXDO_FIXTURE_HOLD_STREAM_OPEN"))
            });
            let hold_response = request.get_ref().request.as_ref().is_some_and(|canon| {
                canon.messages.iter().any(|message| {
                    message
                        .content
                        .contains("DEXDO_FIXTURE_HOLD_SHARED_RESPONSE")
                })
            });
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
                    while let Some(chunk) = stream.next().await {
                        if tx.send(chunk).await.is_err() {
                            break;
                        }
                        if first {
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

        let mut request = request.into_inner();
        let canon = request
            .request
            .take()
            .ok_or_else(|| Status::invalid_argument("output fixture has no canonical request"))?;
        let wire_max = canon
            .params
            .as_ref()
            .map(|params| u64::from(params.max_tokens))
            .filter(|max| *max > 0)
            .ok_or_else(|| Status::invalid_argument("output fixture has no wire output cap"))?;
        let prompt = canon
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.as_str())
            .unwrap_or("");
        let prompt = probe_marker.unwrap_or(prompt);
        let zero_charge = prompt.contains("DEXDO_FIXTURE_ZERO_CHARGE");
        let partial_charge = prompt.contains("DEXDO_FIXTURE_PARTIAL_CHARGE");
        let reasoning_only = prompt.contains("DEXDO_FIXTURE_REASONING_ONLY");
        let mixed_reasoning = prompt.contains("DEXDO_FIXTURE_MIXED_REASONING");
        let delayed_eof = prompt.contains("DEXDO_FIXTURE_DELAY_EOF");
        let usage_then_error = prompt.contains("DEXDO_FIXTURE_USAGE_THEN_ERROR");
        let error_before_usage = prompt.contains("DEXDO_FIXTURE_ERROR_BEFORE_USAGE");
        let hold_after_content = prompt.contains("DEXDO_FIXTURE_HOLD_AFTER_CONTENT");
        let chunks = if reasoning_only {
            wire_max
        } else if prompt.contains("DEXDO_FIXTURE_FATCHUNK_COMPLETE") {
            wire_max / u64::from(mock::FAT_CHUNK_TOKENS)
        } else if prompt.contains("DEXDO_FIXTURE_FATCHUNK") {
            wire_max / u64::from(mock::FAT_CHUNK_TOKENS) + 1
        } else {
            2
        };

        let (up_tx, mut up_rx) = mpsc::channel(GATEWAY_UPSTREAM_CHANNEL_CAPACITY);
        let (client_tx, client_rx) = mpsc::channel(GATEWAY_CLIENT_CHANNEL_CAPACITY);
        let delayed_eof_release = self.delayed_eof_release.clone();
        tokio::spawn(async move {
            let upstream = tokio::spawn(async move {
                mock::run(chunks, Some(&canon), up_tx, false, None).await;
            });
            while let Some(event) = up_rx.recv().await {
                match event {
                    Ok(UpstreamEvent::Chunk(mut chunk)) => {
                        if reasoning_only || mixed_reasoning && chunk.seq == 0 {
                            chunk.reasoning = std::mem::take(&mut chunk.text);
                        }
                        if client_tx.send(Ok(chunk)).await.is_err() {
                            break;
                        }
                        if error_before_usage {
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
                        if hold_after_content {
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
                        let terminal = dexdo_proto::CanonChunk {
                            seq: chunks,
                            usage: Some(usage),
                            ..Default::default()
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

fn is_adversarial_output_request(request: &StreamRequest) -> bool {
    request
        .request
        .as_ref()
        .and_then(|canon| {
            canon
                .messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
        })
        .is_some_and(|message| {
            message.content.contains("DEXDO_FIXTURE_ZERO_CHARGE")
                || message.content.contains("DEXDO_FIXTURE_PARTIAL_CHARGE")
                || message.content.contains("DEXDO_FIXTURE_FATCHUNK")
                || message.content.contains("DEXDO_FIXTURE_STRADDLE")
                || message.content.contains("DEXDO_FIXTURE_REASONING_ONLY")
                || message.content.contains("DEXDO_FIXTURE_MIXED_REASONING")
                || message.content.contains("DEXDO_FIXTURE_DELAY_EOF")
                || message.content.contains("DEXDO_FIXTURE_USAGE_THEN_ERROR")
                || message.content.contains("DEXDO_FIXTURE_HOLD_AFTER_CONTENT")
        })
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
    let hide_reasoning_capability = Arc::new(AtomicBool::new(false));
    let probe_output_fixture = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let service = GatewayServer::new(FixtureGateway {
        production: GatewayService::new(state.clone()),
        fixture_auth: fixture_auth.clone(),
        delayed_eof_release: delayed_eof_release.clone(),
        held_open: held_open.clone(),
        hide_reasoning_capability: hide_reasoning_capability.clone(),
        probe_output_fixture: probe_output_fixture.clone(),
        calls: calls.clone(),
    });
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
        hide_reasoning_capability,
        probe_output_fixture,
        calls,
    })
}
