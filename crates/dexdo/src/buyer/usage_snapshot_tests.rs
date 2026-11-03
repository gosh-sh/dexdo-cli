use super::*;

fn fixture() -> (tempfile::TempDir, UsageStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
    (dir, store)
}

fn compare(store: &UsageStore) {
    let snapshot = store.admission_snapshot("m").unwrap();
    let full = store.query(&UsageQuery::default(), Utc::now()).unwrap();
    assert_eq!(snapshot["quota"], full["quota"]);
    assert_eq!(
        snapshot["complete"],
        full["usage"]["total"]["total"]["complete"]
    );
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        assert_eq!(
            snapshot["accepted"][key],
            full["usage"]["total"]["total"]["accepted"][key]
                .as_u64()
                .unwrap()
                .to_string(),
            "{key}"
        );
    }
}

#[test]
fn snapshot_2237_preserves_accepted_work_replay_late_binding_and_deal_changes() {
    let (dir, store) = fixture();
    compare(&store);
    store.open_deal_quota("d", 3, SHELL_UNIT).unwrap();
    for (id, status) in [("ok", 200), ("failed", 500), ("cancelled", 499)] {
        store.start_request(id, "m", "openai", false).unwrap();
        compare(&store);
        store
            .record_call_usage(id, "answer", "in_progress", BillingUsage::default(), false)
            .unwrap();
        let mut usage = BillingUsage::new(12, 7, None).unwrap();
        usage.cache_read_tokens = 3;
        usage.cache_write_tokens = 2;
        usage.charged_tokens = Some(8);
        store
            .record_call_usage(id, "answer", "answer", usage, false)
            .unwrap();
        compare(&store);
        store
            .record_call_usage(id, "answer", "answer", usage, true)
            .unwrap();
        store
            .record_call_usage(id, "answer", "answer", usage, true)
            .unwrap();
        store
            .record_call_usage(id, "answer", "answer", usage, false)
            .unwrap();
        assert!(store
            .record_call_usage(
                id,
                "answer",
                "answer",
                BillingUsage::new(1, 1, None).unwrap(),
                true
            )
            .is_err());
        compare(&store);
        store.link_deal(id, "d").unwrap();
        store.link_deal(id, "d").unwrap();
        assert!(store.link_deal(id, "other").is_err());
        store
            .record_call_usage(
                id,
                "verification_b8",
                "v",
                BillingUsage::new(2, 1, None).unwrap(),
                true,
            )
            .unwrap();
        store
            .finish_request(id, status, Some("stop"), None)
            .unwrap();
        compare(&store);
    }
    let old = store.admission_snapshot("m").unwrap();
    store.open_deal_quota("next", 2, SHELL_UNIT).unwrap();
    let next = store.admission_snapshot("m").unwrap();
    assert_eq!(next["accepted"], old["accepted"]);
    assert_eq!(next["quota"]["used"], "0");
    store.close_deal_quota("next").unwrap();
    compare(&store);
    drop(store);
    let reopened = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
    compare(&reopened);
    assert_eq!(
        reopened.admission_snapshot("m").unwrap()["accepted"],
        old["accepted"]
    );
    let other = UsageStore::open(dir.path().join("usage.sqlite3"), "other-network").unwrap();
    assert_eq!(
        other.admission_snapshot("m").unwrap()["accepted"]["input_tokens"],
        "0"
    );
}

#[test]
fn snapshot_2237_uncertainty_and_write_failure_are_not_confirmed_zero() {
    let (_dir, store) = fixture();
    store.start_request("r", "m", "openai", false).unwrap();
    store.finish_request("r", 500, Some("error"), None).unwrap();
    compare(&store);
    assert_eq!(store.admission_snapshot("m").unwrap()["complete"], false);
    store
        .record_call_usage(
            "r",
            "answer",
            "answer",
            BillingUsage::new(2, 3, None).unwrap(),
            true,
        )
        .unwrap();
    compare(&store);
    let before = store.admission_snapshot("m").unwrap();
    store.lock().unwrap().execute_batch("CREATE TRIGGER fail_totals BEFORE UPDATE ON admission_models BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(store
        .record_call_usage(
            "r",
            "verification_b7",
            "v",
            BillingUsage::new(3, 4, None).unwrap(),
            true
        )
        .is_err());
    assert_eq!(store.admission_snapshot("m").unwrap(), before);
    compare(&store);
    store
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_totals;")
        .unwrap();
    store.mark_request_history_incomplete("r").unwrap();
    compare(&store);
    store.begin_deal_quota("unknown").unwrap();
    assert!(store.admission_snapshot("m").is_err());
}

#[test]
fn snapshot_2237_migration_is_atomic_and_backfills_every_network_once() {
    let (dir, store) = fixture();
    {
        let other = UsageStore::open(dir.path().join("usage.sqlite3"), "other").unwrap();
        other.start_request("r", "m", "openai", false).unwrap();
        other
            .record_call_usage(
                "r",
                "answer",
                "answer",
                BillingUsage::new(7, 9, None).unwrap(),
                true,
            )
            .unwrap();
        other.finish_request("r", 200, Some("stop"), None).unwrap();
    }
    store.start_request("r", "m", "openai", false).unwrap();
    store
        .record_call_usage(
            "r",
            "answer",
            "answer",
            BillingUsage::new(3, 4, None).unwrap(),
            true,
        )
        .unwrap();
    store.finish_request("r", 200, Some("stop"), None).unwrap();
    let expected = store.admission_snapshot("m").unwrap();
    store
        .lock()
        .unwrap()
        .execute_batch(
            "DROP TABLE admission_models; DROP TABLE admission_deals; PRAGMA user_version=6;",
        )
        .unwrap();
    drop(store);
    for _ in 0..2 {
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        assert_eq!(store.admission_snapshot("m").unwrap(), expected);
        let other = UsageStore::open(dir.path().join("usage.sqlite3"), "other").unwrap();
        assert_eq!(
            other.admission_snapshot("m").unwrap()["accepted"]["output_tokens"],
            "9"
        );
    }
    let connection = Connection::open(dir.path().join("usage.sqlite3")).unwrap();
    connection.execute_batch("DROP TABLE admission_models; DROP TABLE admission_deals; PRAGMA user_version=6; UPDATE request_usage SET input_tokens='broken';").unwrap();
    assert!(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").is_err());
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='admission_models'",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn snapshot_2237_progress_markers_never_add_paid_work() {
    let (_dir, store) = fixture();
    store.open_deal_quota("d", 1, SHELL_UNIT).unwrap();
    store.start_request("r", "m", "openai", false).unwrap();
    store.link_deal("r", "d").unwrap();
    store
        .record_call_usage(
            "r",
            "answer",
            "in_progress",
            BillingUsage::new(7, 9, None).unwrap(),
            true,
        )
        .unwrap();
    let snapshot = store.admission_snapshot("m").unwrap();
    assert_eq!(snapshot["accepted"]["input_tokens"], "0");
    assert_eq!(snapshot["accepted"]["output_tokens"], "0");
    assert_eq!(snapshot["quota"]["used"], "0");
    assert_eq!(snapshot["complete"], false);
}

#[test]
fn snapshot_2237_exact_cumulative_rounding_large_values_and_model_isolation() {
    let (_dir, store) = fixture();
    let half_tick = u64::try_from(dexdo_core::params::TICK_SIZE / 2).unwrap();
    store.open_deal_quota("d", 1, 1).unwrap();
    for (id, model) in [("a", "m"), ("b", "other")] {
        store.start_request(id, model, "openai", false).unwrap();
        store.link_deal(id, "d").unwrap();
        let mut usage = BillingUsage::new(half_tick, 0, None).unwrap();
        usage.charged_tokens = Some(half_tick);
        store
            .record_call_usage(id, "answer", "answer", usage, true)
            .unwrap();
        store.finish_request(id, 200, Some("stop"), None).unwrap();
    }
    assert_eq!(
        store.admission_snapshot("m").unwrap()["quota"]["used"],
        shell(1)
    );
    assert_eq!(
        store.admission_snapshot("m").unwrap()["accepted"]["input_tokens"],
        half_tick.to_string()
    );
    for id in ["large-a", "large-b"] {
        store.start_request(id, "large", "openai", false).unwrap();
        store
            .record_call_usage(
                id,
                "answer",
                "answer",
                BillingUsage::new(u64::MAX, 0, None).unwrap(),
                true,
            )
            .unwrap();
        store.finish_request(id, 200, Some("stop"), None).unwrap();
    }
    assert_eq!(
        store.admission_snapshot("large").unwrap()["accepted"]["input_tokens"],
        (u128::from(u64::MAX) * 2).to_string()
    );
}

proptest::proptest! {
    #[test]
    fn snapshot_2237_invariant_matches_full_history(
        records in proptest::collection::vec((0u64..10000,0u64..10000,0u8..3,proptest::bool::ANY,proptest::bool::ANY),0..18)
    ) {
        let (_dir,store)=fixture();
        store.open_deal_quota("d",3,SHELL_UNIT).unwrap();
        for (i,(input,output,outcome,accepted,verification)) in records.into_iter().enumerate() {
            let id=i.to_string();
            store.start_request(&id,"m","openai",false).unwrap();
            store.link_deal(&id,"d").unwrap();
            let mut usage=BillingUsage::new(input,output,None).unwrap();
            usage.cache_read_tokens=input/3;
            usage.cache_write_tokens=input/4;
            store.record_call_usage(&id,"answer","answer",usage,accepted).unwrap();
            if verification { store.record_call_usage(&id,"verification_b7","v",usage,true).unwrap(); }
            store.finish_request(&id,[200,499,500][usize::from(outcome)],Some("stop"),None).unwrap();
            compare(&store);
            store.record_call_usage(&id,"answer","answer",usage,true).unwrap();
            compare(&store);
        }
    }
}

#[test]
fn snapshot_2237_keyed_reads_have_constant_sql_work() {
    use rusqlite::StatementStatus;
    let (_dir, store) = fixture();
    let steps = || {
        let connection = store.lock().unwrap();
        [MODEL_SQL, DEAL_SQL].map(|sql| {
            let mut statement = connection.prepare(sql).unwrap();
            let _: Option<String> = statement
                .query_row(params!["shellnet", "absent"], |row| row.get(0))
                .optional()
                .unwrap();
            let plan: Vec<String> = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(params!["shellnet", "absent"], |row| row.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter().all(|step| step.starts_with("SEARCH ")),
                "{plan:?}"
            );
            assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
            let steps = statement.get_status(StatementStatus::VmStep);
            eprintln!("2237_SQL plan={plan:?} vm_steps={steps} fullscan_steps=0");
            steps
        })
    };
    let empty = steps();
    store.lock().unwrap().execute_batch("WITH RECURSIVE ids(i) AS(VALUES(1) UNION ALL SELECT i+1 FROM ids WHERE i<100000) INSERT INTO admission_models SELECT 'shellnet','model-'||i,'0','0','0','0','0' FROM ids;
        INSERT INTO admission_deals SELECT network,model,'0' FROM admission_models;").unwrap();
    assert_eq!(steps(), empty);
}
