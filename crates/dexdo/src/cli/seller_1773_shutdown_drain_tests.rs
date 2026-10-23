// a watcher that is told to stop must RETURN, because the pool DRAINS its watchers.

// The pool does not drop its watchers when the operator's signal arrives -- it awaits every one of
// them (`run_seller_pool`, the `watched.next().await` loop), and that is deliberate: a dropped
// watcher takes the identity a relist advanced with it, and the sweep would then prove the absence
// of a generation the book has already consumed.

// So an un-stoppable watcher makes the drain unbounded. Measured live on the test chain before the fix:
// the signal reached the pool, the shutdown arm fired, and the process then ran on for a further
// 591 log lines of chain traffic with no cancellation, no terminal and no `stopping` event --
// because the watcher had been handed `futures::future::pending()` as its stop.

// THE FIXTURE IS THE POINT. This builds its own backend with `matched = false`, so nothing crosses
// the ask and the supervisor keeps watching until something stops it. That is what makes the test
// able to fail: with a matched backend the watcher returns down the match path and never consults
// the stop at all, which is exactly why the pre-existing signal regression
// (`seller_sigint_emits_shutdown_jsonl`) passes over this defect -- measured, its `watched` is
// EMPTY at the drain, so it drains nothing.

/// a fired stop must end the watch instead of leaving the pool's drain unbounded.
#[tokio::test]
async fn issue_1773_signalled_watcher_returns_instead_of_hanging() {
    let root = tempfile::tempdir().expect(" seller pool directory");
    let note = Arc::new(LocalNote::generate());
    let note_addr = format!("0:{}", "a".repeat(64));
    // `matched = false`: no fill exists, so the supervisor stays in its watch and only the pool's
    // stop can end it. This is the arrangement the live wedge had.
    let chain = Arc::new(
        PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            format!("0:{}", "1".repeat(64)),
            8,
            0,
            false,
            i64::MAX - 4,
        )
        // Resting and unexpired are what put the watcher on the branch where only a stop ends it.
        // `true` is the speed opt-in: it changes no assertion, it collapses the 60s cancel budget.
        .resting_unexpired(true),
    );
    let seller = dexdo::seller::start_gateway_with_note(
        "127.0.0.1:0".parse().unwrap(),
        dexdo::seller::UpstreamConfig::Mock,
        note,
    )
    .await
    .unwrap();
    let gateway = seller.listen_addr.to_string();
    let cfg = SellerConfig {
        token_contract: chain.token_contract.clone(),
        price_per_tick: chain.price_per_tick,
        max_ticks: chain.offered_ticks,
        subscription: false,
        gateway_advertise: gateway.clone(),
        mock_token_count: 8,
    };
    let watch = dexdo::seller::SellerMatchWatchConfig {
        cursor_path: root.path().join("issue-1773.cursor.json"),
        poll_interval: std::time::Duration::from_millis(1),
    };
    let identity = dexdo::seller::liveness::RestingOfferIdentity {
        owner_note: note_addr,
        token_contract: cfg.token_contract.clone(),
        order_id: 1,
    };
    let deal = SellerPoolDeal {
        chain: chain.clone(),
        cfg,
        watch,
        upstream: dexdo::seller::UpstreamConfig::Mock,
        nonce: 10,
        market: None,
    };

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (fill_tx, _fill_rx) = tokio::sync::mpsc::unbounded_channel();
    // Fired BEFORE the watch begins, so this asserts that a stop is observed at all rather than
    // that it wins a race.
    stop_tx
        .send(true)
        .expect("the receiver handed to the watcher keeps this sender live");

    // The timeout is the assertion: with the defect present this future never completes, so the
    // test's own bound is what turns the hang into a failure instead of a hung suite.
    let (_deal, _identity, outcome) = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        watch_pool_deal(
            &seller,
            deal,
            Some(identity),
            None,
            fill_tx,
            dexdo::seller::liveness::AdvertiseProbePolicy::default(),
            stop_rx,
        ),
    )
    .await
    .expect(
        ": a watcher handed a fired stop must return; hanging here is the defect itself, \
         because the pool's drain awaits this future unconditionally",
    );

    match outcome.expect("a stopped watch reports an outcome rather than an error") {
        dexdo::seller::liveness::RestingSellerOutcome::Stopped { reason, .. } => assert!(
            matches!(
                reason,
                dexdo::seller::liveness::RestingStopReason::Shutdown
            ),
            "a watcher stopped by the pool's signal must say so: {reason:?}"
        ),
        other => panic!("expected a Stopped outcome for a fired stop, got {other:?}"),
    }
}
