// Reuse live_2199_prepare_two_notes with this task's own state directory and explicit shellnet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "shellnet only; dedicated seller/buyer pools, explicit giver, reserve rejection and clean settlement"]
async fn live_2230_replenished_sell_recovers_without_duplicate_and_settles() {
    use crate::params::*;
    let manifest = manifest_path().unwrap();
    let chain = RealChainBackend::connect(&manifest).unwrap();
    assert_eq!(chain.network(), "shellnet", "never spend on mainnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2230_STATE_DIR").unwrap());
    let (seller_note, seller_keys) = role(&dir, "seller");
    let (buyer_note, buyer_keys) = role(&dir, "buyer");
    let nonce_path = dir.join("nonce.txt");
    let nonce = if nonce_path.exists() {
        fs::read_to_string(&nonce_path)
            .unwrap()
            .parse::<u64>()
            .unwrap()
    } else {
        let nonce = now_secs().unwrap();
        fs::write(&nonce_path, nonce.to_string()).unwrap();
        nonce
    };
    let frame = format!("reserve-recovery-2230-{nonce}");
    let price = 3 * SHELL_UNIT;
    let ticks = 8;
    let market_path = dir.join("market.json");
    let market = if market_path.exists() {
        crate::MarketManifest::from_json(&fs::read_to_string(&market_path).unwrap()).unwrap()
    } else {
        // Constructor burns exactly this reserve, leaving zero for postFromNote.
        chain
            .provision_market_with_deal_gas_overhead_and_before_deploy(
                &seller_keys,
                &seller_note,
                &frame,
                nonce,
                price,
                ticks,
                DEAL_BURN_DEPLOY_RAW,
                0,
                |market| fs::write(&market_path, market.to_json()?).map_err(Into::into),
            )
            .await
            .unwrap()
    };
    let tc = Address::parse(&market.token_contract).unwrap();
    let seller = || {
        RealSellerBackend::new(
            RealChainBackend::connect(&manifest).unwrap(),
            seller_note.clone(),
            KeyPair::from_secret_hex(seller_keys.secret_hex()).unwrap(),
            market.model_hash.clone(),
            frame.clone(),
            nonce,
            TICK_SIZE,
        )
        .with_offer_journal_dir(dir.join("sell-journal"))
    };
    let local_note = LocalNote::generate();
    let offer = || SellOffer {
        token_contract: tc.with_workchain(),
        price_per_tick: price.try_into().unwrap(),
        max_ticks: ticks.try_into().unwrap(),
        flags: crate::market::flags::AON,
    };
    let backend = seller();
    if let Some(state) = chain.token_contract_deal_state(&tc).await.unwrap() {
        if !state.funded {
            let rejection = dir.join("original-rejection.txt");
            if !rejection.exists() {
                live_2199_wait_ecc(&chain, &tc, 0).await;
                if !backend.offer_journal_path(&tc).unwrap().exists() {
                    backend.post_offer(offer(), &local_note).await.unwrap();
                }
                let book = Address::parse(&market.inference_order_book).unwrap();
                let mut evidence = String::new();
                for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
                    if let Err(error) = backend.reconcile_pending_offer(&tc, &book).await {
                        evidence = error.to_string();
                        if evidence.contains("no_funds=true") {
                            break;
                        }
                    }
                    tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
                }
                assert!(evidence.contains("no_funds=true"), "{evidence}");
                fs::write(&rejection, evidence).unwrap();
                println!("2230 finalized reserve rejection retained; zero-reserve retry refused");
            }
            let target = deal_gas_reserve_raw(ticks);
            let have = account_ecc(&chain, &tc, true).await;
            if have == 0 {
                let error = backend.post_offer(offer(), &local_note).await.unwrap_err();
                assert!(
                    error.to_string().contains("current_reserve_raw=0"),
                    "{error}"
                );
            }
            if have < target {
                chain
                    .giver_send_shell(&tc.with_workchain(), target - have)
                    .await
                    .unwrap();
                wait_ecc(&chain, &tc, target, true).await;
            }
            backend.post_offer(offer(), &local_note).await.unwrap();
            assert!(matches!(
                backend
                    .confirm_offer_outcome(&tc.with_workchain())
                    .await
                    .unwrap(),
                Some(crate::market::SellOfferOutcome::Rested { .. })
            ));
            let orders = backend
                .raw_resting_sell_orders_for_tc(&tc.with_workchain())
                .await
                .unwrap();
            assert_eq!(orders.len(), 1);
            assert_eq!(
                (orders[0].price_per_tick, orders[0].ticks, orders[0].flags),
                (price, ticks, crate::market::flags::AON)
            );
            let path = backend.offer_journal_path(&tc).unwrap();
            let active: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let restarted = seller();
            restarted.post_offer(offer(), &local_note).await.unwrap();
            let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(active["message_id"], after["message_id"]);
            assert_eq!(
                restarted
                    .raw_resting_sell_orders_for_tc(&tc.with_workchain())
                    .await
                    .unwrap()
                    .len(),
                1
            );
            fs::write(dir.join("recovered-order.txt"), format!("{orders:#?}")).unwrap();
            println!("2230 recovered original AON terms; one resting SELL across restart");
            chain
                .place_inference_buy(
                    &buyer_note,
                    &buyer_keys,
                    &market.model_hash,
                    price,
                    ticks,
                    crate::market::required_escrow_for_buy(ticks, price),
                    crate::market::flags::AON | crate::market::flags::FOK,
                    default_buy_deadline(now_secs().unwrap()).unwrap(),
                )
                .await
                .unwrap();
            wait_state(&chain, &tc, false).await;
        }
        let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
        if !state.opened {
            backend
                .open_stream(&tc.with_workchain(), vec![1], &local_note)
                .await
                .unwrap();
        }
        let bounds = backend
            .deal_claim_bounds(&tc.with_workchain())
            .await
            .unwrap();
        let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
        if !state.probe_accepted {
            println!("2230 waiting for probe window");
            live_2199_wait_time(&chain, state.probe_time + bounds.probe_window.as_secs()).await;
            backend.accept_probe(&tc.with_workchain()).await.unwrap();
        }
        for count in [2, 3] {
            let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
            if state.tokens_pending >= count * TICK_SIZE {
                continue;
            }
            live_2199_wait_time(
                &chain,
                state.last_claim_time
                    + bounds
                        .min_claim_interval
                        .as_secs()
                        .max(bounds.min_seconds_per_tick.as_secs()),
            )
            .await;
            backend
                .claim_tokens(&tc.with_workchain(), &local_note, count * TICK_SIZE)
                .await
                .unwrap();
            println!("2230 accepted claim {count}");
        }
        chain
            .stream_stop(&buyer_note, &buyer_keys, &tc)
            .await
            .unwrap();
        chain.wait_deal_destroyed(&tc).await.unwrap();
    }
    // Also revalidate the archived original receipt using the final BOC correlation predicate.
    let archive = fs::read_dir(dir.join("sell-journal"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".rejected.json"))
        .unwrap();
    let original = crate::chain::client::SellerPlacement::load(&archive)
        .unwrap()
        .unwrap();
    chain
        .verified_seller_reserve_rejection(&original, crate::market::flags::AON, nonce)
        .await
        .unwrap();
    let receipts = chain.token_contract_settlement_receipts(&tc).await.unwrap();
    assert!(receipts
        .events
        .iter()
        .any(|r| matches!(r.event, TokenContractSettlementEvent::ProbeAccepted { .. })));
    assert!(
        receipts
            .events
            .iter()
            .filter(|r| matches!(r.event, TokenContractSettlementEvent::TicksClaimed { .. }))
            .count()
            >= 2
    );
    assert!(receipts
        .events
        .iter()
        .any(|r| matches!(r.event, TokenContractSettlementEvent::StreamStopped { .. })));
    assert!(!receipts.events.iter().any(|r| matches!(
        r.event,
        TokenContractSettlementEvent::ProbeBurned { .. }
            | TokenContractSettlementEvent::StreamDisputed { .. }
            | TokenContractSettlementEvent::DisputeResolved { .. }
    )));
    fs::write(
        dir.join("settlement-receipts.txt"),
        format!("{receipts:#?}"),
    )
    .unwrap();
    println!("2230 clean AmicableSplit confirmed");
}
