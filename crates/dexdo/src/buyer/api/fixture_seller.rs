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
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::Stream;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

pub(super) struct RunningFixtureSeller {
    state: Arc<GatewayState>,
    fixture_auth: Arc<AuthRegistry>,
    pub(super) server_task: tokio::task::JoinHandle<()>,
    pub(super) listen_addr: SocketAddr,
    pub(super) tls_fingerprint: String,
}

impl RunningFixtureSeller {
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
}

struct FixtureGateway {
    production: GatewayService,
    fixture_auth: Arc<AuthRegistry>,
}

type ChunkStream = Pin<Box<dyn Stream<Item = Result<CanonChunk, Status>> + Send>>;

#[tonic::async_trait]
impl Gateway for FixtureGateway {
    async fn get_challenge(
        &self,
        request: Request<ChallengeRequest>,
    ) -> Result<Response<Challenge>, Status> {
        let token_contract = request.get_ref().token_contract.clone();
        let response = self.production.get_challenge(request).await?;
        self.fixture_auth
            .issue_challenge(&token_contract, response.get_ref().nonce.clone());
        Ok(response)
    }

    type OpenStreamStream = ChunkStream;

    async fn open_stream(
        &self,
        request: Request<StreamRequest>,
    ) -> Result<Response<Self::OpenStreamStream>, Status> {
        let adversarial_output = is_adversarial_output_request(request.get_ref());
        authorize_fixture_request(&self.fixture_auth, request.get_ref())
            .map_err(|status| *status)?;
        if !adversarial_output {
            let response = self.production.open_stream(request).await?;
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
        let chunks = if prompt.contains("DEXDO_FIXTURE_FATCHUNK_COMPLETE") {
            wire_max / u64::from(mock::FAT_CHUNK_TOKENS)
        } else if prompt.contains("DEXDO_FIXTURE_FATCHUNK") {
            wire_max / u64::from(mock::FAT_CHUNK_TOKENS) + 1
        } else {
            2
        };

        let (up_tx, mut up_rx) = mpsc::channel(GATEWAY_UPSTREAM_CHANNEL_CAPACITY);
        let (client_tx, client_rx) = mpsc::channel(GATEWAY_CLIENT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            let upstream = tokio::spawn(async move {
                mock::run(chunks, Some(&canon), up_tx, false, None).await;
            });
            while let Some(event) = up_rx.recv().await {
                match event {
                    Ok(UpstreamEvent::Chunk(chunk)) => {
                        if client_tx.send(Ok(chunk)).await.is_err() {
                            break;
                        }
                    }
                    Ok(UpstreamEvent::Usage(usage)) => {
                        let terminal = dexdo_proto::CanonChunk {
                            seq: chunks,
                            usage: Some(usage),
                            ..Default::default()
                        };
                        if client_tx.send(Ok(terminal)).await.is_err() {
                            break;
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
            message.content.contains("DEXDO_FIXTURE_FATCHUNK")
                || message.content.contains("DEXDO_FIXTURE_STRADDLE")
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
    let service = GatewayServer::new(FixtureGateway {
        production: GatewayService::new(state.clone()),
        fixture_auth: fixture_auth.clone(),
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
    })
}
