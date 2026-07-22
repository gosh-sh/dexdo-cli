//! Note-management command handlers(Track C8/C9/C12, move-only).

use crate::cli::args::{
    NoteBalanceArgs, NoteDeployArgs, NoteRecoverArgs, NoteStreamLocksArgs, NoteWithdrawArgs,
};
#[cfg(feature = "shellnet")]
use crate::cli::commands::{
    is_note_deploy_wallet_busy_error, note_deploy_error, note_deploy_fold_state_into_pool,
    note_deploy_multisig_secret_hex, note_deploy_now_unix, note_deploy_recovery_pool_guard,
    note_deploy_same_file_pool_guard, note_endpoint_url, now_unix_secs, shellnet_doctor_preflight,
    unix_now_secs,
};
#[cfg(feature = "shellnet")]
use crate::cli::support::read_secret_hex;
use anyhow::bail;
use anyhow::Result;
#[cfg(feature = "shellnet")]
use std::io::Write as _;

#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_recover(args: NoteRecoverArgs) -> Result<()> {
    use crate::cli::note::{
        ensure_recovery_owner_matches_target_note, load_note_deploy_recovery,
        resolve_private_file_path,
    };
    use dexdo_core::{private_note::artifacts::PRIVATE_NOTE_ABI_JSON, Address, ChainClient};

    let pool_path = resolve_private_file_path(&args.pool, "--pool")?;
    let recovery_path = resolve_private_file_path(&args.recovery, "--recovery")?;
    note_deploy_recovery_pool_guard(&pool_path, &recovery_path)?;
    let recovery = load_note_deploy_recovery(&recovery_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "note recover: recovery file {} not found",
            recovery_path.display()
        )
    })?;
    recovery.ensure_ready_for_pool()?;
    let state = recovery.to_onboard_state()?;
    let note_addr = state
        .pn_address
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("note recover: recovery state has no pn_address"))?
        .to_string();
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
    ensure_recovery_owner_matches_target_note(
        &recovery_path,
        &recovery,
        details.as_ref().and_then(|d| d["ephemeralPubkey"].as_str()),
    )?;
    let n =
        note_deploy_fold_state_into_pool(&pool_path, &state, &recovery.funding_multisig_address)?;
    std::fs::remove_file(&recovery_path).map_err(|e| {
        anyhow::anyhow!(
            "note recover: remove consumed recovery file {}: {e}",
            recovery_path.display()
        )
    })?;
    println!(
        "note recovered -> PrivateNote {note_addr}; folded into --pool {} ({} note(s)) from recovery {}. \
         No wallet spend was submitted.",
        pool_path.display(),
        n,
        recovery_path.display()
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_recover(_args: NoteRecoverArgs) -> Result<()> {
    bail!("note recover unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
/// Return the clearable-at Unix second. The contract requires strict `>` after the maximum delay.
fn note_stream_lock_deadline(last_change_unix: u64) -> u64 {
    last_change_unix.saturating_add(dexdo_core::shellnet::PRIVATE_NOTE_STREAM_LOCK_MAX_SECS)
}

#[cfg(feature = "shellnet")]
fn render_note_stream_locks(
    note: &str,
    status: &dexdo_core::shellnet::NoteStreamLockStatus,
    now_unix: u64,
) -> String {
    let total = status.stream_count.saturating_add(status.dispute_count);
    let clear_after = note_stream_lock_deadline(status.last_change_unix);
    let remaining = if total > 0 {
        clear_after.saturating_sub(now_unix)
    } else {
        0
    };
    let mut out = format!(
        "note={note}\nstream_locks={}\ndispute_locks={}\nlast_change_unix={}\n",
        status.stream_count, status.dispute_count, status.last_change_unix
    );
    if total == 0 {
        out.push_str("force_clear_after_unix=none\nremaining_secs=0\n");
    } else {
        out.push_str(&format!(
            "force_clear_after_unix={clear_after}\nremaining_secs={remaining}\n"
        ));
    }
    out.push_str(&format!("history_complete={}\n", status.history_complete));
    for entry in &status.entries {
        out.push_str(&format!(
            "lock kind={} deal={} changed_at_unix={} force_clear_after_unix={clear_after}\n",
            entry.kind.as_str(),
            entry.deal,
            entry.changed_at_unix,
        ));
        match entry.kind {
            dexdo_core::shellnet::NoteStreamLockKind::Stream => out.push_str(&format!(
                "recovery deal={} reclaim=\"dexdo reclaim --token-contract {} --note-addr {note} \
                 --note-key <PATH>\" stop_now=\"dexdo stop --token-contract {} --note-addr {note} \
                 --note-key <PATH>\"\n",
                entry.deal, entry.deal, entry.deal
            )),
            dexdo_core::shellnet::NoteStreamLockKind::Dispute => out.push_str(&format!(
                "recovery deal={} action=resolve_dispute_before_force_clear\n",
                entry.deal
            )),
        }
    }
    let unresolved = usize::try_from(total)
        .unwrap_or(usize::MAX)
        .saturating_sub(status.entries.len());
    if unresolved > 0 {
        out.push_str(&format!("unresolved_lock_deals={unresolved}\n"));
    }
    out
}

/// `dexdo note stream-locks`: list authoritative lock counters and reconstructed deal addresses.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_stream_locks(args: NoteStreamLocksArgs) -> Result<()> {
    use dexdo_core::{Address, RealChainBackend};

    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let note = Address::parse(&args.note_addr)
        .map_err(|error| anyhow::anyhow!("--note-addr {}: {error}", args.note_addr))?;
    let note_display = note.with_workchain();
    let chain = RealChainBackend::connect(manifest)?;
    let status = chain
        .note_stream_lock_status(&note)
        .await?
        .ok_or_else(|| anyhow::anyhow!("PrivateNote {note_display} is not active"))?;
    print!(
        "{}",
        render_note_stream_locks(&note_display, &status, now_unix_secs()?)
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_stream_locks(_args: NoteStreamLocksArgs) -> Result<()> {
    bail!("note stream-locks unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
const NOTE_DEPLOY_LOCK_TIMEOUT_SECS: u64 = 3600;

#[cfg(feature = "shellnet")]
const HERMEZ_SRS_NAME: &str = "hermez_kzg_srs_k19.bin";
#[cfg(feature = "shellnet")]
const HERMEZ_SRS_URL: &str = "https://binaries.gosh.sh/dexdo/hermez_kzg_bn254_19.srs";
#[cfg(feature = "shellnet")]
const HERMEZ_SRS_SHA256: &str = "9ebbbbfc3d4899435ef254c915c62f5aa94c539bde1cec52ca7d45679d2adf4a";
#[cfg(feature = "shellnet")]
const HERMEZ_SRS_MAX_BYTES: usize = 128 * 1024 * 1024;
#[cfg(feature = "shellnet")]
const HERMEZ_SRS_MARKER_NAME: &str = ".hermez_srs_sha256";
#[cfg(feature = "shellnet")]
const HERMEZ_SRS_PENDING_MARKER_NAME: &str = ".hermez_srs_sha256.pending";
#[cfg(feature = "shellnet")]
const PROVER_CACHE_ARTIFACTS: [&str; 3] =
    ["pk_cache.bin", "vk_cache.bin", "break_points_cache.bin"];

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
    let timeout = note_deploy_lock_timeout();
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
fn note_deploy_lock_timeout() -> u64 {
    std::env::var("DEXDO_NOTE_DEPLOY_LOCK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(NOTE_DEPLOY_LOCK_TIMEOUT_SECS)
}

#[cfg(feature = "shellnet")]
#[derive(Debug)]
struct NoteDeployProverCacheLock {
    file: std::fs::File,
}

#[cfg(feature = "shellnet")]
impl Drop for NoteDeployProverCacheLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(feature = "shellnet")]
fn acquire_note_deploy_prover_cache_lock(
    prover_cache_dir: &std::path::Path,
) -> Result<NoteDeployProverCacheLock> {
    acquire_note_deploy_prover_cache_lock_with_timeout(
        prover_cache_dir,
        std::time::Duration::from_secs(note_deploy_lock_timeout()),
    )
}

#[cfg(feature = "shellnet")]
fn acquire_note_deploy_prover_cache_lock_with_timeout(
    prover_cache_dir: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<NoteDeployProverCacheLock> {
    std::fs::create_dir_all(prover_cache_dir).map_err(|e| {
        anyhow::anyhow!(
            "create prover cache dir {} for lock: {e}",
            prover_cache_dir.display()
        )
    })?;
    let path = prover_cache_dir.join(".dexdo-prover.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("open prover cache lock {}: {e}", path.display()))?;
    let started = std::time::Instant::now();
    let mut announced = false;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(NoteDeployProverCacheLock { file }),
            Err(error) if note_deploy_lock_is_contended(&error) => {
                if started.elapsed() >= timeout {
                    let waited = started.elapsed().as_secs();
                    bail!(
                        "note deploy prover cache busy: waited {waited}s for {}; another note deploy is \
                         generating or using the shared prover cache. Retry after it finishes, or set \
                         DEXDO_NOTE_DEPLOY_LOCK_TIMEOUT_SECS to a larger bounded wait.",
                        path.display()
                    );
                }
                if !announced {
                    eprintln!(
                        "note deploy: prover cache busy, waited 0s; waiting for {} (timeout {}s)",
                        path.display(),
                        timeout.as_secs()
                    );
                    announced = true;
                }
                let remaining = timeout.saturating_sub(started.elapsed());
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(100)));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "try lock prover cache {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

#[cfg(feature = "shellnet")]
fn note_deploy_lock_is_contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

#[cfg(feature = "shellnet")]
fn note_deploy_multisig_keys(args: &NoteDeployArgs) -> Result<dexdo_core::KeyPair> {
    let (source, secret_hex) = note_deploy_multisig_secret_hex(args)?;
    dexdo_core::KeyPair::from_secret_hex(secret_hex.trim())
        .map_err(|e| anyhow::anyhow!("{source} (SDK secret hex): {e:?}"))
}

#[cfg(feature = "shellnet")]
trait NoteDeployFundingKeyLoader {
    fn load_funding_wallet_keys(&self) -> Result<dexdo_core::KeyPair>;
}

#[cfg(feature = "shellnet")]
impl NoteDeployFundingKeyLoader for NoteDeployArgs {
    fn load_funding_wallet_keys(&self) -> Result<dexdo_core::KeyPair> {
        note_deploy_multisig_keys(self)
    }
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, Copy, Default)]
struct NoteDeployVoucherFailpoints {
    before_voucher_event_wait: bool,
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
const NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH: &str =
    "8470e1da28a2b4c742b5f7edefdd97db81c79e726f8a8b0be78d921adaf32414";

#[cfg(feature = "shellnet")]
fn ensure_note_deploy_update_custodian_code_hash(code_hash: &str) -> Result<()> {
    let code_hash = code_hash.trim();
    let code_hash = code_hash
        .strip_prefix("0x")
        .or_else(|| code_hash.strip_prefix("0X"))
        .unwrap_or(code_hash)
        .to_ascii_lowercase();
    if code_hash == NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH {
        return Ok(());
    }
    bail!(
        "unsupported funding wallet code_hash {code_hash}; dexdo note deploy supports only \
         UpdateCustodianMultisigWallet {NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH}"
    )
}

#[cfg(feature = "shellnet")]
fn note_deploy_update_custodian_send_transaction_params(
    root_pn: &dexdo_core::Address,
    cc: serde_json::Map<String, serde_json::Value>,
    voucher_body: String,
) -> serde_json::Value {
    serde_json::json!({
        "dest": root_pn.with_workchain(),
        "value": NOTE_DEPLOY_SUBMIT_NATIVE_VALUE.to_string(),
        "cc": serde_json::Value::Object(cc),
        "bounce": true,
        "flags": 1,
        "payload": voucher_body,
    })
}

#[cfg(feature = "shellnet")]
fn normalize_multisig_pubkey(pubkey: &str) -> Option<String> {
    let pubkey = pubkey
        .trim()
        .strip_prefix("0x")
        .or_else(|| pubkey.trim().strip_prefix("0X"))
        .unwrap_or_else(|| pubkey.trim());
    if pubkey.is_empty()
        || pubkey.len() > 64
        || !pubkey.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(format!("{pubkey:0>64}").to_ascii_lowercase())
}

#[cfg(feature = "shellnet")]
fn multisig_custodian_pubkeys(custodians: &serde_json::Value) -> Vec<String> {
    custodians
        .get("custodians")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|custodian| custodian.get("owner_pubkey"))
        .filter_map(serde_json::Value::as_str)
        .filter_map(normalize_multisig_pubkey)
        .collect()
}

#[cfg(feature = "shellnet")]
fn multisig_key_is_sole_custodian(derived_pubkey: &str, custodians: &serde_json::Value) -> bool {
    let Some(custodian_entries) = custodians
        .get("custodians")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    if custodian_entries.len() != 1 {
        return false;
    }
    let pubkeys = multisig_custodian_pubkeys(custodians);
    normalize_multisig_pubkey(derived_pubkey).is_some_and(|derived| pubkeys.as_slice() == [derived])
}

#[cfg(feature = "shellnet")]
fn ensure_multisig_key_is_sole_custodian(
    funding_wallet: &str,
    derived_pubkey: &str,
    custodians: &serde_json::Value,
) -> Result<()> {
    let custodian_entries = custodians
        .get("custodians")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "funding wallet {funding_wallet} is Active, but getCustodians returned no \
                 `custodians` array (ABI/getter output mismatch)"
            )
        })?;
    if custodian_entries.len() != 1 {
        bail!(
            "funding wallet {funding_wallet} has {} custodians; direct \
             UpdateCustodianMultisigWallet.sendTransaction \
             requires exactly one pubkey custodian, and --multisig-key must match it",
            custodian_entries.len()
        );
    }

    let derived = normalize_multisig_pubkey(derived_pubkey)
        .unwrap_or_else(|| derived_pubkey.trim().to_ascii_lowercase());
    let pubkeys = multisig_custodian_pubkeys(custodians);
    let [sole_custodian] = pubkeys.as_slice() else {
        bail!(
            "funding wallet {funding_wallet} has zero pubkey custodians in getCustodians output; \
             direct UpdateCustodianMultisigWallet.sendTransaction requires exactly one pubkey custodian"
        );
    };
    if multisig_key_is_sole_custodian(derived_pubkey, custodians) {
        return Ok(());
    }
    bail!(
        "--multisig-key derives pubkey 0x{derived}, but funding wallet {funding_wallet}'s sole \
         custodian is 0x{sole_custodian}. Provide the sole custodian's key \
         (--multisig-key / --multisig-seed-file); no wallet message was submitted."
    )
}

#[cfg(feature = "shellnet")]
fn require_get_custodians_output(
    funding_wallet: &str,
    output: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    match output {
        Some(output)
            if output
                .get("custodians")
                .and_then(serde_json::Value::as_array)
                .is_some() =>
        {
            Ok(output)
        }
        _ => bail!(
            "funding wallet {funding_wallet} is Active, but getCustodians returned no custodians \
             output (ABI/getter output mismatch)"
        ),
    }
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
trait NoteDeployFundingWalletReader {
    async fn funding_wallet_code_hash(
        &self,
        multisig_address: &dexdo_core::Address,
    ) -> Result<String>;

    async fn funding_wallet_custodians(
        &self,
        multisig_address: &dexdo_core::Address,
    ) -> Result<serde_json::Value>;
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
impl NoteDeployFundingWalletReader for dexdo_core::ChainClient {
    async fn funding_wallet_code_hash(
        &self,
        multisig_address: &dexdo_core::Address,
    ) -> Result<String> {
        let funding_multisig_address = multisig_address.with_workchain();
        let funding_wallet = self
            .get_account(multisig_address)
            .await
            .map_err(|e| anyhow::anyhow!("read funding wallet {funding_multisig_address}: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!("funding wallet {funding_multisig_address} not found")
            })?;
        if !funding_wallet.is_active() {
            bail!(
                "funding wallet {funding_multisig_address} is not Active (acc_type={})",
                funding_wallet.status
            );
        }
        let wallet_code_hash = funding_wallet.code_hash.as_deref().ok_or_else(|| {
            anyhow::anyhow!("funding wallet {funding_multisig_address} has no code_hash")
        })?;
        Ok(wallet_code_hash.to_string())
    }

    async fn funding_wallet_custodians(
        &self,
        multisig_address: &dexdo_core::Address,
    ) -> Result<serde_json::Value> {
        let funding_multisig_address = multisig_address.with_workchain();
        let output = self
            .run_getter(
                multisig_address,
                dexdo_core::ackinacki_wallet::contracts::MULTISIG_ABI_JSON,
                "getCustodians",
                serde_json::json!({}),
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("read custodians of funding wallet {funding_multisig_address}: {e}")
            })?;
        require_get_custodians_output(&funding_multisig_address, output)
    }
}

#[cfg(feature = "shellnet")]
async fn note_deploy_preflight_key_owns_wallet(
    wallet_reader: &dyn NoteDeployFundingWalletReader,
    multisig_address: &dexdo_core::Address,
    multisig_keys: &dexdo_core::KeyPair,
) -> Result<()> {
    let funding_multisig_address = multisig_address.with_workchain();
    let code_hash = wallet_reader
        .funding_wallet_code_hash(multisig_address)
        .await?;
    ensure_note_deploy_update_custodian_code_hash(&code_hash)?;
    let custodians = wallet_reader
        .funding_wallet_custodians(multisig_address)
        .await?;
    ensure_multisig_key_is_sole_custodian(
        &funding_multisig_address,
        multisig_keys.public_hex(),
        &custodians,
    )
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
async fn note_deploy_build_voucher_submit_boc(
    multisig_address: &dexdo_core::Address,
    multisig_keys: &dexdo_core::KeyPair,
    root_pn: &dexdo_core::Address,
    checkpoint: &crate::cli::note::NoteDeployVoucherCheckpoint,
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
    let boc = encode_external_call(
        &ctx,
        dexdo_core::ackinacki_wallet::contracts::MULTISIG_ABI_JSON,
        &multisig_address.with_workchain(),
        "sendTransaction",
        note_deploy_update_custodian_send_transaction_params(root_pn, cc, voucher_body),
        multisig_keys.public_hex(),
        multisig_keys.secret_hex(),
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(
            "encode UpdateCustodianMultisigWallet.sendTransaction -> RootPN.generateVoucher: {e}"
        )
    })?;
    Ok(boc)
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
trait NoteDeployVoucherBocBuilder {
    async fn build_voucher_submit_boc(
        &self,
        multisig_address: &dexdo_core::Address,
        multisig_keys: &dexdo_core::KeyPair,
        root_pn: &dexdo_core::Address,
        checkpoint: &crate::cli::note::NoteDeployVoucherCheckpoint,
    ) -> Result<String>;
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
impl NoteDeployVoucherBocBuilder for dexdo_core::ChainClient {
    async fn build_voucher_submit_boc(
        &self,
        multisig_address: &dexdo_core::Address,
        multisig_keys: &dexdo_core::KeyPair,
        root_pn: &dexdo_core::Address,
        checkpoint: &crate::cli::note::NoteDeployVoucherCheckpoint,
    ) -> Result<String> {
        note_deploy_build_voucher_submit_boc(multisig_address, multisig_keys, root_pn, checkpoint)
            .await
    }
}

#[cfg(feature = "shellnet")]
async fn note_deploy_submit_voucher_boc(
    endpoint: &str,
    multisig_address: &dexdo_core::Address,
    boc: &str,
    http: &reqwest::Client,
) -> Result<()> {
    use dexdo_core::ackinacki_wallet::query::send_message_routed;
    dexdo_core::shellnet_clock_skew_preflight(endpoint).await?;
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
        anyhow::anyhow!(
            "submit UpdateCustodianMultisigWallet.sendTransaction -> RootPN.generateVoucher: {e}"
        )
    })?;
    Ok(())
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
trait NoteDeployVoucherSubmitter {
    async fn submit_voucher_boc(
        &self,
        endpoint: &str,
        multisig_address: &dexdo_core::Address,
        boc: &str,
        http: &reqwest::Client,
    ) -> Result<()>;
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
impl NoteDeployVoucherSubmitter for dexdo_core::ChainClient {
    async fn submit_voucher_boc(
        &self,
        endpoint: &str,
        multisig_address: &dexdo_core::Address,
        boc: &str,
        http: &reqwest::Client,
    ) -> Result<()> {
        note_deploy_submit_voucher_boc(endpoint, multisig_address, boc, http).await
    }
}

#[cfg(feature = "shellnet")]
fn is_note_deploy_wallet_submit_busy_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("submit UpdateCustodianMultisigWallet.sendTransaction -> RootPN.generateVoucher:")
        && is_note_deploy_wallet_busy_error(error)
}

#[cfg(feature = "shellnet")]
fn note_deploy_resume_error(funding_multisig_address: &str, error: anyhow::Error) -> anyhow::Error {
    if is_note_deploy_wallet_submit_busy_error(&error) {
        note_deploy_error(funding_multisig_address, error)
    } else {
        anyhow::anyhow!("deploy PrivateNote from wallet {funding_multisig_address}: {error}")
    }
}

#[cfg(feature = "shellnet")]
async fn run_note_deploy_with_wallet_busy_retry<T, Op, Sleep>(
    funding_multisig_address: &str,
    mut op: Op,
    mut sleeper: Sleep,
) -> Result<T>
where
    Op: AsyncFnMut(u64) -> Result<T>,
    Sleep: AsyncFnMut(std::time::Duration),
{
    let mut attempt = 1u64;
    loop {
        match op(attempt).await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if is_note_deploy_wallet_submit_busy_error(&error) && attempt < 3 {
                    let backoff_secs = attempt.saturating_mul(10);
                    eprintln!(
                        "note deploy: funding wallet {funding_multisig_address} looks busy/out-of-sync; retrying \
                         attempt {} after {backoff_secs}s",
                        attempt + 1
                    );
                    sleeper(std::time::Duration::from_secs(backoff_secs)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
                return Err(note_deploy_resume_error(funding_multisig_address, error));
            }
        }
    }
}

#[cfg(feature = "shellnet")]
#[allow(clippy::too_many_arguments)]
async fn note_deploy_mint_voucher_recoverable(
    client: &dexdo_core::ChainClient,
    recovery_path: &std::path::Path,
    recovery: &mut crate::cli::note::NoteDeployRecoveryState,
    kind: crate::cli::note::NoteDeployVoucherKind,
    multisig_address: &dexdo_core::Address,
    funding_key_loader: &dyn NoteDeployFundingKeyLoader,
    wallet_reader: &dyn NoteDeployFundingWalletReader,
    voucher_boc_builder: &dyn NoteDeployVoucherBocBuilder,
    voucher_submitter: &dyn NoteDeployVoucherSubmitter,
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
    let mut guarded_funding_keys = None;
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
            let funding_keys = funding_key_loader.load_funding_wallet_keys()?;
            note_deploy_preflight_key_owns_wallet(wallet_reader, multisig_address, &funding_keys)
                .await?;
            guarded_funding_keys = Some(funding_keys);

            let recovery_was_persisted = recovery_path.exists();
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
            if !recovery_was_persisted {
                eprintln!(
                    "{}",
                    crate::cli::note::recovery_owner_key_written_message(recovery_path)
                );
            }
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
            if guarded_funding_keys.is_none() {
                let funding_keys = funding_key_loader.load_funding_wallet_keys()?;
                note_deploy_preflight_key_owns_wallet(
                    wallet_reader,
                    multisig_address,
                    &funding_keys,
                )
                .await?;
                guarded_funding_keys = Some(funding_keys);
            }
            let funding_keys = guarded_funding_keys.as_ref().ok_or_else(|| {
                anyhow::anyhow!("fresh voucher submit is missing its guarded funding key")
            })?;
            let boc = voucher_boc_builder
                .build_voucher_submit_boc(multisig_address, funding_keys, &root_pn, &checkpoint)
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
            voucher_submitter
                .submit_voucher_boc(endpoint, multisig_address, &boc, &http)
                .await?;
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

        if failpoints.before_voucher_event_wait {
            bail!("simulated interruption before voucher event wait");
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
    let proof = {
        // The pinned prover publishes PK/VK/BP non-atomically. Serialize only the cache preflight,
        // proof/keygen, and marker publication; wallet submissions and chain waits stay outside this lock.
        let _prover_cache_lock =
            acquire_note_deploy_prover_cache_lock(&halo2_paths.prover_cache_dir)?;
        halo2_paths.ensure_srs();
        ensure_hermez_srs_and_valid_pk_cache(&halo2_paths.prover_cache_dir).await?;
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
        // A successful proof is the cache commit point. Later chain retries and pool finalization must
        // never depend on cache metadata or on PK/VK/BP still being present.
        promote_hermez_srs_pending_marker(&halo2_paths.prover_cache_dir, HERMEZ_SRS_SHA256)?;
        proof
    };
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

    dexdo_core::shellnet_clock_skew_preflight(client.endpoint()).await?;
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
#[allow(clippy::too_many_arguments)]
async fn deploy_private_note_from_multisig_recoverable(
    client: &dexdo_core::ChainClient,
    recovery_path: &std::path::Path,
    recovery: &mut crate::cli::note::NoteDeployRecoveryState,
    multisig_address: &dexdo_core::Address,
    funding_key_loader: &dyn NoteDeployFundingKeyLoader,
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
                funding_key_loader,
                client,
                client,
                client,
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
                funding_key_loader,
                client,
                client,
                client,
                pn_keys.public_hex(),
                CURRENCY_ID_SHELL,
                ECC_SHELL_DEPOSIT_RAW,
                true,
                halo2_paths,
                failpoints,
            )
            .await
            .map_err(|e| anyhow::anyhow!("halo2 SHELL gas voucher: {e}"))?;

            dexdo_core::shellnet_clock_skew_preflight(client.endpoint()).await?;
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
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(feature = "shellnet")]
fn invalidate_stale_pk_cache(prover_cache_dir: &std::path::Path) -> Result<()> {
    invalidate_stale_pk_cache_with(prover_cache_dir, |path| std::fs::remove_file(path))
}

#[cfg(feature = "shellnet")]
fn invalidate_stale_pk_cache_with<F>(
    prover_cache_dir: &std::path::Path,
    mut remove_file: F,
) -> Result<()>
where
    F: FnMut(&std::path::Path) -> std::io::Result<()>,
{
    for name in PROVER_CACHE_ARTIFACTS {
        let path = prover_cache_dir.join(name);
        match remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "remove stale prover artifact {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn atomic_replace(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(source, destination)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };

        let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination_wide: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: both buffers are NUL-terminated and remain alive for the duration of the Win32 call.
        let replaced = unsafe {
            MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(feature = "shellnet")]
fn write_file_atomically(path: &std::path::Path, bytes: &[u8], label: &str) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let temp_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("{label} destination has no printable file name"))?;
    let temp_name = format!(".{file_name}.tmp-{}-{temp_id}", std::process::id());
    let temp_path = path.with_file_name(temp_name);
    let install = || -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| anyhow::anyhow!("create {label} temp {}: {e}", temp_path.display()))?;
        file.write_all(bytes)
            .map_err(|e| anyhow::anyhow!("write {label} temp {}: {e}", temp_path.display()))?;
        file.sync_all()
            .map_err(|e| anyhow::anyhow!("sync {label} temp {}: {e}", temp_path.display()))?;
        atomic_replace(&temp_path, path).map_err(|e| {
            anyhow::anyhow!(
                "publish {label} {} from {}: {e}",
                path.display(),
                temp_path.display()
            )
        })
    };
    let result = install();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(feature = "shellnet")]
fn install_hermez_srs_atomically(srs_path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    write_file_atomically(srs_path, bytes, "Hermez SRS")
}

#[cfg(feature = "shellnet")]
fn remove_file_if_exists(path: &std::path::Path, label: &str) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "remove {label} {}: {error}",
            path.display()
        )),
    }
}

#[cfg(feature = "shellnet")]
fn marker_matches(path: &std::path::Path, expected_sha256: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|value| value.trim() == expected_sha256)
        .unwrap_or(false)
}

#[cfg(feature = "shellnet")]
fn prover_cache_artifacts_complete(prover_cache_dir: &std::path::Path) -> bool {
    PROVER_CACHE_ARTIFACTS.iter().all(|name| {
        std::fs::metadata(prover_cache_dir.join(name))
            .map(|metadata| metadata.is_file() && metadata.len() > 0)
            .unwrap_or(false)
    })
}

#[cfg(feature = "shellnet")]
fn promote_hermez_srs_pending_marker(
    prover_cache_dir: &std::path::Path,
    expected_sha256: &str,
) -> Result<()> {
    let pending = prover_cache_dir.join(HERMEZ_SRS_PENDING_MARKER_NAME);
    if !pending.exists() {
        return Ok(());
    }
    if !marker_matches(&pending, expected_sha256) {
        bail!(
            "refuse to publish prover cache marker: pending marker {} does not match pinned Hermez SRS",
            pending.display()
        );
    }
    let srs_path = prover_cache_dir.join(HERMEZ_SRS_NAME);
    let srs_matches = std::fs::read(&srs_path)
        .map(|bytes| sha256_hex(&bytes) == expected_sha256)
        .unwrap_or(false);
    if !srs_matches {
        bail!(
            "refuse to publish prover cache marker: Hermez SRS {} is missing or corrupt",
            srs_path.display()
        );
    }
    if !prover_cache_artifacts_complete(prover_cache_dir) {
        bail!(
            "refuse to publish prover cache marker: PK/VK/break-points cache is incomplete in {}",
            prover_cache_dir.display()
        );
    }
    let marker = prover_cache_dir.join(HERMEZ_SRS_MARKER_NAME);
    atomic_replace(&pending, &marker).map_err(|error| {
        anyhow::anyhow!(
            "promote pending SRS marker {} to {}: {error}",
            pending.display(),
            marker.display()
        )
    })
}

#[cfg(feature = "shellnet")]
fn transient_reqwest_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

#[cfg(feature = "shellnet")]
async fn fetch_hermez_srs_once(
    client: &reqwest::Client,
    url: &str,
) -> std::result::Result<Vec<u8>, (bool, anyhow::Error)> {
    use futures::StreamExt as _;

    let response = client.get(url).send().await.map_err(|error| {
        (
            transient_reqwest_error(&error),
            anyhow::anyhow!("download Hermez SRS: {error}"),
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        let transient =
            status.is_server_error() || status.as_u16() == 408 || status.as_u16() == 429;
        return Err((
            transient,
            anyhow::anyhow!("download Hermez SRS: HTTP {status}"),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > HERMEZ_SRS_MAX_BYTES as u64)
    {
        return Err((
            false,
            anyhow::anyhow!(
                "download Hermez SRS: Content-Length exceeds {} bytes",
                HERMEZ_SRS_MAX_BYTES
            ),
        ));
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            (
                transient_reqwest_error(&error),
                anyhow::anyhow!("download Hermez SRS body: {error}"),
            )
        })?;
        let next_len = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            (
                false,
                anyhow::anyhow!("download Hermez SRS: body length overflow"),
            )
        })?;
        if next_len > HERMEZ_SRS_MAX_BYTES {
            return Err((
                false,
                anyhow::anyhow!(
                    "download Hermez SRS: body exceeds {} bytes",
                    HERMEZ_SRS_MAX_BYTES
                ),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(feature = "shellnet")]
async fn fetch_hermez_srs_with_retry(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    for attempt in 1..=2 {
        match fetch_hermez_srs_once(client, url).await {
            Ok(bytes) => return Ok(bytes),
            Err((true, error)) if attempt == 1 => {
                eprintln!(
                    "note deploy: transient Hermez SRS download error; retrying once: {error}"
                );
            }
            Err((_, error)) => return Err(error),
        }
    }
    unreachable!("two-attempt Hermez SRS download loop must return")
}

/// Mitigates for the `dexdo note deploy` path. Its deposit and SHELL voucher proof steps use the Hermez KZG
/// prover(`generate_proof` -> `Prover::new_with_srs_from_url`), whose cache miss performs blocking HTTP from
/// async proving and whose PK cache is not keyed to the SRS. The canonical SDK/prover async-and-SRS fix for
/// non-CLI callers is tracked separately.
#[cfg(feature = "shellnet")]
pub(crate) async fn ensure_hermez_srs_and_valid_pk_cache(
    prover_cache_dir: &std::path::Path,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| anyhow::anyhow!("build Hermez SRS HTTP client: {e}"))?;
    ensure_hermez_srs_and_valid_pk_cache_with(prover_cache_dir, |url| {
        let url = url.to_owned();
        async move { fetch_hermez_srs_with_retry(&client, &url).await }
    })
    .await
}

#[cfg(feature = "shellnet")]
async fn ensure_hermez_srs_and_valid_pk_cache_with<F, Fut>(
    prover_cache_dir: &std::path::Path,
    fetch: F,
) -> Result<()>
where
    F: FnOnce(&str) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>>>,
{
    ensure_hermez_srs_and_valid_pk_cache_with_options(
        prover_cache_dir,
        HERMEZ_SRS_SHA256,
        fetch,
        invalidate_stale_pk_cache,
    )
    .await
}

#[cfg(feature = "shellnet")]
async fn ensure_hermez_srs_and_valid_pk_cache_with_options<F, Fut, I>(
    prover_cache_dir: &std::path::Path,
    expected_sha256: &str,
    fetch: F,
    invalidate: I,
) -> Result<()>
where
    F: FnOnce(&str) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>>>,
    I: FnOnce(&std::path::Path) -> Result<()>,
{
    std::fs::create_dir_all(prover_cache_dir).map_err(|e| {
        anyhow::anyhow!(
            "create prover cache dir {}: {e}",
            prover_cache_dir.display()
        )
    })?;
    let srs_path = prover_cache_dir.join(HERMEZ_SRS_NAME);
    let have_valid_srs = std::fs::read(&srs_path)
        .map(|bytes| sha256_hex(&bytes) == expected_sha256)
        .unwrap_or(false);
    if !have_valid_srs {
        eprintln!(
            "note deploy: fetching Hermez KZG SRS once -> {}",
            srs_path.display()
        );
        let bytes = fetch(HERMEZ_SRS_URL).await?;
        let got = sha256_hex(&bytes);
        if got != expected_sha256 {
            anyhow::bail!("Hermez SRS sha256 mismatch: got {got}, expected {expected_sha256}");
        }
        install_hermez_srs_atomically(&srs_path, &bytes)?;
    }

    // The final marker certifies a successful proof, not merely successful invalidation. A pending marker makes
    // interrupted non-atomic SDK keygen output fail closed on the next startup.
    let marker = prover_cache_dir.join(HERMEZ_SRS_MARKER_NAME);
    let pending = prover_cache_dir.join(HERMEZ_SRS_PENDING_MARKER_NAME);
    let cache_is_committed = marker_matches(&marker, expected_sha256)
        && !pending.exists()
        && prover_cache_artifacts_complete(prover_cache_dir);
    if !cache_is_committed {
        // Publish pending first: a crash at any later point causes the next pre-flight to invalidate again.
        write_file_atomically(&pending, expected_sha256.as_bytes(), "pending SRS marker")?;
        remove_file_if_exists(&marker, "committed SRS marker")?;
        invalidate(prover_cache_dir)?;
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn note_deploy_recovery_needs_new_proof(
    recovery: &crate::cli::note::NoteDeployRecoveryState,
) -> bool {
    use crate::cli::note::NoteDeployVoucherKind;

    if recovery.shell_funded && recovery.sanity_checked {
        return false;
    }
    let proof_is_persisted = |kind| {
        recovery
            .voucher_checkpoint(kind)
            .and_then(|checkpoint| checkpoint.proof.as_ref())
            .is_some()
    };
    let deposit_proof_needed =
        recovery.pn_address.is_none() && !proof_is_persisted(NoteDeployVoucherKind::Deposit);
    let shell_proof_needed =
        !recovery.shell_funded && !proof_is_persisted(NoteDeployVoucherKind::ShellGas);
    deposit_proof_needed || shell_proof_needed
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
trait NoteDeployResolvedOps {
    async fn load_recovery(&mut self) -> Result<crate::cli::note::NoteDeployRecoveryState>;

    async fn preflight_prover(&mut self) -> Result<()>;

    async fn resume_chain(
        &mut self,
        recovery: &mut crate::cli::note::NoteDeployRecoveryState,
    ) -> Result<crate::cli::note::OnboardPnState>;

    async fn finalize_pool(
        &mut self,
        recovery: &crate::cli::note::NoteDeployRecoveryState,
        state: &crate::cli::note::OnboardPnState,
    ) -> Result<()>;
}

#[cfg(feature = "shellnet")]
async fn run_note_deploy_resolved<O>(ops: &mut O) -> Result<()>
where
    O: NoteDeployResolvedOps,
{
    // Loading and validating recovery is the first orchestration action. Cache/SRS work is allowed only if the
    // persisted state proves that this run can reach a new proof. Completed and persisted-proof recoveries must
    // remain able to finish chain recovery and pool finalization with a missing or contended cache.
    let mut recovery = ops.load_recovery().await?;
    recovery.validate()?;
    if note_deploy_recovery_needs_new_proof(&recovery) {
        ops.preflight_prover().await?;
    }
    let state = ops.resume_chain(&mut recovery).await?;
    ops.finalize_pool(&recovery, &state).await
}

#[cfg(feature = "shellnet")]
struct NoteDeployProductionOps<'a> {
    args: &'a NoteDeployArgs,
    client: &'a dexdo_core::ChainClient,
    recovery_path: &'a std::path::Path,
    pool_path: &'a std::path::Path,
    funding_multisig_address: &'a str,
    recovery_request: crate::cli::note::NoteDeployRecoveryRequest<'a>,
    pn_keys: Option<dexdo_core::KeyPair>,
    halo2_paths: &'a dexdo_core::private_note::Halo2Paths,
    voucher_failpoints: NoteDeployVoucherFailpoints,
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait(?Send)]
impl NoteDeployResolvedOps for NoteDeployProductionOps<'_> {
    async fn load_recovery(&mut self) -> Result<crate::cli::note::NoteDeployRecoveryState> {
        use crate::cli::note::{
            load_note_deploy_recovery, recovery_owner_key_written_message, NoteDeployRecoveryState,
        };

        let (recovery, already_persisted) = match load_note_deploy_recovery(self.recovery_path)? {
            Some(state) => {
                state.ensure_matches_request(self.recovery_request)?;
                eprintln!(
                    "note deploy recovery: using existing state file {}.",
                    self.recovery_path.display()
                );
                (state, true)
            }
            None => {
                let pn_keys = dexdo_core::KeyPair::generate();
                let state = NoteDeployRecoveryState::new(
                    self.recovery_request,
                    pn_keys.public_hex(),
                    pn_keys.secret_hex(),
                )?;
                // Keep a brand-new recovery in memory until the funding wallet passes the exact
                // UpdateCustodian/sole-custodian guard. The first voucher checkpoint persists the owner
                // key and checkpoint together before any signed BOC or wallet submit.
                (state, false)
            }
        };
        if already_persisted {
            eprintln!("{}", recovery_owner_key_written_message(self.recovery_path));
        }
        self.pn_keys = Some(
            dexdo_core::KeyPair::from_secret_hex(&recovery.owner_secret_key_hex)
                .map_err(|e| anyhow::anyhow!("note deploy recovery owner key: {e:?}"))?,
        );
        Ok(recovery)
    }

    async fn preflight_prover(&mut self) -> Result<()> {
        // This early check is allowed only after recovery routing says a new proof is still needed. It prevents
        // a fresh wallet spend from starting when proving cannot run, while funded/persisted-proof recovery never
        // waits for or mutates unrelated cache state.
        let _prover_cache_lock =
            acquire_note_deploy_prover_cache_lock(&self.halo2_paths.prover_cache_dir)?;
        self.halo2_paths.ensure_srs();
        ensure_hermez_srs_and_valid_pk_cache(&self.halo2_paths.prover_cache_dir).await
    }

    async fn resume_chain(
        &mut self,
        recovery: &mut crate::cli::note::NoteDeployRecoveryState,
    ) -> Result<crate::cli::note::OnboardPnState> {
        let pn_keys = self.pn_keys.as_ref().ok_or_else(|| {
            anyhow::anyhow!("note deploy recovery was not loaded before chain resume")
        })?;
        run_note_deploy_with_wallet_busy_retry(
            self.funding_multisig_address,
            async |_attempt| {
                let multisig_address = dexdo_core::Address::parse(self.funding_multisig_address)
                    .map_err(|e| anyhow::anyhow!("--multisig-address: {e}"))?;
                deploy_private_note_from_multisig_recoverable(
                    self.client,
                    self.recovery_path,
                    recovery,
                    &multisig_address,
                    self.args,
                    pn_keys,
                    self.halo2_paths,
                    self.voucher_failpoints,
                )
                .await
            },
            async |duration| tokio::time::sleep(duration).await,
        )
        .await
    }

    async fn finalize_pool(
        &mut self,
        recovery: &crate::cli::note::NoteDeployRecoveryState,
        state: &crate::cli::note::OnboardPnState,
    ) -> Result<()> {
        use crate::cli::note::{
            derive_owner_pubkey_from_secret_hex, ensure_onchain_owner_matches_pool_key,
            refresh_note_deploy_recovery_after_success,
        };
        use dexdo_core::{private_note::artifacts::PRIVATE_NOTE_ABI_JSON, Address};

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
        let details = self
            .client
            .run_getter(
                &note_address,
                PRIVATE_NOTE_ABI_JSON,
                "getDetails",
                serde_json::json!({}),
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("verify deployed PrivateNote {note_addr} owner key: {e}")
            })?;
        ensure_onchain_owner_matches_pool_key(
            "note deploy",
            &note_addr,
            details.as_ref().and_then(|d| d["ephemeralPubkey"].as_str()),
            &derived_owner,
        )?;
        if self.args.simulate_interrupt_after_spend_before_pool {
            bail!(
                "simulated interruption after on-chain spend before final pool write. Recovery state is complete at {}; \
                 run `dexdo note recover --recovery {} --pool {}` to finalize without re-spending.",
                self.recovery_path.display(),
                self.recovery_path.display(),
                self.pool_path.display()
            );
        }

        let n =
            note_deploy_fold_state_into_pool(self.pool_path, state, self.funding_multisig_address)?;
        refresh_note_deploy_recovery_after_success(self.recovery_path, recovery).map_err(|e| {
            anyhow::anyhow!(
                "deployed PrivateNote {note_addr} is preserved in --pool {}, but the recovery file refresh was \
                 refused: {e}",
                self.pool_path.display()
            )
        })?;
        println!(
            "note deployed -> PrivateNote {note_addr} ({} {}); folded into --pool {} ({} note(s)). Recovery state is \
             at {}. The owner secret is stored in the pool for the seller/buyer -- keep both files private.",
            state.nominal,
            state.token_type,
            self.pool_path.display(),
            n,
            self.recovery_path.display()
        );
        Ok(())
    }
}

/// `dexdo note deploy` -- deploy a wallet-funded `PrivateNote` on shellnet in-process through
/// `gosh.ackinacki`, then fold its result into a `DEXDO_PN_POOL` the `seller`/`buyer` consume. The wallet funding
/// secret is read from `--multisig-key` or derived from `--multisig-seed-file`, then passed directly to the SDK.
/// The seed phrase is never printed/logged/stored. The owner secret lands in the pool file(the consumers need it)
/// but is NEVER printed/logged.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_deploy(args: NoteDeployArgs) -> Result<()> {
    use crate::cli::note::{
        default_note_deploy_recovery_path, resolve_private_file_path, NoteDeployRecoveryRequest,
    };
    use dexdo_core::{
        private_note::{proof::ECC_SHELL_DEPOSIT_RAW, Halo2Paths, Nominal, TokenType},
        ChainClient,
    };

    let pool_path = resolve_private_file_path(&args.pool, "--pool")?;
    note_deploy_same_file_pool_guard(std::env::var_os("DEXDO_PN_POOL").as_deref(), &pool_path)?;
    let funding_multisig_address = dexdo_core::normalize_wallet_address(&args.multisig_address)
        .map_err(|e| anyhow::anyhow!("--multisig-address: {e}"))?;
    let nominal = Nominal::parse(&args.nominal)?;
    let token_type = TokenType::parse(&args.token_type)?;
    let nominal_label = nominal.label().to_string();
    let token_type_label = token_type.label().to_string();
    let endpoint = note_endpoint_url(&args.endpoint)?;
    dexdo_core::shellnet_clock_skew_preflight(&endpoint).await?;
    let client = ChainClient::connect(&endpoint)?;
    let _wallet_lock = acquire_note_deploy_wallet_lock(&funding_multisig_address)?;
    let recovery_path = args
        .recovery
        .clone()
        .unwrap_or_else(|| default_note_deploy_recovery_path(&pool_path));
    let recovery_path = resolve_private_file_path(&recovery_path, "--recovery")?;
    note_deploy_recovery_pool_guard(&pool_path, &recovery_path)?;
    let recovery_request = NoteDeployRecoveryRequest {
        endpoint: &endpoint,
        nominal: &nominal_label,
        token_type: token_type.id(),
        raw_value: nominal.raw_value(token_type),
        ecc_shell_deposit: ECC_SHELL_DEPOSIT_RAW,
        funding_multisig_address: &funding_multisig_address,
    };
    let halo2_paths = Halo2Paths::from_env();

    eprintln!(
        "note deploy: in-process gosh.ackinacki -- wallet {} funds a {} {} PrivateNote on {} ...",
        funding_multisig_address, nominal_label, token_type_label, endpoint
    );
    let voucher_failpoints = NoteDeployVoucherFailpoints {
        before_voucher_event_wait: false,
        after_deposit_submit: args.simulate_interrupt_after_deposit_voucher_submit,
        after_deposit_event: args.simulate_interrupt_after_deposit_voucher_event,
        after_shell_submit: args.simulate_interrupt_after_shell_voucher_submit,
        after_deploy_before_note_record: args.simulate_interrupt_after_deploy_before_note_record,
    };
    let mut ops = NoteDeployProductionOps {
        args: &args,
        client: &client,
        recovery_path: &recovery_path,
        pool_path: &pool_path,
        funding_multisig_address: &funding_multisig_address,
        recovery_request,
        pn_keys: None,
        halo2_paths: &halo2_paths,
        voucher_failpoints,
    };
    run_note_deploy_resolved(&mut ops).await
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_deploy(_args: NoteDeployArgs) -> Result<()> {
    bail!("note deploy unavailable: build with `--features shellnet`")
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
    let chain = RealChainBackend::connect_with_endpoint(manifest, args.endpoint.as_deref())?;
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn unsafe_clock_produces_zero_posts_in_note_deploy_direct_send() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for chain_offset in [60_i64, -300] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let posts = Arc::new(AtomicUsize::new(0));
            let server_posts = Arc::clone(&posts);
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0_u8; 8192];
                    let read = socket.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..read]);
                    if request.starts_with("POST /v2/messages ") {
                        server_posts.fetch_add(1, Ordering::SeqCst);
                    }
                    let local = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64;
                    let chain = (local + chain_offset) as u64;
                    let body = serde_json::json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":chain}}]}}}}).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let wallet = dexdo_core::Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
            let error = super::note_deploy_submit_voucher_boc(
                &endpoint,
                &wallet,
                "not-posted",
                &reqwest::Client::new(),
            )
            .await
            .unwrap_err();
            assert!(format!("{error:#}").contains("CLOCK_SKEW"));
            assert_eq!(
                posts.load(Ordering::SeqCst),
                0,
                "no message POST is permitted"
            );
            task.abort();
        }
    }

    #[cfg(feature = "shellnet")]
    struct FixedFundingKeyLoader {
        secret_hex: Option<String>,
        failure: Option<&'static str>,
        calls: std::cell::Cell<usize>,
    }

    #[cfg(feature = "shellnet")]
    impl FixedFundingKeyLoader {
        fn returning(keys: &dexdo_core::KeyPair) -> Self {
            Self {
                secret_hex: Some(keys.secret_hex().to_string()),
                failure: None,
                calls: std::cell::Cell::new(0),
            }
        }

        fn failing(message: &'static str) -> Self {
            Self {
                secret_hex: None,
                failure: Some(message),
                calls: std::cell::Cell::new(0),
            }
        }
    }

    #[cfg(feature = "shellnet")]
    impl super::NoteDeployFundingKeyLoader for FixedFundingKeyLoader {
        fn load_funding_wallet_keys(&self) -> anyhow::Result<dexdo_core::KeyPair> {
            self.calls.set(self.calls.get() + 1);
            if let Some(message) = self.failure {
                anyhow::bail!("{message}");
            }
            dexdo_core::KeyPair::from_secret_hex(
                self.secret_hex
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("fixed key loader has no secret"))?,
            )
            .map_err(|error| anyhow::anyhow!("fixed funding key: {error:?}"))
        }
    }

    #[cfg(feature = "shellnet")]
    struct FixedFundingWalletReader {
        code_hash: Option<String>,
        custodians: Option<serde_json::Value>,
        failure: Option<&'static str>,
        code_hash_calls: std::cell::Cell<usize>,
        custodian_calls: std::cell::Cell<usize>,
    }

    #[cfg(feature = "shellnet")]
    impl FixedFundingWalletReader {
        fn returning(custodians: serde_json::Value) -> Self {
            Self::with_code_hash(
                super::NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH,
                custodians,
            )
        }

        fn with_code_hash(code_hash: &str, custodians: serde_json::Value) -> Self {
            Self {
                code_hash: Some(code_hash.to_string()),
                custodians: Some(custodians),
                failure: None,
                code_hash_calls: std::cell::Cell::new(0),
                custodian_calls: std::cell::Cell::new(0),
            }
        }

        fn failing(message: &'static str) -> Self {
            Self {
                code_hash: None,
                custodians: None,
                failure: Some(message),
                code_hash_calls: std::cell::Cell::new(0),
                custodian_calls: std::cell::Cell::new(0),
            }
        }
    }

    #[cfg(feature = "shellnet")]
    #[async_trait::async_trait(?Send)]
    impl super::NoteDeployFundingWalletReader for FixedFundingWalletReader {
        async fn funding_wallet_code_hash(
            &self,
            _multisig_address: &dexdo_core::Address,
        ) -> anyhow::Result<String> {
            self.code_hash_calls.set(self.code_hash_calls.get() + 1);
            if let Some(message) = self.failure {
                anyhow::bail!("{message}");
            }
            self.code_hash
                .clone()
                .ok_or_else(|| anyhow::anyhow!("fixed wallet reader has no code hash"))
        }

        async fn funding_wallet_custodians(
            &self,
            _multisig_address: &dexdo_core::Address,
        ) -> anyhow::Result<serde_json::Value> {
            self.custodian_calls.set(self.custodian_calls.get() + 1);
            if let Some(message) = self.failure {
                anyhow::bail!("{message}");
            }
            self.custodians
                .clone()
                .ok_or_else(|| anyhow::anyhow!("fixed wallet reader has no custodians"))
        }
    }

    #[cfg(feature = "shellnet")]
    #[derive(Default)]
    struct CountingVoucherBocBuilder {
        calls: std::cell::Cell<usize>,
        saw_nonempty_boc: std::cell::Cell<bool>,
    }

    #[cfg(feature = "shellnet")]
    #[async_trait::async_trait(?Send)]
    impl super::NoteDeployVoucherBocBuilder for CountingVoucherBocBuilder {
        async fn build_voucher_submit_boc(
            &self,
            multisig_address: &dexdo_core::Address,
            multisig_keys: &dexdo_core::KeyPair,
            root_pn: &dexdo_core::Address,
            checkpoint: &crate::cli::note::NoteDeployVoucherCheckpoint,
        ) -> anyhow::Result<String> {
            self.calls.set(self.calls.get() + 1);
            let boc = super::note_deploy_build_voucher_submit_boc(
                multisig_address,
                multisig_keys,
                root_pn,
                checkpoint,
            )
            .await?;
            self.saw_nonempty_boc.set(!boc.is_empty());
            Ok(boc)
        }
    }

    #[cfg(feature = "shellnet")]
    #[derive(Default)]
    struct CountingVoucherSubmitter {
        calls: std::cell::Cell<usize>,
        saw_nonempty_boc: std::cell::Cell<bool>,
    }

    #[cfg(feature = "shellnet")]
    #[async_trait::async_trait(?Send)]
    impl super::NoteDeployVoucherSubmitter for CountingVoucherSubmitter {
        async fn submit_voucher_boc(
            &self,
            _endpoint: &str,
            _multisig_address: &dexdo_core::Address,
            boc: &str,
            _http: &reqwest::Client,
        ) -> anyhow::Result<()> {
            self.calls.set(self.calls.get() + 1);
            self.saw_nonempty_boc.set(!boc.is_empty());
            Ok(())
        }
    }

    #[cfg(feature = "shellnet")]
    fn preflight_fixture_keys() -> dexdo_core::KeyPair {
        dexdo_core::KeyPair::from_secret_hex(&"3a".repeat(32)).expect("fixture funding key")
    }

    #[cfg(feature = "shellnet")]
    async fn run_preflight_with_fixed_custodians(
        custodians: serde_json::Value,
    ) -> anyhow::Result<()> {
        let wallet = dexdo_core::Address::parse(&format!("0:{}", "a".repeat(64)))
            .expect("parse fixture wallet");
        let keys = preflight_fixture_keys();
        let reader = FixedFundingWalletReader::returning(custodians);
        let result = super::note_deploy_preflight_key_owns_wallet(&reader, &wallet, &keys).await;
        assert_eq!(
            reader.code_hash_calls.get(),
            1,
            "pre-flight must read the funding-wallet code hash exactly once"
        );
        assert_eq!(
            reader.custodian_calls.get(),
            1,
            "pre-flight must read getCustodians exactly once"
        );
        result
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_preflight_rejects_zero_pubkey_custodians() {
        let error = run_preflight_with_fixed_custodians(serde_json::json!({
            "custodians": [{
                "index": "0",
                "owner_pubkey": null,
                "owner_address": format!("0:{}", "b".repeat(64)),
            }]
        }))
        .await
        .expect_err("an address-only custodian cannot authorize a pubkey-signed direct send")
        .to_string();
        assert!(error.contains("zero pubkey custodians"), "{error}");
        assert!(error.contains("exactly one pubkey custodian"), "{error}");
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_preflight_rejects_multiple_custodians() {
        let keys = preflight_fixture_keys();
        let error = run_preflight_with_fixed_custodians(serde_json::json!({
            "custodians": [
                {
                    "index": "0",
                    "owner_pubkey": format!("0x{}", keys.public_hex()),
                },
                {
                    "index": "1",
                    "owner_pubkey": format!("0x{}", "11".repeat(32)),
                }
            ]
        }))
        .await
        .expect_err("membership is insufficient when direct sendTransaction would exit 108")
        .to_string();
        assert!(error.contains("has 2 custodians"), "{error}");
        assert!(error.contains("exactly one pubkey custodian"), "{error}");
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_preflight_rejects_mismatched_sole_custodian() {
        let error = run_preflight_with_fixed_custodians(serde_json::json!({
            "custodians": [{
                "index": "0",
                "owner_pubkey": format!("0x{}", "11".repeat(32)),
            }]
        }))
        .await
        .expect_err("a mismatched sole funding key must fail closed")
        .to_string();
        assert!(error.contains("sole custodian is"), "{error}");
        assert!(error.contains("no wallet message was submitted"), "{error}");
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_preflight_accepts_matching_sole_custodian() {
        let keys = preflight_fixture_keys();
        run_preflight_with_fixed_custodians(serde_json::json!({
            "custodians": [{
                "index": "0",
                "owner_pubkey": format!("0X{}", keys.public_hex().to_ascii_uppercase()),
            }]
        }))
        .await
        .expect("the matching sole pubkey custodian must pass");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_get_custodians_none_or_empty_reports_abi_output_error() {
        let wallet = format!("0:{}", "a".repeat(64));
        for output in [None, Some(serde_json::json!({}))] {
            let error = super::require_get_custodians_output(&wallet, output)
                .expect_err("None/empty getter output must fail as an ABI/getter diagnostic")
                .to_string();
            assert!(error.contains("is Active"), "{error}");
            assert!(error.contains("getCustodians"), "{error}");
            assert!(error.contains("no custodians output"), "{error}");
            assert!(!error.contains("is not Active"), "{error}");
        }
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_update_custodian_is_the_only_wallet_canon() {
        let abi: serde_json::Value =
            serde_json::from_str(dexdo_core::ackinacki_wallet::contracts::MULTISIG_ABI_JSON)
                .expect("parse SDK canonical UpdateCustodian ABI");
        let functions = abi["functions"].as_array().expect("ABI functions");
        let send_transaction = functions
            .iter()
            .find(|function| function["name"] == "sendTransaction")
            .expect("canonical sendTransaction function");
        assert_eq!(
            send_transaction["inputs"],
            serde_json::json!([
                { "name": "dest", "type": "address" },
                { "name": "value", "type": "uint128" },
                { "name": "cc", "type": "map(uint32,varuint32)" },
                { "name": "bounce", "type": "bool" },
                { "name": "flags", "type": "uint8" },
                { "name": "payload", "type": "cell" }
            ]),
            "SDK canonical UpdateCustodian sendTransaction shape"
        );
        let get_custodians = functions
            .iter()
            .find(|function| function["name"] == "getCustodians")
            .expect("canonical getCustodians function");
        assert_eq!(get_custodians["inputs"], serde_json::json!([]));
        assert_eq!(
            get_custodians["outputs"],
            serde_json::json!([{
                "name": "custodians",
                "type": "tuple[]",
                "components": [
                    { "name": "owner_pubkey", "type": "optional(uint256)" },
                    { "name": "owner_address", "type": "optional(address)" },
                    { "name": "index", "type": "uint8" }
                ]
            }]),
            "SDK canonical getCustodians getter shape"
        );
        let root_pn = dexdo_core::Address::parse(&format!("0:{}", "b".repeat(64)))
            .expect("parse RootPN fixture");
        let params = super::note_deploy_update_custodian_send_transaction_params(
            &root_pn,
            serde_json::Map::new(),
            "fixture-body".to_string(),
        );
        let fields = params.as_object().expect("wallet-forward params object");
        assert_eq!(
            fields.len(),
            6,
            "UpdateCustodian sendTransaction has six inputs"
        );
        assert!(
            !fields.contains_key("dapp_id"),
            "trailing dapp_id is forbidden"
        );
        assert!(
            !dexdo_core::ackinacki_wallet::contracts::MULTISIG_ABI_JSON.contains("dapp_id"),
            "UpdateCustodian ABI must not grow a Generic-wallet dapp_id"
        );
        super::ensure_note_deploy_update_custodian_code_hash(&format!(
            "0X{}",
            super::NOTE_DEPLOY_UPDATE_CUSTODIAN_MULTISIG_CODE_HASH.to_ascii_uppercase()
        ))
        .expect("canonical UpdateCustodian hash");
        let error = super::ensure_note_deploy_update_custodian_code_hash(
            "3a7a53248ff39fde936a4274eab143b5fac94feac0d8e2e2748aac5e74538d5f",
        )
        .expect_err("Generic Multisig must be unsupported")
        .to_string();
        assert!(error.contains("supports only UpdateCustodian"), "{error}");
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_missing_recovery_remains_in_memory_before_wallet_preflight() {
        let temp = tempfile::tempdir().expect("temp dir");
        let recovery_path = temp.path().join("fresh.recovery.json");
        let pool_path = temp.path().join("fresh.pool.json");
        let key_path = temp.path().join("funding.secret.hex");
        let funding_wallet = format!("0:{}", "a".repeat(64));
        let client = dexdo_core::ChainClient::connect("http://127.0.0.1:9")
            .expect("connect offline fixture endpoint");
        let halo2_paths = dexdo_core::private_note::Halo2Paths::from_env();
        let args = super::NoteDeployArgs {
            multisig_address: funding_wallet.clone(),
            multisig_key: Some(key_path),
            multisig_seed_file: None,
            nominal: "N100".to_string(),
            token_type: "nackl".to_string(),
            endpoint: "http://127.0.0.1:9".to_string(),
            pool: pool_path.clone(),
            recovery: Some(recovery_path.clone()),
            simulate_interrupt_after_spend_before_pool: false,
            simulate_interrupt_after_deposit_voucher_submit: false,
            simulate_interrupt_after_deposit_voucher_event: false,
            simulate_interrupt_after_shell_voucher_submit: false,
            simulate_interrupt_after_deploy_before_note_record: false,
        };
        let recovery_request = crate::cli::note::NoteDeployRecoveryRequest {
            endpoint: "http://127.0.0.1:9",
            nominal: "N100",
            token_type: 1,
            raw_value: 100_000_000_000,
            ecc_shell_deposit: 100_000_000_000,
            funding_multisig_address: &funding_wallet,
        };
        let mut ops = super::NoteDeployProductionOps {
            args: &args,
            client: &client,
            recovery_path: &recovery_path,
            pool_path: &pool_path,
            funding_multisig_address: &funding_wallet,
            recovery_request,
            pn_keys: None,
            halo2_paths: &halo2_paths,
            voucher_failpoints: Default::default(),
        };

        let recovery = super::NoteDeployResolvedOps::load_recovery(&mut ops)
            .await
            .expect("create fresh recovery in memory");
        assert!(
            ops.pn_keys.is_some(),
            "fresh owner key must remain available in memory"
        );
        assert!(recovery.deposit_voucher.is_none());
        assert!(recovery.shell_voucher.is_none());
        assert!(
            !recovery_path.exists(),
            "fresh journal must wait for wallet preflight"
        );
        assert!(!pool_path.exists(), "fresh pool must not exist");
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_fresh_path_rejects_non_custodian_before_all_artifacts_and_submit() {
        use crate::cli::note::NoteDeployVoucherKind;

        let temp = tempfile::tempdir().expect("temp dir");
        let client = dexdo_core::ChainClient::connect("http://127.0.0.1:9")
            .expect("connect offline fixture endpoint");
        let multisig_address = dexdo_core::Address::parse(&format!("0:{}", "a".repeat(64)))
            .expect("parse fixture wallet");
        let multisig_keys =
            dexdo_core::KeyPair::from_secret_hex(&"3a".repeat(32)).expect("fixture funding key");
        let halo2_paths = dexdo_core::private_note::Halo2Paths::from_env();
        let cases = [
            (
                "wrong-sole-key",
                serde_json::json!({
                    "custodians": [{
                        "index": "0",
                        "owner_pubkey": format!("0x{}", "11".repeat(32)),
                    }]
                }),
                "sole custodian is",
            ),
            (
                "non-sole-key",
                serde_json::json!({
                    "custodians": [
                        {
                            "index": "0",
                            "owner_pubkey": format!("0x{}", multisig_keys.public_hex()),
                        },
                        {
                            "index": "1",
                            "owner_pubkey": format!("0x{}", "11".repeat(32)),
                        }
                    ]
                }),
                "has 2 custodians",
            ),
        ];

        for (case, custodians, expected_error) in cases {
            let key_loader = FixedFundingKeyLoader::returning(&multisig_keys);
            let wallet_reader = FixedFundingWalletReader::returning(custodians);
            let boc_builder = CountingVoucherBocBuilder::default();
            let submitter = CountingVoucherSubmitter::default();
            let mut recovery = test_recovery_state();
            let owner = recovery.owner_public_key_hex.clone();
            let token_type = recovery.token_type;
            let raw_value = recovery.raw_value;
            let recovery_path = temp.path().join(format!("{case}.recovery.json"));

            let error = super::note_deploy_mint_voucher_recoverable(
                &client,
                &recovery_path,
                &mut recovery,
                NoteDeployVoucherKind::Deposit,
                &multisig_address,
                &key_loader,
                &wallet_reader,
                &boc_builder,
                &submitter,
                &owner,
                token_type,
                raw_value,
                false,
                &halo2_paths,
                Default::default(),
            )
            .await
            .expect_err("the real fresh path must reject a wrong/non-sole custodian")
            .to_string();

            assert!(error.contains(expected_error), "{case}: {error}");
            assert_eq!(key_loader.calls.get(), 1, "{case}");
            assert_eq!(wallet_reader.code_hash_calls.get(), 1, "{case}");
            assert_eq!(wallet_reader.custodian_calls.get(), 1, "{case}");
            assert_eq!(
                boc_builder.calls.get(),
                0,
                "{case}: rejected wallet must not create a signed wallet BOC"
            );
            assert_eq!(
                submitter.calls.get(),
                0,
                "{case}: rejected wallet must create zero wallet transactions"
            );
            assert!(
                recovery
                    .voucher_checkpoint(NoteDeployVoucherKind::Deposit)
                    .is_none(),
                "{case}: rejection must precede checkpoint creation"
            );
            assert!(
                !recovery_path.exists(),
                "{case}: rejection must precede journal creation"
            );
        }
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_fresh_path_rejects_generic_wallet_before_all_artifacts_and_submit() {
        use crate::cli::note::NoteDeployVoucherKind;

        let temp = tempfile::tempdir().expect("temp dir");
        let client = dexdo_core::ChainClient::connect("http://127.0.0.1:9")
            .expect("connect offline fixture endpoint");
        let multisig_address = dexdo_core::Address::parse(&format!("0:{}", "a".repeat(64)))
            .expect("parse fixture wallet");
        let multisig_keys =
            dexdo_core::KeyPair::from_secret_hex(&"3a".repeat(32)).expect("fixture funding key");
        let wallet_reader = FixedFundingWalletReader::with_code_hash(
            "3a7a53248ff39fde936a4274eab143b5fac94feac0d8e2e2748aac5e74538d5f",
            serde_json::json!({
                "custodians": [{
                    "index": "0",
                    "owner_pubkey": format!("0x{}", multisig_keys.public_hex()),
                }]
            }),
        );
        let key_loader = FixedFundingKeyLoader::returning(&multisig_keys);
        let boc_builder = CountingVoucherBocBuilder::default();
        let submitter = CountingVoucherSubmitter::default();
        let halo2_paths = dexdo_core::private_note::Halo2Paths::from_env();
        let mut recovery = test_recovery_state();
        let owner = recovery.owner_public_key_hex.clone();
        let token_type = recovery.token_type;
        let raw_value = recovery.raw_value;
        let recovery_path = temp.path().join("custodian-recovery.json");

        let error = super::note_deploy_mint_voucher_recoverable(
            &client,
            &recovery_path,
            &mut recovery,
            NoteDeployVoucherKind::Deposit,
            &multisig_address,
            &key_loader,
            &wallet_reader,
            &boc_builder,
            &submitter,
            &owner,
            token_type,
            raw_value,
            false,
            &halo2_paths,
            Default::default(),
        )
        .await
        .expect_err("Generic funding wallet must fail closed")
        .to_string();

        assert!(error.contains("supports only UpdateCustodian"), "{error}");
        assert_eq!(key_loader.calls.get(), 1);
        assert_eq!(wallet_reader.code_hash_calls.get(), 1);
        assert_eq!(
            wallet_reader.custodian_calls.get(),
            0,
            "unsupported code must stop before getter"
        );
        assert_eq!(boc_builder.calls.get(), 0);
        assert_eq!(submitter.calls.get(), 0);
        assert!(recovery
            .voucher_checkpoint(NoteDeployVoucherKind::Deposit)
            .is_none());
        assert!(!recovery_path.exists());
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_fresh_path_matching_custodian_reaches_signed_boc_and_submit_seam() {
        use crate::cli::note::NoteDeployVoucherKind;

        let temp = tempfile::tempdir().expect("temp dir");
        let client = dexdo_core::ChainClient::connect("http://127.0.0.1:9")
            .expect("connect offline fixture endpoint");
        let multisig_address = dexdo_core::Address::parse(&format!("0:{}", "a".repeat(64)))
            .expect("parse fixture wallet");
        let multisig_keys = preflight_fixture_keys();
        let wallet_reader = FixedFundingWalletReader::returning(serde_json::json!({
            "custodians": [{
                "index": "0",
                "owner_pubkey": format!("0x{}", multisig_keys.public_hex()),
            }]
        }));
        let key_loader = FixedFundingKeyLoader::returning(&multisig_keys);
        let boc_builder = CountingVoucherBocBuilder::default();
        let submitter = CountingVoucherSubmitter::default();
        let halo2_paths = dexdo_core::private_note::Halo2Paths::from_env();
        let failpoints = super::NoteDeployVoucherFailpoints {
            after_deposit_submit: true,
            ..Default::default()
        };
        let mut recovery = test_recovery_state();
        let owner = recovery.owner_public_key_hex.clone();
        let token_type = recovery.token_type;
        let raw_value = recovery.raw_value;
        let recovery_path = temp.path().join("matching-custodian-recovery.json");

        let error = super::note_deploy_mint_voucher_recoverable(
            &client,
            &recovery_path,
            &mut recovery,
            NoteDeployVoucherKind::Deposit,
            &multisig_address,
            &key_loader,
            &wallet_reader,
            &boc_builder,
            &submitter,
            &owner,
            token_type,
            raw_value,
            false,
            &halo2_paths,
            failpoints,
        )
        .await
        .expect_err("fixture stops after the injected wallet-submit seam")
        .to_string();

        assert!(
            error.contains("simulated interruption after deposit voucher wallet submit"),
            "{error}"
        );
        assert_eq!(key_loader.calls.get(), 1);
        assert_eq!(wallet_reader.code_hash_calls.get(), 1);
        assert_eq!(wallet_reader.custodian_calls.get(), 1);
        assert_eq!(boc_builder.calls.get(), 1);
        assert!(
            boc_builder.saw_nonempty_boc.get(),
            "matching sole key must produce a signed BOC"
        );
        assert_eq!(
            submitter.calls.get(),
            1,
            "matching sole key must reach wallet submit"
        );
        assert!(
            submitter.saw_nonempty_boc.get(),
            "submit seam must receive a signed BOC"
        );
        assert!(
            recovery_path.exists(),
            "guarded checkpoint must be durable before submit"
        );
        assert!(
            recovery
                .voucher_checkpoint(NoteDeployVoucherKind::Deposit)
                .expect("guarded checkpoint")
                .submit_maybe_sent,
            "submit intent must be durable before transport"
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_submit_maybe_sent_resume_skips_funding_key_and_wallet_preflight() {
        use crate::cli::note::{NoteDeployVoucherCheckpoint, NoteDeployVoucherKind};

        let temp = tempfile::tempdir().expect("temp dir");
        let client = dexdo_core::ChainClient::connect("http://127.0.0.1:9")
            .expect("connect offline fixture endpoint");
        let multisig_address = dexdo_core::Address::parse(&format!("0:{}", "a".repeat(64)))
            .expect("parse fixture wallet");
        let key_loader =
            FixedFundingKeyLoader::failing("submitted recovery must not load funding key");
        let wallet_reader =
            FixedFundingWalletReader::failing("submitted recovery must not read funding wallet");
        let boc_builder = CountingVoucherBocBuilder::default();
        let submitter = CountingVoucherSubmitter::default();
        let halo2_paths = dexdo_core::private_note::Halo2Paths::from_env();
        let failpoints = super::NoteDeployVoucherFailpoints {
            before_voucher_event_wait: true,
            ..Default::default()
        };

        let mut resumed_recovery = test_recovery_state();
        let resumed_owner = resumed_recovery.owner_public_key_hex.clone();
        let resumed_token_type = resumed_recovery.token_type;
        let resumed_raw_value = resumed_recovery.raw_value;
        let mut checkpoint = NoteDeployVoucherCheckpoint::new(
            &resumed_owner,
            resumed_token_type,
            resumed_raw_value,
            false,
            "b".repeat(64),
            "c".repeat(64),
        )
        .expect("fixture voucher checkpoint");
        checkpoint.submit_maybe_sent = true;
        resumed_recovery
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, checkpoint)
            .expect("persist resumed checkpoint");
        let resumed_recovery_path = temp.path().join("resumed-recovery.json");
        crate::cli::note::write_note_deploy_recovery(&resumed_recovery_path, &resumed_recovery)
            .expect("write resumed recovery");
        let before = std::fs::read(&resumed_recovery_path).expect("read recovery before resume");
        let mut resumed_recovery =
            crate::cli::note::load_note_deploy_recovery(&resumed_recovery_path)
                .expect("load submitted recovery")
                .expect("submitted recovery exists");

        let resumed_error = super::note_deploy_mint_voucher_recoverable(
            &client,
            &resumed_recovery_path,
            &mut resumed_recovery,
            NoteDeployVoucherKind::Deposit,
            &multisig_address,
            &key_loader,
            &wallet_reader,
            &boc_builder,
            &submitter,
            &resumed_owner,
            resumed_token_type,
            resumed_raw_value,
            false,
            &halo2_paths,
            failpoints,
        )
        .await
        .expect_err("fixture must stop before the live event wait")
        .to_string();
        assert!(
            resumed_error.contains("simulated interruption before voucher event wait"),
            "{resumed_error}"
        );
        assert!(
            !resumed_error.contains("submitted recovery must not read funding wallet"),
            "{resumed_error}"
        );
        assert!(
            !resumed_error.contains("submitted recovery must not load funding key"),
            "{resumed_error}"
        );
        assert_eq!(key_loader.calls.get(), 0);
        assert_eq!(wallet_reader.code_hash_calls.get(), 0);
        assert_eq!(wallet_reader.custodian_calls.get(), 0);
        assert_eq!(
            boc_builder.calls.get(),
            0,
            "reconciliation must not build another signed wallet BOC"
        );
        assert_eq!(
            submitter.calls.get(),
            0,
            "reconciliation must not submit another wallet BOC"
        );
        assert_eq!(
            std::fs::read(&resumed_recovery_path).expect("read recovery after resume seam"),
            before,
            "read-only reconciliation seam must not rewrite the journal without new chain facts"
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_later_fresh_shell_voucher_is_gated_after_submitted_deposit() {
        use crate::cli::note::{NoteDeployVoucherCheckpoint, NoteDeployVoucherKind};
        use dexdo_core::private_note::proof::{CURRENCY_ID_SHELL, ECC_SHELL_DEPOSIT_RAW};

        let temp = tempfile::tempdir().expect("temp dir");
        let client = dexdo_core::ChainClient::connect("http://127.0.0.1:9")
            .expect("connect offline fixture endpoint");
        let multisig_address = dexdo_core::Address::parse(&format!("0:{}", "a".repeat(64)))
            .expect("parse fixture wallet");
        let multisig_keys = preflight_fixture_keys();
        let key_loader = FixedFundingKeyLoader::returning(&multisig_keys);
        let wallet_reader = FixedFundingWalletReader::returning(serde_json::json!({
            "custodians": [{
                "index": "0",
                "owner_pubkey": format!("0x{}", "11".repeat(32)),
            }]
        }));
        let boc_builder = CountingVoucherBocBuilder::default();
        let submitter = CountingVoucherSubmitter::default();
        let halo2_paths = dexdo_core::private_note::Halo2Paths::from_env();

        let mut recovery = test_recovery_state();
        let owner = recovery.owner_public_key_hex.clone();
        let mut deposit = NoteDeployVoucherCheckpoint::new(
            &owner,
            recovery.token_type,
            recovery.raw_value,
            false,
            "b".repeat(64),
            "c".repeat(64),
        )
        .expect("fixture deposit checkpoint");
        deposit.submit_maybe_sent = true;
        recovery
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, deposit)
            .expect("persist prior deposit checkpoint");
        let recovery_path = temp.path().join("later-shell-recovery.json");
        crate::cli::note::write_note_deploy_recovery(&recovery_path, &recovery)
            .expect("write recovery with submitted deposit");
        let before = std::fs::read(&recovery_path).expect("read recovery before SHELL leg");

        let error = super::note_deploy_mint_voucher_recoverable(
            &client,
            &recovery_path,
            &mut recovery,
            NoteDeployVoucherKind::ShellGas,
            &multisig_address,
            &key_loader,
            &wallet_reader,
            &boc_builder,
            &submitter,
            &owner,
            CURRENCY_ID_SHELL,
            ECC_SHELL_DEPOSIT_RAW,
            true,
            &halo2_paths,
            Default::default(),
        )
        .await
        .expect_err("a later fresh voucher leg must run the wallet guard")
        .to_string();

        assert!(error.contains("sole custodian is"), "{error}");
        assert_eq!(key_loader.calls.get(), 1);
        assert_eq!(wallet_reader.code_hash_calls.get(), 1);
        assert_eq!(wallet_reader.custodian_calls.get(), 1);
        assert_eq!(boc_builder.calls.get(), 0);
        assert_eq!(submitter.calls.get(), 0);
        assert!(
            recovery
                .voucher_checkpoint(NoteDeployVoucherKind::ShellGas)
                .is_none(),
            "rejection must precede the fresh SHELL checkpoint"
        );
        assert!(
            recovery
                .voucher_checkpoint(NoteDeployVoucherKind::Deposit)
                .is_some(),
            "the prior submitted deposit checkpoint must remain intact"
        );
        assert_eq!(
            std::fs::read(&recovery_path).expect("read recovery after rejected SHELL leg"),
            before,
            "rejected later leg must not rewrite the existing journal"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_wallet_replay_conflict_is_busy_retryable_and_actionable() {
        let raw = anyhow::anyhow!(
            "submit UpdateCustodianMultisigWallet.sendTransaction -> RootPN.generateVoucher: block manager rejected \
             message code=TVM_ERROR; exit-code:52 nonce desynchronized"
        );

        assert!(crate::cli::commands::is_note_deploy_wallet_busy_error(&raw));
        assert!(super::is_note_deploy_wallet_submit_busy_error(&raw));
        let error = crate::cli::commands::note_deploy_error("0:wallet", raw).to_string();
        assert!(error.contains("wallet busy/out-of-sync"), "{error}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_root_pn_compute_revert_is_not_wallet_busy_and_keeps_cause() {
        let raw = anyhow::anyhow!(
            "deployPrivateNote reverted: tvm_error exit_code=60 contract execution failed"
        );

        assert!(!crate::cli::commands::is_note_deploy_wallet_busy_error(
            &raw
        ));
        let error = crate::cli::commands::note_deploy_error("0:wallet", raw).to_string();
        assert!(error.contains("exit_code=60"), "{error}");
        assert!(!error.contains("wallet busy"), "{error}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_prover_and_later_stage_replay_errors_are_not_relabeled() {
        let prover = anyhow::anyhow!("prove deposit voucher: ERR_INVALID_ZKPROOF in halo2 prover");
        assert!(!crate::cli::commands::is_note_deploy_wallet_busy_error(
            &prover
        ));
        let prover_error = crate::cli::commands::note_deploy_error("0:wallet", prover).to_string();
        assert!(
            prover_error.contains("ERR_INVALID_ZKPROOF"),
            "{prover_error}"
        );
        assert!(!prover_error.contains("wallet busy"), "{prover_error}");

        let later_stage = anyhow::anyhow!(
            "RootPN.deployPrivateNote: block manager rejected message code=TVM_ERROR; \
             exit_code=52 replay protection exception"
        );
        assert!(crate::cli::commands::is_note_deploy_wallet_busy_error(
            &later_stage
        ));
        assert!(!super::is_note_deploy_wallet_submit_busy_error(
            &later_stage
        ));
        let later_stage_error =
            super::note_deploy_resume_error("0:wallet", later_stage).to_string();
        assert!(later_stage_error.contains("RootPN.deployPrivateNote"));
        assert!(later_stage_error.contains("exit_code=52"));
        assert!(!later_stage_error.contains("wallet busy"));
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_wallet_submit_retry_loop_succeeds_on_third_attempt_with_expected_backoff()
    {
        let mut attempts = Vec::new();
        let mut backoffs = Vec::new();

        let state = super::run_note_deploy_with_wallet_busy_retry(
            "0:wallet",
            async |attempt| {
                attempts.push(attempt);
                if attempt < 3 {
                    Err(anyhow::anyhow!(
                        "submit UpdateCustodianMultisigWallet.sendTransaction -> RootPN.generateVoucher: \
                         tvm_error exit-code:52 nonce desynchronized"
                    ))
                } else {
                    Ok("deployed")
                }
            },
            async |duration| backoffs.push(duration),
        )
        .await
        .expect("wallet-submit retry should succeed on attempt 3");

        assert_eq!(state, "deployed");
        assert_eq!(attempts, vec![1, 2, 3]);
        assert_eq!(
            backoffs,
            vec![
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(20)
            ]
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_wallet_submit_retry_loop_exhausts_three_attempts_with_expected_backoff() {
        let mut attempts = Vec::new();
        let mut backoffs = Vec::new();

        let result: anyhow::Result<()> = super::run_note_deploy_with_wallet_busy_retry(
            "0:wallet",
            async |attempt| {
                attempts.push(attempt);
                Err(anyhow::anyhow!(
                    "submit UpdateCustodianMultisigWallet.sendTransaction -> RootPN.generateVoucher: \
                     tvm_error exit-code:52 nonce desynchronized"
                ))
            },
            async |duration| backoffs.push(duration),
        )
        .await;

        let error = result.expect_err("wallet-submit retry should stop after attempt 3");
        assert!(error.to_string().contains("wallet busy/out-of-sync"));
        assert_eq!(attempts, vec![1, 2, 3]);
        assert_eq!(
            backoffs,
            vec![
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(20)
            ]
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_retry_loop_does_not_retry_root_pn_exit_52() {
        let mut attempts = Vec::new();
        let mut backoffs = Vec::new();

        let result: anyhow::Result<()> = super::run_note_deploy_with_wallet_busy_retry(
            "0:wallet",
            async |attempt| {
                attempts.push(attempt);
                Err(anyhow::anyhow!(
                    "RootPN.deployPrivateNote reverted: tvm_error exit-code:52 replay protection"
                ))
            },
            async |duration| backoffs.push(duration),
        )
        .await;

        let error = result.expect_err("non-wallet RootPN exit 52 must fail immediately");
        let message = error.to_string();
        assert!(message.contains("exit-code:52"), "{message}");
        assert!(!message.contains("wallet busy"), "{message}");
        assert_eq!(attempts, vec![1]);
        assert!(backoffs.is_empty());
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_retry_loop_does_not_retry_downstream_tvm_exit_60() {
        let mut attempts = Vec::new();
        let mut backoffs = Vec::new();

        let result: anyhow::Result<()> = super::run_note_deploy_with_wallet_busy_retry(
            "0:wallet",
            async |attempt| {
                attempts.push(attempt);
                Err(anyhow::anyhow!(
                    "downstream deploy failed: tvm_error exit_code=60"
                ))
            },
            async |duration| backoffs.push(duration),
        )
        .await;

        let error = result.expect_err("downstream TVM exit 60 must fail immediately");
        let message = error.to_string();
        assert!(message.contains("tvm_error exit_code=60"), "{message}");
        assert!(!message.contains("wallet busy"), "{message}");
        assert_eq!(attempts, vec![1]);
        assert!(backoffs.is_empty());
    }

    #[cfg(feature = "shellnet")]
    fn write_test_file(dir: &std::path::Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).expect("write test fixture");
    }

    #[cfg(feature = "shellnet")]
    fn test_recovery_state() -> crate::cli::note::NoteDeployRecoveryState {
        use crate::cli::note::{NoteDeployRecoveryRequest, NoteDeployRecoveryState};

        let owner = dexdo_core::KeyPair::from_secret_hex(&"2a".repeat(32)).expect("test owner key");
        NoteDeployRecoveryState::new(
            NoteDeployRecoveryRequest {
                endpoint: "http://127.0.0.1:9",
                nominal: "N100",
                token_type: 1,
                raw_value: 100_000_000_000,
                ecc_shell_deposit: 100_000_000_000,
                funding_multisig_address: &format!("0:{}", "a".repeat(64)),
            },
            owner.public_hex(),
            owner.secret_hex(),
        )
        .expect("test recovery state")
    }

    #[cfg(feature = "shellnet")]
    fn persisted_voucher_checkpoint(
        owner_public_key_hex: &str,
        token_type: u32,
        raw_value: u64,
        is_fee: bool,
        fixture_digit: char,
    ) -> crate::cli::note::NoteDeployVoucherCheckpoint {
        use crate::cli::note::{
            NoteDeployVoucherCheckpoint, NoteDeployVoucherEvent, NoteDeployVoucherProof,
        };

        let sk_u_hex = fixture_digit.to_string().repeat(64);
        let sk_u_commit_hex = if fixture_digit == 'b' {
            "c".repeat(64)
        } else {
            "d".repeat(64)
        };
        let mut checkpoint = NoteDeployVoucherCheckpoint::new(
            owner_public_key_hex,
            token_type,
            raw_value,
            is_fee,
            sk_u_hex.clone(),
            sk_u_commit_hex.clone(),
        )
        .expect("voucher checkpoint");
        checkpoint.submit_maybe_sent = true;
        checkpoint.event = Some(NoteDeployVoucherEvent {
            id: format!("event-{fixture_digit}"),
            boc: "fixture-boc".into(),
            body: "fixture-body".into(),
            dst: format!("0:{}", "e".repeat(64)),
            created_at: 1,
            block_id: Some("fixture-block".into()),
        });
        checkpoint.proof = Some(NoteDeployVoucherProof {
            proof: format!("fixture-proof-{fixture_digit}"),
            deposit_identifier_hash_hex: fixture_digit.to_string().repeat(64),
            final_layer_historical_hash_root_hex: "1".repeat(64),
            voucher_nominal_fr_hex: "2".repeat(64),
            token_type_fr_hex: "3".repeat(64),
            ephemeral_pubkey_hex: owner_public_key_hex.to_string(),
            voucher_value: raw_value,
            voucher_token_type: token_type,
            layer_number: 1,
            sk_u_hex,
            sk_u_commit_hex,
        });
        checkpoint
            .validate("persisted test voucher")
            .expect("valid persisted voucher");
        checkpoint
    }

    #[cfg(feature = "shellnet")]
    #[derive(Debug, Default)]
    struct FakeNoteDeployResolvedOps {
        recovery: Option<crate::cli::note::NoteDeployRecoveryState>,
        pool_path: std::path::PathBuf,
        cache_unavailable_or_contended: bool,
        events: Vec<&'static str>,
        preflight_calls: usize,
        wallet_submits: usize,
        proof_calls: usize,
        chain_resumes: usize,
        pool_finalizations: usize,
        deposit_proof_preserved: bool,
        shell_proof_preserved: bool,
    }

    #[cfg(feature = "shellnet")]
    #[async_trait::async_trait(?Send)]
    impl super::NoteDeployResolvedOps for FakeNoteDeployResolvedOps {
        async fn load_recovery(
            &mut self,
        ) -> anyhow::Result<crate::cli::note::NoteDeployRecoveryState> {
            self.events.push("recovery_load");
            self.recovery
                .take()
                .ok_or_else(|| anyhow::anyhow!("fake recovery is missing"))
        }

        async fn preflight_prover(&mut self) -> anyhow::Result<()> {
            self.preflight_calls += 1;
            self.events.push("prover_preflight");
            if self.cache_unavailable_or_contended {
                anyhow::bail!("fake prover cache unavailable or contended");
            }
            Ok(())
        }

        async fn resume_chain(
            &mut self,
            recovery: &mut crate::cli::note::NoteDeployRecoveryState,
        ) -> anyhow::Result<crate::cli::note::OnboardPnState> {
            use crate::cli::note::NoteDeployVoucherKind;

            if recovery.shell_funded && recovery.sanity_checked {
                self.events.push("completed_recovery");
                return recovery.to_onboard_state();
            }

            let both_proofs_persisted = [
                NoteDeployVoucherKind::Deposit,
                NoteDeployVoucherKind::ShellGas,
            ]
            .into_iter()
            .all(|kind| {
                recovery
                    .voucher_checkpoint(kind)
                    .and_then(|checkpoint| checkpoint.proof.as_ref())
                    .is_some()
            });
            if both_proofs_persisted {
                self.events.push("chain_resume");
                self.chain_resumes += 1;
            } else {
                if self.preflight_calls != 1 {
                    anyhow::bail!("fresh recovery reached wallet submit before prover preflight");
                }
                self.events.push("wallet_submit");
                self.wallet_submits += 1;
                self.events.push("prove");
                self.proof_calls += 1;
                self.events.push("chain_resume");
                self.chain_resumes += 1;
            }

            recovery.mark_private_note_deployed(
                format!("0:{}", "6".repeat(64)),
                "7".repeat(64),
                2,
            )?;
            recovery.mark_shell_funded_and_checked()?;
            recovery.to_onboard_state()
        }

        async fn finalize_pool(
            &mut self,
            recovery: &crate::cli::note::NoteDeployRecoveryState,
            state: &crate::cli::note::OnboardPnState,
        ) -> anyhow::Result<()> {
            use crate::cli::note::NoteDeployVoucherKind;

            self.events.push("pool_finalize");
            self.pool_finalizations += 1;
            self.deposit_proof_preserved = recovery
                .voucher_checkpoint(NoteDeployVoucherKind::Deposit)
                .and_then(|checkpoint| checkpoint.proof.as_ref())
                .is_some();
            self.shell_proof_preserved = recovery
                .voucher_checkpoint(NoteDeployVoucherKind::ShellGas)
                .and_then(|checkpoint| checkpoint.proof.as_ref())
                .is_some();
            super::note_deploy_fold_state_into_pool(
                &self.pool_path,
                state,
                &recovery.funding_multisig_address,
            )?;
            Ok(())
        }
    }

    #[cfg(feature = "shellnet")]
    fn no_fetch(_url: &str) -> std::future::Ready<anyhow::Result<Vec<u8>>> {
        std::future::ready(Err(anyhow::anyhow!("fetcher must not be called")))
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_committed_complete_cache_skips_fetch_and_invalidation() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path();
        let srs = b"valid cached test SRS";
        let expected = super::sha256_hex(srs);
        write_test_file(dir, super::HERMEZ_SRS_NAME, srs);
        write_test_file(dir, super::HERMEZ_SRS_MARKER_NAME, expected.as_bytes());
        for name in super::PROVER_CACHE_ARTIFACTS {
            write_test_file(dir, name, format!("previously-proven-{name}").as_bytes());
        }

        super::ensure_hermez_srs_and_valid_pk_cache_with_options(
            dir,
            &expected,
            no_fetch,
            super::invalidate_stale_pk_cache,
        )
        .await
        .expect("valid cache");

        for name in super::PROVER_CACHE_ARTIFACTS {
            assert!(dir.join(name).exists(), "{name} was unexpectedly removed");
        }
        assert!(!dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME).exists());
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_wrong_hermez_srs_download_is_rejected_without_install() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path();
        let expected = super::sha256_hex(b"expected SRS");

        let error = super::ensure_hermez_srs_and_valid_pk_cache_with_options(
            dir,
            &expected,
            |_| async { Ok(b"wrong SRS".to_vec()) },
            super::invalidate_stale_pk_cache,
        )
        .await
        .expect_err("wrong SRS must fail");

        assert!(error.to_string().contains("sha256 mismatch"), "{error:#}");
        assert!(!dir.join(super::HERMEZ_SRS_NAME).exists());
        assert!(!dir.join(super::HERMEZ_SRS_MARKER_NAME).exists());
        assert!(!dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME).exists());
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_marker_mismatch_removes_all_stale_pk_artifacts() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path();
        let srs = b"valid test SRS";
        let expected = super::sha256_hex(srs);
        write_test_file(dir, super::HERMEZ_SRS_NAME, srs);
        write_test_file(dir, super::HERMEZ_SRS_MARKER_NAME, b"old-srs");
        for name in super::PROVER_CACHE_ARTIFACTS {
            write_test_file(dir, name, b"stale");
        }

        super::ensure_hermez_srs_and_valid_pk_cache_with_options(
            dir,
            &expected,
            no_fetch,
            super::invalidate_stale_pk_cache,
        )
        .await
        .expect("invalidate stale artifacts");

        for name in super::PROVER_CACHE_ARTIFACTS {
            assert!(!dir.join(name).exists(), "{name} was not removed");
        }
        assert_eq!(
            std::fs::read_to_string(dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME))
                .expect("read pending marker"),
            expected
        );
        assert!(!dir.join(super::HERMEZ_SRS_MARKER_NAME).exists());
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_interrupted_pk_publication_self_heals_before_keygen() {
        for initial_marker in [
            super::HERMEZ_SRS_MARKER_NAME,
            super::HERMEZ_SRS_PENDING_MARKER_NAME,
        ] {
            let temp = tempfile::tempdir().expect("temp dir");
            let dir = temp.path();
            let srs = b"valid interrupted-keygen SRS";
            let expected = super::sha256_hex(srs);
            write_test_file(dir, super::HERMEZ_SRS_NAME, srs);
            write_test_file(dir, initial_marker, expected.as_bytes());
            write_test_file(dir, "pk_cache.bin", b"partially-published-pk");

            super::ensure_hermez_srs_and_valid_pk_cache_with_options(
                dir,
                &expected,
                no_fetch,
                super::invalidate_stale_pk_cache,
            )
            .await
            .expect("self-heal interrupted cache");

            for name in super::PROVER_CACHE_ARTIFACTS {
                assert!(!dir.join(name).exists(), "{name} was not invalidated");
            }
            assert!(!dir.join(super::HERMEZ_SRS_MARKER_NAME).exists());
            assert_eq!(
                std::fs::read_to_string(dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME))
                    .expect("read pending marker"),
                expected
            );

            for name in super::PROVER_CACHE_ARTIFACTS {
                write_test_file(dir, name, format!("clean-keygen-{name}").as_bytes());
            }
            super::promote_hermez_srs_pending_marker(dir, &expected)
                .expect("commit successful proof cache");
            assert!(!dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME).exists());
            assert_eq!(
                std::fs::read_to_string(dir.join(super::HERMEZ_SRS_MARKER_NAME))
                    .expect("read committed marker"),
                expected
            );
        }
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_atomic_install_replaces_existing_corrupt_srs() {
        let temp = tempfile::tempdir().expect("temp dir");
        let srs_path = temp.path().join(super::HERMEZ_SRS_NAME);
        write_test_file(temp.path(), super::HERMEZ_SRS_NAME, b"corrupt");

        super::install_hermez_srs_atomically(&srs_path, b"verified replacement")
            .expect("replace existing SRS");

        assert_eq!(
            std::fs::read(&srs_path).expect("read replaced SRS"),
            b"verified replacement"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_marker_promotion_atomically_replaces_existing_destination() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path();
        let srs = b"valid promotion SRS";
        let expected = super::sha256_hex(srs);
        write_test_file(dir, super::HERMEZ_SRS_NAME, srs);
        write_test_file(dir, super::HERMEZ_SRS_MARKER_NAME, b"stale marker");
        write_test_file(
            dir,
            super::HERMEZ_SRS_PENDING_MARKER_NAME,
            expected.as_bytes(),
        );
        for name in super::PROVER_CACHE_ARTIFACTS {
            write_test_file(dir, name, format!("successful-proof-{name}").as_bytes());
        }

        super::promote_hermez_srs_pending_marker(dir, &expected)
            .expect("atomically replace marker");

        assert!(!dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join(super::HERMEZ_SRS_MARKER_NAME))
                .expect("read promoted marker"),
            expected
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_orchestration_completed_recovery_bypasses_unavailable_cache_and_finalizes_pool(
    ) {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut recovery = test_recovery_state();
        recovery
            .mark_private_note_deployed(format!("0:{}", "4".repeat(64)), "5".repeat(64), 1)
            .expect("record active note");
        recovery
            .mark_shell_funded_and_checked()
            .expect("record completed funding");

        let mut ops = FakeNoteDeployResolvedOps {
            recovery: Some(recovery),
            pool_path: temp.path().join("completed-pool.json"),
            cache_unavailable_or_contended: true,
            ..Default::default()
        };
        super::run_note_deploy_resolved(&mut ops)
            .await
            .expect("completed recovery must finalize while the prover cache is unavailable");

        assert_eq!(ops.preflight_calls, 0);
        assert_eq!(ops.wallet_submits, 0);
        assert_eq!(ops.proof_calls, 0);
        assert_eq!(ops.chain_resumes, 0);
        assert_eq!(ops.pool_finalizations, 1);
        assert_eq!(
            ops.events,
            ["recovery_load", "completed_recovery", "pool_finalize"]
        );
        assert!(
            ops.pool_path.exists(),
            "completed recovery did not write pool"
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_orchestration_persisted_proofs_bypass_contended_cache_and_resume_chain() {
        use crate::cli::note::NoteDeployVoucherKind;

        let temp = tempfile::tempdir().expect("temp dir");
        let mut recovery = test_recovery_state();
        let owner_public_key_hex = recovery.owner_public_key_hex.clone();
        recovery
            .set_voucher_checkpoint(
                NoteDeployVoucherKind::Deposit,
                persisted_voucher_checkpoint(
                    &owner_public_key_hex,
                    recovery.token_type,
                    recovery.raw_value,
                    false,
                    'b',
                ),
            )
            .expect("persist deposit proof");
        recovery
            .set_voucher_checkpoint(
                NoteDeployVoucherKind::ShellGas,
                persisted_voucher_checkpoint(
                    &owner_public_key_hex,
                    2,
                    recovery.ecc_shell_deposit,
                    true,
                    'f',
                ),
            )
            .expect("persist SHELL proof");

        let mut ops = FakeNoteDeployResolvedOps {
            recovery: Some(recovery),
            pool_path: temp.path().join("persisted-proofs-pool.json"),
            cache_unavailable_or_contended: true,
            ..Default::default()
        };
        super::run_note_deploy_resolved(&mut ops)
            .await
            .expect("persisted proofs must resume chain while the prover cache is contended");

        assert_eq!(ops.preflight_calls, 0);
        assert_eq!(ops.wallet_submits, 0);
        assert_eq!(ops.proof_calls, 0);
        assert_eq!(ops.chain_resumes, 1);
        assert_eq!(ops.pool_finalizations, 1);
        assert_eq!(
            ops.events,
            ["recovery_load", "chain_resume", "pool_finalize"]
        );
        assert!(ops.pool_path.exists(), "chain recovery did not write pool");
        assert!(ops.deposit_proof_preserved);
        assert!(ops.shell_proof_preserved);
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_orchestration_fresh_recovery_preflights_before_first_wallet_submit() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut ops = FakeNoteDeployResolvedOps {
            recovery: Some(test_recovery_state()),
            pool_path: temp.path().join("fresh-pool.json"),
            ..Default::default()
        };

        super::run_note_deploy_resolved(&mut ops)
            .await
            .expect("fresh recovery should preflight, prove, resume, and finalize");

        assert_eq!(ops.preflight_calls, 1);
        assert_eq!(ops.wallet_submits, 1);
        assert_eq!(ops.proof_calls, 1);
        assert_eq!(ops.chain_resumes, 1);
        assert_eq!(ops.pool_finalizations, 1);
        assert_eq!(
            ops.events,
            [
                "recovery_load",
                "prover_preflight",
                "wallet_submit",
                "prove",
                "chain_resume",
                "pool_finalize"
            ]
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_pk_removal_failure_never_publishes_marker() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path();
        let srs = b"valid removal-failure SRS";
        let expected = super::sha256_hex(srs);
        write_test_file(dir, super::HERMEZ_SRS_NAME, srs);
        write_test_file(dir, "pk_cache.bin", b"stale");

        let error = super::ensure_hermez_srs_and_valid_pk_cache_with_options(
            dir,
            &expected,
            no_fetch,
            |cache_dir| {
                super::invalidate_stale_pk_cache_with(cache_dir, |path| {
                    if path.file_name().is_some_and(|name| name == "pk_cache.bin") {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "injected removal failure",
                        ))
                    } else {
                        std::fs::remove_file(path)
                    }
                })
            },
        )
        .await
        .expect_err("removal failure must fail pre-flight");

        assert!(
            error.to_string().contains("injected removal failure"),
            "{error:#}"
        );
        assert!(!dir.join(super::HERMEZ_SRS_MARKER_NAME).exists());
        assert!(dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME).exists());
        assert!(dir.join("pk_cache.bin").exists());
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_deploy_failed_download_preserves_previous_srs_bytes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path();
        let previous_srs = b"previously valid SRS";
        write_test_file(dir, super::HERMEZ_SRS_NAME, previous_srs);
        let expected_new_sha = super::sha256_hex(b"new expected SRS");

        let error = super::ensure_hermez_srs_and_valid_pk_cache_with_options(
            dir,
            &expected_new_sha,
            |_| async { Err(anyhow::anyhow!("injected interrupted download")) },
            super::invalidate_stale_pk_cache,
        )
        .await
        .expect_err("failed replacement download");

        assert!(
            error.to_string().contains("injected interrupted download"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read(dir.join(super::HERMEZ_SRS_NAME)).expect("read previous SRS"),
            previous_srs
        );
        assert!(!dir.join(super::HERMEZ_SRS_MARKER_NAME).exists());
        assert!(!dir.join(super::HERMEZ_SRS_PENDING_MARKER_NAME).exists());
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_prover_cache_lock_serializes_times_out_and_releases_on_drop() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path().to_path_buf();
        let lock = super::acquire_note_deploy_prover_cache_lock_with_timeout(
            &dir,
            std::time::Duration::from_secs(1),
        )
        .expect("first lock");
        let contender = std::thread::spawn(move || {
            super::acquire_note_deploy_prover_cache_lock_with_timeout(
                &dir,
                std::time::Duration::from_secs(1),
            )
            .expect_err("second acquirer must time out")
        });
        let error = contender.join().expect("contender thread");
        assert!(
            error.to_string().contains("prover cache busy: waited 1s"),
            "{error:#}"
        );

        drop(lock);
        super::acquire_note_deploy_prover_cache_lock_with_timeout(
            temp.path(),
            std::time::Duration::from_secs(1),
        )
        .expect("lock after guard drop");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_fs2_contended_lock_error_is_retryable_on_this_platform() {
        let error = fs2::lock_contended_error();
        assert!(
            super::note_deploy_lock_is_contended(&error),
            "fs2's platform-specific contention error must enter the bounded retry path"
        );
    }

    /// regression: note withdraw is an owner-signed PrivateNote write. A mismatched --note-key must
    /// hit the existing owner-key guidance before `withdrawTokens` can surface a bare ERR_INVALID_SENDER 101.
    #[test]
    fn note_withdraw_checks_owner_before_submit() {
        let source = include_str!("note_cmd.rs");
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
    fn note_stream_lock_deadline_is_exact_first_clearable_second() {
        const LAST_CHANGE_UNIX: u64 = 1_000_000;
        assert_eq!(
            super::note_stream_lock_deadline(LAST_CHANGE_UNIX),
            LAST_CHANGE_UNIX + dexdo_core::shellnet::PRIVATE_NOTE_STREAM_LOCK_MAX_SECS
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn stream_locks_command_decodes_and_lists_locked_deals_with_timers() {
        const PRIVATE_NOTE_ABI: &str =
            include_str!("../../../../contracts/compiled_0.79.3/dex/PrivateNote.abi.json");
        const STREAM_DEAL: &str =
            "0:1111111111111111111111111111111111111111111111111111111111111111";
        const DISPUTE_DEAL: &str =
            "0:2222222222222222222222222222222222222222222222222222222222222222";
        let context = dexdo_core::airegistry::deploy::local_context().expect("local TVM context");
        let stream_call = dexdo_core::airegistry::calls::encode_internal_payload(
            &context,
            PRIVATE_NOTE_ABI,
            "streamLock",
            serde_json::json!({
                "sellerPubkey": format!("0x{}", "1".repeat(64)),
                "nonce": "7",
            }),
        )
        .await
        .expect("encode streamLock inbound call");
        let dispute_call = dexdo_core::airegistry::calls::encode_internal_payload(
            &context,
            PRIVATE_NOTE_ABI,
            "streamDisputeLock",
            serde_json::json!({
                "sellerPubkey": format!("0x{}", "2".repeat(64)),
                "nonce": "8",
            }),
        )
        .await
        .expect("encode streamDisputeLock inbound call");
        let status = dexdo_core::shellnet::NoteStreamLockStatus::from_successful_inbound_calls(
            1,
            1,
            1_000,
            [
                (900, stream_call.as_str(), true, Some(STREAM_DEAL)),
                (1_000, dispute_call.as_str(), true, Some(DISPUTE_DEAL)),
            ],
        )
        .expect("decode and reconstruct active lock deals");

        let rendered = super::render_note_stream_locks("0:note", &status, 1_100);
        assert!(rendered.contains("stream_locks=1"), "{rendered}");
        assert!(rendered.contains("dispute_locks=1"), "{rendered}");
        assert!(
            rendered.contains(&format!("kind=stream deal={STREAM_DEAL}")),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("kind=dispute deal={DISPUTE_DEAL}")),
            "{rendered}"
        );
        assert!(rendered.contains("force_clear_after_unix="), "{rendered}");
        assert!(rendered.contains("history_complete=true"), "{rendered}");
        assert!(rendered.contains(&format!("dexdo reclaim --token-contract {STREAM_DEAL}")));
        assert!(rendered.contains(&format!("dexdo stop --token-contract {STREAM_DEAL}")));
    }

    /// the command body is read-only and address-only: no key read and no signed/write helper.
    #[test]
    fn note_balance_command_path_is_read_only() {
        let source = include_str!("note_cmd.rs");
        let start = source
            .find("pub(crate) async fn run_note_balance")
            .expect("run_note_balance present");
        // Cover BOTH cfg variants (the shellnet implementation and the
        // not(shellnet) fallback stub): end at the next command handler.
        let end = source[start..]
            .find("/// `dexdo note withdraw`")
            .map(|offset| start + offset)
            .expect("run_note_balance end marker present");
        let body = &source[start..end];
        assert_eq!(
            body.matches("pub(crate) async fn run_note_balance").count(),
            2,
            "expected both run_note_balance variants in the inspected range: {body}"
        );
        assert!(body.contains(".get_account("), "{body}");
        assert!(body.contains(".private_note_details("), "{body}");
        for forbidden in [
            "read_secret_hex",
            "note_key",
            "KeyPair",
            ".submit(",
            ".call(",
            "withdraw_note_tokens",
        ] {
            assert!(
                !body.contains(forbidden),
                "run_note_balance contains forbidden write/key path {forbidden}: {body}"
            );
        }
    }
}
