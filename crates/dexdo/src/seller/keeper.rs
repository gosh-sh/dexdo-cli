//! Permissionless subscription settlement keeper.

//! The seller claim loop and this loop deliberately stay separate. Claims describe delivered tokens;
//! this keeper only promotes mature claim slots and advances the take-or-pay weekly clock. Every decision
//! is made from the current strict typed `TokenContract` getters, and every write is reconciled from a
//! later strict read. Local time is only a wake-up hint: the contract remains authoritative at the write.

use async_trait::async_trait;
use dexdo_core::{
    ChainBackend, ChainError, ClaimBounds, DealChainSnapshot, DealChainState, DealSubscription,
    TokenContract, SUB_WEEK_LEN, TICK_SIZE,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn display_token_contract(token_contract: &str) -> String {
    dexdo_core::address::display_self_dapp(token_contract)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeeperSnapshot {
    deal: DealChainState,
    subscription: DealSubscription,
}

impl From<&DealChainSnapshot> for KeeperSnapshot {
    fn from(snapshot: &DealChainSnapshot) -> Self {
        Self {
            deal: snapshot.state,
            subscription: snapshot.subscription,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeeperPlan {
    NotSubscription,
    Terminal,
    Wait(Duration),
    Finalize,
    SettleWeek(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeeperStep {
    NotSubscription,
    Terminal(u128),
    Wait(Duration),
    Progress,
}

#[derive(Default)]
struct KeeperCursor {
    last_accepted: Option<KeeperSnapshot>,
}

impl KeeperCursor {
    fn accept(
        &mut self,
        token_contract: &TokenContract,
        snapshot: KeeperSnapshot,
    ) -> Result<(), ChainError> {
        validate_snapshot(snapshot, token_contract)?;
        if let Some(previous) = self.last_accepted {
            ensure_non_regressing_transition(token_contract, previous, snapshot)?;
        }
        self.last_accepted = Some(snapshot);
        Ok(())
    }

    fn terminal_tokens_final(&self) -> Option<u128> {
        self.last_accepted
            .map(|snapshot| snapshot.deal.tokens_final)
    }
}

#[async_trait]
trait KeeperClock: Send + Sync {
    fn now_unix(&self) -> Result<u64, ChainError>;
    async fn sleep(&self, delay: Duration);
}

struct SystemClock;

#[async_trait]
impl KeeperClock for SystemClock {
    fn now_unix(&self) -> Result<u64, ChainError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|error| ChainError::Chain(format!("subscription keeper clock error: {error}")))
    }

    async fn sleep(&self, delay: Duration) {
        tokio::time::sleep(delay).await;
    }
}

/// Observes the keeper's existing authoritative reads without polling or submitting independently.

/// `Some(snapshot)` is emitted after every coherent `ChainBackend::deal_snapshot` accepted by the
/// keeper, including its scheduling, immediate pre-write and post-write reads. The same absolute
/// snapshot may be emitted more than once, so observers must reconcile idempotently. `None` is emitted
/// only after a live subscription was already established and coherent account absence therefore means
/// terminal destruction. A stopped `TokenContract` remains a terminal `Some(snapshot)`.

/// Returning an error stops the keeper before its next decision or write.
#[async_trait]
pub trait SubscriptionKeeperObserver: Send + Sync {
    async fn observe(
        &self,
        token_contract: &TokenContract,
        snapshot: Option<&DealChainSnapshot>,
    ) -> Result<(), ChainError>;
}

#[async_trait]
impl SubscriptionKeeperObserver for () {
    async fn observe(
        &self,
        _token_contract: &TokenContract,
        _snapshot: Option<&DealChainSnapshot>,
    ) -> Result<(), ChainError> {
        Ok(())
    }
}

/// Keep one real subscription moving until its `TokenContract` reaches a terminal state.

/// There is no journal and no local "sent" bit. A restart reads the exact current claim pipeline and week
/// index and continues from there. Dropping this future (the seller's graceful shutdown path) performs no
/// terminal action; it merely stops this local loop.
pub async fn drive_subscription_keeper(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    bounds: ClaimBounds,
) -> Result<u128, ChainError> {
    drive_subscription_keeper_with_observer(chain, token_contract, bounds, &()).await
}

/// Run the keeper with an observation-only consumer of its authoritative coherent snapshots.
pub async fn drive_subscription_keeper_with_observer(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    bounds: ClaimBounds,
    observer: &dyn SubscriptionKeeperObserver,
) -> Result<u128, ChainError> {
    drive_subscription_keeper_with_clock_and_observer(
        chain,
        token_contract,
        bounds,
        &SystemClock,
        observer,
    )
    .await
}

#[cfg(test)]
async fn drive_subscription_keeper_with_clock(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    bounds: ClaimBounds,
    clock: &dyn KeeperClock,
) -> Result<u128, ChainError> {
    drive_subscription_keeper_with_clock_and_observer(chain, token_contract, bounds, clock, &())
        .await
}

async fn drive_subscription_keeper_with_clock_and_observer(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    bounds: ClaimBounds,
    clock: &dyn KeeperClock,
    observer: &dyn SubscriptionKeeperObserver,
) -> Result<u128, ChainError> {
    let mut cursor = KeeperCursor::default();
    loop {
        match keeper_step(
            chain,
            token_contract,
            bounds,
            clock.now_unix()?,
            &mut cursor,
            observer,
        )
        .await?
        {
            KeeperStep::NotSubscription => {
                return Err(ChainError::Chain(format!(
                    "TokenContract {}: subscription keeper was started for an ordinary deal",
                    display_token_contract(token_contract)
                )));
            }
            KeeperStep::Terminal(tokens_final) => return Ok(tokens_final),
            KeeperStep::Progress => {}
            KeeperStep::Wait(delay) => clock.sleep(delay).await,
        }
    }
}

async fn keeper_step(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    bounds: ClaimBounds,
    now: u64,
    cursor: &mut KeeperCursor,
    observer: &dyn SubscriptionKeeperObserver,
) -> Result<KeeperStep, ChainError> {
    let Some(observed) = read_accepted_snapshot(chain, token_contract, cursor, observer).await?
    else {
        return if let Some(tokens_final) = cursor.terminal_tokens_final() {
            observer.observe(token_contract, None).await?;
            Ok(KeeperStep::Terminal(tokens_final))
        } else {
            Err(ChainError::Chain(format!(
                "TokenContract {}: keeper getters returned no data before a live subscription \
                 was established",
                display_token_contract(token_contract)
            )))
        };
    };

    match plan(observed, bounds, now, token_contract)? {
        KeeperPlan::NotSubscription => return Ok(KeeperStep::NotSubscription),
        KeeperPlan::Terminal => {
            return Ok(KeeperStep::Terminal(observed.deal.tokens_final));
        }
        KeeperPlan::Wait(delay) => {
            return Ok(KeeperStep::Wait(delay));
        }
        KeeperPlan::Finalize | KeeperPlan::SettleWeek(_) => {}
    }

    // The first read only scheduled the work. Re-read immediately before the write and let the contract's
    // own boundary decide if a concurrent claim/keeper/STOP changed the answer.
    let Some(pre_write) = read_accepted_snapshot(chain, token_contract, cursor, observer).await?
    else {
        observer.observe(token_contract, None).await?;
        return Ok(KeeperStep::Terminal(observed.deal.tokens_final));
    };
    let action = plan(pre_write, bounds, now, token_contract)?;
    match action {
        KeeperPlan::NotSubscription => return Ok(KeeperStep::NotSubscription),
        KeeperPlan::Terminal => {
            return Ok(KeeperStep::Terminal(pre_write.deal.tokens_final));
        }
        KeeperPlan::Wait(delay) => {
            return Ok(KeeperStep::Wait(delay));
        }
        KeeperPlan::Finalize | KeeperPlan::SettleWeek(_) => {}
    }

    let submit = match action {
        KeeperPlan::Finalize => chain.finalize(token_contract).await,
        KeeperPlan::SettleWeek(_) => chain.settle_week(token_contract).await,
        KeeperPlan::NotSubscription | KeeperPlan::Terminal | KeeperPlan::Wait(_) => unreachable!(),
    };

    // Reconcile even after an error: an HTTP response may be lost after the permissionless call applied.
    let post_write = read_accepted_snapshot(chain, token_contract, cursor, observer).await?;
    if post_write.is_none() {
        observer.observe(token_contract, None).await?;
    }
    let step = reconcile_write(
        token_contract,
        bounds,
        action,
        pre_write,
        post_write,
        submit,
    )?;
    Ok(step)
}

fn plan(
    snapshot: KeeperSnapshot,
    bounds: ClaimBounds,
    now: u64,
    token_contract: &TokenContract,
) -> Result<KeeperPlan, ChainError> {
    validate_snapshot(snapshot, token_contract)?;
    let deal = snapshot.deal;
    let subscription = snapshot.subscription;

    if !subscription.is_subscription() {
        return Ok(KeeperPlan::NotSubscription);
    }
    if deal.is_stopped() {
        return Ok(KeeperPlan::Terminal);
    }

    let idle = bounds.min_claim_interval;
    if idle.is_zero() {
        return Err(ChainError::Chain(format!(
            "TokenContract {}: getConfig().minClaimInterval is zero; refusing a busy keeper loop",
            display_token_contract(token_contract)
        )));
    }
    if !deal.opened || !deal.probe_accepted || deal.disputed {
        return Ok(KeeperPlan::Wait(idle));
    }

    let promote = bounds.promote_window;
    if promote.is_zero() {
        return Err(ChainError::Chain(format!(
            "TokenContract {}: derived CLAIM_PROMOTE_WINDOW is zero",
            display_token_contract(token_contract)
        )));
    }

    // Contracts 4.0.35 collapsed the claim conveyor to two slots, so there is one unpromoted claim
    // and one deadline instead of two. `CLAIM_PROMOTE_WINDOW == MIN_CLAIM_INTERVAL` now, which is
    // what makes a second unpromoted slot unreachable rather than merely unobserved -- and `promote`
    // above is that same number, derived in `ClaimBounds::from_config` from the rate floor rather
    // than from twice it, so this deadline is the one the chain itself promotes at.
    let last_deadline = (deal.tokens_pending > deal.tokens_final)
        .then(|| {
            checked_deadline(
                deal.last_claim_time,
                promote,
                token_contract,
                "lastClaimTime",
            )
        })
        .transpose()?;

    if last_deadline.is_some_and(|deadline| now >= deadline) {
        return Ok(KeeperPlan::Finalize);
    }

    let next_boundary = if subscription.week_index < subscription.sub_weeks {
        Some(next_week_boundary(subscription, token_contract)?)
    } else {
        None
    };
    if next_boundary.is_some_and(|deadline| now >= deadline) {
        return Ok(KeeperPlan::SettleWeek(scheduled_week_target(
            subscription,
            now,
            token_contract,
        )?));
    }

    // The first final-week call may charge week four but leave the deal open for the newest claim's window.
    // Once that window has elapsed and no promotion is left, the same permissionless call closes it.
    let final_close_deadline = (subscription.week_index == subscription.sub_weeks)
        .then(|| {
            checked_deadline(
                deal.last_claim_time,
                promote,
                token_contract,
                "lastClaimTime",
            )
        })
        .transpose()?;
    if final_close_deadline.is_some_and(|deadline| now >= deadline)
        && deal.tokens_final == deal.tokens_pending
    {
        return Ok(KeeperPlan::SettleWeek(subscription.sub_weeks));
    }

    let mut delay = idle;
    for deadline in [last_deadline, next_boundary, final_close_deadline]
        .into_iter()
        .flatten()
    {
        if deadline > now {
            delay = delay.min(Duration::from_secs(deadline - now));
        }
    }
    Ok(KeeperPlan::Wait(delay))
}

fn validate_snapshot(
    snapshot: KeeperSnapshot,
    token_contract: &TokenContract,
) -> Result<(), ChainError> {
    let deal = snapshot.deal;
    let subscription = snapshot.subscription;
    let malformed = |reason: String| {
        ChainError::Chain(format!(
            "TokenContract {}: malformed subscription keeper state: {reason}",
            display_token_contract(token_contract)
        ))
    };

    if deal.tokens_final > deal.tokens_pending {
        return Err(malformed(format!(
            "claim pipeline regressed (final={}, pending={})",
            deal.tokens_final, deal.tokens_pending
        )));
    }
    if deal.opened && !deal.funded {
        return Err(malformed("opened=true with funded=false".to_string()));
    }
    if subscription.is_subscription() {
        if subscription.week_index > subscription.sub_weeks {
            return Err(malformed(format!(
                "weekIndex {} exceeds subWeeks {}",
                subscription.week_index, subscription.sub_weeks
            )));
        }
        if deal.tokens_pending > subscription.funded_tokens {
            return Err(malformed(format!(
                "tokensPending {} exceeds fundedTokens {}",
                deal.tokens_pending, subscription.funded_tokens
            )));
        }
        if !deal.is_stopped() && subscription.week_base_tokens > deal.tokens_pending {
            return Err(malformed(format!(
                "weekBaseTokens {} exceeds tokensPending {}",
                subscription.week_base_tokens, deal.tokens_pending
            )));
        }
        if deal.opened && deal.probe_accepted {
            if deal.tokens_final < TICK_SIZE || subscription.tokens_paid < TICK_SIZE {
                return Err(malformed(format!(
                    "accepted subscription is below the canonical probe high-water \
                     (tokensFinal={}, tokensPaid={}, TICK_SIZE={TICK_SIZE})",
                    deal.tokens_final, subscription.tokens_paid
                )));
            }
            if subscription.period_start == 0 {
                return Err(malformed(
                    "accepted subscription has periodStart=0".to_string(),
                ));
            }
            if deal.last_claim_time == 0 {
                return Err(malformed(
                    "accepted subscription has lastClaimTime=0".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn checked_deadline(
    anchor: u64,
    duration: Duration,
    token_contract: &TokenContract,
    field: &str,
) -> Result<u64, ChainError> {
    anchor.checked_add(duration.as_secs()).ok_or_else(|| {
        ChainError::Chain(format!(
            "TokenContract {}: {field} + deadline overflows unix seconds",
            display_token_contract(token_contract)
        ))
    })
}

fn next_week_boundary(
    subscription: DealSubscription,
    token_contract: &TokenContract,
) -> Result<u64, ChainError> {
    let next_week = u64::from(subscription.week_index)
        .checked_add(1)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "TokenContract {}: weekIndex increment overflow",
                display_token_contract(token_contract)
            ))
        })?;
    let offset = SUB_WEEK_LEN
        .as_secs()
        .checked_mul(next_week)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "TokenContract {}: subscription week offset overflow",
                display_token_contract(token_contract)
            ))
        })?;
    subscription
        .period_start
        .checked_add(offset)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "TokenContract {}: periodStart + subscription week offset overflows",
                display_token_contract(token_contract)
            ))
        })
}

fn scheduled_week_target(
    subscription: DealSubscription,
    now: u64,
    token_contract: &TokenContract,
) -> Result<u8, ChainError> {
    let elapsed = now.checked_sub(subscription.period_start).ok_or_else(|| {
        ChainError::Chain(format!(
            "TokenContract {}: current time precedes subscription periodStart",
            display_token_contract(token_contract)
        ))
    })?;
    let elapsed_weeks = elapsed / SUB_WEEK_LEN.as_secs();
    let target = elapsed_weeks.min(u64::from(subscription.sub_weeks));
    let target = u8::try_from(target).map_err(|_| {
        ChainError::Chain(format!(
            "TokenContract {}: scheduled subscription week target does not fit uint8",
            display_token_contract(token_contract)
        ))
    })?;
    if target <= subscription.week_index {
        return Err(ChainError::Chain(format!(
            "TokenContract {}: due settleWeek did not advance its scheduled target \
             beyond current weekIndex {}",
            display_token_contract(token_contract),
            subscription.week_index
        )));
    }
    Ok(target)
}

async fn read_accepted_snapshot(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    cursor: &mut KeeperCursor,
    observer: &dyn SubscriptionKeeperObserver,
) -> Result<Option<KeeperSnapshot>, ChainError> {
    let snapshot = chain.deal_snapshot(token_contract).await?;
    if let Some(snapshot) = snapshot.as_ref() {
        let keeper_snapshot = KeeperSnapshot::from(snapshot);
        cursor.accept(token_contract, keeper_snapshot)?;
        observer.observe(token_contract, Some(snapshot)).await?;
        return Ok(Some(keeper_snapshot));
    }
    Ok(None)
}

fn reconcile_write(
    token_contract: &TokenContract,
    bounds: ClaimBounds,
    action: KeeperPlan,
    before: KeeperSnapshot,
    after: Option<KeeperSnapshot>,
    submit: Result<(), ChainError>,
) -> Result<KeeperStep, ChainError> {
    let Some(after) = after else {
        // Missing after a submitted transition is the normal final self-destruct shape. Replaying the
        // value-moving weekly call would be strictly worse than accepting the terminal chain fact.
        return Ok(KeeperStep::Terminal(before.deal.tokens_final));
    };
    validate_snapshot(after, token_contract)?;

    if after.deal.is_stopped() {
        return Ok(KeeperStep::Terminal(after.deal.tokens_final));
    }
    if after.deal.disputed {
        // A buyer dispute won the race. The keeper never retries either permissionless action while disputed.
        return Ok(KeeperStep::Wait(bounds.min_claim_interval));
    }
    ensure_non_regressing_transition(token_contract, before, after)?;

    let applied = match action {
        KeeperPlan::Finalize => after.deal.tokens_final > before.deal.tokens_final,
        KeeperPlan::SettleWeek(target)
            if before.subscription.week_index < before.subscription.sub_weeks =>
        {
            after.subscription.week_index == target
        }
        KeeperPlan::SettleWeek(_) => false, // post-term retry succeeds only by closing, handled above.
        KeeperPlan::NotSubscription | KeeperPlan::Terminal | KeeperPlan::Wait(_) => false,
    };
    if applied {
        return Ok(KeeperStep::Progress);
    }

    if let KeeperPlan::SettleWeek(target) = action {
        let submit = submit
            .err()
            .map(|error| format!("; submit error: {error}"))
            .unwrap_or_default();
        return Err(ChainError::Chain(format!(
            "TokenContract {}: settleWeek expected exact scheduled weekIndex \
             {target} or terminal state, observed weekIndex {}{submit}",
            display_token_contract(token_contract),
            after.subscription.week_index
        )));
    }

    if let Err(error) = submit {
        return Err(ChainError::Chain(format!(
            "TokenContract {}: {action:?} failed and strict state did not advance: {error}",
            display_token_contract(token_contract)
        )));
    }
    Err(ChainError::Chain(format!(
        "TokenContract {}: {action:?} returned success without a strict state transition",
        display_token_contract(token_contract)
    )))
}

fn ensure_non_regressing_transition(
    token_contract: &TokenContract,
    before: KeeperSnapshot,
    after: KeeperSnapshot,
) -> Result<(), ChainError> {
    let terminal_transition = !before.deal.is_stopped() && after.deal.is_stopped();
    let accepted_probe_now = !before.deal.probe_accepted && after.deal.probe_accepted;
    if after.deal.tokens_final < before.deal.tokens_final
        || (!terminal_transition && after.deal.tokens_pending < before.deal.tokens_pending)
        || after.deal.last_claim_time < before.deal.last_claim_time
        || (before.deal.probe_accepted && !after.deal.probe_accepted)
        || after.subscription.week_index < before.subscription.week_index
        || after.subscription.tokens_paid < before.subscription.tokens_paid
        || after.subscription.week_base_tokens < before.subscription.week_base_tokens
        || after.subscription.period_start < before.subscription.period_start
    {
        return Err(ChainError::Chain(format!(
            "TokenContract {}: keeper observed regressing accepted state",
            display_token_contract(token_contract)
        )));
    }
    for (name, unchanged) in [
        (
            "dealFlags",
            after.subscription.deal_flags == before.subscription.deal_flags,
        ),
        (
            "subWeeks",
            after.subscription.sub_weeks == before.subscription.sub_weeks,
        ),
        (
            "tokensPerWeek",
            after.subscription.tokens_per_week == before.subscription.tokens_per_week,
        ),
        (
            "fundedTokens",
            after.subscription.funded_tokens == before.subscription.funded_tokens,
        ),
    ] {
        if !unchanged {
            return Err(ChainError::Chain(format!(
                "TokenContract {}: immutable subscription field {name} changed after keeper write",
                display_token_contract(token_contract)
            )));
        }
    }
    if after.subscription.period_start != before.subscription.period_start && !accepted_probe_now {
        return Err(ChainError::Chain(format!(
            "TokenContract {}: immutable subscription field periodStart changed after keeper write",
            display_token_contract(token_contract)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{
        DealBuyerBond, DealSellerBond, LocalNote, Match, Note, OfferListing, SellOffer, Settlement,
        StreamSnapshot, SUBSCRIPTION_WEEKS,
    };
    use proptest::prelude::*;
    use std::collections::VecDeque;
    use std::future::pending;
    use std::sync::{Arc, Mutex};

    const TC: &str = "0:keeper";
    const START: u64 = 1_000;
    const WINDOW: u64 = 10;

    fn bounds() -> ClaimBounds {
        ClaimBounds {
            min_claim_interval: Duration::from_secs(2),
            min_seconds_per_tick: Duration::from_secs(5),
            promote_window: Duration::from_secs(WINDOW),
            probe_window: Duration::from_secs(3),
            dispute_window: Duration::from_secs(20),
        }
    }

    fn deal(final_ticks: u128, pending_ticks: u128) -> DealChainState {
        DealChainState {
            funded: true,
            opened: true,
            probe_accepted: true,
            disputed: false,
            deposit: 1_000_000,
            finalized_owed: 0,
            tokens_final: final_ticks * TICK_SIZE,
            tokens_pending: pending_ticks * TICK_SIZE,
            probe_tick: 0,
            funded_time: Some(900),
            probe_time: 950,
            last_claim_time: START,
            dispute_time: 0,
        }
    }

    fn subscription(week_index: u8) -> DealSubscription {
        DealSubscription {
            deal_flags: dexdo_core::order_flags::SUBSCRIPTION,
            sub_weeks: SUBSCRIPTION_WEEKS,
            week_index,
            tokens_per_week: 10 * TICK_SIZE,
            funded_tokens: u128::from(SUBSCRIPTION_WEEKS) * 10 * TICK_SIZE,
            tokens_paid: if week_index == 0 {
                TICK_SIZE
            } else {
                u128::from(week_index) * 10 * TICK_SIZE
            },
            period_start: START,
            week_base_tokens: 0,
        }
    }

    fn snapshot(final_ticks: u128, pending_ticks: u128, week_index: u8) -> KeeperSnapshot {
        KeeperSnapshot {
            deal: deal(final_ticks, pending_ticks),
            subscription: subscription(week_index),
        }
    }

    fn authoritative_snapshot(snapshot: KeeperSnapshot) -> DealChainSnapshot {
        DealChainSnapshot {
            account_code_hash: "keeper-code".to_string(),
            account_boc_hash: "keeper-state".to_string(),
            state: snapshot.deal,
            subscription: snapshot.subscription,
            seller_bond: DealSellerBond {
                bond_funded: true,
                bond_held: 1,
                bond_required: 1,
            },
            buyer_bond: DealBuyerBond {
                bond_held: 1,
                bond_required: 1,
            },
        }
    }

    #[derive(Debug)]
    enum Effect {
        Replace(Option<KeeperSnapshot>),
        ReplaceAndLoseResponse(Option<KeeperSnapshot>),
        NoTransition,
        Fail,
    }

    struct BackendState {
        current: Option<KeeperSnapshot>,
        finalize: VecDeque<Effect>,
        settle: VecDeque<Effect>,
        finalize_calls: usize,
        settle_calls: usize,
        snapshot_read_calls: usize,
        snapshot_reads: VecDeque<Option<KeeperSnapshot>>,
    }

    struct ScriptedBackend {
        state: Mutex<BackendState>,
    }

    impl ScriptedBackend {
        fn new(current: KeeperSnapshot) -> Self {
            Self {
                state: Mutex::new(BackendState {
                    current: Some(current),
                    finalize: VecDeque::new(),
                    settle: VecDeque::new(),
                    finalize_calls: 0,
                    settle_calls: 0,
                    snapshot_read_calls: 0,
                    snapshot_reads: VecDeque::new(),
                }),
            }
        }

        fn push_finalize(&self, effect: Effect) {
            self.state.lock().unwrap().finalize.push_back(effect);
        }

        fn push_settle(&self, effect: Effect) {
            self.state.lock().unwrap().settle.push_back(effect);
        }

        fn set_current(&self, snapshot: Option<KeeperSnapshot>) {
            self.state.lock().unwrap().current = snapshot;
        }

        fn push_snapshot_reads(&self, values: impl IntoIterator<Item = Option<KeeperSnapshot>>) {
            self.state.lock().unwrap().snapshot_reads.extend(values);
        }

        fn calls(&self) -> (usize, usize) {
            let state = self.state.lock().unwrap();
            (state.finalize_calls, state.settle_calls)
        }

        fn reads(&self) -> usize {
            self.state.lock().unwrap().snapshot_read_calls
        }

        fn apply(
            queue: &mut VecDeque<Effect>,
            current: &mut Option<KeeperSnapshot>,
        ) -> Result<(), ChainError> {
            match queue.pop_front().unwrap_or(Effect::NoTransition) {
                Effect::Replace(next) => {
                    *current = next;
                    Ok(())
                }
                Effect::ReplaceAndLoseResponse(next) => {
                    *current = next;
                    Err(ChainError::AmbiguousSubmit("lost response".to_string()))
                }
                Effect::NoTransition => Ok(()),
                Effect::Fail => Err(ChainError::Contract("contract refused".to_string())),
            }
        }
    }

    #[async_trait]
    impl ChainBackend for ScriptedBackend {
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
            Ok(())
        }

        async fn claim_tokens(
            &self,
            _: &TokenContract,
            _: &dyn Note,
            cumulative: u128,
        ) -> Result<(), ChainError> {
            self.state
                .lock()
                .unwrap()
                .current
                .as_mut()
                .expect("claim requires an active scripted deal")
                .deal
                .tokens_pending = cumulative;
            Ok(())
        }

        async fn finalize(&self, _: &TokenContract) -> Result<(), ChainError> {
            let mut state = self.state.lock().unwrap();
            state.finalize_calls += 1;
            let BackendState {
                current, finalize, ..
            } = &mut *state;
            Self::apply(finalize, current)
        }

        async fn settle_week(&self, _: &TokenContract) -> Result<(), ChainError> {
            let mut state = self.state.lock().unwrap();
            state.settle_calls += 1;
            let BackendState {
                current, settle, ..
            } = &mut *state;
            Self::apply(settle, current)
        }

        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }

        async fn deal_snapshot(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainSnapshot>, ChainError> {
            let mut state = self.state.lock().unwrap();
            state.snapshot_read_calls += 1;
            if let Some(value) = state.snapshot_reads.pop_front() {
                return Ok(value.map(authoritative_snapshot));
            }
            Ok(state.current.map(authoritative_snapshot))
        }

        async fn deal_state(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .current
                .map(|snapshot| snapshot.deal))
        }

        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            None
        }
    }

    struct ManualClock {
        now: u64,
    }

    #[async_trait]
    impl KeeperClock for ManualClock {
        fn now_unix(&self) -> Result<u64, ChainError> {
            Ok(self.now)
        }

        async fn sleep(&self, _: Duration) {}
    }

    struct BlockingClock {
        now: u64,
    }

    #[async_trait]
    impl KeeperClock for BlockingClock {
        fn now_unix(&self) -> Result<u64, ChainError> {
            Ok(self.now)
        }

        async fn sleep(&self, _: Duration) {
            pending::<()>().await;
        }
    }

    type Observation = Option<(bool, u128, u8)>;

    struct RecordingObserver {
        observations: Mutex<Vec<Observation>>,
        fail_on: Option<usize>,
    }

    impl RecordingObserver {
        fn recording() -> Self {
            Self {
                observations: Mutex::new(Vec::new()),
                fail_on: None,
            }
        }

        fn failing_on(ordinal: usize) -> Self {
            Self {
                observations: Mutex::new(Vec::new()),
                fail_on: Some(ordinal),
            }
        }

        fn observations(&self) -> Vec<Observation> {
            self.observations.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SubscriptionKeeperObserver for RecordingObserver {
        async fn observe(
            &self,
            _: &TokenContract,
            snapshot: Option<&DealChainSnapshot>,
        ) -> Result<(), ChainError> {
            let observation = snapshot.map(|snapshot| {
                (
                    snapshot.state.probe_accepted,
                    snapshot.state.tokens_pending,
                    snapshot.subscription.week_index,
                )
            });
            let mut observations = self.observations.lock().unwrap();
            observations.push(observation);
            if self.fail_on == Some(observations.len()) {
                return Err(ChainError::Chain(
                    "subscription keeper observer failed".to_string(),
                ));
            }
            Ok(())
        }
    }

    async fn step(backend: &ScriptedBackend, now: u64) -> Result<KeeperStep, ChainError> {
        step_with_observer(backend, now, &()).await
    }

    async fn step_with_observer(
        backend: &ScriptedBackend,
        now: u64,
        observer: &dyn SubscriptionKeeperObserver,
    ) -> Result<KeeperStep, ChainError> {
        let mut cursor = KeeperCursor::default();
        step_with_cursor(backend, now, &mut cursor, observer).await
    }

    async fn step_with_cursor(
        backend: &ScriptedBackend,
        now: u64,
        cursor: &mut KeeperCursor,
        observer: &dyn SubscriptionKeeperObserver,
    ) -> Result<KeeperStep, ChainError> {
        keeper_step(backend, &TC.to_string(), bounds(), now, cursor, observer).await
    }

    #[tokio::test]
    async fn observer_receives_wait_pre_post_and_terminal_snapshots_from_the_keeper_reads() {
        let mut probing = snapshot(1, 1, 0);
        probing.deal.probe_accepted = false;
        let waiting = ScriptedBackend::new(probing);
        let waiting_observer = RecordingObserver::recording();
        assert!(matches!(
            step_with_observer(&waiting, START, &waiting_observer)
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        let mut accepted = probing;
        accepted.deal.probe_accepted = true;
        accepted.deal.tokens_pending = 2 * TICK_SIZE;
        waiting.set_current(Some(accepted));
        assert!(matches!(
            step_with_observer(&waiting, START + 6, &waiting_observer)
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(
            waiting_observer.observations(),
            vec![Some((false, TICK_SIZE, 0)), Some((true, 2 * TICK_SIZE, 0)),]
        );

        let before = snapshot(1, 1, 0);
        let mut after = before;
        after.subscription.week_index = 1;
        after.subscription.tokens_paid = after.subscription.tokens_per_week;
        let backend = ScriptedBackend::new(before);
        backend.push_settle(Effect::Replace(Some(after)));
        let observer = RecordingObserver::recording();

        assert_eq!(
            step_with_observer(&backend, START + SUB_WEEK_LEN.as_secs(), &observer)
                .await
                .unwrap(),
            KeeperStep::Progress
        );
        assert_eq!(
            observer.observations(),
            vec![
                Some((true, TICK_SIZE, 0)),
                Some((true, TICK_SIZE, 0)),
                Some((true, TICK_SIZE, 1)),
            ]
        );
        assert_eq!(backend.reads(), 3);

        let terminal = ScriptedBackend::new(snapshot(2, 2, SUBSCRIPTION_WEEKS));
        terminal.push_settle(Effect::Replace(None));
        let terminal_observer = RecordingObserver::recording();
        assert!(matches!(
            step_with_observer(
                &terminal,
                START + u64::from(SUBSCRIPTION_WEEKS) * SUB_WEEK_LEN.as_secs() + WINDOW,
                &terminal_observer,
            )
            .await
            .unwrap(),
            KeeperStep::Terminal(_)
        ));
        assert_eq!(
            terminal_observer.observations(),
            vec![
                Some((true, 2 * TICK_SIZE, SUBSCRIPTION_WEEKS)),
                Some((true, 2 * TICK_SIZE, SUBSCRIPTION_WEEKS)),
                None,
            ]
        );
        assert_eq!(terminal.calls(), (0, 1));
    }

    #[tokio::test]
    async fn probe_acceptance_reanchors_period_start_once_and_then_freezes_it() {
        let mut funded = snapshot(1, 1, 0);
        funded.deal.probe_accepted = false;
        funded.deal.tokens_final = 0;
        funded.deal.tokens_pending = 0;
        funded.deal.probe_tick = 1;
        funded.deal.last_claim_time = 900;
        funded.subscription.tokens_paid = 0;
        funded.subscription.period_start = 900;

        let backend = ScriptedBackend::new(funded);
        let mut cursor = KeeperCursor::default();
        assert!(matches!(
            step_with_cursor(&backend, 901, &mut cursor, &())
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));

        let mut accepted = funded;
        accepted.deal.probe_accepted = true;
        accepted.deal.probe_tick = 0;
        accepted.deal.tokens_final = TICK_SIZE;
        accepted.deal.tokens_pending = TICK_SIZE;
        accepted.deal.last_claim_time = START;
        accepted.subscription.tokens_paid = TICK_SIZE;
        accepted.subscription.period_start = START;
        backend.set_current(Some(accepted));
        assert!(matches!(
            step_with_cursor(&backend, START + 1, &mut cursor, &())
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));

        let mut changed_again = accepted;
        changed_again.subscription.period_start += 1;
        backend.set_current(Some(changed_again));
        let error = step_with_cursor(&backend, START + 1, &mut cursor, &())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("immutable subscription field periodStart"),
            "{error}"
        );
        assert_eq!(backend.calls(), (0, 0));

        let mut regressing_acceptance = accepted;
        regressing_acceptance.subscription.period_start = funded.subscription.period_start - 1;
        let mut regressing_cursor = KeeperCursor::default();
        regressing_cursor.accept(&TC.to_string(), funded).unwrap();
        let error = regressing_cursor
            .accept(&TC.to_string(), regressing_acceptance)
            .unwrap_err()
            .to_string();
        assert!(error.contains("regressing accepted state"), "{error}");
    }

    #[tokio::test]
    async fn terminal_dispute_resolution_may_collapse_claim_slots_below_week_high_water() {
        let mut boundary = snapshot(1, 3, 1);
        boundary.subscription.week_base_tokens = 3 * TICK_SIZE;
        let backend = ScriptedBackend::new(boundary);
        let mut cursor = KeeperCursor::default();
        assert!(matches!(
            step_with_cursor(&backend, START + 1, &mut cursor, &())
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));

        let mut disputed = boundary;
        disputed.deal.disputed = true;
        disputed.deal.dispute_time = START + 1;
        backend.set_current(Some(disputed));
        assert!(matches!(
            step_with_cursor(&backend, START + 2, &mut cursor, &())
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));

        let mut resolved = disputed;
        resolved.deal.opened = false;
        resolved.deal.disputed = false;
        resolved.deal.deposit = 0;
        resolved.deal.probe_tick = 0;
        resolved.deal.tokens_pending = resolved.deal.tokens_final;
        backend.set_current(Some(resolved));
        assert!(matches!(
            step_with_cursor(&backend, START + 3, &mut cursor, &())
                .await
                .unwrap(),
            KeeperStep::Terminal(value) if value == TICK_SIZE
        ));
        assert_eq!(backend.calls(), (0, 0));

        let active_backend = ScriptedBackend::new(boundary);
        let mut active_cursor = KeeperCursor::default();
        assert!(matches!(
            step_with_cursor(&active_backend, START + 1, &mut active_cursor, &())
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        let mut active_collapse = boundary;
        active_collapse.deal.tokens_pending = active_collapse.deal.tokens_final;
        active_backend.set_current(Some(active_collapse));
        let error = step_with_cursor(&active_backend, START + 2, &mut active_cursor, &())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("weekBaseTokens"), "{error}");
        assert_eq!(active_backend.calls(), (0, 0));

        let mut regressed_money = resolved;
        regressed_money.subscription.tokens_paid -= 1;
        let mut terminal_cursor = KeeperCursor::default();
        terminal_cursor.accept(&TC.to_string(), disputed).unwrap();
        let error = terminal_cursor
            .accept(&TC.to_string(), regressed_money)
            .unwrap_err()
            .to_string();
        assert!(error.contains("regressing accepted state"), "{error}");
    }

    #[tokio::test]
    async fn observer_failure_before_a_write_is_fail_closed() {
        for ordinal in [1, 2] {
            let backend = ScriptedBackend::new(snapshot(1, 1, 0));
            let observer = RecordingObserver::failing_on(ordinal);
            let error = step_with_observer(&backend, START + SUB_WEEK_LEN.as_secs(), &observer)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("observer failed"), "{error}");
            assert_eq!(backend.calls(), (0, 0));
            assert_eq!(backend.reads(), ordinal);
        }
    }

    #[tokio::test]
    async fn observer_failure_after_an_applied_write_propagates_without_replay() {
        let before = snapshot(1, 1, 0);
        let mut after = before;
        after.subscription.week_index = 1;
        after.subscription.tokens_paid = after.subscription.tokens_per_week;
        let backend = ScriptedBackend::new(before);
        backend.push_settle(Effect::Replace(Some(after)));
        let observer = RecordingObserver::failing_on(3);

        let error = step_with_observer(&backend, START + SUB_WEEK_LEN.as_secs(), &observer)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("observer failed"), "{error}");
        assert_eq!(backend.calls(), (0, 1));

        assert!(matches!(
            step(&backend, START + 2 * SUB_WEEK_LEN.as_secs() - 1)
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(
            backend.calls(),
            (0, 1),
            "restart truth comes from the advanced authoritative snapshot"
        );
    }

    #[tokio::test]
    async fn malformed_snapshot_is_rejected_before_observer_or_write() {
        let backend = ScriptedBackend::new(snapshot(3, 2, 0));
        let observer = RecordingObserver::recording();
        let error = step_with_observer(&backend, START + WINDOW, &observer)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("claim pipeline regressed"), "{error}");
        assert!(observer.observations().is_empty());
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn scheduling_to_prewrite_rollback_is_rejected_before_observer_or_write() {
        let scheduled = snapshot(2, 2, 1);
        let mut regressed = scheduled;
        regressed.subscription.week_index = 0;
        regressed.subscription.tokens_paid = TICK_SIZE;
        let backend = ScriptedBackend::new(scheduled);
        backend.push_snapshot_reads([Some(scheduled), Some(regressed)]);
        let observer = RecordingObserver::recording();
        let mut cursor = KeeperCursor::default();

        let error = step_with_cursor(
            &backend,
            START + 2 * SUB_WEEK_LEN.as_secs(),
            &mut cursor,
            &observer,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("regressing accepted state"), "{error}");
        assert_eq!(
            observer.observations(),
            vec![Some((true, 2 * TICK_SIZE, 1))],
            "the regressing coherent snapshot is never published"
        );
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn loop_to_loop_high_water_rollbacks_never_submit_a_second_write() {
        for regression in ["claim", "claim_time", "probe", "week", "immutable"] {
            let before = snapshot(2, 2, 0);
            let mut advanced = before;
            advanced.subscription.week_index = 1;
            advanced.subscription.tokens_paid = advanced.subscription.tokens_per_week;
            let backend = ScriptedBackend::new(before);
            backend.push_settle(Effect::Replace(Some(advanced)));
            let observer = RecordingObserver::recording();
            let mut cursor = KeeperCursor::default();

            assert_eq!(
                step_with_cursor(
                    &backend,
                    START + SUB_WEEK_LEN.as_secs(),
                    &mut cursor,
                    &observer,
                )
                .await
                .unwrap(),
                KeeperStep::Progress
            );
            assert_eq!(backend.calls(), (0, 1));
            assert_eq!(observer.observations().len(), 3);

            let mut regressed = advanced;
            match regression {
                "claim" => {
                    regressed.deal.tokens_final = TICK_SIZE;
                    regressed.deal.tokens_pending = TICK_SIZE;
                }
                "claim_time" => {
                    regressed.deal.last_claim_time -= 1;
                }
                "probe" => {
                    regressed.deal.probe_accepted = false;
                }
                "week" => {
                    regressed.subscription.week_index = 0;
                    regressed.subscription.tokens_paid = TICK_SIZE;
                }
                "immutable" => {
                    regressed.subscription.tokens_per_week += TICK_SIZE;
                }
                _ => unreachable!(),
            }
            backend.set_current(Some(regressed));

            let error = step_with_cursor(
                &backend,
                START + 2 * SUB_WEEK_LEN.as_secs(),
                &mut cursor,
                &observer,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("regressing accepted state")
                    || error.contains("immutable subscription field"),
                "{regression}: {error}"
            );
            assert_eq!(
                observer.observations().len(),
                3,
                "{regression}: rejected snapshots are not observed"
            );
            assert_eq!(
                backend.calls(),
                (0, 1),
                "{regression}: no second write may follow an accepted-state rollback"
            );
        }
    }

    #[tokio::test]
    async fn subscription_advance_delegates_the_only_finalize_submit_to_the_keeper() {
        let before = snapshot(1, 2, 0);
        let backend = ScriptedBackend::new(before);
        let mut after = before;
        after.deal.tokens_final = 2 * TICK_SIZE;
        after.deal.tokens_pending = 3 * TICK_SIZE;
        backend.push_finalize(Effect::Replace(Some(after)));

        let claimed = super::super::advance::drive_advance(
            &backend,
            &TC.to_string(),
            &LocalNote::generate(),
            super::super::advance::AdvanceWindows {
                claim_interval: Duration::ZERO,
                seconds_per_tick: Duration::from_secs(5),
                promote: Duration::ZERO,
                probe: Duration::ZERO,
            },
            4,
            TICK_SIZE as u64,
            false,
            Arc::new(std::sync::atomic::AtomicU64::new(TICK_SIZE as u64)),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        )
        .await
        .unwrap();
        assert_eq!(claimed, 3 * TICK_SIZE);
        assert_eq!(
            backend.calls(),
            (0, 0),
            "subscription advance must not race the keeper's finalize writer"
        );

        assert_eq!(
            step(&backend, START + WINDOW).await.unwrap(),
            KeeperStep::Progress
        );
        assert_eq!(
            backend.calls(),
            (1, 0),
            "the shared backend observes exactly one finalize submit"
        );
    }

    #[tokio::test]
    async fn the_claim_slot_obeys_window_minus_one_exact_and_plus_one() {
        for (name, state, anchor) in [("newest", snapshot(1, 2, 0), START)] {
            for (offset, due) in [(WINDOW - 1, false), (WINDOW, true), (WINDOW + 1, true)] {
                let backend = ScriptedBackend::new(state);
                if due {
                    let mut promoted = state;
                    promoted.deal.tokens_final = 2 * TICK_SIZE;
                    backend.push_finalize(Effect::Replace(Some(promoted)));
                }
                let result = step(&backend, anchor + offset).await.expect(name);
                assert_eq!(
                    backend.calls().0,
                    usize::from(due),
                    "{name} offset={offset}"
                );
                assert_eq!(matches!(result, KeeperStep::Progress), due);
            }
        }
    }

    #[tokio::test]
    async fn a_promotion_confirms_before_the_next_claim_is_due() {
        let mut before = snapshot(1, 3, 0);
        before.deal.last_claim_time = START;

        // TokenContract 4.0.35 promotes the one pending slot whole (`_tokensFinal <- _tokensPend`),
        // and a claim landing in the same moment re-anchors `lastClaimTime` and starts its OWN
        // window. Full `tokensPending` equality is deliberately not expected until it serves it.
        let mut after = before;
        after.deal.tokens_final = 3 * TICK_SIZE;
        after.deal.tokens_pending = 4 * TICK_SIZE;
        after.deal.last_claim_time = START + WINDOW;

        let backend = ScriptedBackend::new(before);
        backend.push_finalize(Effect::Replace(Some(after)));
        assert_eq!(
            step(&backend, START + WINDOW).await.unwrap(),
            KeeperStep::Progress
        );
        assert_eq!(backend.calls(), (1, 0));

        assert!(matches!(
            step(&backend, START + 2 * WINDOW - 1).await.unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(
            backend.calls(),
            (1, 0),
            "the still-contestable newest slot must not be replayed early"
        );
    }

    #[tokio::test]
    async fn lost_finalize_response_is_accepted_only_from_strict_promotion() {
        let before = snapshot(1, 2, 0);
        let mut after = before;
        after.deal.tokens_final = 2 * TICK_SIZE;
        let backend = ScriptedBackend::new(before);
        backend.push_finalize(Effect::ReplaceAndLoseResponse(Some(after)));

        assert_eq!(
            step(&backend, START + WINDOW).await.unwrap(),
            KeeperStep::Progress
        );
        assert_eq!(backend.calls(), (1, 0));
        assert!(matches!(
            step(&backend, START + WINDOW).await.unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(
            backend.calls(),
            (1, 0),
            "reconciled promotion is never replayed"
        );
    }

    #[tokio::test]
    async fn zero_delta_and_success_without_transition_send_nothing_or_fail_closed() {
        let equal = ScriptedBackend::new(snapshot(2, 2, 0));
        assert!(matches!(
            step(&equal, START + WINDOW).await.unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(equal.calls(), (0, 0));

        let due = ScriptedBackend::new(snapshot(1, 2, 0));
        due.push_finalize(Effect::Fail);
        let error = step(&due, START + WINDOW).await.unwrap_err().to_string();
        assert!(
            error.contains("failed and strict state did not advance"),
            "{error}"
        );
        assert_eq!(due.calls(), (1, 0));

        let false_success = ScriptedBackend::new(snapshot(1, 2, 0));
        false_success.push_finalize(Effect::NoTransition);
        let error = step(&false_success, START + WINDOW)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("without a strict state transition"),
            "{error}"
        );
    }

    /// E2E-ROW: E2E-SUB-12/L0
    #[tokio::test]
    async fn exact_week_boundary_and_one_or_many_missed_weeks_use_one_call() {
        let before = snapshot(1, 1, 0);
        let early = ScriptedBackend::new(before);
        assert!(matches!(
            step(&early, START + SUB_WEEK_LEN.as_secs() - 1)
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(early.calls(), (0, 0));

        for (elapsed, expected_index) in [(1_u64, 1), (3_u64, 3)] {
            let backend = ScriptedBackend::new(before);
            let mut after = before;
            after.subscription.week_index = expected_index;
            after.subscription.tokens_paid =
                u128::from(expected_index) * after.subscription.tokens_per_week;
            backend.push_settle(Effect::Replace(Some(after)));
            assert_eq!(
                step(&backend, START + elapsed * SUB_WEEK_LEN.as_secs())
                    .await
                    .unwrap(),
                KeeperStep::Progress
            );
            assert_eq!(backend.calls(), (0, 1));
        }
    }

    #[tokio::test]
    async fn missed_week_settle_rejects_partial_advancement() {
        let before = snapshot(1, 1, 0);
        let mut partial = before;
        partial.subscription.week_index = 1;
        partial.subscription.tokens_paid = partial.subscription.tokens_per_week;
        let backend = ScriptedBackend::new(before);
        backend.push_settle(Effect::Replace(Some(partial)));

        let error = step(&backend, START + 3 * SUB_WEEK_LEN.as_secs())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("expected exact scheduled weekIndex 3")
                && error.contains("observed weekIndex 1"),
            "{error}"
        );
        assert_eq!(
            backend.calls(),
            (0, 1),
            "partial advancement is not accepted and the same step never retries"
        );
    }

    #[tokio::test]
    async fn lost_settle_response_and_restart_from_advanced_index_do_not_replay() {
        let before = snapshot(1, 1, 0);
        let mut after = before;
        after.subscription.week_index = 2;
        after.subscription.tokens_paid = 20 * TICK_SIZE;
        let backend = ScriptedBackend::new(before);
        backend.push_settle(Effect::ReplaceAndLoseResponse(Some(after)));

        assert_eq!(
            step(&backend, START + 2 * SUB_WEEK_LEN.as_secs())
                .await
                .unwrap(),
            KeeperStep::Progress
        );
        assert_eq!(backend.calls(), (0, 1));
        assert!(matches!(
            step(&backend, START + 3 * SUB_WEEK_LEN.as_secs() - 1)
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(
            backend.calls(),
            (0, 1),
            "advanced weekIndex is restart truth"
        );
    }

    #[tokio::test]
    async fn final_week_charges_then_promotes_then_closes_on_permissionless_retry() {
        let mut week_three = snapshot(1, 2, SUBSCRIPTION_WEEKS - 1);
        week_three.deal.last_claim_time =
            START + u64::from(SUBSCRIPTION_WEEKS) * SUB_WEEK_LEN.as_secs() - 5;
        let backend = ScriptedBackend::new(week_three);

        let mut charged = week_three;
        charged.subscription.week_index = SUBSCRIPTION_WEEKS;
        charged.subscription.tokens_paid =
            u128::from(SUBSCRIPTION_WEEKS) * charged.subscription.tokens_per_week;
        backend.push_settle(Effect::Replace(Some(charged)));
        assert_eq!(
            step(
                &backend,
                START + u64::from(SUBSCRIPTION_WEEKS) * SUB_WEEK_LEN.as_secs(),
            )
            .await
            .unwrap(),
            KeeperStep::Progress
        );

        assert!(matches!(
            step(&backend, charged.deal.last_claim_time + WINDOW - 1)
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));

        let mut promoted = charged;
        promoted.deal.tokens_final = 2 * TICK_SIZE;
        backend.push_finalize(Effect::Replace(Some(promoted)));
        assert_eq!(
            step(&backend, charged.deal.last_claim_time + WINDOW)
                .await
                .unwrap(),
            KeeperStep::Progress
        );

        backend.push_settle(Effect::Replace(None));
        assert!(matches!(
            step(&backend, charged.deal.last_claim_time + WINDOW)
                .await
                .unwrap(),
            KeeperStep::Terminal(_)
        ));
        assert_eq!(backend.calls(), (1, 2));
    }

    #[tokio::test]
    async fn close_or_dispute_racing_either_action_is_never_replayed() {
        let finalize_before = snapshot(1, 2, 0);
        let finalize = ScriptedBackend::new(finalize_before);
        finalize.push_finalize(Effect::ReplaceAndLoseResponse(None));
        assert!(matches!(
            step(&finalize, START + WINDOW).await.unwrap(),
            KeeperStep::Terminal(_)
        ));
        assert_eq!(finalize.calls(), (1, 0));

        let finalize_dispute = ScriptedBackend::new(finalize_before);
        let mut disputed_after_finalize = finalize_before;
        disputed_after_finalize.deal.disputed = true;
        finalize_dispute.push_finalize(Effect::ReplaceAndLoseResponse(Some(
            disputed_after_finalize,
        )));
        assert!(matches!(
            step(&finalize_dispute, START + WINDOW).await.unwrap(),
            KeeperStep::Wait(_)
        ));
        assert!(matches!(
            step(&finalize_dispute, START + WINDOW + 1).await.unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(finalize_dispute.calls(), (1, 0));

        let settle_before = snapshot(1, 1, 0);
        let settle = ScriptedBackend::new(settle_before);
        let mut disputed = settle_before;
        disputed.deal.disputed = true;
        settle.push_settle(Effect::ReplaceAndLoseResponse(Some(disputed)));
        assert!(matches!(
            step(&settle, START + SUB_WEEK_LEN.as_secs()).await.unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(settle.calls(), (0, 1));
        assert!(matches!(
            step(&settle, START + SUB_WEEK_LEN.as_secs() + 1)
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(settle.calls(), (0, 1));
    }

    #[tokio::test]
    async fn claim_between_authoritative_snapshots_avoids_a_stale_finalize() {
        let before = snapshot(1, 2, 0);
        let now = before.deal.last_claim_time + WINDOW;
        let mut after = before;
        after.deal.tokens_final = 2 * TICK_SIZE;
        after.deal.tokens_pending = 3 * TICK_SIZE;
        after.deal.last_claim_time = now;

        let backend = ScriptedBackend::new(before);
        backend.set_current(Some(after));
        backend.push_snapshot_reads([Some(before), Some(after)]);

        assert!(matches!(
            step(&backend, now).await.unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn settle_between_authoritative_snapshots_uses_the_advanced_week_without_replay() {
        let before = snapshot(1, 1, 0);
        let mut after = before;
        after.subscription.week_index = 1;
        after.subscription.tokens_paid = after.subscription.tokens_per_week;

        let backend = ScriptedBackend::new(before);
        backend.set_current(Some(after));
        backend.push_snapshot_reads([Some(before), Some(after)]);

        assert!(matches!(
            step(&backend, START + SUB_WEEK_LEN.as_secs())
                .await
                .unwrap(),
            KeeperStep::Wait(_)
        ));
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn terminal_destruction_between_authoritative_snapshots_is_not_written() {
        let before = snapshot(1, 2, 0);
        let backend = ScriptedBackend::new(before);
        backend.set_current(None);
        backend.push_snapshot_reads([Some(before), None]);

        assert!(matches!(
            step(&backend, START + WINDOW).await.unwrap(),
            KeeperStep::Terminal(_)
        ));
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn stable_absence_is_terminal_only_after_live_context() {
        let backend = ScriptedBackend::new(snapshot(1, 1, 0));
        backend.set_current(None);
        let error = keeper_step(
            &backend,
            &TC.to_string(),
            bounds(),
            START,
            &mut KeeperCursor::default(),
            &(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("before a live subscription was established"));
        assert_eq!(backend.calls(), (0, 0));

        let mut live = KeeperCursor {
            last_accepted: Some(snapshot(7, 7, 0)),
        };
        assert!(matches!(
            keeper_step(&backend, &TC.to_string(), bounds(), START, &mut live, &())
                .await
                .unwrap(),
            KeeperStep::Terminal(value) if value == 7 * TICK_SIZE
        ));
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn malformed_or_regressing_state_fails_closed() {
        let mut malformed = snapshot(3, 2, 0);
        let backend = ScriptedBackend::new(malformed);
        assert!(step(&backend, START + WINDOW)
            .await
            .unwrap_err()
            .to_string()
            .contains("claim pipeline regressed"));
        assert_eq!(backend.calls(), (0, 0));

        malformed = snapshot(1, 1, 0);
        malformed.subscription.period_start = u64::MAX;
        let overflow = ScriptedBackend::new(malformed);
        assert!(step(&overflow, u64::MAX)
            .await
            .unwrap_err()
            .to_string()
            .contains("overflows"));
        assert_eq!(overflow.calls(), (0, 0));

        let before = snapshot(1, 1, 1);
        let mut regressed = before;
        regressed.subscription.week_index = 0;
        regressed.subscription.tokens_paid = TICK_SIZE;
        let backend = ScriptedBackend::new(before);
        backend.push_settle(Effect::Replace(Some(regressed)));
        let error = step(&backend, START + 2 * SUB_WEEK_LEN.as_secs())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("regressing accepted state"), "{error}");
    }

    #[tokio::test]
    async fn cross_field_state_fails_before_writes() {
        let claimed_past_the_funded_term = snapshot(1, 41, 0);
        let mut week_base_above_the_claim = snapshot(1, 1, 0);
        week_base_above_the_claim.subscription.week_base_tokens = 2 * TICK_SIZE;
        let mut accepted_without_a_claim_anchor = snapshot(1, 2, 0);
        accepted_without_a_claim_anchor.deal.last_claim_time = 0;

        for (name, malformed) in [
            (
                "tokensPending above fundedTokens",
                claimed_past_the_funded_term,
            ),
            (
                "weekBaseTokens above tokensPending",
                week_base_above_the_claim,
            ),
            (
                "accepted with lastClaimTime=0",
                accepted_without_a_claim_anchor,
            ),
        ] {
            let backend = ScriptedBackend::new(malformed);
            assert!(
                step(&backend, START + SUB_WEEK_LEN.as_secs())
                    .await
                    .is_err(),
                "{name} must fail closed"
            );
            assert_eq!(backend.calls(), (0, 0), "{name} wrote before failing");
        }
    }

    #[tokio::test]
    async fn ordinary_deal_never_enters_keeper_or_writes() {
        let mut ordinary = snapshot(1, 1, 0);
        ordinary.subscription.deal_flags = 0;
        ordinary.subscription.sub_weeks = 0;
        ordinary.subscription.tokens_per_week = ordinary.subscription.funded_tokens;
        ordinary.subscription.week_base_tokens = 0;
        let backend = ScriptedBackend::new(ordinary);
        assert_eq!(
            step(&backend, START + 10 * SUB_WEEK_LEN.as_secs())
                .await
                .unwrap(),
            KeeperStep::NotSubscription
        );
        assert_eq!(backend.calls(), (0, 0));

        let token_contract = TC.to_string();
        let error = drive_subscription_keeper_with_clock(
            &backend,
            &token_contract,
            bounds(),
            &ManualClock {
                now: START + 10 * SUB_WEEK_LEN.as_secs(),
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("started for an ordinary deal"), "{error}");
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn graceful_task_cancellation_performs_zero_chain_writes() {
        let backend = Arc::new(ScriptedBackend::new(snapshot(1, 1, 0)));
        let task_backend = backend.clone();
        let task = tokio::spawn(async move {
            let token_contract = TC.to_string();
            drive_subscription_keeper_with_clock(
                task_backend.as_ref(),
                &token_contract,
                bounds(),
                &BlockingClock { now: START + 1 },
            )
            .await
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        assert_eq!(backend.calls(), (0, 0));
    }

    #[tokio::test]
    async fn injected_clock_drives_one_post_term_retry_to_terminal() {
        let backend = ScriptedBackend::new(snapshot(2, 2, SUBSCRIPTION_WEEKS));
        backend.push_settle(Effect::Replace(None));
        let token_contract = TC.to_string();
        let finalized = drive_subscription_keeper_with_clock(
            &backend,
            &token_contract,
            bounds(),
            &ManualClock {
                now: START + u64::from(SUBSCRIPTION_WEEKS) * SUB_WEEK_LEN.as_secs() + WINDOW,
            },
        )
        .await
        .unwrap();
        assert_eq!(finalized, 2 * TICK_SIZE);
        assert_eq!(backend.calls(), (0, 1));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn weekly_planning_and_reconciliation_is_restart_idempotent(
            (current_week, elapsed_week) in
                (0_u8..SUBSCRIPTION_WEEKS).prop_flat_map(|current_week| {
                    (
                        Just(current_week),
                        (current_week + 1)..=SUBSCRIPTION_WEEKS,
                    )
                }),
            lost_response in any::<bool>(),
        ) {
            let before = snapshot(1, 1, current_week);
            let now =
                START + u64::from(elapsed_week) * SUB_WEEK_LEN.as_secs();
            let action = KeeperPlan::SettleWeek(elapsed_week);
            prop_assert_eq!(
                plan(before, bounds(), now, &TC.to_string()).unwrap(),
                action
            );

            let mut exact = before;
            exact.subscription.week_index = elapsed_week;
            exact.subscription.tokens_paid =
                u128::from(elapsed_week) * exact.subscription.tokens_per_week;
            let submit = if lost_response {
                Err(ChainError::AmbiguousSubmit("lost response".to_string()))
            } else {
                Ok(())
            };
            prop_assert_eq!(
                reconcile_write(
                    &TC.to_string(),
                    bounds(),
                    action,
                    before,
                    Some(exact),
                    submit,
                )
                .unwrap(),
                KeeperStep::Progress
            );

            let no_transition =
                reconcile_write(&TC.to_string(), bounds(), action, before, Some(before), Ok(()));
            prop_assert!(no_transition.is_err());

            if elapsed_week > current_week + 1 {
                let mut partial = before;
                partial.subscription.week_index = elapsed_week - 1;
                partial.subscription.tokens_paid =
                    u128::from(elapsed_week - 1) * partial.subscription.tokens_per_week;
                let partial = reconcile_write(
                    &TC.to_string(),
                    bounds(),
                    action,
                    before,
                    Some(partial),
                    Ok(()),
                );
                prop_assert!(partial.is_err());
            }
            if elapsed_week < SUBSCRIPTION_WEEKS {
                let mut overshot = before;
                overshot.subscription.week_index = elapsed_week + 1;
                overshot.subscription.tokens_paid =
                    u128::from(elapsed_week + 1) * overshot.subscription.tokens_per_week;
                let overshot = reconcile_write(
                    &TC.to_string(),
                    bounds(),
                    action,
                    before,
                    Some(overshot),
                    Ok(()),
                );
                prop_assert!(overshot.is_err());
            }

            let mut regressed = before;
            if current_week == 0 {
                regressed.subscription.period_start -= 1;
            } else {
                regressed.subscription.week_index -= 1;
                regressed.subscription.tokens_paid = if regressed.subscription.week_index == 0 {
                    TICK_SIZE
                } else {
                    u128::from(regressed.subscription.week_index)
                        * regressed.subscription.tokens_per_week
                };
            }
            let regressed = reconcile_write(
                &TC.to_string(),
                bounds(),
                action,
                before,
                Some(regressed),
                Ok(()),
            );
            prop_assert!(regressed.is_err());

            let mut terminal = before;
            terminal.deal.opened = false;
            terminal.deal.deposit = 0;
            terminal.deal.probe_tick = 0;
            let terminal_submit = if lost_response {
                Err(ChainError::AmbiguousSubmit("lost response".to_string()))
            } else {
                Ok(())
            };
            let terminal_result = reconcile_write(
                &TC.to_string(),
                bounds(),
                action,
                before,
                Some(terminal),
                terminal_submit,
            )
            .unwrap();
            prop_assert!(matches!(terminal_result, KeeperStep::Terminal(_)));

            let restart = plan(exact, bounds(), now, &TC.to_string()).unwrap();
            if elapsed_week < SUBSCRIPTION_WEEKS {
                prop_assert!(matches!(restart, KeeperPlan::Wait(_)));
            } else {
                prop_assert_eq!(restart, KeeperPlan::SettleWeek(SUBSCRIPTION_WEEKS));
                prop_assert!(matches!(
                    reconcile_write(
                        &TC.to_string(),
                        bounds(),
                        restart,
                        exact,
                        None,
                        Ok(()),
                    )
                    .unwrap(),
                    KeeperStep::Terminal(_)
                ));
            }
        }
    }

    #[test]
    fn planner_uses_only_canonical_deadlines() {
        let state = snapshot(1, 1, 0);
        assert_eq!(
            plan(state, bounds(), START + 1, &TC.to_string()).unwrap(),
            KeeperPlan::Wait(Duration::from_secs(2))
        );
        assert_eq!(
            next_week_boundary(subscription(0), &TC.to_string()).unwrap(),
            START + SUB_WEEK_LEN.as_secs()
        );
    }
}
