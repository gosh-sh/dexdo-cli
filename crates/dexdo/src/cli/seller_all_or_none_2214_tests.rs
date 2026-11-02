mod all_or_none_2214 {
    use super::*;

    /// One unsold deal of the note, recorded the way `provision` records it: its manifest, with
    /// the all-or-none choice, saved as a seller deal handle in the deals directory.
    fn recorded_deal(
        root: &std::path::Path,
        context: &SellerPoolContext<'_>,
        digit: char,
        all_or_none: bool,
    ) -> (SellerPoolDeal, Arc<PoolTestBackend>) {
        let tc = format!("0:{}", digit.to_string().repeat(64));
        let mut backend =
            PoolTestBackend::new(Arc::new(Mutex::new(Vec::new())), tc.clone(), 50, 50, false, 1);
        backend.price_per_tick = dexdo_core::PRICE_STEP as u64;
        let chain = Arc::new(backend);
        let market = dexdo_core::MarketManifest {
            network: "mock".into(),
            frame_model: "model-x".into(),
            model_hash: dexdo_core::model_hash_for("model-x"),
            inference_order_book: format!("0:{}", "b".repeat(64)),
            root_model: format!("0:{}", "c".repeat(64)),
            token_contract: tc.clone(),
            seller_note: context.note_addr.to_string(),
            nonce: digit.to_digit(10).unwrap() as u64,
            price_per_tick: u128::from(chain.price_per_tick),
            max_ticks: 50,
            all_or_none,
        };
        let deal = SellerPoolDeal {
            chain: chain.clone(),
            cfg: SellerConfig {
                token_contract: tc.clone(),
                price_per_tick: chain.price_per_tick,
                max_ticks: 50,
                subscription: false,
                all_or_none,
                gateway_advertise: context.gateway_advertise.into(),
                mock_token_count: 2,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: seller_watch_cursor_path(Some(root), &tc).unwrap(),
                poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
            },
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: market.nonce,
            market: Some(market),
        };
        save_pool_deal_handle(context, &deal).unwrap();
        (deal, chain)
    }

    /// the all-or-none choice belongs to each deal and lives in its record, not in a seller
    /// flag. One seller start loads every deal of its note, and each one comes back with exactly the
    /// choice its own record carries -- an all-or-none deal beside an ordinary one stays itself.
    #[tokio::test]
    async fn every_loaded_deal_takes_all_or_none_from_its_own_record() {
        let root = tempfile::tempdir().unwrap();
        let note = format!("0:{}", "a".repeat(64));
        let context = SellerPoolContext {
            deals_dir: Some(root.path()),
            contracts: std::path::Path::new("manifest/deployed.manifest.json"),
            note_addr: &note,
            frame_model: "model-x",
            gateway_advertise: "127.0.0.1:9000",
            advertise_probe: Default::default(),
        };
        let (whole, whole_chain) = recorded_deal(root.path(), &context, '1', true);
        let (ordinary, ordinary_chain) = recorded_deal(root.path(), &context, '2', false);
        let (_, other_whole_chain) = recorded_deal(root.path(), &context, '3', true);
        let chains = [whole_chain, ordinary_chain, other_whole_chain];

        // Started on the ordinary deal: the other two come from the deals directory alone.
        let loaded = load_seller_pool_deals(&context, ordinary.clone(), 2, |market| {
            let chain = chains
                .iter()
                .find(|chain| {
                    deals::normalize_addr(&chain.token_contract)
                        == deals::normalize_addr(&market.token_contract)
                })
                .expect("a recorded deal")
                .clone();
            Ok((chain as Arc<dyn ChainBackend>, dexdo::seller::UpstreamConfig::Mock))
        })
        .await
        .expect("all recorded deals load");

        let by_tc = loaded
            .iter()
            .map(|deal| {
                (
                    deals::normalize_addr(&deal.cfg.token_contract),
                    deal.cfg.all_or_none,
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(by_tc.len(), 3, "{by_tc:?}");
        assert!(by_tc[&deals::normalize_addr(&whole.cfg.token_contract)]);
        assert!(!by_tc[&deals::normalize_addr(&ordinary.cfg.token_contract)]);
        assert!(by_tc[&deals::normalize_addr(&format!("0:{}", "3".repeat(64)))]);
    }
}
