use super::read_real_monitor_snapshot_with;
use crate::cli::support::render_tree_snapshot;
use dexdo_core::{model_hash_for, DealRole, DealView, MarketManifest, OrderBookOrder};
use proptest::prelude::*;
use std::path::{Path, PathBuf};

fn address(digit: char) -> String {
    format!("0:{}", digit.to_string().repeat(64))
}

fn market_file(dir: &Path, name: &str, book: char) -> PathBuf {
    let model = "monitor-test-model";
    let manifest = MarketManifest {
        network: "test".into(),
        frame_model: model.into(),
        model_hash: model_hash_for(model),
        inference_order_book: address(book),
        root_model: address('c'),
        token_contract: address('d'),
        seller_note: address('a'),
        nonce: 0,
        price_per_tick: 40 * dexdo_core::params::SHELL_UNIT,
        max_ticks: 446,
    };
    let path = dir.join(name);
    std::fs::write(&path, manifest.to_json().unwrap()).unwrap();
    path
}

fn deal(manifest: &MarketManifest) -> DealView {
    DealView {
        token_contract: manifest.token_contract.clone(),
        role: DealRole::Seller,
        counterparty: None,
        price_per_tick: manifest.price_per_tick.try_into().unwrap(),
        model: Some(manifest.frame_model.clone()),
        snapshot: None,
    }
}

fn sell(order_id: u128) -> OrderBookOrder {
    OrderBookOrder {
        order_id,
        owner_note: address('a'),
        token_contract: Some(address('d')),
        is_buy: false,
        price_per_tick: 40 * dexdo_core::params::SHELL_UNIT,
        ticks: 446,
        escrow: 0,
        deadline: u64::MAX,
        flags: 0,
        timestamp: 0,
    }
}

#[tokio::test]
async fn live_own_sell_appears_in_the_rendered_monitor() {
    let dir = tempfile::tempdir().unwrap();
    let path = market_file(dir.path(), "market.json", 'b');
    let snapshot = read_real_monitor_snapshot_with(&[path], |market| async move {
        let mut row = sell(138);
        row.token_contract = Some(address('e'));
        Ok((deal(&market), vec![row]))
    })
    .await
    .unwrap();
    let rendered = render_tree_snapshot(&snapshot);
    assert!(rendered.contains("offers in book: 1\n"), "{rendered}");
    assert!(rendered.contains("40 SHELL/tick x 446 ticks"), "{rendered}");
    assert_eq!(snapshot.offers[0].seller_id, address('a'));
    assert_eq!(snapshot.offers[0].token_contract, address('e'));
    assert_eq!(snapshot.deals.len(), 1);
    assert_eq!(snapshot.exposure, 0);
}

#[tokio::test]
async fn foreign_buy_expired_and_empty_rows_are_not_live_own_offers() {
    let dir = tempfile::tempdir().unwrap();
    let path = market_file(dir.path(), "market.json", 'b');
    let mut rows = vec![sell(1)];
    for id in 2..=7 {
        let mut row = sell(id);
        match id {
            2 => row.owner_note = address('f'),
            3 => row.is_buy = true,
            4 => row.deadline = crate::cli::provenance::now_unix().unwrap(),
            5 => row.deadline = 0,
            6 => row.ticks = 0,
            7 => row.token_contract = None,
            _ => unreachable!(),
        }
        rows.push(row);
    }
    let snapshot = read_real_monitor_snapshot_with(&[path], |market| {
        let rows = rows.clone();
        async move { Ok((deal(&market), rows)) }
    })
    .await
    .unwrap();
    assert_eq!(snapshot.offers.len(), 1);
    assert_eq!(snapshot.offers[0].max_ticks, 446);
}

#[tokio::test]
async fn a_storage_read_failure_is_an_error_with_the_market_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = market_file(dir.path(), "market.json", 'b');
    let error = read_real_monitor_snapshot_with(std::slice::from_ref(&path), |_| async {
        anyhow::bail!("order-book storage unavailable")
    })
    .await
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(path.to_str().unwrap()), "{message}");
    assert!(
        message.contains("order-book storage unavailable"),
        "{message}"
    );
}

#[tokio::test]
async fn a_read_book_with_no_orders_renders_a_measured_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = market_file(dir.path(), "market.json", 'b');
    let snapshot = read_real_monitor_snapshot_with(&[path], |market| async move {
        Ok((deal(&market), Vec::new()))
    })
    .await
    .unwrap();
    assert!(render_tree_snapshot(&snapshot).contains("offers in book: 0\n"));
}

#[tokio::test]
async fn the_same_order_id_in_different_books_counts_twice() {
    let dir = tempfile::tempdir().unwrap();
    let markets = ['b', 'e'].map(|book| market_file(dir.path(), &format!("{book}.json"), book));
    let snapshot = read_real_monitor_snapshot_with(&markets, |market| async move {
        Ok((deal(&market), vec![sell(138)]))
    })
    .await
    .unwrap();
    assert_eq!(snapshot.offers.len(), 2);
}

proptest! {
    #[test]
    fn repeated_manifests_never_multiply_own_offers(repeats in 1usize..8, foreign in 0u128..10) {
        let dir = tempfile::tempdir().unwrap();
        let path = market_file(dir.path(), "market.json", 'b');
        let other_path = market_file(dir.path(), "another-deal.json", 'b');
        let markets = (0..repeats).map(|i| if i % 2 == 0 { path.clone() } else { other_path.clone() }).collect::<Vec<_>>();
        let snapshot = tokio::runtime::Runtime::new().unwrap().block_on(
            read_real_monitor_snapshot_with(&markets, |market| async move {
                let mut rows = vec![sell(138)];
                for id in 0..foreign {
                    let mut row = sell(id);
                    row.owner_note = address('f');
                    rows.push(row);
                }
                Ok((deal(&market), rows))
            })
        ).unwrap();
        prop_assert_eq!(snapshot.offers.len(), 1);
        prop_assert_eq!(&snapshot.offers[0].seller_id, &address('a'));
    }
}
