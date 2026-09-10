//! Local read-only dashboard for open stream handles.

use crate::cli::deals::{self, DealHandle, DealHandleRole};
use anyhow::{bail, Result};
use async_trait::async_trait;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Json;
use dexdo_core::params::DASHBOARD_REFRESH_INTERVAL_MS;
#[cfg(test)]
use dexdo_core::ChainBackend;
use serde::Serialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Version 2 states money in SHELL. Version 1 stated the same fields in raw ECC[2], and the field
/// names did not change with them -- a consumer keyed on `version` is the only thing that can tell
/// the two apart, so it moves with the unit.
pub(crate) const DASHBOARD_VERSION: u32 = 2;
pub(crate) const DASHBOARD_JSON_PATH: &str = "/api/dashboard.json";

#[derive(Clone)]
pub(crate) struct DashboardAppState {
    deals_dir: PathBuf,
    backend: Arc<dyn DashboardBackend>,
}

impl DashboardAppState {
    pub(crate) fn for_chain(deals_dir: PathBuf) -> Self {
        Self {
            deals_dir,
            backend: Arc::new(ChainDashboardBackend),
        }
    }
}

#[async_trait]
pub(crate) trait DashboardBackend: Send + Sync {
    async fn facts(&self, handle: &DealHandle) -> Result<DashboardFacts>;
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardFacts {
    pub(crate) lifecycle: DashboardLifecycle,
    pub(crate) byfact: DashboardByFact,
    pub(crate) buyer_note: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) model_hash: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardLifecycle {
    pub(crate) state: Option<String>,
    pub(crate) funded: Option<bool>,
    pub(crate) opened: Option<bool>,
    pub(crate) disputed: Option<bool>,
    pub(crate) terminal: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardByFact {
    pub(crate) seller_locked: Option<u128>,
    pub(crate) buyer_locked: Option<u128>,
    pub(crate) buyer_bond: Option<u128>,
    pub(crate) buyer_bond_required: Option<u128>,
    pub(crate) tokens_final: Option<u128>,
    pub(crate) seller_received: Option<u128>,
    pub(crate) buyer_refunded: Option<u128>,
    pub(crate) burned: Option<u128>,
    pub(crate) closed: Option<bool>,
}

#[cfg(test)]
pub(crate) struct FakeChainDashboardBackend<C> {
    chain: Arc<C>,
}

#[cfg(test)]
impl<C> FakeChainDashboardBackend<C> {
    fn new(chain: Arc<C>) -> Self {
        Self { chain }
    }
}

#[cfg(test)]
#[async_trait]
impl<C> DashboardBackend for FakeChainDashboardBackend<C>
where
    C: ChainBackend + Send + Sync,
{
    async fn facts(&self, handle: &DealHandle) -> Result<DashboardFacts> {
        let lifecycle = self
            .chain
            .deal_state(&handle.token_contract)
            .await?
            .map(lifecycle_from_chain_state)
            .unwrap_or_default();
        let byfact = self
            .chain
            .snapshot(&handle.token_contract)
            .await
            .map(byfact_from_chain_snapshot)
            .unwrap_or_default();
        Ok(DashboardFacts {
            lifecycle,
            byfact,
            buyer_note: None,
            model: None,
            model_hash: None,
        })
    }
}

struct ChainDashboardBackend;

#[async_trait]
impl DashboardBackend for ChainDashboardBackend {
    async fn facts(&self, handle: &DealHandle) -> Result<DashboardFacts> {
        use dexdo_core::RealChainBackend;

        let chain = RealChainBackend::connect(&handle.contracts)?;
        let tc = dexdo_core::address::parse_chain_address(&handle.token_contract)
            .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", handle.token_contract))?;
        let snapshot = chain.token_contract_deal_snapshot(&tc).await?;
        let lifecycle = snapshot
            .as_ref()
            .map(|snapshot| lifecycle_from_chain_state(snapshot.state))
            .unwrap_or_else(|| DashboardLifecycle {
                state: Some("terminal".to_string()),
                terminal: Some(true),
                ..DashboardLifecycle::default()
            });
        let byfact = match snapshot {
            Some(snapshot) => dashboard_byfact_from_chain_state(snapshot)?,
            None => DashboardByFact {
                closed: Some(true),
                ..DashboardByFact::default()
            },
        };
        let buyer_note = chain
            .token_contract_buyer_note(&tc)
            .await?
            .map(|a| a.with_workchain());
        Ok(DashboardFacts {
            lifecycle,
            byfact,
            buyer_note,
            model: chain.token_contract_model_name(&tc).await?,
            model_hash: chain.token_contract_model_hash(&tc).await?,
        })
    }
}

fn lifecycle_from_chain_state(state: dexdo_core::DealChainState) -> DashboardLifecycle {
    let terminal = state.is_stopped();
    let state_name = if state.disputed {
        "disputed"
    } else if state.opened {
        "opened"
    } else if terminal {
        "terminal"
    } else if state.funded {
        "funded"
    } else {
        "placed"
    };
    DashboardLifecycle {
        state: Some(state_name.to_string()),
        funded: Some(state.funded),
        opened: Some(state.opened),
        disputed: Some(state.disputed),
        terminal: Some(terminal),
    }
}

#[cfg(test)]
fn byfact_from_chain_snapshot(snapshot: dexdo_core::StreamSnapshot) -> DashboardByFact {
    DashboardByFact {
        seller_locked: Some(snapshot.seller_locked),
        buyer_locked: Some(snapshot.buyer_locked),
        buyer_bond: None,
        buyer_bond_required: None,
        tokens_final: Some(snapshot.tokens_final),
        seller_received: Some(snapshot.seller_received),
        buyer_refunded: Some(snapshot.buyer_refunded),
        burned: Some(snapshot.burned),
        closed: Some(snapshot.closed),
    }
}

fn dashboard_byfact_from_chain_state(
    snapshot: dexdo_core::DealChainSnapshot,
) -> Result<DashboardByFact> {
    let state = snapshot.state;
    let buyer_locked = snapshot.buyer_locked().map_err(anyhow::Error::msg)?;
    Ok(DashboardByFact {
        seller_locked: Some(snapshot.seller_bond.bond_held),
        buyer_locked: Some(buyer_locked),
        buyer_bond: Some(snapshot.buyer_bond.bond_held),
        buyer_bond_required: Some(snapshot.buyer_bond.bond_required),
        tokens_final: Some(state.tokens_final),
        seller_received: Some(state.finalized_owed),
        buyer_refunded: None,
        burned: None,
        closed: Some(state.is_stopped()),
    })
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DashboardSnapshot {
    pub(crate) version: u32,
    pub(crate) generated_at_unix: u64,
    pub(crate) source: DashboardSource,
    pub(crate) buyer: Vec<DashboardDeal>,
    pub(crate) seller: Vec<DashboardDeal>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DashboardSource {
    pub(crate) kind: String,
    pub(crate) json_endpoint: String,
    pub(crate) handle_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DashboardDeal {
    pub(crate) handle: String,
    pub(crate) role: String,
    pub(crate) network: String,
    #[serde(with = "dexdo_core::address::serde_self_dapp")]
    pub(crate) token_contract: String,
    pub(crate) frame_model: Option<String>,
    pub(crate) model_hash: Option<String>,
    pub(crate) state: String,
    pub(crate) funded: Option<bool>,
    pub(crate) opened: Option<bool>,
    pub(crate) disputed: Option<bool>,
    pub(crate) terminal: Option<bool>,
    pub(crate) gateway_endpoint: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) actor_note: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) counterparty_note: Option<String>,
    pub(crate) accounting: DashboardAccounting,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DashboardAccounting {
    pub(crate) shell_paid: Option<String>,
    pub(crate) shell_locked: Option<String>,
    pub(crate) buyer_bond: Option<String>,
    pub(crate) buyer_bond_required: Option<String>,
    pub(crate) shell_refunded: Option<String>,
    pub(crate) shell_burned: Option<String>,
    pub(crate) finalized_owed: Option<String>,
    pub(crate) ticks_spent: Option<String>,
    pub(crate) tokens_spent: Option<String>,
    pub(crate) delivered_ticks: Option<String>,
    pub(crate) delivered_tokens: Option<String>,
}

pub(crate) async fn bind_dashboard(
    listen: SocketAddr,
    state: DashboardAppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<SocketAddr> {
    ensure_loopback(listen)?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let app = router(state);
    tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(shutdown);
        if let Err(e) = server.await {
            tracing::error!("dashboard server stopped: {e}");
        }
    });
    Ok(addr)
}

pub(crate) fn ensure_loopback(listen: SocketAddr) -> Result<()> {
    if !listen.ip().is_loopback() {
        bail!("dashboard is loopback-only; use 127.0.0.1:<port> or [::1]:<port>");
    }
    Ok(())
}

pub(crate) fn router(state: DashboardAppState) -> axum::Router {
    axum::Router::new()
        .route("/", get(index))
        .route(DASHBOARD_JSON_PATH, get(json))
        .with_state(state)
}

async fn json(State(state): State<DashboardAppState>) -> impl IntoResponse {
    match read_dashboard(&state.deals_dir, state.backend.as_ref()).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("dashboard read failed: {e}"),
        )
            .into_response(),
    }
}

async fn index(State(state): State<DashboardAppState>) -> impl IntoResponse {
    match read_dashboard(&state.deals_dir, state.backend.as_ref()).await {
        Ok(snapshot) => Html(render_html(&snapshot)).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("dashboard read failed: {e}"),
        )
            .into_response(),
    }
}

pub(crate) async fn read_dashboard(
    deals_dir: &Path,
    backend: &dyn DashboardBackend,
) -> Result<DashboardSnapshot> {
    let handles = deals::list_deal_handles(deals_dir)?;
    dashboard_from_handles(handles, backend).await
}

pub(crate) async fn dashboard_from_handles(
    handles: Vec<(PathBuf, DealHandle)>,
    backend: &dyn DashboardBackend,
) -> Result<DashboardSnapshot> {
    let generated_at_unix = deals::now_unix()?;
    let handle_count = handles.len();
    let mut buyer = Vec::new();
    let mut seller = Vec::new();
    for (_path, handle) in handles {
        let deal = match backend.facts(&handle).await {
            Ok(facts) => dashboard_deal(&handle, facts),
            Err(error) => {
                let mut deal = dashboard_deal(&handle, DashboardFacts::default());
                deal.state = format!("error: {error}");
                deal
            }
        };
        match handle.role {
            DealHandleRole::Buyer => buyer.push(deal),
            DealHandleRole::Seller => seller.push(deal),
        }
    }
    Ok(DashboardSnapshot {
        version: DASHBOARD_VERSION,
        generated_at_unix,
        source: DashboardSource {
            kind: "local_deal_handles".to_string(),
            json_endpoint: DASHBOARD_JSON_PATH.to_string(),
            handle_count,
        },
        buyer,
        seller,
    })
}

fn dashboard_deal(handle: &DealHandle, facts: DashboardFacts) -> DashboardDeal {
    let state = facts
        .lifecycle
        .state
        .clone()
        .or_else(|| {
            facts
                .byfact
                .closed
                .and_then(|closed| closed.then(|| "terminal".to_string()))
        })
        .unwrap_or_else(|| "unknown".to_string());
    let terminal = facts.lifecycle.terminal.or_else(|| {
        facts
            .byfact
            .closed
            .and_then(|closed| closed.then_some(true))
    });
    let accounting = accounting_for(handle.role, &facts.byfact);
    let counterparty_note = match handle.role {
        DealHandleRole::Buyer => handle.market.as_ref().map(|m| m.seller_note.clone()),
        DealHandleRole::Seller => facts.buyer_note.clone(),
    };
    DashboardDeal {
        handle: handle.handle.clone(),
        role: handle.role.as_str().to_string(),
        network: handle.network.clone(),
        token_contract: handle.token_contract.clone(),
        frame_model: facts.model.or_else(|| Some(handle.frame_model.clone())),
        model_hash: facts.model_hash.or_else(|| handle.model_hash.clone()),
        state,
        funded: facts.lifecycle.funded,
        opened: facts.lifecycle.opened,
        disputed: facts.lifecycle.disputed,
        terminal,
        gateway_endpoint: handle.endpoint.as_ref().and_then(|e| {
            (handle.role == DealHandleRole::Seller && e.kind == "gateway").then(|| e.value.clone())
        }),
        actor_note: Some(handle.note_addr.clone()),
        counterparty_note,
        accounting,
    }
}

pub(crate) fn accounting_for(
    role: DealHandleRole,
    byfact: &DashboardByFact,
) -> DashboardAccounting {
    let tokens = byfact.tokens_final;
    let ticks = tokens.map(|tokens| tokens / dexdo_core::TICK_SIZE);
    // The same by-fact fields `status` reports, from the same snapshot: they are SHELL there and
    // SHELL here. Ticks and tokens are counts and stay counts.
    let shell = |value: Option<u128>| value.map(dexdo_core::shell_amount);
    match role {
        DealHandleRole::Buyer => DashboardAccounting {
            shell_paid: shell(byfact.seller_received),
            shell_locked: shell(byfact.buyer_locked),
            buyer_bond: shell(byfact.buyer_bond),
            buyer_bond_required: shell(byfact.buyer_bond_required),
            shell_refunded: shell(byfact.buyer_refunded),
            shell_burned: shell(byfact.burned),
            ticks_spent: ticks.map(|v| v.to_string()),
            tokens_spent: tokens.map(|v| v.to_string()),
            ..DashboardAccounting::default()
        },
        DealHandleRole::Seller => DashboardAccounting {
            shell_locked: shell(byfact.seller_locked),
            shell_burned: shell(byfact.burned),
            finalized_owed: shell(byfact.seller_received),
            delivered_ticks: ticks.map(|v| v.to_string()),
            delivered_tokens: tokens.map(|v| v.to_string()),
            ..DashboardAccounting::default()
        },
    }
}

#[cfg(test)]
#[path = "dashboard_html_render_1714_tests.rs"]
mod dashboard_html_render_1714_tests;

pub(crate) fn render_html(snapshot: &DashboardSnapshot) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<title>dexdo dashboard</title>");
    out.push_str("<style>");
    out.push_str("body{margin:0;font-family:system-ui,-apple-system,Segoe UI,sans-serif;background:#f7f8fa;color:#15181d}");
    out.push_str("main{max-width:1180px;margin:0 auto;padding:24px}");
    out.push_str("h1{font-size:24px;margin:0 0 18px}h2{font-size:18px;margin:22px 0 10px}");
    out.push_str(
        "table{width:100%;border-collapse:collapse;background:white;border:1px solid #d9dee7}",
    );
    out.push_str("th,td{padding:8px 10px;border-bottom:1px solid #e8ecf2;text-align:left;font-size:13px;vertical-align:top}");
    out.push_str(
        "th{background:#eef2f6;font-weight:600}.muted{color:#697386}.unknown{color:#8a6d1d}",
    );
    out.push_str("code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px}");
    out.push_str("</style></head><body><main>");
    out.push_str("<h1>dexdo stream dashboard</h1>");
    out.push_str(&format!(
        "<p class=\"muted\">JSON: <code>{}</code> * handles: <span id=\"handle-count\">{}</span></p>",
        DASHBOARD_JSON_PATH, snapshot.source.handle_count
    ));
    render_section(&mut out, "Buyer Streams", "buyer-rows", &snapshot.buyer);
    render_section(&mut out, "Seller Streams", "seller-rows", &snapshot.seller);
    out.push_str(&dashboard_script());
    out.push_str("</main></body></html>");
    out
}

fn dashboard_script() -> String {
    let mut script = String::from(
        r#"<script>
function dashEscape(value){
  return String(value).replace(/[&<>"']/g,function(ch){
    return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&;'}[ch];
  });
}
function dashValue(value){
  if (value === null || value === undefined || value === "" || value === "unknown") {
    return '<span class="unknown">unknown</span>';
  }
  return '<code>' + dashEscape(value) + '</code>';
}
function dashAccounting(a){
  a = a || {};
  const rows = [
    ['paid', a.shell_paid],
    ['locked', a.shell_locked],
    ['buyer bond', a.buyer_bond],
    ['buyer bond required', a.buyer_bond_required],
    ['refunded', a.shell_refunded],
    ['burned', a.shell_burned],
    ['owed', a.finalized_owed],
    ['ticks', a.ticks_spent ?? a.delivered_ticks],
    ['tokens', a.tokens_spent ?? a.delivered_tokens],
  ];
  return rows.map(([label,value]) => '<div><span class="muted">' + dashEscape(label) + '</span> ' + dashValue(value) + '</div>').join('');
}
function dashRows(deals){
  if (!deals || deals.length === 0) {
    return '<tr><td colspan="7" class="muted">unknown</td></tr>';
  }
  return deals.map(function(d){
    return '<tr>'
      + '<td>' + dashValue(d.handle) + '</td>'
      + '<td>' + dashValue(d.state) + '</td>'
      + '<td>' + dashValue(d.token_contract) + '</td>'
      + '<td>' + dashValue(d.frame_model) + '</td>'
      + '<td>' + dashValue(d.counterparty_note) + '</td>'
      + '<td>' + dashValue(d.gateway_endpoint) + '</td>'
      + '<td>' + dashAccounting(d.accounting) + '</td>'
      + '</tr>';
  }).join('');
}
function renderDashboard(data){
  document.getElementById('handle-count').textContent = data && data.source ? data.source.handle_count : '0';
  document.getElementById('buyer-rows').innerHTML = dashRows(data ? data.buyer : []);
  document.getElementById('seller-rows').innerHTML = dashRows(data ? data.seller : []);
}
async function refreshDashboard(){
  const r = await fetch('/api/dashboard.json',{cache:'no-store'});
  if (r.ok) {
    const data = await r.json();
    window.dashboardSnapshot = data;
    renderDashboard(data);
  }
}
setInterval(refreshDashboard,"#,
    );
    script.push_str(&DASHBOARD_REFRESH_INTERVAL_MS.to_string());
    script.push_str(
        r#");
refreshDashboard();
</script>"#,
    );
    script
}

fn render_section(out: &mut String, title: &str, tbody_id: &str, deals: &[DashboardDeal]) {
    out.push_str(&format!("<section><h2>{}</h2>", escape_html(title)));
    out.push_str("<table><thead><tr>");
    for h in [
        "handle",
        "state",
        "token contract",
        "model",
        "counterparty",
        "endpoint",
        "accounting",
    ] {
        out.push_str(&format!("<th>{h}</th>"));
    }
    out.push_str(&format!(
        "</tr></thead><tbody id=\"{}\">",
        escape_html(tbody_id)
    ));
    if deals.is_empty() {
        out.push_str("<tr><td colspan=\"7\" class=\"muted\">unknown</td></tr>");
    }
    for d in deals {
        out.push_str("<tr>");
        cell(out, &d.handle);
        cell(out, &d.state);
        cell(
            out,
            &dexdo_core::address::display_self_dapp(&d.token_contract),
        );
        cell(out, d.frame_model.as_deref().unwrap_or("unknown"));
        cell(
            out,
            &dexdo_core::address::display_opt(d.counterparty_note.as_deref(), "unknown"),
        );
        cell(out, d.gateway_endpoint.as_deref().unwrap_or("unknown"));
        out.push_str("<td>");
        render_accounting(out, &d.accounting);
        out.push_str("</td></tr>");
    }
    out.push_str("</tbody></table></section>");
}

fn render_accounting(out: &mut String, a: &DashboardAccounting) {
    for (label, value) in [
        ("paid", a.shell_paid.as_deref()),
        ("locked", a.shell_locked.as_deref()),
        ("buyer bond", a.buyer_bond.as_deref()),
        ("buyer bond required", a.buyer_bond_required.as_deref()),
        ("refunded", a.shell_refunded.as_deref()),
        ("burned", a.shell_burned.as_deref()),
        ("owed", a.finalized_owed.as_deref()),
        (
            "ticks",
            a.ticks_spent.as_ref().or_ref(a.delivered_ticks.as_ref()),
        ),
        (
            "tokens",
            a.tokens_spent.as_ref().or_ref(a.delivered_tokens.as_ref()),
        ),
    ] {
        out.push_str(&format!(
            "<div><span class=\"muted\">{}</span> {}</div>",
            escape_html(label),
            display_value(value)
        ));
    }
}

trait OptionRefExt<'a> {
    fn or_ref(self, other: Option<&'a String>) -> Option<&'a str>;
}

impl<'a> OptionRefExt<'a> for Option<&'a String> {
    fn or_ref(self, other: Option<&'a String>) -> Option<&'a str> {
        self.or(other).map(String::as_str)
    }
}

fn cell(out: &mut String, value: &str) {
    out.push_str("<td>");
    out.push_str(&display_value(Some(value)));
    out.push_str("</td>");
}

fn display_value(value: Option<&str>) -> String {
    match value.filter(|v| !v.trim().is_empty() && *v != "unknown") {
        Some(v) => format!("<code>{}</code>", escape_html(v)),
        None => "<span class=\"unknown\">unknown</span>".to_string(),
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{
        ChainBackend, ChainError, DealChainState, Match, Note, OfferListing, SellOffer, Settlement,
        StreamSnapshot, TokenContract,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    fn sample_handle(role: DealHandleRole) -> DealHandle {
        DealHandle {
            version: deals::DEAL_HANDLE_VERSION,
            handle: format!("{}-tc-open", role.as_str()),
            role,
            network: "net-a".into(),
            token_contract: "0:feedface".into(),
            note_addr: match role {
                DealHandleRole::Buyer => "0:buyer".into(),
                DealHandleRole::Seller => "0:seller".into(),
            },
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
            order_book: Some("0:book".into()),
            root_model: Some("0:root".into()),
            market: Some(dexdo_core::MarketManifest {
                network: "net-a".into(),
                frame_model: "qwen/qwen3-32b".into(),
                model_hash: dexdo_core::model_hash_for("qwen/qwen3-32b"),
                inference_order_book: "0:book".into(),
                root_model: "0:root".into(),
                token_contract: "0:feedface".into(),
                seller_note: "0:seller".into(),
                nonce: 42,
                price_per_tick: 1000,
                max_ticks: 8,
            }),
            contracts: "manifest/deployed.manifest.json".into(),
            endpoint: (role == DealHandleRole::Seller).then(|| deals::DealEndpointInfo {
                kind: "gateway".into(),
                value: "127.0.0.1:8443".into(),
            }),
            created_order_ids: vec![7],
            created_at_unix: 1,
        }
    }

    struct FakeBackend {
        facts: DashboardFacts,
    }

    #[async_trait]
    impl DashboardBackend for FakeBackend {
        async fn facts(&self, _handle: &DealHandle) -> Result<DashboardFacts> {
            Ok(self.facts.clone())
        }
    }

    struct MiddleRowFails;

    #[async_trait]
    impl DashboardBackend for MiddleRowFails {
        async fn facts(&self, handle: &DealHandle) -> Result<DashboardFacts> {
            if handle.handle == "buyer-malformed" {
                return Err(anyhow::anyhow!("deal snapshot unavailable"));
            }
            Ok(open_facts())
        }
    }

    fn open_facts() -> DashboardFacts {
        DashboardFacts {
            lifecycle: DashboardLifecycle {
                state: Some("opened".into()),
                funded: Some(true),
                opened: Some(true),
                disputed: Some(false),
                terminal: Some(false),
            },
            byfact: DashboardByFact {
                seller_locked: Some(25),
                buyer_locked: Some(2050),
                buyer_bond: Some(50),
                buyer_bond_required: Some(50),
                tokens_final: Some(2 * dexdo_core::TICK_SIZE),
                seller_received: Some(2000),
                buyer_refunded: Some(50),
                burned: Some(5),
                closed: Some(false),
            },
            buyer_note: Some("0:buyer".into()),
            model: Some("qwen/qwen3-32b".into()),
            model_hash: Some("hash-qwen".into()),
        }
    }

    #[test]
    fn default_listen_is_loopback_and_non_loopback_is_rejected() {
        ensure_loopback("127.0.0.1:8765".parse().unwrap()).unwrap();
        ensure_loopback("[::1]:8765".parse().unwrap()).unwrap();
        let err = ensure_loopback("0.0.0.0:8765".parse().unwrap()).unwrap_err();
        assert!(err.to_string().contains("loopback-only"), "{err}");
    }

    #[tokio::test]
    async fn json_schema_is_stable_and_secret_free() {
        let snapshot = dashboard_from_handles(
            vec![(
                PathBuf::from("buyer.json"),
                sample_handle(DealHandleRole::Buyer),
            )],
            &FakeBackend {
                facts: open_facts(),
            },
        )
        .await
        .unwrap();
        let json = serde_json::to_value(&snapshot).unwrap();
        // The literal, not the constant: the payload's money is SHELL, and a test that reads the
        // constant agrees with any value it is given, which is how the fields moved units under
        // `"version": 1`. Changing the unit again has to change this line by hand.
        assert_eq!(json["version"], 2);
        assert_eq!(DASHBOARD_VERSION, 2);
        assert_eq!(json["source"]["json_endpoint"], DASHBOARD_JSON_PATH);
        assert_eq!(json["buyer"][0]["token_contract"], "0:feedface");
        assert_eq!(json["buyer"][0]["accounting"]["shell_paid"], "0.000002");
        assert!(json["seller"].as_array().unwrap().is_empty());
        let body = serde_json::to_string(&json).unwrap();
        for forbidden in [
            "note_key",
            "private_key",
            "secret",
            "seed",
            // The manifest path, as this tree spells it. It was "contracts/deployed" until
            // moved the manifests, and a canary looking for a string the client can no longer
            // print is a canary that cannot die.
            "manifest/deployed",
        ] {
            assert!(!body.contains(forbidden), "{body}");
        }
    }

    #[tokio::test]
    async fn fake_open_deal_appears_in_json_and_html() {
        let snapshot = dashboard_from_handles(
            vec![
                (
                    PathBuf::from("buyer.json"),
                    sample_handle(DealHandleRole::Buyer),
                ),
                (
                    PathBuf::from("seller.json"),
                    sample_handle(DealHandleRole::Seller),
                ),
            ],
            &FakeBackend {
                facts: open_facts(),
            },
        )
        .await
        .unwrap();
        assert_eq!(snapshot.buyer[0].state, "opened");
        assert_eq!(
            snapshot.seller[0].gateway_endpoint.as_deref(),
            Some("127.0.0.1:8443")
        );
        let html = render_html(&snapshot);
        assert!(html.contains("buyer-tc-open"), "{html}");
        assert!(html.contains("seller-tc-open"), "{html}");
        assert!(html.contains("0:feedface"), "{html}");
        assert!(html.contains("127.0.0.1:8443"), "{html}");
        assert!(html.contains("/api/dashboard.json"), "{html}");
    }

    #[tokio::test]
    async fn failing_deal_is_an_attributable_row_and_later_rows_still_render() {
        let mut first = sample_handle(DealHandleRole::Buyer);
        first.handle = "buyer-first".into();
        first.token_contract = "0:first".into();
        let mut malformed = sample_handle(DealHandleRole::Buyer);
        malformed.handle = "buyer-malformed".into();
        malformed.token_contract = "0:malformed".into();
        let mut last = sample_handle(DealHandleRole::Buyer);
        last.handle = "buyer-last".into();
        last.token_contract = "0:last".into();

        let snapshot = dashboard_from_handles(
            vec![
                (PathBuf::from("first.json"), first),
                (PathBuf::from("malformed.json"), malformed),
                (PathBuf::from("last.json"), last),
            ],
            &MiddleRowFails,
        )
        .await
        .unwrap();

        assert_eq!(snapshot.source.handle_count, 3);
        assert_eq!(snapshot.buyer.len(), 3);
        assert_eq!(snapshot.buyer[0].handle, "buyer-first");
        assert_eq!(snapshot.buyer[0].state, "opened");
        assert_eq!(
            snapshot.buyer[0].accounting.shell_paid.as_deref(),
            Some("0.000002")
        );
        assert_eq!(snapshot.buyer[1].handle, "buyer-malformed");
        assert_eq!(snapshot.buyer[1].token_contract, "0:malformed");
        assert_eq!(snapshot.buyer[1].state, "error: deal snapshot unavailable");
        assert_eq!(snapshot.buyer[1].accounting.shell_paid, None);
        assert_eq!(snapshot.buyer[2].handle, "buyer-last");
        assert_eq!(snapshot.buyer[2].state, "opened");
        assert_eq!(
            snapshot.buyer[2].accounting.shell_paid.as_deref(),
            Some("0.000002")
        );
    }

    #[tokio::test]
    async fn html_polling_rerenders_visible_rows_from_json() {
        let snapshot = dashboard_from_handles(
            vec![
                (
                    PathBuf::from("buyer.json"),
                    sample_handle(DealHandleRole::Buyer),
                ),
                (
                    PathBuf::from("seller.json"),
                    sample_handle(DealHandleRole::Seller),
                ),
            ],
            &FakeBackend {
                facts: open_facts(),
            },
        )
        .await
        .unwrap();
        let html = render_html(&snapshot);
        assert!(html.contains("id=\"handle-count\""), "{html}");
        assert!(html.contains("id=\"buyer-rows\""), "{html}");
        assert!(html.contains("id=\"seller-rows\""), "{html}");
        assert!(html.contains("function renderDashboard(data)"), "{html}");
        assert!(html.contains("fetch('/api/dashboard.json'"), "{html}");
        assert!(
            html.contains("document.getElementById('buyer-rows').innerHTML = dashRows"),
            "{html}"
        );
        assert!(
            html.contains("document.getElementById('seller-rows').innerHTML = dashRows"),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "setInterval(refreshDashboard,{DASHBOARD_REFRESH_INTERVAL_MS})"
            )),
            "{html}"
        );
    }

    #[tokio::test]
    async fn missing_byfact_fields_are_unknown_not_zero() {
        let snapshot = dashboard_from_handles(
            vec![(
                PathBuf::from("seller.json"),
                sample_handle(DealHandleRole::Seller),
            )],
            &FakeBackend {
                facts: DashboardFacts::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(snapshot.seller[0].state, "unknown");
        assert_eq!(snapshot.seller[0].accounting.shell_locked, None);
        assert_eq!(snapshot.seller[0].accounting.delivered_ticks, None);
        let json = serde_json::to_value(&snapshot).unwrap();
        assert!(json["seller"][0]["accounting"]["shell_locked"].is_null());
        assert!(json["seller"][0]["accounting"]["delivered_ticks"].is_null());
        let html = render_html(&snapshot);
        assert!(html.contains("unknown"), "{html}");
        assert!(!html.contains(">0</code>"), "{html}");
    }

    #[test]
    fn returned_two_p_bond_with_zero_tokens_cannot_invent_dashboard_volume() {
        let byfact = DashboardByFact {
            seller_received: Some(200),
            tokens_final: Some(0),
            ..DashboardByFact::default()
        };

        let seller = accounting_for(DealHandleRole::Seller, &byfact);
        assert_eq!(seller.finalized_owed.as_deref(), Some("0.0000002"));
        assert_eq!(seller.delivered_ticks.as_deref(), Some("0"));
        assert_eq!(seller.delivered_tokens.as_deref(), Some("0"));

        let buyer = accounting_for(DealHandleRole::Buyer, &byfact);
        assert_eq!(buyer.shell_paid.as_deref(), Some("0.0000002"));
        assert_eq!(buyer.ticks_spent.as_deref(), Some("0"));
        assert_eq!(buyer.tokens_spent.as_deref(), Some("0"));
    }

    #[test]
    fn withdrawal_cannot_reduce_authoritative_dashboard_token_volume() {
        let before = DashboardByFact {
            seller_received: Some(200),
            tokens_final: Some(2 * dexdo_core::TICK_SIZE),
            ..DashboardByFact::default()
        };
        let after = DashboardByFact {
            seller_received: Some(0),
            ..before.clone()
        };

        let seller_before = accounting_for(DealHandleRole::Seller, &before);
        let seller_after = accounting_for(DealHandleRole::Seller, &after);
        assert_eq!(seller_before.delivered_ticks.as_deref(), Some("2"));
        assert_eq!(seller_after.delivered_ticks, seller_before.delivered_ticks);
        assert_eq!(
            seller_after.delivered_tokens,
            seller_before.delivered_tokens
        );
        assert_ne!(seller_after.finalized_owed, seller_before.finalized_owed);

        let buyer_before = accounting_for(DealHandleRole::Buyer, &before);
        let buyer_after = accounting_for(DealHandleRole::Buyer, &after);
        assert_eq!(buyer_after.ticks_spent, buyer_before.ticks_spent);
        assert_eq!(buyer_after.tokens_spent, buyer_before.tokens_spent);
        assert_ne!(buyer_after.shell_paid, buyer_before.shell_paid);
    }

    #[test]
    fn dashboard_preserves_money_above_uint64_as_decimal_strings() {
        let above_u64 = u128::from(u64::MAX) + 1;
        let state = dexdo_core::DealChainState {
            funded: true,
            opened: true,
            probe_accepted: true,
            disputed: false,
            deposit: above_u64,
            finalized_owed: above_u64 + 1,
            tokens_final: dexdo_core::TICK_SIZE,
            tokens_pending: dexdo_core::TICK_SIZE,
            probe_tick: 0,
            funded_time: None,
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 0,
        };
        let byfact = dashboard_byfact_from_chain_state(dexdo_core::DealChainSnapshot {
            account_code_hash: "code".to_string(),
            account_boc_hash: "boc".to_string(),
            state,
            subscription: dexdo_core::DealSubscription {
                deal_flags: 0,
                sub_weeks: 0,
                week_index: 0,
                tokens_per_week: dexdo_core::TICK_SIZE,
                funded_tokens: dexdo_core::TICK_SIZE,
                tokens_paid: dexdo_core::TICK_SIZE,
                period_start: 0,
                week_base_tokens: 0,
            },
            seller_bond: dexdo_core::DealSellerBond {
                bond_funded: true,
                bond_held: above_u64 + 2,
                bond_required: above_u64 + 2,
            },
            buyer_bond: dexdo_core::DealBuyerBond {
                bond_held: above_u64 + 3,
                bond_required: above_u64 + 4,
            },
        })
        .expect("uint128 dashboard facts");

        assert_eq!(byfact.seller_received, Some(above_u64 + 1));
        assert_eq!(byfact.seller_locked, Some(above_u64 + 2));
        assert_eq!(byfact.buyer_bond, Some(above_u64 + 3));
        assert_eq!(byfact.buyer_bond_required, Some(above_u64 + 4));
        assert_eq!(byfact.buyer_locked, Some(2 * above_u64 + 3));

        let buyer = accounting_for(DealHandleRole::Buyer, &byfact);
        let seller = accounting_for(DealHandleRole::Seller, &byfact);
        assert_eq!(
            buyer.shell_paid,
            Some(dexdo_core::shell_amount(above_u64 + 1))
        );
        assert_eq!(
            buyer.shell_locked,
            Some(dexdo_core::shell_amount(2 * above_u64 + 3))
        );
        assert_eq!(
            buyer.buyer_bond,
            Some(dexdo_core::shell_amount(above_u64 + 3))
        );
        assert_eq!(
            seller.shell_locked,
            Some(dexdo_core::shell_amount(above_u64 + 2))
        );
        assert_eq!(
            seller.finalized_owed,
            Some(dexdo_core::shell_amount(above_u64 + 1))
        );
    }

    #[tokio::test]
    async fn chain_parser_rejects_missing_live_fields() {
        let incomplete = serde_json::json!({
            "finalizedOwed": "2000",
            "funded": false,
            "opened": false
        });
        assert!(dexdo_core::DealChainState::decode_getter(&incomplete).is_err());

        let state = dexdo_core::DealChainState {
            funded: true,
            opened: true,
            probe_accepted: true,
            disputed: false,
            deposit: 1000,
            finalized_owed: 2000,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 50,
            funded_time: None,
            probe_time: 0,
            last_claim_time: 0,
            dispute_time: 0,
        };
        let lifecycle = lifecycle_from_chain_state(state);
        assert_eq!(lifecycle.state.as_deref(), Some("opened"));
        assert_eq!(lifecycle.terminal, Some(false));

        let byfact = dashboard_byfact_from_chain_state(dexdo_core::DealChainSnapshot {
            account_code_hash: "code".to_string(),
            account_boc_hash: "boc".to_string(),
            state,
            subscription: dexdo_core::DealSubscription {
                deal_flags: dexdo_core::order_flags::SUBSCRIPTION,
                sub_weeks: dexdo_core::SUBSCRIPTION_WEEKS,
                week_index: 0,
                tokens_per_week: 100,
                funded_tokens: 400,
                tokens_paid: 0,
                period_start: 0,
                week_base_tokens: 0,
            },
            seller_bond: dexdo_core::DealSellerBond {
                bond_funded: true,
                bond_held: 0,
                bond_required: 0,
            },
            buyer_bond: dexdo_core::DealBuyerBond {
                bond_held: 200,
                bond_required: 200,
            },
        })
        .unwrap();
        assert_eq!(byfact.seller_received, Some(2000));
        assert_eq!(byfact.buyer_locked, Some(1250));
        assert_eq!(byfact.buyer_bond, Some(200));
        assert_eq!(byfact.buyer_bond_required, Some(200));
        assert_eq!(byfact.tokens_final, Some(0));
        assert_eq!(byfact.seller_locked, Some(0));
        assert_eq!(byfact.buyer_refunded, None);
        assert_eq!(byfact.burned, None);
        assert_eq!(byfact.closed, Some(false));

        let snapshot = dashboard_from_handles(
            vec![(
                PathBuf::from("buyer.json"),
                sample_handle(DealHandleRole::Buyer),
            )],
            &FakeBackend {
                facts: DashboardFacts {
                    lifecycle,
                    byfact,
                    ..DashboardFacts::default()
                },
            },
        )
        .await
        .unwrap();
        let deal = &snapshot.buyer[0];
        assert_eq!(deal.state, "opened");
        assert_eq!(deal.funded, Some(true));
        assert_eq!(deal.opened, Some(true));
        assert_eq!(deal.terminal, Some(false));
        assert_eq!(deal.accounting.shell_paid.as_deref(), Some("0.000002"));
        assert_eq!(deal.accounting.shell_locked.as_deref(), Some("0.00000125"));
        assert_eq!(deal.accounting.buyer_bond.as_deref(), Some("0.0000002"));
        assert_eq!(
            deal.accounting.buyer_bond_required.as_deref(),
            Some("0.0000002")
        );
        assert_eq!(deal.accounting.shell_refunded, None);
        assert_eq!(deal.accounting.shell_burned, None);
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["buyer"][0]["opened"], true);
        assert_eq!(json["buyer"][0]["terminal"], false);
        assert_eq!(json["buyer"][0]["accounting"]["shell_locked"], "0.00000125");
        assert_eq!(json["buyer"][0]["accounting"]["buyer_bond"], "0.0000002");
        assert!(json["buyer"][0]["accounting"]["shell_burned"].is_null());
        let body = serde_json::to_string(&json).unwrap();
        assert!(!body.contains("\"shell_locked\":\"0\""), "{body}");
        assert!(body.contains("\"terminal\":false"), "{body}");
    }

    struct WriteBombChain {
        wrote: AtomicBool,
    }

    impl WriteBombChain {
        fn new() -> Self {
            Self {
                wrote: AtomicBool::new(false),
            }
        }

        fn fail_write(&self, name: &str) -> ! {
            self.wrote.store(true, Ordering::SeqCst);
            panic!("dashboard invoked write method {name}");
        }
    }

    #[async_trait]
    impl ChainBackend for WriteBombChain {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            Ok(Vec::new())
        }

        async fn post_offer(&self, _offer: SellOffer, _note: &dyn Note) -> Result<(), ChainError> {
            self.fail_write("post_offer")
        }

        async fn place_buy(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<(), ChainError> {
            self.fail_write("place_buy")
        }

        async fn read_match(&self, _token_contract: &TokenContract) -> Result<Match, ChainError> {
            Err(ChainError::Chain("unused".into()))
        }

        async fn read_openable_match_now(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Option<Match>, ChainError> {
            Ok(None)
        }

        async fn open_stream(
            &self,
            _token_contract: &TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn Note,
        ) -> Result<(), ChainError> {
            self.fail_write("open_stream")
        }

        async fn read_handover(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Option<Vec<u8>>, ChainError> {
            Ok(None)
        }

        async fn claim_tokens(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
            _cumulative_tokens: u128,
        ) -> Result<(), ChainError> {
            self.fail_write("claim_tokens")
        }

        async fn stop(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<Settlement, ChainError> {
            self.fail_write("stop")
        }

        async fn dispute(
            &self,
            _token_contract: &TokenContract,
            _note: &dyn Note,
        ) -> Result<Settlement, ChainError> {
            self.fail_write("dispute")
        }

        async fn release_dispute(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Settlement, ChainError> {
            self.fail_write("release_dispute")
        }

        async fn cleanup_unopened(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Settlement, ChainError> {
            self.fail_write("cleanup_unopened")
        }

        async fn deal_state(
            &self,
            _token_contract: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            Ok(Some(DealChainState {
                funded: true,
                opened: true,
                probe_accepted: true,
                disputed: false,
                deposit: 1_000,
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                funded_time: Some(1),
                probe_tick: 0,
                probe_time: 0,
                last_claim_time: 2,
                dispute_time: 0,
            }))
        }

        async fn snapshot(&self, _token_contract: &TokenContract) -> Option<StreamSnapshot> {
            Some(StreamSnapshot {
                seller_locked: 25,
                buyer_locked: 2050,
                buyer_lead: 1000,
                tokens_final: 0,
                seller_received: 1000,
                buyer_refunded: 0,
                burned: 0,
                closed: false,
            })
        }
    }

    #[tokio::test]
    async fn dashboard_uses_only_chain_read_methods() {
        let chain = Arc::new(WriteBombChain::new());
        let backend = FakeChainDashboardBackend::new(chain.clone());
        let snapshot = dashboard_from_handles(
            vec![(
                PathBuf::from("buyer.json"),
                sample_handle(DealHandleRole::Buyer),
            )],
            &backend,
        )
        .await
        .unwrap();
        assert_eq!(snapshot.buyer[0].state, "opened");
        assert!(!chain.wrote.load(Ordering::SeqCst));
    }

    #[test]
    fn html_escapes_handle_fields() {
        let mut deal = dashboard_deal(&sample_handle(DealHandleRole::Seller), open_facts());
        deal.handle = "<script>alert(1)</script>".into();
        let snapshot = DashboardSnapshot {
            version: DASHBOARD_VERSION,
            generated_at_unix: 1,
            source: DashboardSource {
                kind: "local_deal_handles".into(),
                json_endpoint: DASHBOARD_JSON_PATH.into(),
                handle_count: 1,
            },
            buyer: Vec::new(),
            seller: vec![deal],
        };
        let html = render_html(&snapshot);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }
}
