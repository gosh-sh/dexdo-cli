// Reuses the existing two-note setup, receipt readers and bounded chain waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "shellnet only; dedicated two-note state, explicit giver, no other note users"]
async fn live_2220_buyer_note_pays_bond_and_each_supported_exit() {
    use crate::params::*;
    let manifest = manifest_path().unwrap();
    let chain = RealChainBackend::connect(&manifest).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2220_STATE_DIR").unwrap());
    let (seller_note, seller_keys) = role(&dir, "seller");
    let (buyer_note, buyer_keys) = role(&dir, "buyer");
    let first_nonce: u64 = fs::read_to_string(dir.join("nonce.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let local_note = LocalNote::generate();
    let mut parking = None;

    for (offset, exit) in ["stop", "dispute", "cleanup"].into_iter().enumerate() {
        let nonce = first_nonce + offset as u64;
        let frame = format!("gas-2220-{nonce}");
        let market_path = dir.join(format!("{exit}.market.json"));
        assert!(
            !market_path.exists(),
            "resume from saved state, never place a duplicate BUY"
        );
        let market = chain
            .provision_market_with_deal_gas_overhead_and_before_deploy(
                &seller_keys,
                &seller_note,
                &frame,
                nonce,
                SHELL_UNIT,
                8,
                deal_gas_reserve_raw(8),
                0,
                |market| fs::write(&market_path, market.to_json()?).map_err(Into::into),
            )
            .await
            .unwrap();
        let tc = Address::parse(&market.token_contract).unwrap();
        let seller = RealSellerBackend::new(
            RealChainBackend::connect(&manifest).unwrap(),
            seller_note.clone(),
            KeyPair::from_secret_hex(seller_keys.secret_hex()).unwrap(),
            model_hash_for(&frame),
            frame,
            nonce,
            TICK_SIZE,
        );
        seller
            .post_offer(
                SellOffer {
                    price_per_tick: SHELL_UNIT.try_into().unwrap(),
                    max_ticks: 8,
                    token_contract: tc.with_workchain(),
                    flags: 0,
                },
                &local_note,
            )
            .await
            .unwrap();

        let buy_gas = buyer_order_gas_raw(8, true).unwrap();
        if offset == 0 {
            // Park the buyer's surplus in its own idle TC, retaining exactly the
            // placement+bond charge. This makes a real zero-pocket negative case.
            let root = chain
                .deploy_root_model_note_funded(&buyer_keys)
                .await
                .unwrap();
            let parking_nonce = first_nonce + 3;
            let addr = chain
                .token_contract_deploy_address(&buyer_keys, &root, parking_nonce)
                .await
                .unwrap();
            private_write(
                &dir.join("parking-address.txt"),
                addr.with_workchain().as_bytes(),
            );
            let have = account_ecc(&chain, &buyer_note, true).await;
            let gas = have.checked_sub(buy_gas).unwrap();
            assert!(gas >= DEAL_BURN_DEPLOY_RAW + DEAL_BURN_TERMINAL_RAW);
            chain
                .note_deploy_deal(
                    &buyer_note,
                    &buyer_keys,
                    crate::chain::client::NoteDeployDealRequest {
                        nonce: parking_nonce,
                        model_name: "gas-2220-parking",
                        price_per_tick: SHELL_UNIT,
                        max_ticks: MIN_STREAM_BUY_TICKS,
                        gas_reserve: gas,
                    },
                )
                .await
                .unwrap();
            wait_active(&chain, &addr).await;
            live_2199_wait_ecc(&chain, &buyer_note, buy_gas).await;
            parking = Some(addr);
        } else {
            chain
                .giver_send_shell(
                    &buyer_note.with_workchain(),
                    buy_gas + DEAL_BURN_TERMINAL_RAW,
                )
                .await
                .unwrap();
            live_2199_wait_ecc(&chain, &buyer_note, buy_gas + DEAL_BURN_TERMINAL_RAW).await;
        }
        let note_before_buy = account_ecc(&chain, &buyer_note, true).await;
        let tc_before_buy = account_ecc(&chain, &tc, true).await;
        chain
            .place_inference_buy(
                &buyer_note,
                &buyer_keys,
                &market.model_hash,
                SHELL_UNIT,
                8,
                crate::market::required_escrow_for_buy(8, SHELL_UNIT),
                crate::market::flags::AON | crate::market::flags::FOK,
                default_buy_deadline(now_secs().unwrap()).unwrap(),
            )
            .await
            .unwrap();
        wait_state(&chain, &tc, false).await;
        live_2199_wait_ecc(&chain, &buyer_note, note_before_buy - buy_gas).await;
        live_2199_wait_ecc(&chain, &tc, tc_before_buy).await;
        println!("2220 {exit}: BUY/bond note debit={buy_gas}, TC reserve unchanged");

        if exit != "cleanup" {
            seller
                .open_stream(
                    &tc.with_workchain(),
                    b"explicit mock consumption".to_vec(),
                    &local_note,
                )
                .await
                .unwrap();
            if exit == "stop" {
                let before = account_ecc(&chain, &tc, true).await;
                for result in [
                    chain
                        .stream_stop(&buyer_note, &buyer_keys, &tc)
                        .await
                        .map(|_| ()),
                    chain
                        .stream_dispute(&buyer_note, &buyer_keys, &tc)
                        .await
                        .map(|_| ()),
                    chain
                        .stream_cleanup(&buyer_note, &buyer_keys, &tc)
                        .await
                        .map(|_| ()),
                ] {
                    assert!(format!("{:#}", result.unwrap_err()).contains("Nothing was sent"));
                }
                assert_eq!(account_ecc(&chain, &tc, true).await, before);
                chain
                    .giver_send_shell(&buyer_note.with_workchain(), DEAL_BURN_TERMINAL_RAW)
                    .await
                    .unwrap();
                live_2199_wait_ecc(&chain, &buyer_note, DEAL_BURN_TERMINAL_RAW).await;
                let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
                let bounds = seller
                    .deal_claim_bounds(&tc.with_workchain())
                    .await
                    .unwrap();
                live_2199_wait_time(&chain, state.probe_time + bounds.probe_window.as_secs()).await;
                seller.accept_probe(&tc.with_workchain()).await.unwrap();
                for ticks in [2, 3] {
                    let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
                    live_2199_wait_time(
                        &chain,
                        state.last_claim_time
                            + bounds
                                .min_claim_interval
                                .as_secs()
                                .max(bounds.min_seconds_per_tick.as_secs()),
                    )
                    .await;
                    seller
                        .claim_tokens(&tc.with_workchain(), &local_note, ticks * TICK_SIZE)
                        .await
                        .unwrap();
                }
            }
        }

        let note_before_exit = account_ecc(&chain, &buyer_note, true).await;
        match exit {
            "stop" => {
                chain
                    .stream_stop(&buyer_note, &buyer_keys, &tc)
                    .await
                    .unwrap();
            }
            "dispute" => {
                let before = account_ecc(&chain, &tc, true).await;
                chain
                    .stream_dispute(&buyer_note, &buyer_keys, &tc)
                    .await
                    .unwrap();
                live_2199_wait_ecc(&chain, &tc, before).await;
                // Seller-only concession cannot be paid through the buyer note.
                let _ = chain.release_dispute(&tc, &seller_keys).await;
            }
            _ => {
                chain
                    .stream_cleanup(&buyer_note, &buyer_keys, &tc)
                    .await
                    .unwrap();
            }
        }
        live_2199_wait_ecc(
            &chain,
            &buyer_note,
            note_before_exit - DEAL_BURN_TERMINAL_RAW,
        )
        .await;
        if exit == "cleanup" {
            for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
                if chain
                    .token_contract_deal_state(&tc)
                    .await
                    .unwrap()
                    .is_some_and(|state| !state.funded)
                {
                    break;
                }
                tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
            }
            assert!(
                !chain
                    .token_contract_deal_state(&tc)
                    .await
                    .unwrap()
                    .unwrap()
                    .funded
            );
            chain.close_unsold_deal(&tc, &seller_keys).await.unwrap();
        }
        chain.wait_deal_destroyed(&tc).await.unwrap();
        let receipts = chain.token_contract_settlement_receipts(&tc).await.unwrap();
        if exit == "stop" {
            assert!(receipts
                .events
                .iter()
                .any(|r| matches!(r.event, TokenContractSettlementEvent::ProbeAccepted { .. })));
            assert!(
                receipts
                    .events
                    .iter()
                    .filter(|r| matches!(
                        r.event,
                        TokenContractSettlementEvent::TicksClaimed { .. }
                    ))
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
        } else if exit == "dispute" {
            assert!(receipts
                .events
                .iter()
                .any(|r| matches!(r.event, TokenContractSettlementEvent::StreamDisputed { .. })));
            assert!(receipts.events.iter().any(|r| matches!(
                r.event,
                TokenContractSettlementEvent::DisputeResolved { .. }
            )));
        }
        fs::write(
            dir.join(format!("{exit}.receipts.txt")),
            format!("{receipts:#?}"),
        )
        .unwrap();
        println!(
            "2220 {exit}: note exit debit={}, receipts={receipts:?}",
            DEAL_BURN_TERMINAL_RAW
        );
    }
    let parking = parking.unwrap();
    chain
        .close_unsold_deal(&parking, &buyer_keys)
        .await
        .unwrap();
    chain.wait_deal_destroyed(&parking).await.unwrap();
    for note in [&seller_note, &buyer_note] {
        let outstanding = chain
            .run_getter_retrying(note, PRIVATENOTE_ABI, "getOutstanding", json!({}))
            .await
            .unwrap()
            .unwrap();
        assert!(outstanding["deals"].as_array().unwrap().is_empty());
        assert!(outstanding["orders"].as_array().unwrap().is_empty());
    }
}
