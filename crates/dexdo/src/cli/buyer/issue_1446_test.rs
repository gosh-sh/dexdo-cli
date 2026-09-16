use dexdo_core::Note as _;

fn issue_1446_waiting_deal_state(age_secs: u64) -> dexdo_core::DealChainState {
    let mut state = deal_state(true, false, false, false);
    let funded_time = super::unix_now_secs().saturating_sub(age_secs);
    state.funded_time = Some(funded_time);
    state.last_claim_time = funded_time;
    state
}

/// the production pre-open poll must tell a human what it is waiting for while the
/// seller still has a funded deal it may open.
#[tokio::test]
async fn issue_1446_funded_never_opened_wait_reports_state_elapsed_and_status_command() {
    let token_contract = format!("0:{}", "4".repeat(64));
    let chain = RecordingRecoveryChain::with_deal_state(issue_1446_waiting_deal_state(0));
    let buyer = dexdo::buyer::Buyer::from_note(std::sync::Arc::new(
        dexdo_core::LocalNote::generate(),
    ));
    let mut progress = super::BuyerHandoverProgress::for_test(
        true,
        std::time::Duration::from_secs(60),
    );

    let failure = buyer
        .resolve_endpoint(&chain, &token_contract)
        .await
        .expect_err("the seller has not written a handover");
    super::wait_for_seller_handover_poll(
        &chain,
        &token_contract,
        &mut progress,
        &failure,
    )
    .await;

    let reports = progress.reported_lines();
    assert_eq!(reports.len(), 1, "one real poll emits one initial status");
    let report = &reports[0];
    for token in [
        "waiting for a seller",
        "elapsed=0s",
        "funded=true opened=false",
        "cleanup_ready=false",
    ] {
        assert!(report.contains(token), "missing {token:?}: {report}");
    }
    for token in [
        "dexdo status",
        &dexdo_core::address::display_self_dapp(&token_contract),
    ] {
        assert!(
            report.contains(token),
            "the unclipped durable line is not actionable; missing {token:?}: {report}"
        );
    }
    let progress_labels = progress.progress_labels();
    assert_eq!(progress_labels.len(), 1);
    let progress_label = &progress_labels[0];
    for token in [
        "waiting for a seller",
        "funded=true opened=false",
        "reclaim_in=",
    ] {
        assert!(
            progress_label.contains(token),
            "missing {token:?}: {progress_label}"
        );
    }
    assert!(
        !progress_label.contains(super::BUYER_STEP_ENDPOINT.0),
        "the endpoint is not being brought up before a handover exists: {progress_label}"
    );
    assert!(
        !progress_label.contains("cleanup_wait_secs"),
        "the live label must stay concise while its reclaim countdown changes"
    );
    assert!(
        !progress_label.contains("dexdo status") && !progress_label.contains(&token_contract),
        "the live label repeats the full durable context: {progress_label}"
    );
}

/// A redirected buyer still polls every 500ms, but its durable status is bounded by the separate
/// progress interval rather than adding one log line per chain read.
#[tokio::test]
async fn issue_1446_non_terminal_wait_status_is_rate_limited_while_polling_continues() {
    let token_contract = format!("0:{}", "5".repeat(64));
    let chain = RecordingRecoveryChain::with_deal_state(issue_1446_waiting_deal_state(0));
    let buyer = dexdo::buyer::Buyer::from_note(std::sync::Arc::new(
        dexdo_core::LocalNote::generate(),
    ));
    let mut progress = super::BuyerHandoverProgress::for_test(
        true,
        std::time::Duration::from_secs(5),
    );

    for _ in 0..2 {
        let failure = buyer
            .resolve_endpoint(&chain, &token_contract)
            .await
            .expect_err("the seller has not written a handover");
        super::wait_for_seller_handover_poll(
            &chain,
            &token_contract,
            &mut progress,
            &failure,
        )
        .await;
    }

    assert!(
        chain
            .handover_reads
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2,
        "the production handover poll must have continued"
    );
    assert_eq!(
        progress.reported_lines().len(),
        1,
        "unchanged non-terminal state must not be printed on every 500ms poll"
    );
}

#[tokio::test]
async fn issue_1446_progress_refreshes_and_redirected_heartbeat_stays_compact() {
    let token_contract = format!("0:{}", "7".repeat(64));
    let fresh = RecordingRecoveryChain::with_deal_state(issue_1446_waiting_deal_state(0));
    let older = RecordingRecoveryChain::with_deal_state(issue_1446_waiting_deal_state(30));
    let mut progress =
        super::BuyerHandoverProgress::for_test(true, std::time::Duration::ZERO);

    progress.report_if_due(&fresh, &token_contract).await;
    progress.report_if_due(&older, &token_contract).await;

    assert_eq!(progress.progress_labels().len(), 2);
    assert_ne!(
        progress.progress_labels()[0],
        progress.progress_labels()[1],
        "the TTY label must refresh its reclaim countdown"
    );
    for label in progress.progress_labels() {
        assert!(label.contains("funded=true opened=false"), "{label}");
        assert!(label.contains("reclaim_in="), "{label}");
        assert!(
            !label.contains("dexdo status") && !label.contains(&token_contract),
            "live label is not compact: {label}"
        );
    }

    let redirected = progress.reported_lines();
    assert_eq!(redirected.len(), 2);
    assert!(redirected[0].contains("dexdo status"), "{}", redirected[0]);
    for token in [
        "still waiting for a seller handover",
        "elapsed=",
        "funded=true opened=false",
        "reclaim_in=",
    ] {
        assert!(redirected[1].contains(token), "{}", redirected[1]);
    }
    assert!(
        !redirected[1].contains("dexdo status") && !redirected[1].contains(&token_contract),
        "redirected heartbeat repeats the full initial record: {}",
        redirected[1]
    );
}

/// The same explicit handle used by the lazy server must not create human progress in machine
/// mode, and it must fall back to durable heartbeats when the command's stderr is redirected.
#[tokio::test]
async fn issue_1446_on_demand_handle_preserves_machine_silence_and_redirected_heartbeats() {
    let token_contract = format!("0:{}", "9".repeat(64));
    let waiting = RecordingRecoveryChain::with_deal_state(issue_1446_waiting_deal_state(0));

    let machine_display = crate::cli::progress::Status::new(super::BUYER_STEP_ENDPOINT.0);
    let machine_shared = machine_display.shared();
    crate::cli::progress::lock(&machine_shared).live = true;
    let mut machine = super::BuyerHandoverProgress::new(
        false,
        None,
        Some(machine_display.handle()),
    );
    machine.interval = std::time::Duration::ZERO;
    machine.report_if_due(&waiting, &token_contract).await;
    assert_eq!(
        crate::cli::progress::lock(&machine_shared).label,
        super::BUYER_STEP_ENDPOINT.0
    );
    assert!(machine.progress_labels().is_empty());
    assert!(machine.reported_lines().is_empty());
    drop(machine);
    drop(machine_display);

    let redirected_display = crate::cli::progress::Status::new(super::BUYER_STEP_ENDPOINT.0);
    let redirected_shared = redirected_display.shared();
    assert!(!crate::cli::progress::lock(&redirected_shared).live);
    let mut redirected = super::BuyerHandoverProgress::new(
        true,
        None,
        Some(redirected_display.handle()),
    );
    redirected.interval = std::time::Duration::ZERO;
    redirected
        .report_if_due(&waiting, &token_contract)
        .await;
    redirected
        .report_if_due(&waiting, &token_contract)
        .await;
    assert_eq!(
        crate::cli::progress::lock(&redirected_shared).label,
        super::BUYER_STEP_ENDPOINT.0
    );
    assert_eq!(redirected.reported_lines().len(), 2);
    assert!(redirected.reported_lines()[1].contains("still waiting for a seller handover"));
}

/// A seller that has already written the handover is not a wait. The first read remains first, so
/// progress cannot briefly claim a no-show or add an avoidable lifecycle read on the happy path.
#[tokio::test]
async fn issue_1446_immediate_handover_emits_no_wait_status() {
    let token_contract = format!("0:{}", "6".repeat(64));
    let buyer_note = std::sync::Arc::new(dexdo_core::LocalNote::generate());
    let seller_note = dexdo_core::LocalNote::generate();
    let handover = dexdo_core::Handover {
        endpoint: "https://seller.example:443".to_string(),
        tls_fingerprint: "ab".repeat(32),
    };
    let encrypted = seller_note.encrypt_to(
        &buyer_note.pubkey(),
        &handover.to_deal_bytes(&token_contract),
    );
    let chain = RecordingRecoveryChain {
        deal_state: Some(deal_state(true, true, false, true)),
        handover: std::sync::Mutex::new(Some(encrypted)),
        ..Default::default()
    };
    let buyer = dexdo::buyer::Buyer::from_note(buyer_note);
    let progress = super::BuyerHandoverProgress::for_test(
        true,
        std::time::Duration::from_secs(5),
    );

    let received = buyer
        .resolve_endpoint(&chain, &token_contract)
        .await
        .expect("the first handover read succeeds");

    assert_eq!(received, handover);
    assert!(
        progress.reported_lines().is_empty(),
        "an immediate handover must not claim that the buyer waited"
    );
    assert_eq!(
        chain
            .handover_reads
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the ready path reads the handover exactly once"
    );
}

/// The first on-demand chat request runs its lazy purchase inside the Axum task, not on the command
/// thread that owns `CURRENT`. Drive that real server boundary: the explicit weak handle must
/// cover the command's current line while the seller is absent. Exercise the adversarial order in
/// which the owner publishes/completes the endpoint only after that wait is visible: removing the
/// wait must reveal the newest owner state, not the pre-ready snapshot.
#[test]
fn issue_1446_on_demand_first_request_updates_command_progress_across_thread() {
    let owner_thread = std::thread::current().id();
    let token_contract = format!("0:{}", "8".repeat(64));
    let chain_state = issue_1446_waiting_deal_state(0);
    let funded_time = chain_state.funded_time;
    let chain = std::sync::Arc::new(RecordingRecoveryChain {
        next_match: Some(token_contract.clone()),
        ..RecordingRecoveryChain::with_monitor_deal_state(chain_state)
    });
    let buyer_note = std::sync::Arc::new(dexdo_core::LocalNote::generate());
    let seller_note = dexdo_core::LocalNote::generate();
    let handover = dexdo_core::Handover {
        endpoint: "https://127.0.0.1:1".to_string(),
        tls_fingerprint: "cd".repeat(32),
    };
    let encrypted_handover = seller_note.encrypt_to(
        &buyer_note.pubkey(),
        &handover.to_deal_bytes(&token_contract),
    );
    let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::from_note(buyer_note));
    let args = std::sync::Arc::new(super::BuyerArgs {
        mock: super::MockFlags {
            mock_model: true,
            mock_chain: true,
        },
        identity: super::IdentityArgs {
            note_key: None,
            note_index: 0,
            note_addr: None,
        },
        registry: super::ModelRegistryValidationArgs::default(),
        endpoints_file: None,
        deals_dir: None,
        token_contract: Some(token_contract.clone()),
        resume: false,
        preserve_deal_on_exit: false,
        wait_for_seller: false,
        market: None,
        max_tokens: 8,
        local_listen: Some("127.0.0.1:0".parse().unwrap()),
        continuity_mode: super::ContinuityModeArg::OnDemand,
        json: false,
        anthropic_compat: false,
        frame_model: Some("Qwen3-32B".to_string()),
        allow_unverified_model: true,
        models: std::path::PathBuf::from("unused-models.json"),
        ticks: 2,
        max_price_per_tick: 1,
        escrow: Some(dexdo_core::required_escrow_for_buy(2, 1)),
        policy: None,
    });
    let raised_money = super::BuyerQuoteSubmitOutcome {
        token_contract: token_contract.clone(),
        status: super::MatchedTokenContractStatus::FundedNeverOpened {
            funded_time,
            cleanup_after_unix: funded_time
                .map(|time| time.saturating_add(super::BUYER_HANDOVER_WAIT_SECS)),
            cleanup_ready: false,
            remaining_secs: Some(super::BUYER_HANDOVER_WAIT_SECS),
        },
        ticks: 2,
        max_price_per_tick: 1,
        escrow: dexdo_core::required_escrow_for_buy(2, 1),
        submit_reconciliation: None,
    };

    let display = crate::cli::progress::Status::with_plan(
        super::BUYER_STEP_CHECKING.0,
        super::buyer_progress_plan(true),
    );
    let shared = display.shared();
    crate::cli::progress::lock(&shared).live = true;
    let checking_started = crate::cli::progress::lock(&shared).started;
    let state = super::build_on_demand_buyer_api_state(
        chain.clone(),
        buyer,
        args,
        Some(token_contract),
        "Qwen3-32B".to_string(),
        dexdo::buyer::api::ContentCheck::Skip,
        std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
        None,
        dexdo::buyer::api::BuyerApiFailurePolicy::default(),
        None,
        Some(display.handle()),
        Some(raised_money),
        super::BuyerChainPreflight::OfflineTest,
        None,
        false,
    );

    let (bound_tx, bound_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build isolated API runtime")
            .block_on(async move {
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
                .expect("bind real lazy Axum API");
                bound_tx.send(addr).unwrap();
                let response = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap()
                    .post(format!("http://{addr}/v1/chat/completions"))
                    .json(&serde_json::json!({
                        "model": "Qwen3-32B",
                        "messages": [{"role": "user", "content": "start the lazy purchase"}],
                        "max_tokens": 1,
                        "stream": false
                    }))
                    .send()
                    .await
                    .expect("first request reaches the configured seller after handover");
                let status = response.status();
                let body = response.text().await.expect("gateway error response body");
                let _ = shutdown_tx.send(());
                task.await.expect("lazy API task joins");
                (status, body)
            })
    });
    bound_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("lazy API binds before the first request");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let label = crate::cli::progress::lock(&shared)
            .displayed_label()
            .to_string();
        if label.contains("waiting for a seller: funded=true opened=false reclaim_in=") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first request left the command-owned display stale: {label}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let wait_started = crate::cli::progress::lock(&shared).displayed_started();
    assert_ne!(
        chain
            .handover_thread
            .lock()
            .unwrap()
            .expect("the real lazy initializer read the handover"),
        owner_thread,
        "the regression must cross the thread-local CURRENT boundary"
    );

    display.step(super::BUYER_STEP_ENDPOINT.0);
    display.finish();
    let endpoint_started = crate::cli::progress::lock(&shared).started;
    assert!(
        endpoint_started > checking_started,
        "the owner must have published a newer endpoint state while the wait is active"
    );
    assert!(
        crate::cli::progress::lock(&shared)
            .displayed_label()
            .contains("waiting for a seller: funded=true opened=false reclaim_in="),
        "the owner update must not hide an active seller wait"
    );
    assert_eq!(
        crate::cli::progress::lock(&shared).displayed_started(),
        wait_started,
        "the owner update must not restart the active wait's elapsed clock"
    );

    chain.set_monitor_deal_state(deal_state(true, true, false, true));
    *chain.handover.lock().unwrap() = Some(encrypted_handover);
    let (status, body) = server.join().expect("isolated API runtime joins");
    // Port 1 deliberately has no seller. Reaching its typed gateway error proves initialization
    // consumed the supplied handover; this regression is about the preceding progress lifecycle.
    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("E_GATEWAY_UNREACHABLE"), "{body}");
    let restored = crate::cli::progress::lock(&shared);
    assert_eq!(restored.label, super::BUYER_STEP_ENDPOINT.0);
    assert_eq!(restored.displayed_label(), super::BUYER_STEP_ENDPOINT.0);
    assert_eq!(
        restored.started, endpoint_started,
        "the request-owned wait must restore the command display's elapsed clock"
    );
}
