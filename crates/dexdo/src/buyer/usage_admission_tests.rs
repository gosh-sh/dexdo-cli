use super::*;

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
                writer.record_call_usage(
                    "new",
                    "answer",
                    "answer",
                    BillingUsage::new(1, 1, None).unwrap(),
                    true,
                )?;
                writer.finish_request("new", 200, Some("stop"), None)?;
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
    assert!(store.link_deal("late", "other-deal").is_err());
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

#[test]
fn issue_2269_legacy_usage_closes_locally_and_preserves_confirmed_phases() {
    let (dir, store) = store();
    for phase in ["answer", "verification_b7", "verification_b8"] {
        store.start_request(phase, "model", "openai", true).unwrap();
        store.link_deal(phase, "deal").unwrap();
        if phase == "answer" {
            store
                .finish_request(phase, 499, Some("cancelled"), Some("consumer_disconnect"))
                .unwrap();
        }
        store
            .record_call_usage(phase, phase, "in_progress", BillingUsage::default(), false)
            .unwrap();
    }
    let usage = BillingUsage {
        charged_tokens: Some(5),
        cumulative_billable_tokens: Some(12),
        cache_read_tokens: 2,
        ..BillingUsage::new(4, 3, None).unwrap()
    };
    store
        .record_call_usage(
            "verification_b7",
            "verification_b8",
            "verification_b8",
            usage,
            true,
        )
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute_batch(include_str!("drop_usage_change_journal.sql"))
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TABLE request_usage_recovery (request_id TEXT);
         INSERT INTO request_usage_recovery VALUES ('answer'), ('verification_b8');
         CREATE TABLE request_usage_sequence (sequence INTEGER);
         PRAGMA user_version=9;",
        )
        .unwrap();
    drop(store);
    for expected_interruptions in [2, 0] {
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        assert_eq!(
            store.recover_interrupted_requests().unwrap(),
            expected_interruptions
        );
        assert_eq!(store.confirmed_usage_above("deal", 0).unwrap(), 5);
        assert_eq!(store.confirmed_usage_above("deal", 9).unwrap(), 3);
        let connection = store.lock().unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT failure_stage FROM requests WHERE request_id='answer'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "consumer_disconnect"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM request_usage WHERE accepted=0 OR
              (call_id='in_progress' AND charged_tokens!='0')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name IN ('request_usage_recovery','request_usage_sequence')",
            [], |row| row.get::<_, i64>(0)
        ).unwrap(), 0);
    }
}

#[test]
fn issue_2269_confirmed_usage_retries_the_local_write_once() {
    let (dir, store) = store();
    let accepted = Utc.with_ymd_and_hms(2026, 11, 12, 23, 59, 59).unwrap();
    let after_midnight = Utc.with_ymd_and_hms(2026, 11, 13, 0, 0, 1).unwrap();
    store
        .start_request_at("r", "model", "openai", true, accepted.timestamp_millis())
        .unwrap();
    store.link_deal("r", "deal").unwrap();
    store.record_deal_price("deal", SHELL_UNIT).unwrap();
    let usage = BillingUsage {
        charged_tokens: Some(5),
        cumulative_billable_tokens: Some(17),
        cache_read_tokens: 2,
        ..BillingUsage::new(4, 3, None).unwrap()
    };
    store
        .record_call_usage("r", "answer", "answer", usage, false)
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_bill BEFORE UPDATE OF accepted ON request_usage
         WHEN NEW.call_id='answer' BEGIN SELECT RAISE(ABORT, 'temporary write failure'); END;",
        )
        .unwrap();
    store
        .record_confirmed_bill(ConfirmedBill {
            request_id: "r".into(),
            phase: "answer".into(),
            usage,
            buyer_billable_floor: None,
            accepted_at: accepted.timestamp_millis(),
        })
        .unwrap();
    store
        .finish_request_at("r", 200, Some("stop"), None, accepted.timestamp_millis())
        .unwrap();
    assert_eq!(store.accepted_tokens("deal").unwrap(), 0);
    assert!(store.flush_confirmed_usage().is_err());
    let path = dir.path().join("usage.sqlite3");
    drop(store);
    assert!(UsageStore::open(&path, "shellnet").is_err());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("DROP TRIGGER reject_bill")
        .unwrap();
    drop(connection);
    let reopened = UsageStore::open(&path, "shellnet").unwrap();
    reopened.flush_confirmed_usage().unwrap();
    drop(reopened);
    let reopened = UsageStore::open(path, "shellnet").unwrap();
    assert_eq!(reopened.accepted_tokens("deal").unwrap(), 5);
    assert_eq!(reopened.confirmed_usage_above("deal", 14).unwrap(), 3);
    assert_eq!(
        reopened.admission_snapshot("model").unwrap()["accepted"]["cache_read_tokens"],
        "2"
    );
    let query = UsageQuery {
        from: Some("2026-11-12T00:00:00Z".into()),
        to: Some("2026-11-13T00:00:00Z".into()),
        days: Some(2),
        timezone: Some("UTC".into()),
        ..Default::default()
    };
    let report = reopened.query(&query, after_midnight).unwrap();
    assert_eq!(report["spend"]["total"]["cost_shell"], "0.000005");
    assert_eq!(report["spend"]["period"]["cost_shell"], "0.000005");
    assert_eq!(report["daily_spend"][0]["date"], "2026-11-12");
    assert_eq!(report["daily_spend"][0]["cost_shell"], "0.000005");
    assert_eq!(report["daily_spend"][1]["date"], "2026-11-13");
    assert_eq!(report["daily_spend"][1]["cost_shell"], "0");
}
