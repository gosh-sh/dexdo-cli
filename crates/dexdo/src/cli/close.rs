//! Deal-close command handlers (Track C7, move-only).

use crate::cli::args::{CloseArgs, DealRoleArg};
use crate::cli::commands::{
    chain_doctor_preflight_market, close_hint, deal_contracts_path, load_deal_target,
};
use crate::cli::commands::{close_guidance, status_command};
use crate::cli::commands::{
    mock_chain_for_machine, require_close_target_identity, resolve_mock_deal_target, role_arg_str,
};
use crate::cli::deals;
use crate::cli::machine;
use crate::cli::recover::check_reclaimable_state;
use anyhow::{bail, Result};
use dexdo_core::ChainBackend;

async fn submit_then_observe_cleanup<S, SF, O, OF>(submit: S, observe: O) -> Result<()>
where
    S: FnOnce() -> SF,
    SF: std::future::Future<Output = Result<()>>,
    O: FnOnce() -> OF,
    OF: std::future::Future<Output = Result<()>>,
{
    submit().await?;
    observe().await
}

#[allow(clippy::too_many_arguments)]
fn close_response(
    network: &str,
    handle: Option<String>,
    role: &str,
    token_contract: String,
    action: &str,
    submitted: bool,
    terminal: bool,
    reason: Option<&str>,
    state_before: &str,
    state_after: &str,
) -> Result<machine::CloseResponse> {
    Ok(machine::CloseResponse {
        schema: machine::CLOSE_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        handle,
        role: role.to_string(),
        token_contract,
        action: action.to_string(),
        submitted,
        terminal,
        reason: reason.map(str::to_string),
        state_before: state_before.to_string(),
        state_after: state_after.to_string(),
        last_observed_promotion: None,
        tx: None,
    })
}

fn with_last_observed_promotion(
    mut response: machine::CloseResponse,
    observation: Option<crate::cli::deals::LastObservedPromotion>,
) -> machine::CloseResponse {
    response.last_observed_promotion = observation;
    response
}

fn last_observed_promotion_text(
    observation: Option<crate::cli::deals::LastObservedPromotion>,
) -> String {
    match observation {
        Some(observation) => format!(
            "last_observed_promotion=(tokens_final={} tokens_pending={} last_claim_time={})",
            observation.tokens_final, observation.tokens_pending, observation.last_claim_time
        ),
        None => "last_observed_promotion=unavailable".to_string(),
    }
}

fn load_close_deal_record(
    input: &str,
    deals_dir: Option<&std::path::Path>,
    role: deals::DealHandleRole,
    note_addr: &str,
) -> Result<Option<(std::path::PathBuf, deals::DealRecord)>> {
    let dir = deals::resolve_deals_dir(deals_dir)?;
    let Some((path, _handle)) = deals::resolve_deal_ref(input, &dir, Some(role), Some(note_addr))?
    else {
        return Ok(None);
    };
    let record = deals::load_deal_record(&path)?;
    Ok(Some((path, record)))
}

fn confirmed_buyer_stop_response(
    network: &str,
    handle: Option<String>,
    role: &str,
    token_contract: String,
    state_before: &str,
    receipt: Option<&dexdo_core::SettlementActionReceipt>,
) -> Result<machine::CloseResponse> {
    let mut response = close_response(
        network,
        handle,
        role,
        token_contract,
        "streamStop",
        true,
        true,
        None,
        state_before,
        "stopped",
    )?;
    response.tx = receipt.map(serde_json::to_value).transpose()?;
    Ok(response)
}

fn confirmed_buyer_stop_followup_error(
    step: &str,
    receipt: &dexdo_core::SettlementActionReceipt,
    error: anyhow::Error,
) -> anyhow::Error {
    anyhow::anyhow!(
        "buyer STOP is already confirmed on-chain; authoritative receipt={receipt}; \
         ancillary {step} failed after the receipt was rendered: {error:#}"
    )
}

fn apply_confirmed_buyer_stop_marker<T>(
    receipt: &dexdo_core::SettlementActionReceipt,
    marker: impl FnOnce() -> Result<T>,
) -> Result<T> {
    marker().map_err(|error| {
        confirmed_buyer_stop_followup_error("local subscription marker", receipt, error)
    })
}

fn apply_prior_stop_marker<T>(confirmation: &str, marker: impl FnOnce() -> Result<T>) -> Result<T> {
    marker().map_err(|error| {
        anyhow::anyhow!(
            "{confirmation}; local subscription marker failed during idempotent reconciliation: \
             {error:#}"
        )
    })
}

fn confirmed_seller_stop_response(
    network: &str,
    handle: Option<String>,
    token_contract: String,
    state_before: &str,
    receipt: &dexdo_core::SettlementActionReceipt,
) -> Result<machine::CloseResponse> {
    let mut response = close_response(
        network,
        handle,
        "seller",
        token_contract,
        "sellerStop",
        true,
        true,
        None,
        state_before,
        "stopped",
    )?;
    response.tx = Some(serde_json::to_value(receipt)?);
    Ok(response)
}

fn confirmed_seller_stop_text(
    handle: Option<&str>,
    token_contract: &str,
    note: &dexdo_core::Address,
    receipt: &dexdo_core::SettlementActionReceipt,
) -> String {
    let token_contract = dexdo_core::address::display_self_dapp(token_contract);
    let note = dexdo_core::address::display(&note.with_workchain());
    format!(
        "close confirmed role=seller action=sellerStop handle={} token_contract={token_contract} \
         note={note} receipt={receipt}",
        handle.unwrap_or("raw-token-contract")
    )
}

#[async_trait::async_trait]
trait SellerStopChain: Sync {
    async fn seller_pubkey(&self, token_contract: &dexdo_core::Address) -> Result<Option<String>>;

    async fn seller_stop(
        &self,
        token_contract: &dexdo_core::Address,
        seller_keys: &dexdo_core::KeyPair,
    ) -> Result<dexdo_core::SettlementActionReceipt>;
}

#[async_trait::async_trait]
impl SellerStopChain for dexdo_core::RealChainBackend {
    async fn seller_pubkey(&self, token_contract: &dexdo_core::Address) -> Result<Option<String>> {
        Ok(self.token_contract_seller_pubkey(token_contract).await?)
    }

    async fn seller_stop(
        &self,
        token_contract: &dexdo_core::Address,
        seller_keys: &dexdo_core::KeyPair,
    ) -> Result<dexdo_core::SettlementActionReceipt> {
        Ok(dexdo_core::RealChainBackend::seller_stop(self, token_contract, seller_keys).await?)
    }
}

async fn submit_seller_stop(
    chain: &dyn SellerStopChain,
    role: deals::DealHandleRole,
    state: dexdo_core::DealChainState,
    token_contract: &dexdo_core::Address,
    seller_keys: &dexdo_core::KeyPair,
) -> Result<dexdo_core::SettlementActionReceipt> {
    let token_contract_display =
        dexdo_core::address::display_self_dapp(&token_contract.with_workchain());
    if role != deals::DealHandleRole::Seller {
        bail!(
            "close sellerStop requires seller role, got {}; refusing before any money POST",
            role.as_str()
        );
    }
    if state.disputed {
        bail!(
            "close: seller deal {token_contract_display} is disputed; use `dexdo release-dispute` instead of sellerStop"
        );
    }
    if !state.opened {
        bail!(
            "close: sellerStop requires an OPEN deal; TokenContract {token_contract_display} is not OPEN; refusing before any money POST"
        );
    }

    let seller_pubkey = chain.seller_pubkey(token_contract).await?;
    dexdo_core::check_seller_pubkey(
        "close sellerStop",
        seller_pubkey.as_deref(),
        seller_keys.public_hex(),
    )
    .map_err(anyhow::Error::msg)?;

    let receipt = chain.seller_stop(token_contract, seller_keys).await?;
    if receipt.action != dexdo_core::SettlementAction::SellerStop {
        bail!(
            "close sellerStop returned an authoritative receipt for wrong action {}",
            receipt.action
        );
    }
    let receipt_token_contract = dexdo_core::normalize_wallet_address(&receipt.token_contract)
        .map_err(anyhow::Error::msg)?;
    let expected_token_contract =
        dexdo_core::normalize_wallet_address(&token_contract.with_workchain())
            .map_err(anyhow::Error::msg)?;
    if receipt_token_contract != expected_token_contract {
        bail!(
            "close sellerStop returned an authoritative receipt for TokenContract {}, expected {}",
            dexdo_core::address::display_self_dapp(&receipt.token_contract),
            token_contract_display
        );
    }
    if !matches!(
        &receipt.event,
        dexdo_core::SettlementActionEvent::StreamStopped { .. }
    ) {
        bail!(
            "close sellerStop returned an authoritative receipt without StreamStopped: {:?}",
            receipt.event
        );
    }
    Ok(receipt)
}

/// The chain surface an UNSOLD-deal close needs.

/// Same seam, and for the same reason, as [`SellerStopChain`] above: the defect being fixed here is
/// "the door exists on the contract and nothing in the client knocks on it", and the only way to
/// prove a client invokes something is to make the invocation observable offline.
#[async_trait::async_trait]
trait CloseUnsoldDealChain: Sync {
    async fn offer_latch(
        &self,
        token_contract: &dexdo_core::Address,
    ) -> Result<Option<dexdo_core::DealOfferLatch>>;

    async fn note_shell_balance(&self, note: &dexdo_core::Address) -> Result<u128>;

    async fn close_unsold_deal(
        &self,
        token_contract: &dexdo_core::Address,
        seller_keys: &dexdo_core::KeyPair,
    ) -> Result<()>;

    async fn wait_deal_destroyed(&self, token_contract: &dexdo_core::Address) -> Result<()>;
}

#[async_trait::async_trait]
impl CloseUnsoldDealChain for dexdo_core::RealChainBackend {
    async fn offer_latch(
        &self,
        token_contract: &dexdo_core::Address,
    ) -> Result<Option<dexdo_core::DealOfferLatch>> {
        self.token_contract_offer(token_contract).await
    }

    async fn note_shell_balance(&self, note: &dexdo_core::Address) -> Result<u128> {
        self.private_note_shell_balance(note).await
    }

    async fn close_unsold_deal(
        &self,
        token_contract: &dexdo_core::Address,
        seller_keys: &dexdo_core::KeyPair,
    ) -> Result<()> {
        dexdo_core::RealChainBackend::close_unsold_deal(self, token_contract, seller_keys).await?;
        Ok(())
    }

    async fn wait_deal_destroyed(&self, token_contract: &dexdo_core::Address) -> Result<()> {
        Ok(dexdo_core::RealChainBackend::wait_deal_destroyed(self, token_contract).await?)
    }
}

/// What the seller note was worth on either side of the close. Both figures are printed, and the
/// refund is their difference -- never a bond amount read out of a getter.
struct ClosedUnsoldDeal {
    refund: u128,
    balance_before: u128,
    balance_after: u128,
}

/// Everything that must be true before `TokenContract.close()` is worth sending, checked with reads
/// only. Nothing here spends, and every refusal names the state that was found.
async fn refuse_close_unless_unsold(
    chain: &dyn CloseUnsoldDealChain,
    role: deals::DealHandleRole,
    state: &deals::DealStateSummary,
    token_contract: &dexdo_core::Address,
) -> Result<()> {
    let token_contract_display =
        dexdo_core::address::display_self_dapp(&token_contract.with_workchain());
    if role != deals::DealHandleRole::Seller {
        bail!(
            "close: TokenContract.close() is the seller's door (onlyOwnerPubkey(_sellerPubkey)), \
             got role {}; refusing before any money POST",
            role.as_str()
        );
    }
    // `close()` opens with `require(!_funded, ERR_ALREADY_FUNDED)`
    // (`contracts/airegistry/TokenContract.sol:804`), so a deal that ever matched reverts inside it.
    // One getter already answered that question; paying gas to be told the same thing is the part
    // being refused.
    if state.funded || state.opened || state.disputed {
        bail!(
            "refusing to close: deal {token_contract_display} is `{}` (funded={} opened={} disputed={}), \
             which is not an unsold deal. TokenContract.close() requires !_funded \
             (contracts/airegistry/TokenContract.sol:804) and would revert; nothing was sent. Read \
             the deal with `dexdo status` and follow the next-action it names.",
            state.kind.as_str(),
            state.funded,
            state.opened,
            state.disputed
        );
    }
    // WHICH BRANCH WILL `close()` TAKE? Ask the deal's own latch, not a folded book view.

    // `_offerPosted` is set when the TC places its ask (`TokenContract.sol:734`) and cleared in
    // exactly one place, `onSellClosed` (`TokenContract.sol:758`), which is guarded to the canonical
    // InferenceOrderBook (`TokenContract.sol:752-755`). So `offerPosted == false` IS the book's own
    // announcement that the ask left, relayed on chain by the book itself -- and it is the same
    // answer whether the ask was cancelled or expired.

    // Looking instead for the row's absence from a folded book view would be wrong in a way that
    // never recovers: `InferenceOrderExpired` is deliberately NOT terminal to that fold and the row
    // STAYS (`crates/core/src/chain/book_events.rs`), so an expired ask would keep a deal
    // unclosable forever while its bond sat inside.
    let Some(latch) = chain.offer_latch(token_contract).await? else {
        bail!(
            "refusing to close: TokenContract {token_contract_display} did not answer getOffer(), so it is \
             not an active deal contract; nothing was sent"
        );
    };
    if latch.offer_posted {
        bail!(
            "refusing to close: deal {token_contract_display} still has a resting sell offer -- its own \
             getOffer() answers offerPosted=true, so the deal is still on the book and still \
             matchable. On contracts 4.0.35 TokenContract.close() requires !_offerPosted and \
             reverts with ERR_OFFER_LIVE \
             (contracts/airegistry/TokenContract.sol:842-847), so the call would fail on chain and \
             nothing was sent. Take the ask off the book first -- run `dexdo orders cancel` with \
             the order id, this note's --note-addr and the seller --note-key to sign, or \
             `dexdo orders expire` once its deadline has passed -- and re-run this command once \
             getOffer() answers offerPosted=false."
        );
    }
    Ok(())
}

/// Send `TokenContract.close()` and report only what was observed.

/// The deal returns the seller bond to its own stored `_sellerNote` and self-destructs in this one
/// transaction (`contracts/airegistry/TokenContract.sol:816-820`), so the destruct is the proof the
/// close happened and the note's balance on either side of it is the proof of what came back.
async fn submit_close_unsold(
    chain: &dyn CloseUnsoldDealChain,
    token_contract: &dexdo_core::Address,
    note: &dexdo_core::Address,
    seller_keys: &dexdo_core::KeyPair,
) -> Result<ClosedUnsoldDeal> {
    let balance_before = chain.note_shell_balance(note).await?;
    chain.close_unsold_deal(token_contract, seller_keys).await?;
    chain.wait_deal_destroyed(token_contract).await?;
    let balance_after = chain.note_shell_balance(note).await?;
    Ok(ClosedUnsoldDeal {
        // OBSERVED, never derived. `getSellerBond().bondHeld` would have been a figure this client
        // watched nobody move; the difference between two reads of the note is a figure it saw.
        refund: balance_after.saturating_sub(balance_before),
        balance_before,
        balance_after,
    })
}

fn closed_unsold_deal_response_for_network(
    network: &str,
    handle: Option<String>,
    token_contract: String,
    state_before: &str,
    closed: &ClosedUnsoldDeal,
) -> Result<machine::CloseResponse> {
    let mut response = close_response(
        network,
        handle,
        "seller",
        token_contract,
        "close",
        true,
        true,
        None,
        state_before,
        "closed",
    )?;
    response.tx = Some(serde_json::json!({
        "refund": closed.refund.to_string(),
        "balance_before": closed.balance_before.to_string(),
        "balance_after": closed.balance_after.to_string(),
    }));
    Ok(response)
}

#[cfg(test)]
fn closed_unsold_deal_response(
    handle: Option<String>,
    token_contract: String,
    state_before: &str,
    closed: &ClosedUnsoldDeal,
) -> Result<machine::CloseResponse> {
    closed_unsold_deal_response_for_network(
        dexdo_core::params::current_network(),
        handle,
        token_contract,
        state_before,
        closed,
    )
}

fn closed_unsold_deal_text(
    token_contract: &str,
    note: &dexdo_core::Address,
    closed: &ClosedUnsoldDeal,
) -> String {
    let token_contract = dexdo_core::address::display_self_dapp(token_contract);
    let note = dexdo_core::address::display(&note.with_workchain());
    format!(
        "close confirmed role=seller action=close token_contract={token_contract} note={note} \
         refund={} balance_before={} balance_after={}",
        dexdo_core::shell_amount(closed.refund),
        dexdo_core::shell_amount(closed.balance_before),
        dexdo_core::shell_amount(closed.balance_after)
    )
}

async fn run_close_mock(args: CloseArgs) -> Result<()> {
    let target = resolve_mock_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let (role, _note_addr) =
        require_close_target_identity(&args.deal, target.role, target.note_addr.as_deref())?;
    let role_s = role_arg_str(role);
    let handle = target.handle.as_ref().map(|h| h.handle.clone());
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let snapshot = chain.checked_snapshot(&target.token_contract).await?;
    match snapshot {
        None => {
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "noop",
                false,
                true,
                Some("already_closed"),
                "closed",
                "closed",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: TokenContract {} is inactive/closed",
                dexdo_core::address::display_self_dapp(&response.token_contract)
            );
            Ok(())
        }
        Some(snapshot) if snapshot.closed => {
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "noop",
                false,
                true,
                Some("already_stopped"),
                "stopped",
                "stopped",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: {} side already STOPped for {}",
                role_s,
                dexdo_core::address::display_self_dapp(&response.token_contract)
            );
            Ok(())
        }
        Some(snapshot) => {
            let state_before = if snapshot.seller_received > 0 {
                "streaming"
            } else {
                "probe"
            };
            match role {
                DealRoleArg::Buyer => {
                    let note_addr = target.note_addr.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "close: `{}` has no persisted buyer note identity",
                            args.deal
                        )
                    })?;
                    chain
                        .stop_by_buyer_note_addr(&target.token_contract, note_addr)
                        .await?;
                    let response = confirmed_buyer_stop_response(
                        "mock",
                        handle,
                        role_s,
                        target.token_contract,
                        state_before,
                        None,
                    )?;
                    if args.json {
                        return machine::print_json(&response);
                    }
                    println!(
                        "close submitted role=buyer action=streamStop token_contract={}",
                        dexdo_core::address::display_self_dapp(&response.token_contract)
                    );
                    Ok(())
                }
                DealRoleArg::Seller => {
                    let state = chain
                        .deal_state(&target.token_contract)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "close: sellerStop requires an OPEN deal; TokenContract {} has no authoritative mock state",
                                dexdo_core::address::display_self_dapp(&target.token_contract)
                            )
                        })?;
                    if state.disputed {
                        bail!(
                            "close: seller deal {} is disputed; use `dexdo release-dispute` instead of sellerStop",
                            dexdo_core::address::display_self_dapp(&target.token_contract)
                        );
                    }
                    if !state.opened {
                        bail!(
                            "close: sellerStop requires an OPEN deal; TokenContract {} is not OPEN",
                            dexdo_core::address::display_self_dapp(&target.token_contract)
                        );
                    }

                    let settlement = chain.seller_stop(&target.token_contract).await?;
                    let response = close_response(
                        "mock",
                        handle,
                        role_s,
                        target.token_contract,
                        "sellerStop",
                        true,
                        true,
                        None,
                        state_before,
                        "stopped",
                    )?;
                    if args.json {
                        return machine::print_json(&response);
                    }
                    println!(
                        "close confirmed role=seller action=sellerStop token_contract={} mock_settlement={settlement}",
                        dexdo_core::address::display_self_dapp(&response.token_contract)
                    );
                    Ok(())
                }
            }
        }
    }
}

pub(crate) async fn run_close(args: CloseArgs) -> Result<()> {
    if args.mock_chain {
        return run_close_mock(args).await;
    }
    use dexdo_core::{check_recoverable, keypair_ed_pubkey, KeyPair, RealChainBackend};
    let target = load_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let (role, note_addr) =
        require_close_target_identity(&args.deal, target.role, target.note_addr.as_deref())?;
    let close_record =
        load_close_deal_record(&args.deal, args.deals_dir.as_deref(), role, &note_addr)?;
    let close_record_path = close_record.as_ref().map(|(path, _)| path.clone());
    let mut last_observed_promotion = close_record
        .as_ref()
        .and_then(|(_, record)| record.last_observed_promotion);
    let contracts_path = deal_contracts_path(&target)?;
    chain_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    // The handle's own manifest, and nothing that could override it: the flag that used to sit
    // here is gone. A deal is settled against the chain it was made on, which the handle
    // recorded -- letting a later run point it somewhere else was a way to answer about one chain
    // using another's pins.
    let contracts = contracts_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = dexdo_core::address::parse_chain_address(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let tc_display = dexdo_core::address::display_self_dapp(&target.token_contract);
    let note_display = dexdo_core::address::display(&note_addr);
    if role == deals::DealHandleRole::Buyer {
        let note = dexdo_core::address::parse_chain_address(&note_addr)
            .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
        let receipts = chain.token_contract_settlement_receipts(&tc).await?;
        if let Some(receipt) =
            super::recover::exact_prior_stop_receipt(&receipts, &note.with_workchain())?
        {
            let confirmation =
                super::recover::prior_stop_confirmation("close", &tc, &note, &receipt);
            if args.json {
                let mut response = close_response(
                    chain.network(),
                    target.handle.as_ref().map(|h| h.handle.clone()),
                    role.as_str(),
                    target.token_contract.clone(),
                    "noop",
                    false,
                    true,
                    Some("already_terminal_receipt"),
                    "closed",
                    "closed",
                )?;
                response.tx = Some(super::recover::prior_stop_receipt_json(&tc, &receipt));
                machine::print_json(&with_last_observed_promotion(
                    response,
                    last_observed_promotion,
                ))?;
            } else {
                println!(
                    "{confirmation} {}",
                    last_observed_promotion_text(last_observed_promotion)
                );
            }
            apply_prior_stop_marker(&confirmation, || {
                super::buyer::mark_buyer_subscription_terminal(
                    &note.with_workchain(),
                    &tc.with_workchain(),
                )
            })?;
            return Ok(());
        }
    }
    let Some(snapshot) = chain.token_contract_deal_snapshot(&tc).await? else {
        if args.json {
            return machine::print_json(&close_response(
                chain.network(),
                target.handle.as_ref().map(|h| h.handle.clone()),
                role.as_str(),
                target.token_contract,
                "noop",
                false,
                true,
                Some("already_closed"),
                "closed",
                "closed",
            )?);
        }
        println!(
            "close noop: TokenContract {} is inactive/closed",
            tc_display
        );
        return Ok(());
    };
    last_observed_promotion = Some(snapshot.state.into());
    if let Some(path) = close_record_path.as_deref() {
        deals::persist_last_observed_promotion(
            path,
            last_observed_promotion.expect("live snapshot supplies an observation"),
        )?;
    }
    let s = deals::summarize_deal_snapshot(&snapshot);
    match role {
        deals::DealHandleRole::Seller => {
            if s.disputed {
                bail!(
                    "close: seller deal {} is disputed. Next: {}.",
                    tc_display,
                    crate::cli::support::release_dispute_guidance(&target.token_contract)
                );
            }
            if s.opened {
                // The flag where it was passed, the pool entry for this note where it was not:
                // `close` carries its key as its own argument, and the rule is the shared one.
                let secret = crate::cli::support::note_owner_secret_for(
                    args.note_key.as_deref(),
                    &note_addr,
                    None,
                    "close seller",
                    "the key that signs sellerStop",
                )?;
                let keys = KeyPair::from_secret_hex(secret.trim())
                    .map_err(|e| anyhow::anyhow!("note owner key (SDK secret hex): {e:?}"))?;
                let note = dexdo_core::address::parse_chain_address(&note_addr)
                    .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
                let receipt = submit_seller_stop(&chain, role, snapshot.state, &tc, &keys).await?;
                let handle = target.handle.as_ref().map(|h| h.handle.clone());
                if args.json {
                    return machine::print_json(&with_last_observed_promotion(
                        confirmed_seller_stop_response(
                            chain.network(),
                            handle,
                            target.token_contract.clone(),
                            s.kind.as_str(),
                            &receipt,
                        )?,
                        last_observed_promotion,
                    ));
                }
                println!(
                    "{} {}",
                    confirmed_seller_stop_text(
                        handle.as_deref(),
                        &target.token_contract,
                        &note,
                        &receipt,
                    ),
                    last_observed_promotion_text(last_observed_promotion)
                );
                return Ok(());
            }
            // an UNSOLD deal -- one that never matched, so `_funded` was never set and
            // `_opened` never followed -- is wound down by `TokenContract.close()`, not `destroy()`.
            // `destroy()` wants a STOPped deal, and stopping is something only a deal that started
            // can do, so this arm used to fall straight through to `no_destroy_yet` and leave the
            // seller's bond inside a contract nothing in this client could close. The refusals come
            // first and read only; `--note-key` is not even asked for until the deal is closeable.
            if !s.funded {
                let note = dexdo_core::address::parse_chain_address(&note_addr)
                    .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
                refuse_close_unless_unsold(&chain, role, &s, &tc).await?;
                let secret = crate::cli::support::note_owner_secret_for(
                    args.note_key.as_deref(),
                    &note_addr,
                    None,
                    "close seller",
                    "the key that signs this deal's owner method",
                )?;
                let keys = KeyPair::from_secret_hex(secret.trim())
                    .map_err(|e| anyhow::anyhow!("note owner key (SDK secret hex): {e:?}"))?;
                let closed = submit_close_unsold(&chain, &tc, &note, &keys).await?;
                let handle = target.handle.as_ref().map(|h| h.handle.clone());
                if args.json {
                    return machine::print_json(&closed_unsold_deal_response_for_network(
                        chain.network(),
                        handle,
                        target.token_contract.clone(),
                        s.kind.as_str(),
                        &closed,
                    )?);
                }
                println!(
                    "{}",
                    closed_unsold_deal_text(&target.token_contract, &note, &closed)
                );
                return Ok(());
            }
            if s.kind != deals::DealStateKind::Stopped {
                bail!("{}", close_hint(&target, &s, args.deals_dir.as_deref()));
            }
            let secret = crate::cli::support::note_owner_secret_for(
                args.note_key.as_deref(),
                &note_addr,
                None,
                "close seller",
                "the key that signs destroy",
            )?;
            let keys = KeyPair::from_secret_hex(secret.trim())
                .map_err(|e| anyhow::anyhow!("note owner key (SDK secret hex): {e:?}"))?;
            let note = dexdo_core::address::parse_chain_address(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            chain.destroy_token_contract(&tc, &note, &keys).await?;
            if args.json {
                return machine::print_json(&close_response(
                    chain.network(),
                    target.handle.as_ref().map(|h| h.handle.clone()),
                    role.as_str(),
                    target.token_contract.clone(),
                    "destroy",
                    true,
                    true,
                    None,
                    s.kind.as_str(),
                    "closed",
                )?);
            }
            println!(
                "close submitted role=seller action=destroy token_contract={} note={}",
                tc_display, note_display
            );
        }
        deals::DealHandleRole::Buyer => {
            if s.disputed {
                super::buyer::mark_buyer_subscription_terminal(&note_addr, &tc.with_workchain())?;
                bail!(
                    "close: buyer deal {} is disputed; wait for seller release/arbitration (), then re-run status.",
                    target.token_contract
                );
            }
            if s.kind == deals::DealStateKind::Stopped {
                super::buyer::mark_buyer_subscription_terminal(&note_addr, &tc.with_workchain())?;
                if args.json {
                    return machine::print_json(&close_response(
                        chain.network(),
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "noop",
                        false,
                        true,
                        Some("already_stopped"),
                        "stopped",
                        "stopped",
                    )?);
                }
                println!(
                    "close noop: buyer side already STOPped for {}. Next: the seller {}.",
                    tc_display,
                    close_guidance(
                        &target.token_contract,
                        Some("seller"),
                        "seller",
                        args.deals_dir.as_deref()
                    )
                );
                return Ok(());
            }
            let secret = crate::cli::support::note_owner_secret_for(
                args.note_key.as_deref(),
                &note_addr,
                None,
                "close buyer",
                "the key that signs the note's owner method",
            )?;
            let keys = KeyPair::from_secret_hex(secret.trim())
                .map_err(|e| anyhow::anyhow!("note owner key (SDK secret hex): {e:?}"))?;
            let note = dexdo_core::address::parse_chain_address(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let buyer_note = chain.token_contract_buyer_note(&tc).await?;
            let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
            let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
            let note_ed = keypair_ed_pubkey(&keys)?;
            if s.opened {
                check_recoverable(
                    s.opened,
                    s.disputed,
                    buyer_note_s.as_deref(),
                    &note.with_workchain(),
                    buyer_pubkey.as_ref(),
                    &note_ed,
                )
                .map_err(anyhow::Error::msg)?;
                let settlement = chain.explicit_buyer_stop(&note, &keys, &tc).await?;
                let receipt = match settlement {
                    dexdo_core::Settlement::AuthoritativeReceipt(receipt) => *receipt,
                    projected => {
                        bail!(
                            "close buyer STOP returned non-authoritative settlement projection: {projected:?}"
                        )
                    }
                };
                if args.json {
                    machine::print_json(&with_last_observed_promotion(
                        confirmed_buyer_stop_response(
                            chain.network(),
                            target.handle.as_ref().map(|h| h.handle.clone()),
                            role.as_str(),
                            target.token_contract.clone(),
                            s.kind.as_str(),
                            Some(&receipt),
                        )?,
                        last_observed_promotion,
                    ))?;
                } else {
                    println!(
                        "close confirmed role=buyer action=streamStop token_contract={} note={} \
                         receipt={receipt} {}",
                        tc_display,
                        note_display,
                        last_observed_promotion_text(last_observed_promotion),
                    );
                }

                apply_confirmed_buyer_stop_marker(&receipt, || {
                    super::buyer::mark_buyer_subscription_terminal(
                        &note.with_workchain(),
                        &tc.with_workchain(),
                    )
                })?;
                return Ok(());
            }
            if s.funded && !s.probe_accepted {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
                    .as_secs();
                check_reclaimable_state(
                    snapshot.state,
                    buyer_note_s.as_deref(),
                    &note.with_workchain(),
                    buyer_pubkey.as_ref(),
                    &note_ed,
                    now,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{e}. Next, after MATCH_OPEN_TIMEOUT, {}. To inspect it first: `{}`.",
                        close_guidance(
                            &args.deal,
                            target.handle.is_none().then_some("buyer"),
                            "buyer",
                            args.deals_dir.as_deref()
                        ),
                        status_command(&args.deal, args.deals_dir.as_deref())
                    )
                })?;
                submit_then_observe_cleanup(
                    || async {
                        chain.stream_cleanup(&note, &keys, &tc).await?;
                        Ok(())
                    },
                    || async { chain.wait_cleanup_unopened(&tc).await.map_err(Into::into) },
                )
                .await?;
                super::buyer::mark_buyer_subscription_terminal(
                    &note.with_workchain(),
                    &tc.with_workchain(),
                )?;
                if args.json {
                    return machine::print_json(&close_response(
                        chain.network(),
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "streamCleanup",
                        true,
                        true,
                        None,
                        s.kind.as_str(),
                        "closed",
                    )?);
                }
                println!(
                    "close confirmed role=buyer action=streamCleanup token_contract={} note={}",
                    tc_display, note_display
                );
                return Ok(());
            }
            bail!("{}", close_hint(&target, &s, args.deals_dir.as_deref()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn seller_stop_receipt(token_contract: &str) -> dexdo_core::SettlementActionReceipt {
        dexdo_core::SettlementActionReceipt {
            token_contract: token_contract.to_string(),
            action: dexdo_core::SettlementAction::SellerStop,
            message_id: "seller-stop-message".to_string(),
            created_at: 43,
            event: dexdo_core::SettlementActionEvent::StreamStopped {
                buyer: format!("0:{}", "44".repeat(32)),
                to_seller: 7u128.into(),
                refund_to_buyer: 11u128.into(),
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

    fn deal_state(opened: bool, disputed: bool) -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: true,
            opened,
            probe_accepted: opened,
            disputed,
            deposit: 12,
            finalized_owed: 0,
            tokens_final: 3,
            tokens_pending: 5,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 2,
            last_claim_time: 4,
            dispute_time: if disputed { 5 } else { 0 },
        }
    }

    struct FakeSellerStopChain {
        seller_pubkey: Option<String>,
        receipt: dexdo_core::SettlementActionReceipt,
        pubkey_reads: std::sync::atomic::AtomicUsize,
        submits: std::sync::atomic::AtomicUsize,
    }

    impl FakeSellerStopChain {
        fn new(
            seller_pubkey: Option<String>,
            receipt: dexdo_core::SettlementActionReceipt,
        ) -> Self {
            Self {
                seller_pubkey,
                receipt,
                pubkey_reads: std::sync::atomic::AtomicUsize::new(0),
                submits: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl super::SellerStopChain for FakeSellerStopChain {
        async fn seller_pubkey(
            &self,
            _token_contract: &dexdo_core::Address,
        ) -> anyhow::Result<Option<String>> {
            self.pubkey_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.seller_pubkey.clone())
        }

        async fn seller_stop(
            &self,
            _token_contract: &dexdo_core::Address,
            _seller_keys: &dexdo_core::KeyPair,
        ) -> anyhow::Result<dexdo_core::SettlementActionReceipt> {
            self.submits
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.receipt.clone())
        }
    }

    #[test]
    fn confirmed_buyer_stop_json_contains_the_authoritative_receipt() {
        let receipt = dexdo_core::SettlementActionReceipt {
            token_contract: "0:tc".to_string(),
            action: dexdo_core::SettlementAction::BuyerStop,
            message_id: "stop-message".to_string(),
            created_at: 42,
            event: dexdo_core::SettlementActionEvent::StreamStopped {
                buyer: format!("0:{}", "44".repeat(32)),
                to_seller: u128::MAX.into(),
                refund_to_buyer: (u64::MAX as u128 + 1).into(),
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
        };
        let response = super::confirmed_buyer_stop_response(
            "net-a",
            Some("buyer-1".to_string()),
            "buyer",
            "0:tc".to_string(),
            "streaming",
            Some(&receipt),
        )
        .unwrap();
        assert_eq!(response.action, "streamStop");
        assert!(response.submitted);
        assert!(response.terminal);
        assert_eq!(response.state_before, "streaming");
        assert_eq!(response.state_after, "stopped");
        let tx = response.tx.expect("authoritative receipt in tx");
        assert_eq!(tx["action"], serde_json::json!("buyer_stop"));
        assert_eq!(tx["message_id"], serde_json::json!("stop-message"));
        assert_eq!(tx["created_at"], serde_json::json!(42));
        assert_eq!(tx["toSeller"], serde_json::json!(u128::MAX.to_string()));
        assert_eq!(
            tx["refundToBuyer"],
            serde_json::json!((u64::MAX as u128 + 1).to_string())
        );
        assert_eq!(tx["post_state"]["tokensFinal"], serde_json::json!("3"));
    }

    #[tokio::test]
    async fn seller_stop_rejects_wrong_role_key_and_non_open_state_before_submit() {
        use std::sync::atomic::Ordering;

        let token_contract =
            dexdo_core::Address::parse(&format!("0:{}", "33".repeat(32))).expect("token contract");
        let keys = dexdo_core::KeyPair::generate();
        let receipt = seller_stop_receipt(&token_contract.with_workchain());

        let right_key_chain =
            FakeSellerStopChain::new(Some(keys.public_hex().to_string()), receipt.clone());
        let wrong_role = super::submit_seller_stop(
            &right_key_chain,
            crate::cli::deals::DealHandleRole::Buyer,
            deal_state(true, false),
            &token_contract,
            &keys,
        )
        .await
        .expect_err("buyer role must never reach sellerStop");
        assert!(wrong_role.to_string().contains("requires seller role"));
        assert_eq!(right_key_chain.pubkey_reads.load(Ordering::SeqCst), 0);
        assert_eq!(right_key_chain.submits.load(Ordering::SeqCst), 0);

        for (state, expected) in [
            (deal_state(false, false), "requires an OPEN deal"),
            (deal_state(true, true), "is disputed"),
        ] {
            let error = super::submit_seller_stop(
                &right_key_chain,
                crate::cli::deals::DealHandleRole::Seller,
                state,
                &token_contract,
                &keys,
            )
            .await
            .expect_err("invalid sellerStop state must fail before submit");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        assert_eq!(right_key_chain.pubkey_reads.load(Ordering::SeqCst), 0);
        assert_eq!(right_key_chain.submits.load(Ordering::SeqCst), 0);

        let wrong_key_chain = FakeSellerStopChain::new(
            Some(dexdo_core::KeyPair::generate().public_hex().to_string()),
            receipt,
        );
        let wrong_key = super::submit_seller_stop(
            &wrong_key_chain,
            crate::cli::deals::DealHandleRole::Seller,
            deal_state(true, false),
            &token_contract,
            &keys,
        )
        .await
        .expect_err("wrong seller key must fail before submit");
        assert!(
            wrong_key
                .to_string()
                .contains("is not the deal's seller key"),
            "{wrong_key:#}"
        );
        assert_eq!(wrong_key_chain.pubkey_reads.load(Ordering::SeqCst), 1);
        assert_eq!(wrong_key_chain.submits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn seller_close_submits_exact_selector_and_renders_authoritative_receipt() {
        use std::sync::atomic::Ordering;

        let token_contract =
            dexdo_core::Address::parse(&format!("0:{}", "33".repeat(32))).expect("token contract");
        let seller_note =
            dexdo_core::Address::parse(&format!("0:{}", "55".repeat(32))).expect("seller note");
        let keys = dexdo_core::KeyPair::generate();
        let chain = FakeSellerStopChain::new(
            Some(keys.public_hex().to_string()),
            seller_stop_receipt(&token_contract.with_workchain()),
        );
        let receipt = super::submit_seller_stop(
            &chain,
            crate::cli::deals::DealHandleRole::Seller,
            deal_state(true, false),
            &token_contract,
            &keys,
        )
        .await
        .expect("sellerStop");
        assert_eq!(chain.pubkey_reads.load(Ordering::SeqCst), 1);
        assert_eq!(chain.submits.load(Ordering::SeqCst), 1);
        assert_eq!(receipt.action, dexdo_core::SettlementAction::SellerStop);

        let response = super::confirmed_seller_stop_response(
            "net-a",
            Some("deal-seller-1".to_string()),
            token_contract.with_workchain(),
            "streaming",
            &receipt,
        )
        .expect("JSON response");
        assert_eq!(response.handle.as_deref(), Some("deal-seller-1"));
        assert_eq!(response.role, "seller");
        assert_eq!(response.action, "sellerStop");
        assert!(response.submitted);
        assert!(response.terminal);
        assert_eq!(response.state_before, "streaming");
        assert_eq!(response.state_after, "stopped");
        let tx = response.tx.expect("authoritative receipt in tx");
        assert_eq!(tx["action"], serde_json::json!("seller_stop"));
        assert_eq!(tx["message_id"], serde_json::json!("seller-stop-message"));
        assert_eq!(tx["event_kind"], serde_json::json!("stream_stopped"));
        assert_eq!(tx["toSeller"], serde_json::json!("7"));
        assert_eq!(tx["refundToBuyer"], serde_json::json!("11"));

        let text = super::confirmed_seller_stop_text(
            Some("deal-seller-1"),
            &token_contract.with_workchain(),
            &seller_note,
            &receipt,
        );
        assert!(text.contains("role=seller action=sellerStop"), "{text}");
        assert!(text.contains("handle=deal-seller-1"), "{text}");
        // the human confirmation names a per-deal TokenContract canonically, and a
        // TokenContract is a self-DApp account, so its DApp half is its own account id.
        let tc_account = token_contract
            .with_workchain()
            .strip_prefix("0:")
            .expect("the fixture TokenContract is in the chain form")
            .to_string();
        assert!(
            text.contains(&format!("token_contract={tc_account}::{tc_account}")),
            "{text}"
        );
        assert!(text.contains("action=seller_stop"), "{text}");
        assert!(text.contains("event_kind=stream_stopped"), "{text}");
        assert!(text.contains("toSeller=7 refundToBuyer=11"), "{text}");
    }

    // ----: closing an UNSOLD deal -------------------------------------------------------

    /// An unsold deal exactly as the chain reports one: nothing ever funded it, so nothing ever
    /// opened it, and `is_stopped()` is false because stopping is something only a started deal can
    /// do. That combination is why this shape used to reach `no_destroy_yet`.
    fn unsold_deal_summary() -> crate::cli::deals::DealStateSummary {
        crate::cli::deals::summarize_deal_snapshot(&unsold_snapshot(false, false, false))
    }

    fn unsold_snapshot(
        funded: bool,
        opened: bool,
        disputed: bool,
    ) -> dexdo_core::DealChainSnapshot {
        dexdo_core::DealChainSnapshot {
            account_code_hash: "code".to_string(),
            account_boc_hash: "boc".to_string(),
            state: dexdo_core::DealChainState {
                funded,
                opened,
                probe_accepted: false,
                disputed,
                deposit: if funded { 12 } else { 0 },
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                probe_tick: 0,
                funded_time: funded.then_some(1),
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: if disputed { 5 } else { 0 },
            },
            subscription: dexdo_core::DealSubscription {
                deal_flags: 0,
                sub_weeks: 0,
                week_index: 0,
                tokens_per_week: 0,
                funded_tokens: 0,
                tokens_paid: 0,
                period_start: 0,
                week_base_tokens: 0,
            },
            seller_bond: dexdo_core::DealSellerBond {
                bond_funded: true,
                bond_held: 6_000_000_000,
                bond_required: 6_000_000_000,
            },
            buyer_bond: dexdo_core::DealBuyerBond {
                bond_held: 0,
                bond_required: 0,
            },
        }
    }

    /// Records WHAT the close did and IN WHICH ORDER, because "the call exists and nothing invokes
    /// it" is exactly the defect reports: a counter that stays at zero is the whole proof.
    struct FakeCloseUnsoldChain {
        latch: Option<dexdo_core::DealOfferLatch>,
        balances: std::sync::Mutex<std::collections::VecDeque<u128>>,
        log: std::sync::Mutex<Vec<String>>,
    }

    impl FakeCloseUnsoldChain {
        fn new(latch: Option<dexdo_core::DealOfferLatch>, balances: &[u128]) -> Self {
            Self {
                latch,
                balances: std::sync::Mutex::new(balances.iter().copied().collect()),
                log: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn record(&self, what: &str) {
            self.log.lock().expect("log").push(what.to_string());
        }

        fn calls(&self) -> Vec<String> {
            self.log.lock().expect("log").clone()
        }
    }

    #[async_trait::async_trait]
    impl super::CloseUnsoldDealChain for FakeCloseUnsoldChain {
        async fn offer_latch(
            &self,
            _token_contract: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::DealOfferLatch>> {
            self.record("offer_latch");
            Ok(self.latch)
        }

        async fn note_shell_balance(&self, _note: &dexdo_core::Address) -> anyhow::Result<u128> {
            self.record("note_shell_balance");
            Ok(self
                .balances
                .lock()
                .expect("balances")
                .pop_front()
                .expect("a scripted note balance for every read"))
        }

        async fn close_unsold_deal(
            &self,
            _token_contract: &dexdo_core::Address,
            _seller_keys: &dexdo_core::KeyPair,
        ) -> anyhow::Result<()> {
            self.record("close_unsold_deal");
            Ok(())
        }

        async fn wait_deal_destroyed(
            &self,
            _token_contract: &dexdo_core::Address,
        ) -> anyhow::Result<()> {
            self.record("wait_deal_destroyed");
            Ok(())
        }
    }

    /// dispatch proof. `TokenContract.close()` is reached, exactly once, and only after the
    /// deal has been read; the destruct is confirmed before any figure is reported; and the refund
    /// is the difference between two observed note balances rather than the 6 SHELL bond the
    /// snapshot happens to declare.
    #[tokio::test]
    async fn close_of_an_unsold_deal_sends_token_contract_close_and_reports_an_observed_refund() {
        let token_contract =
            dexdo_core::Address::parse(&format!("0:{}", "33".repeat(32))).expect("token contract");
        let note =
            dexdo_core::Address::parse(&format!("0:{}", "55".repeat(32))).expect("seller note");
        let keys = dexdo_core::KeyPair::generate();
        let summary = unsold_deal_summary();
        assert_eq!(summary.kind.as_str(), "placed");

        let chain = FakeCloseUnsoldChain::new(
            Some(dexdo_core::DealOfferLatch {
                offer_posted: false,
            }),
            &[1_000, 7_000],
        );
        super::refuse_close_unless_unsold(
            &chain,
            crate::cli::deals::DealHandleRole::Seller,
            &summary,
            &token_contract,
        )
        .await
        .expect("an unsold deal with no resting ask is closeable");
        let closed = super::submit_close_unsold(&chain, &token_contract, &note, &keys)
            .await
            .expect("close");

        assert_eq!(
            chain.calls(),
            vec![
                "offer_latch",
                "note_shell_balance",
                "close_unsold_deal",
                "wait_deal_destroyed",
                "note_shell_balance",
            ],
            "close() must be sent once, after the deal is read, and the destruct confirmed before \
             any figure is reported"
        );
        assert_eq!(closed.balance_before, 1_000);
        assert_eq!(closed.balance_after, 7_000);
        assert_eq!(
            closed.refund, 6_000,
            "the refund is observed as balance_after - balance_before"
        );
        assert_ne!(
            closed.refund,
            unsold_snapshot(false, false, false).seller_bond.bond_held,
            "a refund must never be the recorded escrow/bond figure"
        );

        let text = super::closed_unsold_deal_text(&token_contract.with_workchain(), &note, &closed);
        // The line states money in SHELL, as every other answer this client gives does.
        for fact in [
            "role=seller action=close",
            "refund=0.000006",
            "balance_before=0.000001",
            "balance_after=0.000007",
        ] {
            assert!(text.contains(fact), "missing {fact:?} in {text}");
        }

        let response = super::closed_unsold_deal_response(
            Some("deal-seller-1".to_string()),
            token_contract.with_workchain(),
            summary.kind.as_str(),
            &closed,
        )
        .expect("JSON response");
        assert_eq!(response.action, "close");
        assert!(response.submitted);
        assert!(response.terminal);
        assert_eq!(response.state_before, "placed");
        assert_eq!(response.state_after, "closed");
        let tx = response.tx.expect("observed balances in tx");
        assert_eq!(tx["refund"], serde_json::json!("6000"));
        assert_eq!(tx["balance_before"], serde_json::json!("1000"));
        assert_eq!(tx["balance_after"], serde_json::json!("7000"));
    }

    /// Every state that cannot take the immediate-destruct branch is refused BEFORE anything is
    /// sent, and every refusal names the state it found. The resting-ask case is the one that
    /// matters most: `close()` succeeds there and does not close anything
    /// (`contracts/airegistry/TokenContract.sol:805-810`), so a client that sent it would report a
    /// close that never happened.
    #[tokio::test]
    async fn close_of_an_unsold_deal_refuses_before_spending_and_names_the_state() {
        let token_contract =
            dexdo_core::Address::parse(&format!("0:{}", "33".repeat(32))).expect("token contract");
        let offerless = || {
            Some(dexdo_core::DealOfferLatch {
                offer_posted: false,
            })
        };

        // A buyer never reaches an `onlyOwnerPubkey(_sellerPubkey)` door, and finds out for free.
        let chain = FakeCloseUnsoldChain::new(offerless(), &[]);
        let wrong_role = super::refuse_close_unless_unsold(
            &chain,
            crate::cli::deals::DealHandleRole::Buyer,
            &unsold_deal_summary(),
            &token_contract,
        )
        .await
        .expect_err("buyer role must never reach TokenContract.close()");
        assert!(
            wrong_role.to_string().contains("seller's door"),
            "{wrong_role:#}"
        );
        assert!(chain.calls().is_empty(), "{:?}", chain.calls());

        // A deal that ever matched reverts inside `require(!_funded,...)`; one getter said so.
        for (funded, opened, disputed, expected_state) in [
            (true, false, false, "funded-but-never-opened"),
            (true, true, false, "probe"),
            (true, false, true, "disputed"),
        ] {
            let chain = FakeCloseUnsoldChain::new(offerless(), &[]);
            let summary = crate::cli::deals::summarize_deal_snapshot(&unsold_snapshot(
                funded, opened, disputed,
            ));
            assert_eq!(summary.kind.as_str(), expected_state);
            let error = super::refuse_close_unless_unsold(
                &chain,
                crate::cli::deals::DealHandleRole::Seller,
                &summary,
                &token_contract,
            )
            .await
            .expect_err("a deal that matched is not an unsold deal");
            let error = error.to_string();
            assert!(error.contains(expected_state), "state not named: {error}");
            assert!(error.contains("nothing was sent"), "{error}");
            assert!(chain.calls().is_empty(), "{:?}", chain.calls());
        }

        // The trap branch: `close()` here SUCCEEDS and leaves the deal alive on the book.
        let chain =
            FakeCloseUnsoldChain::new(Some(dexdo_core::DealOfferLatch { offer_posted: true }), &[]);
        let resting = super::refuse_close_unless_unsold(
            &chain,
            crate::cli::deals::DealHandleRole::Seller,
            &unsold_deal_summary(),
            &token_contract,
        )
        .await
        .expect_err("a deal whose ask still rests must not be reported as closed")
        .to_string();
        for fact in [
            "offerPosted=true",
            "still matchable",
            "nothing was sent",
            "dexdo orders cancel",
            "dexdo orders expire",
        ] {
            assert!(resting.contains(fact), "missing {fact:?} in {resting}");
        }
        assert_eq!(chain.calls(), vec!["offer_latch"]);

        // A contract that no longer answers `getOffer()` is not a deal to close.
        let chain = FakeCloseUnsoldChain::new(None, &[]);
        let gone = super::refuse_close_unless_unsold(
            &chain,
            crate::cli::deals::DealHandleRole::Seller,
            &unsold_deal_summary(),
            &token_contract,
        )
        .await
        .expect_err("an inactive contract must not be sent a close")
        .to_string();
        assert!(gone.contains("did not answer getOffer()"), "{gone}");
        assert_eq!(chain.calls(), vec!["offer_latch"]);
    }

    /// The seller arm of `run_close` really routes an unsold deal into the new door, in the right
    /// order, and the door really encodes `TokenContract.close()`. Pinned at source level because
    /// the reported defect was a wiring absence, not a logic error: every unit below passed before
    /// this issue existed and the operator still had no way to close the deal.
    #[test]
    fn production_seller_close_dispatches_an_unsold_deal_to_token_contract_close() {
        let source = include_str!("close.rs");
        let seller = source
            .find("deals::DealHandleRole::Seller =>")
            .expect("seller close branch");
        let buyer = source[seller..]
            .find("deals::DealHandleRole::Buyer =>")
            .map(|offset| seller + offset)
            .expect("buyer close branch");
        let body = &source[seller..buyer];

        let unsold = body
            .find("if !s.funded {")
            .expect("the unsold-deal arm must exist in the seller branch");
        let stopped = body
            .find("if s.kind != deals::DealStateKind::Stopped {")
            .expect("the destroy arm still exists");
        assert!(
            unsold < stopped,
            "an unsold deal must be routed to close() before the destroy hint can claim it"
        );

        let refuse = body[unsold..]
            .find("refuse_close_unless_unsold(&chain, role, &s, &tc).await?")
            .expect("read-only refusals must run in the unsold arm");
        // Keyed on the request for the key, not on the words it used to refuse with: the key comes
        // from `--note-key` or from the pool entry now, and the rule this pins is the ORDER --
        // read-only refusals first, the key after them, the spend last.
        let key = body[unsold..]
            .find("note_owner_secret_for(")
            .expect("the unsold arm must ask for the signing key");
        let submit = body[unsold..]
            .find("submit_close_unsold(&chain, &tc, &note, &keys).await?")
            .expect("the unsold arm must submit TokenContract.close()");
        assert!(
            refuse < key && key < submit,
            "refuse before asking for a key, and ask for a key before spending"
        );

        // `code_of`, not a slice ending at the next sibling. `closed_unsold_deal_response` is a
        // NEIGHBOUR: rename it, move it, or put another function between, and `unwrap_or(source
        // .len())` makes "the body" the whole rest of the file in silence -- where the three calls
        // below are found whether or not `submit_close_unsold` makes them. That is the defect this
        // branch exists to remove, and this guard was the last one still carrying it.
        let door = crate::cli::source_probe::code_of(source, "async fn submit_close_unsold(");
        let door = door.as_str();
        assert!(
            door.contains("chain.close_unsold_deal(token_contract, seller_keys).await?"),
            "the submitter must call the TokenContract.close() encoder"
        );
        assert!(
            door.contains("chain.wait_deal_destroyed(token_contract).await?"),
            "a close is only closed once the deal is gone"
        );
        assert!(
            door.contains("balance_after.saturating_sub(balance_before)"),
            "the refund must be observed, not derived from a bond getter"
        );

        // The client-side encoder really addresses `close()` and passes it nothing: Task O removed
        // the caller-named payee, and a stale extra argument would address a method the deployed
        // contract does not have rather than fail a guard.
        let core = include_str!("../../../core/src/chain/client.rs");
        let encoder = core
            .find("pub async fn close_unsold_deal(")
            .expect("the core TokenContract.close() encoder");
        let encoder_end = core[encoder..]
            .find("\n /// The seller **concedes the dispute**")
            .map(|offset| encoder + offset)
            .unwrap_or(core.len());
        let encoder = &core[encoder..encoder_end];
        assert!(
            encoder.contains("TOKENCONTRACT_ABI, \"close\", json!({}), seller_keys"),
            "close() takes no inputs on 4.0.34"
        );
        assert!(
            encoder.contains("check_seller_pubkey("),
            "close() is onlyOwnerPubkey(_sellerPubkey); a wrong key must fail before the post"
        );
    }

    #[test]
    fn production_seller_close_routes_through_existing_seller_stop_selector() {
        let source = include_str!("close.rs");
        let seller = source
            .find("deals::DealHandleRole::Seller =>")
            .expect("seller close branch");
        let buyer = source[seller..]
            .find("deals::DealHandleRole::Buyer =>")
            .map(|offset| seller + offset)
            .expect("buyer close branch");
        let body = &source[seller..buyer];
        assert!(
            body.contains("submit_seller_stop(&chain, role, snapshot.state, &tc, &keys).await?")
        );
        assert!(!body.contains("seller cannot destroy opened deal"));
    }

    #[test]
    fn destroyed_after_stop_still_attempts_the_terminal_marker_and_preserves_receipt_on_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let receipt = dexdo_core::SettlementActionReceipt {
            token_contract: "0:tc".to_string(),
            action: dexdo_core::SettlementAction::BuyerStop,
            message_id: "landed-stop-message".to_string(),
            created_at: 77,
            event: dexdo_core::SettlementActionEvent::ProbeBurned {
                buyer: format!("0:{}", "55".repeat(32)),
                burned_probe: 11u128.into(),
                burned_bond: 12u128.into(),
                refund_to_buyer: 22u128.into(),
            },
            pre_bonds: dexdo_core::SettlementActionBondState {
                seller_bond_held: 2u128.into(),
                seller_bond_required: 2u128.into(),
                buyer_bond_held: 0u128.into(),
                buyer_bond_required: 0u128.into(),
            },
            post_state: None,
        };
        let marker_calls = AtomicUsize::new(0);
        let error = super::apply_confirmed_buyer_stop_marker(&receipt, || {
            marker_calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(anyhow::anyhow!("durable marker write failed"))
        })
        .expect_err("destroyed TC must not suppress the receipt-driven marker")
        .to_string();
        assert_eq!(marker_calls.load(Ordering::SeqCst), 1);
        for fact in [
            "already confirmed on-chain",
            "action=buyer_stop",
            "message_id=landed-stop-message",
            "created_at=77",
            "burnedProbe=11",
            "burnedBond=12",
            "refundToBuyer=22",
            "local subscription marker",
            "durable marker write failed",
        ] {
            assert!(error.contains(fact), "missing {fact:?} in {error}");
        }

        let source = include_str!("close.rs");
        let stop = source
            .find(".explicit_buyer_stop(&note, &keys, &tc)")
            .expect("buyer STOP submission");
        let tail = &source[stop..];
        let render = tail
            .find("confirmed_buyer_stop_response(")
            .expect("authoritative receipt renderer");
        let marker = tail
            .find("apply_confirmed_buyer_stop_marker(")
            .expect("receipt-driven terminal marker");
        assert!(
            render < marker,
            "the authoritative receipt must render before the fallible local marker"
        );
        assert!(
            !tail[..marker].contains(".token_contract_deal_snapshot(&tc)"),
            "no redundant post-receipt getter may gate the terminal marker"
        );
    }

    #[test]
    fn destroyed_close_retry_reconciles_marker_without_a_second_stop() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let buyer = format!("0:{}", "55".repeat(32));
        let landed = dexdo_core::SettlementActionReceipt {
            token_contract: format!("0:{}", "22".repeat(32)),
            action: dexdo_core::SettlementAction::BuyerStop,
            message_id: "landed-stop".to_string(),
            created_at: 77,
            event: dexdo_core::SettlementActionEvent::StreamStopped {
                buyer: buyer.clone(),
                to_seller: 11u128.into(),
                refund_to_buyer: 22u128.into(),
            },
            pre_bonds: dexdo_core::SettlementActionBondState {
                seller_bond_held: 2u128.into(),
                seller_bond_required: 2u128.into(),
                buyer_bond_held: 0u128.into(),
                buyer_bond_required: 0u128.into(),
            },
            post_state: None,
        };
        let stop_calls = AtomicUsize::new(1);
        let marker_calls = AtomicUsize::new(0);
        let first = super::apply_confirmed_buyer_stop_marker(&landed, || {
            marker_calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(anyhow::anyhow!("first marker failure"))
        })
        .expect_err("the initial post-receipt marker fails");
        assert!(first.to_string().contains("landed-stop"));

        let receipts = dexdo_core::TokenContractSettlementReceipts {
            events: vec![dexdo_core::TokenContractSettlementReceipt {
                message_id: "landed-stop".to_string(),
                created_at: 77,
                cursor: "landed-stop-cursor".to_string(),
                event: dexdo_core::TokenContractSettlementEvent::StreamStopped {
                    buyer,
                    to_seller: 11,
                    refund_to_buyer: 22,
                },
            }],
        };
        let prior = crate::cli::recover::exact_prior_stop_receipt(
            &receipts,
            &format!("0:{}", "55".repeat(32)),
        )
        .unwrap()
        .expect("immutable STOP survives TokenContract destruction");
        let tc = dexdo_core::Address::parse(&landed.token_contract).unwrap();
        let note = dexdo_core::Address::parse(&format!("0:{}", "55".repeat(32))).unwrap();
        let confirmation =
            crate::cli::recover::prior_stop_confirmation("close", &tc, &note, &prior);
        super::apply_prior_stop_marker(&confirmation, || {
            marker_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .expect("retry performs only local reconciliation");
        assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(marker_calls.load(Ordering::SeqCst), 2);
        assert!(confirmation.contains("no second STOP was submitted"));
        let json = crate::cli::recover::prior_stop_receipt_json(&tc, &prior);
        assert_eq!(json["action"], "terminal_stop_reconciliation");
        assert_eq!(json["message_id"], "landed-stop");
        assert_eq!(json["event_kind"], "stream_stopped");
        assert_eq!(json["buyer"], format!("0:{}", "55".repeat(32)));
        assert_eq!(json["toSeller"], "11");
        assert_eq!(json["refundToBuyer"], "22");
        assert!(
            json.get("event").is_none(),
            "Debug event strings are forbidden"
        );

        let source = include_str!("close.rs");
        let retry = source
            .find("if role == deals::DealHandleRole::Buyer {")
            .expect("buyer immutable-receipt retry path");
        let tail = &source[retry..];
        let history = tail
            .find("token_contract_settlement_receipts(&tc)")
            .expect("immutable history read");
        let marker = tail
            .find("apply_prior_stop_marker(")
            .expect("local marker retry");
        let state = tail
            .find("token_contract_deal_snapshot(&tc)")
            .expect("live state read");
        assert!(history < marker && marker < state);
    }

    #[test]
    fn chain_buyer_close_routes_through_shared_explicit_stop() {
        let source = include_str!("close.rs");
        let body = crate::cli::source_probe::code_of(source, "deals::DealHandleRole::Buyer =>");
        assert!(body.contains(".explicit_buyer_stop(&note, &keys, &tc)"));
        assert!(!body.contains("chain.stream_stop(&note, &keys, &tc)"));
    }

    #[tokio::test]
    async fn stream_cleanup_submits_exactly_once_while_confirmation_only_observes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let submits = AtomicUsize::new(0);
        let observations = AtomicUsize::new(0);
        super::submit_then_observe_cleanup(
            || async {
                submits.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || async {
                observations.fetch_add(3, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(submits.load(Ordering::SeqCst), 1);
        assert_eq!(observations.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn settlement_report_carries_the_last_observation_beside_the_payout() {
        let mut response = super::close_response(
            "net-a",
            Some("deal-0-tc-buyer".to_string()),
            "buyer",
            "0:tc".to_string(),
            "streamStop",
            true,
            true,
            None,
            "streaming",
            "stopped",
        )
        .unwrap();
        response.tx = Some(serde_json::json!({
            "toSeller": "7000000000",
            "refundToBuyer": "11000000000"
        }));
        let observed = crate::cli::deals::LastObservedPromotion {
            tokens_final: 2 * dexdo_core::TICK_SIZE,
            tokens_pending: 3 * dexdo_core::TICK_SIZE,
            last_claim_time: 1_754_006_400,
        };

        let report = super::with_last_observed_promotion(response, Some(observed));
        let json = serde_json::to_value(report).unwrap();

        assert_eq!(json["tx"]["toSeller"], "7000000000");
        assert_eq!(
            json["last_observed_promotion"]["tokens_final"],
            (2 * dexdo_core::TICK_SIZE).to_string()
        );
        assert_eq!(
            json["last_observed_promotion"]["tokens_pending"],
            (3 * dexdo_core::TICK_SIZE).to_string()
        );
        assert_eq!(
            json["last_observed_promotion"]["last_claim_time"],
            1_754_006_400_u64
        );
    }

    #[test]
    fn settlement_report_without_an_observation_uses_null_not_zeroes() {
        let response = super::close_response(
            "net-a",
            Some("deal-0-tc-buyer".to_string()),
            "buyer",
            "0:tc".to_string(),
            "streamStop",
            true,
            true,
            None,
            "streaming",
            "stopped",
        )
        .unwrap();

        let report = super::with_last_observed_promotion(response, None);
        let json = serde_json::to_value(report).unwrap();

        assert!(json["last_observed_promotion"].is_null());
        assert_ne!(
            json["last_observed_promotion"],
            serde_json::json!({
                "tokens_final": "0",
                "tokens_pending": "0",
                "last_claim_time": 0
            })
        );
    }
}
