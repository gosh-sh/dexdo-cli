fn issue_2115_post() -> SellerPlacement {
    SellerPlacement::new("shellnet", &issue_2115_address('a'), &issue_2115_address('c'),
        &issue_2115_address('b'), "1".repeat(64), 10)
}

fn issue_2115_address(c: char) -> Address {
    Address::parse(&format!("0:{}", c.to_string().repeat(64))).unwrap()
}

fn issue_2115_message(id: char, src: Option<char>, dst: Option<char>, outputs: &[char]) -> Value {
    json!({"data":{"blockchain":{"message":{
        "id":id.to_string().repeat(64),
        "src":src.map(|c| issue_2115_address(c).with_workchain()),
        "dst":dst.map(|c| issue_2115_address(c).with_workchain()),
        "body":"", "dst_transaction":{
            "id":format!("transaction-{id}"), "status":3, "aborted":false,
            "compute":{"success":true,"exit_code":0},
            "action":{"success":true,"result_code":0},
            "out_msgs":outputs.iter().map(|c| c.to_string().repeat(64)).collect::<Vec<_>>()
        }
    }}}})
}

fn issue_2115_placement_event() -> Value {
    issue_2115_placement_event_for(102)
}

fn issue_2115_placement_event_for(order_id: u128) -> Value {
    use tvm_abi::{Contract, TokenValue, token::Tokenizer};
    use tvm_types::{BuilderData, IBitstring as _};
    let abi = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes()).unwrap();
    let event = abi.event("InferenceOrderPlaced").unwrap();
    let tokens = Tokenizer::tokenize_all_params(&event.inputs, &json!({
        "orderId":order_id.to_string(), "isBuy":false, "price":PRICE_STEP.to_string(), "ticks":"100",
        "note":issue_2115_address('a').with_workchain(),
        "tokenContract":issue_2115_address('c').with_workchain(), "deadline":"1792137676", "flags":0
    })).unwrap();
    let mut prefix = BuilderData::new();
    prefix.append_u32(event.get_id()).unwrap();
    let cell = TokenValue::pack_values_into_chain(&tokens, vec![prefix.into()], &event.abi_version)
        .unwrap().into_cell().unwrap();
    let mut reply = issue_2115_message('5', Some('b'), None, &[]);
    reply["data"]["blockchain"]["message"]["body"] = json!(
        base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&cell).unwrap())
    );
    reply
}

#[tokio::test]
async fn issue_2115_delayed_execution_survives_restart_and_book_queue() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(listener, vec![
        (200, json!({"data":{"blockchain":{"message":null}}})),
        (200, issue_2115_message('1', None, Some('a'), &['2'])),
        (200, issue_2115_message('2', Some('a'), Some('c'), &['3'])),
        (200, issue_2115_message('3', Some('c'), Some('b'), &['4'])),
        (200, issue_2115_message('4', Some('b'), Some('b'), &['5'])),
        (200, issue_2115_placement_event()),
    ]);
    let backend = issue_2030_backend(&endpoint);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let mut post = issue_2115_post();
    post.save(Some(&path)).unwrap();
    let error = backend.reconcile_seller_placement(&mut post, Some(&path)).await.unwrap_err();
    assert!(error.to_string().contains("missing SELL message"));
    assert!(post.terminal.is_none());
    drop(post);
    let mut restored = SellerPlacement::load(&path).unwrap().unwrap();
    backend.reconcile_seller_placement(&mut restored, Some(&path)).await.unwrap();
    assert_eq!(restored.accepted_order_id, Some(102));
    assert_eq!(SellerPlacement::load(&path).unwrap().unwrap().accepted_order_id, Some(102));
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0]["variables"]["hash"], requests[1]["variables"]["hash"]);
    assert!(requests.iter().all(|request| request["query"].as_str().unwrap().contains("message(hash:")));
}

#[tokio::test]
async fn issue_2115_pending_post_blocks_another_submission_after_restart() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(listener, vec![
        (200, json!({"data":{"blockchain":{"message":null}}})),
    ]);
    let chain = issue_2030_backend(&endpoint);
    let directory = tempfile::tempdir().unwrap();
    let tc = issue_2115_address('c');
    let path = directory.path().join(format!("{}.sell.json", tc.bare()));
    let mut post = issue_2115_post();
    post.network = chain.network().to_string();
    post.save(Some(&path)).unwrap();
    let seller = super::super::backends::RealSellerBackend::new(
        chain, issue_2115_address('a'), KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        "model-hash".into(), "model".into(), 1, TICK_SIZE,
    ).with_offer_journal_dir(directory.path().to_path_buf());
    let result = seller.reconcile_pending_offer(&tc, &issue_2115_address('b')).await;
    assert!(matches!(result, Err(crate::ChainError::AmbiguousSubmit(ref message))
        if message.contains(&post.message_id) && message.contains("missing SELL message")));
    assert_eq!(SellerPlacement::load(&path).unwrap().unwrap().message_id, post.message_id);
    server.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1, "no getter or second submission can authorize an unread execution");
}

#[tokio::test]
async fn issue_2115_internal_rejection_is_a_correlated_terminal_fact() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut refusal = issue_2115_message('2', Some('a'), Some('c'), &[]);
    refusal["data"]["blockchain"]["message"]["dst_transaction"]["aborted"] = json!(true);
    refusal["data"]["blockchain"]["message"]["dst_transaction"]["compute"]["exit_code"] = json!(101);
    let (_, server) = issue_2030_producer_server(listener, vec![
        (200, issue_2115_message('1', None, Some('a'), &['2'])), (200, refusal),
    ]);
    let mut post = issue_2115_post();
    issue_2030_backend(&endpoint).reconcile_seller_placement(&mut post, None).await.unwrap();
    assert!(post.terminal.as_ref().unwrap().contains("rejected message_id=2222"));
    assert!(post.terminal.as_ref().unwrap().contains("compute_exit=101"));
    assert_eq!(post.accepted_order_id, None);
    server.await.unwrap();
}

#[tokio::test]
async fn issue_2115_action_38_without_book_dispatch_is_rejected_not_unknown() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut refusal = issue_2115_message('2', Some('a'), Some('c'), &[]);
    let transaction = &mut refusal["data"]["blockchain"]["message"]["dst_transaction"];
    transaction["aborted"] = json!(true);
    transaction["action"] = json!({"success":false,"result_code":38});
    let (_, server) = issue_2030_producer_server(listener, vec![
        (200, issue_2115_message('1', None, Some('a'), &['2'])), (200, refusal),
        (200, issue_2115_message('1', None, Some('a'), &['2'])),
        (200, issue_2115_message('2', Some('a'), Some('c'), &[])),
    ]);
    let mut post = issue_2115_post();
    issue_2030_backend(&endpoint).reconcile_seller_placement(&mut post, None).await.unwrap_err();
    let terminal = post.terminal.unwrap();
    assert!(terminal.contains("rejected message_id=2222"), "{terminal}");
    assert!(terminal.contains("compute_exit=0 action_result=38"), "{terminal}");
    assert!(post.accepted_order_id.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn issue_2115_incomplete_or_foreign_receipts_never_release_pending_execution() {
    let root = issue_2115_message('1', None, Some('a'), &[]);
    let mut cases = vec![json!({"errors":[{"message":"history unavailable"}]}),
        issue_2115_message('9', None, Some('a'), &[]),
        issue_2115_message('1', None, Some('f'), &[])];
    for field in ["id", "status", "aborted", "out_msgs", "action"] {
        let mut broken = root.clone();
        broken["data"]["blockchain"]["message"]["dst_transaction"][field] = Value::Null;
        cases.push(broken);
    }
    for reply in cases {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (_, server) = issue_2030_producer_server(listener, vec![(200, reply)]);
        let mut post = issue_2115_post();
        assert!(issue_2030_backend(&endpoint).reconcile_seller_placement(&mut post, None).await.is_err());
        assert!(post.terminal.is_none());
        assert_eq!(post.accepted_order_id, None);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn issue_2115_resumes_at_missing_internal_message_without_rereading_root() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(listener, vec![
        (200, issue_2115_message('1', None, Some('a'), &['2'])),
        (200, json!({"data":{"blockchain":{"message":null}}})),
        (200, issue_2115_message('2', Some('a'), Some('c'), &[])),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let chain = issue_2030_backend(&endpoint);
    let mut post = issue_2115_post();
    assert!(chain.reconcile_seller_placement(&mut post, Some(&path)).await.is_err());
    let mut restored = SellerPlacement::load(&path).unwrap().unwrap();
    chain.reconcile_seller_placement(&mut restored, Some(&path)).await.unwrap();
    assert!(restored.terminal.is_some());
    server.await.unwrap();
    assert_eq!(requests.lock().unwrap()[2]["variables"]["hash"], "2".repeat(64));
}

#[tokio::test]
async fn issue_2115_journals_the_exact_signed_message_before_post() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let inspect = path.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let is_post = request.starts_with("POST /v2/messages ");
            let response = if is_post {
                let saved = SellerPlacement::load(&inspect).unwrap().unwrap();
                assert_eq!(saved.message_id, external_message_hash(body[0]["body"].as_str().unwrap()).unwrap());
                json!({"result":{}})
            } else {
                json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":local_unix_secs().unwrap()}}]}}}})
            }.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            if is_post { break; }
        }
    });
    let mut journal = |hash| {
        let mut post = issue_2115_post();
        post.message_id = hash;
        post.save(Some(&path))
    };
    issue_2030_backend(&endpoint).post_sell_offer_with_identity(
        &issue_2115_address('a'), &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        0, 1, crate::params::MAX_SELL_TTL.as_secs(), &mut journal,
    ).await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn issue_2115_a_journal_write_failure_prevents_posting() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(listener, vec![(200,
        json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":local_unix_secs().unwrap()}}]}}}}),
    )]);
    let error = issue_2030_backend(&endpoint).post_sell_offer_with_identity(
        &issue_2115_address('a'), &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        0, 1, crate::params::MAX_SELL_TTL.as_secs(),
        &mut |_| Err(anyhow!("journal write failed")),
    ).await.unwrap_err();
    assert!(error.to_string().contains("journal write failed"));
    server.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1, "only the clock preflight, no SELL POST");
}

#[tokio::test]
async fn issue_2115_foreign_child_and_message_cycles_remain_unresolved() {
    for child in [issue_2115_message('2', Some('f'), Some('c'), &[]),
        issue_2115_message('2', Some('a'), Some('c'), &['1'])] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (_, server) = issue_2030_producer_server(listener, vec![
            (200, issue_2115_message('1', None, Some('a'), &['2'])), (200, child),
        ]);
        let mut post = issue_2115_post();
        assert!(issue_2030_backend(&endpoint).reconcile_seller_placement(&mut post, None).await.is_err());
        assert!(post.terminal.is_none());
        server.await.unwrap();
    }
}

proptest::proptest! {
    #[test]
    fn issue_2115_journal_is_bound_to_network_and_each_participant(network in proptest::bool::ANY,
        note in proptest::bool::ANY, tc in proptest::bool::ANY, book in proptest::bool::ANY) {
        let post = issue_2115_post();
        let result = post.validate(if network {"shellnet"} else {"mainnet"},
            &issue_2115_address(if note {'a'} else {'f'}),
            &issue_2115_address(if tc {'c'} else {'f'}),
            &issue_2115_address(if book {'b'} else {'f'}));
        proptest::prop_assert_eq!(result.is_ok(), network && note && tc && book);
    }
}

#[tokio::test]
async fn issue_2115_read_timeout_retains_the_same_durable_submission() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requested = Arc::new(tokio::sync::Notify::new());
    let notify = requested.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_fixture_http_request(&mut socket).await;
        notify.notify_one();
        std::future::pending::<()>().await;
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let mut post = issue_2115_post();
    post.save(Some(&path)).unwrap();
    let chain = issue_2030_backend(&endpoint);
    {
        let read = chain.reconcile_seller_placement(&mut post, Some(&path));
        tokio::pin!(read);
        tokio::select! {
            _ = requested.notified() => {},
            result = &mut read => panic!("receipt read must remain in flight: {result:?}"),
        }
        assert!(tokio::time::timeout(std::time::Duration::from_millis(10), &mut read).await.is_err());
    }
    assert!(post.terminal.is_none());
    assert_eq!(SellerPlacement::load(&path).unwrap().unwrap().message_id, post.message_id);
    server.abort();
    let _ = server.await;
}

include!("issue_2115_review_tests.rs");
