//! `dexdo` CLI command handlers(`seller`/`buyer`/`monitor`/`provision`/`destroy`/`recover`), split out of
//! `main.rs`(PR3, move-only). Behavior-identical to the pre-split handlers.

use crate::cli::args::*;
use crate::cli::audit;
use crate::cli::dashboard;
use crate::cli::deals;
use crate::cli::indexer::{self, DepthQuery, IndexerClient, MarketsQuery};
use crate::cli::machine;
use crate::cli::policy;
use crate::cli::support::*;
use crate::operator_shutdown_signal;
use anyhow::{anyhow, bail, Result};
#[cfg(feature = "shellnet")]
use dexdo::registry::{
    default_model_registry_address,
    enforce_model_registry_policy as enforce_model_registry_policy_with_reader,
    resolve_registered_model_identity, ShellnetModelRegistryReader,
};
use dexdo::registry::{
    BuyerMissingBookPolicy, RegistryBookAction, RegistryRole, RegistryValidationInput,
    RegistryValidationPolicy,
};
use dexdo_core::{
    aggregate_tree, check_buy_deposit_headroom, check_matched_token_contract_state,
    executable_quote, model_hash_for, required_escrow_for_buy, ChainBackend, ChainError,
    DealChainState, DobParams, ExecutableQuote, MatchedTokenContractStatus, MockChainBackend,
    OfferListing, OrderBookOrder, ProtocolConsts, Settlement, MATCH_OPEN_TIMEOUT_SECS,
};
#[cfg(feature = "shellnet")]
use dexdo_core::{OrderBookSnapshot, OrderBookSubscription};
use serde_json::{json, Map, Value};
use std::io::Write as _;
use std::sync::Arc;

/// Deadline for awaiting match/handover: fail-closed, so `seller`/`buyer` don't hang
/// forever if the match didn't go through. Backstop, not SLA -- a real on-chain match completes in ~1-2 min.
pub(crate) const DEAL_WAIT_SECS: u64 = 300;
/// Lookback window for a model-only `--resume`: how far back to scan THIS note's own
/// `InferenceFilledConfirmed` events for the freshly matched deal (the buyer learns its deal from its own
/// note, never a hand-pasted address). Wide enough to survive a process restart / slow match, short enough
/// to skip earlier, already closed deals on the same book. The reader returns the MOST RECENT match in-window.
pub(crate) const RESUME_LOOKBACK_SECS: i64 = 1800;
#[cfg(feature = "shellnet")]
const DEFAULT_CONTRACTS_PATH: &str = "contracts/deployed.shellnet.json";
#[cfg(feature = "shellnet")]
const MODEL_REGISTRY_ABI_PATH: &str = "contracts/compiled_0.79.3/airegistry/ModelRegistry.abi.json";

#[cfg(feature = "shellnet")]
struct DealTarget {
    handle: Option<deals::DealHandle>,
    token_contract: String,
    role: Option<deals::DealHandleRole>,
    note_addr: Option<String>,
    market: Option<dexdo_core::MarketManifest>,
}

struct RuntimeDealHandleInput<'a> {
    role: deals::DealHandleRole,
    deals_dir: Option<&'a std::path::Path>,
    token_contract: &'a str,
    note_addr: &'a str,
    frame_model: &'a str,
    market_path: Option<&'a std::path::Path>,
    contracts: &'a std::path::Path,
    endpoint: Option<deals::DealEndpointInfo>,
}

fn note_pubkey_id(pk: &dexdo_core::NotePubkey) -> String {
    pk.ed.iter().map(|b| format!("{b:02x}")).collect()
}

fn persist_runtime_deal_handle(
    input: RuntimeDealHandleInput<'_>,
    network: &str,
) -> Result<deals::DealHandle> {
    let market = input.market_path.map(load_market).transpose()?;
    let h = deals::DealHandle {
        version: deals::DEAL_HANDLE_VERSION,
        handle: deals::make_handle_id(input.token_contract),
        role: input.role,
        network: network.to_string(),
        token_contract: input.token_contract.to_string(),
        note_addr: input.note_addr.to_string(),
        frame_model: input.frame_model.to_string(),
        model_hash: Some(model_hash_for(input.frame_model)),
        order_book: market.as_ref().map(|m| m.inference_order_book.clone()),
        root_model: market.as_ref().map(|m| m.root_model.clone()),
        market,
        contracts: input.contracts.display().to_string(),
        endpoint: input.endpoint,
        created_order_ids: Vec::new(),
        created_at_unix: deals::now_unix()?,
    };
    deals::validate_deal_handle(&h)?;
    let dir = deals::resolve_deals_dir(input.deals_dir)?;
    deals::save_deal_handle(&dir, &h)?;
    Ok(h)
}

fn save_mock_runtime_deal_handle(input: RuntimeDealHandleInput<'_>) -> Result<deals::DealHandle> {
    persist_runtime_deal_handle(input, "mock")
}

fn load_enabled_model_registry_policy(
    role: RegistryRole,
    args: &ModelRegistryValidationArgs,
    contracts: &std::path::Path,
) -> Result<Option<RegistryValidationPolicy>> {
    let policy = RegistryValidationPolicy::load(
        &RegistryValidationInput {
            config_path: args.model_registry_validation.clone(),
            address_override: args.model_registry_address.clone(),
        },
        contracts,
    )?;
    if policy.check_enabled(role) {
        Ok(Some(policy))
    } else {
        Ok(None)
    }
}

fn reject_buyer_raw_token_contract_without_registry_book_proof(
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    frame_model: &str,
) -> Result<()> {
    if market.is_none() {
        if let Some(tc) = token_contract {
            bail!(
                "buyer model registry check failed: frame_model {frame_model} raw --token-contract {tc} has no \
                 canonical order-book proof; with buyer.check_model_registry=true, pass --market <manifest> \
                 from the canonical registry book or omit --token-contract for a model-only registry buy/resume"
            );
        }
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
async fn enforce_model_registry_policy(
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    contracts: &std::path::Path,
    frame_model: &str,
    expected_order_book: &str,
    order_book_active: bool,
    buyer_missing_book_policy: BuyerMissingBookPolicy,
) -> Result<RegistryBookAction> {
    let registry_address = policy.required_address(role)?;
    let abi_path = std::path::Path::new(MODEL_REGISTRY_ABI_PATH);
    if !abi_path.exists() {
        bail!(
            "ModelRegistry ABI {MODEL_REGISTRY_ABI_PATH} is not committed in this branch and contracts/deployed.shellnet.json has no usable live reader; cannot validate {} frame_model {} against {} before money moves",
            role.as_str(),
            frame_model,
            registry_address
        );
    }
    let reader = ShellnetModelRegistryReader::from_manifest(contracts, registry_address, abi_path)?;
    enforce_model_registry_policy_with_reader(
        &reader,
        role,
        policy,
        frame_model,
        expected_order_book,
        order_book_active,
        buyer_missing_book_policy,
    )
    .await
}

#[cfg(not(feature = "shellnet"))]
async fn enforce_model_registry_policy(
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    contracts: &std::path::Path,
    frame_model: &str,
    expected_order_book: &str,
    order_book_active: bool,
    buyer_missing_book_policy: BuyerMissingBookPolicy,
) -> Result<RegistryBookAction> {
    let _ = (
        role,
        policy,
        contracts,
        frame_model,
        expected_order_book,
        order_book_active,
        buyer_missing_book_policy,
    );
    bail!("ModelRegistry validation requires a shellnet build")
}

#[cfg(feature = "shellnet")]
async fn resolve_content_identity_model(
    contracts: &std::path::Path,
    frame_model: &str,
) -> Result<String> {
    let registry_address = default_model_registry_address(contracts).map_err(|e| {
        anyhow!(
            "read default ModelRegistry address from {} for content identity: {e}",
            contracts.display()
        )
    })?;
    let abi_path = std::path::Path::new(MODEL_REGISTRY_ABI_PATH);
    if !abi_path.exists() {
        bail!(
            "ModelRegistry ABI {MODEL_REGISTRY_ABI_PATH} is not committed in this branch; cannot resolve content identity for frame_model {frame_model}"
        );
    }
    let reader =
        ShellnetModelRegistryReader::from_manifest(contracts, &registry_address, abi_path)?;
    let identity = resolve_registered_model_identity(
        &reader,
        RegistryRole::Buyer,
        &registry_address,
        frame_model,
    )
    .await?;
    Ok(identity.registry_model)
}

#[cfg(not(feature = "shellnet"))]
async fn resolve_content_identity_model(
    contracts: &std::path::Path,
    frame_model: &str,
) -> Result<String> {
    let _ = (contracts, frame_model);
    bail!("content identity ModelRegistry resolution requires a shellnet build")
}

#[cfg(feature = "shellnet")]
fn role_arg_to_handle(role: DealRoleArg) -> deals::DealHandleRole {
    match role {
        DealRoleArg::Buyer => deals::DealHandleRole::Buyer,
        DealRoleArg::Seller => deals::DealHandleRole::Seller,
    }
}

#[cfg(feature = "shellnet")]
fn load_deal_target(
    input: &str,
    deals_dir: Option<&std::path::Path>,
    raw_role: Option<DealRoleArg>,
    raw_note_addr: Option<String>,
) -> Result<DealTarget> {
    let dir = deals::resolve_deals_dir(deals_dir)?;
    if let Some((_path, handle)) = deals::resolve_deal_ref(input, &dir)? {
        let role = handle.role;
        let token_contract = handle.token_contract.clone();
        let note_addr = Some(handle.note_addr.clone());
        let market = handle.market.clone();
        return Ok(DealTarget {
            handle: Some(handle),
            token_contract,
            role: Some(role),
            note_addr,
            market,
        });
    }
    Ok(DealTarget {
        handle: None,
        token_contract: input.to_string(),
        role: raw_role.map(role_arg_to_handle),
        note_addr: raw_note_addr,
        market: None,
    })
}

#[cfg(feature = "shellnet")]
fn deal_contracts_path(
    explicit: Option<&std::path::Path>,
    target: &DealTarget,
) -> std::path::PathBuf {
    explicit
        .map(std::path::PathBuf::from)
        .or_else(|| {
            target.handle.as_ref().and_then(|h| {
                (!h.contracts.trim().is_empty()).then(|| std::path::PathBuf::from(&h.contracts))
            })
        })
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_CONTRACTS_PATH))
}

#[cfg(feature = "shellnet")]
async fn shellnet_doctor_preflight_market(
    contracts: &std::path::Path,
    market: Option<&dexdo_core::MarketManifest>,
) -> Result<()> {
    let contracts = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(contracts)?;
    let report = chain.doctor(market).await?;
    if !report.is_ok() {
        bail!("{}", render_shellnet_doctor_report(&report));
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn save_runtime_deal_handle(
    input: RuntimeDealHandleInput<'_>,
    emit_human_output: bool,
) -> Result<deals::DealHandle> {
    let h = persist_runtime_deal_handle(input, "shellnet")?;
    if emit_human_output {
        println!("deal_handle={}", h.handle);
    }
    Ok(h)
}

#[cfg(not(feature = "shellnet"))]
fn save_runtime_deal_handle(
    _input: RuntimeDealHandleInput<'_>,
    _emit_human_output: bool,
) -> Result<deals::DealHandle> {
    bail!("real shellnet deal handles unavailable: build with `--features shellnet`")
}

fn seller_watch_cursor_path(
    deals_dir: Option<&std::path::Path>,
    token_contract: &str,
) -> Result<std::path::PathBuf> {
    Ok(deals::resolve_deals_dir(deals_dir)?
        .join("seller-watch")
        .join(format!(
            "{}.cursor.json",
            deals::make_handle_id(token_contract)
        )))
}

#[cfg(feature = "shellnet")]
const ORACLE_MIN_RESULT_GAP_SECS: u64 = 120;

#[cfg(feature = "shellnet")]
async fn shellnet_doctor_report(
    network: &str,
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
) -> Result<dexdo_core::ShellnetDoctorReport> {
    if network != "shellnet" {
        bail!("doctor: unsupported --network `{network}` (only `shellnet` is supported)");
    }
    let contracts = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let market = market.map(load_market).transpose()?;
    let chain = dexdo_core::RealChainBackend::connect(contracts)?;
    chain.doctor(market.as_ref()).await
}

#[cfg(feature = "shellnet")]
fn render_shellnet_doctor_report(report: &dexdo_core::ShellnetDoctorReport) -> String {
    let mut out = String::new();
    let status = if report.is_ok() { "PASS" } else { "FAIL" };
    out.push_str(&format!(
        "dexdo doctor: {status} network={}\n",
        report.network
    ));
    if !report.versions.is_empty() {
        out.push_str("versions:\n");
        for (name, version) in &report.versions {
            out.push_str(&format!("  {name}: {version}\n"));
        }
    }
    out.push_str("checks:\n");
    for c in &report.checks {
        out.push_str(&format!("  {:<4} {}", c.status.as_str(), c.name));
        if let Some(addr) = &c.address {
            out.push_str(&format!(" addr={addr}"));
        }
        if let Some(expected) = &c.expected {
            out.push_str(&format!(" expected={expected}"));
        }
        if let Some(actual) = &c.actual {
            out.push_str(&format!(" actual={actual}"));
        }
        out.push_str(&format!(" - {}\n", c.message));
    }
    out
}

#[cfg(feature = "shellnet")]
async fn shellnet_doctor_preflight(
    contracts: &std::path::Path,
    market: Option<&std::path::Path>,
) -> Result<()> {
    let report = shellnet_doctor_report("shellnet", contracts, market).await?;
    if !report.is_ok() {
        bail!("{}", render_shellnet_doctor_report(&report));
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
async fn shellnet_doctor_preflight(
    _contracts: &std::path::Path,
    _market: Option<&std::path::Path>,
) -> Result<()> {
    bail!("shellnet doctor unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_doctor(args: DoctorArgs) -> Result<()> {
    let report =
        shellnet_doctor_report(&args.network, &args.contracts, args.market.as_deref()).await?;
    print!("{}", render_shellnet_doctor_report(&report));
    println!("{}", policy::doctor_policy_line(args.policy.as_deref())?);
    if !report.is_ok() {
        bail!("doctor failed: {}", report.fail_summary());
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_doctor(_args: DoctorArgs) -> Result<()> {
    bail!("shellnet doctor unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
struct BookTarget {
    frame_model: String,
    model_hash: String,
    order_book: Option<String>,
    root_model: Option<String>,
    note_addr: Option<String>,
}

#[cfg(feature = "shellnet")]
fn model_target_from_config(
    models: &std::path::Path,
    model: &str,
    note_addr: Option<String>,
) -> Result<BookTarget> {
    let cfg = dexdo::seller::ModelsConfig::load(models)?;
    let frame_model = cfg.get(model)?.frame_model.clone();
    Ok(BookTarget {
        model_hash: model_hash_for(&frame_model),
        frame_model,
        order_book: None,
        root_model: None,
        note_addr,
    })
}

#[cfg(feature = "shellnet")]
fn target_from_market(path: &std::path::Path) -> Result<BookTarget> {
    let m = load_market(path)?;
    Ok(BookTarget {
        frame_model: m.frame_model,
        model_hash: m.model_hash,
        order_book: Some(m.inference_order_book),
        root_model: Some(m.root_model),
        note_addr: None,
    })
}

#[cfg(feature = "shellnet")]
fn target_from_market_for_model(
    path: &std::path::Path,
    models: &std::path::Path,
    requested_model: &str,
) -> Result<BookTarget> {
    let target = target_from_market(path)?;
    let requested_frame_model = if dexdo_core::validate_canonical_model_id(requested_model).is_ok()
    {
        requested_model.to_string()
    } else {
        dexdo::seller::ModelsConfig::load(models)?
            .get(requested_model)?
            .frame_model
            .clone()
    };
    let requested_hash = model_hash_for(&requested_frame_model);
    if target.frame_model != requested_frame_model || target.model_hash != requested_hash {
        bail!(
            "dexdo market requested model `{requested_model}` -> `{requested_frame_model}`, but --market is for \
             `{}` (model_hash {}): refusing to render the wrong market",
            target.frame_model,
            target.model_hash
        );
    }
    Ok(target)
}

#[cfg(feature = "shellnet")]
async fn read_book_target(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
) -> Result<OrderBookSnapshot> {
    if let Some(ob) = &target.order_book {
        let ob =
            dexdo_core::Address::parse(ob).map_err(|e| anyhow::anyhow!("order_book {ob}: {e}"))?;
        return chain
            .inference_orderbook_snapshot(&ob, &target.frame_model, &target.model_hash)
            .await;
    }
    let note_addr = target
        .note_addr
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--note-addr is required when --market is not supplied"))?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    chain
        .inference_orderbook_snapshot_for_note(
            &note,
            &target.frame_model,
            &target.model_hash,
            dexdo_core::MODEL_TICK_SIZE,
        )
        .await
}

#[cfg(feature = "shellnet")]
async fn read_executable_book_target(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
) -> Result<OrderBookSnapshot> {
    let mut snapshot = read_book_target(chain, target).await?;
    snapshot.orders = chain.executable_resting_asks(&snapshot).await?;
    Ok(snapshot)
}

#[cfg(feature = "shellnet")]
async fn expected_order_book_for_note(
    contracts: &std::path::Path,
    note_addr: &str,
    frame_model: &str,
) -> Result<String> {
    let manifest = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let model_hash = model_hash_for(frame_model);
    let ob = chain
        .inference_orderbook_address(&note, &model_hash, dexdo_core::MODEL_TICK_SIZE)
        .await?;
    Ok(ob.with_workchain())
}

#[cfg(feature = "shellnet")]
async fn order_book_active(
    chain: &dexdo_core::RealChainBackend,
    expected_order_book: &str,
) -> Result<bool> {
    let ob = dexdo_core::Address::parse(expected_order_book)
        .map_err(|e| anyhow::anyhow!("order_book {expected_order_book}: {e}"))?;
    Ok(chain.inference_orderbook_stats(&ob).await?.is_some())
}

#[cfg(feature = "shellnet")]
async fn order_book_active_from_contracts(
    contracts: &std::path::Path,
    expected_order_book: &str,
) -> Result<bool> {
    let manifest = contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    order_book_active(&chain, expected_order_book).await
}

#[cfg(not(feature = "shellnet"))]
async fn order_book_active_from_contracts(
    contracts: &std::path::Path,
    expected_order_book: &str,
) -> Result<bool> {
    let _ = (contracts, expected_order_book);
    bail!("order-book state reads require a shellnet build")
}

#[cfg(not(feature = "shellnet"))]
async fn expected_order_book_for_note(
    contracts: &std::path::Path,
    note_addr: &str,
    frame_model: &str,
) -> Result<String> {
    let _ = (contracts, note_addr, frame_model);
    bail!("order-book derivation requires a shellnet build")
}

#[cfg(feature = "shellnet")]
fn own_orders<'a>(snapshot: &'a OrderBookSnapshot, note_addr: &str) -> Vec<&'a OrderBookOrder> {
    let want = dexdo_core::normalize_wallet_address(note_addr)
        .unwrap_or_else(|_| note_addr.trim().to_string());
    snapshot
        .orders
        .iter()
        .filter(|o| {
            dexdo_core::normalize_wallet_address(&o.owner_note)
                .map(|owner| owner == want)
                .unwrap_or_else(|_| o.owner_note.eq_ignore_ascii_case(&want))
        })
        .collect()
}

#[cfg(feature = "shellnet")]
fn render_order_line(order: &OrderBookOrder) -> String {
    let side = if order.is_buy { "buy" } else { "sell" };
    let tc = order.token_contract.as_deref().unwrap_or("-");
    format!(
        "order_id={} side={} owner={} token_contract={} price_per_tick={} ticks={} escrow={} flags={} deadline={}",
        order.order_id,
        side,
        order.owner_note,
        tc,
        order.price_per_tick,
        order.ticks,
        order.escrow,
        order.flags,
        order.deadline
    )
}

fn mock_chain_for_machine(endpoints_file: Option<std::path::PathBuf>) -> Result<MockChainBackend> {
    let endpoints_file = resolve_endpoints_file(endpoints_file)?;
    Ok(MockChainBackend::new(
        endpoints_file,
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    ))
}

async fn mock_market_entry(
    chain: &MockChainBackend,
    frame_model: &str,
) -> Result<machine::MarketEntry> {
    let offers = chain.discover_offers().await?;
    let depth_ticks: u128 = offers.iter().map(|o| u128::from(o.max_ticks)).sum();
    let best_ask = offers.iter().map(|o| o.price_per_tick).min();
    Ok(machine::MarketEntry {
        frame_model: frame_model.to_string(),
        model_hash: model_hash_for(frame_model),
        order_book: "mock:order-book".to_string(),
        root_model: Some("mock:root-model".to_string()),
        active: true,
        order_count: offers.len() as u128,
        ask_count: offers.len() as u128,
        depth_ticks: machine::amount(depth_ticks),
        best_ask: best_ask.map(machine::amount),
        min_liquidity: machine::amount(0u8),
        tick_size: machine::amount(DobParams::canonical().tick_size),
        source: "mock_chain".to_string(),
    })
}

async fn run_markets_mock(args: MarketsArgs) -> Result<()> {
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let entry = mock_market_entry(&chain, &args.frame_model).await?;
    if args.json {
        return machine::print_json(&machine::MarketsResponse {
            schema: machine::MARKETS_SCHEMA,
            network: "mock".to_string(),
            generated_at_unix: machine::now_unix()?,
            markets: vec![entry],
        });
    }
    println!(
        "model={} order_book={} active={} order_count={} ask_count={} depth_ticks={} best_ask={}",
        entry.frame_model,
        entry.order_book,
        entry.active,
        entry.order_count,
        entry.ask_count,
        entry.depth_ticks,
        entry.best_ask.as_deref().unwrap_or("-")
    );
    Ok(())
}

fn mock_orders_from_offers(offers: Vec<OfferListing>) -> Vec<OrderBookOrder> {
    offers
        .into_iter()
        .enumerate()
        .map(|(i, offer)| OrderBookOrder {
            order_id: (i as u128).saturating_add(1),
            owner_note: offer.seller_id,
            token_contract: Some(offer.token_contract),
            is_buy: false,
            price_per_tick: u128::from(offer.price_per_tick),
            ticks: u128::from(offer.max_ticks),
            escrow: 0,
            deadline: 0,
            flags: 0,
            timestamp: 0,
        })
        .collect()
}

struct BuyerQuoteSelection {
    order_book: &'static str,
    escrow: u128,
    quote: ExecutableQuote,
}

async fn buyer_quote_selection(
    chain: &dyn ChainBackend,
    explicit_tc: Option<&str>,
    ticks: u128,
    max_price_per_tick: u128,
    escrow: Option<u128>,
) -> Result<BuyerQuoteSelection> {
    if explicit_tc.is_none() {
        chain
            .assert_model_buy_matches_executable_quote(ticks, max_price_per_tick)
            .await
            .map_err(|e| anyhow::anyhow!("buyer model-only quote preflight: {e}"))?;
    }
    let mut orders = mock_orders_from_offers(chain.discover_offers().await?);
    let order_book = if let Some(tc) = explicit_tc {
        orders.retain(|o| o.token_contract.as_deref() == Some(tc));
        if orders.is_empty() {
            let tc_owned = tc.to_string();
            if let Some((price_per_tick, max_ticks)) = chain.sell_offer_terms(&tc_owned).await? {
                orders.push(OrderBookOrder {
                    order_id: 1,
                    owner_note: String::new(),
                    token_contract: Some(tc_owned),
                    is_buy: false,
                    price_per_tick: u128::from(price_per_tick),
                    ticks: u128::from(max_ticks),
                    escrow: 0,
                    deadline: 0,
                    flags: 0,
                    timestamp: 0,
                });
            }
        }
        "explicit_token_contract"
    } else {
        "model_order_book"
    };
    orders.retain(|o| o.price_per_tick <= max_price_per_tick);
    let quote = executable_quote(&orders, Some(ticks), None)
        .map_err(|e| anyhow::anyhow!("buyer quote: {e}"))?;
    Ok(BuyerQuoteSelection {
        order_book,
        escrow: escrow.unwrap_or_else(|| required_escrow_for_buy(ticks, max_price_per_tick)),
        quote,
    })
}

fn quote_selected_fields(
    frame_model: &str,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
) -> serde_json::Value {
    let fills = selection
        .quote
        .fills
        .iter()
        .map(|fill| {
            let cost_without_fee = fill.ticks.saturating_mul(fill.price_per_tick);
            json!({
                "order_id": machine::amount(fill.order_id),
                "token_contract": fill.token_contract,
                "ticks": machine::amount(fill.ticks),
                "price_per_tick": machine::amount(fill.price_per_tick),
                "cost_without_fee": machine::amount(cost_without_fee),
                "platform_fee": machine::amount(fill.cost_with_fee.saturating_sub(cost_without_fee)),
                "cost_with_fee": machine::amount(fill.cost_with_fee)
            })
        })
        .collect::<Vec<_>>();
    json!({
        "frame_model": frame_model,
        "model_hash": model_hash_for(frame_model),
        "order_book": selection.order_book,
        "ticks": machine::amount(ticks),
        "max_price_per_tick": machine::amount(max_price_per_tick),
        "escrow": machine::amount(selection.escrow),
        "quote_complete": selection.quote.complete,
        "filled_ticks": machine::amount(selection.quote.filled_ticks),
        "total_with_fee": machine::amount(selection.quote.total_with_fee),
        "fills": fills
    })
}

fn fail_buyer_quote_selection(
    events: &mut machine::BuyerEventWriter,
    frame_model: &str,
    selection: &BuyerQuoteSelection,
    ticks: u128,
    max_price_per_tick: u128,
    context_fields: Value,
) -> Result<Option<()>> {
    let code = if selection.quote.filled_ticks == 0 {
        machine::ErrorCode::NoLiquidity
    } else if !selection.quote.complete {
        machine::ErrorCode::IncompleteQuote
    } else {
        return Ok(None);
    };
    let mut fields = quote_selected_fields(frame_model, selection, ticks, max_price_per_tick);
    merge_json_fields(&mut fields, context_fields);
    if let serde_json::Value::Object(obj) = &mut fields {
        obj.insert(
            "failure_class".to_string(),
            json!(if code == machine::ErrorCode::NoLiquidity {
                "no_liquidity"
            } else {
                "incomplete_quote"
            }),
        );
    }
    events.error(machine::OP_BUYER_START, code, fields)?;
    Ok(Some(()))
}

fn merge_json_fields(base: &mut Value, extra: Value) {
    if let (Value::Object(base), Value::Object(extra)) = (base, extra) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
}

fn quote_response_from_quote(
    network: &str,
    frame_model: &str,
    order_book: &str,
    ticks: Option<u128>,
    budget: Option<u128>,
    q: dexdo_core::ExecutableQuote,
) -> Result<machine::QuoteResponse> {
    let mut total_without_fee = 0u128;
    let fills = q
        .fills
        .into_iter()
        .map(|fill| {
            let cost_without_fee = fill.ticks.saturating_mul(fill.price_per_tick);
            let platform_fee = fill.cost_with_fee.saturating_sub(cost_without_fee);
            total_without_fee = total_without_fee.saturating_add(cost_without_fee);
            machine::QuoteFillEntry {
                order_id: machine::amount(fill.order_id),
                token_contract: fill.token_contract,
                ticks: machine::amount(fill.ticks),
                price_per_tick: machine::amount(fill.price_per_tick),
                cost_without_fee: machine::amount(cost_without_fee),
                platform_fee: machine::amount(platform_fee),
                cost_with_fee: machine::amount(fill.cost_with_fee),
            }
        })
        .collect::<Vec<_>>();
    let platform_fee = q.total_with_fee.saturating_sub(total_without_fee);
    Ok(machine::QuoteResponse {
        schema: machine::QUOTE_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        frame_model: frame_model.to_string(),
        model_hash: model_hash_for(frame_model),
        order_book: order_book.to_string(),
        request: machine::QuoteRequest {
            kind: if ticks.is_some() { "ticks" } else { "budget" },
            ticks: ticks.map(machine::amount),
            budget: budget.map(machine::amount),
        },
        filled_ticks: machine::amount(q.filled_ticks),
        total_without_fee: machine::amount(total_without_fee),
        platform_fee: machine::amount(platform_fee),
        total_with_fee: machine::amount(q.total_with_fee),
        complete: q.complete,
        no_liquidity: q.filled_ticks == 0,
        fills,
    })
}

async fn run_quote_mock(args: QuoteArgs) -> Result<()> {
    if args.ticks.is_some() == args.budget.is_some() {
        bail!("quote requires exactly one of --ticks or --budget");
    }
    let frame_model = args.model.as_deref().unwrap_or("dexdo-mock");
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let orders = mock_orders_from_offers(chain.discover_offers().await?);
    let q = executable_quote(&orders, args.ticks, args.budget)
        .map_err(|e| anyhow::anyhow!("quote: {e}"))?;
    if args.json {
        return machine::print_json(&quote_response_from_quote(
            "mock",
            frame_model,
            "mock:order-book",
            args.ticks,
            args.budget,
            q,
        )?);
    }
    if q.filled_ticks == 0 {
        println!("quote model={frame_model} order_book=mock:order-book no_liquidity=true");
        return Ok(());
    }
    println!(
        "quote model={} order_book=mock:order-book filled_ticks={} total_with_fee={} complete={}",
        frame_model, q.filled_ticks, q.total_with_fee, q.complete
    );
    for fill in q.fills {
        println!(
            "fill order_id={} token_contract={} ticks={} price_per_tick={} cost_with_fee={}",
            fill.order_id, fill.token_contract, fill.ticks, fill.price_per_tick, fill.cost_with_fee
        );
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
fn market_entry_from_snapshot(
    snapshot: &OrderBookSnapshot,
    root_model: Option<String>,
    source: &str,
) -> machine::MarketEntry {
    let depth_ticks: u128 = snapshot.resting_asks().map(|o| o.ticks).sum();
    let best_ask = snapshot.resting_asks().map(|o| o.price_per_tick).min();
    let order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
    machine::MarketEntry {
        frame_model: snapshot.frame_model.clone(),
        model_hash: snapshot.model_hash.clone(),
        order_book: snapshot.order_book.clone(),
        root_model,
        active: snapshot.active(),
        order_count,
        ask_count: snapshot.resting_asks().count() as u128,
        depth_ticks: machine::amount(depth_ticks),
        best_ask: best_ask.map(machine::amount),
        min_liquidity: machine::amount(0u8),
        tick_size: machine::amount(DobParams::canonical().tick_size),
        source: source.to_string(),
    }
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_markets(args: MarketsArgs) -> Result<()> {
    if args.mock_chain {
        return run_markets_mock(args).await;
    }
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let targets = if args.market.is_empty() {
        let note_addr = args.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "markets without --market requires --note-addr to derive order-book addresses"
            )
        })?;
        let cfg = dexdo::seller::ModelsConfig::load(&args.models)?;
        cfg.models
            .values()
            .map(|m| BookTarget {
                frame_model: m.frame_model.clone(),
                model_hash: model_hash_for(&m.frame_model),
                order_book: None,
                root_model: None,
                note_addr: Some(note_addr.clone()),
            })
            .collect::<Vec<_>>()
    } else {
        args.market
            .iter()
            .map(|p| target_from_market(p))
            .collect::<Result<Vec<_>>>()?
    };
    if args.json {
        let mut markets = Vec::new();
        for target in targets {
            let source = if target.order_book.is_some() {
                "market_manifest"
            } else {
                "models_config"
            };
            let root_model = target.root_model.clone();
            let snapshot = read_executable_book_target(&chain, &target).await?;
            markets.push(market_entry_from_snapshot(&snapshot, root_model, source));
        }
        return machine::print_json(&machine::MarketsResponse {
            schema: machine::MARKETS_SCHEMA,
            network: "shellnet".to_string(),
            generated_at_unix: machine::now_unix()?,
            markets,
        });
    }
    for target in targets {
        let snapshot = read_executable_book_target(&chain, &target).await?;
        if let Some(policy) = registry_policy.as_ref() {
            let action = enforce_model_registry_policy(
                RegistryRole::Buyer,
                policy,
                &args.contracts,
                &target.frame_model,
                &snapshot.order_book,
                snapshot.active(),
                BuyerMissingBookPolicy::HideFromAvailableList,
            )
            .await?;
            if action == RegistryBookAction::BuyerHideMissing {
                continue;
            }
        }
        let depth_ticks: u128 = snapshot.resting_asks().map(|o| o.ticks).sum();
        let best_ask = snapshot.resting_asks().map(|o| o.price_per_tick).min();
        let order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
        println!(
            "model={} order_book={} active={} order_count={} ask_count={} depth_ticks={} best_ask={}",
            snapshot.frame_model,
            snapshot.order_book,
            snapshot.active(),
            order_count,
            snapshot.resting_asks().count(),
            depth_ticks,
            best_ask
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_markets(args: MarketsArgs) -> Result<()> {
    if args.mock_chain {
        return run_markets_mock(args).await;
    }
    bail!("markets unavailable: build with `--features shellnet`")
}

/// `dexdo market <canonical-model>` -- render ONE model's order book as the human-readable box table
/// (the same view the buyer shows before a buy). Read-only, keyed by the canonical model name.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_market(args: MarketArgs) -> Result<()> {
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    // The book is keyed by the canonical model: derive it from `--note-addr` (any active note supplies the
    // book code), or read it from a provision manifest. `market.json` is the seller's artifact -- a buyer
    // normally passes only the model name + its own `--note-addr`.
    let target = if let Some(market) = args.market.as_deref() {
        if args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --note-addr");
        }
        target_from_market_for_model(market, &args.models, &args.model)?
    } else {
        model_target_from_config(&args.models, &args.model, args.note_addr.clone()).map_err(|e| {
            anyhow::anyhow!("{e}\n(pass --note-addr 0:<your PrivateNote> so the per-model book can be derived)")
        })?
    };
    let snapshot = read_executable_book_target(&chain, &target).await?;
    if let Some(policy) = registry_policy.as_ref() {
        enforce_model_registry_policy(
            RegistryRole::Buyer,
            policy,
            &args.contracts,
            &target.frame_model,
            &snapshot.order_book,
            snapshot.active(),
            BuyerMissingBookPolicy::Reject,
        )
        .await?;
    }
    let rows: Vec<BookRow> = snapshot
        .resting_asks()
        .map(|o| BookRow {
            price_per_tick: o.price_per_tick,
            max_ticks: o.ticks,
            token_contract: o
                .token_contract
                .as_ref()
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
        })
        .collect();
    if rows.is_empty() {
        let raw_order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
        if raw_order_count > 0 {
            let tick_size = DobParams::canonical().tick_size;
            println!(
                "inference order book -- {}  (1 tick = {tick_size} model tokens)",
                snapshot.frame_model
            );
            println!(
                "  * no executable asks; raw order_count={raw_order_count} is blocked by stale/non-executable rows"
            );
            return Ok(());
        }
    }
    // Read-only discovery: no `--max-price-per-tick` ceiling, so the `exec` column stays blank(this is not a buy).
    print_book_table(&snapshot.frame_model, &rows, None, None);
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_market(_args: MarketArgs) -> Result<()> {
    bail!("market unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_quote(args: QuoteArgs) -> Result<()> {
    if args.mock_chain {
        return run_quote_mock(args).await;
    }
    if args.ticks.is_some() == args.budget.is_some() {
        bail!("quote requires exactly one of --ticks or --budget");
    }
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let target = if let Some(market) = args.market.as_deref() {
        if args.model.is_some() || args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --model/--note-addr for quote");
        }
        target_from_market(market)?
    } else {
        model_target_from_config(
            &args.models,
            args.model
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("quote without --market requires --model"))?,
            args.note_addr.clone(),
        )?
    };
    let snapshot = read_executable_book_target(&chain, &target).await?;
    if let Some(policy) = registry_policy.as_ref() {
        enforce_model_registry_policy(
            RegistryRole::Buyer,
            policy,
            &args.contracts,
            &target.frame_model,
            &snapshot.order_book,
            snapshot.active(),
            BuyerMissingBookPolicy::Reject,
        )
        .await?;
    }
    let q = executable_quote(&snapshot.orders, args.ticks, args.budget)
        .map_err(|e| anyhow::anyhow!("quote: {e}"))?;
    if args.json {
        return machine::print_json(&quote_response_from_quote(
            "shellnet",
            &snapshot.frame_model,
            &snapshot.order_book,
            args.ticks,
            args.budget,
            q,
        )?);
    }
    if q.filled_ticks == 0 {
        println!(
            "quote model={} order_book={} no_liquidity=true",
            snapshot.frame_model, snapshot.order_book
        );
        return Ok(());
    }
    println!(
        "quote model={} order_book={} filled_ticks={} total_with_fee={} complete={}",
        snapshot.frame_model, snapshot.order_book, q.filled_ticks, q.total_with_fee, q.complete
    );
    for fill in q.fills {
        println!(
            "fill order_id={} token_contract={} ticks={} price_per_tick={} cost_with_fee={}",
            fill.order_id, fill.token_contract, fill.ticks, fill.price_per_tick, fill.cost_with_fee
        );
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_quote(args: QuoteArgs) -> Result<()> {
    if args.mock_chain {
        return run_quote_mock(args).await;
    }
    bail!("quote unavailable: build with `--features shellnet`")
}

pub(crate) async fn run_market_data(args: MarketDataArgs) -> Result<()> {
    let base_url = indexer::resolve_base_url(args.indexer_url.as_deref())?;
    let timeout = indexer::timeout_from_ms(args.timeout_ms)?;
    let client = IndexerClient::new(base_url, timeout)?;
    match args.command {
        MarketDataCommand::List {
            producer,
            status,
            cursor,
            limit,
        } => {
            let response = client
                .markets(MarketsQuery {
                    inference_order_book_address: None,
                    producer: producer.as_deref(),
                    status: status.as_deref(),
                    cursor: cursor.as_deref(),
                    limit,
                })
                .await?;
            match args.output {
                MarketDataOutput::Table => {
                    print!(
                        "{}",
                        indexer::render_markets_table(&response, client.base_url())
                    );
                }
                MarketDataOutput::Json => {
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            }
        }
        MarketDataCommand::Show {
            inference_order_book_address,
        } => {
            let response = client
                .markets(MarketsQuery {
                    inference_order_book_address: Some(&inference_order_book_address),
                    producer: None,
                    status: None,
                    cursor: None,
                    limit: None,
                })
                .await?;
            let mut markets = response.markets.into_iter();
            let market = markets.next().ok_or_else(|| {
                anyhow::anyhow!(
                    "Dodex indexer returned no market for inferenceOrderBookAddress={}",
                    inference_order_book_address
                )
            })?;
            if markets.next().is_some() {
                bail!(
                    "Dodex indexer returned multiple markets for inferenceOrderBookAddress={}",
                    inference_order_book_address
                );
            }
            match args.output {
                MarketDataOutput::Table => {
                    print!("{}", indexer::render_market_table(&market));
                }
                MarketDataOutput::Json => {
                    println!("{}", serde_json::to_string_pretty(&market)?);
                }
            }
        }
        MarketDataCommand::Depth {
            inference_order_book_address,
            limit,
        } => {
            let response = client
                .depth(DepthQuery {
                    inference_order_book_address: &inference_order_book_address,
                    limit,
                })
                .await?;
            match args.output {
                MarketDataOutput::Table => {
                    print!(
                        "{}",
                        indexer::render_depth_table(&response, client.base_url())
                    );
                }
                MarketDataOutput::Json => {
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            }
        }
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_orders(args: OrdersArgs) -> Result<()> {
    let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
        anyhow::anyhow!("orders requires --note-addr (the owner PrivateNote to filter/cancel)")
    })?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let target = if let Some(market) = args.market.as_deref() {
        if args.model.is_some() {
            bail!("--market and --model are mutually exclusive for orders");
        }
        target_from_market(market)?
    } else {
        model_target_from_config(
            &args.models,
            args.model
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("orders without --market requires --model"))?,
            Some(note_addr.to_string()),
        )?
    };
    let snapshot = read_book_target(&chain, &target).await?;
    let own = own_orders(&snapshot, note_addr);
    match args.command {
        OrdersCommand::List => {
            if own.is_empty() {
                println!(
                    "orders model={} order_book={} owner={} none=true",
                    snapshot.frame_model, snapshot.order_book, note_addr
                );
            } else {
                for order in own {
                    println!("{}", render_order_line(order));
                }
            }
        }
        OrdersCommand::Show { order_id } => {
            let order = own
                .into_iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "order {order_id} is not a resting order owned by note {note_addr} in {}",
                        snapshot.order_book
                    )
                })?;
            println!("{}", render_order_line(order));
        }
        OrdersCommand::Cancel { order_id } => {
            let order = own
                .into_iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "refusing to cancel: order {order_id} is not owned by note {note_addr} in {}",
                        snapshot.order_book
                    )
                })?;
            let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "orders cancel requires --note-key to sign the PrivateNote owner method"
                )
            })?;
            let note = dexdo_core::Address::parse(note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let keys = dexdo_core::KeyPair::from_secret_hex(
                read_secret_hex(note_key, "--note-key")?.trim(),
            )
            .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            chain
                .assert_note_owner_matches("orders cancel", &note, &keys)
                .await?;
            chain
                .cancel_inference_order(&note, &keys, &target.model_hash, order.order_id)
                .await?;
            println!(
                "cancel submitted model={} order_book={} order_id={} owner={}",
                snapshot.frame_model, snapshot.order_book, order.order_id, note_addr
            );
        }
        OrdersCommand::CancelAll => {
            if own.is_empty() {
                bail!(
                    "refusing to cancel-all: note {note_addr} has no resting orders in {}",
                    snapshot.order_book
                );
            }
            let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "orders cancel-all requires --note-key to sign the PrivateNote owner method"
                )
            })?;
            let note = dexdo_core::Address::parse(note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let keys = dexdo_core::KeyPair::from_secret_hex(
                read_secret_hex(note_key, "--note-key")?.trim(),
            )
            .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            chain
                .assert_note_owner_matches("orders cancel-all", &note, &keys)
                .await?;
            chain
                .cancel_all_inference_orders(&note, &keys, &target.model_hash)
                .await?;
            println!(
                "cancel-all submitted model={} order_book={} owner={} order_count={}",
                snapshot.frame_model,
                snapshot.order_book,
                note_addr,
                own.len()
            );
        }
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_orders(_args: OrdersArgs) -> Result<()> {
    bail!("orders unavailable: build with `--features shellnet`")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
struct SubscriptionPlacePlan {
    ticks: u128,
    escrow: u128,
    unused_budget: u128,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
fn subscription_place_plan(args: &SubscriptionPlaceArgs) -> Result<SubscriptionPlacePlan> {
    if args.max_price_per_tick == 0 {
        bail!("subscription place requires --max-price-per-tick > 0");
    }
    match (args.ticks, args.budget) {
        (Some(_), Some(_)) | (None, None) => {
            bail!("subscription place requires exactly one of --ticks or --budget")
        }
        (Some(ticks), None) => {
            if ticks == 0 {
                bail!("subscription place requires --ticks > 0");
            }
            let escrow = required_escrow_for_buy(ticks, args.max_price_per_tick);
            check_buy_deposit_headroom(escrow, ticks, args.max_price_per_tick)
                .map_err(|e| anyhow::anyhow!("subscription escrow: {e}"))?;
            Ok(SubscriptionPlacePlan {
                ticks,
                escrow,
                unused_budget: 0,
            })
        }
        (None, Some(budget)) => {
            if budget == 0 {
                bail!("subscription place requires --budget > 0");
            }
            let unit = required_escrow_for_buy(1, args.max_price_per_tick);
            check_buy_deposit_headroom(unit, 1, args.max_price_per_tick)
                .map_err(|e| anyhow::anyhow!("subscription budget: {e}"))?;
            let ticks = budget / unit;
            if ticks == 0 {
                bail!(
                    "subscription budget {budget} buys zero whole ticks at maxPricePerTick {} \
                     (fee-inclusive unit {unit})",
                    args.max_price_per_tick
                );
            }
            let escrow = required_escrow_for_buy(ticks, args.max_price_per_tick);
            check_buy_deposit_headroom(escrow, ticks, args.max_price_per_tick)
                .map_err(|e| anyhow::anyhow!("subscription escrow: {e}"))?;
            Ok(SubscriptionPlacePlan {
                ticks,
                escrow,
                unused_budget: budget.saturating_sub(escrow),
            })
        }
    }
}

#[cfg(feature = "shellnet")]
fn subscription_target(args: &SubscriptionArgs) -> Result<BookTarget> {
    if let Some(market) = args.market.as_deref() {
        if args.model.is_some() {
            bail!("--market and --model are mutually exclusive for subscription");
        }
        target_from_market(market)
    } else {
        let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "subscription without --market requires --note-addr to derive the order-book address"
            )
        })?;
        model_target_from_config(
            &args.models,
            args.model
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("subscription without --market requires --model"))?,
            Some(note_addr),
        )
    }
}

#[cfg(feature = "shellnet")]
fn require_subscription_note(args: &SubscriptionArgs, action: &str) -> Result<dexdo_core::Address> {
    let note_addr = args
        .identity
        .note_addr
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("subscription {action} requires --note-addr"))?;
    dexdo_core::Address::parse(note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))
}

#[cfg(feature = "shellnet")]
fn require_subscription_keys(args: &SubscriptionArgs, action: &str) -> Result<dexdo_core::KeyPair> {
    let note_key = args
        .identity
        .note_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("subscription {action} requires --note-key"))?;
    dexdo_core::KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))
}

#[cfg(feature = "shellnet")]
fn order_owned_by_note(order: &OrderBookOrder, note_addr: &str) -> bool {
    let want = dexdo_core::normalize_wallet_address(note_addr)
        .unwrap_or_else(|_| note_addr.trim().to_string());
    dexdo_core::normalize_wallet_address(&order.owner_note)
        .map(|owner| owner == want)
        .unwrap_or_else(|_| order.owner_note.eq_ignore_ascii_case(&want))
}

#[cfg(feature = "shellnet")]
fn render_subscription_line(
    snapshot: &OrderBookSnapshot,
    order_id: u128,
    order: Option<&OrderBookOrder>,
    sub: Option<&OrderBookSubscription>,
) -> String {
    let Some(sub) = sub else {
        return format!(
            "subscription model={} order_book={} order_id={} book_active={} exists=false order_found={}",
            snapshot.frame_model,
            snapshot.order_book,
            order_id,
            snapshot.active(),
            order.is_some()
        );
    };
    let Some(order) = order else {
        let stale = sub.exists;
        return format!(
            "subscription model={} order_book={} order_id={} exists={} order_found=false stale_subscription={} period_start={} cur_cycle={} cycle_budget={} cycle_spent={} cycle_remaining={} auto_renew={}",
            snapshot.frame_model,
            snapshot.order_book,
            order_id,
            sub.exists,
            stale,
            sub.period_start,
            sub.cur_cycle,
            sub.cycle_budget,
            sub.cycle_spent,
            sub.cycle_remaining(),
            sub.auto_renew
        );
    };
    format!(
        "subscription model={} order_book={} order_id={} exists={} owner={} price_per_tick={} ticks={} escrow={} deadline={} period_start={} cur_cycle={} cycle_budget={} cycle_spent={} cycle_remaining={} auto_renew={}",
        snapshot.frame_model,
        snapshot.order_book,
        order_id,
        sub.exists,
        order.owner_note,
        order.price_per_tick,
        order.ticks,
        order.escrow,
        order.deadline,
        sub.period_start,
        sub.cur_cycle,
        sub.cycle_budget,
        sub.cycle_spent,
        sub.cycle_remaining(),
        sub.auto_renew
    )
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_subscription(args: SubscriptionArgs) -> Result<()> {
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
    let target = subscription_target(&args)?;
    let snapshot = read_book_target(&chain, &target).await?;
    if matches!(args.command, SubscriptionCommand::Place(_)) {
        if let Some(policy) = registry_policy.as_ref() {
            enforce_model_registry_policy(
                RegistryRole::Buyer,
                policy,
                &args.contracts,
                &target.frame_model,
                &snapshot.order_book,
                snapshot.active(),
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
    }
    if !snapshot.active() {
        bail!(
            "subscription: InferenceOrderBook {} for model {} is not active; run `dexdo deploy-market` or `dexdo provision` first",
            snapshot.order_book,
            snapshot.frame_model
        );
    }
    let ob = dexdo_core::Address::parse(&snapshot.order_book)
        .map_err(|e| anyhow::anyhow!("order_book {}: {e}", snapshot.order_book))?;

    match &args.command {
        SubscriptionCommand::Place(place) => {
            let note_addr = args
                .identity
                .note_addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("subscription place requires --note-addr"))?;
            let note = require_subscription_note(&args, "place")?;
            let keys = require_subscription_keys(&args, "place")?;
            chain
                .assert_note_owner_matches("subscription place", &note, &keys)
                .await?;
            let plan = subscription_place_plan(place)?;
            let expected_order_id = snapshot
                .stats
                .as_ref()
                .map(|s| s.next_order_id)
                .unwrap_or(0);
            chain
                .place_inference_subscription(
                    &note,
                    &keys,
                    &target.model_hash,
                    place.max_price_per_tick,
                    plan.ticks,
                    plan.escrow,
                    place.auto_renew,
                )
                .await?;
            println!(
                "subscription place submitted model={} order_book={} owner={} expected_order_id={} max_price_per_tick={} ticks={} escrow={} unused_budget={} auto_renew={}",
                snapshot.frame_model,
                snapshot.order_book,
                note_addr,
                expected_order_id,
                place.max_price_per_tick,
                plan.ticks,
                plan.escrow,
                plan.unused_budget,
                place.auto_renew
            );
        }
        SubscriptionCommand::Status { order_id } => {
            let order_id = *order_id;
            let order = snapshot.orders.iter().find(|o| o.order_id == order_id);
            if let Some(note_addr) = args.identity.note_addr.as_deref() {
                if let Some(order) = order {
                    if !order_owned_by_note(order, note_addr) {
                        bail!(
                            "subscription status: order {order_id} is owned by {}, not note {note_addr}",
                            order.owner_note
                        );
                    }
                }
            }
            let sub = chain
                .inference_orderbook_subscription(&ob, order_id)
                .await?;
            println!(
                "{}",
                render_subscription_line(&snapshot, order_id, order, sub.as_ref())
            );
        }
        SubscriptionCommand::Cancel { order_id } => {
            let order_id = *order_id;
            let note_addr = args
                .identity
                .note_addr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("subscription cancel requires --note-addr"))?;
            let order = snapshot
                .orders
                .iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "refusing to cancel subscription: order {order_id} is not resting in {}",
                        snapshot.order_book
                    )
                })?;
            if !order_owned_by_note(order, note_addr) {
                bail!(
                    "refusing to cancel subscription: order {order_id} is owned by {}, not note {note_addr}",
                    order.owner_note
                );
            }
            let sub = chain
                .inference_orderbook_subscription(&ob, order_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "refusing to cancel subscription: could not read getSubscription({order_id})"
                    )
                })?;
            if !sub.exists {
                bail!(
                    "refusing to cancel subscription: order {order_id} is not a live subscription"
                );
            }
            let note = require_subscription_note(&args, "cancel")?;
            let keys = require_subscription_keys(&args, "cancel")?;
            chain
                .assert_note_owner_matches("subscription cancel", &note, &keys)
                .await?;
            chain
                .cancel_inference_order(&note, &keys, &target.model_hash, order_id)
                .await?;
            println!(
                "subscription cancel submitted model={} order_book={} order_id={} owner={} cycle={} cycle_remaining={}",
                snapshot.frame_model,
                snapshot.order_book,
                order_id,
                note_addr,
                sub.cur_cycle,
                sub.cycle_remaining()
            );
        }
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_subscription(_args: SubscriptionArgs) -> Result<()> {
    bail!("subscription unavailable: build with `--features shellnet`")
}

pub(crate) async fn run_deals(args: DealsArgs) -> Result<()> {
    let dir = deals::resolve_deals_dir(args.deals_dir.as_deref())?;
    let handles = deals::list_deal_handles(&dir)?;
    if handles.is_empty() {
        println!("deals dir={} none=true", dir.display());
        return Ok(());
    }
    for (path, h) in handles {
        println!(
            "handle={} role={} network={} note={} model={} token_contract={} order_book={} path={}",
            h.handle,
            h.role.as_str(),
            h.network,
            h.note_addr,
            h.frame_model,
            h.token_contract,
            h.order_book.as_deref().unwrap_or("-"),
            path.display()
        );
    }
    Ok(())
}

pub(crate) async fn run_history(args: HistoryArgs) -> Result<()> {
    let dir = deals::resolve_deals_dir(args.deals_dir.as_deref())?;
    let handles = deals::list_deal_handles(&dir)?;
    let mut shown = 0usize;
    for (path, h) in handles {
        if !audit::history_handle_matches(&h, args.note.as_deref(), args.model.as_deref()) {
            continue;
        }
        shown += 1;
        println!(
            "history handle={} role={} network={} note={} model={} model_hash={} token_contract={} order_book={} created_at={} order_ids={} path={}",
            h.handle,
            h.role.as_str(),
            h.network,
            h.note_addr,
            h.frame_model,
            h.model_hash.as_deref().unwrap_or("-"),
            h.token_contract,
            h.order_book.as_deref().unwrap_or("-"),
            h.created_at_unix,
            if h.created_order_ids.is_empty() {
                "-".to_string()
            } else {
                h.created_order_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            },
            path.display()
        );
    }
    if shown == 0 {
        println!(
            "history dir={} none=true note={} model={}",
            dir.display(),
            args.note.as_deref().unwrap_or("-"),
            args.model.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

pub(crate) async fn run_dashboard(args: DashboardArgs) -> Result<()> {
    dashboard::ensure_loopback(args.listen)?;
    let dir = deals::resolve_deals_dir(args.deals_dir.as_deref())?;
    #[cfg(feature = "shellnet")]
    let state = dashboard::DashboardAppState::shellnet(dir);
    #[cfg(not(feature = "shellnet"))]
    let state = dashboard::DashboardAppState::local(dir);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let addr = dashboard::bind_dashboard(args.listen, state, async move {
        let _ = shutdown_rx.await;
    })
    .await?;
    println!(
        "dashboard_url=http://{addr}/ json=http://{addr}{} read_only=true",
        dashboard::DASHBOARD_JSON_PATH
    );
    operator_shutdown_signal().await;
    let _ = shutdown_tx.send(());
    Ok(())
}

fn role_arg_str(role: DealRoleArg) -> &'static str {
    match role {
        DealRoleArg::Buyer => "buyer",
        DealRoleArg::Seller => "seller",
    }
}

fn handle_role_to_arg(role: deals::DealHandleRole) -> DealRoleArg {
    match role {
        deals::DealHandleRole::Buyer => DealRoleArg::Buyer,
        deals::DealHandleRole::Seller => DealRoleArg::Seller,
    }
}

struct MockDealTarget {
    handle: Option<deals::DealHandle>,
    token_contract: String,
    role: Option<DealRoleArg>,
    note_addr: Option<String>,
    frame_model: Option<String>,
}

fn resolve_mock_deal_target(
    input: &str,
    deals_dir: Option<&std::path::Path>,
    raw_role: Option<DealRoleArg>,
    raw_note_addr: Option<String>,
) -> Result<MockDealTarget> {
    let dir = deals::resolve_deals_dir(deals_dir)?;
    if let Some((_path, handle)) = deals::resolve_deal_ref(input, &dir)? {
        return Ok(MockDealTarget {
            token_contract: handle.token_contract.clone(),
            role: Some(handle_role_to_arg(handle.role)),
            note_addr: Some(handle.note_addr.clone()),
            frame_model: Some(handle.frame_model.clone()),
            handle: Some(handle),
        });
    }
    Ok(MockDealTarget {
        handle: None,
        token_contract: input.to_string(),
        role: raw_role,
        note_addr: raw_note_addr,
        frame_model: None,
    })
}

fn status_next_for(
    role: Option<&str>,
    state: &str,
    funded: bool,
    opened: bool,
    probe_accepted: bool,
) -> machine::StatusNext {
    let action = match (role, state, funded, opened, probe_accepted) {
        (_, "closed", _, _, _) => "none",
        (Some("seller"), "stopped", _, _, _) => "destroy",
        (Some("seller"), _, _, true, _) => "wait_for_buyer_stop",
        (Some("seller"), _, true, false, false) => "buyer_cleanup_after_timeout",
        (Some("buyer"), _, _, true, _) => "stream_stop_or_reclaim_after_timeout",
        (Some("buyer"), _, true, false, false) => "cleanup_unopened_after_timeout",
        (Some("buyer"), "stopped", _, _, _) => "seller_destroy",
        (Some("buyer"), _, _, _, _) => "cancel_resting_bid_or_wait_match",
        _ => "unknown_role",
    };
    machine::StatusNext {
        action: action.to_string(),
        retryable_after_unix: None,
        command: if action == "none" {
            "none".to_string()
        } else {
            "close".to_string()
        },
    }
}

fn status_response_from_summary(
    network: &str,
    handle: Option<String>,
    role: Option<String>,
    token_contract: String,
    frame_model: Option<String>,
    state: &str,
    active: bool,
    s: &deals::DealStateSummary,
) -> Result<machine::StatusResponse> {
    Ok(machine::StatusResponse {
        schema: machine::STATUS_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        handle,
        role: role.clone(),
        token_contract,
        frame_model,
        state: state.to_string(),
        active,
        funded: s.funded,
        opened: s.opened,
        disputed: s.disputed,
        probe_accepted: s.probe_accepted,
        accounting: machine::StatusAccounting {
            finalized_owed: machine::amount(s.finalized_owed),
            buyer_locked: machine::amount(s.buyer_locked()),
            deposit: machine::amount(s.deposit),
            prepaid: machine::amount(s.prepaid),
            frozen: machine::amount(s.frozen),
            last_advance_unix: Some(s.last_advance).filter(|v| *v != 0),
            funded_time_unix: s.funded_time,
        },
        next: status_next_for(role.as_deref(), state, s.funded, s.opened, s.probe_accepted),
    })
}

fn closed_status_response(
    network: &str,
    handle: Option<String>,
    role: Option<String>,
    token_contract: String,
    frame_model: Option<String>,
) -> Result<machine::StatusResponse> {
    let s = deals::DealStateSummary {
        kind: deals::DealStateKind::Stopped,
        funded: false,
        opened: false,
        disputed: false,
        probe_accepted: false,
        deposit: 0,
        prepaid: 0,
        frozen: 0,
        finalized_owed: 0,
        funded_time: None,
        last_advance: 0,
    };
    status_response_from_summary(
        network,
        handle,
        role,
        token_contract,
        frame_model,
        "closed",
        false,
        &s,
    )
}

fn mock_summary_from_snapshot(snapshot: &dexdo_core::StreamSnapshot) -> deals::DealStateSummary {
    let kind = if snapshot.closed {
        deals::DealStateKind::Stopped
    } else if snapshot.seller_received > 0 {
        deals::DealStateKind::Streaming
    } else {
        deals::DealStateKind::Probe
    };
    deals::DealStateSummary {
        kind,
        funded: !snapshot.closed,
        opened: !snapshot.closed,
        disputed: false,
        probe_accepted: snapshot.seller_received > 0,
        deposit: 0,
        prepaid: 0,
        frozen: u128::from(snapshot.buyer_locked),
        finalized_owed: u128::from(snapshot.seller_received),
        funded_time: None,
        last_advance: 0,
    }
}

async fn run_status_mock(args: StatusArgs) -> Result<()> {
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let target = resolve_mock_deal_target(&args.deal, args.deals_dir.as_deref(), None, None)?;
    let handle = target.handle.as_ref().map(|h| h.handle.clone());
    let role = target.role.map(|r| role_arg_str(r).to_string());
    let frame_model = target.frame_model.clone();
    let snapshot = chain.snapshot(&target.token_contract).await;
    if args.json {
        let response = match snapshot {
            Some(snapshot) if !snapshot.closed => {
                let s = mock_summary_from_snapshot(&snapshot);
                let state = s.kind.as_str();
                status_response_from_summary(
                    "mock",
                    handle,
                    role,
                    target.token_contract,
                    frame_model,
                    state,
                    true,
                    &s,
                )?
            }
            _ => closed_status_response("mock", handle, role, target.token_contract, frame_model)?,
        };
        return machine::print_json(&response);
    }
    match snapshot {
        Some(snapshot) if !snapshot.closed => {
            let s = mock_summary_from_snapshot(&snapshot);
            println!(
                "status handle=(raw) role=unknown token_contract={} state={} active=true funded={} opened={} disputed=false probe_accepted={}",
                target.token_contract,
                s.kind.as_str(),
                s.funded,
                s.opened,
                s.probe_accepted
            );
        }
        _ => println!(
            "status handle=(raw) role=unknown token_contract={} state=closed active=false",
            target.token_contract
        ),
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_status(args: StatusArgs) -> Result<()> {
    if args.mock_chain {
        return run_status_mock(args).await;
    }
    use dexdo_core::{Address, RealChainBackend};
    let target = load_deal_target(&args.deal, args.deals_dir.as_deref(), None, None)?;
    let contracts_path = deal_contracts_path(args.contracts.as_deref(), &target);
    shellnet_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    let contracts = args
        .contracts
        .as_deref()
        .unwrap_or(&contracts_path)
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = Address::parse(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let Some(state) = chain.token_contract_state(&tc).await? else {
        if args.json {
            return machine::print_json(&closed_status_response(
                "shellnet",
                target.handle.as_ref().map(|h| h.handle.clone()),
                target.role.map(|r| r.as_str().to_string()),
                target.token_contract,
                target.handle.as_ref().map(|h| h.frame_model.clone()),
            )?);
        }
        println!(
            "status handle={} role={} token_contract={} state=closed active=false",
            target
                .handle
                .as_ref()
                .map(|h| h.handle.as_str())
                .unwrap_or("(raw)"),
            target.role.map(|r| r.as_str()).unwrap_or("unknown"),
            target.token_contract
        );
        return Ok(());
    };
    let s = deals::classify_deal_state(&state);
    if args.json {
        return machine::print_json(&status_response_from_summary(
            "shellnet",
            target.handle.as_ref().map(|h| h.handle.clone()),
            target.role.map(|r| r.as_str().to_string()),
            target.token_contract.clone(),
            target.handle.as_ref().map(|h| h.frame_model.clone()),
            s.kind.as_str(),
            true,
            &s,
        )?);
    }
    println!(
        "status handle={} role={} token_contract={} state={} active=true funded={} opened={} disputed={} probe_accepted={}",
        target
            .handle
            .as_ref()
            .map(|h| h.handle.as_str())
            .unwrap_or("(raw)"),
        target.role.map(|r| r.as_str()).unwrap_or("unknown"),
        target.token_contract,
        s.kind.as_str(),
        s.funded,
        s.opened,
        s.disputed,
        s.probe_accepted
    );
    if let Some(h) = &target.handle {
        println!(
            "context network={} note={} model={} order_book={} root_model={}",
            h.network,
            h.note_addr,
            h.frame_model,
            h.order_book.as_deref().unwrap_or("-"),
            h.root_model.as_deref().unwrap_or("-")
        );
    }
    println!(
        "accounting finalized_owed={} buyer_locked={} deposit={} prepaid={} frozen={} last_advance={} funded_time={}",
        s.finalized_owed,
        s.buyer_locked(),
        s.deposit,
        s.prepaid,
        s.frozen,
        s.last_advance,
        s.funded_time
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("{}", close_hint(&target, &s));
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_status(args: StatusArgs) -> Result<()> {
    if args.mock_chain {
        return run_status_mock(args).await;
    }
    bail!("status unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_export(args: ExportArgs) -> Result<()> {
    use dexdo_core::{Address, RealChainBackend};
    let target = load_deal_target(&args.deal, args.deals_dir.as_deref(), None, None)?;
    let contracts_path = deal_contracts_path(args.contracts.as_deref(), &target);
    shellnet_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    let contracts = contracts_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = Address::parse(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let state = chain.token_contract_state(&tc).await?;
    let active = state.is_some();
    let summary = state.as_ref().map(deals::classify_deal_state);
    let (onchain_model, onchain_model_hash, onchain_buyer_note, deal_terms) = if active {
        let model = chain.token_contract_model_name(&tc).await?;
        let model_hash = chain.token_contract_model_hash(&tc).await?;
        let buyer_note = chain
            .token_contract_buyer_note(&tc)
            .await?
            .map(|a| a.with_workchain());
        let terms = chain.token_contract_deal_terms(&tc).await?.map(
            |(tick_size, price_per_tick, max_ticks)| audit::DealTermsAudit {
                tick_size,
                price_per_tick,
                max_ticks,
            },
        );
        (model, model_hash, buyer_note, terms)
    } else {
        (None, None, None, None)
    };
    let generated_at_unix = deals::now_unix()?;
    let export = audit::build_deal_audit(audit::DealAuditBuild {
        generated_at_unix,
        handle: target.handle.clone(),
        role: target.role,
        token_contract: target.token_contract.clone(),
        note_addr: target.note_addr.clone(),
        contracts: contracts_path.display().to_string(),
        active,
        state,
        summary,
        onchain_model,
        onchain_model_hash,
        onchain_buyer_note,
        deal_terms,
    });
    match args.format {
        ExportFormatArg::Json => println!("{}", serde_json::to_string_pretty(&export)?),
        ExportFormatArg::Md => print!("{}", audit::render_markdown(&export)),
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_export(_args: ExportArgs) -> Result<()> {
    bail!("export unavailable: build with `--features shellnet`")
}

fn close_response(
    network: &str,
    handle: Option<String>,
    role: &str,
    token_contract: String,
    action: &str,
    submitted: bool,
    terminal: bool,
    reason: Option<&str>,
    state_before: &str,
    state_after: &str,
) -> Result<machine::CloseResponse> {
    Ok(machine::CloseResponse {
        schema: machine::CLOSE_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        handle,
        role: role.to_string(),
        token_contract,
        action: action.to_string(),
        submitted,
        terminal,
        reason: reason.map(str::to_string),
        state_before: state_before.to_string(),
        state_after: state_after.to_string(),
        tx: None,
    })
}

async fn run_close_mock(args: CloseArgs) -> Result<()> {
    let target = resolve_mock_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let role = target.role.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --role buyer|seller with a raw TokenContract",
            args.deal
        )
    })?;
    if target.note_addr.is_none() {
        bail!(
            "close: `{}` is not a local handle; pass --note-addr with a raw TokenContract",
            args.deal
        );
    }
    let role_s = role_arg_str(role);
    let handle = target.handle.as_ref().map(|h| h.handle.clone());
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let snapshot = chain.snapshot(&target.token_contract).await;
    match snapshot {
        None => {
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_closed"),
                "closed",
                "closed",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: TokenContract {} is inactive/closed",
                response.token_contract
            );
            Ok(())
        }
        Some(snapshot) if snapshot.closed => {
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_stopped"),
                "stopped",
                "stopped",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: {} side already STOPped for {}",
                role_s, response.token_contract
            );
            Ok(())
        }
        Some(snapshot) => {
            if role != DealRoleArg::Buyer {
                bail!(
                    "close: seller cannot destroy opened deal {}. Buyer must STOP/recover/reclaim first.",
                    target.token_contract
                );
            }
            let state_before = if snapshot.seller_received > 0 {
                "streaming"
            } else {
                "probe"
            };
            let note = dexdo_core::LocalNote::generate();
            chain.stop(&target.token_contract, &note).await?;
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "streamStop",
                true,
                false,
                None,
                state_before,
                "stopped",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close submitted role=buyer action=streamStop token_contract={}",
                response.token_contract
            );
            Ok(())
        }
    }
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_close(args: CloseArgs) -> Result<()> {
    if args.mock_chain {
        return run_close_mock(args).await;
    }
    use dexdo_core::{
        check_reclaimable, check_recoverable, keypair_ed_pubkey, Address, KeyPair,
        RealChainBackend, MATCH_OPEN_TIMEOUT_SECS,
    };
    let target = load_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let role = target.role.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --role buyer|seller with a raw TokenContract",
            args.deal
        )
    })?;
    let note_addr = target.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --note-addr with a raw TokenContract",
            args.deal
        )
    })?;
    if let (Some(handle), Some(arg_note)) = (&target.handle, args.note_addr.as_deref()) {
        if deals::normalize_addr(&handle.note_addr) != deals::normalize_addr(arg_note) {
            bail!(
                "close: --note-addr {arg_note} does not match handle {} note {}",
                handle.handle,
                handle.note_addr
            );
        }
    }
    let contracts_path = deal_contracts_path(args.contracts.as_deref(), &target);
    shellnet_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    let contracts = args
        .contracts
        .as_deref()
        .unwrap_or(&contracts_path)
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = Address::parse(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let Some(state) = chain.token_contract_state(&tc).await? else {
        if args.json {
            return machine::print_json(&close_response(
                "shellnet",
                target.handle.as_ref().map(|h| h.handle.clone()),
                role.as_str(),
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_closed"),
                "closed",
                "closed",
            )?);
        }
        println!(
            "close noop: TokenContract {} is inactive/closed",
            target.token_contract
        );
        return Ok(());
    };
    let s = deals::classify_deal_state(&state);
    match role {
        deals::DealHandleRole::Seller => {
            if s.disputed {
                bail!(
                    "close: seller deal {} is disputed; seller-side release is tracked by . Next command \
                     once exposed: `dexdo release-dispute {}`.",
                    target.token_contract,
                    target
                        .handle
                        .as_ref()
                        .map(|h| h.handle.as_str())
                        .unwrap_or(&target.token_contract)
                );
            }
            if s.opened {
                bail!(
                    "close: seller cannot destroy opened deal {}. Buyer must STOP/recover/reclaim first. Next: \
                     buyer runs `dexdo close <buyer-handle> --note-key <buyer-key>` or `dexdo reclaim --token-contract {}` \
                     when timeout permits.",
                    target.token_contract,
                    target.token_contract
                );
            }
            if s.kind != deals::DealStateKind::Stopped {
                bail!("{}", close_hint(&target, &s));
            }
            let note_key = args.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!("close seller requires --note-key to sign destroy")
            })?;
            let keys = KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            let note = Address::parse(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            chain.destroy_token_contract(&tc, &note, &keys).await?;
            if args.json {
                return machine::print_json(&close_response(
                    "shellnet",
                    target.handle.as_ref().map(|h| h.handle.clone()),
                    role.as_str(),
                    target.token_contract.clone(),
                    "destroy",
                    true,
                    true,
                    None,
                    s.kind.as_str(),
                    "closed",
                )?);
            }
            println!(
                "close submitted role=seller action=destroy token_contract={} note={}",
                target.token_contract, note
            );
        }
        deals::DealHandleRole::Buyer => {
            if s.disputed {
                bail!(
                    "close: buyer deal {} is disputed; wait for seller release/arbitration (), then re-run status.",
                    target.token_contract
                );
            }
            if s.kind == deals::DealStateKind::Stopped {
                if args.json {
                    return machine::print_json(&close_response(
                        "shellnet",
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "noop",
                        false,
                        false,
                        Some("already_stopped"),
                        "stopped",
                        "stopped",
                    )?);
                }
                println!(
                    "close noop: buyer side already STOPped for {}. Next: seller runs `dexdo close <seller-handle> --note-key <seller-key>`.",
                    target.token_contract
                );
                return Ok(());
            }
            let note_key = args.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!("close buyer requires --note-key to sign note owner method")
            })?;
            let keys = KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            let note = Address::parse(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let buyer_note = chain.token_contract_buyer_note(&tc).await?;
            let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
            let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
            let note_ed = keypair_ed_pubkey(&keys)?;
            if s.opened {
                let cfg = chain.token_contract_config(&tc).await?.ok_or_else(|| {
                    anyhow::anyhow!("close: TokenContract {} getConfig unavailable", tc)
                })?;
                let stream_timeout = cfg["streamTimeout"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| anyhow::anyhow!("close: getConfig exposes no streamTimeout"))?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
                    .as_secs();
                match buyer_opened_close_action(now, s.last_advance, stream_timeout) {
                    BuyerOpenedCloseAction::StreamReclaim => {
                        check_reclaimable(
                            s.funded,
                            s.opened,
                            s.disputed,
                            buyer_note_s.as_deref(),
                            &note.with_workchain(),
                            buyer_pubkey.as_ref(),
                            &note_ed,
                            now,
                            s.last_advance,
                            Some(stream_timeout),
                            s.funded_time,
                            MATCH_OPEN_TIMEOUT_SECS,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                        chain.reclaim_on_timeout(&note, &keys, &tc).await?;
                        if args.json {
                            return machine::print_json(&close_response(
                                "shellnet",
                                target.handle.as_ref().map(|h| h.handle.clone()),
                                role.as_str(),
                                target.token_contract.clone(),
                                "streamReclaim",
                                true,
                                false,
                                None,
                                s.kind.as_str(),
                                "stopped",
                            )?);
                        }
                        println!(
                            "close submitted role=buyer action=streamReclaim token_contract={} note={}",
                            target.token_contract, note
                        );
                    }
                    BuyerOpenedCloseAction::StreamStop => {
                        check_recoverable(
                            s.opened,
                            s.disputed,
                            buyer_note_s.as_deref(),
                            &note.with_workchain(),
                            buyer_pubkey.as_ref(),
                            &note_ed,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                        chain.stream_stop(&note, &keys, &tc).await?;
                        if args.json {
                            return machine::print_json(&close_response(
                                "shellnet",
                                target.handle.as_ref().map(|h| h.handle.clone()),
                                role.as_str(),
                                target.token_contract.clone(),
                                "streamStop",
                                true,
                                false,
                                None,
                                s.kind.as_str(),
                                "stopped",
                            )?);
                        }
                        println!(
                            "close submitted role=buyer action=streamStop token_contract={} note={}",
                            target.token_contract, note
                        );
                    }
                }
                return Ok(());
            }
            if s.funded && !s.probe_accepted {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
                    .as_secs();
                check_reclaimable(
                    s.funded,
                    s.opened,
                    s.disputed,
                    buyer_note_s.as_deref(),
                    &note.with_workchain(),
                    buyer_pubkey.as_ref(),
                    &note_ed,
                    now,
                    s.last_advance,
                    None,
                    s.funded_time,
                    MATCH_OPEN_TIMEOUT_SECS,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{e}. Next: re-run `dexdo close {}` after MATCH_OPEN_TIMEOUT, or inspect with `dexdo status {}`.",
                        args.deal,
                        args.deal
                    )
                })?;
                chain.stream_cleanup(&note, &keys, &tc).await?;
                if args.json {
                    return machine::print_json(&close_response(
                        "shellnet",
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "streamCleanup",
                        true,
                        false,
                        None,
                        s.kind.as_str(),
                        "stopped",
                    )?);
                }
                println!(
                    "close submitted role=buyer action=streamCleanup token_contract={} note={}",
                    target.token_contract, note
                );
                return Ok(());
            }
            bail!("{}", close_hint(&target, &s));
        }
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_close(args: CloseArgs) -> Result<()> {
    if args.mock_chain {
        return run_close_mock(args).await;
    }
    bail!("close unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuyerOpenedCloseAction {
    StreamStop,
    StreamReclaim,
}

#[cfg(feature = "shellnet")]
fn buyer_opened_close_action(
    now: u64,
    last_advance: u64,
    stream_timeout: u64,
) -> BuyerOpenedCloseAction {
    if now >= last_advance.saturating_add(stream_timeout) {
        BuyerOpenedCloseAction::StreamReclaim
    } else {
        BuyerOpenedCloseAction::StreamStop
    }
}

#[cfg(feature = "shellnet")]
fn close_hint(target: &DealTarget, s: &deals::DealStateSummary) -> String {
    let deal = target
        .handle
        .as_ref()
        .map(|h| h.handle.as_str())
        .unwrap_or(&target.token_contract);
    match target.role {
        Some(deals::DealHandleRole::Seller) if s.kind == deals::DealStateKind::Stopped => {
            format!("next=destroy command=`dexdo close {deal} --note-key <seller-key>`")
        }
        Some(deals::DealHandleRole::Seller) if s.opened => {
            format!(
                "next=wait_for_buyer_stop command=`dexdo status {deal}`; buyer may run `dexdo close <buyer-handle> --note-key <buyer-key>`"
            )
        }
        Some(deals::DealHandleRole::Seller) if s.funded && !s.probe_accepted => {
            "next=buyer_cleanup_after_timeout command=`dexdo close <buyer-handle> --note-key <buyer-key>`"
                .to_string()
        }
        Some(deals::DealHandleRole::Seller) => {
            "next=no_destroy_yet reason=deal_not_stopped".to_string()
        }
        Some(deals::DealHandleRole::Buyer) if s.opened => format!(
            "next=stream_stop_or_reclaim_after_timeout command=`dexdo close {deal} --note-key <buyer-key>`"
        ),
        Some(deals::DealHandleRole::Buyer) if s.funded && !s.probe_accepted => {
            format!("next=cleanup_unopened_after_timeout command=`dexdo close {deal} --note-key <buyer-key>`")
        }
        Some(deals::DealHandleRole::Buyer) if s.kind == deals::DealStateKind::Stopped => {
            "next=seller_destroy reason=buyer_already_stopped".to_string()
        }
        Some(deals::DealHandleRole::Buyer) => {
            "next=cancel_resting_bid_or_wait_match reason=deal_not_funded".to_string()
        }
        None => "next=unknown_role pass_local_handle_or_--role".to_string(),
    }
}

fn enforce_seller_runtime_policy(policy: &policy::SellerRuntimePolicy) -> Result<()> {
    if policy.max_open_deals != 1 {
        bail!(
            "policy_action failure_class=seller.max_open_deals action=enforce token_contract=<not-posted> \
             state=pre_offer result=unsupported_max_open_deals requested={} supported=1; \
             current seller daemon owns exactly one per-deal TokenContract",
            policy.max_open_deals
        );
    }
    Ok(())
}

async fn apply_seller_dispute_policy(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    policy: &policy::SellerRuntimePolicy,
    reason: &str,
) -> Result<bool> {
    let Some(state) = chain.deal_state(token_contract).await? else {
        return Ok(false);
    };
    if !state.disputed {
        return Ok(false);
    }
    match policy.dispute_against_me {
        policy::SellerDisputeAgainstMeAction::ReleaseIfClean => {
            let settlement = chain.release_dispute(token_contract).await?;
            println!(
                "policy_action failure_class=dispute_against_me action=release_if_clean \
                 token_contract={token_contract} state=funded/opened/disputed result=release_dispute_submitted \
                 reason={reason} settlement={settlement:?}"
            );
            Ok(true)
        }
        policy::SellerDisputeAgainstMeAction::Hold => {
            bail!(
                "policy_action failure_class=dispute_against_me action=hold token_contract={token_contract} \
                 state=funded/opened/disputed result=no_release_submitted reason={reason}"
            );
        }
    }
}

enum SellerTerminalPolicyOutcome {
    StopServing,
}

fn apply_seller_terminal_policy(
    token_contract: &dexdo_core::TokenContract,
    policy: &policy::SellerRuntimePolicy,
    finalized: u128,
) -> Result<SellerTerminalPolicyOutcome> {
    if finalized == 0 {
        match policy.buyer_no_show {
            policy::SellerBuyerNoShowAction::CleanupAndRepublish => {
                bail!(
                    "policy_action failure_class=buyer_no_show action=cleanup_and_republish \
                     token_contract={token_contract} state=funded/opened result=policy_action_unsupported; \
                     seller runtime has no buyer-side cleanup_unopened signer or fresh TC/nonce republish factory"
                );
            }
            policy::SellerBuyerNoShowAction::CleanupAndRetire => {
                println!(
                    "policy_action failure_class=buyer_no_show action=cleanup_and_retire \
                     token_contract={token_contract} state=funded/opened result=retiring_gateway; \
                     cleanup_unopened is buyer-side and was not submitted by seller"
                );
                return Ok(SellerTerminalPolicyOutcome::StopServing);
            }
        }
    }
    match policy.after_deal_done {
        policy::SellerAfterDealDoneAction::Retire => {
            println!(
                "policy_action failure_class=after_deal_done action=retire token_contract={token_contract} \
                 state=closed result=retiring_gateway finalized_ticks={finalized}"
            );
            Ok(SellerTerminalPolicyOutcome::StopServing)
        }
        policy::SellerAfterDealDoneAction::Republish => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish token_contract={token_contract} \
                 state=closed result=policy_action_unsupported finalized_ticks={finalized}; \
                 current seller runtime cannot safely republish without a fresh per-deal TC/nonce"
            );
        }
        policy::SellerAfterDealDoneAction::RepublishWithBackoff => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish_with_backoff \
                 token_contract={token_contract} state=closed result=policy_action_unsupported \
                 finalized_ticks={finalized}; current seller runtime cannot safely republish without a fresh \
                 per-deal TC/nonce"
            );
        }
    }
}

pub(crate) async fn run_seller(args: SellerArgs) -> Result<()> {
    // Issue: the deal token_contract comes from `--market`(a provision manifest) or `--token-contract`.
    // The manifest's frame_model(if any) is validated against `--model` inside `seller_real_backend`.
    let (token_contract, market_frame_model, market_nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    // Review: the deal nonce comes from `--market`(the manifest) or the explicit `--nonce` flag --
    // never both(the manifest is the single source of truth). The real-shellnet seller path requires
    // it(see `seller_real_backend`); the mock path ignores it.
    if args.market.is_some() && args.nonce.is_some() {
        bail!("--market and --nonce are mutually exclusive -- the nonce comes from the manifest");
    }
    let seller_policy = if !args.mock.mock_chain {
        Some(policy::load_seller_runtime_policy(args.policy.as_deref())?)
    } else {
        None
    };
    if let Some(policy) = seller_policy.as_ref() {
        tracing::debug!(
            policy_after_deal_done = policy.after_deal_done.as_str(),
            policy_buyer_no_show = policy.buyer_no_show.as_str(),
            policy_dispute_against_me = policy.dispute_against_me.as_str(),
            policy_max_open_deals = policy.max_open_deals,
            "seller policy loaded"
        );
        enforce_seller_runtime_policy(policy)?;
    }
    // on the real path, the --market manifest's seller_note must be this seller's --note-addr -- else the
    // offer posts a non-canonical TC the InferenceOrderBook won't rest, and the seller never matches.
    if !args.mock.mock_chain {
        if let (Some(market), Some(note_addr)) =
            (args.market.as_deref(), args.identity.note_addr.as_deref())
        {
            let manifest = load_market(market)?;
            assert_market_seller_note(&manifest.seller_note, note_addr)?;
        }
        shellnet_doctor_preflight(&args.contracts, args.market.as_deref()).await?;
        if let Some(policy) = load_enabled_model_registry_policy(
            RegistryRole::Seller,
            &args.registry,
            &args.contracts,
        )? {
            let name = args
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: set --model <name from config> (needed for model registry validation)"
                    )
                })?;
            let frame_model = dexdo::seller::ModelsConfig::load(&args.models)?
                .get(name)?
                .frame_model
                .clone();
            dexdo_core::validate_canonical_model_id(&frame_model)
                .map_err(|e| anyhow::anyhow!(e))?;
            check_market_model_match(market_frame_model.as_deref(), &frame_model, name)?;
            let expected_order_book = if let Some(market) = args.market.as_deref() {
                load_market(market)?.inference_order_book
            } else {
                let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: --note-addr is required to derive the seller order book"
                    )
                })?;
                expected_order_book_for_note(&args.contracts, note_addr, &frame_model).await?
            };
            let order_book_active =
                order_book_active_from_contracts(&args.contracts, &expected_order_book).await?;
            enforce_model_registry_policy(
                RegistryRole::Seller,
                &policy,
                &args.contracts,
                &frame_model,
                &expected_order_book,
                order_book_active,
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
    }
    let deal_nonce = market_nonce.or(args.nonce);
    // Upstream(model) and chain are selected independently: `--mock-model` -> mock upstream,
    // otherwise a real model from the config; `--mock-chain` -> mock chain, otherwise real shellnet
    // (per-role backend behind the feature).
    let upstream = if args.mock.mock_model {
        dexdo::seller::UpstreamConfig::Mock
    } else {
        let name = args
            .model
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "set --model <name from config> (or --mock-model for a mock upstream)"
                )
            })?;
        let models = dexdo::seller::ModelsConfig::load(&args.models)?;
        let mc = models.get(name)?;
        mc.require_api_key_present()?;
        dexdo::seller::UpstreamConfig::OpenAi(dexdo::seller::OpenAiConfig::from_model(mc))
    };
    let seller_frame_model_for_handle = if args.mock.mock_chain {
        None
    } else {
        let name = args
            .model
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "real shellnet: set --model <name from config> (needed for deal handle)"
                )
            })?;
        Some(
            dexdo::seller::ModelsConfig::load(&args.models)?
                .get(name)?
                .frame_model
                .clone(),
        )
    };
    let (chain, note) = if args.mock.mock_chain {
        let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
        mock_chain_and_note(endpoints_file, &args.identity)?
    } else {
        seller_real_backend(&args, market_frame_model.as_deref(), deal_nonce)?
    };
    // Real-shellnet offer terms are bound to the deployed per-deal TokenContract, not prompt/default values.
    // `deploy-market` creates only the shared model book; `dexdo provision` creates the per-deal TC carrying
    // price/maxTicks. The mock path keeps the prior fixed defaults.
    let (offer_ticks, offer_price) = if args.mock.mock_chain {
        (1024u64, args.price_per_tick)
    } else {
        let (price, ticks) = chain
            .sell_offer_terms(&token_contract)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "seller requires a deployed per-deal TokenContract; `dexdo deploy-market` only deploys \
                     the shared model order book. Run `dexdo provision --frame-model ... --nonce ...` and pass \
                     its --market manifest, or pass --token-contract plus --nonce for an already-provisioned TC."
                )
            })?;
        println!(
            "posting offer: {ticks} ticks (= {} model tokens) at {price} SHELL/tick",
            (ticks as u128).saturating_mul(DobParams::canonical().tick_size as u128)
        );
        (ticks, price)
    };
    let gateway_advertise = args.gateway_advertise_addr();
    let cfg = dexdo::seller::SellerConfig {
        token_contract: token_contract.clone(),
        price_per_tick: offer_price,
        max_ticks: offer_ticks,
        gateway_advertise: gateway_advertise.clone(),
        mock_token_count: args.mock_token_count,
    };
    // the seller daemon publishes offers WITHOUT going through `provision_market`'s note-current gate, so
    // a note orphaned by a contract redeploy(stale code_hash) would hit a raw `TVM_ERROR` from `postSellOffer`.
    // Gate here: fail closed with an actionable "re-mint" message before posting(the mock backend no-ops).
    chain.assert_note_current().await?;
    // Resume path: a matched buyer can fund this per-deal TC while no seller process was live (the deal ends up
    // `funded-but-never-opened`). Because a `(sellerPubkey, nonce)` TC is single-use, re-posting the offer would
    // fail -- but the stream can still be opened. This pre-offer probe MUST be non-blocking: fresh normal
    // sellers must post their ask immediately, while `read_match` remains the later wait-loop after the ask rests.
    let already_matched = match chain.read_openable_match_now(&token_contract).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            return Err(anyhow!(
                "seller: existing-match resume preflight failed for {token_contract}: {e}"
            ));
        }
    };
    if already_matched {
        tracing::info!(
            token_contract = %token_contract,
            "seller: TC already funded by a matched buyer (funded-but-never-opened) -- resuming: skipping offer post, opening stream"
        );
    } else {
        // a deterministic per-deal TC(sellerPubkey + nonce) is single-use. If a prior deal already used this
        // nonce's TC(opened/funded/disputed/residual), the seller's pre-stream steps revert with a raw `TVM_ERROR`
        // (ERR_ALREADY_OPEN 321). Fail closed BEFORE post_offer with an actionable "fresh --nonce / recover+destroy"
        // message(the mock backend no-ops; a fresh active-but-unfunded TC passes).
        chain.assert_token_contract_fresh(&token_contract).await?;
        chain.assert_no_active_sell_order(&token_contract).await?;
        tracing::info!(token_contract = %token_contract, "seller posting offer, awaiting buy + match");
        dexdo::seller::post_offer_with_note(note.as_ref(), chain.as_ref(), &cfg).await?;
        // confirm the ask actually RESTED in the InferenceOrderBook before waiting for a match -- a note-level
        // postSellOffer can submit OK while the IOB rejects the ask(non-canonical TC / note-pairing mismatch). Fail
        // closed actionable with the IOB stats instead of silently waiting out the 300s read_match(the mock no-ops).
        // Start the TCP gateway only after this guard passes; external supervisors must not interpret an open port
        // as sell-ready while this deal's ask is still absent from the book.
        chain.assert_offer_rested(&token_contract).await?;
    }
    let seller =
        dexdo::seller::start_gateway_with_note(args.gateway_listen, upstream, note).await?;
    println!(
        "seller_ready token_contract={} gateway={} gateway_listen={} readiness={}",
        token_contract,
        gateway_advertise,
        args.gateway_listen,
        if already_matched {
            "resumed_funded_tc"
        } else {
            "exact_tc_offer_rested"
        }
    );
    let _ = std::io::stdout().flush();
    // match wait + access-handover provisioning belong to the long-running gateway path, not the
    // one-shot seller post flow. The watcher polls the note/fill source(or mock equivalent) with a durable
    // cursor and waits indefinitely while the offer is open; no 300s seller deadline tears down a resting ask.
    let watch = dexdo::seller::SellerMatchWatchConfig {
        cursor_path: seller_watch_cursor_path(args.deals_dir.as_deref(), &token_contract)?,
        poll_interval: dexdo::seller::DEFAULT_MATCH_POLL_INTERVAL,
    };
    let matched =
        dexdo::seller::watch_and_serve_match(&seller, chain.as_ref(), &cfg, &watch).await?;
    println!(
        "seller_match_opened token_contract={} gateway={} gateway_listen={} cursor={}",
        matched.token_contract,
        gateway_advertise,
        args.gateway_listen,
        watch.cursor_path.display()
    );
    let _ = std::io::stdout().flush();
    if !args.mock.mock_chain {
        let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-addr is required to save the deal handle")
        })?;
        save_runtime_deal_handle(
            RuntimeDealHandleInput {
                role: deals::DealHandleRole::Seller,
                deals_dir: args.deals_dir.as_deref(),
                token_contract: &token_contract,
                note_addr,
                frame_model: seller_frame_model_for_handle.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("real shellnet: missing frame_model for deal handle")
                })?,
                market_path: args.market.as_deref(),
                contracts: &args.contracts,
                endpoint: Some(deals::DealEndpointInfo {
                    kind: "gateway".to_string(),
                    value: gateway_advertise.clone(),
                }),
            },
            true,
        )?;
    }
    if let Some(policy) = seller_policy.as_ref() {
        if apply_seller_dispute_policy(chain.as_ref(), &token_contract, policy, "pre-advance")
            .await?
        {
            return Ok(());
        }
    }
    // on the shipped real-money path, drive the seller's by-fact advance. Both safety
    // prerequisites the lead required are met: `drive_advance` is **delivery-bounded** (finalized
    // ticks <= the gateway's delivered canonical-token count, `seller.state.delivery(tc)`, with a merged
    // regression) and it exits on `deal_closed()`. The buyer session-scoped STOP keeps the deal alive
    // across requests so the probe is accepted and ticks finalize by-fact(`AmicableSplit`, no `BurnBoth`).
    // Real-chain only -- the mock chain has no `getConfig` advance window.
    // Two money-path requirements:
    // 1. The stream-phase cadence is `getConfig().settleWindow`; a getter failure must NOT become a silent
    // wrong cadence(advancing too early -> the contract rejects the tick -> the loop dies). Read it
    // FAIL-LOUD before spawning, with TC context -- no default cadence on the real path.
    // 2. `drive_advance` propagates real advance failures as money-path faults. So the task is
    // SUPERVISED, not fire-and-forget: an `Err` is propagated out of `run_seller` (non-zero exit -- by-fact
    // settlement is dead, the gateway must not keep serving as if healthy). Only clean terminals
    // (`Ok(finalized)` / `deal_closed`) are logged and let the gateway serve until shutdown.
    let advance_task = if !args.mock.mock_chain {
        let delivery = seller.state.delivery(&token_contract);
        let settle = chain.deal_settle_window(&token_contract).await.map_err(|e| {
            anyhow::anyhow!(
                "--token-contract {token_contract}: getConfig().settleWindow is unreadable, refusing to \
                 start by-fact advance on a guessed cadence: {e}"
            )
        })?;
        let windows = dexdo::seller::AdvanceWindows::from_settle_window(settle);
        let advance_chain = chain.clone();
        let advance_note = seller.note.clone();
        let advance_tc = token_contract.clone();
        let tick_budget = cfg.max_ticks as u128;
        let tick_size = dexdo_core::DobParams::canonical().tick_size;
        Some(tokio::spawn(async move {
            dexdo::seller::drive_advance(
                advance_chain.as_ref(),
                &advance_tc,
                advance_note.as_ref(),
                windows,
                tick_budget,
                tick_size,
                delivery.count,
                delivery.done,
            )
            .await
        }))
    } else {
        None
    };
    tracing::info!("stream open; serving until shutdown");
    let mut server_task = seller.server_task;
    match advance_task {
        // Supervise: whichever of {by-fact advance, gateway server} ends first decides the exit.
        Some(advance_task) => {
            tokio::select! {
                advanced = advance_task => match advanced {
                    Ok(Ok(finalized)) => {
                        tracing::info!(
                            token_contract = %token_contract, finalized,
                            "drive_advance: finalized ticks by-fact (<= delivered), deal closed; serving until shutdown"
                        );
                        if let Some(policy) = seller_policy.as_ref() {
                            match apply_seller_terminal_policy(&token_contract, policy, finalized)? {
                                SellerTerminalPolicyOutcome::StopServing => {
                                    server_task.abort();
                                    return Ok(());
                                }
                            }
                        }
                        server_task.await?;
                    }
                    Ok(Err(e)) => {
                        if let Some(policy) = seller_policy.as_ref() {
                            if apply_seller_dispute_policy(
                                chain.as_ref(),
                                &token_contract,
                                policy,
                                "advance-error",
                            )
                            .await?
                            {
                                server_task.abort();
                                return Ok(());
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "--token-contract {token_contract}: by-fact advance failed (money-path fault), \
                             stopping the seller: {e}"
                        ));
                    }
                    Err(join) => {
                        return Err(anyhow::anyhow!(
                            "--token-contract {token_contract}: by-fact advance task panicked: {join}"
                        ));
                    }
                },
                served = &mut server_task => served?,
            }
        }
        None => server_task.await?,
    }
    Ok(())
}

/// One resting ask as the order-book renderer needs it: price per tick, its max ticks, and the full deal
/// `TokenContract` address. Kept minimal so both the buyer's pre-buy view and the read-only `markets --table`
/// view can build it from their own sources(`discover_offers` / `OrderBookSnapshot::resting_asks`).
pub struct BookRow {
    pub price_per_tick: u128,
    pub max_ticks: u128,
    pub token_contract: String,
}

/// Render a per-model inference order book to the terminal as a narrow box table (/ UX:
/// "choose a model = choose the market"). Public + read-only: given the resting asks, it prints the
/// `#/price-per-tick/max-ticks/exec` table plus the full `tokenContract` addresses by `#`. `max_price_per_tick`
/// (when `Some`) marks which asks are executable at that ceiling; `your_order_ticks`(when `Some`) appends the
/// buyer's order summary line. The caller sorts nothing -- this sorts by price ascending(best ask first).
pub fn print_book_table(
    frame_model: &str,
    rows: &[BookRow],
    max_price_per_tick: Option<u128>,
    your_order_ticks: Option<u128>,
) {
    use std::io::IsTerminal;
    // ANSI styling only on a real terminal -- piped/headless output stays plain(clean logs, copyable).
    let color = std::io::stdout().is_terminal();
    let paint = |s: &str, code: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    // One tick = a fixed number of delivered model tokens -- print it
    // so price/tick and the tick counts are interpretable in model tokens, not abstract units.
    let tick_size = DobParams::canonical().tick_size as u128;
    let title = format!("inference order book -- {frame_model}");
    let subtitle = format!("1 tick = {tick_size} model tokens");
    if rows.is_empty() {
        println!("{}  ({subtitle})", paint(&title, "1;36"));
        println!(
            "  {} no resting asks yet -- a buy would rest until a seller matches",
            paint("*", "2")
        );
        return;
    }
    let mut sorted: Vec<&BookRow> = rows.iter().collect();
    sorted.sort_by_key(|o| o.price_per_tick);

    // Columns are dynamic: the `exec` verdict only appears when there is a price ceiling to judge against
    // (the buyer's pre-buy view); the read-only `market` discovery view omits it. The full `tokenContract`
    // address is a column IN the table(un-truncated, copy-paste intact) -- the table is as wide as it needs.
    // 0 = center, 1 = right, 2 = left.
    let has_exec = max_price_per_tick.is_some();
    let mut headers: Vec<&str> = vec!["#", "price/tick", "max ticks"];
    let mut aligns: Vec<u8> = vec![0, 1, 1];
    if has_exec {
        headers.push("exec");
        aligns.push(0);
    }
    headers.push("tokenContract");
    aligns.push(2);
    let rows_str: Vec<Vec<String>> = sorted
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let mut cells = vec![
                (i + 1).to_string(),
                o.price_per_tick.to_string(),
                o.max_ticks.to_string(),
            ];
            if let Some(cap) = max_price_per_tick {
                cells.push(if o.price_per_tick <= cap { "yes" } else { "no" }.to_string());
            }
            cells.push(o.token_contract.clone());
            cells
        })
        .collect();
    let n = headers.len();
    let mut w = vec![0usize; n];
    for (i, head) in headers.iter().enumerate() {
        w[i] = head.chars().count();
    }
    for r in &rows_str {
        for i in 0..n {
            w[i] = w[i].max(r[i].chars().count());
        }
    }
    // Box-drawing border for the given junction chars(left, mid, right).
    let border = |l: &str, m: &str, r: &str| {
        let seg: Vec<String> = w.iter().map(|&c| "-".repeat(c + 2)).collect();
        format!("{l}{}{r}", seg.join(m))
    };
    let fit = |s: &str, width: usize, align: u8| {
        let pad = width.saturating_sub(s.chars().count());
        match align {
            1 => format!("{}{}", " ".repeat(pad), s), // right
            2 => format!("{}{}", s, " ".repeat(pad)), // left
            _ => {
                let left = pad / 2;
                format!("{}{}{}", " ".repeat(left), s, " ".repeat(pad - left)) // center
            }
        }
    };
    let bar = paint("-", "2");
    let render_row = |cells: &[String], style: &dyn Fn(&str, usize) -> String| {
        let body: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| style(&fit(c, w[i], aligns[i]), i))
            .collect();
        format!("{bar} {} {bar}", body.join(&format!(" {bar} ")))
    };

    println!("{}  ({subtitle})", paint(&title, "1;36"));
    println!("{}", paint(&border("-", "-", "-"), "2"));
    let head_strings: Vec<String> = headers.iter().map(|s| s.to_string()).collect();
    println!("{}", render_row(&head_strings, &|s, _| paint(s, "1;36")));
    println!("{}", paint(&border("-", "-", "-"), "2"));
    let exec_col = has_exec.then_some(3usize);
    for r in &rows_str {
        println!(
            "{}",
            render_row(r, &|s, i| {
                if Some(i) == exec_col {
                    if s.trim() == "yes" {
                        paint(s, "1;32")
                    } else {
                        paint(s, "2")
                    }
                } else {
                    s.to_string()
                }
            })
        );
    }
    println!("{}", paint(&border("-", "-", "-"), "2"));
    if let (Some(ticks), Some(cap)) = (your_order_ticks, max_price_per_tick) {
        println!(
            "{} {ticks} ticks (= {} model tokens) at up to {} SHELL/tick -- fills the best ask within the limit",
            paint("your order:", "1"),
            ticks.saturating_mul(tick_size),
            paint(&cap.to_string(), "33"),
        );
    }
}

/// Render the per-model inference order book before a model-only buy: reads the resting asks
/// (`discover_offers`) and delegates to [`print_book_table`], marking asks executable at
/// `--max-price-per-tick` and appending the buyer's order summary.
async fn render_inference_book(
    chain: &dyn ChainBackend,
    frame_model: &str,
    max_price_per_tick: u128,
    ticks: u128,
) -> Result<()> {
    chain
        .assert_model_buy_matches_executable_quote(ticks, max_price_per_tick)
        .await
        .map_err(|e| {
            anyhow::anyhow!("could not read a submit-safe order book for {frame_model}: {e}")
        })?;
    let offers = chain.discover_offers().await.map_err(|e| {
        anyhow::anyhow!("could not read a trustworthy order book for {frame_model}: {e}")
    })?;
    let rows: Vec<BookRow> = offers
        .iter()
        .map(|o| BookRow {
            price_per_tick: o.price_per_tick as u128,
            max_ticks: o.max_ticks as u128,
            token_contract: o.token_contract.to_string(),
        })
        .collect();
    print_book_table(frame_model, &rows, Some(max_price_per_tick), Some(ticks));
    Ok(())
}

/// After the book is shown, ask the operator for a numeric order parameter (how many ticks / the per-tick
/// price ceiling). On a TTY it prompts -- empty input keeps the `[default]`(the CLI flag). Non-interactive
/// (piped / headless / daemon) returns the default silently, so automated runs keep working from flags.
fn prompt_u128(label: &str, default: u128) -> u128 {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return default;
    }
    loop {
        print!("{label} [{default}]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return default;
        }
        let s = line.trim();
        if s.is_empty() {
            return default;
        }
        match s.parse::<u128>() {
            Ok(v) => return v,
            Err(_) => eprintln!("enter an integer (or Enter to keep {default})"),
        }
    }
}

fn buyer_renewal_threshold_tokens() -> u64 {
    const ENV: &str = "DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS";
    std::env::var(ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or_else(|| {
            dexdo::buyer::continuity::ContinuityConfig::default().renewal_threshold_tokens
        })
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn elapsed_since(now_secs: u64, at: Option<u64>) -> u64 {
    at.filter(|v| *v > 0)
        .map(|v| now_secs.saturating_sub(v))
        .unwrap_or(0)
}

async fn validate_reported_match_state(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
) -> Result<MatchedTokenContractStatus, ChainError> {
    let state = chain.deal_state(token_contract).await?.ok_or_else(|| {
        ChainError::Chain(format!(
            "reported match {token_contract} has no readable TokenContract state; refusing to wait for handover"
        ))
    })?;
    check_matched_token_contract_state(
        token_contract,
        state,
        unix_now_secs(),
        MATCH_OPEN_TIMEOUT_SECS,
    )
    .map_err(ChainError::Chain)
}

fn matched_state_summary(
    token_contract: &dexdo_core::TokenContract,
    status: &MatchedTokenContractStatus,
) -> String {
    match status {
        MatchedTokenContractStatus::Opened => {
            format!("matched deal state: token_contract={token_contract} funded=true opened=true")
        }
        MatchedTokenContractStatus::FundedNeverOpened {
            funded_time,
            cleanup_after_unix,
            cleanup_ready,
            remaining_secs,
        } => format!(
            "matched deal state: token_contract={token_contract} funded=true opened=false \
             fundedTime={} cleanup_after={} cleanup_ready={} cleanup_wait_secs={}",
            funded_time
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<missing>".to_string()),
            cleanup_after_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            cleanup_ready,
            remaining_secs
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<unknown>".to_string())
        ),
    }
}

async fn handover_timeout_diagnostic(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    last_error: &dyn std::fmt::Display,
) -> String {
    match validate_reported_match_state(chain, token_contract).await {
        Ok(status @ MatchedTokenContractStatus::FundedNeverOpened { .. }) => format!(
            "buyer: matched TokenContract {token_contract} is funded but the seller did not open/write handover \
             within {DEAL_WAIT_SECS}s. {}. This is a funded-never-opened deal; after MATCH_OPEN_TIMEOUT use \
             `dexdo reclaim --token-contract {token_contract} --note-addr <buyer-note> --note-key <buyer-key>` \
             to streamCleanup. Last handover read error: {last_error}",
            matched_state_summary(token_contract, &status)
        ),
        Ok(status) => format!(
            "buyer: the seller did not open the stream / did not write the handover within {DEAL_WAIT_SECS}s. \
             {}. Last handover read error: {last_error}",
            matched_state_summary(token_contract, &status)
        ),
        Err(state_err) => format!(
            "buyer: the seller did not open the stream / did not write the handover within {DEAL_WAIT_SECS}s, \
             and the post-match TC state check now fails: {state_err}. Last handover read error: {last_error}"
        ),
    }
}

fn is_malformed_handover_error(error: &anyhow::Error) -> bool {
    let msg = error.to_string();
    msg.contains("malformed handover") || msg.contains("handover decrypt failed")
}

async fn apply_malformed_handover_policy(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    token_contract: &dexdo_core::TokenContract,
    buyer_policy: &policy::BuyerRuntimePolicy,
    error: &anyhow::Error,
) -> Result<()> {
    match buyer_policy.malformed_handover {
        policy::MalformedHandoverAction::Reclaim => {
            let settlement = chain.seller_timeout(token_contract).await?;
            bail!(
                "buyer: malformed handover for {token_contract}: {error}\n\
                 policy_action failure_class=malformed_handover action=reclaim token_contract={token_contract} \
                 state=funded/opened result=reclaimed settlement={settlement:?}"
            );
        }
        policy::MalformedHandoverAction::Dispute => {
            let settlement = chain.dispute(token_contract, buyer.note.as_ref()).await?;
            bail!(
                "buyer: malformed handover for {token_contract}: {error}\n\
                 policy_action failure_class=malformed_handover action=dispute token_contract={token_contract} \
                 state=funded/opened/disputed result=dispute_opened settlement={settlement:?}; \
                 warning=dispute_locks_buyer_note_until_resolution"
            );
        }
        policy::MalformedHandoverAction::FailClosed => {
            bail!(
                "buyer: malformed handover for {token_contract}: {error}\n\
                 policy_action failure_class=malformed_handover action=fail_closed token_contract={token_contract} \
                 state=funded/opened result=no_recovery_submitted"
            );
        }
    }
}

async fn policy_cleanup_unopened_after_match_timeout(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    policy_action: policy::NoHandoverAfterMatchAction,
) -> Result<PolicyCleanupOutcome> {
    let status = validate_reported_match_state(chain, token_contract).await?;
    let MatchedTokenContractStatus::FundedNeverOpened {
        cleanup_ready,
        remaining_secs,
        ..
    } = status
    else {
        bail!(
            "policy_action failure_class=no_handover_after_match action={} token_contract={} \
             state={} result=not_cleanup_unopened_state",
            policy_action.as_str(),
            token_contract,
            matched_state_summary(token_contract, &status)
        );
    };
    if !cleanup_ready {
        let wait = remaining_secs
            .unwrap_or(MATCH_OPEN_TIMEOUT_SECS)
            .saturating_add(1);
        println!(
            "policy_action failure_class=no_handover_after_match action={} token_contract={} \
             state=funded/opened result=waiting_cleanup_ready wait_secs={wait}",
            policy_action.as_str(),
            token_contract
        );
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        let status = validate_reported_match_state(chain, token_contract).await?;
        match status {
            MatchedTokenContractStatus::Opened => {
                println!(
                    "policy_action failure_class=no_handover_after_match action={} token_contract={} \
                     state=funded/opened result=handover_opened_after_wait",
                    policy_action.as_str(),
                    token_contract
                );
                return Ok(PolicyCleanupOutcome::HandoverOpened);
            }
            MatchedTokenContractStatus::FundedNeverOpened {
                cleanup_ready: true,
                ..
            } => {}
            status => {
                bail!(
                    "policy_action failure_class=no_handover_after_match action={} token_contract={} \
                     state={} result=not_cleanup_unopened_state_after_wait",
                    policy_action.as_str(),
                    token_contract,
                    matched_state_summary(token_contract, &status)
                );
            }
        }
    }
    let settlement = chain.cleanup_unopened(token_contract).await?;
    println!(
        "policy_action failure_class=no_handover_after_match action={} token_contract={} \
         state=funded/opened result=cleanup_unopened_submitted settlement={settlement:?}",
        policy_action.as_str(),
        token_contract
    );
    Ok(PolicyCleanupOutcome::Cleaned(settlement))
}

enum PolicyCleanupOutcome {
    Cleaned(Settlement),
    HandoverOpened,
}

enum NoHandoverPolicyOutcome {
    RetryCurrent,
    RetryNext(dexdo_core::TokenContract),
}

async fn apply_no_handover_after_match_policy(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    token_contract: &dexdo_core::TokenContract,
    buyer_policy: &policy::BuyerRuntimePolicy,
    next_buy: Option<(u128, u128, u128)>,
    attempt: u64,
    diagnostic: &str,
) -> Result<NoHandoverPolicyOutcome> {
    match buyer_policy.no_handover_after_match {
        policy::NoHandoverAfterMatchAction::FailClosed => {
            bail!(
                "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=fail_closed \
                 token_contract={token_contract} state=funded/opened result=no_recovery_submitted"
            );
        }
        policy::NoHandoverAfterMatchAction::WaitThenReclaim => {
            let outcome = policy_cleanup_unopened_after_match_timeout(
                chain,
                token_contract,
                buyer_policy.no_handover_after_match,
            )
            .await?;
            let PolicyCleanupOutcome::Cleaned(settlement) = outcome else {
                return Ok(NoHandoverPolicyOutcome::RetryCurrent);
            };
            bail!(
                "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=wait_then_reclaim \
                 token_contract={token_contract} state=funded/opened result=money_reclaimed settlement={settlement:?}"
            );
        }
        policy::NoHandoverAfterMatchAction::NextSeller => {
            if attempt >= buyer_policy.max_sellers_to_try {
                bail!(
                    "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=next_seller \
                     token_contract={token_contract} state=funded/opened result=max_sellers_to_try_reached \
                     max_sellers_to_try={}",
                    buyer_policy.max_sellers_to_try
                );
            }
            let Some((ticks, max_price, escrow)) = next_buy else {
                bail!(
                    "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=next_seller \
                     token_contract={token_contract} state=funded/opened result=no_model_only_routing_context"
                );
            };
            let outcome = policy_cleanup_unopened_after_match_timeout(
                chain,
                token_contract,
                buyer_policy.no_handover_after_match,
            )
            .await?;
            if matches!(outcome, PolicyCleanupOutcome::HandoverOpened) {
                return Ok(NoHandoverPolicyOutcome::RetryCurrent);
            }
            let next_attempt = attempt.saturating_add(1);
            let projected_spend = escrow.saturating_mul(next_attempt as u128);
            if projected_spend > buyer_policy.total_spend_cap_shells as u128 {
                bail!(
                    "{diagnostic}\npolicy_action failure_class=no_handover_after_match action=next_seller \
                     token_contract={token_contract} state=funded/opened result=total_spend_cap_reached \
                     projected_spend_shells={projected_spend} cap_shells={}",
                    buyer_policy.total_spend_cap_shells
                );
            }
            println!(
                "policy_action failure_class=no_handover_after_match action=next_seller \
                 token_contract={token_contract} state=funded/opened result=placing_next_seller \
                 attempt={next_attempt}"
            );
            let next =
                submit_buyer_monitor_next_deal(chain, buyer, ticks, max_price, escrow).await?;
            println!(
                "policy_action failure_class=no_handover_after_match action=next_seller \
                 token_contract={token_contract} state=funded/opened result=next_seller_matched \
                 next_token_contract={next}"
            );
            Ok(NoHandoverPolicyOutcome::RetryNext(next))
        }
    }
}

fn buyer_monitor_current_facts(
    token_contract: dexdo_core::TokenContract,
    remaining_tokens: u64,
    session_settled: bool,
    chain_state: Option<DealChainState>,
    now_secs: u64,
) -> dexdo::buyer::continuity::DealFacts {
    use dexdo::buyer::continuity::DealFacts;

    if session_settled {
        return DealFacts::closed(token_contract);
    }
    let Some(state) = chain_state else {
        return DealFacts::handover_ready(token_contract, remaining_tokens);
    };
    if state.disputed {
        return DealFacts::closed(token_contract);
    }
    if state.opened {
        let idle_secs = if state.last_advance == 0 {
            0
        } else {
            now_secs.saturating_sub(state.last_advance)
        };
        return DealFacts::opened_idle(token_contract, idle_secs);
    }
    if state.funded && !state.probe_accepted {
        return DealFacts::funded_never_opened(
            token_contract,
            elapsed_since(now_secs, state.funded_time),
        );
    }
    DealFacts::closed(token_contract)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuyerMonitorRecoveryKind {
    CleanupUnopened,
    ReclaimOpened,
}

async fn execute_buyer_monitor_recovery(
    chain: &dyn ChainBackend,
    action: dexdo::buyer::continuity::BuyerAction,
) -> Option<(
    BuyerMonitorRecoveryKind,
    dexdo_core::TokenContract,
    Result<Settlement, ChainError>,
)> {
    use dexdo::buyer::continuity::BuyerAction;

    match action {
        BuyerAction::CleanupUnopened { token_contract } => {
            let result = chain.cleanup_unopened(&token_contract).await;
            Some((
                BuyerMonitorRecoveryKind::CleanupUnopened,
                token_contract,
                result,
            ))
        }
        BuyerAction::ReclaimOpened { token_contract } => {
            let result = chain.seller_timeout(&token_contract).await;
            Some((
                BuyerMonitorRecoveryKind::ReclaimOpened,
                token_contract,
                result,
            ))
        }
        _ => None,
    }
}

async fn submit_buyer_monitor_next_deal(
    chain: &dyn ChainBackend,
    buyer: &dexdo::buyer::Buyer,
    ticks: u128,
    max_price: u128,
    escrow: u128,
) -> Result<dexdo_core::TokenContract, ChainError> {
    let since_unix = unix_now_secs() as i64;
    chain
        .place_buy_by_model(buyer.note.as_ref(), ticks, max_price, escrow)
        .await?;
    let token_contract = chain
        .wait_matched_token_contract(since_unix, std::time::Duration::from_secs(DEAL_WAIT_SECS))
        .await?;
    validate_reported_match_state(chain, &token_contract).await?;
    Ok(token_contract)
}

fn spawn_buyer_service_renewal(
    chain: Arc<dyn ChainBackend>,
    buyer: Arc<dexdo::buyer::Buyer>,
    deals: Arc<dexdo::buyer::api::RouteManager>,
    ticks: u128,
    max_price: u128,
    escrow: u128,
    continuity_mode: dexdo::buyer::continuity::ContinuityMode,
    content_check: dexdo::buyer::api::ContentCheck,
    api_failure_policy: dexdo::buyer::api::BuyerApiFailurePolicy,
) {
    struct PendingRenewal {
        current: dexdo_core::TokenContract,
        next: Option<dexdo_core::TokenContract>,
        matched_at: Option<std::time::Instant>,
    }
    struct PrepareRetry {
        current: dexdo_core::TokenContract,
        retry_at: std::time::Instant,
    }

    const RENEWAL_FAILURE_BACKOFF_SECS: u64 = 30;
    const CONSUMER_DEMAND_RECENT_SECS: u64 = 30;

    tokio::spawn(async move {
        use dexdo::buyer::continuity::{
            BuyerAction, BuyerContinuity, ConsumerDemand, ContinuityConfig, DealFacts,
        };

        let mut planner = BuyerContinuity::default();
        let cfg = ContinuityConfig {
            renewal_threshold_tokens: buyer_renewal_threshold_tokens(),
            ..ContinuityConfig::default()
        };
        let mut pending: Option<PendingRenewal> = None;
        let mut prepare_retry: Option<PrepareRetry> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let active = deals.current().await;
            let current_tc = active.route.token_contract.clone();
            if prepare_retry
                .as_ref()
                .is_some_and(|retry| retry.current != current_tc)
            {
                prepare_retry = None;
            }
            let chain_state = match chain.deal_state(&current_tc).await {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(
                        current = %current_tc,
                        error = %e,
                        "buyer continuity: deal_state read failed; falling back to local session facts"
                    );
                    None
                }
            };
            let now_secs = unix_now_secs();
            let current_facts = buyer_monitor_current_facts(
                current_tc.clone(),
                active.remaining_tokens(),
                active.session.is_settled(),
                chain_state,
                now_secs,
            );
            let consumer_demand =
                if active.has_active_or_recent_request(now_secs, CONSUMER_DEMAND_RECENT_SECS) {
                    ConsumerDemand::ActiveOrRecent
                } else {
                    ConsumerDemand::Idle
                };

            let mut ready_next = None;
            let mut waiting_for_pending_handover = false;
            if let Some(p) = pending.as_ref().filter(|p| p.current == current_tc) {
                if let Some(next) = p.next.as_ref() {
                    if buyer.resolve_endpoint(chain.as_ref(), next).await.is_ok() {
                        ready_next = Some(DealFacts::handover_ready(
                            next.clone(),
                            consumer_api_token_budget(ticks),
                        ));
                    } else if let Some(matched_at) = p.matched_at {
                        waiting_for_pending_handover = true;
                        let age = matched_at.elapsed().as_secs();
                        let recovery = planner.tick(
                            Some(DealFacts::funded_never_opened(next.clone(), age)),
                            None,
                            cfg,
                        );
                        if let Some((_kind, token_contract, result)) =
                            execute_buyer_monitor_recovery(chain.as_ref(), recovery).await
                        {
                            match result {
                                Ok(settlement) => {
                                    tracing::warn!(
                                        current = %current_tc,
                                        next = %token_contract,
                                        settlement = ?settlement,
                                        "buyer continuity: cleaned up renewal deal that never opened"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        current = %current_tc,
                                        next = %token_contract,
                                        error = %e,
                                        "buyer continuity: cleanup_unopened failed"
                                    );
                                }
                            }
                            planner.clear_pending_next(&current_tc);
                            pending = None;
                            continue;
                        }
                    } else {
                        waiting_for_pending_handover = true;
                    }
                }
            } else if pending.is_some() {
                pending = None;
            }
            if waiting_for_pending_handover {
                continue;
            }

            let action = planner.tick_with_mode(
                Some(current_facts),
                ready_next,
                cfg,
                continuity_mode,
                consumer_demand,
            );
            match action {
                BuyerAction::ServeCurrent { .. }
                | BuyerAction::Noop { .. }
                | BuyerAction::IgnoreStale { .. } => {}
                BuyerAction::FailClosed {
                    token_contract,
                    reason,
                } => {
                    tracing::error!(
                        token_contract = %token_contract,
                        reason,
                        "buyer continuity: fail-closed planner action"
                    );
                }
                action @ (BuyerAction::CleanupUnopened { .. }
                | BuyerAction::ReclaimOpened { .. }) => {
                    if let Some((kind, token_contract, result)) =
                        execute_buyer_monitor_recovery(chain.as_ref(), action).await
                    {
                        match (kind, result) {
                            (BuyerMonitorRecoveryKind::CleanupUnopened, Ok(settlement)) => {
                                active.session.mark_recovered("continuity-cleanup");
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    settlement = ?settlement,
                                    "buyer continuity: cleaned current funded-never-opened deal"
                                );
                            }
                            (BuyerMonitorRecoveryKind::CleanupUnopened, Err(e)) => {
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    error = %e,
                                    "buyer continuity: cleanup current funded-never-opened deal failed"
                                );
                            }
                            (BuyerMonitorRecoveryKind::ReclaimOpened, Ok(settlement)) => {
                                active.session.mark_recovered("continuity-reclaim");
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    settlement = ?settlement,
                                    "buyer continuity: reclaimed current opened idle deal"
                                );
                            }
                            (BuyerMonitorRecoveryKind::ReclaimOpened, Err(e)) => {
                                tracing::warn!(
                                    token_contract = %token_contract,
                                    error = %e,
                                    "buyer continuity: reclaim current opened idle deal failed"
                                );
                            }
                        }
                        pending = None;
                    }
                }
                BuyerAction::PlaceNextDeal { reason } => {
                    tracing::info!(reason, "buyer continuity: planner requested a fresh deal");
                    let current = current_tc.clone();
                    if let Some(retry) = prepare_retry.as_ref().filter(|retry| {
                        retry.current == current && std::time::Instant::now() < retry.retry_at
                    }) {
                        planner.clear_pending_next(&current);
                        tracing::debug!(
                            current = %current,
                            retry_after_secs = retry
                                .retry_at
                                .saturating_duration_since(std::time::Instant::now())
                                .as_secs(),
                            "buyer continuity: fresh deal prepare is in retry backoff"
                        );
                        continue;
                    }
                    match submit_buyer_monitor_next_deal(
                        chain.as_ref(),
                        buyer.as_ref(),
                        ticks,
                        max_price,
                        escrow,
                    )
                    .await
                    {
                        Ok(next) => {
                            prepare_retry = None;
                            planner.note_pending_next(current.clone(), next.clone());
                            pending = Some(PendingRenewal {
                                current,
                                next: Some(next.clone()),
                                matched_at: Some(std::time::Instant::now()),
                            });
                            tracing::info!(
                                next = %next,
                                "buyer continuity: fresh buy matched; waiting for handover"
                            );
                        }
                        Err(e) => {
                            planner.clear_pending_next(&current);
                            pending = None;
                            prepare_retry = Some(PrepareRetry {
                                current: current.clone(),
                                retry_at: std::time::Instant::now()
                                    + std::time::Duration::from_secs(RENEWAL_FAILURE_BACKOFF_SECS),
                            });
                            tracing::warn!(
                                current = %current,
                                retry_after_secs = RENEWAL_FAILURE_BACKOFF_SECS,
                                error = %e,
                                "buyer continuity: fresh buy submit/match failed"
                            );
                        }
                    }
                }
                BuyerAction::PrepareNextDeal { current } => {
                    if let Some(retry) = prepare_retry.as_ref().filter(|retry| {
                        retry.current == current && std::time::Instant::now() < retry.retry_at
                    }) {
                        planner.clear_pending_next(&current);
                        tracing::debug!(
                            current = %current,
                            retry_after_secs = retry
                                .retry_at
                                .saturating_duration_since(std::time::Instant::now())
                                .as_secs(),
                            "buyer continuity: renewal prepare is in retry backoff"
                        );
                        continue;
                    }
                    match submit_buyer_monitor_next_deal(
                        chain.as_ref(),
                        buyer.as_ref(),
                        ticks,
                        max_price,
                        escrow,
                    )
                    .await
                    {
                        Ok(next) => {
                            prepare_retry = None;
                            planner.note_pending_next(current.clone(), next.clone());
                            pending = Some(PendingRenewal {
                                current,
                                next: Some(next.clone()),
                                matched_at: Some(std::time::Instant::now()),
                            });
                            tracing::info!(
                                next = %next,
                                "buyer continuity: renewal buy matched; waiting for handover"
                            );
                        }
                        Err(e) => {
                            planner.clear_pending_next(&current);
                            pending = None;
                            prepare_retry = Some(PrepareRetry {
                                current: current.clone(),
                                retry_at: std::time::Instant::now()
                                    + std::time::Duration::from_secs(RENEWAL_FAILURE_BACKOFF_SECS),
                            });
                            tracing::warn!(
                                current = %current,
                                retry_after_secs = RENEWAL_FAILURE_BACKOFF_SECS,
                                error = %e,
                                "buyer continuity: renewal submit/match failed"
                            );
                        }
                    }
                }
                BuyerAction::SwitchToNextDeal { previous, next } => {
                    let handover = match buyer.resolve_endpoint(chain.as_ref(), &next).await {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!(
                                previous = %previous,
                                next = %next,
                                error = %e,
                                "buyer continuity: planner saw next ready but handover reread failed"
                            );
                            continue;
                        }
                    };
                    let session =
                        Arc::new(dexdo::buyer::api::SessionSettle::new_with_failure_policy(
                            chain.clone(),
                            next.clone(),
                            buyer.note.clone(),
                            api_failure_policy,
                        ));
                    let next_deal = dexdo::buyer::api::ApiDeal::new(
                        dexdo::buyer::api::Route {
                            handover,
                            token_contract: next.clone(),
                            max_tokens: consumer_api_token_budget(ticks),
                        },
                        session,
                        Arc::new(dexdo::buyer::api::ContentGate::new(content_check.clone())),
                    );
                    deals.replace_active(next_deal, "continuity-renewal").await;
                    pending = None;
                    prepare_retry = None;
                    tracing::info!(
                        previous = %previous,
                        next = %next,
                        "buyer continuity: switched local API to renewed handover"
                    );
                }
            }
        }
    });
}

pub(crate) async fn run_buyer(args: BuyerArgs) -> Result<()> {
    let json_mode = args.json;
    let mut machine_events = json_mode.then(machine::BuyerEventWriter::new);
    let mut machine_context = BuyerMachineErrorContext::default();
    let result = run_buyer_inner(args, &mut machine_events, &mut machine_context).await;
    if let Err(err) = result {
        if machine::is_printed_error(&err) {
            return Err(err);
        }
        if let Some(events) = machine_events.as_mut() {
            let code = machine::classify_error(machine::OP_BUYER_START, &err);
            events.error(machine::OP_BUYER_START, code, machine_context.fields())?;
            return Err(machine::printed_error());
        }
        return Err(err);
    }
    Ok(())
}

#[derive(Default)]
struct BuyerMachineErrorContext {
    network: Option<String>,
    frame_model: Option<String>,
    order_book: Option<String>,
    token_contract: Option<String>,
    deal_handle: Option<String>,
}

impl BuyerMachineErrorContext {
    fn set_token_contract(&mut self, token_contract: &str) {
        self.token_contract = Some(token_contract.to_string());
        self.deal_handle = Some(deals::make_handle_id(token_contract));
    }

    fn fields(&self) -> Value {
        let mut obj = Map::new();
        if let Some(v) = &self.network {
            obj.insert("network".to_string(), json!(v));
        }
        if let Some(v) = &self.frame_model {
            obj.insert("frame_model".to_string(), json!(v));
        }
        if let Some(v) = &self.order_book {
            obj.insert("order_book".to_string(), json!(v));
        }
        if let Some(v) = &self.token_contract {
            obj.insert("token_contract".to_string(), json!(v));
        }
        if let Some(v) = &self.deal_handle {
            obj.insert("deal_handle".to_string(), json!(v));
        }
        Value::Object(obj)
    }
}

#[cfg(debug_assertions)]
fn buyer_machine_error_fixture_from_env() -> Option<anyhow::Error> {
    let code = std::env::var("DEXDO_BUYER_JSON_ERROR_FIXTURE").ok()?;
    let message = match code.as_str() {
        "NO_LIQUIDITY" => "no liquidity fixture",
        "INSUFFICIENT_BALANCE" => "insufficient balance fixture",
        "HANDOVER_TIMEOUT" => "handover within deadline fixture",
        "CHAIN_TRANSPORT" => "shellnet rpc transport fixture",
        "SETTLEMENT_FAILED" => "settlement streamStop fixture",
        "NOT_RECOVERABLE_YET" => "not recoverable yet fixture",
        "DISPUTED_DEAL" => "deal is disputed fixture",
        _ => return Some(anyhow::anyhow!("invalid fixture code {code}")),
    };
    Some(anyhow::anyhow!(message))
}

async fn run_buyer_inner(
    args: BuyerArgs,
    machine_events: &mut Option<machine::BuyerEventWriter>,
    machine_context: &mut BuyerMachineErrorContext,
) -> Result<()> {
    // Issue: token_contract + frame_model come from `--market`(a provision manifest) or the flags.
    // The buyer ignores the deal nonce: it places a buy, it does not post the offer.
    // Model-only buy: with neither
    // `--token-contract` nor `--market`, the buyer derives the per-model book from `--frame-model`, shows the
    // resting asks, places a model-wide buy, and learns the matched deal `TokenContract` from ITS OWN note's
    // `InferenceFilledConfirmed` event -- no seller hand-off. With `--token-contract`/`--market` the explicit
    // deal address is used as before(back-compat).
    let model_only = args.market.is_none() && args.token_contract.is_none();
    let (explicit_tc, frame_model) = if model_only {
        let fm = args.frame_model.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "provide --frame-model (model-only buy: the orderbook is derived from the model name), \
                 or --token-contract / --market for an explicit deal"
            )
        })?;
        (None, fm)
    } else {
        let (tc, fm, _nonce) = resolve_market_fields(
            args.market.as_deref(),
            args.token_contract.as_deref(),
            args.frame_model.as_deref(),
        )?;
        let fm =
            fm.ok_or_else(|| anyhow::anyhow!("provide --frame-model or --market <manifest>"))?;
        (Some(tc), fm)
    };
    // Model-only discovery derives the order-book address from `sha256(frame_model)`, so the id MUST be the
    // canonical `producer--model--version`(else it looks at the wrong book). Only enforce here: on the explicit
    // `--token-contract`/`--market` path the deal address is given directly (frame_model is only B2/B7 there,
    // where `family_of` matches by substring regardless of form), and the mock demo uses `dexdo-mock`.
    if model_only && !args.mock.mock_chain {
        dexdo_core::validate_canonical_model_id(&frame_model).map_err(|e| anyhow::anyhow!(e))?;
    }
    machine_context.network = Some(
        if args.mock.mock_chain {
            "mock"
        } else {
            "shellnet"
        }
        .to_string(),
    );
    machine_context.frame_model = Some(frame_model.clone());
    if let Some(tc) = explicit_tc.as_deref() {
        machine_context.order_book = Some("explicit_token_contract".to_string());
        machine_context.set_token_contract(tc);
    } else if !args.resume {
        machine_context.order_book = Some("model_order_book".to_string());
    }
    // Model-only `--resume` is supported (directive: the buyer recovers its deal from ITS OWN note's fill
    // event, never a hand-pasted `--token-contract`): it re-scans `InferenceFilledConfirmed` on this note over
    // a lookback window and connects to the freshly matched deal without placing a new buy. Handled below.
    // fail closed BEFORE the on-chain buy if this is a one-shot real-upstream attempt(promptless) --
    // an actionable client-side error, not a deep gateway `InvalidArgument` after place_buy + handover.
    oneshot_real_upstream_guard(args.local_listen.is_some(), args.mock.mock_model)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if model_only && args.mock.mock_chain {
        bail!(
            "model-only buy (no --token-contract/--market) discovers the book on real shellnet; on --mock-chain \
             pass --token-contract 0:<deal> (the mock has no on-chain orderbook to discover)"
        );
    }
    if let Some(events) = machine_events.as_mut() {
        events.event(
            "starting",
            machine::OP_BUYER_START,
            json!({
                "network": if args.mock.mock_chain { "mock" } else { "shellnet" },
                "frame_model": frame_model.clone(),
                "mode": if args.resume { "resume" } else { "buy" },
                "requested_bind_addr": args.local_listen.map(|a| a.to_string()),
                "anthropic_compat": args.anthropic_compat,
                "continuity_mode": args.continuity_mode.as_str()
            }),
        )?;
    }
    #[cfg(debug_assertions)]
    if let Some(err) = buyer_machine_error_fixture_from_env() {
        return Err(err);
    }
    let buyer_policy = if !args.mock.mock_chain {
        Some(policy::load_buyer_runtime_policy(args.policy.as_deref())?)
    } else {
        None
    };
    let api_failure_policy = buyer_policy
        .as_ref()
        .map(policy::BuyerRuntimePolicy::as_api_failure_policy)
        .unwrap_or_default();
    if let Some(policy) = buyer_policy.as_ref() {
        tracing::debug!(
            policy_no_handover_after_match = policy.no_handover_after_match.as_str(),
            policy_malformed_handover = policy.malformed_handover.as_str(),
            policy_dead_gateway = policy.dead_gateway.as_str(),
            policy_empty_stream = policy.empty_stream.as_str(),
            policy_seller_stalls_mid_stream = policy.seller_stalls_mid_stream.as_str(),
            policy_bad_output_scam = policy.bad_output_scam.as_str(),
            policy_max_sellers_to_try = policy.max_sellers_to_try,
            policy_total_spend_cap_shells = policy.total_spend_cap_shells,
            "buyer policy loaded"
        );
    }
    if !args.mock.mock_chain {
        shellnet_doctor_preflight(&args.contracts, args.market.as_deref()).await?;
        if let Some(policy) = load_enabled_model_registry_policy(
            RegistryRole::Buyer,
            &args.registry,
            &args.contracts,
        )? {
            reject_buyer_raw_token_contract_without_registry_book_proof(
                args.market.as_deref(),
                args.token_contract.as_deref(),
                &frame_model,
            )?;
            let expected_order_book = if let Some(market) = args.market.as_deref() {
                load_market(market)?.inference_order_book
            } else {
                let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: --note-addr is required to derive the buyer order book"
                    )
                })?;
                expected_order_book_for_note(&args.contracts, note_addr, &frame_model).await?
            };
            let order_book_active =
                order_book_active_from_contracts(&args.contracts, &expected_order_book).await?;
            enforce_model_registry_policy(
                RegistryRole::Buyer,
                &policy,
                &args.contracts,
                &frame_model,
                &expected_order_book,
                order_book_active,
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
    }
    // The chain is selected by a flag: `--mock-chain` -> mock(as in D1, also requires `--mock-model`), otherwise
    // real shellnet(per-role buyer backend behind the `shellnet` feature; without the feature -> explicit failure).
    let (chain, note) = if args.mock.mock_chain {
        args.mock.require_mock_model()?;
        let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
        mock_chain_and_note(endpoints_file, &args.identity)?
    } else {
        buyer_real_backend(&args, &frame_model)?
    };
    let buyer = dexdo::buyer::Buyer::from_note(note);
    // Resolve the deal `TokenContract`: explicit(flag/manifest) or model-only (book -> choose -> buy -> fill
    // event). `buy_ticks` is the chosen volume(the consumer-API token budget tracks it).
    let mut service_renewal: Option<(u128, u128, u128)> = None;
    let (mut token_contract, buy_ticks) = match explicit_tc {
        Some(tc) => {
            if args.resume {
                // Connect to an ALREADY-matched deal -- escrow is already committed; a fresh place_buy would
                // double-pay. Skip straight to reading the on-chain handover + serving.
                if let Some(events) = machine_events.as_mut() {
                    events.event(
                        "resume_selected",
                        machine::OP_BUYER_START,
                        json!({
                            "token_contract": tc.clone(),
                            "role": "buyer",
                            "source": "token_contract",
                            "deal_handle": deals::make_handle_id(&tc),
                            "frame_model": frame_model.clone()
                        }),
                    )?;
                } else {
                    println!("resuming existing deal {tc} -- connecting without a new buy");
                }
            } else {
                let mut selected = None;
                if let Some(events) = machine_events.as_mut() {
                    let selection = buyer_quote_selection(
                        chain.as_ref(),
                        Some(&tc),
                        args.ticks,
                        args.max_price_per_tick,
                        args.escrow,
                    )
                    .await?;
                    if fail_buyer_quote_selection(
                        events,
                        &frame_model,
                        &selection,
                        args.ticks,
                        args.max_price_per_tick,
                        machine_context.fields(),
                    )?
                    .is_some()
                    {
                        return Err(machine::printed_error());
                    }
                    events.event(
                        "quote_selected",
                        machine::OP_BUYER_START,
                        quote_selected_fields(
                            &frame_model,
                            &selection,
                            args.ticks,
                            args.max_price_per_tick,
                        ),
                    )?;
                    selected = Some(selection);
                }
                let submitted_escrow = selected.as_ref().map(|s| s.escrow).unwrap_or_else(|| {
                    args.escrow.unwrap_or_else(|| {
                        required_escrow_for_buy(args.ticks, args.max_price_per_tick)
                    })
                });
                buyer.place_buy(chain.as_ref(), &tc).await?;
                if let Some(events) = machine_events.as_mut() {
                    events.event(
                        "buy_submitted",
                        machine::OP_BUYER_START,
                        json!({
                            "frame_model": frame_model.clone(),
                            "order_book": "explicit_token_contract",
                            "ticks": machine::amount(args.ticks),
                            "max_price_per_tick": machine::amount(args.max_price_per_tick),
                            "escrow": machine::amount(submitted_escrow)
                        }),
                    )?;
                    events.event(
                        "matched",
                        machine::OP_BUYER_START,
                        json!({
                            "frame_model": frame_model.clone(),
                            "order_book": "explicit_token_contract",
                            "token_contract": tc.clone()
                        }),
                    )?;
                }
            }
            (tc, args.ticks)
        }
        None if args.resume => {
            // Model-only RESUME: recover the already-matched deal from THIS note's own fill event -- no new buy
            // (escrow is already committed). The book is derived from `--frame-model`; we scan the note's
            // `InferenceFilledConfirmed` ext-out over a lookback window and take the most recent buy match.
            let since_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
                - RESUME_LOOKBACK_SECS;
            if machine_events.is_none() {
                println!(
                    "resume (model-only): scanning this note's own fill events (last {RESUME_LOOKBACK_SECS}s) \
                     for a matched deal on {frame_model} -- no new buy"
                );
            }
            let tc = chain
                .wait_matched_token_contract(
                    since_unix,
                    std::time::Duration::from_secs(DEAL_WAIT_SECS),
                )
                .await?;
            chain.assert_model_only_resume_target(&tc).await?;
            machine_context.order_book = Some("model_order_book".to_string());
            machine_context.set_token_contract(&tc);
            if let Some(events) = machine_events.as_mut() {
                events.event(
                    "resume_selected",
                    machine::OP_BUYER_START,
                    json!({
                        "token_contract": tc.clone(),
                        "role": "buyer",
                        "source": "note_fill_event",
                        "deal_handle": deals::make_handle_id(&tc),
                        "frame_model": frame_model.clone()
                    }),
                )?;
            } else {
                println!("recovered matched deal TokenContract from note event: {tc}");
            }
            (tc, args.ticks)
        }
        None => {
            // Show the book, THEN let the buyer choose how many ticks and the per-tick price ceiling
            // (the flags `--ticks`/`--max-price-per-tick` are the defaults / the non-interactive value).
            let (ticks, max_price) = if machine_events.is_none() {
                render_inference_book(
                    chain.as_ref(),
                    &frame_model,
                    args.max_price_per_tick,
                    args.ticks,
                )
                .await?;
                (
                    prompt_u128("How many ticks to buy", args.ticks),
                    prompt_u128(
                        "Maximum price per tick (SHELL/tick)",
                        args.max_price_per_tick,
                    ),
                )
            } else {
                (args.ticks, args.max_price_per_tick)
            };
            // Escrow: an explicit `--escrow` wins(checked == required downstream); otherwise the exact
            // required for the CHOSEN order.
            let escrow = args
                .escrow
                .unwrap_or_else(|| dexdo_core::required_escrow_for_buy(ticks, max_price));
            service_renewal = Some((ticks, max_price, escrow));
            if machine_events.is_none() {
                println!("placing buy: {ticks} ticks at <= {max_price}/tick (escrow {escrow})");
            }
            let mut selected = None;
            if let Some(events) = machine_events.as_mut() {
                let selection =
                    buyer_quote_selection(chain.as_ref(), None, ticks, max_price, Some(escrow))
                        .await?;
                if fail_buyer_quote_selection(
                    events,
                    &frame_model,
                    &selection,
                    ticks,
                    max_price,
                    machine_context.fields(),
                )?
                .is_some()
                {
                    return Err(machine::printed_error());
                }
                events.event(
                    "quote_selected",
                    machine::OP_BUYER_START,
                    quote_selected_fields(&frame_model, &selection, ticks, max_price),
                )?;
                selected = Some(selection);
            }
            let since_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            chain
                .place_buy_by_model(buyer.note.as_ref(), ticks, max_price, escrow)
                .await?;
            if let Some(events) = machine_events.as_mut() {
                events.event(
                    "buy_submitted",
                    machine::OP_BUYER_START,
                    json!({
                        "frame_model": frame_model.clone(),
                        "order_book": selected
                            .as_ref()
                            .map(|s| s.order_book)
                            .unwrap_or("model_order_book"),
                        "ticks": machine::amount(ticks),
                        "max_price_per_tick": machine::amount(max_price),
                        "escrow": machine::amount(escrow)
                    }),
                )?;
            }
            tracing::info!("model-only buy placed; awaiting match on the note's fill event");
            let tc = chain
                .wait_matched_token_contract(
                    since_unix,
                    std::time::Duration::from_secs(DEAL_WAIT_SECS),
                )
                .await?;
            machine_context.set_token_contract(&tc);
            if let Some(events) = machine_events.as_mut() {
                events.event(
                    "matched",
                    machine::OP_BUYER_START,
                    json!({
                        "frame_model": frame_model.clone(),
                        "order_book": "model_order_book",
                        "token_contract": tc.clone()
                    }),
                )?;
            } else {
                println!("matched deal TokenContract: {tc}");
            }
            let status = validate_reported_match_state(chain.as_ref(), &tc).await?;
            if machine_events.is_none() {
                println!("{}", matched_state_summary(&tc, &status));
            }
            (tc, ticks)
        }
    };
    tracing::info!("buy placed; awaiting handover");
    // Wait for the seller to open the stream and write the handover. Issue: fail-closed on the deadline instead of
    // waiting forever; do not swallow the `resolve_endpoint` error(diagnostics for the operator).
    let mut handover_attempt = 1u64;
    let handover = 'handover: loop {
        let hv_deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(DEAL_WAIT_SECS);
        let hv_deadline_unix = machine::now_unix()?.saturating_add(DEAL_WAIT_SECS);
        if let Some(events) = machine_events.as_mut() {
            events.event(
                "handover_waiting",
                machine::OP_BUYER_START,
                json!({
                    "token_contract": token_contract.clone(),
                    "deadline_unix": hv_deadline_unix,
                    "poll_interval_ms": 500
                }),
            )?;
        }
        loop {
            match buyer
                .resolve_endpoint(chain.as_ref(), &token_contract)
                .await
            {
                Ok(h) => break 'handover h,
                Err(e) => {
                    if is_malformed_handover_error(&e) {
                        if let Some(policy) = buyer_policy.as_ref() {
                            apply_malformed_handover_policy(
                                chain.as_ref(),
                                &buyer,
                                &token_contract,
                                policy,
                                &e,
                            )
                            .await?;
                        }
                        anyhow::bail!("buyer: malformed handover for {token_contract}: {e}");
                    }
                    if std::time::Instant::now() >= hv_deadline {
                        let diagnostic =
                            handover_timeout_diagnostic(chain.as_ref(), &token_contract, &e).await;
                        if let Some(policy) = buyer_policy.as_ref() {
                            match apply_no_handover_after_match_policy(
                                chain.as_ref(),
                                &buyer,
                                &token_contract,
                                policy,
                                service_renewal,
                                handover_attempt,
                                &diagnostic,
                            )
                            .await?
                            {
                                NoHandoverPolicyOutcome::RetryCurrent => continue 'handover,
                                NoHandoverPolicyOutcome::RetryNext(next) => {
                                    token_contract = next;
                                    handover_attempt = handover_attempt.saturating_add(1);
                                    continue 'handover;
                                }
                            }
                        }
                        anyhow::bail!("{}", diagnostic);
                    }
                    tracing::debug!(error = %e, "buyer: no handover yet -- waiting for the seller's open_stream");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    };
    let mut deal_handle = deals::make_handle_id(&token_contract);
    if let Some(events) = machine_events.as_mut() {
        events.event(
            "handover_received",
            machine::OP_BUYER_START,
            json!({
                "token_contract": token_contract.clone(),
                "deal_handle": deal_handle.clone(),
                "handover_anchor": {"kind":"token_contract_state","value":"handover_present"}
            }),
        )?;
    }
    let should_save_handle = !args.mock.mock_chain || machine_events.is_some();
    if should_save_handle {
        let mock_note_addr;
        let note_addr = if args.mock.mock_chain {
            mock_note_addr = format!("mock:{}", note_pubkey_id(&buyer.note.pubkey()));
            mock_note_addr.as_str()
        } else {
            args.identity.note_addr.as_deref().ok_or_else(|| {
                anyhow::anyhow!("real shellnet: --note-addr is required to save the deal handle")
            })?
        };
        let endpoint = Some(deals::DealEndpointInfo {
            kind: if args.local_listen.is_some() {
                "local-listen".to_string()
            } else {
                "one-shot".to_string()
            },
            value: args
                .local_listen
                .map(|a| a.to_string())
                .unwrap_or_else(|| "promptless-mock-stream".to_string()),
        });
        let input = RuntimeDealHandleInput {
            role: deals::DealHandleRole::Buyer,
            deals_dir: args.deals_dir.as_deref(),
            token_contract: &token_contract,
            note_addr,
            frame_model: &frame_model,
            market_path: args.market.as_deref(),
            contracts: &args.contracts,
            endpoint,
        };
        let saved = if args.mock.mock_chain {
            save_mock_runtime_deal_handle(input)?
        } else {
            save_runtime_deal_handle(input, machine_events.is_none())?
        };
        deal_handle = saved.handle;
    }
    // B19/B20: if `--local-listen` is set, bring up a local interface to
    // the consumer(OpenAI-compatible + optional Anthropic transcoding) and serve requests.
    if let Some(bind) = args.local_listen {
        use dexdo::buyer::api::{self, ApiState, Route};
        let continuity_mode = args.continuity_mode.as_planner_mode();
        tracing::info!(
            continuity_mode = args.continuity_mode.as_str(),
            "buyer continuity mode selected"
        );
        let buyer = Arc::new(buyer);
        // Session-scoped settlement: one shared SessionSettle for the deal -- STOP once at session
        // end(graceful shutdown) or on a verification-bail, NOT per request.
        let session = Arc::new(api::SessionSettle::new_with_failure_policy(
            chain.clone(),
            token_contract.clone(),
            buyer.note.clone(),
            api_failure_policy,
        ));
        // pick the content-identity policy for this frame model and build the one-per-deal content gate.
        // A B7-full reference key present in env for this exact frame model enables the gate even when the
        // model has no B8 fingerprint; `content_check_policy` fails closed for a name-only model unless
        // `--allow-unverified-model` was passed.
        let content_identity_model = if args.mock.mock_chain {
            None
        } else {
            Some(resolve_content_identity_model(&args.contracts, &frame_model).await?)
        };
        let content_identity_model_ref = content_identity_model.as_deref();
        let content_check_model = content_identity_model_ref.unwrap_or(&frame_model);
        let has_ref_key = dexdo::buyer::verify::reference_endpoint_for(content_check_model)
            .map(|e| {
                std::env::var(e.api_key_env)
                    .map(|k| !k.is_empty())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let content_check = crate::cli::support::content_check_policy(
            &frame_model,
            content_identity_model_ref,
            args.mock.mock_model,
            args.allow_unverified_model,
            has_ref_key,
        )?;
        let renewal_content_check = content_check.clone();
        let state = ApiState::single(
            buyer,
            Route {
                handover,
                token_contract: token_contract.clone(),
                max_tokens: consumer_api_token_budget(buy_ticks),
            },
            frame_model.clone(),
            session,
            std::sync::Arc::new(dexdo::buyer::api::ContentGate::new(content_check)),
        );
        if let Some((ticks, max_price, escrow)) = service_renewal {
            spawn_buyer_service_renewal(
                chain.clone(),
                state.buyer.clone(),
                state.deals.clone(),
                ticks,
                max_price,
                escrow,
                continuity_mode,
                renewal_content_check,
                api_failure_policy,
            );
        }
        // The operator close: SIGINT(Ctrl-C) or SIGTERM(systemd/container) triggers graceful
        // shutdown, after which `serve()` awaits the session STOP before exit -- the funds-safety terminal (not
        // `Drop`). SIGTERM must NOT bypass it(review).
        if let Some(events) = machine_events.as_mut() {
            events.event(
                "endpoint_binding",
                machine::OP_BUYER_START,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "requested_bind_addr": bind.to_string(),
                    "allow_port_zero": bind.port() == 0
                }),
            )?;
        }
        let (addr, task) = api::serve(
            bind,
            state,
            args.anthropic_compat,
            operator_shutdown_signal(),
        )
        .await?;
        let base_url = format!("http://{addr}/v1");
        let models_url = format!("{base_url}/models");
        if let Some(events) = machine_events.as_mut() {
            let models: serde_json::Value = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()?
                .get(&models_url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let ready = models["data"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["id"].as_str() == Some(frame_model.as_str()))
            });
            if !ready {
                anyhow::bail!("endpoint readiness /v1/models did not include the selected model");
            }
            events.event(
                "endpoint_ready",
                machine::OP_BUYER_RUNTIME,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "bind_addr": addr.to_string(),
                    "base_url": base_url,
                    "models_url": models_url,
                    "served_models": [frame_model.clone()],
                    "anthropic_compat": args.anthropic_compat
                }),
            )?;
        }
        tracing::info!(%addr, anthropic_compat = args.anthropic_compat, "consumer API listening (loopback)");
        task.await?;
        if let Some(events) = machine_events.as_mut() {
            events.event(
                "stopping",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "reason": "signal"
                }),
            )?;
            events.event(
                "settlement_submitted",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "role": "buyer",
                    "action": "streamStop",
                    "submitted": true
                }),
            )?;
            events.event(
                "settled",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "role": "buyer",
                    "action": "streamStop",
                    "state": "stopped",
                    "terminal": false
                }),
            )?;
            events.event(
                "exiting",
                machine::OP_BUYER_SHUTDOWN,
                json!({
                    "token_contract": token_contract.clone(),
                    "deal_handle": deal_handle.clone(),
                    "outcome": "settled",
                    "exit_code": 0
                }),
            )?;
        }
        return Ok(());
    }

    let oneshot_session = dexdo::buyer::api::SessionSettle::new_with_failure_policy(
        chain.clone(),
        token_contract.clone(),
        buyer.note.clone(),
        api_failure_policy,
    );
    let out = match buyer
        .connect_and_stream(&handover, &token_contract, args.max_tokens)
        .await
    {
        Ok(out) => out,
        Err(e) => {
            oneshot_session.settle_dead_gateway("dead-gateway").await;
            return Err(e.context(format!(
                "policy_action failure_class=dead_gateway token_contract={token_contract}"
            )));
        }
    };
    if out.received == 0 {
        oneshot_session.settle_empty_stream("empty-stream").await;
        bail!(
            "policy_action failure_class=empty_stream token_contract={token_contract} \
             state=funded/opened result=zero_tokens_received"
        );
    }
    tracing::info!(received = out.received, "received fake tokens; STOP");
    oneshot_session.settle("one-shot-complete").await;
    Ok(())
}

pub(crate) async fn run_monitor(args: MonitorArgs) -> Result<()> {
    // Real shellnet monitoring: a `RealNote` is a single key, not an HD tree, so the real monitor
    // reads the operator's `--market` manifest(s) by-fact on-chain rather than aggregating a `--tree-width`
    // window. The mock path below still aggregates the note tree.
    if !args.mock.mock_chain {
        return run_monitor_real(&args).await;
    }
    // The monitor reads the mock chain. Read-only, moves nothing.
    let tree = load_note_tree(args.identity.note_key.as_deref())?;
    let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
    let chain = MockChainBackend::new(
        endpoints_file,
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    );
    // Aggregate state over the whole tree: a per-note snapshot for each
    // public key in the `0..tree_width` window, then a roll-up. Each order/deal lives on its own sub-note.
    let mut snaps = Vec::new();
    for pk in tree.node_pubkeys(args.tree_width) {
        snaps.push(chain.note_snapshot(&pk).await?);
    }
    print_tree_snapshot(&aggregate_tree(snaps));
    Ok(())
}

/// Real-shellnet monitor: read the operator's `--market` manifest(s) and print each market's
/// by-fact deal state on-chain through the SAME `print_tree_snapshot` (per-model breakdown + anomaly
/// surfacing) as the mock path. Read-only -- only getters, moves nothing. Each manifest's `TokenContract` is
/// read via `real_market_deal_view`(`getState`/`getProbe` + the buyer pubkey); the model/price come from the
/// manifest. Live-verifiable once a deal `TokenContract` is deployed.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_monitor_real(args: &MonitorArgs) -> Result<()> {
    use dexdo_core::{real_market_deal_view, MarketManifest, RealChainBackend, TreeSnapshot};
    if args.market.is_empty() {
        bail!(
            "real shellnet monitor: pass --market <manifest>... (the operator's `dexdo provision` market \
             record(s)); a RealNote is a single key, not an HD tree, so the monitor reads the markets it is given"
        );
    }
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let mut note_ids = Vec::new();
    let mut deals = Vec::new();
    let mut exposure: u64 = 0;
    for m in &args.market {
        let json = std::fs::read_to_string(m)
            .map_err(|e| anyhow::anyhow!("read --market {}: {e}", m.display()))?;
        let manifest = MarketManifest::from_json(&json)
            .map_err(|e| anyhow::anyhow!("parse --market {}: {e}", m.display()))?;
        manifest
            .validate()
            .map_err(|e| anyhow::anyhow!("--market {}: {e}", m.display()))?;
        note_ids.push(manifest.seller_note.clone());
        // Fail loud(review): the real reader returns an error for an undeployed/unreadable TC or a
        // manifest/getter mismatch -- surface it with the offending --market file, never as empty data.
        let deal = real_market_deal_view(&chain, &manifest)
            .await
            .map_err(|e| anyhow::anyhow!("--market {}: {e}", m.display()))?;
        if let Some(s) = &deal.snapshot {
            if !s.closed {
                // The operator is the SELLER of their own market, so the note's at-risk SHELL is the
                // SELLER-side lock(probe/stake) -- NOT the buyer's deposit. This matches the mock's role-side
                // exposure and `TreeSnapshot.exposure`'s contract("the sum locked by the note").
                exposure = exposure.saturating_add(s.seller_locked);
            }
        }
        deals.push(deal);
    }
    print_tree_snapshot(&TreeSnapshot {
        note_ids,
        offers: Vec::new(),
        deals,
        exposure,
    });
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_monitor_real(_args: &MonitorArgs) -> Result<()> {
    bail!("real shellnet monitoring unavailable: build with `--features shellnet`")
}

/// Provision a per-deal market: the seller note brings up the
/// `InferenceOrderBook`(`deployInferenceOrderBook`) and pre-funds + deploys the `RootModel` + per-deal
/// `TokenContract` from its own ECC[2](`fundDeployShell` -> external seller-signed deploys), **no operator
/// multisig and no giver in the operate path**. Emits a
/// `MarketManifest` whose `token_contract` is the deployed, active address.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_provision(args: ProvisionArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "real shellnet provisioning: --note-addr (provisioned note address) is required"
        )
    })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("real shellnet provisioning: --note-key (note seed) is required")
    })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The deployed book/deal model name/hash MUST be canonical `producer--model--version`(indexer-parseable).
    dexdo_core::validate_canonical_model_id(&args.frame_model).map_err(|e| anyhow::anyhow!(e))?;
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    if let Some(policy) =
        load_enabled_model_registry_policy(RegistryRole::Seller, &args.registry, &args.contracts)?
    {
        let expected_order_book = chain
            .inference_orderbook_address(
                &note,
                &dexdo_core::model_hash_for(&args.frame_model),
                dexdo_core::MODEL_TICK_SIZE,
            )
            .await?
            .with_workchain();
        let order_book_active = order_book_active(&chain, &expected_order_book).await?;
        enforce_model_registry_policy(
            RegistryRole::Seller,
            &policy,
            &args.contracts,
            &args.frame_model,
            &expected_order_book,
            order_book_active,
            BuyerMissingBookPolicy::Reject,
        )
        .await?;
    }
    // REQUIRE an explicit, deal-unique nonce BEFORE any deposit/deploy -- the per-deal TokenContract derives
    // from(sellerPubkey, nonce); the old `--nonce 0` default silently reused(overwrote) a prior deal's TC.
    let nonce = require_provision_nonce(args.nonce)?;
    // the note deposit is a user-chosen provision parameter(default >=100 SHELL), framed by deal volume --
    // NOT a MIN_BALANCE-anchored per-op gas knob. 1 SHELL = 1e9 raw ECC[2]. The deposit is split across the
    // RootModel + per-deal `TokenContract` deploys, funded from the note's own ECC[2].
    let deposit_shells = match args.deposit_shells {
        Some(n) => n,
        None => prompt_deposit_shells()?.unwrap_or(DEFAULT_DEPOSIT_SHELLS),
    };
    // Fail-closed: overflow and a below-floor deposit are explicit errors, not a silent clamp/warn.
    let per_deploy = deposit_per_deploy(deposit_shells)?;
    eprintln!(
        "note deposit: {deposit_shells} SHELL ECC[2] (1 SHELL = 1e9 raw); ~{} SHELL per deploy for RootModel + \
         TokenContract after fundDeployShell. Unused deploy remainder burns at destroy; raise --deposit-shells if a \
         live TC needs more runtime gas.",
        per_deploy / SHELL_UNIT
    );
    // Run the stale/orphaned-note check BEFORE reading ECC balance. After a shellnet redeploy, old notes may be
    // absent/inactive/stale-code; reporting that as "0 SHELL" would mask the actionable re-mint reason.
    chain.assert_seller_note_current(&note).await?;
    // Fail-LOUD if the note's ECC[2] SHELL cannot cover the exact deploy deposit. Do not add guessed runtime
    // headroom here: section 6 requires any gas/SHELL threshold beyond the deploy amount to come from
    // contract constants/receipts, not a drifting reserve.
    let note_ecc = chain
        .client()
        .get_account(&note)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("seller note {note} disappeared after current-note preflight")
        })?
        .ecc_balance(2);
    ensure_provision_deposit_covered(note_ecc, deposit_shells, args.price_per_tick)?;
    let m = chain
        .provision_market(
            &keys,
            &note,
            &args.frame_model,
            nonce,
            args.price_per_tick,
            args.max_ticks,
            per_deploy,
        )
        .await?;
    let json = m.to_json()?;
    std::fs::write(&args.output, &json)
        .map_err(|e| anyhow::anyhow!("write --output {}: {e}", args.output.display()))?;
    println!("provisioned market -> {}", args.output.display());
    println!("{json}");
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_provision(_args: ProvisionArgs) -> Result<()> {
    bail!("real shellnet provisioning unavailable: build with `--features shellnet`")
}

/// `dexdo deploy-market`: deploy the per-model `InferenceOrderBook`(the shared market for a model) if it is
/// not yet on-chain -- note-funded, the explicit "list this model" step a seller runs before posting
/// offers. The book address is deterministic from `model_hash`, so this is idempotent (already-deployed ->
/// no-op). Same lazy deploy the seller's `post_offer` does, surfaced as a first-class operate command.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_market_deploy(args: MarketDeployArgs) -> Result<()> {
    use dexdo_core::{model_hash_for, Address, KeyPair, RealChainBackend, MODEL_TICK_SIZE};
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("real shellnet: --note-addr (active inference note) is required")
    })?;
    let note_key =
        args.identity.note_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-key (note owner key) is required")
        })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The book's on-chain model name/hash MUST be the canonical `producer--model--version` (what the indexer
    // parses); reject an OpenAI slug here BEFORE deploying an un-indexable book.
    dexdo_core::validate_canonical_model_id(&args.frame_model).map_err(|e| anyhow::anyhow!(e))?;
    // Fail-closed on a stale binary / live-network skew BEFORE the on-chain deploy -- same gate `provision`/
    // `seller` run. Without it, deploy-market would silently deploy an order book on outdated contract code
    // against a re-deployed network(a live run caught exactly this: live PrivateNote ahead of the binary pin).
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Seller, &args.registry, &args.contracts)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let model_hash = model_hash_for(&args.frame_model);
    let tick_size = MODEL_TICK_SIZE;
    let ob = chain
        .inference_orderbook_address(&note, &model_hash, tick_size)
        .await?;
    let expected_order_book = ob.with_workchain();
    let book_active = chain.inference_orderbook_stats(&ob).await?.is_some();
    if let Some(policy) = registry_policy.as_ref() {
        enforce_model_registry_policy(
            RegistryRole::Seller,
            policy,
            &args.contracts,
            &args.frame_model,
            &expected_order_book,
            book_active,
            BuyerMissingBookPolicy::Reject,
        )
        .await?;
    }
    if book_active {
        println!(
            "inference market already deployed for {} -- order book {}",
            args.frame_model,
            ob.with_workchain()
        );
        return Ok(());
    }
    println!(
        "deploying inference market (order book) for {} ...",
        args.frame_model
    );
    chain
        .deploy_inference_orderbook(&note, &keys, &model_hash, &args.frame_model, tick_size)
        .await?;
    // Wait for activation so a follow-up `post_offer` doesn't race the deploy(the book getter returns once active).
    for _ in 0..30 {
        if chain.inference_orderbook_stats(&ob).await?.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    println!(
        "deployed inference market for {} -- order book {}",
        args.frame_model,
        ob.with_workchain()
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_market_deploy(_args: MarketDeployArgs) -> Result<()> {
    bail!("real shellnet market deploy unavailable: build with `--features shellnet`")
}

/// the seller CLOSES a STOPped deal's per-deal `TokenContract` via `TokenContract::destroy(payoutAddress)`
/// (`onlyOwnerPubkey(_sellerPubkey)`, gated `!_opened && !_disputed`) -> `selfdestruct(payout)`.
/// **DESTRUCTIVE:** it selfdestructs the TC; the held leftover burns cross-dapp (the raw `selfdestruct` return is
/// not credited back to the cross-dapp note). At the right-sized ~10/deploy funding ( -- MIN_BALANCE gates
/// nothing) that leftover is ~a few vmshell(negligible), so the old fail-closed `--acknowledge-burn` for ~110 is
/// overkill -- it is optional now(kept for back-compat).
#[cfg(feature = "shellnet")]
pub(crate) async fn run_destroy(args: DestroyArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    let _ = args.acknowledge_burn; // optional now(the burn is ~a few vmshell) -- kept for back-compat
    eprintln!(
        "dexdo destroy: selfdestructs the TokenContract; the held leftover (~a few vmshell at the right-sized \
         ~10/deploy funding, ) burns cross-dapp (not credited back to the note) -- negligible."
    );
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("destroy: --note-addr (seller note = payout) is required")
    })?;
    let note_key = args
        .identity
        .note_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("destroy: --note-key (seller owner key) is required"))?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The TC comes from --token-contract OR --market(single source of truth, fail-loud).
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    eprintln!(
        "destroy {tc}: selfdestructs the TokenContract; under right-sized funding the remaining few vmshell \
         burn cross-dapp (not credited back to the note {note}). Seller-signed; requires the deal STOPped \
         (!_opened && !_disputed)."
    );
    chain.destroy_token_contract(&tc, &note, &keys).await?;
    println!(
        "destroy submitted -> TokenContract {tc} selfdestructs; remaining cross-dapp gas is not credited to note {note}"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_destroy(_args: DestroyArgs) -> Result<()> {
    bail!("destroy unavailable: build with `--features shellnet`")
}

/// recover an orphaned OPEN deal. The buyer process died mid-stream but the buyer note/key are intact,
/// so no one sent STOP and the deal hangs OPEN(the seller cannot `destroy` an `_opened` deal). `recover`
/// signs the **normal buyer-STOP** (`streamStop(tokenContract)` -> `TokenContract.stop()`, standard
/// split) from the buyer note -- it does NOT place a new buy -- after which the seller `destroy`s the TC.
/// Fails closed(before sending STOP) if the deal is not `_opened`, is `_disputed`, or the note is not the
/// deal's recorded buyer; the on-chain `TC.stop()` also enforces `msg.sender == _buyer`.
/// (The "seller vanished mid-stream" case is instead the contract's `reclaimOnTimeout`/`STREAM_TIMEOUT`.)
#[cfg(feature = "shellnet")]
pub(crate) async fn run_recover(args: RecoverArgs) -> Result<()> {
    use dexdo_core::{check_recoverable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend};
    let note_addr = args
        .identity
        .note_addr
        .clone()
        .ok_or_else(|| anyhow::anyhow!("recover: --note-addr (buyer note) is required"))?;
    let note_key = args
        .identity
        .note_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("recover: --note-key (buyer owner key) is required"))?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The TC comes from --token-contract OR --market(single source of truth, fail-loud).
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: only an OPEN, undisputed deal owned by THIS buyer note can be STOPped.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("recover: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_recoverable(
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "recover {tc}: buyer-signed STOP of an OPEN deal (streamStop -> TokenContract.stop(), standard \
         split). No new buy is placed. After this, the seller closes it: `dexdo destroy --token-contract {tc}`."
    );
    chain.stream_stop(&note, &keys, &tc).await?;
    println!(
        "recover submitted -> streamStop(TokenContract {tc}) from buyer note {note}; the deal STOPs (standard \
         split). Next: the seller runs `dexdo destroy` to close (selfdestruct) the TokenContract."
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_recover(_args: RecoverArgs) -> Result<()> {
    bail!("recover unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_dispute(args: DisputeArgs) -> Result<()> {
    use dexdo_core::{check_disputable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend};
    let note_addr = args
        .identity
        .note_addr
        .clone()
        .ok_or_else(|| anyhow::anyhow!("dispute: --note-addr (buyer note) is required"))?;
    let note_key = args
        .identity
        .note_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("dispute: --note-key (buyer owner key) is required"))?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // The TC comes from --token-contract OR --market(single source of truth, fail-loud).
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: only an OPEN, undisputed deal owned by THIS buyer note/key can be disputed.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("dispute: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_disputable(
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "dispute {tc}: buyer-signed streamDispute -> TokenContract.dispute() () -- LOCKS BOTH notes (yours \
         and the seller's) until releaseDispute/arbitration. Stronger than `recover` (which still pays the \
         seller for delivered ticks); releaseDispute is seller-only."
    );
    chain.stream_dispute(&note, &keys, &tc).await?;
    println!(
        "dispute submitted -> streamDispute(TokenContract {tc}) from buyer note {note}; the deal is DISPUTED \
         and both notes are locked until it resolves (seller releaseDispute, or arbitration)."
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_dispute(_args: DisputeArgs) -> Result<()> {
    bail!("dispute unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_reclaim(args: ReclaimArgs) -> Result<()> {
    use dexdo_core::{
        check_reclaimable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend,
        MATCH_OPEN_TIMEOUT_SECS,
    };
    let note_addr = args
        .identity
        .note_addr
        .clone()
        .ok_or_else(|| anyhow::anyhow!("reclaim: --note-addr (buyer note) is required"))?;
    let note_key = args
        .identity
        .note_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("reclaim: --note-key (buyer owner key) is required"))?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: owned by THIS buyer + funded + not disputed + the
    // relevant timeout reached. OPEN deals use STREAM_TIMEOUT(streamReclaim); funded-but-never-opened deals use
    // MATCH_OPEN_TIMEOUT from fundedTime(streamCleanup). Reject locally rather than letting the contract revert.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("reclaim: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let funded = state["funded"].as_bool().unwrap_or(false);
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let last_advance = state["lastAdvance"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let funded_time = state["fundedTime"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok());
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    // Per-deal dynamic STREAM_TIMEOUT is only needed for OPEN abandoned deals. The never-opened cleanup path
    // gates on fixed MATCH_OPEN_TIMEOUT from getState.fundedTime.
    let stream_timeout = if opened {
        let cfg = chain
            .token_contract_config(&tc)
            .await?
            .ok_or_else(|| anyhow::anyhow!("reclaim: TokenContract {tc} getConfig unavailable"))?;
        Some(
            cfg["streamTimeout"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("reclaim: getConfig exposes no streamTimeout"))?,
        )
    } else {
        None
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs();
    check_reclaimable(
        funded,
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
        now,
        last_advance,
        stream_timeout,
        funded_time,
        MATCH_OPEN_TIMEOUT_SECS,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if opened {
        let stream_timeout = stream_timeout.expect("opened branch parsed streamTimeout");
        eprintln!(
            "reclaim {tc}: buyer-signed streamReclaim -> TokenContract.reclaimOnTimeout() (no burn: probe + \
             deposit back to you, commission to the seller). STREAM_TIMEOUT met: lastAdvance {last_advance} + \
             streamTimeout {stream_timeout} <= now {now}."
        );
        chain.reclaim_on_timeout(&note, &keys, &tc).await?;
        println!(
            "reclaim submitted -> streamReclaim(TokenContract {tc}) from buyer note {note}; the escrow returns \
             to your note and the deal closes (opened=false)."
        );
    } else {
        let funded_time = funded_time.expect("never-opened branch checked fundedTime");
        eprintln!(
            "reclaim {tc}: buyer-signed streamCleanup -> TokenContract.cleanupUnopened() (never-opened refund). \
             MATCH_OPEN_TIMEOUT met: fundedTime {funded_time} + matchOpenTimeout {MATCH_OPEN_TIMEOUT_SECS} <= \
             now {now}."
        );
        chain.stream_cleanup(&note, &keys, &tc).await?;
        println!(
            "reclaim submitted -> streamCleanup(TokenContract {tc}) from buyer note {note}; the never-opened \
             escrow returns to your note and the deal closes."
        );
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_reclaim(_args: ReclaimArgs) -> Result<()> {
    bail!("reclaim unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_release_dispute(args: ReleaseDisputeArgs) -> Result<()> {
    use dexdo_core::{
        check_release_disputable, check_seller_pubkey, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!("release-dispute: --note-addr (seller note) is required")
        })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("release-dispute: --note-key (seller owner key) is required")
    })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("release-dispute: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    check_release_disputable(disputed).map_err(|e| anyhow::anyhow!(e))?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("release-dispute", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "release-dispute {tc}: seller-signed TokenContract.releaseDispute() from note {note}; concedes the \
         dispute, unlocks both notes, and returns the contested tick/deposit to the buyer."
    );
    chain.release_dispute(&tc, &keys).await?;
    println!(
        "release-dispute submitted -> TokenContract {tc}; both notes unlock after the dispute resolution lands"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_release_dispute(_args: ReleaseDisputeArgs) -> Result<()> {
    bail!("release-dispute unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_withdraw_shell(args: WithdrawShellArgs) -> Result<()> {
    use dexdo_core::{
        check_seller_pubkey, check_withdrawable_shell, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!("withdraw-shell: --note-addr (seller note) is required")
        })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("withdraw-shell: --note-key (seller owner key) is required")
    })?;
    let recipient_addr = args.recipient.clone().unwrap_or_else(|| note_addr.clone());
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let recipient = Address::parse(&recipient_addr)
        .map_err(|e| anyhow::anyhow!("--recipient/--note-addr {recipient_addr}: {e}"))?;

    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("withdraw-shell: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let finalized_owed = state["finalizedOwed"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .ok_or_else(|| anyhow::anyhow!("withdraw-shell: getState exposes no finalizedOwed"))?;
    let amount =
        check_withdrawable_shell(finalized_owed, args.amount).map_err(|e| anyhow::anyhow!(e))?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("withdraw-shell", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "withdraw-shell {tc}: seller-signed TokenContract.withdrawShell(amount={amount}, recipient={recipient}). \
         This withdraws finalized seller proceeds only; use `destroy` later to close/selfdestruct the TC."
    );
    chain.withdraw_shell(&tc, amount, &recipient, &keys).await?;
    println!(
        "withdraw-shell submitted -> {amount} finalized SHELL from TokenContract {tc} to {recipient}"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_withdraw_shell(_args: WithdrawShellArgs) -> Result<()> {
    bail!("withdraw-shell unavailable: build with `--features shellnet`")
}

/// write the `DEXDO_PN_POOL`(carries note owner secret keys) privately + atomically --
/// an exclusive 0600 temp in the destination directory, then `rename` over the target. A plain `fs::write`
/// inherits the umask, and a predictable non-exclusive temp path can clobber a pre-created file/symlink.
#[cfg(feature = "shellnet")]
fn write_pool_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("pn_pool.json");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_nanos();
    let tmp = dir.join(format!(".{name}.tmp.{}.{nanos}", std::process::id()));
    write_pool_private_via_temp(path, &tmp, bytes)
}

#[cfg(feature = "shellnet")]
fn write_pool_private_via_temp(
    path: &std::path::Path,
    tmp: &std::path::Path,
    bytes: &[u8],
) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(tmp)
        .map_err(|e| anyhow::anyhow!("create temp pool {}: {e}", tmp.display()))?;
    if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
        let _ = std::fs::remove_file(tmp);
        return Err(anyhow::anyhow!("write temp pool {}: {e}", tmp.display()));
    }
    std::fs::rename(tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        anyhow::anyhow!("rename temp pool into {}: {e}", path.display())
    })?;
    Ok(())
}

#[cfg(feature = "shellnet")]
fn note_endpoint_url(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        bail!("--endpoint must not be empty");
    }
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        Ok(endpoint.to_string())
    } else {
        Ok(format!("https://{endpoint}"))
    }
}

#[cfg(feature = "shellnet")]
fn note_deploy_multisig_secret_hex(args: &NoteDeployArgs) -> Result<(&'static str, String)> {
    match (&args.multisig_key, &args.multisig_seed_file) {
        (Some(_), Some(_)) => bail!("use only one of --multisig-key or --multisig-seed-file"),
        (Some(path), None) => Ok(("--multisig-key", read_secret_hex(path, "--multisig-key")?)),
        (None, Some(path)) => {
            let phrase = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("read --multisig-seed-file {}: {e}", path.display())
            })?;
            if phrase.split_whitespace().next().is_none() {
                bail!("--multisig-seed-file {} is empty", path.display());
            }
            let key = dexdo::wallet_seed::derive_multisig_key_from_seed_phrase(&phrase)
                .map_err(|e| anyhow::anyhow!("--multisig-seed-file {}: {e}", path.display()))?;
            Ok(("--multisig-seed-file", key.secret_hex().to_string()))
        }
        (None, None) => bail!("one of --multisig-key or --multisig-seed-file is required"),
    }
}

#[cfg(feature = "shellnet")]
fn note_deploy_multisig_keys(args: &NoteDeployArgs) -> Result<dexdo_core::KeyPair> {
    let (source, secret_hex) = note_deploy_multisig_secret_hex(args)?;
    dexdo_core::KeyPair::from_secret_hex(secret_hex.trim())
        .map_err(|e| anyhow::anyhow!("{source} (SDK secret hex): {e:?}"))
}

/// `dexdo note deploy` -- deploy a wallet-funded `PrivateNote` on shellnet in-process through
/// `gosh.ackinacki`, then fold its result into a `DEXDO_PN_POOL` the `seller`/`buyer` consume. The wallet funding
/// secret is read from `--multisig-key` or derived from `--multisig-seed-file`, then passed directly to the SDK.
/// The seed phrase is never printed/logged/stored. The owner secret lands in the pool file(the consumers need it)
/// but is NEVER printed/logged.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_deploy(args: NoteDeployArgs) -> Result<()> {
    use crate::cli::note::{pn_state_to_pool_note, pool_with_note_added, OnboardPnState};
    use dexdo_core::{
        private_note::{
            deploy_private_note_from_multisig, DeployPrivateNoteParams, Halo2Paths, Nominal,
            TokenType,
        },
        Address, ChainClient,
    };

    let multisig_keys = note_deploy_multisig_keys(&args)?;
    let funding_multisig_address = dexdo_core::normalize_wallet_address(&args.multisig_address)
        .map_err(|e| anyhow::anyhow!("--multisig-address: {e}"))?;
    let multisig_address = Address::parse(&funding_multisig_address)
        .map_err(|e| anyhow::anyhow!("--multisig-address: {e}"))?;
    let nominal = Nominal::parse(&args.nominal)?;
    let token_type = TokenType::parse(&args.token_type)?;
    let endpoint = note_endpoint_url(&args.endpoint)?;
    let client = ChainClient::connect(&endpoint)?;
    let halo2_paths = Halo2Paths::from_env();
    halo2_paths.ensure_srs();

    eprintln!(
        "note deploy: in-process gosh.ackinacki -- wallet {} funds a {} {} PrivateNote on {} ...",
        funding_multisig_address,
        nominal.label(),
        token_type.label(),
        endpoint
    );

    let state: OnboardPnState = deploy_private_note_from_multisig(
        &client,
        DeployPrivateNoteParams {
            multisig_address,
            multisig_keys,
            nominal,
            token_type,
            halo2_paths,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("deploy PrivateNote from wallet {funding_multisig_address}: {e}"))?
    .into();
    let note = pn_state_to_pool_note(&state)?;
    let note_addr = note["address"].as_str().unwrap_or_default().to_string();

    // Fold into the pool(create or append). The homogeneity + duplicate guards live in the pure adapter.
    let existing = match std::fs::read(&args.pool) {
        Ok(b) => Some(serde_json::from_slice(&b).map_err(|e| {
            anyhow::anyhow!("--pool {} is not valid JSON: {e}", args.pool.display())
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => bail!("read --pool {}: {e}", args.pool.display()),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs();
    let pool = pool_with_note_added(existing, &state, note, now, &funding_multisig_address)?;
    let pool_json = serde_json::to_string_pretty(&pool)?;
    // the pool carries the note owner secret -- write it 0600 + atomically, not under umask.
    write_pool_private(&args.pool, pool_json.as_bytes())?;

    let n = pool["notes"].as_array().map(|a| a.len()).unwrap_or(0);
    println!(
        "note deployed -> PrivateNote {note_addr} ({} {}); folded into --pool {} ({} note(s)). The owner secret is \
         stored in the pool for the seller/buyer -- keep the file private.",
        state.nominal,
        state.token_type,
        args.pool.display(),
        n
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_deploy(_args: NoteDeployArgs) -> Result<()> {
    bail!("note deploy unavailable: build with `--features shellnet`")
}

/// `dexdo note balance`: address-only, read-only PrivateNote balance diagnostics.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_balance(args: NoteBalanceArgs) -> Result<()> {
    use crate::cli::note::{
        build_note_balance_view, note_getter_balance_maps, render_note_balance,
        unknown_note_getter_balance_maps, NoteAccountSnapshot,
    };
    use dexdo_core::{Address, RealChainBackend};

    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let note = Address::parse(&args.note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {}: {e}", args.note_addr))?;
    let note_display = note.with_workchain();
    let chain = RealChainBackend::connect(manifest)?;
    let account = chain
        .client()
        .get_account(&note)
        .await
        .map_err(|e| anyhow::anyhow!("read PrivateNote account {note_display}: {e}"))?;
    if account.is_none() {
        build_note_balance_view(
            &note_display,
            None,
            unknown_note_getter_balance_maps("account was not readable"),
        )?;
    }
    let details = match chain.private_note_details(&note).await {
        Ok(details) => note_getter_balance_maps(details.as_ref()),
        Err(e) => unknown_note_getter_balance_maps(format!("getDetails error: {e}")),
    };
    let account = account.map(|a| NoteAccountSnapshot {
        address: a.address.with_workchain(),
        status: a.status,
        native_raw: a.balance,
        ecc: a.ecc,
        code_hash: a.code_hash,
    });
    let view = build_note_balance_view(&note_display, account, details)?;
    print!("{}", render_note_balance(&view));
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_balance(_args: NoteBalanceArgs) -> Result<()> {
    bail!("note balance unavailable: build with `--features shellnet`")
}

/// `dexdo note withdraw`: submit owner-signed `PrivateNote.withdrawTokens(destWalletAddr, dapp_id)` for a note's
/// available token balances. It is one-shot and not a blanket proof that every native/ECC balance is retired
/// without by-fact evidence on the current contract. `--to` accepts `half1::half2` or `0:<hex>`.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_note_withdraw(args: NoteWithdrawArgs) -> Result<()> {
    use dexdo_core::{normalize_wallet_address, Address, KeyPair, RealChainBackend};
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("real shellnet: --note-addr (the note to withdraw from) is required")
    })?;
    let note_key =
        args.identity.note_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-key (note owner key) is required")
        })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // Normalize the destination before touching the chain.
    let dest = normalize_wallet_address(&args.to).map_err(|e| anyhow::anyhow!("--to: {e}"))?;
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let dest_addr = Address::parse(&dest).map_err(|e| anyhow::anyhow!("--to {dest}: {e}"))?;
    println!("withdrawing note {note_addr} token balances -> {dest}");
    chain.withdraw_note_tokens(&note, &keys, &dest_addr).await?;
    println!("withdrawTokens submitted for note {note_addr} -> {dest}");
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_note_withdraw(_args: NoteWithdrawArgs) -> Result<()> {
    bail!("note withdraw unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
fn load_oracle_market_manifest(path: &std::path::Path) -> Result<dexdo_core::OracleMarketManifest> {
    let json = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read --manifest {}: {e}", path.display()))?;
    let manifest = dexdo_core::OracleMarketManifest::from_json(&json)
        .map_err(|e| anyhow::anyhow!("parse --manifest {}: {e}", path.display()))?;
    manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("--manifest {}: {e}", path.display()))?;
    Ok(manifest)
}

#[cfg(feature = "shellnet")]
fn pmp_resolved_outcome(details: &serde_json::Value) -> Option<String> {
    let v = &details["resolvedOutcome"];
    if v.is_null() {
        return None;
    }
    v.as_str()
        .map(str::to_string)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| {
            v.as_object()
                .and_then(|o| o.get("value").or_else(|| o.get("0")))
                .and_then(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .or_else(|| x.as_u64().map(|n| n.to_string()))
                })
        })
}

#[cfg(feature = "shellnet")]
fn now_unix_secs() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

#[cfg(feature = "shellnet")]
fn validate_oracle_deadline(deadline: u64, now: u64) -> Result<()> {
    let min_deadline = now.saturating_add(ORACLE_MIN_RESULT_GAP_SECS);
    if deadline < min_deadline {
        bail!(
            "oracle provision: --deadline {deadline} must be at least {ORACLE_MIN_RESULT_GAP_SECS}s \
             in the future for OracleEventList.addRangeEvent (now={now}, min={min_deadline})"
        );
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_oracle(args: OracleArgs) -> Result<()> {
    match args.command {
        OracleCommand::Provision(p) => run_oracle_provision(*p).await,
        OracleCommand::State(s) => run_oracle_state(s).await,
        OracleCommand::Resolve(r) => run_oracle_resolve(r).await,
    }
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_oracle(_args: OracleArgs) -> Result<()> {
    bail!("oracle unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
async fn run_oracle_provision(args: OracleProvisionArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    if args.outcome_names.len() != args.bounds.len() + 1 {
        bail!(
            "oracle provision: pass exactly bounds.len()+1 --outcome values (got {}, expected {})",
            args.outcome_names.len(),
            args.bounds.len() + 1
        );
    }
    if args.initial_stakes.len() != args.outcome_names.len() {
        bail!(
            "oracle provision: pass exactly one --initial-stake per outcome (got {}, expected {})",
            args.initial_stakes.len(),
            args.outcome_names.len()
        );
    }
    validate_oracle_deadline(args.deadline, now_unix_secs()?)?;
    shellnet_doctor_preflight(&args.contracts, Some(args.market.as_path())).await?;

    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("oracle provision: --note-addr (PMP deployer PrivateNote) is required")
    })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("oracle provision: --note-key (PMP deployer note owner key) is required")
    })?;
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let market = load_market(&args.market)?;
    let note_seed = read_secret_hex(note_key, "--note-key")?;
    let oracle_seed = read_secret_hex(&args.oracle_key, "--oracle-key")?;
    let note_keys = KeyPair::from_secret_hex(note_seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let oracle_keys = KeyPair::from_secret_hex(oracle_seed.trim())
        .map_err(|e| anyhow::anyhow!("--oracle-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let manifest = chain
        .provision_oracle_market(
            &note_keys,
            &note,
            &oracle_keys,
            &args.oracle_name,
            args.event_list_index,
            &args.event_list_description,
            &args.event_name,
            args.oracle_fee,
            args.deadline,
            &args.describe,
            &args.bounds,
            &args.outcome_names,
            &market,
            args.token_type,
            &args.initial_stakes,
        )
        .await?;
    let json = manifest.to_json()?;
    std::fs::write(&args.output, &json)
        .map_err(|e| anyhow::anyhow!("write --output {}: {e}", args.output.display()))?;
    println!("oracle market provisioned -> {}", args.output.display());
    println!("{json}");
    Ok(())
}

#[cfg(feature = "shellnet")]
async fn run_oracle_state(args: OracleStateArgs) -> Result<()> {
    use dexdo_core::{Address, RealChainBackend};
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let oel = Address::parse(&manifest.oracle_event_list)
        .map_err(|e| anyhow::anyhow!("oracle_event_list {}: {e}", manifest.oracle_event_list))?;
    let pmp =
        Address::parse(&manifest.pmp).map_err(|e| anyhow::anyhow!("pmp {}: {e}", manifest.pmp))?;
    let range = chain.oracle_range_data(&oel, &manifest.event_id).await?;
    let details = chain.pmp_details(&pmp).await?;
    let pmp_ob = chain.pmp_order_book_address(&pmp).await?;
    println!(
        "oracle_state event={} pmp={} token_type={} deadline={} frame_model={} inference_ob={}",
        manifest.event_id,
        manifest.pmp,
        manifest.token_type,
        manifest.deadline,
        manifest.frame_model,
        manifest.inference_order_book
    );
    match range {
        Some(r) => println!("range_data={}", serde_json::to_string(&r)?),
        None => println!("range_data=<inactive-or-missing>"),
    }
    match details {
        Some(d) => {
            let resolved = pmp_resolved_outcome(&d).unwrap_or_else(|| "none".to_string());
            println!(
                "pmp_details approved={} approved_oracles={}/{} resolved_outcome={} raw={}",
                d["approved"].as_bool().unwrap_or(false),
                d["approvedOracleEvents"].as_str().unwrap_or("0"),
                d["numberOfOracleEvents"].as_str().unwrap_or("0"),
                resolved,
                serde_json::to_string(&d)?
            );
        }
        None => println!("pmp_details=<inactive-or-missing>"),
    }
    if let Some(ob) = pmp_ob {
        println!("pmp_order_book={}", ob.with_workchain());
    }
    Ok(())
}

#[cfg(feature = "shellnet")]
async fn run_oracle_resolve(args: OracleResolveArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    let now = now_unix_secs()?;
    if now < manifest.deadline {
        bail!(
            "oracle resolve: deadline not reached (deadline={}, now={now})",
            manifest.deadline
        );
    }
    shellnet_doctor_preflight(&args.contracts, None).await?;
    let contracts = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let oel = Address::parse(&manifest.oracle_event_list)
        .map_err(|e| anyhow::anyhow!("oracle_event_list {}: {e}", manifest.oracle_event_list))?;
    let pmp =
        Address::parse(&manifest.pmp).map_err(|e| anyhow::anyhow!("pmp {}: {e}", manifest.pmp))?;
    let oracle_seed = read_secret_hex(&args.oracle_key, "--oracle-key")?;
    let oracle_keys = KeyPair::from_secret_hex(oracle_seed.trim())
        .map_err(|e| anyhow::anyhow!("--oracle-key (SDK secret hex): {e:?}"))?;
    chain
        .resolve_oracle_range(
            &oel,
            &oracle_keys,
            &manifest.event_id,
            &manifest.oracle_list_hash,
            manifest.token_type,
        )
        .await?;
    println!(
        "resolveRange submitted event={} oracle_list_hash={} pmp={}",
        manifest.event_id, manifest.oracle_list_hash, manifest.pmp
    );
    let mut last_details_error = None;
    for i in 0..60 {
        match chain.pmp_details(&pmp).await {
            Ok(Some(details)) => {
                if let Some(outcome) = pmp_resolved_outcome(&details) {
                    println!(
                        "pmp resolved event={} outcome={} pmp={}",
                        manifest.event_id, outcome, manifest.pmp
                    );
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("pmp details poll failed (will retry): {e}");
                last_details_error = Some(e.to_string());
            }
        }
        if i + 1 < 60 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
    let last_details_error = last_details_error
        .map(|e| format!(" Last transient pmp_details error while polling: {e}."))
        .unwrap_or_default();
    bail!(
        "resolveRange was submitted but PMP {} did not expose resolvedOutcome within 180s. \
         If the bound InferenceOrderBook has no MIN_LIQUIDITY, requestWeeklyMedian reverts under bounce:false \
         and onWeeklyMedian never arrives; this is the  no-liquidity stuck case, not a CLI success.{}",
        manifest.pmp,
        last_details_error
    )
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "shellnet")]
    use crate::cli::args::NoteDeployArgs;
    use crate::cli::args::SubscriptionPlaceArgs;

    #[test]
    fn buyer_renewal_threshold_uses_env_override() {
        let old = std::env::var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS").ok();
        std::env::set_var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS", "999999");
        assert_eq!(super::buyer_renewal_threshold_tokens(), 999_999);
        match old {
            Some(v) => std::env::set_var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS", v),
            None => std::env::remove_var("DEXDO_BUYER_RENEWAL_THRESHOLD_TOKENS"),
        }
    }

    #[test]
    fn subscription_place_plan_uses_exact_fee_inclusive_escrow() {
        let plan = super::subscription_place_plan(&SubscriptionPlaceArgs {
            max_price_per_tick: 1000,
            ticks: Some(4),
            budget: None,
            auto_renew: false,
        })
        .unwrap();
        assert_eq!(plan.ticks, 4);
        assert_eq!(plan.escrow, 4100);
        assert_eq!(plan.unused_budget, 0);

        let plan = super::subscription_place_plan(&SubscriptionPlaceArgs {
            max_price_per_tick: 1000,
            ticks: None,
            budget: Some(4200),
            auto_renew: false,
        })
        .unwrap();
        assert_eq!(plan.ticks, 4);
        assert_eq!(plan.escrow, 4100);
        assert_eq!(plan.unused_budget, 100);
    }

    #[test]
    fn subscription_place_plan_rejects_zero_sized_money_moves() {
        assert!(super::subscription_place_plan(&SubscriptionPlaceArgs {
            max_price_per_tick: 1000,
            ticks: Some(0),
            budget: None,
            auto_renew: false,
        })
        .is_err());
        assert!(super::subscription_place_plan(&SubscriptionPlaceArgs {
            max_price_per_tick: 1000,
            ticks: None,
            budget: Some(1),
            auto_renew: false,
        })
        .is_err());
        assert!(super::subscription_place_plan(&SubscriptionPlaceArgs {
            max_price_per_tick: 0,
            ticks: Some(1),
            budget: None,
            auto_renew: false,
        })
        .is_err());
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn subscription_status_marks_stale_sub_without_resting_order() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "model".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: Some(dexdo_core::OrderBookStats {
                next_order_id: 2,
                order_count: 0,
                executed_notional: 0,
                executed_ticks: 0,
            }),
            orders: Vec::new(),
        };
        let sub = dexdo_core::OrderBookSubscription {
            order_id: 1,
            exists: true,
            period_start: 10,
            cur_cycle: 0,
            cycle_budget: 10250,
            cycle_spent: 10250,
            auto_renew: false,
        };

        let line = super::render_subscription_line(&snapshot, 1, None, Some(&sub));

        assert!(line.contains("exists=true"));
        assert!(line.contains("order_found=false"));
        assert!(line.contains("stale_subscription=true"));
    }

    /// Demo(run with `--nocapture`): render the model-only order book through the REAL `render_inference_book`
    /// against a `MockChainBackend` seeded with a few asks -- shows exactly what the buyer sees before choosing.
    #[tokio::test]
    async fn demo_render_inference_book() {
        use dexdo_core::{
            ChainBackend, DobParams, LocalNote, MockChainBackend, ProtocolConsts, SellOffer,
        };
        let path = std::env::temp_dir().join("dexdo_book_demo_endpoints.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("chainstate.json"));
        let mock = MockChainBackend::new(path, ProtocolConsts::canonical(), DobParams::canonical());
        let note = LocalNote::generate();
        let asks = [
            (
                "0:7c58eff6aa11b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b",
                900u64,
                512u64,
            ),
            (
                "0:18a758c0bb22c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c",
                1000,
                1024,
            ),
            (
                "0:ab1572e0cc33d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d",
                1500,
                256,
            ),
        ];
        for (tc, price, ticks) in asks {
            mock.post_offer(
                SellOffer {
                    price_per_tick: price,
                    max_ticks: ticks,
                    token_contract: tc.into(),
                },
                &note,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            mock.discover_offers().await.unwrap().len(),
            3,
            "three asks seeded"
        );
        // The buyer's view: model `qwen/qwen3-32b`, price ceiling 1000/tick, default 8 ticks.
        super::render_inference_book(&mock, "qwen/qwen3-32b", 1000, 8)
            .await
            .unwrap();
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn market_manifest_must_match_positional_model() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-market-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let models = dir.join("models.json");
        std::fs::write(
            &models,
            r#"{
              "models": {
                "qwen": {
                  "frame_model": "qwen--qwen3--32b",
                  "base_url": "https://example.invalid/openai/v1",
                  "served_model": "qwen/qwen3-32b",
                  "api_key_env": "QWEN_KEY",
                  "tokenizer_family": "qwen",
                  "price_per_tick": 1000
                },
                "llama": {
                  "frame_model": "llama--llama3--8b",
                  "base_url": "https://example.invalid/openai/v1",
                  "served_model": "llama/llama3-8b",
                  "api_key_env": "LLAMA_KEY",
                  "tokenizer_family": "llama",
                  "price_per_tick": 1000
                }
              }
            }"#,
        )
        .unwrap();
        let manifest = dexdo_core::MarketManifest {
            network: "shellnet".to_string(),
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: dexdo_core::model_hash_for("qwen--qwen3--32b"),
            inference_order_book: "0:book".to_string(),
            root_model: "0:root".to_string(),
            token_contract: "0:tc".to_string(),
            seller_note: "0:seller".to_string(),
            nonce: 7,
            price_per_tick: 1000,
            max_ticks: 8,
        };
        let market = dir.join("market.json");
        std::fs::write(&market, manifest.to_json().unwrap()).unwrap();

        assert!(super::target_from_market_for_model(&market, &models, "qwen").is_ok());
        assert!(super::target_from_market_for_model(&market, &models, "qwen--qwen3--32b").is_ok());
        let err = match super::target_from_market_for_model(&market, &models, "llama") {
            Ok(_) => panic!("wrong positional model must fail closed"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("refusing to render the wrong market"), "{err}");
        assert!(err.contains("llama--llama3--8b"), "{err}");
        assert!(err.contains("qwen--qwen3--32b"), "{err}");
    }

    /// (PR155): static guard -- the seller publishes its offer and confirms THIS TC's ask rested in the IOB
    /// BEFORE binding the gateway, so "gateway listening" cannot false-green as market readiness on an empty book.
    #[test]
    fn seller_gateway_listens_only_after_offer_rested_guard() {
        let source = include_str!("commands.rs");
        let terms = source
            .find(&["sell_", "offer_terms(&token_contract)"].concat())
            .expect("seller reads authoritative TC terms before posting");
        let resume_probe = source
            .find(&["read_", "openable_match_now(&token_contract)"].concat())
            .expect("seller uses a non-blocking resume probe before posting");
        let post = source
            .find(&["post_offer", "_with_note(note.as_ref()"].concat())
            .expect("seller posts the offer before opening the gateway");
        let duplicate = source
            .find(&["assert_", "no_active_sell_order(&token_contract)"].concat())
            .expect("seller rejects duplicate active asks before posting");
        let rested = source
            .find(&["assert_", "offer_rested(&token_contract)"].concat())
            .expect("seller waits for this TC's ask to rest");
        let gateway = source
            .find(&["start_gateway", "_with_note(args.gateway_listen"].concat())
            .expect("seller starts the gateway");

        assert!(
            terms < post,
            "seller offer terms must come from the deployed TC before posting"
        );
        assert!(
            terms < resume_probe && resume_probe < post,
            "fresh seller startup must use the non-blocking resume probe before post_offer"
        );
        assert!(
            !source[terms..post].contains("read_match(&token_contract)"),
            "fresh seller startup must not call the read_match wait-loop before post_offer"
        );
        assert!(
            duplicate < post,
            "seller must reject duplicate active asks before postSellOffer"
        );
        assert!(
            post < rested,
            "seller must publish the offer before checking IOB rest"
        );
        assert!(
            rested < gateway,
            "seller gateway must not listen before this TC's ask rests in the IOB"
        );
    }

    /// seller-side ModelRegistry validation must happen before any offer write can move into
    /// `postSellOffer`.
    #[test]
    fn seller_model_registry_preflight_precedes_offer_post() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("/// One resting ask")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];

        let registry = body
            .find("load_enabled_model_registry_policy")
            .expect("seller registry policy load present");
        let role = body[registry..]
            .find("RegistryRole::Seller")
            .map(|offset| registry + offset)
            .expect("seller registry role present");
        let enforce = body[registry..]
            .find("enforce_model_registry_policy(")
            .map(|offset| registry + offset)
            .expect("seller registry preflight present");
        let post = body
            .find(&["post_offer", "_with_note(note.as_ref()"].concat())
            .expect("seller post_offer present");

        assert!(
            registry < role && role < enforce && enforce < post,
            "seller registry validation must run before postSellOffer"
        );
    }

    /// buyer-side ModelRegistry validation must happen before either direct-deal buy or
    /// model-wide `placeInferenceBuy`.
    #[test]
    fn buyer_model_registry_preflight_precedes_buy_writes() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let end = source[start..]
            .find("pub(crate) async fn run_monitor")
            .map(|offset| start + offset)
            .expect("run_buyer end marker present");
        let body = &source[start..end];

        let registry = body
            .find("load_enabled_model_registry_policy")
            .expect("buyer registry policy load present");
        let role = body[registry..]
            .find("RegistryRole::Buyer")
            .map(|offset| registry + offset)
            .expect("buyer registry role present");
        let enforce = body[registry..]
            .find("enforce_model_registry_policy(")
            .map(|offset| registry + offset)
            .expect("buyer registry preflight present");
        let raw_tc_guard = body[registry..]
            .find("reject_buyer_raw_token_contract_without_registry_book_proof")
            .map(|offset| registry + offset)
            .expect("raw --token-contract guard present");
        let direct_buy = body
            .find("buyer.place_buy(chain.as_ref(), &tc)")
            .expect("direct buy present");
        let model_buy = body
            .find(".place_buy_by_model(")
            .expect("model-only buy present");

        assert!(
            registry < role
                && role < raw_tc_guard
                && raw_tc_guard < enforce
                && enforce < direct_buy,
            "registry check must precede direct buy"
        );
        assert!(
            registry < role && role < enforce && enforce < model_buy,
            "registry check must precede model buy"
        );
    }

    /// regression: under buyer registry validation a raw `--token-contract` does not carry canonical
    /// order-book proof, so it must be rejected before escrow/place_buy. `--market` remains the explicit
    /// trusted path because the manifest carries the book checked by the registry preflight.
    #[test]
    fn buyer_registry_enabled_raw_token_contract_rejected_without_book_proof() {
        let err = super::reject_buyer_raw_token_contract_without_registry_book_proof(
            None,
            Some("0:badtc"),
            "qwen--qwen3--32b",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("raw --token-contract"), "{err}");
        assert!(err.contains("canonical order-book proof"), "{err}");
        assert!(err.contains("buyer.check_model_registry=true"), "{err}");

        let market_path = std::path::Path::new("market.json");
        assert!(
            super::reject_buyer_raw_token_contract_without_registry_book_proof(
                Some(market_path),
                None,
                "qwen--qwen3--32b",
            )
            .is_ok()
        );
        assert!(
            super::reject_buyer_raw_token_contract_without_registry_book_proof(
                None,
                None,
                "qwen--qwen3--32b",
            )
            .is_ok()
        );
    }

    /// machine-mode model-only buy must not emit `quote_selected` from executable discovery alone when
    /// the raw shellnet matcher cannot reach that ask.
    #[test]
    fn buyer_model_only_quote_selection_runs_submit_safe_preflight() {
        let source = include_str!("commands.rs");
        let quote = source
            .find("async fn buyer_quote_selection")
            .expect("buyer quote helper present");
        let body = &source[quote..];
        let preflight = body
            .find("assert_model_buy_matches_executable_quote")
            .expect("model-only quote selection checks raw/executable submit safety");
        let discover = body
            .find("chain.discover_offers")
            .expect("buyer quote selection discovers offers");
        assert!(
            preflight < discover,
            "submit-safety preflight must run before executable discovery is rendered as quote_selected"
        );
    }

    /// `dexdo markets` is a discovery/listing path. With buyer registry validation enabled, a
    /// registered model whose canonical book is missing is hidden from the available list instead of rendered as
    /// buyable.
    #[test]
    fn buyer_markets_hides_missing_canonical_book() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_markets")
            .expect("run_markets present");
        let end = source[start + 1..]
            .find("\npub(crate) async fn run_market")
            .map(|offset| start + 1 + offset)
            .expect("run_markets end marker present");
        let body = &source[start..end];

        let hide_policy = body
            .find("BuyerMissingBookPolicy::HideFromAvailableList")
            .expect("markets uses hide policy");
        let hidden_action = body[hide_policy..]
            .find("RegistryBookAction::BuyerHideMissing")
            .map(|offset| hide_policy + offset)
            .expect("markets handles hidden action");
        let skip = body[hidden_action..]
            .find("continue;")
            .map(|offset| hidden_action + offset)
            .expect("markets skips hidden books");
        let print = body
            .find("println!(")
            .expect("markets prints visible books");

        assert!(
            hide_policy < hidden_action && hidden_action < skip && skip < print,
            "markets must skip inactive registry books before printing available books"
        );
    }

    /// regression: `run_seller` must not own the old bounded match wait. After the offer is posted/rested
    /// and the gateway is listening, match wait + handover provisioning are delegated to the gateway watcher.
    #[test]
    fn seller_run_path_uses_gateway_watcher_not_bounded_read_match() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("/// Render the per-model inference order book")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];

        assert!(
            body.contains("watch_and_serve_match"),
            "seller match wait must be gateway-owned"
        );
        assert!(
            body.contains("seller_watch_cursor_path"),
            "gateway watcher must persist a cursor"
        );
        assert!(
            body.contains("DEFAULT_MATCH_POLL_INTERVAL"),
            "gateway watcher must use the ~30s default poll interval"
        );
        assert!(
            !body.contains("read_match(&token_contract)"),
            "run_seller must not block on the old read_match loop"
        );
        assert!(
            !body.contains("DEAL_WAIT_SECS"),
            "run_seller must not carry the old 300s seller deadline"
        );

        let ready = body.find("seller_ready").expect("seller_ready printed");
        let watch = body.find("watch_and_serve_match").expect("watcher started");
        assert!(
            ready < watch,
            "seller posts/rests and reports readiness before entering the long-running watcher"
        );
    }

    /// the model-only buyer must validate the TC state immediately after its fill event and before
    /// waiting for the seller handover.
    #[test]
    fn model_only_buy_validates_match_state_before_handover_wait() {
        let source = include_str!("commands.rs");
        let buy = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let body = &source[buy..];
        let wait_match = body
            .find("wait_matched_token_contract")
            .expect("model-only buy waits for fill event");
        let validate = body
            .find("validate_reported_match_state")
            .expect("model-only buy validates matched TC state");
        let handover = body
            .find("resolve_endpoint(chain.as_ref(), &token_contract)")
            .expect("buyer waits for handover");
        assert!(
            wait_match < validate && validate < handover,
            "matched TC state must be checked before handover wait"
        );
        assert!(
            body.contains("handover_timeout_diagnostic"),
            "handover timeout must re-read TC state for funded-never-opened recovery diagnostics"
        );
    }

    /// in machine mode, model-only buy submission is its own by-fact event. It must be emitted
    /// immediately after `place_buy_by_model` returns, before the process can block in fill/match polling.
    #[test]
    fn model_only_buy_submitted_is_emitted_before_match_wait_path() {
        let source = include_str!("commands.rs");
        let buy = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let body = &source[buy..];
        let model_only = body
            .find("None => {\n // Show the book")
            .expect("model-only branch present");
        let segment = &body[model_only..];
        let submit = segment
            .find("place_buy_by_model")
            .expect("model-only submit present");
        let buy_event = segment
            .find("\"buy_submitted\"")
            .expect("buy_submitted event present");
        let wait_match = segment
            .find("wait_matched_token_contract")
            .expect("match wait present");
        assert!(
            submit < buy_event && buy_event < wait_match,
            "model-only buyer must emit buy_submitted after submit returns and before match wait"
        );
    }

    #[test]
    fn policy_cleanup_rechecks_state_after_wait_before_cleanup() {
        let source = include_str!("commands.rs");
        let start = source
            .find("async fn policy_cleanup_unopened_after_match_timeout")
            .expect("policy cleanup helper present");
        let end = source[start..]
            .find("async fn apply_no_handover_after_match_policy")
            .map(|offset| start + offset)
            .expect("policy cleanup helper end marker present");
        let body = &source[start..end];
        let sleep = body
            .find("tokio::time::sleep")
            .expect("cleanup wait present");
        let recheck = body[sleep..]
            .find("validate_reported_match_state")
            .map(|offset| sleep + offset)
            .expect("state recheck after wait present");
        let cleanup = body
            .find("chain.cleanup_unopened")
            .expect("cleanup lever present");
        assert!(
            sleep < recheck && recheck < cleanup,
            "cleanup must re-read TC state after waiting and before cleanup_unopened"
        );
        assert!(
            body.contains("not_cleanup_unopened_state_after_wait"),
            "unexpected post-wait states must not be cleaned up silently"
        );
        assert!(
            body.contains("handover_opened_after_wait"),
            "late-opened deals must return to the handover path instead of failing cleanup"
        );
    }

    #[test]
    fn policy_buyer_failure_classes_dispatch_runtime_levers() {
        let source = include_str!("commands.rs");
        let malformed = source
            .find("async fn apply_malformed_handover_policy")
            .expect("malformed handover policy helper present");
        let cleanup = source[malformed..]
            .find("async fn policy_cleanup_unopened_after_match_timeout")
            .map(|offset| malformed + offset)
            .expect("malformed helper end marker present");
        let malformed_body = &source[malformed..cleanup];
        assert!(
            malformed_body.contains("chain.seller_timeout(token_contract)"),
            "malformed_handover=reclaim must invoke the reclaim lever"
        );
        assert!(
            malformed_body.contains("chain.dispute(token_contract, buyer.note.as_ref())"),
            "malformed_handover=dispute must invoke stream dispute"
        );

        let buy = source
            .find("pub(crate) async fn run_buyer")
            .expect("run_buyer present");
        let monitor = source[buy..]
            .find("pub(crate) async fn run_monitor")
            .map(|offset| buy + offset)
            .expect("run_buyer end marker present");
        let body = &source[buy..monitor];
        assert!(
            body.contains("is_malformed_handover_error(&e)")
                && body.contains("apply_malformed_handover_policy"),
            "run_buyer must route malformed/decrypt handovers through policy"
        );
        assert!(
            body.contains("settle_dead_gateway(\"dead-gateway\")"),
            "one-shot buyer stream open/connect errors must route through dead_gateway policy"
        );
        assert!(
            body.contains("settle_empty_stream(\"empty-stream\")"),
            "one-shot buyer zero-token stream must route through empty_stream policy"
        );
    }

    #[test]
    fn policy_seller_fields_dispatch_or_fail_closed_explicitly() {
        let source = include_str!("commands.rs");
        let enforce = source
            .find("fn enforce_seller_runtime_policy")
            .expect("seller max-open policy helper present");
        let run = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let helpers = &source[enforce..run];
        assert!(
            helpers.contains("supported=1"),
            "seller max_open_deals must be enforced before offer posting"
        );
        assert!(
            helpers.contains("chain.release_dispute(token_contract)"),
            "seller dispute_against_me=release_if_clean must invoke release_dispute"
        );
        assert!(
            helpers.contains("policy_action_unsupported"),
            "seller unsupported republish/cleanup surfaces must fail closed explicitly"
        );

        let end = source[run..]
            .find("/// Render the per-model inference order book")
            .map(|offset| run + offset)
            .expect("run_seller end marker present");
        let body = &source[run..end];
        assert!(body.contains("enforce_seller_runtime_policy(policy)?"));
        assert!(body.contains("apply_seller_dispute_policy"));
        assert!(body.contains("apply_seller_terminal_policy"));
    }

    /// (file or symlink) is not truncated/clobbered before the final atomic rename.
    #[cfg(feature = "shellnet")]
    #[test]
    fn write_pool_private_refuses_preexisting_temp_path() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-pool-temp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let target = dir.join("pn_pool.json");
        let tmp = dir.join(".pn_pool.json.tmp.preexisting");
        std::fs::write(&tmp, b"do-not-clobber").unwrap();

        let err = super::write_pool_private_via_temp(&target, &tmp, b"secret-pool")
            .unwrap_err()
            .to_string();

        assert!(err.contains("create temp pool"), "unexpected error: {err}");
        assert_eq!(std::fs::read(&tmp).unwrap(), b"do-not-clobber");
        assert!(
            !target.exists(),
            "target must not be written after temp creation failed"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_endpoint_url_accepts_bare_host_or_url() {
        assert_eq!(
            super::note_endpoint_url("shellnet.ackinacki.org").unwrap(),
            "https://shellnet.ackinacki.org"
        );
        assert_eq!(
            super::note_endpoint_url("https://shellnet.ackinacki.org/").unwrap(),
            "https://shellnet.ackinacki.org"
        );
        assert!(super::note_endpoint_url("  ").is_err());
    }

    #[cfg(feature = "shellnet")]
    fn note_deploy_args(
        multisig_key: Option<std::path::PathBuf>,
        multisig_seed_file: Option<std::path::PathBuf>,
    ) -> NoteDeployArgs {
        NoteDeployArgs {
            multisig_address: format!("0:{}", "1".repeat(64)),
            multisig_key,
            multisig_seed_file,
            nominal: "N100".into(),
            token_type: "nackl".into(),
            endpoint: "shellnet.ackinacki.org".into(),
            pool: std::path::PathBuf::from("pn_pool.json"),
        }
    }

    #[cfg(feature = "shellnet")]
    fn tvm_tonos_fixture_phrase() -> String {
        const WORD_INDICES: [u16; 12] = [
            1636, 1293, 905, 102, 1057, 1956, 1247, 1750, 597, 881, 1302, 3,
        ];
        WORD_INDICES
            .iter()
            .map(|i| bip39::Language::English.wordlist().get_word((*i).into()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(feature = "shellnet")]
    fn pinned_tvm_sdk_default_key(phrase: &str) -> tvm_client::crypto::KeyPair {
        assert_eq!(
            tvm_client::crypto::default_hdkey_derivation_path(),
            dexdo::wallet_seed::TVM_DEFAULT_DERIVATION_PATH
        );
        let context = std::sync::Arc::new(
            tvm_client::ClientContext::new(tvm_client::ClientConfig::default()).unwrap(),
        );
        tvm_client::crypto::mnemonic_derive_sign_keys(
            context,
            tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys {
                phrase: phrase.to_owned(),
                path: None,
                dictionary: None,
                word_count: None,
            },
        )
        .unwrap()
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_seed_file_matches_key_file_input() {
        let phrase = tvm_tonos_fixture_phrase();
        let expected_key = pinned_tvm_sdk_default_key(&phrase);
        let dir = std::env::temp_dir().join(format!(
            "dexdo-note-deploy-seed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let key_path = dir.join("wallet.secret.hex");
        let seed_path = dir.join("wallet.seed");
        std::fs::write(&key_path, &expected_key.secret).unwrap();
        std::fs::write(&seed_path, phrase).unwrap();

        let (key_source, key_secret) =
            super::note_deploy_multisig_secret_hex(&note_deploy_args(Some(key_path), None))
                .unwrap();
        let (seed_source, seed_secret) =
            super::note_deploy_multisig_secret_hex(&note_deploy_args(None, Some(seed_path)))
                .unwrap();

        assert_eq!(key_source, "--multisig-key");
        assert_eq!(seed_source, "--multisig-seed-file");
        assert!(
            key_secret == expected_key.secret,
            "key-file input does not match pinned TVM SDK default secret"
        );
        assert!(
            seed_secret == expected_key.secret,
            "seed-file input does not match pinned TVM SDK default secret"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn note_deploy_seed_file_errors_do_not_echo_seed_input() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-note-deploy-invalid-seed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let key_path = dir.join("wallet.secret.hex");
        let seed_path = dir.join("wallet.seed");
        let invalid = "zzzz zzzz zzzz";
        std::fs::write(&key_path, "00").unwrap();
        std::fs::write(&seed_path, invalid).unwrap();

        let err = super::note_deploy_multisig_secret_hex(&note_deploy_args(
            Some(key_path),
            Some(seed_path.clone()),
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("only one"), "{err}");

        let err = super::note_deploy_multisig_secret_hex(&note_deploy_args(None, Some(seed_path)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid seed phrase"), "{err}");
        assert!(!err.contains(invalid), "{err}");

        let missing = dir.join("missing.seed");
        let err = super::note_deploy_multisig_secret_hex(&note_deploy_args(None, Some(missing)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("read --multisig-seed-file"), "{err}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_close_reclaims_opened_deal_after_stream_timeout() {
        assert_eq!(
            super::buyer_opened_close_action(699, 100, 600),
            super::BuyerOpenedCloseAction::StreamStop
        );
        assert_eq!(
            super::buyer_opened_close_action(700, 100, 600),
            super::BuyerOpenedCloseAction::StreamReclaim
        );
        assert_eq!(
            super::buyer_opened_close_action(u64::MAX - 1, u64::MAX - 10, 600),
            super::BuyerOpenedCloseAction::StreamStop
        );
    }

    #[test]
    fn buyer_renewal_monitor_uses_planner_and_recovery_actions() {
        let source = include_str!("commands.rs");
        let start = source
            .find("fn spawn_buyer_service_renewal")
            .expect("renewal task present");
        let end = source[start..]
            .find("pub(crate) async fn run_buyer")
            .map(|offset| start + offset)
            .expect("renewal task end marker present");
        let body = &source[start..end];
        assert!(body.contains("BuyerContinuity"), "{body}");
        assert!(body.contains("planner.tick_with_mode"), "{body}");
        assert!(body.contains("continuity_mode"), "{body}");
        assert!(body.contains("has_active_or_recent_request"), "{body}");
        assert!(body.contains("CONSUMER_DEMAND_RECENT_SECS"), "{body}");
        assert!(body.contains("deal_state"), "{body}");
        assert!(body.contains("cleanup_unopened"), "{body}");
        assert!(body.contains("execute_buyer_monitor_recovery"), "{body}");
        assert!(source.contains("chain.seller_timeout"), "{source}");
        assert!(body.contains("RENEWAL_FAILURE_BACKOFF_SECS"), "{body}");
        assert!(body.contains("prepare_retry"), "{body}");
        assert!(!body.contains("pending_for"), "{body}");
    }

    #[derive(Default)]
    struct RecordingRecoveryChain {
        cleanup_calls: std::sync::atomic::AtomicUsize,
        reclaim_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl dexdo_core::ChainBackend for RecordingRecoveryChain {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn place_buy(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn read_match(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn open_stream(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn read_handover(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn advance_tick(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn accept_probe(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn stop(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            unimplemented!("not needed by recovery monitor tests")
        }

        async fn seller_timeout(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.reclaim_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            })
        }

        async fn cleanup_unopened(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.cleanup_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(dexdo_core::Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            })
        }

        async fn snapshot(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    #[tokio::test]
    async fn buyer_monitor_chain_facts_execute_recovery_once() {
        use dexdo::buyer::continuity::{BuyerAction, BuyerContinuity, ContinuityConfig, DealFacts};
        use std::sync::atomic::Ordering;

        let cfg = ContinuityConfig {
            renewal_threshold_tokens: 10,
            match_open_timeout_secs: 600,
            stream_timeout_secs: 600,
        };
        let chain = RecordingRecoveryChain::default();

        let opened_idle = super::buyer_monitor_current_facts(
            "tc-open".to_string(),
            100,
            false,
            Some(dexdo_core::DealChainState {
                funded: true,
                opened: true,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(1),
                last_advance: 100,
            }),
            700,
        );
        let mut planner = BuyerContinuity::default();
        let action = planner.tick(Some(opened_idle), None, cfg);
        assert_eq!(
            action,
            BuyerAction::ReclaimOpened {
                token_contract: "tc-open".to_string()
            }
        );
        let (kind, tc, result) = super::execute_buyer_monitor_recovery(&chain, action)
            .await
            .expect("reclaim action executes");
        assert_eq!(kind, super::BuyerMonitorRecoveryKind::ReclaimOpened);
        assert_eq!(tc, "tc-open");
        assert!(result.is_ok());
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            planner.tick(Some(DealFacts::opened_idle("tc-open", 601)), None, cfg),
            BuyerAction::IgnoreStale { token_contract } if token_contract == "tc-open"
        ));
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 1);

        let never_opened = super::buyer_monitor_current_facts(
            "tc-clean".to_string(),
            100,
            false,
            Some(dexdo_core::DealChainState {
                funded: true,
                opened: false,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(100),
                last_advance: 0,
            }),
            700,
        );
        let mut planner = BuyerContinuity::default();
        let action = planner.tick(Some(never_opened), None, cfg);
        assert_eq!(
            action,
            BuyerAction::CleanupUnopened {
                token_contract: "tc-clean".to_string()
            }
        );
        let (kind, tc, result) = super::execute_buyer_monitor_recovery(&chain, action)
            .await
            .expect("cleanup action executes");
        assert_eq!(kind, super::BuyerMonitorRecoveryKind::CleanupUnopened);
        assert_eq!(tc, "tc-clean");
        assert!(result.is_ok());
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            planner.tick(
                Some(DealFacts::funded_never_opened("tc-clean", 601)),
                None,
                cfg
            ),
            BuyerAction::IgnoreStale { token_contract } if token_contract == "tc-clean"
        ));
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn oracle_deadline_enforces_contract_result_gap() {
        let now = 1_900_000_000;
        assert!(super::validate_oracle_deadline(now + 119, now).is_err());
        assert!(super::validate_oracle_deadline(now + 120, now).is_ok());
    }

    #[cfg(feature = "shellnet")]
    struct TempDirCleanup(std::path::PathBuf);

    #[cfg(feature = "shellnet")]
    impl Drop for TempDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
