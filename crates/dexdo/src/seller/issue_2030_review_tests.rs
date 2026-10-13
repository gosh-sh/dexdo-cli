#[tokio::test(start_paused = true)]
async fn issue_2030_review_confirmation_reconciles_late_open_without_duplicate() {
    let seller = test_seller();
    let cfg = test_cfg("tc-confirm-late");
    let mut backend = PollBackend::new(
        Some(sample_match(
            &cfg.token_contract,
            LocalNote::generate().pubkey(),
        )),
        None,
        1,
    )
    .with_open_failures(1);
    backend.open_unconfirmed = true;
    backend.failed_open_state = Some(true);
    backend
        .reconcile_transport_failures
        .store(2, Ordering::Relaxed);
    serve_match(&seller, &backend, &cfg).await.unwrap();
    assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
    assert_eq!(
        backend.reconcile_transport_failures.load(Ordering::Relaxed),
        0
    );
    seller.server_task.abort();
}

#[tokio::test(start_paused = true)]
async fn issue_2030_review_confirmation_retries_only_funded_unopened_and_stops_on_cleanup() {
    for cleaned_up in [false, true] {
        let seller = test_seller();
        let cfg = test_cfg("tc-confirm-pending");
        let mut backend = PollBackend::new(
            Some(sample_match(
                &cfg.token_contract,
                LocalNote::generate().pubkey(),
            )),
            None,
            1,
        )
        .with_open_failures(1);
        backend.open_unconfirmed = true;
        if cleaned_up {
            backend.failed_open_state = Some(false);
        }
        let result = serve_match(&seller, &backend, &cfg).await;
        assert_eq!(result.is_err(), cleaned_up);
        assert_eq!(
            backend.opens.load(Ordering::Relaxed),
            if cleaned_up { 1 } else { 2 }
        );
        seller.server_task.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn issue_2030_review_known_match_ends_on_cleanup_without_guessed_fill() {
    for stopped_account_present in [false, true] {
        let (_dir, cursor_path) = temp_cursor_path("known-match-cleanup");
        let seller = test_seller();
        let cfg = test_cfg("tc-known-cleanup");
        let matched = sample_match(&cfg.token_contract, LocalNote::generate().pubkey());
        let mut backend = PollBackend::new(Some(matched.clone()), None, 1);
        backend.omit_fill_history = true;
        backend.cleanup_after_history = true;
        if stopped_account_present {
            let mut terminal = backend
                .deal_state(&cfg.token_contract)
                .await
                .unwrap()
                .unwrap();
            terminal.deposit = 0;
            terminal.probe_tick = 0;
            backend.cleanup_state = Some(terminal);
        }
        let watch = SellerMatchWatchConfig {
            cursor_path: cursor_path.clone(),
            poll_interval: dexdo_core::params::DEFAULT_MATCH_POLL_INTERVAL,
        };
        let error = tokio::time::timeout(
            watch.poll_interval * 3,
            serve_watched_match(&seller, &backend, &cfg, &watch, matched),
        )
        .await
        .expect("known-match recovery must finish after authoritative cleanup")
        .unwrap_err();
        assert!(error.to_string().contains("capacity remains unresolved"));
        assert_eq!(backend.opens.load(Ordering::Relaxed), 0);
        let cursor =
            SellerMatchWatchCursor::load_or_new(&cursor_path, &cfg.token_contract).unwrap();
        assert!(cursor.fill.is_none());
        assert!(cursor.opened_at_unix.is_none());
        seller.server_task.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn issue_2030_review_ordinary_unmatched_offer_keeps_waiting() {
    let (_dir, cursor_path) = temp_cursor_path("ordinary-unmatched");
    let seller = test_seller();
    let cfg = test_cfg("tc-unmatched");
    let mut backend = PollBackend::new(None, None, 1);
    backend.omit_fill_history = true;
    backend.cleaned_up.store(true, Ordering::Relaxed);
    let watch = SellerMatchWatchConfig {
        cursor_path,
        poll_interval: dexdo_core::params::DEFAULT_MATCH_POLL_INTERVAL,
    };
    assert!(tokio::time::timeout(
        watch.poll_interval * 2,
        wait_for_match(&seller, &backend, &cfg, &watch)
    )
    .await
    .is_err());
    assert_eq!(backend.opens.load(Ordering::Relaxed), 0);
    seller.server_task.abort();
}
