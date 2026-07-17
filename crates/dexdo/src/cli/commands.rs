//! `dexdo` CLI command handlers(`seller`/`buyer`/`monitor`/`provision`/`destroy`/`recover`), split out of
//! `main.rs`(PR3, move-only). Behavior-identical to the pre-split handlers.

pub(crate) use crate::cli::admin::{run_destroy, run_market_deploy, run_provision};
use crate::cli::args::*;
pub(crate) use crate::cli::close::run_close;
use crate::cli::deals;
pub(crate) use crate::cli::market_views::{
    run_executable_book, run_market, run_market_data, run_quote,
};
pub(crate) use crate::cli::markets::run_markets;
pub(crate) use crate::cli::monitor::run_monitor;
pub(crate) use crate::cli::note_cmd::{
    run_note_balance, run_note_deploy, run_note_recover, run_note_stream_locks, run_note_withdraw,
};
pub(crate) use crate::cli::oracle::run_oracle;
pub(crate) use crate::cli::orders::run_orders;
#[cfg(feature = "shellnet")]
use crate::cli::policy;
pub(crate) use crate::cli::recover::{
    run_dispute, run_reclaim, run_recover, run_release_dispute, run_withdraw_shell,
};
pub(crate) use crate::cli::reports::{
    run_dashboard, run_deals, run_export, run_history, run_status,
};
pub(crate) use crate::cli::seller::run_seller;
use crate::cli::support::*;
use anyhow::{bail, Result};
#[cfg(feature = "shellnet")]
use dexdo::registry::{
    enforce_model_registry_policy as enforce_model_registry_policy_with_reader,
    ShellnetModelRegistryReader,
};
use dexdo::registry::{
    BuyerMissingBookPolicy, RegistryBookAction, RegistryRole, RegistryValidationInput,
    RegistryValidationPolicy,
};
#[cfg(feature = "shellnet")]
use dexdo_core::shellnet::LiveBookOrder;
#[cfg(feature = "shellnet")]
use dexdo_core::OrderBookSnapshot;
use dexdo_core::{
    model_hash_for, DobParams, MockChainBackend, OfferListing, OrderBookOrder, ProtocolConsts,
};
#[cfg(feature = "shellnet")]
use serde_json::{json, Value};
use std::future::Future;
#[cfg(feature = "shellnet")]
use std::io::Write as _;

/// Deadline for awaiting match/handover: fail-closed, so `seller`/`buyer` don't hang
/// forever if the match didn't go through. Backstop, not SLA -- a real on-chain match completes in ~1-2 min.
pub(crate) const DEAL_WAIT_SECS: u64 = 300;
/// Lookback window for a model-only `--resume`: how far back to scan THIS note's own
/// `InferenceFilledConfirmed` events for the freshly matched deal (the buyer learns its deal from its own
/// note, never a hand-pasted address). Wide enough to survive a process restart / slow match, short enough
/// to skip earlier, already closed deals on the same book. The reader returns the MOST RECENT match in-window.
pub(crate) const RESUME_LOOKBACK_SECS: i64 = 1800;
pub(crate) const TRANSIENT_QUOTE_ATTEMPTS: usize = 3;
pub(crate) const TRANSIENT_QUOTE_INITIAL_BACKOFF: std::time::Duration =
    std::time::Duration::from_millis(250);
#[cfg(feature = "shellnet")]
pub(crate) const EXECUTABLE_READ_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(250),
    std::time::Duration::from_millis(500),
];
#[cfg(feature = "shellnet")]
const DEFAULT_CONTRACTS_PATH: &str = "contracts/deployed.shellnet.json";
#[cfg(feature = "shellnet")]
const POOL_LOCK_TIMEOUT_SECS: u64 = 30;

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) async fn direct_chain_read_with_timeout<T>(
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
pub(crate) struct DealTarget {
    pub(crate) handle: Option<deals::DealHandle>,
    pub(crate) token_contract: String,
    pub(crate) role: Option<deals::DealHandleRole>,
    pub(crate) note_addr: Option<String>,
    pub(crate) market: Option<dexdo_core::MarketManifest>,
}

pub(crate) struct RuntimeDealHandleInput<'a> {
    pub(crate) role: deals::DealHandleRole,
    pub(crate) deals_dir: Option<&'a std::path::Path>,
    pub(crate) token_contract: &'a str,
    pub(crate) note_addr: &'a str,
    pub(crate) frame_model: &'a str,
    pub(crate) market_path: Option<&'a std::path::Path>,
    pub(crate) contracts: &'a std::path::Path,
    pub(crate) endpoint: Option<deals::DealEndpointInfo>,
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone)]
pub(crate) struct PoolRecoveryInputs {
    pub(crate) note_addr: String,
    pub(crate) note_secret_hex: String,
    pub(crate) token_contract: String,
    pub(crate) pool_record: Option<PoolRecoveryRecord>,
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone)]
pub(crate) struct PoolRecoveryRecord {
    pub(crate) pool_path: std::path::PathBuf,
    pub(crate) note_addr: String,
    pub(crate) note_secret_hex: String,
    pub(crate) token_contract: String,
    pub(crate) role: String,
}

#[cfg(feature = "shellnet")]
pub(crate) struct PoolWriteLock {
    path: std::path::PathBuf,
    pool_path: std::path::PathBuf,
}

#[cfg(feature = "shellnet")]
impl Drop for PoolWriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn note_pubkey_id(pk: &dexdo_core::NotePubkey) -> String {
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

pub(crate) fn save_mock_runtime_deal_handle(
    input: RuntimeDealHandleInput<'_>,
) -> Result<deals::DealHandle> {
    persist_runtime_deal_handle(input, "mock")
}

#[cfg(feature = "shellnet")]
pub(crate) fn load_pool_json(path: &std::path::Path) -> Result<Value> {
    let path = crate::cli::note::resolve_private_file_path(path, "DEXDO_PN_POOL")?;
    let bytes = std::fs::read(&path)
        .map_err(|e| anyhow::anyhow!("read DEXDO_PN_POOL {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("parse DEXDO_PN_POOL {}: {e}", path.display()))
}

#[cfg(feature = "shellnet")]
pub(crate) fn acquire_pool_write_lock(pool_path: &std::path::Path) -> Result<PoolWriteLock> {
    acquire_pool_write_lock_inner(pool_path, true)
}

#[cfg(feature = "shellnet")]
pub(crate) fn try_acquire_pool_write_lock(pool_path: &std::path::Path) -> Result<PoolWriteLock> {
    acquire_pool_write_lock_inner(pool_path, false)
}

#[cfg(feature = "shellnet")]
fn acquire_pool_write_lock_inner(pool_path: &std::path::Path, wait: bool) -> Result<PoolWriteLock> {
    let pool_path = crate::cli::note::resolve_private_file_path(pool_path, "DEXDO_PN_POOL")?;
    let mut lock_name = pool_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = std::path::PathBuf::from(lock_name);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(POOL_LOCK_TIMEOUT_SECS);
    loop {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&lock_path) {
            Ok(mut lock) => {
                if let Err(e) = writeln!(lock, "{}", std::process::id()) {
                    let _ = std::fs::remove_file(&lock_path);
                    return Err(anyhow::anyhow!(
                        "write pool lock {}: {e}",
                        lock_path.display()
                    ));
                }
                return Ok(PoolWriteLock {
                    path: lock_path,
                    pool_path,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match std::fs::symlink_metadata(&lock_path) {
                    Ok(metadata) if metadata.file_type().is_file() => {}
                    Ok(_) => bail!("pool lock {} must be a regular file", lock_path.display()),
                    Err(inspect) if inspect.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(inspect) => {
                        bail!("inspect pool lock {}: {inspect}", lock_path.display())
                    }
                }
                if !wait {
                    bail!("pool lock {} is already held", lock_path.display());
                }
                if std::time::Instant::now() >= deadline {
                    bail!(
                        "timed out after {POOL_LOCK_TIMEOUT_SECS}s waiting for pool lock {}; another pool writer may still be active",
                        lock_path.display()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => bail!("create pool lock {}: {e}", lock_path.display()),
        }
    }
}

#[cfg(feature = "shellnet")]
pub(crate) fn with_pool_write_lock<T>(
    pool_path: &std::path::Path,
    update: impl FnOnce(&std::path::Path) -> Result<T>,
) -> Result<T> {
    let lock = acquire_pool_write_lock(pool_path)?;
    update(&lock.pool_path)
}

#[cfg(feature = "shellnet")]
pub(crate) fn note_pool_path(explicit: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    match std::env::var_os("DEXDO_PN_POOL") {
        Some(raw) if !raw.is_empty() => Some(std::path::PathBuf::from(raw)),
        _ => None,
    }
}

#[cfg(feature = "shellnet")]
pub(crate) fn resolve_pool_recovery_inputs(
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
            pool_record: None,
        });
    }

    let Some(pool_path) = note_pool_path(pool) else {
        bail!(
            "{command}: pass --note-addr, --note-key, and --token-contract/--market, or pass --pool / set \
             DEXDO_PN_POOL containing this note entry with token_contract recovery metadata"
        );
    };
    let pool_path = crate::cli::note::resolve_private_file_path(&pool_path, "DEXDO_PN_POOL")?;
    let pool = load_pool_json(&pool_path)?;
    let mut records = crate::cli::note::pool_note_recovery_records(&pool)?
        .into_iter()
        .filter(|(note_addr, _, tc, role)| {
            (role == "buyer" || role == "unknown")
                && explicit_note_addr
                    .as_ref()
                    .is_none_or(|want| want == note_addr)
                && explicit_tc.as_ref().is_none_or(|want| want == tc)
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
    let (pool_note_addr, pool_secret, pool_tc, pool_role) = records.remove(0);
    let note_secret_hex = match identity.note_key.as_deref() {
        Some(path) => read_secret_hex(path, "--note-key")?,
        None => pool_secret.clone(),
    };
    let pool_record = (identity.note_addr.is_none()
        && identity.note_key.is_none()
        && market.is_none()
        && token_contract.is_none())
    .then(|| PoolRecoveryRecord {
        pool_path,
        note_addr: pool_note_addr.clone(),
        note_secret_hex: pool_secret,
        token_contract: pool_tc.clone(),
        role: pool_role,
    });
    Ok(PoolRecoveryInputs {
        note_addr: explicit_note_addr.unwrap_or(pool_note_addr),
        note_secret_hex,
        token_contract: explicit_tc.unwrap_or(pool_tc),
        pool_record,
    })
}

#[cfg(feature = "shellnet")]
pub(crate) fn persist_pool_recovery_record(record: &PoolRecoveryRecord) -> Result<()> {
    with_pool_write_lock(&record.pool_path, |_| {
        persist_pool_recovery_record_locked(record)
    })
}

#[cfg(feature = "shellnet")]
fn persist_pool_recovery_record_locked(record: &PoolRecoveryRecord) -> Result<()> {
    let mut pool = load_pool_json(&record.pool_path)?;
    let notes = pool["notes"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("DEXDO_PN_POOL: malformed (\"notes\" is not an array)"))?;
    let mut matched = Vec::new();
    let mut conflicting_buyer_record = false;
    for (index, note) in notes.iter().enumerate() {
        let Some(address) = note["address"].as_str() else {
            continue;
        };
        let address = dexdo_core::normalize_wallet_address(address)
            .unwrap_or_else(|_| address.trim().to_ascii_lowercase());
        if address != record.note_addr {
            continue;
        }
        let role = note["token_contract_role"].as_str().unwrap_or("unknown");
        let secret = note["owner_secret_key_hex"].as_str();
        let tc = note["token_contract"]
            .as_str()
            .and_then(|tc| dexdo_core::normalize_wallet_address(tc).ok());
        if secret == Some(record.note_secret_hex.as_str())
            && tc.as_deref() == Some(record.token_contract.as_str())
            && role == record.role
        {
            matched.push(index);
        } else if role == "buyer" || role == "unknown" {
            conflicting_buyer_record = true;
        }
    }
    if matched.len() != 1 {
        bail!(
            "recover: DEXDO_PN_POOL {} no longer contains exactly one resolved {} recovery record for note {} and TokenContract {}; refusing to persist a wrong-key or changed record",
            record.pool_path.display(),
            record.role,
            record.note_addr,
            record.token_contract
        );
    }
    if conflicting_buyer_record {
        bail!(
            "recover: DEXDO_PN_POOL {} contains a different buyer recovery record for note {}; refusing to clobber or create an ambiguous record",
            record.pool_path.display(),
            record.note_addr
        );
    }
    let note = &mut notes[matched[0]];
    note["address"] = json!(record.note_addr);
    note["token_contract"] = json!(record.token_contract);
    note["token_contract_role"] = json!("buyer");
    note["token_contract_updated_at_unix"] = json!(unix_now_secs());
    let bytes = serde_json::to_vec_pretty(&pool)?;
    write_pool_private(&record.pool_path, &bytes)
}

#[cfg(feature = "shellnet")]
pub(crate) fn is_note_deploy_wallet_busy_error(error: &anyhow::Error) -> bool {
    let msg = error.to_string().to_ascii_lowercase();
    msg.contains("tvm_error")
        || msg.contains("replay protection")
        || msg.contains("exit code 52")
        || msg.contains("nonce")
        || msg.contains("seqno")
}

#[cfg(feature = "shellnet")]
pub(crate) fn note_deploy_error(
    funding_multisig_address: &str,
    error: anyhow::Error,
) -> anyhow::Error {
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

pub(crate) fn load_enabled_model_registry_policy(
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

#[cfg(feature = "shellnet")]
pub(crate) async fn enforce_model_registry_policy(
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
pub(crate) async fn enforce_model_registry_policy(
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
fn role_arg_to_handle(role: DealRoleArg) -> deals::DealHandleRole {
    match role {
        DealRoleArg::Buyer => deals::DealHandleRole::Buyer,
        DealRoleArg::Seller => deals::DealHandleRole::Seller,
    }
}

#[cfg(feature = "shellnet")]
pub(crate) fn load_deal_target(
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
pub(crate) fn deal_contracts_path(
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
pub(crate) async fn shellnet_doctor_preflight_market(
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
pub(crate) fn save_runtime_deal_handle(
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
pub(crate) fn save_runtime_deal_handle(
    _input: RuntimeDealHandleInput<'_>,
    _emit_human_output: bool,
) -> Result<deals::DealHandle> {
    bail!("real shellnet deal handles unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
async fn shellnet_doctor_report(
    network: &str,
    endpoint: Option<&str>,
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
) -> Result<dexdo_core::ShellnetDoctorReport> {
    let endpoint = endpoint.or((network != "shellnet").then_some(network));
    let contracts = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let market = market.map(load_market).transpose()?;
    let chain = dexdo_core::RealChainBackend::connect_with_endpoint(contracts, endpoint)?;
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
pub(crate) async fn shellnet_doctor_preflight(
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
) -> Result<()> {
    let report = shellnet_doctor_report("shellnet", None, contracts, market).await?;
    if !report.is_ok() {
        bail!("{}", render_shellnet_doctor_report(&report));
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn shellnet_doctor_preflight(
    _contracts: &std::path::Path,
    _market: Option<&std::path::Path>,
) -> Result<()> {
    bail!("shellnet doctor unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_doctor(args: DoctorArgs) -> Result<()> {
    let report = shellnet_doctor_report(
        &args.network,
        args.endpoint.as_deref(),
        &args.contracts,
        args.market.as_deref(),
    )
    .await?;
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
pub(crate) struct BookTarget {
    pub(crate) frame_model: String,
    pub(crate) model_hash: String,
    pub(crate) order_book: Option<String>,
    pub(crate) root_model: Option<String>,
    pub(crate) note_addr: Option<String>,
}

#[cfg(feature = "shellnet")]
pub(crate) fn model_target_from_config(
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
pub(crate) fn target_from_market(path: &std::path::Path) -> Result<BookTarget> {
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
pub(crate) fn target_from_market_for_model(
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
pub(crate) async fn read_book_target(
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
pub(crate) async fn read_executable_book_target(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
) -> Result<OrderBookSnapshot> {
    let mut snapshot = read_book_target(chain, target).await?;
    snapshot.orders = chain.executable_resting_asks(&snapshot).await?;
    Ok(snapshot)
}

#[cfg(feature = "shellnet")]
pub(crate) async fn resolve_order_book_target(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
) -> Result<String> {
    if let Some(order_book) = target.order_book.as_deref() {
        return dexdo_core::Address::parse(order_book)
            .map(|address| address.with_workchain())
            .map_err(|error| anyhow::anyhow!("order_book {order_book}: {error}"));
    }
    let note_addr = target
        .note_addr
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--note-addr is required when --market is not supplied"))?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|error| anyhow::anyhow!("--note-addr {note_addr}: {error}"))?;
    chain
        .inference_orderbook_address(&note, &target.model_hash, dexdo_core::MODEL_TICK_SIZE)
        .await
        .map(|address| address.with_workchain())
}

#[cfg(feature = "shellnet")]
pub(crate) fn fold_snapshot_from_orders<'a>(
    target: &BookTarget,
    order_book: &str,
    orders: impl IntoIterator<Item = &'a LiveBookOrder>,
) -> OrderBookSnapshot {
    OrderBookSnapshot {
        frame_model: target.frame_model.clone(),
        model_hash: target.model_hash.clone(),
        order_book: order_book.to_string(),
        stats: None,
        orders: orders
            .into_iter()
            .map(|order| OrderBookOrder {
                order_id: order.order_id,
                owner_note: order.note.clone(),
                token_contract: (!order.is_buy).then(|| order.token_contract.clone()),
                is_buy: order.is_buy,
                price_per_tick: order.price,
                ticks: order.ticks_remaining,
                escrow: 0,
                deadline: order.deadline,
                flags: 0,
                timestamp: 0,
            })
            .collect(),
    }
}

#[cfg(feature = "shellnet")]
pub(crate) fn snapshot_with_executable_orders(
    mut snapshot: OrderBookSnapshot,
    executable_orders: Vec<OrderBookOrder>,
) -> OrderBookSnapshot {
    snapshot.orders = executable_orders;
    snapshot
}

#[cfg(feature = "shellnet")]
fn transient_executable_read(error: &anyhow::Error) -> bool {
    if error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
            error.is_connect()
                || error.is_timeout()
                || error.is_body()
                || error
                    .status()
                    .is_some_and(|status| status.is_server_error() || status.as_u16() == 429)
        })
    }) {
        return true;
    }
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("timed out")
        || message.contains("timeout")
        || message.contains("connection")
        || message.contains("http 429")
        || (500..=599).any(|status| message.contains(&format!("http {status}")))
}

#[cfg(feature = "shellnet")]
pub(crate) async fn retry_executable_read<T, F, Fut>(label: &str, mut read: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    for (attempt, delay) in EXECUTABLE_READ_BACKOFF.iter().enumerate() {
        match read().await {
            Ok(value) => return Ok(value),
            Err(error) if transient_executable_read(&error) => {
                tracing::warn!(
                    read = label,
                    attempt = attempt + 1,
                    backoff_ms = delay.as_millis(),
                    error = %format!("{error:#}"),
                    "transient executable read failed; retrying"
                );
                tokio::time::sleep(*delay).await;
            }
            Err(error) => return Err(error),
        }
    }
    read().await
}

#[cfg(feature = "shellnet")]
pub(crate) async fn expected_order_book_for_note(
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
pub(crate) async fn order_book_active(
    chain: &dexdo_core::RealChainBackend,
    expected_order_book: &str,
) -> Result<bool> {
    let ob = dexdo_core::Address::parse(expected_order_book)
        .map_err(|e| anyhow::anyhow!("order_book {expected_order_book}: {e}"))?;
    Ok(chain.inference_orderbook_stats(&ob).await?.is_some())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn order_book_active_from_contracts(
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
pub(crate) async fn order_book_active_from_contracts(
    contracts: &std::path::Path,
    expected_order_book: &str,
) -> Result<bool> {
    let _ = (contracts, expected_order_book);
    bail!("order-book state reads require a shellnet build")
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn expected_order_book_for_note(
    contracts: &std::path::Path,
    note_addr: &str,
    frame_model: &str,
) -> Result<String> {
    let _ = (contracts, note_addr, frame_model);
    bail!("order-book derivation requires a shellnet build")
}

pub(crate) fn mock_chain_for_machine(
    endpoints_file: Option<std::path::PathBuf>,
) -> Result<MockChainBackend> {
    let endpoints_file = resolve_endpoints_file(endpoints_file)?;
    Ok(MockChainBackend::new(
        endpoints_file,
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    ))
}

pub(crate) fn mock_orders_from_offers(offers: Vec<OfferListing>) -> Vec<OrderBookOrder> {
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

pub(crate) fn role_arg_str(role: DealRoleArg) -> &'static str {
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

pub(crate) struct MockDealTarget {
    pub(crate) handle: Option<deals::DealHandle>,
    pub(crate) token_contract: String,
    pub(crate) role: Option<DealRoleArg>,
    pub(crate) note_addr: Option<String>,
    pub(crate) frame_model: Option<String>,
}

pub(crate) fn resolve_mock_deal_target(
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

#[cfg(feature = "shellnet")]
pub(crate) fn close_hint(target: &DealTarget, s: &deals::DealStateSummary) -> String {
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

pub(crate) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (review): write the `DEXDO_PN_POOL`(carries note owner secret keys) privately + atomically --
/// an exclusive 0600 temp in the destination directory, then `rename` over the target. A plain `fs::write`
/// inherits the umask, and a predictable non-exclusive temp path can clobber a pre-created file/symlink.
#[cfg(feature = "shellnet")]
pub(crate) fn write_pool_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    crate::cli::note::write_private_atomic(path, bytes)
}

#[cfg(feature = "shellnet")]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write_pool_private_via_temp(
    path: &std::path::Path,
    tmp: &std::path::Path,
    bytes: &[u8],
) -> Result<()> {
    crate::cli::note::write_private_atomic_via_temp(path, tmp, bytes)
}

#[cfg(feature = "shellnet")]
pub(crate) fn note_deploy_same_file_pool_guard(
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
pub(crate) fn note_deploy_recovery_pool_guard(
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
pub(crate) fn note_endpoint_url(endpoint: &str) -> Result<String> {
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
pub(crate) fn note_deploy_multisig_secret_hex(
    args: &NoteDeployArgs,
) -> Result<(&'static str, String)> {
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
pub(crate) fn note_deploy_now_unix() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

#[cfg(feature = "shellnet")]
pub(crate) fn note_deploy_fold_state_into_pool(
    pool_path: &std::path::Path,
    state: &crate::cli::note::OnboardPnState,
    funding_multisig_address: &str,
) -> Result<usize> {
    with_pool_write_lock(pool_path, |pool_path| {
        note_deploy_fold_state_into_pool_locked(pool_path, state, funding_multisig_address, || {})
    })
}

#[cfg(feature = "shellnet")]
pub(crate) fn note_deploy_fold_state_into_pool_locked(
    pool_path: &std::path::Path,
    state: &crate::cli::note::OnboardPnState,
    funding_multisig_address: &str,
    after_read: impl FnOnce(),
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
    after_read();
    let now = note_deploy_now_unix()?;
    let pool = pool_with_note_added(existing, state, note, now, funding_multisig_address)?;
    let pool_json = serde_json::to_string_pretty(&pool)?;
    write_pool_private(pool_path, pool_json.as_bytes())?;
    Ok(pool["notes"].as_array().map(|a| a.len()).unwrap_or(0))
}

#[cfg(feature = "shellnet")]
pub(crate) fn now_unix_secs() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}
