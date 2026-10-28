use super::*;
use rusqlite::StatementStatus;

fn store() -> (tempfile::TempDir, UsageStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
    (dir, store)
}

fn completed_history(store: &UsageStore, first: u32, last: u32) {
    let mut connection = store.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "WITH RECURSIVE ids(i) AS (VALUES(?1) UNION ALL SELECT i+1 FROM ids WHERE i<?2)
         INSERT INTO requests(network,request_id,runtime_id,model,protocol,streamed,deal_id,
            started_at,finished_at,outcome)
         SELECT 'shellnet','completed-'||i,'previous','model','openai',0,'deal',0,1,'succeeded'
         FROM ids",
            params![first, last],
        )
        .unwrap();
    // Successful requests also retain their accepted in-progress state rows.
    transaction
        .execute(
            "INSERT INTO request_usage(network,request_id,phase,call_id,input_tokens,output_tokens,
            total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens,deal_id)
         SELECT network,request_id,'answer','in_progress','0','0','0','0',1,'0','0',deal_id
         FROM requests WHERE network='shellnet' AND rowid BETWEEN ?1 AND ?2",
            params![first, last],
        )
        .unwrap();
    transaction.commit().unwrap();
}

#[test]
fn issue_2193_admission_does_not_walk_100000_completed_requests() {
    let (_dir, store) = store();
    let steps = |deal: &str| {
        let connection = store.lock().unwrap();
        let mut statement = connection.prepare(UNRESOLVED_OUTPUT_USAGE_SQL).unwrap();
        let pending: bool = statement
            .query_row(params![store.network(), deal, &*store.runtime_id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!pending);
        (
            statement.get_status(StatementStatus::VmStep),
            statement.get_status(StatementStatus::FullscanStep),
        )
    };
    completed_history(&store, 1, 1000);
    let small = steps("deal");
    completed_history(&store, 1001, 100_000);
    let large = steps("deal");
    eprintln!("issue 2193: SQLite steps for 1000 requests={small:?}; 100000={large:?}");
    assert!(
        large.0 <= small.0 + 100,
        "completed history increased work: {small:?} -> {large:?}"
    );
    assert_eq!(large.1, 0, "admission must not scan history");

    // Unresolved history of another deal must not slow a clean deal either.
    store
        .lock()
        .unwrap()
        .execute("UPDATE request_usage SET accepted=0", [])
        .unwrap();
    let foreign = steps("empty-deal");
    assert!(
        foreign.0 <= small.0 + 100,
        "other deals increased work: {small:?} -> {foreign:?}"
    );

    let connection = store.lock().unwrap();
    let plan: Vec<String> = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {UNRESOLVED_OUTPUT_USAGE_SQL}"))
        .unwrap()
        .query_map(
            params![store.network(), "deal", &*store.runtime_id],
            |row| row.get(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    eprintln!("issue 2193 query plan: {plan:#?}");
    for step in &plan {
        assert!(
            !step.starts_with("SCAN r") && !step.starts_with("SCAN u"),
            "{step}"
        );
        if step.starts_with("SEARCH r ") || step.starts_with("SEARCH u ") {
            assert!(
                step.contains("deal_id=?") || step.contains("request_id=?"),
                "{step}"
            );
        }
    }
}

proptest::proptest! {
    #[test]
    fn issue_2193_unresolved_check_preserves_accounting_states(
        states in proptest::collection::vec(
            (proptest::bool::ANY, proptest::bool::ANY, proptest::bool::ANY,
             proptest::bool::ANY, proptest::bool::ANY, proptest::bool::ANY,
             proptest::bool::ANY, 0u8..3), 0..24),
    ) {
        let (_dir, store) = store();
        let mut expected = false;
        {
            let mut connection = store.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for (id, &(same_network, same_deal, complete, finished, old_runtime,
                       has_state, accepted, phase)) in states.iter().enumerate() {
                let network = if same_network { store.network() } else { "other-network" };
                let deal = if same_deal { "deal" } else { "other-deal" };
                let runtime = if old_runtime { "previous" } else { &*store.runtime_id };
                let phase = ["answer", "verification_b8", "verification_b7"][phase as usize];
                let id = id.to_string();
                transaction.execute(
                    "INSERT INTO requests(network,request_id,runtime_id,model,protocol,streamed,
                     deal_id,started_at,finished_at,history_complete)
                     VALUES(?1,?2,?3,'model','openai',0,?4,0,?5,?6)",
                    params![network, id, runtime, deal, finished.then_some(1), complete],
                ).unwrap();
                if has_state {
                    transaction.execute(
                        "INSERT INTO request_usage(network,request_id,phase,call_id,input_tokens,
                         output_tokens,total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens,deal_id)
                         VALUES(?1,?2,?3,'in_progress','0','0','0','0',?4,'0','0',?5)",
                        params![network, id, phase, accepted, deal],
                    ).unwrap();
                }
                expected |= same_network && same_deal && (!complete || (!finished && old_runtime)
                    || (has_state && !accepted && (phase == "answer" || finished || old_runtime)));
            }
            transaction.commit().unwrap();
        }
        proptest::prop_assert_eq!(store.has_unresolved_output_usage("deal").unwrap(), expected);
    }
}

#[test]
fn issue_2193_history_snapshot_does_not_block_request_accounting() {
    let (_dir, store) = store();
    let mut history = store.lock_history().unwrap();
    let snapshot = history.transaction().unwrap();
    let count = || {
        snapshot
            .query_row("SELECT count(*) FROM requests", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    assert_eq!(count(), 0);
    let writer = store.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send((|| -> Result<()> {
                writer.start_request("new", "model", "openai", true)?;
                writer.link_deal("new", "deal")?;
                writer.record_call_usage(
                    "new",
                    "answer",
                    "in_progress",
                    BillingUsage::default(),
                    false,
                )?;
                // The admission check must see new output despite the old history snapshot.
                ensure!(
                    writer.has_unresolved_output_usage("deal")?,
                    "missing unresolved output"
                );
                writer.record_call_usage(
                    "new",
                    "answer",
                    "answer",
                    BillingUsage::new(1, 1, None).unwrap(),
                    true,
                )?;
                writer.finish_request("new", 200, Some("stop"), None)?;
                ensure!(
                    !writer.has_unresolved_output_usage("deal")?,
                    "usage did not reconcile"
                );
                Ok(())
            })())
            .unwrap();
    });
    let result = receiver.recv_timeout(BUYER_USAGE_SQLITE_BUSY_TIMEOUT);
    assert_eq!(count(), 0, "history transaction retains its snapshot");
    // Release before asserting the timeout, so a regression cannot strand the writer thread.
    drop(snapshot);
    drop(history);
    worker.join().unwrap();
    result
        .expect("history reader blocked request accounting")
        .unwrap();
    assert_eq!(store.accepted_tokens("deal").unwrap(), 2);
    assert_eq!(
        store.query(&UsageQuery::default(), Utc::now()).unwrap()["usage"]["total"]["requests"]
            ["succeeded"],
        1
    );
    assert!(store.facts(&Default::default()).unwrap().facts.is_empty());
}

#[test]
fn issue_2193_existing_database_reopens_with_indexes_and_pending_output() {
    let (dir, store) = store();
    store
        .start_request("pending", "model", "openai", true)
        .unwrap();
    store.link_deal("pending", "deal").unwrap();
    store
        .record_call_usage(
            "pending",
            "answer",
            "in_progress",
            BillingUsage::default(),
            false,
        )
        .unwrap();
    store
        .finish_request("pending", 502, Some("error"), Some("upstream_stream"))
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute_batch(
            "DROP INDEX requests_incomplete_usage; DROP INDEX requests_unfinished_usage;
         DROP INDEX request_usage_pending; ALTER TABLE request_usage DROP COLUMN deal_id;
             PRAGMA user_version=5;",
        )
        .unwrap();
    drop(store);
    let path = dir.path().join("usage.sqlite3");
    let reopened = UsageStore::open(&path, "shellnet").unwrap();
    assert!(reopened.has_unresolved_output_usage("deal").unwrap());
    let indexes: u32 = reopened
        .lock()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name IN
         ('requests_incomplete_usage','requests_unfinished_usage','request_usage_pending')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 3);
    // A different phase or an unaccepted observation cannot clear the unresolved answer.
    let usage = BillingUsage::new(1, 1, None).unwrap();
    reopened
        .record_call_usage("pending", "verification_b8", "verification_b8", usage, true)
        .unwrap();
    reopened
        .record_call_usage("pending", "answer", "answer", usage, false)
        .unwrap();
    assert!(reopened.has_unresolved_output_usage("deal").unwrap());
    reopened
        .record_call_usage("pending", "answer", "answer", usage, true)
        .unwrap();
    assert!(!reopened.has_unresolved_output_usage("deal").unwrap());
    drop(reopened);
    let reopened = UsageStore::open(path, "shellnet").unwrap();
    assert!(!reopened.has_unresolved_output_usage("deal").unwrap());
    assert_eq!(reopened.accepted_tokens("deal").unwrap(), 4);
}

#[test]
fn issue_2193_late_deal_binding_is_atomic_and_cannot_move_usage() {
    let (_dir, store) = store();
    store
        .start_request("late", "model", "openai", true)
        .unwrap();
    store
        .record_call_usage(
            "late",
            "answer",
            "in_progress",
            BillingUsage::default(),
            false,
        )
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_usage_binding BEFORE UPDATE OF deal_id ON request_usage
         BEGIN SELECT RAISE(ABORT, 'binding failure'); END;",
        )
        .unwrap();
    assert!(store.link_deal("late", "deal").is_err());
    let binding: Option<String> = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT deal_id FROM requests WHERE request_id='late'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        binding, None,
        "failed usage binding must roll back the request binding"
    );
    store
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_usage_binding")
        .unwrap();
    store.link_deal("late", "deal").unwrap();
    store.link_deal("late", "deal").unwrap();
    assert!(store.has_unresolved_output_usage("deal").unwrap());
    assert!(store.link_deal("late", "other-deal").is_err());
    assert!(!store.has_unresolved_output_usage("other-deal").unwrap());
    assert!(store.has_unresolved_output_usage("deal").unwrap());
}

#[test]
fn issue_2193_fifty_concurrent_accounting_clients() {
    for history in [1000, 100_000] {
        let (_dir, store) = store();
        completed_history(&store, 1, history);
        let barrier = std::sync::Barrier::new(50);
        let mut latencies = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..50)
                .map(|client| {
                    let store = &store;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        (0..20)
                            .map(|request| {
                                let started = std::time::Instant::now();
                                let id = format!("client-{client}-{request}");
                                store.start_request(&id, "model", "openai", false).unwrap();
                                assert!(!store.has_unresolved_output_usage("deal").unwrap());
                                store
                                    .finish_request(&id, 429, Some("error"), Some("upstream_open"))
                                    .unwrap();
                                started.elapsed()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        latencies.sort();
        eprintln!("issue 2193 accounting-only load: history={history}, clients=50, requests={}, median={:?}, p90={:?}",
            latencies.len(), latencies[latencies.len()/2], latencies[latencies.len()*9/10]);
        assert_eq!(
            store.query(&UsageQuery::default(), Utc::now()).unwrap()["usage"]["total"]["requests"]
                ["rejected"],
            latencies.len()
        );
    }
}
