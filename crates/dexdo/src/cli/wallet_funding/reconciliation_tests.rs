//! item 2: how a funding request leaves the Vault's queue, and what may follow each way.

//! A request vanishing from the queue does not prove whether it executed. A second submit before
//! the canonical local UTC deadline could transfer twice out of a cold Vault, so queue disappearance
//! alone must never authorize it. Expiry is handled by journal selection before reconciliation.

//! Every test here drives the real entry point [`ensure_hot_funded_with_turn`] through the real
//! production provider [`AckinackiVaultProvider`], composed exactly as a money command composes it -
//! the journal is read first and what it recorded is handed to the provider, then the mechanism runs.
//! Only the chain underneath is scripted. A test that called the provider's matcher directly, or
//! that wrote an end state by hand, would prove a path no command can reach.

use std::cell::{Cell, RefCell};

use super::providers::{
    AckinackiVaultProvider, QueuedTransfer, VaultChain, VaultQueueEvent, VaultQueueEventKind,
};
use super::*;

const SHELL: u32 = 2;
const REQUIRED: u128 = 1_000;
const QUEUED_AT: u64 = 1_000_000;

fn hex64(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn hot_address() -> String {
    format!("{}::{}", hex64(0xa1), hex64(0xa1))
}

fn vault_address() -> String {
    format!("{}::{}", hex64(0xb2), hex64(0xb2))
}

fn creator() -> String {
    hex64(0xc3)
}

fn binding() -> HotFundingBinding {
    HotFundingBinding {
        provider: WalletProvider::AckinackiWallet,
        network: "net-a".to_string(),
        hot_address: hot_address(),
        vault_address: Some(vault_address()),
    }
}

fn requirements() -> FundingRequirements {
    FundingRequirements::new([(SHELL, REQUIRED)])
}

/// One check-and-arrange pass, then give up. Every reconciliation test wants exactly one pass.
fn bounds() -> FundingWaitBounds {
    FundingWaitBounds {
        timeout: Duration::ZERO,
        poll: Duration::from_millis(1),
        lock_timeout: Duration::from_millis(200),
        lock_poll: Duration::from_millis(1),
    }
}

/// A budget wide enough for the balance to arrive mid-wait, so the CONTINUE path is the one under
/// test rather than the timeout.
fn patient_bounds() -> FundingWaitBounds {
    FundingWaitBounds {
        timeout: Duration::from_secs(30),
        ..bounds()
    }
}

/// The transfer the first run creates, as the Vault's queue would report it back.
fn queued(id: u64, native: u128, shortfall: u128) -> QueuedTransfer {
    QueuedTransfer {
        id,
        creator_pubkey: Some(creator()),
        dest: hot_address(),
        value: native,
        cc: [(SHELL, shortfall)].into_iter().collect(),
        send_flags: VAULT_TO_HOT_SEND_FLAGS,
        bounce: VAULT_TO_HOT_BOUNCE,
        dapp_id: hex64(0xa1),
        payload: None,
    }
}

fn submitted_event(id: u64, native: u128, at: u64) -> VaultQueueEvent {
    VaultQueueEvent {
        kind: VaultQueueEventKind::Submitted,
        transaction_id: id,
        dest: hot_address(),
        value: native,
        dapp_id: hex64(0xa1),
        message_id: format!("msg-submitted-{id}"),
        created_at: at,
    }
}

fn sent_event(id: u64, native: u128, at: u64) -> VaultQueueEvent {
    VaultQueueEvent {
        kind: VaultQueueEventKind::Sent,
        transaction_id: id,
        dest: hot_address(),
        value: native,
        dapp_id: hex64(0xa1),
        message_id: format!("msg-sent-{id}"),
        created_at: at,
    }
}

/// A scripted Vault. Every answer is a fact some real chain could return; nothing here decides.
struct FakeVault {
    queue: RefCell<Vec<QueuedTransfer>>,
    history: RefCell<Vec<VaultQueueEvent>>,
    queue_error: RefCell<Option<String>>,
    history_error: RefCell<Option<String>>,
    submits: Cell<usize>,
    submitted: RefCell<Vec<FundingFingerprint>>,
    next_id: Cell<u64>,
    /// Optional malformed queue id reported by a submit receipt.
    reported_pending_id: RefCell<Option<String>>,
    /// When set, the submit's outcome cannot be established.
    indeterminate: Cell<bool>,
}

impl FakeVault {
    fn empty() -> Self {
        Self {
            queue: RefCell::new(Vec::new()),
            history: RefCell::new(Vec::new()),
            queue_error: RefCell::new(None),
            history_error: RefCell::new(None),
            submits: Cell::new(0),
            submitted: RefCell::new(Vec::new()),
            next_id: Cell::new(7),
            reported_pending_id: RefCell::new(None),
            indeterminate: Cell::new(false),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl VaultChain for &FakeVault {
    async fn queue(&self) -> Result<Vec<QueuedTransfer>> {
        if let Some(error) = self.queue_error.borrow().as_ref() {
            bail!("{error}");
        }
        Ok(self.queue.borrow().clone())
    }

    async fn history(&self) -> Result<Vec<VaultQueueEvent>> {
        if let Some(error) = self.history_error.borrow().as_ref() {
            bail!("{error}");
        }
        Ok(self.history.borrow().clone())
    }

    /// A real Vault's executing transaction emits the `TransactionSent` event and the internal
    /// transfer together, so a chain that has the event can also name the sibling that carried the
    /// money. Every execution scripted in this file is one of those, so this fake names it too.
    async fn delivery_message_id(
        &self,
        sent_event_message_id: &str,
        _destination: &str,
        _destination_dapp_id: &str,
    ) -> Result<Option<String>> {
        Ok(Some(format!("delivery-of-{sent_event_message_id}")))
    }

    async fn submit(&self, fingerprint: &FundingFingerprint) -> Result<SubmitOutcome> {
        self.submits.set(self.submits.get() + 1);
        self.submitted.borrow_mut().push(fingerprint.clone());
        if self.indeterminate.get() {
            return Ok(SubmitOutcome::Indeterminate {
                reason: "no receipt".to_string(),
            });
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        // A real Vault takes the request into its queue and records that it did.
        let shortfall = fingerprint.cc.get(&SHELL).copied().unwrap_or_default();
        self.queue
            .borrow_mut()
            .push(queued(id, fingerprint.value, shortfall));
        self.history
            .borrow_mut()
            .push(submitted_event(id, fingerprint.value, QUEUED_AT));
        Ok(SubmitOutcome::Accepted {
            transaction_hash: Some(format!("tx-{id}")),
            pending_transaction_id: Some(
                self.reported_pending_id
                    .borrow()
                    .clone()
                    .unwrap_or_else(|| id.to_string()),
            ),
        })
    }
}

/// A Hot whose balance is whatever the test says it is.
struct FakeHot {
    balances: RefCell<Vec<u128>>,
    last: Cell<u128>,
    native: Cell<u128>,
    reads: Cell<usize>,
}

impl FakeHot {
    fn always(balance: u128) -> Self {
        Self {
            balances: RefCell::new(Vec::new()),
            last: Cell::new(balance),
            native: Cell::new(vault_to_hot_native_value()),
            reads: Cell::new(0),
        }
    }

    fn then_always(mut first: Vec<u128>, last: u128) -> Self {
        first.reverse();
        Self {
            balances: RefCell::new(first),
            last: Cell::new(last),
            native: Cell::new(vault_to_hot_native_value()),
            reads: Cell::new(0),
        }
    }

    fn with_balances(native: u128, shell: u128) -> Self {
        Self {
            balances: RefCell::new(Vec::new()),
            last: Cell::new(shell),
            native: Cell::new(native),
            reads: Cell::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for FakeHot {
    async fn hot_balances(&self, _hot: &CanonicalAddress) -> Result<HotBalances> {
        self.reads.set(self.reads.get() + 1);
        let balance = self
            .balances
            .borrow_mut()
            .pop()
            .unwrap_or_else(|| self.last.get());
        Ok(HotBalances::new(self.native.get(), [(SHELL, balance)]))
    }
}

fn record_of(dir: &Path) -> Option<FundingJournalRecord> {
    load_funding_journal(dir, "net-a", &hot_address()).expect("read journal")
}

fn records_of(dir: &Path) -> Vec<FundingJournalRecord> {
    load_funding_journal_records(dir, "net-a", &hot_address())
        .expect("read journal")
        .unwrap_or_default()
}

/// One run of a money command's funding step, composed exactly as production composes it: read the
/// journal under the held turn, hand what it recorded to the provider, then run the mechanism.
async fn money_command_run(dir: &Path, vault: &FakeVault, hot: &FakeHot) -> Result<FundedHot> {
    money_command_run_with(dir, vault, hot, bounds()).await
}

async fn money_command_run_with(
    dir: &Path,
    vault: &FakeVault,
    hot: &FakeHot,
    bounds: FundingWaitBounds,
) -> Result<FundedHot> {
    money_command_run_with_requirements(dir, vault, hot, &requirements(), bounds).await
}

async fn money_command_run_with_requirements(
    dir: &Path,
    vault: &FakeVault,
    hot: &FakeHot,
    requirements: &FundingRequirements,
    bounds: FundingWaitBounds,
) -> Result<FundedHot> {
    let recorded = record_of(dir)
        .filter(FundingJournalRecord::is_open)
        .map(|record| record.recorded_request());
    let provider = AckinackiVaultProvider::new(vault, recorded);
    let binding = binding();
    ensure_hot_funded_with_turn(
        &HotFundingContext {
            binding: &binding,
            requirements,
            operation: "note deploy",
            creator_pubkey: &creator(),
            data_dir: dir,
            bounds,
        },
        HotTurn::AcquireOwn,
        hot,
        &provider,
    )
    .await
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp data dir")
}

/// another checkout can create the exact transfer before this data directory has a journal.
/// The first local run must adopt that queue entry rather than submit an indistinguishable second
/// transfer.
#[tokio::test]
async fn an_external_exact_request_with_two_minutes_left_keeps_its_real_deadline() {
    let dir = temp();
    let vault = FakeVault::empty();
    let now = unix_now_secs();
    let lifetime = dexdo_core::params::VAULT_FUNDING_REQUEST_LIFETIME.as_secs();
    let queued_at = now.saturating_sub(lifetime.saturating_sub(2 * 60));
    vault.queue.borrow_mut().push(queued(41, 0, REQUIRED));
    vault
        .history
        .borrow_mut()
        .push(submitted_event(41, 0, queued_at));
    let hot = FakeHot::always(0);

    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        0,
        "the existing exact queue entry is the request; no local submit is allowed"
    );
    let record = record_of(dir.path()).expect("the external request is recorded locally");
    assert_eq!(record.state, FundingState::Submitted);
    assert_eq!(record.pending_transaction_id.as_deref(), Some("41"));
    assert_eq!(
        record.created_at_unix, queued_at,
        "the adopted request keeps the finalized queue-admission time"
    );
    assert_eq!(record.expires_at_unix, funding_request_deadline(queued_at));
    assert!(
        !record.is_expired_at(record.expires_at_unix),
        "the adopted request remains live at its actual deadline"
    );
    assert!(
        record.is_expired_at(record.expires_at_unix.saturating_add(1)),
        "it expires one second after that deadline, not one hour after this process adopted it"
    );
}

/// A matching queue entry alone does not reveal when it was created. Without its finalized
/// `TransactionSubmitted` envelope there is no safe timestamp to adopt, so neither adoption nor a
/// second submit is allowed.
#[tokio::test]
async fn an_external_exact_request_without_a_chain_date_fails_closed() {
    let dir = temp();
    let vault = FakeVault::empty();
    vault.queue.borrow_mut().push(queued(41, 0, REQUIRED));
    let hot = FakeHot::always(0);

    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        0,
        "an undated request never authorizes a submit"
    );
    let record = record_of(dir.path()).expect("Prepared is durable before the failed adoption");
    assert_eq!(record.state, FundingState::Prepared);
    assert!(record.pending_transaction_id.is_none());
    assert!(
        !record.submit_attempted,
        "observing an undated external request is not a local submit attempt"
    );

    let lifetime = dexdo_core::params::VAULT_FUNDING_REQUEST_LIFETIME.as_secs();
    let queued_at = unix_now_secs().saturating_sub(lifetime.saturating_sub(2 * 60));
    vault
        .history
        .borrow_mut()
        .push(submitted_event(41, 0, queued_at));
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    let adopted = record_of(dir.path()).expect("the retry adopted the dated queue request");
    assert_eq!(adopted.state, FundingState::Submitted);
    assert!(!adopted.submit_attempted);
    assert_eq!(adopted.created_at_unix, queued_at);
    assert_eq!(adopted.expires_at_unix, funding_request_deadline(queued_at));
    assert_eq!(vault.submits.get(), 0, "neither pass submits a duplicate");
}

/// a queue read that did not answer cannot prove that a first exact request is absent.
#[tokio::test]
async fn an_unreadable_queue_before_the_first_local_submit_fails_closed() {
    let dir = temp();
    let vault = FakeVault::empty();
    *vault.queue_error.borrow_mut() = Some("queue temporarily unavailable".to_string());
    let hot = FakeHot::always(0);

    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(vault.submits.get(), 0, "unknown never authorizes a submit");
    let record = record_of(dir.path()).expect("Prepared is durable before the failed probe");
    assert_eq!(record.state, FundingState::Prepared);
    assert!(record.pending_transaction_id.is_none());
}

// ---------------------------------------------------------------------------------------------
// The rule: queue disappearance alone never authorizes another submit
// ---------------------------------------------------------------------------------------------

/// A finalized execution retires exactly the generation identified by its recorded queue id. If
/// that transfer satisfied an earlier, smaller need but the current command needs more, the old
/// request cannot execute again and a new generation must carry only the remaining shortfall.
#[tokio::test]
async fn an_executed_underfill_opens_a_new_generation_for_the_exact_remaining_shortfall() {
    let dir = temp();
    let vault = FakeVault::empty();

    // Run 1 creates a real generation through the production-composed entry point for the smaller
    // need. No journal end state is fabricated by the test.
    let first_requirements = FundingRequirements::new([(SHELL, 400)]);
    let hot = FakeHot::always(0);
    let _ = money_command_run_with_requirements(
        dir.path(),
        &vault,
        &hot,
        &first_requirements,
        bounds(),
    )
    .await;
    assert_eq!(vault.submits.get(), 1, "the first run creates the request");
    let after_first = record_of(dir.path()).expect("run 1 left a record");
    assert_eq!(after_first.state, FundingState::Submitted);
    assert_eq!(after_first.generation, 1);
    assert_eq!(after_first.pending_transaction_id.as_deref(), Some("7"));

    // The wallet's real queue/history surface now proves that exact id executed, and the observed
    // Hot balance includes the 400 it delivered. The next command needs 1,000, however.
    vault.queue.borrow_mut().clear();
    vault
        .history
        .borrow_mut()
        .push(sent_event(7, 0, QUEUED_AT + 60));

    let current_requirements = FundingRequirements::new([(SHELL, REQUIRED)]);
    let hot = FakeHot::always(400);
    let _ = money_command_run_with_requirements(
        dir.path(),
        &vault,
        &hot,
        &current_requirements,
        bounds(),
    )
    .await;

    assert_eq!(
        vault.submits.get(),
        2,
        "a finalized executed generation cannot move money again, so an unmet current need must \
         open one replacement generation"
    );
    let after_second = record_of(dir.path()).expect("run 2 opened a replacement record");
    assert_eq!(
        after_second.state,
        FundingState::Submitted,
        "the replacement generation was submitted through the provider"
    );
    assert_eq!(
        after_second.generation, 2,
        "the finalized execution retires generation 1 before the replacement is opened"
    );
    assert_eq!(
        after_second.fingerprint.cc.get(&SHELL),
        Some(&600),
        "generation 2 carries the exact current shortfall, not the original amount"
    );
    assert_eq!(after_second.pending_transaction_id.as_deref(), Some("8"));
}

/// The state between the two: gone from the live queue, no execution event, and still inside the
/// window in which the human can confirm it. Neither verdict is proven, so nothing is submitted.
#[tokio::test]
async fn a_disappearance_that_proves_neither_verdict_submits_nothing() {
    let dir = temp();
    let vault = FakeVault::empty();

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1);

    // It is not in the live queue this instant - a stale read, a reorganised view, anything - but
    // the deadline has not passed and no execution event exists.
    vault.queue.borrow_mut().clear();

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "queue disappearance ALONE must never authorize another submit"
    );
    let after = record_of(dir.path()).expect("record");
    assert_eq!(
        after.state,
        FundingState::Submitted,
        "an unproven disappearance must not advance the record either"
    );
    assert!(after.evidence.is_none(), "there was no evidence to record");
}

/// Unreadable history does not authorize another submit for a still-live request. This is the same
/// rule as an unreadable queue, one layer down, and it is what a flaky gateway exercises in
/// production.
#[tokio::test]
async fn an_unreadable_history_never_authorizes_another_submit() {
    let dir = temp();
    let vault = FakeVault::empty();

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1);

    vault.queue.borrow_mut().clear();
    *vault.history_error.borrow_mut() = Some("502 Bad Gateway".to_string());

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "a chain read that failed means unknown, and unknown must never permit a submit"
    );
    assert_eq!(
        record_of(dir.path()).expect("record").state,
        FundingState::Submitted
    );
}

/// A submit whose result was never observed leaves no queue id. The wallet's own
/// `TransactionSubmitted` recovers it, so the request is still recognised as ours - and, being
/// recognised, is not submitted again.
#[tokio::test]
async fn an_unobserved_submit_is_recovered_from_the_wallets_own_submitted_event() {
    let dir = temp();
    let vault = FakeVault::empty();
    vault.indeterminate.set(true);

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1);
    let after_first = record_of(dir.path()).expect("record");
    assert_eq!(
        after_first.state,
        FundingState::Prepared,
        "an unobserved submit must NOT be recorded as submitted"
    );
    assert!(after_first.pending_transaction_id.is_none());

    // On chain it DID land, and then it executed. The client never learned either.
    vault.indeterminate.set(false);
    vault
        .history
        .borrow_mut()
        .push(submitted_event(42, 0, QUEUED_AT));
    vault
        .history
        .borrow_mut()
        .push(sent_event(42, 0, QUEUED_AT + 30));

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "the request landed and executed; the repeat must recognise it from the wallet's own \
         events rather than reading the empty queue as absence"
    );
    let after = record_of(dir.path()).expect("record");
    assert_eq!(after.state, FundingState::Executed);
}

/// A still-live exact request remains the one being reconciled even if history has no admission.
#[tokio::test]
async fn a_request_the_history_never_recorded_queuing_is_refused_rather_than_retried() {
    let dir = temp();
    let vault = FakeVault::empty();
    vault.indeterminate.set(true);
    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1, "the first run creates the request");
    let after_first = record_of(dir.path()).expect("run 1 left a record");
    assert_eq!(
        after_first.state,
        FundingState::Prepared,
        "an unobserved submit is recorded as prepared and nothing more"
    );
    assert!(after_first.pending_transaction_id.is_none());
    assert!(
        vault.history.borrow().is_empty(),
        "the wallet never recorded the admission, which is the state under test"
    );

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "a still-live exact request is reused even when chain history has no admission event"
    );
    let after = record_of(dir.path()).expect("run 2 kept the record");
    assert_eq!(
        after.state,
        FundingState::Prepared,
        "a state the chain did not prove advances nothing"
    );
    assert_eq!(after.generation, 1, "the local lifetime has not elapsed");
    assert!(after.evidence.is_none(), "there was no evidence to record");
}

/// The refusal names the exact request and the UTC journal deadline an operator can inspect.
#[tokio::test]
async fn the_refusal_names_the_request_and_reports_the_local_deadline_without_acting_on_it() {
    let dir = temp();
    let vault = FakeVault::empty();
    vault.indeterminate.set(true);
    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    // The balance arrives mid-wait, so the run returns and carries the notice the funding step
    // reached - which is how a caller ever sees the verdict rather than only its side effects.
    let hot = FakeHot::then_always(vec![0], REQUIRED);
    let funded = money_command_run_with(dir.path(), &vault, &hot, patient_bounds())
        .await
        .expect("the balance arrives and the command continues");

    let FundingNotice::RequestIndeterminate { reason } = funded.notice else {
        panic!(
            "a live request with no chain verdict must stay indeterminate: {:?}",
            funded.notice
        );
    };
    assert!(
        reason.contains("no TransactionSent"),
        "the refusal must name the missing finalized verdict: {reason}"
    );
    assert!(
        reason.contains(&hot_address()) && reason.contains(&creator()),
        "the refusal must name the transfer the operator should go and look for: {reason}"
    );
    assert!(
        reason.contains("local funding journal until"),
        "the canonical journal deadline must be reported: {reason}"
    );
    assert_eq!(
        vault.submits.get(),
        1,
        "and, having decided nothing, it transferred nothing"
    );
}

// ---------------------------------------------------------------------------------------------
// The money command continues once the balance arrives
// ---------------------------------------------------------------------------------------------

/// The step the whole flow exists for: the command does not fail on an insufficient balance, it
/// arranges the top-up, waits, and CONTINUES - holding the turn, with the balances it actually read.
#[tokio::test]
async fn the_money_command_continues_once_the_balance_arrives() {
    let dir = temp();
    let vault = FakeVault::empty();
    // Short, short, then funded - the human confirmed the Vault transfer in between.
    let hot = FakeHot::then_always(vec![0, 400], REQUIRED);

    let funding = money_command_run_with(dir.path(), &vault, &hot, patient_bounds());
    let confirmation = async {
        while vault.submits.get() == 0 {
            tokio::task::yield_now().await;
        }
        vault.queue.borrow_mut().clear();
        vault
            .history
            .borrow_mut()
            .push(sent_event(7, 0, QUEUED_AT + 60));
    };
    let (funded, ()) = tokio::join!(funding, confirmation);
    let funded = funded.expect("the balance arrives and the command continues");

    assert_eq!(
        funded.observed.get(SHELL),
        REQUIRED,
        "the caller must be handed the balances the FINAL check actually read"
    );
    assert!(
        requirements().met_by(&funded.observed),
        "the command may only continue against a balance that meets its requirement"
    );
    assert_eq!(funded.notice, FundingNotice::RequestSubmitted);
    assert!(
        hot.reads.get() >= 3,
        "the wait must have re-read the balance rather than trusting the request"
    );
    let closed = record_of(dir.path()).expect("record");
    assert_eq!(
        closed.state,
        FundingState::Satisfied,
        "only an observed balance closes the record"
    );
    assert_eq!(
        closed
            .satisfied_balances
            .as_ref()
            .and_then(|b| b.get(&SHELL)),
        Some(&REQUIRED)
    );
}

// ---------------------------------------------------------------------------------------------
// The record carries what reconciliation needs
// ---------------------------------------------------------------------------------------------

/// The production provider derives its wire fingerprint from the same native shortfall the journal
/// records. This catches a fixed `value` surviving in the provider while the state machine appears
/// correct through a request-only fake.
#[tokio::test]
async fn the_provider_wire_carries_the_exact_native_shortfall() {
    let dir = temp();
    let vault = FakeVault::empty();
    let native_shortfall = 123;
    let hot = FakeHot::with_balances(vault_to_hot_native_value() - native_shortfall, REQUIRED);

    let _ = money_command_run(dir.path(), &vault, &hot).await;

    let submitted = vault.submitted.borrow();
    let on_wire = submitted
        .first()
        .expect("the provider submitted a fingerprint");
    assert_eq!(on_wire.value, native_shortfall);
    assert!(
        on_wire.cc.is_empty(),
        "an ECC-funded Hot sends no invented ECC entry for native vmshell"
    );
    assert_eq!(
        record_of(dir.path()).expect("journal").fingerprint,
        on_wire.clone(),
        "the wire transfer and durable identity must remain identical"
    );
}

/// Every field the specification names for the fingerprint, on the record, frozen for the
/// generation - and the same fingerprint on the wire, because both come from one derivation.
#[tokio::test]
async fn the_record_freezes_the_full_fingerprint_the_submit_used() {
    let dir = temp();
    let vault = FakeVault::empty();
    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    let record = record_of(dir.path()).expect("record");
    let fingerprint = &record.fingerprint;
    assert_eq!(fingerprint.creator, creator());
    assert_eq!(fingerprint.dest, hot_address());
    assert_eq!(
        fingerprint.dapp_id,
        hex64(0xa1),
        "a Vault -> Hot transfer is addressed into the Hot's OWN self-DApp, never the dexdo one"
    );
    assert_ne!(fingerprint.dapp_id, "4");
    assert_eq!(
        fingerprint.value, 0,
        "the already-funded native floor has zero shortfall"
    );
    assert_eq!(fingerprint.cc.get(&SHELL), Some(&REQUIRED));
    assert_eq!(fingerprint.send_flags, VAULT_TO_HOT_SEND_FLAGS);
    assert_eq!(fingerprint.bounce, VAULT_TO_HOT_BOUNCE);
    assert_eq!(fingerprint.payload_hash, payload_hash(VAULT_TO_HOT_PAYLOAD));

    let on_the_wire = vault.submitted.borrow();
    let on_the_wire = on_the_wire
        .first()
        .expect("the provider was asked to submit");
    assert_eq!(
        on_the_wire, fingerprint,
        "the transfer that went on the wire and the transfer the journal claims went on the wire \
         must be the same one"
    );
}

/// Regression for: a live request for A must not block a changed shortfall B.
#[tokio::test]
async fn a_moved_shortfall_creates_a_second_request_without_mutating_the_first() {
    let dir = temp();
    let vault = FakeVault::empty();
    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    let first = record_of(dir.path()).expect("record").fingerprint;
    assert_eq!(first.cc.get(&SHELL), Some(&REQUIRED));

    // The Hot has moved: today's shortfall is 600, not 1000.
    let hot = FakeHot::always(400);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    let after = records_of(dir.path());
    assert_eq!(
        after.len(),
        2,
        "both different live amounts stay in the journal"
    );
    assert_eq!(
        after[0].fingerprint, first,
        "creating B must not mutate A's frozen fingerprint"
    );
    assert_eq!(after[1].fingerprint.cc.get(&SHELL), Some(&600));
    assert_eq!(vault.submits.get(), 2, "B is a separate Vault request");
}

/// Regression for's live acceptance: once request A funds the Hot, rerunning the command
/// that created A must reconcile A rather than the newest live request B for a different amount.
#[tokio::test]
async fn an_already_funded_command_reconciles_its_own_amount_not_the_newest_other_amount() {
    let dir = temp();
    let vault = FakeVault::empty();
    let empty_hot = FakeHot::always(0);
    let first_requirements = requirements();
    let second_requirements = FundingRequirements::new([(SHELL, REQUIRED + 250)]);

    let _ = money_command_run_with_requirements(
        dir.path(),
        &vault,
        &empty_hot,
        &first_requirements,
        bounds(),
    )
    .await;
    let _ = money_command_run_with_requirements(
        dir.path(),
        &vault,
        &empty_hot,
        &second_requirements,
        bounds(),
    )
    .await;
    assert_eq!(vault.submits.get(), 2, "both distinct amounts were queued");

    // The operator approves only A. B remains confirmable in the Vault while A's exact credit is
    // already visible on the Hot, matching the live N100/N1000 acceptance sequence.
    vault.queue.borrow_mut().retain(|request| request.id != 7);
    vault
        .history
        .borrow_mut()
        .push(sent_event(7, 0, QUEUED_AT + 1));
    let funded_hot = FakeHot::always(REQUIRED);
    money_command_run_with_requirements(
        dir.path(),
        &vault,
        &funded_hot,
        &first_requirements,
        bounds(),
    )
    .await
    .expect("A is funded and must not be blocked by pending B");

    let after = records_of(dir.path());
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].generation, 1);
    assert_eq!(after[0].state, FundingState::Satisfied);
    assert_eq!(after[1].generation, 2);
    assert_eq!(after[1].state, FundingState::Submitted);
    assert_eq!(after[1].pending_transaction_id.as_deref(), Some("8"));
    assert_eq!(vault.submits.get(), 2, "the rerun submits nothing");
}

/// A version-1 record is refused rather than guessed at. It has no fingerprint and no generation,
/// and inventing them would make a request already on chain unrecognisable.
#[test]
fn a_record_from_before_the_fingerprint_is_refused_rather_than_migrated() {
    let dir = temp();
    ensure_funding_requests_dir(dir.path()).expect("dir");
    let path = funding_journal_path(dir.path(), "net-a", &hot_address());
    std::fs::write(
        &path,
        br#"{"version":1,"provider":"ackinacki-wallet","network":"net-a","hot_address":"x"}"#,
    )
    .expect("write");
    let error = load_funding_journal(dir.path(), "net-a", &hot_address())
        .expect_err("a record this client cannot read must not be acted on");
    let message = error.to_string();
    assert!(message.contains("version 1"), "{message}");
    assert!(message.contains("Do not delete it"), "{message}");
}

/// The journal stays non-secret: the new fields are amounts, ids, verdicts and timestamps.
#[tokio::test]
async fn the_extended_record_carries_no_secret() {
    let dir = temp();
    let vault = FakeVault::empty();
    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    let raw = std::fs::read_to_string(funding_journal_path(dir.path(), "net-a", &hot_address()))
        .expect("read raw");
    assert!(raw.contains("\"fingerprint\""), "{raw}");
    assert!(raw.contains("\"generation\""), "{raw}");
    assert!(
        !raw.contains("secret") && !raw.contains("seed") && !raw.contains("phrase"),
        "the journal is non-secret by construction: {raw}"
    );
}

/// A `BTreeMap<u32, _>` is the journal's currency map, and it must survive the JSON round trip that
/// turns every key into a string. A record that came back with a different `cc` would not match the
/// request it describes.
#[tokio::test]
async fn the_fingerprints_currency_map_survives_the_journal_round_trip() {
    let dir = temp();
    let vault = FakeVault::empty();
    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    let written = record_of(dir.path()).expect("record");
    store_funding_journal(dir.path(), &written).expect("re-store");
    let read_back = record_of(dir.path()).expect("record");
    assert_eq!(read_back, written);
    assert_eq!(read_back.fingerprint.cc, written.fingerprint.cc);
}

mod pr1332_regressions;
