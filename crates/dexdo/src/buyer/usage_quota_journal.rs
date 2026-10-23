//! Write-ahead metadata for the one current purchase, independent of history reconciliation.
use super::*;
use std::io::Write;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QuotaJournal {
    pub deal_id: String,
    pub filled_ticks: Option<String>,
    pub price_raw: Option<String>,
    pub closed: bool,
    /// Routes awaiting their first successful state/terms read; not confirmed purchases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_deals: Vec<String>,
}

impl UsageStore {
    /// Persist uncertainty before the first RPC. Keep the previous purchase intact until the
    /// new route supplies actual opened terms or is proved never opened.
    pub fn begin_deal_quota(&self, deal_id: &str) -> Result<()> {
        let _connection = self.lock()?;
        let mut record = self.read_quota_journal()?.unwrap_or_else(|| QuotaJournal {
            deal_id: deal_id.to_owned(),
            filled_ticks: None,
            price_raw: None,
            closed: false,
            pending_deals: Vec::new(),
        });
        if !record.pending_deals.iter().any(|id| id == deal_id) {
            record.pending_deals.push(deal_id.to_owned());
            self.write_quota_journal(&record)?;
        }
        Ok(())
    }

    /// Remove only the candidate whose authoritative state proved it never opened.
    pub(crate) fn discard_unopened_deal_quota(&self, deal_id: &str) -> Result<()> {
        let _connection = self.lock()?;
        let Some(mut record) = self.read_quota_journal()? else {
            return Ok(());
        };
        if !record.pending_deals.iter().any(|id| id == deal_id) {
            return Ok(());
        }
        record.pending_deals.retain(|id| id != deal_id);
        if record.filled_ticks.is_some()
            || record.price_raw.is_some()
            || !record.pending_deals.is_empty()
        {
            return self.write_quota_journal(&record);
        }
        // No prior journal purchase: the existing SQLite quota (if any) was never replaced.
        std::fs::remove_file(self.quota_journal.as_ref())?;
        #[cfg(unix)]
        std::fs::File::open(
            self.quota_journal
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }

    pub(super) fn read_quota_journal(&self) -> Result<Option<QuotaJournal>> {
        match std::fs::read(self.quota_journal.as_ref()) {
            Ok(bytes) => {
                let mut record: QuotaJournal =
                    serde_json::from_slice(&bytes).context("invalid purchase quota journal")?;
                // Earlier journals represented an unresolved route with missing terms alone.
                if record.filled_ticks.is_none()
                    && record.price_raw.is_none()
                    && record.pending_deals.is_empty()
                {
                    record.pending_deals.push(record.deal_id.clone());
                }
                Ok(Some(record))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read purchase quota journal"),
        }
    }

    // Callers hold the store mutex; production also holds the existing buyer instance lock.
    pub(super) fn write_quota_journal(&self, record: &QuotaJournal) -> Result<()> {
        let path = self.quota_journal.as_ref();
        let temporary = path.with_extension(format!("{}.tmp", self.runtime_id));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> Result<()> {
            let mut file = options
                .open(&temporary)
                .context("create purchase quota journal")?;
            file.write_all(&serde_json::to_vec(record)?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path).context("publish purchase quota journal")?;
            #[cfg(unix)]
            std::fs::File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new(".")),
            )?
            .sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }

    pub(super) fn quota_row(&self, connection: &Connection) -> Result<Option<QuotaJournal>> {
        Ok(connection
            .query_row(
                "SELECT deal_id,filled_ticks,price_raw,closed FROM deal_quota WHERE network=?1",
                params![self.network()],
                |row| {
                    Ok(QuotaJournal {
                        deal_id: row.get(0)?,
                        filled_ticks: Some(row.get(1)?),
                        price_raw: Some(row.get(2)?),
                        closed: row.get(3)?,
                        pending_deals: Vec::new(),
                    })
                },
            )
            .optional()?)
    }

    pub(super) fn restore_quota(
        &self,
        connection: &mut Connection,
        require_terms: bool,
    ) -> Result<()> {
        let Some(record) = self.read_quota_journal()? else {
            return Ok(());
        };
        if !record.pending_deals.is_empty() {
            ensure!(!require_terms, "current deal purchase state unavailable");
            return Ok(());
        }
        let (Some(ticks), Some(price)) = (&record.filled_ticks, &record.price_raw) else {
            ensure!(!require_terms, "current deal purchase terms unavailable");
            return Ok(());
        };
        let (ticks_raw, price_raw) = (parse_u128(ticks.clone())?, parse_u128(price.clone())?);
        ensure!(ticks_raw > 0 && price_raw > 0, "empty purchased quota");
        ticks_raw
            .checked_mul(price_raw)
            .context("purchased quota overflow")?;
        Self::record_deal_price_in(connection, self.network(), &record.deal_id, price_raw)?;
        if let Some(old) = self
            .quota_row(connection)?
            .filter(|old| old.deal_id == record.deal_id)
        {
            ensure!(
                old.filled_ticks == record.filled_ticks && old.price_raw == record.price_raw,
                "purchased quota terms changed for {}",
                record.deal_id
            );
            if old.closed || !record.closed {
                return Ok(());
            }
        }
        connection.execute(
            "INSERT INTO deal_quota(network,deal_id,filled_ticks,price_raw,closed)
             VALUES(?1,?2,?3,?4,?5 OR EXISTS(SELECT 1 FROM money_facts WHERE network=?1 AND deal_id=?2 AND kind='settlement'))
             ON CONFLICT(network) DO UPDATE SET deal_id=excluded.deal_id,
             filled_ticks=excluded.filled_ticks,price_raw=excluded.price_raw,closed=excluded.closed",
            params![self.network(),record.deal_id,ticks,price,record.closed],
        )?;
        Ok(())
    }
}
