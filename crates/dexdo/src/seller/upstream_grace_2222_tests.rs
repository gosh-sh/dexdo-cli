// Exercise production supervision and the real HTTP/SSE adapter with a deterministic chain.
async fn issue_2222_assert_transient_recovery(script: Vec<(&'static str, String, Duration)>) {
    let recovery_probe = script.len() as u64;
    let (url, probes, upstream) = scripted_http_server(script);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let backend = issue_2077_backend();
    let original = backend.resting_order_id().unwrap();
    let mut id = identity(&backend.owner, &backend.token_contract, original);
    let (_cursor, watch) = watch("2222-transient");
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        supervise_and_relist_reporting_identity(
            &seller,
            &backend,
            &issue_2077_cfg(&seller, &backend),
            &watch,
            &mut id,
            async {
                // Starting the next probe proves the healthy one completed and resumed supervision.
                while probes.load(Ordering::SeqCst) <= recovery_probe {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(
                    backend.cancelled().is_empty(),
                    "transient probes cancelled a live SELL"
                );
                assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
            },
            SupervisionTiming {
                health_interval: Duration::from_millis(20),
                health_timeout: Duration::from_millis(150),
                upstream_grace: Duration::from_secs(2),
                cycle_timeout: Duration::from_secs(2),
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("transient recovery remains responsive")
    .unwrap();
    assert!(matches!(
        outcome,
        RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Shutdown,
            disposition: CancellationDisposition::Cancelled,
        }
    ));
    assert_eq!(id.order_id, original);
    assert_eq!(
        backend.cancelled(),
        vec![original],
        "only operator shutdown cancels"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2222_repeated_timeouts_recover_without_order_churn() {
    let mut script = vec![("200 OK", healthy_sse(), Duration::from_millis(600)); 3];
    script.push(("200 OK", healthy_sse(), Duration::ZERO));
    issue_2222_assert_transient_recovery(script).await;
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn issue_2222_transient_sequences_never_cancel_or_repost(statuses in prop::collection::vec(0u8..3, 1..8)) {
        let statuses = statuses.into_iter().map(|status| match status {
            0 => "429 Too Many Requests",
            1 => "502 Bad Gateway",
            _ => "503 Service Unavailable",
        });
        let mut script: Vec<_> = statuses.map(|status|
            (status, "{}".to_string(), Duration::ZERO)).collect();
        script.push(("200 OK", healthy_sse(), Duration::ZERO));
        tokio::runtime::Runtime::new().unwrap().block_on(issue_2222_assert_transient_recovery(script));
    }
}

#[tokio::test]
async fn issue_2222_persistent_errors_and_hanging_retry_exhaust_one_fixed_grace() {
    for hanging_retry in [false, true] {
        let mut script = vec![("429 Too Many Requests", "{}".to_string(), Duration::ZERO)];
        if hanging_retry {
            script.push(("200 OK", healthy_sse(), Duration::from_secs(10)));
        }
        let (url, probes, upstream) = scripted_http_server(script);
        let seller = issue_2077_seller(url).await;
        wait_for_gateway_ready(&seller).await;
        let backend = issue_2077_backend();
        let original = backend.resting_order_id().unwrap();
        let id = identity(&backend.owner, &backend.token_contract, original);
        let (_cursor, watch) = watch("2222-persistent");
        let grace = Duration::from_millis(250);
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            supervise_with_timing(
                &seller,
                &backend,
                &issue_2077_cfg(&seller, &backend),
                &watch,
                &id,
                std::future::pending(),
                SupervisionTiming {
                    health_interval: Duration::from_millis(20),
                    health_timeout: Duration::from_secs(2),
                    upstream_grace: grace,
                    cycle_timeout: Duration::from_secs(3),
                    abort_gateway_on_stop: false,
                    ..fast_timing()
                },
            ),
        )
        .await
        .expect("neither 429 nor a hanging retry extends grace")
        .unwrap();
        assert!(started.elapsed() >= grace);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "probe timeout cannot postpone grace"
        );
        assert!(matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Health(HealthFailure {
                    component: HealthComponent::UpstreamModel,
                    ..
                }),
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.cancelled(), vec![original]);
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        assert!(probes.load(Ordering::SeqCst) >= 2);
        assert!(!seller.server_task.is_finished());
        seller.server_task.abort();
        upstream.abort();
    }
}

#[tokio::test]
async fn issue_2222_success_clears_grace_before_a_separate_outage() {
    let failure = ("503 Service Unavailable", "{}".to_string(), Duration::ZERO);
    let healthy = ("200 OK", healthy_sse(), Duration::ZERO);
    let (url, probes, upstream) =
        scripted_http_server(vec![failure.clone(), healthy.clone(), failure, healthy]);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let backend = issue_2077_backend();
    let id = identity(
        &backend.owner,
        &backend.token_contract,
        backend.resting_order_id().unwrap(),
    );
    let (_cursor, watch) = watch("2222-reset");
    // Observe the deadlines themselves: real HTTP/TLS scheduling must not decide whether
    // a 50ms margin expired. A successful probe must make the next outage start a new window.
    let _ = take_observed_cycle_deadlines();
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        supervise_with_timing(
            &seller,
            &backend,
            &issue_2077_cfg(&seller, &backend),
            &watch,
            &id,
            async {
                while probes.load(Ordering::SeqCst) < 5 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(backend.cancelled().is_empty());
            },
            SupervisionTiming {
                upstream_grace: SellerLivenessParams::canonical().upstream_unavailable_grace,
                abort_gateway_on_stop: false,
                ..fast_timing()
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
                ..
            }
        ),
        "{outcome:?}"
    );
    let deadlines: Vec<_> = take_observed_cycle_deadlines()
        .into_iter()
        .filter_map(|(site, deadline)| {
            (site == CycleDeadlineSite::UpstreamGrace).then_some(deadline)
        })
        .collect();
    assert_eq!(deadlines.len(), 2, "one grace for each scripted outage");
    assert!(
        deadlines[1] > deadlines[0],
        "full readiness must clear the first outage's grace deadline"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2222_persistent_failure_recovers_one_successor_with_same_aon_terms() {
    let mut script = vec![("503 Service Unavailable", "{}".to_string(), Duration::ZERO); 8];
    script.push(("200 OK", healthy_sse(), Duration::ZERO));
    let (url, _, upstream) = scripted_http_server(script);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let backend = issue_2077_backend();
    for rows in [&backend.rows, &backend.postings] {
        rows.lock().unwrap()[0].flags |= dexdo_core::order_flags::AON;
    }
    let original = backend.resting_order_id().unwrap();
    let mut id = identity(&backend.owner, &backend.token_contract, original);
    let mut cfg = issue_2077_cfg(&seller, &backend);
    cfg.all_or_none = true;
    let (_cursor, watch) = watch("2222-successor");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervise_and_relist_reporting_identity(
            &seller,
            &backend,
            &cfg,
            &watch,
            &mut id,
            async { backend.successor_observed.notified().await },
            SupervisionTiming {
                health_interval: Duration::from_millis(20),
                upstream_grace: Duration::from_millis(100),
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        outcome,
        RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Shutdown,
            ..
        }
    ));
    assert_eq!(backend.posts.load(Ordering::SeqCst), 1);
    assert_eq!(backend.max_rows_seen.load(Ordering::SeqCst), 1);
    assert_eq!(backend.cancelled(), vec![original, id.order_id]);
    assert_ne!(id.order_id, original);
    let rows = backend.postings.lock().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].flags, rows[1].flags);
    assert_eq!(rows[0].price_per_tick, rows[1].price_per_tick);
    assert_eq!(rows[0].ticks, rows[1].ticks);
    drop(rows);
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2222_match_wins_the_persistent_failure_cancellation_race() {
    let (url, _, upstream) = scripted_http_server(vec![(
        "503 Service Unavailable",
        "{}".to_string(),
        Duration::ZERO,
    )]);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let mut backend = issue_2077_backend();
    backend.match_on_cancel = Some(Match {
        price_per_tick: dexdo_core::params::PRICE_STEP.try_into().unwrap(),
        ..sample_match(&backend.token_contract)
    });
    let mut id = identity(
        &backend.owner,
        &backend.token_contract,
        backend.resting_order_id().unwrap(),
    );
    let (_cursor, watch) = watch("2222-match-race");
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        supervise_and_relist_reporting_identity(
            &seller,
            &backend,
            &issue_2077_cfg(&seller, &backend),
            &watch,
            &mut id,
            std::future::pending(),
            SupervisionTiming {
                upstream_grace: Duration::from_millis(100),
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(outcome, RestingSellerOutcome::Matched(_)));
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}

#[test]
fn issue_2222_canonical_grace_is_sixty_seconds() {
    assert_eq!(
        SellerLivenessParams::canonical().upstream_unavailable_grace,
        Duration::from_secs(60)
    );
}

#[tokio::test]
async fn issue_2222_non_transient_failure_does_not_wait_for_grace() {
    for status in ["401 Unauthorized", "403 Forbidden", "404 Not Found"] {
        let (url, probes, upstream) =
            scripted_http_server(vec![(status, "{}".to_string(), Duration::ZERO)]);
        let seller = issue_2077_seller(url).await;
        wait_for_gateway_ready(&seller).await;
        let backend = issue_2077_backend();
        let original = backend.resting_order_id().unwrap();
        let id = identity(&backend.owner, &backend.token_contract, original);
        let (_cursor, watch) = watch("2222-fatal");
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            supervise_with_timing(
                &seller,
                &backend,
                &issue_2077_cfg(&seller, &backend),
                &watch,
                &id,
                std::future::pending(),
                SupervisionTiming {
                    upstream_grace: SellerLivenessParams::canonical().upstream_unavailable_grace,
                    abort_gateway_on_stop: false,
                    ..fast_timing()
                },
            ),
        )
        .await
        .expect("a deterministic failure must not wait for grace")
        .unwrap();
        assert!(matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Health(HealthFailure {
                    component: HealthComponent::UpstreamModel,
                    ..
                }),
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.cancelled(), vec![original]);
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        seller.server_task.abort();
        upstream.abort();
    }
}

#[tokio::test]
async fn issue_2222_match_and_shutdown_interrupt_an_in_flight_grace_probe() {
    for match_arrives in [false, true] {
        let (url, probes, upstream) = scripted_http_server(vec![
            ("503 Service Unavailable", "{}".to_string(), Duration::ZERO),
            ("200 OK", healthy_sse(), Duration::from_secs(10)),
        ]);
        let seller = issue_2077_seller(url).await;
        wait_for_gateway_ready(&seller).await;
        let mut backend = issue_2077_backend();
        backend.fill_ticks = Some(8);
        let id = identity(
            &backend.owner,
            &backend.token_contract,
            backend.resting_order_id().unwrap(),
        );
        let (_cursor, mut watch) = watch("2222-interrupt");
        watch.poll_interval = Duration::from_millis(5);
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            supervise_with_timing(
                &seller,
                &backend,
                &issue_2077_cfg(&seller, &backend),
                &watch,
                &id,
                async {
                    while probes.load(Ordering::SeqCst) < 2 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    if match_arrives {
                        *backend.matched.lock().unwrap() = Some(Match {
                            price_per_tick: dexdo_core::params::PRICE_STEP.try_into().unwrap(),
                            ..sample_match(&backend.token_contract)
                        });
                        backend.state.lock().unwrap().funded = true;
                        backend.state_reads.store(1, Ordering::SeqCst);
                        backend.rows.lock().unwrap().clear();
                        *backend.offer_posted.lock().unwrap() = false;
                        std::future::pending::<()>().await;
                    }
                },
                SupervisionTiming {
                    health_interval: Duration::from_millis(20),
                    health_timeout: Duration::from_secs(5),
                    upstream_grace: Duration::from_secs(5),
                    cycle_timeout: Duration::from_secs(6),
                    abort_gateway_on_stop: false,
                    ..fast_timing()
                },
            ),
        )
        .await
        .expect("a readiness future cannot block match or shutdown")
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        if match_arrives {
            assert!(matches!(outcome, RestingSellerOutcome::Matched(_)));
            assert!(backend.cancelled().is_empty());
        } else {
            assert!(matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ));
            assert_eq!(backend.cancelled(), vec![id.order_id]);
        }
        assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
        seller.server_task.abort();
        upstream.abort();
    }
}

#[tokio::test]
async fn issue_2222_expiry_interrupts_an_in_flight_grace_probe() {
    let (url, _, upstream) = scripted_http_server(vec![
        ("503 Service Unavailable", "{}".to_string(), Duration::ZERO),
        ("200 OK", healthy_sse(), Duration::from_secs(10)),
    ]);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let backend = issue_2077_backend();
    backend.rows.lock().unwrap()[0].deadline = unix_timestamp() - 1;
    let id = identity(
        &backend.owner,
        &backend.token_contract,
        backend.resting_order_id().unwrap(),
    );
    let (_cursor, watch) = watch("2222-expiry");
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_with_timing(
            &seller,
            &backend,
            &issue_2077_cfg(&seller, &backend),
            &watch,
            &id,
            std::future::pending(),
            SupervisionTiming {
                health_interval: Duration::from_millis(20),
                health_timeout: Duration::from_secs(2),
                upstream_grace: Duration::from_secs(2),
                cycle_timeout: Duration::from_secs(3),
                expiry_poll: Duration::from_millis(100),
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("expiry cannot wait for the upstream probe")
    .unwrap();
    assert!(matches!(
        outcome,
        RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Expired(_),
            disposition: CancellationDisposition::NotAttemptedExpired,
        }
    ));
    assert!(backend.cancelled().is_empty());
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2222_a_hanging_book_read_cannot_extend_upstream_grace() {
    let (url, _, upstream) = scripted_http_server(vec![(
        "503 Service Unavailable",
        "{}".to_string(),
        Duration::ZERO,
    )]);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let tc = address('7');
    let owner = address('8');
    let id = identity(&owner, &tc, 11);
    let backend = CancelBackend::new(
        vec![order(id.order_id, &owner, &tc)],
        owner,
        id.order_id,
        CancelBehavior::Remove,
    )
    .with_hanging_reads();
    let (_cursor, watch) = watch("2222-book-deadline");
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch,
            &id,
            std::future::pending(),
            SupervisionTiming {
                health_interval: Duration::from_millis(20),
                health_timeout: Duration::from_millis(200),
                upstream_grace: Duration::from_secs(1),
                cycle_timeout: Duration::from_secs(1),
                expiry_poll: Duration::from_millis(500),
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("the book read cannot postpone a known outage deadline")
    .unwrap();
    assert!(matches!(
        outcome,
        RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Health(HealthFailure {
                component: HealthComponent::UpstreamModel,
                ..
            }),
            disposition: CancellationDisposition::UnknownFailure { .. },
        }
    ));
    assert!(
        backend.calls().is_empty(),
        "an unreadable book does not authorize a blind cancellation"
    );
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    assert!(!seller.server_task.is_finished());
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2222_pending_book_read_keeps_readiness_probes_running() {
    let (url, probes, upstream) =
        scripted_http_server(vec![("200 OK", healthy_sse(), Duration::from_millis(60))]);
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let tc = address('7');
    let owner = address('8');
    let id = identity(&owner, &tc, 11);
    let backend = CancelBackend::new(
        vec![order(id.order_id, &owner, &tc)],
        owner,
        id.order_id,
        CancelBehavior::Remove,
    )
    .with_hanging_reads();
    let (_cursor, watch) = watch("2222-progress");
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch,
            &id,
            async {
                while probes.load(Ordering::SeqCst) < 3 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(backend.calls().is_empty());
            },
            SupervisionTiming {
                health_interval: Duration::from_millis(20),
                health_timeout: Duration::from_secs(1),
                cycle_timeout: Duration::from_millis(200),
                expiry_poll: Duration::from_millis(50),
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("a pending book read cannot stop polling readiness")
    .unwrap();
    assert!(matches!(
        outcome,
        RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Shutdown,
            disposition: CancellationDisposition::UnknownFailure { .. },
        }
    ));
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}

/// A working HTTPS provider whose certificate is not in the production client's trust store.
async fn issue_2222_https_provider() -> (String, reqwest::Certificate, tokio::task::JoinHandle<()>)
{
    use tokio_rustls::rustls::{pki_types::PrivatePkcs8KeyDer, ServerConfig};
    super::super::tls::ensure_crypto_provider();
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let root = reqwest::Certificate::from_der(certificate.cert.der()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let Ok(mut tls) = acceptor.accept(socket).await else {
                // The production client rejects this untrusted certificate before HTTP.
                continue;
            };
            let mut request = [0_u8; 8192];
            assert!(tls.read(&mut request).await.unwrap() > 0);
            let body = healthy_sse();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            tls.write_all(response.as_bytes()).await.unwrap();
            tls.shutdown().await.unwrap();
        }
    });
    (url, root, task)
}

#[tokio::test]
async fn issue_2222_upstream_certificate_refusal_cancels_without_grace() {
    let (url, root, upstream) = issue_2222_https_provider().await;
    // Positive TLS control: trusting this certificate permits the ordinary healthy SSE response.
    let response = reqwest::Client::builder()
        .add_root_certificate(root)
        .build()
        .unwrap()
        .post(format!("{url}/chat/completions"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), healthy_sse());

    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let backend = issue_2077_backend();
    let original = backend.resting_order_id().unwrap();
    let id = identity(&backend.owner, &backend.token_contract, original);
    let (_cursor, watch) = watch("2222-upstream-certificate");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervise_with_timing(
            &seller,
            &backend,
            &issue_2077_cfg(&seller, &backend),
            &watch,
            &id,
            std::future::pending(),
            SupervisionTiming {
                upstream_grace: SellerLivenessParams::canonical().upstream_unavailable_grace,
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("a certificate refusal must cancel the exact SELL without grace")
    .unwrap();
    let RestingSellerOutcome::Stopped {
        reason: RestingStopReason::Health(failure),
        disposition: CancellationDisposition::Cancelled,
    } = outcome
    else {
        panic!("expected protective cancellation, got {outcome:?}");
    };
    assert_eq!(failure.component, HealthComponent::UpstreamModel);
    assert!(
        !failure.timed_out,
        "the provider refused TLS, not a timeout"
    );
    assert!(!failure.is_transient_upstream());
    assert_eq!(backend.cancelled(), vec![original]);
    assert!(backend.live_rows().is_empty());
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    // The seller gateway remains healthy throughout the provider's certificate refusal.
    wait_for_gateway_ready(&seller).await;
    seller.server_task.abort();
    upstream.abort();
}

#[tokio::test]
async fn issue_2222_temporary_upstream_connection_failure_keeps_grace() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let attempts = Arc::new(AtomicU64::new(0));
    let counter = attempts.clone();
    let upstream = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            drop(socket); // A reset/EOF before TLS gives no certificate verdict.
        }
    });
    let seller = issue_2077_seller(url).await;
    wait_for_gateway_ready(&seller).await;
    let backend = issue_2077_backend();
    let original = backend.resting_order_id().unwrap();
    let id = identity(&backend.owner, &backend.token_contract, original);
    let (_cursor, watch) = watch("2222-upstream-connection");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        supervise_with_timing(
            &seller,
            &backend,
            &issue_2077_cfg(&seller, &backend),
            &watch,
            &id,
            async {
                while attempts.load(Ordering::SeqCst) < 3 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(backend.cancelled().is_empty());
                assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
            },
            SupervisionTiming {
                upstream_grace: SellerLivenessParams::canonical().upstream_unavailable_grace,
                abort_gateway_on_stop: false,
                ..fast_timing()
            },
        ),
    )
    .await
    .expect("temporary connection failures must keep the resting SELL")
    .unwrap();
    assert!(matches!(
        outcome,
        RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Shutdown,
            disposition: CancellationDisposition::Cancelled,
        }
    ));
    assert_eq!(backend.cancelled(), vec![original], "only shutdown cancels");
    assert_eq!(backend.posts.load(Ordering::SeqCst), 0);
    seller.server_task.abort();
    upstream.abort();
}
