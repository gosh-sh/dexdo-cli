//! Local consumer interface: an OpenAI-compatible HTTP endpoint
//! (`/v1/chat/completions`, `/v1/models`) and an optional Anthropic-compatible transcode
//! (`/v1/messages`). The endpoint listens on **loopback** by default.

//! Request path (B19): receive -> build `CanonRequest` -> route to the (mock) seller ->
//! authorized TLS gRPC stream -> receive `CanonChunk` -> re-render to SSE in the desired format.
//! Canonical verification and the visible-output cap are enforced BEFORE re-rendering
//! ([`crate::buyer::verify::StreamVerifier`]); monetary accounting waits for validated terminal
//! input-plus-output usage and clean EOF.

//! The model is forced by the market/frame (B2, B19): the request's `model` field is NOT trusted;
//! a request outside the configured model frame is rejected. Any API key is accepted: this is a
//! loopback endpoint.

pub mod anthropic;
pub mod openai;
mod stream;

use crate::buyer::verify::Verdict;
use crate::buyer::Buyer;
use crate::seller::ModelsConfig;
use anyhow::Result;
use dexdo_core::{
    params::{
        CONTENT_PROBE_MAX_TOKENS, DEFAULT_BUYER_DEAD_GATEWAY_ACTION,
        DEFAULT_BUYER_EMPTY_STREAM_ACTION, DEFAULT_BUYER_STALLS_MID_STREAM_ACTION,
        DEFAULT_BUYER_VERIFICATION_BAIL_ACTION, MATCH_OPEN_TIMEOUT,
    },
    ChainBackend, Handover, Note, TokenContract, MATCH_OPEN_TIMEOUT_SECS,
};
use dexdo_proto::CanonChunk;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Mutex, OnceCell, RwLock};

fn display_token_contract(token_contract: &str) -> String {
    dexdo_core::address::display_self_dapp(token_contract)
}

pub type DealInitFuture = Pin<Box<dyn Future<Output = Result<ApiDeal, DealInitError>> + Send>>;
pub type DealInitializer = Arc<dyn Fn() -> DealInitFuture + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BuyerSubmitRecoveryAnchor {
    #[serde(serialize_with = "serialize_u128_decimal")]
    pub order_id: u128,
    pub token_contract: TokenContract,
}

fn serialize_u128_decimal<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuyerSubmitReconciliationState {
    FreshUnresolved,
    DurableUnresolved,
    RecoveredProven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuyerSubmitReconciliationOrigin {
    FreshSubmit,
    DurableJournal,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BuyerSubmitReconciliation {
    pub submit_identity: String,
    pub recovery_anchor: BuyerSubmitRecoveryAnchor,
    pub state: BuyerSubmitReconciliationState,
    pub origin: BuyerSubmitReconciliationOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DealInitError {
    message: String,
    reconciliation: Option<BuyerSubmitReconciliation>,
}

impl DealInitError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            reconciliation: None,
        }
    }

    pub fn with_reconciliation(
        message: impl Into<String>,
        reconciliation: BuyerSubmitReconciliation,
    ) -> Self {
        Self {
            message: message.into(),
            reconciliation: Some(reconciliation),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn reconciliation(&self) -> Option<&BuyerSubmitReconciliation> {
        self.reconciliation.as_ref()
    }
}

impl fmt::Display for DealInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DealInitError {}

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
pub enum RecoveryKind {
    CleanupUnopened,
    ReclaimOpened,
}

impl RecoveryKind {
    const fn code(self) -> u8 {
        match self {
            Self::CleanupUnopened => 1,
            Self::ReclaimOpened => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::CleanupUnopened),
            2 => Some(Self::ReclaimOpened),
            _ => None,
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
        let verification_bail = match DEFAULT_BUYER_VERIFICATION_BAIL_ACTION {
            "stop" => VerificationBailAction::Stop,
            "dispute" => VerificationBailAction::Dispute,
            "stop_and_blacklist" => VerificationBailAction::StopAndBlacklist,
            value => panic!("invalid canonical buyer verification-bail action: {value}"),
        };
        let dead_gateway = match DEFAULT_BUYER_DEAD_GATEWAY_ACTION {
            "retry_then_reclaim" => DeadGatewayAction::RetryThenReclaim,
            "next_seller" => DeadGatewayAction::NextSeller,
            "fail_closed" => DeadGatewayAction::FailClosed,
            value => panic!("invalid canonical buyer dead-gateway action: {value}"),
        };
        let empty_stream = match DEFAULT_BUYER_EMPTY_STREAM_ACTION {
            "reclaim" => EmptyStreamAction::Reclaim,
            "next_seller" => EmptyStreamAction::NextSeller,
            "fail_closed" => EmptyStreamAction::FailClosed,
            value => panic!("invalid canonical buyer empty-stream action: {value}"),
        };
        let seller_stalls_mid_stream = match DEFAULT_BUYER_STALLS_MID_STREAM_ACTION {
            "accept_delivered_then_reclaim" => {
                SellerStallsMidStreamAction::AcceptDeliveredThenReclaim
            }
            "dispute" => SellerStallsMidStreamAction::Dispute,
            value => panic!("invalid canonical buyer mid-stream-stall action: {value}"),
        };
        Self {
            verification_bail,
            dead_gateway,
            empty_stream,
            seller_stalls_mid_stream,
        }
    }
}

/// Route to the (mock) seller + model frame, shared by the HTTP handlers (B1/B2/B19).
/// In "routing" is a single fixed match (one seller, mock chain); semantic orders
/// and seller selection are the horizon of.
#[derive(Clone)]
pub struct Route {
    pub handover: Handover,
    pub token_contract: TokenContract,
    /// Deal/session token budget as it stood when the route was built. Per-request `max_tokens` is
    /// honored by the handlers but cannot exceed the deal's LIVE budget
    /// ([`ApiDeal::remaining_tokens`]), which for an ordinary deal is exactly this figure minus what
    /// has been delivered and for a subscription tracks the current week.
    pub max_tokens: u64,
}

/// Live weekly allowance of a running subscription route.

/// A subscription's ceiling is one weekly quota measured from what the previous weeks consumed, and it
/// moves at every week boundary of a four-week term. The figure a client computes from
/// `getSubscription()` is not reliably that ceiling: the recorded `weekBaseTokens`/`weekIndex` move
/// when a week is BOOKED, not when the clock passes a boundary, and the relation has three phases
/// (see [`dexdo_core::subscription_claim_cap_at`]) - exact while no boundary has been crossed, an
/// under-statement across one that nobody booked, and an upper bound past the final boundary, where
/// the ceiling becomes the cumulative total already declared. Neither of the last two is strict.

/// So this does not predict. When the allowance is spent, or when the recorded week has run out on the
/// wall clock, it submits the permissionless boundary-booking call and then recomputes from the
/// coherent state that comes back. The buyer's clock only decides WHEN to go and ask; what is
/// authorized comes from the chain, and a failure to book authorizes nothing.
pub struct SubscriptionWeeklyBudget {
    chain: Arc<dyn ChainBackend>,
    token_contract: TokenContract,
    /// One reconciliation at a time; the losers read the ceiling the winner published.
    refresh_lock: Mutex<()>,
    /// The cumulative claim this route's local counter measures FROM: `tokensPending` as it stood
    /// when the route was built and `billable_tokens` was zero. Fixed for the life of the deal.

    /// Everything the contract bounds is CUMULATIVE, and the local counter is not - so the two are
    /// only comparable through this one baseline. `anchor + delivered` is what the deal has actually
    /// consumed, whether or not the seller has got round to claiming it; the ceiling is therefore
    /// `_claimCap - anchor` and never `delivered + (_claimCap - tokensPending)`, which reads the
    /// remainder off the seller's claim and hands the difference back to the route every time he
    /// lags. A seller who stops claiming stops the route: his lag no longer buys it anything.

    /// Rebased exactly once, and only if the route was built BEFORE the trial tick was accepted:
    /// `acceptProbe` seeds all three claim stages with one `TICK_SIZE` (`TokenContract.sol:690`),
    /// which is consumption this route did not make and must therefore sit BELOW its local zero. An
    /// anchor taken at zero and left there would authorize one tick more than the term ever sells.
    claim_anchor: std::sync::Mutex<u128>,
    /// Whether [`Self::claim_anchor`] still needs that one rebase.
    anchored_before_probe: AtomicBool,
    #[cfg(test)]
    rebase_barrier: std::sync::Mutex<Option<Arc<std::sync::Barrier>>>,
    /// Set only from authoritative state: a disputed/stopped deal, or a term whose final boundary is
    /// BOOKED. Terminal is forever - no later reconciliation may reopen it.
    terminal: AtomicBool,
    /// The `weekIndex` the published ceiling belongs to. A ceiling is republished when - and only
    /// when - the chain says the week changed, so an under-used week can never roll into the next.
    published_week: AtomicU8,
    /// Unix second at which the published week runs out on the wall clock. Reaching it forces a
    /// reconciliation before anything further is served; it never authorizes anything by itself.
    published_week_expires_at: AtomicU64,
}

/// What the pre-request admission gate decided.
enum RouteBudget {
    /// Billable tokens this request has reserved against the route. Holding it serializes requests
    /// on one deal; dropping it returns the part not accepted by terminal usage.
    Admitted(RouteReservation),
    /// Nothing is deliverable; the payload is the operator-facing reject text.
    Exhausted(String),
}

/// An admitted request's claim on the route's remaining tokens.

/// Reservation happens once, atomically, before the model is contacted. `granted` is the total
/// input+output billing grant, not the output cap; the consumer's `max_tokens` remains output-only.
/// What terminal usage does not consume comes back when this is dropped.
struct RouteReservation {
    reserved: Arc<AtomicU64>,
    granted: u64,
    used: u64,
}

impl RouteReservation {
    fn remaining(&self) -> u64 {
        self.granted.saturating_sub(self.used)
    }

    fn checked_used_after(&self, billable: u64) -> Result<u64, String> {
        self.used
            .checked_add(billable)
            .filter(|used| *used <= self.granted)
            .ok_or_else(|| {
                format!(
                    "accepted billable usage of {billable} tokens does not fit the held route reservation: \
                     {} used of {} granted",
                    self.used, self.granted
                )
            })
    }
}

impl Drop for RouteReservation {
    fn drop(&mut self) {
        self.reserved.fetch_sub(self.remaining(), Ordering::SeqCst);
    }
}

/// Tell the seller the request's output-only limit.

/// Admission now holds the whole free route remainder because provider-native input usage is unknown.
/// That billing grant is sent separately. This function only clamps canonical `max_tokens` to the
/// consumer's requested output and the available grant, and returns the output cap enforced on reply.
fn cap_canon_to_grant(canon: &mut dexdo_proto::CanonRequest, granted: u64) -> u64 {
    let granted = u32::try_from(granted).unwrap_or(u32::MAX);
    let capped = match canon.params.as_mut() {
        Some(params) => {
            params.max_tokens = match params.max_tokens {
                // `0` is "unset" on the wire, so an unset caller limit becomes the grant itself.
                0 => granted,
                asked => asked.min(granted),
            };
            params.max_tokens
        }
        None => {
            canon.params = Some(dexdo_proto::SamplingParams {
                temperature: 0.0,
                max_tokens: granted,
                stop: Vec::new(),
                greedy: false,
            });
            granted
        }
    };
    u64::from(capped)
}

const ORDINARY_BUDGET_EXHAUSTED: &str =
    "active deal budget exhausted; waiting for renewal handover";

/// Refusal for a deal that still owes its one-per-deal identity verification and no longer has the
/// budget to pay for that AND deliver an answer.

/// It can fire with tokens still on the route, and that is the point: admitting them would spend the
/// last of the deal on a probe no answer could follow, which is exactly the paid-verification /
/// zero-inference outcome this refusal exists to prevent. A resumed deal is where it is reachable --
/// [`ApiDeal::new`] builds a fresh, unverified gate over whatever remainder the route was rebuilt
/// with.
const UNVERIFIED_BUDGET_CANNOT_COVER_VERIFICATION: &str =
    "what is left of this deal cannot pay for the identity verification it still owes and deliver \
     an answer as well; refusing before the verification probe is sent rather than burning it for \
     nothing";

impl SubscriptionWeeklyBudget {
    /// `claim_anchor`, `published_week` and `expires_at` must all come from the SAME coherent snapshot
    /// the route's initial `max_tokens` was computed from: the anchor is that snapshot's
    /// `tokensPending`, which is where the route's local delivered counter starts at zero.
    pub fn new(
        chain: Arc<dyn ChainBackend>,
        token_contract: TokenContract,
        state: &dexdo_core::DealChainState,
        subscription: &dexdo_core::DealSubscription,
    ) -> Self {
        Self {
            chain,
            token_contract,
            refresh_lock: Mutex::new(()),
            claim_anchor: std::sync::Mutex::new(state.tokens_pending),
            anchored_before_probe: AtomicBool::new(!state.probe_accepted),
            #[cfg(test)]
            rebase_barrier: std::sync::Mutex::new(None),
            terminal: AtomicBool::new(subscription.term_is_over()),
            published_week: AtomicU8::new(subscription.week_index),
            published_week_expires_at: AtomicU64::new(subscription.recorded_week_expires_at()),
        }
    }

    /// The local ceiling implied by an authoritative snapshot: the contract's own `_claimCap`,
    /// expressed against the anchor the local counter measures from.

    /// Fails closed rather than saturating. A cap below the anchor means the chain has contradicted
    /// the state this route was built on, and a ceiling that does not fit `u64` cannot be enforced by
    /// a `u64` counter - in both cases a manufactured number would be an authorization nobody
    /// computed, so there is none.
    fn local_ceiling(
        &self,
        snapshot: &dexdo_core::DealChainSnapshot,
        anchor: u128,
    ) -> Result<u64, String> {
        let token_contract = display_token_contract(&self.token_contract);
        let cap = dexdo_core::subscription_claim_cap_at(&snapshot.state, &snapshot.subscription)
            .map_err(|error| format!("subscription {token_contract}: {error}"))?;
        let local = cap.checked_sub(anchor).ok_or_else(|| {
            format!(
                "subscription {}: claim ceiling {cap} is below the cumulative claim {anchor} this \
                 route was anchored on; refusing rather than authorizing on contradictory state",
                token_contract
            )
        })?;
        u64::try_from(local).map_err(|_| {
            format!(
                "subscription {}: claim ceiling {local} tokens above the anchor does not fit the \
                 route's counter; refusing rather than serving a truncated authorization",
                token_contract
            )
        })
    }

    fn is_terminal(&self) -> bool {
        self.terminal.load(Ordering::SeqCst)
    }

    /// Whether the published week has run out on the wall clock. A trigger, never an authorization:
    /// it only says the route must go and ask the chain again before serving anything else.
    fn published_week_ran_out(&self) -> bool {
        unix_now_secs() >= self.published_week_expires_at.load(Ordering::SeqCst)
    }

    /// Rebase the anchor the first time an authoritative read shows the trial tick accepted.
    /// Returns whether it moved, because the ceiling that was published against the old one is then
    /// stale and must be recomputed in the same breath.

    /// `acceptProbe` sets all three claim stages to a FLAT `TICK_SIZE` - it does not add one. So it
    /// absorbs whatever this route had already delivered before acceptance (`delivered`, capped at a
    /// tick by the seller's own pre-probe capacity), and the part of that seed which is NOT this
    /// route's own delivery is `TICK_SIZE - delivered`. That, and only that, belongs below the local
    /// counter's zero.

    /// Adding a whole `TICK_SIZE` instead would be wrong in both directions at once: the current week
    /// would still be over by `TICK_SIZE - delivered`, and the first booking would then measure the
    /// new week short by `delivered`. The errors do not cancel - they change sign at the boundary.

    /// The trigger cannot be `weekIndex`: acceptance does not move it. It is the acceptance flag on a
    /// fresh snapshot, which is why a route still anchored pre-probe reconciles on every admission.
    fn rebase_anchor_on_probe(
        &self,
        state: &dexdo_core::DealChainState,
        delivered: u64,
        anchor: &mut u128,
    ) -> Result<bool, String> {
        if !state.probe_accepted || !self.anchored_before_probe.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let rebased_anchor = dexdo_core::TICK_SIZE
            .checked_sub(u128::from(delivered))
            .ok_or_else(|| {
                let token_contract = display_token_contract(&self.token_contract);
                format!(
                    "subscription {}: the route delivered {delivered} tokens before probe acceptance, \
                     above the one-tick probe claim {}; refusing contradictory state",
                    token_contract,
                    dexdo_core::TICK_SIZE
                )
            })?;
        *anchor = rebased_anchor;
        Ok(true)
    }

    /// Whether this route's anchor still describes a deal whose trial tick was not yet accepted.
    /// While that is true the fast admission path is not safe: the ceiling it would serve was
    /// measured from a cumulative claim the contract is about to move underneath it.
    fn anchored_before_probe(&self) -> bool {
        self.anchored_before_probe.load(Ordering::SeqCst)
    }

    /// Book every boundary the CHAIN says is due, then recompute from the state that comes back.

    /// Returns the reject text when the route may not serve.

    /// The booking is a MONEY PATH, not a read: `_chargeWeeksThrough` moves already-escrowed value -
    /// `_deposit -= pay + fee; _finalizedOwed += pay; _feeAccrued += fee`
    /// (`TokenContract.sol:922-933`). What it does not do is create a new commitment: it charges the
    /// weeks the term already owes, which every exit charges anyway, so it cannot cost the buyer
    /// anything the deal had not already committed. It is also permissionless, and it is the only
    /// thing here that decides a boundary was crossed - the contract refuses it when none was.

    /// Nothing is published until the whole coherent state has been validated. Publishing the week or
    /// its expiry before the ceiling that belongs to them would leave a stale positive ceiling looking
    /// fresh, and the next request would skip reconciliation entirely and serve it.
    async fn reconcile(&self, ceiling: &AtomicU64, delivered: &AtomicU64) -> Result<(), String> {
        let token_contract = display_token_contract(&self.token_contract);
        if self.is_terminal() {
            return Err(format!(
                "subscription {} is terminal; the contract admits no further claim",
                token_contract
            ));
        }
        // Ask the chain to cross whatever it owes. A deal with nothing due answers ERR_SETTLE_WINDOW_OPEN
        // and nothing changes - which is the correct answer to "is the week over?" and is exactly why
        // the buyer's clock is not allowed to answer it.
        let booking = self.chain.settle_week(&self.token_contract).await;
        let snapshot = self
            .chain
            .deal_snapshot(&self.token_contract)
            .await
            .map_err(|error| {
                format!(
                    "subscription {}: authoritative weekly state is unreadable ({error}); refusing \
                     rather than serving on a stale ceiling",
                    token_contract
                )
            })?
            .ok_or_else(|| {
                format!(
                    "subscription {}: no coherent snapshot; refusing rather than serving on a stale \
                     ceiling",
                    token_contract
                )
            })?;
        if !snapshot.subscription.is_subscription() {
            return Err(format!(
                "TokenContract {} is not a subscription; its budget has no weekly boundary",
                token_contract
            ));
        }
        if snapshot.state.disputed || snapshot.state.is_stopped() {
            self.terminal.store(true, Ordering::SeqCst);
            return Err(format!(
                "subscription {} is disputed/stopped; it cannot be revived by a quota refresh",
                token_contract
            ));
        }
        if snapshot.subscription.term_is_over() {
            self.terminal.store(true, Ordering::SeqCst);
            return Err(format!(
                "subscription {} has served its full {}-week term; the claim ceiling is the \
                 cumulative total already declared and no quota remains",
                token_contract, snapshot.subscription.sub_weeks
            ));
        }
        if !snapshot.state.opened {
            // Not latched: only the terminal paths clear `_opened` for good, and the serving gate
            // refuses an unopened deal on its own terms.
            return Err(format!(
                "subscription {} is not open; no weekly quota is servable",
                token_contract
            ));
        }
        let week = snapshot.subscription.week_index;
        let expires_at = snapshot.subscription.recorded_week_expires_at();
        let republished = self.published_week.load(Ordering::SeqCst) != week;

        // The booking's RESPONSE is not evidence; the booked state is. A submission whose response
        // was lost still moved the chain, and a submission that succeeded is not visible until a read
        // shows it. So when the chain says it booked and the state that comes back is still the week
        // we already had, this read cannot support anything: publish nothing and let the next request
        // ask again, rather than republishing an expiry that would let it skip asking.
        if booking.is_ok() && !republished {
            return Err(format!(
                "subscription {} booked a boundary the authoritative read does not show yet; \
                 refusing rather than serving week {} on a read that lags the booking",
                token_contract,
                u32::from(week) + 1
            ));
        }
        // Nothing booked, and the week on record has run out on the wall clock. Whether the contract
        // refused because no boundary was due or the submission never landed cannot be told apart
        // from here - and either way the remainder on record belongs to a week the clock says is
        // over. Publish nothing: a fresh expiry would authorize exactly the stale remainder that must
        // not be served, and the next request must come back and ask again.
        if !republished && unix_now_secs() >= expires_at {
            return Err(format!(
                "subscription {} could not book the boundary its week {} of {} needs (the contract \
                 answered no boundary was due, or the submission did not land); the recorded week \
                 ended at unix {expires_at} and its remainder is not an authorization",
                token_contract,
                u32::from(week) + 1,
                snapshot.subscription.sub_weeks
            ));
        }

        // Everything below is computed BEFORE anything is published, so a failure here leaves the
        // route exactly as it was - still expired, still forced to reconcile on the next request.
        // The trial tick may have been accepted since this route was built. That does not move
        // `weekIndex`, so it is not a republish - but it does move the baseline the ceiling is
        // measured from, which makes the published one stale.
        // `claim_anchor` is also the cutover mutex used by per-chunk accounting while this route is
        // still pre-probe. It is acquired only after all chain awaits and held across the complete
        // linearization point: delivered sample, anchor/ceiling publication, and gate cutover.
        let mut cutover_anchor = if snapshot.state.probe_accepted && self.anchored_before_probe() {
            Some(self.claim_anchor.lock().map_err(|_| {
                format!(
                    "subscription {}: acceptance cutover lock is poisoned; refusing quota",
                    token_contract
                )
            })?)
        } else {
            None
        };
        #[cfg(test)]
        if cutover_anchor.is_some() {
            if let Some(barrier) = self.rebase_barrier.lock().unwrap().take() {
                barrier.wait();
                barrier.wait();
            }
        }
        let delivered_at_cutover = delivered.load(Ordering::SeqCst);
        let rebased = match cutover_anchor.as_deref_mut() {
            Some(anchor) => {
                self.rebase_anchor_on_probe(&snapshot.state, delivered_at_cutover, anchor)?
            }
            None => false,
        };
        let published_ceiling = if republished || rebased {
            // A new week was BOOKED. Its allowance is measured from the cumulative claim the booking
            // re-based on, and it starts here: whatever the previous week left unspent is forfeited,
            // never carried across the boundary.
            let anchor = match cutover_anchor.as_deref() {
                Some(anchor) => *anchor,
                None => *self.claim_anchor.lock().map_err(|_| {
                    format!(
                        "subscription {}: claim anchor lock is poisoned; refusing quota",
                        token_contract
                    )
                })?,
            };
            self.local_ceiling(&snapshot, anchor)?
        } else {
            ceiling.load(Ordering::SeqCst)
        };
        if published_ceiling <= delivered.load(Ordering::SeqCst) {
            let booked = match &booking {
                Ok(()) => "the boundary was booked",
                Err(_) => "no boundary was due",
            };
            // A drawn-down week is a fact about the CURRENT week, so publishing it is right: the
            // route may serve nothing until the next boundary, and the expiry is when to ask again.
            self.publish(week, expires_at, ceiling, published_ceiling);
            if rebased {
                self.anchored_before_probe.store(false, Ordering::SeqCst);
            }
            return Err(format!(
                "subscription {} week {} of {} is drawn down ({booked}); the next weekly quota opens \
                 at unix {expires_at} and the deal stays live until then",
                token_contract,
                u32::from(week) + 1,
                snapshot.subscription.sub_weeks,
            ));
        }
        self.publish(week, expires_at, ceiling, published_ceiling);
        if rebased {
            self.anchored_before_probe.store(false, Ordering::SeqCst);
        }
        drop(cutover_anchor);
        Ok(())
    }

    /// Publish the week, its expiry and the ceiling that belongs to them, in that order, once the
    /// whole snapshot has been validated. They are one fact about one week and are never written
    /// apart: a fresh expiry over a stale ceiling is precisely what lets the next request skip
    /// reconciliation and serve a week that has ended.
    fn publish(&self, week: u8, expires_at: u64, ceiling: &AtomicU64, published_ceiling: u64) {
        ceiling.store(published_ceiling, Ordering::SeqCst);
        self.published_week.store(week, Ordering::SeqCst);
        self.published_week_expires_at
            .store(expires_at, Ordering::SeqCst);
    }
}

/// One currently usable consumer-API deal: route, settlement terminal, and one-per-deal content gate.
#[derive(Clone)]
pub struct ApiDeal {
    pub route: Route,
    pub session: Arc<SessionSettle>,
    pub content_gate: Arc<ContentGate>,
    billable_tokens: Arc<AtomicU64>,
    /// Cumulative accepted output delivery kept separately with the legacy v2 accounting rule
    /// (`max(token_ids.len(), 1)` per accepted content chunk). It is observational only; monetary
    /// accounting uses `billable_tokens`.
    route_visible_output_tokens: Arc<AtomicU64>,
    /// Billable tokens handed out to admitted requests, used or still in flight. Admission moves this and
    /// nothing else, so two requests can never be handed the same remainder.
    reserved_billable_tokens: Arc<AtomicU64>,
    /// Live ceiling on the CUMULATIVE `billable_tokens` counter. An ordinary deal pins it to
    /// `route.max_tokens` for the life of the deal; a subscription republishes it from the contract's
    /// own claim ceiling every time a week boundary is BOOKED.
    token_ceiling: Arc<AtomicU64>,
    weekly: Option<Arc<SubscriptionWeeklyBudget>>,
    last_accepted_output_unix_secs: Arc<AtomicU64>,
    accepted_output_generation: Arc<AtomicU64>,
    active_requests: Arc<AtomicU64>,
    last_request_started_unix_secs: Arc<AtomicU64>,
}

impl ApiDeal {
    pub fn new(route: Route, session: Arc<SessionSettle>, content_gate: Arc<ContentGate>) -> Self {
        let token_ceiling = Arc::new(AtomicU64::new(route.max_tokens));
        // Keep monetary billing and the stable v2/ delivery witness distinct: provider-native
        // usage advances the former only after clean terminal validation, while every accepted
        // content chunk advances the latter immediately.
        let billable_tokens = Arc::new(AtomicU64::new(0));
        let route_visible_output_tokens = Arc::new(AtomicU64::new(0));
        session.bind_route_billing(billable_tokens.clone());
        session.bind_route_delivery(route_visible_output_tokens.clone());
        Self {
            route,
            session,
            content_gate,
            billable_tokens,
            route_visible_output_tokens,
            reserved_billable_tokens: Arc::new(AtomicU64::new(0)),
            token_ceiling,
            weekly: None,
            last_accepted_output_unix_secs: Arc::new(AtomicU64::new(0)),
            accepted_output_generation: Arc::new(AtomicU64::new(0)),
            active_requests: Arc::new(AtomicU64::new(0)),
            last_request_started_unix_secs: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Attach the live weekly allowance of a subscription route. Without it the deal keeps the
    /// fixed `route.max_tokens` budget, which is exactly right for an ordinary by-fact deal.
    pub fn with_weekly_budget(mut self, weekly: Arc<SubscriptionWeeklyBudget>) -> Self {
        self.weekly = Some(weekly);
        self
    }

    /// Cumulative input+output tokens this route has billed. Production never reads it -- the ceiling and the
    /// reservation are what callers act on -- so it exists only where its one consumer does.
    #[cfg(test)]
    fn billable_tokens(&self) -> u64 {
        self.billable_tokens.load(Ordering::SeqCst)
    }

    /// Record one accepted legacy-accounted output chunk for the stable buyer-event v2 high-water.
    /// This counter is deliberately independent of the terminal input+output monetary counter.
    pub(crate) fn record_visible_output(&self, tokens: u64) -> Result<(), String> {
        self.route_visible_output_tokens
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |visible| {
                visible.checked_add(tokens)
            })
            .map(|_| ())
            .map_err(|_| "cumulative visible-output accounting overflow".to_string())
    }

    pub(crate) fn route_visible_output_tokens(&self) -> u64 {
        self.route_visible_output_tokens.load(Ordering::SeqCst)
    }

    /// Tokens still deliverable under the published ceiling -- what has NOT been handed to a request.
    pub fn remaining_tokens(&self) -> u64 {
        if self.weekly.as_ref().is_some_and(|w| w.is_terminal()) {
            return 0;
        }
        // This public snapshot has no fresh chain observation. Use the post-probe ordinary ceiling;
        // request admission applies the authoritative pre-probe clamp immediately after its required
        // chain read.
        self.admission_ceiling(true)
            .saturating_sub(self.reserved_billable_tokens.load(Ordering::SeqCst))
    }

    /// The contract admits at most its canonical trial tick until `acceptProbe` has been observed
    /// and this route's anchor/ceiling cutover is complete. The buyer enforces that cap independently
    /// of the seller's capacity recorder, including while the acceptance snapshot is in flight.
    fn admission_ceiling(&self, probe_accepted: bool) -> u64 {
        // The canonical trial tick as a token count. `TICK_SIZE` is a `u128` only because the
        // contract's cumulative counters are; the canon value fits a `u64` counter exactly, and the
        // assertion is what keeps that a fact rather than an assumption.
        const PROBE_TICK_TOKENS: u64 = dexdo_core::TICK_SIZE as u64;
        const _: () = assert!(PROBE_TICK_TOKENS as u128 == dexdo_core::TICK_SIZE);

        if self
            .weekly
            .as_ref()
            .is_some_and(|weekly| weekly.anchored_before_probe())
        {
            // Flat `TICK_SIZE`, and deliberately NOT `min(TICK_SIZE, published)`: funding refuses
            // anything under two ticks (`TokenContract.sol:453`, `paid - bond < 2 * unit`) and a
            // subscription's volume is additionally a whole number of weeks of ticks, so the
            // smallest legal shape is four ticks over four weeks and the pre-acceptance ceiling is
            // never below one tick. That `min` could not take its second branch on any reachable
            // deal, and a clamp that cannot fire reads as a protection while being none.
            return PROBE_TICK_TOKENS;
        }
        let published = self.token_ceiling.load(Ordering::SeqCst);
        if self.weekly.is_none() && !probe_accepted {
            // Before the trial tick is accepted, the seller's authoritative exposure ceiling is one
            // tick. Sending the ordinary route's larger funded remainder as a billing grant would be
            // rejected before the very request that makes probe acceptance reachable.
            return published.min(PROBE_TICK_TOKENS);
        }
        published
    }

    /// Reserve the route's entire free billable remainder atomically. Provider input is unknown until
    /// terminal usage, so concurrent requests on one deal cannot safely share this reservation.

    /// The full grant is needed because input cannot be predicted from `max_tokens`. The verification
    /// floor still prevents consuming the last remainder on a probe that cannot be followed by an
    /// answer. A compare-exchange loser recomputes both floor and free capacity; while the winner holds
    /// the full remainder, every other request is refused, providing the one-in-flight invariant.
    fn try_reserve(&self, probe_accepted: bool) -> Option<RouteReservation> {
        let mut reserved = self.reserved_billable_tokens.load(Ordering::SeqCst);
        loop {
            let ceiling = self.admission_ceiling(probe_accepted);
            let free = ceiling.saturating_sub(reserved);
            let floor = self.content_gate.outstanding_verification_tokens();
            let granted = free;
            if granted <= floor {
                return None;
            }
            match self.reserved_billable_tokens.compare_exchange(
                reserved,
                reserved.saturating_add(granted),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(RouteReservation {
                        reserved: self.reserved_billable_tokens.clone(),
                        granted,
                        used: 0,
                    })
                }
                Err(observed) => reserved = observed,
            }
        }
    }

    /// The pre-request admission gate: reserve the route's free billable remainder, or say why not.

    /// An ordinary deal exposes one tick until the authoritative open-state read observes an accepted
    /// probe, then answers from its fixed funded budget. A subscription reconciles against the chain
    /// first whenever its published week is spent OR has run out on the wall clock -- the second is what
    /// stops an under-used week being carried across its boundary, and what makes the end of the term
    /// reachable at all rather than leaving a stale positive remainder servable forever.
    /// Reconciliation books the boundary through the permissionless path and recomputes from the
    /// coherent state that comes back; a booking that is not due, or a read that fails, authorizes
    /// nothing.
    #[cfg(test)]
    async fn admit(&self, requested: Option<u32>) -> RouteBudget {
        self.admit_with_probe_observation(requested, true).await
    }

    /// Admit using the probe fact returned by the handler's mandatory authoritative open-state read.
    async fn admit_with_probe_observation(
        &self,
        _requested: Option<u32>,
        probe_accepted: bool,
    ) -> RouteBudget {
        // Provider-native input usage is not known before the terminal record. The admitted request therefore
        // holds the whole free route remainder as its total billing grant; its canonical `max_tokens` remains
        // an independent output-only cap. The reservation guard returns every unused token at request end.
        let Some(weekly) = self.weekly.as_ref() else {
            return match self.try_reserve(probe_accepted) {
                Some(reservation) => RouteBudget::Admitted(reservation),
                None => RouteBudget::Exhausted(self.exhausted_reason()),
            };
        };
        if weekly.is_terminal() {
            return RouteBudget::Exhausted(format!(
                "subscription {} is terminal; the contract admits no further claim",
                display_token_contract(&self.route.token_contract)
            ));
        }
        // A route anchored before the trial tick was accepted must ask the chain: acceptance moves
        // the cumulative claim its local zero is measured from without moving `weekIndex`, so the
        // cached ceiling is a tick too generous until an authoritative read rebases it.
        if self.remaining_tokens() > 0
            && !weekly.published_week_ran_out()
            && !weekly.anchored_before_probe()
        {
            if let Some(reservation) = self.try_reserve(probe_accepted) {
                return RouteBudget::Admitted(reservation);
            }
        }
        let _guard = weekly.refresh_lock.lock().await;
        // The reconciliation that held the lock may already have published this week's ceiling.
        if self.remaining_tokens() > 0
            && !weekly.published_week_ran_out()
            && !weekly.anchored_before_probe()
        {
            return match self.try_reserve(probe_accepted) {
                Some(reservation) => RouteBudget::Admitted(reservation),
                None => RouteBudget::Exhausted(self.exhausted_reason()),
            };
        }
        match weekly
            .reconcile(&self.token_ceiling, &self.billable_tokens)
            .await
        {
            Ok(()) => match self.try_reserve(probe_accepted) {
                Some(reservation) => RouteBudget::Admitted(reservation),
                None => RouteBudget::Exhausted(self.exhausted_reason()),
            },
            Err(reason) => RouteBudget::Exhausted(reason),
        }
    }

    /// Why [`Self::try_reserve`] handed nothing out, in the operator's terms. A deal that still owes
    /// its identity verification can be refused with budget left on the route, so the two
    /// cases do not share one sentence: "exhausted" would send an operator looking for a spent deal
    /// when the remainder is simply too small to verify and answer out of.
    fn exhausted_reason(&self) -> String {
        if self.content_gate.outstanding_verification_tokens() > 0 {
            UNVERIFIED_BUDGET_CANNOT_COVER_VERIFICATION.to_string()
        } else {
            ORDINARY_BUDGET_EXHAUSTED.to_string()
        }
    }

    /// Account one terminal input+output total without crossing the published route ceiling. Reached
    /// only through [`ConsumerRequestGuard::record_billing`], which validates the request's held
    /// reservation first and commits that reservation only after this atomic update succeeds.
    fn record_billable(&self, n: u64) -> Result<(), String> {
        let ceiling = self.token_ceiling.load(Ordering::SeqCst);
        self.billable_tokens
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |billable| {
                billable.checked_add(n).filter(|next| *next <= ceiling)
            })
            .map(|_| ())
            .map_err(|billable| match billable.checked_add(n) {
                Some(next) => format!(
                    "terminal usage would raise cumulative billing from {billable} to {next} \
                     tokens above the currently published route ceiling {ceiling}"
                ),
                None => "cumulative billable-token accounting overflow".to_string(),
            })
    }

    /// Record output immediately before a streaming adapter yields it to its consumer.
    pub fn record_accepted_output(&self, now_secs: u64) {
        self.last_accepted_output_unix_secs
            .fetch_max(now_secs, Ordering::SeqCst);
        self.accepted_output_generation
            .fetch_add(1, Ordering::SeqCst);
    }

    pub fn last_accepted_output_unix_secs(&self) -> u64 {
        self.last_accepted_output_unix_secs.load(Ordering::SeqCst)
    }

    pub fn accepted_output_generation(&self) -> u64 {
        self.accepted_output_generation.load(Ordering::SeqCst)
    }

    pub fn accepted_output_guard(&self) -> dexdo_core::market::HeartbeatGuard {
        dexdo_core::market::HeartbeatGuard::new(self.accepted_output_generation.clone())
    }

    pub(crate) fn begin_request(&self, now_secs: u64) -> ConsumerRequestGuard {
        self.last_request_started_unix_secs
            .store(now_secs, Ordering::SeqCst);
        self.active_requests.fetch_add(1, Ordering::SeqCst);
        ConsumerRequestGuard {
            active_requests: self.active_requests.clone(),
            accepted_output_generation: self.accepted_output_generation.clone(),
            session: self.session.clone(),
            failure_heartbeat: None,
            reservation: None,
        }
    }

    pub fn has_active_request(&self) -> bool {
        self.active_requests.load(Ordering::SeqCst) > 0
    }

    pub fn has_active_or_recent_request(&self, now_secs: u64, recent_window_secs: u64) -> bool {
        if self.has_active_request() {
            return true;
        }
        let last = self.last_request_started_unix_secs.load(Ordering::SeqCst);
        last != 0 && now_secs.saturating_sub(last) <= recent_window_secs
    }
}

pub(crate) struct ConsumerRequestGuard {
    active_requests: Arc<AtomicU64>,
    accepted_output_generation: Arc<AtomicU64>,
    session: Arc<SessionSettle>,
    failure_heartbeat: Option<dexdo_core::market::HeartbeatGuard>,
    /// This request's whole free billing remainder. Dropped with the guard, which returns whatever
    /// terminal input+output usage did not consume.
    reservation: Option<RouteReservation>,
}

impl ConsumerRequestGuard {
    fn hold(&mut self, reservation: RouteReservation) {
        self.reservation = Some(reservation);
    }

    fn remaining_grant(&self) -> u64 {
        self.reservation
            .as_ref()
            .map(RouteReservation::remaining)
            .unwrap_or(0)
    }

    /// Account one validated terminal input+output total against both the route and this request's
    /// held billing reservation. Content chunks never call this money transition.
    fn record_billing(&mut self, deal: &ApiDeal, billable: u64) -> Result<(), String> {
        // The existing anchor mutex is the acceptance cutover mutex. Every subscription chunk takes
        // it, including chunks from old reservations after the pre-probe flag has been cleared: a
        // chunk either commits before the cutover samples delivery, or validates the new ceiling.
        let _cutover = match deal.weekly.as_ref() {
            Some(weekly) => Some(weekly.claim_anchor.lock().map_err(|_| {
                format!(
                    "subscription {}: acceptance cutover lock is poisoned; refusing billing",
                    display_token_contract(&deal.route.token_contract)
                )
            })?),
            None => None,
        };
        let reservation = self.reservation.as_mut().ok_or_else(|| {
            "accepted billing usage has no admitted route reservation".to_string()
        })?;
        let next_used = reservation.checked_used_after(billable)?;
        deal.record_billable(billable)?;
        reservation.used = next_used;
        Ok(())
    }

    pub(crate) fn arm_upstream_failure(&mut self) {
        self.failure_heartbeat = Some(dexdo_core::market::HeartbeatGuard::new(
            self.accepted_output_generation.clone(),
        ));
    }

    pub(crate) fn complete(&mut self) {
        self.failure_heartbeat = None;
    }
}

impl Drop for ConsumerRequestGuard {
    fn drop(&mut self) {
        self.active_requests.fetch_sub(1, Ordering::SeqCst);
        if self
            .failure_heartbeat
            .as_ref()
            .is_some_and(dexdo_core::market::HeartbeatGuard::unchanged)
        {
            self.session.close_recovery_episode(
                RecoveryKind::ReclaimOpened,
                "consumer-request-ended-before-accepted-output",
            );
        }
    }
}

/// Mutable active deal for a long-running local API service.

/// Single-shot/legacy callers build one deal and never replace it. The buyer continuity monitor can prepare a
/// next handover first, settle a delivering old session, then atomically publish that next deal. A route that
/// delivered nothing is dropped without a chain write. That keeps the local OpenAI/Anthropic endpoint alive
/// across deal boundaries without serving a request on a closed TC.
pub struct RouteManager {
    active: RwLock<Option<ApiDeal>>,
    initializer: Option<DealInitializer>,
    replace_settled: bool,
    initializer_timeout: Duration,
    initializer_lock: Mutex<()>,
}

impl RouteManager {
    pub fn new(active: ApiDeal) -> Self {
        Self {
            active: RwLock::new(Some(active)),
            initializer: None,
            replace_settled: false,
            initializer_timeout: MATCH_OPEN_TIMEOUT,
            initializer_lock: Mutex::new(()),
        }
    }

    pub fn lazy(initializer: DealInitializer, initializer_timeout: Duration) -> Self {
        Self {
            active: RwLock::new(None),
            initializer: Some(initializer),
            replace_settled: false,
            initializer_timeout,
            initializer_lock: Mutex::new(()),
        }
    }

    pub fn recoverable_lazy(initializer: DealInitializer, initializer_timeout: Duration) -> Self {
        Self {
            active: RwLock::new(None),
            initializer: Some(initializer),
            replace_settled: true,
            initializer_timeout,
            initializer_lock: Mutex::new(()),
        }
    }

    pub fn recoverable_lazy_with_active(
        active: ApiDeal,
        initializer: DealInitializer,
        initializer_timeout: Duration,
    ) -> Self {
        Self {
            active: RwLock::new(Some(active)),
            initializer: Some(initializer),
            replace_settled: true,
            initializer_timeout,
            initializer_lock: Mutex::new(()),
        }
    }

    pub fn is_lazy(&self) -> bool {
        self.initializer.is_some()
    }

    pub async fn current(&self) -> Option<ApiDeal> {
        self.active.read().await.clone()
    }

    pub async fn current_or_prepare(&self) -> Result<ApiDeal, DealInitError> {
        if let Some(active) = self.current().await {
            if !self.replace_settled || !active.session.is_settled() {
                return Ok(active);
            }
        }
        let _guard = self.initializer_lock.lock().await;
        if let Some(active) = self.current().await {
            if !self.replace_settled || !active.session.is_settled() {
                return Ok(active);
            }
        }
        let initializer = self
            .initializer
            .as_ref()
            .ok_or_else(|| DealInitError::new("consumer API has no active deal"))?;
        let prepared = tokio::time::timeout(self.initializer_timeout, initializer())
            .await
            .map_err(|_| {
                DealInitError::new(format!(
                    "on-demand purchase timed out after {}s before a deal became ready",
                    self.initializer_timeout.as_secs()
                ))
            })??;
        *self.active.write().await = Some(prepared.clone());
        Ok(prepared)
    }

    pub async fn replace_active(
        &self,
        next: impl FnOnce() -> ApiDeal,
        reason: &str,
    ) -> Result<(), dexdo_core::ChainError> {
        let mut active = self.active.write().await;
        if let Some(previous) = active.as_ref() {
            if previous.session.implicit_terminal_lacks_delivery() {
                previous.session.close_local_api();
                previous.session.disable_drop_backup();
                tracing::warn!(
                    %reason,
                    token_contract = %display_token_contract(&previous.route.token_contract),
                    delivered_tokens = 0_u64,
                    chain_write_submitted = false,
                    "consumer API: route swap dropped the previous route without settlement because it \
                     delivered no tokens"
                );
            } else {
                previous.session.settle(reason).await?;
            }
        }
        *active = Some(next());
        Ok(())
    }

    pub async fn settle_active(&self, reason: &str) -> Result<bool, dexdo_core::ChainError> {
        let Some(active) = self.current().await else {
            return Ok(false);
        };
        active.session.settle(reason).await
    }

    async fn settle_active_on_exit(&self, reason: &str) -> Result<bool, dexdo_core::ChainError> {
        let Some(active) = self.current().await else {
            return Ok(false);
        };
        active.session.settle_on_exit(reason).await
    }
}

/// Stable v2 output-delivery count for one accepted content chunk. Structured token ids retain
/// their exact count; an accepted chunk without them retains the pre- one-token fallback.
pub(crate) fn accounted_tokens(chunk: &CanonChunk) -> u64 {
    (chunk.token_ids.len() as u64).max(1)
}

/// What one finished consumer request rendered and, on clean protocol completion, was billed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDelivery {
    /// The deal this request was served on.
    pub token_contract: TokenContract,
    /// Which consumer protocol rendered the answer: `openai` or `anthropic`.
    pub protocol: &'static str,
    /// `true` for SSE, `false` for the aggregated single response.
    pub streamed: bool,
    /// Total input+output capacity held for the request and sent on the wire.
    pub billing_grant_tokens: u64,
    /// Provider output cap sent independently in `CanonRequest.params.max_tokens`.
    pub output_limit_tokens: u64,
    /// Visible output-token lower bound rendered to the local caller.
    pub visible_output_tokens: u64,
    /// The request's stable v2 output-delivery count (`max(token_ids.len(), 1)` per accepted
    /// content chunk), retained independently of provider-native billing usage.
    pub rendered_tokens: u64,
    /// The deal's cumulative stable v2 output-delivery count after this request. Like
    /// `rendered_tokens`, it adds `max(token_ids.len(), 1)` for every accepted content chunk,
    /// including metadata-only chunks, and remains independent of provider-native billing usage.
    pub route_delivered_tokens: Option<u64>,
    /// Provider-native input usage, present only after a clean accepted terminal record.
    pub input_tokens: Option<u64>,
    /// Provider-native output usage, present only after a clean accepted terminal record.
    pub output_tokens: Option<u64>,
    /// Provider-native checked input+output usage charged for this request.
    pub billable_tokens: Option<u64>,
    /// The deal's cumulative accepted billable usage after this request.
    pub route_billable_tokens: Option<u64>,
    /// The terminal value that went out on the wire
    /// (`stop`/`length`/`capacity`/`error`/`content_filter`, or
    /// `end_turn`/`max_tokens`/`error`/`refusal` on the Anthropic transcode).
    pub finish_reason: &'static str,
    /// The render stopped because the next content chunk did not fit the remaining output cap. Reaching
    /// the cap exactly while the seller then ends cleanly does not by itself mark the answer truncated.
    pub truncated_by_output_limit: bool,
    /// The stream ended with part of the output cap unspent. On its own this is not a fault - a model
    /// that finishes early does exactly this - but it is also the shape a stream that dies in
    /// flight has, and without the figure on record the two cannot be told apart afterwards.
    pub ended_before_output_limit: bool,
}

/// Which cumulative chain boundary a buyer-side delivery measurement observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyerClaimObservationKind {
    /// The trial tick became the first cumulative claim.
    Probe,
    /// `tokensPending` advanced beyond the probe or the first available snapshot already contained a later
    /// claim. No historical probe event is fabricated in the latter case.
    Claim,
}

/// Buyer-billed cumulative usage sampled immediately after a fresh chain high-water is read.

/// The chain high-water is the join key for the seller's `claim_submitted` event. The buyer counter comes
/// straight from the active [`ApiDeal`]'s bound [`SessionSettle`] counter, so the event does not re-derive
/// tokens from bytes, frames, or text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuyerClaimObservation {
    pub token_contract: TokenContract,
    pub kind: BuyerClaimObservationKind,
    pub cumulative_tokens: u128,
    pub last_claim_time: u64,
    pub route_delivered_tokens: Option<u64>,
    pub route_billable_tokens: Option<u64>,
}

impl BuyerClaimObservation {
    pub const fn event_name(&self) -> &'static str {
        match self.kind {
            BuyerClaimObservationKind::Probe => "probe_observed",
            BuyerClaimObservationKind::Claim => "claim_observed",
        }
    }

    /// Existing buyer-event vocabulary and amount encoding, without any secret-bearing route material.
    pub fn event_fields(&self) -> serde_json::Value {
        serde_json::json!({
            "token_contract": self.token_contract,
            "cumulative_tokens": self.cumulative_tokens.to_string(),
            "last_claim_time": self.last_claim_time,
            "route_delivered_tokens": self.route_delivered_tokens.map(|value| value.to_string()),
            "route_billable_tokens": self.route_billable_tokens.map(|value| value.to_string()),
        })
    }
}

/// Per-active-route high-water cursor for the existing buyer monitor cadence.
#[derive(Debug, Default)]
pub struct BuyerClaimObservationCursor {
    token_contract: Option<TokenContract>,
    probe_accepted: bool,
    tokens_pending: u128,
}

impl BuyerClaimObservationCursor {
    /// Return one event only when the authoritative cumulative high-water advances. Reading the route counter
    /// happens in this call, immediately after the chain state read that supplied `state`.
    pub fn observe(
        &mut self,
        deal: &ApiDeal,
        state: dexdo_core::DealChainState,
    ) -> Option<BuyerClaimObservation> {
        let token_contract = &deal.route.token_contract;
        if self.token_contract.as_ref() != Some(token_contract) {
            self.token_contract = Some(token_contract.clone());
            self.probe_accepted = false;
            self.tokens_pending = 0;
        }

        let previous_probe_accepted = self.probe_accepted;
        let previous_tokens_pending = self.tokens_pending;
        self.probe_accepted |= state.probe_accepted;
        self.tokens_pending = self.tokens_pending.max(state.tokens_pending);

        if !state.probe_accepted
            || (previous_probe_accepted && state.tokens_pending <= previous_tokens_pending)
        {
            return None;
        }

        let kind = if !previous_probe_accepted && state.tokens_pending == dexdo_core::TICK_SIZE {
            BuyerClaimObservationKind::Probe
        } else {
            BuyerClaimObservationKind::Claim
        };
        Some(BuyerClaimObservation {
            token_contract: token_contract.clone(),
            kind,
            cumulative_tokens: state.tokens_pending,
            last_claim_time: state.last_claim_time,
            route_delivered_tokens: Some(deal.route_visible_output_tokens()),
            route_billable_tokens: deal.session.route_billable_tokens(),
        })
    }
}

/// Where [`RequestDelivery`] records go. Unbounded and non-blocking by construction: the library
/// never makes a paid stream wait on the operator's output surface, and a consumer that is not
/// draining must never be able to stall a render. The CLI owns the JSONL surface, so the counts
/// travel to it rather than the library learning to print.
pub type DeliveryEvents = tokio::sync::mpsc::UnboundedSender<RequestDelivery>;

/// Publish one finished request's delivery record. Always traced; forwarded to the machine
/// surface when one is attached. A closed receiver is not an error - the operator simply stopped
/// listening, and a paid stream must not fail because of it.
pub(crate) fn report_request_delivery(events: Option<&DeliveryEvents>, delivery: RequestDelivery) {
    tracing::info!(
        token_contract = %display_token_contract(&delivery.token_contract),
        protocol = delivery.protocol,
        streamed = delivery.streamed,
        grant_tokens = delivery.output_limit_tokens,
        rendered_tokens = delivery.rendered_tokens,
        route_delivered_tokens = delivery.route_delivered_tokens,
        truncated_by_grant = delivery.truncated_by_output_limit,
        ended_before_grant = delivery.ended_before_output_limit,
        billing_grant_tokens = delivery.billing_grant_tokens,
        output_limit_tokens = delivery.output_limit_tokens,
        visible_output_tokens = delivery.visible_output_tokens,
        input_tokens = delivery.input_tokens,
        output_tokens = delivery.output_tokens,
        billable_tokens = delivery.billable_tokens,
        route_billable_tokens = delivery.route_billable_tokens,
        finish_reason = delivery.finish_reason,
        truncated_by_output_limit = delivery.truncated_by_output_limit,
        ended_before_output_limit = delivery.ended_before_output_limit,
        "consumer API: request delivery"
    );
    if let Some(events) = events {
        let _ = events.send(delivery);
    }
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
            let heartbeat = deal.accepted_output_guard();
            deal.session
                .settle_dead_gateway("stream-error-before-token", &heartbeat)
                .await;
        }
        StreamErrorPolicyAction::SellerStallsMidStream => {
            let heartbeat = deal.accepted_output_guard();
            deal.session
                .settle_seller_stalls_mid_stream("seller-stalls-mid-stream", &heartbeat)
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

/// A stream that never opened because the seller ANSWERED with its canonical capacity refusal is
/// request-scoped, exactly like a 4xx above -- not a dead gateway.

/// This covers both existing capacity boundaries: until `acceptProbe` lands the authoritative cap is
/// one canonical trial tick, and afterwards a new request is refused while the seller's durable
/// unclaimed billable backlog is already one tick. In either case the seller is reachable,
/// authorized and correct. Treating that answer as `dead_gateway` could destroy a healthy deal; the
/// caller is told to retry after capacity reconciliation and the chain is not touched here.
pub(crate) fn is_capacity_backpressure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tonic::Status>()
        .is_some_and(|status| status.code() == tonic::Code::ResourceExhausted)
}

/// Content-identity check selected for a deal. The buyer pays for a model by NAME (B2); a seller
/// declaring the correct name but serving a cheaper model is caught only by the **content** layers B8
/// ([`Buyer::behavioral_probe`]) + B7-full ([`Buyer::reference_spotcheck`]). `Skip` -- no content
/// fingerprint/reference for the exact model id (degradation R3, name-only) or the mock path; `Probe` -- run the
/// content gate once for that exact/reference model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentCheck {
    /// No content-identity layer applies (mock, or no B8 fingerprint and no B7 reference/key) -- name-only.
    Skip,
    /// Run the one-per-deal content gate (B8 + B7-full) for this exact/reference model id.
    Probe { model_id: String },
}

/// decide the content-identity policy for a frame model BEFORE paying/serving. **Shared by BOTH
/// buyer paths** -- the consumer API (Path B, `cli`) and the gateway routing runner (Path A, `buyer::routing`) --
/// so neither silently pays a model that no content layer can verify. Pure w.r.t. env (`has_ref_key` is computed
/// by the caller). A seller can declare the correct model NAME yet serve a cheaper model; only the CONTENT layers
/// (B8 fingerprint / B7-full reference) catch that, and they are **data-driven** from `models`. Policy:
/// - `mock_model` -> `Skip`;
/// - a model id with a B8 fingerprint OR a B7 reference key in env -> `Probe` (run the content gate);
/// - otherwise `allow_unverified` -> `Skip` with a loud warning (name-only, operator opted in);
/// - else **refuse** (fail closed): buying/paying on name-only evidence is rejected.
pub fn content_check_policy(
    frame_model: &str,
    identity_model: Option<&str>,
    mock_model: bool,
    allow_unverified: bool,
    has_ref_key: bool,
    models: &ModelsConfig,
) -> Result<ContentCheck> {
    let family = crate::buyer::verify::family_of(frame_model);
    let identity_model = identity_model.unwrap_or(frame_model);
    if mock_model {
        return Ok(ContentCheck::Skip);
    }
    let has_b8 = crate::buyer::verify::default_probe(identity_model, models).is_some();
    if has_b8 || has_ref_key {
        return Ok(ContentCheck::Probe {
            model_id: identity_model.to_string(),
        });
    }
    if allow_unverified {
        tracing::warn!(
            %frame_model, %family,
            "CONTENT IDENTITY UNVERIFIED (name-only): no B8 fingerprint and no B7 reference/key for this \
             exact model -- proceeding on name-only evidence (--allow-unverified-model). A seller serving a cheaper \
             model under this name cannot be caught by content checks on this deal."
        );
        return Ok(ContentCheck::Skip);
    }
    anyhow::bail!(
        "model `{frame_model}` (family `{family}`) has no exact content-identity check (no B8 fingerprint, no B7 \
         reference/key); refusing to buy on name-only evidence. Pass --allow-unverified-model to accept \
         name-only identity."
    )
}

/// One-per-deal content-identity gate. The inline [`StreamVerifier`](crate::buyer::verify::StreamVerifier)
/// on the consumer-API path only runs B5/B6 + the cheap declared-NAME B7; the strong **content** layers (B8
/// fingerprint + B7-full reference spot-check) were never invoked there, so a seller serving a cheaper model
/// under the correct NAME was paid undetected. This gate runs those layers ONCE, before the first paid stream on
/// each renderer, and caches the **definitive** verdict so later requests do not re-probe. A transport error is
/// NOT cached (the next request retries). On a bail the deal is closed to later requests and policy recovery is
/// attempted.
pub struct ContentGate {
    check: ContentCheck,
    /// Loaded model config -- needed for the `Probe` path (B8 fingerprints / B7-full reference are
    /// data-driven per model). `None` for `Skip` (which never probes).
    models: Option<Arc<ModelsConfig>>,
    /// Cached definitive verdict: `Ok(())` pass, `Err(reason)` bail. A transport error is the cell's init error
    /// (NOT stored) so the gate retries on the next request.
    verdict: OnceCell<Result<(), String>>,
}

fn verification_stream_limits(billing_grant_tokens: u64) -> (u64, u64) {
    (
        CONTENT_PROBE_MAX_TOKENS.min(billing_grant_tokens),
        billing_grant_tokens,
    )
}

impl ContentGate {
    pub fn new(check: ContentCheck, models: Arc<ModelsConfig>) -> Self {
        Self {
            check,
            models: Some(models),
            verdict: OnceCell::new(),
        }
    }

    /// A gate that performs no content check (mock / name-only degradation). Needs no config.
    pub fn skip() -> Self {
        Self {
            check: ContentCheck::Skip,
            models: None,
            verdict: OnceCell::new(),
        }
    }

    /// A gate that runs the content probe for `model_id`, using `models` for its data-driven
    /// fingerprint/reference.
    pub fn probe(model_id: String, models: Arc<ModelsConfig>) -> Self {
        Self {
            check: ContentCheck::Probe { model_id },
            models: Some(models),
            verdict: OnceCell::new(),
        }
    }

    /// Output headroom this deal still owes to its one-per-deal identity verification.

    /// This is the unclampable output FLOOR of the admitting request's reservation
    /// ([`ApiDeal::try_reserve`]): a request that cannot hold it and still deliver an answer is
    /// refused rather than admitted for less. It is the ceiling of what verification can cost -- the
    /// B8 fingerprint probe and then the B7-full reference spot-check, each capped at
    /// `CONTENT_PROBE_MAX_TOKENS`. Provider-native input is deliberately not predicted here: each
    /// accepted probe charges its checked terminal input+output total against the held billing grant,
    /// and fails closed if that total does not fit. Zero once a definitive
    /// verdict is cached: the gate never probes twice, so its admission floor becomes zero. Each
    /// later request still reserves the route's whole free billable remainder while provider-native
    /// input is unknown.
    pub(crate) fn outstanding_verification_tokens(&self) -> u64 {
        match &self.check {
            ContentCheck::Skip => 0,
            ContentCheck::Probe { .. } if self.verdict.get().is_some() => 0,
            ContentCheck::Probe { .. } => CONTENT_PROBE_MAX_TOKENS.saturating_mul(2),
        }
    }

    /// Run the content-identity gate once per deal. `Skip` -> `Ok(())`. `Probe` -> run B8 then B7-full
    /// ONCE (cached): the cached `Ok(())`/`Err(reason)` is the definitive verdict (pass/bail); a transport error
    /// is propagated as `Err` WITHOUT being cached, so the next request retries. On a bail the deal is closed
    /// to new requests before the verdict is cached and returned.

    /// verification spends the CALLER'S held reservation and nothing else. Each clean
    /// probe terminal charges one complete input+output total; content without such a terminal charges
    /// zero. Dropping the handler while B7 is pending cannot return B8's already accepted terminal bill.
    /// `OnceCell` waiters do not run the
    /// initializer, so only the request whose guard is passed into that initializer pays.
    pub(crate) async fn ensure_verified(
        &self,
        buyer: &Buyer,
        deal: &ApiDeal,
        request_guard: &mut ConsumerRequestGuard,
    ) -> Result<(), String> {
        match &self.check {
            ContentCheck::Skip => Ok(()),
            ContentCheck::Probe { model_id } => {
                let Some(models) = self.models.as_deref() else {
                    // A Probe gate is only built via `new`/`probe`, both of which carry the config; a missing
                    // config is a construction bug -- fail closed rather than silently pass a content check.
                    return Err(
                        "content gate: Probe selected without a loaded models config".to_string(),
                    );
                };
                // OUTER Err = transport error (NOT cached -> retried next request); INNER `Result<(), String>` =
                // the cached definitive verdict (`Ok(())` pass, `Err(reason)` bail).
                let cached: &Result<(), String> = self
                    .verdict
                    .get_or_try_init::<String, _, _>(|| async {
                        // B8 content fingerprint. The `?` makes a transport error the OUTER Err (not cached);
                        // a definitive verdict goes through `Ok(...)`.
                        let (b8_output_limit, b8_grant) =
                            verification_stream_limits(request_guard.remaining_grant());
                        let v8 = {
                            let mut charge = |tokens| request_guard.record_billing(deal, tokens);
                            let mut record_output = |tokens| deal.record_visible_output(tokens);
                            buyer
                                .behavioral_probe(
                                    &deal.route.handover,
                                    &deal.route.token_contract,
                                    model_id,
                                    b8_output_limit,
                                    b8_grant,
                                    models,
                                    Some(&mut charge),
                                    Some(&mut record_output),
                                )
                                .await
                                .map_err(|e| e.to_string())?
                        };
                        if let Verdict::Bail(r) = v8 {
                            deal.session
                                .settle_verification_bail("content-identity-bail")
                                .await;
                            return Ok(Err(r));
                        }
                        // B7-full reference spot-check (greedy vs the official endpoint).
                        let (b7_output_limit, b7_grant) =
                            verification_stream_limits(request_guard.remaining_grant());
                        let v7 = {
                            let mut charge = |tokens| request_guard.record_billing(deal, tokens);
                            let mut record_output = |tokens| deal.record_visible_output(tokens);
                            buyer
                                .reference_spotcheck(
                                    &deal.route.handover,
                                    &deal.route.token_contract,
                                    model_id,
                                    b7_output_limit,
                                    b7_grant,
                                    models,
                                    Some(&mut charge),
                                    Some(&mut record_output),
                                )
                                .await
                                .map_err(|e| e.to_string())?
                        };
                        if let Verdict::Bail(r) = v7 {
                            deal.session
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
    /// The configured market/frame model id -- the only one that is served (B2/B19).
    /// The request's `model` field is checked against it; outside the frame -- reject.
    pub frame_model: String,
    /// Locally trusted explicit `--mock-model` mode; never inferred from seller-supplied manifest data.
    pub mock_model: bool,
    /// Active deal slot. A one-shot service never replaces it; continuous service mode may publish the next
    /// already-opened handover here while keeping the local HTTP listener alive.
    pub deals: Arc<RouteManager>,
    /// Where each finished request's [`RequestDelivery`] goes. `None` leaves the record on
    /// the tracing surface only, which is what a test or an embedder that has no machine output
    /// wants; the CLI attaches its JSONL writer here.
    pub delivery_events: Option<DeliveryEvents>,
}

/// Session-scoped deal settlement. The consumer endpoint serves
/// ONE deal (`route.token_contract`) across MANY requests; the deal is STOPped **once at session end**, not per
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
    closed_tx: watch::Sender<bool>,
    recovery_episode: AtomicU8,
    recovery_closed_session: AtomicBool,
    handler_recovery_reconciliation: AtomicBool,
    recovery_submit_may_have_landed: AtomicBool,
    drop_backup_enabled: AtomicBool,
    settle_lock: Mutex<()>,
    failure_policy: BuyerApiFailurePolicy,
    lifetime: SessionLifetimePolicy,
    terminal_action: AtomicU8,
    /// The consumer route's cumulative stable v2 output-delivery witness: each accepted content
    /// chunk adds `max(token_ids.len(), 1)`, including metadata-only chunks. This retains the
    /// implicit-terminal veto and the buyer-event v2 `route_delivered_tokens` meaning.
    route_delivery: OnceLock<Arc<AtomicU64>>,
    /// The consumer route's own cumulative accepted billable-usage counter, shared by
    /// [`ApiDeal::new`].
    /// It is the SAME `Arc` the route accounts against, never a copy, so this witness cannot drift
    /// from the figure the money path actually charged. `None` for a session that serves no consumer
    /// route (the one-shot terminal), which has no route-billing model here at all.
    route_billing: OnceLock<Arc<AtomicU64>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifetimePolicy {
    SettleOnExit,
    Preserve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTerminalAction {
    StreamStop,
    StreamDispute,
    StreamCleanup,
    ObservedTerminal,
    UnknownCloser,
}

impl SessionTerminalAction {
    const fn code(self) -> u8 {
        match self {
            Self::StreamStop => 1,
            Self::StreamDispute => 2,
            Self::StreamCleanup => 3,
            Self::ObservedTerminal => 4,
            Self::UnknownCloser => 5,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::StreamStop),
            2 => Some(Self::StreamDispute),
            3 => Some(Self::StreamCleanup),
            4 => Some(Self::ObservedTerminal),
            5 => Some(Self::UnknownCloser),
            _ => None,
        }
    }

    pub const fn event_action(self) -> &'static str {
        match self {
            Self::StreamStop => "streamStop",
            Self::StreamDispute => "streamDispute",
            Self::StreamCleanup => "streamCleanup",
            Self::ObservedTerminal => "observedTerminal",
            Self::UnknownCloser => "terminalCloserUnknown",
        }
    }

    pub const fn event_state(self) -> &'static str {
        match self {
            Self::StreamStop | Self::StreamCleanup => "stopped",
            Self::StreamDispute => "disputed",
            Self::ObservedTerminal | Self::UnknownCloser => "terminal",
        }
    }

    pub const fn chain_write_submitted(self) -> bool {
        !matches!(self, Self::ObservedTerminal)
    }

    fn from_stop_settlement(settlement: &dexdo_core::Settlement) -> Self {
        match settlement {
            dexdo_core::Settlement::BuyerStopTerminal(receipt) => match receipt.fact {
                dexdo_core::BuyerStopTerminalFact::SubmittedStop => Self::StreamStop,
                dexdo_core::BuyerStopTerminalFact::AlreadyClosed => Self::ObservedTerminal,
                dexdo_core::BuyerStopTerminalFact::UnknownCloser => Self::UnknownCloser,
            },
            _ => Self::StreamStop,
        }
    }
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
        Self::new_with_failure_policy_and_lifetime(
            chain,
            token_contract,
            note,
            failure_policy,
            SessionLifetimePolicy::SettleOnExit,
        )
    }

    pub fn new_with_failure_policy_and_lifetime(
        chain: Arc<dyn ChainBackend>,
        token_contract: TokenContract,
        note: Arc<dyn Note>,
        failure_policy: BuyerApiFailurePolicy,
        lifetime: SessionLifetimePolicy,
    ) -> Self {
        let (closed_tx, _closed_rx) = watch::channel(false);
        Self {
            chain,
            token_contract,
            note,
            settled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closed_tx,
            recovery_episode: AtomicU8::new(0),
            recovery_closed_session: AtomicBool::new(false),
            handler_recovery_reconciliation: AtomicBool::new(false),
            recovery_submit_may_have_landed: AtomicBool::new(false),
            drop_backup_enabled: AtomicBool::new(true),
            settle_lock: Mutex::new(()),
            failure_policy,
            lifetime,
            terminal_action: AtomicU8::new(0),
            route_delivery: OnceLock::new(),
            route_billing: OnceLock::new(),
        }
    }

    /// Bind this session to the billable-token counter of the consumer route it settles.
    /// Called once by [`ApiDeal::new`]; a later route gets its own session, so the first binding is
    /// the only one and a repeated call is ignored rather than silently repointing the witness.
    fn bind_route_billing(&self, billable_tokens: Arc<AtomicU64>) {
        let _ = self.route_billing.set(billable_tokens);
    }

    /// Bind the separate output-only delivery witness used by the stable v2 compatibility surface
    /// and the implicit-terminal decision.
    fn bind_route_delivery(&self, delivered_tokens: Arc<AtomicU64>) {
        let _ = self.route_delivery.set(delivered_tokens);
    }

    pub fn route_delivered_tokens(&self) -> Option<u64> {
        self.route_delivery
            .get()
            .map(|delivered| delivered.load(Ordering::SeqCst))
    }

    /// Input+output tokens the bound consumer route has billed, or `None` when no route was ever bound.
    pub fn route_billable_tokens(&self) -> Option<u64> {
        self.route_billing
            .get()
            .map(|delivered| delivered.load(Ordering::SeqCst))
    }

    /// an IMPLICIT terminal must be bounded by delivered work, not by the session having
    /// existed. A consumer route that never delivered a token has nothing to settle by fact, and
    /// `TokenContract.stop()` before the seller's `acceptProbe` destroys the buyer's trial tick AND
    /// a mirror tick of the seller's bond (`contracts/airegistry/TokenContract.sol`, the
    /// `!_probeAccepted` branch of `stop`) -- a penalty priced for a buyer walking away from a trial
    /// he asked for, charged here to one who never asked. The two implicit terminals -- the awaited
    /// shutdown terminal and the `Drop` backup -- and the automatic continuity route swap consult
    /// this. Every EXPLICIT terminal -- an operator/policy `settle`, a verification bail, a recovery
    /// action -- is deliberately untouched, because those are decisions somebody made about this deal.
    fn implicit_terminal_lacks_delivery(&self) -> bool {
        self.route_delivered_tokens() == Some(0)
    }

    /// The shared veto both implicit terminals run. Returns whether the terminal was vetoed; the log
    /// line is the operator's only notice, so it names the remedy.
    fn veto_implicit_terminal_without_delivery(&self, reason: &str) -> bool {
        if !self.implicit_terminal_lacks_delivery() {
            return false;
        }
        self.close_local_api();
        self.disable_drop_backup();
        tracing::warn!(
            %reason,
            token_contract = %display_token_contract(&self.token_contract),
            delivered_tokens = 0_u64,
            chain_write_submitted = false,
            "consumer API: implicit terminal vetoed; this deal delivered no tokens, and an automatic \
             STOP inside the seller's probe window burns the buyer's trial tick and a mirror tick of \
             the seller's bond for a service that was never asked for. The deal stays open: close it \
             with `dexdo close`, naming this deal and passing its `--note-key`, which settles \
             without a burn once the seller's probe window has passed"
        );
        true
    }

    async fn settle_on_exit(&self, reason: &str) -> Result<bool, dexdo_core::ChainError> {
        match self.lifetime {
            SessionLifetimePolicy::SettleOnExit => {
                if self.veto_implicit_terminal_without_delivery(reason) {
                    return Ok(false);
                }
                self.settle(reason).await
            }
            SessionLifetimePolicy::Preserve => {
                self.preserve_without_implicit_chain_write(reason);
                Ok(false)
            }
        }
    }

    fn preserve_without_implicit_chain_write(&self, reason: &str) -> bool {
        if self.lifetime != SessionLifetimePolicy::Preserve {
            return false;
        }
        self.close_local_api();
        self.disable_drop_backup();
        tracing::info!(
            %reason,
            token_contract = %display_token_contract(&self.token_contract),
            chain_write_submitted = false,
            "consumer API: implicit terminal action vetoed; deal preserved"
        );
        true
    }

    pub fn dead_gateway_action(&self) -> DeadGatewayAction {
        self.failure_policy.dead_gateway
    }

    /// Whether a terminal on-chain action has landed for this deal.
    pub fn is_settled(&self) -> bool {
        self.settled.load(Ordering::SeqCst)
    }

    pub fn preserves_on_exit(&self) -> bool {
        self.lifetime == SessionLifetimePolicy::Preserve
    }

    pub fn terminal_action(&self) -> Option<SessionTerminalAction> {
        SessionTerminalAction::from_code(self.terminal_action.load(Ordering::SeqCst))
    }

    fn record_terminal_action(&self, action: SessionTerminalAction) {
        if action == SessionTerminalAction::ObservedTerminal {
            let _ = self.terminal_action.compare_exchange(
                0,
                action.code(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        } else {
            // A chain write is stronger evidence than a concurrent read-only terminal observation.
            self.terminal_action.store(action.code(), Ordering::SeqCst);
        }
    }

    /// Whether the local API must reject new requests for this deal. This is distinct from terminal settlement:
    /// a policy failure closes serving immediately while leaving STOP-on-shutdown/retry recovery eligible.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn close_local_api(&self) -> bool {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.closed_tx.send_replace(true);
            true
        } else {
            false
        }
    }

    pub(crate) fn closed_receiver(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }

    pub fn recovery_episode(&self) -> Option<RecoveryKind> {
        RecoveryKind::from_code(self.recovery_episode.load(Ordering::SeqCst))
    }

    pub fn take_handler_recovery_reconciliation(&self) -> Option<RecoveryKind> {
        self.handler_recovery_reconciliation
            .swap(false, Ordering::SeqCst)
            .then(|| self.recovery_episode())
            .flatten()
    }

    pub fn recovery_submit_may_have_landed(&self, kind: RecoveryKind) -> bool {
        self.recovery_episode.load(Ordering::SeqCst) == kind.code()
            && self.recovery_submit_may_have_landed.load(Ordering::SeqCst)
    }

    fn latch_possibly_landed_stop(&self, error: &dexdo_core::ChainError) -> bool {
        if !matches!(error, dexdo_core::ChainError::AmbiguousSubmit(_)) {
            return false;
        }
        // A STOP that may have landed supersedes any earlier recovery episode: every STOP entry point
        // must observe this one shared latch and reconcile chain facts instead of posting again.
        self.recovery_episode
            .store(RecoveryKind::ReclaimOpened.code(), Ordering::SeqCst);
        self.recovery_submit_may_have_landed
            .store(true, Ordering::SeqCst);
        self.handler_recovery_reconciliation
            .store(true, Ordering::SeqCst);
        // Drop is a last-chance STOP only when no STOP may already have landed. Once the outcome
        // is ambiguous, every later path is fact reconciliation and must never post again.
        self.disable_drop_backup();
        true
    }

    pub fn close_recovery_episode(&self, kind: RecoveryKind, reason: &str) -> bool {
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        let started = self
            .recovery_episode
            .compare_exchange(0, kind.code(), Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if self.close_local_api() && self.recovery_episode.load(Ordering::SeqCst) == kind.code() {
            self.recovery_closed_session.store(true, Ordering::SeqCst);
        }
        if started {
            tracing::warn!(
                %reason,
                token_contract = %display_token_contract(&self.token_contract),
                recovery_action = ?kind,
                "consumer API: closed failed session and latched one recovery episode"
            );
        }
        started
    }

    fn begin_recovery_attempt(&self, kind: RecoveryKind, handler_origin: bool) -> bool {
        let existing = self.recovery_episode.load(Ordering::SeqCst);
        let started = if existing == 0 {
            self.recovery_episode
                .compare_exchange(0, kind.code(), Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        } else {
            false
        };
        let current = self.recovery_episode.load(Ordering::SeqCst);
        if current != kind.code() {
            return false;
        }
        if self.recovery_submit_may_have_landed(kind) {
            tracing::debug!(
                token_contract = %display_token_contract(&self.token_contract),
                recovery_action = ?kind,
                outcome = "possibly_landed_submit_needs_fact_read",
                "consumer API: suppressed automatic recovery resubmit"
            );
            return false;
        }
        if !handler_origin
            && !started
            && self.handler_recovery_reconciliation.load(Ordering::SeqCst)
        {
            tracing::debug!(
                token_contract = %display_token_contract(&self.token_contract),
                recovery_action = ?kind,
                outcome = "handler_result_needs_fact_read",
                "consumer API: suppressed monitor recovery until handler ambiguity is reconciled"
            );
            return false;
        }
        if handler_origin {
            if self.close_local_api() {
                self.recovery_closed_session.store(true, Ordering::SeqCst);
            }
            if !started {
                tracing::debug!(
                    token_contract = %display_token_contract(&self.token_contract),
                    recovery_action = ?kind,
                    outcome = "episode_already_latched",
                    "consumer API: suppressed duplicate handler recovery attempt"
                );
                return false;
            }
        }
        true
    }

    fn cancel_recovery_without_post(&self, kind: RecoveryKind) {
        if self
            .recovery_episode
            .compare_exchange(kind.code(), 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.handler_recovery_reconciliation
            .store(false, Ordering::SeqCst);
        self.recovery_submit_may_have_landed
            .store(false, Ordering::SeqCst);
        if self.recovery_closed_session.swap(false, Ordering::SeqCst)
            && !self.settled.load(Ordering::SeqCst)
            && self.closed.swap(false, Ordering::SeqCst)
        {
            self.closed_tx.send_replace(false);
        }
    }

    fn disable_drop_backup(&self) {
        self.drop_backup_enabled.store(false, Ordering::SeqCst);
    }

    /// Fail closed before any user-visible response is rendered unless the chain proves this deal is open.

    /// A decrypted handover and a reachable gateway are not enough: showed that a stale handover can let the
    /// local endpoint serve a response while the TokenContract remains funded-but-never-opened and unaccounted.
    pub async fn ensure_open_for_serving(&self) -> Result<dexdo_core::DealChainState, String> {
        let state = match self.chain.deal_state(&self.token_contract).await {
            Ok(Some(state)) => state,
            Ok(None) => {
                let reason = "deal state unavailable before serving user response".to_string();
                self.fail_closed_before_serving(&reason).await;
                return Err(reason);
            }
            Err(e) => {
                let reason = format!("deal state unreadable before serving user response: {e}");
                self.fail_closed_before_serving(&reason).await;
                return Err(reason);
            }
        };
        if state.funded && state.opened && !state.disputed {
            return Ok(state);
        }

        let now_secs = unix_now_secs();
        let cleanup = unopened_cleanup_decision(state, now_secs);
        let reason = not_safely_open_reason(state, cleanup);
        self.fail_closed_unopened_before_serving(&reason, cleanup)
            .await;
        Err(reason)
    }

    async fn fail_closed_before_serving(&self, reason: &str) {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        self.close_local_api();
        tracing::error!(
            %reason,
            token_contract = %display_token_contract(&self.token_contract),
            result = "policy_fail_closed",
            "consumer API: refusing to serve user-visible response without by-fact open/accounting"
        );
    }

    async fn fail_closed_unopened_before_serving(
        &self,
        reason: &str,
        cleanup: Option<UnopenedCleanupDecision>,
    ) {
        if cleanup == Some(UnopenedCleanupDecision::Ready) {
            match self.recover_cleanup_unopened(true).await {
                Ok(Some(s)) => {
                    tracing::warn!(
                        %reason,
                        token_contract = %display_token_contract(&self.token_contract),
                        settlement = ?s,
                        "consumer API: refused response before open and cleaned up unopened deal"
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        %reason,
                        token_contract = %display_token_contract(&self.token_contract),
                        error = %e,
                        "consumer API: refused response before open; unopened cleanup not yet available"
                    );
                }
            }
        } else {
            let _guard = self.settle_lock.lock().await;
            if self.settled.load(Ordering::SeqCst) {
                return;
            }
            self.close_local_api();
            tracing::error!(
                %reason,
                token_contract = %display_token_contract(&self.token_contract),
                result = "policy_fail_closed",
                "consumer API: refusing to serve user-visible response without by-fact open/accounting"
            );
        }
    }

    /// Mark the session terminal after an external recovery path (`streamCleanup` / `streamStop`) already
    /// closed or reclaimed the deal. This prevents a later route swap from sending a duplicate STOP to the
    /// recovered TC.
    pub fn mark_recovered(&self, reason: &str) -> bool {
        if self.settled.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.record_terminal_action(SessionTerminalAction::ObservedTerminal);
        self.recovery_closed_session.store(false, Ordering::SeqCst);
        self.handler_recovery_reconciliation
            .store(false, Ordering::SeqCst);
        self.recovery_submit_may_have_landed
            .store(false, Ordering::SeqCst);
        self.close_local_api();
        tracing::info!(%reason, "consumer API: session deal marked recovered");
        true
    }

    pub async fn mark_recovered_serialized(&self, reason: &str) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        self.record_terminal_action(SessionTerminalAction::ObservedTerminal);
        self.settled.store(true, Ordering::SeqCst);
        self.recovery_closed_session.store(false, Ordering::SeqCst);
        self.handler_recovery_reconciliation
            .store(false, Ordering::SeqCst);
        self.recovery_submit_may_have_landed
            .store(false, Ordering::SeqCst);
        self.close_local_api();
        tracing::info!(%reason, "consumer API: session deal marked recovered");
        true
    }

    pub async fn recover_cleanup_unopened(
        &self,
        handler_origin: bool,
    ) -> Result<Option<dexdo_core::Settlement>, dexdo_core::ChainError> {
        if self.preserve_without_implicit_chain_write("cleanup-unopened") {
            return Ok(None);
        }
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if !self.begin_recovery_attempt(RecoveryKind::CleanupUnopened, handler_origin) {
            return Ok(None);
        }
        let settlement = match self.chain.cleanup_unopened(&self.token_contract).await {
            Ok(settlement) => settlement,
            Err(error) => {
                if handler_origin {
                    self.handler_recovery_reconciliation
                        .store(true, Ordering::SeqCst);
                }
                return Err(error);
            }
        };
        self.close_local_api();
        self.record_terminal_action(SessionTerminalAction::StreamCleanup);
        self.settled.store(true, Ordering::SeqCst);
        self.recovery_closed_session.store(false, Ordering::SeqCst);
        self.handler_recovery_reconciliation
            .store(false, Ordering::SeqCst);
        self.recovery_submit_may_have_landed
            .store(false, Ordering::SeqCst);
        Ok(Some(settlement))
    }

    pub async fn recover_reclaim_opened(
        &self,
        heartbeat: &dexdo_core::market::HeartbeatGuard,
        handler_origin: bool,
    ) -> Result<Option<dexdo_core::Settlement>, dexdo_core::ChainError> {
        if self.preserve_without_implicit_chain_write("reclaim-opened") {
            return Ok(None);
        }
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if !self.begin_recovery_attempt(RecoveryKind::ReclaimOpened, handler_origin) {
            return Ok(None);
        }
        // This path exists only for an explicitly configured automatic failure policy. The continuity
        // monitor never infers STOP from idle time alone. If accepted output resumes while the policy's
        // signed STOP is prepared, the final heartbeat guard cancels that stale automatic decision.
        let settlement = match self
            .chain
            .stop_if_heartbeat(&self.token_contract, self.note.as_ref(), heartbeat)
            .await
        {
            Ok(settlement) => settlement,
            Err(error) => {
                if !self.latch_possibly_landed_stop(&error) && handler_origin {
                    self.handler_recovery_reconciliation
                        .store(true, Ordering::SeqCst);
                }
                return Err(error);
            }
        };
        if settlement.is_none() {
            self.cancel_recovery_without_post(RecoveryKind::ReclaimOpened);
            return Ok(None);
        }
        self.close_local_api();
        self.record_terminal_action(SessionTerminalAction::from_stop_settlement(
            settlement.as_ref().expect("checked Some settlement"),
        ));
        self.settled.store(true, Ordering::SeqCst);
        self.recovery_closed_session.store(false, Ordering::SeqCst);
        self.handler_recovery_reconciliation
            .store(false, Ordering::SeqCst);
        self.recovery_submit_may_have_landed
            .store(false, Ordering::SeqCst);
        Ok(settlement)
    }

    /// STOP or reconcile the deal once. `&self` -- the session is `Arc`-shared across the handlers.
    /// Returns whether this call established a terminal chain outcome; [`SessionTerminalAction`]
    /// preserves whether our STOP was confirmed, the deal was already closed, or the closer is unknown.
    /// An unresolved STOP error leaves the session explicitly recoverable; `Drop` must not issue an
    /// untracked retry after an awaited failure.
    pub async fn settle(&self, reason: &str) -> Result<bool, dexdo_core::ChainError> {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return Ok(false); // already settled by an earlier bail / shutdown / Drop
        }
        if self.recovery_submit_may_have_landed(RecoveryKind::ReclaimOpened) {
            return Err(dexdo_core::ChainError::AmbiguousSubmit(format!(
                "TokenContract {} STOP may already have landed; automatic resubmit is suppressed \
                 until fresh chain facts prove a terminal outcome",
                display_token_contract(&self.token_contract)
            )));
        }
        self.close_local_api();
        match self
            .chain
            .stop(&self.token_contract, self.note.as_ref())
            .await
        {
            Ok(s) => {
                let terminal_action = SessionTerminalAction::from_stop_settlement(&s);
                self.record_terminal_action(terminal_action);
                self.settled.store(true, Ordering::SeqCst);
                self.recovery_closed_session.store(false, Ordering::SeqCst);
                self.handler_recovery_reconciliation
                    .store(false, Ordering::SeqCst);
                self.recovery_submit_may_have_landed
                    .store(false, Ordering::SeqCst);
                tracing::info!(
                    %reason,
                    settlement = ?s,
                    terminal_action = terminal_action.event_action(),
                    chain_write_submitted = terminal_action.chain_write_submitted(),
                    "consumer API: session terminal chain outcome recorded"
                )
            }
            Err(e) => {
                self.latch_possibly_landed_stop(&e);
                tracing::warn!(
                    %reason,
                    error = %e,
                    "consumer API: session STOP/settlement failed; session remains recoverable"
                );
                self.disable_drop_backup();
                return Err(e);
            }
        }
        Ok(true)
    }

    async fn policy_fail_closed(&self, failure_class: &str, action: &str, reason: &str) -> bool {
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) || !self.drop_backup_enabled.load(Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        tracing::error!(
            %reason,
            policy_failure_class = failure_class,
            policy_action = action,
            token_contract = %display_token_contract(&self.token_contract),
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
            token_contract = %display_token_contract(&self.token_contract),
            result = "policy_action_unsupported",
            diagnostic,
            "consumer API: selected policy action is unsupported in this runtime surface; session remains recoverable"
        );
        false
    }

    async fn policy_seller_timeout(
        &self,
        failure_class: &str,
        action: &str,
        reason: &str,
        heartbeat: &dexdo_core::market::HeartbeatGuard,
    ) -> bool {
        match self.recover_reclaim_opened(heartbeat, true).await {
            Ok(Some(s)) => {
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %display_token_contract(&self.token_contract),
                    settlement = ?s,
                    "consumer API: selected policy action reconciled a terminal outcome via seller_timeout"
                );
                true
            }
            Ok(None) => {
                tracing::info!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %display_token_contract(&self.token_contract),
                    result = "accepted_output_heartbeat_changed",
                    "consumer API: cancelled seller_timeout before submit"
                );
                false
            }
            Err(e) => {
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %display_token_contract(&self.token_contract),
                    error = %e,
                    "consumer API: selected seller_timeout policy action failed; session remains recoverable"
                );
                false
            }
        }
    }

    async fn policy_dispute(&self, failure_class: &str, action: &str, reason: &str) -> bool {
        if self.preserve_without_implicit_chain_write(reason) {
            return false;
        }
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
                self.record_terminal_action(SessionTerminalAction::StreamDispute);
                self.settled.store(true, Ordering::SeqCst);
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %display_token_contract(&self.token_contract),
                    settlement = ?s,
                    "consumer API: selected policy action opened DISPUTE; this deal's contested funds are frozen until resolution"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    %reason,
                    policy_failure_class = failure_class,
                    policy_action = action,
                    token_contract = %display_token_contract(&self.token_contract),
                    error = %e,
                    "consumer API: selected DISPUTE policy action failed; session remains recoverable"
                );
                false
            }
        }
    }

    /// Apply the explicit `bad_output_scam` policy on a verification bail. `dispute` uses the
    /// existing streamDispute lever and reports the per-deal freeze. `stop_and_blacklist` is not silently
    /// degraded in the consumer API surface because this surface has no seller-id blacklist store.
    pub async fn settle_verification_bail(&self, reason: &str) -> bool {
        if self.preserve_without_implicit_chain_write(reason) {
            return false;
        }
        let _guard = self.settle_lock.lock().await;
        if self.settled.load(Ordering::SeqCst) {
            return false;
        }
        self.close_local_api();
        let action = self.failure_policy.verification_bail;
        match action {
            VerificationBailAction::Stop
                if self.recovery_submit_may_have_landed(RecoveryKind::ReclaimOpened) =>
            {
                tracing::debug!(
                    %reason,
                    policy_failure_class = "bad_output_scam",
                    policy_action = action.as_str(),
                    token_contract = %display_token_contract(&self.token_contract),
                    outcome = "possibly_landed_submit_needs_fact_read",
                    "consumer API: suppressed verification-bail STOP resubmit"
                );
                false
            }
            VerificationBailAction::Stop => match self
                .chain
                .stop(&self.token_contract, self.note.as_ref())
                .await
            {
                Ok(s) => {
                    let terminal_action = SessionTerminalAction::from_stop_settlement(&s);
                    self.record_terminal_action(terminal_action);
                    self.settled.store(true, Ordering::SeqCst);
                    tracing::info!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %display_token_contract(&self.token_contract),
                        settlement = ?s,
                        terminal_action = terminal_action.event_action(),
                        chain_write_submitted = terminal_action.chain_write_submitted(),
                        "consumer API: verification bail terminal chain outcome recorded"
                    );
                    true
                }
                Err(e) => {
                    self.latch_possibly_landed_stop(&e);
                    tracing::warn!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %display_token_contract(&self.token_contract),
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
                    self.record_terminal_action(SessionTerminalAction::StreamDispute);
                    self.settled.store(true, Ordering::SeqCst);
                    tracing::warn!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %display_token_contract(&self.token_contract),
                        settlement = ?s,
                        "consumer API: verification bail opened DISPUTE; this deal's contested funds are frozen until resolution"
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        %reason,
                        policy_failure_class = "bad_output_scam",
                        policy_action = action.as_str(),
                        token_contract = %display_token_contract(&self.token_contract),
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
                    token_contract = %display_token_contract(&self.token_contract),
                    result = "policy_action_unsupported",
                    diagnostic = "consumer API has no seller identity/blacklist store; refusing to degrade to STOP",
                    "consumer API: stop_and_blacklist unsupported in this runtime surface; session remains recoverable"
                );
                false
            }
        }
    }

    pub async fn settle_dead_gateway(
        &self,
        reason: &str,
        heartbeat: &dexdo_core::market::HeartbeatGuard,
    ) -> bool {
        let action = self.failure_policy.dead_gateway;
        match action {
            DeadGatewayAction::RetryThenReclaim => {
                self.policy_seller_timeout("dead_gateway", action.as_str(), reason, heartbeat)
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

    pub async fn settle_empty_stream(
        &self,
        reason: &str,
        heartbeat: &dexdo_core::market::HeartbeatGuard,
    ) -> bool {
        let action = self.failure_policy.empty_stream;
        match action {
            EmptyStreamAction::Reclaim => {
                self.policy_seller_timeout("empty_stream", action.as_str(), reason, heartbeat)
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

    pub async fn settle_seller_stalls_mid_stream(
        &self,
        reason: &str,
        heartbeat: &dexdo_core::market::HeartbeatGuard,
    ) -> bool {
        let action = self.failure_policy.seller_stalls_mid_stream;
        match action {
            SellerStallsMidStreamAction::AcceptDeliveredThenReclaim => {
                self.policy_seller_timeout(
                    "seller_stalls_mid_stream",
                    action.as_str(),
                    reason,
                    heartbeat,
                )
                .await
            }
            SellerStallsMidStreamAction::Dispute => {
                self.policy_dispute("seller_stalls_mid_stream", action.as_str(), reason)
                    .await
            }
        }
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnopenedCleanupDecision {
    Ready,
    Wait { wait_secs: u64 },
    MissingFundedTime,
}

fn unopened_cleanup_decision(
    state: dexdo_core::DealChainState,
    now_secs: u64,
) -> Option<UnopenedCleanupDecision> {
    // The never-opened case, and only it: every terminal path drains the deposit, so a funded deal with
    // escrow still held is one the seller has not opened yet rather than one that already settled.
    if !(state.funded && !state.opened && !state.disputed && !state.is_stopped()) {
        return None;
    }
    let Some(funded_time) = state.funded_time else {
        return Some(UnopenedCleanupDecision::MissingFundedTime);
    };
    let cleanup_at = funded_time.saturating_add(MATCH_OPEN_TIMEOUT_SECS);
    if now_secs >= cleanup_at {
        Some(UnopenedCleanupDecision::Ready)
    } else {
        Some(UnopenedCleanupDecision::Wait {
            wait_secs: cleanup_at.saturating_sub(now_secs),
        })
    }
}

fn not_safely_open_reason(
    state: dexdo_core::DealChainState,
    cleanup: Option<UnopenedCleanupDecision>,
) -> String {
    let mut reason = format!(
        "deal is not safely opened/accountable before serving user response: funded={} opened={} \
         disputed={} deposit={} tokens_final={}",
        state.funded, state.opened, state.disputed, state.deposit, state.tokens_final
    );
    match cleanup {
        Some(UnopenedCleanupDecision::Ready) => {
            reason.push_str(" cleanup_ready=true");
        }
        Some(UnopenedCleanupDecision::Wait { wait_secs }) => {
            reason.push_str(&format!(
                " cleanup_ready=false cleanup_wait_secs={wait_secs}"
            ));
        }
        Some(UnopenedCleanupDecision::MissingFundedTime) => {
            reason.push_str(" cleanup_ready=false funded_time=<missing>");
        }
        None => {}
    }
    reason
}

impl Drop for SessionSettle {
    fn drop(&mut self) {
        // BEST-EFFORT BACKUP ONLY: the awaited terminal (graceful shutdown / bail) is the
        // funds-safety guarantee. If the session ended with no explicit settle (abnormal teardown), spawn a
        // last-chance STOP -- a crash/SIGKILL/runtime teardown may still skip it, and the on-chain
        // `seller_timeout` is the ultimate backstop.
        if self.lifetime == SessionLifetimePolicy::Preserve
            || self.settled.load(Ordering::SeqCst)
            || !self.drop_backup_enabled.load(Ordering::SeqCst)
            || self.recovery_submit_may_have_landed.load(Ordering::SeqCst)
        {
            return;
        }
        // the same delivery bound the awaited terminal applies, consulted HERE and not only
        // there -- `Drop` runs on unwind and on early return, which is precisely where the client has
        // the least evidence that anything was ever delivered. Placed after the guards above so an
        // already-settled session does not log a veto it never needed.
        if self.veto_implicit_terminal_without_delivery("drop-backup") {
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
                    tracing::error!(error = %e, "consumer API: Drop-path backup STOP failed");
                }
            });
        } else {
            tracing::error!(
                token_contract = %display_token_contract(&self.token_contract),
                "consumer API: Drop-path backup STOP could not be scheduled without a Tokio runtime"
            );
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
        Self::single_deal(
            buyer,
            frame_model,
            ApiDeal::new(route, session, content_gate),
        )
    }

    /// One already-built deal -- the seam a subscription route needs, because its live weekly budget
    /// is attached to the [`ApiDeal`] rather than to the immutable [`Route`].
    pub fn single_deal(buyer: Arc<Buyer>, frame_model: String, deal: ApiDeal) -> Self {
        Self {
            buyer,
            frame_model,
            mock_model: false,
            deals: Arc::new(RouteManager::new(deal)),
            delivery_events: None,
        }
    }

    pub fn lazy(
        buyer: Arc<Buyer>,
        frame_model: String,
        initializer: DealInitializer,
        initializer_timeout: Duration,
    ) -> Self {
        Self {
            buyer,
            frame_model,
            mock_model: false,
            deals: Arc::new(RouteManager::lazy(initializer, initializer_timeout)),
            delivery_events: None,
        }
    }

    pub fn recoverable_lazy(
        buyer: Arc<Buyer>,
        frame_model: String,
        initializer: DealInitializer,
        initializer_timeout: Duration,
    ) -> Self {
        Self {
            buyer,
            frame_model,
            mock_model: false,
            deals: Arc::new(RouteManager::recoverable_lazy(
                initializer,
                initializer_timeout,
            )),
            delivery_events: None,
        }
    }

    pub fn recoverable_lazy_with_active(
        buyer: Arc<Buyer>,
        frame_model: String,
        active: ApiDeal,
        initializer: DealInitializer,
        initializer_timeout: Duration,
    ) -> Self {
        Self {
            buyer,
            frame_model,
            mock_model: false,
            deals: Arc::new(RouteManager::recoverable_lazy_with_active(
                active,
                initializer,
                initializer_timeout,
            )),
            delivery_events: None,
        }
    }

    pub fn with_mock_model(mut self, mock_model: bool) -> Self {
        self.mock_model = mock_model;
        self
    }

    pub async fn current_deal(&self) -> Result<ApiDeal, DealInitError> {
        self.deals.current_or_prepare().await
    }

    /// The model is forced by the market (B2/B19): an empty/None `model` is ok (there is a single
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

/// Build the consumer-interface axum router. The Anthropic transcode (B20) is mounted only
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
/// `ctrl_c`/SIGTERM): on it the server drains in-flight requests (graceful shutdown), then the session deal is
/// STOPped via an **awaited** `session.settle("shutdown")` before the task ends -- the funds-safety
/// guarantee (`SessionSettle::Drop` is only a backup). Tests pass a never-completing signal and abort the task.
pub async fn serve(
    bind: SocketAddr,
    state: ApiState,
    anthropic_compat: bool,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    // same reason as the seller gateway. `dexdo buyer --local-listen 127.0.0.1:0` serves an
    // OpenAI-compatible endpoint, and a consumer that closes the connection part-way through an
    // answer is ordinary. Under the entry policy that a one-shot printer wants, that hangup would
    // end this process instead of this request. Not a duplicate of `main` -- the opposite decision.
    crate::serving_process_ignores_sigpipe();
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
        if let Err(error) = deals.settle_active_on_exit("shutdown").await {
            tracing::error!(%error, "consumer API: graceful shutdown STOP failed");
        }
    });
    Ok((local_addr, task))
}

#[cfg(test)]
mod fixture_seller;

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::SUB_WEEK_LEN;

    mod route_swap_1025;

    #[test]
    fn default_failure_policy_uses_canonical_parameters() {
        let policy = BuyerApiFailurePolicy::default();
        assert_eq!(
            policy.verification_bail.as_str(),
            DEFAULT_BUYER_VERIFICATION_BAIL_ACTION
        );
        assert_eq!(
            policy.dead_gateway.as_str(),
            DEFAULT_BUYER_DEAD_GATEWAY_ACTION
        );
        assert_eq!(
            policy.empty_stream.as_str(),
            DEFAULT_BUYER_EMPTY_STREAM_ACTION
        );
        assert_eq!(
            policy.seller_stalls_mid_stream.as_str(),
            DEFAULT_BUYER_STALLS_MID_STREAM_ACTION
        );
    }

    // fail-loud content-identity policy (pure), shared by both buyer paths. A seller can declare the
    // correct model NAME yet serve a cheaper model; only the CONTENT layers (B8 fingerprint / B7 reference)
    // catch that. A real model identity with neither must FAIL CLOSED unless the operator opts into name-only.
    // Data-driven: a config with qwen (fingerprint) but NOT llama exercises probe vs fail-closed.
    fn policy_models() -> ModelsConfig {
        ModelsConfig::from_json(
            r#"{ "models": { "qwen": {
                "frame_model": "qwen--qwen3--32b",
                "base_url": "https://api.groq.com/openai/v1",
                "served_model": "qwen/qwen3-32b",
                "api_key_env": "GROQ_API_KEY",
                "tokenizer_family": "qwen",
                "price_per_tick": 1000,
                "identity_aliases": ["Qwen/Qwen3-32B"],
                "vocab_size": 152064,
                "fingerprints": [ { "probe_prompt": "What is 17*23? Think step by step.", "expected_contains": "<think>", "accepts_reasoning_side_channel": true } ]
            } } }"#,
        )
        .unwrap()
    }

    #[test]
    fn policy_mock_model_skips() {
        let cfg = policy_models();
        assert_eq!(
            content_check_policy("qwen/qwen3-32b", None, true, false, false, &cfg).unwrap(),
            ContentCheck::Skip
        );
    }

    #[test]
    fn policy_qwen_b8_fingerprint_probes() {
        let cfg = policy_models();
        assert_eq!(
            content_check_policy("qwen--qwen3--32b", None, false, false, false, &cfg).unwrap(),
            ContentCheck::Probe {
                model_id: "qwen--qwen3--32b".to_string()
            }
        );
    }

    #[test]
    fn policy_registry_backed_qwen_identity_probes_by_registry_model() {
        let cfg = policy_models();
        assert_eq!(
            content_check_policy(
                "qwen--qwen3--32b",
                Some("Qwen/Qwen3-32B"),
                false,
                false,
                false,
                &cfg
            )
            .unwrap(),
            ContentCheck::Probe {
                model_id: "Qwen/Qwen3-32B".to_string()
            }
        );
    }

    #[tokio::test]
    async fn lazy_api_models_respond_before_deal_initializer_runs() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let init_calls = Arc::new(AtomicUsize::new(0));
        let init_calls_for_state = init_calls.clone();
        let state = ApiState::lazy(
            Arc::new(Buyer::from_note(
                Arc::new(dexdo_core::LocalNote::generate()),
            )),
            "qwen--qwen3--32b".to_string(),
            Arc::new(move || {
                let init_calls = init_calls_for_state.clone();
                Box::pin(async move {
                    init_calls.fetch_add(1, Ordering::SeqCst);
                    futures::future::pending::<Result<ApiDeal, DealInitError>>().await
                }) as DealInitFuture
            }),
            Duration::from_millis(50),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), state, false, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("bind lazy API");

        let models: serde_json::Value = reqwest::Client::new()
            .get(format!("http://{addr}/v1/models"))
            .send()
            .await
            .expect("models request")
            .error_for_status()
            .expect("models status")
            .json()
            .await
            .expect("models json");
        assert_eq!(models["data"][0]["id"], "qwen--qwen3--32b");
        assert_eq!(
            init_calls.load(Ordering::SeqCst),
            0,
            "/v1/models must not start quote/place_buy/handover work"
        );

        let _ = shutdown_tx.send(());
        task.await.expect("server joins");
    }

    #[tokio::test]
    async fn lazy_chat_initializer_timeout_returns_error_not_hang() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let init_calls = Arc::new(AtomicUsize::new(0));
        let init_calls_for_state = init_calls.clone();
        let state = ApiState::lazy(
            Arc::new(Buyer::from_note(
                Arc::new(dexdo_core::LocalNote::generate()),
            )),
            "qwen--qwen3--32b".to_string(),
            Arc::new(move || {
                let init_calls = init_calls_for_state.clone();
                Box::pin(async move {
                    init_calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Err(DealInitError::new("slow chain init should have timed out"))
                }) as DealInitFuture
            }),
            Duration::from_millis(50),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), state, false, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("bind lazy API");

        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "qwen--qwen3--32b",
                "messages": [{"role": "user", "content": "hello"}],
                "max_tokens": 1,
                "stream": false
            }))
            .send()
            .await
            .expect("chat request returns");
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("body");
        assert!(body.contains("on-demand purchase timed out"), "{body}");
        assert_eq!(init_calls.load(Ordering::SeqCst), 1);

        let _ = shutdown_tx.send(());
        task.await.expect("server joins");
    }

    #[test]
    fn policy_unknown_qwen_variant_does_not_inherit_qwen3_fingerprint() {
        let cfg = policy_models();
        assert_eq!(
            content_check_policy("qwen--qwen3.6--27b", None, false, true, false, &cfg).unwrap(),
            ContentCheck::Skip
        );
    }

    #[test]
    fn policy_real_family_no_fingerprint_no_key_fails_closed() {
        // llama: a real model with NO B8 fingerprint (not in config) and NO B7 reference key -> refuse (fail
        // closed) without --allow-unverified-model.
        let cfg = policy_models();
        let r = content_check_policy("meta-llama/llama-3.1-8b", None, false, false, false, &cfg);
        assert!(r.is_err(), "name-only model must fail closed, got {r:?}");
    }

    #[test]
    fn policy_allow_unverified_downgrades_to_skip() {
        let cfg = policy_models();
        assert_eq!(
            content_check_policy("meta-llama/llama-3.1-8b", None, false, true, false, &cfg)
                .unwrap(),
            ContentCheck::Skip
        );
    }

    #[test]
    fn policy_reference_key_enables_probe() {
        let cfg = policy_models();
        assert_eq!(
            content_check_policy("meta-llama/llama-3.1-8b", None, false, false, true, &cfg)
                .unwrap(),
            ContentCheck::Probe {
                model_id: "meta-llama/llama-3.1-8b".to_string()
            }
        );
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
    fn legacy_delivery_count_stays_distinct_from_visible_output_lower_bound() {
        let structured = CanonChunk {
            token_ids: vec![1, 2, 3],
            ..CanonChunk::default()
        };
        assert_eq!(accounted_tokens(&structured), 3);
        assert_eq!(structured.visible_output_tokens(), 3);
        let metadata_only = CanonChunk::default();
        assert_eq!(
            accounted_tokens(&metadata_only),
            1,
            "the stable v2 delivery field retains its one-token fallback"
        );
        assert_eq!(
            metadata_only.visible_output_tokens(),
            0,
            "the new billing verifier does not manufacture visible provider output"
        );
    }

    fn heartbeat_test_deal() -> ApiDeal {
        ApiDeal::new(
            Route {
                handover: Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "00".repeat(32),
                },
                token_contract: "tc-heartbeat-poll".to_string(),
                max_tokens: 100,
            },
            Arc::new(SessionSettle::new(
                Arc::new(RecordingSettleChain::default()),
                "tc-heartbeat-poll".to_string(),
                Arc::new(dexdo_core::LocalNote::generate()),
            )),
            Arc::new(ContentGate::skip()),
        )
    }

    #[tokio::test]
    async fn openai_content_poll_records_heartbeat_before_returning_event() {
        use futures::StreamExt;
        let deal = heartbeat_test_deal();
        let stream = super::openai::heartbeat_poll_test_stream(deal.clone());
        futures::pin_mut!(stream);
        assert_eq!(deal.accepted_output_generation(), 0);
        assert!(stream.next().await.is_some());
        assert_eq!(deal.accepted_output_generation(), 1);
    }

    #[tokio::test]
    async fn anthropic_content_poll_records_heartbeat_before_returning_event() {
        use futures::StreamExt;
        let deal = heartbeat_test_deal();
        let stream = super::anthropic::heartbeat_poll_test_stream(deal.clone());
        futures::pin_mut!(stream);
        assert_eq!(deal.accepted_output_generation(), 0);
        assert!(stream.next().await.is_some());
        assert_eq!(deal.accepted_output_generation(), 1);
    }

    #[tokio::test]
    async fn accepted_openai_and_anthropic_output_never_turns_old_idle_age_into_stop() {
        use crate::buyer::continuity::{BuyerAction, BuyerContinuity, ContinuityConfig, DealFacts};
        use futures::StreamExt;
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let deal = |token_contract: &str| {
            recovery_test_deal(
                token_contract,
                chain.clone(),
                Arc::new(dexdo_core::LocalNote::generate()),
            )
        };

        let openai_deal = deal("tc-openai-idle");
        let openai = super::openai::heartbeat_poll_test_stream(openai_deal.clone());
        futures::pin_mut!(openai);
        assert!(openai.next().await.is_some());

        let anthropic_deal = deal("tc-anthropic-idle");
        let anthropic = super::anthropic::heartbeat_poll_test_stream(anthropic_deal.clone());
        futures::pin_mut!(anthropic);
        assert!(anthropic.next().await.is_some());

        for deal in [openai_deal, anthropic_deal] {
            assert_eq!(deal.accepted_output_generation(), 1);
            assert!(matches!(
                BuyerContinuity::default().tick(
                    Some(DealFacts::opened_idle(
                        deal.route.token_contract.clone(),
                        u64::MAX,
                    )),
                    None,
                    ContinuityConfig::default(),
                ),
                BuyerAction::ServeCurrent { .. }
            ));
        }
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn accepted_output_heartbeat_is_monotonic() {
        let deal = ApiDeal::new(
            Route {
                handover: Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "00".repeat(32),
                },
                token_contract: "tc-heartbeat".to_string(),
                max_tokens: 100,
            },
            Arc::new(SessionSettle::new(
                Arc::new(RecordingSettleChain::default()),
                "tc-heartbeat".to_string(),
                Arc::new(dexdo_core::LocalNote::generate()),
            )),
            Arc::new(ContentGate::skip()),
        );
        deal.record_accepted_output(200);
        deal.record_accepted_output(199);
        assert_eq!(deal.last_accepted_output_unix_secs(), 200);
    }

    #[derive(Default)]
    struct RecordingSettleChain {
        stop_calls: std::sync::atomic::AtomicUsize,
        dispute_calls: std::sync::atomic::AtomicUsize,
        recovery_stop_calls: std::sync::atomic::AtomicUsize,
        cleanup_unopened_calls: std::sync::atomic::AtomicUsize,
        fail_stop: std::sync::atomic::AtomicBool,
        fail_dispute: std::sync::atomic::AtomicBool,
        fail_recovery_stop: std::sync::atomic::AtomicBool,
        ambiguous_stop: std::sync::atomic::AtomicBool,
        ambiguous_recovery_stop: std::sync::atomic::AtomicBool,
        fail_cleanup_unopened: std::sync::atomic::AtomicBool,
        fail_deal_state: std::sync::atomic::AtomicBool,
        deal_state: std::sync::Mutex<Option<dexdo_core::DealChainState>>,
        heartbeat_during_reclaim_preflight: std::sync::Mutex<Option<ApiDeal>>,
        heartbeat_during_explicit_stop_preflight: std::sync::Mutex<Option<ApiDeal>>,
    }

    impl RecordingSettleChain {
        fn set_deal_state(&self, state: dexdo_core::DealChainState) {
            *self.deal_state.lock().unwrap() = Some(state);
        }
    }

    fn unchanged_heartbeat() -> dexdo_core::market::HeartbeatGuard {
        dexdo_core::market::HeartbeatGuard::new(Arc::new(AtomicU64::new(0)))
    }

    fn never_opened_deal_state(deposit: u128, funded_time: u64) -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: true,
            opened: false,
            probe_accepted: false,
            disputed: false,
            deposit,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            funded_time: Some(funded_time),
            probe_tick: 0,
            probe_time: 0,
            last_claim_time: funded_time,
            dispute_time: 0,
        }
    }

    fn opened_deal_state(deposit: u128, funded_time: u64) -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: true,
            opened: true,
            probe_accepted: false,
            disputed: false,
            deposit,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            funded_time: Some(funded_time),
            probe_tick: 0,
            probe_time: 0,
            last_claim_time: funded_time,
            dispute_time: 0,
        }
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

        async fn claim_tokens(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
            _cumulative_tokens: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by settlement policy tests")
        }

        async fn stop_if_heartbeat(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
            heartbeat: &dexdo_core::market::HeartbeatGuard,
        ) -> Result<Option<dexdo_core::Settlement>, dexdo_core::ChainError> {
            // Simulate a legitimate claim landing between the decision to exit and the money POST.
            if let Some(deal) = self
                .heartbeat_during_reclaim_preflight
                .lock()
                .unwrap()
                .take()
            {
                deal.record_accepted_output(unix_now_secs());
            }
            if !heartbeat.unchanged() {
                return Ok(None);
            }
            self.recovery_stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .ambiguous_recovery_stop
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(dexdo_core::ChainError::AmbiguousSubmit(
                    "injected ambiguous recovery STOP".to_string(),
                ));
            }
            if self
                .fail_recovery_stop
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(dexdo_core::ChainError::Chain(
                    "injected recovery stop failure".to_string(),
                ));
            }
            Ok(Some(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_bond_returned: 0,
            }))
        }

        async fn stop(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            if let Some(deal) = self
                .heartbeat_during_explicit_stop_preflight
                .lock()
                .unwrap()
                .take()
            {
                deal.record_accepted_output(unix_now_secs());
            }
            self.stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .ambiguous_stop
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(dexdo_core::ChainError::AmbiguousSubmit(
                    "injected ambiguous STOP".to_string(),
                ));
            }
            if self.fail_stop.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(dexdo_core::ChainError::Chain(
                    "injected stop failure".to_string(),
                ));
            }
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_bond_returned: 0,
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
                seller_bond_returned: 0,
            })
        }

        async fn cleanup_unopened(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.cleanup_unopened_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .fail_cleanup_unopened
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(dexdo_core::ChainError::Chain(
                    "injected cleanup_unopened failure".to_string(),
                ));
            }
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_bond_returned: 0,
            })
        }

        async fn deal_state(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Option<dexdo_core::DealChainState>, dexdo_core::ChainError> {
            if self
                .fail_deal_state
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(dexdo_core::ChainError::Chain(
                    "injected deal_state failure".to_string(),
                ));
            }
            Ok(*self.deal_state.lock().unwrap())
        }

        async fn snapshot(
            &self,
            _token_contract: &TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    #[tokio::test]
    async fn preserve_lifetime_submits_no_implicit_terminal_chain_writes() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_failure_policy_and_lifetime(
            chain.clone(),
            "tc-subscription".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy {
                verification_bail: VerificationBailAction::Dispute,
                dead_gateway: DeadGatewayAction::RetryThenReclaim,
                empty_stream: EmptyStreamAction::Reclaim,
                seller_stalls_mid_stream: SellerStallsMidStreamAction::Dispute,
            },
            SessionLifetimePolicy::Preserve,
        );

        assert!(session.preserves_on_exit());
        assert!(!session.settle_on_exit("shutdown").await.unwrap());
        assert_eq!(session.terminal_action(), None);
        drop(session);
        tokio::task::yield_now().await;

        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn preserve_lifetime_vetoes_implicit_incident_chain_writes() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = |token_contract: &str, failure_policy| {
            SessionSettle::new_with_failure_policy_and_lifetime(
                chain.clone(),
                token_contract.to_string(),
                Arc::new(dexdo_core::LocalNote::generate()),
                failure_policy,
                SessionLifetimePolicy::Preserve,
            )
        };

        let bad_output = session(
            "tc-subscription-bad-output",
            BuyerApiFailurePolicy {
                verification_bail: VerificationBailAction::Dispute,
                ..BuyerApiFailurePolicy::default()
            },
        );
        assert!(
            !bad_output
                .settle_verification_bail("content-identity-bail")
                .await
        );
        assert_eq!(bad_output.terminal_action(), None);

        let dead_gateway = session(
            "tc-subscription-dead-gateway",
            BuyerApiFailurePolicy {
                dead_gateway: DeadGatewayAction::RetryThenReclaim,
                ..BuyerApiFailurePolicy::default()
            },
        );
        assert!(
            !dead_gateway
                .settle_dead_gateway("dead-gateway", &unchanged_heartbeat())
                .await
        );
        assert_eq!(dead_gateway.terminal_action(), None);

        let empty_stream = session(
            "tc-subscription-empty-stream",
            BuyerApiFailurePolicy {
                empty_stream: EmptyStreamAction::Reclaim,
                ..BuyerApiFailurePolicy::default()
            },
        );
        assert!(
            !empty_stream
                .settle_empty_stream("empty-stream", &unchanged_heartbeat())
                .await
        );

        let stalled = session(
            "tc-subscription-stalled",
            BuyerApiFailurePolicy {
                seller_stalls_mid_stream: SellerStallsMidStreamAction::Dispute,
                ..BuyerApiFailurePolicy::default()
            },
        );
        assert!(
            !stalled
                .settle_seller_stalls_mid_stream("stalled", &unchanged_heartbeat())
                .await
        );

        let direct_reclaim = session(
            "tc-subscription-direct-reclaim",
            BuyerApiFailurePolicy::default(),
        );
        assert!(direct_reclaim
            .recover_reclaim_opened(&unchanged_heartbeat(), false)
            .await
            .unwrap()
            .is_none());

        let cleanup = session("tc-subscription-unopened", BuyerApiFailurePolicy::default());
        assert!(cleanup
            .recover_cleanup_unopened(true)
            .await
            .unwrap()
            .is_none());

        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn preserve_lifetime_naked_drop_submits_no_stop() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = SessionSettle::new_with_failure_policy_and_lifetime(
            chain.clone(),
            "tc-subscription-drop".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy::default(),
            SessionLifetimePolicy::Preserve,
        );

        drop(session);
        tokio::task::yield_now().await;

        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn preserve_lifetime_does_not_veto_explicit_stop() {
        use std::sync::atomic::Ordering;

        let stop_chain = Arc::new(RecordingSettleChain::default());
        let stop = SessionSettle::new_with_failure_policy_and_lifetime(
            stop_chain.clone(),
            "tc-subscription-stop".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy::default(),
            SessionLifetimePolicy::Preserve,
        );
        assert!(stop.settle("explicit-user-stop").await.unwrap());
        assert_eq!(stop_chain.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            stop.terminal_action(),
            Some(SessionTerminalAction::StreamStop)
        );
        assert!(!stop.mark_recovered("later-terminal-observation"));
        assert_eq!(
            stop.terminal_action(),
            Some(SessionTerminalAction::StreamStop)
        );
    }

    fn shutdown_test_state(
        chain: Arc<RecordingSettleChain>,
        lifetime: SessionLifetimePolicy,
        token_contract: &str,
    ) -> ApiState {
        let note = Arc::new(dexdo_core::LocalNote::generate());
        ApiState::single(
            Arc::new(Buyer::from_note(note.clone())),
            Route {
                handover: Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "00".repeat(32),
                },
                token_contract: token_contract.to_string(),
                max_tokens: 100,
            },
            "qwen--qwen3--32b".to_string(),
            Arc::new(SessionSettle::new_with_failure_policy_and_lifetime(
                chain,
                token_contract.to_string(),
                note,
                BuyerApiFailurePolicy::default(),
                lifetime,
            )),
            Arc::new(ContentGate::skip()),
        )
    }

    /// Put a route through the accounting a SERVED request performs: begin the request, take an
    /// admitted reservation, charge terminal billing usage against it, and record the output heartbeat --
    /// the same `begin_request` -> `admit` -> `record_billing` -> `record_accepted_output`
    /// sequence `openai::chat_completions` runs (`openai.rs:48`, `:228`, `:289`).

    /// The routes in these tests point at a dead endpoint (`https://127.0.0.1:1`) and this crate's
    /// unit tests have no mock seller, so this is how a request delivers here. The end-to-end proof
    /// over a real gateway is `shutdown_terminal_is_bounded_by_delivered_tokens` in
    /// `crates/e2e/tests/consumer_api.rs`.
    async fn deliver_one_request(deals: &RouteManager, tokens: u64) {
        let deal = deals
            .current()
            .await
            .expect("the shutdown test state carries one active deal");
        let mut request = deal.begin_request(unix_now_secs());
        match deal.admit(Some(tokens as u32)).await {
            RouteBudget::Admitted(reservation) => request.hold(reservation),
            RouteBudget::Exhausted(reason) => panic!("route refused admission: {reason}"),
        }
        request
            .record_billing(&deal, tokens)
            .expect("terminal billing usage charges against the held reservation");
        deal.record_visible_output(tokens)
            .expect("accepted output updates the separate delivery witness");
        deal.record_accepted_output(unix_now_secs());
        assert_eq!(
            deal.session.route_billable_tokens(),
            Some(tokens),
            "the session must witness what the route billed"
        );
        assert_eq!(
            deal.session.route_delivered_tokens(),
            Some(tokens),
            "the session must separately witness accepted output"
        );
    }

    /// The lifetime policy decides the shutdown terminal: a durable subscription is PRESERVED across
    /// the operator close, an ordinary deal STOPs once.

    /// both legs deliver first, so the lifetime policy is the ONLY thing that differs between
    /// them and this test keeps asserting its own subject. Leaving the ordinary leg with nothing
    /// delivered would make it assert "no delivery -> no STOP" instead, which is what
    /// `shutdown_terminal_is_bounded_by_delivered_tokens` covers -- and the Preserve-vs-SettleOnExit
    /// distinction would have been silently deleted while the suite stayed green.
    #[tokio::test]
    async fn graceful_shutdown_preserves_subscription_but_ordinary_still_stops_once() {
        use std::sync::atomic::Ordering;

        let subscription_chain = Arc::new(RecordingSettleChain::default());
        let (subscription_shutdown_tx, subscription_shutdown_rx) =
            tokio::sync::oneshot::channel::<()>();
        let subscription_state = shutdown_test_state(
            subscription_chain.clone(),
            SessionLifetimePolicy::Preserve,
            "tc-subscription",
        );
        let subscription_deals = subscription_state.deals.clone();
        let (_, subscription_task) = serve(
            "127.0.0.1:0".parse().unwrap(),
            subscription_state,
            false,
            async move {
                let _ = subscription_shutdown_rx.await;
            },
        )
        .await
        .unwrap();
        deliver_one_request(&subscription_deals, 4).await;
        subscription_shutdown_tx.send(()).unwrap();
        subscription_task.await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(subscription_chain.stop_calls.load(Ordering::SeqCst), 0);

        let ordinary_chain = Arc::new(RecordingSettleChain::default());
        let (ordinary_shutdown_tx, ordinary_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let ordinary_state = shutdown_test_state(
            ordinary_chain.clone(),
            SessionLifetimePolicy::SettleOnExit,
            "tc-ordinary",
        );
        let ordinary_deals = ordinary_state.deals.clone();
        let (_, ordinary_task) = serve(
            "127.0.0.1:0".parse().unwrap(),
            ordinary_state,
            false,
            async move {
                let _ = ordinary_shutdown_rx.await;
            },
        )
        .await
        .unwrap();
        deliver_one_request(&ordinary_deals, 4).await;
        ordinary_shutdown_tx.send(()).unwrap();
        ordinary_task.await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(ordinary_chain.stop_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn visible_delivery_without_terminal_billing_still_allows_implicit_stop() {
        let chain = Arc::new(RecordingSettleChain::default());
        let state = shutdown_test_state(
            chain.clone(),
            SessionLifetimePolicy::SettleOnExit,
            "tc-partial-output",
        );
        let deal = state.deals.current().await.expect("active route");
        deal.record_visible_output(1)
            .expect("accepted output updates the delivery witness");
        assert_eq!(deal.session.route_delivered_tokens(), Some(1));
        assert_eq!(deal.session.route_billable_tokens(), Some(0));

        assert!(
            state.deals.settle_active_on_exit("shutdown").await.unwrap(),
            "accepted output must defeat the  no-delivery veto even without terminal usage"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    }

    fn recovery_test_deal(
        token_contract: &str,
        chain: Arc<RecordingSettleChain>,
        note: Arc<dyn Note>,
    ) -> ApiDeal {
        ApiDeal::new(
            Route {
                handover: Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "00".repeat(32),
                },
                token_contract: token_contract.to_string(),
                max_tokens: 100,
            },
            Arc::new(SessionSettle::new_with_failure_policy(
                chain,
                token_contract.to_string(),
                note,
                BuyerApiFailurePolicy {
                    dead_gateway: DeadGatewayAction::RetryThenReclaim,
                    ..BuyerApiFailurePolicy::default()
                },
            )),
            Arc::new(ContentGate::skip()),
        )
    }

    proptest::proptest! {
        #[test]
        fn issue_547_terminal_on_demand_route_initializes_once_for_concurrent_requests(
            pre_terminal_requests in 1usize..8,
            concurrent_requests in 1usize..32,
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let chain = Arc::new(RecordingSettleChain::default());
                chain.fail_recovery_stop.store(true, Ordering::SeqCst);
                let note: Arc<dyn Note> = Arc::new(dexdo_core::LocalNote::generate());
                let initial = recovery_test_deal("tc-dead", chain.clone(), note.clone());
                let initial_session = initial.session.clone();
                let init_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let init_calls_for_initializer = init_calls.clone();
                let chain_for_initializer = chain.clone();
                let note_for_initializer = note.clone();
                let routes = Arc::new(RouteManager::recoverable_lazy_with_active(
                    initial,
                    Arc::new(move || {
                        let init_calls = init_calls_for_initializer.clone();
                        let chain = chain_for_initializer.clone();
                        let note = note_for_initializer.clone();
                        Box::pin(async move {
                            init_calls.fetch_add(1, Ordering::SeqCst);
                            Ok(recovery_test_deal("tc-fresh", chain, note))
                        }) as DealInitFuture
                    }),
                    Duration::from_secs(1),
                ));

                let heartbeat = unchanged_heartbeat();
                assert!(
                    !initial_session
                        .settle_dead_gateway("injected-dead-gateway", &heartbeat)
                        .await
                );
                assert!(initial_session.is_closed());
                assert!(!initial_session.is_settled());
                for _ in 0..pre_terminal_requests {
                    assert_eq!(
                        routes.current_or_prepare().await.unwrap().route.token_contract,
                        "tc-dead"
                    );
                }
                assert_eq!(
                    init_calls.load(Ordering::SeqCst),
                    0,
                    "closed but nonterminal recovery must not move money"
                );

                chain.fail_recovery_stop.store(false, Ordering::SeqCst);
                assert_eq!(
                    initial_session.take_handler_recovery_reconciliation(),
                    Some(RecoveryKind::ReclaimOpened),
                    "the monitor must consume the handler failure before retrying from fresh facts"
                );
                assert!(initial_session
                    .recover_reclaim_opened(&unchanged_heartbeat(), false)
                    .await
                    .unwrap()
                    .is_some());
                assert!(initial_session.is_settled());

                let mut requests = Vec::with_capacity(concurrent_requests);
                for _ in 0..concurrent_requests {
                    let routes = routes.clone();
                    requests.push(tokio::spawn(async move {
                        routes.current_or_prepare().await.unwrap().route.token_contract
                    }));
                }
                for request in requests {
                    assert_eq!(request.await.unwrap(), "tc-fresh");
                }
                assert_eq!(
                    init_calls.load(Ordering::SeqCst),
                    1,
                    "serialized lazy replacement must submit at most one fresh BUY"
                );
            });
        }
    }

    #[tokio::test]
    async fn issue_547_accepted_output_during_reclaim_preflight_sends_no_post() {
        let chain = Arc::new(RecordingSettleChain::default());
        let note: Arc<dyn Note> = Arc::new(dexdo_core::LocalNote::generate());
        let deal = recovery_test_deal("tc-heartbeat-race", chain.clone(), note);
        *chain.heartbeat_during_reclaim_preflight.lock().unwrap() = Some(deal.clone());
        let heartbeat = deal.accepted_output_guard();

        let settlement = deal
            .session
            .recover_reclaim_opened(&heartbeat, false)
            .await
            .unwrap();

        assert!(settlement.is_none());
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        assert!(!deal.session.is_settled());
        assert!(!deal.session.is_closed());
        assert_eq!(
            deal.session.recovery_episode(),
            None,
            "a heartbeat-cancelled reclaim must not leave a stale recovery latch"
        );
    }

    #[tokio::test]
    async fn explicit_user_stop_is_not_vetoed_by_output_during_preflight() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let note: Arc<dyn Note> = Arc::new(dexdo_core::LocalNote::generate());
        let deal = recovery_test_deal("tc-explicit-stop", chain.clone(), note);
        *chain
            .heartbeat_during_explicit_stop_preflight
            .lock()
            .unwrap() = Some(deal.clone());
        assert_eq!(deal.accepted_output_generation(), 0);

        assert!(deal.session.settle("explicit-user-stop").await.unwrap());

        assert_eq!(
            deal.accepted_output_generation(),
            1,
            "the fixture must advance accepted output inside the STOP preflight window"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
        assert!(deal.session.is_settled());
    }

    #[tokio::test]
    async fn concurrent_policy_stop_attempts_serialize_per_token_contract() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let session = Arc::new(SessionSettle::new(
            chain.clone(),
            "tc-concurrent-policy-stop".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
        ));
        let mut attempts = Vec::new();
        for _ in 0..16 {
            let session = session.clone();
            attempts.push(tokio::spawn(async move {
                session
                    .recover_reclaim_opened(&unchanged_heartbeat(), false)
                    .await
                    .unwrap()
                    .is_some()
            }));
        }

        let mut landed = 0;
        for attempt in attempts {
            landed += usize::from(attempt.await.unwrap());
        }
        assert_eq!(landed, 1);
        assert_eq!(
            chain.recovery_stop_calls.load(Ordering::SeqCst),
            1,
            "one SessionSettle is the per-TC serialization boundary"
        );
        assert!(session.is_settled());
    }

    #[tokio::test]
    async fn failed_stop_keeps_previous_route_and_reaches_caller() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_stop.store(true, Ordering::SeqCst);
        let note = Arc::new(dexdo_core::LocalNote::generate());
        let previous_session = Arc::new(SessionSettle::new(
            chain.clone(),
            "tc-previous".to_string(),
            note.clone(),
        ));
        let next_session = Arc::new(SessionSettle::new(
            chain.clone(),
            "tc-next".to_string(),
            note,
        ));
        let deal = |token_contract: &str, session: Arc<SessionSettle>| {
            ApiDeal::new(
                Route {
                    handover: Handover {
                        endpoint: "https://127.0.0.1:1".to_string(),
                        tls_fingerprint: "00".repeat(32),
                    },
                    token_contract: token_contract.to_string(),
                    max_tokens: 1,
                },
                session,
                Arc::new(ContentGate::skip()),
            )
        };
        let routes = RouteManager::new(deal("tc-previous", previous_session.clone()));
        deliver_one_request(&routes, 1).await;
        let next_factory_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let factory_called = next_factory_called.clone();

        let error = routes
            .replace_active(
                || {
                    factory_called.store(true, Ordering::SeqCst);
                    deal("tc-next", next_session.clone())
                },
                "continuity-renewal",
            )
            .await
            .expect_err("old STOP failure must prevent route replacement");

        assert!(
            error.to_string().contains("injected stop failure"),
            "{error}"
        );
        assert_eq!(
            routes.current().await.unwrap().route.token_contract,
            "tc-previous"
        );
        assert!(!previous_session.is_settled());
        assert!(!next_session.is_settled());
        assert!(!next_factory_called.load(Ordering::SeqCst));
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);

        chain.fail_stop.store(false, Ordering::SeqCst);
        routes
            .replace_active(
                || {
                    factory_called.store(true, Ordering::SeqCst);
                    deal("tc-next", next_session.clone())
                },
                "continuity-renewal-retry",
            )
            .await
            .expect("an explicit retry can settle the old deal and install the pending route");
        assert!(previous_session.is_settled());
        assert!(!next_session.is_settled());
        assert!(next_factory_called.load(Ordering::SeqCst));
        assert_eq!(
            routes.current().await.unwrap().route.token_contract,
            "tc-next"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 2);
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
        assert!(session.settle("shutdown").await.unwrap());
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ambiguous_verification_bail_stop_latches_monitor_and_drop_without_repost() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        chain.ambiguous_stop.store(true, Ordering::SeqCst);
        let session = SessionSettle::new_with_verification_bail_action(
            chain.clone(),
            "tc-ambiguous-verification-stop".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            VerificationBailAction::Stop,
        );

        assert!(!session.settle_verification_bail("test-bail").await);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
        assert!(
            !session.settle_verification_bail("repeat-test-bail").await,
            "the same verification-bail entry point must reconcile instead of posting STOP again"
        );
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            1,
            "the same verification-bail entry point must remain suppressed"
        );
        assert!(session.recovery_submit_may_have_landed(RecoveryKind::ReclaimOpened));
        assert_eq!(
            session.take_handler_recovery_reconciliation(),
            Some(RecoveryKind::ReclaimOpened),
            "the service monitor must reconcile fresh facts instead of submitting STOP"
        );
        assert!(matches!(
            session.settle("shutdown").await,
            Err(dexdo_core::ChainError::AmbiguousSubmit(_))
        ));
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            1,
            "an alternate explicit STOP entry point must remain suppressed"
        );

        drop(session);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            1,
            "Drop must not retry an ambiguous verification-bail STOP"
        );
    }

    #[tokio::test]
    async fn ambiguous_stop_replaces_cleanup_episode_and_blocks_every_repost() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_cleanup_unopened.store(true, Ordering::SeqCst);
        let session = SessionSettle::new_with_verification_bail_action(
            chain.clone(),
            "tc-cleanup-then-ambiguous-stop".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            VerificationBailAction::Stop,
        );

        session
            .recover_cleanup_unopened(false)
            .await
            .expect_err("injected cleanup failure must leave its recovery episode visible");
        assert_eq!(
            session.recovery_episode(),
            Some(RecoveryKind::CleanupUnopened)
        );

        chain.ambiguous_stop.store(true, Ordering::SeqCst);
        assert!(!session.settle_verification_bail("test-bail").await);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            session.recovery_episode(),
            Some(RecoveryKind::ReclaimOpened),
            "an ambiguous STOP must supersede an older non-STOP recovery episode"
        );
        assert!(session.recovery_submit_may_have_landed(RecoveryKind::ReclaimOpened));

        assert!(!session.settle_verification_bail("repeat-bail").await);
        assert_eq!(
            session.take_handler_recovery_reconciliation(),
            Some(RecoveryKind::ReclaimOpened),
            "the monitor must observe the replacement STOP episode and reconcile facts"
        );
        assert!(matches!(
            session.settle("shutdown").await,
            Err(dexdo_core::ChainError::AmbiguousSubmit(_))
        ));
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            1,
            "neither repeated verification bail nor shutdown may repost STOP"
        );

        drop(session);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            1,
            "Drop must not retry after an ambiguous STOP replaced an older recovery episode"
        );
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

        assert!(session.settle("shutdown").await.unwrap());
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

        assert!(session.settle("shutdown").await.unwrap());
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dead_gateway_retry_then_reclaim_uses_recovery_stop() {
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

        assert!(
            session
                .settle_dead_gateway("test-dead-gateway", &unchanged_heartbeat())
                .await
        );
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn serving_gate_rejects_unopened_deal_before_timeout_without_cleanup() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let funded_time = unix_now_secs().saturating_sub(MATCH_OPEN_TIMEOUT_SECS / 2);
        let state = never_opened_deal_state(1_000, funded_time);
        assert_eq!(
            (
                state.probe_accepted,
                state.funded_time,
                state.last_claim_time,
            ),
            (false, Some(funded_time), funded_time),
            "serving gate must inspect a canonical 4.0.32 funded-never-opened state"
        );
        chain.set_deal_state(state);
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-not-open".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy::default(),
        );

        let err = session
            .ensure_open_for_serving()
            .await
            .expect_err("funded-never-opened must fail closed before serving");

        assert!(err.contains("opened=false"), "{err}");
        assert!(err.contains("cleanup_ready=false"), "{err}");
        assert!(
            session.is_closed(),
            "local API is closed before any response"
        );
        assert!(
            !session.is_settled(),
            "before MATCH_OPEN_TIMEOUT the session remains recoverable without a cleanup write"
        );
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            0,
            "fail-closed unopened path must not submit STOP"
        );
        assert_eq!(
            chain.recovery_stop_calls.load(Ordering::SeqCst),
            0,
            "fail-closed unopened path must not use opened-deal reclaim"
        );
    }

    #[tokio::test]
    async fn serving_gate_rejects_unopened_deal_after_timeout_with_cleanup() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let funded_time = unix_now_secs().saturating_sub(MATCH_OPEN_TIMEOUT_SECS.saturating_add(1));
        assert!(funded_time > 0, "expired funded time must remain positive");
        let state = never_opened_deal_state(1_000, funded_time);
        assert_eq!(
            (
                state.probe_accepted,
                state.funded_time,
                state.last_claim_time,
            ),
            (false, Some(funded_time), funded_time),
            "cleanup-ready serving gate must use a canonical 4.0.32 funded-never-opened state"
        );
        chain.set_deal_state(state);
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-not-open".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy::default(),
        );

        let err = session
            .ensure_open_for_serving()
            .await
            .expect_err("funded-never-opened must fail closed before serving");

        assert!(err.contains("opened=false"), "{err}");
        assert!(err.contains("cleanup_ready=true"), "{err}");
        assert!(
            session.is_closed(),
            "local API is closed before any response"
        );
        assert!(
            session.is_settled(),
            "timeout-ready cleanup marks the session terminal"
        );
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            0,
            "fail-closed unopened path must not submit STOP"
        );
        assert_eq!(
            chain.recovery_stop_calls.load(Ordering::SeqCst),
            0,
            "fail-closed unopened path must not use opened-deal reclaim"
        );
    }

    #[tokio::test]
    async fn serving_gate_allows_opened_deal() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        let funded_time = unix_now_secs();
        let state = opened_deal_state(1_000, funded_time);
        assert_eq!(
            (
                state.opened,
                state.probe_accepted,
                state.funded_time,
                state.last_claim_time,
            ),
            (true, false, Some(funded_time), funded_time),
            "opened serving fixture must remain distinct from never-opened cleanup facts"
        );
        chain.set_deal_state(state);
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-open".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy::default(),
        );

        session
            .ensure_open_for_serving()
            .await
            .expect("opened non-disputed deal can serve");

        assert!(!session.is_closed());
        assert!(!session.is_settled());
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
    }

    /// The first ordinary request must fit the seller's authoritative one-tick pre-probe capacity;
    /// the next request after `acceptProbe` must immediately regain the funded route remainder.
    #[tokio::test]
    async fn ordinary_admission_tracks_the_authoritative_probe_phase() {
        let tick = u64::try_from(dexdo_core::TICK_SIZE).expect("canonical tick fits u64");
        let route_budget = tick.checked_mul(3).expect("fixture route budget");
        let chain = Arc::new(RecordingSettleChain::default());
        let funded_time = unix_now_secs();
        chain.set_deal_state(opened_deal_state(u128::from(route_budget), funded_time));
        let session = Arc::new(SessionSettle::new(
            chain.clone(),
            "tc-ordinary-probe-phase".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
        ));
        let deal = ApiDeal::new(
            Route {
                handover: Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "00".repeat(32),
                },
                token_contract: "tc-ordinary-probe-phase".to_string(),
                max_tokens: route_budget,
            },
            session.clone(),
            Arc::new(ContentGate::skip()),
        );

        let before_probe = session
            .ensure_open_for_serving()
            .await
            .expect("opened pre-probe deal is servable");
        assert!(!before_probe.probe_accepted);
        let mut first_request = deal.begin_request(funded_time);
        let RouteBudget::Admitted(first) = deal
            .admit_with_probe_observation(Some(8), before_probe.probe_accepted)
            .await
        else {
            panic!("the trial-tick request must be admitted");
        };
        assert_eq!(first.granted, tick, "pre-probe grant is exactly one tick");
        first_request.hold(first);
        first_request
            .record_billing(&deal, 3)
            .expect("complete trial usage fits the one-tick grant");
        drop(first_request);

        let mut accepted = opened_deal_state(u128::from(route_budget), funded_time);
        accepted.probe_accepted = true;
        chain.set_deal_state(accepted);
        let after_probe = session
            .ensure_open_for_serving()
            .await
            .expect("accepted deal remains servable");
        assert!(after_probe.probe_accepted);
        let RouteBudget::Admitted(reopened) = deal
            .admit_with_probe_observation(Some(8), after_probe.probe_accepted)
            .await
        else {
            panic!("accepted ordinary deal must expose its funded remainder");
        };
        assert_eq!(
            reopened.granted,
            route_budget - 3,
            "acceptance reopens everything except already billed usage"
        );
        drop(reopened);

        let short_budget = tick - 1;
        let short_deal = ApiDeal::new(
            Route {
                handover: Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "00".repeat(32),
                },
                token_contract: "tc-short-ordinary-probe-phase".to_string(),
                max_tokens: short_budget,
            },
            Arc::new(SessionSettle::new(
                chain,
                "tc-short-ordinary-probe-phase".to_string(),
                Arc::new(dexdo_core::LocalNote::generate()),
            )),
            Arc::new(ContentGate::skip()),
        );
        let RouteBudget::Admitted(short) = short_deal
            .admit_with_probe_observation(Some(1), false)
            .await
        else {
            panic!("a funded sub-tick route remains fully usable pre-probe");
        };
        assert_eq!(short.granted, short_budget, "the trial ceiling uses min");
    }

    #[tokio::test]
    async fn empty_stream_reclaim_uses_recovery_stop() {
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

        assert!(
            session
                .settle_empty_stream("test-empty", &unchanged_heartbeat())
                .await
        );
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_recovery_stop_keeps_session_recoverable() {
        use std::sync::atomic::Ordering;

        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_recovery_stop.store(true, Ordering::SeqCst);
        let session = SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-timeout-failure".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy {
                empty_stream: EmptyStreamAction::Reclaim,
                ..BuyerApiFailurePolicy::default()
            },
        );

        assert!(
            !session
                .settle_empty_stream("test-empty", &unchanged_heartbeat())
                .await
        );
        assert!(
            session.is_closed(),
            "failed seller_timeout must close the local API route to a second request"
        );
        assert!(
            !session.is_settled(),
            "failed seller_timeout must not make the session terminal"
        );
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);

        assert!(session.settle("shutdown").await.unwrap());
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

        assert!(
            session
                .settle_seller_stalls_mid_stream("test-stall", &unchanged_heartbeat())
                .await
        );
        assert!(session.is_closed());
        assert!(session.is_settled());
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
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

        assert!(
            !session
                .settle_dead_gateway("test-next", &unchanged_heartbeat())
                .await
        );
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
        assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);

        assert!(session.settle("shutdown").await.unwrap());
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
    fn request_gate_checks_chain_open_before_content_probe_and_upstream() {
        let openai = include_str!("openai.rs");
        let anthropic = include_str!("anthropic.rs");
        for source in [openai, anthropic] {
            let open_gate = source
                .find("ensure_open_for_serving")
                .expect("handler gates by-fact open/accounting before serving");
            let content_gate = source
                .find(".content_gate")
                .expect("handler has content-identity probe");
            let upstream_open = source
                .find(".open_canon_stream")
                .expect("handler opens seller upstream");
            assert!(
                open_gate < content_gate,
                "by-fact open gate must run before content probe can consume gateway tokens"
            );
            assert!(
                open_gate < upstream_open,
                "by-fact open gate must run before user-visible upstream stream"
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

    /// A seller answering `RESOURCE_EXHAUSTED` is obeying the pre-`acceptProbe` trial-tick cap, not
    /// dying. Settling that as a dead gateway submits `TokenContract.stop()`, which on an unaccepted
    /// probe burns the probe tick and destroys a healthy deal -- the second request of a fresh
    /// multi-tick deal killed it every time.
    #[test]
    fn capacity_backpressure_is_not_a_dead_gateway() {
        assert!(is_capacity_backpressure(&anyhow::Error::from(
            tonic::Status::resource_exhausted("deal delivery capacity is exhausted")
        )));
        for other in [
            tonic::Status::unavailable("connection refused"),
            tonic::Status::unauthenticated("stream authorization refused"),
            tonic::Status::internal("seller gateway failed"),
        ] {
            assert!(
                !is_capacity_backpressure(&anyhow::Error::from(other)),
                "only the canonical capacity refusal is request-scoped"
            );
        }
        assert!(
            !is_capacity_backpressure(&anyhow::anyhow!(
                "transport error: deal delivery capacity is exhausted"
            )),
            "the classification is the gRPC code, never the message text"
        );

        for source in [include_str!("openai.rs"), include_str!("anthropic.rs")] {
            let classified = source
                .find("if is_capacity_backpressure(&error) {")
                .expect("both consumer surfaces classify the failed stream open");
            let settled = source
                .find("settle_dead_gateway(\"dead-gateway\", &reclaim_heartbeat)")
                .expect("both consumer surfaces keep the dead-gateway reclaim");
            assert!(
                classified < settled,
                "a capacity refusal must be answered before the deal is settled"
            );
        }
    }

    #[test]
    fn issue_547_recovery_policy_is_shared_by_openai_and_anthropic() {
        let openai = include_str!("openai.rs");
        let anthropic = include_str!("anthropic.rs");
        for source in [openai, anthropic] {
            assert!(
                source.contains("handle_stream_error_policy(&deal, received"),
                "both consumer surfaces must route stream errors through the shared  classifier"
            );
            assert!(
                source.contains("let reclaim_heartbeat = deal.accepted_output_guard();")
                    && source
                        .contains("settle_dead_gateway(\"dead-gateway\", &reclaim_heartbeat)",),
                "both consumer surfaces must use the same guarded dead-gateway reclaim path"
            );
        }
        let api = include_str!("mod.rs");
        assert!(api.contains("settle_dead_gateway(\"stream-error-before-token\", &heartbeat)"));
        assert!(api
            .contains("settle_seller_stalls_mid_stream(\"seller-stalls-mid-stream\", &heartbeat)"));
    }

    // ----------------------------------------------------------------------------------------
    // a running subscription route must follow the week the CONTRACT is in.

    // The chain double below is a small faithful `TokenContract`: `settleWeek` books a boundary only
    // when the CHAIN says one is due and otherwise refuses (`ERR_SETTLE_WINDOW_OPEN`), and booking
    // re-bases `weekBaseTokens` on the cumulative claim exactly as `_chargeWeeksThrough` does. The
    // buyer's clock is moved only through `period_start`, which is what the contract itself measures
    // against - so a test can never authorize a week the "chain" has not crossed.

    // The upstream is registered deliberately unconstrained: the seller's own capacity accounting has
    // its own regressions, and here every refusal must be the buyer's weekly budget and nothing else.
    // ----------------------------------------------------------------------------------------

    /// Two ticks a week over a four-week term, eight ticks funded.

    /// Not the smallest shape the book accepts - `InferenceOrderBook.sol:1309` requires only that the
    /// tick count divide by `SUB_WEEKS`, so FOUR ticks (one a week) is the true minimum. Two a week is
    /// the smallest shape in which the accepted probe's tick is visible as a part of a week rather
    /// than the whole of it.
    const WEEK_QUOTA: u128 = 2 * dexdo_core::TICK_SIZE;

    /// What `acceptProbe` has already paid for and claimed: the trial tick (`TokenContract.sol:690`).
    /// No probe-accepted deal is ever below this, on any of the three claim stages.
    const PROBE_CLAIM: u128 = dexdo_core::TICK_SIZE;

    /// Value of one tick in this fixture, so a booking's money movement is a real subtraction rather
    /// than a token count standing in for one.
    const TEST_TICK_VALUE: u128 = 3;

    /// The whole term's escrow: eight ticks at [`TEST_TICK_VALUE`].
    const TEST_DEPOSIT: u128 = 8 * TEST_TICK_VALUE;

    struct WeeklyQuotaChain {
        token_contract: TokenContract,
        snapshot: std::sync::Mutex<dexdo_core::DealChainSnapshot>,
        /// Boundaries the CHAIN clock has crossed and nobody has booked yet.
        due_boundaries: std::sync::Mutex<u8>,
        settle_fails: std::sync::atomic::AtomicBool,
        /// The counterparty files a dispute while a booking submission is in flight.
        dispute_on_settle: std::sync::atomic::AtomicBool,
        /// Reads still to be answered from the state as it stood BEFORE the last booking.
        stale_reads: std::sync::atomic::AtomicUsize,
        /// What the last read would have returned had it not been served stale.
        pre_booking: std::sync::Mutex<Option<dexdo_core::DealChainSnapshot>>,
        snapshot_reads: std::sync::atomic::AtomicUsize,
        settle_calls: std::sync::atomic::AtomicUsize,
        settle_bookings: std::sync::atomic::AtomicUsize,
        dispute_calls: std::sync::atomic::AtomicUsize,
        /// Calls that would move value the route is NEVER allowed to move: a new BUY commitment,
        /// a claim, an exit. The boundary booking is not one of them - it is money, but money the
        /// term already owed, so it is measured by `settle_bookings` and by the deposit itself.
        foreign_money_calls: std::sync::atomic::AtomicUsize,
    }

    impl WeeklyQuotaChain {
        fn new(token_contract: &str, period_start: u64, claimed: u128) -> Self {
            Self {
                token_contract: token_contract.to_string(),
                snapshot: std::sync::Mutex::new(dexdo_core::DealChainSnapshot {
                    account_code_hash: "code".to_string(),
                    account_boc_hash: "boc".to_string(),
                    state: weekly_state(claimed),
                    subscription: weekly_subscription(period_start, 0, 0),
                    seller_bond: dexdo_core::DealSellerBond {
                        bond_funded: true,
                        bond_held: 2,
                        bond_required: 2,
                    },
                    buyer_bond: dexdo_core::DealBuyerBond {
                        bond_held: 2,
                        bond_required: 2,
                    },
                }),
                due_boundaries: std::sync::Mutex::new(0),
                settle_fails: std::sync::atomic::AtomicBool::new(false),
                dispute_on_settle: std::sync::atomic::AtomicBool::new(false),
                stale_reads: std::sync::atomic::AtomicUsize::new(0),
                pre_booking: std::sync::Mutex::new(None),
                snapshot_reads: std::sync::atomic::AtomicUsize::new(0),
                settle_calls: std::sync::atomic::AtomicUsize::new(0),
                settle_bookings: std::sync::atomic::AtomicUsize::new(0),
                dispute_calls: std::sync::atomic::AtomicUsize::new(0),
                foreign_money_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.snapshot_reads.load(Ordering::SeqCst)
        }

        fn settle_calls(&self) -> usize {
            self.settle_calls.load(Ordering::SeqCst)
        }

        fn bookings(&self) -> usize {
            self.settle_bookings.load(Ordering::SeqCst)
        }

        fn dispute_calls(&self) -> usize {
            self.dispute_calls.load(Ordering::SeqCst)
        }

        fn foreign_money_calls(&self) -> usize {
            self.foreign_money_calls.load(Ordering::SeqCst)
        }

        /// Value the bookings have moved out of escrow and credited to the seller.
        fn settled_value(&self) -> (u128, u128) {
            let snapshot = self.snapshot.lock().unwrap();
            (snapshot.state.deposit, snapshot.state.finalized_owed)
        }

        /// The authoritative books, for a test that must pin the contract's own figure rather than
        /// assert an inequality the route could satisfy for the wrong reason.
        fn books(&self) -> (dexdo_core::DealChainState, dexdo_core::DealSubscription) {
            let snapshot = self.snapshot.lock().unwrap();
            (snapshot.state, snapshot.subscription)
        }

        /// The next read answers from BEFORE the booking that precedes it - an ordinary lagging read.
        fn serve_one_stale_read(&self) {
            self.stale_reads.fetch_add(1, Ordering::SeqCst);
        }

        fn week_index(&self) -> u8 {
            self.snapshot.lock().unwrap().subscription.week_index
        }

        /// The seller lands a cumulative claim - the only thing that moves `tokensPending`.
        fn seller_claims(&self, cumulative: u128) {
            self.snapshot.lock().unwrap().state = weekly_state(cumulative);
        }

        /// The CHAIN clock crosses `weeks` boundaries. Nothing is booked by this: `weekIndex` and
        /// `weekBaseTokens` stay exactly where they were, as the live getter leaves them.
        fn chain_crosses_boundaries(&self, weeks: u8) {
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.subscription.period_start = snapshot
                .subscription
                .period_start
                .saturating_sub(u64::from(weeks) * SUB_WEEK_LEN.as_secs());
            *self.due_boundaries.lock().unwrap() += weeks;
        }

        /// `acceptProbe`: the trial tick is accepted, seeding all three claim stages and the money
        /// mark with one `TICK_SIZE` and leaving `weekBaseTokens` at zero - week one counts the probe
        /// against its own quota (`TokenContract.sol:690-696`).
        fn accepts_probe(&self) {
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.state.probe_accepted = true;
            snapshot.state.tokens_final = PROBE_CLAIM;
            snapshot.state.tokens_pending = PROBE_CLAIM;
            snapshot.subscription.tokens_paid = PROBE_CLAIM;
        }

        /// The counterparty disputes the deal while the buyer's boundary booking is in flight.

        /// This is the only ordering in which a request can reach the reconciliation with a
        /// disputed deal at all: the serving gate ([`SessionSettle::ensure_open_for_serving`]) reads
        /// the deal a moment earlier and refuses a disputed one on its own terms, so what the
        /// reconciliation defends against is precisely a dispute that lands inside the submission
        /// window it is already past.
        fn disputes_while_booking(&self) {
            self.dispute_on_settle
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A probe-accepted deal at cumulative claim `claimed`.

    /// `acceptProbe` seeds ALL THREE claim stages and the money mark with one `TICK_SIZE`
    /// (`TokenContract.sol:690-696`), so `claimed` counts the trial tick and can never be below it.
    /// A fixture starting at zero would be a deal no chain can report, and every figure measured from
    /// it would be a tick too generous.
    fn weekly_state(claimed: u128) -> dexdo_core::DealChainState {
        // Zero is the OPEN-but-not-yet-accepted shape; anything else is post-`acceptProbe` and can
        // never be below the trial tick it seeded.
        assert!(
            claimed == 0 || claimed >= PROBE_CLAIM,
            "a probe-accepted deal has already claimed its trial tick"
        );
        dexdo_core::DealChainState {
            funded: true,
            opened: true,
            probe_accepted: claimed > 0,
            disputed: false,
            deposit: TEST_DEPOSIT,
            finalized_owed: 0,
            tokens_final: claimed,
            tokens_pending: claimed,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 0,
        }
    }

    /// `tokens_paid` is the money mark: `acceptProbe` seeds it with one `TICK_SIZE` and every booked
    /// boundary raises it to `(weekIndex + 1) * tokensPerWeek`, so after booking to week `k` it stands
    /// at `k * tokensPerWeek`. It is never zero -- a subscription's volume is a whole number of weeks
    /// of ticks, so `tokensPerWeek >= TICK_SIZE` and no assignment can go under the probe's tick.
    fn weekly_subscription(
        period_start: u64,
        week_index: u8,
        week_base_tokens: u128,
    ) -> dexdo_core::DealSubscription {
        dexdo_core::DealSubscription {
            deal_flags: dexdo_core::order_flags::SUBSCRIPTION,
            sub_weeks: dexdo_core::SUBSCRIPTION_WEEKS,
            week_index,
            tokens_per_week: WEEK_QUOTA,
            funded_tokens: WEEK_QUOTA * u128::from(dexdo_core::SUBSCRIPTION_WEEKS),
            tokens_paid: (u128::from(week_index) * WEEK_QUOTA).max(dexdo_core::TICK_SIZE),
            period_start,
            week_base_tokens,
        }
    }

    #[async_trait::async_trait]
    impl ChainBackend for WeeklyQuotaChain {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            unimplemented!("the weekly-quota route never discovers offers")
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("buyer-only backend")
        }

        async fn place_buy(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<(), dexdo_core::ChainError> {
            self.foreign_money_calls.fetch_add(1, Ordering::SeqCst);
            Err(dexdo_core::ChainError::Chain(
                "a weekly reconciliation must never buy anything".to_string(),
            ))
        }

        async fn read_match(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("the route already holds its handover")
        }

        async fn open_stream(
            &self,
            _token_contract: &TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("buyer-only backend")
        }

        async fn read_handover(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            Ok(None)
        }

        async fn claim_tokens(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
            _cumulative_tokens: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            self.foreign_money_calls.fetch_add(1, Ordering::SeqCst);
            Err(dexdo_core::ChainError::Chain(
                "the buyer never claims".to_string(),
            ))
        }

        /// The permissionless boundary booking, as the contract implements it: it settles only what
        /// the CHAIN has actually crossed, and refuses when the window is still open.

        /// It is a MONEY PATH and is modelled as one. `_chargeWeeksThrough` charges each week it
        /// books - `_deposit -= pay; _finalizedOwed += pay` (`TokenContract.sol:922-933`) - against
        /// `due = (weekIndex + 1) * tokensPerWeek - tokensPaid`, clamped by what the deposit still
        /// holds. What it never does is commit anything NEW: those weeks are already owed and every
        /// exit charges them anyway.
        async fn settle_week(
            &self,
            token_contract: &TokenContract,
        ) -> Result<(), dexdo_core::ChainError> {
            assert_eq!(token_contract, &self.token_contract);
            self.settle_calls.fetch_add(1, Ordering::SeqCst);
            if self.dispute_on_settle.load(Ordering::SeqCst) {
                // Somebody else's transaction lands first: from here on every read of this account
                // is a disputed one.
                self.snapshot.lock().unwrap().state.disputed = true;
            }
            if self.settle_fails.load(Ordering::SeqCst) {
                return Err(dexdo_core::ChainError::Chain(
                    "settleWeek submission failed".to_string(),
                ));
            }
            let mut due = self.due_boundaries.lock().unwrap();
            let mut snapshot = self.snapshot.lock().unwrap();
            *self.pre_booking.lock().unwrap() = Some(snapshot.clone());
            if *due == 0 || snapshot.subscription.term_is_over() {
                return Err(dexdo_core::ChainError::Chain(
                    "ERR_SETTLE_WINDOW_OPEN".to_string(),
                ));
            }
            while *due > 0 && !snapshot.subscription.term_is_over() {
                // `_chargeWeeksThrough`, in order: charge the week, then advance the books.
                let target = u128::from(snapshot.subscription.week_index + 1) * WEEK_QUOTA;
                let owed_tokens = target.saturating_sub(snapshot.subscription.tokens_paid);
                let pay = (owed_tokens / dexdo_core::TICK_SIZE)
                    .saturating_mul(TEST_TICK_VALUE)
                    .min(snapshot.state.deposit);
                snapshot.state.deposit -= pay;
                snapshot.state.finalized_owed += pay;
                snapshot.subscription.tokens_paid = target;
                snapshot.subscription.week_index += 1;
                // The new week starts from what has been consumed so far.
                snapshot.subscription.week_base_tokens = snapshot.state.tokens_pending;
                *due -= 1;
            }
            self.settle_bookings.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn stop(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.foreign_money_calls.fetch_add(1, Ordering::SeqCst);
            Ok(dexdo_core::Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            })
        }

        async fn dispute(
            &self,
            token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            assert_eq!(token_contract, &self.token_contract);
            self.dispute_calls.fetch_add(1, Ordering::SeqCst);
            Ok(dexdo_core::Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            })
        }

        async fn deal_snapshot(
            &self,
            token_contract: &TokenContract,
        ) -> Result<Option<dexdo_core::DealChainSnapshot>, dexdo_core::ChainError> {
            assert_eq!(token_contract, &self.token_contract);
            self.snapshot_reads.fetch_add(1, Ordering::SeqCst);
            if self
                .stale_reads
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                if let Some(stale) = self.pre_booking.lock().unwrap().clone() {
                    return Ok(Some(stale));
                }
            }
            Ok(Some(self.snapshot.lock().unwrap().clone()))
        }

        async fn deal_state(
            &self,
            token_contract: &TokenContract,
        ) -> Result<Option<dexdo_core::DealChainState>, dexdo_core::ChainError> {
            assert_eq!(token_contract, &self.token_contract);
            Ok(Some(self.snapshot.lock().unwrap().state))
        }

        async fn snapshot(
            &self,
            _token_contract: &TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    /// One buyer endpoint in front of one real TLS gateway.
    struct WeeklyRouteHarness {
        addr: SocketAddr,
        buyer: Arc<Buyer>,
        chain: Arc<WeeklyQuotaChain>,
        deals: Arc<RouteManager>,
        seller: super::fixture_seller::RunningFixtureSeller,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        task: Option<tokio::task::JoinHandle<()>>,
        /// Everything the endpoint published about the requests it finished, collected off
        /// the production channel rather than reconstructed from the response body.
        deliveries: Arc<std::sync::Mutex<Vec<RequestDelivery>>>,
    }

    impl WeeklyRouteHarness {
        async fn ask(&self, max_tokens: u64) -> (reqwest::StatusCode, String) {
            self.ask_path("/v1/chat/completions", max_tokens).await
        }

        /// Both consumer paths take the same request fields, so one body drives either endpoint --
        /// which is what makes the two paths comparable when they compete for one remainder.
        async fn ask_path(&self, path: &str, max_tokens: u64) -> (reqwest::StatusCode, String) {
            // Most weekly-boundary tests isolate output accounting. Keep their mock input at zero;
            // input-plus-output fixtures below pass an explicit non-empty prompt.
            self.ask_full(path, max_tokens, "", true).await
        }

        /// One request with everything the adversarial cases need to vary: which consumer protocol,
        /// what the request may cost, what the seller is asked to do, and whether the answer is
        /// streamed or aggregated.
        async fn ask_full(
            &self,
            path: &str,
            max_tokens: u64,
            prompt: &str,
            stream: bool,
        ) -> (reqwest::StatusCode, String) {
            let body = serde_json::json!({
                "model": "dexdo-mock",
                "messages": [{"role": "user", "content": prompt}],
                "max_tokens": max_tokens,
                "stream": stream
            });
            let response = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap()
                .post(format!("http://{}{path}", self.addr))
                .json(&body)
                .send()
                .await
                .expect("the local endpoint answers");
            let status = response.status();
            (status, response.text().await.expect("body"))
        }

        /// What the LIVE route says it may still hand out - the production figure admission gates on.
        async fn remaining(&self) -> u64 {
            self.deals
                .current()
                .await
                .expect("the harness route is published")
                .remaining_tokens()
        }

        async fn delivered(&self) -> u64 {
            self.deals
                .current()
                .await
                .expect("the harness route is published")
                .billable_tokens()
        }

        /// The delivery records this endpoint has published so far.
        fn deliveries(&self) -> Vec<RequestDelivery> {
            self.deliveries
                .lock()
                .expect("delivery capture lock poisoned")
                .clone()
        }

        /// The record of the LAST finished request. The pump runs on its own task, so a request
        /// that has already answered may not have been collected yet.
        async fn last_delivery(&self) -> RequestDelivery {
            for _ in 0..200 {
                if let Some(last) = self.deliveries().pop() {
                    return last;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the endpoint published no delivery record for the finished request");
        }

        async fn shutdown(mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(task) = self.task.take() {
                let _ = task.await;
            }
            self.seller.server_task.abort();
        }
    }

    /// The terminal reason the consumer was actually shown, read off the wire for either protocol
    /// and either response shape. `None` when the body carries no terminal frame at all.
    fn terminal_reason(body: &str) -> Option<String> {
        let mut last = None;
        for line in body.lines() {
            let payload = line.strip_prefix("data: ").unwrap_or(line);
            if payload.trim().is_empty() || payload.trim() == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            for candidate in [
                &v["choices"][0]["finish_reason"],
                &v["stop_reason"],
                &v["delta"]["stop_reason"],
            ] {
                if let Some(reason) = candidate.as_str() {
                    last = Some(reason.to_string());
                }
            }
        }
        last
    }

    /// The seller emits as many tokens as it is asked for: unconstrained, so that every refusal in
    /// these tests is the buyer's weekly budget and nothing else.
    const UNCONSTRAINED_UPSTREAM: u64 = 64 * dexdo_core::TICK_SIZE as u64;

    /// A route built BEFORE the trial tick was accepted: `open()` has funded and opened the deal but
    /// `acceptProbe` has not run, so every claim stage is genuinely zero.
    async fn weekly_route_harness_before_probe(expires_in: u64) -> WeeklyRouteHarness {
        weekly_route_harness_with_upstream(0, true, expires_in, UNCONSTRAINED_UPSTREAM).await
    }

    async fn weekly_route_harness(claimed: u128, subscription: bool) -> WeeklyRouteHarness {
        weekly_route_harness_expiring_in(claimed, subscription, SUB_WEEK_LEN.as_secs()).await
    }

    async fn weekly_route_harness_expiring_in(
        claimed: u128,
        subscription: bool,
        expires_in: u64,
    ) -> WeeklyRouteHarness {
        weekly_route_harness_with_upstream(
            claimed,
            subscription,
            expires_in,
            UNCONSTRAINED_UPSTREAM,
        )
        .await
    }

    /// `expires_in` is how many seconds of the recorded week are left on the WALL CLOCK when the route
    /// is built. A short value lets a test cross the boundary of a RUNNING route by waiting, which is
    /// the only thing that happens in production: `periodStart` never moves, the clock does.

    /// `upstream_tokens` is how many tokens the seller's model will emit at most. Below the request's
    /// own cap it is a model that simply stops early - which is what leaves part of a reservation
    /// unused.
    async fn weekly_route_harness_with_upstream(
        claimed: u128,
        subscription: bool,
        expires_in: u64,
        upstream_tokens: u64,
    ) -> WeeklyRouteHarness {
        weekly_route_harness_gated(
            claimed,
            subscription,
            expires_in,
            upstream_tokens,
            ContentGate::skip(),
            crate::seller::UpstreamConfig::Mock,
        )
        .await
    }

    /// The same route with a real content-identity gate, so a test can watch what verification
    /// itself spends out of the admitted grant.
    async fn weekly_route_harness_gated(
        claimed: u128,
        subscription: bool,
        expires_in: u64,
        upstream_tokens: u64,
        content_gate: ContentGate,
        upstream: crate::seller::UpstreamConfig,
    ) -> WeeklyRouteHarness {
        weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
            claimed,
            subscription,
            expires_in,
            upstream_tokens,
            content_gate,
            upstream,
            failure_policy: BuyerApiFailurePolicy::default(),
            lifetime: SessionLifetimePolicy::Preserve,
        })
        .await
    }

    struct WeeklyRouteScenario {
        claimed: u128,
        subscription: bool,
        expires_in: u64,
        upstream_tokens: u64,
        content_gate: ContentGate,
        upstream: crate::seller::UpstreamConfig,
        failure_policy: BuyerApiFailurePolicy,
        lifetime: SessionLifetimePolicy,
    }

    /// The gated weekly route with an explicit incident policy. Only rows that must observe the
    /// real verification-bail chain action use this extension; the existing weekly fixture keeps
    /// its original defaults and call surface.
    async fn weekly_route_harness_gated_with_policy(
        scenario: WeeklyRouteScenario,
    ) -> WeeklyRouteHarness {
        let WeeklyRouteScenario {
            claimed,
            subscription,
            expires_in,
            upstream_tokens,
            content_gate,
            upstream,
            failure_policy,
            lifetime,
        } = scenario;
        let token_contract = "0:".to_string() + &"9".repeat(64);
        let period_start = unix_now_secs() + expires_in - SUB_WEEK_LEN.as_secs();
        let chain = Arc::new(WeeklyQuotaChain::new(
            &token_contract,
            period_start,
            claimed,
        ));

        // shape B: the gateway makes the ONE bind. Reserving a port here and releasing it
        // before `start_gateway_with` re-binds hands it back to the kernel, and any concurrent
        // `bind(0)` can be given that exact port in between.
        let mock_model = matches!(
            &upstream,
            crate::seller::UpstreamConfig::Mock
                | crate::seller::UpstreamConfig::MockWithClaimedModel(_)
                | crate::seller::UpstreamConfig::MockScammer
        );
        let seller =
            super::fixture_seller::start_gateway_with("127.0.0.1:0".parse().unwrap(), upstream)
                .await
                .expect("TLS mock gateway");
        let gateway_addr = seller.listen_addr;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(gateway_addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let note: Arc<dyn Note> = Arc::new(dexdo_core::LocalNote::generate());
        let buyer = Arc::new(Buyer::from_note(note.clone()));
        // The standalone gateway has no background chain observer in this fixture. Seed its capacity
        // anchor from the same coherent state as the buyer so the tests isolate buyer-side admission;
        // dedicated seller-capacity tests exercise stale and advancing anchors.
        let upstream_state = weekly_state(claimed);
        seller
            .register_stream(
                &token_contract,
                note.pubkey(),
                upstream_tokens,
                upstream_state,
                dexdo_core::DealSubscription {
                    deal_flags: 0,
                    sub_weeks: 0,
                    week_index: 0,
                    tokens_per_week: 64 * dexdo_core::TICK_SIZE,
                    funded_tokens: 64 * dexdo_core::TICK_SIZE,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                },
            )
            .expect("register the upstream stream");

        let initial = weekly_subscription(period_start, 0, 0);
        let headroom =
            dexdo_core::subscription_current_week_headroom(&weekly_state(claimed), &initial)
                .expect("recorded week headroom");
        let route = Route {
            handover: Handover {
                endpoint: format!("https://{gateway_addr}"),
                tls_fingerprint: seller.tls_fingerprint.clone(),
            },
            token_contract: token_contract.clone(),
            max_tokens: u64::try_from(headroom).unwrap(),
        };
        let session = Arc::new(SessionSettle::new_with_failure_policy_and_lifetime(
            chain.clone(),
            token_contract.clone(),
            note,
            failure_policy,
            lifetime,
        ));
        let deal = ApiDeal::new(route, session, Arc::new(content_gate));
        let deal = if subscription {
            deal.with_weekly_budget(Arc::new(SubscriptionWeeklyBudget::new(
                chain.clone(),
                token_contract.clone(),
                &weekly_state(claimed),
                &initial,
            )))
        } else {
            deal
        };
        let mut state = ApiState::single_deal(buyer.clone(), "dexdo-mock".to_string(), deal)
            .with_mock_model(mock_model);
        let deals = state.deals.clone();
        // the harness reads the delivery records off the production channel, so what a test
        // asserts is what an operator's JSONL surface would have received.
        let (delivery_tx, mut delivery_rx) = tokio::sync::mpsc::unbounded_channel();
        state.delivery_events = Some(delivery_tx);
        let deliveries = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collector = deliveries.clone();
        tokio::spawn(async move {
            while let Some(delivery) = delivery_rx.recv().await {
                collector
                    .lock()
                    .expect("delivery capture lock poisoned")
                    .push(delivery);
            }
        });

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), state, true, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("bind the local endpoint");

        WeeklyRouteHarness {
            addr,
            buyer,
            chain,
            deals,
            seller,
            shutdown: Some(shutdown_tx),
            task: Some(task),
            deliveries,
        }
    }

    #[tokio::test]
    async fn booked_boundary_serves_the_next_week_without_a_restart() {
        // Week one with only the accepted probe claimed: the quota LESS that trial tick is available.
        let harness = weekly_route_harness_expiring_in(PROBE_CLAIM, true, 2).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();
        assert_eq!(harness.remaining().await, quota - probe);

        let (status, body) = harness.ask(8).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(harness.delivered().await, 8);

        // The seller claims what he has actually served - the probe and those eight tokens - and the
        // CHAIN crosses one boundary. Nobody has booked it, so the getter still reads
        // weekIndex=0/weekBaseTokens=0: the stale-getter shape of.
        harness.chain.seller_claims(PROBE_CLAIM + 8);
        harness.chain.chain_crosses_boundaries(1);
        tokio::time::sleep(Duration::from_millis(2_200)).await;
        assert_eq!(harness.chain.week_index(), 0);

        let (status, body) = harness.ask(8).await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "the same running route must serve week two without a restart: {body}"
        );
        assert_eq!(
            harness.chain.bookings(),
            1,
            "the new week must come from the permissionless booking, never from the buyer's clock"
        );
        assert_eq!(harness.chain.week_index(), 1);
        // The new week's allowance is a whole quota measured from the cumulative claim, and the 8
        // tokens of the old week were NOT carried into it.
        assert_eq!(harness.remaining().await, quota - 8);
        // The booking is a money path: it charged week one out of escrow and credited the seller.
        // One tick, not two - `acceptProbe` had already paid for the trial tick of week one, and
        // `_chargeWeeksThrough` charges up to the cumulative total the term owes rather than a flat
        // quota on top of it.
        let (deposit, finalized_owed) = harness.chain.settled_value();
        assert_eq!(finalized_owed, TEST_TICK_VALUE);
        assert_eq!(deposit, TEST_DEPOSIT - TEST_TICK_VALUE);
        // ...but no NEW commitment: no buy, no claim, no exit.
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn an_under_used_week_is_forfeited_at_its_boundary_and_never_rolls_over() {
        // Week one barely used: a large POSITIVE remainder is cached on the route.
        let harness = weekly_route_harness_expiring_in(PROBE_CLAIM, true, 2).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();
        let (status, body) = harness.ask(4).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(harness.remaining().await, quota - probe - 4);

        // The chain crosses the boundary with that remainder still positive. The seller claims only
        // what he served: the probe's tick and those four tokens.
        harness.chain.seller_claims(PROBE_CLAIM + 4);
        harness.chain.chain_crosses_boundaries(1);
        tokio::time::sleep(Duration::from_millis(2_200)).await;

        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(
            harness.chain.bookings(),
            1,
            "a boundary must be reconciled even when the cached remainder is positive"
        );
        // One quota from the new base, NOT the new quota plus what week one left unspent.
        assert_eq!(harness.remaining().await, quota - 1);
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn a_positive_remainder_does_not_survive_the_final_boundary() {
        let harness = weekly_route_harness_expiring_in(PROBE_CLAIM, true, 2).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let (status, body) = harness.ask(4).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(
            harness.remaining().await > 0,
            "a positive remainder is cached"
        );

        // The whole term elapses on the chain while that remainder is still spendable.
        harness
            .chain
            .chain_crosses_boundaries(dexdo_core::SUBSCRIPTION_WEEKS);
        tokio::time::sleep(Duration::from_millis(2_200)).await;

        let (status, body) = harness.ask(1).await;
        assert_eq!(
            status,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "a finished term may not keep serving a stale remainder: {body}"
        );
        assert!(body.contains("full 4-week term"), "{body}");
        assert_eq!(harness.remaining().await, 0);
        assert_eq!(harness.chain.week_index(), dexdo_core::SUBSCRIPTION_WEEKS);
        // ...and it never comes back.
        harness.chain.seller_claims(quota.into());
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.contains("terminal"), "{body}");
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_booking_authorizes_nothing_and_names_the_next_boundary() {
        // The recorded week is spent.
        let harness = weekly_route_harness(WEEK_QUOTA, true).await;
        let period_start = harness
            .chain
            .snapshot
            .lock()
            .unwrap()
            .subscription
            .period_start;

        // No boundary is due: the contract refuses the booking and nothing may be served.
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.contains("no boundary was due"), "{body}");
        assert!(
            body.contains(&format!(
                "next weekly quota opens at unix {}",
                period_start + SUB_WEEK_LEN.as_secs()
            )),
            "a temporary weekly state must report when it lifts: {body}"
        );
        assert_eq!(harness.chain.settle_calls(), 1);
        assert_eq!(harness.chain.bookings(), 0);

        // A boundary IS due, but the settlement submission itself fails: fail closed, no allowance.
        harness.chain.chain_crosses_boundaries(1);
        harness
            .chain
            .settle_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (status, body) = harness.ask(1).await;
        assert_eq!(
            status,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "an unbooked boundary is not an allowance: {body}"
        );
        assert_eq!(harness.chain.bookings(), 0);
        assert_eq!(harness.remaining().await, 0);

        // Once the booking lands, the same route serves again.
        harness
            .chain
            .settle_fails
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(harness.chain.bookings(), 1);
        // The one booking that landed charged the week it booked - one tick, the part of week one the
        // accepted probe had not already paid - and nothing else moved value: the two refused
        // attempts before it charged nothing at all.
        let (deposit, finalized_owed) = harness.chain.settled_value();
        assert_eq!(finalized_owed, TEST_TICK_VALUE);
        assert_eq!(deposit, TEST_DEPOSIT - TEST_TICK_VALUE);
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        harness.shutdown().await;
    }

    /// A dispute that lands while the booking is in flight is what the reconciliation's
    /// disputed/stopped branch is for: the request is already past the serving gate, which read a
    /// clean deal a moment earlier. The reconciliation is then the last reader of authoritative
    /// state before anything is served, and what it latches is forever - a boundary the chain
    /// crosses afterwards is the strongest revival there is, and it must republish nothing.
    #[tokio::test]
    async fn terminal_subscription_is_never_revived_by_a_reconciliation() {
        // The recorded week is spent, so the request goes to the chain rather than serving from a
        // cached remainder.
        let harness = weekly_route_harness(WEEK_QUOTA, true).await;
        harness.chain.disputes_while_booking();

        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.contains("disputed/stopped"), "{body}");

        // A boundary is now genuinely due: booking it would measure a whole fresh quota from the
        // cumulative claim, which is exactly the revival a latched route may not have.
        harness.chain.chain_crosses_boundaries(1);
        let reads = harness.chain.reads();
        let settle_calls = harness.chain.settle_calls();
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        let RouteBudget::Exhausted(reason) = deal.admit(Some(1)).await else {
            panic!("a terminal subscription may not admit a request");
        };
        assert!(reason.contains("terminal"), "{reason}");
        assert_eq!(deal.remaining_tokens(), 0);
        assert_eq!(
            harness.chain.bookings(),
            0,
            "a terminal route must never book a boundary it can no longer claim"
        );
        assert_eq!(
            (harness.chain.reads(), harness.chain.settle_calls()),
            (reads, settle_calls),
            "a latched terminal route must not keep polling the chain"
        );
        harness.shutdown().await;
    }

    /// Adversarial admission: the simultaneous requested sum far exceeds the exact remaining quota, on
    /// BOTH consumer paths. Reservation is what must hold the line - delivered may never pass the
    /// authoritative remainder, whoever asks first.
    #[tokio::test]
    async fn concurrent_requests_cannot_be_handed_the_same_remainder() {
        let harness = weekly_route_harness(WEEK_QUOTA - 12, true).await;
        assert_eq!(harness.remaining().await, 12);

        // Six requests race for twelve billable tokens. Exactly one may hold the whole unknown-input
        // grant; after its eight-token terminal bill, the unused four return.
        let mut answers = Vec::new();
        for path in [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/chat/completions",
            "/v1/messages",
        ] {
            answers.push(harness.ask_path(path, 8));
        }
        let answers = futures::future::join_all(answers).await;
        let served = answers
            .iter()
            .filter(|(status, _)| *status == reqwest::StatusCode::OK)
            .count();
        assert_eq!(served, 1, "one request owns the entire in-flight grant");

        assert!(
            harness.delivered().await <= 12,
            "delivered {} exceeded the authoritative weekly remainder of 12",
            harness.delivered().await
        );
        assert_eq!(harness.delivered().await, 8);
        assert_eq!(harness.remaining().await, 4);
        for (status, body) in &answers {
            assert!(
                *status == reqwest::StatusCode::OK
                    || *status == reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "{status}: {body}"
            );
        }
        harness.shutdown().await;
    }

    /// An over-asking request must not strand the week: what it did not deliver comes back.
    #[tokio::test]
    async fn an_unused_reservation_returns_to_the_week() {
        let harness =
            weekly_route_harness_with_upstream(WEEK_QUOTA - 16, true, SUB_WEEK_LEN.as_secs(), 5)
                .await;
        assert_eq!(harness.remaining().await, 16);

        // Reserve the WHOLE remainder against a model that stops after five tokens: admission takes
        // all sixteen out of the week, the stream uses five, and the other eleven must come back. A
        // test that asked for exactly what it delivers would pass with the refund deleted.
        let (status, body) = harness.ask(16).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        let delivered = harness.delivered().await;
        assert!(
            delivered < 16,
            "this test only proves anything while the stream leaves part of its reservation unused; \
             delivered {delivered} of 16"
        );
        assert_eq!(
            harness.remaining().await,
            16 - delivered,
            "exactly the undelivered part of the reservation must come back to the week"
        );
        harness.shutdown().await;
    }

    /// A seller who does not claim what he served must not thereby enlarge the route.

    /// The two counters are not comparable without a baseline: the route's `delivered` starts at zero
    /// while the contract bounds a CUMULATIVE claim. Publishing `delivered + (_claimCap -
    /// tokensPending)` reads the remainder off the seller's claim, so every token he has served but
    /// not claimed is handed back to the route a second time.

    /// The boundary is BOOKED first and the booking is confirmed to have moved `weekIndex` before
    /// anything is compared, so the un-booked-boundary understatement (phase 2) is out of play and
    /// what remains is only the baseline. And the assertion is an EQUALITY against the figure the
    /// contract itself would admit - `ceiling <= cap` would pass just as well on two errors that
    /// happen to cancel.
    #[tokio::test]
    async fn a_lagging_seller_claim_cannot_enlarge_the_route() {
        // Week one, probe claimed. Anchor = PROBE_CLAIM, so the route may deliver quota - probe.
        let harness = weekly_route_harness_expiring_in(PROBE_CLAIM, true, 2).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();
        assert_eq!(harness.remaining().await, quota - probe);

        let (status, body) = harness.ask(8).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(harness.delivered().await, 8);

        // The seller serves those eight tokens and claims NOTHING for them: `tokensPending` stays at
        // the probe. The chain crosses a boundary and the route books it.
        harness.chain.chain_crosses_boundaries(1);
        tokio::time::sleep(Duration::from_millis(2_200)).await;
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");

        // Phase 2 is out of play only once the boundary is actually BOOKED - confirm it moved.
        assert_eq!(harness.chain.bookings(), 1);
        assert_eq!(harness.chain.week_index(), 1);

        // The contract re-based the week on the cumulative claim, which the lagging seller left at
        // the probe: `_claimCap = weekBaseTokens + tokensPerWeek = probe + quota`. Against the anchor
        // the route measures from, that is exactly one quota of local capacity - no more, whatever
        // the seller has or has not claimed.
        let (state, subscription) = harness.chain.books();
        assert_eq!(subscription.week_base_tokens, PROBE_CLAIM);
        assert_eq!(state.tokens_pending, PROBE_CLAIM);
        let cap = dexdo_core::subscription_claim_cap_at(&state, &subscription).expect("claim cap");
        assert_eq!(cap, PROBE_CLAIM + WEEK_QUOTA);
        let authorized = u64::try_from(cap - PROBE_CLAIM).unwrap();
        assert_eq!(authorized, quota);

        // EQUALITY, both sides pinned: what the route may still hand out is the contract's own
        // ceiling minus everything this route has already reserved against it.
        assert_eq!(
            harness.remaining().await,
            authorized - 9,
            "the ceiling must be the contract's cap measured from the route's own anchor"
        );
        harness.shutdown().await;
    }

    /// The other direction, on its own: a boundary the chain has
    /// crossed and NOBODY has booked, with the seller's claim exactly level with delivery. Here the
    /// recorded books understate the contract - phase 2 - and the route must still refuse until it
    /// has booked, rather than reasoning its way to the larger figure.
    #[tokio::test]
    async fn an_unbooked_boundary_understates_the_ceiling_and_authorizes_nothing() {
        // The recorded week is exactly spent: cap = 0 + quota, pending = quota.
        let harness = weekly_route_harness_expiring_in(WEEK_QUOTA, true, 2).await;
        assert_eq!(harness.remaining().await, 0);

        // The chain crosses a boundary. Nobody books it, and the mock refuses to book one that is
        // not due, so the recorded books stay where they are.
        tokio::time::sleep(Duration::from_millis(2_200)).await;
        let (state, subscription) = harness.chain.books();
        assert_eq!(
            subscription.week_index, 0,
            "the getter lags until it is booked"
        );
        let recorded = dexdo_core::subscription_claim_cap_at(&state, &subscription).expect("cap");
        assert_eq!(recorded, WEEK_QUOTA);

        // What the contract would admit once the crossed boundary is booked is strictly more - the
        // understatement, with no delivery lag anywhere in it.
        let booked = dexdo_core::DealSubscription {
            week_index: 1,
            week_base_tokens: state.tokens_pending,
            ..subscription
        };
        let after_booking =
            dexdo_core::subscription_claim_cap_at(&state, &booked).expect("booked cap");
        assert_eq!(after_booking, WEEK_QUOTA + WEEK_QUOTA);
        assert!(recorded < after_booking);

        // The route serves NEITHER figure: it has not booked, so it has no authorization at all.
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(harness.remaining().await, 0);
        harness.shutdown().await;
    }

    /// A reconciliation that cannot finish must publish NOTHING.

    /// The booking landed and the read that followed still shows the old week - a lagging read, which
    /// on a real node is ordinary. Republishing the week's expiry here would mark the route fresh
    /// while its ceiling still belonged to the week that just ended, and the next request would skip
    /// reconciliation entirely and serve it.
    #[tokio::test]
    async fn a_booking_the_read_does_not_show_publishes_nothing() {
        // A POSITIVE remainder on record - the stale figure that must not be served.
        let harness = weekly_route_harness_expiring_in(WEEK_QUOTA - 16, true, 2).await;
        assert_eq!(harness.remaining().await, 16);

        harness.chain.chain_crosses_boundaries(1);
        harness.chain.serve_one_stale_read();
        tokio::time::sleep(Duration::from_millis(2_200)).await;

        // The booking lands, the read lags: refuse, and leave the route exactly as expired as it was.
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.contains("read does not show yet"), "{body}");
        assert_eq!(harness.chain.bookings(), 1);
        assert_eq!(
            harness.remaining().await,
            16,
            "the stale ceiling is untouched - it is simply no longer reachable without reconciling"
        );

        // The next request must reconcile AGAIN rather than serve the stale positive remainder.
        let reads = harness.chain.reads();
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(
            harness.chain.reads() > reads,
            "a refused reconciliation must not leave the route looking fresh"
        );
        assert_eq!(harness.chain.week_index(), 1);
        harness.shutdown().await;
    }

    /// A week that has ended on the wall clock with nothing booked authorizes nothing, however much
    /// it has left on record.
    #[tokio::test]
    async fn an_expired_week_with_no_booking_refuses_its_positive_remainder() {
        // Sixteen tokens left, and the week runs out two seconds from now.
        let harness = weekly_route_harness_expiring_in(WEEK_QUOTA - 16, true, 2).await;
        assert_eq!(harness.remaining().await, 16);

        // The wall clock passes the boundary but the CHAIN has not crossed one, so the contract
        // refuses to book. The remainder on record belongs to a week the clock says is over.
        tokio::time::sleep(Duration::from_millis(2_200)).await;
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.contains("is not an authorization"), "{body}");
        assert_eq!(harness.chain.bookings(), 0);

        // And it stays refused: nothing was published, so every later request goes back to the chain
        // instead of finding a fresh-looking expiry over the stale ceiling.
        let reads = harness.chain.reads();
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            harness.chain.reads() > reads,
            "an unbooked expired week must be re-asked, never assumed"
        );
        harness.shutdown().await;
    }

    /// A seller decides how many tokens a chunk holds, and the grant must hold anyway.

    /// Every chunk here carries four token ids. With three tokens of quota left, the first chunk
    /// already overshoots it - so it must never be rendered at all, on either protocol and whether
    /// the answer is streamed or aggregated. Accounting after rendering would hand the consumer four
    /// tokens against a reservation of three, and the excess would never appear in the next ceiling.
    #[tokio::test]
    async fn a_fat_chunk_cannot_deliver_past_the_grant() {
        for path in ["/v1/chat/completions", "/v1/messages"] {
            for stream in [true, false] {
                let harness = weekly_route_harness(WEEK_QUOTA - 3, true).await;
                assert_eq!(harness.remaining().await, 3);

                let (status, body) = harness
                    .ask_full(path, 3, "DEXDO_FIXTURE_FATCHUNK weekly quota", stream)
                    .await;
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{path} stream={stream}: {body}"
                );
                let delivered = harness.delivered().await;
                assert!(
                    delivered <= 3,
                    "{path} stream={stream}: delivered {delivered} past a grant of 3"
                );
                // The chunk is four tokens against three of grant, so nothing at all is exposed and
                // the whole reservation returns to the week.
                assert_eq!(delivered, 0, "{path} stream={stream}: {body}");
                assert_eq!(harness.remaining().await, 3, "{path} stream={stream}");
                harness.shutdown().await;
            }
        }
    }

    /// Two fat chunks fit the output cap and the third does not. Without a terminal usage record the
    /// visible prefix remains unbilled and the whole billing reservation returns.
    #[tokio::test]
    async fn fat_chunks_stop_exactly_at_the_grant() {
        for path in ["/v1/chat/completions", "/v1/messages"] {
            for stream in [true, false] {
                let harness = weekly_route_harness(WEEK_QUOTA - 9, true).await;
                assert_eq!(harness.remaining().await, 9);

                let (status, body) = harness
                    .ask_full(path, 9, "DEXDO_FIXTURE_FATCHUNK weekly quota", stream)
                    .await;
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{path} stream={stream}: {body}"
                );
                assert_eq!(harness.delivered().await, 0, "{path} stream={stream}");
                assert_eq!(harness.remaining().await, 9, "{path} stream={stream}");
                harness.shutdown().await;
            }
        }
    }

    /// The output cap must reach the WIRE, and hold even when the seller ignores it.

    /// Admission holds the two-token billing remainder for a request that asked for eight output
    /// tokens. Two things must then be
    /// true, on both consumer protocols and whether the answer is streamed or aggregated:

    /// 1. the seller is TOLD two - the outbound output-only `CanonRequest.params.max_tokens` is
    /// clamped to available capacity, not left at the caller's larger figure;
    /// 2. and if the seller ignores it anyway, the buyer still refuses. This one is deliberately
    /// noncompliant: it answers with a one-token chunk and then a two-token chunk, straddling the
    /// remaining allowance. The second chunk is never rendered, and without an accepted terminal
    /// usage record the complete billing reservation returns to the week.
    #[tokio::test]
    async fn the_grant_reaches_the_wire_and_holds_against_a_noncompliant_seller() {
        for path in ["/v1/chat/completions", "/v1/messages"] {
            for stream in [true, false] {
                let harness = weekly_route_harness(WEEK_QUOTA - 2, true).await;
                assert_eq!(harness.remaining().await, 2);

                let (status, body) = harness
                    .ask_full(
                        path,
                        8,
                        "DEXDO_FIXTURE_STRADDLE DEXDO_FIXTURE_ECHOLIMIT weekly quota",
                        stream,
                    )
                    .await;
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{path} stream={stream}: {body}"
                );

                // 1. What the SELLER was told, read straight off the wire: the seller echoes the
                // output limit it received, and it is the available two - not the caller's eight.
                assert!(
                    body.contains("limit=2"),
                    "{path} stream={stream}: the outbound max_tokens must be the grant, not the \
                     caller's limit: {body}"
                );

                // 2. One output token was visible, but the stream ended before an accepted terminal
                // usage record, so atomic accounting charges neither the prefix nor the overrun.
                assert_eq!(
                    harness.delivered().await,
                    0,
                    "{path} stream={stream}: a straddling stream without terminal usage must not bill: \
                     {body}"
                );
                assert_eq!(
                    harness.remaining().await,
                    2,
                    "{path} stream={stream}: the whole billing reservation returns"
                );
                harness.shutdown().await;
            }
        }
    }

    /// An answer cut by the grant is not a clean stop, and it says so on the wire.

    /// A stream can expose a prefix and then cross the output cap before reaching terminal usage.
    /// Such an incomplete response cannot move money, but the local protocol must still report the
    /// truncation it can see: `length` previously required `received == 0`, so a stream that stopped
    /// at 1,972,000 of a 2,000,000 output cap rendered `stop` and was byte-identical to a finished
    /// answer.

    /// This drives the real path with the real noncompliant-seller fixture rather than fabricating
    /// the end state: the seller is told two tokens and answers 1 + 2, so the second chunk cannot
    /// fit and the render genuinely stops at one of a grant of two - short of the grant, with a
    /// token already delivered, which is exactly the shape that used to be indistinguishable.
    #[tokio::test]
    async fn a_stream_cut_by_the_grant_is_not_reported_as_a_clean_stop() {
        for (path, cut, clean) in [
            ("/v1/chat/completions", "length", "stop"),
            ("/v1/messages", "max_tokens", "end_turn"),
        ] {
            for stream in [true, false] {
                let harness = weekly_route_harness(WEEK_QUOTA - 2, true).await;
                assert_eq!(harness.remaining().await, 2);

                let (status, body) = harness
                    .ask_full(
                        path,
                        8,
                        "DEXDO_FIXTURE_STRADDLE DEXDO_FIXTURE_ECHOLIMIT weekly quota",
                        stream,
                    )
                    .await;
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{path} stream={stream}: {body}"
                );
                assert_eq!(harness.delivered().await, 0, "{path} stream={stream}");

                let reason = terminal_reason(&body);
                assert_ne!(
                    reason.as_deref(),
                    Some(clean),
                    "{path} stream={stream}: a cut answer reported as a clean finish is \
                     indistinguishable from a complete one: {body}"
                );
                assert_eq!(
                    reason.as_deref(),
                    Some(cut),
                    "{path} stream={stream}: the grant ended this answer, which is what `{cut}` \
                     means: {body}"
                );

                // And the counts that make the loss attributable afterwards.
                let delivery = harness.last_delivery().await;
                assert_eq!(delivery.billing_grant_tokens, 2, "{path} stream={stream}");
                assert_eq!(delivery.visible_output_tokens, 1, "{path} stream={stream}");
                assert_eq!(delivery.billable_tokens, None, "{path} stream={stream}");
                assert_eq!(delivery.finish_reason, cut, "{path} stream={stream}");
                assert!(delivery.truncated_by_output_limit, "{path} stream={stream}");
                assert!(
                    delivery.ended_before_output_limit,
                    "{path} stream={stream}: one token of a grant of two leaves the grant unspent"
                );
                assert_eq!(delivery.streamed, stream, "{path} stream={stream}");
                // The quantity a claim may be reconciled against: the deal's cumulative ACCOUNTED
                // delivery, not a count of the frames the renderer emitted.
                assert_eq!(
                    delivery.route_billable_tokens,
                    Some(0),
                    "{path} stream={stream}: incomplete output cannot move the monetary counter"
                );
                harness.shutdown().await;
            }
        }
    }

    /// A complete answer that consumes the paid billing grant tells the buyer that no paid volume
    /// remains, while preserving the full answer and its accepted terminal usage.

    /// This is a deliberate compatibility boundary. Reaching `max_tokens` exactly is not proof of
    /// truncation: the buyer keeps consuming through terminal usage and clean EOF, and reports
    /// truncation only when a later content chunk does not fit. Independently, an input+output
    /// terminal total equal to the billing grant means the paid volume is exhausted.
    #[tokio::test]
    async fn a_complete_render_that_spends_the_whole_grant_reports_capacity_exhaustion() {
        for (path, terminal) in [
            ("/v1/chat/completions", "capacity"),
            ("/v1/messages", "max_tokens"),
        ] {
            for stream in [true, false] {
                let harness = weekly_route_harness(WEEK_QUOTA - 6, true).await;
                assert_eq!(harness.remaining().await, 6);

                let (status, body) = harness.ask_full(path, 4, "weekly quota", stream).await;
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{path} stream={stream}: {body}"
                );
                assert_eq!(harness.delivered().await, 6, "{path} stream={stream}");
                assert_eq!(
                    terminal_reason(&body).as_deref(),
                    Some(terminal),
                    "{path} stream={stream}: the full answer is present, but its accepted billing \
                     usage consumed the paid grant: {body}"
                );

                let delivery = harness.last_delivery().await;
                assert_eq!(delivery.billing_grant_tokens, 6, "{path} stream={stream}");
                assert_eq!(delivery.output_limit_tokens, 4, "{path} stream={stream}");
                assert_eq!(delivery.visible_output_tokens, 4, "{path} stream={stream}");
                assert_eq!(delivery.input_tokens, Some(2), "{path} stream={stream}");
                assert_eq!(delivery.output_tokens, Some(4), "{path} stream={stream}");
                assert_eq!(delivery.billable_tokens, Some(6), "{path} stream={stream}");
                assert!(
                    !delivery.truncated_by_output_limit,
                    "{path} stream={stream}: exact cap alone is not truncation"
                );
                assert!(
                    !delivery.ended_before_output_limit,
                    "{path} stream={stream}: the grant was spent to the last token"
                );
                harness.shutdown().await;
            }
        }
    }

    /// Accounted tokens and rendered frames are DIFFERENT quantities, and only the first one is
    /// money.

    /// A seller decides how many tokens a chunk holds. Four token ids in one chunk is four tokens
    /// charged and one SSE frame emitted, so counting frames off the response body and calling the
    /// result "delivered" understates what was paid for - by a factor the seller chooses. The gap
    /// is always in the same direction, and nothing in the renderer enforces the equality.

    /// The endpoint therefore publishes the accounted figure. Anything reconciling a seller's claim
    /// against delivery reads that, never a count of frames.
    #[tokio::test]
    async fn accounted_delivery_is_not_the_number_of_rendered_frames() {
        let harness = weekly_route_harness(WEEK_QUOTA - 9, true).await;
        assert_eq!(harness.remaining().await, 9);

        let (status, body) = harness
            .ask_full(
                "/v1/chat/completions",
                8,
                "DEXDO_FIXTURE_FATCHUNK_COMPLETE",
                true,
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");

        // What a frame-counter off the body sees.
        let frames = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .filter(|data| *data != "[DONE]")
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .filter(|chunk| {
                chunk["choices"][0]["delta"]["content"]
                    .as_str()
                    .is_some_and(|content| !content.is_empty())
            })
            .count() as u64;

        // What the money path charged: one input token plus eight output tokens.
        let billable = harness.delivered().await;
        let delivery = harness.last_delivery().await;

        assert_eq!(delivery.visible_output_tokens, 8, "{body}");
        assert_eq!(delivery.input_tokens, Some(1));
        assert_eq!(delivery.output_tokens, Some(8));
        assert_eq!(delivery.billable_tokens, Some(9));
        assert_eq!(
            delivery.route_billable_tokens,
            Some(billable),
            "the deal's cumulative billable usage reaches the event"
        );
        assert!(
            frames < delivery.visible_output_tokens,
            "a four-token chunk is one frame: frames={frames}, output={}: {body}",
            delivery.visible_output_tokens
        );
        assert_eq!(delivery.visible_output_tokens, frames * 4);
        assert_eq!(billable, 9);
        harness.shutdown().await;
    }

    /// An answer the seller finished on its own is still a clean stop, and the record says how much
    /// of the grant went unused.

    /// The counterpart to the two above: making a cut answer visible must not relabel every short
    /// answer as truncated. A model that stops early is the normal case, it keeps `stop`/`end_turn`,
    /// and `ended_before_output_limit` records that the provider stopped below the output cap.
    #[tokio::test]
    async fn an_answer_the_seller_finished_stays_a_clean_stop() {
        for (path, clean) in [
            ("/v1/chat/completions", "stop"),
            ("/v1/messages", "end_turn"),
        ] {
            for stream in [true, false] {
                // The seller emits three tokens against a grant of sixteen, so the stream ends well
                // before the cap without anything having gone wrong.
                let harness = weekly_route_harness_with_upstream(
                    WEEK_QUOTA - 16,
                    true,
                    SUB_WEEK_LEN.as_secs(),
                    3,
                )
                .await;
                assert_eq!(harness.remaining().await, 16);

                let (status, body) = harness.ask_full(path, 16, "weekly quota", stream).await;
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{path} stream={stream}: {body}"
                );
                assert_eq!(harness.delivered().await, 5, "{path} stream={stream}");
                assert_eq!(
                    terminal_reason(&body).as_deref(),
                    Some(clean),
                    "{path} stream={stream}: a model that finished early has not been truncated: \
                     {body}"
                );

                let delivery = harness.last_delivery().await;
                assert_eq!(delivery.billing_grant_tokens, 16, "{path} stream={stream}");
                assert_eq!(delivery.visible_output_tokens, 3, "{path} stream={stream}");
                assert_eq!(delivery.input_tokens, Some(2), "{path} stream={stream}");
                assert_eq!(delivery.output_tokens, Some(3), "{path} stream={stream}");
                assert_eq!(delivery.billable_tokens, Some(5), "{path} stream={stream}");
                assert!(
                    !delivery.truncated_by_output_limit,
                    "{path} stream={stream}"
                );
                assert!(
                    delivery.ended_before_output_limit,
                    "{path} stream={stream}: thirteen tokens of the grant were never spent, and \
                     that is the figure a billed-but-not-received gap shows up in"
                );
                harness.shutdown().await;
            }
        }
    }

    /// A route built before the trial tick was accepted must not keep a tick of authorization the
    /// term never sold it.

    /// `open()` leaves the claim stages at zero and `acceptProbe` seeds all three with one
    /// `TICK_SIZE`, so a route anchored pre-probe measures its local zero from a cumulative claim the
    /// contract is about to move underneath it. The anchor rebases once on that transition; without
    /// it the published ceiling is a whole tick above what the contract will admit.
    #[tokio::test]
    async fn an_anchor_taken_before_the_probe_rebases_on_acceptance() {
        // D = 8 tokens delivered BEFORE acceptance. D = 0 would hide the whole defect: the two
        // errors it causes are `TICK_SIZE - D` in this week and `D` in the next one.
        let harness = weekly_route_harness_before_probe(2).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();

        // Anchored at zero, pre-probe: the buyer independently exposes only the canonical trial
        // tick even though the recorded weekly ceiling is larger.
        assert_eq!(harness.remaining().await, probe);
        let (status, body) = harness.ask(8).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(harness.delivered().await, 8);

        // The seller accepts the trial tick. `acceptProbe` sets the claim stages to a FLAT
        // TICK_SIZE, so it absorbs those eight tokens rather than adding to them.
        harness.chain.accepts_probe();

        // A boundary is crossed and booked, so the route recomputes from authoritative state.
        harness.chain.chain_crosses_boundaries(1);
        tokio::time::sleep(Duration::from_millis(2_200)).await;
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        let RouteBudget::Admitted(reservation) = deal.admit(Some(1)).await else {
            panic!("the booked week must be servable");
        };
        assert_eq!(harness.chain.bookings(), 1);

        // Week two's cap is one quota measured from the cumulative claim the booking re-based on,
        // which is the probe's tick. The new week must be a WHOLE quota: the eight tokens delivered
        // before acceptance were paid for by the probe's seed and must not be charged again here.
        let (state, subscription) = harness.chain.books();
        let cap = dexdo_core::subscription_claim_cap_at(&state, &subscription).expect("cap");
        assert_eq!(cap, PROBE_CLAIM + WEEK_QUOTA);
        drop(reservation);
        assert_eq!(
            deal.remaining_tokens(),
            quota,
            "the new week is a whole quota - not a quota less the D delivered before the probe"
        );
        assert_eq!(u64::try_from(cap).unwrap() - probe, quota);
        harness.shutdown().await;
    }

    /// The same defect in the OTHER direction, before any boundary: the week the route is already in
    /// . `acceptProbe` does not move `weekIndex`, so a rebase triggered by the week
    /// changing never runs here at all, and the route keeps offering `TICK_SIZE - D` more than the
    /// contract will admit for the rest of the current week.
    #[tokio::test]
    async fn acceptance_corrects_the_current_week_without_a_boundary() {
        // A full week ahead, so nothing here depends on the expiry trigger - only on acceptance.
        let harness = weekly_route_harness_before_probe(SUB_WEEK_LEN.as_secs()).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();

        // D = 8 again: delivered before the trial tick was accepted.
        assert_eq!(harness.remaining().await, probe);
        let (status, body) = harness.ask(8).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert_eq!(harness.delivered().await, 8);

        harness.chain.accepts_probe();
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        let RouteBudget::Admitted(reservation) = deal.admit(Some(1)).await else {
            panic!("the current week is still servable after acceptance");
        };
        drop(reservation);

        // The cumulative claim is now one tick and the week's cap is one quota, so what the contract
        // will still admit is `quota - TICK_SIZE` - whatever this route delivered before acceptance,
        // because the seed absorbed it. No boundary was crossed and none was booked.
        assert_eq!(harness.chain.bookings(), 0);
        assert_eq!(harness.chain.week_index(), 0);
        assert_eq!(
            deal.remaining_tokens(),
            quota - probe,
            "after acceptance the current week may not still offer the pre-probe remainder"
        );
        harness.shutdown().await;
    }

    /// A concurrent admission waits until acceptance rebase and ceiling are both published.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acceptance_reconcile_keeps_stale_ceiling_closed() {
        let harness = weekly_route_harness_before_probe(SUB_WEEK_LEN.as_secs()).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();

        let (status, body) = harness.ask(8).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        harness.chain.accepts_probe();

        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        let weekly = deal.weekly.as_ref().expect("subscription budget").clone();
        let (state, _) = harness.chain.books();
        let contradictory = u64::try_from(dexdo_core::TICK_SIZE).unwrap() + 1;
        {
            let mut anchor = weekly.claim_anchor.lock().unwrap();
            assert!(
                weekly
                    .rebase_anchor_on_probe(&state, contradictory, &mut anchor)
                    .is_err(),
                "delivery above the flat probe claim must fail closed"
            );
        }
        assert!(weekly.anchored_before_probe());
        let rebase_barrier = Arc::new(std::sync::Barrier::new(2));
        *weekly.rebase_barrier.lock().unwrap() = Some(rebase_barrier.clone());
        let first_deal = deal.clone();
        let first = tokio::spawn(async move { first_deal.admit(Some(1)).await });
        rebase_barrier.wait();
        assert!(
            weekly.anchored_before_probe(),
            "the fast-path gate stays closed until the corrected ceiling is published"
        );

        let second_deal = deal.clone();
        let second = tokio::spawn(async move { second_deal.admit(Some(1)).await });
        rebase_barrier.wait();
        let first = first.await.expect("first admission task");
        let second = second.await.expect("second admission task");
        let (admitted, exhausted) = match (first, second) {
            (RouteBudget::Admitted(reservation), RouteBudget::Exhausted(reason))
            | (RouteBudget::Exhausted(reason), RouteBudget::Admitted(reservation)) => {
                (reservation, reason)
            }
            _ => panic!("exactly one request must own the corrected week's billing remainder"),
        };
        assert!(exhausted.contains(ORDINARY_BUDGET_EXHAUSTED));
        drop(admitted);
        assert_eq!(
            deal.remaining_tokens(),
            quota - probe,
            "both admissions were made only after the accepted probe's tick was published"
        );
        harness.shutdown().await;
    }

    /// A pre-acceptance request holds the whole trial-tick billing remainder but keeps the caller's
    /// output-only cap independent on the wire.
    #[tokio::test]
    async fn pre_probe_admission_and_wire_shape_are_capped_to_the_trial_tick() {
        let harness = weekly_route_harness_before_probe(SUB_WEEK_LEN.as_secs()).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();
        let deal = harness
            .deals
            .current()
            .await
            .expect("the connected harness route is published");

        assert_eq!(
            deal.route.max_tokens, quota,
            "the recorded route ceiling is weekly"
        );
        assert_eq!(
            deal.remaining_tokens(),
            probe,
            "pre-probe admission is one tick"
        );
        let (status, body) = harness
            .ask_full("/v1/chat/completions", 2, "DEXDO_FIXTURE_ECHOLIMIT", false)
            .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(
            body.contains("limit=2"),
            "the seller must observe the independent CanonRequest.params.max_tokens=2: \
             {body}"
        );
        let delivery = harness.last_delivery().await;
        assert_eq!(delivery.billing_grant_tokens, probe);
        assert_eq!(delivery.output_limit_tokens, 2);
        assert_eq!(delivery.input_tokens, Some(1));
        assert_eq!(delivery.output_tokens, Some(2));
        assert_eq!(delivery.billable_tokens, Some(3));
        assert_eq!(deal.billable_tokens(), 3);
        assert_eq!(deal.remaining_tokens(), probe - 3);
        harness.shutdown().await;
    }

    /// Actual delivery from a reservation admitted before `acceptProbe` is linearized against the
    /// one-time anchor/ceiling cutover by the same standard-library mutex, without timing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_flight_trial_delivery_is_serialized_with_acceptance_cutover() {
        let harness = weekly_route_harness_before_probe(SUB_WEEK_LEN.as_secs()).await;
        let quota = u64::try_from(WEEK_QUOTA).unwrap();
        let probe = u64::try_from(PROBE_CLAIM).unwrap();
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        let weekly = deal.weekly.as_ref().expect("subscription budget").clone();

        let mut in_flight = deal.begin_request(unix_now_secs());
        let RouteBudget::Admitted(reservation) = deal.admit(Some(2)).await else {
            panic!("the pre-probe trial reservation is available");
        };
        in_flight.hold(reservation);
        harness.chain.accepts_probe();

        // Stop the cutover after it owns `claim_anchor` but before it samples delivered tokens.
        let cutover_barrier = Arc::new(std::sync::Barrier::new(2));
        *weekly.rebase_barrier.lock().unwrap() = Some(cutover_barrier.clone());
        let cutover_deal = deal.clone();
        let cutover = tokio::spawn(async move { cutover_deal.admit(Some(1)).await });
        cutover_barrier.wait();

        // Start one real guard charge while the cutover owns that same mutex. With no shared lock it
        // would enter the sampled probe seed; with the lock it is deterministically charged after it.
        let delivery_barrier = Arc::new(std::sync::Barrier::new(2));
        let delivery_started = delivery_barrier.clone();
        let delivery_deal = deal.clone();
        let delivery = tokio::spawn(async move {
            delivery_started.wait();
            in_flight.record_billing(&delivery_deal, 1)?;
            Ok::<_, String>(in_flight)
        });
        delivery_barrier.wait();
        cutover_barrier.wait();

        let RouteBudget::Exhausted(reason) = cutover.await.expect("acceptance cutover task") else {
            panic!(
                "the in-flight request must retain exclusive ownership of the billing remainder"
            );
        };
        assert!(reason.contains(ORDINARY_BUDGET_EXHAUSTED));
        let in_flight = delivery
            .await
            .expect("delivery task")
            .expect("in-flight delivery accounting");
        assert_eq!(deal.billable_tokens(), 1);
        drop(in_flight);
        assert_eq!(
            deal.remaining_tokens(),
            quota - probe - 1,
            "the post-cutover chunk is charged once outside the accepted probe seed"
        );
        harness.shutdown().await;
    }

    /// A reservation made before acceptance cannot outlive the ceiling that acceptance publishes.
    /// This is the minimum legal subscription: Q=T, with the whole trial tick reserved and D=0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn held_pre_probe_reservation_cannot_deliver_after_zero_ceiling_cutover() {
        let harness = weekly_route_harness_before_probe(SUB_WEEK_LEN.as_secs()).await;
        let probe = u64::try_from(PROBE_CLAIM).unwrap();
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        let weekly = deal.weekly.as_ref().expect("subscription budget").clone();

        // Narrow the existing canonical fixture to its minimum legal shape before any admission:
        // one tick per week, four ticks funded. The accepted probe consumes week one's whole quota.
        {
            let mut snapshot = harness.chain.snapshot.lock().unwrap();
            snapshot.subscription.tokens_per_week = PROBE_CLAIM;
            snapshot.subscription.funded_tokens =
                PROBE_CLAIM * u128::from(dexdo_core::SUBSCRIPTION_WEEKS);
        }
        deal.token_ceiling.store(probe, Ordering::SeqCst);
        assert_eq!(deal.remaining_tokens(), probe, "Q=T before acceptance");

        let mut old_request = deal.begin_request(unix_now_secs());
        let RouteBudget::Admitted(reservation) = deal.admit(None).await else {
            panic!("the pre-probe request reserves R=T");
        };
        assert_eq!(reservation.remaining(), probe, "R=T");
        old_request.hold(reservation);
        assert_eq!(deal.billable_tokens(), 0, "D=0");
        harness.chain.accepts_probe();

        // Hold the real acceptance cutover after it owns `claim_anchor`. The old request begins its
        // late charge while that lock is held, then must validate the newly published zero ceiling.
        let cutover_barrier = Arc::new(std::sync::Barrier::new(2));
        *weekly.rebase_barrier.lock().unwrap() = Some(cutover_barrier.clone());
        let cutover_deal = deal.clone();
        let cutover = tokio::spawn(async move { cutover_deal.admit(Some(1)).await });
        cutover_barrier.wait();

        let delivery_barrier = Arc::new(std::sync::Barrier::new(2));
        let delivery_started = delivery_barrier.clone();
        let delivery_deal = deal.clone();
        let delivery = tokio::spawn(async move {
            delivery_started.wait();
            let result = old_request.record_billing(&delivery_deal, 1);
            (result, old_request)
        });
        delivery_barrier.wait();
        cutover_barrier.wait();

        let RouteBudget::Exhausted(reason) = cutover.await.expect("acceptance cutover task") else {
            panic!("the accepted probe consumes all of Q=T");
        };
        assert!(reason.contains("drawn down"), "{reason}");

        let (late_delivery, old_request) = delivery.await.expect("late delivery task");
        let error = late_delivery.expect_err("a late token is above the rebased zero ceiling");
        assert!(error.contains("published route ceiling 0"), "{error}");
        assert_eq!(
            old_request.remaining_grant(),
            probe,
            "rejection leaves the old reservation entirely unused"
        );
        assert_eq!(
            deal.billable_tokens(),
            0,
            "rejection leaves cumulative delivery unchanged, so no chunk can be exposed"
        );
        assert_eq!(
            deal.reserved_billable_tokens.load(Ordering::SeqCst),
            probe,
            "the held reservation is unchanged until its guard drops"
        );
        drop(old_request);
        assert_eq!(deal.reserved_billable_tokens.load(Ordering::SeqCst), 0);
        assert_eq!(
            deal.remaining_tokens(),
            0,
            "week one remains fully consumed"
        );
        harness.shutdown().await;
    }

    /// What a deal that has not passed its one-per-deal identity verification still owes: the B8
    /// fingerprint probe and then the B7-full reference spot-check, each at the canonical probe
    /// budget. This is the FLOOR of every admission on such a deal, so a fixture's remainder
    /// is written against it rather than as a bare number that would silently stop meaning anything
    /// if the canonical budget moved.
    const VERIFICATION_DEBT: u64 = 2 * CONTENT_PROBE_MAX_TOKENS;

    /// Billable total emitted by the mock for both verification calls used below: two input words
    /// for `identity probe`, nine for [`DEFAULT_SPOTCHECK_PROBE`], and a full output budget for each.
    const FULL_VERIFICATION_BILLABLE: u64 = VERIFICATION_DEBT + 11;

    /// A models config whose ONLY verification layer is B8, and whose fingerprint the mock seller
    /// satisfies: the mock echoes the prompt after a `mock-reply: ` marker, so a one-token answer
    /// already carries it. B7 needs `DEXDO_FIXTURE_ABSENT_KEY` in the environment and degrades to a
    /// pass without spending when it is missing, which keeps these tests to one probe exactly. The
    /// fixture opts into `SEED` explicitly because the production default is conservatively `NONE`.
    fn probe_models(probe_prompt: &str, base_url: &str, api_key_env: &str) -> Arc<ModelsConfig> {
        Arc::new(
            ModelsConfig::from_json(
                &serde_json::json!({
                    "models": { "dexdo-mock": {
                        "frame_model": "dexdo-mock",
                        "base_url": base_url,
                        "served_model": "dexdo-mock",
                        "api_key_env": api_key_env,
                        "tokenizer_family": "mock",
                        "price_per_tick": 1000,
                        "capabilities": { "sample_algorithm": "SEED" },
                        "fingerprints": [ {
                            "probe_prompt": probe_prompt,
                            "expected_contains": "mock-reply"
                        } ]
                    } }
                })
                .to_string(),
            )
            .expect("canonical probe models config"),
        )
    }

    /// Provider-native input on B8 can leave B7 less than its canonical output cap. The exact pair
    /// passed to the second wire call must remain `(min(64, grant), grant)`, rather than asking the
    /// gateway for an output cap larger than the request can pay in total.
    #[test]
    fn second_probe_limits_clamp_output_to_the_remaining_billing_grant() {
        const FREE: u64 = VERIFICATION_DEBT + 1;
        const B8_INPUT_TOKENS: u64 = 70;
        const B8_BILLABLE: u64 = B8_INPUT_TOKENS + 1;
        const REMAINING_AFTER_B8: u64 = FREE - B8_BILLABLE;
        const B7_BILLABLE: u64 = 9 + 1;
        const _: () = assert!(REMAINING_AFTER_B8 < CONTENT_PROBE_MAX_TOKENS);
        const _: () = assert!(B7_BILLABLE <= REMAINING_AFTER_B8);

        assert_eq!(
            verification_stream_limits(REMAINING_AFTER_B8),
            (REMAINING_AFTER_B8, REMAINING_AFTER_B8),
            "the second output-only cap is clamped while the independent total grant is preserved"
        );
    }

    /// A transport-only endpoint for provider-adapter acceptance rows. It reads one complete HTTP
    /// request and returns the same fixed SSE bytes on every connection; provider semantics remain
    /// entirely in the real OpenAI/Anthropic adapters under test.
    async fn fixed_provider_bytes(body: String) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixed provider listener");
        let addr = listener.local_addr().expect("fixed provider address");
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut header_end = None;
                while header_end.is_none() {
                    let mut chunk = [0_u8; 4096];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    header_end = request.windows(4).position(|window| window == b"\r\n\r\n");
                }
                let Some(header_end) = header_end else {
                    continue;
                };
                let content_length = String::from_utf8_lossy(&request[..header_end])
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let request_length = header_end + 4 + content_length;
                while request.len() < request_length {
                    let mut chunk = [0_u8; 4096];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }

                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                if socket.write_all(headers.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(body.as_bytes()).await;
                }
            }
        });
        (addr, task)
    }

    /// The two real adapters' wire encodings for one row-owned visible payload and one
    /// provider-native usage count. The fixture deliberately sends no tokenizer or logprob signal:
    /// only the joined visible text and native usage cross the buyer boundary.
    fn ups_visible_usage_fixtures(visible_payload: &str, native_usage: u64) -> (String, String) {
        let billable_total = native_usage.checked_add(1).unwrap();
        let openai_content = serde_json::json!({
            "choices": [{ "delta": { "content": visible_payload } }]
        });
        let openai_usage = serde_json::json!({
            "choices": [{ "delta": {}, "finish_reason": "stop" }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": native_usage,
                "total_tokens": billable_total,
            },
            "x_groq": { "usage": {
                "prompt_tokens": 1,
                "completion_tokens": native_usage,
                "total_tokens": billable_total,
            } }
        });
        let anthropic_start = serde_json::json!({
            "type": "message_start",
            "message": { "usage": { "input_tokens": 1, "output_tokens": 0 } }
        });
        let anthropic_content = serde_json::json!({
            "type": "content_block_delta",
            "delta": { "type": "text_delta", "text": visible_payload }
        });
        let anthropic_usage = serde_json::json!({
            "type": "message_delta",
            "usage": { "output_tokens": native_usage }
        });

        (
            format!(
                "data: {openai_content}\n\ndata: {openai_usage}\n\ndata: [DONE]\n\n"
            ),
            format!(
                "event: message_start\ndata: {anthropic_start}\n\nevent: content_block_delta\ndata: {anthropic_content}\n\nevent: message_delta\ndata: {anthropic_usage}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            ),
        )
    }

    /// Mechanical OpenAI arm launcher: real adapter + real gate + existing chain counters.
    /// The calling row owns the provider bytes and every expected outcome.
    async fn observe_ups_openai_content_gate_arm(
        body: &str,
        subscription: bool,
        models: Arc<ModelsConfig>,
        api_key_env: &str,
        prompts: &[&str],
    ) -> (Vec<(reqwest::StatusCode, String)>, usize, usize, u64) {
        observe_ups_openai_content_gate_arm_with_max_tokens(
            body,
            subscription,
            models,
            api_key_env,
            prompts,
            1024,
        )
        .await
    }

    async fn observe_ups_openai_content_gate_arm_with_max_tokens(
        body: &str,
        subscription: bool,
        models: Arc<ModelsConfig>,
        api_key_env: &str,
        prompts: &[&str],
        max_tokens: u64,
    ) -> (Vec<(reqwest::StatusCode, String)>, usize, usize, u64) {
        let (addr, task) = fixed_provider_bytes(body.to_string()).await;
        let harness = weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
            claimed: PROBE_CLAIM,
            subscription,
            expires_in: SUB_WEEK_LEN.as_secs(),
            upstream_tokens: UNCONSTRAINED_UPSTREAM,
            content_gate: ContentGate::probe("dexdo-mock".to_string(), models),
            upstream: crate::seller::UpstreamConfig::OpenAi(crate::seller::OpenAiConfig {
                base_url: format!("http://{addr}"),
                model: "dexdo-mock".to_string(),
                frame_model: "dexdo-mock".to_string(),
                claimed_model_override: None,
                api_key_env: api_key_env.to_string(),
                tokenizer_family: "mock".to_string(),
                capabilities: crate::seller::Capabilities {
                    max_output_tokens: Some(1024),
                    ..crate::seller::Capabilities::default()
                },
                identity_aliases: Vec::new(),
            }),
            failure_policy: BuyerApiFailurePolicy {
                verification_bail: VerificationBailAction::Dispute,
                ..BuyerApiFailurePolicy::default()
            },
            lifetime: if subscription {
                SessionLifetimePolicy::Preserve
            } else {
                SessionLifetimePolicy::SettleOnExit
            },
        })
        .await;
        let mut responses = Vec::new();
        for prompt in prompts {
            responses.push(
                harness
                    .ask_full("/v1/chat/completions", max_tokens, prompt, false)
                    .await,
            );
        }
        let disputes = harness.chain.dispute_calls();
        let money = harness.chain.foreign_money_calls();
        let delivered = harness.delivered().await;
        harness.shutdown().await;
        task.abort();
        let _ = task.await;
        (responses, disputes, money, delivered)
    }

    /// SSE text fragments are transport units, not provider-native output tokens. The
    /// provider may split a valid 64-token answer across more than 64 non-empty events; both the
    /// verification probe and the caller's request must accept the answer from terminal usage.
    #[tokio::test]
    async fn fragmented_real_stream_uses_terminal_output_usage_for_the_cap() {
        const API_KEY: &str = "DEXDO_ISSUE1988_OPENAI_KEY";
        std::env::set_var(API_KEY, "test-key");

        let mut body = String::new();
        for index in 0..65 {
            let content = if index == 0 { "mock-reply" } else { "x" };
            let chunk = serde_json::json!({
                "choices": [{ "delta": { "content": content } }]
            });
            body.push_str(&format!("data: {chunk}\n\n"));
        }
        let usage = serde_json::json!({
            "choices": [{ "delta": {}, "finish_reason": "stop" }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": CONTENT_PROBE_MAX_TOKENS,
                "total_tokens": CONTENT_PROBE_MAX_TOKENS + 1,
            },
            "x_groq": { "usage": {
                "prompt_tokens": 1,
                "completion_tokens": CONTENT_PROBE_MAX_TOKENS,
                "total_tokens": CONTENT_PROBE_MAX_TOKENS + 1,
            } }
        });
        body.push_str(&format!("data: {usage}\n\ndata: [DONE]\n\n"));

        let (responses, disputes, money, delivered) =
            observe_ups_openai_content_gate_arm_with_max_tokens(
                &body,
                false,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                API_KEY,
                &["issue 1988"],
                CONTENT_PROBE_MAX_TOKENS,
            )
            .await;
        std::env::remove_var(API_KEY);

        let (status, response_body) = &responses[0];
        assert_eq!(*status, reqwest::StatusCode::OK, "{response_body}");
        assert!(response_body.contains("mock-reply"), "{response_body}");
        assert_eq!(disputes, 0);
        assert_eq!(money, 0);
        assert_eq!(
            delivered,
            2 * (CONTENT_PROBE_MAX_TOKENS + 1),
            "the B8 probe and caller request are billed only from terminal provider usage"
        );
    }

    /// Mechanical Anthropic arm launcher: real adapter + real gate + existing chain counters.
    /// The calling row owns the provider bytes and every expected outcome.
    async fn observe_ups_anthropic_content_gate_arm(
        body: &str,
        subscription: bool,
        models: Arc<ModelsConfig>,
        api_key_env: &str,
        prompts: &[&str],
    ) -> (Vec<(reqwest::StatusCode, String)>, usize, usize, u64) {
        let (addr, task) = fixed_provider_bytes(body.to_string()).await;
        let harness = weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
            claimed: PROBE_CLAIM,
            subscription,
            expires_in: SUB_WEEK_LEN.as_secs(),
            upstream_tokens: UNCONSTRAINED_UPSTREAM,
            content_gate: ContentGate::probe("dexdo-mock".to_string(), models),
            upstream: crate::seller::UpstreamConfig::Anthropic(crate::seller::AnthropicConfig {
                base_url: format!("http://{addr}"),
                model: "dexdo-mock".to_string(),
                frame_model: "dexdo-mock".to_string(),
                api_key_env: api_key_env.to_string(),
                tokenizer_family: "mock".to_string(),
                max_output_tokens: Some(1024),
            }),
            failure_policy: BuyerApiFailurePolicy {
                verification_bail: VerificationBailAction::Dispute,
                ..BuyerApiFailurePolicy::default()
            },
            lifetime: if subscription {
                SessionLifetimePolicy::Preserve
            } else {
                SessionLifetimePolicy::SettleOnExit
            },
        })
        .await;
        let mut responses = Vec::new();
        for prompt in prompts {
            responses.push(
                harness
                    .ask_full("/v1/chat/completions", 1024, prompt, false)
                    .await,
            );
        }
        let disputes = harness.chain.dispute_calls();
        let money = harness.chain.foreign_money_calls();
        let delivered = harness.delivered().await;
        harness.shutdown().await;
        task.abort();
        let _ = task.await;
        (responses, disputes, money, delivered)
    }

    /// E2E-UPS-31/L0: the real content gate must leave an obviously honest native invoice alone,
    /// while the embedded gross-invoice adversary proves the same gate is not merely permissive.
    /// E2E-ROW: E2E-UPS-31/L0
    #[tokio::test]
    #[ignore = "EXPECTED TO FAIL until coarse buyer-visible usage policy exists"]
    async fn e2e_ups_31_honest_visible_volume_proceeds_without_tokenizer() {
        const OPENAI_KEY: &str = "DEXDO_UPS31_OPENAI_KEY";
        const ANTHROPIC_KEY: &str = "DEXDO_UPS31_ANTHROPIC_KEY";
        std::env::set_var(OPENAI_KEY, "test-key");
        std::env::set_var(ANTHROPIC_KEY, "test-key");

        // The row owns exactly 400 visible ASCII characters. The buyer receives that same payload
        // from both adapters, so the native 100/1000 pair compares identical visible service
        // without making SSE framing, tokenizer output or logprobs a billing oracle.
        let visible_payload = format!("mock-reply{}", "x".repeat(390));
        assert!(visible_payload.is_ascii());
        assert_eq!(visible_payload.len(), 400);
        let (openai_honest, anthropic_honest) = ups_visible_usage_fixtures(&visible_payload, 100);
        let (openai_gross, anthropic_gross) = ups_visible_usage_fixtures(&visible_payload, 1000);

        let mut complete = true;
        let mut observations = Vec::new();

        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_openai_content_gate_arm(
                &openai_honest,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                OPENAI_KEY,
                &["UPS31 honest"],
            )
            .await;
            let (status, body) = &responses[0];
            complete &= *status == reqwest::StatusCode::OK
                && disputes == 0
                && money == 0
                && delivered == 100
                && body.contains(visible_payload.as_str());
            observations.push(format!(
                "openai/{subscription}/honest: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
            ));
        }
        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_openai_content_gate_arm(
                &openai_gross,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                OPENAI_KEY,
                &["UPS31 gross"],
            )
            .await;
            let (status, body) = &responses[0];
            complete &= *status != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0
                && !body.contains(visible_payload.as_str());
            observations.push(format!(
                "openai/{subscription}/gross: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
            ));
        }
        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_anthropic_content_gate_arm(
                &anthropic_honest,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                ANTHROPIC_KEY,
                &["UPS31 honest"],
            )
            .await;
            let (status, body) = &responses[0];
            complete &= *status == reqwest::StatusCode::OK
                && disputes == 0
                && money == 0
                && delivered == 100
                && body.contains(visible_payload.as_str());
            observations.push(format!(
                "anthropic/{subscription}/honest: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
            ));
        }
        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_anthropic_content_gate_arm(
                &anthropic_gross,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                ANTHROPIC_KEY,
                &["UPS31 gross"],
            )
            .await;
            let (status, body) = &responses[0];
            complete &= *status != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0
                && !body.contains(visible_payload.as_str());
            observations.push(format!(
                "anthropic/{subscription}/gross: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
            ));
        }
        std::env::remove_var(OPENAI_KEY);
        std::env::remove_var(ANTHROPIC_KEY);
        if !complete {
            eprintln!("{}", observations.join("\n"));
            panic!("E2E-UPS-31 missing capability: coarse buyer-visible usage policy");
        }
    }

    /// E2E-UPS-32/L0: native usage without text, reasoning or another declared output capability
    /// is a bill for no buyer-visible service. It disputes once before any other money path.
    /// E2E-ROW: E2E-UPS-32/L0
    #[tokio::test]
    #[ignore = "EXPECTED TO FAIL until empty-output native usage reaches the content gate"]
    async fn e2e_ups_32_empty_text_positive_invoice_is_rejected() {
        const OPENAI_KEY: &str = "DEXDO_UPS32_OPENAI_KEY";
        const ANTHROPIC_KEY: &str = "DEXDO_UPS32_ANTHROPIC_KEY";
        std::env::set_var(OPENAI_KEY, "test-key");
        std::env::set_var(ANTHROPIC_KEY, "test-key");

        let openai_empty = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":3,\"total_tokens\":4},\"x_groq\":{\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":3,\"total_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        )
        .to_string();
        let anthropic_empty = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
        .to_string();
        let empty_models = Arc::new(
            ModelsConfig::from_json(
                &serde_json::json!({
                    "models": { "dexdo-mock": {
                        "frame_model": "dexdo-mock",
                        "base_url": "https://reference.invalid/v1",
                        "served_model": "dexdo-mock",
                        "api_key_env": "DEXDO_FIXTURE_ABSENT_KEY",
                        "tokenizer_family": "mock",
                        "price_per_tick": 1000,
                        "fingerprints": [ {
                            "probe_prompt": "identity probe",
                            "expected_contains": ""
                        } ]
                    } }
                })
                .to_string(),
            )
            .expect("empty-output probe config"),
        );

        let mut complete = true;
        let mut observations = Vec::new();

        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_openai_content_gate_arm(
                &openai_empty,
                subscription,
                empty_models.clone(),
                OPENAI_KEY,
                &["UPS32 empty", "UPS32 replay"],
            )
            .await;
            let (first, first_body) = &responses[0];
            let (replay, replay_body) = &responses[1];
            complete &= *first != reqwest::StatusCode::OK
                && *replay != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0;
            observations.push(format!(
                "openai/{subscription}/empty: first={first} replay={replay} disputes={disputes} money={money} delivered={delivered} bodies={first_body:?}/{replay_body:?}"
            ));
        }
        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_anthropic_content_gate_arm(
                &anthropic_empty,
                subscription,
                empty_models.clone(),
                ANTHROPIC_KEY,
                &["UPS32 empty", "UPS32 replay"],
            )
            .await;
            let (first, first_body) = &responses[0];
            let (replay, replay_body) = &responses[1];
            complete &= *first != reqwest::StatusCode::OK
                && *replay != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0;
            observations.push(format!(
                "anthropic/{subscription}/empty: first={first} replay={replay} disputes={disputes} money={money} delivered={delivered} bodies={first_body:?}/{replay_body:?}"
            ));
        }

        // Embedded negative: the same real gate must not dispute a plainly nonempty honest response.
        let honest = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"mock-reply\"},\"logprobs\":{\"content\":[{\"token\":\"mock-reply\",\"logprob\":-0.1,\"top_logprobs\":[]}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"x_groq\":{\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}}\n\n",
            "data: [DONE]\n\n"
        );
        let (responses, disputes, money, delivered) = observe_ups_openai_content_gate_arm(
            honest,
            false,
            probe_models(
                "identity probe",
                "https://reference.invalid/v1",
                "DEXDO_FIXTURE_ABSENT_KEY",
            ),
            OPENAI_KEY,
            &["UPS32 honest"],
        )
        .await;
        let (status, body) = &responses[0];
        complete &= *status == reqwest::StatusCode::OK
            && disputes == 0
            && money == 0
            && delivered == 1
            && body.contains("mock-reply");
        observations.push(format!(
            "openai/false/honest-negative: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
        ));
        std::env::remove_var(OPENAI_KEY);
        std::env::remove_var(ANTHROPIC_KEY);
        if !complete {
            eprintln!("{}", observations.join("\n"));
            panic!("E2E-UPS-32 positive empty-output invoice did not dispute once before payment");
        }
    }

    /// E2E-UPS-33/L0: a deliberately clear roughly-twofold native invoice must take the normal
    /// dispute path before any claim/payment, without treating tokenizer output as authority.
    /// E2E-ROW: E2E-UPS-33/L0
    #[tokio::test]
    #[ignore = "EXPECTED TO FAIL until the coarse usage gate rejects a twofold gross invoice"]
    async fn e2e_ups_33_twofold_gross_invoice_is_rejected() {
        const OPENAI_KEY: &str = "DEXDO_UPS33_OPENAI_KEY";
        const ANTHROPIC_KEY: &str = "DEXDO_UPS33_ANTHROPIC_KEY";
        std::env::set_var(OPENAI_KEY, "test-key");
        std::env::set_var(ANTHROPIC_KEY, "test-key");

        // This exact 400-character ASCII payload is the row's fixed visible service. Both provider
        // adapters see the same bytes; only the native invoice moves from honest 100 to twofold 200.
        let visible_payload = format!("mock-reply{}", "x".repeat(390));
        assert!(visible_payload.is_ascii());
        assert_eq!(visible_payload.len(), 400);
        let (openai_twofold, anthropic_twofold) = ups_visible_usage_fixtures(&visible_payload, 200);

        let mut complete = true;
        let mut observations = Vec::new();

        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_openai_content_gate_arm(
                &openai_twofold,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                OPENAI_KEY,
                &["UPS33 twofold"],
            )
            .await;
            let (status, body) = &responses[0];
            complete &= *status != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0
                && !body.contains(visible_payload.as_str());
            observations.push(format!(
                "openai/{subscription}/twofold: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
            ));
        }
        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_anthropic_content_gate_arm(
                &anthropic_twofold,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                ANTHROPIC_KEY,
                &["UPS33 twofold"],
            )
            .await;
            let (status, body) = &responses[0];
            complete &= *status != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0
                && !body.contains(visible_payload.as_str());
            observations.push(format!(
                "anthropic/{subscription}/twofold: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
            ));
        }

        // Embedded negative: one ordinary honest arm still proceeds through this exact gate.
        let (honest, _) = ups_visible_usage_fixtures(&visible_payload, 100);
        let (responses, disputes, money, delivered) = observe_ups_openai_content_gate_arm(
            &honest,
            false,
            probe_models(
                "identity probe",
                "https://reference.invalid/v1",
                "DEXDO_FIXTURE_ABSENT_KEY",
            ),
            OPENAI_KEY,
            &["UPS33 honest"],
        )
        .await;
        let (status, body) = &responses[0];
        complete &= *status == reqwest::StatusCode::OK
            && disputes == 0
            && money == 0
            && delivered == 100
            && body.contains(visible_payload.as_str());
        observations.push(format!(
            "openai/false/honest-negative: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
        ));
        std::env::remove_var(OPENAI_KEY);
        std::env::remove_var(ANTHROPIC_KEY);
        if !complete {
            eprintln!("{}", observations.join("\n"));
            panic!("E2E-UPS-33 twofold gross invoice did not dispute before payment");
        }
    }

    /// E2E-UPS-34/L0: a tenfold native invoice is unambiguously gross. The same incident replay
    /// still opens exactly one dispute and no claim/payment path.
    /// E2E-ROW: E2E-UPS-34/L0
    #[tokio::test]
    #[ignore = "EXPECTED TO FAIL until the coarse usage gate rejects a tenfold gross invoice"]
    async fn e2e_ups_34_tenfold_gross_invoice_is_rejected() {
        const OPENAI_KEY: &str = "DEXDO_UPS34_OPENAI_KEY";
        const ANTHROPIC_KEY: &str = "DEXDO_UPS34_ANTHROPIC_KEY";
        std::env::set_var(OPENAI_KEY, "test-key");
        std::env::set_var(ANTHROPIC_KEY, "test-key");

        // The same exact 400-character ASCII payload is encoded for both providers. Native 1000
        // against honest 100 is therefore an objective tenfold control, without a tolerance rule.
        let visible_payload = format!("mock-reply{}", "x".repeat(390));
        assert!(visible_payload.is_ascii());
        assert_eq!(visible_payload.len(), 400);
        let (openai_tenfold, anthropic_tenfold) =
            ups_visible_usage_fixtures(&visible_payload, 1000);

        let mut complete = true;
        let mut observations = Vec::new();

        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_openai_content_gate_arm(
                &openai_tenfold,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                OPENAI_KEY,
                &["UPS34 tenfold", "UPS34 replay"],
            )
            .await;
            let (first, first_body) = &responses[0];
            let (replay, replay_body) = &responses[1];
            complete &= *first != reqwest::StatusCode::OK
                && *replay != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0
                && !first_body.contains(visible_payload.as_str())
                && !replay_body.contains(visible_payload.as_str());
            observations.push(format!(
                "openai/{subscription}/tenfold: first={first} replay={replay} disputes={disputes} money={money} delivered={delivered} bodies={first_body:?}/{replay_body:?}"
            ));
        }
        for subscription in [false, true] {
            let (responses, disputes, money, delivered) = observe_ups_anthropic_content_gate_arm(
                &anthropic_tenfold,
                subscription,
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
                ANTHROPIC_KEY,
                &["UPS34 tenfold", "UPS34 replay"],
            )
            .await;
            let (first, first_body) = &responses[0];
            let (replay, replay_body) = &responses[1];
            complete &= *first != reqwest::StatusCode::OK
                && *replay != reqwest::StatusCode::OK
                && disputes == 1
                && money == 0
                && delivered == 0
                && !first_body.contains(visible_payload.as_str())
                && !replay_body.contains(visible_payload.as_str());
            observations.push(format!(
                "anthropic/{subscription}/tenfold: first={first} replay={replay} disputes={disputes} money={money} delivered={delivered} bodies={first_body:?}/{replay_body:?}"
            ));
        }

        // Embedded negative: honest native usage still completes without a dispute.
        let (_, honest) = ups_visible_usage_fixtures(&visible_payload, 100);
        let (responses, disputes, money, delivered) = observe_ups_anthropic_content_gate_arm(
            &honest,
            false,
            probe_models(
                "identity probe",
                "https://reference.invalid/v1",
                "DEXDO_FIXTURE_ABSENT_KEY",
            ),
            ANTHROPIC_KEY,
            &["UPS34 honest"],
        )
        .await;
        let (status, body) = &responses[0];
        complete &= *status == reqwest::StatusCode::OK
            && disputes == 0
            && money == 0
            && delivered == 100
            && body.contains(visible_payload.as_str());
        observations.push(format!(
            "anthropic/false/honest-negative: status={status} disputes={disputes} money={money} delivered={delivered} body={body}"
        ));
        std::env::remove_var(OPENAI_KEY);
        std::env::remove_var(ANTHROPIC_KEY);
        if !complete {
            eprintln!("{}", observations.join("\n"));
            panic!("E2E-UPS-34 tenfold gross invoice did not dispute idempotently before payment");
        }
    }

    /// A fresh deal can pay for the identity verification it owes and still answer.

    /// The live blocker, end to end: an ordinary by-fact deal, a gate that has verified nothing, and
    /// an ordinary caller asking for a couple of tokens. Admission used to reserve the ask and
    /// nothing else, so the verification the deal owed had to come out of it - B8 consumed the whole
    /// grant, B7 was then handed zero and refused ("the admitted grant cannot pay for the identity
    /// verification this deal still owes"), and the first real request on a fresh deal came back 502
    /// with the probe tick burned. Nothing here fabricates a grant: it is computed by the real
    /// admission gate, reached through the real HTTP handler, against a real TLS gateway.
    #[tokio::test]
    async fn fresh_deal_pays_for_its_identity_verification_and_still_answers() {
        const REFERENCE_KEY: &str = "DEXDO_FIXTURE_FRESH_DEAL_REFERENCE_KEY";
        std::env::set_var(REFERENCE_KEY, "test-key");
        // A reference that closes every connection. B7 is live - it buys its seller-side probe out
        // of the same grant, which is the layer the live deal was refused at - and then degrades to
        // a pass (R3) without this test depending on a real reference model.
        let reference_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("closing reference listener");
        let reference_addr = reference_listener.local_addr().unwrap();
        let reference_task = tokio::spawn(async move {
            while let Ok((socket, _)) = reference_listener.accept().await {
                drop(socket);
            }
        });
        let harness = weekly_route_harness_gated(
            PROBE_CLAIM,
            // An ordinary by-fact deal: the shape the blocker was observed on, and the one whose
            // admission never asks the chain anything.
            false,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe(
                "dexdo-mock".to_string(),
                probe_models(
                    "identity probe",
                    &format!("http://{reference_addr}"),
                    REFERENCE_KEY,
                ),
            ),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        let budget = harness.remaining().await;
        let verification = FULL_VERIFICATION_BILLABLE;
        assert!(budget > verification + 5, "the deal itself can afford both");

        let (status, body) = harness
            .ask_full(
                "/v1/chat/completions",
                2,
                "DEXDO_FIXTURE_ECHOLIMIT fresh deal",
                false,
            )
            .await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "a fresh deal must reach its first inference: {body}"
        );
        assert!(
            body.contains("limit=2"),
            "the seller is asked for the CALLER's two tokens - never for the verification headroom \
             left in the grant, and never for an unset limit: {body}"
        );
        assert_eq!(
            harness.delivered().await,
            verification + 5,
            "both verification layers include their provider-native input totals, and the answer \
             still got the two output tokens that were asked for"
        );
        assert_eq!(
            harness.remaining().await,
            budget - verification - 5,
            "the deal is charged for what was delivered and nothing else: what the reservation did \
             not spend came back when the request ended"
        );
        let first_delivery = harness.last_delivery().await;
        assert_eq!(first_delivery.rendered_tokens, 2);
        assert_eq!(
            first_delivery.route_delivered_tokens,
            Some(2 * CONTENT_PROBE_MAX_TOKENS + 2),
            "the stable route-delivery witness includes both verification probes and the answer"
        );

        // The deal is verified now, so the next request owes nothing on top of its own ask.
        let (status, body) = harness
            .ask_full(
                "/v1/chat/completions",
                2,
                "DEXDO_FIXTURE_ECHOLIMIT verified deal",
                false,
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(body.contains("limit=2"), "{body}");
        assert_eq!(
            harness.delivered().await,
            verification + 10,
            "identity verification is owed once per deal, not once per request"
        );

        reference_task.abort();
        let _ = reference_task.await;
        std::env::remove_var(REFERENCE_KEY);
        harness.shutdown().await;
    }

    /// A RESUMED deal is admitted only when it can verify AND still answer.

    /// The fresh deal above starts with a whole week in hand, so its first request never comes near
    /// the floor. A resumed one does: [`ApiDeal::new`] builds a new, unverified gate over whatever
    /// remainder the route was rebuilt with, and a subscription route can be rebuilt in the middle of
    /// a drawn-down week. Adding the debt to what a request ASKS for is not enough on its own -
    /// a reservation is `min(want, free)`, so a positive remainder too small to hold the debt would
    /// still be admitted, spend itself on the probe and refuse the answer for a zero grant: the
    /// reported 502, reached by resuming instead of by starting.

    /// The admission floor is the two output probe budgets. Provider-native input is not knowable at
    /// admission, so the request receives the whole free remainder and terminal usage remains the
    /// authoritative boundary: a remainder equal to the output floor is refused locally, while a
    /// remainder that fits both complete probe records and the answer succeeds.
    #[tokio::test]
    async fn a_resumed_deal_is_admitted_only_when_it_can_verify_and_still_answer() {
        const REFERENCE_KEY: &str = "DEXDO_FIXTURE_RESUMED_DEAL_REFERENCE_KEY";
        const ASK: u64 = 2;
        std::env::set_var(REFERENCE_KEY, "test-key");
        // A reference that closes every connection: B7 buys its seller-side probe out of the grant
        // and then degrades to a pass (R3), so the deal really does spend the whole debt it owes and
        // the rows below are the true boundary rather than a generous one.
        let reference_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("closing reference listener");
        let reference_addr = reference_listener.local_addr().unwrap();
        let reference_task = tokio::spawn(async move {
            while let Ok((socket, _)) = reference_listener.accept().await {
                drop(socket);
            }
        });

        // The answer prompt has three input tokens, so its two requested output tokens need five
        // billable tokens after both terminal verification records.
        for (free, answered) in [
            (VERIFICATION_DEBT, None),
            (FULL_VERIFICATION_BILLABLE + 5, Some(ASK)),
        ] {
            let harness = weekly_route_harness_gated(
                WEEK_QUOTA - u128::from(free),
                // A subscription rebuilt mid-week: the route carries the remainder of a week it did
                // not start, and the gate carries none of the verification a restart threw away.
                true,
                SUB_WEEK_LEN.as_secs(),
                UNCONSTRAINED_UPSTREAM,
                ContentGate::probe(
                    "dexdo-mock".to_string(),
                    probe_models(
                        "identity probe",
                        &format!("http://{reference_addr}"),
                        REFERENCE_KEY,
                    ),
                ),
                crate::seller::UpstreamConfig::Mock,
            )
            .await;
            assert_eq!(harness.remaining().await, free);

            let (status, body) = harness
                .ask_full(
                    "/v1/chat/completions",
                    ASK,
                    "DEXDO_FIXTURE_ECHOLIMIT resumed deal",
                    false,
                )
                .await;

            match answered {
                None => {
                    assert_eq!(
                        status,
                        reqwest::StatusCode::SERVICE_UNAVAILABLE,
                        "free={free}: a remainder one token short of verifying and answering must \
                         be refused: {body}"
                    );
                    assert!(
                        body.contains(UNVERIFIED_BUDGET_CANNOT_COVER_VERIFICATION),
                        "free={free}: refused as what it is, not as a spent deal: {body}"
                    );
                    assert_eq!(
                        harness.delivered().await,
                        0,
                        "free={free}: the refusal comes BEFORE the probe, so nothing is charged"
                    );
                    assert_eq!(
                        harness.remaining().await,
                        free,
                        "free={free}: the whole remainder stays on the route"
                    );
                    assert_eq!(
                        harness.chain.settle_calls(),
                        0,
                        "free={free}: a positive, non-expired remainder is refused locally rather \
                         than submitted to settleWeek"
                    );
                    assert_eq!(
                        harness.chain.reads(),
                        0,
                        "free={free}: a local verification-floor refusal reads no chain state"
                    );
                    assert_eq!(
                        harness.chain.foreign_money_calls(),
                        0,
                        "free={free}: nothing is settled against the seller for a probe he was \
                         never asked to serve"
                    );
                }
                Some(answered) => {
                    assert_eq!(
                        status,
                        reqwest::StatusCode::OK,
                        "free={free}: complete terminal usage fits the held remainder: {body}"
                    );
                    assert!(
                        body.contains(&format!("limit={answered}")),
                        "free={free}: the seller is told what the ANSWER may be - the clamp falls \
                         on the answer, never on the verification floor: {body}"
                    );
                    assert_eq!(
                        harness.delivered().await,
                        FULL_VERIFICATION_BILLABLE + 3 + answered,
                        "free={free}: both complete verification usage records, then the answer's \
                         input and output"
                    );
                    assert_eq!(
                        harness.remaining().await,
                        free - FULL_VERIFICATION_BILLABLE - 3 - answered,
                        "free={free}: the deal is charged for what was delivered and nothing else"
                    );
                }
            }
            harness.shutdown().await;
        }

        reference_task.abort();
        let _ = reference_task.await;
        std::env::remove_var(REFERENCE_KEY);
    }

    /// Provider-native input is unknown before terminal usage, so concurrent first requests are
    /// serialized by one reservation over the whole free route remainder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_concurrent_first_request_is_never_handed_a_partial_verification() {
        const ASK: u64 = 2;
        const FREE: u64 = ASK + 2 * VERIFICATION_DEBT;
        let harness = weekly_route_harness_gated(
            WEEK_QUOTA - FREE as u128,
            false,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe(
                "dexdo-mock".to_string(),
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
            ),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        assert_eq!(harness.remaining().await, FREE);
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");

        let start = Arc::new(tokio::sync::Barrier::new(4));
        let racers: Vec<_> = (0..4)
            .map(|_| {
                let deal = deal.clone();
                let start = start.clone();
                tokio::spawn(async move {
                    start.wait().await;
                    deal.admit(Some(ASK as u32)).await
                })
            })
            .collect();
        let mut admitted = Vec::new();
        let mut refused = Vec::new();
        for racer in racers {
            match racer.await.expect("admission task") {
                RouteBudget::Admitted(reservation) => admitted.push(reservation),
                RouteBudget::Exhausted(reason) => refused.push(reason),
            }
        }

        assert_eq!(
            admitted.len(),
            1,
            "only one of these requests can be paid for in full: {refused:?}"
        );
        assert_eq!(
            admitted[0].granted, FREE,
            "the winner holds the entire free billing remainder"
        );
        for reason in &refused {
            assert!(
                reason.contains(UNVERIFIED_BUDGET_CANNOT_COVER_VERIFICATION),
                "the losers see no unreserved remainder: {reason}"
            );
        }
        assert_eq!(
            deal.remaining_tokens(),
            0,
            "the winner holds the whole remainder while its provider-native input is unknown"
        );
        drop(admitted);
        harness.shutdown().await;
    }

    /// The OTHER side of: once the deal has PAID its verification, its admission floor becomes
    /// zero -- and a remainder smaller than that verification is still servable. The one in-flight
    /// request reserves that whole free billable remainder while provider-native input is unknown.

    /// Every other test drives the gate while the debt is outstanding, where the floor is
    /// positive. All of them stay green under a regression that made the floor unconditional -- one
    /// that reserved `ask + VERIFICATION_DEBT` forever instead of only while the deal owes it. That
    /// regression is not benign: it re-introduces a-shaped refusal on the TAIL of every deal,
    /// where the remainder is smaller than a verification the deal has already paid for. The buyer
    /// is then 503'd off budget it owns, having been charged for the probe once already.

    /// `outstanding_verification_tokens` (`:1366-1373`) is where the distinction lives, and it has
    /// no direct test: its only two references are the call sites at `:828` and `:928`. This row
    /// asserts it through the admission gate rather than by calling it, so the guarantee survives a
    /// refactor of the helper.

    /// **The verdict is cached by a REAL first request, not by construction.** An earlier revision
    /// used a `ContentCheck::Skip` gate, whose debt is zero before the test starts; it never drove
    /// a probe, so reintroducing the first-request exhaustion would not have failed it. Here the
    /// gate is a real `Probe`, the first request goes through the real HTTP handler and pays the
    /// real fingerprint layer, and only then is the post-verdict floor asserted. Both halves of
    /// therefore hold in one test: the first request is served while the debt is outstanding,
    /// and the tail is served after it is discharged.

    /// GREEN on this head -- a regression guard, not a specification.
    #[tokio::test]
    async fn a_paid_verification_stops_inflating_every_later_reservation() {
        const ASK: u64 = 2;

        let harness = weekly_route_harness_gated(
            PROBE_CLAIM,
            // An ordinary by-fact deal: the shape was reported on.
            false,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe(
                "dexdo-mock".to_string(),
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    // Absent, so B7 degrades to a pass without spending and the fixture pays for
                    // exactly one probe layer. The VERDICT is still cached, which is what matters.
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
            ),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        assert_eq!(
            deal.content_gate.outstanding_verification_tokens(),
            VERIFICATION_DEBT,
            "fixture guard: the deal starts OWING its identity verification"
        );

        // The first real request, through the real handler: this is what pays the gate and caches
        // the verdict. It is also's own headline case -- a fresh deal must be SERVED.
        let (status, body) = harness
            .ask_full(
                "/v1/chat/completions",
                ASK,
                "DEXDO_FIXTURE_ECHOLIMIT first request",
                false,
            )
            .await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "a fresh deal must reach its first inference: {body}"
        );
        assert_eq!(
            deal.content_gate.outstanding_verification_tokens(),
            0,
            "the real first request must leave a cached verdict, or the tail below is meaningless"
        );

        // Now the TAIL: drive the ceiling down so the whole remainder is smaller than the
        // verification this deal has already paid for. Written against the constant, because a
        // remainder ABOVE the debt would let the unconditional-floor regression pass.
        const _: () = assert!(ASK < VERIFICATION_DEBT);
        let spent = deal.reserved_billable_tokens.load(Ordering::SeqCst);
        deal.token_ceiling.store(spent + ASK, Ordering::SeqCst);
        assert_eq!(deal.remaining_tokens(), ASK, "the tail is the fixture");

        let RouteBudget::Admitted(reservation) = deal.admit(Some(ASK as u32)).await else {
            panic!(
                "a deal that has already paid for its verification was refused a remainder it can \
                 serve in full; the admission floor did not drop once the debt was discharged"
            );
        };
        assert_eq!(
            reservation.granted, ASK,
            "a discharged verification must not keep inflating every later reservation"
        );
        assert_eq!(
            deal.remaining_tokens(),
            0,
            "the whole remainder is handed to the one request that asked for it"
        );
        drop(reservation);
        harness.shutdown().await;
    }

    /// Verification headroom is never served to the caller as answer tokens.

    /// Admission reserves the deal's unpaid verification on top of the ask, so the grant a handler
    /// still HOLDS when it caps the answer can be far larger than the caller's own limit. Here the
    /// fingerprint layer is the only one that spends - B7 has no reference key and degrades without
    /// spending - so its complete usage is 66 tokens and the rest is still held.
    /// `cap_canon_to_grant`
    /// returns the caller's figure and BOTH handlers must enforce THAT on the way back. A handler
    /// that reached for `request_guard.remaining_grant()` instead would be indistinguishable against
    /// a compliant seller, which is why this one is not compliant: told 2, it answers with a
    /// one-token chunk and then a two-token chunk, straddling the ask. Only the first may be shown;
    /// without a valid terminal usage record neither answer chunk is charged, and unused headroom
    /// comes back.
    #[tokio::test]
    async fn verification_headroom_is_never_served_as_answer_tokens() {
        const ASK: u64 = 2;
        const GRANT: u64 = ASK + VERIFICATION_DEBT;
        // B8 probes for the canonical budget; B7 degrades on a missing reference key without
        // spending, so exactly half of the reserved debt is still held when the answer is capped.
        const VERIFICATION_SPEND: u64 = CONTENT_PROBE_MAX_TOKENS + 2;
        const HELD_AT_CAP: u64 = GRANT - VERIFICATION_SPEND;
        // The divergence this test exists to catch, made a property of the fixture rather than of
        // the run: the two candidate caps are different numbers, so a handler that binds the held
        // grant instead of the returned caller cap cannot pass here by coincidence.
        const _: () = assert!(
            HELD_AT_CAP > ASK,
            "the held grant must exceed the caller's ask at the cap, or the wrong cap is invisible"
        );

        for path in ["/v1/chat/completions", "/v1/messages"] {
            let harness = weekly_route_harness_gated(
                WEEK_QUOTA - GRANT as u128,
                false,
                SUB_WEEK_LEN.as_secs(),
                UNCONSTRAINED_UPSTREAM,
                ContentGate::probe(
                    "dexdo-mock".to_string(),
                    probe_models(
                        "identity probe",
                        "https://reference.invalid/v1",
                        "DEXDO_FIXTURE_ABSENT_KEY",
                    ),
                ),
                crate::seller::UpstreamConfig::Mock,
            )
            .await;
            assert_eq!(harness.remaining().await, GRANT);

            let (status, body) = harness
                .ask_full(
                    path,
                    ASK,
                    "overrun DEXDO_FIXTURE_STRADDLE DEXDO_FIXTURE_ECHOLIMIT",
                    false,
                )
                .await;
            assert_eq!(status, reqwest::StatusCode::OK, "{path}: {body}");
            assert!(
                body.contains("limit=2"),
                "{path}: the seller is told the caller's ask, never the held grant: {body}"
            );
            assert!(
                !body.contains("overrun"),
                "{path}: the straddling chunk crosses the caller's cap and may not be rendered, \
                 however much verification headroom the grant still holds: {body}"
            );
            assert_eq!(
                harness.delivered().await,
                VERIFICATION_SPEND,
                "{path}: the complete probe usage is charged, but the answer has no accepted \
                 terminal usage after its straddling chunk: {body}"
            );
            assert_eq!(
                harness.remaining().await,
                GRANT - VERIFICATION_SPEND,
                "{path}: unused verification headroom and the incomplete answer both return to the \
                 route"
            );
            harness.shutdown().await;
        }
    }

    /// A one-token remainder is refused before verification can burn it ( blocker 2, as
    /// closes it).

    /// bounded what verification may SPEND: with a grant of one the gate was ISSUED a budget of
    /// one rather than the canonical 64, so nothing escaped the reservation. What that could not do
    /// is make the request worth admitting - the whole grant went to the probe, the answer was then
    /// refused for a zero grant, and the caller paid for a probe and got no inference. That is the
    /// live failure, and admission now refuses the shape outright: the token stays on the
    /// route, nothing is charged, and nothing is settled against a seller who was never asked to
    /// serve the probe. The spend bound is untouched - it simply has nothing left to bound here,
    /// because an admitted grant is never smaller than the verification it has to cover.
    #[tokio::test]
    async fn a_one_token_remainder_is_refused_before_verification_can_burn_it() {
        let harness = weekly_route_harness_gated(
            WEEK_QUOTA - 1,
            true,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe(
                "dexdo-mock".to_string(),
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
            ),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        assert_eq!(harness.remaining().await, 1);

        // One token cannot hold the verification this deal owes AND an answer, so the request never
        // reaches the probe rather than spending itself on one.
        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            body.contains(UNVERIFIED_BUDGET_CANNOT_COVER_VERIFICATION),
            "{body}"
        );

        assert_eq!(
            harness.delivered().await,
            0,
            "the probe is not sent at all, so nothing is charged for it: {body}"
        );
        assert_eq!(
            harness.remaining().await,
            1,
            "the token stays on the route instead of buying a probe no answer could follow"
        );
        assert_eq!(
            harness.chain.settle_calls(),
            0,
            "a positive, non-expired remainder is refused locally rather than submitted to \
             settleWeek"
        );
        assert_eq!(
            harness.chain.reads(),
            0,
            "a local verification-floor refusal reads no chain state"
        );
        assert_eq!(
            harness.chain.foreign_money_calls(),
            0,
            "a refusal at admission takes no settlement action against the counterparty"
        );
        harness.shutdown().await;
    }

    /// A noncompliant probe chunk cannot cross the verification cap it was issued.

    /// An admitted grant now covers the whole verification a deal owes, so the probe's own
    /// budget is always the canonical one - which a four-token chunk divides exactly and can no
    /// longer straddle. A seller that chunks 1, 2, 2,... still can: it reaches 63 of the 64 and then
    /// offers two more. That chunk is refused before rendering; because the stream cannot then
    /// reach an accepted terminal usage record, the visible prefix remains unbilled and the whole
    /// reservation comes back.
    #[tokio::test]
    #[ignore = "EXPECTED TO FAIL until a seller harness exists that does not route through \
                cap_canon_to_grant (). Same cause as \
                fat_chunks_stop_exactly_at_the_grant, one level down: the probe's wire limit is \
                CONTENT_PROBE_MAX_TOKENS and our seller reserves exactly that, so after  it \
                refuses the straddling chunk before the buyer's cap can. The buyer's refusal is \
                still the property; it is untested here, not false."]
    async fn a_noncompliant_probe_chunk_cannot_cross_the_verification_cap() {
        const ASK: u64 = 2;
        const GRANT: u64 = ASK + VERIFICATION_DEBT;
        let models = probe_models(
            "DEXDO_FIXTURE_STRADDLE identity probe",
            "https://reference.invalid/v1",
            "DEXDO_FIXTURE_ABSENT_KEY",
        );
        let harness = weekly_route_harness_gated(
            WEEK_QUOTA - GRANT as u128,
            true,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe("dexdo-mock".to_string(), models),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;

        let (status, body) = harness.ask(ASK).await;
        let delivered = harness.delivered().await;
        let remaining = harness.remaining().await;
        // Red-by-design reporting shape (ci/run_red_by_design_tests.sh): the probe is refused as a
        // noncompliant chunk, no content-only prefix moves money, and the grant returns.
        let complete = status == reqwest::StatusCode::BAD_GATEWAY
            && body.contains("noncompliant chunk")
            && delivered == 0
            && remaining == GRANT;
        harness.shutdown().await;
        if !complete {
            eprintln!("status={status} delivered={delivered} remaining={remaining} body={body}");
            panic!("E2E-UPS-39B the probe fixture cannot build a noncompliant seller; it needs a harness that does not route through cap_canon_to_grant ()");
        }
    }

    /// Handler cancellation during B7 returns only the unused part of its held reservation.

    /// The grant is the caller's eight tokens plus the verification the deal owes; the seller
    /// emits one output token per stream. Their complete terminal usage records also include two
    /// input tokens for B8 and nine for B7, so the two probes spend thirteen and the rest is slack
    /// the cancellation has to give back.
    #[tokio::test]
    async fn cancelled_handler_keeps_probe_spend_in_its_reservation() {
        const ASK: u64 = 8;
        const GRANT: u64 = ASK + VERIFICATION_DEBT;
        const REFERENCE_KEY: &str = "DEXDO_FIXTURE_PENDING_REFERENCE_KEY";
        std::env::set_var(REFERENCE_KEY, "test-key");
        let reference_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("pending reference listener");
        let reference_addr = reference_listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let reference_task = tokio::spawn(async move {
            let (socket, _) = reference_listener
                .accept()
                .await
                .expect("B7 reference call");
            let _socket_held_open = socket;
            let _ = seen_tx.send(());
            std::future::pending::<()>().await;
        });
        let models = probe_models(
            "identity probe",
            &format!("http://{reference_addr}"),
            REFERENCE_KEY,
        );
        let harness = weekly_route_harness_gated(
            WEEK_QUOTA - GRANT as u128,
            true,
            SUB_WEEK_LEN.as_secs(),
            1,
            ContentGate::probe("dexdo-mock".to_string(), models),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");
        let state = ApiState {
            buyer: harness.buyer.clone(),
            frame_model: "dexdo-mock".to_string(),
            mock_model: true,
            deals: harness.deals.clone(),
            delivery_events: None,
        };
        let request =
            serde_json::from_value::<crate::buyer::render::OpenAiChatRequest>(serde_json::json!({
                "model": "dexdo-mock",
                "messages": [{"role": "user", "content": "ordinary answer"}],
                "max_tokens": 8,
                "stream": false
            }))
            .expect("OpenAI request");
        let mut handler = tokio::spawn(async move {
            openai::chat_completions(axum::extract::State(state), axum::Json(request)).await
        });

        // which of the two outcomes happened is decided by facts, not by a clock. Reaching the
        // reference point has exactly two exits and both arrive as events -- B7 connects to the
        // pending endpoint, or the handler resolves without ever calling it, which is precisely the
        // defect the assertions below exist to catch. Neither is produced by a busy machine, so a
        // loaded builder only makes the first one arrive later. The five-second deadline this
        // replaces decided the VERDICT: it could not tell "the client never got there" apart from
        // "the runner was busy", and chose the second on a builder running five concurrent compiles.

        // The deadline below decides nothing about the verdict; it exists only so a state where
        // NEITHER event can ever arrive -- a reference notifier kept alive and never fired, a
        // deadlock -- ends the test instead of the runner. It has to be finite because the gate this
        // test runs in declares no `timeout-minutes`, so an unbounded hang costs the job's default
        // 360 minutes on every leg of the matrix. Ninety seconds is ten times the worst wall this
        // test has ever taken: 9.06 s, measured with the process descheduled for 990 ms of every
        // second. Load does not reach it. Only a break that stops both events does.
        const NO_EVENT_CAN_STILL_ARRIVE: Duration = Duration::from_secs(90);
        tokio::time::timeout(NO_EVENT_CAN_STILL_ARRIVE, async {
            tokio::select! {
                seen = seen_rx => seen.expect("B7 reference notification"),
                finished = &mut handler => {
                    let outcome = match finished {
                        Ok(response) => format!("it answered HTTP {}", response.status()),
                        Err(join) => format!("it ended abnormally: {join}"),
                    };
                    panic!(
                        "B7 never reached the pending reference endpoint: the handler resolved first, so \
                         the reservation the assertions below read was never held -- {outcome}"
                    );
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "neither event arrived in {NO_EVENT_CAN_STILL_ARRIVE:?}: B7 did not reach the pending \
                 reference endpoint AND the handler did not resolve either. This is a defect, not a slow \
                 machine -- the deadline is ten times this test's worst measured wall under a process \
                 given 1% of a core, so load cannot reach it. Something stopped both events: a reference \
                 notifier that is alive but never fires, or a deadlock before the reference call."
            )
        });
        assert_eq!(
            deal.billable_tokens(),
            13,
            "B8 and the seller half of B7 are charged before B7 awaits its reference"
        );
        assert_eq!(deal.remaining_tokens(), 0, "the whole grant is still held");

        handler.abort();
        let _ = handler.await;
        assert_eq!(deal.billable_tokens(), 13);
        assert_eq!(
            deal.remaining_tokens(),
            GRANT - 13,
            "cancellation returns only the unused tokens, not the complete paid probe usage"
        );
        reference_task.abort();
        let _ = reference_task.await;
        std::env::remove_var(REFERENCE_KEY);
        harness.shutdown().await;
    }

    /// A partial B8 response is not charged when transport fails before terminal usage.
    #[tokio::test]
    async fn partial_probe_transport_failure_bills_nothing_without_terminal_usage() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const ASK: u64 = 8;
        const GRANT: u64 = ASK + VERIFICATION_DEBT;
        const UPSTREAM_KEY: &str = "DEXDO_FIXTURE_PARTIAL_PROBE_KEY";
        std::env::set_var(UPSTREAM_KEY, "test-key");
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("partial upstream listener");
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream_listener.accept().await.expect("B8 upstream call");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("read B8 request");
                assert!(read > 0, "B8 request headers ended early");
                request.extend_from_slice(&chunk[..read]);
            }
            let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"mock-reply: \"},\"logprobs\":{\"content\":[{\"token\":\"mock-reply\",\"logprob\":-0.1,\"top_logprobs\":[]}]}}]}\n\n";
            let headers = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            socket
                .write_all(headers)
                .await
                .expect("write response headers");
            socket
                .write_all(format!("{:x}\r\n", event.len()).as_bytes())
                .await
                .expect("write event size");
            socket.write_all(event).await.expect("write one B8 event");
            socket.write_all(b"\r\n").await.expect("finish event chunk");
            socket.shutdown().await.expect("truncate chunked response");
        });
        let upstream = crate::seller::UpstreamConfig::OpenAi(crate::seller::OpenAiConfig {
            base_url: format!("http://{upstream_addr}"),
            model: "dexdo-mock".to_string(),
            frame_model: "dexdo-mock".to_string(),
            claimed_model_override: None,
            api_key_env: UPSTREAM_KEY.to_string(),
            tokenizer_family: "mock".to_string(),
            capabilities: crate::seller::Capabilities {
                max_output_tokens: Some(64),
                ..crate::seller::Capabilities::default()
            },
            identity_aliases: Vec::new(),
        });
        let harness = weekly_route_harness_gated(
            WEEK_QUOTA - GRANT as u128,
            true,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe(
                "dexdo-mock".to_string(),
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
            ),
            upstream,
        )
        .await;

        let (status, body) = harness.ask(ASK).await;
        assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(
            harness.delivered().await,
            0,
            "content without a complete terminal usage record cannot move money"
        );
        assert_eq!(
            harness.remaining().await,
            GRANT,
            "the whole grant returns when the failed probe has no terminal usage"
        );
        upstream_task.await.expect("partial upstream task");
        std::env::remove_var(UPSTREAM_KEY);
        harness.shutdown().await;
    }

    /// Exactly one request may hold a billing grant on a deal while provider-native input remains
    /// unknown; dropping it reopens the full unused remainder.
    /// E2E-ROW: E2E-UPS-39/L0
    #[tokio::test]
    async fn one_in_flight_billing_reservation_serializes_a_deal() {
        const ASK: u64 = 64;
        const FREE: u64 = ASK + VERIFICATION_DEBT;
        let harness = weekly_route_harness_gated(
            WEEK_QUOTA - FREE as u128,
            true,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe(
                "dexdo-mock".to_string(),
                probe_models(
                    "identity probe",
                    "https://reference.invalid/v1",
                    "DEXDO_FIXTURE_ABSENT_KEY",
                ),
            ),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        let deal = harness
            .deals
            .current()
            .await
            .expect("the harness route is published");

        let first = match deal.admit(Some(ASK as u32)).await {
            RouteBudget::Admitted(reservation) => reservation,
            RouteBudget::Exhausted(reason) => panic!("first request was refused: {reason}"),
        };
        assert_eq!(first.granted, FREE);
        assert_eq!(deal.remaining_tokens(), 0);

        assert!(matches!(
            deal.admit(Some(ASK as u32)).await,
            RouteBudget::Exhausted(_)
        ));

        drop(first);
        let reopened = match deal.admit(Some(ASK as u32)).await {
            RouteBudget::Admitted(reservation) => reservation,
            RouteBudget::Exhausted(reason) => panic!("unused grant did not reopen: {reason}"),
        };
        assert_eq!(reopened.granted, FREE);
        drop(reopened);
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn ordinary_route_budget_is_unchanged_and_reads_no_chain_state() {
        let harness = weekly_route_harness(WEEK_QUOTA - 8, false).await;

        let (status, body) = harness.ask(8).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");

        let (status, body) = harness.ask(1).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.contains(ORDINARY_BUDGET_EXHAUSTED), "{body}");

        harness.chain.chain_crosses_boundaries(1);
        let (status, body) = harness.ask(1).await;
        assert_eq!(
            status,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "an ordinary route has no weekly boundary to cross: {body}"
        );
        assert!(body.contains(ORDINARY_BUDGET_EXHAUSTED), "{body}");
        assert_eq!(harness.chain.reads(), 0);
        assert_eq!(harness.chain.settle_calls(), 0);
        harness.shutdown().await;
    }
}
