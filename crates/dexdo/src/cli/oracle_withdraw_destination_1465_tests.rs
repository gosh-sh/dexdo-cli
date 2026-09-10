//! `oracle withdraw-fees` is the one money-moving address argument with no check on where it
//! sends. These pin the three decisions that replace "any address that parses".

//! One mutant per step, because a step whose removal turns nothing red is not guarded:

//! * delete the destination-side confirmation -> `a_destination_that_never_received_is_not_confirmed`
//! * delete the classification -> `a_deployed_contract_is_refused_as_a_destination`
//! * delete the declared-destination route -> `a_declared_destination_is_admitted_with_no_custodians`

//! Every function under test is pure and takes an already-read fact, which is this file's own
//! convention (`validate_oracle_resolve_liquidity` takes the getter's `Result`). No chain, no seam.

use super::{
    admit_oracle_withdraw_destination, classify_oracle_withdraw_destination,
    oracle_fee_destination_outcome, oracle_fee_status_word, OracleFeeDestinationOutcome,
    OracleFeeDestinationReading, OracleWithdrawDestinationKind, OracleWithdrawDestinationProof,
};

const TO: &str = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The name/hash pairs the classifier walks.

/// Was a `BTreeMap` read out of the deployment manifest; removed that copy, and the pairs now
/// come from the images this build carries. The fixture keeps the same two names and the same two
/// stand-in hashes -- what is under test is the classification, not where the numbers were read.
fn deployed() -> Vec<(&'static str, String)> {
    vec![
        ("PrivateNote", "57e85fa6".to_string()),
        ("TokenContract", "a67e1ae0".to_string()),
    ]
}

fn oracle_key() -> String {
    "11".repeat(32)
}

// ---------------------------------------------------------------- step 1

/// The money left the Oracle and the destination we could read did not rise. Before this
/// printed `status=confirmed`, because the only question asked was whether the SENDER went down.
#[test]
fn a_destination_that_never_received_is_not_confirmed() {
    let outcome = oracle_fee_destination_outcome(
        &OracleFeeDestinationReading::Pocket(100),
        &OracleFeeDestinationReading::Pocket(100),
        50,
    );

    assert_eq!(
        outcome,
        OracleFeeDestinationOutcome::NotCredited {
            before: 100,
            after: 100
        }
    );
    assert_ne!(
        oracle_fee_status_word(&outcome),
        "confirmed",
        "a withdrawal whose destination did not rise must not be called confirmed"
    );
}

/// The predicate is `>=` and not `==` on purpose: a destination wallet receives other money while
/// this command polls, and equality would report a perfectly good withdrawal as a failure. This is
/// the assertion that dies if someone "tidies" it into the exact form the sender side uses.
#[test]
fn a_destination_that_also_received_something_else_is_still_confirmed() {
    let outcome = oracle_fee_destination_outcome(
        &OracleFeeDestinationReading::Pocket(100),
        &OracleFeeDestinationReading::Pocket(400),
        50,
    );

    assert_eq!(
        outcome,
        OracleFeeDestinationOutcome::Credited {
            before: 100,
            after: 400
        }
    );
    assert_eq!(oracle_fee_status_word(&outcome), "confirmed");
}

/// The third outcome. An endpoint that will not answer is an open question, and a question must not
/// be rendered as either answer -- least of all as `confirmed`.
#[test]
fn an_unread_destination_is_its_own_answer_and_not_confirmed() {
    let outcome = oracle_fee_destination_outcome(
        &OracleFeeDestinationReading::Pocket(100),
        &OracleFeeDestinationReading::Unreadable("endpoint refused".to_string()),
        50,
    );

    assert_eq!(
        outcome,
        OracleFeeDestinationOutcome::Unread("endpoint refused".to_string())
    );
    assert_eq!(
        oracle_fee_status_word(&outcome),
        "sender-only-destination-unread"
    );
    assert_ne!(oracle_fee_status_word(&outcome), "confirmed");
}

// ---------------------------------------------------------------- step 2

/// A dex contract is not a payout destination. The refusal is NOT justified by "the money is lost
/// there" -- `withdrawTokens` sweeps a note's raw pocket (`PrivateNote.sol:2510-2515`), so that
/// claim would be false. It is justified by costing the operator nothing to pass the right address.
#[test]
fn a_deployed_contract_is_refused_as_a_destination() {
    let kind =
        classify_oracle_withdraw_destination(true, "Active", true, Some("0x57E85FA6"), &deployed());
    assert_eq!(
        kind,
        OracleWithdrawDestinationKind::DeployedContract("PrivateNote".to_string()),
        "case and 0x prefix must not decide what a code hash is"
    );

    let error = admit_oracle_withdraw_destination(TO, &kind, &oracle_key(), &[], &[])
        .expect_err("a PrivateNote is not where oracle fees are withdrawn to")
        .to_string();
    assert!(error.contains("is a deployed PrivateNote"), "{error}");
    assert!(error.contains("not a wallet"), "{error}");
}

/// The address that parses and names nothing. Refused before the submit, so the question of what
/// becomes of ECC[2] sent to a non-existent account never has to be answered.
#[test]
fn an_address_naming_no_account_is_refused() {
    let kind = classify_oracle_withdraw_destination(false, "", false, None, &deployed());
    assert_eq!(kind, OracleWithdrawDestinationKind::NotFound);

    let error = admit_oracle_withdraw_destination(TO, &kind, &oracle_key(), &[], &[])
        .expect_err("an address naming no account is refused")
        .to_string();
    assert!(error.contains("names no account"), "{error}");
}

/// Neither a known contract nor a known wallet: nothing can be proved either way, so it is its own
/// refusal rather than being folded into "fine".
#[test]
fn an_unknown_code_hash_is_neither_admitted_nor_mistaken_for_a_contract() {
    let kind =
        classify_oracle_withdraw_destination(true, "Active", true, Some("deadbeef"), &deployed());
    assert_eq!(
        kind,
        OracleWithdrawDestinationKind::UnknownCode("deadbeef".to_string())
    );

    let error = admit_oracle_withdraw_destination(TO, &kind, &oracle_key(), &[], &[])
        .expect_err("an unknown destination is refused")
        .to_string();
    assert!(error.contains("code this build does not know"), "{error}");
}

// ---------------------------------------------------------------- step 3

/// THE named loss path of: an address that parses, exists, is Active, and is a real wallet --
/// just not the operator's. Nothing bounces and nothing refuses today; the money is delivered and
/// gone. Here the oracle key is not among its custodians, so it is refused before the submit.
#[test]
fn a_wallet_the_oracle_key_does_not_control_is_refused() {
    let someone_else = vec!["22".repeat(32)];
    let error = admit_oracle_withdraw_destination(
        TO,
        &OracleWithdrawDestinationKind::SupportedWallet,
        &oracle_key(),
        &someone_else,
        &[],
    )
    .expect_err("a wallet this Oracle's key does not control is not a proven destination")
    .to_string();

    assert!(error.contains("not one of its custodians"), "{error}");
    assert!(error.contains("Nothing was submitted"), "{error}");
}

/// The custodian route, and the renderings must not decide it: `getCustodians` returns the key as an
/// integer, so a left-truncated or `0x`-prefixed spelling is the same key.
#[test]
fn a_wallet_the_oracle_key_is_a_custodian_of_is_admitted() {
    let custodians = vec![dexdo_core::normalize_multisig_pubkey(&oracle_key()).unwrap()];
    let proof = admit_oracle_withdraw_destination(
        TO,
        &OracleWithdrawDestinationKind::SupportedWallet,
        &format!("0x{}", oracle_key()),
        &custodians,
        &[],
    )
    .expect("the Oracle's own key being a custodian is the proof of control");

    assert_eq!(proof, OracleWithdrawDestinationProof::Custodian);
}

/// The declared route, and the one that makes the custodian route survivable: an operator whose
/// oracle key is deliberately NOT a custodian of their payout wallet has no other way through, and
/// key separation is good practice rather than a corner case.

/// Custodians are empty here and the destination is deliberately not even classified as a wallet:
/// the declaration is checked first, needs no getter and no second secret. Delete that route and
/// this is the test that turns red.
#[test]
fn a_declared_destination_is_admitted_with_no_custodians() {
    let declared = vec![("hot_address", TO.to_string())];
    let proof = admit_oracle_withdraw_destination(
        TO,
        &OracleWithdrawDestinationKind::UnknownCode("deadbeef".to_string()),
        &oracle_key(),
        &[],
        &declared,
    )
    .expect("an address the operator declared for this network is theirs by declaration");

    assert_eq!(
        proof,
        OracleWithdrawDestinationProof::Declared("hot_address")
    );
}

/// A declaration is for ONE address, not for the idea of declaring. A neighbouring address is still
/// refused while a binding exists -- otherwise the route would admit anything once onboarded.
#[test]
fn a_declaration_admits_only_the_address_it_names() {
    let declared = vec![(
        "hot_address",
        "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
    )];
    let error = admit_oracle_withdraw_destination(
        TO,
        &OracleWithdrawDestinationKind::SupportedWallet,
        &oracle_key(),
        &[],
        &declared,
    )
    .expect_err("a binding for another address does not admit this one")
    .to_string();

    assert!(error.contains("not one of its custodians"), "{error}");
}
