//! Markets list/discovery command handler (Track C11, move-only).

use crate::cli::args::MarketsAddressArgs;
use crate::cli::args::{MarketsArgs, MarketsCommand};
use crate::cli::commands::{
    direct_chain_read_with_timeout, resolve_registry_content_identity, ReadBudget,
};
use crate::cli::commands::{mock_chain_for_machine, render_model_flags_field};
use crate::cli::indexer::{self, IndexerClient, InferenceMarket, MarketsQuery};
use crate::cli::machine;
use anyhow::Context;
use anyhow::Result;
use dexdo::registry::{RegistryRole, RegistrySuggestions};
use dexdo_core::address as addr;
use dexdo_core::{model_hash_for, ChainBackend, DobParams, MockChainBackend};
use std::collections::HashSet;
use std::time::Duration;

async fn mock_market_entry(
    chain: &MockChainBackend,
    frame_model: &str,
) -> Result<machine::MarketEntry> {
    let offers = chain.discover_offers().await?;
    let depth_ticks: u128 = offers.iter().map(|o| u128::from(o.max_ticks)).sum();
    let best_ask = offers.iter().map(|o| o.price_per_tick).min();
    Ok(machine::MarketEntry {
        frame_model: frame_model.to_string(),
        order_book: "mock:order-book".to_string(),
        total_ask_ticks: machine::amount(depth_ticks),
        best_ask: best_ask.map(dexdo_core::shell_amount),
        tick_size: machine::amount(DobParams::canonical().tick_size),
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
        "model={}{} order_book={} total_ask_ticks={} best_ask={} tick_size={}",
        entry.frame_model,
        render_model_flags_field(&entry.frame_model),
        addr::display(&entry.order_book),
        entry.total_ask_ticks,
        entry.best_ask.as_deref().unwrap_or("-"),
        entry.tick_size,
    );
    Ok(())
}

fn market_entry_from_indexer(market: InferenceMarket) -> machine::MarketEntry {
    machine::MarketEntry {
        frame_model: market.model_ref_name,
        order_book: market.inference_order_book_address,
        total_ask_ticks: market.total_ask_ticks,
        best_ask: market.best_ask,
        tick_size: market.tick_size,
    }
}

async fn read_all_indexer_markets(
    client: &IndexerClient,
    liquidity: Option<&str>,
    read_timeout_secs: u64,
) -> Result<Vec<InferenceMarket>> {
    let budget = ReadBudget::new(read_timeout_secs);
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    let mut markets = Vec::new();
    loop {
        let page = budget
            .read(client.markets(MarketsQuery {
                inference_order_book_address: None,
                producer: None,
                status: None,
                liquidity,
                cursor: cursor.as_deref(),
                limit: None,
            }))
            .await
            .context("indexer API unavailable for `dexdo markets`")?;
        markets.extend(page.markets);
        if !page.has_more {
            return Ok(markets);
        }
        let next = page.next_cursor.ok_or_else(|| {
            anyhow::anyhow!(
                "indexer API returned hasMore=true without nextCursor for `dexdo markets`"
            )
        })?;
        if !seen_cursors.insert(next.clone()) {
            return Err(anyhow::anyhow!(
                "indexer API repeated nextCursor `{next}` for `dexdo markets`"
            ));
        }
        cursor = Some(next);
    }
}

fn markets_liquidity_filter(all: bool) -> Option<&'static str> {
    (!all).then_some(dexdo_core::params::MARKETS_INDEXER_LIQUIDITY_FILTER)
}

pub(crate) async fn run_markets(mut args: MarketsArgs) -> Result<()> {
    if let Some(MarketsCommand::Address(address)) = args.command.take() {
        return run_markets_address(address).await;
    }
    if args.mock_chain {
        return run_markets_mock(args).await;
    }
    // Keep the two non-indexer modes above the manifest read: neither `markets address` nor the
    // explicit mock chain depends on a configured indexer.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = indexer::ManifestIndexer::load(&manifest_path)?;
    let network = manifest.network().to_string();
    let base_url = indexer::resolve_base_url(args.indexer_url.as_deref(), Some(&manifest))?;
    let client = IndexerClient::new(
        base_url,
        Duration::from_secs(args.read_timeout.read_timeout_secs),
    )?;
    let liquidity = markets_liquidity_filter(args.all);
    let markets = read_all_indexer_markets(&client, liquidity, args.read_timeout.read_timeout_secs)
        .await?
        .into_iter()
        .map(market_entry_from_indexer)
        .collect::<Vec<_>>();
    if args.json {
        return machine::print_json(&machine::MarketsResponse {
            schema: machine::MARKETS_SCHEMA,
            network,
            generated_at_unix: machine::now_unix()?,
            markets,
        });
    }
    for market in markets {
        println!(
            "model={} order_book={} total_ask_ticks={} best_ask={} tick_size={}",
            market.frame_model,
            addr::display(&market.order_book),
            market.total_ask_ticks,
            market.best_ask.as_deref().unwrap_or("-"),
            market.tick_size,
        );
    }
    Ok(())
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

/// The listing form of `markets` now receives the address from the selected indexer. This command
/// remains the independent on-chain derivation for one requested registry model.

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
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    #[test]
    fn default_lists_sell_liquidity_and_all_omits_the_filter() {
        assert_eq!(
            markets_liquidity_filter(false),
            Some(dexdo_core::params::MARKETS_INDEXER_LIQUIDITY_FILTER)
        );
        assert_eq!(markets_liquidity_filter(true), None);
    }

    #[test]
    fn a_network_without_a_named_indexer_is_a_refusal() {
        let manifest = indexer::ManifestIndexer::from_json(r#"{"network":"isolated"}"#)
            .expect("manifest fixture");
        let error = indexer::resolve_base_url_with_env(None, Some(&manifest), None)
            .expect_err("markets has no chain fallback");
        let rendered = format!("{error:#}");

        assert!(rendered.contains("carries no `indexer`"), "{rendered}");
        assert!(rendered.contains("nothing says who to ask"), "{rendered}");
    }

    /// E2E-ROW: E2E-ORD-26/L0
    #[test]
    fn markets_v3_preserves_indexer_liquidity_aggregates_and_has_exactly_five_fields() {
        let entry = market_entry_from_indexer(market('a', "model-a", "8"));
        let value = serde_json::to_value(entry).expect("serialize v3 market row");
        let object = value.as_object().expect("market row object");
        assert_eq!(object.len(), 5, "v3 must not retain chain-only v2 fields");
        for field in [
            "frame_model",
            "order_book",
            "total_ask_ticks",
            "best_ask",
            "tick_size",
        ] {
            assert!(object.contains_key(field), "missing {field}: {object:?}");
        }
        assert_eq!(object["total_ask_ticks"], "8");
        assert_eq!(object["best_ask"], "1.000000000");
        assert_eq!(object["tick_size"], "1");
    }

    fn market(fill: char, name: &str, total_ask_ticks: &str) -> InferenceMarket {
        InferenceMarket {
            inference_order_book_address: format!("0:{}", fill.to_string().repeat(64)),
            model_ref_name: name.to_string(),
            contract_version: Some("4.0.36".to_string()),
            status: "TRADING".to_string(),
            quote_asset: "SHELL".to_string(),
            maker_commission: "-0.02".to_string(),
            taker_commission: "0.025".to_string(),
            price_precision: 9,
            quantity_precision: 0,
            tick_size: "1".to_string(),
            step_size: "1".to_string(),
            min_notional: "1".to_string(),
            reference_price: None,
            best_ask: Some("1.000000000".to_string()),
            total_ask_ticks: total_ask_ticks.to_string(),
            created_at: 1,
        }
    }

    fn page(market: &InferenceMarket, next_cursor: Option<&str>, has_more: bool) -> String {
        serde_json::json!({
            "serverTime": 1,
            "nextCursor": next_cursor,
            "hasMore": has_more,
            "markets": [market],
        })
        .to_string()
    }

    async fn page_server(pages: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind indexer fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in pages {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0u8; 4096];
                let read = socket.read(&mut request).await.expect("read request");
                requests.push(String::from_utf8_lossy(&request[..read]).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    async fn delayed_page_server(
        pages: Vec<(Duration, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind delayed indexer fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (delay, body) in pages {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0u8; 4096];
                let read = socket.read(&mut request).await.expect("read request");
                requests.push(String::from_utf8_lossy(&request[..read]).into_owned());
                tokio::time::sleep(delay).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    #[tokio::test]
    async fn every_market_page_uses_the_sell_filter_and_next_cursor() {
        let first = market('a', "model-a", "8");
        let second = market('b', "model-b", "13");
        let (endpoint, server) = page_server(vec![
            page(&first, Some("page-2"), true),
            page(&second, None, false),
        ])
        .await;
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let markets = read_all_indexer_markets(
            &client,
            Some(dexdo_core::params::MARKETS_INDEXER_LIQUIDITY_FILTER),
            2,
        )
        .await
        .expect("read every page");
        let requests = server.await.expect("fixture server");

        assert_eq!(markets.len(), 2);
        assert!(requests[0].contains("liquidity=SELL"), "{}", requests[0]);
        assert!(!requests[0].contains("cursor="), "{}", requests[0]);
        assert!(requests[1].contains("liquidity=SELL"), "{}", requests[1]);
        assert!(requests[1].contains("cursor=page-2"), "{}", requests[1]);
    }

    #[tokio::test]
    async fn all_omits_the_liquidity_filter_from_the_request() {
        let only = market('a', "model-a", "0");
        let (endpoint, server) = page_server(vec![page(&only, None, false)]).await;
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let markets = read_all_indexer_markets(&client, None, 2)
            .await
            .expect("read unfiltered markets");
        let requests = server.await.expect("fixture server");

        assert_eq!(markets.len(), 1);
        assert!(!requests[0].contains("liquidity="), "{}", requests[0]);
    }

    #[tokio::test]
    async fn null_and_omitted_contract_versions_keep_every_valid_v3_row() {
        let mut null_version = serde_json::to_value(market('a', "model-a", "0"))
            .expect("serialize null-version fixture");
        null_version["contractVersion"] = serde_json::Value::Null;
        null_version["bestAsk"] = serde_json::Value::Null;
        let mut omitted_version = serde_json::to_value(market('b', "model-b", "0"))
            .expect("serialize omitted-version fixture");
        omitted_version["bestAsk"] = serde_json::Value::Null;
        omitted_version
            .as_object_mut()
            .expect("market fixture object")
            .remove("contractVersion");
        let body = serde_json::json!({
            "serverTime": 1,
            "nextCursor": null,
            "hasMore": false,
            "markets": [null_version, omitted_version],
        })
        .to_string();
        let (endpoint, server) = page_server(vec![body]).await;
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let entries = read_all_indexer_markets(&client, None, 2)
            .await
            .expect("optional contract metadata must not break listing")
            .into_iter()
            .map(market_entry_from_indexer)
            .collect::<Vec<_>>();
        let requests = server.await.expect("fixture server");

        assert_eq!(requests.len(), 1);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].frame_model, "model-a");
        assert_eq!(entries[1].frame_model, "model-b");
        for entry in entries {
            let value = serde_json::to_value(entry).expect("serialize v3 market row");
            let object = value.as_object().expect("v3 market row object");
            assert_eq!(object.len(), 5, "v3 row changed shape: {object:?}");
            assert_eq!(object["total_ask_ticks"], "0");
            assert_eq!(object["best_ask"], serde_json::Value::Null);
        }
    }

    #[tokio::test]
    async fn has_more_without_a_cursor_fails_the_whole_listing() {
        let only = market('a', "model-a", "8");
        let (endpoint, server) = page_server(vec![page(&only, None, true)]).await;
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let error = read_all_indexer_markets(&client, None, 2)
            .await
            .expect_err("a partial listing must not be returned");
        let requests = server.await.expect("fixture server");

        assert_eq!(requests.len(), 1);
        assert!(format!("{error:#}").contains("without nextCursor"));
    }

    #[tokio::test]
    async fn a_directly_repeated_cursor_fails_the_whole_listing() {
        let first = market('a', "model-a", "8");
        let second = market('b', "model-b", "13");
        let (endpoint, server) = page_server(vec![
            page(&first, Some("page-a"), true),
            page(&second, Some("page-a"), true),
        ])
        .await;
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let error = read_all_indexer_markets(&client, None, 2)
            .await
            .expect_err("a partial listing must not be returned");
        let requests = server.await.expect("fixture server");

        assert_eq!(requests.len(), 2);
        assert!(format!("{error:#}").contains("repeated nextCursor `page-a`"));
    }

    #[tokio::test]
    async fn a_non_consecutive_cursor_cycle_fails_the_whole_listing() {
        let first = market('a', "model-a", "8");
        let second = market('b', "model-b", "13");
        let third = market('c', "model-c", "21");
        let (endpoint, server) = page_server(vec![
            page(&first, Some("page-a"), true),
            page(&second, Some("page-b"), true),
            page(&third, Some("page-a"), true),
        ])
        .await;
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let error = read_all_indexer_markets(&client, None, 2)
            .await
            .expect_err("a cursor cycle must not return duplicated partial rows");
        let requests = server.await.expect("fixture server");

        assert_eq!(requests.len(), 3);
        assert!(format!("{error:#}").contains("repeated nextCursor `page-a`"));
    }

    #[tokio::test]
    async fn every_page_spends_the_same_command_read_budget() {
        let first = market('a', "model-a", "8");
        let second = market('b', "model-b", "13");
        let (endpoint, server) = delayed_page_server(vec![
            (
                Duration::from_millis(250),
                page(&first, Some("page-2"), true),
            ),
            (Duration::from_millis(900), page(&second, None, false)),
        ])
        .await;
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let error = read_all_indexer_markets(&client, None, 1)
            .await
            .expect_err("two slow pages must share one one-second budget");
        let requests = server.await.expect("fixture server");
        let rendered = format!("{error:#}");

        assert_eq!(requests.len(), 2);
        assert!(rendered.contains("this command's 1s `--read-timeout` budget"));
    }

    #[tokio::test]
    async fn an_unavailable_indexer_fails_without_a_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind failing indexer fixture");
        let endpoint = format!("http://{}", listener.local_addr().expect("fixture address"));
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0u8; 4096];
            let read = socket.read(&mut request).await.expect("read request");
            let body = "indexer unavailable";
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        let client = IndexerClient::new(endpoint, Duration::from_secs(2)).expect("client");

        let error = read_all_indexer_markets(
            &client,
            Some(dexdo_core::params::MARKETS_INDEXER_LIQUIDITY_FILTER),
            2,
        )
        .await
        .expect_err("markets must fail when its sole source is unavailable");
        let request = server.await.expect("fixture server");
        let rendered = format!("{error:#}");

        assert!(request.contains("liquidity=SELL"), "{request}");
        assert!(rendered.contains("indexer API unavailable"), "{rendered}");
        assert!(rendered.contains("503 Service Unavailable"), "{rendered}");
    }
}
