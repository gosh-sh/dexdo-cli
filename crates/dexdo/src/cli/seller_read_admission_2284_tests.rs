#[tokio::test(start_paused = true)]
async fn issue_2284_pool_reads_progress_while_control_waits_for_the_same_lock() {
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let (incoming, inputs) = tokio::sync::mpsc::unbounded_channel();
    let (outputs, mut results) = tokio::sync::mpsc::unbounded_channel();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let queued_gate = gate.clone();
    incoming
        .send(async move {
            let _turn = queued_gate.lock().await;
            let _ = started.send(());
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            "known funded deal"
        })
        .unwrap();
    drop(incoming);
    let control = async {
        started_rx.await.unwrap();
        // This is the foreground capacity read that previously stopped polling its watcher.
        let _foreground = gate.lock().await;
        assert_eq!(results.recv().await, Some("known funded deal"));
        assert_eq!(
            results.recv().await,
            None,
            "closed inputs drain before ending"
        );
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(control, drive_pool_tasks(inputs, outputs));
    })
    .await
    .expect("the older read must run while the control loop waits");
}

#[tokio::test]
async fn issue_2284_dropping_pool_results_cancels_pending_reads() {
    struct OnDrop(Arc<AtomicU64>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let dropped = Arc::new(AtomicU64::new(0));
    let (incoming, inputs) = tokio::sync::mpsc::unbounded_channel();
    let (outputs, results) = tokio::sync::mpsc::unbounded_channel();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let guard = OnDrop(dropped.clone());
    incoming
        .send(async move {
            let _guard = guard;
            let _ = started.send(());
            std::future::pending::<()>().await;
        })
        .unwrap();
    let control = async {
        started_rx.await.unwrap();
        drop(results);
        drop(incoming);
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(control, drive_pool_tasks(inputs, outputs));
    })
    .await
    .expect("shutdown must not leave a read or its FIFO turn alive");
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn issue_2284_funded_partial_match_opens_while_capacity_waits_for_its_read_turn() {
    let root = tempfile::tempdir().unwrap();
    let note = Arc::new(LocalNote::generate());
    let note_addr = format!("0:{}", "a".repeat(64));
    let mut backend = PoolTestBackend::new(
        Arc::new(Mutex::new(Vec::new())),
        format!("0:{}", "1".repeat(64)),
        10,
        8,
        true,
        2,
    );
    backend.shared_read_turn = Some(Arc::new(tokio::sync::Mutex::new(())));
    let backend = Arc::new(backend);
    let seller = dexdo::seller::start_gateway_with_note(
        "127.0.0.1:0".parse().unwrap(),
        dexdo::seller::UpstreamConfig::Mock,
        note,
    )
    .await
    .unwrap();
    let gateway = seller.listen_addr.to_string();
    let deal = SellerPoolDeal {
        chain: backend.clone(),
        cfg: SellerConfig {
            token_contract: backend.token_contract.clone(),
            price_per_tick: backend.price_per_tick,
            max_ticks: backend.offered_ticks,
            subscription: false,
            all_or_none: false,
            gateway_advertise: gateway.clone(),
            mock_token_count: 8,
        },
        watch: dexdo::seller::SellerMatchWatchConfig {
            cursor_path: root.path().join("funded.cursor.json"),
            poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
        },
        upstream: dexdo::seller::UpstreamConfig::Mock,
        nonce: 1,
        market: None,
    };
    let shutdown = async {
        while backend.open_calls.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    .fuse();
    tokio::pin!(shutdown);
    let mut provision = |_: String, _: String, _: u64, _: u64, _: u64| {
        futures::future::ready(Err(anyhow::anyhow!(
            "a funded parent still occupies the only slot"
        )))
    };
    let result = tokio::time::timeout(
        DEFAULT_MATCH_POLL_INTERVAL,
        run_seller_pool(
            &seller,
            vec![deal],
            SellerPoolContext {
                deals_dir: Some(root.path()),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr: &note_addr,
                frame_model: "openai/gpt-oss-20b",
                gateway_advertise: &gateway,
                advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
            },
            &pool_test_policy(1),
            &mut provision,
            shutdown.as_mut(),
            &mut false,
        ),
    )
    .await;
    seller.server_task.abort();
    let _ = seller.server_task.await;
    result
        .expect("foreground capacity must not stop polling the funded match")
        .unwrap();
    assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
    assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
    let fill = dexdo::seller::read_seller_fill_lineage(
        &root.path().join("funded.cursor.json"),
        &backend.token_contract,
    )
    .unwrap()
    .unwrap();
    assert_eq!(fill.matched_ticks, 8);
    assert_eq!(fill.residual_ticks, 2);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueuedPoolRead {
    Capacity,
    ResidualTerms,
    Nonce,
    ProvisionedTerms,
    Discovery,
}

async fn issue_2284_check_stop_cancels_queued_read(stage: QueuedPoolRead) {
    let root = tempfile::tempdir().unwrap();
    let note = Arc::new(LocalNote::generate());
    let note_addr = format!("0:{}", "a".repeat(64));
    let waiting = Arc::new(tokio::sync::Notify::new());
    let mut backend = PoolTestBackend::new(
        Arc::new(Mutex::new(Vec::new())),
        format!("0:{}", "1".repeat(64)),
        10,
        8,
        true,
        2,
    );
    if stage != QueuedPoolRead::ProvisionedTerms {
        backend.queued_read = Some((stage, waiting.clone()));
    }
    backend.shared_read_turn = Some(Arc::new(tokio::sync::Mutex::new(())));
    let backend = Arc::new(backend);
    let mut successor = PoolTestBackend::new(
        Arc::new(Mutex::new(Vec::new())),
        format!("0:{}", "2".repeat(64)),
        2,
        2,
        false,
        2,
    );
    if stage == QueuedPoolRead::ProvisionedTerms {
        successor.queued_read = Some((stage, waiting.clone()));
    }
    let successor = Arc::new(successor);
    let seller = dexdo::seller::start_gateway_with_note(
        "127.0.0.1:0".parse().unwrap(),
        dexdo::seller::UpstreamConfig::Mock,
        note,
    )
    .await
    .unwrap();
    let gateway = seller.listen_addr.to_string();
    let deal = SellerPoolDeal {
        chain: backend.clone(),
        cfg: SellerConfig {
            token_contract: backend.token_contract.clone(),
            price_per_tick: backend.price_per_tick,
            max_ticks: backend.offered_ticks,
            subscription: false,
            all_or_none: false,
            gateway_advertise: gateway.clone(),
            mock_token_count: 8,
        },
        watch: dexdo::seller::SellerMatchWatchConfig {
            cursor_path: root.path().join("funded.cursor.json"),
            poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
        },
        upstream: dexdo::seller::UpstreamConfig::Mock,
        nonce: 1,
        market: None,
    };
    let shutdown = async {
        if stage != QueuedPoolRead::Capacity {
            while backend.open_calls.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            if stage == QueuedPoolRead::Discovery {
                backend.owner_fills.lock().unwrap().push((
                    backend.created_at + 1,
                    MatchedFill {
                        order_id: 99,
                        token_contract: format!("0:{}", "3".repeat(64)),
                        ticks: 2,
                        price_per_tick: u128::from(backend.price_per_tick),
                    },
                ));
            } else {
                // Free the mock parent's capacity after it has opened. Residual terms remain
                // authoritative in its saved lineage when this getter is absent.
                backend.terms_after_open.store(0, Ordering::Relaxed);
                backend.getdeal_gone.store(true, Ordering::Relaxed);
            }
        }
        waiting.notified().await;
    }
    .fuse();
    tokio::pin!(shutdown);
    let mut provisions = 0;
    let mut provision = |owner: String, model: String, nonce: u64, price: u64, ticks: u64| {
        provisions += 1;
        assert!(
            stage == QueuedPoolRead::ProvisionedTerms,
            "STOP must prevent provisioning"
        );
        let market = dexdo_core::MarketManifest {
            network: "mock".into(),
            model_hash: dexdo_core::model_hash_for(&model),
            frame_model: model,
            inference_order_book: format!("0:{}", "d".repeat(64)),
            root_model: format!("0:{}", "e".repeat(64)),
            token_contract: successor.token_contract.clone(),
            seller_note: owner,
            nonce,
            price_per_tick: u128::from(price),
            max_ticks: u128::from(ticks),
            all_or_none: false,
        };
        persist_provision_deal_handle(
            Some(root.path()),
            &market,
            std::path::Path::new("manifest/deployed.manifest.json"),
            true,
        )
        .unwrap();
        futures::future::ready(Ok((market, successor.clone() as Arc<dyn ChainBackend>)))
    };
    let mut shutdown_requested = false;
    let result = tokio::time::timeout(
        dexdo_core::params::MATCH_OPEN_TIMEOUT * 3,
        run_seller_pool(
            &seller,
            vec![deal],
            SellerPoolContext {
                deals_dir: Some(root.path()),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr: &note_addr,
                frame_model: "openai/gpt-oss-20b",
                gateway_advertise: &gateway,
                advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
            },
            &pool_test_policy(1),
            &mut provision,
            shutdown.as_mut(),
            &mut shutdown_requested,
        ),
    )
    .await;
    seller.server_task.abort();
    let _ = seller.server_task.await;
    result
        .expect("STOP must cancel the queued read and start bounded shutdown")
        .unwrap();
    assert!(shutdown_requested);
    assert_eq!(backend.queued_reads_completed.load(Ordering::Relaxed), 0);
    assert_eq!(successor.queued_reads_completed.load(Ordering::Relaxed), 0);
    assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
    assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
    assert_eq!(successor.post_calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        provisions,
        usize::from(stage == QueuedPoolRead::ProvisionedTerms)
    );
    let fill = dexdo::seller::read_seller_fill_lineage(
        &root.path().join("funded.cursor.json"),
        &backend.token_contract,
    )
    .unwrap()
    .unwrap();
    assert_eq!(fill.matched_ticks, 8);
    assert_eq!(fill.residual_ticks, 2);
    if stage == QueuedPoolRead::ProvisionedTerms {
        assert_eq!(fill.replacement_nonce, Some(2));
        assert_eq!(
            fill.replacement_token_contract.as_deref(),
            Some(successor.token_contract.as_str())
        );
        let records = seller_market_handle_records(Some(root.path()), &note_addr).unwrap();
        assert!(records.values().any(|record| record.provision_pending
            && record.market.nonce == 2
            && record.market.token_contract == successor.token_contract));
    } else {
        assert_eq!(fill.replacement_nonce, None);
        assert_eq!(fill.replacement_token_contract, None);
    }
}

#[tokio::test(start_paused = true)]
async fn issue_2284_stop_cancels_a_queued_capacity_read() {
    issue_2284_check_stop_cancels_queued_read(QueuedPoolRead::Capacity).await;
}

#[tokio::test(start_paused = true)]
async fn issue_2284_stop_cancels_queued_residual_terms_before_reserving() {
    issue_2284_check_stop_cancels_queued_read(QueuedPoolRead::ResidualTerms).await;
}

#[tokio::test(start_paused = true)]
async fn issue_2284_stop_cancels_queued_nonce_before_reserving() {
    issue_2284_check_stop_cancels_queued_read(QueuedPoolRead::Nonce).await;
}

#[tokio::test(start_paused = true)]
async fn issue_2284_stop_preserves_a_provisioned_successor_while_cancelling_its_read() {
    issue_2284_check_stop_cancels_queued_read(QueuedPoolRead::ProvisionedTerms).await;
}

#[tokio::test(start_paused = true)]
async fn issue_2284_stop_cancels_queued_owner_fill_verification() {
    issue_2284_check_stop_cancels_queued_read(QueuedPoolRead::Discovery).await;
}
