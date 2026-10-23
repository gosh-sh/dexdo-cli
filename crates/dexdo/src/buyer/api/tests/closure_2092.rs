use super::*;
use proptest::prelude::*;

async fn diagnostic(session: &SessionSettle) -> serde_json::Value {
    let response = session
        .closed_rejection("diagnostic-request", false)
        .expect("closed session");
    assert_eq!(response.status(), http::StatusCode::GONE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"].clone()
}

fn session_fixture(chain: Arc<RecordingSettleChain>) -> Arc<SessionSettle> {
    Arc::new(SessionSettle::new(
        chain,
        "tc-diagnostic".to_string(),
        Arc::new(dexdo_core::LocalNote::generate()),
    ))
}

#[tokio::test]
async fn closure_2092_later_refusals_keep_first_cause_on_all_endpoints() {
    let chain = Arc::new(RecordingSettleChain::default());
    let note: Arc<dyn Note> = Arc::new(dexdo_core::LocalNote::generate());
    let session = Arc::new(SessionSettle::new_with_failure_policy(
        chain.clone(),
        "tc-closure-2092".to_string(),
        note.clone(),
        BuyerApiFailurePolicy {
            dead_gateway: DeadGatewayAction::FailClosed,
            empty_stream: EmptyStreamAction::FailClosed,
            ..BuyerApiFailurePolicy::default()
        },
    ));
    session
        .settle_dead_gateway("first-failure", &unchanged_heartbeat())
        .await;
    session
        .settle_empty_stream("later-failure", &unchanged_heartbeat())
        .await;
    let deal = ApiDeal::new(
        Route {
            handover: Handover {
                endpoint: "https://127.0.0.1:1".to_string(),
                tls_fingerprint: "00".repeat(32),
            },
            token_contract: "tc-closure-2092".to_string(),
            max_tokens: dexdo_core::TICK_SIZE.try_into().unwrap(),
        },
        session,
        Arc::new(ContentGate::skip()),
    );
    let state = ApiState::single_deal(
        Arc::new(Buyer::from_note(note)),
        "dexdo-mock".to_string(),
        deal,
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), state, true, async move {
        let _ = shutdown_rx.await;
    })
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let mut first_closure = None;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let input = if path == "/v1/responses" {
            serde_json::json!({"model": "dexdo-mock", "input": "hello", "max_output_tokens": 1})
        } else {
            serde_json::json!({"model": "dexdo-mock", "messages": [{"role": "user", "content": "hello"}], "max_tokens": 1})
        };
        let response = client
            .post(format!("http://{addr}{path}"))
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::GONE);
        let request_id = response.headers()[REQUEST_ID_HEADER]
            .to_str()
            .unwrap()
            .to_string();
        let body: serde_json::Value = response.json().await.unwrap();
        let error = &body["error"];
        assert_eq!(error["code"], "deal_session_closed", "{body}");
        assert_eq!(error["request_id"], request_id);
        assert_eq!(error["token_contract"], "tc-closure-2092");
        assert_eq!(error["closure"]["category"], "dead_gateway");
        assert_eq!(error["closure"]["policy_action"], "fail_closed");
        assert_eq!(error["recovery_state"], "not_attempted");
        assert_eq!(error["recovery_action"], "operator_action");
        if let Some(first) = first_closure.as_ref() {
            assert_eq!(&error["closure"], first);
        } else {
            first_closure = Some(error["closure"].clone());
        }
    }
    // Every refusal is a local read; the test backend panics if any BUY is attempted.
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
    assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
    let _ = shutdown_tx.send(());
    task.await.unwrap();
}

#[tokio::test]
async fn closure_2092_pending_failed_ambiguous_and_confirmed_are_distinct() {
    let chain = Arc::new(RecordingSettleChain::default());
    let session = session_fixture(chain.clone());
    session
        .policy_fail_closed("dead_gateway", "fail_closed", "original failure")
        .await;
    let first = diagnostic(&session).await["closure"].clone();
    let (started, waiting) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
    let pending_session = session.clone();
    let pending = tokio::spawn(async move { pending_session.settle("operator-stop").await });
    waiting.await.unwrap();
    let error = diagnostic(&session).await;
    assert_eq!(error["recovery_state"], "pending");
    assert_eq!(error["recovery_action"], "wait_for_reconciliation");
    assert_eq!(error["closure"], first);
    assert!(error["terminal_action"].is_null());
    chain.fail_stop.store(true, Ordering::SeqCst);
    release.notify_one();
    assert!(pending.await.unwrap().is_err());
    let error = diagnostic(&session).await;
    assert_eq!(error["recovery_state"], "attempted_unconfirmed");
    assert_eq!(error["recovery_action"], "read_only_reconciliation");
    assert!(error["terminal_action"].is_null());
    assert_eq!(error["closure"], first);
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);

    chain.ambiguous_stop.store(true, Ordering::SeqCst);
    assert!(session.settle("operator-retry").await.is_err());
    for _ in 0..3 {
        let error = diagnostic(&session).await;
        assert_eq!(error["recovery_state"], "submission_ambiguous");
        assert_eq!(error["recovery_action"], "read_only_reconciliation");
        assert!(error["terminal_action"].is_null());
        assert_eq!(error["closure"], first);
    }
    assert!(session.settle("suppressed-retry").await.is_err());
    assert_eq!(
        chain.stop_calls.load(Ordering::SeqCst),
        2,
        "diagnosis and ambiguity must not resubmit"
    );
    // An external authoritative reconciliation confirms terminal; it does not claim who closed it.
    session
        .mark_recovered_serialized("terminal-facts-observed")
        .await;
    let error = diagnostic(&session).await;
    assert_eq!(error["recovery_state"], "terminal_confirmed");
    assert_eq!(error["terminal_action"], "observedTerminal");
    assert_eq!(error["recovery_action"], "new_purchase");
    assert_eq!(error["closure"], first);
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 2);

    let confirmed_chain = Arc::new(RecordingSettleChain::default());
    confirmed_chain
        .authoritative_stop
        .store(true, Ordering::SeqCst);
    let confirmed = session_fixture(confirmed_chain.clone());
    assert!(confirmed.settle("operator-stop").await.unwrap());
    let error = diagnostic(&confirmed).await;
    assert_eq!(error["recovery_state"], "terminal_confirmed");
    assert_eq!(error["terminal_action"], "streamStop");
    assert_eq!(confirmed_chain.stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn closure_2092_dispute_is_not_reported_as_a_refund() {
    let chain = Arc::new(RecordingSettleChain::default());
    let session = SessionSettle::new_with_verification_bail_action(
        chain.clone(),
        "tc-dispute".to_string(),
        Arc::new(dexdo_core::LocalNote::generate()),
        VerificationBailAction::Dispute,
    );
    chain.fail_dispute.store(true, Ordering::SeqCst);
    assert!(!session.settle_verification_bail("bad-output").await);
    let first = diagnostic(&session).await;
    assert_eq!(first["recovery_state"], "attempted_unconfirmed");
    assert!(first["terminal_action"].is_null());
    chain.fail_dispute.store(false, Ordering::SeqCst);
    assert!(session.settle_verification_bail("bad-output").await);
    let error = diagnostic(&session).await;
    assert_eq!(error["closure"], first["closure"]);
    assert_eq!(error["recovery_state"], "disputed");
    assert_eq!(error["recovery_action"], "operator_action");
    assert_eq!(error["terminal_action"], "streamDispute");
    assert!(error.get("refund").is_none());
    assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn closure_2092_only_planned_preservation_advises_resuming() {
    let chain = Arc::new(RecordingSettleChain::default());
    for planned in [true, false] {
        let session = SessionSettle::new_with_failure_policy_and_lifetime(
            chain.clone(),
            "tc-preserved".to_string(),
            Arc::new(dexdo_core::LocalNote::generate()),
            BuyerApiFailurePolicy::default(),
            SessionLifetimePolicy::Preserve,
        );
        if planned {
            assert!(!session.settle_on_exit("shutdown").await.unwrap());
        } else {
            assert!(!session.settle_verification_bail("bad-output").await);
        }
        let error = diagnostic(&session).await;
        assert_eq!(error["recovery_state"], "not_attempted");
        assert_eq!(
            error["closure"]["category"],
            if planned {
                "planned_shutdown"
            } else {
                "verification_bail"
            }
        );
        assert_eq!(
            error["recovery_action"],
            if planned {
                "resume_same_deal"
            } else {
                "operator_action"
            }
        );
        assert!(error["terminal_action"].is_null());
    }
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn closure_2092_cancelled_recovery_does_not_leave_stale_closure_or_pending_state() {
    let chain = Arc::new(RecordingSettleChain::default());
    let note: Arc<dyn Note> = Arc::new(dexdo_core::LocalNote::generate());
    let deal = recovery_test_deal("tc-cancelled", chain.clone(), note);
    *chain.heartbeat_during_reclaim_preflight.lock().unwrap() = Some(deal.clone());
    let heartbeat = deal.accepted_output_guard();
    assert!(
        !deal
            .session
            .settle_dead_gateway("old-heartbeat", &heartbeat)
            .await
    );
    assert!(!deal.session.is_closed());
    assert!(deal
        .session
        .closed_rejection("after-cancellation", false)
        .is_none());
    {
        let diagnostics = deal.session.diagnostics.lock().unwrap();
        assert!(diagnostics.closure.is_none());
        assert_eq!(diagnostics.recovery_state, RecoveryState::NotAttempted);
    }
    deal.session
        .policy_fail_closed("empty_stream", "fail_closed", "new-incident")
        .await;
    let error = diagnostic(&deal.session).await;
    assert_eq!(error["closure"]["category"], "empty_stream");
    assert_eq!(error["recovery_state"], "not_attempted");
    assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
    deal.session.disable_drop_backup();
}

#[tokio::test]
async fn closure_2092_requests_on_dev_keep_cause_after_a_real_stream_incident() {
    let harness = weekly_route_harness_gated_with_policy(WeeklyRouteScenario {
        claimed: 0,
        subscription: false,
        expires_in: SUB_WEEK_LEN.as_secs(),
        upstream_tokens: UNCONSTRAINED_UPSTREAM,
        content_gate: ContentGate::skip(),
        upstream: crate::seller::UpstreamConfig::Mock,
        failure_policy: BuyerApiFailurePolicy {
            empty_stream: EmptyStreamAction::FailClosed,
            ..BuyerApiFailurePolicy::default()
        },
        lifetime: SessionLifetimePolicy::SettleOnExit,
    })
    .await;
    for (path, output, prompt) in [
        ("/v1/chat/completions", 1, "short".to_string()),
        ("/v1/responses", 64, "long ".repeat(64)),
    ] {
        let (status, body) = harness.ask_full(path, output, &prompt, false).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    }
    let (status, body) = harness
        .ask_full("/v1/messages", 8, "DEXDO_FIXTURE_REASONING_ONLY", false)
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
    let (status, body) = harness
        .ask_full("/v1/chat/completions", 1, "after failure", false)
        .await;
    assert_eq!(status, reqwest::StatusCode::GONE, "{body}");
    let first: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(first["error"]["closure"]["category"], "empty_stream");
    assert_eq!(first["error"]["recovery_state"], "not_attempted");
    let requests = ["/v1/chat/completions", "/v1/responses", "/v1/messages"]
        .map(|path| harness.ask_full(path, 64, "concurrent retry", true));
    for (status, body) in futures::future::join_all(requests).await {
        assert_eq!(status, reqwest::StatusCode::GONE, "{body}");
        let later: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(later["error"]["closure"], first["error"]["closure"]);
    }
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    assert_eq!(harness.chain.dispute_calls(), 0);
    harness.shutdown().await;
}

#[tokio::test]
async fn closure_2092_existing_chain_read_distinguishes_unopened_terminal_and_disputed() {
    for (terminal, disputed, category, recovery, action) in [
        (
            false,
            false,
            "deal_not_open",
            "not_attempted",
            "operator_action",
        ),
        (
            true,
            false,
            "terminal_observed",
            "terminal_confirmed",
            "new_purchase",
        ),
        (false, true, "deal_not_open", "disputed", "operator_action"),
    ] {
        let chain = Arc::new(RecordingSettleChain::default());
        let mut state =
            never_opened_deal_state(if terminal { 0 } else { TEST_DEPOSIT }, unix_now_secs());
        state.disputed = disputed;
        chain.set_deal_state(state);
        let session = session_fixture(chain.clone());
        assert!(session.ensure_open_for_serving().await.is_err());
        let error = diagnostic(&session).await;
        assert_eq!(error["closure"]["category"], category);
        assert_eq!(error["recovery_state"], recovery);
        assert_eq!(error["recovery_action"], action);
        if terminal {
            assert_eq!(error["terminal_action"], "observedTerminal");
        } else {
            assert!(error["terminal_action"].is_null());
        }
        assert!(
            !session.is_settled(),
            "diagnostics must not change the existing settlement gate"
        );
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
        session.disable_drop_backup();
    }
    let chain = Arc::new(RecordingSettleChain::default());
    let session = session_fixture(chain.clone());
    assert!(session.ensure_open_for_serving().await.is_err());
    let error = diagnostic(&session).await;
    assert_eq!(error["closure"]["category"], "chain_state_unavailable");
    assert_eq!(error["recovery_state"], "not_attempted");
    assert!(error["terminal_action"].is_null());
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    session.disable_drop_backup();
}

#[test]
fn closure_2092_concurrent_first_closures_have_one_causal_identity() {
    let session = session_fixture(Arc::new(RecordingSettleChain::default()));
    let barrier = std::sync::Barrier::new(2);
    let winner = std::thread::scope(|scope| {
        let attempts: Vec<_> = [
            ClosureCategory::DeadGateway,
            ClosureCategory::VerificationBail,
        ]
        .into_iter()
        .map(|category| {
            let session = session.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                session
                    .close_local_api(category, Some("stop"))
                    .then_some(category)
            })
        })
        .collect();
        let winners: Vec<_> = attempts
            .into_iter()
            .filter_map(|attempt| attempt.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1);
        winners[0]
    });
    let diagnostics = session.diagnostics.lock().unwrap();
    let closure = diagnostics.closure.as_ref().unwrap();
    assert_eq!(closure.category, winner);
    assert!(!closure.closure_id.is_empty());
    drop(diagnostics);
    session.disable_drop_backup();
}

proptest! {
    #[test]
    fn closure_2092_first_cause_is_invariant(later in prop::collection::vec(any::<bool>(), 0..32)) {
        let session = session_fixture(Arc::new(RecordingSettleChain::default()));
        session.close_local_api(ClosureCategory::DeadGateway, Some("fail_closed"));
        let first = session.diagnostics.lock().unwrap().closure.clone();
        for verification in later {
            let category = if verification {ClosureCategory::VerificationBail} else {ClosureCategory::EmptyStream};
            prop_assert!(!session.close_local_api(category, Some("stop")));
            prop_assert_eq!(&session.diagnostics.lock().unwrap().closure, &first);
        }
        session.disable_drop_backup();
    }
}

// Regression sequences from PR2105 review: confirmed facts survive attempt bookkeeping.
fn monetary_calls(chain: &RecordingSettleChain) -> [usize; 4] {
    [
        chain.stop_calls.load(Ordering::SeqCst),
        chain.dispute_calls.load(Ordering::SeqCst),
        chain.recovery_stop_calls.load(Ordering::SeqCst),
        chain.cleanup_unopened_calls.load(Ordering::SeqCst),
    ]
}

async fn read_only_diagnostic(
    session: &SessionSettle,
    chain: &RecordingSettleChain,
) -> serde_json::Value {
    let calls = monetary_calls(chain);
    let result = diagnostic(session).await;
    assert_eq!(
        monetary_calls(chain),
        calls,
        "diagnosis must not submit a monetary operation"
    );
    result
}

fn assert_known_fact(error: &serde_json::Value, disputed: bool) {
    assert_eq!(
        error["recovery_state"],
        if disputed {
            "disputed"
        } else {
            "terminal_confirmed"
        }
    );
    assert_eq!(
        error["recovery_action"],
        if disputed {
            "operator_action"
        } else {
            "new_purchase"
        }
    );
    assert!(error.get("refund").is_none());
    if disputed {
        assert!(error["terminal_action"].is_null() || error["terminal_action"] == "streamDispute");
    } else {
        assert_ne!(error["terminal_action"], "streamDispute");
    }
}

async fn closure_2092_observed_fact_survives_failed_recovery(disputed: bool) {
    let chain = Arc::new(RecordingSettleChain::default());
    let mut state =
        never_opened_deal_state(if disputed { TEST_DEPOSIT } else { 0 }, unix_now_secs());
    state.disputed = disputed;
    chain.set_deal_state(state);
    let session = session_fixture(chain.clone());
    assert!(session.ensure_open_for_serving().await.is_err());
    let before = read_only_diagnostic(&session, &chain).await;
    let expected = if disputed {
        "disputed"
    } else {
        "terminal_confirmed"
    };
    assert_eq!(before["recovery_state"], expected);

    chain.fail_stop.store(true, Ordering::SeqCst);
    let (started, waiting) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
    let pending_session = session.clone();
    let pending = tokio::spawn(async move {
        pending_session
            .settle("retry-after-observed-chain-fact")
            .await
    });
    waiting.await.unwrap();
    let during = read_only_diagnostic(&session, &chain).await;
    assert_known_fact(&during, disputed);
    assert_eq!(during["closure"], before["closure"]);
    release.notify_one();
    assert!(pending.await.unwrap().is_err());
    let after = read_only_diagnostic(&session, &chain).await;
    session.disable_drop_backup();
    assert_known_fact(&after, disputed);
    assert_eq!(after["closure"], before["closure"]);
    assert_eq!(
        after["recovery_state"], expected,
        "a failed recovery attempt must not erase an authoritative fact; before={before}, after={after}"
    );
    assert!(!session.is_settled());
    assert_eq!(monetary_calls(&chain), [1, 0, 0, 0]);
}

#[tokio::test]
async fn closure_2092_observed_terminal_survives_failed_recovery() {
    closure_2092_observed_fact_survives_failed_recovery(false).await;
}

#[tokio::test]
async fn closure_2092_observed_dispute_survives_failed_recovery() {
    closure_2092_observed_fact_survives_failed_recovery(true).await;
}

#[tokio::test]
async fn closure_2092_fresh_terminal_fact_overrides_historical_dispute() {
    let chain = Arc::new(RecordingSettleChain::default());
    let session = SessionSettle::new_with_verification_bail_action(
        chain.clone(),
        "tc-resolved-dispute".to_string(),
        Arc::new(dexdo_core::LocalNote::generate()),
        VerificationBailAction::Dispute,
    );
    assert!(session.settle_verification_bail("bad-output").await);
    let before = read_only_diagnostic(&session, &chain).await;
    assert_eq!(before["recovery_state"], "disputed");

    chain.set_deal_state(never_opened_deal_state(0, unix_now_secs()));
    assert!(session.ensure_open_for_serving().await.is_err());
    let after = read_only_diagnostic(&session, &chain).await;
    session.disable_drop_backup();
    assert_eq!(after["closure"], before["closure"]);
    assert_eq!(
        after["recovery_state"], "terminal_confirmed",
        "newer strict terminal chain facts must not be hidden by historical streamDispute; after={after}"
    );
    assert_eq!(after["recovery_action"], "new_purchase");
    assert_eq!(after["terminal_action"], "observedTerminal");
    assert_eq!(
        session.terminal_action(),
        Some(SessionTerminalAction::StreamDispute)
    );
    assert!(session.is_settled());
    assert_eq!(monetary_calls(&chain), [0, 1, 0, 0]);
}

async fn closure_2092_fresh_chain_fact_overrides_ambiguous_attempt(disputed: bool) {
    let chain = Arc::new(RecordingSettleChain::default());
    chain.ambiguous_stop.store(true, Ordering::SeqCst);
    let session = session_fixture(chain.clone());
    assert!(session.settle("ambiguous-stop").await.is_err());
    let before = read_only_diagnostic(&session, &chain).await;
    assert_eq!(before["recovery_state"], "submission_ambiguous");

    let mut state =
        never_opened_deal_state(if disputed { TEST_DEPOSIT } else { 0 }, unix_now_secs());
    state.disputed = disputed;
    chain.set_deal_state(state);
    assert!(session.ensure_open_for_serving().await.is_err());
    let after = read_only_diagnostic(&session, &chain).await;
    session.disable_drop_backup();
    assert_eq!(after["closure"], before["closure"]);
    assert_eq!(
            after["recovery_state"],
            if disputed { "disputed" } else { "terminal_confirmed" },
            "authoritative chain state must not be hidden by an earlier ambiguous attempt; after={after}"
        );
    assert_known_fact(&after, disputed);
    assert!(session.recovery_submit_may_have_landed(RecoveryKind::ReclaimOpened));
    assert!(!session.is_settled());
    assert!(session.settle("still-no-automatic-resubmit").await.is_err());
    assert_eq!(monetary_calls(&chain), [1, 0, 0, 0]);
}

#[tokio::test]
async fn closure_2092_fresh_terminal_fact_overrides_ambiguous_attempt() {
    closure_2092_fresh_chain_fact_overrides_ambiguous_attempt(false).await;
}

#[tokio::test]
async fn closure_2092_fresh_disputed_fact_overrides_ambiguous_attempt() {
    closure_2092_fresh_chain_fact_overrides_ambiguous_attempt(true).await;
}

async fn cancelled_attempt_keeps_newer_fact(disputed: bool) {
    let chain = Arc::new(RecordingSettleChain::default());
    let session = session_fixture(chain.clone());
    session.close_local_api(ClosureCategory::DeadGateway, Some("fail_closed"));
    assert!(session.begin_recovery_attempt(
        RecoveryKind::ReclaimOpened,
        true,
        ClosureCategory::Recovery,
        None,
    ));
    let previous = session.recovery_pending();
    let mut state =
        never_opened_deal_state(if disputed { TEST_DEPOSIT } else { 0 }, unix_now_secs());
    state.disputed = disputed;
    chain.set_deal_state(state);
    assert!(session.ensure_open_for_serving().await.is_err());
    let before = read_only_diagnostic(&session, &chain).await;
    assert_known_fact(&before, disputed);

    session.cancel_recovery_without_post(RecoveryKind::ReclaimOpened, previous);
    let after = read_only_diagnostic(&session, &chain).await;
    session.disable_drop_backup();
    assert_eq!(after["closure"], before["closure"]);
    assert_eq!(
        after["recovery_state"],
        if disputed {
            "disputed"
        } else {
            "terminal_confirmed"
        },
        "cancellation must not restore a snapshot older than known chain facts; after={after}"
    );
    assert_known_fact(&after, disputed);
    assert!(!session.is_settled());
    assert!(session.is_closed());
    assert_eq!(monetary_calls(&chain), [0, 0, 0, 0]);
}

#[tokio::test]
async fn closure_2092_cancelled_attempt_keeps_newer_terminal_fact() {
    cancelled_attempt_keeps_newer_fact(false).await;
}

#[tokio::test]
async fn closure_2092_cancelled_attempt_keeps_newer_disputed_fact() {
    cancelled_attempt_keeps_newer_fact(true).await;
}

async fn fact_observed_during_failed_attempt(disputed: bool) {
    let chain = Arc::new(RecordingSettleChain::default());
    chain.fail_stop.store(true, Ordering::SeqCst);
    let session = session_fixture(chain.clone());
    let (started, waiting) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    *chain.stop_gate.lock().unwrap() = Some((started, release.clone()));
    let pending_session = session.clone();
    let pending = tokio::spawn(async move { pending_session.settle("pending-stop").await });
    waiting.await.unwrap();
    let first = read_only_diagnostic(&session, &chain).await;
    assert_eq!(first["recovery_state"], "pending");

    let mut state =
        never_opened_deal_state(if disputed { TEST_DEPOSIT } else { 0 }, unix_now_secs());
    state.disputed = disputed;
    chain.set_deal_state(state);
    // The existing read records its fact, then waits for the failed STOP's settlement lock.
    let mut read = Box::pin(session.ensure_open_for_serving());
    assert!(futures::poll!(&mut read).is_pending());
    let during = read_only_diagnostic(&session, &chain).await;
    assert_known_fact(&during, disputed);
    assert_eq!(during["closure"], first["closure"]);

    release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(read.await.is_err());
    let after = read_only_diagnostic(&session, &chain).await;
    assert_known_fact(&after, disputed);
    assert_eq!(after["closure"], first["closure"]);
    assert!(!session.is_settled());
    assert_eq!(monetary_calls(&chain), [1, 0, 0, 0]);
    session.disable_drop_backup();
}

#[tokio::test]
async fn closure_2092_terminal_fact_observed_during_failed_attempt_survives() {
    fact_observed_during_failed_attempt(false).await;
}

#[tokio::test]
async fn closure_2092_disputed_fact_observed_during_failed_attempt_survives() {
    fact_observed_during_failed_attempt(true).await;
}

#[tokio::test]
async fn closure_2092_successful_stop_resolves_an_observed_dispute() {
    let chain = Arc::new(RecordingSettleChain::default());
    let mut state = never_opened_deal_state(TEST_DEPOSIT, unix_now_secs());
    state.disputed = true;
    chain.set_deal_state(state);
    let session = session_fixture(chain.clone());
    assert!(session.ensure_open_for_serving().await.is_err());
    let before = read_only_diagnostic(&session, &chain).await;
    assert_known_fact(&before, true);

    chain.authoritative_stop.store(true, Ordering::SeqCst);
    assert!(session.settle("confirmed-stop").await.unwrap());
    let after = read_only_diagnostic(&session, &chain).await;
    assert_known_fact(&after, false);
    assert_eq!(after["closure"], before["closure"]);
    assert_eq!(after["terminal_action"], "streamStop");
    // The fixture still exposes the pre-STOP dispute, like a delayed concurrent chain read.
    assert!(session.ensure_open_for_serving().await.is_err());
    let delayed = read_only_diagnostic(&session, &chain).await;
    assert_known_fact(&delayed, false);
    assert_eq!(delayed["closure"], before["closure"]);
    assert_eq!(delayed["terminal_action"], "streamStop");
    assert_eq!(monetary_calls(&chain), [1, 0, 0, 0]);
}
