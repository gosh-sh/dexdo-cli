//! Seller-driven consumption-claim orchestrator.

//! The seller is paid only for consumption he CLAIMS, and each claim promotes the previous one to trusted.
//! Without this loop nothing is ever claimed, so a buyer STOP settles at zero and the delivered work is
//! unpaid. `claimTokens()` is **seller-only** on-chain (`onlyOwnerPubkey(_sellerPubkey)`), so the seller
//! process owns the loop; the buyer's role is to stay silent (no dispute) through the promotion window.

//! Nothing is claimable until the PROBE is accepted. `open()` freezes one tick as a trial owed to nobody,
//! and only after `PROBE_WINDOW` of buyer silence may the seller take it and start claiming -- a deal whose
//! probe is never accepted pays the seller nothing at all, so this loop drives that step first.

//! Three things bound a claim, and the driver must respect all of them because the contract REJECTS rather than
//! trims an out-of-bounds claim:
//! - `MIN_CLAIM_INTERVAL` between claims;
//! - the physical rate -- `delta * MIN_SECONDS_PER_TICK <= elapsed * TICK_SIZE`, i.e. no more output than
//! the elapsed time could have produced;
//! - hard per-call `MAX_CLAIM_DELTA == TICK_SIZE`. Waiting longer can satisfy the rate inequality but
//! never permits a multi-tick single call; backlog is split across later claims.

//! The last claim of a deal needs `finalize()`: nothing supersedes it, so without the permissionless
//! promotion window it would stay contestable forever and never be paid.

//! Bounds are read per-deal from `TokenContract.getConfig()` so a redeployed contract cannot desync the
//! driver from what the chain will accept; tests inject short bounds.

use super::gateway::ChainUnavailableAction;
use dexdo_core::params::{
    ClaimConfirmationParams, MAX_CLAIM_DELTA, PROBE_SEED_TOKENS,
    SELLER_TERMINAL_RECEIPT_POLL_INTERVAL,
};
use dexdo_core::{
    ChainBackend, ChainError, ClaimBounds, DealChainState, Note, TokenContract,
    CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX, TICK_SIZE,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn display_token_contract(token_contract: &str) -> String {
    dexdo_core::address::display_self_dapp(token_contract)
}

/// Per-deal claim cadence, mirrored from `TokenContract.getConfig()`.
#[derive(Debug, Clone, Copy)]
pub struct AdvanceWindows {
    /// Minimum gap between accepted claims (`minClaimInterval`).
    pub claim_interval: Duration,
    /// Generation floor per tick (`minSecondsPerTick`): bounds how large a claim may be for a given wait.
    pub seconds_per_tick: Duration,
    /// Silence after which a pending claim is promotable by anyone (`finalize`).
    pub promote: Duration,
    /// Buyer silence required before the probe tick may be accepted (`PROBE_WINDOW`).
    pub probe: Duration,
}

impl AdvanceWindows {
    /// Build from per-deal [`ClaimBounds`] read off the chain.
    pub fn from_bounds(bounds: ClaimBounds) -> Self {
        Self {
            claim_interval: bounds.min_claim_interval,
            seconds_per_tick: bounds.min_seconds_per_tick,
            promote: bounds.promote_window,
            probe: bounds.probe_window,
        }
    }

    /// Canonical bounds, for tests and for backends that expose no per-deal config.
    pub fn canonical() -> Self {
        Self::from_bounds(ClaimBounds::canonical())
    }

    /// Largest cumulative increment claimable after `elapsed`, mirroring both the on-chain rate bound and
    /// hard per-call `MAX_CLAIM_DELTA`. Claims above this are rejected outright, so the driver clamps before
    /// the money call.
    pub fn max_claim_delta(&self, elapsed: Duration) -> u128 {
        dexdo_core::params::claim_delta_limit(elapsed, self.seconds_per_tick)
    }
}

/// Observes the strict claim states already read by the ordinary-deal driver.

/// The observer never polls or submits independently. Returning an error stops the driver before its next
/// decision/write, which lets capacity persistence fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimDeliveryMeasurement {
    /// The exact [`crate::seller::gateway::DealDelivery::count`] load used at this decision.
    pub deal_delivery_count: u64,
    /// The fresh-process probe snapshot. `None` on a resumed process: the snapshot is not kept once the
    /// probe is accepted, and it plays no part in the claim.
    pub delivery_at_probe: Option<u128>,
    /// The exact cumulative argument passed to `claim_tokens`; absent at the probe decision.
    pub cumulative_tokens: Option<u128>,
}

impl ClaimDeliveryMeasurement {
    /// JSONL fields shared by the seller runtime event and the regression seam. Amounts are decimal strings,
    /// matching the existing machine schemas without passing through a JSON floating-point representation.
    pub fn event_fields(self) -> serde_json::Value {
        let mut fields = serde_json::json!({
            "deal_delivery_count": self.deal_delivery_count.to_string(),
            "delivery_at_probe": self.delivery_at_probe.map(|value| value.to_string()),
        });
        if let (Some(fields), Some(cumulative_tokens)) =
            (fields.as_object_mut(), self.cumulative_tokens)
        {
            fields.insert(
                "cumulative_tokens".to_string(),
                serde_json::Value::String(cumulative_tokens.to_string()),
            );
        }
        fields
    }
}

pub trait ClaimStateObserver: Send + Sync {
    /// Wake an idle claim driver after the gateway publishes billable usage or drains a request.
    fn delivery_changed(&self) -> Option<&tokio::sync::Notify> {
        None
    }

    /// Publish a retry hint from this driver's actual schedule, without changing admission or writes.
    fn observe_retry_after(&self, _token_contract: &TokenContract, _delay: Duration) {}

    fn observe(
        &self,
        token_contract: &TokenContract,
        state: DealChainState,
    ) -> Result<(), ChainError>;

    /// Save the delivered usage at the probe decision before submitting acceptProbe.
    /// Retries may return an already durable boundary; failure must prevent the chain write.
    fn prepare_probe(
        &self,
        _token_contract: &TokenContract,
        delivered: u64,
    ) -> Result<u64, ChainError> {
        Ok(delivered)
    }

    fn active_requests(&self, _token_contract: &TokenContract) -> u32 {
        0
    }

    fn observe_terminal(&self, _token_contract: &TokenContract) -> Result<(), ChainError> {
        Ok(())
    }

    /// Report an exhausted chain read and return the configured seller action.
    fn observe_chain_unavailable(
        &self,
        _token_contract: &TokenContract,
        _error: &ChainError,
    ) -> ChainUnavailableAction {
        ChainUnavailableAction::Stop
    }

    /// Read-only event hook at the fresh probe decision. It is deliberately infallible: losing an operator
    /// output sink must never move, suppress, or duplicate a money-path action.
    fn observe_probe_decision(
        &self,
        _token_contract: &TokenContract,
        _measurement: ClaimDeliveryMeasurement,
    ) {
    }

    /// Read-only event hook after a cumulative `claim_tokens` call returns. The measurement contains the same
    /// counter load and `next` argument the driver already used; the hook cannot alter either.
    fn observe_claim_submitted(
        &self,
        _token_contract: &TokenContract,
        _measurement: ClaimDeliveryMeasurement,
    ) {
    }
}

impl ClaimStateObserver for () {
    fn observe(
        &self,
        _token_contract: &TokenContract,
        _state: DealChainState,
    ) -> Result<(), ChainError> {
        Ok(())
    }
}

async fn required_claim_state(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    observer: &dyn ClaimStateObserver,
) -> Result<DealChainState, ChainError> {
    let state = match chain.deal_state(token_contract).await {
        Ok(state) => state,
        Err(error) => {
            if exhausted_transient_read(&error) {
                observer.observe_chain_unavailable(token_contract, &error);
            }
            return Err(error);
        }
    }
    .ok_or_else(|| {
        ChainError::Chain(format!(
            "TokenContract {}: getState returned no data while reconciling the \
             cumulative claim high-water",
            display_token_contract(token_contract)
        ))
    })?;
    observer.observe(token_contract, state)?;
    Ok(state)
}

fn exhausted_transient_read(error: &ChainError) -> bool {
    matches!(
        error,
        ChainError::Chain(message) | ChainError::Transport(message)
            if message.contains(CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX)
    )
}

fn validated_claim_high_water(
    token_contract: &TokenContract,
    state: DealChainState,
    token_budget: u128,
) -> Result<u128, ChainError> {
    let token_contract = display_token_contract(token_contract);
    if !state.opened || state.disputed || !state.probe_accepted {
        return Err(ChainError::Chain(format!(
            "TokenContract {token_contract}: claim high-water is not actionable \
             (opened={}, disputed={}, probeAccepted={})",
            state.opened, state.disputed, state.probe_accepted
        )));
    }
    if state.tokens_pending < TICK_SIZE {
        return Err(ChainError::Chain(format!(
            "TokenContract {token_contract}: accepted probe reports tokensPending {} below canonical \
             TICK_SIZE {TICK_SIZE}; refusing to reset the cumulative high-water to zero",
            state.tokens_pending
        )));
    }
    if state.tokens_final > state.tokens_pending {
        return Err(ChainError::Chain(format!(
            "TokenContract {token_contract}: tokensFinal {} exceeds tokensPending {}; refusing a \
             regressing claim pipeline",
            state.tokens_final, state.tokens_pending
        )));
    }
    if state.tokens_pending > token_budget {
        return Err(ChainError::Chain(format!(
            "TokenContract {token_contract}: on-chain claim high-water {} exceeds the configured token \
             budget {token_budget}",
            state.tokens_pending
        )));
    }
    Ok(state.tokens_pending)
}

/// The cumulative claim the deal's accepted usage supports: the paid probe tick is the floor, and usage
/// fills that tick before anything beyond it is claimed.
fn claim_target(delivered: u64, token_budget: u128) -> u128 {
    u128::from(delivered)
        .max(PROBE_SEED_TOKENS)
        .min(token_budget)
}

/// Drive one deal's claim loop: repeatedly claim the CUMULATIVE accepted provider-native billable
/// usage, then `finalize()` so the final claim becomes payable. Returns the cumulative tokens
/// successfully claimed.

/// `delivered` is the deal's whole accepted checked input-plus-output usage, including what a previous
/// process of this seller recorded. The claim is `max(one tick, delivered)`: usage first fills the
/// paid probe tick, and beyond it no claim exceeds accepted usage -- a timer firing does
/// not manufacture billable usage. Claims are additionally clamped to the on-chain rate bound, because
/// the contract rejects an over-rate claim outright -- an unclamped driver would lose the whole batch
/// instead of claiming the permitted part of it.

/// The loop ends on a clean external close (buyer STOP / self-destruct), or once the usage stream
/// is complete and all accepted usage has been claimed and promoted. Failed claims retry against
/// authoritative state, using the same delivery, cap and rate bounds as the first attempt.

/// Seller-only: `seller_note` authorizes the on-chain `claimTokens()`.

/// `finalize_last_claim` is false only when the subscription keeper is running for this same deal;
/// that keeper then remains the sole permissionless `finalize()` writer.
#[allow(clippy::too_many_arguments)]
pub async fn drive_advance(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    seller_note: &dyn Note,
    windows: AdvanceWindows,
    tick_budget: u128,
    tick_size: u64,
    finalize_last_claim: bool,
    delivered: Arc<AtomicU64>,
    delivery_done: Arc<AtomicBool>,
) -> Result<u128, ChainError> {
    drive_advance_with_observer(
        chain,
        token_contract,
        seller_note,
        windows,
        tick_budget,
        tick_size,
        finalize_last_claim,
        delivered,
        delivery_done,
        &(),
    )
    .await
}

/// Drive an ordinary deal while exposing only the state reads the driver already performs.
#[allow(clippy::too_many_arguments)]
pub async fn drive_advance_with_observer(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    seller_note: &dyn Note,
    windows: AdvanceWindows,
    tick_budget: u128,
    tick_size: u64,
    finalize_last_claim: bool,
    delivered: Arc<AtomicU64>,
    delivery_done: Arc<AtomicBool>,
    observer: &dyn ClaimStateObserver,
) -> Result<u128, ChainError> {
    assert!(tick_size > 0, "tick_size must be non-zero");
    // Ceiling in TOKENS. The deal's budget is expressed in ticks, so convert once; consumption itself is
    // claimed and disputed in raw tokens, which is why no rounding to whole ticks happens anywhere here.
    let token_budget = tick_budget.saturating_mul(u128::from(tick_size));
    let token_contract_display = display_token_contract(token_contract);
    let mut waiting_event_emitted = false;

    if deal_closed_observed(chain, token_contract, observer).await? {
        return Ok(0);
    }

    // A restarted seller may find the probe and later claims already applied. Read first so restart never
    // repeats `acceptProbe`, then seed the local cumulative cursor from the authoritative on-chain value.
    let before_probe = required_claim_state(chain, token_contract, observer).await?;
    let resumed_after_probe = before_probe.probe_accepted;
    let measured_delivery_at_probe = if !resumed_after_probe {
        if !before_probe.opened || before_probe.disputed || before_probe.tokens_pending != 0 {
            return Err(ChainError::Chain(format!(
                "TokenContract {token_contract_display}: malformed pre-probe claim state \
                 (opened={}, disputed={}, tokensPending={})",
                before_probe.opened, before_probe.disputed, before_probe.tokens_pending
            )));
        }

        // The probe gates everything: wait out the buyer's silence, then take the trial tick. A submit
        // response can race a state update, so the state below -- not the response alone -- decides.
        observer.observe_retry_after(token_contract, windows.probe);
        tokio::time::sleep(windows.probe).await;
        if deal_closed_observed(chain, token_contract, observer).await? {
            return Ok(0);
        }
        // the probe tick is PAID on acceptance, so an elapsed window alone must never take it.
        // Buyer silence accepts recorded billable usage; it does not create usage from nothing
        // . Wait here for the first accepted
        // provider terminal; a TRUE zero-usage terminal -- the session finished, or the deal closed,
        // before any terminal usage -- accepts no probe and finalizes nothing. Any non-empty partial
        // tick counts, so `delivered > 0` is the gate.
        let mut first_tick_wait_emitted = false;
        loop {
            let delivered_tokens = delivered.load(Ordering::Acquire);
            if delivered_tokens > 0 {
                break;
            }
            // Acquire ordering: the producer publishes `delivered` then `delivery_done` with Release, so a
            // `done` observed here implies the matching count is visible -- re-read before giving up, or a
            // token delivered just before `done` would lose its probe (no premature zero).
            if delivery_done.load(Ordering::Acquire) {
                if delivered.load(Ordering::Acquire) > 0 {
                    break;
                }
                return Ok(0);
            }
            if !first_tick_wait_emitted {
                tracing::info!(
                    event = "seller_waiting_for_first_billable_usage",
                    token_contract = %token_contract_display,
                    delivered_tokens,
                    finalized_ticks = 0u128,
                    "seller waiting for first accepted billable input+output usage"
                );
                first_tick_wait_emitted = true;
            }
            // Poll for the first accepted billable token, but stay responsive to an external close.
            observer.observe_retry_after(token_contract, windows.claim_interval);
            if wait_claim_or_closed(chain, token_contract, windows.claim_interval).await {
                return Ok(0);
            }
        }
        // This is the seller's probe decision boundary. The accepted probe is the deal's first, already
        // paid tick: usage before and after this point fills it, and only usage beyond it is claimed.
        let deal_delivery_count = delivered.load(Ordering::Acquire);
        let delivery_at_probe = u128::from(
            observer.prepare_probe(token_contract, deal_delivery_count.min(TICK_SIZE as u64))?,
        );
        if delivery_at_probe == 0
            || delivery_at_probe > u128::from(deal_delivery_count).min(TICK_SIZE)
        {
            return Err(ChainError::Chain(format!(
                "TokenContract {token_contract_display}: invalid durable probe decision prefix {delivery_at_probe}"
            )));
        }
        observer.observe_probe_decision(
            token_contract,
            ClaimDeliveryMeasurement {
                deal_delivery_count,
                delivery_at_probe: Some(delivery_at_probe),
                cumulative_tokens: None,
            },
        );
        let accept_error = chain.accept_probe(token_contract).await.err();
        let after_accept = required_claim_state(chain, token_contract, observer).await?;
        if !after_accept.probe_accepted {
            if deal_closed_observed(chain, token_contract, observer).await? {
                // The buyer walked away from the trial before it could be accepted: the probe burned on both
                // sides and there is nothing left to claim. Not a fault of ours.
                return Ok(0);
            }
            return Err(accept_error.unwrap_or_else(|| {
                ChainError::Chain(format!(
                    "TokenContract {token_contract_display}: acceptProbe returned but probeAccepted stayed false"
                ))
            }));
        }
        Some(delivery_at_probe)
    } else {
        None
    };

    let claim_state = required_claim_state(chain, token_contract, observer).await?;
    let mut claimed = validated_claim_high_water(token_contract, claim_state, token_budget)?;
    let mut last_claim_time = claim_state.last_claim_time;
    let mut offline_claim_started = tokio::time::Instant::now();
    let mut needs_finalize = claim_state.tokens_pending > claim_state.tokens_final;
    let mut claim_retry_pending = false;
    if claimed > TICK_SIZE {
        tracing::info!(
            event = "seller_claim_high_water_resynced",
            token_contract = %token_contract_display,
            on_chain_tokens = claimed,
            "seller resumed from the authoritative cumulative claim high-water"
        );
    }

    loop {
        if claimed >= token_budget {
            break;
        }
        if deal_closed_observed(chain, token_contract, observer).await? {
            break; // closed externally (e.g. buyer STOP) -- nothing more to claim
        }

        // Acquire ordering: the producer publishes `delivered` then `delivery_done` with Release, so a
        // `done` observed here implies the matching `delivered` count is visible (no stale under-read).
        let deal_delivery_count = delivered.load(Ordering::Acquire);
        let target = claim_target(deal_delivery_count, token_budget);
        if claimed >= target {
            if delivery_done.load(Ordering::Acquire) {
                // Re-read after observing `done`: tokens delivered just before it must still be claimed.
                let refreshed = claim_target(delivered.load(Ordering::Acquire), token_budget);
                if claimed >= refreshed {
                    break;
                }
            } else {
                if !waiting_event_emitted {
                    tracing::info!(
                        event = "seller_waiting_for_delivered_tokens",
                        token_contract = %token_contract_display,
                        claimed_tokens = claimed,
                        "seller waiting for accepted billable tokens to claim"
                    );
                    waiting_event_emitted = true;
                }
                let delay = if observer.delivery_changed().is_some() {
                    Duration::ZERO
                } else {
                    windows.claim_interval
                };
                observer.observe_retry_after(token_contract, delay);
                if wait_for_delivery_or_close(
                    chain,
                    token_contract,
                    observer,
                    windows.claim_interval,
                )
                .await?
                {
                    break;
                }
                continue;
            }
        }

        tokio::select! {
            biased;
            () = wait_for_claim_close(chain, token_contract) => {
                observer.observe_terminal(token_contract)?;
                return Ok(claimed);
            }
            result = wait_for_claim_interval(
                chain,
                token_contract,
                observer,
                windows.claim_interval,
                last_claim_time,
                offline_claim_started,
                claim_retry_pending,
            ) => result?,
        }
        let deal_delivery_count = delivered.load(Ordering::Acquire);
        let target = claim_target(deal_delivery_count, token_budget);
        // The observed block can belong to another thread. Base the size on the contract's minimum
        // interval, which it enforces on the execution block, even when the observed clock is ahead.

        // A zero interval is the deterministic test seam: it bypasses only the elapsed-time rate calculation,
        // never the contract's hard one-tick-per-call bound.
        let allowance = if windows.claim_interval.is_zero() {
            MAX_CLAIM_DELTA
        } else {
            windows.max_claim_delta(windows.claim_interval)
        };
        let next = target.min(claimed.saturating_add(allowance));
        // A final claim can be finalized permissionlessly and destroy the contract.
        // Keep that exhausting claim local until every admitted request has terminated.
        if next <= claimed || (next == token_budget && observer.active_requests(token_contract) > 0)
        {
            if wait_for_delivery_or_close(
                chain,
                token_contract,
                observer,
                SELLER_TERMINAL_RECEIPT_POLL_INTERVAL,
            )
            .await?
            {
                break;
            }
            continue;
        }
        let claim_result = chain.claim_tokens(token_contract, seller_note, next).await;
        observer.observe_claim_submitted(
            token_contract,
            ClaimDeliveryMeasurement {
                deal_delivery_count,
                delivery_at_probe: measured_delivery_at_probe,
                cumulative_tokens: Some(next),
            },
        );
        let floor = match claim_result {
            Ok(()) => next,
            Err(ChainError::ClaimHighWaterResync {
                attempted,
                on_chain,
            }) => {
                if attempted != next || on_chain <= next || on_chain > target {
                    return Err(ChainError::Chain(format!(
                        "TokenContract {token_contract_display}: invalid claim resync \
                         (attempted={attempted}, expected={next}, onChain={on_chain}, \
                         deliveryTarget={target}, budget={token_budget})"
                    )));
                }
                tracing::info!(
                    event = "seller_claim_high_water_resynced",
                    token_contract = %token_contract_display,
                    attempted_tokens = next,
                    on_chain_tokens = on_chain,
                    "a concurrent or lost-response claim advanced the chain; resynchronising explicitly"
                );
                on_chain
            }
            Err(error) => {
                crate::telemetry::seller_claim_failed(token_contract, next, &error);
                match error {
                    ChainError::ClaimReplayRecovery { observed, .. } => {
                        claimed.max(observed.unwrap_or(claimed))
                    }
                    _ => claimed,
                }
            }
        };
        if floor > target {
            return Err(ChainError::Chain(format!(
                "TokenContract {token_contract_display}: invalid claim refusal recovery \
                 (onChain={floor}, deliveryTarget={target})"
            )));
        }

        // A confirmed submit is a floor, not permission to release capacity from a stale
        // snapshot. While waiting, keep the route registered and send no further claim.
        let poll = ClaimConfirmationParams::canonical().poll_interval;
        let confirmed_claimed = if floor >= next { floor } else { claimed };
        let (state, on_chain) = loop {
            match chain.deal_state(token_contract).await {
                Ok(Some(state)) if state.is_stopped() => {
                    observer.observe_terminal(token_contract)?;
                    return Ok(confirmed_claimed);
                }
                Ok(Some(state)) => {
                    let on_chain = validated_claim_high_water(token_contract, state, token_budget)?;
                    if on_chain > target || (floor < next && on_chain < claimed) {
                        return Err(ChainError::Chain(format!(
                            "TokenContract {token_contract_display}: invalid claim reconciliation \
                             (previous={claimed}, onChain={on_chain}, deliveryTarget={target})"
                        )));
                    }
                    if on_chain >= floor {
                        break (state, on_chain);
                    }
                }
                Err(error @ ChainError::UnsafeClaimState(_)) => return Err(error),
                _ => {}
            }
            if deal_closed_observed(chain, token_contract, observer).await? {
                return Ok(confirmed_claimed);
            }
            if floor >= next
                && matches!(
                    chain.buyer_stop_settlement(token_contract).await,
                    Ok(Some(_))
                )
            {
                observer.observe_terminal(token_contract)?;
                return Ok(confirmed_claimed);
            }
            observer.observe_retry_after(token_contract, poll);
            tokio::time::sleep(poll).await;
        };
        observer.observe(token_contract, state)?;
        if on_chain == claimed {
            observer.observe_retry_after(token_contract, poll);
            if wait_claim_or_closed(chain, token_contract, poll).await {
                observer.observe_terminal(token_contract)?;
                return Ok(claimed);
            }
        }
        claim_retry_pending = on_chain < next;
        claimed = on_chain;
        last_claim_time = state.last_claim_time;
        offline_claim_started = tokio::time::Instant::now();
        needs_finalize = state.tokens_pending > state.tokens_final;
    }

    // The newest claim is still contestable and nothing will supersede it, so without this the last batch
    // would never be paid. A close may legitimately race the permissionless call; otherwise a failed final
    // promotion must propagate so the ordinary seller never reports terminal success with a contestable tail.
    if needs_finalize && finalize_last_claim {
        tokio::time::sleep(windows.promote).await;
        if let Err(error) = chain.finalize(token_contract).await {
            if deal_closed_observed(chain, token_contract, observer).await? {
                return Ok(claimed);
            }
            tracing::warn!(
                event = "seller_finalize_pending_claim_failed",
                token_contract = %token_contract_display,
                claimed_tokens = claimed,
                %error,
                "the last claim stays contestable until finalize succeeds"
            );
            return Err(error);
        }
    }
    Ok(claimed)
}

async fn wait_for_claim_interval(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    observer: &dyn ClaimStateObserver,
    interval: Duration,
    last_claim_time: u64,
    offline_claim_started: tokio::time::Instant,
    claim_retry_pending: bool,
) -> Result<(), ChainError> {
    if interval.is_zero() {
        return Ok(());
    }
    let anchor = Duration::from_secs(last_claim_time);
    loop {
        let delay = match chain.observed_chain_timestamp().await {
            Ok(Some(now)) => {
                if anchor.is_zero() {
                    return Err(ChainError::UnsafeClaimState(format!(
                        "TokenContract {}: accepted probe has no last claim timestamp",
                        display_token_contract(token_contract)
                    )));
                }
                anchor
                    .saturating_add(interval)
                    .saturating_sub(Duration::from_secs(now))
            }
            Ok(None) => {
                let remaining = interval.saturating_sub(offline_claim_started.elapsed());
                if !remaining.is_zero() {
                    observer.observe_retry_after(token_contract, remaining);
                    tokio::time::sleep(remaining).await;
                }
                return Ok(());
            }
            // A pending claim waits for a usable clock without triggering gateway shutdown.
            Err(_) if claim_retry_pending => ClaimConfirmationParams::canonical().poll_interval,
            Err(error) if exhausted_transient_read(&error) => {
                if observer.observe_chain_unavailable(token_contract, &error)
                    == ChainUnavailableAction::Stop
                {
                    return Err(error);
                }
                ClaimConfirmationParams::canonical().poll_interval
            }
            Err(error) => return Err(error),
        };
        if delay.is_zero() {
            return Ok(());
        }
        observer.observe_retry_after(token_contract, delay);
        // Recheck the network after sleeping: the local timer does not prove a block has arrived.
        tokio::time::sleep(delay).await;
    }
}

async fn wait_for_claim_close(chain: &dyn ChainBackend, token_contract: &TokenContract) {
    while !wait_claim_or_closed(chain, token_contract, SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await
    {
    }
}

async fn wait_for_delivery_or_close(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    observer: &dyn ClaimStateObserver,
    window: Duration,
) -> Result<bool, ChainError> {
    let closed = if let Some(changed) = observer.delivery_changed() {
        tokio::select! {
            () = changed.notified() => false,
            closed = wait_claim_or_closed(chain, token_contract, window) => closed,
        }
    } else {
        wait_claim_or_closed(chain, token_contract, window).await
    };
    if closed {
        observer.observe_terminal(token_contract)?;
    }
    Ok(closed)
}

async fn wait_claim_or_closed(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    window: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL.min(deadline.duration_since(now)))
            .await;
        if deal_closed(chain, token_contract).await {
            return true;
        }
    }
}

/// Whether the deal's stream is closed (settled / self-destructed). A terminal settlement receipt
/// survives account destruction, including a buyer STOP before probe acceptance (`ProbeBurned`).
/// Disappearance or a failed receipt read alone is not terminal proof. Waiting callers retry on their
/// existing poll cadence; this check never adds a separate receipt wait to a live deal.
async fn deal_closed(chain: &dyn ChainBackend, token_contract: &TokenContract) -> bool {
    match chain.snapshot(token_contract).await {
        Some(snapshot) => snapshot.closed,
        None => matches!(chain.terminal_settlement(token_contract).await, Ok(Some(_))),
    }
}

async fn deal_closed_observed(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    observer: &dyn ClaimStateObserver,
) -> Result<bool, ChainError> {
    let closed = deal_closed(chain, token_contract).await;
    if closed {
        observer.observe_terminal(token_contract)?;
    }
    Ok(closed)
}

#[cfg(test)]
pub(crate) use tests::exercise_claim_retry_metrics;

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::params::SELLER_TERMINAL_RECEIPT_TIMEOUT;
    use dexdo_core::{
        LocalNote, Match, OfferListing, SellOffer, Settlement, StreamSnapshot, TICK_SIZE,
    };
    use std::collections::VecDeque;

    fn claim_state(
        probe_accepted: bool,
        tokens_final: u128,
        tokens_pending: u128,
    ) -> DealChainState {
        DealChainState {
            funded: true,
            opened: true,
            probe_accepted,
            disputed: false,
            deposit: 10_000,
            finalized_owed: 0,
            tokens_final,
            tokens_pending,
            probe_tick: if probe_accepted { 0 } else { 1_000 },
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 0,
        }
    }

    /// Canonical bounds: one tick per minute, claims spaced a minute apart, promotion after two.
    #[test]
    fn canonical_bounds_allow_exactly_one_tick_per_minute() {
        let w = AdvanceWindows::canonical();
        assert_eq!(w.claim_interval, Duration::from_secs(60));
        assert_eq!(w.seconds_per_tick, Duration::from_secs(60));
        assert_eq!(
            w.promote,
            Duration::from_secs(60),
            "4.0.35: CLAIM_PROMOTE_WINDOW is MIN_SECONDS_PER_TICK, not twice it"
        );
        assert_eq!(
            w.promote, w.claim_interval,
            "the window EQUALS the claim interval, which is what makes the next claim always \
             arrive with the previous one ripe and leaves exactly one unpromoted tick"
        );
        assert_eq!(
            w.max_claim_delta(Duration::from_secs(60)),
            TICK_SIZE,
            "a minute of generation is worth exactly one tick"
        );
    }

    /// Regression forR20-09: elapsed rate allowance can grow, but the contract's independent
    /// `MAX_CLAIM_DELTA` never permits a multi-tick single call.
    #[test]
    fn waiting_longer_never_exceeds_the_per_call_cap() {
        let w = AdvanceWindows::canonical();
        assert_eq!(w.max_claim_delta(Duration::from_secs(600)), TICK_SIZE);
        assert_eq!(
            w.max_claim_delta(Duration::from_secs(30)),
            TICK_SIZE / 2,
            "half a minute is worth half a tick"
        );
        assert_eq!(w.max_claim_delta(Duration::ZERO), 0);
    }

    /// Bounds come from the deal, so a redeployment with a different rate floor changes both the claim size
    /// and the derived promotion window.
    #[test]
    fn bounds_are_taken_from_the_deal() {
        let w = AdvanceWindows::from_bounds(dexdo_core::ClaimBounds::from_config(30, 120, 600));
        assert_eq!(w.claim_interval, Duration::from_secs(30));
        assert_eq!(w.seconds_per_tick, Duration::from_secs(120));
        assert_eq!(
            w.promote,
            Duration::from_secs(120),
            "4.0.35: the promote window IS the deal's rate floor, not twice it"
        );
        assert_eq!(
            w.max_claim_delta(Duration::from_secs(120)),
            TICK_SIZE,
            "a slower floor means a minute buys less"
        );
    }

    /// Zero windows for tests: no sleeping, and no rate clamp to deadlock against.
    fn instant() -> AdvanceWindows {
        AdvanceWindows {
            claim_interval: Duration::ZERO,
            seconds_per_tick: Duration::from_secs(60),
            promote: Duration::ZERO,
            probe: Duration::ZERO,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2266_retries_failed_claims_on_the_same_open_deal() {
        let backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        let windows = AdvanceWindows::canonical();
        let chain_unavailable =
            || ChainError::Transport(CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX.into());
        backend.claim_failures.lock().unwrap().extend([
            ChainError::Chain("claim not confirmed".into()),
            ChainError::AmbiguousSubmit("producer response lost".into()),
            ChainError::Transport("producer unavailable".into()),
        ]);
        backend.clock_reads.lock().unwrap().extend([
            Ok(Some(1 + windows.claim_interval.as_secs())),
            Err(chain_unavailable()),
        ]);
        backend
            .replay_reads
            .lock()
            .unwrap()
            .push_back(Err(chain_unavailable()));
        let note = LocalNote::generate();
        let observer = RecordingStateObserver::default();
        let delivered = TICK_SIZE + 4;
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc-retry".to_string(),
            &note,
            windows,
            4,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new(delivered as u64)),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .unwrap();
        assert_eq!(claimed, delivered);
        assert_eq!(*backend.attempts.lock().unwrap(), [delivered; 4]);
        assert_eq!(*backend.claims.lock().unwrap(), [delivered]);
        assert_eq!(observer.unavailable.load(Ordering::Relaxed), 0);
    }

    pub(crate) async fn exercise_claim_retry_metrics(tc: &str) {
        let backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        backend.claim_failures.lock().unwrap().extend([
            ChainError::Transport("producer offline".into()),
            ChainError::AmbiguousSubmit("response lost".into()),
        ]);
        let delivered = TICK_SIZE + 4;
        let claimed = drive_advance(
            &backend,
            &tc.to_owned(),
            &LocalNote::generate(),
            instant(),
            3,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new(delivered as u64)),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .unwrap();
        assert_eq!(claimed, delivered);
        assert_eq!(backend.attempts(), [delivered; 3]);
        assert_eq!(backend.claims(), [delivered]);
    }

    /// A backend that records every claimed total and succeeds -- used to prove the driver never claims more
    /// than was delivered, and that what it claims is CUMULATIVE.
    struct RecordingBackend {
        attempts: std::sync::Mutex<Vec<u128>>,
        claim_failures: std::sync::Mutex<VecDeque<ChainError>>,
        claims: std::sync::Mutex<Vec<u128>>,
        finalized: AtomicU64,
        fail_finalize: AtomicBool,
        close_before_finalize_error: AtomicBool,
        close_after_claim: AtomicBool,
        buyer_stop_receipt_after_reads: AtomicU64,
        buyer_stop_receipt_reads: AtomicU64,
        buyer_stop_receipt_errors_remaining: AtomicU64,
        accepts: AtomicU64,
        closed: AtomicBool,
        state: std::sync::Mutex<DealChainState>,
        resync_to: std::sync::Mutex<Option<u128>>,
        replay_refusal: std::sync::Mutex<Option<u128>>,
        replay_reads: std::sync::Mutex<VecDeque<Result<Option<DealChainState>, ChainError>>>,
        chain_started: Option<tokio::time::Instant>,
        clock_reads: std::sync::Mutex<VecDeque<Result<Option<u64>, ChainError>>>,
        confirmation_delay: Duration,
    }
    impl RecordingBackend {
        fn new() -> Self {
            Self {
                attempts: std::sync::Mutex::new(Vec::new()),
                claim_failures: std::sync::Mutex::new(VecDeque::new()),
                claims: std::sync::Mutex::new(Vec::new()),
                finalized: AtomicU64::new(0),
                fail_finalize: AtomicBool::new(false),
                close_before_finalize_error: AtomicBool::new(false),
                close_after_claim: AtomicBool::new(false),
                buyer_stop_receipt_after_reads: AtomicU64::new(u64::MAX),
                buyer_stop_receipt_reads: AtomicU64::new(0),
                buyer_stop_receipt_errors_remaining: AtomicU64::new(0),
                accepts: AtomicU64::new(0),
                closed: AtomicBool::new(false),
                state: std::sync::Mutex::new(claim_state(false, 0, 0)),
                resync_to: std::sync::Mutex::new(None),
                replay_refusal: std::sync::Mutex::new(None),
                replay_reads: std::sync::Mutex::default(),
                chain_started: None,
                clock_reads: std::sync::Mutex::default(),
                confirmation_delay: Duration::ZERO,
            }
        }
        fn resumed(tokens_final: u128, tokens_pending: u128) -> Self {
            Self {
                attempts: std::sync::Mutex::new(Vec::new()),
                claim_failures: std::sync::Mutex::new(VecDeque::new()),
                claims: std::sync::Mutex::new(Vec::new()),
                finalized: AtomicU64::new(0),
                fail_finalize: AtomicBool::new(false),
                close_before_finalize_error: AtomicBool::new(false),
                close_after_claim: AtomicBool::new(false),
                buyer_stop_receipt_after_reads: AtomicU64::new(u64::MAX),
                buyer_stop_receipt_reads: AtomicU64::new(0),
                buyer_stop_receipt_errors_remaining: AtomicU64::new(0),
                accepts: AtomicU64::new(0),
                closed: AtomicBool::new(false),
                state: std::sync::Mutex::new(claim_state(true, tokens_final, tokens_pending)),
                resync_to: std::sync::Mutex::new(None),
                replay_refusal: std::sync::Mutex::new(None),
                replay_reads: std::sync::Mutex::default(),
                chain_started: None,
                clock_reads: std::sync::Mutex::default(),
                confirmation_delay: Duration::ZERO,
            }
        }
        fn resyncing(on_chain: u128) -> Self {
            let backend = Self::new();
            *backend.resync_to.lock().unwrap() = Some(on_chain);
            backend
        }
        fn closed() -> Self {
            let backend = Self::new();
            backend.closed.store(true, Ordering::Relaxed);
            backend
        }
        fn closing_after_claim() -> Self {
            let backend = Self::new();
            backend.close_after_claim.store(true, Ordering::Relaxed);
            backend
                .buyer_stop_receipt_after_reads
                .store(1, Ordering::Relaxed);
            backend
        }
        fn closing_after_claim_with_delayed_receipt() -> Self {
            let backend = Self::closing_after_claim();
            backend
                .buyer_stop_receipt_after_reads
                .store(3, Ordering::Relaxed);
            backend
        }
        fn closing_after_claim_with_transient_receipt_error() -> Self {
            let backend = Self::closing_after_claim_with_delayed_receipt();
            backend
                .buyer_stop_receipt_errors_remaining
                .store(1, Ordering::Relaxed);
            backend
        }
        fn disappearing_after_claim_without_receipt() -> Self {
            let backend = Self::new();
            backend.close_after_claim.store(true, Ordering::Relaxed);
            backend
        }
        fn failing_finalize() -> Self {
            let backend = Self::new();
            backend.fail_finalize.store(true, Ordering::Relaxed);
            backend
        }
        fn closed_before_finalize_error() -> Self {
            let backend = Self::failing_finalize();
            backend
                .close_before_finalize_error
                .store(true, Ordering::Relaxed);
            backend
        }
        fn claims(&self) -> Vec<u128> {
            self.claims.lock().unwrap().clone()
        }
        fn attempts(&self) -> Vec<u128> {
            self.attempts.lock().unwrap().clone()
        }
    }
    #[async_trait::async_trait]
    impl ChainBackend for RecordingBackend {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            unimplemented!()
        }
        async fn post_offer(&self, _: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn read_match(&self, _: &TokenContract) -> Result<Match, ChainError> {
            unimplemented!()
        }
        async fn open_stream(
            &self,
            _: &TokenContract,
            _: Vec<u8>,
            _: &dyn Note,
        ) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            unimplemented!()
        }
        async fn accept_probe(&self, _: &TokenContract) -> Result<(), ChainError> {
            self.accepts.fetch_add(1, Ordering::Relaxed);
            let mut state = self.state.lock().unwrap();
            state.probe_accepted = true;
            state.probe_tick = 0;
            state.tokens_final = TICK_SIZE;
            state.tokens_pending = TICK_SIZE;
            Ok(())
        }

        async fn observed_chain_timestamp(&self) -> Result<Option<u64>, ChainError> {
            if let Some(read) = self.clock_reads.lock().unwrap().pop_front() {
                return read;
            }
            Ok(self
                .chain_started
                .map(|start| 1 + start.elapsed().as_secs()))
        }

        async fn claim_tokens(
            &self,
            _: &TokenContract,
            _: &dyn Note,
            cumulative: u128,
        ) -> Result<(), ChainError> {
            self.attempts.lock().unwrap().push(cumulative);
            if let Some(error) = self.claim_failures.lock().unwrap().pop_front() {
                return Err(error);
            }
            if let Some(observed) = self.replay_refusal.lock().unwrap().take() {
                if self.close_after_claim.load(Ordering::Relaxed) {
                    self.closed.store(true, Ordering::Release);
                }
                return Err(ChainError::ClaimReplayRecovery {
                    message: "TVM_ERROR exit_code=52".into(),
                    observed: Some(observed),
                    read_failure: None,
                });
            }
            if let Some(on_chain) = self.resync_to.lock().unwrap().take() {
                self.state.lock().unwrap().tokens_pending = on_chain;
                return Err(ChainError::ClaimHighWaterResync {
                    attempted: cumulative,
                    on_chain,
                });
            }
            {
                let mut state = self.state.lock().unwrap();
                if let Some(started) = self.chain_started {
                    let now = 1 + started.elapsed().as_secs();
                    let interval = AdvanceWindows::canonical().claim_interval.as_secs();
                    if now < state.last_claim_time + interval {
                        return Err(ChainError::ClaimWindowRefused(
                            "TC window still open".into(),
                        ));
                    }
                    state.last_claim_time = now;
                }
                state.tokens_pending = cumulative;
            }
            self.claims.lock().unwrap().push(cumulative);
            if !self.confirmation_delay.is_zero() {
                tokio::time::sleep(self.confirmation_delay).await;
            }
            if self.close_after_claim.load(Ordering::Relaxed) {
                self.closed.store(true, Ordering::Release);
            }
            Ok(())
        }
        async fn finalize(&self, _: &TokenContract) -> Result<(), ChainError> {
            self.finalized.fetch_add(1, Ordering::Relaxed);
            if self.fail_finalize.load(Ordering::Relaxed) {
                if self.close_before_finalize_error.load(Ordering::Relaxed) {
                    self.closed.store(true, Ordering::Release);
                }
                return Err(ChainError::Chain("finalize boom".to_string()));
            }
            let mut state = self.state.lock().unwrap();
            state.tokens_final = state.tokens_pending;
            Ok(())
        }
        async fn deal_state(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            if self.closed.load(Ordering::Acquire) {
                return Ok(None);
            }
            if !self.attempts.lock().unwrap().is_empty() {
                if let Some(read) = self.replay_reads.lock().unwrap().pop_front() {
                    if let Ok(Some(state)) = &read {
                        *self.state.lock().unwrap() = *state;
                    }
                    return read;
                }
            }
            Ok(Some(*self.state.lock().unwrap()))
        }
        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }
        async fn buyer_stop_settlement(
            &self,
            _: &TokenContract,
        ) -> Result<Option<(u128, u128)>, ChainError> {
            let read = self.buyer_stop_receipt_reads.fetch_add(1, Ordering::AcqRel) + 1;
            if self
                .buyer_stop_receipt_errors_remaining
                .load(Ordering::Acquire)
                > 0
            {
                self.buyer_stop_receipt_errors_remaining
                    .fetch_sub(1, Ordering::AcqRel);
                return Err(ChainError::Chain(
                    "transient receipt read failed".to_string(),
                ));
            }
            Ok((self.closed.load(Ordering::Acquire)
                && self.state.lock().unwrap().probe_accepted
                && read >= self.buyer_stop_receipt_after_reads.load(Ordering::Acquire))
            .then_some((0, 0)))
        }
        async fn probe_burned_settlement(
            &self,
            _: &TokenContract,
        ) -> Result<Option<(u128, u128, u128)>, ChainError> {
            Ok((self.closed.load(Ordering::Acquire)
                && !self.state.lock().unwrap().probe_accepted
                && self.buyer_stop_receipt_reads.load(Ordering::Acquire)
                    >= self.buyer_stop_receipt_after_reads.load(Ordering::Acquire))
            .then_some((0, 0, 0)))
        }
        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            if self.close_after_claim.load(Ordering::Acquire) && self.closed.load(Ordering::Acquire)
            {
                return None;
            }
            Some(StreamSnapshot {
                seller_locked: 0,
                buyer_locked: 0,
                buyer_lead: 0,
                tokens_final: self.state.lock().unwrap().tokens_final,
                seller_received: 0,
                buyer_refunded: 0,
                burned: 0,
                closed: self.closed.load(Ordering::Acquire),
            })
        }
    }

    async fn drive_recording(
        backend: &RecordingBackend,
        windows: AdvanceWindows,
        tick_budget: u128,
        delivered: u64,
    ) -> u128 {
        drive_advance(
            backend,
            &"tc".to_string(),
            &LocalNote::generate(),
            windows,
            tick_budget,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new(delivered)),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .unwrap()
    }

    /// money path: an elapsed `PROBE_WINDOW` alone must NEVER take the paid probe tick. With the
    /// deal open and ZERO accepted billable usage the driver keeps waiting -- `acceptProbe` is not called, nothing is
    /// claimed or finalized -- and it reports the wait exactly once through its structured event, so a live
    /// seller is visibly waiting rather than silently hung.
    #[test]
    fn zero_billable_usage_waits_instead_of_accepting_the_probe() {
        struct SharedWriter(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("capture tracing output").extend(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let windows = AdvanceWindows {
            claim_interval: Duration::from_millis(5),
            seconds_per_tick: Duration::from_secs(60),
            promote: Duration::ZERO,
            probe: Duration::from_millis(1),
        };
        let backend = Arc::new(RecordingBackend::new());
        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || SharedWriter(captured.clone()))
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build test runtime");
        let driver_backend = backend.clone();
        let wait_logs = logs.clone();
        let outcome = tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async move {
                let task = tokio::spawn(async move {
                    drive_advance(
                        driver_backend.as_ref(),
                        &"tc-zero-delivery".to_string(),
                        &LocalNote::generate(),
                        windows,
                        4,
                        TICK_SIZE as u64,
                        true,
                        Arc::new(AtomicU64::new(0)),
                        Arc::new(AtomicBool::new(false)),
                    )
                    .await
                });
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    let seen = String::from_utf8_lossy(
                        &wait_logs.lock().expect("read in-flight tracing output"),
                    )
                    .contains("seller_waiting_for_first_billable_usage");
                    if seen {
                        break;
                    }
                    assert!(
                        !task.is_finished(),
                        "zero billable usage must keep the seller waiting, not return"
                    );
                    assert!(
                        std::time::Instant::now() < deadline,
                        "driver never reported that it is waiting for the first billable usage"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                // Several further poll rounds: the wait stays silent and stays off the money path.
                tokio::time::sleep(Duration::from_millis(60)).await;
                assert!(!task.is_finished(), "the open zero-usage deal is not done");
                task.abort();
                let _ = task.await;
                String::from_utf8_lossy(&wait_logs.lock().expect("read tracing output"))
                    .into_owned()
            })
        });

        assert_eq!(
            outcome
                .matches("seller_waiting_for_first_billable_usage")
                .count(),
            1,
            "the waiting event is emitted once per invocation, not once per poll: {outcome}"
        );
        assert!(
            outcome.contains("delivered_tokens=0") && outcome.contains("finalized_ticks=0"),
            "the waiting event must name the zero facts it is waiting on: {outcome}"
        );
        assert_eq!(
            backend.accepts.load(Ordering::Relaxed),
            0,
            "an elapsed probe window with zero accepted billable usage must not take the paid probe tick"
        );
        assert!(
            backend.claims().is_empty(),
            "nothing delivered, nothing claimed"
        );
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
        let state = *backend.state.lock().unwrap();
        assert!(!state.probe_accepted);
        assert_eq!(state.tokens_pending, 0);
        assert_eq!(state.tokens_final, 0);
    }

    /// A failed final promotion with no authoritative terminal chain fact must remain a failed deal driver.
    /// Returning the claimed cursor here would let the ordinary seller unregister while its newest claim is
    /// still contestable.
    #[tokio::test]
    async fn drive_advance_propagates_finalize_error_without_terminal_chain_fact() {
        let backend = RecordingBackend::failing_finalize();
        let error = drive_advance(
            &backend,
            &"tc-finalize-boom".to_string(),
            &LocalNote::generate(),
            instant(),
            4,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new((TICK_SIZE + 5) as u64)),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .expect_err("a non-terminal finalize failure must propagate");

        assert!(error.to_string().contains("finalize boom"), "{error}");
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 1);
        let state = *backend.state.lock().unwrap();
        assert_eq!(state.tokens_final, TICK_SIZE);
        assert_eq!(state.tokens_pending, TICK_SIZE + 5);
    }

    /// A lost finalize response may race an independently completed close. The immediate authoritative
    /// reread is the only case where the submit error is terminal success rather than a propagated fault.
    #[tokio::test]
    async fn drive_advance_accepts_finalize_error_after_authoritative_close() {
        let backend = RecordingBackend::closed_before_finalize_error();
        let claimed = drive_advance(
            &backend,
            &"tc-finalize-closed".to_string(),
            &LocalNote::generate(),
            instant(),
            4,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new((TICK_SIZE + 5) as u64)),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .expect("the authoritative close reconciles the finalize response error");

        assert_eq!(claimed, TICK_SIZE + 5);
        assert!(backend.closed.load(Ordering::Acquire));
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 1);
    }

    #[derive(Default)]
    struct RecordingStateObserver {
        retry_delays: std::sync::Mutex<Vec<Duration>>,
        states: std::sync::Mutex<Vec<DealChainState>>,
        terminals: AtomicU64,
        delivery: Option<super::super::gateway::DealDelivery>,
        gateway: Option<Arc<super::super::gateway::GatewayState>>,
        unavailable: AtomicU64,
    }

    impl ClaimStateObserver for RecordingStateObserver {
        fn delivery_changed(&self) -> Option<&tokio::sync::Notify> {
            self.delivery.as_ref().map(|delivery| delivery.changed())
        }

        fn observe_retry_after(&self, _: &TokenContract, delay: Duration) {
            self.retry_delays.lock().unwrap().push(delay);
        }

        fn observe(&self, _: &TokenContract, state: DealChainState) -> Result<(), ChainError> {
            self.states.lock().unwrap().push(state);
            Ok(())
        }

        fn observe_chain_unavailable(
            &self,
            token_contract: &TokenContract,
            _: &ChainError,
        ) -> ChainUnavailableAction {
            self.unavailable.fetch_add(1, Ordering::Relaxed);
            self.gateway
                .as_ref()
                .map_or(ChainUnavailableAction::Stop, |gateway| {
                    gateway.report_chain_unavailable(token_contract)
                })
        }

        fn observe_terminal(&self, _: &TokenContract) -> Result<(), ChainError> {
            self.terminals.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn claim_replay_recovery_waits_for_state_before_the_next_claim() {
        let windows = AdvanceWindows::canonical();
        let poll = ClaimConfirmationParams::canonical().poll_interval;
        for on_chain in [TICK_SIZE, 2 * TICK_SIZE] {
            let backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
            *backend.replay_refusal.lock().unwrap() = Some(on_chain);
            backend.replay_reads.lock().unwrap().extend([
                Err(ChainError::Transport("getter unavailable".into())),
                Ok(None),
                Ok(Some(claim_state(true, TICK_SIZE, on_chain))),
            ]);
            let observer = RecordingStateObserver::default();
            let started = tokio::time::Instant::now();
            let delivered = 2 * TICK_SIZE;
            let claimed = drive_advance_with_observer(
                &backend,
                &"tc".into(),
                &LocalNote::generate(),
                windows,
                4,
                TICK_SIZE as u64,
                false,
                Arc::new(AtomicU64::new(delivered as u64)),
                Arc::new(AtomicBool::new(true)),
                &observer,
            )
            .await
            .expect("same deal keeps advancing");

            assert_eq!(claimed, delivered);
            let mut attempts = vec![delivered];
            let mut delays = vec![windows.claim_interval, poll, poll];
            let mut states = vec![TICK_SIZE, TICK_SIZE];
            if on_chain < delivered {
                attempts.push(delivered);
                delays.push(poll);
                delays.push(windows.claim_interval);
                states.push(TICK_SIZE);
                assert_eq!(backend.claims(), vec![delivered]);
            } else {
                assert!(backend.claims().is_empty());
            }
            states.push(delivered);
            assert_eq!(backend.attempts(), attempts);
            assert_eq!(started.elapsed(), delays.iter().sum::<Duration>());
            assert_eq!(*observer.retry_delays.lock().unwrap(), delays);
            assert_eq!(
                observer
                    .states
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|s| s.tokens_pending)
                    .collect::<Vec<_>>(),
                states
            );
            assert_eq!(observer.terminals.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn claim_replay_recovery_stops_for_unsafe_or_terminal_state() {
        for state in [
            Ok(Some(claim_state(true, TICK_SIZE, TICK_SIZE))),
            Ok(Some(claim_state(true, TICK_SIZE, 4 * TICK_SIZE))),
            Err(ChainError::UnsafeClaimState(
                "non-monotonic pipeline".into(),
            )),
            Ok(None),
        ] {
            let backend = RecordingBackend::resumed(TICK_SIZE, 2 * TICK_SIZE);
            *backend.replay_refusal.lock().unwrap() = Some(2 * TICK_SIZE);
            let terminal = matches!(state, Ok(None));
            if !terminal {
                backend.replay_reads.lock().unwrap().push_back(state);
            } else {
                backend.close_after_claim.store(true, Ordering::Relaxed);
                backend
                    .buyer_stop_receipt_after_reads
                    .store(1, Ordering::Relaxed);
            }
            let observer = RecordingStateObserver::default();
            let result = drive_advance_with_observer(
                &backend,
                &"tc".into(),
                &LocalNote::generate(),
                instant(),
                4,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new((3 * TICK_SIZE) as u64)),
                Arc::new(AtomicBool::new(true)),
                &observer,
            )
            .await;

            if terminal {
                assert_eq!(result.unwrap(), 2 * TICK_SIZE);
                assert_eq!(observer.terminals.load(Ordering::Relaxed), 1);
            } else {
                assert!(matches!(
                    result,
                    Err(ChainError::UnsafeClaimState(_)) | Err(ChainError::Chain(_))
                ));
                assert_eq!(observer.terminals.load(Ordering::Relaxed), 0);
            }
            assert_eq!(backend.attempts(), vec![3 * TICK_SIZE]);
            assert!(backend.claims().is_empty());
            assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_2231_follows_the_probe_and_idle_poll() {
        let backend = RecordingBackend::new();
        let observer = RecordingStateObserver::default();
        let windows = AdvanceWindows::canonical();
        let delivered = Arc::new(AtomicU64::new(TICK_SIZE as u64));
        let done = Arc::new(AtomicBool::new(false));
        let later = delivered.clone();
        let finished = done.clone();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(windows.probe + windows.claim_interval / 2).await;
            later.store((2 * TICK_SIZE) as u64, Ordering::Release);
            finished.store(true, Ordering::Release);
        });
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc-retry-schedule".to_owned(),
            &LocalNote::generate(),
            windows,
            2,
            TICK_SIZE as u64,
            true,
            delivered,
            done,
            &observer,
        )
        .await
        .unwrap();
        producer.await.unwrap();
        assert_eq!(claimed, 2 * TICK_SIZE);
        assert_eq!(
            *observer.retry_delays.lock().unwrap(),
            vec![windows.probe, windows.claim_interval]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn claim_2262_uses_block_time_under_continuous_load() {
        let windows = AdvanceWindows::canonical();
        let mut backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        let started = tokio::time::Instant::now();
        backend.chain_started = Some(started);
        backend.confirmation_delay = Duration::from_secs(10);
        let observer = RecordingStateObserver::default();
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc".into(),
            &LocalNote::generate(),
            windows,
            4,
            TICK_SIZE as u64,
            false,
            Arc::new(AtomicU64::new((3 * TICK_SIZE) as u64)),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .unwrap();

        assert_eq!(claimed, 3 * TICK_SIZE);
        assert_eq!(backend.attempts(), vec![2 * TICK_SIZE, 3 * TICK_SIZE]);
        let interval = windows.claim_interval.as_secs();
        let states = observer.states.lock().unwrap();
        assert_eq!(states[2].last_claim_time, 1 + interval);
        assert_eq!(states[3].last_claim_time, 1 + 2 * interval);
        assert_eq!(
            started.elapsed(),
            2 * windows.claim_interval + backend.confirmation_delay
        );
    }

    #[tokio::test(start_paused = true)]
    async fn claim_2262_wakes_on_delivery_after_the_interval() {
        use super::super::gateway::{
            AuthoritativeDeliveryEvent, AuthoritativeDeliveryRecorder, DealDelivery,
        };
        use std::num::NonZeroU64;

        let windows = AdvanceWindows::canonical();
        let mut backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        let started = tokio::time::Instant::now();
        backend.chain_started = Some(started);
        let delivery = DealDelivery::default();
        delivery.count.store(TICK_SIZE as u64, Ordering::Release);
        let observer = RecordingStateObserver {
            delivery: Some(delivery.clone()),
            ..Default::default()
        };
        let idle = windows.claim_interval + Duration::from_secs(5);
        let token_contract = "tc".to_string();
        let note = LocalNote::generate();
        let drive = drive_advance_with_observer(
            &backend,
            &token_contract,
            &note,
            windows,
            2,
            TICK_SIZE as u64,
            false,
            delivery.count.clone(),
            delivery.done.clone(),
            &observer,
        );
        let deliver = async {
            tokio::time::sleep(idle).await;
            assert!(backend.attempts().is_empty());
            delivery
                .record_authoritative_delivery(AuthoritativeDeliveryEvent::Completed(
                    NonZeroU64::new(TICK_SIZE as u64).unwrap(),
                ))
                .unwrap();
        };
        let (claimed, ()) = tokio::join!(drive, deliver);

        assert_eq!(claimed.unwrap(), 2 * TICK_SIZE);
        assert_eq!(backend.attempts(), vec![2 * TICK_SIZE]);
        assert_eq!(started.elapsed(), idle);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_2262_rereads_the_same_deal_after_ahead_clock_refusal() {
        let windows = AdvanceWindows::canonical();
        let poll = ClaimConfirmationParams::canonical().poll_interval;
        let started = tokio::time::Instant::now();
        let mut backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        backend.chain_started = Some(started);
        backend.clock_reads.lock().unwrap().extend([
            Ok(Some(1 + poll.as_secs())),
            Ok(Some(1 + windows.claim_interval.as_secs())),
        ]);
        let observer = RecordingStateObserver::default();
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc".into(),
            &LocalNote::generate(),
            windows,
            3,
            TICK_SIZE as u64,
            false,
            Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .unwrap();

        assert_eq!(claimed, 2 * TICK_SIZE);
        assert_eq!(backend.attempts(), vec![2 * TICK_SIZE, 2 * TICK_SIZE]);
        assert_eq!(backend.claims(), vec![2 * TICK_SIZE]);
        let states = observer.states.lock().unwrap();
        assert_eq!(states[2].tokens_pending, TICK_SIZE, "reread after refusal");
        assert_eq!(
            states[3].last_claim_time,
            1 + windows.claim_interval.as_secs()
        );
        assert_eq!(started.elapsed(), windows.claim_interval);
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_2262_pending_backlog_stops_on_receipt_while_clock_is_stale() {
        let windows = AdvanceWindows::canonical();
        let backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        backend
            .clock_reads
            .lock()
            .unwrap()
            .extend([Ok(Some(1)), Ok(Some(1))]);
        backend.close_after_claim.store(true, Ordering::Release);
        backend
            .buyer_stop_receipt_after_reads
            .store(1, Ordering::Release);
        let observer = RecordingStateObserver::default();
        let tc = "tc".to_owned();
        let note = LocalNote::generate();
        let drive = drive_advance_with_observer(
            &backend,
            &tc,
            &note,
            windows,
            3,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
            Arc::new(AtomicBool::new(false)),
            &observer,
        );
        let stop = async {
            tokio::time::sleep(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL / 2).await;
            backend.closed.store(true, Ordering::Release);
        };
        let (claimed, ()) = tokio::join!(tokio::time::timeout(windows.claim_interval, drive), stop);
        assert_eq!(
            claimed
                .expect("retire before the stale clock wakes")
                .unwrap(),
            TICK_SIZE
        );
        assert!(backend.attempts().is_empty());
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_2262_keep_serving_waits_for_clock_read_recovery_on_same_route() {
        use super::super::gateway::GatewayState;
        let windows = AdvanceWindows::canonical();
        let poll = ClaimConfirmationParams::canonical().poll_interval;
        let started = tokio::time::Instant::now();
        let mut backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        backend.chain_started = Some(started);
        backend
            .clock_reads
            .lock()
            .unwrap()
            .push_back(Err(ChainError::Transport(
                CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX.into(),
            )));
        let gateway = Arc::new(GatewayState::new());
        gateway.set_chain_unavailable_action(ChainUnavailableAction::KeepServing);
        let tc = "tc-clock-recovery".to_owned();
        let note = LocalNote::generate();
        gateway
            .register_stream(
                &tc,
                note.pubkey(),
                1,
                claim_state(true, TICK_SIZE, TICK_SIZE),
                dexdo_core::DealSubscription {
                    deal_flags: 0,
                    sub_weeks: 0,
                    week_index: 0,
                    tokens_per_week: 3 * TICK_SIZE,
                    funded_tokens: 3 * TICK_SIZE,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                },
            )
            .unwrap();
        let capacity = gateway.capacity_snapshot(&tc).unwrap().unwrap();
        let delivery = gateway.delivery(&tc);
        delivery
            .count
            .store((2 * TICK_SIZE) as u64, Ordering::Release);
        delivery.done.store(true, Ordering::Release);
        let observer = RecordingStateObserver {
            gateway: Some(gateway.clone()),
            ..Default::default()
        };
        let drive = drive_advance_with_observer(
            &backend,
            &tc,
            &note,
            windows,
            3,
            TICK_SIZE as u64,
            false,
            delivery.count,
            delivery.done,
            &observer,
        );
        tokio::pin!(drive);
        tokio::select! {
            result = &mut drive => panic!("the active deal was isolated: {result:?}"),
            () = tokio::time::sleep(poll / 2) => {}
        }
        assert_eq!(observer.unavailable.load(Ordering::Relaxed), 1);
        assert!(backend.attempts().is_empty());
        assert_eq!(gateway.capacity_snapshot(&tc).unwrap(), Some(capacity));

        assert_eq!(drive.await.unwrap(), 2 * TICK_SIZE);
        assert_eq!(backend.attempts(), vec![2 * TICK_SIZE]);
        assert_eq!(started.elapsed(), windows.claim_interval);
    }

    #[tokio::test]
    async fn accepted_probe_state_is_observed_before_ordinary_driver_continues() {
        let backend = RecordingBackend::new();
        let observer = RecordingStateObserver::default();
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc-observed".to_string(),
            &LocalNote::generate(),
            instant(),
            2,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new(TICK_SIZE as u64)),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .unwrap();

        assert_eq!(claimed, TICK_SIZE);
        let states = observer.states.lock().unwrap();
        assert!(!states.first().unwrap().probe_accepted);
        assert!(
            states.iter().skip(1).any(|state| state.probe_accepted),
            "the post-accept state must reach capacity reconciliation"
        );
    }

    #[tokio::test]
    async fn confirmed_claim_state_is_observed_before_ordinary_driver_continues() {
        let backend = RecordingBackend::new();
        let observer = RecordingStateObserver::default();
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc-observed-claim".to_string(),
            &LocalNote::generate(),
            instant(),
            2,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .unwrap();

        assert_eq!(claimed, 2 * TICK_SIZE);
        assert!(
            observer
                .states
                .lock()
                .unwrap()
                .iter()
                .any(|state| state.tokens_pending == 2 * TICK_SIZE),
            "the confirmed post-claim state must release ordinary capacity before another request"
        );
    }

    #[tokio::test]
    async fn clean_buyer_stop_after_confirmed_claim_is_terminal_success() {
        let backend = RecordingBackend::closing_after_claim();
        let observer = RecordingStateObserver::default();
        let token_contract = "tc-close-after-claim".to_string();
        let claimed = drive_advance_with_observer(
            &backend,
            &token_contract,
            &LocalNote::generate(),
            instant(),
            2,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .expect("a clean close after the confirmed claim is terminal success");

        assert_eq!(claimed, 2 * TICK_SIZE);
        assert_eq!(backend.claims(), vec![2 * TICK_SIZE]);
        assert!(
            backend.deal_state(&token_contract).await.unwrap().is_none(),
            "the production-faithful self-destruct removes getState"
        );
        assert!(
            backend.snapshot(&token_contract).await.is_none(),
            "the production snapshot delegates to the same missing getter"
        );
        assert!(
            backend
                .buyer_stop_settlement(&token_contract)
                .await
                .unwrap()
                .is_some(),
            "the immutable buyer-owned StreamStopped receipt survives self-destruct"
        );
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 1);
        assert_eq!(
            backend.finalized.load(Ordering::Relaxed),
            0,
            "the destroyed deal must not receive a redundant finalize submit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_buyer_stop_receipt_after_confirmed_claim_is_terminal_success() {
        let backend = Arc::new(RecordingBackend::closing_after_claim_with_delayed_receipt());
        let observer = Arc::new(RecordingStateObserver::default());
        let driver_backend = backend.clone();
        let driver_observer = observer.clone();
        let driver = tokio::spawn(async move {
            drive_advance_with_observer(
                driver_backend.as_ref(),
                &"tc-delayed-receipt-after-claim".to_string(),
                &LocalNote::generate(),
                instant(),
                2,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
                Arc::new(AtomicBool::new(true)),
                driver_observer.as_ref(),
            )
            .await
        });

        while backend.buyer_stop_receipt_reads.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "one empty receipt read must remain inside the coherent readback wait"
        );
        tokio::time::advance(ClaimConfirmationParams::canonical().poll_interval).await;
        tokio::task::yield_now().await;

        assert_eq!(driver.await.unwrap().unwrap(), 2 * TICK_SIZE);
        assert!(backend.buyer_stop_receipt_reads.load(Ordering::Acquire) >= 2);
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 1);
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_buyer_stop_receipt_error_after_confirmed_claim_is_retried() {
        let backend =
            Arc::new(RecordingBackend::closing_after_claim_with_transient_receipt_error());
        let observer = Arc::new(RecordingStateObserver::default());
        let driver_backend = backend.clone();
        let driver_observer = observer.clone();
        let driver = tokio::spawn(async move {
            drive_advance_with_observer(
                driver_backend.as_ref(),
                &"tc-transient-receipt-after-claim".to_string(),
                &LocalNote::generate(),
                instant(),
                2,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
                Arc::new(AtomicBool::new(true)),
                driver_observer.as_ref(),
            )
            .await
        });

        while backend.buyer_stop_receipt_reads.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        assert!(
            !driver.is_finished(),
            "a transient receipt read error must remain inside the coherent readback wait"
        );
        tokio::time::advance(ClaimConfirmationParams::canonical().poll_interval).await;
        tokio::task::yield_now().await;

        assert_eq!(driver.await.unwrap().unwrap(), 2 * TICK_SIZE);
        assert!(backend.buyer_stop_receipt_reads.load(Ordering::Acquire) >= 2);
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 1);
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2266_post_claim_waits_for_a_delayed_stop_receipt() {
        let backend = Arc::new(RecordingBackend::disappearing_after_claim_without_receipt());
        let observer = Arc::new(RecordingStateObserver::default());
        let driver_backend = backend.clone();
        let driver_observer = observer.clone();
        let driver = tokio::spawn(async move {
            drive_advance_with_observer(
                driver_backend.as_ref(),
                &"tc-missing-after-claim".to_string(),
                &LocalNote::generate(),
                instant(),
                2,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
                Arc::new(AtomicBool::new(true)),
                driver_observer.as_ref(),
            )
            .await
        });
        while backend.buyer_stop_receipt_reads.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let poll = ClaimConfirmationParams::canonical().poll_interval;
        tokio::time::advance(poll).await;
        assert!(!driver.is_finished());
        assert_eq!(backend.attempts(), vec![2 * TICK_SIZE]);
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 0);

        backend
            .buyer_stop_receipt_after_reads
            .store(1, Ordering::Release);
        tokio::time::advance(poll).await;
        assert_eq!(driver.await.unwrap().unwrap(), 2 * TICK_SIZE);
        assert_eq!(backend.attempts(), vec![2 * TICK_SIZE]);
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 1);
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2266_confirmed_claim_waits_for_coherent_capacity_without_resubmission() {
        let backend = RecordingBackend::resumed(TICK_SIZE, TICK_SIZE);
        let windows = AdvanceWindows::canonical();
        let poll = ClaimConfirmationParams::canonical().poll_interval;
        let observer = RecordingStateObserver::default();
        let stale = claim_state(true, TICK_SIZE, TICK_SIZE);
        let current = claim_state(true, TICK_SIZE, 2 * TICK_SIZE);
        backend.replay_reads.lock().unwrap().extend([
            Err(ChainError::Transport(
                CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX.into(),
            )),
            Ok(Some(stale)),
            Ok(Some(current)),
        ]);

        let tc = "tc-readback".to_string();
        let note = LocalNote::generate();
        let drive = drive_advance_with_observer(
            &backend,
            &tc,
            &note,
            windows,
            4,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new((3 * TICK_SIZE) as u64)),
            Arc::new(AtomicBool::new(true)),
            &observer,
        );
        let check = async {
            tokio::time::sleep(windows.claim_interval + poll).await;
            assert_eq!(backend.attempts(), vec![2 * TICK_SIZE]);
            assert!(observer
                .states
                .lock()
                .unwrap()
                .iter()
                .all(|state| state.tokens_pending == TICK_SIZE));
            assert_eq!(observer.unavailable.load(Ordering::Relaxed), 0);
        };
        let (claimed, ()) = tokio::join!(drive, check);
        assert_eq!(claimed.unwrap(), 3 * TICK_SIZE);
        assert_eq!(backend.attempts(), vec![2 * TICK_SIZE, 3 * TICK_SIZE]);
        assert_eq!(
            *observer.retry_delays.lock().unwrap(),
            [windows.claim_interval, poll, poll, windows.claim_interval]
        );
        assert_eq!(
            observer
                .states
                .lock()
                .unwrap()
                .iter()
                .filter(|state| state.tokens_pending == 2 * TICK_SIZE)
                .count(),
            1
        );
    }

    struct RejectingStateObserver;

    impl ClaimStateObserver for RejectingStateObserver {
        fn observe(&self, _: &TokenContract, _: DealChainState) -> Result<(), ChainError> {
            Err(ChainError::Chain(
                "injected capacity persistence failure".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn capacity_observer_failure_stops_before_the_next_money_write() {
        let backend = RecordingBackend::new();
        let error = drive_advance_with_observer(
            &backend,
            &"tc-observer-failure".to_string(),
            &LocalNote::generate(),
            instant(),
            2,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new(TICK_SIZE as u64)),
            Arc::new(AtomicBool::new(true)),
            &RejectingStateObserver,
        )
        .await
        .expect_err("capacity persistence failure must stop the claim driver");

        assert!(
            error
                .to_string()
                .contains("injected capacity persistence failure"),
            "{error}"
        );
        assert_eq!(
            backend.accepts.load(Ordering::Relaxed),
            0,
            "no acceptProbe write may happen after observer failure"
        );
        assert!(backend.claims().is_empty());
    }

    struct RejectingProbeObserver;

    impl ClaimStateObserver for RejectingProbeObserver {
        fn observe(&self, _: &TokenContract, _: DealChainState) -> Result<(), ChainError> {
            Ok(())
        }

        fn prepare_probe(&self, _: &TokenContract, _: u64) -> Result<u64, ChainError> {
            Err(ChainError::Chain("probe boundary write failed".to_string()))
        }
    }

    #[tokio::test]
    async fn probe_boundary_write_failure_prevents_accept_and_claim_2046() {
        let backend = RecordingBackend::new();
        let error = drive_advance_with_observer(
            &backend,
            &"tc-probe-boundary-failure-2046".to_string(),
            &LocalNote::generate(),
            instant(),
            2,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new(57)),
            Arc::new(AtomicBool::new(true)),
            &RejectingProbeObserver,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("probe boundary write failed"));
        assert_eq!(backend.accepts.load(Ordering::Relaxed), 0);
        assert!(backend.claims().is_empty());
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn invalid_probe_boundary_prevents_accept_and_claim_2046() {
        struct InvalidProbeObserver(u64);
        impl ClaimStateObserver for InvalidProbeObserver {
            fn observe(&self, _: &TokenContract, _: DealChainState) -> Result<(), ChainError> {
                Ok(())
            }
            fn prepare_probe(&self, _: &TokenContract, _: u64) -> Result<u64, ChainError> {
                Ok(self.0)
            }
        }
        for invalid in [0, 58, TICK_SIZE as u64 + 1] {
            let backend = RecordingBackend::new();
            let error = drive_advance_with_observer(
                &backend,
                &"tc-invalid-probe-boundary-2046".to_string(),
                &LocalNote::generate(),
                instant(),
                2,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new(57)),
                Arc::new(AtomicBool::new(true)),
                &InvalidProbeObserver(invalid),
            )
            .await
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("invalid durable probe decision prefix"));
            assert_eq!(backend.accepts.load(Ordering::Relaxed), 0);
            assert!(backend.claims().is_empty());
        }
    }

    #[tokio::test]
    async fn claim_driver_reconciles_its_own_delivery_after_probe_decision_2046() {
        use crate::seller::capacity::CapacityManager;
        struct ProbeRaceObserver {
            capacity: CapacityManager,
            delivered: Arc<AtomicU64>,
            deal: dexdo_core::DealSubscription,
        }
        impl ClaimStateObserver for ProbeRaceObserver {
            fn observe(&self, tc: &TokenContract, state: DealChainState) -> Result<(), ChainError> {
                self.capacity
                    .reconcile_deal(tc, state, self.deal)
                    .map(|_| ())
                    .map_err(|error| ChainError::Chain(error.to_string()))
            }

            fn prepare_probe(&self, tc: &TokenContract, delivered: u64) -> Result<u64, ChainError> {
                self.capacity
                    .prepare_probe(tc, delivered)
                    .map_err(|error| ChainError::Chain(error.to_string()))
            }

            fn observe_probe_decision(
                &self,
                tc: &TokenContract,
                measurement: ClaimDeliveryMeasurement,
            ) {
                assert_eq!(measurement.delivery_at_probe, Some(57));
                // A provider terminal arrives after the exact probe decision, before its chain readback.
                self.capacity
                    .reserve(tc, 196)
                    .unwrap()
                    .finish_with_delivery(196)
                    .unwrap();
                self.delivered.fetch_add(196, Ordering::Release);
            }
        }
        let backend = RecordingBackend::new();
        let directory = tempfile::tempdir().unwrap();
        let tc = "tc-probe-claim-race-2046".to_string();
        let observer = ProbeRaceObserver {
            capacity: CapacityManager::in_deals_dir(directory.path().to_path_buf()),
            delivered: Arc::new(AtomicU64::new(57)),
            deal: dexdo_core::DealSubscription {
                deal_flags: 0,
                sub_weeks: 0,
                week_index: 0,
                tokens_per_week: 2 * TICK_SIZE,
                funded_tokens: 2 * TICK_SIZE,
                tokens_paid: 0,
                period_start: 0,
                week_base_tokens: 0,
            },
        };
        observer.observe(&tc, claim_state(false, 0, 0)).unwrap();
        observer
            .capacity
            .reserve(&tc, 57)
            .unwrap()
            .finish_with_delivery(57)
            .unwrap();
        let claimed = drive_advance_with_observer(
            &backend,
            &tc,
            &LocalNote::generate(),
            instant(),
            2,
            TICK_SIZE as u64,
            true,
            observer.delivered.clone(),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .unwrap();
        // usage on both sides of the decision fills the paid probe tick; nothing is claimed on top.
        assert_eq!(claimed, TICK_SIZE);
        assert!(backend.claims().is_empty());
        assert_eq!(backend.accepts.load(Ordering::Relaxed), 1);
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
        let snapshot = observer.capacity.snapshot(&tc).unwrap().unwrap();
        assert_eq!(snapshot.tokens_pending_anchor, 57 + 196);
        assert_eq!(snapshot.local_delivered_after_anchor, 0);
        assert!(observer.capacity.reserve(&tc, 1).is_ok());
    }

    /// regression, the shellnet deal of 2026-10-25: 21 977 tokens at the probe decision, 2 280 217 in
    /// all. The seller claimed 3 258 240 -- the probe tick plus everything after it -- and the buyer paid for
    /// 978 023 tokens never received. The claim must end on the buyer's own usage.
    #[tokio::test]
    async fn usage_after_the_probe_fills_the_paid_tick_before_it_is_claimed_2185() {
        const AT_PROBE: u64 = 21_977;
        const DELIVERED: u64 = 2_280_217;
        struct LaterUsage {
            delivered: Arc<AtomicU64>,
        }
        impl ClaimStateObserver for LaterUsage {
            fn observe(&self, _: &TokenContract, _: DealChainState) -> Result<(), ChainError> {
                Ok(())
            }
            fn observe_probe_decision(
                &self,
                _: &TokenContract,
                measurement: ClaimDeliveryMeasurement,
            ) {
                assert_eq!(measurement.delivery_at_probe, Some(u128::from(AT_PROBE)));
                self.delivered.store(DELIVERED, Ordering::Release);
            }
        }
        let backend = RecordingBackend::new();
        let observer = LaterUsage {
            delivered: Arc::new(AtomicU64::new(AT_PROBE)),
        };
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc-2185".to_string(),
            &LocalNote::generate(),
            instant(),
            10,
            TICK_SIZE as u64,
            true,
            observer.delivered.clone(),
            Arc::new(AtomicBool::new(true)),
            &observer,
        )
        .await
        .unwrap();
        assert_eq!(claimed, u128::from(DELIVERED));
        assert_eq!(backend.claims(), vec![2 * TICK_SIZE, u128::from(DELIVERED)]);
        assert!(backend
            .attempts()
            .iter()
            .all(|claim| *claim <= u128::from(DELIVERED)));
    }

    /// on restart: the gateway restores the deal's whole usage, and the resumed driver bills it by
    /// the same rule as a fresh one -- never on top of the probe tick the chain already holds, and never
    /// below what the chain already holds.
    #[tokio::test]
    async fn resumed_claim_is_the_same_whole_usage_rule_2185() {
        let cases: [(u128, u64, Vec<u128>); 5] = [
            (TICK_SIZE, 600_000, vec![]),
            (TICK_SIZE, TICK_SIZE as u64, vec![]),
            (TICK_SIZE, 1_500_000, vec![1_500_000]),
            (2 * TICK_SIZE, 2_280_217, vec![2_280_217]),
            (2 * TICK_SIZE, 0, vec![]),
        ];
        for (on_chain, usage, expected_claims) in cases {
            let backend = RecordingBackend::resumed(on_chain, on_chain);
            let claimed = drive_recording(&backend, instant(), 4, usage).await;
            assert_eq!(
                backend.claims(),
                expected_claims,
                "usage {usage} over {on_chain}"
            );
            assert_eq!(
                claimed,
                on_chain.max(u128::from(usage)),
                "usage {usage} over {on_chain}"
            );
            assert_eq!(backend.accepts.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn terminal_chain_fact_is_observed_before_ordinary_driver_returns() {
        let backend = RecordingBackend::closed();
        let observer = RecordingStateObserver::default();
        let claimed = drive_advance_with_observer(
            &backend,
            &"tc-terminal-observer".to_string(),
            &LocalNote::generate(),
            instant(),
            2,
            TICK_SIZE as u64,
            true,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            &observer,
        )
        .await
        .unwrap();

        assert_eq!(claimed, 0);
        assert_eq!(observer.terminals.load(Ordering::Relaxed), 1);
        assert!(observer.states.lock().unwrap().is_empty());
        assert_eq!(backend.accepts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn buyer_stop_interrupts_claim_wait() {
        let backend = Arc::new(RecordingBackend::new());
        let driver_backend = backend.clone();
        let driver = tokio::spawn(async move {
            drive_advance(
                driver_backend.as_ref(),
                &"tc-stop".to_string(),
                &LocalNote::generate(),
                AdvanceWindows {
                    claim_interval: Duration::from_secs(1_200),
                    seconds_per_tick: Duration::from_secs(1),
                    promote: Duration::ZERO,
                    probe: Duration::ZERO,
                },
                4,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new(TICK_SIZE as u64)),
                Arc::new(AtomicBool::new(false)),
            )
            .await
        });

        while backend.accepts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        backend.closed.store(true, Ordering::Release);
        tokio::time::advance(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
        tokio::task::yield_now().await;

        assert!(driver.is_finished());
        assert_eq!(driver.await.unwrap().unwrap(), TICK_SIZE);
        assert!(backend.claims().is_empty());
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
    }

    /// The real chain removes both getState and snapshot on STOP. Exercise both waits with a
    /// surviving receipt, and prove that disappearance alone (or a failed read) never closes them.
    async fn buyer_stop_wait_2191(first_usage: bool, receipt: bool) {
        let backend = Arc::new(if first_usage {
            RecordingBackend::new()
        } else {
            RecordingBackend::resumed(TICK_SIZE, TICK_SIZE)
        });
        backend.close_after_claim.store(true, Ordering::Relaxed);
        if receipt {
            backend
                .buyer_stop_receipt_after_reads
                .store(3, Ordering::Relaxed);
        }
        let observer = Arc::new(RecordingStateObserver::default());
        let driver_backend = backend.clone();
        let driver_observer = observer.clone();
        let driver = tokio::spawn(async move {
            drive_advance_with_observer(
                driver_backend.as_ref(),
                &"tc-buyer-stop-wait-2191".to_string(),
                &LocalNote::generate(),
                AdvanceWindows {
                    probe: Duration::ZERO,
                    ..AdvanceWindows::canonical()
                },
                4,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new(if first_usage {
                    0
                } else {
                    TICK_SIZE as u64
                })),
                Arc::new(AtomicBool::new(false)),
                driver_observer.as_ref(),
            )
            .await
        });

        while observer.states.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        // Let the zero probe sleep and the initial state reads reach the delivery wait.
        tokio::time::advance(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        assert!(!driver.is_finished());
        assert_eq!(
            backend.buyer_stop_receipt_reads.load(Ordering::Acquire),
            0,
            "a readable open snapshot must not trigger receipt polling"
        );
        backend.closed.store(true, Ordering::Release);
        assert!(backend
            .snapshot(&"tc-buyer-stop-wait-2191".to_string())
            .await
            .is_none());
        if receipt {
            // An indexing delay and a transient receipt error must also leave the wait open.
            backend
                .buyer_stop_receipt_errors_remaining
                .store(1, Ordering::Release);
            tokio::time::advance(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
            tokio::task::yield_now().await;
            assert!(
                !driver.is_finished(),
                "a receipt read error is not terminal proof"
            );
            tokio::time::advance(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
            tokio::task::yield_now().await;
            assert!(
                !driver.is_finished(),
                "an empty receipt read is not terminal proof"
            );
            tokio::time::advance(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
            tokio::task::yield_now().await;
            assert!(
                driver.is_finished(),
                "buyer STOP must interrupt the delivery wait"
            );
            assert_eq!(
                driver.await.unwrap().unwrap(),
                if first_usage { 0 } else { TICK_SIZE }
            );
        } else {
            tokio::time::advance(SELLER_TERMINAL_RECEIPT_TIMEOUT).await;
            tokio::task::yield_now().await;
            assert!(
                !driver.is_finished(),
                "a missing snapshot without a receipt must keep waiting"
            );
            driver.abort();
            assert!(driver.await.unwrap_err().is_cancelled());
        }
        assert!(backend.claims().is_empty());
        assert_eq!(backend.accepts.load(Ordering::Relaxed), 0);
        assert_eq!(backend.finalized.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn buyer_stop_interrupts_delivered_tokens_wait_2191() {
        buyer_stop_wait_2191(false, true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn buyer_stop_interrupts_first_billable_usage_wait_2191() {
        buyer_stop_wait_2191(true, true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn missing_contract_without_stop_receipt_keeps_both_waits_open_2191() {
        buyer_stop_wait_2191(false, false).await;
        buyer_stop_wait_2191(true, false).await;
    }

    /// The live case: delivery grows while the stream is still open. Every other test here hands the
    /// driver a `done` that is already set, so none of them exercises the branch a running seller
    /// actually sits in: waiting, with more requests still to come. A buyer who keeps sending after
    /// the probe must keep being billed.
    #[tokio::test(start_paused = true)]
    async fn delivery_after_the_probe_is_claimed_while_the_stream_stays_open() {
        let backend = Arc::new(RecordingBackend::new());
        let delivered = Arc::new(AtomicU64::new(TICK_SIZE as u64));
        let delivery_done = Arc::new(AtomicBool::new(false));
        let driver_backend = backend.clone();
        let driver_delivered = delivered.clone();
        let driver_done = delivery_done.clone();
        let driver = tokio::spawn(async move {
            drive_advance(
                driver_backend.as_ref(),
                &"tc-open-delivery".to_string(),
                &LocalNote::generate(),
                AdvanceWindows {
                    claim_interval: Duration::from_secs(1),
                    seconds_per_tick: Duration::from_secs(1),
                    promote: Duration::ZERO,
                    probe: Duration::ZERO,
                },
                4,
                TICK_SIZE as u64,
                true,
                driver_delivered,
                driver_done,
            )
            .await
        });

        while backend.accepts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            backend.claims().is_empty(),
            "the driver is waiting after billing only the delivered probe"
        );
        assert!(!delivery_done.load(Ordering::Acquire));

        delivered.store((3 * TICK_SIZE) as u64, Ordering::Release);
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }

        assert!(!delivery_done.load(Ordering::Acquire));
        assert!(
            !driver.is_finished(),
            "an open delivery stream keeps the claim driver running"
        );
        assert_eq!(
            backend.claims(),
            vec![2 * TICK_SIZE, 3 * TICK_SIZE],
            "two ticks delivered after the probe are two further cumulative claims"
        );

        delivery_done.store(true, Ordering::Release);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let claimed = driver.await.unwrap().unwrap();
        assert_eq!(claimed, 3 * TICK_SIZE);
    }

    /// R20-03: a short response must never walk the cumulative cursor below the already-accepted full probe.
    #[tokio::test]
    async fn response_below_tick_keeps_probe_seeded_high_water() {
        let b = RecordingBackend::new();
        let claimed = drive_recording(&b, instant(), 4, (TICK_SIZE - 1) as u64).await;
        assert_eq!(claimed, TICK_SIZE);
        assert!(
            b.claims().is_empty(),
            "no claim may regress below the probe"
        );
        assert_eq!(b.accepts.load(Ordering::Relaxed), 1);
        assert_eq!(b.finalized.load(Ordering::Relaxed), 0);
    }

    /// R20-03: exactly one delivered tick is the accepted probe itself, not a second delta claim.
    #[tokio::test]
    async fn exactly_one_tick_is_not_claimed_twice() {
        let b = RecordingBackend::new();
        let claimed = drive_recording(&b, instant(), 4, TICK_SIZE as u64).await;
        assert_eq!(claimed, TICK_SIZE);
        assert!(b.claims().is_empty(), "the probe must not be re-stated");
    }

    #[test]
    fn claim_is_one_tick_until_usage_passes_it_then_the_usage_2185() {
        let budget = 10 * TICK_SIZE;
        assert_eq!(claim_target(0, budget), TICK_SIZE);
        assert_eq!(
            claim_target(21_977, budget),
            TICK_SIZE,
            "usage below one tick is billed as the paid probe tick"
        );
        assert_eq!(claim_target(TICK_SIZE as u64, budget), TICK_SIZE);
        assert_eq!(
            claim_target(2_280_217, budget),
            2_280_217,
            "usage beyond one tick is billed as itself, not on top of the probe tick"
        );
        assert_eq!(claim_target(11 * TICK_SIZE as u64, budget), budget);
    }

    proptest::proptest! {
        /// invariant: the claim is `max(one tick, usage)` within the budget -- never the probe tick
        /// plus the usage, never below the paid tick, and never decreasing as usage grows.
        #[test]
        fn claim_never_bills_the_probe_tick_on_top_of_usage(
            usage in 0u64..=20 * TICK_SIZE as u64,
            more in 0u64..=TICK_SIZE as u64,
            ticks in 2u128..=10,
        ) {
            let budget = ticks * TICK_SIZE;
            let claim = claim_target(usage, budget);
            proptest::prop_assert_eq!(claim, u128::from(usage).max(TICK_SIZE).min(budget));
            proptest::prop_assert!(claim >= TICK_SIZE);
            proptest::prop_assert!(claim <= u128::from(usage).max(TICK_SIZE));
            proptest::prop_assert!(claim_target(usage + more, budget) >= claim);
        }
    }

    /// Consumption beyond the probe stays in raw tokens: a partial next tick is not rounded up.
    #[tokio::test]
    async fn claims_cumulative_raw_tokens_after_the_probe() {
        let b = RecordingBackend::new();
        let claimed = drive_recording(&b, instant(), 4, (TICK_SIZE + 5) as u64).await;
        assert_eq!(claimed, TICK_SIZE + 5);
        assert_eq!(b.claims(), vec![TICK_SIZE + 5]);
        assert_eq!(b.finalized.load(Ordering::Relaxed), 1);
    }

    /// R20-03 ceiling equality: a claim may land exactly on the funded ceiling, with no extra attempt.
    #[tokio::test]
    async fn exact_ceiling_is_claimed_once() {
        let b = RecordingBackend::new();
        let claimed = drive_recording(&b, instant(), 2, (2 * TICK_SIZE) as u64).await;
        assert_eq!(claimed, 2 * TICK_SIZE);
        assert_eq!(b.claims(), vec![2 * TICK_SIZE]);
    }

    /// Tokens delivered just before `delivery_done` must still be claimed: the driver re-reads `delivered`
    /// after observing `done`, so the last raw-token remainder is never dropped by the load/load race.
    #[tokio::test]
    async fn claims_tokens_delivered_up_to_done() {
        let b = RecordingBackend::new();
        let delivered = Arc::new(AtomicU64::new(TICK_SIZE as u64));
        let done = Arc::new(AtomicBool::new(false));
        delivered.store((TICK_SIZE + 7) as u64, Ordering::Release);
        done.store(true, Ordering::Release);
        let claimed = drive_advance(
            &b,
            &"tc".to_string(),
            &LocalNote::generate(),
            instant(),
            4,
            TICK_SIZE as u64,
            true,
            delivered,
            done,
        )
        .await
        .unwrap();
        assert_eq!(claimed, TICK_SIZE + 7);
        assert_eq!(b.claims(), vec![TICK_SIZE + 7]);
    }

    /// R20-03/R20-09: backlog is a sequence of cumulative one-tick-or-smaller claims, never one oversized
    /// delta.
    #[tokio::test]
    async fn backlog_over_one_tick_is_split_across_later_claims() {
        let b = RecordingBackend::new();
        let windows = AdvanceWindows {
            claim_interval: Duration::from_secs(1),
            seconds_per_tick: Duration::from_secs(1),
            promote: Duration::ZERO,
            probe: Duration::ZERO,
        };
        let claimed = drive_recording(&b, windows, 4, (3 * TICK_SIZE) as u64).await;
        assert_eq!(claimed, 3 * TICK_SIZE);
        assert_eq!(
            b.claims(),
            vec![2 * TICK_SIZE, 3 * TICK_SIZE],
            "the probe seeds one tick; each later cumulative delta is exactly one tick"
        );
    }

    /// Combined R20-03/R20-09 restart regression: an already-accepted high-water H is the local base, the
    /// restored whole usage H+N is the target, and even a backlog above one tick is submitted as
    /// one-tick-or-smaller cumulative increments.
    #[tokio::test]
    async fn restart_at_h_plus_delivered_n_preserves_the_per_call_cap() {
        let b = RecordingBackend::resumed(TICK_SIZE, 2 * TICK_SIZE);
        let claimed = drive_recording(&b, instant(), 4, (3 * TICK_SIZE + 17) as u64).await;
        assert_eq!(claimed, 3 * TICK_SIZE + 17);
        assert_eq!(b.claims(), vec![3 * TICK_SIZE, 3 * TICK_SIZE + 17]);
        assert_eq!(b.attempts(), b.claims());
        let mut previous = 2 * TICK_SIZE;
        for cumulative in b.attempts() {
            assert!(
                cumulative - previous <= MAX_CLAIM_DELTA,
                "restart claim delta exceeded the contract hard cap"
            );
            previous = cumulative;
        }
        assert_eq!(b.accepts.load(Ordering::Relaxed), 0);
    }

    /// Combined lost-response regression: the submitted call advances by one tick, while an authoritative
    /// H+k readback at exact delivery-target equality may move the local cursor farther without another
    /// oversized submission.
    #[tokio::test]
    async fn lost_claim_response_h_plus_k_keeps_the_submitted_delta_capped() {
        let b = RecordingBackend::resyncing(3 * TICK_SIZE);
        let claimed = drive_recording(&b, instant(), 4, (3 * TICK_SIZE) as u64).await;
        assert_eq!(claimed, 3 * TICK_SIZE);
        assert_eq!(
            b.attempts(),
            vec![2 * TICK_SIZE],
            "the only submitted delta is one tick above the probe-seeded high-water"
        );
        assert_eq!(b.attempts()[0] - TICK_SIZE, MAX_CLAIM_DELTA);
        assert!(
            b.claims().is_empty(),
            "the lost-response claim was reconciled, not recorded as a fresh success"
        );
    }

    /// A higher cursor beyond accepted delivery or the funded volume must fail before finalization.
    #[tokio::test]
    async fn higher_claim_readback_beyond_deal_bounds_fails_closed() {
        let funded_ticks = 4;
        for on_chain in [3 * TICK_SIZE, (funded_ticks + 1) * TICK_SIZE] {
            let b = RecordingBackend::resyncing(on_chain);
            let error = drive_advance(
                &b,
                &"tc".to_string(),
                &LocalNote::generate(),
                instant(),
                funded_ticks,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new((2 * TICK_SIZE) as u64)),
                Arc::new(AtomicBool::new(true)),
            )
            .await
            .expect_err("an on-chain cursor beyond the deal bounds must fail closed");

            assert!(
                error.to_string().contains("invalid claim resync"),
                "{error}"
            );
            assert!(b.claims().is_empty());
            assert_eq!(b.attempts(), vec![2 * TICK_SIZE]);
            assert_eq!(b.finalized.load(Ordering::Relaxed), 0);
        }
    }

    /// Boundary timing: a ready backlog is not submitted before the configured interval, while equality at
    /// the boundary is allowed. The canonical test above pins that interval to exactly 60 seconds; this uses
    /// a one-second injected deal bound to keep the focused suite fast.
    #[tokio::test]
    async fn claim_waits_until_interval_boundary() {
        let b = RecordingBackend::new();
        let windows = AdvanceWindows {
            claim_interval: Duration::from_secs(1),
            seconds_per_tick: Duration::from_secs(1),
            promote: Duration::ZERO,
            probe: Duration::ZERO,
        };
        let started = tokio::time::Instant::now();
        let claimed = drive_recording(&b, windows, 4, (2 * TICK_SIZE) as u64).await;
        assert_eq!(claimed, 2 * TICK_SIZE);
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "claim landed before MIN_CLAIM_INTERVAL"
        );
        assert_eq!(b.claims(), vec![2 * TICK_SIZE]);
    }
    #[tokio::test]
    async fn shared_budget_2091_exhausting_claim_waits_for_admitted_work() {
        struct PendingRequest {
            active: std::sync::atomic::AtomicU32,
            checked: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        }
        impl ClaimStateObserver for PendingRequest {
            fn observe(&self, _: &TokenContract, _: DealChainState) -> Result<(), ChainError> {
                Ok(())
            }
            fn active_requests(&self, _: &TokenContract) -> u32 {
                if let Some(tx) = self.checked.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                self.active.load(Ordering::Acquire)
            }
        }
        let backend = Arc::new(RecordingBackend::resumed(TICK_SIZE, TICK_SIZE));
        let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
        let pending = Arc::new(PendingRequest {
            active: std::sync::atomic::AtomicU32::new(1),
            checked: std::sync::Mutex::new(Some(checked_tx)),
        });
        let driver_backend = backend.clone();
        let driver_pending = pending.clone();
        let mut driver = tokio::spawn(async move {
            drive_advance_with_observer(
                driver_backend.as_ref(),
                &"tc-shared-drain".to_string(),
                &LocalNote::generate(),
                instant(),
                2,
                TICK_SIZE as u64,
                true,
                Arc::new(AtomicU64::new(2 * TICK_SIZE as u64)),
                Arc::new(AtomicBool::new(true)),
                driver_pending.as_ref(),
            )
            .await
        });
        tokio::select! {
            checked = checked_rx => checked.unwrap(),
            result = &mut driver => panic!("exhausting claim completed before the active request: {result:?}"),
        }
        assert!(backend.claims().is_empty());
        pending.active.store(0, Ordering::Release);
        assert_eq!(driver.await.unwrap().unwrap(), 2 * TICK_SIZE);
        assert_eq!(backend.claims(), vec![2 * TICK_SIZE]);
    }
}

#[cfg(test)]
#[path = "advance_1196_drift_tests.rs"]
mod issue_1196_drift_tests;
