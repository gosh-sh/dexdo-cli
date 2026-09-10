//! a wallet this client could not READ must never be reported as a wallet without funds.

//! The defect, measured before the fix: both exits of `get_account_retrying` -- transient after its
//! retries were exhausted, and permanent -- arrived as `INSUFFICIENT_BALANCE` with
//! `retryable: false`. Both halves were wrong, and in opposite directions. An operator was sent to
//! top up an account that may have been full, and a genuine outage was reported as final.

//! Two things are asserted here and they are different things. The CODE is asserted, never the
//! sentence. And the sentence is asserted to still contain the word the old substring rule keyed on
//! -- "balances" -- because a fix that worked by rewording would be a fix that breaks again the next
//! time anyone improves a message. Each test proves that trap directly: the same text, carried by a
//! plain error instead of the typed one, still classifies as `INSUFFICIENT_BALANCE`.

use super::*;
use crate::cli::machine::{classify_error, ErrorCode, OP_NOTE_DEPLOY};

const WALLET: &str = "0:1111111111111111111111111111111111111111111111111111111111111111";

/// A real transient failure: a connect refusal against a port nothing is listening on.

/// Self-checking on purpose -- the fixture asserts it really is a CONNECT failure, so it cannot
/// quietly become something else and take the test's meaning with it.
async fn transient_read_failure() -> anyhow::Error {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port to learn a closed one");
    let address = listener.local_addr().expect("local addr");
    drop(listener);
    let error = reqwest::Client::new()
        .get(format!("http://{address}/"))
        .send()
        .await
        .expect_err("connecting to a closed port must fail");
    assert!(
        error.is_connect(),
        "fixture is only meaningful as a connect failure: {error}"
    );
    anyhow::Error::new(error)
}

/// A permanent failure: no transport cause at all, so the retry loop would not have retried it.
fn permanent_read_failure() -> anyhow::Error {
    anyhow::anyhow!("account query rejected by the endpoint")
}

/// What the message alone would classify as, if nothing typed carried it. This is the trap the fix
/// leaves in place rather than edits away.
fn classified_from_the_words_alone(refusal: &anyhow::Error) -> ErrorCode {
    classify_error(OP_NOTE_DEPLOY, &anyhow::anyhow!("{refusal}"))
}

#[tokio::test]
async fn a_transient_wallet_read_is_retryable_and_is_not_a_balance_verdict() {
    let refusal = funding_wallet_read_failure(WALLET, transient_read_failure().await);
    let code = classify_error(OP_NOTE_DEPLOY, &refusal);

    assert_eq!(code, ErrorCode::ChainTransport);
    assert!(
        code.retryable(),
        "a transient read can succeed on the next attempt and must say so"
    );
    assert_ne!(code, ErrorCode::InsufficientBalance);

    // The trap is still in the sentence, and the sentence alone still falls into it.
    assert!(refusal.to_string().contains("balances"), "{refusal}");
    assert_eq!(
        classified_from_the_words_alone(&refusal),
        ErrorCode::InsufficientBalance,
        "the wording must still be caught by the text rule, or this test proves nothing"
    );
}

#[tokio::test]
async fn a_permanent_wallet_read_is_not_retryable_and_is_not_a_balance_verdict() {
    let refusal = funding_wallet_read_failure(WALLET, permanent_read_failure());
    let code = classify_error(OP_NOTE_DEPLOY, &refusal);

    assert_eq!(code, ErrorCode::AccountUnreadable);
    assert!(
        !code.retryable(),
        "reading it again cannot change a permanent refusal"
    );
    assert_ne!(code, ErrorCode::InsufficientBalance);

    assert!(refusal.to_string().contains("balances"), "{refusal}");
    assert_eq!(
        classified_from_the_words_alone(&refusal),
        ErrorCode::InsufficientBalance,
        "the wording must still be caught by the text rule, or this test proves nothing"
    );
}

/// The two halves are the point: one code for both is what made this issue, and the codes must
/// disagree about retrying.
#[tokio::test]
async fn the_two_halves_answer_the_retry_question_oppositely() {
    let transient = classify_error(
        OP_NOTE_DEPLOY,
        &funding_wallet_read_failure(WALLET, transient_read_failure().await),
    );
    let permanent = classify_error(
        OP_NOTE_DEPLOY,
        &funding_wallet_read_failure(WALLET, permanent_read_failure()),
    );

    assert_ne!(transient, permanent, "one code for both is the defect");
    assert!(transient.retryable());
    assert!(!permanent.retryable());
}

/// A new code is a new way to be wrong about its neighbours. This asserts the typed rule captures
/// only what it was written for, and that the failures which already classified correctly still do.
#[tokio::test]
async fn the_new_code_does_not_capture_what_was_already_classified_correctly() {
    // A genuine shortfall is still a shortfall -- the case `INSUFFICIENT_BALANCE` exists for.
    let genuine = anyhow::anyhow!("insufficient balance for the selected action");
    assert_eq!(
        classify_error(OP_NOTE_DEPLOY, &genuine),
        ErrorCode::InsufficientBalance
    );

    // A transport failure carrying no marker is still transport, not the new code.
    let bare_transport = transient_read_failure().await;
    assert_eq!(
        classify_error(OP_NOTE_DEPLOY, &bare_transport),
        ErrorCode::ChainTransport
    );

    // And an unrelated text-classified refusal is untouched.
    let liquidity = anyhow::anyhow!("buyer quote: no liquidity for required quote");
    assert_eq!(
        classify_error(OP_NOTE_DEPLOY, &liquidity),
        ErrorCode::NoLiquidity
    );

    for other in [genuine, bare_transport, liquidity] {
        assert_ne!(
            classify_error(OP_NOTE_DEPLOY, &other),
            ErrorCode::AccountUnreadable,
            "the new code captured a failure it was not written for: {other}"
        );
    }
}
