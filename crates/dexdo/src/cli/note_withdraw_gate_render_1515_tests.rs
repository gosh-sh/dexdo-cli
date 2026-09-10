//! the `note balance` gate section answers, or says it did not check everything.

//! The section's whole reason for existing is that the two lines above it looked like a complete
//! answer while covering two of eleven conditions. So the assertions here are about COVERAGE being
//! stated, not about wording: a rendering that goes quiet, or that reports a partial check without
//! saying it was partial, rebuilds the defect.

use super::render_note_withdraw_gate;
use dexdo_core::{NoteWithdrawGate, WithdrawGate, WITHDRAW_GATE_FIELDS};

/// The section is always emitted, in every one of the three readings. Nothing is reported by
/// silence -- the same rule the `busyAddress` section follows.
#[test]
fn the_section_is_emitted_for_every_reading() {
    let readings = [
        NoteWithdrawGate::Clear,
        NoteWithdrawGate::Held(WithdrawGate::LiveDeals { count: 2 }),
        NoteWithdrawGate::Unreadable {
            field: "_stakes",
            reason: "absent".to_string(),
        },
    ];
    for reading in readings {
        let rendered = render_note_withdraw_gate(&reading);
        assert!(
            rendered.starts_with("PrivateNote withdrawTokens gates (what holds the money):"),
            "{rendered}"
        );
        assert_eq!(
            rendered.lines().count(),
            2,
            "one title, one answer: {rendered}"
        );
        assert!(rendered.ends_with('\n'), "{rendered}");
    }
}

/// A held note names the gate, the count, and what to do -- in the one line.
#[test]
fn a_held_note_names_the_gate_and_the_action() {
    let rendered = render_note_withdraw_gate(&NoteWithdrawGate::Held(WithdrawGate::LiveDeals {
        count: 2,
    }));
    assert!(rendered.contains("_liveDeals"), "{rendered}");
    assert!(rendered.contains('2'), "{rendered}");
    assert!(rendered.contains("note outstanding"), "{rendered}");
    assert!(rendered.contains("167"), "{rendered}");
}

/// A clear note says how many gates it checked, so "clear" is a measured claim rather than a mood.
#[test]
fn a_clear_note_says_how_many_gates_were_checked() {
    let rendered = render_note_withdraw_gate(&NoteWithdrawGate::Clear);
    assert!(
        rendered.contains(&WITHDRAW_GATE_FIELDS.len().to_string()),
        "{rendered}"
    );
    // And the operator is told what was NOT checked, on the screen and not only in the core.
    // A clear state reading followed by a gas refusal must not read as two contradictory answers.
    assert!(
        rendered.contains("NOT a statement that a withdrawal will succeed"),
        "the clear line promises an outcome it did not measure: {rendered}"
    );
    // Both halves of what was not checked reach the screen, not just the one.
    assert!(rendered.contains("gas"), "{rendered}");
    assert!(rendered.contains("amounts"), "{rendered}");
}

/// THE PROPERTY THIS FILE EXISTS FOR: an incomplete check never reads as a clean note.
#[test]
fn an_incomplete_check_says_so_and_does_not_read_as_clear() {
    let rendered = render_note_withdraw_gate(&NoteWithdrawGate::Unreadable {
        field: "_stakes",
        reason: "absent from the decoded storage".to_string(),
    });
    assert!(rendered.contains("_stakes"), "{rendered}");
    assert!(
        rendered.contains("NOT a statement that the note can withdraw"),
        "{rendered}"
    );
    // And it must not read like the clear answer. Compared against the phrase the clear reading
    // ACTUALLY emits, taken from the renderer itself: pinned against a literal that no longer
    // exists, this assertion would pass forever while checking nothing.
    let clear = render_note_withdraw_gate(&NoteWithdrawGate::Clear);
    let clear_phrase = "STATE gates read and closed";
    assert!(
        clear.contains(clear_phrase),
        "the clear reading changed: {clear}"
    );
    assert!(
        !rendered.contains(clear_phrase),
        "an unread gate rendered as a clean note: {rendered}"
    );
}
