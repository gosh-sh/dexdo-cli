//! what `market` and `quote` say when the book a name derives is not on chain.

//! `run_executable_book` always answered this -- an empty result there carries an `empty_reason` and
//! renders `none=true <class>=true reason=...`. `run_market` and `run_quote` had no such mechanism:
//! they read `active` only to feed the policy-gated enforcement, and on an undeployed book printed
//! the context line and an empty table. An empty table asserts that the market is EMPTY, which is a
//! different fact from the market not existing, and only one of the two tells the operator their
//! name was wrong.

//! It lands with this change rather than after it because this change WIDENS the silent path: until
//! now an unknown name was stopped earlier by the mandatory catalog (`model "x" not found in the
//! config`), so the silence was reachable only for a catalogued name whose book was undeployed.
//! With the catalog no longer required, any unresolvable name reaches it.

use super::render_book_not_deployed;

const MODEL: &str = "Qwen3.6-27B";
const BOOK: &str = "0:05cfd1a8e9337141592373a80f0dec2021bffa5781fd439739df72fd9e9ef8a8";

/// The three things the line has to carry, asserted separately: a machine-readable fact, the
/// subject, and a next move. A single `contains` over the whole sentence would pass on any one of
/// them.
#[test]
fn the_line_states_the_fact_the_model_and_the_way_forward() {
    let line = render_book_not_deployed(MODEL, BOOK);
    assert!(
        line.contains("book_not_deployed=true"),
        "a reader parsing this output needs the fact as a token, not as prose: {line}"
    );
    assert!(
        line.contains(MODEL),
        "the line must name the model it is about: {line}"
    );
    assert!(
        line.contains("markets address"),
        "the operator's next move is the command that resolves a name without any local file: \
         {line}"
    );
}

/// It says which book it looked at. Without the address the operator cannot tell a wrong NAME from
/// a right name whose market simply has not been provisioned yet.
#[test]
fn the_line_names_the_book_it_looked_at() {
    let line = render_book_not_deployed(MODEL, BOOK);
    assert!(
        line.contains("05cfd1a8"),
        "the derived order book has to appear so the claim can be checked: {line}"
    );
}

/// It distinguishes "no market" from "an empty market" IN WORDS, because that distinction is the
/// entire reason the line exists.
#[test]
fn the_line_separates_no_market_from_an_empty_one() {
    let line = render_book_not_deployed(MODEL, BOOK);
    assert!(
        line.contains("rather than an empty one"),
        "an empty table already says 'empty'; this line exists to say the other thing: {line}"
    );
}

/// CONTROL: the renderer is not a constant. A test suite that only ever feeds it one model would
/// pass against a hard-coded sentence, which is the failure mode these three assertions invite.
#[test]
fn the_line_is_built_from_its_arguments() {
    let other = render_book_not_deployed(
        "gpt-oss-20b",
        "0:1111111111111111111111111111111111111111111111111111111111111111",
    );
    assert!(
        other.contains("gpt-oss-20b") && other.contains("1111111111"),
        "{other}"
    );
    assert!(
        !other.contains(MODEL) && !other.contains("05cfd1a8"),
        "the previous subject leaked into this one: {other}"
    );
}
