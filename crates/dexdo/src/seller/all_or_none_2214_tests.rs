mod all_or_none_2214 {
    use super::*;

    /// a deal whose manifest says all-or-none posts an ordinary SELL with `AON` alone, at its
    /// full size, so the book matches it only against one buyer taking every tick.
    #[tokio::test]
    async fn all_or_none_deal_posts_aon_alone_at_full_size() {
        let owner = chain_address('1');
        let mut cfg = test_cfg(&chain_address('9'));
        cfg.all_or_none = true;
        let backend = StartupBackend::without_match(RawStartupRead::Orders(Vec::new()));
        prepare_seller_offer(&LocalNote::generate(), &backend, &cfg, Some(&owner))
            .await
            .expect("all-or-none SELL posts");
        let posted = backend
            .posted_offer
            .lock()
            .unwrap()
            .clone()
            .expect("all-or-none offer captured");
        assert_eq!(posted.flags, order_flags::AON);
        assert_eq!(posted.flags, 0x20);
        assert_eq!(posted.max_ticks, cfg.max_ticks);

        // A subscription is already all-or-none: the deal flag adds nothing to its shape.
        cfg.subscription = true;
        assert_eq!(sell_offer_flags(&cfg), SUBSCRIPTION_SELL_FLAGS);
    }

    /// a restarted seller resumes an all-or-none deal's resting SELL only when the book holds
    /// exactly `AON`; an order that a smaller buyer could split, or any other shape, is refused.
    #[tokio::test]
    async fn all_or_none_resume_requires_exact_aon_shape() {
        let tc = chain_address('4');
        let owner = chain_address('5');
        let mut cfg = test_cfg(&tc);
        cfg.all_or_none = true;

        let mut flagged = raw_sell(81, &owner, Some(&tc), 3_000_000_000, 8);
        flagged.flags = order_flags::AON;
        let matching = StartupBackend::without_match(RawStartupRead::Orders(vec![flagged]));
        let startup = prepare_seller_offer(&LocalNote::generate(), &matching, &cfg, Some(&owner))
            .await
            .expect("same-shape all-or-none SELL resumes");
        assert_eq!(startup, SellerOfferStartup::ResumedResting { order_id: 81 });
        assert_eq!(matching.post_calls.load(Ordering::Relaxed), 0);

        for (order_id, invalid_flags) in [
            (82, 0),
            (83, order_flags::AON | order_flags::SUBSCRIPTION),
            (84, order_flags::SUBSCRIPTION),
            (85, 0x80),
        ] {
            let mut invalid = raw_sell(order_id, &owner, Some(&tc), 3_000_000_000, 8);
            invalid.flags = invalid_flags;
            let backend = StartupBackend::without_match(RawStartupRead::Orders(vec![invalid]));
            assert_startup_rejected(
                &backend,
                &cfg,
                &owner,
                &format!("flags 0x{invalid_flags:02x} do not match required seller flags 0x20"),
            )
            .await;
        }
    }
}
