mod watch_read_recovery_2122 {
    use super::*;

    fn resting_backend() -> RelistBackend {
        let backend = issue_2096_backend(82);
        backend.rows.lock().unwrap()[0].deadline =
            unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs();
        backend.read_outage.store(true, Ordering::SeqCst);
        backend
    }

    async fn wait_for_failed_reads(backend: &RelistBackend, count: u64) {
        while backend.failed_reads.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        assert!(backend.cancelled().is_empty());
        assert!(backend.expire_calls().is_empty());
    }

    #[tokio::test]
    async fn issue_2122_resting_offer_resumes_the_same_identity_after_reads_recover() {
        let backend = resting_backend();
        let seller = relist_seller().await;
        let previous = backend.resting_order_id().unwrap();
        let mut id = identity(&backend.owner, &backend.token_contract, previous);
        let (_dir, watch) = watch("2122-same-order");
        let cfg = issue_2096_cfg(&backend, &seller);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            supervise_and_relist_reporting_identity(
                &seller,
                &backend,
                &cfg,
                &watch,
                &mut id,
                async {
                    wait_for_failed_reads(&backend, 3).await;
                    assert!(!seller.server_task.is_finished());
                    backend.read_outage.store(false, Ordering::SeqCst);
                    while backend.successful_reads.load(Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    assert_eq!(backend.resting_order_id(), Some(previous));
                    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
                },
                relist_timing(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    disposition: CancellationDisposition::Cancelled,
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(id.order_id, previous);
        assert_eq!(backend.cancelled(), vec![previous]);
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn issue_2122_expiry_during_outage_produces_exactly_one_successor() {
        let backend = resting_backend();
        let seller = relist_seller().await;
        wait_for_gateway_ready(&seller).await;
        let previous = backend.resting_order_id().unwrap();
        let mut id = identity(&backend.owner, &backend.token_contract, previous);
        let (_dir, watch) = watch("2122-expired-order");
        let cfg = issue_2096_cfg(&backend, &seller);
        let recovery = supervise_and_relist_reporting_identity(
            &seller,
            &backend,
            &cfg,
            &watch,
            &mut id,
            async { backend.successor_observed.notified().await },
            relist_timing(),
        );
        let outage = async {
            wait_for_failed_reads(&backend, 3).await;
            backend.rows.lock().unwrap()[0].deadline = unix_timestamp() - 1;
            wait_for_failed_reads(&backend, 5).await;
            backend.read_outage.store(false, Ordering::SeqCst);
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(recovery, outage)
        })
        .await
        .unwrap();
        assert!(matches!(
            outcome.unwrap(),
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
        assert_eq!(backend.expire_calls(), vec![previous]);
        assert_ne!(id.order_id, previous);
        assert_eq!(id.order_id, backend.successor().order_id);
        assert_eq!(backend.successor().ticks, 82);
        assert_eq!(
            backend.successor().price_per_tick,
            u128::from(cfg.price_per_tick)
        );
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn issue_2122_match_during_outage_recovers_exact_buyer_without_relisting() {
        let mut backend = resting_backend();
        backend.fill_ticks = Some(82);
        let seller = relist_seller().await;
        let mut id = identity(
            &backend.owner,
            &backend.token_contract,
            backend.resting_order_id().unwrap(),
        );
        let (_dir, watch) = watch("2122-matched-order");
        let cfg = issue_2096_cfg(&backend, &seller);
        let expected = Match {
            price_per_tick: cfg.price_per_tick,
            ..sample_match(&backend.token_contract)
        };
        let recovery = supervise_and_relist_reporting_identity(
            &seller,
            &backend,
            &cfg,
            &watch,
            &mut id,
            std::future::pending(),
            relist_timing(),
        );
        let outage = async {
            wait_for_failed_reads(&backend, 3).await;
            backend.rows.lock().unwrap().clear();
            *backend.offer_posted.lock().unwrap() = false;
            {
                let mut state = backend.state.lock().unwrap();
                state.funded = true;
                state.deposit = u128::from(cfg.price_per_tick) * 82;
            }
            *backend.matched.lock().unwrap() = Some(expected.clone());
            backend.read_outage.store(false, Ordering::SeqCst);
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(recovery, outage)
        })
        .await
        .unwrap();
        let RestingSellerOutcome::Matched(matched) = outcome.unwrap() else {
            panic!("buyer must be recovered")
        };
        assert_eq!(matched.token_contract, expected.token_contract);
        assert_eq!(matched.buyer_pubkey, expected.buyer_pubkey);
        assert_eq!(matched.price_per_tick, expected.price_per_tick);
        crate::seller::recover_known_match_fill(&seller, &backend, &cfg, &watch, &matched)
            .await
            .unwrap();
        let fill = crate::seller::read_seller_fill_lineage(&watch.cursor_path, &cfg.token_contract)
            .unwrap()
            .unwrap();
        assert_eq!(fill.matched_ticks, 82);
        assert_eq!(fill.residual_ticks, 0);
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        assert!(backend.cancelled().is_empty());
        assert!(backend.expire_calls().is_empty());
        assert!(!seller.server_task.is_finished());
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn issue_2122_sustained_outage_stays_unresolved_until_explicit_shutdown() {
        let backend = resting_backend();
        let seller = relist_seller().await;
        let previous = backend.resting_order_id().unwrap();
        let mut id = identity(&backend.owner, &backend.token_contract, previous);
        let (_dir, watch) = watch("2122-unresolved-order");
        let cfg = issue_2096_cfg(&backend, &seller);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            supervise_and_relist_reporting_identity(
                &seller,
                &backend,
                &cfg,
                &watch,
                &mut id,
                wait_for_failed_reads(&backend, 5),
                SupervisionTiming {
                    cycle_timeout: Duration::from_millis(10),
                    ..relist_timing()
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    disposition: CancellationDisposition::UnknownFailure { .. },
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(id.order_id, previous);
        assert_eq!(backend.resting_order_id(), Some(previous));
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        assert!(backend.cancelled().is_empty());
        assert!(backend.expire_calls().is_empty());
        assert!(!seller.server_task.is_finished());
        seller.server_task.abort();
    }
}
