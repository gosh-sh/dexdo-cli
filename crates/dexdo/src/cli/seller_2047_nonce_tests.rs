async fn issue_2047_relist(retired: &[u64], saved: Option<u64>) {
    let pool = issue_1057_pool(3).await;
    pool.parent.retired_nonces.lock().unwrap().extend(retired);
    if let Some(nonce) = saved {
        dexdo::seller::persist_seller_replacement(
            &pool.deal.watch.cursor_path,
            &pool.parent.token_contract,
            nonce,
            None,
        )
        .unwrap();
    }
    let expected_nonce = retired.iter().copied().max().unwrap() + 1;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut provision = {
        let calls = calls.clone();
        let successor = pool.successor.clone();
        let note_addr = pool.note_addr.clone();
        let cursor = pool.deal.watch.cursor_path.clone();
        let parent_tc = pool.parent.token_contract.clone();
        move |model: String, nonce: u64, price: u64, ticks: u64| {
            // The chosen, usable identity must survive a crash before this money call.
            let fill = dexdo::seller::read_seller_fill_lineage(&cursor, &parent_tc)
                .unwrap()
                .unwrap();
            assert_eq!(fill.replacement_nonce, Some(nonce));
            calls.lock().unwrap().push((nonce, price, ticks));
            assert_eq!(
                nonce, expected_nonce,
                "must not provision a destroyed address"
            );
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
    let shutdown = async {
        while pool.successor.post_calls.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    }
    .fuse();
    tokio::pin!(shutdown);
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_seller_pool(
            &pool.seller,
            vec![pool.deal.clone()],
            pool.context(),
            &pool_test_policy(2),
            &mut provision,
            shutdown.as_mut(),
            &mut false,
        ),
    )
    .await
    .expect("residual must recover without manual state edits")
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(expected_nonce, pool.parent.price_per_tick, 5)]
    );
    let after = dexdo::seller::read_seller_fill_lineage(
        &pool.deal.watch.cursor_path,
        &pool.parent.token_contract,
    )
    .unwrap()
    .unwrap();
    assert_eq!(after.replacement_nonce, Some(expected_nonce));
    assert_eq!(
        after.replacement_token_contract.as_deref(),
        Some(pool.successor.token_contract.as_str())
    );
    pool.seller.server_task.abort();
}

#[tokio::test]
async fn issue_2047_new_residual_skips_destroyed_nonces_before_provision() {
    issue_2047_relist(&[11, 12, 13], None).await;
}

#[tokio::test]
async fn issue_2047_restart_replaces_saved_destroyed_nonce() {
    issue_2047_relist(&[4, 5], Some(4)).await;
}

#[tokio::test]
async fn issue_2047_nonce_read_failure_preserves_cursor_and_spends_nothing() {
    for saved in [None, Some(4)] {
        let pool = issue_1057_pool(3).await;
        if let Some(nonce) = saved {
            dexdo::seller::persist_seller_replacement(
                &pool.deal.watch.cursor_path,
                &pool.parent.token_contract,
                nonce,
                None,
            )
            .unwrap();
        }
        pool.parent
            .nonce_read_failure
            .store(true, Ordering::Relaxed);
        let before = dexdo::seller::read_seller_fill_lineage(
            &pool.deal.watch.cursor_path,
            &pool.parent.token_contract,
        )
        .unwrap()
        .unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let mut provision = {
            let calls = calls.clone();
            move |_: String, _: u64, _: u64, _: u64| {
                calls.fetch_add(1, Ordering::Relaxed);
                futures::future::ready(Err(anyhow::anyhow!("must not reach money provisioner")))
            }
        };
        let shutdown = async {
            while pool.parent.nonce_read_calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        }
        .fuse();
        tokio::pin!(shutdown);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_seller_pool(
                &pool.seller,
                vec![pool.deal.clone()],
                pool.context(),
                &pool_test_policy(2),
                &mut provision,
                shutdown.as_mut(),
                &mut false,
            ),
        )
        .await
        .unwrap()
        .expect_err("a history read failure must remain an error");
        assert!(
            format!("{error:#}").contains("nonce history read failure"),
            "{error:#}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let after = dexdo::seller::read_seller_fill_lineage(
            &pool.deal.watch.cursor_path,
            &pool.parent.token_contract,
        )
        .unwrap()
        .unwrap();
        assert_eq!(after.replacement_nonce, saved);
        // Pool startup may advance the watch cursor; selection cannot change its fill lineage.
        assert_eq!(after, before);
        pool.seller.server_task.abort();
    }
}

#[tokio::test]
async fn issue_2047_shutdown_after_history_read_spends_nothing_and_does_not_reserve() {
    let pool = issue_1057_pool(3).await;
    let calls = Arc::new(AtomicU64::new(0));
    let mut provision = {
        let calls = calls.clone();
        move |_: String, _: u64, _: u64, _: u64| {
            calls.fetch_add(1, Ordering::Relaxed);
            futures::future::ready(Err(anyhow::anyhow!("must not provision after shutdown")))
        }
    };
    let shutdown = async {
        while pool.parent.nonce_read_calls.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    }
    .fuse();
    tokio::pin!(shutdown);
    let mut shutdown_requested = false;
    tokio::time::timeout(
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
    .unwrap()
    .expect("shutdown after nonce selection must use the normal stop path");
    assert!(shutdown_requested);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let fill = dexdo::seller::read_seller_fill_lineage(
        &pool.deal.watch.cursor_path,
        &pool.parent.token_contract,
    )
    .unwrap()
    .unwrap();
    assert_eq!(fill.replacement_nonce, None);
    assert_eq!(fill.replacement_token_contract, None);
    pool.seller.server_task.abort();
}

#[tokio::test]
async fn issue_2047_linked_and_pending_successors_keep_their_exact_nonce() {
    for pending in [false, true] {
        let pool = issue_1057_pool(3).await;
        pool.parent.retired_nonces.lock().unwrap().insert(4);
        pool.parent
            .nonce_read_failure
            .store(true, Ordering::Relaxed);
        let market = issue_1057_market(
            pool.frame_model.to_string(),
            4,
            pool.parent.price_per_tick,
            5,
            &pool.note_addr,
            pool.successor.clone(),
        )
        .0;
        if pending {
            persist_provision_deal_handle(
                Some(pool.root.path()),
                &market,
                pool.context().contracts,
                true,
            )
            .unwrap();
        }
        let fill = dexdo::seller::persist_seller_replacement(
            &pool.deal.watch.cursor_path,
            &pool.parent.token_contract,
            4,
            (!pending).then_some(pool.successor.token_contract.as_str()),
        )
        .unwrap();
        assert_eq!(
            select_seller_replacement_nonce(
                &pool.deal,
                &fill,
                &std::collections::HashMap::new(),
                &pool.context(),
            )
            .await
            .unwrap(),
            4,
            "a linked or possibly submitted successor must not be replaced"
        );
        assert!(
            dexdo::seller::replace_unlinked_seller_replacement_nonce(
                &pool.deal.watch.cursor_path,
                &pool.parent.token_contract,
                99,
                5,
            )
            .is_err(),
            "a stale nonce reservation must not be overwritten"
        );
        if !pending {
            assert!(
                dexdo::seller::replace_unlinked_seller_replacement_nonce(
                    &pool.deal.watch.cursor_path,
                    &pool.parent.token_contract,
                    4,
                    5,
                )
                .is_err(),
                "a linked successor must not be overwritten"
            );
        }
        assert_eq!(
            dexdo::seller::read_seller_fill_lineage(
                &pool.deal.watch.cursor_path,
                &pool.parent.token_contract,
            )
            .unwrap()
            .unwrap(),
            fill
        );
        pool.seller.server_task.abort();
    }
}

#[tokio::test]
async fn issue_2047_nonce_overflow_does_not_change_saved_state() {
    let pool = issue_1057_pool(3).await;
    pool.parent.retired_nonces.lock().unwrap().insert(u64::MAX);
    let fill = dexdo::seller::persist_seller_replacement(
        &pool.deal.watch.cursor_path,
        &pool.parent.token_contract,
        u64::MAX,
        None,
    )
    .unwrap();
    let error = select_seller_replacement_nonce(
        &pool.deal,
        &fill,
        &std::collections::HashMap::new(),
        &pool.context(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("nonce overflow"));
    assert_eq!(
        dexdo::seller::read_seller_fill_lineage(
            &pool.deal.watch.cursor_path,
            &pool.parent.token_contract,
        )
        .unwrap()
        .unwrap(),
        fill
    );
    pool.seller.server_task.abort();
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]
    #[test]
    fn issue_2047_selection_never_uses_known_or_retired_nonces(retired_count in 1u64..32, known_count in 0u64..16) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let pool = issue_1057_pool(3).await;
            let start = pool.deal.nonce + 1;
            pool.parent.retired_nonces.lock().unwrap().extend(start..start + retired_count);
            let known = (start + retired_count..start + retired_count + known_count)
                .map(|nonce| (nonce, pool.deal.clone())).collect();
            let fill = dexdo::seller::read_seller_fill_lineage(
                &pool.deal.watch.cursor_path, &pool.parent.token_contract,
            ).unwrap().unwrap();
            let next = select_seller_replacement_nonce(&pool.deal, &fill, &known, &pool.context()).await.unwrap();
            assert_eq!(next, start + retired_count + known_count);
            pool.seller.server_task.abort();
        });
    }
}
