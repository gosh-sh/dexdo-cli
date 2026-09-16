//! re-audit items 4 and 8: what the funding wait does with a read that got no answer, and
//! what its failure hands a machine consumer.

//! Every case here drives the real entry point the two money commands reach -
//! `ensure_hot_funded_with_turn` with the turn already held, which is exactly how
//! `fund_hot_for_money_command` calls it - through the two seams a real run uses: a balance reader
//! and a provider. Nothing calls an internal helper, and nothing writes an end state by hand.

//! Item 8's case asserts the FIELDS of the error envelope a `--json` run emits, built by the same
//! `machine::machine_error` that `main.rs` prints, rather than any `stderr` wording.

//! Declared from `cli/mod.rs` rather than from `wallet_funding.rs` so the regression still
//! compiles - and still fails - when the funding module alone is reverted to its pre-fix state.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::cli::machine::{self, ErrorCode, MachineFundingNotice, OP_NOTE_DEPLOY};
use crate::cli::wallet_funding::{
    ensure_hot_funded_with_turn, FundedHot, FundingEvidence, FundingRequest, FundingRequirements,
    FundingWaitBounds, HotBalanceReader, HotBalances, HotFundingContext, HotFundingProvider,
    HotTurn, RequestPresence, SubmitOutcome, WalletBinding, WalletProvider,
};

const SHELL: u32 = 2;
const REQUIRED_SHELL: u128 = 1_000;

/// A creator key that appears nowhere in any operator-facing message, so a test can prove the
/// machine envelope did not pick one up.
const CREATOR_PUBKEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

/// The marker the shared per-read retry stamps on a read it never got an answer for. Spelled
/// through the constant rather than by hand, so the discrimination cannot drift from the producer.
fn no_answer_error() -> String {
    format!(
        "read Hot balances: {}5 attempt(s) in 45s",
        dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX
    )
}

/// An answer that will read the same in ten minutes: the parameters could not even be encoded.
const FINAL_ERROR: &str = "encode getAccount parameters: value is not a valid address";

fn hex64(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn self_dapp_hot() -> String {
    format!("{}::{}", hex64(0xa1), hex64(0xa1))
}

fn vault_address() -> String {
    format!("{}::{}", hex64(0xb2), hex64(0xb2))
}

fn binding() -> WalletBinding {
    WalletBinding {
        provider: WalletProvider::AckinackiWallet,
        network: "net-a".to_string(),
        hot_address: self_dapp_hot(),
        vault_address: Some(vault_address()),
    }
}

fn requirements() -> FundingRequirements {
    FundingRequirements::new([(SHELL, REQUIRED_SHELL)])
}

/// A wait with a real budget, the shape `--funding-timeout 10m` produces. The poll is tiny so the
/// test spends test time, not wall time; the budget is what the case is about.
fn patient_bounds() -> FundingWaitBounds {
    FundingWaitBounds {
        timeout: Duration::from_secs(30),
        poll: Duration::from_millis(1),
        lock_timeout: Duration::from_millis(200),
        lock_poll: Duration::from_millis(1),
    }
}

/// A wait whose budget is already spent, so the first unmet check is also the last.
fn spent_bounds() -> FundingWaitBounds {
    FundingWaitBounds {
        timeout: Duration::ZERO,
        poll: Duration::from_millis(1),
        lock_timeout: Duration::from_millis(200),
        lock_poll: Duration::from_millis(1),
    }
}

/// A balance reader that serves a scripted sequence and then repeats its last answer for ever.
struct ScriptedChain {
    answers: RefCell<Vec<Result<HotBalances, String>>>,
    last: RefCell<Result<HotBalances, String>>,
    reads: Cell<usize>,
}

impl ScriptedChain {
    fn new(answers: Vec<Result<HotBalances, String>>, last: Result<HotBalances, String>) -> Self {
        let mut answers = answers;
        answers.reverse();
        Self {
            answers: RefCell::new(answers),
            last: RefCell::new(last),
            reads: Cell::new(0),
        }
    }

    fn shell(amount: u128) -> Result<HotBalances, String> {
        Ok(HotBalances::new(
            crate::cli::wallet_funding::vault_to_hot_native_value(),
            [(SHELL, amount)],
        ))
    }
}

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for ScriptedChain {
    async fn hot_balances(&self, _hot: &dexdo_core::CanonicalAddress) -> Result<HotBalances> {
        self.reads.set(self.reads.get() + 1);
        let next = self.answers.borrow_mut().pop();
        match next.unwrap_or_else(|| self.last.borrow().clone()) {
            Ok(balances) => Ok(balances),
            Err(reason) => Err(anyhow!("{reason}")),
        }
    }
}

/// A Vault provider that answers scripted probes and counts the submits it was asked for.
struct ScriptedProvider {
    probe: RefCell<Vec<RequestPresence>>,
    probe_default: RequestPresence,
    submits: Cell<usize>,
}

impl ScriptedProvider {
    fn new(probe_default: RequestPresence) -> Self {
        Self {
            probe: RefCell::new(Vec::new()),
            probe_default,
            submits: Cell::new(0),
        }
    }

    fn with_probes(self, probes: Vec<RequestPresence>) -> Self {
        let mut reversed = probes;
        reversed.reverse();
        *self.probe.borrow_mut() = reversed;
        self
    }
}

#[async_trait::async_trait(?Send)]
impl HotFundingProvider for ScriptedProvider {
    fn provider(&self) -> WalletProvider {
        WalletProvider::AckinackiWallet
    }

    async fn probe_existing_request(&self, _request: &FundingRequest) -> Result<RequestPresence> {
        let next = self.probe.borrow_mut().pop();
        Ok(next.unwrap_or_else(|| self.probe_default.clone()))
    }

    async fn create_request(&self, _request: &FundingRequest) -> Result<SubmitOutcome> {
        self.submits.set(self.submits.get() + 1);
        Ok(SubmitOutcome::Accepted {
            transaction_hash: Some("tx".to_string()),
            // A numeric queue id, which is what a real Vault hands back and what a later
            // finalized verdict is bound to.
            pending_transaction_id: Some("7".to_string()),
        })
    }

    fn manual_instruction(&self, _request: &FundingRequest) -> String {
        "top up the Hot yourself".to_string()
    }
}

/// The finalized verdict that retires a queue entry: the transfer executed.
fn executed() -> RequestPresence {
    RequestPresence::Executed {
        evidence: FundingEvidence {
            verdict: "executed".to_string(),
            source: "finalized transaction".to_string(),
            observed_at_unix: Some(1_700_000_000),
            detail: "queue transaction 7 executed".to_string(),
            delivery_message_id: None,
        },
    }
}

/// A transport failure whose WORDS carry none of the vocabulary `classify_error` matches on.

/// That is the point of it: its code can only come from downcasting the typed cause. A message
/// that happens to contain "gateway" would be classified identically with or without the cause,
/// and would prove nothing about whether the cause survived.
const TRANSPORT_MESSAGE: &str = "endpoint closed the connection mid-read";

/// A reader that answers once with a shortfall - so a request IS created and there is a funding
/// state to attach - and then fails with a TYPED cause, the kind `classify_error` reads by
/// downcasting rather than by matching words.
struct ShortfallThenTypedFailure {
    reads: Cell<usize>,
    failure: fn() -> anyhow::Error,
}

impl ShortfallThenTypedFailure {
    fn new(failure: fn() -> anyhow::Error) -> Self {
        Self {
            reads: Cell::new(0),
            failure,
        }
    }

    /// The named case: a gateway `DexdoError`, classified from its code.
    fn gateway() -> anyhow::Error {
        anyhow::Error::new(dexdo_core::DexdoError::new(
            dexdo_core::error_codes::E_GATEWAY_UNREACHABLE,
            "decrypted seller gateway could not be reached",
        ))
    }

    /// A transport `ChainError`, whose code the words cannot reproduce.
    fn transport() -> anyhow::Error {
        anyhow::Error::new(dexdo_core::ChainError::Transport(
            TRANSPORT_MESSAGE.to_string(),
        ))
    }
}

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for ShortfallThenTypedFailure {
    async fn hot_balances(&self, _hot: &dexdo_core::CanonicalAddress) -> Result<HotBalances> {
        self.reads.set(self.reads.get() + 1);
        if self.reads.get() == 1 {
            return ScriptedChain::shell(0).map_err(|reason| anyhow!("{reason}"));
        }
        Err((self.failure)())
    }
}

/// The call production makes: the wallet's turn is already held by the command, so the funding
/// mechanism takes no second lock.
async fn run<R: HotBalanceReader>(
    dir: &Path,
    chain: &R,
    provider: &ScriptedProvider,
    bounds: FundingWaitBounds,
) -> Result<FundedHot> {
    ensure_hot_funded_with_turn(
        &HotFundingContext {
            binding: &binding(),
            requirements: &requirements(),
            operation: "note deploy",
            creator_pubkey: CREATOR_PUBKEY,
            data_dir: dir,
            bounds,
        },
        HotTurn::AlreadyHeldByCaller,
        chain,
        provider,
    )
    .await
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp data dir")
}

fn pending() -> RequestPresence {
    RequestPresence::Present {
        transaction_hash: Some("tx".to_string()),
        pending_transaction_id: Some("7".to_string()),
        chain_created_at_unix: None,
    }
}

// ---------------------------------------------------------------------------------------------
// Item 4: the retry of a transient read lives inside the WHOLE funding timeout
// ---------------------------------------------------------------------------------------------

/// A read that got no answer does not end the wait while the operator's window is still open.

/// The shared per-read retry is sized for one read - five attempts inside forty-five seconds - and
/// that is right for every other caller. Here it is the inner loop: when it gives up, the balance
/// is still unknown, and "unknown" is precisely the state `--funding-timeout` was raised to sit
/// through. Before this, one exhausted read ended a ten-minute wait in forty-five seconds.

/// Both sides of the request are covered: the failure before anything is arranged must not skip
/// the arrangement, and the failure after it must not lose the wait that the request was created
/// for.
#[tokio::test]
async fn a_read_that_got_no_answer_keeps_waiting_inside_the_funding_window() {
    let dir = temp();
    let chain = ScriptedChain::new(
        vec![
            // Before the request exists.
            Err(no_answer_error()),
            // Now readable, and short - this is what makes the request happen.
            ScriptedChain::shell(0),
            // After the request exists, while the transfer is in flight.
            Err(no_answer_error()),
            Err(no_answer_error()),
        ],
        ScriptedChain::shell(REQUIRED_SHELL),
    );
    // Absent, so the request is created; then the finalized verdict that retires it once the
    // balance has arrived, which is the reconciliation a real successful funding goes through.
    let provider = ScriptedProvider::new(RequestPresence::Absent)
        .with_probes(vec![RequestPresence::Absent, executed()]);

    let funded = run(dir.path(), &chain, &provider, patient_bounds())
        .await
        .expect("a chain that never answered is not a verdict, and the balance did arrive");

    assert_eq!(
        funded.observed.get(SHELL),
        REQUIRED_SHELL,
        "the wait must return the balance it actually observed"
    );
    assert_eq!(
        chain.reads.get(),
        5,
        "every read must be repeated inside the funding window, not just the first"
    );
    assert_eq!(
        provider.submits.get(),
        1,
        "a repeated read must never turn into a repeated transfer"
    );
}

/// An answer that will not change is not waited out, and it is the answer that surfaces.

/// Two halves, because the fix must not have turned every failure into a ten-minute wait:
/// a read that answers "this input cannot even be encoded" leaves on its first read, and a final
/// answer arriving after an unanswered one is the error the operator is handed.
#[tokio::test]
async fn a_final_read_failure_leaves_the_funding_window_at_once() {
    let dir = temp();
    let chain = ScriptedChain::new(vec![], Err(FINAL_ERROR.to_string()));
    let provider = ScriptedProvider::new(RequestPresence::Absent);

    let started = std::time::Instant::now();
    let error = run(dir.path(), &chain, &provider, patient_bounds())
        .await
        .expect_err("an unencodable read is an answer, and the answer is no");
    let elapsed = started.elapsed();

    assert!(
        format!("{error:#}").contains(FINAL_ERROR),
        "the final answer must be the error the operator sees: {error:#}"
    );
    assert_eq!(
        chain.reads.get(),
        1,
        "a final answer must not be read a second time"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "a final answer must not sit out the funding window ({elapsed:?} of a 30s budget)"
    );

    // And the same discrimination once the wait is already running: the unanswered read is sat
    // through, the final one that follows ends it immediately and is what surfaces.
    let dir = temp();
    let chain = ScriptedChain::new(vec![Err(no_answer_error())], Err(FINAL_ERROR.to_string()));
    let provider = ScriptedProvider::new(RequestPresence::Absent);
    let error = run(dir.path(), &chain, &provider, patient_bounds())
        .await
        .expect_err("the final answer ends the wait");
    assert!(
        format!("{error:#}").contains(FINAL_ERROR),
        "the unanswered read must not mask the final answer that followed it: {error:#}"
    );
    assert_eq!(
        chain.reads.get(),
        2,
        "exactly one repeat, then the final answer"
    );
}

// ---------------------------------------------------------------------------------------------
// Item 8: the machine error envelope names the funding state
// ---------------------------------------------------------------------------------------------

/// Build the envelope exactly as `main.rs` does for a failing `--json` run.
fn envelope(error: &anyhow::Error) -> serde_json::Value {
    let code = machine::classify_error(OP_NOTE_DEPLOY, error);
    let machine_error = machine::machine_error(OP_NOTE_DEPLOY, code, error);
    serde_json::to_value(&machine_error).expect("serialize the machine error envelope")
}

/// A failure that leaves a Vault request behind says so in the machine envelope.

/// The one question a machine consumer cannot answer from an exit code is whether money has
/// already left the Vault. `funding_notice` on the success object answers it; before this, the
/// timeout and the read failure answered it only in `stderr` prose, which no orchestrator parses.

/// Both endings are covered - the wait's own timeout and a read that answered no - and both
/// funding states an operator can be left in: a request this run created, and a request an earlier
/// run created that is still in the queue.
#[tokio::test]
async fn a_failed_wait_names_the_funding_state_it_left_behind() {
    // The wait's budget runs out with a request this run created.
    let dir = temp();
    let chain = ScriptedChain::new(vec![], ScriptedChain::shell(0));
    let provider = ScriptedProvider::new(RequestPresence::Absent);
    let error = run(dir.path(), &chain, &provider, spent_bounds())
        .await
        .expect_err("a Hot that never reaches the requirement times out");
    assert_eq!(provider.submits.get(), 1, "a request was in fact created");
    assert_eq!(
        envelope(&error)["funding_notice"],
        serde_json::json!({ "event": "request_submitted" }),
        "a timeout that created a Vault request must say so in the machine envelope"
    );

    // A read that answered no, with a request an EARLIER run created still in the queue.
    let dir = temp();
    let seed_chain = ScriptedChain::new(vec![], ScriptedChain::shell(0));
    let seed_provider = ScriptedProvider::new(RequestPresence::Absent);
    let _ = run(dir.path(), &seed_chain, &seed_provider, spent_bounds())
        .await
        .expect_err("the seed run leaves its request pending");
    assert_eq!(seed_provider.submits.get(), 1);
    let chain = ScriptedChain::new(vec![ScriptedChain::shell(0)], Err(FINAL_ERROR.to_string()));
    let provider = ScriptedProvider::new(pending());
    let error = run(dir.path(), &chain, &provider, patient_bounds())
        .await
        .expect_err("a final read failure ends the wait");
    assert_eq!(
        provider.submits.get(),
        0,
        "a pending request of ours is never submitted twice"
    );
    assert_eq!(
        envelope(&error)["funding_notice"],
        serde_json::json!({ "event": "request_already_pending" }),
        "a failure with a pending Vault request must name it, not just the error"
    );
}

/// The field means what it says: nothing was arranged, so nothing is claimed.

/// `already_funded` is a state the operator may be in for real, so it must never be produced by a
/// run that arranged nothing. An absent `funding_notice` is the answer "no funding request of this
/// run exists", and a consumer branches on it.
#[tokio::test]
async fn a_failure_before_any_request_carries_no_funding_state() {
    let dir = temp();
    let chain = ScriptedChain::new(vec![], Err(FINAL_ERROR.to_string()));
    let provider = ScriptedProvider::new(RequestPresence::Absent);
    let error = run(dir.path(), &chain, &provider, patient_bounds())
        .await
        .expect_err("an unencodable read is an answer, and the answer is no");
    assert_eq!(provider.submits.get(), 0, "nothing was arranged");
    let envelope = envelope(&error);
    assert!(
        envelope.get("funding_notice").is_none(),
        "a run that arranged nothing must claim no funding state: {envelope}"
    );
}

/// The envelope carries the funding EVENT and no secret alongside it.

/// The named fields are the whole contract: `schema`, `operation`, `code`, `message`, `cause`,
/// `retryable` and `funding_notice`. The funding half contributes exactly one name from a closed
/// set - never the creator key, the Vault address, the provider's own words or a local path.
#[tokio::test]
async fn the_funding_state_in_the_envelope_is_one_closed_set_name_and_no_secret() {
    let dir = temp();
    let chain = ScriptedChain::new(vec![], ScriptedChain::shell(0));
    let provider = ScriptedProvider::new(RequestPresence::Absent);
    let error = run(dir.path(), &chain, &provider, spent_bounds())
        .await
        .expect_err("a Hot that never reaches the requirement times out");

    let envelope = envelope(&error);
    let object = envelope.as_object().expect("the envelope is one object");
    let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "cause",
            "code",
            "funding_notice",
            "message",
            "operation",
            "retryable",
            "schema",
        ],
        "the failing funding envelope carries exactly the documented fields"
    );
    assert_eq!(envelope["schema"], machine::ERROR_SCHEMA);
    assert_eq!(envelope["operation"], OP_NOTE_DEPLOY);

    let funding = envelope["funding_notice"]
        .as_object()
        .expect("funding_notice is an object");
    assert_eq!(
        funding.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["event"],
        "funding_notice carries the event and nothing else"
    );
    assert_eq!(funding["event"], "request_submitted");

    // The state is carried by a wrapper that renders as its own source's first line, so the naive
    // rendering of the whole error would print that line twice. `cause` says it once, and still
    // says it: a machine consumer reads this field, and 430 duplicated characters in it are a
    // defect of the same envelope item 8 is about.
    let cause = envelope["cause"].as_str().expect("cause is a string");
    assert_eq!(
        cause.matches("timed out after").count(),
        1,
        "the operator message must appear once in `cause`, not once per error layer: {cause}"
    );
    // rewrote the wording; the machine field must still carry the WHOLE message, which is
    // what this asserts -- the instruction to run it again, and the detail with the raw figures a
    // consumer reads.
    let lower = cause.to_ascii_lowercase();
    assert!(
        lower.contains("run the same command again") || lower.contains("re-run the same command"),
        "and it must still be the whole message: {cause}"
    );
    assert!(
        cause.contains("still missing"),
        "the detail with the raw figures stays in the envelope: {cause}"
    );

    let rendered = serde_json::to_string(&envelope).expect("serialize");
    let vault = vault_address();
    let data_dir = dir.path().to_str().expect("utf-8 data dir").to_string();
    for secret in [CREATOR_PUBKEY, vault.as_str(), data_dir.as_str()] {
        assert!(
            !rendered.contains(secret),
            "the machine envelope must not carry {secret}: {rendered}"
        );
    }
}

/// Every funding state keeps its stable machine-contract event name.
#[test]
fn the_funding_event_name_is_the_one_serde_writes() {
    for (notice, expected) in [
        (MachineFundingNotice::AlreadyFunded, "already_funded"),
        (
            MachineFundingNotice::VoucherSubmittedWaitingEventOrProof,
            "voucher_submitted_waiting_event_or_proof",
        ),
        (MachineFundingNotice::RequestSubmitted, "request_submitted"),
        (
            MachineFundingNotice::RequestAlreadyPending,
            "request_already_pending",
        ),
        (MachineFundingNotice::RequestExecuted, "request_executed"),
        (
            MachineFundingNotice::RequestIndeterminate,
            "request_indeterminate",
        ),
        (
            MachineFundingNotice::ManualTopUpRequested,
            "manual_top_up_requested",
        ),
    ] {
        let serialized = serde_json::to_value(notice).expect("serialize the notice");
        assert_eq!(
            serialized["event"], expected,
            "the serialized event must remain stable: {serialized}"
        );
    }
}

/// Carrying a funding state through an error must not rewrite what the operator reads.
#[test]
fn the_funding_context_preserves_the_operator_message() {
    let wrapped = machine::FundingContext::wrap(
        MachineFundingNotice::RequestSubmitted,
        anyhow!("the message the operator reads"),
    );
    assert_eq!(wrapped.to_string(), "the message the operator reads");
    assert_eq!(
        wrapped
            .downcast_ref::<machine::FundingContext>()
            .expect("typed funding context")
            .notice(),
        MachineFundingNotice::RequestSubmitted
    );
}

/// Naming the funding state must not silently rename the failure.

/// The envelope gained `funding_notice`; it must not have given up `code` to get it. `code` is what
/// an orchestrator branches on, and `classify_error` picks it by DOWNCASTING the causes - a
/// `DexdoError` carrying `E_GATEWAY_UNREACHABLE` or `E_GATEWAY_WRONG_ENDPOINT`, a
/// `DealHandleSchemaTooNew` - before it ever looks at words. So the state has to be attached in a
/// way that leaves the error an error.

/// The first shape of this fix did not: it rendered the cause to a string and rebuilt the error
/// around it, which erased every typed cause and quietly demoted a gateway failure to the
/// message-matched fallback. Both halves below are the guard against that returning.
#[tokio::test]
async fn attaching_the_funding_state_does_not_change_the_error_code() {
    // The transport case is the one with teeth: its code exists ONLY in the typed cause. Pinned
    // here, so a message that later grew classifiable words could not turn this guard into a
    // tautology without saying so.
    assert_eq!(
        machine::classify_error(OP_NOTE_DEPLOY, &anyhow!("{TRANSPORT_MESSAGE}")),
        ErrorCode::Internal,
        "the transport message must carry no vocabulary of its own, or the case proves nothing"
    );

    for (case, failure, expected) in [
        (
            "transport",
            ShortfallThenTypedFailure::transport as fn() -> anyhow::Error,
            ErrorCode::ChainTransport,
        ),
        (
            "gateway",
            ShortfallThenTypedFailure::gateway as fn() -> anyhow::Error,
            ErrorCode::GatewayConnectFailed,
        ),
    ] {
        let dir = temp();
        let chain = ShortfallThenTypedFailure::new(failure);
        let provider = ScriptedProvider::new(RequestPresence::Absent);
        let error = run(dir.path(), &chain, &provider, patient_bounds())
            .await
            .expect_err("a typed failure is an answer, and the answer is no");
        assert_eq!(
            provider.submits.get(),
            1,
            "{case}: the request must exist, or there is no funding state to attach"
        );

        let bare = machine::classify_error(OP_NOTE_DEPLOY, &failure());
        assert_eq!(
            bare, expected,
            "{case}: the unwrapped cause names its own code"
        );
        assert_eq!(
            machine::classify_error(OP_NOTE_DEPLOY, &error),
            bare,
            "{case}: carrying the funding state must not change the code - the typed cause has to \
             stay downcastable, not be flattened into a message"
        );
        let envelope = envelope(&error);
        assert_eq!(
            envelope["funding_notice"],
            serde_json::json!({ "event": "request_submitted" }),
            "{case}: and the state still reaches the envelope through the preserved chain"
        );
        // De-duplicating `cause` must not cost depth: what the typed cause itself says is still
        // there. Only the wrapper's repeat of the first line is dropped.
        assert!(
            envelope["cause"]
                .as_str()
                .expect("cause is a string")
                .contains(&failure().to_string()),
            "{case}: the cause the error actually carried must survive in `cause`: {}",
            envelope["cause"]
        );
    }

    // And the operator-facing message is not lost: the timeout still reads in full.
    let dir = temp();
    let chain = ScriptedChain::new(vec![], ScriptedChain::shell(0));
    let provider = ScriptedProvider::new(RequestPresence::Absent);
    let error = run(dir.path(), &chain, &provider, spent_bounds())
        .await
        .expect_err("a Hot that never reaches the requirement times out");
    assert_eq!(
        machine::classify_error(OP_NOTE_DEPLOY, &error),
        ErrorCode::InsufficientBalance
    );
    assert!(
        format!("{error:#}").contains("note deploy: timed out after"),
        "the operator-facing message must survive in full: {error:#}"
    );
}
