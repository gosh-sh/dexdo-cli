//! `chain` data types -- offers/match, deal/stream snapshots, accounting tallies, errors (PR4 move-only).
use crate::note::NotePubkey;
use crate::params::{Shell, SUBSCRIPTION_WEEKS, SUB_WEEK_LEN};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

fn getter_exact_object<'a>(
    value: &'a Value,
    getter: &str,
    expected_fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{getter} returned a non-object JSON value"))?;
    let missing = expected_fields
        .iter()
        .filter(|field| !object.contains_key(**field))
        .copied()
        .collect::<Vec<_>>();
    let mut unexpected = object
        .keys()
        .filter(|field| !expected_fields.contains(&field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unexpected.sort();
    if !missing.is_empty() || !unexpected.is_empty() || object.len() != expected_fields.len() {
        return Err(format!(
            "{getter} returned {} fields, expected exactly {}{}{}",
            object.len(),
            expected_fields.len(),
            if missing.is_empty() {
                String::new()
            } else {
                format!("; missing fields: {}", missing.join(", "))
            },
            if unexpected.is_empty() {
                String::new()
            } else {
                format!("; unexpected fields: {}", unexpected.join(", "))
            }
        ));
    }
    Ok(object)
}

fn getter_field<'a>(value: &'a Value, getter: &str, field: &str) -> Result<&'a Value, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{getter} returned a non-object JSON value"))?
        .get(field)
        .ok_or_else(|| format!("{getter}.{field} is missing"))
}

fn getter_bool(value: &Value, getter: &str, field: &str) -> Result<bool, String> {
    getter_field(value, getter, field)?
        .as_bool()
        .ok_or_else(|| format!("{getter}.{field} is not a bool"))
}

fn getter_decimal<'a>(value: &'a Value, getter: &str, field: &str) -> Result<&'a str, String> {
    let raw = getter_field(value, getter, field)?
        .as_str()
        .ok_or_else(|| format!("{getter}.{field} is not a decimal string"))?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "{getter}.{field} value {raw:?} is not an unsigned decimal integer"
        ));
    }
    Ok(raw)
}

fn getter_u8(value: &Value, getter: &str, field: &str) -> Result<u8, String> {
    let raw = getter_decimal(value, getter, field)?;
    raw.parse::<u8>()
        .map_err(|error| format!("{getter}.{field} value {raw:?} exceeds uint8: {error}"))
}

fn getter_u64(value: &Value, getter: &str, field: &str) -> Result<u64, String> {
    let raw = getter_decimal(value, getter, field)?;
    raw.parse::<u64>()
        .map_err(|error| format!("{getter}.{field} value {raw:?} exceeds uint64: {error}"))
}

fn getter_u128(value: &Value, getter: &str, field: &str) -> Result<u128, String> {
    let raw = getter_decimal(value, getter, field)?;
    raw.parse::<u128>()
        .map_err(|error| format!("{getter}.{field} value {raw:?} exceeds uint128: {error}"))
}

/// `token_contract` address. In the mock -- an identifier string.
pub type TokenContract = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellOfferOutcome {
    Rested { order_id: u128 },
    Matched,
}

/// Sell offer in the book: the endpoint is NOT published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SellOffer {
    pub price_per_tick: Shell,
    pub max_ticks: u64,
    #[serde(with = "crate::address::serde_self_dapp")]
    pub token_contract: TokenContract,
    /// Deal-shape flags passed to `PrivateNote.postSellOffer`.
    #[serde(default)]
    pub flags: u8,
}

/// Book discovery item: offer + **seller identifier** (note) -- for
/// ranking and the blacklist (B16). In the mock `seller_id` = hex of the seller's note ed-pubkey; on the
/// real chain -- the seller from the `InferenceOrderBook` order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferListing {
    #[serde(with = "crate::address::serde_canonical")]
    pub seller_id: String,
    #[serde(with = "crate::address::serde_self_dapp")]
    pub token_contract: TokenContract,
    pub price_per_tick: Shell,
    pub max_ticks: u64,
}

/// One active order in an `InferenceOrderBook`.

/// Sell offers have `is_buy = false` and a non-empty `token_contract`. Resting buy orders have
/// `is_buy = true`, no target `token_contract`, and carry their still-held `escrow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookOrder {
    pub order_id: u128,
    #[serde(with = "crate::address::serde_canonical")]
    pub owner_note: String,
    #[serde(with = "crate::address::serde_self_dapp_opt")]
    pub token_contract: Option<TokenContract>,
    pub is_buy: bool,
    pub price_per_tick: u128,
    pub ticks: u128,
    pub escrow: u128,
    pub deadline: u64,
    pub flags: u8,
    pub timestamp: u64,
}

/// Opaque event-stream boundary captured immediately before one exact resting-SELL cancel submit.

/// The real chain backend uses the newest `InferenceOrderBook` ext-out message id so a later terminal
/// `InferenceOrderCancelRejected` is accepted only when it appeared after this submit began. Mock
/// backends need no event stream and use the empty marker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestingSellCancelWatch {
    event_marker: Option<String>,
}

impl RestingSellCancelWatch {
    pub fn from_event_marker(event_marker: Option<String>) -> Self {
        Self { event_marker }
    }

    pub fn event_marker(&self) -> Option<&str> {
        self.event_marker.as_deref()
    }
}

/// Stage-aware failure while establishing an exact cancel watch and submitting the cancel.
#[derive(Debug)]
pub enum RestingSellCancelStartError {
    /// No cancel was submitted because its event boundary could not be established.
    Preparation(ChainError),
    /// The existing owner-authorized cancel submit returned an error.
    Submit(ChainError),
}

/// The one deadline predicate every view that claims an order is live or executable applies.

/// `now < deadline`, because the book's `_isExpired` is `deadline != 0 && block.timestamp >= deadline`
/// (`contracts/airegistry/InferenceOrderBook.sol:1115-1117`) -- the deadline second itself is already expired
/// on chain and must not be offered off it.

/// `deadline == 0` is side-dependent, and that asymmetry is the contract's, not this client's. The book
/// reads a zero deadline as "never expires", which is a legitimate GTC bid. A SELL commits no
/// collateral so it MUST auto-expire, and `PrivateNote.postSellOffer` rejects `ttl == 0`
/// (`contracts/dex/PrivateNote.sol:41,792`, `ERR_SELL_DEADLINE_TOO_LONG`): a zero-deadline ask cannot
/// come from a well-formed SELL, so it is treated as malformed rather than as immortal liquidity.
pub fn order_deadline_is_live(is_buy: bool, deadline: u64, now_unix: u64) -> bool {
    if deadline == 0 {
        return is_buy;
    }
    now_unix < deadline
}

impl OrderBookOrder {
    /// SHAPE only: a SELL row with a deal TokenContract and capacity left. Deliberately says nothing
    /// about time -- a duplicate-TokenContract safety check is about the shape of the book and needs no
    /// clock. Any view that reports an ask as live or executable must use
    /// [`OrderBookOrder::is_live_resting_ask_at`] instead.
    pub fn is_resting_ask(&self) -> bool {
        !self.is_buy && self.token_contract.is_some() && self.ticks > 0
    }

    /// The right shape AND still valid at `now_unix`. Liveness needs a clock and shape does not, which
    /// is why the two are separate predicates rather than one that silently assumes a time.
    pub fn is_live_resting_ask_at(&self, now_unix: u64) -> bool {
        self.is_resting_ask() && order_deadline_is_live(self.is_buy, self.deadline, now_unix)
    }
}

/// Fail closed unless the fresh matcher head has the order identity and executable terms rendered
/// to the buyer.

/// The frozen row is all SIX fields E2E-ORD-23 names, not four. `deadline` and `flags` are as
/// load-bearing as the price: `deadline` is the moment the book stops honouring this ask
/// (`InferenceOrderBook._isExpired`), and `flags` decides what the deal the escrow funds actually IS
/// -- AON, IOC, subscription, TEE -- forwarded verbatim into the `TokenContract` under `DEAL_FLAGS_MASK`.
/// A row that agrees on id, TokenContract, price and ticks while disagreeing on either of those is a
/// different offer than the one the buyer was shown, and it used to be paid for silently.

/// Both sides of the comparison are read the same way, one after the other, through
/// `model_buy_preflight_selection_once` -> the raw `getOrder` walk, so widening the comparison
/// cannot make an unchanged ask look changed: `getOrder` returns the stored `Order`, and neither
/// field moves for a live order id.

/// `owner_note`, `escrow` and `timestamp` stay OUT, and deliberately: they are not terms the buyer
/// executes against, and showed that comparing the whole row makes a benign non-atomic reread
/// look like a race.
pub fn ensure_pre_submit_quote_unchanged(
    quoted_order: Option<&OrderBookOrder>,
    selected: &OrderBookOrder,
) -> Result<(), ChainError> {
    if quoted_order.is_some_and(|quoted| {
        quoted.order_id == selected.order_id
            && quoted.token_contract == selected.token_contract
            && quoted.price_per_tick == selected.price_per_tick
            && quoted.ticks == selected.ticks
            && quoted.deadline == selected.deadline
            && quoted.flags == selected.flags
    }) {
        return Ok(());
    }
    Err(ChainError::Chain(
        "buyer pre-submit matcher head differs from the rendered quote; no escrow was sent"
            .to_string(),
    ))
}

/// Parsed `InferenceOrderBook.getStats()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookStats {
    pub next_order_id: u128,
    pub order_count: u128,
    pub executed_notional: u128,
    pub executed_ticks: u128,
}

/// Read-only snapshot of one model's `InferenceOrderBook`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    pub frame_model: String,
    pub model_hash: String,
    #[serde(with = "crate::address::serde_canonical")]
    pub order_book: String,
    pub stats: Option<OrderBookStats>,
    pub orders: Vec<OrderBookOrder>,
}

impl OrderBookSnapshot {
    pub fn active(&self) -> bool {
        self.stats.is_some()
    }

    pub fn resting_asks(&self) -> impl Iterator<Item = &OrderBookOrder> {
        self.orders.iter().filter(|o| o.is_resting_ask())
    }

    /// The resting asks a buy could still reach at `now_unix` -- the set every executable-scope view
    /// must draw from.
    pub fn live_resting_asks_at(&self, now_unix: u64) -> impl Iterator<Item = &OrderBookOrder> {
        self.orders
            .iter()
            .filter(move |o| o.is_live_resting_ask_at(now_unix))
    }
}

/// Order flags accepted by `InferenceOrderBook` (`SUPPORTED_FLAGS`).

/// The low bits select taker behaviour and are mutually exclusive with resting; the high bits describe the
/// SHAPE of the resulting deal and are forwarded verbatim into the `TokenContract` (`DEAL_FLAGS_MASK`).
pub mod flags {
    /// Immediate-or-cancel: fill what crosses now, refund the rest.
    pub const IOC: u8 = 0x01;
    /// Fill-or-kill: all of it now, from any number of counterparties, or nothing.
    pub const FOK: u8 = 0x02;
    /// Market order: no price limit.
    pub const MARKET: u8 = 0x04;
    /// Never take -- reject the order if it would cross on arrival.
    pub const POST_ONLY: u8 = 0x08;
    /// Deal shape: the seller attests a trusted-execution endpoint.
    pub const TEE: u8 = 0x10;
    /// All-or-none from a SINGLE counterparty. Unlike [`FOK`] an AON order may rest until one
    /// counterparty can take the whole size.
    pub const AON: u8 = 0x20;
    /// Deal shape: take-or-pay subscription. Requires [`AON`] and a non-zero week count, because a
    /// subscription reserves capacity from exactly one seller for its whole term.
    pub const SUBSCRIPTION: u8 = 0x40;

    /// Flags forwarded into the `TokenContract` as the deal shape.
    pub const DEAL_MASK: u8 = TEE | SUBSCRIPTION;
}

/// Parsed `TokenContract.getSubscription()` -- the deal SHAPE, not a book-side subscription.

/// `sub_weeks == 0` marks an ordinary by-fact deal, in which case the weekly fields carry no meaning
/// beyond `tokens_per_week == fundedTokens` (the whole volume, available from the start).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealSubscription {
    /// Raw deal-shape flags (`flags::DEAL_MASK`) as recorded at funding.
    pub deal_flags: u8,
    /// Term in weeks; zero for an ordinary deal.
    pub sub_weeks: u8,
    /// Weeks already settled take-or-pay.
    pub week_index: u8,
    /// Per-week allowance. Does not roll forward: unused volume is forfeited at the boundary.
    pub tokens_per_week: u128,
    /// Whole funded volume of the deal.
    pub funded_tokens: u128,
    /// Cumulative tokens already paid for (whole quotas for settled weeks).
    pub tokens_paid: u128,
    /// When the weekly clock started -- accepted-probe time for a subscription.
    pub period_start: u64,
    /// Cumulative consumption recorded at the start of the current week. This is the authoritative
    /// no-rollover base used by the contract; it cannot be reconstructed from wall-clock time or
    /// `tokens_paid`.
    pub week_base_tokens: u128,
}

impl DealSubscription {
    /// Strictly decode the exact `TokenContract.getSubscription()` ABI.

    /// Getter integers are ABI decimal strings. Missing fields, alternate JSON kinds, non-decimal
    /// values, width overflow, unknown deal-shape bits and contradictory ordinary/subscription
    /// shapes are rejected rather than being interpreted as an ordinary zero-valued deal.
    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        const GETTER: &str = "getSubscription()";
        getter_exact_object(
            value,
            GETTER,
            &[
                "dealFlags",
                "subWeeks",
                "weekIndex",
                "tokensPerWeek",
                "fundedTokens",
                "tokensPaid",
                "periodStart",
                "weekBaseTokens",
            ],
        )?;

        let decoded = Self {
            deal_flags: getter_u8(value, GETTER, "dealFlags")?,
            sub_weeks: getter_u8(value, GETTER, "subWeeks")?,
            week_index: getter_u8(value, GETTER, "weekIndex")?,
            tokens_per_week: getter_u128(value, GETTER, "tokensPerWeek")?,
            funded_tokens: getter_u128(value, GETTER, "fundedTokens")?,
            tokens_paid: getter_u128(value, GETTER, "tokensPaid")?,
            period_start: getter_u64(value, GETTER, "periodStart")?,
            week_base_tokens: getter_u128(value, GETTER, "weekBaseTokens")?,
        };

        let unknown = decoded.deal_flags & !flags::DEAL_MASK;
        if unknown != 0 {
            return Err(format!(
                "{GETTER}.dealFlags contains unknown deal-shape bits 0x{unknown:02x}"
            ));
        }

        let subscription_flag = decoded.deal_flags & flags::SUBSCRIPTION != 0;
        if subscription_flag != (decoded.sub_weeks != 0) {
            return Err(format!(
                "{GETTER} has contradictory subscription flag/subWeeks shape: dealFlags=0x{:02x}, subWeeks={}",
                decoded.deal_flags, decoded.sub_weeks
            ));
        }
        if decoded.week_index > decoded.sub_weeks {
            return Err(format!(
                "{GETTER}.weekIndex {} exceeds subWeeks {}",
                decoded.week_index, decoded.sub_weeks
            ));
        }
        if decoded.tokens_paid > decoded.funded_tokens {
            return Err(format!(
                "{GETTER}.tokensPaid {} exceeds fundedTokens {}",
                decoded.tokens_paid, decoded.funded_tokens
            ));
        }
        if decoded.week_base_tokens > decoded.funded_tokens {
            return Err(format!(
                "{GETTER}.weekBaseTokens {} exceeds fundedTokens {}",
                decoded.week_base_tokens, decoded.funded_tokens
            ));
        }

        if subscription_flag {
            if decoded.sub_weeks != SUBSCRIPTION_WEEKS {
                return Err(format!(
                    "{GETTER}.subWeeks {} does not equal the canonical {}-week term",
                    decoded.sub_weeks, SUBSCRIPTION_WEEKS
                ));
            }
            let expected = decoded
                .tokens_per_week
                .checked_mul(u128::from(decoded.sub_weeks))
                .ok_or_else(|| format!("{GETTER}.tokensPerWeek x subWeeks overflows uint128"))?;
            if expected != decoded.funded_tokens {
                return Err(format!(
                    "{GETTER} subscription quota {} x {} does not equal fundedTokens {}",
                    decoded.tokens_per_week, decoded.sub_weeks, decoded.funded_tokens
                ));
            }
        } else if decoded.week_index != 0
            || decoded.tokens_per_week != decoded.funded_tokens
            || decoded.week_base_tokens != 0
        {
            return Err(format!(
                "{GETTER} ordinary deal has contradictory weekly state: weekIndex={}, tokensPerWeek={}, \
                 fundedTokens={}, weekBaseTokens={}",
                decoded.week_index,
                decoded.tokens_per_week,
                decoded.funded_tokens,
                decoded.week_base_tokens
            ));
        }

        Ok(decoded)
    }

    pub fn is_subscription(&self) -> bool {
        self.sub_weeks != 0
    }

    pub fn is_tee(&self) -> bool {
        self.deal_flags & flags::TEE != 0
    }

    /// Whole weeks not yet settled. These are the only ones a buyer `stop()` refunds -- the week in
    /// progress is charged in full (take-or-pay).
    pub fn weeks_remaining(&self) -> u8 {
        self.sub_weeks.saturating_sub(self.week_index)
    }

    /// Unix second at which the week recorded in [`Self::week_index`] runs out.

    /// Informational: it says when a client should go and BOOK the boundary, never that the boundary
    /// has been crossed. The contract measures `block.timestamp`; a client measures its own clock.
    pub fn recorded_week_expires_at(&self) -> u64 {
        if !self.is_subscription() || self.week_index >= self.sub_weeks {
            return u64::MAX;
        }
        self.period_start
            .saturating_add(u64::from(self.week_index).saturating_add(1) * SUB_WEEK_LEN.as_secs())
    }

    /// Whether the term is over according to the BOOKED weeks. Authoritative: `weekIndex` only moves
    /// when a week is actually charged.
    pub fn term_is_over(&self) -> bool {
        self.is_subscription() && self.week_index >= self.sub_weeks
    }
}

/// The cumulative claim ceiling implied by a SUBSCRIPTION's recorded weekly books -- `TokenContract._claimCap()`
/// evaluated against the state as stored, with no boundary of its own.

/// This is computed outside the contract, and how it stands to the ceiling the contract actually
/// applies has THREE phases -- not a sign:

/// 1. **No boundary crossed since the last booking -- EXACT.** The stored `weekBaseTokens` is the very
/// one the contract would use and the formula is the same, `weekBaseTokens + tokensPerWeek` clamped
/// by `fundedTokens`. There is no divergence at all in this phase.
/// 2. **A boundary crossed but not booked, term still running -- MAY UNDERSTATE.**
/// `_chargeWeeksThrough` raises `weekBaseTokens` to `max(tokensFinal, tokensPending)` at the
/// boundary, monotonically upward, and `claimTokens` books the crossed boundaries itself before it
/// measures the ceiling. Until someone books, this reads the smaller, older base. Non-strict:
/// booking a week nobody used re-bases onto the same cumulative and raises nothing.
/// 3. **Past the final boundary (`weekIndex >= subWeeks`) -- UPPER BOUND, may overstate.** The contract
/// stops deriving the ceiling from a quota and returns the cumulative total already declared; the
/// quota formula yields at least that, and a claim above it is refused. Non-strict: the two are
/// equal when the final week's quota was fully consumed.

/// One caveat, without which phase 2 reads too strong: inside the term the ceiling is also clamped by
/// `fundedTokens`, so near the end of a term the understated and the exact figures can coincide by
/// hitting that same clamp. That is not a fourth phase -- it is why the divergence disappears where the
/// phase alone would not predict it. Neither phase 2 nor phase 3 is ever a STRICT inequality.

/// The practical conclusion is the same in every phase: this may not be treated as a bound in either
/// direction without comparing `weekIndex` against `subWeeks`, and a client that needs something to
/// stand on calls the permissionless `settleWeek` and computes from the state that comes back rather
/// than guessing which phase it is in. The rule must be fail-closed: past the final boundary, refresh
/// the authoritative state and never carry a stale pre-boundary quota forward as authorization.
pub fn subscription_claim_cap_at(
    state: &DealChainState,
    subscription: &DealSubscription,
) -> Result<u128, String> {
    if !subscription.is_subscription() {
        // An ordinary deal has no weekly books: `weekBaseTokens`/`tokensPerWeek` are not fields it
        // maintains, so a figure computed from them would be a number rather than a ceiling. There is
        // no caller that wants one, and answering anyway is how a wrong one gets used.
        return Err(
            "deal is not a subscription; it has no weekly claim ceiling to compute".to_string(),
        );
    }
    if subscription.term_is_over() {
        // Past the final boundary the term sells no further capacity: the ceiling is the recorded
        // cumulative and admits no new claim. There is no fifth week of a four-week term.
        return Ok(state.tokens_pending);
    }
    subscription
        .week_base_tokens
        .checked_add(subscription.tokens_per_week)
        .map(|cap| cap.min(subscription.funded_tokens))
        .ok_or_else(|| "subscription weekBaseTokens + tokensPerWeek overflows uint128".to_string())
}

/// Tokens the recorded books still admit on top of the cumulative claim already declared. Zero means
/// the RECORDED week is drawn down -- not, on its own, that the deal is finished.
pub fn subscription_current_week_headroom(
    state: &DealChainState,
    subscription: &DealSubscription,
) -> Result<u128, String> {
    let cap = subscription_claim_cap_at(state, subscription)?;
    cap.checked_sub(state.tokens_pending).ok_or_else(|| {
        format!(
            "subscription cumulative claim {} exceeds the recorded week claim ceiling {cap}",
            state.tokens_pending
        )
    })
}

/// A single maker order consumed by an executable quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteFill {
    pub order_id: u128,
    #[serde(with = "crate::address::serde_self_dapp")]
    pub token_contract: TokenContract,
    pub ticks: u128,
    pub price_per_tick: u128,
    pub cost_with_fee: u128,
}

/// Buyer-visible fill details returned after a model-only buy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchedFill {
    /// The receiving owner's authoritative order id from `InferenceFilledConfirmed`:
    /// `buyerOrderId` for the buyer event and `sellerOrderId` for the seller event.
    pub order_id: u128,
    #[serde(with = "crate::address::serde_self_dapp")]
    pub token_contract: TokenContract,
    pub ticks: u128,
    pub price_per_tick: u128,
}

/// Accepted subscription placement fact from the model order book.

/// A subscription is no longer a distinct book primitive: it is an ordinary BUY order carrying
/// `flags::AON | flags::SUBSCRIPTION` plus a week count, so this is reconciled from the same
/// `InferenceOrderPlaced` fact as any other buy. The order id is the durable correlation key for later
/// owner-facing fills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceSubscriptionPlacement {
    pub order_id: u128,
    #[serde(with = "crate::address::serde_canonical")]
    pub buyer_note: String,
    pub max_price_per_tick: u128,
    /// Whole term volume in ticks. Divides evenly by `sub_weeks` -- the book enforces it.
    pub ticks: u128,
    /// Term in weeks; always [`SUBSCRIPTION_WEEKS`] under the current protocol.
    pub sub_weeks: u8,
    /// Mandatory order deadline: an unmatched subscription is expirable by anyone afterwards.
    pub deadline: u64,
    pub created_at: i64,
}

/// One owner-facing `InferenceOrderBook` fact about a BUY order a note submitted.

/// The book emits a distinct event per outcome, so a durable buyer submit record is resolved by the fact
/// that actually happened and never by a clock. `Cancelled` and `Rejected` are terminal -- the order is gone
/// and its money came back. `Expired` is NOT: an order past its deadline can still be matched and settled,
/// or still hold escrow, so it is reported and the record is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuyerOrderFact {
    /// Wall-clock `created_at` of the ext-out message that carried the event.
    pub created_at: i64,
    /// The note the book named as the order's owner.
    #[serde(with = "crate::address::serde_canonical")]
    pub note: String,
    pub kind: BuyerOrderFactKind,
}

/// The book event behind a [`BuyerOrderFact`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuyerOrderFactKind {
    /// `InferenceOrderPlaced` -- the book accepted the buy and assigned it this order id.
    Placed {
        order_id: u128,
        price_per_tick: u128,
        ticks: u128,
        deadline: u64,
    },
    /// `InferenceOrderCancelled` -- the order left the book and `refunded` returned to the note.
    Cancelled { order_id: u128, refunded: u128 },
    /// `InferenceOrderExpired` -- the order's deadline was swept. Reported, never terminal for the record.
    Expired { order_id: u128 },
    /// `InferenceRefunded` -- `amount` came back to the note for an order that already carried an id.

    /// The book emits this ALONGSIDE the reason the order left -- `InferenceOrderBook.sol:387-393`
    /// states it outright: "An expiring bid emits this alongside `InferenceOrderExpired` -- the
    /// refund and the reason are separate facts." Keeping them separate here is the point:
    /// a sweep proves removal, this proves the money came back, and only both together prove an
    /// order stopped holding a buyer's escrow.
    Refunded { order_id: u128, amount: u128 },
    /// `InferenceOrderRejected` -- the book refused the submission, so no order id was ever assigned and
    /// `refund` returned to the note.
    Rejected { reason: u8, refund: u128 },
}

impl BuyerOrderFact {
    /// The order id this fact names, when the book had already assigned one.
    pub fn order_id(&self) -> Option<u128> {
        match self.kind {
            BuyerOrderFactKind::Placed { order_id, .. }
            | BuyerOrderFactKind::Cancelled { order_id, .. }
            | BuyerOrderFactKind::Expired { order_id }
            | BuyerOrderFactKind::Refunded { order_id, .. } => Some(order_id),
            BuyerOrderFactKind::Rejected { .. } => None,
        }
    }
}

/// Read-only quote result over current resting asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableQuote {
    pub filled_ticks: u128,
    pub total_with_fee: u128,
    pub complete: bool,
    pub fills: Vec<QuoteFill>,
}

/// Match result: the seller sees the buyer's recorded pubkey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Match {
    #[serde(with = "crate::address::serde_self_dapp")]
    pub token_contract: TokenContract,
    pub buyer_pubkey: NotePubkey,
    pub price_per_tick: Shell,
}

/// Durable source cursor for a seller gateway match watcher.

/// The concrete source may be note ext-out events (a real chain) or an equivalent
/// authoritative state source (mock / direct TC state). The cursor is intentionally
/// small and secret-free so the CLI can persist it next to local deal handles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchWatchCursor {
    /// Ignore source events older than this wall-clock timestamp.
    pub since_unix: i64,
    /// Highest `created_at` timestamp already consumed from the source.
    pub last_seen_created_at: Option<i64>,
    /// TokenContracts consumed at `last_seen_created_at`, for same-second events.
    #[serde(default, with = "crate::address::serde_self_dapp_vec")]
    pub seen_token_contracts_at_last_seen: Vec<TokenContract>,
}

impl MatchWatchCursor {
    pub fn new(since_unix: i64) -> Self {
        Self {
            since_unix,
            last_seen_created_at: None,
            seen_token_contracts_at_last_seen: Vec::new(),
        }
    }

    pub fn has_seen(&self, created_at: i64, token_contract: &str) -> bool {
        if created_at < self.since_unix {
            return true;
        }
        match self.last_seen_created_at {
            Some(last) if created_at < last => true,
            Some(last) if created_at == last => self
                .seen_token_contracts_at_last_seen
                .iter()
                .any(|tc| tc.eq_ignore_ascii_case(token_contract)),
            _ => false,
        }
    }

    pub fn record_seen_batch<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = (i64, TokenContract)>,
    {
        let mut max_seen = self.last_seen_created_at;
        let mut at_max = if max_seen.is_some() {
            self.seen_token_contracts_at_last_seen.clone()
        } else {
            Vec::new()
        };
        for (created_at, token_contract) in events {
            if created_at < self.since_unix {
                continue;
            }
            match max_seen {
                Some(max) if created_at < max => {}
                Some(max) if created_at == max => {
                    if !at_max
                        .iter()
                        .any(|tc| tc.eq_ignore_ascii_case(&token_contract))
                    {
                        at_max.push(token_contract);
                    }
                }
                _ => {
                    max_seen = Some(created_at);
                    at_max.clear();
                    at_max.push(token_contract);
                }
            }
        }
        if let Some(max) = max_seen {
            self.last_seen_created_at = Some(max);
            at_max.sort();
            at_max.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            self.seen_token_contracts_at_last_seen = at_max;
        }
    }
}

/// Backend errors.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error(
        "no match for token_contract {display}",
        display = crate::address::display_self_dapp(.0)
    )]
    NoMatch(TokenContract),
    #[error(
        "no stream open for {display}",
        display = crate::address::display_self_dapp(.0)
    )]
    NoStream(TokenContract),
    #[error("endpoints file: {0}")]
    EndpointsFile(String),
    /// Error from the real on-chain adapter: submit/getter/chain provisioning.
    #[error("{net}: {0}", net = crate::params::current_network())]
    Chain(String),
    /// The RPC/HTTP transport failed before a by-fact chain result was available.
    #[error("{net} transport: {0}", net = crate::params::current_network())]
    Transport(String),
    /// The chain returned a contract-level refusal/revert.
    #[error("{net} contract: {0}", net = crate::params::current_network())]
    Contract(String),
    /// The order book returned the seller placement value because this TC already has a resting SELL.
    #[error("{0}")]
    DuplicateSell(String),
    /// A non-idempotent money POST may have reached the chain, but its result is not yet provable.
    #[error("{net} ambiguous submit: {0}", net = crate::params::current_network())]
    AmbiguousSubmit(String),
    /// A non-idempotent money write failed before its POST was attempted.
    #[error("{net} money submit was not posted: {0}", net = crate::params::current_network())]
    MoneySubmitPreparation(String),
    /// A non-idempotent money POST returned a decoded protocol/contract rejection.
    #[error("{net} money submit was rejected: {0}", net = crate::params::current_network())]
    MoneySubmitRejected(String),
    /// The agreed deal limit was exceeded (e.g. the offer's `max_ticks`). The real TC bounds it
    /// by deposit; the mock holds the same invariant with a guard.
    #[error("deal limit exceeded: {0}")]
    Limit(String),
    /// A claim retry found that the authoritative cumulative high-water has already advanced beyond
    /// the value this process intended to submit. The seller driver must resynchronise to `on_chain`
    /// instead of treating the stale local value as applied.
    #[error(
        "claim high-water advanced on-chain to {on_chain}, beyond attempted cumulative {attempted}"
    )]
    ClaimHighWaterResync { attempted: u128, on_chain: u128 },
}

/// Snapshot of the stream's state in the mock (for e2e acceptance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSnapshot {
    /// Seller bond held by this TokenContract.
    pub seller_locked: u128,
    /// Total buyer funds still held (`deposit + probe_tick + subscription buyer bond`).
    pub buyer_locked: u128,
    /// The buyer's at-risk lead: an unaccepted probe plus the monetary value of the unpromoted claim tail.
    /// This is distinct from `buyer_locked`, which also carries unspent escrow for the rest of the deal.
    pub buyer_lead: u128,
    /// Authoritative immutable count of finalized model tokens delivered by this TokenContract.
    pub tokens_final: u128,
    /// SHELL owed or paid to the seller. This is money, not delivered-token volume.
    pub seller_received: u128,
    /// Refunded to the buyer.
    pub buyer_refunded: u128,
    /// Total SHELL burned for the contract.
    pub burned: u128,
    /// Stream terminal/STOPped according to the TokenContract lifecycle.

    /// This is not `!opened`: funded-but-never-opened and disputed TCs can hold escrow while still active.
    pub closed: bool,
}

/// Exact typed by-fact lifecycle read for a live `TokenContract`.

/// This mirrors every field of the current 13-field `getState()` ABI so no consumer has to inspect raw JSON
/// or reconstruct a missing claim stage or timeout anchor.

/// The claim pipeline is two-stage in contracts 4.0.35 (`TokenContract.claimTokens`): `tokens_final` is
/// promoted and irrevocably the seller's, and `tokens_pending` is the single contestable cumulative claim.
/// Contracts 4.0.34 carried a third slot with its own timestamp (`tokensSuperseded`/`prevClaimTime`); 4.0.35
/// deleted both, because `CLAIM_PROMOTE_WINDOW` now equals `MIN_CLAIM_INTERVAL`, so the next claim always
/// arrives with the previous one already ripe and at most one tick can be unpromoted. `tokens_pending` is
/// still not earned -- only `tokens_final` is money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealChainState {
    pub funded: bool,
    pub opened: bool,
    /// The probe tick has been accepted, which is what makes the deal claimable at all. Before this the
    /// seller cannot claim anything and a buyer STOP burns the trial tick on both sides.
    pub probe_accepted: bool,
    pub disputed: bool,
    /// Buyer escrow still held by the deal. Zeroed by every terminal path, which is what distinguishes a
    /// settled close from funded-but-never-opened (both have `opened=false`).
    pub deposit: u128,
    /// Withdrawable SHELL already credited to the seller.
    pub finalized_owed: u128,
    /// Promoted cumulative consumption -- the only figure money is computed from.
    pub tokens_final: u128,
    /// The one claimed cumulative consumption still inside its contest window.
    pub tokens_pending: u128,
    /// SHELL held as the unaccepted probe. Owed to nobody: it either becomes the seller's on acceptance, or
    /// burns with a mirror slice of the bond if the buyer walks away from the trial, or returns to the escrow
    /// on any other close.
    pub probe_tick: u128,
    pub funded_time: Option<u64>,
    /// When the probe was frozen (at `open()`); anchors `PROBE_WINDOW`.
    pub probe_time: u64,
    /// Anchor for both claim bounds (`MIN_CLAIM_INTERVAL`) and permissionless promotion
    /// (`CLAIM_PROMOTE_WINDOW`). Set at `open()`, then at every accepted claim.
    pub last_claim_time: u64,
    /// When the buyer opened a dispute; anchors `DISPUTE_WINDOW` for `resolveDisputeTimeout`.
    pub dispute_time: u64,
}

impl DealChainState {
    /// The exact `TokenContract.getState()` output field list this decoder is written for, in the
    /// order the compiled ABI declares it. Named once so the shape pin
    /// (`the_deal_state_decoder_matches_the_compiled_getstate`) can assert this list against
    /// `TokenContract.abi.json` itself rather than against a second hand-written copy of it.
    pub(crate) const GET_STATE_FIELDS: &'static [&'static str] = &[
        "funded",
        "opened",
        "probeAccepted",
        "disputed",
        "deposit",
        "probeTick",
        "finalizedOwed",
        "tokensFinal",
        "tokensPending",
        "probeTime",
        "lastClaimTime",
        "disputeTime",
        "fundedTime",
    ];

    /// Strictly decode the exact 13-field `TokenContract.getState()` ABI.
    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        const GETTER: &str = "getState()";
        getter_exact_object(value, GETTER, Self::GET_STATE_FIELDS)?;

        let funded_time = getter_u64(value, GETTER, "fundedTime")?;
        let decoded = Self {
            funded: getter_bool(value, GETTER, "funded")?,
            opened: getter_bool(value, GETTER, "opened")?,
            probe_accepted: getter_bool(value, GETTER, "probeAccepted")?,
            disputed: getter_bool(value, GETTER, "disputed")?,
            deposit: getter_u128(value, GETTER, "deposit")?,
            probe_tick: getter_u128(value, GETTER, "probeTick")?,
            finalized_owed: getter_u128(value, GETTER, "finalizedOwed")?,
            tokens_final: getter_u128(value, GETTER, "tokensFinal")?,
            tokens_pending: getter_u128(value, GETTER, "tokensPending")?,
            probe_time: getter_u64(value, GETTER, "probeTime")?,
            last_claim_time: getter_u64(value, GETTER, "lastClaimTime")?,
            dispute_time: getter_u64(value, GETTER, "disputeTime")?,
            funded_time: (funded_time != 0).then_some(funded_time),
        };

        if decoded.tokens_final > decoded.tokens_pending {
            return Err(format!(
                "{GETTER} claim pipeline is not monotonic: tokensFinal={} tokensPending={}",
                decoded.tokens_final, decoded.tokens_pending
            ));
        }
        Ok(decoded)
    }

    /// Full unpromoted claim tail (`pending - final`) for monitoring exposure. The contract computes the
    /// actual bounded dispute stake separately; callers must not treat this token count as that SHELL amount.
    pub fn contested_tokens(self) -> u128 {
        self.tokens_pending.saturating_sub(self.tokens_final)
    }

    /// Whether the deal is still on the probe: opened but not yet accepted, so nothing is claimable and a
    /// buyer STOP burns the trial tick rather than settling by fact.
    pub fn on_probe(self) -> bool {
        self.opened && !self.probe_accepted
    }

    /// Match `dexdo status` lifecycle semantics for a STOPped/settled deal.

    /// `opened=false` alone is not terminal: a matched buyer can leave the TC in
    /// funded-but-never-opened, and a dispute can also hold escrow without being
    /// a clean closed settlement. Every terminal path zeroes both the deposit and the probe, while a
    /// funded-but-never-opened deal still holds the buyer's whole escrow -- so a drained escrow is what tells
    /// the two apart.
    pub fn is_stopped(self) -> bool {
        self.funded && !self.opened && !self.disputed && self.deposit == 0 && self.probe_tick == 0
    }

    /// Why this per-deal `TokenContract` is already USED, if it is.

    /// A `(sellerPubkey, nonce)` TC is a single-use deal slot, not reusable capacity: once anything
    /// below is true its `maxTicks` are committed to a buyer and no ask may be posted against them.
    /// `None` means every field is still at its constructor value, so the TC's whole `getDeal().maxTicks`
    /// is unsold -- which is what makes it the authoritative remaining capacity for an expiry relist.
    pub fn used_reason(self) -> Option<String> {
        let mut used = Vec::new();
        if self.opened {
            used.push("opened".to_string());
        }
        if self.funded {
            used.push("funded".to_string());
        }
        if self.disputed {
            used.push("disputed".to_string());
        }
        if self.probe_accepted {
            used.push("probeAccepted".to_string());
        }
        for (field, value) in [
            ("deposit", self.deposit),
            ("probeTick", self.probe_tick),
            ("finalizedOwed", self.finalized_owed),
            ("tokensFinal", self.tokens_final),
            ("tokensPending", self.tokens_pending),
        ] {
            if value > 0 {
                used.push(format!("{field}={value}"));
            }
        }
        if let Some(funded_time) = self.funded_time {
            used.push(format!("fundedTime={funded_time}"));
        }
        for (field, value) in [
            ("probeTime", self.probe_time),
            ("lastClaimTime", self.last_claim_time),
            ("disputeTime", self.dispute_time),
        ] {
            if value > 0 {
                used.push(format!("{field}={value}"));
            }
        }
        (!used.is_empty()).then(|| used.join(", "))
    }
}

/// Strictly decoded `TokenContract.getOffer()` -- the deal's own offer latch.

/// `offer_posted` is the contract's `_offerPosted`: set when the TC posts its ask
/// (`contracts/airegistry/TokenContract.sol:713-714`) and cleared only when the book reports the ask
/// left WITHOUT a fill -- cancel or expiry -- through `onSellClosed`
/// (`contracts/airegistry/TokenContract.sol:729-736`). While it is set, `postFromNote` returns
/// without posting anything, so it is the single authoritative fact that says whether a successor
/// offer can rest at all.

/// The latch is a single flag. Contracts 4.0.34 carried a second one, `closing`, a seller wind-down
/// state in which the TC self-destructed inside the next `onSellClosed`; 4.0.35 deleted `_closing`
/// from the contract's state and from the ABI, because `close()` now refuses outright while an offer
/// is live instead of latching an intent and reporting success. There is no successor field:
/// the state it described is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealOfferLatch {
    pub offer_posted: bool,
}

impl DealOfferLatch {
    /// The exact `TokenContract.getOffer()` output field list, named once so the shape pin can
    /// assert it against the compiled ABI rather than against a second hand-written copy.
    pub(crate) const GET_OFFER_FIELDS: &'static [&'static str] = &["offerPosted"];

    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        const GETTER: &str = "getOffer()";
        getter_exact_object(value, GETTER, Self::GET_OFFER_FIELDS)?;
        Ok(Self {
            offer_posted: getter_bool(value, GETTER, "offerPosted")?,
        })
    }
}

/// Strictly decoded `TokenContract.getSellerBond()` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealSellerBond {
    pub bond_funded: bool,
    pub bond_held: u128,
    pub bond_required: u128,
}

impl DealSellerBond {
    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        const GETTER: &str = "getSellerBond()";
        getter_exact_object(value, GETTER, &["bondFunded", "bondHeld", "bondRequired"])?;
        Ok(Self {
            bond_funded: getter_bool(value, GETTER, "bondFunded")?,
            bond_held: getter_u128(value, GETTER, "bondHeld")?,
            bond_required: getter_u128(value, GETTER, "bondRequired")?,
        })
    }
}

/// Strictly decoded `TokenContract.getBuyerBond()` state.

/// The buyer bond is held apart from ordinary escrow on subscription deals.
/// Ordinary deals must expose the canonical zero/zero shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealBuyerBond {
    pub bond_held: u128,
    pub bond_required: u128,
}

impl DealBuyerBond {
    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        const GETTER: &str = "getBuyerBond()";
        getter_exact_object(value, GETTER, &["bondHeld", "bondRequired"])?;
        Ok(Self {
            bond_held: getter_u128(value, GETTER, "bondHeld")?,
            bond_required: getter_u128(value, GETTER, "bondRequired")?,
        })
    }
}

/// One coherent live accounting/lifecycle view of a `TokenContract`.

/// The account BOC identity brackets all four getters in every bounded read
/// attempt. A changed or destroyed identity rejects that attempt, so consumers
/// never combine fields from different contract revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealChainSnapshot {
    pub account_code_hash: String,
    pub account_boc_hash: String,
    pub state: DealChainState,
    pub subscription: DealSubscription,
    pub seller_bond: DealSellerBond,
    pub buyer_bond: DealBuyerBond,
}

impl DealChainSnapshot {
    /// Validate only relationships exposed authoritatively by the four getters
    /// in this same account revision.
    pub(crate) fn validate_cross_getter_invariants(&self) -> Result<(), String> {
        if self.state.funded {
            let minimum_funded_tokens = crate::params::TICK_SIZE * 2;
            if self.subscription.funded_tokens < minimum_funded_tokens {
                return Err(format!(
                    "funded deal requires at least two ticks ({minimum_funded_tokens} tokens) in \
                     getSubscription().fundedTokens, got {}",
                    self.subscription.funded_tokens
                ));
            }
        }

        let seller = self.seller_bond;
        if seller.bond_held > seller.bond_required {
            return Err(format!(
                "getSellerBond() bondHeld {} exceeds bondRequired {}",
                seller.bond_held, seller.bond_required
            ));
        }
        if !seller.bond_funded && seller.bond_held != 0 {
            return Err(format!(
                "getSellerBond() reports bondFunded=false with non-zero bondHeld {}",
                seller.bond_held
            ));
        }
        if self.state.opened {
            if !self.state.funded {
                return Err("getState() reports opened=true with funded=false".to_string());
            }
            if !seller.bond_funded
                || seller.bond_required == 0
                || seller.bond_held != seller.bond_required
            {
                return Err(format!(
                    "opened deal requires a fully funded non-zero seller bond, got \
                     bondFunded={} bondHeld={} bondRequired={}",
                    seller.bond_funded, seller.bond_held, seller.bond_required
                ));
            }
        }

        // The two bond getters are NOT symmetric, and reading them as if they were is what made this
        // check refuse a legitimate 4.0.35 deal:

        // getSellerBond() -> (_sellerBondFunded, _sellerBond, _bondAmount())
        // getBuyerBond() -> (_buyerBond, _isSubscription() ? _bondAmount(): 0)

        // So `bondRequired` on the BUYER side is not "how much this deal needs from the buyer" -- it
        // is zero on an ordinary deal unconditionally and forever, by that ternary, even while the
        // deal holds a buyer bond. Contracts 4.0.35 posts one on EVERY buy fill: `PrivateNote`
        // sends `fundBuyerBond(2 * clearingPrice)` gated on `isBuy` with no subscription test, and
        // `TokenContract.fundBuyerBond` accepts it on any deal. `bondHeld > bondRequired` is
        // therefore the NORMAL shape of a funded ordinary deal, not an incoherent read.

        // What is genuinely invariant is that both getters name the same contract quantity,
        // `_bondAmount()`, and the seller side reports it unconditionally -- so the seller's
        // `bondRequired` is the deal's bond size whichever kind of deal it is.
        let buyer = self.buyer_bond;
        let expected_buyer_required = if self.subscription.is_subscription() {
            seller.bond_required
        } else {
            0
        };
        if buyer.bond_required != expected_buyer_required {
            return Err(format!(
                "getBuyerBond() bondRequired {} is not the shape getBuyerBond() can report for this \
                 deal: a subscription must mirror getSellerBond().bondRequired {} and an ordinary \
                 deal must report 0 (subscription={})",
                buyer.bond_required,
                seller.bond_required,
                self.subscription.is_subscription()
            ));
        }
        // `fundBuyerBond` stores exactly `_bondAmount()` and refunds the excess, and no path ever
        // raises `_buyerBond` again -- the terminal paths only decrement it. So the held figure can
        // never exceed the deal's bond size as the seller getter reports it, on either kind of deal.
        // This is the bound that replaces the broken comparison, and it still catches a bond larger
        // than the contract could have taken.
        if buyer.bond_held > seller.bond_required {
            return Err(format!(
                "getBuyerBond() bondHeld {} exceeds the deal's bond size {} \
                 (getSellerBond().bondRequired, which is _bondAmount() on both sides)",
                buyer.bond_held, seller.bond_required
            ));
        }
        if !self.subscription.is_subscription() {
            return Ok(());
        }
        let live_funded = self.state.funded && !self.state.is_stopped();
        if live_funded && (buyer.bond_required == 0 || buyer.bond_held != buyer.bond_required) {
            return Err(format!(
                "live funded subscription requires a fully held non-zero buyer bond, got \
                 bondHeld={} bondRequired={}",
                buyer.bond_held, buyer.bond_required
            ));
        }
        Ok(())
    }

    pub fn buyer_locked(&self) -> Result<u128, String> {
        self.state
            .deposit
            .checked_add(self.state.probe_tick)
            .and_then(|total| total.checked_add(self.buyer_bond.bond_held))
            .ok_or_else(|| {
                "getState().deposit + getState().probeTick + getBuyerBond().bondHeld exceeds uint128"
                    .to_string()
            })
    }
}

/// One exact raw `uint128` value. JSON uses a decimal string so values above `u64::MAX`
/// remain lossless for machine consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RawUint128(pub u128);

impl From<u128> for RawUint128 {
    fn from(value: u128) -> Self {
        Self(value)
    }
}

impl fmt::Display for RawUint128 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for RawUint128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for RawUint128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse::<u128>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// The exact write requested by a production settlement action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementAction {
    BuyerStop,
    SellerStop,
    Dispute,
    ReleaseDispute,
    ResolveDisputeTimeout,
}

impl fmt::Display for SettlementAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::BuyerStop => "buyer_stop",
            Self::SellerStop => "seller_stop",
            Self::Dispute => "dispute",
            Self::ReleaseDispute => "release_dispute",
            Self::ResolveDisputeTimeout => "resolve_dispute_timeout",
        };
        formatter.write_str(value)
    }
}

/// Authoritative event payload accepted for one production settlement action.

/// Literal contract field names are retained. In particular `toSeller` includes returned
/// collateral where the contract says so and is never relabelled as earned model payment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_kind", rename_all = "snake_case")]
pub enum SettlementActionEvent {
    ProbeBurned {
        buyer: String,
        #[serde(rename = "burnedProbe")]
        burned_probe: RawUint128,
        #[serde(rename = "burnedBond")]
        burned_bond: RawUint128,
        #[serde(rename = "refundToBuyer")]
        refund_to_buyer: RawUint128,
    },
    StreamStopped {
        buyer: String,
        #[serde(rename = "toSeller")]
        to_seller: RawUint128,
        #[serde(rename = "refundToBuyer")]
        refund_to_buyer: RawUint128,
    },
    StreamDisputed {
        buyer: String,
        at: u64,
    },
    DisputeResolved {
        #[serde(rename = "toSeller")]
        to_seller: RawUint128,
        #[serde(rename = "refundToBuyer")]
        refund_to_buyer: RawUint128,
        released: bool,
    },
}

/// Strict collateral facts captured immediately before the one action POST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementActionBondState {
    #[serde(rename = "sellerBondHeld")]
    pub seller_bond_held: RawUint128,
    #[serde(rename = "sellerBondRequired")]
    pub seller_bond_required: RawUint128,
    #[serde(rename = "buyerBondHeld")]
    pub buyer_bond_held: RawUint128,
    #[serde(rename = "buyerBondRequired")]
    pub buyer_bond_required: RawUint128,
}

/// Strict getter facts read after the event from an active `TokenContract`.

/// `None` on [`SettlementActionReceipt::post_state`] means the account was already destroyed;
/// immutable event history remains authoritative, but getter-only fields are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementActionPostState {
    #[serde(rename = "tokensFinal")]
    pub tokens_final: RawUint128,
    #[serde(rename = "tokensPending")]
    pub tokens_pending: RawUint128,
    #[serde(rename = "sellerBondHeld")]
    pub seller_bond_held: RawUint128,
    #[serde(rename = "sellerBondRequired")]
    pub seller_bond_required: RawUint128,
    #[serde(rename = "buyerBondHeld")]
    pub buyer_bond_held: RawUint128,
    #[serde(rename = "buyerBondRequired")]
    pub buyer_bond_required: RawUint128,
    pub opened: bool,
    pub disputed: bool,
}

/// Small by-fact result returned by one real settlement write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementActionReceipt {
    pub token_contract: TokenContract,
    pub action: SettlementAction,
    pub message_id: String,
    pub created_at: u64,
    #[serde(flatten)]
    pub event: SettlementActionEvent,
    pub pre_bonds: SettlementActionBondState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_state: Option<SettlementActionPostState>,
}

/// What immutable chain history proves when a buyer STOP path sees a terminal deal but cannot
/// honestly return an action receipt for its own call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuyerStopTerminalFact {
    /// The terminal transaction consumed the exact STOP message produced by this invocation.
    SubmittedStop,
    /// The terminal receipt existed before this invocation could submit STOP.
    AlreadyClosed,
    /// This invocation submitted STOP, but the terminal transaction's inbound call does not prove
    /// that this STOP, rather than a racing permissionless or counterparty call, closed the deal.
    UnknownCloser,
}

impl fmt::Display for BuyerStopTerminalFact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SubmittedStop => "submitted_stop",
            Self::AlreadyClosed => "already_closed",
            Self::UnknownCloser => "unknown_closer",
        })
    }
}

/// Honest terminal evidence returned by a buyer STOP path. It is populated only from immutable
/// chain events and exact message identities, never from a client-written receipt or journal row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuyerStopTerminalReceipt {
    pub token_contract: TokenContract,
    pub fact: BuyerStopTerminalFact,
    /// Whether this invocation reached its one STOP POST. This is deliberately independent from
    /// close attribution: `true` does not mean that STOP won a race.
    pub stop_submitted: bool,
    pub message_id: String,
    pub created_at: u64,
    #[serde(flatten)]
    pub event: SettlementActionEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_bonds: Option<SettlementActionBondState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_state: Option<SettlementActionPostState>,
}

impl BuyerStopTerminalReceipt {
    pub fn unknown_closer(receipt: SettlementActionReceipt) -> Self {
        Self {
            token_contract: receipt.token_contract,
            fact: BuyerStopTerminalFact::UnknownCloser,
            stop_submitted: true,
            message_id: receipt.message_id,
            created_at: receipt.created_at,
            event: receipt.event,
            pre_bonds: Some(receipt.pre_bonds),
            post_state: receipt.post_state,
        }
    }
}

impl fmt::Display for BuyerStopTerminalReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "token_contract={} fact={} stop_submitted={} message_id={} created_at={} event={:?}",
            self.token_contract,
            self.fact,
            self.stop_submitted,
            self.message_id,
            self.created_at,
            self.event
        )
    }
}

impl fmt::Display for SettlementActionReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "token_contract={} action={} message_id={} created_at={}",
            crate::address::display_self_dapp(&self.token_contract),
            self.action,
            self.message_id,
            self.created_at
        )?;
        match &self.event {
            SettlementActionEvent::ProbeBurned {
                buyer,
                burned_probe,
                burned_bond,
                refund_to_buyer,
            } => write!(
                formatter,
                " event_kind=probe_burned buyer={} burnedProbe={burned_probe} burnedBond={burned_bond} \
                 refundToBuyer={refund_to_buyer}",
                crate::address::display(buyer)
            )?,
            SettlementActionEvent::StreamStopped {
                buyer,
                to_seller,
                refund_to_buyer,
            } => write!(
                formatter,
                " event_kind=stream_stopped buyer={} toSeller={to_seller} \
                 refundToBuyer={refund_to_buyer}",
                crate::address::display(buyer)
            )?,
            SettlementActionEvent::StreamDisputed { buyer, at } => {
                write!(
                    formatter,
                    " event_kind=stream_disputed buyer={} at={at}",
                    crate::address::display(buyer)
                )?
            }
            SettlementActionEvent::DisputeResolved {
                to_seller,
                refund_to_buyer,
                released,
            } => write!(
                formatter,
                " event_kind=dispute_resolved toSeller={to_seller} \
                 refundToBuyer={refund_to_buyer} released={released}"
            )?,
        }
        write!(
            formatter,
            " preSellerBondHeld={} preSellerBondRequired={} preBuyerBondHeld={} \
             preBuyerBondRequired={}",
            self.pre_bonds.seller_bond_held,
            self.pre_bonds.seller_bond_required,
            self.pre_bonds.buyer_bond_held,
            self.pre_bonds.buyer_bond_required
        )?;
        match &self.post_state {
            Some(state) => write!(
                formatter,
                " tokensFinal={} tokensPending={} sellerBondHeld={} \
                 sellerBondRequired={} buyerBondHeld={} buyerBondRequired={} opened={} disputed={}",
                state.tokens_final,
                state.tokens_pending,
                state.seller_bond_held,
                state.seller_bond_required,
                state.buyer_bond_held,
                state.buyer_bond_required,
                state.opened,
                state.disputed
            ),
            None => formatter.write_str(" post_state=unavailable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        flags, order_deadline_is_live, DealBuyerBond, DealChainSnapshot, DealChainState,
        DealOfferLatch, DealSellerBond, DealSubscription, OrderBookOrder, OrderBookSnapshot,
        SettlementAction, SettlementActionBondState, SettlementActionEvent,
        SettlementActionPostState, SettlementActionReceipt, SUBSCRIPTION_WEEKS,
    };
    use crate::TICK_SIZE;
    use proptest::prelude::*;
    use serde_json::{json, Value};

    /// The ordered `(name, type)` list a compiled ABI declares for one side of one function.
    fn params(abi: &str, function: &str, side: &str) -> Vec<(String, String)> {
        let abi: Value = serde_json::from_str(abi).expect("parse compiled ABI");
        abi["functions"]
            .as_array()
            .expect("compiled ABI functions[]")
            .iter()
            .find(|declared| declared["name"] == function)
            .unwrap_or_else(|| panic!("the compiled ABI declares {function}"))[side]
            .as_array()
            .expect("compiled ABI parameter list")
            .iter()
            .map(|param| {
                (
                    param["name"].as_str().unwrap_or_default().to_string(),
                    param["type"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// The same list, names only -- the shape a strict decoder's field list is compared against.
    fn names(abi: &str, function: &str, side: &str) -> Vec<String> {
        params(abi, function, side)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, kind)| ((*name).to_string(), (*kind).to_string()))
            .collect()
    }

    /// shape pin: the deal's offer latch and the permissionless sweep, read out of the compiled
    /// artifacts this workspace ships rather than out of a hand-written signature.

    /// `getOffer()` is the only authoritative answer to "may a successor ask rest on this deal" --
    /// while `offerPosted` is set, `postFromNote` returns without posting and the seller gets no error
    /// -- and `expireOrder(orderId)` is the only way to clear an ask no taker ever crosses. A decoder
    /// frozen against a proposed shape passes every offline gate while being dead on chain, so the
    /// field list is asserted against the artifact and the strict decoder against the field list.
    #[test]
    fn the_offer_latch_and_the_permissionless_sweep_match_the_compiled_artifacts() {
        const TOKEN_CONTRACT_ABI: &str =
            include_str!("../../../../contracts/compiled/airegistry/TokenContract.abi.json");
        const ORDER_BOOK_ABI: &str =
            include_str!("../../../../contracts/compiled/airegistry/InferenceOrderBook.abi.json");

        assert_eq!(
            params(ORDER_BOOK_ABI, "expireOrder", "inputs"),
            owned(&[("orderId", "uint128")]),
            "the permissionless sweep takes the order id and nothing else -- no owner argument, which \
             is what makes it callable by a keeper, the counterparty or the seller itself"
        );
        assert_eq!(
            params(TOKEN_CONTRACT_ABI, "getOffer", "outputs"),
            owned(&[("offerPosted", "bool")]),
            "4.0.35 deleted _closing from the contract's state and from the ABI, so the deal's offer \
             latch is exactly this one flag"
        );
        assert_eq!(
            names(TOKEN_CONTRACT_ABI, "getOffer", "outputs"),
            DealOfferLatch::GET_OFFER_FIELDS,
            "the strict decoder's field list is the compiled getter's field list"
        );
        assert_eq!(
            DealOfferLatch::decode_getter(&json!({ "offerPosted": true }))
                .expect("the exact declared shape decodes"),
            DealOfferLatch { offer_posted: true }
        );
        assert!(
            DealOfferLatch::decode_getter(&json!({})).is_err(),
            "a latch read missing a field is not a latch this client may act on"
        );
        assert!(
            DealOfferLatch::decode_getter(&json!({
                "offerPosted": true,
                "closing": false,
            }))
            .is_err(),
            "the superseded 4.0.34 shape is not the getter this decoder was written for"
        );
    }

    /// The same discipline as the offer latch, for the getter every deal read goes through.

    /// `getState()` is not compile-coupled to anything: the decoder names its fields in a string
    /// list, so a shape that has moved on chain fails at RUNTIME, on every read, for buyer, keeper,
    /// monitor, audit and recover alike -- and nothing offline notices. Contracts 4.0.35 deleted
    /// `tokensSuperseded` and `prevClaimTime` when the claim pipeline collapsed from three slots to
    /// two, which took the getter from fifteen fields to thirteen. That is exactly the class this
    /// pin exists to catch, and there was no pin here when it happened.

    /// Asserted in both directions on purpose: the ordered `(name, type)` list against the artifact,
    /// so a renamed or retyped field is caught; and the decoder's own field list against the
    /// artifact's names, so the strict decoder can never be a generation apart from the ABI this
    /// workspace embeds.
    #[test]
    fn the_deal_state_decoder_matches_the_compiled_getstate() {
        const TOKEN_CONTRACT_ABI: &str =
            include_str!("../../../../contracts/compiled/airegistry/TokenContract.abi.json");

        assert_eq!(
            params(TOKEN_CONTRACT_ABI, "getState", "outputs"),
            owned(&[
                ("funded", "bool"),
                ("opened", "bool"),
                ("probeAccepted", "bool"),
                ("disputed", "bool"),
                ("deposit", "uint128"),
                ("probeTick", "uint128"),
                ("finalizedOwed", "uint128"),
                ("tokensFinal", "uint128"),
                ("tokensPending", "uint128"),
                ("probeTime", "uint64"),
                ("lastClaimTime", "uint64"),
                ("disputeTime", "uint64"),
                ("fundedTime", "uint64"),
            ]),
            "the deal state getter is exactly these thirteen fields"
        );
        assert_eq!(
            names(TOKEN_CONTRACT_ABI, "getState", "outputs"),
            DealChainState::GET_STATE_FIELDS,
            "the strict decoder's field list is the compiled getter's field list"
        );

        let declared = exact_state();
        assert!(
            DealChainState::decode_getter(&declared).is_ok(),
            "the exact declared shape decodes"
        );
        let mut superseded_shape = declared.clone();
        set_field(&mut superseded_shape, "tokensSuperseded", json!("0"));
        set_field(&mut superseded_shape, "prevClaimTime", json!("0"));
        assert!(
            DealChainState::decode_getter(&superseded_shape).is_err(),
            "the 4.0.34 fifteen-field shape is not the getter this decoder was written for"
        );
    }

    /// The incident, to the second: SELL 11's deadline and the moment it was still being
    /// offered as executable depth, 779 seconds later.
    const LAPSED_DEADLINE: u64 = 1_785_678_525;
    const OBSERVED_AT: u64 = 1_785_679_304;

    fn ask(order_id: u128, deadline: u64) -> OrderBookOrder {
        OrderBookOrder {
            order_id,
            owner_note: "0:seller".to_string(),
            token_contract: Some("0:tc".to_string()),
            is_buy: false,
            price_per_tick: 5_000_000_000,
            ticks: 956,
            escrow: 0,
            deadline,
            flags: 0,
            timestamp: 0,
        }
    }

    /// one predicate, and it matches the book's own `_isExpired` -- `deadline != 0 &&
    /// block.timestamp >= deadline`. The deadline second itself is already expired.
    #[test]
    fn the_deadline_predicate_matches_the_contract_boundary() {
        assert!(order_deadline_is_live(
            false,
            LAPSED_DEADLINE,
            LAPSED_DEADLINE - 1
        ));
        assert!(!order_deadline_is_live(
            false,
            LAPSED_DEADLINE,
            LAPSED_DEADLINE
        ));
        assert!(!order_deadline_is_live(false, LAPSED_DEADLINE, OBSERVED_AT));
    }

    /// A zero deadline reads by side: the contract permits a GTC bid, but a SELL commits no
    /// collateral and `PrivateNote` refuses `ttl == 0`, so a zero-deadline ask is malformed rather
    /// than immortal liquidity.
    #[test]
    fn a_zero_deadline_is_gtc_for_a_bid_and_malformed_for_an_ask() {
        assert!(order_deadline_is_live(true, 0, OBSERVED_AT));
        assert!(!order_deadline_is_live(false, 0, OBSERVED_AT));
    }

    /// Shape and liveness are separate questions: a lapsed row is still a well-formed resting ask,
    /// which is why a duplicate-TokenContract safety check stays deadline-blind while every view
    /// that reports executable depth does not.
    #[test]
    fn live_resting_asks_exclude_lapsed_rows_that_are_still_well_formed() {
        let snapshot = OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: vec![ask(11, LAPSED_DEADLINE), ask(12, OBSERVED_AT + 3_600)],
        };

        assert!(snapshot.orders[0].is_resting_ask());
        assert!(!snapshot.orders[0].is_live_resting_ask_at(OBSERVED_AT));
        assert_eq!(snapshot.resting_asks().count(), 2);
        assert_eq!(
            snapshot
                .live_resting_asks_at(OBSERVED_AT)
                .map(|order| order.order_id)
                .collect::<Vec<_>>(),
            vec![12]
        );
    }

    /// `deposit` carries the lifecycle distinction now: still-held escrow means the deal is live,
    /// a drained deposit means a terminal path already settled it.
    fn state(funded: bool, opened: bool, disputed: bool, deposit: u128) -> DealChainState {
        DealChainState {
            funded,
            opened,
            probe_accepted: true,
            disputed,
            deposit,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 0,
            funded_time: None,
            probe_time: 0,
            last_claim_time: 0,
            dispute_time: 0,
        }
    }

    /// monitor CLOSED semantics must match `dexdo status`: funded-never-opened,
    /// streaming, and disputed deals are active; only STOPped/settled is terminal.
    #[test]
    fn chain_state_stopped_semantics_match_status() {
        assert!(
            !state(true, false, false, 1_000).is_stopped(),
            "funded-but-never-opened still holds the whole escrow, so it is active"
        );
        assert!(
            !state(true, true, false, 1_000).is_stopped(),
            "an opened stream is active"
        );
        assert!(
            !state(true, true, false, 0).is_stopped(),
            "still opened is active even once the escrow is fully consumed"
        );
        assert!(
            !state(true, false, true, 1_000).is_stopped(),
            "disputed escrow is active, not cleanly closed"
        );
        assert!(
            !state(false, false, false, 0).is_stopped(),
            "unfunded/readable state is not a settled close"
        );
        assert!(
            state(true, false, false, 0).is_stopped(),
            "settled close drained the deposit -> status=stopped"
        );
    }

    /// A dispute is only ever about the tail the buyer has not yet had the window to accept.
    #[test]
    fn contested_tokens_is_the_unpromoted_tail() {
        let mut s = state(true, true, false, 1_000);
        s.tokens_final = 500;
        s.tokens_pending = 800;
        assert_eq!(
            s.contested_tokens(),
            300,
            "only claimed-minus-trusted is at stake"
        );

        s.tokens_pending = 500;
        assert_eq!(
            s.contested_tokens(),
            0,
            "nothing pending -> nothing to dispute"
        );

        // Terminal paths reset pending down to final; the tail must never read negative.
        s.tokens_pending = 400;
        assert_eq!(
            s.contested_tokens(),
            0,
            "a reset pending slot saturates at zero"
        );
    }

    fn exact_state() -> Value {
        json!({
            "funded": true,
            "opened": true,
            "probeAccepted": true,
            "disputed": false,
            "deposit": "100",
            "probeTick": "2",
            "finalizedOwed": "3",
            "tokensFinal": "10",
            "tokensPending": "30",
            "probeTime": "40",
            "lastClaimTime": "60",
            "disputeTime": "0",
            "fundedTime": "70"
        })
    }

    fn exact_subscription() -> Value {
        json!({
            "dealFlags": flags::SUBSCRIPTION.to_string(),
            "subWeeks": SUBSCRIPTION_WEEKS.to_string(),
            "weekIndex": "1",
            "tokensPerWeek": "100",
            "fundedTokens": "400",
            "tokensPaid": "100",
            "periodStart": "70",
            "weekBaseTokens": "80"
        })
    }

    /// One canonical week: two ticks. The book sells subscriptions in whole ticks divisible by
    /// `SUB_WEEKS` (`InferenceOrderBook.sol:1309`), so four ticks is the true minimum and eight -- two
    /// a week over four weeks -- is the smallest shape in which the probe's tick is part of a week
    /// rather than the whole of it.
    const WEEK: u128 = 2 * TICK_SIZE;
    /// The whole term's volume: `tokensPerWeek * SUB_WEEKS`, the only relation the contract funds.
    const FUNDED: u128 = WEEK * SUBSCRIPTION_WEEKS as u128;

    /// A four-week subscription in CANONICAL units -- real ticks, not a miniature scale of its own.

    /// `tokens_paid` is the money mark, not a consumption figure. `acceptProbe` seeds it with one
    /// `TICK_SIZE` and `_chargeWeeksThrough` raises it to `(weekIndex + 1) * tokensPerWeek` at every
    /// boundary it books, so after booking to week `k` it stands at `k * tokensPerWeek`. Zero is
    /// unreachable because no assignment can produce it -- every one of them is at least a tick -- so
    /// week zero carries the probe's tick rather than nothing. Nothing here reads it; it is written
    /// this way so the fixture stays a deal the chain could actually report.
    fn weekly_deal(week_index: u8, week_base_tokens: u128) -> DealSubscription {
        DealSubscription {
            deal_flags: flags::SUBSCRIPTION,
            sub_weeks: SUBSCRIPTION_WEEKS,
            week_index,
            tokens_per_week: WEEK,
            funded_tokens: FUNDED,
            tokens_paid: (u128::from(week_index) * WEEK).max(TICK_SIZE),
            period_start: 0,
            week_base_tokens,
        }
    }

    /// The claim pipeline is monotonic, so a state that has settled at one cumulative figure carries
    /// it in both stages.
    fn claimed(cumulative: u128) -> DealChainState {
        let mut state = state(true, true, false, 1_000);
        state.tokens_final = cumulative;
        state.tokens_pending = cumulative;
        state
    }

    #[test]
    fn recorded_week_expiry_marks_when_to_book_not_that_it_was_booked() {
        let week = super::SUB_WEEK_LEN.as_secs();
        assert_eq!(weekly_deal(0, 0).recorded_week_expires_at(), week);
        assert_eq!(
            weekly_deal(2, 2 * WEEK).recorded_week_expires_at(),
            3 * week
        );
        // Past the final booked boundary nothing further is due: the term is over, not pending.
        let finished = weekly_deal(SUBSCRIPTION_WEEKS, FUNDED);
        assert!(finished.term_is_over());
        assert_eq!(finished.recorded_week_expires_at(), u64::MAX);
    }

    /// An ordinary deal has no weekly books at all, so there is nothing here to answer with.
    #[test]
    fn an_ordinary_deal_has_no_weekly_claim_ceiling() {
        let ordinary = DealSubscription {
            deal_flags: 0,
            sub_weeks: 0,
            ..weekly_deal(0, 0)
        };
        assert!(!ordinary.is_subscription());
        let error = super::subscription_claim_cap_at(&claimed(TICK_SIZE), &ordinary).unwrap_err();
        assert!(error.contains("not a subscription"), "{error}");
        let error =
            super::subscription_current_week_headroom(&claimed(TICK_SIZE), &ordinary).unwrap_err();
        assert!(error.contains("not a subscription"), "{error}");
    }

    /// PHASE 1 of three: no boundary has been crossed since the last booking. The recorded
    /// `weekBaseTokens` is the one the contract itself would use and the formula is the same, so the
    /// figure is EXACT - there is no error to have a sign. A client that assumes a divergence here is
    /// as wrong as one that assumes a bound.
    #[test]
    fn recorded_ceiling_is_exact_while_no_boundary_has_been_crossed() {
        // Week two is current and partly consumed; the books are up to date.
        let deal = weekly_deal(1, WEEK);
        let state = claimed(WEEK + TICK_SIZE / 2);
        assert_eq!(
            super::subscription_claim_cap_at(&state, &deal).unwrap(),
            2 * WEEK,
            "`_claimCap`: weekBaseTokens + tokensPerWeek, clamped by fundedTokens"
        );
        assert_eq!(
            super::subscription_current_week_headroom(&state, &deal).unwrap(),
            WEEK - TICK_SIZE / 2
        );
    }

    /// PHASE 2 of three: a boundary the clock has crossed that nobody has booked, term still running.
    /// `_chargeWeeksThrough` raises the base to `max(tokensFinal, tokensPending)` at the boundary and
    /// `claimTokens` books it before measuring, so the recorded figure MAY be understated - a
    /// non-strict "may". Booking a week nobody used re-bases onto the same cumulative and moves
    /// nothing, and where funding allows it both figures can settle on the same `fundedTokens` clamp.
    /// Both witnesses are constructed here rather than left to a generator.
    #[test]
    fn recorded_ceiling_is_understated_across_an_unbooked_boundary() {
        // STRICT witness, clear of the funded clamp. Week one drawn down against a base of 0: the
        // recorded books say nothing is left...
        let deal = weekly_deal(0, 0);
        let state = claimed(WEEK);
        assert_eq!(
            super::subscription_current_week_headroom(&state, &deal).unwrap(),
            0
        );
        // ...but the contract, once the crossed boundary is BOOKED, re-bases on the cumulative claim
        // and admits a whole further quota. The recorded figure was low, which is why booking - not
        // guessing - is the client's move.
        let booked = weekly_deal(1, WEEK);
        assert_eq!(
            super::subscription_claim_cap_at(&state, &booked).unwrap(),
            2 * WEEK
        );
        assert_eq!(
            super::subscription_current_week_headroom(&state, &booked).unwrap(),
            WEEK
        );

        // NON-STRICT witness: a week NOBODY used. The boundary is equally due, but booking it re-bases
        // onto the same cumulative claim, so the recorded figure was already the contract's own. The
        // understatement is a "may", never a guarantee - and this is also why an unused week is
        // forfeited rather than carried across its boundary.
        let untouched = weekly_deal(1, WEEK);
        let unused = claimed(WEEK);
        assert_eq!(
            super::subscription_claim_cap_at(&unused, &untouched).unwrap(),
            super::subscription_claim_cap_at(&unused, &weekly_deal(2, WEEK)).unwrap(),
            "booking a week nobody used raises nothing: phase 2 admits equality"
        );
    }

    /// PHASE 3 of three: past the final boundary the contract stops deriving a ceiling from the quota
    /// and returns the cumulative total already declared, so the recorded figure only UPPER-BOUNDS it.
    /// Strictly above when the last week was not fully used; exactly equal when it was.
    #[test]
    fn recorded_ceiling_only_upper_bounds_the_contract_past_the_final_boundary() {
        // The books still show week four open with an unused quota...
        let stale = weekly_deal(3, 3 * WEEK);
        let state = claimed(3 * WEEK + TICK_SIZE);
        assert_eq!(
            super::subscription_current_week_headroom(&state, &stale).unwrap(),
            WEEK - TICK_SIZE,
            "the pre-boundary quota looks spendable"
        );
        // ...but once the final boundary is booked the ceiling is the declared cumulative total and
        // every larger claim is refused. Carrying the stale figure forward would authorize a fifth
        // week of a four-week term.
        let booked = weekly_deal(SUBSCRIPTION_WEEKS, 3 * WEEK + TICK_SIZE);
        assert_eq!(
            super::subscription_claim_cap_at(&state, &booked).unwrap(),
            3 * WEEK + TICK_SIZE
        );
        assert_eq!(
            super::subscription_current_week_headroom(&state, &booked).unwrap(),
            0
        );

        // Fully consume that final week and the overstatement vanishes: both figures are the declared
        // cumulative total. "Overstated" is an upper bound, never a strict one.
        let spent = claimed(FUNDED);
        assert_eq!(
            super::subscription_claim_cap_at(&spent, &stale).unwrap(),
            FUNDED,
            "the pre-boundary quota, clamped by the funded volume"
        );
        assert_eq!(
            super::subscription_claim_cap_at(&spent, &weekly_deal(SUBSCRIPTION_WEEKS, FUNDED))
                .unwrap(),
            FUNDED,
            "and the terminal ceiling the contract applies: equal, not above"
        );
    }

    #[test]
    fn weekly_ceiling_is_clamped_by_the_funded_volume() {
        let deal = weekly_deal(3, 3 * WEEK + TICK_SIZE);
        let state = claimed(3 * WEEK + TICK_SIZE);
        assert_eq!(
            super::subscription_claim_cap_at(&state, &deal).unwrap(),
            FUNDED,
            "the final week may not reach past the funded volume"
        );
        assert_eq!(
            super::subscription_current_week_headroom(&state, &deal).unwrap(),
            WEEK - TICK_SIZE
        );
    }

    #[test]
    fn a_cumulative_claim_above_the_recorded_week_ceiling_fails_closed() {
        let deal = weekly_deal(0, 0);
        let state = claimed(WEEK + 1);
        let error = super::subscription_current_week_headroom(&state, &deal).unwrap_err();
        assert!(
            error.contains("exceeds the recorded week claim ceiling"),
            "{error}"
        );
    }

    fn exact_ordinary_subscription() -> Value {
        json!({
            "dealFlags": "0",
            "subWeeks": "0",
            "weekIndex": "0",
            "tokensPerWeek": "400",
            "fundedTokens": "400",
            "tokensPaid": "0",
            "periodStart": "70",
            "weekBaseTokens": "0"
        })
    }

    fn exact_bond() -> Value {
        json!({
            "bondFunded": true,
            "bondHeld": "200",
            "bondRequired": "200"
        })
    }

    fn exact_buyer_bond() -> Value {
        json!({
            "bondHeld": "200",
            "bondRequired": "200"
        })
    }

    fn set_field(value: &mut Value, field: &str, replacement: Value) {
        value
            .as_object_mut()
            .expect("fixture object")
            .insert(field.to_string(), replacement);
    }

    /// A snapshot that differs from the next only in the four bond figures under test. Deliberately
    /// NOT funded and NOT opened, so the seller-bond and funded-tokens gates ahead of the buyer-bond
    /// block cannot be what decides these cases.
    fn snapshot_for_bonds(
        subscription: bool,
        deal_bond: u128,
        buyer_held: u128,
        buyer_required: u128,
    ) -> DealChainSnapshot {
        DealChainSnapshot {
            account_code_hash: "code".to_string(),
            account_boc_hash: "boc".to_string(),
            state: state(false, false, false, 1_000),
            subscription: DealSubscription {
                deal_flags: if subscription { flags::SUBSCRIPTION } else { 0 },
                sub_weeks: if subscription { SUBSCRIPTION_WEEKS } else { 0 },
                week_index: 0,
                tokens_per_week: WEEK,
                funded_tokens: FUNDED,
                tokens_paid: TICK_SIZE,
                period_start: 0,
                week_base_tokens: 0,
            },
            seller_bond: DealSellerBond {
                bond_funded: false,
                bond_held: 0,
                bond_required: deal_bond,
            },
            buyer_bond: DealBuyerBond {
                bond_held: buyer_held,
                bond_required: buyer_required,
            },
        }
    }

    /// The buyer bond, pinned against the contract sources rather than against a belief about them.

    /// The belief this replaces was `SUBSCRIPTION_BUYER_BOND_TICKS` prose -- "ordinary BUYs carry no
    /// buyer bond" -- and the coherence check enforced it as `bondHeld == 0 && bondRequired == 0` on
    /// every ordinary deal. Contracts 4.0.35 posts a bond on EVERY buy fill, so that refused six
    /// live proofs with `bondHeld 2000000000 exceeds bondRequired 0` -- a client refusing a chain
    /// state that was perfectly correct.

    /// Three facts are read straight out of the `.sol` this workspace ships, so a contract change
    /// that moves any of them turns this red instead of turning a live campaign red:

    /// 1. `_bondAmount()` is `2 * _pricePerTick` -- NOT scaled by ticks, and not a different figure
    /// per side. The measured `bondHeld` of 2e9 and 4e9 across proofs is that doubled price, not
    /// the tick principal it happened to coincide with.
    /// 2. `getSellerBond()` reports `_bondAmount()` UNCONDITIONALLY, so it is the deal's bond size
    /// whichever kind of deal it is.
    /// 3. `getBuyerBond()` reports it only for a subscription and hard-zero otherwise, which is why
    /// `bondHeld > bondRequired` is the normal shape of a funded ordinary deal.

    /// The asymmetry in 2-vs-3 is the whole defect. Asserted here on the sources, because no offline
    /// fixture can notice a getter that changed shape on chain.
    #[test]
    fn the_buyer_bond_getters_are_asymmetric_and_the_source_says_so() {
        const TOKEN_CONTRACT_SOL: &str =
            include_str!("../../../../contracts/airegistry/TokenContract.sol");
        const PRIVATE_NOTE_SOL: &str = include_str!("../../../../contracts/dex/PrivateNote.sol");

        let squashed: String = TOKEN_CONTRACT_SOL
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            squashed.contains("function _bondAmount() private view returns (uint128) { return 2 * _pricePerTick; }"),
            "the deal's bond is twice the tick price, on both sides and unscaled by ticks"
        );
        // Needles are `concat!` of single literals on purpose: a `\`-continued string keeps the
        // next line's indentation and silently stops matching squashed source.
        assert!(
            squashed.contains(concat!(
                "function getSellerBond() external view returns ",
                "(bool bondFunded, uint128 bondHeld, uint128 bondRequired) ",
                "{ return (_sellerBondFunded, _sellerBond, _bondAmount()); }"
            )),
            "getSellerBond().bondRequired is _bondAmount() unconditionally"
        );
        assert!(
            squashed.contains(concat!(
                "function getBuyerBond() external view returns ",
                "(uint128 bondHeld, uint128 bondRequired) ",
                "{ return (_buyerBond, _isSubscription() ? _bondAmount() : 0); }"
            )),
            "getBuyerBond().bondRequired is hard-zero on an ordinary deal, so it must never be read as an upper bound on bondHeld"
        );
        assert!(
            squashed.contains("_buyerBond = need;")
                && squashed.contains("uint128 need = _bondAmount();")
                && squashed.contains("uint128 excess = amount - need;"),
            "fundBuyerBond stores exactly _bondAmount() and refunds the excess, which is what makes              bondHeld <= getSellerBond().bondRequired an invariant rather than a guess"
        );
        assert!(
            PRIVATE_NOTE_SOL.contains("if (isBuy) {")
                && PRIVATE_NOTE_SOL.contains("uint128 bond = uint128(2 * clearingPrice);"),
            "the note posts the bond on every BUY fill, gated on isBuy and on nothing else -- there              is no subscription test here, which is why an ordinary deal now holds one"
        );

        // And the corrected check admits the shape that broke six live proofs.
        let ordinary_with_a_bond = snapshot_for_bonds(false, 2_000_000_000, 2_000_000_000, 0);
        assert_eq!(
            ordinary_with_a_bond.validate_cross_getter_invariants(),
            Ok(())
        );
        // A bond larger than the deal could ever have taken is still refused.
        let oversized = snapshot_for_bonds(false, 2_000_000_000, 2_000_000_001, 0);
        assert!(oversized.validate_cross_getter_invariants().is_err());
        // An ordinary deal reporting a non-zero requirement is a getter that changed shape.
        let wrong_required = snapshot_for_bonds(false, 2_000_000_000, 0, 2_000_000_000);
        assert!(wrong_required.validate_cross_getter_invariants().is_err());
        // A subscription must still mirror the seller's requirement exactly.
        let sub_mismatch = snapshot_for_bonds(true, 2_000_000_000, 2_000_000_000, 1);
        assert!(sub_mismatch.validate_cross_getter_invariants().is_err());
    }

    #[test]
    fn get_state_decoder_accepts_exact_thirteen_field_abi() {
        let state = DealChainState::decode_getter(&exact_state()).expect("exact getState ABI");
        assert_eq!(state.finalized_owed, 3);
        assert_eq!(state.tokens_final, 10);
        assert_eq!(state.tokens_pending, 30);
        assert_eq!(state.last_claim_time, 60);
        assert_eq!(state.funded_time, Some(70));
    }

    #[test]
    fn buyer_bond_decoder_accepts_exact_two_field_abi_and_preserves_width() {
        let mut value = exact_buyer_bond();
        set_field(&mut value, "bondHeld", json!(u128::MAX.to_string()));
        let decoded = DealBuyerBond::decode_getter(&value).expect("exact getBuyerBond ABI");
        assert_eq!(decoded.bond_held, u128::MAX);
        assert_eq!(decoded.bond_required, 200);
    }

    #[test]
    fn buyer_bond_decoder_rejects_every_field_mutation_missing_and_extra() {
        for field in ["bondHeld", "bondRequired"] {
            let mut missing = exact_buyer_bond();
            missing
                .as_object_mut()
                .expect("fixture object")
                .remove(field);
            assert!(
                DealBuyerBond::decode_getter(&missing).is_err(),
                "missing {field} must fail closed"
            );

            for replacement in [
                Value::Null,
                json!(1),
                json!("-1"),
                json!("0x1"),
                json!(""),
                json!("340282366920938463463374607431768211456"),
            ] {
                let mut mutated = exact_buyer_bond();
                set_field(&mut mutated, field, replacement);
                assert!(
                    DealBuyerBond::decode_getter(&mutated).is_err(),
                    "mutated {field} must fail closed"
                );
            }
        }

        let mut extra = exact_buyer_bond();
        set_field(&mut extra, "bondFunded", json!(true));
        assert!(DealBuyerBond::decode_getter(&extra).is_err());
    }

    #[test]
    fn get_state_decoder_rejects_every_missing_null_and_wrong_kind_field() {
        let bool_fields = ["funded", "opened", "probeAccepted", "disputed"];
        let numeric_fields = [
            "deposit",
            "probeTick",
            "finalizedOwed",
            "tokensFinal",
            "tokensPending",
            "probeTime",
            "lastClaimTime",
            "disputeTime",
            "fundedTime",
        ];
        for field in bool_fields.into_iter().chain(numeric_fields) {
            let mut missing = exact_state();
            missing
                .as_object_mut()
                .expect("fixture object")
                .remove(field);
            assert!(
                DealChainState::decode_getter(&missing).is_err(),
                "missing {field} must fail closed"
            );

            let mut null = exact_state();
            set_field(&mut null, field, Value::Null);
            assert!(
                DealChainState::decode_getter(&null).is_err(),
                "null {field} must fail closed"
            );

            let mut wrong_kind = exact_state();
            set_field(
                &mut wrong_kind,
                field,
                if bool_fields.contains(&field) {
                    json!("false")
                } else {
                    json!(1)
                },
            );
            assert!(
                DealChainState::decode_getter(&wrong_kind).is_err(),
                "wrong JSON kind for {field} must fail closed"
            );
        }
    }

    #[test]
    fn get_state_decoder_rejects_non_decimal_overflow_and_extra_fields() {
        for field in [
            "deposit",
            "probeTick",
            "finalizedOwed",
            "tokensFinal",
            "tokensPending",
            "probeTime",
            "lastClaimTime",
            "disputeTime",
            "fundedTime",
        ] {
            for bad in ["-1", "0x1", "", "+1", " 1"] {
                let mut state = exact_state();
                set_field(&mut state, field, json!(bad));
                assert!(
                    DealChainState::decode_getter(&state).is_err(),
                    "{field}={bad:?} must fail closed"
                );
            }
        }

        for field in [
            "deposit",
            "probeTick",
            "finalizedOwed",
            "tokensFinal",
            "tokensPending",
        ] {
            let mut state = exact_state();
            set_field(
                &mut state,
                field,
                json!("340282366920938463463374607431768211456"),
            );
            assert!(
                DealChainState::decode_getter(&state).is_err(),
                "uint128 overflow in {field} must fail closed"
            );
        }
        for field in ["probeTime", "lastClaimTime", "disputeTime", "fundedTime"] {
            let mut state = exact_state();
            set_field(&mut state, field, json!("18446744073709551616"));
            assert!(
                DealChainState::decode_getter(&state).is_err(),
                "uint64 overflow in {field} must fail closed"
            );
        }

        let mut extra = exact_state();
        set_field(&mut extra, "legacy", json!("0"));
        assert!(DealChainState::decode_getter(&extra).is_err());
    }

    #[test]
    fn get_state_decoder_rejects_non_monotonic_claim_pipeline_boundaries() {
        for (final_tokens, pending) in [(31, 30), (11, 10)] {
            let mut state = exact_state();
            set_field(&mut state, "tokensFinal", json!(final_tokens.to_string()));
            set_field(&mut state, "tokensPending", json!(pending.to_string()));
            assert!(DealChainState::decode_getter(&state).is_err());
        }

        let mut equal = exact_state();
        for field in ["tokensFinal", "tokensPending"] {
            set_field(&mut equal, field, json!("10"));
        }
        assert!(DealChainState::decode_getter(&equal).is_ok());
    }

    proptest! {
        #[test]
        fn get_state_decoder_preserves_monotonic_claim_pipeline(
            final_tokens in any::<u64>(),
            delta in any::<u32>(),
        ) {
            let pending = u128::from(final_tokens) + u128::from(delta);
            let mut state = exact_state();
            set_field(&mut state, "tokensFinal", json!(final_tokens.to_string()));
            set_field(&mut state, "tokensPending", json!(pending.to_string()));
            let decoded = DealChainState::decode_getter(&state).expect("monotonic pipeline");
            prop_assert!(decoded.tokens_final <= decoded.tokens_pending);
        }

        /// over arbitrary weekly books: the three-phase rule, as inequalities that hold in
        /// every phase. Phase 1 is the exact formula the contract itself evaluates; phase 2 is a
        /// crossed-but-unbooked boundary, where booking never admits LESS; phase 3 is past the final
        /// boundary, where the recorded quota only upper-bounds the declared cumulative. None of the
        /// three is a strict inequality, which is exactly why no sign may be assumed.
        #[test]
        fn recorded_ceiling_obeys_the_three_phase_rule_over_arbitrary_books(
            week_index in 0u8..=SUBSCRIPTION_WEEKS,
            claimed_in_week in 0u128..=WEEK,
        ) {
            let base = u128::from(week_index.min(SUBSCRIPTION_WEEKS)) * WEEK;
            let deal = weekly_deal(week_index, base);
            // Every claim the contract accepts is clamped by `fundedTokens`, so a cumulative claim
            // above the funded volume is a state no chain can report: generating one would prove a
            // property about an unreachable deal.
            let pending = (base + claimed_in_week).min(deal.funded_tokens);
            let state = claimed(pending);

            let cap = super::subscription_claim_cap_at(&state, &deal).unwrap();
            let headroom = super::subscription_current_week_headroom(&state, &deal).unwrap();

            // 1. The funded volume is never exceeded, in any phase.
            prop_assert!(cap <= deal.funded_tokens);
            // 2. One week's capacity at a time: a boundary opens a quota, quotas never accumulate.
            prop_assert!(headroom <= deal.tokens_per_week);
            // 3. PHASE 1, whenever no boundary is due: the figure IS the contract's `_claimCap` -
            // `weekBaseTokens + tokensPerWeek` clamped by `fundedTokens` - with no divergence to
            // correct. Past the final boundary that formula no longer governs (phase 3 below).
            if !deal.term_is_over() {
                prop_assert_eq!(
                    cap,
                    (deal.week_base_tokens + deal.tokens_per_week).min(deal.funded_tokens)
                );
            }

            let booked_next = DealSubscription {
                week_index: week_index.saturating_add(1).min(SUBSCRIPTION_WEEKS),
                week_base_tokens: pending,
                tokens_paid: u128::from(week_index.saturating_add(1).min(SUBSCRIPTION_WEEKS))
                    * deal.tokens_per_week,
                ..deal
            };
            let after_booking =
                super::subscription_claim_cap_at(&state, &booked_next).unwrap();
            if deal.term_is_over() {
                // 4a. PHASE 3, already past the final boundary: the ceiling IS the declared cumulative
                // total, and no booking can raise it. Nothing carries forward from before it.
                prop_assert_eq!(cap, pending);
                prop_assert_eq!(headroom, 0);
                prop_assert_eq!(after_booking, pending);
            } else if week_index + 1 < SUBSCRIPTION_WEEKS {
                // 4b. PHASE 2, a crossed-but-unbooked boundary: booking it never admits LESS than the
                // books already showed. Equal when the recorded week went untouched, or when both
                // figures hit the funded clamp; greater otherwise. Never a strict understatement.
                prop_assert!(after_booking >= cap);
            } else {
                // 4c. Booking the FINAL boundary is where the relation flips: the ceiling collapses to
                // the declared cumulative, so the pre-boundary quota UPPER-BOUNDS it - equal when
                // that last quota was fully consumed.
                prop_assert_eq!(after_booking, pending);
                prop_assert!(cap >= after_booking);
            }
        }
    }

    #[test]
    fn subscription_decoder_accepts_exact_new_state_and_ordinary_shape() {
        let subscription =
            DealSubscription::decode_getter(&exact_subscription()).expect("subscription getter");
        assert!(subscription.is_subscription());
        assert_eq!(subscription.week_base_tokens, 80);

        assert!(
            !DealSubscription::decode_getter(&exact_ordinary_subscription())
                .expect("ordinary getter")
                .is_subscription()
        );
    }

    #[test]
    fn subscription_decoder_rejects_contradictory_ordinary_weekly_fields() {
        for (field, value) in [
            ("tokensPerWeek", "401"),
            ("fundedTokens", "401"),
            ("weekBaseTokens", "1"),
        ] {
            let mut ordinary = exact_ordinary_subscription();
            set_field(&mut ordinary, field, json!(value));
            assert!(
                DealSubscription::decode_getter(&ordinary).is_err(),
                "ordinary getSubscription() with {field}={value} must fail closed"
            );
        }
    }

    #[test]
    fn subscription_decoder_rejects_all_field_mutations_and_shape_drift() {
        let fields = [
            "dealFlags",
            "subWeeks",
            "weekIndex",
            "tokensPerWeek",
            "fundedTokens",
            "tokensPaid",
            "periodStart",
            "weekBaseTokens",
        ];
        for field in fields {
            let mut missing = exact_subscription();
            missing
                .as_object_mut()
                .expect("fixture object")
                .remove(field);
            assert!(DealSubscription::decode_getter(&missing).is_err());

            for replacement in [Value::Null, json!(1), json!("-1"), json!("0x1")] {
                let mut mutated = exact_subscription();
                set_field(&mut mutated, field, replacement);
                assert!(
                    DealSubscription::decode_getter(&mutated).is_err(),
                    "mutated {field} must fail closed"
                );
            }
        }

        for (field, value) in [
            ("dealFlags", "256"),
            ("subWeeks", "256"),
            ("weekIndex", "256"),
        ] {
            let mut mutated = exact_subscription();
            set_field(&mut mutated, field, json!(value));
            assert!(DealSubscription::decode_getter(&mutated).is_err());
        }
        for field in [
            "tokensPerWeek",
            "fundedTokens",
            "tokensPaid",
            "weekBaseTokens",
        ] {
            let mut mutated = exact_subscription();
            set_field(
                &mut mutated,
                field,
                json!("340282366920938463463374607431768211456"),
            );
            assert!(DealSubscription::decode_getter(&mutated).is_err());
        }
        let mut time_overflow = exact_subscription();
        set_field(
            &mut time_overflow,
            "periodStart",
            json!("18446744073709551616"),
        );
        assert!(DealSubscription::decode_getter(&time_overflow).is_err());

        for (field, value) in [
            ("dealFlags", "128"),
            ("dealFlags", "0"),
            ("subWeeks", "3"),
            ("weekIndex", "5"),
            ("fundedTokens", "401"),
            ("tokensPaid", "401"),
            ("weekBaseTokens", "401"),
        ] {
            let mut mutated = exact_subscription();
            set_field(&mut mutated, field, json!(value));
            assert!(
                DealSubscription::decode_getter(&mutated).is_err(),
                "contradictory {field}={value} must fail closed"
            );
        }

        let mut extra = exact_subscription();
        set_field(&mut extra, "claimCap", json!("0"));
        assert!(DealSubscription::decode_getter(&extra).is_err());
    }

    #[test]
    fn seller_bond_decoder_rejects_missing_wrong_kind_non_decimal_and_overflow() {
        let bond = DealSellerBond::decode_getter(&exact_bond()).expect("exact bond getter");
        assert!(bond.bond_funded);
        assert_eq!(bond.bond_held, 200);
        assert_eq!(bond.bond_required, 200);

        for field in ["bondFunded", "bondHeld", "bondRequired"] {
            let mut missing = exact_bond();
            missing
                .as_object_mut()
                .expect("fixture object")
                .remove(field);
            assert!(DealSellerBond::decode_getter(&missing).is_err());

            let mut null = exact_bond();
            set_field(&mut null, field, Value::Null);
            assert!(DealSellerBond::decode_getter(&null).is_err());
        }
        for field in ["bondHeld", "bondRequired"] {
            for value in [json!(1), json!("-1"), json!("0x1")] {
                let mut mutated = exact_bond();
                set_field(&mut mutated, field, value);
                assert!(DealSellerBond::decode_getter(&mutated).is_err());
            }
            let mut overflow = exact_bond();
            set_field(
                &mut overflow,
                field,
                json!("340282366920938463463374607431768211456"),
            );
            assert!(DealSellerBond::decode_getter(&overflow).is_err());
        }
        let mut wrong_bool = exact_bond();
        set_field(&mut wrong_bool, "bondFunded", json!("true"));
        assert!(DealSellerBond::decode_getter(&wrong_bool).is_err());

        let mut extra = exact_bond();
        set_field(&mut extra, "legacy", json!("0"));
        assert!(DealSellerBond::decode_getter(&extra).is_err());
    }

    #[test]
    fn authoritative_receipt_json_and_text_preserve_raw_precision_without_tick_flooring() {
        let receipt = SettlementActionReceipt {
            token_contract: "0:tc".to_string(),
            action: SettlementAction::BuyerStop,
            message_id: "message-id".to_string(),
            created_at: 42,
            event: SettlementActionEvent::StreamStopped {
                buyer: format!("0:{}", "44".repeat(32)),
                to_seller: u128::MAX.into(),
                refund_to_buyer: (u64::MAX as u128 + 1).into(),
            },
            pre_bonds: SettlementActionBondState {
                seller_bond_held: 2_000_000_000u128.into(),
                seller_bond_required: 2_000_000_000u128.into(),
                buyer_bond_held: 2_000_000_000u128.into(),
                buyer_bond_required: 2_000_000_000u128.into(),
            },
            post_state: Some(SettlementActionPostState {
                tokens_final: 1_000_001u128.into(),
                tokens_pending: u128::MAX.into(),
                seller_bond_held: 0u128.into(),
                seller_bond_required: 2_000_000_000u128.into(),
                buyer_bond_held: 0u128.into(),
                buyer_bond_required: 2_000_000_000u128.into(),
                opened: false,
                disputed: false,
            }),
        };

        let encoded = serde_json::to_value(&receipt).unwrap();
        assert_eq!(encoded["buyer"], json!(format!("0:{}", "44".repeat(32))));
        assert_eq!(encoded["toSeller"], json!(u128::MAX.to_string()));
        assert_eq!(
            encoded["refundToBuyer"],
            json!((u64::MAX as u128 + 1).to_string())
        );
        assert_eq!(encoded["post_state"]["tokensFinal"], json!("1000001"));
        assert!(encoded.pointer("/post_state/toSellerTicks").is_none());
        assert!(encoded.get("cursor").is_none());
        assert!(encoded.get("post_account_active").is_none());
        assert!(encoded.pointer("/post_state/finalizedOwed").is_none());
        assert!(encoded.pointer("/post_state/deposit").is_none());
        let round_trip: SettlementActionReceipt = serde_json::from_value(encoded).unwrap();
        assert_eq!(round_trip, receipt);

        let text = receipt.to_string();
        // the human receipt renders the buyer's PrivateNote canonically - a system contract of
        // the shared dexdo DApp. The machine `encoded["buyer"]` above stays on the legacy form.
        assert!(text.contains(&format!(
            "buyer={}::{}",
            crate::address::DEXDO_DAPP_ID,
            "44".repeat(32)
        )));
        assert!(text.contains(&format!("toSeller={}", u128::MAX)));
        assert!(text.contains("tokensFinal=1000001"));
        assert!(!text.contains("ticks="));
    }

    #[test]
    fn dispute_opened_has_no_projected_money_and_destroyed_terminal_omits_getters() {
        let dispute = SettlementActionReceipt {
            token_contract: "0:tc".to_string(),
            action: SettlementAction::Dispute,
            message_id: "dispute-message".to_string(),
            created_at: 43,
            event: SettlementActionEvent::StreamDisputed {
                buyer: format!("0:{}", "44".repeat(32)),
                at: 43,
            },
            pre_bonds: SettlementActionBondState {
                seller_bond_held: 2_000_000_000u128.into(),
                seller_bond_required: 2_000_000_000u128.into(),
                buyer_bond_held: 0u128.into(),
                buyer_bond_required: 0u128.into(),
            },
            post_state: Some(SettlementActionPostState {
                tokens_final: 1u128.into(),
                tokens_pending: 1u128.into(),
                seller_bond_held: 2_000_000_000u128.into(),
                seller_bond_required: 2_000_000_000u128.into(),
                buyer_bond_held: 0u128.into(),
                buyer_bond_required: 0u128.into(),
                opened: true,
                disputed: true,
            }),
        };
        let encoded = serde_json::to_value(&dispute).unwrap();
        assert_eq!(encoded["event_kind"], json!("stream_disputed"));
        assert_eq!(encoded["at"], json!(43));
        assert!(encoded.get("toSeller").is_none());
        assert!(encoded.get("refundToBuyer").is_none());

        let terminal = SettlementActionReceipt {
            token_contract: "0:tc".to_string(),
            action: SettlementAction::ReleaseDispute,
            message_id: "resolve-message".to_string(),
            created_at: 44,
            event: SettlementActionEvent::DisputeResolved {
                to_seller: 7u128.into(),
                refund_to_buyer: 8u128.into(),
                released: true,
            },
            pre_bonds: dispute.pre_bonds,
            post_state: None,
        };
        let terminal_json = serde_json::to_value(&terminal).unwrap();
        assert!(terminal_json.get("post_state").is_none());
        assert!(terminal.to_string().contains("post_state=unavailable"));
    }

    #[test]
    fn canonical_getter_decoders_have_no_silent_default_path() {
        let production = include_str!("types.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source before tests");
        for forbidden in ["unwrap_or(", "unwrap_or_default(", ".unwrap_or_else("] {
            assert!(
                !production.contains(forbidden),
                "strict getter decoder source must not contain silent default path {forbidden}"
            );
        }
    }

    #[test]
    fn production_consumers_never_read_deleted_state_fields() {
        let sources = [
            ("core chain", include_str!("mod.rs")),
            (
                "core the chain backend",
                include_str!("../chain/backends.rs"),
            ),
            ("core chain client", include_str!("../chain/client.rs")),
            ("CLI deals", include_str!("../../../dexdo/src/cli/deals.rs")),
            ("CLI audit", include_str!("../../../dexdo/src/cli/audit.rs")),
            (
                "CLI dashboard",
                include_str!("../../../dexdo/src/cli/dashboard.rs"),
            ),
            (
                "CLI reports",
                include_str!("../../../dexdo/src/cli/reports.rs"),
            ),
            (
                "CLI machine schema",
                include_str!("../../../dexdo/src/cli/machine.rs"),
            ),
            ("CLI close", include_str!("../../../dexdo/src/cli/close.rs")),
            (
                "CLI recover",
                include_str!("../../../dexdo/src/cli/recover.rs"),
            ),
            ("CLI args", include_str!("../../../dexdo/src/cli/args.rs")),
            ("CLI main help", include_str!("../../../dexdo/src/main.rs")),
            // This guard was green and right about every file it listed -- and the ONE file
            // in the tree that still read `prepaid`/`frozen`/`prepaidTime`/`lastAdvance` was not on
            // the list, so the settlement receipt declared every live deal `inconsistent` for five
            // contract generations with a check for exactly that defect passing beside it. Verified
            // by execution rather than by reading: with this entry added and the parser at its
            // pre-fix revision this test FAILS -- "CLI settlement receipt:135 still reads/renders a
            // deleted field or reclaim path: prepaid: String," -- and passes on the fixed one.
            (
                "CLI settlement receipt",
                include_str!("../../../dexdo/src/cli/settlement_receipt.rs"),
            ),
        ];
        for (source_name, source) in sources {
            for (line_index, line) in source.lines().enumerate() {
                let names_deleted_field = ["prepaid", "frozen", "lastAdvance"]
                    .iter()
                    .any(|field| line.contains(field));
                let reads_field = line.contains(".get(")
                    || line.contains("[\"")
                    || line.contains("u128_field")
                    || line.contains("u64_field")
                    || line.contains(".prepaid")
                    || line.contains(".frozen")
                    || line.contains(".last_advance");
                let renders_deleted_field = line.contains("\"lastAdvance\"")
                    || line.contains("last_advance")
                    || line.contains("\"prepaid\"")
                    || line.contains(".prepaid")
                    || line.contains(" prepaid:")
                    || line.contains("prepaid=")
                    || line.contains("\"frozen\"")
                    || line.contains(".frozen")
                    || line.contains(" frozen:")
                    || line.contains("frozen=");
                let advertises_deleted_reclaim = line.contains("streamReclaim")
                    || line.contains("reclaimOnTimeout")
                    || line.contains("STREAM_TIMEOUT");
                assert!(
                    !(names_deleted_field && reads_field)
                        && !(source_name.starts_with("CLI")
                            && (renders_deleted_field || advertises_deleted_reclaim)),
                    "{source_name}:{} still reads/renders a deleted field or reclaim path: {}",
                    line_index + 1,
                    line.trim()
                );
            }
        }
    }
}

/// Post-fill state of a `TokenContract` reported by a model-only buyer match event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchedTokenContractStatus {
    Opened,
    FundedNeverOpened {
        funded_time: Option<u64>,
        cleanup_after_unix: Option<u64>,
        cleanup_ready: bool,
        remaining_secs: Option<u64>,
    },
}

/// The note's role in a deal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DealRole {
    Buyer,
    Seller,
}

/// View of one of the note's deals for the monitor: contract, role, **anonymous**
/// counterparty, tick price and the by-fact settlement (`StreamSnapshot`).
#[derive(Debug, Clone)]
pub struct DealView {
    pub token_contract: TokenContract,
    pub role: DealRole,
    /// The counterparty's anonymous note pubkey (hex), if a match has already happened.
    pub counterparty: Option<String>,
    pub price_per_tick: Shell,
    /// The deal's served frame model id. `None` when the source cannot
    /// name it -- the mock book does not track a per-deal model, so it resolves on the real-chain reader
    /// (the `TokenContract`'s `RootModel` -> model name). The breakdown buckets `None` as `(unknown)`.
    pub model: Option<String>,
    /// The by-fact settlement (ticks/tokens/burn/closed), if the stream is open.
    pub snapshot: Option<StreamSnapshot>,
}

/// Snapshot of the note's state for observability: own orders in the book,
/// deals (role + anonymous counterparty + by-fact), total exposure (at risk). "From whom"
/// = the note's anonymous pubkey. Read only -- the monitor moves nothing.
#[derive(Debug, Clone)]
pub struct NoteSnapshot {
    /// The note's own anonymous pubkey (hex).
    pub note_id: String,
    /// Own offers in the book (the seller's orders).
    pub offers: Vec<OfferListing>,
    /// Deals where the note is the seller or the buyer.
    pub deals: Vec<DealView>,
    /// At risk: the role-side funds held in this note's open (not closed) deal TCs.
    pub exposure: Shell,
}

/// Aggregated snapshot of **the entire note tree** of a single identity: the monitor
/// shows the state across ALL (sub)notes under the root key, not only the root. We fold the
/// per-note snapshots (`ChainBackend::note_snapshot` for each pubkey from `NoteTree::node_pubkeys`):
/// offers and deals are concatenated (each lives on its own subnote), exposure is summed.
/// "From whom" remains the counterparty note's anonymous pubkey. Read only.
#[derive(Debug, Clone)]
pub struct TreeSnapshot {
    /// Anonymous pubkeys of all the tree's (sub)notes that were aggregated over (hex).
    pub note_ids: Vec<String>,
    /// All the tree's offers in the book (across all subnotes).
    pub offers: Vec<OfferListing>,
    /// All the tree's deals (across all subnotes), role + anonymous counterparty + by-fact.
    pub deals: Vec<DealView>,
    /// The tree's total exposure: role-side funds held across all open deal TCs of all subnotes.
    pub exposure: Shell,
}

/// Placeholder model id for a deal whose served model is unknown to the source: the mock book
/// tracks no per-deal model, so its deals bucket here until the real-chain reader resolves real names.
pub const UNKNOWN_MODEL: &str = "(unknown)";

/// One counterparty's by-fact tally inside a model bucket: the anonymous counterparty note
/// pubkey and the by-fact figures summed across that counterparty's deals for one role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterpartyTally {
    /// The counterparty's anonymous note pubkey (hex); `None` if no match happened yet.
    pub counterparty: Option<String>,
    /// Finalized ticks settled by-fact, summed: authoritative `tokens_final / TICK_SIZE`.
    pub tokens: u64,
    /// SHELL settled by-fact (seller: received; buyer: paid out of escrow) -- `seller_received`, summed.
    pub money: Shell,
    /// SHELL still frozen for this role (seller: `seller_locked`; buyer: `buyer_locked`), summed.
    pub locked: Shell,
    /// SHELL burned (net fee / dispute), summed.
    pub burned: Shell,
}

/// Per-model by-fact breakdown for ONE role: the note's deals grouped by served model, then by
/// anonymous counterparty, summing tokens / money / lock / burn. Pure (no network) -- the offline core of the
/// seller/buyer accounting view. The roll-up fields are the model's totals across all its counterparties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelBreakdown {
    /// The served model id, or [`UNKNOWN_MODEL`] for deals with no known model.
    pub model: String,
    pub role: DealRole,
    /// Per-counterparty tallies, in first-seen order (deterministic).
    pub counterparties: Vec<CounterpartyTally>,
    pub tokens: u64,
    pub money: Shell,
    pub locked: Shell,
    pub burned: Shell,
}

/// A by-fact accounting anomaly on a deal: a-class problem the accounting view must
/// **surface** rather than paper over (the lead's acceptance: "show the mismatch", "highlight orphaned lock").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DealAnomaly {
    /// SHELL is locked but no counterparty matched -- an **orphaned lock**: funds frozen with no deal.
    LockedNoMatch { locked: Shell },
    /// The deal is **closed** (STOP/settled) yet SHELL is still locked -- STOP should have moved it to
    /// received/refunded, not left it frozen.
    LockedAfterClose { locked: Shell },
    /// The buyer's at-risk **lead** (`prepaid + frozen`) exceeds the **two-tick invariant** ceiling (: the
    /// seller may be at most ~2 ticks ahead of finalized) -- `buyer_lead > 2 x _unit(price_per_tick)`, where the
    /// per-tick unit **includes the book fee** (`_unit(p) = p + pxFEE_BPS/10000`,).: this bounds the
    /// LEAD, not the total `buyer_locked` (which carries the unspent `deposit` for a multi-tick deal's remaining
    /// ticks) -- comparing the total false-flagged every legitimate `maxTicks > 2` deal.
    BuyerLockExceedsTwoTicks { buyer_lead: Shell, ceiling: Shell },
}
