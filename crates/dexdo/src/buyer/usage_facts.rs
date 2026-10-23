//! Exact, deduplicatable financial facts from the same evidence as usage summaries.
use super::*;
use dexdo_core::params::{
    BUYER_USAGE_FACTS_DEFAULT_LIMIT, BUYER_USAGE_FACTS_MAX_CURSOR_BYTES,
    BUYER_USAGE_FACTS_MAX_LIMIT, BUYER_USAGE_FACTS_MIN_LIMIT, TICK_SIZE,
};
use serde::Serialize;

// Keep a single tuple lower bound so SQLite seeks by both time and source ID, even
// when a large history shares one timestamp. A redundant time lower bound prevents that seek.
const FACTS_PAGE_SQL: &str =
    "SELECT source_id,kind,model,occurred_at,escrow_raw,buyer_bond_raw,buyer_credit_raw,spent_raw,
    consumption_raw,probe_consumption_tokens
 FROM money_facts WHERE network=?1 AND (occurred_at,source_id)>(?2,?3) AND occurred_at<?4
    AND (kind!='settlement' OR (spent_raw IS NOT NULL AND buyer_credit_raw IS NOT NULL))
 ORDER BY occurred_at,source_id LIMIT ?5";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageFactsQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactsCursor {
    store: String,
    network: String,
    revision: i64,
    complete: bool,
    from: Option<i64>,
    to: Option<i64>,
    after: (i64, String),
}

/// Financial rows changed between pages: discard the partial traversal and restart.
#[derive(Debug)]
pub struct UsageFactsCursorExpired;

impl std::fmt::Display for UsageFactsCursorExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "usage facts cursor expired or belongs to another history; restart without cursor",
        )
    }
}
impl std::error::Error for UsageFactsCursorExpired {}

impl UsageFactsQuery {
    fn bounds(&self) -> Result<(Option<i64>, Option<i64>)> {
        let parse = |text: &Option<String>| {
            text.as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()
                .context("usage facts from/to must be RFC3339 timestamps")
        };
        let from = parse(&self.from)?;
        let to = parse(&self.to)?;
        ensure!(
            from.zip(to).is_none_or(|(from, to)| from < to),
            "usage facts from must precede to"
        );
        // Ceiling preserves half-open comparisons at stored millisecond precision.
        let millis = |at: DateTime<chrono::FixedOffset>| {
            at.timestamp_millis()
                + i64::from(!at.timestamp_subsec_nanos().is_multiple_of(1_000_000))
        };
        Ok((from.map(millis), to.map(millis)))
    }

    fn page_limit(&self) -> Result<u32> {
        let limit = self.limit.unwrap_or(BUYER_USAGE_FACTS_DEFAULT_LIMIT);
        ensure!(
            (BUYER_USAGE_FACTS_MIN_LIMIT..=BUYER_USAGE_FACTS_MAX_LIMIT).contains(&limit),
            "usage facts limit must be between {BUYER_USAGE_FACTS_MIN_LIMIT} and {BUYER_USAGE_FACTS_MAX_LIMIT}"
        );
        Ok(limit)
    }

    fn decoded_cursor(&self) -> Result<Option<FactsCursor>> {
        self.cursor
            .as_ref()
            .map(|encoded| {
                ensure!(
                    encoded.len() <= BUYER_USAGE_FACTS_MAX_CURSOR_BYTES,
                    "usage facts cursor is too long"
                );
                let cursor: FactsCursor = serde_json::from_slice(
                    &hex::decode(encoded).context("invalid usage facts cursor")?,
                )
                .context("invalid usage facts cursor")?;
                let (from, to) = self.bounds()?;
                ensure!(
                    cursor.from == from && cursor.to == to,
                    "usage facts cursor filters changed"
                );
                ensure!(
                    !cursor.after.1.is_empty()
                        && cursor.revision >= 0
                        && from.is_none_or(|from| cursor.after.0 >= from)
                        && to.is_none_or(|to| cursor.after.0 < to),
                    "invalid usage facts cursor position"
                );
                Ok(cursor)
            })
            .transpose()
    }

    pub fn validate(&self) -> Result<()> {
        self.bounds()?;
        self.page_limit()?;
        self.decoded_cursor()?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct PaidProbeConsumption {
    /// Completed accepted charged tokens, capped at the already paid tick.
    pub accepted_tokens: String,
    pub tick_size: String,
}

#[derive(Debug, Serialize)]
pub struct UsageFact {
    pub network: String,
    pub source_id: String,
    /// Exact decimal SHELL, serialized as a string to preserve uint128 precision.
    pub amount: String,
    /// Identifiable confirmed consumption within amount, in exact decimal SHELL.
    pub cost: String,
    /// A growing lower bound, verifiable against amount and the paid tick size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid_probe: Option<PaidProbeConsumption>,
    pub occurred_at: String,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageFacts {
    pub unit: &'static str,
    /// Financial completeness, independent of whether another page remains.
    pub complete: bool,
    pub next_cursor: Option<String>,
    pub facts: Vec<UsageFact>,
}

impl UsageStore {
    /// Keyset pages over one unchanged local financial revision. No history is materialized
    /// or retained between calls. A changed revision invalidates continuation, including
    /// late-discovered facts with older chain times and classification/model enrichment.
    pub fn facts(&self, query: &UsageFactsQuery) -> Result<UsageFacts> {
        let (from, to) = query.bounds()?;
        let limit = query.page_limit()?;
        let cursor = query.decoded_cursor()?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let (store, revision): (String, i64) = transaction.query_row(
            "SELECT store_id,revision FROM facts_revision WHERE network=?1",
            [self.network()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let complete: bool = transaction.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM deals WHERE network=?1 AND
                (status!='settled' OR operations_complete=0))
             AND NOT EXISTS(SELECT 1 FROM note_operations WHERE network=?1 AND complete=0)
             AND NOT EXISTS(SELECT 1 FROM money_facts WHERE network=?1 AND kind='settlement'
                AND spent_raw IS NULL)",
            params![self.network()],
            |row| row.get(0),
        )?;
        if let Some(cursor) = &cursor {
            if cursor.store != store
                || cursor.network != self.network()
                || cursor.revision != revision
                || cursor.complete != complete
            {
                return Err(UsageFactsCursorExpired.into());
            }
            let anchor_exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM money_facts WHERE network=?1 AND occurred_at=?2 AND source_id=?3)",
                params![self.network(), cursor.after.0, cursor.after.1], |row| row.get(0),
            )?;
            if !anchor_exists {
                return Err(UsageFactsCursorExpired.into());
            }
        }
        let mut after = cursor
            .map(|cursor| cursor.after)
            .unwrap_or((from.unwrap_or(i64::MIN), String::new()));
        let mut statement = transaction.prepare(FACTS_PAGE_SQL)?;
        let mut rows = statement.query(params![
            self.network(),
            after.0,
            after.1,
            to.unwrap_or(i64::MAX),
            limit + 1
        ])?;
        let mut facts = Vec::new();
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if facts.len() == limit as usize {
                has_more = true;
                break;
            }
            let amount = fact_amount(row)?.context("eligible financial fact has no amount")?;
            let kind = row.get::<_, String>("kind")?;
            let operational = kind == "operational";
            let cost = if operational {
                0
            } else {
                parse_u128(row.get("consumption_raw")?)?
            };
            ensure!(cost <= amount, "consumption exceeds confirmed expense");
            let tokens: Option<String> = row.get("probe_consumption_tokens")?;
            let paid_probe = tokens
                .map(|tokens| -> Result<_> {
                    let accepted = parse_u128(tokens.clone())?;
                    ensure!(
                        kind == "accrual"
                            && amount > 0
                            && accepted <= TICK_SIZE
                            && cost == paid_probe_cost(amount, accepted),
                        "invalid paid probe consumption proof"
                    );
                    Ok(PaidProbeConsumption {
                        accepted_tokens: tokens,
                        tick_size: TICK_SIZE.to_string(),
                    })
                })
                .transpose()?;
            after = (row.get("occurred_at")?, row.get("source_id")?);
            facts.push(UsageFact {
                network: self.network().into(),
                source_id: after.1.clone(),
                amount: shell(amount),
                cost: shell(cost),
                paid_probe,
                occurred_at: iso(after.0),
                model: row.get("model")?,
            });
        }
        drop(rows);
        drop(statement);
        let next_cursor = if has_more {
            Some(hex::encode(serde_json::to_vec(&FactsCursor {
                store,
                network: self.network().into(),
                revision,
                complete,
                from,
                to,
                after,
            })?))
        } else {
            None
        };
        transaction.commit()?;
        Ok(UsageFacts {
            unit: "SHELL",
            complete,
            next_cursor,
            facts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn pagination_2167_bounds_the_first_page_and_exposes_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        for id in ["a", "b", "c"] {
            let mut fact = operation(None);
            fact.source_id = id.into();
            s.apply_operational_expense(fact).unwrap();
        }
        let query = serde_json::from_value(json!({"limit": 2})).unwrap();
        let page = serde_json::to_value(s.facts(&query).unwrap()).unwrap();
        assert_eq!(page["facts"].as_array().unwrap().len(), 2);
        assert!(page["next_cursor"].is_string());
        assert_eq!(page["complete"], true);
    }

    #[test]
    fn pagination_2167_large_history_is_bounded_in_sql_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        {
            let mut connection = s.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for n in 0..BUYER_USAGE_FACTS_MAX_LIMIT * 2 {
                transaction.execute(
                    "INSERT INTO money_facts(network,source_id,kind,occurred_at,recorded_at,spent_raw)
                     VALUES('network-a',?1,'operational',1000000,1000000,'1')",
                    [format!("{n:08}")],
                ).unwrap();
            }
            // A later invalid row must not even be decoded on an earlier page.
            transaction
                .execute(
                    "UPDATE money_facts SET spent_raw='invalid' WHERE source_id=?1",
                    [format!("{:08}", BUYER_USAGE_FACTS_MAX_LIMIT * 2 - 1)],
                )
                .unwrap();
            transaction.commit().unwrap();
        }
        let default = s.facts(&Default::default()).unwrap();
        assert_eq!(
            default.facts.len(),
            BUYER_USAGE_FACTS_DEFAULT_LIMIT as usize
        );
        let max = s
            .facts(&UsageFactsQuery {
                limit: Some(BUYER_USAGE_FACTS_MAX_LIMIT),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(max.facts.len(), BUYER_USAGE_FACTS_MAX_LIMIT as usize);
        assert!(max.next_cursor.is_some());
        let bytes = serde_json::to_vec(&max).unwrap().len();
        assert!(bytes < BUYER_USAGE_FACTS_MAX_LIMIT as usize * 256);
        // SQL LIMIT alone is insufficient: seeking deep into tied timestamps must
        // not walk all preceding rows. Measure SQLite VM work for the actual page query.
        let steps = |after: &str| {
            let connection = s.lock().unwrap();
            let mut statement = connection.prepare(FACTS_PAGE_SQL).unwrap();
            let mut rows = statement
                .query(params![
                    s.network(),
                    1000000,
                    after,
                    i64::MAX,
                    BUYER_USAGE_FACTS_DEFAULT_LIMIT + 1
                ])
                .unwrap();
            let mut count = 0;
            while rows.next().unwrap().is_some() {
                count += 1;
            }
            drop(rows);
            assert_eq!(count, BUYER_USAGE_FACTS_DEFAULT_LIMIT + 1);
            statement.get_status(rusqlite::StatementStatus::VmStep)
        };
        let first_steps = steps("");
        let deep_steps = steps(&format!(
            "{:08}",
            BUYER_USAGE_FACTS_MAX_LIMIT * 2 - BUYER_USAGE_FACTS_DEFAULT_LIMIT - 2
        ));
        assert!(
            deep_steps <= first_steps * 2,
            "first={first_steps}, deep={deep_steps}"
        );
        // Unreconciled terminal rows are outside the paging index, so a long
        // pending history cannot turn an empty/short page into an unbounded scan.
        {
            let mut connection = s.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for n in 0..BUYER_USAGE_FACTS_MAX_LIMIT * 2 {
                transaction
                    .execute(
                        "INSERT INTO money_facts(network,source_id,kind,occurred_at,recorded_at)
                    VALUES('network-a',?1,'settlement',1000000,1000000)",
                        [format!("-pending-{n:08}")],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        assert!(steps("") <= first_steps * 2);
        let pending = s.facts(&Default::default()).unwrap();
        assert!(!pending.complete);
        assert_eq!(
            pending.facts.len(),
            BUYER_USAGE_FACTS_DEFAULT_LIMIT as usize
        );
    }

    #[test]
    fn pagination_2167_rejects_invalid_limits_cursors_and_changed_scope() {
        for value in [
            json!({"limit":0}),
            json!({"limit":BUYER_USAGE_FACTS_MAX_LIMIT+1}),
            json!({"cursor":""}),
            json!({"cursor":"not-hex"}),
            json!({"cursor":"00"}),
            json!({"cursor":"a".repeat(BUYER_USAGE_FACTS_MAX_CURSOR_BYTES+1)}),
        ] {
            let query: UsageFactsQuery = serde_json::from_value(value).unwrap();
            assert!(query.validate().is_err());
        }
        for value in [
            json!({"limit":-1}),
            json!({"limit":"many"}),
            json!({"unknown":1}),
        ] {
            assert!(serde_json::from_value::<UsageFactsQuery>(value).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        for id in ["a", "b"] {
            let mut fact = operation(None);
            fact.source_id = id.into();
            s.apply_operational_expense(fact).unwrap();
        }
        let first = s
            .facts(&UsageFactsQuery {
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        let query = UsageFactsQuery {
            cursor: first.next_cursor,
            limit: Some(1),
            ..Default::default()
        };
        assert!(UsageFactsQuery {
            from: Some("1970-01-01T00:00:00Z".into()),
            ..query.clone()
        }
        .validate()
        .is_err());
        let other_dir = tempfile::tempdir().unwrap();
        assert!(store(&other_dir, "network-a")
            .facts(&query)
            .unwrap_err()
            .is::<UsageFactsCursorExpired>());
        assert!(store(&dir, "network-b")
            .facts(&query)
            .unwrap_err()
            .is::<UsageFactsCursorExpired>());
    }

    #[test]
    fn pagination_2167_changes_expire_traversal_even_with_old_chain_times() {
        for change in ["late", "model", "classification", "completeness"] {
            let dir = tempfile::tempdir().unwrap();
            let s = store(&dir, "network-a");
            s.register_deal("d", "model-a").unwrap();
            for id in ["a", "b"] {
                let mut fact = operation(None);
                fact.source_id = id.into();
                s.apply_operational_expense(fact).unwrap();
            }
            s.apply_deal_expenses(
                "d",
                &[AccrualFact {
                    source_id: "paid".into(),
                    occurred_at: 2000,
                    spent_raw: SHELL_UNIT,
                }],
                None,
            )
            .unwrap();
            let first = s
                .facts(&UsageFactsQuery {
                    limit: Some(1),
                    ..Default::default()
                })
                .unwrap();
            let next = UsageFactsQuery {
                limit: Some(1),
                cursor: first.next_cursor,
                ..Default::default()
            };
            // A separate connection represents a concurrent writer, including after restart.
            let writer = store(&dir, "network-a");
            match change {
                "late" => {
                    let mut fact = operation(None);
                    fact.source_id = "late".into();
                    fact.occurred_at = 1;
                    writer.apply_operational_expense(fact).unwrap();
                }
                "model" => {
                    let mut fact = operation(Some("model-a"));
                    fact.source_id = "a".into();
                    writer.apply_operational_expense(fact).unwrap();
                }
                "classification" => writer
                    .classify_expense(
                        "paid",
                        ExpenseBreakdown {
                            consumption_raw: 1,
                            ..Default::default()
                        },
                    )
                    .unwrap(),
                "completeness" => {
                    writer
                        .apply_deal_expenses(
                            "d",
                            &[AccrualFact {
                                source_id: "paid".into(),
                                occurred_at: 2000,
                                spent_raw: SHELL_UNIT,
                            }],
                            Some(SettlementFact {
                                source_id: "stop".into(),
                                deal_id: "d".into(),
                                occurred_at: 3000,
                                escrow_raw: SHELL_UNIT,
                                buyer_bond_raw: 0,
                                buyer_credit_raw: Some(0),
                            }),
                        )
                        .unwrap();
                    writer.mark_operations_complete("d").unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                s.facts(&next).unwrap_err().is::<UsageFactsCursorExpired>(),
                "{change}"
            );
            assert!(s.facts(&Default::default()).unwrap().next_cursor.is_none());
        }
    }

    #[test]
    fn pagination_2167_empty_history_and_financial_completeness_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        let empty = s.facts(&Default::default()).unwrap();
        assert!(empty.facts.is_empty() && empty.complete && empty.next_cursor.is_none());
        s.register_deal("pending", "model").unwrap();
        let pending = s.facts(&Default::default()).unwrap();
        assert!(pending.facts.is_empty() && !pending.complete && pending.next_cursor.is_none());
    }

    #[test]
    fn consumption_2167_probe_proof_growth_preserves_identity_and_rejects_contradictions() {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let stores = [store(&dirs[0], "network-a"), store(&dirs[1], "network-a")];
        let parts = |tokens| ExpenseBreakdown {
            consumption_raw: paid_probe_cost(SHELL_UNIT, tokens),
            probe_consumption_tokens: Some(tokens),
            ..Default::default()
        };
        let mut exports = Vec::new();
        for (s, tokens) in stores.iter().zip([TICK_SIZE / 4, TICK_SIZE / 2]) {
            s.register_deal("d", "model-a").unwrap();
            let mut other = operation(None);
            other.source_id = "z".into();
            s.apply_operational_expense(other).unwrap();
            s.apply_deal_expenses(
                "d",
                &[AccrualFact {
                    source_id: "paid".into(),
                    occurred_at: 1000,
                    spent_raw: SHELL_UNIT,
                }],
                None,
            )
            .unwrap();
            s.classify_expense("paid", parts(tokens)).unwrap();
            exports.push(s.facts(&Default::default()).unwrap());
        }
        // Growing proofs are comparable across independent histories, not additive.
        for order in [vec![0, 1], vec![1, 0], vec![0, 1, 0]] {
            let mut accepted = 0;
            for index in order {
                let fact = &exports[index].facts[0];
                assert_eq!(fact.amount, "1");
                assert_eq!(fact.source_id, "paid");
                assert_eq!(fact.occurred_at, "1970-01-01T00:16:40+00:00");
                let proof = fact.paid_probe.as_ref().unwrap();
                let tokens = proof.accepted_tokens.parse::<u128>().unwrap();
                assert_eq!(proof.tick_size, TICK_SIZE.to_string());
                assert_eq!(fact.cost, shell(paid_probe_cost(SHELL_UNIT, tokens)));
                accepted = accepted.max(tokens);
            }
            assert_eq!(accepted, TICK_SIZE / 2);
        }
        let s = &stores[0];
        s.classify_expense("paid", parts(TICK_SIZE / 2)).unwrap();
        let before = serde_json::to_value(s.facts(&Default::default()).unwrap()).unwrap();
        let first = s
            .facts(&UsageFactsQuery {
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        s.classify_expense("paid", parts(TICK_SIZE / 2)).unwrap();
        let reopened = store(&dirs[0], "network-a");
        let last = reopened
            .facts(&UsageFactsQuery {
                cursor: first.next_cursor,
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(last.facts[0].source_id, "z");
        assert!(last.next_cursor.is_none());
        assert_eq!(
            before,
            serde_json::to_value(s.facts(&Default::default()).unwrap()).unwrap()
        );
        for invalid in [
            parts(TICK_SIZE / 4),
            parts(TICK_SIZE + 1),
            ExpenseBreakdown {
                consumption_raw: 1,
                ..parts(TICK_SIZE / 2)
            },
            ExpenseBreakdown {
                consumption_raw: SHELL_UNIT / 2,
                ..Default::default()
            },
        ] {
            assert!(s.classify_expense("paid", invalid).is_err());
        }
        assert_eq!(
            before,
            serde_json::to_value(s.facts(&Default::default()).unwrap()).unwrap()
        );
    }

    #[test]
    fn consumption_2167_counts_only_completed_accepted_charged_work_for_this_deal() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        for (id, deal, accepted, status) in [
            ("success", "d", true, Some(200)),
            ("failed", "d", true, Some(502)),
            ("observed", "d", false, Some(200)),
            ("unfinished", "d", true, None),
            ("foreign", "other", true, Some(200)),
        ] {
            s.start_request_at(id, "model", "openai", false, 1000)
                .unwrap();
            s.link_deal(id, deal).unwrap();
            let mut usage = BillingUsage::new(5, 5, None).unwrap();
            usage.charged_tokens = Some(3);
            s.record_call_usage(id, "answer", "answer", usage, accepted)
                .unwrap();
            if let Some(status) = status {
                s.finish_request_at(id, status, None, None, 2000).unwrap();
            }
        }
        assert_eq!(s.accepted_tokens("d").unwrap(), 6);
        assert_eq!(store(&dir, "network-b").accepted_tokens("d").unwrap(), 0);
        assert_eq!(store(&dir, "network-a").accepted_tokens("d").unwrap(), 6);
    }

    #[test]
    fn schema_2167_upgrades_existing_history_and_reopens_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.apply_operational_expense(operation(None)).unwrap();
        let before = serde_json::to_value(s.facts(&Default::default()).unwrap()).unwrap();
        s.lock().unwrap().execute_batch("ALTER TABLE money_facts DROP COLUMN probe_consumption_tokens; ALTER TABLE request_usage DROP COLUMN accepted_at; DROP TABLE deal_prices; PRAGMA user_version=3;").unwrap();
        drop(s);
        for _ in 0..2 {
            let upgraded = store(&dir, "network-a");
            assert_eq!(
                before,
                serde_json::to_value(upgraded.facts(&Default::default()).unwrap()).unwrap()
            );
        }
    }

    proptest! {
        #[test]
        fn pagination_2167_keyset_pages_preserve_every_fact_at_shared_timestamps(count in 0u32..80, size in 1u32..12) {
            let dir=tempfile::tempdir().unwrap();
            let s=store(&dir,"network-a");
            for n in (0..count).rev() {
                let mut fact=operation(None); fact.source_id=format!("{n:04}");
                fact.occurred_at=1000+i64::from(n/3); s.apply_operational_expense(fact).unwrap();
            }
            let mut query=UsageFactsQuery {limit:Some(size),from:Some("1970-01-01T00:16:40Z".into()),
                to:Some("1970-01-01T01:00:00Z".into()),..Default::default()};
            let mut ids=Vec::new();
            loop {
                let page=s.facts(&query).unwrap();
                prop_assert!(page.facts.len()<=size as usize);
                prop_assert_eq!(serde_json::to_value(&page).unwrap(),serde_json::to_value(s.facts(&query).unwrap()).unwrap());
                ids.extend(page.facts.into_iter().map(|fact|fact.source_id));
                query.cursor=page.next_cursor;
                if query.cursor.is_none(){break;}
            }
            prop_assert_eq!(ids,(0..count).map(|n|format!("{n:04}")).collect::<Vec<_>>());
        }

        #[test]
        fn consumption_2167_paid_probe_is_bounded_exact_and_monotone(amount in any::<u128>(), first in 0u128..=TICK_SIZE, extra in any::<u64>()) {
            let later=first.saturating_add(u128::from(extra)).min(TICK_SIZE);
            let cost=paid_probe_cost(amount,first);
            let next=paid_probe_cost(amount,later);
            prop_assert!(cost<=next && next<=amount);
            prop_assert_eq!(paid_probe_cost(amount,TICK_SIZE),amount);
            // The complementary split always conserves the confirmed payment.
            prop_assert_eq!(cost+(amount-cost),amount);
            if let Some(product)=amount.checked_mul(first) {prop_assert_eq!(cost,product/TICK_SIZE);}
        }
    }

    fn store(dir: &tempfile::TempDir, network: &str) -> UsageStore {
        UsageStore::open(dir.path().join("usage.sqlite3"), network).unwrap()
    }

    fn operation(model: Option<&str>) -> OperationalFact {
        OperationalFact {
            source_id: "buyer-buy:shared-message".into(),
            deal_id: None,
            model: model.map(str::to_owned),
            occurred_at: 1_000,
            spent_raw: dexdo_core::params::BUY_ORDER_GAS_RAW,
        }
    }

    fn old_writer_history(dir: &tempfile::TempDir, expense: u128) -> UsageStore {
        let s = store(dir, "network-a");
        s.register_deal("d", "model-a").unwrap();
        s.mark_operations_complete("d").unwrap();
        {
            let connection = s.lock().unwrap();
            // Exact prior writer format: confirmed credits, no stored terminal residual.
            connection.execute(
                "INSERT INTO money_facts(network,source_id,kind,deal_id,model,occurred_at,recorded_at,
                    escrow_raw,buyer_bond_raw,buyer_credit_raw,spent_raw)
                 VALUES('network-a','buyer-settlement:stop','settlement','d','model-a',?1,?1,?2,?3,?3,NULL)",
                params![chain_millis(2_000).unwrap(),expense.to_string(),SHELL_UNIT.to_string()],
            ).unwrap();
            connection.execute("UPDATE deals SET status='settled',closed_at=?1 WHERE network='network-a' AND deal_id='d'",
                params![chain_millis(2_000).unwrap()]).unwrap();
        }
        drop(s);
        store(dir, "network-a")
    }

    fn assert_old_and_fresh_history_partition(principal: u128, residual: u128) {
        let old_dir = tempfile::tempdir().unwrap();
        let fresh_dir = tempfile::tempdir().unwrap();
        let total = principal.checked_add(residual).unwrap();
        let old = old_writer_history(&old_dir, total);
        let before = serde_json::to_value(old.facts(&UsageFactsQuery::default()).unwrap()).unwrap();
        assert!(
            before["facts"].as_array().unwrap().is_empty(),
            "unprepared terminal must not be published: {before}"
        );
        assert_eq!(before["complete"], false);
        assert_eq!(
            old.query(&UsageQuery::default(), Utc::now()).unwrap()["spend"]["total"]
                ["actual_cost_shell"],
            shell(total)
        );
        let accrual = AccrualFact {
            source_id: "buyer-accrual:probe".into(),
            occurred_at: 1_000,
            spent_raw: principal,
        };
        let terminal = SettlementFact {
            source_id: "buyer-settlement:stop".into(),
            deal_id: "d".into(),
            occurred_at: 2_000,
            escrow_raw: total,
            buyer_bond_raw: SHELL_UNIT,
            buyer_credit_raw: Some(SHELL_UNIT),
        };
        // A stale open-deal snapshot must not add an accrual over the old full settlement amount.
        assert!(old
            .apply_deal_expenses("d", std::slice::from_ref(&accrual), None)
            .is_err());
        assert_eq!(
            old.query(&UsageQuery::default(), Utc::now()).unwrap()["spend"]["total"]
                ["actual_cost_shell"],
            shell(total)
        );
        old.apply_deal_expenses("d", std::slice::from_ref(&accrual), Some(terminal.clone()))
            .unwrap();
        let after = serde_json::to_value(old.facts(&UsageFactsQuery::default()).unwrap()).unwrap();
        assert_eq!(after["complete"], true);
        assert_eq!(after["facts"][0]["amount"], shell(principal));
        assert_eq!(after["facts"][1]["amount"], shell(residual));
        drop(old);
        let reopened = store(&old_dir, "network-a");
        reopened
            .apply_deal_expenses("d", std::slice::from_ref(&accrual), Some(terminal.clone()))
            .unwrap();
        assert_eq!(
            serde_json::to_value(reopened.facts(&UsageFactsQuery::default()).unwrap()).unwrap(),
            after
        );
        assert_eq!(
            reopened.query(&UsageQuery::default(), Utc::now()).unwrap()["spend"]["total"]
                ["actual_cost_shell"],
            shell(total)
        );

        let fresh = store(&fresh_dir, "network-a");
        fresh.register_deal("d", "model-a").unwrap();
        fresh
            .apply_deal_expenses("d", &[accrual], Some(terminal))
            .unwrap();
        fresh.mark_operations_complete("d").unwrap();
        let independent =
            serde_json::to_value(fresh.facts(&UsageFactsQuery::default()).unwrap()).unwrap();
        assert_eq!(independent, after);
        // Consumers keep facts from incomplete exports and combine independent histories in either order.
        for exports in [
            [&before, &after, &independent],
            [&independent, &after, &before],
        ] {
            let mut unique = BTreeMap::new();
            for export in exports {
                for fact in export["facts"].as_array().unwrap() {
                    let key = (
                        fact["network"].as_str().unwrap().to_owned(),
                        fact["source_id"].as_str().unwrap().to_owned(),
                    );
                    if let Some(previous) = unique.insert(key, fact["amount"].clone()) {
                        assert_eq!(previous, fact["amount"], "exported source amount changed");
                    }
                }
            }
            assert_eq!(
                unique,
                BTreeMap::from([
                    (
                        ("network-a".into(), "buyer-accrual:probe".into()),
                        json!(shell(principal))
                    ),
                    (
                        ("network-a".into(), "buyer-settlement:stop".into()),
                        json!(shell(residual))
                    ),
                ])
            );
        }
    }

    #[test]
    fn old_writer_settlement_never_exports_a_full_amount_under_the_residual_id() {
        assert_old_and_fresh_history_partition(SHELL_UNIT, 2 * SHELL_UNIT);
    }

    #[test]
    fn unprepared_settlement_keeps_other_facts_and_filtered_exports_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let s = old_writer_history(&dir, 3 * SHELL_UNIT);
        s.apply_operational_expense(operation(Some("model-a")))
            .unwrap();
        let filtered = UsageFactsQuery {
            from: Some("1970-01-01T00:16:40Z".into()),
            to: Some("1970-01-01T00:16:41Z".into()),
            ..Default::default()
        };
        for query in [UsageFactsQuery::default(), filtered] {
            let exported = s.facts(&query).unwrap();
            assert!(!exported.complete);
            assert_eq!(exported.facts.len(), 1);
            assert_eq!(exported.facts[0].source_id, "buyer-buy:shared-message");
            assert_eq!(
                exported.facts[0].amount,
                shell(dexdo_core::params::BUY_ORDER_GAS_RAW)
            );
            assert_eq!(exported.facts[0].model.as_deref(), Some("model-a"));
        }
        let other = UsageStore::open(dir.path().join("usage.sqlite3"), "network-b").unwrap();
        assert!(other.facts(&UsageFactsQuery::default()).unwrap().complete);
    }

    #[test]
    fn facts_export_all_available_history_with_network_id_exact_amount_and_chain_time() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.apply_operational_expense(operation(Some("model-a")))
            .unwrap();
        s.apply_operational_expense(operation(Some("model-a")))
            .unwrap();
        let exported = s.facts(&UsageFactsQuery::default()).unwrap();
        assert!(exported.complete);
        assert_eq!(exported.unit, "SHELL");
        assert_eq!(exported.facts.len(), 1);
        assert_eq!(
            serde_json::to_value(&exported).unwrap()["facts"][0]["cost"],
            "0"
        );
        let fact = &exported.facts[0];
        assert_eq!(fact.source_id, "buyer-buy:shared-message");
        assert_eq!(fact.network, "network-a");
        assert_eq!(fact.amount, shell(dexdo_core::params::BUY_ORDER_GAS_RAW));
        assert_eq!(fact.cost, "0");
        assert_eq!(fact.occurred_at, "1970-01-01T00:16:40+00:00");
        assert_eq!(fact.model.as_deref(), Some("model-a"));
        let isolated = UsageStore::open(dir.path().join("usage.sqlite3"), "network-b").unwrap();
        assert!(isolated
            .facts(&UsageFactsQuery::default())
            .unwrap()
            .facts
            .is_empty());
        drop(s);
        assert_eq!(
            store(&dir, "network-a")
                .facts(&UsageFactsQuery::default())
                .unwrap()
                .facts[0]
                .source_id,
            fact.source_id
        );
    }

    #[test]
    fn facts_reject_invalid_ranges_and_filter_by_half_open_chain_time() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.apply_operational_expense(operation(None)).unwrap();
        let at = "1970-01-01T00:16:40Z".to_owned();
        for query in [
            UsageFactsQuery {
                from: Some("invalid".into()),
                to: None,
                ..Default::default()
            },
            UsageFactsQuery {
                from: Some(at.clone()),
                to: Some(at.clone()),
                ..Default::default()
            },
            UsageFactsQuery {
                from: Some("1970-01-02T00:00:00Z".into()),
                to: Some(at.clone()),
                ..Default::default()
            },
        ] {
            assert!(s.facts(&query).is_err());
        }
        assert_eq!(
            s.facts(&UsageFactsQuery {
                from: Some(at.clone()),
                to: None,
                ..Default::default()
            })
            .unwrap()
            .facts
            .len(),
            1
        );
        assert!(s
            .facts(&UsageFactsQuery {
                from: None,
                to: Some(at),
                ..Default::default()
            })
            .unwrap()
            .facts
            .is_empty());
    }

    #[test]
    fn sub_millisecond_bounds_do_not_include_or_drop_a_neighboring_fact() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.apply_operational_expense(operation(None)).unwrap();
        let just_after = "1970-01-01T00:16:40.000001Z".to_owned();
        assert!(s
            .facts(&UsageFactsQuery {
                from: Some(just_after.clone()),
                to: None,
                ..Default::default()
            })
            .unwrap()
            .facts
            .is_empty());
        assert_eq!(
            s.facts(&UsageFactsQuery {
                from: None,
                to: Some(just_after.clone()),
                ..Default::default()
            })
            .unwrap()
            .facts
            .len(),
            1
        );
        assert!(s
            .facts(&UsageFactsQuery {
                from: Some(just_after),
                to: Some("1970-01-01T00:16:40.000002Z".into()),
                ..Default::default()
            })
            .unwrap()
            .facts
            .is_empty());
    }

    #[test]
    fn model_enrichment_keeps_the_same_fact_and_never_overwrites_known_identity_or_money() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.apply_operational_expense(operation(None)).unwrap();
        s.apply_operational_expense(operation(Some("model-a")))
            .unwrap();
        s.apply_operational_expense(operation(None)).unwrap();
        assert!(s
            .apply_operational_expense(operation(Some("model-b")))
            .is_err());
        let mut changed = operation(Some("model-a"));
        changed.spent_raw += 1;
        assert!(s.apply_operational_expense(changed).is_err());
        let facts = s.facts(&UsageFactsQuery::default()).unwrap();
        assert_eq!(facts.facts.len(), 1);
        assert_eq!(facts.facts[0].model.as_deref(), Some("model-a"));
        assert_eq!(
            facts.facts[0].amount,
            shell(dexdo_core::params::BUY_ORDER_GAS_RAW)
        );
    }

    #[test]
    fn concurrent_connections_cannot_overwrite_a_proven_fact_model() {
        let dir = tempfile::tempdir().unwrap();
        let a = store(&dir, "network-a");
        let b = store(&dir, "network-a");
        a.apply_operational_expense(operation(None)).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers = [(a.clone(), "model-a"), (b, "model-b")]
            .into_iter()
            .map(|(store, model)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .apply_operational_expense(operation(Some(model)))
                        .is_ok()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|ok| *ok)
                .count(),
            1
        );
        let facts = a.facts(&UsageFactsQuery::default()).unwrap();
        assert_eq!(facts.facts.len(), 1);
        assert!(matches!(
            facts.facts[0].model.as_deref(),
            Some("model-a" | "model-b")
        ));
    }

    #[test]
    fn incomplete_sources_keep_confirmed_facts_without_inventing_a_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.apply_operational_expense(operation(None)).unwrap();
        s.register_deal("deal", "model-a").unwrap();
        s.apply_settlement(SettlementFact {
            source_id: "settlement:deal".into(),
            deal_id: "deal".into(),
            occurred_at: 1_000,
            escrow_raw: 3 * SHELL_UNIT,
            buyer_bond_raw: SHELL_UNIT,
            buyer_credit_raw: None,
        })
        .unwrap();
        let pending = s.facts(&UsageFactsQuery::default()).unwrap();
        assert!(!pending.complete);
        assert_eq!(pending.facts.len(), 1);
        assert!(pending.facts[0].model.is_none());
        s.apply_settlement(SettlementFact {
            source_id: "settlement:deal".into(),
            deal_id: "deal".into(),
            occurred_at: 1_000,
            escrow_raw: 3 * SHELL_UNIT,
            buyer_bond_raw: SHELL_UNIT,
            buyer_credit_raw: Some(SHELL_UNIT),
        })
        .unwrap();
        s.mark_operations_complete("deal").unwrap();
        assert!(s.facts(&UsageFactsQuery::default()).unwrap().complete);
        assert_eq!(s.facts(&UsageFactsQuery::default()).unwrap().facts.len(), 2);
        s.mark_note_operations_incomplete("note", "history missing")
            .unwrap();
        assert!(!s.facts(&UsageFactsQuery::default()).unwrap().complete);
        s.mark_note_operations_complete("note").unwrap();
        assert!(s.facts(&UsageFactsQuery::default()).unwrap().complete);
    }

    #[test]
    fn export_preserves_the_full_uint128_amount() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        let mut fact = operation(None);
        fact.spent_raw = u128::MAX;
        s.apply_operational_expense(fact).unwrap();
        assert_eq!(
            s.facts(&UsageFactsQuery::default()).unwrap().facts[0].amount,
            "340282366920938463463374607431.768211455"
        );
        let other = store(&dir, "network-b");
        other.register_deal("d", "model-a").unwrap();
        other
            .apply_settlement(SettlementFact {
                source_id: "settled".into(),
                deal_id: "d".into(),
                occurred_at: 1_000,
                escrow_raw: u128::MAX,
                buyer_bond_raw: 0,
                buyer_credit_raw: Some(0),
            })
            .unwrap();
        other
            .classify_expense(
                "settled",
                ExpenseBreakdown {
                    consumption_raw: u128::MAX - 1,
                    ..Default::default()
                },
            )
            .unwrap();
        let exported = other.facts(&UsageFactsQuery::default()).unwrap();
        assert_eq!(
            exported.facts[0].cost,
            "340282366920938463463374607431.768211454"
        );
        assert_eq!(
            exported.facts[0].amount,
            "340282366920938463463374607431.768211455"
        );
    }

    #[test]
    fn consumption_enrichment_2142_deduplicates_without_a_stale_zero_erasing_cost() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let stores = [store(&a, "network-a"), store(&b, "network-a")];
        let export = |s: &UsageStore| {
            serde_json::to_value(s.facts(&UsageFactsQuery::default()).unwrap()).unwrap()
        };
        for s in &stores {
            s.register_deal("d", "model-a").unwrap();
            s.apply_deal_expenses(
                "d",
                &[AccrualFact {
                    source_id: "buyer-accrual:shared".into(),
                    occurred_at: 1_000,
                    spent_raw: SHELL_UNIT + SHELL_UNIT / 40,
                }],
                None,
            )
            .unwrap();
        }
        let unclassified = export(&stores[0]);
        assert_eq!(unclassified["facts"][0]["cost"], "0");
        let parts = ExpenseBreakdown {
            consumption_raw: SHELL_UNIT,
            commission_raw: SHELL_UNIT / 40,
            ..Default::default()
        };
        stores[0]
            .classify_expense("buyer-accrual:shared", parts)
            .unwrap();
        let classified = export(&stores[0]);
        assert_eq!(classified["facts"][0]["cost"], "1");
        assert_eq!(classified["facts"][0]["amount"], "1.025");
        assert_eq!(classified["complete"], false);
        assert_eq!(export(&stores[0]), classified);
        let reopened = store(&a, "network-a");
        assert_eq!(export(&reopened), classified);
        let stale = export(&stores[1]);
        stores[1]
            .classify_expense("buyer-accrual:shared", parts)
            .unwrap();
        assert_eq!(export(&stores[1]), classified);

        // Example consumer merge: zero is no consumption proof; unequal positive splits conflict.
        let merge = |exports: &[&Value]| -> Result<_> {
            let mut unique: BTreeMap<_, (String, String)> = BTreeMap::new();
            for export in exports {
                for fact in export["facts"].as_array().unwrap() {
                    let key = (
                        fact["network"].as_str().unwrap().to_owned(),
                        fact["source_id"].as_str().unwrap().to_owned(),
                    );
                    let amount = fact["amount"].as_str().unwrap();
                    let cost = fact["cost"].as_str().unwrap();
                    if let Some(known) = unique.get_mut(&key) {
                        ensure!(known.0 == amount, "conflicting amount");
                        if known.1 == "0" {
                            known.1 = cost.to_owned();
                        } else {
                            ensure!(cost == "0" || known.1 == cost, "conflicting consumption");
                        }
                    } else {
                        unique.insert(key, (amount.to_owned(), cost.to_owned()));
                    }
                }
            }
            Ok(unique)
        };
        for exports in [
            [&unclassified, &classified, &stale],
            [&stale, &classified, &unclassified],
            [&classified, &stale, &classified],
        ] {
            let unique = merge(&exports).unwrap();
            assert_eq!(unique.len(), 1);
            assert_eq!(
                unique.values().next().unwrap(),
                &("1.025".into(), "1".into())
            );
        }
        stores[1]
            .classify_expense(
                "buyer-accrual:shared",
                ExpenseBreakdown {
                    consumption_raw: SHELL_UNIT / 2,
                    ..Default::default()
                },
            )
            .unwrap();
        let conflicting = export(&stores[1]);
        assert!(merge(&[&classified, &conflicting]).is_err());
        assert!(merge(&[&conflicting, &classified]).is_err());
    }

    #[test]
    fn consumption_export_2142_rejects_invalid_persisted_cost() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.register_deal("d", "model-a").unwrap();
        s.apply_deal_expenses(
            "d",
            &[AccrualFact {
                source_id: "paid".into(),
                occurred_at: 1_000,
                spent_raw: 1,
            }],
            None,
        )
        .unwrap();
        for invalid in [
            "2",
            "-1",
            "not-a-number",
            "340282366920938463463374607431768211456",
        ] {
            s.lock()
                .unwrap()
                .execute(
                    "UPDATE money_facts SET consumption_raw=?1 WHERE source_id='paid'",
                    [invalid],
                )
                .unwrap();
            assert!(s.facts(&UsageFactsQuery::default()).is_err());
        }
    }

    proptest! {
        #[test]
        fn consumption_export_2142_preserves_uint128_bounds_and_reconciles_with_summary(
            amount in any::<u128>(), candidate in any::<u128>(),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let s = store(&dir, "network-a");
            let cost = candidate.min(amount);
            s.register_deal("d", "model-a").unwrap();
            s.apply_settlement(SettlementFact {
                source_id: "settled".into(), deal_id: "d".into(), occurred_at: 1_000,
                escrow_raw: amount, buyer_bond_raw: 0, buyer_credit_raw: Some(0),
            }).unwrap();
            s.classify_expense("settled", ExpenseBreakdown {
                consumption_raw: cost, ..Default::default()
            }).unwrap();
            let exported = s.facts(&UsageFactsQuery::default()).unwrap();
            prop_assert_eq!(&exported.facts[0].amount, &shell(amount));
            prop_assert_eq!(&exported.facts[0].cost, &shell(cost));
            let summary = s.query(&UsageQuery::default(), Utc::now()).unwrap();
            prop_assert_eq!(&summary["spend"]["total"]["cost_shell"], &json!(shell(cost)));
            prop_assert_eq!(&summary["spend"]["total"]["overhead_cost_shell"], "0");
            prop_assert_eq!(&summary["spend"]["total"]["expense_breakdown"]["other_shell"], &json!(shell(amount - cost)));
        }

        #[test]
        fn old_and_fresh_histories_keep_canonical_fact_amounts_in_both_merge_orders(
            principal in 1u64..u64::MAX, residual in 0u64..u64::MAX,
        ) {
            assert_old_and_fresh_history_partition(u128::from(principal),u128::from(residual));
        }

        #[test]
        fn accrual_and_terminal_residual_conserve_expense_across_histories(
            principal in 1u64..1_000_000, remainder in 0u64..1_000_000,
            consumed in 0u64..1_000_000, residual_consumed in 0u64..1_000_000,
        ) {
            let a=tempfile::tempdir().unwrap();
            let b=tempfile::tempdir().unwrap();
            let stores=[store(&a,"network-a"),store(&b,"network-a")];
            let accrued=AccrualFact {source_id:"buyer-accrual:probe".into(),occurred_at:1_000,
                spent_raw:u128::from(principal)};
            let final_fact=SettlementFact {source_id:"buyer-settlement:stop".into(),deal_id:"d".into(),
                occurred_at:2_000,escrow_raw:u128::from(principal)+u128::from(remainder),
                buyer_bond_raw:100,buyer_credit_raw:Some(100)};
            let mut unique=std::collections::BTreeMap::new();
            for (i,s) in stores.iter().enumerate() {
                s.register_deal("d","model-a").unwrap();
                if i == 0 {
                    s.apply_deal_expenses("d",std::slice::from_ref(&accrued),None).unwrap();
                    s.classify_expense("buyer-accrual:probe", ExpenseBreakdown {
                        consumption_raw: u128::from(consumed.min(principal)), ..Default::default()
                    }).unwrap();
                    let open=s.facts(&UsageFactsQuery::default()).unwrap();
                    prop_assert!(!open.complete);
                    for fact in open.facts {unique.insert((fact.network,fact.source_id),(fact.amount, fact.cost));}
                }
                // A buyer discovering an already closed deal derives exactly the same partition.
                for _ in 0..2 {
                    s.apply_deal_expenses("d",std::slice::from_ref(&accrued),Some(final_fact.clone())).unwrap();
                    s.classify_expense("buyer-accrual:probe", ExpenseBreakdown {
                        consumption_raw: u128::from(consumed.min(principal)), ..Default::default()
                    }).unwrap();
                    s.classify_expense("buyer-settlement:stop", ExpenseBreakdown {
                        consumption_raw: u128::from(residual_consumed.min(remainder)), ..Default::default()
                    }).unwrap();
                    s.mark_operations_complete("d").unwrap();
                    let closed=s.facts(&UsageFactsQuery::default()).unwrap();
                    prop_assert!(closed.complete);
                    for fact in closed.facts {
                        prop_assert_eq!(fact.model.as_deref(),Some("model-a"));
                        let money = (fact.amount, fact.cost);
                        if let Some(old)=unique.insert((fact.network,fact.source_id),money.clone()) {
                            prop_assert_eq!(old,money);
                        }
                    }
                }
                let summary=s.query(&UsageQuery::default(),Utc::now()).unwrap();
                prop_assert_eq!(&summary["spend"]["total"]["actual_cost_shell"],
                    &json!(shell(u128::from(principal)+u128::from(remainder))));
                let cost = u128::from(consumed.min(principal)) + u128::from(residual_consumed.min(remainder));
                prop_assert_eq!(&summary["spend"]["total"]["cost_shell"], &json!(shell(cost)));
                prop_assert_eq!(&summary["spend"]["total"]["overhead_cost_shell"],
                    "0");
                prop_assert_eq!(&summary["spend"]["total"]["expense_breakdown"]["other_shell"],
                    &json!(shell(u128::from(principal) + u128::from(remainder) - cost)));
            }
            prop_assert_eq!(unique.len(),2);
            prop_assert_eq!(&unique[&("network-a".into(),"buyer-accrual:probe".into())],
                &(shell(u128::from(principal)), shell(u128::from(consumed.min(principal)))));
            prop_assert_eq!(&unique[&("network-a".into(),"buyer-settlement:stop".into())],
                &(shell(u128::from(remainder)), shell(u128::from(residual_consumed.min(remainder)))));
        }

        #[test]
        fn shared_note_facts_deduplicate_across_histories_and_survive_model_enrichment(amount in 0u64..u64::MAX) {
            let a=tempfile::tempdir().unwrap();
            let b=tempfile::tempdir().unwrap();
            let stores=[store(&a,"network-a"),store(&b,"network-a")];
            let mut unique=std::collections::BTreeMap::new();
            for (index,s) in stores.iter().enumerate() {
                let mut fact=operation((index==1).then_some("model-a"));
                fact.spent_raw=u128::from(amount);
                s.apply_operational_expense(fact).unwrap();
                let output=s.facts(&UsageFactsQuery::default()).unwrap();
                prop_assert!(output.complete);
                for fact in output.facts {
                    let key=(fact.network,fact.source_id);
                    if let Some(old)=unique.insert(key,fact.amount.clone()) {
                        prop_assert_eq!(old,fact.amount);
                    }
                }
            }
            prop_assert_eq!(unique.len(),1);
            prop_assert_eq!(unique.values().next().unwrap(),&shell(u128::from(amount)));
        }
    }

    #[test]
    fn invalid_accruals_roll_back_and_cannot_change_an_exported_partition() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.register_deal("d", "m").unwrap();
        let accrued = AccrualFact {
            source_id: "probe".into(),
            occurred_at: 1_000,
            spent_raw: 10,
        };
        let mut zero = accrued.clone();
        zero.spent_raw = 0;
        assert!(s.apply_deal_expenses("d", &[zero], None).is_err());
        let terminal = SettlementFact {
            source_id: "stop".into(),
            deal_id: "d".into(),
            occurred_at: 2_000,
            escrow_raw: 20,
            buyer_bond_raw: 5,
            buyer_credit_raw: Some(20),
        };
        assert!(s
            .apply_deal_expenses("d", std::slice::from_ref(&accrued), Some(terminal.clone()))
            .is_err());
        assert!(s
            .facts(&UsageFactsQuery::default())
            .unwrap()
            .facts
            .is_empty());
        let mut terminal = terminal;
        terminal.buyer_credit_raw = Some(5);
        s.apply_deal_expenses("d", std::slice::from_ref(&accrued), Some(terminal))
            .unwrap();
        let before = serde_json::to_value(s.facts(&UsageFactsQuery::default()).unwrap()).unwrap();
        let mut changed = accrued.clone();
        changed.spent_raw += 1;
        assert!(s.apply_deal_expenses("d", &[changed], None).is_err());
        let mut changed = accrued.clone();
        changed.occurred_at += 1;
        assert!(s.apply_deal_expenses("d", &[changed], None).is_err());
        let mut late = accrued;
        late.source_id = "late".into();
        assert!(s.apply_deal_expenses("d", &[late], None).is_err());
        assert_eq!(
            serde_json::to_value(s.facts(&UsageFactsQuery::default()).unwrap()).unwrap(),
            before
        );
        drop(s);
        let s = store(&dir, "network-a");
        assert_eq!(
            serde_json::to_value(s.facts(&UsageFactsQuery::default()).unwrap()).unwrap(),
            before
        );
    }

    #[test]
    fn open_deal_accruals_keep_their_original_model_and_period_after_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir, "network-a");
        s.register_deal("d", "model-a").unwrap();
        let accrual = AccrualFact {
            source_id: "probe".into(),
            occurred_at: 1_000,
            spent_raw: SHELL_UNIT,
        };
        s.apply_deal_expenses("d", std::slice::from_ref(&accrual), None)
            .unwrap();
        s.classify_expense(
            "probe",
            ExpenseBreakdown {
                consumption_raw: SHELL_UNIT * 3 / 4,
                commission_raw: SHELL_UNIT / 4,
                ..Default::default()
            },
        )
        .unwrap();
        let query = UsageQuery {
            from: Some("1970-01-01T00:16:40Z".into()),
            to: Some("1970-01-01T00:16:41Z".into()),
            ..Default::default()
        };
        let open = s.query(&query, Utc::now()).unwrap();
        assert_eq!(open["model_spend"][0]["model"], "model-a");
        assert_eq!(open["model_spend"][0]["actual_cost_shell"], "1");
        assert_eq!(open["spend"]["total"]["complete"], false);
        assert_eq!(open["spend"]["period"]["cost_shell"], "0.75");
        let filtered = UsageFactsQuery {
            from: query.from.clone(),
            to: query.to.clone(),
            ..Default::default()
        };
        assert_eq!(s.facts(&filtered).unwrap().facts[0].cost, "0.75");
        s.apply_settlement(SettlementFact {
            source_id: "stop".into(),
            deal_id: "d".into(),
            occurred_at: 2_000,
            escrow_raw: 3 * SHELL_UNIT,
            buyer_bond_raw: SHELL_UNIT,
            buyer_credit_raw: None,
        })
        .unwrap();
        let missing_refund = s.facts(&UsageFactsQuery::default()).unwrap();
        assert!(!missing_refund.complete);
        assert_eq!(missing_refund.facts.len(), 1);
        assert_eq!(missing_refund.facts[0].amount, "1");
        assert_eq!(missing_refund.facts[0].cost, "0.75");
        s.apply_deal_expenses(
            "d",
            &[accrual],
            Some(SettlementFact {
                source_id: "stop".into(),
                deal_id: "d".into(),
                occurred_at: 2_000,
                escrow_raw: 3 * SHELL_UNIT,
                buyer_bond_raw: SHELL_UNIT,
                buyer_credit_raw: Some(SHELL_UNIT),
            }),
        )
        .unwrap();
        let unclassified = s.facts(&UsageFactsQuery::default()).unwrap();
        assert_eq!(unclassified.facts[1].cost, "0");
        s.classify_expense(
            "stop",
            ExpenseBreakdown {
                consumption_raw: SHELL_UNIT / 2,
                burns_raw: SHELL_UNIT / 2,
                unused_capacity_raw: SHELL_UNIT / 2,
                ..Default::default()
            },
        )
        .unwrap();
        let closed = s.query(&query, Utc::now()).unwrap();
        assert_eq!(closed["model_spend"][0]["actual_cost_shell"], "1");
        assert_eq!(closed["spend"]["total"]["actual_cost_shell"], "3");
        assert_eq!(closed["spend"]["total"]["cost_shell"], "1.25");
        assert_eq!(closed["spend"]["total"]["overhead_cost_shell"], "1.25");
        assert_eq!(
            closed["spend"]["total"]["expense_breakdown"]["other_shell"],
            "0.5"
        );
        let facts = s.facts(&UsageFactsQuery::default()).unwrap();
        assert_eq!(facts.facts[0].cost, "0.75");
        assert_eq!(facts.facts[1].amount, "2");
        assert_eq!(facts.facts[1].cost, "0.5");
        let filtered = s.facts(&filtered).unwrap();
        assert_eq!(filtered.facts.len(), 1);
        assert_eq!(
            filtered.facts[0].cost,
            closed["spend"]["period"]["cost_shell"]
        );
    }
}
