//! site 2 -- the executable market view must not call a book empty on an empty fold.

//! Same rule as `orders`, same function, different consequence: here an unbelieved silence did not
//! merely refuse a command, it also asserted `active: true` about a book the client had just failed
//! to see. A view that says "this market is live and rests nothing" is a statement an operator
//! prices against.

//! Driven through `read_executable_market_view_with`, which is the production path with its three
//! reads as seams.

use super::{read_executable_market_view_with, IndexerMarketContext};
use dexdo_core::{OrderBookOrder, OrderBookSnapshot};

const BOOK: &str = "0:917d85f33c24d3ed930355bf98a488456256d27de3c022d2c785315fcfcdb80f";

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

fn row(order_id: u128) -> OrderBookOrder {
    OrderBookOrder {
        order_id,
        owner_note: "0:3bc65e6ab529b648a74ab3da1707edc450e0c91dcc12ffea53f0850117572aa1"
            .to_string(),
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

/// The fold answers with nothing while the book holds a row. The view must show the book's row and
/// attribute it to the book.

/// Red before this change: the empty fold was taken as the view, and the market read as live and
/// empty.
#[tokio::test]
async fn an_empty_fold_does_not_become_an_empty_market() {
    let view = read_executable_market_view_with(
        || async {
            Ok(IndexerMarketContext {
                last_update_id: "ix-1".to_string(),
            })
        },
        || async { Ok((snapshot_with(Vec::new()), "fold-1".to_string())) },
        || async { Ok(snapshot_with(vec![row(3)])) },
    )
    .await
    .expect("the rule reads storage rather than reporting an empty market");

    assert_eq!(
        view.snapshot.orders.len(),
        1,
        "a row the book holds must survive an empty fold"
    );
    assert_eq!(
        view.rows,
        crate::cli::provenance::ROWS_CHAIN_GETTERS,
        "rows taken from storage must be attributed to storage"
    );
    assert_eq!(
        view.source, "chain",
        "an indexer freshness marker may not be attached to rows it did not supply"
    );
}

/// A fold that saw rows keeps them, keeps the indexer's freshness marker, and costs no extra read.
#[tokio::test]
async fn a_fold_that_saw_rows_keeps_its_own_provenance() {
    let storage_reads = std::cell::Cell::new(0u32);
    let view = read_executable_market_view_with(
        || async {
            Ok(IndexerMarketContext {
                last_update_id: "ix-1".to_string(),
            })
        },
        || async { Ok((snapshot_with(vec![row(3)]), "fold-1".to_string())) },
        || {
            storage_reads.set(storage_reads.get() + 1);
            async { Ok(snapshot_with(Vec::new())) }
        },
    )
    .await
    .expect("a non-empty fold answers on its own");

    assert_eq!(view.rows, crate::cli::provenance::ROWS_CHAIN_EVENTS);
    assert_eq!(view.source, "indexer");
    assert!(view.active);
    assert_eq!(storage_reads.get(), 0);
}
