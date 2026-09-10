//! Seller client: gateway + authorization + mock upstream + stream opening.
//! Headless (R12): starts without a GUI and serves the stream as a daemon.

pub mod advance;
pub mod advertise;
pub mod auth;
pub mod capacity;
pub mod gateway;
pub mod keeper;
pub mod liveness;
pub mod models;
pub mod tls;
pub mod upstream;

pub use advance::{
    drive_advance, drive_advance_with_observer, AdvanceWindows, ClaimDeliveryMeasurement,
    ClaimStateObserver,
};
pub use keeper::{
    drive_subscription_keeper, drive_subscription_keeper_with_observer, SubscriptionKeeperObserver,
};
pub use models::{Capabilities, ModelConfig, ModelsConfig, SampleAlgorithm};
pub use upstream::{anthropic::AnthropicConfig, openai::OpenAiConfig, UpstreamConfig};

use anyhow::{anyhow, bail, Result};
use dexdo_core::params::{
    MIN_STREAM_BUY_TICKS, SELLER_OPEN_STATE_INITIAL_BACKOFF, SELLER_OPEN_STATE_READ_ATTEMPTS,
};
use dexdo_core::{
    normalize_wallet_address, order_flags, ChainBackend, DealChainState, DealSubscription,
    Handover, LocalNote, Match, MatchWatchCursor, MatchedFill, Note, OrderBookOrder, SellOffer,
    SellOfferOutcome, TokenContract, SUBSCRIPTION_MAX_TICKS, SUBSCRIPTION_WEEKS,
};
use gateway::{GatewayService, GatewayState};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tls::GatewayTls;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Identity, Server, ServerTlsConfig};

const SUBSCRIPTION_SELL_FLAGS: u8 = order_flags::AON | order_flags::SUBSCRIPTION;
const SELLER_MATCH_WATCH_CURSOR_VERSION: u32 = 1;

fn display_token_contract(token_contract: &str) -> String {
    dexdo_core::address::display_self_dapp(token_contract)
}

/// Seller configuration for one stream.
#[derive(Debug, Clone)]
pub struct SellerConfig {
    /// Contract -- the deal's handover point.
    pub token_contract: TokenContract,
    /// Tick price `P` in raw ECC[2] units. Stated on the command line in whole SHELL
    /// (`--price-per-tick 3`) and converted once, at the argument.
    pub price_per_tick: u64,
    /// Maximum ticks in the offer.
    pub max_ticks: u64,
    /// Whether this SELL accepts only subscription BUYs.
    pub subscription: bool,
    /// Public gateway host:port that will be encrypted to the buyer (R15).
    pub gateway_advertise: String,
    /// How many fake tokens to yield (mock model). `0` = a deliberate seller no-show.
    /// Real upstreams are limited by the buyer request's `max_tokens` and the matched TC's strict
    /// `fundedTokens`/weekly cap, not by this debug fixture or the seller's advertised maximum.
    pub mock_token_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellerOfferStartup {
    ResumedFunded,
    ResumedResting { order_id: u128 },
    Posted { outcome: Option<SellOfferOutcome> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellerOfferInspection {
    Funded,
    Resting { order_id: u128 },
    Vacant,
}

/// A running seller gateway: state handle + handle to the server's background task.
pub struct RunningSeller {
    pub state: Arc<GatewayState>,
    /// The seller's note -- **polymorphic**: `LocalNote` (mock path) OR `RealNote` (a real chain,
    /// one SDK key for signing+handover). The gateway encrypts the endpoint `note.encrypt_to(buyer_pubkey)` -- on
    /// the real path `buyer_pubkey` is reconstructed by the seller from on-chain ed25519 (F1).
    pub note: Arc<dyn Note>,
    pub server_task: tokio::task::JoinHandle<()>,
    /// The socket address actually bound before the server task was spawned.
    pub listen_addr: SocketAddr,
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
    #[serde(with = "dexdo_core::address::serde_self_dapp")]
    token_contract: TokenContract,
    source: MatchWatchCursor,
    last_polled_unix: Option<u64>,
    opened_at_unix: Option<u64>,
    #[serde(default)]
    fill: Option<SellerFillLineage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SellerFillLineage {
    pub order_id: u128,
    pub offered_ticks: u64,
    pub matched_ticks: u64,
    pub residual_ticks: u64,
    pub price_per_tick: u64,
    #[serde(default)]
    pub replacement_nonce: Option<u64>,
    #[serde(default, with = "dexdo_core::address::serde_self_dapp_opt")]
    pub replacement_token_contract: Option<TokenContract>,
}

impl SellerFillLineage {
    fn validate(&self) -> Result<()> {
        if u128::from(self.offered_ticks) < MIN_STREAM_BUY_TICKS
            || self.matched_ticks == 0
            || self.matched_ticks > self.offered_ticks
            || self.residual_ticks != self.offered_ticks - self.matched_ticks
        {
            bail!(
                "invalid seller fill lineage: N={} K={} R={}; expected N>={MIN_STREAM_BUY_TICKS}, 1<=K<=N and R=N-K",
                self.offered_ticks,
                self.matched_ticks,
                self.residual_ticks
            );
        }
        if self.replacement_token_contract.is_some() && self.replacement_nonce.is_none() {
            bail!("invalid seller fill lineage: replacement TokenContract has no reserved nonce");
        }
        Ok(())
    }
}

impl SellerMatchWatchCursor {
    fn new(token_contract: &TokenContract) -> Result<Self> {
        Ok(Self {
            version: SELLER_MATCH_WATCH_CURSOR_VERSION,
            token_contract: token_contract.clone(),
            source: MatchWatchCursor::new(now_unix()? as i64),
            last_polled_unix: None,
            opened_at_unix: None,
            fill: None,
        })
    }

    fn record_fill(
        &mut self,
        cfg: &SellerConfig,
        authoritative_price: u64,
        authoritative_ticks: u64,
        fill: &MatchedFill,
    ) -> Result<bool> {
        if !fill
            .token_contract
            .eq_ignore_ascii_case(&cfg.token_contract)
        {
            bail!(
                "seller fill TokenContract {} does not match {}",
                display_token_contract(&fill.token_contract),
                display_token_contract(&cfg.token_contract)
            );
        }
        let matched_ticks = u64::try_from(fill.ticks)
            .map_err(|_| anyhow!("seller fill ticks {} exceed u64", fill.ticks))?;
        let price_per_tick = u64::try_from(fill.price_per_tick)
            .map_err(|_| anyhow!("seller fill price {} exceeds u64", fill.price_per_tick))?;
        if (cfg.price_per_tick, cfg.max_ticks) != (authoritative_price, authoritative_ticks) {
            bail!(
                "seller config price/ticks ({},{}) do not match TokenContract.getDeal ({},{authoritative_ticks}) for {}",
                dexdo_core::shell_amount(cfg.price_per_tick),
                cfg.max_ticks,
                dexdo_core::shell_amount(authoritative_price),
                display_token_contract(&cfg.token_contract)
            );
        }
        if u128::from(authoritative_ticks) < MIN_STREAM_BUY_TICKS
            || matched_ticks == 0
            || matched_ticks > authoritative_ticks
        {
            bail!(
                "seller fill ticks must be within 1..={} for offer size >={MIN_STREAM_BUY_TICKS}, got {}",
                authoritative_ticks,
                matched_ticks
            );
        }
        if price_per_tick != authoritative_price {
            bail!(
                "seller fill price {price_per_tick} does not match TokenContract.getDeal price {authoritative_price}"
            );
        }
        let next = SellerFillLineage {
            order_id: fill.order_id,
            offered_ticks: authoritative_ticks,
            matched_ticks,
            residual_ticks: authoritative_ticks - matched_ticks,
            price_per_tick,
            replacement_nonce: self.fill.as_ref().and_then(|fill| fill.replacement_nonce),
            replacement_token_contract: self
                .fill
                .as_ref()
                .and_then(|fill| fill.replacement_token_contract.clone()),
        };
        match &self.fill {
            Some(existing) if existing == &next => Ok(false),
            Some(existing) => bail!(
                "conflicting seller fill for {}: existing order_id={} N={} K={}, new order_id={} N={} K={}",
                cfg.token_contract,
                existing.order_id,
                existing.offered_ticks,
                existing.matched_ticks,
                next.order_id,
                next.offered_ticks,
                next.matched_ticks
            ),
            None => {
                self.fill = Some(next);
                Ok(true)
            }
        }
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
                        display_token_contract(&cursor.token_contract),
                        display_token_contract(token_contract)
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

pub fn read_seller_fill_lineage(
    cursor_path: &Path,
    token_contract: &TokenContract,
) -> Result<Option<SellerFillLineage>> {
    let fill = SellerMatchWatchCursor::load_or_new(cursor_path, token_contract)?.fill;
    if let Some(fill) = fill.as_ref() {
        fill.validate()?;
    }
    Ok(fill)
}

pub fn persist_seller_replacement(
    cursor_path: &Path,
    token_contract: &TokenContract,
    nonce: u64,
    replacement_token_contract: Option<&str>,
) -> Result<SellerFillLineage> {
    let token_contract_display = display_token_contract(token_contract);
    let mut cursor = SellerMatchWatchCursor::load_or_new(cursor_path, token_contract)?;
    let fill = cursor.fill.as_mut().ok_or_else(|| {
        anyhow!("seller match for {token_contract_display} has no authoritative owner fill lineage")
    })?;
    fill.validate()?;
    match fill.replacement_nonce {
        Some(existing) if existing != nonce => {
            bail!("seller replacement for {token_contract_display} reserved nonce {existing}, not {nonce}")
        }
        None => fill.replacement_nonce = Some(nonce),
        _ => {}
    }
    if let Some(replacement) = replacement_token_contract {
        match fill.replacement_token_contract.as_deref() {
            Some(existing) if !existing.eq_ignore_ascii_case(replacement) => bail!(
                "seller replacement for {token_contract_display} is linked to {}, not {}",
                display_token_contract(existing),
                display_token_contract(replacement)
            ),
            None => fill.replacement_token_contract = Some(replacement.to_string()),
            _ => {}
        }
    }
    fill.validate()?;
    let result = fill.clone();
    cursor.save(cursor_path)?;
    Ok(result)
}

fn now_unix() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

/// Bring up the seller's gRPC gateway (headless) **over TLS**: a self-signed certificate
/// is generated at startup, its fingerprint is returned for recording in the handover. Returns
/// handles for orchestrating the stream.
pub async fn start_gateway(addr: SocketAddr) -> Result<RunningSeller> {
    start_gateway_with(addr, UpstreamConfig::Mock).await
}

/// Like [`start_gateway`], but with an upstream choice (mock model or real OpenAI-compatible,
/// ). The mock path (`UpstreamConfig::Mock`) is identical to.
pub async fn start_gateway_with(
    addr: SocketAddr,
    upstream: UpstreamConfig,
) -> Result<RunningSeller> {
    // The ephemeral note is a mock fixture; the production path is `start_gateway_with_note`.
    start_gateway_with_note(addr, upstream, Arc::new(LocalNote::generate())).await
}

/// Like [`start_gateway_with`], but with a **loaded persistent** seller note:
/// the identity (from `--note-key`/wallet) is reused across runs -- its offer/deals are
/// visible in the next run. `start_gateway_with` substitutes an ephemeral `generate()` here.
pub async fn start_gateway_with_note(
    addr: SocketAddr,
    upstream: UpstreamConfig,
    note: Arc<dyn Note>,
) -> Result<RunningSeller> {
    start_gateway_with_note_tls(addr, upstream, note, GatewayTls::generate()?).await
}

pub async fn start_gateway_with_note_tls(
    addr: SocketAddr,
    upstream: UpstreamConfig,
    note: Arc<dyn Note>,
    gw_tls: GatewayTls,
) -> Result<RunningSeller> {
    start_gateway_with_note_and_capacity_dir(addr, upstream, note, None, gw_tls).await
}

/// Production seller gateway with crash-safe subscription capacity stored beside deal handles.
pub async fn start_gateway_with_note_and_deals_dir(
    addr: SocketAddr,
    upstream: UpstreamConfig,
    note: Arc<dyn Note>,
    deals_dir: PathBuf,
) -> Result<RunningSeller> {
    start_gateway_with_note_and_capacity_dir(
        addr,
        upstream,
        note,
        Some(deals_dir),
        GatewayTls::generate()?,
    )
    .await
}

/// Production seller gateway with both a persistent TLS identity and crash-safe capacity state.
pub async fn start_gateway_with_note_tls_and_deals_dir(
    addr: SocketAddr,
    upstream: UpstreamConfig,
    note: Arc<dyn Note>,
    deals_dir: PathBuf,
    gw_tls: GatewayTls,
) -> Result<RunningSeller> {
    start_gateway_with_note_and_capacity_dir(addr, upstream, note, Some(deals_dir), gw_tls).await
}

async fn start_gateway_with_note_and_capacity_dir(
    addr: SocketAddr,
    upstream: UpstreamConfig,
    note: Arc<dyn Note>,
    deals_dir: Option<PathBuf>,
    gw_tls: GatewayTls,
) -> Result<RunningSeller> {
    // this process is about to serve buyers, and a buyer that hangs up mid-stream must give
    // this gateway an `EPIPE` to handle rather than a signal that kills it. `main` restores the
    // default SIGPIPE disposition for one-shot printers; a gateway needs it ignored. Do not delete
    // this as a duplicate of the entry policy -- it is the opposite decision, for the opposite kind
    // of process, and the cost of losing it is a seller that dies mid-delivery.
    crate::serving_process_ignores_sigpipe();
    let state = Arc::new(match deals_dir {
        Some(deals_dir) => GatewayState::with_upstream_and_deals_dir(upstream, deals_dir),
        None => GatewayState::with_upstream(upstream),
    });
    let service = GatewayService::new(state.clone()).into_server();

    // Both rustls providers (ring/aws-lc-rs) are present in the tree; pin the process
    // default explicitly (ring) -- otherwise rustls panics, unable to pick on its own. Idempotent.
    tls::ensure_crypto_provider();

    let tls_fingerprint = gw_tls.fingerprint.clone();
    let identity = Identity::from_pem(gw_tls.cert_pem, gw_tls.key_pem);
    let tls_config = ServerTlsConfig::new().identity(identity);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| anyhow!("bind seller gateway {addr}: {error}"))?;
    let listen_addr = listener
        .local_addr()
        .map_err(|error| anyhow!("read bound seller gateway address: {error}"))?;
    let incoming = TcpListenerStream::new(listener);
    let mut builder = Server::builder()
        .tls_config(tls_config)
        .map_err(|error| anyhow!("configure seller gateway TLS: {error}"))?;

    let server_task = tokio::spawn(async move {
        if let Err(error) = builder
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
        {
            tracing::error!("gateway server stopped: {error}");
        }
    });
    Ok(RunningSeller {
        state,
        note,
        server_task,
        listen_addr,
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

/// Like [`post_offer`], but uses a note directly. The CLI calls this only after
/// gateway, advertised endpoint and exact upstream readiness have passed.
pub async fn post_offer_with_note(
    note: &dyn Note,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
) -> Result<()> {
    validate_subscription_offer_volume(cfg)?;
    let offer = SellOffer {
        price_per_tick: cfg.price_per_tick,
        max_ticks: cfg.max_ticks,
        token_contract: cfg.token_contract.clone(),
        flags: if cfg.subscription {
            SUBSCRIPTION_SELL_FLAGS
        } else {
            0
        },
    };
    chain.post_offer(offer, note).await?;
    Ok(())
}

fn validate_subscription_offer_volume(cfg: &SellerConfig) -> Result<()> {
    let term = u64::from(SUBSCRIPTION_WEEKS);
    if cfg.subscription
        && (cfg.max_ticks == 0
            || u128::from(cfg.max_ticks) > SUBSCRIPTION_MAX_TICKS
            || !cfg.max_ticks.is_multiple_of(term))
    {
        bail!(
            "seller subscription offer for TokenContract {} requires non-zero max_ticks no greater \
             than {SUBSCRIPTION_MAX_TICKS} and divisible by the fixed {SUBSCRIPTION_WEEKS}-week \
             term, got {}",
            display_token_contract(&cfg.token_contract),
            cfg.max_ticks
        );
    }
    Ok(())
}

fn resting_offer_error(token_contract: &str, reason: impl std::fmt::Display) -> anyhow::Error {
    let token_contract = display_token_contract(token_contract);
    anyhow!(
        "seller cannot safely resume or post TokenContract {token_contract}: {reason}. \
         Check the raw order book and cancel the existing order before retrying."
    )
}

fn validate_resting_offer(
    order: &OrderBookOrder,
    expected_owner: Option<&str>,
    cfg: &SellerConfig,
) -> Result<()> {
    if order.is_buy {
        return Err(resting_offer_error(
            &cfg.token_contract,
            format!("order {} is not a SELL", order.order_id),
        ));
    }
    let order_tc = order.token_contract.as_deref().ok_or_else(|| {
        resting_offer_error(
            &cfg.token_contract,
            format!(
                "raw order {} is missing the required token_contract fact",
                order.order_id
            ),
        )
    })?;
    let wanted_tc = normalize_wallet_address(&cfg.token_contract).map_err(|error| {
        resting_offer_error(
            &cfg.token_contract,
            format!("expected TokenContract address is invalid: {error}"),
        )
    })?;
    let actual_tc = normalize_wallet_address(order_tc).map_err(|error| {
        resting_offer_error(
            &cfg.token_contract,
            format!(
                "raw order {} has an invalid token_contract fact: {error}",
                order.order_id
            ),
        )
    })?;
    if actual_tc != wanted_tc {
        return Err(resting_offer_error(
            &cfg.token_contract,
            format!(
                "raw order {} belongs to TokenContract {}, not this TC",
                order.order_id,
                display_token_contract(order_tc)
            ),
        ));
    }

    let expected_owner = expected_owner.ok_or_else(|| {
        resting_offer_error(
            &cfg.token_contract,
            "the seller note owner is unavailable for raw order verification",
        )
    })?;
    let wanted_owner = normalize_wallet_address(expected_owner).map_err(|error| {
        resting_offer_error(
            &cfg.token_contract,
            format!("expected seller note owner is invalid: {error}"),
        )
    })?;
    let actual_owner = normalize_wallet_address(&order.owner_note).map_err(|error| {
        resting_offer_error(
            &cfg.token_contract,
            format!(
                "raw order {} is missing or has an invalid owner fact: {error}",
                order.order_id
            ),
        )
    })?;
    if actual_owner != wanted_owner {
        return Err(resting_offer_error(
            &cfg.token_contract,
            format!(
                "raw order {} owner {} does not match seller note {}",
                order.order_id,
                dexdo_core::address::display(&order.owner_note),
                dexdo_core::address::display(expected_owner)
            ),
        ));
    }
    if order.price_per_tick != u128::from(cfg.price_per_tick) {
        return Err(resting_offer_error(
            &cfg.token_contract,
            format!(
                "book order {} price_per_tick {} does not match {}",
                order.order_id,
                dexdo_core::shell_amount(order.price_per_tick),
                dexdo_core::shell_amount(cfg.price_per_tick)
            ),
        ));
    }
    if order.ticks != u128::from(cfg.max_ticks) {
        return Err(resting_offer_error(
            &cfg.token_contract,
            format!(
                "raw order {} remaining ticks {} do not match max ticks {}",
                order.order_id, order.ticks, cfg.max_ticks
            ),
        ));
    }
    let expected_flags = if cfg.subscription {
        SUBSCRIPTION_SELL_FLAGS
    } else {
        0
    };
    if order.flags != expected_flags {
        return Err(resting_offer_error(
            &cfg.token_contract,
            format!(
                "raw order {} flags 0x{:02x} do not match required seller flags 0x{:02x}",
                order.order_id, order.flags, expected_flags
            ),
        ));
    }
    Ok(())
}

/// Read the authoritative seller state without posting. This lets gateway/upstream readiness run
/// before a fresh SELL while still identifying an existing resting SELL that must be cancelled on
/// failed restart readiness.
pub async fn inspect_seller_offer(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    expected_owner: Option<&str>,
) -> Result<SellerOfferInspection> {
    validate_subscription_offer_volume(cfg)?;
    match chain.read_openable_match_now(&cfg.token_contract).await {
        Ok(Some(_)) => return Ok(SellerOfferInspection::Funded),
        Ok(None) => {}
        Err(error) => {
            return Err(anyhow!(
                "seller: existing-match resume preflight failed for {}: {error}",
                display_token_contract(&cfg.token_contract)
            ));
        }
    }

    let raw_orders = chain
        .raw_resting_sell_orders_for_tc(&cfg.token_contract)
        .await
        .map_err(|error| {
            resting_offer_error(
                &cfg.token_contract,
                format!("authoritative raw order-book read failed: {error}"),
            )
        })?;
    match raw_orders.as_slice() {
        [] => Ok(SellerOfferInspection::Vacant),
        [order] => {
            validate_resting_offer(order, expected_owner, cfg)?;
            Ok(SellerOfferInspection::Resting {
                order_id: order.order_id,
            })
        }
        orders => {
            let ids = orders
                .iter()
                .map(|order| order.order_id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Err(resting_offer_error(
                &cfg.token_contract,
                format!(
                    "ambiguous raw order book has {} active SELL rows for this TC (order ids {ids})",
                    orders.len()
                ),
            ))
        }
    }
}

/// Classify authoritative startup state after readiness. A funded match resumes the existing
/// match path; one exact raw resting SELL resumes its watcher; only an empty exact-TC result permits
/// one fresh post.
pub async fn prepare_seller_offer(
    note: &dyn Note,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    expected_owner: Option<&str>,
) -> Result<SellerOfferStartup> {
    match inspect_seller_offer(chain, cfg, expected_owner).await? {
        SellerOfferInspection::Vacant => {
            chain
                .assert_token_contract_fresh(&cfg.token_contract)
                .await?;
            post_offer_with_note(note, chain, cfg).await?;
            let outcome = chain.confirm_offer_outcome(&cfg.token_contract).await?;
            Ok(SellerOfferStartup::Posted { outcome })
        }
        SellerOfferInspection::Resting { order_id } => {
            Ok(SellerOfferStartup::ResumedResting { order_id })
        }
        SellerOfferInspection::Funded => Ok(SellerOfferStartup::ResumedFunded),
    }
}

/// Open the stream for a match:
/// 1. reads the match (the buyer's pubkey is recorded in the contract);
/// 2. encrypts the endpoint to the buyer's pubkey and `open_stream` (probe freeze +
/// exact `2P` seller bond + writing the enc-endpoint into the endpoints file);
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
    let token_contract_display = display_token_contract(token_contract);
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
                token_contract = %token_contract_display,
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
        "TokenContract {token_contract_display} getState unreadable after {SELLER_OPEN_STATE_READ_ATTEMPTS} attempts; refusing to skip open_stream: {last_failure}"
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
            display_token_contract(&m.token_contract),
            display_token_contract(&cfg.token_contract)
        );
    }
    // the handover {gateway endpoint, TLS fingerprint} is encrypted to the buyer's pubkey.
    // The endpoint points at the GATEWAY over TLS (R15); the buyer pins the fingerprint on connect.
    let handover = Handover {
        endpoint: format!("https://{}", cfg.gateway_advertise),
        tls_fingerprint: seller.tls_fingerprint.clone(),
    };
    // The payload names the deal it is written for, so the buyer can refuse it on any other deal
    // (E2E-OPEN-07 wrong-deal replay) before it treats that deal as opened.
    let enc = seller.note.encrypt_to(
        &m.buyer_pubkey,
        &handover.to_deal_bytes(&cfg.token_contract),
    );

    // the gateway must authorize the matched buyer BEFORE that buyer can connect.
    // Register buyer+budget BEFORE writing the handover on-chain: the buyer learns the endpoint only
    // after reading the on-chain ciphertext (written by `open_stream`), so register-before-open rules out a race. Otherwise on a
    // real (slow) chain the buyer manages to knock in the window between open_stream and register_stream
    // -> the gateway still has no pubkey -> `challenge-response failed` (the mock timing did not expose this).
    let (state, deal) = read_coherent_deal_capacity(chain, &cfg.token_contract).await?;
    if cfg.subscription != deal.is_subscription() {
        let expected = if cfg.subscription {
            "subscription"
        } else {
            "ordinary"
        };
        let observed = if deal.is_subscription() {
            "subscription"
        } else {
            "ordinary"
        };
        bail!(
            "TokenContract {}: seller configured {expected} mode but strict deal snapshot is {observed}",
            display_token_contract(&cfg.token_contract)
        );
    }
    seller.state.register_stream(
        &cfg.token_contract,
        m.buyer_pubkey,
        cfg.mock_token_count,
        state,
        deal,
    )?;
    if !read_opened_with_retry(chain, &cfg.token_contract).await? {
        chain
            .open_stream(&cfg.token_contract, enc, seller.note.as_ref())
            .await?;
    } else {
        tracing::info!(
            token_contract = %display_token_contract(&cfg.token_contract),
            "seller gateway restored auth for opened deal; skipping duplicate open_stream"
        );
    }
    Ok(())
}

async fn read_coherent_deal_capacity(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
) -> Result<(DealChainState, DealSubscription)> {
    let snapshot = chain.deal_snapshot(token_contract).await?.ok_or_else(|| {
        anyhow!(
            "TokenContract {}: coherent deal snapshot returned no capacity data",
            display_token_contract(token_contract)
        )
    })?;
    Ok((snapshot.state, snapshot.subscription))
}

/// Perform only the read-only match poll, leaving provisioning to the caller.
async fn poll_match(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    cursor_path: &Path,
) -> Result<(SellerMatchWatchCursor, Option<Match>)> {
    let mut cursor = SellerMatchWatchCursor::load_or_new(cursor_path, &cfg.token_contract)?;
    cursor.last_polled_unix = Some(now_unix()?);
    let fills = chain
        .poll_seller_fills(seller.note.as_ref(), &mut cursor.source)
        .await?;
    let mut matching = fills
        .into_iter()
        .filter(|fill| {
            fill.token_contract
                .eq_ignore_ascii_case(&cfg.token_contract)
        })
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        bail!(
            "seller fill poll returned {} fills for TokenContract {}",
            matching.len(),
            display_token_contract(&cfg.token_contract)
        );
    }
    if let Some(fill) = matching.pop() {
        let (authoritative_price, authoritative_ticks) = chain
            .sell_offer_terms(&cfg.token_contract)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "TokenContract {} getDeal is unavailable for authoritative seller fill accounting",
                    display_token_contract(&cfg.token_contract)
                )
            })?;
        cursor.record_fill(cfg, authoritative_price, authoritative_ticks, &fill)?;
        cursor.save(cursor_path)?;
    }
    let mut openable_match = None;
    if cursor.fill.is_none() {
        openable_match = chain.read_openable_match_now(&cfg.token_contract).await?;
        if cursor.opened_at_unix.is_some() || openable_match.is_some() {
            let mut history = MatchWatchCursor::new(0);
            let mut fills = chain
                .poll_seller_fills(seller.note.as_ref(), &mut history)
                .await?
                .into_iter()
                .filter(|fill| {
                    fill.token_contract
                        .eq_ignore_ascii_case(&cfg.token_contract)
                })
                .collect::<Vec<_>>();
            if fills.len() > 1 {
                bail!(
                    "seller owner history returned {} fills for TokenContract {}",
                    fills.len(),
                    display_token_contract(&cfg.token_contract)
                );
            }
            let fill = fills.pop().ok_or_else(|| {
                anyhow!(
                    "legacy seller cursor for opened TokenContract {} has no persisted fill and \
                     the authoritative owner history cannot recover it; refusing to resume or \
                     start advance with guessed capacity",
                    display_token_contract(&cfg.token_contract)
                )
            })?;
            let (authoritative_price, authoritative_ticks) = chain
                .sell_offer_terms(&cfg.token_contract)
                .await?
                .ok_or_else(|| {
                    anyhow!(
                        "TokenContract {} getDeal is unavailable while recovering legacy seller fill",
                        display_token_contract(&cfg.token_contract)
                    )
                })?;
            cursor.record_fill(cfg, authoritative_price, authoritative_ticks, &fill)?;
            cursor.save(cursor_path)?;
        }
    }
    let found = if cursor.fill.is_some() {
        // A restart must restore in-memory gateway auth for an already-opened
        // deal even though the durable source cursor suppresses the old fill.
        Some(chain.read_match(&cfg.token_contract).await?)
    } else {
        openable_match
    };
    Ok((cursor, found))
}

/// Poll once for a match and provision access if one exists. The cursor is saved on every successful poll so a
/// restarted gateway continues from the same source position instead of rereading the note event window forever.
pub async fn poll_match_and_maybe_open(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    cursor_path: &Path,
) -> Result<Option<Match>> {
    let (mut cursor, found) = poll_match(seller, chain, cfg, cursor_path).await?;
    cursor.save(cursor_path)?;
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

/// Wait for one authoritative match without beginning the on-chain handover write.

/// Keeping this phase read-only lets the resting-offer supervisor select shutdown/health safely. Once a
/// match is observed, [`serve_watched_match`] runs the existing handover path to completion outside that
/// cancellable select.
pub async fn wait_for_match(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
) -> Result<Match> {
    loop {
        let (cursor, found) = match poll_match(seller, chain, cfg, &watch.cursor_path).await {
            Ok(polled) => polled,
            Err(error)
                if error
                    .downcast_ref::<dexdo_core::ChainError>()
                    .is_some_and(|error| matches!(error, dexdo_core::ChainError::Transport(_))) =>
            {
                tracing::warn!(
                    event = "seller_match_watch_network_error",
                    token_contract = %display_token_contract(&cfg.token_contract),
                    error = %error,
                    "transient seller match-watch network error; keeping gateway alive"
                );
                tokio::time::sleep(watch.poll_interval).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Some(m) = found {
            cursor.save(&watch.cursor_path)?;
            return Ok(m);
        }
        cursor.save(&watch.cursor_path)?;
        tokio::time::sleep(watch.poll_interval).await;
    }
}

pub async fn serve_watched_match(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
    matched: Match,
) -> Result<Match> {
    provision_match(seller, chain, cfg, matched.clone()).await?;
    let mut cursor = SellerMatchWatchCursor::load_or_new(&watch.cursor_path, &cfg.token_contract)?;
    cursor.opened_at_unix.get_or_insert(now_unix()?);
    cursor.save(&watch.cursor_path)?;
    Ok(matched)
}

/// Gateway-owned match watcher. This is intentionally an indefinite loop: as long as the offer remains a valid
/// resting/openable deal, no five-minute seller timeout tears down the process.
pub async fn watch_and_serve_match(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
) -> Result<Match> {
    let matched = wait_for_match(seller, chain, cfg, watch).await?;
    serve_watched_match(seller, chain, cfg, watch, matched).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{
        validate_seller_resume_state, ChainError, DealBuyerBond, DealChainSnapshot, DealChainState,
        DealSellerBond, LocalNote, NotePubkey, OfferListing, SellOffer, Settlement, StreamSnapshot,
    };
    use dexdo_proto::{CanonRequest, ChallengeRequest, GatewayClient, StreamRequest};
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;

    async fn gateway_request(
        client: &mut GatewayClient<tonic::transport::Channel>,
        note: &dyn Note,
        token_contract: &str,
    ) -> StreamRequest {
        let challenge = client
            .get_challenge(ChallengeRequest {
                token_contract: token_contract.to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        StreamRequest {
            token_contract: token_contract.to_string(),
            signature: note
                .sign(&crate::seller::auth::challenge_bytes(
                    token_contract,
                    &challenge.nonce,
                ))
                .0
                .to_vec(),
            nonce: challenge.nonce,
            request: Some(CanonRequest {
                params: Some(dexdo_proto::SamplingParams {
                    max_tokens: 1,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            billing_grant_tokens: 1,
        }
    }

    struct PollBackend {
        matched: Option<Match>,
        handover: Mutex<Option<Vec<u8>>>,
        opens: AtomicU64,
        open_failures_remaining: AtomicU64,
        opened: AtomicBool,
        poll_failures_remaining: AtomicU64,
        polls: AtomicU64,
        state_failures_remaining: AtomicU64,
        state_reads: AtomicU64,
        subscription: bool,
        expect_last_seen: Option<i64>,
        record_created_at: i64,
        matched_ticks: u128,
        offer_ticks: u64,
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
                open_failures_remaining: AtomicU64::new(0),
                opened: AtomicBool::new(false),
                poll_failures_remaining: AtomicU64::new(0),
                polls: AtomicU64::new(0),
                state_failures_remaining: AtomicU64::new(0),
                state_reads: AtomicU64::new(0),
                subscription: false,
                expect_last_seen,
                record_created_at,
                matched_ticks: 8,
                offer_ticks: 8,
            }
        }

        fn with_state(matched: Match, handover_present: bool, opened: bool) -> Self {
            Self {
                matched: Some(matched),
                handover: Mutex::new(handover_present.then(|| b"existing-handover".to_vec())),
                opens: AtomicU64::new(0),
                open_failures_remaining: AtomicU64::new(0),
                opened: AtomicBool::new(opened),
                poll_failures_remaining: AtomicU64::new(0),
                polls: AtomicU64::new(0),
                state_failures_remaining: AtomicU64::new(0),
                state_reads: AtomicU64::new(0),
                subscription: false,
                expect_last_seen: None,
                record_created_at: 1,
                matched_ticks: 8,
                offer_ticks: 8,
            }
        }

        fn with_state_failures(mut self, failures: u64) -> Self {
            self.state_failures_remaining = AtomicU64::new(failures);
            self
        }

        fn with_poll_failures(mut self, failures: u64) -> Self {
            self.poll_failures_remaining = AtomicU64::new(failures);
            self
        }

        fn with_open_failures(mut self, failures: u64) -> Self {
            self.open_failures_remaining = AtomicU64::new(failures);
            self
        }

        fn with_matched_ticks(mut self, ticks: u128) -> Self {
            self.matched_ticks = ticks;
            self
        }

        fn with_offer_ticks(mut self, ticks: u64) -> Self {
            self.offer_ticks = ticks;
            self
        }

        fn with_subscription(mut self) -> Self {
            self.subscription = true;
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

        async fn poll_seller_fills(
            &self,
            _: &dyn Note,
            cursor: &mut MatchWatchCursor,
        ) -> Result<Vec<MatchedFill>, ChainError> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            if self.poll_failures_remaining.load(Ordering::Relaxed) > 0 {
                self.poll_failures_remaining.fetch_sub(1, Ordering::Relaxed);
                return Err(ChainError::Transport("connection reset".to_string()));
            }
            if let Some(expected) = self.expect_last_seen {
                assert_eq!(cursor.last_seen_created_at, Some(expected));
            }
            let Some(matched) = &self.matched else {
                return Ok(Vec::new());
            };
            if cursor.has_seen(self.record_created_at, &matched.token_contract) {
                return Ok(Vec::new());
            }
            cursor.record_seen_batch([(self.record_created_at, matched.token_contract.clone())]);
            Ok(vec![MatchedFill {
                order_id: 1,
                token_contract: matched.token_contract.clone(),
                ticks: self.matched_ticks,
                price_per_tick: u128::from(matched.price_per_tick),
            }])
        }

        async fn sell_offer_terms(
            &self,
            _: &TokenContract,
        ) -> Result<Option<(u64, u64)>, ChainError> {
            Ok(self
                .matched
                .as_ref()
                .map(|matched| (matched.price_per_tick, self.offer_ticks)))
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
            if self.open_failures_remaining.load(Ordering::Relaxed) > 0 {
                self.open_failures_remaining.fetch_sub(1, Ordering::Relaxed);
                return Err(ChainError::Transport(
                    "timeout after signed writes".to_string(),
                ));
            }
            self.handover.lock().unwrap().replace(enc_endpoint);
            self.opened.store(true, Ordering::Relaxed);
            Ok(())
        }

        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            Ok(self.handover.lock().unwrap().clone())
        }

        async fn claim_tokens(
            &self,
            _: &TokenContract,
            _: &dyn Note,
            _: u128,
        ) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
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
                opened: self.opened.load(Ordering::Relaxed),
                probe_accepted: false,
                disputed: false,
                deposit: 1_000,
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                funded_time: Some(1),
                probe_tick: 0,
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: 0,
            }))
        }

        async fn deal_subscription(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealSubscription>, ChainError> {
            let funded_tokens = 8 * dexdo_core::TICK_SIZE;
            Ok(Some(if self.subscription {
                DealSubscription {
                    deal_flags: order_flags::SUBSCRIPTION,
                    sub_weeks: 4,
                    week_index: 0,
                    tokens_per_week: 2 * dexdo_core::TICK_SIZE,
                    funded_tokens,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                }
            } else {
                DealSubscription {
                    deal_flags: 0,
                    sub_weeks: 0,
                    week_index: 0,
                    tokens_per_week: funded_tokens,
                    funded_tokens,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                }
            }))
        }

        async fn deal_snapshot(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainSnapshot>, ChainError> {
            self.state_reads.fetch_add(1, Ordering::Relaxed);
            let state = DealChainState {
                funded: true,
                opened: self.opened.load(Ordering::Relaxed),
                probe_accepted: false,
                disputed: false,
                deposit: 1_000,
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                funded_time: Some(1),
                probe_tick: 0,
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: 0,
            };
            let funded_tokens = 8 * dexdo_core::TICK_SIZE;
            let subscription = if self.subscription {
                DealSubscription {
                    deal_flags: order_flags::SUBSCRIPTION,
                    sub_weeks: 4,
                    week_index: 0,
                    tokens_per_week: 2 * dexdo_core::TICK_SIZE,
                    funded_tokens,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                }
            } else {
                DealSubscription {
                    deal_flags: 0,
                    sub_weeks: 0,
                    week_index: 0,
                    tokens_per_week: funded_tokens,
                    funded_tokens,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                }
            };
            Ok(Some(DealChainSnapshot {
                account_code_hash: "test-code".to_string(),
                account_boc_hash: "test-boc".to_string(),
                state,
                subscription,
                seller_bond: DealSellerBond {
                    bond_funded: true,
                    bond_held: 1,
                    bond_required: 1,
                },
                buyer_bond: DealBuyerBond {
                    bond_held: u128::from(self.subscription),
                    bond_required: u128::from(self.subscription),
                },
            }))
        }

        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            None
        }
    }

    #[derive(Clone)]
    enum RawStartupRead {
        Orders(Vec<OrderBookOrder>),
        ChainFailure,
        TransportFailure,
    }

    fn resume_state(deposit: u128) -> DealChainState {
        DealChainState {
            funded: true,
            opened: false,
            probe_accepted: false,
            disputed: false,
            deposit,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 0,
            last_claim_time: 0,
            dispute_time: 0,
        }
    }

    struct StartupBackend {
        raw: RawStartupRead,
        startup_match: Option<Match>,
        watcher_match: Option<Match>,
        post_calls: AtomicU64,
        posted_offer: Mutex<Option<SellOffer>>,
        raw_reads: AtomicU64,
        freshness_checks: AtomicU64,
        poll_calls: AtomicU64,
        poll_failures_remaining: AtomicU64,
        open_calls: AtomicU64,
        startup_failure: Option<String>,
        resume_facts: Option<(DealChainState, u64)>,
    }

    impl StartupBackend {
        fn new(raw: RawStartupRead, startup_match: Option<Match>, watcher_match: Match) -> Self {
            Self {
                raw,
                startup_match,
                watcher_match: Some(watcher_match),
                post_calls: AtomicU64::new(0),
                posted_offer: Mutex::new(None),
                raw_reads: AtomicU64::new(0),
                freshness_checks: AtomicU64::new(0),
                poll_calls: AtomicU64::new(0),
                poll_failures_remaining: AtomicU64::new(0),
                open_calls: AtomicU64::new(0),
                startup_failure: None,
                resume_facts: None,
            }
        }

        fn without_match(raw: RawStartupRead) -> Self {
            Self {
                raw,
                startup_match: None,
                watcher_match: None,
                post_calls: AtomicU64::new(0),
                posted_offer: Mutex::new(None),
                raw_reads: AtomicU64::new(0),
                freshness_checks: AtomicU64::new(0),
                poll_calls: AtomicU64::new(0),
                poll_failures_remaining: AtomicU64::new(0),
                open_calls: AtomicU64::new(0),
                startup_failure: None,
                resume_facts: None,
            }
        }

        fn with_poll_failures(mut self, failures: u64) -> Self {
            self.poll_failures_remaining = AtomicU64::new(failures);
            self
        }

        fn with_startup_failure(mut self, failure: impl Into<String>) -> Self {
            self.startup_failure = Some(failure.into());
            self
        }

        fn with_resume_facts(mut self, state: DealChainState, price_per_tick: u64) -> Self {
            self.resume_facts = Some((state, price_per_tick));
            self
        }
    }

    #[async_trait::async_trait]
    impl ChainBackend for StartupBackend {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            panic!("seller startup must not call discover_offers")
        }

        async fn raw_resting_sell_orders_for_tc(
            &self,
            _: &TokenContract,
        ) -> Result<Vec<OrderBookOrder>, ChainError> {
            self.raw_reads.fetch_add(1, Ordering::Relaxed);
            match &self.raw {
                RawStartupRead::Orders(orders) => Ok(orders.clone()),
                RawStartupRead::ChainFailure => {
                    Err(ChainError::Chain("raw book getter failed".to_string()))
                }
                RawStartupRead::TransportFailure => {
                    Err(ChainError::Transport("raw book timeout".to_string()))
                }
            }
        }

        async fn assert_token_contract_fresh(&self, _: &TokenContract) -> Result<(), ChainError> {
            self.freshness_checks.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn post_offer(&self, offer: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            self.post_calls.fetch_add(1, Ordering::Relaxed);
            self.posted_offer.lock().unwrap().replace(offer);
            Ok(())
        }

        async fn confirm_offer_outcome(
            &self,
            _: &TokenContract,
        ) -> Result<Option<SellOfferOutcome>, ChainError> {
            Ok(Some(SellOfferOutcome::Rested { order_id: 835 }))
        }

        async fn read_openable_match_now(
            &self,
            token_contract: &TokenContract,
        ) -> Result<Option<Match>, ChainError> {
            if let Some(failure) = &self.startup_failure {
                return Err(ChainError::Chain(failure.clone()));
            }
            if let Some((state, price_per_tick)) = &self.resume_facts {
                validate_seller_resume_state(token_contract, *state, *price_per_tick)?;
            }
            Ok(self.startup_match.clone())
        }

        async fn poll_seller_fills(
            &self,
            _: &dyn Note,
            cursor: &mut MatchWatchCursor,
        ) -> Result<Vec<MatchedFill>, ChainError> {
            self.poll_calls.fetch_add(1, Ordering::Relaxed);
            if self.poll_failures_remaining.load(Ordering::Relaxed) > 0 {
                self.poll_failures_remaining.fetch_sub(1, Ordering::Relaxed);
                return Err(ChainError::Transport("temporary watch timeout".to_string()));
            }
            let Some(matched) = &self.watcher_match else {
                return Ok(Vec::new());
            };
            cursor.record_seen_batch([(1, matched.token_contract.clone())]);
            Ok(vec![MatchedFill {
                order_id: 835,
                token_contract: matched.token_contract.clone(),
                ticks: 8,
                price_per_tick: u128::from(matched.price_per_tick),
            }])
        }

        async fn sell_offer_terms(
            &self,
            _: &TokenContract,
        ) -> Result<Option<(u64, u64)>, ChainError> {
            Ok(self
                .watcher_match
                .as_ref()
                .map(|matched| (matched.price_per_tick, 8)))
        }

        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
            self.watcher_match
                .clone()
                .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))
        }

        async fn open_stream(
            &self,
            _: &TokenContract,
            _: Vec<u8>,
            _: &dyn Note,
        ) -> Result<(), ChainError> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            Ok(None)
        }

        async fn claim_tokens(
            &self,
            _: &TokenContract,
            _: &dyn Note,
            _: u128,
        ) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }

        async fn deal_state(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            Ok(Some(DealChainState {
                funded: true,
                opened: false,
                probe_accepted: true,
                disputed: false,
                deposit: 1_000,
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                funded_time: Some(1),
                probe_tick: 0,
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: 0,
            }))
        }

        async fn deal_snapshot(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainSnapshot>, ChainError> {
            let state = self
                .resume_facts
                .as_ref()
                .map(|(state, _)| *state)
                .unwrap_or(DealChainState {
                    funded: true,
                    opened: false,
                    probe_accepted: false,
                    disputed: false,
                    deposit: 1_000,
                    finalized_owed: 0,
                    tokens_final: 0,
                    tokens_pending: 0,
                    funded_time: Some(1),
                    probe_tick: 0,
                    probe_time: 0,
                    last_claim_time: 0,
                    dispute_time: 0,
                });
            let funded_tokens = 8 * dexdo_core::TICK_SIZE;
            Ok(Some(DealChainSnapshot {
                account_code_hash: "test-code".to_string(),
                account_boc_hash: "test-boc".to_string(),
                state,
                subscription: DealSubscription {
                    deal_flags: 0,
                    sub_weeks: 0,
                    week_index: 0,
                    tokens_per_week: funded_tokens,
                    funded_tokens,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                },
                seller_bond: DealSellerBond {
                    bond_funded: true,
                    bond_held: 1,
                    bond_required: 1,
                },
                buyer_bond: DealBuyerBond {
                    bond_held: 0,
                    bond_required: 0,
                },
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
            server_task: tokio::spawn(std::future::pending()),
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            tls_fingerprint: "test-fingerprint".to_string(),
        }
    }

    fn test_cfg(token_contract: &str) -> SellerConfig {
        SellerConfig {
            token_contract: token_contract.to_string(),
            price_per_tick: 3_000_000_000,
            max_ticks: 8,
            subscription: false,
            gateway_advertise: "127.0.0.1:8443".to_string(),
            mock_token_count: 8,
        }
    }

    /// the directory is returned with the path and must be held for as long as the cursor is
    /// read or written -- the previous `<pid>-<seconds>` directory was never removed.
    fn temp_cursor_path(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("dexdo-seller-watch-test")
            .tempdir()
            .expect("seller watch cursor directory");
        let path = dir.path().join(format!("{name}.json"));
        (dir, path)
    }

    fn sample_match(token_contract: &str, buyer_pubkey: NotePubkey) -> Match {
        Match {
            token_contract: token_contract.to_string(),
            buyer_pubkey,
            price_per_tick: 3_000_000_000,
        }
    }

    fn chain_address(hex_digit: char) -> String {
        format!("0:{}", hex_digit.to_string().repeat(64))
    }

    fn raw_sell(
        order_id: u128,
        owner_note: &str,
        token_contract: Option<&str>,
        price_per_tick: u128,
        ticks: u128,
    ) -> OrderBookOrder {
        OrderBookOrder {
            order_id,
            owner_note: owner_note.to_string(),
            token_contract: token_contract.map(str::to_string),
            is_buy: false,
            price_per_tick,
            ticks,
            escrow: 0,
            deadline: 0,
            flags: 0,
            timestamp: 1,
        }
    }

    /// Issues ** /** at a money-path consumer, where the equality DECIDES something.

    /// The helpers that drop the dapp id -- `to_chain_param`, `normalize_wallet_address`,
    /// `parse_chain_address` -- are all documented to yield the workchain form, so asserting that
    /// they preserve it would be asserting against intended behaviour. The defect is not the
    /// conversion; it is that consumers then treat two DIFFERENT accounts as one.

    /// [`validate_resting_offer`] (`seller/mod.rs:508`) is such a consumer, and the decision it
    /// makes is whether this seller ADOPTS a resting SELL as its own. It normalises both sides
    /// through `normalize_wallet_address` (`core/src/wallet.rs:20-24`, which is
    /// `CanonicalAddress::parse(..).legacy()`) and compares:

    /// ```text
    /// if actual_tc != wanted_tc { return Err(..) } //:546
    /// if actual_owner != wanted_owner { return Err(..) } // the same shape, for the owner note
    /// ```

    /// Because both sides collapse to `0:<account_id>`, a resting order belonging to an account in
    /// a DIFFERENT DApp -- a different contract, with different code and different money -- compares
    /// equal to this seller's own. `inspect_seller_offer` (`:650`) then classifies it `Resting` and
    /// `prepare_seller_offer` resumes it INSTEAD of posting: the seller ends up watching, and later
    /// serving a match against, a TokenContract that is not its own.

    /// Both operands are swept, because they are separate `require`-shaped comparisons and fixing
    /// one would leave the other: a foreign-dapp TOKEN CONTRACT, and a foreign-dapp OWNER NOTE.

    /// Asserted on the verdict `validate_resting_offer` returns -- the production decision -- not on
    /// the rendering of any address.

    /// RED on this head, for both operands.
    #[test]
    #[ignore = "issues : address identity collapses to the account id, so a foreign-dapp \
                TokenContract or owner note is adopted as this seller's own. RED until the code PR \
                carries the dapp id into the comparison. UN-IGNORE there."]
    fn a_foreign_dapp_order_is_not_adopted_as_this_sellers_own() {
        let account = "9".repeat(64);
        let owner_account = "b".repeat(64);
        let foreign_dapp = "3".repeat(64);
        assert_ne!(
            foreign_dapp,
            dexdo_core::DEXDO_DAPP_ID,
            "the fixture must name a FOREIGN dapp"
        );

        let cfg = SellerConfig {
            token_contract: format!("0:{account}"),
            price_per_tick: 1_000,
            max_ticks: 8,
            subscription: false,
            gateway_advertise: "127.0.0.1:1".to_string(),
            mock_token_count: 8,
        };
        let ours_owner = format!("0:{owner_account}");

        // Operand 1 -- the order's TokenContract lives in another DApp.
        let foreign_tc = format!("{foreign_dapp}::{account}");
        let order = raw_sell(
            1,
            &ours_owner,
            Some(&foreign_tc),
            cfg.price_per_tick.into(),
            8,
        );
        let verdict = validate_resting_offer(&order, Some(&ours_owner), &cfg);
        assert!(
            verdict.is_err(),
            "a resting SELL on TokenContract {foreign_tc} -- a different DApp from this seller's \
             {} -- was adopted as this seller's own; the seller will watch and serve a contract it \
             does not own",
            cfg.token_contract
        );

        // Operand 2 -- the order's OWNER NOTE lives in another DApp.
        let foreign_owner = format!("{foreign_dapp}::{owner_account}");
        let order = raw_sell(
            2,
            &foreign_owner,
            Some(&cfg.token_contract),
            cfg.price_per_tick.into(),
            8,
        );
        let verdict = validate_resting_offer(&order, Some(&ours_owner), &cfg);
        assert!(
            verdict.is_err(),
            "a resting SELL owned by note {foreign_owner} -- a different DApp from this seller's \
             {ours_owner} -- was accepted as this seller's own order"
        );

        // The controls, and they carry the property under test. A control that is only ever
        // written in the LEGACY `0:<account>` form shares nothing with the negatives above, so the
        // pair would pass on a change that rejected every canonical `::` address -- including valid
        // same-DApp orders. That change is a plausible over-correction of exactly this defect, so
        // the accepted cases must include the canonical form of THIS seller's own DApp.
        let canonical_tc = format!("{}::{account}", dexdo_core::DEXDO_DAPP_ID);
        let canonical_owner = format!("{}::{owner_account}", dexdo_core::DEXDO_DAPP_ID);
        for (label, order, expected_owner) in [
            (
                "legacy form, this seller's own order",
                raw_sell(
                    3,
                    &ours_owner,
                    Some(&cfg.token_contract),
                    cfg.price_per_tick.into(),
                    8,
                ),
                ours_owner.clone(),
            ),
            (
                "CANONICAL form, same DApp, own TokenContract",
                raw_sell(
                    4,
                    &ours_owner,
                    Some(&canonical_tc),
                    cfg.price_per_tick.into(),
                    8,
                ),
                ours_owner.clone(),
            ),
            (
                "CANONICAL form, same DApp, own owner note",
                raw_sell(
                    5,
                    &canonical_owner,
                    Some(&cfg.token_contract),
                    cfg.price_per_tick.into(),
                    8,
                ),
                ours_owner.clone(),
            ),
            (
                "CANONICAL form, same DApp, both halves",
                raw_sell(
                    6,
                    &canonical_owner,
                    Some(&canonical_tc),
                    cfg.price_per_tick.into(),
                    8,
                ),
                canonical_owner.clone(),
            ),
        ] {
            validate_resting_offer(&order, Some(&expected_owner), &cfg).unwrap_or_else(|error| {
                panic!(
                    "{label}: a same-DApp order was refused, so the rejection above cannot be \
                     attributed to the FOREIGN dapp -- it rejects the canonical form as such: \
                     {error}"
                )
            });
        }
    }

    async fn prepare_start_gateway_and_watch(
        backend: &StartupBackend,
        cfg: &SellerConfig,
        expected_owner: &str,
        cursor_name: &str,
    ) -> (SellerOfferStartup, RunningSeller, Match) {
        let note: Arc<dyn Note> = Arc::new(LocalNote::generate());
        let startup = prepare_seller_offer(note.as_ref(), backend, cfg, Some(expected_owner))
            .await
            .expect("seller startup classification");
        let seller =
            start_gateway_with_note("127.0.0.1:0".parse().unwrap(), UpstreamConfig::Mock, note)
                .await
                .expect("gateway starts");
        assert!(!seller.server_task.is_finished(), "gateway remains running");
        let (_cursor_dir, cursor_path) = temp_cursor_path(cursor_name);
        let watch = SellerMatchWatchConfig {
            cursor_path,
            poll_interval: Duration::from_millis(1),
        };
        let matched = tokio::time::timeout(
            Duration::from_secs(2),
            watch_and_serve_match(&seller, backend, cfg, &watch),
        )
        .await
        .expect("watcher stays live")
        .expect("watcher opens the later match");
        (startup, seller, matched)
    }

    async fn assert_startup_rejected(
        backend: &StartupBackend,
        cfg: &SellerConfig,
        expected_owner: &str,
        expected_error: &str,
    ) {
        let note = LocalNote::generate();
        let error = prepare_seller_offer(&note, backend, cfg, Some(expected_owner))
            .await
            .expect_err("unsafe raw order state must fail closed");
        assert!(
            error.to_string().contains(expected_error),
            "expected `{expected_error}` in `{error}`"
        );
        // Identify the TC by its ACCOUNT ID, not by one spelling of the address. The error renders
        // the canonical `<dapp>::<account>` form now while `cfg.token_contract` holds the
        // legacy `0:<account>` one, so a whole-string match asserted the rendering rather than the
        // identification -- and broke the moment the rendering moved, which is exactly what it did.
        // The account id is common to every form the client can print, so this survives the next
        // rendering change too.
        let tc_account = cfg.token_contract.trim_start_matches("0:");
        assert!(
            error.to_string().contains(tc_account),
            "error must identify the TC account {tc_account}: {error}"
        );
        assert!(
            error.to_string().contains("cancel the existing order"),
            "error must tell the operator how to recover: {error}"
        );
        assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn exact_raw_resting_sell_resumes_gateway_and_later_match_without_repost() {
        let tc = chain_address('a');
        let owner = chain_address('b');
        let cfg = test_cfg(&tc);
        let buyer = LocalNote::generate();
        let row = raw_sell(
            70,
            &owner.to_ascii_uppercase(),
            Some(&tc.to_ascii_uppercase()),
            3_000_000_000,
            8,
        );
        let backend = StartupBackend::new(
            RawStartupRead::Orders(vec![row]),
            None,
            sample_match(&tc, buyer.pubkey()),
        );

        let (startup, seller, matched) =
            prepare_start_gateway_and_watch(&backend, &cfg, &owner, "raw-resume").await;

        assert_eq!(startup, SellerOfferStartup::ResumedResting { order_id: 70 });
        assert_eq!(matched.token_contract, tc);
        assert_eq!(backend.raw_reads.load(Ordering::Relaxed), 1);
        assert_eq!(backend.freshness_checks.load(Ordering::Relaxed), 0);
        assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.poll_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn empty_raw_book_posts_once_then_starts_gateway_and_watcher() {
        let tc = chain_address('c');
        let owner = chain_address('d');
        let cfg = test_cfg(&tc);
        let buyer = LocalNote::generate();
        let backend = StartupBackend::new(
            RawStartupRead::Orders(Vec::new()),
            None,
            sample_match(&tc, buyer.pubkey()),
        );

        let (startup, seller, _) =
            prepare_start_gateway_and_watch(&backend, &cfg, &owner, "fresh-post").await;

        assert_eq!(
            startup,
            SellerOfferStartup::Posted {
                outcome: Some(SellOfferOutcome::Rested { order_id: 835 })
            }
        );
        assert_eq!(backend.raw_reads.load(Ordering::Relaxed), 1);
        assert_eq!(backend.freshness_checks.load(Ordering::Relaxed), 1);
        assert_eq!(backend.post_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            backend.posted_offer.lock().unwrap().as_ref().unwrap().flags,
            0,
            "ordinary seller must keep flags=0"
        );
        assert_eq!(backend.poll_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn subscription_mode_posts_explicit_flag_and_preserves_full_size() {
        let owner = chain_address('1');
        let note = LocalNote::generate();

        let ordinary_cfg = test_cfg(&chain_address('2'));
        let ordinary_backend = StartupBackend::without_match(RawStartupRead::Orders(Vec::new()));
        prepare_seller_offer(&note, &ordinary_backend, &ordinary_cfg, Some(&owner))
            .await
            .expect("ordinary SELL posts");
        let ordinary = ordinary_backend
            .posted_offer
            .lock()
            .unwrap()
            .clone()
            .expect("ordinary offer captured");
        assert_eq!(ordinary.flags, 0);

        let mut subscription_cfg = test_cfg(&chain_address('3'));
        subscription_cfg.subscription = true;
        let subscription_backend =
            StartupBackend::without_match(RawStartupRead::Orders(Vec::new()));
        prepare_seller_offer(
            &note,
            &subscription_backend,
            &subscription_cfg,
            Some(&owner),
        )
        .await
        .expect("subscription SELL posts");
        let subscription = subscription_backend
            .posted_offer
            .lock()
            .unwrap()
            .clone()
            .expect("subscription offer captured");
        assert_eq!(
            subscription.flags,
            order_flags::AON | order_flags::SUBSCRIPTION
        );
        assert_eq!(subscription.flags, 0x60);
        assert_eq!(subscription.max_ticks, subscription_cfg.max_ticks);
    }

    #[tokio::test]
    async fn malformed_subscription_volume_fails_before_post_or_resume() {
        let owner = chain_address('4');
        let note = LocalNote::generate();
        let over_cap =
            u64::try_from(SUBSCRIPTION_MAX_TICKS).unwrap() + u64::from(SUBSCRIPTION_WEEKS);
        assert_eq!(over_cap, 40_324);

        for max_ticks in [0, 1023, over_cap] {
            let tc = chain_address(if max_ticks == 0 { '5' } else { '6' });
            let mut cfg = test_cfg(&tc);
            cfg.max_ticks = max_ticks;
            cfg.subscription = true;

            let post_backend = StartupBackend::without_match(RawStartupRead::Orders(Vec::new()));
            let error = post_offer_with_note(&note, &post_backend, &cfg)
                .await
                .expect_err("malformed subscription volume must not be posted");
            assert!(error.to_string().contains("subscription"), "{error}");
            assert!(
                error.to_string().contains(&max_ticks.to_string()),
                "{error}"
            );
            assert_eq!(post_backend.post_calls.load(Ordering::Relaxed), 0);

            let mut resting = raw_sell(
                80 + u128::from(max_ticks),
                &owner,
                Some(&tc),
                3_000_000_000,
                u128::from(max_ticks),
            );
            resting.flags = SUBSCRIPTION_SELL_FLAGS;
            let resume_backend =
                StartupBackend::without_match(RawStartupRead::Orders(vec![resting]));
            let error = prepare_seller_offer(&note, &resume_backend, &cfg, Some(&owner))
                .await
                .expect_err("malformed subscription volume must not resume");
            assert!(error.to_string().contains("subscription"), "{error}");
            assert!(
                error.to_string().contains(&max_ticks.to_string()),
                "{error}"
            );
            assert_eq!(resume_backend.raw_reads.load(Ordering::Relaxed), 0);
            assert_eq!(resume_backend.freshness_checks.load(Ordering::Relaxed), 0);
            assert_eq!(resume_backend.post_calls.load(Ordering::Relaxed), 0);
            assert_eq!(resume_backend.poll_calls.load(Ordering::Relaxed), 0);
            assert_eq!(resume_backend.open_calls.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn ordinary_offer_volume_behavior_is_unchanged() {
        let note = LocalNote::generate();
        for max_ticks in [0, 1023] {
            let mut cfg = test_cfg(&chain_address(if max_ticks == 0 { '7' } else { '8' }));
            cfg.max_ticks = max_ticks;
            let backend = StartupBackend::without_match(RawStartupRead::Orders(Vec::new()));

            post_offer_with_note(&note, &backend, &cfg)
                .await
                .expect("ordinary offer keeps its existing post behavior");

            assert_eq!(backend.post_calls.load(Ordering::Relaxed), 1);
            assert_eq!(
                backend
                    .posted_offer
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .max_ticks,
                max_ticks
            );
        }
    }

    #[tokio::test]
    async fn resting_resume_requires_exact_seller_flag_shape() {
        let tc = chain_address('4');
        let owner = chain_address('5');
        let mut subscription_cfg = test_cfg(&tc);
        subscription_cfg.subscription = true;

        let mut flagged = raw_sell(71, &owner, Some(&tc), 3_000_000_000, 8);
        flagged.flags = order_flags::AON | order_flags::SUBSCRIPTION;
        let matching = StartupBackend::without_match(RawStartupRead::Orders(vec![flagged]));
        let startup = prepare_seller_offer(
            &LocalNote::generate(),
            &matching,
            &subscription_cfg,
            Some(&owner),
        )
        .await
        .expect("same-mode subscription SELL resumes");
        assert_eq!(startup, SellerOfferStartup::ResumedResting { order_id: 71 });
        assert_eq!(matching.post_calls.load(Ordering::Relaxed), 0);

        for (order_id, invalid_flags) in [
            (72, 0),
            (73, order_flags::SUBSCRIPTION),
            (74, order_flags::AON),
            (
                75,
                order_flags::TEE | order_flags::AON | order_flags::SUBSCRIPTION,
            ),
            (76, 0x80),
        ] {
            let mut invalid = raw_sell(order_id, &owner, Some(&tc), 3_000_000_000, 8);
            invalid.flags = invalid_flags;
            let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![invalid]));
            assert_startup_rejected(
                &backend,
                &subscription_cfg,
                &owner,
                &format!("flags 0x{invalid_flags:02x} do not match required seller flags 0x60"),
            )
            .await;
        }

        let ordinary_cfg = test_cfg(&tc);
        for (order_id, invalid_flags) in [
            (77, order_flags::SUBSCRIPTION),
            (78, order_flags::AON),
            (79, order_flags::AON | order_flags::SUBSCRIPTION),
            (80, 0x80),
        ] {
            let mut invalid = raw_sell(order_id, &owner, Some(&tc), 3_000_000_000, 8);
            invalid.flags = invalid_flags;
            let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![invalid]));
            assert_startup_rejected(
                &backend,
                &ordinary_cfg,
                &owner,
                &format!("flags 0x{invalid_flags:02x} do not match required seller flags 0x00"),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn funded_openable_match_keeps_existing_resume_path() {
        let tc = chain_address('e');
        let owner = chain_address('f');
        let cfg = test_cfg(&tc);
        let buyer = LocalNote::generate();
        let matched = sample_match(&tc, buyer.pubkey());
        let backend =
            StartupBackend::new(RawStartupRead::ChainFailure, Some(matched.clone()), matched)
                .with_resume_facts(resume_state(3_000_000_000), 3_000_000_000);

        let (startup, seller, _) =
            prepare_start_gateway_and_watch(&backend, &cfg, &owner, "funded-resume").await;

        assert_eq!(startup, SellerOfferStartup::ResumedFunded);
        assert_eq!(backend.raw_reads.load(Ordering::Relaxed), 0);
        assert_eq!(backend.freshness_checks.load(Ordering::Relaxed), 0);
        assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn terminal_zero_deposit_match_fails_startup_before_post_or_open() {
        for price_per_tick in [1, 2] {
            let tc = chain_address(char::from_digit(price_per_tick, 10).unwrap());
            let owner = chain_address('a');
            let mut cfg = test_cfg(&tc);
            cfg.price_per_tick = price_per_tick.into();
            let buyer = LocalNote::generate();
            let mut matched = sample_match(&tc, buyer.pubkey());
            matched.price_per_tick = price_per_tick.into();
            let backend =
                StartupBackend::new(RawStartupRead::ChainFailure, Some(matched.clone()), matched)
                    .with_resume_facts(resume_state(0), price_per_tick.into());
            let note = LocalNote::generate();

            let error = prepare_seller_offer(&note, &backend, &cfg, Some(&owner))
                .await
                .expect_err("terminal zero-deposit TC must fail startup");
            let error = error.to_string();

            // Account id, not one spelling of the address -- the message is canonical now.
            assert!(error.contains(tc.trim_start_matches("0:")), "{error}");
            assert!(error.contains("deposit=0"), "{error}");
            assert!(
                error.contains(&format!("price_per_tick={price_per_tick}")),
                "{error}"
            );
            assert!(error.contains("cannot be opened"), "{error}");
            assert!(error.contains("fresh --nonce"), "{error}");
            assert!(error.contains("close/destroy"), "{error}");
            assert_eq!(backend.raw_reads.load(Ordering::Relaxed), 0);
            assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
            assert_eq!(backend.open_calls.load(Ordering::Relaxed), 0);
            assert_eq!(backend.poll_calls.load(Ordering::Relaxed), 0);
        }

        let tc = chain_address('3');
        let owner = chain_address('b');
        let mut cfg = test_cfg(&tc);
        cfg.price_per_tick = 1;
        let buyer = LocalNote::generate();
        let mut matched = sample_match(&tc, buyer.pubkey());
        matched.price_per_tick = 2;
        let backend =
            StartupBackend::new(RawStartupRead::ChainFailure, Some(matched.clone()), matched)
                .with_resume_facts(resume_state(1), 2);
        let note = LocalNote::generate();

        let error = prepare_seller_offer(&note, &backend, &cfg, Some(&owner))
            .await
            .expect_err("authoritative price must override the lower local config price")
            .to_string();

        assert!(error.contains("deposit=1"), "{error}");
        assert!(error.contains("price_per_tick=2"), "{error}");
        assert_eq!(backend.raw_reads.load(Ordering::Relaxed), 0);
        assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.poll_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn seller_resume_state_and_terms_read_failures_make_no_writes() {
        for failure in [
            "TokenContract getState read failed",
            "TokenContract getDeal unavailable after match",
        ] {
            let tc = chain_address('b');
            let owner = chain_address('c');
            let cfg = test_cfg(&tc);
            let backend = StartupBackend::without_match(RawStartupRead::ChainFailure)
                .with_startup_failure(failure);
            let note = LocalNote::generate();

            let error = prepare_seller_offer(&note, &backend, &cfg, Some(&owner))
                .await
                .expect_err("resume preflight read failure must fail closed");

            assert!(error.to_string().contains(failure), "{error}");
            assert_eq!(backend.raw_reads.load(Ordering::Relaxed), 0);
            assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
            assert_eq!(backend.open_calls.load(Ordering::Relaxed), 0);
            assert_eq!(backend.poll_calls.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn resting_sell_for_wrong_tc_fails_before_post_or_open() {
        let tc = chain_address('1');
        let owner = chain_address('2');
        let cfg = test_cfg(&tc);
        let row = raw_sell(1, &owner, Some(&chain_address('3')), 3_000_000_000, 8);
        let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![row]));
        assert_startup_rejected(&backend, &cfg, &owner, "not this TC").await;
    }

    #[tokio::test]
    async fn resting_sell_for_wrong_owner_fails_before_post_or_open() {
        let tc = chain_address('4');
        let owner = chain_address('5');
        let cfg = test_cfg(&tc);
        let row = raw_sell(2, &chain_address('6'), Some(&tc), 3_000_000_000, 8);
        let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![row]));
        assert_startup_rejected(&backend, &cfg, &owner, "does not match seller note").await;
    }

    #[tokio::test]
    async fn resting_sell_for_wrong_price_fails_before_post_or_open() {
        let tc = chain_address('7');
        let owner = chain_address('8');
        let cfg = test_cfg(&tc);
        let row = raw_sell(3, &owner, Some(&tc), 2_000_000_000, 8);
        let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![row]));
        assert_startup_rejected(&backend, &cfg, &owner, "price_per_tick 2 does not match 3").await;
    }

    #[tokio::test]
    async fn resting_sell_for_wrong_ticks_fails_before_post_or_open() {
        let tc = chain_address('9');
        let owner = chain_address('a');
        let cfg = test_cfg(&tc);
        let row = raw_sell(4, &owner, Some(&tc), 3_000_000_000, 7);
        let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![row]));
        assert_startup_rejected(&backend, &cfg, &owner, "remaining ticks 7").await;
    }

    #[tokio::test]
    async fn resting_sell_missing_required_fact_fails_before_post_or_open() {
        let tc = chain_address('b');
        let owner = chain_address('c');
        let cfg = test_cfg(&tc);
        let row = raw_sell(5, &owner, None, 3_000_000_000, 8);
        let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![row]));
        assert_startup_rejected(
            &backend,
            &cfg,
            &owner,
            "missing the required token_contract",
        )
        .await;
    }

    #[tokio::test]
    async fn two_equivalent_raw_sell_rows_are_ambiguous_before_post_or_open() {
        let tc = chain_address('d');
        let owner = chain_address('e');
        let cfg = test_cfg(&tc);
        let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![
            raw_sell(6, &owner, Some(&tc), 3_000_000_000, 8),
            raw_sell(7, &owner, Some(&tc), 3_000_000_000, 8),
        ]));
        assert_startup_rejected(&backend, &cfg, &owner, "2 active SELL rows").await;
    }

    #[tokio::test]
    async fn raw_book_chain_error_never_falls_back_to_repost() {
        let tc = chain_address('f');
        let owner = chain_address('1');
        let cfg = test_cfg(&tc);
        let backend = StartupBackend::without_match(RawStartupRead::ChainFailure);
        assert_startup_rejected(&backend, &cfg, &owner, "raw order-book read failed").await;
    }

    #[tokio::test]
    async fn raw_book_timeout_never_falls_back_to_repost() {
        let tc = chain_address('2');
        let owner = chain_address('3');
        let cfg = test_cfg(&tc);
        let backend = StartupBackend::without_match(RawStartupRead::TransportFailure);
        assert_startup_rejected(&backend, &cfg, &owner, "raw book timeout").await;
    }

    #[tokio::test]
    async fn resting_resume_keeps_gateway_alive_across_match_watch_transport_retry() {
        let tc = chain_address('4');
        let owner = chain_address('5');
        let cfg = test_cfg(&tc);
        let buyer = LocalNote::generate();
        let backend = StartupBackend::new(
            RawStartupRead::Orders(vec![raw_sell(8, &owner, Some(&tc), 3_000_000_000, 8)]),
            None,
            sample_match(&tc, buyer.pubkey()),
        )
        .with_poll_failures(1);

        let (startup, seller, _) =
            prepare_start_gateway_and_watch(&backend, &cfg, &owner, "resume-retry").await;

        assert_eq!(startup, SellerOfferStartup::ResumedResting { order_id: 8 });
        assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.poll_calls.load(Ordering::Relaxed), 2);
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
        assert!(!seller.server_task.is_finished());
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn poll_match_cursor_persists_and_resume_uses_it() {
        let (_cursor_dir, cursor_path) = temp_cursor_path("resume");
        let seller = test_seller();
        let cfg = test_cfg("tc-watch");
        let buyer = LocalNote::generate();
        let first_seen = now_unix().unwrap() as i64 + 1;

        let first = PollBackend::new(
            Some(sample_match("tc-other-owner-fill", buyer.pubkey())),
            None,
            first_seen,
        );
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
        assert_eq!(
            saved.fill,
            Some(SellerFillLineage {
                order_id: 1,
                offered_ticks: 8,
                matched_ticks: 8,
                residual_ticks: 0,
                price_per_tick: 3_000_000_000,
                replacement_nonce: None,
                replacement_token_contract: None,
            })
        );
    }

    #[tokio::test]
    async fn legacy_opened_cursor_recovers_fill_before_auth_resume() {
        let (_cursor_dir, cursor_path) = temp_cursor_path("legacy-opened-fill-recovery");
        let seller = test_seller();
        let tc = "tc-legacy-opened";
        let cfg = test_cfg(tc);
        let buyer = LocalNote::generate();
        let backend = PollBackend::with_state(sample_match(tc, buyer.pubkey()), true, true);
        std::fs::write(
            &cursor_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "token_contract": tc,
                "source": {
                    "since_unix": 0,
                    "last_seen_created_at": 1,
                    "seen_token_contracts_at_last_seen": [tc],
                },
                "last_polled_unix": 1,
                "opened_at_unix": 1,
            }))
            .unwrap(),
        )
        .unwrap();

        let matched = poll_match_and_maybe_open(&seller, &backend, &cfg, &cursor_path)
            .await
            .unwrap()
            .expect("legacy opened deal must resume from authoritative owner history");

        assert_eq!(matched.token_contract, tc);
        assert_eq!(
            backend.opens.load(Ordering::Relaxed),
            0,
            "an already-opened legacy deal must not repeat open_stream"
        );
        let saved: SellerMatchWatchCursor =
            serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
        assert_eq!(
            saved.fill,
            Some(SellerFillLineage {
                order_id: 1,
                offered_ticks: 8,
                matched_ticks: 8,
                residual_ticks: 0,
                price_per_tick: 3_000_000_000,
                replacement_nonce: None,
                replacement_token_contract: None,
            })
        );
        let nonce = b"legacy-resume";
        seller.state.auth.issue_challenge(tc, nonce.to_vec());
        let signature = buyer.sign(&crate::seller::auth::challenge_bytes(tc, nonce));
        assert!(seller.state.auth.verify_response(tc, nonce, &signature));
    }

    #[tokio::test]
    async fn partial_fill_lineage_is_saved_before_open_stream() {
        let (_cursor_dir, cursor_path) = temp_cursor_path("partial-before-open");
        let seller = test_seller();
        let cfg = test_cfg("tc-partial-before-open");
        let buyer = LocalNote::generate();
        let backend = PollBackend::new(
            Some(sample_match("tc-partial-before-open", buyer.pubkey())),
            None,
            now_unix().unwrap() as i64 + 1,
        )
        .with_matched_ticks(3)
        .with_open_failures(1);

        let error = poll_match_and_maybe_open(&seller, &backend, &cfg, &cursor_path)
            .await
            .expect_err("the signed open write fails after the fill is persisted");

        assert!(error.to_string().contains("timeout after signed writes"));
        let saved: SellerMatchWatchCursor =
            serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
        assert_eq!(
            saved.fill,
            Some(SellerFillLineage {
                order_id: 1,
                offered_ticks: 8,
                matched_ticks: 3,
                residual_ticks: 5,
                price_per_tick: 3_000_000_000,
                replacement_nonce: None,
                replacement_token_contract: None,
            })
        );
        assert!(
            saved.opened_at_unix.is_none(),
            "the cursor must not claim that a failed open completed"
        );
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn authoritative_tc_size_mismatch_fails_before_persist_or_open() {
        let (_cursor_dir, cursor_path) = temp_cursor_path("authoritative-size-mismatch");
        let seller = test_seller();
        let cfg = test_cfg("tc-authoritative-size-mismatch");
        let buyer = LocalNote::generate();
        let backend = PollBackend::new(
            Some(sample_match(
                "tc-authoritative-size-mismatch",
                buyer.pubkey(),
            )),
            None,
            now_unix().unwrap() as i64 + 1,
        )
        .with_matched_ticks(3)
        .with_offer_ticks(9);

        let error = poll_match_and_maybe_open(&seller, &backend, &cfg, &cursor_path)
            .await
            .expect_err("manifest/config N must be cross-checked against getDeal")
            .to_string();

        assert!(error.contains("TokenContract.getDeal"), "{error}");
        assert!(!cursor_path.exists());
        assert_eq!(backend.opens.load(Ordering::Relaxed), 0);
    }

    proptest! {
        #[test]
        fn seller_fill_lineage_conserves_capacity_and_rejects_invalid_replay(
            (offered, matched) in (u64::try_from(MIN_STREAM_BUY_TICKS)
                .expect("MIN_STREAM_BUY_TICKS must fit u64")..10_000)
                .prop_flat_map(|offered| (Just(offered), 1u64..=offered))
        ) {
            let tc = "tc-fill-lineage-property";
            let mut cfg = test_cfg(tc);
            cfg.max_ticks = offered;
            let fill = MatchedFill {
                order_id: 41,
                token_contract: tc.to_string(),
                ticks: u128::from(matched),
                price_per_tick: u128::from(cfg.price_per_tick),
            };
            let mut cursor = SellerMatchWatchCursor::new(&cfg.token_contract).unwrap();

            prop_assert!(cursor
                .record_fill(&cfg, cfg.price_per_tick, offered, &fill)
                .unwrap());
            let lineage = cursor.fill.as_ref().unwrap();
            prop_assert_eq!(
                lineage.matched_ticks + lineage.residual_ticks,
                lineage.offered_ticks
            );
            prop_assert!(!cursor
                .record_fill(&cfg, cfg.price_per_tick, offered, &fill)
                .unwrap());

            let mut conflicting = fill.clone();
            conflicting.order_id += 1;
            prop_assert!(cursor
                .record_fill(&cfg, cfg.price_per_tick, offered, &conflicting)
                .is_err());

            for invalid in [0, offered + 1] {
                let mut invalid_cursor =
                    SellerMatchWatchCursor::new(&cfg.token_contract).unwrap();
                let mut invalid_fill = fill.clone();
                invalid_fill.ticks = u128::from(invalid);
                prop_assert!(invalid_cursor
                    .record_fill(&cfg, cfg.price_per_tick, offered, &invalid_fill)
                    .is_err());
                prop_assert!(invalid_cursor.fill.is_none());
            }
        }
    }

    #[tokio::test]
    async fn transient_match_watch_network_error_keeps_seller_alive() {
        let (_cursor_dir, cursor_path) = temp_cursor_path("transient-network");
        let seller = test_seller();
        let cfg = test_cfg("tc-transient-network");
        let buyer = LocalNote::generate();
        let backend = PollBackend::new(
            Some(sample_match("tc-transient-network", buyer.pubkey())),
            None,
            now_unix().unwrap() as i64 + 1,
        )
        .with_poll_failures(1);
        let watch = SellerMatchWatchConfig {
            cursor_path,
            poll_interval: Duration::from_millis(1),
        };

        let matched = tokio::time::timeout(
            Duration::from_secs(1),
            watch_and_serve_match(&seller, &backend, &cfg, &watch),
        )
        .await
        .expect("seller watcher stays alive after the transient error")
        .expect("seller watcher recovers without exiting");

        assert_eq!(matched.token_contract, "tc-transient-network");
        assert_eq!(backend.polls.load(Ordering::Relaxed), 2);
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
        assert!(!seller.server_task.is_finished(), "gateway remains alive");
    }

    #[tokio::test]
    async fn issue_1185_classified_transient_match_watch_error_retries_the_poll() {
        let (_cursor_dir, cursor_path) = temp_cursor_path("classified-transient-network");
        let seller = test_seller();
        let cfg = test_cfg("tc-classified-transient-network");
        let buyer = LocalNote::generate();
        let backend = PollBackend::new(
            Some(sample_match(
                "tc-classified-transient-network",
                buyer.pubkey(),
            )),
            None,
            now_unix().unwrap() as i64 + 1,
        )
        .with_poll_failures(1);
        let watch = SellerMatchWatchConfig {
            cursor_path,
            poll_interval: Duration::from_millis(1),
        };

        let matched = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_match(&seller, &backend, &cfg, &watch),
        )
        .await
        .expect("transient classification must keep the match watcher alive")
        .expect("the next poll returns the match");

        assert_eq!(matched.token_contract, "tc-classified-transient-network");
        assert_eq!(backend.polls.load(Ordering::Relaxed), 2);
        assert_eq!(backend.opens.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn post_match_transport_error_surfaces_without_reprovisioning() {
        let (_cursor_dir, cursor_path) = temp_cursor_path("post-match-transport");
        let seller = test_seller();
        let cfg = test_cfg("tc-post-match-transport");
        let buyer = LocalNote::generate();
        let backend = PollBackend::new(
            Some(sample_match("tc-post-match-transport", buyer.pubkey())),
            None,
            now_unix().unwrap() as i64 + 1,
        )
        .with_open_failures(1);
        let watch = SellerMatchWatchConfig {
            cursor_path,
            poll_interval: Duration::from_millis(1),
        };

        let error = watch_and_serve_match(&seller, &backend, &cfg, &watch)
            .await
            .expect_err("post-match transport error must surface");

        assert!(
            error
                .downcast_ref::<ChainError>()
                .is_some_and(|error| matches!(error, ChainError::Transport(_))),
            "post-match transport error must remain observable: {error}"
        );
        assert_eq!(
            backend.polls.load(Ordering::Relaxed),
            1,
            "a post-match failure must not re-enter the read-only match poll"
        );
        assert_eq!(
            backend.opens.load(Ordering::Relaxed),
            1,
            "a post-match failure must not retry signed provisioning writes"
        );
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
    async fn subscription_provision_registers_coherent_authoritative_capacity_before_open() {
        let seller = test_seller();
        let mut cfg = test_cfg("tc-subscription-capacity");
        cfg.subscription = true;
        let buyer = LocalNote::generate();
        let backend = PollBackend::with_state(
            sample_match("tc-subscription-capacity", buyer.pubkey()),
            false,
            false,
        )
        .with_subscription();

        provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-subscription-capacity", buyer.pubkey()),
        )
        .await
        .unwrap();

        let capacity = seller
            .state
            .capacity_snapshot("tc-subscription-capacity")
            .unwrap()
            .unwrap();
        assert_eq!(capacity.funded_tokens, 8 * dexdo_core::TICK_SIZE);
        assert_eq!(capacity.authoritative_cap, dexdo_core::TICK_SIZE);
        assert_eq!(
            backend.state_reads.load(Ordering::Relaxed),
            2,
            "one shared snapshot read is followed by the existing opened-state decision read"
        );
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ordinary_provision_uses_coherent_actual_funded_capacity_before_open() {
        let seller = test_seller();
        let cfg = test_cfg("tc-ordinary-capacity");
        let buyer = LocalNote::generate();
        let backend = PollBackend::with_state(
            sample_match("tc-ordinary-capacity", buyer.pubkey()),
            false,
            false,
        );

        provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-ordinary-capacity", buyer.pubkey()),
        )
        .await
        .unwrap();

        let capacity = seller
            .state
            .capacity_snapshot("tc-ordinary-capacity")
            .unwrap()
            .unwrap();
        assert_eq!(capacity.funded_tokens, 8 * dexdo_core::TICK_SIZE);
        assert_eq!(
            capacity.authoritative_cap,
            dexdo_core::TICK_SIZE,
            "pre-accept delivery is only the single probe allowance"
        );
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn subscription_provision_rejects_ordinary_shape_before_auth_or_open() {
        let seller = test_seller();
        let mut cfg = test_cfg("tc-subscription-missing");
        cfg.subscription = true;
        let buyer = LocalNote::generate();
        let backend = PollBackend::with_state(
            sample_match("tc-subscription-missing", buyer.pubkey()),
            false,
            false,
        );

        let error = provision_match(
            &seller,
            &backend,
            &cfg,
            sample_match("tc-subscription-missing", buyer.pubkey()),
        )
        .await
        .expect_err("ordinary strict shape must not satisfy subscription seller mode");

        assert!(
            error.to_string().contains(
                "seller configured subscription mode but strict deal snapshot is ordinary"
            ),
            "{error:#}"
        );
        assert_eq!(backend.opens.load(Ordering::Relaxed), 0);
        assert!(seller
            .state
            .capacity_snapshot("tc-subscription-missing")
            .unwrap()
            .is_none());
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

        assert_eq!(backend.state_reads.load(Ordering::Relaxed), 3);
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
        assert_eq!(backend.state_reads.load(Ordering::Relaxed), 4);
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

    #[tokio::test]
    async fn shared_gateway_isolates_two_authenticated_tc_routes_and_cleanup() {
        let seller = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let buyer_a = LocalNote::generate();
        let buyer_b = LocalNote::generate();
        let tc_a = "tc-route-a";
        let tc_b = "tc-route-b";
        seller.state.route_stream(
            tc_a,
            UpstreamConfig::MockWithClaimedModel("model-a".to_string()),
        );
        seller.state.route_stream(
            tc_b,
            UpstreamConfig::MockWithClaimedModel("model-b".to_string()),
        );
        let state = DealChainState {
            funded: true,
            opened: false,
            probe_accepted: false,
            disputed: false,
            deposit: 1,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 1,
            funded_time: Some(1),
            probe_time: 0,
            last_claim_time: 0,
            dispute_time: 0,
        };
        let deal = DealSubscription {
            deal_flags: 0,
            sub_weeks: 0,
            week_index: 0,
            tokens_per_week: dexdo_core::TICK_SIZE,
            funded_tokens: dexdo_core::TICK_SIZE,
            tokens_paid: 0,
            period_start: 0,
            week_base_tokens: 0,
        };
        seller
            .state
            .register_stream(tc_a, buyer_a.pubkey(), 2, state, deal)
            .unwrap();
        seller
            .state
            .register_stream(tc_b, buyer_b.pubkey(), 2, state, deal)
            .unwrap();

        let endpoint = format!("https://{}", seller.listen_addr);
        let channel = crate::buyer::tls::connect_pinned(&endpoint, &seller.tls_fingerprint)
            .await
            .unwrap();
        let mut client = GatewayClient::new(channel);
        let request_a = gateway_request(&mut client, &buyer_a, tc_a).await;
        let mut stream_a = client.open_stream(request_a).await.unwrap().into_inner();
        let chunk_a = stream_a.message().await.unwrap().unwrap();
        assert_eq!(chunk_a.manifest.unwrap().claimed_model, "model-a");
        drop(stream_a);
        seller.state.unregister_stream(tc_a);

        let cleaned_a = gateway_request(&mut client, &buyer_a, tc_a).await;
        let rejected = client
            .open_stream(cleaned_a)
            .await
            .expect_err("cleaned deal A must no longer authorize");
        assert_eq!(rejected.code(), tonic::Code::Unauthenticated);

        let request_b = gateway_request(&mut client, &buyer_b, tc_b).await;
        let mut stream_b = client.open_stream(request_b).await.unwrap().into_inner();
        let chunk_b = stream_b.message().await.unwrap().unwrap();
        assert_eq!(chunk_b.manifest.unwrap().claimed_model, "model-b");
        assert!(!seller.server_task.is_finished());
        seller.server_task.abort();
    }
}
