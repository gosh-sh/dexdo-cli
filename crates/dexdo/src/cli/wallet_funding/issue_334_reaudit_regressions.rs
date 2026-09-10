//! re-audit items 2 and 3: the window a delivery has not landed in, and a floor sized for one
//! spend on a path that spends twice.

//! Item 2. `Executed` is a fact about the VAULT - the message left it. The credit arrives on the Hot
//! in a later transaction, and in between the Hot's balance is still the one the executed generation
//! was sized against. Opening the next generation from that reading asks for the whole old shortfall
//! a second time, and once the first delivery lands the Hot holds both. The regression below drives
//! exactly that window: the queue proves execution while the Hot still reads its pre-delivery
//! balance. The reconciliation suite's `an_executed_underfill_opens_a_new_generation_for_the_exact_
//! remaining_shortfall` covers the other side - a delivery that HAS landed and was too small - and
//! must stay green, because refusing there would leave the Hot permanently unfundable.

//! Item 3. The native floor was one `NOTE_DEPLOY_SUBMIT_NATIVE_VALUE`. A fresh `note deploy` submits
//! twice - the deposit voucher and the SHELL gas voucher - and EACH submit takes two amounts out of
//! the Hot: the value it attaches, which does not come back, and the fee its own transaction is
//! charged, which `flag: 1` pays from the wallet's balance. A Hot funded for less than both submits'
//! full cost pays for the first voucher and cannot pay for the second, and the deploy stops half way
//! with the deposit spent and a halo2 proof already made. One test here proves the short Hot is
//! refused; the other proves the floor is enough for both, so that lowering it goes red.

//! Both tests drive the real entry point [`ensure_hot_funded_with_turn`] through the real production
//! provider [`AckinackiVaultProvider`], composed the way a money command composes it: the journal is
//! read first and what it recorded is handed to the provider. Only the chain underneath is scripted,
//! and every journal state under test is one the mechanism itself wrote on an earlier run.

use std::cell::{Cell, RefCell};

use dexdo_core::params::NOTE_DEPLOY_SUBMIT_NATIVE_VALUE;

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

/// A scripted Vault. Every answer is a fact some real chain could return; nothing here decides.
struct FakeVault {
    queue: RefCell<Vec<QueuedTransfer>>,
    history: RefCell<Vec<VaultQueueEvent>>,
    now: Cell<u64>,
    submits: Cell<usize>,
    submitted: RefCell<Vec<FundingFingerprint>>,
    next_id: Cell<u64>,
}

impl FakeVault {
    fn empty() -> Self {
        Self {
            queue: RefCell::new(Vec::new()),
            history: RefCell::new(Vec::new()),
            now: Cell::new(QUEUED_AT),
            submits: Cell::new(0),
            submitted: RefCell::new(Vec::new()),
            next_id: Cell::new(7),
        }
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

    /// A real Vault's executing transaction emits the `TransactionSent` event and the internal
    /// transfer together, so the chain can name the sibling that carried the money as soon as it has
    /// the event. Naming it here keeps item 2's window test discriminating: what refuses there is
    /// the Hot's own pre-delivery READING, with the delivery's identity already established.
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
    native: u128,
    shell: u128,
}

impl FakeHot {
    /// A Hot with every native unit this money path can attach, holding `shell` ECC[2].
    fn holding(shell: u128) -> Self {
        Self {
            native: vault_to_hot_native_value(),
            shell,
        }
    }

    fn with_balances(native: u128, shell: u128) -> Self {
        Self { native, shell }
    }
}

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for FakeHot {
    async fn hot_balances(&self, _hot: &CanonicalAddress) -> Result<HotBalances> {
        Ok(HotBalances::new(self.native, [(SHELL, self.shell)]))
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
async fn money_command_run(dir: &Path, vault: &FakeVault, hot: &FakeHot) -> Result<FundedHot> {
    let recorded = record_of(dir)
        .filter(FundingJournalRecord::is_open)
        .map(|record| record.recorded_request());
    let provider = AckinackiVaultProvider::new(vault, recorded);
    let binding = binding();
    ensure_hot_funded_with_turn(
        &HotFundingContext {
            binding: &binding,
            requirements: &requirements(),
            operation: "note deploy",
            creator_pubkey: &creator(),
            data_dir: dir,
            bounds: one_pass_bounds(),
        },
        HotTurn::AcquireOwn,
        hot,
        &provider,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Item 2: the transfer has left the Vault and the Hot has not been credited yet
// ---------------------------------------------------------------------------------------------

/// The window between "the message left the Vault" and "the Hot holds it".

/// A second generation opened inside that window is sized from a balance the first transfer has not
/// reached yet, so it asks for the whole shortfall a second time - and when the first delivery lands
/// the Hot holds two of them. Nothing here is fabricated: generation 1 is created by the mechanism
/// itself, and the only thing the test changes afterwards is what the chain reports.
#[tokio::test]
async fn an_executed_transfer_the_hot_has_not_shown_yet_opens_no_second_generation() {
    let dir = temp();
    let vault = FakeVault::empty();

    // Run 1: an empty Hot. One generation is created for the whole requirement.
    let hot = FakeHot::holding(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1, "the first run creates the request");
    let first = record_of(dir.path()).expect("run 1 left a record");
    assert_eq!(first.generation, 1);
    assert_eq!(first.state, FundingState::Submitted);
    assert_eq!(first.pending_transaction_id.as_deref(), Some("7"));
    assert_eq!(
        first.fingerprint.cc.get(&SHELL),
        Some(&REQUIRED),
        "generation 1 carries the whole shortfall"
    );

    // The human confirms it. The queue drops the entry and finalized history proves that exact id
    // EXECUTED - which says the message left the VAULT. The Hot still reads 0, because the
    // destination transaction has not been applied yet. This is the window.
    vault.queue.borrow_mut().clear();
    vault
        .history
        .borrow_mut()
        .push(sent_event(7, QUEUED_AT + 60));

    let hot = FakeHot::holding(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "a transfer proven to have left the Vault whose credit the Hot has not shown must not be \
         sized against the balance it has not reached yet: generation 2 would ask the Vault for \
         the same 1000 a second time, and the Hot would end up holding both"
    );
    let mut inside_window = record_of(dir.path()).expect("the executed generation stays recorded");
    assert_eq!(
        inside_window.generation, 1,
        "no generation may be opened from a reading that predates the delivery"
    );
    assert_eq!(inside_window.state, FundingState::Executed);
    assert!(
        inside_window.evidence.is_some(),
        "the execution verdict is kept on the record so the next run reconciles against it"
    );

    // Put the old queue deadline in the past. Once execution is finalized that deadline cannot
    // make the already-sent transfer disappear or make the same amount safe to submit again.
    inside_window.created_at_unix = 0;
    inside_window.expires_at_unix = dexdo_core::params::VAULT_FUNDING_REQUEST_LIFETIME.as_secs();
    store_funding_journal(dir.path(), &inside_window).expect("age the executed record");

    // Run 3: the same pre-delivery balance is still visible after that queue deadline. Persisting
    // `Executed` must keep the generation in duplicate selection until the Hot receives it.
    let hot = FakeHot::holding(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(
        vault.submits.get(),
        1,
        "a later invocation in the same Executed -> Hot-credit window must keep reconciling the \
         original generation instead of submitting the same shortfall again"
    );

    // The delivery is credited. It was sized for exactly this requirement, so the credit alone
    // meets it and the command continues - having transferred once.
    let hot = FakeHot::holding(REQUIRED);
    let funded = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(
        vault.submits.get(),
        1,
        "the delivery the first generation carried is the whole requirement; a second transfer was \
         never needed"
    );
    let funded = funded.expect("a credited delivery lets the money command continue");
    assert_eq!(funded.observed.get(SHELL), REQUIRED);
    assert_eq!(
        record_of(dir.path()).expect("record").state,
        FundingState::Satisfied,
        "the credited generation is retained as completed history"
    );
}

// ---------------------------------------------------------------------------------------------
// Item 3: the floor is everything BOTH voucher submits take out of the Hot
// ---------------------------------------------------------------------------------------------

/// What ONE `note deploy` voucher submit takes out of the Hot, from the canonical facts rather than
/// from the requirement under test.

/// Both halves leave the wallet: the value the submit ATTACHES never comes back
/// (`RootPN.generateVoucher` accepts and sends no change), and `flag: 1` pays the message's fee from
/// the wallet's balance instead of out of the amount being sent.
fn one_voucher_submit_costs() -> u128 {
    NOTE_DEPLOY_SUBMIT_NATIVE_VALUE + dexdo_core::params::WALLET_SUBMIT_NATIVE_FEE_BOUND_RAW
}

/// A Hot funded for ONE voucher submit is short by exactly the OTHER one.

/// This is the failure the floor exists to prevent, in the state it actually occurs in: the Hot can
/// pay for the deposit voucher and cannot pay for the SHELL gas voucher, so the deploy stops half
/// way with the deposit spent and a halo2 proof already made. The funding step runs before the
/// recovery file is read, so it cannot know that only one leg is left; what it can know is the most
/// the path can spend.
#[tokio::test]
async fn a_hot_that_can_pay_for_one_voucher_submit_is_short_by_exactly_the_other() {
    let dir = temp();
    let vault = FakeVault::empty();
    // Every raw unit of ECC[2] the command needs, and native for exactly one voucher submit.
    let hot = FakeHot::with_balances(one_voucher_submit_costs(), REQUIRED);

    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "a Hot that can pay for one voucher submit and not the second is short for this money path, \
         and the funding step must ask the Vault for the difference rather than report it funded"
    );
    let submitted = vault.submitted.borrow();
    let on_wire = submitted
        .first()
        .expect("the provider submitted a fingerprint");
    assert_eq!(
        on_wire.value,
        one_voucher_submit_costs(),
        "the request carries exactly the second submit the Hot cannot pay for - its attached value \
         AND the fee its own transaction charges"
    );
    assert!(
        on_wire.cc.is_empty(),
        "the ECC[2] leg is already met; only the native shortfall is requested"
    );
    assert_eq!(
        record_of(dir.path())
            .expect("the native shortfall opened a record")
            .native_shortfall,
        one_voucher_submit_costs()
    );
}

/// A Hot standing exactly on the floor holds everything BOTH voucher submits take, and is passed
/// through without a request.

/// The other half of the same money fact, and the one that keeps the floor from being quietly
/// lowered again: the left-hand side is derived from the canonical constants of the money path -
/// how many submits it makes, what each attaches, and the bound on what each is charged - while the
/// right-hand side is the requirement `note deploy` and `note topup` actually hand the mechanism.
/// Shrink the floor and this goes red without anyone having to remember why it was raised.
#[tokio::test]
async fn a_hot_on_the_floor_holds_what_both_voucher_submits_take() {
    let both_submits = dexdo_core::params::NOTE_DEPLOY_WALLET_SUBMITS * one_voucher_submit_costs();
    let floor = requirements().required_native;
    assert!(
        floor >= both_submits,
        "the Hot's native floor is {floor} raw but the two voucher submits of one note deploy take \
         {both_submits} raw - {} each, attached value plus the fee bound. A Hot topped up to the \
         floor would still stop between the deposit voucher and the gas voucher",
        one_voucher_submit_costs()
    );
    assert!(
        floor <= dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE,
        "the floor is the sends half of the operator wallet's own predeploy budget, so it must \
         never ask a Hot for more native than `note wallet` already funds a fresh wallet with"
    );

    // The floor is the decision boundary of the real entry point, not just an arithmetic identity.
    let dir = temp();
    let vault = FakeVault::empty();
    let on_the_floor = FakeHot::with_balances(floor, REQUIRED);
    let funded = money_command_run(dir.path(), &vault, &on_the_floor)
        .await
        .expect("a Hot standing on the floor is funded for this money path");
    assert_eq!(funded.notice, FundingNotice::AlreadyFunded);
    assert_eq!(
        vault.submits.get(),
        0,
        "a Hot that already holds what both submits take needs nothing from the Vault"
    );

    let dir = temp();
    let vault = FakeVault::empty();
    let one_short = FakeHot::with_balances(floor - 1, REQUIRED);
    let _ = money_command_run(dir.path(), &vault, &one_short).await;
    let submitted = vault.submitted.borrow();
    let on_wire = submitted
        .first()
        .expect("one raw unit below the floor is not funded");
    assert_eq!(
        on_wire.value, 1,
        "the floor is exact: one raw unit below it, the Vault is asked for exactly that unit"
    );
}
