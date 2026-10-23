/// `keep_open` on the buyer-CLI policy surfaces: the handover wait and the one-shot stream.
mod keep_open_2148 {
    use super::{buyer_policy, ready_funded_never_opened_state, RecordingRecoveryChain};
    use crate::cli::policy::{
        BadOutputScamAction, DeadGatewayAction, EmptyStreamAction, MalformedHandoverAction,
        NoHandoverAfterMatchAction, SellerStallsMidStreamAction,
    };
    use std::sync::atomic::Ordering;

    fn all_keep_open() -> crate::cli::policy::BuyerRuntimePolicy {
        buyer_policy(
            NoHandoverAfterMatchAction::KeepOpen,
            MalformedHandoverAction::KeepOpen,
            DeadGatewayAction::KeepOpen,
            EmptyStreamAction::KeepOpen,
            SellerStallsMidStreamAction::KeepOpen,
            BadOutputScamAction::KeepOpen,
        )
    }

    fn handover_policy(
        no_handover: NoHandoverAfterMatchAction,
        malformed: MalformedHandoverAction,
    ) -> crate::cli::policy::BuyerRuntimePolicy {
        buyer_policy(
            no_handover,
            malformed,
            DeadGatewayAction::FailClosed,
            EmptyStreamAction::FailClosed,
            SellerStallsMidStreamAction::AcceptDeliveredThenReclaim,
            BadOutputScamAction::Stop,
        )
    }

    fn assert_no_chain_write(chain: &RecordingRecoveryChain) {
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 0);
    }

    /// Each handover field decides only its own failure: a malformed handover is kept reading by
    /// `malformed_handover`, a missing one is kept waiting by `no_handover_after_match`.
    #[test]
    fn handover_wait_keeps_open_only_for_the_field_that_owns_the_failure() {
        use super::super::handover_wait_keeps_open as keeps;
        let others = [
            (
                NoHandoverAfterMatchAction::WaitThenReclaim,
                MalformedHandoverAction::Reclaim,
            ),
            (
                NoHandoverAfterMatchAction::NextSeller,
                MalformedHandoverAction::Dispute,
            ),
            (
                NoHandoverAfterMatchAction::FailClosed,
                MalformedHandoverAction::FailClosed,
            ),
        ];
        assert!(!keeps(None, true));
        assert!(!keeps(None, false));
        for (no_handover, malformed) in others {
            let neither = handover_policy(no_handover, malformed);
            assert!(!keeps(Some(&neither), true));
            assert!(!keeps(Some(&neither), false));
            let reading = handover_policy(no_handover, MalformedHandoverAction::KeepOpen);
            assert!(keeps(Some(&reading), true));
            assert!(!keeps(Some(&reading), false), "{no_handover:?}");
            let waiting = handover_policy(NoHandoverAfterMatchAction::KeepOpen, malformed);
            assert!(keeps(Some(&waiting), false));
            assert!(!keeps(Some(&waiting), true), "{malformed:?}");
        }
        assert!(keeps(Some(&all_keep_open()), true));
        assert!(keeps(Some(&all_keep_open()), false));
    }

    /// Both handover waits (the startup one and the on-demand one) gate the malformed refusal and
    /// the deadline on the same decision, so a kept-open failure falls through to the poll.
    #[test]
    fn both_handover_waits_poll_again_instead_of_applying_a_closing_policy() {
        let source = include_str!("../buyer.rs");
        let tests = source.find("#[cfg(test)]\nmod tests").unwrap();
        let production = &source[..tests];
        let decide = "let keep_open = handover_wait_keeps_open(buyer_policy.as_ref(), malformed);";
        let malformed = "if malformed && !keep_open {";
        let deadline = "if !keep_open && machine::now_unix()? >= hv_deadline_unix {";
        assert_eq!(production.matches(decide).count(), 2);
        assert_eq!(production.matches(malformed).count(), 2);
        assert_eq!(production.matches(deadline).count(), 2);
        assert_eq!(
            production
                .matches("let malformed = is_malformed_handover_error(&e);")
                .count(),
            2
        );
        let mut from = 0;
        for _ in 0..2 {
            let at = from + production[from..].find(decide).unwrap();
            let gate = at + production[at..].find(malformed).unwrap();
            let apply = gate
                + production[gate..]
                    .find("apply_malformed_handover_policy(")
                    .unwrap();
            let window = apply + production[apply..].find(deadline).unwrap();
            let policy = window
                + production[window..]
                    .find("apply_no_handover_after_match_policy(")
                    .unwrap();
            let poll = policy
                + production[policy..]
                    .find("wait_for_seller_handover_poll(")
                    .unwrap();
            assert!(
                at < gate && gate < apply && apply < window && window < policy && policy < poll
            );
            from = poll;
        }
    }

    /// Neither handover helper writes to chain under `keep_open`, even when reached directly.
    #[tokio::test]
    async fn handover_helpers_make_no_chain_write_under_keep_open() {
        let chain = RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state());
        let buyer =
            dexdo::buyer::Buyer::from_note(std::sync::Arc::new(dexdo_core::LocalNote::generate()));
        let policy = all_keep_open();
        let outcome = super::super::apply_no_handover_after_match_policy(
            &chain,
            &buyer,
            &"tc-keep-open".to_string(),
            &policy,
            false,
            Some((1, 1, 1)),
            1,
            "diagnostic",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            super::super::NoHandoverPolicyOutcome::RetryCurrent
        ));
        super::super::apply_malformed_handover_policy(
            &chain,
            &buyer,
            &"tc-keep-open".to_string(),
            &policy,
            false,
            &anyhow::anyhow!("malformed handover: invalid bytes"),
            None,
            None,
        )
        .await
        .unwrap();
        assert_no_chain_write(&chain);
    }

    /// A one-shot buyer has one request: under `keep_open` it reports the deal as kept open and
    /// submits nothing, including from the session's drop backup.
    #[tokio::test]
    async fn oneshot_stream_failures_keep_the_deal_open() {
        let chain = std::sync::Arc::new(RecordingRecoveryChain::default());
        let policy = all_keep_open();
        let session = dexdo::buyer::api::SessionSettle::new_with_failure_policy(
            chain.clone(),
            "tc-oneshot-keep-open".to_string(),
            std::sync::Arc::new(dexdo_core::LocalNote::generate()),
            policy.as_api_failure_policy(),
        );
        let report = match super::super::apply_oneshot_dead_gateway_policy(
            &session,
            &"tc-oneshot-keep-open".to_string(),
            Some(&policy),
            1,
        )
        .await
        {
            super::super::OneShotStreamPolicyOutcome::TerminalReport(report) => report,
            other => panic!("expected a report, got {other:?}"),
        };
        assert!(report.contains("failure_class=dead_gateway"), "{report}");
        assert!(report.contains("action=keep_open"), "{report}");
        assert!(report.contains("result=deal_kept_open"), "{report}");
        let report = super::super::apply_oneshot_empty_stream_policy(
            &session,
            &"tc-oneshot-keep-open".to_string(),
            Some(&policy),
        )
        .await;
        assert!(report.contains("failure_class=empty_stream"), "{report}");
        assert!(report.contains("result=deal_kept_open"), "{report}");
        assert!(!session.is_closed());
        drop(session);
        tokio::task::yield_now().await;
        assert_no_chain_write(&chain);
    }

    /// The on-demand initializer runs inside the per-request purchase budget, so under
    /// `keep_open` a handover that never comes ends the request, and the next request runs the
    /// initializer again. That re-run must resume the deal the first run already bought and wait on
    /// it, never place a second BUY: under `keep_open` nothing reclaims the first escrow.
    // Serializes the process-global pool fixture for the whole async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn on_demand_rerun_under_keep_open_waits_on_the_bought_deal_without_a_second_buy() {
        use super::{
            buyer_journal_test_dir, dexdo_pn_pool_env_lock, issue_2064_pool,
            issue_2064_set_handover, issue_2064_stream_terms, issue_2064_tc,
            spot_resume_buyer_args, EnvVarGuard,
        };
        // Longer than a few handover polls, far shorter than the real purchase budget. The
        // client gives up first, which drops the handler and with it the initializer run: the
        // same cancellation the purchase budget applies.
        let budget = std::time::Duration::from_secs(2);
        let _env_lock = dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for field in ["no_handover_after_match", "malformed_handover"] {
            let (dir, _cleanup) = buyer_journal_test_dir("buyer-2148-on-demand-rerun");
            let note = keep_open_test_note();
            let pool = issue_2064_pool(&dir, &note);
            let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
            let lock = super::super::BuyerMoneyLock::open(&note).unwrap();
            let _ = std::fs::remove_file(&lock.journal_path);
            let bought = issue_2064_tc("4");
            let terms = issue_2064_stream_terms();
            let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::generate());
            let chain = std::sync::Arc::new(RecordingRecoveryChain {
                next_match: Some(bought.clone()),
                model_buy_terms: Some(terms),
                expected_journal_path: Some(lock.journal_path.clone()),
                handover_token_contract: Some(bought.clone()),
                handover: std::sync::Mutex::new(
                    (field == "malformed_handover").then(|| vec![1, 2, 3]),
                ),
                ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
            });
            let mut args = spot_resume_buyer_args(&dir, &note, "Qwen3-32B");
            args.resume = false;
            args.ticks = terms.ticks;
            args.max_price_per_tick = Some(terms.max_price);
            args.escrow = Some(terms.escrow);
            args.continuity_mode = super::super::ContinuityModeArg::OnDemand;
            let deals_dir = args.deals_dir.clone().unwrap();
            let policy = if field == "malformed_handover" {
                handover_policy(
                    NoHandoverAfterMatchAction::FailClosed,
                    MalformedHandoverAction::KeepOpen,
                )
            } else {
                handover_policy(
                    NoHandoverAfterMatchAction::KeepOpen,
                    MalformedHandoverAction::FailClosed,
                )
            };
            let state = super::super::build_on_demand_buyer_api_state(
                chain.clone(),
                buyer.clone(),
                std::sync::Arc::new(args),
                None,
                "Qwen3-32B".to_string(),
                dexdo::buyer::api::ContentCheck::Skip,
                std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
                Some(policy.clone()),
                policy.as_api_failure_policy(),
                None,
                None,
                None,
                super::super::BuyerChainPreflight::OfflineTest,
                None,
                true,
            );
            let deals = state.deals.clone();
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let (addr, task) = dexdo::buyer::api::serve(
                "127.0.0.1:0".parse().unwrap(),
                state,
                false,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
            .expect("bind the on-demand API");
            let mut task = AbortOnDrop(task);
            let ask = move |timeout: std::time::Duration| async move {
                reqwest::Client::builder()
                    .timeout(timeout)
                    .build()
                    .unwrap()
                    .post(format!("http://{addr}/v1/chat/completions"))
                    .json(&serde_json::json!({
                        "model": "Qwen3-32B",
                        "messages": [{"role": "user", "content": "keep open"}],
                        "max_tokens": 1,
                        "stream": false
                    }))
                    .send()
                    .await
            };

            // First request: buys, waits, and is ended before any handover arrives.
            assert!(
                ask(budget).await.is_err(),
                "{field}: the first request must still be waiting when it is ended"
            );
            assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1, "{field}");
            // Its run is gone: the handover is no longer being read.
            let reads = chain.handover_reads.load(Ordering::SeqCst);
            assert!(reads > 0, "{field}");
            tokio::time::sleep(budget).await;
            assert_eq!(
                chain.handover_reads.load(Ordering::SeqCst),
                reads,
                "{field}: the first run must be cancelled with its request"
            );

            // Second request: the initializer runs again. It must wait on the bought deal.
            let mut second = AbortOnDrop(tokio::spawn(ask(std::time::Duration::from_secs(30))));
            tokio::time::sleep(budget).await;
            assert!(
                !second.0.is_finished(),
                "{field}: the second run stopped waiting"
            );
            assert!(
                chain.handover_reads.load(Ordering::SeqCst) > reads,
                "{field}: the second run is not reading the handover"
            );
            assert_eq!(
                chain.place_next_calls.load(Ordering::SeqCst),
                1,
                "{field}: the second run placed a second BUY"
            );
            // The seller finally writes a good handover for the deal the first run bought.
            issue_2064_set_handover(chain.as_ref(), buyer.as_ref(), &bought);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), &mut second.0)
                .await
                .unwrap_or_else(|_| panic!("{field}: the second run never took the handover"));
            assert_eq!(
                deals
                    .current()
                    .await
                    .unwrap_or_else(|| panic!("{field}: no deal was published"))
                    .route
                    .token_contract,
                bought,
                "{field}"
            );
            assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1, "{field}");
            assert_no_chain_write_but_the_buy(&chain, field);
            let handle = crate::cli::deals::handle_path(
                &deals_dir,
                &crate::cli::deals::make_handle_id(
                    &bought,
                    crate::cli::deals::DealHandleRole::Buyer,
                ),
            );
            assert_eq!(
                crate::cli::deals::load_deal_handle(&handle)
                    .unwrap()
                    .token_contract,
                bought,
                "{field}"
            );
            let _ = shutdown_tx.send(());
            let _ = (&mut task.0).await;
            let _ = std::fs::remove_file(&lock.journal_path);
        }
    }

    /// A spawned run that dies with its test: a failed assertion must not leave a run behind that
    /// still holds the fixture note's money lock for the next test.
    struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

    impl<T> Drop for AbortOnDrop<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    fn assert_no_chain_write_but_the_buy(chain: &RecordingRecoveryChain, field: &str) {
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 0, "{field}");
        assert_eq!(chain.reclaim_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    }

    /// Log lines written while a test body runs.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    impl Captured {
        fn lines_with(&self, marker: &str) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .filter(|line| line.contains(marker))
                .map(str::to_string)
                .collect()
        }
    }

    fn capture_errors() -> (Captured, tracing::subscriber::DefaultGuard) {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new("error"))
            .finish();
        (captured, tracing::subscriber::set_default(subscriber))
    }

    /// One kept-open purchase on the fixture chain: the deal is funded, never opened, and its
    /// state is the fixture's mutable `successor_state`, so a test can close it mid-wait.
    struct KeptOpenFixture {
        _dir: (std::path::PathBuf, super::TempDirCleanup),
        _env: super::EnvVarGuard,
        lock: super::super::BuyerMoneyLock,
        bought: String,
        buyer: std::sync::Arc<dexdo::buyer::Buyer>,
        chain: std::sync::Arc<RecordingRecoveryChain>,
        args: super::super::BuyerArgs,
    }

    fn kept_open_fixture(name: &str, field: &str, answer: &str) -> KeptOpenFixture {
        use super::{
            buyer_journal_test_dir, issue_2064_pool, issue_2064_stream_terms, issue_2064_tc,
            spot_resume_buyer_args, EnvVarGuard,
        };
        let dir = buyer_journal_test_dir(name);
        let note = keep_open_test_note();
        let pool = issue_2064_pool(&dir.0, &note);
        let env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = super::super::BuyerMoneyLock::open(&note).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let bought = issue_2064_tc("4");
        let terms = issue_2064_stream_terms();
        let chain = std::sync::Arc::new(RecordingRecoveryChain {
            next_match: Some(bought.clone()),
            model_buy_terms: Some(terms),
            expected_journal_path: Some(lock.journal_path.clone()),
            handover_token_contract: Some(bought.clone()),
            successor_state_for: Some(bought.clone()),
            successor_state: std::sync::Mutex::new(Some(ready_funded_never_opened_state())),
            ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
        });
        let mut args = spot_resume_buyer_args(&dir.0, &note, "Qwen3-32B");
        args.resume = false;
        args.ticks = terms.ticks;
        args.max_price_per_tick = Some(terms.max_price);
        args.escrow = Some(terms.escrow);
        args.continuity_mode = super::super::ContinuityModeArg::OnDemand;
        let policy_path = args.policy.as_ref().unwrap();
        let mut policy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(policy_path).unwrap()).unwrap();
        policy["buyer"]["on"][field] = serde_json::json!(answer);
        std::fs::write(policy_path, serde_json::to_vec(&policy).unwrap()).unwrap();
        KeptOpenFixture {
            _dir: dir,
            _env: env,
            lock,
            bought,
            buyer: std::sync::Arc::new(dexdo::buyer::Buyer::generate()),
            chain,
            args,
        }
    }

    impl KeptOpenFixture {
        /// Wait until the run under test is inside the handover wait.
        async fn wait_in_handover_wait(&self) {
            for _ in 0..200 {
                if self.chain.handover_reads.load(Ordering::SeqCst) > 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("the run never reached the handover wait");
        }

        fn close_the_deal(&self, state: Option<dexdo_core::DealChainState>) {
            *self.chain.successor_state.lock().unwrap() = state;
        }

        fn assert_stop_line(&self, error: &anyhow::Error, state: &str) {
            let line = format!("{error:#}");
            let order_book = super::super::display_dexdo_address(format!("0:{}", "2".repeat(64)));
            for field in [
                "keep_open handover wait stopped".to_string(),
                "model=Qwen3-32B".to_string(),
                format!("order_book={order_book}"),
                format!(
                    "token_contract={}",
                    super::super::display_token_contract(&self.bought)
                ),
                format!("state={state}"),
            ] {
                assert!(line.contains(&field), "missing {field}: {line}");
            }
            assert_eq!(
                self.chain.place_next_calls.load(Ordering::SeqCst),
                1,
                "{line}"
            );
            assert_no_chain_write_but_the_buy(&self.chain, "stop");
        }
    }

    fn not_funded() -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: false,
            deposit: 0,
            ..ready_funded_never_opened_state()
        }
    }

    #[test]
    fn keep_open_deal_check_interval_is_one_minute() {
        assert_eq!(
            dexdo_core::params::BUYER_KEEP_OPEN_DEAL_CHECK_INTERVAL,
            std::time::Duration::from_secs(60)
        );
    }

    /// Blocker 5, on-demand: the request budget ended the first run, someone closed the deal, and
    /// the next run stops the buyer instead of waiting forever -- no new BUY, no chain write, the
    /// deal released, and the process told to stop.
    // Serializes the process-global pool fixture for the whole async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn on_demand_keep_open_wait_stops_on_a_closed_deal() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (field, closed, state) in [
            ("no_handover_after_match", None, "not_found"),
            ("malformed_handover", Some(not_funded()), "not_funded"),
        ] {
            let fixture = kept_open_fixture("buyer-2148-closed-on-demand", field, "keep_open");
            if field == "malformed_handover" {
                *fixture.chain.handover.lock().unwrap() = Some(vec![1, 2, 3]);
            }
            let shared = super::super::OnDemandKeptOpen::new();
            let mut stopped = shared.stopped.subscribe();
            let run = || {
                super::super::ON_DEMAND_KEPT_OPEN_WAIT.scope(
                    shared.clone(),
                    super::issue_2064_run_entrypoint(
                        true,
                        fixture.chain.clone(),
                        fixture.buyer.clone(),
                        fixture.args.clone(),
                    ),
                )
            };
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), run())
                    .await
                    .is_err(),
                "{field}: a funded deal keeps the wait open"
            );
            assert!(shared.wait.lock().unwrap().is_some(), "{field}");
            fixture.close_the_deal(closed);
            let error = tokio::time::timeout(std::time::Duration::from_secs(10), run())
                .await
                .unwrap_or_else(|_| panic!("{field}: a closed deal must end the wait"))
                .expect_err("a closed deal stops the buyer");
            fixture.assert_stop_line(&error, state);
            assert!(
                shared.wait.lock().unwrap().is_none(),
                "{field}: deal released"
            );
            assert!(
                stopped.borrow_and_update().as_deref() == Some(format!("{error:#}").as_str()),
                "{field}: the process is told to stop with the same line"
            );
            let _ = std::fs::remove_file(&fixture.lock.journal_path);
        }
    }

    /// Blocker 5, foreground: the same closed deal ends `dexdo buyer` itself, non-zero.
    // Serializes the process-global pool fixture for the whole async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn foreground_keep_open_wait_stops_on_a_closed_deal() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut fixture = kept_open_fixture(
            "buyer-2148-closed-foreground",
            "no_handover_after_match",
            "keep_open",
        );
        fixture.args.continuity_mode = super::super::ContinuityModeArg::Proactive;
        // A one-shot buyer (`--mock-model`, no endpoint) runs the same foreground wait without
        // opening a local usage store.
        fixture.args.local_listen = None;
        fixture.args.mock.mock_model = true;
        let mut run = AbortOnDrop(tokio::spawn(super::issue_2064_run_entrypoint(
            false,
            fixture.chain.clone(),
            fixture.buyer.clone(),
            fixture.args.clone(),
        )));
        fixture.wait_in_handover_wait().await;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(!run.0.is_finished(), "a funded deal keeps the wait open");
        fixture.close_the_deal(None);
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), &mut run.0)
            .await
            .expect("a closed deal must end the wait")
            .unwrap()
            .expect_err("the foreground buyer exits non-zero");
        fixture.assert_stop_line(&error, "not_found");
        let _ = std::fs::remove_file(&fixture.lock.journal_path);
    }

    /// Blocker 5: a state read that fails is logged as an error naming the deal and retried; the
    /// wait goes on, and a deal that is still funded keeps it going.
    // Serializes the process-global pool fixture for the whole async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn keep_open_deal_check_logs_a_failed_read_and_keeps_waiting() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (captured, _log) = capture_errors();
        let fixture = kept_open_fixture(
            "buyer-2148-check-read-failure",
            "no_handover_after_match",
            "keep_open",
        );
        let shared = super::super::OnDemandKeptOpen::new();
        let run = super::super::ON_DEMAND_KEPT_OPEN_WAIT.scope(
            shared.clone(),
            super::issue_2064_run_entrypoint(
                true,
                fixture.chain.clone(),
                fixture.buyer.clone(),
                fixture.args.clone(),
            ),
        );
        let fail_reads = async {
            fixture.wait_in_handover_wait().await;
            fixture
                .chain
                .successor_state_read_failures
                .store(usize::MAX, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            fixture
                .chain
                .successor_state_read_failures
                .store(0, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        };
        tokio::select! {
            result = run => panic!("the wait ended: {result:?}"),
            () = fail_reads => {}
        }
        let lines = captured.lines_with("could not read the deal's on-chain state");
        assert!(lines.len() >= 2, "one line per check: {lines:#?}");
        let order_book = super::super::display_dexdo_address(format!("0:{}", "2".repeat(64)));
        for line in &lines {
            assert!(line.contains("ERROR"), "{line}");
            assert!(line.contains("model=\"Qwen3-32B\""), "{line}");
            assert!(line.contains(&format!("order_book={order_book}")), "{line}");
            assert!(
                line.contains(&format!(
                    "token_contract={}",
                    super::super::display_token_contract(&fixture.bought)
                )),
                "{line}"
            );
            assert!(line.contains("error="), "{line}");
        }
        assert!(shared.stopped.borrow().is_none());
        assert_eq!(fixture.chain.place_next_calls.load(Ordering::SeqCst), 1);
        assert_no_chain_write_but_the_buy(&fixture.chain, "read failure");
        let _ = std::fs::remove_file(&fixture.lock.journal_path);
    }

    /// A closing answer is unchanged: its wait ends at the deadline exactly as before, with no
    /// deal check and no stop line.
    // Serializes the process-global pool fixture for the whole async scenario.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn closing_handover_policy_has_no_keep_open_deal_check() {
        let _env_lock = super::dexdo_pn_pool_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (captured, _log) = capture_errors();
        let fixture = kept_open_fixture(
            "buyer-2148-closing-unchanged",
            "no_handover_after_match",
            "fail_closed",
        );
        let shared = super::super::OnDemandKeptOpen::new();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            super::super::ON_DEMAND_KEPT_OPEN_WAIT.scope(
                shared.clone(),
                super::issue_2064_run_entrypoint(
                    true,
                    fixture.chain.clone(),
                    fixture.buyer.clone(),
                    fixture.args.clone(),
                ),
            ),
        )
        .await
        .expect("a closing answer ends the wait")
        .expect_err("fail_closed ends the purchase");
        let message = format!("{error:#}");
        assert!(message.contains("action=fail_closed"), "{message}");
        assert!(!message.contains("keep_open"), "{message}");
        assert!(captured.lines_with("keep_open handover wait").is_empty());
        assert!(shared.stopped.borrow().is_none());
        assert!(shared.wait.lock().unwrap().is_none());
        assert_no_chain_write_but_the_buy(&fixture.chain, "fail_closed");
        let _ = std::fs::remove_file(&fixture.lock.journal_path);
    }

    /// The on-demand runner stops serving and exits with the stop line once a kept-open wait
    /// reports its deal closed.
    #[test]
    fn on_demand_runner_stops_when_a_kept_open_deal_closes() {
        let source = include_str!("../buyer.rs");
        let start = source
            .find("async fn run_buyer_on_demand_local_api(")
            .unwrap();
        let body = &source[start..start + source[start..].find("\n}\n").unwrap()];
        let watch = body
            .find("_ = stopped.wait_for(Option::is_some) => {}")
            .unwrap();
        let serve = body
            .find("api::serve(bind, state, args.anthropic_compat, shutdown)")
            .unwrap();
        let exit = body
            .find("if let Some(reason) = kept_open_stopped.borrow_and_update().clone() {")
            .unwrap();
        assert!(watch < serve && serve < exit);
    }

    /// This module's own buyer note. The fixture note of the other buyer tests carries a money lock
    /// that a run those tests leave behind may still hold; a note of its own keeps these tests
    /// independent of them.
    fn keep_open_test_note() -> String {
        let note = format!("0:{}", "2148".repeat(16));
        assert_ne!(note, super::buyer_submit_test_journal().note_addr);
        note
    }
}
