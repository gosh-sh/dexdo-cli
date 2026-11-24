// Candidate admission must not own the pool's shared listener.
struct Issue2147Fixture {
    root: tempfile::TempDir,
    seller: dexdo::seller::RunningSeller,
    buyer: dexdo::buyer::Buyer,
    active_chain: MockChainBackend,
    active_tc: String,
    owner: String,
    candidate: Arc<PoolTestBackend>,
    deal: SellerPoolDeal,
    attempts: Arc<AtomicU64>,
    provider: tokio::task::JoinHandle<()>,
}

impl Drop for Issue2147Fixture {
    fn drop(&mut self) {
        self.seller.server_task.abort();
        self.provider.abort();
    }
}

impl Issue2147Fixture {
    async fn new() -> Self {
        use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
        let attempts = Arc::new(AtomicU64::new(0));
        let app = Router::new().route("/v1/chat/completions", post(
            |State(attempts): State<Arc<AtomicU64>>,
             Json(request): Json<serde_json::Value>| async move {
                if request["model"] == "candidate" {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    return http::StatusCode::TOO_MANY_REQUESTS.into_response();
                }
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                );
                ([(http::header::CONTENT_TYPE, "text/event-stream")], body).into_response()
            }
        )).with_state(attempts.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let provider = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let upstream = |model: &str| {
            dexdo::seller::UpstreamConfig::OpenAi(dexdo::seller::OpenAiConfig {
                base_url: url.clone(),
                model: model.to_string(),
                frame_model: "vendor--exact--v1".to_string(),
                api_key_env: "PATH".to_string(),
                identity_aliases: Vec::new(),
            })
        };
        let note = Arc::new(LocalNote::generate());
        let owner = format!("0:{}", hex::encode(note.pubkey().ed));
        let seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            upstream("active"),
            note.clone(),
        )
        .await
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let active_chain = MockChainBackend::new(
            root.path().join("active.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let active_tc = format!("0:{}", "a".repeat(64));
        let price_per_tick = dexdo_core::PRICE_STEP.try_into().unwrap();
        active_chain
            .post_offer(
                SellOffer {
                    token_contract: active_tc.clone(),
                    price_per_tick,
                    max_ticks: 4,
                    flags: 0,
                },
                note.as_ref(),
            )
            .await
            .unwrap();
        let buyer = dexdo::buyer::Buyer::generate();
        buyer.place_buy(&active_chain, &active_tc).await.unwrap();
        let cfg = SellerConfig {
            token_contract: active_tc.clone(),
            price_per_tick,
            max_ticks: 4,
            subscription: false,
            all_or_none: false,
            gateway_advertise: seller.listen_addr.to_string(),
            mock_token_count: 4,
        };
        dexdo::seller::poll_match_and_maybe_open(
            &seller,
            &active_chain,
            &cfg,
            &root.path().join("active.cursor.json"),
        )
        .await
        .unwrap()
        .expect("A is opened before B is admitted");
        let candidate = Arc::new(PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            format!("0:{}", "b".repeat(64)),
            4,
            4,
            false,
            1,
        ));
        let mut deal = advertise_pool_deal(
            candidate.clone(),
            &seller.listen_addr.to_string(),
            root.path().join("candidate.cursor.json"),
        );
        deal.upstream = upstream("candidate");
        Self {
            root,
            seller,
            buyer,
            active_chain,
            active_tc,
            owner,
            candidate,
            deal,
            attempts,
            provider,
        }
    }

    fn context(&self) -> SellerPoolContext<'_> {
        SellerPoolContext {
            deals_dir: Some(self.root.path()),
            contracts: std::path::Path::new("manifest/deployed.manifest.json"),
            note_addr: &self.owner,
            frame_model: "vendor--exact--v1",
            gateway_advertise: &self.deal.cfg.gateway_advertise,
            advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::Required,
        }
    }

    async fn request_a(&self) {
        let handover = self
            .buyer
            .resolve_endpoint(&self.active_chain, &self.active_tc)
            .await
            .unwrap();
        // Buyer creates a fresh pinned-TLS connection and validates the terminal usage through EOF.
        let body = serde_json::json!({
            "model": "vendor--exact--v1",
            "messages": [{ "role": "user", "content": "Say OK" }],
            "max_tokens": 8,
            "stream": true,
        });
        let answer = self
            .buyer
            .exchange(
                &handover,
                &self.active_tc,
                dexdo::api_format::ApiFormat::ChatCompletions,
                serde_json::to_vec(&body).unwrap(),
                8,
                None,
                None,
            )
            .await
            .expect("A must still serve after B's admission failure");
        assert_eq!(answer.tokens.concat(), "OK");
        assert_eq!(answer.usage.total_tokens, 2);
    }
}

#[tokio::test]
async fn issue_2147_failed_pool_candidate_preserves_active_requests_and_can_recover() {
    let mut fixture = Issue2147Fixture::new().await;
    fixture.deal.market = Some(dexdo_core::MarketManifest {
        network: "mock".to_string(),
        frame_model: "vendor--exact--v1".to_string(),
        model_hash: dexdo_core::model_hash_for("vendor--exact--v1"),
        inference_order_book: "mock".to_string(),
        root_model: "mock".to_string(),
        token_contract: fixture.deal.cfg.token_contract.clone(),
        seller_note: fixture.owner.clone(),
        nonce: fixture.deal.nonce,
        price_per_tick: fixture.deal.cfg.price_per_tick.into(),
        max_ticks: fixture.deal.cfg.max_ticks.into(),
        all_or_none: false,
    });
    save_pool_deal_handle(&fixture.context(), &fixture.deal).unwrap();
    let record = seller_market_handle_records(Some(fixture.root.path()), &fixture.owner)
        .unwrap()
        .into_values()
        .next()
        .unwrap();
    deals::persist_provision_pending(&record.path, true).unwrap();
    let recovery_before = std::fs::read(&record.path).unwrap();
    fixture.request_a().await;
    let state_before = std::fs::read(fixture.root.path().join("active.chainstate.json")).unwrap();
    for attempt in 1..=2 {
        fixture.candidate.inspection_fail_once.store(true, Ordering::Relaxed);
        let shutdown = std::future::pending::<()>().fuse();
        tokio::pin!(shutdown);
        let mut shutdown_requested = false;
        let error = prepare_pool_deal_reporting_identity(
            &fixture.seller,
            &fixture.deal,
            &fixture.context(),
            false,
            shutdown.as_mut(),
            &mut shutdown_requested,
        )
        .await
        .err()
        .expect("B's chain read failure must refuse admission");
        assert!(format!("{error:#}").contains("injected restart after provision"), "{error:#}");
        assert_eq!(fixture.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 0);
        assert!(!fixture.candidate.cancel_submitted.load(Ordering::Relaxed));
        assert!(!shutdown_requested);
        assert_eq!(
            std::fs::read(&record.path).unwrap(),
            recovery_before,
            "B retains its durable pending handle and nonce"
        );
        assert!(!fixture.seller.server_task.is_finished());
        assert_eq!(
            std::fs::read(fixture.root.path().join("active.chainstate.json")).unwrap(),
            state_before
        );
        let delivery = fixture.seller.state.delivery(&fixture.active_tc);
        assert_eq!(delivery.count.load(Ordering::Acquire), 2 * attempt);
        assert!(!delivery.done.load(Ordering::Acquire));
        fixture.request_a().await;
    }
    let shutdown = std::future::pending::<()>().fuse();
    tokio::pin!(shutdown);
    assert!(matches!(
        prepare_pool_deal_reporting_identity(
            &fixture.seller,
            &fixture.deal,
            &fixture.context(),
            false,
            shutdown.as_mut(),
            &mut false,
        )
        .await
        .unwrap(),
        PoolDealStart::Watching(None)
    ));
    assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 1);
    // Re-entering recovery adopts the matched deal rather than submitting a second SELL.
    prepare_pool_deal_reporting_identity(
        &fixture.seller,
        &fixture.deal,
        &fixture.context(),
        false,
        shutdown.as_mut(),
        &mut false,
    )
    .await
    .unwrap();
    assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        fixture.candidate.seller_stop_calls.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        fixture.candidate.buyer_stop_calls.load(Ordering::Relaxed),
        0
    );
    fixture.request_a().await;
}

#[tokio::test]
async fn issue_2147_vacant_candidate_shutdown_preserves_shared_gateway() {
    let fixture = Issue2147Fixture::new().await;
    fixture.request_a().await;
    let shutdown = std::future::ready(()).fuse();
    tokio::pin!(shutdown);
    let mut shutdown_requested = false;
    let error = prepare_pool_deal_reporting_identity(
        &fixture.seller,
        &fixture.deal,
        &fixture.context(),
        false,
        shutdown.as_mut(),
        &mut shutdown_requested,
    )
    .await
    .err()
    .expect("interrupted vacant candidate must not start");
    assert!(error.to_string().contains("shutdown"), "{error:#}");
    assert!(shutdown_requested);
    assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.attempts.load(Ordering::SeqCst), 0);
    fixture.request_a().await;
}

#[tokio::test]
async fn issue_2147_genuinely_stopped_gateway_still_fails_readiness_and_buyer_dial() {
    let mut fixture = Issue2147Fixture::new().await;
    fixture.request_a().await;
    fixture.seller.server_task.abort();
    let _ = (&mut fixture.seller.server_task).await;
    let shutdown = std::future::pending::<()>().fuse();
    tokio::pin!(shutdown);
    let error = prepare_pool_deal_reporting_identity(
        &fixture.seller,
        &fixture.deal,
        &fixture.context(),
        false,
        shutdown.as_mut(),
        &mut false,
    )
    .await
    .err()
    .expect("a genuinely dead listener still refuses admission");
    assert!(format!("{error:#}").contains("gateway"), "{error:#}");
    assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 0);
    let handover = fixture
        .buyer
        .resolve_endpoint(&fixture.active_chain, &fixture.active_tc)
        .await
        .unwrap();
    let error = fixture
        .buyer
        .connect_and_stream(&handover, &fixture.active_tc, "vendor--exact--v1", 8)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("E_GATEWAY_UNREACHABLE"),
        "{error:#}"
    );
}

#[tokio::test]
async fn issue_2147_explicit_pool_shutdown_still_stops_the_shared_gateway() {
    let mut fixture = Issue2147Fixture::new().await;
    fixture.request_a().await;
    let shutdown = std::future::ready(()).fuse();
    tokio::pin!(shutdown);
    let mut shutdown_requested = false;
    let mut provision = |_: String, _: String, _: u64, _: u64, _: u64| {
        futures::future::ready(Err(anyhow::anyhow!(
            "shutdown must not provision residuals"
        )))
    };
    let error = run_seller_pool(
        &fixture.seller,
        vec![fixture.deal.clone()],
        fixture.context(),
        &pool_test_policy(1),
        &mut provision,
        shutdown.as_mut(),
        &mut shutdown_requested,
    )
    .await
    .expect_err("shutdown during startup retains its existing interruption result");
    assert!(
        error
            .to_string()
            .contains("startup interrupted by shutdown"),
        "{error:#}"
    );
    assert!(shutdown_requested);
    let _ = (&mut fixture.seller.server_task).await;
    assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 0);
    let error = dexdo::buyer::tls::connect_pinned(
        &format!("https://{}", fixture.seller.listen_addr),
        &fixture.seller.tls_fingerprint,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("tcp_connect"), "{error:#}");
}

#[tokio::test]
async fn issue_2147_standalone_readiness_failure_still_stops_its_gateway() {
    let mut fixture = Issue2147Fixture::new().await;
    let (_reserved, advertise) = crate::test_refusing_endpoint::refusing_endpoint();
    fixture.deal.cfg.gateway_advertise = advertise;
    dexdo::seller::liveness::prepare_seller_offer_with_liveness(
        &fixture.seller,
        fixture.deal.chain.as_ref(),
        &fixture.deal.cfg,
        &fixture.owner,
        None,
        std::future::pending(),
        true,
        dexdo::seller::liveness::AdvertiseProbePolicy::Required,
    )
    .await
    .expect_err("standalone B must fail readiness");
    let _ = (&mut fixture.seller.server_task).await;
    assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 0);
    assert!(fixture.seller.server_task.is_finished());
}

#[tokio::test]
async fn issue_2264_candidate_is_admitted_during_reported_throttling() {
    let fixture = Issue2147Fixture::new().await;
    fixture.request_a().await;
    let timeout = SellerLivenessParams::canonical().health_check_timeout;
    assert_eq!(dexdo::telemetry::probe_upstream(&fixture.deal.upstream, timeout).await, "throttled");
    let before = std::fs::read(fixture.root.path().join("active.chainstate.json")).unwrap();
    let shutdown = std::future::pending::<()>().fuse();
    tokio::pin!(shutdown);
    assert!(matches!(
        prepare_pool_deal_reporting_identity(
            &fixture.seller, &fixture.deal, &fixture.context(), false, shutdown.as_mut(), &mut false,
        ).await.unwrap(),
        PoolDealStart::Watching(None)
    ));
    assert_eq!(fixture.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.candidate.post_calls.load(Ordering::Relaxed), 1);
    assert_eq!(std::fs::read(fixture.root.path().join("active.chainstate.json")).unwrap(), before);
    fixture.request_a().await;
    assert_eq!(dexdo::telemetry::probe_upstream(&fixture.deal.upstream, timeout).await, "throttled");
    assert!(!fixture.candidate.cancel_submitted.load(Ordering::Relaxed));
}

#[tokio::test]
async fn issue_2264_resting_deal_opens_while_another_sell_waits_for_its_ack() {
    for fresh_first in [true, false] {
        issue_2264_admission_order(fresh_first).await;
    }
}

async fn issue_2264_admission_order(fresh_first: bool) {
    let root = tempfile::tempdir().unwrap();
    let seller = dexdo::seller::start_gateway("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let gateway = seller.listen_addr.to_string();
    let fills = Arc::new(Mutex::new(Vec::new()));
    let a = Arc::new(PoolTestBackend::new(
        fills.clone(), format!("0:{}", "1".repeat(64)), 4, 4, false, i64::MAX - 2,
    ).resting_unexpired(true));
    let ack = Arc::new(tokio::sync::Semaphore::new(0));
    let mut b = PoolTestBackend::new(
        fills, format!("0:{}", "2".repeat(64)), 4, 4, false, i64::MAX - 1,
    );
    b.ambiguous_post_once.store(true, Ordering::Relaxed);
    b.post_ack = Some(ack.clone());
    let b = Arc::new(b);
    let deal_a = advertise_pool_deal(a.clone(), &gateway, root.path().join("a.json"));
    let mut deal_b = advertise_pool_deal(b.clone(), &gateway, root.path().join("b.json"));
    deal_b.nonce += 1;
    let context = SellerPoolContext {
        deals_dir: Some(root.path()),
        contracts: std::path::Path::new("manifest/deployed.manifest.json"),
        note_addr: &a.resting_owner,
        frame_model: "mock",
        gateway_advertise: &gateway,
        advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::Required,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let shutdown = async { stopped.await.unwrap() }.fuse();
    tokio::pin!(shutdown);
    let mut provision = |_: String, _: String, _: u64, _: u64, _: u64| {
        futures::future::ready(Err(anyhow::anyhow!("these full fills have no residual")))
    };
    let policy = pool_test_policy(2);
    let mut shutdown_requested = false;
    let candidates = if fresh_first { vec![deal_b, deal_a] } else { vec![deal_a, deal_b] };
    let pool = run_seller_pool(
        &seller, candidates, context, &policy, &mut provision,
        shutdown.as_mut(), &mut shutdown_requested,
    );
    let match_deal = |backend: &PoolTestBackend, order_id| {
        backend.owner_fills.lock().unwrap().push((
            backend.created_at, MatchedFill {
                order_id, token_contract: backend.token_contract.clone(),
                ticks: u128::from(backend.matched_ticks),
                price_per_tick: u128::from(backend.price_per_tick),
            },
        ));
        backend.rests.store(false, Ordering::Release);
        backend.matched.store(true, Ordering::Release);
    };
    let buyer = async {
        while b.post_calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        match_deal(&a, 1);
        while !a.opened.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert!(b.pending_sell.load(Ordering::Acquire));
        assert_eq!(ack.available_permits(), 0);
        assert_eq!(b.open_calls.load(Ordering::Acquire), 0);

        // The original B submission matched even though its acknowledgement was lost.
        match_deal(&b, 2);
        ack.add_permits(1);
        while !b.opened.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert_eq!(a.post_calls.load(Ordering::Acquire), 0);
        assert_eq!(b.post_calls.load(Ordering::Acquire), 1);
        stop.send(()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(pool, buyer)
    }).await.expect("A must open before B is released, then B must recover its own match");
    result.unwrap();
}
