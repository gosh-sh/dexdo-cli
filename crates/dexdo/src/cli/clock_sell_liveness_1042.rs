use super::*;

fn time_before_unix_epoch() -> std::time::SystemTime {
    std::time::UNIX_EPOCH
        .checked_sub(std::time::Duration::from_secs(1))
        .expect("one second before the Unix epoch is representable")
}

#[test]
fn nonzero_sell_deadline_clock_fault_1042_is_not_live() {
    let snapshot = OrderBookSnapshot {
        frame_model: "qwen--qwen3--32b".to_string(),
        model_hash: "model-hash".to_string(),
        order_book: "0:book".to_string(),
        stats: None,
        orders: vec![OrderBookOrder {
            order_id: 1,
            owner_note: "0:seller".to_string(),
            token_contract: Some("0:deal".to_string()),
            is_buy: false,
            price_per_tick: 10,
            ticks: 1,
            escrow: 0,
            deadline: 1_900_000_000,
            flags: 0,
            timestamp: 1,
        }],
    };
    assert_ne!(
        snapshot.orders[0].deadline, 0,
        "the defect needs a dated SELL"
    );

    let clock = crate::cli::provenance::now_unix_at(time_before_unix_epoch());
    let error = match executable_market_rows_with_clock(&snapshot, clock) {
        Ok(rows) => panic!(
            "a broken clock rendered {} live row(s) from a non-zero SELL deadline",
            rows.len()
        ),
        Err(error) => error,
    };

    assert!(error.to_string().contains("system clock"), "{error:#}");
}
