async fn issue_2108_check_pool_history_latency(delay: std::time::Duration) {
    let root = tempfile::tempdir().unwrap();
    let note = Arc::new(LocalNote::generate());
    let note_addr = format!("0:{}", "a".repeat(64));
    let mut backend = PoolTestBackend::new(
        Arc::new(Mutex::new(Vec::new())),
        format!("0:{}", "1".repeat(64)),
        8,
        8,
        true,
        2,
    );
    backend.owner_fill_delay = delay;
    let backend = Arc::new(backend);
    let mut seller = dexdo::seller::start_gateway_with_note(
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
            gateway_advertise: gateway.clone(),
            mock_token_count: 8,
        },
        watch: dexdo::seller::SellerMatchWatchConfig {
            cursor_path: root.path().join("history.cursor.json"),
            poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
        },
        upstream: dexdo::seller::UpstreamConfig::Mock,
        nonce: 1,
        market: None,
    };
    let started = tokio::time::Instant::now();
    let shutdown = tokio::time::sleep(DEFAULT_MATCH_POLL_INTERVAL * 5).fuse();
    tokio::pin!(shutdown);
    let mut provisions = 0;
    let mut provision = |_: String, _: u64, _: u64, _: u64| {
        provisions += 1;
        futures::future::ready(Err(anyhow::anyhow!("no residual for a full match")))
    };
    let mut shutdown_requested = false;
    let outcome = tokio::time::timeout(
        DEFAULT_MATCH_POLL_INTERVAL * 6,
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
    let opens = backend.open_calls.load(Ordering::Relaxed);
    let polls = backend.owner_fill_polls.load(Ordering::Relaxed);
    seller.server_task.abort();
    let _ = (&mut seller.server_task).await;
    eprintln!("history latency={delay:?}; elapsed={:?}; opens={opens}; polls={polls}; outcome={outcome:?}", started.elapsed());
    outcome
        .expect("pool must handle its stop during history discovery")
        .unwrap();
    assert!(shutdown_requested);
    assert_eq!(opens, 1, "serve the funded match exactly once");
    assert!(
        (3..=8).contains(&polls),
        "exercise repeated scans without a busy loop: {polls}"
    );
    assert_eq!(provisions, 0);
    assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
    assert_eq!(backend.nonce_read_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn issue_2108_pool_serves_match_and_stops_during_long_successful_history() {
    // Pace the virtual clock so real loopback TLS also gets CPU and IO time.
    let clock_driver = tokio::spawn(async {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1));
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
    });
    issue_2108_check_pool_history_latency(std::time::Duration::ZERO).await;
    issue_2108_check_pool_history_latency(
        DEFAULT_MATCH_POLL_INTERVAL + std::time::Duration::from_secs(1),
    )
    .await;
    clock_driver.abort();
}
