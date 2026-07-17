//! Markets list/discovery command handler(Track C11, move-only).

use crate::cli::args::MarketsArgs;
use crate::cli::commands::mock_chain_for_machine;
#[cfg(feature = "shellnet")]
use crate::cli::commands::{
    direct_chain_read_with_timeout, enforce_model_registry_policy,
    load_enabled_model_registry_policy, read_executable_book_target, target_from_market,
    BookTarget,
};
use crate::cli::machine;
#[cfg(not(feature = "shellnet"))]
use anyhow::bail;
use anyhow::Result;
#[cfg(feature = "shellnet")]
use dexdo::registry::{BuyerMissingBookPolicy, RegistryBookAction, RegistryRole};
#[cfg(feature = "shellnet")]
use dexdo_core::OrderBookSnapshot;
use dexdo_core::{model_hash_for, ChainBackend, DobParams, MockChainBackend};

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
    direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
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
    })
    .await
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_markets(args: MarketsArgs) -> Result<()> {
    if args.mock_chain {
        return run_markets_mock(args).await;
    }
    bail!("markets unavailable: build with `--features shellnet`")
}

#[cfg(test)]
mod tests {
    /// `dexdo markets` is a discovery/listing path. With buyer registry validation enabled, a
    /// registered model whose canonical book is missing is hidden from the available list instead of rendered as
    /// buyable.
    #[test]
    fn buyer_markets_hides_missing_canonical_book() {
        let source = include_str!("markets.rs");
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
}
