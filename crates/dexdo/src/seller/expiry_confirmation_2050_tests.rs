mod expiry_confirmation_2050_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn landed_expiry_is_confirmed_while_its_reply_is_stalled() {
        let owner = address('e');
        let tc = address('f');
        let mut backend = RelistBackend::new(&tc, &owner, 8);
        backend.expiry_reply_pending = true;
        let order_id = backend.resting_order_id().unwrap();
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        let timing = SellerLivenessParams::canonical();
        let started = tokio::time::Instant::now();
        let decision = reap_expired_offer(
            &backend,
            &cfg,
            &identity(&owner, &tc, order_id),
            started + timing.offer_reap_timeout,
            timing.offer_reap_poll,
        )
        .await;
        assert!(
            matches!(decision, RelistDecision::Relist { .. }),
            "{decision:?}"
        );
        assert_eq!(started.elapsed(), timing.offer_reap_poll);
        assert_eq!(backend.expire_calls(), vec![order_id]);
        seller.server_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn duplicate_and_transient_read_failures_reconcile_the_effect() {
        let owner = address('e');
        let tc = address('f');
        let mut backend = RelistBackend::new(&tc, &owner, 8);
        backend.expiry_duplicate = true;
        backend.reap_read_errors.store(2, Ordering::SeqCst);
        let order_id = backend.resting_order_id().unwrap();
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        let timing = SellerLivenessParams::canonical();
        let started = tokio::time::Instant::now();
        let decision = reap_expired_offer(
            &backend,
            &cfg,
            &identity(&owner, &tc, order_id),
            started + timing.offer_reap_timeout,
            timing.offer_reap_poll,
        )
        .await;
        assert!(
            matches!(decision, RelistDecision::Relist { .. }),
            "{decision:?}"
        );
        assert_eq!(started.elapsed(), timing.offer_reap_poll * 3);
        assert_eq!(backend.expire_calls(), vec![order_id]);
        seller.server_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_duplicate_without_removal_never_authorizes_relist() {
        let owner = address('e');
        let tc = address('f');
        let mut backend = RelistBackend::new(&tc, &owner, 8);
        backend.expiry_is_ignored = true;
        backend.expiry_duplicate = true;
        let order_id = backend.resting_order_id().unwrap();
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        let timing = SellerLivenessParams::canonical();
        let started = tokio::time::Instant::now();
        let decision = reap_expired_offer(
            &backend,
            &cfg,
            &identity(&owner, &tc, order_id),
            started + timing.offer_reap_timeout,
            timing.offer_reap_poll,
        )
        .await;
        match decision {
            RelistDecision::Unproven { known_result } => {
                assert!(known_result.contains("DUPLICATE_MESSAGE"), "{known_result}");
                assert!(known_result.contains("still_in_book"), "{known_result}");
            }
            other => panic!("duplicate alone must not authorize relist: {other:?}"),
        }
        assert_eq!(started.elapsed(), timing.offer_reap_timeout);
        assert_eq!(backend.expire_calls(), vec![order_id]);
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        assert_eq!(backend.live_rows().len(), 1);
        seller.server_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_reply_does_not_bypass_the_offer_latch() {
        let owner = address('e');
        let tc = address('f');
        let mut backend = RelistBackend::new(&tc, &owner, 8);
        backend.expiry_reply_pending = true;
        backend.latch_release_after_reads = u64::MAX;
        let order_id = backend.resting_order_id().unwrap();
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        let timing = SellerLivenessParams::canonical();
        let started = tokio::time::Instant::now();
        let decision = reap_expired_offer(
            &backend,
            &cfg,
            &identity(&owner, &tc, order_id),
            started + timing.offer_reap_timeout,
            timing.offer_reap_poll,
        )
        .await;
        match decision {
            RelistDecision::Unproven { known_result } => {
                assert!(
                    known_result.contains("offer_latch_still_posted"),
                    "{known_result}"
                );
                assert!(
                    known_result.contains("expiry_submit=timeout"),
                    "{known_result}"
                );
            }
            other => panic!("a posted latch must prevent relist: {other:?}"),
        }
        assert_eq!(started.elapsed(), timing.offer_reap_timeout);
        assert_eq!(backend.expire_calls(), vec![order_id]);
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        seller.server_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn another_bounded_attempt_recovers_without_repeating_a_landed_expiry() {
        let owner = address('e');
        let tc = address('f');
        let mut backend = RelistBackend::new(&tc, &owner, 8);
        backend.expiry_reply_pending = true;
        let timing = SellerLivenessParams::canonical();
        let reads = timing.offer_reap_timeout.as_secs() / timing.offer_reap_poll.as_secs() - 1;
        backend.reap_read_errors.store(reads, Ordering::SeqCst);
        let order_id = backend.resting_order_id().unwrap();
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        let identity = identity(&owner, &tc, order_id);
        let first = reap_expired_offer(
            &backend,
            &cfg,
            &identity,
            tokio::time::Instant::now() + timing.offer_reap_timeout,
            timing.offer_reap_poll,
        )
        .await;
        assert!(
            matches!(first, RelistDecision::Unproven { .. }),
            "{first:?}"
        );
        assert_eq!(backend.reap_read_errors.load(Ordering::SeqCst), 0);
        let recovered = reap_expired_offer(
            &backend,
            &cfg,
            &identity,
            tokio::time::Instant::now() + timing.offer_reap_timeout,
            timing.offer_reap_poll,
        )
        .await;
        assert!(
            matches!(recovered, RelistDecision::Relist { .. }),
            "{recovered:?}"
        );
        assert_eq!(backend.expire_calls(), vec![order_id]);
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn stalled_expiry_reply_still_posts_exactly_one_successor() {
        let owner = address('e');
        let tc = address('f');
        let mut backend = RelistBackend::new(&tc, &owner, 8);
        backend.expiry_reply_pending = true;
        backend.reap_read_errors.store(2, Ordering::SeqCst);
        let order_id = backend.resting_order_id().unwrap();
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        let successor_observed = backend.successor_observed.clone();
        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, order_id),
            async move { successor_observed.notified().await },
            cfg,
        )
        .await;
        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.expire_calls(), vec![order_id]);
        assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
        assert_eq!(backend.cancelled(), vec![backend.successor().order_id]);
        assert_ne!(backend.successor().order_id, order_id);
        seller.server_task.abort();
    }
}
