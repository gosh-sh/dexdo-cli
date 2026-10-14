use super::*;
use crate::cli::buyer as command;
use dexdo_core::{ChainBackend, ChainError, Note, TokenContract};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

// The real HTTP adapter and production initializer run against an in-memory chain.
// The POST seam reports an abundant balance, so a missing guard really posts twice.
struct WaitingChain {
    pipeline: JournalPipelineChain,
    posted_contracts: Mutex<Vec<TokenContract>>,
    opened: AtomicBool,
    unreadable: AtomicBool,
    handover: Mutex<Option<Vec<u8>>>,
    block_handover: AtomicBool,
    handover_entered: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ChainBackend for WaitingChain {
    async fn discover_offers(&self) -> Result<Vec<dexdo_core::OfferListing>, ChainError> {
        let mut offers = self.pipeline.discover_offers().await?;
        // After each purchase, a fresh quote points to a DIFFERENT seller deal.
        let generation = self.pipeline.post_count.load(Ordering::SeqCst) + 3;
        offers[0].token_contract = format!("0:{}", format!("{generation:x}").repeat(64));
        Ok(offers)
    }
    async fn post_offer(&self, _: dexdo_core::SellOffer, _: &dyn Note) -> Result<(), ChainError> {
        panic!("no SELL")
    }
    async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
        panic!("no explicit BUY")
    }
    fn model_buy_order_book_identity(&self) -> Option<String> {
        self.pipeline.model_buy_order_book_identity()
    }
    async fn place_buy_by_model_with_submit_identity(
        &self,
        note: &dyn Note,
        order: Option<&dexdo_core::OrderBookOrder>,
        ticks: u128,
        price: u128,
        escrow: u128,
        cursor: &mut dexdo_core::MatchWatchCursor,
        before_post: &mut (dyn FnMut(String, dexdo_core::MatchWatchCursor, u128) -> Result<(), ChainError>
                  + Send),
    ) -> Result<(), ChainError> {
        let result = self
            .pipeline
            .place_buy_by_model_with_submit_identity(
                note,
                order,
                ticks,
                price,
                escrow,
                cursor,
                before_post,
            )
            .await;
        if result.is_ok() {
            self.posted_contracts
                .lock()
                .unwrap()
                .push(order.unwrap().token_contract.clone().unwrap());
        }
        result
    }
    async fn wait_matched_token_contract(
        &self,
        since: i64,
        timeout: std::time::Duration,
    ) -> Result<Option<dexdo_core::MatchedFill>, ChainError> {
        self.pipeline
            .wait_matched_token_contract(since, timeout)
            .await
    }
    async fn poll_matched_model_buys_for_order_book(
        &self,
        book: &str,
        cursor: &mut dexdo_core::MatchWatchCursor,
    ) -> Result<Vec<dexdo_core::MatchedFill>, ChainError> {
        self.pipeline
            .poll_matched_model_buys_for_order_book(book, cursor)
            .await
    }
    async fn read_match(&self, _: &TokenContract) -> Result<dexdo_core::Match, ChainError> {
        panic!("already matched")
    }
    async fn open_stream(
        &self,
        _: &TokenContract,
        _: Vec<u8>,
        _: &dyn Note,
    ) -> Result<(), ChainError> {
        panic!("no OPEN")
    }
    async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
        if self.block_handover.load(Ordering::SeqCst) {
            self.handover_entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(self.handover.lock().unwrap().clone())
    }
    async fn claim_tokens(
        &self,
        _: &TokenContract,
        _: &dyn Note,
        _: u128,
    ) -> Result<(), ChainError> {
        panic!("no claim")
    }
    async fn stop(
        &self,
        _: &TokenContract,
        _: &dyn Note,
    ) -> Result<dexdo_core::Settlement, ChainError> {
        panic!("no settlement")
    }
    async fn deal_state(
        &self,
        _: &TokenContract,
    ) -> Result<Option<dexdo_core::DealChainState>, ChainError> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Err(ChainError::Transport("read unavailable".into()));
        }
        // An expired funding window makes no-handover requests return immediately, without sleeps.
        Ok(Some(deal_state(
            true,
            self.opened.load(Ordering::SeqCst),
            false,
            false,
        )))
    }
    async fn snapshot(&self, _: &TokenContract) -> Option<dexdo_core::StreamSnapshot> {
        None
    }
}

fn waiting_chain(path: &std::path::Path) -> Arc<WaitingChain> {
    let fixture = buyer_submit_test_journal();
    let order = buyer_submit_test_quoted(&fixture);
    let mut pipeline = journal_outcome_chain(path, Vec::new());
    pipeline.fill = Some(dexdo_core::MatchedFill {
        order_id: order.order_id,
        token_contract: order.token_contract.clone().unwrap(),
        ticks: fixture.ticks,
        price_per_tick: order.price_per_tick,
    });
    Arc::new(WaitingChain {
        pipeline,
        posted_contracts: Mutex::new(Vec::new()),
        opened: AtomicBool::new(true),
        unreadable: AtomicBool::new(false),
        handover: Mutex::new(None),
        block_handover: AtomicBool::new(false),
        handover_entered: tokio::sync::Notify::new(),
    })
}

fn api_state(
    chain: Arc<WaitingChain>,
    buyer: Arc<dexdo::buyer::Buyer>,
    dir: &std::path::Path,
    resume: bool,
) -> dexdo::buyer::api::ApiState {
    let fixture = buyer_submit_test_journal();
    let mut args = spot_resume_buyer_args(dir, &fixture.note_addr, "Qwen3-32B");
    args.resume = resume;
    args.policy = None;
    args.preserve_deal_on_exit = true;
    args.ticks = fixture.ticks;
    args.max_price_per_tick = Some(fixture.max_price_per_tick);
    args.escrow = Some(fixture.escrow);
    command::build_on_demand_buyer_api_state(
        chain,
        buyer,
        Arc::new(args),
        None,
        "Qwen3-32B".into(),
        dexdo::buyer::api::ContentCheck::Skip,
        Arc::new(dexdo::seller::ModelsConfig::empty()),
        None,
        dexdo::buyer::api::BuyerApiFailurePolicy::default(),
        None,
        None,
        None,
        command::BuyerChainPreflight::OfflineTest,
        None,
        true,
    )
}

async fn http_request(state: dexdo::buyer::api::ApiState) -> (http::StatusCode, serde_json::Value) {
    use tower::Service;
    let mut router = dexdo::buyer::api::router(state, false);
    let response = router.call(http::Request::builder().method("POST").uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"model":"Qwen3-32B","messages":[{"role":"user","content":"test"}],"max_tokens":1}"#)).unwrap()).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_185_repeated_requests_and_restart_keep_one_funded_deal() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("issue185-repeat");
    let fixture = buyer_submit_test_journal();
    let pool = write_spot_resume_pool(&dir, &fixture.note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let money_lock = command::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
    let _ = std::fs::remove_file(&money_lock.journal_path);
    let chain = waiting_chain(&money_lock.journal_path);
    let buyer = Arc::new(dexdo::buyer::Buyer::generate());
    let state = api_state(chain.clone(), buyer.clone(), &dir, false);
    let mut last_body = serde_json::Value::Null;
    for _ in 0..3 {
        let (status, body) = http_request(state.clone()).await;
        assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        last_body = body;
        assert_eq!(
            chain.pipeline.post_count.load(Ordering::SeqCst),
            1,
            "a second BUY would fund a distinct deal"
        );
        assert_eq!(chain.posted_contracts.lock().unwrap().len(), 1);
        assert!(state.deals.current().await.is_none());
    }
    assert!(money_lock.journal_path.exists());
    assert!(
        last_body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("waiting-for-handover"),
        "{last_body}"
    );
    let replies = tokio::join!(
        http_request(state.clone()),
        http_request(state.clone()),
        http_request(state.clone()),
    );
    for (status, body) in [replies.0, replies.1, replies.2] {
        assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{body}");
    }
    assert_eq!(chain.pipeline.post_count.load(Ordering::SeqCst), 1);
    drop(state);
    // A reconstructed production initializer, including explicit --resume, reads the same journal.
    let restarted = api_state(chain.clone(), buyer, &dir, true);
    let (status, body) = http_request(restarted).await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(chain.pipeline.post_count.load(Ordering::SeqCst), 1);
    let journal = command::load_buyer_submit_journal(&money_lock.journal_path, &fixture.note_addr)
        .unwrap()
        .unwrap();
    assert_eq!(journal.resolved_matches.len(), 1);
    assert_eq!(
        journal.resolved_matches[0].token_contract,
        chain.pipeline.fill.as_ref().unwrap().token_contract
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_185_delayed_handover_adopts_original_and_clears_guard() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("issue185-delayed");
    let fixture = buyer_submit_test_journal();
    let pool = write_spot_resume_pool(&dir, &fixture.note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let money_lock = command::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
    let _ = std::fs::remove_file(&money_lock.journal_path);
    let chain = waiting_chain(&money_lock.journal_path);
    let buyer = Arc::new(dexdo::buyer::Buyer::generate());
    let state = api_state(chain.clone(), buyer.clone(), &dir, false);
    assert!(state.current_deal().await.is_err());
    let tc = chain.pipeline.fill.as_ref().unwrap().token_contract.clone();
    let handover = dexdo_core::Handover {
        endpoint: "https://127.0.0.1:1".into(),
        tls_fingerprint: "aa".repeat(32),
    };
    let seller = dexdo_core::LocalNote::generate();
    *chain.handover.lock().unwrap() =
        Some(seller.encrypt_to(&buyer.note.pubkey(), &handover.to_deal_bytes(&tc)));
    let route = state.current_deal().await.unwrap();
    assert_eq!(route.route.token_contract, tc);
    assert_eq!(chain.pipeline.post_count.load(Ordering::SeqCst), 1);
    assert!(
        !money_lock.journal_path.exists(),
        "durable handle and route release the pending guard"
    );
    let handles = command::deals::list_deal_handles(&dir.join("deals")).unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].1.token_contract, tc);
    assert_eq!(state.current_deal().await.unwrap().route.token_contract, tc);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_185_unopened_bad_handover_and_unreadable_state_never_rebuy() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("issue185-no-endpoint");
    let fixture = buyer_submit_test_journal();
    let pool = write_spot_resume_pool(&dir, &fixture.note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let money_lock = command::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
    let _ = std::fs::remove_file(&money_lock.journal_path);
    let chain = waiting_chain(&money_lock.journal_path);
    chain.opened.store(false, Ordering::SeqCst);
    let state = api_state(
        chain.clone(),
        Arc::new(dexdo::buyer::Buyer::generate()),
        &dir,
        false,
    );
    for _ in 0..2 {
        assert!(state.current_deal().await.is_err());
    }
    chain.opened.store(true, Ordering::SeqCst);
    *chain.handover.lock().unwrap() = Some(vec![0; 5]);
    for _ in 0..2 {
        assert!(state.current_deal().await.is_err());
    }
    chain.unreadable.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        assert!(state.current_deal().await.is_err());
    }
    assert_eq!(chain.pipeline.post_count.load(Ordering::SeqCst), 1);
    assert!(money_lock.journal_path.exists());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_185_cancelled_initializer_retains_funded_journal() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("issue185-cancel");
    let fixture = buyer_submit_test_journal();
    let pool = write_spot_resume_pool(&dir, &fixture.note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let money_lock = command::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
    let _ = std::fs::remove_file(&money_lock.journal_path);
    let chain = waiting_chain(&money_lock.journal_path);
    chain.block_handover.store(true, Ordering::SeqCst);
    let buyer = Arc::new(dexdo::buyer::Buyer::generate());
    let state = api_state(chain.clone(), buyer.clone(), &dir, false);
    let request = tokio::spawn(async move { state.current_deal().await });
    chain.handover_entered.notified().await;
    request.abort();
    assert!(matches!(request.await, Err(error) if error.is_cancelled()));
    assert!(money_lock.journal_path.exists());
    chain.block_handover.store(false, Ordering::SeqCst);
    let restarted = api_state(chain.clone(), buyer, &dir, false);
    assert!(restarted.current_deal().await.is_err());
    assert_eq!(chain.pipeline.post_count.load(Ordering::SeqCst), 1);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_185_policy_successor_retains_guard_until_adopted() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("issue185-policy-successor");
    let fixture = buyer_submit_test_journal();
    let pool = write_spot_resume_pool(&dir, &fixture.note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let money_lock = command::BuyerMoneyLock::open(&fixture.note_addr).unwrap();
    let _ = std::fs::remove_file(&money_lock.journal_path);
    let chain = waiting_chain(&money_lock.journal_path);
    let buyer = dexdo::buyer::Buyer::generate();
    let predecessor = format!("0:{}", "9".repeat(64));
    let intent = command::BuyerSubmitIntent::after(
        command::BuyerSubmitIntentKind::PolicyNextSeller,
        &predecessor,
    );
    let selection = journal_pipeline_selection();
    let outcome = command::execute_buyer_quote_submit(
        chain.as_ref(),
        &buyer,
        false,
        Some(&fixture.note_addr),
        &intent,
        None,
        &selection,
        fixture.ticks,
        fixture.max_price_per_tick,
        fixture.escrow,
        None,
        |_| async { Ok(()) },
    )
    .await
    .unwrap();
    let proof = outcome
        .submit_reconciliation
        .as_ref()
        .expect("confirmed successor has adoption identity");
    assert!(money_lock.journal_path.exists());
    assert_eq!(
        proof.state,
        dexdo::buyer::api::BuyerSubmitReconciliationState::FreshProven
    );
    // A fresh on-demand invocation cannot turn a retained policy successor into a new BUY.
    let state = api_state(chain.clone(), Arc::new(buyer), &dir, false);
    assert!(state.current_deal().await.is_err());
    assert_eq!(chain.pipeline.post_count.load(Ordering::SeqCst), 1);
    command::clear_adopted_buyer_money_journal(
        Some(&fixture.note_addr),
        Some(&proof.submit_identity),
        &outcome.token_contract,
    )
    .unwrap();
    assert!(!money_lock.journal_path.exists());
}
