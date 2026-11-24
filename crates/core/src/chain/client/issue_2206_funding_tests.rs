struct SellerFundingFixture {
    note: Address,
    tc: Address,
    bond: u128,
    preflight_reads: [(u16, Value); 3],
    funded: Value,
    opened: Value,
}

impl SellerFundingFixture {
    fn new() -> Self {
        let note = issue_2115_address('a');
        let tc = issue_2115_address('c');
        let price = crate::params::PRICE_STEP;
        let bond = 2 * price;
        let note_reply = issue_2242_account_reply(issue_2115_account_fixture(
            &note,
            PRIVATENOTE_ABI,
            PRIVATENOTE_TVC,
            json!({"_constructorFlag":true,"_balance":{"2":bond.to_string()}}),
        ));
        let tc_reply = |funded, opened| {
            issue_2242_account_reply(issue_2115_account_fixture(
                &tc,
                TOKENCONTRACT_ABI,
                TOKENCONTRACT_TVC,
                json!({"_constructorFlag":true,"_pricePerTick":price.to_string(),"_funded":true,
                    "_deposit":bond.to_string(),"_opened":opened,"_sellerBondFunded":funded,
                    "_sellerBond":if funded { bond.to_string() } else { "0".to_string() }}),
            ))
        };
        let before = tc_reply(false, false);
        let preflight_reads = [(200, before.clone()), (200, before), (200, note_reply)];
        let funded = tc_reply(true, false);
        let opened = tc_reply(true, true);
        Self {
            note,
            tc,
            bond,
            preflight_reads,
            funded,
            opened,
        }
    }

    fn seller(&self, endpoint: &str) -> super::super::backends::RealSellerBackend {
        super::super::backends::RealSellerBackend::new(
            issue_2030_backend(endpoint),
            self.note.clone(),
            KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            model_hash_for("fixture--model--v1"),
            "fixture--model--v1".into(),
            1,
            TICK_SIZE,
        )
    }

    async fn open(
        &self,
        seller: &super::super::backends::RealSellerBackend,
    ) -> Result<(), crate::ChainError> {
        seller
            .open_stream(
                &self.tc.with_workchain(),
                vec![1],
                &crate::note::LocalNote::generate(),
            )
            .await
    }

    fn preflight(&self) -> Vec<(u16, Value)> {
        let mut replies = self.preflight_reads.to_vec();
        replies.push((200, issue_2115_clock_reply()));
        replies
    }

    fn confirmation(&self) -> [(u16, Value); 4] {
        [
            (200, self.funded.clone()),
            (200, issue_2115_clock_reply()),
            (200, json!({"result":{}})),
            (200, self.opened.clone()),
        ]
    }

    fn assert_funding(&self, item: &Value) {
        assert_eq!(item["account_id"], self.note.bare());
        let funding =
            decode_external_abi_message_boc(item["body"].as_str().unwrap(), PRIVATENOTE_ABI, true)
                .unwrap();
        assert_eq!(funding.function_name, "fundDeal");
        assert_eq!(decoded_u128(&funding.tokens, "amount"), Some(self.bond));
        assert_eq!(decoded_u128(&funding.tokens, "nonce"), Some(1));
    }

    fn assert_open(&self, item: &Value) {
        assert_eq!(item["account_id"], self.tc.bare());
        assert_eq!(
            decode_external_abi_message_boc(
                item["body"].as_str().unwrap(),
                TOKENCONTRACT_ABI,
                true
            )
            .unwrap()
            .function_name,
            "open"
        );
    }
}

#[tokio::test]
async fn issue_2206_seller_funding_recovers_from_clock_outage_before_post() {
    let fixture = SellerFundingFixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut replies = fixture.preflight_reads.to_vec();
    replies.extend((0..crate::params::TRANSIENT_READ_ATTEMPTS).map(|_| (503, json!({}))));
    replies.extend(fixture.preflight());
    replies.push((200, json!({"result":{}})));
    replies.extend(fixture.confirmation());
    let (requests, server) = issue_2030_producer_server(listener, replies);
    let seller = fixture.seller(&endpoint);
    let error = fixture.open(&seller).await.unwrap_err();
    assert!(matches!(error, crate::ChainError::Transport(_)), "{error}");
    {
        let reads = requests.lock().unwrap();
        assert!(
            reads.iter().all(Value::is_object),
            "preflight sent a money POST"
        );
        assert_eq!(
            reads
                .iter()
                .filter(|body| body["query"]
                    .as_str()
                    .is_some_and(|q| q.contains("gen_utime")))
                .count(),
            crate::params::TRANSIENT_READ_ATTEMPTS,
        );
    }
    fixture.open(&seller).await.unwrap();
    server.await.unwrap();
    let bodies = requests.lock().unwrap();
    let posts: Vec<_> = bodies.iter().filter(|body| body.is_array()).collect();
    assert_eq!(posts.len(), 2);
    fixture.assert_funding(&posts[0][0]);
    fixture.assert_open(&posts[1][0]);
}

#[tokio::test]
async fn issue_2206_seller_funding_routes_direct_and_embedded_producer_refusals() {
    seller_funding_routes_embedded_producer_refusal("INTERNAL_ERROR", None).await;
}

#[tokio::test]
async fn seller_funding_preserves_producer_redirect_after_exit_52() {
    seller_funding_routes_embedded_producer_refusal("TVM_ERROR", Some(52)).await;
}

#[tokio::test]
async fn seller_claim_snapshot_reports_a_non_monotonic_getter_as_unsafe() {
    let fixture = SellerFundingFixture::new();
    let mut info = issue_2115_account_fixture(
        &fixture.tc,
        TOKENCONTRACT_ABI,
        TOKENCONTRACT_TVC,
        json!({
            "_constructorFlag": true, "_funded": true, "_opened": true,
            "_probeAccepted": true, "_deposit": fixture.bond.to_string(),
            "_tokensFinal": (2 * TICK_SIZE).to_string(), "_tokensPend": TICK_SIZE.to_string(),
        }),
    );
    info["code_hash"] = json!("1".repeat(64));
    let account = issue_2242_account_reply(info);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(listener, vec![(200, account); 3]);
    let error = fixture
        .seller(&endpoint)
        .deal_state(&fixture.tc.with_workchain())
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            crate::ChainError::UnsafeClaimState(message)
                if message.contains("getState() claim pipeline is not monotonic")
                    && message.contains(&format!("tokensFinal={} tokensPending={}", 2 * TICK_SIZE, TICK_SIZE))
        ),
        "{error}"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        3,
        "unsafe state needs both BOC reads"
    );
    server.await.unwrap();
    assert!(requests.lock().unwrap().iter().all(Value::is_object));
}

async fn seller_funding_routes_embedded_producer_refusal(code: &str, exit_code: Option<u64>) {
    let fixture = SellerFundingFixture::new();
    let edge = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let producer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let final_producer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", edge.local_addr().unwrap());
    let first_thread = "d".repeat(64);
    let final_thread = "e".repeat(64);
    let mut direct = issue_2030_wrong_producer(&producer.local_addr().unwrap().to_string());
    direct["error"]["data"]["thread_id"] = json!(first_thread);
    let mut inner = issue_2030_wrong_producer(&final_producer.local_addr().unwrap().to_string());
    inner["error"]["data"]["thread_id"] = json!(final_thread);
    let mut embedded = json!({"error":{"code":code,"message":format!(
        "Block Producer returned non-success HTTP status 400 Bad Request: {inner}"
    )}});
    if let Some(exit_code) = exit_code {
        embedded["error"]["data"] = json!({"exit_code": exit_code});
    }
    let mut replies = fixture.preflight();
    replies.push((400, direct));
    replies.extend(fixture.confirmation());
    let (edge_requests, edge_server) = issue_2030_producer_server(edge, replies);
    let (producer_requests, producer_server) =
        issue_2030_producer_server(producer, vec![(200, embedded)]);
    let (final_requests, final_server) =
        issue_2030_producer_server(final_producer, vec![(200, json!({"result":{}}))]);

    fixture.open(&fixture.seller(&endpoint)).await.unwrap();
    edge_server.await.unwrap();
    producer_server.await.unwrap();
    final_server.await.unwrap();
    let requests = edge_requests.lock().unwrap();
    let posts: Vec<_> = requests.iter().filter(|body| body.is_array()).collect();
    assert_eq!(posts.len(), 2);
    fixture.assert_funding(&posts[0][0]);
    fixture.assert_open(&posts[1][0]);
    let mut funding = posts[0][0].clone();
    funding["thread_id"] = json!(first_thread);
    assert_eq!(
        producer_requests.lock().unwrap().as_slice(),
        &[json!([funding.clone()])]
    );
    funding["thread_id"] = json!(final_thread);
    let last = final_requests.lock().unwrap();
    assert_eq!(last.as_slice(), &[json!([funding])]);
    fixture.assert_funding(&last[0][0]);
}

#[tokio::test]
async fn issue_2206_seller_funding_reuses_identity_within_queue_retry_budget() {
    let fixture = SellerFundingFixture::new();
    let max_retries = crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL;
    for exhaust in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut replies = fixture.preflight();
        let refusals = if exhaust { max_retries + 1 } else { 1 };
        replies.extend((0..refusals).map(|_| {
            (
                200,
                json!({"error":"QUEUE_OVERFLOW: message queue is full"}),
            )
        }));
        if !exhaust {
            replies.push((200, json!({"result":{}})));
            replies.extend(fixture.confirmation());
        }
        let (requests, server) = issue_2030_producer_server(listener, replies);
        let outcome = fixture.open(&fixture.seller(&endpoint)).await;
        if exhaust {
            let error = outcome.unwrap_err();
            assert!(
                matches!(error, crate::ChainError::MoneySubmitRejected(_)),
                "{error}"
            );
        } else {
            outcome.unwrap();
        }
        server.await.unwrap();
        let requests = requests.lock().unwrap();
        let posts: Vec<_> = requests.iter().filter(|body| body.is_array()).collect();
        let funding_posts = usize::try_from(refusals).unwrap() + usize::from(!exhaust);
        assert_eq!(posts.len(), funding_posts + usize::from(!exhaust));
        for post in &posts[..funding_posts] {
            assert_eq!(*post, posts[0], "funding retry changed its signed request");
        }
        fixture.assert_funding(&posts[0][0]);
        if !exhaust {
            fixture.assert_open(&posts[funding_posts][0]);
        }
    }
}

#[tokio::test]
async fn issue_2206_seller_funding_stops_after_unknown_post_or_invalid_routing() {
    let fixture = SellerFundingFixture::new();
    // A request sink remains active after the first POST, so a replay would be recorded.
    for (status, body) in [
        (None, Value::Null),
        (Some(503), json!({"error":{"code":"QUEUE_OVERFLOW"}})),
        (Some(307), json!({})),
        (
            Some(400),
            issue_2030_wrong_producer("http://127.0.0.1:1/wrong-path"),
        ),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let redirect = format!("Location: {endpoint}/unexpected\r\n");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let mut preflight = fixture.preflight().into_iter();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_fixture_http_request(&mut socket).await;
                let (_, payload) = request.split_once("\r\n\r\n").unwrap();
                let payload: Value = serde_json::from_str(payload).unwrap();
                let (status, body) = if payload.is_array() {
                    captured.lock().unwrap().push(payload);
                    let Some(status) = status else { continue };
                    (status, body.clone())
                } else {
                    preflight.next().unwrap()
                };
                let headers = if status == 307 { redirect.as_str() } else { "" };
                let body = body.to_string();
                socket.write_all(format!(
                    "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                    body.len(),
                ).as_bytes()).await.unwrap();
            }
        });
        let error = fixture.open(&fixture.seller(&endpoint)).await.unwrap_err();
        task.abort();
        assert!(
            matches!(error, crate::ChainError::AmbiguousSubmit(_)),
            "{error}"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        fixture.assert_funding(&requests[0][0]);
    }
}
