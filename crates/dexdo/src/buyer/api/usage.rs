//! HTTP history admission and the durable terminal records of the existing consumer lifecycle.

use super::{ApiState, RequestDelivery};
use crate::buyer::usage::{UsageFactsCursorExpired, UsageFactsQuery, UsageQuery, UsageStore};
use anyhow::{Context, Result};
use axum::{
    extract::{Query, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use http::StatusCode;
use serde_json::{value::RawValue, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[cfg(test)]
#[path = "usage_live_cost_tests.rs"]
mod live_cost_tests;

#[derive(Clone)]
struct RequestContext {
    id: String,
    store: Arc<UsageStore>,
    armed: Arc<AtomicBool>,
    model: String,
}

tokio::task_local! {
    static REQUEST: RequestContext;
}

pub(super) fn current_request_id() -> Option<String> {
    REQUEST.try_with(|request| request.id.clone()).ok()
}

struct Admission(RequestContext);

impl Drop for Admission {
    fn drop(&mut self) {
        if !self.0.armed.swap(true, Ordering::SeqCst) {
            if let Err(error) = self.0.store.finish_request(
                &self.0.id,
                super::CLIENT_CLOSED_REQUEST_STATUS,
                Some("cancelled"),
                Some("consumer_disconnect"),
            ) {
                tracing::error!(%error, "buyer usage: could not persist cancelled admission");
            }
        }
    }
}

pub(super) async fn track_request(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(store) = state.usage.clone() else {
        return next.run(request).await;
    };
    if request.method() != http::Method::POST {
        return next.run(request).await;
    }
    let protocol = match request.uri().path() {
        "/v1/chat/completions" => "openai",
        "/v1/responses" => "openai_responses",
        "/v1/messages" => "anthropic",
        _ => return next.run(request).await,
    };
    let id = super::consumer_request_id();
    if let Err(error) = store.start_request(&id, &state.frame_model, protocol, false) {
        tracing::error!(%error, "buyer usage: history admission failed");
        return super::attach_request_id(unavailable("usage history could not be persisted"), &id);
    }
    let context = RequestContext {
        id: id.clone(),
        store,
        armed: Arc::new(AtomicBool::new(false)),
        model: state.frame_model,
    };
    let admission = Admission(context.clone());
    let response = REQUEST.scope(context.clone(), next.run(request)).await;
    if !context.armed.swap(true, Ordering::SeqCst) {
        if let Err(error) = context.store.finish_request(
            &id,
            response.status().as_u16(),
            Some("error"),
            Some("request_rejected"),
        ) {
            tracing::error!(%error, "buyer usage: could not persist rejected request");
        }
    }
    drop(admission);
    super::attach_request_id(response, &id)
}

pub(super) fn arm_request(id: &str, deal: &str, streamed: bool) -> Result<Option<Arc<UsageStore>>> {
    REQUEST
        .try_with(|request| {
            // No billable work may start before its durable request-to-deal binding exists.
            request.store.register_deal(deal, &request.model)?;
            request.store.link_deal(id, deal)?;
            request.store.set_request_streamed(id, streamed)?;
            request.armed.store(true, Ordering::SeqCst);
            Ok(Some(request.store.clone()))
        })
        .unwrap_or(Ok(None))
}

pub(super) fn finish_delivery(
    store: Option<&UsageStore>,
    delivery: &RequestDelivery,
    history_complete: bool,
) {
    if let Some(store) = store {
        if !history_complete {
            if let Err(error) = store.mark_request_history_incomplete(&delivery.request_id) {
                tracing::error!(%error, "buyer usage: history remains unfinished after a write failure");
                return;
            }
        }
        if let Err(error) = store.finish_request(
            &delivery.request_id,
            delivery.http_status,
            Some(delivery.finish_reason.as_str()),
            delivery.failure_stage,
        ) {
            tracing::error!(%error, "buyer usage: could not persist request completion");
        }
    }
}

pub(super) fn unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": {"message": message, "type": "usage_unavailable"}
        })),
    )
        .into_response()
}

fn invalid_query(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": {"message": message, "type": "invalid_usage_query"}
        })),
    )
        .into_response()
}

/// Read the current serving route without triggering a lazy purchase. Keep the route read lock
/// through persistence so a concurrent replacement cannot publish the previous quota afterwards.
pub(super) async fn sync_deal_quota(state: &ApiState) -> Result<()> {
    let Some(store) = state.usage.as_ref() else {
        return Ok(());
    };
    let active = state.deals.active.read().await;
    let Some(deal) = active.as_ref() else {
        return Ok(());
    };
    persist_deal_quota(deal, store).await
}

#[derive(Debug)]
struct QuotaNotOpened;

impl std::fmt::Display for QuotaNotOpened {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("current deal is funded but not yet open")
    }
}

impl std::error::Error for QuotaNotOpened {}

pub(super) async fn persist_deal_quota(
    deal: &super::ApiDeal,
    store: &Arc<UsageStore>,
) -> Result<()> {
    let id = &deal.route.token_contract;
    // Even a failed lookup must not prevent capturing terms before TC destruction.
    let saved = match store.has_deal_quota(id) {
        Ok(saved) => saved,
        Err(error) => {
            tracing::warn!(%error, "buyer usage: retrying purchase metadata after quota lookup failed");
            false
        }
    };
    if !saved {
        let (ticks, price) =
            deal.purchase_terms
                .get_or_try_init(|| async {
                    // This read can fail after the purchase opened. Preserve the unresolved
                    // route before awaiting it, without replacing the last confirmed quota.
                    store.begin_deal_quota(id)?;
                    let state = deal.session.chain.deal_state(id).await?.context(
                        "current deal state unavailable before purchase terms were saved",
                    )?;
                    if super::unopened_cleanup_decision(state, super::unix_now_secs()).is_some() {
                        store.discard_unopened_deal_quota(id)?;
                        return Err(QuotaNotOpened.into());
                    }
                    anyhow::ensure!(
                        state.opened,
                        "current deal is not open and has no saved purchase terms"
                    );
                    let _ = deal.session.usage_quota.set(store.clone());
                    deal.session
                        .chain
                        .deal_purchase_terms(id)
                        .await?
                        .context("opened deal purchase terms unavailable")
                })
                .await?;
        store.open_deal_quota(id, *ticks, *price)?;
    }
    let _ = deal.session.usage_quota.set(store.clone());
    let terminal = deal
        .session
        .diagnostics
        .lock()
        .expect("serving diagnostics lock poisoned")
        .known_chain_state
        .is_some();
    if deal.session.is_settled() || terminal {
        store.close_deal_quota(id)?;
    }
    Ok(())
}

// The gateway envelope stays flat. Exact monetary decimals are serialized as JSON numbers
// directly, without passing through f64 or changing serde_json's global number representation.
#[derive(Debug, serde::Serialize)]
struct GatewayRow {
    #[serde(flatten)]
    fields: Map<String, Value>,
    cost: Option<Box<RawValue>>,
    overhead_cost: Option<Box<RawValue>>,
}

#[derive(Debug, serde::Serialize)]
struct GatewayQuota {
    limit: Box<RawValue>,
    used: Box<RawValue>,
    remaining: Box<RawValue>,
    unit: &'static str,
}

#[derive(Debug, serde::Serialize)]
struct GatewayUsage {
    today: GatewayRow,
    total: GatewayRow,
}

#[derive(Debug, serde::Serialize)]
struct GatewayResponse {
    mode: &'static str,
    #[serde(rename = "isValid")]
    is_valid: bool,
    quota: GatewayQuota,
    remaining: Box<RawValue>,
    unit: &'static str,
    usage: GatewayUsage,
    daily_usage: Vec<GatewayRow>,
    model_stats: Vec<GatewayRow>,
}

fn exact_amount(spend: &Value, key: &str) -> Result<Option<Box<RawValue>>> {
    match spend
        .get(key)
        .with_context(|| format!("usage spend has no {key}"))?
    {
        Value::Null => Ok(None),
        Value::String(amount) => Ok(Some(RawValue::from_string(amount.clone())?)),
        _ => anyhow::bail!("usage actual cost is not an exact decimal string"),
    }
}

fn gateway_row(summary: &Value, spend: &Value, daily: bool) -> Result<GatewayRow> {
    let mut fields = Map::new();
    fields.insert("requests".into(), summary["requests"]["total"].clone());
    for key in [
        "input_tokens",
        "output_tokens",
        "total_tokens",
        "cache_read_tokens",
    ] {
        fields.insert(key.into(), summary["total"]["accepted"][key].clone());
    }
    fields.insert(
        if daily {
            "cache_write_tokens"
        } else {
            "cache_creation_tokens"
        }
        .into(),
        summary["total"]["accepted"]["cache_write_tokens"].clone(),
    );
    for key in ["date", "model"] {
        if let Some(value) = summary.get(key) {
            fields.insert(key.into(), value.clone());
        }
    }
    Ok(GatewayRow {
        fields,
        cost: exact_amount(spend, "cost_shell")?,
        overhead_cost: exact_amount(spend, "overhead_cost_shell")?,
    })
}

fn gateway_response(details: Value) -> Result<GatewayResponse> {
    let usage = GatewayUsage {
        today: gateway_row(
            &details["usage"]["today"],
            &details["spend"]["today"],
            false,
        )?,
        total: gateway_row(
            &details["usage"]["total"],
            &details["spend"]["total"],
            false,
        )?,
    };
    let daily_money: BTreeMap<_, _> = details["daily_spend"]
        .as_array()
        .context("usage daily spend is not an array")?
        .iter()
        .map(|row| {
            Ok((
                row["date"].as_str().context("usage spend date absent")?,
                row,
            ))
        })
        .collect::<Result<_>>()?;
    let daily_usage = details["daily_usage"]
        .as_array()
        .context("usage daily counters are not an array")?
        .iter()
        .map(|row| {
            let date = row["date"].as_str().context("usage date absent")?;
            gateway_row(
                row,
                daily_money.get(date).context("usage day cost absent")?,
                true,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let token_models: BTreeMap<_, _> = details["model_stats"]
        .as_array()
        .context("usage model counters are not an array")?
        .iter()
        .map(|row| Ok((row["model"].as_str().context("usage model absent")?, row)))
        .collect::<Result<_>>()?;
    let money_models: BTreeMap<_, _> = details["model_spend"]
        .as_array()
        .context("usage model spend is not an array")?
        .iter()
        .map(|row| {
            Ok((
                row["model"].as_str().context("usage cost model absent")?,
                row,
            ))
        })
        .collect::<Result<_>>()?;
    let models: BTreeSet<_> = token_models
        .keys()
        .chain(money_models.keys())
        .copied()
        .collect();
    let empty_tokens = serde_json::json!({"requests":{"total":0},"total":{"accepted":{
        "input_tokens":0,"output_tokens":0,"total_tokens":0,
        "cache_read_tokens":0,"cache_write_tokens":0
    }}});
    let empty_money = serde_json::json!({"cost_shell":"0","overhead_cost_shell":"0"});
    let model_stats = models
        .into_iter()
        .map(|model| {
            let mut row = gateway_row(
                token_models.get(model).copied().unwrap_or(&empty_tokens),
                money_models.get(model).copied().unwrap_or(&empty_money),
                false,
            )?;
            row.fields
                .insert("model".into(), Value::String(model.into()));
            Ok(row)
        })
        .collect::<Result<Vec<_>>>()?;
    let quota_amount = |key: &str| -> Result<Box<RawValue>> {
        RawValue::from_string(
            details["quota"][key]
                .as_str()
                .context("quota amount absent")?
                .to_owned(),
        )
        .map_err(Into::into)
    };
    Ok(GatewayResponse {
        mode: "quota_limited",
        is_valid: true,
        quota: GatewayQuota {
            limit: quota_amount("limit")?,
            used: quota_amount("used")?,
            remaining: quota_amount("remaining")?,
            unit: "SHELL",
        },
        remaining: quota_amount("remaining")?,
        unit: "SHELL",
        usage,
        daily_usage,
        model_stats,
    })
}

pub(super) async fn get_usage(
    State(state): State<ApiState>,
    query: Result<Query<UsageQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(error) => return invalid_query(error.to_string()),
    };
    let Some(store) = state.usage.clone() else {
        return unavailable("usage history is not configured");
    };
    let now = Utc::now();
    if let Err(error) = query.validate(now) {
        return invalid_query(error.to_string());
    }
    if let Err(error) = sync_deal_quota(&state).await {
        // An authoritatively unopened candidate leaves the last confirmed quota readable.
        // Serving still rejects this error; unresolved purchases and storage errors stay fatal.
        if !error.is::<QuotaNotOpened>() {
            tracing::error!(%error, "buyer usage: current quota could not be persisted");
            return unavailable("current deal quota could not be read");
        }
    }
    match tokio::task::spawn_blocking(move || gateway_response(store.query(&query, now)?)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        error => {
            tracing::error!(?error, "buyer usage: history query failed");
            unavailable("usage history could not be read")
        }
    }
}

pub(super) async fn get_facts(
    State(state): State<ApiState>,
    query: Result<Query<UsageFactsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(error) => return invalid_query(error.to_string()),
    };
    if let Err(error) = query.validate() {
        return invalid_query(error.to_string());
    }
    let Some(store) = state.usage else {
        return unavailable("usage history is not configured");
    };
    match tokio::task::spawn_blocking(move || store.facts(&query)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) if error.is::<UsageFactsCursorExpired>() => invalid_query(error.to_string()),
        error => {
            tracing::error!(?error, "buyer usage: financial facts query failed");
            unavailable("usage facts could not be read")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buyer::usage::{OperationalFact, SettlementFact};
    use dexdo_core::params::SHELL_UNIT;
    use dexdo_proto::BillingUsage;

    fn fixture() -> (tempfile::TempDir, UsageStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        let started = Utc::now().timestamp_millis() - 1_000;
        store
            .start_request_at("r", "model-a", "openai", false, started)
            .unwrap();
        store.register_deal("d", "model-a").unwrap();
        store.open_deal_quota("d", 3, SHELL_UNIT).unwrap();
        store.link_deal("r", "d").unwrap();
        let mut answer = BillingUsage::new(2, 3, None).unwrap();
        answer.charged_tokens = Some(4);
        store
            .record_call_usage("r", "answer", "answer", answer, true)
            .unwrap();
        store
            .record_call_usage(
                "r",
                "verification_b8",
                "b8",
                BillingUsage::new(1, 1, None).unwrap(),
                true,
            )
            .unwrap();
        store
            .finish_request_at("r", 200, Some("stop"), None, started + 500)
            .unwrap();
        (dir, store)
    }

    fn snapshot(store: &UsageStore) -> Value {
        store
            .query(
                &UsageQuery {
                    timezone: Some("UTC".into()),
                    days: Some(1),
                    ..Default::default()
                },
                Utc::now(),
            )
            .unwrap()
    }

    // This consumer follows the PDF's flat fields; no buyer extension is returned.
    #[derive(serde::Deserialize)]
    struct PdfRow {
        requests: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        total_tokens: Option<u64>,
        cost: Option<Box<RawValue>>,
        overhead_cost: Option<Box<RawValue>>,
    }
    #[derive(serde::Deserialize)]
    struct PdfWindows {
        today: PdfRow,
        total: PdfRow,
    }
    #[derive(serde::Deserialize)]
    struct PdfClient {
        usage: PdfWindows,
        daily_usage: Vec<PdfRow>,
        model_stats: Vec<PdfRow>,
    }

    #[test]
    fn gateway_response_has_only_the_pdf_fields() {
        let (_dir, store) = fixture();
        store
            .apply_settlement(SettlementFact {
                source_id: "settled".into(),
                deal_id: "d".into(),
                occurred_at: Utc::now().timestamp(),
                escrow_raw: 3 * SHELL_UNIT,
                buyer_bond_raw: SHELL_UNIT,
                buyer_credit_raw: Some(SHELL_UNIT),
            })
            .unwrap();
        store.mark_operations_complete("d").unwrap();
        store
            .apply_operational_expense(OperationalFact {
                source_id: "buy".into(),
                deal_id: None,
                model: None,
                occurred_at: Utc::now().timestamp(),
                spent_raw: SHELL_UNIT / 40,
            })
            .unwrap();
        let details = snapshot(&store);
        let raw = serde_json::to_string(&gateway_response(details.clone()).unwrap()).unwrap();
        let client: PdfClient = serde_json::from_str(&raw).unwrap();
        assert_eq!(client.usage.total.requests, 1);
        assert_eq!(client.usage.today.input_tokens, Some(3));
        assert_eq!(client.usage.total.output_tokens, Some(4));
        assert_eq!(client.usage.total.total_tokens, Some(7));
        assert_eq!(client.usage.total.cost.unwrap().get(), "0.000006");
        assert_eq!(client.usage.total.overhead_cost.unwrap().get(), "0.025");
        assert_eq!(client.daily_usage.len(), 1);
        assert_eq!(client.model_stats.len(), 2);
        let response: Value = serde_json::from_str(&raw).unwrap();
        let top: BTreeSet<_> = response
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            top,
            BTreeSet::from([
                "mode",
                "isValid",
                "quota",
                "remaining",
                "unit",
                "usage",
                "daily_usage",
                "model_stats"
            ])
        );
        assert!(response.get("buyer").is_none());
        assert_eq!(response["mode"], "quota_limited");
        assert_eq!(response["isValid"], true);
        assert_eq!(response["quota"]["unit"], "SHELL");
        assert!(response["quota"]["used"].is_number());
        assert_eq!(response["quota"]["limit"], 3);
        assert_eq!(response["quota"]["used"], 0.000006);
        assert_eq!(response["quota"]["remaining"], 0);
        assert_eq!(response["remaining"], response["quota"]["remaining"]);
        assert_eq!(response["unit"], "SHELL");
        assert!(response["usage"].get("period").is_none());
        let total = &response["usage"]["total"];
        assert!(total.get("answer").is_none());
        assert_eq!(total["cache_creation_tokens"], 0);
        assert!(total.get("cache_write_tokens").is_none());
        assert_eq!(total["cost"], 0.000006);
        assert!(total.get("actual_cost").is_none());
        let daily = &response["daily_usage"][0];
        assert_eq!(daily["cache_write_tokens"], 0);
        assert!(daily.get("cache_creation_tokens").is_none());
        let unknown = response["model_stats"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["model"] == "unknown")
            .unwrap();
        assert_eq!(unknown["requests"], 0);
        assert_eq!(unknown["total_tokens"], 0);
        assert!(unknown["overhead_cost"].is_number());
    }

    #[test]
    fn gateway_pending_deal_preserves_confirmed_expense() {
        let (_dir, store) = fixture();
        store
            .apply_deal_expenses(
                "d",
                &[crate::buyer::usage::AccrualFact {
                    source_id: "accepted-probe".into(),
                    occurred_at: Utc::now().timestamp(),
                    spent_raw: SHELL_UNIT,
                }],
                None,
            )
            .unwrap();
        let raw = serde_json::to_string(&gateway_response(snapshot(&store)).unwrap()).unwrap();
        let response: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(response["quota"]["used"], 0.000006);
        assert_eq!(response["usage"]["total"]["overhead_cost"], 0);
        assert_eq!(response["model_stats"][0]["overhead_cost"], 0);
        assert_eq!(
            snapshot(&store)["spend"]["total"]["confirmed_cost_shell"],
            "1"
        );
        assert_eq!(response["usage"]["total"]["total_tokens"], 7);
        assert!(!store.facts(&UsageFactsQuery::default()).unwrap().complete);
    }

    #[test]
    fn gateway_numeric_money_preserves_every_uint128_decimal_digit() {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        store
            .apply_operational_expense(OperationalFact {
                source_id: "large".into(),
                deal_id: None,
                model: None,
                occurred_at: Utc::now().timestamp(),
                spent_raw: u128::MAX,
            })
            .unwrap();
        let raw = serde_json::to_string(&gateway_response(snapshot(&store)).unwrap()).unwrap();
        assert!(raw.contains("\"overhead_cost\":340282366920938463463374607431.768211455"));
        assert!(raw.contains("\"used\":0"));
        let client: PdfClient = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            client.usage.total.overhead_cost.unwrap().get(),
            "340282366920938463463374607431.768211455"
        );
    }

    #[test]
    fn quota_numbers_preserve_uint128_precision_and_saturate_remaining() {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        store.open_deal_quota("large", 1, u128::MAX).unwrap();
        store.register_deal("large", "m").unwrap();
        store
            .start_request("large-r", "m", "openai", false)
            .unwrap();
        store.link_deal("large-r", "large").unwrap();
        store
            .record_call_usage(
                "large-r",
                "answer",
                "answer",
                BillingUsage::new(dexdo_core::params::TICK_SIZE as u64, 0, None).unwrap(),
                true,
            )
            .unwrap();
        store
            .finish_request("large-r", 200, Some("stop"), None)
            .unwrap();
        store
            .apply_deal_expenses(
                "large",
                &[crate::buyer::usage::AccrualFact {
                    source_id: "large-accrual".into(),
                    occurred_at: 1,
                    spent_raw: u128::MAX,
                }],
                None,
            )
            .unwrap();
        let raw = serde_json::to_string(&gateway_response(snapshot(&store)).unwrap()).unwrap();
        assert!(raw.contains("\"limit\":340282366920938463463374607431.768211455"));
        assert!(raw.contains("\"used\":340282366920938463463374607431.768211455"));
        assert!(raw.contains("\"remaining\":0"));
    }

    #[tokio::test]
    async fn usage_read_keeps_previous_quota_without_starting_a_lazy_purchase() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
        store.open_deal_quota("previous", 100, SHELL_UNIT).unwrap();
        store.close_deal_quota("previous").unwrap();
        let state = ApiState::lazy(
            Arc::new(crate::buyer::Buyer::generate()),
            "m".into(),
            Arc::new(|| Box::pin(async { panic!("GET usage must never execute a new order") })),
            dexdo_core::params::MATCH_OPEN_TIMEOUT,
        )
        .with_usage(store.clone());
        let response = super::super::router(state)
            .oneshot(
                http::Request::get("/v1/usage")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["quota"],
            serde_json::json!({"limit":100,"used":0,"remaining":0,"unit":"SHELL"})
        );
        assert_eq!(snapshot(&store)["usage"]["total"]["requests"]["total"], 0);
    }

    #[tokio::test]
    async fn facts_pagination_2167_returns_pages_and_400_for_invalid_or_expired_cursors() {
        use tower::ServiceExt;
        async fn get(app: axum::Router, path: &str) -> (StatusCode, Value) {
            let response = app
                .oneshot(
                    http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice(&body).unwrap())
        }
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
        let operation = |id: &str| OperationalFact {
            source_id: id.into(),
            deal_id: None,
            model: None,
            occurred_at: 1000,
            spent_raw: SHELL_UNIT,
        };
        for id in ["a", "b"] {
            store.apply_operational_expense(operation(id)).unwrap();
        }
        let state = ApiState::lazy(
            Arc::new(crate::buyer::Buyer::generate()),
            "m".into(),
            Arc::new(|| Box::pin(async { panic!("GET facts must never place an order") })),
            dexdo_core::params::MATCH_OPEN_TIMEOUT,
        )
        .with_usage(store.clone());
        let app = super::super::router(state);
        for suffix in [
            "limit=0".to_string(),
            format!(
                "limit={}",
                dexdo_core::params::BUYER_USAGE_FACTS_MAX_LIMIT + 1
            ),
            "limit=-1".into(),
            "cursor=bad".into(),
            "unknown=1".into(),
        ] {
            let (status, body) = get(app.clone(), &format!("/v1/usage/facts?{suffix}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"]["type"], "invalid_usage_query");
        }
        let (status, first) = get(app.clone(), "/v1/usage/facts?limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["facts"].as_array().unwrap().len(), 1);
        assert_eq!(first["complete"], true);
        let path = format!(
            "/v1/usage/facts?limit=1&cursor={}",
            first["next_cursor"].as_str().unwrap()
        );
        let (status, last) = get(app.clone(), &path).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(last["facts"][0]["source_id"], "b");
        assert!(last["next_cursor"].is_null());
        store.apply_operational_expense(operation("late")).unwrap();
        let (status, body) = get(app, &path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_usage_query");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("restart"));
    }

    #[test]
    fn cache_breakdown_2132_keeps_accepted_verification_on_a_failed_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap();
        store.start_request("r", "m", "anthropic", true).unwrap();
        store.register_deal("d", "m").unwrap();
        store.open_deal_quota("d", 10, SHELL_UNIT).unwrap();
        store.link_deal("r", "d").unwrap();
        let mut usage = BillingUsage::new(5, 1, None).unwrap();
        usage.cache_read_tokens = 2;
        usage.cache_write_tokens = 1;
        usage.charged_tokens = Some(3);
        store
            .record_call_usage("r", "verification_b8", "b8", usage, true)
            .unwrap();
        store
            .finish_request("r", 502, Some("error"), Some("verification_b8"))
            .unwrap();
        let now = Utc::now().timestamp();
        store
            .apply_deal_expenses(
                "d",
                &[crate::buyer::usage::AccrualFact {
                    source_id: "paid".into(),
                    occurred_at: now,
                    spent_raw: SHELL_UNIT,
                }],
                None,
            )
            .unwrap();
        store
            .classify_expense(
                "paid",
                crate::buyer::usage::ExpenseBreakdown {
                    consumption_raw: SHELL_UNIT,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .apply_operational_expense(OperationalFact {
                source_id: "buy-fee".into(),
                deal_id: Some("d".into()),
                model: Some("m".into()),
                occurred_at: now,
                spent_raw: SHELL_UNIT / 20,
            })
            .unwrap();
        let public = serde_json::to_value(gateway_response(snapshot(&store)).unwrap()).unwrap();
        assert_eq!(public["mode"], "quota_limited");
        assert_eq!(
            public["quota"],
            serde_json::json!({"limit":10,"used":0.000003,"remaining":9.999997,"unit":"SHELL"})
        );
        assert_eq!(public["remaining"], public["quota"]["remaining"]);
        assert_eq!(public["unit"], public["quota"]["unit"]);
        for row in [
            &public["usage"]["today"],
            &public["usage"]["total"],
            &public["daily_usage"][0],
            &public["model_stats"][0],
        ] {
            assert_eq!(row["input_tokens"], 2);
            assert_eq!(row["output_tokens"], 1);
            assert_eq!(row["cache_read_tokens"], 2);
            let write = if row.get("date").is_some() {
                "cache_write_tokens"
            } else {
                "cache_creation_tokens"
            };
            assert_eq!(row[write], 1);
            assert_eq!(row["total_tokens"], 6);
            assert_eq!(row["cost"], 0.000003);
            assert_eq!(row["overhead_cost"], 0.05);
            assert!(row.get("actual_cost").is_none());
        }
    }
}
