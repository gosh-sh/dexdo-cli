async fn issue_2115_refusal_restart_case(queue: bool) {
    let (seller, state, server, directory, _book, endpoint) = issue_2115_resting_backend().await;
    let tc = issue_2115_address('c');
    let path = directory.path().join(format!("{}.sell.json", tc.bare()));
    std::fs::remove_file(&path).unwrap();
    {
        let mut state = state.lock().unwrap();
        state.order = None;
        state.reject_posts = !queue;
        if queue {
            state.post_replies = (0..=crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL)
                .map(|_| (200, json!({"error":{"code":"QUEUE_OVERFLOW","message":"message queue is full"}}))).collect();
        }
    }
    let note = crate::note::LocalNote::generate();
    let offer = || crate::market::SellOffer {
        token_contract: tc.with_workchain(),
        price_per_tick: PRICE_STEP.try_into().unwrap(),
        max_ticks: 100,
        flags: 0,
    };
    let first = seller.post_offer(offer(), &note).await.unwrap_err();
    eprintln!(
        "conclusive first result={first:?}; rejected_posts={}; journal_retained={}",
        state.lock().unwrap().rejected_hashes.len(),
        path.exists()
    );
    assert_eq!(
        state.lock().unwrap().rejected_hashes.len(),
        if queue {
            crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL as usize + 1
        } else {
            1
        }
    );
    assert!(
        !path.exists(),
        "entirely refused submissions have no executable root"
    );
    assert!(!seller
        .has_pending_sell_offer(&tc.with_workchain())
        .await
        .unwrap());
    assert!(state.lock().unwrap().posts.is_empty());
    state.lock().unwrap().reject_posts = false;
    drop(seller);
    let resumed = super::super::backends::RealSellerBackend::new(
        issue_2030_backend(&endpoint),
        issue_2115_address('a'),
        KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        model_hash_for("fixture--model--v1"),
        "fixture--model--v1".into(),
        1,
        TICK_SIZE,
    )
    .with_offer_journal_dir(directory.path().to_path_buf());
    let second = resumed.post_offer(offer(), &note).await;
    let successful_posts = state.lock().unwrap().posts.len();
    server.abort();
    eprintln!("healthy restart={second:?}; successful_posts={successful_posts}");
    second.expect("a conclusively refused, never-enqueued SELL must not require an impossible on-chain root before a healthy retry");
    assert_eq!(successful_posts, 1);
}

#[tokio::test]
async fn issue_2115_terminal_checkpoint_retries_after_transient_save_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(
        listener,
        vec![
            (200, issue_2115_placement_event()),
            (200, issue_2115_placement_event()),
        ],
    );
    let chain = issue_2030_backend(&endpoint);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let mut value = serde_json::to_value(issue_2115_post()).unwrap();
    value["pending"] = json!([["5".repeat(64), issue_2115_address('b').with_workchain()]]);
    value["seen"] = json!(["1".repeat(64), "2".repeat(64), "3".repeat(64)]);
    let mut post: SellerPlacement = serde_json::from_value(value).unwrap();
    post.save(Some(&path)).unwrap();
    let blocked_tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::create_dir(&blocked_tmp).unwrap();
    assert!(chain
        .reconcile_seller_placement(&mut post, Some(&path))
        .await
        .is_err());
    assert_eq!(
        SellerPlacement::load(&path)
            .unwrap()
            .unwrap()
            .accepted_order_id,
        None
    );
    std::fs::remove_dir(&blocked_tmp).unwrap();
    chain
        .reconcile_seller_placement(&mut post, Some(&path))
        .await
        .unwrap();
    let restored = SellerPlacement::load(&path).unwrap().unwrap();
    server.abort();
    eprintln!(
        "memory accepted={:?}; after FS recovery disk accepted={:?}; trace_reads={}",
        post.accepted_order_id,
        restored.accepted_order_id,
        requests.lock().unwrap().len()
    );
    assert_eq!(
        restored.accepted_order_id,
        Some(102),
        "terminal-in-memory must not permanently suppress retrying a failed durable checkpoint"
    );
}

#[tokio::test]
async fn issue_2115_conclusive_post_refusal_must_allow_healthy_restart() {
    issue_2115_refusal_restart_case(false).await;
}

#[tokio::test]
async fn issue_2115_exhausted_queue_refusals_allow_one_healthy_restart() {
    issue_2115_refusal_restart_case(true).await;
}

#[tokio::test]
async fn issue_2115_ambiguous_attempt_followed_by_refusal_preserves_identity() {
    for queue in [false, true] {
        let (seller, state, server, directory, _book, endpoint) =
            issue_2115_resting_backend().await;
        let tc = issue_2115_address('c');
        let path = directory.path().join(format!("{}.sell.json", tc.bare()));
        std::fs::remove_file(&path).unwrap();
        {
            let mut state = state.lock().unwrap();
            state.order = None;
            state
                .post_replies
                .push_back((503, json!({"error":"response lost after forwarding"})));
            for _ in 0..if queue {
                crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL
            } else {
                1
            } {
                state.post_replies.push_back((200, json!({"error":{"code":if queue { "QUEUE_OVERFLOW" } else { "INVALID_ARGUMENT" }, "message":"refused retry"}})));
            }
        }
        let note = crate::note::LocalNote::generate();
        let offer = || crate::market::SellOffer {
            token_contract: tc.with_workchain(),
            price_per_tick: PRICE_STEP.try_into().unwrap(),
            max_ticks: 100,
            flags: 0,
        };
        let error = seller.post_offer(offer(), &note).await.unwrap_err();
        assert!(
            matches!(error, crate::market::ChainError::AmbiguousSubmit(_)),
            "{error:?}"
        );
        let before = SellerPlacement::load(&path).unwrap().unwrap();
        assert_eq!(
            state.lock().unwrap().rejected_hashes.len(),
            if queue {
                crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL as usize + 1
            } else {
                2
            }
        );
        assert_eq!(state.lock().unwrap().rejected_hashes[0], before.message_id);
        assert_eq!(state.lock().unwrap().rejected_hashes[1], before.message_id);
        state.lock().unwrap().post_replies.clear();
        drop(seller);
        let resumed = super::super::backends::RealSellerBackend::new(
            issue_2030_backend(&endpoint),
            issue_2115_address('a'),
            KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            model_hash_for("fixture--model--v1"),
            "fixture--model--v1".into(),
            1,
            TICK_SIZE,
        )
        .with_offer_journal_dir(directory.path().to_path_buf());
        assert!(resumed
            .has_pending_sell_offer(&tc.with_workchain())
            .await
            .unwrap());
        assert!(matches!(
            resumed.post_offer(offer(), &note).await.unwrap_err(),
            crate::market::ChainError::AmbiguousSubmit(_)
        ));
        assert!(state.lock().unwrap().posts.is_empty());
        assert_eq!(
            SellerPlacement::load(&path).unwrap().unwrap().message_id,
            before.message_id
        );
        server.abort();
    }
}

#[tokio::test]
async fn issue_2115_failed_terminal_save_is_retried_before_pruned_history() {
    for accepted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pending.sell.json");
        let blocked = path.with_extension(format!("tmp.{}", std::process::id()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = reads.clone();
        let blocked_at_save = blocked.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_fixture_http_request(&mut socket).await;
                let first = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                let reply = if first {
                    // Fail the checkpoint AFTER the receipt is decoded, not the entry save.
                    std::fs::create_dir(&blocked_at_save).unwrap();
                    if accepted {
                        issue_2115_placement_event()
                    } else {
                        issue_2115_message('1', None, Some('a'), &[])
                    }
                } else {
                    json!({"data":{"blockchain":{"message":null}}})
                };
                let body = reply.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let chain = issue_2030_backend(&endpoint);
        let mut value = serde_json::to_value(issue_2115_post()).unwrap();
        if accepted {
            value["pending"] = json!([["5".repeat(64), issue_2115_address('b').with_workchain()]]);
            value["seen"] = json!(["1".repeat(64), "2".repeat(64), "3".repeat(64)]);
        }
        let mut post: SellerPlacement = serde_json::from_value(value).unwrap();
        post.save(Some(&path)).unwrap();
        assert!(chain
            .reconcile_seller_placement(&mut post, Some(&path))
            .await
            .is_err());
        assert_eq!(post.accepted_order_id, accepted.then_some(102));
        assert!(SellerPlacement::load(&path)
            .unwrap()
            .unwrap()
            .terminal
            .is_none());
        std::fs::remove_dir(&blocked).unwrap();
        chain
            .reconcile_seller_placement(&mut post, Some(&path))
            .await
            .unwrap();
        let mut restarted = SellerPlacement::load(&path).unwrap().unwrap();
        assert_eq!(restarted.message_id, post.message_id);
        assert_eq!(restarted.accepted_order_id, accepted.then_some(102));
        assert!(restarted.terminal.is_some());
        chain
            .reconcile_seller_placement(&mut restarted, Some(&path))
            .await
            .unwrap();
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "retry and restart cannot require pruned processed receipts"
        );
        server.abort();
    }
}

#[tokio::test]
async fn issue_2115_forwarding_and_invalid_ack_keep_the_original_journal() {
    for invalid_json in [false, true] {
        let (seller, state, server, directory, _book, endpoint) =
            issue_2115_resting_backend().await;
        let tc = issue_2115_address('c');
        let path = directory.path().join(format!("{}.sell.json", tc.bare()));
        std::fs::remove_file(&path).unwrap();
        {
            let mut state = state.lock().unwrap();
            state.order = None;
            state.invalid_post_reply = invalid_json;
            if !invalid_json {
                state.post_replies.push_back((200, json!({"error":{"code":"INTERNAL_ERROR","message":"The message redirection to the Block Producer has failed: (source: operation timed out)"}})));
            }
        }
        let note = crate::note::LocalNote::generate();
        let offer = || crate::market::SellOffer {
            token_contract: tc.with_workchain(),
            price_per_tick: PRICE_STEP.try_into().unwrap(),
            max_ticks: 100,
            flags: 0,
        };
        assert!(matches!(
            seller.post_offer(offer(), &note).await.unwrap_err(),
            crate::market::ChainError::AmbiguousSubmit(_)
        ));
        let submitted = SellerPlacement::load(&path).unwrap().unwrap();
        assert!(state.lock().unwrap().posts.is_empty());
        state.lock().unwrap().invalid_post_reply = false;
        drop(seller);
        let resumed = super::super::backends::RealSellerBackend::new(
            issue_2030_backend(&endpoint),
            issue_2115_address('a'),
            KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            model_hash_for("fixture--model--v1"),
            "fixture--model--v1".into(),
            1,
            TICK_SIZE,
        )
        .with_offer_journal_dir(directory.path().to_path_buf());
        assert!(matches!(
            resumed.post_offer(offer(), &note).await.unwrap_err(),
            crate::market::ChainError::AmbiguousSubmit(_)
        ));
        assert_eq!(
            SellerPlacement::load(&path).unwrap().unwrap().message_id,
            submitted.message_id
        );
        assert!(state.lock().unwrap().posts.is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn issue_2115_restart_detects_pending_journal_without_chain_reads() {
    let (seller, _state, server, directory, _book, endpoint) = issue_2115_resting_backend().await;
    let tc = issue_2115_address('c');
    server.abort();
    assert!(seller
        .has_pending_sell_offer(&tc.with_workchain())
        .await
        .unwrap());
    drop(seller);
    let resumed = super::super::backends::RealSellerBackend::new(
        issue_2030_backend(&endpoint),
        issue_2115_address('a'),
        KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        model_hash_for("fixture--model--v1"),
        "fixture--model--v1".into(),
        1,
        TICK_SIZE,
    )
    .with_offer_journal_dir(directory.path().to_path_buf());
    assert!(resumed
        .has_pending_sell_offer(&tc.with_workchain())
        .await
        .unwrap());
    let path = directory.path().join(format!("{}.sell.json", tc.bare()));
    std::fs::write(&path, "broken journal").unwrap();
    assert!(resumed
        .has_pending_sell_offer(&tc.with_workchain())
        .await
        .is_err());
    std::fs::remove_file(&path).unwrap();
    assert!(!resumed
        .has_pending_sell_offer(&tc.with_workchain())
        .await
        .unwrap());
}
