fn issue_2030_producer_server(
    listener: tokio::net::TcpListener,
    replies: Vec<(u16, Value)>,
) -> (Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    let task = tokio::spawn(async move {
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let (_, body_text) = request.split_once("\r\n\r\n").unwrap();
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_str(body_text).unwrap());
            let body = body.to_string();
            socket.write_all(format!(
                "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            ).as_bytes()).await.unwrap();
        }
    });
    (bodies, task)
}

fn issue_2030_wrong_producer(target: &str) -> Value {
    json!({"error":{"code":"WRONG_PRODUCER","message":"Resend message to the active Block Producer",
        "data":{"producers":[target]}}})
}

fn issue_2030_backend(endpoint: &str) -> RealChainBackend {
    let deployment = deployed("");
    RealChainBackend {
        client: LimitedChainClient::new(
            ChainClient::connect(endpoint).unwrap(),
            ChainRequestCeiling::Unlimited,
        ),
        http: reqwest::Client::new(),
        money_post_http: build_money_post_http_client().unwrap(),
        superroot: Address::parse(&deployment.superroot).unwrap(),
        deployed: deployment,
    }
}

#[tokio::test]
async fn issue_2030_review_getters_route_both_metered_and_raw_system_contracts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, task) = issue_2030_producer_server(
        listener,
        (0..4)
            .map(|_| {
                (
                    200,
                    json!({"data":{"blockchain":{"account":{"info":null}}}}),
                )
            })
            .collect(),
    );
    let backend = issue_2030_backend(&endpoint);
    let account = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    for (abi, method) in [
        (PRIVATENOTE_ABI, "getDetails"),
        (TOKENCONTRACT_ABI, "getState"),
    ] {
        assert!(backend
            .run_getter(&account, abi, method, json!({}))
            .await
            .unwrap()
            .is_none());
    }
    for (abi, method) in [
        (
            crate::private_note::artifacts::PRIVATE_NOTE_ABI_JSON,
            "getDetails",
        ),
        (TOKENCONTRACT_ABI, "getState"),
    ] {
        assert!(backend
            .client()
            .run_getter_retrying(&account, abi, method, json!({}))
            .await
            .unwrap()
            .is_none());
    }
    task.await.unwrap();
    for request in requests.lock().unwrap().iter() {
        let query = request["query"].as_str().unwrap();
        assert!(
            query.contains(&format!("dapp_id: \"{}\"", crate::address::DEXDO_DAPP_ID)),
            "{query}"
        );
        assert!(
            !query.contains(&format!("dapp_id: \"{}\"", account.bare())),
            "{query}"
        );
    }
}

#[tokio::test]
async fn issue_2030_review_activation_and_gas_reads_use_system_dapp() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, task) = issue_2030_producer_server(listener, (0..6).map(|_| (
        200, json!({"data":{"blockchain":{"account":{"info":{
            "acc_type_name":"Active", "balance":"10", "balance_other":[{"currency":2,"value":"20"}]
        }}}}}),
    )).collect());
    let backend = issue_2030_backend(&endpoint);
    let account = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    assert!(
        backend
            .wait_active(
                &account,
                crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS,
                true
            )
            .await
    );
    assert_eq!(backend.active_deal_reserve_ecc(&account).await.unwrap(), 0x20);
    assert!(backend
        .client()
        .get_system_account_retrying(&account)
        .await
        .unwrap()
        .unwrap()
        .is_active());
    assert!(!backend.token_contract_is_retired(&account).await.unwrap());
    assert!(backend.account_active_code_hash(&account, true).await.unwrap().0);
    backend
        .token_contract_code_hash_account_check("fixture TC", &account, &"a".repeat(64))
        .await
        .unwrap();
    task.await.unwrap();
    for request in requests.lock().unwrap().iter() {
        assert!(request["query"]
            .as_str()
            .unwrap()
            .contains(&format!("dapp_id: \"{}\"", crate::address::DEXDO_DAPP_ID)));
    }
}

#[tokio::test]
async fn issue_2030_review_other_contract_routing_is_preserved() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, task) = issue_2030_producer_server(
        listener,
        (0..4)
            .map(|_| {
                (
                    200,
                    json!({"data":{"blockchain":{"account":{"info":null}}}}),
                )
            })
            .collect(),
    );
    let backend = issue_2030_backend(&endpoint);
    let account = Address::parse(&format!("0:{}", "b".repeat(64))).unwrap();
    assert!(backend
        .run_getter(&account, ROOTMODEL_ABI, "getOwnerPubkey", json!({}))
        .await
        .unwrap()
        .is_none());
    assert!(backend
        .client()
        .run_getter_retrying(&account, ROOTMODEL_ABI, "getOwnerPubkey", json!({}))
        .await
        .unwrap()
        .is_none());
    assert!(backend
        .get_account_retrying(&account)
        .await
        .unwrap()
        .is_none());
    assert!(!backend.account_active_code_hash(&account, false).await.unwrap().0);
    task.await.unwrap();
    for request in requests.lock().unwrap().iter() {
        assert!(request["query"]
            .as_str()
            .unwrap()
            .contains(&format!("dapp_id: \"{}\"", account.bare())));
    }
}

#[tokio::test]
async fn issue_2030_redirect_rereads_each_target_and_preserves_signed_identity() {
    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let third = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", first.local_addr().unwrap());
    let second_addr = second.local_addr().unwrap().to_string();
    let third_addr = third.local_addr().unwrap().to_string();
    let embedded = json!({"error":{"code":"INTERNAL_ERROR","message":format!(
        "Block Producer returned non-success HTTP status 400 Bad Request: {}",
        issue_2030_wrong_producer(&second_addr)
    )}});
    let (a, ta) = issue_2030_producer_server(first, vec![(200, embedded)]);
    let (b, tb) =
        issue_2030_producer_server(second, vec![(400, issue_2030_wrong_producer(&third_addr))]);
    let (c, tc) = issue_2030_producer_server(third, vec![(200, json!({"result":{}}))]);
    let account = "1".repeat(64);
    send_message_routed_checked(
        &build_money_post_http_client().unwrap(),
        &endpoint,
        "original-signed-boc",
        &account,
        crate::address::DEXDO_DAPP_ID,
        None,
    )
    .await
    .unwrap();
    ta.await.unwrap();
    tb.await.unwrap();
    tc.await.unwrap();
    let original = a.lock().unwrap()[0].clone();
    assert_eq!(
        b.lock().unwrap().as_slice(),
        std::slice::from_ref(&original)
    );
    assert_eq!(c.lock().unwrap().as_slice(), &[original]);
}

#[tokio::test]
async fn issue_2030_redirect_budget_is_bounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let endpoint = format!("http://{addr}");
    let (bodies, task) = issue_2030_producer_server(
        listener,
        (0..=crate::params::PRODUCER_REDIRECT_MAX_HOPS)
            .map(|_| (400, issue_2030_wrong_producer(&addr)))
            .collect(),
    );
    let error = send_message_routed_checked(
        &build_money_post_http_client().unwrap(),
        &endpoint,
        "original-signed-boc",
        &"1".repeat(64),
        crate::address::DEXDO_DAPP_ID,
        None,
    )
    .await
    .unwrap_err();
    task.await.unwrap();
    assert!(is_producer_open_failure(&error));
    assert!(!is_transient_transport_failure(&error));
    assert!(!is_transient_submit_failure(&error));
    assert_eq!(
        bodies.lock().unwrap().len(),
        crate::params::PRODUCER_REDIRECT_MAX_HOPS as usize + 1
    );
}

#[test]
fn issue_2030_reported_timeout_is_open_retryable_without_widening_money_retry() {
    let error =
        checked_submit_response(serde_json::from_str(FORWARDING_TIMEOUT).unwrap()).unwrap_err();
    assert!(is_producer_open_failure(&error));
    assert!(!is_transient_transport_failure(&error));
    assert!(!is_transient_submit_failure(&error));
    for response in [
        json!({"error":{"code":"INTERNAL_ERROR","message":"operation timed out"}}),
        json!({"result":{"exit_code":151}}),
        json!({"error":{"code":"TVM_ERROR","data":{"producers":["elsewhere"]}}}),
    ] {
        assert!(!is_producer_open_failure(
            &checked_submit_response(response).unwrap_err()
        ));
    }
}

#[test]
fn issue_2030_newer_sdk_producer_observation_wins() {
    let embedded = json!({"error":{"code":"WRONG_PRODUCER","data":{
        "producers":["older.mainbk.ackinacki.org"], "current_time":1787625858647u64
    }}});
    let response = json!({"error":{"code":"INTERNAL_ERROR",
    "message":format!("Block Producer returned non-success HTTP status 400 Bad Request: {embedded}"),
    "data":{"tvm_sdk_error":{"data":{
        "producers":["newer.mainbk.ackinacki.org"], "current_time":1787625858651u64
    }}}}});
    let data = producer_redirect_data(&response).unwrap();
    assert_eq!(data["producers"][0], "newer.mainbk.ackinacki.org");
}

#[test]
fn issue_2030_malformed_redirects_are_not_posted() {
    for target in [
        "ftp://example.org",
        "http://example.org",
        "https://user:secret@example.org",
        "https://example.org/other",
        "https://example.org/?query",
        "https://example.org/#fragment",
    ] {
        assert!(producer_redirect_endpoint(
            "https://mainnet.ackinacki.org",
            &json!({"producers":[target]})
        )
        .is_err());
    }
    assert!(producer_redirect_data(
        &json!({"error":{"code":"WRONG_PRODUCER","data":{"producers":[]}}})
    )
    .is_none());
}

#[tokio::test]
async fn issue_2030_invalid_redirect_thread_is_not_replayed() {
    for thread in ["a".repeat(62), "z".repeat(64)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut refusal = issue_2030_wrong_producer(&endpoint);
        refusal["error"]["data"]["thread_id"] = json!(thread);
        let (bodies, task) = issue_2030_producer_server(listener, vec![(400, refusal)]);
        let error = send_message_routed_checked(
            &build_money_post_http_client().unwrap(),
            &endpoint,
            "original-signed-boc",
            &"1".repeat(64),
            crate::address::DEXDO_DAPP_ID,
            None,
        )
        .await
        .unwrap_err();
        task.await.unwrap();
        assert!(error.to_string().contains("invalid thread_id"));
        assert_eq!(bodies.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn issue_2030_note_fill_poll_uses_system_dapp_without_account_lookup() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, task) = issue_2030_producer_server(
        listener,
        vec![(
            200,
            json!({"data":{"blockchain":{"account":{
                "info":{"id":"note"},
                "messages":{"edges":[],"pageInfo":{"startCursor":null,"hasPreviousPage":false}}
            }}}}),
        )],
    );
    let deployment = deployed("");
    let backend = RealChainBackend {
        client: LimitedChainClient::new(
            ChainClient::connect(&endpoint).unwrap(),
            ChainRequestCeiling::Unlimited,
        ),
        http: reqwest::Client::new(),
        money_post_http: build_money_post_http_client().unwrap(),
        superroot: Address::parse(&deployment.superroot).unwrap(),
        deployed: deployment,
    };
    let note = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let book = Address::parse(&format!("0:{}", "b".repeat(64))).unwrap();
    let result = backend
        .poll_inference_filled_tcs(&note, &book, false, &mut MatchWatchCursor::new(0))
        .await
        .unwrap();
    task.await.unwrap();
    assert!(result.is_empty());
    assert_eq!(
        requests.lock().unwrap()[0]["variables"]["dappId"],
        crate::address::DEXDO_DAPP_ID
    );
}
