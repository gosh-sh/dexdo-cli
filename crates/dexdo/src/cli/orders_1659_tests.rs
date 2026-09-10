//! site 1 -- `orders` must not refuse on an empty fold without asking the book.

//! This is the site was measured on: 73 notes whose orders the book still held, and whose
//! owner could neither `cancel-all` nor `expire` because the client's own view had no rows for them.
//! Both commands refuse from the same list, so one silent read disabled both exits at once.

//! Driven through `read_live_order_snapshot_with`, which is the production rule with its two chain
//! reads as seams. Asserting the rule anywhere else would be an assertion about the rule rather than
//! about the path that applies it.

use super::{read_live_order_snapshot_with, OrdersView};
use dexdo_core::{OrderBookOrder, OrderBookSnapshot};

const BOOK: &str = "0:917d85f33c24d3ed930355bf98a488456256d27de3c022d2c785315fcfcdb80f";
const OWNER: &str = "0:3bc65e6ab529b648a74ab3da1707edc450e0c91dcc12ffea53f0850117572aa1";

/// The row the book actually holds, from: linked in the owner list, counted, deadline long
/// past, and invisible to the fold.
fn stored_row() -> OrderBookOrder {
    OrderBookOrder {
        order_id: 3,
        owner_note: OWNER.to_string(),
        token_contract: Some(BOOK.to_string()),
        is_buy: false,
        price_per_tick: 1_000_000_000,
        ticks: 2,
        escrow: 0,
        deadline: 1_786_185_607,
        flags: 0,
        timestamp: 0,
    }
}

fn snapshot_with(orders: Vec<OrderBookOrder>) -> OrderBookSnapshot {
    OrderBookSnapshot {
        frame_model: "qwen--qwen3--32b".to_string(),
        model_hash: "0x53c05e91aeb663699a720e7a7e211f2f9eb2aa4b8c68a7f87c80cc56b716d8a8"
            .to_string(),
        order_book: BOOK.to_string(),
        stats: None,
        orders,
    }
}

fn view_with(orders: Vec<OrderBookOrder>) -> OrdersView {
    OrdersView {
        snapshot: snapshot_with(orders),
        rows: crate::cli::provenance::ROWS_CHAIN_EVENTS,
        last_update_id: "fold-1".to_string(),
        swept_order_ids: std::collections::BTreeSet::new(),
    }
}

/// THE SHAPE. The fold succeeds and carries nothing; the book still holds the order. The rows
/// acted on must be the book's, and they must say so.

/// Red before this change: the empty fold was returned as the answer, `own_orders` found nothing,
/// and both `cancel-all` and `expire` refused an order the chain would have cancelled.
#[tokio::test]
async fn an_empty_fold_is_replaced_by_the_rows_the_book_still_holds() {
    let view = read_live_order_snapshot_with(
        || async { Ok(view_with(Vec::new())) },
        || async { Ok(snapshot_with(vec![stored_row()])) },
    )
    .await
    .expect("the rule reads storage rather than refusing");

    assert_eq!(
        view.snapshot.orders.len(),
        1,
        "an order the book holds must survive an empty fold"
    );
    assert_eq!(view.snapshot.orders[0].order_id, 3);
    assert_eq!(
        view.rows,
        crate::cli::provenance::ROWS_CHAIN_GETTERS,
        "rows taken from storage must be attributed to storage"
    );
}

/// The rule is not a second opinion on a positive answer: a fold that saw rows keeps them, keeps its
/// provenance, and costs no extra chain read.
#[tokio::test]
async fn a_fold_that_saw_rows_is_not_second_guessed() {
    let storage_reads = std::cell::Cell::new(0u32);
    let view = read_live_order_snapshot_with(
        || async { Ok(view_with(vec![stored_row()])) },
        || {
            storage_reads.set(storage_reads.get() + 1);
            async { Ok(snapshot_with(Vec::new())) }
        },
    )
    .await
    .expect("a non-empty fold answers on its own");

    assert_eq!(view.snapshot.orders.len(), 1);
    assert_eq!(view.rows, crate::cli::provenance::ROWS_CHAIN_EVENTS);
    assert_eq!(
        storage_reads.get(),
        0,
        "storage must not be read when the fold already answered"
    );
}

/// An emptiness that storage AGREES with is a measured emptiness, and the command may act on it.
/// Without this the rule would only ever be able to say "keep looking".
#[tokio::test]
async fn an_emptiness_storage_confirms_is_reported_as_empty() {
    let view = read_live_order_snapshot_with(
        || async { Ok(view_with(Vec::new())) },
        || async { Ok(snapshot_with(Vec::new())) },
    )
    .await
    .expect("a confirmed emptiness is still an answer");

    assert!(view.snapshot.orders.is_empty());
    assert_eq!(view.rows, crate::cli::provenance::ROWS_CHAIN_GETTERS);
}

// -------------------------------------------------------------------------
// site 3 -- the same rule pointed the other way.
// -------------------------------------------------------------------------

// Sites 1 and 2 refuse on an unbelieved silence; this one CONFIRMS on it, and that is the direction
// that hands an operator a figure instead of withholding one. The former row-only predicate
// answered `true` when the fold simply did not carry the row, so a bounded history became a
// reported removal and a reported refund. `reconcile_order_removal_with` now requires direct
// storage confirmation for that silent-fold branch.

use super::reconcile_order_removal_with;
use dexdo_core::chain::LiveBookOrder;

fn folded(order_id: u128, expired_by_event: bool) -> LiveBookOrder {
    LiveBookOrder {
        order_id,
        is_buy: false,
        price: 1_000_000_000,
        ticks_remaining: 2,
        note: OWNER.to_string(),
        token_contract: BOOK.to_string(),
        deadline: 1_786_185_607,
        flags: 0,
        expired_by_event,
    }
}

/// THE MONEY CASE. The fold does not carry the row and the book still holds it. Reporting a removal
/// here reports a refund that never happened.

/// Red before this change: absence from the fold was read as removal, so this returned
/// `Some((delta, balance))` -- a figure derived from a removal the chain never performed.
#[tokio::test(start_paused = true)]
async fn a_row_the_book_still_holds_is_not_reported_as_removed() {
    let storage_reads = std::cell::Cell::new(0u32);
    let outcome = reconcile_order_removal_with(
        || async { Ok(Vec::new()) },
        || {
            storage_reads.set(storage_reads.get() + 1);
            async { Ok(vec![stored_row()]) }
        },
        || async { Ok(9_999u128) },
        3,
        1_000,
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("the read itself succeeds");

    assert!(
        outcome.is_none(),
        "the book still holds order 3, so no removal and no refund figure may be claimed"
    );
    assert!(
        storage_reads.get() >= 1,
        "the silence must have been put to storage at least once"
    );
}

/// An absence storage AGREES with is a real removal, and the observed balance delta is reported.
/// Without this the rule could only ever withhold, and `cancel`/`expire` would never confirm.
#[tokio::test(start_paused = true)]
async fn an_absence_storage_confirms_is_a_real_removal() {
    let outcome = reconcile_order_removal_with(
        || async { Ok(Vec::new()) },
        || async { Ok(Vec::new()) },
        || async { Ok(4_200u128) },
        3,
        1_000,
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("the read itself succeeds");

    assert_eq!(
        outcome,
        Some((3_200, 4_200)),
        "a confirmed removal reports the observed delta, not a derived one"
    );
}

/// The fold SEEING the row and saying it is swept is a statement, not silence -- it settles the
/// question on its own and costs no storage read. The rule refuses to believe silence, not
/// everything.
#[tokio::test(start_paused = true)]
async fn a_sweep_the_fold_itself_announced_needs_no_second_source() {
    let storage_reads = std::cell::Cell::new(0u32);
    let outcome = reconcile_order_removal_with(
        || async { Ok(vec![folded(3, true)]) },
        || {
            storage_reads.set(storage_reads.get() + 1);
            async { Ok(Vec::new()) }
        },
        || async { Ok(4_200u128) },
        3,
        1_000,
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("the read itself succeeds");

    assert_eq!(outcome, Some((3_200, 4_200)));
    assert_eq!(
        storage_reads.get(),
        0,
        "the fold answered positively, so storage must not be consulted"
    );
}

/// A row the fold still carries as live is not removed, and no storage read is needed to say so.
#[tokio::test(start_paused = true)]
async fn a_row_the_fold_still_carries_is_not_removed() {
    let outcome = reconcile_order_removal_with(
        || async { Ok(vec![folded(3, false)]) },
        || async { Ok(Vec::new()) },
        || async { Ok(4_200u128) },
        3,
        1_000,
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("the read itself succeeds");

    assert!(outcome.is_none());
}
