//! Order-book display command handler(Track C6, move-only).

use crate::cli::args::OrdersArgs;
#[cfg(feature = "shellnet")]
use crate::cli::args::OrdersCommand;
#[cfg(feature = "shellnet")]
use crate::cli::commands::{
    direct_chain_read_with_timeout, fold_snapshot_from_orders, model_target_from_config,
    read_book_target, resolve_order_book_target, retry_executable_read, target_from_market,
    BookTarget,
};
#[cfg(feature = "shellnet")]
use crate::cli::support::read_secret_hex;
use anyhow::{bail, Result};
#[cfg(feature = "shellnet")]
use dexdo_core::shellnet::BookEventFold;
#[cfg(feature = "shellnet")]
use dexdo_core::{OrderBookOrder, OrderBookSnapshot};

#[cfg(feature = "shellnet")]
async fn read_live_order_snapshot(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
    order_book: &str,
) -> Result<OrderBookSnapshot> {
    match retry_executable_read("order-book event fold", || async {
        let fold = chain
            .fold_order_book_events(order_book, BookEventFold::default())
            .await?;
        Ok(fold_snapshot_from_orders(
            target,
            order_book,
            fold.live_orders(),
        ))
    })
    .await
    {
        Ok(snapshot) => Ok(snapshot),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "order-book event fold unavailable; using legacy chain fallback");
            retry_executable_read("legacy order-book fallback", || {
                read_book_target(chain, target)
            })
            .await
        }
    }
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
        "order_id={} side={} owner={} token_contract={} price_per_tick={} ticks={} deadline={}",
        order.order_id,
        side,
        order.owner_note,
        tc,
        order.price_per_tick,
        order.ticks,
        order.deadline
    )
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
    let snapshot = direct_chain_read_with_timeout(args.read_timeout.read_timeout_secs, async {
        let order_book = resolve_order_book_target(&chain, &target).await?;
        read_live_order_snapshot(&chain, &target, &order_book).await
    })
    .await?;
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
            direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.assert_note_owner_matches("orders cancel", &note, &keys),
            )
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
            direct_chain_read_with_timeout(
                args.read_timeout.read_timeout_secs,
                chain.assert_note_owner_matches("orders cancel-all", &note, &keys),
            )
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
