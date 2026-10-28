//! Durable buyer-owned request usage and reconciled SHELL expenses.

//! Accepted charged work determines consumption at the actual deal price. Reconciled chain
//! facts independently retain confirmed expenses, refunds and their financial classification.

use anyhow::{anyhow, ensure, Context, Result};
use chrono::{DateTime, Days, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use dexdo_core::params::{
    BUYER_USAGE_DEFAULT_DAYS, BUYER_USAGE_DEFAULT_TIMEZONE, BUYER_USAGE_MAX_DAYS,
    BUYER_USAGE_MIN_DAYS, BUYER_USAGE_SCHEMA_VERSION, BUYER_USAGE_SQLITE_BUSY_TIMEOUT,
    BUYER_USAGE_SQLITE_INIT_RETRY_INTERVAL, SHELL_UNIT,
};
use dexdo_proto::BillingUsage;
use rusqlite::{params, Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

#[path = "usage_facts.rs"]
mod facts;
pub use facts::{UsageFact, UsageFacts, UsageFactsCursorExpired, UsageFactsQuery};

#[path = "usage_quota_journal.rs"]
mod quota_journal;

#[path = "usage_consumption.rs"]
mod consumption;

#[cfg(test)]
#[path = "usage_quota_tests.rs"]
mod quota_tests;

// Start from exceptional states, never from the completed request history. The pending
// usage lookup visits only unresolved markers, then seeks their exact request by primary key.
const UNRESOLVED_OUTPUT_USAGE_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM requests r WHERE r.network=?1 AND r.deal_id=?2
        AND r.history_complete=0)
     OR EXISTS(SELECT 1 FROM requests r WHERE r.network=?1 AND r.deal_id=?2
        AND r.finished_at IS NULL AND r.runtime_id<>?3)
     OR EXISTS(SELECT 1 FROM request_usage u WHERE u.network=?1 AND u.deal_id=?2
        AND u.call_id='in_progress' AND u.accepted=0
        AND EXISTS(SELECT 1 FROM requests r WHERE r.network=u.network
            AND r.request_id=u.request_id AND r.deal_id=?2
            AND (u.phase='answer' OR r.finished_at IS NOT NULL OR r.runtime_id<>?3)))";

#[cfg(test)]
#[path = "usage_admission_tests.rs"]
mod admission_tests;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub days: Option<u32>,
    pub timezone: Option<String>,
}

impl UsageQuery {
    /// Validation is independent from storage, so malformed HTTP queries remain client errors.
    pub fn validate(&self, now: DateTime<Utc>) -> Result<()> {
        self.bounds(now).map(|_| ())
    }

    fn bounds(&self, now: DateTime<Utc>) -> Result<Bounds> {
        self.bounds_in(now, server_timezone())
    }

    fn bounds_in(&self, now: DateTime<Utc>, server_timezone: Tz) -> Result<Bounds> {
        let default_from = now
            .checked_sub_signed(chrono::Duration::days(i64::from(BUYER_USAGE_DEFAULT_DAYS)))
            .context("usage default range overflow")?;
        let (from, to) = if self.from.is_some() || self.to.is_some() {
            // The precise interval overrides both legacy dates, even malformed legacy values.
            let parse = |value: &Option<String>| -> Result<Option<DateTime<Utc>>> {
                value
                    .as_ref()
                    .map(|text| {
                        DateTime::parse_from_rfc3339(text)
                            .map(|date| date.with_timezone(&Utc))
                            .context("usage from/to must be RFC3339 timestamps")
                    })
                    .transpose()
            };
            (
                parse(&self.from)?.unwrap_or(default_from),
                parse(&self.to)?.unwrap_or(now),
            )
        } else if self.start_date.is_some() || self.end_date.is_some() {
            let today = now.with_timezone(&server_timezone).date_naive();
            let first = today
                .checked_sub_days(Days::new(u64::from(BUYER_USAGE_DEFAULT_DAYS)))
                .context("usage default date range overflow")?;
            let parse = |value: &Option<String>, default: NaiveDate| -> Result<NaiveDate> {
                match value {
                    None => Ok(default),
                    Some(text) => {
                        let date = NaiveDate::parse_from_str(text, "%Y-%m-%d")
                            .context("usage start_date/end_date must be YYYY-MM-DD")?;
                        ensure!(
                            date.format("%Y-%m-%d").to_string() == *text,
                            "usage start_date/end_date must be YYYY-MM-DD"
                        );
                        Ok(date)
                    }
                }
            };
            let from = parse(&self.start_date, first)?;
            let end = parse(&self.end_date, today)?;
            ensure!(from <= end, "usage start_date must not follow end_date");
            let after_end = end
                .checked_add_days(Days::new(1))
                .context("usage end_date overflow")?;
            let day_start = |date: NaiveDate| -> Result<DateTime<Tz>> {
                let midnight = date
                    .and_hms_opt(0, 0, 0)
                    .context("usage day is out of range")?;
                server_timezone
                    .from_local_datetime(&midnight)
                    .earliest()
                    .or_else(|| {
                        chrono_tz::GapInfo::new(&midnight, &server_timezone).and_then(|gap| gap.end)
                    })
                    .context("usage day boundary is out of range")
            };
            let from_at = day_start(from)?;
            let end_at = day_start(end)?;
            ensure!(
                from_at.date_naive() == from && end_at.date_naive() == end,
                "usage date does not exist in the server timezone"
            );
            // A skipped following day has no instants to include; its gap end is the boundary.
            (
                from_at.with_timezone(&Utc),
                day_start(after_end)?.with_timezone(&Utc),
            )
        } else {
            (default_from, now)
        };
        ensure!(from < to, "usage from must precede to");
        let days = self.days.unwrap_or(BUYER_USAGE_DEFAULT_DAYS);
        ensure!(
            (BUYER_USAGE_MIN_DAYS..=BUYER_USAGE_MAX_DAYS).contains(&days),
            "usage days must be between {BUYER_USAGE_MIN_DAYS} and {BUYER_USAGE_MAX_DAYS}"
        );
        let timezone = self
            .timezone
            .as_ref()
            .map(|zone| zone.parse::<Tz>())
            .transpose()
            .context("usage timezone must be an IANA timezone")?
            .unwrap_or(server_timezone);
        Ok(Bounds {
            from: Some(from),
            to: Some(to),
            days,
            timezone,
            server_timezone,
        })
    }
}

fn server_timezone() -> Tz {
    select_server_timezone(std::env::var("TZ").ok().as_deref(), || {
        iana_time_zone::get_timezone().ok()
    })
}

fn select_server_timezone(
    environment: Option<&str>,
    os_timezone: impl FnOnce() -> Option<String>,
) -> Tz {
    let parse = |zone: &str| zone.trim_start_matches(':').parse::<Tz>().ok();
    environment
        .and_then(parse)
        .or_else(|| os_timezone().as_deref().and_then(parse))
        .unwrap_or_else(|| {
            BUYER_USAGE_DEFAULT_TIMEZONE
                .parse()
                .expect("canonical usage fallback timezone")
        })
}

struct Bounds {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    days: u32,
    timezone: Tz,
    server_timezone: Tz,
}

impl Bounds {
    fn contains(&self, at: i64) -> bool {
        DateTime::from_timestamp_millis(at).is_some_and(|at| {
            self.from.is_none_or(|from| at >= from) && self.to.is_none_or(|to| at < to)
        })
    }
}

/// A terminal settlement proven against this buyer's contributions and complete buyer credits.
/// `buyer_credit_raw = None` records a closed but unreconciled deal, never a zero refund.
#[derive(Debug, Clone)]
pub struct SettlementFact {
    pub source_id: String,
    pub deal_id: String,
    /// Original chain timestamp, Unix seconds in UTC (not reconciliation time).
    pub occurred_at: i64,
    pub escrow_raw: u128,
    pub buyer_bond_raw: u128,
    pub buyer_credit_raw: Option<u128>,
}

/// Net SHELL expense proven paid by the buyer, after bounce/refund reconciliation.
/// Funding one's own note/reserve is a transfer, not an operational expense.
#[derive(Debug, Clone)]
pub struct OperationalFact {
    pub source_id: String,
    pub deal_id: Option<String>,
    pub model: Option<String>,
    pub occurred_at: i64,
    pub spent_raw: u128,
}

/// Irrevocable buyer expense announced by a deal before its terminal reconciliation.
#[derive(Debug, Clone)]
pub struct AccrualFact {
    pub source_id: String,
    /// Original chain timestamp, Unix seconds in UTC.
    pub occurred_at: i64,
    pub spent_raw: u128,
}

/// Proven components of a financial fact. The unclassified remainder stays `other`,
/// never an estimate of model work. Updating these does not change the exported fact amount.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExpenseBreakdown {
    pub consumption_raw: u128,
    pub commission_raw: u128,
    pub burns_raw: u128,
    pub unused_capacity_raw: u128,
    /// Accepted charged tokens covered by this fact's one paid probe tick.
    /// None denotes a fixed classification without a growing consumption proof.
    pub probe_consumption_tokens: Option<u128>,
}

impl ExpenseBreakdown {
    fn total(self) -> Result<u128> {
        self.consumption_raw
            .checked_add(self.commission_raw)
            .and_then(|sum| sum.checked_add(self.burns_raw))
            .and_then(|sum| sum.checked_add(self.unused_capacity_raw))
            .context("expense breakdown overflow")
    }
}

#[derive(Clone)]
pub struct UsageStore {
    connection: Arc<Mutex<Connection>>,
    history_connection: Arc<Mutex<Connection>>,
    network: Arc<str>,
    runtime_id: Arc<str>,
    quota_journal: Arc<PathBuf>,
}

impl std::fmt::Debug for UsageStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsageStore")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl UsageStore {
    pub fn open(path: impl AsRef<Path>, network: impl Into<String>) -> Result<Self> {
        let network = network.into();
        ensure!(!network.is_empty(), "usage network is empty");
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let parent_existed = parent.exists();
            std::fs::create_dir_all(parent).context("create buyer usage directory")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if !parent_existed {
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                        .context("make buyer usage directory private")?;
                }
            }
            #[cfg(not(unix))]
            let _ = parent_existed;
        }
        // Precreate with owner-only permissions before SQLite can create WAL/SHM siblings.
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path).context("create buyer usage database")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let mut connection = Connection::open(path).context("open buyer usage database")?;
        // Concurrent first opens can return SQLITE_BUSY from the WAL transition without
        // invoking SQLite's busy handler. Retry only that refusal within the same wait budget.
        let started = std::time::Instant::now();
        loop {
            connection
                .busy_timeout(BUYER_USAGE_SQLITE_BUSY_TIMEOUT.saturating_sub(started.elapsed()))?;
            match connection.pragma_update(None, "journal_mode", "WAL") {
                Ok(()) => break,
                Err(error)
                    if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                        && started.elapsed() < BUYER_USAGE_SQLITE_BUSY_TIMEOUT =>
                {
                    std::thread::sleep(
                        BUYER_USAGE_SQLITE_INIT_RETRY_INTERVAL
                            .min(BUYER_USAGE_SQLITE_BUSY_TIMEOUT.saturating_sub(started.elapsed())),
                    );
                }
                Err(error) => return Err(error).context("enable buyer usage WAL"),
            }
        }
        connection.busy_timeout(BUYER_USAGE_SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        // Reserve the schema writer before reading it: a deferred read-to-write upgrade can
        // fail immediately while another connection is writing, bypassing the busy timeout.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: u32 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            matches!(version, 0 | 3..=5) || version == BUYER_USAGE_SCHEMA_VERSION,
            "unsupported buyer usage schema version {version}"
        );
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                network TEXT NOT NULL, request_id TEXT NOT NULL, runtime_id TEXT NOT NULL,
                model TEXT NOT NULL, protocol TEXT NOT NULL, streamed INTEGER NOT NULL,
                deal_id TEXT, started_at INTEGER NOT NULL, finished_at INTEGER,
                outcome TEXT NOT NULL DEFAULT 'in_progress', http_status INTEGER,
                finish_reason TEXT, failure_stage TEXT, history_complete INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY(network, request_id)
            );
            CREATE TABLE IF NOT EXISTS request_usage (
                network TEXT NOT NULL, request_id TEXT NOT NULL, phase TEXT NOT NULL,
                call_id TEXT NOT NULL, input_tokens TEXT NOT NULL, output_tokens TEXT NOT NULL,
                total_tokens TEXT NOT NULL, charged_tokens TEXT NOT NULL, accepted INTEGER NOT NULL,
                cache_read_tokens TEXT NOT NULL, cache_write_tokens TEXT NOT NULL,
                accepted_at INTEGER, deal_id TEXT,
                PRIMARY KEY(network, request_id, phase, call_id),
                FOREIGN KEY(network, request_id) REFERENCES requests(network, request_id)
            );
            CREATE TABLE IF NOT EXISTS deals (
                network TEXT NOT NULL, deal_id TEXT NOT NULL, model TEXT NOT NULL,
                registered_at INTEGER NOT NULL, closed_at INTEGER,
                status TEXT NOT NULL DEFAULT 'pending', incomplete_reason TEXT,
                operations_complete INTEGER NOT NULL DEFAULT 0, operations_reason TEXT,
                PRIMARY KEY(network, deal_id)
            );
            CREATE TABLE IF NOT EXISTS money_facts (
                network TEXT NOT NULL, source_id TEXT NOT NULL, kind TEXT NOT NULL,
                deal_id TEXT, model TEXT, occurred_at INTEGER NOT NULL, recorded_at INTEGER NOT NULL,
                escrow_raw TEXT, buyer_bond_raw TEXT, buyer_credit_raw TEXT, spent_raw TEXT,
                consumption_raw TEXT NOT NULL DEFAULT '0', commission_raw TEXT NOT NULL DEFAULT '0',
                burns_raw TEXT NOT NULL DEFAULT '0', unused_capacity_raw TEXT NOT NULL DEFAULT '0',
                probe_consumption_tokens TEXT,
                PRIMARY KEY(network, source_id)
            );
            CREATE TABLE IF NOT EXISTS deal_quota (
                network TEXT PRIMARY KEY, deal_id TEXT NOT NULL,
                filled_ticks TEXT NOT NULL, price_raw TEXT NOT NULL,
                closed INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS deal_prices (
                network TEXT NOT NULL, deal_id TEXT NOT NULL, price_raw TEXT NOT NULL,
                PRIMARY KEY(network, deal_id)
            );
            CREATE TABLE IF NOT EXISTS note_operations (
                network TEXT NOT NULL,note_id TEXT NOT NULL,complete INTEGER NOT NULL,
                reason TEXT,registered_at INTEGER NOT NULL,
                PRIMARY KEY(network,note_id)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS one_settlement_per_deal
                ON money_facts(network, deal_id) WHERE kind = 'settlement';
            CREATE INDEX IF NOT EXISTS requests_finished
                ON requests(network, finished_at);
            CREATE INDEX IF NOT EXISTS requests_incomplete_usage
                ON requests(network, deal_id) WHERE history_complete=0;
            CREATE INDEX IF NOT EXISTS requests_unfinished_usage
                ON requests(network, deal_id, runtime_id) WHERE finished_at IS NULL;
            CREATE INDEX IF NOT EXISTS facts_occurred
                ON money_facts(network, occurred_at);
            CREATE INDEX IF NOT EXISTS facts_page_order
                ON money_facts(network, occurred_at, source_id)
                WHERE kind!='settlement' OR (spent_raw IS NOT NULL AND buyer_credit_raw IS NOT NULL);
            CREATE INDEX IF NOT EXISTS facts_pending_settlement
                ON money_facts(network) WHERE kind='settlement' AND spent_raw IS NULL;
            CREATE INDEX IF NOT EXISTS deals_pending_usage
                ON deals(network) WHERE status!='settled' OR operations_complete=0;
            CREATE INDEX IF NOT EXISTS note_operations_pending_usage
                ON note_operations(network) WHERE complete=0;
            CREATE TABLE IF NOT EXISTS facts_revision (
                network TEXT PRIMARY KEY, store_id TEXT NOT NULL, revision INTEGER NOT NULL
            );
            CREATE TRIGGER IF NOT EXISTS facts_insert_revision AFTER INSERT ON money_facts BEGIN
                UPDATE facts_revision SET revision=revision+1 WHERE network=NEW.network;
            END;
            CREATE TRIGGER IF NOT EXISTS facts_update_revision AFTER UPDATE ON money_facts BEGIN
                UPDATE facts_revision SET revision=revision+1 WHERE network=NEW.network;
            END;
            CREATE TRIGGER IF NOT EXISTS facts_delete_revision AFTER DELETE ON money_facts BEGIN
                UPDATE facts_revision SET revision=revision+1 WHERE network=OLD.network;
            END;",
        )?;
        if version == 3 {
            transaction.execute_batch(
                "ALTER TABLE money_facts ADD COLUMN probe_consumption_tokens TEXT;",
            )?;
        }
        if matches!(version, 3 | 4) {
            transaction.execute_batch(
                "ALTER TABLE request_usage ADD COLUMN accepted_at INTEGER;
                 UPDATE request_usage SET accepted_at=(SELECT COALESCE(r.finished_at,r.started_at)
                    FROM requests r WHERE r.network=request_usage.network
                    AND r.request_id=request_usage.request_id) WHERE accepted=1;",
            )?;
        }
        if matches!(version, 3..=5) {
            transaction.execute_batch(
                "ALTER TABLE request_usage ADD COLUMN deal_id TEXT;
                 UPDATE request_usage SET deal_id=(SELECT r.deal_id FROM requests r
                    WHERE r.network=request_usage.network AND r.request_id=request_usage.request_id);",
            )?;
        }
        // Keep the request's immutable deal binding on its usage rows so the pending index
        // can seek one deal without walking unresolved history from the rest of the network.
        transaction.execute_batch(
            "CREATE INDEX IF NOT EXISTS request_usage_pending
                ON request_usage(network, deal_id, request_id, phase)
                WHERE call_id='in_progress' AND accepted=0;",
        )?;
        // Quota terms and a verified one-tick probe payment both prove the execution price.
        // Keep every recovered price after the current purchase is replaced or destroyed.
        let prices = {
            let mut statement = transaction.prepare(
                "SELECT network,deal_id,price_raw FROM deal_quota
                 UNION ALL SELECT network,deal_id,spent_raw FROM money_facts
                 WHERE probe_consumption_tokens IS NOT NULL AND kind='accrual'",
            )?;
            let prices = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            prices
        };
        for (network, deal, price) in prices {
            Self::record_deal_price_in(&transaction, &network, &deal, parse_u128(price)?)?;
        }
        transaction.execute(
            "INSERT OR IGNORE INTO facts_revision(network,store_id,revision) VALUES(?1,lower(hex(randomblob(16))),0)",
            [&network],
        )?;
        transaction.pragma_update(None, "user_version", BUYER_USAGE_SCHEMA_VERSION)?;
        transaction.commit()?;
        let quota_journal = path.with_extension(format!(
            "{}.{}",
            hex::encode(network.as_bytes()),
            dexdo_core::params::BUYER_USAGE_QUOTA_JOURNAL_EXTENSION
        ));
        // WAL lets history snapshots run without holding the request accounting mutex.
        let history_connection =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("open buyer usage history reader")?;
        history_connection.busy_timeout(BUYER_USAGE_SQLITE_BUSY_TIMEOUT)?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            history_connection: Arc::new(Mutex::new(history_connection)),
            network: network.into(),
            runtime_id: format!("{:032x}", rand::random::<u128>()).into(),
            quota_journal: Arc::new(quota_journal),
        };
        // A failed SQLite write must not lose the last opened purchase after TC destruction.
        // Unknown terms stay unavailable until the same serving deal supplies them.
        {
            let mut connection = store.lock()?;
            store.restore_quota(&mut connection, false)?;
        }
        Ok(store)
    }

    pub fn network(&self) -> &str {
        &self.network
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("buyer usage database lock is poisoned"))
    }

    fn lock_history(&self) -> Result<MutexGuard<'_, Connection>> {
        self.history_connection
            .lock()
            .map_err(|_| anyhow!("buyer usage history lock is poisoned"))
    }

    pub fn start_request(
        &self,
        request_id: &str,
        model: &str,
        protocol: &str,
        streamed: bool,
    ) -> Result<()> {
        self.start_request_at(
            request_id,
            model,
            protocol,
            streamed,
            Utc::now().timestamp_millis(),
        )
    }

    pub fn start_request_at(
        &self,
        request_id: &str,
        model: &str,
        protocol: &str,
        streamed: bool,
        at_ms: i64,
    ) -> Result<()> {
        ensure!(!request_id.is_empty(), "usage request ID is empty");
        timestamp(at_ms)?;
        let connection = self.lock()?;
        let old = connection
            .query_row(
                "SELECT model,protocol,streamed FROM requests WHERE network=?1 AND request_id=?2",
                params![self.network(), request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(
                old.0 == model && old.1 == protocol && old.2 == streamed,
                "conflicting replay of buyer request start"
            );
            return Ok(());
        }
        connection.execute(
            "INSERT INTO requests(network,request_id,runtime_id,model,protocol,streamed,started_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![self.network(), request_id, &*self.runtime_id, model, protocol, streamed, at_ms],
        ).context("record buyer usage request start")?;
        Ok(())
    }

    pub fn link_deal(&self, request_id: &str, deal_id: &str) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<Option<String>> = transaction
            .query_row(
                "SELECT deal_id FROM requests WHERE network=?1 AND request_id=?2",
                params![self.network(), request_id],
                |row| row.get(0),
            )
            .optional()?;
        let previous = previous.context("usage request not found")?;
        ensure!(
            previous.as_deref().is_none_or(|id| id == deal_id),
            "usage request deal changed"
        );
        transaction.execute(
            "UPDATE requests SET deal_id=?3 WHERE network=?1 AND request_id=?2",
            params![self.network(), request_id, deal_id],
        )?;
        transaction.execute(
            "UPDATE request_usage SET deal_id=?3 WHERE network=?1 AND request_id=?2",
            params![self.network(), request_id, deal_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_request_streamed(&self, request_id: &str, streamed: bool) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE requests SET streamed=?3 WHERE network=?1 AND request_id=?2",
            params![self.network(), request_id, streamed],
        )?;
        ensure!(changed == 1, "usage request not found");
        Ok(())
    }

    pub fn record_call_usage(
        &self,
        request_id: &str,
        phase: &str,
        call_id: &str,
        usage: BillingUsage,
        accepted: bool,
    ) -> Result<()> {
        ensure!(
            matches!(phase, "answer" | "verification_b8" | "verification_b7"),
            "unknown usage phase"
        );
        ensure!(!call_id.is_empty(), "usage call ID is empty");
        usage.validate().map_err(|error| anyhow!(error))?;
        let connection = self.lock()?;
        let old = connection.query_row(
            "SELECT input_tokens,output_tokens,total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens FROM request_usage
             WHERE network=?1 AND request_id=?2 AND phase=?3 AND call_id=?4",
            params![self.network(), request_id, phase, call_id],
            |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?,row.get::<_, String>(3)?,row.get::<_, bool>(4)?,row.get::<_,String>(5)?,row.get::<_,String>(6)?)),
        ).optional()?;
        let input = usage.input_tokens.to_string();
        let output = usage.output_tokens.to_string();
        let total = usage.total_tokens.to_string();
        let charged = usage.billable_tokens().to_string();
        let cache_read = usage.cache_read_tokens.to_string();
        let cache_write = usage.cache_write_tokens.to_string();
        if let Some(old) = old {
            ensure!(
                old.0 == input
                    && old.1 == output
                    && old.2 == total
                    && old.3 == charged
                    && old.5 == cache_read
                    && old.6 == cache_write,
                "conflicting replay of buyer usage call"
            );
            if accepted && !old.4 {
                connection.execute(
                    "UPDATE request_usage SET accepted=1,accepted_at=?5 WHERE network=?1 AND request_id=?2 AND phase=?3 AND call_id=?4",
                    params![self.network(), request_id, phase, call_id, Utc::now().timestamp_millis()],
                )?;
            }
        } else {
            connection.execute(
                "INSERT INTO request_usage(network,request_id,phase,call_id,input_tokens,output_tokens,total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens,accepted_at,deal_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,
                    (SELECT deal_id FROM requests WHERE network=?1 AND request_id=?2))",
                params![self.network(), request_id, phase, call_id, input, output, total, charged, accepted,cache_read,cache_write,
                    accepted.then(|| Utc::now().timestamp_millis())],
            )?;
        }
        if accepted && phase == call_id {
            connection.execute(
                "UPDATE request_usage SET accepted=1 WHERE network=?1 AND request_id=?2
                 AND phase=?3 AND call_id='in_progress'",
                params![self.network(), request_id, phase],
            )?;
        }
        Ok(())
    }

    /// Unresolved output survives request completion and process restart. Only validated usage
    /// for that exact request and phase can finish its state row; an open chain read cannot.
    /// Missing state writes fall back to incomplete history or an older runtime's unfinished request.
    /// A live verification call's initial state row alone does not pause concurrent waiters.
    pub(crate) fn has_unresolved_output_usage(&self, deal_id: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .prepare_cached(UNRESOLVED_OUTPUT_USAGE_SQL)?
            .query_row(params![self.network(), deal_id, &*self.runtime_id], |row| {
                row.get(0)
            })?)
    }

    /// Persist a write failure before the request is completed, so a missing paid phase cannot
    /// later appear as a confirmed zero. A failed marker leaves the request unfinished instead.
    pub fn mark_request_history_incomplete(&self, request_id: &str) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE requests SET history_complete=0 WHERE network=?1 AND request_id=?2",
            params![self.network(), request_id],
        )?;
        ensure!(changed == 1, "usage request not found");
        Ok(())
    }

    pub fn finish_request(
        &self,
        request_id: &str,
        http_status: u16,
        finish_reason: Option<&str>,
        failure_stage: Option<&str>,
    ) -> Result<()> {
        self.finish_request_at(
            request_id,
            http_status,
            finish_reason,
            failure_stage,
            Utc::now().timestamp_millis(),
        )
    }

    pub fn finish_request_at(
        &self,
        request_id: &str,
        http_status: u16,
        finish_reason: Option<&str>,
        failure_stage: Option<&str>,
        at_ms: i64,
    ) -> Result<()> {
        timestamp(at_ms)?;
        let outcome = if http_status == 499 || finish_reason == Some("cancelled") {
            "cancelled"
        } else if http_status >= 500 {
            "failed"
        } else if http_status >= 400 {
            "rejected"
        } else if failure_stage.is_some() || matches!(finish_reason, Some("error" | "empty_output"))
        {
            "failed"
        } else {
            "succeeded"
        };
        let connection = self.lock()?;
        let existing = connection.query_row(
            "SELECT started_at,finished_at,http_status,finish_reason,failure_stage FROM requests WHERE network=?1 AND request_id=?2",
            params![self.network(), request_id],
            |row| Ok((row.get::<_,i64>(0)?,row.get::<_,Option<i64>>(1)?,row.get::<_,Option<u16>>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?)),
        ).optional()?.context("usage request not found")?;
        ensure!(
            at_ms >= existing.0,
            "usage request finished before it started"
        );
        if existing.1.is_some() {
            ensure!(
                existing.2 == Some(http_status)
                    && existing.3.as_deref() == finish_reason
                    && existing.4.as_deref() == failure_stage,
                "conflicting replay of buyer request completion"
            );
            return Ok(());
        }
        connection.execute(
            "UPDATE requests SET finished_at=?3,outcome=?4,http_status=?5,finish_reason=?6,failure_stage=?7
             WHERE network=?1 AND request_id=?2",
            params![self.network(), request_id,at_ms,outcome,http_status,finish_reason,failure_stage],
        )?;
        Ok(())
    }

    /// Call only after obtaining the buyer's exclusive instance lock. An unfinished request has
    /// no known completion time, so recovery preserves NULL rather than moving it into today.
    pub fn recover_interrupted_requests(&self) -> Result<usize> {
        Ok(self.lock()?.execute(
            "UPDATE requests SET outcome='interrupted',failure_stage='process_interrupted'
             WHERE network=?1 AND outcome='in_progress' AND runtime_id<>?2",
            params![self.network(), &*self.runtime_id],
        )?)
    }

    pub fn register_deal(&self, deal_id: &str, model: &str) -> Result<()> {
        let connection = self.lock()?;
        let old: Option<String> = connection
            .query_row(
                "SELECT model FROM deals WHERE network=?1 AND deal_id=?2",
                params![self.network(), deal_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == model, "usage deal model changed");
        } else {
            connection.execute(
                "INSERT INTO deals(network,deal_id,model,registered_at) VALUES(?1,?2,?3,?4)",
                params![
                    self.network(),
                    deal_id,
                    model,
                    Utc::now().timestamp_millis()
                ],
            )?;
        }
        Ok(())
    }

    pub fn mark_deal_incomplete(&self, deal_id: &str, reason: &str) -> Result<()> {
        ensure!(!reason.is_empty(), "missing incomplete-deal reason");
        let changed = self.lock()?.execute(
            "UPDATE deals SET status='incomplete',closed_at=NULL,incomplete_reason=?3
             WHERE network=?1 AND deal_id=?2",
            params![self.network(), deal_id, reason],
        )?;
        ensure!(changed == 1, "usage deal not found");
        Ok(())
    }

    /// Publish only an actually opened serving deal. Rebinding the same deal on resume is
    /// idempotent: neither its expenses nor a previously recorded STOP are reset.
    pub fn open_deal_quota(
        &self,
        deal_id: &str,
        filled_ticks: u128,
        price_raw: u128,
    ) -> Result<()> {
        ensure!(filled_ticks > 0 && price_raw > 0, "empty purchased quota");
        filled_ticks
            .checked_mul(price_raw)
            .context("purchased quota overflow")?;
        let mut connection = self.lock()?;
        let previous = match self.read_quota_journal()? {
            Some(record) => Some(record),
            None => self.quota_row(&connection)?,
        };
        let mut record = quota_journal::QuotaJournal {
            deal_id: deal_id.to_owned(),
            filled_ticks: Some(filled_ticks.to_string()),
            price_raw: Some(price_raw.to_string()),
            closed: false,
            pending_deals: Vec::new(),
        };
        if let Some(previous) = previous.filter(|row| row.deal_id == deal_id) {
            ensure!(
                previous
                    .filled_ticks
                    .as_ref()
                    .is_none_or(|ticks| *ticks == filled_ticks.to_string())
                    && previous
                        .price_raw
                        .as_ref()
                        .is_none_or(|price| *price == price_raw.to_string()),
                "purchased quota terms changed for {deal_id}"
            );
            record.closed = previous.closed;
        }
        // Durable BEFORE SQLite: the contract may disappear while that database is busy.
        self.write_quota_journal(&record)?;
        self.restore_quota(&mut connection, true)
    }

    pub fn has_deal_quota(&self, deal_id: &str) -> Result<bool> {
        let connection = self.lock()?;
        if let Some(record) = self.read_quota_journal()? {
            return Ok(record.pending_deals.is_empty()
                && record.deal_id == deal_id
                && record.filled_ticks.is_some()
                && record.price_raw.is_some());
        }
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM deal_quota WHERE network=?1 AND deal_id=?2)",
            params![self.network(), deal_id],
            |row| row.get(0),
        )?)
    }

    pub fn close_deal_quota(&self, deal_id: &str) -> Result<()> {
        let connection = self.lock()?;
        let previous = match self.read_quota_journal()? {
            Some(record) => Some(record),
            None => self.quota_row(&connection)?,
        };
        if let Some(mut record) = previous.filter(|record| record.deal_id == deal_id) {
            if !record.closed {
                record.closed = true;
                self.write_quota_journal(&record)?;
            }
        }
        connection.execute(
            "UPDATE deal_quota SET closed=1 WHERE network=?1 AND deal_id=?2",
            params![self.network(), deal_id],
        )?;
        Ok(())
    }

    fn quota_in(&self, transaction: &rusqlite::Transaction<'_>) -> Result<Value> {
        let quota: Option<(String, String, String, bool)> = transaction
            .query_row(
                "SELECT deal_id,filled_ticks,price_raw,closed FROM deal_quota WHERE network=?1",
                params![self.network()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let mut limit = 0u128;
        let mut used = 0u128;
        let mut closed = true;
        if let Some((id, ticks, price, is_closed)) = quota {
            let price = parse_u128(price)?;
            limit = parse_u128(ticks)?
                .checked_mul(price)
                .context("purchased quota overflow")?;
            closed = is_closed;
            used = self.consumed_cost_in(transaction, &id, price)?;
        }
        let remaining = if closed {
            0
        } else {
            limit.saturating_sub(used)
        };
        Ok(
            json!({"limit":shell(limit),"used":shell(used),"remaining":shell(remaining),"unit":"SHELL"}),
        )
    }

    pub fn mark_operations_incomplete(&self, deal_id: &str, reason: &str) -> Result<()> {
        ensure!(!reason.is_empty(), "missing incomplete-operations reason");
        let changed = self.lock()?.execute(
            "UPDATE deals SET operations_complete=0,operations_reason=?3 WHERE network=?1 AND deal_id=?2",
            params![self.network(),deal_id,reason],
        )?;
        ensure!(changed == 1, "usage deal not found");
        Ok(())
    }

    pub fn mark_operations_complete(&self, deal_id: &str) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE deals SET operations_complete=1,operations_reason=NULL WHERE network=?1 AND deal_id=?2",
            params![self.network(),deal_id],
        )?;
        ensure!(changed == 1, "usage deal not found");
        Ok(())
    }

    pub fn mark_note_operations_incomplete(&self, note_id: &str, reason: &str) -> Result<()> {
        ensure!(
            !note_id.is_empty() && !reason.is_empty(),
            "missing note operations evidence identity/reason"
        );
        self.lock()?.execute(
            "INSERT INTO note_operations(network,note_id,complete,reason,registered_at) VALUES(?1,?2,0,?3,?4)
             ON CONFLICT(network,note_id) DO UPDATE SET complete=0,reason=excluded.reason",
            params![self.network(),note_id,reason,Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn mark_note_operations_complete(&self, note_id: &str) -> Result<()> {
        ensure!(
            !note_id.is_empty(),
            "missing note operations evidence identity"
        );
        self.lock()?.execute(
            "INSERT INTO note_operations(network,note_id,complete,registered_at) VALUES(?1,?2,1,?3)
             ON CONFLICT(network,note_id) DO UPDATE SET complete=1,reason=NULL",
            params![self.network(), note_id, Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn apply_settlement(&self, fact: SettlementFact) -> Result<()> {
        let deal_id = fact.deal_id.clone();
        self.apply_deal_expenses(&deal_id, &[], Some(fact))
    }

    /// Completed accepted work may consume already paid capacity after its payment event.
    /// HTTP failures retain accepted work; unfinished and merely observed work are excluded.
    pub fn accepted_tokens(&self, deal_id: &str) -> Result<u128> {
        let connection = self.lock_history()?;
        let mut statement = connection.prepare(
            "SELECT u.charged_tokens FROM request_usage u JOIN requests r
             ON u.network=r.network AND u.request_id=r.request_id
             WHERE u.network=?1 AND r.deal_id=?2 AND u.accepted=1 AND r.finished_at IS NOT NULL",
        )?;
        let mut total = 0_u128;
        for tokens in statement.query_map(params![self.network(), deal_id], |row| {
            row.get::<_, String>(0)
        })? {
            total = total
                .checked_add(parse_u128(tokens?)?)
                .context("accepted consumption overflow")?;
        }
        Ok(total)
    }

    pub fn classify_expense(&self, source_id: &str, parts: ExpenseBreakdown) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT kind,escrow_raw,buyer_bond_raw,buyer_credit_raw,spent_raw,probe_consumption_tokens FROM money_facts
             WHERE network=?1 AND source_id=?2 AND kind!='operational'",
        )?;
        let mut rows = statement.query(params![self.network(), source_id])?;
        let row = rows.next()?.context("financial fact not found")?;
        let is_accrual = row.get::<_, String>("kind")? == "accrual";
        let previous_tokens: Option<String> = row.get("probe_consumption_tokens")?;
        let Some(amount) = fact_amount(row)? else {
            return Ok(());
        };
        drop(rows);
        drop(statement);
        ensure!(
            parts.total()? <= amount,
            "expense breakdown exceeds confirmed expense"
        );
        if let Some(tokens) = parts.probe_consumption_tokens {
            ensure!(
                is_accrual
                    && amount > 0
                    && tokens <= dexdo_core::params::TICK_SIZE
                    && parts.consumption_raw == paid_probe_cost(amount, tokens)
                    && parts.commission_raw == 0
                    && parts.burns_raw == 0
                    && parts.unused_capacity_raw == 0,
                "invalid paid probe consumption proof"
            );
        }
        if let Some(previous) = previous_tokens {
            let previous = parse_u128(previous)?;
            ensure!(
                parts
                    .probe_consumption_tokens
                    .is_some_and(|tokens| tokens >= previous),
                "paid probe consumption proof cannot decrease or disappear"
            );
        }
        let probe_tokens = parts
            .probe_consumption_tokens
            .map(|tokens| tokens.to_string());
        transaction.execute(
            "UPDATE money_facts SET consumption_raw=?3,commission_raw=?4,burns_raw=?5,unused_capacity_raw=?6,probe_consumption_tokens=?7
             WHERE network=?1 AND source_id=?2 AND (consumption_raw!=?3 OR commission_raw!=?4 OR burns_raw!=?5
                OR unused_capacity_raw!=?6 OR probe_consumption_tokens IS NOT ?7)",
            params![self.network(),source_id,parts.consumption_raw.to_string(),parts.commission_raw.to_string(),
                parts.burns_raw.to_string(),parts.unused_capacity_raw.to_string(),probe_tokens])?;
        transaction.commit()?;
        Ok(())
    }

    /// Publish accruals and the terminal residual together. Re-reading a closed deal must rebuild
    /// the same partition even when this buyer never observed it while it was open.
    pub fn apply_deal_expenses(
        &self,
        deal_id: &str,
        accruals: &[AccrualFact],
        settlement: Option<SettlementFact>,
    ) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let model: String = transaction
            .query_row(
                "SELECT model FROM deals WHERE network=?1 AND deal_id=?2",
                params![self.network(), deal_id],
                |row| row.get(0),
            )
            .optional()?
            .context("usage deal not registered")?;
        let prior_terminal: Option<Option<String>> = transaction.query_row(
            "SELECT spent_raw FROM money_facts WHERE network=?1 AND deal_id=?2 AND kind='settlement' AND buyer_credit_raw IS NOT NULL",
            params![self.network(),deal_id], |row| row.get(0),
        ).optional()?;
        ensure!(
            prior_terminal != Some(None)
                || accruals.is_empty()
                || settlement
                    .as_ref()
                    .is_some_and(|fact| fact.buyer_credit_raw.is_some()),
            "existing terminal expense needs an atomic accrual/residual partition"
        );
        for fact in accruals {
            ensure!(!fact.source_id.is_empty(), "accrual source ID is empty");
            ensure!(fact.spent_raw > 0, "accrual expense is zero");
            let at = chain_millis(fact.occurred_at)?;
            let spent = fact.spent_raw.to_string();
            let old = transaction.query_row(
                "SELECT kind,deal_id,occurred_at,spent_raw FROM money_facts WHERE network=?1 AND source_id=?2",
                params![self.network(),fact.source_id],
                |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<String>>(3)?)),
            ).optional()?;
            if let Some(old) = old {
                ensure!(
                    old.0 == "accrual"
                        && old.1.as_deref() == Some(deal_id)
                        && old.2 == at
                        && old.3.as_ref() == Some(&spent),
                    "conflicting replay of accrual fact"
                );
            } else {
                ensure!(
                    !matches!(prior_terminal, Some(Some(_))),
                    "cannot add an accrual after confirmed terminal partition"
                );
                transaction.execute(
                    "INSERT INTO money_facts(network,source_id,kind,deal_id,model,occurred_at,spent_raw,recorded_at)
                     VALUES(?1,?2,'accrual',?3,?4,?5,?6,?7)",
                    params![self.network(),fact.source_id,deal_id,model,at,spent,Utc::now().timestamp_millis()],
                )?;
            }
        }
        let terminal = settlement.is_some();
        if let Some(fact) = settlement {
            ensure!(
                fact.deal_id == deal_id,
                "settlement belongs to a different deal"
            );
            Self::apply_settlement_in(&transaction, self.network(), &model, fact)?;
        }
        transaction.commit()?;
        drop(connection);
        if terminal {
            self.close_deal_quota(deal_id)?;
        }
        Ok(())
    }

    fn apply_settlement_in(
        transaction: &rusqlite::Transaction<'_>,
        network: &str,
        model: &str,
        fact: SettlementFact,
    ) -> Result<()> {
        ensure!(!fact.source_id.is_empty(), "settlement source ID is empty");
        let at = chain_millis(fact.occurred_at)?;
        let contributions = fact
            .escrow_raw
            .checked_add(fact.buyer_bond_raw)
            .context("buyer contributions overflow")?;
        if let Some(credit) = fact.buyer_credit_raw {
            ensure!(
                credit <= contributions,
                "buyer credits exceed buyer contributions"
            );
        }
        let mut accrued = 0u128;
        let mut statement = transaction.prepare(
            "SELECT spent_raw FROM money_facts WHERE network=?1 AND deal_id=?2 AND kind='accrual'",
        )?;
        for spent in statement.query_map(params![network, fact.deal_id], |row| {
            row.get::<_, String>(0)
        })? {
            accrued = accrued
                .checked_add(parse_u128(spent?)?)
                .context("accrual total overflow")?;
        }
        ensure!(
            accrued <= contributions,
            "accruals exceed buyer contributions"
        );
        let residual = fact
            .buyer_credit_raw
            .map(|credit| {
                contributions
                    .checked_sub(credit)
                    .and_then(|net| net.checked_sub(accrued))
                    .context("accruals exceed confirmed settlement expense")
            })
            .transpose()?
            .map(|amount| amount.to_string());
        // Reuse spent_raw for the terminal residual. Contribution/credit evidence remains intact;
        // the exported terminal amount must never include the separately exported accruals again.
        let old = transaction.query_row(
            "SELECT kind,deal_id,occurred_at,escrow_raw,buyer_bond_raw,buyer_credit_raw,spent_raw FROM money_facts
             WHERE network=?1 AND source_id=?2",
            params![network,fact.source_id],
            |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,Option<String>>(5)?,row.get::<_,Option<String>>(6)?)),
        ).optional()?;
        let escrow = fact.escrow_raw.to_string();
        let bond = fact.buyer_bond_raw.to_string();
        let credit = fact.buyer_credit_raw.map(|value| value.to_string());
        if let Some(old) = old {
            ensure!(
                old.0 == "settlement"
                    && old.1.as_deref() == Some(&fact.deal_id)
                    && old.2 == at
                    && old.3.as_ref() == Some(&escrow)
                    && old.4.as_ref() == Some(&bond)
                    && old
                        .5
                        .as_ref()
                        .is_none_or(|old_credit| Some(old_credit) == credit.as_ref()),
                "conflicting replay of settlement fact"
            );
            ensure!(
                old.6.is_none() || old.6 == residual,
                "conflicting terminal residual"
            );
            if old.6.is_none() && credit.is_some() {
                transaction.execute(
                    "UPDATE money_facts SET buyer_credit_raw=?3,spent_raw=?4,recorded_at=?5 WHERE network=?1 AND source_id=?2",
                    params![network,fact.source_id,credit,residual,Utc::now().timestamp_millis()],
                )?;
            }
        } else {
            transaction.execute(
                "INSERT INTO money_facts(network,source_id,kind,deal_id,model,occurred_at,escrow_raw,buyer_bond_raw,buyer_credit_raw,spent_raw,recorded_at)
                 VALUES(?1,?2,'settlement',?3,?4,?5,?6,?7,?8,?9,?10)",
                params![network,fact.source_id,fact.deal_id,model,at,escrow,bond,credit,residual,Utc::now().timestamp_millis()],
            )?;
        }
        transaction.execute(
            "UPDATE deals SET status=?3,closed_at=?4,incomplete_reason=?5 WHERE network=?1 AND deal_id=?2",
            params![network,fact.deal_id,if credit.is_some(){"settled"}else{"incomplete"},at,
                if credit.is_some(){None}else{Some("buyer credits not reconciled")}],
        )?;
        transaction.execute(
            "UPDATE deal_quota SET closed=1 WHERE network=?1 AND deal_id=?2",
            params![network, fact.deal_id],
        )?;
        Ok(())
    }

    pub fn apply_operational_expense(&self, fact: OperationalFact) -> Result<()> {
        ensure!(!fact.source_id.is_empty(), "operational source ID is empty");
        let at = chain_millis(fact.occurred_at)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = transaction.query_row(
            "SELECT kind,deal_id,model,occurred_at,spent_raw FROM money_facts WHERE network=?1 AND source_id=?2",
            params![self.network(),fact.source_id],
            |row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,i64>(3)?,row.get::<_,Option<String>>(4)?)),
        ).optional()?;
        let spent = fact.spent_raw.to_string();
        if let Some(old) = old {
            ensure!(
                old.0 == "operational"
                    && old.1 == fact.deal_id
                    && (old.2.is_none() || fact.model.is_none() || old.2 == fact.model)
                    && old.3 == at
                    && old.4.as_ref() == Some(&spent),
                "conflicting replay of operational expense"
            );
            if old.2.is_none() && fact.model.is_some() {
                transaction.execute(
                    "UPDATE money_facts SET model=?3,recorded_at=?4 WHERE network=?1 AND source_id=?2",
                    params![self.network(),fact.source_id,fact.model,Utc::now().timestamp_millis()],
                )?;
            }
        } else {
            transaction.execute(
                "INSERT INTO money_facts(network,source_id,kind,deal_id,model,occurred_at,spent_raw,recorded_at)
                 VALUES(?1,?2,'operational',?3,?4,?5,?6,?7)",
                params![self.network(),fact.source_id,fact.deal_id,fact.model,at,spent,Utc::now().timestamp_millis()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn query(&self, query: &UsageQuery, now: DateTime<Utc>) -> Result<Value> {
        self.query_with_bounds(query.bounds(now)?, now)
    }

    fn query_with_bounds(&self, bounds: Bounds, now: DateTime<Utc>) -> Result<Value> {
        let today = now.with_timezone(&bounds.server_timezone).date_naive();
        let calendar_today = now.with_timezone(&bounds.timezone).date_naive();
        let first_day = calendar_today
            .checked_sub_days(Days::new(u64::from(bounds.days - 1)))
            .context("usage day range overflow")?;
        let mut days = BTreeMap::new();
        let mut day = first_day;
        loop {
            days.insert(day, TokenSummary::default());
            if day == calendar_today {
                break;
            }
            day = day
                .checked_add_days(Days::new(1))
                .context("usage day range overflow")?;
        }
        let mut daily_money: BTreeMap<_, _> = days
            .keys()
            .map(|day| (*day, MoneySummary::default()))
            .collect();
        let mut models = BTreeMap::<String, TokenSummary>::new();
        let mut model_money = BTreeMap::<String, MoneySummary>::new();
        let (mut total, mut today_usage, mut period_usage) = (
            TokenSummary::default(),
            TokenSummary::default(),
            TokenSummary::default(),
        );
        let (mut spend_total, mut spend_today, mut spend_period) = (
            MoneySummary::default(),
            MoneySummary::default(),
            MoneySummary::default(),
        );
        let mut history_from = None::<i64>;
        let mut last_recorded = None::<i64>;
        let mut unfinished = 0_u64;
        let mut interrupted = 0_u64;
        {
            let mut connection = self.lock()?;
            self.restore_quota(&mut connection, true)?;
        }
        let mut connection = self.lock_history()?;
        let transaction = connection.transaction()?;
        let mut requests = transaction.prepare(
            "SELECT request_id,model,started_at,finished_at,outcome,deal_id,history_complete FROM requests WHERE network=?1",
        )?;
        let mut calls = transaction.prepare(
            "SELECT phase,input_tokens,output_tokens,total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens FROM request_usage
             WHERE network=?1 AND request_id=?2",
        )?;
        let mut request_rows = requests.query(params![self.network()])?;
        while let Some(row) = request_rows.next()? {
            let id: String = row.get(0)?;
            let model: String = row.get(1)?;
            let start: i64 = row.get(2)?;
            let finished: Option<i64> = row.get(3)?;
            let outcome: String = row.get(4)?;
            let deal: Option<String> = row.get(5)?;
            let history_complete: bool = row.get(6)?;
            observe_time(&mut history_from, &mut last_recorded, start);
            let mut summary = TokenSummary {
                request_count: 1,
                ..Default::default()
            };
            summary.requests.insert(outcome.clone(), 1);
            let mut any_answer = false;
            let mut call_rows = calls.query(params![self.network(), id])?;
            while let Some(call) = call_rows.next()? {
                let phase: String = call.get(0)?;
                let accepted = call.get(5)?;
                let input = parse_u64(call.get(1)?)?;
                let output = parse_u64(call.get(2)?)?;
                let total = parse_u64(call.get(3)?)?;
                let accepted_usage = if accepted {
                    let mut usage = BillingUsage::new(input, output, Some(total))
                        .map_err(|error| anyhow!(error))?;
                    usage.cache_read_tokens = parse_u64(call.get(6)?)?;
                    usage.cache_write_tokens = parse_u64(call.get(7)?)?;
                    usage.validate().map_err(|error| anyhow!(error))?;
                    usage
                } else {
                    BillingUsage::default()
                };
                let counter = TokenCounter {
                    input,
                    output,
                    total,
                    charged: parse_u64(call.get(4)?)?,
                    complete: accepted,
                    accepted: accepted_usage,
                };
                summary.total.add(&counter)?;
                if phase == "answer" {
                    any_answer = true;
                    summary.answer.add(&counter)?;
                } else {
                    summary.verification.add(&counter)?;
                }
            }
            // A pre-routing rejection had no paid work. Other missing terminals remain unknown.
            let no_paid_work = outcome == "rejected" && deal.is_none() && !any_answer;
            if !any_answer && !no_paid_work {
                summary.answer.complete = false;
                summary.total.complete = false;
            }
            // An unfinished request may have lost both phase evidence and its failure marker.
            if !history_complete || finished.is_none() {
                summary.answer.complete = false;
                summary.verification.complete = false;
                summary.total.complete = false;
            }
            if outcome == "interrupted" {
                interrupted = checked_add(interrupted, 1)?;
            }
            if finished.is_none() {
                unfinished = checked_add(unfinished, 1)?;
                // Interrupted/in-flight work is included in all-time observed lower bounds only.
                total.add(&summary)?;
                continue;
            }
            let finished = finished.expect("checked above");
            observe_time(&mut history_from, &mut last_recorded, finished);
            total.add(&summary)?;
            let date = timestamp(finished)?
                .with_timezone(&bounds.timezone)
                .date_naive();
            if timestamp(finished)?
                .with_timezone(&bounds.server_timezone)
                .date_naive()
                == today
            {
                today_usage.add(&summary)?;
            }
            if let Some(day) = days.get_mut(&date) {
                day.add(&summary)?;
            }
            if bounds.contains(finished) {
                period_usage.add(&summary)?;
                models.entry(model).or_default().add(&summary)?;
            }
        }
        drop(request_rows);
        drop(calls);
        drop(requests);
        let mut facts = transaction.prepare(
            "SELECT kind,model,occurred_at,escrow_raw,buyer_bond_raw,buyer_credit_raw,spent_raw,recorded_at,
                consumption_raw,commission_raw,burns_raw,unused_capacity_raw,deal_id
             FROM money_facts WHERE network=?1",
        )?;
        let mut rows = facts.query(params![self.network()])?;
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let model: Option<String> = row.get(1)?;
            let at: i64 = row.get(2)?;
            observe_time(&mut history_from, &mut last_recorded, at);
            let recorded_at: i64 = row.get(7)?;
            last_recorded = Some(last_recorded.map_or(recorded_at, |last| last.max(recorded_at)));
            let mut summary = MoneySummary::default();
            if kind != "operational" {
                // Missing settlement credits are counted by the deal's incomplete status below.
                summary.settlement = fact_amount(row)?.unwrap_or_default();
                summary.consumption = parse_u128(row.get("consumption_raw")?)?;
                summary.commission = parse_u128(row.get("commission_raw")?)?;
                summary.burns = parse_u128(row.get("burns_raw")?)?;
                summary.unused_capacity = parse_u128(row.get("unused_capacity_raw")?)?;
                let deal: Option<String> = row.get("deal_id")?;
                // Preserve the proven lower bound for an older history whose price is still
                // unavailable. A recovered price switches the deal to accepted-work accounting.
                if !self.has_deal_price_in(&transaction, deal.as_deref())? {
                    summary.cost = summary.consumption;
                }
            } else {
                summary.operational = fact_amount(row)?.context("operational amount absent")?;
            }
            add_money(
                &bounds,
                today,
                at,
                model.as_deref().unwrap_or("unknown"),
                &summary,
                &mut spend_total,
                &mut spend_today,
                &mut spend_period,
                &mut daily_money,
                &mut model_money,
            )?;
        }
        drop(rows);
        drop(facts);
        self.consumption_in(&transaction, |at, model, cost| {
            let summary = MoneySummary {
                cost,
                ..Default::default()
            };
            add_money(
                &bounds,
                today,
                at,
                model,
                &summary,
                &mut spend_total,
                &mut spend_today,
                &mut spend_period,
                &mut daily_money,
                &mut model_money,
            )
        })?;
        let mut deals = transaction.prepare(
            "SELECT model,registered_at,closed_at,status,operations_complete FROM deals WHERE network=?1",
        )?;
        let mut rows = deals.query(params![self.network()])?;
        while let Some(row) = rows.next()? {
            let model: String = row.get(0)?;
            let registered: i64 = row.get(1)?;
            let closed: Option<i64> = row.get(2)?;
            let status: String = row.get(3)?;
            let operations_complete: bool = row.get(4)?;
            observe_time(&mut history_from, &mut last_recorded, registered);
            if status == "settled" && operations_complete {
                continue;
            }
            if !operations_complete || (status == "incomplete" && closed.is_none()) {
                // Unknown operation dates are independent of the settlement date. No period
                // can claim complete zero until both sources have known coverage.
                let summary = MoneySummary {
                    pending: u64::from(status == "pending"),
                    incomplete: u64::from(status != "pending"),
                    ..Default::default()
                };
                spend_total.add(&summary)?;
                spend_today.add(&summary)?;
                spend_period.add(&summary)?;
                for day in daily_money.values_mut() {
                    day.add(&summary)?;
                }
                // Unknown timing changes completeness, not which models have period records.
                if models.contains_key(&model) || model_money.contains_key(&model) {
                    model_money.entry(model).or_default().add(&summary)?;
                }
                continue;
            }
            let at = closed.unwrap_or(registered);
            let mut summary = MoneySummary::default();
            if status == "pending" {
                summary.pending = 1;
            } else {
                summary.incomplete = 1;
            }
            add_money(
                &bounds,
                today,
                at,
                &model,
                &summary,
                &mut spend_total,
                &mut spend_today,
                &mut spend_period,
                &mut daily_money,
                &mut model_money,
            )?;
        }
        drop(rows);
        drop(deals);
        let mut notes = transaction
            .prepare("SELECT note_id FROM note_operations WHERE network=?1 AND complete=0")?;
        let mut rows = notes.query(params![self.network()])?;
        while rows.next()?.is_some() {
            spend_total.incomplete_sources = checked_add(spend_total.incomplete_sources, 1)?;
            spend_today.incomplete_sources = checked_add(spend_today.incomplete_sources, 1)?;
            spend_period.incomplete_sources = checked_add(spend_period.incomplete_sources, 1)?;
            for day in daily_money.values_mut() {
                day.incomplete_sources = checked_add(day.incomplete_sources, 1)?;
            }
            let unknown = model_money.entry("unknown".into()).or_default();
            unknown.incomplete_sources = checked_add(unknown.incomplete_sources, 1)?;
        }
        drop(rows);
        drop(notes);
        let quota = self.quota_in(&transaction)?;
        transaction.commit()?;
        let daily = days
            .into_iter()
            .map(|(date, summary)| with_field(summary.json(), "date", json!(date.to_string())))
            .collect::<Vec<_>>();
        let model_stats = models
            .into_iter()
            .map(|(model, summary)| with_field(summary.json(), "model", json!(model)))
            .collect::<Vec<_>>();
        let daily_spend = daily_money
            .into_iter()
            .map(|(date, summary)| Ok(with_field(summary.json()?, "date", json!(date.to_string()))))
            .collect::<Result<Vec<_>>>()?;
        let model_spend = model_money
            .into_iter()
            .map(|(model, summary)| Ok(with_field(summary.json()?, "model", json!(model))))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({
            "quota":quota,
            "network":self.network(),"timezone":bounds.timezone.name(),
            "period":{"from":bounds.from.map(|at|at.to_rfc3339()),"to":bounds.to.map(|at|at.to_rfc3339())},
            "usage":{"today":today_usage.json(),"total":total.json(),"period":period_usage.json()},
            "daily_usage":daily,"model_stats":model_stats,
            "spend":{"today":spend_today.json()?,"total":spend_total.json()?,"period":spend_period.json()?},
            "daily_spend":daily_spend,"model_spend":model_spend,
            "coverage":{"history_from":history_from.map(iso),"last_recorded_at":last_recorded.map(iso),
                "pending_deals":spend_total.pending,"incomplete_deals":spend_total.incomplete,
                "incomplete_financial_sources":spend_total.incomplete_sources,
                "unfinished_requests":unfinished,"interrupted_requests":interrupted}
        }))
    }
}

#[derive(Clone)]
struct TokenCounter {
    input: u64,
    output: u64,
    total: u64,
    charged: u64,
    complete: bool,
    accepted: BillingUsage,
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self {
            input: 0,
            output: 0,
            total: 0,
            charged: 0,
            complete: true,
            accepted: BillingUsage::default(),
        }
    }
}

impl TokenCounter {
    fn add(&mut self, other: &Self) -> Result<()> {
        self.input = checked_add(self.input, other.input)?;
        self.output = checked_add(self.output, other.output)?;
        self.total = checked_add(self.total, other.total)?;
        self.charged = checked_add(self.charged, other.charged)?;
        self.complete &= other.complete;
        self.accepted.input_tokens =
            checked_add(self.accepted.input_tokens, other.accepted.input_tokens)?;
        self.accepted.output_tokens =
            checked_add(self.accepted.output_tokens, other.accepted.output_tokens)?;
        self.accepted.total_tokens =
            checked_add(self.accepted.total_tokens, other.accepted.total_tokens)?;
        self.accepted.cache_read_tokens = checked_add(
            self.accepted.cache_read_tokens,
            other.accepted.cache_read_tokens,
        )?;
        self.accepted.cache_write_tokens = checked_add(
            self.accepted.cache_write_tokens,
            other.accepted.cache_write_tokens,
        )?;
        Ok(())
    }

    fn json(&self) -> Value {
        json!({
            "input_tokens":self.complete.then_some(self.input),
            "output_tokens":self.complete.then_some(self.output),
            "total_tokens":self.complete.then_some(self.total),
            "buyer_charged_tokens":self.complete.then_some(self.charged),
            "observed_input_tokens":self.input,"observed_output_tokens":self.output,
            "observed_total_tokens":self.total,
            "cache_read_tokens":self.complete.then_some(self.accepted.cache_read_tokens),
            "cache_write_tokens":self.complete.then_some(self.accepted.cache_write_tokens),
            "complete":self.complete,
            "accepted":{
                "input_tokens":self.accepted.input_tokens - self.accepted.cache_read_tokens - self.accepted.cache_write_tokens,
                "output_tokens":self.accepted.output_tokens,
                "total_tokens":self.accepted.total_tokens,
                "cache_read_tokens":self.accepted.cache_read_tokens,
                "cache_write_tokens":self.accepted.cache_write_tokens
            }
        })
    }
}

#[derive(Default)]
struct TokenSummary {
    request_count: u64,
    requests: BTreeMap<String, u64>,
    answer: TokenCounter,
    verification: TokenCounter,
    total: TokenCounter,
}

impl TokenSummary {
    fn add(&mut self, other: &Self) -> Result<()> {
        self.request_count = checked_add(self.request_count, other.request_count)?;
        for (kind, count) in &other.requests {
            let own = self.requests.entry(kind.clone()).or_default();
            *own = checked_add(*own, *count)?;
        }
        self.answer.add(&other.answer)?;
        self.verification.add(&other.verification)?;
        self.total.add(&other.total)?;
        Ok(())
    }

    fn json(&self) -> Value {
        let mut requests = serde_json::Map::new();
        for kind in [
            "succeeded",
            "rejected",
            "failed",
            "cancelled",
            "interrupted",
            "in_progress",
        ] {
            let count = self.requests.get(kind).copied().unwrap_or_default();
            requests.insert(kind.to_owned(), json!(count));
        }
        requests.insert("total".to_owned(), json!(self.request_count));
        json!({"requests":requests,"answer":self.answer.json(),"verification":self.verification.json(),
            "total":self.total.json(),"usage_complete":self.total.complete})
    }
}

#[derive(Default)]
struct MoneySummary {
    settlement: u128,
    operational: u128,
    consumption: u128,
    cost: u128,
    commission: u128,
    burns: u128,
    unused_capacity: u128,
    pending: u64,
    incomplete: u64,
    incomplete_sources: u64,
}

impl MoneySummary {
    fn add(&mut self, other: &Self) -> Result<()> {
        self.settlement = self
            .settlement
            .checked_add(other.settlement)
            .context("settlement expense overflow")?;
        self.operational = self
            .operational
            .checked_add(other.operational)
            .context("operational expense overflow")?;
        self.consumption = self
            .consumption
            .checked_add(other.consumption)
            .context("consumption expense overflow")?;
        self.cost = self
            .cost
            .checked_add(other.cost)
            .context("billable consumption overflow")?;
        self.commission = self
            .commission
            .checked_add(other.commission)
            .context("commission expense overflow")?;
        self.burns = self
            .burns
            .checked_add(other.burns)
            .context("burn expense overflow")?;
        self.unused_capacity = self
            .unused_capacity
            .checked_add(other.unused_capacity)
            .context("capacity expense overflow")?;
        self.pending = checked_add(self.pending, other.pending)?;
        self.incomplete = checked_add(self.incomplete, other.incomplete)?;
        self.incomplete_sources = checked_add(self.incomplete_sources, other.incomplete_sources)?;
        Ok(())
    }
    fn json(&self) -> Result<Value> {
        let confirmed = self
            .settlement
            .checked_add(self.operational)
            .context("total SHELL expense overflow")?;
        let complete = self.pending == 0 && self.incomplete == 0 && self.incomplete_sources == 0;
        let classified = ExpenseBreakdown {
            consumption_raw: self.consumption,
            commission_raw: self.commission,
            burns_raw: self.burns,
            unused_capacity_raw: self.unused_capacity,
            ..Default::default()
        }
        .total()?;
        let other = self
            .settlement
            .checked_sub(classified)
            .context("expense breakdown exceeds settlement")?;
        // Unclassified money can still contain model principal. Only proven non-model
        // components are overhead; live consumption need not have been settled yet.
        let overhead = self
            .operational
            .checked_add(self.commission)
            .and_then(|sum| sum.checked_add(self.burns))
            .and_then(|sum| sum.checked_add(self.unused_capacity))
            .context("overhead expense overflow")?;
        Ok(
            json!({"settlement_cost_shell":shell(self.settlement),"operational_cost_shell":shell(self.operational),
            "confirmed_cost_shell":shell(confirmed),"actual_cost_shell":shell(confirmed),
            "cost_shell":shell(self.cost),"overhead_cost_shell":shell(overhead),
            "expense_breakdown":{"consumption_shell":shell(self.consumption),"operational_shell":shell(self.operational),
                "commission_shell":shell(self.commission),"burns_shell":shell(self.burns),
                "unused_capacity_shell":shell(self.unused_capacity),"other_shell":shell(other)},
            "complete":complete,"pending_deals":self.pending,"incomplete_deals":self.incomplete,
            "incomplete_financial_sources":self.incomplete_sources}),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn add_money(
    bounds: &Bounds,
    today: NaiveDate,
    at: i64,
    model: &str,
    summary: &MoneySummary,
    total: &mut MoneySummary,
    today_sum: &mut MoneySummary,
    period: &mut MoneySummary,
    days: &mut BTreeMap<NaiveDate, MoneySummary>,
    models: &mut BTreeMap<String, MoneySummary>,
) -> Result<()> {
    total.add(summary)?;
    let date = timestamp(at)?.with_timezone(&bounds.timezone).date_naive();
    if timestamp(at)?
        .with_timezone(&bounds.server_timezone)
        .date_naive()
        == today
    {
        today_sum.add(summary)?;
    }
    if let Some(day) = days.get_mut(&date) {
        day.add(summary)?;
    }
    if bounds.contains(at) {
        period.add(summary)?;
        models.entry(model.to_owned()).or_default().add(summary)?;
    }
    Ok(())
}

fn with_field(mut value: Value, key: &str, field: Value) -> Value {
    value
        .as_object_mut()
        .expect("summary is an object")
        .insert(key.to_owned(), field);
    value
}
fn checked_add(left: u64, right: u64) -> Result<u64> {
    left.checked_add(right).context("usage counter overflow")
}
/// Exact floor of the consumed part of one confirmed probe payment, without uint128 overflow.
pub fn paid_probe_cost(amount: u128, tokens: u128) -> u128 {
    let tick = dexdo_core::params::TICK_SIZE;
    let tokens = tokens.min(tick);
    amount / tick * tokens + (amount % tick) * tokens / tick
}

fn fact_amount(row: &rusqlite::Row<'_>) -> Result<Option<u128>> {
    if row.get::<_, String>("kind")? == "settlement" {
        let credit: Option<String> = row.get("buyer_credit_raw")?;
        let residual: Option<String> = row.get("spent_raw")?;
        if credit.is_some() {
            if let Some(residual) = residual {
                return parse_u128(residual).map(Some);
            }
        }
        credit
            .map(|credit| {
                parse_u128(row.get("escrow_raw")?)?
                    .checked_add(parse_u128(row.get("buyer_bond_raw")?)?)
                    .and_then(|sum| sum.checked_sub(parse_u128(credit).ok()?))
                    .context("invalid or overflowing settlement expense")
            })
            .transpose()
    } else {
        Ok(Some(parse_u128(row.get("spent_raw")?)?))
    }
}

fn parse_u64(value: String) -> Result<u64> {
    value.parse().context("invalid persisted usage counter")
}
fn parse_u128(value: String) -> Result<u128> {
    value.parse().context("invalid persisted SHELL amount")
}
fn timestamp(at: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(at).context("usage timestamp out of range")
}
fn chain_millis(seconds: i64) -> Result<i64> {
    let millis = seconds
        .checked_mul(1_000)
        .context("chain timestamp overflow")?;
    timestamp(millis)?;
    Ok(millis)
}
fn iso(at: i64) -> String {
    timestamp(at)
        .expect("validated stored timestamp")
        .to_rfc3339()
}
fn observe_time(first: &mut Option<i64>, last: &mut Option<i64>, at: i64) {
    *first = Some(first.map_or(at, |first| first.min(at)));
    *last = Some(last.map_or(at, |last| last.max(at)));
}
fn shell(raw: u128) -> String {
    let whole = raw / SHELL_UNIT;
    let remainder = raw % SHELL_UNIT;
    if remainder == 0 {
        return whole.to_string();
    }
    let width = (SHELL_UNIT.ilog10()) as usize;
    let fraction = format!("{remainder:0width$}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, UsageStore) {
        let dir = TempDir::new().unwrap();
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        (dir, store)
    }
    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }
    fn request(store: &UsageStore, id: &str, model: &str, at: i64, input: u64, output: u64) {
        store
            .start_request_at(id, model, "openai", false, at - 1)
            .unwrap();
        store
            .record_call_usage(
                id,
                "answer",
                "answer",
                BillingUsage::new(input, output, None).unwrap(),
                true,
            )
            .unwrap();
        store
            .finish_request_at(id, 200, Some("stop"), None, at)
            .unwrap();
    }
    fn settlement(id: &str, credit: Option<u128>) -> SettlementFact {
        SettlementFact {
            source_id: format!("settlement:{id}"),
            deal_id: id.into(),
            occurred_at: at("2026-10-19T12:00:00Z").timestamp(),
            escrow_raw: 2 * SHELL_UNIT,
            buyer_bond_raw: SHELL_UNIT,
            buyer_credit_raw: credit,
        }
    }

    #[test]
    fn legacy_dates_include_the_last_server_day_across_dst() {
        let now = at("2026-03-09T12:00:00Z");
        let query = UsageQuery {
            start_date: Some("2026-03-08".into()),
            end_date: Some("2026-03-08".into()),
            timezone: Some("Asia/Tokyo".into()),
            ..Default::default()
        };
        let server: Tz = "America/New_York".parse().unwrap();
        let bounds = query.bounds_in(now, server).unwrap();
        assert_eq!(bounds.from, Some(at("2026-03-08T05:00:00Z")));
        assert_eq!(bounds.to, Some(at("2026-03-09T04:00:00Z")));
        assert_eq!(bounds.timezone.name(), "Asia/Tokyo");
        assert!(bounds.contains(at("2026-03-09T03:59:59.999Z").timestamp_millis()));
        assert!(!bounds.contains(at("2026-03-09T04:00:00Z").timestamp_millis()));
        let default = UsageQuery::default().bounds_in(now, server).unwrap();
        assert_eq!(default.timezone, server);
    }

    #[test]
    fn midnight_dst_gap_uses_the_first_existing_instant_of_each_calendar_day() {
        let now = at("2026-10-19T12:00:00Z");
        let server: Tz = "America/Santiago".parse().unwrap();
        for (first, last, from, to) in [
            (
                "2026-09-06",
                "2026-09-06",
                "2026-09-06T04:00:00Z",
                "2026-09-07T03:00:00Z",
            ),
            (
                "2026-09-05",
                "2026-09-05",
                "2026-09-05T04:00:00Z",
                "2026-09-06T04:00:00Z",
            ),
            (
                "2026-09-05",
                "2026-09-06",
                "2026-09-05T04:00:00Z",
                "2026-09-07T03:00:00Z",
            ),
        ] {
            let query = UsageQuery {
                start_date: Some(first.into()),
                end_date: Some(last.into()),
                ..Default::default()
            };
            let bounds = query.bounds_in(now, server).unwrap();
            assert_eq!(bounds.from, Some(at(from)));
            assert_eq!(bounds.to, Some(at(to)));
            assert!(bounds.contains(at(from).timestamp_millis()));
            assert!(!bounds.contains(at(to).timestamp_millis()));
        }
    }

    #[test]
    fn missing_civil_day_is_rejected_as_a_date_but_can_follow_an_inclusive_end() {
        let now = at("2026-10-19T12:00:00Z");
        let server: Tz = "Pacific/Apia".parse().unwrap();
        for (first, last) in [
            ("2011-12-30", "2011-12-30"),
            ("2011-12-29", "2011-12-30"),
            ("2011-12-30", "2011-12-31"),
        ] {
            assert!(UsageQuery {
                start_date: Some(first.into()),
                end_date: Some(last.into()),
                ..Default::default()
            }
            .bounds_in(now, server)
            .is_err());
        }
        let bounds = UsageQuery {
            start_date: Some("2011-12-29".into()),
            end_date: Some("2011-12-29".into()),
            ..Default::default()
        }
        .bounds_in(now, server)
        .unwrap();
        assert_eq!(bounds.from, Some(at("2011-12-29T10:00:00Z")));
        assert_eq!(bounds.to, Some(at("2011-12-30T10:00:00Z")));
    }

    #[test]
    fn timezone_selection_tries_valid_env_then_os_then_utc_near_midnight() {
        let (dir, store) = store();
        let now = at("2026-10-19T22:00:00Z");
        request(&store, "r", "m", now.timestamp_millis(), 1, 2);
        for environment in [None, Some(""), Some("not/a/timezone")] {
            let server = select_server_timezone(environment, || Some("Europe/Sofia".into()));
            assert_eq!(server.name(), "Europe/Sofia");
            let query = UsageQuery {
                start_date: Some("2026-10-20".into()),
                end_date: Some("2026-10-20".into()),
                days: Some(1),
                ..Default::default()
            };
            let bounds = query.bounds_in(now, server).unwrap();
            assert_eq!(bounds.from, Some(at("2026-10-19T21:00:00Z")));
            assert_eq!(bounds.to, Some(at("2026-10-20T21:00:00Z")));
            let summary = store.query_with_bounds(bounds, now).unwrap();
            assert_eq!(summary["usage"]["today"]["requests"]["total"], 1);
            assert_eq!(summary["daily_usage"][0]["date"], "2026-10-20");
            assert_eq!(summary["usage"]["period"]["requests"]["total"], 1);
        }
        let server = select_server_timezone(Some(":America/New_York"), || {
            panic!("a valid TZ must not consult OS fallback")
        });
        assert_eq!(server.name(), "America/New_York");
        let query = UsageQuery {
            days: Some(1),
            ..Default::default()
        };
        let summary = store
            .query_with_bounds(query.bounds_in(now, server).unwrap(), now)
            .unwrap();
        assert_eq!(summary["daily_usage"][0]["date"], "2026-10-19");
        for (environment, os) in [
            (None, None),
            (Some(""), Some("")),
            (Some("invalid"), Some("invalid")),
        ] {
            assert_eq!(
                select_server_timezone(environment, || os.map(str::to_owned)),
                chrono_tz::UTC
            );
        }
        drop(store);
        let reopened = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        assert_eq!(
            reopened
                .query_with_bounds(query.bounds_in(now, server).unwrap(), now)
                .unwrap()["usage"]["today"]["requests"]["total"],
            1
        );
    }

    #[test]
    fn precise_bounds_override_both_legacy_dates_without_parsing_them() {
        let now = at("2026-10-19T12:00:00Z");
        let server: Tz = "Europe/Sofia".parse().unwrap();
        let query = UsageQuery {
            from: Some("2026-10-18T12:30:00Z".into()),
            start_date: Some("not-a-date".into()),
            end_date: Some("2026-02-30".into()),
            ..Default::default()
        };
        let bounds = query.bounds_in(now, server).unwrap();
        assert_eq!(bounds.from, Some(at("2026-10-18T12:30:00Z")));
        assert_eq!(bounds.to, Some(now));
        let query = UsageQuery {
            from: None,
            to: Some(now.to_rfc3339()),
            ..query
        };
        let bounds = query.bounds_in(now, server).unwrap();
        assert_eq!(bounds.from, Some(at("2026-09-19T12:00:00Z")));
    }

    #[test]
    fn malformed_reversed_and_nonexistent_legacy_days_are_rejected() {
        let now = at("2026-10-19T12:00:00Z");
        for (from, to) in [
            ("2026-10-19", "2026-10-18"),
            ("2026-2-03", "2026-10-19"),
            ("2026-02-30", "2026-10-19"),
            ("", "2026-10-19"),
        ] {
            let query = UsageQuery {
                start_date: Some(from.into()),
                end_date: Some(to.into()),
                ..Default::default()
            };
            assert!(query.bounds_in(now, chrono_tz::UTC).is_err());
        }
        let skipped_day = UsageQuery {
            start_date: Some("2011-12-30".into()),
            end_date: Some("2011-12-30".into()),
            ..Default::default()
        };
        assert!(skipped_day
            .bounds_in(now, "Pacific/Apia".parse().unwrap())
            .is_err());
    }

    #[test]
    fn daily_timezone_does_not_move_the_server_today_summary() {
        let (_dir, store) = store();
        request(
            &store,
            "yesterday",
            "m",
            at("2026-10-18T23:30:00Z").timestamp_millis(),
            1,
            2,
        );
        request(
            &store,
            "today",
            "m",
            at("2026-10-19T00:30:00Z").timestamp_millis(),
            3,
            4,
        );
        let now = at("2026-10-19T01:00:00Z");
        let query = UsageQuery {
            days: Some(1),
            timezone: Some("America/New_York".into()),
            ..Default::default()
        };
        let result = store
            .query_with_bounds(query.bounds_in(now, chrono_tz::UTC).unwrap(), now)
            .unwrap();
        assert_eq!(result["usage"]["today"]["requests"]["total"], 1);
        assert_eq!(result["usage"]["today"]["total"]["total_tokens"], 7);
        assert_eq!(result["daily_usage"][0]["date"], "2026-10-18");
        assert_eq!(result["daily_usage"][0]["requests"]["total"], 2);
    }

    #[test]
    fn lost_phase_write_remains_unknown_after_success_and_restart() {
        let (dir, store) = store();
        store.start_request("r", "m", "openai", false).unwrap();
        store
            .record_call_usage(
                "r",
                "answer",
                "answer",
                BillingUsage::new(2, 3, None).unwrap(),
                true,
            )
            .unwrap();
        store.mark_request_history_incomplete("r").unwrap();
        store.finish_request("r", 200, Some("stop"), None).unwrap();
        drop(store);
        let reopened = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        let result = reopened.query(&UsageQuery::default(), Utc::now()).unwrap();
        let total = &result["usage"]["total"];
        assert_eq!(total["requests"]["succeeded"], 1);
        assert_eq!(total["answer"]["observed_total_tokens"], 5);
        assert!(total["verification"]["total_tokens"].is_null());
        assert!(total["total"]["total_tokens"].is_null());
        assert_eq!(total["usage_complete"], false);
    }

    #[test]
    fn competing_writer_cannot_admit_an_unpersisted_request() {
        let (dir, store) = store();
        let competing = Connection::open(dir.path().join("usage.sqlite3")).unwrap();
        competing.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(store
            .start_request("blocked", "m", "openai", false)
            .is_err());
        competing.execute_batch("ROLLBACK").unwrap();
        let summary = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(summary["usage"]["total"]["requests"]["total"], 0);
        store.start_request("retry", "m", "openai", false).unwrap();
        store
            .finish_request("retry", 400, Some("error"), Some("request_rejected"))
            .unwrap();
        assert_eq!(
            store.query(&UsageQuery::default(), Utc::now()).unwrap()["usage"]["total"]["requests"]
                ["rejected"],
            1
        );
    }

    #[test]
    fn sqlite_reopen_retains_requests_but_separates_networks() {
        let (dir, store) = store();
        let now = at("2026-10-19T12:00:00Z");
        request(&store, "r1", "model-a", now.timestamp_millis(), 3, 5);
        drop(store);
        let reopened = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        let usage = reopened.query(&UsageQuery::default(), now).unwrap();
        assert_eq!(usage["usage"]["total"]["requests"]["total"], 1);
        assert_eq!(usage["usage"]["total"]["answer"]["total_tokens"], 8);
        let other = UsageStore::open(dir.path().join("usage.sqlite3"), "mainnet").unwrap();
        assert_eq!(
            other.query(&UsageQuery::default(), now).unwrap()["usage"]["total"]["requests"]
                ["total"],
            0
        );
        request(&other, "r1", "model-a", now.timestamp_millis(), 10, 20);
        assert_eq!(
            reopened.query(&UsageQuery::default(), now).unwrap()["usage"]["total"]["answer"]
                ["total_tokens"],
            8
        );
    }

    #[test]
    fn call_replay_upgrades_observed_terminal_once_and_rejects_changed_facts() {
        let (_dir, store) = store();
        let now = Utc::now();
        store.start_request("r", "m", "openai", true).unwrap();
        let mut billing = BillingUsage::new(4, 6, None).unwrap();
        billing.charged_tokens = Some(3);
        store
            .record_call_usage("r", "answer", "answer", billing, false)
            .unwrap();
        store.finish_request("r", 200, Some("stop"), None).unwrap();
        let observed = store.query(&UsageQuery::default(), now).unwrap();
        assert!(observed["usage"]["total"]["answer"]["total_tokens"].is_null());
        assert_eq!(
            observed["usage"]["total"]["answer"]["observed_total_tokens"],
            10
        );
        for _ in 0..3 {
            store
                .record_call_usage("r", "answer", "answer", billing, true)
                .unwrap();
        }
        let accepted = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(accepted["usage"]["total"]["answer"]["total_tokens"], 10);
        assert_eq!(
            accepted["usage"]["total"]["answer"]["buyer_charged_tokens"],
            3
        );
        assert!(store
            .record_call_usage(
                "r",
                "answer",
                "answer",
                BillingUsage::new(4, 7, None).unwrap(),
                true
            )
            .is_err());
        assert!(store
            .record_call_usage("missing", "answer", "answer", billing, true)
            .is_err());
        assert!(store
            .record_call_usage("r", "unknown", "answer", billing, true)
            .is_err());
    }

    #[test]
    fn paid_verification_survives_failed_request_and_is_separate_from_answer() {
        let (_dir, store) = store();
        store.start_request("r", "m", "anthropic", true).unwrap();
        store.link_deal("r", "deal").unwrap();
        store
            .record_call_usage(
                "r",
                "verification_b8",
                "b8",
                BillingUsage::new(20, 5, None).unwrap(),
                true,
            )
            .unwrap();
        store
            .finish_request("r", 502, Some("error"), Some("verification_b8"))
            .unwrap();
        let result = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(result["usage"]["total"]["requests"]["failed"], 1);
        assert_eq!(result["usage"]["total"]["verification"]["total_tokens"], 25);
        assert!(result["usage"]["total"]["answer"]["total_tokens"].is_null());
        assert_eq!(
            result["usage"]["total"]["total"]["observed_total_tokens"],
            25
        );
        assert!(result["usage"]["total"]["total"]["total_tokens"].is_null());
    }

    #[test]
    fn accepted_cache_counts_2132_survive_restart_failure_and_replay() {
        let (dir, store) = store();
        store.start_request("cache", "m", "openai", true).unwrap();
        let mut usage = BillingUsage::new(12, 3, None).unwrap();
        usage.cache_read_tokens = 7;
        usage.cache_write_tokens = 2;
        usage.charged_tokens = Some(4);
        store
            .record_call_usage("cache", "verification_b8", "b8", usage, false)
            .unwrap();
        assert_eq!(
            store.query(&UsageQuery::default(), Utc::now()).unwrap()["usage"]["total"]["total"]
                ["accepted"]["total_tokens"],
            0
        );
        for _ in 0..2 {
            store
                .record_call_usage("cache", "verification_b8", "b8", usage, true)
                .unwrap();
        }
        store
            .finish_request("cache", 502, Some("error"), Some("verification_b8"))
            .unwrap();
        let mut conflict = usage;
        conflict.cache_read_tokens = 6;
        assert!(store
            .record_call_usage("cache", "verification_b8", "b8", conflict, true)
            .is_err());
        drop(store);
        let reopened = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        let snapshot = reopened.query(&UsageQuery::default(), Utc::now()).unwrap();
        let accepted = &snapshot["usage"]["total"]["total"]["accepted"];
        assert_eq!(
            accepted,
            &json!({"input_tokens":3,"output_tokens":3,"total_tokens":15,
            "cache_read_tokens":7,"cache_write_tokens":2})
        );
        assert_eq!(snapshot["model_stats"][0]["total"]["accepted"], *accepted);
        assert_eq!(
            snapshot["daily_usage"].as_array().unwrap().last().unwrap()["total"]["accepted"],
            *accepted
        );
    }

    #[test]
    fn unknown_expense_2132_stays_other_until_a_bounded_classification_is_known() {
        let (_dir, store) = store();
        store.register_deal("d", "m").unwrap();
        store
            .apply_settlement(settlement("d", Some(SHELL_UNIT)))
            .unwrap();
        let before = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(before["spend"]["total"]["cost_shell"], "0");
        assert_eq!(
            before["spend"]["total"]["expense_breakdown"]["other_shell"],
            "2"
        );
        store
            .classify_expense(
                "settlement:d",
                ExpenseBreakdown {
                    consumption_raw: SHELL_UNIT,
                    commission_raw: SHELL_UNIT / 4,
                    burns_raw: SHELL_UNIT / 4,
                    unused_capacity_raw: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(store
            .classify_expense(
                "settlement:d",
                ExpenseBreakdown {
                    consumption_raw: 3 * SHELL_UNIT,
                    ..Default::default()
                }
            )
            .is_err());
        let after = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(after["spend"]["total"]["cost_shell"], "1");
        assert_eq!(after["spend"]["total"]["overhead_cost_shell"], "0.5");
        assert_eq!(
            after["spend"]["total"]["expense_breakdown"]["other_shell"],
            "0.5"
        );
        assert_eq!(
            before["spend"]["total"]["confirmed_cost_shell"],
            after["spend"]["total"]["confirmed_cost_shell"]
        );
    }

    #[test]
    fn exact_half_open_ranges_and_dst_calendar_days_are_independent() {
        let (_dir, store) = store();
        let before = at("2026-03-08T04:59:59.999Z").timestamp_millis();
        let start = at("2026-03-08T05:00:00Z").timestamp_millis();
        let inside = at("2026-03-09T03:59:59.999Z").timestamp_millis();
        let end = at("2026-03-09T04:00:00Z").timestamp_millis();
        for (id, time) in [
            ("before", before),
            ("start", start),
            ("inside", inside),
            ("end", end),
        ] {
            request(&store, id, "m", time, 1, 2);
        }
        let query = UsageQuery {
            from: Some("2026-03-08T05:00:00Z".into()),
            to: Some("2026-03-09T04:00:00Z".into()),
            days: Some(1),
            timezone: Some("America/New_York".into()),
            ..Default::default()
        };
        let result = store.query(&query, at("2026-03-09T03:59:59.999Z")).unwrap();
        assert_eq!(result["usage"]["total"]["requests"]["total"], 4);
        assert_eq!(result["usage"]["today"]["requests"]["total"], 2);
        assert_eq!(result["daily_usage"][0]["date"], "2026-03-08");
        assert_eq!(result["daily_usage"][0]["requests"]["total"], 2);
        assert_eq!(result["usage"]["period"]["requests"]["total"], 2);
        assert_eq!(result["usage"]["period"]["total"]["total_tokens"], 6);
        assert_eq!(result["model_stats"][0]["requests"]["total"], 2);
        assert_eq!(result["model_stats"][0]["total"]["total_tokens"], 6);
        let excluded = UsageQuery {
            from: Some("2026-03-10T00:00:00Z".into()),
            to: Some("2026-03-11T00:00:00Z".into()),
            ..query
        };
        let excluded = store
            .query(&excluded, at("2026-03-09T03:59:59.999Z"))
            .unwrap();
        assert!(excluded["model_stats"].as_array().unwrap().is_empty());
        assert_eq!(excluded["usage"]["period"]["requests"]["total"], 0);
        assert_eq!(excluded["usage"]["period"]["total"]["total_tokens"], 0);
        assert_eq!(excluded["usage"]["today"]["requests"]["total"], 2);
    }

    #[test]
    fn submillisecond_bounds_preserve_half_open_selection_before_and_after_epoch() {
        let (_dir, store) = store();
        let now = at("2026-10-19T12:00:00Z");
        let modern = at("2026-10-19T12:00:00Z").timestamp_millis();
        for (id, time, output) in [
            ("modern-zero", modern, 1),
            ("modern-one", modern + 1, 2),
            ("epoch-negative", -1, 3),
            ("epoch-zero", 0, 5),
            ("epoch-positive", 1, 7),
        ] {
            request(&store, id, "m", time, 0, output);
        }
        for (from, to, expected_output) in [
            ("2026-10-19T12:00:00.0001Z", "2026-10-19T12:00:00.0011Z", 2),
            ("2026-10-19T12:00:00Z", "2026-10-19T12:00:00.0001Z", 1),
            ("1969-12-31T23:59:59.9991Z", "1970-01-01T00:00:00.0001Z", 5),
        ] {
            let query = UsageQuery {
                from: Some(from.into()),
                to: Some(to.into()),
                ..Default::default()
            };
            query.validate(now).unwrap();
            let result = store.query(&query, now).unwrap();
            assert_eq!(result["model_stats"][0]["requests"]["total"], 1);
            assert_eq!(
                result["model_stats"][0]["total"]["output_tokens"],
                expected_output
            );
            assert_eq!(at(result["period"]["from"].as_str().unwrap()), at(from));
            assert_eq!(at(result["period"]["to"].as_str().unwrap()), at(to));
        }
        let empty = UsageQuery {
            from: Some("2026-10-19T12:00:00.0001Z".into()),
            to: Some("2026-10-19T12:00:00.0009Z".into()),
            ..Default::default()
        };
        empty.validate(now).unwrap();
        assert!(store.query(&empty, now).unwrap()["model_stats"]
            .as_array()
            .unwrap()
            .is_empty());
        let reversed = UsageQuery {
            from: empty.to,
            to: empty.from,
            ..Default::default()
        };
        assert!(reversed.validate(now).is_err());
    }

    #[test]
    fn invalid_queries_and_unsupported_schema_fail_closed() {
        let now = at("2026-10-19T12:00:00Z");
        for query in [
            UsageQuery {
                days: Some(0),
                ..Default::default()
            },
            UsageQuery {
                days: Some(BUYER_USAGE_MAX_DAYS + 1),
                ..Default::default()
            },
            UsageQuery {
                timezone: Some("not/a/timezone".into()),
                ..Default::default()
            },
            UsageQuery {
                from: Some("yesterday".into()),
                ..Default::default()
            },
            UsageQuery {
                from: Some("2026-10-20T00:00:00Z".into()),
                ..Default::default()
            },
            UsageQuery {
                to: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
            UsageQuery {
                from: Some(now.to_rfc3339()),
                to: Some(now.to_rfc3339()),
                ..Default::default()
            },
        ] {
            assert!(query.validate(now).is_err(), "{query:?}");
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("future.sqlite3");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", BUYER_USAGE_SCHEMA_VERSION + 1)
            .unwrap();
        assert!(UsageStore::open(path, "shellnet").is_err());
    }

    #[test]
    fn default_period_is_thirty_days_and_does_not_follow_daily_days_parameter() {
        let (_dir, store) = store();
        let now = at("2026-10-19T12:00:00Z");
        request(
            &store,
            "old",
            "m",
            at("2026-08-01T12:00:00Z").timestamp_millis(),
            1,
            1,
        );
        request(
            &store,
            "recent",
            "m",
            at("2026-10-01T12:00:00Z").timestamp_millis(),
            1,
            1,
        );
        let query = UsageQuery {
            days: Some(1),
            ..Default::default()
        };
        let result = store.query(&query, now).unwrap();
        assert_eq!(result["usage"]["total"]["requests"]["total"], 2);
        assert_eq!(result["model_stats"][0]["requests"]["total"], 1);
        assert_eq!(result["usage"]["period"]["requests"]["total"], 1);
        assert_eq!(result["daily_usage"].as_array().unwrap().len(), 1);
        assert_eq!(result["period"]["from"], "2026-09-19T12:00:00+00:00");
    }

    #[test]
    fn incomplete_reconciliation_preserves_confirmed_actual_expense() {
        let (_dir, store) = store();
        store.register_deal("d", "m").unwrap();
        assert_eq!(
            store.query(&UsageQuery::default(), Utc::now()).unwrap()["spend"]["total"]
                ["actual_cost_shell"],
            "0"
        );
        store.apply_settlement(settlement("d", None)).unwrap();
        let incomplete = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(incomplete["coverage"]["incomplete_deals"], 1);
        assert_eq!(incomplete["spend"]["total"]["actual_cost_shell"], "0");
        assert_eq!(incomplete["spend"]["total"]["complete"], false);
        store
            .apply_settlement(settlement("d", Some(SHELL_UNIT)))
            .unwrap();
        assert_eq!(
            store.query(&UsageQuery::default(), Utc::now()).unwrap()["spend"]["total"]
                ["actual_cost_shell"],
            "2"
        );
        store.mark_operations_complete("d").unwrap();
        let complete = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(complete["spend"]["total"]["actual_cost_shell"], "2");
        assert!(complete["spend"]["total"]["complete"].as_bool().unwrap());
        store
            .mark_operations_incomplete("d", "RPC unavailable")
            .unwrap();
        assert_eq!(
            store.query(&UsageQuery::default(), Utc::now()).unwrap()["spend"]["total"]
                ["actual_cost_shell"],
            "2"
        );
        store
            .mark_note_operations_incomplete("note", "BUY receipts not read")
            .unwrap();
        assert_eq!(
            store.query(&UsageQuery::default(), Utc::now()).unwrap()["coverage"]
                ["incomplete_financial_sources"],
            1
        );
    }

    #[test]
    fn unknown_operation_dates_keep_other_periods_incomplete_until_reconciliation() {
        let (dir, store) = store();
        let now = at("2026-10-20T12:00:00Z");
        store.register_deal("d", "m").unwrap();
        store
            .apply_settlement(settlement("d", Some(SHELL_UNIT)))
            .unwrap();
        let operation = OperationalFact {
            source_id: "before-settlement".into(),
            deal_id: Some("d".into()),
            model: Some("m".into()),
            occurred_at: at("2026-10-18T12:00:00Z").timestamp(),
            spent_raw: SHELL_UNIT / 4,
        };
        store.apply_operational_expense(operation.clone()).unwrap();
        let queries = [
            ("2026-10-18T00:00:00Z", "2026-10-19T00:00:00Z", "0.25"),
            ("2026-10-19T00:00:00Z", "2026-10-20T00:00:00Z", "2"),
            ("2026-10-20T00:00:00Z", "2026-10-21T00:00:00Z", "0"),
        ];
        for (from, to, confirmed) in queries {
            let q = UsageQuery {
                from: Some(from.into()),
                to: Some(to.into()),
                days: Some(3),
                timezone: Some("UTC".into()),
                ..Default::default()
            };
            let summary = store
                .query_with_bounds(q.bounds_in(now, chrono_tz::UTC).unwrap(), now)
                .unwrap();
            assert_eq!(
                summary["spend"]["period"]["confirmed_cost_shell"],
                confirmed
            );
            for kind in ["total", "today", "period"] {
                assert_eq!(
                    summary["spend"][kind]["actual_cost_shell"],
                    summary["spend"][kind]["confirmed_cost_shell"]
                );
                assert_eq!(summary["spend"][kind]["complete"], false);
            }
            assert_eq!(summary["spend"]["total"]["confirmed_cost_shell"], "2.25");
            assert!(summary["daily_spend"]
                .as_array()
                .unwrap()
                .iter()
                .all(
                    |day| day["actual_cost_shell"] == day["confirmed_cost_shell"]
                        && day["complete"] == false
                ));
            if confirmed == "0" {
                assert!(summary["model_spend"].as_array().unwrap().is_empty());
            } else {
                assert_eq!(summary["model_spend"][0]["actual_cost_shell"], confirmed);
                assert_eq!(summary["model_spend"][0]["complete"], false);
            }
        }
        store.apply_operational_expense(operation).unwrap();
        store.mark_operations_complete("d").unwrap();
        drop(store);
        let reopened = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        for (from, to, confirmed) in queries {
            let q = UsageQuery {
                from: Some(from.into()),
                to: Some(to.into()),
                days: Some(3),
                timezone: Some("UTC".into()),
                ..Default::default()
            };
            for _ in 0..2 {
                let summary = reopened
                    .query_with_bounds(q.bounds_in(now, chrono_tz::UTC).unwrap(), now)
                    .unwrap();
                assert_eq!(summary["spend"]["period"]["actual_cost_shell"], confirmed);
                assert_eq!(summary["spend"]["total"]["actual_cost_shell"], "2.25");
                assert_eq!(summary["daily_spend"][0]["actual_cost_shell"], "0.25");
                assert_eq!(summary["daily_spend"][1]["actual_cost_shell"], "2");
                assert_eq!(summary["daily_spend"][2]["actual_cost_shell"], "0");
            }
        }
    }

    #[test]
    fn uncertain_reused_deal_preserves_confirmed_cost_but_cannot_claim_zero_in_a_later_period() {
        let (_dir, store) = store();
        store.register_deal("d", "m").unwrap();
        store
            .apply_settlement(settlement("d", Some(SHELL_UNIT)))
            .unwrap();
        store.mark_operations_complete("d").unwrap();
        store
            .mark_deal_incomplete("d", "multiple_funding_cycles")
            .unwrap();
        let now = at("2026-10-21T12:00:00Z");
        let query = UsageQuery {
            from: Some("2026-10-21T00:00:00Z".into()),
            to: Some("2026-10-22T00:00:00Z".into()),
            days: Some(1),
            ..Default::default()
        };
        let result = store.query(&query, now).unwrap();
        assert_eq!(result["spend"]["total"]["confirmed_cost_shell"], "2");
        assert_eq!(result["spend"]["total"]["actual_cost_shell"], "2");
        assert_eq!(result["spend"]["period"]["confirmed_cost_shell"], "0");
        assert_eq!(result["spend"]["period"]["actual_cost_shell"], "0");
        assert_eq!(result["daily_spend"][0]["actual_cost_shell"], "0");
        assert_eq!(result["spend"]["total"]["complete"], false);
    }

    #[test]
    fn financial_periods_use_chain_time_and_local_days_instead_of_reconciliation_time() {
        let (_dir, store) = store();
        store.register_deal("d", "m").unwrap();
        let mut fact = settlement("d", Some(SHELL_UNIT));
        fact.occurred_at = at("2026-10-19T21:30:00Z").timestamp();
        store.apply_settlement(fact).unwrap();
        store
            .apply_operational_expense(OperationalFact {
                source_id: "buy-before-midnight".into(),
                deal_id: Some("d".into()),
                model: Some("m".into()),
                occurred_at: at("2026-10-19T20:59:59Z").timestamp(),
                spent_raw: SHELL_UNIT / 40,
            })
            .unwrap();
        store.mark_operations_complete("d").unwrap();
        let query = UsageQuery {
            from: Some("2026-10-19T21:00:00Z".into()),
            to: Some("2026-10-19T22:00:00Z".into()),
            timezone: Some("Europe/Sofia".into()),
            days: Some(2),
            ..Default::default()
        };
        let now = at("2026-10-19T21:45:00Z");
        let result = store
            .query_with_bounds(query.bounds_in(now, chrono_tz::UTC).unwrap(), now)
            .unwrap();
        assert_eq!(result["spend"]["total"]["actual_cost_shell"], "2.025");
        assert_eq!(result["spend"]["period"]["actual_cost_shell"], "2");
        assert_eq!(result["spend"]["today"]["actual_cost_shell"], "2.025");
        assert_eq!(result["daily_spend"][0]["date"], "2026-10-19");
        assert_eq!(result["daily_spend"][0]["actual_cost_shell"], "0.025");
        assert_eq!(result["daily_spend"][1]["date"], "2026-10-20");
        assert_eq!(result["daily_spend"][1]["actual_cost_shell"], "2");
        assert_eq!(result["model_spend"][0]["actual_cost_shell"], "2");
    }

    #[test]
    fn money_facts_deduplicate_and_reject_contradictory_or_oversized_refunds() {
        let (_dir, store) = store();
        store.register_deal("d", "m").unwrap();
        let fact = settlement("d", Some(SHELL_UNIT));
        store.apply_settlement(fact.clone()).unwrap();
        store.apply_settlement(fact.clone()).unwrap();
        let mut conflict = fact.clone();
        conflict.buyer_credit_raw = Some(0);
        assert!(store.apply_settlement(conflict).is_err());
        let mut duplicate = fact.clone();
        duplicate.source_id = "another-settlement".into();
        assert!(store.apply_settlement(duplicate).is_err());
        let mut oversized = fact;
        oversized.buyer_credit_raw = Some(4 * SHELL_UNIT);
        assert!(store.apply_settlement(oversized).is_err());
        let operation = OperationalFact {
            source_id: "buy:note:message".into(),
            deal_id: Some("d".into()),
            model: Some("m".into()),
            occurred_at: Utc::now().timestamp(),
            spent_raw: SHELL_UNIT / 40,
        };
        store.apply_operational_expense(operation.clone()).unwrap();
        store.apply_operational_expense(operation.clone()).unwrap();
        let mut changed = operation;
        changed.spent_raw += 1;
        assert!(store.apply_operational_expense(changed).is_err());
        store.mark_operations_complete("d").unwrap();
        let result = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(result["spend"]["total"]["settlement_cost_shell"], "2");
        assert_eq!(result["spend"]["total"]["operational_cost_shell"], "0.025");
        assert_eq!(result["spend"]["total"]["actual_cost_shell"], "2.025");
    }

    #[test]
    fn large_money_remains_decimal_text_and_overflow_is_an_error() {
        let (_dir, store) = store();
        let value = u128::from(u64::MAX) + 1;
        store
            .apply_operational_expense(OperationalFact {
                source_id: "large".into(),
                deal_id: None,
                model: None,
                occurred_at: Utc::now().timestamp(),
                spent_raw: value,
            })
            .unwrap();
        let result = store.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(result["spend"]["total"]["actual_cost_shell"], shell(value));
        let persisted: (String, String) = store
            .lock()
            .unwrap()
            .query_row(
                "SELECT typeof(spent_raw),spent_raw FROM money_facts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(persisted, ("text".into(), value.to_string()));
        store
            .apply_operational_expense(OperationalFact {
                source_id: "overflow".into(),
                deal_id: None,
                model: None,
                occurred_at: Utc::now().timestamp(),
                spent_raw: u128::MAX,
            })
            .unwrap();
        assert!(store.query(&UsageQuery::default(), Utc::now()).is_err());
    }

    #[test]
    fn recovery_preserves_observed_work_without_inventing_completion_or_usage() {
        let (dir, store) = store();
        store.start_request("crashed", "m", "openai", true).unwrap();
        store
            .record_call_usage(
                "crashed",
                "verification_b8",
                "b8",
                BillingUsage::new(2, 3, None).unwrap(),
                true,
            )
            .unwrap();
        drop(store);
        let recovered = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        assert_eq!(recovered.recover_interrupted_requests().unwrap(), 1);
        assert_eq!(recovered.recover_interrupted_requests().unwrap(), 0);
        let result = recovered.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(result["coverage"]["interrupted_requests"], 1);
        assert_eq!(result["coverage"]["unfinished_requests"], 1);
        assert_eq!(result["usage"]["total"]["verification"]["complete"], false);
        assert!(result["usage"]["total"]["verification"]["total_tokens"].is_null());
        assert_eq!(
            result["usage"]["total"]["verification"]["observed_total_tokens"],
            5
        );
        assert!(result["usage"]["total"]["total"]["total_tokens"].is_null());
        assert_eq!(result["usage"]["today"]["requests"]["total"], 0);
    }

    #[test]
    fn opening_waits_for_an_existing_writer_without_schema_upgrade_failure() {
        use std::sync::{mpsc, Barrier};
        let (dir, store) = store();
        request(&store, "retained", "m", Utc::now().timestamp_millis(), 1, 2);
        let path = dir.path().join("usage.sqlite3");
        let writer = store.lock().unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            sender.send(UsageStore::open(path, "shellnet")).unwrap();
        });
        barrier.wait();
        let early = receiver.recv_timeout(BUYER_USAGE_SQLITE_BUSY_TIMEOUT / 10);
        writer.execute_batch("COMMIT").unwrap();
        drop(writer);
        worker.join().unwrap();
        assert!(
            matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
            "opening must wait for the writer, rather than immediately fail: {early:?}"
        );
        let reopened = receiver.recv().unwrap().unwrap();
        let summary = reopened.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(summary["usage"]["total"]["requests"]["succeeded"], 1);
        assert_eq!(summary["usage"]["total"]["answer"]["total_tokens"], 3);
    }

    #[test]
    fn opening_times_out_on_a_retained_writer_and_can_retry_after_release() {
        let (dir, store) = store();
        request(&store, "retained", "m", Utc::now().timestamp_millis(), 1, 2);
        let path = dir.path().join("usage.sqlite3");
        let writer = store.lock().unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        let error = UsageStore::open(&path, "shellnet").unwrap_err();
        let elapsed = started.elapsed();
        writer.execute_batch("COMMIT").unwrap();
        drop(writer);
        assert!(
            elapsed >= BUYER_USAGE_SQLITE_BUSY_TIMEOUT / 2,
            "returned before the busy wait: {elapsed:?}"
        );
        assert_eq!(
            error
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
        let reopened = UsageStore::open(path, "shellnet").unwrap();
        let summary = reopened.query(&UsageQuery::default(), Utc::now()).unwrap();
        assert_eq!(summary["usage"]["total"]["requests"]["succeeded"], 1);
        assert_eq!(summary["usage"]["total"]["answer"]["total_tokens"], 3);
    }

    #[test]
    fn simultaneous_fresh_opens_preserve_every_connection_write() {
        use std::sync::Barrier;
        for _ in 0..4 {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("usage.sqlite3");
            let workers = 8;
            let barrier = Barrier::new(workers);
            std::thread::scope(|scope| {
                for i in 0..workers {
                    let path = &path;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let store = UsageStore::open(path, "shellnet").unwrap();
                        request(
                            &store,
                            &format!("request-{i}"),
                            "m",
                            Utc::now().timestamp_millis(),
                            1,
                            2,
                        );
                    });
                }
            });
            let reopened = UsageStore::open(path, "shellnet").unwrap();
            let summary = reopened.query(&UsageQuery::default(), Utc::now()).unwrap();
            assert_eq!(summary["usage"]["total"]["requests"]["succeeded"], workers);
            assert_eq!(
                summary["usage"]["total"]["answer"]["total_tokens"],
                workers * 3
            );
        }
    }

    #[test]
    fn schema_is_wal_full_and_owner_only_on_unix() {
        let (dir, store) = store();
        let connection = store.lock().unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let mode = std::fs::metadata(dir.path().join(format!("usage.sqlite3{suffix}")))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o077, 0);
            }
        }
        #[cfg(not(unix))]
        let _ = dir;
    }

    proptest! {
        #[test]
        fn replay_and_phase_partition_preserve_provider_and_charged_counts(
            answer_input in 0_u64..1_000_000, answer_output in 0_u64..1_000_000,
            verify_input in 0_u64..1_000_000, verify_output in 0_u64..1_000_000,
        ) {
            let (_dir,store)=store();
            store.start_request("r","m","openai",false).unwrap();
            let mut answer=BillingUsage::new(answer_input,answer_output,None).unwrap();
            answer.charged_tokens=Some(answer.total_tokens/2);
            answer.cache_read_tokens=answer_input/2;
            answer.cache_write_tokens=(answer_input-answer.cache_read_tokens)/2;
            let verify=BillingUsage::new(verify_input,verify_output,None).unwrap();
            for _ in 0..2 {
                store.record_call_usage("r","answer","answer",answer,true).unwrap();
                store.record_call_usage("r","verification_b8","b8",verify,true).unwrap();
            }
            store.finish_request("r",200,Some("stop"),None).unwrap();
            let result=store.query(&UsageQuery::default(),Utc::now()).unwrap();
            prop_assert_eq!(result["usage"]["total"]["requests"]["total"].as_u64(),Some(1));
            prop_assert_eq!(result["usage"]["total"]["total"]["total_tokens"].as_u64(),Some(answer.total_tokens+verify.total_tokens));
            prop_assert_eq!(result["usage"]["total"]["total"]["buyer_charged_tokens"].as_u64(),Some(answer.billable_tokens()+verify.billable_tokens()));
            let accepted=&result["usage"]["total"]["total"]["accepted"];
            let sum=["input_tokens","output_tokens","cache_read_tokens","cache_write_tokens"].iter()
                .map(|field|accepted[field].as_u64().unwrap()).sum::<u64>();
            prop_assert_eq!(sum,answer.total_tokens+verify.total_tokens);
        }

        #[test]
        fn settlement_and_operation_expense_conservation(escrow in 0_u64..u64::MAX/4,bond in 0_u64..u64::MAX/4,credit_fraction in 0_u8..=100,operation in 0_u64..u64::MAX/4) {
            let (_dir,store)=store();
            store.register_deal("d","m").unwrap();
            let contributions=u128::from(escrow)+u128::from(bond);
            let credit=contributions*u128::from(credit_fraction)/100;
            let fact=SettlementFact{source_id:"settlement".into(),deal_id:"d".into(),occurred_at:Utc::now().timestamp(),escrow_raw:u128::from(escrow),buyer_bond_raw:u128::from(bond),buyer_credit_raw:Some(credit)};
            store.apply_settlement(fact.clone()).unwrap();
            store.apply_settlement(fact).unwrap();
            let consumption=(contributions-credit)/2;
            store.classify_expense("settlement",ExpenseBreakdown {consumption_raw:consumption,
                commission_raw:(contributions-credit-consumption)/2,..Default::default()}).unwrap();
            store.apply_operational_expense(OperationalFact{source_id:"operation".into(),deal_id:Some("d".into()),model:Some("m".into()),occurred_at:Utc::now().timestamp(),spent_raw:u128::from(operation)}).unwrap();
            store.mark_operations_complete("d").unwrap();
            let result=store.query(&UsageQuery::default(),Utc::now()).unwrap();
            let expected=shell(contributions-credit+u128::from(operation));
            prop_assert_eq!(result["spend"]["total"]["actual_cost_shell"].as_str(),Some(expected.as_str()));
            prop_assert_eq!(result["model_spend"][0]["actual_cost_shell"].as_str(),Some(expected.as_str()));
            let cost=shell(consumption);
            let overhead=shell((contributions-credit-consumption)/2+u128::from(operation));
            prop_assert_eq!(result["spend"]["total"]["cost_shell"].as_str(),Some(cost.as_str()));
            prop_assert_eq!(result["spend"]["total"]["overhead_cost_shell"].as_str(),Some(overhead.as_str()));
        }
    }
}
