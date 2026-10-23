//! Narrow live wire regression: one ask, two buyers, then the same paid deal's gateway.
use super::super::buyer_deal_init_error;
use dexdo::buyer::{api::*, Buyer};
use dexdo::seller::{self, AdvanceWindows, SellerConfig, UpstreamConfig};
use dexdo_core::{
    address::parse_chain_address, default_buy_deadline, params::*, required_escrow_for_buy,
    Address, ChainBackend, KeyPair, Note, RealBuyerBackend, RealChainBackend, RealSellerBackend,
    TokenContractSettlementEvent,
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn address(value: &str) -> Address {
    parse_chain_address(value).unwrap().into_chain()
}

async fn wait_state(
    chain: &dyn ChainBackend,
    tc: &String,
    ready: impl Fn(dexdo_core::DealChainState) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(BUYER_ON_DEMAND_PURCHASE_SECS), async {
        loop {
            if chain.deal_state(tc).await.unwrap().is_some_and(&ready) {
                return;
            }
            tokio::time::sleep(DEFAULT_MATCH_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("live deal reached its required state");
}

// Hold only the long response after production authorization/reservation. This is a wire barrier,
// not a replacement for the gateway's accounting or concurrency guard.
struct HeldGateway {
    production: seller::gateway::GatewayService,
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
}

#[tonic::async_trait]
impl dexdo_proto::Gateway for HeldGateway {
    async fn get_challenge(
        &self,
        request: tonic::Request<dexdo_proto::ChallengeRequest>,
    ) -> Result<tonic::Response<dexdo_proto::Challenge>, tonic::Status> {
        self.production.get_challenge(request).await
    }
    type OpenStreamStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<dexdo_proto::CanonChunk, tonic::Status>> + Send>,
    >;
    async fn open_stream(
        &self,
        request: tonic::Request<dexdo_proto::StreamRequest>,
    ) -> Result<tonic::Response<Self::OpenStreamStream>, tonic::Status> {
        use tokio_stream::StreamExt;
        let hold = request
            .get_ref()
            .request
            .as_ref()
            .and_then(|r| r.params.as_ref())
            .is_some_and(|p| p.max_tokens > 1);
        let mut stream = self.production.open_stream(request).await?.into_inner();
        if hold {
            self.entered.add_permits(1);
        }
        let release = self.release.clone();
        Ok(tonic::Response::new(Box::pin(async_stream::stream! {
            if hold {release.acquire().await.unwrap().forget();}
            while let Some(chunk)=stream.next().await {yield chunk;}
        })))
    }
}

async fn held_gateway(
    note: Arc<dyn Note>,
) -> (
    seller::RunningSeller,
    Arc<tokio::sync::Semaphore>,
    Arc<tokio::sync::Semaphore>,
) {
    use tonic::transport::{Identity, Server, ServerTlsConfig};
    let state = Arc::new(seller::gateway::GatewayState::with_upstream(
        UpstreamConfig::Mock,
    ));
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let service = HeldGateway {
        production: seller::gateway::GatewayService::new(state.clone()),
        entered: entered.clone(),
        release: release.clone(),
    };
    seller::tls::ensure_crypto_provider();
    let tls = seller::tls::GatewayTls::generate().unwrap();
    let tls_fingerprint = tls.fingerprint;
    let identity = Identity::from_pem(tls.cert_pem, tls.key_pem);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen_addr = listener.local_addr().unwrap();
    let mut builder = Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity))
        .unwrap();
    let server_task = tokio::spawn(async move {
        builder
            .add_service(dexdo_proto::GatewayServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (
        seller::RunningSeller {
            state,
            note,
            server_task,
            listen_addr,
            tls_fingerprint,
        },
        entered,
        release,
    )
}

async fn http(state: ApiState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state, true)).await.unwrap();
    });
    (url, task)
}

fn state(buyer: Arc<Buyer>, frame: &str, deals: RouteManager) -> ApiState {
    ApiState {
        buyer,
        frame_model: frame.into(),
        mock_model: true,
        renewal_enabled: false,
        deals: Arc::new(deals),
        delivery_events: None,
        usage: None,
    }
}

fn active(
    buyer: Arc<Buyer>,
    frame: &str,
    chain: Arc<dyn ChainBackend>,
    tc: &str,
    handover: dexdo_core::Handover,
    tokens: u64,
) -> (ApiState, Arc<SessionSettle>) {
    let session = Arc::new(SessionSettle::new_with_failure_policy_and_lifetime(
        chain,
        tc.into(),
        buyer.note.clone(),
        BuyerApiFailurePolicy::default(),
        SessionLifetimePolicy::Preserve,
    ));
    let deal = ApiDeal::new(
        Route {
            handover,
            token_contract: tc.into(),
            max_tokens: tokens,
        },
        session.clone(),
        Arc::new(ContentGate::skip()),
    );
    (state(buyer, frame, RouteManager::new(deal)), session)
}

async fn request(
    client: &reqwest::Client,
    url: &str,
    frame: &str,
    path: &str,
    tokens: u64,
    stream: bool,
) -> reqwest::Response {
    let body = if path == "/v1/responses" {
        serde_json::json!({"model":frame,"input":"x","max_output_tokens":tokens,"stream":stream})
    } else {
        serde_json::json!({"model":frame,"messages":[{"role":"user","content":"x"}],"max_tokens":tokens,"stream":stream})
    };
    client
        .post(format!("{url}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn refusal(response: reqwest::Response, status: u16, code: &str, action: &str) {
    let observed = response.status().as_u16();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(observed, status, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert_eq!(body["error"]["recovery_action"], action, "{body}");
    eprintln!("1450_WIRE status={status} code={code} recovery_action={action}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live : three dedicated notes; one funded deal, mock tokens, clean settlement"]
async fn live_consumer_errors_1450() {
    let manifest =
        PathBuf::from(std::env::var("DEXDO_MANIFEST").expect("explicit development manifest"));
    assert_eq!(
        dexdo_core::Deployed::load(&manifest).unwrap().network,
        "shellnet"
    );
    let pool_path =
        PathBuf::from(std::env::var("DEXDO_PN_POOL").expect("three dedicated fresh notes"));
    let pool: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pool_path).unwrap()).unwrap();
    assert_eq!(
        pool["token_type"].as_u64(),
        Some(u64::from(SHELL_CURRENCY_ID))
    );
    let notes = pool["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 3, "this narrow run owns exactly three notes");
    let identity = |i: usize| {
        (
            notes[i]["address"].as_str().unwrap().to_owned(),
            notes[i]["owner_secret_key_hex"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    };
    let (seller_addr, seller_secret) = identity(0);
    let (winner_addr, winner_secret) = identity(1);
    let (loser_addr, loser_secret) = identity(2);
    let manifest_arg = manifest.to_str().unwrap();
    let low = RealChainBackend::connect(&manifest).unwrap();
    // Resume only a funded unopened fixture; never repeat its paid BUY after an interrupted oracle.
    let previous: Option<serde_json::Value> = std::env::var("DEXDO_1450_RESUME").ok().map(|p| {
        let text = std::fs::read_to_string(p).unwrap();
        serde_json::from_str(text.lines().next().unwrap()).unwrap()
    });
    let nonce = previous
        .as_ref()
        .map(|p| p["nonce"].as_u64().unwrap())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        });
    let frame = previous
        .as_ref()
        .map(|p| p["frame"].as_str().unwrap().to_owned())
        .unwrap_or_else(|| format!("mock-issue1450-{nonce}"));
    let ticks = 4;
    let price = PRICE_STEP;
    let escrow = required_escrow_for_buy(ticks, price);
    let seller_keys = KeyPair::from_secret_hex(&seller_secret).unwrap();
    // Use the receipt-backed 4.0.36 reserve for these exact terms, with no operator surplus.
    let tc = if let Some(previous) = &previous {
        previous["token_contract"].as_str().unwrap().to_owned()
    } else {
        let market = low
            .provision_market(
                &seller_keys,
                &address(&seller_addr),
                &frame,
                nonce,
                price,
                ticks,
                deal_gas_requirement_raw(ticks),
            )
            .await
            .unwrap();
        market.token_contract
    };
    let tc_addr = address(&tc);
    // Persist the exact identity before the first BUY so an interrupted run can reconcile it.
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let evidence_path = pool_path.with_file_name(format!("issue1450-{nonce}.jsonl"));
    let mut evidence = std::fs::OpenOptions::new()
        .create_new(previous.is_none())
        .write(true)
        .append(previous.is_some())
        .mode(0o600)
        .open(&evidence_path)
        .unwrap();
    writeln!(
        evidence,
        "{}",
        serde_json::json!({"frame":frame,"nonce":nonce,"token_contract":tc})
    )
    .unwrap();
    evidence.sync_all().unwrap();
    if previous.is_none() {
        low.post_sell_offer(
            &address(&seller_addr),
            &seller_keys,
            0,
            nonce,
            MAX_SELL_TTL.as_secs(),
        )
        .await
        .unwrap();
    }
    let (winner, winner_note) = RealBuyerBackend::from_provisioned(
        manifest_arg,
        &winner_addr,
        &winner_secret,
        &frame,
        price,
        ticks,
        escrow,
    )
    .unwrap();
    let (loser, loser_note) = RealBuyerBackend::from_provisioned(
        manifest_arg,
        &loser_addr,
        &loser_secret,
        &frame,
        price,
        ticks,
        escrow,
    )
    .unwrap();
    let winner = Arc::new(winner);
    let loser = Arc::new(loser);
    if previous.is_none() {
        // Freeze the same executable target for both buyers, then let one consume it.
        tokio::time::timeout(Duration::from_secs(DEAL_WAIT_SECS), async {
            loop {
                if loser
                    .submit_safe_explicit_buy_quote_order(&tc, ticks, price)
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(DEFAULT_MATCH_POLL_INTERVAL).await;
            }
        })
        .await
        .unwrap();
        winner
            .place_buy_by_model(
                &winner_note,
                ticks,
                price,
                escrow,
                0,
                default_buy_deadline(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                )
                .expect("bounded live BUY lifetime"),
            )
            .await
            .unwrap();
    }
    if previous.is_some() {
        let observed = winner.deal_state(&tc).await.unwrap().unwrap();
        assert!(
            observed.funded && !observed.opened && !observed.is_stopped(),
            "resume accepts only a funded unopened fixture"
        );
    }
    wait_state(winner.as_ref(), &tc, |s| s.funded && !s.opened).await;
    let loser_buys = low
        .successful_place_inference_buy_receipts(&address(&loser_addr))
        .await
        .unwrap()
        .len();
    let loser = loser.clone();
    let loser_note: Arc<dyn Note> = Arc::new(loser_note);
    let init_tc = tc.clone();
    let init_note = loser_note.clone();
    let initializer: DealInitializer = Arc::new(move || {
        let (chain, note, tc) = (loser.clone(), init_note.clone(), init_tc.clone());
        Box::pin(async move {
            let error = chain
                .place_buy(&tc, note.as_ref())
                .await
                .expect_err("the other buyer consumed this exact ask");
            assert!(
                matches!(error, dexdo_core::ChainError::OrderUnavailable(_)),
                "{error}"
            );
            Err(buyer_deal_init_error(anyhow::Error::new(error)))
        })
    });
    let client = reqwest::Client::new();
    let (url, task) = http(state(
        Arc::new(Buyer::from_note(loser_note)),
        &frame,
        RouteManager::lazy(initializer, Duration::from_secs(DEAL_WAIT_SECS)),
    ))
    .await;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        refusal(
            request(&client, &url, &frame, path, 1, false).await,
            503,
            "order_unavailable",
            "retry",
        )
        .await;
    }
    task.abort();
    let _ = task.await;
    assert_eq!(
        low.successful_place_inference_buy_receipts(&address(&loser_addr))
            .await
            .unwrap()
            .len(),
        loser_buys,
        "losing retries submit no money"
    );
    let buyer = Arc::new(Buyer::from_note(Arc::new(winner_note)));
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let (api, _session) = active(
            buyer.clone(),
            &frame,
            winner.clone(),
            &tc,
            dexdo_core::Handover {
                endpoint: "https://127.0.0.1:1".into(),
                tls_fingerprint: String::new(),
            },
            (ticks * TICK_SIZE) as u64,
        );
        let (url, task) = http(api).await;
        refusal(
            request(&client, &url, &frame, path, 1, false).await,
            502,
            "deal_not_opened",
            "read_only_reconciliation",
        )
        .await;
        task.abort();
        let _ = task.await;
    }
    wait_state(winner.as_ref(), &tc, |s| {
        s.funded && !s.opened && !s.probe_accepted
    })
    .await;
    // Open that same deal only after observing the never-opened HTTP refusal.
    let (seller_chain, seller_note) = RealSellerBackend::from_provisioned(
        manifest_arg,
        &seller_addr,
        &seller_secret,
        &frame,
        nonce,
    )
    .unwrap();
    let seller_chain = Arc::new(seller_chain);
    let (gateway, entered, release) = held_gateway(Arc::new(seller_note)).await;
    let long_tokens =
        (2 * (GATEWAY_CLIENT_CHANNEL_CAPACITY + GATEWAY_UPSTREAM_CHANNEL_CAPACITY) + 1) as u64;
    gateway
        .state
        .set_max_concurrent_requests_per_deal(std::num::NonZeroU32::new(1).unwrap());
    seller::serve_match(
        &gateway,
        seller_chain.as_ref(),
        &SellerConfig {
            token_contract: tc.clone(),
            price_per_tick: price as u64,
            max_ticks: ticks as u64,
            subscription: false,
            gateway_advertise: gateway.listen_addr.to_string(),
            mock_token_count: long_tokens,
        },
    )
    .await
    .unwrap();
    let handover = buyer.resolve_endpoint(winner.as_ref(), &tc).await.unwrap();
    let (api, session) = active(
        buyer.clone(),
        &frame,
        winner.clone(),
        &tc,
        handover,
        (ticks * TICK_SIZE) as u64,
    );
    let (url, api_task) = http(api).await;
    let probe = request(&client, &url, &frame, "/v1/chat/completions", 1, false).await;
    assert_eq!(probe.status(), 200);
    let _ = probe.bytes().await.unwrap();
    let delivery = gateway.state.delivery(&tc);
    let (chain, claim_tc, note) = (seller_chain.clone(), tc.clone(), gateway.note.clone());
    let windows = AdvanceWindows::from_bounds(chain.deal_claim_bounds(&tc).await.unwrap());
    let (count, done) = (delivery.count.clone(), delivery.done.clone());
    let driver = tokio::spawn(async move {
        seller::drive_advance(
            chain.as_ref(),
            &claim_tc,
            note.as_ref(),
            windows,
            ticks,
            TICK_SIZE as u64,
            false,
            count,
            done,
        )
        .await
    });
    wait_state(winner.as_ref(), &tc, |s| s.probe_accepted).await;
    let (a_client, a_url, a_frame) = (client.clone(), url.clone(), frame.clone());
    let pending = tokio::spawn(async move {
        request(
            &a_client,
            &a_url,
            &a_frame,
            "/v1/chat/completions",
            long_tokens,
            true,
        )
        .await
    });
    tokio::time::timeout(SELLER_TERMINAL_RECEIPT_TIMEOUT, entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    refusal(
        request(&client, &url, &frame, "/v1/chat/completions", 1, false).await,
        429,
        "deal_busy",
        "retry_with_backoff",
    )
    .await;
    let before = winner.deal_state(&tc).await.unwrap().unwrap();
    assert!(before.opened && before.probe_accepted && !before.is_stopped());
    release.add_permits(1);
    let mut held = pending.await.unwrap();
    assert_eq!(held.status(), reqwest::StatusCode::OK);
    while held.chunk().await.unwrap().is_some() {}
    let prior = before.tokens_final.max(before.tokens_pending);
    wait_state(winner.as_ref(), &tc, |s| {
        s.tokens_final.max(s.tokens_pending) > prior
    })
    .await;

    tokio::time::timeout(SELLER_TERMINAL_RECEIPT_TIMEOUT, async {
        loop {
            let retry = request(&client, &url, &frame, "/v1/chat/completions", 1, false).await;
            if retry.status() == reqwest::StatusCode::OK {
                let _ = retry.bytes().await.unwrap();
                break;
            }
            let status = retry.status();
            let body: serde_json::Value = retry.json().await.unwrap();
            assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS, "{body}");
            assert!(
                matches!(
                    body["error"]["code"].as_str(),
                    Some("deal_busy" | "deal_capacity_unavailable")
                ),
                "{body}"
            );
            tokio::time::sleep(DEFAULT_MATCH_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("readmission must follow bounded backoff after the occupied slot drains");
    eprintln!("1450_WIRE busy slot released; retry status=200");
    delivery
        .done
        .store(true, std::sync::atomic::Ordering::Release);
    tokio::time::timeout(Duration::from_secs(BUYER_ON_DEMAND_PURCHASE_SECS), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let settlement = winner.stop(&tc, buyer.note.as_ref()).await.unwrap();
    assert!(
        matches!(settlement, dexdo_core::Settlement::AuthoritativeReceipt(_)),
        "{settlement:?}"
    );
    let receipts = tokio::time::timeout(SELLER_TERMINAL_RECEIPT_TIMEOUT, async {
        loop {
            let receipts = low
                .token_contract_settlement_receipts(&tc_addr)
                .await
                .unwrap();
            if receipts
                .events
                .iter()
                .any(|r| matches!(r.event, TokenContractSettlementEvent::StreamStopped { .. }))
            {
                break receipts;
            }
            tokio::time::sleep(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("the terminal receipt must be indexed");
    let claims = receipts
        .events
        .iter()
        .filter(|r| matches!(r.event, TokenContractSettlementEvent::TicksClaimed { .. }))
        .count();
    assert!(
        claims >= 2,
        "clean live acceptance requires two advance claims, got {claims}"
    );
    assert!(
        receipts
            .events
            .iter()
            .any(|r| matches!(r.event, TokenContractSettlementEvent::ProbeAccepted { .. })),
        "paid probe acceptance must have a receipt"
    );
    assert!(
        !receipts
            .events
            .iter()
            .any(|r| matches!(r.event, TokenContractSettlementEvent::ProbeBurned { .. })),
        "probe must never burn"
    );
    writeln!(evidence,"{}",serde_json::json!({"result":"passed","claims":claims,"settlement":"AmicableSplit","receipts":receipts.events.iter().map(|r| serde_json::json!({"message_id":r.message_id,"created_at":r.created_at,"event":format!("{:?}",r.event)})).collect::<Vec<_>>()})).unwrap();
    evidence.sync_all().unwrap();
    eprintln!("1450_LIVE passed: claims={claims}; AmicableSplit; no ProbeBurned");
    // Settlement is explicit above; these embedded servers have no process-level shutdown policy.
    drop(session);
    api_task.abort();
    gateway.server_task.abort();
    let _ = api_task.await;
    let _ = gateway.server_task.await;
}
