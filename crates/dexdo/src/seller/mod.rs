//! Seller client: gateway + authorization + mock upstream + stream opening.
//! Headless(R12): starts without a GUI and serves the stream as a daemon.

pub mod advance;
pub mod auth;
pub mod gateway;
pub mod models;
pub mod tls;
pub mod upstream;

pub use advance::{drive_advance, AdvanceWindows};
pub use models::{Capabilities, ModelConfig, ModelsConfig};
pub use upstream::{anthropic::AnthropicConfig, openai::OpenAiConfig, UpstreamConfig};

use anyhow::{bail, Result};
use dexdo_core::{
    ChainBackend, DobParams, Handover, LocalNote, Match, MatchWatchCursor, Note, SellOffer,
    TokenContract,
};
use gateway::{GatewayService, GatewayState};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tls::GatewayTls;
use tonic::transport::{Identity, Server, ServerTlsConfig};

pub const DEFAULT_MATCH_POLL_INTERVAL: Duration = Duration::from_secs(30);
const SELLER_MATCH_WATCH_CURSOR_VERSION: u32 = 1;
const SELLER_OPEN_STATE_READ_ATTEMPTS: usize = 3;
const SELLER_OPEN_STATE_INITIAL_BACKOFF: Duration = Duration::from_millis(100);

/// Seller configuration for one stream.
pub struct SellerConfig {
    /// Contract -- the deal's handover point.
    pub token_contract: TokenContract,
    /// Tick price `P` in SHELL.
    pub price_per_tick: u64,
    /// Maximum ticks in the offer.
    pub max_ticks: u64,
    /// Public gateway host:port that will be encrypted to the buyer(R15).
    pub gateway_advertise: String,
    /// How many fake tokens to yield(mock model). `0` = a deliberate seller no-show.
    /// Real upstreams are limited by the buyer request's `max_tokens` and the market cap
    /// (`max_ticks * TICK_SIZE`), not by this debug fixture.
    pub mock_token_count: u64,
}

/// A running seller gateway: state handle + handle to the server's background task.
pub struct RunningSeller {
    pub state: Arc<GatewayState>,
    /// The seller's note -- **polymorphic**: `LocalNote`(mock path) OR `RealNote` (real shellnet,
    /// one SDK key for signing+handover). The gateway encrypts the endpoint `note.encrypt_to(buyer_pubkey)` -- on
    /// the real path `buyer_pubkey` is reconstructed by the seller from on-chain ed25519(F1).
    pub note: Arc<dyn Note>,
    pub server_task: tokio::task::JoinHandle<()>,
    /// Fingerprint of the gateway's self-signed TLS certificate -- goes into the handover.
    pub tls_fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct SellerMatchWatchConfig {
    pub cursor_path: PathBuf,
    pub poll_interval: Duration,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SellerMatchWatchCursor {
    version: u32,
    token_contract: TokenContract,
    source: MatchWatchCursor,
    last_polled_unix: Option<u64>,
    opened_at_unix: Option<u64>,
}

impl SellerMatchWatchCursor {
    fn new(token_contract: &TokenContract) -> Result<Self> {
        Ok(Self {
            version: SELLER_MATCH_WATCH_CURSOR_VERSION,
            token_contract: token_contract.clone(),
            source: MatchWatchCursor::new(now_unix()? as i64),
            last_polled_unix: None,
            opened_at_unix: None,
        })
    }

    fn load_or_new(path: &Path, token_contract: &TokenContract) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) if !bytes.is_empty() => {
                let cursor: Self = serde_json::from_slice(&bytes).map_err(|e| {
                    anyhow::anyhow!("parse seller watch cursor {}: {e}", path.display())
                })?;
                if cursor.version != SELLER_MATCH_WATCH_CURSOR_VERSION {
                    bail!(
                        "seller watch cursor {} has version {}; expected {}",
                        path.display(),
                        cursor.version,
                        SELLER_MATCH_WATCH_CURSOR_VERSION
                    );
                }
                if cursor.token_contract != *token_contract {
                    bail!(
                        "seller watch cursor {} is for token_contract {}, not {}",
                        path.display(),
                        cursor.token_contract,
                        token_contract
                    );
                }
                Ok(cursor)
            }
            Ok(_) => Self::new(token_contract),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(token_contract),
            Err(e) => Err(anyhow::anyhow!(
                "read seller watch cursor {}: {e}",
                path.display()
            )),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!("create seller watch cursor dir {}: {e}", parent.display())
                })?;
            }
        }
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, bytes).map_err(|e| {
            anyhow::anyhow!("write seller watch cursor temp {}: {e}", tmp.display())
        })?;
        std::fs::rename(&tmp, path).map_err(|e| {
            anyhow::anyhow!(
                "commit seller watch cursor {} from temp {}: {e}",
                path.display(),
                tmp.display()
            )
        })
    }
}

fn now_unix() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

/// Bring up the seller's gRPC gateway(headless) **over TLS**: a self-signed certificate
/// is generated at startup, its fingerprint is returned for recording in the handover. Returns
/// handles for orchestrating the stream.
pub async fn start_gateway(addr: SocketAddr) -> Result<RunningSeller> {
    start_gateway_with(addr, UpstreamConfig::Mock).await
}

/// Like [`start_gateway`], but with an upstream choice (mock model or real OpenAI-compatible,
/// ). The mock path(`UpstreamConfig::Mock`) is identical to.
pub async fn start_gateway_with(
    addr: SocketAddr,
    upstream: UpstreamConfig,
) -> Result<RunningSeller> {
    // The ephemeral note is a mock fixture; the production path is `start_gateway_with_note`.
    start_gateway_with_note(addr, upstream, Arc::new(LocalNote::generate())).await
}

/// Like [`start_gateway_with`], but with a **loaded persistent** seller note:
/// the identity(from `--note-key`/wallet) is reused across runs -- its offer/deals are
/// visible in the next run. `start_gateway_with` substitutes an ephemeral `generate()` here.
pub async fn start_gateway_with_note(
    addr: SocketAddr,
    upstream: UpstreamConfig,
    note: Arc<dyn Note>,
) -> Result<RunningSeller> {
    let state = Arc::new(GatewayState::with_upstream(upstream));
    let service = GatewayService::new(state.clone()).into_server();

    // Both rustls providers(ring/aws-lc-rs) are present in the tree; pin the process
    // default explicitly(ring) -- otherwise rustls panics, unable to pick on its own. Idempotent.
    tls::ensure_crypto_provider();

    // the gateway's self-signed TLS certificate; trust comes from the encrypted handover.
    let gw_tls = GatewayTls::generate()?;
    let tls_fingerprint = gw_tls.fingerprint.clone();
    let identity = Identity::from_pem(gw_tls.cert_pem, gw_tls.key_pem);
    let tls_config = ServerTlsConfig::new().identity(identity);

    let server_task = tokio::spawn(async move {
        match Server::builder().tls_config(tls_config) {
            Ok(mut builder) => {
                if let Err(e) = builder.add_service(service).serve(addr).await {
                    tracing::error!("gateway server stopped: {e}");
                }
            }
            Err(e) => tracing::error!("gateway TLS config failed: {e}"),
        }
    });
    Ok(RunningSeller {
        state,
        note,
        server_task,
        tls_fingerprint,
    })
}

/// Post a sell offer from the note into the book. Done before the
/// buyer places a buy order.
pub async fn post_offer(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
) -> Result<()> {
    post_offer_with_note(seller.note.as_ref(), chain, cfg).await
}

/// Like [`post_offer`], but uses a note directly. The CLI calls this before
/// opening the gateway so TCP listening cannot be mistaken for market readiness.
pub async fn post_offer_with_note(
    note: &dyn Note,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
) -> Result<()> {
    let offer = SellOffer {
        price_per_tick: cfg.price_per_tick,
        max_ticks: cfg.max_ticks,
        token_contract: cfg.token_contract.clone(),
    };
    chain.post_offer(offer, note).await?;
    Ok(())
}

/// Open the stream for a match:
/// 1. reads the match(the buyer's pubkey is recorded in the contract);
/// 2. encrypts the endpoint to the buyer's pubkey and `open_stream` (probe freeze +
/// `SELLER_PROBE_COMMISSION` + writing the enc-endpoint into the endpoints file);
/// 3. registers the buyer's pubkey and the fake-token budget in the gateway for authorization.
pub async fn serve_match(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
) -> Result<()> {
    let m = chain.read_match(&cfg.token_contract).await?;
    provision_match(seller, chain, cfg, m).await
}

async fn read_opened_with_retry(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
) -> Result<bool> {
    let mut last_failure = String::new();
    for attempt in 1..=SELLER_OPEN_STATE_READ_ATTEMPTS {
        match chain.deal_state(token_contract).await {
            Ok(Some(state)) => return Ok(state.opened),
            Ok(None) => {
                last_failure = "getState returned no TokenContract state".to_string();
            }
            Err(error) => {
                last_failure = format!("getState failed: {error}");
            }
        }
        if attempt < SELLER_OPEN_STATE_READ_ATTEMPTS {
            let delay = SELLER_OPEN_STATE_INITIAL_BACKOFF * attempt as u32;
            tracing::warn!(
                token_contract = %token_contract,
                attempt,
                max_attempts = SELLER_OPEN_STATE_READ_ATTEMPTS,
                backoff_ms = delay.as_millis(),
                failure = %last_failure,
                "seller open decision state read failed; retrying"
            );
            tokio::time::sleep(delay).await;
        }
    }
    bail!(
        "TokenContract {token_contract} getState unreadable after {SELLER_OPEN_STATE_READ_ATTEMPTS} attempts; refusing to skip open_stream: {last_failure}"
    )
}

/// Provision access for a known match: register gateway authorization, then open the stream only when the
/// authoritative on-chain `getState.opened` flag is false. A restarted gateway always rebuilds in-memory auth,
/// while an already-opened deal skips the duplicate chain write.
pub async fn provision_match(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    m: Match,
) -> Result<()> {
    if m.token_contract != cfg.token_contract {
        bail!(
            "seller watcher returned match for token_contract {}, expected {}",
            m.token_contract,
            cfg.token_contract
        );
    }
    // the handover {gateway endpoint, TLS fingerprint} is encrypted to the buyer's pubkey.
    // The endpoint points at the GATEWAY over TLS(R15); the buyer pins the fingerprint on connect.
    let handover = Handover {
        endpoint: format!("https://{}", cfg.gateway_advertise),
        tls_fingerprint: seller.tls_fingerprint.clone(),
    };
    let enc = seller
        .note
        .encrypt_to(&m.buyer_pubkey, &handover.to_bytes());

    // the gateway must authorize the matched buyer BEFORE that buyer can connect.
    // Register buyer+budget BEFORE writing the handover on-chain: the buyer learns the endpoint only
    // after reading the on-chain ciphertext(written by `open_stream`), so register-before-open rules out a race. Otherwise on a
    // real(slow) chain the buyer manages to knock in the window between open_stream and register_stream
    // -> the gateway still has no pubkey -> `challenge-response failed`(the mock timing did not expose this).
    seller.state.register_stream(
        &cfg.token_contract,
        m.buyer_pubkey,
        cfg.mock_token_count,
        cfg.max_ticks,
        DobParams::canonical().tick_size,
    );
    if !read_opened_with_retry(chain, &cfg.token_contract).await? {
        chain
            .open_stream(&cfg.token_contract, enc, seller.note.as_ref())
            .await?;
    } else {
        tracing::info!(
            token_contract = %cfg.token_contract,
            "seller gateway restored auth for opened deal; skipping duplicate open_stream"
        );
    }
    Ok(())
}

/// Poll once for a match and provision access if one exists. The cursor is saved on every successful poll so a
/// restarted gateway continues from the same source position instead of rereading the note event window forever.
pub async fn poll_match_and_maybe_open(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    cursor_path: &Path,
) -> Result<Option<Match>> {
    let mut cursor = SellerMatchWatchCursor::load_or_new(cursor_path, &cfg.token_contract)?;
    cursor.last_polled_unix = Some(now_unix()?);
    let found = chain
        .poll_openable_match(&cfg.token_contract, &mut cursor.source)
        .await?;
    if let Some(m) = found {
        provision_match(seller, chain, cfg, m.clone()).await?;
        cursor.opened_at_unix.get_or_insert(now_unix()?);
        cursor.save(cursor_path)?;
        Ok(Some(m))
    } else {
        cursor.save(cursor_path)?;
        Ok(None)
    }
}

/// Gateway-owned match watcher. This is intentionally an indefinite loop: as long as the offer remains a valid
/// resting/openable deal, no five-minute seller timeout tears down the process.
pub async fn watch_and_serve_match(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
) -> Result<Match> {
    loop {
        if let Some(m) = poll_match_and_maybe_open(seller, chain, cfg, &watch.cursor_path).await? {
            return Ok(m);
        }
        tokio::time::sleep(watch.poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{
        ChainError, DealChainState, LocalNote, NotePubkey, OfferListing, SellOffer, Settlement,
        StreamSnapshot,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    struct PollBackend {
        matched: Option<Match>,
        handover: Mutex<Option<Vec<u8>>>,
        opens: AtomicU64,
        opened: bool,
        state_failures_remaining: AtomicU64,
        state_reads: AtomicU64,
        expect_last_seen: Option<i64>,
        record_created_at: i64,
    }

    impl PollBackend {
        fn new(
            matched: Option<Match>,
            expect_last_seen: Option<i64>,
            record_created_at: i64,
        ) -> Self {
            Self {
                matched,
                handover: Mutex::new(None),
                opens: AtomicU64::new(0),
                opened: false,
                state_failures_remaining: AtomicU64::new(0),
                state_reads: AtomicU64::new(0),
                expect_last_seen,
                record_created_at,
            }
        }

        fn with_state(matched: Match, handover_present: bool, opened: bool) -> Self {
            Self {
                matched: Some(matched),
                handover: Mutex::new(handover_present.then(|| b"existing-handover".to_vec())),
                opens: AtomicU64::new(0),
                opened,
                state_failures_remaining: AtomicU64::new(0),
                state_reads: AtomicU64::new(0),
                expect_last_seen: None,
                record_created_at: 1,
            }
        }

        fn with_state_failures(mut self, failures: u64) -> Self {
            self.state_failures_remaining = AtomicU64::new(failures);
            self
        }
    }

    #[async_trait::async_trait]
    impl ChainBackend for PollBackend {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            unimplemented!()
        }

        async fn post_offer(&self, _: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn poll_openable_match(
            &self,
            token_contract: &TokenContract,
            cursor: &mut MatchWatchCursor,
        ) -> Result<Option<Match>, ChainError> {
            assert_eq!(cursor.last_seen_created_at, self.expect_last_seen);
            cursor.record_seen_batch([(self.record_created_at, token_contract.clone())]);
            Ok(self.matched.clone())
        }

        async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
            self.matched
                .clone()
                .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))
        }

        async fn open_stream(
            &self,
            _token_contract: &TokenContract,
            enc_endpoint: Vec<u8>,
            _: &dyn Note,
        ) -> Result<(), ChainError> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            self.handover.lock().unwrap().replace(enc_endpoint);
            Ok(())
        }

        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            Ok(self.handover.lock().unwrap().clone())
        }

        async fn advance_tick(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn accept_probe(&self, _: &TokenContract) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }

        async fn seller_timeout(&self, _: &TokenContract) -> Result<Settlement, ChainError> {
            unimplemented!()
        }

        async fn deal_state(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            self.state_reads.fetch_add(1, Ordering::Relaxed);
            if self.state_failures_remaining.load(Ordering::Relaxed) > 0 {
                self.state_failures_remaining
                    .fetch_sub(1, Ordering::Relaxed);
                return Err(ChainError::Chain("transient getState failure".to_string()));
            }
            Ok(Some(DealChainState {
                funded: true,
                opened: self.opened,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(1),
                last_advance: 0,
            }))
        }

        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            None
        }
    }

    fn test_seller() -> RunningSeller {
        RunningSeller {
            state: Arc::new(GatewayState::new()),
            note: Arc::new(LocalNote::generate()),
            server_task: tokio::spawn(async {}),
            tls_fingerprint: "test-fingerprint".to_string(),
        }
    }

    fn test_cfg(token_contract: &str) -> SellerConfig {
        SellerConfig {
            token_contract: token_contract.to_string(),
            price_per_tick: 1000,
            max_ticks: 8,
            gateway_advertise: "127.0.0.1:8443".to_string(),
            mock_token_count: 8,
        }
    }

    fn temp_cursor_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-seller-watch-test-{}-{}",
            std::process::id(),
            now_unix().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}.json"))
    }

    fn sample_match(token_contract: &str, buyer_pubkey: NotePubkey) -> Match {
        Match {
            token_contract: token_contract.to_string(),
            buyer_pubkey,
            price_per_tick: 1000,
        }
    }

    #[tokio::test]
    async fn poll_match_cursor_persists_and_resume_uses_it() {
        let cursor_path = temp_cursor_path("resume");
        let seller = test_seller();
        let cfg = test_cfg("tc-watch");
        let buyer = LocalNote::generate();
        let first_seen = now_unix().unwrap() as i64 + 1;

        let first = PollBackend::new(None, None, first_seen);
        assert!(
            poll_match_and_maybe_open(&seller, &first, &cfg, &cursor_path)
                .await
                .unwrap()
                .is_none(),
            "first poll has no match but persists cursor"
        );
        assert_eq!(first.opens.load(Ordering::Relaxed), 0);

        let second = PollBackend::new(
            Some(sample_match("tc-watch", buyer.pubkey())),
            Some(first_seen),
            first_seen + 1,
        );
        let matched = poll_match_and_maybe_open(&seller, &second, &cfg, &cursor_path)
            .await
            .unwrap()
            .expect("second poll resumes cursor and sees the match");
        assert_eq!(matched.token_contract, "tc-watch");
        assert_eq!(second.opens.load(Ordering::Relaxed), 1);

        let saved: SellerMatchWatchCursor =
            serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
        assert_eq!(saved.source.last_seen_created_at, Some(first_seen + 1));
        assert!(saved.opened_at_unix.is_some());
    }

    #[tokio::test]
    async fn handover_present_and_opened_false_calls_open_stream() {
        let seller = test_seller();
        let cfg = test_cfg("tc-partial-open");
        let buyer = LocalNote::generate();
        let backend =
            PollBackend::with_state(sample_match("tc-partial-open", buyer.pubkey()), true, false);

        provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-partial-open", buyer.pubkey()),
        )
        .await
        .unwrap();

        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn opened_true_skips_duplicate_open_stream_and_restores_auth() {
        let seller = test_seller();
        let cfg = test_cfg("tc-opened");
        let buyer = LocalNote::generate();
        let backend =
            PollBackend::with_state(sample_match("tc-opened", buyer.pubkey()), true, true);

        provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-opened", buyer.pubkey()),
        )
        .await
        .unwrap();

        assert_eq!(
            backend.opens.load(Ordering::Relaxed),
            0,
            "existing handover must not be opened again"
        );
        let nonce = b"nonce";
        seller
            .state
            .auth
            .issue_challenge("tc-opened", nonce.to_vec());
        let sig = buyer.sign(&crate::seller::auth::challenge_bytes("tc-opened", nonce));
        assert!(
            seller.state.auth.verify_response("tc-opened", nonce, &sig),
            "gateway auth was restored for the matched buyer"
        );
    }

    #[tokio::test]
    async fn handover_absent_and_opened_false_calls_open_stream() {
        let seller = test_seller();
        let cfg = test_cfg("tc-fresh-open");
        let buyer = LocalNote::generate();
        let backend =
            PollBackend::with_state(sample_match("tc-fresh-open", buyer.pubkey()), false, false);

        provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-fresh-open", buyer.pubkey()),
        )
        .await
        .unwrap();

        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn transient_get_state_failure_retries_then_opens() {
        let seller = test_seller();
        let cfg = test_cfg("tc-transient-state");
        let buyer = LocalNote::generate();
        let backend = PollBackend::with_state(
            sample_match("tc-transient-state", buyer.pubkey()),
            true,
            false,
        )
        .with_state_failures(1);

        provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-transient-state", buyer.pubkey()),
        )
        .await
        .unwrap();

        assert_eq!(backend.state_reads.load(Ordering::Relaxed), 2);
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn unreadable_get_state_fails_loud_without_opening() {
        let seller = test_seller();
        let cfg = test_cfg("tc-unreadable-state");
        let buyer = LocalNote::generate();
        let backend = PollBackend::with_state(
            sample_match("tc-unreadable-state", buyer.pubkey()),
            true,
            false,
        )
        .with_state_failures(SELLER_OPEN_STATE_READ_ATTEMPTS as u64);

        let error = provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-unreadable-state", buyer.pubkey()),
        )
        .await
        .expect_err("unreadable getState must fail closed");

        assert!(error
            .to_string()
            .contains("getState unreadable after 3 attempts"));
        assert!(error.to_string().contains("refusing to skip open_stream"));
        assert_eq!(backend.state_reads.load(Ordering::Relaxed), 3);
        assert_eq!(backend.opens.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn provision_match_writes_advertised_gateway_to_handover() {
        let seller = test_seller();
        let mut cfg = test_cfg("tc-advertise");
        cfg.gateway_advertise = "seller.example.net:443".to_string();
        let buyer = LocalNote::generate();
        let backend = PollBackend::new(Some(sample_match("tc-advertise", buyer.pubkey())), None, 1);

        provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-advertise", buyer.pubkey()),
        )
        .await
        .unwrap();

        let enc = backend
            .handover
            .lock()
            .unwrap()
            .clone()
            .expect("handover written");
        let plaintext = buyer.decrypt(&enc).expect("buyer decrypts handover");
        let handover = Handover::from_bytes(&plaintext).expect("handover json");
        assert_eq!(handover.endpoint, "https://seller.example.net:443");
    }
}
