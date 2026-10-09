use crate::cli::args::MonitorArgs;
use crate::cli::support::{load_note_tree, print_tree_snapshot, resolve_endpoints_file};
use anyhow::{bail, Result};
use dexdo_core::{
    aggregate_tree, ChainBackend, DealView, DobParams, MarketManifest, MockChainBackend,
    OfferListing, OrderBookOrder, ProtocolConsts, TreeSnapshot,
};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;

pub(crate) async fn run_monitor(args: MonitorArgs) -> Result<()> {
    // Real chain monitoring: a `RealNote` is a single key, not an HD tree, so the real monitor
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

/// Real-chain monitor: read the operator's `--market` manifest(s) and print each market's
/// by-fact deal state on-chain through the SAME `print_tree_snapshot` (per-model breakdown + anomaly
/// surfacing) as the mock path. Read-only -- only getters, moves nothing. Each manifest's `TokenContract` is
/// read via `real_market_deal_view` (`getState`/`getSellerBond` + the buyer pubkey); the model/price come from the
/// manifest. Own live SELL offers are read from the book's current storage, rather than bounded event history.
pub(crate) async fn run_monitor_real(args: &MonitorArgs) -> Result<()> {
    use dexdo_core::{real_market_deal_view, Address, RealChainBackend};
    if args.market.is_empty() {
        bail!(
            "a real chain monitor: pass --market <manifest>... (the operator's `dexdo provision` market \
             record(s)); a RealNote is a single key, not an HD tree, so the monitor reads the markets it is given"
        );
    }
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let contracts_path = crate::cli::commands::manifest_path()?;
    let contracts = contracts_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            contracts_path.display()
        )
    })?;
    let chain = RealChainBackend::connect(contracts)?;
    let snapshot = read_real_monitor_snapshot_with(&args.market, |manifest| {
        let chain = &chain;
        async move {
            let deal = real_market_deal_view(chain, &manifest).await?;
            let book = Address::parse(&manifest.inference_order_book)?;
            let orders = chain.inference_orderbook_live_orders(&book).await?;
            Ok((deal, orders))
        }
    })
    .await?;
    print_tree_snapshot(&snapshot);
    Ok(())
}

// Keep the snapshot assembly on the production path; tests replace only the chain reads.
async fn read_real_monitor_snapshot_with<R, Fut>(
    markets: &[PathBuf],
    mut read_market: R,
) -> Result<TreeSnapshot>
where
    R: FnMut(MarketManifest) -> Fut,
    Fut: Future<Output = Result<(DealView, Vec<OrderBookOrder>)>>,
{
    let mut offers = Vec::new();
    let mut seen_offers = BTreeSet::new();
    let mut note_ids = Vec::new();
    let mut deals = Vec::new();
    let mut exposure: u64 = 0;
    for m in markets {
        let json = std::fs::read_to_string(m)
            .map_err(|e| anyhow::anyhow!("read --market {}: {e}", m.display()))?;
        let manifest = MarketManifest::from_json(&json)
            .map_err(|e| anyhow::anyhow!("parse --market {}: {e}", m.display()))?;
        manifest
            .validate()
            .map_err(|e| anyhow::anyhow!("--market {}: {e}", m.display()))?;
        note_ids.push(manifest.seller_note.clone());
        // Fail loud (review): the real reader returns an error for an undeployed/unreadable TC or a
        // manifest/getter mismatch -- surface it with the offending --market file, never as empty data.
        let (deal, orders) = read_market(manifest.clone())
            .await
            .map_err(|e| anyhow::anyhow!("--market {}: {e}", m.display()))?;
        let as_of = crate::cli::provenance::now_unix()?;
        let seller = dexdo_core::normalize_wallet_address(&manifest.seller_note)
            .map_err(anyhow::Error::msg)?;
        for order in orders
            .iter()
            .filter(|order| order.is_live_resting_ask_at(as_of))
        {
            if dexdo_core::normalize_wallet_address(&order.owner_note)
                .map_err(anyhow::Error::msg)?
                != seller
                || !seen_offers.insert((manifest.inference_order_book.clone(), order.order_id))
            {
                continue;
            }
            offers.push(OfferListing {
                seller_id: order.owner_note.clone(),
                // The live resting-ask predicate already requires a TokenContract.
                token_contract: order.token_contract.clone().unwrap(),
                price_per_tick: u64::try_from(order.price_per_tick).unwrap_or(u64::MAX),
                max_ticks: u64::try_from(order.ticks).unwrap_or(u64::MAX),
            });
        }
        if let Some(s) = &deal.snapshot {
            if !s.closed {
                // The operator is the SELLER of their own market, so the note's at-risk SHELL is the
                // seller bond held by this TC -- NOT the buyer's deposit. This matches the mock's role-side
                // exposure and `TreeSnapshot.exposure`'s per-TC contract.
                exposure =
                    exposure.saturating_add(u64::try_from(s.seller_locked).unwrap_or(u64::MAX));
            }
        }
        deals.push(deal);
    }
    Ok(TreeSnapshot {
        note_ids,
        offers,
        deals,
        exposure,
    })
}

#[cfg(test)]
#[path = "monitor_2048_tests.rs"]
mod tests;
