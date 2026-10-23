//! Consumption is priced from accepted billing evidence, independently of cash settlement.
use super::*;

/// Exact cumulative price without overflowing the intermediate tokens * price product.
fn cost(price: u128, tokens: u128) -> Result<u128> {
    let tick = dexdo_core::params::TICK_SIZE;
    (tokens / tick)
        .checked_mul(price)
        .and_then(|whole| {
            let tail = tokens % tick;
            (price / tick)
                .checked_mul(tail)
                .and_then(|part| part.checked_add((price % tick) * tail / tick))
                .and_then(|part| whole.checked_add(part))
        })
        .context("billable consumption overflow")
}

impl UsageStore {
    pub fn record_deal_price(&self, deal: &str, price: u128) -> Result<()> {
        let connection = self.lock()?;
        Self::record_deal_price_in(&connection, self.network(), deal, price)
    }

    pub(super) fn record_deal_price_in(
        connection: &Connection,
        network: &str,
        deal: &str,
        price: u128,
    ) -> Result<()> {
        ensure!(price > 0, "empty execution price");
        let previous: Option<String> = connection
            .query_row(
                "SELECT price_raw FROM deal_prices WHERE network=?1 AND deal_id=?2",
                params![network, deal],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            ensure!(
                parse_u128(previous)? == price,
                "execution price changed for {deal}"
            );
        } else {
            connection.execute(
                "INSERT INTO deal_prices(network,deal_id,price_raw) VALUES(?1,?2,?3)",
                params![network, deal, price.to_string()],
            )?;
        }
        Ok(())
    }

    pub(super) fn has_deal_price_in(
        &self,
        connection: &Connection,
        deal: Option<&str>,
    ) -> Result<bool> {
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM deal_prices WHERE network=?1 AND deal_id=?2)",
            params![self.network(), deal],
            |row| row.get(0),
        )?)
    }

    pub(super) fn consumed_cost_in(
        &self,
        connection: &Connection,
        deal: &str,
        price: u128,
    ) -> Result<u128> {
        let mut statement = connection.prepare(
            "SELECT u.charged_tokens FROM request_usage u JOIN requests r
             ON u.network=r.network AND u.request_id=r.request_id
             WHERE u.network=?1 AND r.deal_id=?2 AND u.accepted=1 AND u.call_id!='in_progress'",
        )?;
        let mut tokens = 0_u128;
        for row in
            statement.query_map(params![self.network(), deal], |row| row.get::<_, String>(0))?
        {
            tokens = tokens
                .checked_add(parse_u128(row?)?)
                .context("accepted consumption overflow")?;
        }
        cost(price, tokens)
    }

    pub(super) fn consumption_in(
        &self,
        connection: &Connection,
        mut consume: impl FnMut(i64, &str, u128) -> Result<()>,
    ) -> Result<()> {
        let mut statement = connection.prepare(
            "SELECT r.deal_id,r.model,u.accepted_at,u.charged_tokens,p.price_raw
             FROM request_usage u JOIN requests r ON u.network=r.network AND u.request_id=r.request_id
             JOIN deal_prices p ON p.network=r.network AND p.deal_id=r.deal_id
             WHERE u.network=?1 AND u.accepted=1 AND u.call_id!='in_progress'
             ORDER BY u.accepted_at,r.request_id,u.phase,u.call_id",
        )?;
        let mut rows = statement.query([self.network()])?;
        let mut totals = BTreeMap::<String, u128>::new();
        while let Some(row) = rows.next()? {
            let deal: String = row.get(0)?;
            let price = parse_u128(row.get(4)?)?;
            let previous = totals.entry(deal).or_default();
            let next = previous
                .checked_add(parse_u128(row.get(3)?)?)
                .context("accepted consumption overflow")?;
            let delta = cost(price, next)?
                .checked_sub(cost(price, *previous)?)
                .context("consumption decreased")?;
            *previous = next;
            let model: String = row.get(1)?;
            consume(row.get(2)?, &model, delta)?;
        }
        Ok(())
    }
}
