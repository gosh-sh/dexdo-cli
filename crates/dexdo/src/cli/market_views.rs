//! Market-data and quote read/display command handlers(Track C5, move-only).

use crate::cli::args::*;
#[cfg(feature = "shellnet")]
use crate::cli::commands::{
    direct_chain_read_with_timeout, enforce_model_registry_policy, fold_snapshot_from_orders,
    load_enabled_model_registry_policy, model_target_from_config, print_book_table,
    read_book_target, read_executable_book_target, resolve_order_book_target,
    retry_executable_read, snapshot_with_executable_orders, target_from_market,
    target_from_market_for_model, BookRow, BookTarget,
};
use crate::cli::commands::{mock_chain_for_machine, mock_orders_from_offers};
use crate::cli::indexer::{self, DepthQuery, IndexerClient, MarketsQuery};
use crate::cli::machine;
use anyhow::{bail, Result};
#[cfg(feature = "shellnet")]
use dexdo::registry::{BuyerMissingBookPolicy, RegistryRole};
use dexdo_core::{executable_quote, model_hash_for, ChainBackend};
#[cfg(feature = "shellnet")]
use dexdo_core::{
    shellnet::BookEventFold, submit_safe_single_ask_quote, DobParams, ExecutableQuote,
    OrderBookOrder, OrderBookSnapshot,
};
#[cfg(feature = "shellnet")]
use serde_json::json;
#[cfg(feature = "shellnet")]
use std::future::Future;

#[cfg(feature = "shellnet")]
const INDEXER_FAST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(feature = "shellnet")]
#[derive(Debug)]
struct IndexerMarketContext {
    last_update_id: String,
}

#[cfg(feature = "shellnet")]
#[derive(Debug)]
struct ExecutableMarketView {
    snapshot: OrderBookSnapshot,
    active: bool,
    source: &'static str,
    last_update_id: String,
}

#[cfg(feature = "shellnet")]
async fn read_indexer_market_context(order_book: &str) -> Result<IndexerMarketContext> {
    let base_url = indexer::resolve_base_url(None)?;
    let client = IndexerClient::new(base_url, INDEXER_FAST_TIMEOUT)?;
    let markets = client
        .markets(MarketsQuery {
            inference_order_book_address: Some(order_book),
            limit: Some(1),
            ..MarketsQuery::default()
        })
        .await?;
    if !markets.markets.iter().any(|market| {
        market
            .inference_order_book_address
            .eq_ignore_ascii_case(order_book)
    }) {
        bail!("Dodex indexer has no market context for {order_book}");
    }
    let depth = client
        .depth(DepthQuery {
            inference_order_book_address: order_book,
            limit: None,
        })
        .await?;
    Ok(IndexerMarketContext {
        last_update_id: if depth.last_update_id.is_empty() {
            "-".to_string()
        } else {
            depth.last_update_id
        },
    })
}

#[cfg(feature = "shellnet")]
async fn read_executable_market_view_with<FI, FFI, FF, FFF, FB, FBFut>(
    mut indexer_read: FI,
    mut fold_read: FF,
    mut fallback_read: FB,
) -> Result<ExecutableMarketView>
where
    FI: FnMut() -> FFI,
    FFI: Future<Output = Result<IndexerMarketContext>>,
    FF: FnMut() -> FFF,
    FFF: Future<Output = Result<(OrderBookSnapshot, String)>>,
    FB: FnMut() -> FBFut,
    FBFut: Future<Output = Result<OrderBookSnapshot>>,
{
    let indexer = retry_executable_read("indexer market context", &mut indexer_read).await;
    match retry_executable_read("order-book event fold", &mut fold_read).await {
        Ok((snapshot, fold_id)) => {
            let (source, last_update_id) = match indexer {
                Ok(context) => ("indexer", context.last_update_id),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "indexer unavailable; using chain event context");
                    ("chain", fold_id)
                }
            };
            Ok(ExecutableMarketView {
                snapshot,
                active: true,
                source,
                last_update_id,
            })
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "order-book event fold unavailable; using legacy chain fallback");
            let snapshot =
                retry_executable_read("legacy order-book fallback", &mut fallback_read).await?;
            let active = snapshot.active();
            Ok(ExecutableMarketView {
                snapshot,
                active,
                source: "chain",
                last_update_id: "-".to_string(),
            })
        }
    }
}

#[cfg(feature = "shellnet")]
async fn read_executable_market_view(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
    order_book: &str,
) -> Result<ExecutableMarketView> {
    read_executable_market_view_with(
        || read_indexer_market_context(order_book),
        || async {
            let fold = chain
                .fold_order_book_events(order_book, BookEventFold::default())
                .await?;
            let last_update_id = fold.last_seen_id().unwrap_or("-").to_string();
            let snapshot = fold_snapshot_from_orders(target, order_book, fold.live_orders());
            let executable_orders = chain.executable_resting_asks(&snapshot).await?;
            let snapshot = snapshot_with_executable_orders(snapshot, executable_orders);
            Ok((snapshot, last_update_id))
        },
        || read_executable_book_target(chain, target),
    )
    .await
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
fn executable_market_rows(snapshot: &OrderBookSnapshot) -> Vec<BookRow> {
    snapshot
        .resting_asks()
        .map(|order| BookRow {
            price_per_tick: order.price_per_tick,
            max_ticks: order.ticks,
            token_contract: order
                .token_contract
                .as_ref()
                .map(|token_contract| token_contract.to_string())
                .unwrap_or_else(|| "-".to_string()),
        })
        .collect()
}

#[cfg(feature = "shellnet")]
fn render_market_context(source: &str, last_update_id: &str) -> String {
    format!("market source={source} lastUpdateId={last_update_id}")
}

#[cfg(feature = "shellnet")]
fn render_quote_summary(
    snapshot: &OrderBookSnapshot,
    quote: &ExecutableQuote,
    source: &str,
    last_update_id: &str,
) -> String {
    if quote.filled_ticks == 0 {
        return format!(
            "quote model={} order_book={} source={} lastUpdateId={} no_liquidity=true",
            snapshot.frame_model, snapshot.order_book, source, last_update_id
        );
    }
    format!(
        "quote model={} order_book={} source={} lastUpdateId={} filled_ticks={} total_with_fee={} complete={}",
        snapshot.frame_model,
        snapshot.order_book,
        source,
        last_update_id,
        quote.filled_ticks,
        quote.total_with_fee,
        quote.complete
    )
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
    let view = direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
        let order_book = resolve_order_book_target(&chain, &target).await?;
        let view = read_executable_market_view(&chain, &target, &order_book).await?;
        if let Some(policy) = registry_policy.as_ref() {
            enforce_model_registry_policy(
                RegistryRole::Buyer,
                policy,
                &args.contracts,
                &target.frame_model,
                &view.snapshot.order_book,
                view.active,
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
        Ok(view)
    })
    .await?;
    let snapshot = &view.snapshot;
    let rows = executable_market_rows(snapshot);
    println!(
        "{}",
        render_market_context(view.source, &view.last_update_id)
    );
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
fn selection_error_is_empty_book_state(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("no executable matching ask")
        || reason.contains("no submit-safe ask")
        || reason.contains("best ask price")
        || reason.contains("no resting asks")
        || reason.contains("no matchable ask")
        || reason.contains("raw order-book matcher")
        || reason.contains("refusing multi-ask fill")
}

#[cfg(feature = "shellnet")]
fn render_executable_book_line(
    snapshot: &OrderBookSnapshot,
    order: &OrderBookOrder,
    ticks: u128,
    max_price_per_tick: u128,
) -> String {
    format!(
        "executable_ask model={} order_book={} order_id={} token_contract={} price_per_tick={} ticks={} requested_ticks={} max_price_per_tick={}",
        snapshot.frame_model,
        snapshot.order_book,
        order.order_id,
        order.token_contract.as_deref().unwrap_or("-"),
        order.price_per_tick,
        order.ticks,
        ticks,
        max_price_per_tick
    )
}

#[cfg(feature = "shellnet")]
fn render_no_executable_book_line(
    snapshot: &OrderBookSnapshot,
    ticks: u128,
    max_price_per_tick: u128,
    reason: &str,
) -> String {
    format!(
        "executable_ask model={} order_book={} none=true no_executable_ask=true requested_ticks={} max_price_per_tick={} reason={}",
        snapshot.frame_model,
        snapshot.order_book,
        ticks,
        max_price_per_tick,
        reason.replace('\n', " ")
    )
}

#[cfg(feature = "shellnet")]
fn render_executable_book_output(
    snapshot: &OrderBookSnapshot,
    orders: &[OrderBookOrder],
    ticks: u128,
    max_price_per_tick: u128,
    empty_reason: Option<&str>,
) -> String {
    if orders.is_empty() {
        return render_no_executable_book_line(
            snapshot,
            ticks,
            max_price_per_tick,
            empty_reason.unwrap_or("no executable matching ask"),
        );
    }
    orders
        .iter()
        .map(|order| render_executable_book_line(snapshot, order, ticks, max_price_per_tick))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `dexdo executable-book <model>`: show all currently executable asks for this tick count and ceiling.
/// Rows hidden behind a stale cheaper raw row are intentionally not listed, because the model-wide matcher
/// would hit that unsafe row first.
#[cfg(feature = "shellnet")]
pub(crate) async fn run_executable_book(args: ExecutableBookArgs) -> Result<()> {
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &args.contracts)?;
    let chain = dexdo_core::RealChainBackend::connect(
        args.contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?,
    )?;
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
    let (snapshot, orders, empty_reason) =
        direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
            let snapshot = read_book_target(&chain, &target).await?;
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
            match chain
                .submit_safe_executable_book_asks(&snapshot, args.ticks, args.max_price_per_tick)
                .await
            {
                Ok((orders, reason)) => Ok((snapshot, orders, reason)),
                Err(err) if selection_error_is_empty_book_state(&format!("{err:#}")) => {
                    Ok((snapshot, Vec::new(), Some(format!("{err:#}"))))
                }
                Err(err) => Err(err),
            }
        })
        .await?;
    println!(
        "{}",
        render_executable_book_output(
            &snapshot,
            &orders,
            args.ticks,
            args.max_price_per_tick,
            empty_reason.as_deref()
        )
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_executable_book(_args: ExecutableBookArgs) -> Result<()> {
    bail!("executable-book unavailable: build with `--features shellnet`")
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
    let (view, q) = direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
        let order_book = resolve_order_book_target(&chain, &target).await?;
        let view = read_executable_market_view(&chain, &target, &order_book).await?;
        if let Some(policy) = registry_policy.as_ref() {
            enforce_model_registry_policy(
                RegistryRole::Buyer,
                policy,
                &args.contracts,
                &target.frame_model,
                &view.snapshot.order_book,
                view.active,
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
        let q = submit_safe_single_ask_quote(&view.snapshot.orders, args.ticks, args.budget)
            .map_err(|e| anyhow::anyhow!("quote: {e}"))?;
        Ok((view, q))
    })
    .await?;
    let snapshot = &view.snapshot;
    if args.json {
        let response = quote_response_from_quote(
            "shellnet",
            &snapshot.frame_model,
            &snapshot.order_book,
            args.ticks,
            args.budget,
            q,
        )?;
        let mut response = serde_json::to_value(response)?;
        let object = response
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("quote response is not an object"))?;
        object.insert("source".to_string(), json!(view.source));
        object.insert("lastUpdateId".to_string(), json!(view.last_update_id));
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    if q.filled_ticks == 0 {
        println!(
            "{}",
            render_quote_summary(snapshot, &q, view.source, &view.last_update_id)
        );
        return Ok(());
    }
    println!(
        "{}",
        render_quote_summary(snapshot, &q, view.source, &view.last_update_id)
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "shellnet")]
    fn wire_read_target() -> super::BookTarget {
        super::BookTarget {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "model-hash".to_string(),
            order_book: Some("0:book".to_string()),
            root_model: None,
            note_addr: None,
        }
    }

    #[cfg(feature = "shellnet")]
    fn wire_live_order(
        order_id: u128,
        price: u128,
        token_contract: &str,
    ) -> dexdo_core::shellnet::LiveBookOrder {
        dexdo_core::shellnet::LiveBookOrder {
            order_id,
            is_buy: false,
            price,
            ticks_remaining: 8,
            note: "0:seller".to_string(),
            token_contract: token_contract.to_string(),
            deadline: 1_900_000_000,
        }
    }

    #[cfg(feature = "shellnet")]
    fn wire_snapshot() -> dexdo_core::OrderBookSnapshot {
        let target = wire_read_target();
        let orders = [wire_live_order(7, 20, "0:live")];
        super::fold_snapshot_from_orders(&target, "0:book", orders.iter())
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn market_uses_indexer_for_fast_path_no_getorder_walk() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let indexer_calls = Arc::new(AtomicUsize::new(0));
        let fold_calls = Arc::new(AtomicUsize::new(0));
        let getorder_walk_calls = Arc::new(AtomicUsize::new(0));
        let snapshot = wire_snapshot();
        let view = super::read_executable_market_view_with(
            {
                let calls = indexer_calls.clone();
                move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(super::IndexerMarketContext {
                            last_update_id: "indexer-77".to_string(),
                        })
                    }
                }
            },
            {
                let calls = fold_calls.clone();
                let snapshot = snapshot.clone();
                move || {
                    let calls = calls.clone();
                    let snapshot = snapshot.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok((snapshot, "fold-12".to_string()))
                    }
                }
            },
            {
                let calls = getorder_walk_calls.clone();
                let snapshot = snapshot.clone();
                move || {
                    let calls = calls.clone();
                    let snapshot = snapshot.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(snapshot)
                    }
                }
            },
        )
        .await
        .expect("indexer and fold reads succeed");

        assert_eq!(view.source, "indexer");
        assert_eq!(view.last_update_id, "indexer-77");
        assert_eq!(indexer_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fold_calls.load(Ordering::SeqCst), 1);
        assert_eq!(getorder_walk_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            super::render_market_context(view.source, &view.last_update_id),
            "market source=indexer lastUpdateId=indexer-77"
        );
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn market_falls_back_to_chain_when_indexer_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let indexer_calls = Arc::new(AtomicUsize::new(0));
        let fold_calls = Arc::new(AtomicUsize::new(0));
        let getorder_walk_calls = Arc::new(AtomicUsize::new(0));
        let snapshot = wire_snapshot();
        let view = super::read_executable_market_view_with(
            {
                let calls = indexer_calls.clone();
                move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(anyhow::anyhow!("Dodex indexer HTTP 500"))
                    }
                }
            },
            {
                let calls = fold_calls.clone();
                let snapshot = snapshot.clone();
                move || {
                    let calls = calls.clone();
                    let snapshot = snapshot.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok((snapshot, "fold-13".to_string()))
                    }
                }
            },
            {
                let calls = getorder_walk_calls.clone();
                let snapshot = snapshot.clone();
                move || {
                    let calls = calls.clone();
                    let snapshot = snapshot.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(snapshot)
                    }
                }
            },
        )
        .await
        .expect("event-fold chain path succeeds");

        assert_eq!(view.source, "chain");
        assert_eq!(view.last_update_id, "fold-13");
        assert_eq!(indexer_calls.load(Ordering::SeqCst), 3);
        assert_eq!(fold_calls.load(Ordering::SeqCst), 1);
        assert_eq!(getorder_walk_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            super::render_market_context(view.source, &view.last_update_id),
            "market source=chain lastUpdateId=fold-13"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn market_shows_only_executable_orders() {
        let target = wire_read_target();
        let folded_rows = [
            wire_live_order(7, 20, "0:live"),
            wire_live_order(8, 5, "0:cancelled"),
            wire_live_order(9, 6, "0:filled-or-dead"),
        ];
        let raw = super::fold_snapshot_from_orders(&target, "0:book", folded_rows.iter());
        let executable = vec![raw.orders[0].clone()];
        let snapshot = super::snapshot_with_executable_orders(raw, executable);
        let rows = super::executable_market_rows(&snapshot);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token_contract, "0:live");
        assert_eq!(rows[0].price_per_tick, 20);
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn quote_returns_best_executable_ask() {
        let target = wire_read_target();
        let asks = [
            wire_live_order(7, 30, "0:third"),
            wire_live_order(8, 10, "0:best"),
            wire_live_order(9, 20, "0:second"),
        ];
        let snapshot = super::fold_snapshot_from_orders(&target, "0:book", asks.iter());
        let quote = dexdo_core::submit_safe_single_ask_quote(&snapshot.orders, Some(2), None)
            .expect("quote executable asks");

        assert!(quote.complete);
        assert_eq!(quote.fills.len(), 1);
        assert_eq!(quote.fills[0].order_id, 8);
        assert_eq!(quote.fills[0].token_contract, "0:best");
        assert_eq!(quote.fills[0].price_per_tick, 10);
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn quote_reports_indexer_last_update_id() {
        let snapshot = wire_snapshot();
        let quote = dexdo_core::submit_safe_single_ask_quote(&snapshot.orders, Some(2), None)
            .expect("quote executable ask");
        let output = super::render_quote_summary(&snapshot, &quote, "indexer", "depth-991");

        assert!(output.contains("source=indexer"), "{output}");
        assert!(output.contains("lastUpdateId=depth-991"), "{output}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn executable_book_line_includes_selection_fields() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };
        let order = dexdo_core::OrderBookOrder {
            order_id: 7,
            owner_note: "0:seller".to_string(),
            token_contract: Some("0:tc".to_string()),
            is_buy: false,
            price_per_tick: 42,
            ticks: 1024,
            escrow: 0,
            deadline: 0,
            flags: 0,
            timestamp: 0,
        };

        let line = super::render_executable_book_line(&snapshot, &order, 8, 50);

        assert!(line.contains("executable_ask"), "{line}");
        assert!(line.contains("order_id=7"), "{line}");
        assert!(line.contains("token_contract=0:tc"), "{line}");
        assert!(line.contains("price_per_tick=42"), "{line}");
        assert!(line.contains("ticks=1024"), "{line}");
        assert!(line.contains("requested_ticks=8"), "{line}");
        assert!(line.contains("max_price_per_tick=50"), "{line}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn executable_book_output_includes_multiple_rows() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };
        let orders = vec![
            dexdo_core::OrderBookOrder {
                order_id: 7,
                owner_note: "0:seller-a".to_string(),
                token_contract: Some("0:tc-a".to_string()),
                is_buy: false,
                price_per_tick: 42,
                ticks: 1024,
                escrow: 0,
                deadline: 0,
                flags: 0,
                timestamp: 0,
            },
            dexdo_core::OrderBookOrder {
                order_id: 8,
                owner_note: "0:seller-b".to_string(),
                token_contract: Some("0:tc-b".to_string()),
                is_buy: false,
                price_per_tick: 43,
                ticks: 2048,
                escrow: 0,
                deadline: 0,
                flags: 0,
                timestamp: 0,
            },
        ];

        let output = super::render_executable_book_output(&snapshot, &orders, 8, 50, None);
        let rows = output
            .lines()
            .filter(|line| line.starts_with("executable_ask "))
            .collect::<Vec<_>>();

        assert_eq!(rows.len(), 2, "{output}");
        assert!(rows[0].contains("token_contract=0:tc-a"), "{output}");
        assert!(rows[1].contains("token_contract=0:tc-b"), "{output}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn executable_book_output_empty_is_terminal_and_clear() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };

        let output = super::render_executable_book_output(
            &snapshot,
            &[],
            8,
            10,
            Some("raw order-book matcher would hit non-executable order "),
        );

        assert!(output.contains("none=true"), "{output}");
        assert!(output.contains("no_executable_ask=true"), "{output}");
        assert!(output.contains("non-executable order "), "{output}");
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn no_executable_book_line_is_terminal_and_clear() {
        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };

        let line = super::render_no_executable_book_line(
            &snapshot,
            8,
            10,
            "no executable matching ask\nbest ask price 11 is above buyer max_price_per_tick 10",
        );

        assert!(line.contains("none=true"), "{line}");
        assert!(line.contains("no_executable_ask=true"), "{line}");
        assert!(line.contains("requested_ticks=8"), "{line}");
        assert!(line.contains("max_price_per_tick=10"), "{line}");
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("best ask price 11"), "{line}");
    }
}
