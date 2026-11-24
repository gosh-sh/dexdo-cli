//! A read-only synchronization contract for buyer usage metadata.

use dexdo_core::params::{
    BUYER_USAGE_CHANGE_JOURNAL_PRUNE_BATCH, BUYER_USAGE_CHANGE_JOURNAL_RETAIN_EVENTS,
};
use rusqlite::{params, Result, Transaction};

struct JournalTable {
    name: &'static str,
    key: &'static [&'static str],
    fields: &'static [&'static str],
}

const JOURNAL_TABLES: &[JournalTable] = &[
    JournalTable {
        name: "requests",
        key: &["network", "request_id"],
        fields: &[
            "runtime_id",
            "model",
            "protocol",
            "streamed",
            "deal_id",
            "started_at",
            "finished_at",
            "outcome",
            "http_status",
            "finish_reason",
            "failure_stage",
            "history_complete",
        ],
    },
    JournalTable {
        name: "request_usage",
        key: &["network", "request_id", "phase", "call_id"],
        fields: &[
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "charged_tokens",
            "accepted",
            "cache_read_tokens",
            "cache_write_tokens",
            "accepted_at",
            "deal_id",
            "cumulative_billable_tokens",
            "buyer_billable_floor",
        ],
    },
    JournalTable {
        name: "deal_prices",
        key: &["network", "deal_id"],
        fields: &["price_raw"],
    },
    JournalTable {
        name: "deal_quota",
        key: &["network"],
        fields: &["deal_id", "filled_ticks", "price_raw", "closed"],
    },
];

pub(super) fn create(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE usage_change_journal (
            id INTEGER PRIMARY KEY CHECK (id=1),
            source_id BLOB NOT NULL,
            pruned_through INTEGER NOT NULL DEFAULT 0,
            pruned_event_id BLOB,
            retain_events INTEGER NOT NULL,
            prune_batch INTEGER NOT NULL
        );
        CREATE TABLE usage_changes (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id BLOB NOT NULL DEFAULT (randomblob(16)),
            table_name TEXT NOT NULL,
            operation TEXT NOT NULL,
            row_key TEXT NOT NULL,
            row_data TEXT
        );
        CREATE TRIGGER usage_journal_prune AFTER INSERT ON usage_changes
        WHEN NEW.seq >= (
            SELECT pruned_through+retain_events+prune_batch FROM usage_change_journal WHERE id=1
        )
        BEGIN
            UPDATE usage_change_journal
            SET (pruned_through, pruned_event_id) = (
                SELECT seq,event_id FROM usage_changes WHERE seq<=NEW.seq-retain_events
                ORDER BY seq DESC LIMIT 1
            ) WHERE id=1;
            DELETE FROM usage_changes
            WHERE seq<=(SELECT pruned_through FROM usage_change_journal WHERE id=1);
        END;",
    )?;
    transaction.execute(
        "INSERT INTO usage_change_journal(id,source_id,retain_events,prune_batch)
         VALUES(1,randomblob(16),?1,?2)",
        params![
            BUYER_USAGE_CHANGE_JOURNAL_RETAIN_EVENTS,
            BUYER_USAGE_CHANGE_JOURNAL_PRUNE_BATCH,
        ],
    )?;
    for table in JOURNAL_TABLES {
        let name = table.name;
        let key = |row| {
            table
                .key
                .iter()
                .map(|column| format!("{row}.{column}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let old_key = key("OLD");
        let new_key = key("NEW");
        let data = table
            .fields
            .iter()
            .map(|column| format!("'{column}',NEW.{column}"))
            .collect::<Vec<_>>()
            .join(",");
        let fields_changed = table
            .fields
            .iter()
            .map(|column| format!("OLD.{column} IS NOT NEW.{column}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let key_changed = format!("json_array({old_key}) IS NOT json_array({new_key})");
        transaction.execute_batch(&format!(
            "CREATE TRIGGER usage_journal_{name}_insert AFTER INSERT ON {name} BEGIN
                INSERT INTO usage_changes(table_name,operation,row_key,row_data)
                VALUES('{name}','insert',json_array({new_key}),json_object({data}));
            END;
            CREATE TRIGGER usage_journal_{name}_update AFTER UPDATE ON {name}
            WHEN {fields_changed} OR {key_changed} BEGIN
                INSERT INTO usage_changes(table_name,operation,row_key)
                SELECT '{name}','delete',json_array({old_key}) WHERE {key_changed};
                INSERT INTO usage_changes(table_name,operation,row_key,row_data)
                VALUES('{name}','update',json_array({new_key}),json_object({data}));
            END;
            CREATE TRIGGER usage_journal_{name}_delete AFTER DELETE ON {name} BEGIN
                INSERT INTO usage_changes(table_name,operation,row_key)
                VALUES('{name}','delete',json_array({old_key}));
            END;"
        ))?;
    }
    Ok(())
}

pub(super) fn configure_retention(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "UPDATE usage_change_journal SET retain_events=?1,prune_batch=?2
         WHERE id=1 AND (retain_events!=?1 OR prune_batch!=?2)",
        params![
            BUYER_USAGE_CHANGE_JOURNAL_RETAIN_EVENTS,
            BUYER_USAGE_CHANGE_JOURNAL_PRUNE_BATCH,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "usage_change_journal_tests.rs"]
mod tests;
