//! a refusal that says the chain could not be reached has to say WHERE.

//! `for_operator` is the floor under every unrecognised error, and its transport branch rewrote any
//! connection failure into two lines that had dropped the address:

//! ```text
//! The client could not reach the chain, and nothing was sent.
//! Check the network, then run the same command again. `dexdo doctor` says what it can reach and
//! what it cannot.
//! ```

//! An operator carries three candidate addresses into every command -- `--endpoint`, the endpoint
//! the manifest names, and the network default that fills in when neither was given -- and this
//! sentence tells them which one was dialled: none. That is the single fact the run was there to
//! establish, and it was the one thing the refusal removed. Everything else on the screen was
//! already known before the command was typed.

//! The address was never missing from the error, only from the sentence: the SDK's liveness probe
//! writes `POST <url>` into its own context and `reqwest` repeats the url again underneath, so
//! `{error:#}` carries it to the exact line that threw it away.

//! The end-to-end proof through the shipped binary is
//! `tests/contracts_manifest_default_334.rs`.

use super::{for_operator, Kind};

/// What `dexdo doctor` leaves in `{error:#}` when the manifest names a dead endpoint, copied from
/// a run.

/// The run that produced it passed `--endpoint http://127.0.0.1:1`. That flag is gone and
/// the dead port now comes from the manifest, which changes how the run is set up and not one
/// character of the chain below -- it is the SDK's own wording for a refused connection.

/// Not composed here: this is the flattened chain the shipped binary printed at `RUST_LOG=info`
/// against a dead port, retry wrapper and `reqwest` wording included, so what the assertions below
/// read is the text the site really receives rather than a tidied version of it.
const DOCTOR_AGAINST_A_DEAD_PORT: &str =
    "chain read got no answer after 5 attempt(s) in 7.507548544s: POST \
     http://127.0.0.1:1/graphql: error sending request for url (http://127.0.0.1:1/graphql): \
     client error (Connect): tcp connect error: Connection refused (os error 111)";

/// The other site that dials on the operator's behalf, worded by us rather than by the SDK.

/// Its address is bare of any path and sits in front of a colon, which is a different shape from
/// the one above -- pinning both is what keeps the fix from being a match on one wording.
const BALANCE_READ_AGAINST_A_REFUSED_HOST: &str =
    "connect read-only balance endpoint https://net-a.example: transport refused";

#[test]
fn an_unreachable_chain_is_named_by_the_address_the_run_dialled() {
    for raw in [
        DOCTOR_AGAINST_A_DEAD_PORT,
        BALANCE_READ_AGAINST_A_REFUSED_HOST,
    ] {
        let refusal = for_operator(&anyhow::anyhow!("{raw}"))
            .unwrap_or_else(|| panic!("a transport failure has to be recognised: {raw}"));
        let shown = refusal.render();
        // The address as the operator would recognise it, host and port together: `--endpoint
        // http://127.0.0.1:1` and a manifest naming the same host on another port are the two
        // candidates this line exists to tell apart.
        let dialled = if raw == DOCTOR_AGAINST_A_DEAD_PORT {
            "http://127.0.0.1:1/graphql"
        } else {
            "https://net-a.example"
        };
        assert!(
            shown.contains(dialled),
            "a refusal for an unreachable chain must name the address it could not reach: {shown}"
        );
        // Still the shape 679 asks for: ONE statement of what did not happen, then ONE instruction,
        // AND NOTHING ELSE.

        // Counted by CONTENT, not by indentation. A long instruction is wrapped at the window and
        // every row after the first continues under the value column -- but so would a third
        // sentence, because it goes through the same `field_wrapped`. Filtering indented rows away
        // therefore left exactly one row no matter how much a refusal grew, and the boundary this
        // assertion is named for was gone. Rebuilding the indented rows into one string and
        // comparing it to the refusal's own instruction restores it: anything printed beyond the
        // news and that instruction fails here.
        let mut rows = shown.lines();
        let news = rows.next().unwrap_or_default();
        let printed_instruction = rows
            .map(|row| row.trim())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        assert!(
            !news.starts_with(' '),
            "the news is not the first row: {shown}"
        );
        assert!(news.contains("could not reach the chain"), "{shown}");
        assert_eq!(
            printed_instruction,
            refusal.do_next(),
            "one statement of news and one instruction, and nothing else: {shown}"
        );
        assert!(
            refusal.do_next().contains("dexdo doctor"),
            "the second line stays an action: {}",
            refusal.do_next()
        );
        assert_eq!(refusal.kind(), Kind::Breakage, "{raw}");
        // And naming the address is not an invitation to bring the rest of the chain with it.
        assert!(
            !shown.contains("os error 111") && !shown.contains("attempt(s)"),
            "the record stays a record: {shown}"
        );
        // The record still holds everything, unchanged.
        assert_eq!(refusal.detail(), raw, "{shown}");
    }
}

/// A transport failure whose own text names no address keeps the sentence it always had.

/// The fix lifts an address out of the error; it does not manufacture one. A refusal that guessed
/// an endpoint here would be worse than one that names none, because the operator cannot tell a
/// guess from a fact.
#[test]
fn a_transport_failure_that_names_no_address_does_not_acquire_one() {
    let refusal = for_operator(&anyhow::anyhow!("connection reset by peer"))
        .expect("a transport failure has to be recognised");
    let shown = refusal.render();
    assert!(
        shown.contains("The client could not reach the chain, and nothing was sent."),
        "{shown}"
    );
    assert!(
        !shown.contains("://"),
        "nothing was invented to fill the gap: {shown}"
    );
    // One statement of news, one instruction under it, and nothing else -- wrapped rows are still
    // that instruction, and a row that starts its own sentence is not.
    let unindented: Vec<&str> = shown
        .lines()
        .filter(|row| !row.starts_with("            "))
        .collect();
    assert_eq!(unindented.len(), 1, "{shown}");
    assert!(
        unindented[0].contains("could not reach the chain"),
        "{shown}"
    );
}
