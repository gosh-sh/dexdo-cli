//! `dexdo` CLI command handlers(`seller`/`buyer`/`monitor`/`provision`/`destroy`/`recover`), split out of
//! `main.rs`(PR3, move-only). Behavior-identical to the pre-split handlers.

use crate::cli::args::*;
use crate::cli::audit;
use crate::cli::dashboard;
use crate::cli::deals;
use crate::cli::indexer::{self, DepthQuery, IndexerClient, MarketsQuery};
use crate::cli::machine;
use crate::cli::policy;
use crate::cli::support::*;
use crate::operator_shutdown_signal;
use anyhow::{anyhow, bail, Result};
#[cfg(feature = "shellnet")]
use dexdo::registry::{
    default_model_registry_address,
    enforce_model_registry_policy as enforce_model_registry_policy_with_reader,
    resolve_registered_model_identity, ShellnetModelRegistryReader,
};
use dexdo::registry::{
    BuyerMissingBookPolicy, RegistryBookAction, RegistryRole, RegistryValidationInput,
    RegistryValidationPolicy,
};
use dexdo_core::{
    aggregate_tree, check_buy_deposit_headroom, check_matched_token_contract_state,
    executable_quote, model_hash_for, required_escrow_for_buy, submit_safe_single_ask_quote,
    ChainBackend, ChainError, DealChainState, DobParams, ExecutableQuote,
    MatchedTokenContractStatus, MockChainBackend, OfferListing, OrderBookOrder, ProtocolConsts,
    Settlement, MATCH_OPEN_TIMEOUT_SECS,
};
#[cfg(feature = "shellnet")]
use dexdo_core::{OrderBookSnapshot, OrderBookSubscription};
use serde_json::{json, Map, Value};
use std::future::Future;
use std::io::Write as _;
use std::sync::Arc;

/// Deadline for awaiting match/handover: fail-closed, so `seller`/`buyer` don't hang
/// forever if the match didn't go through. Backstop, not SLA -- a real on-chain match completes in ~1-2 min.
pub(crate) const DEAL_WAIT_SECS: u64 = 300;
/// Lookback window for a model-only `--resume`: how far back to scan THIS note's own
/// `InferenceFilledConfirmed` events for the freshly matched deal (the buyer learns its deal from its own
/// note, never a hand-pasted address). Wide enough to survive a process restart / slow match, short enough
/// to skip earlier, already closed deals on the same book. The reader returns the MOST RECENT match in-window.
pub(crate) const RESUME_LOOKBACK_SECS: i64 = 1800;
#[cfg(feature = "shellnet")]
const DEFAULT_CONTRACTS_PATH: &str = "contracts/deployed.shellnet.json";
#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_LOCK_TIMEOUT_SECS: u64 = 3600;

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
async fn direct_chain_read_with_timeout<T>(
    timeout_secs: u64,
    read: impl Future<Output = Result<T>>,
) -> Result<T> {
    let duration = std::time::Duration::from_secs(timeout_secs);
    match tokio::time::timeout(duration, read).await {
        Ok(result) => result,
        Err(_) => bail!(
            "chain read timed out after {timeout_secs}s; retry or use `dexdo market-data` where applicable"
        ),
    }
}

#[cfg(feature = "shellnet")]
struct DealTarget {
    handle: Option<deals::DealHandle>,
    token_contract: String,
    role: Option<deals::DealHandleRole>,
    note_addr: Option<String>,
    market: Option<dexdo_core::MarketManifest>,
}

struct RuntimeDealHandleInput<'a> {
    role: deals::DealHandleRole,
    deals_dir: Option<&'a std::path::Path>,
    token_contract: &'a str,
    note_addr: &'a str,
    frame_model: &'a str,
    market_path: Option<&'a std::path::Path>,
    contracts: &'a std::path::Path,
    endpoint: Option<deals::DealEndpointInfo>,
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone)]
struct PoolRecoveryInputs {
    note_addr: String,
    note_secret_hex: String,
    token_contract: String,
}

#[cfg(feature = "shellnet")]
struct NoteDeployWalletLock {
    path: std::path::PathBuf,
}

#[cfg(feature = "shellnet")]
impl Drop for NoteDeployWalletLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn note_pubkey_id(pk: &dexdo_core::NotePubkey) -> String {
    pk.ed.iter().map(|b| format!("{b:02x}")).collect()
}

fn persist_runtime_deal_handle(
    input: RuntimeDealHandleInput<'_>,
    network: &str,
) -> Result<deals::DealHandle> {
    let market = input.market_path.map(load_market).transpose()?;
    let h = deals::DealHandle {
        version: deals::DEAL_HANDLE_VERSION,
        handle: deals::make_handle_id(input.token_contract),
        role: input.role,
        network: network.to_string(),
        token_contract: input.token_contract.to_string(),
        note_addr: input.note_addr.to_string(),
        frame_model: input.frame_model.to_string(),
        model_hash: Some(model_hash_for(input.frame_model)),
        order_book: market.as_ref().map(|m| m.inference_order_book.clone()),
        root_model: market.as_ref().map(|m| m.root_model.clone()),
        market,
        contracts: input.contracts.display().to_string(),
        endpoint: input.endpoint,
        created_order_ids: Vec::new(),
        created_at_unix: deals::now_unix()?,
    };
    deals::validate_deal_handle(&h)?;
    let dir = deals::resolve_deals_dir(input.deals_dir)?;
    deals::save_deal_handle(&dir, &h)?;
    Ok(h)
}

fn save_mock_runtime_deal_handle(input: RuntimeDealHandleInput<'_>) -> Result<deals::DealHandle> {
    persist_runtime_deal_handle(input, "mock")
}

#[cfg(feature = "shellnet")]
fn load_pool_json(path: &std::path::Path) -> Result<Value> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("read DEXDO_PN_POOL {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("parse DEXDO_PN_POOL {}: {e}", path.display()))
}

#[cfg(feature = "shellnet")]
fn note_pool_path(explicit: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    match std::env::var_os("DEXDO_PN_POOL") {
        Some(raw) if !raw.is_empty() => Some(std::path::PathBuf::from(raw)),
        _ => None,
    }
}

#[cfg(feature = "shellnet")]
fn persist_pool_token_contract_for_note(
    pool_path: &std::path::Path,
    note_addr: &str,
    token_contract: &str,
    role: &str,
) -> Result<()> {
    let pool = load_pool_json(pool_path)?;
    let updated = crate::cli::note::pool_with_note_token_contract_recorded(
        pool,
        note_addr,
        token_contract,
        role,
        unix_now_secs(),
    )?;
    let bytes = serde_json::to_vec_pretty(&updated)?;
    write_pool_private(pool_path, &bytes)?;
    Ok(())
}

#[cfg(feature = "shellnet")]
fn preflight_buyer_pool_for_note(note_addr: Option<&str>) -> Result<()> {
    let Some(pool_path) = note_pool_path(None) else {
        return Ok(());
    };
    let note_addr = note_addr.ok_or_else(|| {
        anyhow::anyhow!(
            "real shellnet: --note-addr is required to preflight DEXDO_PN_POOL before buying"
        )
    })?;
    let pool = load_pool_json(&pool_path)?;
    crate::cli::note::pool_has_unique_note_entry(&pool, note_addr)?;
    let bytes = serde_json::to_vec_pretty(&pool)?;
    write_pool_private(&pool_path, &bytes).map_err(|e| {
        anyhow::anyhow!(
            "preflight DEXDO_PN_POOL {} before buying: pool is not safely updateable: {e}",
            pool_path.display()
        )
    })
}

#[cfg(not(feature = "shellnet"))]
fn preflight_buyer_pool_for_note(_note_addr: Option<&str>) -> Result<()> {
    Ok(())
}

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
        .map_err(|e| anyhow::anyhow!("{e}"))
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

#[cfg(feature = "shellnet")]
fn resolve_pool_recovery_inputs(
    command: &str,
    identity: &IdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
) -> Result<PoolRecoveryInputs> {
    let explicit_tc = if market.is_some() || token_contract.is_some() {
        let (tc, _frame, _nonce) = resolve_market_fields(market, token_contract, None)?;
        Some(dexdo_core::normalize_wallet_address(&tc).map_err(|e| anyhow::anyhow!("{e}"))?)
    } else {
        None
    };
    let explicit_note_addr = identity
        .note_addr
        .as_deref()
        .map(dexdo_core::normalize_wallet_address)
        .transpose()
        .map_err(|e| anyhow::anyhow!("--note-addr: {e}"))?;
    if let (Some(note_addr), Some(note_key), Some(tc)) = (
        &explicit_note_addr,
        identity.note_key.as_deref(),
        &explicit_tc,
    ) {
        return Ok(PoolRecoveryInputs {
            note_addr: note_addr.clone(),
            note_secret_hex: read_secret_hex(note_key, "--note-key")?,
            token_contract: tc.clone(),
        });
    }

    let Some(pool_path) = note_pool_path(pool) else {
        bail!(
            "{command}: pass --note-addr, --note-key, and --token-contract/--market, or pass --pool / set \
             DEXDO_PN_POOL containing this note entry with token_contract recovery metadata"
        );
    };
    let pool = load_pool_json(&pool_path)?;
    let mut records = crate::cli::note::pool_note_recovery_records(&pool)?
        .into_iter()
        .filter(|(note_addr, _, tc)| {
            explicit_note_addr
                .as_ref()
                .map_or(true, |want| want == note_addr)
                && explicit_tc.as_ref().map_or(true, |want| want == tc)
        })
        .collect::<Vec<_>>();
    if records.is_empty() {
        bail!(
            "{command}: DEXDO_PN_POOL {} has no matching note entry with token_contract recovery metadata; \
             run the buyer once with this pool active, or pass explicit --note-addr/--note-key/--token-contract",
            pool_path.display()
        );
    }
    if records.len() > 1 {
        bail!(
            "{command}: DEXDO_PN_POOL {} has {} matching recovery entries; pass --note-addr or --token-contract \
             to disambiguate",
            pool_path.display(),
            records.len()
        );
    }
    let (pool_note_addr, pool_secret, pool_tc) = records.remove(0);
    let note_secret_hex = match identity.note_key.as_deref() {
        Some(path) => read_secret_hex(path, "--note-key")?,
        None => pool_secret,
    };
    Ok(PoolRecoveryInputs {
        note_addr: explicit_note_addr.unwrap_or(pool_note_addr),
        note_secret_hex,
        token_contract: explicit_tc.unwrap_or(pool_tc),
    })
}

#[cfg(feature = "shellnet")]
fn note_deploy_lock_path(funding_multisig_address: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(funding_multisig_address.as_bytes());
    std::env::temp_dir().join(format!(
        "dexdo-note-deploy-wallet-{}.lock",
        &hex::encode(digest)[..16]
    ))
}

#[cfg(feature = "shellnet")]
fn acquire_note_deploy_wallet_lock(funding_multisig_address: &str) -> Result<NoteDeployWalletLock> {
    let path = note_deploy_lock_path(funding_multisig_address);
    let timeout = std::env::var("DEXDO_NOTE_DEPLOY_LOCK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(NOTE_DEPLOY_LOCK_TIMEOUT_SECS);
    let started = std::time::Instant::now();
    let mut announced = false;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(
                    file,
                    "pid={} wallet={} created_at_unix={}",
                    std::process::id(),
                    funding_multisig_address,
                    unix_now_secs()
                )
                .ok();
                return Ok(NoteDeployWalletLock { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if started.elapsed().as_secs() >= timeout {
                    bail!(
                        "note deploy wallet busy: another `dexdo note deploy` appears to be using funding wallet \
                         {funding_multisig_address}; lock {} remained for {timeout}s. Retry after the previous \
                         deploy reaches a terminal state, or remove the lock only after confirming no deploy is \
                         running.",
                        path.display()
                    );
                }
                if !announced {
                    eprintln!(
                        "note deploy: funding wallet {funding_multisig_address} is already in use locally; \
                         waiting for {} (timeout {timeout}s)",
                        path.display()
                    );
                    announced = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => bail!("create note deploy wallet lock {}: {e}", path.display()),
        }
    }
}

#[cfg(feature = "shellnet")]
fn is_note_deploy_wallet_busy_error(error: &anyhow::Error) -> bool {
    let msg = error.to_string().to_ascii_lowercase();
    msg.contains("tvm_error")
        || msg.contains("replay protection")
        || msg.contains("exit code 52")
        || msg.contains("nonce")
        || msg.contains("seqno")
}

#[cfg(feature = "shellnet")]
fn note_deploy_error(funding_multisig_address: &str, error: anyhow::Error) -> anyhow::Error {
    if is_note_deploy_wallet_busy_error(&error) {
        anyhow::anyhow!(
            "note deploy wallet busy/out-of-sync for funding wallet {funding_multisig_address}: a previous \
             wallet transaction is likely still pending or the wallet nonce cache is stale. Retry after the prior \
             `dexdo note deploy` reaches a terminal state; local deploys are serialized by a wallet lock."
        )
    } else {
        anyhow::anyhow!("deploy PrivateNote from wallet {funding_multisig_address}: {error}")
    }
}

fn load_enabled_model_registry_policy(
    role: RegistryRole,
    args: &ModelRegistryValidationArgs,
    contracts: &std::path::Path,
) -> Result<Option<RegistryValidationPolicy>> {
    let policy = RegistryValidationPolicy::load(
        &RegistryValidationInput {
            config_path: args.model_registry_validation.clone(),
            address_override: args.model_registry_address.clone(),
        },
        contracts,
    )?;
    if policy.check_enabled(role) {
        Ok(Some(policy))
    } else {
        Ok(None)
    }
}

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
async fn enforce_model_registry_policy(
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    contracts: &std::path::Path,
    frame_model: &str,
    expected_order_book: &str,
    order_book_active: bool,
    buyer_missing_book_policy: BuyerMissingBookPolicy,
) -> Result<RegistryBookAction> {
    let registry_address = policy.required_address(role)?;
    let reader = ShellnetModelRegistryReader::from_manifest(contracts, registry_address)?;
    enforce_model_registry_policy_with_reader(
        &reader,
        role,
        policy,
        frame_model,
        expected_order_book,
        order_book_active,
        buyer_missing_book_policy,
    )
    .await
}

#[cfg(not(feature = "shellnet"))]
async fn enforce_model_registry_policy(
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    contracts: &std::path::Path,
    frame_model: &str,
    expected_order_book: &str,
    order_book_active: bool,
    buyer_missing_book_policy: BuyerMissingBookPolicy,
) -> Result<RegistryBookAction> {
    let _ = (
        role,
        policy,
        contracts,
        frame_model,
        expected_order_book,
        order_book_active,
        buyer_missing_book_policy,
    );
    bail!("ModelRegistry validation requires a shellnet build")
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

#[cfg(feature = "shellnet")]
fn role_arg_to_handle(role: DealRoleArg) -> deals::DealHandleRole {
    match role {
        DealRoleArg::Buyer => deals::DealHandleRole::Buyer,
        DealRoleArg::Seller => deals::DealHandleRole::Seller,
    }
}

#[cfg(feature = "shellnet")]
fn load_deal_target(
    input: &str,
    deals_dir: Option<&std::path::Path>,
    raw_role: Option<DealRoleArg>,
    raw_note_addr: Option<String>,
) -> Result<DealTarget> {
    let dir = deals::resolve_deals_dir(deals_dir)?;
    if let Some((_path, handle)) = deals::resolve_deal_ref(input, &dir)? {
        let role = handle.role;
        let token_contract = handle.token_contract.clone();
        let note_addr = Some(handle.note_addr.clone());
        let market = handle.market.clone();
        return Ok(DealTarget {
            handle: Some(handle),
            token_contract,
            role: Some(role),
            note_addr,
            market,
        });
    }
    Ok(DealTarget {
        handle: None,
        token_contract: input.to_string(),
        role: raw_role.map(role_arg_to_handle),
        note_addr: raw_note_addr,
        market: None,
    })
}

#[cfg(feature = "shellnet")]
fn deal_contracts_path(
    explicit: Option<&std::path::Path>,
    target: &DealTarget,
) -> std::path::PathBuf {
    explicit
        .map(std::path::PathBuf::from)
        .or_else(|| {
            target.handle.as_ref().and_then(|h| {
                (!h.contracts.trim().is_empty()).then(|| std::path::PathBuf::from(&h.contracts))
            })
        })
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_CONTRACTS_PATH))
}

#[cfg(feature = "shellnet")]
async fn shellnet_doctor_preflight_market(
    contracts: &std::path::Path,
    market: Option<&dexdo_core::MarketManifest>,
) -> Result<()> {
    let contracts = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(contracts)?;
    let report = chain.doctor(market).await?;
    if !report.is_ok() {
        bail!("{}", render_shellnet_doctor_report(&report));
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn save_runtime_deal_handle(
    input: RuntimeDealHandleInput<'_>,
    emit_human_output: bool,
) -> Result<deals::DealHandle> {
    let h = persist_runtime_deal_handle(input, "shellnet")?;
    if emit_human_output {
        println!("deal_handle={}", h.handle);
    }
    Ok(h)
}

#[cfg(not(feature = "shellnet"))]
fn save_runtime_deal_handle(
    _input: RuntimeDealHandleInput<'_>,
    _emit_human_output: bool,
) -> Result<deals::DealHandle> {
    bail!("real shellnet deal handles unavailable: build with `--features shellnet`")
}

fn seller_watch_cursor_path(
    deals_dir: Option<&std::path::Path>,
    token_contract: &str,
) -> Result<std::path::PathBuf> {
    Ok(deals::resolve_deals_dir(deals_dir)?
        .join("seller-watch")
        .join(format!(
            "{}.cursor.json",
            deals::make_handle_id(token_contract)
        )))
}

#[cfg(feature = "shellnet")]
const ORACLE_MIN_RESULT_GAP_SECS: u64 = 120;

#[cfg(feature = "shellnet")]
async fn shellnet_doctor_report(
    network: &str,
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
) -> Result<dexdo_core::ShellnetDoctorReport> {
    if network != "shellnet" {
        bail!("doctor: unsupported --network `{network}` (only `shellnet` is supported)");
    }
    let contracts = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let market = market.map(load_market).transpose()?;
    let chain = dexdo_core::RealChainBackend::connect(contracts)?;
    chain.doctor(market.as_ref()).await
}

#[cfg(feature = "shellnet")]
fn render_shellnet_doctor_report(report: &dexdo_core::ShellnetDoctorReport) -> String {
    let mut out = String::new();
    let status = if report.is_ok() { "PASS" } else { "FAIL" };
    out.push_str(&format!(
        "dexdo doctor: {status} network={}\n",
        report.network
    ));
    if !report.versions.is_empty() {
        out.push_str("versions:\n");
        for (name, version) in &report.versions {
            out.push_str(&format!("  {name}: {version}\n"));
        }
    }
    out.push_str("checks:\n");
    for c in &report.checks {
        out.push_str(&format!("  {:<4} {}", c.status.as_str(), c.name));
        if let Some(addr) = &c.address {
            out.push_str(&format!(" addr={addr}"));
        }
        if let Some(expected) = &c.expected {
            out.push_str(&format!(" expected={expected}"));
        }
        if let Some(actual) = &c.actual {
            out.push_str(&format!(" actual={actual}"));
        }
        out.push_str(&format!(" - {}\n", c.message));
    }
    out
}

#[cfg(feature = "shellnet")]
async fn shellnet_doctor_preflight(
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
) -> Result<()> {
    let report = shellnet_doctor_report("shellnet", contracts, market).await?;
    if !report.is_ok() {
        bail!("{}", render_shellnet_doctor_report(&report));
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
async fn shellnet_doctor_preflight(
    _contracts: &std::path::Path,
    _market: Option<&std::path::Path>,
) -> Result<()> {
    bail!("shellnet doctor unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_doctor(args: DoctorArgs) -> Result<()> {
    let report =
        shellnet_doctor_report(&args.network, &args.contracts, args.market.as_deref()).await?;
    print!("{}", render_shellnet_doctor_report(&report));
    println!("{}", policy::doctor_policy_line(args.policy.as_deref())?);
    if !report.is_ok() {
        bail!("doctor failed: {}", report.fail_summary());
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_doctor(_args: DoctorArgs) -> Result<()> {
    bail!("shellnet doctor unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
struct BookTarget {
    frame_model: String,
    model_hash: String,
    order_book: Option<String>,
    root_model: Option<String>,
    note_addr: Option<String>,
}

#[cfg(feature = "shellnet")]
fn model_target_from_config(
    models: &std::path::Path,
    model: &str,
    note_addr: Option<String>,
) -> Result<BookTarget> {
    let cfg = dexdo::seller::ModelsConfig::load(models)?;
    let frame_model = cfg.get(model)?.frame_model.clone();
    Ok(BookTarget {
        model_hash: model_hash_for(&frame_model),
        frame_model,
        order_book: None,
        root_model: None,
        note_addr,
    })
}

#[cfg(feature = "shellnet")]
fn target_from_market(path: &std::path::Path) -> Result<BookTarget> {
    let m = load_market(path)?;
    Ok(BookTarget {
        frame_model: m.frame_model,
        model_hash: m.model_hash,
        order_book: Some(m.inference_order_book),
        root_model: Some(m.root_model),
        note_addr: None,
    })
}

#[cfg(feature = "shellnet")]
fn target_from_market_for_model(
    path: &std::path::Path,
    models: &std::path::Path,
    requested_model: &str,
) -> Result<BookTarget> {
    let target = target_from_market(path)?;
    let requested_frame_model = if dexdo_core::validate_canonical_model_id(requested_model).is_ok()
    {
        requested_model.to_string()
    } else {
        dexdo::seller::ModelsConfig::load(models)?
            .get(requested_model)?
            .frame_model
            .clone()
    };
    let requested_hash = model_hash_for(&requested_frame_model);
    if target.frame_model != requested_frame_model || target.model_hash != requested_hash {
        bail!(
            "dexdo market requested model `{requested_model}` -> `{requested_frame_model}`, but --market is for \
             `{}` (model_hash {}): refusing to render the wrong market",
            target.frame_model,
            target.model_hash
        );
    }
    Ok(target)
}

#[cfg(feature = "shellnet")]
async fn read_book_target(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
) -> Result<OrderBookSnapshot> {
    if let Some(ob) = &target.order_book {
        let ob =
            dexdo_core::Address::parse(ob).map_err(|e| anyhow::anyhow!("order_book {ob}: {e}"))?;
        return chain
            .inference_orderbook_snapshot(&ob, &target.frame_model, &target.model_hash)
            .await;
    }
    let note_addr = target
        .note_addr
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--note-addr is required when --market is not supplied"))?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    chain
        .inference_orderbook_snapshot_for_note(
            &note,
            &target.frame_model,
            &target.model_hash,
            dexdo_core::MODEL_TICK_SIZE,
        )
        .await
}

#[cfg(feature = "shellnet")]
async fn read_executable_book_target(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
) -> Result<OrderBookSnapshot> {
    let mut snapshot = read_book_target(chain, target).await?;
    snapshot.orders = chain.executable_resting_asks(&snapshot).await?;
    Ok(snapshot)
}

#[cfg(feature = "shellnet")]
async fn expected_order_book_for_note(
    contracts: &std::path::Path,
    note_addr: &str,
    frame_model: &str,
) -> Result<String> {
    let manifest = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let model_hash = model_hash_for(frame_model);
    let ob = chain
        .inference_orderbook_address(&note, &model_hash, dexdo_core::MODEL_TICK_SIZE)
        .await?;
    Ok(ob.with_workchain())
}

#[cfg(feature = "shellnet")]
async fn order_book_active(
    chain: &dexdo_core::RealChainBackend,
    expected_order_book: &str,
) -> Result<bool> {
    let ob = dexdo_core::Address::parse(expected_order_book)
        .map_err(|e| anyhow::anyhow!("order_book {expected_order_book}: {e}"))?;
    Ok(chain.inference_orderbook_stats(&ob).await?.is_some())
}

#[cfg(feature = "shellnet")]
async fn order_book_active_from_contracts(
    contracts: &std::path::Path,
    expected_order_book: &str,
) -> Result<bool> {
    let manifest = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    order_book_active(&chain, expected_order_book).await
}

#[cfg(not(feature = "shellnet"))]
async fn order_book_active_from_contracts(
    contracts: &std::path::Path,
    expected_order_book: &str,
) -> Result<bool> {
    let _ = (contracts, expected_order_book);
    bail!("order-book state reads require a shellnet build")
}

#[cfg(not(feature = "shellnet"))]
async fn expected_order_book_for_note(
    contracts: &std::path::Path,
    note_addr: &str,
    frame_model: &str,
) -> Result<String> {
    let _ = (contracts, note_addr, frame_model);
    bail!("order-book derivation requires a shellnet build")
}

#[cfg(feature = "shellnet")]
fn own_orders<'a>(snapshot: &'a OrderBookSnapshot, note_addr: &str) -> Vec<&'a OrderBookOrder> {
    let want = dexdo_core::normalize_wallet_address(note_addr)
        .unwrap_or_else(|_| note_addr.trim().to_string());
    snapshot
        .orders
        .iter()
        .filter(|o| {
            dexdo_core::normalize_wallet_address(&o.owner_note)
                .map(|owner| owner == want)
                .unwrap_or_else(|_| o.owner_note.eq_ignore_ascii_case(&want))
        })
        .collect()
}

#[cfg(feature = "shellnet")]
fn render_order_line(order: &OrderBookOrder) -> String {
    let side = if order.is_buy { "buy" } else { "sell" };
    let tc = order.token_contract.as_deref().unwrap_or("-");
    format!(
        "order_id={} side={} owner={} token_contract={} price_per_tick={} ticks={} escrow={} flags={} deadline={}",
        order.order_id,
        side,
        order.owner_note,
        tc,
        order.price_per_tick,
        order.ticks,
        order.escrow,
        order.flags,
        order.deadline
    )
}

fn mock_chain_for_machine(endpoints_file: Option<std::path::PathBuf>) -> Result<MockChainBackend> {
    let endpoints_file = resolve_endpoints_file(endpoints_file)?;
    Ok(MockChainBackend::new(
        endpoints_file,
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    ))
}

async fn mock_market_entry(
    chain: &MockChainBackend,
    frame_model: &str,
) -> Result<machine::MarketEntry> {
    let offers = chain.discover_offers().await?;
    let depth_ticks: u128 = offers.iter().map(|o| u128::from(o.max_ticks)).sum();
    let best_ask = offers.iter().map(|o| o.price_per_tick).min();
    Ok(machine::MarketEntry {
        frame_model: frame_model.to_string(),
        model_hash: model_hash_for(frame_model),
        order_book: "mock:order-book".to_string(),
        root_model: Some("mock:root-model".to_string()),
        active: true,
        order_count: offers.len() as u128,
        ask_count: offers.len() as u128,
        depth_ticks: machine::amount(depth_ticks),
        best_ask: best_ask.map(machine::amount),
        min_liquidity: machine::amount(0u8),
        tick_size: machine::amount(DobParams::canonical().tick_size),
        source: "mock_chain".to_string(),
    })
}

async fn run_markets_mock(args: MarketsArgs) -> Result<()> {
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let entry = mock_market_entry(&chain, &args.frame_model).await?;
    if args.json {
        return machine::print_json(&machine::MarketsResponse {
            schema: machine::MARKETS_SCHEMA,
            network: "mock".to_string(),
            generated_at_unix: machine::now_unix()?,
            markets: vec![entry],
        });
    }
    println!(
        "model={} order_book={} active={} order_count={} ask_count={} depth_ticks={} best_ask={}",
        entry.frame_model,
        entry.order_book,
        entry.active,
        entry.order_count,
        entry.ask_count,
        entry.depth_ticks,
        entry.best_ask.as_deref().unwrap_or("-")
    );
    Ok(())
}

fn mock_orders_from_offers(offers: Vec<OfferListing>) -> Vec<OrderBookOrder> {
    offers
        .into_iter()
        .enumerate()
        .map(|(i, offer)| OrderBookOrder {
            order_id: (i as u128).saturating_add(1),
            owner_note: offer.seller_id,
            token_contract: Some(offer.token_contract),
            is_buy: false,
            price_per_tick: u128::from(offer.price_per_tick),
            ticks: u128::from(offer.max_ticks),
            escrow: 0,
            deadline: 0,
            flags: 0,
            timestamp: 0,
        })
        .collect()
}

struct BuyerQuoteSelection {
    order_book: &'static str,
    escrow: u128,
    quote: ExecutableQuote,
}

async fn buyer_quote_selection(
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
            .map_err(|e| anyhow::anyhow!("buyer model-only quote preflight: {e}"))?;
    } else if let Some(tc) = explicit_tc {
        let tc_owned = tc.to_string();
        explicit_submit_safe_order = chain
            .submit_safe_explicit_buy_quote_order(&tc_owned, ticks, max_price_per_tick)
            .await
            .map_err(|e| anyhow::anyhow!("buyer explicit-token quote preflight: {e}"))?;
        if explicit_submit_safe_order.is_none() {
            chain
                .assert_explicit_buy_matches_executable_quote(&tc_owned, ticks, max_price_per_tick)
                .await
                .map_err(|e| anyhow::anyhow!("buyer explicit-token quote preflight: {e}"))?;
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
    Ok(BuyerQuoteSelection {
        order_book,
        escrow: escrow.unwrap_or_else(|| required_escrow_for_buy(ticks, max_price_per_tick)),
        quote,
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

fn quote_response_from_quote(
    network: &str,
    frame_model: &str,
    order_book: &str,
    ticks: Option<u128>,
    budget: Option<u128>,
    q: dexdo_core::ExecutableQuote,
) -> Result<machine::QuoteResponse> {
    let mut total_without_fee = 0u128;
    let fills = q
        .fills
        .into_iter()
        .map(|fill| {
            let cost_without_fee = fill.ticks.saturating_mul(fill.price_per_tick);
            let platform_fee = fill.cost_with_fee.saturating_sub(cost_without_fee);
            total_without_fee = total_without_fee.saturating_add(cost_without_fee);
            machine::QuoteFillEntry {
                order_id: machine::amount(fill.order_id),
                token_contract: fill.token_contract,
                ticks: machine::amount(fill.ticks),
                price_per_tick: machine::amount(fill.price_per_tick),
                cost_without_fee: machine::amount(cost_without_fee),
                platform_fee: machine::amount(platform_fee),
                cost_with_fee: machine::amount(fill.cost_with_fee),
            }
        })
        .collect::<Vec<_>>();
    let platform_fee = q.total_with_fee.saturating_sub(total_without_fee);
    Ok(machine::QuoteResponse {
        schema: machine::QUOTE_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        frame_model: frame_model.to_string(),
        model_hash: model_hash_for(frame_model),
        order_book: order_book.to_string(),
        request: machine::QuoteRequest {
            kind: if ticks.is_some() { "ticks" } else { "budget" },
            ticks: ticks.map(machine::amount),
            budget: budget.map(machine::amount),
        },
        filled_ticks: machine::amount(q.filled_ticks),
        total_without_fee: machine::amount(total_without_fee),
        platform_fee: machine::amount(platform_fee),
        total_with_fee: machine::amount(q.total_with_fee),
        complete: q.complete,
        no_liquidity: q.filled_ticks == 0,
        fills,
    })
}

async fn run_quote_mock(args: QuoteArgs) -> Result<()> {
    if args.ticks.is_some() == args.budget.is_some() {
        bail!("quote requires exactly one of --ticks or --budget");
    }
    let frame_model = args.model.as_deref().unwrap_or("dexdo-mock");
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let orders = mock_orders_from_offers(chain.discover_offers().await?);
    let q = executable_quote(&orders, args.ticks, args.budget)
        .map_err(|e| anyhow::anyhow!("quote: {e}"))?;
    if args.json {
        return machine::print_json(&quote_response_from_quote(
            "mock",
            frame_model,
            "mock:order-book",
            args.ticks,
            args.budget,
            q,
        )?);
    }
    if q.filled_ticks == 0 {
        println!("quote model={frame_model} order_book=mock:order-book no_liquidity=true");
        return Ok(());
    }
    println!(
        "quote model={} order_book=mock:order-book filled_ticks={} total_with_fee={} complete={}",
        frame_model, q.filled_ticks, q.total_with_fee, q.complete
    );
    for fill in q.fills {
        println!(
            "fill order_id={} token_contract={} ticks={} price_per_tick={} cost_with_fee={}",
            fill.order_id, fill.token_contract, fill.ticks, fill.price_per_tick, fill.cost_with_fee
        );
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn market_entry_from_snapshot(
    snapshot: &OrderBookSnapshot,
    root_model: Option<String>,
    source: &str,
) -> machine::MarketEntry {
    let depth_ticks: u128 = snapshot.resting_asks().map(|o| o.ticks).sum();
    let best_ask = snapshot.resting_asks().map(|o| o.price_per_tick).min();
    let order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
    machine::MarketEntry {
        frame_model: snapshot.frame_model.clone(),
        model_hash: snapshot.model_hash.clone(),
        order_book: snapshot.order_book.clone(),
        root_model,
        active: snapshot.active(),
        order_count,
        ask_count: snapshot.resting_asks().count() as u128,
        depth_ticks: machine::amount(depth_ticks),
        best_ask: best_ask.map(machine::amount),
        min_liquidity: machine::amount(0u8),
        tick_size: machine::amount(DobParams::canonical().tick_size),
        source: source.to_string(),
    }
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_markets(args: MarketsArgs) -> Result<()> {
    if args.mock_chain {
        return run_markets_mock(args).await;
    }
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let targets = if args.market.is_empty() {
        let note_addr = args.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "markets without --market requires --note-addr to derive order-book addresses"
            )
        })?;
        let cfg = dexdo::seller::ModelsConfig::load(&args.models)?;
        cfg.models
            .values()
            .map(|m| BookTarget {
                frame_model: m.frame_model.clone(),
                model_hash: model_hash_for(&m.frame_model),
                order_book: None,
                root_model: None,
                note_addr: Some(note_addr.clone()),
            })
            .collect::<Vec<_>>()
    } else {
        args.market
            .iter()
            .map(|p| target_from_market(p))
            .collect::<Result<Vec<_>>>()?
    };
    direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
        if args.json {
            let mut markets = Vec::new();
            for target in targets {
                let source = if target.order_book.is_some() {
                    "market_manifest"
                } else {
                    "models_config"
                };
                let root_model = target.root_model.clone();
                let snapshot = read_executable_book_target(&chain, &target).await?;
                markets.push(market_entry_from_snapshot(&snapshot, root_model, source));
            }
            return machine::print_json(&machine::MarketsResponse {
                schema: machine::MARKETS_SCHEMA,
                network: "shellnet".to_string(),
                generated_at_unix: machine::now_unix()?,
                markets,
            });
        }
        for target in targets {
            let snapshot = read_executable_book_target(&chain, &target).await?;
            if let Some(policy) = registry_policy.as_ref() {
                let action = enforce_model_registry_policy(
                    RegistryRole::Buyer,
                    policy,
                    &args.contracts,
                    &target.frame_model,
                    &snapshot.order_book,
                    snapshot.active(),
                    BuyerMissingBookPolicy::HideFromAvailableList,
                )
                .await?;
                if action == RegistryBookAction::BuyerHideMissing {
                    continue;
                }
            }
            let depth_ticks: u128 = snapshot.resting_asks().map(|o| o.ticks).sum();
            let best_ask = snapshot.resting_asks().map(|o| o.price_per_tick).min();
            let order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
            println!(
                "model={} order_book={} active={} order_count={} ask_count={} depth_ticks={} best_ask={}",
                snapshot.frame_model,
                snapshot.order_book,
                snapshot.active(),
                order_count,
                snapshot.resting_asks().count(),
                depth_ticks,
                best_ask
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string())
            );
        }
        Ok(())
    })
    .await
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_markets(args: MarketsArgs) -> Result<()> {
    if args.mock_chain {
        return run_markets_mock(args).await;
    }
    bail!("markets unavailable: build with `--features shellnet`")
}

/// `dexdo market <canonical-model>` -- render ONE model's order book as the human-readable box table
/// (the same view the buyer shows before a buy). Read-only, keyed by the canonical model name.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_market(args: MarketArgs) -> Result<()> {
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    // The book is keyed by the canonical model: derive it from `--note-addr` (any active note supplies the
    // book code), or read it from a provision manifest. `market.json` is the seller's artifact -- a buyer
    // normally passes only the model name + its own `--note-addr`.
    let target = if let Some(market) = args.market.as_deref() {
        if args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --note-addr");
        }
        target_from_market_for_model(market, &args.models, &args.model)?
    } else {
        model_target_from_config(&args.models, &args.model, args.note_addr.clone()).map_err(|e| {
            anyhow::anyhow!("{e}\n(pass --note-addr 0:<your PrivateNote> so the per-model book can be derived)")
        })?
    };
    let snapshot = direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
        let snapshot = read_executable_book_target(&chain, &target).await?;
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
        Ok(snapshot)
    })
    .await?;
    let rows: Vec<BookRow> = snapshot
        .resting_asks()
        .map(|o| BookRow {
            price_per_tick: o.price_per_tick,
            max_ticks: o.ticks,
            token_contract: o
                .token_contract
                .as_ref()
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
        })
        .collect();
    if rows.is_empty() {
        let raw_order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
        if raw_order_count > 0 {
            let tick_size = DobParams::canonical().tick_size;
            println!(
                "inference order book -- {}  (1 tick = {tick_size} model tokens)",
                snapshot.frame_model
            );
            println!(
                "  * no executable asks; raw order_count={raw_order_count} is blocked by stale/non-executable rows"
            );
            return Ok(());
        }
    }
    // Read-only discovery: no `--max-price-per-tick` ceiling, so the `exec` column stays blank(this is not a buy).
    print_book_table(&snapshot.frame_model, &rows, None, None);
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_market(_args: MarketArgs) -> Result<()> {
    bail!("market unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
fn selection_error_is_empty_book_state(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("no executable matching ask")
        || reason.contains("no submit-safe ask")
        || reason.contains("best ask price")
        || reason.contains("no resting asks")
        || reason.contains("no matchable ask")
        || reason.contains("raw order-book matcher")
        || reason.contains("refusing multi-ask fill")
}

#[cfg(feature = "shellnet")]
fn render_executable_book_line(
    snapshot: &OrderBookSnapshot,
    order: &OrderBookOrder,
    ticks: u128,
    max_price_per_tick: u128,
) -> String {
    format!(
        "executable_ask model={} order_book={} order_id={} token_contract={} price_per_tick={} ticks={} requested_ticks={} max_price_per_tick={}",
        snapshot.frame_model,
        snapshot.order_book,
        order.order_id,
        order.token_contract.as_deref().unwrap_or("-"),
        order.price_per_tick,
        order.ticks,
        ticks,
        max_price_per_tick
    )
}

#[cfg(feature = "shellnet")]
fn render_no_executable_book_line(
    snapshot: &OrderBookSnapshot,
    ticks: u128,
    max_price_per_tick: u128,
    reason: &str,
) -> String {
    format!(
        "executable_ask model={} order_book={} none=true no_executable_ask=true requested_ticks={} max_price_per_tick={} reason={}",
        snapshot.frame_model,
        snapshot.order_book,
        ticks,
        max_price_per_tick,
        reason.replace('\n', " ")
    )
}

#[cfg(feature = "shellnet")]
fn render_executable_book_output(
    snapshot: &OrderBookSnapshot,
    orders: &[OrderBookOrder],
    ticks: u128,
    max_price_per_tick: u128,
    empty_reason: Option<&str>,
) -> String {
    if orders.is_empty() {
        return render_no_executable_book_line(
            snapshot,
            ticks,
            max_price_per_tick,
            empty_reason.unwrap_or("no executable matching ask"),
        );
    }
    orders
        .iter()
        .map(|order| render_executable_book_line(snapshot, order, ticks, max_price_per_tick))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `dexdo executable-book <model>`: show all currently executable asks for this tick count and ceiling.
/// Rows hidden behind a stale cheaper raw row are intentionally not listed, because the model-wide matcher
/// would hit that unsafe row first.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_executable_book(args: ExecutableBookArgs) -> Result<()> {
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let target = if let Some(market) = args.market.as_deref() {
        if args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --note-addr");
        }
        target_from_market_for_model(market, &args.models, &args.model)?
    } else {
        model_target_from_config(&args.models, &args.model, args.note_addr.clone()).map_err(|e| {
            anyhow::anyhow!("{e}\n(pass --note-addr 0:<your PrivateNote> so the per-model book can be derived)")
        })?
    };
    let (snapshot, orders, empty_reason) =
        direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
            let snapshot = read_book_target(&chain, &target).await?;
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
            match chain
                .submit_safe_executable_book_asks(&snapshot, args.ticks, args.max_price_per_tick)
                .await
            {
                Ok((orders, reason)) => Ok((snapshot, orders, reason)),
                Err(err) if selection_error_is_empty_book_state(&err.to_string()) => {
                    Ok((snapshot, Vec::new(), Some(err.to_string())))
                }
                Err(err) => Err(err),
            }
        })
        .await?;
    println!(
        "{}",
        render_executable_book_output(
            &snapshot,
            &orders,
            args.ticks,
            args.max_price_per_tick,
            empty_reason.as_deref()
        )
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_executable_book(_args: ExecutableBookArgs) -> Result<()> {
    bail!("executable-book unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_quote(args: QuoteArgs) -> Result<()> {
    if args.mock_chain {
        return run_quote_mock(args).await;
    }
    if args.ticks.is_some() == args.budget.is_some() {
        bail!("quote requires exactly one of --ticks or --budget");
    }
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let target = if let Some(market) = args.market.as_deref() {
        if args.model.is_some() || args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --model/--note-addr for quote");
        }
        target_from_market(market)?
    } else {
        model_target_from_config(
            &args.models,
            args.model
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("quote without --market requires --model"))?,
            args.note_addr.clone(),
        )?
    };
    let (snapshot, q) =
        direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
            let snapshot = read_book_target(&chain, &target).await?;
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
            let q = chain
                .submit_safe_single_ask_quote(&snapshot, args.ticks, args.budget)
                .await
                .map_err(|e| anyhow::anyhow!("quote: {e}"))?;
            Ok((snapshot, q))
        })
        .await?;
    if args.json {
        return machine::print_json(&quote_response_from_quote(
            "shellnet",
            &snapshot.frame_model,
            &snapshot.order_book,
            args.ticks,
            args.budget,
            q,
        )?);
    }
    if q.filled_ticks == 0 {
        println!(
            "quote model={} order_book={} no_liquidity=true",
            snapshot.frame_model, snapshot.order_book
        );
        return Ok(());
    }
    println!(
        "quote model={} order_book={} filled_ticks={} total_with_fee={} complete={}",
        snapshot.frame_model, snapshot.order_book, q.filled_ticks, q.total_with_fee, q.complete
    );
    for fill in q.fills {
        println!(
            "fill order_id={} token_contract={} ticks={} price_per_tick={} cost_with_fee={}",
            fill.order_id, fill.token_contract, fill.ticks, fill.price_per_tick, fill.cost_with_fee
        );
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_quote(args: QuoteArgs) -> Result<()> {
    if args.mock_chain {
        return run_quote_mock(args).await;
    }
    bail!("quote unavailable: build with `--features shellnet`")
}

pub(crate) async fn run_market_data(args: MarketDataArgs) -> Result<()> {
    let base_url = indexer::resolve_base_url(args.indexer_url.as_deref())?;
    let timeout = indexer::timeout_from_ms(args.timeout_ms)?;
    let client = IndexerClient::new(base_url, timeout)?;
    match args.command {
        MarketDataCommand::List {
            producer,
            status,
            cursor,
            limit,
        } => {
            let response = client
                .markets(MarketsQuery {
                    inference_order_book_address: None,
                    producer: producer.as_deref(),
                    status: status.as_deref(),
                    cursor: cursor.as_deref(),
                    limit,
                })
                .await?;
            match args.output {
                MarketDataOutput::Table => {
                    print!(
                        "{}",
                        indexer::render_markets_table(&response, client.base_url())
                    );
                }
                MarketDataOutput::Json => {
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            }
        }
        MarketDataCommand::Show {
            inference_order_book_address,
        } => {
            let response = client
                .markets(MarketsQuery {
                    inference_order_book_address: Some(&inference_order_book_address),
                    producer: None,
                    status: None,
                    cursor: None,
                    limit: None,
                })
                .await?;
            let mut markets = response.markets.into_iter();
            let market = markets.next().ok_or_else(|| {
                anyhow::anyhow!(
                    "Dodex indexer returned no market for inferenceOrderBookAddress={}",
                    inference_order_book_address
                )
            })?;
            if markets.next().is_some() {
                bail!(
                    "Dodex indexer returned multiple markets for inferenceOrderBookAddress={}",
                    inference_order_book_address
                );
            }
            match args.output {
                MarketDataOutput::Table => {
                    print!("{}", indexer::render_market_table(&market));
                }
                MarketDataOutput::Json => {
                    println!("{}", serde_json::to_string_pretty(&market)?);
                }
            }
        }
        MarketDataCommand::Depth {
            inference_order_book_address,
            limit,
        } => {
            let response = client
                .depth(DepthQuery {
                    inference_order_book_address: &inference_order_book_address,
                    limit,
                })
                .await?;
            match args.output {
                MarketDataOutput::Table => {
                    print!(
                        "{}",
                        indexer::render_depth_table(&response, client.base_url())
                    );
                }
                MarketDataOutput::Json => {
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            }
        }
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_orders(args: OrdersArgs) -> Result<()> {
    let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
        anyhow::anyhow!("orders requires --note-addr (the owner PrivateNote to filter/cancel)")
    })?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let target = if let Some(market) = args.market.as_deref() {
        if args.model.is_some() {
            bail!("--market and --model are mutually exclusive for orders");
        }
        target_from_market(market)?
    } else {
        model_target_from_config(
            &args.models,
            args.model
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("orders without --market requires --model"))?,
            Some(note_addr.to_string()),
        )?
    };
    let snapshot = direct_chain_read_with_timeout(
        args.read_timeout.read_timeout_secs,
        read_book_target(&chain, &target),
    )
    .await?;
    let own = own_orders(&snapshot, note_addr);
    match args.command {
        OrdersCommand::List => {
            if own.is_empty() {
                println!(
                    "orders model={} order_book={} owner={} none=true",
                    snapshot.frame_model, snapshot.order_book, note_addr
                );
            } else {
                for order in own {
                    println!("{}", render_order_line(order));
                }
            }
        }
        OrdersCommand::Show { order_id } => {
            let order = own
                .into_iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "order {order_id} is not a resting order owned by note {note_addr} in {}",
                        snapshot.order_book
                    )
                })?;
            println!("{}", render_order_line(order));
        }
        OrdersCommand::Cancel { order_id } => {
            let order = own
                .into_iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "refusing to cancel: order {order_id} is not owned by note {note_addr} in {}",
                        snapshot.order_book
                    )
                })?;
            let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "orders cancel requires --note-key to sign the PrivateNote owner method"
                )
            })?;
            let note = dexdo_core::Address::parse(note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let keys = dexdo_core::KeyPair::from_secret_hex(
                read_secret_hex(note_key, "--note-key")?.trim(),
            )
            .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.assert_note_owner_matches("orders cancel", &note, &keys),
            )
            .await?;
            chain
                .cancel_inference_order(&note, &keys, &target.model_hash, order.order_id)
                .await?;
            println!(
                "cancel submitted model={} order_book={} order_id={} owner={}",
                snapshot.frame_model, snapshot.order_book, order.order_id, note_addr
            );
        }
        OrdersCommand::CancelAll => {
            if own.is_empty() {
                bail!(
                    "refusing to cancel-all: note {note_addr} has no resting orders in {}",
                    snapshot.order_book
                );
            }
            let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "orders cancel-all requires --note-key to sign the PrivateNote owner method"
                )
            })?;
            let note = dexdo_core::Address::parse(note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let keys = dexdo_core::KeyPair::from_secret_hex(
                read_secret_hex(note_key, "--note-key")?.trim(),
            )
            .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.assert_note_owner_matches("orders cancel-all", &note, &keys),
            )
            .await?;
            chain
                .cancel_all_inference_orders(&note, &keys, &target.model_hash)
                .await?;
            println!(
                "cancel-all submitted model={} order_book={} owner={} order_count={}",
                snapshot.frame_model,
                snapshot.order_book,
                note_addr,
                own.len()
            );
        }
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_orders(_args: OrdersArgs) -> Result<()> {
    bail!("orders unavailable: build with `--features shellnet`")
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
pub(crate) async fn run_subscription(args: SubscriptionArgs) -> Result<()> {
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
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
            direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.assert_note_owner_matches("subscription place", &note, &keys),
            )
            .await?;
            let plan = subscription_place_plan(place)?;
            let expected_order_id = snapshot
                .stats
                .as_ref()
                .map(|s| s.next_order_id)
                .unwrap_or(0);
            chain
                .place_inference_subscription(
                    &note,
                    &keys,
                    &target.model_hash,
                    place.max_price_per_tick,
                    plan.ticks,
                    plan.escrow,
                    place.auto_renew,
                )
                .await?;
            println!(
                "subscription place submitted model={} order_book={} owner={} expected_order_id={} max_price_per_tick={} ticks={} escrow={} unused_budget={} auto_renew={}",
                snapshot.frame_model,
                snapshot.order_book,
                note_addr,
                expected_order_id,
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

pub(crate) async fn run_deals(args: DealsArgs) -> Result<()> {
    let dir = deals::resolve_deals_dir(args.deals_dir.as_deref())?;
    let handles = deals::list_deal_handles(&dir)?;
    if handles.is_empty() {
        println!("deals dir={} none=true", dir.display());
        return Ok(());
    }
    for (path, h) in handles {
        println!(
            "handle={} role={} network={} note={} model={} token_contract={} order_book={} path={}",
            h.handle,
            h.role.as_str(),
            h.network,
            h.note_addr,
            h.frame_model,
            h.token_contract,
            h.order_book.as_deref().unwrap_or("-"),
            path.display()
        );
    }
    Ok(())
}

pub(crate) async fn run_history(args: HistoryArgs) -> Result<()> {
    let dir = deals::resolve_deals_dir(args.deals_dir.as_deref())?;
    let handles = deals::list_deal_handles(&dir)?;
    let mut shown = 0usize;
    for (path, h) in handles {
        if !audit::history_handle_matches(&h, args.note.as_deref(), args.model.as_deref()) {
            continue;
        }
        shown += 1;
        println!(
            "history handle={} role={} network={} note={} model={} model_hash={} token_contract={} order_book={} created_at={} order_ids={} path={}",
            h.handle,
            h.role.as_str(),
            h.network,
            h.note_addr,
            h.frame_model,
            h.model_hash.as_deref().unwrap_or("-"),
            h.token_contract,
            h.order_book.as_deref().unwrap_or("-"),
            h.created_at_unix,
            if h.created_order_ids.is_empty() {
                "-".to_string()
            } else {
                h.created_order_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            },
            path.display()
        );
    }
    if shown == 0 {
        println!(
            "history dir={} none=true note={} model={}",
            dir.display(),
            args.note.as_deref().unwrap_or("-"),
            args.model.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

pub(crate) async fn run_dashboard(args: DashboardArgs) -> Result<()> {
    dashboard::ensure_loopback(args.listen)?;
    let dir = deals::resolve_deals_dir(args.deals_dir.as_deref())?;
    #[cfg(feature = "shellnet")]
    let state = dashboard::DashboardAppState::shellnet(dir);
    #[cfg(not(feature = "shellnet"))]
    let state = dashboard::DashboardAppState::local(dir);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let addr = dashboard::bind_dashboard(args.listen, state, async move {
        let _ = shutdown_rx.await;
    })
    .await?;
    println!(
        "dashboard_url=http://{addr}/ json=http://{addr}{} read_only=true",
        dashboard::DASHBOARD_JSON_PATH
    );
    operator_shutdown_signal().await;
    let _ = shutdown_tx.send(());
    Ok(())
}

fn role_arg_str(role: DealRoleArg) -> &'static str {
    match role {
        DealRoleArg::Buyer => "buyer",
        DealRoleArg::Seller => "seller",
    }
}

fn handle_role_to_arg(role: deals::DealHandleRole) -> DealRoleArg {
    match role {
        deals::DealHandleRole::Buyer => DealRoleArg::Buyer,
        deals::DealHandleRole::Seller => DealRoleArg::Seller,
    }
}

struct MockDealTarget {
    handle: Option<deals::DealHandle>,
    token_contract: String,
    role: Option<DealRoleArg>,
    note_addr: Option<String>,
    frame_model: Option<String>,
}

fn resolve_mock_deal_target(
    input: &str,
    deals_dir: Option<&std::path::Path>,
    raw_role: Option<DealRoleArg>,
    raw_note_addr: Option<String>,
) -> Result<MockDealTarget> {
    let dir = deals::resolve_deals_dir(deals_dir)?;
    if let Some((_path, handle)) = deals::resolve_deal_ref(input, &dir)? {
        return Ok(MockDealTarget {
            token_contract: handle.token_contract.clone(),
            role: Some(handle_role_to_arg(handle.role)),
            note_addr: Some(handle.note_addr.clone()),
            frame_model: Some(handle.frame_model.clone()),
            handle: Some(handle),
        });
    }
    Ok(MockDealTarget {
        handle: None,
        token_contract: input.to_string(),
        role: raw_role,
        note_addr: raw_note_addr,
        frame_model: None,
    })
}

fn status_next_for(
    role: Option<&str>,
    state: &str,
    funded: bool,
    opened: bool,
    probe_accepted: bool,
) -> machine::StatusNext {
    let action = match (role, state, funded, opened, probe_accepted) {
        (_, "closed", _, _, _) => "none",
        (Some("seller"), "stopped", _, _, _) => "destroy",
        (Some("seller"), _, _, true, false) => "seller_advance_probe_after_timeout",
        (Some("seller"), _, _, true, true) => "seller_advance_or_wait_buyer_stop",
        (Some("seller"), _, true, false, false) => "buyer_cleanup_after_timeout",
        (Some("buyer"), _, _, true, _) => "stream_stop_or_reclaim_after_timeout",
        (Some("buyer"), _, true, false, false) => "cleanup_unopened_after_timeout",
        (Some("buyer"), "stopped", _, _, _) => "seller_destroy",
        (Some("buyer"), _, _, _, _) => "cancel_resting_bid_or_wait_match",
        _ => "unknown_role",
    };
    machine::StatusNext {
        action: action.to_string(),
        retryable_after_unix: None,
        command: if action == "none" {
            "none".to_string()
        } else if action.starts_with("seller_advance") {
            "seller".to_string()
        } else {
            "close".to_string()
        },
    }
}

fn status_response_from_summary(
    network: &str,
    handle: Option<String>,
    role: Option<String>,
    token_contract: String,
    frame_model: Option<String>,
    state: &str,
    active: bool,
    s: &deals::DealStateSummary,
) -> Result<machine::StatusResponse> {
    Ok(machine::StatusResponse {
        schema: machine::STATUS_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        handle,
        role: role.clone(),
        token_contract,
        frame_model,
        state: state.to_string(),
        active,
        funded: s.funded,
        opened: s.opened,
        disputed: s.disputed,
        probe_accepted: s.probe_accepted,
        accounting: machine::StatusAccounting {
            finalized_owed: machine::amount(s.finalized_owed),
            buyer_locked: machine::amount(s.buyer_locked()),
            deposit: machine::amount(s.deposit),
            prepaid: machine::amount(s.prepaid),
            frozen: machine::amount(s.frozen),
            last_advance_unix: Some(s.last_advance).filter(|v| *v != 0),
            funded_time_unix: s.funded_time,
        },
        next: status_next_for(role.as_deref(), state, s.funded, s.opened, s.probe_accepted),
    })
}

fn closed_status_response(
    network: &str,
    handle: Option<String>,
    role: Option<String>,
    token_contract: String,
    frame_model: Option<String>,
) -> Result<machine::StatusResponse> {
    let s = deals::DealStateSummary {
        kind: deals::DealStateKind::Stopped,
        funded: false,
        opened: false,
        disputed: false,
        probe_accepted: false,
        deposit: 0,
        prepaid: 0,
        frozen: 0,
        finalized_owed: 0,
        funded_time: None,
        last_advance: 0,
    };
    status_response_from_summary(
        network,
        handle,
        role,
        token_contract,
        frame_model,
        "closed",
        false,
        &s,
    )
}

fn mock_summary_from_snapshot(snapshot: &dexdo_core::StreamSnapshot) -> deals::DealStateSummary {
    let kind = if snapshot.closed {
        deals::DealStateKind::Stopped
    } else if snapshot.seller_received > 0 {
        deals::DealStateKind::Streaming
    } else {
        deals::DealStateKind::Probe
    };
    deals::DealStateSummary {
        kind,
        funded: !snapshot.closed,
        opened: !snapshot.closed,
        disputed: false,
        probe_accepted: snapshot.seller_received > 0,
        deposit: 0,
        prepaid: 0,
        frozen: u128::from(snapshot.buyer_locked),
        finalized_owed: u128::from(snapshot.seller_received),
        funded_time: None,
        last_advance: 0,
    }
}

async fn run_status_mock(args: StatusArgs) -> Result<()> {
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let target = resolve_mock_deal_target(&args.deal, args.deals_dir.as_deref(), None, None)?;
    let handle = target.handle.as_ref().map(|h| h.handle.clone());
    let role = target.role.map(|r| role_arg_str(r).to_string());
    let frame_model = target.frame_model.clone();
    let snapshot = chain.snapshot(&target.token_contract).await;
    if args.json {
        let response = match snapshot {
            Some(snapshot) if !snapshot.closed => {
                let s = mock_summary_from_snapshot(&snapshot);
                let state = s.kind.as_str();
                status_response_from_summary(
                    "mock",
                    handle,
                    role,
                    target.token_contract,
                    frame_model,
                    state,
                    true,
                    &s,
                )?
            }
            _ => closed_status_response("mock", handle, role, target.token_contract, frame_model)?,
        };
        return machine::print_json(&response);
    }
    match snapshot {
        Some(snapshot) if !snapshot.closed => {
            let s = mock_summary_from_snapshot(&snapshot);
            println!(
                "status handle=(raw) role=unknown token_contract={} state={} active=true funded={} opened={} disputed=false probe_accepted={}",
                target.token_contract,
                s.kind.as_str(),
                s.funded,
                s.opened,
                s.probe_accepted
            );
        }
        _ => println!(
            "status handle=(raw) role=unknown token_contract={} state=closed active=false",
            target.token_contract
        ),
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_status(args: StatusArgs) -> Result<()> {
    if args.mock_chain {
        return run_status_mock(args).await;
    }
    use dexdo_core::{Address, RealChainBackend};
    let target = load_deal_target(&args.deal, args.deals_dir.as_deref(), None, None)?;
    let contracts_path = deal_contracts_path(args.contracts.as_deref(), &target);
    shellnet_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    let contracts = args
        .contracts
        .as_deref()
        .unwrap_or(&contracts_path)
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = Address::parse(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let Some(state) = chain.token_contract_state(&tc).await? else {
        if args.json {
            return machine::print_json(&closed_status_response(
                "shellnet",
                target.handle.as_ref().map(|h| h.handle.clone()),
                target.role.map(|r| r.as_str().to_string()),
                target.token_contract,
                target.handle.as_ref().map(|h| h.frame_model.clone()),
            )?);
        }
        println!(
            "status handle={} role={} token_contract={} state=closed active=false",
            target
                .handle
                .as_ref()
                .map(|h| h.handle.as_str())
                .unwrap_or("(raw)"),
            target.role.map(|r| r.as_str()).unwrap_or("unknown"),
            target.token_contract
        );
        return Ok(());
    };
    let s = deals::classify_deal_state(&state);
    if args.json {
        return machine::print_json(&status_response_from_summary(
            "shellnet",
            target.handle.as_ref().map(|h| h.handle.clone()),
            target.role.map(|r| r.as_str().to_string()),
            target.token_contract.clone(),
            target.handle.as_ref().map(|h| h.frame_model.clone()),
            s.kind.as_str(),
            true,
            &s,
        )?);
    }
    println!(
        "status handle={} role={} token_contract={} state={} active=true funded={} opened={} disputed={} probe_accepted={}",
        target
            .handle
            .as_ref()
            .map(|h| h.handle.as_str())
            .unwrap_or("(raw)"),
        target.role.map(|r| r.as_str()).unwrap_or("unknown"),
        target.token_contract,
        s.kind.as_str(),
        s.funded,
        s.opened,
        s.disputed,
        s.probe_accepted
    );
    if let Some(h) = &target.handle {
        println!(
            "context network={} note={} model={} order_book={} root_model={}",
            h.network,
            h.note_addr,
            h.frame_model,
            h.order_book.as_deref().unwrap_or("-"),
            h.root_model.as_deref().unwrap_or("-")
        );
    }
    println!(
        "accounting finalized_owed={} buyer_locked={} deposit={} prepaid={} frozen={} last_advance={} funded_time={}",
        s.finalized_owed,
        s.buyer_locked(),
        s.deposit,
        s.prepaid,
        s.frozen,
        s.last_advance,
        s.funded_time
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("{}", close_hint(&target, &s));
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_status(args: StatusArgs) -> Result<()> {
    if args.mock_chain {
        return run_status_mock(args).await;
    }
    bail!("status unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_export(args: ExportArgs) -> Result<()> {
    use dexdo_core::{Address, RealChainBackend};
    let target = load_deal_target(&args.deal, args.deals_dir.as_deref(), None, None)?;
    let contracts_path = deal_contracts_path(args.contracts.as_deref(), &target);
    shellnet_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    let contracts = contracts_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = Address::parse(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let state = chain.token_contract_state(&tc).await?;
    let active = state.is_some();
    let summary = state.as_ref().map(deals::classify_deal_state);
    let (onchain_model, onchain_model_hash, onchain_buyer_note, deal_terms) = if active {
        let model = chain.token_contract_model_name(&tc).await?;
        let model_hash = chain.token_contract_model_hash(&tc).await?;
        let buyer_note = chain
            .token_contract_buyer_note(&tc)
            .await?
            .map(|a| a.with_workchain());
        let terms = chain.token_contract_deal_terms(&tc).await?.map(
            |(tick_size, price_per_tick, max_ticks)| audit::DealTermsAudit {
                tick_size,
                price_per_tick,
                max_ticks,
            },
        );
        (model, model_hash, buyer_note, terms)
    } else {
        (None, None, None, None)
    };
    let generated_at_unix = deals::now_unix()?;
    let export = audit::build_deal_audit(audit::DealAuditBuild {
        generated_at_unix,
        handle: target.handle.clone(),
        role: target.role,
        token_contract: target.token_contract.clone(),
        note_addr: target.note_addr.clone(),
        contracts: contracts_path.display().to_string(),
        active,
        state,
        summary,
        onchain_model,
        onchain_model_hash,
        onchain_buyer_note,
        deal_terms,
    });
    match args.format {
        ExportFormatArg::Json => println!("{}", serde_json::to_string_pretty(&export)?),
        ExportFormatArg::Md => print!("{}", audit::render_markdown(&export)),
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_export(_args: ExportArgs) -> Result<()> {
    bail!("export unavailable: build with `--features shellnet`")
}

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
        tx: None,
    })
}

async fn run_close_mock(args: CloseArgs) -> Result<()> {
    let target = resolve_mock_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let role = target.role.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --role buyer|seller with a raw TokenContract",
            args.deal
        )
    })?;
    if target.note_addr.is_none() {
        bail!(
            "close: `{}` is not a local handle; pass --note-addr with a raw TokenContract",
            args.deal
        );
    }
    let role_s = role_arg_str(role);
    let handle = target.handle.as_ref().map(|h| h.handle.clone());
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let snapshot = chain.snapshot(&target.token_contract).await;
    match snapshot {
        None => {
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_closed"),
                "closed",
                "closed",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: TokenContract {} is inactive/closed",
                response.token_contract
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
                false,
                Some("already_stopped"),
                "stopped",
                "stopped",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: {} side already STOPped for {}",
                role_s, response.token_contract
            );
            Ok(())
        }
        Some(snapshot) => {
            if role != DealRoleArg::Buyer {
                bail!(
                    "close: seller cannot destroy opened deal {}. Buyer must STOP/recover/reclaim first.",
                    target.token_contract
                );
            }
            let state_before = if snapshot.seller_received > 0 {
                "streaming"
            } else {
                "probe"
            };
            let note = dexdo_core::LocalNote::generate();
            chain.stop(&target.token_contract, &note).await?;
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "streamStop",
                true,
                false,
                None,
                state_before,
                "stopped",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close submitted role=buyer action=streamStop token_contract={}",
                response.token_contract
            );
            Ok(())
        }
    }
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_close(args: CloseArgs) -> Result<()> {
    if args.mock_chain {
        return run_close_mock(args).await;
    }
    use dexdo_core::{
        check_reclaimable, check_recoverable, keypair_ed_pubkey, Address, KeyPair,
        RealChainBackend, MATCH_OPEN_TIMEOUT_SECS,
    };
    let target = load_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let role = target.role.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --role buyer|seller with a raw TokenContract",
            args.deal
        )
    })?;
    let note_addr = target.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --note-addr with a raw TokenContract",
            args.deal
        )
    })?;
    if let (Some(handle), Some(arg_note)) = (&target.handle, args.note_addr.as_deref()) {
        if deals::normalize_addr(&handle.note_addr) != deals::normalize_addr(arg_note) {
            bail!(
                "close: --note-addr {arg_note} does not match handle {} note {}",
                handle.handle,
                handle.note_addr
            );
        }
    }
    let contracts_path = deal_contracts_path(args.contracts.as_deref(), &target);
    shellnet_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    let contracts = args
        .contracts
        .as_deref()
        .unwrap_or(&contracts_path)
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = Address::parse(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let Some(state) = chain.token_contract_state(&tc).await? else {
        if args.json {
            return machine::print_json(&close_response(
                "shellnet",
                target.handle.as_ref().map(|h| h.handle.clone()),
                role.as_str(),
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_closed"),
                "closed",
                "closed",
            )?);
        }
        println!(
            "close noop: TokenContract {} is inactive/closed",
            target.token_contract
        );
        return Ok(());
    };
    let s = deals::classify_deal_state(&state);
    match role {
        deals::DealHandleRole::Seller => {
            if s.disputed {
                bail!(
                    "close: seller deal {} is disputed; seller-side release is tracked by . Next command \
                     once exposed: `dexdo release-dispute {}`.",
                    target.token_contract,
                    target
                        .handle
                        .as_ref()
                        .map(|h| h.handle.as_str())
                        .unwrap_or(&target.token_contract)
                );
            }
            if s.opened {
                bail!(
                    "close: seller cannot destroy opened deal {}. {}",
                    target.token_contract,
                    close_hint(&target, &s)
                );
            }
            if s.kind != deals::DealStateKind::Stopped {
                bail!("{}", close_hint(&target, &s));
            }
            let note_key = args.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!("close seller requires --note-key to sign destroy")
            })?;
            let keys = KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            let note = Address::parse(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            chain.destroy_token_contract(&tc, &note, &keys).await?;
            if args.json {
                return machine::print_json(&close_response(
                    "shellnet",
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
                target.token_contract, note
            );
        }
        deals::DealHandleRole::Buyer => {
            if s.disputed {
                bail!(
                    "close: buyer deal {} is disputed; wait for seller release/arbitration (), then re-run status.",
                    target.token_contract
                );
            }
            if s.kind == deals::DealStateKind::Stopped {
                if args.json {
                    return machine::print_json(&close_response(
                        "shellnet",
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "noop",
                        false,
                        false,
                        Some("already_stopped"),
                        "stopped",
                        "stopped",
                    )?);
                }
                println!(
                    "close noop: buyer side already STOPped for {}. Next: seller runs `dexdo close <seller-handle> --note-key <seller-key>`.",
                    target.token_contract
                );
                return Ok(());
            }
            let note_key = args.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!("close buyer requires --note-key to sign note owner method")
            })?;
            let keys = KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            let note = Address::parse(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let buyer_note = chain.token_contract_buyer_note(&tc).await?;
            let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
            let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
            let note_ed = keypair_ed_pubkey(&keys)?;
            if s.opened {
                let cfg = chain.token_contract_config(&tc).await?.ok_or_else(|| {
                    anyhow::anyhow!("close: TokenContract {} getConfig unavailable", tc)
                })?;
                let stream_timeout = cfg["streamTimeout"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| anyhow::anyhow!("close: getConfig exposes no streamTimeout"))?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
                    .as_secs();
                match buyer_opened_close_action(now, s.last_advance, stream_timeout) {
                    BuyerOpenedCloseAction::StreamReclaim => {
                        check_reclaimable(
                            s.funded,
                            s.opened,
                            s.disputed,
                            buyer_note_s.as_deref(),
                            &note.with_workchain(),
                            buyer_pubkey.as_ref(),
                            &note_ed,
                            now,
                            s.last_advance,
                            Some(stream_timeout),
                            s.funded_time,
                            MATCH_OPEN_TIMEOUT_SECS,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                        chain.reclaim_on_timeout(&note, &keys, &tc).await?;
                        if args.json {
                            return machine::print_json(&close_response(
                                "shellnet",
                                target.handle.as_ref().map(|h| h.handle.clone()),
                                role.as_str(),
                                target.token_contract.clone(),
                                "streamReclaim",
                                true,
                                false,
                                None,
                                s.kind.as_str(),
                                "stopped",
                            )?);
                        }
                        println!(
                            "close submitted role=buyer action=streamReclaim token_contract={} note={}",
                            target.token_contract, note
                        );
                    }
                    BuyerOpenedCloseAction::StreamStop => {
                        check_recoverable(
                            s.opened,
                            s.disputed,
                            buyer_note_s.as_deref(),
                            &note.with_workchain(),
                            buyer_pubkey.as_ref(),
                            &note_ed,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                        chain.stream_stop(&note, &keys, &tc).await?;
                        if args.json {
                            return machine::print_json(&close_response(
                                "shellnet",
                                target.handle.as_ref().map(|h| h.handle.clone()),
                                role.as_str(),
                                target.token_contract.clone(),
                                "streamStop",
                                true,
                                false,
                                None,
                                s.kind.as_str(),
                                "stopped",
                            )?);
                        }
                        println!(
                            "close submitted role=buyer action=streamStop token_contract={} note={}",
                            target.token_contract, note
                        );
                    }
                }
                return Ok(());
            }
            if s.funded && !s.probe_accepted {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
                    .as_secs();
                check_reclaimable(
                    s.funded,
                    s.opened,
                    s.disputed,
                    buyer_note_s.as_deref(),
                    &note.with_workchain(),
                    buyer_pubkey.as_ref(),
                    &note_ed,
                    now,
                    s.last_advance,
                    None,
                    s.funded_time,
                    MATCH_OPEN_TIMEOUT_SECS,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{e}. Next: re-run `dexdo close {}` after MATCH_OPEN_TIMEOUT, or inspect with `dexdo status {}`.",
                        args.deal,
                        args.deal
                    )
                })?;
                chain.stream_cleanup(&note, &keys, &tc).await?;
                if args.json {
                    return machine::print_json(&close_response(
                        "shellnet",
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "streamCleanup",
                        true,
                        false,
                        None,
                        s.kind.as_str(),
                        "stopped",
                    )?);
                }
                println!(
                    "close submitted role=buyer action=streamCleanup token_contract={} note={}",
                    target.token_contract, note
                );
                return Ok(());
            }
            bail!("{}", close_hint(&target, &s));
        }
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_close(args: CloseArgs) -> Result<()> {
    if args.mock_chain {
        return run_close_mock(args).await;
    }
    bail!("close unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuyerOpenedCloseAction {
    StreamStop,
    StreamReclaim,
}

#[cfg(feature = "shellnet")]
fn buyer_opened_close_action(
    now: u64,
    last_advance: u64,
    stream_timeout: u64,
) -> BuyerOpenedCloseAction {
    if now >= last_advance.saturating_add(stream_timeout) {
        BuyerOpenedCloseAction::StreamReclaim
    } else {
        BuyerOpenedCloseAction::StreamStop
    }
}

#[cfg(feature = "shellnet")]
fn close_hint(target: &DealTarget, s: &deals::DealStateSummary) -> String {
    let deal = target
        .handle
        .as_ref()
        .map(|h| h.handle.as_str())
        .unwrap_or(&target.token_contract);
    match target.role {
        Some(deals::DealHandleRole::Seller) if s.kind == deals::DealStateKind::Stopped => {
            format!("next=destroy command=`dexdo close {deal} --note-key <seller-key>`")
        }
        Some(deals::DealHandleRole::Seller) if s.opened && !s.probe_accepted => {
            format!(
                "next=seller_advance_probe_after_timeout command=`keep dexdo seller running for {deal}; it calls TokenContract.advance() after PROBE_WINDOW` reason=buyer_silent_probe"
            )
        }
        Some(deals::DealHandleRole::Seller) if s.opened => {
            format!(
                "next=seller_advance_or_wait_buyer_stop command=`keep dexdo seller running for {deal}`; buyer may STOP when done"
            )
        }
        Some(deals::DealHandleRole::Seller) if s.funded && !s.probe_accepted => {
            "next=buyer_cleanup_after_timeout command=`dexdo close <buyer-handle> --note-key <buyer-key>`"
                .to_string()
        }
        Some(deals::DealHandleRole::Seller) => {
            "next=no_destroy_yet reason=deal_not_stopped".to_string()
        }
        Some(deals::DealHandleRole::Buyer) if s.opened => format!(
            "next=stream_stop_or_reclaim_after_timeout command=`dexdo close {deal} --note-key <buyer-key>`"
        ),
        Some(deals::DealHandleRole::Buyer) if s.funded && !s.probe_accepted => {
            format!("next=cleanup_unopened_after_timeout command=`dexdo close {deal} --note-key <buyer-key>`")
        }
        Some(deals::DealHandleRole::Buyer) if s.kind == deals::DealStateKind::Stopped => {
            "next=seller_destroy reason=buyer_already_stopped".to_string()
        }
        Some(deals::DealHandleRole::Buyer) => {
            "next=cancel_resting_bid_or_wait_match reason=deal_not_funded".to_string()
        }
        None => "next=unknown_role pass_local_handle_or_--role".to_string(),
    }
}

fn enforce_seller_runtime_policy(policy: &policy::SellerRuntimePolicy) -> Result<()> {
    if policy.max_open_deals != 1 {
        bail!(
            "policy_action failure_class=seller.max_open_deals action=enforce token_contract=<not-posted> \
             state=pre_offer result=unsupported_max_open_deals requested={} supported=1; \
             current seller daemon owns exactly one per-deal TokenContract",
            policy.max_open_deals
        );
    }
    let mut unsupported = Vec::new();
    match policy.after_deal_done {
        policy::SellerAfterDealDoneAction::Retire => {}
        policy::SellerAfterDealDoneAction::Republish => {
            unsupported.push("seller.on.after_deal_done=republish");
        }
        policy::SellerAfterDealDoneAction::RepublishWithBackoff => {
            unsupported.push("seller.on.after_deal_done=republish_with_backoff");
        }
    }
    match policy.buyer_no_show {
        policy::SellerBuyerNoShowAction::CleanupAndRepublish => {
            unsupported.push("seller.on.buyer_no_show=cleanup_and_republish");
        }
        policy::SellerBuyerNoShowAction::CleanupAndRetire => {
            unsupported.push("seller.on.buyer_no_show=cleanup_and_retire");
        }
        policy::SellerBuyerNoShowAction::RetireGateway => {}
    }
    if !unsupported.is_empty() {
        bail!(
            "policy_action failure_class=policy_validation action=fail_closed token_contract=<not-posted> \
             state=pre_offer result=unsupported_policy_choice runtime=seller unsupported_choices={} \
             next_action=edit_policy diagnostic=seller runtime cannot execute fresh-TC republish or \
             buyer-side cleanup_unopened from this seller daemon before/following an offer; supported seller \
             terminal actions today are seller.on.after_deal_done=retire and \
             seller.on.buyer_no_show=retire_gateway",
            unsupported.join(",")
        );
    }
    Ok(())
}

async fn apply_seller_dispute_policy(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    policy: &policy::SellerRuntimePolicy,
    reason: &str,
) -> Result<bool> {
    let Some(state) = chain.deal_state(token_contract).await? else {
        return Ok(false);
    };
    if !state.disputed {
        return Ok(false);
    }
    match policy.dispute_against_me {
        policy::SellerDisputeAgainstMeAction::ReleaseIfClean => {
            let settlement = chain.release_dispute(token_contract).await?;
            println!(
                "policy_action failure_class=dispute_against_me action=release_if_clean \
                 token_contract={token_contract} state=funded/opened/disputed result=release_dispute_submitted \
                 reason={reason} settlement={settlement:?}"
            );
            Ok(true)
        }
        policy::SellerDisputeAgainstMeAction::Hold => {
            bail!(
                "policy_action failure_class=dispute_against_me action=hold token_contract={token_contract} \
                 state=funded/opened/disputed result=no_release_submitted reason={reason}"
            );
        }
    }
}

#[derive(Debug)]
enum SellerTerminalPolicyOutcome {
    StopServing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AdvanceFailureDisposition {
    BenignTerminal { reason: String },
    Fault { reason: String },
}

fn is_err_not_open(error: &ChainError) -> bool {
    match error {
        ChainError::Chain(msg) => {
            msg.contains("airegistry::ERR_NOT_OPEN") || msg.contains("exit_code=320")
        }
        _ => false,
    }
}

async fn classify_by_fact_advance_failure(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    error: &ChainError,
) -> Result<AdvanceFailureDisposition> {
    if !is_err_not_open(error) {
        return Ok(AdvanceFailureDisposition::Fault {
            reason: "reason=not_err_not_open".to_string(),
        });
    }

    let state = chain.deal_state(token_contract).await?.ok_or_else(|| {
        anyhow!("reason=state_unavailable cannot prove ERR_NOT_OPEN is terminal/no-money")
    })?;
    if state.opened || state.probe_accepted || state.disputed {
        return Ok(AdvanceFailureDisposition::Fault {
            reason: format!(
                "reason=unsafe_lifecycle funded={} opened={} probe_accepted={} disputed={}",
                state.funded, state.opened, state.probe_accepted, state.disputed
            ),
        });
    }

    let snapshot = chain.snapshot(token_contract).await.ok_or_else(|| {
        anyhow!("reason=snapshot_unavailable cannot prove ERR_NOT_OPEN has no locked/owed money")
    })?;
    if snapshot.buyer_locked != 0
        || snapshot.seller_locked != 0
        || snapshot.buyer_lead != 0
        || snapshot.seller_received != 0
        || snapshot.burned != 0
    {
        return Ok(AdvanceFailureDisposition::Fault {
            reason: format!(
                "reason=money_or_locks_present buyer_locked={} buyer_lead={} seller_locked={} \
                 finalized_owed={} burned={}",
                snapshot.buyer_locked,
                snapshot.buyer_lead,
                snapshot.seller_locked,
                snapshot.seller_received,
                snapshot.burned
            ),
        });
    }

    Ok(AdvanceFailureDisposition::BenignTerminal {
        reason: format!(
            "reason=err_not_open_unopened_no_money funded={} opened={} probe_accepted={} disputed={} \
             buyer_locked={} buyer_lead={} seller_locked={} finalized_owed={} burned={}",
            state.funded,
            state.opened,
            state.probe_accepted,
            state.disputed,
            snapshot.buyer_locked,
            snapshot.buyer_lead,
            snapshot.seller_locked,
            snapshot.seller_received,
            snapshot.burned
        ),
    })
}

fn apply_seller_terminal_policy(
    token_contract: &dexdo_core::TokenContract,
    policy: &policy::SellerRuntimePolicy,
    finalized: u128,
) -> Result<SellerTerminalPolicyOutcome> {
    if finalized == 0 {
        match policy.buyer_no_show {
            policy::SellerBuyerNoShowAction::CleanupAndRepublish => {
                bail!(
                    "policy_action failure_class=buyer_no_show action=cleanup_and_republish \
                     token_contract={token_contract} state=funded/opened result=policy_action_unsupported; \
                     seller runtime has no buyer-side cleanup_unopened signer or fresh TC/nonce republish factory"
                );
            }
            policy::SellerBuyerNoShowAction::CleanupAndRetire => {
                bail!(
                    "policy_action failure_class=buyer_no_show action=cleanup_and_retire \
                     token_contract={token_contract} state=funded/opened result=policy_action_unsupported; \
                     cleanup_unopened is buyer-side and was not submitted by seller"
                );
            }
            policy::SellerBuyerNoShowAction::RetireGateway => {
                println!(
                    "policy_action failure_class=buyer_no_show action=retire_gateway \
                     token_contract={token_contract} state=closed result=retiring_gateway finalized_ticks=0; \
                     no cleanup_unopened submitted by seller"
                );
                return Ok(SellerTerminalPolicyOutcome::StopServing);
            }
        }
    }
    match policy.after_deal_done {
        policy::SellerAfterDealDoneAction::Retire => {
            println!(
                "policy_action failure_class=after_deal_done action=retire token_contract={token_contract} \
                 state=closed result=retiring_gateway finalized_ticks={finalized}"
            );
            Ok(SellerTerminalPolicyOutcome::StopServing)
        }
        policy::SellerAfterDealDoneAction::Republish => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish token_contract={token_contract} \
                 state=closed result=policy_action_unsupported finalized_ticks={finalized}; \
                 current seller runtime cannot safely republish without a fresh per-deal TC/nonce"
            );
        }
        policy::SellerAfterDealDoneAction::RepublishWithBackoff => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish_with_backoff \
                 token_contract={token_contract} state=closed result=policy_action_unsupported \
                 finalized_ticks={finalized}; current seller runtime cannot safely republish without a fresh \
                 per-deal TC/nonce"
            );
        }
    }
}

pub(crate) async fn run_seller(args: SellerArgs) -> Result<()> {
    // Issue: the deal token_contract comes from `--market`(a provision manifest) or `--token-contract`.
    // The manifest's frame_model(if any) is validated against `--model` inside `seller_real_backend`.
    let (token_contract, market_frame_model, market_nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    // Review: the deal nonce comes from `--market`(the manifest) or the explicit `--nonce` flag --
    // never both(the manifest is the single source of truth). The real-shellnet seller path requires
    // it(see `seller_real_backend`); the mock path ignores it.
    if args.market.is_some() && args.nonce.is_some() {
        bail!("--market and --nonce are mutually exclusive -- the nonce comes from the manifest");
    }
    let seller_policy = if !args.mock.mock_chain {
        Some(policy::load_seller_runtime_policy(args.policy.as_deref())?)
    } else {
        None
    };
    if let Some(policy) = seller_policy.as_ref() {
        tracing::debug!(
            policy_after_deal_done = policy.after_deal_done.as_str(),
            policy_buyer_no_show = policy.buyer_no_show.as_str(),
            policy_dispute_against_me = policy.dispute_against_me.as_str(),
            policy_max_open_deals = policy.max_open_deals,
            "seller policy loaded"
        );
        enforce_seller_runtime_policy(policy)?;
    }
    // on the real path, the --market manifest's seller_note must be this seller's --note-addr -- else the
    // offer posts a non-canonical TC the InferenceOrderBook won't rest, and the seller never matches.
    if !args.mock.mock_chain {
        if let (Some(market), Some(note_addr)) =
            (args.market.as_deref(), args.identity.note_addr.as_deref())
        {
            let manifest = load_market(market)?;
            assert_market_seller_note(&manifest.seller_note, note_addr)?;
        }
        shellnet_doctor_preflight(&args.contracts, args.market.as_deref()).await?;
        if let Some(policy) = load_enabled_model_registry_policy(
            RegistryRole::Seller,
            &args.registry,
            &args.contracts,
        )? {
            let name = args
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: set --model <name from config> (needed for model registry validation)"
                    )
                })?;
            let frame_model = dexdo::seller::ModelsConfig::load(&args.models)?
                .get(name)?
                .frame_model
                .clone();
            dexdo_core::validate_canonical_model_id(&frame_model)
                .map_err(|e| anyhow::anyhow!(e))?;
            check_market_model_match(market_frame_model.as_deref(), &frame_model, name)?;
            let expected_order_book = if let Some(market) = args.market.as_deref() {
                load_market(market)?.inference_order_book
            } else {
                let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: --note-addr is required to derive the seller order book"
                    )
                })?;
                expected_order_book_for_note(&args.contracts, note_addr, &frame_model).await?
            };
            let order_book_active =
                order_book_active_from_contracts(&args.contracts, &expected_order_book).await?;
            enforce_model_registry_policy(
                RegistryRole::Seller,
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
    let deal_nonce = market_nonce.or(args.nonce);
    // Upstream(model) and chain are selected independently: `--mock-model` -> mock upstream,
    // otherwise a real model from the config; `--mock-chain` -> mock chain, otherwise real shellnet
    // (per-role backend behind the feature).
    let upstream = if args.mock.mock_model {
        dexdo::seller::UpstreamConfig::Mock
    } else {
        let name = args
            .model
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "set --model <name from config> (or --mock-model for a mock upstream)"
                )
            })?;
        let models = dexdo::seller::ModelsConfig::load(&args.models)?;
        let mc = models.get(name)?;
        mc.require_api_key_present()?;
        dexdo::seller::UpstreamConfig::OpenAi(dexdo::seller::OpenAiConfig::from_model(mc))
    };
    let seller_frame_model_for_handle = if args.mock.mock_chain {
        None
    } else {
        let name = args
            .model
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "real shellnet: set --model <name from config> (needed for deal handle)"
                )
            })?;
        Some(
            dexdo::seller::ModelsConfig::load(&args.models)?
                .get(name)?
                .frame_model
                .clone(),
        )
    };
    let (chain, note) = if args.mock.mock_chain {
        let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
        mock_chain_and_note(endpoints_file, &args.identity)?
    } else {
        seller_real_backend(&args, market_frame_model.as_deref(), deal_nonce)?
    };
    // the seller daemon publishes offers WITHOUT going through `provision_market`'s note-current gate, so
    // a note orphaned by a contract redeploy(stale code_hash) would hit a raw `TVM_ERROR` from `postSellOffer`.
    // Gate here: fail closed with an actionable "re-mint" message before any seller-chain read/write path.
    chain.assert_note_current().await?;
    // a withdrawn PrivateNote is final for seller writes. Fail before even reading per-deal TC terms, so a
    // withdrawn note surfaces the fresh-note action instead of any later TC/postSellOffer error.
    chain.assert_note_can_post_sell_offer().await?;
    // Real-shellnet offer terms are bound to the deployed per-deal TokenContract, not prompt/default values.
    // `deploy-market` creates only the shared model book; `dexdo provision` creates the per-deal TC carrying
    // price/maxTicks. The mock path keeps the prior fixed defaults.
    let (offer_ticks, offer_price) = if args.mock.mock_chain {
        (1024u64, args.price_per_tick)
    } else {
        let (price, ticks) = chain
            .sell_offer_terms(&token_contract)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "seller requires a deployed per-deal TokenContract; `dexdo deploy-market` only deploys \
                     the shared model order book. Run `dexdo provision --frame-model ... --nonce ...` and pass \
                     its --market manifest, or pass --token-contract plus --nonce for an already-provisioned TC."
                )
            })?;
        println!(
            "posting offer: {ticks} ticks (= {} model tokens) at {price} SHELL/tick",
            (ticks as u128).saturating_mul(DobParams::canonical().tick_size as u128)
        );
        (ticks, price)
    };
    let gateway_advertise = args.gateway_advertise_addr();
    let cfg = dexdo::seller::SellerConfig {
        token_contract: token_contract.clone(),
        price_per_tick: offer_price,
        max_ticks: offer_ticks,
        gateway_advertise: gateway_advertise.clone(),
        mock_token_count: args.mock_token_count,
    };
    // Resume path: a matched buyer can fund this per-deal TC while no seller process was live (the deal ends up
    // `funded-but-never-opened`). Because a `(sellerPubkey, nonce)` TC is single-use, re-posting the offer would
    // fail -- but the stream can still be opened. This pre-offer probe MUST be non-blocking: fresh normal
    // sellers must post their ask immediately, while `read_match` remains the later wait-loop after the ask rests.
    let already_matched = match chain.read_openable_match_now(&token_contract).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            return Err(anyhow!(
                "seller: existing-match resume preflight failed for {token_contract}: {e}"
            ));
        }
    };
    if already_matched {
        tracing::info!(
            token_contract = %token_contract,
            "seller: TC already funded by a matched buyer (funded-but-never-opened) -- resuming: skipping offer post, opening stream"
        );
    } else {
        // a deterministic per-deal TC(sellerPubkey + nonce) is single-use. If a prior deal already used this
        // nonce's TC(opened/funded/disputed/residual), the seller's pre-stream steps revert with a raw `TVM_ERROR`
        // (ERR_ALREADY_OPEN 321). Fail closed BEFORE post_offer with an actionable "fresh --nonce / recover+destroy"
        // message(the mock backend no-ops; a fresh active-but-unfunded TC passes).
        chain.assert_token_contract_fresh(&token_contract).await?;
        chain.assert_no_active_sell_order(&token_contract).await?;
        tracing::info!(token_contract = %token_contract, "seller posting offer, awaiting buy + match");
        dexdo::seller::post_offer_with_note(note.as_ref(), chain.as_ref(), &cfg).await?;
        // confirm the accepted postSellOffer reached an IOB-acceptable outcome before waiting for a
        // match. In a shared book the ask may match immediately, so the guard accepts either THIS TC's resting ask
        // or THIS TC's funded/openable/fill-event evidence. Start the TCP gateway only after this guard passes;
        // external supervisors must not interpret an open port as sell-ready while this deal is absent from the
        // book and also not matched.
        chain.assert_offer_rested(&token_contract).await?;
    }
    let seller =
        dexdo::seller::start_gateway_with_note(args.gateway_listen, upstream, note).await?;
    println!(
        "seller_ready token_contract={} gateway={} gateway_listen={} readiness={}",
        token_contract,
        gateway_advertise,
        args.gateway_listen,
        if already_matched {
            "resumed_funded_tc"
        } else {
            "exact_tc_offer_accepted"
        }
    );
    let _ = std::io::stdout().flush();
    // match wait + access-handover provisioning belong to the long-running gateway path, not the
    // one-shot seller post flow. The watcher polls the note/fill source(or mock equivalent) with a durable
    // cursor and waits indefinitely while the offer is open; no 300s seller deadline tears down a resting ask.
    let watch = dexdo::seller::SellerMatchWatchConfig {
        cursor_path: seller_watch_cursor_path(args.deals_dir.as_deref(), &token_contract)?,
        poll_interval: dexdo::seller::DEFAULT_MATCH_POLL_INTERVAL,
    };
    let matched =
        dexdo::seller::watch_and_serve_match(&seller, chain.as_ref(), &cfg, &watch).await?;
    println!(
        "seller_match_opened token_contract={} gateway={} gateway_listen={} cursor={}",
        matched.token_contract,
        gateway_advertise,
        args.gateway_listen,
        watch.cursor_path.display()
    );
    let _ = std::io::stdout().flush();
    if !args.mock.mock_chain {
        let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-addr is required to save the deal handle")
        })?;
        save_runtime_deal_handle(
            RuntimeDealHandleInput {
                role: deals::DealHandleRole::Seller,
                deals_dir: args.deals_dir.as_deref(),
                token_contract: &token_contract,
                note_addr,
                frame_model: seller_frame_model_for_handle.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("real shellnet: missing frame_model for deal handle")
                })?,
                market_path: args.market.as_deref(),
                contracts: &args.contracts,
                endpoint: Some(deals::DealEndpointInfo {
                    kind: "gateway".to_string(),
                    value: gateway_advertise.clone(),
                }),
            },
            true,
        )?;
    }
    if let Some(policy) = seller_policy.as_ref() {
        if apply_seller_dispute_policy(chain.as_ref(), &token_contract, policy, "pre-advance")
            .await?
        {
            return Ok(());
        }
    }
    // on the shipped real-money path, drive the seller's by-fact advance. Both safety
    // prerequisites the lead required are met: `drive_advance` is **delivery-bounded** (finalized
    // ticks <= the gateway's delivered canonical-token count, `seller.state.delivery(tc)`, with a merged
    // regression) and it exits on `deal_closed()`. The buyer session-scoped STOP keeps the deal alive
    // across requests so the probe is accepted and ticks finalize by-fact(`AmicableSplit`, no `BurnBoth`).
    // Real-chain only -- the mock chain has no `getConfig` advance window.
    // Two money-path requirements:
    // 1. The stream-phase cadence is `getConfig().settleWindow`; a getter failure must NOT become a silent
    // wrong cadence(advancing too early -> the contract rejects the tick -> the loop dies). Read it
    // FAIL-LOUD before spawning, with TC context -- no default cadence on the real path.
    // 2. `drive_advance` propagates real advance failures as money-path faults. So the task is
    // SUPERVISED, not fire-and-forget: an `Err` is propagated out of `run_seller` (non-zero exit -- by-fact
    // settlement is dead, the gateway must not keep serving as if healthy). Only clean terminals
    // (`Ok(finalized)` / `deal_closed`) are logged and let the gateway serve until shutdown.
    let advance_task = if !args.mock.mock_chain {
        let delivery = seller.state.delivery(&token_contract);
        let settle = chain.deal_settle_window(&token_contract).await.map_err(|e| {
            anyhow::anyhow!(
                "--token-contract {token_contract}: getConfig().settleWindow is unreadable, refusing to \
                 start by-fact advance on a guessed cadence: {e}"
            )
        })?;
        let windows = dexdo::seller::AdvanceWindows::from_settle_window(settle);
        let advance_chain = chain.clone();
        let advance_note = seller.note.clone();
        let advance_tc = token_contract.clone();
        let tick_budget = cfg.max_ticks as u128;
        let tick_size = dexdo_core::DobParams::canonical().tick_size;
        Some(tokio::spawn(async move {
            dexdo::seller::drive_advance(
                advance_chain.as_ref(),
                &advance_tc,
                advance_note.as_ref(),
                windows,
                tick_budget,
                tick_size,
                delivery.count,
                delivery.done,
            )
            .await
        }))
    } else {
        None
    };
    tracing::info!("stream open; serving until shutdown");
    let mut server_task = seller.server_task;
    match advance_task {
        // Supervise: whichever of {by-fact advance, gateway server} ends first decides the exit.
        Some(advance_task) => {
            tokio::select! {
                advanced = advance_task => match advanced {
                    Ok(Ok(finalized)) => {
                        tracing::info!(
                            token_contract = %token_contract, finalized,
                            "drive_advance: finalized ticks by-fact (<= delivered), deal closed; serving until shutdown"
                        );
                        if let Some(policy) = seller_policy.as_ref() {
                            match apply_seller_terminal_policy(&token_contract, policy, finalized)? {
                                SellerTerminalPolicyOutcome::StopServing => {
                                    server_task.abort();
                                    return Ok(());
                                }
                            }
                        }
                        server_task.await?;
                    }
                    Ok(Err(e)) => {
                        if is_err_not_open(&e) {
                            match classify_by_fact_advance_failure(
                                chain.as_ref(),
                                &token_contract,
                                &e,
                            )
                            .await
                            {
                                Ok(AdvanceFailureDisposition::BenignTerminal { reason }) => {
                                    tracing::info!(
                                        token_contract = %token_contract,
                                        %reason,
                                        "drive_advance: ERR_NOT_OPEN is terminal for this unopened/no-money deal"
                                    );
                                    println!(
                                        "by_fact_advance_terminal token_contract={token_contract} \
                                         action=retire_gateway {reason}"
                                    );
                                    server_task.abort();
                                    return Ok(());
                                }
                                Ok(AdvanceFailureDisposition::Fault { reason }) => {
                                    return Err(anyhow::anyhow!(
                                        "--token-contract {token_contract}: by-fact advance failed \
                                         (money-path fault), stopping the seller: {e}; ERR_NOT_OPEN \
                                         terminal check: {reason}"
                                    ));
                                }
                                Err(classify_err) => {
                                    return Err(anyhow::anyhow!(
                                        "--token-contract {token_contract}: by-fact advance failed \
                                         (money-path fault), stopping the seller: {e}; ERR_NOT_OPEN \
                                         terminal check: reason=terminal_classification_failed \
                                         error={classify_err}"
                                    ));
                                }
                            }
                        }
                        if let Some(policy) = seller_policy.as_ref() {
                            if apply_seller_dispute_policy(
                                chain.as_ref(),
                                &token_contract,
                                policy,
                                "advance-error",
                            )
                            .await?
                            {
                                server_task.abort();
                                return Ok(());
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "--token-contract {token_contract}: by-fact advance failed (money-path fault), \
                             stopping the seller: {e}"
                        ));
                    }
                    Err(join) => {
                        return Err(anyhow::anyhow!(
                            "--token-contract {token_contract}: by-fact advance task panicked: {join}"
                        ));
                    }
                },
                served = &mut server_task => served?,
            }
        }
        None => server_task.await?,
    }
    Ok(())
}

/// One resting ask as the order-book renderer needs it: price per tick, its max ticks, and the full deal
/// `TokenContract` address. Kept minimal so both the buyer's pre-buy view and the read-only `markets --table`
/// view can build it from their own sources(`discover_offers` / `OrderBookSnapshot::resting_asks`).
pub struct BookRow {
    pub price_per_tick: u128,
    pub max_ticks: u128,
    pub token_contract: String,
}

/// Render a per-model inference order book to the terminal as a narrow box table (/ UX:
/// "choose a model = choose the market"). Public + read-only: given the resting asks, it prints the
/// `#/price-per-tick/max-ticks/exec` table plus the full `tokenContract` addresses by `#`. `max_price_per_tick`
/// (when `Some`) marks which asks are executable at that ceiling; `your_order_ticks`(when `Some`) appends the
/// buyer's order summary line. The caller sorts nothing -- this sorts by price ascending(best ask first).
pub fn print_book_table(
    frame_model: &str,
    rows: &[BookRow],
    max_price_per_tick: Option<u128>,
    your_order_ticks: Option<u128>,
) {
    use std::io::IsTerminal;
    // ANSI styling only on a real terminal -- piped/headless output stays plain(clean logs, copyable).
    let color = std::io::stdout().is_terminal();
    let paint = |s: &str, code: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    // One tick = a fixed number of delivered model tokens -- print it
    // so price/tick and the tick counts are interpretable in model tokens, not abstract units.
    let tick_size = DobParams::canonical().tick_size as u128;
    let title = format!("inference order book -- {frame_model}");
    let subtitle = format!("1 tick = {tick_size} model tokens");
    if rows.is_empty() {
        println!("{}  ({subtitle})", paint(&title, "1;36"));
        println!(
            "  {} no resting asks yet -- a buy would rest until a seller matches",
            paint("*", "2")
        );
        return;
    }
    let mut sorted: Vec<&BookRow> = rows.iter().collect();
    sorted.sort_by_key(|o| o.price_per_tick);

    // Columns are dynamic: the `exec` verdict only appears when there is a price ceiling to judge against
    // (the buyer's pre-buy view); the read-only `market` discovery view omits it. The full `tokenContract`
    // address is a column IN the table(un-truncated, copy-paste intact) -- the table is as wide as it needs.
    // 0 = center, 1 = right, 2 = left.
    let has_exec = max_price_per_tick.is_some();
    let mut headers: Vec<&str> = vec!["#", "price/tick", "max ticks"];
    let mut aligns: Vec<u8> = vec![0, 1, 1];
    if has_exec {
        headers.push("exec");
        aligns.push(0);
    }
    headers.push("tokenContract");
    aligns.push(2);
    let rows_str: Vec<Vec<String>> = sorted
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let mut cells = vec![
                (i + 1).to_string(),
                o.price_per_tick.to_string(),
                o.max_ticks.to_string(),
            ];
            if let Some(cap) = max_price_per_tick {
                cells.push(if o.price_per_tick <= cap { "yes" } else { "no" }.to_string());
            }
            cells.push(o.token_contract.clone());
            cells
        })
        .collect();
    let n = headers.len();
    let mut w = vec![0usize; n];
    for (i, head) in headers.iter().enumerate() {
        w[i] = head.chars().count();
    }
    for r in &rows_str {
        for i in 0..n {
            w[i] = w[i].max(r[i].chars().count());
        }
    }
    // Box-drawing border for the given junction chars(left, mid, right).
    let border = |l: &str, m: &str, r: &str| {
        let seg: Vec<String> = w.iter().map(|&c| "-".repeat(c + 2)).collect();
        format!("{l}{}{r}", seg.join(m))
    };
    let fit = |s: &str, width: usize, align: u8| {
        let pad = width.saturating_sub(s.chars().count());
        match align {
            1 => format!("{}{}", " ".repeat(pad), s), // right
            2 => format!("{}{}", s, " ".repeat(pad)), // left
            _ => {
                let left = pad / 2;
                format!("{}{}{}", " ".repeat(left), s, " ".repeat(pad - left)) // center
            }
        }
    };
    let bar = paint("-", "2");
    let render_row = |cells: &[String], style: &dyn Fn(&str, usize) -> String| {
        let body: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| style(&fit(c, w[i], aligns[i]), i))
            .collect();
        format!("{bar} {} {bar}", body.join(&format!(" {bar} ")))
    };

    println!("{}  ({subtitle})", paint(&title, "1;36"));
    println!("{}", paint(&border("-", "-", "-"), "2"));
    let head_strings: Vec<String> = headers.iter().map(|s| s.to_string()).collect();
    println!("{}", render_row(&head_strings, &|s, _| paint(s, "1;36")));
    println!("{}", paint(&border("-", "-", "-"), "2"));
    let exec_col = has_exec.then_some(3usize);
    for r in &rows_str {
        println!(
            "{}",
            render_row(r, &|s, i| {
                if Some(i) == exec_col {
                    if s.trim() == "yes" {
                        paint(s, "1;32")
                    } else {
                        paint(s, "2")
                    }
                } else {
                    s.to_string()
                }
            })
        );
    }
    println!("{}", paint(&border("-", "-", "-"), "2"));
    if let (Some(ticks), Some(cap)) = (your_order_ticks, max_price_per_tick) {
        println!(
            "{} {ticks} ticks (= {} model tokens) at up to {} SHELL/tick -- fills the best ask within the limit",
            paint("your order:", "1"),
            ticks.saturating_mul(tick_size),
            paint(&cap.to_string(), "33"),
        );
    }
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
            anyhow::anyhow!("could not read a submit-safe order book for {frame_model}: {e}")
        })?;
    let offers = chain.discover_offers().await.map_err(|e| {
        anyhow::anyhow!("could not read a trustworthy order book for {frame_model}: {e}")
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

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    let msg = error.to_string();
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
    let token_contract = chain
        .wait_matched_token_contract(since_unix, std::time::Duration::from_secs(DEAL_WAIT_SECS))
        .await?;
    validate_reported_match_state(chain, &token_contract).await?;
    Ok(token_contract)
}

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
                    let session =
                        Arc::new(dexdo::buyer::api::SessionSettle::new_with_failure_policy(
                            chain.clone(),
                            next.clone(),
                            buyer.note.clone(),
                            api_failure_policy,
                        ));
                    let next_deal = dexdo::buyer::api::ApiDeal::new(
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
                    );
                    deals.replace_active(next_deal, "continuity-renewal").await;
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

pub(crate) async fn run_buyer(args: BuyerArgs) -> Result<()> {
    let json_mode = args.json;
    let mut machine_events = json_mode.then(machine::BuyerEventWriter::new);
    let mut machine_context = BuyerMachineErrorContext::default();
    let result = run_buyer_inner(args, &mut machine_events, &mut machine_context).await;
    if let Err(err) = result {
        if machine::is_printed_error(&err) {
            return Err(err);
        }
        if let Some(events) = machine_events.as_mut() {
            let code = machine::classify_error(machine::OP_BUYER_START, &err);
            if code == machine::ErrorCode::NoLiquidity
                && err
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("no_executable_ask")
            {
                machine_context.failure_class = Some("no_executable_ask".to_string());
            }
            events.error(machine::OP_BUYER_START, code, machine_context.fields())?;
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
    let message = match code.as_str() {
        "NO_LIQUIDITY" => "no liquidity fixture",
        "INSUFFICIENT_BALANCE" => "insufficient balance fixture",
        "HANDOVER_TIMEOUT" => "handover within deadline fixture",
        "CHAIN_TRANSPORT" => "shellnet rpc transport fixture",
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
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("exit code 52") || msg.contains("replay protection")
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
) -> Result<dexdo::buyer::api::ApiDeal, String> {
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
            attempt,
        )
        .await;
        match result {
            Ok(deal) => return Ok(deal),
            Err(err) if is_replay_protection_error(&err) && attempt < MAX_ATTEMPTS => {
                let backoff_secs = attempt.saturating_mul(2);
                let _ = emit_shared_buyer_event(
                    &events,
                    "purchase_progress",
                    machine::OP_BUYER_RUNTIME,
                    json!({
                        "stage": "replay_protection_backoff",
                        "attempt": attempt,
                        "next_attempt": attempt + 1,
                        "backoff_secs": backoff_secs,
                        "diagnostic": "shellnet replay protection exit 52; retrying after backoff"
                    }),
                )
                .await;
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                attempt = attempt.saturating_add(1);
            }
            Err(err) if is_replay_protection_error(&err) => {
                return Err(format!(
                    "on-demand purchase failed after replay-protection retries: {err}"
                ));
            }
            Err(err) => return Err(err.to_string()),
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
    attempt: u64,
) -> Result<dexdo::buyer::api::ApiDeal> {
    emit_shared_buyer_event(
        &events,
        "purchase_progress",
        machine::OP_BUYER_RUNTIME,
        json!({
            "stage": "started",
            "attempt": attempt,
            "frame_model": frame_model.clone()
        }),
    )
    .await?;
    require_stream_buy_ticks(args.ticks)?;
    if !args.mock.mock_chain {
        emit_shared_buyer_event(
            &events,
            "purchase_progress",
            machine::OP_BUYER_RUNTIME,
            json!({
                "stage": "shellnet_preflight",
                "attempt": attempt,
                "frame_model": frame_model.clone()
            }),
        )
        .await?;
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

    let mut service_renewal: Option<(u128, u128, u128)> = None;
    let (mut token_contract, buy_ticks) = match explicit_tc.clone() {
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
                let selection = buyer_quote_selection(
                    chain.as_ref(),
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
                preflight_buyer_pool_for_money_move(args.as_ref())?;
                buyer.place_buy(chain.as_ref(), &tc).await?;
                emit_shared_buyer_event(
                    &events,
                    "buy_submitted",
                    machine::OP_BUYER_START,
                    json!({
                        "frame_model": frame_model.clone(),
                        "order_book": "explicit_token_contract",
                        "ticks": machine::amount(args.ticks),
                        "max_price_per_tick": machine::amount(args.max_price_per_tick),
                        "escrow": machine::amount(selection.escrow)
                    }),
                )
                .await?;
                emit_shared_buyer_event(
                    &events,
                    "matched",
                    machine::OP_BUYER_START,
                    json!({
                        "frame_model": frame_model.clone(),
                        "order_book": "explicit_token_contract",
                        "token_contract": tc.clone()
                    }),
                )
                .await?;
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
                .await?;
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
            let selection =
                buyer_quote_selection(chain.as_ref(), None, ticks, max_price, Some(escrow)).await?;
            require_complete_buyer_quote(&selection)?;
            emit_shared_buyer_event(
                &events,
                "quote_selected",
                machine::OP_BUYER_START,
                quote_selected_fields(&frame_model, &selection, ticks, max_price),
            )
            .await?;
            require_stream_buy_ticks(ticks)?;
            let since_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            place_buy_by_model_after_pool_preflight(
                chain.as_ref(),
                buyer.as_ref(),
                !args.mock.mock_chain,
                args.identity.note_addr.as_deref(),
                ticks,
                max_price,
                escrow,
            )
            .await?;
            emit_shared_buyer_event(
                &events,
                "buy_submitted",
                machine::OP_BUYER_START,
                json!({
                    "frame_model": frame_model.clone(),
                    "order_book": selection.order_book,
                    "ticks": machine::amount(ticks),
                    "max_price_per_tick": machine::amount(max_price),
                    "escrow": machine::amount(escrow)
                }),
            )
            .await?;
            let tc = chain
                .wait_matched_token_contract(
                    since_unix,
                    std::time::Duration::from_secs(DEAL_WAIT_SECS),
                )
                .await?;
            validate_reported_match_state(chain.as_ref(), &tc).await?;
            emit_shared_buyer_event(
                &events,
                "matched",
                machine::OP_BUYER_START,
                json!({
                    "frame_model": frame_model.clone(),
                    "order_book": "model_order_book",
                    "token_contract": tc.clone()
                }),
            )
            .await?;
            (tc, ticks)
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
                        anyhow::bail!("buyer: malformed handover for {token_contract}: {e}");
                    }
                    if std::time::Instant::now() >= hv_deadline {
                        let last_error = e.to_string();
                        let diagnostic = handover_timeout_diagnostic(
                            chain.as_ref(),
                            &token_contract,
                            &last_error,
                        )
                        .await;
                        if let Some(policy) = buyer_policy.as_ref() {
                            match apply_no_handover_after_match_policy(
                                chain.as_ref(),
                                buyer.as_ref(),
                                &token_contract,
                                policy,
                                service_renewal,
                                handover_attempt,
                                &diagnostic,
                                args.identity.note_addr.as_deref(),
                            )
                            .await?
                            {
                                NoHandoverPolicyOutcome::RetryCurrent => continue 'handover,
                                NoHandoverPolicyOutcome::RetryNext(next) => {
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
                        anyhow::bail!("{}", diagnostic);
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
) -> Result<()> {
    use dexdo::buyer::api::{self, ApiState};

    let bind = args
        .local_listen
        .ok_or_else(|| anyhow::anyhow!("on-demand local API requires --local-listen"))?;
    let buyer = Arc::new(buyer);
    let args = Arc::new(args);
    let pending_token_contract = "pending:on-demand";
    let pending_deal_handle = "pending:on-demand";
    emit_shared_buyer_event(
        &events,
        "endpoint_binding",
        machine::OP_BUYER_START,
        json!({
            "token_contract": pending_token_contract,
            "deal_handle": pending_deal_handle,
            "requested_bind_addr": bind.to_string(),
            "allow_port_zero": bind.port() == 0
        }),
    )
    .await?;

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
                )
                .await
            }) as dexdo::buyer::api::DealInitFuture
        }) as dexdo::buyer::api::DealInitializer
    };
    let state = ApiState::lazy(
        buyer,
        frame_model.clone(),
        initializer,
        std::time::Duration::from_secs(DEAL_WAIT_SECS),
    );
    let deals = state.deals.clone();
    let (addr, task) = match api::serve(
        bind,
        state,
        args.anthropic_compat,
        operator_shutdown_signal(),
    )
    .await
    {
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
            "token_contract": pending_token_contract,
            "deal_handle": pending_deal_handle,
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
        .unwrap_or_else(|| {
            (
                pending_token_contract.to_string(),
                pending_deal_handle.to_string(),
            )
        });
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
) -> Result<()> {
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
    let (chain, note) = if args.mock.mock_chain {
        args.mock.require_mock_model()?;
        let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
        mock_chain_and_note(endpoints_file, &args.identity)?
    } else {
        buyer_real_backend(&args, &frame_model)?
    };
    let buyer = dexdo::buyer::Buyer::from_note(note);
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
    let mut service_renewal: Option<(u128, u128, u128)> = None;
    let (mut token_contract, buy_ticks) = match explicit_tc {
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
                let mut selected = None;
                if let Some(events) = machine_events.as_mut() {
                    let selection = buyer_quote_selection(
                        chain.as_ref(),
                        Some(&tc),
                        args.ticks,
                        args.max_price_per_tick,
                        args.escrow,
                    )
                    .await?;
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
                    selected = Some(selection);
                }
                require_stream_buy_ticks(args.ticks)?;
                let submitted_escrow = selected.as_ref().map(|s| s.escrow).unwrap_or_else(|| {
                    args.escrow.unwrap_or_else(|| {
                        required_escrow_for_buy(args.ticks, args.max_price_per_tick)
                    })
                });
                preflight_buyer_pool_for_money_move(&args)?;
                buyer.place_buy(chain.as_ref(), &tc).await?;
                if let Some(events) = machine_events.as_mut() {
                    events.event(
                        "buy_submitted",
                        machine::OP_BUYER_START,
                        json!({
                            "frame_model": frame_model.clone(),
                            "order_book": "explicit_token_contract",
                            "ticks": machine::amount(args.ticks),
                            "max_price_per_tick": machine::amount(args.max_price_per_tick),
                            "escrow": machine::amount(submitted_escrow)
                        }),
                    )?;
                    events.event(
                        "matched",
                        machine::OP_BUYER_START,
                        json!({
                            "frame_model": frame_model.clone(),
                            "order_book": "explicit_token_contract",
                            "token_contract": tc.clone()
                        }),
                    )?;
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
                .await?;
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
            let mut selected = None;
            if let Some(events) = machine_events.as_mut() {
                let selection =
                    buyer_quote_selection(chain.as_ref(), None, ticks, max_price, Some(escrow))
                        .await?;
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
                selected = Some(selection);
            }
            require_stream_buy_ticks(ticks)?;
            let since_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            place_buy_by_model_after_pool_preflight(
                chain.as_ref(),
                &buyer,
                !args.mock.mock_chain,
                args.identity.note_addr.as_deref(),
                ticks,
                max_price,
                escrow,
            )
            .await?;
            if let Some(events) = machine_events.as_mut() {
                events.event(
                    "buy_submitted",
                    machine::OP_BUYER_START,
                    json!({
                        "frame_model": frame_model.clone(),
                        "order_book": selected
                            .as_ref()
                            .map(|s| s.order_book)
                            .unwrap_or("model_order_book"),
                        "ticks": machine::amount(ticks),
                        "max_price_per_tick": machine::amount(max_price),
                        "escrow": machine::amount(escrow)
                    }),
                )?;
            }
            tracing::info!("model-only buy placed; awaiting match on the note's fill event");
            let tc = chain
                .wait_matched_token_contract(
                    since_unix,
                    std::time::Duration::from_secs(DEAL_WAIT_SECS),
                )
                .await?;
            machine_context.set_token_contract(&tc);
            if let Some(events) = machine_events.as_mut() {
                events.event(
                    "matched",
                    machine::OP_BUYER_START,
                    json!({
                        "frame_model": frame_model.clone(),
                        "order_book": "model_order_book",
                        "token_contract": tc.clone()
                    }),
                )?;
            } else {
                println!("matched deal TokenContract: {tc}");
            }
            let status = validate_reported_match_state(chain.as_ref(), &tc).await?;
            if machine_events.is_none() {
                println!("{}", matched_state_summary(&tc, &status));
            }
            (tc, ticks)
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
                        anyhow::bail!("buyer: malformed handover for {token_contract}: {e}");
                    }
                    if std::time::Instant::now() >= hv_deadline {
                        let last_error = e.to_string();
                        let diagnostic = handover_timeout_diagnostic(
                            chain.as_ref(),
                            &token_contract,
                            &last_error,
                        )
                        .await;
                        if let Some(policy) = buyer_policy.as_ref() {
                            match apply_no_handover_after_match_policy(
                                chain.as_ref(),
                                &buyer,
                                &token_contract,
                                policy,
                                service_renewal,
                                handover_attempt,
                                &diagnostic,
                                args.identity.note_addr.as_deref(),
                            )
                            .await?
                            {
                                NoHandoverPolicyOutcome::RetryCurrent => continue 'handover,
                                NoHandoverPolicyOutcome::RetryNext(next) => {
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
                        anyhow::bail!("{}", diagnostic);
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
    oneshot_session.settle("one-shot-complete").await;
    Ok(())
}

pub(crate) async fn run_monitor(args: MonitorArgs) -> Result<()> {
    // Real shellnet monitoring: a `RealNote` is a single key, not an HD tree, so the real monitor
    // reads the operator's `--market` manifest(s) by-fact on-chain rather than aggregating a `--tree-width`
    // window. The mock path below still aggregates the note tree.
    if !args.mock.mock_chain {
        return run_monitor_real(&args).await;
    }
    // The monitor reads the mock chain. Read-only, moves nothing.
    let tree = load_note_tree(args.identity.note_key.as_deref())?;
    let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
    let chain = MockChainBackend::new(
        endpoints_file,
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    );
    // Aggregate state over the whole tree: a per-note snapshot for each
    // public key in the `0..tree_width` window, then a roll-up. Each order/deal lives on its own sub-note.
    let mut snaps = Vec::new();
    for pk in tree.node_pubkeys(args.tree_width) {
        snaps.push(chain.note_snapshot(&pk).await?);
    }
    print_tree_snapshot(&aggregate_tree(snaps));
    Ok(())
}

/// Real-shellnet monitor: read the operator's `--market` manifest(s) and print each market's
/// by-fact deal state on-chain through the SAME `print_tree_snapshot` (per-model breakdown + anomaly
/// surfacing) as the mock path. Read-only -- only getters, moves nothing. Each manifest's `TokenContract` is
/// read via `real_market_deal_view`(`getState`/`getProbe` + the buyer pubkey); the model/price come from the
/// manifest. Live-verifiable once a deal `TokenContract` is deployed.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_monitor_real(args: &MonitorArgs) -> Result<()> {
    use dexdo_core::{real_market_deal_view, MarketManifest, RealChainBackend, TreeSnapshot};
    if args.market.is_empty() {
        bail!(
            "real shellnet monitor: pass --market <manifest>... (the operator's `dexdo provision` market \
             record(s)); a RealNote is a single key, not an HD tree, so the monitor reads the markets it is given"
        );
    }
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let mut note_ids = Vec::new();
    let mut deals = Vec::new();
    let mut exposure: u64 = 0;
    for m in &args.market {
        let json = std::fs::read_to_string(m)
            .map_err(|e| anyhow::anyhow!("read --market {}: {e}", m.display()))?;
        let manifest = MarketManifest::from_json(&json)
            .map_err(|e| anyhow::anyhow!("parse --market {}: {e}", m.display()))?;
        manifest
            .validate()
            .map_err(|e| anyhow::anyhow!("--market {}: {e}", m.display()))?;
        note_ids.push(manifest.seller_note.clone());
        // Fail loud(review): the real reader returns an error for an undeployed/unreadable TC or a
        // manifest/getter mismatch -- surface it with the offending --market file, never as empty data.
        let deal = real_market_deal_view(&chain, &manifest)
            .await
            .map_err(|e| anyhow::anyhow!("--market {}: {e}", m.display()))?;
        if let Some(s) = &deal.snapshot {
            if !s.closed {
                // The operator is the SELLER of their own market, so the note's at-risk SHELL is the
                // SELLER-side lock(probe/stake) -- NOT the buyer's deposit. This matches the mock's role-side
                // exposure and `TreeSnapshot.exposure`'s contract("the sum locked by the note").
                exposure = exposure.saturating_add(s.seller_locked);
            }
        }
        deals.push(deal);
    }
    print_tree_snapshot(&TreeSnapshot {
        note_ids,
        offers: Vec::new(),
        deals,
        exposure,
    });
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_monitor_real(_args: &MonitorArgs) -> Result<()> {
    bail!("real shellnet monitoring unavailable: build with `--features shellnet`")
}

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

/// recover an orphaned OPEN deal. The buyer process died mid-stream but the buyer note/key are intact,
/// so no one sent STOP and the deal hangs OPEN(the seller cannot `destroy` an `_opened` deal). `recover`
/// signs the **normal buyer-STOP** (`streamStop(tokenContract)` -> `TokenContract.stop()`, standard
/// split) from the buyer note -- it does NOT place a new buy -- after which the seller `destroy`s the TC.
/// Fails closed(before sending STOP) if the deal is not `_opened`, is `_disputed`, or the note is not the
/// deal's recorded buyer; the on-chain `TC.stop()` also enforces `msg.sender == _buyer`.
/// (The "seller vanished mid-stream" case is instead the contract's `reclaimOnTimeout`/`STREAM_TIMEOUT`.)
#[cfg(feature = "shellnet")]
pub(crate) async fn run_recover(args: RecoverArgs) -> Result<()> {
    use dexdo_core::{check_recoverable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend};
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let resolved = resolve_pool_recovery_inputs(
        "recover",
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    )?;
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: only an OPEN, undisputed deal owned by THIS buyer note can be STOPped.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("recover: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_recoverable(
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "recover {tc}: buyer-signed STOP of an OPEN deal (streamStop -> TokenContract.stop(), standard \
         split). No new buy is placed. After this, the seller closes it: `dexdo destroy --token-contract {tc}`."
    );
    chain.stream_stop(&note, &keys, &tc).await?;
    println!(
        "recover submitted -> streamStop(TokenContract {tc}) from buyer note {note}; the deal STOPs (standard \
         split). Next: the seller runs `dexdo destroy` to close (selfdestruct) the TokenContract."
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_recover(_args: RecoverArgs) -> Result<()> {
    bail!("recover unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_dispute(args: DisputeArgs) -> Result<()> {
    use dexdo_core::{check_disputable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend};
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let resolved = resolve_pool_recovery_inputs(
        "dispute",
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    )?;
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: only an OPEN, undisputed deal owned by THIS buyer note/key can be disputed.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("dispute: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_disputable(
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "dispute {tc}: buyer-signed streamDispute -> TokenContract.dispute() () -- LOCKS BOTH notes (yours \
         and the seller's) until releaseDispute/arbitration. Stronger than `recover` (which still pays the \
         seller for delivered ticks); releaseDispute is seller-only."
    );
    chain.stream_dispute(&note, &keys, &tc).await?;
    println!(
        "dispute submitted -> streamDispute(TokenContract {tc}) from buyer note {note}; the deal is DISPUTED \
         and both notes are locked until it resolves (seller releaseDispute, or arbitration)."
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_dispute(_args: DisputeArgs) -> Result<()> {
    bail!("dispute unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_reclaim(args: ReclaimArgs) -> Result<()> {
    use dexdo_core::{
        check_reclaimable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend,
        MATCH_OPEN_TIMEOUT_SECS,
    };
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let resolved = resolve_pool_recovery_inputs(
        "reclaim",
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    )?;
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: owned by THIS buyer + funded + not disputed + the
    // relevant timeout reached. OPEN deals use STREAM_TIMEOUT(streamReclaim); funded-but-never-opened deals use
    // MATCH_OPEN_TIMEOUT from fundedTime(streamCleanup). Reject locally rather than letting the contract revert.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("reclaim: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let funded = state["funded"].as_bool().unwrap_or(false);
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let last_advance = state["lastAdvance"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let funded_time = state["fundedTime"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok());
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    // Per-deal dynamic STREAM_TIMEOUT is only needed for OPEN abandoned deals. The never-opened cleanup path
    // gates on fixed MATCH_OPEN_TIMEOUT from getState.fundedTime.
    let stream_timeout = if opened {
        let cfg = chain
            .token_contract_config(&tc)
            .await?
            .ok_or_else(|| anyhow::anyhow!("reclaim: TokenContract {tc} getConfig unavailable"))?;
        Some(
            cfg["streamTimeout"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("reclaim: getConfig exposes no streamTimeout"))?,
        )
    } else {
        None
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs();
    check_reclaimable(
        funded,
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
        now,
        last_advance,
        stream_timeout,
        funded_time,
        MATCH_OPEN_TIMEOUT_SECS,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if opened {
        let stream_timeout = stream_timeout.expect("opened branch parsed streamTimeout");
        eprintln!(
            "reclaim {tc}: buyer-signed streamReclaim -> TokenContract.reclaimOnTimeout() (no burn: probe + \
             deposit back to you, commission to the seller). STREAM_TIMEOUT met: lastAdvance {last_advance} + \
             streamTimeout {stream_timeout} <= now {now}."
        );
        chain.reclaim_on_timeout(&note, &keys, &tc).await?;
        println!(
            "reclaim submitted -> streamReclaim(TokenContract {tc}) from buyer note {note}; the escrow returns \
             to your note and the deal closes (opened=false)."
        );
    } else {
        let funded_time = funded_time.expect("never-opened branch checked fundedTime");
        eprintln!(
            "reclaim {tc}: buyer-signed streamCleanup -> TokenContract.cleanupUnopened() (never-opened refund). \
             MATCH_OPEN_TIMEOUT met: fundedTime {funded_time} + matchOpenTimeout {MATCH_OPEN_TIMEOUT_SECS} <= \
             now {now}."
        );
        chain.stream_cleanup(&note, &keys, &tc).await?;
        println!(
            "reclaim submitted -> streamCleanup(TokenContract {tc}) from buyer note {note}; the never-opened \
             escrow returns to your note and the deal closes."
        );
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_reclaim(_args: ReclaimArgs) -> Result<()> {
    bail!("reclaim unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_release_dispute(args: ReleaseDisputeArgs) -> Result<()> {
    use dexdo_core::{
        check_release_disputable, check_seller_pubkey, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!("release-dispute: --note-addr (seller note) is required")
        })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("release-dispute: --note-key (seller owner key) is required")
    })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
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

    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("release-dispute: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    check_release_disputable(disputed).map_err(|e| anyhow::anyhow!(e))?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("release-dispute", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "release-dispute {tc}: seller-signed TokenContract.releaseDispute() from note {note}; concedes the \
         dispute, unlocks both notes, and returns the contested tick/deposit to the buyer."
    );
    chain.release_dispute(&tc, &keys).await?;
    println!(
        "release-dispute submitted -> TokenContract {tc}; both notes unlock after the dispute resolution lands"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_release_dispute(_args: ReleaseDisputeArgs) -> Result<()> {
    bail!("release-dispute unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_withdraw_shell(args: WithdrawShellArgs) -> Result<()> {
    use dexdo_core::{
        check_seller_pubkey, check_withdrawable_shell, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!("withdraw-shell: --note-addr (seller note) is required")
        })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("withdraw-shell: --note-key (seller owner key) is required")
    })?;
    let recipient_addr = args.recipient.clone().unwrap_or_else(|| note_addr.clone());
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let recipient = Address::parse(&recipient_addr)
        .map_err(|e| anyhow::anyhow!("--recipient/--note-addr {recipient_addr}: {e}"))?;

    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("withdraw-shell: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let finalized_owed = state["finalizedOwed"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .ok_or_else(|| anyhow::anyhow!("withdraw-shell: getState exposes no finalizedOwed"))?;
    let amount =
        check_withdrawable_shell(finalized_owed, args.amount).map_err(|e| anyhow::anyhow!(e))?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("withdraw-shell", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "withdraw-shell {tc}: seller-signed TokenContract.withdrawShell(amount={amount}, recipient={recipient}). \
         This withdraws finalized seller proceeds only; use `destroy` later to close/selfdestruct the TC."
    );
    chain.withdraw_shell(&tc, amount, &recipient, &keys).await?;
    println!(
        "withdraw-shell submitted -> {amount} finalized SHELL from TokenContract {tc} to {recipient}"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_withdraw_shell(_args: WithdrawShellArgs) -> Result<()> {
    bail!("withdraw-shell unavailable: build with `--features shellnet`")
}

/// write the `DEXDO_PN_POOL`(carries note owner secret keys) privately + atomically --
/// an exclusive 0600 temp in the destination directory, then `rename` over the target. A plain `fs::write`
/// inherits the umask, and a predictable non-exclusive temp path can clobber a pre-created file/symlink.
#[cfg(feature = "shellnet")]
fn write_pool_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    crate::cli::note::write_private_atomic(path, bytes)
}

#[cfg(feature = "shellnet")]
#[cfg_attr(not(test), allow(dead_code))]
fn write_pool_private_via_temp(
    path: &std::path::Path,
    tmp: &std::path::Path,
    bytes: &[u8],
) -> Result<()> {
    crate::cli::note::write_private_atomic_via_temp(path, tmp, bytes)
}

#[cfg(feature = "shellnet")]
fn note_deploy_same_file_pool_guard(
    env_pool: Option<&std::ffi::OsStr>,
    pool: &std::path::Path,
) -> Result<()> {
    let Some(env_pool) = env_pool else {
        return Ok(());
    };
    if env_pool.is_empty() {
        return Ok(());
    }
    let env_pool = std::path::Path::new(env_pool);
    let (Ok(env_pool), Ok(pool)) = (std::fs::canonicalize(env_pool), std::fs::canonicalize(pool))
    else {
        return Ok(());
    };
    if env_pool == pool {
        bail!(
            "note deploy refused: DEXDO_PN_POOL and --pool both point to the same existing file {}. \
             This append mode can hide note-key confusion and leave a pool entry whose --note-key later fails \
             owner-signed writes with ERR_INVALID_SENDER 101. Unset DEXDO_PN_POOL while deploying, or deploy \
             into a fresh --pool <new_file> and switch DEXDO_PN_POOL to that file after the command succeeds.",
            pool.display()
        );
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn note_deploy_recovery_pool_guard(
    pool: &std::path::Path,
    recovery: &std::path::Path,
) -> Result<()> {
    if comparable_path(pool)? == comparable_path(recovery)? {
        bail!(
            "note deploy refused: --recovery and --pool both point to {}. The recovery file is an \
             intermediate secret-bearing state file; keep it separate from the final DEXDO_PN_POOL.",
            pool.display()
        );
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn comparable_path(path: &std::path::Path) -> Result<std::path::PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let base = match parent {
        Some(parent) => std::fs::canonicalize(parent).unwrap_or_else(|_| cwd.join(parent)),
        None => cwd,
    };
    let file = path.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "path {} has no file name for same-file check",
            path.display()
        )
    })?;
    Ok(base.join(file))
}

#[cfg(feature = "shellnet")]
fn note_endpoint_url(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        bail!("--endpoint must not be empty");
    }
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        Ok(endpoint.to_string())
    } else {
        Ok(format!("https://{endpoint}"))
    }
}

#[cfg(feature = "shellnet")]
fn note_deploy_multisig_secret_hex(args: &NoteDeployArgs) -> Result<(&'static str, String)> {
    match (&args.multisig_key, &args.multisig_seed_file) {
        (Some(_), Some(_)) => bail!("use only one of --multisig-key or --multisig-seed-file"),
        (Some(path), None) => Ok(("--multisig-key", read_secret_hex(path, "--multisig-key")?)),
        (None, Some(path)) => {
            let phrase = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("read --multisig-seed-file {}: {e}", path.display())
            })?;
            if phrase.split_whitespace().next().is_none() {
                bail!("--multisig-seed-file {} is empty", path.display());
            }
            let key = dexdo::wallet_seed::derive_multisig_key_from_seed_phrase(&phrase)
                .map_err(|e| anyhow::anyhow!("--multisig-seed-file {}: {e}", path.display()))?;
            Ok(("--multisig-seed-file", key.secret_hex().to_string()))
        }
        (None, None) => bail!("one of --multisig-key or --multisig-seed-file is required"),
    }
}

#[cfg(feature = "shellnet")]
fn note_deploy_multisig_keys(args: &NoteDeployArgs) -> Result<dexdo_core::KeyPair> {
    let (source, secret_hex) = note_deploy_multisig_secret_hex(args)?;
    dexdo_core::KeyPair::from_secret_hex(secret_hex.trim())
        .map_err(|e| anyhow::anyhow!("{source} (SDK secret hex): {e:?}"))
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, Copy, Default)]
struct NoteDeployVoucherFailpoints {
    after_deposit_submit: bool,
    after_deposit_event: bool,
    after_shell_submit: bool,
    after_deploy_before_note_record: bool,
}

#[cfg(feature = "shellnet")]
impl NoteDeployVoucherFailpoints {
    fn after_submit(self, kind: crate::cli::note::NoteDeployVoucherKind) -> bool {
        match kind {
            crate::cli::note::NoteDeployVoucherKind::Deposit => self.after_deposit_submit,
            crate::cli::note::NoteDeployVoucherKind::ShellGas => self.after_shell_submit,
        }
    }

    fn after_event(self, kind: crate::cli::note::NoteDeployVoucherKind) -> bool {
        match kind {
            crate::cli::note::NoteDeployVoucherKind::Deposit => self.after_deposit_event,
            crate::cli::note::NoteDeployVoucherKind::ShellGas => false,
        }
    }
}

#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_SUBMIT_NATIVE_VALUE: u128 = 2_000_000_000;
#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_VOUCHER_EVENT_TIMEOUT_SECS: u64 = 480;
#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_ROOT_PN_DAPP_ID: &str = "0";
#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_GENERIC_MULTISIG_CODE_HASH: &str =
    "3a7a53248ff39fde936a4274eab143b5fac94feac0d8e2e2748aac5e74538d5f";
#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH: &str =
    "8470e1da28a2b4c742b5f7edefdd97db81c79e726f8a8b0be78d921adaf32414";

#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_GENERIC_MULTISIG_ABI_JSON: &str = r#"{
  "ABI version": 2,
  "version": "2.4",
  "header": ["pubkey", "time", "expire"],
  "functions": [
    {
      "name": "sendTransaction",
      "inputs": [
        { "name": "dest", "type": "address" },
        { "name": "value", "type": "uint128" },
        { "name": "cc", "type": "map(uint32,varuint32)" },
        { "name": "bounce", "type": "bool" },
        { "name": "flags", "type": "uint8" },
        { "name": "payload", "type": "cell" },
        { "name": "dapp_id", "type": "uint256" }
      ],
      "outputs": [{ "name": "value0", "type": "address" }]
    }
  ],
  "events": [],
  "data": []
}"#;

#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_ABI_JSON: &str = r#"{
  "ABI version": 2,
  "version": "2.4",
  "header": ["pubkey", "time", "expire"],
  "functions": [
    {
      "name": "sendTransaction",
      "inputs": [
        { "name": "dest", "type": "address" },
        { "name": "value", "type": "uint128" },
        { "name": "cc", "type": "map(uint32,varuint32)" },
        { "name": "bounce", "type": "bool" },
        { "name": "flags", "type": "uint8" },
        { "name": "payload", "type": "cell" }
      ],
      "outputs": [{ "name": "value0", "type": "address" }]
    }
  ],
  "events": [],
  "data": []
}"#;

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteDeployMultisigForwardKind {
    Generic,
    UpdateCustodian,
}

#[cfg(feature = "shellnet")]
impl NoteDeployMultisigForwardKind {
    fn from_code_hash(code_hash: &str) -> Result<Self> {
        let code_hash = code_hash
            .trim()
            .trim_start_matches("0x")
            .to_ascii_lowercase();
        match code_hash.as_str() {
            NOTE_DEPLOY_GENERIC_MULTISIG_CODE_HASH => Ok(Self::Generic),
            NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH => Ok(Self::UpdateCustodian),
            other => Err(anyhow::anyhow!(
                "unsupported funding wallet code_hash {other}; supported generic Multisig \
                 {NOTE_DEPLOY_GENERIC_MULTISIG_CODE_HASH} and UpdateCustodianMultisigWallet \
                 {NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH}"
            )),
        }
    }

    fn abi_json(self) -> &'static str {
        match self {
            Self::Generic => NOTE_DEPLOY_GENERIC_MULTISIG_ABI_JSON,
            Self::UpdateCustodian => NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_ABI_JSON,
        }
    }

    fn send_transaction_params(
        self,
        root_pn: &dexdo_core::Address,
        cc: serde_json::Map<String, serde_json::Value>,
        voucher_body: String,
    ) -> serde_json::Value {
        let mut params = serde_json::json!({
            "dest": root_pn.with_workchain(),
            "value": NOTE_DEPLOY_SUBMIT_NATIVE_VALUE.to_string(),
            "cc": serde_json::Value::Object(cc),
            "bounce": true,
            "flags": 1,
            "payload": voucher_body,
        });
        if self == Self::Generic {
            params["dapp_id"] = serde_json::Value::String(NOTE_DEPLOY_ROOT_PN_DAPP_ID.to_string());
        }
        params
    }
}

#[cfg(feature = "shellnet")]
async fn note_deploy_fetch_wallet_code_hash(
    http: &reqwest::Client,
    endpoint: &str,
    wallet: &dexdo_core::Address,
) -> Result<String> {
    let bare = wallet.bare();
    let query = format!(
        "{{ blockchain {{ account(account_id: \"{bare}\", dapp_id: \"{bare}\") {{ info {{ acc_type_name code_hash }} }} }} }}"
    );
    let resp: serde_json::Value = http
        .post(format!("{}/graphql", endpoint.trim_end_matches('/')))
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("read funding wallet code_hash: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("read funding wallet code_hash: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("decode funding wallet code_hash response: {e}"))?;
    if let Some(errors) = resp.get("errors") {
        bail!("read funding wallet code_hash GraphQL errors: {errors}");
    }
    let info = resp
        .pointer("/data/blockchain/account/info")
        .ok_or_else(|| anyhow::anyhow!("funding wallet {} not found", wallet.with_workchain()))?;
    let acc_type = info
        .get("acc_type_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    if acc_type != "Active" {
        bail!(
            "funding wallet {} is not Active (acc_type={acc_type})",
            wallet.with_workchain()
        );
    }
    info.get("code_hash")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "funding wallet {} has no code_hash",
                wallet.with_workchain()
            )
        })
}

#[cfg(feature = "shellnet")]
fn note_deploy_persist_voucher_checkpoint(
    recovery_path: &std::path::Path,
    recovery: &mut crate::cli::note::NoteDeployRecoveryState,
    kind: crate::cli::note::NoteDeployVoucherKind,
    checkpoint: crate::cli::note::NoteDeployVoucherCheckpoint,
) -> Result<()> {
    recovery.set_voucher_checkpoint(kind, checkpoint)?;
    crate::cli::note::write_note_deploy_recovery(recovery_path, recovery)
}

#[cfg(feature = "shellnet")]
#[allow(clippy::too_many_arguments)]
async fn note_deploy_build_voucher_submit_boc(
    endpoint: &str,
    multisig_address: &dexdo_core::Address,
    multisig_keys: &dexdo_core::KeyPair,
    root_pn: &dexdo_core::Address,
    checkpoint: &crate::cli::note::NoteDeployVoucherCheckpoint,
    http: &reqwest::Client,
) -> Result<String> {
    use dexdo_core::{
        airegistry::{
            calls::{encode_external_call, encode_internal_payload},
            deploy::local_context,
        },
        private_note::artifacts::ROOT_PN_ABI_JSON,
    };

    let ctx = local_context()?;
    let voucher_body = encode_internal_payload(
        &ctx,
        ROOT_PN_ABI_JSON,
        "generateVoucher",
        serde_json::json!({
            "skUCommit": format!("0x{}", checkpoint.sk_u_commit_hex),
            "isFee": checkpoint.is_fee,
        }),
    )
    .await
    .map_err(|e| anyhow::anyhow!("encode RootPN.generateVoucher body: {e}"))?;

    let mut cc = serde_json::Map::new();
    cc.insert(
        checkpoint.token_type.to_string(),
        serde_json::Value::String(checkpoint.raw_value.to_string()),
    );
    let wallet_code_hash =
        note_deploy_fetch_wallet_code_hash(http, endpoint, multisig_address).await?;
    let forward_kind = NoteDeployMultisigForwardKind::from_code_hash(&wallet_code_hash)?;
    let boc = encode_external_call(
        &ctx,
        forward_kind.abi_json(),
        &multisig_address.with_workchain(),
        "sendTransaction",
        forward_kind.send_transaction_params(root_pn, cc, voucher_body),
        multisig_keys.public_hex(),
        multisig_keys.secret_hex(),
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!("encode Multisig.sendTransaction -> RootPN.generateVoucher: {e}")
    })?;
    Ok(boc)
}

#[cfg(feature = "shellnet")]
async fn note_deploy_submit_voucher_boc(
    endpoint: &str,
    multisig_address: &dexdo_core::Address,
    boc: &str,
    http: &reqwest::Client,
) -> Result<()> {
    use dexdo_core::ackinacki_wallet::query::send_message_routed;
    send_message_routed(
        http,
        endpoint,
        boc,
        multisig_address.bare(),
        multisig_address.bare(),
        None,
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!("submit Multisig.sendTransaction -> RootPN.generateVoucher: {e}")
    })?;
    Ok(())
}

#[cfg(feature = "shellnet")]
#[allow(clippy::too_many_arguments)]
async fn note_deploy_mint_voucher_recoverable(
    client: &dexdo_core::ChainClient,
    recovery_path: &std::path::Path,
    recovery: &mut crate::cli::note::NoteDeployRecoveryState,
    kind: crate::cli::note::NoteDeployVoucherKind,
    multisig_address: &dexdo_core::Address,
    multisig_keys: &dexdo_core::KeyPair,
    recipient_ephemeral_pubkey_hex: &str,
    voucher_token_type: u32,
    voucher_value: u64,
    is_fee: bool,
    halo2_paths: &dexdo_core::private_note::Halo2Paths,
    failpoints: NoteDeployVoucherFailpoints,
) -> Result<dexdo_core::private_note::halo2::live::Halo2Proof> {
    use dexdo_core::private_note::{
        artifacts::ROOT_PN_ADDRESS,
        halo2::{
            live::{prove_voucher_for_event, ProveVoucherForEventParams},
            sk_commit::compute_sk_u_commit_hex,
        },
        proof, voucher_event,
    };
    use std::time::Duration;

    let endpoint = client.endpoint();
    let root_pn = dexdo_core::Address::parse(ROOT_PN_ADDRESS)?;
    let recipient_ephemeral_pubkey_hex = proof::strip_0x(recipient_ephemeral_pubkey_hex);
    let mut checkpoint = match recovery.voucher_checkpoint(kind).cloned() {
        Some(checkpoint) => {
            checkpoint.ensure_matches(
                kind,
                recipient_ephemeral_pubkey_hex,
                voucher_token_type,
                voucher_value,
                is_fee,
            )?;
            checkpoint
        }
        None => {
            let sk_u_hex = proof::random_secret_key();
            let sk_u_commit_hex = compute_sk_u_commit_hex(&sk_u_hex)
                .map_err(|e| anyhow::anyhow!("compute {} voucher skUCommit: {e}", kind.label()))?;
            let checkpoint = crate::cli::note::NoteDeployVoucherCheckpoint::new(
                recipient_ephemeral_pubkey_hex,
                voucher_token_type,
                voucher_value,
                is_fee,
                sk_u_hex,
                sk_u_commit_hex,
            )?;
            note_deploy_persist_voucher_checkpoint(
                recovery_path,
                recovery,
                kind,
                checkpoint.clone(),
            )?;
            eprintln!(
                "note deploy recovery: recorded {} voucher checkpoint in {} before wallet spend.",
                kind.label(),
                recovery_path.display()
            );
            checkpoint
        }
    };

    if let Some(proof) = checkpoint.proof.as_ref() {
        eprintln!(
            "note deploy recovery: reusing persisted {} voucher proof from {}; no wallet spend will be submitted.",
            kind.label(),
            recovery_path.display()
        );
        return Ok(proof.to_halo2());
    }

    let http = reqwest::Client::new();
    if checkpoint.event.is_none() {
        if !checkpoint.submit_maybe_sent {
            let boc = note_deploy_build_voucher_submit_boc(
                endpoint,
                multisig_address,
                multisig_keys,
                &root_pn,
                &checkpoint,
                &http,
            )
            .await?;
            checkpoint.submit_maybe_sent = true;
            note_deploy_persist_voucher_checkpoint(
                recovery_path,
                recovery,
                kind,
                checkpoint.clone(),
            )?;
            eprintln!(
                "note deploy recovery: marked {} voucher wallet submit as uncertain in {}; reruns will not submit a second wallet spend.",
                kind.label(),
                recovery_path.display()
            );
            note_deploy_submit_voucher_boc(endpoint, multisig_address, &boc, &http).await?;
            if failpoints.after_submit(kind) {
                bail!(
                    "simulated interruption after {} voucher wallet submit. Recovery state is at {}; rerun `dexdo note deploy --recovery <this-file> --pool <pool>` to resume without a second wallet spend.",
                    kind.label(),
                    recovery_path.display()
                );
            }
        } else {
            eprintln!(
                "note deploy recovery: resuming {} voucher from {}; waiting/proving the existing skUCommit without submitting another wallet spend.",
                kind.label(),
                recovery_path.display()
            );
        }

        let event = voucher_event::wait_for_voucher_event_by_sk_u_commit(
            &http,
            endpoint,
            &root_pn,
            &format!("0x{}", checkpoint.sk_u_commit_hex),
            Duration::from_secs(NOTE_DEPLOY_VOUCHER_EVENT_TIMEOUT_SECS),
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "wait for {} VoucherGenerated from persisted wallet submit: {e}; refusing to submit a second wallet spend for recovery {}",
                kind.label(),
                recovery_path.display()
            )
        })?;
        checkpoint.event = Some(crate::cli::note::NoteDeployVoucherEvent::from_sdk(event));
        note_deploy_persist_voucher_checkpoint(recovery_path, recovery, kind, checkpoint.clone())?;
        eprintln!(
            "note deploy recovery: recorded {} VoucherGenerated event in {}; reruns will prove this voucher without a second wallet spend.",
            kind.label(),
            recovery_path.display()
        );
        if failpoints.after_event(kind) {
            bail!(
                "simulated interruption after {} VoucherGenerated event before proof/deploy. Recovery state is at {}; rerun `dexdo note deploy --recovery <this-file> --pool <pool>` to resume without a second wallet spend.",
                kind.label(),
                recovery_path.display()
            );
        }
    }

    let event = checkpoint
        .event
        .as_ref()
        .ok_or_else(|| {
            anyhow::anyhow!("{} voucher event missing after recovery wait", kind.label())
        })?
        .to_sdk();
    let proof = prove_voucher_for_event(ProveVoucherForEventParams {
        endpoint: endpoint.to_string(),
        event,
        sk_u_hex: checkpoint.sk_u_hex.clone(),
        sk_u_commit_hex: checkpoint.sk_u_commit_hex.clone(),
        voucher_value,
        voucher_token_type,
        ephemeral_pubkey_hex: recipient_ephemeral_pubkey_hex.to_string(),
        history_proof_window_size: None,
        paths: halo2_paths,
    })
    .await
    .map_err(|e| anyhow::anyhow!("prove {} voucher: {e}", kind.label()))?;
    checkpoint.proof = Some(crate::cli::note::NoteDeployVoucherProof::from_halo2(&proof));
    note_deploy_persist_voucher_checkpoint(recovery_path, recovery, kind, checkpoint)?;
    eprintln!(
        "note deploy recovery: recorded {} voucher proof in {}; reruns will not re-spend this voucher.",
        kind.label(),
        recovery_path.display()
    );
    Ok(proof)
}

#[cfg(feature = "shellnet")]
async fn note_deploy_submit_private_note(
    client: &dexdo_core::ChainClient,
    root_pn: &dexdo_core::Address,
    pn_keys: &dexdo_core::KeyPair,
    deposit_zk: &dexdo_core::private_note::halo2::live::Halo2Proof,
    deposit_identifier_hash: &str,
) -> Result<()> {
    use dexdo_core::private_note::{
        artifacts::ROOT_PN_ABI_JSON,
        proof::{hex_u256_to_dec, pubkey_to_dec},
    };

    client
        .call(
            root_pn,
            ROOT_PN_ABI_JSON,
            "deployPrivateNote",
            serde_json::json!({
                "zkproof": deposit_zk.proof,
                "depositIdentifierHash": deposit_identifier_hash,
                "finalLayerHistoricalHashRoot": hex_u256_to_dec(&deposit_zk.final_layer_historical_hash_root_hex)?,
                "voucherNominalFr": hex_u256_to_dec(&deposit_zk.voucher_nominal_fr_hex)?,
                "tokenTypeFr": hex_u256_to_dec(&deposit_zk.token_type_fr_hex)?,
                "ephemeralPubkey": pubkey_to_dec(pn_keys.public_hex())?,
                "value": deposit_zk.voucher_value,
                "tokenType": deposit_zk.voucher_token_type,
                "layerNumber": deposit_zk.layer_number,
            }),
            pn_keys,
        )
        .await
        .map_err(|e| anyhow::anyhow!("RootPN.deployPrivateNote: {e}"))
        .map(|_| ())
}

#[cfg(feature = "shellnet")]
async fn deploy_private_note_from_multisig_recoverable(
    client: &dexdo_core::ChainClient,
    recovery_path: &std::path::Path,
    recovery: &mut crate::cli::note::NoteDeployRecoveryState,
    multisig_address: &dexdo_core::Address,
    multisig_keys: &dexdo_core::KeyPair,
    pn_keys: &dexdo_core::KeyPair,
    halo2_paths: &dexdo_core::private_note::Halo2Paths,
    failpoints: NoteDeployVoucherFailpoints,
) -> Result<crate::cli::note::OnboardPnState> {
    use dexdo_core::private_note::{
        artifacts::{PRIVATE_NOTE_ABI_JSON, ROOT_PN_ABI_JSON, ROOT_PN_ADDRESS},
        proof::{hex_u256_to_dec, pubkey_to_dec, CURRENCY_ID_SHELL, ECC_SHELL_DEPOSIT_RAW},
    };
    use dexdo_core::Address;
    use serde_json::json;
    use std::time::Duration;

    if recovery.shell_funded && recovery.sanity_checked {
        recovery.ensure_ready_for_pool()?;
        return recovery.to_onboard_state();
    }

    let root_pn = Address::parse(ROOT_PN_ADDRESS)?;
    let mut resumed_existing_note = false;
    let (pn_address, deposit_identifier_hash) = match (
        recovery.pn_address.clone(),
        recovery.deposit_identifier_hash.clone(),
    ) {
        (Some(pn_address), Some(deposit_identifier_hash)) => {
            resumed_existing_note = true;
            eprintln!(
                "note deploy recovery: PrivateNote {pn_address} is already recorded in {}; skipping \
                 deployPrivateNote spend and resuming later steps.",
                recovery_path.display()
            );
            (pn_address, deposit_identifier_hash)
        }
        (None, None) => {
            eprintln!(
                "note deploy recovery: no on-chain PrivateNote recorded yet; continuing deploy with the \
                 persisted owner key in {}.",
                recovery_path.display()
            );
            let deposit_token_type = recovery.token_type;
            let deposit_raw_value = recovery.raw_value;
            let had_persisted_deposit_proof = recovery
                .voucher_checkpoint(crate::cli::note::NoteDeployVoucherKind::Deposit)
                .and_then(|checkpoint| checkpoint.proof.as_ref())
                .is_some();
            let deposit_zk = note_deploy_mint_voucher_recoverable(
                client,
                recovery_path,
                recovery,
                crate::cli::note::NoteDeployVoucherKind::Deposit,
                multisig_address,
                multisig_keys,
                pn_keys.public_hex(),
                deposit_token_type,
                deposit_raw_value,
                false,
                halo2_paths,
                failpoints,
            )
            .await
            .map_err(|e| anyhow::anyhow!("halo2 deposit voucher: {e}"))?;

            let dih_dec = hex_u256_to_dec(&deposit_zk.deposit_identifier_hash_hex)?;
            if had_persisted_deposit_proof {
                let pn_address = note_deploy_private_note_address(client, &root_pn, &dih_dec)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "RootPN.getPrivateNoteAddress before repeat deployPrivateNote: {e}"
                        )
                    })?;
                let pn = Address::parse(&pn_address)?;
                if note_deploy_wait_existing_active(client, &pn, Duration::from_secs(120)).await? {
                    let deployed_at_unix = note_deploy_now_unix()?;
                    recovery.mark_private_note_deployed(
                        pn_address.clone(),
                        dih_dec.clone(),
                        deployed_at_unix,
                    )?;
                    crate::cli::note::write_note_deploy_recovery(recovery_path, recovery)?;
                    eprintln!(
                        "note deploy recovery: recovered active PrivateNote {pn_address} from persisted \
                         deposit proof in {}; skipping repeat deployPrivateNote submit.",
                        recovery_path.display()
                    );
                    resumed_existing_note = true;
                    (pn_address, dih_dec)
                } else {
                    eprintln!(
                        "note deploy recovery: persisted deposit proof in {} has no active PrivateNote yet; \
                         submitting deployPrivateNote once.",
                        recovery_path.display()
                    );
                    note_deploy_submit_private_note(
                        client,
                        &root_pn,
                        pn_keys,
                        &deposit_zk,
                        &dih_dec,
                    )
                    .await?;

                    let pn_address =
                        note_deploy_private_note_address(client, &root_pn, &dih_dec).await?;
                    let pn = Address::parse(&pn_address)?;
                    note_deploy_wait_active(client, &pn, Duration::from_secs(120)).await?;
                    if failpoints.after_deploy_before_note_record {
                        bail!(
                            "simulated interruption after deployPrivateNote active before recovery note record. \
                             Recovery state is at {}; rerun `dexdo note deploy --recovery <this-file> \
                             --pool <pool>` to discover the active PrivateNote without repeating deployPrivateNote.",
                            recovery_path.display()
                        );
                    }
                    let deployed_at_unix = note_deploy_now_unix()?;
                    recovery.mark_private_note_deployed(
                        pn_address.clone(),
                        dih_dec.clone(),
                        deployed_at_unix,
                    )?;
                    crate::cli::note::write_note_deploy_recovery(recovery_path, recovery)?;
                    eprintln!(
                        "note deploy recovery: recorded deployed PrivateNote {pn_address} in {}; a later recovery \
                         will not repeat deployPrivateNote.",
                        recovery_path.display()
                    );
                    (pn_address, dih_dec)
                }
            } else {
                note_deploy_submit_private_note(client, &root_pn, pn_keys, &deposit_zk, &dih_dec)
                    .await?;

                let pn_address =
                    note_deploy_private_note_address(client, &root_pn, &dih_dec).await?;
                let pn = Address::parse(&pn_address)?;
                note_deploy_wait_active(client, &pn, Duration::from_secs(120)).await?;
                if failpoints.after_deploy_before_note_record {
                    bail!(
                        "simulated interruption after deployPrivateNote active before recovery note record. \
                         Recovery state is at {}; rerun `dexdo note deploy --recovery <this-file> --pool <pool>` \
                         to discover the active PrivateNote without repeating deployPrivateNote.",
                        recovery_path.display()
                    );
                }
                let deployed_at_unix = note_deploy_now_unix()?;
                recovery.mark_private_note_deployed(
                    pn_address.clone(),
                    dih_dec.clone(),
                    deployed_at_unix,
                )?;
                crate::cli::note::write_note_deploy_recovery(recovery_path, recovery)?;
                eprintln!(
                    "note deploy recovery: recorded deployed PrivateNote {pn_address} in {}; a later recovery \
                     will not repeat deployPrivateNote.",
                    recovery_path.display()
                );
                (pn_address, dih_dec)
            }
        }
        _ => {
            bail!(
                "note deploy recovery {} is inconsistent: pn_address and deposit_identifier_hash must both be \
                 present or both absent",
                recovery_path.display()
            );
        }
    };

    if !recovery.shell_funded {
        let pn = Address::parse(&pn_address)?;
        let expected_shell = recovery.ecc_shell_deposit as u128;
        let already_funded = resumed_existing_note
            && note_deploy_wait_existing_shell_funding(
                client,
                &pn,
                expected_shell,
                Duration::from_secs(60),
            )
            .await?;
        if already_funded {
            eprintln!(
                "note deploy recovery: PrivateNote {pn_address} already has expected ECC[2] funding; \
                 skipping sendEccShellToPrivateNote spend."
            );
        } else {
            let gas_zk = note_deploy_mint_voucher_recoverable(
                client,
                recovery_path,
                recovery,
                crate::cli::note::NoteDeployVoucherKind::ShellGas,
                multisig_address,
                multisig_keys,
                pn_keys.public_hex(),
                CURRENCY_ID_SHELL,
                ECC_SHELL_DEPOSIT_RAW,
                true,
                halo2_paths,
                failpoints,
            )
            .await
            .map_err(|e| anyhow::anyhow!("halo2 SHELL gas voucher: {e}"))?;

            client
                .call(
                    &root_pn,
                    ROOT_PN_ABI_JSON,
                    "sendEccShellToPrivateNote",
                    json!({
                        "proof": gas_zk.proof,
                        "nullifierHash": hex_u256_to_dec(&gas_zk.deposit_identifier_hash_hex)?,
                        "depositIdentifierHash": deposit_identifier_hash,
                        "finalLayerHistoricalHashRoot": hex_u256_to_dec(&gas_zk.final_layer_historical_hash_root_hex)?,
                        "voucherNominalFr": hex_u256_to_dec(&gas_zk.voucher_nominal_fr_hex)?,
                        "tokenTypeFr": hex_u256_to_dec(&gas_zk.token_type_fr_hex)?,
                        "value": gas_zk.voucher_value,
                        "layerNumber": gas_zk.layer_number,
                        "recipientEphemeralPubkey": pubkey_to_dec(pn_keys.public_hex())?,
                    }),
                    pn_keys,
                )
                .await
                .map_err(|e| anyhow::anyhow!("RootPN.sendEccShellToPrivateNote: {e}"))?;
            if !note_deploy_wait_existing_shell_funding(
                client,
                &pn,
                expected_shell,
                Duration::from_secs(180),
            )
            .await?
            {
                bail!(
                    "PrivateNote {pn_address} did not show expected ECC[2] funding {expected_shell} within \
                     180s after sendEccShellToPrivateNote; recovery state was left unfinalized so rerun \
                     `dexdo note deploy --recovery {}` before pooling.",
                    recovery_path.display()
                );
            }
        }
    }

    let pn = Address::parse(&pn_address)?;
    client
        .run_getter(&pn, PRIVATE_NOTE_ABI_JSON, "getDetails", json!({}))
        .await?
        .ok_or_else(|| anyhow::anyhow!("PrivateNote.getDetails returned no output"))?;
    recovery.mark_shell_funded_and_checked()?;
    crate::cli::note::write_note_deploy_recovery(recovery_path, recovery)?;
    recovery.to_onboard_state()
}

#[cfg(feature = "shellnet")]
async fn note_deploy_wait_existing_shell_funding(
    client: &dexdo_core::ChainClient,
    note: &dexdo_core::Address,
    expected_shell_ecc: u128,
    timeout: std::time::Duration,
) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(acc) = client.get_account(note).await? {
            if acc.ecc_balance(2) >= expected_shell_ecc {
                return Ok(true);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[cfg(feature = "shellnet")]
async fn note_deploy_wait_existing_active(
    client: &dexdo_core::ChainClient,
    note: &dexdo_core::Address,
    timeout: std::time::Duration,
) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(acc) = client.get_account(note).await? {
            if acc.is_active() {
                return Ok(true);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(feature = "shellnet")]
async fn note_deploy_private_note_address(
    client: &dexdo_core::ChainClient,
    root_pn: &dexdo_core::Address,
    deposit_identifier_hash: &str,
) -> Result<String> {
    use dexdo_core::private_note::artifacts::ROOT_PN_ABI_JSON;
    let out = client
        .run_getter(
            root_pn,
            ROOT_PN_ABI_JSON,
            "getPrivateNoteAddress",
            serde_json::json!({ "depositIdentifierHash": deposit_identifier_hash }),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("RootPN.getPrivateNoteAddress returned no output"))?;
    out.get("privateNoteAddress")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!("RootPN.getPrivateNoteAddress missing privateNoteAddress: {out}")
        })
}

#[cfg(feature = "shellnet")]
async fn note_deploy_wait_active(
    client: &dexdo_core::ChainClient,
    address: &dexdo_core::Address,
    timeout: std::time::Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(acc) = client.get_account(address).await? {
            if acc.is_active() {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "{address} did not become Active within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(feature = "shellnet")]
fn note_deploy_now_unix() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

#[cfg(feature = "shellnet")]
fn note_deploy_fold_state_into_pool(
    pool_path: &std::path::Path,
    state: &crate::cli::note::OnboardPnState,
    funding_multisig_address: &str,
) -> Result<usize> {
    use crate::cli::note::{pn_state_to_pool_note, pool_with_note_added};

    let note = pn_state_to_pool_note(state)?;
    let existing = match std::fs::read(pool_path) {
        Ok(b) => Some(serde_json::from_slice(&b).map_err(|e| {
            anyhow::anyhow!("--pool {} is not valid JSON: {e}", pool_path.display())
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => bail!("read --pool {}: {e}", pool_path.display()),
    };
    let now = note_deploy_now_unix()?;
    let pool = pool_with_note_added(existing, state, note, now, funding_multisig_address)?;
    let pool_json = serde_json::to_string_pretty(&pool)?;
    write_pool_private(pool_path, pool_json.as_bytes())?;
    Ok(pool["notes"].as_array().map(|a| a.len()).unwrap_or(0))
}

/// `dexdo note deploy` -- deploy a wallet-funded `PrivateNote` on shellnet in-process through
/// `gosh.ackinacki`, then fold its result into a `DEXDO_PN_POOL` the `seller`/`buyer` consume. The wallet funding
/// secret is read from `--multisig-key` or derived from `--multisig-seed-file`, then passed directly to the SDK.
/// The seed phrase is never printed/logged/stored. The owner secret lands in the pool file(the consumers need it)
/// but is NEVER printed/logged.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_deploy(args: NoteDeployArgs) -> Result<()> {
    use crate::cli::note::{
        default_note_deploy_recovery_path, derive_owner_pubkey_from_secret_hex,
        ensure_onchain_owner_matches_pool_key, load_note_deploy_recovery,
        recovery_owner_key_written_message, write_note_deploy_recovery, NoteDeployRecoveryRequest,
        NoteDeployRecoveryState, OnboardPnState,
    };
    use dexdo_core::{
        private_note::{
            artifacts::PRIVATE_NOTE_ABI_JSON, proof::ECC_SHELL_DEPOSIT_RAW, Halo2Paths, Nominal,
            TokenType,
        },
        Address, ChainClient, KeyPair,
    };

    note_deploy_same_file_pool_guard(std::env::var_os("DEXDO_PN_POOL").as_deref(), &args.pool)?;
    let funding_multisig_address = dexdo_core::normalize_wallet_address(&args.multisig_address)
        .map_err(|e| anyhow::anyhow!("--multisig-address: {e}"))?;
    Address::parse(&funding_multisig_address)
        .map_err(|e| anyhow::anyhow!("--multisig-address: {e}"))?;
    let nominal = Nominal::parse(&args.nominal)?;
    let token_type = TokenType::parse(&args.token_type)?;
    let nominal_label = nominal.label().to_string();
    let token_type_label = token_type.label().to_string();
    let endpoint = note_endpoint_url(&args.endpoint)?;
    let client = ChainClient::connect(&endpoint)?;
    let _wallet_lock = acquire_note_deploy_wallet_lock(&funding_multisig_address)?;
    let halo2_paths = Halo2Paths::from_env();
    halo2_paths.ensure_srs();
    let recovery_path = args
        .recovery
        .clone()
        .unwrap_or_else(|| default_note_deploy_recovery_path(&args.pool));
    note_deploy_recovery_pool_guard(&args.pool, &recovery_path)?;
    let recovery_request = NoteDeployRecoveryRequest {
        endpoint: &endpoint,
        nominal: &nominal_label,
        token_type: token_type.id(),
        raw_value: nominal.raw_value(token_type),
        ecc_shell_deposit: ECC_SHELL_DEPOSIT_RAW,
        funding_multisig_address: &funding_multisig_address,
    };
    let mut recovery = match load_note_deploy_recovery(&recovery_path)? {
        Some(state) => {
            state.ensure_matches_request(recovery_request)?;
            eprintln!(
                "note deploy recovery: using existing state file {}.",
                recovery_path.display()
            );
            state
        }
        None => {
            let pn_keys = KeyPair::generate();
            let state = NoteDeployRecoveryState::new(
                recovery_request,
                pn_keys.public_hex(),
                pn_keys.secret_hex(),
            )?;
            write_note_deploy_recovery(&recovery_path, &state)?;
            state
        }
    };
    eprintln!("{}", recovery_owner_key_written_message(&recovery_path));
    let pn_keys = KeyPair::from_secret_hex(&recovery.owner_secret_key_hex)
        .map_err(|e| anyhow::anyhow!("note deploy recovery owner key: {e:?}"))?;

    eprintln!(
        "note deploy: in-process gosh.ackinacki -- wallet {} funds a {} {} PrivateNote on {} ...",
        funding_multisig_address, nominal_label, token_type_label, endpoint
    );
    let voucher_failpoints = NoteDeployVoucherFailpoints {
        after_deposit_submit: args.simulate_interrupt_after_deposit_voucher_submit,
        after_deposit_event: args.simulate_interrupt_after_deposit_voucher_event,
        after_shell_submit: args.simulate_interrupt_after_shell_voucher_submit,
        after_deploy_before_note_record: args.simulate_interrupt_after_deploy_before_note_record,
    };

    let state: OnboardPnState = {
        let mut attempt = 1u64;
        loop {
            let multisig_address = Address::parse(&funding_multisig_address)
                .map_err(|e| anyhow::anyhow!("--multisig-address: {e}"))?;
            let multisig_keys = note_deploy_multisig_keys(&args)?;
            match deploy_private_note_from_multisig_recoverable(
                &client,
                &recovery_path,
                &mut recovery,
                &multisig_address,
                &multisig_keys,
                &pn_keys,
                &halo2_paths,
                voucher_failpoints,
            )
            .await
            {
                Ok(state) => break state,
                Err(error) => {
                    if is_note_deploy_wallet_busy_error(&error) && attempt < 3 {
                        let backoff_secs = attempt.saturating_mul(10);
                        eprintln!(
                            "note deploy: funding wallet {funding_multisig_address} looks busy/out-of-sync; \
                             retrying attempt {} after {backoff_secs}s",
                            attempt + 1
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    return Err(note_deploy_error(&funding_multisig_address, error));
                }
            }
        }
    };
    let note_addr = state
        .pn_address
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!("pn_state has no pn_address -- note deploy did not complete")
        })?
        .to_string();
    let owner_secret = state.owner_secret_key_hex.as_deref().ok_or_else(|| {
        anyhow::anyhow!("pn_state has no owner_secret_key_hex -- incomplete note deploy")
    })?;
    let derived_owner = derive_owner_pubkey_from_secret_hex(owner_secret)?;
    let note_address = Address::parse(&note_addr)
        .map_err(|e| anyhow::anyhow!("deployed note {note_addr}: {e}"))?;
    let details = client
        .run_getter(
            &note_address,
            PRIVATE_NOTE_ABI_JSON,
            "getDetails",
            serde_json::json!({}),
        )
        .await
        .map_err(|e| anyhow::anyhow!("verify deployed PrivateNote {note_addr} owner key: {e}"))?;
    ensure_onchain_owner_matches_pool_key(
        "note deploy",
        &note_addr,
        details.as_ref().and_then(|d| d["ephemeralPubkey"].as_str()),
        &derived_owner,
    )?;
    if args.simulate_interrupt_after_spend_before_pool {
        bail!(
            "simulated interruption after on-chain spend before final pool write. Recovery state is complete at {}; \
             run `dexdo note recover --recovery {} --pool {}` to finalize without re-spending.",
            recovery_path.display(),
            recovery_path.display(),
            args.pool.display()
        );
    }

    let n = note_deploy_fold_state_into_pool(&args.pool, &state, &funding_multisig_address)?;
    println!(
        "note deployed -> PrivateNote {note_addr} ({} {}); folded into --pool {} ({} note(s)). Recovery state is \
         at {}. The owner secret is stored in the pool for the seller/buyer -- keep both files private.",
        state.nominal,
        state.token_type,
        args.pool.display(),
        n,
        recovery_path.display()
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_deploy(_args: NoteDeployArgs) -> Result<()> {
    bail!("note deploy unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_recover(args: NoteRecoverArgs) -> Result<()> {
    use crate::cli::note::{
        derive_owner_pubkey_from_secret_hex, ensure_onchain_owner_matches_pool_key,
        load_note_deploy_recovery,
    };
    use dexdo_core::{private_note::artifacts::PRIVATE_NOTE_ABI_JSON, Address, ChainClient};

    note_deploy_recovery_pool_guard(&args.pool, &args.recovery)?;
    let recovery = load_note_deploy_recovery(&args.recovery)?.ok_or_else(|| {
        anyhow::anyhow!(
            "note recover: recovery file {} not found",
            args.recovery.display()
        )
    })?;
    recovery.ensure_ready_for_pool()?;
    let state = recovery.to_onboard_state()?;
    let note_addr = state
        .pn_address
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("note recover: recovery state has no pn_address"))?
        .to_string();
    let owner_secret = state.owner_secret_key_hex.as_deref().ok_or_else(|| {
        anyhow::anyhow!("note recover: recovery state has no owner_secret_key_hex")
    })?;
    let derived_owner = derive_owner_pubkey_from_secret_hex(owner_secret)?;
    let client = ChainClient::connect(&recovery.endpoint)?;
    let note_address = Address::parse(&note_addr)
        .map_err(|e| anyhow::anyhow!("recovered note {note_addr}: {e}"))?;
    let details = client
        .run_getter(
            &note_address,
            PRIVATE_NOTE_ABI_JSON,
            "getDetails",
            serde_json::json!({}),
        )
        .await
        .map_err(|e| anyhow::anyhow!("verify recovered PrivateNote {note_addr} owner key: {e}"))?;
    ensure_onchain_owner_matches_pool_key(
        "note recover",
        &note_addr,
        details.as_ref().and_then(|d| d["ephemeralPubkey"].as_str()),
        &derived_owner,
    )?;
    let n =
        note_deploy_fold_state_into_pool(&args.pool, &state, &recovery.funding_multisig_address)?;
    println!(
        "note recovered -> PrivateNote {note_addr}; folded into --pool {} ({} note(s)) from recovery {}. \
         No wallet spend was submitted.",
        args.pool.display(),
        n,
        args.recovery.display()
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_recover(_args: NoteRecoverArgs) -> Result<()> {
    bail!("note recover unavailable: build with `--features shellnet`")
}

/// `dexdo note balance`: address-only, read-only PrivateNote balance diagnostics.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_balance(args: NoteBalanceArgs) -> Result<()> {
    use crate::cli::note::{
        build_note_balance_view, note_getter_balance_maps, render_note_balance,
        unknown_note_getter_balance_maps, NoteAccountSnapshot,
    };
    use dexdo_core::{Address, RealChainBackend};

    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let note = Address::parse(&args.note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {}: {e}", args.note_addr))?;
    let note_display = note.with_workchain();
    let chain = RealChainBackend::connect(manifest)?;
    let account = chain
        .client()
        .get_account(&note)
        .await
        .map_err(|e| anyhow::anyhow!("read PrivateNote account {note_display}: {e}"))?;
    if account.is_none() {
        build_note_balance_view(
            &note_display,
            None,
            unknown_note_getter_balance_maps("account was not readable"),
        )?;
    }
    let details = match chain.private_note_details(&note).await {
        Ok(details) => note_getter_balance_maps(details.as_ref()),
        Err(e) => unknown_note_getter_balance_maps(format!("getDetails error: {e}")),
    };
    let account = account.map(|a| NoteAccountSnapshot {
        address: a.address.with_workchain(),
        status: a.status,
        native_raw: a.balance,
        ecc: a.ecc,
        code_hash: a.code_hash,
    });
    let view = build_note_balance_view(&note_display, account, details)?;
    print!("{}", render_note_balance(&view));
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_balance(_args: NoteBalanceArgs) -> Result<()> {
    bail!("note balance unavailable: build with `--features shellnet`")
}

/// `dexdo note withdraw`: submit owner-signed `PrivateNote.withdrawTokens(destWalletAddr, dapp_id)` for a note's
/// available token balances. It is one-shot and not a blanket proof that every native/ECC balance is retired
/// without by-fact evidence on the current contract. `--to` accepts `half1::half2` or `0:<hex>`.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_withdraw(args: NoteWithdrawArgs) -> Result<()> {
    use dexdo_core::{normalize_wallet_address, Address, KeyPair, RealChainBackend};
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("real shellnet: --note-addr (the note to withdraw from) is required")
    })?;
    let note_key =
        args.identity.note_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-key (note owner key) is required")
        })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // Normalize the destination before touching the chain.
    let dest = normalize_wallet_address(&args.to).map_err(|e| anyhow::anyhow!("--to: {e}"))?;
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let dest_addr = Address::parse(&dest).map_err(|e| anyhow::anyhow!("--to {dest}: {e}"))?;
    chain
        .assert_note_owner_matches("note withdraw", &note, &keys)
        .await?;
    // Fund-safety: a note from a previous contract generation accepts withdrawTokens,
    // zeroes its balance, but never credits the destination -- the SHELL is lost. Fail closed before
    // any on-chain write when the note's code_hash is not the current generation.
    chain.assert_note_withdraw_generation(&note).await?;
    println!("withdrawing note {note_addr} token balances -> {dest}");
    chain.withdraw_note_tokens(&note, &keys, &dest_addr).await?;
    println!("withdrawTokens submitted for note {note_addr} -> {dest}");
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_withdraw(_args: NoteWithdrawArgs) -> Result<()> {
    bail!("note withdraw unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
fn load_oracle_market_manifest(path: &std::path::Path) -> Result<dexdo_core::OracleMarketManifest> {
    let json = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read --manifest {}: {e}", path.display()))?;
    let manifest = dexdo_core::OracleMarketManifest::from_json(&json)
        .map_err(|e| anyhow::anyhow!("parse --manifest {}: {e}", path.display()))?;
    manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("--manifest {}: {e}", path.display()))?;
    Ok(manifest)
}

#[cfg(feature = "shellnet")]
fn pmp_resolved_outcome(details: &serde_json::Value) -> Option<String> {
    let v = &details["resolvedOutcome"];
    if v.is_null() {
        return None;
    }
    v.as_str()
        .map(str::to_string)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| {
            v.as_object()
                .and_then(|o| o.get("value").or_else(|| o.get("0")))
                .and_then(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .or_else(|| x.as_u64().map(|n| n.to_string()))
                })
        })
}

#[cfg(feature = "shellnet")]
fn now_unix_secs() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

#[cfg(feature = "shellnet")]
fn validate_oracle_deadline(deadline: u64, now: u64) -> Result<()> {
    let min_deadline = now.saturating_add(ORACLE_MIN_RESULT_GAP_SECS);
    if deadline < min_deadline {
        bail!(
            "oracle provision: --deadline {deadline} must be at least {ORACLE_MIN_RESULT_GAP_SECS}s \
             in the future for OracleEventList.addRangeEvent (now={now}, min={min_deadline})"
        );
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_oracle(args: OracleArgs) -> Result<()> {
    match args.command {
        OracleCommand::Provision(p) => run_oracle_provision(*p).await,
        OracleCommand::State(s) => run_oracle_state(s).await,
        OracleCommand::Resolve(r) => run_oracle_resolve(r).await,
    }
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_oracle(_args: OracleArgs) -> Result<()> {
    bail!("oracle unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
async fn run_oracle_provision(args: OracleProvisionArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    if args.outcome_names.len() != args.bounds.len() + 1 {
        bail!(
            "oracle provision: pass exactly bounds.len()+1 --outcome values (got {}, expected {})",
            args.outcome_names.len(),
            args.bounds.len() + 1
        );
    }
    if args.initial_stakes.len() != args.outcome_names.len() {
        bail!(
            "oracle provision: pass exactly one --initial-stake per outcome (got {}, expected {})",
            args.initial_stakes.len(),
            args.outcome_names.len()
        );
    }
    validate_oracle_deadline(args.deadline, now_unix_secs()?)?;
    shellnet_doctor_preflight(&args.contracts, Some(args.market.as_path())).await?;

    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("oracle provision: --note-addr (PMP deployer PrivateNote) is required")
    })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("oracle provision: --note-key (PMP deployer note owner key) is required")
    })?;
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let market = load_market(&args.market)?;
    let note_seed = read_secret_hex(note_key, "--note-key")?;
    let oracle_seed = read_secret_hex(&args.oracle_key, "--oracle-key")?;
    let note_keys = KeyPair::from_secret_hex(note_seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let oracle_keys = KeyPair::from_secret_hex(oracle_seed.trim())
        .map_err(|e| anyhow::anyhow!("--oracle-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let manifest = chain
        .provision_oracle_market(
            &note_keys,
            &note,
            &oracle_keys,
            &args.oracle_name,
            args.event_list_index,
            &args.event_list_description,
            &args.event_name,
            args.oracle_fee,
            args.deadline,
            &args.describe,
            &args.bounds,
            &args.outcome_names,
            &market,
            args.token_type,
            &args.initial_stakes,
        )
        .await?;
    let json = manifest.to_json()?;
    std::fs::write(&args.output, &json)
        .map_err(|e| anyhow::anyhow!("write --output {}: {e}", args.output.display()))?;
    println!("oracle market provisioned -> {}", args.output.display());
    println!("{json}");
    Ok(())
}

#[cfg(feature = "shellnet")]
async fn run_oracle_state(args: OracleStateArgs) -> Result<()> {
    use dexdo_core::{Address, RealChainBackend};
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let oel = Address::parse(&manifest.oracle_event_list)
        .map_err(|e| anyhow::anyhow!("oracle_event_list {}: {e}", manifest.oracle_event_list))?;
    let pmp =
        Address::parse(&manifest.pmp).map_err(|e| anyhow::anyhow!("pmp {}: {e}", manifest.pmp))?;
    let range = chain.oracle_range_data(&oel, &manifest.event_id).await?;
    let details = chain.pmp_details(&pmp).await?;
    let pmp_ob = chain.pmp_order_book_address(&pmp).await?;
    println!(
        "oracle_state event={} pmp={} token_type={} deadline={} frame_model={} inference_ob={}",
        manifest.event_id,
        manifest.pmp,
        manifest.token_type,
        manifest.deadline,
        manifest.frame_model,
        manifest.inference_order_book
    );
    match range {
        Some(r) => println!("range_data={}", serde_json::to_string(&r)?),
        None => println!("range_data=<inactive-or-missing>"),
    }
    match details {
        Some(d) => {
            let resolved = pmp_resolved_outcome(&d).unwrap_or_else(|| "none".to_string());
            println!(
                "pmp_details approved={} approved_oracles={}/{} resolved_outcome={} raw={}",
                d["approved"].as_bool().unwrap_or(false),
                d["approvedOracleEvents"].as_str().unwrap_or("0"),
                d["numberOfOracleEvents"].as_str().unwrap_or("0"),
                resolved,
                serde_json::to_string(&d)?
            );
        }
        None => println!("pmp_details=<inactive-or-missing>"),
    }
    if let Some(ob) = pmp_ob {
        println!("pmp_order_book={}", ob.with_workchain());
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
async fn run_oracle_resolve(args: OracleResolveArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    let now = now_unix_secs()?;
    if now < manifest.deadline {
        bail!(
            "oracle resolve: deadline not reached (deadline={}, now={now})",
            manifest.deadline
        );
    }
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let oel = Address::parse(&manifest.oracle_event_list)
        .map_err(|e| anyhow::anyhow!("oracle_event_list {}: {e}", manifest.oracle_event_list))?;
    let pmp =
        Address::parse(&manifest.pmp).map_err(|e| anyhow::anyhow!("pmp {}: {e}", manifest.pmp))?;
    let oracle_seed = read_secret_hex(&args.oracle_key, "--oracle-key")?;
    let oracle_keys = KeyPair::from_secret_hex(oracle_seed.trim())
        .map_err(|e| anyhow::anyhow!("--oracle-key (SDK secret hex): {e:?}"))?;
    chain
        .resolve_oracle_range(
            &oel,
            &oracle_keys,
            &manifest.event_id,
            &manifest.oracle_list_hash,
            manifest.token_type,
        )
        .await?;
    println!(
        "resolveRange submitted event={} oracle_list_hash={} pmp={}",
        manifest.event_id, manifest.oracle_list_hash, manifest.pmp
    );
    let mut last_details_error = None;
    for i in 0..60 {
        match chain.pmp_details(&pmp).await {
            Ok(Some(details)) => {
                if let Some(outcome) = pmp_resolved_outcome(&details) {
                    println!(
                        "pmp resolved event={} outcome={} pmp={}",
                        manifest.event_id, outcome, manifest.pmp
                    );
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("pmp details poll failed (will retry): {e}");
                last_details_error = Some(e.to_string());
            }
        }
        if i + 1 < 60 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
    let last_details_error = last_details_error
        .map(|e| format!(" Last transient pmp_details error while polling: {e}."))
        .unwrap_or_default();
    bail!(
        "resolveRange was submitted but PMP {} did not expose resolvedOutcome within 180s. \
         If the bound InferenceOrderBook has no MIN_LIQUIDITY, requestWeeklyMedian reverts under bounce:false \
         and onWeeklyMedian never arrives; this is the  no-liquidity stuck case, not a CLI success.{}",
        manifest.pmp,
        last_details_error
    )
}

#[cfg(test)]
mod tests {
    use crate::cli::args::SubscriptionPlaceArgs;
    #[cfg(feature = "shellnet")]
    use crate::cli::args::{IdentityArgs, NoteDeployArgs};

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
    #[test]
    fn executable_book_line_includes_selection_fields() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };
        let order = dexdo_core::OrderBookOrder {
            order_id: 7,
            owner_note: "0:seller".to_string(),
            token_contract: Some("0:tc".to_string()),
            is_buy: false,
            price_per_tick: 42,
            ticks: 1024,
            escrow: 0,
            deadline: 0,
            flags: 0,
            timestamp: 0,
        };

        let line = super::render_executable_book_line(&snapshot, &order, 8, 50);

        assert!(line.contains("executable_ask"), "{line}");
        assert!(line.contains("order_id=7"), "{line}");
        assert!(line.contains("token_contract=0:tc"), "{line}");
        assert!(line.contains("price_per_tick=42"), "{line}");
        assert!(line.contains("ticks=1024"), "{line}");
        assert!(line.contains("requested_ticks=8"), "{line}");
        assert!(line.contains("max_price_per_tick=50"), "{line}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn executable_book_output_includes_multiple_rows() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };
        let orders = vec![
            dexdo_core::OrderBookOrder {
                order_id: 7,
                owner_note: "0:seller-a".to_string(),
                token_contract: Some("0:tc-a".to_string()),
                is_buy: false,
                price_per_tick: 42,
                ticks: 1024,
                escrow: 0,
                deadline: 0,
                flags: 0,
                timestamp: 0,
            },
            dexdo_core::OrderBookOrder {
                order_id: 8,
                owner_note: "0:seller-b".to_string(),
                token_contract: Some("0:tc-b".to_string()),
                is_buy: false,
                price_per_tick: 43,
                ticks: 2048,
                escrow: 0,
                deadline: 0,
                flags: 0,
                timestamp: 0,
            },
        ];

        let output = super::render_executable_book_output(&snapshot, &orders, 8, 50, None);
        let rows = output
            .lines()
            .filter(|line| line.starts_with("executable_ask "))
            .collect::<Vec<_>>();

        assert_eq!(rows.len(), 2, "{output}");
        assert!(rows[0].contains("token_contract=0:tc-a"), "{output}");
        assert!(rows[1].contains("token_contract=0:tc-b"), "{output}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn executable_book_output_empty_is_terminal_and_clear() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };

        let output = super::render_executable_book_output(
            &snapshot,
            &[],
            8,
            10,
            Some("raw order-book matcher would hit non-executable order "),
        );

        assert!(output.contains("none=true"), "{output}");
        assert!(output.contains("no_executable_ask=true"), "{output}");
        assert!(output.contains("non-executable order "), "{output}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn no_executable_book_line_is_terminal_and_clear() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };

        let line = super::render_no_executable_book_line(
            &snapshot,
            8,
            10,
            "no executable matching ask\nbest ask price 11 is above buyer max_price_per_tick 10",
        );

        assert!(line.contains("none=true"), "{line}");
        assert!(line.contains("no_executable_ask=true"), "{line}");
        assert!(line.contains("requested_ticks=8"), "{line}");
        assert!(line.contains("max_price_per_tick=10"), "{line}");
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("best ask price 11"), "{line}");
    }

    #[test]
    fn seller_open_probe_status_points_to_advance_not_buyer_stop() {
        let next = super::status_next_for(Some("seller"), "probe", true, true, false);

        assert_eq!(next.action, "seller_advance_probe_after_timeout");
        assert_eq!(next.command, "seller");
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

    #[derive(Default)]
    struct QuotePreflightChain {
        offers: Vec<dexdo_core::OfferListing>,
        model_preflight_error: Option<String>,
        explicit_preflight_error: Option<String>,
        explicit_submit_safe_order: Option<dexdo_core::OrderBookOrder>,
        sell_offer_terms: Option<(u64, u64)>,
        sell_offer_terms_calls: std::sync::atomic::AtomicUsize,
        submit_safe_single_ask_quote: bool,
    }

    impl QuotePreflightChain {
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
            Err(err) => err.to_string(),
        };

        assert!(err.contains("buyer model-only quote preflight"), "{err}");
        assert!(err.contains("best ask price 11"), "{err}");
        assert!(err.contains("above buyer max_price_per_tick 10"), "{err}");
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
            Err(err) => err.to_string(),
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

    /// static guard -- the seller publishes its offer and confirms THIS TC either rested in the IOB or
    /// immediately matched/funded BEFORE binding the gateway, so "gateway listening" cannot false-green as market
    /// readiness on an empty and unmatched book.
    #[test]
    fn seller_gateway_listens_only_after_offer_acceptance_guard() {
        let source = include_str!("commands.rs");
        let terms = source
            .find(&["sell_", "offer_terms(&token_contract)"].concat())
            .expect("seller reads authoritative TC terms before posting");
        let resume_probe = source
            .find(&["read_", "openable_match_now(&token_contract)"].concat())
            .expect("seller uses a non-blocking resume probe before posting");
        let post = source
            .find(&["post_offer", "_with_note(note.as_ref()"].concat())
            .expect("seller posts the offer before opening the gateway");
        let duplicate = source
            .find(&["assert_", "no_active_sell_order(&token_contract)"].concat())
            .expect("seller rejects duplicate active asks before posting");
        let withdrawn = source
            .find(&["assert_note_can_", "post_sell_offer()"].concat())
            .expect("seller checks withdrawn note state before posting");
        let accepted = source
            .find(&["assert_", "offer_rested(&token_contract)"].concat())
            .expect("seller waits for this TC's postSellOffer to be accepted by rest or match");
        let gateway = source
            .find(&["start_gateway", "_with_note(args.gateway_listen"].concat())
            .expect("seller starts the gateway");
        let real_backend = include_str!("../../../core/src/shellnet/backends.rs");
        let guard = real_backend
            .find("async fn assert_offer_rested(&self, tc: &TokenContract)")
            .expect("real seller acceptance guard present");
        let guard_body = &real_backend[guard..];

        assert!(
            terms < post,
            "seller offer terms must come from the deployed TC before posting"
        );
        assert!(
            terms < resume_probe && resume_probe < post,
            "fresh seller startup must use the non-blocking resume probe before post_offer"
        );
        assert!(
            !source[terms..post].contains("read_match(&token_contract)"),
            "fresh seller startup must not call the read_match wait-loop before post_offer"
        );
        assert!(
            duplicate < post,
            "seller must reject duplicate active asks before postSellOffer"
        );
        assert!(
            withdrawn < post,
            "seller must reject withdrawn notes before postSellOffer"
        );
        assert!(
            post < accepted,
            "seller must publish the offer before checking IOB acceptance"
        );
        assert!(
            accepted < gateway,
            "seller gateway must not listen before this TC's offer rested or immediately matched"
        );
        assert!(
            guard_body.contains("read_openable_match_once(tc)"),
            "post-offer acceptance must allow an immediate funded/openable match"
        );
        assert!(
            guard_body.contains("poll_inference_filled_tcs"),
            "post-offer acceptance must inspect this seller note's exact fill events"
        );
    }

    /// seller-side ModelRegistry validation must happen before any offer write can move into
    /// `postSellOffer`.
    #[test]
    fn seller_model_registry_preflight_precedes_offer_post() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("/// One resting ask")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];

        let registry = body
            .find("load_enabled_model_registry_policy")
            .expect("seller registry policy load present");
        let role = body[registry..]
            .find("RegistryRole::Seller")
            .map(|offset| registry + offset)
            .expect("seller registry role present");
        let enforce = body[registry..]
            .find("enforce_model_registry_policy(")
            .map(|offset| registry + offset)
            .expect("seller registry preflight present");
        let post = body
            .find(&["post_offer", "_with_note(note.as_ref()"].concat())
            .expect("seller post_offer present");

        assert!(
            registry < role && role < enforce && enforce < post,
            "seller registry validation must run before postSellOffer"
        );
    }

    /// buyer-side ModelRegistry validation must happen before either direct-deal buy or
    /// model-wide `placeInferenceBuy`.
    #[test]
    fn buyer_model_registry_preflight_precedes_buy_writes() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let end = source[start..]
            .find("pub(crate) async fn run_monitor")
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
        let direct_buy = body
            .find("buyer.place_buy(chain.as_ref(), &tc)")
            .expect("direct buy present");
        let model_buy = body
            .find("place_buy_by_model_after_pool_preflight(")
            .expect("model-only buy present");

        assert!(
            registry < role
                && role < raw_tc_guard
                && raw_tc_guard < enforce
                && enforce < direct_buy,
            "registry check must precede direct buy"
        );
        assert!(
            registry < role && role < enforce && enforce < model_buy,
            "registry check must precede model buy"
        );
    }

    /// PR347 review blocker regression: active-pool validation must stay before both direct and model-only
    /// money-moving buy submissions in lazy and one-shot buyer flows.
    #[test]
    fn buyer_pool_preflight_precedes_money_moving_buy_paths() {
        let source = include_str!("commands.rs");
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
        let lazy_preflight = lazy
            .find("preflight_buyer_pool_for_money_move(args.as_ref())?")
            .expect("lazy direct-buy pool preflight present");
        let lazy_direct = lazy
            .find("buyer.place_buy(chain.as_ref(), &tc)")
            .expect("lazy direct-buy submit present");
        let lazy_model = lazy
            .find("place_buy_by_model_after_pool_preflight(")
            .expect("lazy model-buy wrapper present");
        let lazy_match = lazy[lazy_model..]
            .find("wait_matched_token_contract")
            .map(|offset| lazy_model + offset)
            .expect("lazy match wait present");
        assert!(lazy_preflight < lazy_direct);
        assert!(lazy_model < lazy_match);

        let oneshot_start = source
            .find("async fn run_buyer_inner")
            .expect("one-shot buyer helper present");
        let oneshot_end = source[oneshot_start..]
            .find("pub(crate) async fn run_monitor")
            .map(|offset| oneshot_start + offset)
            .expect("one-shot buyer helper end marker present");
        let oneshot = &source[oneshot_start..oneshot_end];
        let oneshot_preflight = oneshot
            .find("preflight_buyer_pool_for_money_move(&args)?")
            .expect("one-shot direct-buy pool preflight present");
        let oneshot_direct = oneshot
            .find("buyer.place_buy(chain.as_ref(), &tc)")
            .expect("one-shot direct-buy submit present");
        let oneshot_model = oneshot
            .find("place_buy_by_model_after_pool_preflight(")
            .expect("one-shot model-buy wrapper present");
        let oneshot_match = oneshot[oneshot_model..]
            .find("wait_matched_token_contract")
            .map(|offset| oneshot_model + offset)
            .expect("one-shot match wait present");
        assert!(oneshot_preflight < oneshot_direct);
        assert!(oneshot_model < oneshot_match);
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
        let source = include_str!("commands.rs");
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
        let source = include_str!("commands.rs");
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
        let source = include_str!("commands.rs");
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

    /// `dexdo markets` is a discovery/listing path. With buyer registry validation enabled, a
    /// registered model whose canonical book is missing is hidden from the available list instead of rendered as
    /// buyable.
    #[test]
    fn buyer_markets_hides_missing_canonical_book() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_markets")
            .expect("run_markets present");
        let end = source[start + 1..]
            .find("\npub(crate) async fn run_market")
            .map(|offset| start + 1 + offset)
            .expect("run_markets end marker present");
        let body = &source[start..end];

        let hide_policy = body
            .find("BuyerMissingBookPolicy::HideFromAvailableList")
            .expect("markets uses hide policy");
        let hidden_action = body[hide_policy..]
            .find("RegistryBookAction::BuyerHideMissing")
            .map(|offset| hide_policy + offset)
            .expect("markets handles hidden action");
        let skip = body[hidden_action..]
            .find("continue;")
            .map(|offset| hidden_action + offset)
            .expect("markets skips hidden books");
        let print = body
            .find("println!(")
            .expect("markets prints visible books");

        assert!(
            hide_policy < hidden_action && hidden_action < skip && skip < print,
            "markets must skip inactive registry books before printing available books"
        );
    }

    /// regression: `run_seller` must not own the old bounded match wait. After the offer is posted/rested
    /// and the gateway is listening, match wait + handover provisioning are delegated to the gateway watcher.
    #[test]
    fn seller_run_path_uses_gateway_watcher_not_bounded_read_match() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("/// Render the per-model inference order book")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];

        assert!(
            body.contains("watch_and_serve_match"),
            "seller match wait must be gateway-owned"
        );
        assert!(
            body.contains("seller_watch_cursor_path"),
            "gateway watcher must persist a cursor"
        );
        assert!(
            body.contains("DEFAULT_MATCH_POLL_INTERVAL"),
            "gateway watcher must use the ~30s default poll interval"
        );
        assert!(
            !body.contains("read_match(&token_contract)"),
            "run_seller must not block on the old read_match loop"
        );
        assert!(
            !body.contains("DEAL_WAIT_SECS"),
            "run_seller must not carry the old 300s seller deadline"
        );

        let ready = body.find("seller_ready").expect("seller_ready printed");
        let watch = body.find("watch_and_serve_match").expect("watcher started");
        assert!(
            ready < watch,
            "seller posts/rests and reports readiness before entering the long-running watcher"
        );
    }

    /// the model-only buyer must validate the TC state immediately after its fill event and before
    /// waiting for the seller handover.
    #[test]
    fn model_only_buy_validates_match_state_before_handover_wait() {
        let source = include_str!("commands.rs");
        let buy = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let body = &source[buy..];
        let wait_match = body
            .find("wait_matched_token_contract")
            .expect("model-only buy waits for fill event");
        let validate = body
            .find("validate_reported_match_state")
            .expect("model-only buy validates matched TC state");
        let handover = body
            .find("resolve_endpoint(chain.as_ref(), &token_contract)")
            .expect("buyer waits for handover");
        assert!(
            wait_match < validate && validate < handover,
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
        let source = include_str!("commands.rs");
        let buy = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let body = &source[buy..];
        let model_only = body
            .find("None => {\n // Show the book")
            .expect("model-only branch present");
        let segment = &body[model_only..];
        let submit = segment
            .find("place_buy_by_model")
            .expect("model-only submit present");
        let buy_event = segment
            .find("\"buy_submitted\"")
            .expect("buy_submitted event present");
        let wait_match = segment
            .find("wait_matched_token_contract")
            .expect("match wait present");
        assert!(
            submit < buy_event && buy_event < wait_match,
            "model-only buyer must emit buy_submitted after submit returns and before match wait"
        );
    }

    #[test]
    fn policy_cleanup_rechecks_state_after_wait_before_cleanup() {
        let source = include_str!("commands.rs");
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
        let source = include_str!("commands.rs");
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
            .find("pub(crate) async fn run_monitor")
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

    #[test]
    fn policy_seller_fields_dispatch_or_fail_closed_explicitly() {
        let source = include_str!("commands.rs");
        let enforce = source
            .find("fn enforce_seller_runtime_policy")
            .expect("seller max-open policy helper present");
        let run = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let helpers = &source[enforce..run];
        assert!(
            helpers.contains("supported=1"),
            "seller max_open_deals must be enforced before offer posting"
        );
        assert!(
            helpers.contains("chain.release_dispute(token_contract)"),
            "seller dispute_against_me=release_if_clean must invoke release_dispute"
        );
        assert!(
            helpers.contains("policy_action_unsupported"),
            "seller unsupported republish/cleanup surfaces must fail closed explicitly"
        );
        assert!(
            helpers.contains("action=retire_gateway"),
            "seller buyer_no_show=retire_gateway must have an explicit runtime terminal action"
        );

        let end = source[run..]
            .find("/// Render the per-model inference order book")
            .map(|offset| run + offset)
            .expect("run_seller end marker present");
        let body = &source[run..end];
        let enforce = body
            .find("enforce_seller_runtime_policy(policy)?")
            .expect("seller policy enforcement present");
        let doctor = body
            .find("shellnet_doctor_preflight")
            .expect("real shellnet preflight present");
        let post_offer = body
            .find("dexdo::seller::post_offer_with_note")
            .expect("seller offer post present");
        assert!(enforce < doctor);
        assert!(enforce < post_offer);
        assert!(body.contains("apply_seller_dispute_policy"));
        assert!(body.contains("apply_seller_terminal_policy"));

        let advance_error = body
            .find("Ok(Err(e)) => {")
            .expect("supervised advance error branch present");
        let join_error = body[advance_error..]
            .find("Err(join)")
            .map(|offset| advance_error + offset)
            .expect("advance error branch end marker present");
        let branch = &body[advance_error..join_error];
        assert!(
            branch.contains("is_err_not_open(&e)")
                && branch.contains("classify_by_fact_advance_failure")
                && branch.contains("by_fact_advance_terminal"),
            "ERR_NOT_OPEN must be classified before the seller turns it into a process fault"
        );
        let classify = branch
            .find("classify_by_fact_advance_failure")
            .expect("ERR_NOT_OPEN classifier present");
        let policy = branch
            .find("apply_seller_dispute_policy")
            .expect("non-ERR_NOT_OPEN dispute policy fallback present");
        assert!(
            classify < policy,
            "unsafe ERR_NOT_OPEN must return a money-path fault before generic dispute policy can consume it"
        );
    }

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

    /// regression: note withdraw is an owner-signed PrivateNote write. A mismatched --note-key must
    /// hit the existing owner-key guidance before `withdrawTokens` can surface a bare ERR_INVALID_SENDER 101.
    #[test]
    fn note_withdraw_checks_owner_before_submit() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_note_withdraw")
            .expect("run_note_withdraw present");
        let end = source[start..]
            .find("#[cfg(not(feature = \"shellnet\"))]")
            .map(|offset| start + offset)
            .expect("run_note_withdraw cfg end present");
        let body = &source[start..end];
        let guard = body
            .find("assert_note_owner_matches(\"note withdraw\"")
            .expect("note withdraw owner-key guard present");
        let submit = body
            .find("withdraw_note_tokens")
            .expect("note withdraw submit present");
        assert!(
            guard < submit,
            "note withdraw must check note owner key before submitting withdrawTokens"
        );
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
        let invalid = "zzzz zzzz zzzz";
        std::fs::write(&key_path, "00").unwrap();
        std::fs::write(&seed_path, invalid).unwrap();

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
        assert!(!err.contains(invalid), "{err}");

        let missing = dir.join("missing.seed");
        let err = super::note_deploy_multisig_secret_hex(&note_deploy_args(None, Some(missing)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("read --multisig-seed-file"), "{err}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_close_reclaims_opened_deal_after_stream_timeout() {
        assert_eq!(
            super::buyer_opened_close_action(699, 100, 600),
            super::BuyerOpenedCloseAction::StreamStop
        );
        assert_eq!(
            super::buyer_opened_close_action(700, 100, 600),
            super::BuyerOpenedCloseAction::StreamReclaim
        );
        assert_eq!(
            super::buyer_opened_close_action(u64::MAX - 1, u64::MAX - 10, 600),
            super::BuyerOpenedCloseAction::StreamStop
        );
    }

    #[test]
    fn buyer_renewal_monitor_uses_planner_and_recovery_actions() {
        let source = include_str!("commands.rs");
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
            Ok(())
        }

        async fn wait_matched_token_contract(
            &self,
            _since_unix: i64,
            _timeout: std::time::Duration,
        ) -> Result<dexdo_core::TokenContract, dexdo_core::ChainError> {
            self.wait_match_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .next_match
                .clone()
                .unwrap_or_else(|| "tc-next".to_string()))
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

    fn err_not_open() -> dexdo_core::ChainError {
        dexdo_core::ChainError::Chain(
            "block manager rejected message code=TVM_ERROR; exit_code=320 \
             (airegistry::ERR_NOT_OPEN) stage=data"
                .to_string(),
        )
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

    #[tokio::test]
    async fn policy_no_handover_next_seller_cleans_then_places_next_buy() {
        use std::sync::atomic::Ordering;

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
            None,
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
        let err =
            anyhow::anyhow!("run_tvm getter getDetails exit code 52: Replay protection exception");
        assert!(super::is_replay_protection_error(&err));
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
    #[test]
    fn oracle_deadline_enforces_contract_result_gap() {
        let now = 1_900_000_000;
        assert!(super::validate_oracle_deadline(now + 119, now).is_err());
        assert!(super::validate_oracle_deadline(now + 120, now).is_ok());
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
