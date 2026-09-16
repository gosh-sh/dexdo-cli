//! `dexdo` CLI command handlers (`seller`/`buyer`/`monitor`/`provision`/`destroy`/`recover`), split out of
//! `main.rs` (PR3, move-only). Behavior-identical to the pre-split handlers.

pub(crate) use crate::cli::accumulator::run_accumulator;
pub(crate) use crate::cli::admin::{
    run_destroy, run_market_deploy, run_provision, run_provision_with_deal_gas_overhead,
};
use crate::cli::args::*;
pub(crate) use crate::cli::close::run_close;
use crate::cli::deals;
pub(crate) use crate::cli::market_views::{
    run_executable_book, run_market, run_market_data, run_quote,
};
pub(crate) use crate::cli::markets::run_markets;
pub(crate) use crate::cli::monitor::run_monitor;
pub(crate) use crate::cli::note_cmd::{
    run_note_balance, run_note_deploy, run_note_outstanding, run_note_recover, run_note_sweep,
    run_note_topup, run_note_transfer, run_note_wallet, run_note_withdraw,
};
pub(crate) use crate::cli::oracle::run_oracle;
pub(crate) use crate::cli::orders::run_orders;
use crate::cli::policy;
pub(crate) use crate::cli::recover::{
    run_dispute, run_reclaim, run_recover, run_release_dispute, run_resolve_dispute_timeout,
    run_withdraw_shell,
};
pub(crate) use crate::cli::reports::{
    run_dashboard, run_deals, run_export, run_history, run_status,
};
pub(crate) use crate::cli::seller::{run_seller, run_seller_with_deal_gas_overhead};
pub(crate) use crate::cli::settlement_receipt::run_settlement_receipt;
use crate::cli::support::*;
use anyhow::{bail, Context as _, Result};
use dexdo::registry::{
    default_model_registry_address, resolve_registered_model_identity,
    resolve_registered_model_identity_with, ModelRegistryReader, RegistrySuggestions,
};
use dexdo::registry::{
    enforce_model_registry_policy as enforce_model_registry_policy_with_reader,
    ChainModelRegistryReader,
};
use dexdo::registry::{
    BuyerMissingBookPolicy, RegistryBookAction, RegistryRole, RegistryValidationInput,
    RegistryValidationPolicy,
};
use dexdo_core::chain::LiveBookOrder;
use dexdo_core::params::{
    DEFAULT_PN_POOL_PATH, EXECUTABLE_READ_BACKOFF, POOL_LOCK_POLL_INTERVAL, POOL_LOCK_TIMEOUT_SECS,
};
use dexdo_core::OrderBookSnapshot;
use dexdo_core::{
    model_hash_for, DobParams, MockChainBackend, OfferListing, OrderBookOrder, ProtocolConsts,
};
use serde_json::{json, Value};
use std::future::Future;
use std::io::Write as _;
use zeroize::Zeroizing;

/// Where the deployment manifest is, or the refusal that says how to say so.

/// One place, so the sentence an operator reads is the same whichever command they ran. The flag
/// this replaces (`--contracts`) is gone for good: it was the last way left to disagree with the
/// manifest about which network a command belongs to, and removed the disagreement rather

pub(crate) fn manifest_path() -> Result<std::path::PathBuf> {
    // A UNIT test calls these functions directly, in this process, so it needs a manifest without
    // a variable -- and it must get the SAME one on every machine. So in a unit-test build THIS
    // function does not consult the environment: the answer is this thread's override if a test set
    // one (`manifest_for_this_thread`), and otherwise the manifest `manifest/for-tests` names.

    // "This function", and not "the suite". `dexdo_core::params::current_network()` reads
    // `DEXDO_MANIFEST` directly, and `dexdo-core` is a dependency -- `cfg(test)` is false there, so
    // nothing here can reach it. Measured: no test diverges today, `--bin dexdo` gives the same
    // 1526/0/11 with the variable unset and with it pointed at a hostile fixture. It is a latent
    // trap rather than a break, and worth knowing before someone asserts on a refusal built from
    // that label.

    // Consulting the variable first was the bug. After `DEXDO_MANIFEST` is the ONLY way to
    // run the client, so every contributor has it exported -- and then it outranked the thread-local
    // override, which is reached from `committed_manifest_for_tests` alone. Measured on
    // `note_topup_refuses_while_note_deploy_holds_the_funding_wallet_1291`: green in 1.06s with the
    // variable unset, red in 7.57s with it set, the seconds being the test dialling the endpoint
    // that manifest names. `cargo test` was reading the operator's own chain out of their shell.

    // It is `#[cfg(test)]`, so no shipped binary has a fallback of any kind -- the refusal is what
    // an operator gets, and the tests that assert THAT run the binary as a child with the variable
    // cleared, where this branch does not exist.
    #[cfg(test)]
    {
        Ok(committed_manifest_for_tests())
    }
    #[cfg(not(test))]
    {
        dexdo_core::params::manifest_path().map_err(|refusal| anyhow::anyhow!("{refusal}"))
    }
}

// A manifest one test wants this process to read, for the duration of that test.

// THREAD-local, and that is the whole point. The obvious way to aim an in-process command at a
// fixture manifest is `std::env::set_var("DEXDO_MANIFEST",...)`, and it is wrong here: the
// environment belongs to the PROCESS, unit tests run on many threads at once, and a fixture set by
// one test is read by every other test that happens to be mid-flight. Measured, not imagined --
// four unrelated tests went red on CI while passing locally, because the scheduling differed
// (pipeline 6884, `build-test-lint`): one test's `net-a` fixture, which carries no `endpoint`, was
// picked up by the onboarding and buyer-backend tests as the manifest of their own run.

// A `#[tokio::test]` runs its future on the thread that started it (current-thread runtime), so a
// thread-local reaches the command under test and nothing else.
#[cfg(test)]
thread_local! {
    static TEST_MANIFEST: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point THIS thread's manifest reads at `path` until the returned guard is dropped.
#[cfg(test)]
#[must_use = "the override lasts only as long as the guard"]
pub(crate) fn manifest_for_this_thread(path: impl Into<std::path::PathBuf>) -> TestManifestGuard {
    TEST_MANIFEST.with(|slot| *slot.borrow_mut() = Some(path.into()));
    TestManifestGuard
}

#[cfg(test)]
pub(crate) struct TestManifestGuard;

#[cfg(test)]
impl Drop for TestManifestGuard {
    fn drop(&mut self) {
        TEST_MANIFEST.with(|slot| *slot.borrow_mut() = None);
    }
}

/// The manifest an in-process test falls back to, named by DATA rather than in this source.

/// `manifest/for-tests` holds the file name, one line. Writing it here instead would put a network
/// name back into `crates/**/*.rs`, which is what removes -- and the argument that a test is
/// an exception holds equally well for any other file, so there is no exception.
#[cfg(test)]
pub(crate) fn committed_manifest_for_tests() -> std::path::PathBuf {
    if let Some(path) = TEST_MANIFEST.with(|slot| slot.borrow().clone()) {
        return path;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../manifest");
    let named = std::fs::read_to_string(dir.join("for-tests")).expect(
        "manifest/for-tests must name the manifest this tree's tests run against, one file name \
         on one line",
    );
    let named = named.trim();
    assert!(
        !named.is_empty(),
        "manifest/for-tests is empty, so nothing says which manifest the tests run against"
    );
    dir.join(named)
}

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

/// ONE `--read-timeout` for the whole command, shared by every chain read it makes.

/// `direct_chain_read_with_timeout` bounds ONE read. A command that makes two -- ask the
/// ModelRegistry what the model is called, then read that model's book -- gave each its own full
/// budget, so `--read-timeout 30` could block for 60s against a bound the operator set to 30. A
/// budget the flag does not name is not a bound; it is a multiplier whose factor is however many
/// reads the command happens to make today.

/// WHAT IT COUNTS IS TIME SPENT INSIDE READS, not wall clock. The first shape of this was a
/// deadline taken at the top of the command, and that charged the budget for work that is not a
/// read: `dexdo subscription cancel` waits in `reconcile_existing_subscription_journal` for up to a
/// full `--read-timeout` of its own, and the next read then started with nothing left and refused
/// as a timeout while nothing was hung. A wait for a human, a local file read and a chain WRITE
/// have the same problem. Only `read` spends.

/// This is a real change to what the flag means -- from "each read may hang this long" to "all the
/// reads together may take this long" -- and `args.rs` says so in the flag's own help.
pub(crate) struct ReadBudget {
    total: std::time::Duration,
    /// Nanoseconds already spent inside `read`. Atomic rather than `Cell` because the future this
    /// is borrowed across may be moved between runtime threads.
    spent_nanos: std::sync::atomic::AtomicU64,
}

impl ReadBudget {
    pub(crate) fn new(timeout_secs: u64) -> Self {
        Self {
            total: std::time::Duration::from_secs(timeout_secs),
            spent_nanos: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) async fn read<T>(&self, read: impl Future<Output = Result<T>>) -> Result<T> {
        use std::sync::atomic::Ordering;
        let spent = std::time::Duration::from_nanos(self.spent_nanos.load(Ordering::Relaxed));
        let left = self.total.saturating_sub(spent);
        // `tokio::time::Instant`, not `std`: the runtime's clock is the one the `timeout` below
        // obeys, and under a paused clock the std one does not move at all -- which would make
        // every test of this budget pass by never spending anything.
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(left, read).await;
        self.spent_nanos.fetch_add(
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        match outcome {
            Ok(result) => result,
            // BOTH numbers, and no claim about WHY. A threshold on "was much already spent?" would
            // be a number nobody chose, and saying "the rest was spent by earlier reads" is false
            // when this IS the first read -- which is the commonest timeout of all.
            Err(_) => {
                let left = left.as_secs_f64();
                let total = self.total.as_secs();
                bail!(
                    "chain read timed out after the {left:.1}s it had of this command's {total}s \
                     `--read-timeout` budget; raise `--read-timeout` or use `dexdo market-data` \
                     where applicable"
                )
            }
        }
    }
}

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
    pub(crate) market: Option<&'a dexdo_core::MarketManifest>,
    pub(crate) market_path: Option<&'a std::path::Path>,
    pub(crate) contracts: &'a std::path::Path,
    pub(crate) endpoint: Option<deals::DealEndpointInfo>,
    pub(crate) created_order_ids: Vec<u128>,
}

pub(crate) struct PoolRecoveryInputs {
    pub(crate) note_addr: String,
    pub(crate) note_secret_hex: Zeroizing<String>,
    pub(crate) token_contract: String,
    pub(crate) pool_record: Option<PoolRecoveryRecord>,
}

pub(crate) struct PoolRecoveryRecord {
    pub(crate) pool_path: std::path::PathBuf,
    pub(crate) note_addr: String,
    pub(crate) note_secret_hex: Zeroizing<String>,
    pub(crate) token_contract: String,
    pub(crate) role: String,
}

pub(crate) struct PoolWriteLock {
    path: std::path::PathBuf,
    pool_path: std::path::PathBuf,
    /// The OS advisory lock on the sentinel file, held for exactly as long as the sentinel exists.

    /// The sentinel records a PID together with the OS host name whose process table gives that PID
    /// meaning. A PID still is not evidence by itself: it outlives the process that wrote it and it
    /// is reused. This handle makes same-host liveness observable -- the kernel drops an advisory
    /// lock when the process holding it dies, however it died, including under SIGKILL where no
    /// `Drop` runs. Reclaim therefore requires a same-host identity match before it trusts either
    /// the advisory lock or the PID probe.

    /// `fs2` is the mechanism this client already locks with -- `acquire_seller_pool_lock`
    /// (`crates/dexdo/src/cli/seller.rs`) and the note-deploy wallet lock
    /// (`crates/dexdo/src/cli/note_cmd.rs`) are the same call.
    file: Option<std::fs::File>,
}

impl Drop for PoolWriteLock {
    fn drop(&mut self) {
        // Unlink BEFORE releasing, and this order is load-bearing. A recovery that opened
        // this sentinel while the advisory lock was still held is refused as contended, and one
        // that opens after the unlink finds nothing to reclaim and takes the lock the ordinary way.
        // Releasing first would leave a moment in which the sentinel is present and unlocked: a
        // recovery landing in that moment would claim it, and the next line here would then delete
        // the lock it had just legitimately taken.

        // The fallback is for the platform that refuses to remove a file while it is still open;
        // this handle is opened with the sharing that allows it, so the first attempt is expected
        // to succeed, and the sentinel must not survive an ordinary release either way -- one that
        // did would read to the next run as a crashed holder.
        let unlinked = std::fs::remove_file(&self.path).is_ok();
        self.file.take();
        if !unlinked {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn note_pubkey_id(pk: &dexdo_core::NotePubkey) -> String {
    pk.ed.iter().map(|b| format!("{b:02x}")).collect()
}

fn persist_runtime_deal_handle(
    input: RuntimeDealHandleInput<'_>,
    network: &str,
) -> Result<deals::DealHandle> {
    let market = match input.market {
        Some(market) => Some(market.clone()),
        None => input.market_path.map(load_market).transpose()?,
    };
    let h = deals::DealHandle {
        version: deals::DEAL_HANDLE_VERSION,
        handle: deals::make_handle_id(input.token_contract, input.role),
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
        created_order_ids: input.created_order_ids,
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

pub(crate) fn load_pool_json(path: &std::path::Path) -> Result<Value> {
    let path = crate::cli::note::resolve_private_file_path(path, "DEXDO_PN_POOL")?;
    // the pool carries `owner_secret_key_hex` for EVERY note in it -- the most secret-dense
    // file the client has -- and it was read with no permission check at all. put that check
    // on `--note-key`, `--multisig-private-key` and the rest, and this file, which is worth all of
    // them together, was not among them.
    crate::cli::support::refuse_exposed_secret_file(&path, "DEXDO_PN_POOL")?;
    let bytes = std::fs::read(&path)
        .map_err(|e| anyhow::anyhow!("read DEXDO_PN_POOL {}: {e}", path.display()))?;
    let pool = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("parse DEXDO_PN_POOL {}: {e}", path.display()))?;
    crate::cli::note::ensure_shell_pool_currency(&pool)?;
    Ok(pool)
}

pub(crate) fn validate_existing_pool_if_present(path: &std::path::Path) -> Result<()> {
    // Absent is fine here and the next line says so; exposed is not.
    crate::cli::support::refuse_exposed_secret_file_if_present(path, "--pool")?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => bail!("read --pool {}: {error}", path.display()),
    };
    let pool = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("--pool {} is not valid JSON: {error}", path.display()))?;
    crate::cli::note::ensure_shell_pool_currency(&pool)
}

pub(crate) fn acquire_pool_write_lock(pool_path: &std::path::Path) -> Result<PoolWriteLock> {
    acquire_pool_write_lock_inner(pool_path, true)
}

pub(crate) fn try_acquire_pool_write_lock(pool_path: &std::path::Path) -> Result<PoolWriteLock> {
    acquire_pool_write_lock_inner(pool_path, false)
}

/// The sentinel path for one pool file, and the resolved pool path itself.

/// One place, so the acquiring path and the recovery path can never disagree about which file the
/// lock IS.
fn pool_write_lock_paths(
    pool_path: &std::path::Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let pool_path = crate::cli::note::resolve_private_file_path(pool_path, "DEXDO_PN_POOL")?;
    let mut lock_name = pool_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    Ok((std::path::PathBuf::from(lock_name), pool_path))
}

/// Is this error the platform saying somebody else holds the advisory lock?
fn pool_lock_is_contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        || error.kind() == std::io::ErrorKind::WouldBlock
}

#[derive(serde::Deserialize, serde::Serialize)]
struct PoolWriteLockHolder {
    pid: u32,
    host: String,
}

enum RecordedPoolWriteLockHolder {
    HostAware(PoolWriteLockHolder),
    LegacyPid(u32),
}

fn parse_pool_write_lock_holder(recorded: &str) -> Option<RecordedPoolWriteLockHolder> {
    if let Ok(pid) = recorded.parse::<u32>() {
        return Some(RecordedPoolWriteLockHolder::LegacyPid(pid));
    }
    serde_json::from_str::<PoolWriteLockHolder>(recorded)
        .ok()
        .filter(|holder| !holder.host.is_empty())
        .map(RecordedPoolWriteLockHolder::HostAware)
}

/// The operating system's name for this host.

/// Rust's standard library has no hostname query. The target-specific dependencies already used by
/// this crate expose the native query, so the lock does not need another identity crate or a
/// filesystem-dependent substitute.
fn current_pool_lock_host_identity() -> Result<String> {
    #[cfg(unix)]
    {
        let mut identity = std::mem::MaybeUninit::<libc::utsname>::uninit();
        // SAFETY: uname initializes the caller-owned utsname on success. The return value is
        // checked before the value is assumed initialized.
        if unsafe { libc::uname(identity.as_mut_ptr()) } != 0 {
            bail!(
                "query host identity with uname: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: the successful uname call above initialized every field.
        let identity = unsafe { identity.assume_init() };
        let end = identity
            .nodename
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| {
                anyhow::anyhow!("uname returned a host identity without a terminator")
            })?;
        let bytes = identity.nodename[..end]
            .iter()
            .map(|byte| byte.to_ne_bytes()[0])
            .collect::<Vec<_>>();
        let host = String::from_utf8(bytes)
            .context("uname returned a host identity that is not valid UTF-8")?;
        if host.is_empty() {
            bail!("uname returned an empty host identity");
        }
        Ok(host)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::WindowsProgramming::{
            GetComputerNameW, MAX_COMPUTERNAME_LENGTH,
        };

        let mut identity = vec![0_u16; MAX_COMPUTERNAME_LENGTH as usize + 1];
        let mut length = identity.len() as u32;
        // SAFETY: the buffer is writable for length UTF-16 code units, and length points to an
        // initialized count. GetComputerNameW writes at most that many units and updates the count.
        if unsafe { GetComputerNameW(identity.as_mut_ptr(), &mut length) } == 0 {
            bail!(
                "query host identity with GetComputerNameW: {}",
                std::io::Error::last_os_error()
            );
        }
        let host = String::from_utf16(&identity[..length as usize])
            .context("GetComputerNameW returned an invalid host identity")?;
        if host.is_empty() {
            bail!("GetComputerNameW returned an empty host identity");
        }
        return Ok(host);
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!("this platform has no supported host identity query");
    }
}

fn encode_pool_write_lock_holder(holder: &PoolWriteLockHolder) -> Result<String> {
    serde_json::to_string(holder).context("encode pool lock holder identity")
}

/// Is the process a sentinel recorded still running? `None` when this platform cannot say.

/// SECOND signal, never the first. A PID is reused and it outlives the process that wrote it, so
/// this can never establish on its own that a holder is gone -- [`reclaim_pool_write_lock_if_holder_is_gone`]
/// uses it only to REFUSE, by requiring the advisory lock and this to agree before anything is
/// taken over. What it is here to catch is the one direction the advisory lock alone gets wrong: a
/// sentinel written by a build that did not take the lock carries none, and a live holder of one
/// would otherwise read as gone.

/// This is a local process-table query. A host-aware sentinel reaches it only after its recorded
/// host matches this host; a foreign-host sentinel is refused before either local signal is used.

/// `kill(pid, 0)` is the POSIX existence check -- it performs the permission and existence test and
/// delivers no signal. Its three answers map exactly onto the three this returns, and anything else
/// is `None` rather than a guess.
pub(crate) fn recorded_holder_is_running(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        // 0 and negatives are process GROUP selectors to `kill`, not process ids, and a value past
        // `pid_t` cannot name one at all. None of them is a question this can answer.
        if pid == 0 || pid > i32::MAX as u32 {
            return None;
        }
        // SAFETY: `kill` with signal 0 is a pure existence/permission query on a value already
        // checked to be a representable, positive `pid_t`. It writes nothing, delivers nothing, and
        // borrows nothing.
        let answered = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if answered == 0 {
            return Some(true);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            // No process bears this id.
            Some(libc::ESRCH) => Some(false),
            // One does, and it is not ours to signal. Still running.
            Some(libc::EPERM) => Some(true),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Take over a pool lock whose holder is PROVABLY gone, and only then.

/// `--resume` is the crash-recovery entry point, and a buyer that was SIGKILLed mid-submit left a
/// sentinel behind: `Drop` never ran, nothing else removes one, and every later attempt -- including
/// the recovery attempt -- was refused for the lifetime of the file. The safety property and the
/// recovery path contradicted each other and the safety property won by accident of implementation.

/// What makes taking it over defensible is that the verdict is LIVENESS, not absence:

/// * a host-aware sentinel must name this host. Advisory locks and process tables are local
/// signals, so neither is consulted for a foreign host; the host-identity refusal names the
/// recorded host, this host, and the two signals it cannot trust. A pid-only legacy sentinel
/// cannot make that check and keeps the pre- two-signal fallback for compatibility.
/// * the first signal is the OS advisory lock every holder takes on the sentinel for as long as
/// it holds it. The kernel releases it when the holding process dies, whatever killed it, so a
/// lock that is still held is a holder that is still running, and a live holder is refused -- in
/// those words, rather than as a bare "already held".
/// * an UNLOCKED sentinel is not, by itself, evidence that the holder is gone. A sentinel written
/// by a build that did not take the lock carries none while its holder is alive and well, so
/// lock-absence alone would report that live holder as gone -- the one direction in which this
/// could fail open, and the only one, since every other branch refuses. So the second signal has
/// to agree: the recorded process must ALSO not be running. The PID is still never sufficient --
/// it is reused, and it is not consulted at all until the lock is already free -- but it is
/// necessary, and two independent signals saying "gone" is what a take-over costs.
/// * anything this cannot establish -- a foreign or unreadable host identity, an unreadable or
/// unparseable sentinel, a lock error that is not contention, a platform that cannot answer the
/// liveness question, an unlocked sentinel whose recorded process IS running, or a read-back
/// that does not find our own claim -- is a refusal. Doubt fails closed, in the words of the
/// doubt.

/// Taking the lock over does NOT mean forgetting what it guarded. The caller reclaims it only in
/// order to run the by-fact reconciliation the buyer money journal exists for, before any second
/// submission is allowed; see `raise_pending_buyer_money_before_fresh_reads`.

/// Returns `Ok(None)` when there is no sentinel at all -- nothing to reclaim, and the caller takes
/// the lock the ordinary way.
pub(crate) fn reclaim_pool_write_lock_if_holder_is_gone(
    pool_path: &std::path::Path,
) -> Result<Option<PoolWriteLock>> {
    use std::io::{Read as _, Seek as _};

    let (lock_path, pool_path) = pool_write_lock_paths(pool_path)?;
    match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => bail!("pool lock {} must be a regular file", lock_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("inspect pool lock {}: {e}", lock_path.display()),
    }
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!(
            "cannot establish whether the holder of pool lock {} is still running: opening it \
             failed with {e}",
            lock_path.display()
        ),
    };
    // The recorded holder. Its host decides whether the two local liveness signals below are
    // meaningful; the raw text is also what refusals name so an operator can inspect the claim.
    let recorded = {
        let mut recorded = String::new();
        match file.read_to_string(&mut recorded) {
            Ok(_) => recorded.trim().to_string(),
            Err(_) => String::new(),
        }
    };
    let recorded_holder = parse_pool_write_lock_holder(&recorded);
    let shown = match &recorded_holder {
        Some(RecordedPoolWriteLockHolder::HostAware(holder)) => {
            format!("pid {} on host {:?}", holder.pid, holder.host)
        }
        Some(RecordedPoolWriteLockHolder::LegacyPid(pid)) => format!("pid {pid}"),
        None if recorded.is_empty() => "nothing".to_string(),
        None => format!("unparseable value {recorded:?}"),
    };
    let host_for_claim = match &recorded_holder {
        Some(RecordedPoolWriteLockHolder::HostAware(holder)) => {
            let this_host = match current_pool_lock_host_identity() {
                Ok(host) => host,
                Err(error) => bail!(
                    concat!(
                        "host identity check for pool lock {} saw recorded host {:?}, but reading ",
                        "this host failed with {:#}; it needed the same host before the ",
                        "advisory-lock and PID liveness signals could be trusted, so nothing was ",
                        "reclaimed"
                    ),
                    lock_path.display(),
                    holder.host,
                    error
                ),
            };
            if holder.host != this_host {
                bail!(
                    concat!(
                        "host identity check for pool lock {} saw recorded host {:?}, but this host ",
                        "is {:?}; it needed the same host before the advisory-lock and PID liveness ",
                        "signals could be trusted, so nothing was reclaimed"
                    ),
                    lock_path.display(),
                    holder.host,
                    this_host
                );
            }
            Some(this_host)
        }
        Some(RecordedPoolWriteLockHolder::LegacyPid(_)) | None => None,
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(e) if pool_lock_is_contended(&e) => bail!(
            "pool lock {} is held by a process that is still running (it recorded {shown}); this is \
             a live holder, not a leftover",
            lock_path.display()
        ),
        Err(e) => bail!(
            "cannot establish whether the holder of pool lock {} is still running: locking it \
             failed with {e}",
            lock_path.display()
        ),
    }
    // The lock is free. That is NOT yet a holder that is gone: a sentinel written by a build that
    // did not take the lock carries none while its holder is alive, so the recorded process has to
    // agree before anything is taken over.
    let recorded_pid = match &recorded_holder {
        Some(RecordedPoolWriteLockHolder::HostAware(holder)) => Some(holder.pid),
        Some(RecordedPoolWriteLockHolder::LegacyPid(pid)) => Some(*pid),
        None => None,
    };
    match recorded_pid.and_then(recorded_holder_is_running) {
        Some(false) => {}
        Some(true) => bail!(
            "pool lock {} carries no lock, but the process it records is running ({shown}); an \
             unlocked sentinel is not evidence its holder is gone, so this is undecidable and \
             nothing was reclaimed",
            lock_path.display()
        ),
        None => bail!(
            "cannot establish whether the holder of pool lock {} is still running: it records \
             {shown}, and this platform cannot answer that for it; nothing was reclaimed",
            lock_path.display()
        ),
    }
    // Both signals say the holder is gone. Claim the sentinel as ours by recording this process,
    // then read it back BY PATH: if the file we just locked had already been unlinked by a reclaim
    // that completed between our open and our lock, the read-back finds a different sentinel -- or
    // none -- and we refuse rather than run beside whoever created it.
    let ours = match host_for_claim {
        Some(host) => encode_pool_write_lock_holder(&PoolWriteLockHolder {
            pid: std::process::id(),
            host,
        })?,
        // A pre- sentinel has no host identity to validate. Preserve its pid-only format while
        // applying the exact advisory-lock plus local-pid fallback it was written for.
        None => std::process::id().to_string(),
    };
    let claim = |file: &mut std::fs::File| -> std::io::Result<()> {
        // Rewind before truncating: the read above left the cursor at the end of the old pid, and
        // writing there would leave that many NUL bytes in front of ours.
        file.rewind()?;
        file.set_len(0)?;
        writeln!(file, "{ours}")?;
        file.flush()
    };
    if let Err(e) = claim(&mut file) {
        bail!(
            "claim pool lock {} after its holder was found gone: {e}",
            lock_path.display()
        );
    }
    match std::fs::read_to_string(&lock_path) {
        Ok(observed) if observed.trim() == ours => {}
        Ok(observed) => bail!(
            "pool lock {} was taken by another process while it was being reclaimed (it now \
             records {}); nothing was reclaimed",
            lock_path.display(),
            observed.trim()
        ),
        Err(e) => bail!(
            "cannot confirm the reclaimed pool lock {} is the one now held: {e}",
            lock_path.display()
        ),
    }
    Ok(Some(PoolWriteLock {
        path: lock_path,
        pool_path,
        file: Some(file),
    }))
}

fn acquire_pool_write_lock_inner(pool_path: &std::path::Path, wait: bool) -> Result<PoolWriteLock> {
    let (lock_path, pool_path) = pool_write_lock_paths(pool_path)?;
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
                let holder = current_pool_lock_host_identity().and_then(|host| {
                    encode_pool_write_lock_holder(&PoolWriteLockHolder {
                        pid: std::process::id(),
                        host,
                    })
                });
                let holder = match holder {
                    Ok(holder) => holder,
                    Err(error) => {
                        let _ = std::fs::remove_file(&lock_path);
                        return Err(error.context(format!(
                            "record host identity in pool lock {}",
                            lock_path.display()
                        )));
                    }
                };
                if let Err(e) = writeln!(lock, "{holder}") {
                    let _ = std::fs::remove_file(&lock_path);
                    return Err(anyhow::anyhow!(
                        "write pool lock {}: {e}",
                        lock_path.display()
                    ));
                }
                // the sentinel first binds this PID to this host. Hold the OS advisory
                // lock for as long as it exists, so a same-host recovery can tell THIS holder still
                // running from THIS holder having died without releasing. Nobody else can hold it:
                // the file was created a line ago by an atomic `create_new`.
                if let Err(e) = fs2::FileExt::try_lock_exclusive(&lock) {
                    let _ = std::fs::remove_file(&lock_path);
                    return Err(anyhow::anyhow!(
                        "hold pool lock {}: {e}",
                        lock_path.display()
                    ));
                }
                return Ok(PoolWriteLock {
                    path: lock_path,
                    pool_path,
                    file: Some(lock),
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
                std::thread::sleep(POOL_LOCK_POLL_INTERVAL);
            }
            Err(e) => bail!("create pool lock {}: {e}", lock_path.display()),
        }
    }
}

/// Hold the write lock for one update of a pool-shaped FILE, and let a dead holder's sentinel go.

/// Everything that comes through here is a plain file write -- the pool JSON, the buyer money
/// journal, the buyer subscription state, the pool recovery record, the note-deploy fold -- and every
/// one of them lands atomically (`write_pool_private` -> `write_private_atomic`). So the file a
/// crashed holder leaves behind is whole either way, and its sentinel asserts one thing only:
/// somebody is writing this file RIGHT NOW. It is not the buyer money lock, which asserts that a
/// money submission is awaiting by-fact reconciliation; that one is taken through
/// [`try_acquire_pool_write_lock`] by `BuyerMoneyLock` and stays gated on the recovery entry point,
/// because taking IT over means promising to reconcile first.

/// Here there is nothing to reconcile, so a same-host holder both local signals say is gone is
/// simply not a holder, and no command has to earn the right to say so. That is why this is not
/// gated on `--resume`: a dead mutex wedges every writer, and gating would leave a fresh buy,
/// `note deploy` and `recover` refused forever by a process that no longer exists.

/// It is what still stood in front of `--resume` after the money lock learned to reclaim.
/// `run_buyer_inner`'s FIRST statement is `preflight_buyer_pool_for_money_move`, which arrives here
/// on the pool file ahead of every reclaim seam the money lock has; and `write_buyer_submit_journal`
/// arrives here from inside the money-lock hold, behind them. A buyer SIGKILLed mid-submit holds the
/// money lock and is inside one of those writes, so it leaves both sentinels -- and either one alone
/// refused recovery for the lifetime of the file.

/// The proof is [`reclaim_pool_write_lock_if_holder_is_gone`]'s and it is not weakened to reuse it:
/// a foreign host is refused before local signals are trusted, a held same-host advisory lock is a
/// live holder, an unlocked sentinel whose recorded process is still running is undecidable, and
/// anything that cannot be established is a refusal. A refusal does not fail the update -- it falls
/// through to the ordinary acquire, which waits for a live holder exactly as it did before and names
/// what could not be established if that wait runs out. So doubt always lands on the unchanged
/// behaviour and never on a take-over: the check errs towards leaving the
/// sentinel alone, which costs an operator-visible stall, rather than towards taking it, which would
/// cost two writers on one money-adjacent file.
pub(crate) fn with_pool_write_lock<T>(
    pool_path: &std::path::Path,
    update: impl FnOnce(&std::path::Path) -> Result<T>,
) -> Result<T> {
    let lock = match reclaim_pool_write_lock_if_holder_is_gone(pool_path) {
        Ok(Some(reclaimed)) => {
            eprintln!(
                "pool_write_lock_reclaimed {} was left behind by a process that is no longer \
                 running; taken over for this write",
                reclaimed.path.display()
            );
            reclaimed
        }
        // No sentinel: nothing to reclaim, and the ordinary rules apply.
        Ok(None) => acquire_pool_write_lock(pool_path)?,
        // Not proof the holder is gone. Wait for it exactly as before, and if that wait runs out,
        // say what could not be established rather than reporting a live writer.
        Err(undecided) => acquire_pool_write_lock(pool_path)
            .map_err(|waited| waited.context(format!("{undecided:#}")))?,
    };
    update(&lock.pool_path)
}

/// Where this instance's note pool is, for the commands that READ it.

/// **The defect this closes.** The last arm was `None`, and the client therefore wrote a
/// pool it could not then find. `note deploy` and `note recover` take their target from
/// `data_dir::automatic_private_file` when `--pool` is absent (`main.rs`), which resolves through
/// `data_dir::effective()` and lands in the platform data directory. The readers stopped at
/// `data_dir::explicit()`, which is `Some` only when `--data-dir` was passed. So on a default
/// install the money was on disk, at the right path, owner-only -- and `dexdo note list` answered
/// "this instance has deployed no notes yet" about a pool written minutes earlier. That sentence was
/// not a limitation, it was false, and `recover`, `dispute`, `reclaim` and the note picker were
/// equally blind.

/// **Why the new arm is LAST rather than a swap.** Replacing `explicit()` with `effective()` was the
/// obvious repair and it is wrong: `effective()` always resolves, so `DEXDO_PN_POOL` below would
/// become unreachable. The variable is the READER's pointer by design -- the writer never consults
/// it, and `note_deploy_same_file_pool_guard` refuses outright when it and `--pool` name the same
/// existing file, because appending there "can hide note-key confusion". Making the variable
/// unreachable would be a silent break on a money path, which is worse than the defect being fixed.

/// So nothing that resolves today changes what it resolves to; only the case that resolved to
/// NOTHING now resolves, and it resolves to the writer's own location by the writer's own call.

/// **The one case still answered as `None`:** a platform data directory that cannot be determined at
/// all -- no `$HOME`, or an OS `directories` does not know. That is what the code does today too, and
/// the alternative is changing this function's return type across its nine callers to improve a
/// message for a case none of them can reach in practice.
pub(crate) fn note_pool_path(explicit: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(root) = crate::cli::data_dir::explicit() {
        return Some(root.join(DEFAULT_PN_POOL_PATH));
    }
    if let Some(raw) = std::env::var_os("DEXDO_PN_POOL") {
        if !raw.is_empty() {
            return Some(std::path::PathBuf::from(raw));
        }
    }
    // ONLY IF IT IS THERE, and the asymmetry with the arms above is deliberate.

    // An explicit `--pool`, `--data-dir` or `DEXDO_PN_POOL` is returned whether or not the file
    // exists: the operator named that path, and a typo in it has to be reported against it rather
    // than swallowed. Nobody named this one. A default that is not on disk means this instance has
    // not deployed a note, which is exactly what the callers' own refusals say -- and returning the
    // path anyway made them say something else: `note list` answered "resolve parent directory for
    // DEXDO_PN_POOL <path>: No such file or directory", naming a variable the operator never set.
    // Caught by `a_home_with_no_pool_still_says_so`, which is why it is in the suite.
    let shipped = crate::cli::data_dir::effective()
        .ok()?
        .join(DEFAULT_PN_POOL_PATH);
    shipped.is_file().then_some(shipped)
}

/// The explicitly supplied recovery identity, normalized once for both the single-target resolver and
/// the multi-target plan: `(--note-addr, --token-contract/--market)`.
fn explicit_recovery_identity(
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
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
    Ok((explicit_note_addr, explicit_tc))
}

/// `--note-addr` + `--note-key` + `--token-contract/--market` fully determine one deal; the pool is not
/// read at all in that case. Returns the signing identity of that deal: `(note, owner key, TC)`.
fn fully_explicit_recovery_identity(
    identity: &RecoveryIdentityArgs,
    explicit_note_addr: Option<&String>,
    explicit_tc: Option<&String>,
) -> Result<Option<(String, Zeroizing<String>, String)>> {
    let (Some(note_addr), Some(note_key), Some(tc)) = (
        explicit_note_addr,
        identity.note_key.as_deref(),
        explicit_tc,
    ) else {
        return Ok(None);
    };
    Ok(Some((
        note_addr.clone(),
        read_secret_hex(note_key, "--note-key")?.into(),
        tc.clone(),
    )))
}

/// The owner key a recovery signs with: an explicit `--note-key` overrides the key recorded next to the
/// entry. Exactly one copy of the secret is produced, and it is moved, never cloned.
fn recovery_note_secret(
    identity: &RecoveryIdentityArgs,
    recorded: String,
) -> Result<Zeroizing<String>> {
    match identity.note_key.as_deref() {
        Some(path) => Ok(read_secret_hex(path, "--note-key")?.into()),
        None => Ok(recorded.into()),
    }
}

/// Every pool note entry that records a `token_contract` and matches the explicitly supplied filters --
/// **every role**, including `seller`. Role selection belongs to the caller: a seller row for the same
/// note and TokenContract as a buyer row is a same-deal contradiction that a buyer-side plan must be able
/// to see, while a seller row for some other deal is simply not part of that plan.
fn matching_pool_recovery_records(
    command: &str,
    pool: Option<&std::path::Path>,
    explicit_note_addr: Option<&String>,
    explicit_tc: Option<&String>,
) -> Result<(
    std::path::PathBuf,
    Vec<crate::cli::note::PoolNoteRecoveryRecord>,
)> {
    let Some(pool_path) = note_pool_path(pool) else {
        bail!(
            "{command}: pass --note-addr, --note-key, and --token-contract/--market, or pass --pool / set \
             DEXDO_PN_POOL containing this note entry with token_contract recovery metadata"
        );
    };
    let pool_path = crate::cli::note::resolve_private_file_path(&pool_path, "DEXDO_PN_POOL")?;
    let pool = load_pool_json(&pool_path)?;
    let records = crate::cli::note::pool_note_recovery_records(&pool)?
        .into_iter()
        .filter(|record| {
            explicit_note_addr.is_none_or(|want| *want == record.note_addr)
                && explicit_tc.is_none_or(|want| *want == record.token_contract)
        })
        .collect::<Vec<_>>();
    Ok((pool_path, records))
}

/// the pool records several recoverable deals and `recover`/`dispute` act on exactly one.

/// Returned as a typed error instead of a bare message so the caller can resolve the choice from chain
/// facts and then ask for that one deal **by name**, through the same resolver, rather than opening a
/// second path into the money. It carries addresses only -- the recorded owner keys stay inside the
/// resolver, so nothing that decides *which* deal is acted on has ever seen one.

/// `Display` is the unchanged refusal an operator sees when nothing resolves the choice.
#[derive(Debug)]
pub(crate) struct AmbiguousRecoveryDeals {
    message: String,
    /// The pool file these deals were read from, already resolved through any symlink.
    pub(crate) pool: std::path::PathBuf,
    /// Every selectable deal as `(note address, TokenContract address)`, in the plan's recorded order.
    pub(crate) deals: Vec<(String, String)>,
}

impl std::fmt::Display for AmbiguousRecoveryDeals {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AmbiguousRecoveryDeals {}

/// Resolve the one deal `dispute` acts on. `dispute` persists nothing back into the pool.
pub(crate) fn resolve_pool_recovery_inputs(
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
) -> Result<PoolRecoveryInputs> {
    resolve_recovery_inputs(
        "dispute",
        identity,
        market,
        token_contract,
        pool,
        false,
        None,
    )
}

/// the same resolution, narrowed to the one recorded deal the caller has proved from the chain is
/// the one this invocation acts on.
pub(crate) fn resolve_pool_recovery_inputs_for_deal(
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
    deal: &(String, String),
) -> Result<PoolRecoveryInputs> {
    resolve_recovery_inputs(
        "dispute",
        identity,
        market,
        token_contract,
        pool,
        false,
        Some(deal),
    )
}

/// Resolve the one deal `recover` acts on. `recover` writes the resolved buyer record back into the pool
/// after its STOP and is the only consumer of `PoolRecoveryInputs::pool_record`.
pub(crate) fn resolve_persistable_pool_recovery_inputs(
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
) -> Result<PoolRecoveryInputs> {
    resolve_recovery_inputs(
        "recover",
        identity,
        market,
        token_contract,
        pool,
        true,
        None,
    )
}

/// the same resolution, narrowed to the one recorded deal the caller has proved from the chain is
/// the one this invocation acts on. The pool record `recover` writes back is still built only from what
/// the pool itself recorded, exactly as it is for a pool holding a single deal.
pub(crate) fn resolve_persistable_pool_recovery_inputs_for_deal(
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
    deal: &(String, String),
) -> Result<PoolRecoveryInputs> {
    resolve_recovery_inputs(
        "recover",
        identity,
        market,
        token_contract,
        pool,
        true,
        Some(deal),
    )
}

/// Resolve the ONE deal `recover`/`dispute` acts on out of everything the pool records.

/// the selection is made from [`pool_recovery_plan`] -- the same primitive `reclaim` drives --
/// rather than from the raw row list, because the raw row count is not the number of deals. Rows that
/// agree on every recorded fact describe ONE deal recorded twice, and the plan collapses them; the
/// row count did not, so a pool holding a duplicated entry refused with `pass --note-addr or
/// --token-contract to disambiguate` while both rows carried the SAME note and the SAME
/// TokenContract. Neither flag can separate values that are equal, so the advice could not be
/// followed and that deal's escrow had no way out short of hand-copying the owner key out of the pool
/// file. Failing closed on a genuine ambiguity is right; refusing where there was none, with no
/// alternative, is money stranded by the client rather than by the chain.

/// What did NOT change is the money decision. One invocation still acts on one deal, and fanning a
/// buyer STOP or a bond-locking dispute across every recorded deal remains a different decision that
/// belongs to whoever asks for it.

/// follow-up: with several deals and `selected == None` this still REFUSES, returning
/// [`AmbiguousRecoveryDeals`] naming each deal so `--token-contract` can select it. `selected` is that
/// same one-deal decision made for the operator, and only where the chain itself proved it: the caller
/// reads every recorded deal and comes back naming the one the chain places in the state the command
/// acts on. It re-enters here rather than driving the plan directly, so the recorded owner key, the
/// contradiction rules and the record `recover` persists all stay in one place. The plan is re-read
/// rather than trusted from the caller's first read: a pool that changed in between, or a choice that
/// is no longer exactly one recorded deal, is refused.
fn resolve_recovery_inputs(
    command: &str,
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
    persists_pool_record: bool,
    selected: Option<&(String, String)>,
) -> Result<PoolRecoveryInputs> {
    let mut plan = pool_recovery_plan(command, identity, market, token_contract, pool)?;
    // the plan no longer refuses a pool that records no recoverable deal, because for `reclaim`
    // -- which drives every recorded deal -- that is an ordinary outcome. These two commands act on ONE
    // deal, so zero deals is still the refusal it has always been, in the same words, before anything
    // else is decided: `selected` and the ambiguity report below both describe a non-empty pool.
    if plan.targets.is_empty() && plan.refused.is_empty() {
        bail!(
            "{command}: DEXDO_PN_POOL {} has no matching note entry with token_contract recovery metadata; \
             run the buyer once with this pool active, or pass explicit --note-addr/--note-key/--token-contract",
            plan.pool_path
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("-"))
                .display()
        );
    }
    if let Some((want_note, want_tc)) = selected {
        plan.targets
            .retain(|target| &target.note_addr == want_note && &target.token_contract == want_tc);
        if plan.targets.len() != 1 {
            let want_note = dexdo_core::address::display(want_note);
            let want_tc = dexdo_core::address::display_self_dapp(want_tc);
            bail!(
                "{command}: DEXDO_PN_POOL {} no longer records exactly one recoverable deal for note \
                 {want_note} and TokenContract {want_tc} ({} found); nothing was submitted",
                plan.pool_path
                    .as_deref()
                    .unwrap_or_else(|| std::path::Path::new("-"))
                    .display(),
                plan.targets.len()
            );
        }
    }
    if plan.targets.len() > 1 {
        // Every selectable deal is named, so the operator picks one instead of being told to narrow
        // a set they cannot see. Addresses only: a target also holds the recorded owner key.
        let choices = plan
            .targets
            .iter()
            .map(|target| {
                format!(
                    "\n  --note-addr {} --token-contract {}",
                    dexdo_core::address::display(&target.note_addr),
                    dexdo_core::address::display_self_dapp(&target.token_contract)
                )
            })
            .collect::<String>();
        let pool_path = plan
            .pool_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("-"));
        return Err(anyhow::Error::new(AmbiguousRecoveryDeals {
            message: format!(
                "{command}: DEXDO_PN_POOL {} records {} recoverable deals and {command} acts on one; \
                 pass --note-addr and/or --token-contract to disambiguate, naming exactly one of:{choices}",
                pool_path.display(),
                plan.targets.len()
            ),
            deals: plan
                .targets
                .iter()
                .map(|target| (target.note_addr.clone(), target.token_contract.clone()))
                .collect(),
            pool: pool_path,
        }));
    }
    let Some(target) = plan.targets.pop() else {
        // The plan itself refuses an empty pool, so reaching here means every recorded deal was
        // contradicted. Each contradiction says which entry and why, which is what the operator needs
        // to repair the pool -- a bare count never was.
        bail!(
            "{command}: DEXDO_PN_POOL {} records no recoverable deal; every matching entry was refused:\n  {}",
            plan.pool_path
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("-"))
                .display(),
            plan.refused
                .iter()
                .map(|refusal| refusal.reason.clone())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    };
    // One deal is selected, but a contradicted sibling is still the operator's money and stays
    // unrecoverable until the pool is fixed. Saying so here is the only report it gets: this command
    // is about to act on a different deal and will otherwise look like a clean success.
    for refusal in &plan.refused {
        eprintln!("{command}: skipped recovery entry: {}", refusal.reason);
    }
    // Built only for the caller that persists it, and only when the whole identity came from the pool:
    // no other path carries a second copy of the recorded owner key.
    let pool_record = (persists_pool_record
        && identity.note_addr.is_none()
        && identity.note_key.is_none()
        && market.is_none()
        && token_contract.is_none())
    .then(|| {
        plan.pool_path.map(|pool_path| PoolRecoveryRecord {
            pool_path,
            note_addr: target.note_addr.clone(),
            note_secret_hex: target.note_secret_hex.clone(),
            token_contract: target.token_contract.clone(),
            role: target.role.clone(),
        })
    })
    .flatten();
    Ok(PoolRecoveryInputs {
        note_addr: target.note_addr,
        note_secret_hex: target.note_secret_hex,
        token_contract: target.token_contract,
        pool_record,
    })
}

/// One deal a pool-only recovery may act on: exactly the facts the driver signs and decides with, and
/// nothing else. A target still carries no `PoolRecoveryRecord` -- the recorded owner key is held once,
/// by the consumer that actually reads it, and a persistence record is built by the one caller that
/// persists rather than handed to every caller that drives.
pub(crate) struct PoolRecoveryTarget {
    pub(crate) note_addr: String,
    pub(crate) note_secret_hex: Zeroizing<String>,
    pub(crate) token_contract: String,
    /// The entry's recorded `token_contract_updated_at_unix`, never the reader's clock.
    pub(crate) recorded_at_unix: Option<u64>,
    /// The entry's recorded `token_contract_role` (`buyer` or `unknown`; a coherent `seller` row is
    /// never a buyer-side target). Carried because `recover` must name the row it is writing back to
    /// (`persist_pool_recovery_record_locked` matches on role), and re-reading the pool to recover a
    /// fact this plan already read is how a resolved record and a persisted one drift apart.
    pub(crate) role: String,
}

/// A recorded entry the plan refuses to act on because the pool's own records contradict each other.
pub(crate) struct PoolRecoveryRefusal {
    pub(crate) note_addr: String,
    pub(crate) token_contract: String,
    pub(crate) reason: String,
}

/// Every deal a pool-only recovery can drive, in a deterministic order, plus the entries it refuses.
pub(crate) struct PoolRecoveryPlan {
    pub(crate) targets: Vec<PoolRecoveryTarget>,
    pub(crate) refused: Vec<PoolRecoveryRefusal>,
    /// The pool file these targets were read from, already resolved through any symlink, or
    /// `None` when the identity was given in full on the command line and no pool was read at all.
    pub(crate) pool_path: Option<std::path::PathBuf>,
}

/// plan a recovery from recorded pool metadata alone. Where
/// [`resolve_pool_recovery_inputs`] resolves exactly one deal and refuses as soon as the pool holds a
/// second recoverable entry, this returns **all** recorded deals so the caller can drive each of them as
/// its own individually idempotent action -- an ordinary pool holds one entry per deal the note took part
/// in, and after a crash the pool file is all the operator has.

/// Money-safety rules enforced here, before any chain contact:
/// * exactly one planned target per recorded deal -- rows for the same note and TokenContract collapse
/// only when **every** recorded fact agrees (owner key, role and recorded time); a row that agrees on
/// the deal but disagrees on any of them -- including one row calling the note the buyer and another
/// calling it the seller -- is a contradiction, not a duplicate, and is refused with it;
/// * a note whose records contradict each other (the same note claiming two different TokenContracts) is
/// refused outright, and so is a TokenContract claimed by more than one note. These are counted over
/// the **complete** buyer-side candidate set, contradicted deals included, so a contradiction refuses
/// every deal it touches instead of quietly clearing the way for its own sibling;
/// * a recorded `seller` deal is not a buyer-side candidate at all: a note that sold one deal and bought
/// another is ordinary, and its seller record neither joins nor blocks the buyer plan;
/// * the order is taken from the recorded `token_contract_updated_at_unix` (entries with a recorded time
/// first, earliest first; entries without one last), tie-broken by the recorded note/TokenContract
/// addresses -- never from the reader's wall clock and never from the entry's position in the file, so
/// permuting the pool file cannot change what runs or in which order.
pub(crate) fn resolve_pool_recovery_plan(
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
) -> Result<PoolRecoveryPlan> {
    pool_recovery_plan("reclaim", identity, market, token_contract, pool)
}

/// The plan itself, named by the command whose messages it will appear in.

/// `reclaim` drives every target. `recover` and `dispute` act on ONE -- and this function does not pick
/// it for them: given several targets they REFUSE, and act only if the caller comes back naming the one
/// deal the chain proved is the only one they can act on (`resolve_recovery_inputs`'s `selected`,
/// ). Two live deals, or a chain that cannot answer for one of them, still refuse. Both commands
/// read the pool through this one function, so what counts as a deal, what counts as a contradiction
/// and what order entries come back in cannot diverge between them.
fn pool_recovery_plan(
    command: &str,
    identity: &RecoveryIdentityArgs,
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    pool: Option<&std::path::Path>,
) -> Result<PoolRecoveryPlan> {
    let (explicit_note_addr, explicit_tc) =
        explicit_recovery_identity(identity, market, token_contract)?;
    if let Some((note_addr, note_secret_hex, token_contract)) = fully_explicit_recovery_identity(
        identity,
        explicit_note_addr.as_ref(),
        explicit_tc.as_ref(),
    )? {
        return Ok(PoolRecoveryPlan {
            targets: vec![PoolRecoveryTarget {
                note_addr,
                note_secret_hex,
                token_contract,
                recorded_at_unix: None,
                // Nothing was read from a pool, so there is no recorded row to write back to.
                role: String::new(),
            }],
            refused: Vec::new(),
            pool_path: None,
        });
    }
    let (pool_path, records) = matching_pool_recovery_records(
        command,
        pool,
        explicit_note_addr.as_ref(),
        explicit_tc.as_ref(),
    )?;

    // One candidate per recorded deal, in a key order fixed by the recorded addresses. A candidate is
    // either coherent (one agreed row) or contradicted (rows that disagree); both are candidates, so a
    // contradiction is still counted when deciding whether some other deal is ambiguous.
    let mut by_deal: std::collections::BTreeMap<
        (String, String),
        Vec<crate::cli::note::PoolNoteRecoveryRecord>,
    > = std::collections::BTreeMap::new();
    for record in records {
        by_deal
            .entry((record.note_addr.clone(), record.token_contract.clone()))
            .or_default()
            .push(record);
    }
    let mut candidates: Vec<(crate::cli::note::PoolNoteRecoveryRecord, Option<String>)> =
        Vec::new();
    for ((_, token_contract), rows) in by_deal {
        let first = &rows[0];
        let disagreeing = rows
            .iter()
            .filter(|row| {
                row.owner_secret_hex != first.owner_secret_hex
                    || row.role != first.role
                    || row.recorded_at_unix != first.recorded_at_unix
            })
            .count();
        if disagreeing == 0 {
            // A coherent `seller` record is this note's own sold deal, not a buyer recovery entry.
            if first.role == "seller" {
                continue;
            }
            candidates.push((rows.into_iter().next().expect("group is never empty"), None));
            continue;
        }
        // A contradicted deal is a buyer-side concern only if some row claims the buyer side.
        if !rows
            .iter()
            .any(|row| row.role == "buyer" || row.role == "unknown")
        {
            continue;
        }
        let reason = format!(
            "DEXDO_PN_POOL {} holds {} rows for TokenContract {} whose recorded facts \
             disagree (owner key, role or recorded time); refusing to guess which row is the deal -- fix \
             the pool or pass explicit --note-addr/--note-key/--token-contract",
            pool_path.display(),
            rows.len(),
            dexdo_core::address::display_self_dapp(&token_contract),
        );
        candidates.push((
            rows.into_iter().next().expect("group is never empty"),
            Some(reason),
        ));
    }
    // an empty candidate set is NOT decided here. `reclaim` drives every recorded deal, so a
    // pool that records none is a complete, ordinary answer for it -- only the buyer client ever writes
    // `token_contract` into a pool entry, so a pool whose notes never bought is empty by construction.
    // `recover`/`dispute` act on exactly ONE deal, so for them zero deals is still a refusal, and they
    // raise it themselves in `resolve_recovery_inputs` with the message they have always raised.

    // Except when the caller NAMED what it wanted. `--note-addr`/`--token-contract` are a filter, and
    // `matching_pool_recovery_records` applies it by dropping every row that misses -- so a filter that
    // matched nothing arrives here as the same empty set as a pool that records nothing at all. The
    // two are different answers to different questions: "reclaim everything recorded here" and nothing
    // recorded is a complete answer, while "reclaim deal X" and no X is "X is not recorded here", and
    // reporting that as an ordinary empty plan is how `reclaim` would exit 0 on a deal it never
    // touched. Decided here rather than in `resolve_recovery_inputs` because `reclaim` reaches this
    // function through `resolve_pool_recovery_plan` and never enters that one.
    if candidates.is_empty() {
        let named = match (explicit_note_addr.as_deref(), explicit_tc.as_deref()) {
            (Some(note_addr), Some(token_contract)) => Some(format!(
                "--note-addr {note_addr} and --token-contract {token_contract}"
            )),
            (Some(note_addr), None) => Some(format!("--note-addr {note_addr}")),
            (None, Some(token_contract)) => Some(format!("--token-contract {token_contract}")),
            // No filter was given, so the empty set is the answer and not a miss. Matched exhaustively
            // rather than guarded by `is_some()` so this stays total without a panicking arm.
            (None, None) => None,
        };
        if let Some(named) = named {
            bail!(
                "{command}: DEXDO_PN_POOL {} records no deal matching {named}; that deal was asked for \
                 by name, so there is nothing here to act on and no plan was made. Check the address \
                 against the pool, or drop the filter to drive every deal this pool records",
                pool_path.display()
            );
        }
    }
    if candidates.len() > 1 && identity.note_key.is_some() {
        bail!(
            "{command}: DEXDO_PN_POOL {} has {} matching recovery entries and --note-key names a single \
             note's owner key; pass --note-addr to select that entry, or drop --note-key so each entry is \
             driven with its own recorded owner key",
            pool_path.display(),
            candidates.len()
        );
    }
    // Counted over the complete candidate set -- contradicted deals included -- and over addresses only,
    // so the recorded owner key is never copied to decide admissibility.
    let mut deals_per_note: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut notes_per_tc: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (record, _) in &candidates {
        *deals_per_note.entry(record.note_addr.clone()).or_default() += 1;
        *notes_per_tc
            .entry(record.token_contract.clone())
            .or_default() += 1;
    }

    let mut refused = Vec::new();
    let mut targets = Vec::new();
    for (record, contradiction) in candidates {
        let deals_for_note = deals_per_note[&record.note_addr];
        let notes_for_tc = notes_per_tc[&record.token_contract];
        if let Some(reason) = contradiction {
            refused.push(PoolRecoveryRefusal {
                reason,
                note_addr: record.note_addr,
                token_contract: record.token_contract,
            });
            continue;
        }
        if deals_for_note > 1 {
            refused.push(PoolRecoveryRefusal {
                reason: format!(
                    "DEXDO_PN_POOL {} holds {deals_for_note} contradictory recovery records for note {}; \
                     refusing to act on any of them -- fix the pool or pass explicit \
                     --note-addr/--note-key/--token-contract for the intended deal",
                    pool_path.display(),
                    dexdo_core::address::display(&record.note_addr)
                ),
                note_addr: record.note_addr,
                token_contract: record.token_contract,
            });
            continue;
        }
        if notes_for_tc > 1 {
            refused.push(PoolRecoveryRefusal {
                reason: format!(
                    "DEXDO_PN_POOL {} has {notes_for_tc} different notes recorded as the buyer of \
                     TokenContract {}; refusing to act on a contradictory record -- fix the pool or pass \
                     explicit --note-addr/--note-key/--token-contract for the intended deal",
                    pool_path.display(),
                    dexdo_core::address::display_self_dapp(&record.token_contract)
                ),
                note_addr: record.note_addr,
                token_contract: record.token_contract,
            });
            continue;
        }
        targets.push(PoolRecoveryTarget {
            note_secret_hex: recovery_note_secret(identity, record.owner_secret_hex)?,
            note_addr: explicit_note_addr.clone().unwrap_or(record.note_addr),
            token_contract: explicit_tc.clone().unwrap_or(record.token_contract),
            recorded_at_unix: record.recorded_at_unix,
            role: record.role,
        });
    }
    targets.sort_by(|left, right| plan_order_key(left).cmp(&plan_order_key(right)));
    refused.sort_by(|left, right| {
        (&left.note_addr, &left.token_contract).cmp(&(&right.note_addr, &right.token_contract))
    });
    Ok(PoolRecoveryPlan {
        targets,
        refused,
        pool_path: Some(pool_path),
    })
}

/// The total order a plan runs in, made of recorded facts only: entries carrying a recorded time come
/// first and earliest first, entries without one come last, and both are tie-broken by the recorded
/// note and TokenContract addresses. Nothing here can vary with the pool file's row order.
fn plan_order_key(target: &PoolRecoveryTarget) -> (bool, u64, &String, &String) {
    (
        target.recorded_at_unix.is_none(),
        target.recorded_at_unix.unwrap_or(0),
        &target.note_addr,
        &target.token_contract,
    )
}

pub(crate) fn persist_pool_recovery_record(record: &PoolRecoveryRecord) -> Result<()> {
    with_pool_write_lock(&record.pool_path, |_| {
        persist_pool_recovery_record_locked(record)
    })
}

fn persist_pool_recovery_record_locked(record: &PoolRecoveryRecord) -> Result<()> {
    let mut pool = load_pool_json(&record.pool_path)?;
    crate::cli::note::remove_legacy_pool_funding_multisig(&mut pool);
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
            dexdo_core::address::display(&record.note_addr),
            dexdo_core::address::display_self_dapp(&record.token_contract)
        );
    }
    if conflicting_buyer_record {
        bail!(
            "recover: DEXDO_PN_POOL {} contains a different buyer recovery record for note {}; refusing to clobber or create an ambiguous record",
            record.pool_path.display(),
            dexdo_core::address::display(&record.note_addr)
        );
    }
    let note = &mut notes[matched[0]];
    // Canonical `<dapp_id>::<account_id>`, the one spelling the pool records -- and
    // taken from the ENTRY, not from `record.note_addr`. That field is the workchain form the
    // comparison above runs on; writing it back downgraded an entry `note deploy` had already
    // written canonically, and canonicalising it instead would re-scope an entry recorded under
    // another DApp, because the comparison drops the DApp half to match.

    // `expect`, not a fallback: the loop above skips every entry whose `address` is not a string,
    // so `matched[0]` always indexes one that is. A fallback to `record.note_addr` would be dead
    // code that reads as if the workchain form could be written back here -- the exact downgrade
    // this comment says must not happen.
    let recorded = note["address"]
        .as_str()
        .expect("the matching loop skips every entry whose address is not a string")
        .to_string();
    note["address"] = json!(crate::cli::note::pool_note_address_as_recorded(&recorded));
    // The per-deal TokenContract is a self-DApp account, and this file already prints it that way
    // in the refusals a few lines up. One entry must not hold two address conventions.

    // From the ENTRY where it has one, for the same reason as the address: `record.token_contract`
    // has been through `normalize_wallet_address`, which drops the DApp half, so rendering it would
    // re-scope a TokenContract recorded under another DApp.
    // `expect` for the same reason as the address above: the matching loop required this field to
    // be a string that normalises equal to `record.token_contract`, so a fallback would be dead
    // code -- and it would read as licence to write the normalised workchain form back here, which
    // is the downgrade the comment two lines up forbids.
    let recorded_tc = note["token_contract"]
        .as_str()
        .expect("the matching loop required a string token_contract on this entry")
        .to_string();
    note["token_contract"] = json!(dexdo_core::address::display_self_dapp(&recorded_tc));
    note["token_contract_role"] = json!("buyer");
    note["token_contract_updated_at_unix"] = json!(unix_now_secs());
    let bytes = serde_json::to_vec_pretty(&pool)?;
    write_pool_private(&record.pool_path, &bytes)
}

pub(crate) fn is_note_deploy_wallet_busy_error(error: &anyhow::Error) -> bool {
    let msg = error.to_string().to_ascii_lowercase();
    let norm = msg.replace(['_', '=', ':', '-'], " ");
    let exit_code_52 = norm.split("exit code").skip(1).any(|suffix| {
        suffix
            .trim_start()
            .split(|character: char| !character.is_ascii_digit())
            .next()
            .and_then(|code| code.parse::<u32>().ok())
            == Some(52)
    });
    // bare `tvm_error` is not a busy signal; matching it masked real
    // deployPrivateNote reverts behind retries that never surfaced the cause.
    !msg.contains("rootpn.deployprivatenote")
        && (norm.contains("replay protection")
            || exit_code_52
            || norm.contains("nonce")
            || norm.contains("seqno"))
}

fn is_note_deploy_history_proof_expired_error(error: &anyhow::Error) -> bool {
    crate::cli::note_cmd::note_deploy_has_exact_finalized_rootpn_exit_code(error, 403)
}

/// `dex::ERR_INVALID_ZKPROOF` (137) finalized by RootPN, which is the OPPOSITE of the 403 next to it.

/// 403 is a race against the node's history window and is answered by proving again; 137 says the
/// proof's public inputs disagree with the `value`/`tokenType` RootPN was handed, which no retry can
/// change. Both arrive as "the submit failed", and found them sharing one outcome -- so they are
/// separated here by the exact finalized exit code, never by matching text.
fn is_note_deploy_zk_public_input_mismatch_error(error: &anyhow::Error) -> bool {
    crate::cli::note_cmd::note_deploy_has_exact_finalized_rootpn_exit_code(error, 137)
}

pub(crate) fn note_deploy_error(
    funding_multisig_address: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let funding_multisig_address = dexdo_core::address::display_self_dapp(funding_multisig_address);
    if is_note_deploy_history_proof_expired_error(&error) {
        anyhow::anyhow!(
            "note deploy failed: history proof expired (exit 403). The same paid voucher recovery is preserved; \
             re-run the same `dexdo note deploy` command with its `--recovery` file later \
             (action=resume_same_paid_voucher_later). \
             Do not fund a new voucher. Raw error: deploy PrivateNote from wallet \
             {funding_multisig_address}: {error}"
        )
    } else if is_note_deploy_zk_public_input_mismatch_error(&error) {
        anyhow::anyhow!(
            "note deploy failed: the proof's public inputs do not match what RootPN was given \
             (exit 137, dex::ERR_INVALID_ZKPROOF). This is NOT the 403 history-proof race: proving \
             the same voucher again, on any history layer, reverts identically \
             (action=do_not_retry_this_voucher). Raw error: deploy PrivateNote from wallet \
             {funding_multisig_address}: {error}"
        )
    } else if is_note_deploy_wallet_busy_error(&error) {
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

pub(crate) async fn preload_model_registry_policy(
    role: RegistryRole,
    policy: Option<&RegistryValidationPolicy>,
    contracts: &std::path::Path,
) -> Result<()> {
    preload_model_registry_policy_with_endpoint(role, policy, contracts, None).await
}

pub(crate) async fn preload_model_registry_policy_with_endpoint(
    role: RegistryRole,
    policy: Option<&RegistryValidationPolicy>,
    contracts: &std::path::Path,
    endpoint: Option<&str>,
) -> Result<()> {
    let Some(policy) = policy else {
        return Ok(());
    };
    ChainModelRegistryReader::from_manifest_with_endpoint(
        contracts,
        endpoint,
        policy.required_address(role)?,
    )?
    .read_account_once()
    .await
}

pub(crate) async fn preload_default_model_registry(contracts: &std::path::Path) -> Result<()> {
    preload_default_model_registry_with_endpoint(contracts, None).await
}

pub(crate) async fn preload_default_model_registry_with_endpoint(
    contracts: &std::path::Path,
    endpoint: Option<&str>,
) -> Result<()> {
    let registry_address = default_model_registry_address(contracts)?;
    ChainModelRegistryReader::from_manifest_with_endpoint(contracts, endpoint, &registry_address)?
        .read_account_once()
        .await
}

pub(crate) async fn enforce_model_registry_policy(
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    contracts: &std::path::Path,
    frame_model: &str,
    expected_order_book: &str,
    order_book_active: bool,
    buyer_missing_book_policy: BuyerMissingBookPolicy,
) -> Result<RegistryBookAction> {
    enforce_model_registry_policy_with_endpoint(
        role,
        policy,
        contracts,
        None,
        RegistryBookExpectation {
            frame_model,
            expected_order_book,
            order_book_active,
            buyer_missing_book_policy,
        },
    )
    .await
}

pub(crate) struct RegistryBookExpectation<'a> {
    pub(crate) frame_model: &'a str,
    pub(crate) expected_order_book: &'a str,
    pub(crate) order_book_active: bool,
    pub(crate) buyer_missing_book_policy: BuyerMissingBookPolicy,
}

pub(crate) async fn enforce_model_registry_policy_with_endpoint(
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    contracts: &std::path::Path,
    endpoint: Option<&str>,
    expectation: RegistryBookExpectation<'_>,
) -> Result<RegistryBookAction> {
    let RegistryBookExpectation {
        frame_model,
        expected_order_book,
        order_book_active,
        buyer_missing_book_policy,
    } = expectation;
    let registry_address = policy.required_address(role)?;
    let reader = ChainModelRegistryReader::from_manifest_with_endpoint(
        contracts,
        endpoint,
        registry_address,
    )?;
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

async fn resolve_model_registry_target_with_reader(
    reader: &dyn ModelRegistryReader,
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    requested_model: &str,
    mut target: BookTarget,
) -> Result<BookTarget> {
    let registry_address = policy.required_address(role)?;
    let identity =
        resolve_registered_model_identity(reader, role, registry_address, requested_model).await?;
    if target.order_book.is_some()
        && (target.frame_model != identity.registry_model
            || !target.model_hash.eq_ignore_ascii_case(&identity.model_hash)
            || !target
                .order_book
                .as_deref()
                .is_some_and(|book| book.eq_ignore_ascii_case(&identity.order_book)))
    {
        let registry_address = dexdo_core::address::display(registry_address);
        let identity_order_book = dexdo_core::address::display(&identity.order_book);
        let target_order_book = dexdo_core::address::display_opt(target.order_book.as_deref(), "-");
        bail!(
            "{} model registry check failed: requested model {} resolved to exact ModelRegistry {} \
             identity {} (modelHash {}, orderBook {}), but the selected market target is {} \
             (modelHash {}, orderBook {})",
            role.as_str(),
            requested_model,
            registry_address,
            identity.registry_model,
            identity.model_hash,
            identity_order_book,
            target.frame_model,
            target.model_hash,
            target_order_book
        );
    }
    // NO SUBSTITUTION, and the config is not an exception to that.

    // These two lines used to overwrite `frame_model` and `model_hash` with the registry's. On the
    // `--market` path the bail above catches a mismatch first, but with `order_book: None` -- which
    // is what `provision` passes -- nothing caught it: the operator typed one name and the book was
    // deployed under another, silently. So supplying `--model-registry-validation` was the way
    // around the refusal the rest of this change installs.


    // the client does not rewrite what the operator typed into what it found. A refusal that a
    // config can switch into a rename is the defect reported, wearing a config.
    if target.frame_model != identity.registry_model {
        bail!(
            "{} model registry check failed: the ModelRegistry {} holds this model as `{}`, and \
             `{}` is a different name to the chain -- the book address is `sha256` over the exact \
             bytes. Write `{}`. Nothing was substituted and nothing was sent",
            role.as_str(),
            dexdo_core::address::display(registry_address),
            identity.registry_model,
            target.frame_model,
            identity.registry_model
        );
    }
    target.model_hash = identity.model_hash;
    Ok(target)
}

pub(crate) async fn resolve_model_registry_target(
    role: RegistryRole,
    policy: Option<&RegistryValidationPolicy>,
    contracts: &std::path::Path,
    requested_model: &str,
    target: BookTarget,
) -> Result<BookTarget> {
    resolve_model_registry_target_with_endpoint(
        role,
        policy,
        contracts,
        None,
        requested_model,
        target,
    )
    .await
}

pub(crate) async fn resolve_model_registry_target_with_endpoint(
    role: RegistryRole,
    policy: Option<&RegistryValidationPolicy>,
    contracts: &std::path::Path,
    endpoint: Option<&str>,
    requested_model: &str,
    target: BookTarget,
) -> Result<BookTarget> {
    let Some(policy) = policy else {
        return Ok(target);
    };
    let registry_address = policy.required_address(role)?;
    let reader = ChainModelRegistryReader::from_manifest_with_endpoint(
        contracts,
        endpoint,
        registry_address,
    )?;
    resolve_model_registry_target_with_reader(&reader, role, policy, requested_model, target).await
}

/// Resolve a model name against the ModelRegistry the way the BUYER resolves it, and refuse if it
/// does not resolve.

/// The lookup is `resolve_registered_model_identity` against the registry named by the contracts
/// manifest -- the same function, the same candidate order and the same failure text the buyer's
/// content-identity preflight produces. The point is that both sides ask the SAME question: the
/// buyer's version is mandatory, and the seller's used to be reachable only through
/// `resolve_model_registry_target`, which returns its target untouched when no
/// `--model-registry-validation` config is passed. The guard was there; on the default path nothing
/// called it, and a seller could deploy a whole market for a name no buyer could ever resolve.

/// **Answer only, and since the answer has consequences.** A spending caller
/// (`provision`, `deploy-market`) refuses when the resolved name differs from the one it was given
/// (`admin.rs`, the `Ok(registry_model)` arm) rather than proceeding, so "yes" now means "yes, and
/// spelled exactly the way you asked". The resolved name is returned but deliberately not substituted for what
/// the operator asked for: renaming the market under the seller would change the derived
/// `model_hash` and the book with it, which is the separate canonicalisation question.
/// This says yes or no, before money moves.
pub(crate) async fn resolve_registry_content_identity(
    role: RegistryRole,
    contracts: &std::path::Path,
    endpoint: Option<&str>,
    requested_model: &str,
    suggestions_policy: RegistrySuggestions,
) -> Result<String> {
    let registry_address = default_model_registry_address(contracts).with_context(|| {
        format!(
            "read default ModelRegistry address from {} for content identity",
            contracts.display()
        )
    })?;
    let reader = ChainModelRegistryReader::from_manifest_with_endpoint(
        contracts,
        endpoint,
        &registry_address,
    )?;
    let identity = resolve_registered_model_identity_with(
        &reader,
        role,
        &registry_address,
        requested_model,
        suggestions_policy,
    )
    .await?;
    Ok(identity.registry_model)
}

#[cfg(test)]
mod registry_target_tests {
    use super::*;
    use async_trait::async_trait;
    use dexdo::registry::ModelRegistryEntry;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    const REGISTRY: &str = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const EXACT_BOOK: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const LEGACY_BOOK: &str = "0:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const NOTE: &str = "0:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const REQUESTED: &str = "qwen--qwen3--32b";
    const EXACT: &str = "Qwen/Qwen3-32B";

    #[derive(Default)]
    struct FakeReader {
        entries: BTreeMap<String, ModelRegistryEntry>,
        queries: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ModelRegistryReader for FakeReader {
        async fn model(&self, frame_model: &str) -> Result<Option<ModelRegistryEntry>> {
            self.queries
                .lock()
                .expect("query lock")
                .push(frame_model.to_string());
            Ok(self.entries.get(frame_model).cloned())
        }
    }

    fn policy() -> RegistryValidationPolicy {
        RegistryValidationPolicy {
            network: "net-a".to_string(),
            registry_address: Some(REGISTRY.to_string()),
            seller_check_model_registry: true,
            seller_deploy_missing_order_book: true,
            buyer_check_model_registry: true,
            source: None,
            address_overridden: false,
        }
    }

    fn reader() -> FakeReader {
        FakeReader {
            entries: BTreeMap::from([(
                EXACT.to_string(),
                ModelRegistryEntry {
                    exists: true,
                    model_hash: model_hash_for(EXACT),
                    order_book: EXACT_BOOK.to_string(),
                },
            )]),
            queries: Mutex::new(Vec::new()),
        }
    }

    fn target(frame_model: &str, order_book: Option<&str>) -> BookTarget {
        BookTarget {
            frame_model: frame_model.to_string(),
            model_hash: model_hash_for(frame_model),
            order_book: order_book.map(str::to_string),
            root_model: None,
            note_addr: Some(NOTE.to_string()),
        }
    }

    /// Two reads in one command spend ONE `--read-timeout`, not one each.

    /// This is the shape the commands make: ask the ModelRegistry what the model is called, then
    /// read that model's book. Each used to get its own full budget, so `--read-timeout 30` could
    /// block for 60s. Delete the shared deadline -- give `ReadBudget::read` its own
    /// `timeout(total_secs)` per call -- and the second read below succeeds, because 20s is under
    /// 30s when the clock starts over.

    /// Time is paused, so this measures the bound and not the machine.
    #[tokio::test(start_paused = true)]
    async fn two_reads_share_one_budget_instead_of_one_each() {
        let budget = ReadBudget::new(30);
        let first: Result<()> = budget
            .read(async {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                Ok(())
            })
            .await;
        first.expect("the first read finishes inside the budget");

        let second: Result<()> = budget
            .read(async {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                Ok(())
            })
            .await;
        let error = second.expect_err(
            "the second read ran to completion, so the command spent 40s against a 30s bound",
        );
        let said = error.to_string();
        assert!(
            said.contains("30s `--read-timeout`"),
            "the refusal must name the bound the operator set: {said}"
        );
        assert!(
            said.contains("the 10.0s it had of this command's 30s"),
            "and how much of it THIS read got, or a 0.1s read reads as a 30s hang and the operator \
             raises a bound that was never the problem: {said}"
        );
    }

    /// Work that is not a read does not spend the read budget.

    /// The first shape of `ReadBudget` was a wall-clock deadline, and it charged the budget for
    /// `reconcile_existing_subscription_journal` -- which waits up to a full `--read-timeout` of
    /// its own -- so the next read started with nothing left and refused as a timeout while
    /// nothing was hung. A wait for a human and a chain WRITE sit in the same place.
    #[tokio::test(start_paused = true)]
    async fn only_reads_spend_the_budget() {
        let budget = ReadBudget::new(30);
        let first: Result<()> = budget
            .read(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(())
            })
            .await;
        first.expect("5s of a 30s budget");

        // Not a read: a wait that belongs to something else entirely.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;

        let after: Result<()> = budget
            .read(async {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                Ok(())
            })
            .await;
        after.expect(
            "25s of the budget was still unspent: the 60s wait was not a read and must not have \
             been charged to it",
        );
    }

    /// THE DEFECT, on the one path that still had it: a config turns the refusal into a
    /// silent rename.

    /// With `--model-registry-validation` supplied and no `--market`, `target.order_book` is `None`,
    /// so the mismatch bail above never fires and the two assignments that follow it overwrite
    /// `frame_model` and `model_hash` with the registry's. The operator typed one name, the book is
    /// deployed under another, and no refusal is produced anywhere -- the config was the way around
    /// the rule.


    /// qualification: the client does not rewrite what the operator typed into what it found. A
    /// rule with a config-shaped exception is the shape reported in the first place.
    #[tokio::test]
    async fn a_configured_registry_does_not_turn_the_refusal_into_a_silent_rename() {
        let resolved = resolve_model_registry_target_with_reader(
            &reader(),
            RegistryRole::Seller,
            &policy(),
            REQUESTED,
            target(REQUESTED, None),
        )
        .await;

        let error = match resolved {
            Ok(target) => panic!(
                "a spelling the registry does not hold was renamed instead of refused: \
                 frame_model={} model_hash={}",
                target.frame_model, target.model_hash
            ),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains(REQUESTED) && error.contains(EXACT),
            "the refusal names what was given and what the registry holds: {error}"
        );
    }

    /// And the registry's own spelling still resolves, so the fix is not "refuse everything".
    #[tokio::test]
    async fn the_registered_spelling_resolves_through_the_configured_path() {
        let target = resolve_model_registry_target_with_reader(
            &reader(),
            RegistryRole::Seller,
            &policy(),
            EXACT,
            target(EXACT, None),
        )
        .await
        .expect("the registry's own spelling is the one name that must never be refused");
        assert_eq!(target.frame_model, EXACT);
        assert_eq!(target.model_hash, model_hash_for(EXACT));
    }

    #[tokio::test]
    async fn resolved_target_rejects_legacy_manifest_before_dispatch() {
        let error = match resolve_model_registry_target_with_reader(
            &reader(),
            RegistryRole::Seller,
            &policy(),
            REQUESTED,
            target(REQUESTED, Some(LEGACY_BOOK)),
        )
        .await
        {
            Ok(_) => panic!("legacy manifest target must fail closed"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains(REQUESTED), "{error}");
        assert!(error.contains(EXACT), "{error}");
        // the refusal names the registry's exact order book canonically; an
        // `InferenceOrderBook` is a contract of the shared dexdo DApp.
        assert!(
            error.contains(&format!(
                "{}::{}",
                dexdo_core::DEXDO_DAPP_ID,
                EXACT_BOOK.strip_prefix("0:").expect("fixture chain form")
            )),
            "{error}"
        );
    }

    #[tokio::test]
    async fn disabled_registry_policy_preserves_legacy_target() {
        let original = target(REQUESTED, Some(LEGACY_BOOK));
        let resolved = resolve_model_registry_target(
            RegistryRole::Buyer,
            None,
            std::path::Path::new("missing-contracts.json"),
            REQUESTED,
            target(REQUESTED, Some(LEGACY_BOOK)),
        )
        .await
        .unwrap();
        assert_eq!(resolved.frame_model, original.frame_model);
        assert_eq!(resolved.model_hash, original.model_hash);
        assert_eq!(resolved.order_book, original.order_book);
    }

    #[test]
    fn explicit_registry_identity_wins_over_colliding_config_key() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.json");
        std::fs::write(
            &models,
            r#"{
              "models": {
                "Qwen/Qwen3-32B": {
                  "frame_model": "other--model--1",
                  "base_url": "https://provider.example/v1",
                  "served_model": "other/model",
                  "api_key_env": "PROVIDER_API_KEY",
                  "tokenizer_family": "other",
                  "price_per_tick": 1
                }
              }
            }"#,
        )
        .unwrap();

        // settled by the REGISTRY's answer, not by the name's shape. `EXACT` is a name the
        // catalog carries, so it is the model -- even though `models.json` here defines a key
        // spelled the same way and pointing somewhere else.: a local alias is not
        // registry authority.
        assert_eq!(
            requested_model_for_registry_answer(
                true,
                Some(("other--model--1".into(), &models)),
                EXACT
            )
            .model,
            EXACT
        );
    }

    #[test]
    fn explicit_registry_identity_does_not_require_models_config() {
        // Registered: the name is the model, and no config is consulted.
        assert_eq!(
            requested_model_for_registry_answer(true, None, EXACT).model,
            EXACT
        );
        // NOT registered and no config to fall back to: the name stands for itself and reaches the
        // caller's registry gate, which refuses it naming the candidates it tried. An absent
        // `models.json` must not become "unknown model" -- a buyer routinely has none.
        assert_eq!(
            requested_model_for_registry_answer(false, None, REQUESTED).model,
            REQUESTED
        );
    }

    /// A `models.json` that exists and cannot be read is an error naming the file, not a shrug.

    /// It used to be `.ok()?`, which made a truncated config indistinguishable from an absent one:
    /// the nickname went unresolved, and the operator was told the ModelRegistry does not carry
    /// `llama70` -- sent to the chain to debug a file on their own disk. The sibling seam
    /// `requested_model_for_market` reports the same failure, so swallowing it here also made the
    /// two disagree about one fact.
    #[test]
    fn an_unreadable_models_config_is_reported_and_not_read_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.json");
        std::fs::write(&models, "{\"models\": {\"llama70\": ").unwrap();

        let error = configured_frame_model(&models, "llama70")
            .expect_err("a config that cannot be parsed is not the same as no config");
        let text = format!("{error:#}");
        assert!(
            text.contains("models.json"),
            "the refusal must name the file the operator has to fix: {text}"
        );

        // And the absent case stays absent, which is the buyer's normal state.
        assert_eq!(
            configured_frame_model(&dir.path().join("nothing-here.json"), "llama70")
                .expect("an absent config is not a failure"),
            None
        );
    }

    /// the config entry that LOSES a collision is named, not dropped in silence.

    /// settles who wins -- the registry -- and the test above pins that. What was
    /// untested is the other half of the rule, and it is the half that cost money: a config naming
    /// `qwen--qwen3--32b--w8k--tools` is a different market, flags and all, and it was ignored
    /// without a word while escrow went to `sha256("Qwen3-32B")`.

    /// THE NOTE IS ASSERTED WHERE THE OPERATOR GETS IT. The first version of this test captured
    /// `tracing`, which the shipped binary throws away: `main.rs` sets the default level to
    /// `error`, so with no `RUST_LOG` the `warn!` never reaches a terminal. Deleting the
    /// `eprintln!` -- the only channel that does reach one -- left that test green. The note is now
    /// RETURNED, so what is checked here is the sentence itself.
    #[test]
    fn the_config_entry_that_loses_a_collision_is_named() {
        let models = std::path::Path::new("/nowhere/models.json");
        let answer = requested_model_for_registry_answer(
            true,
            Some(("qwen--qwen3--32b--w8k--tools".into(), models)),
            "Qwen3-32B",
        );

        assert_eq!(
            answer.model, "Qwen3-32B",
            "the registry still wins ()"
        );
        let note = answer
            .note
            .expect("the operator is told which config entry was ignored");
        assert!(
            note.contains("qwen--qwen3--32b--w8k--tools"),
            "the losing config entry has to be named, or the operator learns it from the escrow: \
             {note}"
        );
        assert!(
            note.contains("/nowhere/models.json"),
            "the note has to say WHICH file said it, or the operator does not know what to edit: \
             {note}"
        );
    }

    /// And the note is actually said out loud, on the channel the operator has.

    /// `RegistryAnswer::tell` is the only caller path, and the test above proves the sentence is
    /// built, not that anyone hears it. Delete the `eprintln!` from `tell` and this fails; delete
    /// the `tracing::warn!` and it does not, which is the right asymmetry -- the log line is for
    /// someone reading afterwards, the printed line is for the operator standing there.
    #[test]
    fn the_note_reaches_the_operator_and_not_only_the_log() {
        // THE NEEDLE IS BUILT, NOT WRITTEN. `body_of` takes the FIRST occurrence of the signature
        // in the file, and a test that spells its target out loud IS an earlier occurrence -- this
        // module sits above the code it reads. Measured twice: written as `"fn tell(self)"` the
        // guard read its own literal and returned `{note}`; written as `"impl RegistryAnswer {"` it
        // opened a brace inside a string and panicked with "the braces do not balance". Assembling
        // it at run time is what makes the guard point at the code instead of at itself.
        let needle = format!("fn {}(self) -> String", "tell");
        let told = crate::cli::source_probe::code_of(include_str!("commands.rs"), &needle);
        assert!(
            told.contains("eprintln!"),
            "the note is built and dropped: nothing prints it to the operator: {told}"
        );
    }

    /// a `models.json` nickname still resolves -- removing the grammar must not remove the
    /// feature that config exists for.

    /// The nickname branch is reached only when the registry does NOT carry the typed name, which
    /// is what a nickname is: a local name for a model whose real name is written beside it.
    #[test]
    fn a_models_json_nickname_still_resolves_when_the_registry_does_not_carry_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.json");
        std::fs::write(
            &models,
            r#"{
              "models": {
                "llama70": {
                  "frame_model": "meta--llama-3.3--70b",
                  "base_url": "https://provider.example/v1",
                  "served_model": "meta/llama",
                  "api_key_env": "PROVIDER_API_KEY",
                  "tokenizer_family": "llama",
                  "price_per_tick": 1
                }
              }
            }"#,
        )
        .unwrap();

        // The tuple comes from the FILE, through the seam that reads it. Handing it in by literal
        // would leave `configured_frame_model` -- which decides whether the chain is read at all --
        // covered by nothing.
        let configured = configured_frame_model(&models, "llama70")
            .expect("a readable config is not a failure")
            .expect("the config knows this nickname");
        assert_eq!(
            requested_model_for_registry_answer(false, Some((configured, &models)), "llama70")
                .model,
            "meta--llama-3.3--70b"
        );
        assert_eq!(
            configured_frame_model(&models, "not-in-this-config")
                .expect("a readable config is not a failure"),
            None,
            "a key the config lacks is not an error, it is the buyer's normal state"
        );
        // A name the config does not know is not an error here either: it stands for itself, and
        // the caller's gate reports it with the candidates. Refusing at this seam is what sent
        // `Qwen3-32B` to `models.json` and told the operator their correct name was unknown.
        assert_eq!(
            requested_model_for_registry_answer(false, None, "Qwen3-32B").model,
            "Qwen3-32B"
        );
    }

    /// a market is rendered for the model its own manifest names, whatever that name is
    /// SHAPED like.

    /// The 4.0.36 catalog drops the producer -- `Qwen/Qwen3-32B` is seeded as `Qwen3-32B` -- and
    /// names like `qwen3.8-max` carry no `--` at all. `target_from_market_for_model` used to ask
    /// `validate_canonical_model_id` whether the operator had typed a model name; for these names
    /// the answer was no, so the name written in the manifest under the operator's nose was sent to
    /// `models.json` and refused there as an unknown model. There is no `models.json` here, so the
    /// old path fails on the read before it can even refuse -- which is the operator's situation.
    #[test]
    fn a_market_renders_for_a_registry_name_that_is_not_producer_model_version() {
        let dir = tempfile::tempdir().unwrap();
        let no_models = dir.path().join("absent-models.json");

        for name in ["qwen3.8-max", "Qwen3-32B"] {
            let market = dir.path().join(format!("market-{name}.json"));
            std::fs::write(
                &market,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "network": "net-a",
                    "frame_model": name,
                    "model_hash": model_hash_for(name),
                    "inference_order_book": EXACT_BOOK,
                    "root_model": LEGACY_BOOK,
                    "token_contract": NOTE,
                    "seller_note": NOTE,
                    "nonce": 1,
                    "price_per_tick": 700,
                    "max_ticks": 1000
                }))
                .unwrap(),
            )
            .unwrap();

            let (target, requested) =
                target_from_market_for_model(&market, &no_models, name, false)
                    .unwrap_or_else(|error| panic!("`{name}` is this market's own model: {error}"));
            assert_eq!(target.frame_model, name);
            assert_eq!(target.model_hash, model_hash_for(name));
            assert_eq!(
                requested, name,
                "the typed name is carried out, not dropped"
            );
        }
    }

    /// The other half of the same rule, so removing the grammar did not remove the guard: a name
    /// that is neither this market's model nor a configured nickname is still refused, and the
    /// refusal still names both models.
    #[test]
    fn a_market_still_refuses_a_model_that_is_not_the_one_it_is_for() {
        let dir = tempfile::tempdir().unwrap();
        let no_models = dir.path().join("absent-models.json");
        let market = dir.path().join("market.json");
        std::fs::write(
            &market,
            serde_json::to_vec_pretty(&serde_json::json!({
                "network": "net-a",
                "frame_model": "qwen3.8-max",
                "model_hash": model_hash_for("qwen3.8-max"),
                "inference_order_book": EXACT_BOOK,
                "root_model": LEGACY_BOOK,
                "token_contract": NOTE,
                "seller_note": NOTE,
                "nonce": 1,
                "price_per_tick": 700,
                "max_ticks": 1000
            }))
            .unwrap(),
        )
        .unwrap();

        // `BookTarget` carries no `Debug`, so the success case is named by hand rather than by
        // `expect_err` -- a rendered market here would mean the guard is gone.
        let error =
            match target_from_market_for_model(&market, &no_models, "some-other-model", false) {
                Ok((target, _)) => panic!(
                    "a different model rendered this market as `{}`",
                    target.frame_model
                ),
                Err(error) => error.to_string(),
            };
        // The WRONG-MARKET refusal specifically, not merely "some error". An earlier draft accepted
        // the absent `models.json` read failure as well, and would have stayed green with the
        // mismatch check deleted outright -- which is the whole thing this test exists to hold.
        assert!(
            error.contains("refusing to render the wrong market"),
            "the mismatch guard did not run; this is some other failure: {error}"
        );
        assert!(
            error.contains("some-other-model") && error.contains("qwen3.8-max"),
            "the refusal has to name both the model asked for and the one this market is for: \
             {error}"
        );
    }
}

fn role_arg_to_handle(role: DealRoleArg) -> deals::DealHandleRole {
    match role {
        DealRoleArg::Buyer => deals::DealHandleRole::Buyer,
        DealRoleArg::Seller => deals::DealHandleRole::Seller,
    }
}

pub(crate) fn load_deal_target(
    input: &str,
    deals_dir: Option<&std::path::Path>,
    raw_role: Option<DealRoleArg>,
    raw_note_addr: Option<String>,
) -> Result<DealTarget> {
    let dir = deals::resolve_deals_dir(deals_dir)?;
    if let Some((_path, handle)) = deals::resolve_deal_ref(
        input,
        &dir,
        raw_role.map(role_arg_to_handle),
        raw_note_addr.as_deref(),
    )? {
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

/// The manifest a deal command reads: the one the deal itself recorded, else this process's.

/// The `explicit` parameter is gone with `--contracts`, and its removal RESTORES the
/// intended order rather than changing it. A deal is settled against the chain it was made on, and
/// the handle wrote that chain down when the deal was created; the flag sat ahead of it and could
/// answer about one chain using another's pins.

/// The fallback is fallible now -- there is no default path to fall back TO -- so this returns a
/// `Result`. That is the type saying out loud what the directive says: with nothing named, the
/// client refuses instead of choosing.
pub(crate) fn deal_contracts_path(target: &DealTarget) -> Result<std::path::PathBuf> {
    target
        .handle
        .as_ref()
        .and_then(|h| {
            (!h.contracts.trim().is_empty()).then(|| std::path::PathBuf::from(&h.contracts))
        })
        .map(Ok)
        .unwrap_or_else(manifest_path)
}

pub(crate) async fn chain_doctor_preflight_market(
    contracts: &std::path::Path,
    market: Option<&dexdo_core::MarketManifest>,
) -> Result<()> {
    let contracts = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(contracts)?;
    let report = chain.doctor(market).await?;
    if !report.is_ok() {
        bail!("{}", render_chain_doctor_preflight_report(&report));
    }
    Ok(())
}

pub(crate) fn save_runtime_deal_handle_for_network(
    input: RuntimeDealHandleInput<'_>,
    network: &str,
    emit_human_output: bool,
) -> Result<deals::DealHandle> {
    let h = persist_runtime_deal_handle(input, network)?;
    if emit_human_output {
        println!("deal_handle={}", h.handle);
    }
    Ok(h)
}

const GATEWAY_CHECK_STAGES: [&str; 6] = [
    "dns_resolve",
    "tcp_connect",
    "tls_handshake",
    "http2_handshake",
    "grpc_challenge",
    "challenge_response",
];

fn completed_gateway_check_stages(failed: &str) -> &'static [&'static str] {
    let barrier = match failed {
        "tls_certificate_pin" => "tls_handshake",
        stage => stage,
    };
    let completed = GATEWAY_CHECK_STAGES
        .iter()
        .position(|stage| *stage == barrier)
        .unwrap_or(0);
    &GATEWAY_CHECK_STAGES[..completed]
}

pub(crate) async fn run_gateway_check(args: GatewayCheckArgs) -> Result<()> {
    let timeout = dexdo_core::params::SellerLivenessParams::canonical().health_check_timeout;
    println!("gateway={}", args.endpoint);
    match dexdo::seller::liveness::probe_gateway_with_timeout(
        &args.endpoint,
        &args.tls_fingerprint,
        timeout,
    )
    .await
    {
        Ok(()) => {
            for stage in GATEWAY_CHECK_STAGES {
                println!("PASS stage={stage}");
            }
            Ok(())
        }
        Err(fault) => {
            for stage in completed_gateway_check_stages(fault.stage()) {
                println!("PASS stage={stage}");
            }
            println!(
                "FAIL stage={} wrong_endpoint={} error={}",
                fault.stage(),
                fault.is_wrong_endpoint(),
                fault.cause_detail()
            );
            bail!("gateway reachability check failed at {}", fault.stage())
        }
    }
}

/// The refusal for a manifest that names no endpoint.

/// The label used to become an address here: it was substituted for the endpoint whenever it was
/// not one particular literal, so asking doctor for another network dialled a host named after that
/// network, died resolving it, and never read the endpoint the manifest was carrying. A LABEL is a
/// key into per-network state; it is not a host, and a name yields no address at all rather than one
/// assembled from it.

/// Reached only when the manifest names no `endpoint` of its own -- every manifest in the tree does,
/// so this is the answer for a hand-written one.
fn no_endpoint_in_manifest(network: &str) -> Result<&'static str> {
    anyhow::bail!(
        "the manifest names the network `{network}` and carries no `endpoint`, so there is nothing \
         to dial. A network label names a CHAIN and is never a Block Manager host, so no address \
         was assembled from it -- and there is no list of known networks to look it up in, because \
         this client has no opinion about which chains exist. Add an `endpoint` field to the \
         manifest."
    )
}

/// Which source names the Block Manager `doctor` dials: the manifest's own `endpoint` field, and
/// failing that, the default its network label implies.

/// There used to be a third, ranked above both -- `--endpoint`. It is gone, and with it the
/// reason the ordering had to be spelled out: `--network` carried a default of the chain build the
/// operator never typed, so a label allowed to outrank the manifest would have refused the very
/// form the mainnet runs use. Both inputs are gone; the manifest is the only source, and there is
/// no ordering left to get wrong.

/// The chosen string is normalized by `dexdo_core::resolve_endpoint`, where every other caller
/// normalizes too.
fn doctor_endpoint_source<'a>(
    network: &str,
    explicit: Option<&'a str>,
    manifest_endpoint: Option<&'a str>,
) -> Result<&'a str> {
    // The fall-through is asked for LAST, and only when nothing else named an address. It used to
    // be resolved first, with `?`, back when it returned a table lookup that usually succeeded --
    // and once the table went that line refused every run before the manifest's own
    // `endpoint` was ever looked at, so `doctor` could not reach any chain at all.
    match explicit.or(manifest_endpoint) {
        Some(endpoint) => Ok(endpoint),
        None => no_endpoint_in_manifest(network),
    }
}

// /: one defect class -- a network's NAME used as a decision in place of a
// fact about that network -- across `doctor`, `market-data` and `--model-registry-validation`.
// Declared here because the `doctor` halves it drives are private to this module.

// The file that used to be declared here, `network_label_not_address_1438.rs`, asserted against the
// compiled-in table of networks and went with it. What replaced it holds the same class
// where the answer now lives -- the manifest's own `endpoint` field -- and holds the ordering that
// removal broke once already.
#[cfg(test)]
#[path = "doctor_endpoint_is_the_manifests_1640.rs"]
mod doctor_endpoint_is_the_manifests_1640;

#[cfg(test)]
#[path = "empty_pool_still_refuses_1535.rs"]
mod empty_pool_still_refuses_1535;

async fn chain_doctor_report(
    network: &str,
    endpoint: Option<&str>,
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
    observe: impl FnMut(usize, &dexdo_core::ChainDoctorCheck),
) -> Result<dexdo_core::ChainDoctorReport> {
    let contracts_path = contracts;
    let contracts = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?;
    // Loaded here, ahead of the connector, because whether the manifest names an endpoint of its
    // own is what decides whether `--network`'s default is consulted at all.
    let deployed = dexdo_core::Deployed::load(contracts_path)
        .map_err(|error| doctor_contracts_error(contracts_path, error))?;
    let endpoint = doctor_endpoint_source(network, endpoint, deployed.endpoint.as_deref())?;
    let market = market.map(load_market).transpose()?;
    let chain = dexdo_core::RealChainBackend::connect_with_endpoint(contracts, Some(endpoint))
        .map_err(|error| doctor_contracts_error(std::path::Path::new(contracts), error))?;
    chain.doctor_observing(market.as_ref(), observe).await
}

fn doctor_contracts_error(path: &std::path::Path, error: anyhow::Error) -> anyhow::Error {
    let not_found = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    });
    if not_found {
        anyhow::anyhow!(
            "contracts manifest {} not found; run `dexdo doctor` from the repository root or pass \
             DEXDO_MANIFEST at a manifest you downloaded",
            path.display()
        )
    } else {
        error
    }
}

fn render_chain_doctor_preflight_report(report: &dexdo_core::ChainDoctorReport) -> String {
    let mut out = format!("dexdo doctor: FAIL network={}\n", report.network);
    if !report.versions.is_empty() {
        out.push_str("versions:\n");
        for (name, version) in &report.versions {
            out.push_str(&format!("  {name}: {version}\n"));
        }
    }
    out.push_str("checks:\n");
    for check in &report.checks {
        out.push_str(&format!("  {:<4} {}", check.status.as_str(), check.name));
        if let Some(address) = &check.address {
            out.push_str(&format!(" addr={address}"));
        }
        if let Some(expected) = &check.expected {
            out.push_str(&format!(" expected={expected}"));
        }
        if let Some(actual) = &check.actual {
            out.push_str(&format!(" actual={actual}"));
        }
        out.push_str(&format!(" - {}\n", check.message));
    }
    out
}

fn render_chain_doctor_step(
    index: usize,
    check: &dexdo_core::ChainDoctorCheck,
    raw: bool,
) -> Option<String> {
    use crate::cli::style::{self, Palette, Role};
    use dexdo_core::ChainDoctorStatus;

    if check.status == ChainDoctorStatus::Skip {
        return None;
    }
    let palette = Palette::stderr();
    let (glyph, role, ending) = match check.status {
        ChainDoctorStatus::Pass => (style::OK, Role::Ok, "checked"),
        ChainDoctorStatus::Fail => (style::ERR, Role::Err, "failed"),
        ChainDoctorStatus::Skip => unreachable!("skips are rendered separately"),
    };
    let mut out = style::glyph_line(
        palette,
        glyph,
        role,
        &format!(
            "[{index}/{}] {} {ending}",
            dexdo_core::CHAIN_DOCTOR_CHECK_COUNT,
            check.name
        ),
    );
    if raw {
        let fields = [
            check
                .address
                .as_deref()
                .map(|value| format!("addr={value}")),
            check
                .expected
                .as_deref()
                .map(|value| format!("expected={value}")),
            check
                .actual
                .as_deref()
                .map(|value| format!("actual={value}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        for field in fields {
            out.push('\n');
            out.push_str(&style::raw_line(palette, &field));
        }
    }
    Some(out)
}

fn emit_doctor_step(rendered: &str, mut emit: impl FnMut(&str)) {
    for line in rendered.lines() {
        emit(line);
    }
}

fn doctor_summary(report: &dexdo_core::ChainDoctorReport) -> crate::cli::machine::DoctorSummary {
    use dexdo_core::ChainDoctorStatus;

    let passed = report
        .checks
        .iter()
        .filter(|check| check.status == ChainDoctorStatus::Pass)
        .count();
    let failed = report
        .checks
        .iter()
        .filter(|check| check.status == ChainDoctorStatus::Fail)
        .count();
    let skipped = report
        .checks
        .iter()
        .filter(|check| check.status == ChainDoctorStatus::Skip)
        .count();
    crate::cli::machine::DoctorSummary {
        checked: passed + failed,
        passed,
        failed,
        skipped,
    }
}

fn render_clock_skew(seconds: i64) -> String {
    match seconds.cmp(&0) {
        std::cmp::Ordering::Greater => format!("{seconds}s ahead of chain time"),
        std::cmp::Ordering::Less => format!("{}s behind chain time", seconds.unsigned_abs()),
        std::cmp::Ordering::Equal => "in sync with chain time".to_string(),
    }
}

fn parse_contract_generation(generation: &str) -> Option<(u64, u64, u64)> {
    let mut parts = generation.split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

fn doctor_upgrade_notice(report: &dexdo_core::ChainDoctorReport) -> Option<String> {
    let manifest = report.manifest_generation.as_deref()?;
    let chain = report.chain_generation.as_deref()?;
    (parse_contract_generation(manifest)? < parse_contract_generation(chain)?).then(|| {
        format!(
            "Upgrade dexdo: this installation's manifest generation {manifest} is behind the live chain generation {chain}."
        )
    })
}

fn render_chain_doctor_report(
    report: &dexdo_core::ChainDoctorReport,
    policy: &policy::DoctorPolicyAssessment,
) -> String {
    use crate::cli::style::{self, Palette, Role};
    use dexdo_core::ChainDoctorStatus;

    let palette = Palette::stdout();
    let mut out = String::new();
    if let Some(notice) = doctor_upgrade_notice(report) {
        out.push_str(&notice);
        out.push('\n');
    }
    out.push_str("Doctor report\n");
    out.push_str(&format!(
        "{}\n",
        style::field(palette, "network", &report.network, Role::Text)
    ));
    out.push_str(&format!(
        "{}\n",
        style::field(palette, "endpoint", &report.endpoint, Role::Text)
    ));
    out.push_str(&format!(
        "{}\n",
        style::field(
            palette,
            "generation",
            &format!(
                "manifest {}, chain {}",
                report.manifest_generation.as_deref().unwrap_or("unknown"),
                report.chain_generation.as_deref().unwrap_or("unknown")
            ),
            Role::Text,
        )
    ));
    out.push_str(&format!(
        "{}\n",
        style::field(
            palette,
            "clock",
            &render_clock_skew(report.clock_skew_seconds),
            Role::Text,
        )
    ));
    if !report.versions.is_empty() {
        out.push_str("  versions\n");
        for (name, version) in &report.versions {
            out.push_str(&format!(
                "{}\n",
                style::field(palette, name, version, Role::Text)
            ));
        }
    }
    let skipped = report
        .checks
        .iter()
        .enumerate()
        .filter(|(_, check)| check.status == ChainDoctorStatus::Skip)
        .collect::<Vec<_>>();
    if !skipped.is_empty() {
        out.push_str("\nSkipped\n");
        for (index, check) in skipped {
            out.push_str(&format!(
                "{}\n",
                style::glyph_line(
                    palette,
                    "\u{2610}",
                    Role::Label,
                    &format!(
                        "[{}/{}] SKIP {} - {}",
                        index + 1,
                        dexdo_core::CHAIN_DOCTOR_CHECK_COUNT,
                        check.name,
                        check.message
                    )
                )
            ));
        }
    }
    let summary = doctor_summary(report);
    out.push_str(&format!(
        "{}\n",
        style::field(
            palette,
            "policy",
            &policy::doctor_policy_line(policy),
            Role::Text,
        )
    ));
    out.push_str(&format!(
        "{}\n",
        style::field(
            palette,
            "checked",
            &format!(
                "{} ({} passed, {} failed, {} skipped)",
                summary.checked, summary.passed, summary.failed, summary.skipped
            ),
            Role::Text,
        )
    ));
    let verdict = if report.is_ok() { "PASS" } else { "FAIL" };
    let detail = if report.is_ok() {
        format!(
            "{} checks passed, {} skipped",
            summary.passed, summary.skipped
        )
    } else {
        let failed = report
            .checks
            .iter()
            .filter(|check| check.status == ChainDoctorStatus::Fail)
            .map(|check| check.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} checks failed: {failed}", summary.failed)
    };
    out.push_str(&format!("dexdo doctor: {verdict} - {detail}\n"));
    out
}

fn doctor_machine_response(
    report: &dexdo_core::ChainDoctorReport,
    policy: &policy::DoctorPolicyAssessment,
) -> crate::cli::machine::DoctorResponse {
    use crate::cli::machine;
    use dexdo_core::ChainDoctorStatus;

    let checks = report
        .checks
        .iter()
        .map(|check| machine::DoctorCheck {
            name: check.name.clone(),
            verdict: match check.status {
                ChainDoctorStatus::Pass => "pass",
                ChainDoctorStatus::Fail => "fail",
                ChainDoctorStatus::Skip => "skip",
            },
            skip_reason: (check.status == ChainDoctorStatus::Skip).then(|| check.message.clone()),
            address: check.address.clone(),
            expected: check.expected.clone(),
            actual: check.actual.clone(),
            message: check.message.clone(),
        })
        .collect();
    machine::DoctorResponse {
        schema: machine::DOCTOR_SCHEMA,
        network: report.network.clone(),
        endpoint: report.endpoint.clone(),
        manifest_generation: report.manifest_generation.clone(),
        chain_generation: report.chain_generation.clone(),
        versions: report
            .versions
            .iter()
            .map(|(contract, version)| machine::DoctorVersion {
                contract: contract.clone(),
                version: version.clone(),
            })
            .collect(),
        checks,
        clock_skew_seconds: report.clock_skew_seconds,
        policy: machine::DoctorPolicy {
            status: policy.status.as_str(),
            problems: policy.problems.clone(),
        },
        summary: doctor_summary(report),
        verdict: if report.is_ok() { "pass" } else { "fail" },
    }
}

pub(crate) async fn chain_doctor_preflight(
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
) -> Result<()> {
    chain_doctor_preflight_with_endpoint(contracts, None, market).await
}

pub(crate) async fn chain_doctor_preflight_with_endpoint(
    contracts: &std::path::Path,
    endpoint: Option<&str>,
    market: Option<&std::path::Path>,
) -> Result<()> {
    let deployed = dexdo_core::Deployed::load(contracts)
        .with_context(|| format!("load the manifest {}", contracts.display()))?;
    let endpoint = manifest_preflight_endpoint(&deployed, endpoint)?;
    // the network named here is the manifest's own, never a default constant. It is inert on
    // this path -- `chain_doctor_report` only consults it when no endpoint was resolved, and one
    // always is here -- but a preflight that reads mainnet while naming the chain build is the same
    // wrong answer the report itself used to print.
    let report = chain_doctor_report(
        &deployed.network,
        Some(&endpoint),
        contracts,
        market,
        |_, _| {},
    )
    .await?;
    if !report.is_ok() {
        bail!("{}", render_chain_doctor_preflight_report(&report));
    }
    Ok(())
}

fn manifest_preflight_endpoint(
    deployed: &dexdo_core::Deployed,
    endpoint: Option<&str>,
) -> Result<String> {
    // `network` selects the SDK profile; it is never a Block Manager host.
    dexdo_core::resolve_endpoint(endpoint, deployed)
}

pub(crate) async fn run_doctor(args: DoctorArgs) -> Result<()> {
    // The network comes from the manifest, and there is no `--network` to disagree with it.
    // It used to be an argument with a compiled-in default naming the test network, so every run
    // that did not say otherwise declared itself to be on the chain -- including the mainnet run
    // that was answered out of another chain's pins.
    let manifest = crate::cli::commands::manifest_path()?;
    let declared = dexdo_core::Deployed::load(&manifest)
        .with_context(|| format!("load the manifest {}", manifest.display()))?
        .network;
    let status = (!args.json)
        .then(|| crate::cli::progress::Status::new("checking the network and deployed contracts"));
    let raw = crate::cli::style::raw_requested();
    let report = chain_doctor_report(
        &declared,
        None,
        &manifest,
        args.market.as_deref(),
        |index, check| {
            if let (Some(status), Some(line)) =
                (status.as_ref(), render_chain_doctor_step(index, check, raw))
            {
                emit_doctor_step(&line, |line| status.keep_exact(line));
            }
        },
    )
    .await;
    let report = match report {
        Ok(report) => report,
        Err(error) if args.json => {
            let code = crate::cli::machine::classify_error(crate::cli::machine::OP_DOCTOR, &error);
            crate::cli::machine::print_short_error(crate::cli::machine::OP_DOCTOR, code)?;
            return Err(crate::cli::machine::printed_error());
        }
        Err(error) => return Err(error),
    };
    let policy = policy::doctor_policy_assessment(args.policy.as_deref())?;
    drop(status);
    if args.json {
        crate::cli::machine::print_json(&doctor_machine_response(&report, &policy))?;
    } else {
        print!("{}", render_chain_doctor_report(&report, &policy));
    }
    if !report.is_ok() {
        return Err(crate::cli::machine::printed_error());
    }
    Ok(())
}

#[cfg(test)]
mod doctor_output_1860_tests {
    use super::*;
    use dexdo_core::{ChainDoctorCheck, ChainDoctorReport, ChainDoctorStatus};

    fn report() -> ChainDoctorReport {
        ChainDoctorReport {
            network: "net-a".to_string(),
            endpoint: "https://net-a.example/graphql".to_string(),
            manifest_generation: Some("4.0.36".to_string()),
            chain_generation: Some("4.0.36".to_string()),
            clock_skew_seconds: -2,
            versions: vec![("SuperRoot".to_string(), "4.0.36 SuperRoot".to_string())],
            checks: vec![
                ChainDoctorCheck {
                    name: "SuperRoot code hash".to_string(),
                    status: ChainDoctorStatus::Pass,
                    address: Some("0:abcd".to_string()),
                    expected: Some("expected-hash".to_string()),
                    actual: Some("actual-hash".to_string()),
                    message: "binary pin matches the live chain".to_string(),
                },
                ChainDoctorCheck {
                    name: "RootModel code hash".to_string(),
                    status: ChainDoctorStatus::Skip,
                    address: None,
                    expected: None,
                    actual: None,
                    message: "pass --market <manifest> to check it".to_string(),
                },
            ],
        }
    }

    fn missing_policy() -> policy::DoctorPolicyAssessment {
        policy::DoctorPolicyAssessment {
            status: policy::DoctorPolicyStatus::Missing,
            problems: Vec::new(),
        }
    }

    #[test]
    fn doctor_json_is_one_complete_path_free_document() {
        let value = serde_json::to_value(doctor_machine_response(&report(), &missing_policy()))
            .expect("serialize doctor response");
        assert_eq!(value["schema"], "dexdo.doctor.v1");
        assert_eq!(value["network"], "net-a");
        assert_eq!(value["endpoint"], "https://net-a.example/graphql");
        assert_eq!(value["manifest_generation"], "4.0.36");
        assert_eq!(value["chain_generation"], "4.0.36");
        assert_eq!(value["clock_skew_seconds"], -2);
        assert_eq!(value["policy"]["status"], "missing");
        assert_eq!(value["verdict"], "pass");
        assert_eq!(value["summary"]["checked"], 1);
        assert_eq!(value["summary"]["skipped"], 1);
        assert_eq!(value["checks"][0]["expected"], "expected-hash");
        assert_eq!(value["checks"][0]["actual"], "actual-hash");
        assert_eq!(value["checks"][1]["verdict"], "skip");
        assert_eq!(
            value["checks"][1]["skip_reason"],
            "pass --market <manifest> to check it"
        );
        assert!(
            !value.to_string().contains("/Users/") && !value.to_string().contains("policy.json"),
            "machine output must not carry a local policy path: {value}"
        );
    }

    #[test]
    fn human_checks_hide_raw_fields_unless_raw_was_requested() {
        let check = &report().checks[0];
        let ordinary = render_chain_doctor_step(1, check, false).expect("performed check");
        assert!(ordinary.contains("\u{2714} [1/14] SuperRoot code hash checked"));
        for raw in ["addr=", "expected=", "actual="] {
            assert!(
                !ordinary.contains(raw),
                "ordinary step leaked {raw}: {ordinary}"
            );
        }

        let raw = render_chain_doctor_step(1, check, true).expect("performed check");
        for field in [
            "addr=0:abcd",
            "expected=expected-hash",
            "actual=actual-hash",
        ] {
            assert!(raw.contains(field), "raw step omitted {field}: {raw}");
        }
        assert!(
            render_chain_doctor_step(2, &report().checks[1], false).is_none(),
            "a skipped check must not look like a performed step"
        );
    }

    #[test]
    fn raw_doctor_progress_emits_every_physical_line_to_the_writer() {
        let check = &report().checks[0];
        let rendered = render_chain_doctor_step(1, check, true).expect("performed check");
        let mut emitted = Vec::new();
        emit_doctor_step(&rendered, |line| emitted.push(line.to_string()));

        assert_eq!(
            emitted.len(),
            4,
            "one step and three raw fields: {emitted:#?}"
        );
        for field in [
            "addr=0:abcd",
            "expected=expected-hash",
            "actual=actual-hash",
        ] {
            assert!(
                emitted.iter().any(|line| line.contains(field)),
                "progress writer lost {field}: {emitted:#?}"
            );
        }
    }

    #[test]
    fn human_report_separates_skips_and_ends_with_the_verdict() {
        let rendered = render_chain_doctor_report(&report(), &missing_policy());
        assert!(rendered.contains("\nSkipped\n"), "{rendered}");
        assert!(rendered.contains("SKIP RootModel code hash"), "{rendered}");
        assert!(
            rendered.contains("policy")
                && rendered.contains("not configured (optional for doctor)"),
            "{rendered}"
        );
        for raw in ["addr=", "expected=", "actual="] {
            assert!(
                !rendered.contains(raw),
                "human report leaked {raw}: {rendered}"
            );
        }
        assert_eq!(
            rendered.lines().last(),
            Some("dexdo doctor: PASS - 1 checks passed, 1 skipped")
        );
    }

    #[test]
    fn a_failed_human_report_names_the_failed_check_in_its_final_line() {
        let mut report = report();
        report.checks[0].status = ChainDoctorStatus::Fail;
        let rendered = render_chain_doctor_report(&report, &missing_policy());
        assert_eq!(
            rendered.lines().last(),
            Some("dexdo doctor: FAIL - 1 checks failed: SuperRoot code hash")
        );
    }

    #[test]
    fn a_manifest_behind_the_chain_leads_with_upgrade_guidance() {
        let mut report = report();
        report.manifest_generation = Some("4.0.35".to_string());
        report.chain_generation = Some("4.0.36".to_string());
        report.checks[0].status = ChainDoctorStatus::Fail;

        let rendered = render_chain_doctor_report(&report, &missing_policy());

        assert_eq!(
            rendered.lines().next(),
            Some(
                "Upgrade dexdo: this installation's manifest generation 4.0.35 is behind the live chain generation 4.0.36."
            )
        );
        assert_eq!(rendered.matches("Upgrade dexdo:").count(), 1, "{rendered}");
    }

    #[test]
    fn doctor_does_not_guess_that_upgrade_is_the_fix() {
        let mut report = report();
        report.manifest_generation = Some("4.0.37".to_string());
        report.chain_generation = Some("4.0.36".to_string());
        report.checks[0].status = ChainDoctorStatus::Fail;

        let rendered = render_chain_doctor_report(&report, &missing_policy());

        assert_eq!(rendered.lines().next(), Some("Doctor report"));
        assert!(!rendered.contains("Upgrade dexdo:"), "{rendered}");
    }
}

pub(crate) struct BookTarget {
    pub(crate) frame_model: String,
    // Filled on every path; only the chain book/market views read these three back.
    pub(crate) model_hash: String,
    pub(crate) order_book: Option<String>,
    pub(crate) root_model: Option<String>,
    pub(crate) note_addr: Option<String>,
}

/// The per-model book target for a name that is already resolved. Address arithmetic only: it reads
/// no file, asks no chain, and decides nothing.

/// **What stood here, and why it is gone.** `model_target_from_config` began
/// `ModelsConfig::load(models)?` with no guard, so a `models.json` was MANDATORY to ask what a
/// model's book was -- and it was the DEFAULT arm. Every caller read
/// `if registry_policy.is_some() { registry_requested_model(..) } else { model_target_from_config(..) }`,
/// so the on-chain catalog was consulted only when the operator passed
/// `--model-registry-validation`. That is an authority standing behind an optional config file,

/// `market`, `market-data`, `quote` and `orders` among the read paths the registry serves.

/// The cost was borne by the user who has no catalog: they could not ask what any market was, not
/// even one the registry names, and the refusal pointed at a file they never had.

/// **The fork is deleted rather than wrapped.** The name is resolved by `registry_requested_model`
/// at the call site, inside that command's own `ReadBudget`, and this function only turns the
/// answer into a target. Keeping the resolution AT the call site is deliberate: the read-budget
/// guards in `market_views.rs` and `buyer.rs` pin `.read(registry_requested_model(` there, and a
/// guard edited by the same change it guards is no longer independent of it.

/// The policy `registry_requested_model` carries, unchanged and relied on here: no config, or a
/// config that does not know the name, means the name is the model and the chain is not read at
/// all; a config that maps it elsewhere is decided by the registry, with the losing
/// entry named; and **a name the registry does not carry but the config does still resolves** -- an
/// operator serving their own model does not lose these commands.
pub(crate) fn book_target_for(frame_model: String, note_addr: Option<String>) -> BookTarget {
    BookTarget {
        model_hash: model_hash_for(&frame_model),
        frame_model,
        order_book: None,
        root_model: None,
        note_addr,
    }
}

/// Which model the operator named, when there is NO market manifest to ask: the model itself, or
/// their own `models.json` nickname for it.

/// asks the REGISTRY. The slot held a name grammar -- a string containing `--` or `/` was
/// taken as a model name and anything else was looked up in `models.json` -- and the 4.0.36 catalog
/// does not use that grammar: it seeds `Qwen/Qwen3-32B` as `Qwen3-32B`, and names such as
/// `qwen3.8-max` carry neither separator. So the exact name the catalog carries was sent to
/// `models.json` and refused there, for a config a buyer never needs to have.

/// The grammar could not simply be deleted, and the two obvious replacements each break something
/// real:

/// * always take the name as given -- nicknames stop working, and `models.json` exists to define
/// them;
/// * always resolve through `models.json` -- a registry name that happens to collide with a config
/// key resolves to that key's model. forbids exactly this ("do not accept an
/// arbitrary local alias from `models.json` as registry authority"), and
/// `explicit_registry_identity_wins_over_colliding_config_key` pins it.

/// So the authority decides the collision -- but it is asked only WHEN there is a collision to
/// decide. The config is read first, because reading a local file is free and a chain read is not:

/// * `models.json` does not know the name (the buyer's normal state, config or no config) -- the
/// name is the model, and the registry is not asked at all. Nothing it could answer would change
/// the result: there is no nickname to prefer and nothing to be ambiguous with;
/// * `models.json` maps the name to ITSELF -- same, and again no read;
/// * `models.json` maps it somewhere ELSE -- now the registry's answer decides, and it is asked.
/// Carried by the registry, under its exact bytes or an alias: the registry wins (625), and the
/// config entry that lost is named to the operator. Not carried: the nickname resolves.

/// The earlier shape asked on every registry-enabled run, then threw the identity away and let the
/// caller walk the same candidates again -- up to ten `ModelRegistry` reads where five suffice,
/// against an endpoint that refuses above three requests a second from one address.

/// A registry that cannot be READ is not an answer either way, so the config nickname stands and,
/// failing that, the name stands for itself: the caller's own registry gate refuses it a moment
/// later with the candidates it tried, which is the report the operator can act on. Refusing here
/// would turn every unreachable endpoint into "unknown model".

/// A `models.json` that EXISTS and cannot be parsed is a different fact from an absent one, and it
/// is not swallowed: it comes back as an error naming the file.
pub(crate) async fn registry_requested_model(
    contracts: &std::path::Path,
    endpoint: Option<&str>,
    models: &std::path::Path,
    model: &str,
) -> Result<String> {
    // THE REGISTRY IS ASKED ONLY WHEN THE ANSWER CAN CHANGE ANYTHING.

    // If `models.json` does not know this name -- and usually there is no config at all -- then
    // whatever the registry says, the name is the model: there is no nickname to prefer and nothing
    // to be ambiguous with. Asking anyway cost a full candidate walk, up to five chain reads, and
    // the caller then ran the identical walk again through `resolve_model_registry_target`. Ten
    // reads where five would do, against an endpoint that refuses more than three requests a second
    // from one address -- so the second walk could be rate-limited into failure by the first.

    // That duplication is also the only reason the read budget was ever split in two. It is not,
    // now: this path makes no chain read at all unless the config names the model.
    let Some(configured) = configured_frame_model(models, model)? else {
        return Ok(model.to_string());
    };
    if configured == model {
        return Ok(model.to_string());
    }
    // The config knows it and points somewhere else. Now the registry's answer decides.
    let registered =
        // Skip: this call keeps only `.is_ok()`. The message is never rendered by anyone, so a
        // registry walk to build one is work with no reader at all -- the plainest case of it.
        resolve_registry_content_identity(
            RegistryRole::Buyer,
            contracts,
            endpoint,
            model,
            RegistrySuggestions::Skip,
        )
        .await
        .is_ok();
    // `configured` is passed in, not re-read. Reading again would parse the file a second and third
    // time for one resolution, and the entry named in the operator-facing note would come from a
    // LATER read than the one that drove the decision -- a file edited in between would make the
    // client report an entry it did not act on.
    Ok(requested_model_for_registry_answer(registered, Some((configured, models)), model).tell())
}

/// The `frame_model` this name maps to in `models.json`, or `None` when the config does not know it.

/// An absent config is the buyer's normal state, and so is a config that simply lacks this key:
/// both are `Ok(None)`.

/// A config that IS there and cannot be READ is neither. It used to be swallowed with `.ok()?`,
/// which turned "your `models.json` is truncated" into "the ModelRegistry does not carry `llama70`"
/// -- an operator sent to the chain to debug a file on their own disk. The sibling seam
/// `requested_model_for_market` propagates the same failure, so swallowing it here also made the
/// two disagree about one fact.
fn configured_frame_model(models: &std::path::Path, model: &str) -> Result<Option<String>> {
    if !models.exists() {
        return Ok(None);
    }
    let config = dexdo::seller::ModelsConfig::load(models)?;
    Ok(config
        .get(model)
        .ok()
        .map(|entry| entry.frame_model.clone()))
}

/// The decision the registry answer feeds, split out so it is exercised without a node -- the shape
/// `model_resolution_result` uses for the same reason.

/// `registered` is "the ModelRegistry answered for this name", under its exact bytes or under one
/// of the alias candidates the resolver walks. It is deliberately NOT "these exact bytes": a name
/// the catalog holds only as `Qwen/Qwen3-32B` is still that model when the operator types
/// `qwen--qwen3--32b`, and treating it as unregistered sent it to `models.json`, where a key of the
/// same spelling could retarget the book.

/// AND IT IS NOT A LICENCE TO IGNORE THE CONFIG. Both readings of this seam have now produced a
/// defect, in opposite directions:

/// * exact-bytes-only sent an aliased registry name to the config, which could name another model;
/// * any-alias-wins ignored a `models.json` key the operator wrote deliberately -- `qwen3-32b`
/// mapping to `qwen--qwen3--32b--w8k--tools` is a DIFFERENT market, flags and all, and the run
/// silently placed escrow on `sha256("Qwen3-32B")` instead, saying nothing about the config.

/// A name that is both a registry identity and a config key naming a different model is settled by
/// the REGISTRY wins, and the test beside this one is called
/// `explicit_registry_identity_wins_over_colliding_config_key`. This seam does not refuse it and
/// does not guess -- it follows the rule and NAMES the config entry that lost, to the log and to
/// the screen, so the operator is not left to discover it from the escrow.

/// A registry that could not be read answers `false`, which is deliberately not a refusal -- the
/// config is tried, and failing that the name stands for itself so the caller's own registry gate
/// can refuse it with the candidates it tried. Refusing here would turn every unreachable endpoint
/// into "unknown model".
pub(crate) fn requested_model_for_registry_answer(
    registered: bool,
    configured: Option<(String, &std::path::Path)>,
    model: &str,
) -> RegistryAnswer {
    match (registered, configured) {
        // The registry answered AND the config names something else. The registry wins (625), and
        // the losing entry is named rather than dropped in silence -- that silence is what turned a
        // config pointing at `qwen--qwen3--32b--w8k--tools` into escrow on `sha256("Qwen3-32B")`
        // with nothing said.
        (true, Some((other, models))) if other != model => RegistryAnswer {
            model: model.to_string(),
            note: Some(format!(
                "note: `{model}` is a model the ModelRegistry carries, so it is used as the model. \
                 {} maps the same key to `{other}`, which is a different market and is being \
                 ignored. Pass `{other}` if that is the one you meant.",
                models.display()
            )),
        },
        (true, _) => RegistryAnswer::plain(model),
        // The registry did not answer -- unreachable or a real miss. The config's own name is the
        // best answer available; failing that the name stands for itself and the caller's registry
        // gate refuses it with the candidates it tried.
        (false, Some((other, _))) => RegistryAnswer {
            model: other,
            note: None,
        },
        (false, None) => RegistryAnswer::plain(model),
    }
}

/// The chosen model, plus what the operator has to be told about how it was chosen.

/// The note is RETURNED rather than printed here, and that is the whole point of the type. It used
/// to go out as `tracing::warn!` beside an `eprintln!`, and a test asserted on the `tracing` side --
/// the side the shipped binary throws away, because `main.rs` sets the default level to `error`.
/// Deleting the `eprintln!` left that test green while the operator heard nothing at all, which is
/// the exact silence the note exists to break.
pub(crate) struct RegistryAnswer {
    pub(crate) model: String,
    pub(crate) note: Option<String>,
}

impl RegistryAnswer {
    fn plain(model: &str) -> Self {
        Self {
            model: model.to_string(),
            note: None,
        }
    }

    /// Say it on the channel the operator has, and record it on the one an operator debugging
    /// afterwards has.
    fn tell(self) -> String {
        if let Some(note) = self.note {
            tracing::warn!(
                model = %self.model,
                "the ModelRegistry answers for this name, so it names the model; the models.json \
                 entry of the same key is NOT used"
            );
            eprintln!("{note}");
        }
        self.model
    }
}

/// Which model the operator named, when a market MANIFEST is in hand: their `models.json` nickname
/// for it, or the model name itself.

/// asks the manifest instead of a grammar. The slot used to hold
/// `validate_canonical_model_id` -- `producer--model--version` was taken as a model name and
/// anything else was looked up in `models.json` -- and the registry's own names do not have that
/// shape: 4.0.36 seeds `Qwen/Qwen3-32B` as `Qwen3-32B`, and names such as `qwen3.8-max` carry no
/// `--` at all. The exact name written in the manifest under the operator's nose was sent to
/// `models.json` and refused there as an unknown model.

/// The three branches, and why each is not the others:

/// * the name IS this market's model -- it is itself, and no config is consulted;
/// * `models.json` is absent -- the buyer routinely has none, and answering "you asked for a market
/// this manifest is not for" with "no such file: models.json" hides the real answer behind a file
/// the operator never needed. The name stays itself and reaches the caller's mismatch refusal;
/// * `models.json` is present -- it is read, and a failure to READ or PARSE it is reported, because
/// continuing would produce a refusal naming the raw input as the model, which is false whenever
/// the config would have resolved it to something else.

/// A present, readable config that simply does not know the name is NOT a refusal here. It used to
/// be, and that made the caller's wrong-market refusal unreachable whenever a config existed: a
/// name that is neither this market's model nor a configured nickname got `unknown model` instead
/// of a message naming both markets. It stays itself and reaches that refusal.
pub(crate) fn requested_model_for_market(
    market_frame_model: &str,
    models: &std::path::Path,
    requested_model: &str,
) -> Result<String> {
    if requested_model == market_frame_model {
        return Ok(requested_model.to_string());
    }
    if !models.exists() {
        return Ok(requested_model.to_string());
    }
    let configured = dexdo::seller::ModelsConfig::load(models)?;
    Ok(configured
        .get(requested_model)
        .map(|entry| entry.frame_model.clone())
        .unwrap_or_else(|_| requested_model.to_string()))
}

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

pub(crate) fn target_from_market_for_model(
    path: &std::path::Path,
    models: &std::path::Path,
    requested_model: &str,
    registry_decides: bool,
) -> Result<(BookTarget, String)> {
    let target = target_from_market(path)?;
    // Is the name the operator typed a model name, or a `models.json` nickname for one? asks
    // the MANIFEST, which is present and authoritative here, instead of asking a grammar.

    // `validate_canonical_model_id` stood in this slot: a name shaped `producer--model--version`
    // was taken as itself and anything else was looked up in `models.json`. The registry's own
    // names do not have that shape -- 4.0.36 seeds `Qwen/Qwen3-32B` as `Qwen3-32B` -- so the exact
    // name written in this very manifest was sent to `models.json` and refused there as an unknown
    // model, while the manifest beside it said it plainly.

    // The mismatch refusal below still does the real work: a name that is neither this market's
    // model nor a configured nickname reaches it and is named as the wrong market.

    // WHY IT IS A NOTE AND NOT A REFUSAL WHEN THE REGISTRY IS ON. This compares bytes, and bytes do
    // not know about aliases: the 4.0.36 catalog carries `Qwen/Qwen3-32B` and answers to
    // `qwen--qwen3--32b`, which is the SAME market and would be refused here. So with registry
    // validation enabled the decision belongs to the registry, which can tell an alias from a typo.

    // AND NOTHING IS SAID ON THE WAY PAST. An intermediate version printed a note here whenever
    // the bytes differed, which is EVERY legitimate alias -- the case `an_alias_is_not_refused_
    // offline_when_the_registry_decides` pins as correct. A warning on the correct path is a
    // warning the operator learns to ignore, and it fired before the authority had decided
    // anything. The genuinely wrong name is refused a moment later by
    // `resolve_model_registry_target`, which names both identities; that is the report to act on.
    let requested_frame_model =
        requested_model_for_market(&target.frame_model, models, requested_model)?;
    let requested_hash = model_hash_for(&requested_frame_model);
    if target.frame_model != requested_frame_model || target.model_hash != requested_hash {
        if registry_decides {
            // THE TYPED NAME GOES ON, or the registry is asked a question that always answers yes.
            // Returning only the target made the caller resolve the MARKET's own model -- so
            // `--market qwen.json --model llama--llama-3.3--70b` rendered the Qwen book as the
            // answer to a question about llama. The registry can only decide a name it is given.
            return Ok((target, requested_frame_model));
        }
        bail!(
            "dexdo market requested model `{requested_model}` -> `{requested_frame_model}`, but \
             --market is for `{}` (model_hash {}): refusing to render the wrong market",
            target.frame_model,
            target.model_hash
        );
    }
    Ok((target, requested_frame_model))
}

#[cfg(test)]
mod market_fork_tests {
    use super::*;

    const BOOK: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ROOT: &str = "0:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const NOTE_ADDR: &str = "0:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn market_with(frame_model: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let market = dir.path().join("market.json");
        std::fs::write(
            &market,
            serde_json::to_vec_pretty(&serde_json::json!({
                "network": "net-a",
                "frame_model": frame_model,
                "model_hash": model_hash_for(frame_model),
                "inference_order_book": BOOK,
                "root_model": ROOT,
                "token_contract": NOTE_ADDR,
                "seller_note": NOTE_ADDR,
                "nonce": 1,
                "price_per_tick": 700,
                "max_ticks": 1000
            }))
            .expect("market json"),
        )
        .expect("write market");
        (dir, market)
    }

    /// with the registry deciding, an ALIAS of this market's model is not refused offline.

    /// Bytes do not know about aliases. The 4.0.36 catalog carries `Qwen/Qwen3-32B` and answers to
    /// `qwen--qwen3--32b`, so a byte compare refuses the RIGHT market. Re-forking the caller to skip
    /// the compare is what removed; the fork is gone and the flag decides severity instead.
    #[test]
    fn an_alias_is_not_refused_offline_when_the_registry_decides() {
        let (_dir, market) = market_with("Qwen/Qwen3-32B");
        let models = std::path::Path::new("/nowhere/models.json");

        let (target, requested) =
            target_from_market_for_model(&market, models, "qwen--qwen3--32b", true)
                .expect("the registry decides whether this is an alias, not a byte compare");
        assert_eq!(
            target.frame_model, "Qwen/Qwen3-32B",
            "the manifest still names the market"
        );
        assert_eq!(
            requested, "qwen--qwen3--32b",
            "the TYPED name goes on to the registry: handing it the market's own model asks a \
             question that always answers yes, and nothing downstream would ever check the name \
             the operator typed"
        );
    }

    /// And with the registry off, the same input is still refused offline, naming both models.

    /// This is the half that must not be lost: without an authority to ask, the manifest in hand IS
    /// the answer, and a chain round trip to learn it is a round trip for nothing.
    #[test]
    fn without_the_registry_a_mismatch_is_still_refused_and_names_both_models() {
        let (_dir, market) = market_with("Qwen/Qwen3-32B");
        let models = std::path::Path::new("/nowhere/models.json");

        let said = match target_from_market_for_model(&market, models, "qwen--qwen3--32c", false) {
            Ok((target, _)) => panic!(
                "a name that is neither the model nor a nickname was rendered as market `{}`",
                target.frame_model
            ),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            said.contains("qwen--qwen3--32c") && said.contains("Qwen/Qwen3-32B"),
            "the refusal must name what was asked for AND what the manifest is for: {said}"
        );
    }
}

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
            dexdo_core::TICK_SIZE,
        )
        .await
}

pub(crate) async fn read_executable_book_target(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
) -> Result<OrderBookSnapshot> {
    let mut snapshot = read_book_target(chain, target).await?;
    snapshot.orders = chain.executable_resting_asks(&snapshot).await?;
    Ok(snapshot)
}

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
        .inference_orderbook_address(&note, &target.model_hash, dexdo_core::TICK_SIZE)
        .await
        .map_err(|error| market_note_getter_error(note_addr, chain.client().endpoint(), error))
        .map(|address| address.with_workchain())
}

fn market_note_getter_error(
    note_addr: &str,
    endpoint: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let note_addr = dexdo_core::address::display(note_addr);
    let message = format!("{error:#}").to_ascii_lowercase();
    let exit_code = message
        .split_once("exit code:")
        .and_then(|(_, suffix)| suffix.split_whitespace().next())
        .and_then(|value| value.parse::<u32>().ok());
    let getter_exit_60 =
        message.contains("run_tvm getter getinferenceorderbookaddress") && exit_code == Some(60);
    if message == "note is not active" {
        anyhow::anyhow!(
            "note {note_addr} not found or not initialized on {endpoint}; verify `--note-addr` and \
             the chain endpoint"
        )
    } else if getter_exit_60 {
        anyhow::anyhow!(
            "market lookup failed for note {note_addr} on {endpoint} \
             (getInferenceOrderBookAddress exit 60) -- verify the note address is a deployed, \
             initialized order-book note"
        )
    } else {
        error
    }
}

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
                // the placement event declares `flags`, so this is the book's own answer --
                // unlike `escrow` just above, which the event does not carry and which the row
                // therefore renders as `-` rather than as this filler zero.
                flags: order.flags,
                timestamp: 0,
            })
            .collect(),
    }
}

pub(crate) fn snapshot_with_executable_orders(
    mut snapshot: OrderBookSnapshot,
    executable_orders: Vec<OrderBookOrder>,
) -> OrderBookSnapshot {
    snapshot.orders = executable_orders;
    snapshot
}

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

pub(crate) async fn expected_order_book_for_note(
    contracts: &std::path::Path,
    note_addr: &str,
    frame_model: &str,
) -> Result<String> {
    let manifest = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let model_hash = model_hash_for(frame_model);
    let ob = chain
        .inference_orderbook_address(&note, &model_hash, dexdo_core::TICK_SIZE)
        .await?;
    Ok(ob.with_workchain())
}

pub(crate) async fn order_book_active(
    chain: &dexdo_core::RealChainBackend,
    expected_order_book: &str,
) -> Result<bool> {
    let ob = dexdo_core::Address::parse(expected_order_book)
        .map_err(|e| anyhow::anyhow!("order_book {expected_order_book}: {e}"))?;
    Ok(chain.inference_orderbook_stats(&ob).await?.is_some())
}

pub(crate) async fn order_book_active_from_contracts(
    contracts: &std::path::Path,
    expected_order_book: &str,
) -> Result<bool> {
    let manifest = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    order_book_active(&chain, expected_order_book).await
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
    if let Some((_path, handle)) = deals::resolve_deal_ref(
        input,
        &dir,
        raw_role.map(role_arg_to_handle),
        raw_note_addr.as_deref(),
    )? {
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

/// The inputs `close` needs *below* clap: a role and an actor note. A stored handle carries
/// both; a raw `TokenContract` has neither, so the operator must pass `--role` and `--note-addr`.
/// The handlers resolve identity through this, and the tests that check the `close` lines the CLI
/// prints call it too, so a printed line cannot drift from what its handler demands.
pub(crate) fn require_close_target_identity<R>(
    deal: &str,
    role: Option<R>,
    note_addr: Option<&str>,
) -> Result<(R, String)> {
    let role = role.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{deal}` is not a local handle; pass --role buyer|seller with a raw TokenContract"
        )
    })?;
    let note_addr = note_addr.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{deal}` is not a local handle; pass --note-addr with a raw TokenContract"
        )
    })?;
    Ok((role, note_addr.to_string()))
}

/// The `dexdo close` follow-up the CLI hands an operator, built in one place so every message says
/// the same thing.

/// It is guidance, not an argv template, and that is forced by what this site knows. `close` signs,
/// so it demands `--note-key`, and the note owner key is never a value dexdo may render into a
/// printed line; a raw `TokenContract` target additionally demands the `--role`/`--note-addr` that
/// `require_close_target_identity` enforces below clap. Emitting `--note-key <buyer-key>` would not
/// even be a command: a POSIX shell reads `<buyer-key>` as an input redirection and never hands the
/// token to `dexdo`. So the command is named, the deal reference is rendered shell-quoted (a handle
/// id or path containing a space is otherwise split by the operator's shell), and the remaining
/// inputs -- including any non-default `--deals-dir` this run resolved, which a follow-up would
/// otherwise silently lose -- are stated in prose the shell never sees. `--contracts` is not among
/// them: removed it, and the manifest travels in `DEXDO_MANIFEST`, which the shell that ran
/// this command already carries.
pub(crate) fn close_guidance(
    deal: &str,
    raw_target_role: Option<&str>,
    actor: &str,
    deals_dir: Option<&std::path::Path>,
) -> String {
    let identity = match raw_target_role {
        Some(role) => format!(
            ", passing --role {role} and the {actor} --note-addr because this deal reference is a \
             raw TokenContract and carries neither"
        ),
        None => String::new(),
    };
    format!(
        "run `dexdo close` on {}{identity}, with the {actor} --note-key to sign{}",
        crate::cli::support::shell_arg(deal),
        identity_free_options(deals_dir)
    )
}

/// The `--deals-dir` a `close`/`status` follow-up must repeat, stated as prose.

/// `--contracts` used to be repeated beside it and is gone: the manifest comes from
/// `DEXDO_MANIFEST`, so a follow-up run in the same shell reaches the same deployment without being
/// told, and naming a removed flag would hand the operator a line that fails on paste.
fn identity_free_options(deals_dir: Option<&std::path::Path>) -> String {
    crate::cli::support::stated_options(&[("--deals-dir", deals_dir)])
}

/// The one `close` follow-up that *is* a complete runnable line: `dexdo status` reads, so it needs
/// no key and no role, and everything it takes is known here. The deal reference is shell-quoted
/// and the run's own `--deals-dir`/`--contracts` are carried, so the line resolves the same deal
/// against the same deployment.

/// It lives beside the other identity-free follow-up builders so every human path uses the same
/// quoting and data-directory convention.
pub(crate) fn status_command(deal: &str, deals_dir: Option<&std::path::Path>) -> String {
    let mut command = format!("dexdo status {}", crate::cli::support::shell_arg(deal));
    for (flag, path) in [("--deals-dir", deals_dir)] {
        if let Some(path) = path {
            command.push_str(&format!(
                " {flag} {}",
                crate::cli::support::shell_arg(&path.display().to_string())
            ));
        }
    }
    command
}

/// `deals_dir`/`contracts` are the options the *current* run was given, and they are threaded in
/// rather than re-derived: a handle resolved from a custom `--deals-dir`, or a deal read through an
/// explicit `--contracts` manifest, would otherwise be pointed at the defaults by the follow-up
/// this prints.
pub(crate) fn close_hint(
    target: &DealTarget,
    s: &deals::DealStateSummary,
    deals_dir: Option<&std::path::Path>,
) -> String {
    let deal = target
        .handle
        .as_ref()
        .map(|h| h.handle.as_str())
        .unwrap_or(&target.token_contract);
    // A raw TokenContract target has no stored handle, so the guidance must also name the
    // `--role`/`--note-addr` the close handler requires below clap, and it must state the
    // `--deals-dir`/`--contracts` this run resolved rather than let a rerun fall back to defaults.
    let raw_seller = target.handle.is_none().then_some("seller");
    let raw_buyer = target.handle.is_none().then_some("buyer");
    let close_as_seller = close_guidance(deal, raw_seller, "seller", deals_dir);
    let close_as_buyer = close_guidance(deal, raw_buyer, "buyer", deals_dir);
    match target.role {
        Some(deals::DealHandleRole::Seller) if s.kind == deals::DealStateKind::Stopped => {
            format!("next=destroy action={close_as_seller}")
        }
        Some(deals::DealHandleRole::Seller) if s.opened && !s.probe_accepted => {
            format!(
                "next=seller_wait_delivery_then_accept_probe action=keep the seller gateway running for {deal} detail=it waits for the first delivered canonical tick, then calls TokenContract.acceptProbe() after PROBE_WINDOW stop_action={close_as_seller} stop_effect=TokenContract.sellerStop() reason=awaiting_delivery_then_probe_window"
            )
        }
        Some(deals::DealHandleRole::Seller) if s.opened => {
            format!(
                "next=seller_claim_finalize_or_settle_week_or_seller_stop action=keep the seller gateway running for {deal} detail=it calls TokenContract.claimTokens(cumulativeTokens) for delivered output and TokenContract.finalize() for mature claims, while the subscription keeper also calls TokenContract.settleWeek() at crossed week boundaries stop_action={close_as_seller} stop_effect=TokenContract.sellerStop(); buyer may STOP when done"
            )
        }
        Some(deals::DealHandleRole::Seller) if s.funded && !s.probe_accepted => {
            format!(
                "next=buyer_cleanup_after_timeout action=the buyer {}",
                close_guidance(&target.token_contract, Some("buyer"), "buyer", deals_dir)
            )
        }
        // what is left here is the UNSOLD deal -- never funded, so never opened and never
        // stopped. "not stopped" was true and useless: there is nothing to stop in a deal that never
        // started, and the destroy it pointed at could never accept this shape. The applicable
        // action is `TokenContract.close()`, which returns the seller bond to the note and
        // self-destructs (`contracts/airegistry/TokenContract.sol:803-821`) -- but only once the ask
        // is off the book, so the ordering is part of the answer.
        Some(deals::DealHandleRole::Seller) => format!(
            "next=close_unsold_deal command=`dexdo close {deal} --note-key '<seller-key>'` reason=deal_never_matched note=cancel_any_resting_ask_first_with_`dexdo orders cancel`"
        ),
        Some(deals::DealHandleRole::Buyer) if s.kind == deals::DealStateKind::Stopped => {
            "next=none reason=deal_already_terminal".to_string()
        }
        Some(deals::DealHandleRole::Buyer) if s.opened => {
            format!("next=stream_stop action={close_as_buyer}")
        }
        Some(deals::DealHandleRole::Buyer) if s.funded && !s.probe_accepted => {
            format!("next=cleanup_unopened_after_timeout action={close_as_buyer}")
        }
        Some(deals::DealHandleRole::Buyer) => {
            "next=cancel_resting_bid_or_wait_match reason=deal_not_funded".to_string()
        }
        None => "next=unknown_role pass_local_handle_or_--role".to_string(),
    }
}

/// One resting ask as the order-book renderer needs it: price per tick, its max ticks, and the full deal
/// `TokenContract` address. Kept minimal so both the buyer's pre-buy view and the read-only `dexdo markets`
/// table view can build it from their own sources (`discover_offers` / `OrderBookSnapshot::resting_asks`).
pub struct BookRow {
    pub price_per_tick: u128,
    pub max_ticks: u128,
    pub token_contract: String,
}

pub(crate) fn declared_model_flags(frame_model: &str) -> Option<dexdo_core::CanonicalModelFlags> {
    dexdo_core::parse_canonical_model_id(frame_model)
        .ok()
        .map(|parsed| parsed.flags)
        .filter(|flags| !flags.is_empty())
}

pub(crate) fn render_model_flags_field(frame_model: &str) -> String {
    declared_model_flags(frame_model)
        .map(|flags| format!(" model_flags={}", flags.render_human()))
        .unwrap_or_default()
}

/// Render a per-model inference order book to the terminal as a narrow box table (/ UX:
/// "choose a model = choose the market"). Public + read-only: given the resting asks, it prints the
/// `#/price-per-tick/max-ticks/exec` table plus the full `tokenContract` addresses by `#`. `max_price_per_tick`
/// (when `Some`) marks which asks are executable at that ceiling; `your_order_ticks` (when `Some`) appends the
/// buyer's order summary line. The caller sorts nothing -- this sorts by price ascending (best ask first).
/// `1000000` -> `1M`: the tick size as a person says it.
fn human_tokens(tokens: u128) -> String {
    match tokens {
        t if t >= 1_000_000 && t % 1_000_000 == 0 => format!("{}M", t / 1_000_000),
        t if t >= 1_000 && t % 1_000 == 0 => format!("{}k", t / 1_000),
        t => t.to_string(),
    }
}

pub fn print_book_table(
    frame_model: &str,
    rows: &[BookRow],
    max_price_per_tick: Option<u128>,
    your_order_ticks: Option<u128>,
) {
    // One tick = a fixed number of delivered model tokens -- print it
    // so price/tick and the tick counts are interpretable in model tokens, not abstract units.
    let tick_size = DobParams::canonical().tick_size as u128;
    // `spec.md`: an informational section opens with its own glyph, and the parts of the heading are
    // separated by a middle dot rather than by brackets. One tick is stated in the tokens it buys,
    // because that is the unit the operator thinks in.

    // Both units the rows below are read in are stated here, and the second is not decoration. A
    // price is a whole number of SHELL a tick -- `PRICE_STEP == SHELL_UNIT` -- and that is the fact
    // that makes drawing every amount to two decimals lossless rather than a rounding an operator
    // has to trust. The heading that stated it was dropped when this block was redrawn; printing
    // the shortened form while removing the invariant that justifies it is the one combination
    // that cannot stand.
    let heading = {
        use crate::cli::style::{self, Role};
        let palette = crate::cli::style::Palette::stdout();
        let dot = style::paint(palette, Role::Label, " \u{b7} ");
        style::glyph_line(
            palette,
            style::INFO,
            Role::Id,
            &format!(
                "order book{dot}{}{}{dot}1 tick = {} tokens{dot}{}",
                style::paint(palette, Role::Bold, frame_model),
                render_model_flags_field(frame_model),
                human_tokens(tick_size),
                style::paint(palette, Role::Label, "prices are whole SHELL a tick"),
            ),
        )
    };
    if rows.is_empty() {
        println!("{heading}");
        println!(
            "{}",
            crate::cli::style::field_continued(
                "no resting asks - your buy will rest until a seller matches"
            )
        );
        return;
    }
    println!("{heading}");
    let mut sorted: Vec<&BookRow> = rows.iter().collect();
    sorted.sort_by_key(|o| o.price_per_tick);
    print_book_rows(&sorted, max_price_per_tick, your_order_ticks, tick_size);
}

/// The asks themselves: a rank, a price, a volume, and the addresses under them.

/// No box-drawing frame. The frame put the full `tokenContract` in a cell, and an address is 130
/// characters, so the table drew itself 170 columns wide -- `spec.md` lays out for 120, and every
/// window narrower than the table folded the borders through the middle of the rows. The addresses
/// are not shortened to make it fit, because a shortened address cannot be copied and copying it is
/// the only reason it is printed: they move under the table, one per rank, where each is a line of
/// its own that a terminal folds but a copy still takes whole.
fn print_book_rows(
    sorted: &[&BookRow],
    max_price_per_tick: Option<u128>,
    your_order_ticks: Option<u128>,
    tick_size: u128,
) {
    for line in book_rows_lines(
        crate::cli::style::Palette::stdout(),
        sorted,
        max_price_per_tick,
        your_order_ticks,
        tick_size,
    ) {
        println!("{line}");
    }
}

/// The lines themselves, built rather than printed, so the layout can be read by a test.
fn book_rows_lines(
    palette: crate::cli::style::Palette,
    sorted: &[&BookRow],
    max_price_per_tick: Option<u128>,
    your_order_ticks: Option<u128>,
    tick_size: u128,
) -> Vec<String> {
    use crate::cli::style::{self, Role};

    let cells: Vec<(String, String, String)> = sorted
        .iter()
        .enumerate()
        .map(|(i, o)| {
            (
                (i + 1).to_string(),
                format!("{} SHELL", style::shell(o.price_per_tick)),
                format!("{} ticks", o.max_ticks),
            )
        })
        .collect();
    let width = |head: &str, of: &dyn Fn(&(String, String, String)) -> &String| {
        cells
            .iter()
            .map(|c| of(c).chars().count())
            .fold(head.chars().count(), usize::max)
    };
    let (rank_w, price_w) = (width("#", &|c| &c.0), width("price", &|c| &c.1));

    // The rank IS the row's label: it goes where every label goes, and the price starts in the
    // value column with the rest of the client's values. The header names the two columns in the
    // label role, because it is not the news either.
    let mut lines = vec![style::field(
        palette,
        "#",
        &style::paint(
            palette,
            Role::Label,
            &format!("{:<price_w$}  {}", "price", "volume", price_w = price_w),
        ),
        Role::Text,
    )];
    for (rank, price, volume) in &cells {
        lines.push(style::field(
            palette,
            rank,
            &format!(
                "{}  {}",
                style::paint(
                    palette,
                    Role::Bold,
                    &format!("{price:<price_w$}", price_w = price_w)
                ),
                volume
            ),
            Role::Text,
        ));
    }
    // The addresses, one per rank, whole. The rank is repeated in front of each because this list
    // and the table above it are two blocks of rows and nothing else ties a line here to a line
    // there.
    for (index, o) in sorted.iter().enumerate() {
        lines.push(style::field(
            palette,
            if index == 0 { "contracts" } else { "" },
            &format!(
                "{:<rank_w$}  {}",
                index + 1,
                style::paint(
                    palette,
                    Role::Id,
                    &dexdo_core::address::display_self_dapp(&o.token_contract)
                ),
                rank_w = rank_w,
            ),
            Role::Text,
        ));
    }
    if let (Some(ticks), Some(cap)) = (your_order_ticks, max_price_per_tick) {
        lines.push(style::field_wrapped(
            palette,
            "order",
            &format!(
                "{ticks} ticks (= {} tokens) at up to {} SHELL / tick - fills the best ask within \
                 the limit",
                human_tokens(ticks.saturating_mul(tick_size)),
                style::shell(cap),
            ),
            Role::Text,
        ));
    }
    lines
}

pub(crate) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (review): write the `DEXDO_PN_POOL` (carries note owner secret keys) privately + atomically --
/// an exclusive 0600 temp in the destination directory, then `rename` over the target. A plain `fs::write`
/// inherits the umask, and a predictable non-exclusive temp path can clobber a pre-created file/symlink.
pub(crate) fn write_pool_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    crate::cli::note::write_private_atomic(path, bytes)
}

// The only caller is the temp-clobber regression, so the seam exists exactly where it is used.
#[cfg(test)]
pub(crate) fn write_pool_private_via_temp(
    path: &std::path::Path,
    tmp: &std::path::Path,
    bytes: &[u8],
) -> Result<()> {
    crate::cli::note::write_private_atomic_via_temp(path, tmp, bytes)
}

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

pub(crate) fn note_deploy_multisig_secret_hex(
    args: &NoteDeployArgs,
) -> Result<(&'static str, String)> {
    multisig_secret_hex(&args.multisig_private_key, &args.multisig_seed_file)
}

/// The `--multisig-private-key` / `--multisig-seed-file` pair, read once for every command that spends from
/// the funding wallet. Taken as the two option paths rather than one command's args struct so a
/// second such command reuses this reading instead of growing its own: two readings of one operator
/// secret is two places for the "which flag wins" answer to differ.
pub(crate) fn multisig_secret_hex(
    multisig_private_key: &Option<std::path::PathBuf>,
    multisig_seed_file: &Option<std::path::PathBuf>,
) -> Result<(&'static str, String)> {
    match (multisig_private_key, multisig_seed_file) {
        (Some(_), Some(_)) => {
            bail!("use only one of --multisig-private-key or --multisig-seed-file")
        }
        (Some(path), None) => Ok((
            "--multisig-private-key",
            read_secret_hex(path, "--multisig-private-key")?,
        )),
        (None, Some(path)) => {
            // A seed phrase derives the key and is not less of a secret than the hex one guarded on
            // the line above; leaving it unchecked here would be's own asymmetry, one branch
            // apart.
            crate::cli::support::refuse_exposed_secret_file(path, "--multisig-seed-file")?;
            let phrase = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("read --multisig-seed-file {}: {e}", path.display())
            })?;
            if phrase.split_whitespace().next().is_none() {
                bail!("--multisig-seed-file {} is empty", path.display());
            }
            let key = dexdo::wallet_seed::derive_multisig_private_key_from_seed_phrase(&phrase)
                .map_err(|e| anyhow::anyhow!("--multisig-seed-file {}: {e}", path.display()))?;
            Ok(("--multisig-seed-file", key.secret_hex().to_string()))
        }
        (None, None) => bail!("one of --multisig-private-key or --multisig-seed-file is required"),
    }
}

pub(crate) fn note_deploy_now_unix() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

pub(crate) fn note_deploy_fold_state_into_pool(
    pool_path: &std::path::Path,
    state: &crate::cli::note::OnboardPnState,
) -> Result<usize> {
    with_pool_write_lock(pool_path, |pool_path| {
        note_deploy_fold_state_into_pool_locked(pool_path, state, || {})
    })
}

pub(crate) fn note_deploy_fold_state_into_pool_locked(
    pool_path: &std::path::Path,
    state: &crate::cli::note::OnboardPnState,
    after_read: impl FnOnce(),
) -> Result<usize> {
    use crate::cli::note::{pn_state_to_pool_note, pool_with_note_added};

    let note = pn_state_to_pool_note(state)?;
    // this reads the existing pool -- every note's owner secret -- before folding a new note
    // in. A pool that does not exist yet is the first-note case and stays fine.
    crate::cli::support::refuse_exposed_secret_file_if_present(pool_path, "--pool")?;
    let existing = match std::fs::read(pool_path) {
        Ok(b) => Some(serde_json::from_slice(&b).map_err(|e| {
            anyhow::anyhow!("--pool {} is not valid JSON: {e}", pool_path.display())
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => bail!("read --pool {}: {e}", pool_path.display()),
    };
    after_read();
    let now = note_deploy_now_unix()?;
    let pool = pool_with_note_added(existing, state, note, now)?;
    let pool_json = serde_json::to_string_pretty(&pool)?;
    write_pool_private(pool_path, pool_json.as_bytes())?;
    Ok(pool["notes"].as_array().map(|a| a.len()).unwrap_or(0))
}

pub(crate) fn now_unix_secs() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

#[cfg(test)]
mod actionable_error_tests {
    use super::*;

    #[tokio::test]
    async fn doctor_missing_contracts_manifest_names_path_and_fix() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("manifest/deployed.manifest.json");

        let error = chain_doctor_report("net-a", None, &missing, None, |_, _| {})
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains(&missing.display().to_string()));
        assert!(error.contains("run `dexdo doctor` from the repository root"));
        assert!(error.contains("DEXDO_MANIFEST"));
        assert!(!error.starts_with("No such file or directory"));
    }

    #[test]
    fn market_missing_note_getter_is_actionable_but_other_errors_pass_through() {
        let note = "0:0000000000000000000000000000000000000000000000000000000000000001";
        let mapped = market_note_getter_error(
            note,
            "https://net-a.example",
            anyhow::anyhow!("note is not active"),
        )
        .to_string();
        // the actionable error names the note canonically; a PrivateNote is a contract of the
        // shared dexdo DApp, so its DApp half is `DEXDO_DAPP_ID`.
        let note_rendered = format!(
            "{}::{}",
            dexdo_core::DEXDO_DAPP_ID,
            note.strip_prefix("0:").expect("fixture is the chain form")
        );
        let expected = format!(
            "note {note_rendered} not found or not initialized on https://net-a.example; verify \
             `--note-addr` and the chain endpoint"
        );
        assert_eq!(mapped, expected);

        let exit_60_expected = format!(
            "market lookup failed for note {note_rendered} on https://net-a.example \
             (getInferenceOrderBookAddress exit 60) -- verify the note address is a deployed, \
             initialized order-book note"
        );
        let exit_60 = anyhow::anyhow!(
            "run_tvm getter getInferenceOrderBookAddress: Contract execution was terminated with \
             error: Unknown error, exit code: 60 (Contract has no fallback function but function ID \
             is wrong)"
        );
        assert_eq!(
            market_note_getter_error(note, "https://net-a.example", exit_60).to_string(),
            exit_60_expected
        );

        let exit_600_message = "run_tvm getter getInferenceOrderBookAddress: Contract execution was \
            terminated with error: Unknown error, exit code: 600 (Contract has no fallback function \
            but function ID is wrong)";
        assert_eq!(
            market_note_getter_error(
                note,
                "https://net-a.example",
                anyhow::anyhow!(exit_600_message)
            )
            .to_string(),
            exit_600_message
        );

        let exit_601_message = "run_tvm getter getInferenceOrderBookAddress: Contract execution was \
            terminated with error: Unknown error, exit code: 601 (Contract has no fallback function \
            but function ID is wrong)";
        assert_eq!(
            market_note_getter_error(
                note,
                "https://net-a.example",
                anyhow::anyhow!(exit_601_message)
            )
            .to_string(),
            exit_601_message
        );

        let exit_160_message = "run_tvm getter getInferenceOrderBookAddress: Contract execution was \
            terminated with error: Unknown error, exit code: 160 (Contract has no fallback function \
            but function ID is wrong)";
        assert_eq!(
            market_note_getter_error(
                note,
                "https://net-a.example",
                anyhow::anyhow!(exit_160_message)
            )
            .to_string(),
            exit_160_message
        );

        let different = anyhow::anyhow!(
            "run_tvm getter getInferenceOrderBookAddress: transport connection refused"
        );
        assert_eq!(
            market_note_getter_error(note, "https://net-a.example", different).to_string(),
            "run_tvm getter getInferenceOrderBookAddress: transport connection refused"
        );
    }
}

/// checks that must run in the default build too: the remote PR gate does not compile
/// the removed chain feature, and these are about what the CLI prints, not about the chain.
#[cfg(test)]
mod printed_command_tests {
    use super::*;
    use clap::Parser as _;

    /// The book lays out inside the window, and the address it prints is the whole address.

    /// Both halves of this were broken by the same old shape. The `tokenContract` sat in a cell of a
    /// box-drawing table, and the canonical address is 130 characters -- 64, `::`, 64 -- so the
    /// table drew itself 170 columns wide against the 120 `spec.md` lays out for, and any narrower
    /// window folded the borders through the middle of the rows. The obvious fix, shortening the
    /// address to fit, is the one thing this output may not do: it is printed to be copied, and a
    /// shortened address cannot be. So the rows are as wide as a row and the addresses go under
    /// them, one per rank, each on a line of its own.

    /// Rendered without colour, which is what a pipe and `NO_COLOR` get, so the assertions read
    /// columns rather than escape sequences.
    #[test]
    fn the_book_fits_the_window_and_the_address_stays_whole() {
        let address = format!("0:{}", "a".repeat(64));
        let whole = dexdo_core::address::display_self_dapp(&address);
        let rows = [
            BookRow {
                price_per_tick: dexdo_core::params::SHELL_UNIT * 2,
                max_ticks: 40,
                token_contract: address.clone(),
            },
            BookRow {
                price_per_tick: dexdo_core::params::SHELL_UNIT * 5,
                max_ticks: 12,
                token_contract: address.clone(),
            },
        ];
        let borrowed: Vec<&BookRow> = rows.iter().collect();
        let lines = book_rows_lines(
            crate::cli::style::Palette::None,
            &borrowed,
            Some(dexdo_core::params::SHELL_UNIT * 3),
            Some(2),
            DobParams::canonical().tick_size as u128,
        );
        let shown = lines.join("\n");

        assert!(
            !shown.contains('\u{2502}') && !shown.contains('\u{2500}'),
            "the frame is gone: {shown}"
        );
        assert_eq!(
            shown.matches(whole.as_str()).count(),
            2,
            "each ask prints its address once, and whole: {shown}"
        );
        // Every line that is not an address line lays out inside the window. The address lines are
        // the deliberate exception -- 130 characters cannot be made to fit anything narrower
        // without cutting them, and a terminal fold still copies as one string.
        for line in lines.iter().flat_map(|line| line.lines()) {
            if line.contains(whole.as_str()) {
                continue;
            }
            assert!(
                line.chars().count() <= crate::cli::style::window_columns(),
                "a row wider than the window: {} columns in {line}",
                line.chars().count()
            );
        }
        // Amounts read as a person says them, and the ceiling is stated once, under the rows.
        assert!(shown.contains("2.00 SHELL"), "{shown}");
        assert!(shown.contains("40 ticks"), "{shown}");
        assert!(
            shown.contains("at up to 3.00 SHELL / tick"),
            "the ceiling belongs under the book, not in a column of its own: {shown}"
        );
    }

    #[test]
    fn provision_mainnet_profile_keeps_manifest_endpoint() {
        let manifest: dexdo_core::Deployed = serde_json::from_value(serde_json::json!({
            "network": "mainnet",
            "endpoint": "https://net-b.example",
            "superroot": format!("0:{}", "0".repeat(64)),
            "dapp_config": "",
            "dapp_id": "0".repeat(64)
        }))
        .expect("mainnet manifest fixture");

        assert_eq!(
            manifest_preflight_endpoint(&manifest, None).expect("resolve provision endpoint"),
            "https://net-b.example"
        );
        assert_eq!(
            manifest_preflight_endpoint(
                &manifest,
                Some("https://explicit-mainnet.example/graphql")
            )
            .expect("resolve explicit provision endpoint"),
            "https://explicit-mainnet.example"
        );
    }

    /// `close` guidance must be guidance and nothing more. `close` signs, so its handler
    /// demands a `--note-key` this site does not have; an argv template filling that gap with
    /// `<buyer-key>` is not even argv, because the shell consumes `<buyer-key>` as a redirection.
    /// So every command span here has to be a bare command path, and every input the handler
    /// enforces below clap -- including the `--role`/`--note-addr` a raw `TokenContract` target
    /// lacks, named through the handler's own `require_close_target_identity` -- has to be stated
    /// in the prose around it. Both target modes are driven, with a deal reference a shell would
    /// otherwise split.
    #[test]
    fn close_guidance_names_the_command_and_states_every_input_its_handler_demands() {
        use crate::cli::support::printed_commands::assert_emitted_commands_name_only;
        let deals_dir = std::path::Path::new("/tmp/my deals");
        for (raw_role, deal, actor) in [
            (None, "seller-0:33 with space", "seller"),
            (Some("seller"), "0:33", "seller"),
            (Some("buyer"), "0:33", "buyer"),
        ] {
            let guidance = close_guidance(deal, raw_role, actor, Some(deals_dir));
            // `close` signs, so the key must be stated; and the options this run was given must
            // survive into the follow-up, or it silently resolves a different handle directory and
            // a different deployment. Stating them through the helper is what makes the guarantee
            // structural rather than something this call site remembered to check.
            assert_emitted_commands_name_only(
                &guidance,
                &format!("close guidance (raw_role={raw_role:?})"),
                &[
                    &format!("{actor} --note-key"),
                    "--deals-dir '/tmp/my deals'",
                ],
            );
            assert!(
                guidance.contains(&crate::cli::support::shell_arg(deal)),
                "the deal reference must be quoted so a shell keeps it whole: {guidance}"
            );
            // Exactly what `load_deal_target` carries for a raw TokenContract: without a stored
            // handle the role and note come from flags, and the handler rejects their absence.
            // The two rejections are read from the handler's own function, one input at a time,
            // so this states what `close` demands rather than restating it.
            let missing_role =
                require_close_target_identity::<deals::DealHandleRole>(deal, None, None)
                    .expect_err("the handler must demand a role for a raw TokenContract")
                    .to_string();
            let missing_note =
                require_close_target_identity(deal, Some(deals::DealHandleRole::Buyer), None)
                    .expect_err("the handler must demand a note for a raw TokenContract")
                    .to_string();
            if raw_role.is_some() {
                for demanded in [missing_role.as_str(), missing_note.as_str()] {
                    let flag = if demanded.contains("--role") {
                        "--role"
                    } else {
                        "--note-addr"
                    };
                    assert!(
                        guidance.contains(flag),
                        "the handler demands {flag} but the guidance omits it: {guidance}"
                    );
                }
            } else {
                assert!(
                    !guidance.contains("--role"),
                    "a stored handle carries the role; guidance must not ask for it: {guidance}"
                );
            }
        }
    }

    /// `dexdo status` is the one close follow-up that *is* a complete line -- it reads, so it
    /// needs neither a key nor a role -- and it must survive the operator's shell with the deal
    /// reference intact and the run's own manifest/handle directory attached.
    #[test]
    fn printed_status_line_survives_a_shell_and_keeps_this_run_s_options() {
        use crate::cli::support::printed_commands::assert_emitted_commands_parse;
        let line = status_command(
            "seller-0:33 with space",
            Some(std::path::Path::new("/tmp/my deals")),
        );
        assert_emitted_commands_parse(
            &format!("inspect with `{line}`"),
            "close status hint",
            false,
        );
        let parsed = crate::Cli::try_parse_from(
            crate::cli::support::printed_commands::shell_split(&line)
                .expect("the printed status line is one a shell accepts"),
        )
        .unwrap_or_else(|e| panic!("the printed line must parse: {line}\n{e}"));
        let crate::Command::Status(args) = parsed.command else {
            panic!("status command");
        };
        assert_eq!(args.deal, "seller-0:33 with space");
        assert_eq!(
            args.deals_dir.as_deref(),
            Some(std::path::Path::new("/tmp/my deals"))
        );
    }

    /// the dispute follow-up names a command whose handler demands a seller note and owner
    /// key *after* clap accepts the line. Neither is known where it is printed, so it must be prose
    /// naming the command.
    #[test]
    fn settlement_guidance_names_its_command_and_no_flag_that_is_gone() {
        use crate::cli::support::{
            printed_commands::assert_emitted_commands_name_only, release_dispute_guidance,
        };
        let guidance = release_dispute_guidance("0:33");
        assert!(
            !guidance.contains("--contracts"),
            "a pasted line may not offer a flag that no longer parses: {guidance}"
        );
        assert_emitted_commands_name_only(
            &guidance,
            "settlement guidance",
            &["--token-contract", "--note-addr", "--note-key"],
        );
    }
}
