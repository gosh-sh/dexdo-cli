//! Reconcile usage history against immutable buyer-side money evidence. Never submits transactions.
use super::deals::{self, DealHandle, DealHandleRole};
use anyhow::{anyhow, bail, Context, Result};
use dexdo::buyer::usage::{
    AccrualFact, ExpenseBreakdown, OperationalFact, SettlementFact, UsageStore,
};
use dexdo_core::{
    Address, Deployed, RealChainBackend, TokenContractReceiptChainData,
    TokenContractSettlementEvent as Event,
};
use std::collections::BTreeSet;
use std::path::Path;

fn same_address(left: &str, right: &str) -> bool {
    match (Address::parse(left), Address::parse(right)) {
        (Ok(left), Ok(right)) => left
            .with_workchain()
            .eq_ignore_ascii_case(&right.with_workchain()),
        _ => false,
    }
}

/// Only explicit monetary events, never a reserve or a token/clock-based estimate. ProbeAccepted
/// proves its principal; TickFinalized proves the cumulative escrow debit (including accrued fees).
/// Ordinary claims do not debit escrow until a terminal settlement, so they are not exported here.
fn accruals(
    handle: &DealHandle,
    chain: &TokenContractReceiptChainData,
) -> Result<Vec<AccrualFact>> {
    let events = chain.receipts.current_lifecycle();
    let escrow = events
        .iter()
        .find_map(|receipt| match &receipt.event {
            Event::StreamFunded { buyer, deposit } if same_address(buyer, &handle.note_addr) => {
                Some(*deposit)
            }
            _ => None,
        })
        .context("buyer escrow funding is unproven")?;
    let mut confirmed = 0u128;
    let mut accepted = false;
    let mut seen = std::collections::BTreeMap::new();
    let mut facts = Vec::new();
    for receipt in events {
        if let Some(previous) =
            seen.insert(&receipt.message_id, (&receipt.event, receipt.created_at))
        {
            anyhow::ensure!(
                previous == (&receipt.event, receipt.created_at),
                "conflicting accrual event replay"
            );
            continue;
        }
        let amount = match &receipt.event {
            Event::ProbeAccepted {
                buyer,
                to_seller,
                bond_returned,
            } => {
                anyhow::ensure!(
                    same_address(buyer, &handle.note_addr)
                        && !accepted
                        && *bond_returned == 0
                        && *to_seller > 0,
                    "unsupported probe accrual evidence"
                );
                accepted = true;
                *to_seller
            }
            Event::TickFinalized { deposit, .. } => {
                anyhow::ensure!(accepted, "weekly accrual precedes accepted probe");
                escrow
                    .checked_sub(*deposit)
                    .context("remaining escrow exceeds funded escrow")?
                    .checked_sub(confirmed)
                    .context("confirmed escrow expense decreased")?
            }
            Event::StreamStopped { .. }
            | Event::ProbeBurned { .. }
            | Event::DisputeResolved { .. }
            | Event::StreamReclaimed { .. } => break,
            _ => continue,
        };
        confirmed = confirmed
            .checked_add(amount)
            .context("accrual total overflow")?;
        anyhow::ensure!(confirmed <= escrow, "accruals exceed funded escrow");
        if amount > 0 {
            facts.push(AccrualFact {
                source_id: format!("buyer-accrual:{}", receipt.message_id),
                occurred_at: i64::try_from(receipt.created_at)
                    .context("accrual time exceeds i64")?,
                spent_raw: amount,
            });
        }
    }
    Ok(facts)
}

/// Classify only what these receipts prove. Terminal totals can include returned seller bonds,
/// rebates and penalties; an inseparable amount remains `other` instead of guessed model work.
fn classify_expenses(
    store: &UsageStore,
    handle: &DealHandle,
    chain: &TokenContractReceiptChainData,
    accrued: &[AccrualFact],
) -> Result<()> {
    let mut principal = 0_u128;
    let mut withdrawn = 0_u128;
    let mut seen = BTreeSet::new();
    for receipt in chain.receipts.current_lifecycle() {
        if !seen.insert(&receipt.message_id) {
            continue;
        }
        let mut parts = ExpenseBreakdown::default();
        let source = match receipt.event {
            Event::ProbeAccepted { to_seller, .. } => {
                // The receipt irrevocably pays for one tick. Later completed accepted
                // work consumes that same capacity; claims beyond it are not new expenses.
                let tokens = store
                    .accepted_tokens(&handle.token_contract)?
                    .min(dexdo_core::params::TICK_SIZE);
                parts.consumption_raw = dexdo::buyer::usage::paid_probe_cost(to_seller, tokens);
                parts.probe_consumption_tokens = Some(tokens);
                principal = to_seller;
                format!("buyer-accrual:{}", receipt.message_id)
            }
            Event::ShellWithdrawn { amount, .. } => {
                withdrawn = withdrawn
                    .checked_add(amount)
                    .context("seller withdrawal total overflow")?;
                continue;
            }
            Event::TickFinalized { finalized_owed, .. } => {
                let credited = finalized_owed
                    .checked_add(withdrawn)
                    .context("seller credit total overflow")?;
                let delta = credited
                    .checked_sub(principal)
                    .context("seller principal decreased")?;
                let source = format!("buyer-accrual:{}", receipt.message_id);
                let fact = accrued
                    .iter()
                    .find(|fact| fact.source_id == source)
                    .context("weekly expense fact missing")?;
                parts.commission_raw = fact
                    .spent_raw
                    .checked_sub(delta)
                    .context("seller principal exceeds weekly expense")?;
                principal = credited;
                source
            }
            Event::ProbeBurned { burned_probe, .. } => {
                parts.burns_raw = burned_probe;
                format!("buyer-settlement:{}", receipt.message_id)
            }
            Event::StreamStopped { .. }
            | Event::DisputeResolved { .. }
            | Event::StreamReclaimed { .. } => break,
            _ => continue,
        };
        store.classify_expense(&source, parts)?;
    }
    Ok(())
}

/// A settlement is confirmed only after matching the deal's declared refund against the actual
/// buyer-note credits. Missing credits never silently turn into a zero refund.
fn settlement_with_contributions(
    handle: &DealHandle,
    chain: &TokenContractReceiptChainData,
    contributions: &std::collections::BTreeMap<String, u128>,
) -> Result<Option<SettlementFact>> {
    if !same_address(&handle.token_contract, &chain.account_id) {
        bail!("usage receipt belongs to a different deal");
    }
    let events = chain.receipts.current_lifecycle();
    // This address can be reused after cleanup without redeployment. A local address-only handle
    // cannot prove which of two funding cycles a request belongs to, so do not merge their money.
    if chain
        .receipts
        .events
        .iter()
        .filter(|receipt| matches!(receipt.event, Event::StreamFunded { .. }))
        .map(|receipt| &receipt.message_id)
        .collect::<BTreeSet<_>>()
        .len()
        > 1
    {
        bail!("multiple funding cycles require separate deal identity");
    }
    let funded = events.iter().find_map(|receipt| match &receipt.event {
        Event::StreamFunded { buyer, deposit } if same_address(buyer, &handle.note_addr) => {
            Some((receipt, *deposit))
        }
        _ => None,
    });
    let Some((funding, escrow)) = funded else {
        bail!("buyer escrow funding is unproven")
    };
    if chain.current.as_ref().is_some_and(|current| {
        current
            .parties
            .get("buyerNote")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|buyer| !same_address(buyer, &handle.note_addr))
    }) {
        bail!("current deal buyer differs from persisted buyer note");
    }
    let book_paid = *contributions
        .get(&funding.message_id)
        .context("book funding contribution unproven")?;
    let mut bond = book_paid
        .checked_sub(escrow)
        .context("book contribution is below recorded escrow")?;
    let mut excess_refunded = 0u128;
    let mut seen_bonds = std::collections::BTreeMap::new();
    for receipt in events {
        if let Event::BuyerBondFunded { amount } = receipt.event {
            if let Some(previous) = seen_bonds.insert(&receipt.message_id, amount) {
                if previous != amount {
                    bail!("conflicting buyer bond event replay");
                }
            } else {
                let paid = *contributions
                    .get(&receipt.message_id)
                    .context("buyer bond contribution unproven")?;
                let excess = paid
                    .checked_sub(amount)
                    .context("buyer bond contribution below event")?;
                excess_refunded = excess_refunded
                    .checked_add(excess)
                    .context("buyer excess total overflow")?;
                bond = bond
                    .checked_add(paid)
                    .context("buyer bond total overflow")?;
            }
        }
    }
    let total_debit = escrow
        .checked_add(bond)
        .context("buyer funding total overflow")?;
    let terminal = events
        .iter()
        .rev()
        .find_map(|receipt| match &receipt.event {
            Event::StreamStopped {
                buyer,
                refund_to_buyer,
                ..
            }
            | Event::ProbeBurned {
                buyer,
                refund_to_buyer,
                ..
            }
            | Event::StreamReclaimed {
                buyer,
                refund_to_buyer,
            } if same_address(buyer, &handle.note_addr) => Some((receipt, *refund_to_buyer)),
            Event::DisputeResolved {
                refund_to_buyer, ..
            } => Some((receipt, *refund_to_buyer)),
            _ => None,
        });
    // An opened stream requires its buyer bond. Subscriptions prove it inside the book payment;
    // ordinary streams prove it through BuyerBondFunded. Do not freeze a missing contribution
    // under an immutable terminal id while the bond event is still catching up in the indexer.
    if terminal.is_some() && bond == 0 {
        bail!("terminal deal buyer bond contribution is unproven");
    }
    let mut credits = 0u128;
    let mut seen_credits = std::collections::BTreeMap::new();
    let mut latest_credit_time = funding.created_at;
    for credit in &chain.note_credits {
        if !same_address(&credit.deal, &handle.token_contract)
            || !same_address(&credit.note, &handle.note_addr)
        {
            continue;
        }
        if credit.created_at < funding.created_at {
            bail!("buyer credit predates this funding cycle");
        }
        if let Some(previous) = seen_credits.insert(&credit.message_id, credit.amount) {
            if previous != credit.amount {
                bail!("conflicting buyer credit replay");
            }
        } else {
            credits = credits
                .checked_add(credit.amount)
                .context("buyer credit total overflow")?;
            latest_credit_time = latest_credit_time.max(credit.created_at);
        }
    }
    let notes_read = chain
        .notes_read
        .iter()
        .any(|note| same_address(note, &handle.note_addr));
    let (source, occurred_at, expected_credit) = if let Some((terminal, refund)) = terminal {
        (
            terminal.message_id.clone(),
            terminal.created_at,
            refund
                .checked_add(excess_refunded)
                .context("buyer expected refund overflow")?,
        )
    } else {
        let idle = !chain.account_active
            || chain.current.as_ref().is_some_and(|current| {
                current
                    .state
                    .get("funded")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
                    && current
                        .state
                        .get("opened")
                        .and_then(serde_json::Value::as_bool)
                        == Some(false)
            });
        if !idle || credits != total_debit || !notes_read {
            return Ok(None);
        }
        // cleanupUnopened emits no settlement event. Its full refund is independently proven by
        // the buyer's own credits, plus idle/destroyed state; an active funded deal is never final.
        (
            format!("returned:{}", funding.message_id),
            latest_credit_time,
            total_debit,
        )
    };
    if credits > total_debit {
        bail!("buyer credits exceed proven funding");
    }
    let complete = notes_read && credits == expected_credit;
    Ok(Some(SettlementFact {
        source_id: format!("buyer-settlement:{source}"),
        deal_id: handle.token_contract.clone(),
        occurred_at: i64::try_from(occurred_at).context("settlement time exceeds i64")?,
        escrow_raw: escrow,
        buyer_bond_raw: bond,
        buyer_credit_raw: complete.then_some(credits),
    }))
}

/// Include BUY fees even when no deal filled. Caller supplies the instance's current note.
pub(crate) async fn reconcile_note(
    store: &UsageStore,
    manifest: &Path,
    note: &Address,
) -> Result<()> {
    let note_id = note.with_workchain();
    store.mark_note_operations_incomplete(&note_id, "BUY_history_reconciliation_pending")?;
    let deployed = Deployed::load(manifest)?;
    if deployed.network != store.network() {
        bail!("usage manifest network differs from store");
    }
    let backend = RealChainBackend::connect(manifest)?;
    if !backend.buyer_usage_note_code_verified(note).await? {
        bail!("BUY expense needs the pinned PrivateNote code");
    }
    let mut models = std::collections::BTreeMap::new();
    for receipt in backend.buyer_usage_buy_receipts(note).await? {
        if let std::collections::btree_map::Entry::Vacant(entry) =
            models.entry(receipt.model_hash.clone())
        {
            let model = match backend
                .buyer_usage_buy_model_name(&receipt.model_hash)
                .await
            {
                Ok(model) => model,
                Err(error) => {
                    tracing::warn!(%error, model_hash=%receipt.model_hash, "BUY model metadata unavailable");
                    None
                }
            };
            entry.insert(model);
        }
        // This is a proven successful execution of pinned code whose direct burn precedes the
        // outbound book message. A later book bounce refunds escrow, never the already burnt fee.
        store.apply_operational_expense(OperationalFact {
            source_id: format!("buyer-buy:{}", receipt.message_id),
            deal_id: None,
            // Attribute the original BUY model even when it never matched a deal.
            model: models.get(&receipt.model_hash).cloned().flatten(),
            occurred_at: i64::try_from(receipt.created_at).context("BUY time exceeds i64")?,
            spent_raw: dexdo_core::params::BUY_ORDER_GAS_RAW,
        })?;
    }
    store.mark_note_operations_complete(&note_id)?;
    Ok(())
}

/// Retry all persisted buyer handles. Both restart and seller changes use the same durable scope.
pub(crate) async fn reconcile(store: &UsageStore, deals_dir: &Path) -> Result<()> {
    let handles = deals::list_deal_handles_strict(deals_dir)?;
    let mut notes_read = BTreeSet::new();
    let mut errors = Vec::new();
    for (_, handle) in handles {
        if handle.role != DealHandleRole::Buyer || handle.network != store.network() {
            continue;
        }
        store.register_deal(&handle.token_contract, &handle.frame_model)?;
        let result = reconcile_deal(store, &handle).await;
        if let Err(error) = result {
            store
                .mark_operations_incomplete(&handle.token_contract, "chain_evidence_unavailable")?;
            errors.push(format!("{}: {error}", handle.handle));
        }
        if notes_read.insert(handle.note_addr.clone()) {
            match Address::parse(&handle.note_addr) {
                Ok(note) => {
                    if let Err(error) =
                        reconcile_note(store, Path::new(&handle.contracts), &note).await
                    {
                        errors.push(format!("{} BUY history: {error}", handle.handle));
                    }
                }
                Err(error) => errors.push(format!("{} note: {error}", handle.handle)),
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(errors.join("; ")))
    }
}

async fn reconcile_deal(store: &UsageStore, handle: &DealHandle) -> Result<()> {
    let manifest = Path::new(&handle.contracts);
    let deployed = Deployed::load(manifest)?;
    if deployed.network != handle.network {
        bail!("persisted deal manifest changed network");
    }
    let backend = RealChainBackend::connect(manifest)?;
    let token_contract = Address::parse(&handle.token_contract)?;
    let note = Address::parse(&handle.note_addr)?;
    store.mark_operations_incomplete(&handle.token_contract, "operation_reconciliation_pending")?;
    let chain = backend
        .token_contract_receipt_chain_data(&token_contract)
        .await?;
    let mut contributions = std::collections::BTreeMap::new();
    for receipt in chain.receipts.current_lifecycle() {
        let buyer_bond = match receipt.event {
            Event::StreamFunded { .. } => false,
            Event::BuyerBondFunded { .. } => true,
            _ => continue,
        };
        if contributions.contains_key(&receipt.message_id) {
            continue;
        }
        let amount = backend
            .buyer_usage_funding_contribution(
                &token_contract,
                &note,
                &receipt.message_id,
                buyer_bond,
            )
            .await?;
        contributions.insert(receipt.message_id.clone(), amount);
    }
    let settlement = match settlement_with_contributions(handle, &chain, &contributions) {
        Ok(fact) => fact,
        Err(error) => {
            store.mark_deal_incomplete(&handle.token_contract, "settlement_evidence_incomplete")?;
            return Err(error);
        }
    };
    let accrued = accruals(handle, &chain)?;
    // Weekly deltas depend on the ORIGINAL escrow, not only on the later remaining deposit.
    // Bind that base to its immutable event too, rather than trusting the indexer's decoded body.
    for receipt in chain.receipts.current_lifecycle() {
        if matches!(receipt.event, Event::StreamFunded { .. }) {
            backend
                .buyer_usage_verify_deal_receipt(&token_contract, receipt)
                .await?;
        }
    }
    for receipt in chain.receipts.current_lifecycle() {
        if !matches!(
            receipt.event,
            Event::ProbeAccepted { .. }
                | Event::StreamOpened { .. }
                | Event::TickFinalized { .. }
                | Event::ShellWithdrawn { .. }
                | Event::ProbeBurned { .. }
        ) {
            continue;
        }
        backend
            .buyer_usage_verify_deal_receipt(&token_contract, receipt)
            .await?;
        if let Event::StreamOpened {
            buyer,
            price_per_tick,
        } = &receipt.event
        {
            anyhow::ensure!(
                same_address(buyer, &handle.note_addr),
                "execution price belongs to another buyer"
            );
            store.record_deal_price(&handle.token_contract, *price_per_tick)?;
        }
    }
    store.apply_deal_expenses(&handle.token_contract, &accrued, settlement)?;
    classify_expenses(store, handle, &chain, &accrued)?;
    if !backend.buyer_usage_note_code_verified(&note).await? {
        bail!("buyer operation expense needs the pinned PrivateNote code");
    }
    let operations = backend
        .buyer_usage_deal_operations(&note, &token_contract)
        .await?;
    for operation in operations.operations {
        store.apply_operational_expense(OperationalFact {
            source_id: operation.source_id,
            deal_id: Some(handle.token_contract.clone()),
            model: Some(handle.frame_model.clone()),
            occurred_at: i64::try_from(operation.created_at)
                .context("operation time exceeds i64")?,
            spent_raw: operation.spent_raw,
        })?;
    }
    if operations.incomplete.is_empty() {
        store.mark_operations_complete(&handle.token_contract)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{
        NoteDealCreditReceipt, TokenContractSettlementReceipt, TokenContractSettlementReceipts,
    };

    fn address(byte: char) -> String {
        format!("0:{}", byte.to_string().repeat(64))
    }

    #[test]
    fn expense_split_2132_uses_only_paid_probe_work_and_preserves_other() {
        use dexdo_core::params::{SHELL_UNIT, TICK_SIZE};
        let large_price = u128::MAX / 8 / dexdo_core::PRICE_STEP * dexdo_core::PRICE_STEP;
        for price in [SHELL_UNIT, large_price] {
            let (handle, mut chain) = fixture();
            chain
                .receipts
                .events
                .retain(|receipt| receipt.message_id == "fund");
            chain.receipts.events[0].created_at = 98;
            if let Event::StreamFunded { deposit, .. } = &mut chain.receipts.events[0].event {
                *deposit = price;
            }
            chain.receipts.events.push(TokenContractSettlementReceipt {
                message_id: "probe".into(),
                created_at: 100,
                cursor: "probe".into(),
                event: Event::ProbeAccepted {
                    buyer: handle.note_addr.clone(),
                    to_seller: price,
                    bond_returned: 0,
                },
            });
            let dir = tempfile::tempdir().unwrap();
            let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
            store
                .register_deal(&handle.token_contract, &handle.frame_model)
                .unwrap();
            store
                .start_request_at("r", &handle.frame_model, "openai", false, 99_000)
                .unwrap();
            store.link_deal("r", &handle.token_contract).unwrap();
            let usage = dexdo_proto::BillingUsage::new(TICK_SIZE as u64 / 2 - 1, 1, None).unwrap();
            store
                .record_call_usage("r", "verification_b8", "b8", usage, true)
                .unwrap();
            store
                .finish_request_at("r", 502, Some("error"), Some("verification_b8"), 99_500)
                .unwrap();
            let facts = accruals(&handle, &chain).unwrap();
            store
                .apply_deal_expenses(&handle.token_contract, &facts, None)
                .unwrap();
            for _ in 0..2 {
                classify_expenses(&store, &handle, &chain, &facts).unwrap();
            }
            let summary = store
                .query(
                    &dexdo::buyer::usage::UsageQuery::default(),
                    chrono::Utc::now(),
                )
                .unwrap();
            let parsed = |key: &str| {
                let decimal = summary["spend"]["total"][key].as_str().unwrap();
                dexdo_core::params::shell_amount_raw(decimal).unwrap()
            };
            assert_eq!(parsed("cost_shell"), price / 2);
            assert_eq!(parsed("overhead_cost_shell"), 0);
            assert_eq!(
                summary["spend"]["total"]["expense_breakdown"]["other_shell"],
                dexdo_core::params::shell_amount(price - price / 2)
            );
            assert_eq!(
                summary["spend"]["total"]["expense_breakdown"]["commission_shell"],
                "0"
            );
            // Accepted work after the probe still consumes that already paid tick,
            // including verification followed by an HTTP failure.
            store
                .start_request_at("later", &handle.frame_model, "openai", false, 101_000)
                .unwrap();
            store.link_deal("later", &handle.token_contract).unwrap();
            store
                .record_call_usage("later", "verification_b7", "b7", usage, true)
                .unwrap();
            store
                .finish_request_at(
                    "later",
                    502,
                    Some("error"),
                    Some("verification_b7"),
                    102_000,
                )
                .unwrap();
            classify_expenses(&store, &handle, &chain, &facts).unwrap();
            let later = store
                .query(&Default::default(), chrono::Utc::now())
                .unwrap();
            assert_eq!(
                later["spend"]["total"]["cost_shell"],
                dexdo_core::params::shell_amount(price)
            );
            // Reconciliation is idempotent and cannot bill beyond the already paid tick.
            let exported = serde_json::to_value(store.facts(&Default::default()).unwrap()).unwrap();
            assert_eq!(
                exported["facts"][0]["cost"],
                later["spend"]["total"]["cost_shell"]
            );
            assert_eq!(
                exported["facts"][0]["paid_probe"]["accepted_tokens"],
                TICK_SIZE.to_string()
            );
            classify_expenses(&store, &handle, &chain, &facts).unwrap();
            assert_eq!(
                serde_json::to_value(store.facts(&Default::default()).unwrap()).unwrap(),
                exported
            );
        }
    }

    #[test]
    fn weekly_commission_2132_accounts_for_prior_seller_withdrawals() {
        use dexdo_core::params::SHELL_UNIT;
        let (handle, mut chain) = fixture();
        chain
            .receipts
            .events
            .retain(|receipt| receipt.message_id == "fund");
        if let Event::StreamFunded { deposit, .. } = &mut chain.receipts.events[0].event {
            *deposit = 4 * SHELL_UNIT;
        }
        for (id, event) in [
            (
                "probe",
                Event::ProbeAccepted {
                    buyer: handle.note_addr.clone(),
                    to_seller: SHELL_UNIT,
                    bond_returned: 0,
                },
            ),
            (
                "withdraw",
                Event::ShellWithdrawn {
                    recipient: address('3'),
                    amount: SHELL_UNIT / 2,
                },
            ),
            (
                "week",
                Event::TickFinalized {
                    finalized_owed: 3 * SHELL_UNIT / 2,
                    deposit: 2 * SHELL_UNIT - SHELL_UNIT / 20,
                },
            ),
        ] {
            chain.receipts.events.push(TokenContractSettlementReceipt {
                message_id: id.into(),
                created_at: 100,
                cursor: id.into(),
                event,
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        store
            .register_deal(&handle.token_contract, &handle.frame_model)
            .unwrap();
        let facts = accruals(&handle, &chain).unwrap();
        store
            .apply_deal_expenses(&handle.token_contract, &facts, None)
            .unwrap();
        classify_expenses(&store, &handle, &chain, &facts).unwrap();
        let snapshot = store
            .query(
                &dexdo::buyer::usage::UsageQuery::default(),
                chrono::Utc::now(),
            )
            .unwrap();
        assert_eq!(
            snapshot["spend"]["total"]["expense_breakdown"]["commission_shell"],
            "0.05"
        );
        assert_eq!(
            snapshot["spend"]["total"]["expense_breakdown"]["other_shell"],
            "2"
        );
        assert_eq!(snapshot["spend"]["total"]["overhead_cost_shell"], "0.05");
        assert_eq!(store.facts(&Default::default()).unwrap().facts.len(), 2);
    }

    fn fixture() -> (DealHandle, TokenContractReceiptChainData) {
        let handle: DealHandle = serde_json::from_value(serde_json::json!({
            "version": 5, "handle": "deal-fixture", "role": "buyer", "network": "shellnet",
            "token_contract": address('1'), "note_addr": address('2'), "frame_model": "fixture--model--v1",
            "model_hash": null, "order_book": null, "root_model": null, "market": null,
            "contracts": "unused.json", "endpoint": null, "created_order_ids": [], "created_at_unix": 1
        })).expect("deal fixture");
        let receipt = |id: &str, event| TokenContractSettlementReceipt {
            message_id: id.to_string(),
            created_at: 100,
            cursor: id.to_string(),
            event,
        };
        let chain = TokenContractReceiptChainData {
            account_id: handle.token_contract.clone(),
            account_active: false,
            code_hash: None,
            current: None,
            receipts: TokenContractSettlementReceipts {
                events: vec![
                    receipt(
                        "fund",
                        Event::StreamFunded {
                            buyer: handle.note_addr.clone(),
                            deposit: 1_000,
                        },
                    ),
                    receipt("bond", Event::BuyerBondFunded { amount: 200 }),
                    receipt(
                        "stop",
                        Event::StreamStopped {
                            buyer: handle.note_addr.clone(),
                            to_seller: 300,
                            refund_to_buyer: 800,
                        },
                    ),
                ],
            },
            note_credits: vec![NoteDealCreditReceipt {
                note: handle.note_addr.clone(),
                deal: handle.token_contract.clone(),
                amount: 800,
                message_id: "credit".into(),
                created_at: 101,
                cursor: "credit".into(),
            }],
            notes_read: vec![handle.note_addr.clone()],
        };
        (handle, chain)
    }

    fn settlement(
        handle: &DealHandle,
        chain: &TokenContractReceiptChainData,
    ) -> Result<Option<SettlementFact>> {
        let contributions = chain
            .receipts
            .events
            .iter()
            .filter_map(|receipt| {
                let amount = match receipt.event {
                    Event::StreamFunded { deposit, .. } => deposit,
                    Event::BuyerBondFunded { amount } => amount,
                    _ => return None,
                };
                Some((receipt.message_id.clone(), amount))
            })
            .collect();
        settlement_with_contributions(handle, chain, &contributions)
    }

    #[test]
    fn subscription_bond_comes_from_book_contribution_without_bond_event() {
        let (handle, mut chain) = fixture();
        chain.receipts.events.remove(1);
        let contributions = [("fund".to_string(), 1_200)].into_iter().collect();
        let fact = settlement_with_contributions(&handle, &chain, &contributions)
            .unwrap()
            .unwrap();
        assert_eq!(fact.escrow_raw, 1_000);
        assert_eq!(fact.buyer_bond_raw, 200);
        assert_eq!(fact.buyer_credit_raw, Some(800));
    }

    #[test]
    fn excess_bond_contribution_and_return_are_both_accounted() {
        let (handle, mut chain) = fixture();
        let mut excess_credit = chain.note_credits[0].clone();
        excess_credit.amount = 50;
        excess_credit.message_id = "excess-credit".into();
        chain.note_credits.push(excess_credit);
        let contributions = [("fund".to_string(), 1_000), ("bond".to_string(), 250)]
            .into_iter()
            .collect();
        let fact = settlement_with_contributions(&handle, &chain, &contributions)
            .unwrap()
            .unwrap();
        assert_eq!(fact.buyer_bond_raw, 250);
        assert_eq!(fact.buyer_credit_raw, Some(850));
        assert_eq!(
            fact.escrow_raw + fact.buyer_bond_raw - fact.buyer_credit_raw.unwrap(),
            400
        );
    }

    #[test]
    fn confirmed_expense_includes_bond_and_preserves_chain_time() {
        let (handle, chain) = fixture();
        let fact = settlement(&handle, &chain).unwrap().unwrap();
        assert_eq!(
            fact.escrow_raw + fact.buyer_bond_raw - fact.buyer_credit_raw.unwrap(),
            400
        );
        assert_eq!(fact.occurred_at, 100);
    }

    #[test]
    fn missing_credit_is_unknown_and_can_later_reconcile() {
        let (handle, mut chain) = fixture();
        let credits = std::mem::take(&mut chain.note_credits);
        let incomplete = settlement(&handle, &chain).unwrap().unwrap();
        assert_eq!(incomplete.buyer_credit_raw, None);
        chain.note_credits = credits;
        let complete = settlement(&handle, &chain).unwrap().unwrap();
        assert_eq!(complete.source_id, incomplete.source_id);
        assert_eq!(complete.buyer_credit_raw, Some(800));
    }

    #[test]
    fn delayed_buyer_bond_event_does_not_freeze_an_incorrect_gross_contribution() {
        let (handle, mut chain) = fixture();
        let bond = chain.receipts.events.remove(1);
        assert!(settlement(&handle, &chain).is_err());
        chain.receipts.events.insert(1, bond);
        let recovered = settlement(&handle, &chain).unwrap().unwrap();
        assert_eq!(recovered.escrow_raw, 1_000);
        assert_eq!(recovered.buyer_bond_raw, 200);
        assert_eq!(recovered.buyer_credit_raw, Some(800));
    }

    #[test]
    fn seller_credits_and_foreign_deal_credits_do_not_refund_buyer() {
        let (handle, mut chain) = fixture();
        chain.note_credits[0].note = address('3');
        assert_eq!(
            settlement(&handle, &chain)
                .unwrap()
                .unwrap()
                .buyer_credit_raw,
            None
        );
        chain.note_credits[0].note = handle.note_addr.clone();
        chain.note_credits[0].deal = address('4');
        assert_eq!(
            settlement(&handle, &chain)
                .unwrap()
                .unwrap()
                .buyer_credit_raw,
            None
        );
    }

    #[test]
    fn duplicate_credit_is_idempotent_and_conflict_is_rejected() {
        let (handle, mut chain) = fixture();
        chain.note_credits.push(chain.note_credits[0].clone());
        assert_eq!(
            settlement(&handle, &chain)
                .unwrap()
                .unwrap()
                .buyer_credit_raw,
            Some(800)
        );
        chain.note_credits[1].amount += 1;
        assert!(settlement(&handle, &chain).is_err());
    }

    #[test]
    fn open_deal_has_no_final_expense() {
        let (handle, mut chain) = fixture();
        chain.receipts.events.pop();
        chain.account_active = true;
        chain.note_credits.clear();
        assert!(settlement(&handle, &chain).unwrap().is_none());
    }

    #[test]
    fn cleanup_requires_full_independent_buyer_refund() {
        let (handle, mut chain) = fixture();
        chain.receipts.events.pop();
        assert!(settlement(&handle, &chain).unwrap().is_none());
        chain.note_credits[0].amount = 1_200;
        let fact = settlement(&handle, &chain).unwrap().unwrap();
        assert_eq!(fact.buyer_credit_raw, Some(1_200));
        assert_eq!(fact.occurred_at, 101);
    }

    #[test]
    fn zero_declared_refund_needs_a_read_buyer_note() {
        let (handle, mut chain) = fixture();
        if let Event::StreamStopped {
            refund_to_buyer, ..
        } = &mut chain.receipts.events[2].event
        {
            *refund_to_buyer = 0;
        }
        chain.note_credits.clear();
        assert_eq!(
            settlement(&handle, &chain)
                .unwrap()
                .unwrap()
                .buyer_credit_raw,
            Some(0)
        );
        chain.notes_read.clear();
        assert_eq!(
            settlement(&handle, &chain)
                .unwrap()
                .unwrap()
                .buyer_credit_raw,
            None
        );
    }

    #[test]
    fn foreign_buyer_or_account_and_ambiguous_reuse_are_rejected() {
        let (mut handle, mut chain) = fixture();
        handle.note_addr = address('9');
        assert!(settlement(&handle, &chain).is_err());
        let (handle, _) = fixture();
        chain.account_id = address('8');
        assert!(settlement(&handle, &chain).is_err());
        chain.account_id = handle.token_contract.clone();
        let mut second_funding = chain.receipts.events[0].clone();
        second_funding.message_id = "another-funding".into();
        chain.receipts.events.push(second_funding);
        assert!(settlement(&handle, &chain).is_err());
    }

    proptest::proptest! {
        #[test]
        fn weekly_expenses_follow_escrow_debits_not_seller_withdrawals(
            probe in 1u128..100, first in 1u128..100, second in 1u128..100,
            withdrawn in 0u128..u128::MAX,
        ) {
            let (handle,mut chain)=fixture();
            chain.receipts.events.truncate(2);
            let mut push=|id:&str,event| chain.receipts.events.push(TokenContractSettlementReceipt {
                message_id:id.into(),created_at:110,cursor:id.into(),event,
            });
            push("probe",Event::ProbeAccepted {buyer:handle.note_addr.clone(),to_seller:probe,bond_returned:0});
            push("week-1",Event::TickFinalized {finalized_owed:first,deposit:1_000-probe-first});
            push("withdraw",Event::ShellWithdrawn {recipient:address('3'),amount:withdrawn});
            push("week-2",Event::TickFinalized {finalized_owed:0,deposit:1_000-probe-first-second});
            let facts=accruals(&handle,&chain).unwrap();
            proptest::prop_assert_eq!(facts.len(),3);
            proptest::prop_assert_eq!(facts.iter().map(|f|f.spent_raw).collect::<Vec<_>>(),vec![probe,first,second]);
            proptest::prop_assert_eq!(&facts[1].source_id,"buyer-accrual:week-1");
        }

        #[test]
        fn buyer_expense_is_independent_of_seller_funding(seller_bond in 0u128..u128::MAX) {
            let (handle, mut chain) = fixture();
            let original = settlement(&handle, &chain).unwrap().unwrap();
            let mut seller = chain.receipts.events[1].clone();
            seller.message_id = "seller-bond".into();
            seller.event = Event::SellerBondFunded { amount: seller_bond };
            chain.receipts.events.push(seller);
            let changed = settlement(&handle, &chain).unwrap().unwrap();
            proptest::prop_assert_eq!(changed.escrow_raw, original.escrow_raw);
            proptest::prop_assert_eq!(changed.buyer_bond_raw, original.buyer_bond_raw);
            proptest::prop_assert_eq!(changed.buyer_credit_raw, original.buyer_credit_raw);
        }
    }

    #[test]
    fn reserve_and_claims_never_masquerade_as_expense_and_probe_survives_close() {
        let (handle, mut chain) = fixture();
        let stop = chain.receipts.events.pop().unwrap();
        assert!(accruals(&handle, &chain).unwrap().is_empty());
        let mut claimed = stop.clone();
        claimed.message_id = "claim".into();
        claimed.event = Event::TicksClaimed {
            trusted: 0,
            claimed: dexdo_core::params::TICK_SIZE,
        };
        chain.receipts.events.push(claimed);
        assert!(accruals(&handle, &chain).unwrap().is_empty());
        let mut probe = stop.clone();
        probe.message_id = "probe".into();
        probe.created_at = 105;
        probe.event = Event::ProbeAccepted {
            buyer: handle.note_addr.clone(),
            to_seller: 100,
            bond_returned: 0,
        };
        chain.receipts.events.push(probe.clone());
        chain.receipts.events.push(probe);
        let open = accruals(&handle, &chain).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].spent_raw, 100);
        assert_eq!(open[0].occurred_at, 105);
        chain.receipts.events.push(stop);
        let closed = accruals(&handle, &chain).unwrap();
        assert_eq!(closed[0].source_id, open[0].source_id);
        assert_eq!(closed[0].spent_raw, open[0].spent_raw);
    }

    #[test]
    fn malformed_accrual_histories_are_rejected() {
        let (handle, mut chain) = fixture();
        chain.receipts.events.truncate(2);
        let mut event = chain.receipts.events[1].clone();
        event.message_id = "weekly".into();
        event.event = Event::TickFinalized {
            finalized_owed: 100,
            deposit: 800,
        };
        chain.receipts.events.push(event);
        assert!(accruals(&handle, &chain).is_err());
        chain.receipts.events.pop();
        let mut probe = chain.receipts.events[1].clone();
        probe.message_id = "probe".into();
        probe.event = Event::ProbeAccepted {
            buyer: address('9'),
            to_seller: 100,
            bond_returned: 0,
        };
        chain.receipts.events.push(probe.clone());
        assert!(accruals(&handle, &chain).is_err());
        probe.event = Event::ProbeAccepted {
            buyer: handle.note_addr.clone(),
            to_seller: 1_001,
            bond_returned: 0,
        };
        *chain.receipts.events.last_mut().unwrap() = probe;
        assert!(accruals(&handle, &chain).is_err());
    }
}
