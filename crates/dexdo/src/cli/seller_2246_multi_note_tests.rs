fn issue_2246_market(digit: u8, owner: u8) -> dexdo_core::MarketManifest {
    let address = |byte: u8| format!("0:{}", hex::encode([byte; 32]));
    let frame_model = "multi-note";
    dexdo_core::MarketManifest {
        network: "shellnet".into(),
        frame_model: frame_model.into(),
        model_hash: dexdo_core::model_hash_for(frame_model),
        inference_order_book: address(0x0b),
        root_model: address(owner),
        token_contract: address(digit),
        seller_note: address(owner),
        nonce: 1,
        price_per_tick: dexdo_core::PRICE_STEP,
        max_ticks: dexdo_core::MIN_STREAM_BUY_TICKS,
        all_or_none: false,
    }
}

#[tokio::test]
async fn issue_2246_legacy_tls_upgrade_preserves_both_pins_with_migration() {
    let root = tempfile::tempdir().unwrap();
    let dir = Some(root.path());
    let contracts = root.path().join("contracts.json");
    let first = issue_2246_market(0x11, 0xa1);
    let second = issue_2246_market(0x22, 0xa2);
    let primary_socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let secondary_socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = primary_socket.local_addr().unwrap();
    let other_gateway = secondary_socket.local_addr().unwrap();
    let advertised = gateway.to_string();
    let mut legacy = Vec::new();
    let mut handovers = Vec::new();
    let mut opened = std::collections::HashMap::new();
    for (market, original_gateway) in [(&first, gateway), (&second, other_gateway)] {
        let path = persist_provision_deal_handle(dir, market, &contracts, false).unwrap();
        let mut handle = deals::load_deal_handle(&path).unwrap();
        // The old process could rewrite B's handle after publishing its original handover.
        handle.endpoint = Some(deals::DealEndpointInfo {
            kind: "gateway".into(),
            value: advertised.clone(),
        });
        deals::save_deal_handle(root.path(), &handle).unwrap();
        let note_dir = seller_pool_dir(dir, &market.seller_note).unwrap();
        let tls = load_or_migrate_gateway_tls(&note_dir, &[]).unwrap();
        handovers.push(dexdo_core::Handover {
            endpoint: format!("https://{original_gateway}"),
            tls_fingerprint: tls.fingerprint,
        });
        legacy.push(note_dir);
        opened.insert(market.token_contract.clone(), b"legacy".to_vec());
    }
    assert_ne!(handovers[0].tls_fingerprint, handovers[1].tls_fingerprint);
    legacy.reverse(); // The selected note has the other certificate.
    let records = seller_market_handle_records_matching(dir, Some("shellnet"), |_| true).unwrap();
    let startup = async |opened: &std::collections::HashMap<String, Vec<u8>>, advertised: &str| {
        seller_gateway_tls_for_startup(dir, gateway, advertised, &legacy, &records, async |tc| {
            Ok(opened.get(tc).cloned())
        })
        .await
    };
    let pool = root.path().join("seller-pool");
    let shared_path = pool.join("gateway.pem");
    let store = crate::cli::secret_store::SecretStore::open().unwrap();
    let shared_name = crate::cli::secret_store::SecretName::at(&shared_path);
    let mut only_second = opened.clone();
    only_second.remove(&first.token_contract).unwrap();
    let error = startup(&only_second, &advertised)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains(&display_token_contract(&second.token_contract)),
        "{error}"
    );
    assert!(error.contains("confirmation of the original endpoint and TLS pin"), "{error}");
    assert!(error.contains(GATEWAY_MIGRATION_FILE), "{error}");
    assert!(store.read(&shared_name).unwrap().is_none());

    std::fs::create_dir_all(&pool).unwrap();
    let migration = pool.join(GATEWAY_MIGRATION_FILE);
    let first_entry = json!({
        "token_contract": display_token_contract(&first.token_contract),
        "endpoint": handovers[0].endpoint,
        "listen": gateway,
        "tls_fingerprint": handovers[0].tls_fingerprint,
    });
    let mut entry = json!({
        "token_contract": display_token_contract(&second.token_contract),
        "endpoint": handovers[1].endpoint,
        "listen": gateway,
        "tls_fingerprint": handovers[1].tls_fingerprint,
    });
    let save_migration = |entries: &[&serde_json::Value]| {
        std::fs::write(&migration, serde_json::to_vec(entries).unwrap()).unwrap();
    };
    save_migration(&[&first_entry, &entry]);
    let error = startup(&opened, &advertised).await.err().unwrap().to_string();
    assert!(error.contains("has a different pin"), "{error}");
    for market in [&first, &second] {
        assert!(error.contains(&display_token_contract(&market.token_contract)), "{error}");
    }
    assert!(store.read(&shared_name).unwrap().is_none());

    entry["listen"] = json!(other_gateway);
    save_migration(&[&first_entry, &entry]);
    let tls = startup(&opened, &advertised).await.unwrap();
    assert_eq!(tls.primary.fingerprint, handovers[0].tls_fingerprint);
    assert_eq!(tls.additional.len(), 1);
    let restored = startup(&only_second, &advertised).await.unwrap();
    let route = &restored.migrated_routes[&deals::normalize_addr(&second.token_contract)];
    assert_eq!(route.advertise, other_gateway.to_string());
    assert_eq!(route.listen, other_gateway);
    drop((primary_socket, secondary_socket));
    let seller = dexdo::seller::start_gateway_with_note_tls_listeners(
        gateway,
        dexdo::seller::UpstreamConfig::Mock,
        Arc::new(LocalNote::generate()),
        root.path().into(),
        tls.primary,
        tls.additional,
    )
    .await
    .unwrap();
    for handover in &handovers {
        dexdo::buyer::tls::connect_pinned(&handover.endpoint, &handover.tls_fingerprint)
            .await
            .unwrap();
    }
    let error =
        dexdo::buyer::tls::connect_pinned(&handovers[0].endpoint, &handovers[1].tls_fingerprint)
            .await
            .unwrap_err();
    assert!(dexdo::buyer::tls::dial_reached_wrong_endpoint(&error));
    seller.server_task.abort();
    let _ = seller.server_task.await;

    let cursor = seller_watch_cursor_path(dir, &second.token_contract).unwrap();
    assert!(
        dexdo::seller::tls::read_handover(&cursor, b"legacy")
            .unwrap()
            .is_none()
    );
    // An operator mapping cannot replace confirmed evidence for a handover from the new seller.
    dexdo::seller::tls::persist_handover(&cursor, b"shared", &handovers[0]).unwrap();
    opened.insert(second.token_contract.clone(), b"shared".to_vec());
    let before = store.read(&shared_name).unwrap().unwrap();
    let mismatch = startup(&opened, &advertised).await.err().unwrap();
    assert!(
        mismatch
            .to_string()
            .contains("disagrees with the on-chain handover evidence")
    );
    entry["endpoint"] = json!(handovers[0].endpoint);
    entry["tls_fingerprint"] = json!(handovers[0].tls_fingerprint);
    save_migration(&[&first_entry, &entry]);
    assert!(startup(&opened, &advertised).await.is_ok());
    save_migration(&[&first_entry]);
    let compatible = startup(&opened, &advertised).await.unwrap();
    assert_eq!(compatible.primary.fingerprint, handovers[0].tls_fingerprint);
    assert!(compatible.additional.is_empty());

    let moved = startup(&opened, "127.0.0.1:9").await.err().unwrap();
    assert!(moved.to_string().contains(GATEWAY_MIGRATION_FILE));
    let after = store.read(&shared_name).unwrap().unwrap();
    assert!(
        before.as_str() == after.as_str(),
        "migration changed the shared TLS key"
    );

    // STOP removes B after the first state read. A keeps serving with its original pin.
    let open_chain = PoolTestBackend::new(
        Arc::new(Mutex::new(Vec::new())),
        second.token_contract.clone(),
        2,
        2,
        true,
        1,
    );
    open_chain.opened.store(true, Ordering::Relaxed);
    let open = open_chain.deal_state(&second.token_contract).await.unwrap();
    let resumed =
        seller_gateway_tls_for_startup(dir, gateway, &advertised, &legacy, &records, async |tc| {
            if tc == second.token_contract {
                let mut states = VecDeque::from([open, None]);
                read_open_seller_handover(
                    tc,
                    async || Ok(states.pop_front().unwrap()),
                    async || Ok(None),
                )
                .await
            } else {
                Ok(opened.get(tc).cloned())
            }
        })
        .await
        .unwrap();
    assert_eq!(resumed.primary.fingerprint, handovers[0].tls_fingerprint);
    let seller = dexdo::seller::start_gateway_with_note_tls(
        gateway,
        dexdo::seller::UpstreamConfig::Mock,
        Arc::new(LocalNote::generate()),
        resumed.primary,
    )
    .await
    .unwrap();
    dexdo::buyer::tls::connect_pinned(&handovers[0].endpoint, &handovers[0].tls_fingerprint)
        .await
        .unwrap();
    seller.server_task.abort();
    let error =
        read_open_seller_handover(&second.token_contract, async || Ok(open), async || Ok(None))
            .await
            .unwrap_err();
    assert!(error.to_string().contains("has no readable handover"));
}

#[tokio::test]
async fn issue_2246_stale_anchor_recovers_pending_sell_before_any_book_row() {
    let root = tempfile::tempdir().unwrap();
    let dir = Some(root.path());
    let contracts = root.path().join("contracts.json");
    let terminal = issue_2246_market(0x11, 0xa1);
    let pending = issue_2246_market(0x22, 0xa2);
    let vacant = issue_2246_market(0x33, 0xa3);
    let backends = [&terminal, &pending, &vacant].map(|market| {
        persist_provision_deal_handle(dir, market, &contracts, false).unwrap();
        let ticks = u64::try_from(market.max_ticks).unwrap();
        Arc::new(PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            market.token_contract.clone(),
            ticks,
            ticks,
            false,
            1,
        ))
    });
    backends[0].settle_and_lose_getdeal();
    let chain = &backends[1];
    let records = seller_market_handle_records_matching(dir, Some("shellnet"), |_| true).unwrap();
    let backend = |market: &dexdo_core::MarketManifest| {
        let chain = backends
            .iter()
            .find(|chain| chain.token_contract == market.token_contract)
            .unwrap()
            .clone();
        Ok((chain as Arc<dyn ChainBackend>, ()))
    };
    assert!(
        working_seller_startup_market(&records, &terminal.token_contract, backend)
            .await
            .unwrap()
            .is_none()
    );
    chain.pending_sell.store(true, Ordering::Relaxed);
    chain.inspection_fail_once.store(true, Ordering::Relaxed);
    let selected = working_seller_startup_market(&records, &terminal.token_contract, backend)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected, pending);
    assert!(chain.inspection_fail_once.swap(false, Ordering::Relaxed));

    let seller = dexdo::seller::start_gateway_with_note_and_deals_dir(
        "127.0.0.1:0".parse().unwrap(),
        dexdo::seller::UpstreamConfig::Mock,
        Arc::new(LocalNote::generate()),
        root.path().into(),
    )
    .await
    .unwrap();
    let advertised = seller.listen_addr.to_string();
    let context = SellerPoolContext {
        deals_dir: dir,
        contracts: &contracts,
        note_addr: &pending.seller_note,
        frame_model: &pending.frame_model,
        gateway_advertise: &advertised,
        advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::Required,
    };
    let cursor = seller_watch_cursor_path(dir, &pending.token_contract).unwrap();
    dexdo::seller::persist_seller_offer_mode(&cursor, &pending.token_contract, false).unwrap();
    let subscription =
        recorded_seller_offer_mode(chain.as_ref(), &pending.token_contract, &cursor, None)
            .await
            .unwrap()
            .unwrap();
    let deal = SellerPoolDeal {
        chain: chain.clone(),
        cfg: SellerConfig {
            token_contract: pending.token_contract.clone(),
            price_per_tick: pending.price_per_tick.try_into().unwrap(),
            max_ticks: pending.max_ticks.try_into().unwrap(),
            subscription,
            all_or_none: false,
            gateway_advertise: advertised.clone(),
            mock_token_count: 1,
        },
        watch: dexdo::seller::SellerMatchWatchConfig {
            cursor_path: cursor,
            poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
        },
        upstream: dexdo::seller::UpstreamConfig::Mock,
        nonce: pending.nonce,
        market: Some(pending.clone()),
    };
    let shutdown = futures::future::pending().fuse();
    tokio::pin!(shutdown);
    let (identity, owner) = prepare_pool_deal(
        &seller,
        &deal,
        &context,
        false,
        shutdown.as_mut(),
        &mut false,
    )
    .await
    .unwrap();
    assert!(identity.is_none());
    assert_eq!(owner.as_deref(), Some(pending.seller_note.as_str()));

    // The already-submitted SELL matches after startup has reserved its watcher.
    chain.matched.store(true, Ordering::Relaxed);
    chain.owner_fills.lock().unwrap().push((
        1,
        MatchedFill {
            order_id: pending.nonce.into(),
            token_contract: pending.token_contract.clone(),
            ticks: pending.max_ticks,
            price_per_tick: pending.price_per_tick,
        },
    ));
    let (fills, mut fill_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop) = tokio::sync::watch::channel(false);
    let (_, _, result) = watch_pool_deal(
        &seller,
        deal,
        identity,
        owner,
        fills,
        context.advertise_probe,
        stop,
    )
    .await;
    assert!(matches!(
        result.unwrap(),
        PoolWatchOutcome::Matched(_, OpenedDealPlan::Drive { .. })
    ));
    assert_eq!(
        fill_rx.try_recv().unwrap().cfg.token_contract,
        pending.token_contract
    );
    assert_eq!(chain.post_calls.load(Ordering::Relaxed), 0);
    assert_eq!(chain.open_calls.load(Ordering::Relaxed), 1);
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2246_recovers_funded_and_resting_modes_without_guessing_other_notes() {
    let root = tempfile::tempdir().unwrap();
    let contracts = root.path().join("contracts.json");
    let ordinary = issue_2246_market(0x11, 0xa1);
    let mut subscription = issue_2246_market(0x22, 0xa2);
    subscription.max_ticks = 2 * u128::from(dexdo_core::SUBSCRIPTION_WEEKS);
    let mut resting = subscription.clone();
    resting.token_contract = issue_2246_market(0x33, 0xa2).token_contract;
    resting.nonce += 1;
    let vacant = issue_2246_market(0x44, 0xa3);
    let mut chains = std::collections::HashMap::new();
    for (market, funded, is_subscription) in [
        (&ordinary, true, false), (&subscription, true, true),
        (&resting, false, true), (&vacant, false, false),
    ] {
        persist_provision_deal_handle(Some(root.path()), market, &contracts, false).unwrap();
        let ticks = u64::try_from(market.max_ticks).unwrap();
        let mut chain = PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())), market.token_contract.clone(), ticks, ticks, funded, 1,
        );
        chain.subscription = is_subscription;
        chain.signing_note = Some(Arc::new(LocalNote::generate()));
        chain.opened.store(funded, Ordering::Relaxed);
        chain.probe_accepted.store(funded, Ordering::Relaxed);
        if market == &resting {
            chain.resting_owner = market.seller_note.clone();
            chain = chain.resting_unexpired(true);
        }
        chains.insert(market.token_contract.clone(), Arc::new(chain));
    }
    let ctx = SellerPoolContext {
        deals_dir: Some(root.path()), contracts: &contracts, note_addr: &ordinary.seller_note,
        frame_model: &ordinary.frame_model, gateway_advertise: "127.0.0.1:1234",
        advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::Required,
    };
    let initial = SellerPoolDeal {
        chain: chains[&ordinary.token_contract].clone(),
        cfg: SellerConfig {
            token_contract: ordinary.token_contract.clone(), price_per_tick: ordinary.price_per_tick.try_into().unwrap(),
            max_ticks: ordinary.max_ticks.try_into().unwrap(), subscription: false, all_or_none: false,
            gateway_advertise: ctx.gateway_advertise.to_owned(), mock_token_count: 1,
        },
        watch: dexdo::seller::SellerMatchWatchConfig {
            cursor_path: seller_watch_cursor_path(ctx.deals_dir, &ordinary.token_contract).unwrap(),
            poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
        },
        upstream: dexdo::seller::UpstreamConfig::Mock, nonce: ordinary.nonce, market: Some(ordinary.clone()),
    };
    let records = seller_market_handle_records_matching(ctx.deals_dir, Some("shellnet"), |_| true).unwrap();
    let backend = |market: &dexdo_core::MarketManifest| {
        Ok((chains[&market.token_contract].clone() as Arc<dyn ChainBackend>, dexdo::seller::UpstreamConfig::Mock))
    };
    let pool = load_seller_pool_deals_with_scope(&ctx, initial.clone(), 1, None, records, (&ordinary.seller_note, false), backend)
        .await.unwrap();
    assert_eq!(pool.deals.len(), 3);
    let seller = dexdo::seller::start_gateway_with_note_and_deals_dir(
        "127.0.0.1:0".parse().unwrap(), dexdo::seller::UpstreamConfig::Mock,
        Arc::new(LocalNote::generate()), root.path().into(),
    ).await.unwrap();
    for deal in &pool.deals {
        let tc = &deal.cfg.token_contract;
        assert_eq!(deal.cfg.subscription, tc != &ordinary.token_contract);
        let owner = &deal.market.as_ref().unwrap().seller_note;
        let inspection = dexdo::seller::inspect_seller_offer(deal.chain.as_ref(), &deal.cfg, Some(owner)).await.unwrap();
        if tc == &resting.token_contract {
            assert!(matches!(inspection, dexdo::seller::SellerOfferInspection::Resting { order_id: 1 }));
        } else {
            assert!(matches!(inspection, dexdo::seller::SellerOfferInspection::Funded));
            let buyer = chains[tc].matched();
            let watch = &deal.watch;
            let resumed = dexdo::seller::serve_watched_match(&seller, deal.chain.as_ref(), &deal.cfg, watch, buyer.clone()).await.unwrap();
            assert_eq!(resumed.buyer_pubkey, buyer.buyer_pubkey);
            assert_eq!(chains[tc].open_calls.load(Ordering::Relaxed), 0);
            let OpenedDealPlan::Drive { snapshot, .. } = plan_opened_deal(&seller, deal.chain.as_ref(), tc).await.unwrap() else {
                panic!("the paid deal must have a settlement driver");
            };
            assert_eq!(snapshot.subscription.is_subscription(), deal.cfg.subscription);
        }
        assert_eq!(chains[tc].post_calls.load(Ordering::Relaxed), 0);
    }
    seller.server_task.abort();

    // The default market can outlive its TC. The other paid deal is still found with its own note.
    chains[&ordinary.token_contract].exists.store(false, Ordering::Relaxed);
    chains[&ordinary.token_contract].getdeal_gone.store(true, Ordering::Relaxed);
    let records = seller_market_handle_records_matching(ctx.deals_dir, Some("shellnet"), |_| true).unwrap();
    let recovered = working_seller_startup_market(&records, &ordinary.token_contract, backend).await.unwrap().unwrap();
    assert_eq!(recovered, subscription);
    assert_eq!(chains[&ordinary.token_contract].post_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn issue_2246_replenishment_releases_only_the_idle_note_lock() {
    assert_issue_2246_replenishment(false, false).await;
}

#[tokio::test]
async fn issue_2246_replenishment_starts_with_a_terminal_anchor() {
    assert_issue_2246_replenishment(true, false).await;
}

#[tokio::test]
async fn issue_2246_replenishment_activates_the_already_installed_market_once() {
    assert_issue_2246_replenishment(true, true).await;
}

async fn assert_issue_2246_replenishment(terminal_at_start: bool, held: bool) {
    let root = tempfile::tempdir().unwrap();
    let contracts = root.path().join("contracts.json");
    let market_path = root.path().join("market.json");
    let handoff = root.path().join("handoff.json");
    let seller = dexdo::seller::start_gateway_with_note_and_deals_dir(
        "127.0.0.1:0".parse().unwrap(), dexdo::seller::UpstreamConfig::Mock,
        Arc::new(LocalNote::generate()), root.path().into(),
    ).await.unwrap();
    let advertised = seller.listen_addr.to_string();
    let make = |market: &dexdo_core::MarketManifest, funded, buyer: Option<&LocalNote>| {
        let ticks = u64::try_from(market.max_ticks).unwrap();
        let mut chain = PoolTestBackend::new(Arc::new(Mutex::new(Vec::new())), market.token_contract.clone(), ticks, ticks, funded, 1);
        chain.signing_note = Some(Arc::new(LocalNote::generate()));
        chain.probe_accepted.store(funded, Ordering::Relaxed);
        if let Some(buyer) = buyer {
            chain.buyer_pubkey = buyer.pubkey();
        }
        let chain = Arc::new(chain);
        let deal = SellerPoolDeal {
            chain: chain.clone(),
            cfg: SellerConfig {
                token_contract: market.token_contract.clone(), price_per_tick: market.price_per_tick.try_into().unwrap(),
                max_ticks: ticks, subscription: false, all_or_none: market.all_or_none,
                gateway_advertise: advertised.clone(), mock_token_count: 1,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: seller_watch_cursor_path(Some(root.path()), &market.token_contract).unwrap(),
                poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
            },
            upstream: dexdo::seller::UpstreamConfig::Mock, nonce: market.nonce, market: Some(market.clone()),
        };
        (chain, deal)
    };
    let mut first = issue_2246_market(0x11, 0xa1);
    first.all_or_none = true;
    let mut successor = issue_2246_market(0x22, 0xa1);
    successor.all_or_none = true;
    successor.nonce = first.nonce + 1;
    let second = issue_2246_market(0x33, 0xa2);
    let (first_chain, first_deal) = make(&first, true, None);
    let (next_chain, _) = make(&successor, false, None);
    let second_buyer = Arc::new(LocalNote::generate());
    let (second_chain, second_deal) = make(&second, true, Some(second_buyer.as_ref()));
    first_chain.opened.store(true, Ordering::Relaxed);
    dexdo::seller::poll_match_and_maybe_open(&seller, first_chain.as_ref(), &first_deal.cfg, &first_deal.watch.cursor_path).await.unwrap().unwrap();
    first_chain.exists.store(false, Ordering::Relaxed);
    first_chain.getdeal_gone.store(true, Ordering::Relaxed);
    second_chain.opened.store(true, Ordering::Relaxed);
    assert!(!seller_has_startup_obligation(next_chain.as_ref(), &successor.token_contract).await.unwrap());
    assert!(seller_has_startup_obligation(second_chain.as_ref(), &second.token_contract).await.unwrap());

    let dir = Some(root.path());
    let first_pool = seller_pool_dir(dir, &first.seller_note).unwrap();
    let second_pool = seller_pool_dir(dir, &second.seller_note).unwrap();
    let first_lock = acquire_seller_pool_lock(&first_pool).unwrap();
    let _second_lock = acquire_seller_pool_lock(&second_pool).unwrap();
    let context = SellerPoolContext {
        deals_dir: dir, contracts: &contracts, note_addr: &first.seller_note,
        frame_model: &first.frame_model, gateway_advertise: &advertised,
        advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::Required,
    };
    let shutdown = async {
        loop {
            if std::fs::read_to_string(&handoff).is_ok_and(|json| json.contains("idle")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(acquire_seller_pool_lock(&second_pool).is_err());
        assert_eq!(next_chain.post_calls.load(Ordering::Relaxed), 0);
        {
            let _provision_lock = acquire_seller_pool_lock(&first_pool).unwrap();
            if !held {
                let record = persist_provision_deal_handle(dir, &successor, &contracts, false).unwrap();
                let market = deals::load_deal_handle(&record).unwrap().market.unwrap();
                std::fs::write(&market_path, serde_json::to_vec(&market).unwrap()).unwrap();
            }
        }
        next_chain.opened.store(true, Ordering::Relaxed);
        next_chain.probe_accepted.store(true, Ordering::Relaxed);
        deals::write_private_atomic(&handoff, &serde_json::to_vec(&json!({
            "phase": "activate", "token_contract": successor.token_contract,
        })).unwrap()).unwrap();
        while next_chain.post_calls.load(Ordering::Relaxed) != 1 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(acquire_seller_pool_lock(&first_pool).is_err());
        assert!(acquire_seller_pool_lock(&second_pool).is_err());
        let handover = dexdo_core::Handover {
            endpoint: format!("https://{advertised}"), tls_fingerprint: seller.tls_fingerprint.clone(),
        };
        let stream = dexdo::buyer::Buyer::from_note(second_buyer)
            .connect_and_stream(&handover, &second.token_contract, &second.frame_model, 1)
            .await.unwrap();
        assert_eq!(stream.received, 1);
    }.fuse();
    tokio::pin!(shutdown);
    let installed = if held { &successor } else { &first };
    let persisted = persist_provision_deal_handle(dir, installed, &contracts, false).unwrap();
    let market = deals::load_deal_handle(&persisted).unwrap().market.unwrap();
    std::fs::write(&market_path, serde_json::to_vec(&market).unwrap()).unwrap();
    let policy = pool_test_policy(1);
    tokio::time::timeout(std::time::Duration::from_secs(25), Box::pin(seller_replenish::run(
        &seller, context, &policy, if terminal_at_start { vec![] } else { vec![first_deal] }, vec![second_deal],
        |_: String, _: String, _: u64, _: u64, _: u64| futures::future::ready(Err(anyhow::anyhow!("no residual provision"))),
        seller_replenish::Handoff { path: handoff.clone(), market_path: &market_path, market, lock: first_lock, mock_token_count: 1, hold_initial_market: held },
        async |_| Ok((next_chain.clone() as Arc<dyn ChainBackend>, dexdo::seller::UpstreamConfig::Mock)), shutdown.as_mut(),
    ))).await.unwrap().unwrap();
    assert_eq!(next_chain.post_calls.load(Ordering::Relaxed), 1);
    assert_eq!(second_chain.post_calls.load(Ordering::Relaxed), 0);
    assert_eq!(second_chain.open_calls.load(Ordering::Relaxed), 0);
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2246_restart_serves_and_claims_deals_from_both_notes() {
    let root = tempfile::tempdir().unwrap();
    let contracts = root.path().join("contracts.json");
    let first = issue_2246_market(0x11, 0xa1);
    let second = issue_2246_market(0x22, 0xa2);
    let make_backend = |market: &dexdo_core::MarketManifest| {
        let ticks = u64::try_from(market.max_ticks).unwrap();
        let mut backend = PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            market.token_contract.clone(),
            ticks,
            ticks,
            true,
            1,
        );
        backend.signing_note = Some(Arc::new(LocalNote::generate()));
        backend.opened.store(true, Ordering::Relaxed);
        backend.probe_accepted.store(true, Ordering::Relaxed);
        Arc::new(backend)
    };
    let first_chain = make_backend(&first);
    let second_chain = make_backend(&second);

    for (primary, primary_chain, expected) in [
        (&first, &first_chain, vec![&first_chain]),
        (&second, &second_chain, vec![&first_chain, &second_chain]),
    ] {
        persist_provision_deal_handle(Some(root.path()), primary, &contracts, false).unwrap();
        let capacity = dexdo::seller::capacity::CapacityManager::in_deals_dir(root.path().into());
        let mut targets = Vec::new();
        for chain in &expected {
            let tc = &chain.token_contract;
            let snapshot = chain.deal_snapshot(tc).await.unwrap().unwrap();
            capacity
                .reconcile_deal(tc, snapshot.state, snapshot.subscription)
                .unwrap();
            capacity
                .reserve(tc, 1)
                .unwrap()
                .finish_with_delivery(1)
                .unwrap();
            targets.push((
                *chain,
                u64::try_from(snapshot.state.tokens_pending).unwrap() + 1,
            ));
        }

        let seller = dexdo::seller::start_gateway_with_note_and_deals_dir(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            primary_chain.signing_note.clone().unwrap(),
            root.path().into(),
        )
        .await
        .unwrap();
        let advertised = seller.listen_addr.to_string();
        let context = SellerPoolContext {
            deals_dir: Some(root.path()),
            contracts: &contracts,
            note_addr: &primary.seller_note,
            frame_model: &primary.frame_model,
            gateway_advertise: &advertised,
            advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::Required,
        };
        let initial = SellerPoolDeal {
            chain: primary_chain.clone(),
            cfg: SellerConfig {
                token_contract: primary.token_contract.clone(),
                price_per_tick: primary.price_per_tick.try_into().unwrap(),
                max_ticks: primary.max_ticks.try_into().unwrap(),
                subscription: false,
                all_or_none: false,
                gateway_advertise: advertised.clone(),
                mock_token_count: 1,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: seller_watch_cursor_path(Some(root.path()), &primary.token_contract)
                    .unwrap(),
                poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
            },
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: primary.nonce,
            market: Some(primary.clone()),
        };
        let records =
            seller_market_handle_records_matching(Some(root.path()), Some("shellnet"), |_| true)
                .unwrap();
        let pool =
            load_seller_pool_deals_with_scope(&context, initial, 1, None, records, (&primary.seller_note, false), |market| {
                assert_eq!(market.seller_note, first.seller_note);
                Ok((first_chain.clone(), dexdo::seller::UpstreamConfig::Mock))
            })
            .await
            .unwrap();
        assert_eq!(pool.deals.len(), expected.len());
        let mut provisioner = |_: String, _: String, _: u64, _: u64, _: u64| {
            futures::future::ready(Err(anyhow::anyhow!("full fills have no residual")))
        };

        let shutdown = async {
            // TLS readiness needs real time. Advance the claim clock only after both
            // buyers' routes have restored their durable delivery counters.
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                while targets.iter().any(|(chain, target)| {
                    seller
                        .state
                        .delivery(&chain.token_contract)
                        .count
                        .load(Ordering::Acquire)
                        < *target
                }) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("restore each buyer's gateway route");
            tokio::time::pause();
            loop {
                if targets
                    .iter()
                    .all(|(chain, target)| chain.claimed.load(Ordering::Relaxed) == *target)
                {
                    break;
                }
                tokio::time::sleep(DEFAULT_MATCH_POLL_INTERVAL).await;
            }
        }
        .fuse();
        tokio::pin!(shutdown);
        let policy = pool_test_policy(2);
        let mut stopped = false;
        tokio::time::timeout(
            std::time::Duration::from_secs(1_200),
            run_seller_pool(
                &seller,
                pool.deals,
                context,
                &policy,
                &mut provisioner,
                shutdown.as_mut(),
                &mut stopped,
            ),
        )
        .await
        .expect("both note signers must claim through the shared gateway")
        .unwrap();
        assert!(stopped);
        tokio::time::resume();
    }
    assert_eq!(first_chain.post_calls.load(Ordering::Relaxed), 0);
    assert_eq!(second_chain.post_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn issue_2246_startup_names_the_deal_whose_note_key_is_missing() {
    let root = tempfile::tempdir().unwrap();
    let mut first = issue_2246_market(0x11, 0xa1);
    let mut missing = issue_2246_market(0x22, 0xa2);
    let contracts = crate::cli::commands::manifest_path().unwrap();
    let network = dexdo_core::Deployed::load(&contracts).unwrap().network;
    for market in [&mut first, &mut missing] {
        market.network.clone_from(&network);
        persist_provision_deal_handle(Some(root.path()), market, &contracts, false).unwrap();
    }
    let pool = root.path().join("pn_pool.json");
    write_owner_only_key_fixture(
        &pool,
        &json!({"notes": [{
            "address": first.seller_note, "owner_secret_key_hex": "11".repeat(32),
        }]})
        .to_string(),
    );
    let policy = root.path().join("policy.json");
    std::fs::write(&policy, json!({
        "version": 1, "seller": {
            "on": {"after_deal_done": "retire", "buyer_no_show": "retire_gateway", "dispute_against_me": "hold"},
            "max_open_deals": 1
        }
    }).to_string()).unwrap();
    let mut args = parsed_seller_args(&["--allow-private-advertise"]);
    args.token_contract = Some(first.token_contract);
    args.deals_dir = Some(root.path().to_path_buf());
    args.policy = Some(policy);
    let error = run_seller_from_pool(args, None, Some(&pool))
        .await
        .unwrap_err();
    let rendered = format!("{error:#}");
    for fragment in [
        "DEXDO_PN_POOL",
        &missing.token_contract[2..],
        &missing.seller_note[2..],
    ] {
        assert!(rendered.contains(fragment), "{rendered}");
    }
    assert!(!root.path().join("seller-pool/gateway.pem").exists());
}
