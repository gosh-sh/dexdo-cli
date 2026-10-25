/// A provider answer is enough to withdraw the exact offer, but must not permanently
/// remove healthy capacity from the market. Recovery uses the real HTTP/SSE adapter.
#[tokio::test]
async fn issue_2077_any_upstream_error_recovers_the_cancelled_offer() {
    for (status, body) in [
        ("200 OK", "data: {\"error\":{\"message\":\"Our servers are currently overloaded. Please try again later.\"}}\n\n"),
        ("503 Service Unavailable\r\nRetry-After: 1", "{\"error\":{\"message\":\"maintenance\"}}"),
        ("401 Unauthorized", "{\"error\":{\"message\":\"credential rejected\"}}"),
        ("200 OK", "data: {\"error\":{\"message\":\"an entirely different provider failure\"}}\n\n"),
    ] {
        // Retryable HTTP statuses first exhaust the existing timeout tolerance.
        // The provider's explicit delay exceeds fast_timing().health_timeout, so each
        // scripted 503 occupies a separate probe. Full jitter may otherwise consume
        // both failures in one probe and recover without withdrawing the offer.
        let attempts = if status.starts_with("429") || status.starts_with("503") {
            SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS as usize
        } else {
            1
        };
        let mut script = vec![(status, body.to_string(), Duration::ZERO); attempts];
        script.push(("200 OK", healthy_sse(), Duration::ZERO));
        let (base_url, probes, upstream) = scripted_http_server(script);
        let tc = address('7');
        let owner = address('8');
        let backend = issue_2077_backend();
        let mut id = identity(&owner, &tc, backend.resting_order_id().unwrap());
        let previous = id.order_id;
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        wait_for_gateway_ready(&seller).await;
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            supervise_and_relist_reporting_identity(
                &seller,
                &backend,
                &issue_2077_cfg(&seller, &backend),
                &watch("2077-recover").1,
                &mut id,
                async { backend.successor_observed.notified().await },
                SupervisionTiming {
                    abort_gateway_on_stop: false,
                    ..fast_timing()
                },
            ),
        )
        .await
        .expect("recovered provider must put the offer back")
        .unwrap();
        assert!(
            matches!(outcome, RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::Cancelled,
            }),
            "{status}: expected recovery followed by operator shutdown, got {outcome:?}"
        );
        assert_eq!(backend.posts.load(Ordering::SeqCst), 1, "{status}");
        assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
        assert_eq!(backend.cancelled(), vec![previous, id.order_id]);
        assert_ne!(previous, id.order_id, "the pool must receive the successor identity");
        assert!(probes.load(Ordering::SeqCst) >= 2);
        assert!(backend.live_rows().is_empty());
        assert!(backend.expire_calls().is_empty(), "cancellation needs no expiry write");
        seller.server_task.abort();
        upstream.abort();
    }
}

fn issue_2077_backend() -> RelistBackend {
    let backend = RelistBackend::new(&address('7'), &address('8'), 8);
    let backend = backend.with_deal_terms((dexdo_core::params::PRICE_STEP.try_into().unwrap(), 8));
    for rows in [&backend.rows, &backend.postings] {
        let mut rows = rows.lock().unwrap();
        rows[0].deadline = unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs();
        rows[0].price_per_tick = dexdo_core::params::PRICE_STEP;
    }
    backend
}

fn issue_2077_cfg(seller: &RunningSeller, backend: &RelistBackend) -> SellerConfig {
    let mut cfg = cfg_for_seller(&backend.token_contract, seller);
    cfg.price_per_tick = dexdo_core::params::PRICE_STEP.try_into().unwrap();
    cfg
}

async fn issue_2077_run(
    seller: &RunningSeller,
    backend: &RelistBackend,
    shutdown: impl Future<Output = ()>,
) -> (RestingOfferIdentity, RestingSellerOutcome) {
    wait_for_gateway_ready(seller).await;
    let mut id = identity(
        &backend.owner,
        &backend.token_contract,
        backend.resting_order_id().unwrap(),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        supervise_and_relist_reporting_identity(
            seller,
            backend,
            &issue_2077_cfg(seller, backend),
            &watch("2077-cases").1,
            &mut id,
            shutdown,
            SupervisionTiming {
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("recovery must remain responsive to shutdown")
    .unwrap();
    (id, outcome)
}

async fn issue_2077_seller(base_url: String) -> RunningSeller {
    super::super::start_gateway_with_note(
        "127.0.0.1:0".parse().unwrap(),
        openai(base_url),
        Arc::new(LocalNote::generate()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn issue_2077_persistent_failure_stays_off_book_and_shutdown_stops_retries() {
    let (url, probes, upstream) = scripted_http_server(vec![(
        "200 OK",
        "data: {\"error\":{\"message\":\"still unavailable\"}}\n\n".to_string(),
        Duration::ZERO,
    )]);
    let backend = issue_2077_backend();
    let original = backend.resting_order_id().unwrap();
    let seller = issue_2077_seller(url).await;
    let (_, outcome) = issue_2077_run(&seller, &backend, async {
        while probes.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            backend.live_rows().is_empty(),
            "failed probes must leave no SELL"
        );
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
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
    assert_eq!(backend.cancelled(), vec![original]);
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    assert!(probes.load(Ordering::SeqCst) >= 4);
    tokio::task::yield_now().await;
    assert!(
        !seller.server_task.is_finished(),
        "paused-deal shutdown must preserve the shared gateway"
    );
    wait_for_gateway_ready(&seller).await;
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2077_failure_after_reposting_is_recovered_again() {
    let failure = (
        "200 OK",
        "data: {\"error\":{\"message\":\"temporary failure\"}}\n\n".to_string(),
        Duration::ZERO,
    );
    for post_confirmation_failure in [false, true] {
        let healthy = ("200 OK", healthy_sse(), Duration::ZERO);
        let mut script = vec![failure.clone(), healthy.clone()];
        if !post_confirmation_failure {
            script.push(healthy.clone());
        }
        script.push(failure.clone());
        script.push(healthy);
        let (url, _, upstream) = scripted_http_server(script);
        let backend = issue_2077_backend();
        let seller = issue_2077_seller(url).await;
        let (id, outcome) = issue_2077_run(&seller, &backend, async {
            while backend.posts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
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
        assert_eq!(backend.posts.load(Ordering::SeqCst), 2);
        assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
        assert_eq!(
            id.order_id,
            backend.postings.lock().unwrap().last().unwrap().order_id
        );
        assert_eq!(backend.cancelled().len(), 3);
        assert!(backend.live_rows().is_empty());
        seller.server_task.abort();
        upstream.abort();
    }
}

#[tokio::test]
async fn issue_2077_expiry_successor_post_probe_failure_recovers() {
    let healthy = ("200 OK", healthy_sse(), Duration::ZERO);
    let (url, _, upstream) = scripted_http_server(vec![
        healthy.clone(),
        (
            "200 OK",
            "data: {\"error\":{\"message\":\"provider failed after expiry successor was posted\"}}\n\n".to_string(),
            Duration::ZERO,
        ),
        healthy,
    ]);
    let backend = issue_2077_backend();
    for rows in [&backend.rows, &backend.postings] {
        rows.lock().unwrap()[0].deadline = unix_timestamp() - 1;
    }
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let original = backend.resting_order_id().unwrap();
    let mut id = identity(&backend.owner, &backend.token_contract, original);
    let (_cursor_dir, watch) = watch("2077-expiry-upstream");
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        supervise_and_relist_reporting_identity(
            &seller,
            &backend,
            &issue_2077_cfg(&seller, &backend),
            &watch,
            &mut id,
            async {
                while backend.posts.load(Ordering::SeqCst) < 2 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            },
            SupervisionTiming {
                health_interval: Duration::from_secs(1),
                expiry_poll: Duration::from_millis(1),
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("expiry successor recovery must remain responsive")
    .unwrap();
    tokio::task::yield_now().await;
    let gateway_alive = !seller.server_task.is_finished();
    assert!(
        matches!(outcome, RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Shutdown,
            disposition: CancellationDisposition::Cancelled,
        }),
        "expiry successor must recover before shutdown: {outcome:?}; posts={}; shared_gateway_alive={gateway_alive}",
        backend.posts.load(Ordering::SeqCst),
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 2);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    assert_eq!(backend.expire_calls(), vec![original]);
    {
        let posted = backend.postings.lock().unwrap();
        assert_eq!(id.order_id, posted.last().unwrap().order_id);
        assert_eq!(backend.cancelled(), vec![posted[1].order_id, id.order_id]);
        for successor in posted.iter().skip(1) {
            assert_eq!(
                successor.token_contract.as_ref(),
                Some(&backend.token_contract)
            );
            assert_eq!(successor.price_per_tick, dexdo_core::params::PRICE_STEP);
            assert_eq!(successor.ticks, 8);
        }
    }
    assert!(backend.live_rows().is_empty());
    assert!(
        gateway_alive,
        "shared gateway must survive recovery and shutdown"
    );
    wait_for_gateway_ready(&seller).await;
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2077_rejected_cancellation_never_starts_recovery_or_posts() {
    let (url, probes, upstream) = scripted_http_server(vec![(
        "200 OK",
        "data: {\"error\":{\"message\":\"failure\"}}\n\n".to_string(),
        Duration::ZERO,
    )]);
    let mut backend = issue_2077_backend();
    backend.reject_cancellation = true;
    let original = backend.resting_order_id().unwrap();
    let seller = issue_2077_seller(url).await;
    let (_, outcome) = issue_2077_run(&seller, &backend, std::future::pending()).await;
    assert!(
        matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                disposition: CancellationDisposition::RejectedStillResting { .. },
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(backend.resting_order_id(), Some(original));
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2077_match_during_cancellation_is_served_without_relisting() {
    let (url, _, upstream) = scripted_http_server(vec![(
        "200 OK",
        "data: {\"error\":{\"message\":\"failure\"}}\n\n".to_string(),
        Duration::ZERO,
    )]);
    let mut backend = issue_2077_backend();
    backend.match_on_cancel = Some(sample_match(&backend.token_contract));
    let seller = issue_2077_seller(url).await;
    let (_, outcome) = issue_2077_run(&seller, &backend, std::future::pending()).await;
    assert!(
        matches!(outcome, RestingSellerOutcome::Matched(_)),
        "{outcome:?}"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn issue_2077_recovery_never_duplicates_live_capacity(failures in 1usize..8, message in "[a-z ]{1,40}") {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let failure = ("200 OK",
                format!("data: {}\n\n", serde_json::json!({"error": {"message": message}})),
                Duration::ZERO);
            let mut script = vec![failure; failures];
            script.push(("200 OK", healthy_sse(), Duration::ZERO));
            let (url, probes, upstream) = scripted_http_server(script);
            let backend = issue_2077_backend();
            let seller = issue_2077_seller(url).await;
            let (_, outcome) = issue_2077_run(&seller, &backend, async {
                backend.successor_observed.notified().await;
            }).await;
            assert!(matches!(outcome, RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Shutdown, ..
            }), "{outcome:?}");
            assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
            assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
            assert!(backend.live_rows().is_empty());
            assert!(probes.load(Ordering::SeqCst) > failures as u64);
            seller.server_task.abort();
            upstream.abort();
        });
    }
}


#[tokio::test]
async fn issue_2135_repeated_throttling_keeps_the_same_resting_offer() {
    let (url, probes, upstream) = scripted_http_server(vec![
        ("429 Too Many Requests", "{}".into(), Duration::ZERO),
        ("429 Too Many Requests", "{}".into(), Duration::ZERO),
        ("200 OK", healthy_sse(), Duration::ZERO),
    ]);
    let backend = issue_2077_backend();
    let original = backend.resting_order_id().unwrap();
    let seller = issue_2077_seller(url).await;
    let (_, outcome) = issue_2077_run(&seller, &backend, async {
        while probes.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(backend.resting_order_id(), Some(original));
        assert!(backend.cancelled().is_empty(), "429 must not withdraw a resting SELL");
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        assert!(!seller.server_task.is_finished());
    }).await;
    assert!(matches!(outcome, RestingSellerOutcome::Stopped {
        reason: RestingStopReason::Shutdown, ..
    }), "{outcome:?}");
    assert_eq!(backend.cancelled(), vec![original], "only operator shutdown cancels");
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}


#[test]
fn issue_2135_throttling_never_overrides_gateway_or_untyped_health_failures() {
    for component in [HealthComponent::UpstreamModel, HealthComponent::AdvertisedGateway, HealthComponent::GatewayTask] {
        for http in [401, 429, 503] {
            for typed in [false, true] {
                let status = tonic::Status::unavailable(format!("upstream HTTP {http} failed"));
                let status = if typed { dexdo_proto::retryable_upstream_status(status) } else { status };
                let failure = HealthFailure::new(component, false, "failure").with_source(status);
                assert_eq!(failure.is_upstream_throttled(), component == HealthComponent::UpstreamModel && http == 429 && typed);
            }
        }
    }
}
