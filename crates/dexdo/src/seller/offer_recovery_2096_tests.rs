include!("issue_2115_pending_tests.rs");

#[tokio::test]
async fn issue_2096_cancelled_successor_does_not_retire_the_sale() {
    let tc = address('e');
    let owner = address('f');
    let backend = issue_2096_backend(86);
    backend.post_failures.store(1, Ordering::SeqCst);
    let previous = backend.resting_order_id().unwrap();
    let mut id = identity(&owner, &tc, previous);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let cfg = issue_2096_cfg(&backend, &seller);
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervise_and_relist_reporting_identity(
            &seller,
            &backend,
            &cfg,
            &watch("2096-regression").1,
            &mut id,
            async {
                loop {
                    if backend.posts.load(Ordering::SeqCst) == 2 && !backend.live_rows().is_empty()
                    {
                        assert_eq!(backend.live_rows().len(), 1);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            },
            SupervisionTiming {
                cycle_timeout: relist_timing().cycle_timeout,
                ..issue_2096_timing()
            },
        ),
    )
    .await
    .expect("the sale must recover without restarting")
    .unwrap();
    assert!(
        matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::Cancelled,
            }
        ),
        "recovery must continue until the operator stops it: {outcome:?}"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 2);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    let postings = backend.postings.lock().unwrap();
    assert_eq!(postings.len(), 3);
    assert_eq!(
        backend.cancelled(),
        vec![postings[1].order_id, postings[2].order_id]
    );
    assert_eq!(id.order_id, postings[2].order_id);
    assert_eq!(postings[2].ticks, 86);
    assert_eq!(postings[2].price_per_tick, u128::from(cfg.price_per_tick));
    assert_eq!(postings[2].token_contract.as_deref(), Some(tc.as_str()));
    assert_eq!(backend.expire_calls(), vec![previous]);
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2115_restart_without_order_id_keeps_startup_until_recovery() {
    let mut backend = issue_2096_backend(100);
    backend.failed_post_lands = false;
    backend.post_failures.store(1, Ordering::SeqCst);
    backend.rows.lock().unwrap().clear();
    *backend.offer_posted.lock().unwrap() = false;
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), prepare_pending_offer_with_timing(
        &seller, &backend, &issue_2096_cfg(&backend, &seller), &backend.owner, None,
        std::future::pending(), issue_2115_timing(),
    )).await.expect("startup must recover an unresolved SELL").unwrap();
    assert!(matches!(outcome, SellerStartupOutcome::Ready(SellerOfferStartup::Posted {
        outcome: Some(dexdo_core::SellOfferOutcome::Rested { .. })
    })), "{outcome:?}");
    assert!(!seller.server_task.is_finished());
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    assert_eq!(backend.live_rows()[0].ticks, 100);
    seller.server_task.abort();
}

fn issue_2096_timing() -> SupervisionTiming {
    SupervisionTiming {
        cycle_timeout: Duration::from_millis(30),
        health_interval: relist_timing().recovery_backoff,
        ..relist_timing()
    }
}

#[tokio::test]
async fn issue_2115_buyer_match_wins_while_identity_less_startup_recovers() {
    let mut backend = issue_2096_backend(100);
    backend.failed_post_lands = false;
    backend.post_failures.store(1, Ordering::SeqCst);
    backend.rows.lock().unwrap().clear();
    *backend.offer_posted.lock().unwrap() = false;
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let cfg = issue_2096_cfg(&backend, &seller);
    let recovery = prepare_pending_offer_with_timing(
        &seller, &backend, &cfg, &backend.owner, None, std::future::pending(), issue_2115_timing(),
    );
    let buyer = async {
        issue_2096_wait_for_post(&backend, 1).await;
        backend.state.lock().unwrap().funded = true;
        *backend.matched.lock().unwrap() = Some(Match {
            price_per_tick: cfg.price_per_tick, ..sample_match(&backend.token_contract)
        });
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(recovery, buyer)
    }).await.unwrap();
    assert!(matches!(outcome.unwrap(), SellerStartupOutcome::Ready(SellerOfferStartup::ResumedFunded)));
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
}

fn issue_2096_backend(ticks: u64) -> RelistBackend {
    let price = dexdo_core::params::PRICE_STEP;
    let backend = RelistBackend::new(&address('e'), &address('f'), ticks)
        .with_deal_terms((price.try_into().unwrap(), ticks));
    for rows in [&backend.rows, &backend.postings] {
        rows.lock().unwrap()[0].price_per_tick = price;
    }
    backend
}

fn issue_2096_cfg(backend: &RelistBackend, seller: &RunningSeller) -> SellerConfig {
    let mut cfg = cfg_for_seller(&backend.token_contract, seller);
    cfg.price_per_tick = dexdo_core::params::PRICE_STEP.try_into().unwrap();
    cfg.max_ticks = backend.deal_terms.unwrap().1;
    cfg
}

async fn issue_2096_run(
    backend: &RelistBackend,
    seller: &RunningSeller,
    shutdown: impl Future<Output = ()>,
) -> RestingSellerOutcome {
    let mut id = identity(
        &backend.owner,
        &backend.token_contract,
        backend.resting_order_id().unwrap(),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        supervise_and_relist_reporting_identity(
            seller,
            backend,
            &issue_2096_cfg(backend, seller),
            &watch("2096-cases").1,
            &mut id,
            shutdown,
            issue_2096_timing(),
        ),
    )
    .await
    .expect("recovery must stay responsive")
    .unwrap()
}

async fn issue_2096_wait_for_post(backend: &RelistBackend, attempts: u64) {
    while backend.posts.load(Ordering::SeqCst) < attempts {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn issue_2096_a_post_that_never_lands_is_retried() {
    let mut backend = issue_2096_backend(86);
    backend.failed_post_lands = false;
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, issue_2096_wait_for_post(&backend, 2)).await;
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
    assert_eq!(backend.posts.load(Ordering::SeqCst), 2);
    assert_eq!(backend.postings.lock().unwrap().len(), 2);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    assert!(
        !seller.server_task.is_finished(),
        "a failed post must preserve the shared gateway"
    );
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_confirmation_failure_is_recovered() {
    let backend = issue_2096_backend(86);
    backend.confirmation_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, issue_2096_wait_for_post(&backend, 2)).await;
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
    assert_eq!(backend.posts.load(Ordering::SeqCst), 2);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_existing_successor_is_adopted_after_unreadable_reconciliation() {
    let backend = issue_2096_backend(86);
    backend.post_failures.store(1, Ordering::SeqCst);
    backend
        .reconciliation_read_failures
        .store(45, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, async {
        while backend.reconciliation_read_failures.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(issue_2096_timing().recovery_backoff * 2).await;
    })
    .await;
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
    assert_eq!(
        backend.posts.load(Ordering::SeqCst),
        1,
        "adopt the landed successor without a second post"
    );
    assert_eq!(backend.cancelled(), vec![backend.successor().order_id]);
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_a_successor_appearing_between_recovery_reads_is_adopted() {
    let mut backend = issue_2096_backend(86);
    backend.failed_post_lands = false;
    backend.post_failures.store(1, Ordering::SeqCst);
    backend
        .late_successor_on_latch_read
        .store(true, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, async {
        while backend.postings.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(issue_2096_timing().recovery_backoff * 2).await;
    })
    .await;
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
    assert_eq!(
        backend.posts.load(Ordering::SeqCst),
        1,
        "a late successor must not terminate recovery or create another offer"
    );
    assert_eq!(backend.cancelled(), vec![backend.successor().order_id]);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_unresolved_cancellation_never_posts_beside_the_successor() {
    let mut backend = issue_2096_backend(86);
    backend.cancel_fails = true;
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, async {
        while backend.cancelled().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(issue_2096_timing().recovery_backoff * 2).await;
    })
    .await;
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
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert_eq!(backend.live_rows().len(), 1);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_a_funded_match_wins_the_failed_post_cancel_race() {
    let tc = address('e');
    let mut backend = issue_2096_backend(86);
    backend.match_on_cancel = Some(Match {
        price_per_tick: dexdo_core::params::PRICE_STEP.try_into().unwrap(),
        ..sample_match(&tc)
    });
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, std::future::pending()).await;
    assert!(matches!(outcome, RestingSellerOutcome::Matched(m) if m.token_contract == tc));
    assert_eq!(
        backend.posts.load(Ordering::SeqCst),
        1,
        "never repost already funded capacity"
    );
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_sustained_outage_backs_off_and_shutdown_stops_recovery() {
    let mut backend = issue_2096_backend(86);
    backend.failed_post_lands = false;
    backend.post_failures.store(u64::MAX, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, issue_2096_wait_for_post(&backend, 3)).await;
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
    assert_eq!(backend.posts.load(Ordering::SeqCst), 3);
    assert!(backend.live_rows().is_empty());
    for times in backend.post_times.lock().unwrap().windows(2) {
        assert!(times[1] - times[0] >= issue_2096_timing().recovery_backoff);
    }
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_shutdown_after_cleanup_prevents_the_next_post() {
    let backend = issue_2096_backend(86);
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, async {
        while backend.cancelled().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
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
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert!(backend.live_rows().is_empty());
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_shutdown_reconciles_the_unobserved_successor() {
    let backend = issue_2096_backend(86);
    backend.post_failures.store(1, Ordering::SeqCst);
    backend
        .reconciliation_read_failures
        .store(100, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, async {
        issue_2096_wait_for_post(&backend, 1).await;
        tokio::time::sleep(issue_2096_timing().cycle_timeout * 2).await;
        backend
            .reconciliation_read_failures
            .store(0, Ordering::SeqCst);
    })
    .await;
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
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert_eq!(backend.cancelled(), vec![backend.successor().order_id]);
    assert!(backend.live_rows().is_empty());
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_contract_rejection_is_not_an_outage_retry() {
    let mut backend = issue_2096_backend(86);
    backend.reject_post = true;
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, std::future::pending()).await;
    assert!(
        matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Watcher(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2096_upstream_blip_after_cancelled_post_preserves_sale_intent() {
    let (url, probes, upstream) = scripted_http_server(vec![
        ("200 OK", healthy_sse(), Duration::ZERO),
        ("200 OK", "data: {\"error\":{\"message\":\"temporary upstream outage during offer recovery\"}}\n\n".to_string(), Duration::ZERO),
        ("200 OK", healthy_sse(), Duration::ZERO),
    ]);
    let backend = issue_2096_backend(86);
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, async {
        issue_2096_wait_for_post(&backend, 2).await;
        assert!(
            probes.load(Ordering::SeqCst) >= 3,
            "readiness must recover before posting"
        );
        let live = backend.live_rows();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].price_per_tick, dexdo_core::params::PRICE_STEP);
        assert_eq!(live[0].ticks, 86);
        assert_eq!(
            live[0].token_contract.as_ref(),
            Some(&backend.token_contract)
        );
    })
    .await;
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
    assert_eq!(backend.posts.load(Ordering::SeqCst), 2);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    let posted = backend.postings.lock().unwrap();
    assert_eq!(
        backend.cancelled(),
        vec![posted[1].order_id, posted[2].order_id]
    );
    let post_times = backend.post_times.lock().unwrap();
    assert!(post_times[1] - post_times[0] >= issue_2096_timing().recovery_backoff);
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2096_unavailable_upstream_keeps_recovery_off_book_until_shutdown() {
    let (url, probes, upstream) = scripted_http_server(vec![
        ("200 OK", healthy_sse(), Duration::ZERO),
        ("200 OK", "data: {\"error\":{\"message\":\"upstream remains unavailable during offer recovery\"}}\n\n".to_string(), Duration::ZERO),
    ]);
    let backend = issue_2096_backend(86);
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let outcome = issue_2096_run(&backend, &seller, async {
        while probes.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            1,
            "unavailable readiness must not publish another SELL"
        );
        assert!(backend.live_rows().is_empty());
        assert_eq!(backend.cancelled(), vec![backend.successor().order_id]);
    })
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
    assert!(backend.live_rows().is_empty());
    assert!(
        !seller.server_task.is_finished(),
        "shutdown must preserve the shared gateway"
    );
    seller.server_task.abort();
    upstream.abort();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]
    #[test]
    fn issue_2096_recovery_preserves_one_order_and_authoritative_capacity(
        failures in 1_u64..=3, lands in any::<bool>(), ticks in 2_u64..=256,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut backend = issue_2096_backend(ticks);
            backend.failed_post_lands = lands;
            backend.post_failures.store(failures, Ordering::SeqCst);
            let seller = relist_seller().await;
            wait_for_gateway_ready(&seller).await;
            let outcome = issue_2096_run(&backend, &seller, issue_2096_wait_for_post(&backend, failures + 1)).await;
            assert!(matches!(outcome, RestingSellerOutcome::Stopped { reason: RestingStopReason::Shutdown, .. }), "{outcome:?}");
            assert_eq!(backend.posts.load(Ordering::SeqCst), failures + 1);
            assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
            for row in backend.postings.lock().unwrap().iter() {
                assert_eq!(row.ticks, u128::from(ticks));
                assert_eq!(row.price_per_tick, dexdo_core::params::PRICE_STEP);
                assert_eq!(row.token_contract.as_deref(), Some(backend.token_contract.as_str()));
            }
            seller.server_task.abort();
        });
    }
}

fn issue_2115_timing() -> SupervisionTiming {
    SupervisionTiming {
        health_interval: relist_timing().recovery_backoff,
        ..relist_timing()
    }
}

async fn issue_2115_pending_shutdown_case(funded: bool, during_readiness: bool) {
    let backend = issue_2096_backend(100);
    backend.posts.store(1, Ordering::SeqCst);
    for rows in [&backend.rows, &backend.postings] {
        rows.lock().unwrap()[0].deadline =
            unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs();
    }
    let order_id = backend.resting_order_id().unwrap();
    if funded {
        backend.rows.lock().unwrap().clear();
        backend.state.lock().unwrap().funded = true;
        *backend.matched.lock().unwrap() = Some(Match {
            price_per_tick: dexdo_core::params::PRICE_STEP.try_into().unwrap(),
            ..sample_match(&backend.token_contract)
        });
    }
    let requested = Arc::new(tokio::sync::Notify::new());
    let mut upstream_task = None;
    let seller = if during_readiness {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let notify = requested.clone();
        upstream_task = Some(tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 8192];
            assert!(socket.read(&mut bytes).await.unwrap() > 0);
            notify.notify_one();
            std::future::pending::<()>().await;
        }));
        super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap()
    } else {
        relist_seller().await
    };
    wait_for_gateway_ready(&seller).await;
    let cfg = issue_2096_cfg(&backend, &seller);
    let shutdown = async {
        if during_readiness { requested.notified().await; }
    };
    let recovery = async {
        if during_readiness {
            // Reach readiness promptly instead of sleeping the production health cadence.
            prepare_pending_offer_with_timing(
                &seller, &backend, &cfg, &backend.owner, None, shutdown, issue_2115_timing(),
            ).await
        } else {
            recover_pending_seller_offer(
                &seller, &backend, &cfg, &backend.owner, shutdown, AdvertiseProbePolicy::default(),
            ).await
        }
    };
    let outcome = tokio::time::timeout(Duration::from_secs(5), recovery)
        .await.expect("pending watcher shutdown must remain bounded").unwrap();
    if funded {
        assert!(
            matches!(
                outcome,
                SellerStartupOutcome::Ready(SellerOfferStartup::ResumedFunded)
            ),
            "{outcome:?}"
        );
        assert!(backend.cancelled().is_empty());
    } else {
        assert!(
            matches!(outcome, SellerStartupOutcome::Stopped {
            identity: Some(ref id), reason: RestingStopReason::Shutdown,
            disposition: CancellationDisposition::Cancelled,
        } if id.order_id == order_id),
            "{outcome:?}"
        );
        assert!(backend.live_rows().is_empty());
        assert_eq!(backend.cancelled(), vec![order_id]);
    }
    assert_eq!(
        backend.posts.load(Ordering::SeqCst),
        1,
        "shutdown cannot repost"
    );
    assert!(
        !seller.server_task.is_finished(),
        "pool still owns gateway/service"
    );
    seller.server_task.abort();
    if let Some(task) = upstream_task {
        task.abort();
    }
}

#[tokio::test]
async fn issue_2115_pending_shutdown_before_first_poll_cancels_exact_ask() {
    issue_2115_pending_shutdown_case(false, false).await;
}

#[tokio::test]
async fn issue_2115_pending_shutdown_before_first_poll_serves_funded_match() {
    issue_2115_pending_shutdown_case(true, false).await;
}

#[tokio::test]
async fn issue_2115_pending_shutdown_during_readiness_cancels_exact_ask() {
    issue_2115_pending_shutdown_case(false, true).await;
}

#[tokio::test]
async fn issue_2115_pending_shutdown_during_readiness_serves_funded_match() {
    issue_2115_pending_shutdown_case(true, true).await;
}

#[tokio::test]
async fn issue_2115_pending_shutdown_with_unknown_execution_retains_ownership() {
    let backend = issue_2096_backend(100).reaped_by_another_actor();
    backend.posts.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    let cfg = issue_2096_cfg(&backend, &seller);
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_pending_offer_with_timing(
            &seller,
            &backend,
            &cfg,
            &backend.owner,
            None,
            std::future::ready(()),
            issue_2115_timing(),
        ),
    )
    .await
    .expect("unresolved shutdown is bounded")
    .unwrap();
    assert!(
        matches!(
            outcome,
            SellerStartupOutcome::Stopped {
                identity: None,
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::UnknownFailure { .. },
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2115_pending_shutdown_during_unresolved_recovery_never_reposts() {
    let mut backend = issue_2096_backend(100).reaped_by_another_actor();
    backend.failed_post_lands = false;
    backend.post_failures.store(1, Ordering::SeqCst);
    let seller = relist_seller().await;
    wait_for_gateway_ready(&seller).await;
    let cfg = issue_2096_cfg(&backend, &seller);
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_pending_offer_with_timing(
            &seller,
            &backend,
            &cfg,
            &backend.owner,
            None,
            issue_2096_wait_for_post(&backend, 1),
            issue_2115_timing(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(
            outcome,
            SellerStartupOutcome::Stopped {
                identity: None,
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::UnknownFailure { .. },
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
}
