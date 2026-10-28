//! Opt-in shellnet evidence for the currency gate. Keys and recovery stay in a caller-owned directory.
// Reuse this module's two-note setup and receipt helpers for the focused seller regression.
include!("backends_issue_2199_live.rs");
use super::*;
use std::{fs, os::unix::fs::OpenOptionsExt, path::Path, process::Command};

fn role(dir: &Path, name: &str) -> (Address, KeyPair) {
    let pool: Value =
        serde_json::from_slice(&fs::read(dir.join(format!("{name}.pool.json"))).unwrap()).unwrap();
    let note = &pool["notes"][0];
    (
        crate::address::parse_chain_address(note["address"].as_str().unwrap())
            .unwrap()
            .into_chain(),
        KeyPair::from_secret_hex(note["owner_secret_key_hex"].as_str().unwrap()).unwrap(),
    )
}

async fn wait_active(chain: &RealChainBackend, address: &Address) {
    for _ in 0..crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if chain
            .get_system_account_retrying(address)
            .await
            .unwrap()
            .is_some_and(|account| account.is_active())
        {
            return;
        }
        tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
    }
    panic!("live account not Active: {}", address.with_workchain());
}

#[tokio::test]
#[ignore = "read-only verification after the large CLI BUY/serving/STOP"]
async fn live_2196_verify_large_settlement_receipts() {
    let chain = RealChainBackend::connect(crate::params::manifest_path().unwrap()).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2196_STATE_DIR").unwrap());
    let market = crate::MarketManifest::from_json(
        &fs::read_to_string(dir.join("large.market.json")).unwrap(),
    )
    .unwrap();
    let tc = Address::parse(&market.token_contract).unwrap();
    let receipts = chain.token_contract_settlement_receipts(&tc).await.unwrap();
    let count = |predicate: fn(&TokenContractSettlementEvent) -> bool| {
        receipts
            .events
            .iter()
            .filter(|r| predicate(&r.event))
            .count()
    };
    assert_eq!(
        count(|e| matches!(e, TokenContractSettlementEvent::ProbeAccepted { .. })),
        1
    );
    assert!(count(|e| matches!(e, TokenContractSettlementEvent::TicksClaimed { .. })) >= 2);
    assert_eq!(
        count(|e| matches!(e, TokenContractSettlementEvent::StreamStopped { .. })),
        1
    );
    assert_eq!(
        count(|e| matches!(
            e,
            TokenContractSettlementEvent::ProbeBurned { .. }
                | TokenContractSettlementEvent::StreamDisputed { .. }
                | TokenContractSettlementEvent::DisputeResolved { .. }
                | TokenContractSettlementEvent::StreamReclaimed { .. }
        )),
        0
    );
    assert!(chain
        .token_contract_deal_state(&tc)
        .await
        .unwrap()
        .is_none());
    fs::write(dir.join("large-receipts.txt"), format!("{receipts:#?}")).unwrap();
    for receipt in receipts.events {
        println!("2196 receipt {} {:?}", receipt.message_id, receipt.event);
    }
}

async fn wait_ecc_zero(chain: &RealChainBackend, address: &Address) {
    for _ in 0..crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if account_ecc(chain, address, true).await == 0 {
            return;
        }
        tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
    }
    panic!("ECC debit not confirmed for {}", address.with_workchain());
}

async fn retire_idle_cleanup_fixture(
    chain: &RealChainBackend,
    tc: &Address,
    seller_keys: &KeyPair,
) {
    let Some(state) = chain.token_contract_deal_state(tc).await.unwrap() else {
        return;
    };
    assert!(!state.funded && !state.opened && state.deposit == 0);
    // cleanupUnopened deliberately keeps an idle TC alive for the seller to reuse.
    // The zero-ECC refund is already proved; this funding is only for test teardown.
    let have = account_ecc(chain, tc, true).await;
    if have < crate::params::DEAL_BURN_TERMINAL_RAW {
        chain
            .giver_send_shell(
                &tc.with_workchain(),
                crate::params::DEAL_BURN_TERMINAL_RAW - have,
            )
            .await
            .unwrap();
        wait_ecc(chain, tc, crate::params::DEAL_BURN_TERMINAL_RAW, true).await;
    }
    chain.close_unsold_deal(tc, seller_keys).await.unwrap();
    chain.wait_deal_destroyed(tc).await.unwrap();
}

#[tokio::test]
#[ignore = "teardown-only resume for a successfully refunded idle cleanup fixture"]
async fn live_2196_close_idle_cleanup_fixture() {
    let chain = RealChainBackend::connect(crate::params::manifest_path().unwrap()).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2196_STATE_DIR").unwrap());
    let (_, seller_keys) = role(&dir, "seller");
    let tc = Address::parse(
        fs::read_to_string(dir.join("exit-101-address.txt"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    retire_idle_cleanup_fixture(&chain, &tc, &seller_keys).await;
}

#[tokio::test]
#[ignore = "read-only final audit of the dedicated live run"]
async fn live_2196_verify_fixture_retirements() {
    let chain = RealChainBackend::connect(crate::params::manifest_path().unwrap()).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2196_STATE_DIR").unwrap());
    let mut addresses = Vec::new();
    for name in ["large.market.json", "first-cycle/large.market.json"] {
        if name.starts_with("first-cycle/") && !dir.join(name).exists() {
            continue;
        }
        let market =
            crate::MarketManifest::from_json(&fs::read_to_string(dir.join(name)).unwrap()).unwrap();
        addresses.push(Address::parse(&market.token_contract).unwrap());
    }
    for name in [
        "small-address.txt",
        "parking-address.txt",
        "exit-100-address.txt",
        "exit-101-address.txt",
    ] {
        addresses.push(Address::parse(fs::read_to_string(dir.join(name)).unwrap().trim()).unwrap());
    }
    for tc in addresses {
        assert!(chain
            .token_contract_deal_state(&tc)
            .await
            .unwrap()
            .is_none());
        assert!(!chain
            .get_system_account_retrying(&tc)
            .await
            .unwrap()
            .is_some_and(|a| a.is_active()));
        let receipts = chain.token_contract_settlement_receipts(&tc).await.unwrap();
        assert!(receipts.events.iter().any(|r| matches!(
            r.event,
            TokenContractSettlementEvent::ContractDestroyed { .. }
        )));
        println!("2196 retired {}", tc.with_workchain());
    }
    for name in ["seller", "buyer"] {
        let (note, _) = role(&dir, name);
        let outstanding = chain
            .run_getter_retrying(&note, PRIVATENOTE_ABI, "getOutstanding", json!({}))
            .await
            .unwrap()
            .unwrap();
        assert!(
            outstanding["deals"].as_array().unwrap().is_empty(),
            "{name}: {outstanding}"
        );
        assert!(
            outstanding["orders"].as_array().unwrap().is_empty(),
            "{name}: {outstanding}"
        );
        println!("2196 {name} note has zero outstanding deals/orders");
    }
}

async fn wait_state(chain: &RealChainBackend, tc: &Address, opened: bool) {
    for _ in 0..crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if chain
            .token_contract_deal_state(tc)
            .await
            .unwrap()
            .is_some_and(|s| s.funded && s.opened == opened)
        {
            return;
        }
        tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
    }
    panic!(
        "expected live state at {} opened={opened}",
        tc.with_workchain()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reuses the two dedicated notes; checks an Astra-sized fresh ask at the exact ECC boundary"]
async fn live_2196_small_buy_ecc_boundary() {
    use crate::params::*;
    let chain = RealChainBackend::connect(manifest_path().unwrap()).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2196_STATE_DIR").unwrap());
    let market = crate::MarketManifest::from_json(
        &fs::read_to_string(dir.join("large.market.json")).unwrap(),
    )
    .unwrap();
    let (seller, keys) = role(&dir, "seller");
    let nonce = market.nonce + 300;
    let frame = format!("gas-2196-small-{nonce}");
    let hash = model_hash_for(&frame);
    let root_model = Address::parse(&market.root_model).unwrap();
    let tc = chain
        .token_contract_deploy_address(&keys, &root_model, nonce)
        .await
        .unwrap();
    fs::write(dir.join("small-address.txt"), tc.with_workchain()).unwrap();
    let gas = deal_gas_reserve_raw(875) - 1;
    let have = account_ecc(&chain, &seller, true).await;
    if have < gas {
        chain
            .giver_send_shell(&seller.with_workchain(), gas - have)
            .await
            .unwrap();
        wait_ecc(&chain, &seller, gas, true).await;
    }
    chain
        .deploy_inference_orderbook(&seller, &keys, &hash, &frame, TICK_SIZE)
        .await
        .unwrap();
    let book = chain
        .inference_orderbook_address(&seller, &hash, TICK_SIZE)
        .await
        .unwrap();
    wait_active(&chain, &book).await;
    chain
        .note_deploy_deal(
            &seller,
            &keys,
            crate::chain::client::NoteDeployDealRequest {
                nonce,
                model_name: &frame,
                price_per_tick: 50 * SHELL_UNIT,
                max_ticks: 875,
                gas_reserve: gas,
            },
        )
        .await
        .unwrap();
    wait_active(&chain, &tc).await;
    chain
        .post_sell_offer(&seller, &keys, 0, nonce, MAX_SELL_TTL.as_secs())
        .await
        .unwrap();
    let mut snapshot = chain
        .inference_orderbook_snapshot(&book, &frame, &hash)
        .await
        .unwrap();
    for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if !snapshot.orders.is_empty() {
            break;
        }
        tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
        snapshot = chain
            .inference_orderbook_snapshot(&book, &frame, &hash)
            .await
            .unwrap();
    }
    let required = deal_buy_reserve_raw(875, false);
    assert_eq!(account_ecc(&chain, &tc, true).await, required - 1);
    let error = chain
        .submit_safe_model_buy_ask(&snapshot, 2, 50 * SHELL_UNIT)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("ECC[2] reserve"), "{error}");
    for target in [required, required + 1] {
        chain
            .giver_send_shell(&tc.with_workchain(), 1)
            .await
            .unwrap();
        wait_ecc(&chain, &tc, target, true).await;
        let ask = chain
            .submit_safe_model_buy_ask(&snapshot, 2, 50 * SHELL_UNIT)
            .await
            .unwrap();
        let (listed, reason) = chain
            .submit_safe_executable_book_asks(&snapshot, 2, 50 * SHELL_UNIT)
            .await
            .unwrap();
        assert!(reason.is_none() && listed.iter().any(|row| row.order_id == ask.order_id));
        println!(
            "2196 small BUY boundary {} ECC[2]={target} maxTicks=875 requested=2 price=50",
            tc.with_workchain()
        );
    }
    chain
        .cancel_inference_order(&seller, &keys, &hash, snapshot.orders[0].order_id)
        .await
        .unwrap();
    for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if chain
            .token_contract_offer(&tc)
            .await
            .unwrap()
            .is_some_and(|offer| !offer.offer_posted)
        {
            break;
        }
        tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
    }
    chain.close_unsold_deal(&tc, &keys).await.unwrap();
    chain.wait_deal_destroyed(&tc).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run after the large BUY/serving/STOP; reuses its two notes and needs giver env"]
async fn live_2196_low_ecc_exit_gates_and_cleanup() {
    use crate::params::*;
    let chain = RealChainBackend::connect(manifest_path().unwrap()).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2196_STATE_DIR").unwrap());
    let market = crate::MarketManifest::from_json(
        &fs::read_to_string(dir.join("large.market.json")).unwrap(),
    )
    .unwrap();
    let (seller, seller_keys) = role(&dir, "seller");
    let (buyer, buyer_keys) = role(&dir, "buyer");
    let root_model = Address::parse(&market.root_model).unwrap();
    let parking_nonce = market.nonce + 200;
    let buyer_root = chain
        .deploy_root_model_note_funded(&buyer_keys)
        .await
        .unwrap();
    let parking = chain
        .token_contract_deploy_address(&buyer_keys, &buyer_root, parking_nonce)
        .await
        .unwrap();
    fs::write(dir.join("parking-address.txt"), parking.with_workchain()).unwrap();
    let mut note_ecc = account_ecc(&chain, &buyer, true).await;
    if chain
        .token_contract_deal_state(&parking)
        .await
        .unwrap()
        .is_none()
    {
        if note_ecc < DEAL_BURN_DEPLOY_RAW {
            chain
                .giver_send_shell(&buyer.with_workchain(), DEAL_BURN_DEPLOY_RAW - note_ecc)
                .await
                .unwrap();
            wait_ecc(&chain, &buyer, DEAL_BURN_DEPLOY_RAW, true).await;
            note_ecc = account_ecc(&chain, &buyer, true).await;
        }
        chain
            .note_deploy_deal(
                &buyer,
                &buyer_keys,
                crate::chain::client::NoteDeployDealRequest {
                    nonce: parking_nonce,
                    model_name: &market.frame_model,
                    price_per_tick: SHELL_UNIT,
                    max_ticks: MIN_STREAM_BUY_TICKS,
                    gas_reserve: note_ecc,
                },
            )
            .await
            .unwrap();
    } else if note_ecc > 0 {
        chain
            .note_fund_deploy_shell(&buyer, &buyer_keys, parking_nonce, 0, note_ecc)
            .await
            .unwrap();
    }
    wait_active(&chain, &parking).await;
    wait_ecc_zero(&chain, &buyer).await;

    let frame = format!("gas-2196-exits-{}", market.nonce);
    let hash = model_hash_for(&frame);
    let book = chain
        .inference_orderbook_address(&seller, &hash, TICK_SIZE)
        .await
        .unwrap();
    if chain
        .inference_orderbook_stats(&book)
        .await
        .unwrap()
        .is_none()
    {
        chain
            .deploy_inference_orderbook(&seller, &seller_keys, &hash, &frame, TICK_SIZE)
            .await
            .unwrap();
    }
    wait_active(&chain, &book).await;

    for (offset, dispute) in [(100, true), (101, false)] {
        let nonce = market.nonce + offset;
        let tc = chain
            .token_contract_deploy_address(&seller_keys, &root_model, nonce)
            .await
            .unwrap();
        fs::write(
            dir.join(format!("exit-{offset}-address.txt")),
            tc.with_workchain(),
        )
        .unwrap();
        if chain
            .token_contract_deal_state(&tc)
            .await
            .unwrap()
            .is_none()
        {
            if let Some(receipts) = chain
                .token_contract_settlement_receipts_if_present(&tc)
                .await
                .unwrap()
            {
                if receipts.events.iter().any(|r| {
                    matches!(
                        r.event,
                        TokenContractSettlementEvent::ContractDestroyed { .. }
                    )
                }) {
                    assert!(
                        dispute
                            && receipts.events.iter().any(|r| matches!(
                                r.event,
                                TokenContractSettlementEvent::StreamDisputed { .. }
                            ))
                    );
                    fs::write(
                        dir.join(format!("exit-{offset}-receipts.txt")),
                        format!("{receipts:#?}"),
                    )
                    .unwrap();
                    println!(
                        "2196 resumed already-completed dispute fixture {}",
                        tc.with_workchain()
                    );
                    continue;
                }
            }
            // Deliberately short seller reserve: setup succeeds and consumes it exactly.
            // This is adversarial fixture setup, never a production BUY bypass.
            let setup = DEAL_BURN_DEPLOY_RAW + DEAL_BURN_POST_FROM_NOTE_RAW;
            let have = account_ecc(&chain, &seller, true).await;
            if have < setup {
                chain
                    .giver_send_shell(&seller.with_workchain(), setup - have)
                    .await
                    .unwrap();
                wait_ecc(&chain, &seller, setup, true).await;
            }
            chain
                .note_deploy_deal(
                    &seller,
                    &seller_keys,
                    crate::chain::client::NoteDeployDealRequest {
                        nonce,
                        model_name: &frame,
                        price_per_tick: SHELL_UNIT,
                        max_ticks: MIN_STREAM_BUY_TICKS,
                        gas_reserve: setup,
                    },
                )
                .await
                .unwrap();
            wait_active(&chain, &tc).await;
            chain
                .post_sell_offer(&seller, &seller_keys, 0, nonce, MAX_SELL_TTL.as_secs())
                .await
                .unwrap();
        } else {
            assert!(
                !chain
                    .token_contract_deal_state(&tc)
                    .await
                    .unwrap()
                    .unwrap()
                    .funded,
                "resume of a funded fixture requires its saved receipts, not another BUY"
            );
        }
        let mut snapshot = chain
            .inference_orderbook_snapshot(&book, &frame, &hash)
            .await
            .unwrap();
        for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
            if snapshot
                .orders
                .iter()
                .any(|ask| ask.token_contract.as_deref() == Some(&tc.with_workchain()))
            {
                break;
            }
            tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
            snapshot = chain
                .inference_orderbook_snapshot(&book, &frame, &hash)
                .await
                .unwrap();
        }
        assert_eq!(account_ecc(&chain, &tc, true).await, 0);
        let account = chain
            .get_system_account_retrying(&tc)
            .await
            .unwrap()
            .unwrap();
        println!(
            "2196 negative fixture {} native={} ECC[2]=0",
            tc.with_workchain(),
            account.balance
        );
        let error = chain
            .submit_safe_model_buy_ask(&snapshot, 2, SHELL_UNIT)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ECC[2] reserve 0"), "{error}");
        let (listed, reason) = chain
            .submit_safe_executable_book_asks(&snapshot, 2, SHELL_UNIT)
            .await
            .unwrap();
        assert!(listed.is_empty() && reason.unwrap().contains("ECC[2] reserve 0"));
        assert!(chain
            .require_tc_gas(&tc, TcGasOperation::Stop(&buyer))
            .await
            .is_err());
        chain
            .require_tc_gas(&tc, TcGasOperation::CleanupUnopened)
            .await
            .unwrap();

        if dispute {
            chain
                .giver_send_shell(&buyer.with_workchain(), DEAL_BURN_TERMINAL_RAW - 1)
                .await
                .unwrap();
            wait_ecc(&chain, &buyer, DEAL_BURN_TERMINAL_RAW - 1, true).await;
            assert!(chain
                .require_tc_gas(&tc, TcGasOperation::Dispute(&buyer))
                .await
                .is_err());
            chain
                .giver_send_shell(&buyer.with_workchain(), 1)
                .await
                .unwrap();
            wait_ecc(&chain, &buyer, DEAL_BURN_TERMINAL_RAW, true).await;
            chain
                .require_tc_gas(&tc, TcGasOperation::Stop(&buyer))
                .await
                .unwrap();
            chain
                .require_tc_gas(&tc, TcGasOperation::Dispute(&buyer))
                .await
                .unwrap();
            chain
                .note_fund_deploy_shell(
                    &buyer,
                    &buyer_keys,
                    parking_nonce,
                    0,
                    DEAL_BURN_TERMINAL_RAW,
                )
                .await
                .unwrap();
            wait_ecc_zero(&chain, &buyer).await;
            chain
                .giver_send_shell(&tc.with_workchain(), DEAL_BURN_SETTLE_RAW - 1)
                .await
                .unwrap();
            wait_ecc(&chain, &tc, DEAL_BURN_SETTLE_RAW - 1, true).await;
            assert!(chain
                .require_tc_gas(&tc, TcGasOperation::SettleWeek)
                .await
                .is_err());
            chain
                .giver_send_shell(&tc.with_workchain(), 1)
                .await
                .unwrap();
            wait_ecc(&chain, &tc, DEAL_BURN_SETTLE_RAW, true).await;
            chain
                .require_tc_gas(&tc, TcGasOperation::SettleWeek)
                .await
                .unwrap();
            chain
                .giver_send_shell(
                    &tc.with_workchain(),
                    DEAL_BURN_TERMINAL_RAW - DEAL_BURN_SETTLE_RAW,
                )
                .await
                .unwrap();
            wait_ecc(&chain, &tc, DEAL_BURN_TERMINAL_RAW, true).await;
            chain
                .require_tc_gas(&tc, TcGasOperation::Stop(&buyer))
                .await
                .unwrap();
            chain
                .require_tc_gas(&tc, TcGasOperation::Dispute(&buyer))
                .await
                .unwrap();
        }

        // Establish an exhausted-reserve deal on chain to exercise recovery itself.
        // The controlled fixture submits directly; normal buyers above correctly refuse it.
        let buy_charge = BUY_ORDER_GAS_RAW + DEAL_BURN_FUND_DEAL_RAW;
        chain
            .giver_send_shell(&buyer.with_workchain(), buy_charge)
            .await
            .unwrap();
        wait_ecc(&chain, &buyer, buy_charge, true).await;
        if dispute {
            chain
                .note_fund_deal(&seller, &seller_keys, nonce, 0, 2 * SHELL_UNIT)
                .await
                .unwrap();
        }
        let buyer_before = seller_note_spendable_shell(&chain, &buyer).await.unwrap();
        chain
            .place_inference_buy(
                &buyer,
                &buyer_keys,
                &hash,
                SHELL_UNIT,
                2,
                crate::market::required_escrow_for_buy(2, SHELL_UNIT),
                crate::market::flags::AON | crate::market::flags::FOK,
                default_buy_deadline(now_secs().unwrap()).unwrap(),
            )
            .await
            .unwrap();
        wait_state(&chain, &tc, false).await;
        let backend = RealBuyerBackend::new(
            RealChainBackend::connect(manifest_path().unwrap()).unwrap(),
            buyer.clone(),
            KeyPair::from_secret_hex(buyer_keys.secret_hex()).unwrap(),
            hash.clone(),
            TICK_SIZE,
            SHELL_UNIT,
            2,
            crate::market::required_escrow_for_buy(2, SHELL_UNIT),
        );
        if dispute {
            for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
                if chain
                    .token_contract_deal_buyer_bond(&tc)
                    .await
                    .unwrap()
                    .is_some_and(|b| b.bond_held == 2 * SHELL_UNIT)
                {
                    break;
                }
                tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
            }
            chain
                .open_stream(&tc, &seller_keys, b"controlled exit fixture")
                .await
                .unwrap();
            wait_state(&chain, &tc, true).await;
            chain
                .giver_send_shell(&buyer.with_workchain(), DEAL_BURN_TERMINAL_RAW)
                .await
                .unwrap();
            wait_ecc(&chain, &buyer, DEAL_BURN_TERMINAL_RAW, true).await;
            backend
                .dispute(
                    &tc.with_workchain(),
                    &RealNote::from_secret_hex(buyer_keys.secret_hex()).unwrap(),
                )
                .await
                .unwrap();
            let have = account_ecc(&chain, &tc, true).await;
            if have < DEAL_BURN_TERMINAL_RAW {
                chain
                    .giver_send_shell(&tc.with_workchain(), DEAL_BURN_TERMINAL_RAW - have)
                    .await
                    .unwrap();
                wait_ecc(&chain, &tc, DEAL_BURN_TERMINAL_RAW, true).await;
            }
            let release = chain.release_dispute(&tc, &seller_keys).await;
            chain.wait_deal_destroyed(&tc).await.unwrap();
            if let Err(error) = release {
                // Ordinary releaseDispute emits released=false; the existing client matcher
                // expects true. Verify the teardown by fact without retrying that money call.
                let receipts = chain.token_contract_settlement_receipts(&tc).await.unwrap();
                assert!(receipts.events.iter().any(|r| matches!(
                    r.event,
                    TokenContractSettlementEvent::DisputeResolved {
                        released: false,
                        ..
                    }
                )));
                println!("2196 teardown confirmed by ordinary DisputeResolved receipt: {error}");
            }
        } else {
            assert_eq!(account_ecc(&chain, &tc, true).await, 0);
            backend
                .cleanup_unopened(&tc.with_workchain())
                .await
                .unwrap();
            for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
                if seller_note_spendable_shell(&chain, &buyer).await.unwrap() == buyer_before {
                    break;
                }
                tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
            }
            assert_eq!(
                seller_note_spendable_shell(&chain, &buyer).await.unwrap(),
                buyer_before,
                "zero-ECC cleanup must return both escrow and buyer bond"
            );
            println!("2196 zero-ECC cleanup restored buyer trading balance {buyer_before}");
            retire_idle_cleanup_fixture(&chain, &tc, &seller_keys).await;
        }
        let receipts = chain.token_contract_settlement_receipts(&tc).await.unwrap();
        fs::write(
            dir.join(format!("exit-{offset}-receipts.txt")),
            format!("{receipts:#?}"),
        )
        .unwrap();
        println!(
            "2196 live exit dispute={dispute} receipts={:?}",
            receipts.events
        );
    }
    chain
        .close_unsold_deal(&parking, &buyer_keys)
        .await
        .unwrap();
    chain.wait_deal_destroyed(&parking).await.unwrap();
}

fn private_write(path: &Path, value: &[u8]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(value).unwrap();
}

async fn account_ecc(chain: &RealChainBackend, address: &Address, system_dapp: bool) -> u128 {
    let account = if system_dapp {
        chain.get_system_account_retrying(address).await
    } else {
        chain.get_account_retrying(address).await
    }
    .unwrap()
    .expect("funded live account exists in its DApp");
    account.ecc_balance(crate::params::SHELL_CURRENCY_ID)
}

async fn wait_ecc(chain: &RealChainBackend, address: &Address, required: u128, system_dapp: bool) {
    for _ in 0..crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if account_ecc(chain, address, system_dapp).await >= required {
            return;
        }
        tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
    }
    panic!(
        "funding did not reach {}: {required} raw ECC[2]",
        address.with_workchain()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit shellnet manifest, giver env, built CLI and a persistent private state directory"]
async fn live_2196_prepare_two_notes_and_large_offer() {
    let chain = RealChainBackend::connect(crate::params::manifest_path().unwrap()).unwrap();
    assert_eq!(
        chain.network(),
        "shellnet",
        "this test must never spend on mainnet"
    );
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2196_STATE_DIR").unwrap());
    assert!(dir.is_absolute() && dir.is_dir());
    let bin = std::env::var("DEXDO_2196_BIN").unwrap();
    let key_path = dir.join("funding.secret.hex");
    let keys = if key_path.exists() {
        KeyPair::from_secret_hex(fs::read_to_string(&key_path).unwrap().trim()).unwrap()
    } else {
        let keys = KeyPair::generate();
        private_write(&key_path, keys.secret_hex().as_bytes());
        keys
    };
    let wallet_path = dir.join("funding-address.json");
    let wallet = if wallet_path.exists() {
        Address::parse(fs::read_to_string(&wallet_path).unwrap().trim()).unwrap()
    } else {
        let wallet = chain.deploy_multisig(&keys).await.unwrap();
        private_write(&wallet_path, wallet.with_workchain().as_bytes());
        wallet
    };
    for (role, nominal, shells) in [("seller", "N100", 100u128), ("buyer", "N100000", 100_000)] {
        let pool_path = dir.join(format!("{role}.pool.json"));
        if pool_path.exists() {
            continue;
        }
        let funding = crate::params::note_deploy_wallet_funding_raw(
            (shells * crate::params::SHELL_UNIT).try_into().unwrap(),
        );
        let have = account_ecc(&chain, &wallet, false).await;
        if have < funding {
            chain
                .giver_send_shell(&wallet.with_workchain(), funding - have)
                .await
                .unwrap();
            wait_ecc(&chain, &wallet, funding, false).await;
        }
        chain
            .giver_fund(
                &wallet.with_workchain(),
                crate::params::FUNDING_WALLET_NATIVE_FLOOR_RAW,
            )
            .await
            .unwrap();
        println!(
            "2196 mint {role} {nominal}; funding wallet {}",
            wallet.with_workchain()
        );
        let output = Command::new(&bin)
            .current_dir(&dir)
            .env("HALO2_FIXTURE_DIR", dir.join("halo2-fixtures"))
            .arg("--data-dir")
            .arg(dir.join(format!("{role}-deploy")))
            .args([
                "note",
                "deploy",
                "--multisig-address",
                &wallet.with_workchain(),
                "--multisig-private-key",
            ])
            .arg(&key_path)
            .arg("--pool")
            .arg(&pool_path)
            .args(["--nominal", nominal, "--json"])
            .env_remove("DEXDO_PN_POOL")
            .output()
            .unwrap();
        let mut log = output.stderr;
        log.extend_from_slice(&output.stdout);
        fs::write(dir.join(format!("{role}-deploy.log")), log).unwrap();
        assert!(
            output.status.success(),
            "{role} mint failed; inspect private deploy log"
        );
    }
    let (buyer, _) = role(&dir, "buyer");
    let buy_charge = crate::params::BUY_ORDER_GAS_RAW + crate::params::DEAL_BURN_FUND_DEAL_RAW;
    let have = account_ecc(&chain, &buyer, true).await;
    if have < buy_charge {
        chain
            .giver_send_shell(&buyer.with_workchain(), buy_charge - have)
            .await
            .unwrap();
        wait_ecc(&chain, &buyer, buy_charge, true).await;
    }
    let pool: Value =
        serde_json::from_slice(&fs::read(dir.join("seller.pool.json")).unwrap()).unwrap();
    let seller = &pool["notes"][0];
    let seller_note = crate::address::parse_chain_address(seller["address"].as_str().unwrap())
        .unwrap()
        .into_chain();
    let seller_keys =
        KeyPair::from_secret_hex(seller["owner_secret_key_hex"].as_str().unwrap()).unwrap();
    let market_path = dir.join("large.market.json");
    let market: crate::MarketManifest = if market_path.exists() {
        crate::MarketManifest::from_json(&fs::read_to_string(&market_path).unwrap()).unwrap()
    } else {
        let nonce = now_secs().unwrap();
        let gas = crate::params::default_deposit_shells(57_500) * crate::params::SHELL_UNIT;
        let have = account_ecc(&chain, &seller_note, true).await;
        if have < gas {
            chain
                .giver_send_shell(&seller_note.with_workchain(), gas - have)
                .await
                .unwrap();
            wait_ecc(&chain, &seller_note, gas, true).await;
        }
        let market = chain
            .provision_market(
                &seller_keys,
                &seller_note,
                &format!("gas-2196-{nonce}"),
                nonce,
                crate::params::SHELL_UNIT,
                57_500,
                gas,
            )
            .await
            .unwrap();
        fs::write(&market_path, market.to_json().unwrap()).unwrap();
        market
    };
    let tc = Address::parse(&market.token_contract).unwrap();
    let nonce = market.nonce;
    let book = Address::parse(&market.inference_order_book).unwrap();
    chain
        .post_sell_offer(
            &seller_note,
            &seller_keys,
            0,
            nonce,
            crate::params::MAX_SELL_TTL.as_secs(),
        )
        .await
        .unwrap();
    let mut snapshot = chain
        .inference_orderbook_snapshot(&book, &market.frame_model, &market.model_hash)
        .await
        .unwrap();
    for _ in 0..crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if !snapshot.orders.is_empty() {
            break;
        }
        tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
        snapshot = chain
            .inference_orderbook_snapshot(&book, &market.frame_model, &market.model_hash)
            .await
            .unwrap();
    }
    let account = chain
        .get_system_account_retrying(&tc)
        .await
        .unwrap()
        .unwrap();
    let terms = chain.token_contract_deal_terms(&tc).await.unwrap().unwrap();
    let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
    println!(
        "2196 LIVE TC={} terms={terms:?} state={state:?} native={} ecc2={}",
        tc.with_workchain(),
        account.balance,
        account.ecc_balance(crate::params::SHELL_CURRENCY_ID)
    );
    assert!(account.balance < crate::params::deal_gas_reserve_raw(terms.2));
    chain
        .require_tc_gas(&tc, TcGasOperation::Buy)
        .await
        .unwrap();
    let selected = chain
        .submit_safe_model_buy_ask(&snapshot, 57_500, crate::params::SHELL_UNIT)
        .await
        .unwrap();
    let (listed, reason) = chain
        .submit_safe_executable_book_asks(&snapshot, 57_500, crate::params::SHELL_UNIT)
        .await
        .unwrap();
    assert!(reason.is_none() && listed.iter().any(|ask| ask.order_id == selected.order_id));
    fs::write(dir.join("large-before.json"), serde_json::to_vec_pretty(&json!({
        "token_contract": tc.with_workchain(), "terms": terms, "native": account.balance.to_string(),
        "ecc2": account.ecc_balance(crate::params::SHELL_CURRENCY_ID).to_string(), "order_id": selected.order_id.to_string(),
    })).unwrap()).unwrap();
}
