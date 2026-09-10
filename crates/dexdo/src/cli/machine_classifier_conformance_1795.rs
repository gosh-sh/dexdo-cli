//! two classifier verdicts brought back to the codes `runtime-machine-contract.md` already
//! publishes.

//! `runtime-machine-contract.md:1075` requires contract tests for a code on the stable `v1` surface.
//! Every assertion here is on the CODE and on `retryable` -- the two fields the contract tells a
//! machine to branch on (`:152`) -- and never on the sentence. Each case keeps the word that used to
//! decide it and asserts the word is still present, because a fix by rewording would leave the
//! classifier just as wrong for the next message carrying that word. `machine.rs` says as much
//! directly above these rules: a reworded message can change the emitted code. This removes that
//! fragility for these two conditions rather than moving it.

use super::{classify_error, ErrorCode, OP_CLOSE, OP_DEALS};

/// `:1067` DISPUTED_DEAL = "Deal is disputed and cannot be closed by the requested command" -- the
/// exact opposite of this refusal. The classifier lower-cases before matching, so "deal is not
/// DISPUTED" contains "disputed": the word matched inside its own negation and the consumer was told
/// the reverse of the fact.
#[test]
fn a_deal_that_is_not_disputed_is_no_longer_reported_as_a_disputed_deal() {
    let error = anyhow::Error::new(crate::cli::recover::DealIsNotDisputed::new(
        "resolve-dispute-timeout: deal is not DISPUTED - nothing to resolve",
    ));
    let code = classify_error(OP_CLOSE, &error);

    assert_ne!(code, ErrorCode::DisputedDeal);
    assert_eq!(code, ErrorCode::InvalidArgument);
    assert!(!code.retryable(), ":1049 is retryable false");
    assert!(
        error.to_string().to_lowercase().contains("disputed"),
        "the trap word must still be in the message: {error}"
    );
}

/// The trap is real: the same sentence carried by a plain error still lands on the old code, so it is
/// the TYPE that decides and not the wording.
#[test]
fn the_same_sentence_without_the_type_still_lands_on_the_old_code() {
    let plain =
        anyhow::anyhow!("resolve-dispute-timeout: deal is not DISPUTED - nothing to resolve");
    assert_eq!(classify_error(OP_CLOSE, &plain), ErrorCode::DisputedDeal);
}

/// A genuinely disputed deal is untouched.
#[test]
fn a_genuinely_disputed_deal_still_reports_disputed_deal() {
    let error = anyhow::anyhow!("close: seller deal 0:aa is disputed; use `dexdo release-dispute`");
    assert_eq!(classify_error(OP_CLOSE, &error), ErrorCode::DisputedDeal);
}

/// `:1057` HANDOVER_DECRYPT_FAILED = "Handover is present but malformed or not decryptable by this
/// note". The message the client emits is "malformed handover: invalid bytes", and with the generic
/// `invalid` rule ahead of it this code could never be emitted at all -- the operator was told their
/// command INPUT was invalid when a seller's handover was undecryptable.

/// This is the one case in this change that makes a code START being emitted rather than replacing a
/// wrong one with a right one.
#[test]
fn a_malformed_handover_reaches_its_own_code_instead_of_invalid_argument() {
    let error = anyhow::anyhow!("malformed handover: invalid bytes");
    let code = classify_error(OP_CLOSE, &error);

    assert_eq!(code, ErrorCode::HandoverDecryptFailed);
    assert_ne!(code, ErrorCode::InvalidArgument);
    assert!(!code.retryable(), ":1057 is retryable false");
    assert!(
        error.to_string().contains("invalid"),
        "the trap word must still be in the message: {error}"
    );
}

/// The reorder must not have swallowed the rule it was moved in front of.
#[test]
fn an_ordinary_invalid_argument_still_reports_invalid_argument() {
    for text in [
        "--nonce is required and must be UNIQUE per deal",
        "invalid --market path",
        "pass --note-addr or --market",
        "could not parse the deal handle",
    ] {
        assert_eq!(
            classify_error(OP_DEALS, &anyhow::anyhow!("{text}")),
            ErrorCode::InvalidArgument,
            "the moved rule swallowed: {text}"
        );
    }
}

/// And the neighbours that were already right do not move.
#[test]
fn the_codes_that_were_already_correct_do_not_move() {
    for (text, want) in [
        (
            "buyer quote: no liquidity for the requested model",
            ErrorCode::NoLiquidity,
        ),
        (
            "buyer quote: incomplete quote filled_ticks=1",
            ErrorCode::IncompleteQuote,
        ),
        (
            "endpoint readiness /v1/models failed",
            ErrorCode::EndpointReadinessFailed,
        ),
        (
            "gateway reachability check failed at host",
            ErrorCode::GatewayConnectFailed,
        ),
        (
            "streamStop settlement submission failed",
            ErrorCode::SettlementFailed,
        ),
        ("chain returned exit_code=101", ErrorCode::ChainRevert),
        (
            "insufficient balance for the selected action",
            ErrorCode::InsufficientBalance,
        ),
        (
            "close sellerStop returned an authoritative receipt",
            ErrorCode::GatewayAuthFailed,
        ),
    ] {
        assert_eq!(
            classify_error(OP_CLOSE, &anyhow::anyhow!("{text}")),
            want,
            "moved: {text}"
        );
    }
}
