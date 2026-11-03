//! Keyed admission reads; totals change in the same transaction as request accounting.
use super::*;

#[cfg(test)]
#[path = "usage_snapshot_tests.rs"]
mod tests;

const MODEL_SQL: &str = "SELECT input,output,cache_read,cache_write,incomplete FROM admission_models WHERE network=?1 AND model=?2";
const DEAL_SQL: &str = "SELECT charged FROM admission_deals WHERE network=?1 AND deal_id=?2";

#[derive(Default, PartialEq, Eq)]
pub(super) struct RequestTotals {
    model: String,
    deal: Option<String>,
    // Non-cached input, output, cache read, cache write, incomplete requests.
    model_totals: [u128; 5],
    charged: u128,
}

fn add(left: u128, right: u128) -> Result<u128> {
    left.checked_add(right)
        .context("admission counter overflow")
}

fn read_model(connection: &Connection, network: &str, model: &str) -> Result<[u128; 5]> {
    let row = connection
        .query_row(MODEL_SQL, params![network, model], |row| {
            Ok([
                row.get::<_, String>(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ])
        })
        .optional()?;
    let mut totals = [0; 5];
    if let Some(row) = row {
        for (total, value) in totals.iter_mut().zip(row) {
            *total = parse_u128(value)?;
        }
    }
    Ok(totals)
}

fn read_deal(connection: &Connection, network: &str, deal: &str) -> Result<u128> {
    connection
        .query_row(DEAL_SQL, params![network, deal], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .map(parse_u128)
        .transpose()
        .map(|value| value.unwrap_or(0))
}

impl UsageStore {
    pub(super) fn request_totals_in(
        connection: &Connection,
        network: &str,
        request: &str,
    ) -> Result<RequestTotals> {
        let (model, deal, finished, outcome, history): (String, Option<String>, bool, String, bool) = connection.query_row(
            "SELECT model,deal_id,finished_at IS NOT NULL,outcome,history_complete FROM requests WHERE network=?1 AND request_id=?2",
            params![network, request], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
        let mut result = RequestTotals {
            model,
            deal,
            ..Default::default()
        };
        let mut complete = finished && history;
        let mut answer = false;
        let mut statement = connection.prepare(
            "SELECT phase,call_id,input_tokens,output_tokens,total_tokens,charged_tokens,cache_read_tokens,cache_write_tokens,accepted FROM request_usage WHERE network=?1 AND request_id=?2")?;
        let mut rows = statement.query(params![network, request])?;
        while let Some(row) = rows.next()? {
            answer |= row.get::<_, String>(0)? == "answer";
            let accepted: bool = row.get(8)?;
            complete &= accepted;
            if !accepted || row.get::<_, String>(1)? == "in_progress" {
                continue;
            }
            let mut usage = BillingUsage::new(
                parse_u64(row.get(2)?)?,
                parse_u64(row.get(3)?)?,
                Some(parse_u64(row.get(4)?)?),
            )
            .map_err(|error| anyhow!(error))?;
            usage.cache_read_tokens = parse_u64(row.get(6)?)?;
            usage.cache_write_tokens = parse_u64(row.get(7)?)?;
            usage.validate().map_err(|error| anyhow!(error))?;
            for (total, value) in result.model_totals.iter_mut().zip([
                usage.input_tokens - usage.cache_read_tokens - usage.cache_write_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
            ]) {
                *total = add(*total, u128::from(value))?;
            }
            result.charged = add(result.charged, parse_u128(row.get(5)?)?)?;
        }
        complete &= answer || (outcome == "rejected" && result.deal.is_none());
        result.model_totals[4] = u128::from(!complete);
        Ok(result)
    }

    pub(super) fn update_totals_in(
        connection: &Connection,
        network: &str,
        request: &str,
        before: Option<RequestTotals>,
    ) -> Result<()> {
        let after = Self::request_totals_in(connection, network, request)?;
        if before.as_ref() == Some(&after) {
            return Ok(());
        }
        let mut totals = read_model(connection, network, &after.model)?;
        let previous = before
            .as_ref()
            .map(|value| value.model_totals)
            .unwrap_or_default();
        for ((total, old), new) in totals.iter_mut().zip(previous).zip(after.model_totals) {
            *total = add(
                total
                    .checked_sub(old)
                    .context("admission counter underflow")?,
                new,
            )?;
        }
        connection.execute(
            "INSERT INTO admission_models(network,model,input,output,cache_read,cache_write,incomplete) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(network,model) DO UPDATE SET input=excluded.input,output=excluded.output,cache_read=excluded.cache_read,cache_write=excluded.cache_write,incomplete=excluded.incomplete",
            params![network, after.model, totals[0].to_string(), totals[1].to_string(), totals[2].to_string(), totals[3].to_string(), totals[4].to_string()])?;
        // A request can bind late, but cannot move from one deal to another.
        if let Some(deal) = after.deal.as_deref() {
            let old = before
                .as_ref()
                .filter(|value| value.deal == after.deal)
                .map_or(0, |value| value.charged);
            let total = add(
                read_deal(connection, network, deal)?
                    .checked_sub(old)
                    .context("admission deal counter underflow")?,
                after.charged,
            )?;
            connection.execute("INSERT INTO admission_deals(network,deal_id,charged) VALUES(?1,?2,?3) ON CONFLICT(network,deal_id) DO UPDATE SET charged=excluded.charged",
                    params![network, deal, total.to_string()])?;
        }
        Ok(())
    }

    pub(super) fn migrate_admission(connection: &Connection) -> Result<()> {
        connection.execute_batch(
            "CREATE TABLE admission_models(network TEXT NOT NULL,model TEXT NOT NULL,input TEXT NOT NULL,output TEXT NOT NULL,cache_read TEXT NOT NULL,cache_write TEXT NOT NULL,incomplete TEXT NOT NULL,PRIMARY KEY(network,model));
             CREATE TABLE admission_deals(network TEXT NOT NULL,deal_id TEXT NOT NULL,charged TEXT NOT NULL,PRIMARY KEY(network,deal_id));")?;
        let mut statement = connection.prepare("SELECT network,request_id FROM requests")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            Self::update_totals_in(
                connection,
                &row.get::<_, String>(0)?,
                &row.get::<_, String>(1)?,
                None,
            )?;
        }
        Ok(())
    }

    /// A saved snapshot, not a reservation or authorization for a future inference request.
    pub fn admission_snapshot(&self, model: &str) -> Result<Value> {
        let waiting = std::time::Instant::now();
        let mut connection = self.lock()?;
        let db_wait_us = waiting.elapsed().as_secs_f64() * 1_000_000.0;
        let reading = std::time::Instant::now();
        self.restore_quota(&mut connection, true)?;
        let transaction = connection.transaction()?;
        let totals = read_model(&transaction, self.network(), model)?;
        let quota = self.quota_with_cost_in(&transaction, |id, price| {
            consumption::cost(price, read_deal(&transaction, self.network(), id)?)
        })?;
        transaction.commit()?;
        tracing::debug!(
            db_wait_us,
            read_us = reading.elapsed().as_secs_f64() * 1_000_000.0,
            "admission snapshot timing"
        );
        Ok(
            json!({"network": self.network(), "model":model, "complete":totals[4] == 0,
            "quota":quota, "accepted":{"unit":"tokens", "input_tokens":totals[0].to_string(),
                "output_tokens":totals[1].to_string(),"cache_read_tokens":totals[2].to_string(),
                "cache_write_tokens":totals[3].to_string()}}),
        )
    }
}
