#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "shellnet only; mints exactly two N100 notes using the explicit giver and CLI"]
async fn live_2199_prepare_two_notes() {
    let chain = RealChainBackend::connect(crate::params::manifest_path().unwrap()).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2199_STATE_DIR").unwrap());
    assert!(dir.is_absolute() && dir.is_dir());
    let key_path = dir.join("funding.secret.hex");
    let keys = if key_path.exists() {
        KeyPair::from_secret_hex(fs::read_to_string(&key_path).unwrap().trim()).unwrap()
    } else {
        let keys = KeyPair::generate();
        private_write(&key_path, keys.secret_hex().as_bytes());
        keys
    };
    let wallet_path = dir.join("funding-address.txt");
    let wallet = if wallet_path.exists() {
        Address::parse(fs::read_to_string(&wallet_path).unwrap().trim()).unwrap()
    } else {
        let wallet = chain.deploy_multisig(&keys).await.unwrap();
        private_write(&wallet_path, wallet.with_workchain().as_bytes());
        wallet
    };
    for name in ["seller", "buyer"] {
        let pool = dir.join(format!("{name}.pool.json"));
        if pool.exists() {
            continue;
        }
        let funding = crate::params::note_deploy_wallet_funding_raw(
            (100 * crate::params::SHELL_UNIT).try_into().unwrap(),
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
        let output = Command::new(std::env::var("DEXDO_2199_BIN").unwrap())
            .current_dir(&dir)
            .env("HALO2_FIXTURE_DIR", dir.join("halo2-fixtures"))
            .env_remove("DEXDO_PN_POOL")
            .arg("--data-dir")
            .arg(dir.join(format!("{name}-deploy")))
            .args([
                "note",
                "deploy",
                "--multisig-address",
                &wallet.with_workchain(),
                "--multisig-private-key",
            ])
            .arg(&key_path)
            .arg("--pool")
            .arg(&pool)
            .args(["--nominal", "N100", "--json"])
            .output()
            .unwrap();
        let mut log = output.stderr;
        log.extend_from_slice(&output.stdout);
        fs::write(dir.join(format!("{name}-deploy.log")), log).unwrap();
        assert!(
            output.status.success(),
            "{name} mint failed; inspect its private deploy log"
        );
        println!("2199 minted {name} N100");
    }
}

async fn live_2199_wait_ecc(chain: &RealChainBackend, tc: &Address, expected: u128) {
    for _ in 0..crate::params::ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
        if account_ecc(chain, tc, true).await == expected {
            return;
        }
        tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
    }
    panic!("2199 expected ECC[2]={expected} at {tc}");
}

async fn live_2199_wait_time(chain: &RealChainBackend, deadline: u64) {
    while chain.observed_chain_timestamp().await.unwrap() < deadline {
        tokio::time::sleep(crate::params::ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "focused shellnet lifecycle and two exhausted-reserve rejections; dedicated two-note state only"]
async fn live_2199_reserve_consumption_rejections_and_clean_settlement() {
    use crate::params::*;
    let manifest = manifest_path().unwrap();
    let chain = RealChainBackend::connect(&manifest).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2199_STATE_DIR").unwrap());
    let (seller_note, seller_keys) = role(&dir, "seller");
    let (buyer_note, buyer_keys) = role(&dir, "buyer");
    let nonce_path = dir.join("nonce.txt");
    let nonce: u64 = if nonce_path.exists() {
        fs::read_to_string(&nonce_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    } else {
        let nonce = now_secs().unwrap();
        fs::write(&nonce_path, nonce.to_string()).unwrap();
        nonce
    };
    // Reproduce the incident's terms, but purchase only eight ticks from this dedicated book.
    let max_ticks = 57_500;
    let frame = format!("gas-2199-{nonce}");
    let gas = default_deposit_shells(max_ticks) * SHELL_UNIT;
    let floor = deal_gas_requirement_raw(max_ticks);
    let seller = RealSellerBackend::new(
        RealChainBackend::connect(&manifest).unwrap(),
        seller_note.clone(),
        KeyPair::from_secret_hex(seller_keys.secret_hex()).unwrap(),
        model_hash_for(&frame),
        frame.clone(),
        nonce,
        TICK_SIZE,
    )
    .with_offer_journal_dir(dir.join("sell-journal"));
    let market_path = dir.join("market.json");
    let market = if market_path.exists() {
        crate::MarketManifest::from_json(&fs::read_to_string(&market_path).unwrap()).unwrap()
    } else {
        let have = account_ecc(&chain, &seller_note, true).await;
        assert!(
            have <= gas,
            "dedicated note has more ECC than this fixture needs"
        );
        if have < gas {
            chain
                .giver_send_shell(&seller_note.with_workchain(), gas - have)
                .await
                .unwrap();
            wait_ecc(&chain, &seller_note, gas, true).await;
        }
        chain
            .provision_market_with_deal_gas_overhead_and_before_deploy(
                &seller_keys,
                &seller_note,
                &frame,
                nonce,
                SHELL_UNIT,
                max_ticks,
                gas,
                0,
                |market| fs::write(&market_path, market.to_json()?).map_err(Into::into),
            )
            .await
            .unwrap()
    };
    let tc = Address::parse(&market.token_contract).unwrap();
    if let Some(state) = chain.token_contract_deal_state(&tc).await.unwrap() {
        live_2199_wait_ecc(&chain, &seller_note, 0).await;
        let note = LocalNote::generate();
        if !state.funded {
            seller
                .post_offer(
                    SellOffer {
                        price_per_tick: SHELL_UNIT.try_into().unwrap(),
                        max_ticks: max_ticks.try_into().unwrap(),
                        token_contract: tc.with_workchain(),
                        flags: 0,
                    },
                    &note,
                )
                .await
                .unwrap();
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
        }
        if !chain
            .token_contract_deal_state(&tc)
            .await
            .unwrap()
            .unwrap()
            .opened
        {
            seller
                .open_stream(&tc.with_workchain(), vec![1], &note)
                .await
                .unwrap();
        }
        let bounds = seller
            .deal_claim_bounds(&tc.with_workchain())
            .await
            .unwrap();
        let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
        if !state.probe_accepted {
            println!(
                "2199 opened {}; waiting for the contract probe window",
                tc.with_workchain()
            );
            live_2199_wait_time(&chain, state.probe_time + bounds.probe_window.as_secs()).await;
            seller.accept_probe(&tc.with_workchain()).await.unwrap();
        }
        let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
        if state.tokens_pending == TICK_SIZE {
            // Reach the exact former floor by measured GAS_LIGHT burns, with no guessed threshold.
            let have = account_ecc(&chain, &tc, true).await;
            assert!(have >= floor && (have - floor).is_multiple_of(DEAL_BURN_LIGHT_RAW));
            for index in 0..(have - floor) / DEAL_BURN_LIGHT_RAW {
                chain.withdraw_shell(&tc, 1, &seller_keys).await.unwrap();
                live_2199_wait_ecc(&chain, &tc, have - (index + 1) * DEAL_BURN_LIGHT_RAW).await;
            }
            // Idempotent provisioning/resume must not restore the original reserve from an empty note.
            chain
                .provision_market(
                    &seller_keys,
                    &seller_note,
                    &frame,
                    nonce,
                    SHELL_UNIT,
                    max_ticks,
                    gas,
                )
                .await
                .unwrap();
            live_2199_wait_ecc(&chain, &tc, floor).await;
            println!("2199 exact floor {floor}; seller note ECC[2]=0");
        }
        for ticks in [2, 3] {
            let state = chain.token_contract_deal_state(&tc).await.unwrap().unwrap();
            let cumulative = ticks * TICK_SIZE;
            if state.tokens_pending >= cumulative {
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
            seller
                .claim_tokens(&tc.with_workchain(), &note, cumulative)
                .await
                .unwrap();
            assert_eq!(account_ecc(&chain, &seller_note, true).await, 0);
            assert!(account_ecc(&chain, &tc, true).await < floor);
            println!(
                "2199 claim {ticks}: reserve={} below former floor={floor}",
                account_ecc(&chain, &tc, true).await
            );
        }
        chain
            .stream_stop(&buyer_note, &buyer_keys, &tc)
            .await
            .unwrap();
        chain.wait_deal_destroyed(&tc).await.unwrap();
    }
    let receipts = chain.token_contract_settlement_receipts(&tc).await.unwrap();
    assert!(
        receipts
            .events
            .iter()
            .any(|r| matches!(r.event, TokenContractSettlementEvent::ProbeAccepted { .. }))
    );
    assert!(
        receipts
            .events
            .iter()
            .filter(|r| matches!(r.event, TokenContractSettlementEvent::TicksClaimed { .. }))
            .count()
            >= 2
    );
    assert!(
        receipts
            .events
            .iter()
            .any(|r| matches!(r.event, TokenContractSettlementEvent::StreamStopped { .. }))
    );
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
    println!(
        "2199 clean AmicableSplit confirmed for {}",
        tc.with_workchain()
    );

    for (offset, remainder) in [(1, 0), (2, DEAL_BURN_TERMINAL_RAW - 1)] {
        let negative_nonce = nonce + offset;
        let negative = chain
            .token_contract_deploy_address(
                &seller_keys,
                &Address::parse(&market.root_model).unwrap(),
                negative_nonce,
            )
            .await
            .unwrap();
        let address_path = dir.join(format!("exhausted-{offset}.txt"));
        if !address_path.exists() {
            fs::write(&address_path, negative.with_workchain()).unwrap();
            let needed = DEAL_BURN_DEPLOY_RAW + remainder;
            let have = account_ecc(&chain, &seller_note, true).await;
            if have < needed {
                chain
                    .giver_send_shell(&seller_note.with_workchain(), needed - have)
                    .await
                    .unwrap();
                wait_ecc(&chain, &seller_note, needed, true).await;
            }
            chain
                .note_deploy_deal(
                    &seller_note,
                    &seller_keys,
                    crate::chain::client::NoteDeployDealRequest {
                        nonce: negative_nonce,
                        model_name: &frame,
                        price_per_tick: SHELL_UNIT,
                        max_ticks: MIN_STREAM_BUY_TICKS,
                        gas_reserve: needed,
                    },
                )
                .await
                .unwrap();
            wait_active(&chain, &negative).await;
        }
        if chain
            .token_contract_deal_state(&negative)
            .await
            .unwrap()
            .is_some()
        {
            live_2199_wait_ecc(&chain, &negative, remainder).await;
            let error = chain
                .close_unsold_deal(&negative, &seller_keys)
                .await
                .unwrap_err();
            let evidence = format!("{error:#}");
            for fact in [
                negative.bare(),
                "close",
                "ECC[2]/SHELL reserve",
                "transaction_hash=",
                "settlement is unconfirmed",
            ] {
                assert!(evidence.contains(fact), "{evidence}");
            }
            assert!(
                evidence.contains(&format!("observed_reserve_raw={remainder}")),
                "{evidence}"
            );
            assert!(
                chain
                    .token_contract_deal_state(&negative)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert!(
                address_path.exists(),
                "recovery identity survives the rejection"
            );
            fs::write(
                dir.join(format!("exhausted-{offset}-receipt.txt")),
                evidence,
            )
            .unwrap();
            println!("2199 finalized TC reserve rejection: remaining={remainder} operation=close");
            // Explicit adversarial-fixture teardown, never a production automatic refill.
            chain
                .giver_send_shell(
                    &negative.with_workchain(),
                    DEAL_BURN_TERMINAL_RAW - remainder,
                )
                .await
                .unwrap();
            live_2199_wait_ecc(&chain, &negative, DEAL_BURN_TERMINAL_RAW).await;
            chain
                .close_unsold_deal(&negative, &seller_keys)
                .await
                .unwrap();
            chain.wait_deal_destroyed(&negative).await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "shellnet only; reuse the dedicated seller note for four internal reserve rejections"]
async fn live_2199_internal_note_calls_preserve_tc_reserve_diagnostics() {
    use crate::params::*;
    let manifest = manifest_path().unwrap();
    let chain = RealChainBackend::connect(&manifest).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2199_STATE_DIR").unwrap());
    let (note, keys) = role(&dir, "seller");
    let market =
        crate::MarketManifest::from_json(&fs::read_to_string(dir.join("market.json")).unwrap())
            .unwrap();
    assert!(chain.private_note_shell_balance(&note).await.unwrap() >= 2 * SHELL_UNIT);
    let output = dir.join("review-fixes");
    fs::create_dir_all(&output).unwrap();
    let nonce_path = output.join("nonce.txt");
    let nonce: u64 = if nonce_path.exists() {
        fs::read_to_string(&nonce_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    } else {
        let nonce = now_secs().unwrap();
        fs::write(&nonce_path, nonce.to_string()).unwrap();
        nonce
    };
    for (offset, operation, remainder) in [
        (0, "fundDeal", 0),
        (1, "fundDeal", DEAL_BURN_FUND_DEAL_RAW - 1),
        (2, "postFromNote", 0),
        (3, "postFromNote", DEAL_BURN_POST_FROM_NOTE_RAW - 1),
    ] {
        let nonce = nonce + offset;
        let tc = chain
            .token_contract_deploy_address(
                &keys,
                &Address::parse(&market.root_model).unwrap(),
                nonce,
            )
            .await
            .unwrap();
        let recovery = output.join(format!("{operation}-{remainder}.tc.txt"));
        let deployed = recovery.with_extension("deployed");
        if !deployed.exists() {
            if !recovery.exists() {
                private_write(&recovery, tc.with_workchain().as_bytes());
            }
            if chain
                .token_contract_deal_state(&tc)
                .await
                .unwrap()
                .is_none()
            {
                let funding = DEAL_BURN_DEPLOY_RAW + remainder;
                let have = account_ecc(&chain, &note, true).await;
                if have < funding {
                    chain
                        .giver_send_shell(&note.with_workchain(), funding - have)
                        .await
                        .unwrap();
                    wait_ecc(&chain, &note, funding, true).await;
                }
                chain
                    .note_deploy_deal(
                        &note,
                        &keys,
                        crate::chain::client::NoteDeployDealRequest {
                            nonce,
                            model_name: &market.frame_model,
                            price_per_tick: SHELL_UNIT,
                            max_ticks: MIN_STREAM_BUY_TICKS,
                            gas_reserve: funding,
                        },
                    )
                    .await
                    .unwrap();
                wait_active(&chain, &tc).await;
            }
            fs::write(&deployed, tc.with_workchain()).unwrap();
        }
        if chain
            .token_contract_deal_state(&tc)
            .await
            .unwrap()
            .is_none()
        {
            let prior =
                fs::read_to_string(output.join(format!("{operation}-{remainder}.receipt.txt")))
                    .unwrap();
            assert!(prior.contains("ECC[2]/SHELL reserve"), "{prior}");
            assert!(
                prior.contains(&format!("observed_reserve_raw={remainder}")),
                "{prior}"
            );
            println!(
                "2199 prior internal rejection retained: operation={operation} reserve={remainder}; TC destroyed"
            );
            continue;
        }
        live_2199_wait_ecc(&chain, &tc, remainder).await;
        let nominal_before = chain.private_note_shell_balance(&note).await.unwrap();
        assert!(nominal_before >= 2 * SHELL_UNIT);
        let evidence = if operation == "fundDeal" {
            let error =
                post_seller_bond_and_wait(&chain, &note, &keys, nonce, &tc.with_workchain(), &tc)
                    .await
                    .unwrap_err();
            error.to_string()
        } else {
            let seller = RealSellerBackend::new(
                RealChainBackend::connect(&manifest).unwrap(),
                note.clone(),
                KeyPair::from_secret_hex(keys.secret_hex()).unwrap(),
                market.model_hash.clone(),
                market.frame_model.clone(),
                nonce,
                TICK_SIZE,
            )
            .with_offer_journal_dir(output.join("sell-journal"));
            seller
                .post_offer(
                    SellOffer {
                        token_contract: tc.with_workchain(),
                        price_per_tick: SHELL_UNIT.try_into().unwrap(),
                        max_ticks: MIN_STREAM_BUY_TICKS.try_into().unwrap(),
                        flags: 0,
                    },
                    &LocalNote::generate(),
                )
                .await
                .unwrap();
            let book = chain
                .inference_orderbook_address(&note, &market.model_hash, TICK_SIZE)
                .await
                .unwrap();
            let mut evidence = String::new();
            for _ in 0..ACCOUNT_ACTIVATION_MAX_ATTEMPTS {
                if let Err(error) = seller.reconcile_pending_offer(&tc, &book).await {
                    evidence = error.to_string();
                    if evidence.contains("ECC[2]/SHELL reserve") {
                        break;
                    }
                }
                tokio::time::sleep(ACCOUNT_ACTIVATION_POLL_INTERVAL).await;
            }
            assert!(seller.offer_journal_path(&tc).unwrap().exists());
            evidence
        };
        fs::write(
            output.join(format!("{operation}-{remainder}.receipt.txt")),
            &evidence,
        )
        .unwrap();
        // Always retire this adversarial fixture before asserting the collected diagnosis.
        assert!(
            chain
                .token_contract_deal_state(&tc)
                .await
                .unwrap()
                .is_some()
        );
        assert!(recovery.exists());
        let have = account_ecc(&chain, &tc, true).await;
        assert_eq!(have, remainder, "no automatic replenishment");
        chain
            .giver_send_shell(&tc.with_workchain(), DEAL_BURN_TERMINAL_RAW - have)
            .await
            .unwrap();
        live_2199_wait_ecc(&chain, &tc, DEAL_BURN_TERMINAL_RAW).await;
        chain.close_unsold_deal(&tc, &keys).await.unwrap();
        chain.wait_deal_destroyed(&tc).await.unwrap();
        for fact in [
            tc.bare(),
            operation,
            "ECC[2]/SHELL reserve",
            "transaction_hash=",
            "settlement is unconfirmed",
        ] {
            assert!(evidence.contains(fact), "{evidence}");
        }
        assert!(
            evidence.contains(&format!("observed_reserve_raw={remainder}")),
            "{evidence}"
        );
        assert!(!evidence.contains("Re-mint"), "{evidence}");
        println!(
            "2199 internal rejection PASS: operation={operation} reserve={remainder} nominal_before={nominal_before}; TC destroyed"
        );
    }
}

#[tokio::test]
#[ignore = "read-only shellnet audit of the four internal-failure fixtures"]
async fn live_2199_review_cleanup_audit() {
    let chain = RealChainBackend::connect(crate::params::manifest_path().unwrap()).unwrap();
    assert_eq!(chain.network(), "shellnet");
    let dir = std::path::PathBuf::from(std::env::var("DEXDO_2199_STATE_DIR").unwrap());
    let output = dir.join("review-fixes");
    for (operation, remainder) in [
        ("fundDeal", 0),
        ("fundDeal", crate::params::DEAL_BURN_FUND_DEAL_RAW - 1),
        ("postFromNote", 0),
        (
            "postFromNote",
            crate::params::DEAL_BURN_POST_FROM_NOTE_RAW - 1,
        ),
    ] {
        let tc = Address::parse(
            fs::read_to_string(output.join(format!("{operation}-{remainder}.tc.txt")))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert!(
            chain
                .token_contract_deal_state(&tc)
                .await
                .unwrap()
                .is_none()
        );
    }
    for name in ["seller", "buyer"] {
        let (note, _) = role(&dir, name);
        let report = chain.private_note_outstanding(&note).await.unwrap();
        let nominal = chain.private_note_shell_balance(&note).await.unwrap();
        fs::write(
            output.join(format!("{name}-audit.txt")),
            format!("nominal_shell_raw={nominal}\n{report:#?}"),
        )
        .unwrap();
        assert_eq!(report.opaque_order_count, 0);
        assert!(report.unexplained_order_keys.is_empty());
        assert!(report.deal_leads.is_empty());
        println!(
            "2199 cleanup audit: {name} nominal_shell_raw={nominal}, outstanding orders=0, funded leads=0"
        );
    }
}
