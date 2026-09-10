//! Crash-safe seller delivery capacity for one funded TokenContract.

//! The chain is authoritative for the funded total, the current no-rollover subscription cap and the
//! cumulative `tokensPending` high-water. The gateway persists only the delivery debt that the chain cannot
//! know yet: exact authoritative tokens forwarded after that high-water and request capacity reserved before
//! contacting an upstream. Prompts, responses, keys and provider credentials never enter this file.

use anyhow::{anyhow, bail, Result};
use dexdo_core::{
    order_flags as flags, params::SELLER_UNCLAIMED_BILLABLE_RISK_LIMIT, DealChainState,
    DealSubscription, TokenContract, PROBE_SEED_TOKENS, TICK_SIZE,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

const CAPACITY_RECORD_VERSION: u32 = 1;
pub(super) const POISONED_LOCK_MESSAGE: &str = "seller runtime lock poisoned";
const CAPACITY_ENTRIES_LOCK: &str = "seller capacity entries";
const CAPACITY_ENTRY_STATE_LOCK: &str = "seller capacity entry state";
const CAPACITY_REQUEST_LOCK: &str = "seller capacity reservation request";

pub(super) fn lock_or_recover<'a, T>(
    lock: &'a Mutex<T>,
    lock_name: &'static str,
) -> MutexGuard<'a, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!("{POISONED_LOCK_MESSAGE}: {lock_name}");
            poisoned.into_inner()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableCapacityRecord {
    version: u32,
    #[serde(with = "dexdo_core::address::serde_self_dapp")]
    token_contract: TokenContract,
    #[serde(with = "decimal_u128")]
    funded_tokens: u128,
    #[serde(with = "decimal_u128")]
    authoritative_cap: u128,
    #[serde(with = "decimal_u128")]
    tokens_pending_anchor: u128,
    #[serde(with = "decimal_u128")]
    local_delivered_after_anchor: u128,
    #[serde(with = "decimal_u128")]
    outstanding_reservation: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacitySnapshot {
    pub funded_tokens: u128,
    pub authoritative_cap: u128,
    pub tokens_pending_anchor: u128,
    pub local_delivered_after_anchor: u128,
    pub outstanding_reservation: u128,
}

impl From<&DurableCapacityRecord> for CapacitySnapshot {
    fn from(record: &DurableCapacityRecord) -> Self {
        Self {
            funded_tokens: record.funded_tokens,
            authoritative_cap: record.authoritative_cap,
            tokens_pending_anchor: record.tokens_pending_anchor,
            local_delivered_after_anchor: record.local_delivered_after_anchor,
            outstanding_reservation: record.outstanding_reservation,
        }
    }
}

impl CapacitySnapshot {
    pub fn committed(self) -> Result<u128> {
        self.tokens_pending_anchor
            .checked_add(self.local_delivered_after_anchor)
            .and_then(|value| value.checked_add(self.outstanding_reservation))
            .ok_or_else(|| anyhow!("seller capacity committed-token sum overflows uint128"))
    }

    pub fn available(self) -> Result<u128> {
        self.authoritative_cap
            .checked_sub(self.committed()?)
            .ok_or_else(|| anyhow!("seller capacity invariant is already exceeded"))
    }
}

/// How many delivered tokens may sit in memory before the durable record is rewritten.

/// This compatibility path remains for callers that account incrementally. The gateway path
/// instead commits one complete terminal bill through `finish_with_delivery`, without using this
/// coalesced split.
const CAPACITY_PERSIST_TOKEN_INTERVAL: u128 = 1_000;

struct CapacityEntryState {
    record: DurableCapacityRecord,
    terminal: bool,
    /// Delivered tokens applied to `record` in memory but not yet written to disk by the compatibility
    /// incremental-accounting path.
    unpersisted_delivered: u128,
}

struct CapacityEntry {
    path: Option<PathBuf>,
    state: Mutex<CapacityEntryState>,
}

impl CapacityEntry {
    /// Apply any coalesced delivered tokens to the record and make it durable for the compatibility
    /// incremental-accounting path.
    fn flush_delivered(&self, locked: &mut CapacityEntryState) -> Result<u64> {
        let pending = locked.unpersisted_delivered;
        if pending == 0 {
            return Ok(0);
        }
        let mut candidate = locked.record.clone();
        candidate.outstanding_reservation = candidate
            .outstanding_reservation
            .checked_sub(pending)
            .ok_or_else(|| anyhow!("aggregate reservation underflow"))?;
        candidate.local_delivered_after_anchor = candidate
            .local_delivered_after_anchor
            .checked_add(pending)
            .ok_or_else(|| anyhow!("local delivered counter overflows uint128"))?;
        validate_record(&candidate)?;
        persist_candidate(self.path.as_deref(), &candidate)?;
        locked.record = candidate;
        locked.unpersisted_delivered = 0;
        u64::try_from(pending).map_err(|_| anyhow!("durable delivered delta does not fit u64"))
    }
}

/// One capacity ledger per running gateway. Entries remain independently locked, so concurrent requests for
/// different TCs do not serialize and concurrent requests for one TC cannot reserve the same token.
pub struct CapacityManager {
    store_dir: Option<PathBuf>,
    entries: Mutex<HashMap<TokenContract, Arc<CapacityEntry>>>,
}

impl CapacityManager {
    pub fn in_memory() -> Self {
        Self {
            store_dir: None,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn in_deals_dir(deals_dir: PathBuf) -> Self {
        Self {
            store_dir: Some(deals_dir.join("seller-capacity")),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Register or refresh one funded deal from a strict paired
    /// `getState()` / `getSubscription()` read.

    /// `tokensPending` is the only chain anchor: promotion lag must never make an already delivered token
    /// available again. A later anchor advance consumes exactly the corresponding prefix of durable local
    /// delivery. Outstanding/ambiguous reservations are never guessed away.
    pub fn reconcile_deal(
        &self,
        token_contract: &TokenContract,
        state: DealChainState,
        deal: DealSubscription,
    ) -> Result<Option<CapacitySnapshot>> {
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        if state.is_stopped() {
            self.mark_terminal(token_contract)?;
            return Ok(None);
        }
        validate_live_deal_shape(token_contract, state, deal)?;
        let mut cap = authoritative_cap(state, deal)?;
        let subscription_term_ended = deal.is_subscription() && deal.week_index >= deal.sub_weeks;

        let entry = {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRIES_LOCK}"))?;
            if let Some(entry) = entries.get(token_contract) {
                entry.clone()
            } else {
                let path = self
                    .store_dir
                    .as_ref()
                    .map(|directory| capacity_path(directory, token_contract));
                let record = match path.as_deref().map(load_record).transpose()? {
                    Some(Some(record)) => {
                        validate_record(&record)?;
                        if record.token_contract != *token_contract {
                            bail!(
                                "seller capacity file is for TokenContract {}, not {}",
                                dexdo_core::address::display_self_dapp(&record.token_contract),
                                token_contract_display
                            );
                        }
                        record
                    }
                    Some(None) | None => DurableCapacityRecord {
                        version: CAPACITY_RECORD_VERSION,
                        token_contract: token_contract.clone(),
                        funded_tokens: deal.funded_tokens,
                        authoritative_cap: cap,
                        tokens_pending_anchor: state.tokens_pending,
                        local_delivered_after_anchor: 0,
                        outstanding_reservation: 0,
                    },
                };
                let entry = Arc::new(CapacityEntry {
                    path,
                    state: Mutex::new(CapacityEntryState {
                        record,
                        terminal: false,
                        unpersisted_delivered: 0,
                    }),
                });
                entries.insert(token_contract.clone(), entry.clone());
                entry
            }
        };

        let mut locked = entry
            .state
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"))?;
        if locked.terminal {
            bail!("TokenContract {token_contract_display} capacity is terminal");
        }
        let old = &locked.record;
        if old.funded_tokens != deal.funded_tokens {
            bail!(
                "TokenContract {token_contract_display} fundedTokens changed from {} to {}",
                old.funded_tokens,
                deal.funded_tokens
            );
        }
        if state.tokens_pending < old.tokens_pending_anchor {
            bail!(
                "TokenContract {token_contract_display} tokensPending regressed from {} to {}",
                old.tokens_pending_anchor,
                state.tokens_pending
            );
        }
        if cap < old.authoritative_cap && !subscription_term_ended {
            bail!(
                "TokenContract {token_contract_display} authoritative capacity regressed from {} to {}",
                old.authoritative_cap,
                cap
            );
        }
        let acknowledged = state.tokens_pending - old.tokens_pending_anchor;
        // An accepted probe is one tick credited AND paid, per the single statement of that rule on
        // `PROBE_SEED_TOKENS`: the buyer bought the trial tick whatever the model actually produced for it,
        // and the claim driver already starts its cumulative high-water from it. So the seed is a
        // protocol-owned credit, not an unbacked delivery advance. Exclude exactly that seed from the
        // delivery-backed comparison on the single observation that crosses into `probeAccepted`; every
        // token beyond it keeps the strict comparison below. `validate_live_deal_shape` pins a pre-probe
        // anchor to exactly zero and an accepted-probe anchor to at least one tick, and the durable anchor
        // never regresses, so the crossing is credited at most once per deal -- re-observing the same state,
        // and restarting from a record already carrying the crossed anchor, both leave the anchor at or
        // above the seed.
        let probe_seed = if state.probe_accepted && old.tokens_pending_anchor == 0 {
            PROBE_SEED_TOKENS.min(acknowledged)
        } else {
            0
        };
        let backed_by_delivery = acknowledged - probe_seed;
        if backed_by_delivery > old.local_delivered_after_anchor && !subscription_term_ended {
            bail!(
                "TokenContract {token_contract_display} tokensPending advanced by {acknowledged} \
                 ({backed_by_delivery} beyond the protocol probe seed), beyond durable local delivery {}",
                old.local_delivered_after_anchor
            );
        }
        let local_delivered_after_anchor = old
            .local_delivered_after_anchor
            .saturating_sub(acknowledged);
        if subscription_term_ended {
            // The final claim grace keeps the deal open but must not expose a fifth weekly quota. Keep
            // every amount already durably committed before the boundary, then make new availability zero.
            cap = state
                .tokens_pending
                .checked_add(local_delivered_after_anchor)
                .and_then(|value| value.checked_add(old.outstanding_reservation))
                .ok_or_else(|| anyhow!("post-term seller capacity commitment overflows uint128"))?;
        }
        let candidate = DurableCapacityRecord {
            version: CAPACITY_RECORD_VERSION,
            token_contract: token_contract.clone(),
            funded_tokens: deal.funded_tokens,
            authoritative_cap: cap,
            tokens_pending_anchor: state.tokens_pending,
            local_delivered_after_anchor,
            outstanding_reservation: old.outstanding_reservation,
        };
        validate_record(&candidate)?;
        if candidate != locked.record {
            persist_candidate(entry.path.as_deref(), &candidate)?;
            locked.record = candidate;
        }
        Ok(Some(CapacitySnapshot::from(&locked.record)))
    }

    pub fn reserve(
        &self,
        token_contract: &TokenContract,
        requested: u64,
    ) -> std::result::Result<CapacityReservation, ReserveError> {
        if requested == 0 {
            return Err(ReserveError::Exhausted);
        }
        let entry = self
            .entries
            .lock()
            .map_err(|_| {
                ReserveError::InvalidState(anyhow!(
                    "{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRIES_LOCK}"
                ))
            })?
            .get(token_contract)
            .cloned()
            .ok_or(ReserveError::UnknownDeal)?;
        let mut locked = entry.state.lock().map_err(|_| {
            ReserveError::InvalidState(anyhow!(
                "{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"
            ))
        })?;
        if locked.terminal {
            return Err(ReserveError::Terminal);
        }
        validate_record(&locked.record).map_err(ReserveError::InvalidState)?;
        if locked.record.local_delivered_after_anchor >= SELLER_UNCLAIMED_BILLABLE_RISK_LIMIT {
            return Err(ReserveError::Exhausted);
        }
        let available = CapacitySnapshot::from(&locked.record)
            .available()
            .map_err(ReserveError::InvalidState)?;
        let amount = available.min(u128::from(requested));
        if amount == 0 {
            return Err(ReserveError::Exhausted);
        }
        let mut candidate = locked.record.clone();
        candidate.outstanding_reservation = candidate
            .outstanding_reservation
            .checked_add(amount)
            .ok_or_else(|| {
            ReserveError::InvalidState(anyhow!("request reservation overflows uint128"))
        })?;
        validate_record(&candidate).map_err(ReserveError::InvalidState)?;
        persist_candidate(entry.path.as_deref(), &candidate).map_err(ReserveError::InvalidState)?;
        locked.unpersisted_delivered = 0;
        locked.record = candidate;
        drop(locked);

        Ok(CapacityReservation {
            entry,
            request: Mutex::new(RequestReservation {
                initial: amount,
                remaining: amount,
                finished: false,
            }),
        })
    }

    pub fn snapshot(&self, token_contract: &TokenContract) -> Result<Option<CapacitySnapshot>> {
        let Some(entry) = self
            .entries
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRIES_LOCK}"))?
            .get(token_contract)
            .cloned()
        else {
            return Ok(None);
        };
        let locked = entry
            .state
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"))?;
        if locked.terminal {
            return Ok(None);
        }
        validate_record(&locked.record)?;
        Ok(Some(CapacitySnapshot::from(&locked.record)))
    }

    pub fn mark_terminal(&self, token_contract: &TokenContract) -> Result<()> {
        let entry = self
            .entries
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRIES_LOCK}"))?
            .remove(token_contract);
        if let Some(entry) = entry {
            let mut locked = entry
                .state
                .lock()
                .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"))?;
            locked.terminal = true;
            if let Some(path) = &entry.path {
                remove_if_present(path)?;
            }
        } else if let Some(directory) = &self.store_dir {
            remove_if_present(&capacity_path(directory, token_contract))?;
        }
        Ok(())
    }
}

impl Default for CapacityManager {
    fn default() -> Self {
        Self::in_memory()
    }
}

#[derive(Debug)]
pub enum ReserveError {
    UnknownDeal,
    Terminal,
    Exhausted,
    InvalidState(anyhow::Error),
}

impl fmt::Display for ReserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDeal => write!(formatter, "deal capacity is not registered"),
            Self::Terminal => write!(formatter, "deal capacity is terminal"),
            Self::Exhausted => write!(formatter, "deal capacity is exhausted"),
            Self::InvalidState(error) => write!(formatter, "invalid deal capacity: {error}"),
        }
    }
}

impl std::error::Error for ReserveError {}

struct RequestReservation {
    initial: u128,
    remaining: u128,
    finished: bool,
}

/// Request-scoped reservation. Dropping it deliberately does not release anything: a task/process crash is
/// ambiguous, so restart must retain the durable upper bound until exact chain evidence acknowledges it.
pub struct CapacityReservation {
    entry: Arc<CapacityEntry>,
    request: Mutex<RequestReservation>,
}

impl CapacityReservation {
    pub fn amount(&self) -> u64 {
        let initial = lock_or_recover(&self.request, CAPACITY_REQUEST_LOCK).initial;
        u64::try_from(initial).expect("reservation is bounded by requested u64")
    }

    pub fn remaining(&self) -> u64 {
        let remaining = lock_or_recover(&self.request, CAPACITY_REQUEST_LOCK).remaining;
        u64::try_from(remaining).expect("reservation is bounded by requested u64")
    }

    /// Atomically turn this request's whole durable billing reservation into one validated terminal
    /// provider-usage total and release the unused remainder. No partial delivery split is persisted.
    pub fn finish_with_delivery(&self, tokens: u64) -> Result<u64> {
        if tokens == 0 {
            bail!("terminal billable token total must be positive");
        }
        let mut request = self
            .request
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_REQUEST_LOCK}"))?;
        if request.finished {
            bail!("capacity reservation already finished");
        }
        let tokens = u128::from(tokens);
        if tokens > request.remaining {
            bail!(
                "terminal billable total {tokens} exceeds request reservation {}",
                request.remaining
            );
        }
        let mut locked = self
            .entry
            .state
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"))?;
        if locked.terminal {
            bail!("deal capacity became terminal");
        }
        if locked.unpersisted_delivered != 0 {
            bail!("terminal billing cannot follow partial per-chunk delivery accounting");
        }
        let mut candidate = locked.record.clone();
        candidate.outstanding_reservation = candidate
            .outstanding_reservation
            .checked_sub(request.remaining)
            .ok_or_else(|| anyhow!("aggregate reservation underflow"))?;
        candidate.local_delivered_after_anchor = candidate
            .local_delivered_after_anchor
            .checked_add(tokens)
            .ok_or_else(|| anyhow!("local billable counter overflows uint128"))?;
        validate_record(&candidate)?;
        persist_candidate(self.entry.path.as_deref(), &candidate)?;
        locked.record = candidate;
        request.remaining = 0;
        request.finished = true;
        u64::try_from(tokens).map_err(|_| anyhow!("terminal billable total does not fit u64"))
    }

    /// Compatibility gate for callers that still expose output before authoritative usage arrives.

    /// The gateway path does not call this method: it validates one typed terminal usage record
    /// and commits it atomically through [`Self::finish_with_delivery`]. This method remains for API
    /// compatibility with incremental-accounting consumers.

    /// `min_billable` is what this upstream has already charged for one run of unaccounted output on
    /// this stream, and zero before it has charged anything. Asking only whether the reservation is
    /// non-empty refuses at exactly zero and nowhere else, so a reservation that lands short of the
    /// next run rather than on top of it still exposes one run it cannot bill: the seller has to be
    /// able to pay what this upstream has already shown a run costs, not merely one token.
    pub fn authorize_exposure(&self, min_billable: u64) -> std::result::Result<(), ReserveError> {
        let request = self.request.lock().map_err(|_| {
            ReserveError::InvalidState(anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_REQUEST_LOCK}"))
        })?;
        if request.finished || request.remaining < u128::from(min_billable.max(1)) {
            return Err(ReserveError::Exhausted);
        }
        Ok(())
    }

    /// Compatibility API: record an incremental billable delta and return how much became durable.

    /// The gateway path commits only a complete provider-native terminal bill through
    /// [`Self::finish_with_delivery`]; this per-delta path remains for API compatibility.
    pub fn record_delivered(&self, tokens: u64) -> Result<u64> {
        if tokens == 0 {
            bail!("authoritative delivered delta must be positive");
        }
        let mut request = self
            .request
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_REQUEST_LOCK}"))?;
        if request.finished {
            bail!("capacity reservation already finished");
        }
        let tokens = u128::from(tokens);
        if tokens > request.remaining {
            bail!(
                "authoritative delivered delta {tokens} exceeds request reservation {}",
                request.remaining
            );
        }
        let mut locked = self
            .entry
            .state
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"))?;
        if locked.terminal {
            bail!("deal capacity became terminal");
        }
        // The tokens are held as an unapplied split: while they wait they stay counted as outstanding
        // reservation, so `committed` and the funded ceiling read exactly as they would if each token
        // had been written through. Only the flush moves them, and only the flush may be claimed.
        let unpersisted = locked
            .unpersisted_delivered
            .checked_add(tokens)
            .ok_or_else(|| anyhow!("local delivered counter overflows uint128"))?;
        if unpersisted > locked.record.outstanding_reservation {
            bail!("aggregate reservation underflow");
        }
        locked.unpersisted_delivered = unpersisted;
        // Coalescing exists only to avoid a disk write, so a ledger with no store never delays: it
        // applies the split immediately and keeps the write-through semantics exactly.
        let durable = if unpersisted >= CAPACITY_PERSIST_TOKEN_INTERVAL || self.entry.path.is_none()
        {
            self.entry.flush_delivered(&mut locked)?
        } else {
            0
        };
        request.remaining -= tokens;
        Ok(durable)
    }

    /// Release a request's exact unused remainder. The gateway uses this only when a request
    /// failed before any billable total could be accepted.
    /// Returns the delivered tokens that became durable in this call (see [`Self::record_delivered`]).
    pub fn finish_exact(&self) -> Result<u64> {
        let mut request = self
            .request
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_REQUEST_LOCK}"))?;
        if request.finished {
            bail!("capacity reservation already finished");
        }
        let mut locked = self
            .entry
            .state
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"))?;
        if locked.terminal {
            request.finished = true;
            request.remaining = 0;
            return Ok(0);
        }
        // A request terminal owes the coalesced split a write, and it must land before the remainder
        // is released so that a crash between the two cannot drop delivered tokens back into a
        // reservation that has already been given away.
        let durable = self.entry.flush_delivered(&mut locked)?;
        let mut candidate = locked.record.clone();
        candidate.outstanding_reservation = candidate
            .outstanding_reservation
            .checked_sub(request.remaining)
            .ok_or_else(|| anyhow!("aggregate reservation underflow"))?;
        validate_record(&candidate)?;
        persist_candidate(self.entry.path.as_deref(), &candidate)?;
        locked.record = candidate;
        request.remaining = 0;
        request.finished = true;
        Ok(durable)
    }

    /// Preserve all unresolved capacity. This is the only safe terminal when some output may have reached the
    /// buyer without a valid authoritative usage count.
    /// Returns the delivered tokens that became durable in this call (see [`Self::record_delivered`]).
    pub fn finish_ambiguous(&self) -> Result<u64> {
        let mut request = self
            .request
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_REQUEST_LOCK}"))?;
        if request.finished {
            bail!("capacity reservation already finished");
        }
        // The unresolved remainder is deliberately kept committed, but any coalesced delivery split
        // still owes a write: this is a request terminal, so nothing may stay only in memory.
        let mut locked = self
            .entry
            .state
            .lock()
            .map_err(|_| anyhow!("{POISONED_LOCK_MESSAGE}: {CAPACITY_ENTRY_STATE_LOCK}"))?;
        let durable = if locked.terminal {
            0
        } else {
            self.entry.flush_delivered(&mut locked)?
        };
        drop(locked);
        request.remaining = 0;
        request.finished = true;
        Ok(durable)
    }
}

fn authoritative_cap(state: DealChainState, deal: DealSubscription) -> Result<u128> {
    if deal.is_subscription() && deal.week_index >= deal.sub_weeks {
        // Defense in depth for: an open final-claim grace is not a fifth subscription week.
        return Ok(state.tokens_pending);
    }
    if !state.probe_accepted {
        return Ok(TICK_SIZE.min(deal.funded_tokens));
    }
    if !deal.is_subscription() {
        return Ok(deal.funded_tokens);
    }
    deal.week_base_tokens
        .checked_add(deal.tokens_per_week)
        .map(|cap| cap.min(deal.funded_tokens))
        .ok_or_else(|| anyhow!("subscription weekBaseTokens + tokensPerWeek overflows uint128"))
}

fn validate_live_deal_shape(
    token_contract: &TokenContract,
    state: DealChainState,
    deal: DealSubscription,
) -> Result<()> {
    let token_contract = dexdo_core::address::display_self_dapp(token_contract);
    if deal.is_subscription() != (deal.deal_flags & flags::SUBSCRIPTION != 0) {
        bail!("TokenContract {token_contract} has contradictory subscription flag/subWeeks shape");
    }
    if !deal.is_subscription()
        && (deal.week_index != 0
            || deal.tokens_per_week != deal.funded_tokens
            || deal.week_base_tokens != 0)
    {
        bail!("TokenContract {token_contract} has contradictory ordinary deal capacity shape");
    }
    if !state.funded {
        bail!("TokenContract {token_contract} is not funded");
    }
    if state.disputed {
        bail!("TokenContract {token_contract} is disputed; refusing to serve");
    }
    if !state.opened && state.probe_accepted {
        bail!(
            "TokenContract {token_contract} reports accepted probe while not open and not terminal"
        );
    }
    if deal.funded_tokens == 0 {
        bail!("TokenContract {token_contract} fundedTokens is zero");
    }
    if state.tokens_pending > deal.funded_tokens {
        bail!(
            "TokenContract {token_contract} tokensPending {} exceeds fundedTokens {}",
            state.tokens_pending,
            deal.funded_tokens
        );
    }
    if state.probe_accepted {
        if state.tokens_pending < TICK_SIZE {
            bail!(
                "TokenContract {token_contract} accepted probe has tokensPending {} below TICK_SIZE {}",
                state.tokens_pending,
                TICK_SIZE
            );
        }
    } else if state.tokens_pending != 0 {
        bail!(
            "TokenContract {token_contract} pre-probe tokensPending must be zero, got {}",
            state.tokens_pending
        );
    }
    let cap = authoritative_cap(state, deal)?;
    if state.tokens_pending > cap {
        bail!(
            "TokenContract {token_contract} tokensPending {} exceeds current authoritative cap {}",
            state.tokens_pending,
            cap
        );
    }
    Ok(())
}

fn validate_record(record: &DurableCapacityRecord) -> Result<()> {
    if record.version != CAPACITY_RECORD_VERSION {
        bail!(
            "seller capacity {} has version {}; expected {}",
            dexdo_core::address::display_self_dapp(&record.token_contract),
            record.version,
            CAPACITY_RECORD_VERSION
        );
    }
    if record.token_contract.trim().is_empty() {
        bail!("seller capacity record has empty token_contract");
    }
    let snapshot = CapacitySnapshot::from(record);
    let committed = snapshot.committed()?;
    if committed > snapshot.authoritative_cap {
        bail!(
            "seller capacity invariant violated: pending {} + local {} + reserved {} = {} > cap {}",
            snapshot.tokens_pending_anchor,
            snapshot.local_delivered_after_anchor,
            snapshot.outstanding_reservation,
            committed,
            snapshot.authoritative_cap
        );
    }
    if snapshot.authoritative_cap > snapshot.funded_tokens {
        bail!(
            "seller capacity invariant violated: cap {} > fundedTokens {}",
            snapshot.authoritative_cap,
            snapshot.funded_tokens
        );
    }
    Ok(())
}

fn capacity_path(directory: &Path, token_contract: &str) -> PathBuf {
    let safe = token_contract
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    directory.join(format!("deal-{safe}-seller-capacity.json"))
}

fn load_record(path: &Path) -> Result<Option<DurableCapacityRecord>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!("read seller capacity {}: {error}", path.display())),
    };
    if bytes.is_empty() {
        bail!("seller capacity {} is empty", path.display());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| anyhow!("parse seller capacity {}: {error}", path.display()))
}

fn persist_candidate(path: Option<&Path>, record: &DurableCapacityRecord) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("seller capacity path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| anyhow!("create seller capacity dir {}: {error}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(record)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("seller-capacity.json");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| anyhow!("system clock before epoch: {error}"))?
        .as_nanos();
    let temporary = parent.join(format!(".{name}.tmp.{}.{nanos}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, READ_CONTROL, WRITE_DAC};
        options.access_mode(GENERIC_WRITE | READ_CONTROL | WRITE_DAC);
        options.share_mode(FILE_SHARE_DELETE);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        anyhow!(
            "create seller capacity temp {}: {error}",
            temporary.display()
        )
    })?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(anyhow!(
            "write seller capacity temp {}: {error}",
            temporary.display()
        ));
    }
    if let Err(error) = atomic_replace(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(anyhow!(
            "commit seller capacity {} from {}: {error}",
            path.display(),
            temporary.display()
        ));
    }
    sync_parent(parent)?;
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both paths are NUL-terminated and remain alive for the duration of the call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| anyhow!("sync seller capacity dir {}: {error}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<()> {
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_parent(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow!(
            "remove terminal seller capacity {}: {error}",
            path.display()
        )),
    }
}

mod decimal_u128 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse::<u128>().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[path = "capacity_1157_tests.rs"]
mod issue_1157_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::SUBSCRIPTION_WEEKS;
    use proptest::prelude::*;
    use std::sync::Barrier;

    const WEEK_QUOTA: u128 = 2 * TICK_SIZE;
    const FUNDED: u128 = (SUBSCRIPTION_WEEKS as u128) * WEEK_QUOTA;
    const ORDINARY_FUNDED: u128 = 10 * TICK_SIZE;
    /// What the model really produced for the trial request in the live run: far below one tick.
    const PROBE_OUTPUT: u64 = 29_354;

    fn state(probe_accepted: bool, pending: u128) -> DealChainState {
        DealChainState {
            funded: true,
            opened: true,
            probe_accepted,
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

    fn subscription(week_index: u8, week_base_tokens: u128) -> DealSubscription {
        DealSubscription {
            deal_flags: flags::SUBSCRIPTION,
            sub_weeks: SUBSCRIPTION_WEEKS,
            week_index,
            tokens_per_week: WEEK_QUOTA,
            funded_tokens: FUNDED,
            tokens_paid: u128::from(week_index) * WEEK_QUOTA,
            period_start: 1,
            week_base_tokens,
        }
    }

    fn ordinary(funded_tokens: u128) -> DealSubscription {
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

    fn assert_invariant(snapshot: CapacitySnapshot) {
        assert!(snapshot.committed().unwrap() <= snapshot.authoritative_cap);
        assert!(snapshot.authoritative_cap <= snapshot.funded_tokens);
    }

    /// Delivery must not rewrite the durable record once per token.

    /// The pre-fix path fsynced the capacity file twice -- plus a file create and a rename -- for every
    /// delivered token, which floored the gateway at ~13k tokens/min on a real disk no matter how fast
    /// the model produced. Coalescing is only sound because `record_delivered` reclassifies reserved
    /// tokens as delivered without moving `committed`, and `reserve` already made the whole request
    /// durable; this pins all three halves of that argument: the write lags during the request, the
    /// funded ceiling never moves, and the terminal is exact.
    #[test]
    fn delivery_coalesces_durable_writes_and_is_exact_at_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let manager = CapacityManager::in_deals_dir(dir.path().to_path_buf());
        let tc = "0:coalesce".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(ORDINARY_FUNDED))
            .unwrap();
        let path = capacity_path(&dir.path().join("seller-capacity"), &tc);
        let on_disk = || load_record(&path).unwrap().unwrap();

        let total = 4 * CAPACITY_PERSIST_TOKEN_INTERVAL;
        let reservation = manager.reserve(&tc, total as u64).unwrap();
        let committed_at_reserve = CapacitySnapshot::from(&on_disk()).committed().unwrap();

        // One token is held back -- a per-token durable rewrite is exactly the cost this removes.
        reservation.record_delivered(1).unwrap();
        assert_eq!(
            on_disk().local_delivered_after_anchor,
            0,
            "the first delivered token must not trigger a durable rewrite"
        );

        for _ in 1..total {
            reservation.record_delivered(1).unwrap();
            assert_eq!(
                CapacitySnapshot::from(&on_disk()).committed().unwrap(),
                committed_at_reserve,
                "committed capacity is invariant across delivery, flushed or not"
            );
        }
        assert!(
            on_disk().local_delivered_after_anchor >= total - CAPACITY_PERSIST_TOKEN_INTERVAL,
            "the durable record must track delivery to within one coalescing interval"
        );

        // The request terminal is exact: nothing is left only in memory.
        reservation.finish_exact().unwrap();
        let snapshot = manager.snapshot(&tc).unwrap().unwrap();
        let durable = on_disk();
        assert_eq!(
            durable.local_delivered_after_anchor, total,
            "every delivered token is durable once the request ends"
        );
        assert_eq!(
            durable.local_delivered_after_anchor,
            snapshot.local_delivered_after_anchor
        );
        assert_eq!(
            durable.outstanding_reservation,
            snapshot.outstanding_reservation
        );
        assert_invariant(CapacitySnapshot::from(&durable));
    }

    /// The other request terminal: an ambiguous finish must also leave nothing only in memory.
    #[test]
    fn ambiguous_terminal_flushes_the_coalesced_delivery_split() {
        let dir = tempfile::tempdir().unwrap();
        let manager = CapacityManager::in_deals_dir(dir.path().to_path_buf());
        let tc = "0:coalesce-ambiguous".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(ORDINARY_FUNDED))
            .unwrap();
        let path = capacity_path(&dir.path().join("seller-capacity"), &tc);

        let reservation = manager.reserve(&tc, 16).unwrap();
        reservation.record_delivered(4).unwrap();
        assert_eq!(
            load_record(&path)
                .unwrap()
                .unwrap()
                .local_delivered_after_anchor,
            0,
            "still coalesced below the interval"
        );

        reservation.finish_ambiguous().unwrap();
        let durable = load_record(&path).unwrap().unwrap();
        assert_eq!(
            durable.local_delivered_after_anchor, 4,
            "the ambiguous terminal flushes the delivered split"
        );
        // Ambiguous deliberately keeps the unresolved remainder committed.
        assert_eq!(durable.outstanding_reservation, 12);
        assert_invariant(CapacitySnapshot::from(&durable));
    }

    /// MEASUREMENT INSTRUMENT (not a CI gate): per-delta cost of the durable compatibility path,
    /// reported by decade. The gateway does not call `record_delivered`; this instrument is
    /// retained for callers of the public incremental-accounting API.

    /// PERF_N=200000 cargo test -p dexdo --release --lib \
    /// seller::capacity::tests::measure_delivery_persist_by_decade -- --ignored --nocapture

    /// `PERF_INMEM=1` switches the store off (`CapacityManager::in_memory`) -- the A/B that
    /// separates the disk cost from everything else.
    #[test]
    #[ignore = "measurement instrument; run explicitly with --ignored --nocapture"]
    fn measure_delivery_persist_by_decade() {
        let n: u64 = std::env::var("PERF_N")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(50_000);
        let dir = tempfile::tempdir().unwrap();
        let in_memory = std::env::var("PERF_INMEM").is_ok();
        let manager = if in_memory {
            CapacityManager::in_memory()
        } else {
            CapacityManager::in_deals_dir(dir.path().to_path_buf())
        };
        let tc = "0:perf".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(ORDINARY_FUNDED))
            .unwrap();
        let reservation = manager.reserve(&tc, n).unwrap();
        assert_eq!(u128::from(reservation.amount()), u128::from(n));

        let decade = (n / 10).max(1);
        let start = std::time::Instant::now();
        let mut mark = start;
        println!("store={} n={n}", if in_memory { "memory" } else { "disk" });
        println!("decade    tokens        ms      tokens/min");
        for index in 1..=n {
            reservation.record_delivered(1).unwrap();
            if index % decade == 0 {
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(mark).as_secs_f64();
                println!(
                    "{:>6}  {:>8}  {:>8.0}  {:>14.0}",
                    index / decade,
                    decade,
                    elapsed * 1000.0,
                    decade as f64 / elapsed * 60.0
                );
                mark = now;
            }
        }
        let total = start.elapsed().as_secs_f64();
        println!(
            "TOTAL {n} tokens in {total:.2}s = {:.0} tokens/min",
            n as f64 / total * 60.0
        );
    }

    #[test]
    fn sequential_requests_share_one_cumulative_limit() {
        let manager = CapacityManager::in_memory();
        let tc = "0:sequential".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        let first = manager.reserve(&tc, 600_000).unwrap();
        first.record_delivered(600_000).unwrap();
        first.finish_exact().unwrap();
        let second = manager.reserve(&tc, 600_000).unwrap();
        assert_eq!(second.amount(), 400_000);
        second.record_delivered(400_000).unwrap();
        second.finish_exact().unwrap();
        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));
    }

    /// E2E-ROW: E2E-UPS-40/L0
    #[test]
    fn next_request_is_gated_at_one_tick_and_reopens_after_claim_reconciliation() {
        let manager = CapacityManager::in_memory();
        let tc = "0:billable-risk-gate".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(ORDINARY_FUNDED))
            .unwrap();

        let below_limit = (SELLER_UNCLAIMED_BILLABLE_RISK_LIMIT - 1) as u64;
        let first = manager.reserve(&tc, below_limit).unwrap();
        assert_eq!(
            first.finish_with_delivery(below_limit).unwrap(),
            below_limit
        );
        assert_eq!(
            manager
                .reserve(&tc, 1)
                .unwrap()
                .finish_with_delivery(1)
                .unwrap(),
            1,
            "the already-admitted request completes across the risk boundary"
        );

        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));

        let reconciled = manager
            .reconcile_deal(
                &tc,
                state(true, TICK_SIZE + SELLER_UNCLAIMED_BILLABLE_RISK_LIMIT),
                ordinary(ORDINARY_FUNDED),
            )
            .unwrap()
            .unwrap();
        assert_eq!(reconciled.local_delivered_after_anchor, 0);
        assert_eq!(manager.reserve(&tc, 1).unwrap().amount(), 1);
    }

    proptest! {
        #[test]
        fn admission_risk_gate_matches_the_durable_one_tick_boundary(
            backlog in 0_u32..=SELLER_UNCLAIMED_BILLABLE_RISK_LIMIT as u32,
        ) {
            let manager = CapacityManager::in_memory();
            let tc = "0:billable-risk-property".to_string();
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(ORDINARY_FUNDED))
                .unwrap();
            if backlog > 0 {
                let accepted = manager.reserve(&tc, u64::from(backlog)).unwrap();
                accepted.finish_with_delivery(u64::from(backlog)).unwrap();
            }

            let admission = manager.reserve(&tc, 1);
            if u128::from(backlog) < SELLER_UNCLAIMED_BILLABLE_RISK_LIMIT {
                prop_assert!(admission.is_ok());
            } else {
                prop_assert!(matches!(
                    admission,
                    Err(ReserveError::Exhausted)
                ));
            }
        }
    }

    #[test]
    fn simultaneous_requests_cannot_reserve_the_same_last_capacity() {
        let manager = Arc::new(CapacityManager::in_memory());
        let tc = "0:race".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let manager = manager.clone();
            let tc = tc.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                manager.reserve(&tc, 750_000).unwrap()
            }));
        }
        barrier.wait();
        let reservations = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            reservations
                .iter()
                .map(CapacityReservation::amount)
                .sum::<u64>(),
            TICK_SIZE as u64
        );
        assert_invariant(manager.snapshot(&tc).unwrap().unwrap());
    }

    #[test]
    fn probe_is_one_tick_and_acceptance_never_counts_it_twice() {
        let manager = CapacityManager::in_memory();
        let tc = "0:probe".to_string();
        manager
            .reconcile_deal(&tc, state(false, 0), subscription(0, 0))
            .unwrap();
        let probe = manager.reserve(&tc, u64::MAX).unwrap();
        assert_eq!(probe.amount(), TICK_SIZE as u64);
        probe.record_delivered(TICK_SIZE as u64).unwrap();
        probe.finish_exact().unwrap();

        let after_accept = manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(after_accept.tokens_pending_anchor, TICK_SIZE);
        assert_eq!(after_accept.local_delivered_after_anchor, 0);
        assert_eq!(after_accept.authoritative_cap, WEEK_QUOTA);
        assert_eq!(after_accept.available().unwrap(), TICK_SIZE);
    }

    /// The two live deal shapes that both register a pre-probe anchor of zero and then cross into
    /// `probeAccepted`: label, deal shape, and the authoritative cap the crossing opens.
    fn probe_shapes() -> [(&'static str, DealSubscription, u128); 2] {
        [
            ("ordinary", ordinary(ORDINARY_FUNDED), ORDINARY_FUNDED),
            ("subscription", subscription(0, 0), WEEK_QUOTA),
        ]
    }

    #[test]
    fn accepted_probe_seed_is_not_an_unbacked_delivery_advance() {
        for (label, deal, cap) in probe_shapes() {
            let manager = CapacityManager::in_memory();
            let tc = format!("0:probe-seed-{label}");
            manager
                .reconcile_deal(&tc, state(false, 0), deal)
                .unwrap()
                .unwrap();
            let probe = manager.reserve(&tc, u64::MAX).unwrap();
            assert_eq!(probe.amount(), TICK_SIZE as u64, "{label}");
            probe.record_delivered(PROBE_OUTPUT).unwrap();
            probe.finish_exact().unwrap();

            let accepted = manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), deal)
                .unwrap()
                .unwrap();
            assert_eq!(accepted.tokens_pending_anchor, TICK_SIZE, "{label}");
            assert_eq!(accepted.local_delivered_after_anchor, 0, "{label}");
            assert_eq!(accepted.outstanding_reservation, 0, "{label}");
            assert_eq!(accepted.authoritative_cap, cap, "{label}");
            assert_eq!(accepted.available().unwrap(), cap - TICK_SIZE, "{label}");
            assert_invariant(accepted);

            // Serving continues past the acceptance instead of the deal dying on it.
            assert_eq!(
                manager.reserve(&tc, 4_096).unwrap().amount(),
                4_096,
                "{label}"
            );
        }
    }

    #[test]
    fn advance_beyond_delivered_output_after_the_probe_still_fails_closed() {
        for (label, deal, _cap) in probe_shapes() {
            let manager = CapacityManager::in_memory();
            let tc = format!("0:probe-seed-overclaim-{label}");
            manager
                .reconcile_deal(&tc, state(false, 0), deal)
                .unwrap()
                .unwrap();
            let probe = manager.reserve(&tc, u64::MAX).unwrap();
            probe.record_delivered(PROBE_OUTPUT).unwrap();
            probe.finish_exact().unwrap();
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), deal)
                .unwrap()
                .unwrap();

            let served = manager.reserve(&tc, 5_000).unwrap();
            served.record_delivered(5_000).unwrap();
            served.finish_exact().unwrap();

            let error = manager
                .reconcile_deal(&tc, state(true, TICK_SIZE + 5_001), deal)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("beyond durable local delivery 5000"),
                "{label}: {error:#}"
            );

            // Exactly what was delivered is still acknowledged without complaint.
            let exact = manager
                .reconcile_deal(&tc, state(true, TICK_SIZE + 5_000), deal)
                .unwrap()
                .unwrap();
            assert_eq!(exact.local_delivered_after_anchor, 0, "{label}");
            assert_invariant(exact);
        }
    }

    #[test]
    fn probe_seed_is_credited_exactly_once_across_repeats_and_restarts() {
        for (label, deal, cap) in probe_shapes() {
            // Crash between serving the trial request and observing the acceptance: the durable record
            // still carries the pre-probe anchor, so the restarted observer performs the crossing itself.
            let directory = tempfile::tempdir().unwrap();
            let tc = format!("0:probe-seed-restart-{label}");
            {
                let manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
                manager
                    .reconcile_deal(&tc, state(false, 0), deal)
                    .unwrap()
                    .unwrap();
                let probe = manager.reserve(&tc, u64::MAX).unwrap();
                probe.record_delivered(PROBE_OUTPUT).unwrap();
                probe.finish_exact().unwrap();
            }

            let manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
            let crossed = manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), deal)
                .unwrap()
                .unwrap();
            assert_eq!(crossed.tokens_pending_anchor, TICK_SIZE, "{label}");
            assert_eq!(crossed.local_delivered_after_anchor, 0, "{label}");
            assert_eq!(crossed.available().unwrap(), cap - TICK_SIZE, "{label}");

            // The same observation again is a no-op, not a second seed.
            let repeated = manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), deal)
                .unwrap()
                .unwrap();
            assert_eq!(repeated, crossed, "{label}");

            // Neither is a restart from the durable record that already carries the crossed anchor.
            drop(manager);
            let restarted = CapacityManager::in_deals_dir(directory.path().to_path_buf());
            let after_restart = restarted
                .reconcile_deal(&tc, state(true, TICK_SIZE), deal)
                .unwrap()
                .unwrap();
            assert_eq!(after_restart, crossed, "{label}");

            // A second seed-sized advance is a plain unbacked advance: nothing was delivered since.
            let error = restarted
                .reconcile_deal(&tc, state(true, 2 * TICK_SIZE), deal)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("beyond durable local delivery 0"),
                "{label}: {error:#}"
            );
        }
    }

    #[test]
    fn only_authoritative_week_base_opens_one_no_rollover_quota() {
        let manager = CapacityManager::in_memory();
        let tc = "0:week".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        let first = manager.reserve(&tc, TICK_SIZE as u64).unwrap();
        first.record_delivered(TICK_SIZE as u64).unwrap();
        first.finish_exact().unwrap();
        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));

        // Wall clock is intentionally absent from this API: the same strict state cannot reopen capacity.
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));

        let next = manager
            .reconcile_deal(
                &tc,
                state(true, 2 * TICK_SIZE),
                subscription(1, 2 * TICK_SIZE),
            )
            .unwrap()
            .unwrap();
        assert_eq!(next.authoritative_cap, 4 * TICK_SIZE);
        assert_eq!(next.available().unwrap(), 2 * TICK_SIZE);
    }

    #[test]
    fn final_subscription_boundary_preserves_commitment_and_opens_no_capacity() {
        let manager = CapacityManager::in_memory();
        let tc = "0:final-boundary".to_string();
        let pending = 5 * TICK_SIZE;
        manager
            .reconcile_deal(&tc, state(true, pending), subscription(3, pending))
            .unwrap();
        let in_flight = manager.reserve(&tc, 300).unwrap();
        in_flight.record_delivered(100).unwrap();
        drop(in_flight);

        let final_snapshot = manager
            .reconcile_deal(
                &tc,
                state(true, pending),
                subscription(SUBSCRIPTION_WEEKS, pending),
            )
            .unwrap()
            .unwrap();
        assert_eq!(final_snapshot.tokens_pending_anchor, pending);
        assert_eq!(final_snapshot.local_delivered_after_anchor, 100);
        assert_eq!(final_snapshot.outstanding_reservation, 200);
        assert_eq!(final_snapshot.authoritative_cap, pending + 300);
        assert_eq!(final_snapshot.available().unwrap(), 0);
        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));
    }

    #[test]
    fn post_term_chain_growth_cannot_leave_cached_pre_term_capacity_open() {
        let manager = CapacityManager::in_memory();
        let tc = "0:post-term-growth".to_string();
        let pending = 5 * TICK_SIZE;
        manager
            .reconcile_deal(&tc, state(true, pending), subscription(3, pending))
            .unwrap();
        let in_flight = manager.reserve(&tc, 300).unwrap();
        in_flight.record_delivered(100).unwrap();
        drop(in_flight);

        let post_term_pending = pending + 500;
        let final_snapshot = manager
            .reconcile_deal(
                &tc,
                state(true, post_term_pending),
                subscription(SUBSCRIPTION_WEEKS, pending),
            )
            .unwrap()
            .unwrap();
        assert_eq!(final_snapshot.tokens_pending_anchor, post_term_pending);
        assert_eq!(final_snapshot.local_delivered_after_anchor, 0);
        assert_eq!(final_snapshot.outstanding_reservation, 200);
        assert_eq!(final_snapshot.authoritative_cap, post_term_pending + 200);
        assert_eq!(final_snapshot.available().unwrap(), 0);
        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));
    }

    #[test]
    fn exact_short_or_interrupted_completion_releases_only_unused_reservation() {
        let manager = CapacityManager::in_memory();
        let tc = "0:short".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        let reservation = manager.reserve(&tc, 80).unwrap();
        reservation.record_delivered(20).unwrap();
        reservation.finish_exact().unwrap();
        let snapshot = manager.snapshot(&tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 20);
        assert_eq!(snapshot.outstanding_reservation, 0);
        assert_eq!(snapshot.available().unwrap(), TICK_SIZE - 20);
    }

    #[test]
    fn ambiguous_usage_retains_the_unresolved_upper_bound() {
        let manager = CapacityManager::in_memory();
        let tc = "0:ambiguous".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        let reservation = manager.reserve(&tc, 80).unwrap();
        reservation.record_delivered(20).unwrap();
        reservation.finish_ambiguous().unwrap();
        let snapshot = manager.snapshot(&tc).unwrap().unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 20);
        assert_eq!(snapshot.outstanding_reservation, 60);
        assert_eq!(snapshot.available().unwrap(), TICK_SIZE - 80);
    }

    #[test]
    fn error_before_any_forwarded_output_releases_the_whole_reservation() {
        let manager = CapacityManager::in_memory();
        let tc = "0:no-output".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        let reservation = manager.reserve(&tc, 80).unwrap();
        reservation.finish_exact().unwrap();
        assert_eq!(
            manager.snapshot(&tc).unwrap().unwrap().available().unwrap(),
            TICK_SIZE
        );
    }

    #[test]
    fn lost_claim_response_reconciles_h_h_plus_k_and_h_plus_n_without_reopening() {
        for acknowledged in [0, 40, 100] {
            let manager = CapacityManager::in_memory();
            let tc = format!("0:lost-{acknowledged}");
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
                .unwrap();
            let reservation = manager.reserve(&tc, 100).unwrap();
            reservation.record_delivered(100).unwrap();
            reservation.finish_exact().unwrap();
            let available_before = manager.snapshot(&tc).unwrap().unwrap().available().unwrap();
            let after = manager
                .reconcile_deal(
                    &tc,
                    state(true, TICK_SIZE + acknowledged),
                    subscription(0, 0),
                )
                .unwrap()
                .unwrap();
            assert_eq!(after.available().unwrap(), available_before);
            assert_eq!(after.local_delivered_after_anchor, 100 - acknowledged);
            assert_invariant(after);
        }
    }

    #[test]
    fn restart_loads_local_delivery_and_outstanding_reservation_before_serving() {
        let directory = tempfile::tempdir().unwrap();
        let tc = "0:restart".to_string();
        {
            let manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
                .unwrap();
            let delivered = manager.reserve(&tc, 100).unwrap();
            // Below one coalescing interval, so this delivery is still an unapplied split at the crash.
            assert_eq!(delivered.record_delivered(40).unwrap(), 0);
            // Crash: neither the delivered remainder nor the outstanding reservation is released.
        }
        let restarted = CapacityManager::in_deals_dir(directory.path().to_path_buf());
        let snapshot = restarted
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap()
            .unwrap();
        // A crash mid-request loses the SPLIT, not the capacity: the 40 stay classified as reserved
        // rather than delivered. That is the conservative direction -- the tokens remain committed and
        // are never handed back out -- and it is sound only because the claim-driving counter is
        // advanced by what `record_delivered` reports DURABLE, so nothing was ever claimed for them.
        // What it does cost is revenue: up to one interval of genuinely delivered tokens the seller
        // can no longer bill for. The invariant the deal depends on is the last assertion.
        assert_eq!(snapshot.local_delivered_after_anchor, 0);
        assert_eq!(snapshot.outstanding_reservation, 100);
        assert_eq!(snapshot.available().unwrap(), TICK_SIZE - 100);
    }

    /// The flushed half of the same story: once a delivery crosses the interval it survives a crash,
    /// and the claim-driving counter is told about exactly that amount.
    #[test]
    fn restart_keeps_flushed_local_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let tc = "0:restart-flushed".to_string();
        let flushed = CAPACITY_PERSIST_TOKEN_INTERVAL as u64;
        {
            let manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
                .unwrap();
            let delivered = manager.reserve(&tc, flushed + 60).unwrap();
            assert_eq!(delivered.record_delivered(flushed).unwrap(), flushed);
        }
        let restarted = CapacityManager::in_deals_dir(directory.path().to_path_buf());
        let snapshot = restarted
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.local_delivered_after_anchor,
            u128::from(flushed),
            "a flushed delivery is durable across a crash"
        );
        assert_eq!(snapshot.outstanding_reservation, 60);
        assert_eq!(
            snapshot.available().unwrap(),
            TICK_SIZE - u128::from(flushed) - 60
        );
    }

    #[test]
    fn crash_after_reservation_before_upstream_remains_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let tc = "0:pre-upstream-crash".to_string();
        {
            let manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
                .unwrap();
            let _reservation = manager.reserve(&tc, TICK_SIZE as u64).unwrap();
        }
        let restarted = CapacityManager::in_deals_dir(directory.path().to_path_buf());
        let snapshot = restarted
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.outstanding_reservation, TICK_SIZE);
        assert!(matches!(
            restarted.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));
    }

    #[test]
    fn malformed_regressing_and_overflowing_states_fail_closed() {
        let manager = CapacityManager::in_memory();
        let tc = "0:bad".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE + 10), subscription(0, 0))
            .unwrap();
        let regression = manager
            .reconcile_deal(&tc, state(true, TICK_SIZE + 9), subscription(0, 0))
            .unwrap_err();
        assert!(
            regression.to_string().contains("regressed"),
            "{regression:#}"
        );

        let overflow = authoritative_cap(
            state(true, TICK_SIZE),
            DealSubscription {
                deal_flags: flags::SUBSCRIPTION,
                sub_weeks: 4,
                week_index: 1,
                tokens_per_week: 1,
                funded_tokens: u128::MAX,
                tokens_paid: 1,
                period_start: 1,
                week_base_tokens: u128::MAX,
            },
        )
        .unwrap_err();
        assert!(overflow.to_string().contains("overflows"), "{overflow:#}");
    }

    #[test]
    fn ordinary_deal_is_bounded_by_actual_funded_volume() {
        let manager = CapacityManager::in_memory();
        let tc = "0:ordinary".to_string();
        let funded = TICK_SIZE + 125;
        let registered = manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(funded))
            .unwrap()
            .unwrap();
        assert_eq!(registered.authoritative_cap, funded);
        assert_eq!(registered.available().unwrap(), 125);
        let exact_remainder = manager.reserve(&tc, u64::MAX).unwrap();
        assert_eq!(exact_remainder.amount(), 125);
        exact_remainder.record_delivered(125).unwrap();
        exact_remainder.finish_exact().unwrap();
        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));
    }

    #[test]
    fn ordinary_probe_opens_only_one_tick_then_exact_funded_remainder() {
        let manager = CapacityManager::in_memory();
        let tc = "0:ordinary-probe".to_string();
        let funded = 3 * TICK_SIZE;
        manager
            .reconcile_deal(&tc, state(false, 0), ordinary(funded))
            .unwrap();
        let probe = manager.reserve(&tc, u64::MAX).unwrap();
        assert_eq!(probe.amount(), TICK_SIZE as u64);
        probe.record_delivered(TICK_SIZE as u64).unwrap();
        probe.finish_exact().unwrap();

        let accepted = manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(funded))
            .unwrap()
            .unwrap();
        assert_eq!(accepted.local_delivered_after_anchor, 0);
        assert_eq!(accepted.authoritative_cap, funded);
        assert_eq!(accepted.available().unwrap(), 2 * TICK_SIZE);
    }

    #[test]
    fn ordinary_sequential_requests_share_the_funded_total() {
        let manager = CapacityManager::in_memory();
        let tc = "0:ordinary-sequential".to_string();
        let funded = TICK_SIZE + 900;
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(funded))
            .unwrap();
        let first = manager.reserve(&tc, 600).unwrap();
        first.record_delivered(600).unwrap();
        first.finish_exact().unwrap();
        let second = manager.reserve(&tc, 600).unwrap();
        assert_eq!(second.amount(), 300);
        second.record_delivered(300).unwrap();
        second.finish_exact().unwrap();
        assert!(matches!(
            manager.reserve(&tc, 1),
            Err(ReserveError::Exhausted)
        ));
    }

    #[test]
    fn ordinary_concurrent_requests_cannot_share_the_same_remainder() {
        let manager = Arc::new(CapacityManager::in_memory());
        let tc = "0:ordinary-race".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(TICK_SIZE + 1_000))
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let manager = manager.clone();
            let tc = tc.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                manager.reserve(&tc, 750).unwrap()
            }));
        }
        barrier.wait();
        let reservations = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            reservations
                .iter()
                .map(CapacityReservation::amount)
                .sum::<u64>(),
            1_000
        );
        assert_invariant(manager.snapshot(&tc).unwrap().unwrap());
    }

    #[test]
    fn ordinary_restart_retains_local_and_ambiguous_capacity_debt() {
        let directory = tempfile::tempdir().unwrap();
        let tc = "0:ordinary-restart".to_string();
        let funded = TICK_SIZE + 1_000;
        {
            let manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(funded))
                .unwrap();
            let reservation = manager.reserve(&tc, 800).unwrap();
            reservation.record_delivered(300).unwrap();
            reservation.finish_ambiguous().unwrap();
        }
        let restarted = CapacityManager::in_deals_dir(directory.path().to_path_buf());
        let snapshot = restarted
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(funded))
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.local_delivered_after_anchor, 300);
        assert_eq!(snapshot.outstanding_reservation, 500);
        assert_eq!(snapshot.available().unwrap(), 200);
        assert_invariant(snapshot);
    }

    #[test]
    fn terminal_state_removes_durable_capacity_record() {
        let directory = tempfile::tempdir().unwrap();
        let tc = "0:terminal".to_string();
        let manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
            .unwrap();
        let reservation = manager.reserve(&tc, 1).unwrap();
        reservation.finish_ambiguous().unwrap();
        let mut terminal = state(true, TICK_SIZE);
        terminal.opened = false;
        terminal.deposit = 0;
        assert!(manager
            .reconcile_deal(&tc, terminal, subscription(0, 0))
            .unwrap()
            .is_none());
        assert!(manager.snapshot(&tc).unwrap().is_none());
        assert_eq!(
            std::fs::read_dir(directory.path().join("seller-capacity"))
                .unwrap()
                .count(),
            0
        );
    }

    proptest! {
        #[test]
        fn arbitrary_weekly_underuse_never_becomes_post_term_capacity(
            weekly_delivery_after_probe in (
                0_u32..=(WEEK_QUOTA - TICK_SIZE) as u32,
                0_u32..=WEEK_QUOTA as u32,
                0_u32..=WEEK_QUOTA as u32,
                0_u32..=WEEK_QUOTA as u32,
            )
        ) {
            let (week_zero, week_one, week_two, week_three) = weekly_delivery_after_probe;
            let pending = TICK_SIZE
                + u128::from(week_zero)
                + u128::from(week_one)
                + u128::from(week_two)
                + u128::from(week_three);
            let manager = CapacityManager::in_memory();
            let tc = "0:post-term-property".to_string();
            let snapshot = manager
                .reconcile_deal(
                    &tc,
                    state(true, pending),
                    subscription(SUBSCRIPTION_WEEKS, pending),
                )
                .unwrap()
                .unwrap();

            prop_assert_eq!(snapshot.tokens_pending_anchor, pending);
            prop_assert_eq!(snapshot.authoritative_cap, pending);
            prop_assert_eq!(snapshot.available().unwrap(), 0);
            prop_assert!(matches!(
                manager.reserve(&tc, 1),
                Err(ReserveError::Exhausted)
            ));
        }
    }

    proptest! {
        #[test]
        fn capacity_invariant_survives_request_interleavings(
            operations in prop::collection::vec((0_u8..4, 1_u16..40), 1..120)
        ) {
            let manager = CapacityManager::in_memory();
            let tc = "0:property".to_string();
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), subscription(0, 0))
                .unwrap();
            let mut reservations: Vec<CapacityReservation> = Vec::new();
            for (operation, amount) in operations {
                match operation {
                    0 => {
                        if let Ok(reservation) = manager.reserve(&tc, u64::from(amount)) {
                            reservations.push(reservation);
                        }
                    }
                    1 => {
                        if let Some(reservation) = reservations.first() {
                            let delivered = reservation.remaining().min(u64::from(amount));
                            if delivered > 0 {
                                reservation.record_delivered(delivered).unwrap();
                            }
                        }
                    }
                    2 => {
                        if !reservations.is_empty() {
                            reservations.remove(0).finish_exact().unwrap();
                        }
                    }
                    _ => {
                        if !reservations.is_empty() {
                            reservations.remove(0).finish_ambiguous().unwrap();
                        }
                    }
                }
                let snapshot = manager.snapshot(&tc).unwrap().unwrap();
                prop_assert!(snapshot.committed().unwrap() <= snapshot.authoritative_cap);
                prop_assert!(snapshot.authoritative_cap <= snapshot.funded_tokens);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn capacity_invariant_survives_durable_crash_and_restart_points(
            operations in prop::collection::vec((1_u16..200, 0_u16..200, 0_u8..3, 0_u16..200), 1..40)
        ) {
            let directory = tempfile::tempdir().unwrap();
            let tc = "0:durable-property".to_string();
            let mut pending = TICK_SIZE;
            let mut manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
            manager
                .reconcile_deal(&tc, state(true, pending), subscription(0, 0))
                .unwrap();

            for (requested, delivered, finish, acknowledged) in operations {
                if let Ok(reservation) = manager.reserve(&tc, u64::from(requested)) {
                    let delivered = reservation.remaining().min(u64::from(delivered));
                    if delivered > 0 {
                        reservation.record_delivered(delivered).unwrap();
                    }
                    match finish {
                        0 => {
                            reservation.finish_exact().unwrap();
                        }
                        1 => {
                            reservation.finish_ambiguous().unwrap();
                        }
                        _ => drop(reservation),
                    }
                }
                let before = manager.snapshot(&tc).unwrap().unwrap();
                assert_invariant(before);
                let acknowledged =
                    before.local_delivered_after_anchor.min(u128::from(acknowledged));
                pending += acknowledged;

                drop(manager);
                manager = CapacityManager::in_deals_dir(directory.path().to_path_buf());
                let after = manager
                    .reconcile_deal(&tc, state(true, pending), subscription(0, 0))
                    .unwrap()
                    .unwrap();
                prop_assert_eq!(after.available().unwrap(), before.available().unwrap());
                prop_assert!(after.committed().unwrap() <= after.authoritative_cap);
                prop_assert!(after.authoritative_cap <= after.funded_tokens);
            }
        }
    }

    proptest! {
        #[test]
        fn ordinary_capacity_invariant_survives_request_interleavings(
            funded_remainder in 1_u16..2_000,
            operations in prop::collection::vec((0_u8..4, 1_u16..100), 1..80),
        ) {
            let manager = CapacityManager::in_memory();
            let tc = "0:ordinary-property".to_string();
            let funded = TICK_SIZE + u128::from(funded_remainder);
            manager
                .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(funded))
                .unwrap();
            let mut reservations: Vec<CapacityReservation> = Vec::new();
            for (operation, amount) in operations {
                match operation {
                    0 => {
                        if let Ok(reservation) = manager.reserve(&tc, u64::from(amount)) {
                            reservations.push(reservation);
                        }
                    }
                    1 => {
                        if let Some(reservation) = reservations.first() {
                            let delivered = reservation.remaining().min(u64::from(amount));
                            if delivered > 0 {
                                reservation.record_delivered(delivered).unwrap();
                            }
                        }
                    }
                    2 => {
                        if !reservations.is_empty() {
                            reservations.remove(0).finish_exact().unwrap();
                        }
                    }
                    _ => {
                        if !reservations.is_empty() {
                            reservations.remove(0).finish_ambiguous().unwrap();
                        }
                    }
                }
                let snapshot = manager.snapshot(&tc).unwrap().unwrap();
                prop_assert!(snapshot.committed().unwrap() <= snapshot.authoritative_cap);
                prop_assert!(snapshot.authoritative_cap <= snapshot.funded_tokens);
            }
        }
    }

    // ----: what the served-model check may cost the buyer's reservation ----

    /// A provider socket that answers one `chat/completions` with the given SSE body and closes.
    /// A real socket and a real HTTP response, because the check under test reads PROVIDER frames: a
    /// hand-built event would prove nothing about the path that runs in production.
    async fn provider_serving(body: String) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = vec![0_u8; 8192];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        (address, handle)
    }

    /// The gateway's own recorder (`seller::gateway::CapacityDeliveryRecorder`) is private to that module,
    /// so this reproduces exactly its reservation contract and nothing else: clean terminal usage atomically
    /// commits its total, while an unsuccessful terminal classification either releases the grant or keeps
    /// the unresolved reservation committed.
    #[derive(Clone)]
    struct RelayRecorder {
        reservation: std::sync::Arc<CapacityReservation>,
        finish: std::sync::Arc<
            std::sync::Mutex<Option<crate::seller::gateway::AuthoritativeDeliveryFinish>>,
        >,
    }

    impl crate::seller::gateway::AuthoritativeDeliveryRecorder for RelayRecorder {
        fn record_authoritative_delivery(
            &self,
            event: crate::seller::gateway::AuthoritativeDeliveryEvent,
        ) -> std::result::Result<(), tonic::Status> {
            use crate::seller::gateway::{AuthoritativeDeliveryEvent, AuthoritativeDeliveryFinish};
            match event {
                AuthoritativeDeliveryEvent::Completed(tokens) => {
                    self.reservation.finish_with_delivery(tokens.get()).unwrap();
                }
                AuthoritativeDeliveryEvent::Finished(finish) => {
                    *self.finish.lock().unwrap() = Some(finish);
                    match finish {
                        AuthoritativeDeliveryFinish::Interrupted => {
                            self.reservation.finish_exact().unwrap()
                        }
                        AuthoritativeDeliveryFinish::AmbiguousUsage => {
                            self.reservation.finish_ambiguous().unwrap()
                        }
                    };
                }
            }
            Ok(())
        }
    }

    /// What one buyer request against `body` leaves behind: everything the buyer received, the relay's
    /// terminal classification, and the durable capacity record after the request terminal.
    async fn relay_one_request(
        upstream: impl FnOnce(String) -> crate::seller::OpenAiConfig,
        body: String,
    ) -> (
        Vec<std::result::Result<dexdo_proto::CanonChunk, tonic::Status>>,
        Option<crate::seller::gateway::AuthoritativeDeliveryFinish>,
        DurableCapacityRecord,
    ) {
        use dexdo_proto::{CanonRequest, ChatMessage, SamplingParams};

        const GRANT: u64 = 8;

        let (address, provider) = provider_serving(body).await;
        let cfg = upstream(format!("http://{address}"));

        let dir = tempfile::tempdir().unwrap();
        let manager = CapacityManager::in_deals_dir(dir.path().to_path_buf());
        let tc = "0:served-model".to_string();
        manager
            .reconcile_deal(&tc, state(true, TICK_SIZE), ordinary(ORDINARY_FUNDED))
            .unwrap();
        let path = capacity_path(&dir.path().join("seller-capacity"), &tc);
        let recorder = RelayRecorder {
            reservation: std::sync::Arc::new(manager.reserve(&tc, GRANT).unwrap()),
            finish: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };

        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            params: Some(SamplingParams {
                temperature: 0.0,
                max_tokens: GRANT as u32,
                stop: Vec::new(),
                greedy: false,
            }),
        };
        let (up_tx, up_rx) = tokio::sync::mpsc::channel(8);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let adapter = tokio::spawn(async move {
            crate::seller::UpstreamConfig::OpenAi(cfg)
                .run(GRANT, Some(request), up_tx)
                .await;
        });
        crate::seller::gateway::relay_counting(up_rx, tx, recorder.clone(), None).await;
        adapter.await.unwrap();
        provider.abort();

        let mut received = Vec::new();
        while let Some(item) = rx.recv().await {
            received.push(item);
        }
        let finish = *recorder.finish.lock().unwrap();
        (received, finish, load_record(&path).unwrap().unwrap())
    }

    /// The seller's committed identity in the shipped configuration: the Groq slug it sends upstream, the
    /// canonical market id it sells under, and no declared extra spellings.
    fn qwen_upstream(base_url: String) -> crate::seller::OpenAiConfig {
        crate::seller::OpenAiConfig {
            base_url,
            model: "qwen/qwen3-32b".to_string(),
            frame_model: "qwen--qwen3--32b".to_string(),
            // `PATH` is always set and non-empty, so the adapter reaches the provider without mutating
            // process-global environment while other tests run.
            api_key_env: "PATH".to_string(),
            ..crate::seller::OpenAiConfig::default()
        }
    }

    /// (money): the served-model check must not fire once the buyer's capacity is in flight.

    /// An error returned from the adapter after a chunk has been forwarded reaches the relay with output
    /// delivered and its authoritative usage still outstanding -- which is `AmbiguousUsage`, and that
    /// terminal deliberately keeps the unresolved remainder COMMITTED (pinned by
    /// `ambiguous_terminal_flushes_the_coalesced_delivery_split` above). The buyer would lose capacity it
    /// paid for and the seller could not claim the tokens it had already delivered: two-sided loss, in
    /// answer to a misspelled `served_model`. So past the first delivered output the divergence is a
    /// diagnostic, and the stream is carried to its honest terminal.

    /// Nothing is given up by that bound: an OpenAI-compatible provider names the model in its FIRST frame,
    /// so a real mismatch is always seen before any output -- including in seller readiness, where
    /// E2E-ADV-02 refuses before `postSellOffer` (`upstream::tests`).
    #[tokio::test]
    async fn late_served_model_divergence_does_not_strand_the_reservation() {
        let (received, finish, durable) = relay_one_request(
            qwen_upstream,
            "data: {\"model\":\"qwen/qwen3-32b\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"first \"}}]}\n\n\
             data: {\"model\":\"meta-llama/llama-3.3-70b-versatile\",\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n\
             data: [DONE]\n\n"
                .to_string(),
        )
        .await;

        let delivered: Vec<String> = received
            .iter()
            .filter_map(|item| match item {
                Ok(chunk) if chunk.usage.is_none() => Some(chunk.text.clone()),
                Ok(_) => None,
                Err(status) => panic!(
                    ": a provider that renamed itself after the first delivered chunk tore the \
                     stream down: {status:?}"
                ),
            })
            .collect();
        assert_eq!(delivered, vec!["first ".to_string(), "second".to_string()]);
        assert_eq!(
            finish, None,
            ": successful completion uses the single atomic Completed transition"
        );
        assert_eq!(
            durable.outstanding_reservation, 0,
            ": the unused remainder of the buyer's grant came back"
        );
        assert_eq!(
            durable.local_delivered_after_anchor, 3,
            "the provider's own terminal total is what was delivered and is claimable"
        );
        assert_invariant(CapacitySnapshot::from(&durable));
    }

    /// an operator who declared the provider's own spelling the one supported way (`identity_aliases`
    /// in `models.json`, the same field the buyer reconciles identity through) is served, not refused.

    /// Here the provider answers `Qwen/Qwen3-32B` while the seller sends the slug `qwen3-32b` upstream and
    /// sells under `alibaba--qwen3--32b`: the reported spelling is reachable ONLY through the declared
    /// alias, so this fails the moment `identity_aliases` stops being part of the accepted set -- and it
    /// fails at the first frame, taking an honest seller off the market for a spelling it declared.
    #[tokio::test]
    async fn a_declared_identity_alias_is_an_accepted_served_model() {
        let (received, finish, durable) = relay_one_request(
            |base_url| crate::seller::OpenAiConfig {
                base_url,
                model: "qwen3-32b".to_string(),
                frame_model: "alibaba--qwen3--32b".to_string(),
                api_key_env: "PATH".to_string(),
                identity_aliases: vec!["Qwen/Qwen3-32B".to_string()],
                ..crate::seller::OpenAiConfig::default()
            },
            "data: {\"model\":\"Qwen/Qwen3-32B\",\"choices\":[{\"delta\":{\"content\":\"served\"}}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n\
             data: [DONE]\n\n"
                .to_string(),
        )
        .await;

        let delivered: Vec<String> = received
            .iter()
            .filter_map(|item| match item {
                Ok(chunk) if chunk.usage.is_none() => Some(chunk.text.clone()),
                Ok(_) => None,
                Err(status) => panic!(
                    ": a model declared through identity_aliases was refused as a substitution: \
                     {status:?}"
                ),
            })
            .collect();
        assert_eq!(delivered, vec!["served".to_string()]);
        assert_eq!(
            finish, None,
            ": the declared alias completes through the single atomic transition"
        );
        assert_eq!(durable.outstanding_reservation, 0);
        assert_invariant(CapacitySnapshot::from(&durable));
    }
}
