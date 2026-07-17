//! Market/pool lifecycle administration command handlers.

use crate::cli::args::{DestroyArgs, MarketDeployArgs, ProvisionArgs};
#[cfg(feature = "shellnet")]
use crate::cli::commands::{
    enforce_model_registry_policy, load_enabled_model_registry_policy, order_book_active,
    shellnet_doctor_preflight,
};
#[cfg(feature = "shellnet")]
use crate::cli::support::{
    deposit_per_deploy, ensure_provision_deposit_covered, prompt_deposit_shells, read_secret_hex,
    require_provision_nonce, resolve_market_fields, DEFAULT_DEPOSIT_SHELLS, SHELL_UNIT,
};
#[cfg(not(feature = "shellnet"))]
use anyhow::bail;
use anyhow::Result;
#[cfg(feature = "shellnet")]
use dexdo::registry::{BuyerMissingBookPolicy, RegistryRole};

/// Provision a per-deal market: the seller note brings up the
/// `InferenceOrderBook`(`deployInferenceOrderBook`) and pre-funds + deploys the `RootModel` + per-deal
/// `TokenContract` from its own ECC[2](`fundDeployShell` -> external seller-signed deploys), **no operator
/// multisig and no giver in the operate path**. Emits a
/// `MarketManifest` whose `token_contract` is the deployed, active address.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_provision(args: ProvisionArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "real shellnet provisioning: --note-addr (provisioned note address) is required"
        )
    })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("real shellnet provisioning: --note-key (note seed) is required")
    })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The deployed book/deal model name/hash MUST be canonical `producer--model--version`(indexer-parseable).
    dexdo_core::validate_canonical_model_id(&args.frame_model).map_err(|e| anyhow::anyhow!(e))?;
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    if let Some(policy) =
        load_enabled_model_registry_policy(RegistryRole::Seller, &args.registry, &args.contracts)?
    {
        let expected_order_book = chain
            .inference_orderbook_address(
                &note,
                &dexdo_core::model_hash_for(&args.frame_model),
                dexdo_core::MODEL_TICK_SIZE,
            )
            .await?
            .with_workchain();
        let order_book_active = order_book_active(&chain, &expected_order_book).await?;
        enforce_model_registry_policy(
            RegistryRole::Seller,
            &policy,
            &args.contracts,
            &args.frame_model,
            &expected_order_book,
            order_book_active,
            BuyerMissingBookPolicy::Reject,
        )
        .await?;
    }
    // REQUIRE an explicit, deal-unique nonce BEFORE any deposit/deploy -- the per-deal TokenContract derives
    // from(sellerPubkey, nonce); the old `--nonce 0` default silently reused(overwrote) a prior deal's TC.
    let nonce = require_provision_nonce(args.nonce)?;
    // the note deposit is a user-chosen provision parameter(default >=100 SHELL), framed by deal volume --
    // NOT a MIN_BALANCE-anchored per-op gas knob. 1 SHELL = 1e9 raw ECC[2]. The deposit is split across the
    // RootModel + per-deal `TokenContract` deploys, funded from the note's own ECC[2].
    let deposit_shells = match args.deposit_shells {
        Some(n) => n,
        None => prompt_deposit_shells()?.unwrap_or(DEFAULT_DEPOSIT_SHELLS),
    };
    // Fail-closed: overflow and a below-floor deposit are explicit errors, not a silent clamp/warn.
    let per_deploy = deposit_per_deploy(deposit_shells)?;
    eprintln!(
        "note deposit: {deposit_shells} SHELL ECC[2] (1 SHELL = 1e9 raw); ~{} SHELL per deploy for RootModel + \
         TokenContract after fundDeployShell. Unused deploy remainder burns at destroy; raise --deposit-shells if a \
         live TC needs more runtime gas.",
        per_deploy / SHELL_UNIT
    );
    // Run the stale/orphaned-note check BEFORE reading ECC balance. After a shellnet redeploy, old notes may be
    // absent/inactive/stale-code; reporting that as "0 SHELL" would mask the actionable re-mint reason.
    chain.assert_seller_note_current(&note).await?;
    // Fail-LOUD if the note's ECC[2] SHELL cannot cover the exact deploy deposit. Do not add guessed runtime
    // headroom here: section 6 requires any gas/SHELL threshold beyond the deploy amount to come from
    // contract constants/receipts, not a drifting reserve.
    let note_ecc = chain
        .client()
        .get_account(&note)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("seller note {note} disappeared after current-note preflight")
        })?
        .ecc_balance(2);
    ensure_provision_deposit_covered(note_ecc, deposit_shells, args.price_per_tick)?;
    let m = chain
        .provision_market(
            &keys,
            &note,
            &args.frame_model,
            nonce,
            args.price_per_tick,
            args.max_ticks,
            per_deploy,
        )
        .await?;
    let json = m.to_json()?;
    std::fs::write(&args.output, &json)
        .map_err(|e| anyhow::anyhow!("write --output {}: {e}", args.output.display()))?;
    println!("provisioned market -> {}", args.output.display());
    println!("{json}");
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_provision(_args: ProvisionArgs) -> Result<()> {
    bail!("real shellnet provisioning unavailable: build with `--features shellnet`")
}

/// `dexdo deploy-market`: deploy the per-model `InferenceOrderBook`(the shared market for a model) if it is
/// not yet on-chain -- note-funded, the explicit "list this model" step a seller runs before posting
/// offers. The book address is deterministic from `model_hash`, so this is idempotent (already-deployed ->
/// no-op). Same lazy deploy the seller's `post_offer` does, surfaced as a first-class operate command.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_market_deploy(args: MarketDeployArgs) -> Result<()> {
    use dexdo_core::{model_hash_for, Address, KeyPair, RealChainBackend, MODEL_TICK_SIZE};
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("real shellnet: --note-addr (active inference note) is required")
    })?;
    let note_key =
        args.identity.note_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-key (note owner key) is required")
        })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The book's on-chain model name/hash MUST be the canonical `producer--model--version` (what the indexer
    // parses); reject an OpenAI slug here BEFORE deploying an un-indexable book.
    dexdo_core::validate_canonical_model_id(&args.frame_model).map_err(|e| anyhow::anyhow!(e))?;
    // Fail-closed on a stale binary / live-network skew BEFORE the on-chain deploy -- same gate `provision`/
    // `seller` run. Without it, deploy-market would silently deploy an order book on outdated contract code
    // against a re-deployed network(a live run caught exactly this: live PrivateNote ahead of the binary pin).
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Seller, &args.registry, &args.contracts)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let model_hash = model_hash_for(&args.frame_model);
    let tick_size = MODEL_TICK_SIZE;
    let ob = chain
        .inference_orderbook_address(&note, &model_hash, tick_size)
        .await?;
    let expected_order_book = ob.with_workchain();
    let book_active = chain.inference_orderbook_stats(&ob).await?.is_some();
    if let Some(policy) = registry_policy.as_ref() {
        enforce_model_registry_policy(
            RegistryRole::Seller,
            policy,
            &args.contracts,
            &args.frame_model,
            &expected_order_book,
            book_active,
            BuyerMissingBookPolicy::Reject,
        )
        .await?;
    }
    if book_active {
        println!(
            "inference market already deployed for {} -- order book {}",
            args.frame_model,
            ob.with_workchain()
        );
        return Ok(());
    }
    println!(
        "deploying inference market (order book) for {} ...",
        args.frame_model
    );
    chain
        .deploy_inference_orderbook(&note, &keys, &model_hash, &args.frame_model, tick_size)
        .await?;
    // Wait for activation so a follow-up `post_offer` doesn't race the deploy(the book getter returns once active).
    for _ in 0..30 {
        if chain.inference_orderbook_stats(&ob).await?.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    println!(
        "deployed inference market for {} -- order book {}",
        args.frame_model,
        ob.with_workchain()
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_market_deploy(_args: MarketDeployArgs) -> Result<()> {
    bail!("real shellnet market deploy unavailable: build with `--features shellnet`")
}

/// the seller CLOSES a STOPped deal's per-deal `TokenContract` via `TokenContract::destroy(payoutAddress)`
/// (`onlyOwnerPubkey(_sellerPubkey)`, gated `!_opened && !_disputed`) -> `selfdestruct(payout)`.
/// **DESTRUCTIVE:** it selfdestructs the TC; the held leftover burns cross-dapp (the raw `selfdestruct` return is
/// not credited back to the cross-dapp note). At the right-sized ~10/deploy funding ( -- MIN_BALANCE gates
/// nothing) that leftover is ~a few vmshell(negligible), so the old fail-closed `--acknowledge-burn` for ~110 is
/// overkill -- it is optional now(kept for back-compat).
#[cfg(feature = "shellnet")]
pub(crate) async fn run_destroy(args: DestroyArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    let _ = args.acknowledge_burn; // optional now(the burn is ~a few vmshell) -- kept for back-compat
    eprintln!(
        "dexdo destroy: selfdestructs the TokenContract; the held leftover (~a few vmshell at the right-sized \
         ~10/deploy funding, ) burns cross-dapp (not credited back to the note) -- negligible."
    );
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("destroy: --note-addr (seller note = payout) is required")
    })?;
    let note_key = args
        .identity
        .note_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("destroy: --note-key (seller owner key) is required"))?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The TC comes from --token-contract OR --market(single source of truth, fail-loud).
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    eprintln!(
        "destroy {tc}: selfdestructs the TokenContract; under right-sized funding the remaining few vmshell \
         burn cross-dapp (not credited back to the note {note}). Seller-signed; requires the deal STOPped \
         (!_opened && !_disputed)."
    );
    chain.destroy_token_contract(&tc, &note, &keys).await?;
    println!(
        "destroy submitted -> TokenContract {tc} selfdestructs; remaining cross-dapp gas is not credited to note {note}"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_destroy(_args: DestroyArgs) -> Result<()> {
    bail!("destroy unavailable: build with `--features shellnet`")
}
