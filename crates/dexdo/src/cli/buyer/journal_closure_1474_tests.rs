//! Closing the durable BUY record is a reading of the money, not a list of outcomes.

//! A note reported a buy refused forever. Its order had expired and been swept from the book, the
//! escrow had come back -- `note balance` showed `lockedInOrders: none reported` against a whole
//! 100 SHELL nominal -- and yet the record stayed open, so the money lock was retaken on every
//! journal run and every later buy was refused. `closes_record` listed `Cancelled | Rejected` and
//! a swept-and-refunded expiry was not in the list.

//! The list is the defect, not its length. These two tests hold the reading from both sides,
//! because only one of them is the fix and the other is what keeps the fix from becoming a
//! different hole: closing on the sweep ALONE would lift a money lock over an escrow the book has
//! not accounted for.

use super::{classify_buyer_submit_standing, BuyerSubmitJournal, BuyerSubmitStanding};

const ORDER_ID: u128 = 1118;
const TICKS: u128 = 2;
const PRICE_PER_TICK: u128 = 1_000_000;
const SUBMITTED_AT: u64 = 1_000;

fn journal() -> BuyerSubmitJournal {
    let token_contract = format!("0:{}", "3".repeat(64));
    let escrow = dexdo_core::required_escrow_for_buy(TICKS, PRICE_PER_TICK);
    BuyerSubmitJournal {
        schema: super::BUYER_SUBMIT_JOURNAL_SCHEMA.to_string(),
        note_addr: format!("0:{}", "1".repeat(64)),
        order_book: format!("0:{}", "2".repeat(64)),
        intent: super::BuyerSubmitIntent::foreground(),
        expected_token_contract: Some(token_contract.clone()),
        quoted_order: None,
        quote: dexdo_core::ExecutableQuote {
            filled_ticks: TICKS,
            total_with_fee: escrow,
            complete: true,
            fills: vec![dexdo_core::QuoteFill {
                order_id: 7,
                token_contract,
                ticks: TICKS,
                price_per_tick: PRICE_PER_TICK,
                cost_with_fee: escrow,
            }],
        },
        cursor: dexdo_core::MatchWatchCursor::new(
            i64::try_from(SUBMITTED_AT).expect("fixture timestamp fits"),
        ),
        ticks: TICKS,
        max_price_per_tick: PRICE_PER_TICK,
        escrow,
        // The book proved the id; the live record in carried it too.
        order_id: Some(ORDER_ID),
        submit_identity: format!("boc-sha256:{}", "a".repeat(64)),
        created_at_unix: SUBMITTED_AT,
        resolved_match: None,
        resolved_matches: Vec::new(),
    }
}

fn fact(kind: dexdo_core::BuyerOrderFactKind) -> dexdo_core::BuyerOrderFact {
    dexdo_core::BuyerOrderFact {
        created_at: i64::try_from(SUBMITTED_AT).expect("fixture timestamp fits"),
        note: format!("0:{}", "1".repeat(64)),
        kind,
    }
}

fn placed() -> dexdo_core::BuyerOrderFact {
    fact(dexdo_core::BuyerOrderFactKind::Placed {
        order_id: ORDER_ID,
        price_per_tick: PRICE_PER_TICK,
        ticks: TICKS,
        deadline: SUBMITTED_AT + 60,
    })
}

fn swept() -> dexdo_core::BuyerOrderFact {
    fact(dexdo_core::BuyerOrderFactKind::Expired { order_id: ORDER_ID })
}

fn refunded() -> dexdo_core::BuyerOrderFact {
    fact(dexdo_core::BuyerOrderFactKind::Refunded {
        order_id: ORDER_ID,
        amount: dexdo_core::required_escrow_for_buy(TICKS, PRICE_PER_TICK),
    })
}

/// The defect itself: the book swept the order AND accounted for the refund, so the escrow is out
/// of the lock and the record has nothing left to guard.
#[test]
fn a_swept_expiry_the_book_also_refunded_closes_the_record() {
    let standing = classify_buyer_submit_standing(
        &journal(),
        &[placed(), swept(), refunded()],
        SUBMITTED_AT + 120,
    );
    assert!(
        matches!(standing, BuyerSubmitStanding::Expired { cleared: true, .. }),
        "the fixture must reach a swept expiry: {standing:?}"
    );
    assert!(
        standing.closes_record(),
        "a swept expiry the book refunded leaves nothing under the lock, so the record closes: \
         {standing:?}"
    );
    // The third symptom of the same defect: this line announced `terminal=false` unconditionally,
    // so even a fully accounted-for expiry told the operator it was not terminal.
    let line = standing.operator_state();
    assert!(
        line.contains("outcome=expired terminal=true"),
        "the operator's record must read the money, not a constant: {line}"
    );
}

/// The other side, and the reason this is not simply "expiry closes the record": a sweep proves the
/// order left the book, never that the escrow came back. Closing here would lift the money lock
/// over an escrow nobody has accounted for.
#[test]
fn a_sweep_the_book_never_refunded_keeps_the_record_open() {
    let standing =
        classify_buyer_submit_standing(&journal(), &[placed(), swept()], SUBMITTED_AT + 120);
    assert!(
        matches!(standing, BuyerSubmitStanding::Expired { cleared: true, .. }),
        "the fixture must reach a swept expiry: {standing:?}"
    );
    assert!(
        !standing.closes_record(),
        "a sweep alone does not account for the escrow, so the record is retained: {standing:?}"
    );
    let line = standing.operator_state();
    assert!(
        line.contains("outcome=expired terminal=false"),
        "an unaccounted-for sweep must not read as terminal: {line}"
    );
}
