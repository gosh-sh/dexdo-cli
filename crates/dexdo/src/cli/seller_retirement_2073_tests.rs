mod retirement_2073 {
    use super::*;

    struct Fixture {
        root: tempfile::TempDir,
        note: String,
        deals: Vec<SellerPoolDeal>,
        chains: std::collections::HashMap<String, Arc<PoolTestBackend>>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let note = format!("0:{}", "a".repeat(64));
            let mut fixture = Self {
                root,
                note,
                deals: Vec::new(),
                chains: Default::default(),
            };
            for (digit, ticks, funded, price) in [
                ('1', 100, true, 10),
                ('2', 98, false, 10),
                ('3', 100, false, 60),
            ] {
                let tc = format!("0:{}", digit.to_string().repeat(64));
                let mut backend = PoolTestBackend::new(
                    Arc::new(Mutex::new(Vec::new())),
                    tc.clone(),
                    ticks,
                    2,
                    funded,
                    1,
                );
                backend.price_per_tick = price * dexdo_core::PRICE_STEP as u64;
                let chain = Arc::new(backend);
                let market = dexdo_core::MarketManifest {
                    network: "mock".into(),
                    frame_model: "model-x".into(),
                    model_hash: dexdo_core::model_hash_for("model-x"),
                    inference_order_book: format!("0:{}", "b".repeat(64)),
                    root_model: format!("0:{}", "c".repeat(64)),
                    token_contract: tc.clone(),
                    seller_note: fixture.note.clone(),
                    nonce: digit.to_digit(10).unwrap() as u64,
                    price_per_tick: u128::from(chain.price_per_tick),
                    max_ticks: u128::from(ticks),
                };
                let deal = SellerPoolDeal {
                    chain: chain.clone(),
                    cfg: SellerConfig {
                        token_contract: tc.clone(),
                        price_per_tick: chain.price_per_tick,
                        max_ticks: ticks,
                        subscription: false,
                        gateway_advertise: "127.0.0.1:9000".into(),
                        mock_token_count: 2,
                    },
                    watch: dexdo::seller::SellerMatchWatchConfig {
                        cursor_path: seller_watch_cursor_path(Some(fixture.root.path()), &tc)
                            .unwrap(),
                        poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
                    },
                    upstream: dexdo::seller::UpstreamConfig::Mock,
                    nonce: market.nonce,
                    market: Some(market),
                };
                save_pool_deal_handle(&fixture.context(), &deal).unwrap();
                fixture.chains.insert(deals::normalize_addr(&tc), chain);
                fixture.deals.push(deal);
            }
            fixture
        }

        fn context(&self) -> SellerPoolContext<'_> {
            SellerPoolContext {
                deals_dir: Some(self.root.path()),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr: &self.note,
                frame_model: "model-x",
                gateway_advertise: "127.0.0.1:9000",
                advertise_probe: Default::default(),
            }
        }

        fn write_cursor(
            &self,
            index: usize,
            retired: bool,
            fill: Option<dexdo::seller::SellerFillLineage>,
        ) {
            let deal = &self.deals[index];
            std::fs::create_dir_all(deal.watch.cursor_path.parent().unwrap()).unwrap();
            std::fs::write(
                &deal.watch.cursor_path,
                serde_json::to_vec(&serde_json::json!({
                    "version": 1, "token_contract": deal.cfg.token_contract,
                    "source": dexdo_core::MatchWatchCursor::new(0), "last_polled_unix": null,
                    "opened_at_unix": null, "fill": fill, "unsold_retired": retired,
                }))
                .unwrap(),
            )
            .unwrap();
        }

        fn fill(&self) -> dexdo::seller::SellerFillLineage {
            dexdo::seller::SellerFillLineage {
                order_id: 5,
                offered_ticks: 100,
                matched_ticks: 2,
                residual_ticks: 98,
                price_per_tick: self.deals[0].cfg.price_per_tick,
                replacement_nonce: Some(2),
                replacement_token_contract: Some(self.deals[1].cfg.token_contract.clone()),
            }
        }

        async fn load(&self) -> Result<Vec<SellerPoolDeal>> {
            load_seller_pool_deals(&self.context(), self.deals[2].clone(), 2, |market| {
                Ok((
                    self.chains[&deals::normalize_addr(&market.token_contract)].clone(),
                    dexdo::seller::UpstreamConfig::Mock,
                ))
            })
            .await
        }

        async fn retire(&self) -> Result<()> {
            retire_superseded_seller_offers(
                Some(self.root.path()),
                self.deals[0].market.as_ref().unwrap(),
                self.deals[2].market.as_ref().unwrap(),
                self.context().contracts,
                |market| Ok(self.chains[&deals::normalize_addr(&market.token_contract)].clone()),
            )
            .await
        }

        async fn replace_current_with_old_target(&self) -> Result<()> {
            retire_superseded_seller_offers(
                Some(self.root.path()),
                self.deals[2].market.as_ref().unwrap(),
                self.deals[1].market.as_ref().unwrap(),
                self.context().contracts,
                |market| Ok(self.chains[&deals::normalize_addr(&market.token_contract)].clone()),
            )
            .await
        }

        fn current_offer_and_manifest(&self) -> (std::path::PathBuf, Vec<u8>, Vec<u8>) {
            let current = &self.deals[2];
            let chain = &self.chains[&deals::normalize_addr(&current.cfg.token_contract)];
            chain.rests.store(true, Ordering::Relaxed);
            chain.never_expires.store(true, Ordering::Relaxed);
            chain.cancel_confirms.store(true, Ordering::Relaxed);
            self.write_cursor(2, false, None);
            let path = self.root.path().join("market.json");
            std::fs::write(&path, current.market.as_ref().unwrap().to_json().unwrap()).unwrap();
            let manifest = std::fs::read(&path).unwrap();
            let cursor = std::fs::read(&current.watch.cursor_path).unwrap();
            (path, manifest, cursor)
        }

        async fn assert_current_offer_unchanged(
            &self,
            path: &std::path::Path,
            manifest: &[u8],
            cursor: &[u8],
        ) {
            let current = &self.deals[2];
            let chain = &self.chains[&deals::normalize_addr(&current.cfg.token_contract)];
            assert!(!chain.cancel_submitted.load(Ordering::Relaxed));
            assert_eq!(chain.post_calls.load(Ordering::Relaxed), 0);
            assert_eq!(
                chain
                    .raw_resting_sell_orders_for_tc(&current.cfg.token_contract)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(std::fs::read(path).unwrap(), manifest);
            assert_eq!(std::fs::read(&current.watch.cursor_path).unwrap(), cursor);
            assert!(!self.retired(2));
        }

        fn rest_child(&self, confirms: bool) {
            let chain = &self.chains[&deals::normalize_addr(&self.deals[1].cfg.token_contract)];
            chain.rests.store(true, Ordering::Relaxed);
            chain.never_expires.store(true, Ordering::Relaxed);
            chain.cancel_confirms.store(confirms, Ordering::Relaxed);
        }

        fn retired(&self, index: usize) -> bool {
            let deal = &self.deals[index];
            dexdo::seller::seller_unsold_is_retired(
                &deal.watch.cursor_path,
                &deal.cfg.token_contract,
            )
            .unwrap()
        }
    }

    #[tokio::test]
    async fn issue_2073_restart_does_not_restore_retired_residual() {
        let fixture = Fixture::new();
        let fill = fixture.fill();
        fixture.write_cursor(0, true, Some(fill.clone()));
        fixture.write_cursor(1, true, None);
        for _ in 0..2 {
            let loaded = fixture.load().await.unwrap();
            let contracts = loaded
                .iter()
                .map(|deal| &deal.cfg.token_contract)
                .collect::<Vec<_>>();
            assert_eq!(contracts.len(), 2);
            assert!(
                contracts.contains(&&fixture.deals[0].cfg.token_contract),
                "purchased parent must remain serviceable"
            );
            assert!(
                !contracts.contains(&&fixture.deals[1].cfg.token_contract),
                "retired 98 ticks must not be reposted"
            );
            assert_eq!(
                dexdo::seller::read_seller_fill_lineage(
                    &fixture.deals[0].watch.cursor_path,
                    &fixture.deals[0].cfg.token_contract
                )
                .unwrap(),
                Some(fill.clone())
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_retired_target_refuses_before_cancelling_the_current_offer() {
        let fixture = Fixture::new();
        fixture.write_cursor(1, true, None);
        let (path, manifest, cursor) = fixture.current_offer_and_manifest();
        let error = fixture.replace_current_with_old_target().await.unwrap_err();
        assert!(error.to_string().contains("retired"), "{error:#}");
        assert!(error.to_string().contains("--nonce auto"), "{error:#}");
        fixture
            .assert_current_offer_unchanged(&path, &manifest, &cursor)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_used_inactive_target_refuses_before_cancelling_the_current_offer() {
        let fixture = Fixture::new();
        let old = &fixture.chains[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)];
        old.exists.store(false, Ordering::Relaxed);
        old.getdeal_gone.store(true, Ordering::Relaxed);
        old.retired_nonces
            .lock()
            .unwrap()
            .insert(fixture.deals[1].nonce);
        let (path, manifest, cursor) = fixture.current_offer_and_manifest();
        let error = fixture.replace_current_with_old_target().await.unwrap_err();
        assert!(error.to_string().contains("already used"), "{error:#}");
        assert!(error.to_string().contains("--nonce auto"), "{error:#}");
        fixture
            .assert_current_offer_unchanged(&path, &manifest, &cursor)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_target_history_read_failure_preserves_the_current_offer() {
        let fixture = Fixture::new();
        let old = &fixture.chains[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)];
        old.exists.store(false, Ordering::Relaxed);
        old.getdeal_gone.store(true, Ordering::Relaxed);
        old.nonce_read_failure.store(true, Ordering::Relaxed);
        let (path, manifest, cursor) = fixture.current_offer_and_manifest();
        let error = fixture.replace_current_with_old_target().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("nonce history read failure"),
            "{error:#}"
        );
        fixture
            .assert_current_offer_unchanged(&path, &manifest, &cursor)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_fresh_target_without_history_remains_eligible() {
        let fixture = Fixture::new();
        let fresh = &fixture.chains[&deals::normalize_addr(&fixture.deals[2].cfg.token_contract)];
        fresh.exists.store(false, Ordering::Relaxed);
        fresh.getdeal_gone.store(true, Ordering::Relaxed);
        fixture.rest_child(true);
        fixture.retire().await.unwrap();
        assert!(fixture.retired(0));
        assert!(fixture.retired(1));
        assert!(!fixture.retired(2));
        assert_eq!(fresh.post_calls.load(Ordering::Relaxed), 0);
        assert!(!fresh.cancel_submitted.load(Ordering::Relaxed));
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_replace_retires_the_98_unsold_ticks_and_keeps_the_two_purchased_ticks() {
        let fixture = Fixture::new();
        let fill = fixture.fill();
        fixture.write_cursor(0, false, Some(fill.clone()));
        fixture.rest_child(true);
        fixture.retire().await.unwrap();
        assert!(fixture.retired(0));
        assert!(fixture.retired(1));
        assert!(!fixture.retired(2));
        fixture.retire().await.unwrap();
        let loaded = fixture.load().await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded[0].cfg.price_per_tick,
            60 * dexdo_core::PRICE_STEP as u64
        );
        assert_eq!(
            dexdo::seller::read_seller_fill_lineage(
                &fixture.deals[0].watch.cursor_path,
                &fixture.deals[0].cfg.token_contract
            )
            .unwrap(),
            Some(fill)
        );
        assert!(
            fixture.chains[&deals::normalize_addr(&fixture.deals[0].cfg.token_contract)]
                .matched
                .load(Ordering::Relaxed)
        );
        assert!(
            fixture.chains[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)]
                .cancel_submitted
                .load(Ordering::Relaxed)
        );
        let capacity = seller_deal_capacity(Some(fixture.root.path()), &fixture.note, |market| {
            Ok(fixture.chains[&deals::normalize_addr(&market.token_contract)].clone())
        })
        .await
        .unwrap();
        assert_eq!(
            capacity.total(),
            2,
            "retired unsold contract must not occupy a new provision slot"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_archived_child_handle_does_not_hide_its_linked_live_offer() {
        let fixture = Fixture::new();
        fixture.write_cursor(0, false, Some(fixture.fill()));
        fixture.rest_child(true);
        let handle = deals::make_handle_id(
            &fixture.deals[1].cfg.token_contract,
            deals::DealHandleRole::Seller,
        );
        std::fs::remove_file(fixture.root.path().join(format!("{handle}.json"))).unwrap();
        fixture.retire().await.unwrap();
        assert!(fixture.retired(0));
        assert!(fixture.retired(1));
        assert_eq!(fixture.load().await.unwrap().len(), 2);
        let chain = &fixture.chains[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)];
        assert!(chain.cancel_submitted.load(Ordering::Relaxed));
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_unconfirmed_cancel_does_not_retire_or_discard_recovery() {
        let fixture = Fixture::new();
        fixture.write_cursor(0, false, Some(fixture.fill()));
        let before = std::fs::read(&fixture.deals[0].watch.cursor_path).unwrap();
        fixture.rest_child(false);
        let error = fixture.retire().await.unwrap_err();
        assert!(
            error.to_string().contains("could not confirm cancellation"),
            "{error:#}"
        );
        assert!(!fixture.retired(0));
        assert!(!fixture.retired(1));
        assert_eq!(
            std::fs::read(&fixture.deals[0].watch.cursor_path).unwrap(),
            before
        );
        assert_eq!(fixture.load().await.unwrap().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_read_failure_and_unresolved_deploy_abort_replacement() {
        for pending in [false, true] {
            let fixture = Fixture::new();
            let child =
                &fixture.chains[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)];
            if pending {
                child.getdeal_gone.store(true, Ordering::Relaxed);
                let handle = deals::make_handle_id(
                    &fixture.deals[1].cfg.token_contract,
                    deals::DealHandleRole::Seller,
                );
                deals::persist_provision_pending(
                    &fixture.root.path().join(format!("{handle}.json")),
                    true,
                )
                .unwrap();
            } else {
                child.terms_fail_once.store(true, Ordering::Relaxed);
            }
            fixture.retire().await.unwrap_err();
            assert!(!fixture.retired(0));
            assert!(!fixture.retired(1));
            assert!(deals::list_deal_handles(fixture.root.path()).unwrap().len() == 3);
            if pending {
                let records =
                    seller_market_handle_records(Some(fixture.root.path()), &fixture.note).unwrap();
                assert!(
                    records[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)]
                        .provision_pending
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_match_wins_cancel_race_and_remains_recoverable() {
        let fixture = Fixture::new();
        fixture.rest_child(true);
        let child = &fixture.chains[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)];
        child.match_during_cancel.store(true, Ordering::Relaxed);
        let error = fixture.retire().await.unwrap_err();
        assert!(error.to_string().contains("buyer matched"), "{error:#}");
        assert!(child.matched.load(Ordering::Relaxed));
        assert!(!fixture.retired(1));
        let recovered = fixture.load().await.unwrap();
        assert!(recovered
            .iter()
            .any(|deal| deal.cfg.token_contract == child.token_contract));
    }

    #[tokio::test]
    async fn issue_2073_funded_recovery_is_preserved_even_when_open_validation_would_fail() {
        let fixture = Fixture::new();
        let parent = &fixture.chains[&deals::normalize_addr(&fixture.deals[0].cfg.token_contract)];
        parent.opened.store(true, Ordering::Relaxed);
        parent.inspection_fail_once.store(true, Ordering::Relaxed);
        fixture.retire().await.unwrap();
        assert!(fixture.retired(0));
        assert!(
            parent.inspection_fail_once.load(Ordering::Relaxed),
            "retirement must not validate a funded obligation as a fresh open"
        );
    }

    #[tokio::test]
    async fn issue_2073_post_boundary_refuses_a_retired_candidate() {
        let fixture = Fixture::new();
        fixture.write_cursor(1, true, None);
        let mut gateway = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let shutdown = futures::future::pending::<()>().fuse();
        tokio::pin!(shutdown);
        let result = prepare_pool_deal_reporting_identity(
            &gateway,
            &fixture.deals[1],
            &fixture.context(),
            false,
            shutdown.as_mut(),
            &mut false,
        )
        .await;
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("retired unsold capacity"));
        assert_eq!(
            fixture.chains[&deals::normalize_addr(&fixture.deals[1].cfg.token_contract)]
                .post_calls
                .load(Ordering::Relaxed),
            0
        );
        gateway.server_task.abort();
        let _ = (&mut gateway.server_task).await;
    }

    #[tokio::test]
    async fn issue_2073_malformed_retirement_cursor_fails_closed() {
        let fixture = Fixture::new();
        fixture.write_cursor(1, true, None);
        let path = &fixture.deals[1].watch.cursor_path;
        let mut cursor: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        cursor["unsold_retired"] = serde_json::json!("true");
        std::fs::write(path, serde_json::to_vec(&cursor).unwrap()).unwrap();
        let error = fixture
            .load()
            .await
            .err()
            .expect("invalid retirement state must not allow a repost");
        assert!(
            error.to_string().contains("parse seller watch cursor"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn issue_2073_archived_funded_child_is_retained_in_the_restart_pool() {
        let fixture = Fixture::new();
        fixture.write_cursor(0, false, Some(fixture.fill()));
        let parent_before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&fixture.deals[0].watch.cursor_path).unwrap())
                .unwrap();
        let child = &fixture.deals[1];
        let handle = deals::handle_path(
            fixture.root.path(),
            &deals::make_handle_id(&child.cfg.token_contract, deals::DealHandleRole::Seller),
        );
        std::fs::remove_file(&handle).unwrap();
        let chain = &fixture.chains[&deals::normalize_addr(&child.cfg.token_contract)];
        chain.matched.store(true, Ordering::Relaxed);
        fixture.retire().await.unwrap();
        for _ in 0..2 {
            let loaded = fixture.load().await.unwrap();
            let recovered = loaded
                .iter()
                .find(|deal| deal.cfg.token_contract == child.cfg.token_contract)
                .expect("the same funded child must remain in the service pool after restart");
            assert_eq!(recovered.market, child.market);
            assert_eq!(recovered.nonce, child.nonce);
            assert!(
                recovered
                    .chain
                    .deal_state(&child.cfg.token_contract)
                    .await
                    .unwrap()
                    .unwrap()
                    .funded
            );
        }
        assert!(
            handle.exists(),
            "funded descendant recovery must be durable"
        );
        assert_eq!(chain.post_calls.load(Ordering::Relaxed), 0);
        assert!(!chain.cancel_submitted.load(Ordering::Relaxed));
        let parent_after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&fixture.deals[0].watch.cursor_path).unwrap())
                .unwrap();
        for field in ["fill", "source", "opened_at_unix", "last_polled_unix"] {
            assert_eq!(parent_after[field], parent_before[field]);
        }
    }

    #[tokio::test]
    async fn issue_2073_failed_funded_handle_restore_does_not_retire_its_recovery_path() {
        let fixture = Fixture::new();
        fixture.write_cursor(0, false, Some(fixture.fill()));
        let before = std::fs::read(&fixture.deals[0].watch.cursor_path).unwrap();
        let child = &fixture.deals[1];
        let handle = deals::handle_path(
            fixture.root.path(),
            &deals::make_handle_id(&child.cfg.token_contract, deals::DealHandleRole::Seller),
        );
        std::fs::remove_file(&handle).unwrap();
        std::fs::create_dir(&handle).unwrap();
        fixture.chains[&deals::normalize_addr(&child.cfg.token_contract)]
            .matched
            .store(true, Ordering::Relaxed);
        let error = fixture.retire().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("read deal handle"),
            "{error:#}"
        );
        assert!(!fixture.retired(0));
        assert!(!fixture.retired(1));
        assert_eq!(
            std::fs::read(&fixture.deals[0].watch.cursor_path).unwrap(),
            before
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_retired_active_provision_has_no_pending_capacity() {
        let fixture = Fixture::new();
        let handle = deals::handle_path(
            fixture.root.path(),
            &deals::make_handle_id(
                &fixture.deals[1].cfg.token_contract,
                deals::DealHandleRole::Seller,
            ),
        );
        deals::persist_provision_pending(&handle, true).unwrap();
        fixture.rest_child(true);
        fixture.retire().await.unwrap();
        assert!(fixture.retired(1));
        let loaded = load_seller_pool_deals_with_scope(
            &fixture.context(),
            fixture.deals[2].clone(),
            2,
            None,
            |market| {
                Ok((
                    fixture.chains[&deals::normalize_addr(&market.token_contract)].clone(),
                    dexdo::seller::UpstreamConfig::Mock,
                ))
            },
        )
        .await
        .unwrap();
        assert_eq!(
            loaded.pending_provisions, 0,
            "retirement must not leave a phantom startup slot"
        );
        assert_eq!(
            seller_pool_occupied_capacity(
                Some(fixture.root.path()),
                &fixture.note,
                &Default::default(),
            )
            .await
            .unwrap(),
            0,
            "retirement must not block the residual queue through a pending handle"
        );
        assert!(!deals::load_deal_record(&handle).unwrap().provision_pending);
    }

    #[tokio::test(start_paused = true)]
    async fn issue_2073_linked_intended_replacement_is_never_cancelled_or_retired() {
        let fixture = Fixture::new();
        let fill = fixture.fill();
        fixture.write_cursor(0, false, Some(fill.clone()));
        fixture.rest_child(true);
        let target = &fixture.deals[1].cfg.token_contract;
        retire_superseded_seller_offers(
            Some(fixture.root.path()),
            fixture.deals[0].market.as_ref().unwrap(),
            fixture.deals[1].market.as_ref().unwrap(),
            fixture.context().contracts,
            |market| Ok(fixture.chains[&deals::normalize_addr(&market.token_contract)].clone()),
        )
        .await
        .unwrap();
        assert!(
            !fixture.retired(1),
            "lineage traversal must respect the intended replacement exclusion"
        );
        let child = &fixture.chains[&deals::normalize_addr(target)];
        assert!(!child.cancel_submitted.load(Ordering::Relaxed));
        assert_eq!(
            child
                .raw_resting_sell_orders_for_tc(target)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            dexdo::seller::read_seller_fill_lineage(
                &fixture.deals[0].watch.cursor_path,
                &fixture.deals[0].cfg.token_contract,
            )
            .unwrap(),
            Some(fill)
        );
    }

    proptest::proptest! {
        #[test]
        fn issue_2073_retirement_preserves_fill_and_replacement_history(offered in 2u64..10_000, matched_seed in 0u64..10_000) {
            let fixture = Fixture::new();
            let matched = 1 + matched_seed % (offered - 1);
            let fill = dexdo::seller::SellerFillLineage { offered_ticks: offered, matched_ticks: matched, residual_ticks: offered - matched, ..fixture.fill() };
            fixture.write_cursor(0, false, Some(fill.clone()));
            dexdo::seller::retire_seller_unsold(&fixture.deals[0].watch.cursor_path, &fixture.deals[0].cfg.token_contract).unwrap();
            proptest::prop_assert!(fixture.retired(0));
            proptest::prop_assert_eq!(dexdo::seller::read_seller_fill_lineage(&fixture.deals[0].watch.cursor_path, &fixture.deals[0].cfg.token_contract).unwrap(), Some(fill));
        }
    }
}
