//! the other half of the sweep's rule -- an empty answer that hides nothing stays an answer.

//! `list_deal_handles` refuses when every candidate was skipped, so a caller is never told "no
//! deals" about a directory holding one it could not read. The complement has to hold too: a
//! directory that genuinely has nothing for us returns an empty list, NOT a refusal. `dexdo seller`,
//! `dexdo status`, the dashboard and the reports all sweep this way on a machine that has never made
//! a deal, and a refusal there would break every one of them on first run.

//! Nothing asserted this before. Measured while adding it: removing the "and something was skipped"
//! condition -- so an empty result always refuses -- reddened NOTHING in
//! `cargo test -p dexdo --bin dexdo` (it needed a cargo feature once; there are none now). The production paths above would all have
//! started failing on an empty deals directory with no test to say so.

use super::*;

/// An existing directory with no handle files at all.
#[test]
fn an_empty_deals_directory_is_an_empty_list_and_not_a_refusal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let listed =
        list_deal_handles(temp.path()).expect("a directory that holds no handles is not a refusal");
    assert!(listed.is_empty(), "{listed:?}");
}

/// A directory holding files that are not deal handles at all. They are filtered out before the
/// sweep ever tries to read them, so nothing is skipped and nothing is hidden.
#[test]
fn a_directory_of_unrelated_files_is_an_empty_list_and_not_a_refusal() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("notes.txt"), b"not a deal handle").expect("write");
    crate::cli::support::write_owner_only_key_fixture(&temp.path().join("pn_pool.json"), "{}");

    let listed = list_deal_handles(temp.path())
        .expect("files that are not deal handles are not skipped handles");
    assert!(listed.is_empty(), "{listed:?}");
}

/// The distinction the rule turns on, stated as one pair: same empty RESULT, opposite verdicts.
/// Nothing to read -> `Ok([])`. Something unreadable and nothing left -> `Err`.
#[test]
fn an_empty_result_refuses_only_when_something_was_skipped() {
    let nothing = tempfile::tempdir().expect("tempdir");
    assert!(
        list_deal_handles(nothing.path()).is_ok(),
        "an empty directory must not refuse"
    );

    let skipped = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        skipped.path().join("deal-0-broken-seller.json"),
        b"{ not json",
    )
    .expect("write the only handle, unreadable");
    let error = list_deal_handles(skipped.path())
        .expect_err("the only handle was unreadable, so there is no empty list to return");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("parse deal handle"),
        "the refusal carries the reason the sweep came back empty: {rendered}"
    );
    assert!(
        rendered.contains("deal-0-broken-seller.json"),
        "the refusal names the file: {rendered}"
    );
}
