//! `chain` pure accounting/escrow helpers -- fee-inclusive escrow, tree aggregation, per-model breakdown,
//! deal anomalies, recoverability(PR4 move-only). No I/O.
use super::types::*;
use crate::params::Shell;
use std::collections::BTreeMap;

/// Order book platform fee(`InferenceOrderBook._tickFee`), bps: **250 = 2.5 %**,
/// charged ON TOP of the limit price per tick. The `placeBuyOrder` deposit check:
/// `escrow >= ticks x _unit(maxPricePerTick)`, where `_unit(p) = p + p x bps / 10000`. If the escrow
/// does not cover the fee, the order is rejected with `ERR_INSUFFICIENT_DEPOSIT`, but the SHELL has
/// already gone into the book: no match, no resting bid, no refund (orphaned escrow -- the "fourth
/// state", a track-2 contract bug). The client must check the invariant BEFORE `placeInferenceBuy`(track-1).
pub const ORDERBOOK_FEE_BPS: u128 = 250;

/// Fee-inclusive required escrow for `(ticks, max_price_per_tick)`, computed with **checked** arithmetic.
/// Returns `None` if ANY step overflows `u128` -- including the *intermediate* `p x FEE_BPS` fee product,
/// which can overflow and then be divided(`/ 10000`) back below `u128::MAX`, yielding a truncated value a
/// final `== u128::MAX` check would miss. This is the single source of truth for the escrow amount; the guard
/// rejects on `None`(fail-closed), not merely on a saturated final result.
fn checked_required_escrow_for_buy(ticks: u128, max_price_per_tick: u128) -> Option<u128> {
    let fee = max_price_per_tick.checked_mul(ORDERBOOK_FEE_BPS)? / 10_000;
    let unit = max_price_per_tick.checked_add(fee)?;
    ticks.checked_mul(unit)
}

/// Minimum escrow that passes the book's deposit check for `(ticks, max_price_per_tick)`.
/// Mirrors the contract's integer arithmetic: `ticks x(p + p x FEE_BPS / 10000)` (truncation, as in
/// Solidity). Convenience wrapper over [`checked_required_escrow_for_buy`]: on ANY overflow it saturates to
/// `u128::MAX`(does not panic in debug, does not wrap in release), and [`check_buy_deposit_headroom`] rejects
/// the configuration(**fail-closed**). For real values(`<< u128::MAX`) the result exactly equals the contract's.
pub fn required_escrow_for_buy(ticks: u128, max_price_per_tick: u128) -> u128 {
    checked_required_escrow_for_buy(ticks, max_price_per_tick).unwrap_or(u128::MAX)
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

/// Compute a quote for the current shellnet submit path.
/// The contract's taker side is FOK: the requested amount must be covered by crossing liquidity, while maker
/// asks may be partial-taken and consumed as deal slots. Real shellnet callers additionally verify that every
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
                         {tc} at order_ids [{ids}]. Refusing to coalesce ambiguous liquidity."
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

pub fn check_matched_token_contract_state(
    token_contract: &str,
    state: DealChainState,
    now_secs: u64,
    match_open_timeout_secs: u64,
) -> Result<MatchedTokenContractStatus, String> {
    if state.disputed {
        return Err(format!(
            "reported match {token_contract} is disputed immediately after fill: funded={} opened={} \
             probeAccepted={} fundedTime={:?} lastAdvance={}. Refusing to wait for handover.",
            state.funded,
            state.opened,
            state.probe_accepted,
            state.funded_time,
            state.last_advance
        ));
    }
    if !state.funded {
        return Err(format!(
            "reported match {token_contract} is not funded after the fill event: funded=false opened={} \
             probeAccepted={} fundedTime={:?} lastAdvance={}. The book/fill event and TokenContract state \
             disagree; refusing to wait for handover or treat this as recoverable.",
            state.opened, state.probe_accepted, state.funded_time, state.last_advance
        ));
    }
    if state.opened {
        return Ok(MatchedTokenContractStatus::Opened);
    }
    if state.probe_accepted {
        return Err(format!(
            "reported match {token_contract} has funded=true/opened=false/probeAccepted=true. That is not the \
             funded-never-opened recovery state; refusing to wait for handover."
        ));
    }
    let cleanup_after_unix = state
        .funded_time
        .map(|t| t.saturating_add(match_open_timeout_secs));
    let cleanup_ready = cleanup_after_unix.is_some_and(|deadline| now_secs >= deadline);
    let remaining_secs = cleanup_after_unix.map(|deadline| deadline.saturating_sub(now_secs));
    Ok(MatchedTokenContractStatus::FundedNeverOpened {
        funded_time: state.funded_time,
        cleanup_after_unix,
        cleanup_ready,
        remaining_secs,
    })
}

/// Pre-flight check of the buyer's deposit BEFORE `placeInferenceBuy`: the escrow must equal exactly
/// `required = ticks x maxPricePerTick x(1 + fee)`. UNDER: the book accepts the SHELL
/// and orphans it(no match, no bid, no refund). OVER: the surplus `escrow - required` is
/// debited but is NOT refunded when the buy rests and is filled as a maker -- `InferenceOrderBook._removeFromBook`
/// drops the residual, so it strands(live-proven on 4.0.10). The client rejects both IN ADVANCE rather than
/// send funds into the book blindly. Returns a human-readable reject reason.
pub fn check_buy_deposit_headroom(
    escrow: u128,
    ticks: u128,
    max_price_per_tick: u128,
) -> Result<(), String> {
    // Use the CHECKED helper directly: reject on ANY arithmetic overflow, not just a final `== u128::MAX`.
    // The intermediate `p x FEE_BPS` fee product can overflow then divide back below u128::MAX (a truncated
    // value), which a saturated-final check would miss -- letting `escrow == required`(the garbage) slip
    // through. Covers the omitted-`--escrow` default path too(it computes the same required).
    let required = checked_required_escrow_for_buy(ticks, max_price_per_tick).ok_or_else(|| format!(
        "escrow check: ticks {ticks} x maxPricePerTick {max_price_per_tick} x (1 + {ORDERBOOK_FEE_BPS}bps fee) \
         overflows u128 -- absurd configuration, rejected fail-closed ()."
    ))?;
    if escrow < required {
        return Err(format!(
            "escrow {escrow} < minimum {required} (= ticks {ticks} x maxPricePerTick \
             {max_price_per_tick} x (1 + {ORDERBOOK_FEE_BPS}bps book fee)): \
             placeInferenceBuy will be rejected with ERR_INSUFFICIENT_DEPOSIT, and the escrow will orphan in \
             the book (). Raise --escrow to >={required} or lower --ticks/--max-price-per-tick."
        ));
    }
    if escrow > required {
        return Err(format!(
            "escrow {escrow} > required {required} (= ticks {ticks} x maxPricePerTick {max_price_per_tick} \
             x (1 + {ORDERBOOK_FEE_BPS}bps fee)): the surplus ({}) is debited but is NOT refunded when the buy \
             rests and is filled as a maker () -- it strands. Set --escrow to exactly {required}, or \
             omit --escrow to use the computed default.",
            escrow - required
        ));
    }
    Ok(())
}

/// Fold the tree's per-note snapshots into one: the monitor aggregates across all notes
/// under the key. Snapshot order = the enumeration order of `NoteTree::nodes`. Pure function(no network).
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

/// Finalized ticks(tokens) of a deal: `seller_received / price_per_tick` -- each finalized tick pays the
/// seller exactly `price_per_tick` SHELL. Zero when the price is zero(no division by zero) or the
/// stream never opened.
fn finalized_ticks(snapshot: Option<&StreamSnapshot>, price_per_tick: Shell) -> u64 {
    match snapshot {
        Some(s) if price_per_tick > 0 => s.seller_received / price_per_tick,
        _ => 0,
    }
}

/// By-fact accounting view for one role, broken down by served model and counterparty. The
/// monitor calls it once per role(`Seller` for the seller view, `Buyer` for the buyer view). Deals of the
/// other role are skipped; a deal without a snapshot still appears(zero figures) so a lock-without-match /
/// `seller_received=0` anomaly stays visible. Grouping is first-seen order(deterministic); all
/// sums saturate.
pub fn per_model_breakdown(deals: &[DealView], role: DealRole) -> Vec<ModelBreakdown> {
    let mut models: Vec<ModelBreakdown> = Vec::new();
    for d in deals.iter().filter(|d| d.role == role) {
        let model_id = d.model.clone().unwrap_or_else(|| UNKNOWN_MODEL.to_string());
        let tokens = finalized_ticks(d.snapshot.as_ref(), d.price_per_tick);
        let (money, locked, burned) = match &d.snapshot {
            Some(s) => {
                let locked = match role {
                    DealRole::Seller => s.seller_locked,
                    DealRole::Buyer => s.buyer_locked,
                };
                (s.seller_received, locked, s.burned)
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
    let locked = snap.seller_locked.saturating_add(snap.buyer_locked);
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
        // lock arithmetic). Saturates to `Shell::MAX` on absurd prices(then `buyer_lead` can't exceed it).
        let ceiling = required_escrow_for_buy(2, deal.price_per_tick as u128)
            .min(Shell::MAX as u128) as Shell;
        // bound the at-risk LEAD(`prepaid + frozen`), NOT the total `buyer_locked` -- the unspent deposit
        // for a multi-tick deal's remaining ticks is not part of the two-tick lead, so checking the total
        // false-flagged every legitimate `maxTicks > 2` deal(e.g. an 8-tick lock of 8200 vs a 2050 ceiling).
        if snap.buyer_lead > ceiling {
            out.push(DealAnomaly::BuyerLockExceedsTwoTicks {
                buyer_lead: snap.buyer_lead,
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
/// on-chain `TokenContract.stop()` still enforces `msg.sender == _buyer`(this mirrors it client-side).
/// Kept here(no chain deps) so the recovery precondition is offline-regression-tested.
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

/// Shared buyer-ownership gate for the recovery preflights(`dispute`/`reclaim`): the deal's recorded buyer
/// note + ed-pubkey must be THIS note(`--note-addr`/`--note-key`). The on-chain `TokenContract` enforces
/// `msg.sender == _buyer`; this mirrors it client-side so the operator gets an actionable error, not a bare revert.
fn check_buyer_owns(
    action: &str,
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
                 (the TokenContract enforces msg.sender == _buyer)"
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
             (the TokenContract enforces msg.sender == _buyer)"
        )),
        Some(_) => Ok(()),
    }
}

/// `dexdo dispute` pre-flight -- the **pure** precondition behind the buyer-side on-chain dispute
/// (`streamDispute` -> `TC.dispute()`, which LOCKS both notes,). Gates: the deal is OPEN, not already
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
        buyer_note,
        note_addr,
        buyer_pubkey,
        note_ed_pubkey,
    )
}

/// Contract fixed constant for the funded-but-never-opened cleanup path
/// (`contracts/airegistry/modifiers/modifiers.sol::MATCH_OPEN_TIMEOUT`).
pub const MATCH_OPEN_TIMEOUT_SECS: u64 = 600;

/// `dexdo reclaim` pre-flight -- the pure timer gate behind the buyer-side timeout reclaim:
/// `streamReclaim` for opened-abandoned deals, `streamCleanup` for funded-but-never-opened deals. Fails LOUD
/// before sending rather than letting the contract revert:
/// - not disputed, matched, owned by THIS buyer(else reject);
/// - funded(else nothing to reclaim);
/// - OPENED + `now >= last_advance + stream_timeout` -> Ok(the `streamReclaim` path, `TC.sol:597`);
/// - OPENED but before the timeout -> reject(too early);
/// - funded but never opened + `now >= funded_time + match_open_timeout` -> Ok(`streamCleanup` path);
/// - funded but never opened before `MATCH_OPEN_TIMEOUT` -> reject(too early).
/// Times are seconds(client `SystemTime` vs on-chain `lastAdvance`/`fundedTime` + contract timeouts).
/// Offline-regression-tested.
#[allow(clippy::too_many_arguments)]
pub fn check_reclaimable(
    funded: bool,
    opened: bool,
    disputed: bool,
    buyer_note: Option<&str>,
    note_addr: &str,
    buyer_pubkey: Option<&[u8; 32]>,
    note_ed_pubkey: &[u8; 32],
    now: u64,
    last_advance: u64,
    stream_timeout: Option<u64>,
    funded_time: Option<u64>,
    match_open_timeout: u64,
) -> Result<(), String> {
    if disputed {
        return Err("reclaim: deal is DISPUTED -- resolve via the dispute path (releaseDispute/arbitration), not reclaim".into());
    }
    check_buyer_owns(
        "reclaim",
        buyer_note,
        note_addr,
        buyer_pubkey,
        note_ed_pubkey,
    )?;
    if !funded {
        return Err("reclaim: deal is not funded (not matched) -- nothing to reclaim".into());
    }
    if !opened {
        let funded_time = funded_time.ok_or_else(|| {
            "reclaim: getState exposes no fundedTime; cannot preflight the never-opened MATCH_OPEN_TIMEOUT"
                .to_string()
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
        return Ok(());
    }
    let stream_timeout = stream_timeout.ok_or_else(|| {
        "reclaim: getConfig exposes no streamTimeout; cannot preflight the OPEN deal timeout"
            .to_string()
    })?;
    let deadline = last_advance.saturating_add(stream_timeout);
    if now < deadline {
        return Err(format!(
            "reclaim: too early -- the OPEN deal's STREAM_TIMEOUT is not reached: lastAdvance {last_advance} + \
             streamTimeout {stream_timeout} = {deadline} > now {now} ({} s remaining). The seller can still \
             advance; reclaim only after the timeout.",
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
        return Err(format!(
            "withdraw-shell: amount {amount} exceeds finalizedOwed {finalized_owed}"
        ));
    }
    Ok(amount)
}
