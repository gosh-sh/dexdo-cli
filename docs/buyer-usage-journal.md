# SQLite usage change journal

The buyer upgrades `usage.sqlite3` to schema 12 on open, in a single SQLite transaction.
The migration creates an empty journal and preserves existing usage rows and amounts.
Read-only clients can use a full snapshot with earlier schemas. Older buyer binaries
cannot reopen a database upgraded to schema 12.

The journal covers `requests`, `request_usage`, `deal_prices`, and `deal_quota` across
all networks. Its events commit and roll back together with their source writes.
Updates that leave all published fields unchanged produce no event.

| Table | `row_key` JSON array, in order |
| --- | --- |
| `requests` | `network, request_id` |
| `request_usage` | `network, request_id, phase, call_id` |
| `deal_prices` | `network, deal_id` |
| `deal_quota` | `network` |

Each `usage_changes` row contains `seq` (monotonic per journal), `event_id` (16-byte BLOB),
`table_name`, `operation` (`insert`, `update`, or `delete`), `row_key`, and `row_data`.
For inserts and updates, `row_data` is a JSON object containing all non-key columns of
the schema-12 table, with SQLite integers as JSON numbers and decimal token and money
amounts as strings. Nullable fields are present with JSON null values. Apply these events
as replacements by key. Deletes have SQL NULL `row_data` and remove the given key. A
primary key change produces a delete for the old key followed by an update for the new key.
The journal contains usage metadata only.

### Taking a snapshot and resuming

Open the database read-only with access to the live WAL, and use one SQLite read
transaction to read `PRAGMA user_version` and the journal state.

```sql
SELECT source_id, pruned_through, pruned_event_id,
       COALESCE((SELECT seq FROM sqlite_sequence WHERE name='usage_changes'), 0) AS high_water
FROM usage_change_journal WHERE id=1;
```

`source_id` is a 16-byte BLOB generated when the journal is created. A cursor consists
of `(source_id, seq, event_id)`. For an initial snapshot, read all four source tables
and the `event_id` at `high_water` in the same transaction. At zero the event ID is
NULL. Save the copied state and its cursor together.

For replay, validate the cursor in the same SQLite snapshot used for the batch.
The source ID must match, and the saved sequence must be between `pruned_through`
and `high_water`, inclusive. At `pruned_through`, compare the saved event ID to
`pruned_event_id`. Above it, compare to `event_id` in the journal at that sequence.
Checking the event ID catches an older backup that has since diverged and reused
a sequence. An invalid cursor, an absent anchor, or a gap in the expected sequence
range requires a fresh snapshot.

```sql
SELECT seq, event_id, table_name, operation, row_key, row_data
FROM usage_changes
WHERE seq > :cursor_seq AND seq <= :high_water
ORDER BY seq LIMIT :batch_size;
```

Apply each batch in order. Save the cursor of its last applied event atomically with
the consumer's own updates. A stopped writer's final committed position is the
watermark read after it exits, including committed changes still in the WAL.

The buyer retains at least 100,000 events once the journal reaches that size and
prunes the oldest 1,000 at a time. The journal carries its pruning boundary forward
atomically with the deletion. Freed SQLite pages can be reused by later writes.
Readers that fall behind this window can recover the current state with a fresh
snapshot. The window is measured in events across all networks and readers.
