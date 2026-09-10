use crate::cli::args::MonitorArgs;
use crate::cli::support::{load_note_tree, print_tree_snapshot, resolve_endpoints_file};
use anyhow::{bail, Result};
use dexdo_core::{aggregate_tree, ChainBackend, DobParams, MockChainBackend, ProtocolConsts};

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
/// manifest. Live-verifiable once a deal `TokenContract` is deployed.
pub(crate) async fn run_monitor_real(args: &MonitorArgs) -> Result<()> {
    use dexdo_core::{real_market_deal_view, MarketManifest, RealChainBackend, TreeSnapshot};
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
        // Fail loud (review): the real reader returns an error for an undeployed/unreadable TC or a
        // manifest/getter mismatch -- surface it with the offending --market file, never as empty data.
        let deal = real_market_deal_view(&chain, &manifest)
            .await
            .map_err(|e| anyhow::anyhow!("--market {}: {e}", m.display()))?;
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
    print_tree_snapshot(&TreeSnapshot {
        note_ids,
        offers: Vec::new(),
        deals,
        exposure,
    });
    Ok(())
}
