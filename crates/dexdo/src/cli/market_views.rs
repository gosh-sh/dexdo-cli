//! Market-data and quote read/display command handlers (Track C5, move-only).

use crate::cli::args::*;
use crate::cli::commands::{
    book_target_for, enforce_model_registry_policy, fold_snapshot_from_orders,
    load_enabled_model_registry_policy, preload_model_registry_policy, print_book_table,
    read_book_target, read_executable_book_target, registry_requested_model,
    resolve_model_registry_target, resolve_order_book_target, retry_executable_read,
    snapshot_with_executable_orders, target_from_market, target_from_market_for_model, BookRow,
    BookTarget, ReadBudget,
};
use crate::cli::commands::{
    declared_model_flags, mock_chain_for_machine, mock_orders_from_offers, render_model_flags_field,
};
use crate::cli::indexer::{self, DepthQuery, IndexerClient, MarketsQuery};
use crate::cli::machine;
use anyhow::{bail, Result};
use dexdo::registry::{BuyerMissingBookPolicy, RegistryRole};
use dexdo_core::address as addr;
use dexdo_core::params::INDEXER_FAST_TIMEOUT;
use dexdo_core::{
    chain::BookEventFold, submit_safe_single_ask_quote, DobParams, ExecutableQuote, OrderBookOrder,
    OrderBookSnapshot,
};
use dexdo_core::{executable_quote, model_hash_for, ChainBackend};
use serde_json::json;
use std::future::Future;

/// One market's target when `--market` names the manifest: the same decision for `market`,
/// `executable-book` and any later reader, written once.

/// this stood copied out twice, character for character, comment and all, in `run_market`
/// and `run_executable_book`, so any edit to one silently split them. `run_quote` is not folded in:
/// its `--market` arm takes no `--model` at all, so it has no name to reconcile.

/// The fork is about WHO may alias the name.

/// * **Registry off** -- the manifest in hand is the whole authority, and `target_from_market_for_model`
/// refuses a mismatch OFFLINE, naming both models, instead of spending a round trip to say what
/// the file already said.
/// * **Registry on** -- the operator may legitimately paste a name the catalog resolves to this
/// market's model: `qwen--qwen3--32b` for a manifest that says `Qwen/Qwen3-32B`. The byte-compare
/// knows nothing of aliases, so it must not pre-empt the resolution that happens a moment later.
/// The cost is that a plain typo is reported by the registry rather than offline; the benefit is
/// that a correct paste is not refused.

/// `models.json` is consulted on BOTH arms: a nickname is the operator's own shorthand and must keep

#[derive(Debug)]
struct IndexerMarketContext {
    last_update_id: String,
}

#[derive(Debug)]
struct ExecutableMarketView {
    snapshot: OrderBookSnapshot,
    active: bool,
    source: &'static str,
    last_update_id: String,
    /// where the displayed ROWS came from, which is not always where `source`/`last_update_id`
    /// came from -- the freshness marker can be the indexer's while the rows are the chain's.
    rows: &'static str,
}

async fn read_indexer_market_context(order_book: &str) -> Result<IndexerMarketContext> {
    // which indexer may answer follows from the chain this run is on, not from a
    // compile-time default. On a network with no indexer this resolves to a refusal, and the caller
    // falls back to reading the book from the chain itself -- which is the right answer, not a
    // degraded one.

    // The manifest is LOADED here, as `run_market_data` loads it. It used to be built by a second
    // constructor that took the connected backend's label and set `indexer: None` -- it carried the
    // network and threw the field away. That was survivable while `resolve_base_url` fell through to
    // a compiled-in default per network; removed the default, so the throw-away became "no
    // indexer named" on EVERY chain, including the two whose manifests declare one. Nothing broke
    // loudly, because the caller catches the refusal and reads the book from the chain: the fast
    // path simply stopped existing, and `source=indexer` became unreachable outside
    // `DEXDO_INDEXER_URL`.

    // That constructor is DELETED rather than fixed. It had this one caller, and a record that can
    // be built without the field is a way for this to come back that no test can watch for.
    let manifest =
        indexer::ManifestIndexer::load(crate::cli::commands::manifest_path()?.as_path())?;
    let base_url = indexer::resolve_base_url(None, Some(&manifest))?;
    let client = IndexerClient::new(base_url, INDEXER_FAST_TIMEOUT)?;
    let markets = client
        .markets(indexer_market_address_query(order_book))
        .await?;
    if !markets.markets.iter().any(|market| {
        market
            .inference_order_book_address
            .eq_ignore_ascii_case(order_book)
    }) {
        bail!(
            "Dodex indexer has no market context for {}",
            addr::display(order_book)
        );
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

fn indexer_market_address_query(order_book: &str) -> MarketsQuery<'_> {
    MarketsQuery {
        inference_order_book_address: Some(order_book),
        ..MarketsQuery::default()
    }
}

#[cfg(test)]
#[path = "market_views_1659_tests.rs"]
mod market_views_1659_tests;

/// the undeployed-book line these views did not have. Its own file so nothing existing here
/// is edited to make it pass.
#[cfg(test)]
#[path = "market_views_1871_tests.rs"]
mod market_views_1871_tests;

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
            // the fold ANSWERED, and an empty answer is the one it cannot settle. History is
            // kept in a window, so "this book rests nothing" and "its rows are older than what I can
            // see" arrive here identically -- and this arm went on to claim `active: true` about a
            // book it had just failed to see. Storage decides. Same rule as `orders`, same function.
            let (snapshot, row_source) =
                crate::cli::fold_completeness::answer_or_storage_when_empty(
                    snapshot,
                    |snapshot| snapshot.orders.is_empty(),
                    || async {
                        retry_executable_read(
                            "storage confirmation of an empty order-book fold",
                            &mut fallback_read,
                        )
                        .await
                    },
                )
                .await?;
            let from_storage = row_source == crate::cli::fold_completeness::RowSource::Storage;
            let (source, last_update_id) = match indexer {
                Ok(context) => ("indexer", context.last_update_id),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "indexer unavailable; using chain event context");
                    ("chain", fold_id)
                }
            };
            let active = if from_storage {
                // The storage answer is the book's own state, so liveness comes from it rather than
                // from an assumption made before it was read.
                snapshot.active()
            } else {
                true
            };
            Ok(ExecutableMarketView {
                snapshot,
                active,
                source: if from_storage { "chain" } else { source },
                last_update_id: if from_storage {
                    "-".to_string()
                } else {
                    last_update_id
                },
                rows: row_source.provenance(),
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
                rows: crate::cli::provenance::ROWS_CHAIN_GETTERS,
            })
        }
    }
}

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
            // the deadline is applied HERE, before anything downstream can call these rows
            // executable. The clock is read per attempt, not once per command, so a retry cannot
            // carry a stale "still live" verdict past a deadline it crossed while retrying.
            let snapshot = fold_snapshot_from_orders(
                target,
                order_book,
                fold.live_orders_at(crate::cli::provenance::now_unix()?),
            );
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
                price_per_tick: dexdo_core::shell_amount(fill.price_per_tick),
                cost_without_fee: dexdo_core::shell_amount(cost_without_fee),
                platform_fee: dexdo_core::shell_amount(platform_fee),
                cost_with_fee: dexdo_core::shell_amount(fill.cost_with_fee),
            }
        })
        .collect::<Vec<_>>();
    let platform_fee = q.total_with_fee.saturating_sub(total_without_fee);
    Ok(machine::QuoteResponse {
        schema: machine::QUOTE_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        frame_model: frame_model.to_string(),
        model_flags: declared_model_flags(frame_model),
        model_hash: model_hash_for(frame_model),
        order_book: order_book.to_string(),
        request: machine::QuoteRequest {
            kind: if ticks.is_some() { "ticks" } else { "budget" },
            ticks: ticks.map(machine::amount),
            budget: budget.map(dexdo_core::shell_amount),
        },
        filled_ticks: machine::amount(q.filled_ticks),
        total_without_fee: dexdo_core::shell_amount(total_without_fee),
        platform_fee: dexdo_core::shell_amount(platform_fee),
        total_with_fee: dexdo_core::shell_amount(q.total_with_fee),
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
        println!(
            "quote model={frame_model}{} order_book=mock:order-book no_liquidity=true",
            render_model_flags_field(frame_model)
        );
        return Ok(());
    }
    println!(
        "quote model={}{} order_book=mock:order-book filled_ticks={} total_with_fee={} complete={}",
        frame_model,
        render_model_flags_field(frame_model),
        q.filled_ticks,
        dexdo_core::shell_amount(q.total_with_fee),
        q.complete
    );
    for fill in q.fills {
        println!("{}", quote_fill_line(&fill));
    }
    Ok(())
}

/// One line of a quote: which order it eats, how much of it, and at what price.

/// Both quote paths print it -- the one that reads the live book and the one that answers from a
/// fixture -- and a figure told in two units by two paths is the whole defect this change is about,
/// so there is one line and both call it.
pub(crate) fn quote_fill_line(fill: &dexdo_core::market::QuoteFill) -> String {
    format!(
        "fill order_id={} token_contract={} ticks={} price_per_tick={} cost_with_fee={}",
        fill.order_id,
        addr::display_self_dapp(&fill.token_contract),
        fill.ticks,
        dexdo_core::shell_amount(fill.price_per_tick),
        dexdo_core::shell_amount(fill.cost_with_fee)
    )
}

/// the rows are chosen at a clock read taken HERE, when they are rendered -- not at the shape
/// of the book, and not at the age of the snapshot behind it.

/// `resting_asks()` is deadline-blind on purpose (`chain/types.rs:159-162`): it answers "is this a
/// well-formed SELL with capacity", which is exactly the question `is_resting_ask` was reported for.
/// The snapshot reaching this function was gated at the chain read, but that read costs a book walk
/// plus a `getState` and a balance read per ask, so it can be minutes old by the time it prints --
/// and this is the single-model view a buyer reads immediately before buying. `dexdo markets`
/// already re-filters at print time for precisely this reason; the
/// single-model view did not, so an ask that lapsed during the reads was still printed as
/// executable depth under an `as_of` stamp newer than the verdict.

/// The clock is read here rather than passed in so that no future caller can render this table
/// against no clock at all; the `as_of` the context line prints is a second sample taken in the same
/// breath, which narrows the window to that statement instead of to the whole command.
fn executable_market_rows_with_clock(
    snapshot: &OrderBookSnapshot,
    now_unix: std::result::Result<u64, dexdo_core::ChainError>,
) -> Result<Vec<BookRow>> {
    let now_unix = now_unix?;
    Ok(snapshot
        .live_resting_asks_at(now_unix)
        .map(|order| BookRow {
            price_per_tick: order.price_per_tick,
            max_ticks: order.ticks,
            token_contract: order
                .token_contract
                .as_ref()
                .map(|token_contract| token_contract.to_string())
                .unwrap_or_else(|| "-".to_string()),
        })
        .collect())
}

fn executable_market_rows(snapshot: &OrderBookSnapshot) -> Result<Vec<BookRow>> {
    executable_market_rows_with_clock(snapshot, crate::cli::provenance::now_unix())
}

/// say where these rows came from and how fresh they are, so a divergence from
/// `dexdo orders list` reads as indexer lag / a different scope, not as contradictory truth.
fn render_market_context(source: &str, last_update_id: &str, as_of: u64, rows: &str) -> String {
    format!(
        "market {}",
        crate::cli::provenance::render(
            source,
            last_update_id,
            as_of,
            rows,
            crate::cli::provenance::SCOPE_EXECUTABLE_ASKS,
        )
    )
}

fn render_quote_summary(
    snapshot: &OrderBookSnapshot,
    quote: &ExecutableQuote,
    source: &str,
    last_update_id: &str,
) -> String {
    if quote.filled_ticks == 0 {
        return format!(
            "quote model={}{} order_book={} source={} lastUpdateId={} no_liquidity=true",
            snapshot.frame_model,
            render_model_flags_field(&snapshot.frame_model),
            addr::display(&snapshot.order_book),
            source,
            last_update_id
        );
    }
    format!(
        "quote model={}{} order_book={} source={} lastUpdateId={} filled_ticks={} total_with_fee={} complete={}",
        snapshot.frame_model,
        render_model_flags_field(&snapshot.frame_model),
        addr::display(&snapshot.order_book),
        source,
        last_update_id,
        quote.filled_ticks,
        dexdo_core::shell_amount(quote.total_with_fee),
        quote.complete
    )
}

/// `dexdo market`, given one canonical model name -- render THAT model's order book as the
/// human-readable box table (the same view the buyer shows before a buy). Read-only.
pub(crate) async fn run_market(args: MarketArgs) -> Result<()> {
    // read the manifest path ONCE, and open ONE read budget for the whole command. Each
    // `manifest_path()` call re-reads the environment and can fail on its own; two
    // `direct_chain_read_with_timeout` calls in a row gave each read the FULL `--read-timeout`, so
    // `--read-timeout 30` could block for 60s against the bound the operator set.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let budget = ReadBudget::new(args.read_timeout.read_timeout_secs);
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &manifest_path)?;
    let chain = dexdo_core::RealChainBackend::connect(
        manifest_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    // The book is keyed by the canonical model: derive it from `--note-addr` (any active note supplies the
    // book code), or read it from a provision manifest. `market.json` is the seller's artifact -- a buyer
    // normally passes only the model name + its own `--note-addr`.
    let (requested_model, target) = if let Some(market) = args.market.as_deref() {
        if args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --note-addr");
        }
        {
            // The typed name goes on to the registry, not the market's own model: see
            // `target_from_market_for_model`, which is where the whole decision lives.
            let (target, requested) = target_from_market_for_model(
                market,
                &args.models,
                &args.model,
                registry_policy.is_some(),
            )?;
            (requested, target)
        }
    } else {
        // ONE ARM, NOT TWO. This forked on `registry_policy.is_some()`: the registry when a
        // config file switched it on, a MANDATORY `models.json` otherwise. So the catalog decided
        // the default and the chain decided only on request -- an authority behind an optional

        // no catalog could not ask what a registered market was.
        let requested_model = budget
            .read(registry_requested_model(
                &manifest_path,
                None,
                &args.models,
                &args.model,
            ))
            .await?;
        let target = book_target_for(requested_model.clone(), args.note_addr.clone());
        (requested_model, target)
    };
    let view = budget
        .read(async {
            preload_model_registry_policy(
                RegistryRole::Buyer,
                registry_policy.as_ref(),
                &manifest_path,
            )
            .await?;
            let target = resolve_model_registry_target(
                RegistryRole::Buyer,
                registry_policy.as_ref(),
                &manifest_path,
                &requested_model,
                target,
            )
            .await?;
            let order_book = resolve_order_book_target(&chain, &target).await?;
            let view = read_executable_market_view(&chain, &target, &order_book).await?;
            if let Some(policy) = registry_policy.as_ref() {
                enforce_model_registry_policy(
                    RegistryRole::Buyer,
                    policy,
                    &manifest_path,
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
    if !view.active {
        eprint_book_not_deployed(&snapshot.frame_model, &snapshot.order_book);
    }
    let rows = executable_market_rows(snapshot)?;
    println!(
        "{}",
        render_market_context(
            view.source,
            &view.last_update_id,
            crate::cli::provenance::now_unix()?,
            view.rows,
        )
    );
    if rows.is_empty() {
        let raw_order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
        if raw_order_count > 0 {
            let tick_size = DobParams::canonical().tick_size;
            println!(
                "inference order book -- {}{}  (1 tick = {tick_size} model tokens)",
                snapshot.frame_model,
                render_model_flags_field(&snapshot.frame_model)
            );
            println!(
                "  * no executable asks; raw order_count={raw_order_count} is blocked by stale/non-executable rows"
            );
            return Ok(());
        }
    }
    // Read-only discovery: no `--max-price-per-tick` ceiling, so the `exec` column stays blank (this is not a buy).
    print_book_table(&snapshot.frame_model, &rows, None, None);
    Ok(())
}

/// Is this failure a state of the book (which `executable-book` reports as an empty listing plus a
/// reason), or a failure to read it at all (which stays an error)?

/// the shared classifier answers it. A hand-kept list of phrases here folded the states
/// deliberately separated back into one bucket, and a new reason string could silently land in the
/// wrong one.
fn selection_error_is_empty_book_state(reason: &str) -> bool {
    dexdo_core::params::book_refusal_class(reason).is_some()
}

fn render_executable_book_line(
    snapshot: &OrderBookSnapshot,
    order: &OrderBookOrder,
    ticks: u128,
    max_price_per_tick: u128,
) -> String {
    format!(
        "executable_ask model={}{} order_book={} order_id={} token_contract={} price_per_tick={} ticks={} requested_ticks={} max_price_per_tick={}",
        snapshot.frame_model,
        render_model_flags_field(&snapshot.frame_model),
        addr::display(&snapshot.order_book),
        order.order_id,
        addr::display_self_dapp_opt(order.token_contract.as_deref(), "-"),
        dexdo_core::shell_amount(order.price_per_tick),
        order.ticks,
        ticks,
        dexdo_core::shell_amount(max_price_per_tick)
    )
}

/// Name the state this book is in, not a blanket "nothing matched".

/// the class comes from `buy_refusal_class` -- the same function the buy preflight stamps its
/// own refusal with -- so the listing and the buyer cannot describe one book at one ceiling with two
/// different answers. `no_executable_ask` is now one of the four possible flags rather than the flag
/// every empty result carries: an empty book prints `empty_model_book=true`, a short head prints
/// `insufficient_head_ask=true`, an all-lapsed book prints `expired_counterparty_ask=true`, and only
/// "rows exist, none of them usable" keeps `no_executable_ask=true`.
/// The book this name derives is not on chain, said out loud.

/// **Why it exists.** `run_executable_book` already says this: an empty result there carries an
/// `empty_reason` and renders `none=true <class>=true reason=...`. `run_market` and `run_quote` had no
/// such mechanism -- they read `active` only to feed the policy-gated enforcement, and on an
/// undeployed book printed the context line and an empty table. An empty table is a statement that
/// the market is empty, and that is a different fact from the market not existing.

/// **Why it lands with this change rather than after it.** Until now an unknown name was stopped
/// earlier by the mandatory catalog (`model "x" not found in the config`), so the silent branch was
/// reachable only for a catalogued name whose book was undeployed. Removing that requirement lets
/// any unresolvable name reach it, so the same change that widens the path has to close it.

/// **STDERR, not stdout.** `run_quote --json` writes a machine document to stdout and
/// `machine_contract_covers_readers_1641` reads it; a human line there would corrupt it. This is the
/// same split `run_markets_address` uses -- the answer on stdout, what it means on stderr.
/// Rendered rather than printed, so the sentence is asserted without a chain or a terminal -- the
/// shape `render_no_executable_book_line` beside it already uses.
pub(crate) fn render_book_not_deployed(frame_model: &str, order_book: &str) -> String {
    format!(
        "book_not_deployed=true model={frame_model} order_book={} -- nothing has been listed under \
         this name, so there is no market here rather than an empty one. Check the spelling with \
         `dexdo markets address --model {frame_model}`, which names the book the ModelRegistry \
         derives for it",
        addr::display(order_book)
    )
}

fn eprint_book_not_deployed(frame_model: &str, order_book: &str) {
    eprintln!("{}", render_book_not_deployed(frame_model, order_book));
}

fn render_no_executable_book_line(
    snapshot: &OrderBookSnapshot,
    ticks: u128,
    max_price_per_tick: u128,
    reason: &str,
) -> String {
    format!(
        "executable_ask model={}{} order_book={} none=true {}=true requested_ticks={} max_price_per_tick={} reason={}",
        snapshot.frame_model,
        render_model_flags_field(&snapshot.frame_model),
        addr::display(&snapshot.order_book),
        dexdo_core::params::buy_refusal_class(reason),
        ticks,
        dexdo_core::shell_amount(max_price_per_tick),
        reason.replace('\n', " ")
    )
}

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

/// `dexdo executable-book`, given one model: show all currently executable asks for this tick count and ceiling.
/// Rows hidden behind a stale cheaper raw row are intentionally not listed, because the model-wide matcher
/// would hit that unsafe row first.
pub(crate) async fn run_executable_book(args: ExecutableBookArgs) -> Result<()> {
    // read the manifest path ONCE, and open ONE read budget for the whole command. Each
    // `manifest_path()` call re-reads the environment and can fail on its own; two
    // `direct_chain_read_with_timeout` calls in a row gave each read the FULL `--read-timeout`, so
    // `--read-timeout 30` could block for 60s against the bound the operator set.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let budget = ReadBudget::new(args.read_timeout.read_timeout_secs);
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &manifest_path)?;
    let chain = dexdo_core::RealChainBackend::connect(
        manifest_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    let (requested_model, target) = if let Some(market) = args.market.as_deref() {
        if args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --note-addr");
        }
        {
            // The typed name goes on to the registry, not the market's own model: see
            // `target_from_market_for_model`, which is where the whole decision lives.
            let (target, requested) = target_from_market_for_model(
                market,
                &args.models,
                &args.model,
                registry_policy.is_some(),
            )?;
            (requested, target)
        }
    } else {
        // One arm, not two -- see `run_market` above for why the fork was wrong.
        let requested_model = budget
            .read(registry_requested_model(
                &manifest_path,
                None,
                &args.models,
                &args.model,
            ))
            .await?;
        let target = book_target_for(requested_model.clone(), args.note_addr.clone());
        (requested_model, target)
    };
    let (snapshot, orders, empty_reason) = budget
        .read(async {
            preload_model_registry_policy(
                RegistryRole::Buyer,
                registry_policy.as_ref(),
                &manifest_path,
            )
            .await?;
            let target = resolve_model_registry_target(
                RegistryRole::Buyer,
                registry_policy.as_ref(),
                &manifest_path,
                &requested_model,
                target,
            )
            .await?;
            let snapshot = read_book_target(&chain, &target).await?;
            if let Some(policy) = registry_policy.as_ref() {
                enforce_model_registry_policy(
                    RegistryRole::Buyer,
                    policy,
                    &manifest_path,
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

pub(crate) async fn run_quote(args: QuoteArgs) -> Result<()> {
    if args.mock_chain {
        return run_quote_mock(args).await;
    }
    if args.ticks.is_some() == args.budget.is_some() {
        bail!("quote requires exactly one of --ticks or --budget");
    }
    // read the manifest path ONCE, and open ONE read budget for the whole command. Each
    // `manifest_path()` call re-reads the environment and can fail on its own; two
    // `direct_chain_read_with_timeout` calls in a row gave each read the FULL `--read-timeout`, so
    // `--read-timeout 30` could block for 60s against the bound the operator set.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let budget = ReadBudget::new(args.read_timeout.read_timeout_secs);
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &manifest_path)?;
    let chain = dexdo_core::RealChainBackend::connect(
        manifest_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    let (requested_model, target) = if let Some(market) = args.market.as_deref() {
        if args.model.is_some() || args.note_addr.is_some() {
            bail!("--market is mutually exclusive with --model/--note-addr for quote");
        }
        let target = target_from_market(market)?;
        (target.frame_model.clone(), target)
    } else {
        // One arm, not two -- see `run_market` above for why the fork was wrong.
        let model = args
            .model
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("quote without --market requires --model"))?;
        let requested_model = budget
            .read(registry_requested_model(
                &manifest_path,
                None,
                &args.models,
                model,
            ))
            .await?;
        let target = book_target_for(requested_model.clone(), args.note_addr.clone());
        (requested_model, target)
    };
    let (view, q) = budget
        .read(async {
            preload_model_registry_policy(
                RegistryRole::Buyer,
                registry_policy.as_ref(),
                &manifest_path,
            )
            .await?;
            let target = resolve_model_registry_target(
                RegistryRole::Buyer,
                registry_policy.as_ref(),
                &manifest_path,
                &requested_model,
                target,
            )
            .await?;
            let order_book = resolve_order_book_target(&chain, &target).await?;
            let view = read_executable_market_view(&chain, &target, &order_book).await?;
            if let Some(policy) = registry_policy.as_ref() {
                enforce_model_registry_policy(
                    RegistryRole::Buyer,
                    policy,
                    &manifest_path,
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
    if !view.active {
        eprint_book_not_deployed(&snapshot.frame_model, &snapshot.order_book);
    }
    if args.json {
        let response = quote_response_from_quote(
            chain.network(),
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
        println!("{}", quote_fill_line(&fill));
    }
    Ok(())
}

pub(crate) async fn run_market_data(args: MarketDataArgs) -> Result<()> {
    // Always loaded, never optional: the manifest is what says which indexer may answer, and
    // there is exactly one of it. It used to be `--contracts` and therefore absent by
    // default, which made "no indexer named" and "no manifest given" the same silence.
    let manifest = Some(indexer::ManifestIndexer::load(
        crate::cli::commands::manifest_path()?.as_path(),
    )?);
    let base_url = indexer::resolve_base_url(args.indexer_url.as_deref(), manifest.as_ref())?;
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
                        indexer::render_depth_output(
                            &response,
                            client.base_url(),
                            crate::cli::provenance::now_unix()?,
                        )
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
#[path = "clock_sell_liveness_1042.rs"]
mod clock_sell_liveness_1042;

#[cfg(test)]
mod tests {
    #[test]
    fn registry_getters_use_existing_read_timeout_scope() {
        let source = include_str!("market_views.rs");
        for function in [
            "pub(crate) async fn run_market(args: MarketArgs)",
            "pub(crate) async fn run_executable_book(args: ExecutableBookArgs)",
            "pub(crate) async fn run_quote(args: QuoteArgs)",
        ] {
            // `code_of`, not the next `#[cfg(`: removing the cargo features deleted every one of
            // those stubs, and a guard anchored on a neighbour reports a missing anchor as a
            // missing call. It also silently took the whole rest of the file as "the body", and
            // comments survived it, so a commented-out call still read as a call.
            let body = crate::cli::source_probe::code_of(source, function);

            // CONTAINMENT, not order. The first version of this compared the offset of
            // `ReadBudget::new(` with the offset of the getter -- and `ReadBudget::new` is the
            // second statement of every one of these functions, so no edit to the getter could
            // ever flip it. It was green while the same function made four reads with no bound at
            // all. What has to be true is that the read goes THROUGH the budget.
            assert!(
                body.contains(".read(registry_requested_model("),
                "{function}: the registry getter must be read through the command's budget, not \
                 on its own"
            );
            // The whole PRODUCTION half of the file, not just this body: moving the unbudgeted
            // read into a helper called from here would put it outside the body and leave the
            // guard green while `--read-timeout 30` blocked for 60s again. A helper in ANOTHER
            // file still escapes this, and that is named in the PR rather than pretended away.
            let production = source
                .split_once("#[cfg(test)]\nmod tests")
                .map_or(source, |(before, _)| before);
            assert!(
                !production.contains("direct_chain_read_with_timeout("),
                "{function}: a second full `--read-timeout` is back. That helper bounds ONE read, \
                 so every call it wraps is another whole budget: two of them and `--read-timeout \
                 30` blocks for 60s"
            );
        }
    }

    fn wire_read_target() -> super::BookTarget {
        super::BookTarget {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "model-hash".to_string(),
            order_book: Some("0:book".to_string()),
            root_model: None,
            note_addr: None,
        }
    }

    fn wire_live_order(
        order_id: u128,
        price: u128,
        token_contract: &str,
    ) -> dexdo_core::chain::LiveBookOrder {
        dexdo_core::chain::LiveBookOrder {
            order_id,
            is_buy: false,
            price,
            ticks_remaining: 8,
            note: "0:seller".to_string(),
            token_contract: token_contract.to_string(),
            deadline: 1_900_000_000,
            flags: 0,
            expired_by_event: false,
        }
    }

    fn wire_snapshot() -> dexdo_core::OrderBookSnapshot {
        let target = wire_read_target();
        let orders = [wire_live_order(7, 20, "0:live")];
        super::fold_snapshot_from_orders(&target, "0:book", orders.iter())
    }

    #[test]
    fn indexer_market_address_lookup_omits_list_limit() {
        let query = super::indexer_market_address_query("0:book");

        assert_eq!(query.inference_order_book_address, Some("0:book"));
        assert_eq!(query.limit, None);
    }

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
        // the rows are the chain's even when the freshness marker is the indexer's -- the
        // annotation must say both, or an indexer lag reads as a contradiction against `orders`.
        assert_eq!(view.rows, crate::cli::provenance::ROWS_CHAIN_EVENTS);
        assert_eq!(
            super::render_market_context(
                view.source,
                &view.last_update_id,
                1_754_006_400,
                view.rows
            ),
            "market source=indexer lastUpdateId=indexer-77 as_of=1754006400 \
             rows=chain:order-book-events scope=executable-asks"
        );
    }

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
        assert_eq!(view.rows, crate::cli::provenance::ROWS_CHAIN_EVENTS);
        assert_eq!(
            super::render_market_context(
                view.source,
                &view.last_update_id,
                1_754_006_400,
                view.rows
            ),
            "market source=chain lastUpdateId=fold-13 as_of=1754006400 \
             rows=chain:order-book-events scope=executable-asks"
        );
    }

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
        let rows = super::executable_market_rows(&snapshot).expect("render market rows");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token_contract, "0:live");
        assert_eq!(rows[0].price_per_tick, 20);
    }

    /// at the render function the issue names.

    /// Both rows below are well-formed resting SELLs with capacity, which is everything
    /// `is_resting_ask` looks at -- so the deadline-blind `resting_asks()` returned both, and that is
    /// how an ask which had been expired for hours was printed as this market's only executable
    /// depth. The clock that retires the lapsed row is read when the row is RENDERED, not when the
    /// book was read, because the reads in between cost a book walk plus a `getState` and a balance
    /// read per ask.

    /// The live row is the half that makes the assertion mean something: a build that printed no
    /// depth at all would pass a lapsed-row-only test.

    /// The last assertion is the dispatch half. A correct row builder that `run_market` does not
    /// call is the failure this repo keeps shipping, and `run_market` has exactly one row source.
    #[test]
    fn market_rows_retire_an_ask_that_lapsed_before_it_was_rendered() {
        let target = wire_read_target();
        let live = wire_live_order(7, 20, "0:live");
        let mut lapsed = wire_live_order(8, 5, "0:lapsed");
        // A real second in the past, not the malformed zero deadline: this is expiry, not a
        // rejected shape.
        lapsed.deadline = 1;
        let snapshot = super::fold_snapshot_from_orders(&target, "0:book", [&live, &lapsed]);

        let rows = super::executable_market_rows(&snapshot).expect("render market rows");
        let rendered = rows
            .iter()
            .map(|row| row.token_contract.as_str())
            .collect::<Vec<_>>()
            .join(",");

        assert_eq!(rendered, "0:live", "rendered depth was [{rendered}]");

        // `code_of`: the `#[cfg(` anchor this used was a neighbouring stub -- deletable by work
        // that has nothing to do with this guard, and `unwrap_or(body.len())` made its absence
        // silent by calling the whole rest of the file "the body". Comments go too, so a
        // commented-out call does not read as a call.
        let body = crate::cli::source_probe::code_of(
            include_str!("market_views.rs"),
            "pub(crate) async fn run_market(args: MarketArgs)",
        );
        assert!(
            body.contains("executable_market_rows(snapshot)"),
            "dexdo market must build its table through the gated row builder"
        );
    }

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

    #[test]
    fn quote_reports_indexer_last_update_id() {
        let snapshot = wire_snapshot();
        let quote = dexdo_core::submit_safe_single_ask_quote(&snapshot.orders, Some(2), None)
            .expect("quote executable ask");
        let output = super::render_quote_summary(&snapshot, &quote, "indexer", "depth-991");

        assert!(output.contains("source=indexer"), "{output}");
        assert!(output.contains("lastUpdateId=depth-991"), "{output}");
    }

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
            // 42 SHELL a tick, in the raw units the book row carries.
            price_per_tick: 42 * dexdo_core::PRICE_STEP,
            ticks: 1024,
            escrow: 0,
            deadline: 0,
            flags: 0,
            timestamp: 0,
        };

        let line =
            super::render_executable_book_line(&snapshot, &order, 8, 50 * dexdo_core::PRICE_STEP);

        assert!(line.contains("executable_ask"), "{line}");
        assert!(line.contains("order_id=7"), "{line}");
        assert!(line.contains("token_contract=0:tc"), "{line}");
        assert!(line.contains("price_per_tick=42"), "{line}");
        assert!(line.contains("ticks=1024"), "{line}");
        assert!(line.contains("requested_ticks=8"), "{line}");
        assert!(line.contains("max_price_per_tick=50"), "{line}");
    }

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
            10 * dexdo_core::PRICE_STEP,
            "no executable matching ask\nbest ask price 11 is above buyer max_price_per_tick 10",
        );

        assert!(line.contains("none=true"), "{line}");
        assert!(line.contains("no_executable_ask=true"), "{line}");
        assert!(line.contains("requested_ticks=8"), "{line}");
        assert!(line.contains("max_price_per_tick=10"), "{line}");
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("best ask price 11"), "{line}");
    }

    /// the line names the state the buy preflight names, for each of the four classes.

    /// Observed live on the 4.0.35 acceptance campaign: `executable-book` printed
    /// `none=true no_executable_ask=true` for a book the buyer refused, one ceiling and seconds
    /// later, as `empty_model_book`. The class here is read from `buy_refusal_class` -- the same
    /// function that stamps the buyer's refusal -- so this asserts AGREEMENT rather than the presence
    /// of one string: exactly one class flag is set, and it is the one the buyer would report.
    #[test]
    fn no_executable_book_line_names_the_class_the_buyer_reports() {
        use dexdo_core::params;

        let snapshot = dexdo_core::OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: Vec::new(),
        };
        let all_classes = [
            params::EXPIRED_COUNTERPARTY_ASK_CLASS,
            params::INSUFFICIENT_HEAD_ASK_CLASS,
            params::EMPTY_MODEL_BOOK_CLASS,
            params::NO_EXECUTABLE_ASK_CLASS,
        ];
        // Each refusal is spelled with the literal its producer leads with, never a hand-copied
        // sentence, so a reworded message moves this test with it instead of leaving it green.
        let cases = [
            (
                format!(
                    "{} 1785678525: every ask crossing this buy is out of the book by its own deadline",
                    params::EXPIRED_COUNTERPARTY_ASK_REASON
                ),
                params::EXPIRED_COUNTERPARTY_ASK_CLASS,
            ),
            (
                format!(
                    "{} for max_price_per_tick 10, requested ticks 8: 1 resting ask(s) are past their deadline",
                    params::LAPSED_MODEL_BOOK_REASON
                ),
                params::EXPIRED_COUNTERPARTY_ASK_CLASS,
            ),
            (
                format!(
                    "{}: {} order  tokenContract 0:tc has only 1 ticks, buyer requested 8",
                    params::RAW_MATCHER_NO_SUBMIT_SAFE_ASK,
                    params::INSUFFICIENT_HEAD_ASK_REASON
                ),
                params::INSUFFICIENT_HEAD_ASK_CLASS,
            ),
            (
                format!(
                    "{}: {} for max_price_per_tick 10, requested ticks 8",
                    params::RAW_MATCHER_NO_SUBMIT_SAFE_ASK,
                    params::EMPTY_MODEL_BOOK_REASON
                ),
                params::EMPTY_MODEL_BOOK_CLASS,
            ),
            (
                "no executable matching ask for max_price_per_tick 10, requested ticks 8".to_string(),
                params::NO_EXECUTABLE_ASK_CLASS,
            ),
        ];

        for (reason, expected) in cases {
            let line = super::render_no_executable_book_line(&snapshot, 8, 10, &reason);

            assert_eq!(
                params::buy_refusal_class(&reason),
                expected,
                "the buy preflight would not report {expected} for {reason}"
            );
            assert!(line.contains("none=true"), "{line}");
            assert!(
                line.contains(&format!("{expected}=true")),
                "executable-book must name {expected}, the class the buyer reports: {line}"
            );
            for other in all_classes.iter().filter(|class| **class != expected) {
                assert!(
                    !line.contains(&format!("{other}=true")),
                    "executable-book named {other} as well as {expected}: {line}"
                );
            }
        }
    }

    /// The predicate that decides "the book refused this buy" vs "the read failed" is the shared
    /// classifier, so every state the buyer classifies is still folded into an empty listing with a
    /// reason, and a genuine read failure still surfaces as an error.
    #[test]
    fn selection_error_is_empty_book_state_follows_the_shared_classifier() {
        use dexdo_core::params;

        for reason in [
            format!(
                "{} 1785678525: nearest ask is gone",
                params::EXPIRED_COUNTERPARTY_ASK_REASON
            ),
            format!(
                "{} order  has only 1 ticks",
                params::INSUFFICIENT_HEAD_ASK_REASON
            ),
            format!(
                "{}: {} for max_price_per_tick 10",
                params::RAW_MATCHER_NO_SUBMIT_SAFE_ASK,
                params::EMPTY_MODEL_BOOK_REASON
            ),
            format!(
                "{} for max_price_per_tick 10",
                params::LAPSED_MODEL_BOOK_REASON
            ),
            "no executable matching ask for max_price_per_tick 10".to_string(),
            "best ask price 11 is above buyer max_price_per_tick 10".to_string(),
            "no matchable ask for max_price_per_tick 10".to_string(),
            "raw order-book matcher would select order ".to_string(),
        ] {
            assert!(
                super::selection_error_is_empty_book_state(&reason),
                "{reason}"
            );
        }

        for reason in [
            &format!(
                "{}: GraphQL request failed: 502 Bad Gateway",
                dexdo_core::params::current_network()
            ),
            "InferenceOrderBook 0:book is not active",
            "DEXDO_MANIFEST: non-printable path",
        ] {
            assert!(
                !super::selection_error_is_empty_book_state(reason),
                "{reason}"
            );
        }
    }
}
