/// `--wait-for-seller` and `--continuity-mode` are independent of each other and of the buy
/// method; `off` is the default; a stopped buyer takes its resting order out of the book.
mod issue_2163 {
    use super::super as buyer;
    use super::{buyer_journal_test_dir, dexdo_pn_pool_env_lock, issue_2064_pool, EnvVarGuard};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const MODEL: &str = "Qwen3-32B";
    const TICKS: u128 = dexdo_core::params::MIN_STREAM_BUY_TICKS;
    const PRICE: u128 = dexdo_core::PRICE_STEP;
    const ASK_ORDER_ID: u128 = 900;

    fn order_book() -> String {
        format!("0:{}", "2".repeat(64))
    }

    fn tc(digit: &str) -> String {
        format!("0:{}", digit.repeat(64))
    }

    /// A note address of its own per test; `tag` must be hex.
    fn note(tag: &str) -> String {
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()), "{tag}");
        format!("0:{}{}", tag, "0".repeat(64 - tag.len()))
    }

    fn escrow() -> u128 {
        dexdo_core::required_escrow_for_buy(TICKS, PRICE)
    }

    fn terms() -> (u128, u128, u128) {
        (TICKS, PRICE, escrow())
    }

    fn opened_state() -> dexdo_core::DealChainState {
        let now = buyer::unix_now_secs();
        dexdo_core::DealChainState {
            funded: true,
            opened: true,
            probe_accepted: true,
            disputed: false,
            deposit: 1_000,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            funded_time: Some(now),
            probe_tick: 0,
            probe_time: 0,
            last_claim_time: now,
            dispute_time: 0,
        }
    }

    /// What a deal reads like on chain once it has ended: no longer funded, nothing held.
    fn ended_state() -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: false,
            opened: false,
            probe_accepted: false,
            disputed: false,
            deposit: 0,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            funded_time: Some(1),
            probe_tick: 0,
            probe_time: 0,
            last_claim_time: 1,
            dispute_time: 0,
        }
    }

    #[derive(Debug, Clone)]
    struct PostedBuy {
        order_id: u128,
        /// A quoted ask was frozen for it -- a buy that fills now or not at all. `false` is a bid
        /// allowed to rest.
        quoted: bool,
        created_at: i64,
        cancelled: bool,
        filled: bool,
    }

    /// One model book, as the buyer reads and writes it.

    /// `resting_allowed` is what the real backend answers for `--wait-for-seller`; the matrix fills
    /// it from the real backend built from the parsed command line, so it is not this fixture's
    /// opinion. `seller` says whether an ask that crosses the bid is in the book.
    struct Book2163 {
        resting_allowed: bool,
        seller: AtomicBool,
        deal_tc: Mutex<String>,
        buyer_pubkey: Mutex<Option<dexdo_core::NotePubkey>>,
        posted: Mutex<Vec<PostedBuy>>,
        fills: Mutex<Vec<dexdo_core::MatchedFill>>,
        ended: Mutex<std::collections::HashSet<String>>,
        model_quotes: AtomicUsize,
        explicit_quotes: AtomicUsize,
        cancels: AtomicUsize,
        stops: AtomicUsize,
        hidden_placement_reads: AtomicUsize,
        hide_cancel_confirmation: AtomicBool,
        missing_deal: AtomicBool,
        deal_read_error: AtomicBool,
        deadline: AtomicU64,
        expired: AtomicBool,
        refunded: AtomicBool,
        expire_on_cancel: AtomicBool,
        fill_on_cancel: AtomicBool,
        refund_on_cancel: AtomicBool,
        corrupt_fill: AtomicBool,
        order_read_error: AtomicBool,
    }

    impl Book2163 {
        fn new(resting_allowed: bool, seller: bool, deal_tc: &str) -> Self {
            Self {
                resting_allowed,
                seller: AtomicBool::new(seller),
                deal_tc: Mutex::new(deal_tc.to_string()),
                buyer_pubkey: Mutex::new(None),
                posted: Mutex::new(Vec::new()),
                fills: Mutex::new(Vec::new()),
                ended: Mutex::new(Default::default()),
                model_quotes: AtomicUsize::new(0),
                explicit_quotes: AtomicUsize::new(0),
                cancels: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
                hidden_placement_reads: AtomicUsize::new(0),
                hide_cancel_confirmation: AtomicBool::new(false),
                missing_deal: AtomicBool::new(false),
                deal_read_error: AtomicBool::new(false),
                deadline: AtomicU64::new(0),
                expired: AtomicBool::new(false),
                refunded: AtomicBool::new(false),
                expire_on_cancel: AtomicBool::new(false),
                fill_on_cancel: AtomicBool::new(false),
                refund_on_cancel: AtomicBool::new(true),
                corrupt_fill: AtomicBool::new(false),
                order_read_error: AtomicBool::new(false),
            }
        }

        fn for_buyer(self, buyer: &dexdo::buyer::Buyer) -> Self {
            *self.buyer_pubkey.lock().unwrap() = Some(buyer.note.pubkey());
            self
        }

        fn posted(&self) -> Vec<PostedBuy> {
            self.posted.lock().unwrap().clone()
        }

        fn end_deal(&self, token_contract: &str) {
            self.ended
                .lock()
                .unwrap()
                .insert(token_contract.to_string());
        }

        fn ask(&self) -> dexdo_core::OrderBookOrder {
            dexdo_core::OrderBookOrder {
                order_id: ASK_ORDER_ID,
                owner_note: note("5e11"),
                token_contract: Some(self.deal_tc.lock().unwrap().clone()),
                is_buy: false,
                price_per_tick: PRICE,
                ticks: TICKS,
                escrow: 0,
                deadline: 0,
                flags: 0,
                timestamp: 0,
            }
        }

        fn fill(&self, order_id: u128) -> dexdo_core::MatchedFill {
            dexdo_core::MatchedFill {
                order_id,
                token_contract: self.deal_tc.lock().unwrap().clone(),
                ticks: TICKS,
                price_per_tick: PRICE,
            }
        }
    }

    /// The book's own words for "nothing is offered", as the real model-only preflight builds them.
    fn empty_book_refusal() -> dexdo_core::ChainError {
        dexdo_core::ChainError::Chain(format!(
            "buyer model-only preflight failed: {}: {} for max_price_per_tick {PRICE}, requested \
             ticks {TICKS}",
            dexdo_core::params::RAW_MATCHER_NO_SUBMIT_SAFE_ASK,
            dexdo_core::params::EMPTY_MODEL_BOOK_REASON
        ))
    }

    #[async_trait::async_trait]
    impl dexdo_core::ChainBackend for Book2163 {
        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            Ok(Vec::new())
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("the buyer posts no offer")
        }

        async fn place_buy(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("a real-chain buy goes through the durable model-book submit")
        }

        async fn read_match(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("seller-only")
        }

        async fn open_stream(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("seller-only")
        }

        async fn claim_tokens(
            &self,
            _: &dexdo_core::TokenContract,
            _: &dyn dexdo_core::Note,
            _: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("seller-only")
        }

        async fn read_handover(
            &self,
            token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            let Some(buyer_pubkey) = self.buyer_pubkey.lock().unwrap().clone() else {
                return Ok(None);
            };
            let seller = dexdo_core::LocalNote::generate();
            let handover = dexdo_core::Handover {
                endpoint: "https://127.0.0.1:1".to_string(),
                tls_fingerprint: "ab".repeat(32),
            };
            Ok(Some(dexdo_core::Note::encrypt_to(
                &seller,
                &buyer_pubkey,
                &handover.to_deal_bytes(token_contract),
            )))
        }

        async fn stop(
            &self,
            token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.end_deal(token_contract);
            Ok(dexdo_core::Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            })
        }

        async fn snapshot(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }

        fn requires_submit_safe_single_ask_quote(&self) -> bool {
            true
        }

        fn allows_resting_model_buy(&self) -> bool {
            self.resting_allowed
        }

        fn model_buy_order_book_identity(&self) -> Option<String> {
            Some(order_book())
        }

        async fn submit_safe_model_buy_quote_order(
            &self,
            _ticks: u128,
            _max_price_per_tick: u128,
        ) -> Result<Option<dexdo_core::OrderBookOrder>, dexdo_core::ChainError> {
            self.model_quotes.fetch_add(1, Ordering::SeqCst);
            // The real backend freezes no quote for a bid allowed to rest.
            if self.resting_allowed {
                return Ok(None);
            }
            if self.seller.load(Ordering::SeqCst) {
                return Ok(Some(self.ask()));
            }
            Err(empty_book_refusal())
        }

        async fn submit_safe_explicit_buy_quote_order(
            &self,
            token_contract: &dexdo_core::TokenContract,
            _ticks: u128,
            _max_price_per_tick: u128,
        ) -> Result<Option<dexdo_core::OrderBookOrder>, dexdo_core::ChainError> {
            self.explicit_quotes.fetch_add(1, Ordering::SeqCst);
            if self.seller.load(Ordering::SeqCst)
                && token_contract.eq_ignore_ascii_case(&self.deal_tc.lock().unwrap())
            {
                return Ok(Some(self.ask()));
            }
            Err(dexdo_core::ChainError::OrderUnavailable(format!(
                "buyer target preflight failed: no resting ask for expected tokenContract \
                 {token_contract}, and no matchable ask for max_price_per_tick {PRICE}, requested \
                 ticks {TICKS}"
            )))
        }

        async fn place_buy_by_model_with_submit_identity(
            &self,
            _note: &dyn dexdo_core::Note,
            quoted_order: Option<&dexdo_core::OrderBookOrder>,
            _ticks: u128,
            _max_price_per_tick: u128,
            _escrow: u128,
            cursor: &mut dexdo_core::MatchWatchCursor,
            before_post: &mut (dyn FnMut(
                String,
                dexdo_core::MatchWatchCursor,
                u128,
            ) -> Result<(), dexdo_core::ChainError>
                      + Send),
        ) -> Result<(), dexdo_core::ChainError> {
            if self.resting_allowed && quoted_order.is_some() {
                return Err(dexdo_core::ChainError::Chain(
                    "buyer wait-for-seller preflight unexpectedly received a quoted ask".into(),
                ));
            }
            let order_id = {
                let posted = self.posted.lock().unwrap();
                1 + posted.len() as u128
            };
            *cursor = dexdo_core::MatchWatchCursor::new(1);
            before_post(
                format!("boc-sha256:{order_id:064x}"),
                cursor.clone(),
                u128::MAX,
            )?;
            let seller_now = self.seller.load(Ordering::SeqCst);
            self.posted.lock().unwrap().push(PostedBuy {
                order_id,
                quoted: quoted_order.is_some(),
                created_at: buyer::unix_now_secs() as i64,
                cancelled: false,
                filled: seller_now,
            });
            if seller_now {
                self.fills.lock().unwrap().push(self.fill(order_id));
            }
            Ok(())
        }

        async fn wait_matched_token_contract(
            &self,
            _since_unix: i64,
            timeout: std::time::Duration,
        ) -> Result<Option<dexdo_core::MatchedFill>, dexdo_core::ChainError> {
            let started = std::time::Instant::now();
            loop {
                if let Some(fill) = {
                    let mut fills = self.fills.lock().unwrap();
                    (!fills.is_empty()).then(|| fills.remove(0))
                } {
                    return Ok(Some(fill));
                }
                if started.elapsed() >= timeout {
                    return Err(dexdo_core::ChainError::Transport(
                        "timed out waiting for InferenceFilledConfirmed".into(),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        async fn deal_state(
            &self,
            token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<dexdo_core::DealChainState>, dexdo_core::ChainError> {
            if self.deal_read_error.load(Ordering::SeqCst) {
                return Err(dexdo_core::ChainError::Transport(
                    "fixture read outage".into(),
                ));
            }
            if self.missing_deal.load(Ordering::SeqCst) {
                return Ok(None);
            }
            Ok(Some(
                if self.ended.lock().unwrap().contains(token_contract) {
                    ended_state()
                } else {
                    opened_state()
                },
            ))
        }

        async fn buyer_order_facts_for_note(
            &self,
            _order_book: &str,
            buyer_note: &str,
        ) -> Result<Vec<dexdo_core::BuyerOrderFact>, dexdo_core::ChainError> {
            if self.order_read_error.load(Ordering::SeqCst) {
                return Err(dexdo_core::ChainError::Transport(
                    "fixture order facts unavailable".into(),
                ));
            }
            if self
                .hidden_placement_reads
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Ok(Vec::new());
            }
            let mut facts = Vec::new();
            for posted in self.posted.lock().unwrap().iter() {
                facts.push(dexdo_core::BuyerOrderFact {
                    created_at: posted.created_at,
                    note: buyer_note.to_string(),
                    kind: dexdo_core::BuyerOrderFactKind::Placed {
                        order_id: posted.order_id,
                        price_per_tick: PRICE,
                        ticks: TICKS,
                        deadline: match self.deadline.load(Ordering::SeqCst) {
                            0 => buyer::unix_now_secs() + 3_600,
                            deadline => deadline,
                        },
                    },
                });
                if self.expired.load(Ordering::SeqCst) {
                    facts.push(dexdo_core::BuyerOrderFact {
                        created_at: posted.created_at,
                        note: buyer_note.to_string(),
                        kind: dexdo_core::BuyerOrderFactKind::Expired {
                            order_id: posted.order_id,
                        },
                    });
                    if self.refunded.load(Ordering::SeqCst) {
                        facts.push(dexdo_core::BuyerOrderFact {
                            created_at: posted.created_at,
                            note: buyer_note.to_string(),
                            kind: dexdo_core::BuyerOrderFactKind::Refunded {
                                order_id: posted.order_id,
                                amount: escrow(),
                            },
                        });
                    }
                }
                if posted.cancelled && !self.hide_cancel_confirmation.load(Ordering::SeqCst) {
                    facts.push(dexdo_core::BuyerOrderFact {
                        created_at: posted.created_at,
                        note: buyer_note.to_string(),
                        kind: dexdo_core::BuyerOrderFactKind::Cancelled {
                            order_id: posted.order_id,
                            refunded: escrow(),
                        },
                    });
                }
            }
            Ok(facts)
        }

        async fn deal_purchase_terms(
            &self,
            token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<(u128, u128)>, dexdo_core::ChainError> {
            Ok((!self.ended.lock().unwrap().contains(token_contract)).then_some((TICKS, PRICE)))
        }

        async fn buyer_order_is_active_for_owner(
            &self,
            _order_book: &str,
            order_id: u128,
            _buyer_note: &str,
        ) -> Result<bool, dexdo_core::ChainError> {
            Ok(self
                .posted
                .lock()
                .unwrap()
                .iter()
                .any(|posted| posted.order_id == order_id && !posted.cancelled && !posted.filled))
        }

        async fn cancel_buyer_order(&self, order_id: u128) -> Result<(), dexdo_core::ChainError> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            if self.expire_on_cancel.load(Ordering::SeqCst) {
                self.expired.store(true, Ordering::SeqCst);
                self.refunded.store(
                    self.refund_on_cancel.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
                return Ok(());
            }
            for posted in self.posted.lock().unwrap().iter_mut() {
                if posted.order_id == order_id && !posted.filled {
                    if self.fill_on_cancel.load(Ordering::SeqCst) {
                        posted.filled = true;
                    } else {
                        posted.cancelled = true;
                    }
                }
            }
            if self.fill_on_cancel.load(Ordering::SeqCst) {
                return Err(dexdo_core::ChainError::Chain(
                    "order filled before cancel".into(),
                ));
            }
            Ok(())
        }

        async fn poll_matched_model_buys_for_order_book(
            &self,
            _order_book: &str,
            _cursor: &mut dexdo_core::MatchWatchCursor,
        ) -> Result<Vec<dexdo_core::MatchedFill>, dexdo_core::ChainError> {
            Ok(self
                .posted
                .lock()
                .unwrap()
                .iter()
                .filter(|posted| posted.filled)
                .map(|posted| {
                    let mut fill = self.fill(posted.order_id);
                    if self.corrupt_fill.load(Ordering::SeqCst) {
                        fill.ticks += 1;
                    }
                    fill
                })
                .collect())
        }
    }

    fn api_deal_on(
        chain: Arc<Book2163>,
        buyer: &dexdo::buyer::Buyer,
        token_contract: &str,
    ) -> dexdo::buyer::api::ApiDeal {
        let session = Arc::new(dexdo::buyer::api::SessionSettle::new_with_failure_policy(
            chain,
            token_contract.to_string(),
            buyer.note.clone(),
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
        ));
        dexdo::buyer::api::ApiDeal::new(
            dexdo::buyer::api::Route {
                handover: dexdo_core::Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "ab".repeat(32),
                },
                token_contract: token_contract.to_string(),
                max_tokens: buyer::consumer_api_token_budget(TICKS),
            },
            session,
            Arc::new(dexdo::buyer::api::ContentGate::skip()),
        )
    }

    fn route_manager_on(
        chain: Arc<Book2163>,
        buyer: &dexdo::buyer::Buyer,
        token_contract: &str,
    ) -> Arc<dexdo::buyer::api::RouteManager> {
        Arc::new(dexdo::buyer::api::RouteManager::new(api_deal_on(
            chain,
            buyer,
            token_contract,
        )))
    }

    /// Spawn the production watcher for `mode` over a deal that has already ended.
    fn watch_an_ended_deal(
        mode: crate::cli::args::ContinuityModeArg,
        chain: Arc<Book2163>,
        buyer: Arc<dexdo::buyer::Buyer>,
        note_addr: &str,
        deals_dir: std::path::PathBuf,
    ) -> buyer::BuyerContinuityTask {
        let current = tc("4");
        chain.end_deal(&current);
        let deals = route_manager_on(chain.clone(), &buyer, &current);
        buyer::BuyerContinuityTask::new(buyer::spawn_buyer_continuity(
            mode,
            Some(terms()),
            chain,
            buyer,
            deals,
            Some(note_addr.to_string()),
            dexdo::buyer::api::ContentCheck::Skip,
            Arc::new(dexdo::seller::ModelsConfig::empty()),
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            false,
            MODEL.to_string(),
            Some(deals_dir),
        ))
    }

    /// The watcher's own verdict, given the time a few monitor ticks take, or `None` when it is still
    /// running -- the answer a buyer whose BUY rests in the book should give.
    async fn verdict(task: &buyer::BuyerContinuityTask) -> Option<buyer::BuyerRunEnd> {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = task.shutdown_with(Box::pin(async move {
            let _ = rx.await;
        }));
        let ended = tokio::time::timeout(std::time::Duration::from_millis(600), shutdown)
            .await
            .is_ok();
        drop(tx);
        if ended {
            task.stop().await
        } else {
            None
        }
    }

    #[test]
    fn the_default_continuity_mode_is_off() {
        assert_eq!(dexdo_core::params::DEFAULT_CONTINUITY_MODE, "off");
        let crate::Command::Buyer(args) = <crate::Cli as clap::Parser>::try_parse_from([
            "dexdo",
            "buyer",
            "--frame-model",
            MODEL,
            "--max-price-per-tick",
            "1",
        ])
        .expect("a buyer with no --continuity-mode parses")
        .command
        else {
            panic!("expected the buyer command");
        };
        assert_eq!(
            args.continuity_mode,
            crate::cli::args::ContinuityModeArg::Off
        );
        assert_eq!(
            args.continuity_mode.as_planner_mode(),
            dexdo::buyer::continuity::ContinuityMode::Off
        );
        // `off` never asks the planner for a deal: an ended deal, an empty route and a dry budget
        // all answer "do nothing", with demand or without.
        let cfg = dexdo::buyer::continuity::ContinuityConfig::default();
        for demand in [
            dexdo::buyer::continuity::ConsumerDemand::Idle,
            dexdo::buyer::continuity::ConsumerDemand::ActiveOrRecent,
        ] {
            for current in [
                None,
                Some(dexdo::buyer::continuity::DealFacts::closed(tc("4"))),
                Some(dexdo::buyer::continuity::DealFacts::handover_ready(
                    tc("4"),
                    0,
                )),
            ] {
                let action = dexdo::buyer::continuity::BuyerContinuity::default().tick_with_mode(
                    current,
                    None,
                    cfg,
                    dexdo::buyer::continuity::ContinuityMode::Off,
                    demand,
                );
                assert!(
                    matches!(action, dexdo::buyer::continuity::BuyerAction::Noop { .. }),
                    "{action:?}"
                );
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Method {
        Model,
        Market,
        TokenContract,
    }

    /// Every buy method, with and without `--wait-for-seller`, in every continuity mode: the order
    /// flags are the flag's answer alone, a buyer with no seller rests or exits by the flag alone,
    /// and whether the ended deal is replaced is the mode's answer alone.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn buy_method_by_wait_for_seller_by_continuity_mode_matrix() {
        use crate::cli::args::ContinuityModeArg;
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-matrix");
        let note_addr = note("2163a");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let note_key = dir.join("note.key");
        crate::cli::support::write_owner_only_key_fixture(
            &note_key,
            "3d1c8f5b2a704e6913c85af0d27b64e8915caf3072d6be4189305f7ac2b1de60",
        );
        let market = dir.join("market.json");
        std::fs::write(
            &market,
            dexdo_core::MarketManifest {
                network: "shellnet".to_string(),
                frame_model: MODEL.to_string(),
                model_hash: dexdo_core::model_hash_for(MODEL),
                inference_order_book: order_book(),
                root_model: tc("3"),
                token_contract: tc("4"),
                seller_note: note("5e11"),
                nonce: 7,
                price_per_tick: PRICE,
                max_ticks: 8,
            }
            .to_json()
            .unwrap(),
        )
        .unwrap();
        let mut rows = 0;
        for method in [Method::Model, Method::Market, Method::TokenContract] {
            for wait in [false, true] {
                for mode in [
                    ContinuityModeArg::Off,
                    ContinuityModeArg::OnDemand,
                    ContinuityModeArg::Proactive,
                ] {
                    let label = format!("{method:?} wait={wait} mode={}", mode.as_str());
                    let mut argv = vec![
                        "dexdo".to_string(),
                        "buyer".to_string(),
                        "--note-key".to_string(),
                        note_key.display().to_string(),
                        "--note-addr".to_string(),
                        note_addr.clone(),
                        "--ticks".to_string(),
                        TICKS.to_string(),
                        "--max-price-per-tick".to_string(),
                        "1".to_string(),
                        "--continuity-mode".to_string(),
                        mode.as_str().to_string(),
                    ];
                    match method {
                        Method::Model => argv.extend(["--frame-model".into(), MODEL.into()]),
                        Method::Market => {
                            argv.extend(["--market".into(), market.display().to_string()])
                        }
                        Method::TokenContract => argv.extend([
                            "--token-contract".into(),
                            tc("4"),
                            "--frame-model".into(),
                            MODEL.into(),
                        ]),
                    }
                    if wait {
                        argv.push("--wait-for-seller".to_string());
                    }
                    let crate::Command::Buyer(args) =
                        <crate::Cli as clap::Parser>::try_parse_from(&argv)
                            .unwrap_or_else(|error| panic!("{label}: {error}"))
                            .command
                    else {
                        panic!("{label}: expected the buyer command");
                    };
                    assert_eq!(args.continuity_mode, mode, "{label}");

                    // 1. Accepted for every buy method; the run-level refusal is gone.
                    buyer::refuse_wait_for_seller_without_a_buy(&args)
                        .unwrap_or_else(|error| panic!("{label}: {error:#}"));

                    // 2. The flags the real backend built from this command line puts on every
                    // BUY it places -- the first one and each replacement.
                    let explicit_tc = match method {
                        Method::Model => None,
                        _ => Some(
                            crate::cli::support::resolve_market_fields(
                                args.market.as_deref(),
                                args.token_contract.as_deref(),
                                args.frame_model.as_deref(),
                            )
                            .unwrap()
                            .0,
                        ),
                    };
                    let (real, _note) =
                        crate::cli::support::buyer_real_backend_parts(&args, MODEL).unwrap();
                    let expected_flags = if wait {
                        dexdo_core::market::flags::AON
                    } else {
                        dexdo_core::market::flags::AON | dexdo_core::market::flags::FOK
                    };
                    assert_eq!(real.buy_order_flags(), expected_flags, "{label}");
                    let resting_allowed = dexdo_core::ChainBackend::allows_resting_model_buy(&real);
                    assert_eq!(resting_allowed, wait, "{label}");

                    // 3. No seller in the book: the first buy rests with the flag and is refused --
                    // nothing placed, the buyer exits -- without it.
                    let empty = Book2163::new(resting_allowed, false, &tc("4"));
                    let selection = buyer::buyer_quote_selection(
                        &empty,
                        explicit_tc.as_deref(),
                        TICKS,
                        PRICE,
                        None,
                    )
                    .await;
                    if wait {
                        let selection = selection.unwrap_or_else(|e| panic!("{label}: {e:#}"));
                        assert!(selection.resting_buy, "{label}");
                        assert!(selection.quoted_order.is_none(), "{label}");
                    } else {
                        let Err(error) = selection else {
                            panic!("{label}: no seller and no flag must refuse the buy");
                        };
                        let error = format!("{error:#}");
                        assert!(
                            dexdo_core::params::book_refusal_class(&error).is_some(),
                            "{label}: {error}"
                        );
                    }

                    // 4. Replacement, by the mode alone. The terms are the ones every buy method
                    // now carries into the run.
                    let budget = buyer::buyer_fresh_buy_budget(
                        args.ticks,
                        args.max_price_per_tick,
                        args.escrow,
                    );
                    assert!(budget.is_some(), "{label}");
                    if mode == ContinuityModeArg::OnDemand {
                        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
                        let book = Arc::new(
                            Book2163::new(resting_allowed, false, &tc("6")).for_buyer(&shopper),
                        );
                        let ended = api_deal_on(book.clone(), &shopper, &tc("4"));
                        book.end_deal(&tc("4"));
                        ended
                            .session
                            .mark_recovered_serialized("matrix: first deal ended")
                            .await;
                        let mut continuation_args = args.clone();
                        continuation_args.deals_dir = Some(dir.join("deals"));
                        let kept_open = buyer::OnDemandKeptOpen::new();
                        let state = buyer::build_on_demand_buyer_api_state_with(
                            book.clone(),
                            shopper,
                            Arc::new(continuation_args),
                            explicit_tc.clone(),
                            MODEL.to_string(),
                            dexdo::buyer::api::ContentCheck::Skip,
                            Arc::new(dexdo::seller::ModelsConfig::empty()),
                            None,
                            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
                            None,
                            None,
                            None,
                            buyer::BuyerChainPreflight::OfflineTest,
                            Some(ended),
                            true,
                            kept_open.clone(),
                        );
                        let purchase = tokio::spawn(async move {
                            state.deals.current_or_prepare().await.map(|_| ())
                        });
                        if wait {
                            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                while book.posted().is_empty() {
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .expect("on-demand replacement must rest");
                            assert!(!purchase.is_finished(), "{label}");
                            assert!(kept_open.stopped.borrow().is_none(), "{label}");
                            purchase.abort();
                            let _ = purchase.await;
                            assert_eq!(
                                buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                                    .await
                                    .unwrap(),
                                Some(book.posted()[0].order_id)
                            );
                        } else {
                            let refusal = purchase.await.unwrap().unwrap_err();
                            assert!(
                                dexdo_core::params::book_refusal_class(&refusal.to_string())
                                    .is_some(),
                                "{label}: {refusal}"
                            );
                            assert!(kept_open.stopped.borrow().is_some(), "{label}");
                            assert!(book.posted().is_empty(), "{label}");
                        }
                        assert!(!lock.journal_path.exists(), "{label}");
                    } else {
                        let book = Arc::new(Book2163::new(resting_allowed, false, &tc("6")));
                        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
                        let task = watch_an_ended_deal(
                            mode,
                            book.clone(),
                            shopper,
                            &note_addr,
                            dir.join("deals"),
                        );
                        let end = verdict(&task).await;
                        match (mode, wait) {
                            (ContinuityModeArg::Off, _) => {
                                assert!(
                                    matches!(end, Some(buyer::BuyerRunEnd::DealEnded { .. })),
                                    "{label}: {end:?}"
                                );
                                assert!(book.posted().is_empty(), "{label}");
                            }
                            (_, false) => {
                                assert!(
                                    matches!(end, Some(buyer::BuyerRunEnd::NoSeller { .. })),
                                    "{label}: {end:?}"
                                );
                                assert!(book.posted().is_empty(), "{label}");
                            }
                            (_, true) => {
                                assert_eq!(end, None, "{label}: a resting replacement waits");
                                let posted = book.posted();
                                assert_eq!(posted.len(), 1, "{label}");
                                assert!(!posted[0].quoted, "{label}: the replacement rests");
                                task.stop().await;
                                assert_eq!(
                                    buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                                        .await
                                        .unwrap(),
                                    Some(posted[0].order_id),
                                    "{label}"
                                );
                            }
                        }
                        assert!(!lock.journal_path.exists(), "{label}");
                    }
                    rows += 1;
                }
            }
        }
        assert_eq!(rows, 18);
    }

    /// The first deal of a `--market` run is the one its file describes; a deal a resting bid or a
    /// replacement brought is not, and its handle must not claim the file.
    #[test]
    fn a_deal_handle_records_the_market_file_only_for_the_deal_it_describes() {
        let market = std::path::Path::new("market.json");
        assert_eq!(
            buyer::market_path_for_deal(Some(market), Some(&tc("4")), &tc("4")),
            Some(market)
        );
        assert_eq!(
            buyer::market_path_for_deal(Some(market), Some(&tc("4")), &tc("6")),
            None
        );
        assert_eq!(
            buyer::market_path_for_deal(Some(market), None, &tc("6")),
            None
        );
        assert_eq!(
            buyer::market_path_for_deal(None, Some(&tc("4")), &tc("4")),
            None
        );
    }

    fn write_policy(dir: &std::path::Path) -> std::path::PathBuf {
        let policy = dir.join("policy.json");
        std::fs::write(
            &policy,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "buyer": {
                    "on": {
                        "no_handover_after_match": "fail_closed",
                        "malformed_handover": "fail_closed",
                        "dead_gateway": "fail_closed",
                        "empty_stream": "fail_closed",
                        "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                        "bad_output_scam": "stop"
                    },
                    "failover": {"max_sellers_to_try": 1, "total_spend_cap_shells": 1000000000}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        policy
    }

    /// The incident of 2026-10-21, kept so it cannot come back. A buyer started the way that one
    /// was -- by model, `--wait-for-seller`, nothing said about continuity -- whose deal has ended,
    /// with no seller left in the book. The watcher the default mode starts must place nothing and
    /// end the run; the old default `proactive` sent a BUY 7 seconds after the deal closed, and it
    /// rested in the book.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn the_default_mode_places_no_buy_after_the_deal_ends() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-incident");
        let note_addr = note("2163b");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let crate::Command::Buyer(args) = <crate::Cli as clap::Parser>::try_parse_from([
            "dexdo",
            "buyer",
            "--frame-model",
            MODEL,
            "--wait-for-seller",
            "--max-price-per-tick",
            "1",
        ])
        .unwrap()
        .command
        else {
            panic!("expected the buyer command");
        };
        let book = Arc::new(Book2163::new(args.wait_for_seller, false, &tc("6")));
        let task = watch_an_ended_deal(
            args.continuity_mode,
            book.clone(),
            Arc::new(dexdo::buyer::Buyer::generate()),
            &note_addr,
            dir.join("deals"),
        );
        let end = verdict(&task).await;
        task.stop().await;
        let posted = book.posted();
        if !posted.is_empty() {
            // Leave nothing behind for the next test, whatever this one concludes.
            let _ = buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr).await;
            let _ = std::fs::remove_file(&lock.journal_path);
        }
        assert!(
            posted.is_empty(),
            "under the default --continuity-mode {} a BUY was placed after the deal ended: \
             {posted:?}",
            args.continuity_mode.as_str()
        );
        assert!(
            matches!(end, Some(buyer::BuyerRunEnd::DealEnded { .. })),
            "under the default the run ends with its deal: {end:?}"
        );
    }

    #[tokio::test]
    async fn off_waits_on_read_errors_and_exits_when_the_deal_account_is_gone() {
        let book = Arc::new(Book2163::new(false, false, &tc("4")));
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let deals = route_manager_on(book.clone(), &shopper, &tc("4"));
        book.deal_read_error.store(true, Ordering::SeqCst);
        let task = buyer::BuyerContinuityTask::new(Some(buyer::spawn_buyer_deal_end_watch(
            book.clone(),
            deals,
        )));
        assert_eq!(
            verdict(&task).await,
            None,
            "a failed read does not prove closure"
        );
        book.deal_read_error.store(false, Ordering::SeqCst);
        book.missing_deal.store(true, Ordering::SeqCst);
        let end = verdict(&task).await;
        task.stop().await;
        assert!(
            matches!(end, Some(buyer::BuyerRunEnd::DealEnded { .. })),
            "a destroyed contract ends the buyer under off: {end:?}"
        );
        assert!(book.posted().is_empty());
        assert_eq!(book.stops.load(Ordering::SeqCst), 0);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn proactive_replaces_a_destroyed_deal_but_not_a_failed_read() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-destroyed-renewal");
        let note_addr = note("2163e3");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let book = Arc::new(Book2163::new(true, false, &tc("6")));
        book.deal_read_error.store(true, Ordering::SeqCst);
        let task = watch_an_ended_deal(
            crate::cli::args::ContinuityModeArg::Proactive,
            book.clone(),
            Arc::new(dexdo::buyer::Buyer::generate()),
            &note_addr,
            dir.join("deals"),
        );
        assert_eq!(verdict(&task).await, None);
        assert!(
            book.posted().is_empty(),
            "a read failure does not prove closure"
        );
        book.deal_read_error.store(false, Ordering::SeqCst);
        book.missing_deal.store(true, Ordering::SeqCst);
        assert_eq!(verdict(&task).await, None);
        task.stop().await;
        assert_eq!(book.posted().len(), 1, "a destroyed deal must be replaced");
        assert_eq!(
            buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                .await
                .unwrap(),
            Some(book.posted()[0].order_id)
        );
        assert!(!lock.journal_path.exists());
    }

    /// The same run end to end through the serving buyer: by model, `--wait-for-seller`, the
    /// default mode. It serves its deal, the deal ends, the seller is gone -- and the buyer exits by
    /// itself, reports `policy` as the reason it stopped, and placed one BUY in all.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_serving_buyer_under_the_default_exits_when_its_deal_ends() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-serving");
        let note_addr = note("21637");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let policy = write_policy(&dir);
        let deals_dir = dir.join("deals");
        let crate::Command::Buyer(args) = <crate::Cli as clap::Parser>::try_parse_from([
            "dexdo".to_string(),
            "buyer".to_string(),
            "--json".to_string(),
            "--frame-model".to_string(),
            MODEL.to_string(),
            "--wait-for-seller".to_string(),
            "--allow-unverified-model".to_string(),
            "--note-addr".to_string(),
            note_addr.clone(),
            "--ticks".to_string(),
            TICKS.to_string(),
            "--max-price-per-tick".to_string(),
            "1".to_string(),
            "--local-listen".to_string(),
            "127.0.0.1:0".to_string(),
            "--deals-dir".to_string(),
            deals_dir.display().to_string(),
            "--policy".to_string(),
            policy.display().to_string(),
            "--models".to_string(),
            dir.join("models.json").display().to_string(),
        ])
        .unwrap()
        .command
        else {
            panic!("expected the buyer command");
        };
        let first_deal = tc("4");
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let book = Arc::new(Book2163::new(true, true, &first_deal).for_buyer(&shopper));
        let (writer, captured) = crate::cli::machine::BuyerEventWriter::capturing();
        let ender = {
            let book = book.clone();
            let captured = captured.clone();
            let first_deal = first_deal.clone();
            tokio::spawn(async move {
                while !captured
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event["event"] == "endpoint_ready")
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                // The deal ends, and no seller is left in the book.
                book.seller.store(false, Ordering::SeqCst);
                book.end_deal(&first_deal);
            })
        };
        let run = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            buyer::run_buyer_inner(
                args,
                &mut Some(writer),
                &mut buyer::BuyerMachineErrorContext::default(),
                buyer::BuyerCommandRuntime {
                    backend: Some((book.clone(), shopper.note.clone())),
                    chain_preflight: buyer::BuyerChainPreflight::OfflineTest,
                    shutdown: Box::pin(std::future::pending()),
                },
            ),
        )
        .await;
        ender.abort();
        let posted = book.posted();
        let run = run.unwrap_or_else(|_| {
            panic!("the buyer kept running after its deal ended; BUYs placed: {posted:?}")
        });
        if let Err(error) = run {
            panic!("the buyer failed: {error:#}; BUYs placed: {posted:?}");
        }
        assert_eq!(
            posted.len(),
            1,
            "the buyer placed a BUY after its deal ended: {posted:?}"
        );
        let events = captured.lock().unwrap().clone();
        let stopping = events
            .iter()
            .find(|event| event["event"] == "stopping")
            .expect("the buyer reports how it stopped");
        assert_eq!(stopping["reason"], "policy", "{stopping}");
        assert!(events.iter().any(|event| event["event"] == "exiting"));
        assert!(!lock.journal_path.exists());
    }

    /// The incident's second half: a replacement BUY rests, the buyer is stopped. It cancels the
    /// order, the book confirms it, and the next start is clean. Left in place -- which is what
    /// happened on 2026-10-21 -- the record of that BUY refuses the next start.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_stopped_buyer_cancels_its_resting_replacement_and_the_next_start_is_clean() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-stop");
        let note_addr = note("2163c");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let restart_budget = buyer::buyer_fresh_buy_budget(TICKS, Some(PRICE), None);

        for cancel_at_stop in [false, true] {
            let book = Arc::new(Book2163::new(true, false, &tc("6")));
            let shopper = Arc::new(dexdo::buyer::Buyer::generate());
            let task = watch_an_ended_deal(
                crate::cli::args::ContinuityModeArg::Proactive,
                book.clone(),
                shopper.clone(),
                &note_addr,
                dir.join("deals"),
            );
            assert_eq!(verdict(&task).await, None);
            assert_eq!(book.posted().len(), 1, "the replacement BUY rests");
            let journal = buyer::load_buyer_submit_journal(&lock.journal_path, &lock.note_addr)
                .unwrap()
                .expect("the resting replacement is on record");
            assert_eq!(
                journal.intent.kind,
                buyer::BuyerSubmitIntentKind::ContinuityNextSeller
            );
            task.stop().await;
            if cancel_at_stop {
                let cancelled = buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                    .await
                    .unwrap();
                assert_eq!(cancelled, Some(1));
                assert_eq!(book.cancels.load(Ordering::SeqCst), 1);
                assert!(book.posted()[0].cancelled);
                assert!(!lock.journal_path.exists());
            }
            let restart = buyer::raise_pending_buyer_money_before_fresh_reads(
                book.as_ref(),
                shopper.as_ref(),
                Some(&note_addr),
                &buyer::BuyerSubmitIntent::foreground(),
                None,
                restart_budget,
                false,
            )
            .await;
            if cancel_at_stop {
                assert!(
                    restart.unwrap().is_none(),
                    "nothing is pending at the restart"
                );
            } else {
                let error = format!("{:#}", restart.unwrap_err());
                assert!(error.contains("different logical invocation"), "{error}");
                let _ = std::fs::remove_file(&lock.journal_path);
            }
        }
    }

    #[test]
    fn model_book_buys_do_not_preflight_the_market_files_ended_deal() {
        let crate::Command::Buyer(mut args) = <crate::Cli as clap::Parser>::try_parse_from([
            "dexdo",
            "buyer",
            "--market",
            "market.json",
            "--max-price-per-tick",
            "1",
        ])
        .unwrap()
        .command
        else {
            panic!("expected buyer");
        };
        let first = tc("4");
        assert_eq!(
            buyer::buyer_preflight_market(&args, Some(&first)),
            args.market.as_deref()
        );
        assert_eq!(
            buyer::buyer_preflight_market(&args, None),
            None,
            "a replacement still verifies the model book, but the original deal is gone"
        );
        args.wait_for_seller = true;
        assert_eq!(
            buyer::buyer_preflight_market(&args, Some(&first)),
            None,
            "a resting BUY may be filled by another seller, even after the named deal ended"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_stop_waits_for_the_sent_buys_delayed_placement_then_cancels_it() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-delayed-placement");
        let note_addr = note("2163e");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let book = Arc::new(Book2163::new(true, false, &tc("6")));
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let task = watch_an_ended_deal(
            crate::cli::args::ContinuityModeArg::Proactive,
            book.clone(),
            shopper.clone(),
            &note_addr,
            dir.join("deals"),
        );
        assert_eq!(verdict(&task).await, None);
        task.stop().await;
        book.hidden_placement_reads.store(2, Ordering::SeqCst);
        assert_eq!(
            buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(book.posted().len(), 1, "the original BUY is never resent");
        assert_eq!(book.cancels.load(Ordering::SeqCst), 1);
        assert!(!lock.journal_path.exists());
        assert!(buyer::raise_pending_buyer_money_before_fresh_reads(
            book.as_ref(),
            shopper.as_ref(),
            Some(&note_addr),
            &buyer::BuyerSubmitIntent::foreground(),
            None,
            buyer::buyer_fresh_buy_budget(TICKS, Some(PRICE), None),
            false,
        )
        .await
        .unwrap()
        .is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(start_paused = true)]
    async fn a_stop_retains_unconfirmed_money_without_resending_buy_or_cancel() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for hide_placement in [true, false] {
            let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-unconfirmed-stop");
            let note_addr = note(if hide_placement { "2163e1" } else { "2163e2" });
            let pool = issue_2064_pool(&dir, &note_addr);
            let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
            let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
            let book = Arc::new(Book2163::new(true, false, &tc("6")));
            let task = watch_an_ended_deal(
                crate::cli::args::ContinuityModeArg::Proactive,
                book.clone(),
                Arc::new(dexdo::buyer::Buyer::generate()),
                &note_addr,
                dir.join("deals"),
            );
            assert_eq!(verdict(&task).await, None);
            task.stop().await;
            if hide_placement {
                book.hidden_placement_reads
                    .store(usize::MAX, Ordering::SeqCst);
            } else {
                book.hide_cancel_confirmation.store(true, Ordering::SeqCst);
            }
            let before = std::fs::read(&lock.journal_path).unwrap();
            let args = super::spot_resume_buyer_args(&dir, &note_addr, MODEL);
            let error = buyer::cancel_resting_buyer_order_at_stop(book.as_ref(), &args)
                .await
                .expect_err("an unconfirmed outcome must not be reported as a clean stop");
            assert!(error.to_string().contains("record is kept"), "{error:#}");
            assert_eq!(book.posted().len(), 1);
            assert_eq!(
                book.cancels.load(Ordering::SeqCst),
                usize::from(!hide_placement)
            );
            let after = buyer::load_buyer_submit_journal(&lock.journal_path, &note_addr)
                .unwrap()
                .expect("money remains protected");
            let before: buyer::BuyerSubmitJournal = serde_json::from_slice(&before).unwrap();
            assert_eq!(after.submit_identity, before.submit_identity);
            std::fs::remove_file(&lock.journal_path).unwrap();
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn stop_preserves_successor_fill_for_real_resume_before_or_during_cancel() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        for mode in [
            buyer::BuyerSubmitIntentKind::ContinuityNextSeller,
            buyer::BuyerSubmitIntentKind::ContinuityRenewal,
        ] {
            for during_cancel in [false, true] {
                let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-filled-stop-review");
                let note_addr = note("2163f1");
                let pool = issue_2064_pool(&dir, &note_addr);
                let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
                let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
                let _ = std::fs::remove_file(&lock.journal_path);
                let shopper = Arc::new(dexdo::buyer::Buyer::generate());
                let book = Arc::new(Book2163::new(true, false, &tc("6")).for_buyer(&shopper));
                let task = watch_an_ended_deal(
                    crate::cli::args::ContinuityModeArg::Proactive,
                    book.clone(),
                    shopper.clone(),
                    &note_addr,
                    dir.join("deals"),
                );
                assert_eq!(verdict(&task).await, None);
                task.stop().await;
                let mut journal = buyer::load_buyer_submit_journal(&lock.journal_path, &note_addr)
                    .unwrap()
                    .unwrap();
                journal.intent = buyer::BuyerSubmitIntent::after(mode, &tc("4"));
                buyer::write_buyer_submit_journal(&lock.journal_path, &journal).unwrap();
                if during_cancel {
                    book.fill_on_cancel.store(true, Ordering::SeqCst);
                } else {
                    book.posted.lock().unwrap()[0].filled = true;
                }
                assert_eq!(
                    buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                        .await
                        .unwrap(),
                    None
                );
                let adopted = buyer::raise_pending_buyer_money_before_fresh_reads(
                    book.as_ref(),
                    shopper.as_ref(),
                    Some(&note_addr),
                    &buyer::BuyerSubmitIntent::foreground(),
                    None,
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("resume must adopt the paid successor");
                assert_eq!(adopted.token_contract, tc("6"));
                assert_eq!(book.posted().len(), 1, "resume must never buy again");
                assert_eq!(
                    book.cancels.load(Ordering::SeqCst),
                    usize::from(during_cancel)
                );
                let _ = std::fs::remove_file(&lock.journal_path);
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(start_paused = true)]
    async fn stop_cancels_unswept_expiry_and_accepts_only_proven_terminal_refunds() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        for (swept, refunded, during_cancel, refund_during_cancel) in [
            (false, false, false, true),
            (true, true, false, true),
            (false, false, true, true),
            (true, false, false, true),
            (false, false, true, false),
        ] {
            let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-expired-stop-review");
            let note_addr = note("2163f2");
            let pool = issue_2064_pool(&dir, &note_addr);
            let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
            let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
            let _ = std::fs::remove_file(&lock.journal_path);
            let book = Arc::new(Book2163::new(true, false, &tc("6")));
            let task = watch_an_ended_deal(
                crate::cli::args::ContinuityModeArg::Proactive,
                book.clone(),
                Arc::new(dexdo::buyer::Buyer::generate()),
                &note_addr,
                dir.join("deals"),
            );
            assert_eq!(verdict(&task).await, None);
            task.stop().await;
            book.deadline.store(1, Ordering::SeqCst);
            book.expired.store(swept, Ordering::SeqCst);
            book.refunded.store(refunded, Ordering::SeqCst);
            book.expire_on_cancel.store(during_cancel, Ordering::SeqCst);
            book.refund_on_cancel
                .store(refund_during_cancel, Ordering::SeqCst);
            let outcome = buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr).await;
            if (swept && !refunded) || (during_cancel && !refund_during_cancel) {
                assert!(outcome.is_err(), "sweep without refund is unresolved");
                assert!(lock.journal_path.exists());
            } else {
                assert_eq!(
                    outcome.unwrap(),
                    if swept || during_cancel {
                        None
                    } else {
                        Some(1)
                    }
                );
                assert!(!lock.journal_path.exists());
            }
            assert_eq!(book.cancels.load(Ordering::SeqCst), usize::from(!swept));
            assert_eq!(book.posted().len(), 1);
            let _ = std::fs::remove_file(&lock.journal_path);
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn actual_on_demand_api_stop_cancels_the_buy_its_http_request_is_waiting_for() {
        let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
        for unreadable in [false, true] {
            let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-api-drain-review");
            let note_addr = note("2163f3");
            let pool = issue_2064_pool(&dir, &note_addr);
            let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
            let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
            let _ = std::fs::remove_file(&lock.journal_path);
            let mut args = super::spot_resume_buyer_args(&dir, &note_addr, MODEL);
            args.resume = false;
            args.wait_for_seller = true;
            args.ticks = TICKS;
            args.escrow = None;
            let shopper = Arc::new(dexdo::buyer::Buyer::generate());
            let book = Arc::new(Book2163::new(true, false, &tc("6")).for_buyer(&shopper));
            let (writer, captured) = crate::cli::machine::BuyerEventWriter::capturing();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let runtime = buyer::BuyerCommandRuntime {
                backend: Some((book.clone(), shopper.note.clone())),
                chain_preflight: buyer::BuyerChainPreflight::OfflineTest,
                shutdown: Box::pin(async move {
                    let _ = stopped.await;
                }),
            };
            let mut run = tokio::spawn(async move {
                buyer::run_buyer_inner(
                    args,
                    &mut Some(writer),
                    &mut buyer::BuyerMachineErrorContext::default(),
                    runtime,
                )
                .await
            });
            let base_url = tokio::time::timeout(
                std::time::Duration::from_secs(dexdo_core::params::DEFAULT_CHAIN_READ_TIMEOUT_SECS),
                async {
                    loop {
                        if let Some(url) = captured
                            .lock()
                            .unwrap()
                            .iter()
                            .find(|e| e["event"] == "endpoint_ready")
                            .and_then(|e| e["base_url"].as_str())
                            .map(str::to_owned)
                        {
                            break url;
                        }
                        tokio::task::yield_now().await;
                    }
                },
            )
            .await
            .unwrap();
            let request = tokio::spawn(async move {
                reqwest::Client::new()
                .post(format!("{base_url}/chat/completions"))
                .json(&serde_json::json!({"model":MODEL,"messages":[{"role":"user","content":"hi"}],"max_tokens":1,"stream":false}))
                .send().await
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(dexdo_core::params::DEFAULT_CHAIN_READ_TIMEOUT_SECS),
                async {
                    while book.posted().is_empty() {
                        tokio::task::yield_now().await;
                    }
                },
            )
            .await
            .unwrap();
            book.order_read_error.store(unreadable, Ordering::SeqCst);
            stop.send(()).unwrap();
            let completed = tokio::time::timeout(std::time::Duration::from_secs(2), &mut run).await;
            let cancels = book.cancels.load(Ordering::SeqCst);
            // Observe the production outcome before cleaning up a failed regression run.
            if completed.is_err() {
                run.abort();
                request.abort();
            }
            assert!(
                completed.is_ok(),
                "shutdown waited for its own purchase; cancels={cancels}"
            );
            let result = completed.unwrap().unwrap();
            assert_eq!(result.is_err(), unreadable, "{result:?}");
            request.await.unwrap().unwrap();
            assert_eq!(cancels, usize::from(!unreadable));
            assert_eq!(book.posted().len(), 1);
            assert_eq!(book.posted()[0].cancelled, !unreadable);
            assert_eq!(lock.journal_path.exists(), unreadable);
            if unreadable {
                std::fs::remove_file(&lock.journal_path).unwrap();
            }
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn api_stop_releases_initial_and_replacement_purchases_before_drain() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for replacement in [false, true] {
            let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-api-purchase-stop");
            let note_addr = note("2163f4");
            let pool = issue_2064_pool(&dir, &note_addr);
            let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
            let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
            let _ = std::fs::remove_file(&lock.journal_path);
            let mut args = super::spot_resume_buyer_args(&dir, &note_addr, MODEL);
            args.resume = false;
            args.wait_for_seller = true;
            args.ticks = TICKS;
            args.escrow = None;
            let args = Arc::new(args);
            let shopper = Arc::new(dexdo::buyer::Buyer::generate());
            let book = Arc::new(Book2163::new(true, false, &tc("6")).for_buyer(&shopper));
            let active = if replacement {
                let active = api_deal_on(book.clone(), &shopper, &tc("4"));
                book.end_deal(&tc("4"));
                book.deal_read_error.store(true, Ordering::SeqCst);
                Some(active)
            } else {
                None
            };
            let state = buyer::build_on_demand_buyer_api_state_with(
                book.clone(),
                shopper.clone(),
                args.clone(),
                None,
                MODEL.into(),
                dexdo::buyer::api::ContentCheck::Skip,
                Arc::new(dexdo::seller::ModelsConfig::empty()),
                None,
                dexdo::buyer::api::BuyerApiFailurePolicy::default(),
                None,
                None,
                None,
                buyer::BuyerChainPreflight::OfflineTest,
                active,
                true,
                buyer::OnDemandKeptOpen::new(),
            );
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let continuity = buyer::BuyerContinuityTask::new(if replacement {
                buyer::spawn_buyer_continuity(
                    crate::cli::args::ContinuityModeArg::OnDemand,
                    Some(terms()),
                    book.clone(),
                    shopper,
                    state.deals.clone(),
                    Some(note_addr.clone()),
                    dexdo::buyer::api::ContentCheck::Skip,
                    Arc::new(dexdo::seller::ModelsConfig::empty()),
                    dexdo::buyer::api::BuyerApiFailurePolicy::default(),
                    false,
                    MODEL.into(),
                    Some(dir.join("deals")),
                )
            } else {
                None
            });
            if replacement {
                assert_eq!(verdict(&continuity).await, None);
                let active = state.deals.current().await.unwrap();
                assert!(
                    !active.session.is_settled(),
                    "a read failure does not end a paid deal"
                );
                book.deal_read_error.store(false, Ordering::SeqCst);
                book.missing_deal.store(true, Ordering::SeqCst);
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !active.session.is_settled() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                book.missing_deal.store(false, Ordering::SeqCst);
                assert!(
                    book.posted().is_empty(),
                    "on-demand waits for the next request after closure"
                );
            }
            let (shutdown, cancelled) = buyer::buyer_api_shutdown_before_drain(
                &continuity,
                Box::pin(async move {
                    let _ = stopped.await;
                }),
                state.deals.clone(),
                book.clone(),
                args,
            );
            let (addr, mut server) =
                dexdo::buyer::api::serve("127.0.0.1:0".parse().unwrap(), state, shutdown)
                    .await
                    .unwrap();
            let request = tokio::spawn(async move {
                reqwest::Client::new()
                .post(format!("http://{addr}/v1/chat/completions"))
                .json(&serde_json::json!({"model":MODEL,"messages":[{"role":"user","content":"hi"}],"max_tokens":1,"stream":false}))
                .send().await
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while book.posted().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!request.is_finished());
            stop.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut server)
                .await
                .unwrap()
                .unwrap();
            cancelled.await.unwrap().unwrap();
            assert_eq!(
                request.await.unwrap().unwrap().status(),
                reqwest::StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(book.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(book.posted().len(), 1);
            assert!(book.posted()[0].cancelled);
            assert!(!lock.journal_path.exists());
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn proactive_stop_cancels_before_a_long_http_request_drains() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-proactive-drain");
        let note_addr = note("2163f5");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let book = Arc::new(Book2163::new(true, false, &tc("6")).for_buyer(&shopper));
        let continuity = watch_an_ended_deal(
            crate::cli::args::ContinuityModeArg::Proactive,
            book.clone(),
            shopper.clone(),
            &note_addr,
            dir.join("deals"),
        );
        assert_eq!(verdict(&continuity).await, None);
        const KEY_ENV: &str = "DEXDO_2163_DRAIN_PROVIDER_KEY";
        let _key = EnvVarGuard::set(KEY_ENV, std::ffi::OsStr::new("test-only"));
        let (provider_addr, calls, release, provider) =
            super::start_issue_547_provider(super::Issue547ProviderBehavior::FailWhenReleased)
                .await;
        let upstream = dexdo::seller::UpstreamConfig::OpenAi(dexdo::seller::OpenAiConfig {
            base_url: format!("http://{provider_addr}/v1"),
            frame_model: MODEL.into(),
            api_key_env: KEY_ENV.into(),
            ..Default::default()
        });
        let (gateway, handover) =
            super::start_issue_547_gateway(upstream, &shopper, &tc("8")).await;
        let mut active = api_deal_on(book.clone(), &shopper, &tc("8"));
        active.route.handover = handover;
        active.observe_deal_state(&Ok(Some(opened_state()))).await;
        let state = dexdo::buyer::api::ApiState {
            buyer: shopper,
            frame_model: MODEL.into(),
            mock_model: false,
            renewal_enabled: true,
            deals: Arc::new(dexdo::buyer::api::RouteManager::new(active)),
            delivery_events: None,
            usage: None,
        };
        let args = Arc::new(super::spot_resume_buyer_args(&dir, &note_addr, MODEL));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (shutdown, cancelled) = buyer::buyer_api_shutdown_before_drain(
            &continuity,
            Box::pin(async move {
                let _ = stopped.await;
            }),
            state.deals.clone(),
            book.clone(),
            args,
        );
        let (addr, mut server) =
            dexdo::buyer::api::serve("127.0.0.1:0".parse().unwrap(), state, shutdown)
                .await
                .unwrap();
        let request = tokio::spawn(async move {
            reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&serde_json::json!({"model":MODEL,"messages":[{"role":"user","content":"hi"}],"max_tokens":1,"stream":false}))
            .send().await
        });
        super::wait_for_counter(&calls, 1, "2163 in-flight provider request").await;
        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), cancelled)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            !server.is_finished(),
            "ordinary in-flight HTTP must still drain"
        );
        assert!(!request.is_finished());
        assert_eq!(book.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(book.posted().len(), 1);
        assert!(!lock.journal_path.exists());
        release.release();
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut server)
            .await
            .unwrap()
            .unwrap();
        request.await.unwrap().unwrap();
        assert_eq!(book.posted().len(), 1, "no renewal can race shutdown/drain");
        gateway.server_task.abort();
        provider.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn stop_refuses_an_uncorrelated_fill_without_clearing_or_adopting_it() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-foreign-stop");
        let note_addr = note("2163f6");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let book = Arc::new(Book2163::new(true, false, &tc("6")).for_buyer(&shopper));
        let task = watch_an_ended_deal(
            crate::cli::args::ContinuityModeArg::Proactive,
            book.clone(),
            shopper.clone(),
            &note_addr,
            dir.join("deals"),
        );
        assert_eq!(verdict(&task).await, None);
        task.stop().await;
        let before = std::fs::read(&lock.journal_path).unwrap();
        book.posted.lock().unwrap()[0].filled = true;
        book.corrupt_fill.store(true, Ordering::SeqCst);
        assert!(buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
            .await
            .is_err());
        assert_eq!(std::fs::read(&lock.journal_path).unwrap(), before);
        assert!(buyer::raise_pending_buyer_money_before_fresh_reads(
            book.as_ref(),
            shopper.as_ref(),
            Some(&note_addr),
            &buyer::BuyerSubmitIntent::foreground(),
            None,
            None,
            true
        )
        .await
        .is_err());
        assert_eq!(book.posted().len(), 1);
        assert_eq!(book.cancels.load(Ordering::SeqCst), 0);
        std::fs::remove_file(&lock.journal_path).unwrap();
    }

    proptest::proptest! {
        #[test]
        fn off_never_plans_another_purchase(remaining in proptest::num::u64::ANY,
            closed in proptest::bool::ANY, active_demand in proptest::bool::ANY) {
            use dexdo::buyer::continuity::*;
            let current = if closed { DealFacts::closed(tc("4")) }
                else { DealFacts::handover_ready(tc("4"), remaining) };
            let action = BuyerContinuity::default().tick_with_mode(Some(current), None,
                ContinuityConfig::default(), ContinuityMode::Off,
                if active_demand { ConsumerDemand::ActiveOrRecent } else { ConsumerDemand::Idle });
            proptest::prop_assert!(matches!(action,
                BuyerAction::Noop { .. } | BuyerAction::ServeCurrent { .. }), "{action:?}");
        }
    }

    /// Nothing resting, nothing sent: no record at all, and a record whose BUY the book has already
    /// filled, both end without a cancel.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_stop_with_no_resting_buy_sends_no_cancel() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-no-cancel");
        let note_addr = note("2163d");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);

        let book = Book2163::new(true, false, &tc("6"));
        assert_eq!(
            buyer::cancel_resting_buyer_order(&book, &note_addr)
                .await
                .unwrap(),
            None
        );
        assert_eq!(book.cancels.load(Ordering::SeqCst), 0);

        // A resting replacement that a seller then fills: it is no longer in the book.
        let book = Arc::new(Book2163::new(true, false, &tc("6")));
        let task = watch_an_ended_deal(
            crate::cli::args::ContinuityModeArg::Proactive,
            book.clone(),
            Arc::new(dexdo::buyer::Buyer::generate()),
            &note_addr,
            dir.join("deals"),
        );
        assert_eq!(verdict(&task).await, None);
        task.stop().await;
        book.posted.lock().unwrap()[0].filled = true;
        assert_eq!(
            buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                .await
                .unwrap(),
            None
        );
        assert_eq!(book.cancels.load(Ordering::SeqCst), 0);
        assert!(
            lock.journal_path.exists(),
            "a filled BUY stays on record for the next start to adopt"
        );
        let _ = std::fs::remove_file(&lock.journal_path);
    }

    /// Stopped while its first BUY rests -- before any endpoint exists -- the buyer still takes the
    /// BUY out of the book, and the next start is clean. This is a `--market` buy with
    /// `--wait-for-seller`: the bid rests on the model book, as a buy by model does.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_buyer_stopped_while_its_first_buy_rests_cancels_it_before_exiting() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-first-rest");
        let note_addr = note("2163e");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let market = dir.join("market.json");
        std::fs::write(
            &market,
            dexdo_core::MarketManifest {
                network: "shellnet".to_string(),
                frame_model: MODEL.to_string(),
                model_hash: dexdo_core::model_hash_for(MODEL),
                inference_order_book: order_book(),
                root_model: tc("3"),
                token_contract: tc("4"),
                seller_note: note("5e11"),
                nonce: 7,
                price_per_tick: PRICE,
                max_ticks: 8,
            }
            .to_json()
            .unwrap(),
        )
        .unwrap();
        let crate::Command::Buyer(args) = <crate::Cli as clap::Parser>::try_parse_from([
            "dexdo".to_string(),
            "buyer".to_string(),
            "--json".to_string(),
            "--market".to_string(),
            market.display().to_string(),
            "--wait-for-seller".to_string(),
            "--allow-unverified-model".to_string(),
            "--note-addr".to_string(),
            note_addr.clone(),
            "--ticks".to_string(),
            TICKS.to_string(),
            "--max-price-per-tick".to_string(),
            "1".to_string(),
            "--deals-dir".to_string(),
            dir.join("deals").display().to_string(),
            "--policy".to_string(),
            write_policy(&dir).display().to_string(),
            "--models".to_string(),
            dir.join("models.json").display().to_string(),
            "--mock-model".to_string(),
        ])
        .unwrap()
        .command
        else {
            panic!("expected the buyer command");
        };
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let book = Arc::new(Book2163::new(true, false, &tc("4")).for_buyer(&shopper));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let stopper = {
            let book = book.clone();
            tokio::spawn(async move {
                while book.posted().is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                let _ = stop.send(true);
            })
        };
        let (writer, _captured) = crate::cli::machine::BuyerEventWriter::capturing();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            buyer::run_buyer_until_stopped(
                buyer::run_buyer_inner(
                    args,
                    &mut Some(writer),
                    &mut buyer::BuyerMachineErrorContext::default(),
                    buyer::BuyerCommandRuntime {
                        backend: Some((book.clone(), shopper.note.clone())),
                        chain_preflight: buyer::BuyerChainPreflight::OfflineTest,
                        shutdown: Box::pin(buyer::operator_stopped(stopped.clone())),
                    },
                ),
                stopped,
            ),
        )
        .await
        .expect("a stopped buyer exits");
        stopper.await.unwrap();
        let error = format!("{:#}", result.unwrap_err());
        assert!(
            error.contains("stopped before its endpoint came up")
                && error.contains(&format!("resting buy order #{} was cancelled", 1)),
            "{error}"
        );
        let posted = book.posted();
        assert_eq!(posted.len(), 1);
        assert!(
            !posted[0].quoted,
            "the --market bid rested on the model book"
        );
        assert!(posted[0].cancelled);
        assert_eq!(book.cancels.load(Ordering::SeqCst), 1);
        assert!(!lock.journal_path.exists());
        let restart = buyer::raise_pending_buyer_money_before_fresh_reads(
            book.as_ref(),
            shopper.as_ref(),
            Some(&note_addr),
            &buyer::BuyerSubmitIntent::foreground(),
            Some(&tc("4")),
            buyer::buyer_fresh_buy_budget(TICKS, Some(PRICE), None),
            false,
        )
        .await;
        assert!(restart.unwrap().is_none());
    }

    /// On demand, a `--market` run buys its first deal from the market, and every replacement from
    /// the model book: the first deal has ended and cannot be bought again.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn on_demand_buys_the_replacement_of_an_explicit_deal_from_the_model_book() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-on-demand");
        let note_addr = note("2163f");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let first = tc("4");
        let second = tc("6");
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let book = Arc::new(Book2163::new(false, true, &first).for_buyer(&shopper));
        let mut args = super::spot_resume_buyer_args(&dir, &note_addr, MODEL);
        args.resume = false;
        args.token_contract = Some(first.clone());
        args.ticks = TICKS;
        args.max_price_per_tick = Some(PRICE);
        args.escrow = None;
        let continuation = buyer::buyer_fresh_buy_budget(TICKS, Some(PRICE), None);
        let state = buyer::build_on_demand_buyer_api_state(
            book.clone(),
            shopper.clone(),
            Arc::new(args),
            Some(first.clone()),
            MODEL.to_string(),
            dexdo::buyer::api::ContentCheck::Skip,
            Arc::new(dexdo::seller::ModelsConfig::empty()),
            None,
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            None,
            None,
            None,
            buyer::BuyerChainPreflight::OfflineTest,
            None,
            continuation.is_some(),
        );
        let deal = state.deals.current_or_prepare().await.unwrap();
        assert_eq!(deal.route.token_contract, first);
        assert_eq!(book.explicit_quotes.load(Ordering::SeqCst), 1);
        assert_eq!(book.model_quotes.load(Ordering::SeqCst), 0);

        deal.session
            .mark_recovered_serialized("test: the first deal ended")
            .await;
        book.end_deal(&first);
        *book.deal_tc.lock().unwrap() = second.clone();
        let replacement = state.deals.current_or_prepare().await.unwrap();
        assert_eq!(replacement.route.token_contract, second);
        assert_eq!(
            book.explicit_quotes.load(Ordering::SeqCst),
            1,
            "the ended deal is not bought again"
        );
        assert_eq!(book.model_quotes.load(Ordering::SeqCst), 1);
        assert_eq!(book.posted().len(), 2);
        let _ = std::fs::remove_file(&lock.journal_path);
    }

    /// The on-demand state a run builds after its first deal has ended, with the shared stop watch
    /// the process listens on.
    fn on_demand_after_an_ended_deal(
        book: Arc<Book2163>,
        shopper: Arc<dexdo::buyer::Buyer>,
        dir: &std::path::Path,
        note_addr: &str,
    ) -> (
        dexdo::buyer::api::ApiState,
        Arc<buyer::OnDemandKeptOpen>,
        dexdo::buyer::api::ApiDeal,
    ) {
        let ended = api_deal_on(book.clone(), &shopper, &tc("4"));
        book.end_deal(&tc("4"));
        let mut args = super::spot_resume_buyer_args(dir, note_addr, MODEL);
        args.resume = false;
        args.ticks = TICKS;
        args.max_price_per_tick = Some(PRICE);
        args.escrow = None;
        let kept_open = buyer::OnDemandKeptOpen::new();
        let state = buyer::build_on_demand_buyer_api_state_with(
            book,
            shopper,
            Arc::new(args),
            None,
            MODEL.to_string(),
            dexdo::buyer::api::ContentCheck::Skip,
            Arc::new(dexdo::seller::ModelsConfig::empty()),
            None,
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            None,
            None,
            None,
            buyer::BuyerChainPreflight::OfflineTest,
            Some(ended.clone()),
            true,
            kept_open.clone(),
        );
        (state, kept_open, ended)
    }

    /// On demand, without `--wait-for-seller`: the request after the deal ends finds no seller, the
    /// purchase is refused with nothing placed, and the buyer is told to stop -- the same answer a
    /// first buy gets.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn on_demand_without_the_flag_ends_the_buyer_when_the_replacement_finds_no_seller() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-on-demand-no-seller");
        let note_addr = note("21638");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let book = Arc::new(Book2163::new(false, false, &tc("6")).for_buyer(&shopper));
        let (state, kept_open, ended) =
            on_demand_after_an_ended_deal(book.clone(), shopper, &dir, &note_addr);
        ended
            .session
            .mark_recovered_serialized("test: the deal ended")
            .await;
        let Err(refusal) = state.deals.current_or_prepare().await else {
            panic!("no seller and no --wait-for-seller: the replacement must be refused");
        };
        assert!(
            dexdo_core::params::book_refusal_class(&refusal.to_string()).is_some(),
            "{refusal}"
        );
        let stopped = kept_open.stopped.borrow().clone();
        assert!(
            stopped
                .as_deref()
                .is_some_and(|reason| dexdo_core::params::book_refusal_class(reason).is_some()),
            "the buyer must be told to stop with the book's refusal: {stopped:?}"
        );
        assert!(book.posted().is_empty(), "nothing is left in the book");
        assert!(!lock.journal_path.exists());
    }

    /// On demand, with `--wait-for-seller`: the replacement rests on the model book without a
    /// frozen quote, nothing tells the buyer to stop, and a stop then cancels it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn on_demand_with_the_flag_rests_the_replacement_and_keeps_the_buyer() {
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2163-on-demand-rests");
        let note_addr = note("21639");
        let pool = issue_2064_pool(&dir, &note_addr);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = buyer::BuyerMoneyLock::open(&note_addr).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let shopper = Arc::new(dexdo::buyer::Buyer::generate());
        let book = Arc::new(Book2163::new(true, false, &tc("6")).for_buyer(&shopper));
        let (state, kept_open, ended) =
            on_demand_after_an_ended_deal(book.clone(), shopper, &dir, &note_addr);
        ended
            .session
            .mark_recovered_serialized("test: the deal ended")
            .await;
        let deals = state.deals.clone();
        let purchase = tokio::spawn(async move { deals.current_or_prepare().await.map(|_| ()) });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while book.posted().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the replacement is posted");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let posted = book.posted();
        assert_eq!(posted.len(), 1);
        assert!(
            !posted[0].quoted,
            "the replacement rests without a frozen quote"
        );
        assert!(!purchase.is_finished(), "the purchase waits for a seller");
        assert_eq!(*kept_open.stopped.borrow(), None, "nothing stops the buyer");
        purchase.abort();
        let _ = purchase.await;
        assert_eq!(
            buyer::cancel_resting_buyer_order(book.as_ref(), &note_addr)
                .await
                .unwrap(),
            Some(posted[0].order_id)
        );
        assert!(!lock.journal_path.exists());
    }
}
