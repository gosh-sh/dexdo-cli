//! re-audit item 2, second reading: a grown balance is not the identity of a delivery.

//! `Executed` is a fact about the VAULT - the message left it. Retiring that generation and opening
//! the next one needs a second fact: that THIS transfer is what credited the Hot. An aggregated
//! balance cannot carry it. Any incoming transfer of the same size produces exactly the same reading,
//! so a Hot topped up from anywhere else while the Vault delivery is still in flight looks identical
//! to a Hot the delivery has reached - and the replacement generation opened from it asks the Vault
//! for the same shortfall a second time. When the original delivery lands, the Hot holds both.

//! What carries the identity is on chain. The queued path runs `txn.dest.transfer(...)` and then
//! `emit TransactionSent(...)` inside ONE Vault transaction, so the event message is an anchor to
//! that transaction and the delivery is its sibling out-message addressed to the Hot. That sibling's
//! id, checked through the destination's own finalized receipt, is the fact - and it is recorded on
//! [`FundingEvidence`] as its own field rather than inside a sentence, because a decision is taken
//! from it.

//! Every test here drives the real entry point [`ensure_hot_funded_with_turn`] through the real
//! production provider [`AckinackiVaultProvider`], composed the way a money command composes it.
//! Generation 1 is always created by the mechanism itself; the only thing a test changes afterwards
//! is what the chain reports.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::providers::{
    AckinackiVaultProvider, QueuedTransfer, VaultChain, VaultQueueEvent, VaultQueueEventKind,
};
use super::*;

const SHELL: u32 = 2;
/// What the first command needs, and therefore what generation 1 carries.
const FIRST_REQUIREMENT: u128 = 400;
/// What the next command needs. Larger, so the requirement stays unmet and the executed generation
/// has to be reconciled rather than simply closed. Its remainder deliberately equals the first
/// transfer: once that transfer is provably credited and closed, the same amount is valid again.
const SECOND_REQUIREMENT: u128 = 800;
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

/// One check-and-arrange pass, then give up - one run of the command, as an operator repeats it.
fn one_pass_bounds() -> FundingWaitBounds {
    FundingWaitBounds {
        timeout: Duration::ZERO,
        poll: Duration::from_millis(1),
        lock_timeout: Duration::from_millis(200),
        lock_poll: Duration::from_millis(1),
    }
}

fn sent_event(id: u64, at: u64) -> VaultQueueEvent {
    VaultQueueEvent {
        kind: VaultQueueEventKind::Sent,
        transaction_id: id,
        dest: hot_address(),
        value: 0,
        dapp_id: hex64(0xa1),
        message_id: format!("msg-sent-{id}"),
        created_at: at,
    }
}

/// What the chain can say about the internal message behind an executed transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// The sibling is there and the Hot's own receipt for it is finalized.
    Proven,
    /// The anchor, the sibling or the receipt is not readable yet: no sibling addressed to the Hot,
    /// two of them, or a destination transaction that has not finalized.
    Unproven,
    /// The read itself failed. Not an answer about money at all.
    Unreadable,
}

/// A scripted Vault. Every answer is a fact some real chain could return; nothing here decides.
struct FakeVault {
    queue: RefCell<Vec<QueuedTransfer>>,
    history: RefCell<Vec<VaultQueueEvent>>,
    delivery: Cell<Delivery>,
    delivery_after_first_probe: Cell<Option<Delivery>>,
    delivery_probes: Cell<usize>,
    now: Cell<u64>,
    submits: Cell<usize>,
    submitted: RefCell<Vec<FundingFingerprint>>,
    next_id: Cell<u64>,
    credit_on_delivery: RefCell<Option<(Rc<Cell<u128>>, u128)>>,
}

impl FakeVault {
    fn with_delivery(delivery: Delivery) -> Self {
        Self {
            queue: RefCell::new(Vec::new()),
            history: RefCell::new(Vec::new()),
            delivery: Cell::new(delivery),
            delivery_after_first_probe: Cell::new(None),
            delivery_probes: Cell::new(0),
            now: Cell::new(QUEUED_AT),
            submits: Cell::new(0),
            submitted: RefCell::new(Vec::new()),
            next_id: Cell::new(7),
            credit_on_delivery: RefCell::new(None),
        }
    }

    fn credit_hot_when_delivery_is_resolved(&self, hot: &FakeHot, amount: u128) {
        *self.credit_on_delivery.borrow_mut() = Some((hot.shell.clone(), amount));
    }

    fn resolve_delivery_after_first_probe(&self) {
        self.delivery_after_first_probe.set(Some(Delivery::Proven));
    }
}

#[async_trait::async_trait(?Send)]
impl VaultChain for &FakeVault {
    async fn queue(&self) -> Result<Vec<QueuedTransfer>> {
        Ok(self.queue.borrow().clone())
    }

    async fn history(&self) -> Result<Vec<VaultQueueEvent>> {
        Ok(self.history.borrow().clone())
    }

    async fn delivery_message_id(
        &self,
        sent_event_message_id: &str,
        destination: &str,
        destination_dapp_id: &str,
    ) -> Result<Option<String>> {
        assert_eq!(
            destination,
            hot_address(),
            "a delivery may only ever be proven at the destination the generation froze"
        );
        assert_eq!(destination_dapp_id, hex64(0xa1));
        let delivery = self.delivery.get();
        let probe = self.delivery_probes.get();
        self.delivery_probes.set(probe + 1);
        if probe == 0 {
            if let Some(next) = self.delivery_after_first_probe.take() {
                self.delivery.set(next);
            }
        }
        match delivery {
            Delivery::Proven => {
                if let Some((shell, amount)) = self.credit_on_delivery.borrow_mut().take() {
                    shell.set(shell.get() + amount);
                }
                Ok(Some(format!("delivery-of-{sent_event_message_id}")))
            }
            Delivery::Unproven => Ok(None),
            Delivery::Unreadable => bail!("the delivery message could not be read"),
        }
    }

    async fn submit(&self, fingerprint: &FundingFingerprint) -> Result<SubmitOutcome> {
        self.submits.set(self.submits.get() + 1);
        self.submitted.borrow_mut().push(fingerprint.clone());
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        // A real Vault takes the request into its queue and records that it did.
        self.queue.borrow_mut().push(QueuedTransfer {
            id,
            creator_pubkey: Some(creator()),
            dest: hot_address(),
            value: fingerprint.value,
            cc: fingerprint.cc.clone(),
            send_flags: VAULT_TO_HOT_SEND_FLAGS,
            bounce: VAULT_TO_HOT_BOUNCE,
            dapp_id: hex64(0xa1),
            payload: None,
        });
        self.history.borrow_mut().push(VaultQueueEvent {
            kind: VaultQueueEventKind::Submitted,
            transaction_id: id,
            dest: hot_address(),
            value: fingerprint.value,
            dapp_id: hex64(0xa1),
            message_id: format!("msg-submitted-{id}"),
            created_at: self.now.get(),
        });
        Ok(SubmitOutcome::Accepted {
            transaction_hash: Some(format!("tx-{id}")),
            pending_transaction_id: Some(id.to_string()),
        })
    }
}

/// A Hot whose balances are whatever the chain would report at that moment.
struct FakeHot {
    shell: Rc<Cell<u128>>,
    reads: Cell<usize>,
    never_answer_on_read: Cell<Option<usize>>,
    no_answer_then_hold: Cell<Option<(usize, u128)>>,
}

impl FakeHot {
    /// A Hot with every native unit this money path can attach, holding `shell` ECC[2]. The native
    /// leg is deliberately never the refusal here: what is under test is the SHELL identity.
    fn holding(shell: u128) -> Self {
        Self {
            shell: Rc::new(Cell::new(shell)),
            reads: Cell::new(0),
            never_answer_on_read: Cell::new(None),
            no_answer_then_hold: Cell::new(None),
        }
    }

    fn never_answer_on_read(&self, read: usize) {
        self.never_answer_on_read.set(Some(read));
    }

    fn no_answer_then_hold(&self, read: usize, shell: u128) {
        self.no_answer_then_hold.set(Some((read, shell)));
    }
}

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for FakeHot {
    async fn hot_balances(&self, _hot: &CanonicalAddress) -> Result<HotBalances> {
        let read = self.reads.get() + 1;
        self.reads.set(read);
        if self.never_answer_on_read.get() == Some(read) {
            std::future::pending::<()>().await;
        }
        if let Some((failed_read, next_shell)) = self.no_answer_then_hold.get() {
            if failed_read == read {
                self.no_answer_then_hold.set(None);
                self.shell.set(next_shell);
                bail!(
                    "{}5 attempt(s) in 45s",
                    dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX
                );
            }
        }
        Ok(HotBalances::new(
            vault_to_hot_native_value(),
            [(SHELL, self.shell.get())],
        ))
    }
}

fn record_of(dir: &Path) -> Option<FundingJournalRecord> {
    load_funding_journal(dir, "net-a", &hot_address()).expect("read journal")
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp data dir")
}

/// One run of a money command's funding step, composed exactly as production composes it: read the
/// journal under the held turn, hand what it recorded to the provider, then run the mechanism.
async fn money_command_run(
    dir: &Path,
    vault: &FakeVault,
    hot: &FakeHot,
    required: u128,
) -> Result<FundedHot> {
    money_command_run_with_bounds(dir, vault, hot, required, one_pass_bounds()).await
}

async fn money_command_run_with_bounds(
    dir: &Path,
    vault: &FakeVault,
    hot: &FakeHot,
    required: u128,
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
            requirements: &FundingRequirements::new([(SHELL, required)]),
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

/// Run 1 through the real mechanism: an empty Hot, one generation created for the whole shortfall.
async fn generation_one(dir: &Path, vault: &FakeVault) {
    let hot = FakeHot::holding(0);
    let _ = money_command_run(dir, vault, &hot, FIRST_REQUIREMENT).await;
    assert_eq!(vault.submits.get(), 1, "the first run creates the request");
    let first = record_of(dir).expect("run 1 left a record");
    assert_eq!(first.generation, 1);
    assert_eq!(first.state, FundingState::Submitted);
    assert_eq!(first.pending_transaction_id.as_deref(), Some("7"));
    assert_eq!(
        first.fingerprint.cc.get(&SHELL),
        Some(&FIRST_REQUIREMENT),
        "generation 1 carries the whole shortfall it was sized for"
    );
}

/// The human confirms the request: the queue drops the entry and finalized history proves that exact
/// id EXECUTED. Nothing here says the Hot has been credited.
fn the_vault_transfer_executes(vault: &FakeVault) {
    vault.queue.borrow_mut().clear();
    vault
        .history
        .borrow_mut()
        .push(sent_event(7, QUEUED_AT + 60));
}

// ---------------------------------------------------------------------------------------------
// The defect: a balance that grew by the right amount, for the wrong reason
// ---------------------------------------------------------------------------------------------

/// An unproven delivery does not duplicate the same exact live amount.
#[tokio::test]
async fn an_unproven_delivery_does_not_duplicate_the_same_exact_amount() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Unproven);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);

    let hot = FakeHot::holding(0);
    let _ = money_command_run(dir.path(), &vault, &hot, FIRST_REQUIREMENT).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "an exact live amount remains de-duplicated while its delivery cannot be proven"
    );
    let after = record_of(dir.path()).expect("the executed generation stays recorded");
    assert_eq!(
        after.generation, 1,
        "no generation may be opened while the executed one's delivery is unidentified"
    );
    assert_eq!(after.state, FundingState::Executed);
    let evidence = after.evidence.expect("the execution verdict is recorded");
    assert_eq!(
        evidence.delivery_message_id, None,
        "an unproven delivery must be recorded as unproven, never inferred from the balance"
    );
}

/// The same observation with the one fact that separates it: the chain names the internal message
/// that carried our transfer to the Hot, and the Hot's own receipt for it is finalized.

/// This is the half that keeps the refusal above honest. A fix that simply stopped opening
/// replacement generations would pass that test and leave a Hot that was underfunded once
/// permanently unfundable, which is worse than the defect.
#[tokio::test]
async fn a_delivery_the_chain_can_name_opens_the_next_generation_for_the_exact_remainder() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Proven);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);

    let hot = FakeHot::holding(FIRST_REQUIREMENT);
    let _ = money_command_run(dir.path(), &vault, &hot, SECOND_REQUIREMENT).await;

    assert_eq!(
        vault.submits.get(),
        2,
        "a delivery proven by the destination's own receipt retires its generation, and an unmet \
         requirement must then open exactly one replacement"
    );
    let after = record_of(dir.path()).expect("run 2 opened a replacement record");
    assert_eq!(after.generation, 2);
    assert_eq!(after.state, FundingState::Submitted);
    assert_eq!(
        after.fingerprint.cc.get(&SHELL),
        Some(&(SECOND_REQUIREMENT - FIRST_REQUIREMENT)),
        "the replacement carries the exact remaining shortfall even when it equals the completed \
         generation's amount"
    );
    assert_eq!(
        vault.submitted.borrow().len(),
        2,
        "exactly two transfers were ever put on the wire"
    );
}

/// The balance used to size a replacement must be read after the destination receipt is known.
/// Here the first 400 are unrelated funds. The executed generation's own 400 land exactly while
/// the provider resolves its delivery message, so the pre-receipt read says "short 400" while the
/// required post-receipt read says "funded".
#[tokio::test]
async fn a_pre_receipt_balance_cannot_open_a_duplicate_remainder() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Proven);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);

    let hot = FakeHot::holding(FIRST_REQUIREMENT);
    vault.credit_hot_when_delivery_is_resolved(&hot, FIRST_REQUIREMENT);
    let funded = money_command_run(dir.path(), &vault, &hot, SECOND_REQUIREMENT).await;

    assert_eq!(
        hot.reads.get(),
        2,
        "the Hot must be read again after the finalized delivery receipt is resolved"
    );
    assert_eq!(
        vault.submits.get(),
        1,
        "the stale pre-receipt shortfall must not create a second Vault transfer"
    );
    assert_eq!(
        funded
            .expect("the post-receipt balance is sufficient")
            .observed
            .get(SHELL),
        SECOND_REQUIREMENT,
    );
    let record = record_of(dir.path()).expect("record");
    assert_eq!(
        record.state,
        FundingState::Satisfied,
        "generation 1 closes only against the post-receipt balance"
    );
    assert_eq!(
        record
            .satisfied_balances
            .as_ref()
            .and_then(|balances| balances.get(&SHELL)),
        Some(&SECOND_REQUIREMENT),
        "the journal must persist the post-receipt balance, not the stale first read"
    );
}

/// A receipt that appears on a later poll still shares the command's original wait deadline. The
/// mandatory read is allowed to consume what remains of that window, but it cannot silently start a
/// new unbounded chain-read budget.
#[tokio::test]
async fn a_late_post_receipt_read_cannot_outlive_the_funding_wait() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Unproven);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);
    vault.resolve_delivery_after_first_probe();

    let hot = FakeHot::holding(FIRST_REQUIREMENT);
    hot.never_answer_on_read(3);
    let started = tokio::time::Instant::now();
    let result = money_command_run_with_bounds(
        dir.path(),
        &vault,
        &hot,
        SECOND_REQUIREMENT,
        FundingWaitBounds {
            timeout: Duration::from_millis(500),
            poll: Duration::from_millis(1),
            lock_timeout: Duration::from_millis(200),
            lock_poll: Duration::from_millis(1),
        },
    )
    .await;

    let refusal = result.expect_err("the post-receipt read must share the funding deadline");
    let refusal_chain = format!("{refusal:#}");
    assert!(
        refusal_chain.contains("timed out after 0s"),
        "the operator must get the ordinary funding-timeout verdict: {refusal_chain}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the post-receipt read escaped the funding deadline"
    );
    assert_eq!(
        hot.reads.get(),
        3,
        "the third read is the post-receipt read"
    );
    assert_eq!(vault.submits.get(), 1, "a timed-out read submits nothing");
    let record = record_of(dir.path()).expect("record");
    assert_eq!(record.state, FundingState::Executed);
    assert!(
        record
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.delivery_message_id.is_some()),
        "the finalized receipt remains durable for the next command"
    );
}

/// Exhausting one shared-reader attempt after the receipt is not a balance verdict. The finalized
/// delivery evidence is already durable at this point, so the same funding wait must poll again
/// rather than fail the command or submit another transfer.
#[tokio::test]
async fn a_post_receipt_no_answer_is_retried_within_the_funding_wait() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Proven);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);

    let hot = FakeHot::holding(FIRST_REQUIREMENT);
    hot.no_answer_then_hold(2, SECOND_REQUIREMENT);
    let funded = money_command_run_with_bounds(
        dir.path(),
        &vault,
        &hot,
        SECOND_REQUIREMENT,
        FundingWaitBounds {
            timeout: Duration::from_secs(30),
            poll: Duration::from_millis(1),
            lock_timeout: Duration::from_millis(200),
            lock_poll: Duration::from_millis(1),
        },
    )
    .await;

    assert_eq!(
        funded
            .expect("the next Hot read within the funding window is sufficient")
            .observed
            .get(SHELL),
        SECOND_REQUIREMENT,
    );
    assert_eq!(
        hot.reads.get(),
        3,
        "the ordinary polling path must retry the failed post-receipt read"
    );
    assert_eq!(
        vault.submits.get(),
        1,
        "a transient read failure must not submit a second transfer"
    );
    let record = record_of(dir.path()).expect("record");
    assert_eq!(record.state, FundingState::Satisfied);
    assert!(
        record
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.delivery_message_id.is_some()),
        "the finalized delivery evidence remains durable across the failed read"
    );
}

/// The same transient failure can occur while an already sufficient Hot is reconciling its exact
/// recorded generation. That caller must rejoin the funding wait too, rather than turn a temporary
/// read failure into a queue-settlement refusal.
#[tokio::test]
async fn an_already_funded_reconciliation_retries_a_post_receipt_no_answer() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Proven);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);

    let hot = FakeHot::holding(FIRST_REQUIREMENT);
    hot.no_answer_then_hold(2, FIRST_REQUIREMENT);
    let funded = money_command_run_with_bounds(
        dir.path(),
        &vault,
        &hot,
        FIRST_REQUIREMENT,
        FundingWaitBounds {
            timeout: Duration::from_secs(30),
            poll: Duration::from_millis(1),
            lock_timeout: Duration::from_millis(200),
            lock_poll: Duration::from_millis(1),
        },
    )
    .await
    .expect("the already-funded reconciliation must retry the Hot read");

    assert_eq!(funded.observed.get(SHELL), FIRST_REQUIREMENT);
    assert_eq!(hot.reads.get(), 3, "the failed read must be retried");
    assert_eq!(vault.submits.get(), 1, "reconciliation submits nothing");
    let record = record_of(dir.path()).expect("record");
    assert_eq!(record.state, FundingState::Satisfied);
    assert!(
        record
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.delivery_message_id.is_some()),
        "the finalized delivery evidence remains durable across the failed read"
    );
}

/// The delivery message id is a fact a later audit has to be able to read back, so it is recorded as
/// its own field. A phrase inside `source` or `detail` would be prose: the mechanism itself takes a
/// decision from this, and prose is not a decidable record.
#[tokio::test]
async fn the_proven_delivery_is_recorded_as_its_own_journal_field() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Proven);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);

    // A requirement the delivery already meets, so the run stops on the executed verdict itself and
    // the record keeps it rather than being replaced by the next generation's.
    let hot = FakeHot::holding(0);
    let _ = money_command_run(dir.path(), &vault, &hot, FIRST_REQUIREMENT).await;

    let evidence = record_of(dir.path())
        .expect("record")
        .evidence
        .expect("the execution verdict is recorded");
    assert_eq!(
        evidence.delivery_message_id.as_deref(),
        Some("delivery-of-msg-sent-7"),
        "the internal message that carried the money is what the journal has to keep"
    );
    assert_ne!(
        evidence.source, "delivery-of-msg-sent-7",
        "`source` names the ext-out EVENT message, which is a different message from the delivery"
    );
}

// ---------------------------------------------------------------------------------------------
// A read that failed is not an answer about money
// ---------------------------------------------------------------------------------------------

/// A delivery read failure cannot authorize a duplicate of the same exact live amount.
#[tokio::test]
async fn a_delivery_read_that_failed_leaves_the_verdict_unknown_and_submits_nothing() {
    let dir = temp();
    let vault = FakeVault::with_delivery(Delivery::Unreadable);
    generation_one(dir.path(), &vault).await;
    the_vault_transfer_executes(&vault);

    let hot = FakeHot::holding(0);
    let _ = money_command_run(dir.path(), &vault, &hot, FIRST_REQUIREMENT).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "a chain read that failed authorizes nothing"
    );
    let after = record_of(dir.path()).expect("the record survives an unreadable chain");
    assert_eq!(after.generation, 1);
    assert_eq!(
        after.state,
        FundingState::Submitted,
        "an unknown verdict must not advance the record to a finalized one"
    );
}
