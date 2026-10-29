fn issue_2192_empty_history() -> Value {
    json!({"data":{"blockchain":{"account":{"messages":{
        "pageInfo":{"hasPreviousPage":false}, "edges":[]
    }}}}})
}

async fn issue_2192_inline_read(backend: &RealChainBackend, reader: usize) -> Result<()> {
    let account = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    match reader {
        0 => backend
            .seller_offer_events_since(&account, &account, &account, 0)
            .await
            .map(|_| ()),
        1 => backend
            .note_inbound_inference_order_calls(&account)
            .await
            .map(|_| ()),
        2 => backend
            .place_inference_buy_receipts(&account, true)
            .await
            .map(|_| ()),
        3 => backend
            .token_contract_settlement_inbound_call(&account, "event")
            .await
            .map(|_| ()),
        4 => backend
            .buyer_usage_deal_operations(&account, &account)
            .await
            .map(|_| ()),
        5 => backend
            .buyer_usage_funding_contribution(&account, &account, "event", false)
            .await
            .map(|_| ()),
        _ => unreachable!(),
    }
}

fn issue_2192_funding_anchor() -> (u16, Value) {
    (
        200,
        json!({"data":{"blockchain":{"message":{
            "id":"event", "src":format!("0:{}", "a".repeat(64)),
            "src_transaction":{"id":"transaction"}
        }}}}),
    )
}

#[tokio::test]
async fn issue_2192_inline_chain_reads_survive_502_without_any_money_post() {
    for reader in 0..6 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut replies = Vec::new();
        if reader == 5 {
            replies.push(issue_2192_funding_anchor());
        }
        replies.extend([
            (502, json!({"error":"Bad Gateway"})),
            (200, issue_2192_empty_history()),
        ]);
        let expected = replies.len();
        let (requests, task) = issue_2030_producer_server(listener, replies);
        let result = issue_2192_inline_read(&issue_2030_backend(&endpoint), reader).await;
        if matches!(reader, 3 | 5) {
            // An empty history cannot prove an actor/funding source; the transport recovered,
            // but the evidence refusal must still stand.
            let error = result.unwrap_err();
            assert!(
                !is_transient_read_failure(&error),
                "reader {reader}: {error:#}"
            );
        } else {
            result.unwrap();
        }
        task.await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), expected, "reader {reader}");
        for request in requests.iter() {
            assert!(
                request["query"].is_string(),
                "a read retry must never submit a BOC"
            );
        }
        let page_reads = &requests[usize::from(reader == 5)..];
        assert_eq!(
            page_reads[0], page_reads[1],
            "the same page must be repeated"
        );
    }
}

#[tokio::test]
async fn issue_2192_inline_chain_reads_do_not_retry_permanent_refusals() {
    for reader in 0..6 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut replies = Vec::new();
        if reader == 5 {
            replies.push(issue_2192_funding_anchor());
        }
        replies.push((400, json!({"error":"invalid query"})));
        let expected = replies.len();
        let (requests, task) = issue_2030_producer_server(listener, replies);
        let error = issue_2192_inline_read(&issue_2030_backend(&endpoint), reader)
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX));
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), expected, "reader {reader}");
    }
}

#[tokio::test]
async fn issue_2192_exhausted_page_read_is_not_restarted_by_pagination() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, task) = issue_2030_producer_server(
        listener,
        (0..crate::params::TRANSIENT_READ_ATTEMPTS)
            .map(|_| (502, json!({})))
            .collect(),
    );
    let error = issue_2192_inline_read(&issue_2030_backend(&endpoint), 1)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX));
    task.await.unwrap();
    assert_eq!(
        requests.lock().unwrap().len(),
        crate::params::TRANSIENT_READ_ATTEMPTS
    );
}

#[tokio::test]
async fn issue_2192_activation_read_failure_preserves_the_retry_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, task) = issue_2030_producer_server(
        listener,
        (0..crate::params::TRANSIENT_READ_ATTEMPTS)
            .map(|_| (502, json!({})))
            .collect(),
    );
    let backend = issue_2030_backend(&endpoint);
    let account = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let error = backend
        .wait_active_checked(
            &account,
            crate::params::ACCOUNT_ACTIVE_SINGLE_CHECK_ATTEMPTS,
            true,
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX));
    task.await.unwrap();
    assert_eq!(
        requests.lock().unwrap().len(),
        crate::params::TRANSIENT_READ_ATTEMPTS
    );
}

#[tokio::test]
async fn issue_2192_provision_failure_names_the_step_before_any_spend() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, task) = issue_2030_producer_server(
        listener,
        (0..crate::params::TRANSIENT_READ_ATTEMPTS)
            .map(|_| (502, json!({})))
            .collect(),
    );
    let backend = issue_2030_backend(&endpoint);
    let note = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let keys = KeyPair::from_secret_hex(&"22".repeat(32)).unwrap();
    let error = backend
        .provision_market_with_deal_gas_overhead_and_before_deploy(
            &keys,
            &note,
            "test-model",
            1,
            PRICE_STEP,
            4,
            0,
            0,
            |_| panic!("a failed read must never reach the money reservation"),
        )
        .await
        .unwrap_err();
    let shown = format!("{error:#}");
    assert!(
        shown.contains("provision step=validate seller note"),
        "{shown}"
    );
    assert!(shown.contains("no deployment requested"), "{shown}");
    assert!(
        shown.contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX),
        "{shown}"
    );
    task.await.unwrap();
    assert_eq!(
        requests.lock().unwrap().len(),
        crate::params::TRANSIENT_READ_ATTEMPTS
    );
}

#[tokio::test]
async fn issue_2192_review_http200_pool_timeout_retries() {
    for reader in 0..6 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut replies = Vec::new();
        if reader == 5 {
            replies.push(issue_2192_funding_anchor());
        }
        replies.extend([
            (200, json!({"errors":[{"message":"pool timed out while waiting for an open connection"}]})),
            (200, issue_2192_empty_history()),
        ]);
        let expected = replies.len();
        let (requests, task) = issue_2030_producer_server(listener, replies);
        let backend = issue_2030_backend(&endpoint);
        let result = issue_2192_inline_read(&backend, reader).await;
        if matches!(reader, 3 | 5) {
            let error = result.unwrap_err();
            assert!(
                !is_transient_read_failure(&error),
                "reader {reader}: {error:#}"
            );
            assert!(
                !format!("{error:#}").contains("pool timed out"),
                "reader {reader}: {error:#}"
            );
        } else {
            result.unwrap();
        }
        task.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), expected, "reader {reader}");
        assert_eq!(
            backend.client.gate().admissions(),
            2 + usize::from(reader == 5)
        );
    }
}

#[tokio::test]
async fn issue_2192_review_http200_errors_exhaust_once_per_reader() {
    let mut readers = tokio::task::JoinSet::new();
    for reader in 0..6 {
        readers.spawn(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let mut replies = Vec::new();
            if reader == 5 {
                replies.push(issue_2192_funding_anchor());
            }
            replies.extend((0..crate::params::TRANSIENT_READ_ATTEMPTS).map(|_| {
                (200, json!({"errors":[{"message":"pool timed out while waiting for an open connection"}]}))
            }));
            let expected = replies.len();
            let (requests, task) = issue_2030_producer_server(listener, replies);
            let backend = issue_2030_backend(&endpoint);
            let error = issue_2192_inline_read(&backend, reader).await.unwrap_err();
            assert!(error.downcast_ref::<GraphQlBodyError>().is_some(), "{error:#}");
            assert!(format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX));
            task.await.unwrap();
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), expected, "reader {reader}");
            assert_eq!(backend.client.gate().admissions(), expected);
            let pages = &requests[usize::from(reader == 5)..];
            assert!(pages.iter().all(|request| request == &pages[0] && request["query"].is_string()));
        });
    }
    while let Some(result) = readers.join_next().await {
        result.unwrap();
    }
}

#[tokio::test]
async fn issue_2192_review_http200_permanent_errors_are_not_retried() {
    for reader in 0..6 {
        for message in [
            "Cannot query field unknown on type Query",
            "invalid query syntax",
            "contract execution failed: insufficient SHELL",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let mut replies = Vec::new();
            if reader == 5 {
                replies.push(issue_2192_funding_anchor());
            }
            replies.push((200, json!({"errors":[{"message":message}]})));
            let expected = replies.len();
            let (requests, task) = issue_2030_producer_server(listener, replies);
            let backend = issue_2030_backend(&endpoint);
            let error = issue_2192_inline_read(&backend, reader).await.unwrap_err();
            assert!(
                error.downcast_ref::<GraphQlBodyError>().is_some(),
                "{error:#}"
            );
            assert!(format!("{error:#}").contains(message), "{error:#}");
            assert!(!is_transient_read_failure(&error));
            assert!(!format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX));
            task.await.unwrap();
            assert_eq!(requests.lock().unwrap().len(), expected, "reader {reader}");
            assert_eq!(backend.client.gate().admissions(), expected);
        }
    }
}

#[tokio::test]
async fn issue_2192_review_later_page_retry_preserves_cursor_and_decoded_orders() {
    let context = local_context().unwrap();
    let note = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let placed = |id: &str| {
        json!({
            "modelHash":"0x1234", "tokenContract":note.with_workchain(), "orderId":id,
            "clientOrderId":"0", "isBuy":false, "price":"1", "ticks":"1"
        })
    };
    let first = gosh_ackinacki::airegistry::calls::encode_internal_payload(
        &context,
        PRIVATENOTE_ABI,
        "onInferencePlaced",
        placed("11"),
    )
    .await
    .unwrap();
    let removed = gosh_ackinacki::airegistry::calls::encode_internal_payload(
        &context,
        PRIVATENOTE_ABI,
        "onInferenceOrderRemoved",
        json!({"modelHash":"0x1234", "orderId":"22", "cause":"0", "refunded":"0"}),
    )
    .await
    .unwrap();
    let last = gosh_ackinacki::airegistry::calls::encode_internal_payload(
        &context,
        PRIVATENOTE_ABI,
        "onInferencePlaced",
        placed("33"),
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let cursor = "opaque-cursor/+=2192";
    let (requests, task) = issue_2030_producer_server(
        listener,
        vec![
            (
                200,
                json!({"data":{"blockchain":{"account":{"messages":{
                    "pageInfo":{"hasPreviousPage":true,"startCursor":cursor},
                    "edges":[{"node":{"body":first}},{"node":{"body":removed}}]
                }}}}}),
            ),
            (
                200,
                json!({"errors":[{"message":"pool timed out while waiting for an open connection"}]}),
            ),
            (
                200,
                json!({"data":{"blockchain":{"account":{"messages":{
                    "pageInfo":{"hasPreviousPage":false}, "edges":[{"node":{"body":last}}]
                }}}}}),
            ),
        ],
    );
    let backend = issue_2030_backend(&endpoint);
    let (placed, removed, history) = backend
        .note_inbound_inference_order_calls(&note)
        .await
        .unwrap();
    assert_eq!(
        placed.iter().map(|call| call.order_id).collect::<Vec<_>>(),
        [11, 33]
    );
    assert_eq!(
        removed.iter().map(|call| call.order_id).collect::<Vec<_>>(),
        [22]
    );
    assert_eq!(history.messages_read, 3);
    assert!(history.reached_beginning);
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0]["variables"]["before"].is_null());
    assert_eq!(requests[1]["variables"]["before"], cursor);
    assert_eq!(requests[1], requests[2]);
    assert_eq!(backend.client.gate().admissions(), 3);
}
