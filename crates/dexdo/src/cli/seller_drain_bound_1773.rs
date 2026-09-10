//! point 2: the shutdown drain is bounded, and says what it could not finish.

//! Point 1 gave every watcher a stop signal it can observe. That makes the operator's signal REACH
//! the pool; it does not make any watcher finish. The loop was measured continuing for 591 further
//! lines of chain traffic after the shutdown arm fired, with no terminal and no `stopping` event --
//! the signal delivered and the process still never exiting.

//! The bound is `MATCH_OPEN_TIMEOUT`, taken from the domain rather than chosen as a smaller number:
//! it is the contract's own funded-but-unopened window, after which a deal that never opened is
//! cleanable by the chain, so no watcher can still be doing legitimate work on one.

//! The bound is asserted by driving `drain_watchers_within` -- the production function the shutdown
//! path calls -- with a stream and a bound. An earlier pair of tests here wrapped `pending()` in a
//! `tokio::time::timeout` of their own and asserted on THAT; both stayed green with the bound
//! removed from the shutdown path entirely, which is the only failure they were there to catch.

use super::*;

fn deals(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

/// A silent exit on a timer is worse than the hang it replaces: the hang is at least visible. The
/// refusal must name the deals whose watcher never reported back.
#[test]
fn the_refusal_names_every_watcher_that_did_not_report_back() {
    let outstanding = deals(&["0:aaa", "0:bbb"]);
    let refusal = undrained_watchers_refusal(&outstanding).to_string();

    assert!(refusal.contains("0:aaa"), "{refusal}");
    assert!(refusal.contains("0:bbb"), "{refusal}");
    assert!(
        refusal.contains("2 watcher(s)"),
        "the count is stated: {refusal}"
    );
}

/// The bound is named in the text, and by its own name rather than by a bare number, so an operator
/// reading it can find the constant that produced it.
#[test]
fn the_refusal_names_the_bound_and_its_value() {
    let refusal = undrained_watchers_refusal(&deals(&["0:aaa"])).to_string();

    assert!(refusal.contains("MATCH_OPEN_TIMEOUT"), "{refusal}");
    assert!(
        refusal.contains(&dexdo_core::params::MATCH_OPEN_TIMEOUT.as_secs().to_string()),
        "the value must come from the constant, not a literal: {refusal}"
    );
}

/// must not be traded away for the bound. The refusal has to say that the offers were left
/// UNSWEPT, because sweeping them would prove absence against a generation that may already be
/// consumed -- the exact defect closed.
#[test]
fn the_refusal_says_the_offers_were_left_unswept_rather_than_swept_on_a_stale_identity() {
    let refusal = undrained_watchers_refusal(&deals(&["0:aaa"])).to_string();

    assert!(refusal.contains("unswept"), "{refusal}");
    assert!(
        refusal.contains("never reported back"),
        "it must say WHY they were left: {refusal}"
    );
}

/// An empty list is still a refusal and still readable: it must not render as a dangling colon with
/// nothing after it.
#[test]
fn an_empty_outstanding_list_still_reads() {
    let refusal = undrained_watchers_refusal(&[]).to_string();

    assert!(refusal.contains("<none recorded>"), "{refusal}");
    assert!(!refusal.ends_with(": "), "{refusal}");
}

/// The bound itself, asserted on OUR drain.

/// Two watchers are expected back; one reports and one never does. `drain_watchers_within` must END
/// -- and fixes how it ends: the watcher that never reported was advancing an identity nobody
/// else has seen, so it is NAMED and handed back for the caller to keep out of the sweep, while the
/// one that did report is drained normally and is not named.

/// The outer guard is twice the bound under test and is what makes this test honest. With the
/// production bound taken out, the drain runs forever; the guard fires instead of the bound and the
/// `expect` below is red, rather than the test hanging on a wall clock. On a paused clock both
/// deadlines cost no real time, and the bound comes from the same constant production uses.
#[tokio::test(start_paused = true)]
async fn a_watcher_that_never_reports_back_ends_the_drain_and_is_named() {
    let bound = dexdo_core::params::MATCH_OPEN_TIMEOUT;
    let expected: std::collections::BTreeSet<String> =
        deals(&["0:aaa", "0:bbb"]).into_iter().collect();
    let mut reported: Vec<String> = Vec::new();
    let mut watched =
        futures::stream::iter(deals(&["0:aaa"])).chain(futures::stream::pending::<String>());

    let ended = tokio::time::timeout(
        bound * 2,
        drain_watchers_within(
            &mut watched,
            bound,
            &expected,
            |name: &String| name.clone(),
            async |name| reported.push(name),
        ),
    )
    .await
    .expect("the drain must be ended by its own bound, not by this test's guard");

    let (outstanding, refusal) = ended.expect("a drain that could not finish must refuse");
    assert_eq!(
        outstanding,
        deals(&["0:bbb"]),
        "only the watcher that never reported back is withheld from the sweep"
    );
    assert_eq!(
        reported,
        deals(&["0:aaa"]),
        "the watcher that did report back was drained, not dropped"
    );
    let refusal = refusal.to_string();
    assert!(refusal.contains("0:bbb"), "{refusal}");
    assert!(
        !refusal.contains("0:aaa"),
        "a watcher that reported back must not be named as outstanding: {refusal}"
    );
}

/// And the bound the caller GIVES is the bound the drain honours: work that fits inside it runs to
/// its end, every watcher applied, nothing named.

/// This is the direction a bound can fail in silently. A drain that ends early is a truncation, not
/// a bound: the pool would report deals as never having reported back while their watchers were
/// still working inside the window the chain allows them.
#[tokio::test(start_paused = true)]
async fn work_that_finishes_inside_the_bound_is_drained_whole() {
    let bound = dexdo_core::params::MATCH_OPEN_TIMEOUT;
    let expected: std::collections::BTreeSet<String> =
        deals(&["0:aaa", "0:bbb"]).into_iter().collect();
    let mut reported: Vec<String> = Vec::new();
    let mut watched = futures::stream::iter(deals(&["0:aaa", "0:bbb"]));

    // Each watcher spends a real quarter of the bound: together half of it, inside but not instant.
    let ended = drain_watchers_within(
        &mut watched,
        bound,
        &expected,
        |name: &String| name.clone(),
        async |name| {
            reported.push(name);
            tokio::time::sleep(bound / 4).await;
        },
    )
    .await;

    if let Some((outstanding, refusal)) = ended {
        panic!("the drain was cut short inside its own bound: {outstanding:?}; {refusal}");
    }
    assert_eq!(
        reported,
        deals(&["0:aaa", "0:bbb"]),
        "every watcher that reported inside the bound was applied"
    );
}
