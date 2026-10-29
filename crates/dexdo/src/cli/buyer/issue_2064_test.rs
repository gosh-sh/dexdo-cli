fn issue_2064_forwarding_error() -> anyhow::Error {
    let response = serde_json::from_str(include_str!(
        "../../../../core/src/chain/client/fixtures/block_producer_forwarding_timeout_2064.json"
    ))
    .unwrap();
    let decoded =
        dexdo_core::onchain_diagnostics::validate_onchain_submit_response(response).unwrap_err();
    anyhow::Error::new(dexdo_core::ChainError::AmbiguousSubmit(decoded.to_string()))
}

fn issue_2064_fill(journal: &super::BuyerSubmitJournal) -> dexdo_core::MatchedFill {
    let quoted = buyer_submit_test_quoted(journal);
    dexdo_core::MatchedFill {
        order_id: quoted.order_id,
        token_contract: quoted.token_contract.clone().unwrap(),
        ticks: journal.ticks,
        price_per_tick: quoted.price_per_tick,
    }
}

fn issue_2064_pool(dir: &std::path::Path, note_addr: &str) -> std::path::PathBuf {
    let path = dir.join("pool.json");
    crate::cli::support::write_owner_only_key_fixture(
        &path,
        &serde_json::json!({"token_type": dexdo_core::params::SHELL_CURRENCY_ID,
            "notes":[{"address":note_addr,"owner_secret_key_hex":"00"}]})
        .to_string(),
    );
    path
}

// Serializes the existing process-global pool fixture throughout the async scenario.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_2064_delayed_match_survives_restart_before_handover() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-delayed");
    let fixture = buyer_submit_test_journal();
    let pool = issue_2064_pool(&dir, &fixture.note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let journal_path = dir.join("journal.json");
    let mut chain = journal_outcome_chain(&journal_path, Vec::new());
    chain.submit_error = Some("ambiguous");
    let (note_addr, selection, submitted) = journal_pipeline_place(&chain, &journal_path).await;
    assert!(submitted.is_err());
    let original = super::load_buyer_submit_journal(&journal_path, &note_addr)
        .unwrap()
        .unwrap();
    let error = super::complete_buyer_submit_with_journal(
        &chain,
        selection.quoted_order.as_ref(),
        original.ticks,
        original.max_price_per_tick,
        Err(issue_2064_forwarding_error()),
        &note_addr,
        &journal_path,
    )
    .await
    .unwrap_err();
    assert!(super::is_ambiguous_submit_error(&error));
    let code = crate::cli::machine::classify_error(crate::cli::machine::OP_BUYER_START, &error);
    assert_eq!(code.as_str(), "AMBIGUOUS_SUBMIT");
    assert!(!code.retryable(), "fresh money retry must remain forbidden");
    assert_eq!(
        super::load_buyer_submit_journal(&journal_path, &note_addr)
            .unwrap()
            .unwrap(),
        original
    );

    // The effect becomes visible only after the first command has already returned.
    chain.fill = Some(issue_2064_fill(&original));
    let buyer = dexdo::buyer::Buyer::generate();
    let resumed = super::start_durable_buyer_submit(
        &chain,
        &buyer,
        &original.intent,
        original.expected_token_contract.as_deref(),
        &selection,
        original.ticks,
        original.max_price_per_tick,
        original.escrow,
        &note_addr,
        &journal_path,
        None,
    )
    .await
    .unwrap();
    let super::DurableBuyerSubmitStart::Reconciled { token_contract, .. } = resumed else {
        panic!("restart must recover the original purchase");
    };
    assert_eq!(token_contract, chain.fill.as_ref().unwrap().token_contract);
    let recovered = super::load_buyer_submit_journal(&journal_path, &note_addr)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.submit_identity, original.submit_identity);
    assert_eq!(recovered.resolved_matches.len(), 1);
    assert_eq!(
        chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(
        !dir.join("deals").exists(),
        "recovery must precede the completed deal handle"
    );
}

// Serializes the existing process-global pool fixture throughout the async scenario.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_2064_immediate_match_keeps_identity_until_deal_handle_is_saved() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-matched");
    let fixture = buyer_submit_test_journal();
    let pool = issue_2064_pool(&dir, &fixture.note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let journal_path = dir.join("journal.json");
    let mut chain = journal_outcome_chain(&journal_path, Vec::new());
    chain.fill = Some(issue_2064_fill(&fixture));
    chain.submit_error = Some("ambiguous");
    let (note_addr, selection, _) = journal_pipeline_place(&chain, &journal_path).await;
    let original = super::load_buyer_submit_journal(&journal_path, &note_addr)
        .unwrap()
        .unwrap();
    let (token_contract, _) = super::complete_buyer_submit_with_journal(
        &chain,
        selection.quoted_order.as_ref(),
        original.ticks,
        original.max_price_per_tick,
        Err(issue_2064_forwarding_error()),
        &note_addr,
        &journal_path,
    )
    .await
    .unwrap();
    let recovered = super::load_buyer_submit_journal(&journal_path, &note_addr)
        .unwrap()
        .expect("the journal must survive the match-to-handover gap");
    assert_eq!(recovered.submit_identity, original.submit_identity);
    assert_eq!(recovered.resolved_matches[0].token_contract, token_contract);
    let super::DurableBuyerSubmitStart::Reconciled {
        token_contract: restarted,
        ..
    } = super::start_durable_buyer_submit(
        &chain,
        &dexdo::buyer::Buyer::generate(),
        &original.intent,
        original.expected_token_contract.as_deref(),
        &selection,
        original.ticks,
        original.max_price_per_tick,
        original.escrow,
        &note_addr,
        &journal_path,
        None,
    )
    .await
    .unwrap()
    else {
        panic!("restart before handover must adopt the same deal");
    };
    assert_eq!(restarted, token_contract);
    assert_eq!(
        chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn issue_2064_restart_read_failure_retains_identity_and_never_posts() {
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-unreadable");
    let journal_path = dir.join("journal.json");
    let journal = buyer_submit_test_journal();
    super::write_buyer_submit_journal(&journal_path, &journal).unwrap();
    let mut chain = journal_outcome_chain(&journal_path, Vec::new());
    chain.submit_error = Some("replay_once");
    let selection = journal_pipeline_selection();
    let error = super::start_durable_buyer_submit(
        &chain,
        &dexdo::buyer::Buyer::generate(),
        &journal.intent,
        journal.expected_token_contract.as_deref(),
        &selection,
        journal.ticks,
        journal.max_price_per_tick,
        journal.escrow,
        &journal.note_addr,
        &journal_path,
        None,
    )
    .await
    .err()
    .expect("unavailable reads cannot authorize a fresh buy");
    assert!(format!("{error:#}").contains("replay protection"));
    assert_eq!(
        super::load_buyer_submit_journal(&journal_path, &journal.note_addr)
            .unwrap()
            .unwrap(),
        journal
    );
    assert_eq!(
        chain.post_count.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

proptest::proptest! {
    #[test]
    fn issue_2064_foreign_fill_never_replaces_pending_identity(other in 4_u8..=15) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-foreign");
            let journal_path = dir.join("journal.json");
            let journal = buyer_submit_test_journal();
            super::write_buyer_submit_journal(&journal_path, &journal).unwrap();
            let mut chain = journal_outcome_chain(&journal_path, Vec::new());
            let mut fill = issue_2064_fill(&journal);
            fill.token_contract = format!("0:{}", format!("{other:x}").repeat(64));
            chain.fill = Some(fill);
            assert!(super::reconcile_pending_buyer_submit(&chain, &journal.note_addr,
                &journal_path, None).await.is_err());
            assert_eq!(super::load_buyer_submit_journal(&journal_path, &journal.note_addr).unwrap().unwrap(), journal);
            assert_eq!(chain.post_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        });
    }
}

async fn issue_2064_policy_buy(
    chain: &RecordingRecoveryChain,
    buyer: &dexdo::buyer::Buyer,
    note: &str,
    intent: &super::BuyerSubmitIntent,
) -> super::BuyerQuoteSubmitOutcome {
    let selection = super::buyer_quote_selection_for_submit(
        chain,
        false,
        Some(note),
        intent,
        None,
        1,
        1,
        Some(1),
        None,
    )
    .await
    .unwrap();
    super::execute_buyer_quote_submit(
        chain,
        buyer,
        false,
        Some(note),
        intent,
        None,
        &selection,
        1,
        1,
        1,
        None,
        |_| async { Ok(()) },
    )
    .await
    .unwrap()
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_2064_confirmed_cleanup_releases_old_identity_before_next_buy() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-cleanup-next");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let a = issue_2064_tc("4");
    let b = issue_2064_tc("5");
    let mut chain = RecordingRecoveryChain {
        next_match: Some(a.clone()),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    };
    let buyer = dexdo::buyer::Buyer::generate();
    let initial = issue_2064_policy_buy(
        &chain,
        &buyer,
        &note,
        &super::BuyerSubmitIntent::foreground(),
    )
    .await;
    assert_eq!(initial.token_contract, a);
    assert!(lock.journal_path.exists());
    chain.next_match = Some(b.clone());
    let policy = policy_for_no_handover(crate::cli::policy::NoHandoverAfterMatchAction::NextSeller);
    let outcome = super::apply_no_handover_after_match_policy(
        &chain,
        &buyer,
        &a,
        &policy,
        false,
        Some((1, 1, 1)),
        1,
        "timeout",
        Some(&note),
        initial
            .submit_reconciliation
            .as_ref()
            .map(|proof| proof.submit_identity.as_str()),
    )
    .await
    .expect("confirmed cleanup must allow the next logical BUY");
    assert!(matches!(
        outcome,
        super::NoHandoverPolicyOutcome::RetryNext(_)
    ));
    let journal = super::load_buyer_submit_journal(&lock.journal_path, &note)
        .unwrap()
        .unwrap();
    assert_eq!(
        journal.intent,
        super::BuyerSubmitIntent::after(super::BuyerSubmitIntentKind::PolicyNextSeller, &a)
    );
    assert_eq!(journal.resolved_matches[0].token_contract, b);
    assert_eq!(
        chain
            .place_next_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    let _ = std::fs::remove_file(&lock.journal_path);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_2064_wait_then_reclaim_finishes_retained_journal() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-cleanup-terminal");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let chain = RecordingRecoveryChain {
        next_match: Some(issue_2064_tc("5")),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    };
    let buyer = dexdo::buyer::Buyer::generate();
    let initial = issue_2064_policy_buy(
        &chain,
        &buyer,
        &note,
        &super::BuyerSubmitIntent::foreground(),
    )
    .await;
    let policy =
        policy_for_no_handover(crate::cli::policy::NoHandoverAfterMatchAction::WaitThenReclaim);
    let error = super::apply_no_handover_after_match_policy(
        &chain,
        &buyer,
        &initial.token_contract,
        &policy,
        false,
        None,
        1,
        "timeout",
        Some(&note),
        initial
            .submit_reconciliation
            .as_ref()
            .map(|proof| proof.submit_identity.as_str()),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("money_reclaimed"));
    assert!(
        !lock.journal_path.exists(),
        "a confirmed terminal purchase must stop being pending"
    );
    assert_eq!(
        chain
            .place_next_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

fn issue_2064_tc(digit: &str) -> String {
    format!("0:{}", digit.repeat(64))
}

fn issue_2064_set_handover(
    chain: &RecordingRecoveryChain,
    buyer: &dexdo::buyer::Buyer,
    token_contract: &str,
) {
    let seller = dexdo_core::LocalNote::generate();
    let handover = dexdo_core::Handover {
        endpoint: "https://127.0.0.1:1".to_string(),
        tls_fingerprint: "ab".repeat(32),
    };
    *chain.handover.lock().unwrap() = Some(dexdo_core::Note::encrypt_to(
        &seller,
        &buyer.note.pubkey(),
        &handover.to_deal_bytes(token_contract),
    ));
}

#[allow(clippy::await_holding_lock)]
async fn issue_2064_continuity_adoption(kind: super::BuyerSubmitIntentKind) {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-continuity");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let a = issue_2064_tc("4");
    let b = issue_2064_tc("5");
    let c = issue_2064_tc("6");
    let terms = super::BuyerOrderTerms {
        ticks: dexdo_core::params::MIN_STREAM_BUY_TICKS,
        max_price: dexdo_core::PRICE_STEP,
        escrow: dexdo_core::required_escrow_for_buy(
            dexdo_core::params::MIN_STREAM_BUY_TICKS,
            dexdo_core::PRICE_STEP,
        ),
    };
    let mut state = ready_funded_never_opened_state();
    state.opened = true;
    state.probe_accepted = true;
    state.last_claim_time = super::unix_now_secs();
    let chain = std::sync::Arc::new(RecordingRecoveryChain {
        next_match: Some(b.clone()),
        model_buy_terms: Some(terms),
        // Renewal falls back to the local remaining budget on a read outage. An absent
        // account instead proves closure and must select the next-seller intent.
        no_deal_state_for: (kind == super::BuyerSubmitIntentKind::ContinuityNextSeller)
            .then(|| a.clone()),
        deal_state_read_error_for: (kind == super::BuyerSubmitIntentKind::ContinuityRenewal)
            .then(|| a.clone()),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        ..RecordingRecoveryChain::with_deal_state(state)
    });
    let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::generate());
    let session = std::sync::Arc::new(dexdo::buyer::api::SessionSettle::new_with_failure_policy(
        chain.clone(),
        a.clone(),
        buyer.note.clone(),
        dexdo::buyer::api::BuyerApiFailurePolicy::default(),
    ));
    if kind == super::BuyerSubmitIntentKind::ContinuityNextSeller {
        session
            .mark_recovered_serialized("test-confirmed-terminal-A")
            .await;
    }
    let routes = std::sync::Arc::new(dexdo::buyer::api::RouteManager::new(
        dexdo::buyer::api::ApiDeal::new(
            dexdo::buyer::api::Route {
                handover: dexdo_core::Handover {
                    endpoint: "https://127.0.0.1:1".to_string(),
                    tls_fingerprint: "ab".repeat(32),
                },
                token_contract: a.clone(),
                max_tokens: dexdo_core::params::BUYER_RENEWAL_THRESHOLD_TOKENS,
            },
            session,
            std::sync::Arc::new(dexdo::buyer::api::ContentGate::skip()),
        ),
    ));
    let deals_dir = dir.join("deals");
    super::spawn_buyer_service_renewal(
        chain.clone(),
        buyer.clone(),
        routes.clone(),
        Some(note.clone()),
        terms.ticks,
        terms.max_price,
        terms.escrow,
        dexdo::buyer::continuity::ContinuityMode::Proactive,
        dexdo::buyer::api::ContentCheck::Skip,
        std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
        dexdo::buyer::api::BuyerApiFailurePolicy::default(),
        false,
        "Qwen3-32B".to_string(),
        Some(deals_dir.clone()),
        None,
    );
    wait_for_counter(&chain.place_next_calls, 1, "continuity BUY B").await;
    let original = super::load_buyer_submit_journal(&lock.journal_path, &note)
        .unwrap()
        .unwrap();
    assert_eq!(original.intent, super::BuyerSubmitIntent::after(kind, &a));
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert_eq!(
        routes.current().await.unwrap().route.token_contract,
        a,
        "missing handover must keep the old route"
    );
    assert_eq!(
        super::load_buyer_submit_journal(&lock.journal_path, &note)
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        chain
            .place_next_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    issue_2064_set_handover(chain.as_ref(), buyer.as_ref(), &b);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while routes.current().await.unwrap().route.token_contract != b {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the actual continuity monitor must adopt recovered B");
    let handle_path = crate::cli::deals::handle_path(
        &deals_dir,
        &crate::cli::deals::make_handle_id(&b, crate::cli::deals::DealHandleRole::Buyer),
    );
    let saved = crate::cli::deals::load_deal_handle(&handle_path).unwrap();
    assert_eq!(saved.token_contract, b);
    assert!(
        saved.created_order_ids.is_empty(),
        "a quoted SELL is not a created BUY order"
    );
    assert!(!lock.journal_path.exists());
    assert_eq!(
        chain
            .place_next_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "B must not be reposted during adoption"
    );

    let next_chain = RecordingRecoveryChain {
        next_match: Some(c.clone()),
        model_buy_terms: Some(terms),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        ..RecordingRecoveryChain::with_deal_state(state)
    };
    let next_intent = super::BuyerSubmitIntent::after(kind, &b);
    let next = super::submit_buyer_continuity_next_deal(
        &next_chain,
        buyer.as_ref(),
        None,
        &b,
        Some(&note),
        &next_intent,
        terms,
    )
    .await
    .unwrap();
    assert_eq!(next.token_contract, c);
    assert_eq!(
        next_chain
            .place_next_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        super::load_buyer_submit_journal(&lock.journal_path, &note)
            .unwrap()
            .unwrap()
            .intent,
        next_intent
    );
    let _ = std::fs::remove_file(&lock.journal_path);
}

#[tokio::test]
async fn issue_2064_continuity_next_seller_adopts_before_next_buy() {
    issue_2064_continuity_adoption(super::BuyerSubmitIntentKind::ContinuityNextSeller).await;
}

#[tokio::test]
async fn issue_2064_continuity_renewal_adopts_before_next_buy() {
    issue_2064_continuity_adoption(super::BuyerSubmitIntentKind::ContinuityRenewal).await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_2064_failed_handle_save_retains_identity_and_blocks_next_buy() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-save-failure");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let a = issue_2064_tc("4");
    let b = issue_2064_tc("5");
    let mut chain = RecordingRecoveryChain {
        next_match: Some(b.clone()),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    };
    let buyer = dexdo::buyer::Buyer::generate();
    let intent =
        super::BuyerSubmitIntent::after(super::BuyerSubmitIntentKind::ContinuityRenewal, &a);
    let mut outcome = issue_2064_policy_buy(&chain, &buyer, &note, &intent).await;
    let original = super::load_buyer_submit_journal(&lock.journal_path, &note)
        .unwrap()
        .unwrap();
    issue_2064_set_handover(&chain, &buyer, &b);
    buyer.resolve_endpoint(&chain, &b).await.unwrap();
    let deals_dir = dir.join("deals");
    std::fs::write(&deals_dir, "not a directory").unwrap();
    assert!(super::adopt_buyer_continuity_deal(
        &mut outcome,
        Some(&note),
        "Qwen3-32B",
        Some(&deals_dir),
        None,
        "shellnet"
    )
    .is_err());
    assert_eq!(
        outcome
            .submit_reconciliation
            .as_ref()
            .unwrap()
            .submit_identity,
        original.submit_identity
    );
    assert_eq!(
        super::load_buyer_submit_journal(&lock.journal_path, &note)
            .unwrap()
            .unwrap(),
        original
    );
    chain.next_match = Some(issue_2064_tc("6"));
    let next_intent =
        super::BuyerSubmitIntent::after(super::BuyerSubmitIntentKind::ContinuityRenewal, &b);
    assert!(super::buyer_quote_selection_for_submit(
        &chain,
        false,
        Some(&note),
        &next_intent,
        None,
        1,
        1,
        Some(1),
        None
    )
    .await
    .is_err());
    assert_eq!(
        chain
            .place_next_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let _ = std::fs::remove_file(&lock.journal_path);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_2064_unconfirmed_cleanup_keeps_identity_and_never_buys_again() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-unknown-cleanup");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let chain = RecordingRecoveryChain {
        next_match: Some(issue_2064_tc("4")),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        cleanup_ambiguous_remaining: std::sync::atomic::AtomicUsize::new(2),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    };
    let buyer = dexdo::buyer::Buyer::generate();
    let initial = issue_2064_policy_buy(
        &chain,
        &buyer,
        &note,
        &super::BuyerSubmitIntent::foreground(),
    )
    .await;
    let original = super::load_buyer_submit_journal(&lock.journal_path, &note)
        .unwrap()
        .unwrap();
    for action in [
        crate::cli::policy::NoHandoverAfterMatchAction::NextSeller,
        crate::cli::policy::NoHandoverAfterMatchAction::WaitThenReclaim,
    ] {
        let error = super::apply_no_handover_after_match_policy(
            &chain,
            &buyer,
            &initial.token_contract,
            &policy_for_no_handover(action),
            false,
            Some((1, 1, 1)),
            1,
            "timeout",
            Some(&note),
            Some(&original.submit_identity),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected unknown cleanup result"));
        assert_eq!(
            super::load_buyer_submit_journal(&lock.journal_path, &note)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(
            chain
                .place_next_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
    let _ = std::fs::remove_file(&lock.journal_path);
}

#[allow(clippy::await_holding_lock)]
async fn issue_2064_policy_entrypoint_adoption(lazy: bool, fail_save: bool) {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-policy-entrypoint");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let _ = std::fs::remove_file(&lock.subscriptions_path);
    let a = issue_2064_tc("4");
    let b = issue_2064_tc("5");
    let terms = super::BuyerOrderTerms {
        ticks: dexdo_core::params::MIN_STREAM_BUY_TICKS,
        max_price: dexdo_core::PRICE_STEP,
        escrow: dexdo_core::required_escrow_for_buy(
            dexdo_core::params::MIN_STREAM_BUY_TICKS,
            dexdo_core::PRICE_STEP,
        ),
    };
    let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::generate());
    let chain = std::sync::Arc::new(RecordingRecoveryChain {
        next_match: Some(a),
        next_match_after_cleanup: Some(b.clone()),
        model_buy_terms: Some(terms),
        model_buy_ambiguous_after_cleanup: true,
        expected_journal_path: Some(lock.journal_path.clone()),
        handover_token_contract: Some(b.clone()),
        successor_state_for: Some(b.clone()),
        successor_state: std::sync::Mutex::new(Some(ready_funded_never_opened_state())),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    });
    issue_2064_set_handover(chain.as_ref(), buyer.as_ref(), &b);
    let mut args = spot_resume_buyer_args(&dir, &note, "Qwen3-32B");
    args.resume = false;
    args.ticks = terms.ticks;
    args.max_price_per_tick = Some(terms.max_price);
    args.escrow = Some(terms.escrow);
    args.continuity_mode = super::ContinuityModeArg::Proactive;
    let mut policy =
        policy_for_no_handover(crate::cli::policy::NoHandoverAfterMatchAction::NextSeller);
    policy.total_spend_cap_shells =
        u64::try_from(terms.escrow).unwrap() * policy.max_sellers_to_try;
    let policy_path = args.policy.as_ref().unwrap();
    let mut policy_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(policy_path).unwrap()).unwrap();
    policy_json["buyer"]["on"]["no_handover_after_match"] = serde_json::json!("next_seller");
    policy_json["buyer"]["failover"]["max_sellers_to_try"] =
        serde_json::json!(policy.max_sellers_to_try);
    policy_json["buyer"]["failover"]["total_spend_cap_shells"] =
        serde_json::json!(policy.total_spend_cap_shells);
    std::fs::write(policy_path, serde_json::to_vec(&policy_json).unwrap()).unwrap();
    let deals_dir = args.deals_dir.as_ref().unwrap().clone();
    // Keep history preflight readable; fail only B's later handle publication.
    let blocked_lock = deals_dir.join(format!(
        ".{}.json.lock",
        crate::cli::deals::make_handle_id(&b, crate::cli::deals::DealHandleRole::Buyer)
    ));
    if fail_save {
        std::fs::create_dir_all(&blocked_lock).unwrap();
    }
    let result = if lazy {
        super::prepare_lazy_buyer_api_deal_once(
            chain.clone(),
            buyer.clone(),
            std::sync::Arc::new(args),
            None,
            "Qwen3-32B".to_string(),
            dexdo::buyer::api::ContentCheck::Skip,
            std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
            Some(policy),
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            None,
            None,
            None,
            super::BuyerChainPreflight::OfflineTest,
        )
        .await
        .map(|deal| {
            assert_eq!(deal.route.token_contract, b);
        })
    } else {
        let (writer, captured) = crate::cli::machine::BuyerEventWriter::capturing();
        super::run_buyer_inner(
            args,
            &mut Some(writer),
            &mut super::BuyerMachineErrorContext::default(),
            super::BuyerCommandRuntime {
                backend: Some((chain.clone(), buyer.note.clone())),
                chain_preflight: super::BuyerChainPreflight::OfflineTest,
                shutdown: Box::pin(async move {
                    loop {
                        if captured
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|event| event["event"] == "endpoint_ready")
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }),
            },
        )
        .await
    };
    assert_eq!(
        chain
            .cleanup_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        chain
            .place_next_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "A and B each get one BUY, recovered B is never resubmitted"
    );
    if fail_save {
        let error = result.expect_err("handle persistence must fail");
        assert!(
            format!("{error:#}").contains("open deal-record lock"),
            "{error:#}"
        );
        let retained = super::load_buyer_submit_journal(&lock.journal_path, &note)
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.intent.kind,
            super::BuyerSubmitIntentKind::PolicyNextSeller
        );
        assert_eq!(retained.resolved_matches[0].token_contract, b);
        std::fs::remove_dir(&blocked_lock).unwrap();
        issue_2064_restart_retained_successor(
            lazy,
            chain.clone(),
            buyer.clone(),
            &dir,
            &note,
            &lock.journal_path,
            &retained,
        )
        .await;
    } else {
        result.unwrap();
        let path = crate::cli::deals::handle_path(
            &deals_dir,
            &crate::cli::deals::make_handle_id(&b, crate::cli::deals::DealHandleRole::Buyer),
        );
        let saved = crate::cli::deals::load_deal_handle(&path).unwrap();
        assert_eq!(saved.token_contract, b);
        assert!(
            saved.created_order_ids.is_empty(),
            "a quoted SELL is not a created BUY order"
        );
        assert!(
            !lock.journal_path.exists(),
            "the actual entrypoint must finish B's identity"
        );
    }
    let _ = std::fs::remove_file(&lock.journal_path);
}

#[tokio::test]
async fn issue_2064_normal_policy_next_seller_adopts_identity() {
    issue_2064_policy_entrypoint_adoption(false, false).await;
}

#[tokio::test]
async fn issue_2064_lazy_policy_next_seller_adopts_identity() {
    issue_2064_policy_entrypoint_adoption(true, false).await;
}

#[tokio::test]
async fn issue_2064_normal_policy_next_seller_save_failure_retains_identity() {
    issue_2064_policy_entrypoint_adoption(false, true).await;
}

#[tokio::test]
async fn issue_2064_lazy_policy_next_seller_save_failure_retains_identity() {
    issue_2064_policy_entrypoint_adoption(true, true).await;
}

fn issue_2064_stream_terms() -> super::BuyerOrderTerms {
    let ticks = dexdo_core::params::MIN_STREAM_BUY_TICKS;
    let max_price = dexdo_core::PRICE_STEP;
    super::BuyerOrderTerms {
        ticks,
        max_price,
        escrow: dexdo_core::required_escrow_for_buy(ticks, max_price),
    }
}

async fn issue_2064_run_entrypoint(
    lazy: bool,
    chain: std::sync::Arc<RecordingRecoveryChain>,
    buyer: std::sync::Arc<dexdo::buyer::Buyer>,
    args: super::BuyerArgs,
) -> anyhow::Result<()> {
    if lazy {
        let policy = crate::cli::policy::load_buyer_runtime_policy(args.policy.as_deref())?;
        super::prepare_lazy_buyer_api_deal_once(
            chain,
            buyer,
            std::sync::Arc::new(args),
            None,
            "Qwen3-32B".into(),
            dexdo::buyer::api::ContentCheck::Skip,
            std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
            Some(policy),
            dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            None,
            None,
            None,
            super::BuyerChainPreflight::OfflineTest,
        )
        .await
        .map(|_| ())
    } else {
        let (writer, captured) = crate::cli::machine::BuyerEventWriter::capturing();
        super::run_buyer_inner(
            args,
            &mut Some(writer),
            &mut super::BuyerMachineErrorContext::default(),
            super::BuyerCommandRuntime {
                backend: Some((chain, buyer.note.clone())),
                chain_preflight: super::BuyerChainPreflight::OfflineTest,
                shutdown: Box::pin(async move {
                    loop {
                        if captured
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|e| e["event"] == "endpoint_ready")
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }),
            },
        )
        .await
    }
}

fn issue_2064_resume_args(dir: &std::path::Path, note: &str, lazy: bool) -> super::BuyerArgs {
    let mut args = spot_resume_buyer_args(dir, note, "Qwen3-32B");
    args.ticks = dexdo_core::params::MIN_STREAM_BUY_TICKS;
    args.max_price_per_tick = None;
    args.escrow = None;
    args.continuity_mode = if lazy {
        super::ContinuityModeArg::OnDemand
    } else {
        super::ContinuityModeArg::Proactive
    };
    args
}

// Drives the actual resume entry twice: before handle persistence and after handle save with
// the original journal still present. A failed authoritative read precedes each successful restart.
async fn issue_2064_restart_retained_successor(
    lazy: bool,
    chain: std::sync::Arc<RecordingRecoveryChain>,
    buyer: std::sync::Arc<dexdo::buyer::Buyer>,
    dir: &std::path::Path,
    note: &str,
    journal_path: &std::path::Path,
    original: &super::BuyerSubmitJournal,
) {
    use std::sync::atomic::Ordering;
    let posts = chain.place_next_calls.load(Ordering::SeqCst);
    let tc = original.resolved_matches[0].token_contract.clone();
    let deals_dir = dir.join("deals");
    for handle_saved in [false, true] {
        if handle_saved {
            super::write_buyer_submit_journal(journal_path, original).unwrap();
        }
        chain
            .successor_state_read_failures
            .store(1, Ordering::SeqCst);
        let error = issue_2064_run_entrypoint(
            lazy,
            chain.clone(),
            buyer.clone(),
            issue_2064_resume_args(dir, note, lazy),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("temporary successor read failure"),
            "{error:#}"
        );
        assert_eq!(
            super::load_buyer_submit_journal(journal_path, note)
                .unwrap()
                .unwrap(),
            *original
        );
        assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), posts);
        issue_2064_run_entrypoint(
            lazy,
            chain.clone(),
            buyer.clone(),
            issue_2064_resume_args(dir, note, lazy),
        )
        .await
        .unwrap();
        let handle = crate::cli::deals::load_deal_handle(&crate::cli::deals::handle_path(
            &deals_dir,
            &crate::cli::deals::make_handle_id(&tc, crate::cli::deals::DealHandleRole::Buyer),
        ))
        .unwrap();
        assert_eq!(handle.token_contract, tc);
        assert!(handle.created_order_ids.is_empty());
        assert!(!journal_path.exists());
        assert_eq!(
            chain.place_next_calls.load(Ordering::SeqCst),
            posts,
            "resume must never buy B or C again"
        );
    }
}

#[allow(clippy::await_holding_lock)]
async fn issue_2064_continuity_restart(kind: super::BuyerSubmitIntentKind) {
    use std::sync::atomic::Ordering;
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-successor-restart");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let a = issue_2064_tc("4");
    let b = issue_2064_tc("5");
    let terms = issue_2064_stream_terms();
    let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::generate());
    let chain = std::sync::Arc::new(RecordingRecoveryChain {
        next_match: Some(b.clone()),
        model_buy_terms: Some(terms),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        expected_journal_path: Some(lock.journal_path.clone()),
        successor_state_for: Some(b.clone()),
        successor_state: std::sync::Mutex::new(Some(ready_funded_never_opened_state())),
        handover_token_contract: Some(b.clone()),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    });
    let intent = super::BuyerSubmitIntent::after(kind, &a);
    super::submit_buyer_continuity_next_deal(
        chain.as_ref(),
        buyer.as_ref(),
        None,
        &a,
        Some(&note),
        &intent,
        terms,
    )
    .await
    .unwrap();
    let original = super::load_buyer_submit_journal(&lock.journal_path, &note)
        .unwrap()
        .unwrap();
    assert_eq!(original.intent, intent);
    issue_2064_set_handover(chain.as_ref(), buyer.as_ref(), &b);
    let mut fresh = issue_2064_resume_args(&dir, &note, false);
    fresh.resume = false;
    fresh.max_price_per_tick = Some(terms.max_price);
    assert!(format!(
        "{:#}",
        issue_2064_run_entrypoint(false, chain.clone(), buyer.clone(), fresh)
            .await
            .unwrap_err()
    )
    .contains("different logical invocation"));
    let mut foreign = issue_2064_resume_args(&dir, &note, false);
    foreign.token_contract = Some(a);
    assert!(format!(
        "{:#}",
        issue_2064_run_entrypoint(false, chain.clone(), buyer.clone(), foreign)
            .await
            .unwrap_err()
    )
    .contains("does not match the requested TokenContract"));
    assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        super::load_buyer_submit_journal(&lock.journal_path, &note)
            .unwrap()
            .unwrap(),
        original
    );
    issue_2064_restart_retained_successor(
        false,
        chain,
        buyer,
        &dir,
        &note,
        &lock.journal_path,
        &original,
    )
    .await;
}

#[tokio::test]
async fn issue_2064_continuity_next_seller_restarts_original_successor() {
    issue_2064_continuity_restart(super::BuyerSubmitIntentKind::ContinuityNextSeller).await;
}
#[tokio::test]
async fn issue_2064_continuity_renewal_restarts_original_successor() {
    issue_2064_continuity_restart(super::BuyerSubmitIntentKind::ContinuityRenewal).await;
}

#[allow(clippy::await_holding_lock)]
async fn issue_2064_pending_cleanup_recovery(mode: &str) {
    use std::sync::atomic::Ordering;
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-pending-cleanup");
    let note = buyer_submit_test_journal().note_addr;
    let pool = issue_2064_pool(&dir, &note);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let lock = super::BuyerMoneyLock::open(&note).unwrap();
    let _ = std::fs::remove_file(&lock.journal_path);
    let a = issue_2064_tc("4");
    let b = issue_2064_tc("5");
    let terms = issue_2064_stream_terms();
    let chain = std::sync::Arc::new(RecordingRecoveryChain {
        next_match: Some(b.clone()),
        model_buy_terms: Some(terms),
        no_deal_state_for: (mode != "local").then(|| a.clone()),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        successor_state_for: Some(if mode == "local" {
            a.clone()
        } else {
            b.clone()
        }),
        successor_state: std::sync::Mutex::new(
            (mode != "local").then(ready_funded_never_opened_state),
        ),
        cleanup_failures_remaining: std::sync::atomic::AtomicUsize::new(usize::from(
            mode == "refusal",
        )),
        cleanup_ambiguous_remaining: std::sync::atomic::AtomicUsize::new(usize::from(
            mode == "unknown",
        )),
        cleanup_completes_successor: true,
        cleanup_completion_lock_note: (mode == "local").then(|| note.clone()),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    });
    let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::generate());
    let make_active = |tokens| {
        dexdo::buyer::api::ApiDeal::new(
            dexdo::buyer::api::Route {
                handover: dexdo_core::Handover {
                    endpoint: "https://127.0.0.1:1".into(),
                    tls_fingerprint: "ab".repeat(32),
                },
                token_contract: a.clone(),
                max_tokens: tokens,
            },
            std::sync::Arc::new(dexdo::buyer::api::SessionSettle::new_with_failure_policy(
                chain.clone(),
                a.clone(),
                buyer.note.clone(),
                dexdo::buyer::api::BuyerApiFailurePolicy::default(),
            )),
            std::sync::Arc::new(dexdo::buyer::api::ContentGate::skip()),
        )
    };
    let routes = std::sync::Arc::new(dexdo::buyer::api::RouteManager::new(make_active(
        dexdo_core::params::BUYER_RENEWAL_THRESHOLD_TOKENS,
    )));
    super::spawn_buyer_service_renewal(
        chain.clone(),
        buyer.clone(),
        routes.clone(),
        Some(note.clone()),
        terms.ticks,
        terms.max_price,
        terms.escrow,
        dexdo::buyer::continuity::ContinuityMode::Proactive,
        dexdo::buyer::api::ContentCheck::Skip,
        std::sync::Arc::new(dexdo::seller::ModelsConfig::empty()),
        dexdo::buyer::api::BuyerApiFailurePolicy::default(),
        false,
        "Qwen3-32B".into(),
        Some(dir.join("deals")),
        None,
    );
    wait_for_counter(&chain.place_next_calls, 1, "pending BUY B").await;
    let original = super::load_buyer_submit_journal(&lock.journal_path, &note)
        .unwrap()
        .unwrap();
    // Keep A serviceable above the renewal threshold while the existing monitor recovers B.
    routes
        .replace_active(
            || make_active(dexdo_core::params::BUYER_RENEWAL_THRESHOLD_TOKENS + 1),
            "test-current-has-capacity",
        )
        .await
        .unwrap();
    tokio::time::advance(std::time::Duration::from_secs(
        dexdo_core::params::MATCH_OPEN_TIMEOUT_SECS + 1,
    ))
    .await;
    wait_for_counter(&chain.cleanup_calls, 1, "first pending cleanup").await;
    if mode != "local" {
        let reads = chain.successor_state_reads.load(Ordering::SeqCst);
        chain
            .successor_state_read_failures
            .store(1, Ordering::SeqCst);
        wait_for_counter(
            &chain.successor_state_reads,
            reads + 1,
            "failed cleanup fact read",
        )
        .await;
        assert_eq!(
            super::load_buyer_submit_journal(&lock.journal_path, &note)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
    }
    if mode == "unknown" {
        let reads = chain.successor_state_reads.load(Ordering::SeqCst);
        wait_for_counter(
            &chain.successor_state_reads,
            reads + 1,
            "nonterminal cleanup fact read",
        )
        .await;
        assert_eq!(
            super::load_buyer_submit_journal(&lock.journal_path, &note)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
        *chain.successor_state.lock().unwrap() = None;
    } else if mode == "local" {
        // B's cleanup is already terminal. Failed and nonterminal current-A reads must
        // not discard that proof or turn a local completion retry into another cleanup.
        let reads = chain.successor_state_reads.load(Ordering::SeqCst);
        chain
            .successor_state_read_failures
            .store(1, Ordering::SeqCst);
        wait_for_counter(
            &chain.successor_state_reads,
            reads + 1,
            "failed current-A read",
        )
        .await;
        *chain.successor_state.lock().unwrap() = Some(ready_funded_never_opened_state());
        let reads = chain.successor_state_reads.load(Ordering::SeqCst);
        wait_for_counter(
            &chain.successor_state_reads,
            reads + 1,
            "nonterminal current-A read",
        )
        .await;
        tokio::time::advance(super::buyer_monitor_recovery_backoff()).await;
        assert_eq!(
            super::load_buyer_submit_journal(&lock.journal_path, &note)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(chain.cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
        *chain.successor_state.lock().unwrap() = None;
        chain.cleanup_completion_lock.lock().unwrap().take();
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while lock.journal_path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pending cleanup must complete after facts/persistence recover");
    assert_eq!(
        chain.cleanup_calls.load(Ordering::SeqCst),
        if mode == "refusal" { 2 } else { 1 }
    );
    assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
    assert_eq!(routes.current().await.unwrap().route.token_contract, a);
    let next_chain = RecordingRecoveryChain {
        next_match: Some(issue_2064_tc("6")),
        model_buy_terms: Some(terms),
        expected_journal_path: Some(lock.journal_path.clone()),
        model_buy_failure: Some(ModelBuyFailure::Ambiguous),
        ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
    };
    super::submit_buyer_continuity_next_deal(
        &next_chain,
        buyer.as_ref(),
        None,
        &a,
        Some(&note),
        &super::BuyerSubmitIntent::after(super::BuyerSubmitIntentKind::ContinuityRenewal, &a),
        terms,
    )
    .await
    .unwrap();
    assert_eq!(next_chain.place_next_calls.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_file(&lock.journal_path);
}

#[tokio::test(start_paused = true)]
async fn issue_2064_pending_cleanup_recovers_definite_refusal() {
    issue_2064_pending_cleanup_recovery("refusal").await;
}
#[tokio::test(start_paused = true)]
async fn issue_2064_pending_cleanup_reconciles_unknown_without_repost() {
    issue_2064_pending_cleanup_recovery("unknown").await;
}
#[tokio::test(start_paused = true)]
async fn issue_2064_pending_cleanup_retries_only_local_completion() {
    issue_2064_pending_cleanup_recovery("local").await;
}

#[allow(clippy::await_holding_lock)]
async fn issue_2064_malformed_entrypoint(lazy: bool) {
    use std::sync::atomic::Ordering;
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    for (action, unknown_stop) in [
        ("reclaim", false),
        ("reclaim", true),
        ("fail_closed", false),
        ("dispute", false),
    ] {
        let (dir, _cleanup) = buyer_journal_test_dir("buyer-2064-malformed");
        let note = buyer_submit_test_journal().note_addr;
        let pool = issue_2064_pool(&dir, &note);
        let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
        let lock = super::BuyerMoneyLock::open(&note).unwrap();
        let _ = std::fs::remove_file(&lock.journal_path);
        let b = issue_2064_tc("5");
        let terms = issue_2064_stream_terms();
        let buyer = std::sync::Arc::new(dexdo::buyer::Buyer::generate());
        let chain = std::sync::Arc::new(RecordingRecoveryChain {
            next_match: Some(b.clone()),
            model_buy_terms: Some(terms),
            model_buy_failure: Some(ModelBuyFailure::Ambiguous),
            expected_journal_path: Some(lock.journal_path.clone()),
            stop_ambiguous: unknown_stop,
            handover: std::sync::Mutex::new(Some(vec![1, 2, 3])),
            handover_token_contract: Some(b),
            ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
        });
        let mut args = spot_resume_buyer_args(&dir, &note, "Qwen3-32B");
        args.resume = false;
        args.ticks = terms.ticks;
        args.max_price_per_tick = Some(terms.max_price);
        args.escrow = Some(terms.escrow);
        args.continuity_mode = if lazy {
            super::ContinuityModeArg::OnDemand
        } else {
            super::ContinuityModeArg::Proactive
        };
        let policy_path = args.policy.as_ref().unwrap();
        let mut policy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(policy_path).unwrap()).unwrap();
        policy["buyer"]["on"]["malformed_handover"] = serde_json::json!(action);
        std::fs::write(policy_path, serde_json::to_vec(&policy).unwrap()).unwrap();
        if lazy {
            // On-demand returns recovery to the handler before waiting for an ambiguous BUY.
            // The next initializer entry reconciles that same journal, then sees the bad handover.
            let pending =
                issue_2064_run_entrypoint(true, chain.clone(), buyer.clone(), args.clone())
                    .await
                    .unwrap_err();
            assert!(format!("{pending:#}").contains("durable journal retained"));
            assert!(lock.journal_path.exists());
            assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
            assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
            // Persist the fill through the production match helper before the initializer
            // resumes this matched journal. This fixture's unresolved book poll is unsupported.
            let original = super::load_buyer_submit_journal(&lock.journal_path, &note)
                .unwrap()
                .unwrap();
            super::complete_buyer_submit_with_journal(
                chain.as_ref(),
                original.quoted_order.as_ref(),
                original.ticks,
                original.max_price_per_tick,
                Err(issue_2064_forwarding_error()),
                &note,
                &lock.journal_path,
            )
            .await
            .unwrap();
            assert_eq!(
                super::load_buyer_submit_journal(&lock.journal_path, &note)
                    .unwrap()
                    .unwrap()
                    .submit_identity,
                original.submit_identity
            );
            args.resume = true;
        }
        let error = issue_2064_run_entrypoint(lazy, chain.clone(), buyer.clone(), args)
            .await
            .unwrap_err();
        assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            chain.stop_calls.load(Ordering::SeqCst),
            usize::from(action == "reclaim"),
            "{error:#}"
        );
        if action == "reclaim" && !unknown_stop {
            assert!(
                format!("{error:#}").contains("result=reclaimed"),
                "{error:#}"
            );
            assert!(!lock.journal_path.exists());
            let next = RecordingRecoveryChain {
                next_match: Some(issue_2064_tc("6")),
                model_buy_terms: Some(terms),
                expected_journal_path: Some(lock.journal_path.clone()),
                model_buy_failure: Some(ModelBuyFailure::Ambiguous),
                ..RecordingRecoveryChain::with_deal_state(ready_funded_never_opened_state())
            };
            super::submit_buyer_continuity_next_deal(
                &next,
                buyer.as_ref(),
                None,
                &issue_2064_tc("5"),
                Some(&note),
                &super::BuyerSubmitIntent::after(
                    super::BuyerSubmitIntentKind::PolicyNextSeller,
                    &issue_2064_tc("5"),
                ),
                terms,
            )
            .await
            .unwrap();
            assert_eq!(next.place_next_calls.load(Ordering::SeqCst), 1);
        } else {
            let original = super::load_buyer_submit_journal(&lock.journal_path, &note)
                .unwrap()
                .unwrap();
            if unknown_stop {
                assert!(super::is_ambiguous_submit_error(&error));
            }
            assert!(super::buyer_quote_selection_for_submit(
                chain.as_ref(),
                false,
                Some(&note),
                &super::BuyerSubmitIntent::after(
                    super::BuyerSubmitIntentKind::PolicyNextSeller,
                    &issue_2064_tc("5")
                ),
                None,
                terms.ticks,
                terms.max_price,
                Some(terms.escrow),
                None
            )
            .await
            .is_err());
            assert_eq!(
                super::load_buyer_submit_journal(&lock.journal_path, &note)
                    .unwrap()
                    .unwrap(),
                original
            );
            assert_eq!(chain.place_next_calls.load(Ordering::SeqCst), 1);
        }
        let _ = std::fs::remove_file(&lock.journal_path);
    }
}
#[tokio::test]
async fn issue_2064_normal_malformed_stop_completes_only_confirmed_journal() {
    issue_2064_malformed_entrypoint(false).await;
}
#[tokio::test]
async fn issue_2064_lazy_malformed_stop_completes_only_confirmed_journal() {
    issue_2064_malformed_entrypoint(true).await;
}
