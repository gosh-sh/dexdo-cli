//! `dexdo oracle forfeit-stake` -- the third PMP exit, the one that costs the stake.

//! `PMP.forfeitStake` gates on the sender and nothing else, where `cancelStake` needs the event
//! cancelled, the book drained, and the freeze-time clean refund acknowledged. Three reachable
//! states leave `cancel-stake` refusing forever while `_stakes` keeps `withdrawTokens` and
//! `initTransfer` shut -- freezing the note's WHOLE balance, not just the stake.

//! So the command is strictly better than its absence, on one condition: that the operator is told
//! the order. Forfeit, wait for the close and the credit, THEN withdraw. Withdrawing first is how
//! the stake is actually lost, because `PrivateNote.acceptFee` drops the credit on a withdrawn
//! note and nothing -- sweep included -- reaches money that was never credited.

//! Everything here is offline. What no offline test can show is the live half: that the record
//! really clears and the note really unfreezes.

use super::{forfeit_stake_consent, render_forfeit_epilogue};

/// THE SAFETY MUST BE SEEN TO FIRE. A flag nobody has watched refuse is not a flag.

/// Asserted on the real gate rather than a copy of its logic, and it is not cfg-gated, so this runs
/// under the default features CI actually builds.
#[test]
fn without_the_flag_the_command_refuses() {
    let refusal = forfeit_stake_consent(false)
        .expect_err("the default must refuse")
        .to_string();
    assert!(refusal.contains("refused"), "{refusal}");
    assert!(
        refusal.contains("--abandon-the-stake"),
        "the refusal must name the way through, or it is a wall rather than a gate: {refusal}"
    );
}

/// And the other direction, or the test above passes on a gate that refuses everything.
#[test]
fn with_the_flag_the_command_proceeds() {
    assert!(
        forfeit_stake_consent(true).is_ok(),
        "the flag must actually open the gate"
    );
}

/// The refusal states the PRICE. An operator who reads only this must know what is being spent.
#[test]
fn the_refusal_names_what_the_forfeit_costs() {
    let refusal = forfeit_stake_consent(false).unwrap_err().to_string();
    assert!(refusal.contains("ABANDONS"), "{refusal}");
    assert!(
        refusal.contains("forfeited mass"),
        "where the stake goes is the price: {refusal}"
    );
    assert!(
        refusal.contains("cancel-stake"),
        "the cheaper exit must be named first-class, not implied: {refusal}"
    );
}

/// The refusal states the ORDER, as an order and not as an aside.

/// This is the half that decides whether the command recovers the money or loses it, so it is
/// asserted as a numbered sequence rather than as the presence of the word "withdraw".
#[test]
fn the_refusal_states_the_order_as_a_sequence() {
    let refusal = forfeit_stake_consent(false).unwrap_err().to_string();
    assert!(refusal.contains("THE ORDER MATTERS"), "{refusal}");
    for step in ["1.", "2.", "3."] {
        assert!(refusal.contains(step), "step {step} missing: {refusal}");
    }
    let forfeit_at = refusal.find("forfeit the stake").expect("step 1");
    let wait_at = refusal.find("WAIT").expect("step 2");
    let withdraw_at = refusal.find("note withdraw`").expect("step 3");
    assert!(
        forfeit_at < wait_at && wait_at < withdraw_at,
        "the three steps must appear in the order they must be performed: {refusal}"
    );
}

/// It must NOT promise the stake back. The close is moved by other parties on their own schedule.

/// The forbidden shape is a bare promise; the required shape is the conditional. Asserted as the
/// presence of the condition rather than the absence of a word, because a future rewording can drop
/// any particular word and still promise.
#[test]
fn the_refusal_promises_nothing_and_states_the_condition() {
    let refusal = forfeit_stake_consent(false).unwrap_err().to_string();
    assert!(
        refusal.contains("only at close, and only if"),
        "the return is conditional twice over and the text must carry both conditions: {refusal}"
    );
    assert!(
        refusal.contains("has not withdrawn"),
        "the condition that actually loses the money must be named: {refusal}"
    );
    for promise in [
        "will be returned",
        "you will get it back",
        "is returned to you",
    ] {
        assert!(
            !refusal.contains(promise),
            "the refusal promises a return it cannot promise: {promise:?}"
        );
    }
}

/// The result says what was confirmed, and says the unmoved balance is CORRECT.

/// The shared exit line prints `note_balance=N->N` here, because a forfeit moves nothing now. Left
/// alone that reads as "nothing happened" or as "confirmed" meaning the money is back.
#[test]
fn the_result_explains_the_balance_that_did_not_move() {
    let out = render_forfeit_epilogue(7, 7);
    assert!(out.contains("stake record is gone"), "{out}");
    assert!(out.contains("did NOT move"), "{out}");
    assert!(out.contains("that is correct"), "{out}");
    assert!(out.contains("NEXT, IN THIS ORDER"), "{out}");
}

/// And if the balance DID move, the result says so rather than printing the reassuring sentence.

/// A forfeit does not credit. A run where the balance changed saw something else happen too, and
/// the line that says "this is correct" would be false there -- the one place this output could
/// mislead in the expensive direction.
#[test]
fn a_moved_balance_is_reported_instead_of_reassured_about() {
    let out = render_forfeit_epilogue(7, 9);
    assert!(!out.contains("did NOT move"), "{out}");
    assert!(out.contains("read it again before acting"), "{out}");
}
