//! Tests for the shared Hot check-and-fund mechanism.

//! Every one of these drives the real entry point [`ensure_hot_funded`] through the two seams a
//! real run uses - a balance reader and a provider - rather than calling an internal helper or
//! writing an end state by hand. A test that fabricated the state it then asserts on would prove a
//! path no command can reach.

use std::cell::{Cell, RefCell};

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

const SHELL: u32 = 2;

fn hex64(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

/// A Hot as the wallet hands one over: a self-DApp multisig, whose DApp half is its own account id
/// and is therefore NOT the dexdo DApp.
fn self_dapp_hot() -> String {
    format!("{}::{}", hex64(0xa1), hex64(0xa1))
}

fn vault_address() -> String {
    format!("{}::{}", hex64(0xb2), hex64(0xb2))
}

fn binding(provider: WalletProvider) -> WalletBinding {
    WalletBinding {
        provider,
        network: "net-a".to_string(),
        hot_address: self_dapp_hot(),
        vault_address: provider.creates_vault_request().then(vault_address),
    }
}

fn requirements() -> FundingRequirements {
    FundingRequirements::new([(SHELL, 1_000u128)])
}

fn tight_bounds() -> FundingWaitBounds {
    FundingWaitBounds {
        timeout: Duration::ZERO,
        poll: Duration::from_millis(1),
        lock_timeout: Duration::from_millis(200),
        lock_poll: Duration::from_millis(1),
    }
}

fn patient_bounds() -> FundingWaitBounds {
    FundingWaitBounds {
        timeout: Duration::from_secs(30),
        poll: Duration::from_millis(1),
        lock_timeout: Duration::from_millis(200),
        lock_poll: Duration::from_millis(1),
    }
}

/// A balance reader that serves a scripted sequence and counts how often it was asked.
struct FakeChain {
    answers: RefCell<Vec<Result<HotBalances, String>>>,
    last: RefCell<Result<HotBalances, String>>,
    reads: Cell<usize>,
}

impl FakeChain {
    fn always(balance: u128) -> Self {
        Self {
            answers: RefCell::new(Vec::new()),
            last: RefCell::new(Ok(HotBalances::new(
                vault_to_hot_native_value(),
                [(SHELL, balance)],
            ))),
            reads: Cell::new(0),
        }
    }

    fn then_always(mut answers: Vec<u128>, last: u128) -> Self {
        answers.reverse();
        Self {
            answers: RefCell::new(
                answers
                    .into_iter()
                    .map(|balance| {
                        Ok(HotBalances::new(
                            vault_to_hot_native_value(),
                            [(SHELL, balance)],
                        ))
                    })
                    .collect(),
            ),
            last: RefCell::new(Ok(HotBalances::new(
                vault_to_hot_native_value(),
                [(SHELL, last)],
            ))),
            reads: Cell::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for FakeChain {
    async fn hot_balances(&self, _hot: &CanonicalAddress) -> Result<HotBalances> {
        self.reads.set(self.reads.get() + 1);
        let next = self.answers.borrow_mut().pop();
        match next.unwrap_or_else(|| self.last.borrow().clone()) {
            Ok(balances) => Ok(balances),
            Err(reason) => Err(anyhow!("{reason}")),
        }
    }
}

/// A provider that answers a scripted probe, records every request it is handed, and counts the
/// submits it was asked to make.
struct FakeProvider {
    provider: WalletProvider,
    probe: RefCell<Vec<RequestPresence>>,
    probe_default: RequestPresence,
    submit: RefCell<Vec<SubmitOutcome>>,
    submits: Cell<usize>,
    probes: Cell<usize>,
    seen: RefCell<Vec<FundingRequest>>,
}

impl FakeProvider {
    fn new(provider: WalletProvider, probe_default: RequestPresence) -> Self {
        Self {
            provider,
            probe: RefCell::new(Vec::new()),
            probe_default,
            submit: RefCell::new(Vec::new()),
            submits: Cell::new(0),
            probes: Cell::new(0),
            seen: RefCell::new(Vec::new()),
        }
    }

    fn ackinacki(probe_default: RequestPresence) -> Self {
        Self::new(WalletProvider::AckinackiWallet, probe_default)
    }

    fn with_submit(self, outcomes: Vec<SubmitOutcome>) -> Self {
        let mut reversed = outcomes;
        reversed.reverse();
        *self.submit.borrow_mut() = reversed;
        self
    }
}

#[async_trait::async_trait(?Send)]
impl HotFundingProvider for FakeProvider {
    fn provider(&self) -> WalletProvider {
        self.provider
    }

    async fn probe_existing_request(&self, request: &FundingRequest) -> Result<RequestPresence> {
        self.probes.set(self.probes.get() + 1);
        self.seen.borrow_mut().push(request.clone());
        let next = self.probe.borrow_mut().pop();
        Ok(next.unwrap_or_else(|| self.probe_default.clone()))
    }

    async fn create_request(&self, request: &FundingRequest) -> Result<SubmitOutcome> {
        self.submits.set(self.submits.get() + 1);
        self.seen.borrow_mut().push(request.clone());
        Ok(self
            .submit
            .borrow_mut()
            .pop()
            .unwrap_or(SubmitOutcome::Accepted {
                transaction_hash: Some("tx".to_string()),
                pending_transaction_id: Some("pending".to_string()),
            }))
    }

    fn manual_instruction(&self, request: &FundingRequest) -> String {
        format!("top up {} yourself", request.hot_address)
    }
}

async fn run(
    dir: &Path,
    binding: &WalletBinding,
    chain: &FakeChain,
    provider: &FakeProvider,
    bounds: FundingWaitBounds,
) -> Result<FundedHot> {
    ensure_hot_funded(
        &HotFundingContext {
            binding,
            requirements: &requirements(),
            operation: "note deploy",
            creator_pubkey: "creator-pubkey",
            data_dir: dir,
            bounds,
        },
        chain,
        provider,
    )
    .await
}

fn record(dir: &Path) -> Option<FundingJournalRecord> {
    load_funding_journal(dir, "net-a", &self_dapp_hot()).expect("read journal")
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp data dir")
}

fn account_response(native: u128, shell: u128) -> String {
    format!(
        r#"{{"data":{{"blockchain":{{"account":{{"info":{{"acc_type_name":"Active","boc":null,"code_hash":"abc","balance":"0x{native:x}","balance_other":[{{"currency":2.0,"value":"0x{shell:x}"}}]}}}}}}}}}}"#
    )
}

async fn serve_account_responses(
    responses: Vec<(&'static str, String)>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind account fixture");
    let endpoint = format!("http://{}", listener.local_addr().expect("fixture address"));
    let reads = Arc::new(AtomicUsize::new(0));
    let task_reads = Arc::clone(&reads);
    let task = tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.expect("accept account POST");
            task_reads.fetch_add(1, Ordering::SeqCst);
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read account POST");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write account response");
        }
    });
    (endpoint, reads, task)
}

async fn run_with_real_reader(
    dir: &Path,
    client: &dexdo_core::ChainClient,
    provider: &FakeProvider,
) -> Result<FundedHot> {
    ensure_hot_funded(
        &HotFundingContext {
            binding: &binding(WalletProvider::AckinackiWallet),
            requirements: &requirements(),
            operation: "note deploy",
            creator_pubkey: "creator-pubkey",
            data_dir: dir,
            bounds: tight_bounds(),
        },
        client,
        provider,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Journal key and paths
// ---------------------------------------------------------------------------------------------

#[test]
fn journal_path_is_one_file_per_hot_under_the_data_directory() {
    let dir = temp();
    let path = funding_journal_path(dir.path(), "net-a", &self_dapp_hot());
    assert_eq!(
        path.parent().expect("parent"),
        dir.path().join("wallet").join("funding-requests")
    );
    let name = path
        .file_name()
        .expect("name")
        .to_string_lossy()
        .to_string();
    assert_eq!(name.len(), 64 + ".json".len());
    assert!(name.ends_with(".json"));
}

#[test]
fn journal_key_separates_networks_that_would_otherwise_share_a_file() {
    // A bare concatenation of the two inputs is not injective: "shell" + "net<addr>" and
    // "net-a" + "<addr>" would produce the same bytes and therefore the same file for two
    // different Hots. One file per Hot is the property the key is relied on for.
    let hot = self_dapp_hot();
    assert_ne!(
        funding_journal_key("net-a", &hot),
        funding_journal_key("shell", &format!("net{hot}"))
    );
    assert_ne!(
        funding_journal_key("net-a", &hot),
        funding_journal_key("mainnet", &hot)
    );
    assert_eq!(
        funding_journal_key("net-a", &hot),
        funding_journal_key("net-a", &hot)
    );
}

#[test]
fn the_lock_and_the_journal_name_the_same_hot() {
    let dir = temp();
    let hot = self_dapp_hot();
    let journal = funding_journal_path(dir.path(), "net-a", &hot);
    let lock = hot_lock_path(dir.path(), "net-a", &hot);
    assert_eq!(journal.parent(), lock.parent());
    assert_eq!(
        journal.file_stem().expect("journal stem"),
        lock.file_stem().expect("lock stem"),
        "one Hot must not be locked under one key and journalled under another"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_journal_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp();
    let chain = FakeChain::always(0);
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);
    let _ = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await;

    let path = funding_journal_path(dir.path(), "net-a", &self_dapp_hot());
    let mode = std::fs::metadata(&path)
        .expect("journal metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the funding journal must be owner-only");
    let dir_mode = std::fs::metadata(funding_requests_dir(dir.path()))
        .expect("dir metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "the funding journal directory must be owner-only"
    );
}

// ---------------------------------------------------------------------------------------------
// The already-funded path
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_hot_that_already_holds_enough_never_reaches_the_provider() {
    let dir = temp();
    let chain = FakeChain::always(5_000);
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);
    let funded = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await
    .expect("already funded");

    assert_eq!(funded.notice, FundingNotice::AlreadyFunded);
    assert_eq!(funded.observed.get(SHELL), 5_000);
    assert_eq!(provider.submits.get(), 0);
    assert_eq!(provider.probes.get(), 0);
    assert!(
        record(dir.path()).is_none(),
        "a Hot that needed nothing must not leave a funding record behind"
    );
}

// ---------------------------------------------------------------------------------------------
// Repeat safety - the property that costs money to get wrong
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_repeat_after_an_unobserved_successful_spend_does_not_spend_twice() {
    let dir = temp();
    let binding = binding(WalletProvider::AckinackiWallet);

    // Run 1: nothing on chain yet, so the request is created - and its result is never observed.
    // On chain it landed; this client does not know that.
    let chain1 = FakeChain::always(0);
    let provider1 = FakeProvider::ackinacki(RequestPresence::Absent).with_submit(vec![
        SubmitOutcome::Indeterminate {
            reason: "gateway timed out".to_string(),
        },
    ]);
    let first = run(dir.path(), &binding, &chain1, &provider1, tight_bounds()).await;
    assert!(
        first.is_err(),
        "the balance never arrived, so the wait must fail"
    );
    assert_eq!(provider1.submits.get(), 1);
    let after_first = record(dir.path()).expect("run 1 left a record");
    assert_eq!(
        after_first.state,
        FundingState::Prepared,
        "an unobserved submit must NOT be recorded as submitted"
    );

    // Run 2: the same command again. The request from run 1 IS in the queue.
    let chain2 = FakeChain::always(0);
    let provider2 = FakeProvider::ackinacki(RequestPresence::Present {
        transaction_hash: Some("tx-from-run-1".to_string()),
        pending_transaction_id: Some("pending-7".to_string()),
        chain_created_at_unix: None,
    });
    let second = run(dir.path(), &binding, &chain2, &provider2, tight_bounds()).await;
    assert!(second.is_err(), "still unfunded, so the wait still fails");

    assert_eq!(
        provider2.submits.get(),
        0,
        "a repeat must not create a second Vault request once the first is proven present"
    );
    assert_eq!(
        provider2.probes.get(),
        1,
        "the repeat must prove presence before deciding"
    );
    let after_second = record(dir.path()).expect("run 2 kept the record");
    assert_eq!(after_second.state, FundingState::Submitted);
    assert_eq!(
        after_second.transaction_hash.as_deref(),
        Some("tx-from-run-1")
    );
    assert_eq!(
        after_second.pending_transaction_id.as_deref(),
        Some("pending-7")
    );
}

#[tokio::test]
async fn an_exact_repeat_probes_the_request_the_first_run_recorded() {
    let dir = temp();
    let binding = binding(WalletProvider::AckinackiWallet);

    let chain1 = FakeChain::always(100);
    let provider1 = FakeProvider::ackinacki(RequestPresence::Absent).with_submit(vec![
        SubmitOutcome::Indeterminate {
            reason: "unknown".to_string(),
        },
    ]);
    let _ = run(dir.path(), &binding, &chain1, &provider1, tight_bounds()).await;
    let recorded_shortfall = record(dir.path()).expect("record").shortfall.clone();
    assert_eq!(recorded_shortfall.get(&SHELL), Some(&900));

    // The exact same shortfall must look for the request the earlier run may have created rather
    // than prepare another generation.
    let chain2 = FakeChain::always(100);
    let provider2 = FakeProvider::ackinacki(RequestPresence::Present {
        transaction_hash: None,
        pending_transaction_id: None,
        chain_created_at_unix: None,
    });
    let _ = run(dir.path(), &binding, &chain2, &provider2, tight_bounds()).await;

    let probed = provider2.seen.borrow();
    let probed = probed.first().expect("the repeat probed");
    assert_eq!(
        probed.shortfall.get(&SHELL),
        Some(&900),
        "the probe must describe the recorded exact request"
    );
    assert_eq!(probed.creator_pubkey, "creator-pubkey");
    assert_eq!(probed.hot_address, self_dapp_hot());
}

#[tokio::test]
async fn an_unreadable_queue_is_not_absence_and_forbids_a_second_request() {
    let dir = temp();
    let binding = binding(WalletProvider::AckinackiWallet);

    let chain1 = FakeChain::always(0);
    let provider1 = FakeProvider::ackinacki(RequestPresence::Absent).with_submit(vec![
        SubmitOutcome::Indeterminate {
            reason: "no receipt".to_string(),
        },
    ]);
    let _ = run(dir.path(), &binding, &chain1, &provider1, tight_bounds()).await;

    let chain2 = FakeChain::always(0);
    let provider2 = FakeProvider::ackinacki(RequestPresence::Unknown {
        reason: "502 from the gateway".to_string(),
    });
    let _ = run(dir.path(), &binding, &chain2, &provider2, tight_bounds()).await;

    assert_eq!(
        provider2.submits.get(),
        0,
        "a chain read that failed means unknown, and unknown must never permit a submit"
    );
    assert_eq!(
        record(dir.path()).expect("record").state,
        FundingState::Prepared,
        "an unknown probe must not advance the record either"
    );
}

#[tokio::test]
async fn an_executed_record_followed_by_contradictory_absence_never_submits() {
    let dir = temp();
    let chain = FakeChain::always(0);
    let provider = FakeProvider::ackinacki(RequestPresence::Absent).with_submit(vec![
        SubmitOutcome::Accepted {
            transaction_hash: Some("tx-7".to_string()),
            // A malformed id exercises the conservative execution fallback: it can forbid a
            // submit, but cannot authorize retirement of this generation.
            pending_transaction_id: Some("not-a-queue-id".to_string()),
        },
    ]);

    let _ = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await;
    assert_eq!(provider.submits.get(), 1);

    provider.probe.borrow_mut().push(RequestPresence::Executed {
        evidence: FundingEvidence {
            verdict: "executed".to_string(),
            source: "history fallback".to_string(),
            observed_at_unix: Some(7),
            detail: "a generation-invariant sent event".to_string(),
            delivery_message_id: None,
        },
    });
    let _ = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await;
    assert_eq!(
        record(dir.path()).expect("record").state,
        FundingState::Executed
    );

    provider.probe.borrow_mut().push(RequestPresence::Absent);
    let _ = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await;
    assert_eq!(
        provider.submits.get(),
        1,
        "an erroneous Absent verdict must not overwrite an Executed generation and submit again"
    );
    assert_eq!(
        record(dir.path()).expect("record").state,
        FundingState::Executed
    );
}

#[tokio::test]
async fn a_repeat_cannot_resume_down_a_different_providers_flow() {
    let dir = temp();

    let chain1 = FakeChain::always(0);
    let provider1 = FakeProvider::ackinacki(RequestPresence::Absent).with_submit(vec![
        SubmitOutcome::Indeterminate {
            reason: "unknown".to_string(),
        },
    ]);
    let _ = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain1,
        &provider1,
        tight_bounds(),
    )
    .await;

    // The same Hot, now bound to a provider that has no Vault. The request the first provider
    // created may still be pending, so this run has nothing it can conclude.
    let chain2 = FakeChain::always(0);
    let provider2 = FakeProvider::new(WalletProvider::GoshAi, RequestPresence::Absent);
    let error = run(
        dir.path(),
        &binding(WalletProvider::GoshAi),
        &chain2,
        &provider2,
        tight_bounds(),
    )
    .await
    .expect_err("a provider change over an open request must be refused");
    let message = format!("{error:#}");
    assert!(message.contains("ackinacki-wallet"), "{message}");
    assert!(message.contains("gosh-ai"), "{message}");
    assert_eq!(provider2.submits.get(), 0);
}

#[tokio::test]
async fn a_funding_flow_from_a_provider_the_binding_does_not_name_is_refused() {
    let dir = temp();
    let chain = FakeChain::always(0);
    let provider = FakeProvider::new(WalletProvider::Manual, RequestPresence::Absent);
    let error = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await
    .expect_err("provider mismatch");
    assert!(
        format!("{error:#}").contains("provider mismatch"),
        "{error}"
    );
    assert!(
        record(dir.path()).is_none(),
        "a refused run must not open a record"
    );
}

// ---------------------------------------------------------------------------------------------
// Timeout and cancel
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_timeout_leaves_a_state_that_a_rerun_re_checks() {
    let dir = temp();
    let binding = binding(WalletProvider::AckinackiWallet);

    let chain1 = FakeChain::always(0);
    let provider1 = FakeProvider::ackinacki(RequestPresence::Absent).with_submit(vec![
        SubmitOutcome::Accepted {
            transaction_hash: Some("tx".to_string()),
            pending_transaction_id: Some("7".to_string()),
        },
    ]);
    let error = run(dir.path(), &binding, &chain1, &provider1, tight_bounds())
        .await
        .expect_err("the balance never arrived");
    let message = format!("{error:#}");
    assert!(message.contains("timed out"), "{message}");
    // rewrote the wording; what is pinned is the MEANING, which is what these two lines were
    // ever for: the operator is told to run the command again, and told that nothing pending was
    // cancelled -- the clause that exists because somebody read a timeout as a cancellation and
    // sent a second transfer.
    let lower = message.to_ascii_lowercase();
    assert!(
        lower.contains("run the same command again") || lower.contains("re-run the same command"),
        "the operator must be told to repeat the command: {message}"
    );
    assert!(
        lower.contains("nothing was cancelled"),
        "the operator must be told nothing pending was cancelled: {message}"
    );
    let after_timeout = record(dir.path()).expect("the timeout kept the record");
    assert_eq!(after_timeout.state, FundingState::Submitted);

    // No lock survives the timeout: the next run must not have to wait on a corpse.
    let lock = acquire_hot_lock(
        dir.path(),
        "net-a",
        &self_dapp_hot(),
        Duration::from_millis(50),
        Duration::from_millis(1),
    )
    .expect("a timed-out run leaves no held lock");
    drop(lock);

    // The rerun re-reads the balance and continues from the same state.
    let chain2 = FakeChain::always(1_000);
    let provider2 = FakeProvider::ackinacki(RequestPresence::Executed {
        evidence: FundingEvidence {
            verdict: "executed".to_string(),
            source: "finalized TransactionSent".to_string(),
            observed_at_unix: Some(1),
            detail: "the timed-out request executed".to_string(),
            delivery_message_id: None,
        },
    });
    let funded = run(dir.path(), &binding, &chain2, &provider2, patient_bounds())
        .await
        .expect("the rerun sees the funded Hot");
    assert!(
        chain2.reads.get() >= 1,
        "the rerun must re-check the balance"
    );
    assert_eq!(funded.observed.get(SHELL), 1_000);
    assert_eq!(provider2.submits.get(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_timeout_writes_nothing_of_its_own_into_the_record() {
    let dir = temp();
    let binding = binding(WalletProvider::AckinackiWallet);

    // One pass: arrange, then cross the bound immediately.
    let chain = FakeChain::always(0);
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);
    let _ = run(dir.path(), &binding, &chain, &provider, tight_bounds())
        .await
        .expect_err("timeout");
    let after_one_pass = record(dir.path()).expect("record");

    // Several passes over a longer bound: the extra waiting must not change the record at all.
    let dir2 = temp();
    let chain2 = FakeChain::always(0);
    let provider2 = FakeProvider::ackinacki(RequestPresence::Absent);
    // One virtual second against a one-millisecond poll drives many passes deterministically.
    // Tokio time is paused for this test, so filesystem and scheduler time cannot consume the bound.
    let bounds = FundingWaitBounds {
        timeout: Duration::from_secs(1),
        ..tight_bounds()
    };
    let _ = run(dir2.path(), &binding, &chain2, &provider2, bounds)
        .await
        .expect_err("timeout");
    let after_many_passes = load_funding_journal(dir2.path(), "net-a", &self_dapp_hot())
        .expect("read")
        .expect("record");

    assert!(
        chain2.reads.get() > 1,
        "the longer wait really did poll again"
    );
    assert_eq!(after_one_pass.state, after_many_passes.state);
    assert_eq!(after_one_pass.required, after_many_passes.required);
    assert_eq!(after_one_pass.shortfall, after_many_passes.shortfall);
    assert_eq!(
        after_one_pass.satisfied_balances, after_many_passes.satisfied_balances,
        "waiting longer is not a fact about the chain and must not move the record"
    );
}

// ---------------------------------------------------------------------------------------------
// What closes the journal
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_journal_closes_only_on_an_observed_balance_that_meets_the_requirement() {
    let dir = temp();
    let binding = binding(WalletProvider::AckinackiWallet);

    // Under the requirement for two reads, then over it.
    let chain = FakeChain::then_always(vec![0, 999], 1_000);
    let provider = FakeProvider::ackinacki(RequestPresence::Executed {
        evidence: FundingEvidence {
            verdict: "executed".to_string(),
            source: "finalized TransactionSent".to_string(),
            observed_at_unix: Some(1),
            detail: "the accepted request executed".to_string(),
            delivery_message_id: None,
        },
    })
    .with_submit(vec![SubmitOutcome::Accepted {
        transaction_hash: Some("tx".to_string()),
        pending_transaction_id: Some("7".to_string()),
    }]);
    provider.probe.borrow_mut().push(RequestPresence::Absent);
    let funded = run(dir.path(), &binding, &chain, &provider, patient_bounds())
        .await
        .expect("the balance eventually arrives");

    assert_eq!(funded.notice, FundingNotice::RequestSubmitted);
    let closed = record(dir.path()).expect("record");
    assert_eq!(closed.state, FundingState::Satisfied);
    assert_eq!(
        closed
            .satisfied_balances
            .as_ref()
            .and_then(|b| b.get(&SHELL)),
        Some(&1_000u128),
        "the record must close with the balances that were actually read"
    );
    assert!(closed.last_checked_at_unix.is_some());
    assert!(
        chain.reads.get() >= 3,
        "999 is under 1000 and must not have closed the record"
    );
}

#[tokio::test]
async fn an_accepted_submit_does_not_close_the_journal() {
    let dir = temp();
    let chain = FakeChain::always(0);
    let provider = FakeProvider::ackinacki(RequestPresence::Absent).with_submit(vec![
        SubmitOutcome::Accepted {
            transaction_hash: Some("tx".to_string()),
            pending_transaction_id: Some("p1".to_string()),
        },
    ]);
    let _ = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await
    .expect_err("the request was accepted, the money has not arrived");

    let after = record(dir.path()).expect("record");
    assert_eq!(
        after.state,
        FundingState::Submitted,
        "a provider's own answer is not proof of funding and must not close the record"
    );
    assert!(after.satisfied_balances.is_none());
}

#[tokio::test]
async fn a_chain_read_error_neither_closes_the_record_nor_counts_as_a_balance() {
    let dir = temp();
    let chain = FakeChain {
        answers: RefCell::new(Vec::new()),
        last: RefCell::new(Err("502 Bad Gateway".to_string())),
        reads: Cell::new(0),
    };
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);
    let error = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await
    .expect_err("an unreadable chain is not a funded Hot");
    assert!(format!("{error:#}").contains("502"), "{error}");
    assert!(record(dir.path()).is_none());
    assert_eq!(provider.submits.get(), 0);
}

#[tokio::test]
async fn the_production_hot_reader_retries_one_transient_account_failure() {
    let body = account_response(vault_to_hot_native_value(), 1_000);
    let (endpoint, reads, server) = serve_account_responses(vec![
        (
            "503 Service Unavailable",
            r#"{"error":"try again"}"#.to_string(),
        ),
        ("200 OK", body),
    ])
    .await;
    let client = dexdo_core::ChainClient::connect(&endpoint).expect("connect fixture client");
    let hot = CanonicalAddress::parse(&self_dapp_hot()).expect("canonical Hot");

    let balances = HotBalanceReader::hot_balances(&client, &hot)
        .await
        .expect("one transient read failure must be retried by the production Hot reader");

    server.await.expect("account fixture task");
    assert_eq!(balances.get(SHELL), 1_000);
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "the first transient response must not abort the funding read"
    );
}

#[tokio::test]
async fn an_ecc_funded_hot_requests_only_its_exact_native_shortfall() {
    let native_shortfall = 123;
    let observed_native = vault_to_hot_native_value() - native_shortfall;
    let (endpoint, reads, server) =
        serve_account_responses(vec![("200 OK", account_response(observed_native, 1_000))]).await;
    let client = dexdo_core::ChainClient::connect(&endpoint).expect("connect fixture client");
    let dir = temp();
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);

    let _ = run_with_real_reader(dir.path(), &client, &provider).await;

    server.await.expect("account fixture task");
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.submits.get(),
        1,
        "sufficient ECC does not hide a native vmshell shortfall"
    );
    let submitted = record(dir.path()).expect("native shortfall request");
    assert_eq!(submitted.fingerprint.value, native_shortfall);
    assert!(
        submitted.fingerprint.cc.is_empty(),
        "the native balance is not ECC[2] and must not invent a currency entry"
    );
}

#[tokio::test]
async fn a_native_funded_hot_requests_only_its_exact_ecc_shortfall() {
    let (endpoint, _, server) = serve_account_responses(vec![(
        "200 OK",
        account_response(vault_to_hot_native_value(), 400),
    )])
    .await;
    let client = dexdo_core::ChainClient::connect(&endpoint).expect("connect fixture client");
    let dir = temp();
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);

    let _ = run_with_real_reader(dir.path(), &client, &provider).await;

    server.await.expect("account fixture task");
    let submitted = record(dir.path()).expect("ECC shortfall request");
    assert_eq!(
        submitted.fingerprint.value, 0,
        "an already-satisfied native floor must not be transferred again"
    );
    assert_eq!(submitted.fingerprint.cc.get(&SHELL), Some(&600));
}

// ---------------------------------------------------------------------------------------------
// Providers with no request to create
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_without_a_vault_creates_no_request_and_probes_nothing() {
    for provider_kind in [WalletProvider::GoshAi, WalletProvider::Manual] {
        let dir = temp();
        let chain = FakeChain::always(0);
        let provider = FakeProvider::new(provider_kind, RequestPresence::Absent);
        let error = run(
            dir.path(),
            &binding(provider_kind),
            &chain,
            &provider,
            tight_bounds(),
        )
        .await
        .expect_err("nothing topped the Hot up");
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("timed out")),
            "{error}"
        );
        assert_eq!(provider.submits.get(), 0, "{provider_kind:?}");
        assert_eq!(provider.probes.get(), 0, "{provider_kind:?}");
        let open = record(dir.path()).expect("the open need is still recorded");
        assert_eq!(open.state, FundingState::Prepared);
        assert_eq!(open.provider, provider_kind);
        assert!(open.vault_address.is_none());
    }
}

// ---------------------------------------------------------------------------------------------
// The DApp the transfer is addressed into
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_request_carries_the_hots_own_dapp_and_not_the_dexdo_constant() {
    let dir = temp();
    let chain = FakeChain::always(0);
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);
    let _ = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await;

    let seen = provider.seen.borrow();
    let request = seen.last().expect("the provider was handed a request");
    assert_eq!(
        request.hot_dapp_id,
        hex64(0xa1),
        "a Vault -> Hot transfer is addressed into the Hot's own self-DApp"
    );
    assert_ne!(
        request.hot_dapp_id,
        dexdo_core::DEXDO_DAPP_ID,
        "addressing the transfer into the dexdo DApp would send it to an account that is not there"
    );
    assert_ne!(request.hot_dapp_id, "4");
}

// ---------------------------------------------------------------------------------------------
// Requirements arithmetic
// ---------------------------------------------------------------------------------------------

#[test]
fn shortfall_is_per_currency_and_saturates() {
    let requirements = FundingRequirements::new([(SHELL, 1_000u128), (7, 5u128)]);
    let balances = HotBalances::new(vault_to_hot_native_value(), [(SHELL, 1_200u128)]);
    let shortfall = requirements.shortfall(&balances);
    assert_eq!(
        shortfall.get(&SHELL),
        None,
        "an over-funded currency is not a shortfall"
    );
    assert_eq!(
        shortfall.get(&7),
        Some(&5),
        "an absent currency reads as zero, not as met"
    );
    assert!(!requirements.met_by(&balances));
    assert!(requirements.met_by(&HotBalances::new(
        vault_to_hot_native_value(),
        [(SHELL, 1_000u128), (7, 5u128)],
    )));
}

// ---------------------------------------------------------------------------------------------
// Serialisation of two runs against one Hot
// ---------------------------------------------------------------------------------------------

#[test]
fn concurrent_runs_against_the_same_hot_are_serialised_by_the_lock() {
    let dir = temp();
    let hot = self_dapp_hot();
    let held = acquire_hot_lock(
        dir.path(),
        "net-a",
        &hot,
        Duration::from_millis(50),
        Duration::from_millis(1),
    )
    .expect("first holder");
    assert_eq!(
        held.path(),
        hot_lock_path(dir.path(), "net-a", &hot),
        "the holder must name the Hot it actually locked"
    );

    // A second process against the SAME Hot cannot take it while the first holds it.
    let dir_path = dir.path().to_path_buf();
    let hot_for_thread = hot.clone();
    let blocked = std::thread::spawn(move || {
        acquire_hot_lock(
            &dir_path,
            "net-a",
            &hot_for_thread,
            Duration::from_millis(50),
            Duration::from_millis(1),
        )
        .map(|_| ())
    })
    .join()
    .expect("thread");
    assert!(blocked.is_err(), "two runs must not hold one Hot at once");

    // A different Hot is a different lock and is not blocked by this one.
    let other = format!("{}::{}", hex64(0xc3), hex64(0xc3));
    let other_lock = acquire_hot_lock(
        dir.path(),
        "net-a",
        &other,
        Duration::from_millis(50),
        Duration::from_millis(1),
    )
    .expect("a different Hot is a different lock");
    drop(other_lock);

    // Once released, the next run takes it.
    drop(held);
    let dir_path = dir.path().to_path_buf();
    let next = std::thread::spawn(move || {
        acquire_hot_lock(
            &dir_path,
            "net-a",
            &hot,
            Duration::from_millis(500),
            Duration::from_millis(1),
        )
        .map(|_| ())
    })
    .join()
    .expect("thread");
    assert!(
        next.is_ok(),
        "the lock must be available once the holder releases it"
    );
}

#[tokio::test]
async fn the_funded_hot_keeps_the_lock_until_the_caller_drops_it() {
    let dir = temp();
    let chain = FakeChain::always(5_000);
    let provider = FakeProvider::ackinacki(RequestPresence::Absent);
    let funded = run(
        dir.path(),
        &binding(WalletProvider::AckinackiWallet),
        &chain,
        &provider,
        tight_bounds(),
    )
    .await
    .expect("funded");

    // While the caller holds the proof of the final check, nobody else may spend this Hot.
    let dir_path = dir.path().to_path_buf();
    let hot = self_dapp_hot();
    let contended = std::thread::spawn(move || {
        acquire_hot_lock(
            &dir_path,
            "net-a",
            &hot,
            Duration::from_millis(50),
            Duration::from_millis(1),
        )
        .map(|_| ())
    })
    .join()
    .expect("thread");
    assert!(
        contended.is_err(),
        "the final check and the spend that follows it must be serialised together"
    );

    drop(funded);
    let lock = acquire_hot_lock(
        dir.path(),
        "net-a",
        &self_dapp_hot(),
        Duration::from_millis(500),
        Duration::from_millis(1),
    );
    assert!(lock.is_ok(), "dropping the proof releases the Hot");
}

// ---------------------------------------------------------------------------------------------
// Journal round-trip
// ---------------------------------------------------------------------------------------------

#[test]
fn a_journal_record_round_trips_and_carries_every_field_the_specification_names() {
    let dir = temp();
    let request = FundingRequest {
        provider: WalletProvider::AckinackiWallet,
        network: "net-a".to_string(),
        vault_address: Some(vault_address()),
        hot_address: self_dapp_hot(),
        hot_dapp_id: hex64(0xa1),
        creator_pubkey: "pubkey".to_string(),
        required: [(SHELL, 1_000u128)].into_iter().collect(),
        required_native: vault_to_hot_native_value(),
        shortfall: [(SHELL, 400u128)].into_iter().collect(),
        native_shortfall: 40,
    };
    let mut written = FundingJournalRecord::open(&request, 1_700_000_000);
    written.state = FundingState::Submitted;
    written.transaction_hash = Some("tx".to_string());
    written.pending_transaction_id = Some("pending".to_string());
    written.last_checked_at_unix = Some(1_700_000_010);
    store_funding_journal(dir.path(), &written).expect("store");

    let read = record(dir.path()).expect("record");
    assert_eq!(read, written);
    assert_eq!(read.provider, WalletProvider::AckinackiWallet);
    assert_eq!(read.network, "net-a");
    assert_eq!(
        read.vault_address.as_deref(),
        Some(vault_address().as_str())
    );
    assert_eq!(read.hot_address, self_dapp_hot());
    assert_eq!(read.creator_pubkey, "pubkey");
    assert_eq!(read.required.get(&SHELL), Some(&1_000));
    assert_eq!(read.required_native, vault_to_hot_native_value());
    assert_eq!(read.shortfall.get(&SHELL), Some(&400));
    assert_eq!(read.native_shortfall, 40);
    assert_eq!(read.created_at_unix, 1_700_000_000);

    let raw = std::fs::read_to_string(funding_journal_path(dir.path(), "net-a", &self_dapp_hot()))
        .expect("read raw");
    assert!(raw.contains("\"ackinacki-wallet\""), "{raw}");
    assert!(raw.contains("\"submitted\""), "{raw}");
    assert!(
        !raw.contains("secret") && !raw.contains("seed") && !raw.contains("phrase"),
        "the journal is non-secret by construction: {raw}"
    );
}

#[test]
fn a_record_this_client_cannot_read_is_refused_rather_than_acted_on() {
    let dir = temp();
    ensure_funding_requests_dir(dir.path()).expect("dir");
    let path = funding_journal_path(dir.path(), "net-a", &self_dapp_hot());
    std::fs::write(&path, br#"{"version":99999,"written_by":"a newer client"}"#).expect("write");
    let error = load_funding_journal(dir.path(), "net-a", &self_dapp_hot())
        .expect_err("an unreadable record must not be treated as absence");
    let message = format!("{error:#}");
    assert!(
        message.contains("version 99999"),
        "a record from a newer client must be reported as a version this client does not \
         understand, not as corrupt JSON: {message}"
    );
    assert!(
        message.contains("Do not delete it"),
        "the record may be the only local trace of a request already on chain: {message}"
    );

    std::fs::write(&path, b"not json at all").expect("write");
    assert!(
        load_funding_journal(dir.path(), "net-a", &self_dapp_hot()).is_err(),
        "an unreadable record must never read as absence, which is the state that permits a submit"
    );
}

mod issue_334_explicit_hot_regressions;
mod pr1332_retirement_regressions;
