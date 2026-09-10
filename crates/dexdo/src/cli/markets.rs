//! Markets list/discovery command handler (Track C11, move-only).

use crate::cli::args::MarketsAddressArgs;
use crate::cli::args::{MarketsArgs, MarketsCommand};
use crate::cli::commands::{
    declared_model_flags, mock_chain_for_machine, render_model_flags_field,
};
use crate::cli::commands::{
    direct_chain_read_with_timeout, enforce_model_registry_policy,
    load_enabled_model_registry_policy, preload_model_registry_policy, read_executable_book_target,
    resolve_model_registry_target, resolve_registry_content_identity, target_from_market,
    BookTarget,
};
use crate::cli::machine;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use dexdo::registry::{
    BuyerMissingBookPolicy, RegistryBookAction, RegistryRole, RegistrySuggestions,
};
use dexdo_core::address as addr;
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
        model_flags: declared_model_flags(frame_model),
        model_hash: model_hash_for(frame_model),
        order_book: "mock:order-book".to_string(),
        root_model: Some("mock:root-model".to_string()),
        active: true,
        order_count: offers.len() as u128,
        ask_count: offers.len() as u128,
        depth_ticks: machine::amount(depth_ticks),
        best_ask: best_ask.map(dexdo_core::shell_amount),
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
        "model={}{} order_book={} active={} order_count={} ask_count={} depth_ticks={} best_ask={}",
        entry.frame_model,
        render_model_flags_field(&entry.frame_model),
        addr::display(&entry.order_book),
        entry.active,
        entry.order_count,
        entry.ask_count,
        entry.depth_ticks,
        entry.best_ask.as_deref().unwrap_or("-")
    );
    Ok(())
}

/// the totals a buyer reads off `dexdo markets` are a liquidity claim, so they count only asks
/// that are still reachable at `now_unix` -- the deadline-blind `resting_asks()` shape filter is not
/// enough here.

/// The clock belongs to the caller, sampled AFTER the chain reads rather than once per command. Each
/// target in `run_markets` costs its own book read plus a registry getter, so the snapshot that
/// produced these rows can be minutes old by the time they print; a deadline crossed in between is
/// exactly the incident, where 956 ticks were advertised at a price no buy could obtain.

/// `order_count` stays the book's own `getStats` number and is deliberately NOT filtered: it is the
/// contract's count of everything the book holds, not a claim about what a buyer can reach.
fn market_entry_from_snapshot(
    snapshot: &OrderBookSnapshot,
    root_model: Option<String>,
    source: &str,
    now_unix: u64,
) -> machine::MarketEntry {
    let depth_ticks: u128 = snapshot
        .live_resting_asks_at(now_unix)
        .map(|o| o.ticks)
        .sum();
    let best_ask = snapshot
        .live_resting_asks_at(now_unix)
        .map(|o| o.price_per_tick)
        .min();
    let order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
    machine::MarketEntry {
        frame_model: snapshot.frame_model.clone(),
        model_flags: declared_model_flags(&snapshot.frame_model),
        model_hash: snapshot.model_hash.clone(),
        order_book: snapshot.order_book.clone(),
        root_model,
        active: snapshot.active(),
        order_count,
        ask_count: snapshot.live_resting_asks_at(now_unix).count() as u128,
        depth_ticks: machine::amount(depth_ticks),
        best_ask: best_ask.map(dexdo_core::shell_amount),
        min_liquidity: machine::amount(0u8),
        tick_size: machine::amount(DobParams::canonical().tick_size),
        source: source.to_string(),
    }
}

pub(crate) async fn run_markets(mut args: MarketsArgs) -> Result<()> {
    if let Some(MarketsCommand::Address(address)) = args.command.take() {
        return run_markets_address(address).await;
    }
    if args.mock_chain {
        return run_markets_mock(args).await;
    }
    // read ONCE -- but BELOW the two early returns, not above them. Each call re-reads the
    // environment and can fail on its own, so hoisting it to the top of the function would make
    // `--mock-chain` and the `address` subcommand refuse without `DEXDO_MANIFEST`, which neither of
    // them needs: one never touches a chain and the other reads the manifest itself.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Buyer, &args.registry, &manifest_path)?;
    let chain = dexdo_core::RealChainBackend::connect(
        manifest_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    let targets = if args.market.is_empty() {
        let note_addr = args.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "markets without --market requires --note-addr to derive order-book addresses"
            )
        })?;
        // THE CATALOG IS THE RIGHT SOURCE HERE, and it is the only one of these read paths where
        // that is true. The other views are given a NAME and can ask the registry what it
        // means; this form is given nothing and answers "the markets of the models I serve", which
        // is a question only a local catalog can answer -- the registry holds ten thousand names
        // and none of them is "mine".

        // So the file stays required, and what changes is the refusal. `ModelsConfig::load` says
        // `read models config models.json: No such file or directory`: true, and useless to someone
        // who never had that file and did not know this command wanted one. It names neither what
        // the command was trying to do nor any way forward.
        if !args.models.exists() {
            bail!(
                "`markets` without --market lists the books of the models in your catalog, and \
                 there is no catalog at {}. Nothing was read.\n\n    \
                 * one model, no catalog needed:  dexdo markets address --model '<name>'\n    \
                 * a market you provisioned:      dexdo markets --market '<market.json>'\n    \
                 * your own catalog:              write {} with the models you serve\n\n\
                 The names the chain carries are the ModelRegistry's; `markets address` resolves \
                 one without any local file.",
                args.models.display(),
                args.models.display()
            );
        }
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
        preload_model_registry_policy(
            RegistryRole::Buyer,
            registry_policy.as_ref(),
            &manifest_path,
        )
        .await?;
        let mut resolved_targets = Vec::with_capacity(targets.len());
        for target in targets {
            let requested_model = target.frame_model.clone();
            resolved_targets.push(
                resolve_model_registry_target(
                    RegistryRole::Buyer,
                    registry_policy.as_ref(),
                    &manifest_path,
                    &requested_model,
                    target,
                )
                .await?,
            );
        }
        let mut available = Vec::with_capacity(resolved_targets.len());
        for target in resolved_targets {
            let source = if target.order_book.is_some() {
                "market_manifest"
            } else {
                "models_config"
            };
            let root_model = target.root_model.clone();
            let snapshot = read_executable_book_target(&chain, &target).await?;
            if let Some(policy) = registry_policy.as_ref() {
                let action = enforce_model_registry_policy(
                    RegistryRole::Buyer,
                    policy,
                    &manifest_path,
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
            available.push((snapshot, root_model, source));
        }
        // one clock, sampled here -- after every book read and registry getter above, and once
        // for the whole listing so two markets in the same output cannot disagree about what "now" is.
        let as_of = crate::cli::provenance::now_unix()?;
        if args.json {
            let markets = available
                .iter()
                .map(|(snapshot, root_model, source)| {
                    market_entry_from_snapshot(snapshot, root_model.clone(), source, as_of)
                })
                .collect();
            return machine::print_json(&machine::MarketsResponse {
                schema: machine::MARKETS_SCHEMA,
                network: chain.network().to_string(),
                generated_at_unix: machine::now_unix()?,
                markets,
            });
        }
        for (snapshot, _, _) in available {
            let depth_ticks: u128 = snapshot.live_resting_asks_at(as_of).map(|o| o.ticks).sum();
            let best_ask = snapshot
                .live_resting_asks_at(as_of)
                .map(|o| o.price_per_tick)
                .min();
            let order_count = snapshot.stats.as_ref().map(|s| s.order_count).unwrap_or(0);
            println!(
                "model={}{} order_book={} active={} order_count={} ask_count={} depth_ticks={} best_ask={}",
                snapshot.frame_model,
                render_model_flags_field(&snapshot.frame_model),
                addr::display(&snapshot.order_book),
                snapshot.active(),
                order_count,
                snapshot.live_resting_asks_at(as_of).count(),
                depth_ticks,
                best_ask
                    .map(dexdo_core::shell_amount)
                    .unwrap_or_else(|| "-".to_string())
            );
        }
        Ok(())
    })
    .await
}

/// name one model's canonical order-book address, without a note and without spending.

/// Every step is a path this client already runs; nothing here derives an address a second way.

/// * `resolve_registry_content_identity` asks the on-chain `ModelRegistry` which spelling of the
/// name it actually holds. It is the same lookup, the same candidate order and the same refusal
/// text the buyer's content-identity preflight produces, so a name refused here is a name a buy
/// would refuse later. The registry account is downloaded once and every getter then runs locally
/// against that snapshot: nothing is signed, nothing is submitted, no note is read.
/// * `canonical_inference_orderbook_address` derives the address OFFLINE from the
/// `InferenceOrderBook` code image this binary carries and the model hash. That is the same
/// arithmetic `ModelRegistry._orderBook` performs on chain -- `abi.stateInitHash` over the pinned
/// code hash and depth -- so the derived address IS the registry's answer, not a second opinion
/// about it.

/// The listing form of `markets` gets the same address by running `getInferenceOrderBookAddress` on
/// a NOTE, passing that same carried code image in as an argument. The note contributes nothing to
/// the result; it only has to be alive to execute the getter. That is the cost reported, and
/// the reason this question is asked on its own.

/// The candidate walk is why the requested spelling is reported next to the registry's own: 4.0.36
/// seeded the catalogue WITHOUT producers, so `openai/gpt-5.2-pro` is answered about
/// `gpt-5.2-pro`, and the model hash -- and with it the address -- belongs to the name the registry
/// holds, not to the one that was typed.
pub(crate) async fn run_markets_address(args: MarketsAddressArgs) -> Result<()> {
    // The manifest names the network without contacting it; `Deployed::load` is a file read.
    let manifest = crate::cli::commands::manifest_path()?;
    let network = dexdo_core::Deployed::load(&manifest)
        .with_context(|| {
            format!(
                "read the deployed-contracts manifest {}",
                manifest.display()
            )
        })?
        .network;
    // step 6: a direct chain read ends inside the configured bound, never hangs.
    let registry_model =
        direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
            // Compute: this command's whole output is the answer, so its refusal is read by someone
            // who has been stopped -- and the warning paths now name THIS command as where the list
            // comes from, which would be a lie if it did not produce one.
            resolve_registry_content_identity(
                RegistryRole::Buyer,
                &manifest,
                None,
                &args.model,
                RegistrySuggestions::Compute,
            )
            .await
        })
        .await?;

    let model_hash = model_hash_for(&registry_model);
    let order_book = addr::display(
        &dexdo_core::RealChainBackend::canonical_inference_orderbook_address(&model_hash)?
            .with_workchain(),
    );

    if args.json {
        return machine::print_json(&machine::MarketsAddressResponse {
            schema: machine::MARKETS_ADDRESS_SCHEMA,
            network,
            requested_model: args.model.clone(),
            registry_model,
            model_hash,
            order_book,
        });
    }
    // stdout carries the ADDRESS and nothing else, so the answer substitutes straight into
    // the next command. Everything explanatory goes to stderr, which a pipe does not carry: a
    // reading command whose output cannot be piped is half a command. `--json` above is the other
    // consumer shape, and there the whole document IS the answer.
    eprintln!(
        "model={registry_model} requested={} model_hash={model_hash}",
        args.model
    );
    println!("{order_book}");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// one bounded read per command here, and a guard so it stays that way.

    /// `markets.rs` was checked for the read multiplier and correctly left alone: both commands
    /// wrap everything in ONE `direct_chain_read_with_timeout`, so there is no second budget to
    /// share. But `buyer.rs` and `market_views.rs` carry a guard forbidding a second call and this
    /// file did not, which means the day someone adds one the doubling comes back in silence. The
    /// class is only closed when the file that has nothing to fix is guarded too.
    #[test]
    fn each_command_here_makes_at_most_one_bounded_read() {
        let source = include_str!("markets.rs");
        let production = source
            .split_once("#[cfg(test)]\nmod tests")
            .map_or(source, |(before, _)| before);
        let calls = production
            .matches("direct_chain_read_with_timeout(")
            .count();
        assert_eq!(
            calls, 2,
            "one per command and no more. Two commands live here; a third call means one of them \
             now takes two full `--read-timeout` budgets, which is the defect  removed from \
             `market_views.rs`, `buyer.rs` and `orders.rs`. Share a `ReadBudget` instead"
        );
    }

    /// `--mock-chain` and the `address` subcommand must not need `DEXDO_MANIFEST`.

    /// Same defect as in `run_subscription`, same cause: hoisting the manifest read to the top of
    /// the function to stop re-reading it put it in front of two early returns that never touch a
    /// chain. `run_markets_address` reads the manifest itself; the mock path reads nothing.
    #[test]
    fn the_early_returns_come_before_the_manifest_is_read() {
        let body = crate::cli::source_probe::code_of(
            include_str!("markets.rs"),
            "pub(crate) async fn run_markets(mut args: MarketsArgs)",
        );
        let manifest = body
            .find("let manifest_path = crate::cli::commands::manifest_path()?;")
            .expect("the manifest path is bound once");
        for early in ["return run_markets_address(", "return run_markets_mock("] {
            let at = body
                .find(early)
                .unwrap_or_else(|| panic!("`{early}` is an early return in this command"));
            assert!(
                at < manifest,
                "`{early}` would refuse without DEXDO_MANIFEST: the manifest is read at \
                 {manifest}, before the return at {at}"
            );
        }
    }

    /// `dexdo markets` is a discovery/listing path. With buyer registry validation enabled, a
    /// registered model whose canonical book is missing is hidden from the available list instead
    /// of rendered as buyable.
    #[test]
    fn buyer_markets_hides_missing_canonical_book() {
        let source = include_str!("markets.rs");
        let body = crate::cli::source_probe::code_of(source, "pub(crate) async fn run_markets");

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
        let timeout = body
            .find("direct_chain_read_with_timeout(")
            .expect("markets read timeout present");
        let resolution = body
            .find("resolve_model_registry_target(")
            .expect("markets registry resolution present");
        let json = body.find("if args.json").expect("markets JSON branch");
        let print = body
            .find("println!(")
            .expect("markets prints visible books");

        assert!(
            hide_policy < hidden_action && hidden_action < skip && skip < json && json < print,
            "markets must apply one registry filter before JSON or human output"
        );
        assert!(
            timeout < resolution,
            "markets registry getter must run inside the existing read timeout"
        );
    }

    /// E2E-ORD-26 /: `markets` reads each book, then runs a registry getter per target, then
    /// prints. A snapshot is therefore already older than the print, and the deadline it was filtered
    /// against is the READ's, not the print's.

    /// This is the live incident's shape: SELL 11 rested 956 ticks at 5 SHELL/tick with deadline
    /// 1785678525 and was still being advertised at 1785679304, 779 seconds later -- the cheapest ask
    /// in the book, so a deadline-blind `min()` publishes a `best_ask` no buy can obtain.

    /// The lapsed row and the live row are priced apart and sized apart, so no single number can pass
    /// by accident: `ask_count` distinguishes them by row, `depth_ticks` by size, `best_ask` by price.
    /// E2E-ROW: E2E-ORD-26/L0
    #[test]
    fn market_totals_exclude_an_ask_that_lapsed_between_the_read_and_the_print() {
        use dexdo_core::{OrderBookOrder, OrderBookSnapshot};

        /// SELL 11's deadline, and the moment it was still being advertised.
        const LAPSED_DEADLINE: u64 = 1_785_678_525;
        const PRINTED_AT: u64 = 1_785_679_304;

        fn ask(order_id: u128, price_per_tick: u128, ticks: u128, deadline: u64) -> OrderBookOrder {
            OrderBookOrder {
                order_id,
                owner_note: "0:seller".to_string(),
                token_contract: Some(format!("0:tc{order_id}")),
                is_buy: false,
                price_per_tick,
                ticks,
                escrow: 0,
                deadline,
                flags: 0,
                timestamp: 0,
            }
        }

        let snapshot = OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: vec![
                ask(11, 5_000_000_000, 956, LAPSED_DEADLINE),
                ask(13, 7_000_000_000, 40, PRINTED_AT + 3_600),
            ],
        };

        let read_at = LAPSED_DEADLINE - 1;
        assert_eq!(
            super::market_entry_from_snapshot(&snapshot, None, "market_manifest", read_at)
                .ask_count,
            2,
            "before the deadline both asks are reachable, so this snapshot is one a real read produces"
        );

        let entry =
            super::market_entry_from_snapshot(&snapshot, None, "market_manifest", PRINTED_AT);
        assert_eq!(
            entry.ask_count, 1,
            "the lapsed ask is no longer an offer a buyer can reach"
        );
        assert_eq!(
            entry.depth_ticks,
            crate::cli::machine::amount(40u128),
            "advertised depth is the live ask's 40 ticks, not 956 + 40"
        );
        assert_eq!(
            entry.best_ask,
            Some(dexdo_core::shell_amount(7_000_000_000u128)),
            "the lapsed ask was the cheapest row and must not set the advertised price"
        );
    }

    /// The deadline second itself is already expired on chain -- `_isExpired` is
    /// `deadline != 0 && block.timestamp >= deadline`
    /// (`contracts/airegistry/InferenceOrderBook.sol:1115-1117`). The advertised totals must not be
    /// one second more generous than the matcher.
    #[test]
    fn market_totals_drop_an_ask_on_the_deadline_second_itself() {
        use dexdo_core::{OrderBookOrder, OrderBookSnapshot};

        const DEADLINE: u64 = 1_785_678_525;

        let snapshot = OrderBookSnapshot {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "hash".to_string(),
            order_book: "0:book".to_string(),
            stats: None,
            orders: vec![OrderBookOrder {
                order_id: 11,
                owner_note: "0:seller".to_string(),
                token_contract: Some("0:tc11".to_string()),
                is_buy: false,
                price_per_tick: 5_000_000_000,
                ticks: 956,
                escrow: 0,
                deadline: DEADLINE,
                flags: 0,
                timestamp: 0,
            }],
        };

        assert_eq!(
            super::market_entry_from_snapshot(&snapshot, None, "market_manifest", DEADLINE - 1)
                .ask_count,
            1
        );
        assert_eq!(
            super::market_entry_from_snapshot(&snapshot, None, "market_manifest", DEADLINE)
                .ask_count,
            0,
            "at `now == deadline` the book already refuses the order"
        );
    }
}
