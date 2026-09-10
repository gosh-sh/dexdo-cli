//! `chain` pure accounting/escrow helpers -- fee-inclusive escrow, tree aggregation, per-model breakdown,
//! deal anomalies, recoverability (PR4 move-only). No I/O.
use super::types::*;
use crate::params::{Shell, PLATFORM_FEE_BPS, SUBSCRIPTION_BUYER_BOND_TICKS, TICK_SIZE};
use std::collections::BTreeMap;

/// Order book platform fee (`InferenceOrderBook._tickFee`), bps: **250 = 2.5 %**,
/// charged ON TOP of the limit price per tick. The `placeBuyOrder` deposit check:
/// `escrow >= ticks x _unit(maxPricePerTick)`, where `_unit(p) = p + p x bps / 10000`. If the escrow
/// does not cover the fee, the order is rejected with `ERR_INSUFFICIENT_DEPOSIT`, but the SHELL has
/// already gone into the book: no match, no resting bid, no refund (orphaned escrow -- the "fourth
/// state", a track-2 contract bug). The client must check the invariant BEFORE `placeInferenceBuy` (track-1).
/// Fee-inclusive required escrow for `(ticks, max_price_per_tick)`, computed with **checked** arithmetic.
/// Returns `None` if ANY step overflows `u128` -- including the *intermediate* `p x FEE_BPS` fee product,
/// which can overflow and then be divided (`/ 10000`) back below `u128::MAX`, yielding a truncated value a
/// final `== u128::MAX` check would miss. This is the single source of truth for the escrow amount; the guard
/// rejects on `None` (fail-closed), not merely on a saturated final result.
fn checked_required_escrow_for_buy(ticks: u128, max_price_per_tick: u128) -> Option<u128> {
    let fee = max_price_per_tick.checked_mul(u128::from(PLATFORM_FEE_BPS))? / 10_000;
    let unit = max_price_per_tick.checked_add(fee)?;
    ticks.checked_mul(unit)
}

/// Minimum escrow that passes the book's deposit check for `(ticks, max_price_per_tick)`.
/// Mirrors the contract's integer arithmetic: `ticks x (p + p x FEE_BPS / 10000)` (truncation, as in
/// Solidity). Convenience wrapper over [`checked_required_escrow_for_buy`]: on ANY overflow it saturates to
/// `u128::MAX` (does not panic in debug, does not wrap in release), and [`check_buy_deposit_headroom`] rejects
/// the configuration (**fail-closed**). For real values (`<< u128::MAX`) the result exactly equals the contract's.
pub fn required_escrow_for_buy(ticks: u128, max_price_per_tick: u128) -> u128 {
    checked_required_escrow_for_buy(ticks, max_price_per_tick).unwrap_or(u128::MAX)
}

/// Exact subscription BUY money split at one price.

/// `deposit` buys the requested ticks including the platform fee. `buyer_bond` is a separate,
/// refundable `2P` reserve. `total_escrow` is the amount moved from the buyer note into the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionBuyReserve {
    pub deposit: u128,
    pub buyer_bond: u128,
    pub total_escrow: u128,
}

/// Checked subscription reserve at `price_per_tick`.

/// Every multiplication and addition is checked independently so an intermediate overflow cannot
/// be hidden by later division or truncation.
pub fn subscription_buy_reserve(
    ticks: u128,
    price_per_tick: u128,
) -> Result<SubscriptionBuyReserve, String> {
    let deposit = checked_required_escrow_for_buy(ticks, price_per_tick).ok_or_else(|| {
        format!(
            "subscription reserve: ticks {ticks} x pricePerTick {price_per_tick} x \
             (1 + {PLATFORM_FEE_BPS}bps fee) overflows u128"
        )
    })?;
    let buyer_bond = price_per_tick
        .checked_mul(SUBSCRIPTION_BUYER_BOND_TICKS)
        .ok_or_else(|| {
            format!(
                "subscription reserve: buyer bond {SUBSCRIPTION_BUYER_BOND_TICKS} x \
                 pricePerTick {price_per_tick} overflows u128"
            )
        })?;
    let total_escrow = deposit.checked_add(buyer_bond).ok_or_else(|| {
        format!("subscription reserve: deposit {deposit} + buyer bond {buyer_bond} overflows u128")
    })?;
    Ok(SubscriptionBuyReserve {
        deposit,
        buyer_bond,
        total_escrow,
    })
}

/// Checked ordinary BUY preflight reserve at `max_price_per_tick`.

/// `contracts/dex/PrivateNote.sol:748` debits `bond = 2 * clearingPrice` on every BUY fill.
/// Before matching, the client can prove that debit only at its known ceiling,
/// `max_price_per_tick`. This thin ordinary entry point delegates to
/// [`subscription_buy_reserve`] so the deposit, bond, and total stay in one checked
/// implementation.
pub fn ordinary_buy_reserve(
    ticks: u128,
    max_price_per_tick: u128,
) -> Result<SubscriptionBuyReserve, String> {
    subscription_buy_reserve(ticks, max_price_per_tick)
}

/// Require the exact subscription reserve before a money submit.

/// Both underfunding and overfunding are rejected: a subscription message carries precisely the
/// fee-inclusive service deposit plus its separate limit-priced buyer bond.
pub fn check_subscription_buy_reserve(
    escrow: u128,
    ticks: u128,
    max_price_per_tick: u128,
) -> Result<SubscriptionBuyReserve, String> {
    let required = subscription_buy_reserve(ticks, max_price_per_tick)?;
    if escrow < required.total_escrow {
        return Err(format!(
            "subscription escrow {} SHELL < required {} (= deposit {} + buyer bond {} at limit \
             price {})",
            crate::params::shell_amount(escrow),
            crate::params::shell_amount(required.total_escrow),
            crate::params::shell_amount(required.deposit),
            crate::params::shell_amount(required.buyer_bond),
            crate::params::shell_amount(max_price_per_tick)
        ));
    }
    if escrow > required.total_escrow {
        return Err(format!(
            "subscription escrow {} SHELL > exact required {} (= deposit {} + buyer bond {} at \
             limit price {}); do not overfund the order",
            crate::params::shell_amount(escrow),
            crate::params::shell_amount(required.total_escrow),
            crate::params::shell_amount(required.deposit),
            crate::params::shell_amount(required.buyer_bond),
            crate::params::shell_amount(max_price_per_tick)
        ));
    }
    Ok(required)
}

/// Exact book refund when a subscription clears below its BUY limit.

/// The deal receives only the fee-inclusive deposit plus `2P` bond at the clearing price; the
/// difference from the limit-priced reserve remains in the book's leftover escrow and returns to
/// the buyer.
pub fn subscription_buy_clearing_refund(
    ticks: u128,
    limit_price_per_tick: u128,
    clearing_price_per_tick: u128,
) -> Result<u128, String> {
    if clearing_price_per_tick > limit_price_per_tick {
        return Err(format!(
            "subscription clearing price {clearing_price_per_tick} exceeds BUY limit \
             {limit_price_per_tick}"
        ));
    }
    let reserved = subscription_buy_reserve(ticks, limit_price_per_tick)?;
    let forwarded = subscription_buy_reserve(ticks, clearing_price_per_tick)?;
    reserved
        .total_escrow
        .checked_sub(forwarded.total_escrow)
        .ok_or_else(|| "subscription clearing refund underflows u128".to_string())
}

/// Compute the executable quote over current resting asks in price/time order.

/// `wanted_ticks = Some(n)` quotes exactly up to `n` ticks; `budget = Some(x)` quotes as many ticks as fit in
/// fee-inclusive budget `x`. Exactly one selector must be set. The function is read-only and pure; callers decide
/// whether an incomplete quote is acceptable.
pub fn executable_quote(
    asks: &[OrderBookOrder],
    wanted_ticks: Option<u128>,
    budget: Option<u128>,
) -> Result<ExecutableQuote, String> {
    if wanted_ticks.is_some() == budget.is_some() {
        return Err("set exactly one of ticks or budget".to_string());
    }
    let asks = coalesce_equivalent_resting_asks(asks)?;

    let mut remaining_ticks = wanted_ticks.unwrap_or(u128::MAX);
    let mut remaining_budget = budget.unwrap_or(u128::MAX);
    let mut filled_ticks = 0u128;
    let mut total_with_fee = 0u128;
    let mut fills = Vec::new();

    for ask in asks {
        if remaining_ticks == 0 || remaining_budget == 0 {
            break;
        }
        let unit = required_escrow_for_buy(1, ask.price_per_tick);
        if unit == 0 || unit == u128::MAX {
            continue;
        }
        let by_budget = remaining_budget / unit;
        let fill_ticks = ask.ticks.min(remaining_ticks).min(by_budget);
        if fill_ticks == 0 {
            break;
        }
        let cost = required_escrow_for_buy(fill_ticks, ask.price_per_tick);
        let Some(token_contract) = ask.token_contract.clone() else {
            continue;
        };
        fills.push(QuoteFill {
            order_id: ask.order_id,
            token_contract,
            ticks: fill_ticks,
            price_per_tick: ask.price_per_tick,
            cost_with_fee: cost,
        });
        filled_ticks = filled_ticks.saturating_add(fill_ticks);
        total_with_fee = total_with_fee.saturating_add(cost);
        remaining_ticks = remaining_ticks.saturating_sub(fill_ticks);
        remaining_budget = remaining_budget.saturating_sub(cost);
    }

    let complete = match wanted_ticks {
        Some(want) => filled_ticks >= want,
        None => filled_ticks > 0,
    };
    Ok(ExecutableQuote {
        filled_ticks,
        total_with_fee,
        complete,
        fills,
    })
}

/// Compute a quote for the current chain submit path.

/// The contract's taker side is FOK: the requested amount must be covered by crossing liquidity, while maker
/// asks may be partial-taken and consumed as deal slots. Real chain callers additionally verify that every
/// raw fill selected by this quote points at a fresh/readable `TokenContract`; this pure helper has no state I/O.
pub fn submit_safe_single_ask_quote(
    asks: &[OrderBookOrder],
    wanted_ticks: Option<u128>,
    budget: Option<u128>,
) -> Result<ExecutableQuote, String> {
    executable_quote(asks, wanted_ticks, budget)
}

/// Coalesce duplicate resting asks for the same `TokenContract` only when they are equivalent candidates.

/// Legacy live books can contain repeated active SELL rows for one TC. They are not independent liquidity, but
/// if they expose the same owner/economic state, the order book's deterministic price/time head can still be
/// represented as one candidate without increasing buyer risk.
pub fn coalesce_equivalent_resting_asks(
    orders: &[OrderBookOrder],
) -> Result<Vec<OrderBookOrder>, String> {
    let mut by_tc: BTreeMap<String, Vec<&OrderBookOrder>> = BTreeMap::new();
    for order in orders.iter().filter(|o| o.is_resting_ask()) {
        if let Some(tc) = order.token_contract.as_deref() {
            by_tc
                .entry(tc.to_ascii_lowercase())
                .or_default()
                .push(order);
        }
    }

    let mut coalesced = Vec::with_capacity(by_tc.len());
    for (tc, mut group) in by_tc {
        group.sort_by_key(|o| o.order_id);
        let representative = group[0];
        if group.len() > 1 {
            for other in group.iter().skip(1) {
                if !equivalent_resting_ask(representative, other) {
                    let ids = group
                        .iter()
                        .map(|o| o.order_id.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    return Err(format!(
                        "duplicate active sell orders for one TokenContract have conflicting terms/state: \
                         {} at order_ids [{ids}]. Refusing to coalesce ambiguous liquidity.",
                        crate::address::display_self_dapp(&tc)
                    ));
                }
            }
        }
        coalesced.push((*representative).clone());
    }
    coalesced.sort_by_key(|o| (o.price_per_tick, o.order_id));
    Ok(coalesced)
}

fn equivalent_resting_ask(a: &OrderBookOrder, b: &OrderBookOrder) -> bool {
    a.owner_note == b.owner_note
        && a.price_per_tick == b.price_per_tick
        && a.ticks == b.ticks
        && a.escrow == b.escrow
        && a.deadline == b.deadline
        && a.flags == b.flags
}

/// Return the funding timestamp only for the exact state written by the contract at match time,
/// before the seller has ever opened the deal.
fn exact_never_opened_funded_time(state: DealChainState) -> Option<u64> {
    let funded_time = state.funded_time.filter(|value| *value > 0)?;
    (state.funded
        && !state.opened
        && !state.probe_accepted
        && !state.disputed
        && state.deposit > 0
        && state.probe_tick == 0
        && state.finalized_owed == 0
        && state.tokens_final == 0
        && state.tokens_pending == 0
        && state.probe_time == 0
        && state.last_claim_time == funded_time
        && state.dispute_time == 0)
        .then_some(funded_time)
}

pub fn check_matched_token_contract_state(
    token_contract: &str,
    state: DealChainState,
    now_secs: u64,
    match_open_timeout_secs: u64,
) -> Result<MatchedTokenContractStatus, String> {
    let token_contract = crate::address::display_self_dapp(token_contract);
    if state.disputed {
        return Err(format!(
            "reported match {token_contract} is disputed immediately after fill: funded={} opened={} \
             deposit={} tokensFinal={} fundedTime={:?} lastClaimTime={}. Refusing to wait for handover.",
            state.funded,
            state.opened,
            crate::params::shell_amount(state.deposit),
            state.tokens_final,
            state.funded_time,
            state.last_claim_time
        ));
    }
    if !state.funded {
        return Err(format!(
            "reported match {token_contract} is not funded after the fill event: funded=false opened={} \
             deposit={} fundedTime={:?} lastClaimTime={}. The book/fill event and TokenContract state \
             disagree; refusing to wait for handover or treat this as recoverable.",
            state.opened, state.deposit, state.funded_time, state.last_claim_time
        ));
    }
    if state.opened {
        return Ok(MatchedTokenContractStatus::Opened);
    }
    let funded_time = exact_never_opened_funded_time(state).ok_or_else(|| {
        format!(
            "reported match {token_contract} is not the authoritative funded-never-opened shape: \
             probeAccepted={} deposit={} probeTick={} finalizedOwed={} tokensFinal={} \
             tokensPending={} probeTime={} lastClaimTime={} \
             fundedTime={:?} disputeTime={}. Refusing to wait for handover or offer cleanup.",
            state.probe_accepted,
            state.deposit,
            state.probe_tick,
            state.finalized_owed,
            state.tokens_final,
            state.tokens_pending,
            state.probe_time,
            state.last_claim_time,
            state.funded_time,
            state.dispute_time,
        )
    })?;
    let cleanup_after_unix = funded_time.saturating_add(match_open_timeout_secs);
    let cleanup_ready = now_secs >= cleanup_after_unix;
    let remaining_secs = cleanup_after_unix.saturating_sub(now_secs);
    Ok(MatchedTokenContractStatus::FundedNeverOpened {
        funded_time: Some(funded_time),
        cleanup_after_unix: Some(cleanup_after_unix),
        cleanup_ready,
        remaining_secs: Some(remaining_secs),
    })
}

/// Pre-flight check of the buyer's deposit BEFORE `placeInferenceBuy`: the escrow must equal exactly
/// `required = ticks x maxPricePerTick x (1 + fee)`. UNDER: the book accepts the SHELL
/// and orphans it (no match, no bid, no refund). OVER: the surplus `escrow - required` is
/// debited but is NOT refunded when the buy rests and is filled as a maker -- `InferenceOrderBook._removeFromBook`
/// drops the residual, so it strands (live-proven on 4.0.10). The client rejects both IN ADVANCE rather than
/// send funds into the book blindly. Returns a human-readable reject reason.
pub fn check_buy_deposit_headroom(
    escrow: u128,
    ticks: u128,
    max_price_per_tick: u128,
) -> Result<(), String> {
    // Use the CHECKED helper directly: reject on ANY arithmetic overflow, not just a final `== u128::MAX`.
    // The intermediate `p x FEE_BPS` fee product can overflow then divide back below u128::MAX (a truncated
    // value), which a saturated-final check would miss -- letting `escrow == required` (the garbage) slip
    // through. Covers the omitted-`--escrow` default path too (it computes the same required).
    let required = checked_required_escrow_for_buy(ticks, max_price_per_tick).ok_or_else(|| format!(
        "escrow check: ticks {ticks} x maxPricePerTick {max_price_per_tick} x (1 + {PLATFORM_FEE_BPS}bps fee) \
         overflows u128 -- absurd configuration, rejected fail-closed ()."
    ))?;
    if escrow < required {
        // Every figure here is SHELL, including the two an operator types back: `--escrow` takes
        // SHELL and `--max-price-per-tick` takes whole SHELL a tick, and a remedy stated in raw
        // ECC[2] asks for a billion times the amount it means.
        return Err(format!(
            "escrow {} SHELL < minimum {} (= ticks {ticks} x maxPricePerTick \
             {} x (1 + {PLATFORM_FEE_BPS}bps book fee)): \
             placeInferenceBuy will be rejected with ERR_INSUFFICIENT_DEPOSIT, and the escrow will orphan in \
             the book (). Raise --escrow to >={} or lower --ticks/--max-price-per-tick.",
            crate::params::shell_amount(escrow),
            crate::params::shell_amount(required),
            crate::params::shell_amount(max_price_per_tick),
            crate::params::shell_amount(required)
        ));
    }
    if escrow > required {
        return Err(format!(
            "escrow {} SHELL > required {} (= ticks {ticks} x maxPricePerTick {} \
             x (1 + {PLATFORM_FEE_BPS}bps fee)): the surplus ({}) is debited but is NOT refunded when the buy \
             rests and is filled as a maker () -- it strands. Set --escrow to exactly {}, or \
             omit --escrow to use the computed default.",
            crate::params::shell_amount(escrow),
            crate::params::shell_amount(required),
            crate::params::shell_amount(max_price_per_tick),
            crate::params::shell_amount(escrow - required),
            crate::params::shell_amount(required)
        ));
    }
    Ok(())
}

/// Fold the tree's per-note snapshots into one: the monitor aggregates across all notes
/// under the key. Snapshot order = the enumeration order of `NoteTree::nodes`. Pure function (no network).
pub fn aggregate_tree(snaps: Vec<NoteSnapshot>) -> TreeSnapshot {
    let mut note_ids = Vec::with_capacity(snaps.len());
    let mut offers = Vec::new();
    let mut deals = Vec::new();
    let mut exposure: Shell = 0;
    for s in snaps {
        note_ids.push(s.note_id);
        offers.extend(s.offers);
        deals.extend(s.deals);
        exposure = exposure.saturating_add(s.exposure);
    }
    TreeSnapshot {
        note_ids,
        offers,
        deals,
        exposure,
    }
}

/// Finalized ticks of a deal come solely from the contract's immutable delivered-token counter.
fn finalized_ticks(snapshot: Option<&StreamSnapshot>) -> u64 {
    snapshot
        .map(|s| u64::try_from(s.tokens_final / TICK_SIZE).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Keep the existing `u64` saturation boundary for cross-deal monitor summaries.
fn summary_shell(amount: u128) -> Shell {
    Shell::try_from(amount).unwrap_or(Shell::MAX)
}

/// By-fact accounting view for one role, broken down by served model and counterparty. The
/// monitor calls it once per role (`Seller` for the seller view, `Buyer` for the buyer view). Deals of the
/// other role are skipped; a deal without a snapshot still appears (zero figures) so a lock-without-match /
/// `seller_received=0` anomaly stays visible. Grouping is first-seen order (deterministic); all
/// sums saturate.
pub fn per_model_breakdown(deals: &[DealView], role: DealRole) -> Vec<ModelBreakdown> {
    let mut models: Vec<ModelBreakdown> = Vec::new();
    for d in deals.iter().filter(|d| d.role == role) {
        let model_id = d.model.clone().unwrap_or_else(|| UNKNOWN_MODEL.to_string());
        let tokens = finalized_ticks(d.snapshot.as_ref());
        let (money, locked, burned) = match &d.snapshot {
            Some(s) => {
                let locked = match role {
                    DealRole::Seller => s.seller_locked,
                    DealRole::Buyer => s.buyer_locked,
                };
                (
                    summary_shell(s.seller_received),
                    summary_shell(locked),
                    summary_shell(s.burned),
                )
            }
            None => (0, 0, 0),
        };
        let mi = match models.iter().position(|m| m.model == model_id) {
            Some(i) => i,
            None => {
                models.push(ModelBreakdown {
                    model: model_id,
                    role,
                    counterparties: Vec::new(),
                    tokens: 0,
                    money: 0,
                    locked: 0,
                    burned: 0,
                });
                models.len() - 1
            }
        };
        let m = &mut models[mi];
        m.tokens = m.tokens.saturating_add(tokens);
        m.money = m.money.saturating_add(money);
        m.locked = m.locked.saturating_add(locked);
        m.burned = m.burned.saturating_add(burned);
        let ci = match m
            .counterparties
            .iter()
            .position(|c| c.counterparty == d.counterparty)
        {
            Some(i) => i,
            None => {
                m.counterparties.push(CounterpartyTally {
                    counterparty: d.counterparty.clone(),
                    tokens: 0,
                    money: 0,
                    locked: 0,
                    burned: 0,
                });
                m.counterparties.len() - 1
            }
        };
        let c = &mut m.counterparties[ci];
        c.tokens = c.tokens.saturating_add(tokens);
        c.money = c.money.saturating_add(money);
        c.locked = c.locked.saturating_add(locked);
        c.burned = c.burned.saturating_add(burned);
    }
    models
}

/// Surface by-fact accounting anomalies on a deal. The lead requires the view to HIGHLIGHT
/// class problems -- an orphaned lock, a lock that survived a STOP, a buyer lock past the two-tick
/// invariant -- rather than hide them behind a clean-looking number. Pure: operates on the by-fact snapshot;
/// a deal with no snapshot has nothing to flag.
pub fn deal_anomalies(deal: &DealView) -> Vec<DealAnomaly> {
    let mut out = Vec::new();
    let Some(snap) = deal.snapshot.as_ref() else {
        return out;
    };
    let locked = summary_shell(snap.seller_locked.saturating_add(snap.buyer_locked));
    if locked > 0 && deal.counterparty.is_none() {
        out.push(DealAnomaly::LockedNoMatch { locked });
    }
    if snap.closed && locked > 0 {
        out.push(DealAnomaly::LockedAfterClose { locked });
    }
    if deal.price_per_tick > 0 {
        // the buyer lock the contract escrows is `ticks x _unit(p)` with `_unit(p) = p + pxFEE_BPS/10000`
        // (the book fee, `required_escrow_for_buy`). So the two-tick ceiling is `2 x _unit(p)`, NOT a fee-less
        // `2 x p` -- the latter false-flagged every legitimate two-tick deal ( /: match the contract's
        // lock arithmetic). Saturates to `Shell::MAX` on absurd prices (then `buyer_lead` can't exceed it).
        let ceiling = required_escrow_for_buy(2, deal.price_per_tick as u128)
            .min(Shell::MAX as u128) as Shell;
        let buyer_lead = summary_shell(snap.buyer_lead);
        // bound the at-risk LEAD (`prepaid + frozen`), NOT the total `buyer_locked` -- the unspent deposit
        // for a multi-tick deal's remaining ticks is not part of the two-tick lead, so checking the total
        // false-flagged every legitimate `maxTicks > 2` deal (e.g. an 8-tick lock of 8200 vs a 2050 ceiling).
        if buyer_lead > ceiling {
            out.push(DealAnomaly::BuyerLockExceedsTwoTicks {
                buyer_lead,
                ceiling,
            });
        }
    }
    out
}

/// `dexdo recover` pre-flight -- the **pure** precondition behind the buyer-side recovery STOP.
/// An operator whose buyer process died can STOP an orphaned OPEN deal from the buyer note (the normal
/// buyer-STOP split -- no new protocol); the seller then `destroy`s the TC. This fails closed BEFORE
/// the on-chain `streamStop` so the operator gets an actionable error instead of a bare revert; the
/// on-chain `TokenContract.stop()` still enforces `msg.sender == _buyer` (this mirrors it client-side).
/// Kept here (no chain deps) so the recovery precondition is offline-regression-tested.
pub fn check_recoverable(
    opened: bool,
    disputed: bool,
    buyer_note: Option<&str>,
    note_addr: &str,
    buyer_pubkey: Option<&[u8; 32]>,
    note_ed_pubkey: &[u8; 32],
) -> Result<(), String> {
    if !opened {
        return Err(
            "recover: deal is not OPEN (already closed, or never matched) -- nothing to STOP".into(),
        );
    }
    if disputed {
        return Err("recover: deal is DISPUTED -- resolve via the dispute path, not recover".into());
    }
    match buyer_note {
        None => {
            return Err(
                "recover: deal has no recorded buyer note (not matched) -- nothing to STOP".into(),
            );
        }
        Some(buyer) if buyer != note_addr => {
            return Err(
                "recover: --note-addr is not the deal's buyer note -- only the buyer note can STOP \
                 (TokenContract.stop() enforces msg.sender == _buyer)"
                    .into(),
            );
        }
        Some(_) => {}
    }
    match buyer_pubkey {
        None => Err("recover: deal has no recorded buyer (not matched) -- nothing to STOP".into()),
        Some(bpk) if bpk != note_ed_pubkey => Err(
            "recover: --note-key is not the deal's buyer key -- only the buyer can STOP \
             (TokenContract.stop() enforces msg.sender == _buyer)"
                .into(),
        ),
        Some(_) => Ok(()),
    }
}

/// Why buyer ownership is required for a given action -- the two recovery preflights do not share one
/// reason, and telling the operator the wrong one is telling them the money is protected by a check that
/// does not exist.

/// * `dispute` is authorized on chain: `TokenContract.dispute()` requires `msg.sender == _buyer`.
/// * `reclaim` is **not**: `TokenContract.cleanupUnopened()` is permissionless, with payouts fixed by the
/// contract (deposit refund to the recorded buyer, seller bond back to the seller note). What is gated
/// is this client's submission path, the owner-keyed `PrivateNote.streamCleanup` wrapper, so the
/// ownership check is a client/pool-integrity policy: a recovery record the chain contradicts is
/// refused instead of being driven from some other note's key.
const DISPUTE_BUYER_ENFORCEMENT: &str =
    "the TokenContract enforces msg.sender == _buyer for this action";
const RECLAIM_BUYER_ENFORCEMENT: &str =
    "this client only submits the never-opened cleanup through the deal's own owner-keyed \
     PrivateNote.streamCleanup wrapper, and refuses a recovery record the chain contradicts; \
     TokenContract.cleanupUnopened() itself is permissionless with contract-fixed payouts, so no key \
     of ours can redirect that money";

/// Shared buyer-ownership gate for the recovery preflights (`dispute`/`reclaim`): the deal's recorded buyer
/// note + ed-pubkey must be THIS note (`--note-addr`/`--note-key`). `enforcement` states, per action, what
/// actually backs that requirement, so the error never claims a contract check the contract does not make.
fn check_buyer_owns(
    action: &str,
    enforcement: &str,
    buyer_note: Option<&str>,
    note_addr: &str,
    buyer_pubkey: Option<&[u8; 32]>,
    note_ed_pubkey: &[u8; 32],
) -> Result<(), String> {
    match buyer_note {
        None => {
            return Err(format!(
                "{action}: deal has no recorded buyer note (not matched) -- nothing to {action}"
            ))
        }
        Some(buyer) if buyer != note_addr => {
            return Err(format!(
                "{action}: --note-addr is not the deal's buyer note -- only the buyer note can {action} \
                 ({enforcement})"
            ))
        }
        Some(_) => {}
    }
    match buyer_pubkey {
        None => Err(format!(
            "{action}: deal has no recorded buyer (not matched) -- nothing to {action}"
        )),
        Some(bpk) if bpk != note_ed_pubkey => Err(format!(
            "{action}: --note-key is not the deal's buyer key -- only the buyer can {action} \
             ({enforcement})"
        )),
        Some(_) => Ok(()),
    }
}

/// `dexdo dispute` pre-flight -- the **pure** precondition behind the buyer-side on-chain dispute
/// (`streamDispute` -> `TC.dispute()`, which freezes this TC's contested funds,). Gates: the deal is OPEN, not already
/// disputed, and owned by THIS buyer note/key. Strictly stronger than `recover`'s STOP (which still pays for
/// delivered ticks) -- the anti-scam lever for an observed substitution. Offline-regression-tested.
pub fn check_disputable(
    opened: bool,
    disputed: bool,
    buyer_note: Option<&str>,
    note_addr: &str,
    buyer_pubkey: Option<&[u8; 32]>,
    note_ed_pubkey: &[u8; 32],
) -> Result<(), String> {
    if !opened {
        return Err(
            "dispute: deal is not OPEN (already closed, or never matched) -- nothing to dispute"
                .into(),
        );
    }
    if disputed {
        return Err(
            "dispute: deal is ALREADY disputed -- wait for releaseDispute/arbitration".into(),
        );
    }
    check_buyer_owns(
        "dispute",
        DISPUTE_BUYER_ENFORCEMENT,
        buyer_note,
        note_addr,
        buyer_pubkey,
        note_ed_pubkey,
    )
}

/// `dexdo reclaim` pre-flight -- whether the one write this command owns,
/// `PrivateNote.streamCleanup` -> `TC.cleanupUnopened()`, is admissible at all. Fails LOUD before sending
/// rather than letting the contract revert:
/// - not disputed, matched, owned by THIS buyer (else reject);
/// - funded (else nothing to reclaim);
/// - OPENED -> reject: explicit STOP is selected by `close`/`recover`, never rewritten from `reclaim`;
/// - funded, closed, and the exact never-opened state produced at funding +
/// `now >= funded_time + match_open_timeout` -> admissible;
/// - funded but never opened before `MATCH_OPEN_TIMEOUT` -> reject (too early).

/// Times are seconds (client `SystemTime` vs on-chain `lastClaimTime`/`fundedTime` + contract timeouts).
/// Offline-regression-tested.
pub fn check_reclaimable(
    state: DealChainState,
    buyer_note: Option<&str>,
    note_addr: &str,
    buyer_pubkey: Option<&[u8; 32]>,
    note_ed_pubkey: &[u8; 32],
    now: u64,
    match_open_timeout: u64,
) -> Result<(), String> {
    if state.disputed {
        return Err("reclaim: deal is DISPUTED -- resolve via the dispute path (releaseDispute/resolveDisputeTimeout), not reclaim".into());
    }
    check_buyer_owns(
        "reclaim",
        RECLAIM_BUYER_ENFORCEMENT,
        buyer_note,
        note_addr,
        buyer_pubkey,
        note_ed_pubkey,
    )?;
    if !state.funded {
        return Err("reclaim: deal is not funded (not matched) -- nothing to reclaim".into());
    }
    if state.opened {
        return Err(
            "reclaim: deal is OPEN -- use explicit `dexdo close` or `dexdo recover` to STOP it"
                .into(),
        );
    }

    if state.is_stopped() {
        return Err(
            "reclaim: deal is already terminal/drained; refusing streamCleanup before submit"
                .into(),
        );
    }
    let funded_time = exact_never_opened_funded_time(state).ok_or_else(|| {
        format!(
            "reclaim: CLOSED deal is not the authoritative never-opened shape; refusing \
             streamCleanup before submit (probeAccepted={} deposit={} probeTick={} \
             finalizedOwed={} tokensFinal={} tokensPending={} probeTime={} \
             lastClaimTime={} fundedTime={:?} disputeTime={})",
            state.probe_accepted,
            state.deposit,
            state.probe_tick,
            state.finalized_owed,
            state.tokens_final,
            state.tokens_pending,
            state.probe_time,
            state.last_claim_time,
            state.funded_time,
            state.dispute_time,
        )
    })?;
    let deadline = funded_time.saturating_add(match_open_timeout);
    if now < deadline {
        return Err(format!(
            "reclaim: too early -- the NEVER-OPENED deal's MATCH_OPEN_TIMEOUT is not reached: fundedTime \
             {funded_time} + matchOpenTimeout {match_open_timeout} = {deadline} > now {now} ({} s \
             remaining). The seller can still open; cleanup only after the timeout.",
            deadline.saturating_sub(now)
        ));
    }
    Ok(())
}

/// `dexdo release-dispute` pre-flight -- the seller can concede only an actually disputed deal.
/// The on-chain `TokenContract.releaseDispute()` also enforces `onlyOwnerPubkey(_sellerPubkey)`; this pure
/// gate keeps the client from submitting a known no-op/revert when the deal is not in dispute.
pub fn check_release_disputable(disputed: bool) -> Result<(), String> {
    if disputed {
        Ok(())
    } else {
        Err("release-dispute: deal is not DISPUTED -- nothing to release".into())
    }
}

/// Shared seller-key gate for seller-signed TC actions. `getSeller().sellerPubkey` is a uint256 hex string
/// (usually `0x...`), while the SDK key exposes bare hex. This mirrors `onlyOwnerPubkey(_sellerPubkey)` so a
/// wrong key fails before an on-chain submit where the getter is available.
pub fn check_seller_pubkey(
    action: &str,
    seller_pubkey: Option<&str>,
    signing_pubkey_hex: &str,
) -> Result<(), String> {
    let norm = |s: &str| {
        s.trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X")
            .to_ascii_lowercase()
            .trim_start_matches('0')
            .to_string()
    };
    let seller = seller_pubkey
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{action}: TokenContract exposes no seller pubkey"))?;
    if norm(seller) == norm(signing_pubkey_hex) {
        Ok(())
    } else {
        Err(format!(
            "{action}: --note-key is not the deal's seller key -- TokenContract onlyOwnerPubkey(_sellerPubkey) \
             will reject it (contract seller 0x{}, signing key 0x{})",
            norm(seller),
            norm(signing_pubkey_hex)
        ))
    }
}

/// `dexdo withdraw-shell` pre-flight -- withdraw either an explicit amount or all currently finalized
/// seller proceeds. Reject zero and over-withdraw locally before calling `TokenContract.withdrawShell`.
pub fn check_withdrawable_shell(
    finalized_owed: u128,
    amount: Option<u128>,
) -> Result<u128, String> {
    let amount = amount.unwrap_or(finalized_owed);
    if amount == 0 {
        return Err("withdraw-shell: no finalized SHELL is withdrawable".into());
    }
    if amount > finalized_owed {
        // The operator typed SHELL, so the refusal shows SHELL: echoing the raw figure showed him
        // a number he never entered and could not act on.
        return Err(format!(
            "withdraw-shell: amount {} SHELL exceeds finalizedOwed {}",
            crate::params::shell_amount(amount),
            crate::params::shell_amount(finalized_owed)
        ));
    }
    Ok(amount)
}

#[cfg(test)]
mod ordinary_buy_reserve_tests {
    use super::{ordinary_buy_reserve, subscription_buy_reserve};

    #[test]
    fn ordinary_and_subscription_buy_reserves_match_at_the_same_inputs() {
        let ticks = 3;
        let price_per_tick = 1_000_000;

        assert_eq!(
            ordinary_buy_reserve(ticks, price_per_tick),
            subscription_buy_reserve(ticks, price_per_tick)
        );
    }
}
