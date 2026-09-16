//! part two: what `dexdo note sweep` refuses, and what it reports when it does not.

//! The command moves the note's PHYSICAL ECC[2] pocket after `withdrawTokens` has run. There is no
//! live run behind this file -- the live gate is blocked on CI -- so what is asserted here is
//! everything that can be decided without a chain: the refusals, their wording, the order they
//! happen in, and the shape of the result line.

//! # Why the result line is tested at all

//! Because "submitted" is not "arrived". The sibling `note withdraw` prints
//! `withdrawTokens submitted`, which is true of the message and says nothing about the money. A
//! sweep that was accepted and moved nothing looks identical in that line, and this is a recovery
//! path -- the operator reaching for it has already been told once that their money was gone.

use super::render_note_sweep;
use serde_json::json;

/// The confirmed case names both readings and the amount, so the operator can check the arithmetic
/// rather than take the word "CONFIRMED" for it.
#[test]
fn a_confirmed_sweep_reports_both_readings_and_the_difference() {
    let line = render_note_sweep(
        "0000...0004::abcd",
        "0000...0004::wxyz",
        &json!({
            "pocket_before": "3000000000",
            "pocket_after": "0",
            "swept": "3000000000",
            "confirmed": true,
        }),
    );
    assert!(line.contains("SWEEP CONFIRMED"), "{line}");
    assert!(line.contains("3000000000"), "{line}");
    assert!(line.contains("pocket before"), "{line}");
    assert!(line.contains("pocket after"), "{line}");
    assert!(line.contains("0000...0004::wxyz"), "{line}");
}

/// The unconfirmed case must NOT read as a failure, and must not invite a second send.

/// This is the direction that costs money. A sweep whose confirmation window expired may still
/// land; an operator who reads "failed" sends again, and the second send moves a pocket the first
/// one already emptied -- or worse, is aimed at a wallet on the strength of a line that was wrong.
#[test]
fn an_unconfirmed_sweep_says_so_without_calling_it_a_failure() {
    let line = render_note_sweep(
        "0000...0004::abcd",
        "0000...0004::wxyz",
        &json!({
            "pocket_before": "3000000000",
            "pocket_after": "3000000000",
            "swept": "0",
            "confirmed": false,
        }),
    );
    assert!(line.contains("SWEEP UNVERIFIED"), "{line}");
    assert!(
        line.contains("may yet land"),
        "an unconfirmed sweep that reads as failed invites a second send: {line}"
    );
    assert!(
        !line.contains("SWEEP CONFIRMED"),
        "the confirmed verdict must not appear on an unconfirmed sweep: {line}"
    );
}

/// The money-kind warning is not optional, and it is the one thing a reader cannot infer.

/// `sweepShell` sends under `flag: 1`, so the SHELL lands at the destination as ECC[2] -- the
/// traded asset, in `balance_other`. It does NOT become spendable native gas there. An operator
/// sweeping a note to rescue a gas-starved wallet gets their money back and a wallet that still
/// cannot send anything, and nothing else in the output would tell them.
#[test]
fn the_result_says_the_shell_arrives_as_ecc_and_not_as_gas() {
    for confirmed in [true, false] {
        let line = render_note_sweep(
            "0000...0004::abcd",
            "0000...0004::wxyz",
            &json!({
                "pocket_before": "1",
                "pocket_after": "0",
                "swept": "1",
                "confirmed": confirmed,
            }),
        );
        assert!(
            line.contains("not as spendable"),
            "confirmed={confirmed}: {line}"
        );
    }
}

/// A result with fields missing renders rather than panicking, and says which figure it lacks.

/// Runtime paths do not panic (owner's rule), and this one runs after money has already moved --
/// the worst possible moment to lose the output. `?` is deliberately visible instead of a plausible
/// zero, because a zero here would read as "nothing was swept".
#[test]
fn a_result_missing_its_figures_still_renders_and_shows_the_gap() {
    let line = render_note_sweep("note", "dest", &json!({}));
    assert!(line.contains('?'), "{line}");
    assert!(line.contains("note sweep"), "{line}");
    assert!(line.contains("SWEEP UNVERIFIED"), "{line}");
}

/// The two verdicts must not nest, or a grep for the good one matches the bad one.

/// This is not hypothetical and not caught by reading: the first wording was `CONFIRMED` against
/// `NOT CONFIRMED`, and the second CONTAINS the first. Any operator or script matching on
/// `CONFIRMED` would have read an unverified sweep as a settled one -- on a money command, in the
/// direction that says the money is safe. The regression above found it; this keeps it found.
#[test]
fn the_two_verdicts_cannot_be_confused_by_a_substring_match() {
    let result = |confirmed: bool| {
        json!({
            "pocket_before": "1",
            "pocket_after": "0",
            "swept": "1",
            "confirmed": confirmed,
        })
    };
    let good = render_note_sweep("n", "d", &result(true));
    let bad = render_note_sweep("n", "d", &result(false));

    let verdict_of = |line: &str| -> String {
        line.lines()
            .find(|l| l.contains("SWEEP "))
            .expect("a verdict line")
            .trim()
            .to_string()
    };
    let (good_verdict, bad_verdict) = (verdict_of(&good), verdict_of(&bad));
    let good_token = good_verdict.split(':').next().expect("token").trim();
    let bad_token = bad_verdict.split(':').next().expect("token").trim();

    assert!(
        !bad_token.contains(good_token),
        "the unconfirmed verdict `{bad_token}` contains the confirmed one `{good_token}`, so a \
         substring match on the confirmed verdict is true of both"
    );
    assert!(
        !good_token.contains(bad_token),
        "`{good_token}` contains `{bad_token}` -- the confusion runs the other way"
    );
}

/// The refusals are decided before the owner secret is read, and before anything is submitted.

/// Same ordering `note withdraw` holds and asserted the same way -- against the source, because the
/// property is about the sequence in the body and there is no chain here to run it against. An
/// argument the command refuses must be refused before a secret is looked for, let alone read.
#[test]
fn the_destination_is_refused_before_the_secret_is_read_or_anything_is_sent() {
    let source = include_str!("note_cmd.rs");
    // The body is brace-matched rather than cut at the next `#[cfg(...)]`: this tree has no cargo
    // features left, so the `#[cfg(not(feature = "net-a"))]` twin that used to mark the
    // end is gone, and a probe anchored to a marker that no longer exists cannot answer about the
    // ordering it is for.
    let body = crate::cli::source_probe::code_of(source, "pub(crate) async fn run_note_sweep");

    let destination = body
        .find("parse_note_withdraw_destination")
        .expect("canonical destination guard present");
    let secret = body
        .find("note_owner_secret_for")
        .expect("owner secret read present");
    let owner = body
        .find("assert_note_owner_matches")
        .expect("owner preflight present");
    let submit = body.find("sweep_note_shell").expect("sweep submit present");

    assert!(
        destination < secret,
        "input refusal must precede the secret"
    );
    assert!(
        destination < owner,
        "input refusal must precede owner reads"
    );
    assert!(
        destination < submit,
        "input refusal must precede the submit"
    );
    assert!(owner < submit, "owner must be checked before money moves");
}

/// `--to` carries the DApp id, so both irreversible exits require it and have no default.

/// Measured rather than assumed: the contract takes `dapp_id` as its own second parameter and sends
/// to it as `dest_dapp_id`, so the destination's DApp half is load-bearing and not decoration. A
/// default here would be a guess at where someone's money goes.
#[test]
fn irreversible_destination_arguments_have_no_default() {
    let source = include_str!("args.rs");
    for command in ["NoteWithdrawArgs", "NoteSweepArgs"] {
        let marker = format!("pub(crate) struct {command}");
        let start = source
            .find(&marker)
            .unwrap_or_else(|| panic!("{command} present"));
        let end = source[start..]
            .find("\n}")
            .map(|o| start + o)
            .expect("struct closes");
        let body = &source[start..end];
        let to = body.find("pub(crate) to: String").expect("--to present");
        let before_to = &body[..to];
        assert!(
            !before_to
                .rsplit("#[arg(")
                .next()
                .is_some_and(|attr| attr.contains("default_value")),
            "{command} --to must not carry a default: an irreversible transfer never guesses its destination"
        );
        assert!(
            body.contains("REQUIRED") && body.contains("never defaulted"),
            "{command} help must state that the destination is always explicit"
        );
    }
}

/// `note sweep` encodes its call with `note withdraw`'s payload builder, and that is only sound
/// while the two ABI shapes agree. Pinned against the COMPILED ABI, not the Solidity prose.

/// They agree today -- `sweepShell(address destWalletAddr, uint256 dapp_id)` is
/// `withdrawTokens`'s signature exactly -- which is why one builder serves both. If a generation
/// renames either parameter, the shared builder would keep encoding the old name and the call would
/// fail on chain, or worse, encode a field the contract reads differently. This turns that into a
/// red test instead of a failed transfer.
#[test]
fn sweep_and_withdraw_take_the_same_two_parameters_in_the_compiled_abi() {
    const PRIVATE_NOTE_ABI: &str =
        include_str!("../../../../contracts/compiled/dex/PrivateNote.abi.json");
    let abi: serde_json::Value =
        serde_json::from_str(PRIVATE_NOTE_ABI).expect("the compiled PrivateNote ABI parses");
    let functions = abi["functions"].as_array().expect("ABI lists functions");
    let shape = |name: &str| -> Vec<(String, String)> {
        functions
            .iter()
            .find(|f| f["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("the compiled ABI still carries `{name}`"))["inputs"]
            .as_array()
            .unwrap_or_else(|| panic!("`{name}` has inputs"))
            .iter()
            .map(|i| {
                (
                    i["name"].as_str().unwrap_or_default().to_string(),
                    i["type"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    let sweep = shape("sweepShell");
    assert_eq!(
        sweep,
        shape("withdrawTokens"),
        "`note sweep` reuses `withdraw_note_tokens_payload_for_destination`; the two signatures \
         have diverged, so that reuse now encodes the wrong call"
    );
    // And named explicitly, so a rename that happened to move BOTH still goes red here.
    assert_eq!(
        sweep,
        vec![
            ("destWalletAddr".to_string(), "address".to_string()),
            ("dapp_id".to_string(), "uint256".to_string()),
        ],
        "the payload builder writes these two keys by name"
    );
}

/// The result's indentation is CONTENT, and it was wrong once for a reason worth pinning.

/// The first version built the block as one long `\`-continued literal. rustfmt is entitled to
/// rejoin those, and when it did it baked the source's own indentation into the string -- eleven
/// spaces per line in what an operator reads. Nothing else caught it: every assertion here matched
/// on words, and whitespace is exactly where a wrapper is allowed to intervene.
#[test]
fn every_detail_line_is_indented_by_two_spaces_and_not_by_the_source_layout() {
    let line = render_note_sweep(
        "n",
        "d",
        &json!({
            "pocket_before": "1",
            "pocket_after": "0",
            "swept": "1",
            "confirmed": true,
        }),
    );
    let mut lines = line.lines();
    let head = lines.next().expect("a heading line");
    assert!(
        !head.starts_with(' '),
        "the heading is not indented: {head:?}"
    );
    for detail in lines.filter(|l| !l.trim().is_empty()) {
        let indent = detail.len() - detail.trim_start().len();
        assert!(
            (2..=9).contains(&indent),
            "detail line carries {indent} spaces of indent, which is source layout rather than \
             formatting: {detail:?}"
        );
    }
}
