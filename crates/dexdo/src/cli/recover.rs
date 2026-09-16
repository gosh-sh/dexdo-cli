//! `dexdo` pool-recovery command handlers (`recover`/`dispute`/`reclaim`/`release-dispute`/`withdraw-shell`),
//! extracted from `commands.rs` (move-only / behavior-identical, anti-entropy refactor Track C2).

use crate::cli::args::{
    DisputeArgs, ReclaimArgs, RecoverArgs, ReleaseDisputeArgs, ResolveDisputeTimeoutArgs,
    WithdrawShellArgs,
};
use anyhow::Result;

use crate::cli::commands::{
    persist_pool_recovery_record, resolve_persistable_pool_recovery_inputs,
    resolve_persistable_pool_recovery_inputs_for_deal, resolve_pool_recovery_inputs,
    resolve_pool_recovery_inputs_for_deal, resolve_pool_recovery_plan, AmbiguousRecoveryDeals,
    PoolRecoveryPlan, PoolRecoveryTarget,
};
use crate::cli::support::{load_market, resolve_market_fields};
use serde_json::Value;

fn display_token_contract(value: &dyn std::fmt::Display) -> String {
    dexdo_core::address::display_self_dapp(&value.to_string())
}

fn display_dexdo_address(value: &dyn std::fmt::Display) -> String {
    dexdo_core::address::display(&value.to_string())
}

/// recover an orphaned OPEN deal. The buyer process died mid-stream but the buyer note/key are intact,
/// so no one sent STOP and the deal hangs OPEN (the seller cannot `destroy` an `_opened` deal). `recover`
/// signs the **normal buyer-STOP** (`streamStop(tokenContract)` -> `TokenContract.stop()`, standard
/// split) from the buyer note -- it does NOT place a new buy -- after which the seller `destroy`s the TC.
/// Fails closed (before sending STOP) if the deal is not `_opened`, is `_disputed`, or the note is not the
/// deal's recorded buyer; the on-chain `TC.stop()` also enforces `msg.sender == _buyer`.
#[async_trait::async_trait]
trait RecoverChain: Sync {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>>;
    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>>;
    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>>;
    async fn stop(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<dexdo_core::SettlementActionReceipt>;
    async fn settlement_receipts(
        &self,
        tc: &dexdo_core::Address,
    ) -> Result<dexdo_core::TokenContractSettlementReceipts>;
}

#[async_trait::async_trait]
impl RecoverChain for dexdo_core::RealChainBackend {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>> {
        Ok(self
            .token_contract_deal_snapshot(tc)
            .await?
            .map(|snapshot| snapshot.state))
    }

    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>> {
        Ok(self.token_contract_buyer_note(tc).await?)
    }

    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>> {
        Ok(self.token_contract_buyer_pubkey(tc).await?)
    }

    async fn stop(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<dexdo_core::SettlementActionReceipt> {
        match self.explicit_buyer_stop(note, keys, tc).await? {
            dexdo_core::Settlement::AuthoritativeReceipt(receipt) => Ok(*receipt),
            settlement => Err(anyhow::anyhow!(
                "recover STOP returned non-authoritative settlement projection: {settlement:?}"
            )),
        }
    }

    async fn settlement_receipts(
        &self,
        tc: &dexdo_core::Address,
    ) -> Result<dexdo_core::TokenContractSettlementReceipts> {
        Ok(self.token_contract_settlement_receipts(tc).await?)
    }
}

pub(crate) fn exact_prior_stop_receipt(
    receipts: &dexdo_core::TokenContractSettlementReceipts,
    expected_buyer: &str,
) -> Result<Option<dexdo_core::TokenContractSettlementReceipt>> {
    // Historical settlement events prove terminality, not necessarily who submitted the action.
    // In particular, `StreamStopped.buyer` is the settlement beneficiary for both `stop()` and
    // `sellerStop()`; it must never be interpreted as the action actor.
    let actions = receipts
        .current_lifecycle()
        .iter()
        .filter(|receipt| {
            matches!(
                receipt.event,
                dexdo_core::TokenContractSettlementEvent::ProbeBurned { .. }
                    | dexdo_core::TokenContractSettlementEvent::StreamStopped { .. }
                    | dexdo_core::TokenContractSettlementEvent::StreamDisputed { .. }
                    | dexdo_core::TokenContractSettlementEvent::DisputeResolved { .. }
            )
        })
        .collect::<Vec<_>>();
    let stops = actions
        .iter()
        .filter(|receipt| {
            matches!(
                receipt.event,
                dexdo_core::TokenContractSettlementEvent::ProbeBurned { .. }
                    | dexdo_core::TokenContractSettlementEvent::StreamStopped { .. }
            )
        })
        .collect::<Vec<_>>();
    if stops.is_empty() {
        return Ok(None);
    }
    if actions.len() != 1 || stops.len() != 1 {
        anyhow::bail!(
            "prior settlement history contains a STOP mixed with another action; refusing local \
             terminal reconciliation"
        );
    }
    let observed_beneficiary = match &stops[0].event {
        dexdo_core::TokenContractSettlementEvent::ProbeBurned { buyer, .. }
        | dexdo_core::TokenContractSettlementEvent::StreamStopped { buyer, .. } => buyer,
        _ => unreachable!("stops contains only buyer-bearing STOP events"),
    };
    let observed = dexdo_core::normalize_wallet_address(observed_beneficiary)
        .map_err(|error| anyhow::anyhow!("prior terminal receipt beneficiary: {error}"))?;
    let expected = dexdo_core::normalize_wallet_address(expected_buyer)
        .map_err(|error| anyhow::anyhow!("local buyer note: {error}"))?;
    if observed != expected {
        anyhow::bail!(
            "prior terminal receipt beneficiary {} does not match local buyer note {}; refusing \
             local reconciliation",
            display_dexdo_address(&observed),
            display_dexdo_address(&expected)
        );
    }
    Ok(Some((*stops[0]).clone()))
}

pub(crate) fn prior_stop_receipt_json(
    tc: &dexdo_core::Address,
    receipt: &dexdo_core::TokenContractSettlementReceipt,
) -> serde_json::Value {
    // This object becomes the `tx` of the versioned machine schema `dexdo.close.v1`, and it is the
    // SECOND producer of that field: `close::confirmed_seller_stop_response` fills the same `tx` by
    // serializing `SettlementActionReceipt`, whose canonical serde attributes were deliberately
    // removed so the schema keeps the legacy `0:<account_id>` spelling until a coordinated version
    // bump. One unversioned machine field must mean one thing, so this producer spells addresses the
    // same way rather than canonically. The HUMAN rendering is unaffected and stays canonical - see
    // `prior_stop_confirmation` just below.
    let mut value = serde_json::json!({
        "token_contract": tc.with_workchain(),
        "action": "terminal_stop_reconciliation",
        "message_id": receipt.message_id,
        "created_at": receipt.created_at,
    });
    let fields = match &receipt.event {
        dexdo_core::TokenContractSettlementEvent::ProbeBurned {
            buyer,
            burned_probe,
            burned_bond,
            refund_to_buyer,
        } => serde_json::json!({
            "event_kind": "probe_burned",
            "action_attribution": "buyer_stop",
            "buyer": buyer,
            "burnedProbe": burned_probe.to_string(),
            "burnedBond": burned_bond.to_string(),
            "refundToBuyer": refund_to_buyer.to_string(),
        }),
        dexdo_core::TokenContractSettlementEvent::StreamStopped {
            buyer,
            to_seller,
            refund_to_buyer,
        } => serde_json::json!({
            "event_kind": "stream_stopped",
            "closer": "unknown",
            "possible_closers": ["buyer_stop", "seller_stop", "finalize"],
            "action_attribution": "unknown_buyer_stop_or_seller_stop",
            "buyer": buyer,
            "toSeller": to_seller.to_string(),
            "refundToBuyer": refund_to_buyer.to_string(),
        }),
        _ => unreachable!("prior_stop_receipt_json receives only exact STOP receipts"),
    };
    value
        .as_object_mut()
        .expect("receipt JSON object")
        .extend(fields.as_object().expect("event JSON object").clone());
    value
}

pub(crate) fn prior_stop_confirmation(
    command: &str,
    tc: &dexdo_core::Address,
    note: &dexdo_core::Address,
    receipt: &dexdo_core::TokenContractSettlementReceipt,
) -> String {
    let attribution = match &receipt.event {
        dexdo_core::TokenContractSettlementEvent::ProbeBurned { .. } => {
            "action=buyer_stop (ProbeBurned-specific)"
        }
        dexdo_core::TokenContractSettlementEvent::StreamStopped { .. } => {
            "action=unknown (StreamStopped records the buyer beneficiary, not whether buyer stop or sellerStop or permissionless finalize submitted it)"
        }
        _ => unreachable!("prior_stop_confirmation receives only exact STOP receipts"),
    };
    format!(
        "{command} noop: TokenContract {} is already terminal by immutable receipt \
         message_id={} created_at={} event={:?}; {attribution}; buyer note {}; no second STOP was submitted",
        display_token_contract(tc),
        receipt.message_id, receipt.created_at, receipt.event,
        display_dexdo_address(note),
    )
}

fn recover_confirmation(
    tc: &dexdo_core::Address,
    note: &dexdo_core::Address,
    receipt: &dexdo_core::SettlementActionReceipt,
) -> String {
    let tc_display = display_token_contract(tc);
    // NO `destroy` FOLLOW-UP IS NAMED HERE, and its absence is the point. Both exits of
    // `TokenContract.stop()` end in a selfdestruct -- `_payOwedAndDie()` on the unaccepted-probe
    // branch, `_payFinalAndClose()` on the other -- so after this line there is no deal left to
    // close and no second step to perform. The advice that used to sit here named `dexdo destroy`,
    // which is the SELLER's cleanup of a deployed-but-unfunded contract: a different situation,
    // reached by a different path, and requiring a key the buyer does not have and on a real
    // market never will. It was printed unconditionally, on success, so it read as instruction.
    format!(
        "recover confirmed -> streamStop(TokenContract {tc_display}) from buyer note {}; \
         receipt={receipt}; the deal STOPs.",
        display_dexdo_address(note)
    )
}

fn apply_recover_terminal_marker(
    confirmation: &str,
    marker: impl FnOnce() -> Result<()>,
) -> Result<()> {
    marker().map_err(|error| {
        anyhow::anyhow!(
            "{confirmation}; local subscription marker failed after the authoritative receipt \
             was rendered: {error:#}"
        )
    })
}

/// one recorded deal, and what the chain says about acting on it in this invocation.
struct RecordedDealVerdict {
    note_addr: String,
    token_contract: String,
    /// `None` -- the chain proves this recorded deal is one the command can act on right now.
    /// `Some(reason)` -- the chain proves it is not, with the decoded reason.
    refused: Option<String>,
}

/// The chain's verdict on one recorded deal, decided from exactly the facts `check_recoverable` and
/// `check_disputable` share: the deal is OPEN, not disputed, and its recorded buyer note is this note.
/// Both preflights re-decode the full gate -- including the buyer key -- on the deal that is actually
/// selected, so this is strictly weaker than the money gate and can only make the selection refuse more
/// often, never act more often.
fn recorded_deal_verdict(
    note_addr: &str,
    token_contract: &str,
    state: Option<dexdo_core::DealChainState>,
    buyer_note: Option<&str>,
) -> RecordedDealVerdict {
    let refused = match state {
        None => Some("TokenContract is not active (undeployed/closed)".to_string()),
        Some(state) if !state.opened => {
            Some("deal is not OPEN (already closed, or never matched)".to_string())
        }
        Some(state) if state.disputed => Some("deal is already DISPUTED".to_string()),
        Some(_) => match buyer_note {
            None => Some("deal records no buyer note (not matched)".to_string()),
            Some(buyer) if buyer != note_addr => Some(format!(
                "deal records buyer note {}, not the note this entry was recorded for",
                display_dexdo_address(&buyer)
            )),
            Some(_) => None,
        },
    };
    RecordedDealVerdict {
        note_addr: note_addr.to_string(),
        token_contract: token_contract.to_string(),
        refused,
    }
}

/// which recorded deal a one-deal recovery acts on when the pool records several.

/// The choice is made from the chain's verdict on **every** recorded deal -- never from the pool's own row
/// order or recorded timestamps, which say nothing about which deal is still live. Acting is allowed only
/// where the chain proves exactly one recorded deal is in the state this command acts on **and** proves
/// every other one is not.

/// Every direction of doubt errs towards refusing. A chain read that fails for any recorded deal has
/// already aborted the invocation before this is reached, so a deal whose state could not be read is
/// never silently dropped from the set; a deal the chain cannot place is not a candidate; two candidates
/// refuse and name both. A refusal leaves every recorded deal exactly where it was. That is the safe side
/// for money because a wrong pick is irreversible: a `recover` STOP pays a seller for a deal the operator
/// never meant to end, and a `dispute` freezes the contested amount and the seller bond and starts an
/// arbitration clock on a deal that was fine. A refusal costs the operator one flag.
fn select_recorded_deal(
    command: &str,
    ambiguous: &AmbiguousRecoveryDeals,
    verdicts: Vec<RecordedDealVerdict>,
) -> Result<(String, String)> {
    let recorded = verdicts.len();
    let (mut actionable, refused): (Vec<_>, Vec<_>) = verdicts
        .into_iter()
        .partition(|verdict| verdict.refused.is_none());
    let render = |entries: &[RecordedDealVerdict]| {
        entries
            .iter()
            .map(|verdict| {
                let detail = verdict
                    .refused
                    .as_ref()
                    .map_or_else(String::new, |reason| format!(": {reason}"));
                format!(
                    "\n  --note-addr {} --token-contract {}{detail}",
                    display_dexdo_address(&verdict.note_addr),
                    display_token_contract(&verdict.token_contract)
                )
            })
            .collect::<String>()
    };
    if actionable.len() == 1 {
        let chosen = actionable.pop().expect("exactly one actionable deal");
        // The deals this invocation is NOT acting on are still the operator's money. Naming them, with
        // the chain's reason, is the only report they get: this command is about to act on a different
        // deal and would otherwise look like a plain success.
        for verdict in &refused {
            eprintln!(
                "{command}: skipped recorded deal note={} token_contract={}: {}; nothing was submitted for it",
                display_dexdo_address(&verdict.note_addr),
                display_token_contract(&verdict.token_contract),
                verdict.refused.as_deref().unwrap_or("")
            );
        }
        eprintln!(
            "{command}: DEXDO_PN_POOL {} records {recorded} recoverable deals and the chain places \
             exactly one of them in the state {command} acts on: note {} TokenContract {}.",
            ambiguous.pool.display(),
            display_dexdo_address(&chosen.note_addr),
            display_token_contract(&chosen.token_contract)
        );
        return Ok((chosen.note_addr, chosen.token_contract));
    }
    if actionable.is_empty() {
        anyhow::bail!(
            "{command}: DEXDO_PN_POOL {} records {recorded} recoverable deals and the chain places none \
             of them in the state {command} acts on; nothing was submitted:{}",
            ambiguous.pool.display(),
            render(&refused)
        );
    }
    anyhow::bail!(
        "{command}: DEXDO_PN_POOL {} records {recorded} recoverable deals and the chain places {} of \
         them in the state {command} acts on; {command} acts on one, so pass --note-addr and/or \
         --token-contract naming exactly one of:{}",
        ambiguous.pool.display(),
        actionable.len(),
        render(&actionable)
    );
}

async fn run_recover_with_chain(args: RecoverArgs, chain: &dyn RecoverChain) -> Result<()> {
    run_recover_with_chain_and_marker(args, chain, &|note_addr, token_contract| {
        // `recover` needs the marker to have run, not whether it changed a record: an ordinary deal
        // without a durable subscription sidecar is an expected no-op here.
        super::buyer::mark_buyer_subscription_terminal(note_addr, token_contract).map(drop)
    })
    .await
}

async fn run_recover_with_chain_and_marker(
    args: RecoverArgs,
    chain: &dyn RecoverChain,
    marker: &(dyn Fn(&str, &str) -> Result<()> + Sync),
) -> Result<()> {
    use dexdo_core::{check_recoverable, keypair_ed_pubkey, Address, KeyPair};
    let resolved = match resolve_persistable_pool_recovery_inputs(
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    ) {
        Ok(resolved) => resolved,
        // several recorded deals, so the chain decides which one this invocation acts on.
        Err(error) => match error.downcast::<AmbiguousRecoveryDeals>() {
            Ok(ambiguous) => {
                let mut verdicts = Vec::with_capacity(ambiguous.deals.len());
                for (note_addr, tc_str) in &ambiguous.deals {
                    let tc = Address::parse(tc_str).map_err(|e| {
                        anyhow::anyhow!("recover: recorded token_contract {tc_str}: {e}")
                    })?;
                    // A chain that cannot answer for ONE recorded deal cannot rule that deal out, so
                    // the whole invocation refuses rather than acting on a sibling.
                    let state = chain.state(&tc).await?;
                    let buyer_note = chain.buyer_note(&tc).await?;
                    verdicts.push(recorded_deal_verdict(
                        note_addr,
                        tc_str,
                        state,
                        buyer_note
                            .as_ref()
                            .map(|note| note.with_workchain())
                            .as_deref(),
                    ));
                }
                let deal = select_recorded_deal("recover", &ambiguous, verdicts)?;
                resolve_persistable_pool_recovery_inputs_for_deal(
                    &args.identity,
                    args.market.as_deref(),
                    args.token_contract.as_deref(),
                    args.pool.as_deref(),
                    &deal,
                )?
            }
            Err(other) => return Err(other),
        },
    };
    let pool_record = resolved.pool_record;
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let tc_display = display_token_contract(&tc);

    let prior_receipts = chain.settlement_receipts(&tc).await?;
    if let Some(receipt) = exact_prior_stop_receipt(&prior_receipts, &note.with_workchain())? {
        let confirmation = prior_stop_confirmation("recover", &tc, &note, &receipt);
        println!("{confirmation}");
        apply_recover_terminal_marker(&confirmation, || marker(&note.with_workchain(), &tc_str))?;
        if let Some(record) = pool_record.as_ref() {
            persist_pool_recovery_record(record).map_err(|error| {
                anyhow::anyhow!(
                    "{confirmation}; local pool persistence failed during idempotent reconciliation: \
                     {error:#}"
                )
            })?;
        }
        return Ok(());
    }

    let state = chain.state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("recover: TokenContract {tc_display} is not active (undeployed/closed)")
    })?;
    let buyer_note = chain.buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_recoverable(
        state.opened,
        state.disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "recover {tc_display}: buyer-signed STOP of an OPEN deal (streamStop -> TokenContract.stop(), standard \
         split). No new buy is placed."
    );
    let receipt = chain.stop(&note, &keys, &tc).await?;
    let confirmation = recover_confirmation(&tc, &note, &receipt);
    println!("{confirmation}");
    apply_recover_terminal_marker(&confirmation, || marker(&note_s, &tc.with_workchain()))?;
    if let Some(record) = pool_record.as_ref() {
        persist_pool_recovery_record(record).map_err(|error| {
            anyhow::anyhow!(
                "{confirmation}; local pool persistence failed after the authoritative receipt \
                 was rendered: {error:#}"
            )
        })?;
    }
    Ok(())
}

pub(crate) async fn run_recover(args: RecoverArgs) -> Result<()> {
    use dexdo_core::RealChainBackend;
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let chain = RealChainBackend::connect(manifest)?;
    run_recover_with_chain(args, &chain).await
}

/// The chain surface `dispute` uses, mirroring [`RecoverChain`] so the buyer-side dispute has the same
/// offline seam its sibling recovery already has.
#[async_trait::async_trait]
trait DisputeChain: Sync {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>>;
    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>>;
    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>>;
    async fn dispute(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<dexdo_core::SettlementActionReceipt>;
}

#[async_trait::async_trait]
impl DisputeChain for dexdo_core::RealChainBackend {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>> {
        Ok(self
            .token_contract_deal_snapshot(tc)
            .await?
            .map(|snapshot| snapshot.state))
    }

    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>> {
        Ok(self.token_contract_buyer_note(tc).await?)
    }

    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>> {
        Ok(self.token_contract_buyer_pubkey(tc).await?)
    }

    async fn dispute(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<dexdo_core::SettlementActionReceipt> {
        Ok(self.stream_dispute(note, keys, tc).await?)
    }
}

pub(crate) async fn run_dispute(args: DisputeArgs) -> Result<()> {
    use dexdo_core::RealChainBackend;
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let chain = RealChainBackend::connect(manifest)?;
    run_dispute_with_chain(args, &chain).await
}

async fn run_dispute_with_chain(args: DisputeArgs, chain: &dyn DisputeChain) -> Result<()> {
    use dexdo_core::{check_disputable, keypair_ed_pubkey, Address, KeyPair};
    let resolved = match resolve_pool_recovery_inputs(
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    ) {
        Ok(resolved) => resolved,
        // several recorded deals, so the chain decides which one this invocation acts on.
        Err(error) => match error.downcast::<AmbiguousRecoveryDeals>() {
            Ok(ambiguous) => {
                let mut verdicts = Vec::with_capacity(ambiguous.deals.len());
                for (note_addr, tc_str) in &ambiguous.deals {
                    let tc = Address::parse(tc_str).map_err(|e| {
                        anyhow::anyhow!("dispute: recorded token_contract {tc_str}: {e}")
                    })?;
                    // A chain that cannot answer for ONE recorded deal cannot rule that deal out, so
                    // the whole invocation refuses rather than acting on a sibling.
                    let state = chain.state(&tc).await?;
                    let buyer_note = chain.buyer_note(&tc).await?;
                    verdicts.push(recorded_deal_verdict(
                        note_addr,
                        tc_str,
                        state,
                        buyer_note
                            .as_ref()
                            .map(|note| note.with_workchain())
                            .as_deref(),
                    ));
                }
                let deal = select_recorded_deal("dispute", &ambiguous, verdicts)?;
                resolve_pool_recovery_inputs_for_deal(
                    &args.identity,
                    args.market.as_deref(),
                    args.token_contract.as_deref(),
                    args.pool.as_deref(),
                    &deal,
                )?
            }
            Err(other) => return Err(other),
        },
    };
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let tc_display = display_token_contract(&tc);
    let note_display = display_dexdo_address(&note);

    // Fail-loud pre-flight: only an OPEN, undisputed deal owned by THIS buyer note/key can be disputed.
    let state = chain.state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("dispute: TokenContract {tc_display} is not active (undeployed/closed)")
    })?;
    let buyer_note = chain.buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_disputable(
        state.opened,
        state.disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "dispute {tc_display}: buyer-signed streamDispute -> TokenContract.dispute() () -- freezes this TC's \
         contested amount and seller bond until resolution. Stronger than `recover` (which still pays the \
         seller for delivered ticks); both whole notes remain usable for independent deals."
    );
    let receipt = chain.dispute(&note, &keys, &tc).await?;
    println!(
        "dispute_opened -> streamDispute(TokenContract {tc_display}) from buyer note {note_display}; receipt={receipt}; \
         no terminal payment/refund split exists yet"
    );
    Ok(())
}

pub(crate) fn check_reclaimable_state(
    state: dexdo_core::DealChainState,
    buyer_note: Option<&str>,
    note_addr: &str,
    buyer_pubkey: Option<&[u8; 32]>,
    note_ed_pubkey: &[u8; 32],
    now: u64,
) -> Result<(), String> {
    dexdo_core::check_reclaimable(
        state,
        buyer_note,
        note_addr,
        buyer_pubkey,
        note_ed_pubkey,
        now,
        dexdo_core::MATCH_OPEN_TIMEOUT_SECS,
    )
}

/// `reclaim` recovers never-opened deals **from recorded pool metadata alone**, because after a
/// crash the pool file is all the operator has. The pool ordinarily records one entry per deal the notes
/// took part in, so more than one recoverable entry is the normal case, not an error: every recorded deal
/// is driven as its own separately decided, individually idempotent reclaim.
pub(crate) async fn run_reclaim(args: ReclaimArgs) -> Result<()> {
    use dexdo_core::RealChainBackend;
    let plan = resolve_pool_recovery_plan(
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    )?;
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let chain = RealChainBackend::connect(manifest)?;
    drive_reclaim_plan(plan, &chain, &|note_addr, token_contract| {
        super::buyer::mark_buyer_subscription_terminal(note_addr, token_contract)
    })
    .await
}

#[async_trait::async_trait]
trait ReclaimChain: Sync {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>>;
    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>>;
    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>>;
    async fn submit_cleanup(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<()>;
    async fn confirm_cleanup(&self, tc: &dexdo_core::Address) -> Result<()>;
}

#[async_trait::async_trait]
impl ReclaimChain for dexdo_core::RealChainBackend {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>> {
        Ok(self
            .token_contract_deal_snapshot(tc)
            .await?
            .map(|snapshot| snapshot.state))
    }

    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>> {
        Ok(self.token_contract_buyer_note(tc).await?)
    }

    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>> {
        Ok(self.token_contract_buyer_pubkey(tc).await?)
    }

    async fn submit_cleanup(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<()> {
        self.stream_cleanup(note, keys, tc).await?;
        Ok(())
    }

    async fn confirm_cleanup(&self, tc: &dexdo_core::Address) -> Result<()> {
        Ok(self.wait_cleanup_unopened(tc).await?)
    }
}

/// What one recorded entry turned into. There is no third case: either this invocation moved that deal's
/// money, or the chain itself proved there was nothing for `reclaim` to move.
enum ReclaimEntryOutcome {
    /// `cleanupUnopened` was submitted **and** confirmed by the bounded observer in this invocation.
    Reclaimed,
    /// No money was moved for this entry, with the chain-decoded reason why.
    NotActionable(String),
}

/// A deal the chain proves is gone or unfunded moves no money -- and it is also exactly the state a
/// confirmed `cleanupUnopened` leaves behind, so this is where a durable subscription marker that failed
/// after an earlier confirmed cleanup gets repaired. The marker is idempotent and reports whether it
/// actually changed anything; a marker that still fails keeps the entry loud rather than leaving local
/// state permanently stale.
fn terminal_entry(
    reason: String,
    note_s: &str,
    tc: &dexdo_core::Address,
    marker: &(dyn Fn(&str, &str) -> Result<bool> + Sync),
) -> Result<ReclaimEntryOutcome> {
    let repaired = marker(note_s, &tc.with_workchain())?;
    Ok(ReclaimEntryOutcome::NotActionable(if repaired {
        format!("{reason}; repaired the stale durable buyer subscription record for this deal")
    } else {
        reason
    }))
}

/// One recorded deal's reclaim. This is the whole money path, and it is idempotent by construction: the
/// strict never-opened preflight is re-decoded from the chain on every attempt, and a deal that was
/// already reclaimed is no longer active/funded, so it can only come back `NotActionable`.
async fn reclaim_one(
    chain: &dyn ReclaimChain,
    target: &PoolRecoveryTarget,
    now: u64,
    marker: &(dyn Fn(&str, &str) -> Result<bool> + Sync),
) -> Result<ReclaimEntryOutcome> {
    use dexdo_core::{keypair_ed_pubkey, Address, KeyPair, MATCH_OPEN_TIMEOUT_SECS};
    let note_addr = &target.note_addr;
    let tc_str = &target.token_contract;
    let keys = KeyPair::from_secret_hex(target.note_secret_hex.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note = dexdo_core::address::parse_chain_address(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc = Address::parse(tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let note_s = note.with_workchain();
    let note_display = display_dexdo_address(&note);
    let tc_display = display_token_contract(&tc);

    // This command owns only the strictly decoded never-opened cleanup. An OPEN deal is stopped
    // explicitly through `dexdo close` or `dexdo recover`, never rewritten from this legacy name.
    let Some(state) = chain.state(&tc).await? else {
        return terminal_entry(
            format!("reclaim: TokenContract {tc_display} is not active (undeployed/closed)"),
            &note_s,
            &tc,
            marker,
        );
    };
    let buyer_note = chain.buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let buyer_pubkey = chain.buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    // A deal that exists and names a *different* buyer note or key is not a decided chain no-op: the
    // pool claims this note owns the deal and the chain contradicts it. That is a corrupt or forged
    // recovery record, so it fails loudly (and submits nothing) instead of being counted as "nothing to
    // do". `check_reclaimable_state` below re-checks the same ownership as the admission gate; this
    // decides how severe a mismatch is, not whether the cleanup may be submitted.
    if let Some(buyer) = buyer_note_s.as_deref() {
        if buyer != note_s {
            let buyer_display = display_dexdo_address(&buyer);
            anyhow::bail!(
                "reclaim: the recovery record claims note {note_display} owns TokenContract {tc_display}, but the \
                 deal's buyer note is {buyer_display}; refusing to treat a contradicted recovery record as a \
                 decided no-op (nothing was submitted)"
            );
        }
    }
    if let Some(buyer_pubkey) = buyer_pubkey.as_ref() {
        if buyer_pubkey != &note_ed {
            anyhow::bail!(
                "reclaim: the owner key recorded for note {note_display} is not the buyer key of \
                 TokenContract {tc_display}; refusing to treat a contradicted recovery record as a decided \
                 no-op (nothing was submitted)"
            );
        }
    }
    if state.opened {
        return Ok(ReclaimEntryOutcome::NotActionable(format!(
            "reclaim: OPEN deal {tc_display} must use the explicit buyer STOP path (`dexdo close` or `dexdo recover`)"
        )));
    }
    if state.probe_accepted {
        return Ok(ReclaimEntryOutcome::NotActionable(
            "reclaim: deal is not OPEN and probeAccepted=true; it is not a never-opened cleanup candidate"
                .to_string(),
        ));
    }
    if let Err(reason) = check_reclaimable_state(
        state,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
        now,
    ) {
        // An unfunded deal is the other shape a confirmed cleanup can leave behind, so it repairs a
        // stale marker too; every other refusal is a plain decided no-op.
        return if !state.funded {
            terminal_entry(reason, &note_s, &tc, marker)
        } else {
            Ok(ReclaimEntryOutcome::NotActionable(reason))
        };
    }

    let funded_time = state
        .funded_time
        .expect("successful never-opened preflight requires fundedTime");
    eprintln!(
        "reclaim {tc_display}: buyer-signed streamCleanup -> TokenContract.cleanupUnopened() (never-opened refund). \
         MATCH_OPEN_TIMEOUT met: fundedTime {funded_time} + matchOpenTimeout {MATCH_OPEN_TIMEOUT_SECS} <= \
         now {now}."
    );
    if let Err(submit) = chain.submit_cleanup(&note, &keys, &tc).await {
        // A failed submit is not proof that nothing landed: the action may have been delivered and only
        // its response lost. This code drives no further cleanup for it -- the outcome is resolved with
        // the same bounded observation that confirms an ordinary cleanup, and reported by fact.

        // The guarantee is no second **money move**, not no second POST: the submit transport retries a
        // BOC on transient errors, and the operator may re-run the command. Neither can pay twice,
        // because `TokenContract.cleanupUnopened()` requires a funded, never-opened deal and destroys it
        // as it refunds, so every later delivery of the same action finds nothing to move.
        return match chain.confirm_cleanup(&tc).await {
            Ok(()) => {
                let marked = marker(&note_s, &tc.with_workchain())?;
                println!(
                    "reclaim confirmed -> streamCleanup(TokenContract {tc_display}) after an outcome-ambiguous \
                     submit ({submit}); bounded on-chain observation found the TokenContract absent or no \
                     longer funded, so the cleanup landed; this run drove no further cleanup for it, and \
                     the destroyed deal cannot be paid out twice; subscription_marked={marked}"
                );
                Ok(ReclaimEntryOutcome::Reclaimed)
            }
            Err(observation) => Err(anyhow::anyhow!(
                "reclaim: streamCleanup(TokenContract {tc_display}) submit failed and its outcome is \
                 unresolved: {submit}; the bounded observation did not find the TokenContract absent \
                 or unfunded either: {observation}; this run drove no further cleanup for it -- re-run \
                 to re-decide this deal from the chain, which cannot pay it out twice because \
                 cleanupUnopened only accepts a funded, never-opened deal"
            )),
        };
    }
    chain.confirm_cleanup(&tc).await.map_err(|error| {
        anyhow::anyhow!(
            "reclaim submitted -> streamCleanup(TokenContract {tc_display}); bounded cleanup \
                 confirmation failed: {error}; settlement is not confirmed"
        )
    })?;
    let marked = marker(&note_s, &tc.with_workchain())?;
    println!(
        "reclaim confirmed -> streamCleanup(TokenContract {tc_display}); bounded on-chain observation \
         found the TokenContract absent or no longer funded; subscription_marked={marked}"
    );
    Ok(ReclaimEntryOutcome::Reclaimed)
}

/// Drive every planned entry as its own reclaim, one at a time, reporting each by fact.

/// Exactly-once per deal never depends on this loop, and it is a guarantee about **money moves**, not
/// about network submissions: the transport retries a BOC and the operator may re-run the command, but
/// `TokenContract.cleanupUnopened()` requires a funded, never-opened deal and destroys it as it refunds,
/// so only the first delivery can pay. On top of that the strict never-opened preflight is re-decoded
/// from the chain for every entry on every attempt. A mid-sequence failure therefore loses nothing: the
/// remaining entries are still driven, the failed one is reported, and the command exits non-zero so a
/// retry re-decides every entry from the chain.

/// Authorization is a separate axis, and the two levels are not the same. `cleanupUnopened()` is
/// permissionless on chain with fixed payouts -- refund to the recorded buyer, bond back to the seller
/// note -- so no signature of ours decides where the money goes. What is owner-gated is the CLI's wrapper
/// path, `PrivateNote.streamCleanup`: this client only ever submits it from the deal's own buyer
/// note key, which is why a contradicted ownership record is refused rather than driven.
async fn drive_reclaim_plan(
    plan: PoolRecoveryPlan,
    chain: &dyn ReclaimChain,
    marker: &(dyn Fn(&str, &str) -> Result<bool> + Sync),
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs();
    if plan.targets.len() == 1 && plan.refused.is_empty() {
        // One recorded deal: "this deal cannot be reclaimed" is the answer to the operator's question,
        // so it stays exactly the loud failure it has always been.
        return match reclaim_one(chain, &plan.targets[0], now, marker).await? {
            ReclaimEntryOutcome::Reclaimed => Ok(()),
            ReclaimEntryOutcome::NotActionable(reason) => Err(anyhow::anyhow!("{reason}")),
        };
    }

    if plan.targets.is_empty() && plan.refused.is_empty() {
        // nothing recorded is the complete answer to what `reclaim` was asked, not a failure to
        // answer. Only the buyer client writes `token_contract` into a pool entry, so a pool whose notes
        // never bought carries no reclaimable record by construction -- and a sweep across several pools
        // hits that case every run. Reported as an error it teaches an operator to read past the word
        // `Error` in the one log that says whether the cleanup ran, which is the opposite of loud. The
        // summary below still prints, so `planned=0` is a measured count and not an absence of output.
        println!(
            "reclaim: this pool records no entries to reclaim; no money was moved and nothing was submitted"
        );
    }

    for refusal in &plan.refused {
        println!(
            "reclaim refused note={} token_contract={}: {}; no money was moved for it",
            display_dexdo_address(&refusal.note_addr),
            display_token_contract(&refusal.token_contract),
            refusal.reason
        );
    }
    let planned = plan.targets.len();
    let mut reclaimed = 0usize;
    let mut noop = 0usize;
    let mut failed = Vec::new();
    for (index, target) in plan.targets.iter().enumerate() {
        let position = index + 1;
        let note_addr = &target.note_addr;
        let token_contract = &target.token_contract;
        let note_display = display_dexdo_address(note_addr);
        let token_contract_display = display_token_contract(token_contract);
        let recorded_at = target
            .recorded_at_unix
            .map_or_else(|| "unrecorded".to_string(), |at| at.to_string());
        eprintln!(
            "reclaim entry {position}/{planned}: note {note_display} TokenContract {token_contract_display} \
             recorded_at_unix={recorded_at}; each recorded deal is decided on its own chain state."
        );
        match reclaim_one(chain, target, now, marker).await {
            Ok(ReclaimEntryOutcome::Reclaimed) => {
                reclaimed += 1;
                println!(
                    "reclaim entry {position}/{planned} reclaimed note={note_display} \
                     token_contract={token_contract_display}"
                );
            }
            Ok(ReclaimEntryOutcome::NotActionable(reason)) => {
                noop += 1;
                println!(
                    "reclaim entry {position}/{planned} noop note={note_display} \
                     token_contract={token_contract_display}: {reason}; no money was moved for it"
                );
            }
            Err(error) => {
                println!(
                    "reclaim entry {position}/{planned} failed note={note_display} \
                     token_contract={token_contract_display}: {error:#}"
                );
                failed.push(format!(
                    "note={note_display} token_contract={token_contract_display}: {error:#}"
                ));
            }
        }
    }
    println!(
        "reclaim summary: planned={planned} reclaimed={reclaimed} noop={noop} failed={} refused={}",
        failed.len(),
        plan.refused.len()
    );
    if !failed.is_empty() || !plan.refused.is_empty() {
        // The error itself carries every reason: a caller that only sees the failure must still learn
        // which recorded deals were left undecided and why.
        anyhow::bail!(
            "reclaim: {} of {} recorded entries were not decided ({} refused as contradictory); \
             re-run after fixing them -- every entry is re-decided from the chain, so nothing is \
             reclaimed twice. Refused: [{}]. Failures: [{}]",
            failed.len() + plan.refused.len(),
            planned + plan.refused.len(),
            plan.refused.len(),
            plan.refused
                .iter()
                .map(|refusal| format!(
                    "note={} token_contract={}: {}",
                    display_dexdo_address(&refusal.note_addr),
                    display_token_contract(&refusal.token_contract),
                    refusal.reason
                ))
                .collect::<Vec<_>>()
                .join("; "),
            failed.join("; ")
        );
    }
    Ok(())
}

pub(crate) async fn run_release_dispute(args: ReleaseDisputeArgs) -> Result<()> {
    use dexdo_core::{
        check_release_disputable, check_seller_pubkey, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        crate::cli::support::require_note_addr(&args.identity, "release-dispute", "seller note")?;

    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    // The address this command already settled -- resolving it a second time would offer the pool
    // again, and a key from one note with an address from another is a signature that cannot verify.
    let seed = crate::cli::support::note_owner_secret_for(
        args.identity.note_key.as_deref(),
        &note_addr,
        None,
        "release-dispute",
        "the seller note's owner key",
    )?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let tc_display = display_token_contract(&tc);
    let note_display = display_dexdo_address(&note);

    let state = chain
        .token_contract_deal_snapshot(&tc)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release-dispute: TokenContract {tc_display} is not active (undeployed/closed)"
            )
        })?
        .state;
    check_release_disputable(state.disputed).map_err(anyhow::Error::msg)?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("release-dispute", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "release-dispute {tc_display}: seller-signed TokenContract.releaseDispute() from note {note_display}; \
         exact burns, returns and seller payout will be reported only by DisputeResolved and strict getters."
    );
    let receipt = chain.release_dispute(&tc, &keys).await?;
    println!("release-dispute confirmed -> TokenContract {tc_display}; receipt={receipt}");
    Ok(())
}

fn required_u64(value: &Value, field: &str, context: &str) -> Result<u64> {
    value[field]
        .as_u64()
        .or_else(|| value[field].as_str().and_then(|raw| raw.parse().ok()))
        .ok_or_else(|| anyhow::anyhow!("{context}: getter exposes no {field}"))
}

/// the deal is NOT disputed, so there is nothing for this command to resolve.

/// `runtime-machine-contract.md:1067` defines `DISPUTED_DEAL` as "Deal is disputed and cannot be
/// closed by the requested command" -- the exact opposite of this condition. The classifier reached
/// it because it lower-cases the message before matching, so "deal is not DISPUTED" contains
/// "disputed": the word matched inside its own negation and the consumer was told the reverse of the
/// fact. Typed, so the sentence keeps saying what it says.

/// It carries `INVALID_ARGUMENT` (`:1049`): the command was asked to resolve a dispute on a deal
/// that has none, which is a refusal about the request rather than about the chain. That is the one
/// judgement call in this change, and it is called out in the pull request.
#[derive(Debug)]
pub(crate) struct DealIsNotDisputed {
    message: String,
}

impl DealIsNotDisputed {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DealIsNotDisputed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DealIsNotDisputed {}

fn validate_dispute_timeout(
    state: dexdo_core::DealChainState,
    config: &Value,
    now: u64,
) -> Result<u64> {
    if !state.disputed {
        return Err(anyhow::Error::new(DealIsNotDisputed::new(
            "resolve-dispute-timeout: deal is not DISPUTED -- nothing to resolve",
        )));
    }
    let dispute_time = state.dispute_time;
    if dispute_time == 0 {
        anyhow::bail!("resolve-dispute-timeout: disputed deal has zero disputeTime");
    }
    let dispute_window =
        required_u64(config, "disputeWindow", "resolve-dispute-timeout getConfig")?;
    let deadline = dispute_time.saturating_add(dispute_window);
    if now < deadline {
        anyhow::bail!(
            "resolve-dispute-timeout: too early -- disputeTime {dispute_time} + disputeWindow \
             {dispute_window} = {deadline} > now {now} ({} s remaining)",
            deadline - now
        );
    }
    Ok(deadline)
}

async fn submit_dispute_timeout_after_validation<T>(
    preflight: Result<u64>,
    submit: impl std::future::Future<Output = Result<T>>,
) -> Result<(u64, T)> {
    let deadline = preflight?;
    let receipt = submit.await?;
    Ok((deadline, receipt))
}

pub(crate) async fn run_resolve_dispute_timeout(args: ResolveDisputeTimeoutArgs) -> Result<()> {
    use dexdo_core::{Address, RealChainBackend};

    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let contracts_path = crate::cli::commands::manifest_path()?;
    let contracts = contracts_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            contracts_path.display()
        )
    })?;
    let chain = RealChainBackend::connect(contracts)?;
    let now = chain.observed_chain_timestamp().await?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let tc_display = display_token_contract(&tc);
    let expected_hash = dexdo_core::chain::compiled_contract_hash("TokenContract")?;
    let (active, code_hash) = chain.account_active_code_hash(&tc).await?;
    let mut identity_ok =
        active && code_hash.as_deref() == Some(expected_hash.trim_start_matches("0x"));
    if let Some(path) = args.market.as_deref() {
        let market = load_market(path)?;
        let seller = chain
            .token_contract_seller_pubkey(&tc)
            .await?
            .ok_or_else(|| anyhow::anyhow!("TokenContract {tc_display} getSeller unavailable"))?;
        let seller = serde_json::json!(format!("0x{seller:0>64}"));
        let root = dexdo_core::address::parse_chain_address(&market.root_model)?;
        identity_ok &= chain.token_contract_model_hash(&tc).await?.as_deref()
            == Some(market.model_hash.as_str())
            && chain
                .root_model_address_for(&seller)
                .await?
                .with_workchain()
                == root.with_workchain()
            && chain
                .resolve_token_contract(&root, &seller, market.nonce)
                .await?
                .with_workchain()
                == tc.with_workchain();
    }
    let before = chain
        .token_contract_deal_snapshot(&tc)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "resolve-dispute-timeout: TokenContract {tc_display} is not active (undeployed/closed)"
            )
        })?
        .state;
    let config = chain.token_contract_config(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("resolve-dispute-timeout: TokenContract {tc_display} getConfig unavailable")
    })?;
    let preflight = if identity_ok {
        validate_dispute_timeout(before, &config, now)
    } else {
        Err(anyhow::anyhow!(
            "resolve-dispute-timeout: wrong TokenContract identity"
        ))
    };
    let (deadline, receipt) =
        submit_dispute_timeout_after_validation(preflight, chain.resolve_dispute_timeout(&tc))
            .await?;
    println!(
        "resolve-dispute-timeout confirmed token_contract={tc_display} deadline={deadline} receipt={receipt}"
    );
    Ok(())
}

const WITHDRAW_SHELL_GUIDANCE: &str =
    "This withdraws finalized seller proceeds. If this drains the last finalized proceeds from a funded, closed, undisputed deal with no live offer, the TC also selfdestructs; otherwise it remains active.";

/// `withdrawShell(uint128 amount)` pays the `_sellerNote` the deal stored at construction.
const WITHDRAW_SHELL_PAYEE: &str =
    "The deal pays the seller note it stored at construction; withdrawShell accepts only amount.";

pub(crate) async fn run_withdraw_shell(args: WithdrawShellArgs) -> Result<()> {
    use dexdo_core::{
        check_seller_pubkey, check_withdrawable_shell, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        crate::cli::support::require_note_addr(&args.identity, "withdraw-shell", "seller note")?;

    Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = crate::cli::support::note_owner_secret_for(
        args.identity.note_key.as_deref(),
        &note_addr,
        None,
        "withdraw-shell",
        "the seller note's owner key",
    )?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let tc_display = display_token_contract(&tc);

    let state = chain
        .token_contract_deal_snapshot(&tc)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "withdraw-shell: TokenContract {tc_display} is not active (undeployed/closed)"
            )
        })?
        .state;
    let amount =
        check_withdrawable_shell(state.finalized_owed, args.amount).map_err(anyhow::Error::msg)?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("withdraw-shell", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "withdraw-shell {tc_display}: seller-signed TokenContract.withdrawShell(amount={} SHELL). \
         {WITHDRAW_SHELL_PAYEE} {WITHDRAW_SHELL_GUIDANCE}",
        dexdo_core::shell_amount(amount)
    );
    chain.withdraw_shell(&tc, amount, &keys).await?;
    println!(
        "withdraw-shell submitted -> {} finalized SHELL from TokenContract {tc_display} to the seller note it \
         stored at construction",
        dexdo_core::shell_amount(amount)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::args::{RecoverArgs, RecoveryIdentityArgs};

    // `include!` rather than a `mod` declaration because the test drives the private
    // `recover_confirmation` and reuses this module's own `test_stop_receipt` fixture.
    include!("recover/destroy_advice_1523_test.rs");

    #[test]
    fn recover_uses_shared_explicit_stop_while_legacy_reclaim_rejects_open() {
        let source = include_str!("recover.rs");
        let end = source
            .find("#[cfg(test)]")
            .expect("production/test boundary");
        let production = &source[..end];
        assert_eq!(
            production.matches(".explicit_buyer_stop(").count(),
            1,
            "recover must use the shared explicit STOP path"
        );
        assert!(!production.contains(".stream_stop("));
        // renamed the interpolated binding to the canonically rendered `{tc_display}`; the
        // sentence itself, and the path it names, are unchanged.
        assert!(production.contains(
            "OPEN deal {tc_display} must use the explicit buyer STOP path (`dexdo close` or `dexdo recover`)"
        ));
    }

    fn chain_state(
        funded: bool,
        opened: bool,
        probe_accepted: bool,
        disputed: bool,
        deposit: u128,
    ) -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded,
            opened,
            probe_accepted,
            disputed,
            deposit,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 0,
            last_claim_time: 1,
            dispute_time: if disputed { 100 } else { 0 },
        }
    }

    /// A stale funded read after submission is not success: reclaim uses the existing bounded
    /// observer and marks local state only after that observer confirms absent/unfunded state.
    #[test]
    fn reclaim_requires_bounded_cleanup_confirmation_before_success() {
        let source = include_str!("recover.rs");
        let end = source
            .find("#[cfg(test)]")
            .expect("production/test boundary");
        let production = &source[..end];
        let submit = production
            .find("chain.submit_cleanup(&note, &keys, &tc).await")
            .expect("cleanup submit");
        let tail = &production[submit..];
        let observe = tail
            .find("chain.confirm_cleanup(&tc)")
            .expect("bounded cleanup observer");
        let marker = tail.find("marker(&note_s,").expect("terminal marker");
        let success = tail
            .find("reclaim confirmed -> streamCleanup")
            .expect("success rendering");
        assert!(observe < marker && marker < success);
        assert!(
            !tail.contains("single post-read"),
            "one pending/stale read must never be reported as a successful reclaim"
        );
        // Scope note: this pins one submit **call site** -- that the reconciliation path does not drive
        // a second cleanup itself. It is NOT a claim that the action is POSTed at most once: the submit
        // transport retries a BOC, and a re-run submits again. The money guarantee is proved by fact in
        // `a_duplicated_cleanup_post_moves_the_deals_money_only_once`.
        assert_eq!(
            production.matches("chain.submit_cleanup(").count(),
            1,
            "the ambiguous-outcome path must reconcile, not drive a second cleanup"
        );
        // The injected marker seam exists for the offline regressions only: the production command
        // must still mark the durable buyer subscription terminal, and the real chain must still
        // submit `streamCleanup` and wait on the bounded `cleanupUnopened` observer.
        assert!(production.contains(
            "drive_reclaim_plan(plan, &chain, &|note_addr, token_contract| {\n        super::buyer::mark_buyer_subscription_terminal(note_addr, token_contract)\n    })"
        ));
        assert!(production.contains("self.stream_cleanup(note, keys, tc).await?"));
        assert!(production.contains("self.wait_cleanup_unopened(tc).await?"));
    }

    #[test]
    fn real_never_opened_state_selects_one_cleanup_but_terminal_prior_open_selects_no_write() {
        let me = [7u8; 32];
        let mut state = chain_state(true, false, false, false, 100);
        state.funded_time = Some(1);
        state.last_claim_time = 1;
        let cleanup = super::check_reclaimable_state(
            state,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            1_000,
        );
        assert_eq!(usize::from(cleanup.is_ok()), 1, "cleanup POST count");
        cleanup.expect("the exact never-opened shape past MATCH_OPEN_TIMEOUT is admissible");

        state.deposit = 0;
        state.last_claim_time = 2;
        let preflight = super::check_reclaimable_state(
            state,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            1_000,
        );
        let money_writes = usize::from(preflight.is_ok());
        assert_eq!(money_writes, 0);
        assert!(preflight.unwrap_err().contains("terminal/drained"));
    }

    struct TempDirCleanup(std::path::PathBuf);

    impl Drop for TempDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct PoolRecoverChain {
        buyer_note: dexdo_core::Address,
        buyer_pubkey: [u8; 32],
        stop_calls: std::sync::atomic::AtomicUsize,
        terminal: std::sync::atomic::AtomicBool,
        poison_pool_after_stop: Option<std::path::PathBuf>,
    }

    fn test_stop_receipt(tc: &dexdo_core::Address) -> dexdo_core::SettlementActionReceipt {
        dexdo_core::SettlementActionReceipt {
            token_contract: tc.with_workchain(),
            action: dexdo_core::SettlementAction::BuyerStop,
            message_id: "test-stop-message".to_string(),
            created_at: 1,
            event: dexdo_core::SettlementActionEvent::StreamStopped {
                buyer: format!("0:{}", "1".repeat(64)),
                to_seller: 1u128.into(),
                refund_to_buyer: 2u128.into(),
            },
            pre_bonds: dexdo_core::SettlementActionBondState {
                seller_bond_held: 2u128.into(),
                seller_bond_required: 2u128.into(),
                buyer_bond_held: 0u128.into(),
                buyer_bond_required: 0u128.into(),
            },
            post_state: Some(dexdo_core::SettlementActionPostState {
                tokens_final: 3u128.into(),
                tokens_pending: 5u128.into(),
                seller_bond_held: 0u128.into(),
                seller_bond_required: 2u128.into(),
                buyer_bond_held: 0u128.into(),
                buyer_bond_required: 0u128.into(),
                opened: false,
                disputed: false,
            }),
        }
    }

    #[test]
    fn recover_confirmation_renders_the_authoritative_stop_receipt() {
        let tc = dexdo_core::Address::parse(&format!("0:{}", "2".repeat(64))).unwrap();
        let note = dexdo_core::Address::parse(&format!("0:{}", "1".repeat(64))).unwrap();
        let rendered = super::recover_confirmation(&tc, &note, &test_stop_receipt(&tc));
        for fact in [
            "action=buyer_stop",
            "message_id=test-stop-message",
            "created_at=1",
            // the human confirmation names the buyer's PrivateNote canonically; a note is a
            // contract of the shared dexdo DApp.
            "buyer=0000000000000000000000000000000000000000000000000000000000000004::1111111111111111111111111111111111111111111111111111111111111111",
            "toSeller=1",
            "refundToBuyer=2",
            "tokensFinal=3",
            "tokensPending=5",
        ] {
            assert!(rendered.contains(fact), "missing {fact:?} in {rendered}");
        }
    }

    #[test]
    fn foreign_stop_receipt_never_reconciles_local_state_or_submits_money() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let expected = format!("0:{}", "11".repeat(32));
        let foreign = format!("0:{}", "22".repeat(32));
        for event in [
            dexdo_core::TokenContractSettlementEvent::ProbeBurned {
                buyer: foreign.clone(),
                burned_probe: 1,
                burned_bond: 2,
                refund_to_buyer: 3,
            },
            dexdo_core::TokenContractSettlementEvent::StreamStopped {
                buyer: foreign.clone(),
                to_seller: 4,
                refund_to_buyer: 5,
            },
        ] {
            let receipts = dexdo_core::TokenContractSettlementReceipts {
                events: vec![dexdo_core::TokenContractSettlementReceipt {
                    message_id: "foreign-stop".to_string(),
                    created_at: 7,
                    cursor: "foreign-stop-cursor".to_string(),
                    event,
                }],
            };
            let marker_calls = AtomicUsize::new(0);
            let stop_calls = AtomicUsize::new(0);
            let result = (|| -> anyhow::Result<()> {
                super::exact_prior_stop_receipt(&receipts, &expected)?;
                marker_calls.fetch_add(1, Ordering::SeqCst);
                stop_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })();
            let message = result
                .expect_err("foreign buyer receipt must fail before local reconciliation")
                .to_string();
            assert!(
                message.contains("does not match local buyer note"),
                "{message}"
            );
            assert_eq!(marker_calls.load(Ordering::SeqCst), 0);
            assert_eq!(stop_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn issue_1948_recycled_prior_stop_with_same_buyer_is_not_current_noop() {
        let buyer = format!("0:{}", "11".repeat(32));
        let tc = format!("0:{}", "22".repeat(32));
        let receipt =
            |message_id: &str, created_at: u64, event: dexdo_core::TokenContractSettlementEvent| {
                dexdo_core::TokenContractSettlementReceipt {
                    message_id: message_id.to_string(),
                    created_at,
                    cursor: format!("cursor-{message_id}"),
                    event,
                }
            };
        let mut receipts = dexdo_core::TokenContractSettlementReceipts {
            events: vec![
                receipt(
                    "old-deploy",
                    1,
                    dexdo_core::TokenContractSettlementEvent::ContractDeployed {
                        token_contract: tc.clone(),
                    },
                ),
                receipt(
                    "old-stop",
                    2,
                    dexdo_core::TokenContractSettlementEvent::StreamStopped {
                        buyer: buyer.clone(),
                        to_seller: 4,
                        refund_to_buyer: 5,
                    },
                ),
                receipt(
                    "old-destroy",
                    3,
                    dexdo_core::TokenContractSettlementEvent::ContractDestroyed {
                        token_contract: tc.clone(),
                    },
                ),
                receipt(
                    "current-deploy",
                    4,
                    dexdo_core::TokenContractSettlementEvent::ContractDeployed {
                        token_contract: tc,
                    },
                ),
                receipt(
                    "current-open",
                    5,
                    dexdo_core::TokenContractSettlementEvent::StreamOpened {
                        buyer: buyer.clone(),
                        price_per_tick: 1,
                    },
                ),
            ],
        };

        assert!(super::exact_prior_stop_receipt(&receipts, &buyer)
            .expect("the prior lifecycle must not be reconciled as current")
            .is_none());

        receipts.events.push(receipt(
            "current-stop",
            6,
            dexdo_core::TokenContractSettlementEvent::StreamStopped {
                buyer: buyer.clone(),
                to_seller: 6,
                refund_to_buyer: 7,
            },
        ));
        assert_eq!(
            super::exact_prior_stop_receipt(&receipts, &buyer)
                .expect("the current lifecycle terminal is valid")
                .expect("the current terminal must reconcile")
                .message_id,
            "current-stop"
        );
    }

    #[test]
    fn seller_stop_shape_with_local_beneficiary_is_only_actor_unknown_terminal() {
        let beneficiary = format!("0:{}", "11".repeat(32));
        let receipts = dexdo_core::TokenContractSettlementReceipts {
            events: vec![dexdo_core::TokenContractSettlementReceipt {
                message_id: "seller-stop-shaped".to_string(),
                created_at: 9,
                cursor: "seller-stop-shaped-cursor".to_string(),
                event: dexdo_core::TokenContractSettlementEvent::StreamStopped {
                    buyer: beneficiary.clone(),
                    to_seller: 4,
                    refund_to_buyer: 5,
                },
            }],
        };
        let receipt = super::exact_prior_stop_receipt(&receipts, &beneficiary)
            .unwrap()
            .expect("StreamStopped proves terminality for the local beneficiary");
        let tc = dexdo_core::Address::parse(&format!("0:{}", "22".repeat(32))).unwrap();
        let note = dexdo_core::Address::parse(&beneficiary).unwrap();
        let json = super::prior_stop_receipt_json(&tc, &receipt);
        assert_eq!(json["action"], "terminal_stop_reconciliation");
        assert_eq!(
            json["action_attribution"],
            "unknown_buyer_stop_or_seller_stop"
        );
        assert_ne!(json["action_attribution"], "buyer_stop");

        let confirmation = super::prior_stop_confirmation("recover", &tc, &note, &receipt);
        for fact in [
            "action=unknown",
            "buyer beneficiary",
            "buyer stop or sellerStop",
            "no second STOP was submitted",
        ] {
            assert!(
                confirmation.contains(fact),
                "missing {fact:?} in {confirmation}"
            );
        }
        assert!(
            !confirmation.contains("action=buyer_stop"),
            "sellerStop-shaped receipt must not be attributed to buyer STOP: {confirmation}"
        );
    }

    #[test]
    fn recover_marks_subscription_or_noops_for_ordinary_deal_after_rendering_receipt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tc = dexdo_core::Address::parse(&format!("0:{}", "2".repeat(64))).unwrap();
        let note = dexdo_core::Address::parse(&format!("0:{}", "1".repeat(64))).unwrap();
        let receipt = test_stop_receipt(&tc);
        let confirmation = super::recover_confirmation(&tc, &note, &receipt);
        let calls = AtomicUsize::new(0);

        // `recover` consumes only "the marker ran without failing": a matched subscription and an
        // ordinary deal without a sidecar are both a plain success here.
        super::apply_recover_terminal_marker(&confirmation, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect("a marked subscription is a success");
        super::apply_recover_terminal_marker(&confirmation, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect("an ordinary deal without a subscription sidecar is a success too");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let error = super::apply_recover_terminal_marker(&confirmation, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("marker disk failure"))
        })
        .expect_err("marker failure must remain explicit after the landed receipt")
        .to_string();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        for fact in [
            "local subscription marker failed",
            "action=buyer_stop",
            "message_id=test-stop-message",
            "created_at=1",
            "toSeller=1",
            "refundToBuyer=2",
            "marker disk failure",
        ] {
            assert!(error.contains(fact), "missing {fact:?} in {error}");
        }

        let source = include_str!("recover.rs");
        let stop = source
            .find("let receipt = chain.stop(")
            .expect("recover STOP");
        let tail = &source[stop..];
        let render = tail
            .find("println!(\"{confirmation}\")")
            .expect("receipt render");
        let marker = tail
            .find("apply_recover_terminal_marker(")
            .expect("receipt-driven terminal marker");
        let pool = tail
            .find("persist_pool_recovery_record(")
            .expect("pool recovery persistence");
        assert!(render < marker && marker < pool);
    }

    #[async_trait::async_trait]
    impl super::RecoverChain for PoolRecoverChain {
        async fn state(
            &self,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::DealChainState>> {
            Ok((!self.terminal.load(std::sync::atomic::Ordering::SeqCst))
                .then(|| chain_state(true, true, true, false, 100)))
        }

        async fn buyer_note(
            &self,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::Address>> {
            Ok(Some(self.buyer_note.clone()))
        }

        async fn buyer_pubkey(
            &self,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<[u8; 32]>> {
            Ok(Some(self.buyer_pubkey))
        }

        async fn stop(
            &self,
            note: &dexdo_core::Address,
            _keys: &dexdo_core::KeyPair,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<dexdo_core::SettlementActionReceipt> {
            assert_eq!(note.with_workchain(), self.buyer_note.with_workchain());
            self.stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.terminal
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(path) = &self.poison_pool_after_stop {
                std::fs::write(path, b"{")?;
            }
            Ok(test_stop_receipt(tc))
        }

        async fn settlement_receipts(
            &self,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<dexdo_core::TokenContractSettlementReceipts> {
            if !self.terminal.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(dexdo_core::TokenContractSettlementReceipts::default());
            }
            Ok(dexdo_core::TokenContractSettlementReceipts {
                events: vec![dexdo_core::TokenContractSettlementReceipt {
                    message_id: "test-stop-message".to_string(),
                    created_at: 1,
                    cursor: "test-stop-cursor".to_string(),
                    event: dexdo_core::TokenContractSettlementEvent::StreamStopped {
                        buyer: self.buyer_note.with_workchain(),
                        to_seller: 1,
                        refund_to_buyer: 2,
                    },
                }],
            })
        }
    }

    #[tokio::test]
    async fn dispute_timeout_validation_matches_deployed_boundary() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let disputed = chain_state(true, false, true, true, 100);
        let mut terminal = chain_state(true, false, true, false, 0);
        terminal.dispute_time = 100;
        let config = serde_json::json!({"disputeWindow": "600"});

        assert_eq!(
            super::validate_dispute_timeout(disputed, &config, 700).unwrap(),
            700
        );
        assert!(super::validate_dispute_timeout(disputed, &config, 699)
            .unwrap_err()
            .to_string()
            .contains("too early"));
        assert!(super::validate_dispute_timeout(terminal, &config, 700)
            .unwrap_err()
            .to_string()
            .contains("not DISPUTED"));

        let posts = AtomicUsize::new(0);
        let post = || async {
            posts.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({}))
        };
        for preflight in [
            Err(anyhow::anyhow!("wrong TokenContract identity")),
            super::validate_dispute_timeout(disputed, &config, 699),
        ] {
            assert!(
                super::submit_dispute_timeout_after_validation(preflight, post())
                    .await
                    .is_err()
            );
        }
        assert_eq!(posts.load(Ordering::SeqCst), 0);

        let (deadline, receipt) = super::submit_dispute_timeout_after_validation(
            super::validate_dispute_timeout(disputed, &config, 700),
            post(),
        )
        .await
        .unwrap();
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert_eq!(deadline, 700);
        assert_eq!(receipt, serde_json::json!({}));
    }

    /// primary regression: the production recover flow must atomically write the selected pool-only buyer
    /// record after STOP, so a fresh pool load observes it as a durable buyer recovery record.
    #[tokio::test]
    async fn run_recover_persists_pool_only_record_across_reload() {
        use std::sync::atomic::Ordering;

        let dir = std::env::temp_dir().join(format!(
            "dexdo-run-recover-persist-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        // This test gets a buyer note nobody else in the binary uses, and so do its two
        // neighbours below. The temp dir above is private to this test; the BUYER SUBMIT JOURNAL
        // is not -- `buyer_submit_state_dir` builds one directory per test PROCESS behind a
        // `OnceLock`, and `BuyerMoneyLock` names its files `sha256(note address)`. Two tests
        // sharing a note address therefore share one lock file, and the one that loses the race
        // fails with "already has another money submission awaiting by-fact reconciliation...
        // pool lock is already held" -- a sentence about neither test.

        // Measured in CI 6734: this test and `pool_write_failure_after_stop_...` failed together,
        // both on `note-b2b1ccde....money.lock`, both having used `0:1111...`. Three local runs of
        // the whole binary did not reproduce it, which is what a scheduling race looks like from
        // a machine with fewer cores -- and why the fix is a distinct address rather than a retry.
        let note_addr = format!("0:{}", "1a".repeat(32));
        let token_contract = format!("0:{}", "2".repeat(64));
        let seller_tc = format!("0:{}", "3".repeat(64));
        let secret = "2a".repeat(32);
        crate::cli::support::write_owner_only_key_fixture(
            &pool_path,
            &serde_json::to_string_pretty(&serde_json::json!({
                "token_type": dexdo_core::params::SHELL_CURRENCY_ID,
                "notes": [
                    {
                        "address": note_addr,
                        "owner_secret_key_hex": secret,
                        "token_contract": seller_tc,
                        "token_contract_role": "seller",
                        "token_contract_updated_at_unix": 7
                    },
                    {
                        "address": note_addr,
                        "owner_secret_key_hex": secret,
                        "token_contract": token_contract
                    }
                ]
            }))
            .unwrap(),
        );

        let keys = dexdo_core::KeyPair::from_secret_hex(&secret).unwrap();
        let chain = PoolRecoverChain {
            buyer_note: dexdo_core::Address::parse(&note_addr).unwrap(),
            buyer_pubkey: dexdo_core::keypair_ed_pubkey(&keys).unwrap(),
            stop_calls: std::sync::atomic::AtomicUsize::new(0),
            terminal: std::sync::atomic::AtomicBool::new(false),
            poison_pool_after_stop: None,
        };
        super::run_recover_with_chain(
            RecoverArgs {
                identity: RecoveryIdentityArgs {
                    note_key: None,
                    note_addr: None,
                },
                token_contract: None,
                market: None,
                pool: Some(pool_path.clone()),
            },
            &chain,
        )
        .await
        .unwrap();

        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
        let reloaded = crate::cli::commands::load_pool_json(&pool_path).unwrap();
        let notes = reloaded["notes"].as_array().unwrap();
        // The seller row was not written by this run, so it keeps the spelling the fixture gave it.
        let seller = notes
            .iter()
            .find(|note| note["token_contract"] == seller_tc)
            .expect("different seller record must remain present");
        assert_eq!(seller["token_contract_role"], "seller");
        assert_eq!(seller["token_contract_updated_at_unix"], 7);
        // The recovered row WAS written, so since it carries the TokenContract in its
        // canonical self-DApp form -- a per-deal TC is a self-DApp account, and one pool entry must
        // not hold two address conventions. Stated as a literal over the same `2` seed the fixture
        // uses, not by calling the renderer.
        let recovered_tc = format!("{}::{}", "2".repeat(64), "2".repeat(64));
        assert_ne!(
            recovered_tc, token_contract,
            "the fixture has to be a form the write actually changes, or this proves nothing"
        );
        let recovered = notes
            .iter()
            .find(|note| note["token_contract"] == recovered_tc)
            .expect("recovered buyer record must survive pool reload");
        assert_eq!(recovered["owner_secret_key_hex"], secret);
        assert_eq!(recovered["token_contract_role"], "buyer");
        assert!(recovered["token_contract_updated_at_unix"]
            .as_u64()
            .is_some());
    }

    #[tokio::test]
    async fn recover_retry_reconciles_local_state_without_a_second_stop() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = std::env::temp_dir().join(format!(
            "dexdo-run-recover-retry-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        // Its own buyer note: the buyer submit journal is one directory per test process,
        // and the money lock is named after this address. See the neighbour above.
        let note_addr = format!("0:{}", "1b".repeat(32));
        let token_contract = format!("0:{}", "2".repeat(64));
        let secret = "2a".repeat(32);
        crate::cli::support::write_owner_only_key_fixture(
            &pool_path,
            &serde_json::to_string_pretty(&serde_json::json!({
                "token_type": dexdo_core::params::SHELL_CURRENCY_ID,
                "notes": [{
                    "address": note_addr,
                    "owner_secret_key_hex": secret,
                    "token_contract": token_contract
                }]
            }))
            .unwrap(),
        );
        let keys = dexdo_core::KeyPair::from_secret_hex(&secret).unwrap();
        let chain = PoolRecoverChain {
            buyer_note: dexdo_core::Address::parse(&note_addr).unwrap(),
            buyer_pubkey: dexdo_core::keypair_ed_pubkey(&keys).unwrap(),
            stop_calls: AtomicUsize::new(0),
            terminal: std::sync::atomic::AtomicBool::new(false),
            poison_pool_after_stop: None,
        };
        let marker_calls = AtomicUsize::new(0);
        let marker = |_: &str, _: &str| {
            let call = marker_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                anyhow::bail!("transient marker failure")
            }
            Ok(())
        };
        let args = || RecoverArgs {
            identity: RecoveryIdentityArgs {
                note_key: None,
                note_addr: None,
            },
            token_contract: None,
            market: None,
            pool: Some(pool_path.clone()),
        };

        let first = super::run_recover_with_chain_and_marker(args(), &chain, &marker)
            .await
            .expect_err("first local marker attempt fails after the landed STOP")
            .to_string();
        assert!(first.contains("test-stop-message"), "{first}");
        assert!(first.contains("transient marker failure"), "{first}");
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);

        super::run_recover_with_chain_and_marker(args(), &chain, &marker)
            .await
            .expect("retry reconciles immutable receipt without a second STOP");
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(marker_calls.load(Ordering::SeqCst), 2);
        let reloaded = crate::cli::commands::load_pool_json(&pool_path).unwrap();
        assert_eq!(reloaded["notes"][0]["token_contract_role"], "buyer");
    }

    #[tokio::test]
    async fn pool_write_failure_after_stop_preserves_the_authoritative_receipt() {
        use std::sync::atomic::Ordering;

        let dir = std::env::temp_dir().join(format!(
            "dexdo-run-recover-write-failure-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        // Its own buyer note: the buyer submit journal is one directory per test process,
        // and the money lock is named after this address. See the neighbour above.
        let note_addr = format!("0:{}", "1c".repeat(32));
        let token_contract = format!("0:{}", "2".repeat(64));
        let secret = "2a".repeat(32);
        crate::cli::support::write_owner_only_key_fixture(
            &pool_path,
            &serde_json::to_string_pretty(&serde_json::json!({
                "token_type": dexdo_core::params::SHELL_CURRENCY_ID,
                "notes": [{
                    "address": note_addr,
                    "owner_secret_key_hex": secret,
                    "token_contract": token_contract
                }]
            }))
            .unwrap(),
        );

        let keys = dexdo_core::KeyPair::from_secret_hex(&secret).unwrap();
        let chain = PoolRecoverChain {
            buyer_note: dexdo_core::Address::parse(&note_addr).unwrap(),
            buyer_pubkey: dexdo_core::keypair_ed_pubkey(&keys).unwrap(),
            stop_calls: std::sync::atomic::AtomicUsize::new(0),
            terminal: std::sync::atomic::AtomicBool::new(false),
            poison_pool_after_stop: Some(pool_path.clone()),
        };
        let error = super::run_recover_with_chain(
            RecoverArgs {
                identity: RecoveryIdentityArgs {
                    note_key: None,
                    note_addr: None,
                },
                token_contract: None,
                market: None,
                pool: Some(pool_path),
            },
            &chain,
        )
        .await
        .expect_err("a poisoned pool must fail only after the confirmed STOP is reported")
        .to_string();

        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
        for fact in [
            "local pool persistence failed",
            "action=buyer_stop",
            "message_id=test-stop-message",
            "created_at=1",
            "toSeller=1",
            "refundToBuyer=2",
        ] {
            assert!(error.contains(fact), "missing {fact:?} in {error}");
        }
    }

    #[test]
    fn withdraw_shell_guidance_describes_conditional_selfdestruct() {
        assert_eq!(
            super::WITHDRAW_SHELL_GUIDANCE,
            "This withdraws finalized seller proceeds. If this drains the last finalized proceeds from a funded, closed, undisputed deal with no live offer, the TC also selfdestructs; otherwise it remains active."
        );
    }

    /// The operator-facing line must name the payee the compiled ABI allows: the deal's stored
    /// seller note, with no caller-selected recipient input.
    #[test]
    fn withdraw_shell_run_line_states_the_payee_the_abi_allows() {
        let abi: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../contracts/compiled/airegistry/TokenContract.abi.json"
        ))
        .expect("compiled TokenContract ABI parses");
        let inputs: Vec<&str> = abi["functions"]
            .as_array()
            .expect("compiled ABI declares functions")
            .iter()
            .find(|function| function["name"] == "withdrawShell")
            .expect("compiled ABI declares withdrawShell")["inputs"]
            .as_array()
            .expect("declared inputs")
            .iter()
            .map(|input| input["name"].as_str().expect("declared input name"))
            .collect();
        assert_eq!(
            inputs,
            vec!["amount"],
            "withdrawShell gained or lost an input; the operator-facing line must move with it"
        );

        let payee = super::WITHDRAW_SHELL_PAYEE;
        assert!(
            payee.contains("accepts only amount"),
            "the run line must match the contract input: {payee}"
        );
        assert!(
            payee.contains("seller note it stored at construction"),
            "the run line must name the payee the contract uses: {payee}"
        );
    }

    // ----: pool-only reclaim drives every recorded deal, exactly once each ----

    /// One fake never-opened deal: the chain facts `reclaim` decodes, plus the submission outcomes a
    /// real node can produce -- a clean failure, an **outcome-ambiguous** failure (the action landed and
    /// only the response was lost), and a bounded observation that stays stale.
    struct PoolReclaimDeal {
        buyer_note: String,
        buyer_pubkey: [u8; 32],
        state: Option<dexdo_core::DealChainState>,
        fail_submits: usize,
        ambiguous_submits: usize,
        deferred_submits: usize,
        duplicate_posts: usize,
        fail_confirms: usize,
    }

    /// A fake chain keyed by TokenContract. `cleanupUnopened` is modelled exactly as the contract
    /// behaves: it destroys the TC, so a second attempt on the same deal can find nothing to move.
    #[derive(Default)]
    struct PoolReclaimChain {
        deals: std::sync::Mutex<std::collections::BTreeMap<String, PoolReclaimDeal>>,
        /// Deliveries of the cleanup action, including transport retries of the same BOC.
        posts: std::sync::Mutex<Vec<String>>,
        /// Deliveries the modelled contract actually paid out -- the money moves.
        cleanups: std::sync::Mutex<Vec<String>>,
    }

    impl PoolReclaimChain {
        fn key(tc: &dexdo_core::Address) -> String {
            tc.with_workchain()
        }

        fn with_deal(
            self,
            token_contract: &str,
            buyer_note: &str,
            secret_hex: &str,
            state: Option<dexdo_core::DealChainState>,
            fail_submits: usize,
        ) -> Self {
            let keys = dexdo_core::KeyPair::from_secret_hex(secret_hex).unwrap();
            self.deals.lock().unwrap().insert(
                dexdo_core::Address::parse(token_contract)
                    .unwrap()
                    .with_workchain(),
                PoolReclaimDeal {
                    buyer_note: dexdo_core::Address::parse(buyer_note)
                        .unwrap()
                        .with_workchain(),
                    buyer_pubkey: dexdo_core::keypair_ed_pubkey(&keys).unwrap(),
                    state,
                    fail_submits,
                    ambiguous_submits: 0,
                    deferred_submits: 0,
                    duplicate_posts: 0,
                    fail_confirms: 0,
                },
            );
            self
        }

        fn patch(self, token_contract: &str, patch: impl FnOnce(&mut PoolReclaimDeal)) -> Self {
            let key = dexdo_core::Address::parse(token_contract)
                .unwrap()
                .with_workchain();
            patch(
                self.deals
                    .lock()
                    .unwrap()
                    .get_mut(&key)
                    .expect("known deal"),
            );
            self
        }

        /// The submit applies the cleanup on chain and then fails: the CLI cannot tell whether it landed.
        fn with_ambiguous_submit(self, token_contract: &str) -> Self {
            self.patch(token_contract, |deal| deal.ambiguous_submits = 1)
        }

        /// The bounded observation window stays stale for the first attempt.
        fn with_stale_observation(self, token_contract: &str) -> Self {
            self.patch(token_contract, |deal| deal.fail_confirms = 1)
        }

        /// `send_with_retry` delivers the same BOC a second time on a transient error.
        fn with_duplicate_post(self, token_contract: &str) -> Self {
            self.patch(token_contract, |deal| deal.duplicate_posts = 1)
        }

        /// The submission goes out and then errors, and its BOC has not been applied yet: the outcome is
        /// genuinely unknown to the client and the action can still land at any later moment.
        fn with_deferred_submit(self, token_contract: &str) -> Self {
            self.patch(token_contract, |deal| deal.deferred_submits = 1)
        }

        /// One delivery of `cleanupUnopened`, gated exactly as the contract gates it: it pays only a
        /// funded, never-opened deal, and destroys it as it refunds. Every later delivery of the same
        /// action therefore finds nothing to move.
        fn deliver(
            &self,
            deals: &mut std::collections::BTreeMap<String, PoolReclaimDeal>,
            key: &str,
        ) {
            self.posts.lock().unwrap().push(key.to_string());
            let deal = deals.get_mut(key).expect("delivery to a known deal");
            if deal
                .state
                .is_some_and(|state| state.funded && !state.opened)
            {
                self.cleanups.lock().unwrap().push(key.to_string());
                deal.state = None;
            }
        }

        /// A submission whose response was lost lands later, out of band, exactly like a retried BOC.
        fn deliver_pending(&self, token_contract: &str) {
            let key = dexdo_core::Address::parse(token_contract)
                .unwrap()
                .with_workchain();
            let mut deals = self.deals.lock().unwrap();
            self.deliver(&mut deals, &key);
        }

        fn posts(&self) -> Vec<String> {
            self.posts.lock().unwrap().clone()
        }

        /// The deal is really owned by a different buyer note (a corrupt/forged recovery record).
        fn owned_by_note(self, token_contract: &str, buyer_note: &str) -> Self {
            let buyer_note = dexdo_core::Address::parse(buyer_note)
                .unwrap()
                .with_workchain();
            self.patch(token_contract, move |deal| deal.buyer_note = buyer_note)
        }

        /// The deal is really owned by a different buyer key.
        fn owned_by_key(self, token_contract: &str, secret_hex: &str) -> Self {
            let keys = dexdo_core::KeyPair::from_secret_hex(secret_hex).unwrap();
            let buyer_pubkey = dexdo_core::keypair_ed_pubkey(&keys).unwrap();
            self.patch(token_contract, move |deal| deal.buyer_pubkey = buyer_pubkey)
        }

        fn cleanups(&self) -> Vec<String> {
            self.cleanups.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl super::ReclaimChain for PoolReclaimChain {
        async fn state(
            &self,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::DealChainState>> {
            Ok(self
                .deals
                .lock()
                .unwrap()
                .get(&Self::key(tc))
                .and_then(|deal| deal.state))
        }

        async fn buyer_note(
            &self,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::Address>> {
            Ok(self
                .deals
                .lock()
                .unwrap()
                .get(&Self::key(tc))
                .map(|deal| dexdo_core::Address::parse(&deal.buyer_note).unwrap()))
        }

        async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> anyhow::Result<Option<[u8; 32]>> {
            Ok(self
                .deals
                .lock()
                .unwrap()
                .get(&Self::key(tc))
                .map(|deal| deal.buyer_pubkey))
        }

        async fn submit_cleanup(
            &self,
            note: &dexdo_core::Address,
            keys: &dexdo_core::KeyPair,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<()> {
            let key = Self::key(tc);
            let mut deals = self.deals.lock().unwrap();
            let deal = deals
                .get_mut(&key)
                .expect("cleanup is only ever submitted for a deal the chain knows");
            assert_eq!(
                note.with_workchain(),
                deal.buyer_note,
                "each entry must be signed by its own recorded note"
            );
            assert_eq!(
                dexdo_core::keypair_ed_pubkey(keys).unwrap(),
                deal.buyer_pubkey,
                "each entry must be signed by its own recorded owner key"
            );
            if deal.fail_submits > 0 {
                deal.fail_submits -= 1;
                anyhow::bail!("simulated cleanupUnopened submit failure for {key}");
            }
            if deal.ambiguous_submits > 0 {
                deal.ambiguous_submits -= 1;
                self.deliver(&mut deals, &key);
                anyhow::bail!("simulated lost response after cleanupUnopened landed for {key}");
            }
            if deal.deferred_submits > 0 {
                deal.deferred_submits -= 1;
                // The BOC is out -- recorded as a delivery attempt -- but has not been applied yet.
                self.posts.lock().unwrap().push(key.clone());
                anyhow::bail!("simulated submission of {key} whose outcome is still unknown");
            }
            let retries = deal.duplicate_posts;
            self.deliver(&mut deals, &key);
            for _ in 0..retries {
                self.deliver(&mut deals, &key);
            }
            Ok(())
        }

        async fn confirm_cleanup(&self, tc: &dexdo_core::Address) -> anyhow::Result<()> {
            let key = Self::key(tc);
            let mut deals = self.deals.lock().unwrap();
            let deal = deals.get_mut(&key).expect("observation of a known deal");
            if deal.fail_confirms > 0 {
                deal.fail_confirms -= 1;
                anyhow::bail!("simulated stale observation window for {key}");
            }
            // The real observer polls until the TokenContract is absent or no longer funded, and errors
            // out otherwise; it never reports a still-funded deal as a confirmed cleanup.
            match &deal.state {
                None => Ok(()),
                Some(state) if !state.funded => Ok(()),
                Some(_) => {
                    anyhow::bail!("simulated observation: TokenContract {key} is still funded")
                }
            }
        }
    }

    fn reclaim_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn write_reclaim_pool(dir: &std::path::Path, notes: serde_json::Value) -> std::path::PathBuf {
        let pool_path = dir.join("pn_pool.json");
        crate::cli::support::write_owner_only_key_fixture(
            &pool_path,
            &serde_json::to_string_pretty(&serde_json::json!({
                "token_type": dexdo_core::params::SHELL_CURRENCY_ID,
                "notes": notes,
            }))
            .unwrap(),
        );
        pool_path
    }

    /// The production entry point of `dexdo reclaim` given a pool file: the real plan resolver over the real
    /// pool file, then the real driver. Only the chain and the durable subscription marker are faked.
    async fn run_pool_only_reclaim(
        pool_path: &std::path::Path,
        chain: &PoolReclaimChain,
    ) -> anyhow::Result<()> {
        // `Ok(false)`: these fixtures carry no durable subscription record, so there is nothing to mark.
        run_pool_only_reclaim_with_marker(pool_path, chain, &|_note, _tc| Ok(false)).await
    }

    async fn run_pool_only_reclaim_with_marker(
        pool_path: &std::path::Path,
        chain: &PoolReclaimChain,
        marker: &(dyn Fn(&str, &str) -> anyhow::Result<bool> + Sync),
    ) -> anyhow::Result<()> {
        let plan = crate::cli::commands::resolve_pool_recovery_plan(
            &RecoveryIdentityArgs {
                note_key: None,
                note_addr: None,
            },
            None,
            None,
            Some(pool_path),
        )?;
        super::drive_reclaim_plan(plan, chain, marker).await
    }

    fn never_opened_state() -> dexdo_core::DealChainState {
        chain_state(true, false, false, false, 100)
    }

    fn note_a() -> (String, String, String) {
        (
            format!("0:{}", "1".repeat(64)),
            "2a".repeat(32),
            format!("0:{}", "2".repeat(64)),
        )
    }

    fn note_b() -> (String, String, String) {
        (
            format!("0:{}", "3".repeat(64)),
            "3b".repeat(32),
            format!("0:{}", "4".repeat(64)),
        )
    }

    /// One recorded pool row, exactly as the buyer writes it.
    fn recorded_row(
        note_addr: &str,
        secret: &str,
        token_contract: &str,
        recorded_at_unix: u64,
    ) -> serde_json::Value {
        recorded_row_as(note_addr, secret, token_contract, recorded_at_unix, "buyer")
    }

    fn recorded_row_as(
        note_addr: &str,
        secret: &str,
        token_contract: &str,
        recorded_at_unix: u64,
        role: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "address": note_addr,
            "owner_secret_key_hex": secret,
            "token_contract": token_contract,
            "token_contract_role": role,
            "token_contract_updated_at_unix": recorded_at_unix,
        })
    }

    /// primary regression: an ordinary pool holding two recoverable entries is driven from pool
    /// metadata alone -- no `--note-addr`, no `--token-contract` -- and the order comes from each entry's
    /// own recorded `token_contract_updated_at_unix`, not from its position in the file.
    #[tokio::test]
    async fn pool_only_reclaim_drives_every_recorded_entry_in_recorded_order() {
        let dir = reclaim_test_dir("reclaim-two-entries");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        // The later-recorded deal is written first, so file order and recorded order disagree.
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                },
                {
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 100
                }
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("pool metadata alone must be enough to reclaim every recorded deal");

        let tc_a = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();
        let tc_b = dexdo_core::Address::parse(&tc_b).unwrap().with_workchain();
        assert_eq!(
            chain.cleanups(),
            vec![tc_b, tc_a],
            "both recorded deals are reclaimed, earliest recorded first"
        );
    }

    /// Restart/idempotence: the very same invocation, run twice, moves no deal's money twice.
    #[tokio::test]
    async fn pool_only_reclaim_repeated_invocation_moves_no_money_twice() {
        let dir = reclaim_test_dir("reclaim-repeat");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 100
                },
                {
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                }
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);

        run_pool_only_reclaim(&pool_path, &chain).await.unwrap();
        let after_first = chain.cleanups();
        assert_eq!(after_first.len(), 2);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("re-running the identical command is a decided no-op, not a failure");
        assert_eq!(
            chain.cleanups(),
            after_first,
            "an already-reclaimed deal must never be moved a second time"
        );
    }

    /// An entry that was already reclaimed (its TokenContract is gone) and an entry that was never funded
    /// are both decided as no-ops, while the one live recoverable entry is still recovered.
    #[tokio::test]
    async fn pool_only_reclaim_skips_already_reclaimed_entries_without_a_second_move() {
        let dir = reclaim_test_dir("reclaim-already-done");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let note_c = format!("0:{}", "5".repeat(64));
        let secret_c = "4c".repeat(32);
        let tc_c = format!("0:{}", "6".repeat(64));
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 100
                },
                {
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                },
                {
                    "address": note_c,
                    "owner_secret_key_hex": secret_c,
                    "token_contract": tc_c,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 300
                }
            ]),
        );
        let chain = PoolReclaimChain::default()
            // already reclaimed: the TokenContract selfdestructed
            .with_deal(&tc_a, &note_a, &secret_a, None, 0)
            // never funded: nothing to reclaim
            .with_deal(
                &tc_b,
                &note_b,
                &secret_b,
                Some(chain_state(false, false, false, false, 0)),
                0,
            )
            .with_deal(&tc_c, &note_c, &secret_c, Some(never_opened_state()), 0);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("decided no-ops must not fail the recovery of the other entries");
        assert_eq!(
            chain.cleanups(),
            vec![dexdo_core::Address::parse(&tc_c).unwrap().with_workchain()],
            "only the one live never-opened deal may move money"
        );
    }

    /// Contradictory records still fail closed: a note recorded against two different TokenContracts, and
    /// one TokenContract recorded against two different notes, are both refused outright -- the pool is
    /// never guessed at -- while an unambiguous entry alongside them is still recovered.
    #[tokio::test]
    async fn pool_only_reclaim_fails_closed_on_contradictory_records() {
        let dir = reclaim_test_dir("reclaim-contradictory");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let contradicting_tc = format!("0:{}", "7".repeat(64));
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 100
                },
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": contradicting_tc,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 150
                },
                {
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                }
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(
                &contradicting_tc,
                &note_a,
                &secret_a,
                Some(never_opened_state()),
                0,
            )
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("a contradictory record must keep the command failing closed")
            .to_string();
        assert!(
            error.contains("2 of 3 recorded entries were not decided (2 refused as contradictory)"),
            "{error}"
        );
        assert_eq!(
            chain.cleanups(),
            vec![dexdo_core::Address::parse(&tc_b).unwrap().with_workchain()],
            "neither TokenContract of the contradictory note may be touched"
        );
    }

    /// The same fail-closed rule for the mirror-image contradiction: two notes recorded as the buyer of
    /// one TokenContract.
    #[tokio::test]
    async fn pool_only_reclaim_fails_closed_when_two_notes_claim_one_deal() {
        let dir = reclaim_test_dir("reclaim-two-notes-one-tc");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, ..) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 100
                },
                {
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                }
            ]),
        );
        let chain = PoolReclaimChain::default().with_deal(
            &tc_a,
            &note_a,
            &secret_a,
            Some(never_opened_state()),
            0,
        );

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("two notes claiming one deal must fail closed")
            .to_string();
        assert!(error.contains("2 refused as contradictory"), "{error}");
        assert!(chain.cleanups().is_empty(), "no money may move");
    }

    /// A single recorded entry keeps its pre- behaviour exactly: it is reclaimed when it is
    /// reclaimable, and a deal this command cannot move stays a loud, non-zero failure.
    #[tokio::test]
    async fn single_recorded_entry_keeps_the_unchanged_loud_failure() {
        let dir = reclaim_test_dir("reclaim-single");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([{
                "address": note_a,
                "owner_secret_key_hex": secret_a,
                "token_contract": tc_a,
                "token_contract_role": "buyer",
                "token_contract_updated_at_unix": 100
            }]),
        );

        let gone = PoolReclaimChain::default().with_deal(&tc_a, &note_a, &secret_a, None, 0);
        let error = run_pool_only_reclaim(&pool_path, &gone)
            .await
            .expect_err("a single unreclaimable entry stays a loud failure")
            .to_string();
        // the refusal names the TokenContract canonically, and a TokenContract is a self-DApp
        // account, so its DApp half is its own account id.
        let tc_a_account = tc_a.strip_prefix("0:").expect("fixture is the chain form");
        assert_eq!(
            error,
            format!("reclaim: TokenContract {tc_a_account}::{tc_a_account} is not active (undeployed/closed)")
        );
        assert!(gone.cleanups().is_empty());

        let open = PoolReclaimChain::default().with_deal(
            &tc_a,
            &note_a,
            &secret_a,
            Some(chain_state(true, true, false, false, 100)),
            0,
        );
        let error = run_pool_only_reclaim(&pool_path, &open)
            .await
            .expect_err("an OPEN deal still refuses the never-opened cleanup")
            .to_string();
        assert!(
            error.contains("must use the explicit buyer STOP path"),
            "{error}"
        );
        assert!(open.cleanups().is_empty());

        let live = PoolReclaimChain::default().with_deal(
            &tc_a,
            &note_a,
            &secret_a,
            Some(never_opened_state()),
            0,
        );
        run_pool_only_reclaim(&pool_path, &live)
            .await
            .expect("a single reclaimable entry is reclaimed as before");
        assert_eq!(live.cleanups().len(), 1);
    }

    /// Option (1)'s own risk: a failure part-way through the sequence must neither lose the remaining
    /// entries nor let a retry drive an already-reclaimed one a second time.
    #[tokio::test]
    async fn pool_only_reclaim_partial_failure_neither_loses_nor_double_drives_the_rest() {
        let dir = reclaim_test_dir("reclaim-partial-failure");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let note_c = format!("0:{}", "5".repeat(64));
        let secret_c = "4c".repeat(32);
        let tc_c = format!("0:{}", "6".repeat(64));
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 100
                },
                {
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                },
                {
                    "address": note_c,
                    "owner_secret_key_hex": secret_c,
                    "token_contract": tc_c,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 300
                }
            ]),
        );
        // The middle entry's submit fails once, exactly as a transient submission failure would.
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 1)
            .with_deal(&tc_c, &note_c, &secret_c, Some(never_opened_state()), 0);

        let tc_a = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();
        let tc_b = dexdo_core::Address::parse(&tc_b).unwrap().with_workchain();
        let tc_c = dexdo_core::Address::parse(&tc_c).unwrap().with_workchain();

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("a failed entry must surface as a non-zero exit")
            .to_string();
        assert!(
            error.contains("simulated cleanupUnopened submit failure"),
            "{error}"
        );
        assert_eq!(
            chain.cleanups(),
            vec![tc_a.clone(), tc_c.clone()],
            "the entries after the failure are still driven"
        );

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("the retry recovers the remaining entry");
        assert_eq!(
            chain.cleanups(),
            vec![tc_a, tc_c, tc_b],
            "the retry moves only the entry that was never reclaimed"
        );
    }

    /// what `reclaim` reports when a pool records nothing, and what it still reports when a pool
    /// records something whose reclaim failed. The two belong together: an assertion that an empty
    /// sweep exits 0 would pass just as well against a client that had stopped reporting failures at
    /// all, so the pair -- and not the first test alone -- is what pins the distinction.
    mod empty_reclaim_plan_is_not_a_failure {
        use super::*;

        /// Only the buyer client ever writes `token_contract` into a pool entry, so a pool whose notes
        /// never bought carries no reclaimable record at all -- and an operator sweeping `reclaim` over
        /// every pool they hold meets that on every run. Nothing was refused, nothing failed, and no
        /// money was left behind: this is the complete answer to what the command was asked.
        #[tokio::test]
        async fn a_pool_recording_no_deal_succeeds_and_submits_nothing() {
            let dir = reclaim_test_dir("reclaim-empty-plan");
            let _cleanup = TempDirCleanup(dir.clone());
            let (note_a, secret_a, _) = note_a();
            let (note_b, secret_b, _) = note_b();
            let pool_path = write_reclaim_pool(
                &dir,
                serde_json::json!([
                    { "address": note_a, "owner_secret_key_hex": secret_a },
                    { "address": note_b, "owner_secret_key_hex": secret_b },
                ]),
            );
            let chain = PoolReclaimChain::default();

            run_pool_only_reclaim(&pool_path, &chain)
                .await
                .expect("a pool that records no reclaimable deal is an ordinary, successful sweep");
            assert!(
                chain.posts().is_empty(),
                "an empty plan must reach the chain not at all"
            );
        }

        /// The other end, and the one that must not move: the same sweep over a pool that DOES record
        /// deals, whose submits fail and whose bounded observation still finds each deal funded. Two
        /// recorded entries, so the decision is made by the aggregate branch an empty plan now returns
        /// through -- the exact place where silencing an absence could turn into silencing a failure.
        #[tokio::test]
        async fn a_pool_recording_deals_that_failed_to_reclaim_still_fails() {
            let dir = reclaim_test_dir("reclaim-recorded-but-failing");
            let _cleanup = TempDirCleanup(dir.clone());
            let (note_a, secret_a, tc_a) = note_a();
            let (note_b, secret_b, tc_b) = note_b();
            let pool_path = write_reclaim_pool(
                &dir,
                serde_json::json!([
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                    recorded_row(&note_b, &secret_b, &tc_b, 200),
                ]),
            );
            let chain = PoolReclaimChain::default()
                .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 1)
                .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 1);

            let error = run_pool_only_reclaim(&pool_path, &chain)
                .await
                .expect_err("recorded deals whose reclaim failed must still exit non-zero")
                .to_string();
            assert!(
                error.contains("2 of 2 recorded entries were not decided"),
                "{error}"
            );
            assert!(
                error.contains("simulated cleanupUnopened submit failure"),
                "{error}"
            );
            assert!(
                chain.cleanups().is_empty(),
                "no money moved, and the command said so by failing"
            );
        }

        /// The half an unfiltered sweep cannot reach. `--token-contract` is a FILTER, applied in
        /// `matching_pool_recovery_records` by dropping every row that misses, so a deal that is named
        /// and not recorded arrives at the plan as the very same empty set as a pool that records
        /// nothing at all. The two answer different questions -- "reclaim everything here" and nothing
        /// recorded is complete, "reclaim deal X" and no X is not -- and collapsing them is how
        /// `reclaim` would exit 0 on a deal it never touched.

        /// Driven through `resolve_pool_recovery_plan` because that is the function `run_reclaim`
        /// itself calls; it never enters `resolve_recovery_inputs`, which is where the single-deal
        /// refusal for `recover`/`dispute` lives.
        #[test]
        fn a_named_deal_this_pool_does_not_record_is_refused_not_reported_as_empty() {
            let dir = reclaim_test_dir("reclaim-named-deal-absent");
            let _cleanup = TempDirCleanup(dir.clone());
            let (note_a, secret_a, tc_a) = note_a();
            let (_note_b, _secret_b, tc_b) = note_b();
            let pool_path = write_reclaim_pool(
                &dir,
                serde_json::json!([recorded_row(&note_a, &secret_a, &tc_a, 100)]),
            );

            // `expect_err` would demand `Debug` on `PoolRecoveryPlan`, whose targets carry a note owner
            // secret; the match asks the same question without widening a production type for a test.
            let error = match crate::cli::commands::resolve_pool_recovery_plan(
                &RecoveryIdentityArgs {
                    note_key: None,
                    note_addr: None,
                },
                None,
                Some(tc_b.as_str()),
                Some(pool_path.as_path()),
            ) {
                Ok(_) => panic!(
                    "a deal named with --token-contract that this pool does not record must refuse, \
                     or `reclaim` reports success having touched nothing"
                ),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("records no deal matching --token-contract"),
                "the refusal names the filter that matched nothing: {error}"
            );

            // The other end, on the SAME pool and through the same call. Without it the assertion above
            // would pass just as well against a client that had started refusing EVERY filter, matching
            // or not -- which would break the one thing was opened to fix.
            let matched = crate::cli::commands::resolve_pool_recovery_plan(
                &RecoveryIdentityArgs {
                    note_key: None,
                    note_addr: None,
                },
                None,
                Some(tc_a.as_str()),
                Some(pool_path.as_path()),
            )
            .expect("the deal this pool DOES record still plans when it is named");
            assert_eq!(
                matched.targets.len(),
                1,
                "a filter that matches must still plan exactly the deal it names"
            );
            assert_eq!(
                matched.targets[0].token_contract, tc_a,
                "and it must be that deal, not some other row of the pool"
            );
        }
    }

    /// `--note-key` names one note's owner key: applying it to several recorded notes is an ambiguous
    /// instruction on a money path, so it fails closed instead of guessing.
    #[test]
    fn note_key_with_several_recorded_entries_fails_closed() {
        let dir = reclaim_test_dir("reclaim-note-key-ambiguous");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                {
                    "address": note_a,
                    "owner_secret_key_hex": secret_a,
                    "token_contract": tc_a,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 100
                },
                {
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                }
            ]),
        );
        let key_path = dir.join("note.key");
        crate::cli::support::write_owner_only_key_fixture(&key_path, &secret_a);

        // Not `expect_err`: a plan carries note secrets, and nothing on this path may render them.
        let error = match crate::cli::commands::resolve_pool_recovery_plan(
            &RecoveryIdentityArgs {
                note_key: Some(key_path),
                note_addr: None,
            },
            None,
            None,
            Some(&pool_path),
        ) {
            Ok(_) => panic!("one key cannot stand for several recorded notes"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("--note-key names a single"), "{error}");
    }

    /// A recorded entry the chain contradicts on ownership is a corrupt or forged recovery record, not a
    /// decided no-op: it must never be counted as a clean "nothing to do" beside a valid sibling.
    #[tokio::test]
    async fn contradicted_buyer_note_or_key_fails_loud_with_no_submit() {
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let foreign_note = format!("0:{}", "9".repeat(64));
        let foreign_secret = "5d".repeat(32);

        for (name, expected) in [
            ("wrong-note", "the deal's buyer note is"),
            ("wrong-key", "is not the buyer key of"),
        ] {
            let dir = reclaim_test_dir(&format!("reclaim-contradicted-{name}"));
            let _cleanup = TempDirCleanup(dir.clone());
            let pool_path = write_reclaim_pool(
                &dir,
                serde_json::json!([
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                    recorded_row(&note_b, &secret_b, &tc_b, 200),
                ]),
            );
            let chain = PoolReclaimChain::default()
                .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
                .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);
            let chain = if name == "wrong-note" {
                chain.owned_by_note(&tc_a, &foreign_note)
            } else {
                chain.owned_by_key(&tc_a, &foreign_secret)
            };

            let error = run_pool_only_reclaim(&pool_path, &chain)
                .await
                .expect_err("a contradicted ownership record must not exit 0")
                .to_string();
            assert!(error.contains(expected), "{name}: {error}");
            assert!(error.contains("nothing was submitted"), "{name}: {error}");
            assert_eq!(
                chain.cleanups(),
                vec![dexdo_core::Address::parse(&tc_b).unwrap().with_workchain()],
                "{name}: the contradicted entry must move no money while its valid sibling still does"
            );
        }
    }

    /// A submit whose outcome is ambiguous (the action landed, the response was lost) is reconciled by
    /// the same bounded observation, never by submitting the money action again.
    #[tokio::test]
    async fn ambiguous_submit_is_reconciled_by_observation_without_a_second_submit() {
        let dir = reclaim_test_dir("reclaim-ambiguous-submit");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_a, 100),
                recorded_row(&note_b, &secret_b, &tc_b, 200),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0)
            .with_ambiguous_submit(&tc_a);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("a cleanup proven landed by observation is a success, not a failure");
        let tc_a_key = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();
        let tc_b_key = dexdo_core::Address::parse(&tc_b).unwrap().with_workchain();
        assert_eq!(chain.cleanups(), vec![tc_a_key.clone(), tc_b_key.clone()]);

        // Restart: the reconciled deal is already terminal, so nothing is submitted for it again.
        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("the restart is a decided no-op");
        assert_eq!(chain.cleanups(), vec![tc_a_key, tc_b_key]);
    }

    /// A submit that fails while the observation window stays stale is genuinely unresolved: it must be
    /// loud, must not be re-submitted, and must still be recoverable on a later run.
    #[tokio::test]
    async fn unresolved_submit_outcome_is_loud_and_still_reclaimable_later() {
        let dir = reclaim_test_dir("reclaim-unresolved-submit");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_a, 100),
                recorded_row(&note_b, &secret_b, &tc_b, 200),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 1)
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);
        let tc_a_key = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();
        let tc_b_key = dexdo_core::Address::parse(&tc_b).unwrap().with_workchain();

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("an unresolved submit outcome must exit non-zero")
            .to_string();
        assert!(error.contains("its outcome is unresolved"), "{error}");
        assert!(
            error.contains("this run drove no further cleanup for it"),
            "{error}"
        );
        assert_eq!(chain.cleanups(), vec![tc_b_key.clone()]);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("the retry recovers the entry whose submit had failed");
        assert_eq!(chain.cleanups(), vec![tc_b_key, tc_a_key]);
    }

    /// The guarantee actually needs, stated and tested as itself: **no second money move**. The
    /// transport may deliver the same cleanup BOC more than once, and an operator may re-run the command
    /// after an unresolved outcome -- neither can pay the deal out twice, because `cleanupUnopened` only
    /// accepts a funded, never-opened deal and destroys it as it refunds.
    #[tokio::test]
    async fn a_duplicated_cleanup_post_moves_the_deals_money_only_once() {
        let (note_a, secret_a, tc_a) = note_a();
        let tc_a_key = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();

        // 1. The transport retries the same BOC: two deliveries, one payout.
        let dir = reclaim_test_dir("reclaim-duplicate-post");
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([recorded_row(&note_a, &secret_a, &tc_a, 100)]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_duplicate_post(&tc_a);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("a retried delivery is still one successful reclaim");
        assert_eq!(
            chain.posts(),
            vec![tc_a_key.clone(), tc_a_key.clone()],
            "the transport delivered the action twice"
        );
        assert_eq!(
            chain.cleanups(),
            vec![tc_a_key.clone()],
            "the contract paid the deal exactly once"
        );

        // 2. An unresolved submit that lands out of band BEFORE the operator re-runs: the re-run finds
        // the deal already settled, so it submits nothing at all and no second payout is possible.
        let dir = reclaim_test_dir("reclaim-late-landing");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_a, 100),
                recorded_row(&note_b, &secret_b, &tc_b, 200),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 1)
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);
        let tc_b_key = dexdo_core::Address::parse(&tc_b).unwrap().with_workchain();

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("the first run cannot resolve the submitted outcome")
            .to_string();
        assert!(error.contains("its outcome is unresolved"), "{error}");
        assert!(error.contains("cannot pay it out twice"), "{error}");
        assert_eq!(chain.cleanups(), vec![tc_b_key.clone()]);

        // The lost submission lands out of band, exactly like a retried BOC would.
        chain.deliver_pending(&tc_a);
        assert_eq!(chain.cleanups(), vec![tc_b_key.clone(), tc_a_key.clone()]);
        let posts_before_rerun = chain.posts();

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("the re-run finds both deals settled");
        assert_eq!(
            chain.posts(),
            posts_before_rerun,
            "a deal already settled on chain is not submitted again at all"
        );
        assert_eq!(
            chain.cleanups(),
            vec![tc_b_key, tc_a_key.clone()],
            "the landed submission paid once and the re-run added nothing"
        );

        // 3. The case the guarantee is really about: the first run's BOC is still in flight when the
        // operator re-runs, so the deal still looks funded and the re-run DOES submit a second time.
        // Then the first submission lands too -- three deliveries of the same action, one payout.
        let dir = reclaim_test_dir("reclaim-inflight-rerun");
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([recorded_row(&note_a, &secret_a, &tc_a, 100)]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deferred_submit(&tc_a);

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("an in-flight submission is an unresolved outcome")
            .to_string();
        assert!(error.contains("its outcome is unresolved"), "{error}");
        assert_eq!(chain.posts(), vec![tc_a_key.clone()], "one delivery so far");
        assert!(chain.cleanups().is_empty(), "nothing has been paid yet");

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("the re-run drives the still-funded deal");
        assert_eq!(
            chain.posts(),
            vec![tc_a_key.clone(), tc_a_key.clone()],
            "the re-run submitted the same action a second time"
        );

        // The first, in-flight submission finally lands on top of the settled deal.
        chain.deliver_pending(&tc_a);
        assert_eq!(
            chain.posts(),
            vec![tc_a_key.clone(), tc_a_key.clone(), tc_a_key.clone()],
            "three deliveries of the same cleanup action"
        );
        assert_eq!(
            chain.cleanups(),
            vec![tc_a_key],
            "the contract paid the deal exactly once across all three"
        );
    }

    /// The other ambiguous shape: the submit is accepted but the bounded observation window stays
    /// stale. That is not a settled cleanup, so it must be loud -- and the retry must not submit again.
    #[tokio::test]
    async fn accepted_submit_with_a_stale_observation_is_never_reported_as_settled() {
        let dir = reclaim_test_dir("reclaim-stale-observation");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_a, 100),
                recorded_row(&note_b, &secret_b, &tc_b, 200),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0)
            .with_stale_observation(&tc_a);
        let tc_a_key = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();
        let tc_b_key = dexdo_core::Address::parse(&tc_b).unwrap().with_workchain();

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("an unconfirmed cleanup must never be reported as settled")
            .to_string();
        assert!(error.contains("settlement is not confirmed"), "{error}");
        assert_eq!(chain.cleanups(), vec![tc_a_key.clone(), tc_b_key.clone()]);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("the retry observes the settled deal and moves nothing");
        assert_eq!(chain.cleanups(), vec![tc_a_key, tc_b_key]);
    }

    /// A confirmed cleanup whose local marker failed leaves durable subscription state stale. The next
    /// run must repair it -- with zero submits -- instead of walking past it forever.
    #[tokio::test]
    async fn failed_subscription_marker_is_repaired_on_the_next_run_without_a_second_submit() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = reclaim_test_dir("reclaim-marker-repair");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_a, 100),
                recorded_row(&note_b, &secret_b, &tc_b, 200),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
            // The sibling is already terminal; only the marker keeps it interesting.
            .with_deal(&tc_b, &note_b, &secret_b, None, 0);
        let marker_calls = AtomicUsize::new(0);
        let marker = |_note: &str, _tc: &str| {
            if marker_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("simulated durable subscription write failure");
            }
            Ok(true)
        };

        let error = run_pool_only_reclaim_with_marker(&pool_path, &chain, &marker)
            .await
            .expect_err("a failed marker after a confirmed money move must be loud")
            .to_string();
        assert!(
            error.contains("simulated durable subscription write failure"),
            "{error}"
        );
        let tc_a_key = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();
        assert_eq!(chain.cleanups(), vec![tc_a_key.clone()]);

        run_pool_only_reclaim_with_marker(&pool_path, &chain, &marker)
            .await
            .expect("the repair run succeeds");
        assert_eq!(
            chain.cleanups(),
            vec![tc_a_key],
            "the repair must not move money again"
        );
        assert_eq!(
            marker_calls.load(Ordering::SeqCst),
            4,
            "run 1 marks the reclaimed deal (failing) and the terminal sibling; run 2 re-marks both, \
             which is what repairs the record the first run could not write"
        );
    }

    /// A pool row that claims recovery metadata but is missing or mistyping part of it is refused before
    /// any chain contact -- never silently dropped from the plan while its escrow stays stranded.
    #[tokio::test]
    async fn malformed_recovery_metadata_is_refused_before_any_chain_contact() {
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        for (name, malformed, expected) in [
            (
                "no-secret",
                serde_json::json!({
                    "address": note_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                }),
                "no string owner_secret_key_hex",
            ),
            (
                "no-address",
                serde_json::json!({
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": 200
                }),
                "no string address",
            ),
            (
                "non-string-tc",
                serde_json::json!({
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": 7,
                    "token_contract_role": "buyer"
                }),
                "token_contract is present but is not a string",
            ),
            (
                "malformed-timestamp",
                serde_json::json!({
                    "address": note_b,
                    "owner_secret_key_hex": secret_b,
                    "token_contract": tc_b,
                    "token_contract_role": "buyer",
                    "token_contract_updated_at_unix": "yesterday"
                }),
                "token_contract_updated_at_unix is not a unix second count",
            ),
        ] {
            let dir = reclaim_test_dir(&format!("reclaim-malformed-{name}"));
            let _cleanup = TempDirCleanup(dir.clone());
            let pool_path = write_reclaim_pool(
                &dir,
                serde_json::json!([recorded_row(&note_a, &secret_a, &tc_a, 100), malformed]),
            );
            let chain = PoolReclaimChain::default().with_deal(
                &tc_a,
                &note_a,
                &secret_a,
                Some(never_opened_state()),
                0,
            );

            let error = run_pool_only_reclaim(&pool_path, &chain)
                .await
                .expect_err("malformed recovery metadata must be refused")
                .to_string();
            assert!(error.contains(expected), "{name}: {error}");
            assert!(
                chain.cleanups().is_empty(),
                "{name}: the refusal must happen before any chain contact"
            );
        }
    }

    /// The documented duplicate rule: rows for one recorded deal collapse only when every recorded fact
    /// agrees. Exact duplicates are one deal; rows that disagree on the role or the recorded time are a
    /// contradiction. Either way the outcome cannot depend on the order of rows in the file.
    #[tokio::test]
    async fn rows_for_one_deal_collapse_only_when_every_recorded_fact_agrees() {
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let tc_a_key = dexdo_core::Address::parse(&tc_a).unwrap().with_workchain();
        let tc_b_key = dexdo_core::Address::parse(&tc_b).unwrap().with_workchain();

        // Exact duplicate rows are one deal recorded twice: one money move, in both file orders.
        for (name, rows) in [
            (
                "duplicate-first",
                serde_json::json!([
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                    recorded_row(&note_b, &secret_b, &tc_b, 200),
                ]),
            ),
            (
                "duplicate-last",
                serde_json::json!([
                    recorded_row(&note_b, &secret_b, &tc_b, 200),
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                ]),
            ),
        ] {
            let dir = reclaim_test_dir(&format!("reclaim-{name}"));
            let _cleanup = TempDirCleanup(dir.clone());
            let pool_path = write_reclaim_pool(&dir, rows);
            let chain = PoolReclaimChain::default()
                .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
                .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);

            run_pool_only_reclaim(&pool_path, &chain)
                .await
                .expect("exact duplicate rows are one deal, not a contradiction");
            assert_eq!(
                chain.cleanups(),
                vec![tc_a_key.clone(), tc_b_key.clone()],
                "{name}: one move per recorded deal, ordered by the recorded facts"
            );
        }

        // Rows that agree on the deal but disagree on a recorded fact are refused, in either order.
        let disagreeing_role = serde_json::json!({
            "address": note_a,
            "owner_secret_key_hex": secret_a,
            "token_contract": tc_a,
            "token_contract_role": "unknown",
            "token_contract_updated_at_unix": 100
        });
        let disagreeing_time = recorded_row(&note_a, &secret_a, &tc_a, 101);
        for (name, rows) in [
            (
                "role",
                serde_json::json!([
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                    disagreeing_role,
                    recorded_row(&note_b, &secret_b, &tc_b, 200),
                ]),
            ),
            (
                "time",
                serde_json::json!([
                    recorded_row(&note_b, &secret_b, &tc_b, 200),
                    disagreeing_time,
                    recorded_row(&note_a, &secret_a, &tc_a, 100),
                ]),
            ),
        ] {
            let dir = reclaim_test_dir(&format!("reclaim-disagreeing-{name}"));
            let _cleanup = TempDirCleanup(dir.clone());
            let pool_path = write_reclaim_pool(&dir, rows);
            let chain = PoolReclaimChain::default()
                .with_deal(&tc_a, &note_a, &secret_a, Some(never_opened_state()), 0)
                .with_deal(&tc_b, &note_b, &secret_b, Some(never_opened_state()), 0);

            let error = run_pool_only_reclaim(&pool_path, &chain)
                .await
                .expect_err("rows that disagree on a recorded fact must fail closed")
                .to_string();
            assert!(
                error.contains("whose recorded facts disagree"),
                "{name}: {error}"
            );
            assert_eq!(
                chain.cleanups(),
                vec![tc_b_key.clone()],
                "{name}: the contradicted deal must move no money"
            );
        }
    }

    /// A generated pool of recorded entries, with an index-derived identity per entry so every note,
    /// key and TokenContract is distinct.
    fn generated_entry(index: usize) -> (String, String, String) {
        let byte = format!("{:02x}", index as u8 + 1);
        (
            format!("0:{}", byte.repeat(32)),
            format!("{:02x}", index as u8 + 128).repeat(32),
            format!("0:{}", format!("{:02x}", index as u8 + 64).repeat(32)),
        )
    }

    /// What the generated entry's deal looks like on chain.
    #[derive(Clone, Debug)]
    enum GeneratedDeal {
        Reclaimable,
        Gone,
        Unfunded,
    }

    fn generated_plan_run(
        specs: &[(GeneratedDeal, u64, bool, bool)],
        rotation: usize,
    ) -> (Vec<String>, bool) {
        let dir = reclaim_test_dir("reclaim-proptest");
        let _cleanup = TempDirCleanup(dir.clone());
        let mut rows = Vec::new();
        let chain = PoolReclaimChain::default();
        let mut chain = chain;
        for (index, (deal, recorded_at, fail_once, duplicated)) in specs.iter().enumerate() {
            let (note, secret, tc) = generated_entry(index);
            rows.push(recorded_row(&note, &secret, &tc, *recorded_at));
            if *duplicated {
                rows.push(recorded_row(&note, &secret, &tc, *recorded_at));
            }
            let state = match deal {
                GeneratedDeal::Reclaimable => Some(never_opened_state()),
                GeneratedDeal::Gone => None,
                GeneratedDeal::Unfunded => Some(chain_state(false, false, false, false, 0)),
            };
            chain = chain.with_deal(&tc, &note, &secret, state, usize::from(*fail_once));
        }
        let rotate_by = rotation % rows.len().max(1);
        rows.rotate_right(rotate_by);
        let pool_path = write_reclaim_pool(&dir, serde_json::Value::Array(rows));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let first = runtime.block_on(run_pool_only_reclaim(&pool_path, &chain));
        // Restart twice more: neither may move any deal a second time.
        let second = runtime.block_on(run_pool_only_reclaim(&pool_path, &chain));
        let after_second = chain.cleanups();
        runtime
            .block_on(run_pool_only_reclaim(&pool_path, &chain))
            .expect("a settled pool is a decided no-op");
        assert_eq!(
            chain.cleanups(),
            after_second,
            "a settled pool must move nothing further"
        );
        assert!(
            second.is_ok(),
            "the retry after a transient submit failure must settle: {second:?}"
        );
        (chain.cleanups(), first.is_ok())
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        /// invariant, over arbitrary pools: pool-only reclaim moves **each** recoverable deal
        /// exactly once and no other deal at all, keeps going past a failing entry, exits non-zero
        /// exactly when an entry failed, and never moves a deal again on a restart. Duplicate rows are
        /// one deal, and the whole outcome is independent of the row order in the file.

        /// Sizes start at two recorded deals on purpose: a pool holding exactly one recorded entry keeps
        /// its deliberately different pre- contract, where a deal this command cannot move stays a
        /// loud failure -- `single_recorded_entry_keeps_the_unchanged_loud_failure` owns that case.
        #[test]
        fn pool_only_reclaim_moves_each_recoverable_deal_exactly_once(
            specs in proptest::collection::vec(
                (
                    proptest::prop_oneof![
                        proptest::strategy::Just(GeneratedDeal::Reclaimable),
                        proptest::strategy::Just(GeneratedDeal::Gone),
                        proptest::strategy::Just(GeneratedDeal::Unfunded),
                    ],
                    0u64..4,
                    proptest::bool::ANY,
                    proptest::bool::ANY,
                ),
                2..5,
            ),
            rotation in 0usize..8,
        ) {
            let (cleanups, first_ok) = generated_plan_run(&specs, 0);
            let expected: std::collections::BTreeSet<String> = specs
                .iter()
                .enumerate()
                .filter(|(_, (deal, ..))| matches!(deal, GeneratedDeal::Reclaimable))
                .map(|(index, _)| {
                    let (_, _, tc) = generated_entry(index);
                    dexdo_core::Address::parse(&tc).unwrap().with_workchain()
                })
                .collect();
            let moved: std::collections::BTreeSet<String> = cleanups.iter().cloned().collect();
            proptest::prop_assert_eq!(&moved, &expected, "every recoverable deal moves, nothing else does");
            proptest::prop_assert_eq!(
                cleanups.len(),
                moved.len(),
                "no deal may be moved twice across restarts"
            );
            let failed_first = specs
                .iter()
                .any(|(deal, _, fail_once, _)| matches!(deal, GeneratedDeal::Reclaimable) && *fail_once);
            proptest::prop_assert_eq!(
                first_ok,
                !failed_first,
                "the first run exits non-zero exactly when an entry failed"
            );

            // The same pool in a rotated row order settles on exactly the same moves, in the same order.
            let (rotated, rotated_ok) = generated_plan_run(&specs, rotation);
            proptest::prop_assert_eq!(&rotated, &cleanups, "row order must not change what runs");
            proptest::prop_assert_eq!(rotated_ok, first_ok, "row order must not change the exit status");
        }
    }

    /// re-review item 1: a contradicted deal must refuse every deal it touches. Counting the
    /// note/TokenContract collisions only among the groups that survived contradiction removal let the
    /// contradiction quietly clear the way for its own sibling, which then moved money.
    #[tokio::test]
    async fn a_contradicted_deal_also_refuses_the_deals_it_collides_with() {
        let (note_a, secret_a, tc_1) = generated_entry(0);
        let (note_b, secret_b, tc_2) = generated_entry(1);
        let (note_c, secret_c, tc_3) = generated_entry(2);
        let other_secret = "5d".repeat(32);
        let key = |tc: &str| dexdo_core::Address::parse(tc).unwrap().with_workchain();

        // Same note, two recorded deals, one of them internally contradicted.
        let dir = reclaim_test_dir("reclaim-contradiction-poisons-note");
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_1, 100),
                recorded_row(&note_a, &other_secret, &tc_1, 100),
                recorded_row(&note_a, &secret_a, &tc_2, 110),
                recorded_row(&note_c, &secret_c, &tc_3, 120),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_1, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_2, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_3, &note_c, &secret_c, Some(never_opened_state()), 0);

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("a contradicted note must not reclaim its other deal either")
            .to_string();
        assert!(error.contains("refused as contradictory"), "{error}");
        assert_eq!(
            chain.cleanups(),
            vec![key(&tc_3)],
            "only the unrelated note's deal may move money"
        );

        // Mirror image: one TokenContract recorded by two notes, one of those records contradicted.
        let dir = reclaim_test_dir("reclaim-contradiction-poisons-tc");
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_1, 100),
                recorded_row(&note_a, &other_secret, &tc_1, 100),
                recorded_row(&note_b, &secret_b, &tc_1, 110),
                recorded_row(&note_c, &secret_c, &tc_3, 120),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_1, &note_b, &secret_b, Some(never_opened_state()), 0)
            .with_deal(&tc_3, &note_c, &secret_c, Some(never_opened_state()), 0);

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("a contested TokenContract must not be reclaimed by the surviving claimant")
            .to_string();
        assert!(error.contains("refused as contradictory"), "{error}");
        assert_eq!(
            chain.cleanups(),
            vec![key(&tc_3)],
            "the contested TokenContract must move no money"
        );
    }

    /// re-review item 2: one deal recorded as both buyer and seller is a contradiction about that
    /// deal. The buyer-only pre-filter used to drop the seller row before anything could compare them.
    #[tokio::test]
    async fn a_deal_recorded_as_both_buyer_and_seller_fails_closed() {
        let dir = reclaim_test_dir("reclaim-role-contradiction");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_1) = generated_entry(0);
        let (note_b, secret_b, tc_2) = generated_entry(1);
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                recorded_row_as(&note_a, &secret_a, &tc_1, 100, "buyer"),
                recorded_row_as(&note_a, &secret_a, &tc_1, 100, "seller"),
                recorded_row(&note_b, &secret_b, &tc_2, 110),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_1, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_2, &note_b, &secret_b, Some(never_opened_state()), 0);

        let error = run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect_err("a same-deal role contradiction must fail closed")
            .to_string();
        assert!(error.contains("whose recorded facts disagree"), "{error}");
        assert_eq!(
            chain.cleanups(),
            vec![dexdo_core::Address::parse(&tc_2).unwrap().with_workchain()],
            "the contradicted deal must move no money"
        );
    }

    /// The other half of the same rule, and the shape of every real pool: a note that sold one deal and
    /// bought another is ordinary. Its seller record must neither join the buyer plan nor block it, and
    /// a seller record for the deal another note bought must not look like two claimants.
    #[tokio::test]
    async fn unrelated_seller_records_stay_outside_the_buyer_plan() {
        let dir = reclaim_test_dir("reclaim-seller-records");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_1) = generated_entry(0);
        let (_, _, tc_2) = generated_entry(1);
        let (note_b, secret_b, tc_3) = generated_entry(2);
        let (note_c, secret_c, _) = generated_entry(3);
        let pool_path = write_reclaim_pool(
            &dir,
            serde_json::json!([
                // note A sold tc_1 and bought tc_2
                recorded_row_as(&note_a, &secret_a, &tc_1, 100, "seller"),
                recorded_row_as(&note_a, &secret_a, &tc_2, 110, "buyer"),
                // note B sold tc_3, which note C bought -- both sides of one deal in one pool
                recorded_row_as(&note_b, &secret_b, &tc_3, 120, "seller"),
                recorded_row_as(&note_c, &secret_c, &tc_3, 130, "buyer"),
            ]),
        );
        let chain = PoolReclaimChain::default()
            .with_deal(&tc_2, &note_a, &secret_a, Some(never_opened_state()), 0)
            .with_deal(&tc_3, &note_c, &secret_c, Some(never_opened_state()), 0);

        run_pool_only_reclaim(&pool_path, &chain)
            .await
            .expect("seller records are not contradictions");
        assert_eq!(
            chain.cleanups(),
            vec![
                dexdo_core::Address::parse(&tc_2).unwrap().with_workchain(),
                dexdo_core::Address::parse(&tc_3).unwrap().with_workchain(),
            ],
            "both bought deals are reclaimed, in recorded order"
        );
    }

    // ----: `recover`/`dispute` on a pool holding more than one recovery entry ----

    use crate::cli::args::DisputeArgs;

    /// A chain that answers per TokenContract, so a pool recording several deals can be put in the state
    /// a real one is in: one deal still OPEN and its siblings already over. A TokenContract absent from
    /// `deals` is inactive, which is what a destroyed deal looks like; one listed in `unreadable` fails
    /// the read, which is what an unreachable node looks like.
    struct RecordedDeal {
        opened: bool,
        buyer_note: String,
        buyer_pubkey: [u8; 32],
    }

    #[derive(Default)]
    struct RecordedDealsChain {
        deals: std::collections::BTreeMap<String, RecordedDeal>,
        unreadable: std::collections::BTreeSet<String>,
        stopped: std::sync::Mutex<Vec<String>>,
        disputed: std::sync::Mutex<Vec<String>>,
    }

    impl RecordedDealsChain {
        fn with_deal(
            mut self,
            tc: &str,
            opened: bool,
            buyer_note: &str,
            buyer_secret: &str,
        ) -> Self {
            let keys = dexdo_core::KeyPair::from_secret_hex(buyer_secret).unwrap();
            self.deals.insert(
                dexdo_core::Address::parse(tc).unwrap().with_workchain(),
                RecordedDeal {
                    opened,
                    buyer_note: dexdo_core::Address::parse(buyer_note)
                        .unwrap()
                        .with_workchain(),
                    buyer_pubkey: dexdo_core::keypair_ed_pubkey(&keys).unwrap(),
                },
            );
            self
        }

        fn with_unreadable_deal(mut self, tc: &str) -> Self {
            self.unreadable
                .insert(dexdo_core::Address::parse(tc).unwrap().with_workchain());
            self
        }

        fn deal(&self, tc: &dexdo_core::Address) -> anyhow::Result<Option<&RecordedDeal>> {
            let tc = tc.with_workchain();
            if self.unreadable.contains(&tc) {
                anyhow::bail!("chain read failed for TokenContract {tc}");
            }
            Ok(self.deals.get(&tc))
        }

        fn stopped(&self) -> Vec<String> {
            self.stopped.lock().unwrap().clone()
        }

        fn disputed(&self) -> Vec<String> {
            self.disputed.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl super::RecoverChain for RecordedDealsChain {
        async fn state(
            &self,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::DealChainState>> {
            Ok(self
                .deal(tc)?
                .map(|deal| chain_state(true, deal.opened, true, false, 100)))
        }

        async fn buyer_note(
            &self,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::Address>> {
            Ok(self
                .deal(tc)?
                .map(|deal| dexdo_core::Address::parse(&deal.buyer_note).unwrap()))
        }

        async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> anyhow::Result<Option<[u8; 32]>> {
            Ok(self.deal(tc)?.map(|deal| deal.buyer_pubkey))
        }

        async fn stop(
            &self,
            note: &dexdo_core::Address,
            _keys: &dexdo_core::KeyPair,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<dexdo_core::SettlementActionReceipt> {
            let deal = self.deal(tc)?.expect("STOP on an inactive TokenContract");
            assert_eq!(
                note.with_workchain(),
                deal.buyer_note,
                "the STOP must be signed by the deal's own buyer note"
            );
            self.stopped.lock().unwrap().push(tc.with_workchain());
            Ok(test_stop_receipt(tc))
        }

        async fn settlement_receipts(
            &self,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<dexdo_core::TokenContractSettlementReceipts> {
            let tc_s = tc.with_workchain();
            if !self.stopped.lock().unwrap().contains(&tc_s) {
                return Ok(dexdo_core::TokenContractSettlementReceipts::default());
            }
            let deal = self.deal(tc)?.expect("a stopped deal is a known deal");
            Ok(dexdo_core::TokenContractSettlementReceipts {
                events: vec![dexdo_core::TokenContractSettlementReceipt {
                    message_id: "test-stop-message".to_string(),
                    created_at: 1,
                    cursor: "test-stop-cursor".to_string(),
                    event: dexdo_core::TokenContractSettlementEvent::StreamStopped {
                        buyer: deal.buyer_note.clone(),
                        to_seller: 1,
                        refund_to_buyer: 2,
                    },
                }],
            })
        }
    }

    #[async_trait::async_trait]
    impl super::DisputeChain for RecordedDealsChain {
        async fn state(
            &self,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::DealChainState>> {
            super::RecoverChain::state(self, tc).await
        }

        async fn buyer_note(
            &self,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::Address>> {
            super::RecoverChain::buyer_note(self, tc).await
        }

        async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> anyhow::Result<Option<[u8; 32]>> {
            super::RecoverChain::buyer_pubkey(self, tc).await
        }

        async fn dispute(
            &self,
            note: &dexdo_core::Address,
            _keys: &dexdo_core::KeyPair,
            tc: &dexdo_core::Address,
        ) -> anyhow::Result<dexdo_core::SettlementActionReceipt> {
            let deal = self
                .deal(tc)?
                .expect("dispute on an inactive TokenContract");
            assert_eq!(
                note.with_workchain(),
                deal.buyer_note,
                "the dispute must be signed by the deal's own buyer note"
            );
            self.disputed.lock().unwrap().push(tc.with_workchain());
            Ok(dexdo_core::SettlementActionReceipt {
                action: dexdo_core::SettlementAction::Dispute,
                event: dexdo_core::SettlementActionEvent::StreamDisputed {
                    buyer: deal.buyer_note.clone(),
                    at: 1,
                },
                ..test_stop_receipt(tc)
            })
        }
    }

    /// The ordinary two-deal pool found live: two notes, one recorded deal each. The later-recorded
    /// deal is the one the chain still holds OPEN, so "the first recorded entry" and "the deal to act on"
    /// are different entries.
    fn two_recorded_deals_pool(dir: &std::path::Path) -> std::path::PathBuf {
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        write_reclaim_pool(
            dir,
            serde_json::json!([
                recorded_row(&note_a, &secret_a, &tc_a, 10),
                recorded_row(&note_b, &secret_b, &tc_b, 20),
            ]),
        )
    }

    fn pool_only_recovery_identity() -> RecoveryIdentityArgs {
        RecoveryIdentityArgs {
            note_key: None,
            note_addr: None,
        }
    }

    fn recorded_row_updated_at(pool_path: &std::path::Path, token_contract: &str) -> u64 {
        // Identify the row by its ACCOUNT ID, not by one spelling of the address: a pool the command
        // rewrote records the canonical `<account_id>::<account_id>` while a pool a refusal
        // left untouched still holds the hand-built `0:<account_id>`, and this helper reads both.
        let account_of = |value: &str| {
            value
                .rsplit_once("::")
                .map(|(_, account)| account)
                .unwrap_or_else(|| value.strip_prefix("0:").unwrap_or(value))
                .to_string()
        };
        let account = account_of(token_contract);
        let pool = crate::cli::commands::load_pool_json(pool_path).unwrap();
        pool["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|note| note["token_contract"].as_str().map(account_of) == Some(account.clone()))
            .unwrap_or_else(|| panic!("pool must still record {token_contract}"))
            ["token_contract_updated_at_unix"]
            .as_u64()
            .expect("a recorded row keeps its recorded time")
    }

    /// primary regression, through `dexdo recover --pool <file>` itself: a pool recording TWO
    /// recoverable deals no longer refuses. The chain places exactly one of them in the state `recover`
    /// acts on, that one is STOPped, and the other recorded deal is not touched at all.
    #[tokio::test]
    async fn recover_acts_on_the_one_recorded_deal_the_chain_places_open() {
        let dir = reclaim_test_dir("recover-two-recorded-deals");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = two_recorded_deals_pool(&dir);
        let chain = RecordedDealsChain::default()
            // recorded first, but the chain says this deal is over
            .with_deal(&tc_a, false, &note_a, &secret_a)
            // recorded second, and still the OPEN deal an orphaned buyer left behind
            .with_deal(&tc_b, true, &note_b, &secret_b);

        super::run_recover_with_chain(
            RecoverArgs {
                identity: pool_only_recovery_identity(),
                token_contract: None,
                market: None,
                pool: Some(pool_path.clone()),
            },
            &chain,
        )
        .await
        .expect("two recorded deals with exactly one OPEN must not refuse");

        assert_eq!(
            chain.stopped(),
            vec![dexdo_core::Address::parse(&tc_b).unwrap().with_workchain()],
            "exactly one STOP, and it is the deal the chain placed OPEN"
        );
        assert_eq!(
            recorded_row_updated_at(&pool_path, &tc_a),
            10,
            "the deal that was not acted on must be left exactly as recorded"
        );
        assert!(
            recorded_row_updated_at(&pool_path, &tc_b) > 20,
            "the acted-on deal's own row is the one that is written back"
        );
    }

    /// The other half of the same rule: when the chain cannot tell the recorded deals apart -- both are
    /// still OPEN and owned by their recorded notes -- `recover` still refuses and moves nothing. This is
    /// the side the change errs on.
    #[tokio::test]
    async fn recover_refuses_when_the_chain_places_two_recorded_deals_open() {
        let dir = reclaim_test_dir("recover-two-open-recorded-deals");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = two_recorded_deals_pool(&dir);
        let chain = RecordedDealsChain::default()
            .with_deal(&tc_a, true, &note_a, &secret_a)
            .with_deal(&tc_b, true, &note_b, &secret_b);

        let error = super::run_recover_with_chain(
            RecoverArgs {
                identity: pool_only_recovery_identity(),
                token_contract: None,
                market: None,
                pool: Some(pool_path.clone()),
            },
            &chain,
        )
        .await
        .expect_err("two OPEN recorded deals cannot be told apart and must refuse")
        .to_string();

        // Specific to the chain-decoded refusal, so reverting the fix cannot satisfy it with the old
        // "pass --note-addr or --token-contract to disambiguate" message.
        assert!(
            error.contains("the chain places 2 of them in the state recover acts on"),
            "{error}"
        );
        // the refusal names each deal's TokenContract canonically, and a TokenContract is a
        // self-DApp account, so its DApp half is its own account id.
        let named = |tc: &str| {
            let account = tc.strip_prefix("0:").expect("fixture is the chain form");
            format!("{account}::{account}")
        };
        assert!(
            error.contains(&named(&tc_a)),
            "the first deal is unnamed: {error}"
        );
        assert!(
            error.contains(&named(&tc_b)),
            "the second deal is unnamed: {error}"
        );
        assert!(!error.contains(&secret_a), "refusal leaked an owner key");
        assert!(!error.contains(&secret_b), "refusal leaked an owner key");
        assert!(chain.stopped().is_empty(), "a refusal must move no money");
        assert_eq!(recorded_row_updated_at(&pool_path, &tc_a), 10);
        assert_eq!(recorded_row_updated_at(&pool_path, &tc_b), 20);
    }

    /// A chain that cannot answer for ONE recorded deal has not ruled that deal out, so the whole
    /// invocation refuses rather than acting on the sibling it happened to read successfully.
    #[tokio::test]
    async fn recover_refuses_when_one_recorded_deal_cannot_be_read() {
        let dir = reclaim_test_dir("recover-unreadable-recorded-deal");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_b, secret_b, tc_b) = note_b();
        let (_, _, tc_a) = note_a();
        let pool_path = two_recorded_deals_pool(&dir);
        let chain = RecordedDealsChain::default()
            .with_unreadable_deal(&tc_a)
            .with_deal(&tc_b, true, &note_b, &secret_b);

        let error = super::run_recover_with_chain(
            RecoverArgs {
                identity: pool_only_recovery_identity(),
                token_contract: None,
                market: None,
                pool: Some(pool_path.clone()),
            },
            &chain,
        )
        .await
        .expect_err("an unreadable recorded deal must refuse the whole invocation")
        .to_string();

        assert!(error.contains("chain read failed"), "{error}");
        assert!(chain.stopped().is_empty(), "a refusal must move no money");
    }

    /// the `dispute` half: the same selection, for the action that also freezes the seller bond and
    /// starts the arbitration clock.
    #[tokio::test]
    async fn dispute_acts_on_the_one_recorded_deal_the_chain_places_open() {
        let dir = reclaim_test_dir("dispute-two-recorded-deals");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = two_recorded_deals_pool(&dir);
        let chain = RecordedDealsChain::default()
            .with_deal(&tc_a, false, &note_a, &secret_a)
            .with_deal(&tc_b, true, &note_b, &secret_b);

        super::run_dispute_with_chain(
            DisputeArgs {
                identity: pool_only_recovery_identity(),
                token_contract: None,
                market: None,
                pool: Some(pool_path.clone()),
            },
            &chain,
        )
        .await
        .expect("two recorded deals with exactly one OPEN must not refuse");

        assert_eq!(
            chain.disputed(),
            vec![dexdo_core::Address::parse(&tc_b).unwrap().with_workchain()],
            "exactly one bond is locked, and it is the deal the chain placed OPEN"
        );
        assert_eq!(
            recorded_row_updated_at(&pool_path, &tc_a),
            10,
            "dispute persists nothing, so both rows are exactly as recorded"
        );
        assert_eq!(recorded_row_updated_at(&pool_path, &tc_b), 20);
    }

    #[tokio::test]
    async fn dispute_refuses_when_the_chain_places_two_recorded_deals_open() {
        let dir = reclaim_test_dir("dispute-two-open-recorded-deals");
        let _cleanup = TempDirCleanup(dir.clone());
        let (note_a, secret_a, tc_a) = note_a();
        let (note_b, secret_b, tc_b) = note_b();
        let pool_path = two_recorded_deals_pool(&dir);
        let chain = RecordedDealsChain::default()
            .with_deal(&tc_a, true, &note_a, &secret_a)
            .with_deal(&tc_b, true, &note_b, &secret_b);

        let error = super::run_dispute_with_chain(
            DisputeArgs {
                identity: pool_only_recovery_identity(),
                token_contract: None,
                market: None,
                pool: Some(pool_path.clone()),
            },
            &chain,
        )
        .await
        .expect_err("two OPEN recorded deals cannot be told apart and must refuse")
        .to_string();

        assert!(
            error.contains("the chain places 2 of them in the state dispute acts on"),
            "{error}"
        );
        // the refusal names each deal's TokenContract canonically, and a TokenContract is a
        // self-DApp account, so its DApp half is its own account id.
        let named = |tc: &str| {
            let account = tc.strip_prefix("0:").expect("fixture is the chain form");
            format!("{account}::{account}")
        };
        assert!(
            error.contains(&named(&tc_a)),
            "the first deal is unnamed: {error}"
        );
        assert!(
            error.contains(&named(&tc_b)),
            "the second deal is unnamed: {error}"
        );
        assert!(
            chain.disputed().is_empty(),
            "a refusal must lock no bond and start no clock"
        );
    }
}
