#[tokio::test]
async fn issue_2115_restart_shutdown_must_not_forget_a_prior_submission() {
    let backend = issue_2096_backend(100);
    backend.posts.store(1, Ordering::SeqCst);
    backend.pending_submission.store(true, Ordering::SeqCst);
    backend.rows.lock().unwrap().clear();
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let cfg = issue_2096_cfg(&backend, &seller);
    assert!(matches!(
        inspect_seller_offer(&backend, &cfg, Some(&backend.owner))
            .await
            .unwrap(),
        SellerOfferInspection::Vacant
    ));
    tokio::time::pause(); // Only reconciliation timers are virtual; TLS warm-up uses real I/O.
    let result = prepare_seller_offer_with_liveness(
        &seller,
        &backend,
        &cfg,
        &backend.owner,
        None,
        std::future::ready(()),
        true,
        AdvertiseProbePolicy::default(),
    )
    .await
    .unwrap();
    eprintln!("restart with prior submission but no visible row: {result:?}");
    seller.server_task.abort();
    assert!(!matches!(result, SellerStartupOutcome::Stopped {
        disposition: CancellationDisposition::AlreadyAbsent, ..
    }), "the pool's cold entry must distinguish a restored outstanding SELL from one never submitted");
}

#[tokio::test]
async fn issue_2115_getter_outage_must_not_end_pending_mode_as_unknown_watcher() {
    let backend = issue_2096_backend(100);
    backend.posts.store(1, Ordering::SeqCst);
    backend.pending_submission.store(true, Ordering::SeqCst);
    backend.rows.lock().unwrap().clear();
    backend
        .reconciliation_read_failures
        .store(u64::MAX, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let cfg = issue_2096_cfg(&backend, &seller);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        prepare_pending_offer_with_timing(
            &seller,
            &backend,
            &cfg,
            &backend.owner,
            None,
            std::future::pending(),
            issue_2115_timing(),
        ),
    )
    .await;
    eprintln!(
        "getter outage result={result:?}; posts={}",
        backend.posts.load(Ordering::SeqCst)
    );
    seller.server_task.abort();
    assert!(result.is_err(), "without shutdown or authoritative resolution the pending watcher must not exit as Watcher+Unknown for the pool to rearm in known-match mode");
}

#[tokio::test]
async fn issue_2115_pending_failures_reconcile_resting_funded_and_unknown_fates() {
    for failure in ["cover", "advertise", "gateway"] {
        for fate in ["resting", "funded", "unknown"] {
            let backend = issue_2096_backend(100);
            backend.posts.store(1, Ordering::SeqCst);
            backend.pending_submission.store(true, Ordering::SeqCst);
            for rows in [&backend.rows, &backend.postings] {
                rows.lock().unwrap()[0].deadline =
                    unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs();
            }
            if fate != "resting" {
                backend.rows.lock().unwrap().clear();
            }
            if fate == "funded" {
                backend.state.lock().unwrap().funded = true;
                *backend.matched.lock().unwrap() = Some(Match {
                    price_per_tick: dexdo_core::params::PRICE_STEP.try_into().unwrap(),
                    ..sample_match(&backend.token_contract)
                });
            }
            backend
                .cover_failure
                .store(failure == "cover", Ordering::SeqCst);
            let seller = relist_seller().await;
            wait_for_gateway_ready(&seller).await;
            let mut cfg = issue_2096_cfg(&backend, &seller);
            if failure == "advertise" {
                cfg.gateway_advertise = "https://127.0.0.1:1".into();
            }
            if failure == "gateway" {
                seller.server_task.abort();
            }
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                prepare_pending_offer_with_timing(
                    &seller,
                    &backend,
                    &cfg,
                    &backend.owner,
                    None,
                    tokio::time::sleep(Duration::from_millis(100)),
                    issue_2115_timing(),
                ),
            )
            .await
            .expect("pending failures and shutdown must remain bounded")
            .unwrap();
            match fate {
                "resting" => {
                    assert!(
                        matches!(
                            result,
                            SellerStartupOutcome::Stopped {
                                disposition: CancellationDisposition::Cancelled,
                                ..
                            }
                        ),
                        "{failure}: {result:?}"
                    );
                    assert_eq!(backend.cancelled().len(), 1);
                    assert!(backend.live_rows().is_empty());
                }
                "funded" => assert!(
                    matches!(
                        result,
                        SellerStartupOutcome::Ready(SellerOfferStartup::ResumedFunded)
                    ),
                    "{failure}: {result:?}"
                ),
                _ => assert!(
                    matches!(
                        result,
                        SellerStartupOutcome::Stopped {
                            reason: RestingStopReason::Shutdown,
                            disposition: CancellationDisposition::UnknownFailure { .. },
                            ..
                        }
                    ),
                    "{failure}: {result:?}"
                ),
            }
            assert_eq!(
                backend.posts.load(Ordering::SeqCst),
                1,
                "{failure}/{fate}: cannot repost"
            );
            if failure != "gateway" {
                assert!(!seller.server_task.is_finished());
            }
            seller.server_task.abort();
        }
    }
}

#[tokio::test]
async fn issue_2115_pending_cover_failure_preserves_a_concurrent_match() {
    let mut backend = issue_2096_backend(100);
    backend.posts.store(1, Ordering::SeqCst);
    backend.cover_failure.store(true, Ordering::SeqCst);
    backend.match_on_cancel = Some(Match {
        price_per_tick: dexdo_core::params::PRICE_STEP.try_into().unwrap(),
        ..sample_match(&backend.token_contract)
    });
    backend.rows.lock().unwrap()[0].deadline =
        unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs();
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let result = tokio::time::timeout(Duration::from_secs(5), prepare_pending_offer_with_timing(
        &seller,
        &backend,
        &issue_2096_cfg(&backend, &seller),
        &backend.owner,
        None,
        std::future::pending(),
        issue_2115_timing(),
    ))
    .await
    .expect("pending recovery must keep the concurrent match")
    .unwrap();
    assert!(
        matches!(
            result,
            SellerStartupOutcome::Ready(SellerOfferStartup::ResumedFunded)
        ),
        "{result:?}"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2115_removing_a_visible_ask_does_not_release_an_unknown_submission() {
    let backend = issue_2096_backend(100);
    backend.posts.store(1, Ordering::SeqCst);
    backend.pending_submission.store(true, Ordering::SeqCst);
    backend.checkpoint_fails.store(true, Ordering::SeqCst);
    backend.cover_failure.store(true, Ordering::SeqCst);
    backend.rows.lock().unwrap()[0].deadline =
        unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs();
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_pending_offer_with_timing(
            &seller,
            &backend,
            &issue_2096_cfg(&backend, &seller),
            &backend.owner,
            None,
            async {
                while backend.cancelled().is_empty() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            },
            issue_2115_timing(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(
            result,
            SellerStartupOutcome::Stopped {
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::UnknownFailure { .. },
                ..
            }
        ),
        "{result:?}"
    );
    assert!(backend.live_rows().is_empty());
    assert!(backend.pending_submission.load(Ordering::SeqCst));
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
}


#[tokio::test]
async fn issue_2264_fresh_timeout_yields_to_other_sales_and_adopts_late_order() {
    let mut backend = issue_2096_backend(100);
    backend.failed_post_lands = false;
    backend.rows.lock().unwrap().clear();
    *backend.offer_posted.lock().unwrap() = false;
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let cfg = issue_2096_cfg(&backend, &seller);
    let outcome = prepare_seller_offer_with_timing(
        &seller, &backend, &cfg, &backend.owner, None, std::future::pending(),
        issue_2115_timing(),
    ).await.unwrap();
    assert!(matches!(outcome, SellerStartupOutcome::Stopped {
        identity: None,
        reason: RestingStopReason::OfferPost(_),
        disposition: CancellationDisposition::UnknownFailure { .. },
    }));
    assert!(backend.pending_submission.load(Ordering::SeqCst));

    let other_tc = address('7');
    let other = CancelBackend::new(Vec::new(), backend.owner.clone(), 2264, CancelBehavior::Remove);
    let other_cfg = cfg_for_seller(&other_tc, &seller);
    let other_outcome = prepare_seller_offer_with_timing(
        &seller, &other, &other_cfg, &backend.owner, None, std::future::pending(),
        issue_2115_timing(),
    ).await.unwrap();
    assert!(matches!(other_outcome, SellerStartupOutcome::Ready(_)));
    assert_eq!(other.order_ids(), vec![2264]);
    assert!(backend.pending_submission.load(Ordering::SeqCst));

    backend.rest_offer(unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs(), 100);
    let order_id = backend.resting_order_id().unwrap();
    let recovered = prepare_pending_offer_with_timing(
        &seller, &backend, &cfg, &backend.owner, None, std::future::pending(),
        issue_2115_timing(),
    ).await.unwrap();
    assert!(matches!(recovered, SellerStartupOutcome::Ready(
        SellerOfferStartup::ResumedResting { order_id: observed }) if observed == order_id));
    assert!(backend.cancelled().is_empty());
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    seller.server_task.abort();
}
