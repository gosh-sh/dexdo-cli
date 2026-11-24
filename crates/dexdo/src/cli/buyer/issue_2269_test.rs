#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn issue_2269_resume_clears_legacy_usage_and_serves_the_existing_deal() {
    use dexdo_core::ChainBackend as _;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let (dir, _cleanup) = buyer_journal_test_dir("buyer-2269-model-resume");
    let note_addr = format!("0:2260{}1", "0".repeat(59));
    let tc = subscription_test_tc('b');
    let model = "Qwen3-32B";
    let request = "req-dexdo-1-00000000000000000000000000002260";
    let pool = write_spot_resume_pool(&dir, &note_addr);
    let _env = EnvVarGuard::set("DEXDO_PN_POOL", pool.as_os_str());
    let mut args = spot_resume_buyer_args(&dir, &note_addr, model);
    let deals_dir = args.deals_dir.as_ref().unwrap();
    std::fs::create_dir_all(deals_dir).unwrap();
    write_spot_resume_deal_handle(deals_dir, &note_addr, model, &tc);

    let manifest =
        dexdo_core::Deployed::load(crate::cli::commands::manifest_path().unwrap()).unwrap();
    let registry_path = dir.join("registry.json");
    std::fs::write(
        &registry_path,
        serde_json::json!({
            "schema": "dexdo.model_registry_validation.v1",
            "registry": {"network": manifest.network},
            "seller": {"check_model_registry": false, "deploy_missing_order_book": false},
            "buyer": {"check_model_registry": true}
        })
        .to_string(),
    )
    .unwrap();
    args.registry.model_registry_validation = Some(registry_path);
    args.mock.mock_model = true;

    let note = Arc::new(dexdo_core::LocalNote::generate());
    let buyer_pubkey = dexdo_core::Note::pubkey(note.as_ref());
    let seller = dexdo::seller::start_gateway("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let handover = dexdo_core::Handover {
        endpoint: format!("https://{}", seller.listen_addr),
        tls_fingerprint: seller.tls_fingerprint.clone(),
    };
    let register_deal = |tc: &str| {
        let (state, deal) = ordinary_gateway_snapshot(SPOT_RESUME_TEST_TICKS as u64);
        seller
            .state
            .register_stream(tc, buyer_pubkey.clone(), 2, state, deal)
            .unwrap();
        Arc::new(SpotHandleResumeChain::new(
            tc,
            seller
                .note
                .encrypt_to(&buyer_pubkey, &handover.to_deal_bytes(tc)),
            Some(42),
        ))
    };
    let chain = register_deal(&tc);
    let usage_path = dir.join("usage.sqlite3");
    let store = dexdo::buyer::usage::UsageStore::open(&usage_path, chain.network()).unwrap();
    store.start_request(request, model, "openai", true).unwrap();
    store.link_deal(request, &tc).unwrap();
    store
        .record_call_usage(
            request,
            "verification_b8",
            "verification_b8",
            dexdo_proto::BillingUsage::new(1, 1, None).unwrap(),
            true,
        )
        .unwrap();
    store
        .finish_request(request, 499, Some("cancelled"), Some("consumer_disconnect"))
        .unwrap();
    store
        .record_call_usage(
            request,
            "answer",
            "in_progress",
            dexdo_proto::BillingUsage::default(),
            false,
        )
        .unwrap();
    drop(store);
    let database = rusqlite::Connection::open(&usage_path).unwrap();
    database.execute_batch(include_str!("../../buyer/drop_usage_change_journal.sql")).unwrap();
    database
        .execute_batch(
            "CREATE TABLE request_usage_recovery (request_id TEXT);
         INSERT INTO request_usage_recovery VALUES ('req-dexdo-1-00000000000000000000000000002260');
         PRAGMA user_version=9;",
        )
        .unwrap();
    drop(database);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let (writer, events) = crate::cli::machine::BuyerEventWriter::capturing();
    let command_chain = chain.clone();
    let command_note = note.clone();
    let command = tokio::spawn(super::BUYER_USAGE_PATH_FOR_TEST.scope(
        usage_path.clone(),
        async move {
            super::run_buyer_inner(
                args,
                &mut Some(writer),
                &mut super::BuyerMachineErrorContext::default(),
                super::BuyerCommandRuntime {
                    backend: Some((command_chain, command_note)),
                    chain_preflight: super::BuyerChainPreflight::OfflineTest,
                    shutdown: Box::pin(async move {
                        let _ = stopped.await;
                    }),
                },
            )
            .await
        },
    ));
    let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(event) = events
                .lock()
                .unwrap()
                .iter()
                .find(|event| event["event"] == "endpoint_ready")
                .cloned()
            {
                return Some(event);
            }
            if command.is_finished() {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let Some(ready) = ready else {
        panic!(
            "resume ended before its endpoint was ready: {:?}",
            command.await
        );
    };
    assert_eq!(ready["token_contract"], tc);
    let address = ready["bind_addr"].as_str().unwrap();
    let response = client
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": model, "messages": [{"role": "user", "content": "resume"}],
            "max_tokens": 1, "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("[DONE]"), "{body}");
    let database = rusqlite::Connection::open(&usage_path).unwrap();
    let marker: (bool, String) = database
        .query_row(
            "SELECT u.accepted,r.failure_stage FROM request_usage u
         JOIN requests r USING(network,request_id)
         WHERE u.request_id=?1 AND u.call_id='in_progress'",
            [request],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(marker, (true, "consumer_disconnect".into()));
    let store = dexdo::buyer::usage::UsageStore::open(&usage_path, chain.network()).unwrap();
    assert!(
        store.accepted_tokens(&tc).unwrap() > 2,
        "confirmed old and new work remains"
    );
    assert_eq!(chain.money_posts.load(Ordering::SeqCst), 0);
    assert_eq!(chain.stop_posts.load(Ordering::SeqCst), 0);
    shutdown.send(()).unwrap();
    command.await.unwrap().unwrap();
    seller.server_task.abort();
}

#[tokio::test(start_paused = true)]
async fn issue_2269_idle_deal_end_monitor_persists_closed_quota_before_exit() {
    use dexdo::buyer::api::{self, ApiDeal, ApiState, ContentGate, Route, SessionSettle};
    use std::sync::{atomic::Ordering, Arc};

    let opened = dexdo_core::DealChainState {
        opened: true,
        probe_accepted: true,
        ..ready_funded_never_opened_state()
    };
    for destroyed in [false, true] {
        let tc = "tc-idle-exhausted";
        let dir = tempfile::tempdir().unwrap();
        let usage = Arc::new(
            dexdo::buyer::usage::UsageStore::open(dir.path().join("usage.sqlite3"), "mock")
                .unwrap(),
        );
        let chain = Arc::new(RecordingRecoveryChain::with_monitor_deal_state(opened));
        let buyer = Arc::new(dexdo::buyer::Buyer::generate());
        let session = Arc::new(SessionSettle::new_with_failure_policy_and_lifetime(
            chain.clone(),
            tc.into(),
            buyer.note.clone(),
            api::BuyerApiFailurePolicy::default(),
            api::SessionLifetimePolicy::Preserve,
        ));
        let deal = ApiDeal::new(
            Route {
                token_contract: tc.into(),
                handover: dexdo_core::Handover {
                    endpoint: String::new(),
                    tls_fingerprint: String::new(),
                },
                max_tokens: dexdo_core::TICK_SIZE as u64,
            },
            session.clone(),
            Arc::new(ContentGate::skip()),
        );
        let state = ApiState::single_deal(buyer, "fixture".into(), deal).with_usage(usage.clone());
        let deals = state.deals.clone();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let (_, server) = api::serve("127.0.0.1:0".parse().unwrap(), state, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
        let quota = || usage.admission_snapshot("fixture").unwrap()["quota"].clone();
        assert_ne!(quota()["remaining"], "0");
        let monitor = super::spawn_buyer_deal_end_watch(chain.clone(), deals);
        *chain.monitor_deal_state.lock().unwrap() = if destroyed {
            None
        } else {
            Some(dexdo_core::DealChainState {
                opened: false,
                deposit: 0,
                tokens_final: dexdo_core::TICK_SIZE,
                tokens_pending: dexdo_core::TICK_SIZE,
                ..opened
            })
        };
        assert!(
            matches!(monitor.await.unwrap(), super::BuyerRunEnd::DealEnded { token_contract } if token_contract == tc)
        );
        assert!(session.is_closed());
        assert_eq!(quota()["remaining"], "0");
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}
