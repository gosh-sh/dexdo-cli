//! at the exit code: what the settlement receipt found reaches the process's status only when
//! the operator asks for it, and when they ask, zero means conservation was PROVEN.

//! Two rules are asserted here, and they are opposite ways round on purpose. The DEFAULT is that no
//! verdict fails: this command emits a stable reporting object whose consumers live outside this
//! tree, and an exit code that flipped on content would stop their scripts mid-run for a change
//! none of them asked for. That default is proved here so it cannot be changed by accident.

//! The OPT-IN rule is strict, and strict in the direction that is easy to get wrong: `unbalanced`,
//! `incomplete` and a receipt with no conservation block at all all fail. "Could not check" is not
//! "checked and fine" -- a gate that passes what it could not verify teaches its operator that a
//! green run means nothing.

//! Every fixture below is built through the production builders (`build_receipt`,
//! `unavailable_receipt`) rather than hand-assembled, and every case asserts the verdict it is
//! about BEFORE it asserts the exit status. A fixture that quietly drifted to another verdict would
//! otherwise pass the wrong test for the wrong reason.

use super::*;
use dexdo_core::{
    NoteDealCreditReceipt, TokenContractReceiptChainData, TokenContractSettlementEvent,
    TokenContractSettlementReceipt, TokenContractSettlementReceipts,
};

const DEAL: &str = "0:3f5a1c2d4e6b8a09f1e2d3c4b5a69788796a5b4c3d2e1f091827364554637281";
const BUYER: &str = "0:11c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2";
const SELLER: &str = "0:22d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3";

fn event(id: &str, at: u64, event: TokenContractSettlementEvent) -> TokenContractSettlementReceipt {
    TokenContractSettlementReceipt {
        message_id: id.to_string(),
        created_at: at,
        cursor: format!("cursor-{id}"),
        event,
    }
}

fn credit(note: &str, amount: u128, id: &str) -> NoteDealCreditReceipt {
    NoteDealCreditReceipt {
        note: note.to_string(),
        deal: DEAL.to_string(),
        amount,
        message_id: id.to_string(),
        created_at: 1_787_300_100,
        cursor: format!("cursor-{id}"),
    }
}

fn context() -> ReceiptContext {
    ReceiptContext {
        generated_at: 1_787_300_200,
        network: "net-a".to_string(),
        chain_endpoint: "https://net-a.example/graphql".to_string(),
        contracts_generation: Some("4.0.35".to_string()),
        expected_code_hash: None,
        token_contract: DEAL.to_string(),
        season: None,
    }
}

fn receipt(
    events: Vec<TokenContractSettlementReceipt>,
    note_credits: Vec<NoteDealCreditReceipt>,
    notes_read: Vec<String>,
) -> SettlementReceiptV1 {
    build_receipt(
        context(),
        &TokenContractReceiptChainData {
            account_id: DEAL.to_string(),
            account_active: false,
            code_hash: None,
            current: None,
            receipts: TokenContractSettlementReceipts { events },
            note_credits,
            notes_read,
        },
    )
}

fn funding() -> Vec<TokenContractSettlementReceipt> {
    vec![
        event(
            "9e1a70c4b2d85f36",
            1_787_300_000,
            TokenContractSettlementEvent::StreamFunded {
                buyer: BUYER.to_string(),
                deposit: 8_000_000_000,
            },
        ),
        event(
            "4c7b91d3a6e0f582",
            1_787_300_010,
            TokenContractSettlementEvent::SellerBondFunded {
                amount: 2_000_000_000,
            },
        ),
    ]
}

/// A settled deal that adds up: 10 SHELL funded, 9 credited to the two notes, 1 written off, and
/// the deal's own split agrees with what the notes report.
fn conserved() -> SettlementReceiptV1 {
    let mut events = funding();
    events.push(event(
        "b8f0a25d1c74e639",
        1_787_300_090,
        TokenContractSettlementEvent::StreamStopped {
            buyer: BUYER.to_string(),
            to_seller: 3_000_000_000,
            refund_to_buyer: 6_000_000_000,
        },
    ));
    receipt(
        events,
        vec![
            credit(BUYER, 6_000_000_000, "d3e5079a1b2c4d6f"),
            credit(SELLER, 3_000_000_000, "a1b2c3d4e5f60718"),
        ],
        vec![BUYER.to_string(), SELLER.to_string()],
    )
}

/// The same deal, paying out more than it was ever funded: the notes were credited 12 SHELL against
/// 10 funded in. This is the shape was opened about -- money that does not conserve.
fn unbalanced() -> SettlementReceiptV1 {
    let mut events = funding();
    events.push(event(
        "c9a1b34e2d85f470",
        1_787_300_090,
        TokenContractSettlementEvent::StreamStopped {
            buyer: BUYER.to_string(),
            to_seller: 5_000_000_000,
            refund_to_buyer: 7_000_000_000,
        },
    ));
    receipt(
        events,
        vec![
            credit(BUYER, 7_000_000_000, "e4f6180b2c3d5e7a"),
            credit(SELLER, 5_000_000_000, "b2c3d4e5f6071829"),
        ],
        vec![BUYER.to_string(), SELLER.to_string()],
    )
}

/// The same shape with different figures, so two refusals of one verdict differ in wording while
/// meaning the same thing. Used to prove the code does not follow the sentence.
fn unbalanced_with_other_figures() -> SettlementReceiptV1 {
    let mut events = funding();
    events.push(event(
        "d0b2c46f3e97a581",
        1_787_300_090,
        TokenContractSettlementEvent::StreamStopped {
            buyer: BUYER.to_string(),
            to_seller: 9_000_000_000,
            refund_to_buyer: 4_000_000_000,
        },
    ));
    receipt(
        events,
        vec![
            credit(BUYER, 4_000_000_000, "f5a7291c3d4e6b80"),
            credit(SELLER, 9_000_000_000, "c3d4e5f60718293a"),
        ],
        vec![BUYER.to_string(), SELLER.to_string()],
    )
}

/// A gone deal whose terminal split was never observed. Nothing says the money is wrong; nothing
/// says it is right either, and the identity was never evaluated at all.
fn incomplete() -> SettlementReceiptV1 {
    receipt(funding(), Vec::new(), Vec::new())
}

/// The chain could not be read. The command deliberately returns this receipt rather than an error,
/// and it carries no conservation block at all -- there is no money identity to judge.

/// Named for the absent block and NOT "unverified": PR1787 makes `unverified` a public value of
/// `conservation.status` meaning something else entirely (an identity that closed by construction).
/// The public name is older and weightier, so this one gives way.
fn no_conservation_block() -> SettlementReceiptV1 {
    unavailable_receipt(context())
}

/// PR1787's fourth `conservation.status`: the identity had only ONE account's word for both of its
/// sides, so it closed by construction and states nothing about the money.

/// Driven through the production builder rather than stamped onto a field. The condition is real and
/// reachable: with fewer than two notes read there is no second statement to check the deal's own
/// figure against, so `payout` IS `declared_payout` and `unexplained` is 0 for every input the chain
/// can produce -- which is exactly why that reading must not be called `conserved`.
fn identity_closed_by_construction() -> SettlementReceiptV1 {
    let mut events = funding();
    events.push(event(
        "b8f0a25d1c74e639",
        1_787_300_090,
        TokenContractSettlementEvent::StreamStopped {
            buyer: BUYER.to_string(),
            to_seller: 3_000_000_000,
            refund_to_buyer: 6_000_000_000,
        },
    ));
    // No note was read, so nothing independent confirms the deal's own split.
    receipt(events, Vec::new(), Vec::new())
}

fn status(receipt: &SettlementReceiptV1) -> &str {
    receipt
        .conservation
        .as_ref()
        .map(|conservation| conservation.status)
        .unwrap_or("<no conservation block>")
}

// -------------------------------------------------------------------------
// The opt-in gate: zero only when conservation was PROVEN.

// every refusal below is asserted on the CODE a machine consumer reads, never on the
// sentence it travels with. The generic classifier derives a code from message text, and these
// three refusals are the case that showed what that costs -- so each test states the exact code,
// and each also states that the wording WOULD have been caught by a text rule, which is what makes
// "the code is not read off the words" a measurement rather than a claim.
// -------------------------------------------------------------------------

use crate::cli::machine::ErrorCode;

fn refusal(receipt: &SettlementReceiptV1) -> ConservationRefusal {
    conservation_refusal(receipt).expect("this verdict must refuse under the gate")
}

#[test]
fn require_conserved_passes_a_deal_whose_money_is_proven_to_conserve() {
    let receipt = conserved();
    assert_eq!(status(&receipt), "conserved");
    assert!(
        conservation_refusal(&receipt).is_none(),
        "a proven-conserved deal must not refuse"
    );
    assert!(
        receipt_exit_status(&receipt, true).is_ok(),
        "a proven-conserved deal must exit zero under the gate"
    );
}

/// Money that does not conserve. Its own wording contains "balance" -- the word "unbalanced" does --
/// which is the rule that used to report this as a note needing a top-up.
#[test]
fn an_unbalanced_deal_refuses_with_a_contradiction_code_not_an_insufficient_balance_one() {
    let receipt = unbalanced();
    assert_eq!(status(&receipt), "unbalanced");
    let refusal = refusal(&receipt);

    assert_eq!(refusal.code, ErrorCode::ContradictoryState);
    assert!(
        !refusal.code.retryable(),
        "nothing was submitted; a retry repeats the finding"
    );
    // The wording the text classifier would have keyed on is still present, and no longer decides.
    assert!(
        refusal.cause.to_ascii_lowercase().contains("balance"),
        "this test is only meaningful while the wording would still be caught: {}",
        refusal.cause
    );
}

/// Conservation never evaluated. Its wording carries the receipt's own reason code
/// `terminal_settlement_event_absent`, whose "settlement" is what used to make this retryable.
#[test]
fn an_unevaluated_deal_refuses_with_a_contradiction_code_not_a_retryable_settlement_one() {
    let receipt = incomplete();
    assert_eq!(status(&receipt), "incomplete");
    let refusal = refusal(&receipt);

    assert_eq!(refusal.code, ErrorCode::ContradictoryState);
    assert!(
        !refusal.code.retryable(),
        "a permanent finding must never ask to be retried"
    );
    assert!(
        refusal.cause.to_ascii_lowercase().contains("settlement"),
        "this test is only meaningful while the wording would still be caught: {}",
        refusal.cause
    );
}

/// Nothing verified at all. Its wording matches no text rule, which is how it used to become
/// `INTERNAL` -- "this client has a bug" -- for an ordinary and expected outcome.
#[test]
fn a_receipt_with_no_conservation_block_refuses_with_a_named_code_not_internal() {
    let receipt = no_conservation_block();
    assert_eq!(status(&receipt), "<no conservation block>");
    let refusal = refusal(&receipt);

    assert_eq!(refusal.code, ErrorCode::ContradictoryState);
    assert_ne!(refusal.code, ErrorCode::Internal);
    assert!(
        !refusal.code.retryable(),
        "an unreadable chain is not a retry instruction here"
    );
}

/// PR1787's `unverified`: the identity closed against itself, so it says nothing about the money.
/// Like the absent-block case its wording matches no text rule, so `INTERNAL` is what it would have
/// become -- an "ordinary and expected outcome" reported as a client bug, which is's defect.
#[test]
fn an_identity_that_closed_by_construction_refuses_with_a_named_code_not_internal() {
    let receipt = identity_closed_by_construction();
    assert_eq!(status(&receipt), "unverified");
    let refusal = refusal(&receipt);

    assert_eq!(refusal.code, ErrorCode::ContradictoryState);
    assert_ne!(refusal.code, ErrorCode::Internal);
    assert!(
        !refusal.code.retryable(),
        "an identity that closes against itself closes the same way on every retry"
    );
    // Its own sentence, not the catch-all's: we know this verdict is coming and say what it means.
    assert!(
        refusal.cause.contains("closed by construction"),
        "the fourth verdict fell through to the generic arm: {}",
        refusal.cause
    );
    assert!(
        !refusal.cause.contains("is not conserved"),
        "that is the catch-all's wording: {}",
        refusal.cause
    );
}

/// The property, stated as states it: rewording the carrier must not move the code.

/// Two `unbalanced` receipts whose figures differ produce different sentences. If the code were
/// still derived from those sentences this would be the test that noticed.
#[test]
fn rewording_a_refusal_does_not_move_its_code() {
    let first = refusal(&unbalanced());
    let second = refusal(&unbalanced_with_other_figures());

    assert_ne!(
        first.cause, second.cause,
        "the two fixtures must differ in wording"
    );
    assert_eq!(first.code, second.code);
    assert_eq!(first.code, ErrorCode::ContradictoryState);
}

/// Three failures, three reasons. One code is deliberate -- they are one class of finding -- but the
/// operator still has to be told which of three situations they are in.
#[test]
fn each_failing_verdict_refuses_in_its_own_words() {
    let causes: Vec<String> = [
        unbalanced(),
        incomplete(),
        identity_closed_by_construction(),
        no_conservation_block(),
    ]
    .iter()
    .map(|receipt| refusal(receipt).cause)
    .collect();
    for (first, second) in [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)] {
        assert_ne!(
            causes[first], causes[second],
            "two verdicts refuse with the same sentence"
        );
    }
}

// -------------------------------------------------------------------------
// The default, proved from the same code path: no verdict reaches the exit code.
// -------------------------------------------------------------------------

#[test]
fn without_the_flag_a_conserved_deal_exits_zero() {
    let receipt = conserved();
    assert_eq!(status(&receipt), "conserved");
    assert!(receipt_exit_status(&receipt, false).is_ok());
}

#[test]
fn without_the_flag_an_unbalanced_deal_still_exits_zero() {
    let receipt = unbalanced();
    assert_eq!(status(&receipt), "unbalanced");
    assert!(
        receipt_exit_status(&receipt, false).is_ok(),
        "the default must keep reporting rather than failing"
    );
}

#[test]
fn without_the_flag_an_incomplete_deal_still_exits_zero() {
    let receipt = incomplete();
    assert_eq!(status(&receipt), "incomplete");
    assert!(receipt_exit_status(&receipt, false).is_ok());
}

#[test]
fn without_the_flag_a_receipt_with_no_conservation_block_still_exits_zero() {
    let receipt = no_conservation_block();
    assert_eq!(status(&receipt), "<no conservation block>");
    assert!(receipt_exit_status(&receipt, false).is_ok());
}

// -------------------------------------------------------------------------
// Help: every verdict says what it does to the exit code, in words.
// -------------------------------------------------------------------------

fn long_help() -> String {
    use clap::CommandFactory as _;
    let mut command = crate::Cli::command();
    let rendered = command
        .find_subcommand_mut("settlement-receipt")
        .expect("settlement-receipt is a subcommand")
        .render_long_help()
        .to_string();
    // Normalised because clap wraps to the terminal width: what is asserted is the wording, not
    // where the wrapper happened to break the line.
    rendered.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn help_states_what_every_verdict_does_to_the_exit_code() {
    let help = long_help();
    for wording in [
        "`conserved` -- what was funded in equals what was credited to the notes plus what was written off",
        "`unbalanced` -- the identity does not hold",
        "`incomplete` -- a term of the identity could not be read, so conservation was never evaluated. Exit code non-zero",
        "`unverified` -- the identity had only ONE account's word for both of its sides, so it closed by construction and states nothing about the money. Exit code non-zero",
        "No conservation block at all -- the chain read was unavailable, so this receipt carries no money identity to judge. Exit code non-zero",
    ] {
        assert!(help.contains(wording), "help does not say: {wording}\n{help}");
    }
    assert_eq!(
        help.matches("Exit code non-zero").count(),
        4,
        "each failing verdict states its own exit code:\n{help}"
    );
    assert!(help.contains("Exit code 0."), "{help}");
}

/// The default is written down as a decision, so a later reader cannot take it for an omission and
/// "fix" it.
#[test]
fn help_says_the_default_never_fails_and_that_this_was_decided() {
    let help = long_help();
    assert!(
        help.contains(
            "WITHOUT this flag the command never fails on any verdict, only on a read, parse or endpoint error."
        ),
        "{help}"
    );
    assert!(
        help.contains("That is a deliberate decision, not an oversight"),
        "{help}"
    );
}

/// A caller told to expect a hard stop must also be told what the stop does to stdout. The refusal
/// travels as a second JSON document behind the receipt, and finding that out from a parse error is
/// finding it out too late.
#[test]
fn help_warns_that_the_failing_path_prints_two_json_documents() {
    let help = long_help();
    assert!(
        help.contains(
            "On the failing path stdout therefore carries TWO JSON documents -- this receipt, then a `dexdo.error.v1` envelope -- so read stdout as a stream of values, not as one object"
        ),
        "{help}"
    );
}

/// The two assertions above normalise the rendered help before matching, so they are only worth
/// anything if the normalised text can still MISS. This feeds it wording that is not there.
#[test]
fn the_help_assertions_can_still_fail_to_match() {
    let help = long_help();
    for absent in [
        "Exit code 2.",
        "`conserved` -- the identity does not hold",
        "WITH this flag the command never fails on any verdict",
    ] {
        assert!(
            !help.contains(absent),
            "the help matcher accepts wording that is not there: {absent}"
        );
    }
}
