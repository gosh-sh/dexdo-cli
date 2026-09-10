//! `dexdo note balance` reports the `_busy` latch.

//! `dex::ERR_NOTE_BUSY (121)` sends the operator here -- "check what the note is busy with
//! (`dexdo note balance` on the SENDING note reports `busyAddress`)" -- and the command printed no
//! such line, so a latched note was indistinguishable from a free one, `status: Active` included.
//! The latch does not time out: the contract clears it only on the acknowledgement of the operation
//! that set it, or when that message bounces, so the operator had nowhere to read the counterparty
//! they have to resolve.

//! The `getDetails` payloads below are the two shapes
//! `crates/core/src/chain/client.rs::note_transfer_refusals_are_read_from_get_details` already
//! pins for the refusal that raises 121 -- `busyAddress: null` and the same set address -- so the
//! rendering is asserted against the same fixture the error path is.

use super::{
    build_note_balance_view, note_busy_latch, note_getter_balance_maps, render_note_balance,
    render_note_busy_latch, NoteAccountSnapshot, NoteBusyLatch,
};
use serde_json::{json, Value};

/// The address `_busy` carries in the pinned fixture, and the canonical form the CLI renders every
/// other address in.
const BUSY_RAW: &str = "0:2222222222222222222222222222222222222222222222222222222222222222";
const BUSY_CANONICAL: &str = "0000000000000000000000000000000000000000000000000000000000000004::2222222222222222222222222222222222222222222222222222222222222222";

fn free_details() -> Value {
    json!({
        "depositIdentifierHash": "42",
        "balance": { "2": "81730000000" },
        "lockedInOrders": { "2": "0" },
        "busyAddress": null,
        "couponsValue": "0",
        "hasWithdrawn": false,
    })
}

fn account() -> NoteAccountSnapshot {
    NoteAccountSnapshot {
        address: "0:2bdbe7d0bfae641518b50d78a66d3cfc26e98af63c2667eb62f6891e45e7aed6".into(),
        status: "Active".into(),
        native_raw: 141_415_488_000,
        ecc: vec![(2, 309_000_000_000)],
        code_hash: Some("57e85fa67cc90284b907ea7e9d8c6d35830c02d14bd04d4be6ec884b5748ca0c".into()),
    }
}

/// Everything `run_note_balance` prints for one `getDetails` response, composed exactly as the
/// command composes it, so what is asserted here is what the operator reads.
fn rendered_note_balance(details: Option<&Value>) -> String {
    let view =
        build_note_balance_view("0:note", Some(account()), note_getter_balance_maps(details))
            .expect("balance view");
    let mut out = render_note_balance(&view);
    out.push_str(&render_note_busy_latch(&note_busy_latch(details)));
    out
}

/// The defect itself: a latched note must name the counterparty that has to be resolved.
#[test]
fn note_balance_reports_the_address_a_busy_note_is_latched_to() {
    let mut busy = free_details();
    busy["busyAddress"] = json!(BUSY_RAW);

    let section = render_note_busy_latch(&note_busy_latch(Some(&busy)));
    assert_eq!(
        section,
        format!(
            "PrivateNote.getDetails busyAddress (in-flight operation latch):\n  busy with \
             {BUSY_CANONICAL}\n"
        ),
        "note balance must name the counterparty the latch holds"
    );

    let out = rendered_note_balance(Some(&busy));
    assert!(
        out.ends_with(&section),
        "the latch section is missing from what the command prints: {out}"
    );
    assert!(
        !out.contains("not busy"),
        "a latched note must not also read as free: {out}"
    );
}

/// A note with no latch says so, so an absent line can never be read as "the field is unsupported".
#[test]
fn note_balance_says_a_note_with_no_latch_is_not_busy() {
    let free = free_details();
    let section = render_note_busy_latch(&note_busy_latch(Some(&free)));
    assert_eq!(
        section, "PrivateNote.getDetails busyAddress (in-flight operation latch):\n  not busy\n",
        "a free note must state that it is not busy"
    );

    let out = rendered_note_balance(Some(&free));
    assert!(
        out.ends_with(&section),
        "the latch section is missing from what the command prints: {out}"
    );
    assert!(
        !out.contains("2222222222222222222222222222222222222222222222222222222222222222"),
        "a free note must not name a counterparty: {out}"
    );
}

/// Negative: an unread latch is not evidence of a free note. `getDetails` failing, or
/// answering without the field, must render as unknown rather than as "not busy" -- reporting the
/// safe-looking answer for a state nobody read is the same defect this issue is about.
#[test]
fn note_balance_does_not_report_an_unread_latch_as_free() {
    assert_eq!(
        note_busy_latch(None),
        NoteBusyLatch::Unknown("getDetails returned no data".to_string())
    );
    assert_eq!(
        note_busy_latch(Some(&json!({ "balance": {}, "lockedInOrders": {} }))),
        NoteBusyLatch::Unknown("busyAddress field unavailable".to_string())
    );
    assert_eq!(
        note_busy_latch(Some(&json!({ "busyAddress": 7 }))),
        NoteBusyLatch::Unknown("busyAddress is not an address".to_string())
    );
    // `optional(address)` decodes to null or to an address. An empty string is neither, so it is a
    // decoding fault -- and a fault reported as "not busy" would be this issue one level down.
    assert_eq!(
        note_busy_latch(Some(&json!({ "busyAddress": "   " }))),
        NoteBusyLatch::Unknown("busyAddress decoded as an empty string".to_string())
    );

    let out = rendered_note_balance(None);
    assert!(
        out.contains(
            "PrivateNote.getDetails busyAddress (in-flight operation latch):\n  unknown (getDetails \
             returned no data)\n"
        ),
        "an unread latch must render as unknown: {out}"
    );
    assert!(!out.contains("not busy"), "{out}");
}

/// The renderer is only worth anything if the command prints it: this pins the wiring, since the
/// latch section is printed beside `render_note_balance` rather than from inside it.
#[test]
fn note_balance_command_prints_the_busy_latch_section() {
    let source = include_str!("note_cmd.rs");
    let start = source
        .find("pub(crate) async fn run_note_balance")
        .expect("run_note_balance present");
    let end = source[start..]
        .find("/// `dexdo note withdraw`")
        .map(|offset| start + offset)
        .expect("run_note_balance end marker present");
    let body = &source[start..end];
    let details = body
        .find(".private_note_details(")
        .expect("getDetails read present");
    let latch = body
        .find("note_busy_latch(details.as_ref())")
        .expect("busy latch read from the same getDetails response");
    let render = body
        .find("render_note_busy_latch(&busy)")
        .expect("busy latch section printed");
    assert!(
        details < latch && latch < render,
        "the latch must be read from the single getDetails response and then printed: {body}"
    );
    assert_eq!(
        body.matches(".private_note_details(").count(),
        1,
        "reporting the latch must not add a second chain read: {body}"
    );
}
