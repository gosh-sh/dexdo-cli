#[tokio::test]
async fn issue_2108_buy_first_post_has_fresh_expiry_and_primed_identity() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let chain_offset = MAX_CLOCK_BEHIND_SECS - 1;
    let server = tokio::spawn(async move {
        let mut pages = 0;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let is_post = request.starts_with("POST /v2/messages ");
            let response = if is_post {
                json!({"result":{}})
            } else if body["query"].as_str().unwrap().contains("gen_utime") {
                assert_eq!(pages, 65, "complete history before the signing preflight");
                json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{
                    "gen_utime":local_unix_secs().unwrap() + chain_offset
                }}]}}}})
            } else {
                let page = body["variables"]["before"]
                    .as_str()
                    .map(|s| s.parse::<u32>().unwrap())
                    .unwrap_or(0);
                assert_eq!(page, pages);
                pages += 1;
                let mut edges = (0..crate::params::EXT_OUT_PAGE_SIZE).map(|i| {
                    let id = format!("event-{page}-{i}");
                    json!({"cursor":id,"node":{"id":id,"body":"unrelated-event","created_at":1}})
                }).collect::<Vec<_>>();
                if page == 64 {
                    edges[0] = issue_2108_fill_edge(5, 2, PRICE_STEP, true);
                }
                let previous = (page < 64).then(|| (page + 1).to_string());
                issue_2108_page(edges, previous.as_deref())
            }
            .to_string();
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()
            ).as_bytes()).await.unwrap();
            if is_post {
                let boc = body[0]["body"].as_str().unwrap().to_string();
                let decoded = tvm_client::abi::decode_message(
                    local_context().unwrap(),
                    tvm_client::abi::ParamsOfDecodeMessage {
                        abi: tvm_client::abi::Abi::Json(PRIVATENOTE_ABI.to_string()),
                        message: boc.clone(),
                        ..Default::default()
                    },
                )
                .unwrap();
                let expires = u64::from(decoded.header.unwrap().expire.unwrap());
                assert!(
                    expires > local_unix_secs().unwrap() + chain_offset,
                    "the first POST must carry a still-valid BUY"
                );
                return boc;
            }
        }
    });
    let mut backend = issue_2030_backend(&endpoint);
    backend.client = LimitedChainClient::new(
        ChainClient::connect(&endpoint).unwrap(),
        ChainRequestCeiling::PerSecond(3),
    );
    let note = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let book = Address::parse(&format!("0:{}", "b".repeat(64))).unwrap();
    let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
    let mut cursor = MatchWatchCursor::new(0);
    let started = tokio::time::Instant::now();
    backend
        .poll_inference_filled_tcs(&note, &book, true, &mut cursor)
        .await
        .unwrap();
    assert!(started.elapsed() > crate::params::TRANSIENT_READ_ATTEMPT_TIMEOUT);
    // Exercise the real reader, SDK signature and first POST. The source-order
    // regression binds this sequence to both public BUY preparation methods.
    let (endpoint, boc, account_id, dapp_id) = backend
        .prepare_money_post(
            &note,
            PRIVATENOTE_ABI,
            "placeInferenceBuy",
            place_inference_buy_payload(
                &format!("0x{}", "b".repeat(64)),
                PRICE_STEP,
                2,
                3 * PRICE_STEP,
                0,
                0,
            ),
            &keys,
        )
        .await
        .unwrap();
    let journal_identity = money_submit_identity(&boc);
    let journal_cursor = cursor.clone();
    retry_buyer_money_submit(
        &backend.money_post_http,
        &endpoint,
        &boc,
        &account_id,
        &dapp_id,
    )
    .await
    .unwrap();
    let sent_boc = server.await.unwrap();
    assert_eq!(journal_identity, money_submit_identity(&sent_boc));
    assert_eq!(sent_boc, boc);
    assert!(journal_cursor.has_seen(10, &format!("0:{}", "c".repeat(64))));
}

#[tokio::test]
async fn issue_2108_buy_priming_failure_never_signs_journals_or_posts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(
        listener,
        vec![(
            200,
            json!({"errors":[{"message":"Cannot query field messages"}]}),
        )],
    );
    let backend = issue_2030_backend(&endpoint);
    let note = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let book = Address::parse(&format!("0:{}", "b".repeat(64))).unwrap();
    let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
    let mut cursor = MatchWatchCursor::new(0);
    let original_cursor = serde_json::to_value(&cursor).unwrap();
    let mut journal_calls = 0;
    let mut before_post = |_: String, _: MatchWatchCursor, _: u128| {
        journal_calls += 1;
        Ok(())
    };
    let error = backend
        .place_inference_buy_with_submit_identity(
            &note,
            &keys,
            &book,
            &format!("0x{}", "b".repeat(64)),
            PRICE_STEP,
            2,
            3 * PRICE_STEP,
            0,
            0,
            &mut cursor,
            &mut before_post,
        )
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(matches!(
        error.downcast_ref::<MoneySubmitError>(),
        Some(MoneySubmitError::Preparation { .. })
    ));
    assert_eq!(journal_calls, 0);
    assert_eq!(serde_json::to_value(cursor).unwrap(), original_cursor);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]["query"]
        .as_str()
        .unwrap()
        .contains("messages(msg_type: [ExtOut]"));
}
