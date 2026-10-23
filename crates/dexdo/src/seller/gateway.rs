//! Seller gateway: accepting buyer connections, stream-session
//! authorization and incremental yielding of the canonical fake-token stream.

use crate::seller::auth::{challenge_bytes, AuthRegistry, HEALTH_CHALLENGE_TC};
use crate::seller::capacity::{
    lock_or_recover, CapacityManager, CapacityReservation, CapacitySnapshot, ReserveError,
    POISONED_LOCK_MESSAGE,
};
use crate::seller::upstream::is_seller_config_http_status;
use crate::seller::upstream::UpstreamConfig;
use crate::seller::upstream::UpstreamEvent;
use anyhow::{anyhow, Result as AnyResult};
use dexdo_core::note::Signature;
use dexdo_core::params::{
    GATEWAY_CLIENT_CHANNEL_CAPACITY, GATEWAY_REQUEST_MAX_BYTES, GATEWAY_UPSTREAM_CHANNEL_CAPACITY,
};
use dexdo_core::{DealChainState, DealSubscription};
use dexdo_proto::{
    BillingUsage, CanonChunk, CanonRequest, Challenge, ChallengeRequest, Gateway, GatewayServer,
    StreamRequest,
};
use rand::RngCore;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::{Request, Response, Status};

const DELIVERY_UPDATE_LOCK: &str = "seller gateway delivery update";
const GATEWAY_LIMITS_LOCK: &str = "seller gateway limits";
const GATEWAY_DELIVERED_LOCK: &str = "seller gateway delivered";
const GATEWAY_UPSTREAMS_LOCK: &str = "seller gateway upstreams";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChainUnavailableAction {
    #[default]
    Stop,
    KeepServing,
}

impl ChainUnavailableAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::KeepServing => "keep_serving",
        }
    }

    pub fn from_config(value: &str) -> Option<Self> {
        match value {
            "stop" => Some(Self::Stop),
            "keep_serving" => Some(Self::KeepServing),
            _ => None,
        }
    }

    pub fn supported_values() -> &'static [&'static str] {
        &["stop", "keep_serving"]
    }
}

#[derive(Clone, Debug)]
pub struct UpstreamFailure {
    pub token_contract: String,
    pub error_class: &'static str,
    pub retryable: bool,
    pub grpc_status: String,
    pub http_status: Option<u16>,
    event_sequence: Arc<AtomicU64>,
}

impl UpstreamFailure {
    fn from_status(token_contract: &str, status: &Status, event_sequence: Arc<AtomicU64>) -> Self {
        let http_status = status
            .message()
            .strip_prefix("upstream HTTP ")
            .and_then(|suffix| suffix.split_whitespace().next())
            .and_then(|code| code.parse::<u16>().ok());
        let message = status.message().to_ascii_lowercase();
        let error_class = if matches!(http_status, Some(401 | 403))
            || matches!(
                status.code(),
                tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
            ) {
            "auth"
        } else if status.code() == tonic::Code::DeadlineExceeded
            || message.contains("timeout")
            || message.contains("timed out")
        {
            "timeout"
        } else if message.starts_with("upstream connect failed") {
            "connect"
        } else if http_status.is_some_and(is_seller_config_http_status) {
            // the seller builds the whole upstream request, so a `4xx` request rejection is the
            // seller's own configuration fault -- a distinct, never-retryable class, not a generic `http`.
            "seller_config"
        } else if http_status.is_some() {
            "http"
        } else {
            "upstream"
        };
        let retryable = error_class != "auth"
            && (dexdo_proto::is_retryable_upstream(status)
                || http_status.map_or_else(
                    || {
                        matches!(
                            status.code(),
                            tonic::Code::DeadlineExceeded
                                | tonic::Code::ResourceExhausted
                                | tonic::Code::Aborted
                                | tonic::Code::Unavailable
                        )
                    },
                    |code| code == 408 || code == 429 || code >= 500,
                ));
        Self {
            token_contract: token_contract.to_string(),
            error_class,
            retryable,
            grpc_status: format!("{:?}", status.code()),
            http_status,
            event_sequence,
        }
    }

    pub fn next_event_seq(&self) -> u64 {
        self.event_sequence.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Per-deal payment tracking. `count` is the cumulative accepted payment in
/// provider-token units across all of this deal's gRPC streams. Each clean request terminal adds its
/// charged portion exactly once, excluding excess work borne by the seller. `done` means **no more billable usage will ever arrive for this deal/session**. The ordinary owner
/// is the buyer **session lifecycle**; a provider bill beyond one request's paid reservation also closes
/// that lifecycle after committing the capped amount and draining admitted work. `billing_exhausted`
/// distinguishes only that latter fact,
/// so an ordinary clean session end never causes a seller-initiated close. The seller's `drive_advance` reads
/// `count` and `done` (Acquire) so finalized ticks never exceed accepted billable usage.
#[derive(Clone, Default)]
pub struct DealDelivery {
    pub count: Arc<AtomicU64>,
    pub done: Arc<AtomicBool>,
    pub billing_exhausted: Arc<AtomicBool>,
    update_lock: Arc<Mutex<()>>,
    event_sequence: Arc<AtomicU64>,
    terminal_trail_emitted: Arc<AtomicBool>,
}

impl DealDelivery {
    pub fn next_event_seq(&self) -> u64 {
        self.event_sequence.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn claim_terminal_trail(&self) -> bool {
        self.terminal_trail_emitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Terminal classification for one request's authoritative delivery accounting.

/// Capacity consumers use this only to reconcile an already-created request reservation. Provider-specific
/// counting remains entirely inside [`crate::seller::upstream`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthoritativeDeliveryFinish {
    /// The request ended before forwarding output and before accepting terminal usage.
    Interrupted,
    /// Some non-empty output was forwarded without a valid clean terminal usage record.
    AmbiguousUsage,
}

fn unfinished_delivery_finish(saw_output: bool) -> AuthoritativeDeliveryFinish {
    if saw_output {
        AuthoritativeDeliveryFinish::AmbiguousUsage
    } else {
        AuthoritativeDeliveryFinish::Interrupted
    }
}

/// Request-scoped accounting event emitted by the gateway relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthoritativeDeliveryEvent {
    /// One validated terminal request total, committed atomically with reservation completion.
    Completed(NonZeroU64),
    /// Emitted exactly once when the relay fails before a valid clean completion.
    Finished(AuthoritativeDeliveryFinish),
}

/// Narrow seam for consumers that must durably reconcile request reservations with authoritative delivery.

/// Implementations must record each event before returning. A recorder failure terminates the stream
/// fail-closed; the gateway never asks the recorder to derive tokens from text, bytes, words or frames.
#[allow(clippy::result_large_err)]
pub trait AuthoritativeDeliveryRecorder: Send + Sync {
    fn record_authoritative_delivery(
        &self,
        event: AuthoritativeDeliveryEvent,
    ) -> Result<(), Status>;

    /// Whether admission and failure reconciliation use the shared request budget.
    fn shared_budget(&self) -> bool {
        false
    }

    /// Commit the accepted amount together with any provider overrun beyond its grant.
    fn complete_billing(&self, tokens: NonZeroU64, billing_exhausted: bool) -> Result<u64, Status> {
        self.record_authoritative_delivery(AuthoritativeDeliveryEvent::Completed(tokens))?;
        if billing_exhausted {
            self.mark_billing_exhausted();
        }
        Ok(tokens.get())
    }

    /// Mark that the provider's terminal bill exceeded this request's authorized reservation.
    /// The accepted amount is recorded first; `done` then tells the existing advance driver that no
    /// later request may add billable usage to this deal.
    fn mark_billing_exhausted(&self) {}
}

impl AuthoritativeDeliveryRecorder for DealDelivery {
    fn record_authoritative_delivery(
        &self,
        event: AuthoritativeDeliveryEvent,
    ) -> Result<(), Status> {
        match event {
            AuthoritativeDeliveryEvent::Completed(tokens) => {
                let _guard = self.update_lock.lock().map_err(|_| {
                    capacity_status(anyhow!("{POISONED_LOCK_MESSAGE}: {DELIVERY_UPDATE_LOCK}"))
                })?;
                let next = checked_authoritative_tokens(&self.count, tokens.get())?;
                self.count.store(next, Ordering::Release);
                Ok(())
            }
            AuthoritativeDeliveryEvent::Finished(_) => Ok(()),
        }
    }

    fn mark_billing_exhausted(&self) {
        self.billing_exhausted.store(true, Ordering::Release);
        self.done.store(true, Ordering::Release);
    }
}

struct CapacityDeliveryRecorder {
    reservation: CapacityReservation,
    delivery: DealDelivery,
}

impl AuthoritativeDeliveryRecorder for CapacityDeliveryRecorder {
    fn shared_budget(&self) -> bool {
        self.reservation.is_shared()
    }

    fn complete_billing(&self, tokens: NonZeroU64, billing_exhausted: bool) -> Result<u64, Status> {
        let _guard = self.delivery.update_lock.lock().map_err(|_| {
            capacity_status(anyhow!("{POISONED_LOCK_MESSAGE}: {DELIVERY_UPDATE_LOCK}"))
        })?;
        // Admission and completion share this lock: close admission before publishing done.
        // It also serializes the durable allocation and its claim counter.
        // Check the wider sum before persistence, including the zero-charge case.
        let before = self.delivery.count.load(Ordering::Acquire);
        checked_authoritative_tokens(&self.delivery.count, tokens.get())?;
        let charged = self
            .reservation
            .finish_with_delivery(tokens.get())
            .map_err(capacity_status)?;
        self.delivery
            .count
            .store(before + charged, Ordering::Release);
        if billing_exhausted || charged < tokens.get() {
            self.delivery
                .billing_exhausted
                .store(true, Ordering::Release);
        }
        if self.delivery.billing_exhausted.load(Ordering::Acquire)
            && self.reservation.active_shared_requests() == 0
        {
            self.delivery.done.store(true, Ordering::Release);
        }
        Ok(charged)
    }

    fn record_authoritative_delivery(
        &self,
        event: AuthoritativeDeliveryEvent,
    ) -> Result<(), Status> {
        match event {
            AuthoritativeDeliveryEvent::Completed(tokens) => {
                self.complete_billing(tokens, false).map(|_| ())
            }
            AuthoritativeDeliveryEvent::Finished(finish) => {
                let _guard = self.delivery.update_lock.lock().map_err(|_| {
                    capacity_status(anyhow!("{POISONED_LOCK_MESSAGE}: {DELIVERY_UPDATE_LOCK}"))
                })?;
                let durable = match finish {
                    AuthoritativeDeliveryFinish::Interrupted => self.reservation.finish_exact(),
                    AuthoritativeDeliveryFinish::AmbiguousUsage => {
                        self.reservation.finish_ambiguous()
                    }
                }
                .map_err(capacity_status)?;
                if durable > 0 {
                    let next = checked_authoritative_tokens(&self.delivery.count, durable)?;
                    self.delivery.count.store(next, Ordering::Release);
                }
                if self.delivery.billing_exhausted.load(Ordering::Acquire)
                    && self.reservation.active_shared_requests() == 0
                {
                    self.delivery.done.store(true, Ordering::Release);
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StreamLimits {
    mock_token_count: u64,
    deal: DealSubscription,
}

/// Gateway state, shared across gRPC calls.
pub struct GatewayState {
    pub auth: AuthRegistry,
    /// Per-deal limits. An empty/zero mock entry = seller no-show in mock mode.
    limits: Mutex<HashMap<String, StreamLimits>>,
    /// Per-deal accepted billable-usage tracking, created on first access.
    delivered: Mutex<HashMap<String, DealDelivery>>,
    /// Durable per-TC capacity derived only from strict on-chain deal state.
    capacity: CapacityManager,
    /// Upstream choice (mock model vs the real adapter). Immutable for the gateway's lifetime.
    upstream: UpstreamConfig,
    upstreams: Mutex<HashMap<String, UpstreamConfig>>,
    upstream_failure_tx: mpsc::UnboundedSender<UpstreamFailure>,
    upstream_failure_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<UpstreamFailure>>,
    chain_unavailable: watch::Sender<bool>,
    keep_serving_when_chain_unavailable: AtomicBool,
    max_concurrent_requests_per_deal: AtomicU32,
}

impl GatewayState {
    /// Gateway with the mock model.
    pub fn new() -> Self {
        Self::with_upstream(UpstreamConfig::Mock)
    }

    /// Gateway with the chosen upstream.
    pub fn with_upstream(upstream: UpstreamConfig) -> Self {
        Self::with_capacity(upstream, CapacityManager::in_memory())
    }

    pub fn with_upstream_and_deals_dir(upstream: UpstreamConfig, deals_dir: PathBuf) -> Self {
        Self::with_capacity(upstream, CapacityManager::in_deals_dir(deals_dir))
    }

    fn with_capacity(upstream: UpstreamConfig, capacity: CapacityManager) -> Self {
        let (upstream_failure_tx, upstream_failure_rx) = mpsc::unbounded_channel();
        let (chain_unavailable, _) = watch::channel(false);
        Self {
            auth: AuthRegistry::new(),
            limits: Mutex::new(HashMap::new()),
            delivered: Mutex::new(HashMap::new()),
            capacity,
            upstream,
            upstreams: Mutex::new(HashMap::new()),
            upstream_failure_tx,
            upstream_failure_rx: tokio::sync::Mutex::new(upstream_failure_rx),
            chain_unavailable,
            keep_serving_when_chain_unavailable: AtomicBool::new(false),
            max_concurrent_requests_per_deal: AtomicU32::new(
                dexdo_core::params::SELLER_MAX_CONCURRENT_REQUESTS_PER_DEAL.get(),
            ),
        }
    }

    pub fn set_max_concurrent_requests_per_deal(&self, limit: std::num::NonZeroU32) {
        self.max_concurrent_requests_per_deal
            .store(limit.get(), Ordering::Release);
    }

    pub fn active_requests(&self, token_contract: &str) -> u32 {
        self.capacity
            .active_shared_requests(&token_contract.to_string())
    }

    pub fn set_chain_unavailable_action(&self, action: ChainUnavailableAction) {
        self.keep_serving_when_chain_unavailable.store(
            action == ChainUnavailableAction::KeepServing,
            Ordering::Release,
        );
    }

    pub fn report_chain_unavailable(&self, token_contract: &str) {
        if self
            .keep_serving_when_chain_unavailable
            .load(Ordering::Acquire)
        {
            return;
        }
        tracing::error!(
            event = "seller_chain_unavailable",
            token_contract,
            action = "stop",
            "required chain read exhausted its retry budget; stopping seller delivery"
        );
        self.chain_unavailable.send_replace(true);
    }

    fn chain_unavailable(&self) -> bool {
        *self.chain_unavailable.borrow()
    }

    fn subscribe_chain_unavailable(&self) -> watch::Receiver<bool> {
        self.chain_unavailable.subscribe()
    }

    pub fn route_stream(&self, token_contract: &str, upstream: UpstreamConfig) {
        lock_or_recover(&self.upstreams, GATEWAY_UPSTREAMS_LOCK)
            .insert(token_contract.to_string(), upstream);
    }

    /// Register a deal from one coherent strict chain snapshot before exposing it to the buyer.
    pub fn register_stream(
        &self,
        token_contract: &str,
        buyer_pubkey: dexdo_core::note::NotePubkey,
        mock_token_count: u64,
        state: DealChainState,
        deal: DealSubscription,
    ) -> AnyResult<()> {
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        let snapshot = self
            .capacity
            .reconcile_deal(&token_contract.to_string(), state, deal)?
            .ok_or_else(|| anyhow!("TokenContract {token_contract_display} is terminal"))?;
        let local_delivered =
            u64::try_from(snapshot.local_delivered_after_anchor).map_err(|_| {
                anyhow!(
                "TokenContract {token_contract_display} durable local delivery {} exceeds gateway uint64",
                snapshot.local_delivered_after_anchor
            )
            })?;
        self.limits
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {GATEWAY_LIMITS_LOCK}"))?
            .insert(
                token_contract.to_string(),
                StreamLimits {
                    mock_token_count,
                    deal,
                },
            );
        self.auth.register(token_contract, buyer_pubkey);
        self.delivery(token_contract)
            .count
            .store(local_delivered, Ordering::Release);
        Ok(())
    }

    /// Remove only one terminal/failed deal from the shared listener.
    pub fn unregister_stream(&self, token_contract: &str) {
        crate::telemetry::forget(token_contract);
        if !self.chain_unavailable() {
            self.auth.unregister(token_contract);
        }
        lock_or_recover(&self.limits, GATEWAY_LIMITS_LOCK).remove(token_contract);
        lock_or_recover(&self.delivered, GATEWAY_DELIVERED_LOCK).remove(token_contract);
        lock_or_recover(&self.upstreams, GATEWAY_UPSTREAMS_LOCK).remove(token_contract);
    }

    pub fn reconcile_subscription_capacity(
        &self,
        token_contract: &str,
        state: DealChainState,
        subscription: DealSubscription,
    ) -> AnyResult<Option<CapacitySnapshot>> {
        if !subscription.is_subscription() {
            let token_contract = dexdo_core::address::display_self_dapp(token_contract);
            return Err(anyhow!(
                "TokenContract {token_contract}: subscription keeper observed an ordinary deal shape"
            ));
        }
        self.capacity
            .reconcile_deal(&token_contract.to_string(), state, subscription)
    }

    pub fn reconcile_deal_capacity(
        &self,
        token_contract: &str,
        state: DealChainState,
        deal: DealSubscription,
    ) -> AnyResult<Option<CapacitySnapshot>> {
        self.capacity
            .reconcile_deal(&token_contract.to_string(), state, deal)
    }

    pub fn prepare_probe_capacity(&self, token_contract: &str, delivered: u64) -> AnyResult<u64> {
        self.capacity
            .prepare_probe(&token_contract.to_string(), delivered)
    }

    pub fn reconcile_ordinary_capacity(
        &self,
        token_contract: &str,
        state: DealChainState,
    ) -> AnyResult<Option<CapacitySnapshot>> {
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        let deal = self
            .limits(token_contract)
            .ok_or_else(|| {
                anyhow!("TokenContract {token_contract_display}: deal capacity is not registered")
            })?
            .deal;
        if deal.is_subscription() {
            return Err(anyhow!(
                "TokenContract {token_contract_display}: ordinary claim driver observed a subscription deal shape"
            ));
        }
        self.reconcile_deal_capacity(token_contract, state, deal)
    }

    pub fn mark_subscription_terminal(&self, token_contract: &str) -> AnyResult<()> {
        self.mark_deal_terminal(token_contract)
    }

    pub fn mark_deal_terminal(&self, token_contract: &str) -> AnyResult<()> {
        self.capacity.mark_terminal(&token_contract.to_string())
    }

    #[cfg(test)]
    pub(crate) fn capacity_snapshot(
        &self,
        token_contract: &str,
    ) -> AnyResult<Option<CapacitySnapshot>> {
        self.capacity.snapshot(&token_contract.to_string())
    }

    #[cfg(test)]
    pub(crate) fn poison_limits_for_test(&self, panic_message: &'static str) {
        let _guard = self
            .limits
            .lock()
            .expect("gateway limits lock starts healthy");
        panic!("{panic_message}");
    }

    fn limits(&self, token_contract: &str) -> Option<StreamLimits> {
        lock_or_recover(&self.limits, GATEWAY_LIMITS_LOCK)
            .get(token_contract)
            .copied()
    }

    #[cfg(test)]
    fn stream_token_limit(
        &self,
        token_contract: &str,
        req: Option<&CanonRequest>,
        mock: bool,
    ) -> u64 {
        let Some(limits) = self.limits(token_contract) else {
            return 0;
        };
        Self::registered_stream_token_limit(limits, req, mock)
    }

    fn registered_stream_token_limit(
        limits: StreamLimits,
        req: Option<&CanonRequest>,
        mock: bool,
    ) -> u64 {
        if mock {
            return requested_max_tokens(req)
                .map(|max| limits.mock_token_count.min(max))
                .unwrap_or(limits.mock_token_count);
        }
        requested_max_tokens(req).unwrap_or(u64::MAX)
    }

    /// The per-deal [`DealDelivery`] tracker (created on first access, shared across the deal's streams). Each
    /// clean request terminal adds its checked billable total to cumulative `count`; `done` is not set here --
    /// it means "no more billable usage will ever arrive for this deal/session" and is owned by the buyer
    /// session lifecycle, never by a single stream. The seller driver reads both to bound finalized
    /// ticks by accepted billable usage.
    pub fn delivery(&self, token_contract: &str) -> DealDelivery {
        lock_or_recover(&self.delivered, GATEWAY_DELIVERED_LOCK)
            .entry(token_contract.to_string())
            .or_default()
            .clone()
    }

    pub(crate) fn upstream(&self, token_contract: &str) -> UpstreamConfig {
        lock_or_recover(&self.upstreams, GATEWAY_UPSTREAMS_LOCK)
            .get(token_contract)
            .cloned()
            .unwrap_or_else(|| self.upstream.clone())
    }

    pub async fn recv_upstream_failure(&self) -> Option<UpstreamFailure> {
        self.upstream_failure_rx.lock().await.recv().await
    }
}

fn requested_max_tokens(req: Option<&CanonRequest>) -> Option<u64> {
    req.and_then(|r| r.params.as_ref())
        .and_then(|p| (p.max_tokens != 0).then_some(p.max_tokens as u64))
}

impl Default for GatewayState {
    fn default() -> Self {
        Self::new()
    }
}

/// Relay content immediately, but hold terminal usage until a clean upstream EOF proves that no duplicate
/// usage or post-terminal content follows it. Only then is the complete billable total committed durably and
/// the typed usage frame released to the buyer.
#[cfg(test)]
pub(crate) async fn relay_counting<R>(
    up_rx: mpsc::Receiver<Result<UpstreamEvent, Status>>,
    tx: mpsc::Sender<Result<CanonChunk, Status>>,
    recorder: R,
    failure_context: Option<(
        String,
        Arc<AtomicU64>,
        mpsc::UnboundedSender<UpstreamFailure>,
    )>,
) where
    R: AuthoritativeDeliveryRecorder,
{
    relay_counting_with_chain_availability(
        up_rx,
        tx,
        recorder,
        failure_context,
        None,
        u64::MAX,
        u64::MAX,
        false,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn relay_counting_with_chain_availability<R>(
    mut up_rx: mpsc::Receiver<Result<UpstreamEvent, Status>>,
    tx: mpsc::Sender<Result<CanonChunk, Status>>,
    recorder: R,
    failure_context: Option<(
        String,
        Arc<AtomicU64>,
        mpsc::UnboundedSender<UpstreamFailure>,
    )>,
    mut chain_unavailable: Option<watch::Receiver<bool>>,
    output_limit_tokens: u64,
    billing_grant_tokens: u64,
    exact_output_usage: bool,
) where
    R: AuthoritativeDeliveryRecorder,
{
    let mut saw_output = false;
    let mut visible_output_tokens = 0u64;
    let mut next_seq = 0u64;
    let mut terminal_usage: Option<BillingUsage> = None;
    let mut billing_exhausted = false;
    let mut terminal_error = None;
    let finish = 'relay: loop {
        let event = if let Some(unavailable) = chain_unavailable.as_mut() {
            if *unavailable.borrow() {
                terminal_error = Some(chain_unavailable_status());
                break unfinished_delivery_finish(saw_output);
            }
            tokio::select! {
                biased;
                changed = unavailable.changed() => {
                    if changed.is_ok() && *unavailable.borrow() {
                        terminal_error = Some(chain_unavailable_status());
                        break unfinished_delivery_finish(saw_output);
                    }
                    if changed.is_err() {
                        up_rx.recv().await
                    } else {
                        continue;
                    }
                }
                event = up_rx.recv() => event,
            }
        } else {
            up_rx.recv().await
        };
        let Some(event) = event else {
            let Some(mut usage) = terminal_usage.take() else {
                terminal_error = Some(dexdo_proto::upstream_failure_status(
                    Status::data_loss("upstream ended without terminal billing usage"),
                    saw_output,
                ));
                break unfinished_delivery_finish(saw_output);
            };
            let Some(tokens) = NonZeroU64::new(usage.total_tokens.min(billing_grant_tokens)) else {
                terminal_error = Some(Status::data_loss(
                    "terminal billing usage has zero total after delivered output",
                ));
                break unfinished_delivery_finish(saw_output);
            };
            let permit = match tx.reserve().await {
                Ok(permit) => permit,
                Err(_) => break unfinished_delivery_finish(saw_output),
            };
            let charged = match recorder.complete_billing(tokens, billing_exhausted) {
                Ok(charged) => charged,
                Err(status) => {
                    terminal_error = Some(status);
                    break unfinished_delivery_finish(saw_output);
                }
            };
            if recorder.shared_budget() || charged != usage.total_tokens {
                usage.charged_tokens = Some(charged);
            }
            permit.send(Ok(CanonChunk {
                seq: next_seq,
                usage: Some(usage),
                ..CanonChunk::default()
            }));
            return;
        };
        match event {
            Ok(UpstreamEvent::Chunk(chunk)) => {
                if terminal_usage.is_some() {
                    terminal_error = Some(Status::data_loss(
                        "upstream content continued after terminal billing usage",
                    ));
                    break unfinished_delivery_finish(saw_output);
                }
                if chunk.usage.is_some() {
                    terminal_error = Some(Status::data_loss(
                        "upstream content event carried wire terminal usage",
                    ));
                    break unfinished_delivery_finish(saw_output);
                }
                if chunk.seq != next_seq {
                    terminal_error = Some(Status::data_loss(format!(
                        "upstream canonical sequence {} is not next expected {}",
                        chunk.seq, next_seq
                    )));
                    break unfinished_delivery_finish(saw_output);
                }
                let has_output = !chunk.text.is_empty()
                    || !chunk.reasoning.is_empty()
                    || !chunk.token_ids.is_empty();
                let chunk_output_tokens = if exact_output_usage || !chunk.token_ids.is_empty() {
                    chunk.visible_output_tokens()
                } else {
                    0
                };
                visible_output_tokens = match visible_output_tokens
                    .checked_add(chunk_output_tokens)
                    .filter(|tokens| *tokens <= output_limit_tokens)
                {
                    Some(tokens) => tokens,
                    None => {
                        terminal_error = Some(Status::data_loss(format!(
                            "upstream canonical output exceeds output limit {output_limit_tokens}"
                        )));
                        break unfinished_delivery_finish(saw_output);
                    }
                };
                next_seq = match chunk.seq.checked_add(1) {
                    Some(next) => next,
                    None => {
                        terminal_error =
                            Some(Status::data_loss("canonical sequence overflows u64"));
                        break 'relay unfinished_delivery_finish(saw_output);
                    }
                };
                let permit = match tx.reserve().await {
                    Ok(permit) => permit,
                    Err(_) => break unfinished_delivery_finish(saw_output),
                };
                permit.send(Ok(chunk));
                saw_output |= has_output;
            }
            Ok(UpstreamEvent::Usage(usage)) => {
                if terminal_usage.is_some() {
                    terminal_error = Some(Status::data_loss(
                        "upstream emitted duplicate terminal billing usage",
                    ));
                    break unfinished_delivery_finish(saw_output);
                }
                if let Err(error) = usage.validate() {
                    terminal_error = Some(Status::data_loss(error));
                    break unfinished_delivery_finish(saw_output);
                }
                billing_exhausted = usage.total_tokens > billing_grant_tokens;
                let output_matches = if exact_output_usage {
                    usage.output_tokens == visible_output_tokens
                } else {
                    usage.output_tokens >= visible_output_tokens
                };
                if !output_matches {
                    terminal_error = Some(Status::data_loss(format!(
                        "terminal output usage {} does not match visible output {visible_output_tokens}",
                        usage.output_tokens
                    )));
                    break unfinished_delivery_finish(saw_output);
                }
                if !saw_output || usage.output_tokens == 0 {
                    terminal_error = Some(Status::data_loss(
                        "terminal billing usage has no preceding delivered output",
                    ));
                    break AuthoritativeDeliveryFinish::Interrupted;
                }
                terminal_usage = Some(usage);
            }
            Err(status) => {
                let status = dexdo_proto::upstream_failure_status(status, saw_output);
                if let Some((token_contract, event_sequence, failure_tx)) = &failure_context {
                    let _ = failure_tx.send(UpstreamFailure::from_status(
                        token_contract,
                        &status,
                        event_sequence.clone(),
                    ));
                }
                terminal_error = Some(status);
                break unfinished_delivery_finish(saw_output);
            }
        }
    };
    if recorder.shared_budget() {
        if let Some(status) = terminal_error.take() {
            let _ = tx.send(Err(status)).await;
        }
        drop(tx);
        while up_rx.recv().await.is_some() {}
        if let Err(status) =
            recorder.record_authoritative_delivery(AuthoritativeDeliveryEvent::Finished(finish))
        {
            tracing::error!(%status, "shared request reconciliation failed after upstream drain");
        }
        return;
    }
    if let Err(status) =
        recorder.record_authoritative_delivery(AuthoritativeDeliveryEvent::Finished(finish))
    {
        terminal_error.get_or_insert(status);
    }
    if let Some(status) = terminal_error {
        let _ = tx.send(Err(status)).await;
    }
}

fn chain_unavailable_status() -> Status {
    Status::failed_precondition(
        "SELLER_CHAIN_UNAVAILABLE: required chain read exhausted its retry budget; restore chain access and restart the seller",
    )
}

fn capacity_status(error: impl std::fmt::Display) -> Status {
    Status::data_loss(format!("seller deal capacity persistence failed: {error}"))
}

fn reserve_status(error: ReserveError) -> Status {
    match error {
        ReserveError::Exhausted => {
            Status::resource_exhausted("deal delivery capacity is exhausted")
        }
        ReserveError::Concurrency => {
            let mut status = Status::resource_exhausted(error.to_string());
            status
                .metadata_mut()
                .insert("dexdo-error-code", "deal_busy".parse().unwrap());
            status
        }
        ReserveError::UnknownDeal | ReserveError::Terminal => {
            Status::failed_precondition(error.to_string())
        }
        ReserveError::InvalidState(_) => capacity_status(error),
    }
}

#[allow(clippy::result_large_err)]
fn checked_authoritative_tokens(count: &AtomicU64, tokens: u64) -> Result<u64, Status> {
    debug_assert!(tokens > 0);
    count
        .load(Ordering::Acquire)
        .checked_add(tokens)
        .ok_or_else(|| Status::data_loss("authoritative billable-token high-water overflow"))
}

/// gRPC implementation of the gateway service.
pub struct GatewayService {
    state: Arc<GatewayState>,
}

impl GatewayService {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Self { state }
    }

    /// Wrap into a tonic server for mounting in `Server::builder`.
    pub fn into_server(self) -> GatewayServer<Self> {
        GatewayServer::new(self).max_decoding_message_size(GATEWAY_REQUEST_MAX_BYTES)
    }
}

type ChunkStream = Pin<Box<dyn Stream<Item = Result<CanonChunk, Status>> + Send>>;

#[tonic::async_trait]
impl Gateway for GatewayService {
    /// Authorization step 1: issue a nonce bound to the token_contract.
    async fn get_challenge(
        &self,
        request: Request<ChallengeRequest>,
    ) -> Result<Response<Challenge>, Status> {
        let request = request.into_inner();
        let tc = request.token_contract;
        let mut nonce = Vec::new();
        if !request.capabilities_only {
            nonce.resize(32, 0);
            rand::rngs::OsRng.fill_bytes(&mut nonce);
            self.state.auth.issue_challenge(&tc, nonce.clone());
            if tc == HEALTH_CHALLENGE_TC {
                self.state.auth.discard_challenge(&tc, &nonce);
            }
        }
        let upstream = self.state.upstream(&tc);
        Ok(Response::new(Challenge {
            nonce,
            supports_reasoning_disable: upstream.supports_reasoning_disable(),
            supported_reasoning_efforts: upstream.supported_reasoning_efforts(),
            token_contract: tc,
        }))
    }

    type OpenStreamStream = ChunkStream;

    /// Step 2: verify the signature against the pubkey from the contract. Without a valid
    /// signature the connection closes BEFORE forwarding. Otherwise -- an incremental stream (R6).
    async fn open_stream(
        &self,
        request: Request<StreamRequest>,
    ) -> Result<Response<Self::OpenStreamStream>, Status> {
        let req = request.into_inner();
        // The billing-grant gate runs before signature verification consumes the single-use nonce
        // and before capacity or upstream observe the request.
        if req.billing_grant_tokens == 0 {
            return Err(Status::invalid_argument(
                "billing_grant_tokens must be nonzero",
            ));
        }
        if req.signature.len() != 64 {
            return Err(Status::unauthenticated("bad signature length"));
        }
        let mut sig = [0u8; 64];
        sig.copy_from_slice(&req.signature);
        let signature = Signature(sig);

        // Authorization BEFORE any forwarding: a scan/address leak without a key = rejection.
        if !self
            .state
            .auth
            .verify_response(&req.token_contract, &req.nonce, &signature)
        {
            return Err(Status::unauthenticated("challenge-response failed"));
        }
        // (challenge_bytes is used both here and on the buyer's side -- the same domain.)
        let _ = challenge_bytes(&req.token_contract, &req.nonce);

        if self.state.chain_unavailable() {
            return Err(chain_unavailable_status());
        }

        let limits = self.state.limits(&req.token_contract).ok_or_else(|| {
            Status::failed_precondition("this deal is not registered on this gateway")
        })?;

        let request = req.request;
        let upstream = self.state.upstream(&req.token_contract);
        upstream.validate_reasoning(request.as_ref().and_then(|r| r.params.as_ref()))?;
        let mock_upstream = matches!(
            &upstream,
            UpstreamConfig::Mock
                | UpstreamConfig::MockWithClaimedModel(_)
                | UpstreamConfig::MockScammer
        );
        let explicit_output_limit = requested_max_tokens(request.as_ref());
        let configured_output_limit =
            GatewayState::registered_stream_token_limit(limits, request.as_ref(), mock_upstream);
        if configured_output_limit == 0 {
            return Err(Status::invalid_argument(
                "canonical request output limit must be nonzero",
            ));
        }
        if explicit_output_limit.is_some_and(|limit| limit > req.billing_grant_tokens) {
            return Err(Status::invalid_argument(
                "canonical request output limit exceeds billing grant",
            ));
        }
        // The legacy promptless helper has no explicit CanonRequest output cap. Its declared billing
        // grant is still a hard total bound, so it also supplies the only buyer-side output ceiling;
        // an explicit contradictory max_tokens remains an error instead of being rewritten.
        let requested_output_limit = configured_output_limit.min(req.billing_grant_tokens);
        // Per-deal admission is checked before the upstream task can observe the request. Shared
        // requests hold live slots; legacy exclusive requests durably reserve their token amount.
        // Both ordinary and subscription limits come only from the matched TC's strict chain snapshot.
        // Seller-local delivery since that snapshot is private, so the actual reservation may be below
        // the buyer's declared grant; that authoritative amount becomes the stricter total-usage limit
        // and also clamps the provider's output-only cap.
        let delivered = self.state.delivery(&req.token_contract);
        let reservation = {
            let _guard = delivered.update_lock.lock().map_err(|_| {
                capacity_status(anyhow!("{POISONED_LOCK_MESSAGE}: {DELIVERY_UPDATE_LOCK}"))
            })?;
            // An overrun closes admission while accepted work drains, even with paid capacity left.
            // The same lock covers completion and its last-active-request check.
            if delivered.billing_exhausted.load(Ordering::Acquire) {
                return Err(reserve_status(ReserveError::Exhausted));
            }
            if req.shared_budget {
                self.state.capacity.admit_shared(
                    &req.token_contract,
                    req.billing_grant_tokens,
                    std::num::NonZeroU32::new(
                        self.state
                            .max_concurrent_requests_per_deal
                            .load(Ordering::Acquire),
                    )
                    .expect("positive seller concurrency limit"),
                )
            } else {
                self.state
                    .capacity
                    .reserve_exclusive(&req.token_contract, req.billing_grant_tokens)
            }
            .map_err(reserve_status)?
        };
        let effective_billing_grant = reservation.amount();
        let output_limit = requested_output_limit.min(effective_billing_grant);
        // R1: the upstream adapts the CANONICAL request that arrived in the opening
        // call alongside authorization. The mock model builds fake output from the prompt; the real
        // provider adapter proxies the request and normalizes the SSE (R1/R5/R6).
        // The per-deal billable tracker is shared across all of this deal's streams (the gateway map returns
        // the same `DealDelivery`), so checked input-plus-output totals accumulate over sequential requests.
        // An ordinary completion leaves the session open; an overrun closes it after accepted work drains.
        let chain_unavailable = self.state.subscribe_chain_unavailable();
        let failure_context = (
            req.token_contract.clone(),
            delivered.event_sequence.clone(),
            self.state.upstream_failure_tx.clone(),
        );
        // Incremental yielding (R6): content is forwarded without buffering. Terminal provider usage is held
        // through clean EOF, validated against both limits, then atomically commits the request total before
        // the same typed record is released to the buyer.
        let (up_tx, up_rx) = mpsc::channel(GATEWAY_UPSTREAM_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            upstream.run(output_limit, request, up_tx).await;
        });
        let (tx, rx) = mpsc::channel::<Result<CanonChunk, Status>>(GATEWAY_CLIENT_CHANNEL_CAPACITY);
        tokio::spawn(relay_counting_with_chain_availability(
            up_rx,
            tx,
            CapacityDeliveryRecorder {
                reservation,
                delivery: delivered,
            },
            Some(failure_context),
            Some(chain_unavailable),
            output_limit,
            effective_billing_grant,
            mock_upstream,
        ));

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{order_flags, LocalNote, Note, NotePubkey, SUBSCRIPTION_WEEKS, TICK_SIZE};
    use dexdo_proto::SamplingParams;
    use tokio_stream::StreamExt;

    fn recorder(count: Arc<AtomicU64>) -> DealDelivery {
        DealDelivery {
            count,
            ..DealDelivery::default()
        }
    }

    async fn relay_with_failure_seam(
        events: Vec<Result<UpstreamEvent, Status>>,
    ) -> (Vec<Result<CanonChunk, Status>>, Option<UpstreamFailure>) {
        let (up_tx, up_rx) = mpsc::channel(16);
        for event in events {
            up_tx.send(event).await.unwrap();
        }
        drop(up_tx);
        let (tx, mut rx) = mpsc::channel(16);
        let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
        relay_counting(
            up_rx,
            tx,
            recorder(Arc::new(AtomicU64::new(0))),
            Some((
                "0:deal".to_string(),
                Arc::new(AtomicU64::new(0)),
                failure_tx,
            )),
        )
        .await;
        let mut buyer = Vec::new();
        while let Some(item) = rx.recv().await {
            buyer.push(item);
        }
        let failure = failure_rx.try_recv().ok();
        assert!(failure_rx.try_recv().is_err(), "duplicate upstream event");
        (buyer, failure)
    }

    fn subscription_state(pending: u128) -> DealChainState {
        DealChainState {
            funded: true,
            opened: true,
            probe_accepted: true,
            disputed: false,
            deposit: 1,
            finalized_owed: 0,
            tokens_final: pending,
            tokens_pending: pending,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 0,
        }
    }

    fn subscription_shape() -> DealSubscription {
        DealSubscription {
            deal_flags: order_flags::SUBSCRIPTION,
            sub_weeks: 4,
            week_index: 0,
            tokens_per_week: 2 * TICK_SIZE,
            funded_tokens: 8 * TICK_SIZE,
            tokens_paid: 0,
            period_start: 1,
            week_base_tokens: 0,
        }
    }

    fn ordinary_shape(funded_tokens: u128) -> DealSubscription {
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
    }

    fn buyer_pubkey() -> NotePubkey {
        NotePubkey {
            x: [1; 32],
            ed: [2; 32],
        }
    }

    #[tokio::test]
    async fn upstream_401_is_forwarded_unchanged_and_reported_once_without_detail() {
        let message = "upstream HTTP 401 Unauthorized: sensitive provider error detail redacted";
        let (buyer, failure) =
            relay_with_failure_seam(vec![Err(Status::unavailable(message))]).await;

        assert_eq!(buyer.len(), 1);
        assert!(buyer[0]
            .as_ref()
            .is_err_and(|status| status.message() == message));
        let failure = failure.unwrap();
        assert_eq!(failure.token_contract, "0:deal");
        assert_eq!(failure.error_class, "auth");
        assert!(!failure.retryable);
        assert_eq!(failure.grpc_status, "Unavailable");
        assert_eq!(failure.http_status, Some(401));
        let safe = format!("{failure:?}");
        assert!(!safe.contains("provider error"), "{safe}");
        assert!(!safe.contains("Authorization"), "{safe}");
    }

    #[tokio::test]
    async fn upstream_request_rejection_is_reported_as_a_seller_configuration_fault() {
        // a provider `400` on a request the seller fully built is its own configuration fault -- a
        // distinct, never-retryable class, so the operator sees the cause instead of a generic `http`.
        let message =
            "upstream HTTP 400 Bad Request: max_tokens must be less than or equal to 40960 \
             [seller configuration fault: model \"qwen/qwen3-32b\" sent max_tokens=2000000 at \
             capabilities.max_output_tokens=2000000; correct this model's max_output_tokens in the \
             models config]";
        let (buyer, failure) =
            relay_with_failure_seam(vec![Err(Status::unavailable(message))]).await;

        assert_eq!(buyer.len(), 1);
        assert!(buyer[0]
            .as_ref()
            .is_err_and(|status| status.message() == message));
        let failure = failure.unwrap();
        assert_eq!(failure.error_class, "seller_config");
        assert!(!failure.retryable);
        assert_eq!(failure.http_status, Some(400));

        // Transient provider trouble keeps its existing retryable classes.
        let (_, throttled) = relay_with_failure_seam(vec![Err(Status::unavailable(
            "upstream HTTP 429 Too Many Requests",
        ))])
        .await;
        let throttled = throttled.unwrap();
        assert_eq!(throttled.error_class, "http");
        assert!(throttled.retryable);
    }

    fn authorized_request(
        state: &GatewayState,
        buyer: &LocalNote,
        token_contract: &str,
        max_tokens: u32,
        billing_grant_tokens: u64,
    ) -> Request<StreamRequest> {
        let nonce = vec![9; 32];
        state.auth.issue_challenge(token_contract, nonce.clone());
        let signature = buyer.sign(&challenge_bytes(token_contract, &nonce));
        Request::new(StreamRequest {
            shared_budget: false,
            token_contract: token_contract.to_string(),
            nonce,
            signature: signature.0.to_vec(),
            request: Some(CanonRequest {
                messages: Vec::new(),
                params: Some(SamplingParams {
                    max_tokens,
                    ..SamplingParams::default()
                }),
                ..Default::default()
            }),
            billing_grant_tokens,
        })
    }

    #[derive(Clone, Default)]
    struct EventRecorder(Arc<Mutex<Vec<AuthoritativeDeliveryEvent>>>);

    impl AuthoritativeDeliveryRecorder for EventRecorder {
        fn record_authoritative_delivery(
            &self,
            event: AuthoritativeDeliveryEvent,
        ) -> Result<(), Status> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    impl EventRecorder {
        fn events(&self) -> Vec<AuthoritativeDeliveryEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    #[derive(Clone, Default)]
    struct RecordingDelivery {
        delivery: DealDelivery,
        events: EventRecorder,
    }

    impl AuthoritativeDeliveryRecorder for RecordingDelivery {
        fn record_authoritative_delivery(
            &self,
            event: AuthoritativeDeliveryEvent,
        ) -> Result<(), Status> {
            self.delivery.record_authoritative_delivery(event)?;
            self.events.record_authoritative_delivery(event)
        }

        fn mark_billing_exhausted(&self) {
            self.delivery.mark_billing_exhausted();
        }
    }

    async fn run_openai_through_relay(
        body: String,
        key_env: &str,
    ) -> (
        u64,
        Vec<AuthoritativeDeliveryEvent>,
        Vec<Result<CanonChunk, Status>>,
        String,
    ) {
        use crate::seller::upstream::openai::{self, OpenAiConfig};
        use dexdo_proto::ChatMessage;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut next = [0_u8; 4096];
            loop {
                let read = socket.read(&mut next).await.unwrap();
                assert_ne!(read, 0, "fake provider received a truncated HTTP request");
                request.extend_from_slice(&next[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        std::env::set_var(key_env, "fake-provider-secret");
        let cfg = OpenAiConfig {
            base_url: format!("http://{address}"),
            api_key_env: key_env.to_string(),
            ..OpenAiConfig::default()
        };
        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            params: None,
            ..Default::default()
        };
        let (up_tx, up_rx) = mpsc::channel(8);
        let upstream = tokio::spawn(async move {
            openai::run(&cfg, None, 8, Some(request), up_tx).await;
        });
        let (buyer_tx, mut buyer_rx) = mpsc::channel(8);
        let recorder = RecordingDelivery::default();
        relay_counting(up_rx, buyer_tx, recorder.clone(), None).await;
        upstream.await.unwrap();
        std::env::remove_var(key_env);
        let provider_request = provider.await.unwrap();
        let mut buyer_events = Vec::new();
        while let Some(event) = buyer_rx.recv().await {
            buyer_events.push(event);
        }
        (
            recorder.delivery.count.load(Ordering::Acquire),
            recorder.events.events(),
            buyer_events,
            provider_request,
        )
    }

    /// Drive one gRPC stream through `relay_counting`: emit `n_ok` content chunks followed by either one
    /// terminal usage record or an error, and return how many items reached the buyer.
    async fn run_one_stream(count: Arc<AtomicU64>, n_ok: usize, trailing_err: bool) -> usize {
        let (up_tx, up_rx) = mpsc::channel(16);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(16);
        tokio::spawn(async move {
            for seq in 0..n_ok {
                up_tx
                    .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                        token_ids: vec![42],
                        seq: seq as u64,
                        ..CanonChunk::default()
                    })))
                    .await
                    .unwrap();
            }
            if trailing_err {
                up_tx
                    .send(Err(Status::internal("upstream error")))
                    .await
                    .unwrap();
            } else if n_ok > 0 {
                up_tx
                    .send(Ok(UpstreamEvent::Usage(
                        BillingUsage::new(1, n_ok as u64, None).unwrap(),
                    )))
                    .await
                    .unwrap();
            }
        });
        let relay = tokio::spawn(relay_counting(up_rx, tx, recorder(count), None));
        let mut forwarded = 0;
        while rx.recv().await.is_some() {
            forwarded += 1;
        }
        relay.await.unwrap();
        forwarded
    }

    /// an errored request never advances the monetary count, while the deal-level `done` remains
    /// owned by the buyer session lifecycle rather than one gRPC request.
    #[tokio::test]
    async fn relay_rejects_unbilled_output_without_marking_deal_done() {
        let delivery = DealDelivery::default();
        let forwarded = run_one_stream(delivery.count.clone(), 3, true).await;
        assert_eq!(
            delivery.count.load(Ordering::Acquire),
            0,
            "content without a clean terminal usage record is not billable"
        );
        assert_eq!(
            forwarded, 4,
            "three content chunks and the terminal error reach the buyer"
        );
        assert!(
            !delivery.done.load(Ordering::Acquire),
            "a single stream ending must NOT mark the deal done (the session lifecycle owns `done`)"
        );
    }

    /// a deal/session serves MANY sequential streams on one `token_contract`. Fetching the
    /// tracker by tc returns the SAME per-deal counter, so `count` accumulates across streams, and no stream may
    /// prematurely mark the deal `done` -- otherwise the seller `drive_advance` would catch up to only the first
    /// request and exit, under-finalizing a sustained by-fact session.
    #[tokio::test]
    async fn two_sequential_streams_accumulate_count_and_never_mark_deal_done() {
        let state = GatewayState::new();
        let tc = "0:deal";
        // First request's stream.
        let d1 = state.delivery(tc);
        run_one_stream(d1.count.clone(), 3, false).await;
        assert_eq!(
            d1.count.load(Ordering::Acquire),
            4,
            "the first request bills one input plus three output tokens"
        );
        assert!(
            !d1.done.load(Ordering::Acquire),
            "deal not done after the first stream"
        );
        // Second request's stream -- fetched by tc anew, as a fresh `open_stream` would: the same tracker,
        // still usable, already carrying the first stream's count.
        let d2 = state.delivery(tc);
        assert_eq!(
            d2.count.load(Ordering::Acquire),
            4,
            "the tracker fetched by tc shares the first stream's count"
        );
        run_one_stream(d2.count.clone(), 2, false).await;
        assert_eq!(
            d2.count.load(Ordering::Acquire),
            7,
            "billable totals accumulate across streams ((1 + 3) + (1 + 2))"
        );
        assert_eq!(
            d1.count.load(Ordering::Acquire),
            7,
            "both handles observe the shared cumulative count"
        );
        assert!(
            !d2.done.load(Ordering::Acquire),
            "still not done -- only the session lifecycle sets it"
        );
    }

    #[test]
    fn real_upstream_limit_uses_request_and_market_not_mock_fixture() {
        let state = GatewayState::new();
        let tc = "0:deal";
        state.limits.lock().unwrap().insert(
            tc.to_string(),
            StreamLimits {
                mock_token_count: 8,
                deal: ordinary_shape(300),
            },
        );
        let req = CanonRequest {
            messages: Vec::new(),
            params: Some(SamplingParams {
                max_tokens: 256,
                ..SamplingParams::default()
            }),
            ..Default::default()
        };

        assert_eq!(
            state.stream_token_limit(tc, Some(&req), false),
            256,
            "real upstream follows request max_tokens, not --mock-token-count"
        );
        assert_eq!(
            state.stream_token_limit(tc, None, false),
            u64::MAX,
            "without request max_tokens the later authoritative reservation supplies the cap"
        );
        assert_eq!(
            state.stream_token_limit(tc, Some(&req), true),
            8,
            "mock upstream keeps the explicit fake-token fixture"
        );
    }

    #[test]
    fn ordinary_registration_uses_actual_funded_capacity_not_seller_configuration() {
        let state = GatewayState::new();
        let tc = "0:ordinary";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + 17),
            )
            .unwrap();
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.funded_tokens, TICK_SIZE + 17);
        assert_eq!(snapshot.available().unwrap(), 17);
        assert_eq!(state.stream_token_limit(tc, None, false), u64::MAX);
        assert_eq!(
            state
                .capacity
                .reserve(&tc.to_string(), u64::MAX)
                .unwrap()
                .amount(),
            17
        );
    }

    #[test]
    fn ordinary_claim_state_reconciliation_opens_only_the_funded_remainder() {
        let state = GatewayState::new();
        let tc = "0:ordinary-probe-observer";
        let mut pre_probe = subscription_state(0);
        pre_probe.probe_accepted = false;
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                pre_probe,
                ordinary_shape(2 * TICK_SIZE),
            )
            .unwrap();
        let probe = state.capacity.reserve(&tc.to_string(), u64::MAX).unwrap();
        probe.record_delivered(TICK_SIZE as u64).unwrap();
        probe.finish_exact().unwrap();

        let accepted = state
            .reconcile_ordinary_capacity(tc, subscription_state(TICK_SIZE))
            .unwrap()
            .unwrap();
        assert_eq!(accepted.authoritative_cap, 2 * TICK_SIZE);
        assert_eq!(accepted.available().unwrap(), TICK_SIZE);
    }

    /// The counter the claim driver reads must keep counting across requests, including across the
    /// probe. `record_delivered` on a bare reservation moves the capacity ledger only; the request
    /// path goes through `CapacityDeliveryRecorder`, and it is that path which also feeds
    /// `delivery(tc).count` and decides whether a seller bills what it served.
    #[test]
    fn delivery_after_the_probe_reaches_the_counter_the_claim_driver_reads() {
        let directory = tempfile::tempdir().unwrap();
        let state = GatewayState::with_upstream_and_deals_dir(
            UpstreamConfig::Mock,
            directory.path().to_path_buf(),
        );
        let tc = "0:ordinary-two-requests";
        let mut pre_probe = subscription_state(0);
        pre_probe.probe_accepted = false;
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                pre_probe,
                ordinary_shape(4 * TICK_SIZE),
            )
            .unwrap();

        let deliver = |tokens: u128| {
            let recorder = CapacityDeliveryRecorder {
                reservation: state.capacity.reserve(&tc.to_string(), u64::MAX).unwrap(),
                delivery: state.delivery(tc),
            };
            recorder
                .record_authoritative_delivery(AuthoritativeDeliveryEvent::Completed(
                    NonZeroU64::new(tokens as u64).unwrap(),
                ))
                .unwrap();
        };

        deliver(TICK_SIZE);
        assert_eq!(
            state.delivery(tc).count.load(Ordering::Acquire),
            TICK_SIZE as u64,
            "the probe request is visible to the claim driver"
        );

        state
            .reconcile_ordinary_capacity(tc, subscription_state(TICK_SIZE))
            .unwrap()
            .unwrap();

        deliver(2 * TICK_SIZE);
        assert_eq!(
            state.delivery(tc).count.load(Ordering::Acquire),
            (3 * TICK_SIZE) as u64,
            "two ticks served after the probe must reach the counter the claim driver reads, or \
             the seller delivers them and never bills them"
        );
    }

    #[test]
    fn probe_acceptance_preserves_delivery_after_the_decision_2046() {
        let directory = tempfile::tempdir().unwrap();
        let state = GatewayState::with_upstream_and_deals_dir(
            UpstreamConfig::Mock,
            directory.path().to_path_buf(),
        );
        let tc = "0:probe-decision-race-2046";
        let mut pre_probe = subscription_state(0);
        pre_probe.probe_accepted = false;
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                pre_probe,
                ordinary_shape(2 * TICK_SIZE),
            )
            .unwrap();
        let deliver = |tokens| {
            CapacityDeliveryRecorder {
                reservation: state.capacity.reserve(&tc.to_string(), u64::MAX).unwrap(),
                delivery: state.delivery(tc),
            }
            .record_authoritative_delivery(AuthoritativeDeliveryEvent::Completed(
                NonZeroU64::new(tokens).unwrap(),
            ))
            .unwrap();
        };
        deliver(57);
        let delivery_at_probe = state.delivery(tc).count.load(Ordering::Acquire);
        assert_eq!(delivery_at_probe, 57);
        assert_eq!(
            state.prepare_probe_capacity(tc, delivery_at_probe).unwrap(),
            57
        );
        // Another request finishes while acceptProbe is awaiting its chain response.
        deliver(196);
        let accepted = state
            .reconcile_ordinary_capacity(tc, subscription_state(TICK_SIZE))
            .unwrap()
            .unwrap();
        assert_eq!(accepted.local_delivered_after_anchor, 196);
        let claim = TICK_SIZE
            + u128::from(state.delivery(tc).count.load(Ordering::Acquire) - delivery_at_probe);
        let reconciled = state
            .reconcile_ordinary_capacity(tc, subscription_state(claim))
            .unwrap()
            .unwrap();
        assert_eq!(reconciled.tokens_pending_anchor, TICK_SIZE + 196);
        assert_eq!(reconciled.local_delivered_after_anchor, 0);
    }

    #[test]
    fn subscription_reservation_uses_authoritative_week_not_advertised_maximum() {
        let state = GatewayState::new();
        let tc = "0:subscription";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                subscription_state(TICK_SIZE),
                subscription_shape(),
            )
            .unwrap();
        let reservation = state.capacity.reserve(&tc.to_string(), u64::MAX).unwrap();
        assert_eq!(
            reservation.amount(),
            TICK_SIZE as u64,
            "the current week has only one tick left after the accepted probe"
        );
    }

    #[tokio::test]
    async fn missing_anthropic_usage_retains_the_durable_request_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let state = GatewayState::with_upstream_and_deals_dir(
            UpstreamConfig::Mock,
            directory.path().to_path_buf(),
        );
        let tc = "0:anthropic-ambiguous";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                100,
                subscription_state(TICK_SIZE),
                subscription_shape(),
            )
            .unwrap();
        let reservation = state.capacity.reserve(&tc.to_string(), 100).unwrap();
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                text: "forwarded before usage".into(),
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(
            up_rx,
            tx,
            CapacityDeliveryRecorder {
                reservation,
                delivery: state.delivery(tc),
            },
            None,
        )
        .await;
        assert!(rx.recv().await.unwrap().is_ok());
        assert_eq!(
            rx.recv().await.unwrap().unwrap_err().code(),
            tonic::Code::DataLoss
        );
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 0);
        assert_eq!(snapshot.outstanding_reservation, 100);
        assert_eq!(snapshot.available().unwrap(), TICK_SIZE - 100);
    }

    /// terminal usage is tentative until clean EOF. A later duplicate invalidates the request and
    /// cannot advance either the claim-driving counter or durable billable delivery.
    #[tokio::test]
    async fn duplicate_terminal_usage_after_output_has_no_accounting_delta() {
        let state = GatewayState::new();
        let tc = "0:expose-after-exhaustion";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                100,
                subscription_state(TICK_SIZE),
                subscription_shape(),
            )
            .unwrap();
        let reservation = state.capacity.reserve(&tc.to_string(), 4).unwrap();
        assert_eq!(reservation.amount(), 4);
        let (up_tx, up_rx) = mpsc::channel(4);
        let (tx, mut rx) = mpsc::channel(4);
        for event in [
            Ok(UpstreamEvent::Chunk(CanonChunk {
                text: "billable".into(),
                ..CanonChunk::default()
            })),
            Ok(UpstreamEvent::Usage(BillingUsage::new(3, 1, None).unwrap())),
            Ok(UpstreamEvent::Usage(BillingUsage::new(3, 1, None).unwrap())),
        ] {
            up_tx.send(event).await.unwrap();
        }
        drop(up_tx);

        relay_counting(
            up_rx,
            tx,
            CapacityDeliveryRecorder {
                reservation,
                delivery: state.delivery(tc),
            },
            None,
        )
        .await;

        let mut buyer = Vec::new();
        while let Some(item) = rx.recv().await {
            buyer.push(item);
        }
        assert_eq!(buyer[0].as_ref().unwrap().text, "billable");
        let refusal = buyer
            .last()
            .expect("the refused request terminates the stream")
            .as_ref()
            .expect_err("the request is refused, not silently truncated");
        assert_eq!(refusal.code(), tonic::Code::DataLoss);
        assert_eq!(
            state.delivery(tc).count.load(Ordering::Acquire),
            0,
            "duplicate terminal usage cannot become claimable"
        );
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 0);
        assert_eq!(snapshot.outstanding_reservation, 4);
    }

    #[tokio::test]
    async fn provider_error_before_output_releases_subscription_reservation() {
        let state = GatewayState::new();
        let tc = "0:provider-error";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                100,
                subscription_state(TICK_SIZE),
                subscription_shape(),
            )
            .unwrap();
        let reservation = state.capacity.reserve(&tc.to_string(), 100).unwrap();
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel(1);
        up_tx
            .send(Err(Status::unavailable("provider unavailable")))
            .await
            .unwrap();
        drop(up_tx);
        relay_counting(
            up_rx,
            tx,
            CapacityDeliveryRecorder {
                reservation,
                delivery: state.delivery(tc),
            },
            None,
        )
        .await;
        assert_eq!(
            rx.recv().await.unwrap().unwrap_err().code(),
            tonic::Code::Unavailable
        );
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.outstanding_reservation, 0);
        assert_eq!(snapshot.available().unwrap(), TICK_SIZE);
    }

    #[tokio::test]
    async fn terminal_total_above_reservation_delivers_the_full_response_and_caps_billing() {
        let state = GatewayState::new();
        let tc = "0:fat-structured-chunk";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + 2),
            )
            .unwrap();
        let reservation = state.capacity.reserve(&tc.to_string(), u64::MAX).unwrap();
        assert_eq!(reservation.amount(), 2);
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                text: "full provider response".into(),
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        up_tx
            .send(Ok(UpstreamEvent::Usage(
                BillingUsage::new(0, 3, None).unwrap(),
            )))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting_with_chain_availability(
            up_rx,
            tx,
            CapacityDeliveryRecorder {
                reservation,
                delivery: state.delivery(tc),
            },
            None,
            None,
            2,
            2,
            false,
        )
        .await;
        assert_eq!(
            rx.recv().await.unwrap().unwrap().text,
            "full provider response"
        );
        let usage = rx.recv().await.unwrap().unwrap().usage.unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.total_tokens, 3);
        assert_eq!(usage.billable_tokens(), 2);
        assert!(rx.recv().await.is_none());
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 2);
        assert_eq!(snapshot.outstanding_reservation, 0);
        assert_eq!(snapshot.available().unwrap(), 0);
        assert_eq!(state.delivery(tc).count.load(Ordering::Acquire), 2);
        assert!(state.delivery(tc).done.load(Ordering::Acquire));
        assert!(state.delivery(tc).billing_exhausted.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn capacity_recorder_overflow_changes_no_state_and_exposes_no_chunk() {
        let directory = tempfile::tempdir().unwrap();
        let state = GatewayState::with_upstream_and_deals_dir(
            UpstreamConfig::Mock,
            directory.path().to_path_buf(),
        );
        let tc = "0:capacity-recorder-overflow";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + 2),
            )
            .unwrap();
        let reservation = state.capacity.reserve(&tc.to_string(), u64::MAX).unwrap();
        assert_eq!(reservation.amount(), 2);
        let delivery = state.delivery(tc);
        delivery.count.store(u64::MAX, Ordering::Relaxed);
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                token_ids: vec![1],
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        up_tx
            .send(Ok(UpstreamEvent::Usage(
                BillingUsage::new(2, 1, None).unwrap(),
            )))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting_with_chain_availability(
            up_rx,
            tx,
            CapacityDeliveryRecorder {
                reservation,
                delivery: delivery.clone(),
            },
            None,
            None,
            1,
            2,
            true,
        )
        .await;
        assert_eq!(rx.recv().await.unwrap().unwrap().token_ids, vec![1]);
        let status = rx.recv().await.unwrap().unwrap_err();
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert_eq!(
            status.message(),
            "authoritative billable-token high-water overflow"
        );
        assert!(rx.recv().await.is_none());
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 0);
        assert_eq!(snapshot.outstanding_reservation, 2);
        assert_eq!(snapshot.available().unwrap(), 0);
        assert_eq!(delivery.count.load(Ordering::Acquire), u64::MAX);
        assert!(!delivery.done.load(Ordering::Acquire));
        assert!(!delivery.billing_exhausted.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn open_stream_keeps_output_cap_separate_from_total_billing_grant() {
        let state = Arc::new(GatewayState::new());
        let buyer = LocalNote::generate();
        let tc = "0:open-stream-clamp";
        state
            .register_stream(
                tc,
                buyer.pubkey(),
                100,
                subscription_state(2 * TICK_SIZE - 3),
                subscription_shape(),
            )
            .unwrap();
        let service = GatewayService::new(state.clone());
        let mut request = authorized_request(&state, &buyer, tc, 2, 3);
        request.get_mut().request.as_mut().unwrap().messages = vec![dexdo_proto::ChatMessage {
            role: "user".into(),
            content: "one".into(),
        }];
        let response = service.open_stream(request).await.unwrap();
        let mut stream = response.into_inner();
        let mut chunks = 0;
        while stream.next().await.is_some() {
            chunks += 1;
        }
        assert_eq!(
            chunks, 3,
            "two output frames plus one terminal usage frame are returned"
        );
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 3);
        assert_eq!(snapshot.outstanding_reservation, 0);
        assert_eq!(snapshot.available().unwrap(), 0);
    }

    #[tokio::test]
    async fn open_stream_clamps_private_capacity_below_the_buyer_grant() {
        let state = Arc::new(GatewayState::new());
        let buyer = LocalNote::generate();
        let tc = "0:open-stream-private-cap";
        state
            .register_stream(
                tc,
                buyer.pubkey(),
                100,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + 3),
            )
            .unwrap();
        assert_eq!(
            state
                .capacity_snapshot(tc)
                .unwrap()
                .unwrap()
                .available()
                .unwrap(),
            3,
            "only the seller knows its three-token authoritative remainder"
        );

        let service = GatewayService::new(state.clone());
        let response = service
            .open_stream(authorized_request(&state, &buyer, tc, 100, 100))
            .await
            .expect("a larger buyer grant is clamped, not rejected");
        let mut stream = response.into_inner();
        let mut output = 0_u64;
        let mut terminal = None;
        while let Some(item) = stream.next().await {
            let chunk = item.expect("empty-input mock usage fits the private remainder");
            if let Some(usage) = chunk.usage {
                terminal = Some(usage);
            } else {
                output += chunk.visible_output_tokens();
            }
        }
        assert_eq!(
            output, 3,
            "the provider output cap is physically reduced from the requested 100 to capacity 3"
        );
        assert_eq!(terminal.unwrap().total_tokens, 3);
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 3);
        assert_eq!(snapshot.outstanding_reservation, 0);
        assert_eq!(state.delivery(tc).count.load(Ordering::Acquire), 3);
    }

    #[tokio::test]
    async fn reasoning_2090_capability_queries_leave_authorization_nonces_untouched() {
        let state = Arc::new(GatewayState::new());
        let buyer = LocalNote::generate();
        let tc = "0:capability-query";
        state.auth.register(tc, buyer.pubkey());
        let service = GatewayService::new(state.clone());
        let challenge = service
            .get_challenge(Request::new(ChallengeRequest {
                token_contract: tc.into(),
                capabilities_only: false,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!challenge.nonce.is_empty());
        assert_eq!(state.auth.outstanding_challenge_count(tc), 1);
        for _ in 0..3 {
            let query = service
                .get_challenge(Request::new(ChallengeRequest {
                    token_contract: tc.into(),
                    capabilities_only: true,
                }))
                .await
                .unwrap()
                .into_inner();
            assert!(query.nonce.is_empty());
            assert_eq!(query.token_contract, tc);
            assert_eq!(
                query.supports_reasoning_disable,
                challenge.supports_reasoning_disable
            );
            assert_eq!(state.auth.outstanding_challenge_count(tc), 1);
            let signature = buyer.sign(&challenge_bytes(tc, &query.nonce));
            assert!(!state.auth.verify_response(tc, &query.nonce, &signature));
        }
        let signature = buyer.sign(&challenge_bytes(tc, &challenge.nonce));
        assert!(state.auth.verify_response(tc, &challenge.nonce, &signature));
        assert_eq!(state.auth.outstanding_challenge_count(tc), 0);
    }

    #[tokio::test]
    async fn reasoning_2172_gateway_rechecks_effort_before_capacity_reservation() {
        for (effort, disabled, declared) in [
            ("high", false, false),
            ("invalid", false, true),
            ("low", false, true),
            ("high", true, true),
        ] {
            let mut cfg = crate::seller::OpenAiConfig {
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: "DEXDO_2172_UNUSED_FIXTURE_KEY".into(),
                ..Default::default()
            };
            cfg.capabilities.reasoning_disable =
                dexdo_core::params::ReasoningDisableControl::ChatTemplate;
            if declared {
                cfg.capabilities.reasoning_effort = Some(
                    serde_json::from_value(serde_json::json!({
                        "control":"reasoning_effort", "supported":["high"]
                    }))
                    .unwrap(),
                );
            }
            let state = Arc::new(GatewayState::with_upstream(UpstreamConfig::OpenAi(cfg)));
            let buyer = LocalNote::generate();
            let tc = "0:unsupported-effort";
            state
                .register_stream(
                    tc,
                    buyer.pubkey(),
                    8,
                    subscription_state(TICK_SIZE),
                    ordinary_shape(TICK_SIZE + 8),
                )
                .unwrap();
            let before = state.capacity_snapshot(tc).unwrap().unwrap();
            let service = GatewayService::new(state.clone());
            let query = service
                .get_challenge(Request::new(ChallengeRequest {
                    token_contract: tc.into(),
                    capabilities_only: true,
                }))
                .await
                .unwrap()
                .into_inner();
            assert!(query.nonce.is_empty());
            assert_eq!(
                query.supported_reasoning_efforts,
                if declared {
                    vec!["high".to_owned()]
                } else {
                    vec![]
                }
            );
            assert_eq!(state.auth.outstanding_challenge_count(tc), 0);
            let mut request = authorized_request(&state, &buyer, tc, 8, 8);
            let params = request
                .get_mut()
                .request
                .as_mut()
                .unwrap()
                .params
                .as_mut()
                .unwrap();
            params.reasoning_effort = Some(effort.into());
            params.disable_reasoning = disabled;
            let Err(error) = service.open_stream(request).await else {
                panic!("unsupported effort must be refused")
            };
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(error.message().starts_with("unsupported_parameter:"));
            let after = state.capacity_snapshot(tc).unwrap().unwrap();
            assert_eq!(
                after.outstanding_reservation,
                before.outstanding_reservation
            );
            assert_eq!(
                after.local_delivered_after_anchor,
                before.local_delivered_after_anchor
            );
            assert_eq!(after.available().unwrap(), before.available().unwrap());
        }
    }

    #[tokio::test]
    async fn unsupported_reasoning_2090_is_refused_before_capacity_reservation() {
        let state = Arc::new(GatewayState::new());
        let buyer = LocalNote::generate();
        let tc = "0:unsupported-reasoning";
        state
            .register_stream(
                tc,
                buyer.pubkey(),
                8,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + 8),
            )
            .unwrap();
        let before = state.capacity_snapshot(tc).unwrap().unwrap();
        let mut request = authorized_request(&state, &buyer, tc, 8, 8);
        request
            .get_mut()
            .request
            .as_mut()
            .unwrap()
            .params
            .as_mut()
            .unwrap()
            .disable_reasoning = true;
        let result = GatewayService::new(state.clone())
            .open_stream(request)
            .await;
        let Err(error) = result else {
            panic!("unsupported control must be refused");
        };
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(error.message().starts_with("unsupported_parameter:"));
        let after = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(
            after.outstanding_reservation,
            before.outstanding_reservation
        );
        assert_eq!(
            after.local_delivered_after_anchor,
            before.local_delivered_after_anchor
        );
        assert_eq!(after.available().unwrap(), before.available().unwrap());
    }

    #[tokio::test]
    async fn terminal_total_above_the_private_capacity_is_capped_and_marks_the_deal_done() {
        let state = Arc::new(GatewayState::new());
        let buyer = LocalNote::generate();
        let tc = "0:open-stream-private-cap-overflow";
        state
            .register_stream(
                tc,
                buyer.pubkey(),
                100,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + 3),
            )
            .unwrap();
        let service = GatewayService::new(state.clone());
        let mut request = authorized_request(&state, &buyer, tc, 100, 100);
        request.get_mut().request.as_mut().unwrap().messages = vec![dexdo_proto::ChatMessage {
            role: "user".into(),
            content: "one".into(),
        }];
        let response = service
            .open_stream(request)
            .await
            .expect("the authoritative remainder can reserve before provider usage is known");
        let mut stream = response.into_inner();
        let mut output = 0_u64;
        let mut terminal = None;
        while let Some(item) = stream.next().await {
            let chunk = item.expect("provider overrun is capped to the authorized reservation");
            if let Some(usage) = chunk.usage {
                terminal = Some(usage);
            } else {
                output += chunk.visible_output_tokens();
            }
        }
        assert_eq!(
            output, 3,
            "the effective output cap is the private reservation"
        );
        let usage = terminal.expect("the buyer receives native usage with a capped charge");
        usage.validate().unwrap();
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.total_tokens, 4);
        assert_eq!(usage.billable_tokens(), 3);
        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(
            snapshot.local_delivered_after_anchor, 3,
            "only the authorized reservation becomes durable billable delivery"
        );
        assert_eq!(
            snapshot.outstanding_reservation, 0,
            "the accepted capped delivery consumes the reservation"
        );
        assert_eq!(state.delivery(tc).count.load(Ordering::Acquire), 3);
        assert!(state.delivery(tc).done.load(Ordering::Acquire));
        assert!(state.delivery(tc).billing_exhausted.load(Ordering::Acquire));
    }

    /// E2E-ROW: E2E-USAGE-14/L0
    #[tokio::test]
    async fn cache_usage_2132_gateway_preserves_native_counts_and_caps_only_payment() {
        use crate::seller::{AnthropicConfig, OpenAiConfig};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for anthropic in [false, true] {
            for shared in [false, true] {
                for (grant, invalid) in [(5, false), (12, false), (5, true)] {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    let write = if invalid { -1 } else { 3 };
                    let wire = if anthropic {
                        format!("event: message_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\nevent: message_delta\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                            serde_json::json!({"type":"message_start","message":{"usage":{
                                "input_tokens":0,"cache_read_input_tokens":4,"cache_creation_input_tokens":write}}}),
                            serde_json::json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"answer"}}),
                            serde_json::json!({"type":"message_delta","usage":{"output_tokens":5}}))
                    } else {
                        format!(
                            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            serde_json::json!({"choices":[{"delta":{"content":"answer"}}]}),
                            serde_json::json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":5,
                                "total_tokens":12,"prompt_tokens_details":{"cached_tokens":4,"cache_write_tokens":write}}})
                        )
                    };
                    let server = tokio::spawn(async move {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        let mut request = vec![0; 8192];
                        let read = socket.read(&mut request).await.unwrap();
                        assert_ne!(read, 0, "scripted provider received no request bytes");
                        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{wire}", wire.len());
                        socket.write_all(response.as_bytes()).await.unwrap();
                    });
                    let upstream = if anthropic {
                        UpstreamConfig::Anthropic(AnthropicConfig {
                            base_url: format!("http://{address}"),
                            model: "scripted".into(),
                            frame_model: "m".into(),
                            api_key_env: "PATH".into(),
                            tokenizer_family: "scripted".into(),
                            max_output_tokens: Some(5),
                            reasoning_disable: Default::default(),
                        })
                    } else {
                        UpstreamConfig::OpenAi(OpenAiConfig {
                            base_url: format!("http://{address}"),
                            frame_model: "m".into(),
                            api_key_env: "PATH".into(),
                            ..Default::default()
                        })
                    };
                    let state = Arc::new(GatewayState::with_upstream(upstream));
                    let buyer = LocalNote::generate();
                    let tc = "0:cache-usage-2132";
                    state
                        .register_stream(
                            tc,
                            buyer.pubkey(),
                            5,
                            subscription_state(TICK_SIZE),
                            ordinary_shape(TICK_SIZE + 20),
                        )
                        .unwrap();
                    let mut request = authorized_request(&state, &buyer, tc, 5, grant);
                    request.get_mut().shared_budget = shared;
                    let mut stream = GatewayService::new(state.clone())
                        .open_stream(request)
                        .await
                        .unwrap()
                        .into_inner();
                    let mut terminal = None;
                    let mut error = None;
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(chunk) if chunk.usage.is_some() => terminal = chunk.usage,
                            Ok(_) => {}
                            Err(status) => error = Some(status),
                        }
                    }
                    server.await.unwrap();
                    let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
                    if invalid {
                        assert!(terminal.is_none());
                        assert_eq!(error.unwrap().code(), tonic::Code::DataLoss);
                        assert_eq!(snapshot.local_delivered_after_anchor, 0);
                        assert_eq!(state.delivery(tc).count.load(Ordering::Acquire), 0);
                    } else {
                        assert!(error.is_none(), "{error:?}");
                        let usage = terminal.unwrap();
                        usage.validate().unwrap();
                        assert_eq!(
                            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
                            (7, 5, 12)
                        );
                        assert_eq!((usage.cache_read_tokens, usage.cache_write_tokens), (4, 3));
                        assert_eq!(usage.billable_tokens(), grant);
                        assert_eq!(snapshot.local_delivered_after_anchor, u128::from(grant));
                        assert_eq!(snapshot.outstanding_reservation, 0);
                        assert_eq!(state.delivery(tc).count.load(Ordering::Acquire), grant);
                        assert_eq!(
                            state.delivery(tc).billing_exhausted.load(Ordering::Acquire),
                            grant < 12
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn exhausted_subscription_rejects_before_upstream_task_creation() {
        let state = Arc::new(GatewayState::new());
        let buyer = LocalNote::generate();
        let tc = "0:open-stream-exhausted";
        state
            .register_stream(
                tc,
                buyer.pubkey(),
                100,
                subscription_state(2 * TICK_SIZE),
                subscription_shape(),
            )
            .unwrap();
        let service = GatewayService::new(state.clone());
        let error = service
            .open_stream(authorized_request(&state, &buyer, tc, 100, 100))
            .await
            .err()
            .expect("zero weekly remainder must be rejected synchronously");
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        assert_eq!(
            state.delivery(tc).count.load(Ordering::Acquire),
            0,
            "no upstream delivery tracker changed"
        );
    }

    #[tokio::test]
    async fn post_term_subscription_rejects_before_upstream_task_creation() {
        let state = Arc::new(GatewayState::new());
        let buyer = LocalNote::generate();
        let tc = "0:post-term-open-stream";
        let pending = 3 * TICK_SIZE;
        let mut shape = subscription_shape();
        shape.week_index = SUBSCRIPTION_WEEKS;
        shape.tokens_paid = shape.funded_tokens;
        shape.week_base_tokens = pending;
        state
            .register_stream(tc, buyer.pubkey(), 100, subscription_state(pending), shape)
            .unwrap();

        let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
        assert_eq!(snapshot.authoritative_cap, pending);
        assert_eq!(snapshot.available().unwrap(), 0);
        let service = GatewayService::new(state.clone());
        let error = service
            .open_stream(authorized_request(&state, &buyer, tc, 100, 100))
            .await
            .err()
            .expect("post-term request must be rejected before upstream");
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        assert_eq!(state.delivery(tc).count.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn relay_counts_tokens_from_structured_signals_not_chunks() {
        let count = Arc::new(AtomicU64::new(0));
        let (up_tx, up_rx) = mpsc::channel(16);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(16);
        tokio::spawn(async move {
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    token_ids: vec![1, 2, 3],
                    ..CanonChunk::default()
                })))
                .await
                .unwrap();
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    seq: 1,
                    ..CanonChunk::default()
                })))
                .await
                .unwrap();
            up_tx
                .send(Ok(UpstreamEvent::Usage(
                    BillingUsage::new(2, 3, None).unwrap(),
                )))
                .await
                .unwrap();
        });
        relay_counting(up_rx, tx, recorder(count.clone()), None).await;
        while rx.recv().await.is_some() {}
        assert_eq!(
            count.load(Ordering::Acquire),
            5,
            "the monetary counter uses terminal input + output, not chunk count"
        );
    }

    #[tokio::test]
    async fn relay_uses_authoritative_usage_instead_of_anthropic_delta_count() {
        let count = Arc::new(AtomicU64::new(0));
        let (up_tx, up_rx) = mpsc::channel(16);
        let (tx, mut rx) = mpsc::channel(16);
        tokio::spawn(async move {
            for text in ["Hello", " world"] {
                up_tx
                    .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                        text: text.into(),
                        seq: if text == "Hello" { 0 } else { 1 },
                        ..CanonChunk::default()
                    })))
                    .await
                    .unwrap();
            }
            up_tx
                .send(Ok(UpstreamEvent::Usage(
                    BillingUsage::new(7, 5, None).unwrap(),
                )))
                .await
                .unwrap();
        });
        relay_counting(up_rx, tx, recorder(count.clone()), None).await;
        let mut delivered_chunks = 0;
        while rx.recv().await.is_some() {
            delivered_chunks += 1;
        }
        assert_eq!(delivered_chunks, 3);
        assert_eq!(count.load(Ordering::Acquire), 12);
    }

    #[tokio::test]
    async fn buyer_disconnect_before_delivery_does_not_advance_count() {
        let count = Arc::new(AtomicU64::new(0));
        let (up_tx, up_rx) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel::<Result<CanonChunk, Status>>(1);
        drop(rx);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                token_ids: vec![1, 2, 3],
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, recorder(count.clone()), None).await;
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "only a chunk successfully accepted by the buyer stream is billable"
        );
    }

    #[tokio::test]
    async fn high_water_overflow_is_explicit_and_does_not_wrap() {
        let count = Arc::new(AtomicU64::new(u64::MAX));
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                token_ids: vec![1],
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        up_tx
            .send(Ok(UpstreamEvent::Usage(
                BillingUsage::new(1, 1, None).unwrap(),
            )))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, recorder(count.clone()), None).await;
        assert_eq!(rx.recv().await.unwrap().unwrap().token_ids, vec![1]);
        let status = rx.recv().await.unwrap().unwrap_err();
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(rx.recv().await.is_none());
        assert_eq!(count.load(Ordering::Acquire), u64::MAX);
    }

    #[tokio::test]
    async fn accepted_structured_chunk_is_forwarded_once_and_recorded_once() {
        let events = EventRecorder::default();
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                token_ids: vec![1, 2, 3],
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        up_tx
            .send(Ok(UpstreamEvent::Usage(
                BillingUsage::new(2, 3, None).unwrap(),
            )))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, events.clone(), None).await;
        let chunk = rx.recv().await.unwrap().unwrap();
        assert_eq!(chunk.token_ids, vec![1, 2, 3]);
        let usage = rx.recv().await.unwrap().unwrap().usage.unwrap();
        assert_eq!(usage.total_tokens, 5);
        assert!(rx.recv().await.is_none());
        assert_eq!(
            events.events(),
            vec![AuthoritativeDeliveryEvent::Completed(
                NonZeroU64::new(5).unwrap()
            )]
        );
    }

    /// E2E-UPS-07: a provider stream that dies before its terminal native usage forwards its output but
    /// records no delivery at all -- the request is never cleanly releasable and never billable.
    #[tokio::test]
    async fn truncated_openai_eof_records_no_delivery_and_never_clean() {
        let events = EventRecorder::default();
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                text: "forwarded".into(),
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        up_tx
            .send(Err(Status::data_loss(
                "OpenAI-compatible SSE ended without [DONE]",
            )))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, events.clone(), None).await;
        assert!(rx.recv().await.unwrap().is_ok());
        assert_eq!(
            rx.recv().await.unwrap().unwrap_err().code(),
            tonic::Code::DataLoss
        );
        assert_eq!(
            events.events(),
            vec![AuthoritativeDeliveryEvent::Finished(
                AuthoritativeDeliveryFinish::AmbiguousUsage
            )],
            "a truncated provider request is never cleanly releasable and bills nothing"
        );
    }

    #[tokio::test]
    async fn closed_receiver_before_permit_records_no_delivery() {
        let events = EventRecorder::default();
        let (up_tx, up_rx) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel::<Result<CanonChunk, Status>>(1);
        drop(rx);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                text: "not accepted".into(),
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, events.clone(), None).await;
        assert_eq!(
            events.events(),
            vec![AuthoritativeDeliveryEvent::Finished(
                AuthoritativeDeliveryFinish::Interrupted
            )],
            "buyer disconnect is distinct from provider truncation and records no delivery"
        );
    }

    #[tokio::test]
    async fn separate_usage_after_buyer_disconnect_never_records_delivery() {
        let events = EventRecorder::default();
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, rx) = mpsc::channel::<Result<CanonChunk, Status>>(1);
        drop(rx);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                text: "not accepted".into(),
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        up_tx
            .send(Ok(UpstreamEvent::Usage(
                BillingUsage::new(1, 1, None).unwrap(),
            )))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, events.clone(), None).await;
        assert_eq!(
            events.events(),
            vec![AuthoritativeDeliveryEvent::Finished(
                AuthoritativeDeliveryFinish::Interrupted
            )],
            "terminal usage cannot monetize output the buyer channel rejected"
        );
    }

    /// E2E-UPS-01/02 through the whole relay: a provider that returns no optional log-probability data
    /// still serves the buyer, and its terminal native total is the exact delivered amount.
    #[tokio::test]
    async fn openai_terminal_usage_authorizes_text_without_logprobs_through_relay() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"forwarded\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5},",
            "\"x_groq\":{\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,",
            "\"total_tokens\":5}}}\n\n",
            "data: [DONE]\n\n"
        )
        .to_string();
        let (count, events, buyer_events, provider_request) =
            run_openai_through_relay(body, "DEXDO_861_RELAY_NATIVE_USAGE_KEY").await;

        assert!(provider_request.starts_with("POST "));
        assert!(
            provider_request.contains("\"include_usage\":true"),
            "the seller must ask for the record it bills by: {provider_request}"
        );
        assert_eq!(buyer_events.len(), 2);
        assert_eq!(buyer_events[0].as_ref().unwrap().text, "forwarded");
        assert_eq!(
            buyer_events[1]
                .as_ref()
                .unwrap()
                .usage
                .as_ref()
                .unwrap()
                .total_tokens,
            5
        );
        assert_eq!(count, 5, "prompt plus completion is the delivered amount");
        assert_eq!(
            events,
            vec![AuthoritativeDeliveryEvent::Completed(
                NonZeroU64::new(5).unwrap()
            )]
        );
    }

    /// A corrupt complete provider frame kills the request before it can reach a terminal aggregate, so
    /// nothing is delivered and nothing is billable (E2E-UPS-07/18).
    #[tokio::test]
    async fn malformed_complete_openai_sse_is_data_loss_and_bills_nothing() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"forwarded\"},",
            "\"logprobs\":{\"content\":[{\"token\":\"forwarded\",\"logprob\":-0.1,",
            "\"top_logprobs\":[]}]}}]}\n\n",
            "data: {\"choices\":[\n\n",
            "data: {\"choices\":[],\"usage\":{\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        )
        .to_string();
        let (count, events, buyer_events, provider_request) =
            run_openai_through_relay(body, "DEXDO_768_MALFORMED_SSE_KEY").await;

        assert!(provider_request.starts_with("POST "));
        assert_eq!(buyer_events.len(), 2);
        assert_eq!(buyer_events[0].as_ref().unwrap().text, "forwarded");
        let status = buyer_events[1].as_ref().unwrap_err();
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(
            status
                .message()
                .starts_with("malformed OpenAI-compatible SSE JSON:"),
            "{}",
            status.message()
        );
        assert_eq!(
            count, 0,
            "no terminal aggregate was ever reached, so no amount is authorized"
        );
        assert_eq!(
            events,
            vec![AuthoritativeDeliveryEvent::Finished(
                AuthoritativeDeliveryFinish::AmbiguousUsage
            )],
            "a corrupt complete provider frame is never a clean request"
        );
    }

    #[tokio::test]
    async fn separate_usage_missing_after_forward_remains_ambiguous() {
        let events = EventRecorder::default();
        let (up_tx, up_rx) = mpsc::channel(1);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                text: "forwarded".into(),
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, events.clone(), None).await;
        assert_eq!(rx.recv().await.unwrap().unwrap().text, "forwarded");
        assert_eq!(
            rx.recv().await.unwrap().unwrap_err().code(),
            tonic::Code::DataLoss
        );
        assert_eq!(
            events.events(),
            vec![AuthoritativeDeliveryEvent::Finished(
                AuthoritativeDeliveryFinish::AmbiguousUsage
            )]
        );
    }

    #[tokio::test]
    async fn request_recorder_marks_output_then_error_ambiguous() {
        let events = EventRecorder::default();
        let (up_tx, up_rx) = mpsc::channel(2);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(2);
        up_tx
            .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                token_ids: vec![1],
                ..CanonChunk::default()
            })))
            .await
            .unwrap();
        up_tx
            .send(Err(Status::unavailable("provider stopped")))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting(up_rx, tx, events.clone(), None).await;
        while rx.recv().await.is_some() {}
        assert_eq!(
            events.events(),
            vec![AuthoritativeDeliveryEvent::Finished(
                AuthoritativeDeliveryFinish::AmbiguousUsage
            )]
        );
    }

    #[tokio::test]
    async fn relay_accepts_total_equal_to_grant_and_caps_grant_plus_one() {
        for (input_tokens, exhausted) in [(3, false), (4, true)] {
            let recorder = RecordingDelivery::default();
            let (up_tx, up_rx) = mpsc::channel(2);
            let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(3);
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    text: "one".into(),
                    ..CanonChunk::default()
                })))
                .await
                .unwrap();
            up_tx
                .send(Ok(UpstreamEvent::Usage(
                    BillingUsage::new(input_tokens, 1, None).unwrap(),
                )))
                .await
                .unwrap();
            drop(up_tx);

            relay_counting_with_chain_availability(
                up_rx,
                tx,
                recorder.clone(),
                None,
                None,
                1,
                4,
                false,
            )
            .await;
            assert_eq!(rx.recv().await.unwrap().unwrap().text, "one");
            let usage = rx.recv().await.unwrap().unwrap().usage.unwrap();
            assert_eq!(usage.input_tokens, input_tokens);
            assert_eq!(usage.output_tokens, 1);
            assert_eq!(usage.total_tokens, input_tokens + 1);
            assert_eq!(usage.billable_tokens(), 4);
            assert_eq!(
                recorder.events.events(),
                vec![AuthoritativeDeliveryEvent::Completed(
                    NonZeroU64::new(4).unwrap()
                )]
            );
            assert_eq!(
                recorder.delivery.billing_exhausted.load(Ordering::Acquire),
                exhausted
            );
            assert_eq!(recorder.delivery.done.load(Ordering::Acquire), exhausted);
        }
    }

    /// text-only SSE fragments are not token units, and provider billing output can
    /// include hidden reasoning above the visible-output generation limit.
    #[tokio::test]
    async fn fragmented_real_output_accepts_billing_above_the_visible_output_limit() {
        for output_tokens in [64, 65] {
            let recorder = RecordingDelivery::default();
            let (up_tx, up_rx) = mpsc::channel(66);
            let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(67);
            for seq in 0..65 {
                up_tx
                    .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                        text: "x".into(),
                        seq,
                        ..CanonChunk::default()
                    })))
                    .await
                    .unwrap();
            }
            up_tx
                .send(Ok(UpstreamEvent::Usage(
                    BillingUsage::new(1, output_tokens, None).unwrap(),
                )))
                .await
                .unwrap();
            drop(up_tx);

            relay_counting_with_chain_availability(
                up_rx,
                tx,
                recorder.clone(),
                None,
                None,
                64,
                66,
                false,
            )
            .await;

            let mut content_chunks = 0;
            let mut terminal = None;
            let mut error = None;
            while let Some(event) = rx.recv().await {
                match event {
                    Ok(chunk) if chunk.usage.is_some() => terminal = chunk.usage,
                    Ok(_) => content_chunks += 1,
                    Err(status) => error = Some(status),
                }
            }
            assert_eq!(content_chunks, 65, "all transport fragments are forwarded");
            assert_eq!(terminal.unwrap().output_tokens, output_tokens);
            assert!(error.is_none());
            assert_eq!(
                recorder.events.events(),
                vec![AuthoritativeDeliveryEvent::Completed(
                    NonZeroU64::new(output_tokens + 1).unwrap()
                )]
            );
            assert!(
                !recorder.delivery.done.load(Ordering::Acquire),
                "billing above the visible-output cap alone must not close the deal"
            );
            assert!(
                !recorder.delivery.billing_exhausted.load(Ordering::Acquire),
                "only billing beyond the reservation marks exhaustion"
            );
        }
    }

    #[tokio::test]
    async fn over_grant_terminal_before_output_releases_the_capacity_reservation() {
        const GRANT: u64 = 8;

        let directory = tempfile::tempdir().unwrap();
        let state = GatewayState::with_upstream_and_deals_dir(
            UpstreamConfig::Mock,
            directory.path().to_path_buf(),
        );
        let tc = "0:pre-output-invalid-usage".to_string();
        state
            .register_stream(
                &tc,
                buyer_pubkey(),
                u64::MAX,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + u128::from(GRANT)),
            )
            .unwrap();
        let delivery = state.delivery(&tc);
        let recorder = CapacityDeliveryRecorder {
            reservation: state.capacity.reserve(&tc, GRANT).unwrap(),
            delivery: delivery.clone(),
        };
        let (up_tx, up_rx) = mpsc::channel(1);
        let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(1);
        up_tx
            .send(Ok(UpstreamEvent::Usage(
                BillingUsage::new(GRANT, 1, None).unwrap(),
            )))
            .await
            .unwrap();
        drop(up_tx);

        relay_counting_with_chain_availability(
            up_rx, tx, recorder, None, None, GRANT, GRANT, false,
        )
        .await;

        assert_eq!(
            rx.recv().await.unwrap().unwrap_err().code(),
            tonic::Code::DataLoss
        );
        assert!(rx.recv().await.is_none());
        let snapshot = state.capacity_snapshot(&tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 0);
        assert_eq!(snapshot.outstanding_reservation, 0);
        assert_eq!(snapshot.available().unwrap(), u128::from(GRANT));
        assert_eq!(delivery.count.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn repeated_or_skipped_content_sequence_never_commits_terminal_usage() {
        for bad_seq in [0, 2] {
            let events = EventRecorder::default();
            let (up_tx, up_rx) = mpsc::channel(3);
            let (tx, mut rx) = mpsc::channel::<Result<CanonChunk, Status>>(3);
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    text: "first".into(),
                    ..CanonChunk::default()
                })))
                .await
                .unwrap();
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    text: "bad".into(),
                    seq: bad_seq,
                    ..CanonChunk::default()
                })))
                .await
                .unwrap();
            up_tx
                .send(Ok(UpstreamEvent::Usage(
                    BillingUsage::new(1, 2, None).unwrap(),
                )))
                .await
                .unwrap();
            drop(up_tx);

            relay_counting(up_rx, tx, events.clone(), None).await;
            assert_eq!(rx.recv().await.unwrap().unwrap().text, "first");
            assert_eq!(
                rx.recv().await.unwrap().unwrap_err().code(),
                tonic::Code::DataLoss
            );
            assert!(!events
                .events()
                .iter()
                .any(|event| matches!(event, AuthoritativeDeliveryEvent::Completed(_))));
        }
    }
    #[tokio::test]
    async fn shared_budget_2091_preserves_full_usage_and_a_zero_charge_answer() {
        let state = GatewayState::new();
        let tc = "0:shared-zero-charge";
        state
            .register_stream(
                tc,
                buyer_pubkey(),
                u64::MAX,
                subscription_state(TICK_SIZE),
                ordinary_shape(TICK_SIZE + 2),
            )
            .unwrap();
        let first = state
            .capacity
            .admit_shared(
                &tc.to_string(),
                2,
                dexdo_core::params::SELLER_MAX_CONCURRENT_REQUESTS_PER_DEAL,
            )
            .unwrap();
        let second = state
            .capacity
            .admit_shared(
                &tc.to_string(),
                2,
                dexdo_core::params::SELLER_MAX_CONCURRENT_REQUESTS_PER_DEAL,
            )
            .unwrap();
        for (index, reservation) in [first, second].into_iter().enumerate() {
            let (up_tx, up_rx) = mpsc::channel(2);
            let (tx, mut rx) = mpsc::channel(2);
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    text: "full answer".into(),
                    ..Default::default()
                })))
                .await
                .unwrap();
            up_tx
                .send(Ok(UpstreamEvent::Usage(
                    BillingUsage::new(2, 1, None).unwrap(),
                )))
                .await
                .unwrap();
            drop(up_tx);
            relay_counting_with_chain_availability(
                up_rx,
                tx,
                CapacityDeliveryRecorder {
                    reservation,
                    delivery: state.delivery(tc),
                },
                None,
                None,
                2,
                2,
                false,
            )
            .await;
            assert_eq!(rx.recv().await.unwrap().unwrap().text, "full answer");
            let usage = rx.recv().await.unwrap().unwrap().usage.unwrap();
            assert_eq!(usage.total_tokens, 3);
            assert_eq!(usage.billable_tokens(), if index == 0 { 2 } else { 0 });
            assert!(rx.recv().await.is_none());
            assert_eq!(state.delivery(tc).done.load(Ordering::Acquire), index == 1);
        }
        assert_eq!(state.delivery(tc).count.load(Ordering::Acquire), 2);
        assert_eq!(state.active_requests(tc), 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_budget_2091_overrun_closes_admission_before_draining() {
        for finish in [
            AuthoritativeDeliveryEvent::Completed(NonZeroU64::MIN),
            AuthoritativeDeliveryEvent::Finished(AuthoritativeDeliveryFinish::Interrupted),
            AuthoritativeDeliveryEvent::Finished(AuthoritativeDeliveryFinish::AmbiguousUsage),
        ] {
            let expected_charge = if matches!(finish, AuthoritativeDeliveryEvent::Completed(_)) {
                2
            } else {
                1
            };
            let state = Arc::new(GatewayState::new());
            let buyer = LocalNote::generate();
            let tc = "0:shared-overrun-drain";
            state
                .register_stream(
                    tc,
                    buyer.pubkey(),
                    u64::MAX,
                    subscription_state(TICK_SIZE),
                    ordinary_shape(4 * TICK_SIZE),
                )
                .unwrap();
            let limit = dexdo_core::params::SELLER_MAX_CONCURRENT_REQUESTS_PER_DEAL;
            let first = state.capacity.admit_shared(&tc.into(), 1, limit).unwrap();
            let second = state.capacity.admit_shared(&tc.into(), 16, limit).unwrap();
            let delivery = state.delivery(tc);
            let (up_tx, up_rx) = mpsc::channel(2);
            let (tx, mut rx) = mpsc::channel(2);
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    text: "full answer".into(),
                    ..Default::default()
                })))
                .await
                .unwrap();
            up_tx
                .send(Ok(UpstreamEvent::Usage(
                    BillingUsage::new(1, 1, None).unwrap(),
                )))
                .await
                .unwrap();
            drop(up_tx);
            relay_counting_with_chain_availability(
                up_rx,
                tx,
                CapacityDeliveryRecorder {
                    reservation: first,
                    delivery: delivery.clone(),
                },
                None,
                None,
                1,
                1,
                false,
            )
            .await;
            assert_eq!(rx.recv().await.unwrap().unwrap().text, "full answer");
            assert_eq!(
                rx.recv()
                    .await
                    .unwrap()
                    .unwrap()
                    .usage
                    .unwrap()
                    .charged_tokens,
                Some(1)
            );
            assert!(delivery.billing_exhausted.load(Ordering::Acquire));
            assert!(!delivery.done.load(Ordering::Acquire));
            assert_eq!(state.active_requests(tc), 1);
            assert!(
                state
                    .capacity_snapshot(tc)
                    .unwrap()
                    .unwrap()
                    .available()
                    .unwrap()
                    > 0
            );

            let service = GatewayService::new(state.clone());
            // A slot and paid capacity remain, but neither protocol may enter a draining deal.
            for shared in [true, false] {
                let mut request = authorized_request(&state, &buyer, tc, 1, 100).into_inner();
                request.shared_budget = shared;
                let error = service
                    .open_stream(Request::new(request))
                    .await
                    .err()
                    .expect("an overrun must stop new work before draining accepted work");
                assert_eq!(error.code(), tonic::Code::ResourceExhausted);
            }
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let finish_barrier = barrier.clone();
            let finish_delivery = delivery.clone();
            let task = tokio::spawn(async move {
                finish_barrier.wait().await;
                CapacityDeliveryRecorder {
                    reservation: second,
                    delivery: finish_delivery,
                }
                .record_authoritative_delivery(finish)
                .unwrap();
            });
            let mut request = authorized_request(&state, &buyer, tc, 1, 100).into_inner();
            request.shared_budget = true;
            barrier.wait().await;
            assert_eq!(
                service
                    .open_stream(Request::new(request))
                    .await
                    .err()
                    .unwrap()
                    .code(),
                tonic::Code::ResourceExhausted
            );
            task.await.unwrap();
            assert!(delivery.done.load(Ordering::Acquire));
            assert_eq!(state.active_requests(tc), 0);
            assert_eq!(delivery.count.load(Ordering::Acquire), expected_charge);
            // A normal terminal, a cancellation and ambiguous usage must all preserve the closure.
            for shared in [true, false] {
                let mut request = authorized_request(&state, &buyer, tc, 1, 100).into_inner();
                request.shared_budget = shared;
                assert_eq!(
                    service
                        .open_stream(Request::new(request))
                        .await
                        .err()
                        .unwrap()
                        .code(),
                    tonic::Code::ResourceExhausted
                );
            }
            assert!(
                state
                    .capacity_snapshot(tc)
                    .unwrap()
                    .unwrap()
                    .available()
                    .unwrap()
                    > 0
            );
        }
    }
    #[tokio::test]
    async fn shared_budget_2091_gateway_enforces_configuration_and_legacy_cannot_bypass_it() {
        for count in [1_u32, 4] {
            let state = Arc::new(GatewayState::new());
            let buyer = LocalNote::generate();
            let limit = std::num::NonZeroU32::new(count).unwrap();
            state.set_max_concurrent_requests_per_deal(limit);
            let tc = "0:shared-limit";
            state
                .register_stream(
                    tc,
                    buyer.pubkey(),
                    u64::MAX,
                    subscription_state(TICK_SIZE),
                    ordinary_shape(2 * TICK_SIZE),
                )
                .unwrap();
            let service = GatewayService::new(state.clone());
            // Keep each real gateway response open beyond its bounded channel capacity. The
            // terminal cannot commit until we read, so all configured slots stay occupied.
            let output = u32::try_from(GATEWAY_CLIENT_CHANNEL_CAPACITY + 1).unwrap();
            let mut requests = Vec::new();
            for _ in 0..count {
                let mut request = authorized_request(&state, &buyer, tc, output, 100).into_inner();
                request.shared_budget = true;
                requests.push(
                    service
                        .open_stream(Request::new(request))
                        .await
                        .unwrap()
                        .into_inner(),
                );
            }
            assert_eq!(state.active_requests(tc), count);
            for shared in [true, false] {
                let mut request = authorized_request(&state, &buyer, tc, 1, 100).into_inner();
                request.shared_budget = shared;
                let error = service
                    .open_stream(Request::new(request))
                    .await
                    .err()
                    .expect("full deal must reject before upstream");
                assert_eq!(error.code(), tonic::Code::ResourceExhausted);
            }
            for mut request in requests {
                let mut terminal = None;
                while let Some(chunk) = request.next().await {
                    terminal = chunk.unwrap().usage.or(terminal);
                }
                assert_eq!(terminal.unwrap().billable_tokens(), u64::from(output));
            }
            assert_eq!(state.active_requests(tc), 0);
            let legacy = state
                .capacity
                .reserve_exclusive(&tc.to_string(), 100)
                .unwrap();
            let mut request = authorized_request(&state, &buyer, tc, 1, 100).into_inner();
            request.shared_budget = true;
            assert_eq!(
                service
                    .open_stream(Request::new(request))
                    .await
                    .err()
                    .unwrap()
                    .code(),
                tonic::Code::ResourceExhausted
            );
            assert!(state
                .capacity
                .reserve_exclusive(&tc.to_string(), 100)
                .is_err());
            legacy.finish_exact().unwrap();
        }
    }
    #[tokio::test]
    async fn shared_budget_2155_gateway_reuses_the_deal_after_repeated_interruptions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for failure in [
            "before_output",
            "provider_error",
            "missing_usage",
            "disconnect",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let release = Arc::new(tokio::sync::Notify::new());
            let provider_release = release.clone();
            let provider = tokio::spawn(async move {
                for attempt in 0..6 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    loop {
                        let read = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "provider request must be complete");
                        request.extend_from_slice(&buffer[..read]);
                        let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    let clean = attempt == 0 || attempt >= 4;
                    let prefix = if !clean && failure == "before_output" {
                        ""
                    } else {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n"
                    };
                    let suffix = if clean {
                        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\ndata: [DONE]\n\n"
                    } else if failure == "provider_error" {
                        "data: {\"error\":{\"message\":\"scripted provider failure\",\"type\":\"server_error\"}}\n\n"
                    } else if failure == "disconnect" {
                        // The relay still observes the upstream after the receiver disappears.
                        // Charging this cut-off request remains outside this PR.
                        "data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}\n\ndata: [DONE]\n\n"
                    } else {
                        "data: [DONE]\n\n"
                    };
                    let header = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", prefix.len() + suffix.len());
                    socket.write_all(header.as_bytes()).await.unwrap();
                    socket.write_all(prefix.as_bytes()).await.unwrap();
                    if !clean && failure == "disconnect" {
                        provider_release.notified().await;
                    }
                    socket.write_all(suffix.as_bytes()).await.unwrap();
                }
            });
            let state = Arc::new(GatewayState::with_upstream(UpstreamConfig::OpenAi(
                crate::seller::OpenAiConfig {
                    base_url: format!("http://{address}"),
                    api_key_env: "PATH".into(),
                    ..Default::default()
                },
            )));
            state.set_max_concurrent_requests_per_deal(std::num::NonZeroU32::MIN);
            let buyer = LocalNote::generate();
            let tc = "0:shared-2155-gateway";
            state
                .register_stream(
                    tc,
                    buyer.pubkey(),
                    u64::MAX,
                    subscription_state(TICK_SIZE),
                    ordinary_shape(TICK_SIZE + 9),
                )
                .unwrap();
            let service = GatewayService::new(state.clone());
            let mut charged = 0;
            for attempt in 0..6 {
                let clean = attempt == 0 || attempt >= 4;
                let mut request = authorized_request(&state, &buyer, tc, 3, 100).into_inner();
                request.shared_budget = true;
                let mut response = service
                    .open_stream(Request::new(request))
                    .await
                    .unwrap()
                    .into_inner();
                if !clean && failure == "disconnect" {
                    assert_eq!(response.next().await.unwrap().unwrap().text, "answer");
                    drop(response);
                    assert_eq!(state.active_requests(tc), 1);
                    let mut blocked = authorized_request(&state, &buyer, tc, 3, 100).into_inner();
                    blocked.shared_budget = true;
                    assert_eq!(
                        service
                            .open_stream(Request::new(blocked))
                            .await
                            .err()
                            .unwrap()
                            .code(),
                        tonic::Code::ResourceExhausted
                    );
                    release.notify_one();
                } else {
                    let mut terminal = None;
                    let mut error = false;
                    while let Some(chunk) = response.next().await {
                        match chunk {
                            Ok(chunk) => {
                                terminal = chunk.usage.or(terminal);
                            }
                            Err(_) => {
                                error = true;
                            }
                        }
                    }
                    if clean {
                        assert!(!error, "{failure}: clean request failed");
                        assert_eq!(terminal.unwrap().billable_tokens(), 3);
                        charged += 3;
                    } else {
                        assert!(error, "{failure}: interrupted request must report failure");
                        assert!(terminal.is_none());
                    }
                }
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while state.active_requests(tc) != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("upstream drain must release the live slot");
                let snapshot = state.capacity_snapshot(tc).unwrap().unwrap();
                assert_eq!(snapshot.local_delivered_after_anchor, charged);
                assert_eq!(snapshot.outstanding_reservation, 0);
                assert_eq!(snapshot.available().unwrap(), 9 - charged);
                assert_eq!(
                    u128::from(state.delivery(tc).count.load(Ordering::Acquire)),
                    charged
                );
                assert!(!state.delivery(tc).done.load(Ordering::Acquire));
            }
            let mut exhausted = authorized_request(&state, &buyer, tc, 1, 100).into_inner();
            exhausted.shared_budget = true;
            assert_eq!(
                service
                    .open_stream(Request::new(exhausted))
                    .await
                    .err()
                    .unwrap()
                    .code(),
                tonic::Code::ResourceExhausted
            );
            provider.await.unwrap();
        }
    }

    #[tokio::test]
    async fn shared_budget_2091_disconnect_holds_the_slot_until_upstream_really_ends() {
        let state = Arc::new(GatewayState::new());
        let tc = "0:shared-drain".to_string();
        state
            .register_stream(
                &tc,
                buyer_pubkey(),
                u64::MAX,
                subscription_state(TICK_SIZE),
                ordinary_shape(2 * TICK_SIZE),
            )
            .unwrap();
        let one = std::num::NonZeroU32::MIN;
        let reservation = state.capacity.admit_shared(&tc, 100, one).unwrap();
        let (up_tx, up_rx) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let delivery = state.delivery(&tc);
        let relay = tokio::spawn(relay_counting_with_chain_availability(
            up_rx,
            tx,
            CapacityDeliveryRecorder {
                reservation,
                delivery,
            },
            None,
            None,
            100,
            100,
            false,
        ));
        // With a one-event channel, all three sends can finish only if the relay keeps
        // consuming the real upstream after discovering that the client disappeared.
        for seq in 0..3 {
            up_tx
                .send(Ok(UpstreamEvent::Chunk(CanonChunk {
                    text: "provider work".into(),
                    seq,
                    ..Default::default()
                })))
                .await
                .expect("upstream must remain observed through completion");
        }
        assert_eq!(state.active_requests(&tc), 1);
        assert!(matches!(
            state.capacity.admit_shared(&tc, 100, one),
            Err(ReserveError::Concurrency)
        ));
        drop(up_tx);
        relay.await.unwrap();
        assert_eq!(state.active_requests(&tc), 0);
        state
            .capacity
            .admit_shared(&tc, 100, one)
            .unwrap()
            .finish_exact()
            .unwrap();
    }
}

#[cfg(test)]
#[path = "gateway_1046_tests.rs"]
mod issue_1046_tests;

#[cfg(test)]
#[path = "gateway_1196_tests.rs"]
mod issue_1196_tests;
