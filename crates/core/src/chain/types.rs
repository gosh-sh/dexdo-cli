//! `chain` data types -- offers/match, deal/stream snapshots, accounting tallies, errors(PR4 move-only).
use crate::note::NotePubkey;
use crate::params::Shell;
use serde::{Deserialize, Serialize};

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
    pub token_contract: TokenContract,
}

/// Book discovery item: offer + **seller identifier**(note) -- for
/// ranking and the blacklist(B16). In the mock `seller_id` = hex of the seller's note ed-pubkey; on the
/// real chain -- the seller from the `InferenceOrderBook` order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferListing {
    pub seller_id: String,
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
    pub owner_note: String,
    pub token_contract: Option<TokenContract>,
    pub is_buy: bool,
    pub price_per_tick: u128,
    pub ticks: u128,
    pub escrow: u128,
    pub deadline: u64,
    pub flags: u8,
    pub timestamp: u64,
}

impl OrderBookOrder {
    pub fn is_resting_ask(&self) -> bool {
        !self.is_buy && self.token_contract.is_some() && self.ticks > 0
    }
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
}

/// Parsed `InferenceOrderBook.getSubscription(orderId)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookSubscription {
    pub order_id: u128,
    pub exists: bool,
    pub period_start: u64,
    pub cur_cycle: u8,
    pub cycle_budget: u128,
    pub cycle_spent: u128,
    pub auto_renew: bool,
}

impl OrderBookSubscription {
    pub fn cycle_remaining(&self) -> u128 {
        self.cycle_budget.saturating_sub(self.cycle_spent)
    }
}

/// A single maker order consumed by an executable quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteFill {
    pub order_id: u128,
    pub token_contract: TokenContract,
    pub ticks: u128,
    pub price_per_tick: u128,
    pub cost_with_fee: u128,
}

/// Buyer-visible fill details returned after a model-only buy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchedFill {
    pub token_contract: TokenContract,
    pub ticks: u128,
    pub price_per_tick: u128,
}

/// Accepted `InferenceSubscriptionPlaced` fact from the model order book.
/// The order id is the durable correlation key for later owner-facing fills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceSubscriptionPlacement {
    pub order_id: u128,
    pub buyer_note: String,
    pub max_price_per_tick: u128,
    pub ticks: u128,
    pub cycle_budget: u128,
    pub auto_renew: bool,
    pub created_at: i64,
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
    pub token_contract: TokenContract,
    pub buyer_pubkey: NotePubkey,
    pub price_per_tick: Shell,
}

/// Durable source cursor for a seller gateway match watcher.
/// The concrete source may be note ext-out events(real shellnet) or an equivalent
/// authoritative state source(mock / direct TC state). The cursor is intentionally
/// small and secret-free so the CLI can persist it next to local deal handles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchWatchCursor {
    /// Ignore source events older than this wall-clock timestamp.
    pub since_unix: i64,
    /// Highest `created_at` timestamp already consumed from the source.
    pub last_seen_created_at: Option<i64>,
    /// TokenContracts consumed at `last_seen_created_at`, for same-second events.
    #[serde(default)]
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
    #[error("no match for token_contract {0}")]
    NoMatch(TokenContract),
    #[error("no stream open for {0}")]
    NoStream(TokenContract),
    #[error("endpoints file: {0}")]
    EndpointsFile(String),
    /// Error from the real on-chain adapter: submit/getter/shellnet provisioning.
    #[error("shellnet: {0}")]
    Chain(String),
    /// The RPC/HTTP transport failed before a by-fact chain result was available.
    #[error("shellnet transport: {0}")]
    Transport(String),
    /// The chain returned a contract-level refusal/revert.
    #[error("shellnet contract: {0}")]
    Contract(String),
    /// The order book returned the seller placement value because this TC already has a resting SELL.
    #[error("{0}")]
    DuplicateSell(String),
    /// A non-idempotent money POST may have reached the chain, but its result is not yet provable.
    #[error("shellnet ambiguous submit: {0}")]
    AmbiguousSubmit(String),
    /// A non-idempotent money write failed before its POST was attempted.
    #[error("shellnet money submit was not posted: {0}")]
    MoneySubmitPreparation(String),
    /// A non-idempotent money POST returned a decoded protocol/contract rejection.
    #[error("shellnet money submit was rejected: {0}")]
    MoneySubmitRejected(String),
    /// Note locked by a dispute/stream -- cannot trade/withdraw (, anti-scam;
    /// analog of the contract's `ERR_STREAM_LOCKED`).
    #[error("note locked (dispute/stream): {0}")]
    Locked(String),
    /// The agreed deal limit was exceeded(e.g. the offer's `max_ticks`). The real TC bounds it
    /// by deposit; the mock holds the same invariant with a guard.
    #[error("deal limit exceeded: {0}")]
    Limit(String),
}

/// Snapshot of the stream's state in the mock(for e2e acceptance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSnapshot {
    /// Locked at the seller(stake/probe commission).
    pub seller_locked: Shell,
    /// Locked at the buyer(deposited ticks) -- the TOTAL escrow still held(`prepaid + frozen + deposit`).
    pub buyer_locked: Shell,
    /// the buyer's at-risk **lead**(`prepaid + frozen`) -- the two-tick invariant bounds THIS
    /// (seller <= ~2 ticks ahead of finalized), NOT the total `buyer_locked` (which also carries the unspent
    /// `deposit` for the remaining ticks of a multi-tick deal). On the mock path the lock IS the lead.
    pub buyer_lead: Shell,
    /// Ticks sent to the seller(finalized).
    pub seller_received: Shell,
    /// Refunded to the buyer.
    pub buyer_refunded: Shell,
    /// Total SHELL burned for the contract.
    pub burned: Shell,
    /// Stream terminal/STOPped according to the TokenContract lifecycle.
    /// This is not `!opened`: funded-but-never-opened and disputed TCs can hold escrow while still active.
    pub closed: bool,
}

/// Minimal by-fact lifecycle read for a live `TokenContract`.
/// This is intentionally smaller than the raw chain getter JSON: service orchestrators need only the phase
/// booleans and timeout anchors(`fundedTime`, `lastAdvance`) to decide cleanup/reclaim/renewal actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DealChainState {
    pub funded: bool,
    pub opened: bool,
    pub disputed: bool,
    pub probe_accepted: bool,
    pub funded_time: Option<u64>,
    pub last_advance: u64,
}

impl DealChainState {
    /// Match `dexdo status` lifecycle semantics for a STOPped/settled deal.
    /// `opened=false` alone is not terminal: a matched buyer can leave the TC in
    /// funded-but-never-opened, and a dispute can also hold escrow without being
    /// a clean closed settlement.
    pub fn is_stopped(self) -> bool {
        self.funded && !self.opened && !self.disputed && self.probe_accepted
    }
}

#[cfg(test)]
mod tests {
    use super::DealChainState;

    fn state(funded: bool, opened: bool, disputed: bool, probe_accepted: bool) -> DealChainState {
        DealChainState {
            funded,
            opened,
            disputed,
            probe_accepted,
            funded_time: None,
            last_advance: 0,
        }
    }

    /// monitor CLOSED semantics must match `dexdo status`: funded-never-opened,
    /// probe, streaming, and disputed deals are active; only STOPped/settled is terminal.
    #[test]
    fn chain_state_stopped_semantics_match_status() {
        assert!(
            !state(true, false, false, false).is_stopped(),
            "funded-but-never-opened is active"
        );
        assert!(
            !state(true, true, false, false).is_stopped(),
            "opened/probe is active"
        );
        assert!(
            !state(true, true, false, true).is_stopped(),
            "streaming is active"
        );
        assert!(
            !state(true, false, true, true).is_stopped(),
            "disputed escrow is active, not cleanly closed"
        );
        assert!(
            !state(false, false, false, false).is_stopped(),
            "unfunded/readable state is not a settled close"
        );
        assert!(
            state(true, false, false, true).is_stopped(),
            "STOPped/settled matches status=stopped"
        );
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
/// counterparty, tick price and the by-fact settlement(`StreamSnapshot`).
#[derive(Debug, Clone)]
pub struct DealView {
    pub token_contract: TokenContract,
    pub role: DealRole,
    /// The counterparty's anonymous note pubkey(hex), if a match has already happened.
    pub counterparty: Option<String>,
    pub price_per_tick: Shell,
    /// The deal's served frame model id. `None` when the source cannot
    /// name it -- the mock book does not track a per-deal model, so it resolves on the real-chain reader
    /// (the `TokenContract`'s `RootModel` -> model name). The breakdown buckets `None` as `(unknown)`.
    pub model: Option<String>,
    /// The by-fact settlement(ticks/tokens/burn/closed), if the stream is open.
    pub snapshot: Option<StreamSnapshot>,
}

/// Snapshot of the note's state for observability: own orders in the book,
/// deals(role + anonymous counterparty + by-fact), total exposure(at risk). "From whom"
/// = the note's anonymous pubkey. Read only -- the monitor moves nothing.
#[derive(Debug, Clone)]
pub struct NoteSnapshot {
    /// The note's own anonymous pubkey(hex).
    pub note_id: String,
    /// Own offers in the book(the seller's orders).
    pub offers: Vec<OfferListing>,
    /// Deals where the note is the seller or the buyer.
    pub deals: Vec<DealView>,
    /// At risk: the sum locked by the note in open(not closed) deals.
    pub exposure: Shell,
}

/// Aggregated snapshot of **the entire note tree** of a single identity: the monitor
/// shows the state across ALL(sub)notes under the root key, not only the root. We fold the
/// per-note snapshots(`ChainBackend::note_snapshot` for each pubkey from `NoteTree::node_pubkeys`):
/// offers and deals are concatenated(each lives on its own subnote), exposure is summed.
/// "From whom" remains the counterparty note's anonymous pubkey. Read only.
#[derive(Debug, Clone)]
pub struct TreeSnapshot {
    /// Anonymous pubkeys of all the tree's(sub)notes that were aggregated over(hex).
    pub note_ids: Vec<String>,
    /// All the tree's offers in the book(across all subnotes).
    pub offers: Vec<OfferListing>,
    /// All the tree's deals(across all subnotes), role + anonymous counterparty + by-fact.
    pub deals: Vec<DealView>,
    /// The tree's total exposure: the sum locked across all open deals of all subnotes.
    pub exposure: Shell,
}

/// Placeholder model id for a deal whose served model is unknown to the source: the mock book
/// tracks no per-deal model, so its deals bucket here until the real-chain reader resolves real names.
pub const UNKNOWN_MODEL: &str = "(unknown)";

/// One counterparty's by-fact tally inside a model bucket: the anonymous counterparty note
/// pubkey and the by-fact figures summed across that counterparty's deals for one role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterpartyTally {
    /// The counterparty's anonymous note pubkey(hex); `None` if no match happened yet.
    pub counterparty: Option<String>,
    /// Finalized ticks(tokens) settled by-fact, summed: `seller_received / price_per_tick`.
    pub tokens: u64,
    /// SHELL settled by-fact(seller: received; buyer: paid out of escrow) -- `seller_received`, summed.
    pub money: Shell,
    /// SHELL still frozen for this role(seller: `seller_locked`; buyer: `buyer_locked`), summed.
    pub locked: Shell,
    /// SHELL burned(net fee / dispute), summed.
    pub burned: Shell,
}

/// Per-model by-fact breakdown for ONE role: the note's deals grouped by served model, then by
/// anonymous counterparty, summing tokens / money / lock / burn. Pure(no network) -- the offline core of the
/// seller/buyer accounting view. The roll-up fields are the model's totals across all its counterparties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelBreakdown {
    /// The served model id, or [`UNKNOWN_MODEL`] for deals with no known model.
    pub model: String,
    pub role: DealRole,
    /// Per-counterparty tallies, in first-seen order(deterministic).
    pub counterparties: Vec<CounterpartyTally>,
    pub tokens: u64,
    pub money: Shell,
    pub locked: Shell,
    pub burned: Shell,
}

/// A by-fact accounting anomaly on a deal: a-class problem the accounting view must
/// **surface** rather than paper over(the lead's acceptance: "show the mismatch", "highlight orphaned lock").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DealAnomaly {
    /// SHELL is locked but no counterparty matched -- an **orphaned lock**: funds frozen with no deal.
    LockedNoMatch { locked: Shell },
    /// The deal is **closed**(STOP/settled) yet SHELL is still locked -- STOP should have moved it to
    /// received/refunded, not left it frozen.
    LockedAfterClose { locked: Shell },
    /// The buyer's at-risk **lead**(`prepaid + frozen`) exceeds the **two-tick invariant** ceiling (: the
    /// seller may be at most ~2 ticks ahead of finalized) -- `buyer_lead > 2 x _unit(price_per_tick)`, where the
    /// per-tick unit **includes the book fee** (`_unit(p) = p + pxFEE_BPS/10000`,).: this bounds the
    /// LEAD, not the total `buyer_locked` (which carries the unspent `deposit` for a multi-tick deal's remaining
    /// ticks) -- comparing the total false-flagged every legitimate `maxTicks > 2` deal.
    BuyerLockExceedsTwoTicks { buyer_lead: Shell, ceiling: Shell },
}
