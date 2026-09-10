mod issue_1203 {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const TICKS: u128 = 2;
    const MAX_PRICE: u128 = 1_000_000;

    #[derive(Clone, Copy)]
    enum SubmitOutcome {
        SuccessThenPollError,
        PreparationRefusal,
    }

    struct ContinuitySubmitChain {
        outcome: SubmitOutcome,
        order: dexdo_core::OrderBookOrder,
        expected_journal_path: std::path::PathBuf,
        before_post_count: AtomicUsize,
        post_count: AtomicUsize,
        fill_poll_count: AtomicUsize,
    }

    impl ContinuitySubmitChain {
        fn new(outcome: SubmitOutcome, expected_journal_path: std::path::PathBuf) -> Self {
            Self {
                outcome,
                order: dexdo_core::OrderBookOrder {
                    order_id: 1203,
                    owner_note: format!("0:{}", "d".repeat(64)),
                    token_contract: Some(format!("0:{}", "3".repeat(64))),
                    is_buy: false,
                    price_per_tick: MAX_PRICE,
                    ticks: TICKS,
                    escrow: 0,
                    deadline: u64::MAX,
                    flags: 0,
                    timestamp: 1,
                },
                expected_journal_path,
                before_post_count: AtomicUsize::new(0),
                post_count: AtomicUsize::new(0),
                fill_poll_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl dexdo_core::ChainBackend for ContinuitySubmitChain {
        async fn claim_tokens(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
            _cumulative_tokens: u128,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not used by continuity submit tests")
        }

        async fn discover_offers(
            &self,
        ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
            unimplemented!("the submit-safe quote row is authoritative")
        }

        async fn post_offer(
            &self,
            _offer: dexdo_core::SellOffer,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not used by continuity submit tests")
        }

        async fn place_buy(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not used by continuity submit tests")
        }

        async fn submit_safe_model_buy_quote_order(
            &self,
            _ticks: u128,
            _max_price_per_tick: u128,
        ) -> Result<Option<dexdo_core::OrderBookOrder>, dexdo_core::ChainError> {
            Ok(Some(self.order.clone()))
        }

        fn requires_submit_safe_single_ask_quote(&self) -> bool {
            true
        }

        fn model_buy_order_book_identity(&self) -> Option<String> {
            Some(format!("0:{}", "2".repeat(64)))
        }

        async fn place_buy_by_model(
            &self,
            _note: &dyn dexdo_core::Note,
            _ticks: u128,
            _max_price_per_tick: u128,
            _escrow: u128,
            _flags: u8,
            _deadline: u64,
        ) -> Result<(), dexdo_core::ChainError> {
            match self.outcome {
                SubmitOutcome::SuccessThenPollError => {
                    self.post_count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
                SubmitOutcome::PreparationRefusal => {
                    Err(dexdo_core::ChainError::MoneySubmitPreparation(
                        "injected refusal before POST".to_string(),
                    ))
                }
            }
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
            assert_eq!(quoted_order, Some(&self.order));
            *cursor = dexdo_core::MatchWatchCursor::new(77);
            before_post(
                format!("boc-sha256:{}", "a".repeat(64)),
                cursor.clone(),
                u128::MAX,
            )?;
            self.before_post_count.fetch_add(1, Ordering::SeqCst);
            assert!(
                self.expected_journal_path.exists(),
                "the durable record must exist before the POST seam"
            );
            match self.outcome {
                SubmitOutcome::SuccessThenPollError => {
                    self.post_count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
                SubmitOutcome::PreparationRefusal => {
                    Err(dexdo_core::ChainError::MoneySubmitPreparation(
                        "injected refusal before POST".to_string(),
                    ))
                }
            }
        }

        async fn wait_matched_token_contract(
            &self,
            _since_unix: i64,
            _timeout: std::time::Duration,
        ) -> Result<Option<dexdo_core::MatchedFill>, dexdo_core::ChainError> {
            self.fill_poll_count.fetch_add(1, Ordering::SeqCst);
            Err(dexdo_core::ChainError::Transport(
                "injected terminal fill-poll failure after successful POST".to_string(),
            ))
        }

        async fn read_match(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
            unimplemented!("not used by continuity submit tests")
        }

        async fn open_stream(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _enc_endpoint: Vec<u8>,
            _note: &dyn dexdo_core::Note,
        ) -> Result<(), dexdo_core::ChainError> {
            unimplemented!("not used by continuity submit tests")
        }

        async fn read_handover(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
            unimplemented!("not used by continuity submit tests")
        }

        async fn stop(
            &self,
            _token_contract: &dexdo_core::TokenContract,
            _note: &dyn dexdo_core::Note,
        ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
            unimplemented!("not used by continuity submit tests")
        }

        async fn snapshot(
            &self,
            _token_contract: &dexdo_core::TokenContract,
        ) -> Option<dexdo_core::StreamSnapshot> {
            None
        }
    }

    struct BuyerMoneyArtifacts(Vec<std::path::PathBuf>);

    impl Drop for BuyerMoneyArtifacts {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn setup(
        label: &str,
        outcome: SubmitOutcome,
    ) -> (
        std::path::PathBuf,
        super::TempDirCleanup,
        String,
        String,
        super::super::BuyerMoneyLock,
        BuyerMoneyArtifacts,
        ContinuitySubmitChain,
    ) {
        use sha2::Digest;

        let (dir, cleanup) = super::buyer_journal_test_dir(label);
        let note_addr = format!(
            "0:{}",
            hex::encode(sha2::Sha256::digest(dir.to_string_lossy().as_bytes()))
        );
        let current = format!("0:{}", "1".repeat(64));
        let pool_path = dir.join("pool.json");
        crate::cli::support::write_owner_only_key_fixture(
            &pool_path,
            &serde_json::to_string_pretty(&serde_json::json!({
                "token_type": dexdo_core::params::SHELL_CURRENCY_ID,
                "notes": [{
                    "address": note_addr,
                    "owner_secret_key_hex": "00"
                }]
            }))
            .unwrap(),
        );
        let money_lock = super::super::BuyerMoneyLock::open(&note_addr).unwrap();
        let artifacts = BuyerMoneyArtifacts(vec![
            money_lock.path.clone(),
            money_lock.journal_path.clone(),
            money_lock.subscriptions_path.clone(),
        ]);
        for path in &artifacts.0 {
            let _ = std::fs::remove_file(path);
        }
        let chain = ContinuitySubmitChain::new(outcome, money_lock.journal_path.clone());
        (
            pool_path, cleanup, note_addr, current, money_lock, artifacts, chain,
        )
    }

    fn planner_with_pending(current: &str) -> dexdo::buyer::continuity::BuyerContinuity {
        let mut planner = dexdo::buyer::continuity::BuyerContinuity::default();
        let action = planner.tick(
            Some(dexdo::buyer::continuity::DealFacts::open(current, 1)),
            None,
            planner_config(),
        );
        assert_eq!(
            action,
            dexdo::buyer::continuity::BuyerAction::PrepareNextDeal {
                current: current.to_string()
            }
        );
        planner
    }

    fn planner_config() -> dexdo::buyer::continuity::ContinuityConfig {
        dexdo::buyer::continuity::ContinuityConfig {
            renewal_threshold_tokens: 10,
            match_open_timeout_secs: 600,
        }
    }

    fn run_submit(
        chain: &ContinuitySubmitChain,
        note_addr: &str,
        current: &str,
        planner: &mut dexdo::buyer::continuity::BuyerContinuity,
    ) -> anyhow::Result<dexdo_core::TokenContract> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build continuity submit test runtime");
        let buyer = dexdo::buyer::Buyer::generate();
        let intent = super::super::BuyerSubmitIntent::after(
            super::super::BuyerSubmitIntentKind::ContinuityRenewal,
            current,
        );
        runtime.block_on(super::super::submit_buyer_continuity_next_deal(
            chain,
            &buyer,
            Some(planner),
            &current.to_string(),
            Some(note_addr),
            &intent,
            super::super::BuyerOrderTerms {
                ticks: TICKS,
                max_price: MAX_PRICE,
                escrow: dexdo_core::required_escrow_for_buy(TICKS, MAX_PRICE),
            },
        ))
    }

    #[test]
    fn issue_1203_successful_post_then_terminal_poll_keeps_pending_and_blocks_second_buy() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (pool_path, _cleanup, note_addr, current, _money_lock, _artifacts, chain) = setup(
            "issue-1203-unknown-pending",
            SubmitOutcome::SuccessThenPollError,
        );
        let _env = super::EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let mut planner = planner_with_pending(&current);

        let error = run_submit(&chain, &note_addr, &current, &mut planner)
            .expect_err("the injected terminal fill poll must remain unknown");

        assert_eq!(chain.post_count.load(Ordering::SeqCst), 1);
        assert_eq!(chain.fill_poll_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            planner.tick(
                Some(dexdo::buyer::continuity::DealFacts::open(&current, 1)),
                None,
                planner_config(),
            ),
            dexdo::buyer::continuity::BuyerAction::Noop {
                reason: "next-deal-already-pending"
            },
            "an unknown post-submit outcome must not re-arm the planner"
        );
        assert!(
            super::super::is_ambiguous_submit_error(&error),
            "the existing ChainError distinction must carry the unknown outcome: {error:#}"
        );
    }

    #[test]
    fn issue_1203_pre_submit_refusal_clears_pending_and_rearms() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (pool_path, _cleanup, note_addr, current, money_lock, _artifacts, chain) = setup(
            "issue-1203-known-refusal",
            SubmitOutcome::PreparationRefusal,
        );
        let _env = super::EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let mut planner = planner_with_pending(&current);

        let error = run_submit(&chain, &note_addr, &current, &mut planner)
            .expect_err("the injected pre-submit refusal must be returned");

        assert!(!super::super::is_ambiguous_submit_error(&error));
        assert_eq!(chain.post_count.load(Ordering::SeqCst), 0);
        assert!(
            !money_lock.journal_path.exists(),
            "a proven no-money-moved refusal must clear the pre-POST record"
        );
        assert_eq!(
            planner.tick(
                Some(dexdo::buyer::continuity::DealFacts::open(&current, 1)),
                None,
                planner_config(),
            ),
            dexdo::buyer::continuity::BuyerAction::PrepareNextDeal {
                current: current.clone()
            },
            "a proven no-money-moved refusal remains eligible for a later retry"
        );
    }

    #[test]
    fn issue_1203_unknown_outcome_retains_identifying_buy_journal() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (pool_path, _cleanup, note_addr, current, money_lock, _artifacts, chain) =
            setup("issue-1203-journal", SubmitOutcome::SuccessThenPollError);
        let _env = super::EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let mut planner = planner_with_pending(&current);

        run_submit(&chain, &note_addr, &current, &mut planner)
            .expect_err("the injected terminal fill poll must remain unknown");

        let journal = super::super::load_buyer_submit_journal(&money_lock.journal_path, &note_addr)
            .unwrap()
            .expect("the possibly landed continuity BUY must retain its journal");
        assert_eq!(
            journal.submit_identity,
            format!("boc-sha256:{}", "a".repeat(64))
        );
        assert_eq!(journal.cursor.since_unix, 77);
        assert_eq!(journal.ticks, TICKS);
        assert_eq!(journal.max_price_per_tick, MAX_PRICE);
        assert_eq!(
            journal.escrow,
            dexdo_core::required_escrow_for_buy(TICKS, MAX_PRICE)
        );
        assert_eq!(journal.quoted_order, Some(chain.order.clone()));
        assert_eq!(
            journal
                .quoted_order
                .as_ref()
                .and_then(|order| order.token_contract.as_ref()),
            chain.order.token_contract.as_ref(),
            "the quoted row identifies the possibly matched TokenContract"
        );
        assert_eq!(
            journal.intent.kind,
            super::super::BuyerSubmitIntentKind::ContinuityRenewal
        );
        assert_eq!(
            journal.intent.predecessor_token_contract.as_deref(),
            Some(current.as_str())
        );
        assert_eq!(chain.before_post_count.load(Ordering::SeqCst), 1);
        assert_eq!(chain.post_count.load(Ordering::SeqCst), 1);
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn issue_1203_unknown_warning_does_not_claim_submit_failed() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (pool_path, _cleanup, note_addr, current, _money_lock, _artifacts, chain) =
            setup("issue-1203-warning", SubmitOutcome::SuccessThenPollError);
        let _env = super::EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());
        let mut planner = planner_with_pending(&current);
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || SharedWriter(Arc::clone(&captured)))
            .finish();

        let error = tracing::subscriber::with_default(subscriber, || {
            run_submit(&chain, &note_addr, &current, &mut planner)
        })
        .expect_err("the injected terminal fill poll must remain unknown");
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("buy outcome unknown after money submit"),
            "the warning must name the unknown money outcome: {output}"
        );
        assert!(
            output.contains("fresh buy suppressed"),
            "the warning must say the planner remains disarmed: {output}"
        );
        assert!(
            !output.contains("submit/match failed") && !output.contains("submit failed"),
            "the warning must not claim an unknown submit failed: {output}"
        );
        assert!(super::super::is_ambiguous_submit_error(&error));
    }
}
