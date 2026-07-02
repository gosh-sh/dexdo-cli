//! Local consumer interface: an OpenAI-compatible HTTP endpoint
//! (`/v1/chat/completions`, `/v1/models`) and an optional Anthropic-compatible transcode
//! (`/v1/messages`). The endpoint listens on **loopback** by default.
//! Request path(B19): receive -> build `CanonRequest` -> route to the(mock) seller ->
//! authorized TLS gRPC stream -> receive `CanonChunk` -> re-render to SSE in the desired format.
//! Tick accounting/verification happen on the canonical stream BEFORE re-rendering
//! ([`crate::buyer::verify::StreamVerifier`]).
//! The model is forced by the market/frame(B2, B19): the request's `model` field is NOT trusted;
//! a request outside the configured model frame is rejected. Any API key is accepted: this is a
//! loopback endpoint.

pub mod anthropic;
pub mod openai;
mod stream;

use crate::buyer::verify::Verdict;
use crate::buyer::Buyer;
use anyhow::Result;
use dexdo_core::{ChainBackend, Handover, Note, TokenContract};
use dexdo_proto::CanonChunk;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationBailAction {
    Stop,
    Dispute,
    StopAndBlacklist,
}

impl VerificationBailAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Dispute => "dispute",
            Self::StopAndBlacklist => "stop_and_blacklist",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadGatewayAction {
    RetryThenReclaim,
    NextSeller,
    FailClosed,
}

impl DeadGatewayAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::RetryThenReclaim => "retry_then_reclaim",
            Self::NextSeller => "next_seller",
            Self::FailClosed => "fail_closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyStreamAction {
    Reclaim,
    NextSeller,
    FailClosed,
}

impl EmptyStreamAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reclaim => "reclaim",
            Self::NextSeller => "next_seller",
            Self::FailClosed => "fail_closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellerStallsMidStreamAction {
    AcceptDeliveredThenReclaim,
    Dispute,
}

impl SellerStallsMidStreamAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::AcceptDeliveredThenReclaim => "accept_delivered_then_reclaim",
            Self::Dispute => "dispute",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyerApiFailurePolicy {
    pub verification_bail: VerificationBailAction,
    pub dead_gateway: DeadGatewayAction,
    pub empty_stream: EmptyStreamAction,
    pub seller_stalls_mid_stream: SellerStallsMidStreamAction,
}

impl Default for BuyerApiFailurePolicy {
    fn default() -> Self {
        Self {
            verification_bail: VerificationBailAction::Stop,
            dead_gateway: DeadGatewayAction::RetryThenReclaim,
            empty_stream: EmptyStreamAction::Reclaim,
            seller_stalls_mid_stream: SellerStallsMidStreamAction::AcceptDeliveredThenReclaim,
        }
    }
}

/// Route to the(mock) seller + model frame, shared by the HTTP handlers(B1/B2/B19).
/// In "routing" is a single fixed match(one seller, mock chain); semantic orders
/// and seller selection are the horizon of.
#[derive(Clone)]
pub struct Route {
    pub handover: Handover,
    pub token_contract: TokenContract,
    /// Deal/session token budget. Per-request `max_tokens` is honored by the handlers but cannot exceed this.
    pub max_tokens: u64,
}

/// One currently usable consumer-API deal: route, settlement terminal, and one-per-deal content gate.
#[derive(Clone)]
pub struct ApiDeal {
    pub route: Route,
    pub session: Arc<SessionSettle>,
    pub content_gate: Arc<ContentGate>,
    delivered_tokens: Arc<AtomicU64>,
    active_requests: Arc<AtomicU64>,
    last_request_started_unix_secs: Arc<AtomicU64>,
}

impl ApiDeal {
    pub fn new(route: Route, session: Arc<SessionSettle>, content_gate: Arc<ContentGate>) -> Self {
        Self {
            route,
            session,
            content_gate,
            delivered_tokens: Arc::new(AtomicU64::new(0)),
            active_requests: Arc::new(AtomicU64::new(0)),
            last_request_started_unix_secs: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn delivered_tokens(&self) -> u64 {
        self.delivered_tokens.load(Ordering::SeqCst)
    }

    pub fn remaining_tokens(&self) -> u64 {
        self.route
            .max_tokens
            .saturating_sub(self.delivered_tokens())
    }

    pub fn record_delivered(&self, n: u64) {
        self.delivered_tokens.fetch_add(n, Ordering::SeqCst);
    }

    pub(crate) fn begin_request(&self, now_secs: u64) -> ConsumerRequestGuard {
        self.last_request_started_unix_secs
            .store(now_secs, Ordering::SeqCst);
        self.active_requests.fetch_add(1, Ordering::SeqCst);
        ConsumerRequestGuard {
            active_requests: self.active_requests.clone(),
        }
    }

    pub fn has_active_or_recent_request(&self, now_secs: u64, recent_window_secs: u64) -> bool {
        if self.active_requests.load(Ordering::SeqCst) > 0 {
            return true;
        }
        let last = self.last_request_started_unix_secs.load(Ordering::SeqCst);
        last != 0 && now_secs.saturating_sub(last) <= recent_window_secs
    }
}

pub(crate) struct ConsumerRequestGuard {
    active_requests: Arc<AtomicU64>,
}

impl Drop for ConsumerRequestGuard {
    fn drop(&mut self) {
        self.active_requests.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Mutable active deal for a long-running local API service.
/// Single-shot/legacy callers build one deal and never replace it. The buyer continuity monitor can prepare a
/// next handover first, atomically publish that next deal, then STOP the old session. That keeps the local
/// OpenAI/Anthropic endpoint alive across deal boundaries without serving a request on a closed TC.
pub struct RouteManager {
    active: RwLock<ApiDeal>,
}

impl RouteManager {
    pub fn new(active: ApiDeal) -> Self {
        Self {
            active: RwLock::new(active),
        }
    }

    pub async fn current(&self) -> ApiDeal {
        self.active.read().await.clone()
    }

    pub async fn replace_active(&self, next: ApiDeal, reason: &str) {
        let previous = {
            let mut active = self.active.write().await;
            std::mem::replace(&mut *active, next)
        };
        previous.session.settle(reason).await;
    }

    pub async fn settle_active(&self, reason: &str) -> bool {
        let active = self.current().await;
        active.session.settle(reason).await
    }
}

/// Canonical delivered-token count for a normalized chunk. Prefer structured token signals; a non-empty chunk
/// with no token-level metadata still counts as one delivered token.
pub(crate) fn accounted_tokens(chunk: &CanonChunk) -> u64 {
    let token_ids = chunk.token_ids.len() as u64;
    let logprobs = chunk.logprobs.len() as u64;
    token_ids.max(logprobs).max(1)
}

/// Consumer request limit, bounded by the already-purchased deal budget. `None`/`0` means "use the budget".
pub(crate) fn request_token_limit(requested: Option<u32>, budget: u64) -> u64 {
    requested
        .map(u64::from)
        .filter(|n| *n > 0)
        .unwrap_or(budget)
        .min(budget)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamErrorPolicyAction {
    RequestScoped,
    DeadGateway,
    SellerStallsMidStream,
}

pub(crate) fn stream_error_policy_action(error: &str, received: u64) -> StreamErrorPolicyAction {
    if received == 0 && is_request_scoped_upstream_rejection(error) {
        StreamErrorPolicyAction::RequestScoped
    } else if received == 0 {
        StreamErrorPolicyAction::DeadGateway
    } else {
        StreamErrorPolicyAction::SellerStallsMidStream
    }
}

pub(crate) async fn handle_stream_error_policy(
    deal: &ApiDeal,
    received: u64,
    error: &str,
) -> StreamErrorPolicyAction {
    let action = stream_error_policy_action(error, received);
    match action {
        StreamErrorPolicyAction::RequestScoped => {}
        StreamErrorPolicyAction::DeadGateway => {
            deal.session
                .settle_dead_gateway("stream-error-before-token")
                .await;
        }
        StreamErrorPolicyAction::SellerStallsMidStream => {
            deal.session
                .settle_seller_stalls_mid_stream("seller-stalls-mid-stream")
                .await;
        }
    }
    action
}

fn is_request_scoped_upstream_rejection(error: &str) -> bool {
    error
        .split("upstream HTTP ")
        .skip(1)
        .any(|rest| rest.as_bytes().first() == Some(&b'4'))
}

/// Token cap for the one-per-deal content-identity probe. It is **<< `tick_size`**(1_000_000), so the
/// probe stays on the probe tick -- preserving the two-tick exposure invariant (the content gate spends at
/// most the probe tick, never a second deal tick worth of budget).
pub(crate) const CONTENT_PROBE_MAX_TOKENS: u64 = 64;

/// Content-identity check selected for a deal. The buyer pays for a model by NAME(B2); a seller
/// declaring the correct name but serving a cheaper model is caught only by the **content** layers B8
/// ([`Buyer::behavioral_probe`]) + B7-full([`Buyer::reference_spotcheck`]). `Skip` -- no content
/// fingerprint/reference for the exact model id(degradation R3, name-only) or the mock path; `Probe` -- run the
/// content gate once for that exact/reference model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentCheck {
    /// No content-identity layer applies(mock, or no B8 fingerprint and no B7 reference/key) -- name-only.
    Skip,
    /// Run the one-per-deal content gate(B8 + B7-full) for this exact/reference model id.
    Probe { model_id: String },
}

/// One-per-deal content-identity gate. The inline [`StreamVerifier`](crate::buyer::verify::StreamVerifier)
/// on the consumer-API path only runs B5/B6 + the cheap declared-NAME B7; the strong **content** layers (B8
/// fingerprint + B7-full reference spot-check) were never invoked there, so a seller serving a cheaper model
/// under the correct NAME was paid undetected. This gate runs those layers ONCE, before the first paid stream on
/// each renderer, and caches the **definitive** verdict so later requests do not re-probe. A transport error is
/// NOT cached(the next request retries). On a bail the deal is closed to later requests and policy recovery is
/// attempted.
pub struct ContentGate {
    check: ContentCheck,
    /// Cached definitive verdict: `Ok(())` pass, `Err(reason)` bail. A transport error is the cell's init error
    /// (NOT stored) so the gate retries on the next request.
    verdict: OnceCell<Result<(), String>>,
}

impl ContentGate {
    pub fn new(check: ContentCheck) -> Self {
        Self {
            check,
            verdict: OnceCell::new(),
        }
    }

    /// A gate that performs no content check(mock / name-only degradation).
    pub fn skip() -> Self {
        Self::new(ContentCheck::Skip)
    }

    /// A gate that runs the content probe for `model_id`.
    pub fn probe(model_id: String) -> Self {
        Self::new(ContentCheck::Probe { model_id })
    }

    /// Run the content-identity gate once per deal. `Skip` -> `Ok(())`. `Probe` -> run B8 then B7-full
    /// ONCE(cached): the cached `Ok(())`/`Err(reason)` is the definitive verdict(pass/bail); a transport error
    /// is propagated as `Err` WITHOUT being cached, so the next request retries. On a bail the deal is closed
    /// to new requests before the verdict is cached and returned.
    pub async fn ensure_verified(
        &self,
        buyer: &Buyer,
        route: &Route,
        session: &SessionSettle,
    ) -> Result<(), String> {
        match &self.check {
            ContentCheck::Skip => Ok(()),
            ContentCheck::Probe { model_id } => {
                // OUTER Err = transport error(NOT cached -> retried next request); INNER `Result<(), String>` =
                // the cached definitive verdict (`Ok(())` pass, `Err(reason)` bail).
                let cached: &Result<(), String> = self
                    .verdict
                    .get_or_try_init::<String, _, _>(|| async {
                        // B8 content fingerprint. The `?` makes a transport error the OUTER Err(not cached);
                        // a definitive verdict goes through `Ok(...)`.
                        let v8 = buyer
                            .behavioral_probe(
                                &route.handover,
                                &route.token_contract,
                                model_id,
                                CONTENT_PROBE_MAX_TOKENS,
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        if let Verdict::Bail(r) = v8 {
                            session
                                .settle_verification_bail("content-identity-bail")
                                .await;
                            return Ok(Err(r));
                        }
                        // B7-full reference spot-check(greedy vs the official endpoint).
                        let v7 = buyer
                            .reference_spotcheck(
                                &route.handover,
                                &route.token_contract,
                                model_id,
                                CONTENT_PROBE_MAX_TOKENS,
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        if let Verdict::Bail(r) = v7 {
                            session
                                .settle_verification_bail("content-identity-bail")
                                .await;
                            return Ok(Err(r));
                        }
                        Ok(Ok(()))
                    })
                    .await?;
                cached.clone()
            }
        }
    }
}

/// Consumer-interface state, shared across the HTTP handlers.
#[derive(Clone)]
pub struct ApiState {
    pub buyer: Arc<Buyer>,
    /// The configured market/frame model id -- the only one that is served(B2/B19).
    /// The request's `model` field is checked against it; outside the frame -- reject.
    pub frame_model: String,
    /// Active deal slot. A one-shot service never replaces it; continuous service mode may publish the next
    /// already-opened handover here while keeping the local HTTP listener alive.
    pub deals: Arc<RouteManager>,
}

/// Session-scoped deal settlement. The consumer endpoint serves
/// ONE deal(`route.token_contract`) across MANY requests; the deal is STOPped **once at session end**, not per
/// request. A single shared `Arc<SessionSettle>` lives on [`ApiState`]. The funds-safety guarantee is an
/// **awaited** STOP -- a verification-bail/dispute (`settle().await` in a handler) or graceful shutdown
/// (`serve()` awaits `settle("shutdown")`). [`Drop`] is ONLY a best-effort backup for abnormal teardown
/// (crash/SIGKILL), never the guarantee. `settled` is set only after a terminal recovery action lands; `closed`
/// gates the local API immediately after a policy incident even when recovery is still pending.
pub struct SessionSettle {
    chain: Arc<dyn ChainBackend>,
    token_contract: TokenContract,
    note: Arc<dyn Note>,
    settled: AtomicBool,
    closed: AtomicBool,
    settle_lock: Mutex<()>,
    failure_policy: BuyerApiFailurePolicy,
}

impl SessionSettle {
    /// From the deal's chain / `token_contract` / note. The session owns its own refs so it can settle (and
    /// Drop-backup) independently of any single request.
    pub fn new(
        chain: Arc<dyn ChainBackend>,
        token_contract: TokenContract,
        note: Arc<dyn Note>,
    ) -> Self {
        Self::new_with_verification_bail_action(
            chain,
            token_contract,
            note,
            VerificationBailAction::Stop,
        )
    }

    pub fn new_with_verification_bail_action(
        chain: Arc<dyn ChainBackend>,
        token_contract: TokenContract,
        note: Arc<dyn Note>,
        verification_bail_action: VerificationBailAction,
    ) -> Self {
        Self::new_with_failure_policy(
            chain,
            token_contract,
            note,
            BuyerApiFailurePolicy {
                verification_bail: verification_bail_action,
                ..BuyerApiFailurePolicy::default()
            },
        )
    }

    pub fn new_with_failure_policy(
        chain: Arc<dyn ChainBackend>,
        token_contract: TokenContract,
        note: Arc<dyn Note>,
        failure_policy: BuyerApiFailurePolicy,
    ) -> Self {
        Self {
            chain,
            token_contract,
            note,
            settled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            settle_lock: Mutex::new(()),
            failure_policy,
        }
    }

    pub fn dead_gateway_action(&self) -> DeadGatewayAction {
        self.failure_policy.dead_gateway
    }

    /// Whether a terminal on-chain action has landed for this deal.
    pub fn is_settled(&self) -> bool {
        self.settled.load(Ordering::SeqCst)
    }

    /// Whether the local API must reject new requests for this deal. This is distinct from terminal settlement:
    /// a policy failure closes serving immediately while leaving STOP-on-shutdown/retry recovery eligible.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn close_local_api(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Mark the session terminal after an external recovery path(`streamCleanup` / `streamReclaim`) already
    /// closed or reclaimed the deal. This prevents a later route swap from sending a duplicate STOP to the
    /// recovered TC.
    pub fn mark_recovered(&self, reason: &str) -> bool {
        if self.settled.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        tracing::info!(%reason, "consumer API: session deal marked recovered");
        true
    }

    /// STOP the deal once. `&self` -- the session is `Arc`-shared across the handlers. Returns whether THIS
    /// call landed a terminal STOP. A STOP error is logged, not panicked, and leaves the session recoverable.
    pub async fn settle(&self, reason: &str) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false; // already settled by an earlier bail / shutdown / Drop
        }
        self.close_local_api();
        match self
            .chain
            .stop(&self.token_contract, self.note.as_ref())
            .await
        {
            Ok(s) => {
                self.settled.store(true, Ordering::SeqCst);
                tracing::info!(%reason, settlement = ?s, "consumer API: session deal closed with STOP")
            }
            Err(e) => {
                tracing::warn!(
                    %reason,
                    error = %e,
                    "consumer API: session STOP/settlement failed; session remains recoverable"
                );
                return false;
            }
        }
        true
    }

    async fn policy_fail_closed(&self, failure_class: &str, action: &str, reason: &str) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        tracing::error!(
            %reason,
            policy_failure_class = failure_class,
            policy_action = action,
            token_contract = %self.token_contract,
            result = "policy_fail_closed",
            "consumer API: selected policy action failed closed; no recovery transaction submitted; session remains recoverable"
        );
        false
    }

    async fn policy_unsupported(
        &self,
        failure_class: &str,
        action: &str,
        reason: &str,
        diagnostic: &str,
    ) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        tracing::error!(
            %reason,
            policy_failure_class = failure_class,
            policy_action = action,
            token_contract = %self.token_contract,
            result = "policy_action_unsupported",
            diagnostic,
            "consumer API: selected policy action is unsupported in this runtime surface; session remains recoverable"
        );
        false
    }

    async fn policy_seller_timeout(&self, failure_class: &str, action: &str, reason: &str) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        match self.chain.seller_timeout(&self.token_contract).await {
            Ok(s) => {
                self.settled.store(true, Ordering::SeqCst);
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %self.token_contract,
                    settlement = ?s,
                    "consumer API: selected policy action reclaimed via seller_timeout"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %self.token_contract,
                    error = %e,
                    "consumer API: selected seller_timeout policy action failed; session remains recoverable"
                );
                false
            }
        }
    }

    async fn policy_dispute(&self, failure_class: &str, action: &str, reason: &str) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        match self
            .chain
            .dispute(&self.token_contract, self.note.as_ref())
            .await
        {
            Ok(s) => {
                self.settled.store(true, Ordering::SeqCst);
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %self.token_contract,
                    settlement = ?s,
                    "consumer API: selected policy action opened DISPUTE; the buyer note is locked until resolution"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %self.token_contract,
                    error = %e,
                    "consumer API: selected DISPUTE policy action failed; session remains recoverable"
                );
                false
            }
        }
    }

    /// Apply the explicit `bad_output_scam` policy on a verification bail. `dispute` uses the
    /// existing streamDispute lever and warns about the note lock. `stop_and_blacklist` is not silently
    /// degraded in the consumer API surface because this surface has no seller-id blacklist store.
    pub async fn settle_verification_bail(&self, reason: &str) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        let action = self.failure_policy.verification_bail;
        match action {
            VerificationBailAction::Stop => match self
                .chain
                .stop(&self.token_contract, self.note.as_ref())
                .await
            {
                Ok(s) => {
                    self.settled.store(true, Ordering::SeqCst);
                    tracing::info!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %self.token_contract,
                        settlement = ?s,
                        "consumer API: verification bail settled with STOP"
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %self.token_contract,
                        error = %e,
                        "consumer API: verification-bail STOP failed; session remains recoverable"
                    );
                    false
                }
            },
            VerificationBailAction::Dispute => match self
                .chain
                .dispute(&self.token_contract, self.note.as_ref())
                .await
            {
                Ok(s) => {
                    self.settled.store(true, Ordering::SeqCst);
                    tracing::warn!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %self.token_contract,
                        settlement = ?s,
                        "consumer API: verification bail opened DISPUTE; the buyer note is locked until resolution"
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %self.token_contract,
                        error = %e,
                        "consumer API: verification-bail DISPUTE failed; session remains recoverable"
                    );
                    false
                }
            },
            VerificationBailAction::StopAndBlacklist => {
                tracing::error!(
                    %reason,
                    policy_failure_class = "bad_output_scam",
                    policy_action = action.as_str(),
                    token_contract = %self.token_contract,
                    result = "policy_action_unsupported",
                    diagnostic = "consumer API has no seller identity/blacklist store; refusing to degrade to STOP",
                    "consumer API: stop_and_blacklist unsupported in this runtime surface; session remains recoverable"
                );
                false
            }
        }
    }

    pub async fn settle_dead_gateway(&self, reason: &str) -> bool {
        let action = self.failure_policy.dead_gateway;
        match action {
            DeadGatewayAction::RetryThenReclaim => {
                self.policy_seller_timeout("dead_gateway", action.as_str(), reason)
                    .await
            }
            DeadGatewayAction::NextSeller => {
                self.policy_unsupported(
                    "dead_gateway",
                    action.as_str(),
                    reason,
                    "local consumer API has no model-only seller failover context for this request",
                )
                .await
            }
            DeadGatewayAction::FailClosed => {
                self.policy_fail_closed("dead_gateway", action.as_str(), reason)
                    .await
            }
        }
    }

    pub async fn settle_empty_stream(&self, reason: &str) -> bool {
        let action = self.failure_policy.empty_stream;
        match action {
            EmptyStreamAction::Reclaim => {
                self.policy_seller_timeout("empty_stream", action.as_str(), reason)
                    .await
            }
            EmptyStreamAction::NextSeller => {
                self.policy_unsupported(
                    "empty_stream",
                    action.as_str(),
                    reason,
                    "local consumer API has no model-only seller failover context for this request",
                )
                .await
            }
            EmptyStreamAction::FailClosed => {
                self.policy_fail_closed("empty_stream", action.as_str(), reason)
                    .await
            }
        }
    }

    pub async fn settle_seller_stalls_mid_stream(&self, reason: &str) -> bool {
        let action = self.failure_policy.seller_stalls_mid_stream;
        match action {
            SellerStallsMidStreamAction::AcceptDeliveredThenReclaim => {
                self.policy_seller_timeout("seller_stalls_mid_stream", action.as_str(), reason)
                    .await
            }
            SellerStallsMidStreamAction::Dispute => {
                self.policy_dispute("seller_stalls_mid_stream", action.as_str(), reason)
                    .await
            }
        }
    }
}

impl Drop for SessionSettle {
    fn drop(&mut self) {
        // BEST-EFFORT BACKUP ONLY: the awaited terminal(graceful shutdown / bail) is the
        // funds-safety guarantee. If the session ended with no explicit settle(abnormal teardown), spawn a
        // last-chance STOP -- a crash/SIGKILL/runtime teardown may still skip it, and the on-chain
        // `seller_timeout` is the ultimate backstop.
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        let (chain, tc, note) = (
            self.chain.clone(),
            self.token_contract.clone(),
            self.note.clone(),
        );
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                if let Err(e) = chain.stop(&tc, note.as_ref()).await {
                    tracing::warn!(error = %e, "consumer API: backup STOP on session drop failed");
                }
            });
        }
    }
}

impl ApiState {
    pub fn single(
        buyer: Arc<Buyer>,
        route: Route,
        frame_model: String,
        session: Arc<SessionSettle>,
        content_gate: Arc<ContentGate>,
    ) -> Self {
        Self {
            buyer,
            frame_model,
            deals: Arc::new(RouteManager::new(ApiDeal::new(
                route,
                session,
                content_gate,
            ))),
        }
    }

    pub async fn current_deal(&self) -> ApiDeal {
        self.deals.current().await
    }

    /// The model is forced by the market(B2/B19): an empty/None `model` is ok (there is a single
    /// frame), otherwise we require a match with `frame_model`. Returns `Err` with a
    /// human-readable reject reason.
    pub fn check_model(&self, requested: Option<&str>) -> Result<(), String> {
        match requested {
            None => Ok(()),
            Some("") => Ok(()),
            Some(m) if m == self.frame_model => Ok(()),
            Some(m) => Err(format!(
                "model `{m}` is outside the configured frame `{}` (B2)",
                self.frame_model
            )),
        }
    }
}

/// Build the consumer-interface axum router. The Anthropic transcode(B20) is mounted only
/// when `anthropic_compat = true`.
pub fn router(state: ApiState, anthropic_compat: bool) -> axum::Router {
    use axum::routing::{get, post};
    let mut app = axum::Router::new()
        .route("/v1/chat/completions", post(openai::chat_completions))
        .route("/v1/models", get(openai::models));
    if anthropic_compat {
        app = app.route("/v1/messages", post(anthropic::messages));
    }
    app.with_state(state)
}

/// Bring up the local consumer interface on `bind`. Returns the actual address
/// and a handle to the server's background task. `shutdown` is the session terminal signal (the CLI passes
/// `ctrl_c`/SIGTERM): on it the server drains in-flight requests(graceful shutdown), then the session deal is
/// STOPped via an **awaited** `session.settle("shutdown")` before the task ends -- the funds-safety
/// guarantee(`SessionSettle::Drop` is only a backup). Tests pass a never-completing signal and abort the task.
pub async fn serve(
    bind: SocketAddr,
    state: ApiState,
    anthropic_compat: bool,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    let deals = state.deals.clone();
    let app = router(state, anthropic_compat);
    let task = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(shutdown);
        if let Err(e) = server.await {
            tracing::error!("consumer API server stopped: {e}");
        }
        // Awaited session terminal: after graceful shutdown drains in-flight requests, STOP the
        // deal once before exit. This awaited path -- not `Drop` -- is the funds-safety guarantee.
        deals.settle_active("shutdown").await;
    });
    Ok((local_addr, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_limit_honors_request_and_caps_by_budget() {
        assert_eq!(request_token_limit(Some(400), 1_000_000), 400);
        assert_eq!(request_token_limit(Some(2_000_000), 1_000_000), 1_000_000);
        assert_eq!(request_token_limit(None, 1_000_000), 1_000_000);
        assert_eq!(request_token_limit(Some(0), 1_000_000), 1_000_000);
    }

    #[test]
    fn api_deal_tracks_active_and_recent_consumer_demand() {
        let deal = ApiDeal::new(
            Route {
                handover: Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "00".repeat(32),
                },
                token_contract: "tc-demand".to_string(),
                max_tokens: 100,
            },
            Arc::new(SessionSettle::new(
                Arc::new(RecordingSettleChain::default()),
                "tc-demand".to_string(),
                Arc::new(dexdo_core::LocalNote::generate()),
            )),
            Arc::new(ContentGate::skip()),
        );

        assert!(!deal.has_active_or_recent_request(100, 30));
        {
            let _request = deal.begin_request(100);
            assert!(
                deal.has_active_or_recent_request(1_000, 30),
                "an in-flight consumer request is demand even after the recent window"
            );
        }
        assert!(deal.has_active_or_recent_request(120, 30));
        assert!(!deal.has_active_or_recent_request(131, 30));
    }

    #[test]
    fn accounted_tokens_uses_structured_token_signals() {
        assert_eq!(
            accounted_tokens(&CanonChunk {
                token_ids: vec![1, 2, 3],
                ..CanonChunk::default()
            }),
            3
        );
        assert_eq!(
            accounted_tokens(&CanonChunk {
                logprobs: vec![Default::default(), Default::default()],
                ..CanonChunk::default()
            }),
            2
        );
        assert_eq!(accounted_tokens(&CanonChunk::default()), 1);
    }

    #[test]
    fn sse_paths_record_delivery_per_chunk() {
        let openai = include_str!("openai.rs");
        let anthropic = include_str!("anthropic.rs");
        for source in [openai, anthropic] {
            assert!(
                source.contains("let before = driver.received();"),
                "SSE path must snapshot delivery count before accounting"
            );
            assert!(
                source.contains("deal.record_delivered(driver.received().saturating_sub(before));"),
                "SSE path must record each rendered chunk immediately"
            );
            assert!(
                !source.contains("let received = driver.received();\n        drop(driver);\n        deal.record_delivered(received);"),
                "SSE path must not wait until stream end to publish delivered tokens"
            );
        }
    }

    #[derive(Default)]
    struct RecordingSettleChain {
        stop_calls: std::sync::atomic::AtomicUsize,
        dispute_calls: std::sync::atomic::AtomicUsize,
        seller_timeout_calls: std::sync::atomic::AtomicUsize,
        fail_stop: std::sync::atomic::AtomicBool,
        fail_dispute: std::sync::atomic::AtomicBool,
        fail_seller_timeout: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl ChainBackend for RecordingSettleChain {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn place_buy(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn read_match(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn open_stream(
            &self,
            _token_contract: &TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn read_handover(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn advance_tick(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn accept_probe(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn stop(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_stop.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(dexdo_core::ChainError::Chain(
                    "injected stop failure".to_string(),
                ));
            }
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            })
        }

        async fn dispute(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.dispute_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_dispute.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(dexdo_core::ChainError::Chain(
                    "injected dispute failure".to_string(),
                ));
            }
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            })
        }

        async fn seller_timeout(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.seller_timeout_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .fail_seller_timeout
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(dexdo_core::ChainError::Chain(
                    "injected seller_timeout failure".to_string(),
                ));
            }
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            })
        }

        async fn snapshot(
            &self,
            _token_contract: &TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    #[tokio::test]
    async fn verification_bail_dispute_uses_dispute_lever() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_verification_bail_action(
            chain.clone(),
            "tc-dispute".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            VerificationBailAction::Dispute,
        );

        assert!(session.settle_verification_bail("test-bail").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert!(
            !session.settle_verification_bail("duplicate").await,
            "settlement remains idempotent"
        );
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_verification_bail_stop_keeps_session_recoverable() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_stop.store(true, Ordering::SeqCst);
        let session = SessionSettle::new_with_verification_bail_action(
            chain.clone(),
            "tc-failed-stop".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            VerificationBailAction::Stop,
        );

        assert!(!session.settle_verification_bail("test-bail").await);
        assert!(
            session.is_closed(),
            "failed STOP must close the local API route to a second request"
        );
        assert!(
            !session.is_settled(),
            "failed STOP must not make the session terminal"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);

        chain.fail_stop.store(false, Ordering::SeqCst);
        assert!(session.settle("shutdown").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_verification_bail_dispute_keeps_session_recoverable() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_dispute.store(true, Ordering::SeqCst);
        let session = SessionSettle::new_with_verification_bail_action(
            chain.clone(),
            "tc-failed-dispute".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            VerificationBailAction::Dispute,
        );

        assert!(!session.settle_verification_bail("test-bail").await);
        assert!(
            session.is_closed(),
            "failed DISPUTE must close the local API route to a second request"
        );
        assert!(
            !session.is_settled(),
            "failed DISPUTE must not make the session terminal"
        );
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);

        assert!(session.settle("shutdown").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn verification_bail_stop_and_blacklist_fails_closed_without_stop() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_verification_bail_action(
            chain.clone(),
            "tc-stop".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            VerificationBailAction::StopAndBlacklist,
        );

        assert!(!session.settle_verification_bail("test-bail").await);
        assert!(
            session.is_closed(),
            "unsupported stop_and_blacklist must close the local API route to a second request"
        );
        assert!(
            !session.is_settled(),
            "unsupported stop_and_blacklist must keep the session recoverable"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);

        assert!(session.settle("shutdown").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dead_gateway_retry_then_reclaim_uses_seller_timeout() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-dead-gateway".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy {
                dead_gateway: DeadGatewayAction::RetryThenReclaim,
                ..BuyerApiFailurePolicy::default()
            },
        );

        assert!(session.settle_dead_gateway("test-dead-gateway").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.seller_timeout_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn empty_stream_reclaim_uses_seller_timeout() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-empty".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy {
                empty_stream: EmptyStreamAction::Reclaim,
                ..BuyerApiFailurePolicy::default()
            },
        );

        assert!(session.settle_empty_stream("test-empty").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.seller_timeout_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_seller_timeout_recovery_keeps_session_recoverable() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_seller_timeout.store(true, Ordering::SeqCst);
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-timeout-failure".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy {
                empty_stream: EmptyStreamAction::Reclaim,
                ..BuyerApiFailurePolicy::default()
            },
        );

        assert!(!session.settle_empty_stream("test-empty").await);
        assert!(
            session.is_closed(),
            "failed seller_timeout must close the local API route to a second request"
        );
        assert!(
            !session.is_settled(),
            "failed seller_timeout must not make the session terminal"
        );
        assert_eq!(chain.seller_timeout_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);

        assert!(session.settle("shutdown").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn seller_stalls_mid_stream_dispute_uses_dispute() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-stall".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy {
                seller_stalls_mid_stream: SellerStallsMidStreamAction::Dispute,
                ..BuyerApiFailurePolicy::default()
            },
        );

        assert!(session.settle_seller_stalls_mid_stream("test-stall").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.seller_timeout_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dead_gateway_next_seller_fails_closed_but_keeps_session_recoverable() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-next-seller".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy {
                dead_gateway: DeadGatewayAction::NextSeller,
                ..BuyerApiFailurePolicy::default()
            },
        );

        assert!(!session.settle_dead_gateway("test-next").await);
        assert!(
            session.is_closed(),
            "unsupported next_seller must close the local API route to a second request"
        );
        assert!(
            !session.is_settled(),
            "unsupported next_seller must keep the session recoverable"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.seller_timeout_calls.load(Ordering::SeqCst), 0);

        assert!(session.settle("shutdown").await);
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn request_gate_uses_local_api_closed_state() {
        let openai = include_str!("openai.rs");
        let anthropic = include_str!("anthropic.rs");
        for source in [openai, anthropic] {
            assert!(
                source.contains("deal.session.is_closed()"),
                "request gate must reject locally closed recovery-pending sessions"
            );
            assert!(
                !source.contains("deal.session.is_settled()"),
                "request gate must not use terminal settlement as the local API closed state"
            );
        }
    }

    #[test]
    fn valid_consumer_request_marks_demand_before_closed_session_reject() {
        let openai = include_str!("openai.rs");
        let anthropic = include_str!("anthropic.rs");
        for source in [openai, anthropic] {
            let model_check = source
                .find("state.check_model")
                .expect("handler validates model before demand");
            let mark_demand = source
                .find("begin_request")
                .expect("handler records consumer demand");
            let closed_gate = source
                .find("deal.session.is_closed()")
                .expect("handler gates closed sessions");
            assert!(
                model_check < mark_demand,
                "invalid model requests must not wake renewal"
            );
            assert!(
                mark_demand < closed_gate,
                "valid requests hitting a closed session must wake demand-driven renewal"
            );
        }
    }

    #[test]
    fn every_verification_bail_path_uses_policy_settlement() {
        let openai = include_str!("openai.rs");
        let anthropic = include_str!("anthropic.rs");
        let api = include_str!("mod.rs");
        assert!(openai.contains("settle_verification_bail(\"verify-bail\")"));
        assert!(anthropic.contains("settle_verification_bail(\"verify-bail\")"));
        assert!(api.contains("settle_verification_bail(\"content-identity-bail\")"));
        assert!(!anthropic.contains("settle(\"verify-bail\")"));
        assert!(!api.contains("settle(\"content-identity-bail\")"));
    }

    #[test]
    fn stream_error_policy_action_is_narrow() {
        assert_eq!(
            stream_error_policy_action(
                r#"status: Unavailable, message: "upstream HTTP 400 Bad Request""#,
                0,
            ),
            StreamErrorPolicyAction::RequestScoped,
            "known upstream 4xx request rejections are per-request 502s"
        );
        assert_eq!(
            stream_error_policy_action(
                r#"status: Unavailable, message: "upstream HTTP 400 Bad Request""#,
                1,
            ),
            StreamErrorPolicyAction::SellerStallsMidStream,
            "once chunks were accepted, later errors keep seller-stall policy"
        );
        assert_eq!(
            stream_error_policy_action(
                r#"status: Unavailable, message: "upstream HTTP 500 Internal Server Error""#,
                0,
            ),
            StreamErrorPolicyAction::DeadGateway,
            "generic pre-token stream errors keep  dead-gateway policy"
        );
        assert_eq!(
            stream_error_policy_action("upstream SSE frame exceeds buffer cap", 2),
            StreamErrorPolicyAction::SellerStallsMidStream,
            "generic post-delivery stream errors keep  seller-stall policy"
        );
    }

    #[test]
    fn stream_error_policy_is_shared_by_openai_and_anthropic() {
        let openai = include_str!("openai.rs");
        let anthropic = include_str!("anthropic.rs");
        for source in [openai, anthropic] {
            assert!(
                source.contains("handle_stream_error_policy(&deal, received"),
                "both consumer surfaces must route stream errors through the shared  classifier"
            );
        }
        let api = include_str!("mod.rs");
        assert!(api.contains("settle_dead_gateway(\"stream-error-before-token\")"));
        assert!(api.contains("settle_seller_stalls_mid_stream(\"seller-stalls-mid-stream\")"));
    }
}
