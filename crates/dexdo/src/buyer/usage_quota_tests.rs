use super::*;

fn store() -> (tempfile::TempDir, UsageStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
    (dir, store)
}

fn snapshot(store: &UsageStore) -> Value {
    store.query(&UsageQuery::default(), Utc::now()).unwrap()
}

fn accrue(store: &UsageStore, id: &str, amount: u128) {
    store.register_deal(id, "m").unwrap();
    store
        .apply_deal_expenses(
            id,
            &[AccrualFact {
                source_id: format!("accrual:{id}"),
                occurred_at: 1,
                spent_raw: amount,
            }],
            None,
        )
        .unwrap();
}

fn consume(store: &UsageStore, deal: &str, tokens: u64) {
    let request = format!("request:{deal}");
    store.start_request(&request, "m", "openai", false).unwrap();
    store.link_deal(&request, deal).unwrap();
    store
        .record_call_usage(
            &request,
            "answer",
            "answer",
            BillingUsage::new(tokens, 0, None).unwrap(),
            true,
        )
        .unwrap();
    store
        .finish_request(&request, 200, Some("stop"), None)
        .unwrap();
}

fn settle(store: &UsageStore, id: &str, escrow: u128, refund: Option<u128>) {
    store
        .apply_settlement(SettlementFact {
            source_id: format!("settlement:{id}"),
            deal_id: id.into(),
            occurred_at: 2,
            escrow_raw: escrow,
            buyer_bond_raw: 0,
            buyer_credit_raw: refund,
        })
        .unwrap();
}

#[test]
fn quota_resets_only_on_open_and_excludes_old_settlement_and_buy_fees() {
    let (_dir, store) = store();
    store.open_deal_quota("old", 100, SHELL_UNIT).unwrap();
    accrue(&store, "old", 30 * SHELL_UNIT);
    consume(&store, "old", (30 * dexdo_core::params::TICK_SIZE) as u64);
    let previous = snapshot(&store)["quota"].clone();
    store
        .start_request("new-request", "m", "openai", false)
        .unwrap();
    store.register_deal("unopened", "m").unwrap();
    assert_eq!(snapshot(&store)["quota"], previous);

    store.open_deal_quota("new", 10, 2 * SHELL_UNIT).unwrap();
    assert_eq!(
        snapshot(&store)["quota"],
        json!({"limit":"20","used":"0","remaining":"20","unit":"SHELL"})
    );
    // The old STOP's accounting arrives after the new deal is open.
    settle(&store, "old", 100 * SHELL_UNIT, Some(60 * SHELL_UNIT));
    store
        .apply_operational_expense(OperationalFact {
            source_id: "BUY".into(),
            deal_id: Some("new".into()),
            model: Some("m".into()),
            occurred_at: 3,
            spent_raw: SHELL_UNIT,
        })
        .unwrap();
    let current = snapshot(&store);
    assert_eq!(current["quota"]["used"], "0");
    assert_eq!(current["quota"]["remaining"], "20");
    assert_eq!(current["spend"]["total"]["actual_cost_shell"], "41");
    accrue(&store, "new", 3 * SHELL_UNIT);
    consume(
        &store,
        "new",
        (3 * dexdo_core::params::TICK_SIZE / 2) as u64,
    );
    assert_eq!(snapshot(&store)["quota"]["used"], "3");
    assert_eq!(snapshot(&store)["quota"]["remaining"], "17");
}

#[test]
fn stopped_quota_keeps_actual_expense_and_never_reopens_on_resume() {
    let (dir, store) = store();
    store.open_deal_quota("d", 100, SHELL_UNIT).unwrap();
    accrue(&store, "d", 30 * SHELL_UNIT);
    consume(&store, "d", (30 * dexdo_core::params::TICK_SIZE) as u64);
    store.close_deal_quota("d").unwrap();
    let expected = json!({"limit":"100","used":"30","remaining":"0","unit":"SHELL"});
    assert_eq!(snapshot(&store)["quota"], expected);
    // The refund may still be reconciling; STOP already made the quota unusable.
    settle(&store, "d", 100 * SHELL_UNIT, None);
    assert_eq!(snapshot(&store)["quota"], expected);
    settle(&store, "d", 100 * SHELL_UNIT, Some(70 * SHELL_UNIT));
    assert_eq!(snapshot(&store)["quota"], expected);
    drop(store);
    let resumed = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
    resumed.open_deal_quota("d", 100, SHELL_UNIT).unwrap();
    assert_eq!(snapshot(&resumed)["quota"], expected);
    // Reconciliation failures must not restore usable quota either.
    resumed.mark_deal_incomplete("d", "retry").unwrap();
    assert_eq!(snapshot(&resumed)["quota"], expected);
}

#[test]
fn active_quota_survives_restart_and_is_independent_of_ranges_and_network() {
    let (dir, store) = store();
    store.open_deal_quota("d", 100, SHELL_UNIT).unwrap();
    accrue(&store, "d", 30 * SHELL_UNIT);
    consume(&store, "d", (30 * dexdo_core::params::TICK_SIZE) as u64);
    let expected = snapshot(&store)["quota"].clone();
    for timezone in ["UTC", "Europe/Sofia", "Pacific/Kiritimati"] {
        for days in [1, BUYER_USAGE_MAX_DAYS] {
            let filtered = store
                .query(
                    &UsageQuery {
                        from: Some("2020-01-01T00:00:00Z".into()),
                        to: Some("2020-01-02T00:00:00Z".into()),
                        timezone: Some(timezone.into()),
                        days: Some(days),
                        ..Default::default()
                    },
                    Utc::now(),
                )
                .unwrap();
            assert_eq!(filtered["quota"], expected);
        }
    }
    drop(store);
    let resumed = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
    resumed.open_deal_quota("d", 100, SHELL_UNIT).unwrap();
    assert_eq!(snapshot(&resumed)["quota"], expected);
    let other = UsageStore::open(dir.path().join("usage.sqlite3"), "other").unwrap();
    assert_eq!(snapshot(&other)["quota"]["limit"], "0");
}

#[test]
fn invalid_and_changed_purchase_terms_are_rejected_without_resetting_quota() {
    let (_dir, store) = store();
    assert!(store.open_deal_quota("d", 0, SHELL_UNIT).is_err());
    assert!(store.open_deal_quota("d", 100, 0).is_err());
    assert!(store.open_deal_quota("d", 2, u128::MAX).is_err());
    store.open_deal_quota("d", 100, SHELL_UNIT).unwrap();
    accrue(&store, "d", 30 * SHELL_UNIT);
    let expected = snapshot(&store)["quota"].clone();
    assert!(store.open_deal_quota("d", 101, SHELL_UNIT).is_err());
    assert!(store.open_deal_quota("d", 100, 2 * SHELL_UNIT).is_err());
    assert_eq!(snapshot(&store)["quota"], expected);
}

#[test]
fn opening_a_quota_does_not_change_request_or_financial_history() {
    let (_dir, store) = store();
    let before = snapshot(&store);
    store.open_deal_quota("opened", 100, SHELL_UNIT).unwrap();
    let after = snapshot(&store);
    for field in [
        "usage",
        "daily_usage",
        "model_stats",
        "spend",
        "daily_spend",
        "model_spend",
        "coverage",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert!(store.facts(&UsageFactsQuery::default()).unwrap().complete);
}

#[test]
fn earlier_schema_is_rejected_without_rewriting_history() {
    let (dir, store) = store();
    store.register_deal("old", "m").unwrap();
    accrue(&store, "old", SHELL_UNIT);
    drop(store);
    let path = dir.path().join("usage.sqlite3");
    let connection = Connection::open(&path).unwrap();
    for version in 1..BUYER_USAGE_SCHEMA_VERSION {
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        assert!(UsageStore::open(&path, "shellnet").is_err());
        let unchanged: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(unchanged, version);
    }
}

#[test]
fn legacy_pending_journal_survives_discarding_a_different_unopened_candidate() {
    let (dir, store) = store();
    store.open_deal_quota("A", 100, SHELL_UNIT).unwrap();
    // The earlier journal format had no pending_deals field.
    std::fs::write(
        store.quota_journal.as_ref(),
        r#"{"deal_id":"B","filled_ticks":null,"price_raw":null,"closed":false}"#,
    )
    .unwrap();
    store.begin_deal_quota("C").unwrap();
    store.discard_unopened_deal_quota("C").unwrap();
    assert!(store.query(&UsageQuery::default(), Utc::now()).is_err());
    drop(store);
    let reopened = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
    assert!(reopened.query(&UsageQuery::default(), Utc::now()).is_err());
}

proptest::proptest! {
    #[test]
    fn remaining_is_bounded_and_terminal_is_zero(ticks in 1u128..1000, price in 1u128..1_000_000, tokens in 1u64..(1000 * dexdo_core::params::TICK_SIZE) as u64) {
        let (_dir, store) = store();
        store.open_deal_quota("d", ticks, price).unwrap();
        accrue(&store, "d", price);
        consume(&store, "d", tokens);
        let expense = u128::from(tokens) * price / dexdo_core::params::TICK_SIZE;
        let active = snapshot(&store)["quota"].clone();
        proptest::prop_assert_eq!(&active["limit"], &json!(shell(ticks * price)));
        proptest::prop_assert_eq!(&active["used"], &json!(shell(expense)));
        proptest::prop_assert_eq!(&active["remaining"], &json!(shell((ticks * price).saturating_sub(expense))));
        store.close_deal_quota("d").unwrap();
        let closed = snapshot(&store)["quota"].clone();
        proptest::prop_assert_eq!(&closed["remaining"], "0");
        proptest::prop_assert_eq!(&closed["used"], &active["used"]);
        proptest::prop_assert_eq!(&closed["limit"], &active["limit"]);
    }
}
