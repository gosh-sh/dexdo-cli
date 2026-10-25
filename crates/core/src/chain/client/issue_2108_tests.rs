include!("issue_2108_buy_tests.rs");

fn issue_2108_fill_edge(order_id: u128, ticks: u128, price: u128, is_buy: bool) -> Value {
    use tvm_abi::token::Tokenizer;
    use tvm_abi::{Contract, TokenValue};
    use tvm_types::{BuilderData, IBitstring as _};

    let abi = Contract::load(PRIVATENOTE_ABI.as_bytes()).unwrap();
    let event = abi.event("InferenceFilledConfirmed").unwrap();
    let fields = json!({
        "orderBook": format!("0:{}", "b".repeat(64)),
        "tokenContract": format!("0:{}", "c".repeat(64)),
        "orderId": order_id.to_string(), "ticks": ticks.to_string(),
        "clearingPrice": price.to_string(), "isBuy": is_buy,
    });
    let tokens = Tokenizer::tokenize_all_params(&event.inputs, &fields).unwrap();
    let mut prefix = BuilderData::new();
    prefix.append_u32(event.get_id()).unwrap();
    let cell = TokenValue::pack_values_into_chain(&tokens, vec![prefix.into()], &event.abi_version)
        .unwrap()
        .into_cell()
        .unwrap();
    let body =
        base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&cell).unwrap());
    json!({"cursor":"fill-cursor", "node":{
        "id":"fill-id", "created_at":10, "body":body,
    }})
}

fn issue_2108_page(edges: Vec<Value>, previous: Option<&str>) -> Value {
    json!({"data":{"blockchain":{"account":{
        "info":{"id":"note"}, "messages":{
            "pageInfo":{"startCursor":previous, "hasPreviousPage":previous.is_some()},
            "edges":edges,
        }
    }}}})
}

#[tokio::test]
async fn issue_2108_rejects_non_adjacent_pagination_cycle_without_consuming_fills() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let fill = issue_2108_fill_edge(5, 2, PRICE_STEP, false);
    let (requests, task) = issue_2030_producer_server(
        listener,
        vec![
            (
                200,
                issue_2108_page(vec![fill.clone()], Some("opaque-A/+=")),
            ),
            (
                200,
                issue_2108_page(vec![fill.clone()], Some("opaque-B/+=")),
            ),
            (200, issue_2108_page(vec![fill], Some("opaque-A/+="))),
            (
                200,
                json!({"errors":[{"message":"cycle sentinel must not be fetched"}]}),
            ),
        ],
    );
    let mut cursor = MatchWatchCursor::new(0);
    let original_cursor = serde_json::to_value(&cursor).unwrap();
    let error = issue_2108_poll(&issue_2030_backend(&endpoint), &mut cursor)
        .await
        .unwrap_err();
    task.abort();
    let _ = task.await;
    assert_eq!(serde_json::to_value(cursor).unwrap(), original_cursor);
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert!(error.to_string().contains("pagination"), "{error:#}");
    assert!(!is_transient_read_failure(&error));
}

#[test]
fn issue_2108_buy_signing_follows_preparation_reads_and_precedes_journaling() {
    let source = include_str!("../client.rs");
    for (start, end, reads) in [
        (
            "pub async fn place_inference_buy_with_submit_identity(",
            "pub async fn place_inference_buy_with_identity_and_cursors(",
            [
                ".poll_inference_filled_tcs(",
                ".private_note_shell_balance(",
            ],
        ),
        (
            "pub async fn place_inference_buy_with_identity_and_cursors(",
            "pub async fn cancel_inference_order(",
            [
                ".inference_orderbook_stats(",
                ".poll_inference_attributed_fills(",
            ],
        ),
    ] {
        let body = source
            .split_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        let signing = body.find(".prepare_money_post(").unwrap();
        for read in reads {
            assert!(
                body.find(read).unwrap() < signing,
                "{start} signs before {read}"
            );
        }
        let journal = body.find("before_post(").unwrap();
        assert!(signing < journal);
        assert!(journal < body.find("retry_buyer_money_submit(").unwrap());
        assert!(body[journal..].contains("money_submit_identity(&boc)"));
    }
}

fn issue_2108_pool_timeout() -> Value {
    json!({"data":{"blockchain":{"account":null}}, "errors":[{
        "message":"pool timed out while waiting for an open connection",
        "path":["blockchain","account","messages"],
    }]})
}

async fn issue_2108_poll(
    backend: &RealChainBackend,
    cursor: &mut MatchWatchCursor,
) -> Result<Vec<MatchedFill>> {
    backend
        .poll_inference_filled_tcs(
            &Address::parse(&format!("0:{}", "a".repeat(64))).unwrap(),
            &Address::parse(&format!("0:{}", "b".repeat(64))).unwrap(),
            false,
            cursor,
        )
        .await
}

#[tokio::test]
async fn issue_2108_recovers_old_page_and_does_not_replay_after_cursor_restart() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let recent = (0..crate::params::EXT_OUT_PAGE_SIZE)
        .map(|i| {
            json!({
                "cursor":format!("recent-{i}"), "node":{
                    "id":format!("recent-{i}"), "created_at":20+i, "body":"unrelated-event",
                }
            })
        })
        .collect();
    let first = issue_2108_page(recent, Some("opaque-before/+="));
    let old = issue_2108_page(
        vec![issue_2108_fill_edge(5, 2, 10 * PRICE_STEP, false)],
        None,
    );
    let (requests, task) = issue_2030_producer_server(
        listener,
        vec![
            (200, first.clone()),
            (200, old.clone()),
            (200, first),
            (200, old),
        ],
    );
    let backend = issue_2030_backend(&endpoint);
    let mut cursor = MatchWatchCursor::new(0);
    let fills = issue_2108_poll(&backend, &mut cursor).await.unwrap();
    assert_eq!(
        fills,
        vec![MatchedFill {
            order_id: 5,
            token_contract: format!("0:{}", "c".repeat(64)),
            ticks: 2,
            price_per_tick: 10 * PRICE_STEP,
        }]
    );
    let mut restarted = serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
    assert!(issue_2108_poll(&backend, &mut restarted)
        .await
        .unwrap()
        .is_empty());
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for (i, request) in requests.iter().enumerate() {
        assert_eq!(
            request["variables"]["last"],
            crate::params::EXT_OUT_PAGE_SIZE
        );
        assert_eq!(
            request["variables"]["dappId"],
            crate::address::DEXDO_DAPP_ID
        );
        assert_eq!(
            request["variables"]["before"],
            if i % 2 == 0 {
                Value::Null
            } else {
                json!("opaque-before/+=")
            }
        );
    }
}

#[tokio::test]
async fn issue_2108_pool_timeout_on_older_page_retries_without_losing_fill() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let first = issue_2108_page(vec![], Some("older"));
    let (requests, task) = issue_2030_producer_server(
        listener,
        vec![
            (200, first),
            (200, issue_2108_pool_timeout()),
            (
                200,
                issue_2108_page(vec![issue_2108_fill_edge(5, 2, PRICE_STEP, false)], None),
            ),
        ],
    );
    let backend = issue_2030_backend(&endpoint);
    let mut cursor = MatchWatchCursor::new(0);
    assert_eq!(
        issue_2108_poll(&backend, &mut cursor).await.unwrap().len(),
        1
    );
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0]["variables"]["before"].is_null());
    assert_eq!(requests[1]["variables"]["before"], "older");
    assert_eq!(requests[2]["variables"]["before"], "older");
    assert_eq!(cursor.last_seen_created_at, Some(10));
}

#[tokio::test]
async fn issue_2108_retries_http_200_pool_error_before_reading_history() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let page = issue_2108_page(vec![issue_2108_fill_edge(5, 2, PRICE_STEP, false)], None);
    let (requests, task) = issue_2030_producer_server(
        listener,
        vec![(200, issue_2108_pool_timeout()), (200, page)],
    );
    let mut cursor = MatchWatchCursor::new(0);
    let fills = issue_2108_poll(&issue_2030_backend(&endpoint), &mut cursor)
        .await
        .unwrap();
    task.await.unwrap();
    assert_eq!(fills.len(), 1);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn issue_2108_failed_history_never_advances_the_fill_cursor() {
    let failures = [
        json!({"errors":[{"message":"Cannot query field messages"}]}),
        json!({"data":{"blockchain":{"account":null}}}),
        json!({"data":{"blockchain":{"account":{"info":{},"messages":{"edges":[]}}}}}),
    ];
    for failure in failures {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        // A valid first page must not be consumed when the older page is unavailable.
        let first = issue_2108_page(
            vec![issue_2108_fill_edge(5, 2, PRICE_STEP, false)],
            Some("older"),
        );
        let (requests, task) =
            issue_2030_producer_server(listener, vec![(200, first), (200, failure)]);
        let backend = issue_2030_backend(&endpoint);
        let mut cursor = MatchWatchCursor::new(0);
        assert!(issue_2108_poll(&backend, &mut cursor).await.is_err());
        task.await.unwrap();
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "permanent errors must not retry"
        );
        assert_eq!(cursor.last_seen_created_at, None);
        assert!(cursor.seen_token_contracts_at_last_seen.is_empty());
    }
}

#[tokio::test]
async fn issue_2108_rejects_stuck_pagination_without_consuming_a_fill() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let page = issue_2108_page(
        vec![issue_2108_fill_edge(5, 2, PRICE_STEP, false)],
        Some("stuck"),
    );
    let (requests, task) =
        issue_2030_producer_server(listener, vec![(200, page.clone()), (200, page)]);
    let backend = issue_2030_backend(&endpoint);
    let mut cursor = MatchWatchCursor::new(0);
    let error = issue_2108_poll(&backend, &mut cursor).await.unwrap_err();
    task.await.unwrap();
    assert!(
        error.to_string().contains("pagination made no progress"),
        "{error:#}"
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(cursor.last_seen_created_at, None);
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn issue_2108_preserves_authoritative_fill_terms_and_filters_buyer_events(
        order_id in 1u128..1_000,
        ticks in 1u128..1_000,
        price_ticks in 1u128..100,
    ) {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let price = price_ticks * PRICE_STEP;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let matching = issue_2108_fill_edge(order_id, ticks, price, false);
            let mut wrong_side = issue_2108_fill_edge(order_id, ticks, price, true);
            wrong_side["node"]["id"] = json!("buyer-fill-id");
            wrong_side["cursor"] = json!("buyer-fill-cursor");
            let (requests, task) = issue_2030_producer_server(listener, vec![(200, issue_2108_page(vec![wrong_side, matching], None))]);
            let backend = issue_2030_backend(&endpoint);
            let fills = issue_2108_poll(&backend, &mut MatchWatchCursor::new(0)).await.unwrap();
            task.await.unwrap();
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert_eq!(fills.len(), 1);
            assert_eq!((fills[0].order_id, fills[0].ticks, fills[0].price_per_tick), (order_id, ticks, price));
        });
    }
}

async fn issue_2108_check_long_history(fill_page: u32) {
    // With the production-like 3/s ceiling, 65 canonical pages take more than one
    // read-attempt timeout even when every server response is immediate and valid.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let body: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let page = body["variables"]["before"]
                .as_str()
                .map(|cursor| {
                    cursor
                        .strip_prefix("page-")
                        .unwrap()
                        .parse::<u32>()
                        .unwrap()
                })
                .unwrap_or(0);
            captured.lock().unwrap().push(body);
            let mut edges = (0..crate::params::EXT_OUT_PAGE_SIZE).map(|i| json!({
                    "cursor":format!("event-{page}-{i}"), "node":{
                        "id":format!("event-{page}-{i}"), "created_at":1, "body":"unrelated-event"
                    }
                })).collect::<Vec<_>>();
            if page == fill_page {
                edges[0] = issue_2108_fill_edge(5, 2, PRICE_STEP, false);
                edges[0]["node"]["created_at"] = json!(30);
            }
            let previous = (page < 64).then(|| format!("page-{}", page + 1));
            let response = issue_2108_page(edges, previous.as_deref()).to_string();
            if socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len(),
                ).as_bytes()).await.is_err() { continue; }
        }
    });
    let mut backend = issue_2030_backend(&endpoint);
    backend.client = LimitedChainClient::new(
        ChainClient::connect(&endpoint).unwrap(),
        ChainRequestCeiling::PerSecond(3),
    );
    // The first case has a fresh fill and old events below the durable cursor;
    // the second requires walking all pages for an old legacy recovery fill.
    let mut cursor = MatchWatchCursor::new(if fill_page == 0 { 20 } else { 0 });
    let started = tokio::time::Instant::now();
    let result = issue_2108_poll(&backend, &mut cursor).await;
    let elapsed = started.elapsed();
    server.abort();
    let _ = server.await;
    let fills = result.expect("valid long history must finish without restarting its scan");
    assert_eq!(fills.len(), 1);
    assert_eq!(
        (fills[0].order_id, fills[0].ticks, fills[0].price_per_tick),
        (5, 2, PRICE_STEP)
    );
    assert_eq!(cursor.last_seen_created_at, Some(30));
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["variables"]["before"].is_null())
            .count(),
        1
    );
    if fill_page != 0 {
        assert_eq!(requests.len(), 65);
        assert!(elapsed > crate::params::TRANSIENT_READ_ATTEMPT_TIMEOUT);
    }
}

#[tokio::test]
async fn issue_2108_long_history_returns_fresh_fill_without_scan_restarts() {
    issue_2108_check_long_history(0).await;
}

#[tokio::test]
async fn issue_2108_long_history_recovers_old_fill_beyond_one_read_timeout() {
    issue_2108_check_long_history(64).await;
}

#[tokio::test(start_paused = true)]
async fn issue_2108_read_wrapper_timeout_does_not_enable_money_submit_retry() {
    let error = retry_transient_read(std::future::pending::<Result<()>>)
        .await
        .unwrap_err();
    assert!(!is_transient_submit_failure(&error));
}
