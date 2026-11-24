use super::super::UsageStore;
use proptest::prelude::*;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, process::Command};

fn reader(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn head(db: &Connection) -> (Vec<u8>, i64, Option<Vec<u8>>) {
    db.query_row(
        "SELECT source_id,COALESCE(c.seq,0),c.event_id
         FROM usage_change_journal
         LEFT JOIN usage_changes c ON c.seq=(SELECT seq FROM sqlite_sequence WHERE name='usage_changes')
         WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

#[derive(Debug)]
struct Event {
    seq: i64,
    table_name: String,
    operation: String,
    key: String,
    data: Option<Value>,
}

fn events(db: &Connection, after: i64, through: i64) -> Vec<Event> {
    db.prepare(
        "SELECT seq,table_name,operation,row_key,row_data FROM usage_changes
         WHERE seq>?1 AND seq<=?2 ORDER BY seq",
    )
    .unwrap()
    .query_map(params![after, through], |row| {
        let data: Option<String> = row.get(4)?;
        Ok(Event {
            seq: row.get(0)?,
            table_name: row.get(1)?,
            operation: row.get(2)?,
            key: row.get(3)?,
            data: data.map(|data| serde_json::from_str(&data).unwrap()),
        })
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

#[test]
fn upgraded_large_history_drains_only_late_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    drop(UsageStore::open(&path, "shellnet").unwrap());
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!("drop_usage_change_journal.sql"))
        .unwrap();
    db.execute_batch(
        "PRAGMA user_version=11;
         WITH RECURSIVE ids(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM ids WHERE i<200000)
         INSERT INTO requests(network,request_id,runtime_id,model,protocol,streamed,started_at)
         SELECT 'shellnet','req-'||i,'old-runtime','model','openai',0,1 FROM ids;
         INSERT INTO request_usage(network,request_id,phase,call_id,input_tokens,output_tokens,
            total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens)
         VALUES ('shellnet','req-1','answer','answer','1','2','3','3',0,'0','0'),
                ('shellnet','req-1','answer','in_progress','0','0','0','0',0,'0','0');",
    )
    .unwrap();

    let store = UsageStore::open(&path, "shellnet").unwrap();
    let mut read = reader(&path);
    let baseline = read.transaction().unwrap();
    let (source_id, start, _) = head(&baseline);
    assert_eq!(start, 0);
    assert_eq!(
        baseline
            .query_row("SELECT count(*) FROM requests", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        200_000
    );
    db.execute_batch(
        "BEGIN IMMEDIATE;
         UPDATE requests SET finished_at=2,outcome='succeeded',history_complete=0 WHERE rowid=1;
         UPDATE request_usage SET accepted=1,accepted_at=2 WHERE call_id='answer';
         DELETE FROM request_usage WHERE call_id='in_progress';
         COMMIT;",
    )
    .unwrap();
    assert_eq!(head(&baseline).1, start);
    assert_eq!(
        baseline
            .query_row(
                "SELECT history_complete FROM requests WHERE rowid=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    baseline.commit().unwrap();
    drop(store);
    drop(db);

    let (final_source, through, _) = head(&read);
    assert_eq!(final_source, source_id);
    let batch = events(&read, start, through);
    assert_eq!(batch.len(), 3);
    assert_eq!(batch[0].table_name, "requests");
    assert_eq!(batch[0].key, r#"["shellnet","req-1"]"#);
    assert_eq!(batch[0].data.as_ref().unwrap()["outcome"], "succeeded");
    assert_eq!(batch[0].data.as_ref().unwrap()["history_complete"], 0);
    let call = batch[1].data.as_ref().unwrap();
    assert_eq!(call["accepted"], 1);
    assert_eq!(call["charged_tokens"], "3");
    assert_eq!(batch[2].operation, "delete");
    assert_eq!(
        batch[2].key,
        r#"["shellnet","req-1","answer","in_progress"]"#
    );
    assert_eq!(batch[2].data, None);
    drop(UsageStore::open(&path, "shellnet").unwrap());
    assert_eq!(head(&read).1, through);
}

#[test]
fn quota_and_price_share_the_journal_and_no_op_updates_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = UsageStore::open(&path, "shellnet").unwrap();
    let read = reader(&path);

    store.open_deal_quota("deal", 2, 3).unwrap();
    store.start_request("r", "model", "openai", false).unwrap();
    store.link_deal("r", "deal").unwrap();
    let through = head(&read).1;
    store.set_request_streamed("r", false).unwrap();
    store.link_deal("r", "deal").unwrap();
    assert_eq!(head(&read).1, through);
    store.close_deal_quota("deal").unwrap();
    let batch = events(&read, 0, head(&read).1);
    assert_eq!(batch.len(), 5);
    assert_eq!(batch[0].table_name, "deal_prices");
    assert_eq!(batch[0].data.as_ref().unwrap()["price_raw"], "3");
    assert_eq!(batch[1].table_name, "deal_quota");
    assert_eq!(batch[1].data.as_ref().unwrap()["filled_ticks"], "2");
    assert_eq!(batch[4].data.as_ref().unwrap()["closed"], 1);
}

#[test]
fn pruning_preserves_the_boundary_anchor_and_rolls_back_with_its_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = UsageStore::open(&path, "shellnet").unwrap();
    let read = reader(&path);
    let writer = store.lock().unwrap();
    writer.execute_batch(
        "UPDATE usage_change_journal SET retain_events=2,prune_batch=2;
         INSERT INTO deal_prices VALUES ('shellnet','a','1'),('shellnet','b','2'),('shellnet','c','3');",
    ).unwrap();
    let (source_id, seq, event_id) = read
        .query_row(
            "SELECT source_id,seq,event_id FROM usage_change_journal JOIN usage_changes ON seq=2",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .unwrap();
    writer
        .execute_batch(
            "BEGIN IMMEDIATE; INSERT INTO deal_prices VALUES ('shellnet','d','4'); ROLLBACK;",
        )
        .unwrap();
    assert_eq!(events(&read, 0, head(&read).1).len(), 3);
    writer
        .execute_batch("INSERT INTO deal_prices VALUES ('shellnet','d','4')")
        .unwrap();
    let (floor, floor_event_id): (i64, Vec<u8>) = read
        .query_row(
            "SELECT pruned_through,pruned_event_id FROM usage_change_journal WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((floor, floor_event_id), (seq, event_id));
    assert!(1 < floor);
    let (actual_source, through, _) = head(&read);
    assert_eq!(actual_source, source_id);
    assert_eq!(
        events(&read, 0, through)
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        [3, 4]
    );
}

#[test]
fn copied_history_diverging_at_the_same_sequence_has_a_different_event_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let copy = dir.path().join("copy.sqlite3");
    let store = UsageStore::open(&path, "shellnet").unwrap();
    store.record_deal_price("common", 1).unwrap();
    store
        .lock()
        .unwrap()
        .execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
        .unwrap();
    let copied = UsageStore::open(&copy, "shellnet").unwrap();
    store.record_deal_price("original", 2).unwrap();
    copied.record_deal_price("restored", 3).unwrap();
    let (original_id, original_seq, original_event) = head(&reader(&path));
    let (copied_id, copied_seq, copied_event) = head(&reader(&copy));
    assert_eq!(copied_id, original_id);
    assert_eq!(copied_seq, original_seq);
    assert_ne!(copied_event, original_event);
    let fresh = dir.path().join("fresh.sqlite3");
    drop(UsageStore::open(&fresh, "shellnet").unwrap());
    assert_ne!(head(&reader(&fresh)).0, original_id);
}

#[test]
fn interrupted_writer_keeps_committed_journal_events_in_wal() {
    const CHILD_DB: &str = "DEXDO_USAGE_CHANGE_JOURNAL_TEST_DB";
    if let Some(path) = std::env::var_os(CHILD_DB) {
        let store = UsageStore::open(Path::new(&path), "shellnet").unwrap();
        store
            .finish_request_at("r", 200, Some("stop"), None, 2)
            .unwrap();
        let writer = store.lock().unwrap();
        writer
            .execute_batch(
                "BEGIN IMMEDIATE; UPDATE requests SET history_complete=0 WHERE request_id='r';",
            )
            .unwrap();
        // Leave the SQLite connections and the uncommitted transaction open.
        std::process::exit(0);
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = UsageStore::open(&path, "shellnet").unwrap();
    store
        .start_request_at("r", "model", "openai", false, 1)
        .unwrap();
    drop(store);
    let (_, baseline, _) = head(&reader(&path));
    let module = module_path!().split_once("::").unwrap().1;
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(format!(
            "{module}::interrupted_writer_keeps_committed_journal_events_in_wal"
        ))
        .env(CHILD_DB, &path)
        .output()
        .unwrap();
    assert!(child.status.success(), "{child:?}");
    assert!(dir.path().join("usage.sqlite3-wal").is_file(), "{child:?}");
    let read = reader(&path);
    let replay = events(&read, baseline, head(&read).1);
    assert_eq!(replay.len(), 1);
    let finished = replay[0].data.as_ref().unwrap();
    assert_eq!(finished["outcome"], "succeeded");
    assert_eq!(finished["history_complete"], 1);
    drop(UsageStore::open(&path, "shellnet").unwrap());
    assert_eq!(head(&read).1, replay[0].seq);
}

proptest! {
    #[test]
    fn replayed_call_projection_matches_committed_usage(
        changes in prop::collection::vec((0..5u8, 0..100u32, any::<bool>(), any::<bool>(), any::<bool>()), 1..40)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = UsageStore::open(&path, "shellnet").unwrap();
        store.start_request("r", "model", "openai", false).unwrap();
        let read = reader(&path);
        let baseline = head(&read).1;
        let mut writer = store.lock().unwrap();
        for (id, tokens, accepted, delete, commit) in changes {
            let tx = writer.transaction().unwrap();
            if delete {
                tx.execute("DELETE FROM request_usage WHERE call_id=?1", [id.to_string()]).unwrap();
            } else {
                tx.execute(
                    "INSERT INTO request_usage(network,request_id,phase,call_id,input_tokens,output_tokens,
                        total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens)
                     VALUES ('shellnet','r','answer',?1,'0',?2,?2,?2,?3,'0','0')
                     ON CONFLICT(network,request_id,phase,call_id) DO UPDATE SET output_tokens=excluded.output_tokens,
                     total_tokens=excluded.total_tokens,charged_tokens=excluded.charged_tokens,accepted=excluded.accepted",
                    params![id.to_string(), tokens.to_string(), accepted],
                ).unwrap();
            }
            if commit {
                tx.commit().unwrap();
            }
        }
        let replay = events(&read, baseline, head(&read).1);
        let mut projected = BTreeMap::new();
        for event in replay.iter().chain(&replay) {
            if let Some(data) = &event.data {
                projected.insert(event.key.clone(), (
                    data["accepted"].as_i64().unwrap(),
                    data["charged_tokens"].as_str().unwrap().to_owned(),
                ));
            } else {
                projected.remove(&event.key);
            }
        }
        let actual = read.prepare(
            "SELECT json_array(network,request_id,phase,call_id),accepted,charged_tokens FROM request_usage"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))
        .unwrap().collect::<rusqlite::Result<BTreeMap<_, _>>>().unwrap();
        prop_assert_eq!(projected, actual);
    }
}
