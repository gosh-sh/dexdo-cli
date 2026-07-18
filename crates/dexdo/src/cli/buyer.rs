//! Buyer and subscription command handlers, moved out of `commands.rs`(C15, move-only).

use crate::cli::args::*;
#[cfg(any(test, feature = "shellnet"))]
use crate::cli::commands::direct_chain_read_with_timeout;
#[cfg(feature = "shellnet")]
use crate::cli::commands::{
    acquire_pool_write_lock, load_pool_json, model_target_from_config, note_pool_path,
    read_book_target, target_from_market, try_acquire_pool_write_lock, with_pool_write_lock,
    write_pool_private, BookTarget, PoolWriteLock,
};
#[cfg(all(test, feature = "shellnet"))]
use crate::cli::commands::{
    close_hint, is_note_deploy_wallet_busy_error, note_deploy_error,
    note_deploy_fold_state_into_pool, note_deploy_fold_state_into_pool_locked,
    note_deploy_multisig_secret_hex, note_deploy_recovery_pool_guard,
    note_deploy_same_file_pool_guard, note_endpoint_url, persist_pool_recovery_record,
    resolve_pool_recovery_inputs, retry_executable_read, target_from_market_for_model,
    write_pool_private_via_temp, DealTarget, PoolRecoveryRecord, EXECUTABLE_READ_BACKOFF,
};
use crate::cli::commands::{
    enforce_model_registry_policy, expected_order_book_for_note,
    load_enabled_model_registry_policy, mock_orders_from_offers, note_pubkey_id,
    order_book_active_from_contracts, print_book_table, save_mock_runtime_deal_handle,
    save_runtime_deal_handle, shellnet_doctor_preflight, unix_now_secs, BookRow,
    RuntimeDealHandleInput, DEAL_WAIT_SECS, RESUME_LOOKBACK_SECS, TRANSIENT_QUOTE_ATTEMPTS,
    TRANSIENT_QUOTE_INITIAL_BACKOFF,
};
use crate::cli::deals;
use crate::cli::machine;
use crate::cli::policy;
#[cfg(test)]
use crate::cli::seller::enforce_seller_runtime_policy;
#[cfg(test)]
use crate::cli::seller_policy::{
    apply_seller_dispute_policy, apply_seller_terminal_policy, classify_by_fact_advance_failure,
    is_err_not_open, AdvanceFailureDisposition, SellerTerminalPolicyOutcome,
};
use crate::cli::support::*;
use crate::operator_shutdown_signal;
use anyhow::{anyhow, bail, Result};
#[cfg(feature = "shellnet")]
use dexdo::registry::{
    default_model_registry_address, resolve_registered_model_identity, ShellnetModelRegistryReader,
};
use dexdo::registry::{BuyerMissingBookPolicy, RegistryRole};
use dexdo_core::{
    check_buy_deposit_headroom, check_matched_token_contract_state, executable_quote,
    model_hash_for, required_escrow_for_buy, submit_safe_single_ask_quote, ChainBackend,
    ChainError, DealChainState, ExecutableQuote, MatchedTokenContractStatus, OrderBookOrder,
    Settlement, MATCH_OPEN_TIMEOUT_SECS,
};
#[cfg(feature = "shellnet")]
use dexdo_core::{InferenceSubscriptionPlacement, OrderBookSnapshot, OrderBookSubscription};
use serde_json::{json, Map, Value};
#[cfg(feature = "shellnet")]
use std::future::Future;
use std::sync::Arc;

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
struct BuyerMoneyLock {
    note_addr: String,
    path: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    subscriptions_path: std::path::PathBuf,
    lock: Option<PoolWriteLock>,
}
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "shellnet", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "shellnet", serde(rename_all = "snake_case"))]
enum BuyerSubmitIntentKind {
    LegacyUnknown,
    Foreground,
    OnDemand,
    PolicyNextSeller,
    ContinuityNextSeller,
    ContinuityRenewal,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "shellnet", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "shellnet", serde(deny_unknown_fields))]
struct BuyerSubmitIntent {
    kind: BuyerSubmitIntentKind,
    predecessor_token_contract: Option<dexdo_core::TokenContract>,
}

#[allow(dead_code)]
impl BuyerSubmitIntent {
    fn foreground() -> Self {
        Self {
            kind: BuyerSubmitIntentKind::Foreground,
            predecessor_token_contract: None,
        }
    }

    fn on_demand() -> Self {
        Self {
            kind: BuyerSubmitIntentKind::OnDemand,
            predecessor_token_contract: None,
        }
    }

    fn after(kind: BuyerSubmitIntentKind, predecessor: &str) -> Self {
        Self {
            kind,
            predecessor_token_contract: Some(predecessor.to_string()),
        }
    }

    #[cfg(feature = "shellnet")]
    fn validate(&self) -> Result<()> {
        let requires_predecessor = matches!(
            self.kind,
            BuyerSubmitIntentKind::PolicyNextSeller
                | BuyerSubmitIntentKind::ContinuityNextSeller
                | BuyerSubmitIntentKind::ContinuityRenewal
        );
        if requires_predecessor != self.predecessor_token_contract.is_some() {
            bail!(
                "buyer submit intent {:?} has invalid predecessor presence",
                self.kind
            );
        }
        if let Some(predecessor) = &self.predecessor_token_contract {
            dexdo_core::Address::parse(predecessor).map_err(|error| {
                anyhow::anyhow!("buyer submit predecessor TokenContract: {error}")
            })?;
        }
        Ok(())
    }
}

#[cfg(feature = "shellnet")]
const BUYER_SUBMIT_JOURNAL_SCHEMA: &str = "dexdo.buyer.submit.v2";
#[cfg(feature = "shellnet")]
const BUYER_SUBMIT_JOURNAL_SCHEMA_V1: &str = "dexdo.buyer.submit.v1";
#[cfg(feature = "shellnet")]
const BUYER_SUBSCRIPTION_SUBMIT_SCHEMA: &str = "dexdo.buyer.subscription.submit.v1";
#[cfg(feature = "shellnet")]
const BUYER_SUBSCRIPTION_STATE_SCHEMA: &str = "dexdo.buyer.subscriptions.v1";
#[cfg(feature = "shellnet")]
const INFERENCE_SUBSCRIPTION_CYCLES: u128 = 4;

/// Journal-only representation of an owner-facing fill. The chain event decoder
/// that produces these records is intentionally wired in a later layer.
#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerJournalMatch {
    token_contract: dexdo_core::TokenContract,
    order_id: u128,
    ticks: u128,
    clearing_price: u128,
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerSubmitJournal {
    schema: String,
    note_addr: String,
    order_book: String,
    intent: BuyerSubmitIntent,
    expected_token_contract: Option<dexdo_core::TokenContract>,
    quoted_order: OrderBookOrder,
    quote: ExecutableQuote,
    cursor: dexdo_core::MatchWatchCursor,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
    submit_identity: String,
    created_at_unix: u64,
    #[serde(default)]
    resolved_match: Option<BuyerJournalMatch>,
    #[serde(default)]
    resolved_matches: Vec<BuyerJournalMatch>,
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerSubmitJournalV1 {
    schema: String,
    note_addr: String,
    order_book: String,
    expected_token_contract: Option<dexdo_core::TokenContract>,
    quoted_order: OrderBookOrder,
    quote: ExecutableQuote,
    cursor: dexdo_core::MatchWatchCursor,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
    submit_identity: String,
    created_at_unix: u64,
    #[serde(default)]
    resolved_match: Option<BuyerJournalMatch>,
}

#[cfg(feature = "shellnet")]
impl From<BuyerSubmitJournalV1> for BuyerSubmitJournal {
    fn from(legacy: BuyerSubmitJournalV1) -> Self {
        let resolved_matches = legacy.resolved_match.clone().into_iter().collect();
        Self {
            schema: BUYER_SUBMIT_JOURNAL_SCHEMA.to_string(),
            note_addr: legacy.note_addr,
            order_book: legacy.order_book,
            intent: BuyerSubmitIntent {
                kind: BuyerSubmitIntentKind::LegacyUnknown,
                predecessor_token_contract: None,
            },
            expected_token_contract: legacy.expected_token_contract,
            quoted_order: legacy.quoted_order,
            quote: legacy.quote,
            cursor: legacy.cursor,
            ticks: legacy.ticks,
            max_price_per_tick: legacy.max_price_per_tick,
            escrow: legacy.escrow,
            submit_identity: legacy.submit_identity,
            created_at_unix: legacy.created_at_unix,
            resolved_match: legacy.resolved_match,
            resolved_matches,
        }
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerSubscriptionSubmitJournal {
    schema: String,
    note_addr: String,
    order_book: String,
    frame_model: String,
    model_hash: String,
    max_price_per_tick: u128,
    ticks: u128,
    escrow: u128,
    cycle_budget: u128,
    auto_renew: bool,
    order_id_floor: u128,
    fill_cursor: dexdo_core::MatchWatchCursor,
    submit_identity: String,
    created_at_unix: u64,
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerSubscriptionState {
    schema: String,
    note_addr: String,
    books: Vec<BuyerSubscriptionBookState>,
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerSubscriptionBookState {
    order_book: String,
    frame_model: String,
    model_hash: String,
    fill_cursor: dexdo_core::MatchWatchCursor,
    subscriptions: Vec<BuyerSubscriptionRecord>,
    #[serde(default)]
    unattributed_matches: Vec<BuyerJournalMatch>,
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerSubscriptionRecord {
    order_id: u128,
    max_price_per_tick: u128,
    ticks: u128,
    escrow: u128,
    cycle_budget: u128,
    auto_renew: bool,
    placed_at_unix: i64,
    active: bool,
    #[serde(default)]
    matches: Vec<BuyerJournalMatch>,
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
#[derive(Debug)]
enum BuyerMoneyJournal {
    Buy(Box<BuyerSubmitJournal>),
    Subscription(Box<BuyerSubscriptionSubmitJournal>),
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
impl BuyerSubmitJournal {
    fn validate(&self, expected_note_addr: &str) -> Result<()> {
        if self.schema != BUYER_SUBMIT_JOURNAL_SCHEMA {
            bail!("unsupported buyer submit journal schema {}", self.schema);
        }
        let note_addr = dexdo_core::Address::parse(&self.note_addr)
            .map_err(|error| anyhow::anyhow!("buyer submit journal note_addr: {error}"))?
            .with_workchain();
        if !note_addr.eq_ignore_ascii_case(expected_note_addr) {
            bail!(
                "buyer submit journal belongs to note {}, expected {}",
                note_addr,
                expected_note_addr
            );
        }
        dexdo_core::Address::parse(&self.order_book)
            .map_err(|error| anyhow::anyhow!("buyer submit journal order_book: {error}"))?;
        self.intent.validate()?;
        let quoted_tc =
            self.quoted_order.token_contract.as_deref().ok_or_else(|| {
                anyhow::anyhow!("buyer submit journal quote has no TokenContract")
            })?;
        dexdo_core::Address::parse(quoted_tc).map_err(|error| {
            anyhow::anyhow!("buyer submit journal quoted TokenContract: {error}")
        })?;
        if let Some(expected) = &self.expected_token_contract {
            let expected = dexdo_core::Address::parse(expected)
                .map_err(|error| {
                    anyhow::anyhow!("buyer submit journal expected TokenContract: {error}")
                })?
                .with_workchain();
            if !expected.eq_ignore_ascii_case(quoted_tc) {
                bail!(
                    "buyer submit journal expected TokenContract {} differs from quoted {}",
                    expected,
                    quoted_tc
                );
            }
        }
        if !self.quoted_order.is_resting_ask()
            || self.quoted_order.ticks < self.ticks
            || self.quoted_order.price_per_tick > self.max_price_per_tick
        {
            bail!("buyer submit journal quote is not executable for its recorded request");
        }
        check_buy_deposit_headroom(self.escrow, self.ticks, self.max_price_per_tick)
            .map_err(anyhow::Error::msg)?;
        let quoted_fill = self.quote.fills.as_slice();
        if !self.quote.complete
            || self.quote.filled_ticks != self.ticks
            || quoted_fill.len() != 1
            || quoted_fill[0].order_id != self.quoted_order.order_id
            || !quoted_fill[0]
                .token_contract
                .eq_ignore_ascii_case(quoted_tc)
            || quoted_fill[0].ticks != self.ticks
            || quoted_fill[0].price_per_tick != self.quoted_order.price_per_tick
            || quoted_fill[0].cost_with_fee != self.quote.total_with_fee
        {
            bail!("buyer submit journal executable quote differs from its recorded order/request");
        }
        validate_buyer_submit_identity(&self.submit_identity, "buyer submit journal")?;
        if let Some(resolved) = &self.resolved_match {
            dexdo_core::Address::parse(&resolved.token_contract).map_err(|error| {
                anyhow::anyhow!("buyer submit journal resolved TokenContract: {error}")
            })?;
        }
        let mut resolved_token_contracts = std::collections::BTreeSet::new();
        for resolved in &self.resolved_matches {
            let token_contract = dexdo_core::Address::parse(&resolved.token_contract)
                .map_err(|error| {
                    anyhow::anyhow!("buyer submit journal resolved TokenContract: {error}")
                })?
                .with_workchain();
            if !resolved_token_contracts.insert(token_contract.clone()) {
                bail!("buyer submit journal repeats resolved TokenContract {token_contract}");
            }
        }
        if let (Some(first), Some(resolved)) =
            (self.resolved_matches.first(), self.resolved_match.as_ref())
        {
            if first != resolved {
                bail!("buyer submit journal scalar/vector resolved match disagree");
            }
        }
        Ok(())
    }
}

#[cfg(feature = "shellnet")]
fn validate_buyer_submit_identity(identity: &str, label: &str) -> Result<()> {
    let digest = identity
        .strip_prefix("boc-sha256:")
        .ok_or_else(|| anyhow::anyhow!("{label} has no BOC identity"))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{label} has malformed BOC identity");
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn buyer_submit_recovery_anchor(
    order: &OrderBookOrder,
) -> Result<dexdo::buyer::api::BuyerSubmitRecoveryAnchor> {
    let token_contract = order
        .token_contract
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("buyer recovery anchor has no TokenContract"))?;
    let token_contract = dexdo_core::Address::parse(token_contract)
        .map_err(|error| anyhow::anyhow!("buyer recovery anchor TokenContract: {error}"))?
        .with_workchain();
    Ok(dexdo::buyer::api::BuyerSubmitRecoveryAnchor {
        order_id: order.order_id,
        token_contract,
    })
}

#[cfg(feature = "shellnet")]
fn buyer_submit_reconciliation(
    journal: &BuyerSubmitJournal,
    state: dexdo::buyer::api::BuyerSubmitReconciliationState,
    origin: dexdo::buyer::api::BuyerSubmitReconciliationOrigin,
) -> Result<dexdo::buyer::api::BuyerSubmitReconciliation> {
    validate_buyer_submit_identity(&journal.submit_identity, "buyer submit journal")?;
    Ok(dexdo::buyer::api::BuyerSubmitReconciliation {
        submit_identity: journal.submit_identity.clone(),
        recovery_anchor: buyer_submit_recovery_anchor(&journal.quoted_order)?,
        state,
        origin,
    })
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
impl BuyerSubscriptionSubmitJournal {
    fn validate(&self, expected_note_addr: &str) -> Result<()> {
        if self.schema != BUYER_SUBSCRIPTION_SUBMIT_SCHEMA {
            bail!(
                "unsupported buyer subscription submit journal schema {}",
                self.schema
            );
        }
        let note_addr = dexdo_core::Address::parse(&self.note_addr)
            .map_err(|error| anyhow::anyhow!("buyer subscription journal note_addr: {error}"))?
            .with_workchain();
        if !note_addr.eq_ignore_ascii_case(expected_note_addr) {
            bail!(
                "buyer subscription journal belongs to note {}, expected {}",
                note_addr,
                expected_note_addr
            );
        }
        dexdo_core::Address::parse(&self.order_book)
            .map_err(|error| anyhow::anyhow!("buyer subscription journal order_book: {error}"))?;
        if self.frame_model.trim().is_empty()
            || !model_hash_for(&self.frame_model).eq_ignore_ascii_case(&self.model_hash)
        {
            bail!("buyer subscription journal model identity is inconsistent");
        }
        if self.max_price_per_tick == 0 || self.ticks == 0 || self.escrow == 0 {
            bail!("buyer subscription journal has a zero-sized money term");
        }
        check_buy_deposit_headroom(self.escrow, self.ticks, self.max_price_per_tick)
            .map_err(anyhow::Error::msg)?;
        if self.cycle_budget != self.escrow / INFERENCE_SUBSCRIPTION_CYCLES {
            bail!(
                "buyer subscription journal cycle_budget {} differs from contract escrow/{} = {}",
                self.cycle_budget,
                INFERENCE_SUBSCRIPTION_CYCLES,
                self.escrow / INFERENCE_SUBSCRIPTION_CYCLES
            );
        }
        validate_buyer_submit_identity(&self.submit_identity, "buyer subscription submit journal")
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
impl BuyerSubscriptionState {
    fn empty(note_addr: &str) -> Result<Self> {
        let note_addr = dexdo_core::Address::parse(note_addr)
            .map_err(|error| anyhow::anyhow!("buyer subscription state note_addr: {error}"))?
            .with_workchain();
        Ok(Self {
            schema: BUYER_SUBSCRIPTION_STATE_SCHEMA.to_string(),
            note_addr,
            books: Vec::new(),
        })
    }

    fn validate(&self, expected_note_addr: &str) -> Result<()> {
        if self.schema != BUYER_SUBSCRIPTION_STATE_SCHEMA {
            bail!(
                "unsupported buyer subscription state schema {}",
                self.schema
            );
        }
        let note_addr = dexdo_core::Address::parse(&self.note_addr)
            .map_err(|error| anyhow::anyhow!("buyer subscription state note_addr: {error}"))?
            .with_workchain();
        if !note_addr.eq_ignore_ascii_case(expected_note_addr) {
            bail!(
                "buyer subscription state belongs to note {}, expected {}",
                note_addr,
                expected_note_addr
            );
        }
        let mut books = std::collections::BTreeSet::new();
        let mut token_contracts = std::collections::BTreeSet::new();
        for book in &self.books {
            let order_book = dexdo_core::Address::parse(&book.order_book)
                .map_err(|error| anyhow::anyhow!("buyer subscription state order_book: {error}"))?
                .with_workchain();
            if !books.insert(order_book.clone()) {
                bail!("buyer subscription state repeats order book {order_book}");
            }
            if book.frame_model.trim().is_empty()
                || !model_hash_for(&book.frame_model).eq_ignore_ascii_case(&book.model_hash)
            {
                bail!("buyer subscription state has inconsistent model identity for {order_book}");
            }
            let mut order_ids = std::collections::BTreeSet::new();
            for subscription in &book.subscriptions {
                if !order_ids.insert(subscription.order_id) {
                    bail!(
                        "buyer subscription state repeats order #{} in {}",
                        subscription.order_id,
                        order_book
                    );
                }
                if subscription.max_price_per_tick == 0
                    || subscription.ticks == 0
                    || subscription.escrow == 0
                    || subscription.cycle_budget
                        != subscription.escrow / INFERENCE_SUBSCRIPTION_CYCLES
                {
                    bail!(
                        "buyer subscription state has invalid terms for order #{}",
                        subscription.order_id
                    );
                }
                for matched in &subscription.matches {
                    if matched.order_id != subscription.order_id {
                        bail!(
                            "buyer subscription fill order #{} is stored under order #{}",
                            matched.order_id,
                            subscription.order_id
                        );
                    }
                    let tc = dexdo_core::Address::parse(&matched.token_contract)
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "buyer subscription state TokenContract {}: {error}",
                                matched.token_contract
                            )
                        })?
                        .with_workchain();
                    if !token_contracts.insert(tc.clone()) {
                        bail!("buyer subscription state repeats TokenContract {tc}");
                    }
                }
            }
            for matched in &book.unattributed_matches {
                let tc = dexdo_core::Address::parse(&matched.token_contract)
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "buyer subscription state unattributed TokenContract {}: {error}",
                            matched.token_contract
                        )
                    })?
                    .with_workchain();
                if !token_contracts.insert(tc.clone()) {
                    bail!("buyer subscription state repeats TokenContract {tc}");
                }
            }
        }
        Ok(())
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn buyer_submit_state_dir() -> Result<std::path::PathBuf> {
    #[cfg(test)]
    let path = std::env::temp_dir().join("dexdo-buyer-submits-tests");
    #[cfg(not(test))]
    let path = directories::ProjectDirs::from("ai", "gosh", "dexdo")
        .ok_or_else(|| {
            anyhow::anyhow!("could not determine platform data directory for buyer submit journal")
        })?
        .data_dir()
        .join("buyer-submits");
    std::fs::create_dir_all(&path).map_err(|error| {
        anyhow::anyhow!(
            "create buyer submit journal directory {}: {error}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| {
                anyhow::anyhow!(
                    "set private buyer submit journal directory {}: {error}",
                    path.display()
                )
            },
        )?;
    }
    Ok(path)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
impl BuyerMoneyLock {
    fn open(note_addr: &str) -> Result<Self> {
        use sha2::{Digest, Sha256};

        let note_addr = dexdo_core::Address::parse(note_addr)
            .map_err(|error| anyhow::anyhow!("buyer note money lock address {note_addr}: {error}"))?
            .with_workchain();
        let digest = Sha256::digest(note_addr.as_bytes());
        let basename = format!("note-{}", hex::encode(digest));
        let state_dir = buyer_submit_state_dir()?;
        let path = crate::cli::note::resolve_private_file_path(
            &state_dir.join(format!("{basename}.money")),
            "buyer note money lock target",
        )?;
        let journal_path = crate::cli::note::resolve_private_file_path(
            &state_dir.join(format!("{basename}.json")),
            "buyer money journal",
        )?;
        let subscriptions_path = crate::cli::note::resolve_private_file_path(
            &state_dir.join(format!("{basename}.subscriptions.json")),
            "buyer subscription state",
        )?;
        Ok(Self {
            note_addr,
            path,
            journal_path,
            subscriptions_path,
            lock: None,
        })
    }

    fn acquire(&mut self) -> Result<()> {
        if self.lock.is_some() {
            bail!(
                "buyer note {} money lock is already acquired",
                self.note_addr
            );
        }
        self.lock = Some(acquire_pool_write_lock(&self.path).map_err(|error| {
            anyhow::anyhow!(
                "acquire buyer note {} money lock {} before submit: {error}",
                self.note_addr,
                self.path.display()
            )
        })?);
        Ok(())
    }

    fn try_acquire(&mut self) -> Result<()> {
        if self.lock.is_some() {
            bail!(
                "buyer note {} money lock is already acquired",
                self.note_addr
            );
        }
        self.lock = Some(try_acquire_pool_write_lock(&self.path).map_err(|error| {
            anyhow::anyhow!(
                "buyer note {} already has another money submission awaiting by-fact reconciliation; no BOC was sent ({}: {error})",
                self.note_addr,
                self.path.display()
            )
        })?);
        Ok(())
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn read_buyer_private_state(path: &std::path::Path, label: &str) -> Result<Option<Vec<u8>>> {
    let path = crate::cli::note::resolve_private_file_path(path, label)?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!("read {label} {}: {error}", path.display())),
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn load_buyer_money_journal(
    path: &std::path::Path,
    expected_note_addr: &str,
) -> Result<Option<BuyerMoneyJournal>> {
    let Some(bytes) = read_buyer_private_state(path, "buyer money journal")? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "buyer money journal {} is invalid JSON: {error}",
            path.display()
        )
    })?;
    let schema = value
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("buyer money journal {} has no schema", path.display()))?;
    let journal = match schema {
        BUYER_SUBMIT_JOURNAL_SCHEMA => {
            let journal: BuyerSubmitJournal = serde_json::from_value(value).map_err(|error| {
                anyhow::anyhow!(
                    "buyer submit journal {} is invalid: {error}",
                    path.display()
                )
            })?;
            journal.validate(expected_note_addr)?;
            BuyerMoneyJournal::Buy(Box::new(journal))
        }
        BUYER_SUBMIT_JOURNAL_SCHEMA_V1 => {
            let legacy: BuyerSubmitJournalV1 = serde_json::from_value(value).map_err(|error| {
                anyhow::anyhow!(
                    "legacy buyer submit journal {} is invalid: {error}",
                    path.display()
                )
            })?;
            let journal = BuyerSubmitJournal::from(legacy);
            journal.validate(expected_note_addr)?;
            BuyerMoneyJournal::Buy(Box::new(journal))
        }
        BUYER_SUBSCRIPTION_SUBMIT_SCHEMA => {
            let journal: BuyerSubscriptionSubmitJournal =
                serde_json::from_value(value).map_err(|error| {
                    anyhow::anyhow!(
                        "buyer subscription submit journal {} is invalid: {error}",
                        path.display()
                    )
                })?;
            journal.validate(expected_note_addr)?;
            BuyerMoneyJournal::Subscription(Box::new(journal))
        }
        other => bail!("unsupported buyer money journal schema {other}"),
    };
    Ok(Some(journal))
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn load_buyer_submit_journal(
    path: &std::path::Path,
    expected_note_addr: &str,
) -> Result<Option<BuyerSubmitJournal>> {
    match load_buyer_money_journal(path, expected_note_addr)? {
        None => Ok(None),
        Some(BuyerMoneyJournal::Buy(journal)) => Ok(Some(*journal)),
        Some(BuyerMoneyJournal::Subscription(journal)) => bail!(
            "buyer note {} has unresolved subscription submit {} in {}; reconcile it before a quote-bound buy",
            journal.note_addr,
            journal.submit_identity,
            journal.order_book
        ),
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn write_buyer_submit_journal(path: &std::path::Path, journal: &BuyerSubmitJournal) -> Result<()> {
    journal.validate(&journal.note_addr)?;
    let bytes = serde_json::to_vec_pretty(journal)?;
    with_pool_write_lock(path, |path| write_pool_private(path, &bytes))
        .map_err(|error| anyhow::anyhow!("write buyer submit journal {}: {error}", path.display()))
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn write_buyer_subscription_submit_journal(
    path: &std::path::Path,
    journal: &BuyerSubscriptionSubmitJournal,
) -> Result<()> {
    journal.validate(&journal.note_addr)?;
    let bytes = serde_json::to_vec_pretty(journal)?;
    with_pool_write_lock(path, |path| write_pool_private(path, &bytes)).map_err(|error| {
        anyhow::anyhow!(
            "write buyer subscription submit journal {}: {error}",
            path.display()
        )
    })
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn load_buyer_subscription_state(
    path: &std::path::Path,
    expected_note_addr: &str,
) -> Result<BuyerSubscriptionState> {
    let Some(bytes) = read_buyer_private_state(path, "buyer subscription state")? else {
        return BuyerSubscriptionState::empty(expected_note_addr);
    };
    let state: BuyerSubscriptionState = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "buyer subscription state {} is invalid JSON: {error}",
            path.display()
        )
    })?;
    state.validate(expected_note_addr)?;
    Ok(state)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn write_buyer_subscription_state(
    path: &std::path::Path,
    state: &BuyerSubscriptionState,
) -> Result<()> {
    state.validate(&state.note_addr)?;
    let bytes = serde_json::to_vec_pretty(state)?;
    with_pool_write_lock(path, |path| write_pool_private(path, &bytes)).map_err(|error| {
        anyhow::anyhow!("write buyer subscription state {}: {error}", path.display())
    })
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn clear_buyer_submit_journal(path: &std::path::Path) -> Result<()> {
    with_pool_write_lock(path, |path| match std::fs::remove_file(path) {
        Ok(()) => crate::cli::note::sync_parent_dir(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "remove reconciled buyer submit journal {}: {error}",
            path.display()
        )),
    })
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn buyer_money_lock_for_submit(
    mock_chain: bool,
    note_addr: Option<&str>,
) -> Result<Option<BuyerMoneyLock>> {
    if mock_chain {
        return Ok(None);
    }
    let note_addr = note_addr.ok_or_else(|| {
        anyhow::anyhow!("real shellnet buyer money submit requires --note-addr before locking")
    })?;
    BuyerMoneyLock::open(note_addr).map(Some)
}

#[cfg(feature = "shellnet")]
fn persist_pool_token_contract_for_note(
    pool_path: &std::path::Path,
    note_addr: &str,
    token_contract: &str,
    role: &str,
) -> Result<()> {
    with_pool_write_lock(pool_path, |pool_path| {
        let pool = load_pool_json(pool_path)?;
        let updated = crate::cli::note::pool_with_note_token_contract_recorded(
            pool,
            note_addr,
            token_contract,
            role,
            unix_now_secs(),
        )?;
        let bytes = serde_json::to_vec_pretty(&updated)?;
        write_pool_private(pool_path, &bytes)
    })
}

#[cfg(feature = "shellnet")]
fn preflight_buyer_pool_for_note(note_addr: Option<&str>) -> Result<()> {
    let Some(pool_path) = note_pool_path(None) else {
        bail!(
            "real shellnet buyer money writes require DEXDO_PN_POOL before any escrow POST so a matched \
             TokenContract can be persisted durably; set DEXDO_PN_POOL to the pool containing --note-addr"
        );
    };
    let note_addr = note_addr.ok_or_else(|| {
        anyhow::anyhow!(
            "real shellnet: --note-addr is required to preflight DEXDO_PN_POOL before buying"
        )
    })?;
    with_pool_write_lock(&pool_path, |pool_path| {
        let pool = load_pool_json(pool_path)?;
        crate::cli::note::pool_has_unique_note_entry(&pool, note_addr)?;
        let bytes = serde_json::to_vec_pretty(&pool)?;
        write_pool_private(pool_path, &bytes).map_err(|e| {
            anyhow::anyhow!(
                "preflight DEXDO_PN_POOL {} before buying: pool is not safely updateable: {e}",
                pool_path.display()
            )
        })
    })
}

#[cfg(not(feature = "shellnet"))]
fn preflight_buyer_pool_for_note(_note_addr: Option<&str>) -> Result<()> {
    Ok(())
}

#[allow(dead_code)]
fn preflight_buyer_pool_for_money_move(args: &BuyerArgs) -> Result<()> {
    if args.mock.mock_chain {
        return Ok(());
    }
    preflight_buyer_pool_for_note(args.identity.note_addr.as_deref())
}

async fn place_buy_by_model_after_pool_preflight(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    preflight_pool: bool,
    pool_note_addr: Option<&str>,
    ticks: u128,
    max_price: u128,
    escrow: u128,
) -> Result<()> {
    if preflight_pool {
        preflight_buyer_pool_for_note(pool_note_addr)?;
    }
    chain
        .place_buy_by_model(buyer.note.as_ref(), ticks, max_price, escrow)
        .await
        .map_err(|e| anyhow::Error::new(e).context("place model-only buy after pool preflight"))
}

#[cfg(feature = "shellnet")]
fn is_ambiguous_submit_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<ChainError>(),
            Some(ChainError::AmbiguousSubmit(_))
        ) || cause
            .downcast_ref::<dexdo::buyer::api::DealInitError>()
            .is_some_and(|error| error.reconciliation().is_some())
    })
}

#[cfg(feature = "shellnet")]
fn money_submit_error_clears_journal(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<dexdo_core::MoneySubmitError>()
            .is_some_and(dexdo_core::MoneySubmitError::clears_journal)
            || matches!(
                cause.downcast_ref::<ChainError>(),
                Some(ChainError::MoneySubmitPreparation(_) | ChainError::MoneySubmitRejected(_))
            )
    })
}

#[cfg(feature = "shellnet")]
fn journal_match(fill: &dexdo_core::MatchedFill, order_id: u128) -> BuyerJournalMatch {
    BuyerJournalMatch {
        token_contract: fill.token_contract.clone(),
        order_id,
        ticks: fill.ticks,
        clearing_price: fill.price_per_tick,
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn ensure_subscription_book(
    state: &mut BuyerSubscriptionState,
    order_book: &str,
    frame_model: &str,
    model_hash: &str,
    initial_cursor: &dexdo_core::MatchWatchCursor,
) -> Result<usize> {
    let order_book = dexdo_core::Address::parse(order_book)
        .map_err(|error| anyhow::anyhow!("buyer subscription order_book: {error}"))?
        .with_workchain();
    if let Some(index) = state
        .books
        .iter()
        .position(|book| book.order_book.eq_ignore_ascii_case(&order_book))
    {
        let book = &state.books[index];
        if book.frame_model != frame_model || !book.model_hash.eq_ignore_ascii_case(model_hash) {
            bail!(
                "buyer subscription state binds {} to model {}/{}, not {}/{}",
                order_book,
                book.frame_model,
                book.model_hash,
                frame_model,
                model_hash
            );
        }
        return Ok(index);
    }
    state.books.push(BuyerSubscriptionBookState {
        order_book,
        frame_model: frame_model.to_string(),
        model_hash: model_hash.to_string(),
        fill_cursor: initial_cursor.clone(),
        subscriptions: Vec::new(),
        unattributed_matches: Vec::new(),
    });
    Ok(state.books.len() - 1)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn subscription_state_contains_tc(state: &BuyerSubscriptionState, token_contract: &str) -> bool {
    state.books.iter().any(|book| {
        book.subscriptions.iter().any(|subscription| {
            subscription
                .matches
                .iter()
                .any(|matched| matched.token_contract.eq_ignore_ascii_case(token_contract))
        }) || book
            .unattributed_matches
            .iter()
            .any(|matched| matched.token_contract.eq_ignore_ascii_case(token_contract))
    })
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn route_unattributed_subscription_matches(book: &mut BuyerSubscriptionBookState) {
    let mut remaining = Vec::new();
    for matched in std::mem::take(&mut book.unattributed_matches) {
        if let Some(subscription) = book
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.order_id == matched.order_id)
        {
            if !subscription.matches.iter().any(|existing| {
                existing
                    .token_contract
                    .eq_ignore_ascii_case(&matched.token_contract)
            }) {
                subscription.matches.push(matched);
            }
        } else {
            remaining.push(matched);
        }
    }
    book.unattributed_matches = remaining;
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn persist_subscription_fills_and_cursor(
    note_addr: &str,
    state_path: &std::path::Path,
    state: &mut BuyerSubscriptionState,
    book_index: usize,
    cursor: dexdo_core::MatchWatchCursor,
    fills: Vec<(u128, dexdo_core::MatchedFill)>,
    unattributed_order_id_floor: Option<u128>,
) -> Result<()> {
    let mut fresh = Vec::new();
    for (order_id, fill) in fills {
        let matched = BuyerJournalMatch {
            token_contract: dexdo_core::Address::parse(&fill.token_contract)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "buyer subscription fill TokenContract {}: {error}",
                        fill.token_contract
                    )
                })?
                .with_workchain(),
            order_id,
            ticks: fill.ticks,
            clearing_price: fill.price_per_tick,
        };
        let known_subscription = state.books[book_index]
            .subscriptions
            .iter()
            .any(|subscription| subscription.order_id == matched.order_id);
        if !known_subscription
            && unattributed_order_id_floor.is_none_or(|floor| matched.order_id < floor)
        {
            continue;
        }
        if subscription_state_contains_tc(state, &matched.token_contract)
            || fresh.iter().any(|existing: &BuyerJournalMatch| {
                existing
                    .token_contract
                    .eq_ignore_ascii_case(&matched.token_contract)
            })
        {
            continue;
        }
        persist_buyer_token_contract_for_note_result(Some(note_addr), &matched.token_contract)
            .map_err(|error| {
                anyhow::anyhow!(
                    "persist subscription fill TokenContract {} before advancing its cursor: {error:#}",
                    matched.token_contract
                )
            })?;
        fresh.push(matched);
    }
    let book = state
        .books
        .get_mut(book_index)
        .ok_or_else(|| anyhow::anyhow!("buyer subscription state lost book index {book_index}"))?;
    for matched in fresh {
        if let Some(subscription) = book
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.order_id == matched.order_id)
        {
            subscription.matches.push(matched);
        } else {
            book.unattributed_matches.push(matched);
        }
    }
    route_unattributed_subscription_matches(book);
    book.fill_cursor = cursor;
    write_buyer_subscription_state(state_path, state)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn route_known_subscription_fills(
    note_addr: &str,
    state_path: &std::path::Path,
    order_book: &str,
    cursor: dexdo_core::MatchWatchCursor,
    fills: Vec<(u128, dexdo_core::MatchedFill)>,
) -> Result<Vec<(u128, dexdo_core::MatchedFill)>> {
    let mut state = load_buyer_subscription_state(state_path, note_addr)?;
    let Some(book_index) = state
        .books
        .iter()
        .position(|book| book.order_book.eq_ignore_ascii_case(order_book))
    else {
        return Ok(fills);
    };
    let subscription_order_ids = state.books[book_index]
        .subscriptions
        .iter()
        .map(|subscription| subscription.order_id)
        .collect::<std::collections::BTreeSet<_>>();
    let (subscription_fills, other_fills): (Vec<_>, Vec<_>) = fills
        .into_iter()
        .partition(|(order_id, _)| subscription_order_ids.contains(order_id));
    persist_subscription_fills_and_cursor(
        note_addr,
        state_path,
        &mut state,
        book_index,
        cursor,
        subscription_fills,
        None,
    )?;
    Ok(other_fills)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn record_subscription_placements(
    state: &mut BuyerSubscriptionState,
    book_index: usize,
    journal: &BuyerSubscriptionSubmitJournal,
    placements: &[InferenceSubscriptionPlacement],
) -> Result<()> {
    let book = state
        .books
        .get_mut(book_index)
        .ok_or_else(|| anyhow::anyhow!("buyer subscription state lost book index {book_index}"))?;
    for placement in placements {
        if let Some(existing) = book
            .subscriptions
            .iter()
            .find(|subscription| subscription.order_id == placement.order_id)
        {
            if existing.max_price_per_tick != placement.max_price_per_tick
                || existing.ticks != placement.ticks
                || existing.cycle_budget != placement.cycle_budget
                || existing.auto_renew != placement.auto_renew
            {
                bail!(
                    "subscription placement event for order #{} conflicts with durable state",
                    placement.order_id
                );
            }
            continue;
        }
        book.subscriptions.push(BuyerSubscriptionRecord {
            order_id: placement.order_id,
            max_price_per_tick: placement.max_price_per_tick,
            ticks: placement.ticks,
            escrow: journal.escrow,
            cycle_budget: placement.cycle_budget,
            auto_renew: placement.auto_renew,
            placed_at_unix: placement.created_at,
            active: true,
            matches: Vec::new(),
        });
    }
    book.subscriptions
        .sort_by_key(|subscription| subscription.order_id);
    route_unattributed_subscription_matches(book);
    Ok(())
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
async fn sync_subscription_state_with_backend(
    chain: &dyn ChainBackend,
    note_addr: &str,
    state_path: &std::path::Path,
    order_book: &str,
    unattributed_order_id_floor: Option<u128>,
) -> Result<BuyerSubscriptionState> {
    let mut state = load_buyer_subscription_state(state_path, note_addr)?;
    let Some(book_index) = state
        .books
        .iter()
        .position(|book| book.order_book.eq_ignore_ascii_case(order_book))
    else {
        return Ok(state);
    };
    let mut cursor = state.books[book_index].fill_cursor.clone();
    let fills = chain
        .poll_attributed_model_buys_for_order_book(order_book, &mut cursor)
        .await
        .map_err(anyhow::Error::new)?;
    persist_subscription_fills_and_cursor(
        note_addr,
        state_path,
        &mut state,
        book_index,
        cursor,
        fills,
        unattributed_order_id_floor,
    )?;
    let order_ids = state.books[book_index]
        .subscriptions
        .iter()
        .map(|subscription| subscription.order_id)
        .collect::<Vec<_>>();
    for order_id in order_ids {
        let active = chain
            .buyer_order_is_active_for_owner(order_book, order_id, note_addr)
            .await
            .map_err(anyhow::Error::new)?;
        if let Some(subscription) = state.books[book_index]
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.order_id == order_id)
        {
            subscription.active = active;
        }
    }
    write_buyer_subscription_state(state_path, &state)?;
    Ok(state)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
async fn reconcile_subscription_submit_with_backend(
    chain: &dyn ChainBackend,
    journal_path: &std::path::Path,
    state_path: &std::path::Path,
    journal: &BuyerSubscriptionSubmitJournal,
    wait: Option<std::time::Duration>,
) -> Result<Vec<InferenceSubscriptionPlacement>> {
    let started = std::time::Instant::now();
    loop {
        let placements = chain
            .subscription_placements_since(
                &journal.order_book,
                &journal.note_addr,
                journal.order_id_floor,
                journal.max_price_per_tick,
                journal.ticks,
                journal.cycle_budget,
                journal.auto_renew,
            )
            .await
            .map_err(|error| {
                anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                    "could not read placement facts for durable subscription submit {}; journal retained and no fresh BOC is safe: {error}",
                    journal.submit_identity
                )))
            })?;
        let mut state = load_buyer_subscription_state(state_path, &journal.note_addr)?;
        let book_index = ensure_subscription_book(
            &mut state,
            &journal.order_book,
            &journal.frame_model,
            &journal.model_hash,
            &journal.fill_cursor,
        )?;
        record_subscription_placements(&mut state, book_index, journal, &placements)?;
        write_buyer_subscription_state(state_path, &state)?;
        sync_subscription_state_with_backend(
            chain,
            &journal.note_addr,
            state_path,
            &journal.order_book,
            Some(journal.order_id_floor),
        )
        .await
        .map_err(|error| {
            anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "could not persist fill/activity facts for durable subscription submit {}; journal retained and no fresh BOC is safe: {error:#}",
                journal.submit_identity
            )))
        })?;
        if placements.len() == 1 {
            clear_buyer_submit_journal(journal_path)?;
            return Ok(placements);
        }
        if placements.len() > 1 {
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "durable subscription submit {} produced {} correlated placements; journal retained and no new BOC is safe",
                journal.submit_identity,
                placements.len()
            ))));
        }
        let Some(timeout) = wait else {
            return Ok(Vec::new());
        };
        if started.elapsed() >= timeout {
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "subscription submit {} has no authoritative InferenceSubscriptionPlaced event yet; journal retained and no new BOC is safe",
                journal.submit_identity
            ))));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
async fn sync_subscription_state_with_real_chain(
    chain: &dexdo_core::RealChainBackend,
    note: &dexdo_core::Address,
    state_path: &std::path::Path,
    order_book: &dexdo_core::Address,
    unattributed_order_id_floor: Option<u128>,
) -> Result<BuyerSubscriptionState> {
    let note_addr = note.with_workchain();
    let order_book_addr = order_book.with_workchain();
    let mut state = load_buyer_subscription_state(state_path, &note_addr)?;
    let Some(book_index) = state
        .books
        .iter()
        .position(|book| book.order_book.eq_ignore_ascii_case(&order_book_addr))
    else {
        return Ok(state);
    };
    let mut cursor = state.books[book_index].fill_cursor.clone();
    let fills = chain
        .poll_inference_attributed_fills(note, order_book, &mut cursor)
        .await?;
    persist_subscription_fills_and_cursor(
        &note_addr,
        state_path,
        &mut state,
        book_index,
        cursor,
        fills,
        unattributed_order_id_floor,
    )?;
    let order_ids = state.books[book_index]
        .subscriptions
        .iter()
        .map(|subscription| subscription.order_id)
        .collect::<Vec<_>>();
    for order_id in order_ids {
        let active = chain
            .inference_buyer_order_is_active_for_owner(order_book, order_id, &note_addr)
            .await?;
        if let Some(subscription) = state.books[book_index]
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.order_id == order_id)
        {
            subscription.active = active;
        }
    }
    write_buyer_subscription_state(state_path, &state)?;
    Ok(state)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
async fn reconcile_subscription_submit_with_real_chain(
    chain: &dexdo_core::RealChainBackend,
    note: &dexdo_core::Address,
    journal_path: &std::path::Path,
    state_path: &std::path::Path,
    journal: &BuyerSubscriptionSubmitJournal,
    wait: Option<std::time::Duration>,
) -> Result<Vec<InferenceSubscriptionPlacement>> {
    let order_book = dexdo_core::Address::parse(&journal.order_book)
        .map_err(|error| anyhow::anyhow!("buyer subscription journal order_book: {error}"))?;
    let started = std::time::Instant::now();
    loop {
        let placements = chain
            .inference_subscription_placements_since(
                &order_book,
                note,
                journal.order_id_floor,
                journal.max_price_per_tick,
                journal.ticks,
                journal.cycle_budget,
                journal.auto_renew,
            )
            .await
            .map_err(|error| {
                anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                    "could not read placement facts for durable subscription submit {}; journal retained and no fresh BOC is safe: {error:#}",
                    journal.submit_identity
                )))
            })?;
        let mut state = load_buyer_subscription_state(state_path, &journal.note_addr)?;
        let book_index = ensure_subscription_book(
            &mut state,
            &journal.order_book,
            &journal.frame_model,
            &journal.model_hash,
            &journal.fill_cursor,
        )?;
        record_subscription_placements(&mut state, book_index, journal, &placements)?;
        write_buyer_subscription_state(state_path, &state)?;
        sync_subscription_state_with_real_chain(
            chain,
            note,
            state_path,
            &order_book,
            Some(journal.order_id_floor),
        )
        .await
        .map_err(|error| {
            anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "could not persist fill/activity facts for durable subscription submit {}; journal retained and no fresh BOC is safe: {error:#}",
                journal.submit_identity
            )))
        })?;
        if placements.len() == 1 {
            clear_buyer_submit_journal(journal_path)?;
            return Ok(placements);
        }
        if placements.len() > 1 {
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "durable subscription submit {} produced {} correlated placements; journal retained and no new BOC is safe",
                journal.submit_identity,
                placements.len()
            ))));
        }
        let Some(timeout) = wait else {
            return Ok(Vec::new());
        };
        if started.elapsed() >= timeout {
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "subscription submit {} has no authoritative InferenceSubscriptionPlaced event yet; journal retained and no new BOC is safe",
                journal.submit_identity
            ))));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn retain_subscription_journal_after_submit_result<T>(
    journal_path: &std::path::Path,
    submit_result: &Result<T>,
) -> Result<bool> {
    let Some(error) = submit_result.as_ref().err() else {
        return Ok(true);
    };
    if !money_submit_error_clears_journal(error) {
        return Ok(true);
    }
    clear_buyer_submit_journal(journal_path)?;
    Ok(false)
}

#[cfg(feature = "shellnet")]
fn persist_buyer_token_contract_for_note_result(
    note_addr: Option<&str>,
    token_contract: &str,
) -> Result<()> {
    let pool_path = note_pool_path(None)
        .ok_or_else(|| anyhow::anyhow!("DEXDO_PN_POOL disappeared after buyer money moved"))?;
    let note_addr = note_addr
        .ok_or_else(|| anyhow::anyhow!("buyer note address disappeared after buyer money moved"))?;
    persist_pool_token_contract_for_note(&pool_path, note_addr, token_contract, "buyer")
}

#[cfg(feature = "shellnet")]
#[allow(dead_code, clippy::too_many_arguments)]
async fn place_quote_bound_buy_with_journal(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    intent: &BuyerSubmitIntent,
    expected_token_contract: Option<&str>,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
    note_addr: &str,
    cursor: &mut dexdo_core::MatchWatchCursor,
    journal_path: &std::path::Path,
) -> Result<()> {
    let order_book = chain.model_buy_order_book_identity().ok_or_else(|| {
        anyhow::anyhow!(
            "real shellnet backend did not expose its canonical model order-book identity; no BOC was sent"
        )
    })?;
    let quoted_order = selection.quoted_order.clone().ok_or_else(|| {
        anyhow::anyhow!("real shellnet submit requires the exact rendered order row")
    })?;
    let canonical_note = dexdo_core::Address::parse(note_addr)
        .map_err(|error| anyhow::anyhow!("buyer submit journal note address: {error}"))?
        .with_workchain();
    let canonical_expected_token_contract = expected_token_contract
        .map(|address| {
            dexdo_core::Address::parse(address)
                .map(|address| address.with_workchain())
                .map_err(|error| {
                    anyhow::anyhow!("buyer submit journal expected TokenContract: {error}")
                })
        })
        .transpose()?;
    let template = BuyerSubmitJournal {
        schema: BUYER_SUBMIT_JOURNAL_SCHEMA.to_string(),
        note_addr: canonical_note,
        order_book,
        intent: intent.clone(),
        expected_token_contract: canonical_expected_token_contract,
        quoted_order,
        quote: selection.quote.clone(),
        cursor: dexdo_core::MatchWatchCursor::default(),
        ticks,
        max_price_per_tick,
        escrow,
        submit_identity: String::new(),
        created_at_unix: unix_now_secs(),
        resolved_match: None,
        resolved_matches: Vec::new(),
    };
    let mut before_post = |submit_identity: String, final_cursor: dexdo_core::MatchWatchCursor| {
        let mut journal = template.clone();
        journal.submit_identity = submit_identity;
        journal.cursor = final_cursor;
        write_buyer_submit_journal(journal_path, &journal).map_err(|error| {
            ChainError::Chain(format!(
                "persist buyer submit journal before POST: {error:#}; no BOC was sent"
            ))
        })
    };
    chain
        .place_buy_by_model_with_submit_identity(
            buyer.note.as_ref(),
            selection.quoted_order.as_ref(),
            ticks,
            max_price_per_tick,
            escrow,
            cursor,
            &mut before_post,
        )
        .await
        .map_err(anyhow::Error::new)
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
fn persist_resolved_buyer_submits(
    journal_path: &std::path::Path,
    note_addr: &str,
    matches: &[BuyerJournalMatch],
) -> Result<()> {
    let first = matches
        .first()
        .ok_or_else(|| anyhow::anyhow!("cannot persist an empty buyer submit reconciliation"))?;
    let mut journal = load_buyer_submit_journal(journal_path, note_addr)?.ok_or_else(|| {
        anyhow::anyhow!(
            "buyer submit journal {} disappeared after money moved",
            journal_path.display()
        )
    })?;
    journal.resolved_match = Some(first.clone());
    journal.resolved_matches = matches.to_vec();
    write_buyer_submit_journal(journal_path, &journal)?;
    for matched in matches {
        persist_buyer_token_contract_for_note_result(Some(note_addr), &matched.token_contract)?;
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
#[allow(dead_code, clippy::too_many_arguments)]
async fn complete_buyer_submit_with_journal(
    chain: &dyn ChainBackend,
    quoted_order: Option<&OrderBookOrder>,
    ticks: u128,
    max_price_per_tick: u128,
    submit_result: Result<()>,
    note_addr: &str,
    journal_path: &std::path::Path,
) -> Result<(dexdo_core::TokenContract, MatchedTokenContractStatus)> {
    if let Err(error) = &submit_result {
        if money_submit_error_clears_journal(error) {
            clear_buyer_submit_journal(journal_path)?;
            return submit_result.map(|_| unreachable!());
        }
        if !is_ambiguous_submit_error(error) {
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "unclassified money submit outcome; journal retained and no resubmit is safe: {error:#}"
            ))));
        }
    }
    let fill = chain
        .wait_matched_token_contract(0, std::time::Duration::from_secs(DEAL_WAIT_SECS))
        .await
        .map_err(|error| {
            anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "buyer money POST may have landed but its MatchedFill is not yet provable; journal retained and no resubmit is safe: {error}"
            )))
        })?
        .ok_or_else(|| {
            anyhow::Error::new(ChainError::AmbiguousSubmit(
                "buyer money POST may have landed but returned no MatchedFill; journal retained"
                    .to_string(),
            ))
        })?;
    let expected = quoted_order.and_then(|order| {
        order
            .token_contract
            .as_ref()
            .map(|token_contract| dexdo_core::QuoteFill {
                order_id: order.order_id,
                token_contract: token_contract.clone(),
                ticks,
                price_per_tick: order.price_per_tick,
                cost_with_fee: 0,
            })
    });
    let token_contract =
        correlated_buy_token_contract(fill.clone(), expected.as_ref(), ticks, max_price_per_tick)
            .map_err(anyhow::Error::new)?;
    let resolved = journal_match(&fill, quoted_order.map_or(0, |order| order.order_id));
    persist_resolved_buyer_submits(journal_path, note_addr, &[resolved])?;
    let status = validate_reported_match_state(chain, &token_contract)
        .await
        .map_err(anyhow::Error::new)?;
    clear_buyer_submit_journal(journal_path)?;
    Ok((token_contract, status))
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
async fn reconcile_pending_buyer_submit(
    chain: &dyn ChainBackend,
    note_addr: &str,
    journal_path: &std::path::Path,
    wait: Option<std::time::Duration>,
) -> Result<Option<(dexdo_core::TokenContract, MatchedTokenContractStatus)>> {
    let Some(journal) = load_buyer_submit_journal(journal_path, note_addr)? else {
        return Ok(None);
    };
    let fills = if !journal.resolved_matches.is_empty() {
        journal
            .resolved_matches
            .iter()
            .map(|matched| dexdo_core::MatchedFill {
                token_contract: matched.token_contract.clone(),
                ticks: matched.ticks,
                price_per_tick: matched.clearing_price,
            })
            .collect::<Vec<_>>()
    } else {
        let mut cursor = journal.cursor.clone();
        let started = std::time::Instant::now();
        loop {
            let fills = chain
                .poll_matched_model_buys_for_order_book(&journal.order_book, &mut cursor)
                .await
                .map_err(|error| match error {
                    ChainError::Transport(_) => anyhow::Error::new(error),
                    _ => anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                        "could not reconcile durable buyer submit {}; journal retained: {error}",
                        journal.submit_identity
                    ))),
                })?;
            if !fills.is_empty() {
                break fills;
            }
            let Some(timeout) = wait else {
                return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                    "durable buyer submit {} is unresolved; journal retained and no BOC was sent",
                    journal.submit_identity
                ))));
            };
            if started.elapsed() >= timeout {
                return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                    "timed out reconciling durable buyer submit {}; journal retained",
                    journal.submit_identity
                ))));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    let expected = journal.quote.fills.first();
    let matching = fills
        .iter()
        .filter(|fill| {
            correlated_buy_token_contract(
                (*fill).clone(),
                expected,
                journal.ticks,
                journal.max_price_per_tick,
            )
            .is_ok()
        })
        .cloned()
        .collect::<Vec<_>>();
    if !matching.is_empty() {
        let resolved = matching
            .iter()
            .map(|fill| journal_match(fill, journal.quoted_order.order_id))
            .collect::<Vec<_>>();
        persist_resolved_buyer_submits(journal_path, note_addr, &resolved)?;
    }
    if matching.len() != 1 {
        return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
            "durable buyer submit {} produced {} correlated fills; journal retained",
            journal.submit_identity,
            matching.len()
        ))));
    }
    let fill = &matching[0];
    let status = validate_reported_match_state(chain, &fill.token_contract)
        .await
        .map_err(anyhow::Error::new)?;
    Ok(Some((fill.token_contract.clone(), status)))
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
#[allow(clippy::large_enum_variant)]
enum DurableBuyerSubmitStart {
    Submitted {
        result: Result<()>,
    },
    Reconciled {
        proof: BuyerJournalResumeProof,
        token_contract: dexdo_core::TokenContract,
        status: MatchedTokenContractStatus,
    },
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct BuyerJournalResumeProof {
    order_book: String,
    submit_identity: String,
    submit_reconciliation: dexdo::buyer::api::BuyerSubmitReconciliation,
    intent: BuyerSubmitIntent,
    expected_token_contract: Option<dexdo_core::TokenContract>,
    quoted_order: OrderBookOrder,
    quote: ExecutableQuote,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
}

#[cfg(feature = "shellnet")]
impl BuyerJournalResumeProof {
    fn from_journal(journal: &BuyerSubmitJournal) -> Result<Self> {
        Ok(Self {
            order_book: journal.order_book.clone(),
            submit_identity: journal.submit_identity.clone(),
            submit_reconciliation: buyer_submit_reconciliation(
                journal,
                dexdo::buyer::api::BuyerSubmitReconciliationState::RecoveredProven,
                dexdo::buyer::api::BuyerSubmitReconciliationOrigin::DurableJournal,
            )?,
            intent: journal.intent.clone(),
            expected_token_contract: journal.expected_token_contract.clone(),
            quoted_order: journal.quoted_order.clone(),
            quote: journal.quote.clone(),
            ticks: journal.ticks,
            max_price_per_tick: journal.max_price_per_tick,
            escrow: journal.escrow,
        })
    }
}

#[cfg(feature = "shellnet")]
#[derive(Debug)]
struct DurableBuyerSubmitReconciliationError {
    deal_init: dexdo::buyer::api::DealInitError,
    source: ChainError,
}

#[cfg(feature = "shellnet")]
impl std::fmt::Display for DurableBuyerSubmitReconciliationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deal_init.fmt(formatter)
    }
}

#[cfg(feature = "shellnet")]
impl std::error::Error for DurableBuyerSubmitReconciliationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(feature = "shellnet")]
fn durable_buyer_submit_reconciliation_error(
    error: anyhow::Error,
    journal: &BuyerSubmitJournal,
) -> anyhow::Error {
    if !is_ambiguous_submit_error(&error) {
        return error;
    }
    let message = format!("{error:#}");
    match buyer_submit_reconciliation(
        journal,
        dexdo::buyer::api::BuyerSubmitReconciliationState::DurableUnresolved,
        dexdo::buyer::api::BuyerSubmitReconciliationOrigin::DurableJournal,
    ) {
        Ok(reconciliation) => anyhow::Error::new(DurableBuyerSubmitReconciliationError {
            deal_init: dexdo::buyer::api::DealInitError::with_reconciliation(
                message.clone(),
                reconciliation,
            ),
            source: ChainError::AmbiguousSubmit(message),
        }),
        Err(reconciliation_error) => error.context(format!(
            "could not preserve durable buyer submit recovery facts: {reconciliation_error:#}"
        )),
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct BuyerQuoteSubmitOutcome {
    token_contract: dexdo_core::TokenContract,
    status: MatchedTokenContractStatus,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
    submit_reconciliation: Option<dexdo::buyer::api::BuyerSubmitReconciliation>,
}

#[cfg(feature = "shellnet")]
#[allow(clippy::too_many_arguments)]
async fn raise_pending_buyer_money_before_fresh_reads(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    note_addr: Option<&str>,
    intent: &BuyerSubmitIntent,
    expected_token_contract: Option<&str>,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
) -> Result<Option<BuyerQuoteSubmitOutcome>> {
    let mut money_lock = buyer_money_lock_for_submit(false, note_addr)?
        .ok_or_else(|| anyhow::anyhow!("real shellnet buyer recovery requires a money lock"))?;
    money_lock.try_acquire()?;
    let journal_note = money_lock.note_addr.clone();
    let journal_path = money_lock.journal_path.clone();
    let subscriptions_path = money_lock.subscriptions_path.clone();
    let Some(journal) = load_buyer_money_journal(&journal_path, &journal_note)? else {
        return Ok(None);
    };
    let pending = match journal {
        BuyerMoneyJournal::Buy(pending) => *pending,
        BuyerMoneyJournal::Subscription(pending) => {
            let placements = reconcile_subscription_submit_with_backend(
                chain,
                &journal_path,
                &subscriptions_path,
                &pending,
                None,
            )
            .await?;
            if placements.is_empty() {
                return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                    "durable subscription submit {} is unresolved in {}; no fresh buyer read or BOC is safe",
                    pending.submit_identity, pending.order_book
                ))));
            }
            let ids = placements
                .iter()
                .map(|placement| placement.order_id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "durable subscription submit {} was reconciled before fresh buyer reads as order(s) {ids}; no new buyer BOC was sent",
                pending.submit_identity
            ))));
        }
    };
    let selection = BuyerQuoteSelection {
        order_book: if pending.expected_token_contract.is_some() {
            "explicit_token_contract"
        } else {
            "model_order_book"
        },
        escrow: pending.escrow,
        quote: pending.quote.clone(),
        quoted_order: Some(pending.quoted_order.clone()),
    };
    match start_durable_buyer_submit(
        chain,
        buyer,
        intent,
        expected_token_contract,
        &selection,
        ticks,
        max_price_per_tick,
        escrow,
        &journal_note,
        &journal_path,
    )
    .await?
    {
        DurableBuyerSubmitStart::Reconciled {
            proof,
            token_contract,
            status,
        } => Ok(Some(BuyerQuoteSubmitOutcome {
            token_contract,
            status,
            ticks: proof.ticks,
            max_price_per_tick: proof.max_price_per_tick,
            escrow: proof.escrow,
            submit_reconciliation: Some(proof.submit_reconciliation),
        })),
        DurableBuyerSubmitStart::Submitted { .. } => unreachable!(
            "a durable journal loaded before fresh reads cannot start a second submission"
        ),
    }
}

#[cfg(feature = "shellnet")]
fn clear_adopted_buyer_money_journal(
    note_addr: Option<&str>,
    submit_identity: Option<&str>,
    token_contract: &str,
) -> Result<()> {
    let Some(submit_identity) = submit_identity else {
        return Ok(());
    };
    let mut money_lock = buyer_money_lock_for_submit(false, note_addr)?
        .ok_or_else(|| anyhow::anyhow!("adopted buyer journal requires a money lock"))?;
    money_lock.try_acquire()?;
    let journal = load_buyer_submit_journal(&money_lock.journal_path, &money_lock.note_addr)?
        .ok_or_else(|| anyhow::anyhow!("adopted buyer journal disappeared before service start"))?;
    let resolved = journal
        .resolved_matches
        .iter()
        .chain(journal.resolved_match.iter())
        .any(|matched| matched.token_contract.eq_ignore_ascii_case(token_contract));
    if journal.submit_identity != submit_identity || !resolved {
        bail!("adopted buyer journal changed before service start; refusing to clear it");
    }
    clear_buyer_submit_journal(&money_lock.journal_path)
}

#[cfg(not(feature = "shellnet"))]
fn clear_adopted_buyer_money_journal(
    _note_addr: Option<&str>,
    _submit_identity: Option<&str>,
    _token_contract: &str,
) -> Result<()> {
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
#[allow(clippy::too_many_arguments)]
async fn raise_pending_buyer_money_before_fresh_reads(
    _chain: &dyn ChainBackend,
    _buyer: &dexdo::buyer::Buyer,
    _note_addr: Option<&str>,
    _intent: &BuyerSubmitIntent,
    _expected_token_contract: Option<&str>,
    _ticks: u128,
    _max_price_per_tick: u128,
    _escrow: u128,
) -> Result<Option<BuyerQuoteSubmitOutcome>> {
    Ok(None)
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct BuyerSubmitProgress {
    reconciled_ambiguous_submit: bool,
    submit_reconciliation: Option<dexdo::buyer::api::BuyerSubmitReconciliation>,
}

#[cfg(feature = "shellnet")]
#[allow(dead_code, clippy::too_many_arguments)]
fn ensure_pending_buyer_submit_matches_invocation(
    pending: &BuyerSubmitJournal,
    intent: &BuyerSubmitIntent,
    expected_token_contract: Option<&str>,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
) -> Result<()> {
    let expected_token_contract = expected_token_contract
        .map(|address| dexdo_core::Address::parse(address).map(|address| address.with_workchain()))
        .transpose()
        .map_err(|error| anyhow::anyhow!("buyer restart expected TokenContract: {error}"))?;
    if pending.intent == *intent
        && pending.expected_token_contract == expected_token_contract
        && selection.quoted_order.as_ref() == Some(&pending.quoted_order)
        && selection.quote == pending.quote
        && selection.escrow == pending.escrow
        && ticks == pending.ticks
        && max_price_per_tick == pending.max_price_per_tick
        && escrow == pending.escrow
    {
        return Ok(());
    }
    Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
        "durable buyer submit {} belongs to a different logical invocation; no new BOC was sent",
        pending.submit_identity
    ))))
}

#[cfg(feature = "shellnet")]
#[allow(dead_code, clippy::too_many_arguments)]
async fn start_durable_buyer_submit(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    intent: &BuyerSubmitIntent,
    expected_token_contract: Option<&str>,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
    note_addr: &str,
    journal_path: &std::path::Path,
) -> Result<DurableBuyerSubmitStart> {
    intent.validate()?;
    if let Some(pending) = load_buyer_submit_journal(journal_path, note_addr)? {
        if pending.intent.kind == BuyerSubmitIntentKind::LegacyUnknown {
            reconcile_pending_buyer_submit(chain, note_addr, journal_path, None).await?;
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "legacy durable buyer submit {} was reconciled for facts but cannot be adopted as a fresh intent; no new BOC was sent",
                pending.submit_identity
            ))));
        }
        let current_order_book = chain.model_buy_order_book_identity().ok_or_else(|| {
            anyhow::anyhow!(
                "real shellnet backend did not expose its canonical model order-book identity; no BOC was sent"
            )
        })?;
        if !pending.order_book.eq_ignore_ascii_case(&current_order_book) {
            return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "durable buyer submit {} belongs to order book {}, current invocation is bound to {}; no chain read or new BOC was performed",
                pending.submit_identity, pending.order_book, current_order_book
            ))));
        }
        ensure_pending_buyer_submit_matches_invocation(
            &pending,
            intent,
            expected_token_contract,
            selection,
            ticks,
            max_price_per_tick,
            escrow,
        )?;
        if let Some((token_contract, status)) =
            reconcile_pending_buyer_submit(chain, note_addr, journal_path, None)
                .await
                .map_err(|error| durable_buyer_submit_reconciliation_error(error, &pending))?
        {
            return Ok(DurableBuyerSubmitStart::Reconciled {
                proof: BuyerJournalResumeProof::from_journal(&pending)?,
                token_contract,
                status,
            });
        }
    }
    preflight_buyer_pool_for_note(Some(note_addr))?;
    let mut cursor = dexdo_core::MatchWatchCursor::default();
    let result = place_quote_bound_buy_with_journal(
        chain,
        buyer,
        intent,
        expected_token_contract,
        selection,
        ticks,
        max_price_per_tick,
        escrow,
        note_addr,
        &mut cursor,
        journal_path,
    )
    .await;
    Ok(DurableBuyerSubmitStart::Submitted { result })
}

#[allow(dead_code, clippy::too_many_arguments)]
async fn execute_buyer_quote_submit<F, Fut>(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    mock_chain: bool,
    note_addr: Option<&str>,
    intent: &BuyerSubmitIntent,
    expected_token_contract: Option<&str>,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
    mut on_submit_observed: F,
) -> Result<BuyerQuoteSubmitOutcome>
where
    F: FnMut(BuyerSubmitProgress) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    #[cfg(not(feature = "shellnet"))]
    let _ = (intent, expected_token_contract);

    #[cfg(feature = "shellnet")]
    if !mock_chain {
        let mut money_lock = buyer_money_lock_for_submit(false, note_addr)?
            .ok_or_else(|| anyhow::anyhow!("real shellnet buyer submit requires a money lock"))?;
        money_lock.try_acquire()?;
        let journal_note = money_lock.note_addr.clone();
        let journal_path = money_lock.journal_path.clone();
        match start_durable_buyer_submit(
            chain,
            buyer,
            intent,
            expected_token_contract,
            selection,
            ticks,
            max_price_per_tick,
            escrow,
            &journal_note,
            &journal_path,
        )
        .await?
        {
            DurableBuyerSubmitStart::Reconciled {
                proof,
                token_contract,
                status,
            } => {
                on_submit_observed(BuyerSubmitProgress {
                    reconciled_ambiguous_submit: true,
                    submit_reconciliation: Some(proof.submit_reconciliation.clone()),
                })
                .await?;
                return Ok(BuyerQuoteSubmitOutcome {
                    token_contract,
                    status,
                    ticks: proof.ticks,
                    max_price_per_tick: proof.max_price_per_tick,
                    escrow: proof.escrow,
                    submit_reconciliation: Some(proof.submit_reconciliation),
                });
            }
            DurableBuyerSubmitStart::Submitted { result } => {
                let ambiguous_submit = result.as_ref().is_err_and(is_ambiguous_submit_error);
                let submit_reconciliation = if ambiguous_submit {
                    let journal = load_buyer_submit_journal(&journal_path, &journal_note)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "buyer submit journal {} disappeared after an ambiguous money submit",
                                journal_path.display()
                            )
                        })?;
                    Some(buyer_submit_reconciliation(
                        &journal,
                        dexdo::buyer::api::BuyerSubmitReconciliationState::FreshUnresolved,
                        dexdo::buyer::api::BuyerSubmitReconciliationOrigin::FreshSubmit,
                    )?)
                } else {
                    None
                };
                on_submit_observed(BuyerSubmitProgress {
                    reconciled_ambiguous_submit: ambiguous_submit,
                    submit_reconciliation: submit_reconciliation.clone(),
                })
                .await?;
                if intent.kind == BuyerSubmitIntentKind::OnDemand {
                    if let Some(reconciliation) = submit_reconciliation.clone() {
                        let anchor = &reconciliation.recovery_anchor;
                        return Err(anyhow::Error::new(
                            dexdo::buyer::api::DealInitError::with_reconciliation(
                                format!(
                                    "ambiguous submit {}; recovery anchor order {} / {}; durable journal retained -- resume without creating a fresh BOC",
                                    reconciliation.submit_identity,
                                    anchor.order_id,
                                    anchor.token_contract
                                ),
                                reconciliation,
                            ),
                        ));
                    }
                }
                let (token_contract, status) = complete_buyer_submit_with_journal(
                    chain,
                    selection.quoted_order.as_ref(),
                    ticks,
                    max_price_per_tick,
                    result,
                    &journal_note,
                    &journal_path,
                )
                .await?;
                return Ok(BuyerQuoteSubmitOutcome {
                    token_contract,
                    status,
                    ticks,
                    max_price_per_tick,
                    escrow,
                    submit_reconciliation,
                });
            }
        }
    }

    if let Some(token_contract) = expected_token_contract {
        let token_contract = token_contract.to_string();
        if !mock_chain {
            preflight_buyer_pool_for_note(note_addr)?;
        }
        buyer.place_buy(chain, &token_contract).await?;
        on_submit_observed(BuyerSubmitProgress {
            reconciled_ambiguous_submit: false,
            submit_reconciliation: None,
        })
        .await?;
        let status = validate_reported_match_state(chain, &token_contract).await?;
        return Ok(BuyerQuoteSubmitOutcome {
            token_contract,
            status,
            ticks,
            max_price_per_tick,
            escrow,
            submit_reconciliation: None,
        });
    }

    let since_unix = unix_now_secs() as i64;
    place_buy_by_model_after_pool_preflight(
        chain,
        buyer,
        !mock_chain,
        note_addr,
        ticks,
        max_price_per_tick,
        escrow,
    )
    .await?;
    on_submit_observed(BuyerSubmitProgress {
        reconciled_ambiguous_submit: false,
        submit_reconciliation: None,
    })
    .await?;
    let fill = chain
        .wait_matched_token_contract(since_unix, std::time::Duration::from_secs(DEAL_WAIT_SECS))
        .await?
        .ok_or_else(|| anyhow::anyhow!("buyer fill event returned no match"))?;
    let token_contract = correlated_buy_token_contract(
        fill,
        selection.quote.fills.first(),
        ticks,
        max_price_per_tick,
    )?;
    let status = validate_reported_match_state(chain, &token_contract).await?;
    Ok(BuyerQuoteSubmitOutcome {
        token_contract,
        status,
        ticks,
        max_price_per_tick,
        escrow,
        submit_reconciliation: None,
    })
}

fn record_buyer_token_contract_after_money_move(args: &BuyerArgs, token_contract: &str) {
    if let Err(e) = persist_buyer_token_contract_in_env_pool(args, token_contract) {
        tracing::warn!(
            token_contract = %token_contract,
            error = %e,
            "failed to persist buyer TokenContract recovery metadata after preflight; continuing handover/recovery"
        );
        eprintln!(
            "warning: failed to persist TokenContract recovery metadata in DEXDO_PN_POOL after buy; \
             continuing handover/recovery: {e}"
        );
    }
}

#[cfg(feature = "shellnet")]
fn persist_buyer_token_contract_in_env_pool(args: &BuyerArgs, token_contract: &str) -> Result<()> {
    if args.mock.mock_chain {
        return Ok(());
    }
    let Some(pool_path) = note_pool_path(None) else {
        return Ok(());
    };
    let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "real shellnet: --note-addr is required to persist TokenContract in DEXDO_PN_POOL"
        )
    })?;
    persist_pool_token_contract_for_note(&pool_path, note_addr, token_contract, "buyer")
}

#[cfg(feature = "shellnet")]
fn persist_buyer_token_contract_for_note(note_addr: Option<&str>, token_contract: &str) {
    let Some(note_addr) = note_addr else {
        return;
    };
    let Some(pool_path) = note_pool_path(None) else {
        return;
    };
    if let Err(e) =
        persist_pool_token_contract_for_note(&pool_path, note_addr, token_contract, "buyer")
    {
        tracing::warn!(
            token_contract = %token_contract,
            note_addr,
            pool = %pool_path.display(),
            error = %e,
            "failed to persist buyer TokenContract recovery metadata in DEXDO_PN_POOL"
        );
    }
}

#[cfg(not(feature = "shellnet"))]
fn persist_buyer_token_contract_in_env_pool(
    _args: &BuyerArgs,
    _token_contract: &str,
) -> Result<()> {
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
fn persist_buyer_token_contract_for_note(_note_addr: Option<&str>, _token_contract: &str) {}

fn reject_buyer_raw_token_contract_without_registry_book_proof(
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    frame_model: &str,
) -> Result<()> {
    if market.is_none() {
        if let Some(tc) = token_contract {
            bail!(
                "buyer model registry check failed: frame_model {frame_model} raw --token-contract {tc} has no \
                 canonical order-book proof; with buyer.check_model_registry=true, pass --market <manifest> \
                 from the canonical registry book or omit --token-contract for a model-only registry buy/resume"
            );
        }
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
async fn resolve_content_identity_model(
    contracts: &std::path::Path,
    frame_model: &str,
) -> Result<String> {
    let registry_address = default_model_registry_address(contracts).map_err(|e| {
        anyhow!(
            "read default ModelRegistry address from {} for content identity: {e}",
            contracts.display()
        )
    })?;
    let reader = ShellnetModelRegistryReader::from_manifest(contracts, &registry_address)?;
    let identity = resolve_registered_model_identity(
        &reader,
        RegistryRole::Buyer,
        &registry_address,
        frame_model,
    )
    .await?;
    Ok(identity.registry_model)
}

#[cfg(not(feature = "shellnet"))]
async fn resolve_content_identity_model(
    contracts: &std::path::Path,
    frame_model: &str,
) -> Result<String> {
    let _ = (contracts, frame_model);
    bail!("content identity ModelRegistry resolution requires a shellnet build")
}

fn buyer_content_identity_resolution_result(
    frame_model: &str,
    allow_unverified_model: bool,
    result: Result<String>,
) -> Result<Option<String>> {
    match result {
        Ok(identity_model) => Ok(Some(identity_model)),
        Err(error) if allow_unverified_model => {
            tracing::warn!(
                %frame_model,
                error = %error,
                "content identity registry resolution failed; continuing on name-only evidence because --allow-unverified-model was set"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

async fn resolve_buyer_content_identity_model(
    contracts: &std::path::Path,
    frame_model: &str,
    allow_unverified_model: bool,
) -> Result<Option<String>> {
    buyer_content_identity_resolution_result(
        frame_model,
        allow_unverified_model,
        resolve_content_identity_model(contracts, frame_model).await,
    )
}

async fn build_buyer_content_policy(
    args: &BuyerArgs,
    frame_model: &str,
) -> Result<(
    dexdo::buyer::api::ContentCheck,
    Arc<dexdo::seller::ModelsConfig>,
)> {
    let content_identity_model = if args.mock.mock_chain {
        None
    } else {
        resolve_buyer_content_identity_model(
            &args.contracts,
            frame_model,
            args.allow_unverified_model,
        )
        .await?
    };
    let content_identity_model_ref = content_identity_model.as_deref();
    let content_check_model = content_identity_model_ref.unwrap_or(frame_model);
    let models_cfg = Arc::new(dexdo::seller::ModelsConfig::load_or_empty(&args.models)?);
    let has_ref_key =
        dexdo::buyer::verify::reference_endpoint_for(content_check_model, &models_cfg)
            .map(|e| {
                std::env::var(&e.api_key_env)
                    .map(|k| !k.is_empty())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
    let content_check = dexdo::buyer::api::content_check_policy(
        frame_model,
        content_identity_model_ref,
        args.mock.mock_model,
        args.allow_unverified_model,
        has_ref_key,
        &models_cfg,
    )
    .map_err(|e| {
        anyhow!(
            "buyer content-identity preflight failed before buy: \
             missing_or_unset=allow_unverified_model_or_models_data; {e}"
        )
    })?;
    Ok((content_check, models_cfg))
}
#[derive(Debug, Clone)]
struct BuyerQuoteSelection {
    order_book: &'static str,
    escrow: u128,
    quote: ExecutableQuote,
    #[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
    quoted_order: Option<OrderBookOrder>,
}

#[cfg(feature = "shellnet")]
#[allow(dead_code, clippy::too_many_arguments)]
fn pending_buyer_submit_selection(
    journal_path: &std::path::Path,
    note_addr: &str,
    intent: &BuyerSubmitIntent,
    expected_token_contract: Option<&str>,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
) -> Result<Option<BuyerQuoteSelection>> {
    let Some(pending) = load_buyer_submit_journal(journal_path, note_addr)? else {
        return Ok(None);
    };
    if pending.intent.kind == BuyerSubmitIntentKind::LegacyUnknown {
        return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
            "legacy durable buyer submit {} cannot be adopted as a fresh intent; no quote was read and no BOC was sent",
            pending.submit_identity
        ))));
    }
    let selection = BuyerQuoteSelection {
        order_book: if pending.expected_token_contract.is_some() {
            "explicit_token_contract"
        } else {
            "model_order_book"
        },
        escrow: pending.escrow,
        quote: pending.quote.clone(),
        quoted_order: Some(pending.quoted_order.clone()),
    };
    ensure_pending_buyer_submit_matches_invocation(
        &pending,
        intent,
        expected_token_contract,
        &selection,
        ticks,
        max_price_per_tick,
        escrow,
    )?;
    Ok(Some(selection))
}

#[allow(clippy::too_many_arguments)]
async fn buyer_quote_selection_for_submit(
    chain: &dyn ChainBackend,
    mock_chain: bool,
    note_addr: Option<&str>,
    intent: &BuyerSubmitIntent,
    explicit_tc: Option<&str>,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: Option<u128>,
) -> Result<BuyerQuoteSelection> {
    #[cfg(feature = "shellnet")]
    if !mock_chain {
        intent.validate()?;
        preflight_buyer_pool_for_note(note_addr)?;
        let money_lock = buyer_money_lock_for_submit(false, note_addr)?
            .ok_or_else(|| anyhow::anyhow!("real shellnet quote requires a money lock"))?;
        let submitted_escrow =
            escrow.unwrap_or_else(|| required_escrow_for_buy(ticks, max_price_per_tick));
        if let Some(selection) = pending_buyer_submit_selection(
            &money_lock.journal_path,
            &money_lock.note_addr,
            intent,
            explicit_tc,
            ticks,
            max_price_per_tick,
            submitted_escrow,
        )? {
            return Ok(selection);
        }
    }
    #[cfg(not(feature = "shellnet"))]
    let _ = (mock_chain, note_addr, intent);

    buyer_quote_selection(chain, explicit_tc, ticks, max_price_per_tick, escrow).await
}

async fn buyer_quote_selection(
    chain: &dyn ChainBackend,
    explicit_tc: Option<&str>,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: Option<u128>,
) -> Result<BuyerQuoteSelection> {
    let mut delay = TRANSIENT_QUOTE_INITIAL_BACKOFF;
    for attempt in 0..TRANSIENT_QUOTE_ATTEMPTS {
        match buyer_quote_selection_once(chain, explicit_tc, ticks, max_price_per_tick, escrow)
            .await
        {
            Err(error)
                if attempt + 1 < TRANSIENT_QUOTE_ATTEMPTS
                    && error.chain().any(|cause| {
                        matches!(
                            cause.downcast_ref::<ChainError>(),
                            Some(ChainError::Transport(_))
                        )
                    }) =>
            {
                eprintln!(
                    "transient quote read failed on attempt {}/{}; retrying after {}ms: {error:#}",
                    attempt + 1,
                    TRANSIENT_QUOTE_ATTEMPTS,
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
            result => return result,
        }
    }
    unreachable!("quote attempt count is nonzero")
}

async fn buyer_quote_selection_once(
    chain: &dyn ChainBackend,
    explicit_tc: Option<&str>,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: Option<u128>,
) -> Result<BuyerQuoteSelection> {
    let mut explicit_submit_safe_order = None;
    if explicit_tc.is_none() {
        chain
            .assert_model_buy_matches_executable_quote(ticks, max_price_per_tick)
            .await
            .map_err(|e| anyhow::Error::new(e).context("buyer model-only quote preflight"))?;
    } else if let Some(tc) = explicit_tc {
        let tc_owned = tc.to_string();
        explicit_submit_safe_order = chain
            .submit_safe_explicit_buy_quote_order(&tc_owned, ticks, max_price_per_tick)
            .await
            .map_err(|e| anyhow::Error::new(e).context("buyer explicit-token quote preflight"))?;
        if explicit_submit_safe_order.is_none() {
            chain
                .assert_explicit_buy_matches_executable_quote(&tc_owned, ticks, max_price_per_tick)
                .await
                .map_err(|e| {
                    anyhow::Error::new(e).context("buyer explicit-token quote preflight")
                })?;
        }
    }
    let explicit_submit_safe_selected = explicit_submit_safe_order.is_some();
    let mut orders = if let Some(order) = explicit_submit_safe_order {
        vec![order]
    } else {
        mock_orders_from_offers(chain.discover_offers().await?)
    };
    let order_book = if let Some(tc) = explicit_tc {
        if !explicit_submit_safe_selected {
            orders.retain(|o| o.token_contract.as_deref() == Some(tc));
            if orders.is_empty() {
                let tc_owned = tc.to_string();
                if let Some((price_per_tick, max_ticks)) = chain.sell_offer_terms(&tc_owned).await?
                {
                    orders.push(OrderBookOrder {
                        order_id: 1,
                        owner_note: String::new(),
                        token_contract: Some(tc_owned),
                        is_buy: false,
                        price_per_tick: u128::from(price_per_tick),
                        ticks: u128::from(max_ticks),
                        escrow: 0,
                        deadline: 0,
                        flags: 0,
                        timestamp: 0,
                    });
                }
            }
        }
        "explicit_token_contract"
    } else {
        "model_order_book"
    };
    orders.retain(|o| o.price_per_tick <= max_price_per_tick);
    let quote = if chain.requires_submit_safe_single_ask_quote() {
        submit_safe_single_ask_quote(&orders, Some(ticks), None)
    } else {
        executable_quote(&orders, Some(ticks), None)
    }
    .map_err(|e| anyhow::anyhow!("buyer quote: {e}"))?;
    let quoted_order = quote.fills.first().and_then(|fill| {
        orders
            .iter()
            .find(|order| order.order_id == fill.order_id)
            .cloned()
    });
    Ok(BuyerQuoteSelection {
        order_book,
        escrow: escrow.unwrap_or_else(|| required_escrow_for_buy(ticks, max_price_per_tick)),
        quote,
        quoted_order,
    })
}

fn quote_selected_fields(
    frame_model: &str,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
) -> serde_json::Value {
    let fills = selection
        .quote
        .fills
        .iter()
        .map(|fill| {
            let cost_without_fee = fill.ticks.saturating_mul(fill.price_per_tick);
            json!({
                "order_id": machine::amount(fill.order_id),
                "token_contract": fill.token_contract,
                "ticks": machine::amount(fill.ticks),
                "price_per_tick": machine::amount(fill.price_per_tick),
                "cost_without_fee": machine::amount(cost_without_fee),
                "platform_fee": machine::amount(fill.cost_with_fee.saturating_sub(cost_without_fee)),
                "cost_with_fee": machine::amount(fill.cost_with_fee)
            })
        })
        .collect::<Vec<_>>();
    json!({
        "frame_model": frame_model,
        "model_hash": model_hash_for(frame_model),
        "order_book": selection.order_book,
        "ticks": machine::amount(ticks),
        "max_price_per_tick": machine::amount(max_price_per_tick),
        "escrow": machine::amount(selection.escrow),
        "quote_complete": selection.quote.complete,
        "filled_ticks": machine::amount(selection.quote.filled_ticks),
        "total_with_fee": machine::amount(selection.quote.total_with_fee),
        "fills": fills
    })
}

fn buyer_submit_event_fields(
    frame_model: &str,
    order_book: &str,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: u128,
    progress: BuyerSubmitProgress,
) -> serde_json::Value {
    let mut fields = json!({
        "frame_model": frame_model,
        "order_book": order_book,
        "ticks": machine::amount(ticks),
        "max_price_per_tick": machine::amount(max_price_per_tick),
        "escrow": machine::amount(escrow),
        "reconciled_ambiguous_submit": progress.reconciled_ambiguous_submit
    });
    if let Some(reconciliation) = progress.submit_reconciliation {
        fields["submit_reconciliation"] = json!(reconciliation);
    }
    fields
}

fn recovered_buyer_resume_selected_fields(
    frame_model: &str,
    outcome: &BuyerQuoteSubmitOutcome,
) -> Result<serde_json::Value> {
    let submit_reconciliation = outcome.submit_reconciliation.as_ref().ok_or_else(|| {
        anyhow::anyhow!("recovered buyer resume has no durable submit reconciliation")
    })?;
    let token_contract = dexdo_core::normalize_wallet_address(&outcome.token_contract)
        .map_err(|error| anyhow::anyhow!("recovered buyer TokenContract: {error}"))?;
    Ok(json!({
        "token_contract": token_contract,
        "role": "buyer",
        "source": "durable_journal",
        "deal_handle": deals::make_handle_id(&token_contract),
        "frame_model": frame_model,
        "submit_reconciliation": submit_reconciliation
    }))
}

fn fail_buyer_quote_selection(
    events: &mut machine::BuyerEventWriter,
    frame_model: &str,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
    context_fields: Value,
) -> Result<Option<()>> {
    let code = if selection.quote.filled_ticks == 0 {
        machine::ErrorCode::NoLiquidity
    } else if !selection.quote.complete {
        machine::ErrorCode::IncompleteQuote
    } else {
        return Ok(None);
    };
    let mut fields = quote_selected_fields(frame_model, selection, ticks, max_price_per_tick);
    merge_json_fields(&mut fields, context_fields);
    let failure_class = buyer_quote_failure_class(selection, code);
    if let serde_json::Value::Object(obj) = &mut fields {
        obj.insert("failure_class".to_string(), json!(failure_class));
        if failure_class == "no_executable_ask" {
            obj.insert("no_executable_ask".to_string(), json!(true));
        }
    }
    events.error(machine::OP_BUYER_START, code, fields)?;
    Ok(Some(()))
}

fn buyer_quote_failure_class(
    selection: &BuyerQuoteSelection,
    code: machine::ErrorCode,
) -> &'static str {
    if code == machine::ErrorCode::NoLiquidity && selection.order_book == "model_order_book" {
        "no_executable_ask"
    } else if code == machine::ErrorCode::NoLiquidity {
        "no_liquidity"
    } else {
        "incomplete_quote"
    }
}

fn merge_json_fields(base: &mut Value, extra: Value) {
    if let (Value::Object(base), Value::Object(extra)) = (base, extra) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
struct SubscriptionPlacePlan {
    ticks: u128,
    escrow: u128,
    unused_budget: u128,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
fn subscription_place_plan(args: &SubscriptionPlaceArgs) -> Result<SubscriptionPlacePlan> {
    if args.max_price_per_tick == 0 {
        bail!("subscription place requires --max-price-per-tick > 0");
    }
    match (args.ticks, args.budget) {
        (Some(_), Some(_)) | (None, None) => {
            bail!("subscription place requires exactly one of --ticks or --budget")
        }
        (Some(ticks), None) => {
            if ticks == 0 {
                bail!("subscription place requires --ticks > 0");
            }
            let escrow = required_escrow_for_buy(ticks, args.max_price_per_tick);
            check_buy_deposit_headroom(escrow, ticks, args.max_price_per_tick)
                .map_err(|e| anyhow::anyhow!("subscription escrow: {e}"))?;
            Ok(SubscriptionPlacePlan {
                ticks,
                escrow,
                unused_budget: 0,
            })
        }
        (None, Some(budget)) => {
            if budget == 0 {
                bail!("subscription place requires --budget > 0");
            }
            let unit = required_escrow_for_buy(1, args.max_price_per_tick);
            check_buy_deposit_headroom(unit, 1, args.max_price_per_tick)
                .map_err(|e| anyhow::anyhow!("subscription budget: {e}"))?;
            let ticks = budget / unit;
            if ticks == 0 {
                bail!(
                    "subscription budget {budget} buys zero whole ticks at maxPricePerTick {} \
                     (fee-inclusive unit {unit})",
                    args.max_price_per_tick
                );
            }
            let escrow = required_escrow_for_buy(ticks, args.max_price_per_tick);
            check_buy_deposit_headroom(escrow, ticks, args.max_price_per_tick)
                .map_err(|e| anyhow::anyhow!("subscription escrow: {e}"))?;
            Ok(SubscriptionPlacePlan {
                ticks,
                escrow,
                unused_budget: budget.saturating_sub(escrow),
            })
        }
    }
}

#[cfg(feature = "shellnet")]
#[allow(dead_code)]
async fn place_subscription_after_pool_preflight(
    note_addr: &str,
    submit: impl Future<Output = Result<Value>>,
) -> Result<Value> {
    preflight_buyer_pool_for_note(Some(note_addr))?;
    submit.await
}

#[cfg(feature = "shellnet")]
fn subscription_target(args: &SubscriptionArgs) -> Result<BookTarget> {
    if let Some(market) = args.market.as_deref() {
        if args.model.is_some() {
            bail!("--market and --model are mutually exclusive for subscription");
        }
        target_from_market(market)
    } else {
        let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "subscription without --market requires --note-addr to derive the order-book address"
            )
        })?;
        model_target_from_config(
            &args.models,
            args.model
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("subscription without --market requires --model"))?,
            Some(note_addr),
        )
    }
}

#[cfg(feature = "shellnet")]
fn require_subscription_note(args: &SubscriptionArgs, action: &str) -> Result<dexdo_core::Address> {
    let note_addr = args
        .identity
        .note_addr
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("subscription {action} requires --note-addr"))?;
    dexdo_core::Address::parse(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))
}

#[cfg(feature = "shellnet")]
fn require_subscription_keys(
    args: &SubscriptionArgs,
    action: &str,
    subcommand_note_key: Option<&std::path::Path>,
) -> Result<dexdo_core::KeyPair> {
    let note_key = match (args.identity.note_key.as_deref(), subcommand_note_key) {
        (Some(parent), Some(child)) if parent != child => {
            bail!(
                "subscription {action}: pass --note-key only once; parent and place values differ"
            )
        }
        (Some(parent), _) => parent,
        (_, Some(child)) => child,
        (None, None) => bail!("subscription {action} requires --note-key"),
    };
    dexdo_core::KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))
}

#[cfg(feature = "shellnet")]
fn order_owned_by_note(order: &OrderBookOrder, note_addr: &str) -> bool {
    let want = dexdo_core::normalize_wallet_address(note_addr)
        .unwrap_or_else(|_| note_addr.trim().to_string());
    dexdo_core::normalize_wallet_address(&order.owner_note)
        .map(|owner| owner == want)
        .unwrap_or_else(|_| order.owner_note.eq_ignore_ascii_case(&want))
}

#[cfg(feature = "shellnet")]
fn render_subscription_line(
    snapshot: &OrderBookSnapshot,
    order_id: u128,
    order: Option<&OrderBookOrder>,
    sub: Option<&OrderBookSubscription>,
) -> String {
    let Some(sub) = sub else {
        return format!(
            "subscription model={} order_book={} order_id={} book_active={} exists=false order_found={}",
            snapshot.frame_model,
            snapshot.order_book,
            order_id,
            snapshot.active(),
            order.is_some()
        );
    };
    let Some(order) = order else {
        let stale = sub.exists;
        return format!(
            "subscription model={} order_book={} order_id={} exists={} order_found=false stale_subscription={} period_start={} cur_cycle={} cycle_budget={} cycle_spent={} cycle_remaining={} auto_renew={}",
            snapshot.frame_model,
            snapshot.order_book,
            order_id,
            sub.exists,
            stale,
            sub.period_start,
            sub.cur_cycle,
            sub.cycle_budget,
            sub.cycle_spent,
            sub.cycle_remaining(),
            sub.auto_renew
        );
    };
    format!(
        "subscription model={} order_book={} order_id={} exists={} owner={} price_per_tick={} ticks={} escrow={} deadline={} period_start={} cur_cycle={} cycle_budget={} cycle_spent={} cycle_remaining={} auto_renew={}",
        snapshot.frame_model,
        snapshot.order_book,
        order_id,
        sub.exists,
        order.owner_note,
        order.price_per_tick,
        order.ticks,
        order.escrow,
        order.deadline,
        sub.period_start,
        sub.cur_cycle,
        sub.cycle_budget,
        sub.cycle_spent,
        sub.cycle_remaining(),
        sub.auto_renew
    )
}

#[cfg(feature = "shellnet")]
async fn raise_subscription_place_journal_before_fresh_reads(
    chain: &dexdo_core::RealChainBackend,
    args: &SubscriptionArgs,
) -> Result<()> {
    if !matches!(&args.command, SubscriptionCommand::Place(_)) {
        return Ok(());
    }
    let note_addr = args
        .identity
        .note_addr
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("subscription place requires --note-addr"))?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|error| anyhow::anyhow!("subscription place note: {error}"))?;
    let mut money_lock = BuyerMoneyLock::open(note_addr)?;
    let journal_note = money_lock.note_addr.clone();
    let journal_path = money_lock.journal_path.clone();
    let subscriptions_path = money_lock.subscriptions_path.clone();
    money_lock.try_acquire()?;
    let Some(journal) = load_buyer_money_journal(&journal_path, &journal_note)? else {
        return Ok(());
    };
    match journal {
        BuyerMoneyJournal::Buy(journal) => Err(anyhow::Error::new(
            ChainError::AmbiguousSubmit(format!(
                "subscription place refused before fresh market reads: buyer note {journal_note} has durable BUY submit {}",
                journal.submit_identity
            )),
        )),
        BuyerMoneyJournal::Subscription(journal) => {
            let placements = reconcile_subscription_submit_with_real_chain(
                chain,
                &note,
                &journal_path,
                &subscriptions_path,
                &journal,
                Some(std::time::Duration::from_secs(DEAL_WAIT_SECS)),
            )
            .await?;
            let ids = placements
                .iter()
                .map(|placement| placement.order_id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                "durable subscription submit {} was reconciled before fresh market reads as order(s) {ids}; no new subscription BOC was sent",
                journal.submit_identity
            ))))
        }
    }
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_subscription(args: SubscriptionArgs) -> Result<()> {
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    raise_subscription_place_journal_before_fresh_reads(&chain, &args).await?;
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let target = subscription_target(&args)?;
    let snapshot = direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
        let snapshot = read_book_target(&chain, &target).await?;
        if matches!(&args.command, SubscriptionCommand::Place(_)) {
            if let Some(policy) = registry_policy.as_ref() {
                enforce_model_registry_policy(
                    RegistryRole::Buyer,
                    policy,
                    &args.contracts,
                    &target.frame_model,
                    &snapshot.order_book,
                    snapshot.active(),
                    BuyerMissingBookPolicy::Reject,
                )
                .await?;
            }
        }
        Ok(snapshot)
    })
    .await?;
    if !snapshot.active() {
        bail!(
            "subscription: InferenceOrderBook {} for model {} is not active; run `dexdo deploy-market` or `dexdo provision` first",
            snapshot.order_book,
            snapshot.frame_model
        );
    }
    let ob = dexdo_core::Address::parse(&snapshot.order_book)
        .map_err(|e| anyhow::anyhow!("order_book {}: {e}", snapshot.order_book))?;

    match &args.command {
        SubscriptionCommand::Place(place) => {
            let note_addr = args
                .identity
                .note_addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("subscription place requires --note-addr"))?;
            let note = require_subscription_note(&args, "place")?;
            let keys = require_subscription_keys(&args, "place", place.note_key.as_deref())?;
            let mut money_lock =
                buyer_money_lock_for_submit(false, Some(note_addr))?.ok_or_else(|| {
                    anyhow::anyhow!("subscription place requires a per-note money lock")
                })?;
            let journal_note = money_lock.note_addr.clone();
            let journal_path = money_lock.journal_path.clone();
            let subscriptions_path = money_lock.subscriptions_path.clone();
            money_lock.try_acquire()?;
            if let Some(pending) = load_buyer_money_journal(&journal_path, &journal_note)? {
                match pending {
                    BuyerMoneyJournal::Buy(pending) => {
                        bail!(
                            "subscription place refused: buyer note {} has unresolved quote-bound submit {} for intent {:?}",
                            journal_note,
                            pending.submit_identity,
                            pending.intent.kind
                        );
                    }
                    BuyerMoneyJournal::Subscription(pending) => {
                        let placements = reconcile_subscription_submit_with_real_chain(
                            &chain,
                            &note,
                            &journal_path,
                            &subscriptions_path,
                            &pending,
                            Some(std::time::Duration::from_secs(DEAL_WAIT_SECS)),
                        )
                        .await?;
                        let ids = placements
                            .iter()
                            .map(|placement| placement.order_id.to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        return Err(anyhow::Error::new(ChainError::AmbiguousSubmit(format!(
                            "reconciled prior durable subscription submit {} as order(s) {ids}; no new subscription BOC was sent",
                            pending.submit_identity
                        ))));
                    }
                }
            }
            preflight_buyer_pool_for_note(Some(note_addr))?;
            direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.assert_note_owner_matches("subscription place", &note, &keys),
            )
            .await?;
            let plan = subscription_place_plan(place)?;
            let mut state = load_buyer_subscription_state(&subscriptions_path, &journal_note)?;
            let book_index = ensure_subscription_book(
                &mut state,
                &snapshot.order_book,
                &snapshot.frame_model,
                &target.model_hash,
                &dexdo_core::MatchWatchCursor::new(0),
            )?;
            write_buyer_subscription_state(&subscriptions_path, &state)?;
            let mut fill_cursor = state.books[book_index].fill_cursor.clone();
            let cycle_budget = plan.escrow / INFERENCE_SUBSCRIPTION_CYCLES;
            let mut before_post =
                |submit_identity: String,
                 order_id_floor: u128,
                 final_cursor: dexdo_core::MatchWatchCursor,
                 pre_post_fills: Vec<(u128, dexdo_core::MatchedFill)>| {
                    let mut state =
                        load_buyer_subscription_state(&subscriptions_path, &journal_note)?;
                    let book_index = ensure_subscription_book(
                        &mut state,
                        &snapshot.order_book,
                        &snapshot.frame_model,
                        &target.model_hash,
                        &final_cursor,
                    )?;
                    persist_subscription_fills_and_cursor(
                        &journal_note,
                        &subscriptions_path,
                        &mut state,
                        book_index,
                        final_cursor.clone(),
                        pre_post_fills,
                        None,
                    )?;
                    write_buyer_subscription_submit_journal(
                        &journal_path,
                        &BuyerSubscriptionSubmitJournal {
                            schema: BUYER_SUBSCRIPTION_SUBMIT_SCHEMA.to_string(),
                            note_addr: journal_note.clone(),
                            order_book: snapshot.order_book.clone(),
                            frame_model: snapshot.frame_model.clone(),
                            model_hash: target.model_hash.clone(),
                            max_price_per_tick: place.max_price_per_tick,
                            ticks: plan.ticks,
                            escrow: plan.escrow,
                            cycle_budget,
                            auto_renew: place.auto_renew,
                            order_id_floor,
                            fill_cursor: final_cursor,
                            submit_identity,
                            created_at_unix: unix_now_secs(),
                        },
                    )
                };
            let submit_result = chain
                .place_inference_subscription_with_identity_and_cursors(
                    &note,
                    &keys,
                    &ob,
                    &target.model_hash,
                    place.max_price_per_tick,
                    plan.ticks,
                    plan.escrow,
                    place.auto_renew,
                    &mut fill_cursor,
                    &mut before_post,
                )
                .await;
            retain_subscription_journal_after_submit_result(&journal_path, &submit_result)?;
            if let Err(error) = submit_result {
                if money_submit_error_clears_journal(&error) {
                    return Err(error);
                }
            }
            let journal = match load_buyer_money_journal(&journal_path, &journal_note)? {
                Some(BuyerMoneyJournal::Subscription(journal)) => journal,
                Some(BuyerMoneyJournal::Buy(_)) => {
                    bail!("subscription submit journal was replaced by a BUY journal")
                }
                None => bail!(
                    "subscription money POST may have landed, but its durable journal disappeared"
                ),
            };
            let placements = reconcile_subscription_submit_with_real_chain(
                &chain,
                &note,
                &journal_path,
                &subscriptions_path,
                &journal,
                Some(std::time::Duration::from_secs(DEAL_WAIT_SECS)),
            )
            .await?;
            if placements.len() != 1 {
                bail!(
                    "subscription submit {} matched {} placement events; no single order was adopted",
                    journal.submit_identity,
                    placements.len()
                );
            }
            println!(
                "subscription place confirmed model={} order_book={} owner={} order_id={} max_price_per_tick={} ticks={} escrow={} unused_budget={} auto_renew={}",
                snapshot.frame_model,
                snapshot.order_book,
                note_addr,
                placements[0].order_id,
                place.max_price_per_tick,
                plan.ticks,
                plan.escrow,
                plan.unused_budget,
                place.auto_renew
            );
        }
        SubscriptionCommand::Status { order_id } => {
            let order_id = *order_id;
            let order = snapshot.orders.iter().find(|o| o.order_id == order_id);
            if let Some(note_addr) = args.identity.note_addr.as_deref() {
                if let Some(order) = order {
                    if !order_owned_by_note(order, note_addr) {
                        bail!(
                            "subscription status: order {order_id} is owned by {}, not note {note_addr}",
                            order.owner_note
                        );
                    }
                }
            }
            let sub = direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.inference_orderbook_subscription(&ob, order_id),
            )
            .await?;
            println!(
                "{}",
                render_subscription_line(&snapshot, order_id, order, sub.as_ref())
            );
        }
        SubscriptionCommand::Cancel { order_id } => {
            let order_id = *order_id;
            let note_addr = args
                .identity
                .note_addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("subscription cancel requires --note-addr"))?;
            let order = snapshot
                .orders
                .iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "refusing to cancel subscription: order {order_id} is not resting in {}",
                        snapshot.order_book
                    )
                })?;
            if !order_owned_by_note(order, note_addr) {
                bail!(
                    "refusing to cancel subscription: order {order_id} is owned by {}, not note {note_addr}",
                    order.owner_note
                );
            }
            let sub = direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.inference_orderbook_subscription(&ob, order_id),
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "refusing to cancel subscription: could not read getSubscription({order_id})"
                )
            })?;
            if !sub.exists {
                bail!(
                    "refusing to cancel subscription: order {order_id} is not a live subscription"
                );
            }
            let note = require_subscription_note(&args, "cancel")?;
            let keys = require_subscription_keys(&args, "cancel", None)?;
            direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.assert_note_owner_matches("subscription cancel", &note, &keys),
            )
            .await?;
            chain
                .cancel_inference_order(&note, &keys, &target.model_hash, order_id)
                .await?;
            println!(
                "subscription cancel submitted model={} order_book={} order_id={} owner={} cycle={} cycle_remaining={}",
                snapshot.frame_model,
                snapshot.order_book,
                order_id,
                note_addr,
                sub.cur_cycle,
                sub.cycle_remaining()
            );
        }
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_subscription(_args: SubscriptionArgs) -> Result<()> {
    bail!("subscription unavailable: build with `--features shellnet`")
}
/// Render the per-model inference order book before a model-only buy: reads the resting asks
/// (`discover_offers`) and delegates to [`print_book_table`], marking asks executable at
/// `--max-price-per-tick` and appending the buyer's order summary.
async fn render_inference_book(
    chain: &dyn ChainBackend,
    frame_model: &str,
    max_price_per_tick: u128,
    ticks: u128,
) -> Result<()> {
    chain
        .assert_model_buy_matches_executable_quote(ticks, max_price_per_tick)
        .await
        .map_err(|e| {
            anyhow::Error::new(e).context(format!(
                "could not read a submit-safe order book for {frame_model}"
            ))
        })?;
    let offers = chain.discover_offers().await.map_err(|e| {
        anyhow::Error::new(e).context(format!(
            "could not read a trustworthy order book for {frame_model}"
        ))
    })?;
    let rows: Vec<BookRow> = offers
        .iter()
        .map(|o| BookRow {
            price_per_tick: o.price_per_tick as u128,
            max_ticks: o.max_ticks as u128,
            token_contract: o.token_contract.to_string(),
        })
        .collect();
    print_book_table(frame_model, &rows, Some(max_price_per_tick), Some(ticks));
    Ok(())
}

/// After the book is shown, ask the operator for a numeric order parameter (how many ticks / the per-tick
/// price ceiling). On a TTY it prompts -- empty input keeps the `[default]`(the CLI flag). Non-interactive
/// (piped / headless / daemon) returns the default silently, so automated runs keep working from flags.
fn prompt_u128(label: &str, default: u128) -> u128 {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return default;
    }
    loop {
        print!("{label} [{default}]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return default;
        }
        let s = line.trim();
        if s.is_empty() {
            return default;
        }
        match s.parse::<u128>() {
            Ok(v) => return v,
            Err(_) => eprintln!("enter an integer (or Enter to keep {default})"),
        }
    }
}

fn buyer_renewal_threshold_tokens() -> u64 {
    const ENV: &str = "DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS";
    std::env::var(ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or_else(|| {
            dexdo::buyer::continuity::ContinuityConfig::default().renewal_threshold_tokens
        })
}
fn elapsed_since(now_secs: u64, at: Option<u64>) -> u64 {
    at.filter(|v| *v > 0)
        .map(|v| now_secs.saturating_sub(v))
        .unwrap_or(0)
}

async fn validate_reported_match_state(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
) -> Result<MatchedTokenContractStatus, ChainError> {
    let state = chain.deal_state(token_contract).await?.ok_or_else(|| {
        ChainError::Chain(format!(
            "reported match {token_contract} has no readable TokenContract state; refusing to wait for handover"
        ))
    })?;
    check_matched_token_contract_state(
        token_contract,
        state,
        unix_now_secs(),
        MATCH_OPEN_TIMEOUT_SECS,
    )
    .map_err(ChainError::Chain)
}

fn matched_state_summary(
    token_contract: &dexdo_core::TokenContract,
    status: &MatchedTokenContractStatus,
) -> String {
    match status {
        MatchedTokenContractStatus::Opened => {
            format!("matched deal state: token_contract={token_contract} funded=true opened=true")
        }
        MatchedTokenContractStatus::FundedNeverOpened {
            funded_time,
            cleanup_after_unix,
            cleanup_ready,
            remaining_secs,
        } => format!(
            "matched deal state: token_contract={token_contract} funded=true opened=false \
             fundedTime={} cleanup_after={} cleanup_ready={} cleanup_wait_secs={}",
            funded_time
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<missing>".to_string()),
            cleanup_after_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            cleanup_ready,
            remaining_secs
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<unknown>".to_string())
        ),
    }
}

async fn handover_timeout_diagnostic(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    last_error: &str,
) -> String {
    match validate_reported_match_state(chain, token_contract).await {
        Ok(status @ MatchedTokenContractStatus::FundedNeverOpened { .. }) => format!(
            "buyer: matched TokenContract {token_contract} is funded but the seller did not open/write handover \
             within {DEAL_WAIT_SECS}s. {}. This is a funded-never-opened deal; after MATCH_OPEN_TIMEOUT use \
             `dexdo reclaim --token-contract {token_contract} --note-addr <buyer-note> --note-key <buyer-key>` \
             to streamCleanup. Last handover read error: {last_error}",
            matched_state_summary(token_contract, &status)
        ),
        Ok(status) => format!(
            "buyer: the seller did not open the stream / did not write the handover within {DEAL_WAIT_SECS}s. \
             {}. Last handover read error: {last_error}",
            matched_state_summary(token_contract, &status)
        ),
        Err(state_err) => format!(
            "buyer: the seller did not open the stream / did not write the handover within {DEAL_WAIT_SECS}s, \
             and the post-match TC state check now fails: {state_err}. Last handover read error: {last_error}"
        ),
    }
}

fn is_malformed_handover_error(error: &anyhow::Error) -> bool {
    let msg = format!("{error:#}");
    msg.contains("malformed handover") || msg.contains("handover decrypt failed")
}

async fn apply_malformed_handover_policy(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    token_contract: &dexdo_core::TokenContract,
    buyer_policy: &policy::BuyerRuntimePolicy,
    error: &anyhow::Error,
) -> Result<()> {
    match buyer_policy.malformed_handover {
        policy::MalformedHandoverAction::Reclaim => {
            let settlement = chain.seller_timeout(token_contract).await?;
            bail!(
                "buyer: malformed handover for {token_contract}: {error}\n\
                 policy_action failure_class=malformed_handover action=reclaim token_contract={token_contract} \
                 state=funded/opened result=reclaimed settlement={settlement:?}"
            );
        }
        policy::MalformedHandoverAction::Dispute => {
            let settlement = chain.dispute(token_contract, buyer.note.as_ref()).await?;
            bail!(
                "buyer: malformed handover for {token_contract}: {error}\n\
                 policy_action failure_class=malformed_handover action=dispute token_contract={token_contract} \
                 state=funded/opened/disputed result=dispute_opened settlement={settlement:?}; \
                 warning=dispute_locks_buyer_note_until_resolution"
            );
        }
        policy::MalformedHandoverAction::FailClosed => {
            bail!(
                "buyer: malformed handover for {token_contract}: {error}\n\
                 policy_action failure_class=malformed_handover action=fail_closed token_contract={token_contract} \
                 state=funded/opened result=no_recovery_submitted"
            );
        }
    }
}

async fn policy_cleanup_unopened_after_match_timeout(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    policy_action: policy::NoHandoverAfterMatchAction,
) -> Result<PolicyCleanupOutcome> {
    let status = validate_reported_match_state(chain, token_contract).await?;
    let MatchedTokenContractStatus::FundedNeverOpened {
        cleanup_ready,
        remaining_secs,
        ..
    } = status
    else {
        bail!(
            "policy_action failure_class=no_handover_after_match action={} token_contract={} \
             state={} result=not_cleanup_unopened_state",
            policy_action.as_str(),
            token_contract,
            matched_state_summary(token_contract, &status)
        );
    };
    if !cleanup_ready {
        let wait = remaining_secs
            .unwrap_or(MATCH_OPEN_TIMEOUT_SECS)
            .saturating_add(1);
        println!(
            "policy_action failure_class=no_handover_after_match action={} token_contract={} \
             state=funded/opened result=waiting_cleanup_ready wait_secs={wait}",
            policy_action.as_str(),
            token_contract
        );
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        let status = validate_reported_match_state(chain, token_contract).await?;
        match status {
            MatchedTokenContractStatus::Opened => {
                println!(
                    "policy_action failure_class=no_handover_after_match action={} token_contract={} \
                     state=funded/opened result=handover_opened_after_wait",
                    policy_action.as_str(),
                    token_contract
                );
                return Ok(PolicyCleanupOutcome::HandoverOpened);
            }
            MatchedTokenContractStatus::FundedNeverOpened {
                cleanup_ready: true,
                ..
            } => {}
            status => {
                bail!(
                    "policy_action failure_class=no_handover_after_match action={} token_contract={} \
                     state={} result=not_cleanup_unopened_state_after_wait",
                    policy_action.as_str(),
                    token_contract,
                    matched_state_summary(token_contract, &status)
                );
            }
        }
    }
    let settlement = chain.cleanup_unopened(token_contract).await?;
    println!(
        "policy_action failure_class=no_handover_after_match action={} token_contract={} \
         state=funded/opened result=cleanup_unopened_submitted settlement={settlement:?}",
        policy_action.as_str(),
        token_contract
    );
    Ok(PolicyCleanupOutcome::Cleaned(settlement))
}

enum PolicyCleanupOutcome {
    Cleaned(Settlement),
    HandoverOpened,
}

#[derive(Debug)]
enum NoHandoverPolicyOutcome {
    RetryCurrent,
    RetryNext(dexdo_core::TokenContract),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OneShotStreamPolicyOutcome {
    RetryCurrent,
    TerminalReport(String),
}

fn oneshot_stream_policy_report(
    failure_class: &str,
    action: &str,
    token_contract: &dexdo_core::TokenContract,
    submitted: bool,
) -> String {
    let (result, next_action) = match (failure_class, action, submitted) {
        ("dead_gateway", "retry_then_reclaim", true) => {
            ("reclaim_submitted", "observe_reclaim_status")
        }
        ("dead_gateway", "retry_then_reclaim", false) => (
            "reclaim_not_submitted",
            "retry_reclaim_or_run_dexdo_reclaim_after_timeout",
        ),
        ("dead_gateway", "next_seller", _) => (
            "policy_action_unsupported",
            "recover_current_deal_before_failover",
        ),
        ("dead_gateway", "fail_closed", _) => ("no_recovery_submitted", "operator_decision"),
        ("empty_stream", "reclaim", true) => ("reclaim_submitted", "observe_reclaim_status"),
        ("empty_stream", "reclaim", false) => (
            "reclaim_not_submitted",
            "retry_reclaim_or_run_dexdo_reclaim_after_timeout",
        ),
        ("empty_stream", "next_seller", _) => (
            "policy_action_unsupported",
            "recover_current_deal_before_failover",
        ),
        ("empty_stream", "fail_closed", _) => ("no_recovery_submitted", "operator_decision"),
        _ => ("policy_action_reported", "operator_decision"),
    };
    format!(
        "policy_action failure_class={failure_class} action={action} token_contract={token_contract} \
         state=funded/opened result={result} next_action={next_action}"
    )
}

async fn apply_oneshot_dead_gateway_policy(
    session: &dexdo::buyer::api::SessionSettle,
    token_contract: &dexdo_core::TokenContract,
    buyer_policy: Option<&policy::BuyerRuntimePolicy>,
    attempt: u64,
) -> OneShotStreamPolicyOutcome {
    let action = buyer_policy
        .map(|policy| policy.dead_gateway.as_str())
        .unwrap_or("retry_then_reclaim");
    if action == "retry_then_reclaim" && attempt == 1 {
        println!(
            "policy_action failure_class=dead_gateway action=retry_then_reclaim \
             token_contract={token_contract} state=funded/opened result=retrying_gateway attempt=2"
        );
        return OneShotStreamPolicyOutcome::RetryCurrent;
    }
    let submitted = session.settle_dead_gateway("dead-gateway").await;
    OneShotStreamPolicyOutcome::TerminalReport(oneshot_stream_policy_report(
        "dead_gateway",
        action,
        token_contract,
        submitted,
    ))
}

async fn apply_oneshot_empty_stream_policy(
    session: &dexdo::buyer::api::SessionSettle,
    token_contract: &dexdo_core::TokenContract,
    buyer_policy: Option<&policy::BuyerRuntimePolicy>,
) -> String {
    let action = buyer_policy
        .map(|policy| policy.empty_stream.as_str())
        .unwrap_or("reclaim");
    let submitted = session.settle_empty_stream("empty-stream").await;
    oneshot_stream_policy_report("empty_stream", action, token_contract, submitted)
}

#[allow(clippy::too_many_arguments)]
async fn apply_no_handover_after_match_policy(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    token_contract: &dexdo_core::TokenContract,
    buyer_policy: &policy::BuyerRuntimePolicy,
    next_buy: Option<(u128, u128, u128)>,
    attempt: u64,
    diagnostic: &str,
    pool_note_addr: Option<&str>,
) -> Result<NoHandoverPolicyOutcome> {
    match buyer_policy.no_handover_after_match {
        policy::NoHandoverAfterMatchAction::FailClosed => {
            bail!(
                "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=fail_closed \
                 token_contract={token_contract} state=funded/opened result=no_recovery_submitted"
            );
        }
        policy::NoHandoverAfterMatchAction::WaitThenReclaim => {
            let outcome = policy_cleanup_unopened_after_match_timeout(
                chain,
                token_contract,
                buyer_policy.no_handover_after_match,
            )
            .await?;
            let PolicyCleanupOutcome::Cleaned(settlement) = outcome else {
                return Ok(NoHandoverPolicyOutcome::RetryCurrent);
            };
            bail!(
                "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=wait_then_reclaim \
                 token_contract={token_contract} state=funded/opened result=money_reclaimed settlement={settlement:?}"
            );
        }
        policy::NoHandoverAfterMatchAction::NextSeller => {
            if attempt >= buyer_policy.max_sellers_to_try {
                bail!(
                    "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=next_seller \
                     token_contract={token_contract} state=funded/opened result=max_sellers_to_try_reached \
                     max_sellers_to_try={}",
                    buyer_policy.max_sellers_to_try
                );
            }
            let Some((ticks, max_price, escrow)) = next_buy else {
                bail!(
                    "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=next_seller \
                     token_contract={token_contract} state=funded/opened result=no_model_only_routing_context"
                );
            };
            let outcome = policy_cleanup_unopened_after_match_timeout(
                chain,
                token_contract,
                buyer_policy.no_handover_after_match,
            )
            .await?;
            if matches!(outcome, PolicyCleanupOutcome::HandoverOpened) {
                return Ok(NoHandoverPolicyOutcome::RetryCurrent);
            }
            let next_attempt = attempt.saturating_add(1);
            let projected_spend = escrow.saturating_mul(next_attempt as u128);
            if projected_spend > buyer_policy.total_spend_cap_shells as u128 {
                bail!(
                    "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=next_seller \
                     token_contract={token_contract} state=funded/opened result=total_spend_cap_reached \
                     projected_spend_shells={projected_spend} cap_shells={}",
                    buyer_policy.total_spend_cap_shells
                );
            }
            println!(
                "policy_action failure_class=no_handover_after_match action=next_seller \
                 token_contract={token_contract} state=funded/opened result=placing_next_seller \
                 attempt={next_attempt}"
            );
            preflight_buyer_pool_for_note(pool_note_addr)?;
            let next =
                submit_buyer_monitor_next_deal(chain, buyer, ticks, max_price, escrow).await?;
            println!(
                "policy_action failure_class=no_handover_after_match action=next_seller \
                 token_contract={token_contract} state=funded/opened result=next_seller_matched \
                 next_token_contract={next}"
            );
            Ok(NoHandoverPolicyOutcome::RetryNext(next))
        }
    }
}

fn buyer_monitor_current_facts(
    token_contract: dexdo_core::TokenContract,
    remaining_tokens: u64,
    session_settled: bool,
    chain_state: Option<DealChainState>,
    now_secs: u64,
) -> dexdo::buyer::continuity::DealFacts {
    use dexdo::buyer::continuity::DealFacts;

    if session_settled {
        return DealFacts::closed(token_contract);
    }
    let Some(state) = chain_state else {
        return DealFacts::handover_ready(token_contract, remaining_tokens);
    };
    if state.disputed {
        return DealFacts::closed(token_contract);
    }
    if state.opened {
        let idle_secs = if state.last_advance == 0 {
            0
        } else {
            now_secs.saturating_sub(state.last_advance)
        };
        return DealFacts::opened_idle(token_contract, idle_secs);
    }
    if state.funded && !state.probe_accepted {
        return DealFacts::funded_never_opened(
            token_contract,
            elapsed_since(now_secs, state.funded_time),
        );
    }
    DealFacts::closed(token_contract)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuyerMonitorRecoveryKind {
    CleanupUnopened,
    ReclaimOpened,
}

async fn execute_buyer_monitor_recovery(
    chain: &dyn ChainBackend,
    action: dexdo::buyer::continuity::BuyerAction,
) -> Option<(
    BuyerMonitorRecoveryKind,
    dexdo_core::TokenContract,
    Result<Settlement, ChainError>,
)> {
    use dexdo::buyer::continuity::BuyerAction;

    match action {
        BuyerAction::CleanupUnopened { token_contract } => {
            let result = chain.cleanup_unopened(&token_contract).await;
            Some((
                BuyerMonitorRecoveryKind::CleanupUnopened,
                token_contract,
                result,
            ))
        }
        BuyerAction::ReclaimOpened { token_contract } => {
            let result = chain.seller_timeout(&token_contract).await;
            Some((
                BuyerMonitorRecoveryKind::ReclaimOpened,
                token_contract,
                result,
            ))
        }
        _ => None,
    }
}

fn correlated_buy_token_contract(
    fill: dexdo_core::MatchedFill,
    expected: Option<&dexdo_core::QuoteFill>,
    ticks: u128,
    max_price_per_tick: u128,
) -> Result<dexdo_core::TokenContract, ChainError> {
    let terms_valid = fill.ticks == ticks && fill.price_per_tick <= max_price_per_tick;
    let exact_match = expected.is_none_or(|expected| {
        fill.token_contract
            .eq_ignore_ascii_case(&expected.token_contract)
            && fill.ticks == expected.ticks
            && fill.price_per_tick == expected.price_per_tick
    });
    if terms_valid && exact_match {
        return Ok(fill.token_contract);
    }
    Err(ChainError::Chain(format!(
        "buyer fill correlation failed closed: got tokenContract {} ticks {} price_per_tick {}, \
         intended tokenContract {} ticks {} price_per_tick {}; refusing wrong-fill attribution",
        fill.token_contract,
        fill.ticks,
        fill.price_per_tick,
        expected
            .map(|fill| fill.token_contract.as_str())
            .unwrap_or("<backend-preflighted>"),
        expected.map(|fill| fill.ticks).unwrap_or(ticks),
        expected
            .map(|fill| fill.price_per_tick)
            .unwrap_or(max_price_per_tick)
    )))
}

async fn submit_buyer_monitor_next_deal(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    ticks: u128,
    max_price: u128,
    escrow: u128,
) -> Result<dexdo_core::TokenContract, ChainError> {
    let since_unix = unix_now_secs() as i64;
    chain
        .place_buy_by_model(buyer.note.as_ref(), ticks, max_price, escrow)
        .await?;
    let fill = chain
        .wait_matched_token_contract(since_unix, std::time::Duration::from_secs(DEAL_WAIT_SECS))
        .await?
        .ok_or_else(|| ChainError::Chain("buyer fill event returned no match".to_string()))?;
    let token_contract = correlated_buy_token_contract(fill, None, ticks, max_price)?;
    validate_reported_match_state(chain, &token_contract).await?;
    Ok(token_contract)
}

#[allow(clippy::too_many_arguments)]
fn spawn_buyer_service_renewal(
    chain: Arc<dyn ChainBackend>,
    buyer: Arc<dexdo::buyer::Buyer>,
    deals: Arc<dexdo::buyer::api::RouteManager>,
    pool_note_addr: Option<String>,
    ticks: u128,
    max_price: u128,
    escrow: u128,
    continuity_mode: dexdo::buyer::continuity::ContinuityMode,
    content_check: dexdo::buyer::api::ContentCheck,
    models_cfg: Arc<dexdo::seller::ModelsConfig>,
    api_failure_policy: dexdo::buyer::api::BuyerApiFailurePolicy,
) {
    struct PendingRenewal {
        current: dexdo_core::TokenContract,
        next: Option<dexdo_core::TokenContract>,
        matched_at: Option<std::time::Instant>,
    }
    struct PrepareRetry {
        current: dexdo_core::TokenContract,
        retry_at: std::time::Instant,
    }

    const RENEWAL_FAILURE_BACKOFF_SECS: u64 = 30;
    const CONSUMER_DEMAND_RECENT_SECS: u64 = 30;

    tokio::spawn(async move {
        use dexdo::buyer::continuity::{
            BuyerAction, BuyerContinuity, ConsumerDemand, ContinuityConfig, DealFacts,
        };

        let mut planner = BuyerContinuity::default();
        let cfg = ContinuityConfig {
            renewal_threshold_tokens: buyer_renewal_threshold_tokens(),
            ..ContinuityConfig::default()
        };
        let mut pending: Option<PendingRenewal> = None;
        let mut prepare_retry: Option<PrepareRetry> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(active) = deals.current().await else {
                continue;
            };
            let current_tc = active.route.token_contract.clone();
            if prepare_retry
                .as_ref()
                .is_some_and(|retry| retry.current != current_tc)
            {
                prepare_retry = None;
            }
            let chain_state = match chain.deal_state(&current_tc).await {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(
                        current = %current_tc,
                        error = %e,
                        "buyer continuity: deal_state read failed; falling back to local session facts"
                    );
                    None
                }
            };
            let now_secs = unix_now_secs();
            let current_facts = buyer_monitor_current_facts(
                current_tc.clone(),
                active.remaining_tokens(),
                active.session.is_settled(),
                chain_state,
                now_secs,
            );
            let consumer_demand =
                if active.has_active_or_recent_request(now_secs, CONSUMER_DEMAND_RECENT_SECS) {
                    ConsumerDemand::ActiveOrRecent
                } else {
                    ConsumerDemand::Idle
                };

            let mut ready_next = None;
            let mut waiting_for_pending_handover = false;
            if let Some(p) = pending.as_ref().filter(|p| p.current == current_tc) {
                if let Some(next) = p.next.as_ref() {
                    if buyer.resolve_endpoint(chain.as_ref(), next).await.is_ok() {
                        ready_next = Some(DealFacts::handover_ready(
                            next.clone(),
                            consumer_api_token_budget(ticks),
                        ));
                    } else if let Some(matched_at) = p.matched_at {
                        waiting_for_pending_handover = true;
                        let age = matched_at.elapsed().as_secs();
                        let recovery = planner.tick(
                            Some(DealFacts::funded_never_opened(next.clone(), age)),
                            None,
                            cfg,
                        );
                        if let Some((_kind, token_contract, result)) =
                            execute_buyer_monitor_recovery(chain.as_ref(), recovery).await
                        {
                            match result {
                                Ok(settlement) => {
                                    tracing::warn!(
                                        current = %current_tc,
                                        next = %token_contract,
                                        settlement = ?settlement,
                                        "buyer continuity: cleaned up renewal deal that never opened"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        current = %current_tc,
                                        next = %token_contract,
                                        error = %e,
                                        "buyer continuity: cleanup_unopened failed"
                                    );
                                }
                            }
                            planner.clear_pending_next(&current_tc);
                            pending = None;
                            continue;
                        }
                    } else {
                        waiting_for_pending_handover = true;
                    }
                }
            } else if pending.is_some() {
                pending = None;
            }
            if waiting_for_pending_handover {
                continue;
            }

            let action = planner.tick_with_mode(
                Some(current_facts),
                ready_next,
                cfg,
                continuity_mode,
                consumer_demand,
            );
            match action {
                BuyerAction::ServeCurrent { .. }
                | BuyerAction::Noop { .. }
                | BuyerAction::IgnoreStale { .. } => {}
                BuyerAction::FailClosed {
                    token_contract,
                    reason,
                } => {
                    tracing::error!(
                        token_contract = %token_contract,
                        reason,
                        "buyer continuity: fail-closed planner action"
                    );
                }
                action @ (BuyerAction::CleanupUnopened { .. }
                | BuyerAction::ReclaimOpened { .. }) => {
                    if let Some((kind, token_contract, result)) =
                        execute_buyer_monitor_recovery(chain.as_ref(), action).await
                    {
                        match (kind, result) {
                            (BuyerMonitorRecoveryKind::CleanupUnopened, Ok(settlement)) => {
                                active.session.mark_recovered("continuity-cleanup");
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    settlement = ?settlement,
                                    "buyer continuity: cleaned current funded-never-opened deal"
                                );
                            }
                            (BuyerMonitorRecoveryKind::CleanupUnopened, Err(e)) => {
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    error = %e,
                                    "buyer continuity: cleanup current funded-never-opened deal failed"
                                );
                            }
                            (BuyerMonitorRecoveryKind::ReclaimOpened, Ok(settlement)) => {
                                active.session.mark_recovered("continuity-reclaim");
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    settlement = ?settlement,
                                    "buyer continuity: reclaimed current opened idle deal"
                                );
                            }
                            (BuyerMonitorRecoveryKind::ReclaimOpened, Err(e)) => {
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    error = %e,
                                    "buyer continuity: reclaim current opened idle deal failed"
                                );
                            }
                        }
                        pending = None;
                    }
                }
                BuyerAction::PlaceNextDeal { reason } => {
                    tracing::info!(reason, "buyer continuity: planner requested a fresh deal");
                    let current = current_tc.clone();
                    if let Some(retry) = prepare_retry.as_ref().filter(|retry| {
                        retry.current == current && std::time::Instant::now() < retry.retry_at
                    }) {
                        planner.clear_pending_next(&current);
                        tracing::debug!(
                            current = %current,
                            retry_after_secs = retry
                                .retry_at
                                .saturating_duration_since(std::time::Instant::now())
                                .as_secs(),
                            "buyer continuity: fresh deal prepare is in retry backoff"
                        );
                        continue;
                    }
                    if let Err(e) = preflight_buyer_pool_for_note(pool_note_addr.as_deref()) {
                        planner.clear_pending_next(&current);
                        pending = None;
                        prepare_retry = Some(PrepareRetry {
                            current: current.clone(),
                            retry_at: std::time::Instant::now()
                                + std::time::Duration::from_secs(RENEWAL_FAILURE_BACKOFF_SECS),
                        });
                        tracing::warn!(
                            current = %current,
                            retry_after_secs = RENEWAL_FAILURE_BACKOFF_SECS,
                            error = %e,
                            "buyer continuity: pool preflight failed before fresh buy submit"
                        );
                        continue;
                    }
                    match submit_buyer_monitor_next_deal(
                        chain.as_ref(),
                        buyer.as_ref(),
                        ticks,
                        max_price,
                        escrow,
                    )
                    .await
                    {
                        Ok(next) => {
                            persist_buyer_token_contract_for_note(pool_note_addr.as_deref(), &next);
                            prepare_retry = None;
                            planner.note_pending_next(current.clone(), next.clone());
                            pending = Some(PendingRenewal {
                                current,
                                next: Some(next.clone()),
                                matched_at: Some(std::time::Instant::now()),
                            });
                            tracing::info!(
                                next = %next,
                                "buyer continuity: fresh buy matched; waiting for handover"
                            );
                        }
                        Err(e) => {
                            planner.clear_pending_next(&current);
                            pending = None;
                            prepare_retry = Some(PrepareRetry {
                                current: current.clone(),
                                retry_at: std::time::Instant::now()
                                    + std::time::Duration::from_secs(RENEWAL_FAILURE_BACKOFF_SECS),
                            });
                            tracing::warn!(
                                current = %current,
                                retry_after_secs = RENEWAL_FAILURE_BACKOFF_SECS,
                                error = %e,
                                "buyer continuity: fresh buy submit/match failed"
                            );
                        }
                    }
                }
                BuyerAction::PrepareNextDeal { current } => {
                    if let Some(retry) = prepare_retry.as_ref().filter(|retry| {
                        retry.current == current && std::time::Instant::now() < retry.retry_at
                    }) {
                        planner.clear_pending_next(&current);
                        tracing::debug!(
                            current = %current,
                            retry_after_secs = retry
                                .retry_at
                                .saturating_duration_since(std::time::Instant::now())
                                .as_secs(),
                            "buyer continuity: renewal prepare is in retry backoff"
                        );
                        continue;
                    }
                    if let Err(e) = preflight_buyer_pool_for_note(pool_note_addr.as_deref()) {
                        planner.clear_pending_next(&current);
                        pending = None;
                        prepare_retry = Some(PrepareRetry {
                            current: current.clone(),
                            retry_at: std::time::Instant::now()
                                + std::time::Duration::from_secs(RENEWAL_FAILURE_BACKOFF_SECS),
                        });
                        tracing::warn!(
                            current = %current,
                            retry_after_secs = RENEWAL_FAILURE_BACKOFF_SECS,
                            error = %e,
                            "buyer continuity: pool preflight failed before renewal buy submit"
                        );
                        continue;
                    }
                    match submit_buyer_monitor_next_deal(
                        chain.as_ref(),
                        buyer.as_ref(),
                        ticks,
                        max_price,
                        escrow,
                    )
                    .await
                    {
                        Ok(next) => {
                            persist_buyer_token_contract_for_note(pool_note_addr.as_deref(), &next);
                            prepare_retry = None;
                            planner.note_pending_next(current.clone(), next.clone());
                            pending = Some(PendingRenewal {
                                current,
                                next: Some(next.clone()),
                                matched_at: Some(std::time::Instant::now()),
                            });
                            tracing::info!(
                                next = %next,
                                "buyer continuity: renewal buy matched; waiting for handover"
                            );
                        }
                        Err(e) => {
                            planner.clear_pending_next(&current);
                            pending = None;
                            prepare_retry = Some(PrepareRetry {
                                current: current.clone(),
                                retry_at: std::time::Instant::now()
                                    + std::time::Duration::from_secs(RENEWAL_FAILURE_BACKOFF_SECS),
                            });
                            tracing::warn!(
                                current = %current,
                                retry_after_secs = RENEWAL_FAILURE_BACKOFF_SECS,
                                error = %e,
                                "buyer continuity: renewal submit/match failed"
                            );
                        }
                    }
                }
                BuyerAction::SwitchToNextDeal { previous, next } => {
                    let handover = match buyer.resolve_endpoint(chain.as_ref(), &next).await {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!(
                                previous = %previous,
                                next = %next,
                                error = %e,
                                "buyer continuity: planner saw next ready but handover reread failed"
                            );
                            continue;
                        }
                    };
                    if let Err(error) = deals
                        .replace_active(
                            || {
                                let session = Arc::new(
                                    dexdo::buyer::api::SessionSettle::new_with_failure_policy(
                                        chain.clone(),
                                        next.clone(),
                                        buyer.note.clone(),
                                        api_failure_policy,
                                    ),
                                );
                                dexdo::buyer::api::ApiDeal::new(
                                    dexdo::buyer::api::Route {
                                        handover,
                                        token_contract: next.clone(),
                                        max_tokens: consumer_api_token_budget(ticks),
                                    },
                                    session,
                                    Arc::new(dexdo::buyer::api::ContentGate::new(
                                        content_check.clone(),
                                        models_cfg.clone(),
                                    )),
                                )
                            },
                            "continuity-renewal",
                        )
                        .await
                    {
                        tracing::error!(
                            previous = %previous,
                            next = %next,
                            error = %error,
                            "buyer continuity: old deal STOP failed; keeping current route and pending renewal"
                        );
                        continue;
                    }
                    pending = None;
                    prepare_retry = None;
                    tracing::info!(
                        previous = %previous,
                        next = %next,
                        "buyer continuity: switched local API to renewed handover"
                    );
                }
            }
        }
    });
}

#[derive(Clone, Copy)]
enum BuyerShellnetPreflight {
    Production,
    #[cfg(all(test, feature = "shellnet"))]
    OfflineTest,
}

impl BuyerShellnetPreflight {
    fn should_run(self) -> bool {
        match self {
            Self::Production => true,
            #[cfg(all(test, feature = "shellnet"))]
            Self::OfflineTest => false,
        }
    }
}

type BuyerShutdownSignal =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;

struct BuyerCommandRuntime {
    backend: Option<ChainAndNote>,
    shellnet_preflight: BuyerShellnetPreflight,
    shutdown: BuyerShutdownSignal,
}

impl BuyerCommandRuntime {
    fn production() -> Self {
        Self {
            backend: None,
            shellnet_preflight: BuyerShellnetPreflight::Production,
            shutdown: Box::pin(operator_shutdown_signal()),
        }
    }
}

pub(crate) async fn run_buyer(args: BuyerArgs) -> Result<()> {
    let json_mode = args.json;
    let mut machine_events = json_mode.then(machine::BuyerEventWriter::new);
    let mut machine_context = BuyerMachineErrorContext::default();
    let result = run_buyer_inner(
        args,
        &mut machine_events,
        &mut machine_context,
        BuyerCommandRuntime::production(),
    )
    .await;
    if let Err(err) = result {
        if machine::is_printed_error(&err) {
            return Err(err);
        }
        if let Some(events) = machine_events.as_mut() {
            let code = machine::classify_error(machine::OP_BUYER_START, &err);
            if code == machine::ErrorCode::NoLiquidity
                && format!("{err:#}")
                    .to_ascii_lowercase()
                    .contains("no_executable_ask")
            {
                machine_context.failure_class = Some("no_executable_ask".to_string());
            }
            events.error_with_cause(
                machine::OP_BUYER_START,
                code,
                &err,
                machine_context.fields(),
            )?;
            return Err(machine::printed_error());
        }
        return Err(err);
    }
    Ok(())
}

#[derive(Default)]
struct BuyerMachineErrorContext {
    network: Option<String>,
    frame_model: Option<String>,
    order_book: Option<String>,
    token_contract: Option<String>,
    deal_handle: Option<String>,
    failure_class: Option<String>,
    missing_or_unset: Option<String>,
}

impl BuyerMachineErrorContext {
    fn set_token_contract(&mut self, token_contract: &str) {
        self.token_contract = Some(token_contract.to_string());
        self.deal_handle = Some(deals::make_handle_id(token_contract));
    }

    fn fields(&self) -> Value {
        let mut obj = Map::new();
        if let Some(v) = &self.network {
            obj.insert("network".to_string(), json!(v));
        }
        if let Some(v) = &self.frame_model {
            obj.insert("frame_model".to_string(), json!(v));
        }
        if let Some(v) = &self.order_book {
            obj.insert("order_book".to_string(), json!(v));
        }
        if let Some(v) = &self.token_contract {
            obj.insert("token_contract".to_string(), json!(v));
        }
        if let Some(v) = &self.deal_handle {
            obj.insert("deal_handle".to_string(), json!(v));
        }
        if let Some(v) = &self.failure_class {
            obj.insert("failure_class".to_string(), json!(v));
        }
        if let Some(v) = &self.missing_or_unset {
            obj.insert("missing_or_unset".to_string(), json!(v));
        }
        Value::Object(obj)
    }
}

#[cfg(debug_assertions)]
fn buyer_machine_error_fixture_from_env() -> Option<anyhow::Error> {
    let code = std::env::var("DEXDO_BUYER_JSON_ERROR_FIXTURE").ok()?;
    if code == "CHAIN_TRANSPORT" {
        return Some(anyhow::Error::new(ChainError::Transport(
            "shellnet rpc transport fixture".to_string(),
        )));
    }
    let message = match code.as_str() {
        "NO_LIQUIDITY" => "no liquidity fixture",
        "INSUFFICIENT_BALANCE" => "insufficient balance fixture",
        "HANDOVER_TIMEOUT" => "handover within deadline fixture",
        "SETTLEMENT_FAILED" => "settlement streamStop fixture",
        "NOT_RECOVERABLE_YET" => "not recoverable yet fixture",
        "DISPUTED_DEAL" => "deal is disputed fixture",
        _ => return Some(anyhow::anyhow!("invalid fixture code {code}")),
    };
    Some(anyhow::anyhow!(message))
}

fn validate_buyer_runtime_surface_policy(
    policy: &policy::BuyerRuntimePolicy,
    local_listen: Option<std::net::SocketAddr>,
) -> Result<()> {
    if local_listen.is_some() {
        return Ok(());
    }

    let mut unsupported = Vec::new();
    if policy.dead_gateway == policy::DeadGatewayAction::NextSeller {
        unsupported.push("buyer.on.dead_gateway=next_seller");
    }
    if policy.empty_stream == policy::EmptyStreamAction::NextSeller {
        unsupported.push("buyer.on.empty_stream=next_seller");
    }
    if unsupported.is_empty() {
        return Ok(());
    }

    bail!(
        "policy_action failure_class=policy_validation action=fail_closed token_contract=<not-placed> \
         state=pre_order result=unsupported_policy_choice runtime=one-shot unsupported_choices={} \
         diagnostic=one-shot dead_gateway/empty_stream next_seller failover is not implemented; choose \
         dead_gateway=retry_then_reclaim|fail_closed and empty_stream=reclaim|fail_closed",
        unsupported.join(",")
    );
}

type SharedBuyerEvents = Option<Arc<tokio::sync::Mutex<machine::BuyerEventWriter>>>;

async fn emit_shared_buyer_event(
    events: &SharedBuyerEvents,
    event: &'static str,
    operation: &'static str,
    fields: Value,
) -> Result<()> {
    if let Some(events) = events {
        events.lock().await.event(event, operation, fields)?;
    }
    Ok(())
}

fn require_complete_buyer_quote(selection: &BuyerQuoteSelection) -> Result<()> {
    if selection.quote.filled_ticks == 0 {
        bail!("buyer quote: no liquidity");
    }
    if !selection.quote.complete {
        bail!(
            "buyer quote: incomplete quote filled_ticks={}",
            selection.quote.filled_ticks
        );
    }
    Ok(())
}

fn require_stream_buy_ticks(ticks: u128) -> Result<()> {
    const MIN_STREAM_BUY_TICKS: u128 = 2;
    if ticks >= MIN_STREAM_BUY_TICKS {
        return Ok(());
    }
    bail!(
        "invalid buy ticks: --ticks {ticks} is below the {MIN_STREAM_BUY_TICKS}-tick stream minimum; \
         TokenContract funding needs the probe tick plus at least one streaming tick. \
         Buy at least {MIN_STREAM_BUY_TICKS} ticks or wait for an ask with >= {MIN_STREAM_BUY_TICKS} ticks"
    );
}

fn is_replay_protection_error(err: &anyhow::Error) -> bool {
    if err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<ChainError>(),
            Some(ChainError::AmbiguousSubmit(_))
        )
    }) {
        return false;
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("exit code 52") || msg.contains("replay protection")
}

fn buyer_deal_init_error(err: anyhow::Error) -> dexdo::buyer::api::DealInitError {
    #[cfg(feature = "shellnet")]
    if let Some(error) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<DurableBuyerSubmitReconciliationError>())
    {
        return error.deal_init.clone();
    }
    if let Some(error) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<dexdo::buyer::api::DealInitError>())
    {
        return error.clone();
    }
    dexdo::buyer::api::DealInitError::new(format!("{err:#}"))
}

#[allow(clippy::too_many_arguments)]
async fn prepare_lazy_buyer_api_deal_with_replay_backoff(
    chain: Arc<dyn ChainBackend>,
    buyer: Arc<dexdo::buyer::Buyer>,
    args: Arc<BuyerArgs>,
    explicit_tc: Option<String>,
    frame_model: String,
    content_check: dexdo::buyer::api::ContentCheck,
    models_cfg: Arc<dexdo::seller::ModelsConfig>,
    buyer_policy: Option<policy::BuyerRuntimePolicy>,
    api_failure_policy: dexdo::buyer::api::BuyerApiFailurePolicy,
    events: SharedBuyerEvents,
    raised_money: Option<BuyerQuoteSubmitOutcome>,
    shellnet_preflight: BuyerShellnetPreflight,
) -> std::result::Result<dexdo::buyer::api::ApiDeal, dexdo::buyer::api::DealInitError> {
    const MAX_ATTEMPTS: u64 = 3;
    let mut attempt = 1u64;
    loop {
        let result = prepare_lazy_buyer_api_deal_once(
            chain.clone(),
            buyer.clone(),
            args.clone(),
            explicit_tc.clone(),
            frame_model.clone(),
            content_check.clone(),
            models_cfg.clone(),
            buyer_policy.clone(),
            api_failure_policy,
            events.clone(),
            raised_money.clone(),
            shellnet_preflight,
        )
        .await;
        match result {
            Ok(deal) => return Ok(deal),
            Err(err) if is_replay_protection_error(&err) && attempt < MAX_ATTEMPTS => {
                let backoff_secs = attempt.saturating_mul(2);
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                attempt = attempt.saturating_add(1);
            }
            Err(err) if is_replay_protection_error(&err) => {
                return Err(dexdo::buyer::api::DealInitError::new(format!(
                    "on-demand purchase failed after replay-protection retries: {err:#}"
                )));
            }
            Err(err) => return Err(buyer_deal_init_error(err)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_lazy_buyer_api_deal_once(
    chain: Arc<dyn ChainBackend>,
    buyer: Arc<dexdo::buyer::Buyer>,
    args: Arc<BuyerArgs>,
    explicit_tc: Option<String>,
    frame_model: String,
    content_check: dexdo::buyer::api::ContentCheck,
    models_cfg: Arc<dexdo::seller::ModelsConfig>,
    buyer_policy: Option<policy::BuyerRuntimePolicy>,
    api_failure_policy: dexdo::buyer::api::BuyerApiFailurePolicy,
    events: SharedBuyerEvents,
    raised_money: Option<BuyerQuoteSubmitOutcome>,
    shellnet_preflight: BuyerShellnetPreflight,
) -> Result<dexdo::buyer::api::ApiDeal> {
    let raised_money = if args.mock.mock_chain {
        raised_money
    } else {
        let escrow = args
            .escrow
            .unwrap_or_else(|| required_escrow_for_buy(args.ticks, args.max_price_per_tick));
        raise_pending_buyer_money_before_fresh_reads(
            chain.as_ref(),
            buyer.as_ref(),
            args.identity.note_addr.as_deref(),
            &BuyerSubmitIntent::on_demand(),
            explicit_tc.as_deref(),
            args.ticks,
            args.max_price_per_tick,
            escrow,
        )
        .await?
        .or(raised_money)
    };
    require_stream_buy_ticks(args.ticks)?;
    if !args.mock.mock_chain && shellnet_preflight.should_run() {
        shellnet_doctor_preflight(&args.contracts, args.market.as_deref()).await?;
        if let Some(policy) = load_enabled_model_registry_policy(
            RegistryRole::Buyer,
            &args.registry,
            &args.contracts,
        )? {
            reject_buyer_raw_token_contract_without_registry_book_proof(
                args.market.as_deref(),
                args.token_contract.as_deref(),
                &frame_model,
            )?;
            let expected_order_book = if let Some(market) = args.market.as_deref() {
                load_market(market)?.inference_order_book
            } else {
                let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: --note-addr is required to derive the buyer order book"
                    )
                })?;
                expected_order_book_for_note(&args.contracts, note_addr, &frame_model).await?
            };
            let order_book_active =
                order_book_active_from_contracts(&args.contracts, &expected_order_book).await?;
            enforce_model_registry_policy(
                RegistryRole::Buyer,
                &policy,
                &args.contracts,
                &frame_model,
                &expected_order_book,
                order_book_active,
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
    }

    let adopted_submit_identity = raised_money
        .as_ref()
        .and_then(|outcome| outcome.submit_reconciliation.as_ref())
        .map(|reconciliation| reconciliation.submit_identity.clone());
    let mut service_renewal: Option<(u128, u128, u128)> = None;
    let (mut token_contract, buy_ticks) = if let Some(outcome) = raised_money {
        if args.resume {
            emit_shared_buyer_event(
                &events,
                "resume_selected",
                machine::OP_BUYER_START,
                recovered_buyer_resume_selected_fields(&frame_model, &outcome)?,
            )
            .await?;
        } else {
            emit_shared_buyer_event(
                &events,
                "buy_submitted",
                machine::OP_BUYER_START,
                buyer_submit_event_fields(
                    &frame_model,
                    if explicit_tc.is_some() {
                        "explicit_token_contract"
                    } else {
                        "model_order_book"
                    },
                    outcome.ticks,
                    outcome.max_price_per_tick,
                    outcome.escrow,
                    BuyerSubmitProgress {
                        reconciled_ambiguous_submit: true,
                        submit_reconciliation: outcome.submit_reconciliation.clone(),
                    },
                ),
            )
            .await?;
        }
        (outcome.token_contract, outcome.ticks)
    } else {
        match explicit_tc.clone() {
            Some(tc) => {
                if args.resume {
                    emit_shared_buyer_event(
                        &events,
                        "resume_selected",
                        machine::OP_BUYER_START,
                        json!({
                            "token_contract": tc.clone(),
                            "role": "buyer",
                            "source": "token_contract",
                            "deal_handle": deals::make_handle_id(&tc),
                            "frame_model": frame_model.clone()
                        }),
                    )
                    .await?;
                } else {
                    let selection = buyer_quote_selection_for_submit(
                        chain.as_ref(),
                        args.mock.mock_chain,
                        args.identity.note_addr.as_deref(),
                        &BuyerSubmitIntent::on_demand(),
                        Some(&tc),
                        args.ticks,
                        args.max_price_per_tick,
                        args.escrow,
                    )
                    .await?;
                    require_complete_buyer_quote(&selection)?;
                    emit_shared_buyer_event(
                        &events,
                        "quote_selected",
                        machine::OP_BUYER_START,
                        quote_selected_fields(
                            &frame_model,
                            &selection,
                            args.ticks,
                            args.max_price_per_tick,
                        ),
                    )
                    .await?;
                    require_stream_buy_ticks(args.ticks)?;
                    let submit_frame_model = frame_model.clone();
                    let outcome = execute_buyer_quote_submit(
                        chain.as_ref(),
                        buyer.as_ref(),
                        args.mock.mock_chain,
                        args.identity.note_addr.as_deref(),
                        &BuyerSubmitIntent::on_demand(),
                        Some(&tc),
                        &selection,
                        args.ticks,
                        args.max_price_per_tick,
                        selection.escrow,
                        |progress| {
                            emit_shared_buyer_event(
                                &events,
                                "buy_submitted",
                                machine::OP_BUYER_START,
                                buyer_submit_event_fields(
                                    &submit_frame_model,
                                    "explicit_token_contract",
                                    args.ticks,
                                    args.max_price_per_tick,
                                    selection.escrow,
                                    progress,
                                ),
                            )
                        },
                    )
                    .await?;
                    emit_shared_buyer_event(
                        &events,
                        "matched",
                        machine::OP_BUYER_START,
                        json!({
                            "frame_model": frame_model.clone(),
                            "order_book": "explicit_token_contract",
                            "token_contract": outcome.token_contract.clone()
                        }),
                    )
                    .await?;
                    if !outcome.token_contract.eq_ignore_ascii_case(&tc) {
                        bail!(
                            "explicit on-demand submit matched {}, expected {}",
                            outcome.token_contract,
                            tc
                        );
                    }
                }
                (tc, args.ticks)
            }
            None if args.resume => {
                let since_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
                    - RESUME_LOOKBACK_SECS;
                let tc = chain
                    .wait_matched_token_contract(
                        since_unix,
                        std::time::Duration::from_secs(DEAL_WAIT_SECS),
                    )
                    .await?
                    .ok_or_else(|| {
                        ChainError::Chain("buyer fill event returned no match".to_string())
                    })?
                    .token_contract;
                chain.assert_model_only_resume_target(&tc).await?;
                emit_shared_buyer_event(
                    &events,
                    "resume_selected",
                    machine::OP_BUYER_START,
                    json!({
                        "token_contract": tc.clone(),
                        "role": "buyer",
                        "source": "note_fill_event",
                        "deal_handle": deals::make_handle_id(&tc),
                        "frame_model": frame_model.clone()
                    }),
                )
                .await?;
                (tc, args.ticks)
            }
            None => {
                let ticks = args.ticks;
                let max_price = args.max_price_per_tick;
                let escrow = args
                    .escrow
                    .unwrap_or_else(|| dexdo_core::required_escrow_for_buy(ticks, max_price));
                service_renewal = Some((ticks, max_price, escrow));
                let selection = buyer_quote_selection_for_submit(
                    chain.as_ref(),
                    args.mock.mock_chain,
                    args.identity.note_addr.as_deref(),
                    &BuyerSubmitIntent::on_demand(),
                    None,
                    ticks,
                    max_price,
                    Some(escrow),
                )
                .await?;
                require_complete_buyer_quote(&selection)?;
                emit_shared_buyer_event(
                    &events,
                    "quote_selected",
                    machine::OP_BUYER_START,
                    quote_selected_fields(&frame_model, &selection, ticks, max_price),
                )
                .await?;
                require_stream_buy_ticks(ticks)?;
                let submit_frame_model = frame_model.clone();
                let outcome = execute_buyer_quote_submit(
                    chain.as_ref(),
                    buyer.as_ref(),
                    args.mock.mock_chain,
                    args.identity.note_addr.as_deref(),
                    &BuyerSubmitIntent::on_demand(),
                    None,
                    &selection,
                    ticks,
                    max_price,
                    escrow,
                    |progress| {
                        emit_shared_buyer_event(
                            &events,
                            "buy_submitted",
                            machine::OP_BUYER_START,
                            buyer_submit_event_fields(
                                &submit_frame_model,
                                "model_order_book",
                                ticks,
                                max_price,
                                escrow,
                                progress,
                            ),
                        )
                    },
                )
                .await?;
                emit_shared_buyer_event(
                    &events,
                    "matched",
                    machine::OP_BUYER_START,
                    json!({
                        "frame_model": frame_model.clone(),
                        "order_book": "model_order_book",
                        "token_contract": outcome.token_contract.clone()
                    }),
                )
                .await?;
                (outcome.token_contract, outcome.ticks)
            }
        }
    };

    record_buyer_token_contract_after_money_move(args.as_ref(), &token_contract);

    let mut handover_attempt = 1u64;
    let handover = 'handover: loop {
        let hv_deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(DEAL_WAIT_SECS);
        let hv_deadline_unix = machine::now_unix()?.saturating_add(DEAL_WAIT_SECS);
        emit_shared_buyer_event(
            &events,
            "handover_waiting",
            machine::OP_BUYER_START,
            json!({
                "token_contract": token_contract.clone(),
                "deadline_unix": hv_deadline_unix,
                "poll_interval_ms": 500
            }),
        )
        .await?;
        loop {
            match buyer
                .resolve_endpoint(chain.as_ref(), &token_contract)
                .await
            {
                Ok(h) => break 'handover h,
                Err(e) => {
                    if is_malformed_handover_error(&e) {
                        if let Some(policy) = buyer_policy.as_ref() {
                            apply_malformed_handover_policy(
                                chain.as_ref(),
                                buyer.as_ref(),
                                &token_contract,
                                policy,
                                &e,
                            )
                            .await?;
                        }
                        return Err(
                            e.context(format!("buyer: malformed handover for {token_contract}"))
                        );
                    }
                    if std::time::Instant::now() >= hv_deadline {
                        let last_error = format!("{e:#}");
                        let diagnostic = handover_timeout_diagnostic(
                            chain.as_ref(),
                            &token_contract,
                            &last_error,
                        )
                        .await;
                        if let Some(policy) = buyer_policy.as_ref() {
                            let policy_outcome = apply_no_handover_after_match_policy(
                                chain.as_ref(),
                                buyer.as_ref(),
                                &token_contract,
                                policy,
                                service_renewal,
                                handover_attempt,
                                &diagnostic,
                                args.identity.note_addr.as_deref(),
                            )
                            .await;
                            match policy_outcome {
                                Err(policy_err) => {
                                    return Err(e.context(format!("{policy_err:#}")));
                                }
                                Ok(NoHandoverPolicyOutcome::RetryCurrent) => {
                                    continue 'handover;
                                }
                                Ok(NoHandoverPolicyOutcome::RetryNext(next)) => {
                                    token_contract = next;
                                    record_buyer_token_contract_after_money_move(
                                        args.as_ref(),
                                        &token_contract,
                                    );
                                    handover_attempt = handover_attempt.saturating_add(1);
                                    continue 'handover;
                                }
                            }
                        }
                        return Err(e.context(diagnostic));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    };

    let deal_handle = deals::make_handle_id(&token_contract);
    emit_shared_buyer_event(
        &events,
        "handover_received",
        machine::OP_BUYER_START,
        json!({
            "token_contract": token_contract.clone(),
            "deal_handle": deal_handle.clone(),
            "handover_anchor": {"kind":"token_contract_state","value":"handover_present"}
        }),
    )
    .await?;

    let should_save_handle = !args.mock.mock_chain || events.is_some();
    if should_save_handle {
        let mock_note_addr;
        let note_addr = if args.mock.mock_chain {
            mock_note_addr = format!("mock:{}", note_pubkey_id(&buyer.note.pubkey()));
            mock_note_addr.as_str()
        } else {
            args.identity.note_addr.as_deref().ok_or_else(|| {
                anyhow::anyhow!("real shellnet: --note-addr is required to save the deal handle")
            })?
        };
        let endpoint = args.local_listen.map(|addr| deals::DealEndpointInfo {
            kind: "local-listen".to_string(),
            value: addr.to_string(),
        });
        let input = RuntimeDealHandleInput {
            role: deals::DealHandleRole::Buyer,
            deals_dir: args.deals_dir.as_deref(),
            token_contract: &token_contract,
            note_addr,
            frame_model: &frame_model,
            market_path: args.market.as_deref(),
            contracts: &args.contracts,
            endpoint,
        };
        if args.mock.mock_chain {
            save_mock_runtime_deal_handle(input)?;
        } else {
            save_runtime_deal_handle(input, events.is_none())?;
        }
    }

    clear_adopted_buyer_money_journal(
        args.identity.note_addr.as_deref(),
        adopted_submit_identity.as_deref(),
        &token_contract,
    )?;
    let session = Arc::new(dexdo::buyer::api::SessionSettle::new_with_failure_policy(
        chain,
        token_contract.clone(),
        buyer.note.clone(),
        api_failure_policy,
    ));
    Ok(dexdo::buyer::api::ApiDeal::new(
        dexdo::buyer::api::Route {
            handover,
            token_contract,
            max_tokens: consumer_api_token_budget(buy_ticks),
        },
        session,
        Arc::new(dexdo::buyer::api::ContentGate::new(
            content_check,
            models_cfg,
        )),
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_on_demand_buyer_api_state(
    chain: Arc<dyn ChainBackend>,
    buyer: Arc<dexdo::buyer::Buyer>,
    args: Arc<BuyerArgs>,
    explicit_tc: Option<String>,
    frame_model: String,
    content_check: dexdo::buyer::api::ContentCheck,
    models_cfg: Arc<dexdo::seller::ModelsConfig>,
    buyer_policy: Option<policy::BuyerRuntimePolicy>,
    api_failure_policy: dexdo::buyer::api::BuyerApiFailurePolicy,
    events: SharedBuyerEvents,
    raised_money: Option<BuyerQuoteSubmitOutcome>,
    shellnet_preflight: BuyerShellnetPreflight,
) -> dexdo::buyer::api::ApiState {
    let initializer = {
        let chain = chain.clone();
        let buyer = buyer.clone();
        let args = args.clone();
        let explicit_tc = explicit_tc.clone();
        let frame_model = frame_model.clone();
        let content_check = content_check.clone();
        let models_cfg = models_cfg.clone();
        let buyer_policy = buyer_policy.clone();
        let events = events.clone();
        let raised_money = raised_money.clone();
        Arc::new(move || {
            let chain = chain.clone();
            let buyer = buyer.clone();
            let args = args.clone();
            let explicit_tc = explicit_tc.clone();
            let frame_model = frame_model.clone();
            let content_check = content_check.clone();
            let models_cfg = models_cfg.clone();
            let buyer_policy = buyer_policy.clone();
            let events = events.clone();
            let raised_money = raised_money.clone();
            Box::pin(async move {
                prepare_lazy_buyer_api_deal_with_replay_backoff(
                    chain,
                    buyer,
                    args,
                    explicit_tc,
                    frame_model,
                    content_check,
                    models_cfg,
                    buyer_policy,
                    api_failure_policy,
                    events,
                    raised_money,
                    shellnet_preflight,
                )
                .await
            }) as dexdo::buyer::api::DealInitFuture
        }) as dexdo::buyer::api::DealInitializer
    };
    dexdo::buyer::api::ApiState::lazy(
        buyer,
        frame_model,
        initializer,
        std::time::Duration::from_secs(DEAL_WAIT_SECS),
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_buyer_on_demand_local_api(
    args: BuyerArgs,
    chain: Arc<dyn ChainBackend>,
    buyer: dexdo::buyer::Buyer,
    explicit_tc: Option<String>,
    frame_model: String,
    content_check: dexdo::buyer::api::ContentCheck,
    models_cfg: Arc<dexdo::seller::ModelsConfig>,
    buyer_policy: Option<policy::BuyerRuntimePolicy>,
    api_failure_policy: dexdo::buyer::api::BuyerApiFailurePolicy,
    events: SharedBuyerEvents,
    raised_money: Option<BuyerQuoteSubmitOutcome>,
    shellnet_preflight: BuyerShellnetPreflight,
    shutdown: BuyerShutdownSignal,
) -> Result<()> {
    use dexdo::buyer::api;

    let bind = args
        .local_listen
        .ok_or_else(|| anyhow::anyhow!("on-demand local API requires --local-listen"))?;
    let buyer = Arc::new(buyer);
    let args = Arc::new(args);
    let pre_adopted_deal = if args.resume {
        Some(
            prepare_lazy_buyer_api_deal_with_replay_backoff(
                chain.clone(),
                buyer.clone(),
                args.clone(),
                explicit_tc.clone(),
                frame_model.clone(),
                content_check.clone(),
                models_cfg.clone(),
                buyer_policy.clone(),
                api_failure_policy,
                events.clone(),
                raised_money.clone(),
                shellnet_preflight,
            )
            .await?,
        )
    } else {
        None
    };
    let endpoint_token_contract = pre_adopted_deal
        .as_ref()
        .map(|deal| deal.route.token_contract.as_str())
        .or(explicit_tc.as_deref())
        .map(|token_contract| {
            dexdo_core::normalize_wallet_address(token_contract)
                .map_err(|error| anyhow::anyhow!("on-demand endpoint TokenContract: {error}"))
        })
        .transpose()?;
    let endpoint_deal_handle = endpoint_token_contract
        .as_deref()
        .map(deals::make_handle_id);
    emit_shared_buyer_event(
        &events,
        "endpoint_binding",
        machine::OP_BUYER_START,
        json!({
            "token_contract": endpoint_token_contract,
            "deal_handle": endpoint_deal_handle,
            "requested_bind_addr": bind.to_string(),
            "allow_port_zero": bind.port() == 0
        }),
    )
    .await?;

    let state = if let Some(deal) = pre_adopted_deal {
        api::ApiState {
            buyer,
            frame_model: frame_model.clone(),
            deals: Arc::new(api::RouteManager::new(deal)),
        }
    } else {
        build_on_demand_buyer_api_state(
            chain,
            buyer,
            args.clone(),
            explicit_tc,
            frame_model.clone(),
            content_check,
            models_cfg,
            buyer_policy,
            api_failure_policy,
            events.clone(),
            raised_money,
            shellnet_preflight,
        )
    };
    let deals = state.deals.clone();
    let (addr, task) = match api::serve(bind, state, args.anthropic_compat, shutdown).await {
        Ok(ok) => ok,
        Err(err) => {
            if let Some(events) = &events {
                let code = machine::classify_error(machine::OP_BUYER_START, &err);
                events.lock().await.error(
                    machine::OP_BUYER_START,
                    code,
                    json!({
                        "network": if args.mock.mock_chain { "mock" } else { "shellnet" },
                        "frame_model": frame_model.clone(),
                        "requested_bind_addr": bind.to_string()
                    }),
                )?;
                return Err(machine::printed_error());
            }
            return Err(err);
        }
    };
    let base_url = format!("http://{addr}/v1");
    let models_url = format!("{base_url}/models");
    let readiness = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?
        .get(&models_url)
        .send()
        .await
        .and_then(|r| r.error_for_status());
    let models: serde_json::Value = match readiness {
        Ok(response) => response.json().await?,
        Err(err) => {
            if let Some(events) = &events {
                events.lock().await.error(
                    machine::OP_BUYER_START,
                    machine::ErrorCode::EndpointReadinessFailed,
                    json!({
                        "network": if args.mock.mock_chain { "mock" } else { "shellnet" },
                        "frame_model": frame_model.clone(),
                        "requested_bind_addr": bind.to_string()
                    }),
                )?;
                return Err(machine::printed_error());
            }
            return Err(anyhow::anyhow!(
                "endpoint readiness /v1/models failed: {err}"
            ));
        }
    };
    let ready = models["data"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["id"].as_str() == Some(frame_model.as_str()))
    });
    if !ready {
        if let Some(events) = &events {
            events.lock().await.error(
                machine::OP_BUYER_START,
                machine::ErrorCode::EndpointReadinessFailed,
                json!({
                    "network": if args.mock.mock_chain { "mock" } else { "shellnet" },
                    "frame_model": frame_model.clone(),
                    "requested_bind_addr": bind.to_string()
                }),
            )?;
            return Err(machine::printed_error());
        }
        bail!("endpoint readiness /v1/models did not include the selected model");
    }
    emit_shared_buyer_event(
        &events,
        "endpoint_ready",
        machine::OP_BUYER_RUNTIME,
        json!({
            "token_contract": endpoint_token_contract,
            "deal_handle": endpoint_deal_handle,
            "bind_addr": addr.to_string(),
            "base_url": base_url,
            "models_url": models_url,
            "served_models": [frame_model.clone()],
            "anthropic_compat": args.anthropic_compat
        }),
    )
    .await?;
    tracing::info!(
        %addr,
        anthropic_compat = args.anthropic_compat,
        "consumer API listening; on-demand purchase will run on first chat request"
    );
    task.await?;

    let active = deals.current().await;
    let (token_contract, deal_handle) = active
        .as_ref()
        .map(|deal| {
            let tc = deal.route.token_contract.clone();
            let handle = deals::make_handle_id(&tc);
            (tc, handle)
        })
        .or_else(|| endpoint_token_contract.zip(endpoint_deal_handle))
        .unwrap_or_default();
    emit_shared_buyer_event(
        &events,
        "stopping",
        machine::OP_BUYER_SHUTDOWN,
        json!({
            "token_contract": token_contract.clone(),
            "deal_handle": deal_handle.clone(),
            "reason": "signal"
        }),
    )
    .await?;
    emit_shared_buyer_event(
        &events,
        "settlement_submitted",
        machine::OP_BUYER_SHUTDOWN,
        json!({
            "token_contract": token_contract.clone(),
            "deal_handle": deal_handle.clone(),
            "role": "buyer",
            "action": "streamStop",
            "submitted": active.is_some()
        }),
    )
    .await?;
    emit_shared_buyer_event(
        &events,
        "settled",
        machine::OP_BUYER_SHUTDOWN,
        json!({
            "token_contract": token_contract.clone(),
            "deal_handle": deal_handle.clone(),
            "role": "buyer",
            "action": "streamStop",
            "state": if active.is_some() { "stopped" } else { "no_deal" },
            "terminal": false
        }),
    )
    .await?;
    emit_shared_buyer_event(
        &events,
        "exiting",
        machine::OP_BUYER_SHUTDOWN,
        json!({
            "token_contract": token_contract,
            "deal_handle": deal_handle,
            "outcome": "settled",
            "exit_code": 0
        }),
    )
    .await?;
    Ok(())
}

async fn run_buyer_inner(
    args: BuyerArgs,
    machine_events: &mut Option<machine::BuyerEventWriter>,
    machine_context: &mut BuyerMachineErrorContext,
    runtime: BuyerCommandRuntime,
) -> Result<()> {
    let BuyerCommandRuntime {
        backend,
        shellnet_preflight,
        shutdown,
    } = runtime;
    // Issue: token_contract + frame_model come from `--market`(a provision manifest) or the flags.
    // The buyer ignores the deal nonce: it places a buy, it does not post the offer.
    // Model-only buy: with neither
    // `--token-contract` nor `--market`, the buyer derives the per-model book from `--frame-model`, shows the
    // resting asks, places a model-wide buy, and learns the matched deal `TokenContract` from ITS OWN note's
    // `InferenceFilledConfirmed` event -- no seller hand-off. With `--token-contract`/`--market` the explicit
    // deal address is used as before(back-compat).
    let model_only = args.market.is_none() && args.token_contract.is_none();
    let (explicit_tc, frame_model) = if model_only {
        let fm = args.frame_model.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "provide --frame-model (model-only buy: the orderbook is derived from the model name), \
                 or --token-contract / --market for an explicit deal"
            )
        })?;
        (None, fm)
    } else {
        let (tc, fm, _nonce) = resolve_market_fields(
            args.market.as_deref(),
            args.token_contract.as_deref(),
            args.frame_model.as_deref(),
        )?;
        let fm =
            fm.ok_or_else(|| anyhow::anyhow!("provide --frame-model or --market <manifest>"))?;
        (Some(tc), fm)
    };
    // Model-only discovery derives the order-book address from `sha256(frame_model)`, so the id MUST be the
    // canonical `producer--model--version`(else it looks at the wrong book). Only enforce here: on the explicit
    // `--token-contract`/`--market` path the deal address is given directly (frame_model is only B2/B7 there,
    // where `family_of` matches by substring regardless of form), and the mock demo uses `dexdo-mock`.
    if model_only && !args.mock.mock_chain {
        dexdo_core::validate_canonical_model_id(&frame_model).map_err(|e| anyhow::anyhow!(e))?;
    }
    machine_context.network = Some(
        if args.mock.mock_chain {
            "mock"
        } else {
            "shellnet"
        }
        .to_string(),
    );
    machine_context.frame_model = Some(frame_model.clone());
    if let Some(tc) = explicit_tc.as_deref() {
        machine_context.order_book = Some("explicit_token_contract".to_string());
        machine_context.set_token_contract(tc);
    } else if !args.resume {
        machine_context.order_book = Some("model_order_book".to_string());
    }
    // Model-only `--resume` is supported (directive: the buyer recovers its deal from ITS OWN note's fill
    // event, never a hand-pasted `--token-contract`): it re-scans `InferenceFilledConfirmed` on this note over
    // a lookback window and connects to the freshly matched deal without placing a new buy. Handled below.
    // fail closed BEFORE the on-chain buy if this is a one-shot real-upstream attempt(promptless) --
    // an actionable client-side error, not a deep gateway `InvalidArgument` after place_buy + handover.
    oneshot_real_upstream_guard(args.local_listen.is_some(), args.mock.mock_model)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if model_only && args.mock.mock_chain {
        bail!(
            "model-only buy (no --token-contract/--market) discovers the book on real shellnet; on --mock-chain \
             pass --token-contract 0:<deal> (the mock has no on-chain orderbook to discover)"
        );
    }
    if let Some(events) = machine_events.as_mut() {
        events.event(
            "starting",
            machine::OP_BUYER_START,
            json!({
                "network": if args.mock.mock_chain { "mock" } else { "shellnet" },
                "frame_model": frame_model.clone(),
                "mode": if args.resume { "resume" } else { "buy" },
                "requested_bind_addr": args.local_listen.map(|a| a.to_string()),
                "anthropic_compat": args.anthropic_compat,
                "continuity_mode": args.continuity_mode.as_str()
            }),
        )?;
    }
    #[cfg(debug_assertions)]
    if let Some(err) = buyer_machine_error_fixture_from_env() {
        return Err(err);
    }
    let buyer_policy = if !args.mock.mock_chain {
        Some(policy::load_buyer_runtime_policy(args.policy.as_deref())?)
    } else {
        None
    };
    let api_failure_policy = buyer_policy
        .as_ref()
        .map(policy::BuyerRuntimePolicy::as_api_failure_policy)
        .unwrap_or_default();
    if let Some(policy) = buyer_policy.as_ref() {
        tracing::debug!(
            policy_no_handover_after_match = policy.no_handover_after_match.as_str(),
            policy_malformed_handover = policy.malformed_handover.as_str(),
            policy_dead_gateway = policy.dead_gateway.as_str(),
            policy_empty_stream = policy.empty_stream.as_str(),
            policy_seller_stalls_mid_stream = policy.seller_stalls_mid_stream.as_str(),
            policy_bad_output_scam = policy.bad_output_scam.as_str(),
            policy_max_sellers_to_try = policy.max_sellers_to_try,
            policy_total_spend_cap_shells = policy.total_spend_cap_shells,
            "buyer policy loaded"
        );
        validate_buyer_runtime_surface_policy(policy, args.local_listen)?;
    }
    // The chain is selected by a flag: `--mock-chain` -> mock(as in D1, also requires `--mock-model`), otherwise
    // real shellnet(per-role buyer backend behind the `shellnet` feature; without the feature -> explicit failure).
    let (chain, note) = if let Some(backend) = backend {
        backend
    } else if args.mock.mock_chain {
        args.mock.require_mock_model()?;
        let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
        mock_chain_and_note(endpoints_file, &args.identity)?
    } else {
        buyer_real_backend(&args, &frame_model)?
    };
    let buyer = dexdo::buyer::Buyer::from_note(note);
    let submit_intent = if args.continuity_mode == ContinuityModeArg::OnDemand {
        BuyerSubmitIntent::on_demand()
    } else {
        BuyerSubmitIntent::foreground()
    };
    let raised_money = if args.mock.mock_chain {
        None
    } else {
        let escrow = args
            .escrow
            .unwrap_or_else(|| required_escrow_for_buy(args.ticks, args.max_price_per_tick));
        raise_pending_buyer_money_before_fresh_reads(
            chain.as_ref(),
            &buyer,
            args.identity.note_addr.as_deref(),
            &submit_intent,
            explicit_tc.as_deref(),
            args.ticks,
            args.max_price_per_tick,
            escrow,
        )
        .await?
    };
    let buyer_content_policy = if args.local_listen.is_some() {
        match build_buyer_content_policy(&args, &frame_model).await {
            Ok(policy) => Some(policy),
            Err(err) => {
                machine_context.failure_class = Some("content_identity_preflight".to_string());
                machine_context.missing_or_unset =
                    Some("allow_unverified_model_or_models_data".to_string());
                return Err(err);
            }
        }
    } else {
        None
    };
    if args.local_listen.is_some() && args.continuity_mode == ContinuityModeArg::OnDemand {
        let events = machine_events
            .take()
            .map(|writer| Arc::new(tokio::sync::Mutex::new(writer)));
        let (content_check, models_cfg) = buyer_content_policy
            .expect("local-listen buyer content policy is preflighted before on-demand");
        return run_buyer_on_demand_local_api(
            args,
            chain,
            buyer,
            explicit_tc,
            frame_model,
            content_check,
            models_cfg,
            buyer_policy,
            api_failure_policy,
            events,
            raised_money,
            shellnet_preflight,
            shutdown,
        )
        .await;
    }
    if !args.mock.mock_chain {
        shellnet_doctor_preflight(&args.contracts, args.market.as_deref()).await?;
        if let Some(policy) = load_enabled_model_registry_policy(
            RegistryRole::Buyer,
            &args.registry,
            &args.contracts,
        )? {
            reject_buyer_raw_token_contract_without_registry_book_proof(
                args.market.as_deref(),
                args.token_contract.as_deref(),
                &frame_model,
            )?;
            let expected_order_book = if let Some(market) = args.market.as_deref() {
                load_market(market)?.inference_order_book
            } else {
                let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: --note-addr is required to derive the buyer order book"
                    )
                })?;
                expected_order_book_for_note(&args.contracts, note_addr, &frame_model).await?
            };
            let order_book_active =
                order_book_active_from_contracts(&args.contracts, &expected_order_book).await?;
            enforce_model_registry_policy(
                RegistryRole::Buyer,
                &policy,
                &args.contracts,
                &frame_model,
                &expected_order_book,
                order_book_active,
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
    }
    // Resolve the deal `TokenContract`: explicit(flag/manifest) or model-only (book -> choose -> buy -> fill
    // event). `buy_ticks` is the chosen volume(the consumer-API token budget tracks it).
    let adopted_submit_identity = raised_money
        .as_ref()
        .and_then(|outcome| outcome.submit_reconciliation.as_ref())
        .map(|reconciliation| reconciliation.submit_identity.clone());
    let mut service_renewal: Option<(u128, u128, u128)> = None;
    let (mut token_contract, buy_ticks) = if let Some(outcome) = raised_money {
        machine_context.set_token_contract(&outcome.token_contract);
        if let Some(events) = machine_events.as_mut() {
            if args.resume {
                events.event(
                    "resume_selected",
                    machine::OP_BUYER_START,
                    recovered_buyer_resume_selected_fields(&frame_model, &outcome)?,
                )?;
            } else {
                events.event(
                    "buy_submitted",
                    machine::OP_BUYER_START,
                    buyer_submit_event_fields(
                        &frame_model,
                        if explicit_tc.is_some() {
                            "explicit_token_contract"
                        } else {
                            "model_order_book"
                        },
                        outcome.ticks,
                        outcome.max_price_per_tick,
                        outcome.escrow,
                        BuyerSubmitProgress {
                            reconciled_ambiguous_submit: true,
                            submit_reconciliation: outcome.submit_reconciliation.clone(),
                        },
                    ),
                )?;
            }
        }
        (outcome.token_contract, outcome.ticks)
    } else {
        match explicit_tc {
            Some(tc) => {
                if args.resume {
                    // Connect to an ALREADY-matched deal -- escrow is already committed; a fresh place_buy would
                    // double-pay. Skip straight to reading the on-chain handover + serving.
                    if let Some(events) = machine_events.as_mut() {
                        events.event(
                            "resume_selected",
                            machine::OP_BUYER_START,
                            json!({
                                "token_contract": tc.clone(),
                                "role": "buyer",
                                "source": "token_contract",
                                "deal_handle": deals::make_handle_id(&tc),
                                "frame_model": frame_model.clone()
                            }),
                        )?;
                    } else {
                        println!("resuming existing deal {tc} -- connecting without a new buy");
                    }
                } else {
                    require_stream_buy_ticks(args.ticks)?;
                    let selection = buyer_quote_selection_for_submit(
                        chain.as_ref(),
                        args.mock.mock_chain,
                        args.identity.note_addr.as_deref(),
                        &submit_intent,
                        Some(&tc),
                        args.ticks,
                        args.max_price_per_tick,
                        args.escrow,
                    )
                    .await?;
                    if let Some(events) = machine_events.as_mut() {
                        if fail_buyer_quote_selection(
                            events,
                            &frame_model,
                            &selection,
                            args.ticks,
                            args.max_price_per_tick,
                            machine_context.fields(),
                        )?
                        .is_some()
                        {
                            return Err(machine::printed_error());
                        }
                        events.event(
                            "quote_selected",
                            machine::OP_BUYER_START,
                            quote_selected_fields(
                                &frame_model,
                                &selection,
                                args.ticks,
                                args.max_price_per_tick,
                            ),
                        )?;
                    } else {
                        require_complete_buyer_quote(&selection)?;
                    }
                    require_stream_buy_ticks(args.ticks)?;
                    let submitted_escrow = selection.escrow;
                    let submit_frame_model = frame_model.clone();
                    let submit_ticks = args.ticks;
                    let submit_max_price = args.max_price_per_tick;
                    let outcome = execute_buyer_quote_submit(
                        chain.as_ref(),
                        &buyer,
                        args.mock.mock_chain,
                        args.identity.note_addr.as_deref(),
                        &submit_intent,
                        Some(&tc),
                        &selection,
                        args.ticks,
                        args.max_price_per_tick,
                        submitted_escrow,
                        |progress| {
                            let result = match machine_events.as_mut() {
                                Some(events) => events.event(
                                    "buy_submitted",
                                    machine::OP_BUYER_START,
                                    buyer_submit_event_fields(
                                        &submit_frame_model,
                                        "explicit_token_contract",
                                        submit_ticks,
                                        submit_max_price,
                                        submitted_escrow,
                                        progress,
                                    ),
                                ),
                                None => Ok(()),
                            };
                            std::future::ready(result)
                        },
                    )
                    .await?;
                    if let Some(events) = machine_events.as_mut() {
                        events.event(
                            "matched",
                            machine::OP_BUYER_START,
                            json!({
                                "frame_model": frame_model.clone(),
                                "order_book": "explicit_token_contract",
                                "token_contract": outcome.token_contract.clone()
                            }),
                        )?;
                    }
                    if !outcome.token_contract.eq_ignore_ascii_case(&tc) {
                        bail!(
                            "explicit buyer submit matched {}, expected {}; journal retained",
                            outcome.token_contract,
                            tc
                        );
                    }
                }
                (tc, args.ticks)
            }
            None if args.resume => {
                // Model-only RESUME: recover the already-matched deal from THIS note's own fill event -- no new buy
                // (escrow is already committed). The book is derived from `--frame-model`; we scan the note's
                // `InferenceFilledConfirmed` ext-out over a lookback window and take the most recent buy match.
                let since_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
                    - RESUME_LOOKBACK_SECS;
                if machine_events.is_none() {
                    println!(
                    "resume (model-only): scanning this note's own fill events (last {RESUME_LOOKBACK_SECS}s) \
                     for a matched deal on {frame_model} -- no new buy"
                );
                }
                let tc = chain
                    .wait_matched_token_contract(
                        since_unix,
                        std::time::Duration::from_secs(DEAL_WAIT_SECS),
                    )
                    .await?
                    .ok_or_else(|| {
                        ChainError::Chain("buyer fill event returned no match".to_string())
                    })?
                    .token_contract;
                chain.assert_model_only_resume_target(&tc).await?;
                machine_context.order_book = Some("model_order_book".to_string());
                machine_context.set_token_contract(&tc);
                if let Some(events) = machine_events.as_mut() {
                    events.event(
                        "resume_selected",
                        machine::OP_BUYER_START,
                        json!({
                            "token_contract": tc.clone(),
                            "role": "buyer",
                            "source": "note_fill_event",
                            "deal_handle": deals::make_handle_id(&tc),
                            "frame_model": frame_model.clone()
                        }),
                    )?;
                } else {
                    println!("recovered matched deal TokenContract from note event: {tc}");
                }
                (tc, args.ticks)
            }
            None => {
                // Show the book, THEN let the buyer choose how many ticks and the per-tick price ceiling
                // (the flags `--ticks`/`--max-price-per-tick` are the defaults / the non-interactive value).
                let (ticks, max_price) = if machine_events.is_none() {
                    render_inference_book(
                        chain.as_ref(),
                        &frame_model,
                        args.max_price_per_tick,
                        args.ticks,
                    )
                    .await?;
                    (
                        prompt_u128("How many ticks to buy", args.ticks),
                        prompt_u128(
                            "Maximum price per tick (SHELL/tick)",
                            args.max_price_per_tick,
                        ),
                    )
                } else {
                    (args.ticks, args.max_price_per_tick)
                };
                // Escrow: an explicit `--escrow` wins(checked == required downstream); otherwise the exact
                // required for the CHOSEN order.
                let escrow = args
                    .escrow
                    .unwrap_or_else(|| dexdo_core::required_escrow_for_buy(ticks, max_price));
                service_renewal = Some((ticks, max_price, escrow));
                require_stream_buy_ticks(ticks)?;
                if machine_events.is_none() {
                    println!("placing buy: {ticks} ticks at <= {max_price}/tick (escrow {escrow})");
                }
                let selection = buyer_quote_selection_for_submit(
                    chain.as_ref(),
                    args.mock.mock_chain,
                    args.identity.note_addr.as_deref(),
                    &submit_intent,
                    None,
                    ticks,
                    max_price,
                    Some(escrow),
                )
                .await?;
                if let Some(events) = machine_events.as_mut() {
                    if fail_buyer_quote_selection(
                        events,
                        &frame_model,
                        &selection,
                        ticks,
                        max_price,
                        machine_context.fields(),
                    )?
                    .is_some()
                    {
                        return Err(machine::printed_error());
                    }
                    events.event(
                        "quote_selected",
                        machine::OP_BUYER_START,
                        quote_selected_fields(&frame_model, &selection, ticks, max_price),
                    )?;
                } else {
                    require_complete_buyer_quote(&selection)?;
                }
                require_stream_buy_ticks(ticks)?;
                let submit_frame_model = frame_model.clone();
                let outcome = execute_buyer_quote_submit(
                    chain.as_ref(),
                    &buyer,
                    args.mock.mock_chain,
                    args.identity.note_addr.as_deref(),
                    &submit_intent,
                    None,
                    &selection,
                    ticks,
                    max_price,
                    escrow,
                    |progress| {
                        let result = match machine_events.as_mut() {
                            Some(events) => events.event(
                                "buy_submitted",
                                machine::OP_BUYER_START,
                                buyer_submit_event_fields(
                                    &submit_frame_model,
                                    "model_order_book",
                                    ticks,
                                    max_price,
                                    escrow,
                                    progress,
                                ),
                            ),
                            None => Ok(()),
                        };
                        std::future::ready(result)
                    },
                )
                .await?;
                tracing::info!("model-only buy placed and matched from the note's fill event");
                machine_context.set_token_contract(&outcome.token_contract);
                if let Some(events) = machine_events.as_mut() {
                    events.event(
                        "matched",
                        machine::OP_BUYER_START,
                        json!({
                            "frame_model": frame_model.clone(),
                            "order_book": "model_order_book",
                            "token_contract": outcome.token_contract.clone()
                        }),
                    )?;
                } else {
                    println!("matched deal TokenContract: {}", outcome.token_contract);
                }
                if machine_events.is_none() {
                    println!(
                        "{}",
                        matched_state_summary(&outcome.token_contract, &outcome.status)
                    );
                }
                (outcome.token_contract, outcome.ticks)
            }
        }
    };
    record_buyer_token_contract_after_money_move(&args, &token_contract);
    tracing::info!("buy placed; awaiting handover");
    // Wait for the seller to open the stream and write the handover. Issue: fail-closed on the deadline instead of
    // waiting forever; do not swallow the `resolve_endpoint` error(diagnostics for the operator).
    let mut handover_attempt = 1u64;
    let handover = 'handover: loop {
        let hv_deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(DEAL_WAIT_SECS);
        let hv_deadline_unix = machine::now_unix()?.saturating_add(DEAL_WAIT_SECS);
        if let Some(events) = machine_events.as_mut() {
            events.event(
                "handover_waiting",
                machine::OP_BUYER_START,
                json!({
                    "token_contract": token_contract.clone(),
                    "deadline_unix": hv_deadline_unix,
                    "poll_interval_ms": 500
                }),
            )?;
        }
        loop {
            match buyer
                .resolve_endpoint(chain.as_ref(), &token_contract)
                .await
            {
                Ok(h) => break 'handover h,
                Err(e) => {
                    if is_malformed_handover_error(&e) {
                        if let Some(policy) = buyer_policy.as_ref() {
                            apply_malformed_handover_policy(
                                chain.as_ref(),
                                &buyer,
                                &token_contract,
                                policy,
                                &e,
                            )
                            .await?;
                        }
                        return Err(
                            e.context(format!("buyer: malformed handover for {token_contract}"))
                        );
                    }
                    if std::time::Instant::now() >= hv_deadline {
                        let last_error = format!("{e:#}");
                        let diagnostic = handover_timeout_diagnostic(
                            chain.as_ref(),
                            &token_contract,
                            &last_error,
                        )
                        .await;
                        if let Some(policy) = buyer_policy.as_ref() {
                            let policy_outcome = apply_no_handover_after_match_policy(
                                chain.as_ref(),
                                &buyer,
                                &token_contract,
                                policy,
                                service_renewal,
                                handover_attempt,
                                &diagnostic,
                                args.identity.note_addr.as_deref(),
                            )
                            .await;
                            match policy_outcome {
                                Err(policy_err) => {
                                    return Err(e.context(format!("{policy_err:#}")));
                                }
                                Ok(NoHandoverPolicyOutcome::RetryCurrent) => {
                                    continue 'handover;
                                }
                                Ok(NoHandoverPolicyOutcome::RetryNext(next)) => {
                                    token_contract = next;
                                    record_buyer_token_contract_after_money_move(
                                        &args,
                                        &token_contract,
                                    );
                                    handover_attempt = handover_attempt.saturating_add(1);
                                    continue 'handover;
                                }
                            }
                        }
                        return Err(e.context(diagnostic));
                    }
                    tracing::debug!(error = %e, "buyer: no handover yet -- waiting for the seller's open_stream");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    };
    let mut deal_handle = deals::make_handle_id(&token_contract);
    if let Some(events) = machine_events.as_mut() {
        events.event(
            "handover_received",
            machine::OP_BUYER_START,
            json!({
                "token_contract": token_contract.clone(),
                "deal_handle": deal_handle.clone(),
                "handover_anchor": {"kind":"token_contract_state","value":"handover_present"}
            }),
        )?;
    }
    let should_save_handle = !args.mock.mock_chain || machine_events.is_some();
    if should_save_handle {
        let mock_note_addr;
        let note_addr = if args.mock.mock_chain {
            mock_note_addr = format!("mock:{}", note_pubkey_id(&buyer.note.pubkey()));
            mock_note_addr.as_str()
        } else {
            args.identity.note_addr.as_deref().ok_or_else(|| {
                anyhow::anyhow!("real shellnet: --note-addr is required to save the deal handle")
            })?
        };
        let endpoint = Some(deals::DealEndpointInfo {
            kind: if args.local_listen.is_some() {
                "local-listen".to_string()
            } else {
                "one-shot".to_string()
            },
            value: args
                .local_listen
                .map(|a| a.to_string())
                .unwrap_or_else(|| "promptless-mock-stream".to_string()),
        });
        let input = RuntimeDealHandleInput {
            role: deals::DealHandleRole::Buyer,
            deals_dir: args.deals_dir.as_deref(),
            token_contract: &token_contract,
            note_addr,
            frame_model: &frame_model,
            market_path: args.market.as_deref(),
            contracts: &args.contracts,
            endpoint,
        };
        let saved = if args.mock.mock_chain {
            save_mock_runtime_deal_handle(input)?
        } else {
            save_runtime_deal_handle(input, machine_events.is_none())?
        };
        deal_handle = saved.handle;
    }
    clear_adopted_buyer_money_journal(
        args.identity.note_addr.as_deref(),
        adopted_submit_identity.as_deref(),
        &token_contract,
    )?;
    // B19/B20: if `--local-listen` is set, bring up a local interface to
    // the consumer(OpenAI-compatible + optional Anthropic transcoding) and serve requests.
    if let Some(bind) = args.local_listen {
        use dexdo::buyer::api::{self, ApiState, Route};
        let continuity_mode = args.continuity_mode.as_planner_mode();
        tracing::info!(
            continuity_mode = args.continuity_mode.as_str(),
            "buyer continuity mode selected"
        );
        let buyer = Arc::new(buyer);
        // Session-scoped settlement: one shared SessionSettle for the deal -- STOP once at session
        // end(graceful shutdown) or on a verification-bail, NOT per request.
        let session = Arc::new(api::SessionSettle::new_with_failure_policy(
            chain.clone(),
            token_contract.clone(),
            buyer.note.clone(),
            api_failure_policy,
        ));
        let (content_check, models_cfg) = buyer_content_policy
            .expect("local-listen buyer content policy is preflighted before buy");
        let renewal_content_check = content_check.clone();
        let state = ApiState::single(
            buyer,
            Route {
                handover,
                token_contract: token_contract.clone(),
                max_tokens: consumer_api_token_budget(buy_ticks),
            },
            frame_model.clone(),
            session,
            std::sync::Arc::new(dexdo::buyer::api::ContentGate::new(
                content_check,
                models_cfg.clone(),
            )),
        );
        if let Some((ticks, max_price, escrow)) = service_renewal {
            spawn_buyer_service_renewal(
                chain.clone(),
                state.buyer.clone(),
                state.deals.clone(),
                if args.mock.mock_chain {
                    None
                } else {
                    args.identity.note_addr.clone()
                },
                ticks,
                max_price,
                escrow,
                continuity_mode,
                renewal_content_check,
                models_cfg.clone(),
                api_failure_policy,
            );
        }
        // The operator close: SIGINT(Ctrl-C) or SIGTERM(systemd/container) triggers graceful
        // shutdown, after which `serve()` awaits the session STOP before exit -- the funds-safety terminal (not
        // `Drop`). SIGTERM must NOT bypass it(review).
        if let Some(events) = machine_events.as_mut() {
            events.event(
                "endpoint_binding",
                machine::OP_BUYER_START,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "requested_bind_addr": bind.to_string(),
                    "allow_port_zero": bind.port() == 0
                }),
            )?;
        }
        let (addr, task) = api::serve(
            bind,
            state,
            args.anthropic_compat,
            operator_shutdown_signal(),
        )
        .await?;
        let base_url = format!("http://{addr}/v1");
        let models_url = format!("{base_url}/models");
        if let Some(events) = machine_events.as_mut() {
            let models: serde_json::Value = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()?
                .get(&models_url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let ready = models["data"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["id"].as_str() == Some(frame_model.as_str()))
            });
            if !ready {
                anyhow::bail!("endpoint readiness /v1/models did not include the selected model");
            }
            events.event(
                "endpoint_ready",
                machine::OP_BUYER_RUNTIME,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "bind_addr": addr.to_string(),
                    "base_url": base_url,
                    "models_url": models_url,
                    "served_models": [frame_model.clone()],
                    "anthropic_compat": args.anthropic_compat
                }),
            )?;
        }
        tracing::info!(%addr, anthropic_compat = args.anthropic_compat, "consumer API listening (loopback)");
        task.await?;
        if let Some(events) = machine_events.as_mut() {
            events.event(
                "stopping",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "reason": "signal"
                }),
            )?;
            events.event(
                "settlement_submitted",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "role": "buyer",
                    "action": "streamStop",
                    "submitted": true
                }),
            )?;
            events.event(
                "settled",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "role": "buyer",
                    "action": "streamStop",
                    "state": "stopped",
                    "terminal": false
                }),
            )?;
            events.event(
                "exiting",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "outcome": "settled",
                    "exit_code": 0
                }),
            )?;
        }
        return Ok(());
    }

    let oneshot_session = dexdo::buyer::api::SessionSettle::new_with_failure_policy(
        chain.clone(),
        token_contract.clone(),
        buyer.note.clone(),
        api_failure_policy,
    );
    let mut stream_attempt = 1u64;
    let out = loop {
        match buyer
            .connect_and_stream(&handover, &token_contract, args.max_tokens)
            .await
        {
            Ok(out) => break out,
            Err(e) => match apply_oneshot_dead_gateway_policy(
                &oneshot_session,
                &token_contract,
                buyer_policy.as_ref(),
                stream_attempt,
            )
            .await
            {
                OneShotStreamPolicyOutcome::RetryCurrent => {
                    stream_attempt = stream_attempt.saturating_add(1);
                    continue;
                }
                OneShotStreamPolicyOutcome::TerminalReport(report) => {
                    return Err(e.context(report));
                }
            },
        }
    };
    if out.received == 0 {
        let report = apply_oneshot_empty_stream_policy(
            &oneshot_session,
            &token_contract,
            buyer_policy.as_ref(),
        )
        .await;
        bail!("{report}");
    }
    tracing::info!(received = out.received, "received fake tokens; STOP");
    settle_completed_oneshot(&oneshot_session).await?;
    Ok(())
}

async fn settle_completed_oneshot(session: &dexdo::buyer::api::SessionSettle) -> Result<()> {
    session
        .settle("one-shot-complete")
        .await
        .map_err(anyhow::Error::new)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::args::SubscriptionPlaceArgs;
    #[cfg(feature = "shellnet")]
    use crate::cli::args::{IdentityArgs, NoteDeployArgs};

    #[cfg(feature = "shellnet")]
    struct CountingSubscriptionChain {
        submit_calls: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "shellnet")]
    impl CountingSubscriptionChain {
        #[allow(clippy::too_many_arguments)]
        async fn place_inference_subscription(
            &self,
            _note: &dexdo_core::Address,
            _owner_keys: &dexdo_core::KeyPair,
            _model_hash: &str,
            _max_price_per_tick: u128,
            _ticks: u128,
            _escrow: u128,
            _auto_renew: bool,
        ) -> anyhow::Result<serde_json::Value> {
            self.submit_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({}))
        }
    }

    #[tokio::test]
    async fn direct_chain_read_timeout_returns_terminal_retryable_error() {
        let started = std::time::Instant::now();
        let err = super::direct_chain_read_with_timeout(1, async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .expect_err("slow read must fail at the bounded timeout")
        .to_string();

        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "timeout should be terminal within the configured bound"
        );
        assert!(err.contains("chain read timed out after 1s"), "{err}");
        assert!(err.contains("retry"), "{err}");
        assert!(err.contains("dexdo market-data"), "{err}");
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn transient_read_retries_with_backoff_not_hard_fail() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let attempts = Arc::new(AtomicUsize::new(0));
        let started = std::time::Instant::now();
        let value = super::retry_executable_read("test executable read", {
            let attempts = attempts.clone();
            move || {
                let attempts = attempts.clone();
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(anyhow::anyhow!("request timed out"))
                    } else {
                        Ok("ok")
                    }
                }
            }
        })
        .await
        .expect("transient failure must retry successfully");

        assert_eq!(value, "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() >= super::EXECUTABLE_READ_BACKOFF[0]);
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn seller_open_probe_close_hint_points_to_advance_not_buyer_cleanup() {
        let target = super::DealTarget {
            handle: None,
            token_contract: "0:tc".to_string(),
            role: Some(crate::cli::deals::DealHandleRole::Seller),
            note_addr: Some("0:seller".to_string()),
            market: None,
        };
        let summary = crate::cli::deals::DealStateSummary {
            kind: crate::cli::deals::DealStateKind::Probe,
            funded: true,
            opened: true,
            disputed: false,
            probe_accepted: false,
            deposit: 0,
            prepaid: 0,
            frozen: 0,
            finalized_owed: 0,
            funded_time: Some(1),
            last_advance: 1,
        };

        let hint = super::close_hint(&target, &summary);

        assert!(
            hint.contains("next=seller_advance_probe_after_timeout"),
            "{hint}"
        );
        assert!(hint.contains("TokenContract.advance()"), "{hint}");
        assert!(!hint.contains("wait_for_buyer_stop"), "{hint}");
    }

    #[test]
    fn buyer_renewal_threshold_uses_env_override() {
        let old = std::env::var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS").ok();
        std::env::set_var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS", "999999");
        assert_eq!(super::buyer_renewal_threshold_tokens(), 999_999);
        match old {
            Some(v) => std::env::set_var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS", v),
            None => std::env::remove_var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS"),
        }
    }

    #[test]
    fn subscription_place_plan_uses_exact_fee_inclusive_escrow() {
        let plan = super::subscription_place_plan(&SubscriptionPlaceArgs {
            note_key: None,
            max_price_per_tick: 1000,
            ticks: Some(4),
            budget: None,
            auto_renew: false,
        })
        .unwrap();
        assert_eq!(plan.ticks, 4);
        assert_eq!(plan.escrow, 4100);
        assert_eq!(plan.unused_budget, 0);

        let plan = super::subscription_place_plan(&SubscriptionPlaceArgs {
            note_key: None,
            max_price_per_tick: 1000,
            ticks: None,
            budget: Some(4200),
            auto_renew: false,
        })
        .unwrap();
        assert_eq!(plan.ticks, 4);
        assert_eq!(plan.escrow, 4100);
        assert_eq!(plan.unused_budget, 100);
    }

    #[test]
    fn subscription_place_plan_rejects_zero_sized_money_moves() {
        assert!(super::subscription_place_plan(&SubscriptionPlaceArgs {
            note_key: None,
            max_price_per_tick: 1000,
            ticks: Some(0),
            budget: None,
            auto_renew: false,
        })
        .is_err());
        assert!(super::subscription_place_plan(&SubscriptionPlaceArgs {
            note_key: None,
            max_price_per_tick: 1000,
            ticks: None,
            budget: Some(1),
            auto_renew: false,
        })
        .is_err());
        assert!(super::subscription_place_plan(&SubscriptionPlaceArgs {
            note_key: None,
            max_price_per_tick: 0,
            ticks: Some(1),
            budget: None,
            auto_renew: false,
        })
        .is_err());
    }

    #[derive(Clone, Copy)]
    enum QuotePreflightFailure {
        Transport,
        Contract,
    }

    #[derive(Default)]
    struct QuotePreflightChain {
        offers: Vec<dexdo_core::OfferListing>,
        model_preflight_error: Option<String>,
        model_preflight_failure: Option<QuotePreflightFailure>,
        model_preflight_calls: std::sync::atomic::AtomicUsize,
        model_preflight_transport_failures: std::sync::atomic::AtomicUsize,
        model_presubmit_preflight_calls: std::sync::atomic::AtomicUsize,
        model_submit_calls: std::sync::atomic::AtomicUsize,
        explicit_preflight_error: Option<String>,
        explicit_submit_safe_order: Option<dexdo_core::OrderBookOrder>,
        sell_offer_terms: Option<(u64, u64)>,
        sell_offer_terms_calls: std::sync::atomic::AtomicUsize,
        submit_safe_single_ask_quote: bool,
    }

    impl QuotePreflightChain {
        fn consume_transport_failure(counter: &std::sync::atomic::AtomicUsize) -> bool {
            counter
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
        }

        fn offer(
            token_contract: &str,
            price_per_tick: u64,
            max_ticks: u64,
        ) -> dexdo_core::OfferListing {
            dexdo_core::OfferListing {
                seller_id: "seller".to_string(),
                token_contract: token_contract.to_string(),
                price_per_tick,
                max_ticks,
            }
        }

        fn order(
            order_id: u128,
            token_contract: &str,
            price_per_tick: u128,
            ticks: u128,
        ) -> dexdo_core::OrderBookOrder {
            dexdo_core::OrderBookOrder {
                order_id,
                owner_note: "seller".to_string(),
                token_contract: Some(token_contract.to_string()),
                is_buy: false,
                price_per_tick,
                ticks,
                escrow: 0,
                deadline: 0,
                flags: 0,
                timestamp: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl dexdo_core::ChainBackend for QuotePreflightChain {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            Ok(self.offers.clone())
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn sell_offer_terms(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<(u64, u64)>, dexdo_core::ChainError> {
            self.sell_offer_terms_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.sell_offer_terms)
        }

        async fn assert_model_buy_matches_executable_quote(
            &self,
            _ticks: u128,
            _max_price_per_tick: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            self.model_preflight_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if Self::consume_transport_failure(&self.model_preflight_transport_failures) {
                return Err(dexdo_core::ChainError::Transport(
                    "injected model preflight transport failure".to_string(),
                ));
            }
            match self.model_preflight_failure {
                Some(QuotePreflightFailure::Transport) => {
                    return Err(dexdo_core::ChainError::Transport(
                        "quote preflight rpc transport cause".to_string(),
                    ));
                }
                Some(QuotePreflightFailure::Contract) => {
                    return Err(dexdo_core::ChainError::Contract(
                        "quote preflight contract revert cause".to_string(),
                    ));
                }
                None => {}
            }
            match &self.model_preflight_error {
                Some(err) => Err(dexdo_core::ChainError::Chain(err.clone())),
                None => Ok(()),
            }
        }

        async fn assert_explicit_buy_matches_executable_quote(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _ticks: u128,
            _max_price_per_tick: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            match &self.explicit_preflight_error {
                Some(err) => Err(dexdo_core::ChainError::Chain(err.clone())),
                None => Ok(()),
            }
        }

        async fn submit_safe_explicit_buy_quote_order(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _ticks: u128,
            _max_price_per_tick: u128,
        ) -> Result<Option<dexdo_core::OrderBookOrder>, dexdo_core::ChainError> {
            Ok(self.explicit_submit_safe_order.clone())
        }

        fn requires_submit_safe_single_ask_quote(&self) -> bool {
            self.submit_safe_single_ask_quote
        }

        async fn place_buy(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn place_buy_by_model(
            &self,
            _note: &dyn dexdo_core::Note,
            _ticks: u128,
            _max_price_per_tick: u128,
            _escrow: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            self.model_presubmit_preflight_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.model_submit_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn read_match(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn open_stream(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn read_handover(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn advance_tick(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn accept_probe(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn stop(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn seller_timeout(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            unimplemented!("not needed by quote preflight tests")
        }

        async fn snapshot(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    #[tokio::test]
    async fn buyer_model_only_quote_selection_surfaces_price_ceiling_preflight() {
        let offers = vec![QuotePreflightChain::offer("0:best", 11, 1)];
        let quote = dexdo_core::executable_quote(
            &super::mock_orders_from_offers(offers.clone()),
            Some(1),
            None,
        )
        .expect("standalone quote accepts the book without the buyer ceiling");
        assert!(quote.complete);
        let chain = QuotePreflightChain {
            offers,
            model_preflight_error: Some(
                "best ask price 11 is above buyer max_price_per_tick 10; requested ticks 1"
                    .to_string(),
            ),
            ..Default::default()
        };

        let err = match super::buyer_quote_selection(&chain, None, 1, 10, None).await {
            Ok(_) => panic!("model-only preflight must reject the quote before quote_selected"),
            Err(err) => format!("{err:#}"),
        };

        assert!(err.contains("buyer model-only quote preflight"), "{err}");
        assert!(err.contains("best ask price 11"), "{err}");
        assert!(err.contains("above buyer max_price_per_tick 10"), "{err}");
    }

    #[tokio::test]
    async fn buyer_quote_preflight_preserves_typed_chain_errors_for_classification() {
        for (failure, expected_code, expected_cause) in [
            (
                QuotePreflightFailure::Transport,
                crate::cli::machine::ErrorCode::ChainTransport,
                "quote preflight rpc transport cause",
            ),
            (
                QuotePreflightFailure::Contract,
                crate::cli::machine::ErrorCode::ChainRevert,
                "quote preflight contract revert cause",
            ),
        ] {
            let chain = QuotePreflightChain {
                model_preflight_failure: Some(failure),
                ..Default::default()
            };
            let err = match super::buyer_quote_selection(&chain, None, 1, 10, None).await {
                Ok(_) => panic!("typed quote preflight failure must propagate"),
                Err(err) => err,
            };

            assert_eq!(
                crate::cli::machine::classify_error(crate::cli::machine::OP_BUYER_START, &err,),
                expected_code
            );
            assert!(
                err.chain().any(|cause| cause
                    .downcast_ref::<dexdo_core::ChainError>()
                    .is_some_and(|chain_error| chain_error.to_string().contains(expected_cause))),
                "typed preflight cause missing from anyhow chain: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn buyer_quote_to_submit_path_stops_after_exactly_three_transient_preflight_attempts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let chain = QuotePreflightChain {
            model_preflight_transport_failures: AtomicUsize::new(super::TRANSIENT_QUOTE_ATTEMPTS),
            ..Default::default()
        };

        let note = dexdo_core::LocalNote::from_seed(&[7_u8; 32]);
        let result = async {
            let _selection = super::buyer_quote_selection(&chain, None, 2, 1000, None).await?;
            dexdo_core::ChainBackend::place_buy_by_model(&chain, &note, 2, 1000, 2050)
                .await
                .map_err(anyhow::Error::new)
        }
        .await;
        let error = match result {
            Ok(()) => panic!("three transient preflight failures must stop before submit"),
            Err(error) => error,
        };

        assert_eq!(
            chain.model_preflight_calls.load(Ordering::SeqCst),
            super::TRANSIENT_QUOTE_ATTEMPTS
        );
        assert_eq!(
            chain.model_presubmit_preflight_calls.load(Ordering::SeqCst),
            0,
            "the pre-submit selection must not run after quote retries are exhausted"
        );
        assert_eq!(
            chain.model_submit_calls.load(Ordering::SeqCst),
            0,
            "the money-moving submit must remain outside retries"
        );
        assert!(error.chain().any(|cause| matches!(
            cause.downcast_ref::<dexdo_core::ChainError>(),
            Some(dexdo_core::ChainError::Transport(message))
                if message.contains("injected model preflight transport failure")
        )));
    }

    #[tokio::test]
    async fn buyer_quote_to_submit_path_recovers_on_third_attempt_and_submits_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let chain = QuotePreflightChain {
            model_preflight_transport_failures: AtomicUsize::new(2),
            ..Default::default()
        };

        let note = dexdo_core::LocalNote::from_seed(&[7_u8; 32]);
        let _selection = super::buyer_quote_selection(&chain, None, 2, 1000, None)
            .await
            .expect("the third quote-boundary preflight attempt must succeed");
        dexdo_core::ChainBackend::place_buy_by_model(&chain, &note, 2, 1000, 2050)
            .await
            .expect("the single pre-submit preflight and money submit must succeed");

        assert_eq!(
            chain.model_preflight_calls.load(Ordering::SeqCst),
            super::TRANSIENT_QUOTE_ATTEMPTS,
            "the quote boundary must recover on its third and final attempt"
        );
        assert_eq!(
            chain.model_presubmit_preflight_calls.load(Ordering::SeqCst),
            1,
            "the production-mirroring pre-submit selection must run exactly once"
        );
        assert_eq!(
            chain.model_submit_calls.load(Ordering::SeqCst),
            1,
            "the money-moving submit must happen exactly once"
        );
    }

    #[tokio::test]
    async fn wrapped_model_preflight_chain_marker_classifies_as_no_liquidity() {
        let chain = QuotePreflightChain {
            model_preflight_error: Some(
                "no_executable_ask: no executable matching ask for InferenceOrderBook 0:book"
                    .to_string(),
            ),
            ..Default::default()
        };
        let err = match super::buyer_quote_selection(&chain, None, 1, 10, None).await {
            Ok(_) => panic!("model-only preflight marker must propagate"),
            Err(err) => err,
        };

        assert_eq!(
            crate::cli::machine::classify_error(crate::cli::machine::OP_BUYER_START, &err),
            crate::cli::machine::ErrorCode::NoLiquidity,
            "wrapped no_executable_ask marker was not classified from the full chain: {err:#}"
        );
        assert!(
            err.chain().any(|cause| cause
                .downcast_ref::<dexdo_core::ChainError>()
                .is_some_and(|chain_error| matches!(chain_error, dexdo_core::ChainError::Chain(message) if message.contains("no_executable_ask")))),
            "ChainError::Chain marker missing from production preflight chain: {err:#}"
        );
    }

    #[tokio::test]
    async fn wrapped_explicit_target_chain_marker_classifies_as_chain_revert() {
        let chain = QuotePreflightChain {
            explicit_preflight_error: Some(
                "buyer target preflight failed for InferenceOrderBook 0:book: no resting ask for expected tokenContract 0:dead"
                    .to_string(),
            ),
            ..Default::default()
        };
        let err = match super::buyer_quote_selection(&chain, Some("0:dead"), 1, 10, None).await {
            Ok(_) => panic!("explicit target preflight marker must propagate"),
            Err(err) => err,
        };

        assert_eq!(
            crate::cli::machine::classify_error(crate::cli::machine::OP_BUYER_START, &err),
            crate::cli::machine::ErrorCode::ChainRevert,
            "wrapped buyer target preflight marker was not classified from the full chain: {err:#}"
        );
        assert!(
            err.chain().any(|cause| cause
                .downcast_ref::<dexdo_core::ChainError>()
                .is_some_and(|chain_error| matches!(chain_error, dexdo_core::ChainError::Chain(message) if message.contains("buyer target preflight failed")))),
            "ChainError::Chain marker missing from production explicit preflight chain: {err:#}"
        );
    }

    #[test]
    fn model_only_no_liquidity_failure_class_is_no_executable_ask() {
        let selection = super::BuyerQuoteSelection {
            order_book: "model_order_book",
            escrow: 0,
            quote: dexdo_core::ExecutableQuote {
                filled_ticks: 0,
                total_with_fee: 0,
                complete: false,
                fills: Vec::new(),
            },
            quoted_order: None,
        };

        assert_eq!(
            super::buyer_quote_failure_class(&selection, super::machine::ErrorCode::NoLiquidity),
            "no_executable_ask"
        );
    }

    #[tokio::test]
    async fn buyer_explicit_quote_selection_runs_target_preflight_before_synthetic_terms() {
        let chain = QuotePreflightChain {
            explicit_preflight_error: Some(
                "buyer target preflight failed for InferenceOrderBook 0:book: no resting ask for expected tokenContract 0:dead"
                    .to_string(),
            ),
            sell_offer_terms: Some((11, 1)),
            ..Default::default()
        };

        let err = match super::buyer_quote_selection(&chain, Some("0:dead"), 1, 11, None).await {
            Ok(_) => panic!("explicit target preflight must reject before quote_selected"),
            Err(err) => format!("{err:#}"),
        };

        assert!(
            err.contains("buyer explicit-token quote preflight"),
            "{err}"
        );
        assert!(err.contains("buyer target preflight failed"), "{err}");
        assert_eq!(
            chain
                .sell_offer_terms_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "explicit target preflight must fail before synthetic sell_offer_terms can fabricate quote_selected"
        );
    }

    #[tokio::test]
    async fn buyer_model_only_quote_selection_accepts_partial_head_ask() {
        let chain = QuotePreflightChain {
            offers: vec![QuotePreflightChain::offer("0:big", 1000, 1024)],
            submit_safe_single_ask_quote: true,
            ..Default::default()
        };

        let selection = super::buyer_quote_selection(&chain, None, 1, 1000, None)
            .await
            .expect("selection returns an explicit no-liquidity quote");

        assert_eq!(selection.order_book, "model_order_book");
        assert!(selection.quote.complete);
        assert_eq!(selection.quote.filled_ticks, 1);
        assert_eq!(
            selection.quote.total_with_fee,
            dexdo_core::required_escrow_for_buy(1, 1000)
        );
        assert_eq!(selection.quote.fills.len(), 1);
        assert_eq!(selection.quote.fills[0].ticks, 1);
        assert_eq!(selection.quote.fills[0].token_contract, "0:big");
    }

    #[tokio::test]
    async fn buyer_explicit_quote_selection_accepts_partial_synthetic_terms() {
        let chain = QuotePreflightChain {
            sell_offer_terms: Some((1000, 1024)),
            submit_safe_single_ask_quote: true,
            ..Default::default()
        };

        let selection = super::buyer_quote_selection(&chain, Some("0:big"), 1, 1000, None)
            .await
            .expect("selection returns an explicit no-liquidity quote");

        assert_eq!(selection.order_book, "explicit_token_contract");
        assert!(selection.quote.complete);
        assert_eq!(selection.quote.filled_ticks, 1);
        assert_eq!(
            selection.quote.total_with_fee,
            dexdo_core::required_escrow_for_buy(1, 1000)
        );
        assert_eq!(selection.quote.fills.len(), 1);
        assert_eq!(selection.quote.fills[0].ticks, 1);
        assert_eq!(selection.quote.fills[0].token_contract, "0:big");
    }

    #[tokio::test]
    async fn buyer_explicit_quote_selection_uses_submit_safe_row_before_synthetic_terms() {
        let chain = QuotePreflightChain {
            explicit_submit_safe_order: Some(QuotePreflightChain::order(7, "0:big", 1000, 1024)),
            submit_safe_single_ask_quote: true,
            ..Default::default()
        };

        let selection = super::buyer_quote_selection(&chain, Some("0:big"), 1, 1000, None)
            .await
            .expect("selection returns an explicit submit-safe quote");

        assert_eq!(selection.order_book, "explicit_token_contract");
        assert!(selection.quote.complete);
        assert_eq!(selection.quote.filled_ticks, 1);
        assert_eq!(selection.quote.fills.len(), 1);
        assert_eq!(selection.quote.fills[0].order_id, 7);
        assert_eq!(selection.quote.fills[0].token_contract, "0:big");
        assert_eq!(
            chain
                .sell_offer_terms_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "explicit submit-safe row should not be replaced by synthetic terms"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn subscription_status_marks_stale_sub_without_resting_order() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "model".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: Some(dexdo_core::OrderBookStats {
                next_order_id: 2,
                order_count: 0,
                executed_notional: 0,
                executed_ticks: 0,
            }),
            orders: Vec::new(),
        };
        let sub = dexdo_core::OrderBookSubscription {
            order_id: 1,
            exists: true,
            period_start: 10,
            cur_cycle: 0,
            cycle_budget: 10250,
            cycle_spent: 10250,
            auto_renew: false,
        };

        let line = super::render_subscription_line(&snapshot, 1, None, Some(&sub));

        assert!(line.contains("exists=true"));
        assert!(line.contains("order_found=false"));
        assert!(line.contains("stale_subscription=true"));
    }

    /// Demo(run with `--nocapture`): render the model-only order book through the REAL `render_inference_book`
    /// against a `MockChainBackend` seeded with a few asks -- shows exactly what the buyer sees before choosing.
    #[tokio::test]
    async fn demo_render_inference_book() {
        use dexdo_core::{
            ChainBackend, DobParams, LocalNote, MockChainBackend, ProtocolConsts, SellOffer,
        };
        let path = std::env::temp_dir().join("dexdo_book_demo_endpoints.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("chainstate.json"));
        let mock = MockChainBackend::new(path, ProtocolConsts::canonical(), DobParams::canonical());
        let note = LocalNote::generate();
        let asks = [
            (
                "0:7c58eff6aa11b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b",
                900u64,
                512u64,
            ),
            (
                "0:18a758c0bb22c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c",
                1000,
                1024,
            ),
            (
                "0:ab1572e0cc33d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d",
                1500,
                256,
            ),
        ];
        for (tc, price, ticks) in asks {
            mock.post_offer(
                SellOffer {
                    price_per_tick: price,
                    max_ticks: ticks,
                    token_contract: tc.into(),
                },
                &note,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            mock.discover_offers().await.unwrap().len(),
            3,
            "three asks seeded"
        );
        // The buyer's view: model `qwen/qwen3-32b`, price ceiling 1000/tick, default 8 ticks.
        super::render_inference_book(&mock, "qwen/qwen3-32b", 1000, 8)
            .await
            .unwrap();
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn market_manifest_must_match_positional_model() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-market-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let models = dir.join("models.json");
        std::fs::write(
            &models,
            r#"{
              "models": {
                "qwen": {
                  "frame_model": "qwen--qwen3--32b",
                  "base_url": "https://example.invalid/openai/v1",
                  "served_model": "qwen/qwen3-32b",
                  "api_key_env": "QWEN_KEY",
                  "tokenizer_family": "qwen",
                  "price_per_tick": 1000
                },
                "llama": {
                  "frame_model": "llama--llama3--8b",
                  "base_url": "https://example.invalid/openai/v1",
                  "served_model": "llama/llama3-8b",
                  "api_key_env": "LLAMA_KEY",
                  "tokenizer_family": "llama",
                  "price_per_tick": 1000
                }
              }
            }"#,
        )
        .unwrap();
        let manifest = dexdo_core::MarketManifest {
            network: "shellnet".to_string(),
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: dexdo_core::model_hash_for("qwen--qwen3--32b"),
            inference_order_book: "0:book".to_string(),
            root_model: "0:root".to_string(),
            token_contract: "0:tc".to_string(),
            seller_note: "0:seller".to_string(),
            nonce: 7,
            price_per_tick: 1000,
            max_ticks: 8,
        };
        let market = dir.join("market.json");
        std::fs::write(&market, manifest.to_json().unwrap()).unwrap();

        assert!(super::target_from_market_for_model(&market, &models, "qwen").is_ok());
        assert!(super::target_from_market_for_model(&market, &models, "qwen--qwen3--32b").is_ok());
        let err = match super::target_from_market_for_model(&market, &models, "llama") {
            Ok(_) => panic!("wrong positional model must fail closed"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("refusing to render the wrong market"), "{err}");
        assert!(err.contains("llama--llama3--8b"), "{err}");
        assert!(err.contains("qwen--qwen3--32b"), "{err}");
    }

    #[test]
    fn seller_offer_path_has_no_exact_tc_id_walk() {
        let backend = include_str!("../../../core/src/shellnet/backends.rs");
        assert!(!backend.contains("ORDERBOOK_EXACT_TC_SCAN_TIMEOUT"));
        assert!(!backend.contains("active_sell_order_ids_for_exact_tc_bounded"));
        assert!(!backend.contains("duplicate active sell order preflight incomplete"));
    }

    /// buyer-side ModelRegistry validation must happen before either direct-deal buy or
    /// model-wide `placeInferenceBuy`.
    #[test]
    fn buyer_model_registry_preflight_precedes_buy_writes() {
        let source = include_str!("buyer.rs");
        let start = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let end = source[start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| start + offset)
            .expect("run_buyer end marker present");
        let body = &source[start..end];

        let registry = body
            .find("load_enabled_model_registry_policy")
            .expect("buyer registry policy load present");
        let role = body[registry..]
            .find("RegistryRole::Buyer")
            .map(|offset| registry + offset)
            .expect("buyer registry role present");
        let enforce = body[registry..]
            .find("enforce_model_registry_policy(")
            .map(|offset| registry + offset)
            .expect("buyer registry preflight present");
        let raw_tc_guard = body[registry..]
            .find("reject_buyer_raw_token_contract_without_registry_book_proof")
            .map(|offset| registry + offset)
            .expect("raw --token-contract guard present");
        let durable_buy = body
            .find("execute_buyer_quote_submit(")
            .expect("durable buyer submit present");

        assert!(
            registry < role
                && role < raw_tc_guard
                && raw_tc_guard < enforce
                && enforce < durable_buy,
            "registry check must precede every durable buy"
        );
    }

    /// PR347 review blocker regression: active-pool validation must stay before both direct and model-only
    /// money-moving buy submissions in lazy and one-shot buyer flows.
    #[test]
    fn buyer_pool_preflight_precedes_money_moving_buy_paths() {
        let source = include_str!("buyer.rs");
        let wrapper_start = source
            .find("async fn place_buy_by_model_after_pool_preflight")
            .expect("model buy wrapper present");
        let wrapper_end = source[wrapper_start..]
            .find("fn record_buyer_token_contract_after_money_move")
            .map(|offset| wrapper_start + offset)
            .expect("model buy wrapper end marker present");
        let wrapper = &source[wrapper_start..wrapper_end];
        let wrapper_preflight = wrapper
            .find("preflight_buyer_pool_for_note(pool_note_addr)?")
            .expect("wrapper pool preflight present");
        let wrapper_submit = wrapper
            .find(".place_buy_by_model(")
            .expect("wrapper model buy submit present");
        assert!(
            wrapper_preflight < wrapper_submit,
            "model buy wrapper must preflight DEXDO_PN_POOL before place_buy_by_model"
        );

        let lazy_start = source
            .find("async fn prepare_lazy_buyer_api_deal_once")
            .expect("lazy buyer helper present");
        let lazy_end = source[lazy_start..]
            .find("async fn run_buyer_on_demand_local_api")
            .map(|offset| lazy_start + offset)
            .expect("lazy buyer helper end marker present");
        let lazy = &source[lazy_start..lazy_end];
        assert_eq!(lazy.matches("execute_buyer_quote_submit(").count(), 2);
        assert!(!lazy.contains("buyer.place_buy(chain.as_ref(), &tc)"));

        let oneshot_start = source
            .find("async fn run_buyer_inner")
            .expect("one-shot buyer helper present");
        let oneshot_end = source[oneshot_start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| oneshot_start + offset)
            .expect("one-shot buyer helper end marker present");
        let oneshot = &source[oneshot_start..oneshot_end];
        assert_eq!(oneshot.matches("execute_buyer_quote_submit(").count(), 2);
        assert!(!oneshot.contains("buyer.place_buy(chain.as_ref(), &tc)"));
    }

    /// regression: subscription placement must fail closed before its escrow POST when the
    /// recovery pool is unavailable.
    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn subscription_missing_pool_blocks_money_moving_submit() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let _env = EnvVarGuard::unset("DEXDO_PN_POOL");
        let note_addr = format!("0:{}", "1".repeat(64));
        let note = dexdo_core::Address::parse(&note_addr).unwrap();
        let keys = dexdo_core::KeyPair::from_secret_hex(&"2a".repeat(32)).unwrap();
        let chain = CountingSubscriptionChain {
            submit_calls: std::sync::atomic::AtomicUsize::new(0),
        };

        let err = super::place_subscription_after_pool_preflight(
            &note_addr,
            chain.place_inference_subscription(&note, &keys, "model-hash", 10, 2, 20, true),
        )
        .await
        .expect_err("subscription placement must reject a missing recovery pool")
        .to_string();
        assert!(err.contains("require DEXDO_PN_POOL"), "{err}");
        assert_eq!(
            chain.submit_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "missing DEXDO_PN_POOL must block the escrow submit"
        );
    }

    /// regression: under buyer registry validation a raw `--token-contract` does not carry canonical
    /// order-book proof, so it must be rejected before escrow/place_buy. `--market` remains the explicit
    /// trusted path because the manifest carries the book checked by the registry preflight.
    #[test]
    fn buyer_registry_enabled_raw_token_contract_rejected_without_book_proof() {
        let err = super::reject_buyer_raw_token_contract_without_registry_book_proof(
            None,
            Some("0:badtc"),
            "qwen--qwen3--32b",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("raw --token-contract"), "{err}");
        assert!(err.contains("canonical order-book proof"), "{err}");
        assert!(err.contains("buyer.check_model_registry=true"), "{err}");

        let market_path = std::path::Path::new("market.json");
        assert!(
            super::reject_buyer_raw_token_contract_without_registry_book_proof(
                Some(market_path),
                None,
                "qwen--qwen3--32b",
            )
            .is_ok()
        );
        assert!(
            super::reject_buyer_raw_token_contract_without_registry_book_proof(
                None,
                None,
                "qwen--qwen3--32b",
            )
            .is_ok()
        );
    }

    /// released-style binaries must not need
    /// `contracts/compiled_0.79.3/airegistry/ModelRegistry.abi.json` in the current working directory just to
    /// resolve the buyer's content identity. The ABI source is embedded in `registry.rs`; this guard keeps the
    /// CLI from reintroducing the old `abi_path.exists()` bail.
    #[test]
    fn content_identity_resolution_uses_embedded_model_registry_abi() {
        let source = include_str!("buyer.rs");
        let start = source
            .find("async fn resolve_content_identity_model")
            .expect("content identity resolver present");
        let end = source[start..]
            .find("#[cfg(not(feature = \"shellnet\"))]")
            .map(|offset| start + offset)
            .expect("resolver end marker present");
        let body = &source[start..end];

        assert!(
            body.contains(
                "ShellnetModelRegistryReader::from_manifest(contracts, &registry_address)"
            ),
            "resolver must use the embedded-ABI ModelRegistry reader"
        );
        assert!(
            !body.contains("abi_path") && !body.contains(".exists()"),
            "resolver must not depend on a cwd/filesystem ABI path"
        );
        assert!(
            !body.contains("not committed in this branch"),
            "released binaries must not bail because ModelRegistry.abi.json is absent from cwd"
        );
    }

    #[test]
    fn buyer_content_identity_resolution_error_fails_closed_without_allow_flag() {
        let err = super::buyer_content_identity_resolution_result(
            "qwen--qwen3--32b",
            false,
            Err(anyhow::anyhow!("registry unreachable")),
        )
        .expect_err("strict buyer must fail closed on registry resolution failure")
        .to_string();

        assert!(err.contains("registry unreachable"), "{err}");
    }

    #[test]
    fn buyer_allow_unverified_model_degrades_resolution_error_to_name_only() {
        let identity = super::buyer_content_identity_resolution_result(
            "qwen--qwen3--32b",
            true,
            Err(anyhow::anyhow!("registry unreachable")),
        )
        .expect("allow-unverified buyer may continue on name-only evidence");

        assert_eq!(identity, None);
    }

    #[test]
    fn buyer_local_api_content_identity_preflights_before_any_buy() {
        let source = include_str!("buyer.rs");
        let start = source
            .find("let buyer_content_policy = if args.local_listen.is_some()")
            .expect("buyer content preflight present");
        let body = &source[start..];
        let preflight = body
            .find("build_buyer_content_policy")
            .expect("content policy helper called");
        let on_demand = body
            .find("run_buyer_on_demand_local_api")
            .expect("on-demand branch present");
        let direct_buy = body
            .find("buyer.place_buy(chain.as_ref(), &tc)")
            .expect("direct buy path present");
        let model_buy = body
            .find(".place_buy_by_model(")
            .expect("model-only buy path present");

        assert!(
            preflight < on_demand,
            "on-demand buyer must reject missing content-identity inputs before lazy buy/handover"
        );
        assert!(
            preflight < direct_buy && preflight < model_buy,
            "local API buyer must reject missing content-identity inputs before escrow/place_buy"
        );
    }

    #[test]
    fn buyer_content_identity_preflight_error_names_operator_input() {
        let err = dexdo::buyer::api::content_check_policy(
            "qwen--qwen3--32b",
            None,
            false,
            false,
            false,
            &dexdo::seller::ModelsConfig::empty(),
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "buyer content-identity preflight failed before buy: \
                 missing_or_unset=allow_unverified_model_or_models_data; {e}"
            )
        })
        .expect_err("strict name-only content identity must fail closed")
        .to_string();

        assert!(
            err.contains("missing_or_unset=allow_unverified_model_or_models_data"),
            "{err}"
        );
        assert!(err.contains("before buy"), "{err}");
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize the process-global current directory for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    #[ignore = "live : read-only released-style content identity resolution via embedded ModelRegistry ABI"]
    async fn live_content_identity_resolution_works_without_modelregistry_abi_file_in_cwd() {
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = CWD_LOCK.lock().unwrap();

        struct RestoreCwd {
            old: std::path::PathBuf,
            tmp: std::path::PathBuf,
        }

        impl Drop for RestoreCwd {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.old);
                let _ = std::fs::remove_dir_all(&self.tmp);
            }
        }

        let old = std::env::current_dir().expect("current cwd");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!(
            "dexdo-308-release-cwd-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&tmp).expect("create release-style cwd");
        let _restore = RestoreCwd {
            old,
            tmp: tmp.clone(),
        };
        std::env::set_current_dir(&tmp).expect("enter release-style cwd");

        let cwd_abi = tmp.join("contracts/compiled_0.79.3/airegistry/ModelRegistry.abi.json");
        assert!(
            !cwd_abi.exists(),
            "test cwd must not carry the ModelRegistry ABI file"
        );
        let contracts = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../contracts/deployed.shellnet.json");
        let identity = super::resolve_content_identity_model(&contracts, "qwen--qwen3--32b")
            .await
            .expect("resolve qwen content identity from embedded ModelRegistry ABI");
        assert_eq!(identity, "Qwen/Qwen3-32B");
        println!(
            "live  evidence: release-style cwd={} cwd_abi_absent=true frame_model=qwen--qwen3--32b identity={identity}",
            tmp.display()
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    #[ignore = "live -carry: bad ModelRegistry manifest fails strict and downgrades only with --allow-unverified-model"]
    async fn live_allow_unverified_model_downgrades_unreachable_registry_to_name_only() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!(
            "dexdo-307-bad-registry-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&tmp).expect("create scratch manifest dir");
        let _cleanup = TempDirCleanup(tmp.clone());

        let contracts = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../contracts/deployed.shellnet.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&contracts).expect("read contracts manifest"))
                .expect("parse contracts manifest");
        let bad_registry = "0:2222222222222222222222222222222222222222222222222222222222222222";
        manifest["model_registry"] = serde_json::Value::String(bad_registry.to_string());
        let scratch = tmp.join("deployed.bad-registry.json");
        std::fs::write(
            &scratch,
            serde_json::to_vec_pretty(&manifest).expect("serialize scratch manifest"),
        )
        .expect("write scratch manifest");

        let strict =
            super::resolve_buyer_content_identity_model(&scratch, "qwen--qwen3--32b", false)
                .await
                .expect_err("strict buyer must fail closed when ModelRegistry is unreachable")
                .to_string();
        assert!(strict.contains("ModelRegistry"), "{strict}");

        let allowed =
            super::resolve_buyer_content_identity_model(&scratch, "qwen--qwen3--32b", true)
                .await
                .expect("allow-unverified buyer may continue on name-only evidence");
        assert_eq!(allowed, None);
        println!(
            "live -carry evidence: scratch_manifest={} bad_registry={} strict_failed=true allow_unverified_name_only=true",
            scratch.display(),
            bad_registry
        );
    }

    /// machine-mode model-only buy must not emit `quote_selected` from executable discovery alone when
    /// the raw shellnet matcher cannot reach that ask.
    #[test]
    fn buyer_model_only_quote_selection_runs_submit_safe_preflight() {
        let source = include_str!("buyer.rs");
        let quote = source
            .find("async fn buyer_quote_selection")
            .expect("buyer quote helper present");
        let body = &source[quote..];
        let preflight = body
            .find("assert_model_buy_matches_executable_quote")
            .expect("model-only quote selection checks raw/executable submit safety");
        let discover = body
            .find("chain.discover_offers")
            .expect("buyer quote selection discovers offers");
        assert!(
            preflight < discover,
            "submit-safety preflight must run before executable discovery is rendered as quote_selected"
        );
    }

    /// the model-only buyer must validate the TC state immediately after its fill event and before
    /// waiting for the seller handover.
    #[test]
    fn model_only_buy_validates_match_state_before_handover_wait() {
        let source = include_str!("buyer.rs");
        let executor = source
            .find("async fn execute_buyer_quote_submit")
            .expect("durable buyer executor present");
        let executor_end = source[executor..]
            .find("fn record_buyer_token_contract_after_money_move")
            .map(|offset| executor + offset)
            .unwrap();
        let durable = &source[executor..executor_end];
        let wait_match = durable
            .find("wait_matched_token_contract")
            .expect("model-only buy waits for fill event");
        let validate = durable[wait_match..]
            .find("validate_reported_match_state")
            .map(|offset| wait_match + offset)
            .expect("model-only buy validates matched TC state");
        assert!(wait_match < validate);
        let buy = source.find("async fn run_buyer_inner").unwrap();
        let body = &source[buy..];
        let submit = body.find("execute_buyer_quote_submit(").unwrap();
        let handover = body
            .find("resolve_endpoint(chain.as_ref(), &token_contract)")
            .expect("buyer waits for handover");
        assert!(
            submit < handover,
            "matched TC state must be checked before handover wait"
        );
        assert!(
            body.contains("handover_timeout_diagnostic"),
            "handover timeout must re-read TC state for funded-never-opened recovery diagnostics"
        );
    }

    /// in machine mode, model-only buy submission is its own by-fact event. It must be emitted
    /// immediately after `place_buy_by_model` returns, before the process can block in fill/match polling.
    #[test]
    fn model_only_buy_submitted_is_emitted_before_match_wait_path() {
        let source = include_str!("buyer.rs");
        let executor = source
            .find("async fn execute_buyer_quote_submit")
            .expect("durable buyer executor present");
        let executor_end = source[executor..]
            .find("fn record_buyer_token_contract_after_money_move")
            .map(|offset| executor + offset)
            .unwrap();
        let segment = &source[executor..executor_end];
        let submit = segment.find("start_durable_buyer_submit(").unwrap();
        let buy_event = segment.find("on_submit_observed(").unwrap();
        let wait_match = segment.find("complete_buyer_submit_with_journal(").unwrap();
        assert!(
            submit < buy_event && buy_event < wait_match,
            "model-only buyer must emit buy_submitted after submit returns and before match wait"
        );
    }

    #[test]
    fn policy_cleanup_rechecks_state_after_wait_before_cleanup() {
        let source = include_str!("buyer.rs");
        let start = source
            .find("async fn policy_cleanup_unopened_after_match_timeout")
            .expect("policy cleanup helper present");
        let end = source[start..]
            .find("async fn apply_no_handover_after_match_policy")
            .map(|offset| start + offset)
            .expect("policy cleanup helper end marker present");
        let body = &source[start..end];
        let sleep = body
            .find("tokio::time::sleep")
            .expect("cleanup wait present");
        let recheck = body[sleep..]
            .find("validate_reported_match_state")
            .map(|offset| sleep + offset)
            .expect("state recheck after wait present");
        let cleanup = body
            .find("chain.cleanup_unopened")
            .expect("cleanup lever present");
        assert!(
            sleep < recheck && recheck < cleanup,
            "cleanup must re-read TC state after waiting and before cleanup_unopened"
        );
        assert!(
            body.contains("not_cleanup_unopened_state_after_wait"),
            "unexpected post-wait states must not be cleaned up silently"
        );
        assert!(
            body.contains("handover_opened_after_wait"),
            "late-opened deals must return to the handover path instead of failing cleanup"
        );
    }

    #[test]
    fn policy_buyer_failure_classes_dispatch_runtime_levers() {
        let source = include_str!("buyer.rs");
        let malformed = source
            .find("async fn apply_malformed_handover_policy")
            .expect("malformed handover policy helper present");
        let cleanup = source[malformed..]
            .find("async fn policy_cleanup_unopened_after_match_timeout")
            .map(|offset| malformed + offset)
            .expect("malformed helper end marker present");
        let malformed_body = &source[malformed..cleanup];
        assert!(
            malformed_body.contains("chain.seller_timeout(token_contract)"),
            "malformed_handover=reclaim must invoke the reclaim lever"
        );
        assert!(
            malformed_body.contains("chain.dispute(token_contract, buyer.note.as_ref())"),
            "malformed_handover=dispute must invoke stream dispute"
        );

        let buy = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let monitor = source[buy..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| buy + offset)
            .expect("run_buyer end marker present");
        let body = &source[buy..monitor];
        assert!(
            body.contains("is_malformed_handover_error(&e)")
                && body.contains("apply_malformed_handover_policy"),
            "run_buyer must route malformed/decrypt handovers through policy"
        );
        assert!(
            body.contains("apply_oneshot_dead_gateway_policy"),
            "one-shot buyer stream open/connect errors must route through dead_gateway policy"
        );
        assert!(
            body.contains("apply_oneshot_empty_stream_policy"),
            "one-shot buyer zero-token stream must route through empty_stream policy"
        );
    }

    /// re-review: the secret-bearing pool temp must be exclusive. A pre-created temp path
    /// (file or symlink) is not truncated/clobbered before the final atomic rename.
    #[cfg(feature = "shellnet")]
    #[test]
    fn write_pool_private_refuses_preexisting_temp_path() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-pool-temp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let target = dir.join("pn_pool.json");
        let tmp = dir.join(".pn_pool.json.tmp.preexisting");
        std::fs::write(&tmp, b"do-not-clobber").unwrap();

        let err = super::write_pool_private_via_temp(&target, &tmp, b"secret-pool")
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("create temp secret file"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&tmp).unwrap(), b"do-not-clobber");
        assert!(
            !target.exists(),
            "target must not be written after temp creation failed"
        );
    }

    /// regression: writers using different symlinks to one pool must share the canonical lock and
    /// target. The second writer re-reads the first result, so neither recovery key is lost.
    #[cfg(all(feature = "shellnet", unix))]
    #[test]
    fn concurrent_note_pool_writers_via_symlinks_preserve_both_notes() {
        fn state(seed_byte: u8, address_byte: char) -> crate::cli::note::OnboardPnState {
            let secret = format!("{seed_byte:02x}").repeat(32);
            let public = crate::cli::note::derive_owner_pubkey_from_secret_hex(&secret).unwrap();
            crate::cli::note::OnboardPnState {
                endpoint: "shellnet.ackinacki.org".into(),
                nominal: "N100".into(),
                token_type: 1,
                raw_value: 100_000_000_000,
                ecc_shell_deposit: 100_000_000_000,
                pn_address: Some(format!("0:{}", address_byte.to_string().repeat(64))),
                deposit_identifier_hash: Some(address_byte.to_string().repeat(64)),
                owner_public_key_hex: Some(public),
                owner_secret_key_hex: Some(secret),
                deployed_at_unix: Some(1_000),
                shell_funded: true,
                sanity_checked: true,
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "dexdo-pool-concurrent-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        let wallet = format!("0:{}", "c".repeat(64));
        let initial_state = state(0x1a, 'd');
        let initial_note = crate::cli::note::pn_state_to_pool_note(&initial_state).unwrap();
        let initial_pool = crate::cli::note::pool_with_note_added(
            None,
            &initial_state,
            initial_note,
            1_000,
            &wallet,
        )
        .unwrap();
        std::fs::write(&pool_path, serde_json::to_vec(&initial_pool).unwrap()).unwrap();
        let first_alias = dir.join("first-pool.json");
        let second_alias = dir.join("second-pool.json");
        std::os::unix::fs::symlink(&pool_path, &first_alias).unwrap();
        std::os::unix::fs::symlink(&pool_path, &second_alias).unwrap();
        let first_state = state(0x2a, 'a');
        let second_state = state(0x3a, 'b');

        let (first_read_tx, first_read_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let first_pool = first_alias;
        let first_wallet = wallet.clone();
        let first = std::thread::spawn(move || {
            super::with_pool_write_lock(&first_pool, |first_pool| {
                super::note_deploy_fold_state_into_pool_locked(
                    first_pool,
                    &first_state,
                    &first_wallet,
                    || {
                        first_read_tx.send(()).unwrap();
                        release_first_rx.recv().unwrap();
                    },
                )
            })
            .unwrap();
        });
        first_read_rx.recv().unwrap();

        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let (second_done_tx, second_done_rx) = std::sync::mpsc::channel();
        let second_pool = second_alias;
        let second = std::thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            super::note_deploy_fold_state_into_pool(&second_pool, &second_state, &wallet).unwrap();
            second_done_tx.send(()).unwrap();
        });
        second_started_rx.recv().unwrap();
        let completed_while_first_writer_was_paused = second_done_rx
            .recv_timeout(std::time::Duration::from_millis(250))
            .is_ok();

        release_first_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert!(
            !completed_while_first_writer_was_paused,
            "the second writer entered the pool read-modify-write while the first held the lock"
        );

        let pool = super::load_pool_json(&pool_path).unwrap();
        let addresses = pool["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["address"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            addresses.len(),
            3,
            "both concurrently added notes must survive"
        );
        assert!(addresses.contains(format!("0:{}", "a".repeat(64)).as_str()));
        assert!(addresses.contains(format!("0:{}", "b".repeat(64)).as_str()));
    }

    /// negative regression: pool targets and lock sentinels must be regular files.
    #[cfg(all(feature = "shellnet", unix))]
    #[test]
    fn pool_and_lock_non_regular_sentinels_are_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-pool-nonregular-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());

        let pool_directory = dir.join("pool-directory");
        std::fs::create_dir(&pool_directory).unwrap();
        let err = super::with_pool_write_lock(&pool_directory, |_| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("regular file"), "{err}");

        let pool = dir.join("pn_pool.json");
        std::fs::write(&pool, br#"{"notes":[]}"#).unwrap();
        let lock = dir.join("pn_pool.json.lock");
        std::os::unix::fs::symlink(&pool, &lock).unwrap();
        let err = super::with_pool_write_lock(&pool, |_| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pool lock"), "{err}");
        assert!(err.contains("regular file"), "{err}");
    }

    /// regression: `DEXDO_PN_POOL=<same existing file> dexdo note deploy --pool <same file>` is the
    /// reported footgun. Refuse before chain work, so a bad append cannot silently poison the active pool.
    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_rejects_same_file_env_pool_append() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-same-pool-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool = dir.join("pn_pool.json");
        let other = dir.join("other_pool.json");
        std::fs::write(&pool, br#"{"notes":[]}"#).unwrap();
        std::fs::write(&other, br#"{"notes":[]}"#).unwrap();

        let err = super::note_deploy_same_file_pool_guard(Some(pool.as_os_str()), &pool)
            .unwrap_err()
            .to_string();

        assert!(err.contains("DEXDO_PN_POOL"), "{err}");
        assert!(err.contains("--pool"), "{err}");
        assert!(err.contains("ERR_INVALID_SENDER 101"), "{err}");
        assert!(err.contains("--pool <new_file>"), "{err}");
        super::note_deploy_same_file_pool_guard(Some(other.as_os_str()), &pool)
            .expect("different existing pool file is allowed");
        super::note_deploy_same_file_pool_guard(None, &pool).expect("unset env pool is allowed");
    }

    /// PR347 review blocker regression: a stale active pool must fail before the money-moving model buy call.
    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn stale_pool_preflight_blocks_model_buy_before_chain_call() {
        use std::sync::atomic::Ordering;

        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "dexdo-stale-pool-preflight-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool = dir.join("pn_pool.json");
        let stale_note = format!("0:{}", "1".repeat(64));
        let buyer_note = format!("0:{}", "2".repeat(64));
        std::fs::write(
            &pool,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": stale_note,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let chain = RecordingRecoveryChain::default();
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));

        let err = super::place_buy_by_model_after_pool_preflight(
            &chain,
            &buyer,
            true,
            Some(&buyer_note),
            1,
            1,
            1,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("no note entry"), "{err}");
        assert_eq!(
            chain.place_next_calls.load(Ordering::SeqCst),
            0,
            "stale pool must fail before place_buy_by_model moves escrow"
        );
    }

    /// regression: a direct note identity without DEXDO_PN_POOL must fail before escrow moves.
    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn missing_pool_preflight_blocks_model_buy_before_chain_call() {
        use std::sync::atomic::Ordering;

        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let _env = EnvVarGuard::unset("DEXDO_PN_POOL");
        let buyer_note = format!("0:{}", "2".repeat(64));
        let chain = RecordingRecoveryChain::default();
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));

        let err = super::place_buy_by_model_after_pool_preflight(
            &chain,
            &buyer,
            true,
            Some(&buyer_note),
            1,
            1,
            1,
        )
        .await
        .expect_err("missing pool must fail before model buy")
        .to_string();

        assert!(err.contains("require DEXDO_PN_POOL"), "{err}");
        assert_eq!(
            chain.place_next_calls.load(Ordering::SeqCst),
            0,
            "missing pool must fail before place_buy_by_model moves escrow"
        );
    }

    #[tokio::test]
    async fn model_only_buy_preserves_typed_chain_errors_for_classification() {
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));

        for (failure, expected_code, expected_cause) in [
            (
                ModelBuyFailure::Transport,
                crate::cli::machine::ErrorCode::ChainTransport,
                "model-only transport cause",
            ),
            (
                ModelBuyFailure::Contract,
                crate::cli::machine::ErrorCode::ChainRevert,
                "model-only contract cause",
            ),
        ] {
            let chain = RecordingRecoveryChain {
                model_buy_failure: Some(failure),
                ..RecordingRecoveryChain::default()
            };
            let err = super::place_buy_by_model_after_pool_preflight(
                &chain, &buyer, false, None, 1, 1, 1,
            )
            .await
            .expect_err("typed model-only buy failure must propagate");

            assert_eq!(
                crate::cli::machine::classify_error(crate::cli::machine::OP_BUYER_START, &err,),
                expected_code
            );
            assert!(
                err.chain().any(|cause| cause
                    .downcast_ref::<dexdo_core::ChainError>()
                    .is_some_and(|chain_error| chain_error.to_string().contains(expected_cause))),
                "typed cause missing from anyhow chain: {err:#}"
            );
        }
    }

    /// residual: recovery/reclaim can be driven from the pool file alone once the buyer has recorded the
    /// matched TokenContract next to the note entry.
    #[cfg(feature = "shellnet")]
    #[test]
    fn recovery_inputs_can_use_pool_only() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-recovery-pool-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        let note_addr = format!("0:{}", "1".repeat(64));
        let token_contract = format!("0:{}", "2".repeat(64));
        let secret = "2a".repeat(32);
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": note_addr,
                    "owner_secret_key_hex": secret,
                    "token_contract": token_contract,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 99
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let resolved = super::resolve_pool_recovery_inputs(
            "reclaim",
            &IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: None,
            },
            None,
            None,
            Some(pool_path.as_path()),
        )
        .unwrap();

        assert_eq!(resolved.note_addr, format!("0:{}", "1".repeat(64)));
        assert_eq!(resolved.note_secret_hex, "2a".repeat(32));
        assert_eq!(resolved.token_contract, format!("0:{}", "2".repeat(64)));
    }

    /// regression: pool-only recovery must retain the path resolved before STOP even if its symlink alias
    /// is retargeted before the recovery record is persisted.
    #[cfg(all(feature = "shellnet", unix))]
    #[test]
    fn pool_recovery_persists_to_the_initially_resolved_symlink_target() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-recovery-pool-retarget-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let original_pool = dir.join("original-pool.json");
        let retargeted_pool = dir.join("retargeted-pool.json");
        let pool_alias = dir.join("pn_pool.json");
        let note_addr = format!("0:{}", "1".repeat(64));
        let token_contract = format!("0:{}", "2".repeat(64));
        let secret = "2a".repeat(32);
        let pool_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "notes": [{
                "address": note_addr,
                "owner_secret_key_hex": secret,
                "token_contract": token_contract,
                "token_contract_role": "buyer",
                "token_contract_updated_at_unix": 99
            }]
        }))
        .unwrap();
        std::fs::write(&original_pool, &pool_bytes).unwrap();
        std::fs::write(&retargeted_pool, &pool_bytes).unwrap();
        std::os::unix::fs::symlink(&original_pool, &pool_alias).unwrap();

        let resolved = super::resolve_pool_recovery_inputs(
            "recover",
            &IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: None,
            },
            None,
            None,
            Some(pool_alias.as_path()),
        )
        .unwrap();
        let record = resolved.pool_record.unwrap();
        assert_eq!(
            record.pool_path,
            std::fs::canonicalize(&original_pool).unwrap()
        );

        std::fs::remove_file(&pool_alias).unwrap();
        std::os::unix::fs::symlink(&retargeted_pool, &pool_alias).unwrap();
        super::persist_pool_recovery_record(&record).unwrap();

        let original = super::load_pool_json(&original_pool).unwrap();
        assert_ne!(
            original["notes"][0]["token_contract_updated_at_unix"],
            serde_json::json!(99)
        );
        assert_eq!(std::fs::read(&retargeted_pool).unwrap(), pool_bytes);
    }

    /// recovery-key safety: a record changed after resolution must remain byte-for-byte untouched.
    #[cfg(feature = "shellnet")]
    #[test]
    fn pool_recovery_persistence_refuses_a_changed_key_record() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-recover-key-safety-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        let note_addr = format!("0:{}", "1".repeat(64));
        let token_contract = format!("0:{}", "2".repeat(64));
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "notes": [{
                "address": note_addr,
                "owner_secret_key_hex": "3b".repeat(32),
                "token_contract": token_contract,
                "token_contract_role": "buyer",
                "token_contract_updated_at_unix": 11
            }]
        }))
        .unwrap();
        std::fs::write(&pool_path, &bytes).unwrap();

        let err = super::persist_pool_recovery_record(&super::PoolRecoveryRecord {
            pool_path: pool_path.clone(),
            note_addr,
            note_secret_hex: "2a".repeat(32),
            token_contract,
            role: "buyer".to_string(),
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("wrong-key or changed record"), "{err}");
        assert_eq!(std::fs::read(pool_path).unwrap(), bytes);
    }

    /// regression: buyer-only recovery ignores seller records while preserving legacy records without a role.
    #[cfg(feature = "shellnet")]
    #[test]
    fn recovery_inputs_select_buyer_role_and_keep_legacy_unknown() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-recovery-pool-role-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        let note_addr = format!("0:{}", "1".repeat(64));
        let buyer_tc = format!("0:{}", "2".repeat(64));
        let seller_tc = format!("0:{}", "3".repeat(64));
        let secret = "2a".repeat(32);

        for buyer_role in [Some("buyer"), None] {
            let mut buyer_note = serde_json::json!({
                "address": note_addr,
                "owner_secret_key_hex": secret,
                "token_contract": buyer_tc,
            });
            if let Some(role) = buyer_role {
                buyer_note["token_contract_role"] = serde_json::json!(role);
            }
            std::fs::write(
                &pool_path,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "notes": [
                        {
                            "address": note_addr,
                            "owner_secret_key_hex": secret,
                            "token_contract": seller_tc,
                            "token_contract_role": "seller"
                        },
                        buyer_note
                    ]
                }))
                .unwrap(),
            )
            .unwrap();

            let resolved = super::resolve_pool_recovery_inputs(
                "reclaim",
                &IdentityArgs {
                    note_key: None,
                    note_index: 0,
                    note_addr: None,
                },
                None,
                None,
                Some(pool_path.as_path()),
            )
            .unwrap();
            assert_eq!(resolved.note_addr, note_addr);
            assert_eq!(resolved.token_contract, buyer_tc);
        }
    }

    /// negative: pool-only recovery must not guess when several note entries carry TokenContracts.
    #[cfg(feature = "shellnet")]
    #[test]
    fn recovery_inputs_reject_ambiguous_pool() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-recovery-pool-ambiguous-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [
                    {
                        "address": format!("0:{}", "1".repeat(64)),
                        "owner_secret_key_hex": "2a".repeat(32),
                        "token_contract": format!("0:{}", "2".repeat(64))
                    },
                    {
                        "address": format!("0:{}", "3".repeat(64)),
                        "owner_secret_key_hex": "3a".repeat(32),
                        "token_contract": format!("0:{}", "4".repeat(64))
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let err = super::resolve_pool_recovery_inputs(
            "recover",
            &IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: None,
            },
            None,
            None,
            Some(pool_path.as_path()),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("disambiguate"), "{err}");
    }

    /// regression: the recovery state and final pool are different JSON formats; first-run absent paths
    /// must still reject an accidental same path before any wallet spend.
    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_rejects_same_recovery_and_pool_path() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-recovery-pool-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool = dir.join("pn_pool.json");
        let recovery = dir.join("pn_pool.json.recovery.json");

        let err = super::note_deploy_recovery_pool_guard(&pool, &pool)
            .unwrap_err()
            .to_string();

        assert!(err.contains("--recovery"), "{err}");
        assert!(err.contains("--pool"), "{err}");
        assert!(err.contains("DEXDO_PN_POOL"), "{err}");
        super::note_deploy_recovery_pool_guard(&pool, &recovery)
            .expect("separate recovery and pool paths are allowed");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_endpoint_url_accepts_bare_host_or_url() {
        assert_eq!(
            super::note_endpoint_url("shellnet.ackinacki.org").unwrap(),
            "https://shellnet.ackinacki.org"
        );
        assert_eq!(
            super::note_endpoint_url("https://shellnet.ackinacki.org/").unwrap(),
            "https://shellnet.ackinacki.org"
        );
        assert!(super::note_endpoint_url("  ").is_err());
    }

    #[cfg(feature = "shellnet")]
    fn note_deploy_args(
        multisig_key: Option<std::path::PathBuf>,
        multisig_seed_file: Option<std::path::PathBuf>,
    ) -> NoteDeployArgs {
        NoteDeployArgs {
            multisig_address: format!("0:{}", "1".repeat(64)),
            multisig_key,
            multisig_seed_file,
            nominal: "N100".into(),
            token_type: "nackl".into(),
            endpoint: "shellnet.ackinacki.org".into(),
            pool: std::path::PathBuf::from("pn_pool.json"),
            recovery: None,
            simulate_interrupt_after_spend_before_pool: false,
            simulate_interrupt_after_deposit_voucher_submit: false,
            simulate_interrupt_after_deposit_voucher_event: false,
            simulate_interrupt_after_shell_voucher_submit: false,
            simulate_interrupt_after_deploy_before_note_record: false,
        }
    }

    #[cfg(feature = "shellnet")]
    fn tvm_tonos_fixture_phrase() -> String {
        const WORD_INDICES: [u16; 12] = [
            1636, 1293, 905, 102, 1057, 1956, 1247, 1750, 597, 881, 1302, 3,
        ];
        WORD_INDICES
            .iter()
            .map(|i| bip39::Language::English.wordlist().get_word((*i).into()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(feature = "shellnet")]
    fn pinned_tvm_sdk_default_key(phrase: &str) -> tvm_client::crypto::KeyPair {
        assert_eq!(
            tvm_client::crypto::default_hdkey_derivation_path(),
            dexdo::wallet_seed::TVM_DEFAULT_DERIVATION_PATH
        );
        let context = std::sync::Arc::new(
            tvm_client::ClientContext::new(tvm_client::ClientConfig::default()).unwrap(),
        );
        tvm_client::crypto::mnemonic_derive_sign_keys(
            context,
            tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys {
                phrase: phrase.to_owned(),
                path: None,
                dictionary: None,
                word_count: None,
            },
        )
        .unwrap()
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_seed_file_matches_key_file_input() {
        let phrase = tvm_tonos_fixture_phrase();
        let expected_key = pinned_tvm_sdk_default_key(&phrase);
        let dir = std::env::temp_dir().join(format!(
            "dexdo-note-deploy-seed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let key_path = dir.join("wallet.secret.hex");
        let seed_path = dir.join("wallet.seed");
        std::fs::write(&key_path, &expected_key.secret).unwrap();
        std::fs::write(&seed_path, phrase).unwrap();

        let (key_source, key_secret) =
            super::note_deploy_multisig_secret_hex(&note_deploy_args(Some(key_path), None))
                .unwrap();
        let (seed_source, seed_secret) =
            super::note_deploy_multisig_secret_hex(&note_deploy_args(None, Some(seed_path)))
                .unwrap();

        assert_eq!(key_source, "--multisig-key");
        assert_eq!(seed_source, "--multisig-seed-file");
        assert!(
            key_secret == expected_key.secret,
            "key-file input does not match pinned TVM SDK default secret"
        );
        assert!(
            seed_secret == expected_key.secret,
            "seed-file input does not match pinned TVM SDK default secret"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_seed_file_errors_do_not_echo_seed_input() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-note-deploy-invalid-seed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let key_path = dir.join("wallet.secret.hex");
        let seed_path = dir.join("wallet.seed");
        let invalid = std::iter::repeat_n("zzzz", 12)
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(&key_path, "00").unwrap();
        std::fs::write(&seed_path, &invalid).unwrap();

        let err = super::note_deploy_multisig_secret_hex(&note_deploy_args(
            Some(key_path),
            Some(seed_path.clone()),
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("only one"), "{err}");

        let err = super::note_deploy_multisig_secret_hex(&note_deploy_args(None, Some(seed_path)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid seed phrase"), "{err}");
        assert!(!err.contains(&invalid), "{err}");

        let missing = dir.join("missing.seed");
        let err = super::note_deploy_multisig_secret_hex(&note_deploy_args(None, Some(missing)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("read --multisig-seed-file"), "{err}");
    }

    #[test]
    fn buyer_renewal_monitor_uses_planner_and_recovery_actions() {
        let source = include_str!("buyer.rs");
        let start = source
            .find("fn spawn_buyer_service_renewal")
            .expect("renewal task present");
        let end = source[start..]
            .find("pub(crate) async fn run_buyer")
            .map(|offset| start + offset)
            .expect("renewal task end marker present");
        let body = &source[start..end];
        assert!(body.contains("BuyerContinuity"), "{body}");
        assert!(body.contains("planner.tick_with_mode"), "{body}");
        assert!(body.contains("continuity_mode"), "{body}");
        assert!(body.contains("has_active_or_recent_request"), "{body}");
        assert!(body.contains("CONSUMER_DEMAND_RECENT_SECS"), "{body}");
        assert!(body.contains("deal_state"), "{body}");
        assert!(body.contains("cleanup_unopened"), "{body}");
        assert!(body.contains("execute_buyer_monitor_recovery"), "{body}");
        assert!(source.contains("chain.seller_timeout"), "{source}");
        assert!(body.contains("RENEWAL_FAILURE_BACKOFF_SECS"), "{body}");
        assert!(body.contains("prepare_retry"), "{body}");
        assert!(!body.contains("pending_for"), "{body}");
    }

    #[derive(Clone, Copy)]
    enum ModelBuyFailure {
        Transport,
        Contract,
    }

    #[derive(Default)]
    struct RecordingRecoveryChain {
        cleanup_calls: std::sync::atomic::AtomicUsize,
        reclaim_calls: std::sync::atomic::AtomicUsize,
        dispute_calls: std::sync::atomic::AtomicUsize,
        release_calls: std::sync::atomic::AtomicUsize,
        stop_calls: std::sync::atomic::AtomicUsize,
        place_next_calls: std::sync::atomic::AtomicUsize,
        wait_match_calls: std::sync::atomic::AtomicUsize,
        deal_state: Option<dexdo_core::DealChainState>,
        snapshot: Option<dexdo_core::StreamSnapshot>,
        next_match: Option<dexdo_core::TokenContract>,
        model_buy_failure: Option<ModelBuyFailure>,
        stop_error: Option<String>,
        subscription_placements: Vec<dexdo_core::InferenceSubscriptionPlacement>,
        subscription_fills: std::sync::Mutex<Vec<(u128, dexdo_core::MatchedFill)>>,
        subscription_placement_calls: std::sync::atomic::AtomicUsize,
        subscription_placement_error: bool,
        subscription_order_active: bool,
    }

    impl RecordingRecoveryChain {
        fn with_deal_state(state: dexdo_core::DealChainState) -> Self {
            Self {
                deal_state: Some(state),
                next_match: Some("tc-next".to_string()),
                ..Self::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl dexdo_core::ChainBackend for RecordingRecoveryChain {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn place_buy(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn read_match(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn open_stream(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn read_handover(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn advance_tick(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn accept_probe(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn stop(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(error) = &self.stop_error {
                return Err(dexdo_core::ChainError::Transport(error.clone()));
            }
            Ok(dexdo_core::Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            })
        }

        async fn dispute(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.dispute_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(dexdo_core::Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            })
        }

        async fn release_dispute(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.release_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(dexdo_core::Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            })
        }

        async fn seller_timeout(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.reclaim_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            })
        }

        async fn cleanup_unopened(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.cleanup_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            })
        }

        async fn place_buy_by_model(
            &self,
            _note: &dyn dexdo_core::Note,
            _ticks: u128,
            _max_price_per_tick: u128,
            _escrow: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            self.place_next_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.model_buy_failure {
                Some(ModelBuyFailure::Transport) => Err(dexdo_core::ChainError::Transport(
                    "model-only transport cause".to_string(),
                )),
                Some(ModelBuyFailure::Contract) => Err(dexdo_core::ChainError::Contract(
                    "model-only contract cause".to_string(),
                )),
                None => Ok(()),
            }
        }

        async fn wait_matched_token_contract(
            &self,
            _since_unix: i64,
            _timeout: std::time::Duration,
        ) -> Result<Option<dexdo_core::MatchedFill>, dexdo_core::ChainError> {
            self.wait_match_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(dexdo_core::MatchedFill {
                token_contract: self
                    .next_match
                    .clone()
                    .unwrap_or_else(|| "tc-next".to_string()),
                ticks: 1,
                price_per_tick: 1,
            }))
        }

        async fn poll_attributed_model_buys_for_order_book(
            &self,
            _order_book: &str,
            _cursor: &mut dexdo_core::MatchWatchCursor,
        ) -> Result<Vec<(u128, dexdo_core::MatchedFill)>, dexdo_core::ChainError> {
            Ok(std::mem::take(
                &mut *self.subscription_fills.lock().unwrap(),
            ))
        }

        async fn subscription_placements_since(
            &self,
            _order_book: &str,
            _buyer_note: &str,
            _order_id_floor: u128,
            _max_price_per_tick: u128,
            _ticks: u128,
            _cycle_budget: u128,
            _auto_renew: bool,
        ) -> Result<Vec<dexdo_core::InferenceSubscriptionPlacement>, dexdo_core::ChainError>
        {
            self.subscription_placement_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.subscription_placement_error {
                return Err(dexdo_core::ChainError::Transport(
                    "injected placement read ambiguity".to_string(),
                ));
            }
            Ok(self.subscription_placements.clone())
        }

        async fn buyer_order_is_active_for_owner(
            &self,
            _order_book: &str,
            _order_id: u128,
            _buyer_note: &str,
        ) -> Result<bool, dexdo_core::ChainError> {
            Ok(self.subscription_order_active)
        }

        async fn deal_state(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<dexdo_core::DealChainState>, dexdo_core::ChainError> {
            Ok(self.deal_state)
        }

        async fn snapshot(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            self.snapshot.clone()
        }
    }

    #[test]
    fn buyer_fill_caller_rejects_wrong_tc_ticks_and_price() {
        let expected = dexdo_core::QuoteFill {
            order_id: 7,
            token_contract: "tc-intended".to_string(),
            ticks: 2,
            price_per_tick: 700,
            cost_with_fee: 0,
        };
        for fill in [
            dexdo_core::MatchedFill {
                token_contract: "tc-wrong".to_string(),
                ticks: 2,
                price_per_tick: 700,
            },
            dexdo_core::MatchedFill {
                token_contract: "tc-intended".to_string(),
                ticks: 3,
                price_per_tick: 700,
            },
            dexdo_core::MatchedFill {
                token_contract: "tc-intended".to_string(),
                ticks: 2,
                price_per_tick: 701,
            },
        ] {
            let error = super::correlated_buy_token_contract(fill, Some(&expected), 2, 900)
                .expect_err("wrong fill terms must fail closed at the caller");
            assert!(error
                .to_string()
                .contains("refusing wrong-fill attribution"));
        }
    }

    #[tokio::test]
    async fn one_shot_completion_propagates_stop_failure() {
        use std::sync::atomic::Ordering;

        let chain = std::sync::Arc::new(RecordingRecoveryChain {
            stop_error: Some("injected one-shot STOP failure".to_string()),
            ..Default::default()
        });
        let session = dexdo::buyer::api::SessionSettle::new(
            chain.clone(),
            "tc-one-shot".to_string(),
            std::sync::Arc::new(dexdo_core::LocalNote::generate()),
        );

        let error = super::settle_completed_oneshot(&session)
            .await
            .expect_err("one-shot success must not hide a failed STOP");

        assert!(
            error.to_string().contains("one-shot STOP failure"),
            "{error:#}"
        );
        assert!(!session.is_settled());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);

        drop(session);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            1,
            "one-shot awaited STOP failure must not trigger a detached retry"
        );
    }

    fn err_not_open() -> dexdo_core::ChainError {
        dexdo_core::ChainError::Contract(
            "block manager rejected message code=TVM_ERROR; exit_code=320 \
             (airegistry::ERR_NOT_OPEN) stage=data"
                .to_string(),
        )
    }

    #[test]
    fn err_not_open_recognizes_production_contract_and_legacy_chain_shapes() {
        assert!(super::is_err_not_open(&err_not_open()));
        assert!(super::is_err_not_open(&dexdo_core::ChainError::Chain(
            "exit_code=320 (airegistry::ERR_NOT_OPEN)".to_string()
        )));
        assert!(!super::is_err_not_open(&dexdo_core::ChainError::Transport(
            "exit_code=320".to_string()
        )));
        for message in [
            "exit_code=320.",
            "exit_code=320: stage",
            "exit_code=320!",
            "exit_code=320(x)",
        ] {
            assert!(
                super::is_err_not_open(&dexdo_core::ChainError::Contract(message.to_string())),
                "must classify {message:?} as exact ERR_NOT_OPEN"
            );
        }
        for message in [
            "exit_code=3200",
            "exit_code=3201",
            "exit_code=32",
            "exit_code=320suffix",
            "exit_code=320.5",
            "exit_code=320:5",
            "airegistry::ERR_NOT_OPENED",
            "xairegistry::ERR_NOT_OPEN",
            "exit_code=3200 (airegistry::ERR_NOT_OPEN)",
            "exit_code=321; previous exit_code=320",
            "exit_code=320; code=321; airegistry::ERR_NOT_OPEN",
            "exit_code=320; code=320; airegistry::ERR_NOT_OPEN",
            "code=321; airegistry::ERR_NOT_OPEN",
            "exit code 321; airegistry::ERR_NOT_OPEN",
            "action_result_code=321; airegistry::ERR_NOT_OPEN",
            "exit_code=320; result_code=321; airegistry::ERR_NOT_OPEN",
            "exit_code=320; resultCode=321; airegistry::ERR_NOT_OPEN",
            "exit_code=320; actionResultCode=321; airegistry::ERR_NOT_OPEN",
        ] {
            assert!(
                !super::is_err_not_open(&dexdo_core::ChainError::Contract(message.to_string())),
                "must not classify {message:?} as exact ERR_NOT_OPEN"
            );
        }
        assert!(super::is_err_not_open(&dexdo_core::ChainError::Contract(
            "airegistry::ERR_NOT_OPEN".to_string()
        )));
        assert!(super::is_err_not_open(&dexdo_core::ChainError::Contract(
            "exit_code=320; previous exit_code=320".to_string()
        )));
        assert!(super::is_err_not_open(&dexdo_core::ChainError::Contract(
            "exitCode=320; result_code=0; airegistry::ERR_NOT_OPEN".to_string()
        )));
    }

    fn deal_state(
        funded: bool,
        opened: bool,
        disputed: bool,
        probe_accepted: bool,
    ) -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded,
            opened,
            disputed,
            probe_accepted,
            funded_time: Some(100),
            last_advance: 0,
        }
    }

    fn stream_snapshot(
        buyer_locked: u64,
        buyer_lead: u64,
        seller_locked: u64,
        seller_received: u64,
        burned: u64,
    ) -> dexdo_core::StreamSnapshot {
        dexdo_core::StreamSnapshot {
            seller_locked,
            buyer_locked,
            buyer_lead,
            seller_received,
            buyer_refunded: 0,
            burned,
            closed: false,
        }
    }

    #[tokio::test]
    async fn post_reject_err_not_open_never_opened_no_money_is_terminal() {
        let chain = RecordingRecoveryChain {
            deal_state: Some(deal_state(true, false, false, false)),
            snapshot: Some(stream_snapshot(0, 0, 0, 0, 0)),
            ..Default::default()
        };

        let disposition = super::classify_by_fact_advance_failure(
            &chain,
            &"tc-safe".to_string(),
            &err_not_open(),
        )
        .await
        .expect("classification reads by-fact state");

        match disposition {
            super::AdvanceFailureDisposition::BenignTerminal { reason } => {
                assert!(reason.contains("reason=err_not_open_unopened_no_money"));
                assert!(reason.contains("opened=false"));
                assert!(reason.contains("probe_accepted=false"));
                assert!(reason.contains("disputed=false"));
                assert!(reason.contains("buyer_locked=0"));
                assert!(reason.contains("buyer_lead=0"));
                assert!(reason.contains("seller_locked=0"));
                assert!(reason.contains("finalized_owed=0"));
                assert!(reason.contains("burned=0"));
            }
            other => panic!("expected benign terminal ERR_NOT_OPEN, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn err_not_open_opened_probe_disputed_or_money_at_risk_remains_fault() {
        for (name, state) in [
            ("opened_probe", deal_state(true, true, false, false)),
            ("streaming", deal_state(true, true, false, true)),
            ("disputed", deal_state(true, false, true, false)),
        ] {
            let chain = RecordingRecoveryChain {
                deal_state: Some(state),
                snapshot: Some(stream_snapshot(0, 0, 0, 0, 0)),
                ..Default::default()
            };

            let disposition = super::classify_by_fact_advance_failure(
                &chain,
                &format!("tc-{name}"),
                &err_not_open(),
            )
            .await
            .expect("classification reads by-fact state");

            match disposition {
                super::AdvanceFailureDisposition::Fault { reason } => {
                    assert!(
                        reason.contains("reason=unsafe_lifecycle"),
                        "{name}: {reason}"
                    );
                }
                other => panic!("expected {name} ERR_NOT_OPEN to remain fatal, got {other:?}"),
            }
        }

        for (name, snapshot) in [
            ("buyer_locked", stream_snapshot(1, 0, 0, 0, 0)),
            ("buyer_lead", stream_snapshot(0, 1, 0, 0, 0)),
            ("seller_locked", stream_snapshot(0, 0, 1, 0, 0)),
            ("finalized_owed", stream_snapshot(0, 0, 0, 1, 0)),
            ("burned", stream_snapshot(0, 0, 0, 0, 1)),
        ] {
            let chain = RecordingRecoveryChain {
                deal_state: Some(deal_state(true, false, false, false)),
                snapshot: Some(snapshot),
                ..Default::default()
            };
            let disposition = super::classify_by_fact_advance_failure(
                &chain,
                &format!("tc-{name}"),
                &err_not_open(),
            )
            .await
            .expect("classification reads by-fact state");

            match disposition {
                super::AdvanceFailureDisposition::Fault { reason } => {
                    assert!(
                        reason.contains("reason=money_or_locks_present"),
                        "{name}: {reason}"
                    );
                }
                other => panic!("expected {name} ERR_NOT_OPEN to remain fatal, got {other:?}"),
            }
        }
    }

    fn ready_funded_never_opened_state() -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: true,
            opened: false,
            disputed: false,
            probe_accepted: false,
            funded_time: Some(1),
            last_advance: 0,
        }
    }

    fn disputed_deal_state() -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: true,
            opened: true,
            disputed: true,
            probe_accepted: true,
            funded_time: Some(1),
            last_advance: 100,
        }
    }

    fn seller_policy(
        after_deal_done: crate::cli::policy::SellerAfterDealDoneAction,
        buyer_no_show: crate::cli::policy::SellerBuyerNoShowAction,
        dispute_against_me: crate::cli::policy::SellerDisputeAgainstMeAction,
    ) -> crate::cli::policy::SellerRuntimePolicy {
        crate::cli::policy::SellerRuntimePolicy {
            after_deal_done,
            buyer_no_show,
            dispute_against_me,
            max_open_deals: 1,
        }
    }

    fn assert_seller_policy_startup_fails_closed(
        policy: crate::cli::policy::SellerRuntimePolicy,
        expected_choice: &str,
    ) {
        let err = super::enforce_seller_runtime_policy(&policy)
            .unwrap_err()
            .to_string();

        assert!(err.contains("failure_class=policy_validation"), "{err}");
        assert!(err.contains("action=fail_closed"), "{err}");
        assert!(err.contains("token_contract=<not-posted>"), "{err}");
        assert!(err.contains("state=pre_offer"), "{err}");
        assert!(err.contains("result=unsupported_policy_choice"), "{err}");
        assert!(err.contains("next_action=edit_policy"), "{err}");
        assert!(err.contains(expected_choice), "{err}");
    }

    #[test]
    fn policy_seller_after_done_republish_fails_closed_before_offer() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Republish,
            crate::cli::policy::SellerBuyerNoShowAction::RetireGateway,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        assert_seller_policy_startup_fails_closed(policy, "seller.on.after_deal_done=republish");
    }

    #[test]
    fn policy_seller_after_done_republish_with_backoff_fails_closed_before_offer() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::RepublishWithBackoff,
            crate::cli::policy::SellerBuyerNoShowAction::RetireGateway,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        assert_seller_policy_startup_fails_closed(
            policy,
            "seller.on.after_deal_done=republish_with_backoff",
        );
    }

    #[test]
    fn policy_seller_buyer_no_show_cleanup_and_republish_fails_closed_before_offer() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::CleanupAndRepublish,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        assert_seller_policy_startup_fails_closed(
            policy,
            "seller.on.buyer_no_show=cleanup_and_republish",
        );
    }

    #[test]
    fn policy_seller_buyer_no_show_cleanup_and_retire_fails_closed_before_offer() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::CleanupAndRetire,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        assert_seller_policy_startup_fails_closed(
            policy,
            "seller.on.buyer_no_show=cleanup_and_retire",
        );
    }

    #[test]
    fn policy_seller_complete_supported_policy_passes_startup_before_offer() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::RetireGateway,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        super::enforce_seller_runtime_policy(&policy).expect("supported seller policy starts");
    }

    #[tokio::test]
    async fn policy_seller_dispute_release_if_clean_executes_release_dispute_lever() {
        use std::sync::atomic::Ordering;

        let chain = RecordingRecoveryChain::with_deal_state(disputed_deal_state());
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::RetireGateway,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        let handled =
            super::apply_seller_dispute_policy(&chain, &"tc-disputed".to_string(), &policy, "test")
                .await
                .expect("release dispute succeeds");

        assert!(handled);
        assert_eq!(chain.release_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_seller_dispute_hold_fails_closed_without_release() {
        use std::sync::atomic::Ordering;

        let chain = RecordingRecoveryChain::with_deal_state(disputed_deal_state());
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::RetireGateway,
            crate::cli::policy::SellerDisputeAgainstMeAction::Hold,
        );

        let err =
            super::apply_seller_dispute_policy(&chain, &"tc-disputed".to_string(), &policy, "test")
                .await
                .unwrap_err()
                .to_string();

        assert!(err.contains("failure_class=dispute_against_me"), "{err}");
        assert!(err.contains("action=hold"), "{err}");
        assert!(err.contains("result=no_release_submitted"), "{err}");
        assert_eq!(chain.release_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn policy_seller_after_done_retire_stops_serving() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::RetireGateway,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        let outcome = super::apply_seller_terminal_policy(&"tc-done".to_string(), &policy, 1)
            .expect("retire stops serving");

        assert!(matches!(
            outcome,
            super::SellerTerminalPolicyOutcome::StopServing
        ));
    }

    #[test]
    fn policy_seller_buyer_no_show_retire_gateway_stops_serving_without_cleanup_claim() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::RetireGateway,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        let outcome = super::apply_seller_terminal_policy(&"tc-noshow".to_string(), &policy, 0)
            .expect("retire_gateway stops serving without cleanup");

        assert!(matches!(
            outcome,
            super::SellerTerminalPolicyOutcome::StopServing
        ));
    }

    #[test]
    fn policy_seller_buyer_no_show_cleanup_and_retire_fails_closed_if_bypassed() {
        let policy = seller_policy(
            crate::cli::policy::SellerAfterDealDoneAction::Retire,
            crate::cli::policy::SellerBuyerNoShowAction::CleanupAndRetire,
            crate::cli::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        );

        let err = super::apply_seller_terminal_policy(&"tc-noshow".to_string(), &policy, 0)
            .unwrap_err()
            .to_string();

        assert!(err.contains("failure_class=buyer_no_show"), "{err}");
        assert!(err.contains("action=cleanup_and_retire"), "{err}");
        assert!(err.contains("result=policy_action_unsupported"), "{err}");
    }

    fn buyer_policy(
        no_handover_after_match: crate::cli::policy::NoHandoverAfterMatchAction,
        malformed_handover: crate::cli::policy::MalformedHandoverAction,
        dead_gateway: crate::cli::policy::DeadGatewayAction,
        empty_stream: crate::cli::policy::EmptyStreamAction,
        seller_stalls_mid_stream: crate::cli::policy::SellerStallsMidStreamAction,
        bad_output_scam: crate::cli::policy::BadOutputScamAction,
    ) -> crate::cli::policy::BuyerRuntimePolicy {
        crate::cli::policy::BuyerRuntimePolicy {
            no_handover_after_match,
            malformed_handover,
            dead_gateway,
            empty_stream,
            seller_stalls_mid_stream,
            bad_output_scam,
            max_sellers_to_try: 3,
            total_spend_cap_shells: 1_000_000_000,
        }
    }

    fn policy_for_no_handover(
        action: crate::cli::policy::NoHandoverAfterMatchAction,
    ) -> crate::cli::policy::BuyerRuntimePolicy {
        buyer_policy(
            action,
            crate::cli::policy::MalformedHandoverAction::FailClosed,
            crate::cli::policy::DeadGatewayAction::FailClosed,
            crate::cli::policy::EmptyStreamAction::FailClosed,
            crate::cli::policy::SellerStallsMidStreamAction::AcceptDeliveredThenReclaim,
            crate::cli::policy::BadOutputScamAction::Stop,
        )
    }

    fn policy_for_malformed(
        action: crate::cli::policy::MalformedHandoverAction,
    ) -> crate::cli::policy::BuyerRuntimePolicy {
        buyer_policy(
            crate::cli::policy::NoHandoverAfterMatchAction::FailClosed,
            action,
            crate::cli::policy::DeadGatewayAction::FailClosed,
            crate::cli::policy::EmptyStreamAction::FailClosed,
            crate::cli::policy::SellerStallsMidStreamAction::AcceptDeliveredThenReclaim,
            crate::cli::policy::BadOutputScamAction::Stop,
        )
    }

    fn policy_for_stream_failure(
        dead_gateway: crate::cli::policy::DeadGatewayAction,
        empty_stream: crate::cli::policy::EmptyStreamAction,
    ) -> crate::cli::policy::BuyerRuntimePolicy {
        buyer_policy(
            crate::cli::policy::NoHandoverAfterMatchAction::FailClosed,
            crate::cli::policy::MalformedHandoverAction::FailClosed,
            dead_gateway,
            empty_stream,
            crate::cli::policy::SellerStallsMidStreamAction::AcceptDeliveredThenReclaim,
            crate::cli::policy::BadOutputScamAction::Stop,
        )
    }

    #[test]
    fn policy_oneshot_dead_gateway_next_seller_fails_closed_before_order() {
        let policy = policy_for_stream_failure(
            crate::cli::policy::DeadGatewayAction::NextSeller,
            crate::cli::policy::EmptyStreamAction::Reclaim,
        );

        let err = super::validate_buyer_runtime_surface_policy(&policy, None)
            .unwrap_err()
            .to_string();

        assert!(err.contains("failure_class=policy_validation"), "{err}");
        assert!(err.contains("token_contract=<not-placed>"), "{err}");
        assert!(err.contains("state=pre_order"), "{err}");
        assert!(err.contains("buyer.on.dead_gateway=next_seller"), "{err}");
        assert!(
            err.contains("dead_gateway=retry_then_reclaim|fail_closed"),
            "{err}"
        );
    }

    #[test]
    fn policy_oneshot_empty_stream_next_seller_fails_closed_before_order() {
        let policy = policy_for_stream_failure(
            crate::cli::policy::DeadGatewayAction::RetryThenReclaim,
            crate::cli::policy::EmptyStreamAction::NextSeller,
        );

        let err = super::validate_buyer_runtime_surface_policy(&policy, None)
            .unwrap_err()
            .to_string();

        assert!(err.contains("failure_class=policy_validation"), "{err}");
        assert!(err.contains("token_contract=<not-placed>"), "{err}");
        assert!(err.contains("state=pre_order"), "{err}");
        assert!(err.contains("buyer.on.empty_stream=next_seller"), "{err}");
        assert!(err.contains("empty_stream=reclaim|fail_closed"), "{err}");
    }

    #[test]
    fn policy_local_listen_keeps_next_seller_policy_surface() {
        let policy = policy_for_stream_failure(
            crate::cli::policy::DeadGatewayAction::NextSeller,
            crate::cli::policy::EmptyStreamAction::NextSeller,
        );
        let bind = "127.0.0.1:0".parse().expect("socket addr");

        super::validate_buyer_runtime_surface_policy(&policy, Some(bind))
            .expect("local-listen surface handles unsupported actions at runtime");
    }

    #[tokio::test]
    async fn policy_no_handover_wait_then_reclaim_executes_cleanup_lever() {
        use std::sync::atomic::Ordering;

        let chain = RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state());
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));
        let policy =
            policy_for_no_handover(crate::cli::policy::NoHandoverAfterMatchAction::WaitThenReclaim);

        let err = super::apply_no_handover_after_match_policy(
            &chain,
            &buyer,
            &"tc-clean".to_string(),
            &policy,
            None,
            1,
            "diagnostic",
            None,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("failure_class=no_handover_after_match"),
            "{err}"
        );
        assert!(err.contains("action=wait_then_reclaim"), "{err}");
        assert!(err.contains("result=money_reclaimed"), "{err}");
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    // With shellnet enabled, this test serializes process-global DEXDO_PN_POOL for the full async scenario.
    #[cfg_attr(feature = "shellnet", allow(clippy::await_holding_lock))]
    #[tokio::test]
    async fn policy_no_handover_next_seller_cleans_then_places_next_buy() {
        use std::sync::atomic::Ordering;

        #[cfg(feature = "shellnet")]
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        #[cfg(feature = "shellnet")]
        let dir = std::env::temp_dir().join(format!(
            "dexdo-next-seller-pool-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        #[cfg(feature = "shellnet")]
        std::fs::create_dir(&dir).unwrap();
        #[cfg(feature = "shellnet")]
        let _cleanup = TempDirCleanup(dir.clone());
        #[cfg(feature = "shellnet")]
        let pool = dir.join("pn_pool.json");
        #[cfg(feature = "shellnet")]
        let buyer_note = format!("0:{}", "3".repeat(64));
        #[cfg(feature = "shellnet")]
        std::fs::write(
            &pool,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": buyer_note,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(feature = "shellnet")]
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        #[cfg(feature = "shellnet")]
        let pool_note_addr = Some(buyer_note.as_str());
        #[cfg(not(feature = "shellnet"))]
        let pool_note_addr = None;

        let chain = RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state());
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));
        let policy =
            policy_for_no_handover(crate::cli::policy::NoHandoverAfterMatchAction::NextSeller);

        let outcome = super::apply_no_handover_after_match_policy(
            &chain,
            &buyer,
            &"tc-current".to_string(),
            &policy,
            Some((1, 1, 1)),
            1,
            "diagnostic",
            pool_note_addr,
        )
        .await
        .expect("next_seller dispatch succeeds");

        assert!(matches!(
            outcome,
            super::NoHandoverPolicyOutcome::RetryNext(tc) if tc == "tc-next"
        ));
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.wait_match_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_no_handover_fail_closed_reports_without_recovery_lever() {
        use std::sync::atomic::Ordering;

        let chain = RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state());
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));
        let policy =
            policy_for_no_handover(crate::cli::policy::NoHandoverAfterMatchAction::FailClosed);

        let err = super::apply_no_handover_after_match_policy(
            &chain,
            &buyer,
            &"tc-fail".to_string(),
            &policy,
            None,
            1,
            "diagnostic",
            None,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("action=fail_closed"), "{err}");
        assert!(err.contains("result=no_recovery_submitted"), "{err}");
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_malformed_handover_reclaim_executes_reclaim_lever() {
        use std::sync::atomic::Ordering;

        let chain = RecordingRecoveryChain::default();
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));
        let policy = policy_for_malformed(crate::cli::policy::MalformedHandoverAction::Reclaim);
        let handover_error = anyhow::anyhow!("malformed handover: invalid bytes");

        let err = super::apply_malformed_handover_policy(
            &chain,
            &buyer,
            &"tc-malformed".to_string(),
            &policy,
            &handover_error,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("failure_class=malformed_handover"), "{err}");
        assert!(err.contains("action=reclaim"), "{err}");
        assert!(err.contains("result=reclaimed"), "{err}");
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_malformed_handover_dispute_executes_dispute_lever() {
        use std::sync::atomic::Ordering;

        let chain = RecordingRecoveryChain::default();
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));
        let policy = policy_for_malformed(crate::cli::policy::MalformedHandoverAction::Dispute);
        let handover_error = anyhow::anyhow!("handover decrypt failed: bad key");

        let err = super::apply_malformed_handover_policy(
            &chain,
            &buyer,
            &"tc-dispute".to_string(),
            &policy,
            &handover_error,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("failure_class=malformed_handover"), "{err}");
        assert!(err.contains("action=dispute"), "{err}");
        assert!(err.contains("result=dispute_opened"), "{err}");
        assert!(err.contains("dispute_locks_buyer_note"), "{err}");
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_malformed_handover_fail_closed_reports_without_recovery_lever() {
        use std::sync::atomic::Ordering;

        let chain = RecordingRecoveryChain::default();
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));
        let policy = policy_for_malformed(crate::cli::policy::MalformedHandoverAction::FailClosed);
        let handover_error = anyhow::anyhow!("malformed handover: invalid bytes");

        let err = super::apply_malformed_handover_policy(
            &chain,
            &buyer,
            &"tc-fail".to_string(),
            &policy,
            &handover_error,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("action=fail_closed"), "{err}");
        assert!(err.contains("result=no_recovery_submitted"), "{err}");
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_oneshot_dead_gateway_retry_then_reclaim_retries_once_then_reclaims() {
        use std::sync::atomic::Ordering;

        let chain = std::sync::Arc::new(RecordingRecoveryChain::default());
        let policy = policy_for_stream_failure(
            crate::cli::policy::DeadGatewayAction::RetryThenReclaim,
            crate::cli::policy::EmptyStreamAction::FailClosed,
        );
        let session = dexdo::buyer::api::SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-dead".to_string(),
            std::sync::Arc::new(dexdo_core::LocalNote::generate()),
            policy.as_api_failure_policy(),
        );

        assert_eq!(
            super::apply_oneshot_dead_gateway_policy(
                &session,
                &"tc-dead".to_string(),
                Some(&policy),
                1,
            )
            .await,
            super::OneShotStreamPolicyOutcome::RetryCurrent
        );
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);

        let report = match super::apply_oneshot_dead_gateway_policy(
            &session,
            &"tc-dead".to_string(),
            Some(&policy),
            2,
        )
        .await
        {
            super::OneShotStreamPolicyOutcome::TerminalReport(report) => report,
            other => panic!("expected terminal report, got {other:?}"),
        };

        assert!(report.contains("failure_class=dead_gateway"), "{report}");
        assert!(report.contains("action=retry_then_reclaim"), "{report}");
        assert!(report.contains("result=reclaim_submitted"), "{report}");
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_oneshot_dead_gateway_fail_closed_reports_without_recovery_lever() {
        use std::sync::atomic::Ordering;

        let chain = std::sync::Arc::new(RecordingRecoveryChain::default());
        let policy = policy_for_stream_failure(
            crate::cli::policy::DeadGatewayAction::FailClosed,
            crate::cli::policy::EmptyStreamAction::FailClosed,
        );
        let session = dexdo::buyer::api::SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-dead-fail".to_string(),
            std::sync::Arc::new(dexdo_core::LocalNote::generate()),
            policy.as_api_failure_policy(),
        );

        let report = match super::apply_oneshot_dead_gateway_policy(
            &session,
            &"tc-dead-fail".to_string(),
            Some(&policy),
            1,
        )
        .await
        {
            super::OneShotStreamPolicyOutcome::TerminalReport(report) => report,
            other => panic!("expected terminal report, got {other:?}"),
        };

        assert!(report.contains("action=fail_closed"), "{report}");
        assert!(report.contains("result=no_recovery_submitted"), "{report}");
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_oneshot_empty_stream_reclaim_executes_reclaim_lever() {
        use std::sync::atomic::Ordering;

        let chain = std::sync::Arc::new(RecordingRecoveryChain::default());
        let policy = policy_for_stream_failure(
            crate::cli::policy::DeadGatewayAction::FailClosed,
            crate::cli::policy::EmptyStreamAction::Reclaim,
        );
        let session = dexdo::buyer::api::SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-empty".to_string(),
            std::sync::Arc::new(dexdo_core::LocalNote::generate()),
            policy.as_api_failure_policy(),
        );

        let report = super::apply_oneshot_empty_stream_policy(
            &session,
            &"tc-empty".to_string(),
            Some(&policy),
        )
        .await;

        assert!(report.contains("failure_class=empty_stream"), "{report}");
        assert!(report.contains("action=reclaim"), "{report}");
        assert!(report.contains("result=reclaim_submitted"), "{report}");
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn policy_oneshot_empty_stream_fail_closed_reports_without_recovery_lever() {
        use std::sync::atomic::Ordering;

        let chain = std::sync::Arc::new(RecordingRecoveryChain::default());
        let policy = policy_for_stream_failure(
            crate::cli::policy::DeadGatewayAction::FailClosed,
            crate::cli::policy::EmptyStreamAction::FailClosed,
        );
        let session = dexdo::buyer::api::SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-empty-fail".to_string(),
            std::sync::Arc::new(dexdo_core::LocalNote::generate()),
            policy.as_api_failure_policy(),
        );

        let report = super::apply_oneshot_empty_stream_policy(
            &session,
            &"tc-empty-fail".to_string(),
            Some(&policy),
        )
        .await;

        assert!(report.contains("failure_class=empty_stream"), "{report}");
        assert!(report.contains("action=fail_closed"), "{report}");
        assert!(report.contains("result=no_recovery_submitted"), "{report}");
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn buyer_monitor_chain_facts_execute_recovery_once() {
        use dexdo::buyer::continuity::{BuyerAction, BuyerContinuity, ContinuityConfig, DealFacts};
        use std::sync::atomic::Ordering;

        let cfg = ContinuityConfig {
            renewal_threshold_tokens: 10,
            match_open_timeout_secs: 600,
            stream_timeout_secs: 600,
        };
        let chain = RecordingRecoveryChain::default();

        let opened_idle = super::buyer_monitor_current_facts(
            "tc-open".to_string(),
            100,
            false,
            Some(dexdo_core::DealChainState {
                funded: true,
                opened: true,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(1),
                last_advance: 100,
            }),
            700,
        );
        let mut planner = BuyerContinuity::default();
        let action = planner.tick(Some(opened_idle), None, cfg);
        assert_eq!(
            action,
            BuyerAction::ReclaimOpened {
                token_contract: "tc-open".to_string()
            }
        );
        let (kind, tc, result) = super::execute_buyer_monitor_recovery(&chain, action)
            .await
            .expect("reclaim action executes");
        assert_eq!(kind, super::BuyerMonitorRecoveryKind::ReclaimOpened);
        assert_eq!(tc, "tc-open");
        assert!(result.is_ok());
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            planner.tick(Some(DealFacts::opened_idle("tc-open", 601)), None, cfg),
            BuyerAction::IgnoreStale { token_contract } if token_contract == "tc-open"
        ));
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 1);

        let never_opened = super::buyer_monitor_current_facts(
            "tc-clean".to_string(),
            100,
            false,
            Some(dexdo_core::DealChainState {
                funded: true,
                opened: false,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(100),
                last_advance: 0,
            }),
            700,
        );
        let mut planner = BuyerContinuity::default();
        let action = planner.tick(Some(never_opened), None, cfg);
        assert_eq!(
            action,
            BuyerAction::CleanupUnopened {
                token_contract: "tc-clean".to_string()
            }
        );
        let (kind, tc, result) = super::execute_buyer_monitor_recovery(&chain, action)
            .await
            .expect("cleanup action executes");
        assert_eq!(kind, super::BuyerMonitorRecoveryKind::CleanupUnopened);
        assert_eq!(tc, "tc-clean");
        assert!(result.is_ok());
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            planner.tick(
                Some(DealFacts::funded_never_opened("tc-clean", 601)),
                None,
                cfg
            ),
            BuyerAction::IgnoreStale { token_contract } if token_contract == "tc-clean"
        ));
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn replay_protection_exit_52_is_retryable_for_lazy_buyer_init() {
        let err = anyhow::Error::new(dexdo_core::ChainError::Contract(
            "run_tvm getter getDetails exit code 52: Replay protection exception".to_string(),
        ))
        .context("lazy buyer initialization failed");
        assert!(super::is_replay_protection_error(&err));
    }

    #[test]
    fn ambiguous_submit_is_not_retried_as_replay_protection() {
        let err = anyhow::Error::new(dexdo_core::ChainError::AmbiguousSubmit(
            "replay protection response left submit outcome unknown".to_string(),
        ));
        assert!(!super::is_replay_protection_error(&err));
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_wallet_busy_error_is_actionable() {
        let raw = anyhow::anyhow!(
            "block manager rejected message code=TVM_ERROR; exit_code=52 nonce desynchronized"
        );
        assert!(super::is_note_deploy_wallet_busy_error(&raw));
        let err = super::note_deploy_error("0:wallet", raw).to_string();
        assert!(err.contains("wallet busy/out-of-sync"), "{err}");
        assert!(err.contains("Retry after"), "{err}");
        assert!(!err.contains("TVM_ERROR"), "{err}");
    }

    #[cfg(feature = "shellnet")]
    struct TempDirCleanup(std::path::PathBuf);

    #[cfg(feature = "shellnet")]
    impl Drop for TempDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(feature = "shellnet")]
    fn buyer_journal_test_dir(label: &str) -> (std::path::PathBuf, TempDirCleanup) {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        (dir.clone(), TempDirCleanup(dir))
    }

    #[cfg(feature = "shellnet")]
    fn buyer_submit_test_journal() -> super::BuyerSubmitJournal {
        let note_addr = format!("0:{}", "1".repeat(64));
        let order_book = format!("0:{}", "2".repeat(64));
        let token_contract = format!("0:{}", "3".repeat(64));
        let ticks = 2;
        let price_per_tick = 1_000_000;
        let escrow = dexdo_core::required_escrow_for_buy(ticks, price_per_tick);
        super::BuyerSubmitJournal {
            schema: super::BUYER_SUBMIT_JOURNAL_SCHEMA.to_string(),
            note_addr,
            order_book,
            intent: super::BuyerSubmitIntent::foreground(),
            expected_token_contract: Some(token_contract.clone()),
            quoted_order: dexdo_core::OrderBookOrder {
                order_id: 7,
                owner_note: format!("0:{}", "4".repeat(64)),
                token_contract: Some(token_contract.clone()),
                is_buy: false,
                price_per_tick,
                ticks,
                escrow: 0,
                deadline: 0,
                flags: 0,
                timestamp: 0,
            },
            quote: dexdo_core::ExecutableQuote {
                filled_ticks: ticks,
                total_with_fee: escrow,
                complete: true,
                fills: vec![dexdo_core::QuoteFill {
                    order_id: 7,
                    token_contract,
                    ticks,
                    price_per_tick,
                    cost_with_fee: escrow,
                }],
            },
            cursor: dexdo_core::MatchWatchCursor::new(1_000),
            ticks,
            max_price_per_tick: price_per_tick,
            escrow,
            submit_identity: format!("boc-sha256:{}", "a".repeat(64)),
            created_at_unix: 1_000,
            resolved_match: None,
            resolved_matches: Vec::new(),
        }
    }

    #[cfg(feature = "shellnet")]
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PreviousBuyerSubmitJournalV2 {
        schema: String,
        note_addr: String,
        order_book: String,
        intent: super::BuyerSubmitIntent,
        expected_token_contract: Option<dexdo_core::TokenContract>,
        quoted_order: dexdo_core::OrderBookOrder,
        quote: dexdo_core::ExecutableQuote,
        cursor: dexdo_core::MatchWatchCursor,
        ticks: u128,
        max_price_per_tick: u128,
        escrow: u128,
        submit_identity: String,
        created_at_unix: u64,
        #[serde(default)]
        resolved_match: Option<super::BuyerJournalMatch>,
        #[serde(default)]
        resolved_matches: Vec<super::BuyerJournalMatch>,
    }

    #[cfg(feature = "shellnet")]
    impl From<&super::BuyerSubmitJournal> for PreviousBuyerSubmitJournalV2 {
        fn from(journal: &super::BuyerSubmitJournal) -> Self {
            Self {
                schema: journal.schema.clone(),
                note_addr: journal.note_addr.clone(),
                order_book: journal.order_book.clone(),
                intent: journal.intent.clone(),
                expected_token_contract: journal.expected_token_contract.clone(),
                quoted_order: journal.quoted_order.clone(),
                quote: journal.quote.clone(),
                cursor: journal.cursor.clone(),
                ticks: journal.ticks,
                max_price_per_tick: journal.max_price_per_tick,
                escrow: journal.escrow,
                submit_identity: journal.submit_identity.clone(),
                created_at_unix: journal.created_at_unix,
                resolved_match: journal.resolved_match.clone(),
                resolved_matches: journal.resolved_matches.clone(),
            }
        }
    }

    #[cfg(feature = "shellnet")]
    struct JournalPipelineChain {
        submit_error: Option<&'static str>,
        fill: Option<dexdo_core::MatchedFill>,
        expected_journal_path: std::path::PathBuf,
        sequence: std::sync::Mutex<Vec<&'static str>>,
        post_count: std::sync::atomic::AtomicUsize,
        poll_count: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "shellnet")]
    #[async_trait::async_trait]
    impl dexdo_core::ChainBackend for JournalPipelineChain {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            self.sequence.lock().unwrap().push("fresh_read");
            Ok(vec![dexdo_core::OfferListing {
                seller_id: format!("0:{}", "4".repeat(64)),
                token_contract: format!("0:{}", "3".repeat(64)),
                price_per_tick: 1_000_000,
                max_ticks: 2,
            }])
        }

        async fn stop(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn seller_timeout(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn place_buy(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!()
        }

        fn model_buy_order_book_identity(&self) -> Option<String> {
            Some(format!("0:{}", "2".repeat(64)))
        }

        async fn place_buy_by_model_with_submit_identity(
            &self,
            _note: &dyn dexdo_core::Note,
            _quoted_order: Option<&dexdo_core::OrderBookOrder>,
            _ticks: u128,
            _max_price_per_tick: u128,
            _escrow: u128,
            cursor: &mut dexdo_core::MatchWatchCursor,
            before_post: &mut (dyn FnMut(
                String,
                dexdo_core::MatchWatchCursor,
            ) -> Result<(), dexdo_core::ChainError>
                      + Send),
        ) -> Result<(), dexdo_core::ChainError> {
            *cursor = dexdo_core::MatchWatchCursor::new(77);
            before_post(format!("boc-sha256:{}", "a".repeat(64)), cursor.clone())?;
            assert!(
                self.expected_journal_path.exists(),
                "journal callback must finish before the POST seam"
            );
            self.sequence.lock().unwrap().push("post");
            self.post_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.submit_error {
                Some("ambiguous") => Err(dexdo_core::ChainError::AmbiguousSubmit(
                    "injected ambiguous POST".to_string(),
                )),
                Some("rejected") => Err(dexdo_core::ChainError::MoneySubmitRejected(
                    "injected clean rejection".to_string(),
                )),
                Some("preparation") => Err(dexdo_core::ChainError::MoneySubmitPreparation(
                    "injected pre-POST failure".to_string(),
                )),
                _ => Ok(()),
            }
        }

        async fn wait_matched_token_contract(
            &self,
            _since_unix: i64,
            _timeout: std::time::Duration,
        ) -> Result<Option<dexdo_core::MatchedFill>, dexdo_core::ChainError> {
            match &self.fill {
                Some(fill) => Ok(Some(fill.clone())),
                None => Err(dexdo_core::ChainError::Transport(
                    "injected unresolved fill".to_string(),
                )),
            }
        }

        async fn poll_matched_model_buys_for_order_book(
            &self,
            _order_book: &str,
            _cursor: &mut dexdo_core::MatchWatchCursor,
        ) -> Result<Vec<dexdo_core::MatchedFill>, dexdo_core::ChainError> {
            let poll = self
                .poll_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.submit_error == Some("replay_once") && poll == 0 {
                self.sequence.lock().unwrap().push("replay_protection");
                return Err(dexdo_core::ChainError::Transport(
                    "injected replay protection exit code 52".to_string(),
                ));
            }
            self.sequence.lock().unwrap().push("reconcile");
            Ok(self.fill.clone().into_iter().collect())
        }

        async fn read_match(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn open_stream(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn read_handover(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn advance_tick(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn accept_probe(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!()
        }

        async fn deal_state(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<dexdo_core::DealChainState>, dexdo_core::ChainError> {
            Ok(Some(deal_state(true, false, false, false)))
        }

        async fn snapshot(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    #[cfg(feature = "shellnet")]
    struct ResumeCommandChain {
        fill: dexdo_core::MatchedFill,
        handover: Vec<u8>,
        post_count: std::sync::atomic::AtomicUsize,
        poll_count: std::sync::atomic::AtomicUsize,
        stop_count: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "shellnet")]
    #[async_trait::async_trait]
    impl dexdo_core::ChainBackend for ResumeCommandChain {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            panic!("retained-journal resume must not perform fresh offer discovery")
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not used by buyer resume")
        }

        async fn place_buy(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            self.post_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(dexdo_core::ChainError::Chain(
                "resume attempted a forbidden second money POST".to_string(),
            ))
        }

        fn model_buy_order_book_identity(&self) -> Option<String> {
            Some(format!("0:{}", "2".repeat(64)))
        }

        async fn place_buy_by_model_with_submit_identity(
            &self,
            _note: &dyn dexdo_core::Note,
            _quoted_order: Option<&dexdo_core::OrderBookOrder>,
            _ticks: u128,
            _max_price_per_tick: u128,
            _escrow: u128,
            _cursor: &mut dexdo_core::MatchWatchCursor,
            _before_post: &mut (dyn FnMut(
                String,
                dexdo_core::MatchWatchCursor,
            ) -> Result<(), dexdo_core::ChainError>
                      + Send),
        ) -> Result<(), dexdo_core::ChainError> {
            self.post_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(dexdo_core::ChainError::Chain(
                "resume attempted a forbidden second money POST".to_string(),
            ))
        }

        async fn poll_matched_model_buys_for_order_book(
            &self,
            _order_book: &str,
            _cursor: &mut dexdo_core::MatchWatchCursor,
        ) -> Result<Vec<dexdo_core::MatchedFill>, dexdo_core::ChainError> {
            self.poll_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![self.fill.clone()])
        }

        async fn read_match(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("not used by retained-journal resume")
        }

        async fn open_stream(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("handover is already authoritative")
        }

        async fn read_handover(
            &self,
            token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            assert_eq!(token_contract, &self.fill.token_contract);
            Ok(Some(self.handover.clone()))
        }

        async fn advance_tick(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            Ok(())
        }

        async fn accept_probe(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<(), dexdo_core::ChainError> {
            Ok(())
        }

        async fn stop(
            &self,
            token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            assert_eq!(token_contract, &self.fill.token_contract);
            self.stop_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(dexdo_core::Settlement::AmicableSplit {
                to_seller_ticks: 1,
                to_buyer_refund: 1,
            })
        }

        async fn seller_timeout(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            unimplemented!("healthy stream settles through buyer STOP")
        }

        async fn deal_state(
            &self,
            token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<dexdo_core::DealChainState>, dexdo_core::ChainError> {
            assert_eq!(token_contract, &self.fill.token_contract);
            Ok(Some(deal_state(true, true, false, true)))
        }

        async fn snapshot(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    #[cfg(feature = "shellnet")]
    fn journal_pipeline_selection() -> super::BuyerQuoteSelection {
        let journal = buyer_submit_test_journal();
        super::BuyerQuoteSelection {
            order_book: "model_order_book",
            escrow: journal.escrow,
            quote: journal.quote,
            quoted_order: Some(journal.quoted_order),
        }
    }

    #[cfg(feature = "shellnet")]
    async fn journal_pipeline_place(
        chain: &JournalPipelineChain,
        journal_path: &std::path::Path,
    ) -> (String, super::BuyerQuoteSelection, anyhow::Result<()>) {
        let journal = buyer_submit_test_journal();
        let note_addr = journal.note_addr;
        let selection = journal_pipeline_selection();
        let mut cursor = dexdo_core::MatchWatchCursor::default();
        let result = super::place_quote_bound_buy_with_journal(
            chain,
            &dexdo::buyer::Buyer::generate(),
            &super::BuyerSubmitIntent::foreground(),
            None,
            &selection,
            2,
            1_000_000,
            selection.escrow,
            &note_addr,
            &mut cursor,
            journal_path,
        )
        .await;
        (note_addr, selection, result)
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn durable_buy_journals_before_single_post_and_retains_ambiguity() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-pipeline-ambiguous");
        let journal_path = dir.join("journal.json");
        let chain = JournalPipelineChain {
            submit_error: Some("ambiguous"),
            fill: None,
            expected_journal_path: journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let (note_addr, selection, result) = journal_pipeline_place(&chain, &journal_path).await;
        assert!(result.as_ref().is_err_and(super::is_ambiguous_submit_error));
        assert!(
            journal_path.exists(),
            "callback must durably write before POST"
        );
        assert_eq!(chain.sequence.lock().unwrap().as_slice(), &["post"]);
        let error = super::complete_buyer_submit_with_journal(
            &chain,
            selection.quoted_order.as_ref(),
            2,
            1_000_000,
            result,
            &note_addr,
            &journal_path,
        )
        .await
        .expect_err("changed quoted row must fail closed");
        assert!(super::is_ambiguous_submit_error(&error));
        assert!(journal_path.exists());
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "reconciliation must never resubmit"
        );
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn on_demand_production_initializer_two_requests_return_fresh_then_durable_typed_503_without_second_post(
    ) {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-issue-61-http-ambiguous");
        let pool_path = dir.join("pool.json");
        let mut fixture = buyer_submit_test_journal();
        fixture.intent = super::BuyerSubmitIntent::on_demand();
        fixture.expected_token_contract = None;
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": fixture.note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let money_lock = super::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
        let _ = std::fs::remove_file(&money_lock.journal_path);
        let chain = std::sync::Arc::new(JournalPipelineChain {
            submit_error: Some("ambiguous"),
            fill: None,
            expected_journal_path: money_lock.journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::generate());
        let args = std::sync::Arc::new(super::BuyerArgs {
            mock: super::MockFlags {
                mock_model: false,
                mock_chain: false,
            },
            identity: super::IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: Some(fixture.note_addr.clone()),
            },
            registry: super::ModelRegistryValidationArgs::default(),
            endpoints_file: None,
            deals_dir: Some(dir.join("deals")),
            token_contract: None,
            resume: false,
            market: None,
            max_tokens: 8,
            local_listen: None,
            continuity_mode: super::ContinuityModeArg::OnDemand,
            json: false,
            anthropic_compat: false,
            frame_model: Some("qwen--qwen3--32b".to_string()),
            allow_unverified_model: true,
            models: dir.join("models.json"),
            ticks: fixture.ticks,
            max_price_per_tick: fixture.max_price_per_tick,
            escrow: Some(fixture.escrow),
            contracts: dir.join("offline-contracts.json"),
            policy: None,
        });
        let state = super::build_on_demand_buyer_api_state(
            chain.clone(),
            buyer,
            args,
            None,
            "qwen--qwen3--32b".to_string(),
            dexdo::buyer::api::ContentCheck::Skip,
            std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
            None,
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            None,
            None,
            super::BuyerShellnetPreflight::OfflineTest,
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (addr, task) =
            dexdo::buyer::api::serve("127.0.0.1:0".parse().unwrap(), state, false, async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("bind real lazy Axum API");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        for (request, expected_state, expected_origin) in [
            (1, "fresh_unresolved", "fresh_submit"),
            (2, "durable_unresolved", "durable_journal"),
        ] {
            let started = std::time::Instant::now();
            let response = client
                .post(format!("http://{addr}/v1/chat/completions"))
                .json(&serde_json::json!({
                    "model": "qwen--qwen3--32b",
                    "messages": [{"role": "user", "content": format!("issue 61 request {request}")}],
                    "max_tokens": 1,
                    "stream": false
                }))
                .send()
                .await
                .unwrap_or_else(|error| panic!("request {request} must return before timeout: {error}"));
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "request {request} must return before the outer initializer timeout"
            );
            assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
            let body = response.bytes().await.expect("503 body must be readable");
            assert!(!body.is_empty(), "request {request} 503 must carry JSON");
            let body: serde_json::Value =
                serde_json::from_slice(&body).expect("503 body must be JSON");
            let recovery = &body["error"]["submit_reconciliation"];
            assert!(
                !recovery.is_null(),
                "request {request} lost typed reconciliation: {body}"
            );
            assert_eq!(
                recovery["submit_identity"],
                serde_json::json!(fixture.submit_identity)
            );
            assert_eq!(recovery["recovery_anchor"]["order_id"], "1");
            assert_eq!(
                recovery["recovery_anchor"]["token_contract"],
                serde_json::json!(fixture.quoted_order.token_contract)
            );
            assert_eq!(recovery["state"], expected_state);
            assert_eq!(recovery["origin"], expected_origin);
        }

        let stored = super::load_buyer_submit_journal(&money_lock.journal_path, &fixture.note_addr)
            .unwrap()
            .expect("ambiguous on-demand submit must retain its durable journal");
        assert_eq!(stored.submit_identity, fixture.submit_identity);
        let serialized: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&money_lock.journal_path).unwrap()).unwrap();
        assert!(
            serialized.get("reconciled_submit_identity").is_none(),
            "v2 journal shape must not grow a recovery field"
        );
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the durable second request must not send a second BOC"
        );
        let _ = shutdown_tx.send(());
        task.await.expect("real lazy Axum API joins");
    }

    #[tokio::test]
    async fn anthropic_real_router_preserves_typed_submit_reconciliation() {
        let reconciliation = dexdo::buyer::api::BuyerSubmitReconciliation {
            submit_identity: format!("boc-sha256:{}", "a".repeat(64)),
            recovery_anchor: dexdo::buyer::api::BuyerSubmitRecoveryAnchor {
                order_id: 7,
                token_contract: format!("0:{}", "3".repeat(64)),
            },
            state: dexdo::buyer::api::BuyerSubmitReconciliationState::DurableUnresolved,
            origin: dexdo::buyer::api::BuyerSubmitReconciliationOrigin::DurableJournal,
        };
        let expected = reconciliation.clone();
        let state = dexdo::buyer::api::ApiState::lazy(
            std::sync::Arc::new(dexdo::buyer::Buyer::generate()),
            "qwen--qwen3--32b".to_string(),
            std::sync::Arc::new(move || {
                let reconciliation = reconciliation.clone();
                Box::pin(async move {
                    Err(dexdo::buyer::api::DealInitError::with_reconciliation(
                        "durable submit remains unresolved",
                        reconciliation,
                    ))
                }) as dexdo::buyer::api::DealInitFuture
            }),
            std::time::Duration::from_secs(2),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (addr, task) =
            dexdo::buyer::api::serve("127.0.0.1:0".parse().unwrap(), state, true, async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("bind Anthropic-compatible router");

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .json(&serde_json::json!({
                "model": "qwen--qwen3--32b",
                "messages": [{"role": "user", "content": "resume"}],
                "max_tokens": 1,
                "stream": false
            }))
            .send()
            .await
            .expect("Anthropic request returns");
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = response.json().await.expect("typed Anthropic JSON");
        assert_eq!(body["type"], "error");
        assert_eq!(
            body["error"]["submit_reconciliation"]["submit_identity"],
            expected.submit_identity
        );
        assert_eq!(
            body["error"]["submit_reconciliation"]["recovery_anchor"]["order_id"],
            "7"
        );
        assert_eq!(
            body["error"]["submit_reconciliation"]["recovery_anchor"]["token_contract"],
            expected.recovery_anchor.token_contract
        );
        assert_eq!(
            body["error"]["submit_reconciliation"]["state"],
            "durable_unresolved"
        );
        assert_eq!(
            body["error"]["submit_reconciliation"]["origin"],
            "durable_journal"
        );

        let _ = shutdown_tx.send(());
        task.await.expect("Anthropic router joins");
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn buyer_recovered_on_demand_resume_emits_exact_canonical_stream_without_second_money_post(
    ) {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-issue-61-resume");
        let pool_path = dir.join("pool.json");
        let mut fixture = buyer_submit_test_journal();
        fixture.intent = super::BuyerSubmitIntent::on_demand();
        fixture.expected_token_contract = None;
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": fixture.note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let money_lock = super::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
        let _ = std::fs::remove_file(&money_lock.journal_path);
        let selection = journal_pipeline_selection();
        let first = JournalPipelineChain {
            submit_error: Some("ambiguous"),
            fill: None,
            expected_journal_path: money_lock.journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let buyer = dexdo::buyer::Buyer::generate();
        let error = super::execute_buyer_quote_submit(
            &first,
            &buyer,
            false,
            Some(&fixture.note_addr),
            &fixture.intent,
            None,
            &selection,
            fixture.ticks,
            fixture.max_price_per_tick,
            fixture.escrow,
            |_| std::future::ready(Ok(())),
        )
        .await
        .expect_err("first submit is intentionally ambiguous");
        assert!(super::is_ambiguous_submit_error(&error), "{error:#}");
        assert_eq!(
            first.post_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let fill = dexdo_core::MatchedFill {
            token_contract: fixture.quoted_order.token_contract.clone().unwrap(),
            ticks: fixture.ticks,
            price_per_tick: fixture.quoted_order.price_per_tick,
        };
        let buyer_note = std::sync::Arc::new(dexdo_core::LocalNote::generate());
        let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve gateway port");
        let gateway_addr = gateway_listener.local_addr().unwrap();
        drop(gateway_listener);
        let seller = dexdo::seller::start_gateway(gateway_addr)
            .await
            .expect("start TLS mock-token gateway");
        let mut gateway_ready = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(gateway_addr).await.is_ok() {
                gateway_ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gateway_ready, "TLS mock-token gateway must bind");
        let buyer_pubkey = dexdo_core::Note::pubkey(buyer_note.as_ref());
        seller.state.register_stream(
            &fill.token_contract,
            buyer_pubkey.clone(),
            2,
            fixture.ticks as u64,
            dexdo_core::DobParams::canonical().tick_size,
        );
        let handover = dexdo_core::Handover {
            endpoint: format!("https://{gateway_addr}"),
            tls_fingerprint: seller.tls_fingerprint.clone(),
        };
        let encrypted_handover = seller.note.encrypt_to(&buyer_pubkey, &handover.to_bytes());
        let resumed = std::sync::Arc::new(ResumeCommandChain {
            fill: fill.clone(),
            handover: encrypted_handover,
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
            stop_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve local API port");
        let api_addr = api_listener.local_addr().unwrap();
        drop(api_listener);
        let policy_path = dir.join("policy.json");
        std::fs::write(
            &policy_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "buyer": {
                    "on": {
                        "no_handover_after_match": "fail_closed",
                        "malformed_handover": "fail_closed",
                        "dead_gateway": "fail_closed",
                        "empty_stream": "fail_closed",
                        "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                        "bad_output_scam": "stop"
                    },
                    "failover": {
                        "max_sellers_to_try": 1,
                        "total_spend_cap_shells": 1000000000
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let args = super::BuyerArgs {
            mock: super::MockFlags {
                mock_model: false,
                mock_chain: false,
            },
            identity: super::IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: Some(fixture.note_addr.clone()),
            },
            registry: super::ModelRegistryValidationArgs::default(),
            endpoints_file: None,
            deals_dir: Some(dir.join("deals")),
            token_contract: None,
            resume: true,
            market: None,
            max_tokens: 8,
            local_listen: Some(api_addr),
            continuity_mode: super::ContinuityModeArg::OnDemand,
            json: true,
            anthropic_compat: false,
            frame_model: Some("qwen--qwen3--32b".to_string()),
            allow_unverified_model: true,
            models: dir.join("models.json"),
            ticks: fixture.ticks,
            max_price_per_tick: fixture.max_price_per_tick,
            escrow: Some(fixture.escrow),
            contracts: dir.join("offline-contracts.json"),
            policy: Some(policy_path),
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let command_chain: std::sync::Arc<dyn dexdo_core::ChainBackend> = resumed.clone();
        let command_note: std::sync::Arc<dyn dexdo_core::Note> = buyer_note;
        let (machine_writer, captured_machine_events) =
            crate::cli::machine::BuyerEventWriter::capturing();
        let command = tokio::spawn(async move {
            let mut machine_events = Some(machine_writer);
            let mut machine_context = super::BuyerMachineErrorContext::default();
            super::run_buyer_inner(
                args,
                &mut machine_events,
                &mut machine_context,
                super::BuyerCommandRuntime {
                    backend: Some((command_chain, command_note)),
                    shellnet_preflight: super::BuyerShellnetPreflight::OfflineTest,
                    shutdown: Box::pin(async move {
                        let _ = shutdown_rx.await;
                    }),
                },
            )
            .await
        });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let models_url = format!("http://{api_addr}/v1/models");
        let mut ready = false;
        for _ in 0..100 {
            if client
                .get(&models_url)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(ready, "run_buyer_inner must bind the real local API");
        let response = client
            .post(format!("http://{api_addr}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "qwen--qwen3--32b",
                "messages": [{"role": "user", "content": "resume through the real command"}],
                "max_tokens": 1,
                "stream": true
            }))
            .send()
            .await
            .expect("resumed local request reaches the gateway stream");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let stream = response.text().await.expect("SSE response body");
        assert!(stream.contains("data:"), "{stream}");
        assert!(stream.contains("[DONE]"), "{stream}");

        let _ = shutdown_tx.send(());
        command
            .await
            .expect("run_buyer_inner task joins")
            .expect("run_buyer_inner resume completes through settlement");
        assert_eq!(
            resumed.post_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "run_buyer_inner --resume must not send a second money POST"
        );
        assert_eq!(
            resumed.poll_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "run_buyer_inner must reconcile the retained journal exactly once"
        );
        assert_eq!(
            resumed.stop_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "graceful command shutdown must submit one terminal AmicableSplit STOP"
        );
        assert!(
            super::load_buyer_submit_journal(&money_lock.journal_path, &fixture.note_addr)
                .unwrap()
                .is_none(),
            "authoritatively matched journal must clear only after handover is adopted"
        );
        let captured = captured_machine_events
            .lock()
            .expect("captured buyer events lock poisoned");
        let event_names = captured
            .iter()
            .filter_map(|event| event["event"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            event_names,
            [
                "starting",
                "resume_selected",
                "handover_waiting",
                "handover_received",
                "endpoint_binding",
                "endpoint_ready",
                "stopping",
                "settlement_submitted",
                "settled",
                "exiting",
            ],
            "recovered on-demand resume must emit only the complete canonical stream"
        );
        let canonical_token_contract = dexdo_core::Address::parse(&fill.token_contract)
            .unwrap()
            .with_workchain();
        let canonical_deal_handle = super::deals::make_handle_id(&canonical_token_contract);
        for event in captured.iter().filter(|event| {
            matches!(
                event["event"].as_str(),
                Some(
                    "resume_selected"
                        | "handover_waiting"
                        | "handover_received"
                        | "endpoint_binding"
                        | "endpoint_ready"
                        | "stopping"
                        | "settlement_submitted"
                        | "settled"
                        | "exiting"
                )
            )
        }) {
            assert_eq!(
                event["token_contract"], canonical_token_contract,
                "every deal-bound event must carry the real normalized TokenContract: {event}"
            );
            assert!(
                !event.to_string().contains("pending:"),
                "canonical stream must reject pending placeholders: {event}"
            );
        }
        for event_name in [
            "resume_selected",
            "handover_received",
            "endpoint_binding",
            "endpoint_ready",
            "stopping",
            "settlement_submitted",
            "settled",
            "exiting",
        ] {
            let event = captured
                .iter()
                .find(|event| event["event"] == event_name)
                .unwrap_or_else(|| panic!("missing {event_name} event"));
            assert_eq!(
                event["deal_handle"], canonical_deal_handle,
                "{event_name} must carry the deal handle derived from the real TokenContract"
            );
        }
        assert_eq!(
            &event_names[..6],
            [
                "starting",
                "resume_selected",
                "handover_waiting",
                "handover_received",
                "endpoint_binding",
                "endpoint_ready",
            ],
            "{event_names:?}"
        );
        let resume = captured
            .iter()
            .find(|event| event["event"] == "resume_selected")
            .expect("resume_selected object");
        assert_eq!(resume["source"], "durable_journal");
        assert_eq!(
            resume["token_contract"],
            serde_json::json!(canonical_token_contract)
        );
        assert_eq!(
            resume["submit_reconciliation"]["submit_identity"],
            fixture.submit_identity
        );
        assert_eq!(
            resume["submit_reconciliation"]["recovery_anchor"]["order_id"],
            fixture.quoted_order.order_id.to_string()
        );
        assert_eq!(
            resume["submit_reconciliation"]["recovery_anchor"]["token_contract"],
            serde_json::json!(canonical_token_contract)
        );
        seller.server_task.abort();
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn durable_buy_clean_rejection_clears_journal() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-pipeline-rejected");
        let journal_path = dir.join("journal.json");
        let chain = JournalPipelineChain {
            submit_error: Some("rejected"),
            fill: None,
            expected_journal_path: journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let (note_addr, selection, result) = journal_pipeline_place(&chain, &journal_path).await;
        assert!(journal_path.exists());
        super::complete_buyer_submit_with_journal(
            &chain,
            selection.quoted_order.as_ref(),
            2,
            1_000_000,
            result,
            &note_addr,
            &journal_path,
        )
        .await
        .expect_err("changed executable quote must fail closed");
        assert!(!journal_path.exists());
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn durable_buy_pre_post_failure_clears_but_unclassified_failure_retains() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-pipeline-preparation");
        let journal_path = dir.join("journal.json");
        let chain = JournalPipelineChain {
            submit_error: Some("preparation"),
            fill: None,
            expected_journal_path: journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let (note_addr, selection, result) = journal_pipeline_place(&chain, &journal_path).await;
        super::complete_buyer_submit_with_journal(
            &chain,
            selection.quoted_order.as_ref(),
            2,
            1_000_000,
            result,
            &note_addr,
            &journal_path,
        )
        .await
        .expect_err("pre-POST failure must propagate");
        assert!(!journal_path.exists());

        super::write_buyer_submit_journal(&journal_path, &buyer_submit_test_journal()).unwrap();
        let error = super::complete_buyer_submit_with_journal(
            &chain,
            selection.quoted_order.as_ref(),
            2,
            1_000_000,
            Err(anyhow::anyhow!("unclassified submit failure")),
            &note_addr,
            &journal_path,
        )
        .await
        .expect_err("unclassified outcome must fail closed");
        assert!(super::is_ambiguous_submit_error(&error), "{error:#}");
        assert!(journal_path.exists());
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn durable_buy_reconcile_matches_pending_without_second_post() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-pipeline-reconcile");
        let journal_path = dir.join("journal.json");
        let pool_path = dir.join("pool.json");
        let fixture = buyer_submit_test_journal();
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": fixture.note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let fill = dexdo_core::MatchedFill {
            token_contract: fixture.quoted_order.token_contract.clone().unwrap(),
            ticks: fixture.ticks,
            price_per_tick: fixture.quoted_order.price_per_tick,
        };
        let chain = JournalPipelineChain {
            submit_error: Some("ambiguous"),
            fill: Some(fill.clone()),
            expected_journal_path: journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let (note_addr, _selection, result) = journal_pipeline_place(&chain, &journal_path).await;
        assert!(result.as_ref().is_err_and(super::is_ambiguous_submit_error));
        let reconciled =
            super::reconcile_pending_buyer_submit(&chain, &note_addr, &journal_path, None)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(reconciled.0, fill.token_contract);
        let stored = super::load_buyer_submit_journal(&journal_path, &note_addr)
            .unwrap()
            .unwrap();
        assert_eq!(stored.resolved_matches.len(), 1);
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn real_entry_raise_reconciles_before_fresh_reads_and_uses_journal_budget() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-entry-raise");
        let pool_path = dir.join("pool.json");
        let fixture = buyer_submit_test_journal();
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": fixture.note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let money_lock = super::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
        let _ = std::fs::remove_file(&money_lock.journal_path);
        super::write_buyer_submit_journal(&money_lock.journal_path, &fixture).unwrap();
        let fill = dexdo_core::MatchedFill {
            token_contract: fixture.quoted_order.token_contract.clone().unwrap(),
            ticks: fixture.ticks,
            price_per_tick: fixture.quoted_order.price_per_tick,
        };
        let chain = JournalPipelineChain {
            submit_error: None,
            fill: Some(fill.clone()),
            expected_journal_path: money_lock.journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let buyer = dexdo::buyer::Buyer::generate();
        let outcome = super::raise_pending_buyer_money_before_fresh_reads(
            &chain,
            &buyer,
            Some(&fixture.note_addr),
            &fixture.intent,
            fixture.expected_token_contract.as_deref(),
            fixture.ticks,
            fixture.max_price_per_tick,
            fixture.escrow,
        )
        .await
        .unwrap()
        .expect("matching durable submit must be raised at the entry seam");
        dexdo_core::ChainBackend::discover_offers(&chain)
            .await
            .unwrap();
        assert_eq!(
            chain.sequence.lock().unwrap().as_slice(),
            &["reconcile", "fresh_read"],
            "durable money reconciliation must precede every fresh book read"
        );
        assert_eq!(outcome.token_contract, fill.token_contract);
        assert_eq!(outcome.ticks, fixture.ticks);
        assert_eq!(
            super::consumer_api_token_budget(outcome.ticks),
            super::consumer_api_token_budget(fixture.ticks),
            "served budget must use journal ticks"
        );
        assert_ne!(
            super::consumer_api_token_budget(outcome.ticks),
            super::consumer_api_token_budget(fixture.ticks + 6),
            "a restarted --ticks value must not expand service"
        );
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "raising an existing submit must never POST again"
        );

        let polls_before = chain.poll_count.load(std::sync::atomic::Ordering::SeqCst);
        let error = super::raise_pending_buyer_money_before_fresh_reads(
            &chain,
            &buyer,
            Some(&fixture.note_addr),
            &fixture.intent,
            fixture.expected_token_contract.as_deref(),
            fixture.ticks + 6,
            fixture.max_price_per_tick,
            fixture.escrow,
        )
        .await
        .expect_err("changed restart terms must fail closed");
        assert!(error.to_string().contains("different logical invocation"));
        assert_eq!(
            chain.poll_count.load(std::sync::atomic::Ordering::SeqCst),
            polls_before,
            "changed terms must fail before another chain read"
        );
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "changed terms must never POST"
        );
        for (label, expected_tc, ticks, max_price, escrow) in [
            (
                "max_price_per_tick",
                fixture.expected_token_contract.as_deref(),
                fixture.ticks,
                fixture.max_price_per_tick + 1,
                fixture.escrow,
            ),
            (
                "escrow",
                fixture.expected_token_contract.as_deref(),
                fixture.ticks,
                fixture.max_price_per_tick,
                fixture.escrow + 1,
            ),
            (
                "expected_token_contract",
                Some("0:9999999999999999999999999999999999999999999999999999999999999999"),
                fixture.ticks,
                fixture.max_price_per_tick,
                fixture.escrow,
            ),
        ] {
            let error = super::raise_pending_buyer_money_before_fresh_reads(
                &chain,
                &buyer,
                Some(&fixture.note_addr),
                &fixture.intent,
                expected_tc,
                ticks,
                max_price,
                escrow,
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("different logical invocation"),
                "{label}: {error:#}"
            );
        }
        let mut changed_row = journal_pipeline_selection();
        changed_row.quoted_order.as_mut().unwrap().order_id += 1;
        let error = super::start_durable_buyer_submit(
            &chain,
            &buyer,
            &fixture.intent,
            fixture.expected_token_contract.as_deref(),
            &changed_row,
            fixture.ticks,
            fixture.max_price_per_tick,
            fixture.escrow,
            &fixture.note_addr,
            &money_lock.journal_path,
        )
        .await
        .err()
        .expect("changed quoted row must fail closed");
        assert!(error.to_string().contains("different logical invocation"));
        let mut changed_quote = journal_pipeline_selection();
        changed_quote.quote.total_with_fee += 1;
        let error = super::start_durable_buyer_submit(
            &chain,
            &buyer,
            &fixture.intent,
            fixture.expected_token_contract.as_deref(),
            &changed_quote,
            fixture.ticks,
            fixture.max_price_per_tick,
            fixture.escrow,
            &fixture.note_addr,
            &money_lock.journal_path,
        )
        .await
        .err()
        .expect("changed executable quote must fail closed");
        assert!(error.to_string().contains("different logical invocation"));
        assert_eq!(
            chain.poll_count.load(std::sync::atomic::Ordering::SeqCst),
            polls_before,
            "every durable-term mismatch must fail before reconciliation reads"
        );
        super::clear_adopted_buyer_money_journal(
            Some(&fixture.note_addr),
            outcome
                .submit_reconciliation
                .as_ref()
                .map(|reconciliation| reconciliation.submit_identity.as_str()),
            &outcome.token_contract,
        )
        .unwrap();
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn foreground_and_on_demand_entry_modes_raise_before_fresh_book_reads() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-entry-modes");
        let pool_path = dir.join("pool.json");
        let base = buyer_submit_test_journal();
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": base.note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let money_lock = super::BuyerMoneyLock::open(&base.note_addr).unwrap();
        let buyer = dexdo::buyer::Buyer::generate();

        for (label, intent, explicit) in [
            (
                "foreground-model-only",
                super::BuyerSubmitIntent::foreground(),
                false,
            ),
            (
                "foreground-explicit-token-contract",
                super::BuyerSubmitIntent::foreground(),
                true,
            ),
            (
                "on-demand-model-only",
                super::BuyerSubmitIntent::on_demand(),
                false,
            ),
            (
                "on-demand-explicit-token-contract",
                super::BuyerSubmitIntent::on_demand(),
                true,
            ),
        ] {
            let mut fixture = base.clone();
            fixture.intent = intent.clone();
            if !explicit {
                fixture.expected_token_contract = None;
            }
            let _ = std::fs::remove_file(&money_lock.journal_path);
            super::write_buyer_submit_journal(&money_lock.journal_path, &fixture).unwrap();
            let fill = dexdo_core::MatchedFill {
                token_contract: fixture.quoted_order.token_contract.clone().unwrap(),
                ticks: fixture.ticks,
                price_per_tick: fixture.quoted_order.price_per_tick,
            };
            let chain = JournalPipelineChain {
                submit_error: None,
                fill: Some(fill),
                expected_journal_path: money_lock.journal_path.clone(),
                sequence: std::sync::Mutex::new(Vec::new()),
                post_count: std::sync::atomic::AtomicUsize::new(0),
                poll_count: std::sync::atomic::AtomicUsize::new(0),
            };
            let outcome = super::raise_pending_buyer_money_before_fresh_reads(
                &chain,
                &buyer,
                Some(&fixture.note_addr),
                &intent,
                fixture.expected_token_contract.as_deref(),
                fixture.ticks,
                fixture.max_price_per_tick,
                fixture.escrow,
            )
            .await
            .unwrap()
            .unwrap();
            dexdo_core::ChainBackend::discover_offers(&chain)
                .await
                .unwrap();
            assert_eq!(
                chain.sequence.lock().unwrap().as_slice(),
                &["reconcile", "fresh_read"],
                "{label} must raise durable money before a fresh book read"
            );
            assert_eq!(
                chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{label} must adopt without a second POST"
            );
            assert_eq!(outcome.ticks, fixture.ticks, "{label} journal budget");
            super::clear_adopted_buyer_money_journal(
                Some(&fixture.note_addr),
                outcome
                    .submit_reconciliation
                    .as_ref()
                    .map(|reconciliation| reconciliation.submit_identity.as_str()),
                &outcome.token_contract,
            )
            .unwrap();
        }
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn on_demand_attempt_two_reconciles_before_failing_fresh_preflight() {
        // run_buyer_inner and run_subscription construct their real backends at the command
        // boundary. Their coverage is intentionally deferred to the live shellnet proof; do not
        // replace it with a fake command-boundary test.
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-on-demand-attempt-two");
        let pool_path = dir.join("pool.json");
        let mut fixture = buyer_submit_test_journal();
        fixture.intent = super::BuyerSubmitIntent::on_demand();
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": fixture.note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let money_lock = super::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
        let _ = std::fs::remove_file(&money_lock.journal_path);
        super::write_buyer_submit_journal(&money_lock.journal_path, &fixture).unwrap();
        let fill = dexdo_core::MatchedFill {
            token_contract: fixture.quoted_order.token_contract.clone().unwrap(),
            ticks: fixture.ticks,
            price_per_tick: fixture.quoted_order.price_per_tick,
        };
        let chain = std::sync::Arc::new(JournalPipelineChain {
            submit_error: Some("replay_once"),
            fill: Some(fill),
            expected_journal_path: money_lock.journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let missing_contracts = dir.join("missing-contracts.json");
        let args = std::sync::Arc::new(super::BuyerArgs {
            mock: super::MockFlags {
                mock_model: false,
                mock_chain: false,
            },
            identity: super::IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: Some(fixture.note_addr.clone()),
            },
            registry: super::ModelRegistryValidationArgs::default(),
            endpoints_file: None,
            deals_dir: None,
            token_contract: fixture.expected_token_contract.clone(),
            resume: false,
            market: None,
            max_tokens: 8,
            local_listen: None,
            continuity_mode: super::ContinuityModeArg::OnDemand,
            json: false,
            anthropic_compat: false,
            frame_model: Some("qwen--qwen3--32b".to_string()),
            allow_unverified_model: true,
            models: dir.join("models.json"),
            ticks: fixture.ticks,
            max_price_per_tick: fixture.max_price_per_tick,
            escrow: Some(fixture.escrow),
            contracts: missing_contracts.clone(),
            policy: None,
        });
        let error = super::prepare_lazy_buyer_api_deal_with_replay_backoff(
            chain.clone(),
            std::sync::Arc::new(dexdo::buyer::Buyer::generate()),
            args,
            fixture.expected_token_contract.clone(),
            "qwen--qwen3--32b".to_string(),
            dexdo::buyer::api::ContentCheck::Skip,
            std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
            None,
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            None,
            None,
            super::BuyerShellnetPreflight::Production,
        )
        .await
        .err()
        .expect("the real retry wrapper must reach the deliberately failing fresh preflight");
        assert!(
            error
                .message()
                .contains(&missing_contracts.display().to_string())
                || error.message().contains("No such file"),
            "{error}"
        );
        assert_eq!(
            chain.sequence.lock().unwrap().as_slice(),
            &["replay_protection", "reconcile"],
            "attempt one must trigger retry and attempt two must reconcile before the fresh doctor read fails"
        );
        assert_eq!(
            chain.poll_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the real retry wrapper must call the protected journal path on both attempts"
        );
        let reconciled =
            super::load_buyer_submit_journal(&money_lock.journal_path, &fixture.note_addr)
                .unwrap()
                .expect("the attempt-two journal must remain available after the fresh read fails");
        assert_eq!(
            reconciled.resolved_matches.len(),
            1,
            "attempt two must persist reconciliation before the fresh doctor read fires"
        );
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "attempt two must not POST while adopting the retained journal"
        );
    }

    #[cfg(feature = "shellnet")]
    // This test must serialize process-global DEXDO_PN_POOL for the full async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn legacy_v1_reconciles_and_persists_facts_but_is_not_adopted() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-v1-fact-reconcile");
        let journal_path = dir.join("journal.json");
        let pool_path = dir.join("pool.json");
        let fixture = buyer_submit_test_journal();
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [{
                    "address": fixture.note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let fill = dexdo_core::MatchedFill {
            token_contract: fixture.quoted_order.token_contract.clone().unwrap(),
            ticks: fixture.ticks,
            price_per_tick: fixture.quoted_order.price_per_tick,
        };
        let legacy = super::BuyerSubmitJournalV1 {
            schema: super::BUYER_SUBMIT_JOURNAL_SCHEMA_V1.to_string(),
            note_addr: fixture.note_addr.clone(),
            order_book: fixture.order_book.clone(),
            expected_token_contract: fixture.expected_token_contract.clone(),
            quoted_order: fixture.quoted_order.clone(),
            quote: fixture.quote.clone(),
            cursor: fixture.cursor.clone(),
            ticks: fixture.ticks,
            max_price_per_tick: fixture.max_price_per_tick,
            escrow: fixture.escrow,
            submit_identity: fixture.submit_identity.clone(),
            created_at_unix: fixture.created_at_unix,
            resolved_match: Some(super::journal_match(&fill, fixture.quoted_order.order_id)),
        };
        let bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        super::with_pool_write_lock(&journal_path, |path| {
            super::write_pool_private(path, &bytes)
        })
        .unwrap();
        let chain = JournalPipelineChain {
            submit_error: None,
            fill: None,
            expected_journal_path: journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let selection = journal_pipeline_selection();
        let error = super::start_durable_buyer_submit(
            &chain,
            &dexdo::buyer::Buyer::generate(),
            &super::BuyerSubmitIntent::foreground(),
            fixture.expected_token_contract.as_deref(),
            &selection,
            fixture.ticks,
            fixture.max_price_per_tick,
            fixture.escrow,
            &fixture.note_addr,
            &journal_path,
        )
        .await
        .err()
        .expect("legacy journal must not be adopted");
        assert!(error
            .to_string()
            .contains("cannot be adopted as a fresh intent"));
        assert_eq!(
            chain.poll_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let stored = super::load_buyer_submit_journal(&journal_path, &fixture.note_addr)
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.intent.kind,
            super::BuyerSubmitIntentKind::LegacyUnknown
        );
        assert_eq!(stored.resolved_matches.len(), 1);
        assert_eq!(
            stored.resolved_matches[0].token_contract,
            fill.token_contract
        );
        let pool = std::fs::read_to_string(&pool_path).unwrap();
        assert!(pool.contains(&fill.token_contract));
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn durable_recovery_rejects_cross_kind_before_chain_read_or_post() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-cross-kind");
        let journal_path = dir.join("journal.json");
        let fixture = buyer_submit_test_journal();
        super::write_buyer_submit_journal(&journal_path, &fixture).unwrap();
        let chain = JournalPipelineChain {
            submit_error: None,
            fill: None,
            expected_journal_path: journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let selection = journal_pipeline_selection();
        let error = super::start_durable_buyer_submit(
            &chain,
            &dexdo::buyer::Buyer::generate(),
            &super::BuyerSubmitIntent::on_demand(),
            fixture.expected_token_contract.as_deref(),
            &selection,
            fixture.ticks,
            fixture.max_price_per_tick,
            fixture.escrow,
            &fixture.note_addr,
            &journal_path,
        )
        .await
        .err()
        .expect("cross-kind recovery must fail closed");
        assert!(error.to_string().contains("different logical invocation"));
        assert_eq!(
            chain.poll_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn durable_recovery_rejects_wrong_continuity_generation_before_chain_read_or_post() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-wrong-generation");
        let journal_path = dir.join("journal.json");
        let mut fixture = buyer_submit_test_journal();
        let predecessor = format!("0:{}", "5".repeat(64));
        fixture.intent = super::BuyerSubmitIntent::after(
            super::BuyerSubmitIntentKind::ContinuityRenewal,
            &predecessor,
        );
        super::write_buyer_submit_journal(&journal_path, &fixture).unwrap();
        let chain = JournalPipelineChain {
            submit_error: None,
            fill: None,
            expected_journal_path: journal_path.clone(),
            sequence: std::sync::Mutex::new(Vec::new()),
            post_count: std::sync::atomic::AtomicUsize::new(0),
            poll_count: std::sync::atomic::AtomicUsize::new(0),
        };
        let selection = journal_pipeline_selection();
        let wrong_predecessor = format!("0:{}", "6".repeat(64));
        let intent = super::BuyerSubmitIntent::after(
            super::BuyerSubmitIntentKind::ContinuityRenewal,
            &wrong_predecessor,
        );
        let error = super::start_durable_buyer_submit(
            &chain,
            &dexdo::buyer::Buyer::generate(),
            &intent,
            fixture.expected_token_contract.as_deref(),
            &selection,
            fixture.ticks,
            fixture.max_price_per_tick,
            fixture.escrow,
            &fixture.note_addr,
            &journal_path,
        )
        .await
        .err()
        .expect("wrong continuity generation must fail closed");
        assert!(error.to_string().contains("different logical invocation"));
        assert_eq!(
            chain.poll_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_money_journal_schema_first_load_dispatches_v1_and_v2() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-journal-schema");
        let v2_path = dir.join("v2.json");
        let v1_path = dir.join("v1.json");
        let journal = buyer_submit_test_journal();

        super::write_buyer_submit_journal(&v2_path, &journal).unwrap();
        let loaded = super::load_buyer_money_journal(&v2_path, &journal.note_addr)
            .unwrap()
            .unwrap();
        let super::BuyerMoneyJournal::Buy(loaded) = loaded else {
            panic!("v2 schema must dispatch to a buy journal");
        };
        assert_eq!(*loaded, journal);

        let legacy = super::BuyerSubmitJournalV1 {
            schema: super::BUYER_SUBMIT_JOURNAL_SCHEMA_V1.to_string(),
            note_addr: journal.note_addr.clone(),
            order_book: journal.order_book.clone(),
            expected_token_contract: journal.expected_token_contract.clone(),
            quoted_order: journal.quoted_order.clone(),
            quote: journal.quote.clone(),
            cursor: journal.cursor.clone(),
            ticks: journal.ticks,
            max_price_per_tick: journal.max_price_per_tick,
            escrow: journal.escrow,
            submit_identity: journal.submit_identity.clone(),
            created_at_unix: journal.created_at_unix,
            resolved_match: None,
        };
        let bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        super::with_pool_write_lock(&v1_path, |path| super::write_pool_private(path, &bytes))
            .unwrap();
        let loaded = super::load_buyer_money_journal(&v1_path, &journal.note_addr)
            .unwrap()
            .unwrap();
        let super::BuyerMoneyJournal::Buy(loaded) = loaded else {
            panic!("v1 schema must dispatch to a buy journal");
        };
        assert_eq!(loaded.schema, super::BUYER_SUBMIT_JOURNAL_SCHEMA);
        assert_eq!(
            loaded.intent.kind,
            super::BuyerSubmitIntentKind::LegacyUnknown
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_submit_journal_v1_conversion_marks_legacy_unknown() {
        let journal = buyer_submit_test_journal();
        let legacy = super::BuyerSubmitJournalV1 {
            schema: super::BUYER_SUBMIT_JOURNAL_SCHEMA_V1.to_string(),
            note_addr: journal.note_addr,
            order_book: journal.order_book,
            expected_token_contract: journal.expected_token_contract,
            quoted_order: journal.quoted_order,
            quote: journal.quote,
            cursor: journal.cursor,
            ticks: journal.ticks,
            max_price_per_tick: journal.max_price_per_tick,
            escrow: journal.escrow,
            submit_identity: journal.submit_identity,
            created_at_unix: journal.created_at_unix,
            resolved_match: None,
        };
        let converted = super::BuyerSubmitJournal::from(legacy);
        assert_eq!(
            converted.intent.kind,
            super::BuyerSubmitIntentKind::LegacyUnknown
        );
        assert!(converted.intent.predecessor_token_contract.is_none());
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_submit_journal_round_trip_write_load() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-journal-roundtrip");
        let path = dir.join("journal.json");
        let journal = buyer_submit_test_journal();
        super::write_buyer_submit_journal(&path, &journal).unwrap();
        let loaded = super::load_buyer_submit_journal(&path, &journal.note_addr)
            .unwrap()
            .unwrap();
        assert_eq!(loaded, journal);
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_submit_journal_v2_previous_and_current_readers_are_bidirectionally_compatible() {
        let journal = buyer_submit_test_journal();

        let previous_writer = PreviousBuyerSubmitJournalV2::from(&journal);
        let previous_bytes = serde_json::to_vec_pretty(&previous_writer).unwrap();
        let loaded_current: super::BuyerSubmitJournal = serde_json::from_slice(&previous_bytes)
            .expect("previous v2 journal loads on this head");
        assert_eq!(loaded_current, journal);

        let current_bytes = serde_json::to_vec_pretty(&journal).unwrap();
        let loaded_previous: PreviousBuyerSubmitJournalV2 = serde_json::from_slice(&current_bytes)
            .expect(
                "journal written by this head remains readable by the previous strict v2 reader",
            );
        assert_eq!(loaded_previous.schema, super::BUYER_SUBMIT_JOURNAL_SCHEMA);
        assert_eq!(loaded_previous.submit_identity, journal.submit_identity);

        let current_shape = serde_json::to_value(&journal).unwrap();
        let previous_shape = serde_json::to_value(previous_writer).unwrap();
        assert_eq!(
            current_shape
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            previous_shape
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            "schema v2 field names must remain byte-shape compatible"
        );
    }

    #[cfg(feature = "shellnet")]
    fn subscription_submit_test_journal() -> super::BuyerSubscriptionSubmitJournal {
        let frame_model = "qwen--qwen3--32b";
        let ticks = 2;
        let max_price_per_tick = 1_000_000;
        let escrow = dexdo_core::required_escrow_for_buy(ticks, max_price_per_tick);
        super::BuyerSubscriptionSubmitJournal {
            schema: super::BUYER_SUBSCRIPTION_SUBMIT_SCHEMA.to_string(),
            note_addr: format!("0:{}", "5".repeat(64)),
            order_book: format!("0:{}", "6".repeat(64)),
            frame_model: frame_model.to_string(),
            model_hash: dexdo_core::model_hash_for(frame_model),
            max_price_per_tick,
            ticks,
            escrow,
            cycle_budget: escrow / super::INFERENCE_SUBSCRIPTION_CYCLES,
            auto_renew: true,
            order_id_floor: 40,
            fill_cursor: dexdo_core::MatchWatchCursor::new(1_000),
            submit_identity: format!("boc-sha256:{}", "b".repeat(64)),
            created_at_unix: 1_000,
        }
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn subscription_journal_and_state_round_trip() {
        let (dir, _cleanup) = buyer_journal_test_dir("subscription-journal-roundtrip");
        let journal_path = dir.join("journal.json");
        let state_path = dir.join("subscriptions.json");
        let journal = subscription_submit_test_journal();
        super::write_buyer_subscription_submit_journal(&journal_path, &journal).unwrap();
        let loaded = super::load_buyer_money_journal(&journal_path, &journal.note_addr)
            .unwrap()
            .unwrap();
        let super::BuyerMoneyJournal::Subscription(loaded) = loaded else {
            panic!("subscription schema must dispatch to a subscription journal");
        };
        assert_eq!(*loaded, journal);

        let mut state = super::BuyerSubscriptionState::empty(&journal.note_addr).unwrap();
        super::ensure_subscription_book(
            &mut state,
            &journal.order_book,
            &journal.frame_model,
            &journal.model_hash,
            &journal.fill_cursor,
        )
        .unwrap();
        super::write_buyer_subscription_state(&state_path, &state).unwrap();
        assert_eq!(
            super::load_buyer_subscription_state(&state_path, &journal.note_addr).unwrap(),
            state
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn unattributed_fill_routes_to_matching_subscription_record() {
        let journal = subscription_submit_test_journal();
        let token_contract = format!("0:{}", "7".repeat(64));
        let mut state = super::BuyerSubscriptionState::empty(&journal.note_addr).unwrap();
        let book_index = super::ensure_subscription_book(
            &mut state,
            &journal.order_book,
            &journal.frame_model,
            &journal.model_hash,
            &journal.fill_cursor,
        )
        .unwrap();
        state.books[book_index]
            .unattributed_matches
            .push(super::BuyerJournalMatch {
                token_contract: token_contract.clone(),
                order_id: 41,
                ticks: 1,
                clearing_price: 900_000,
            });
        super::record_subscription_placements(
            &mut state,
            book_index,
            &journal,
            &[dexdo_core::InferenceSubscriptionPlacement {
                order_id: 41,
                buyer_note: journal.note_addr.clone(),
                max_price_per_tick: journal.max_price_per_tick,
                ticks: journal.ticks,
                cycle_budget: journal.cycle_budget,
                auto_renew: journal.auto_renew,
                created_at: 1_001,
            }],
        )
        .unwrap();
        assert!(state.books[book_index].unattributed_matches.is_empty());
        assert_eq!(
            state.books[book_index].subscriptions[0].matches[0].token_contract,
            token_contract
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn subscription_reconcile_retains_ambiguous_and_clears_proven_results() {
        let (dir, _cleanup) = buyer_journal_test_dir("subscription-reconcile");
        let journal_path = dir.join("journal.json");
        let state_path = dir.join("subscriptions.json");
        let journal = subscription_submit_test_journal();
        super::write_buyer_subscription_submit_journal(&journal_path, &journal).unwrap();

        let ambiguous_chain = RecordingRecoveryChain {
            subscription_placement_error: true,
            ..Default::default()
        };
        let error = super::reconcile_subscription_submit_with_backend(
            &ambiguous_chain,
            &journal_path,
            &state_path,
            &journal,
            None,
        )
        .await
        .expect_err("ambiguous placement read must retain the journal");
        assert!(super::is_ambiguous_submit_error(&error), "{error:#}");
        assert!(journal_path.exists());

        let second_placement = dexdo_core::InferenceSubscriptionPlacement {
            order_id: 42,
            buyer_note: journal.note_addr.clone(),
            max_price_per_tick: journal.max_price_per_tick,
            ticks: journal.ticks,
            cycle_budget: journal.cycle_budget,
            auto_renew: journal.auto_renew,
            created_at: 1_002,
        };
        let two_placements_chain = RecordingRecoveryChain {
            subscription_placements: vec![
                dexdo_core::InferenceSubscriptionPlacement {
                    order_id: 41,
                    buyer_note: journal.note_addr.clone(),
                    max_price_per_tick: journal.max_price_per_tick,
                    ticks: journal.ticks,
                    cycle_budget: journal.cycle_budget,
                    auto_renew: journal.auto_renew,
                    created_at: 1_001,
                },
                second_placement,
            ],
            subscription_order_active: true,
            ..Default::default()
        };
        for retry in 1..=2 {
            let error = super::reconcile_subscription_submit_with_backend(
                &two_placements_chain,
                &journal_path,
                &state_path,
                &journal,
                None,
            )
            .await
            .expect_err("two correlated placements must fail closed");
            assert!(super::is_ambiguous_submit_error(&error), "{error:#}");
            assert!(
                journal_path.exists(),
                "retry {retry} must retain the journal and cannot reach a second POST"
            );
        }
        assert_eq!(
            two_placements_chain
                .subscription_placement_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both command retries must reconcile the retained journal"
        );

        let rejected: anyhow::Result<()> =
            Err(anyhow::Error::new(dexdo_core::MoneySubmitError::Rejected {
                source: anyhow::anyhow!("authoritative submit rejection"),
            }));
        assert!(
            !super::retain_subscription_journal_after_submit_result(&journal_path, &rejected)
                .unwrap()
        );
        assert!(!journal_path.exists());

        super::write_buyer_subscription_submit_journal(&journal_path, &journal).unwrap();
        let ambiguous: anyhow::Result<()> = Err(anyhow::Error::new(
            dexdo_core::MoneySubmitError::Ambiguous {
                source: anyhow::anyhow!("POST response lost"),
            },
        ));
        assert!(
            super::retain_subscription_journal_after_submit_result(&journal_path, &ambiguous)
                .unwrap()
        );
        assert!(journal_path.exists());

        let unclassified: anyhow::Result<()> = Err(anyhow::anyhow!("unknown submit outcome"));
        assert!(super::retain_subscription_journal_after_submit_result(
            &journal_path,
            &unclassified
        )
        .unwrap());
        assert!(journal_path.exists());

        let placement = dexdo_core::InferenceSubscriptionPlacement {
            order_id: 41,
            buyer_note: journal.note_addr.clone(),
            max_price_per_tick: journal.max_price_per_tick,
            ticks: journal.ticks,
            cycle_budget: journal.cycle_budget,
            auto_renew: journal.auto_renew,
            created_at: 1_001,
        };
        let proven_chain = RecordingRecoveryChain {
            subscription_placements: vec![placement],
            subscription_order_active: true,
            ..Default::default()
        };
        let placements = super::reconcile_subscription_submit_with_backend(
            &proven_chain,
            &journal_path,
            &state_path,
            &journal,
            None,
        )
        .await
        .expect("authoritative placement reconciles");
        assert_eq!(placements.len(), 1);
        assert!(!journal_path.exists());
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_money_lock_acquire_and_try_acquire_serialize() {
        use sha2::Digest;

        let note_addr = format!(
            "0:{}",
            hex::encode(sha2::Sha256::digest(
                format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )
                .as_bytes()
            ))
        );
        let mut first = super::BuyerMoneyLock::open(&note_addr).unwrap();
        let mut second = super::BuyerMoneyLock::open(&note_addr).unwrap();
        first.acquire().unwrap();
        let error = second.try_acquire().unwrap_err().to_string();
        assert!(error.contains("another money submission"), "{error}");
        assert!(error.contains("no BOC was sent"), "{error}");
        drop(first);
        second.try_acquire().unwrap();
    }

    #[cfg(all(feature = "shellnet", unix))]
    #[test]
    fn non_regular_buyer_journal_path_is_rejected() {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-journal-nonregular");
        let journal_dir = dir.join("journal.json");
        std::fs::create_dir(&journal_dir).unwrap();
        let note_addr = format!("0:{}", "1".repeat(64));
        let error = super::load_buyer_money_journal(&journal_dir, &note_addr)
            .unwrap_err()
            .to_string();
        assert!(error.contains("regular file"), "{error}");
    }

    #[cfg(feature = "shellnet")]
    fn dexdo_pn_pool_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[cfg(feature = "shellnet")]
    struct EnvVarGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    #[cfg(feature = "shellnet")]
    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }

        fn unset(key: &'static str) -> Self {
            let old = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, old }
        }
    }

    #[cfg(feature = "shellnet")]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.old.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
