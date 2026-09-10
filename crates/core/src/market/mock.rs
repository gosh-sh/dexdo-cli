//! `MockChainBackend` + its in-memory state -- the offline e2e on-chain stand-in (PR4 move-only).
use super::types::*;
use super::{note_id_hex, ChainBackend};
use crate::machine::{Settlement, StreamMachine, StreamState};
use crate::note::{Note, NotePubkey};
use crate::params::{
    DobParams, ProtocolConsts, Shell, MATCH_OPEN_TIMEOUT_SECS, MAX_CLAIM_DELTA,
    MIN_STREAM_BUY_TICKS, PRICE_STEP, SUBSCRIPTION_MAX_TICKS, SUBSCRIPTION_WEEKS, SUB_WEEK_LEN,
    TICK_SIZE,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Endpoints file record: key -- `token_contract`, value -- the endpoint ciphertext.
/// The same format carries over to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct EndpointsFile {
    /// `token_contract` -> base64-independent raw ciphertext (as Vec<u8> in JSON).
    handovers: HashMap<TokenContract, Vec<u8>>,
}

/// Internal state of a single stream in the mock.
#[derive(Serialize, Deserialize)]
struct StreamCell {
    schema_version: u8,
    machine: StreamMachine,
    /// Durable equivalent of `TokenContract._probeAccepted`.

    /// This must not be reconstructed from `StreamMachine::on_probe()`: every terminal transition
    /// leaves the machine outside `Probe`, including a buyer STOP that happened before acceptance.
    /// Persisting the latch keeps pre-probe no-show classification truthful after STOP and restart.
    probe_accepted: bool,
    buyer_pubkey: NotePubkey,
    seller_locked: u128,
    /// Buyer escrow still held by the TC, excluding the separately frozen probe and buyer bond.
    buyer_locked: u128,
    /// Trial tick removed from `_deposit` by `open()` until acceptance or terminal release/burn.
    probe_locked: u128,
    /// Exact `TokenContract._probeTime`, set once by `open()` and retained for strict state reads.
    probe_time: u64,
    /// Refundable subscription-only buyer dispute bond (`2P`).
    buyer_bond: u128,
    /// Buyer-side platform fees already removed from escrow but not settled at close.
    fee_accrued: u128,
    /// Exact raw-token claim pipeline, matching TokenContract `_tokensFinal/_tokensPend1/_tokensPend2`.
    tokens_final: u128,
    tokens_pending: u128,
    /// Landing times for the two raw pending slots.
    last_claim_time: u64,
    seller_received: u128,
    buyer_refunded: u128,
    burned: u128,
    closed: bool,
    /// The agreed ceiling on delivered ticks. Guard in `claim_tokens`
    /// the mock rejects a cumulative claim beyond the offer, as the real TC does.
    max_ticks: u64,
    /// A dispute is open: this deal's contested amount and seller bond are frozen (not burned)
    /// until `release_dispute`, which returns the contested amount to the buyer.
    disputed: bool,
    /// Durable instant at which a terminal dispute froze weekly accounting.
    dispute_time: u64,
    /// Exact mock-chain equivalent of the buyer-owned terminal contract event.
    buyer_stop_settlement: Option<(u128, u128)>,
}

const STREAM_CELL_SCHEMA_VERSION: u8 = 3;

/// Internal state of the mock on-chain. Serialized to a sidecar file -- this makes the mock
/// **shared across processes** the same way the real chain is in (book/matches/streams
/// live outside the processes). The endpoints file holds ONLY the handover format SEPARATELY.
#[derive(Serialize, Deserialize, Default)]
struct MockState {
    offers: HashMap<TokenContract, SellOffer>,
    /// Filled offers are no longer active book asks, but the consumed terms remain part of the deal.
    #[serde(default)]
    matched_offers: HashMap<TokenContract, SellOffer>,
    /// Authoritative filled volume K for a consumed offer; absent legacy entries mean full fill.
    #[serde(default)]
    matched_ticks: HashMap<TokenContract, u64>,
    /// Exact persisted `TokenContract.getSubscription()` projection for every funded deal.
    #[serde(default)]
    deal_subscriptions: HashMap<TokenContract, DealSubscription>,
    /// Seller (hex of the note's ed-pubkey) per offer -- for discovery/blacklist.
    #[serde(default)]
    offer_sellers: HashMap<TokenContract, String>,
    /// Absolute SELL deadline per offer, anchored ONCE when the offer was posted.

    /// The chain anchors it the same way -- `block.timestamp + ttl` inside
    /// `PrivateNote.postSellOffer` (`contracts/dex/PrivateNote.sol:793`) -- so it must be stored, not
    /// recomputed per read: a deadline that moves every time somebody looks is not a deadline, and a
    /// resting order would never reach it.
    #[serde(default)]
    offer_deadlines: HashMap<TokenContract, u64>,
    /// Constructor-bound `TokenContract.getDeal()` terms, kept for as long as the deal exists.

    /// `_pricePerTick` and `_maxTicks` are TC statics: they outlive every ask posted against them and
    /// never change, which is exactly why they are the authoritative remaining capacity of an unsold
    /// deal after its ask expired off the book.
    #[serde(default)]
    deal_terms: HashMap<TokenContract, (u64, u64)>,
    /// How many of this TC's asks have already been reaped by expiry.

    /// The book never reuses an order id, so the successor of a reaped ask must not carry the id that
    /// was just reaped.
    #[serde(default)]
    expired_generations: HashMap<TokenContract, u128>,
    matches: HashMap<TokenContract, Match>,
    /// Wall-clock source cursor for owner-facing mock fill events.
    #[serde(default)]
    match_created_at: HashMap<TokenContract, i64>,
    streams: HashMap<TokenContract, StreamCell>,
    /// Resting single-seller subscription BUYs used by the explicit production mock path.
    #[serde(default)]
    subscription_orders: HashMap<u128, OrderBookOrder>,
    #[serde(default)]
    subscription_order_books: HashMap<u128, String>,
    /// Subscription BUYs that have LEFT the book, and how.

    /// Removal used to drop the row, which threw the fact away: a lifecycle command could reach a
    /// terminal and then neither re-read it nor answer a retry with it. The row is kept so the end
    /// of the lifecycle stays observable, which is what an orchestrator reconciles against.
    #[serde(default)]
    subscription_terminal_orders: HashMap<u128, MockSubscriptionTerminal>,
    #[serde(default)]
    next_subscription_order_id: u128,
}

/// Why a subscription BUY is no longer in the mock book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MockSubscriptionExit {
    /// Its owner asked the book to take it out.
    Cancelled,
    /// Its deadline had passed and the book swept it.
    Expired,
}

/// One subscription BUY as it left the mock book, with the escrow the book itself paid back.

/// `refunded` is recorded by the removal that paid it, and a reader reports THAT number rather than
/// re-deriving one from `order.escrow`. On the deployed book the removal and the money are two
/// separate announcements -- `InferenceOrderBook.sol:387-393`, "the refund and the reason are
/// separate facts" -- and collapsing them here would let a reader report a refund the book never
/// made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockSubscriptionTerminal {
    pub order: OrderBookOrder,
    pub order_book: String,
    pub reason: MockSubscriptionExit,
    pub refunded: u128,
}

fn invalid_persisted_state(token_contract: &str, reason: impl std::fmt::Display) -> ChainError {
    ChainError::EndpointsFile(format!(
        "mock TokenContract {token_contract}: invalid persisted state: {reason}"
    ))
}

fn validate_persisted_subscription(
    token_contract: &str,
    subscription: &DealSubscription,
    now: u64,
) -> Result<(), ChainError> {
    let unknown = subscription.deal_flags & !flags::DEAL_MASK;
    if unknown != 0 {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!("dealFlags contains unknown deal-shape bits 0x{unknown:02x}"),
        ));
    }
    let subscription_flag = subscription.deal_flags & flags::SUBSCRIPTION != 0;
    if subscription_flag != (subscription.sub_weeks != 0) {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "contradictory subscription flag/subWeeks shape: dealFlags=0x{:02x}, subWeeks={}",
                subscription.deal_flags, subscription.sub_weeks
            ),
        ));
    }
    if subscription.week_index > subscription.sub_weeks {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "weekIndex {} exceeds subWeeks {}",
                subscription.week_index, subscription.sub_weeks
            ),
        ));
    }
    if subscription.tokens_paid > subscription.funded_tokens {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "tokensPaid {} exceeds fundedTokens {}",
                subscription.tokens_paid, subscription.funded_tokens
            ),
        ));
    }
    if subscription.week_base_tokens > subscription.funded_tokens {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "weekBaseTokens {} exceeds fundedTokens {}",
                subscription.week_base_tokens, subscription.funded_tokens
            ),
        ));
    }
    if subscription.period_start > now {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "periodStart {} is in the future relative to {now}",
                subscription.period_start
            ),
        ));
    }

    if subscription_flag {
        if subscription.sub_weeks != SUBSCRIPTION_WEEKS {
            return Err(invalid_persisted_state(
                token_contract,
                format_args!(
                    "subWeeks {} does not equal the canonical {SUBSCRIPTION_WEEKS}-week term",
                    subscription.sub_weeks
                ),
            ));
        }
        let expected_funded = subscription
            .tokens_per_week
            .checked_mul(u128::from(subscription.sub_weeks))
            .ok_or_else(|| {
                invalid_persisted_state(
                    token_contract,
                    "tokensPerWeek multiplied by subWeeks overflows uint128",
                )
            })?;
        if expected_funded != subscription.funded_tokens {
            return Err(invalid_persisted_state(
                token_contract,
                format_args!(
                    "subscription quota {} x {} does not equal fundedTokens {}",
                    subscription.tokens_per_week,
                    subscription.sub_weeks,
                    subscription.funded_tokens
                ),
            ));
        }
    } else if subscription.week_index != 0
        || subscription.tokens_per_week != subscription.funded_tokens
        || subscription.week_base_tokens != 0
    {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "ordinary deal has contradictory weekly state: weekIndex={}, tokensPerWeek={}, \
                 fundedTokens={}, weekBaseTokens={}",
                subscription.week_index,
                subscription.tokens_per_week,
                subscription.funded_tokens,
                subscription.week_base_tokens
            ),
        ));
    }
    Ok(())
}

fn validate_persisted_stream(
    token_contract: &str,
    cell: &StreamCell,
    now: u64,
) -> Result<(), ChainError> {
    if cell.probe_time == 0 || cell.probe_time > now {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "probeTime {} must be a non-zero timestamp no later than {now}",
                cell.probe_time
            ),
        ));
    }
    if cell.tokens_final > cell.tokens_pending {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "claim pipeline is not monotonic: tokensFinal={} tokensPending={}",
                cell.tokens_final, cell.tokens_pending
            ),
        ));
    }
    if cell.last_claim_time > now {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "claim timestamp is in the future: lastClaimTime={} now={now}",
                cell.last_claim_time
            ),
        ));
    }
    if cell.tokens_pending != 0 && cell.last_claim_time == 0 {
        return Err(invalid_persisted_state(
            token_contract,
            "non-zero claim pipeline has no landing timestamp",
        ));
    }
    if cell.dispute_time > now {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "disputeTime {} is in the future relative to {now}",
                cell.dispute_time
            ),
        ));
    }
    if cell.disputed && cell.dispute_time == 0 {
        return Err(invalid_persisted_state(
            token_contract,
            "disputed stream has no disputeTime",
        ));
    }
    if !cell.disputed && !cell.closed && cell.dispute_time != 0 {
        return Err(invalid_persisted_state(
            token_contract,
            "open undisputed stream retains a disputeTime",
        ));
    }
    if cell.dispute_time != 0 && cell.last_claim_time > cell.dispute_time {
        return Err(invalid_persisted_state(
            token_contract,
            "claim timestamp is later than disputeTime",
        ));
    }
    if !cell.probe_accepted && cell.tokens_pending != 0 {
        return Err(invalid_persisted_state(
            token_contract,
            "unaccepted probe carries a non-zero claim pipeline",
        ));
    }
    if cell.probe_accepted && cell.tokens_final < TICK_SIZE {
        return Err(invalid_persisted_state(
            token_contract,
            "accepted probe does not seed one canonical tick",
        ));
    }
    if cell.probe_accepted && cell.probe_locked != 0 {
        return Err(invalid_persisted_state(
            token_contract,
            "accepted probe still carries locked probe value",
        ));
    }

    let lifecycle_coherent = match cell.machine.state() {
        StreamState::Probe { tick } => {
            !cell.closed
                && !cell.disputed
                && !cell.probe_accepted
                && tick.price == cell.machine.price()
        }
        StreamState::Streaming { trusted, pending } => {
            !cell.closed && !cell.disputed && cell.probe_accepted && pending >= trusted
        }
        StreamState::Disputed { trusted, contested } => {
            !cell.closed && cell.disputed && trusted.checked_add(*contested).is_some()
        }
        StreamState::Closed => cell.closed && !cell.disputed,
        StreamState::Opening | StreamState::Stopping => false,
    };
    if !lifecycle_coherent {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "closed/disputed/probe flags disagree with machine state {:?}",
                cell.machine.state()
            ),
        ));
    }

    let raw_ticks = |field: &str, tokens: u128| {
        u64::try_from(tokens.div_ceil(TICK_SIZE)).map_err(|_| {
            invalid_persisted_state(
                token_contract,
                format_args!("{field} does not fit the StreamMachine tick counter"),
            )
        })
    };
    match cell.machine.state() {
        StreamState::Streaming { trusted, pending } => {
            let raw_final = raw_ticks("tokensFinal", cell.tokens_final)?;
            let raw_pending = raw_ticks("tokensPending", cell.tokens_pending)?;
            if *pending != raw_pending {
                return Err(invalid_persisted_state(
                    token_contract,
                    format_args!(
                        "machine pending ticks {pending} disagree with raw pending ticks {raw_pending}"
                    ),
                ));
            }
            if *trusted != raw_final {
                return Err(invalid_persisted_state(
                    token_contract,
                    format_args!(
                        "machine trusted ticks {trusted} disagree with raw trusted ticks {raw_final}"
                    ),
                ));
            }
        }
        StreamState::Disputed { trusted, contested } => {
            let raw_final = raw_ticks("tokensFinal", cell.tokens_final)?;
            let raw_pending = raw_ticks("tokensPending", cell.tokens_pending)?;
            let machine_pending = trusted.checked_add(*contested).ok_or_else(|| {
                invalid_persisted_state(
                    token_contract,
                    "disputed machine tick total overflows uint64",
                )
            })?;
            if machine_pending != raw_pending {
                return Err(invalid_persisted_state(
                    token_contract,
                    format_args!(
                        "machine disputed total ticks {machine_pending} disagree with raw pending ticks \
                         {raw_pending}"
                    ),
                ));
            }
            if *trusted != raw_final {
                return Err(invalid_persisted_state(
                    token_contract,
                    format_args!(
                        "machine trusted ticks {trusted} disagree with raw trusted ticks {raw_final}"
                    ),
                ));
            }
        }
        StreamState::Opening
        | StreamState::Probe { .. }
        | StreamState::Stopping
        | StreamState::Closed => {}
    }
    Ok(())
}

fn validate_persisted_deal_binding(
    state: &MockState,
    token_contract: &str,
    subscription: &DealSubscription,
    cell: Option<&StreamCell>,
) -> Result<(), ChainError> {
    let matched = state.matches.get(token_contract).ok_or_else(|| {
        invalid_persisted_state(token_contract, "funded deal has no persisted Match")
    })?;
    if matched.token_contract != token_contract {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "Match TokenContract {} disagrees with its persisted key",
                matched.token_contract
            ),
        ));
    }
    let offer = state
        .matched_offers
        .get(token_contract)
        .or_else(|| state.offers.get(token_contract))
        .ok_or_else(|| {
            invalid_persisted_state(token_contract, "funded deal has no persisted filled offer")
        })?;
    if offer.token_contract != token_contract {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "filled offer TokenContract {} disagrees with its persisted key",
                offer.token_contract
            ),
        ));
    }
    let seller = state.offer_sellers.get(token_contract).ok_or_else(|| {
        invalid_persisted_state(token_contract, "funded deal has no persisted seller actor")
    })?;
    if seller.len() != 64
        || !seller
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_persisted_state(
            token_contract,
            "persisted seller actor is not a canonical ed25519 pubkey",
        ));
    }
    if matched.price_per_tick != offer.price_per_tick {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "Match price {} disagrees with filled offer price {}",
                matched.price_per_tick, offer.price_per_tick
            ),
        ));
    }
    let matched_ticks = state
        .matched_ticks
        .get(token_contract)
        .copied()
        .unwrap_or(offer.max_ticks);
    if u128::from(matched_ticks) < MIN_STREAM_BUY_TICKS || matched_ticks > offer.max_ticks {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "matched ticks {matched_ticks} are outside filled offer volume {MIN_STREAM_BUY_TICKS}..={}",
                offer.max_ticks
            ),
        ));
    }
    let funded_tokens = u128::from(matched_ticks)
        .checked_mul(TICK_SIZE)
        .ok_or_else(|| {
            invalid_persisted_state(
                token_contract,
                "matched ticks multiplied by TICK_SIZE overflows uint128",
            )
        })?;
    if subscription.funded_tokens != funded_tokens {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "fundedTokens {} disagree with matched volume {funded_tokens}",
                subscription.funded_tokens
            ),
        ));
    }
    let deal_flags = offer.flags & flags::DEAL_MASK;
    if subscription.deal_flags != deal_flags {
        return Err(invalid_persisted_state(
            token_contract,
            format_args!(
                "dealFlags 0x{:02x} disagree with filled offer deal flags 0x{deal_flags:02x}",
                subscription.deal_flags
            ),
        ));
    }

    if let Some(cell) = cell {
        if cell.buyer_pubkey != matched.buyer_pubkey {
            return Err(invalid_persisted_state(
                token_contract,
                "stream buyer does not match persisted Match buyer",
            ));
        }
        if cell.machine.price() != matched.price_per_tick {
            return Err(invalid_persisted_state(
                token_contract,
                format_args!(
                    "machine price {} disagrees with matched price {}",
                    cell.machine.price(),
                    matched.price_per_tick
                ),
            ));
        }
        if cell.max_ticks != matched_ticks {
            return Err(invalid_persisted_state(
                token_contract,
                format_args!(
                    "stream maxTicks {} disagree with matched ticks {matched_ticks}",
                    cell.max_ticks
                ),
            ));
        }
        if cell.tokens_pending > funded_tokens {
            return Err(invalid_persisted_state(
                token_contract,
                format_args!(
                    "tokensPending {} exceed matched fundedTokens {funded_tokens}",
                    cell.tokens_pending
                ),
            ));
        }
    }
    Ok(())
}

fn validate_persisted_state(state: &MockState, now: u64) -> Result<(), ChainError> {
    for token_contract in state.matches.keys() {
        if !state.deal_subscriptions.contains_key(token_contract) {
            return Err(invalid_persisted_state(
                token_contract,
                "funded Match has no persisted deal shape",
            ));
        }
        let funded_time = mock_funded_time(state, token_contract)?;
        if funded_time == 0 || funded_time > now {
            return Err(invalid_persisted_state(
                token_contract,
                format_args!(
                    "fundedTime {funded_time} must be a non-zero timestamp no later than {now}"
                ),
            ));
        }
    }
    for token_contract in state.match_created_at.keys() {
        if !state.matches.contains_key(token_contract) {
            return Err(invalid_persisted_state(
                token_contract,
                "orphan match creation time has no persisted Match",
            ));
        }
    }
    for (token_contract, subscription) in &state.deal_subscriptions {
        validate_persisted_subscription(token_contract, subscription, now)?;
        validate_persisted_deal_binding(
            state,
            token_contract,
            subscription,
            state.streams.get(token_contract),
        )?;
    }
    for (token_contract, cell) in &state.streams {
        if cell.schema_version != STREAM_CELL_SCHEMA_VERSION {
            return Err(ChainError::EndpointsFile(format!(
                "mock TokenContract {token_contract}: unsupported persisted stream schema {}, expected {STREAM_CELL_SCHEMA_VERSION}",
                cell.schema_version
            )));
        }
        if !state.deal_subscriptions.contains_key(token_contract) {
            return Err(invalid_persisted_state(
                token_contract,
                "stream has no persisted deal shape",
            ));
        }
        validate_persisted_stream(token_contract, cell, now)?;
    }
    Ok(())
}

/// Mock on-chain backend. Book/matches/streams -- in the sidecar state file;
/// the enc-endpoint -- in the endpoints file.
#[derive(Clone)]
pub struct MockChainBackend {
    /// Serialization of critical sections (atomicity of read-modify-write over the file).
    lock: Arc<Mutex<()>>,
    endpoints_path: PathBuf,
    state_path: PathBuf,
    consts: ProtocolConsts,
    params: DobParams,
}

impl MockChainBackend {
    /// Create a mock with the given endpoints file path. The on-chain state is placed alongside
    /// in `<endpoints>.chainstate.json` -- shared between the seller/buyer processes.
    pub fn new(endpoints_path: PathBuf, consts: ProtocolConsts, params: DobParams) -> Self {
        let state_path = endpoints_path.with_extension("chainstate.json");
        Self {
            lock: Arc::new(Mutex::new(())),
            endpoints_path,
            state_path,
            consts,
            params,
        }
    }

    /// Place one exact AON+SUBSCRIPTION limit BUY into the persisted mock book.
    // The arguments intentionally preserve the production order-submission shape.
    #[allow(clippy::too_many_arguments)]
    pub fn place_subscription_order(
        &self,
        order_book: &str,
        note: &dyn Note,
        max_price_per_tick: u128,
        ticks: u128,
        escrow: u128,
        flags: u8,
        deadline: u64,
    ) -> Result<OrderBookOrder, ChainError> {
        let expected_flags = super::flags::AON | super::flags::SUBSCRIPTION;
        if flags != expected_flags || flags & super::flags::MARKET != 0 {
            return Err(ChainError::Chain(format!(
                "mock subscription flags must be exactly AON|SUBSCRIPTION (0x{expected_flags:02x})"
            )));
        }
        super::check_subscription_buy_reserve(escrow, ticks, max_price_per_tick)
            .map_err(ChainError::Chain)?;
        let _g = self.lock.lock().unwrap();
        let mut state = self.load_state()?;
        let order_id = state.next_subscription_order_id.max(1);
        state.next_subscription_order_id = order_id.checked_add(1).ok_or_else(|| {
            ChainError::Chain("mock subscription order id overflows u128".to_string())
        })?;
        let order = OrderBookOrder {
            order_id,
            owner_note: format!("0:{}", note_id_hex(&note.pubkey())),
            token_contract: None,
            is_buy: true,
            price_per_tick: max_price_per_tick,
            ticks,
            escrow,
            deadline,
            flags,
            timestamp: unix_now_secs(),
        };
        if state
            .subscription_orders
            .insert(order_id, order.clone())
            .is_some()
        {
            return Err(ChainError::Chain(format!(
                "mock subscription order id {order_id} already exists"
            )));
        }
        state
            .subscription_order_books
            .insert(order_id, order_book.to_string());
        self.store_state(&state)?;
        Ok(order)
    }

    /// Read one owned resting mock subscription BUY.
    pub fn subscription_order(
        &self,
        order_book: &str,
        order_id: u128,
        note: &dyn Note,
    ) -> Result<Option<OrderBookOrder>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let state = self.swept_state(order_id)?;
        let owner = format!("0:{}", note_id_hex(&note.pubkey()));
        Ok(state
            .subscription_orders
            .get(&order_id)
            .filter(|order| {
                order.owner_note == owner
                    && state
                        .subscription_order_books
                        .get(&order_id)
                        .is_some_and(|book| book == order_book)
            })
            .cloned())
    }

    /// Read one owned subscription BUY that has already LEFT the mock book.

    /// `None` means the book has no record of this id for this note under this order book at all --
    /// a mistyped id, not a terminal. That distinction is the reason this is a separate reader from
    /// [`MockChainBackend::subscription_order`]: "gone because it ended" and "never here" call for
    /// opposite actions from a machine consumer.
    pub fn subscription_terminal_order(
        &self,
        order_book: &str,
        order_id: u128,
        note: &dyn Note,
    ) -> Result<Option<MockSubscriptionTerminal>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let state = self.swept_state(order_id)?;
        let owner = format!("0:{}", note_id_hex(&note.pubkey()));
        Ok(state
            .subscription_terminal_orders
            .get(&order_id)
            .filter(|terminal| {
                terminal.order.owner_note == owner && terminal.order_book == order_book
            })
            .cloned())
    }

    /// Cancel one owned resting mock subscription BUY and return its exact escrow refund.
    pub fn cancel_subscription_order(
        &self,
        order_book: &str,
        order_id: u128,
        note: &dyn Note,
    ) -> Result<OrderBookOrder, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut state = self.swept_state(order_id)?;
        let owner = format!("0:{}", note_id_hex(&note.pubkey()));
        let order = state
            .subscription_orders
            .get(&order_id)
            .filter(|order| {
                order.owner_note == owner
                    && state
                        .subscription_order_books
                        .get(&order_id)
                        .is_some_and(|book| book == order_book)
            })
            .cloned()
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "mock subscription order {order_id} is absent or owned by another note"
                ))
            })?;
        let refunded = order.escrow;
        remove_subscription_order_from_book(
            &mut state,
            order_id,
            MockSubscriptionExit::Cancelled,
            refunded,
        );
        self.store_state(&state)?;
        Ok(order)
    }

    /// The persisted state with the deployed book's own bid-expiry sweep applied to one order id.

    /// `InferenceOrderBook.expireOrder` is permissionless, silent on a row that is gone or still
    /// live, and `_removeExpiredBid` refunds the buyer's escrow as it removes the row. The practice
    /// book runs no second process to call it, so it sweeps the row a subscription command names.
    /// The CLIENT still decides nothing from a clock: it reads what the book removed and what the
    /// book paid back, exactly as it does against the deployed one.
    fn swept_state(&self, order_id: u128) -> Result<MockState, ChainError> {
        let mut state = self.load_state()?;
        let now = unix_now_secs();
        let expired = state
            .subscription_orders
            .get(&order_id)
            .is_some_and(|order| order.deadline != 0 && order.deadline <= now);
        if !expired {
            return Ok(state);
        }
        let refunded = state
            .subscription_orders
            .get(&order_id)
            .map(|order| order.escrow)
            .unwrap_or_default();
        remove_subscription_order_from_book(
            &mut state,
            order_id,
            MockSubscriptionExit::Expired,
            refunded,
        );
        self.store_state(&state)?;
        Ok(state)
    }

    /// Submit buyer STOP from a persisted mock runtime handle.

    /// Mock handles cannot retain the ephemeral buyer secret, so their stable actor identity is the
    /// `mock:<ed25519-pubkey>` address written when the match is created. This adapter validates that
    /// address against the authoritative matched buyer stored in the stream before applying the same
    /// transition as [`ChainBackend::stop`]. A stale or forged handle therefore fails before mutation.
    pub async fn stop_by_buyer_note_addr(
        &self,
        token_contract: &TokenContract,
        buyer_note_addr: &str,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut state = self.load_state()?;
        require_mock_buyer_note_addr(&state, token_contract, buyer_note_addr, "stop")?;
        let settlement = stop_mock_stream(&mut state, token_contract, &self.consts)?;
        self.store_state(&state)?;
        Ok(settlement)
    }

    /// Read one mock stream without hiding a malformed persisted sidecar as an absent stream.
    pub async fn checked_snapshot(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<StreamSnapshot>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let state = self.load_state()?;
        let Some(cell) = state.streams.get(token_contract) else {
            return Ok(None);
        };
        let buyer_locked = cell
            .buyer_locked
            .checked_add(cell.probe_locked)
            .and_then(|amount| amount.checked_add(cell.buyer_bond))
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "mock TokenContract {token_contract}: total buyer lock overflows uint128"
                ))
            })?;
        let contested_tokens = cell
            .tokens_pending
            .checked_sub(cell.tokens_final)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "mock TokenContract {token_contract}: pending tokens are below finalized tokens"
                ))
            })?;
        let contested_value = checked_mul_div_floor(
            token_contract,
            contested_tokens,
            u128::from(cell.machine.price()),
            TICK_SIZE,
            "buyer exposure",
        )?;
        let buyer_lead = cell
            .probe_locked
            .checked_add(contested_value)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "mock TokenContract {token_contract}: buyer exposure overflows uint128"
                ))
            })?;
        Ok(Some(StreamSnapshot {
            seller_locked: cell.seller_locked,
            buyer_locked,
            buyer_lead,
            tokens_final: cell.tokens_final,
            seller_received: cell.seller_received,
            buyer_refunded: cell.buyer_refunded,
            burned: cell.burned,
            closed: cell.closed,
        }))
    }

    fn load_state(&self) -> Result<MockState, ChainError> {
        match std::fs::read(&self.state_path) {
            Ok(bytes) if !bytes.is_empty() => {
                let state: MockState = serde_json::from_slice(&bytes)
                    .map_err(|e| ChainError::EndpointsFile(e.to_string()))?;
                validate_persisted_state(&state, unix_now_secs())?;
                Ok(state)
            }
            Ok(_) => Ok(MockState::default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MockState::default()),
            Err(e) => Err(ChainError::EndpointsFile(e.to_string())),
        }
    }

    fn store_state(&self, st: &MockState) -> Result<(), ChainError> {
        let bytes = serde_json::to_vec(st).map_err(|e| ChainError::EndpointsFile(e.to_string()))?;
        std::fs::write(&self.state_path, bytes)
            .map_err(|e| ChainError::EndpointsFile(e.to_string()))
    }

    fn read_endpoints(&self) -> Result<EndpointsFile, ChainError> {
        match std::fs::read(&self.endpoints_path) {
            Ok(bytes) if !bytes.is_empty() => {
                serde_json::from_slice(&bytes).map_err(|e| ChainError::EndpointsFile(e.to_string()))
            }
            Ok(_) => Ok(EndpointsFile::default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(EndpointsFile::default()),
            Err(e) => Err(ChainError::EndpointsFile(e.to_string())),
        }
    }

    fn write_endpoints(&self, f: &EndpointsFile) -> Result<(), ChainError> {
        let bytes = serde_json::to_vec(f).map_err(|e| ChainError::EndpointsFile(e.to_string()))?;
        std::fs::write(&self.endpoints_path, bytes)
            .map_err(|e| ChainError::EndpointsFile(e.to_string()))
    }

    fn place_buy_ticks_inner(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
        ticks: Option<u64>,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        if st.matches.contains_key(token_contract) {
            return Err(ChainError::Chain(format!(
                "mock TokenContract {token_contract} is already matched; refusing to replace its buyer"
            )));
        }
        let offer = st
            .offers
            .get(token_contract)
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))?
            .clone();
        let matched_ticks = ticks.unwrap_or(offer.max_ticks);
        if u128::from(matched_ticks) < MIN_STREAM_BUY_TICKS || matched_ticks > offer.max_ticks {
            return Err(ChainError::Chain(format!(
                "mock buy ticks must be within {MIN_STREAM_BUY_TICKS}..={}, got {matched_ticks}",
                offer.max_ticks,
            )));
        }
        if offer.flags & flags::AON != 0 && matched_ticks != offer.max_ticks {
            return Err(ChainError::Chain(format!(
                "mock AON sell offer for {token_contract} requires its full {} ticks, got {matched_ticks}",
                offer.max_ticks
            )));
        }
        st.offers.remove(token_contract);
        st.matched_offers
            .insert(token_contract.clone(), offer.clone());
        st.matched_ticks
            .insert(token_contract.clone(), matched_ticks);
        let subscription = mock_deal_shape(&offer, matched_ticks, unix_now_secs())?;
        st.deal_subscriptions
            .insert(token_contract.clone(), subscription);
        st.matches.insert(
            token_contract.clone(),
            Match {
                token_contract: token_contract.clone(),
                buyer_pubkey: note.pubkey(),
                price_per_tick: offer.price_per_tick,
            },
        );
        st.match_created_at
            .insert(token_contract.clone(), mock_now_unix());
        self.store_state(&st)
    }

    /// Mock order-book fill of an exact partial volume, used by the production mock e2e path.
    pub async fn place_buy_ticks(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
        ticks: u64,
    ) -> Result<(), ChainError> {
        self.place_buy_ticks_inner(token_contract, note, Some(ticks))
    }
}

/// Take one subscription BUY out of the mock book and keep it as a terminal fact.

/// The book row and its order-book binding go, so nothing can match, cancel or expire it a second
/// time; what stays is the record of how it left and what the book paid back. A no-op when the id
/// is not in the book, which is what makes both removals idempotent -- the deployed `expireOrder` is
/// silent on a row that is already gone, and a retried cancel must be answerable rather than an
/// error.
fn remove_subscription_order_from_book(
    state: &mut MockState,
    order_id: u128,
    reason: MockSubscriptionExit,
    refunded: u128,
) {
    let Some(order) = state.subscription_orders.remove(&order_id) else {
        return;
    };
    let order_book = state
        .subscription_order_books
        .remove(&order_id)
        .unwrap_or_default();
    state.subscription_terminal_orders.insert(
        order_id,
        MockSubscriptionTerminal {
            order,
            order_book,
            reason,
            refunded,
        },
    );
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn mock_order_id(token_contract: &str) -> u128 {
    let digest = Sha256::digest(token_contract.as_bytes());
    u128::from_be_bytes(digest[..16].try_into().expect("SHA-256 prefix"))
}

/// The id of this TC's CURRENT ask.

/// Generation zero is the historical id, so nothing that never expires an order sees a change. Each
/// reaped generation moves it, because `InferenceOrderBook` allocates a new id per posting and never
/// hands a reaped one back -- a successor that reused it would be indistinguishable from the order it
/// replaced, in the book and in every assertion about "exactly one live offer".
fn mock_current_order_id(state: &MockState, token_contract: &str) -> u128 {
    mock_order_id(token_contract).wrapping_add(
        state
            .expired_generations
            .get(token_contract)
            .copied()
            .unwrap_or(0),
    )
}

fn mock_now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn validate_mock_sell_offer(offer: &SellOffer, consts: &ProtocolConsts) -> Result<(), ChainError> {
    const SUPPORTED_FLAGS: u8 = flags::IOC
        | flags::FOK
        | flags::MARKET
        | flags::POST_ONLY
        | flags::TEE
        | flags::AON
        | flags::SUBSCRIPTION;
    const TAKER_FLAGS: u8 = flags::IOC | flags::FOK | flags::MARKET;

    let ticks = u128::from(offer.max_ticks);
    if ticks < MIN_STREAM_BUY_TICKS {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {}: sell offer needs at least {MIN_STREAM_BUY_TICKS} ticks, got {}",
            offer.token_contract, offer.max_ticks
        )));
    }
    let price = u128::from(offer.price_per_tick);
    if price == 0 || !price.is_multiple_of(PRICE_STEP) {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {}: price_per_tick {} must be a positive multiple of PRICE_STEP {PRICE_STEP}",
            offer.token_contract, offer.price_per_tick
        )));
    }
    let unknown = offer.flags & !SUPPORTED_FLAGS;
    if unknown != 0 {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {}: sell offer has unsupported flags 0x{unknown:02x}",
            offer.token_contract
        )));
    }
    if offer.flags & flags::MARKET != 0 {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {}: a sell offer cannot be a MARKET order",
            offer.token_contract
        )));
    }
    if offer.flags & flags::POST_ONLY != 0 && offer.flags & TAKER_FLAGS != 0 {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {}: POST_ONLY cannot be combined with taker flags",
            offer.token_contract
        )));
    }
    if offer.flags & flags::IOC != 0 && offer.flags & flags::FOK != 0 {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {}: IOC and FOK are mutually exclusive",
            offer.token_contract
        )));
    }
    if offer.flags & flags::SUBSCRIPTION != 0 {
        if offer.flags & flags::AON == 0 {
            return Err(ChainError::Chain(format!(
                "mock TokenContract {}: SUBSCRIPTION requires AON",
                offer.token_contract
            )));
        }
        if !ticks.is_multiple_of(u128::from(SUBSCRIPTION_WEEKS)) || ticks > SUBSCRIPTION_MAX_TICKS {
            return Err(ChainError::Chain(format!(
                "mock TokenContract {}: subscription ticks must divide into {SUBSCRIPTION_WEEKS} weeks and not exceed {SUBSCRIPTION_MAX_TICKS}, got {}",
                offer.token_contract, offer.max_ticks
            )));
        }
    }

    let funded_tokens = ticks.checked_mul(TICK_SIZE).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {}: offer token volume overflows uint128",
            offer.token_contract
        ))
    })?;
    let (pay, fee) = token_pay_and_fee(
        &offer.token_contract,
        funded_tokens,
        offer.price_per_tick,
        consts,
    )?;
    pay.checked_add(fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {}: offer value plus fee overflows uint128",
            offer.token_contract
        ))
    })?;
    Ok(())
}

fn mock_funded_time(st: &MockState, token_contract: &str) -> Result<u64, ChainError> {
    let created_at = st.match_created_at.get(token_contract).ok_or_else(|| {
        invalid_persisted_state(token_contract, "funded deal has no match creation time")
    })?;
    u64::try_from(*created_at).map_err(|_| {
        invalid_persisted_state(
            token_contract,
            format_args!("match creation time {created_at} is negative"),
        )
    })
}

fn mock_deal_state(
    st: &MockState,
    token_contract: &TokenContract,
    consts: &ProtocolConsts,
) -> Result<Option<DealChainState>, ChainError> {
    let funded_time = st
        .matches
        .contains_key(token_contract)
        .then(|| mock_funded_time(st, token_contract))
        .transpose()?;
    if let Some(cell) = st.streams.get(token_contract) {
        let funded_time = funded_time
            .ok_or_else(|| invalid_persisted_state(token_contract, "stream has no funded match"))?;
        return Ok(Some(DealChainState {
            funded: true,
            opened: !cell.closed,
            probe_accepted: cell.probe_accepted,
            disputed: cell.disputed,
            // Every terminal path drains the escrow; a still-open deal keeps whatever is unspent.
            deposit: if cell.closed { 0 } else { cell.buyer_locked },
            finalized_owed: cell.seller_received,
            tokens_final: cell.tokens_final,
            tokens_pending: cell.tokens_pending,
            probe_tick: cell.probe_locked,
            funded_time: Some(funded_time),
            probe_time: cell.probe_time,
            // Non-zero once opened: the mock has no clock, but callers use this only to tell an
            // opened-and-settled deal from a never-opened one.
            last_claim_time: cell.last_claim_time,
            dispute_time: cell.dispute_time,
        }));
    }
    let Some(funded_time) = funded_time else {
        return Ok(None);
    };
    let subscription = st.deal_subscriptions.get(token_contract).ok_or_else(|| {
        invalid_persisted_state(token_contract, "funded deal has no persisted deal shape")
    })?;
    let matched = st.matches.get(token_contract).ok_or_else(|| {
        invalid_persisted_state(token_contract, "funded deal has no persisted Match")
    })?;
    let (pay, fee) = token_pay_and_fee(
        token_contract,
        subscription.funded_tokens,
        matched.price_per_tick,
        consts,
    )?;
    let deposit = pay.checked_add(fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: buyer deposit plus fee overflows uint128"
        ))
    })?;
    Ok(Some(DealChainState {
        funded: true,
        opened: false,
        probe_accepted: false,
        disputed: false,
        deposit,
        finalized_owed: 0,
        tokens_final: 0,
        tokens_pending: 0,
        probe_tick: 0,
        funded_time: Some(funded_time),
        probe_time: 0,
        last_claim_time: funded_time,
        dispute_time: 0,
    }))
}

fn mock_deal_subscription(
    st: &MockState,
    token_contract: &TokenContract,
) -> Result<Option<DealSubscription>, ChainError> {
    if let Some(subscription) = st.deal_subscriptions.get(token_contract) {
        return Ok(Some(*subscription));
    }
    let Some(offer) = st
        .matched_offers
        .get(token_contract)
        .or_else(|| st.offers.get(token_contract))
    else {
        return Ok(None);
    };
    let funded_ticks = st
        .matched_ticks
        .get(token_contract)
        .copied()
        .unwrap_or(offer.max_ticks);
    mock_deal_shape(offer, funded_ticks, 0).map(Some)
}

fn mock_deal_shape(
    offer: &SellOffer,
    funded_ticks: u64,
    period_start: u64,
) -> Result<DealSubscription, ChainError> {
    let funded_tokens = u128::from(funded_ticks)
        .checked_mul(TICK_SIZE)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "mock TokenContract {}: funded token volume overflows uint128",
                offer.token_contract
            ))
        })?;
    let deal_flags = offer.flags & flags::DEAL_MASK;
    if deal_flags & flags::SUBSCRIPTION != 0 {
        if !funded_ticks.is_multiple_of(u64::from(SUBSCRIPTION_WEEKS)) {
            return Err(ChainError::Chain(format!(
                "mock TokenContract {}: subscription funded ticks {} is not divisible by \
                 {SUBSCRIPTION_WEEKS}",
                offer.token_contract, funded_ticks
            )));
        }
        Ok(DealSubscription {
            deal_flags,
            sub_weeks: SUBSCRIPTION_WEEKS,
            week_index: 0,
            tokens_per_week: funded_tokens / u128::from(SUBSCRIPTION_WEEKS),
            funded_tokens,
            tokens_paid: 0,
            period_start,
            week_base_tokens: 0,
        })
    } else {
        Ok(DealSubscription {
            deal_flags,
            sub_weeks: 0,
            week_index: 0,
            tokens_per_week: funded_tokens,
            funded_tokens,
            tokens_paid: 0,
            period_start,
            week_base_tokens: 0,
        })
    }
}

fn pending_tokens(cell: &StreamCell) -> u128 {
    cell.tokens_pending
}

fn claim_deadline_reached(
    token_contract: &TokenContract,
    landed_at: u64,
    window: std::time::Duration,
    now: u64,
) -> Result<bool, ChainError> {
    let due = landed_at.checked_add(window.as_secs()).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: claim promotion deadline overflows uint64"
        ))
    })?;
    Ok(now >= due)
}

/// Contracts 4.0.35 collapsed the claim conveyor to two slots: `_tokensFinal <- _tokensPend`, with
/// `CLAIM_PROMOTE_WINDOW == MIN_CLAIM_INTERVAL`, so the arrival of the next claim finalizes the
/// previous one and at most one tick is ever unpromoted.
fn promote_once(cell: &mut StreamCell) {
    if cell.tokens_pending > cell.tokens_final {
        cell.tokens_final = cell.tokens_pending;
    }
}

fn promote_due(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    now: u64,
    consts: &ProtocolConsts,
) -> Result<(), ChainError> {
    if claim_deadline_reached(
        token_contract,
        cell.last_claim_time,
        consts.claim_promote_window,
        now,
    )? {
        promote_once(cell);
    }
    Ok(())
}

fn has_due_claim(
    token_contract: &TokenContract,
    cell: &StreamCell,
    now: u64,
    consts: &ProtocolConsts,
) -> Result<bool, ChainError> {
    Ok(cell.tokens_pending > cell.tokens_final
        && claim_deadline_reached(
            token_contract,
            cell.last_claim_time,
            consts.claim_promote_window,
            now,
        )?)
}

fn void_claims_at(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    at: u64,
    consts: &ProtocolConsts,
) -> Result<(), ChainError> {
    if cell.tokens_pending > cell.tokens_final
        && claim_deadline_reached(
            token_contract,
            cell.last_claim_time,
            consts.claim_promote_window,
            at,
        )?
    {
        cell.tokens_final = cell.tokens_pending;
    }
    cell.tokens_pending = cell.tokens_final;
    Ok(())
}

fn finalized_ticks(token_contract: &TokenContract, tokens: u128) -> Result<u64, ChainError> {
    u64::try_from(tokens / TICK_SIZE).map_err(|_| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: finalized token total {tokens} exceeds the tick counter"
        ))
    })
}

fn checked_mul_div_floor(
    token_contract: &TokenContract,
    value: u128,
    multiplier: u128,
    denominator: u128,
    context: &str,
) -> Result<u128, ChainError> {
    let whole = value / denominator;
    let remainder = value % denominator;
    whole
        .checked_mul(multiplier)
        .and_then(|amount| {
            remainder
                .checked_mul(multiplier)
                .map(|tail| tail / denominator)
                .and_then(|tail| amount.checked_add(tail))
        })
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "mock TokenContract {token_contract}: {context} overflows uint128"
            ))
        })
}

fn token_pay_and_fee(
    token_contract: &TokenContract,
    tokens: u128,
    price: Shell,
    consts: &ProtocolConsts,
) -> Result<(u128, u128), ChainError> {
    let pay = checked_mul_div_floor(
        token_contract,
        tokens,
        u128::from(price),
        TICK_SIZE,
        "token payment",
    )?;
    let fee = checked_mul_div_floor(
        token_contract,
        pay,
        u128::from(consts.platform_fee_bps),
        10_000,
        "platform fee",
    )?;
    Ok((pay, fee))
}

fn credit_token_delta(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    tokens: u128,
    consts: &ProtocolConsts,
) -> Result<(), ChainError> {
    let (pay, fee) = token_pay_and_fee(token_contract, tokens, cell.machine.price(), consts)?;
    let debit = pay.checked_add(fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: token payment plus fee overflows uint128"
        ))
    })?;
    if debit > cell.buyer_locked {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {token_contract}: token payment plus fee {debit} exceeds buyer escrow {}",
            cell.buyer_locked
        )));
    }
    cell.buyer_locked -= debit;
    cell.seller_received = cell.seller_received.checked_add(pay).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: seller proceeds overflow uint128"
        ))
    })?;
    cell.fee_accrued = cell.fee_accrued.checked_add(fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: accrued fee overflows uint128"
        ))
    })?;
    Ok(())
}

fn credit_tokens_through(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    subscription: &mut DealSubscription,
    target_tokens_paid: u128,
    consts: &ProtocolConsts,
) -> Result<(), ChainError> {
    if target_tokens_paid < subscription.tokens_paid {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {token_contract}: payment target {target_tokens_paid} regresses tokensPaid {}",
            subscription.tokens_paid
        )));
    }
    if target_tokens_paid > subscription.funded_tokens {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {token_contract}: payment target {target_tokens_paid} exceeds fundedTokens {}",
            subscription.funded_tokens
        )));
    }
    let (paid_value, paid_fee) = token_pay_and_fee(
        token_contract,
        subscription.tokens_paid,
        cell.machine.price(),
        consts,
    )?;
    let (target_value, target_fee) = token_pay_and_fee(
        token_contract,
        target_tokens_paid,
        cell.machine.price(),
        consts,
    )?;
    let pay = target_value.checked_sub(paid_value).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: cumulative seller payment regresses"
        ))
    })?;
    let fee = target_fee.checked_sub(paid_fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: cumulative fee regresses"
        ))
    })?;
    let debit = pay.checked_add(fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: payment plus fee overflows uint128"
        ))
    })?;
    if debit > cell.buyer_locked {
        return Err(ChainError::Chain(format!(
            "mock TokenContract {token_contract}: payment plus fee {debit} exceeds buyer escrow {}",
            cell.buyer_locked
        )));
    }
    cell.buyer_locked -= debit;
    cell.seller_received = cell.seller_received.checked_add(pay).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: seller proceeds overflow uint128"
        ))
    })?;
    cell.fee_accrued = cell.fee_accrued.checked_add(fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: accrued fee overflows uint128"
        ))
    })?;
    subscription.tokens_paid = target_tokens_paid;
    Ok(())
}

fn subscription_weeks_at(
    token_contract: &TokenContract,
    subscription: &DealSubscription,
    now: u64,
    include_started: bool,
) -> Result<u8, ChainError> {
    let elapsed = now.checked_sub(subscription.period_start).ok_or_else(|| {
        ChainError::Chain(format!(
            "subscription clock: current time precedes {token_contract} periodStart"
        ))
    })?;
    let elapsed_weeks = elapsed / SUB_WEEK_LEN.as_secs();
    let elapsed_weeks = elapsed_weeks.min(u64::from(subscription.sub_weeks));
    let target = if include_started && elapsed_weeks < u64::from(subscription.sub_weeks) {
        elapsed_weeks + 1
    } else {
        elapsed_weeks
    };
    u8::try_from(target).map_err(|_| {
        ChainError::Chain(format!(
            "subscription clock: week count does not fit {token_contract} weekIndex"
        ))
    })
}

fn charge_subscription_through(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    subscription: &mut DealSubscription,
    target_week: u8,
    consts: &ProtocolConsts,
) -> Result<bool, ChainError> {
    if target_week <= subscription.week_index {
        return Ok(false);
    }
    let target_tokens_paid = u128::from(target_week)
        .checked_mul(subscription.tokens_per_week)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "settle_week: {token_contract} cumulative weekly payment overflows uint128"
            ))
        })?;
    credit_tokens_through(
        token_contract,
        cell,
        subscription,
        target_tokens_paid,
        consts,
    )?;
    subscription.week_index = target_week;
    subscription.week_base_tokens = pending_tokens(cell);
    Ok(true)
}

/// Advance the persisted subscription books through every elapsed boundary without closing the TC.
/// `claimTokens` and `settleWeek` share this exact accounting step in TokenContract 4.0.31.
fn charge_elapsed_subscription_weeks(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    subscription: &mut DealSubscription,
    now: u64,
    consts: &ProtocolConsts,
) -> Result<bool, ChainError> {
    if !subscription.is_subscription() {
        return Ok(false);
    }
    if !cell.probe_accepted || cell.closed || cell.disputed {
        return Err(ChainError::Chain(format!(
            "settle_week: {token_contract} is not an open accepted subscription"
        )));
    }
    let due = subscription_weeks_at(token_contract, subscription, now, false)?;
    charge_subscription_through(token_contract, cell, subscription, due, consts)
}

fn delivered_ticks(token_contract: &TokenContract, cell: &StreamCell) -> Result<u64, ChainError> {
    Ok(finalized_ticks(token_contract, cell.tokens_final)?.max(u64::from(cell.probe_accepted)))
}

fn settle_mock_fees(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    delivered_ticks: u64,
    consts: &ProtocolConsts,
    clean: bool,
) -> Result<(), ChainError> {
    let rebate = if clean {
        let gross = u128::from(delivered_ticks)
            .checked_mul(u128::from(cell.machine.price()))
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "mock TokenContract {token_contract}: rebate volume overflows uint128"
                ))
            })?;
        let rate = u128::from(consts.rebate_slope_bps)
            .saturating_mul(u128::from(delivered_ticks))
            .min(u128::from(consts.rebate_max_bps));
        checked_mul_div_floor(token_contract, gross, rate, 10_000, "seller rebate")?
            .min(cell.fee_accrued)
    } else {
        0
    };
    cell.seller_received = cell.seller_received.checked_add(rebate).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: seller rebate overflows uint128"
        ))
    })?;
    cell.burned = cell
        .burned
        .checked_add(cell.fee_accrued - rebate)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "mock TokenContract {token_contract}: fee burn overflows uint128"
            ))
        })?;
    cell.fee_accrued = 0;
    Ok(())
}

fn close_mock_clean(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    delivered_ticks: u64,
    consts: &ProtocolConsts,
) -> Result<u128, ChainError> {
    settle_mock_fees(token_contract, cell, delivered_ticks, consts, true)?;
    let refund = cell
        .buyer_locked
        .checked_add(cell.probe_locked)
        .and_then(|amount| amount.checked_add(cell.buyer_bond))
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "mock TokenContract {token_contract}: clean buyer refund overflows uint128"
            ))
        })?;
    cell.buyer_refunded = cell.buyer_refunded.checked_add(refund).ok_or_else(|| {
        ChainError::Chain(format!(
            "mock TokenContract {token_contract}: cumulative buyer refund overflows uint128"
        ))
    })?;
    cell.seller_received = cell
        .seller_received
        .checked_add(cell.seller_locked)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "mock TokenContract {token_contract}: finalized seller amount overflows uint128"
            ))
        })?;
    cell.buyer_locked = 0;
    cell.probe_locked = 0;
    cell.buyer_bond = 0;
    cell.seller_locked = 0;
    cell.machine.close();
    cell.closed = true;
    Ok(refund)
}

fn resolve_mock_dispute(
    token_contract: &TokenContract,
    cell: &mut StreamCell,
    subscription: &mut DealSubscription,
    consts: &ProtocolConsts,
    timed_out: bool,
) -> Result<(u128, u128), ChainError> {
    if !cell.disputed {
        return Err(ChainError::Chain(format!(
            "release_dispute: {token_contract} not in dispute"
        )));
    }
    if subscription.is_subscription() && cell.probe_accepted {
        let completed =
            subscription_weeks_at(token_contract, subscription, cell.dispute_time, false)?;
        charge_subscription_through(token_contract, cell, subscription, completed, consts)?;
    }

    let pending = pending_tokens(cell);
    let claimed_base = if subscription.is_subscription() && subscription.week_index > 0 {
        subscription.week_base_tokens
    } else {
        subscription.tokens_paid
    };
    let claimed_unpaid = pending.saturating_sub(claimed_base);
    let (claimed_pay, claimed_fee) =
        token_pay_and_fee(token_contract, claimed_unpaid, cell.machine.price(), consts)?;
    let claimed_value = claimed_pay.checked_add(claimed_fee).ok_or_else(|| {
        ChainError::Chain(format!(
            "release_dispute: {token_contract} dispute stake overflows uint128"
        ))
    })?;
    let price = u128::from(cell.machine.price());
    let mut stake = claimed_value
        .max(price)
        .min(2 * price)
        .min(cell.seller_locked);
    stake = if subscription.is_subscription() {
        stake.min(cell.buyer_bond)
    } else {
        stake.min(cell.buyer_locked)
    };

    let dispute_time = cell.dispute_time;
    void_claims_at(token_contract, cell, dispute_time, consts)?;
    let trusted_tokens = cell.tokens_final;
    let trusted_base = if subscription.is_subscription() && subscription.week_index > 0 {
        subscription.week_base_tokens
    } else {
        subscription.tokens_paid
    };
    let trusted_owed = trusted_tokens.saturating_sub(trusted_base);
    credit_token_delta(token_contract, cell, trusted_owed, consts)?;
    subscription.tokens_paid = subscription.tokens_paid.max(trusted_tokens);

    if subscription.is_subscription() {
        cell.buyer_bond -= stake;
    } else {
        stake = stake.min(cell.buyer_locked);
        cell.buyer_locked -= stake;
    }
    cell.burned = cell.burned.checked_add(stake).ok_or_else(|| {
        ChainError::Chain(format!(
            "release_dispute: {token_contract} buyer stake burn overflows uint128"
        ))
    })?;
    let seller_burn = if timed_out {
        stake.min(cell.seller_locked)
    } else {
        0
    };
    cell.seller_locked -= seller_burn;
    cell.burned = cell.burned.checked_add(seller_burn).ok_or_else(|| {
        ChainError::Chain(format!(
            "release_dispute: {token_contract} seller stake burn overflows uint128"
        ))
    })?;
    let delivered = delivered_ticks(token_contract, cell)?;
    settle_mock_fees(token_contract, cell, delivered, consts, false)?;

    let remaining_deposit = cell
        .buyer_locked
        .checked_add(cell.probe_locked)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "release_dispute: {token_contract} remaining deposit overflows uint128"
            ))
        })?;
    let refundable_deposit = if subscription.is_subscription() && cell.probe_accepted {
        let started = subscription_weeks_at(token_contract, subscription, cell.dispute_time, true)?;
        let unstarted = subscription.sub_weeks.saturating_sub(started);
        let future_tokens = u128::from(unstarted)
            .checked_mul(subscription.tokens_per_week)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "release_dispute: {token_contract} future token refund overflows uint128"
                ))
            })?;
        let (future_pay, future_fee) =
            token_pay_and_fee(token_contract, future_tokens, cell.machine.price(), consts)?;
        future_pay
            .checked_add(future_fee)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "release_dispute: {token_contract} future refund overflows uint128"
                ))
            })?
            .min(remaining_deposit)
    } else {
        remaining_deposit
    };
    let unearned_burn = remaining_deposit - refundable_deposit;
    cell.burned = cell.burned.checked_add(unearned_burn).ok_or_else(|| {
        ChainError::Chain(format!(
            "release_dispute: {token_contract} unearned-period burn overflows uint128"
        ))
    })?;
    let refund = refundable_deposit
        .checked_add(cell.buyer_bond)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "release_dispute: {token_contract} buyer refund overflows uint128"
            ))
        })?;
    cell.buyer_refunded = cell.buyer_refunded.checked_add(refund).ok_or_else(|| {
        ChainError::Chain(format!(
            "release_dispute: {token_contract} cumulative buyer refund overflows uint128"
        ))
    })?;
    let seller_refund = cell.seller_locked;
    cell.seller_received = cell
        .seller_received
        .checked_add(seller_refund)
        .ok_or_else(|| {
            ChainError::Chain(format!(
                "release_dispute: {token_contract} finalized seller amount overflows uint128"
            ))
        })?;
    cell.buyer_locked = 0;
    cell.probe_locked = 0;
    cell.buyer_bond = 0;
    cell.seller_locked = 0;
    cell.disputed = false;
    if timed_out {
        let _ = cell.machine.resolve_dispute_timeout();
    } else {
        let _ = cell.machine.release_dispute();
    }
    cell.closed = true;
    Ok((refund, seller_refund))
}

fn require_mock_seller_actor(
    state: &MockState,
    token_contract: &TokenContract,
    note: &dyn Note,
    action: &str,
) -> Result<(), ChainError> {
    let expected = state.offer_sellers.get(token_contract).ok_or_else(|| {
        ChainError::Chain(format!(
            "{action}: {token_contract} has no persisted seller actor"
        ))
    })?;
    if note_id_hex(&note.pubkey()) != *expected {
        return Err(ChainError::Chain(format!(
            "{action}: {token_contract} requires the matched seller note"
        )));
    }
    Ok(())
}

fn require_mock_buyer_actor(
    state: &MockState,
    token_contract: &TokenContract,
    note: &dyn Note,
    action: &str,
) -> Result<(), ChainError> {
    let cell = state
        .streams
        .get(token_contract)
        .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
    if note_id_hex(&note.pubkey()) != note_id_hex(&cell.buyer_pubkey) {
        return Err(ChainError::Chain(format!(
            "{action}: {token_contract} requires the matched buyer note"
        )));
    }
    Ok(())
}

fn require_mock_buyer_note_addr(
    state: &MockState,
    token_contract: &TokenContract,
    buyer_note_addr: &str,
    action: &str,
) -> Result<(), ChainError> {
    let cell = state
        .streams
        .get(token_contract)
        .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
    let expected = format!("mock:{}", note_id_hex(&cell.buyer_pubkey));
    if buyer_note_addr.trim().to_ascii_lowercase() != expected {
        return Err(ChainError::Chain(format!(
            "{action}: {token_contract} requires the matched buyer note"
        )));
    }
    Ok(())
}

fn stop_mock_stream(
    state: &mut MockState,
    token_contract: &TokenContract,
    consts: &ProtocolConsts,
) -> Result<Settlement, ChainError> {
    let (streams, deal_subscriptions) = (&mut state.streams, &mut state.deal_subscriptions);
    let cell = streams
        .get_mut(token_contract)
        .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
    let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
        ChainError::Chain(format!(
            "stop: {token_contract} has no persisted deal shape"
        ))
    })?;
    if cell.closed {
        return Err(ChainError::Chain(format!(
            "stop: {token_contract} is already closed"
        )));
    }
    if cell.disputed {
        return Err(ChainError::Chain(format!(
            "stop: {token_contract} is disputed"
        )));
    }
    let settlement = if !cell.probe_accepted {
        let Settlement::BurnBoth(mut burn) = cell.machine.buyer_stop() else {
            return Err(ChainError::Chain(format!(
                "stop: {token_contract} pre-probe state did not produce ProbeBurned"
            )));
        };
        let probe = cell.probe_locked;
        let seller_burn = probe.min(cell.seller_locked);
        let refund = cell
            .buyer_locked
            .checked_add(cell.buyer_bond)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "stop: {token_contract} pre-probe buyer refund overflows uint128"
                ))
            })?;
        cell.burned = cell
            .burned
            .checked_add(probe)
            .and_then(|amount| amount.checked_add(seller_burn))
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "stop: {token_contract} pre-probe burn overflows uint128"
                ))
            })?;
        cell.buyer_refunded = cell.buyer_refunded.checked_add(refund).ok_or_else(|| {
            ChainError::Chain(format!(
                "stop: {token_contract} cumulative buyer refund overflows uint128"
            ))
        })?;
        cell.seller_received = cell
            .seller_received
            .checked_add(cell.seller_locked - seller_burn)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "stop: {token_contract} finalized seller amount overflows uint128"
                ))
            })?;
        cell.buyer_locked = 0;
        cell.probe_locked = 0;
        cell.buyer_bond = 0;
        cell.seller_locked = 0;
        cell.machine.close();
        cell.closed = true;
        burn.buyer_refund = refund;
        Settlement::BurnBoth(burn)
    } else {
        promote_due(token_contract, cell, unix_now_secs(), consts)?;
        if subscription.is_subscription() {
            let started =
                subscription_weeks_at(token_contract, subscription, unix_now_secs(), true)?;
            charge_subscription_through(token_contract, cell, subscription, started, consts)?;
        }
        let trusted_tokens = cell.tokens_final;
        if trusted_tokens > subscription.tokens_paid {
            credit_tokens_through(token_contract, cell, subscription, trusted_tokens, consts)?;
        }
        let delivered = delivered_ticks(token_contract, cell)?;
        let _ = cell.machine.buyer_stop();
        let refund = close_mock_clean(token_contract, cell, delivered, consts)?;
        let paid_ticks = finalized_ticks(token_contract, subscription.tokens_paid)?;
        cell.buyer_stop_settlement = Some((cell.seller_received, refund));
        Settlement::AmicableSplit {
            to_seller_ticks: paid_ticks,
            to_buyer_refund: refund,
        }
    };
    Ok(settlement)
}

#[async_trait]
impl ChainBackend for MockChainBackend {
    fn network(&self) -> &str {
        "mock"
    }

    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        Ok(st
            .offers
            .values()
            .map(|o| OfferListing {
                seller_id: st
                    .offer_sellers
                    .get(&o.token_contract)
                    .cloned()
                    .unwrap_or_default(),
                token_contract: o.token_contract.clone(),
                price_per_tick: o.price_per_tick,
                max_ticks: o.max_ticks,
            })
            .collect())
    }

    async fn post_offer(&self, offer: SellOffer, note: &dyn Note) -> Result<(), ChainError> {
        validate_mock_sell_offer(&offer, &self.consts)?;
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        // Seller = hex of the note's ed-pubkey.
        let seller_id: String = note
            .pubkey()
            .ed
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if st.offers.contains_key(&offer.token_contract) {
            return Err(ChainError::Chain(format!(
                "duplicate active sell order for TokenContract {}: cancel/fill the old order before reposting",
                offer.token_contract
            )));
        }
        if st.matches.contains_key(&offer.token_contract)
            || st.matched_offers.contains_key(&offer.token_contract)
            || st.matched_ticks.contains_key(&offer.token_contract)
            || st.deal_subscriptions.contains_key(&offer.token_contract)
            || st.streams.contains_key(&offer.token_contract)
        {
            return Err(ChainError::Chain(format!(
                "mock TokenContract {} was already filled/matched; refusing to replace its seller",
                offer.token_contract
            )));
        }
        if st
            .offer_sellers
            .get(&offer.token_contract)
            .is_some_and(|original_seller| original_seller != &seller_id)
        {
            return Err(ChainError::Chain(format!(
                "mock TokenContract {} remains bound to its original seller after cancellation",
                offer.token_contract
            )));
        }
        st.offer_sellers
            .entry(offer.token_contract.clone())
            .or_insert(seller_id);
        // A SELL's deadline is mandatory and capped: `PrivateNote.postSellOffer` refuses `ttl == 0`
        // and requires `ttl <= MAX_SELL_TTL` (`contracts/dex/PrivateNote.sol:41,792`,
        // `ERR_SELL_DEADLINE_TOO_LONG`). The mock posts at the same canonical TTL the real backend
        // does, anchored here and only here.
        st.offer_deadlines.insert(
            offer.token_contract.clone(),
            unix_now_secs() + crate::params::MAX_SELL_TTL.as_secs(),
        );
        // `getDeal()` is constructor-bound, so it is recorded once and never re-derived from a later
        // posting: a successor ask that quietly changed the deal's terms would be a different deal
        // .
        st.deal_terms
            .entry(offer.token_contract.clone())
            .or_insert((offer.price_per_tick, offer.max_ticks));
        st.offers.insert(offer.token_contract.clone(), offer);
        self.store_state(&st)
    }

    /// `expireOrder` as the book implements it.

    /// Permissionless and idempotent in BOTH directions: `_doExpire` silently ignores an order that
    /// is already gone and one whose deadline has not passed
    /// (`contracts/airegistry/InferenceOrderBook.sol:1679-1691`), so a keeper may spam it and racing
    /// callers are harmless. Removing a SELL frees the deal's `_offerPosted` latch through
    /// `onSellClosed` -- modelled here by the ask leaving `offers`, which is the same fact this mock
    /// reports as the latch -- while the deal itself, its terms and its capacity survive
    /// (`contracts/airegistry/InferenceOrderBook.sol:1138-1149`).
    async fn expire_resting_sell_order(
        &self,
        token_contract: &TokenContract,
        order_id: u128,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        if !st.offers.contains_key(token_contract)
            || mock_current_order_id(&st, token_contract) != order_id
        {
            return Ok(());
        }
        let live = st
            .offer_deadlines
            .get(token_contract)
            .copied()
            .is_none_or(|deadline| unix_now_secs() < deadline);
        if live {
            return Ok(());
        }
        st.offers.remove(token_contract);
        *st.expired_generations
            .entry(token_contract.clone())
            .or_insert(0) += 1;
        self.store_state(&st)
    }

    async fn token_contract_offer_latch(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealOfferLatch>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        if !st.deal_terms.contains_key(token_contract) && !st.offers.contains_key(token_contract) {
            return Ok(None);
        }
        Ok(Some(DealOfferLatch {
            offer_posted: st.offers.contains_key(token_contract),
        }))
    }

    async fn confirm_offer_outcome(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<SellOfferOutcome>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        if st.matches.contains_key(token_contract) {
            return Ok(Some(SellOfferOutcome::Matched));
        }
        Ok(st
            .offers
            .contains_key(token_contract)
            .then(|| SellOfferOutcome::Rested {
                order_id: mock_current_order_id(&st, token_contract),
            }))
    }

    async fn raw_resting_sell_orders_for_tc(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Vec<OrderBookOrder>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        let Some(offer) = st.offers.get(token_contract) else {
            return Ok(Vec::new());
        };
        let owner = st
            .offer_sellers
            .get(token_contract)
            .map(|seller| format!("0:{seller}"))
            .unwrap_or_default();
        Ok(vec![OrderBookOrder {
            order_id: mock_current_order_id(&st, token_contract),
            owner_note: owner,
            token_contract: Some(token_contract.clone()),
            is_buy: false,
            price_per_tick: u128::from(offer.price_per_tick),
            ticks: u128::from(offer.max_ticks),
            escrow: 0,
            // The deadline anchored when this offer was posted. A zero here would be a shape the chain
            // cannot produce, and every deadline-aware view reads it as malformed; the
            // fallback covers only a state file written before this field existed.
            deadline: st
                .offer_deadlines
                .get(token_contract)
                .copied()
                .unwrap_or_else(|| unix_now_secs() + crate::params::MAX_SELL_TTL.as_secs()),
            flags: offer.flags,
            timestamp: 0,
        }])
    }

    async fn cancel_resting_sell_order(
        &self,
        token_contract: &TokenContract,
        order_id: u128,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let expected = mock_current_order_id(&st, token_contract);
        if order_id != expected || !st.offers.contains_key(token_contract) {
            return Err(ChainError::Chain(format!(
                "resting SELL {order_id} is absent for TokenContract {token_contract}"
            )));
        }
        st.offers.remove(token_contract);
        self.store_state(&st)
    }

    async fn place_buy(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<(), ChainError> {
        self.place_buy_ticks_inner(token_contract, note, None)
    }

    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        st.matches
            .get(token_contract)
            .cloned()
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))
    }

    async fn read_openable_match_now(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        if let Some(cell) = st.streams.get(token_contract) {
            let lifecycle = if cell.closed {
                "terminal"
            } else {
                "already open"
            };
            return Err(ChainError::Chain(format!(
                "mock TokenContract {token_contract} is {lifecycle}, not openable for seller resume"
            )));
        }
        Ok(st.matches.get(token_contract).cloned())
    }

    async fn sell_offer_terms(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(u64, u64)>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        // the deal's constructor-bound terms come first, because they are the ones that survive
        // the ask leaving the book. Falling back to a resting or consumed ask keeps every caller that
        // predates `deal_terms` reading exactly what it read before.
        Ok(st.deal_terms.get(token_contract).copied().or_else(|| {
            st.offers
                .get(token_contract)
                .or_else(|| st.matched_offers.get(token_contract))
                .map(|offer| (offer.price_per_tick, offer.max_ticks))
        }))
    }

    async fn poll_seller_fills(
        &self,
        note: &dyn Note,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        let seller_id = note_id_hex(&note.pubkey());
        let mut batch = st
            .matches
            .keys()
            .filter(|token_contract| st.offer_sellers.get(*token_contract) == Some(&seller_id))
            .filter_map(|token_contract| {
                let offer = st.matched_offers.get(token_contract)?;
                let created_at = st
                    .match_created_at
                    .get(token_contract)
                    .copied()
                    .unwrap_or(cursor.since_unix);
                (!cursor.has_seen(created_at, token_contract)).then(|| {
                    (
                        created_at,
                        MatchedFill {
                            order_id: mock_current_order_id(&st, token_contract),
                            token_contract: token_contract.clone(),
                            ticks: u128::from(
                                st.matched_ticks
                                    .get(token_contract)
                                    .copied()
                                    .unwrap_or(offer.max_ticks),
                            ),
                            price_per_tick: u128::from(offer.price_per_tick),
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        batch.sort_by(|(left_at, left), (right_at, right)| {
            left_at
                .cmp(right_at)
                .then_with(|| left.token_contract.cmp(&right.token_contract))
        });
        cursor.record_seen_batch(
            batch
                .iter()
                .map(|(created_at, fill)| (*created_at, fill.token_contract.clone())),
        );
        Ok(batch.into_iter().map(|(_, fill)| fill).collect())
    }

    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        enc_endpoint: Vec<u8>,
        note: &dyn Note,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        require_mock_seller_actor(&st, token_contract, note, "open_stream")?;
        if let Some(cell) = st.streams.get(token_contract) {
            let lifecycle = if cell.closed {
                "terminal"
            } else {
                "already open"
            };
            return Err(ChainError::Chain(format!(
                "open_stream: {token_contract} has an {lifecycle} persisted stream"
            )));
        }
        let m = st
            .matches
            .get(token_contract)
            .cloned()
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))?;

        // Ceiling on cumulative claimed ticks from the consumed offer.
        let max_ticks = st
            .matched_ticks
            .get(token_contract)
            .copied()
            .or_else(|| {
                st.matched_offers
                    .get(token_contract)
                    .or_else(|| st.offers.get(token_contract))
                    .map(|offer| offer.max_ticks)
            })
            .unwrap_or(u64::MAX);
        let subscription = st.deal_subscriptions.get(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "open_stream: {token_contract} has no persisted deal shape"
            ))
        })?;
        let (pay, fee) = token_pay_and_fee(
            token_contract,
            subscription.funded_tokens,
            m.price_per_tick,
            &self.consts,
        )?;
        let gross_deposit = pay.checked_add(fee).ok_or_else(|| {
            ChainError::Chain(format!(
                "open_stream: {token_contract} buyer deposit plus fee overflows uint128"
            ))
        })?;
        let probe = u128::from(m.price_per_tick);
        let buyer_locked = gross_deposit.checked_sub(probe).ok_or_else(|| {
            ChainError::Chain(format!(
                "open_stream: {token_contract} buyer deposit cannot freeze the probe"
            ))
        })?;
        let machine = StreamMachine::open(m.price_per_tick, &self.params);
        let opened_at = unix_now_secs();
        let cell = StreamCell {
            schema_version: STREAM_CELL_SCHEMA_VERSION,
            machine,
            probe_accepted: false,
            buyer_pubkey: m.buyer_pubkey.clone(),
            seller_locked: 2 * u128::from(m.price_per_tick),
            buyer_locked,
            probe_locked: probe,
            probe_time: opened_at,
            buyer_bond: if subscription.is_subscription() {
                2 * probe
            } else {
                0
            },
            fee_accrued: 0,
            tokens_final: 0,
            tokens_pending: 0,
            last_claim_time: opened_at,
            seller_received: 0,
            buyer_refunded: 0,
            burned: 0,
            closed: false,
            max_ticks,
            disputed: false,
            dispute_time: 0,
            buyer_stop_settlement: None,
        };
        st.streams.insert(token_contract.clone(), cell);
        self.store_state(&st)?;

        // Seam: the enc-endpoint is placed into the endpoints file.
        let mut ef = self.read_endpoints()?;
        ef.handovers.insert(token_contract.clone(), enc_endpoint);
        self.write_endpoints(&ef)?;
        Ok(())
    }

    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let ef = self.read_endpoints()?;
        Ok(ef.handovers.get(token_contract).cloned())
    }

    /// Permissionless promotion (`finalize`): only a claim whose own window elapsed becomes trusted.
    async fn finalize(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let (streams, deal_subscriptions) = (&mut st.streams, &mut st.deal_subscriptions);
        let cell = streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "finalize: {token_contract} has no persisted deal shape"
            ))
        })?;
        if cell.closed || cell.disputed || !cell.probe_accepted {
            return Err(ChainError::Chain(format!(
                "finalize: {token_contract} is not an open undisputed accepted deal"
            )));
        }
        let now = unix_now_secs();
        if !has_due_claim(token_contract, cell, now, &self.consts)? {
            return Err(ChainError::Chain(format!(
                "finalize: {token_contract} has no claim whose promotion window elapsed"
            )));
        }
        promote_due(token_contract, cell, now, &self.consts)?;
        if cell.tokens_final == cell.tokens_pending {
            cell.machine
                .on_promote()
                .map_err(|e| ChainError::EndpointsFile(e.0.to_string()))?;
        }
        if !subscription.is_subscription() && cell.tokens_final >= subscription.funded_tokens {
            let target = cell.tokens_final;
            credit_tokens_through(token_contract, cell, subscription, target, &self.consts)?;
            let delivered = delivered_ticks(token_contract, cell)?;
            close_mock_clean(token_contract, cell, delivered, &self.consts)?;
        }
        self.store_state(&st)
    }

    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let (streams, deal_subscriptions) = (&mut st.streams, &mut st.deal_subscriptions);
        let cell = streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "accept_probe: {token_contract} has no persisted deal shape"
            ))
        })?;
        if cell.closed || cell.disputed || cell.probe_accepted {
            return Err(ChainError::Chain(format!(
                "accept_probe: {token_contract} is not an open undisputed probe"
            )));
        }
        let now = unix_now_secs();
        let deadline = cell
            .probe_time
            .checked_add(self.consts.probe_window.as_secs())
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "accept_probe: {token_contract} probe deadline overflows uint64"
                ))
            })?;
        if now < deadline {
            return Err(ChainError::Chain(format!(
                "accept_probe: {token_contract} probe window is still open"
            )));
        }
        cell.machine
            .on_probe_accepted()
            .map_err(|e| ChainError::EndpointsFile(e.0.to_string()))?;
        cell.probe_accepted = true;
        let probe = cell.probe_locked;
        let (_, fee) = token_pay_and_fee(
            token_contract,
            TICK_SIZE,
            cell.machine.price(),
            &self.consts,
        )?;
        if fee > cell.buyer_locked {
            return Err(ChainError::Chain(format!(
                "accept_probe: {token_contract} probe fee {fee} exceeds buyer escrow {}",
                cell.buyer_locked
            )));
        }
        cell.buyer_locked -= fee;
        cell.probe_locked = 0;
        cell.seller_received = cell.seller_received.checked_add(probe).ok_or_else(|| {
            ChainError::Chain(format!(
                "accept_probe: {token_contract} seller proceeds overflow uint128"
            ))
        })?;
        cell.fee_accrued = cell.fee_accrued.checked_add(fee).ok_or_else(|| {
            ChainError::Chain(format!(
                "accept_probe: {token_contract} accrued fee overflows uint128"
            ))
        })?;
        subscription.tokens_paid = TICK_SIZE;
        let accepted_at = now;
        subscription.period_start = accepted_at;
        subscription.week_base_tokens = 0;
        cell.tokens_final = TICK_SIZE;
        cell.tokens_pending = TICK_SIZE;
        cell.last_claim_time = accepted_at;
        self.store_state(&st)
    }

    async fn claim_tokens(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
        cumulative_tokens: u128,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        require_mock_seller_actor(&st, token_contract, note, "claim_tokens")?;
        let (streams, deal_subscriptions) = (&mut st.streams, &mut st.deal_subscriptions);
        let cell = streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "claim_tokens: {token_contract} has no persisted deal shape"
            ))
        })?;
        // TokenContract checks lifecycle gates before its cumulative high-water equality no-op.
        // An equal-value retry is therefore idempotent only while the deal is open, undisputed,
        // and probe-accepted; it must not revive a terminal or frozen deal.
        if cell.closed || cell.disputed || !cell.probe_accepted {
            return Err(ChainError::Chain(format!(
                "claim_tokens: {token_contract} is not an open undisputed accepted deal"
            )));
        }
        if cell.machine.on_probe() {
            return Err(ChainError::Chain(format!(
                "claim_tokens: {token_contract} persisted lifecycle disagrees with probe acceptance"
            )));
        }
        if cumulative_tokens < cell.tokens_pending {
            return Err(ChainError::Chain(format!(
                "claim_tokens: cumulative {cumulative_tokens} tokens regresses pending total {}",
                cell.tokens_pending
            )));
        }
        if cumulative_tokens == cell.tokens_pending {
            return Ok(());
        }
        if subscription.is_subscription() {
            // `claimTokens` first crosses every elapsed boundary before calculating the current-week cap.
            charge_elapsed_subscription_weeks(
                token_contract,
                cell,
                subscription,
                unix_now_secs(),
                &self.consts,
            )?;
        }
        // the mock does not deliver beyond the offer's `max_ticks` (the real TC is bounded by the
        // deposit instead). Reject a claim that would exceed the ceiling rather than silently trimming it,
        // matching the contract's fail-closed behaviour on an over-cap claim.
        let max_tokens = u128::from(cell.max_ticks)
            .checked_mul(TICK_SIZE)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "claim_tokens: {token_contract} maximum token volume overflows uint128"
                ))
            })?;
        if cumulative_tokens > max_tokens {
            return Err(ChainError::Limit(format!(
                "claim_tokens: cumulative {cumulative_tokens} tokens exceeds max_tokens ({max_tokens}) -- the mock \
                 does not deliver beyond the offer",
            )));
        }
        if subscription.is_subscription() {
            let cap = if subscription.week_index >= subscription.sub_weeks {
                // The subscription term is over. `claimTokens` may still receive an idempotent
                // retry of the existing high-water while its final promotion window is open, but
                // no new delivery can belong to a fifth week.
                cell.tokens_pending
            } else {
                subscription
                    .week_base_tokens
                    .checked_add(subscription.tokens_per_week)
                    .ok_or_else(|| {
                        ChainError::Chain(format!(
                            "claim_tokens: {token_contract} subscription weekBaseTokens + \
                             tokensPerWeek overflows uint128"
                        ))
                    })?
                    .min(subscription.funded_tokens)
            };
            if cumulative_tokens > cap {
                return Err(ChainError::Limit(format!(
                    "claim_tokens: cumulative {cumulative_tokens} tokens exceeds current subscription \
                     cap {cap}; unused weekly quota does not roll forward"
                )));
            }
        }
        let claim_time = unix_now_secs();
        let elapsed = claim_time
            .checked_sub(cell.last_claim_time)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "claim_tokens: {token_contract} logical claim clock regresses"
                ))
            })?;
        if elapsed < self.consts.min_claim_interval.as_secs() {
            return Err(ChainError::Chain(format!(
                "claim_tokens: {token_contract} minimum claim interval is still open"
            )));
        }
        let delta = cumulative_tokens - cell.tokens_pending;
        if delta > MAX_CLAIM_DELTA {
            return Err(ChainError::Limit(format!(
                "claim_tokens: delta {delta} exceeds MAX_CLAIM_DELTA {MAX_CLAIM_DELTA}"
            )));
        }
        let produced_time = delta
            .checked_mul(u128::from(self.consts.min_seconds_per_tick.as_secs()))
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "claim_tokens: {token_contract} physical-rate numerator overflows uint128"
                ))
            })?;
        let allowed_time = u128::from(elapsed).checked_mul(TICK_SIZE).ok_or_else(|| {
            ChainError::Chain(format!(
                "claim_tokens: {token_contract} physical-rate allowance overflows uint128"
            ))
        })?;
        if produced_time > allowed_time {
            return Err(ChainError::Limit(format!(
                "claim_tokens: delta {delta} exceeds the physical rate allowance"
            )));
        }
        promote_due(token_contract, cell, claim_time, &self.consts)?;
        if cell.tokens_pending != cell.tokens_final {
            return Err(ChainError::Chain(format!(
                "claim_tokens: {token_contract} pending claim slot is still inside its promotion window"
            )));
        }
        let machine_ticks = u64::try_from(cumulative_tokens.div_ceil(TICK_SIZE)).map_err(|_| {
            ChainError::Chain(format!(
                "claim_tokens: cumulative {cumulative_tokens} tokens does not fit the lifecycle tick counter"
            ))
        })?;
        cell.machine
            .on_claim(machine_ticks)
            .map_err(|e| ChainError::EndpointsFile(e.0.to_string()))?;
        cell.tokens_pending = cumulative_tokens;
        cell.last_claim_time = claim_time;
        self.store_state(&st)
    }

    async fn settle_week(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let (streams, deal_subscriptions) = (&mut st.streams, &mut st.deal_subscriptions);
        let cell = streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "settle_week: {token_contract} has no persisted deal shape"
            ))
        })?;
        if !subscription.is_subscription() {
            return Err(ChainError::Chain(format!(
                "settle_week: {token_contract} is an ordinary deal"
            )));
        }
        let now = unix_now_secs();
        let crossed = charge_elapsed_subscription_weeks(
            token_contract,
            cell,
            subscription,
            now,
            &self.consts,
        )?;
        if !crossed && subscription.week_index < subscription.sub_weeks {
            return Err(ChainError::Chain(format!(
                "settle_week: {token_contract} has no crossed weekly boundary"
            )));
        }
        if subscription.week_index >= subscription.sub_weeks
            && claim_deadline_reached(
                token_contract,
                cell.last_claim_time,
                self.consts.claim_promote_window,
                now,
            )?
        {
            promote_due(token_contract, cell, now, &self.consts)?;
            if cell.tokens_final == cell.tokens_pending {
                cell.machine
                    .on_promote()
                    .map_err(|error| ChainError::EndpointsFile(error.0.to_string()))?;
            }
            let trusted = cell.tokens_final;
            if trusted > subscription.tokens_paid {
                credit_tokens_through(token_contract, cell, subscription, trusted, &self.consts)?;
            }
            let delivered = delivered_ticks(token_contract, cell)?;
            close_mock_clean(token_contract, cell, delivered, &self.consts)?;
        }
        self.store_state(&st)
    }

    async fn stop(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        require_mock_buyer_actor(&st, token_contract, note, "stop")?;
        let settlement = stop_mock_stream(&mut st, token_contract, &self.consts)?;
        self.store_state(&st)?;
        Ok(settlement)
    }

    async fn dispute(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        require_mock_buyer_actor(&st, token_contract, note, "dispute")?;
        let is_subscription = st
            .deal_subscriptions
            .get(token_contract)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "dispute: {token_contract} has no persisted deal shape"
                ))
            })?
            .is_subscription();
        let receipt = {
            let cell = st
                .streams
                .get_mut(token_contract)
                .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
            if cell.closed || cell.disputed {
                return Err(ChainError::Chain(format!(
                    "dispute: {token_contract} is not an undisputed open deal"
                )));
            }
            let bond_required =
                u128::from(cell.machine.price())
                    .checked_mul(2)
                    .ok_or_else(|| {
                        ChainError::Chain(format!(
                            "dispute: {token_contract} two-tick bond overflows uint128"
                        ))
                    })?;
            let buyer_bond_required = if is_subscription { bond_required } else { 0 };
            let at = unix_now_secs();
            let buyer = format!("mock:{}", note_id_hex(&cell.buyer_pubkey));
            let pre_bonds = SettlementActionBondState {
                seller_bond_held: cell.seller_locked.into(),
                seller_bond_required: bond_required.into(),
                buyer_bond_held: cell.buyer_bond.into(),
                buyer_bond_required: buyer_bond_required.into(),
            };
            cell.machine.buyer_dispute();
            cell.disputed = true;
            cell.dispute_time = at;
            SettlementActionReceipt {
                token_contract: token_contract.clone(),
                action: SettlementAction::Dispute,
                message_id: format!("mock:{token_contract}:dispute:{at}"),
                created_at: at,
                event: SettlementActionEvent::StreamDisputed { buyer, at },
                pre_bonds,
                post_state: Some(SettlementActionPostState {
                    tokens_final: cell.tokens_final.into(),
                    tokens_pending: cell.tokens_pending.into(),
                    seller_bond_held: cell.seller_locked.into(),
                    seller_bond_required: bond_required.into(),
                    buyer_bond_held: cell.buyer_bond.into(),
                    buyer_bond_required: buyer_bond_required.into(),
                    opened: true,
                    disputed: true,
                }),
            }
        };
        self.store_state(&st)?;
        Ok(Settlement::AuthoritativeReceipt(Box::new(receipt)))
    }

    async fn release_dispute(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let (streams, deal_subscriptions) = (&mut st.streams, &mut st.deal_subscriptions);
        let cell = streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "release_dispute: {token_contract} has no persisted deal shape"
            ))
        })?;
        let (to_buyer, seller_bond) =
            resolve_mock_dispute(token_contract, cell, subscription, &self.consts, false)?;
        let settlement = Settlement::SellerNoShow {
            to_buyer_refund: to_buyer,
            seller_bond_returned: seller_bond,
        };
        self.store_state(&st)?;
        Ok(settlement)
    }

    async fn resolve_dispute_timeout(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let (streams, deal_subscriptions) = (&mut st.streams, &mut st.deal_subscriptions);
        let cell = streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        if !cell.disputed || cell.dispute_time == 0 {
            return Err(ChainError::Chain(format!(
                "resolve_dispute_timeout: {token_contract} is not in a timed dispute"
            )));
        }
        let deadline = cell
            .dispute_time
            .checked_add(self.consts.dispute_window.as_secs())
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "resolve_dispute_timeout: {token_contract} dispute deadline overflows uint64"
                ))
            })?;
        if unix_now_secs() < deadline {
            return Err(ChainError::Chain(format!(
                "resolve_dispute_timeout: {token_contract} dispute window is still open"
            )));
        }
        let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "resolve_dispute_timeout: {token_contract} has no persisted deal shape"
            ))
        })?;
        let (to_buyer, seller_bond) =
            resolve_mock_dispute(token_contract, cell, subscription, &self.consts, true)?;
        let settlement = Settlement::SellerNoShow {
            to_buyer_refund: to_buyer,
            seller_bond_returned: seller_bond,
        };
        self.store_state(&st)?;
        Ok(settlement)
    }

    /// The seller abandons an open deal (`sellerStop`). An ordinary deal pays trusted consumption;
    /// a subscription pays completed weeks only and adds nothing for the unfinished current week.
    /// The buyer takes back the rest of the escrow along with the returned bond.
    async fn seller_stop(&self, token_contract: &TokenContract) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let (streams, deal_subscriptions) = (&mut st.streams, &mut st.deal_subscriptions);
        let cell = streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let subscription = deal_subscriptions.get_mut(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "seller_stop: {token_contract} has no persisted deal shape"
            ))
        })?;
        if cell.closed {
            return Err(ChainError::Chain(format!(
                "seller_stop: {token_contract} is already closed"
            )));
        }
        if cell.disputed {
            return Err(ChainError::Chain(format!(
                "seller_stop: {token_contract} is disputed"
            )));
        }
        if !cell.probe_accepted {
            let seller_burn = u128::from(cell.machine.price()).min(cell.seller_locked);
            let refund = cell
                .buyer_locked
                .checked_add(cell.probe_locked)
                .and_then(|amount| amount.checked_add(cell.buyer_bond))
                .ok_or_else(|| {
                    ChainError::Chain(format!(
                        "seller_stop: {token_contract} pre-probe buyer refund overflows uint128"
                    ))
                })?;
            cell.burned = cell.burned.checked_add(seller_burn).ok_or_else(|| {
                ChainError::Chain(format!(
                    "seller_stop: {token_contract} seller burn overflows uint128"
                ))
            })?;
            cell.buyer_refunded = cell.buyer_refunded.checked_add(refund).ok_or_else(|| {
                ChainError::Chain(format!(
                    "seller_stop: {token_contract} cumulative buyer refund overflows uint128"
                ))
            })?;
            cell.seller_received = cell
                .seller_received
                .checked_add(cell.seller_locked - seller_burn)
                .ok_or_else(|| {
                    ChainError::Chain(format!(
                        "seller_stop: {token_contract} finalized seller amount overflows uint128"
                    ))
                })?;
            cell.buyer_locked = 0;
            cell.probe_locked = 0;
            cell.buyer_bond = 0;
            cell.seller_locked = 0;
            cell.machine.close();
            cell.closed = true;
            self.store_state(&st)?;
            return Ok(Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: refund,
            });
        }
        promote_due(token_contract, cell, unix_now_secs(), &self.consts)?;
        if subscription.is_subscription() {
            let elapsed =
                subscription_weeks_at(token_contract, subscription, unix_now_secs(), false)?;
            charge_subscription_through(token_contract, cell, subscription, elapsed, &self.consts)?;
        }
        let trusted_tokens = cell.tokens_final;
        if !subscription.is_subscription() && trusted_tokens > subscription.tokens_paid {
            credit_tokens_through(
                token_contract,
                cell,
                subscription,
                trusted_tokens,
                &self.consts,
            )?;
        }
        let delivered = delivered_ticks(token_contract, cell)?;
        let _ = cell.machine.seller_stop();
        let refund = close_mock_clean(token_contract, cell, delivered, &self.consts)?;
        let paid_ticks = finalized_ticks(token_contract, subscription.tokens_paid)?;
        self.store_state(&st)?;
        Ok(Settlement::AmicableSplit {
            to_seller_ticks: paid_ticks,
            to_buyer_refund: refund,
        })
    }

    async fn cleanup_unopened(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        if st.streams.contains_key(token_contract) {
            return Err(ChainError::Chain(format!(
                "cleanup_unopened: {token_contract} is already opened"
            )));
        }
        if !st.matches.contains_key(token_contract) {
            return Err(ChainError::NoMatch(token_contract.clone()));
        }
        let state = mock_deal_state(&st, token_contract, &self.consts)?
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))?;
        let funded_time = state.funded_time.ok_or_else(|| {
            invalid_persisted_state(token_contract, "funded deal has no fundedTime")
        })?;
        let cleanup_at = funded_time
            .checked_add(MATCH_OPEN_TIMEOUT_SECS)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "cleanup_unopened: {token_contract} cleanup deadline overflows uint64"
                ))
            })?;
        let now = unix_now_secs();
        if now < cleanup_at {
            return Err(ChainError::Chain(format!(
                "cleanup_unopened: {token_contract} MATCH_OPEN_TIMEOUT is still open until {cleanup_at}"
            )));
        }
        let subscription = st.deal_subscriptions.get(token_contract).ok_or_else(|| {
            invalid_persisted_state(token_contract, "funded deal has no persisted deal shape")
        })?;
        let matched = st.matches.get(token_contract).ok_or_else(|| {
            invalid_persisted_state(token_contract, "funded deal has no persisted Match")
        })?;
        let seller_bond = u128::from(matched.price_per_tick)
            .checked_mul(2)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "cleanup_unopened: {token_contract} seller bond overflows uint128"
                ))
            })?;
        let buyer_bond = if subscription.is_subscription() {
            seller_bond
        } else {
            0
        };
        let buyer_refund = state.deposit.checked_add(buyer_bond).ok_or_else(|| {
            ChainError::Chain(format!(
                "cleanup_unopened: {token_contract} buyer deposit plus bond overflows uint128"
            ))
        })?;
        st.matches.remove(token_contract);
        st.match_created_at.remove(token_contract);
        st.matched_offers.remove(token_contract);
        st.matched_ticks.remove(token_contract);
        st.deal_subscriptions.remove(token_contract);
        self.store_state(&st)?;
        Ok(Settlement::SellerNoShow {
            to_buyer_refund: buyer_refund,
            seller_bond_returned: seller_bond,
        })
    }

    async fn buyer_stop_settlement(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(u128, u128)>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        Ok(st
            .streams
            .get(token_contract)
            .and_then(|cell| cell.buyer_stop_settlement))
    }

    async fn deal_state(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        mock_deal_state(&st, token_contract, &self.consts)
    }

    async fn deal_subscription(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealSubscription>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        mock_deal_subscription(&st, token_contract)
    }

    async fn deal_snapshot(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainSnapshot>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        let Some(state) = mock_deal_state(&st, token_contract, &self.consts)? else {
            return Ok(None);
        };
        let subscription = mock_deal_subscription(&st, token_contract)?.ok_or_else(|| {
            ChainError::Chain(format!(
                "mock TokenContract {token_contract}: state exists without matched deal terms"
            ))
        })?;
        let offer = st.matched_offers.get(token_contract).ok_or_else(|| {
            ChainError::Chain(format!(
                "mock TokenContract {token_contract}: state exists without a consumed offer"
            ))
        })?;
        let bond_required = u128::from(offer.price_per_tick)
            .checked_mul(2)
            .ok_or_else(|| {
                ChainError::Chain(format!(
                    "mock TokenContract {token_contract}: two-tick bond overflows uint128"
                ))
            })?;
        let seller_bond_held = st
            .streams
            .get(token_contract)
            .map(|cell| cell.seller_locked)
            .unwrap_or(bond_required);
        let subscription_bond_required = if subscription.is_subscription() {
            bond_required
        } else {
            0
        };
        let subscription_bond_held = st
            .streams
            .get(token_contract)
            .map(|cell| cell.buyer_bond)
            .unwrap_or(subscription_bond_required);
        let snapshot = DealChainSnapshot {
            account_code_hash: "mock-token-contract".to_string(),
            account_boc_hash: format!("mock:{token_contract}"),
            state,
            subscription,
            seller_bond: DealSellerBond {
                bond_funded: true,
                bond_held: seller_bond_held,
                bond_required,
            },
            buyer_bond: DealBuyerBond {
                bond_held: subscription_bond_held,
                bond_required: subscription_bond_required,
            },
        };
        snapshot
            .validate_cross_getter_invariants()
            .map_err(|reason| invalid_persisted_state(token_contract, reason))?;
        Ok(Some(snapshot))
    }

    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot> {
        self.checked_snapshot(token_contract).await.ok().flatten()
    }

    /// Full scan of the note's state: own offers + deals (as seller/buyer)
    /// with the anonymous counterparty and by-fact settlement + exposure (locked in open deals).
    /// The lists are taken under the lock, then the lock is released -- by-fact snapshots are pulled via separate
    /// `snapshot` calls (the sync Mutex is not reentrant). Read only.
    async fn note_snapshot(&self, note: &NotePubkey) -> Result<NoteSnapshot, ChainError> {
        let note_id = note_id_hex(note);
        let (offers, deal_keys) = {
            let _g = self.lock.lock().unwrap();
            let st = self.load_state()?;
            let mut offers = Vec::new();
            for (tc, o) in &st.offers {
                if st.offer_sellers.get(tc) == Some(&note_id) {
                    offers.push(OfferListing {
                        seller_id: note_id.clone(),
                        token_contract: tc.clone(),
                        price_per_tick: o.price_per_tick,
                        max_ticks: o.max_ticks,
                    });
                }
            }
            let mut deal_keys: Vec<(TokenContract, DealRole, Option<String>, Shell)> = Vec::new();
            let mut seen = HashSet::new();
            for (tc, seller) in &st.offer_sellers {
                if seller == &note_id
                    && (st.offers.contains_key(tc)
                        || st.matches.contains_key(tc)
                        || st.matched_offers.contains_key(tc)
                        || st.streams.contains_key(tc))
                {
                    let counterparty = st.matches.get(tc).map(|m| note_id_hex(&m.buyer_pubkey));
                    let price = st
                        .matches
                        .get(tc)
                        .map(|m| m.price_per_tick)
                        .or_else(|| st.matched_offers.get(tc).map(|o| o.price_per_tick))
                        .or_else(|| st.offers.get(tc).map(|o| o.price_per_tick))
                        .unwrap_or(0);
                    deal_keys.push((tc.clone(), DealRole::Seller, counterparty, price));
                    seen.insert(tc.clone());
                }
            }
            for (tc, m) in &st.matches {
                if note_id_hex(&m.buyer_pubkey) == note_id && seen.insert(tc.clone()) {
                    let counterparty = st.offer_sellers.get(tc).cloned();
                    deal_keys.push((tc.clone(), DealRole::Buyer, counterparty, m.price_per_tick));
                }
            }
            (offers, deal_keys)
        };
        let mut deals = Vec::new();
        let mut exposure: Shell = 0;
        for (tc, role, counterparty, price) in deal_keys {
            let snapshot = self.snapshot(&tc).await;
            if let Some(s) = &snapshot {
                if !s.closed {
                    let locked = match role {
                        DealRole::Buyer => s.buyer_locked,
                        DealRole::Seller => s.seller_locked,
                    };
                    exposure =
                        exposure.saturating_add(Shell::try_from(locked).unwrap_or(Shell::MAX));
                }
            }
            deals.push(DealView {
                token_contract: tc,
                role,
                counterparty,
                price_per_tick: price,
                // The mock book carries no per-deal model (the offer has none); real model names are
                // resolved by the real-chain reader from the TC's RootModel.
                model: None,
                snapshot,
            });
        }
        Ok(NoteSnapshot {
            note_id,
            offers,
            deals,
            exposure,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::LocalNote;

    const TEST_PRICE: Shell = PRICE_STEP as Shell;

    async fn open_ordinary_fixture(
        chain: &MockChainBackend,
        token_contract: &str,
        seller: &LocalNote,
        buyer: &LocalNote,
        price: Shell,
    ) {
        chain
            .post_offer(
                SellOffer {
                    price_per_tick: price,
                    max_ticks: 4,
                    token_contract: token_contract.to_string(),
                    flags: 0,
                },
                seller,
            )
            .await
            .unwrap();
        chain
            .place_buy(&token_contract.to_string(), buyer)
            .await
            .unwrap();
        chain
            .open_stream(&token_contract.to_string(), vec![], seller)
            .await
            .unwrap();
        elapse_probe_window(chain, token_contract);
        chain
            .accept_probe(&token_contract.to_string())
            .await
            .unwrap();
    }

    fn elapse_probe_window(chain: &MockChainBackend, token_contract: &str) {
        let mut state = chain.load_state().unwrap();
        let cell = state.streams.get_mut(token_contract).unwrap();
        let elapsed_probe_time =
            unix_now_secs().saturating_sub(chain.consts.probe_window.as_secs());
        cell.probe_time = elapsed_probe_time;
        chain.store_state(&state).unwrap();
    }

    fn elapse_min_claim_interval(chain: &MockChainBackend, token_contract: &str) {
        let mut state = chain.load_state().unwrap();
        let cell = state.streams.get_mut(token_contract).unwrap();
        let interval = chain.consts.min_claim_interval.as_secs();
        cell.last_claim_time = cell.last_claim_time.saturating_sub(interval);
        chain.store_state(&state).unwrap();
    }

    async fn assert_corrupt_restart_rejected_without_mutation(
        endpoints: &std::path::Path,
        token_contract: &str,
        corrupted: serde_json::Value,
        expected: &str,
    ) {
        let state_path = endpoints.with_extension("chainstate.json");
        let corrupted = serde_json::to_vec(&corrupted).unwrap();
        std::fs::write(&state_path, &corrupted).unwrap();
        let restarted = MockChainBackend::new(
            endpoints.to_path_buf(),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let error = restarted
            .seller_stop(&token_contract.to_string())
            .await
            .expect_err("a corrupt persisted state must be rejected before a state transition");
        assert!(
            error.to_string().contains("invalid persisted state"),
            "{expected}: {error}"
        );
        assert!(error.to_string().contains(expected), "{expected}: {error}");
        assert_eq!(
            std::fs::read(state_path).unwrap(),
            corrupted,
            "rejected persisted state must not be rewritten or partially settled"
        );
    }

    #[tokio::test]
    async fn mock_deal_state_classifies_openable_and_open_matches() {
        let base_dir = tempfile::tempdir().expect("test temp dir");
        let base = base_dir.path();
        let chain = MockChainBackend::new(
            base.join("eps.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[83u8; 32]);
        let buyer = LocalNote::from_seed(&[84u8; 32]);
        let tc = "tc-contract-state-shape".to_string();
        chain
            .post_offer(
                SellOffer {
                    price_per_tick: TEST_PRICE,
                    max_ticks: 4,
                    token_contract: tc.clone(),
                    flags: 0,
                },
                &seller,
            )
            .await
            .unwrap();
        chain.place_buy_ticks(&tc, &buyer, 2).await.unwrap();
        let unopened = chain.deal_state(&tc).await.unwrap().unwrap();
        let funded_time = unopened.funded_time.unwrap();
        crate::market::check_matched_token_contract_state(
            &tc,
            unopened,
            funded_time,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .expect("mock match must expose the authoritative funded-never-opened shape");
        assert!(chain.read_openable_match_now(&tc).await.unwrap().is_some());

        chain.open_stream(&tc, vec![], &seller).await.unwrap();
        let open_error = chain
            .read_openable_match_now(&tc)
            .await
            .expect_err("an already-open deal is not a seller resume match");
        assert!(open_error.to_string().contains("already open"));
    }

    /// the mock book must expire an ask the way the deployed one does, because the seller's
    /// relist is decided entirely by what it reads back afterwards.

    /// `expireOrder` is permissionless and idempotent: a gone order AND a still-live one are silent
    /// no-ops (`contracts/airegistry/InferenceOrderBook.sol:1679-1691`). Reaping a SELL frees the
    /// deal's `_offerPosted` latch through `onSellClosed` while the deal, its constructor-bound terms
    /// and its capacity survive (`contracts/airegistry/InferenceOrderBook.sol:1138-1149`,
    /// `contracts/airegistry/TokenContract.sol:729-736`), and the successor gets a new order id
    /// because the book never hands a removed one back.
    #[tokio::test]
    async fn a_lapsed_ask_is_reaped_once_and_leaves_the_deal_relistable() {
        let base_dir = tempfile::tempdir().expect("test temp dir");
        let chain = MockChainBackend::new(
            base_dir.path().join("eps.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[91u8; 32]);
        let tc = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let offer = SellOffer {
            price_per_tick: TEST_PRICE,
            max_ticks: 64,
            token_contract: tc.clone(),
            flags: 0,
        };
        chain.post_offer(offer.clone(), &seller).await.unwrap();
        let resting = chain.raw_resting_sell_orders_for_tc(&tc).await.unwrap();
        assert_eq!(resting.len(), 1, "one ask rests for the deal");
        let first_id = resting[0].order_id;
        assert_eq!(
            chain.token_contract_offer_latch(&tc).await.unwrap(),
            Some(DealOfferLatch { offer_posted: true }),
            "posting an ask sets the deal's own latch"
        );

        // A live order is refused, silently: this is not a back-door cancel.
        chain
            .expire_resting_sell_order(&tc, first_id)
            .await
            .unwrap();
        assert_eq!(
            chain
                .raw_resting_sell_orders_for_tc(&tc)
                .await
                .unwrap()
                .len(),
            1,
            "an ask whose deadline has not passed is not expirable by anyone"
        );

        // The clock moves; the deal does not.
        let mut state = chain.load_state().unwrap();
        *state.offer_deadlines.get_mut(&tc).unwrap() = unix_now_secs() - 1;
        chain.store_state(&state).unwrap();

        chain
            .expire_resting_sell_order(&tc, first_id.wrapping_add(1_000))
            .await
            .unwrap();
        assert_eq!(
            chain
                .raw_resting_sell_orders_for_tc(&tc)
                .await
                .unwrap()
                .len(),
            1,
            "an id that is not this deal's resting ask removes nothing"
        );

        chain
            .expire_resting_sell_order(&tc, first_id)
            .await
            .unwrap();
        assert!(
            chain
                .raw_resting_sell_orders_for_tc(&tc)
                .await
                .unwrap()
                .is_empty(),
            "the lapsed ask is gone from the book"
        );
        assert_eq!(
            chain.token_contract_offer_latch(&tc).await.unwrap(),
            Some(DealOfferLatch {
                offer_posted: false,
            }),
            "the removal freed the deal's latch, which is what makes it re-listable"
        );
        assert_eq!(
            chain.sell_offer_terms(&tc).await.unwrap(),
            Some((TEST_PRICE, 64)),
            "getDeal is constructor-bound: the deal's capacity outlives the ask posted against it"
        );

        chain
            .expire_resting_sell_order(&tc, first_id)
            .await
            .unwrap();
        assert!(
            chain
                .raw_resting_sell_orders_for_tc(&tc)
                .await
                .unwrap()
                .is_empty(),
            "reaping an order that is already gone is a no-op, so keepers may race and spam it"
        );

        chain.post_offer(offer, &seller).await.unwrap();
        let successor = chain.raw_resting_sell_orders_for_tc(&tc).await.unwrap();
        assert_eq!(successor.len(), 1, "exactly one successor rests");
        assert_ne!(
            successor[0].order_id, first_id,
            "a reaped order id is never handed back to the successor"
        );
        assert_eq!(
            successor[0].ticks, 64,
            "the successor carries the deal's whole remaining capacity"
        );
        assert!(
            successor[0].deadline > unix_now_secs(),
            "the successor is anchored to a fresh finite deadline"
        );
    }

    #[tokio::test]
    async fn snapshot_arithmetic_fails_closed() {
        let base_dir = tempfile::tempdir().expect("test temp dir");
        let base = base_dir.path();
        let chain = MockChainBackend::new(
            base.join("eps.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[87u8; 32]);
        let buyer = LocalNote::from_seed(&[88u8; 32]);
        let tc = "tc-dispute-receipt".to_string();
        open_ordinary_fixture(&chain, &tc, &seller, &buyer, TEST_PRICE).await;
        let before = chain.checked_snapshot(&tc).await.unwrap().unwrap();

        let mut persisted = chain.load_state().unwrap();
        let cell = persisted.streams.get_mut(&tc).unwrap();
        cell.buyer_locked = u128::MAX;
        cell.buyer_bond = 1;
        chain.store_state(&persisted).unwrap();
        let corrupt_bytes = std::fs::read(&chain.state_path).unwrap();
        let overflow = chain
            .checked_snapshot(&tc)
            .await
            .expect_err("money overflow must not saturate into a plausible snapshot");
        assert!(overflow.to_string().contains("buyer lock overflows"));
        assert_eq!(std::fs::read(&chain.state_path).unwrap(), corrupt_bytes);

        let mut persisted = chain.load_state().unwrap();
        let cell = persisted.streams.get_mut(&tc).unwrap();
        cell.buyer_locked = before.buyer_locked;
        cell.buyer_bond = 0;
        cell.seller_locked = 2 * u128::from(TEST_PRICE) + 1;
        chain.store_state(&persisted).unwrap();
        let incoherent = chain
            .deal_snapshot(&tc)
            .await
            .expect_err("cross-getter bond drift must reject the snapshot");
        assert!(incoherent.to_string().contains("exceeds bondRequired"));
    }

    #[tokio::test]
    async fn persisted_stream_schema_rejects_missing_or_unknown_money_fields() {
        let base_dir = tempfile::tempdir().expect("test temp dir");
        let base = base_dir.path();
        let endpoints = base.join("eps.json");
        let chain = MockChainBackend::new(
            endpoints.clone(),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[39u8; 32]);
        let buyer = LocalNote::from_seed(&[40u8; 32]);
        let tc = "tc-versioned-stream".to_string();
        open_ordinary_fixture(&chain, &tc, &seller, &buyer, TEST_PRICE).await;
        elapse_min_claim_interval(&chain, &tc);
        chain
            .claim_tokens(&tc, &seller, TICK_SIZE + 5)
            .await
            .unwrap();

        let restarted = MockChainBackend::new(
            endpoints.clone(),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        assert_eq!(
            restarted.deal_state(&tc).await.unwrap().unwrap(),
            chain.deal_state(&tc).await.unwrap().unwrap(),
            "a complete current-version stream must restart without migration"
        );

        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&chain.state_path).unwrap()).unwrap();
        for missing in [
            "schema_version",
            "probe_accepted",
            "probe_locked",
            "probe_time",
            "buyer_bond",
            "fee_accrued",
            "tokens_final",
            "tokens_pending",
            "last_claim_time",
            "dispute_time",
        ] {
            let mut corrupted = original.clone();
            corrupted["streams"][&tc]
                .as_object_mut()
                .unwrap()
                .remove(missing);
            std::fs::write(&chain.state_path, serde_json::to_vec(&corrupted).unwrap()).unwrap();
            let error = restarted
                .deal_state(&tc)
                .await
                .expect_err("an incomplete persisted money/claim record must fail closed");
            assert!(error.to_string().contains(missing), "{missing}: {error}");
        }

        let mut unknown = original;
        unknown["streams"][&tc]["schema_version"] = serde_json::json!(4);
        std::fs::write(&chain.state_path, serde_json::to_vec(&unknown).unwrap()).unwrap();
        let error = restarted
            .deal_state(&tc)
            .await
            .expect_err("an unknown persisted stream schema must fail closed");
        assert!(error
            .to_string()
            .contains("unsupported persisted stream schema 4"));
    }

    #[tokio::test]
    async fn corrupted_raw_finalized_volume_fails_instead_of_fabricating_max_ticks() {
        let base_dir = tempfile::tempdir().expect("test temp dir");
        let base = base_dir.path();
        let chain = MockChainBackend::new(
            base.join("eps.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[41u8; 32]);
        let buyer = LocalNote::from_seed(&[42u8; 32]);
        let tc = "tc-corrupt-finalized-volume".to_string();
        open_ordinary_fixture(&chain, &tc, &seller, &buyer, TEST_PRICE).await;

        let impossible = (u128::from(u64::MAX) + 1) * TICK_SIZE;
        let mut state = chain.load_state().unwrap();
        let cell = state.streams.get_mut(&tc).unwrap();
        cell.tokens_final = impossible;
        cell.tokens_pending = impossible;
        let subscription = state.deal_subscriptions.get_mut(&tc).unwrap();
        subscription.funded_tokens = impossible;
        subscription.tokens_per_week = impossible;
        subscription.tokens_paid = impossible;
        chain.store_state(&state).unwrap();

        let before = std::fs::read(&chain.state_path).unwrap();
        let error = chain.stop(&tc, &buyer).await.unwrap_err();
        assert!(error.to_string().contains("fundedTokens"), "{error}");
        assert_eq!(
            std::fs::read(&chain.state_path).unwrap(),
            before,
            "an impossible raw volume must fail during load without rewriting the sidecar"
        );
    }

    #[tokio::test]
    async fn corrupt_persisted_actor_bindings_fail_closed_without_mutation() {
        let base_dir = tempfile::tempdir().expect("test temp dir");
        let base = base_dir.path();
        let endpoints = base.join("eps.json");
        let chain = MockChainBackend::new(
            endpoints.clone(),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[59u8; 32]);
        let buyer = LocalNote::from_seed(&[60u8; 32]);
        let stranger = LocalNote::from_seed(&[61u8; 32]);
        let tc = "tc-corrupt-persisted-bindings".to_string();
        open_ordinary_fixture(&chain, &tc, &seller, &buyer, TEST_PRICE).await;
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&chain.state_path).unwrap()).unwrap();

        let mut corrupted = original.clone();
        corrupted["streams"][&tc]["buyer_pubkey"] =
            serde_json::to_value(stranger.pubkey()).unwrap();
        assert_corrupt_restart_rejected_without_mutation(
            &endpoints,
            &tc,
            corrupted,
            "stream buyer does not match persisted Match buyer",
        )
        .await;

        let mut corrupted = original.clone();
        corrupted["offer_sellers"]
            .as_object_mut()
            .unwrap()
            .remove(&tc);
        assert_corrupt_restart_rejected_without_mutation(
            &endpoints,
            &tc,
            corrupted,
            "funded deal has no persisted seller actor",
        )
        .await;
    }
}
