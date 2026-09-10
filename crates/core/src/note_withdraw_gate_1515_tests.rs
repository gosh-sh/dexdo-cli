//! for each of the eleven `withdrawTokens` gates, the reading names THAT gate.

//! Directive `money-lost-in-tests-is-lost-for-users.md` asks for exactly this shape, and it
//! asks for it per gate rather than in aggregate. The reason is in the finding: a test that checked
//! only the text "not busy" would have passed on the day the defect was found, because "not busy"
//! was TRUE -- it was the other nine unread gates that held the money. So the assertion here is
//! never "the line is non-empty"; it is "the line names the field the contract would have stopped
//! on", once per field.

use super::*;
use serde_json::json;

/// A note with every one of the eleven gates closed. Each test opens exactly one.

/// Written out in full rather than built by a loop so that a gate added to the contract does not
/// quietly acquire a default here: a new gate has to be typed into this fixture, and the count
/// assertion below fails until it is.
fn all_gates_closed() -> serde_json::Value {
    json!({
        "_hasWithdrawn": false,
        "_busy": null,
        "_stakes": {},
        "_debt": "0",
        "_lockedInOrders": { "2": "0" },
        "_pendingPlaceBuyLock": "0",
        "_pendingBatchBuyLock": "0",
        "_openOrderCount": 0,
        "_restingInf": {},
        "_pendingInf": {},
        "_liveDeals": {},
    })
}

fn held_gate(fields: &serde_json::Value) -> WithdrawGate {
    match note_withdraw_gate_from_storage(fields) {
        NoteWithdrawGate::Held(gate) => gate,
        other => panic!("expected a held gate, got {other:?}"),
    }
}

/// The fixture itself must be clean, or every case below would pass for the wrong reason.
#[test]
fn a_note_with_all_eleven_gates_closed_reads_clear() {
    assert_eq!(
        note_withdraw_gate_from_storage(&all_gates_closed()),
        NoteWithdrawGate::Clear
    );
}

/// One case per gate: open it alone, and the reading must name it and carry its reading.
#[test]
fn each_of_the_eleven_gates_is_named_when_it_is_the_one_holding() {
    let cases: Vec<(&str, serde_json::Value, WithdrawGate)> = vec![
        ("_hasWithdrawn", json!(true), WithdrawGate::HasWithdrawn),
        (
            "_busy",
            json!("0:2222222222222222222222222222222222222222222222222222222222222222"),
            WithdrawGate::Busy {
                with: "0:2222222222222222222222222222222222222222222222222222222222222222"
                    .to_string(),
            },
        ),
        (
            "_stakes",
            json!({ "0x7cefd55652469dd381746e792d7781fef69f5d8ea458c80bf6eaf782c2ff8fa9": { "tokenType": 2 } }),
            WithdrawGate::Stakes { count: 1 },
        ),
        ("_debt", json!("5"), WithdrawGate::Debt { raw: 5 }),
        (
            "_lockedInOrders",
            json!({ "2": "5000" }),
            WithdrawGate::LockedInOrders {
                token_type: 2,
                locked: 5000,
            },
        ),
        (
            "_pendingPlaceBuyLock",
            json!("7"),
            WithdrawGate::PendingPlaceBuyLock { raw: 7 },
        ),
        (
            "_pendingBatchBuyLock",
            json!("9"),
            WithdrawGate::PendingBatchBuyLock { raw: 9 },
        ),
        (
            "_openOrderCount",
            json!(3),
            WithdrawGate::OpenOrders { count: 3 },
        ),
        (
            "_restingInf",
            json!({ "0x11": true }),
            WithdrawGate::RestingInference { count: 1 },
        ),
        (
            "_pendingInf",
            json!({ "42": "100" }),
            WithdrawGate::PendingInference { count: 1 },
        ),
        (
            "_liveDeals",
            json!({ "0:3333333333333333333333333333333333333333333333333333333333333333": true }),
            WithdrawGate::LiveDeals { count: 1 },
        ),
    ];

    // Every gate the contract has is covered exactly once, so a gate cannot be dropped from this
    // list without the count failing.
    assert_eq!(cases.len(), WITHDRAW_GATE_FIELDS.len());
    for (field, _, _) in &cases {
        assert!(
            WITHDRAW_GATE_FIELDS.contains(field),
            "{field} is not one of the declared gates"
        );
    }

    for (field, open_value, expected) in cases {
        let mut fields = all_gates_closed();
        fields[field] = open_value;
        let gate = held_gate(&fields);
        assert_eq!(gate, expected, "{field}: wrong gate named");
        assert_eq!(gate.field(), field, "{field}: gate names another field");

        // The line the operator actually reads names the field, not just the enum.
        let line = withdraw_gate_line(&NoteWithdrawGate::Held(gate.clone()));
        assert!(
            line.contains(field),
            "{field}: the line does not name it: {line}"
        );
        assert!(
            line.contains(&gate.exit_code().to_string()),
            "{field}: the line does not carry the exit code the operator saw: {line}"
        );
    }
}

/// The exit code each gate reports is the one `errors.sol` gives it, spelled out per gate.

/// This is what lets an operator match the line against the refusal already in their terminal: a
/// gate that named the wrong code would send them looking at the wrong condition.
#[test]
fn every_gate_reports_the_exit_code_its_require_raises() {
    assert_eq!(WithdrawGate::HasWithdrawn.exit_code(), 151);
    assert_eq!(
        WithdrawGate::Busy {
            with: "0:1".to_string()
        }
        .exit_code(),
        121
    );
    assert_eq!(WithdrawGate::Stakes { count: 1 }.exit_code(), 121);
    assert_eq!(WithdrawGate::Debt { raw: 1 }.exit_code(), 150);
    assert_eq!(
        WithdrawGate::LockedInOrders {
            token_type: 2,
            locked: 1
        }
        .exit_code(),
        144
    );
    assert_eq!(
        WithdrawGate::PendingPlaceBuyLock { raw: 1 }.exit_code(),
        144
    );
    assert_eq!(
        WithdrawGate::PendingBatchBuyLock { raw: 1 }.exit_code(),
        144
    );
    assert_eq!(WithdrawGate::OpenOrders { count: 1 }.exit_code(), 167);
    assert_eq!(WithdrawGate::RestingInference { count: 1 }.exit_code(), 167);
    assert_eq!(WithdrawGate::PendingInference { count: 1 }.exit_code(), 167);
    assert_eq!(WithdrawGate::LiveDeals { count: 1 }.exit_code(), 167);
}

/// THE FINDING ITSELF: `busyAddress: not busy` while `_stakes` holds the money.

/// This is the exact snapshot that was read off a live note and misread as "not locked". The gate
/// reading must call it held, and must call it held BY `_stakes`.
#[test]
fn the_note_that_read_not_busy_and_refused_121_is_named_as_stakes() {
    let mut fields = all_gates_closed();
    fields["_busy"] = json!(null);
    fields["_stakes"] = json!({
        "0x7cefd55652469dd381746e792d7781fef69f5d8ea458c80bf6eaf782c2ff8fa9": { "tokenType": 2 }
    });
    let reading = note_withdraw_gate_from_storage(&fields);
    assert_eq!(
        reading,
        NoteWithdrawGate::Held(WithdrawGate::Stakes { count: 1 })
    );
    let line = withdraw_gate_line(&reading);
    assert!(line.contains("_stakes"), "{line}");
    assert!(line.contains("121"), "{line}");
    // And it does not repeat the sentence that misled the operator.
    assert!(!line.contains("not busy"), "{line}");
}

/// A gate that could not be read is never reported as closed -- once per gate.

/// This is the anti-"picture of completeness" property. Reporting a note as withdrawable because a
/// field failed to decode is the same defect the module exists to remove, one level down.
#[test]
fn an_unreadable_gate_is_reported_as_unread_and_never_as_clear() {
    for field in WITHDRAW_GATE_FIELDS {
        let mut fields = all_gates_closed();
        fields
            .as_object_mut()
            .expect("object")
            .remove(field)
            .expect("the fixture declares every gate");
        let reading = note_withdraw_gate_from_storage(&fields);
        assert_eq!(
            reading,
            NoteWithdrawGate::Unreadable {
                field,
                reason: format!("{field} is absent from the decoded storage")
            },
            "{field}: a missing gate did not read as unreadable"
        );
        assert_ne!(reading, NoteWithdrawGate::Clear, "{field}");
        let line = withdraw_gate_line(&reading);
        assert!(line.contains(field), "{field}: {line}");
        assert!(
            line.contains("NOT a statement that the note can withdraw"),
            "{field}: the line does not disclaim a verdict it did not reach: {line}"
        );
    }
}

/// The contract stops at the FIRST failed require, so the reading does too.
#[test]
fn the_first_gate_in_contract_order_is_the_one_named() {
    let mut fields = all_gates_closed();
    // Open the last three at once; `_openOrderCount` comes first of them in the contract.
    fields["_openOrderCount"] = json!(2);
    fields["_restingInf"] = json!({ "0x11": true });
    fields["_liveDeals"] = json!({ "0:33": true });
    assert_eq!(
        held_gate(&fields),
        WithdrawGate::OpenOrders { count: 2 },
        "a later gate was reported over an earlier one"
    );

    // And `_hasWithdrawn` outranks everything, being the contract's first require.
    fields["_hasWithdrawn"] = json!(true);
    assert_eq!(held_gate(&fields), WithdrawGate::HasWithdrawn);
}

/// A `_lockedInOrders` entry that is present and zero closes the gate; only a non-zero one holds.

/// The contract iterates the map and requires each value to be zero, so an entry is not by itself
/// evidence of a lock -- reading it as one would refuse a note that can withdraw perfectly well.
#[test]
fn a_zero_locked_in_orders_entry_does_not_hold_the_note() {
    let mut fields = all_gates_closed();
    fields["_lockedInOrders"] = json!({ "2": "0", "3": "0" });
    assert_eq!(
        note_withdraw_gate_from_storage(&fields),
        NoteWithdrawGate::Clear
    );

    fields["_lockedInOrders"] = json!({ "2": "0", "3": "9" });
    assert_eq!(
        held_gate(&fields),
        WithdrawGate::LockedInOrders {
            token_type: 3,
            locked: 9
        }
    );
}

/// Both renderings of a map are read, so a decoder answering in the other shape cannot report a
/// full map as empty -- which would report a locked note as free.
#[test]
fn a_map_rendered_as_an_array_of_pairs_is_read_the_same_way() {
    let mut fields = all_gates_closed();
    fields["_lockedInOrders"] = json!([{ "key": 2, "value": "5000" }]);
    assert_eq!(
        held_gate(&fields),
        WithdrawGate::LockedInOrders {
            token_type: 2,
            locked: 5000
        }
    );

    let mut fields = all_gates_closed();
    fields["_liveDeals"] = json!([["0:33", true], ["0:44", true]]);
    assert_eq!(held_gate(&fields), WithdrawGate::LiveDeals { count: 2 });
}

/// `0x`-rendered integers are read, because the storage decoder emits them that way.
#[test]
fn hex_rendered_integers_are_read_as_numbers() {
    let mut fields = all_gates_closed();
    fields["_debt"] = json!("0x2a");
    assert_eq!(held_gate(&fields), WithdrawGate::Debt { raw: 42 });

    let mut fields = all_gates_closed();
    fields["_debt"] = json!("0x0");
    assert_eq!(
        note_withdraw_gate_from_storage(&fields),
        NoteWithdrawGate::Clear
    );
}

/// Every gate says what to do about it, because a named condition with no action is half a refusal
/// . `_hasWithdrawn` is the deliberate exception and says so.
#[test]
fn every_gate_names_something_to_do_or_says_there_is_nothing() {
    let gates = [
        WithdrawGate::HasWithdrawn,
        WithdrawGate::Busy {
            with: "0:1".to_string(),
        },
        WithdrawGate::Stakes { count: 1 },
        WithdrawGate::Debt { raw: 1 },
        WithdrawGate::LockedInOrders {
            token_type: 2,
            locked: 1,
        },
        WithdrawGate::PendingPlaceBuyLock { raw: 1 },
        WithdrawGate::PendingBatchBuyLock { raw: 1 },
        WithdrawGate::OpenOrders { count: 1 },
        WithdrawGate::RestingInference { count: 1 },
        WithdrawGate::PendingInference { count: 1 },
        WithdrawGate::LiveDeals { count: 1 },
    ];
    assert_eq!(gates.len(), WITHDRAW_GATE_FIELDS.len());
    for gate in gates {
        let step = gate.next_step();
        assert!(!step.is_empty(), "{}: no next step", gate.field());
        let actionable = step.contains("dexdo ")
            || step.contains("re-read before acting")
            || step.contains("resolve that counterparty")
            || step.contains("must be settled");
        assert!(
            actionable,
            "{}: next step is not actionable: {step}",
            gate.field()
        );
    }
}

/// The gate line carries the contract line number, so the operator can read the require itself.

/// re-points the value: `_liveDeals` moved to:2683 when the contract grew 181 lines above
/// `withdrawTokens`, and this pin still named the old one. What this test checks is the PLUMBING --
/// that the renderer carries a gate's number into the string it prints -- and that is worth
/// checking on its own. It was never the correctness check its name suggests: the number it
/// asserted was the table's own, so it agreed with the table however wrong the table was. Whether
/// the number is RIGHT is asked in `note_withdraw_gate_contract_line_1744_tests`, against the
/// vendored contract. The two links are different and neither implies the other.
#[test]
fn the_line_points_at_the_require_in_the_contract() {
    let gate = WithdrawGate::LiveDeals { count: 1 };
    let line = withdraw_gate_line(&NoteWithdrawGate::Held(gate.clone()));
    assert!(line.contains("PrivateNote.sol:2683"), "{line}");
    // Added with: the rendered number is this gate's own, not a constant that happens to
    // agree with it. Without this the literal above could outlive the table it is meant to mirror.
    assert!(
        line.contains(&format!("PrivateNote.sol:{}", gate.contract_line())),
        "the rendered line does not carry this gate's own contract_line(): {line}"
    );
}

/// The exit-code set and the gates cannot drift apart: asserted in BOTH directions.

/// One direction alone is not enough. "Every gate's code is in the set" passes with a set full of
/// codes no gate raises; "every code in the set is raised" passes with a set missing half the gates.
/// A refusal filter built on either mistake spends chain reads on refusals no gate explains, or
/// stays silent on the one the operator is holding.
#[test]
fn the_gate_exit_code_set_matches_the_gates_in_both_directions() {
    let gates = [
        WithdrawGate::HasWithdrawn,
        WithdrawGate::Busy {
            with: "0:1".to_string(),
        },
        WithdrawGate::Stakes { count: 1 },
        WithdrawGate::Debt { raw: 1 },
        WithdrawGate::LockedInOrders {
            token_type: 2,
            locked: 1,
        },
        WithdrawGate::PendingPlaceBuyLock { raw: 1 },
        WithdrawGate::PendingBatchBuyLock { raw: 1 },
        WithdrawGate::OpenOrders { count: 1 },
        WithdrawGate::RestingInference { count: 1 },
        WithdrawGate::PendingInference { count: 1 },
        WithdrawGate::LiveDeals { count: 1 },
    ];
    assert_eq!(gates.len(), WITHDRAW_GATE_FIELDS.len());
    for gate in &gates {
        assert!(
            WITHDRAW_GATE_EXIT_CODES.contains(&gate.exit_code()),
            "{} raises {} which is not in the declared set",
            gate.field(),
            gate.exit_code()
        );
    }
    for code in WITHDRAW_GATE_EXIT_CODES {
        assert!(
            gates.iter().any(|gate| gate.exit_code() == code),
            "{code} is declared but no gate raises it"
        );
    }
}

/// The refusal filter keys on the code, and lets through only refusals a gate can explain.
#[test]
fn only_a_refusal_a_gate_can_explain_is_worth_reading_state_for() {
    for code in WITHDRAW_GATE_EXIT_CODES {
        let text =
            format!("on-chain submit failed: exit_code={code} (dex::SOMETHING) stage=compute");
        assert!(
            refusal_carries_a_withdraw_gate_code(&text),
            "{code} is a gate code and was not recognised"
        );
    }
    // A gas failure is not a note-state problem, and must not spend a chain read or attract a
    // gate reading: "all eleven closed" beside a gas refusal is a true sentence that explains
    // nothing and reads as though it did.
    for text in [
        "on-chain submit failed: exit_code=102 (dex::ERR_LOW_VALUE) stage=compute",
        "block manager rejected message code=TVM_ERROR stage=action",
        "chain read got no answer after 3 attempts",
        "",
    ] {
        assert!(
            !refusal_carries_a_withdraw_gate_code(text),
            "no gate raises this and it was accepted: {text}"
        );
    }
    // Keyed on the code and not on a number appearing anywhere in the sentence.
    assert!(!refusal_carries_a_withdraw_gate_code(
        "transferred 121 SHELL; exit_code=0"
    ));
}

/// A clear reading states what was measured and REFUSES to predict the withdrawal.

/// The operator has already been given one complete-looking answer that was not one. "Nothing holds
/// the money" would be the second: they would read it as "the withdrawal will go through", and a
/// gas refusal a minute later would be a false answer following a false answer, after which the
/// line stops being read at all.
#[test]
fn a_clear_reading_does_not_promise_that_the_withdrawal_succeeds() {
    let line = withdraw_gate_line(&NoteWithdrawGate::Clear);
    // It says what it measured, and how many.
    assert!(line.contains("STATE gates"), "{line}");
    assert!(
        line.contains(&WITHDRAW_GATE_FIELDS.len().to_string()),
        "{line}"
    );
    // It names what it did NOT measure, rather than leaving the reader to assume it was
    // everything -- and it names BOTH halves, because "gas" alone still leaves amounts unstated.
    assert!(line.contains("gas"), "{line}");
    assert!(line.contains("amounts"), "{line}");
    assert!(
        line.contains("NOT a statement that a withdrawal will succeed"),
        "{line}"
    );

    // The disclaimer must carry the same weight the `Unreadable` branch gives its own, or the
    // limit is present in the source and absent to the reader. Measured, not eyeballed: the two
    // branches state their limit in the same form.
    let unreadable = withdraw_gate_line(&NoteWithdrawGate::Unreadable {
        field: "_stakes",
        reason: "absent".to_string(),
    });
    for branch in [&line, &unreadable] {
        assert!(
            branch.contains("NOT a statement that"),
            "a branch states its limit in a weaker form than the other: {branch}"
        );
    }
    // And it makes no claim of the form the operator would read as a guarantee.
    for promise in [
        "no note state blocks a withdrawal",
        "nothing holds the money",
        "the note can withdraw",
        "ready to withdraw",
    ] {
        assert!(
            !line.to_lowercase().contains(promise),
            "a clear reading promises an outcome it did not measure ({promise}): {line}"
        );
    }
}
