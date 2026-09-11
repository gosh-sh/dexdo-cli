struct Issue1057Pool {
    root: tempfile::TempDir,
    seller: dexdo::seller::RunningSeller,
    note_addr: String,
    gateway: String,
    frame_model: &'static str,
    parent: Arc<PoolTestBackend>,
    successor: Arc<PoolTestBackend>,
    deal: SellerPoolDeal,
}

impl Issue1057Pool {
    fn context(&self) -> SellerPoolContext<'_> {
        SellerPoolContext {
            deals_dir: Some(self.root.path()),
            contracts: std::path::Path::new("manifest/deployed.manifest.json"),
            note_addr: &self.note_addr,
            frame_model: self.frame_model,
            gateway_advertise: &self.gateway,
            advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
            recover_publication: None,
        }
    }
}

async fn issue_1057_pool(matched_ticks: u64) -> Issue1057Pool {
    let root = tempfile::tempdir().expect(" seller pool directory");
    let note = Arc::new(LocalNote::generate());
    let note_addr = format!("0:{}", "a".repeat(64));
    let frame_model = "openai/gpt-oss-20b";
    let owner_fills = Arc::new(Mutex::new(Vec::new()));
    let parent = Arc::new(PoolTestBackend::new(
        owner_fills.clone(),
        format!("0:{}", "1".repeat(64)),
        8,
        matched_ticks,
        true,
        i64::MAX - 4,
    ));
    let successor_ticks = 8 - matched_ticks;
    let successor = Arc::new(PoolTestBackend::new(
        owner_fills,
        format!("0:{}", "2".repeat(64)),
        successor_ticks.max(2),
        successor_ticks.max(2),
        false,
        i64::MAX - 3,
    ));
    let seller = dexdo::seller::start_gateway_with_note(
        "127.0.0.1:0".parse().unwrap(),
        dexdo::seller::UpstreamConfig::Mock,
        note,
    )
    .await
    .unwrap();
    let gateway = seller.listen_addr.to_string();
    let cfg = SellerConfig {
        token_contract: parent.token_contract.clone(),
        price_per_tick: parent.price_per_tick,
        max_ticks: parent.offered_ticks,
        subscription: false,
        gateway_advertise: gateway.clone(),
        mock_token_count: 8,
    };
    let watch = dexdo::seller::SellerMatchWatchConfig {
        cursor_path: root.path().join("parent.cursor.json"),
        poll_interval: std::time::Duration::from_millis(1),
    };

    // Persist the authoritative fill through the same match entry point production uses. Reset only
    // the scripted open bit afterwards so the pool run has an observable match-to-active boundary.
    dexdo::seller::poll_match_and_maybe_open(&seller, parent.as_ref(), &cfg, &watch.cursor_path)
        .await
        .unwrap()
        .expect("the scripted partial match");
    parent.opened.store(false, Ordering::Relaxed);
    parent.open_calls.store(0, Ordering::Relaxed);
    seller.state.unregister_stream(&parent.token_contract);

    Issue1057Pool {
        root,
        seller,
        note_addr,
        gateway,
        frame_model,
        parent: parent.clone(),
        successor,
        deal: SellerPoolDeal {
            chain: parent,
            cfg,
            watch,
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: 10,
            market: None,
        },
    }
}

fn issue_1057_market(
    model: String,
    nonce: u64,
    price: u64,
    ticks: u64,
    note_addr: &str,
    successor: Arc<PoolTestBackend>,
) -> (dexdo_core::MarketManifest, Arc<dyn ChainBackend>) {
    let market = dexdo_core::MarketManifest {
        network: "net-a".to_string(),
        model_hash: dexdo_core::model_hash_for(&model),
        frame_model: model,
        inference_order_book: format!("0:{}", "d".repeat(64)),
        root_model: format!("0:{}", "e".repeat(64)),
        token_contract: successor.token_contract.clone(),
        seller_note: note_addr.to_string(),
        nonce,
        price_per_tick: u128::from(price),
        max_ticks: u128::from(ticks),
    };
    let chain: Arc<dyn ChainBackend> = successor;
    (market, chain)
}

enum Issue1057ShutdownTrigger {
    ParentOpened(Arc<PoolTestBackend>),
    ActiveStarted(Arc<Issue1057ActiveBackend>),
}

struct Issue1057Shutdown {
    trigger: Issue1057ShutdownTrigger,
    pending_polls_after_trigger: usize,
    polls_after_trigger: usize,
    terminated: bool,
}

impl Issue1057Shutdown {
    fn after_parent_open(parent: Arc<PoolTestBackend>, pending_polls: usize) -> Self {
        Self {
            trigger: Issue1057ShutdownTrigger::ParentOpened(parent),
            pending_polls_after_trigger: pending_polls,
            polls_after_trigger: 0,
            terminated: false,
        }
    }

    fn after_active_started(active: Arc<Issue1057ActiveBackend>) -> Self {
        Self {
            trigger: Issue1057ShutdownTrigger::ActiveStarted(active),
            pending_polls_after_trigger: 0,
            polls_after_trigger: 0,
            terminated: false,
        }
    }

    fn triggered(&self) -> bool {
        match &self.trigger {
            Issue1057ShutdownTrigger::ParentOpened(parent) => {
                parent.open_calls.load(Ordering::Relaxed) > 0
            }
            Issue1057ShutdownTrigger::ActiveStarted(active) => {
                active.started.load(Ordering::Relaxed) > 0
            }
        }
    }
}

impl std::future::Future for Issue1057Shutdown {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        if this.terminated || !this.triggered() {
            return std::task::Poll::Pending;
        }
        this.polls_after_trigger += 1;
        if this.polls_after_trigger <= this.pending_polls_after_trigger {
            return std::task::Poll::Pending;
        }
        this.terminated = true;
        std::task::Poll::Ready(())
    }
}

impl futures::future::FusedFuture for Issue1057Shutdown {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

struct Issue1057ActiveBackend {
    inner: Arc<PoolTestBackend>,
    started: AtomicU64,
}

#[async_trait::async_trait]
impl ChainBackend for Issue1057ActiveBackend {
    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
        self.inner.discover_offers().await
    }

    async fn post_offer(&self, offer: SellOffer, note: &dyn Note) -> Result<(), ChainError> {
        self.inner.post_offer(offer, note).await
    }

    async fn confirm_offer_outcome(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<SellOfferOutcome>, ChainError> {
        self.inner.confirm_offer_outcome(token_contract).await
    }

    async fn sell_offer_terms(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(u64, u64)>, ChainError> {
        self.inner.sell_offer_terms(token_contract).await
    }

    async fn read_openable_match_now(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        self.inner.read_openable_match_now(token_contract).await
    }

    async fn poll_seller_fills(
        &self,
        note: &dyn Note,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>, ChainError> {
        self.inner.poll_seller_fills(note, cursor).await
    }

    async fn place_buy(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<(), ChainError> {
        self.inner.place_buy(token_contract, note).await
    }

    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        self.inner.read_match(token_contract).await
    }

    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        enc_endpoint: Vec<u8>,
        note: &dyn Note,
    ) -> Result<(), ChainError> {
        self.inner
            .open_stream(token_contract, enc_endpoint, note)
            .await
    }

    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        self.inner.read_handover(token_contract).await
    }

    async fn claim_tokens(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
        cumulative_tokens: u128,
    ) -> Result<(), ChainError> {
        self.inner
            .claim_tokens(token_contract, note, cumulative_tokens)
            .await
    }

    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        self.inner.accept_probe(token_contract).await
    }

    async fn stop(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        self.inner.stop(token_contract, note).await
    }

    async fn deal_state(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        self.inner.deal_state(token_contract).await
    }

    async fn deal_snapshot(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainSnapshot>, ChainError> {
        self.inner.deal_snapshot(token_contract).await
    }

    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot> {
        self.inner.snapshot(token_contract).await
    }

    async fn probe_burned_settlement(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(u128, u128, u128)>, ChainError> {
        self.inner.probe_burned_settlement(token_contract).await
    }

    async fn deal_claim_bounds(
        &self,
        token_contract: &TokenContract,
    ) -> Result<dexdo_core::ClaimBounds, ChainError> {
        self.started.fetch_add(1, Ordering::Relaxed);
        self.inner.deal_claim_bounds(token_contract).await
    }
}

/// money assertion. The parent match has been opened, its residual has reached the pool's
/// `pending` channel, and this future deliberately remains pending for the poll that admits that
/// channel item. The next observation is therefore the spending boundary under test: current `dev`
/// calls the provisioner first; the fix must take the shutdown first and exit normally.
#[tokio::test]
async fn issue_1057_shutdown_with_pending_residual_spends_zero_provision_calls() {
    let pool = issue_1057_pool(2).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut provision = {
        let calls = calls.clone();
        let successor = pool.successor.clone();
        let note_addr = pool.note_addr.clone();
        move |model: String, nonce: u64, price: u64, ticks: u64| {
            calls.lock().unwrap().push((nonce, price, ticks));
            futures::future::ready(Ok(issue_1057_market(
                model,
                nonce,
                price,
                ticks,
                &note_addr,
                successor.clone(),
            )))
        }
    };
    let shutdown = Issue1057Shutdown::after_parent_open(pool.parent.clone(), 1);
    tokio::pin!(shutdown);
    let mut shutdown_requested = false;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_seller_pool(
            &pool.seller,
            vec![pool.deal.clone()],
            pool.context(),
            &pool_test_policy(2),
            &mut provision,
            shutdown.as_mut(),
            &mut shutdown_requested,
        ),
    )
    .await
    .expect(" pool must exit through its own shutdown path");

    assert!(
        calls.lock().unwrap().is_empty(),
        "a shutdown with a queued residual must make zero spending provisioner calls: {:?}",
        calls.lock().unwrap()
    );
    outcome.expect("the pre-provision guard must use the normal operator-shutdown exit");
    assert!(shutdown_requested, "the consumed shutdown must be recorded");
}

#[tokio::test]
async fn issue_1057_no_shutdown_before_residual_preserves_one_provision() {
    let pool = issue_1057_pool(2).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut provision = {
        let calls = calls.clone();
        let successor = pool.successor.clone();
        let note_addr = pool.note_addr.clone();
        move |model: String, nonce: u64, price: u64, ticks: u64| {
            calls.lock().unwrap().push((nonce, price, ticks));
            futures::future::ready(Ok(issue_1057_market(
                model,
                nonce,
                price,
                ticks,
                &note_addr,
                successor.clone(),
            )))
        }
    };
    let successor_for_shutdown = pool.successor.clone();
    let shutdown = async move {
        while successor_for_shutdown.post_calls.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    }
    .fuse();
    tokio::pin!(shutdown);
    let mut shutdown_requested = false;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_seller_pool(
            &pool.seller,
            vec![pool.deal.clone()],
            pool.context(),
            &pool_test_policy(2),
            &mut provision,
            shutdown.as_mut(),
            &mut shutdown_requested,
        ),
    )
    .await
    .expect("ordinary residual provision must complete before the test shutdown");

    outcome.expect("ordinary residual provision must retain the existing normal path");
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(11, pool.parent.price_per_tick, 6)],
        "without an earlier shutdown the exact residual is still provisioned once"
    );
    assert_eq!(
        pool.successor.post_calls.load(Ordering::Relaxed),
        1,
        "the ordinary path must still post the provisioned successor"
    );
}

#[tokio::test]
async fn issue_1057_shutdown_after_active_start_keeps_existing_active_path() {
    let mut pool = issue_1057_pool(8).await;
    let active = Arc::new(Issue1057ActiveBackend {
        inner: pool.parent.clone(),
        started: AtomicU64::new(0),
    });
    pool.deal.chain = active.clone();
    let calls = Arc::new(AtomicU64::new(0));
    let mut provision = {
        let calls = calls.clone();
        let successor = pool.successor.clone();
        let note_addr = pool.note_addr.clone();
        move |model: String, nonce: u64, price: u64, ticks: u64| {
            calls.fetch_add(1, Ordering::Relaxed);
            futures::future::ready(Ok(issue_1057_market(
                model,
                nonce,
                price,
                ticks,
                &note_addr,
                successor.clone(),
            )))
        }
    };
    let shutdown = Issue1057Shutdown::after_active_started(active.clone());
    tokio::pin!(shutdown);
    let mut shutdown_requested = false;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_seller_pool(
            &pool.seller,
            vec![pool.deal.clone()],
            pool.context(),
            &pool_test_policy(1),
            &mut provision,
            shutdown.as_mut(),
            &mut shutdown_requested,
        ),
    )
    .await
    .expect("active-deal shutdown must retain the existing bounded exit");

    outcome.expect("active-deal shutdown must retain the normal operator-shutdown path");
    assert_eq!(
        active.started.load(Ordering::Relaxed),
        1,
        "the matched deal must reach the existing active settlement-driver path before shutdown"
    );
    assert_eq!(
        pool.parent.open_calls.load(Ordering::Relaxed),
        1,
        "the already-active deal's existing match/open handling must remain unchanged"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "a fully matched active deal has no residual to provision"
    );
}

struct Issue1057YieldingRetiredBackend {
    inner: Arc<PoolTestBackend>,
}

#[async_trait::async_trait]
impl ChainBackend for Issue1057YieldingRetiredBackend {
    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
        self.inner.discover_offers().await
    }

    async fn post_offer(&self, offer: SellOffer, note: &dyn Note) -> Result<(), ChainError> {
        self.inner.post_offer(offer, note).await
    }

    async fn confirm_offer_outcome(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<SellOfferOutcome>, ChainError> {
        self.inner.confirm_offer_outcome(token_contract).await
    }

    async fn sell_offer_terms(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(u64, u64)>, ChainError> {
        self.inner.sell_offer_terms(token_contract).await
    }

    async fn read_openable_match_now(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        self.inner.read_openable_match_now(token_contract).await
    }

    async fn poll_seller_fills(
        &self,
        note: &dyn Note,
        cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>, ChainError> {
        self.inner.poll_seller_fills(note, cursor).await
    }

    async fn place_buy(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<(), ChainError> {
        self.inner.place_buy(token_contract, note).await
    }

    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        self.inner.read_match(token_contract).await
    }

    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        enc_endpoint: Vec<u8>,
        note: &dyn Note,
    ) -> Result<(), ChainError> {
        // Let the fill notification win the pool's biased select before this watcher releases its
        // capacity. That gives the production `pending` queue both residuals before A is provisioned.
        tokio::task::yield_now().await;
        self.inner
            .open_stream(token_contract, enc_endpoint, note)
            .await?;
        self.inner.exists.store(false, Ordering::Relaxed);
        Ok(())
    }

    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        self.inner.read_handover(token_contract).await
    }

    async fn claim_tokens(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
        cumulative_tokens: u128,
    ) -> Result<(), ChainError> {
        self.inner
            .claim_tokens(token_contract, note, cumulative_tokens)
            .await
    }

    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        self.inner.accept_probe(token_contract).await
    }

    async fn stop(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        self.inner.stop(token_contract, note).await
    }

    async fn deal_state(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        self.inner.deal_state(token_contract).await
    }

    async fn deal_snapshot(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainSnapshot>, ChainError> {
        self.inner.deal_snapshot(token_contract).await
    }

    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot> {
        self.inner.snapshot(token_contract).await
    }

    async fn probe_burned_settlement(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<(u128, u128, u128)>, ChainError> {
        self.inner.probe_burned_settlement(token_contract).await
    }

    async fn deal_claim_bounds(
        &self,
        token_contract: &TokenContract,
    ) -> Result<dexdo_core::ClaimBounds, ChainError> {
        self.inner.deal_claim_bounds(token_contract).await
    }
}

#[tokio::test]
async fn issue_1057_consumed_shutdown_between_two_residuals_provisions_once() {
    let pool = issue_1057_pool(2).await;
    let second_parent = Arc::new(PoolTestBackend::new(
        pool.parent.owner_fills.clone(),
        format!("0:{}", "3".repeat(64)),
        8,
        2,
        true,
        i64::MAX - 2,
    ));
    let second_cfg = SellerConfig {
        token_contract: second_parent.token_contract.clone(),
        price_per_tick: second_parent.price_per_tick,
        max_ticks: second_parent.offered_ticks,
        subscription: false,
        gateway_advertise: pool.gateway.clone(),
        mock_token_count: 8,
    };
    let second_watch = dexdo::seller::SellerMatchWatchConfig {
        cursor_path: pool.root.path().join("second-parent.cursor.json"),
        poll_interval: std::time::Duration::from_millis(1),
    };
    dexdo::seller::poll_match_and_maybe_open(
        &pool.seller,
        second_parent.as_ref(),
        &second_cfg,
        &second_watch.cursor_path,
    )
    .await
    .unwrap()
    .expect("the second scripted partial match");
    second_parent.opened.store(false, Ordering::Relaxed);
    second_parent.open_calls.store(0, Ordering::Relaxed);
    pool.seller
        .state
        .unregister_stream(&second_parent.token_contract);
    let second_deal = SellerPoolDeal {
        chain: Arc::new(Issue1057YieldingRetiredBackend {
            inner: second_parent,
        }),
        cfg: second_cfg,
        watch: second_watch,
        upstream: dexdo::seller::UpstreamConfig::Mock,
        nonce: 20,
        market: None,
    };

    // Two watched parents fill the two-deal capacity while both fill notifications enter `pending`.
    // Once one active driver finishes, residual A gets the single free slot. Its provision call
    // makes shutdown ready; `prepare_pool_deal` must consume and record it before residual B.
    let provision_calls = Arc::new(AtomicU64::new(0));
    let mut provision = {
        let provision_calls = provision_calls.clone();
        let owner_fills = pool.parent.owner_fills.clone();
        let note_addr = pool.note_addr.clone();
        move |model: String, nonce: u64, price: u64, ticks: u64| {
            provision_calls.fetch_add(1, Ordering::Relaxed);
            let successor = Arc::new(PoolTestBackend::new(
                owner_fills.clone(),
                format!("0:{nonce:064x}"),
                ticks.max(2),
                ticks.max(2),
                false,
                i64::MAX - 1,
            ));
            futures::future::ready(Ok(issue_1057_market(
                model, nonce, price, ticks, &note_addr, successor,
            )))
        }
    };
    let provision_calls_for_shutdown = provision_calls.clone();
    let shutdown = async move {
        while provision_calls_for_shutdown.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    }
    .fuse();
    tokio::pin!(shutdown);
    let mut shutdown_requested = false;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_seller_pool(
            &pool.seller,
            vec![pool.deal.clone(), second_deal],
            pool.context(),
            &pool_test_policy(2),
            &mut provision,
            shutdown.as_mut(),
            &mut shutdown_requested,
        ),
    )
    .await
    .expect("the consumed-shutdown regression must reach the bounded pool exit");

    assert_eq!(
        provision_calls.load(Ordering::Relaxed),
        1,
        "a stop consumed while provisioning residual A must leave queued residual B unprovisioned"
    );
    assert!(
        futures::future::FusedFuture::is_terminated(shutdown.as_ref().get_ref()),
        "prepare_pool_deal must consume the fused shutdown"
    );
    assert!(
        shutdown_requested,
        "prepare_pool_deal must preserve the consumed shutdown in the record"
    );
    let error = outcome.expect_err("the interrupted residual startup remains the pool error");
    assert!(
        error
            .to_string()
            .contains("seller pool startup interrupted by shutdown"),
        "unexpected interrupted residual error: {error:#}"
    );
}
