use crate::market::ChainBackend as _;

fn issue_2115_clock_reply() -> Value {
    json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":local_unix_secs().unwrap()}}]}}}})
}

#[tokio::test]
async fn issue_2115_all_preflight_reads_finish_before_the_journal_boundary() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let inspect = path.clone();
    let clocks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = clocks.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let is_post = request.starts_with("POST /v2/messages ");
            let (status, reply) = if is_post {
                assert!(
                    SellerPlacement::load(&inspect).unwrap().is_some(),
                    "POST must be durable first"
                );
                (200, json!({"result":{}}))
            } else if counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                assert!(!inspect.exists());
                (200, issue_2115_clock_reply())
            } else {
                (403, json!({"error":"second clock read unavailable"}))
            };
            let body = reply.to_string();
            socket.write_all(format!("HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            if is_post {
                break;
            }
        }
    });
    let result = issue_2030_backend(&endpoint)
        .post_sell_offer_with_identity(
            &issue_2115_address('a'),
            &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            0,
            1,
            crate::params::MAX_SELL_TTL.as_secs(),
            &mut |hash| {
                let mut post = issue_2115_post();
                post.message_id = hash;
                post.save(Some(&path))
            },
        )
        .await;
    if let Err(error) = result {
        server.abort();
        panic!("no second preflight may strand a never-POSTed SELL: {error:#}");
    }
    server.await.unwrap();
    assert_eq!(clocks.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn issue_2115_failed_preflight_allows_one_healthy_post_after_restart() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(
        listener,
        vec![
            (
                200,
                json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":0}}]}}}}),
            ),
            (200, issue_2115_clock_reply()),
            (200, json!({"result":{}})),
        ],
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let mut journal = |hash| {
        let mut post = issue_2115_post();
        post.message_id = hash;
        post.save(Some(&path))
    };
    for healthy in [false, true] {
        let chain = issue_2030_backend(&endpoint);
        let result = chain
            .post_sell_offer_with_identity(
                &issue_2115_address('a'),
                &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
                0,
                1,
                crate::params::MAX_SELL_TTL.as_secs(),
                &mut journal,
            )
            .await;
        assert_eq!(result.is_ok(), healthy);
        assert_eq!(SellerPlacement::load(&path).unwrap().is_some(), healthy);
    }
    server.await.unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.is_array())
            .count(),
        1
    );
}

#[tokio::test]
async fn issue_2115_cancelled_dispatched_post_keeps_durable_identity() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requested = Arc::new(tokio::sync::Notify::new());
    let notify = requested.clone();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sell.json");
    let inspect = path.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            if request.starts_with("POST /v2/messages ") {
                let body: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                assert_eq!(
                    SellerPlacement::load(&inspect).unwrap().unwrap().message_id,
                    external_message_hash(body[0]["body"].as_str().unwrap()).unwrap()
                );
                notify.notify_one();
                std::future::pending::<()>().await;
            }
            let body = issue_2115_clock_reply().to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let chain = issue_2030_backend(&endpoint);
    let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
    let note = issue_2115_address('a');
    let mut journal = |hash| {
        let mut post = issue_2115_post();
        post.message_id = hash;
        post.save(Some(&path))
    };
    {
        let submit = chain.post_sell_offer_with_identity(
            &note,
            &keys,
            0,
            1,
            crate::params::MAX_SELL_TTL.as_secs(),
            &mut journal,
        );
        tokio::pin!(submit);
        tokio::select! {
            _ = requested.notified() => {},
            result = &mut submit => panic!("POST must remain unacknowledged: {result:?}"),
        }
    }
    assert!(SellerPlacement::load(&path).unwrap().is_some());
    server.abort();
}

/// Real compiled getters/storage, served through the ordinary chain HTTP boundary.
fn issue_2115_account_fixture(address: &Address, abi: &str, tvc: &[u8], overrides: Value) -> Value {
    use tvm_abi::token::Tokenizer;
    use tvm_block::{
        Account as TvmAccount, CurrencyCollection, MsgAddressInt, Serializable, StateInit,
    };
    let mut layout: Value = serde_json::from_str(abi).unwrap();
    for field in layout["fields"].as_array_mut().unwrap() {
        field["init"] = json!(false);
    }
    let defaults = tvm_abi::Contract::load(layout.to_string().as_bytes())
        .unwrap()
        .encode_storage_fields(Default::default())
        .unwrap()
        .into_cell()
        .unwrap();
    let contract = tvm_abi::Contract::load(abi.as_bytes()).unwrap();
    let mut tokens = contract
        .decode_storage_fields(tvm_types::SliceData::load_cell(defaults).unwrap(), false)
        .unwrap();
    for token in &mut tokens {
        if let Some(value) = overrides.get(&token.name) {
            let param = contract
                .fields()
                .iter()
                .find(|p| p.name == token.name)
                .unwrap();
            token.value = Tokenizer::tokenize_parameter(&param.kind, value, &token.name).unwrap();
        }
    }
    let mut state =
        StateInit::construct_from_cell(tvm_types::read_single_root_boc(tvc).unwrap()).unwrap();
    state.data = Some(
        tvm_abi::TokenValue::pack_values_into_chain(&tokens, vec![], contract.version())
            .unwrap()
            .into_cell()
            .unwrap(),
    );
    let account = TvmAccount::active_by_init_code_hash(
        address.with_workchain().parse::<MsgAddressInt>().unwrap(),
        CurrencyCollection::from(1_000_000_000u64),
        0,
        state,
        false,
    )
    .unwrap();
    json!({"address":address.bare(),"acc_type_name":"Active",
        "balance":"0x3b9aca00", "balance_other":[{"currency":crate::params::SHELL_CURRENCY_ID,"value":format!("0x{:x}", crate::params::deal_gas_health_target_raw(100))}],
        "boc":base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&account.serialize().unwrap()).unwrap())})
}

#[derive(Default)]
struct Issue2115RestingState {
    order: Option<u128>,
    pruned: bool,
    posts: Vec<String>,
    roots_read: usize,
    unknown: bool,
    reject_posts: bool,
    invalid_post_reply: bool,
    rejected_hashes: Vec<String>,
    post_replies: std::collections::VecDeque<(u16, Value)>,
}

async fn issue_2115_resting_backend() -> (
    super::super::backends::RealSellerBackend,
    Arc<Mutex<Issue2115RestingState>>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
    Address,
    String,
) {
    let model = model_hash_for("fixture--model--v1");
    let book = RealChainBackend::canonical_inference_orderbook_address(&model).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(Issue2115RestingState {
        order: Some(102),
        ..Default::default()
    }));
    let server_state = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server_book = book.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let input: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let mut status = 200;
            let mut invalid_payload = false;
            let reply = {
                let mut state = server_state.lock().unwrap();
                if input.is_array() {
                    let hash = external_message_hash(input[0]["body"].as_str().unwrap()).unwrap();
                    if state.invalid_post_reply {
                        invalid_payload = true;
                        state.rejected_hashes.push(hash);
                        Value::Null
                    } else if let Some((reply_status, reply)) = state.post_replies.pop_front() {
                        status = reply_status;
                        state.rejected_hashes.push(hash);
                        reply
                    } else if state.reject_posts {
                        state.rejected_hashes.push(hash);
                        json!({"error":{"code":"INVALID_ARGUMENT", "message":"fixture conclusive producer refusal before enqueue"}})
                    } else {
                        state.posts.push(hash);
                        state.order = Some(103);
                        json!({"result":{}})
                    }
                } else if let Some(hash) = input["variables"]["hash"].as_str() {
                    let initial = hash == "1".repeat(64);
                    if initial { state.roots_read += 1; }
                    if state.unknown || (initial && state.pruned)
                        || (state.rejected_hashes.iter().any(|h| h == hash) && !state.posts.iter().any(|h| h == hash)) {
                        json!({"data":{"blockchain":{"message":null}}})
                    } else {
                        let mut reply = if initial || state.posts.iter().any(|h| h == hash) {
                            let mut reply = issue_2115_message('1', None, Some('a'), &['2']);
                            reply["data"]["blockchain"]["message"]["id"] = json!(hash); reply
                        } else if hash == "2".repeat(64) {
                            issue_2115_message('2', Some('a'), Some('c'), &['3'])
                        } else if hash == "3".repeat(64) {
                            issue_2115_message('3', Some('c'), Some('b'), &['5'])
                        } else {
                            assert_eq!(hash, "5".repeat(64));
                            issue_2115_placement_event_for(state.order.unwrap())
                        };
                        let node = &mut reply["data"]["blockchain"]["message"];
                        for field in ["src", "dst"] {
                            if node[field].as_str() == Some(issue_2115_address('b').with_workchain().as_str()) {
                                node[field] = json!(server_book.with_workchain());
                            }
                        }
                        reply
                    }
                } else if input["query"].as_str().unwrap().contains("blocks") {
                    issue_2115_clock_reply()
                } else {
                    let query = input["query"].as_str().unwrap();
                    let note = issue_2115_address('a');
                    let tc = issue_2115_address('c');
                    let info = if query.contains(note.bare()) {
                        issue_2115_account_fixture(&note, PRIVATENOTE_ABI, PRIVATENOTE_TVC, json!({
                            "_constructorFlag":true,"_ephemeralPubkey":format!("0x{}", KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap().public_hex()),
                            "_inferenceOrderBookCode":RealChainBackend::inference_orderbook_code_b64().unwrap()
                        }))
                    } else if query.contains(tc.bare()) {
                        issue_2115_account_fixture(&tc, TOKENCONTRACT_ABI, TOKENCONTRACT_TVC, json!({
                            "_constructorFlag":true,"_modelName":"fixture--model--v1","_modelHash":model,
                            "_pricePerTick":PRICE_STEP.to_string(),"_maxTicks":"100","_sellerNote":note.with_workchain(),
                            "_offerPosted":state.order.is_some()
                        }))
                    } else {
                        assert!(query.contains(server_book.bare()), "unexpected query: {query}");
                        let orders = state.order.map(|id| json!({id.to_string():{
                            "note":note.with_workchain(),"buyerPubkey":"0","tokenContract":tc.with_workchain(),
                            "price":PRICE_STEP.to_string(),"amount":"100","initialAmount":"100","filledAccum":"0","escrow":"0",
                            "deadline":(local_unix_secs().unwrap()+crate::params::MAX_SELL_TTL.as_secs()).to_string(),
                            "clientOrderId":"0","ts":"1","flags":"0","isBuy":false,
                            "nextAtPrice":"0","prevAtPrice":"0","nextInOwner":"0","prevInOwner":"0"
                        }})).unwrap_or(json!({}));
                        issue_2115_account_fixture(&server_book, INFERENCE_ORDERBOOK_ABI, INFERENCE_ORDERBOOK_TVC, json!({
                            "_constructorFlag":true,"_modelHash":model,"_modelName":"fixture--model--v1", "_orders":orders,
                            "_orderCount":u8::from(state.order.is_some()).to_string(),"_nextOrderId":"104"
                        }))
                    };
                    json!({"data":{"blockchain":{"account":{"info":info}}}})
                }
            };
            let reply = if invalid_payload { "{".to_string() } else { reply.to_string() };
            socket.write_all(format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
        }
    });
    let chain = issue_2030_backend(&endpoint);
    let mut post = issue_2115_post();
    post.network = chain.network().to_string();
    post.order_book = book.with_workchain();
    post.save(Some(
        &directory
            .path()
            .join(format!("{}.sell.json", issue_2115_address('c').bare())),
    ))
    .unwrap();
    let seller = super::super::backends::RealSellerBackend::new(
        chain,
        issue_2115_address('a'),
        KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        model_hash_for("fixture--model--v1"),
        "fixture--model--v1".into(),
        1,
        TICK_SIZE,
    )
    .with_offer_journal_dir(directory.path().to_path_buf());
    (seller, state, server, directory, book, endpoint)
}

include!("issue_2115_acceptance_tests.rs");

#[tokio::test]
async fn issue_2115_normal_rested_and_restart_checkpoint_before_history_pruning() {
    for restart_before_confirmation in [false, true] {
        let (seller, state, server, directory, book, endpoint) = issue_2115_resting_backend().await;
        let tc = issue_2115_address('c').with_workchain();
        if restart_before_confirmation {
            // This is the production inspection used by prepare_seller_offer -> ResumedResting.
            let rows = seller.raw_resting_sell_orders_for_tc(&tc).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].order_id, 102);
        } else {
            assert!(matches!(
                seller.confirm_offer_outcome(&tc).await.unwrap(),
                Some(crate::market::SellOfferOutcome::Rested { order_id: 102 })
            ));
        }
        let path = directory
            .path()
            .join(format!("{}.sell.json", issue_2115_address('c').bare()));
        assert_eq!(
            SellerPlacement::load(&path)
                .unwrap()
                .unwrap()
                .accepted_order_id,
            Some(102)
        );
        let roots = state.lock().unwrap().roots_read;
        assert!(roots > 0);
        {
            let mut state = state.lock().unwrap();
            state.pruned = true;
            state.order = None;
        }
        let seller = super::super::backends::RealSellerBackend::new(
            issue_2030_backend(&endpoint),
            issue_2115_address('a'),
            KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            model_hash_for("fixture--model--v1"),
            "fixture--model--v1".into(),
            1,
            TICK_SIZE,
        )
        .with_offer_journal_dir(directory.path().to_path_buf());
        let note = crate::note::LocalNote::generate();
        seller
            .post_offer(
                crate::market::SellOffer {
                    token_contract: tc.clone(),
                    price_per_tick: PRICE_STEP.try_into().unwrap(),
                    max_ticks: 100,
                    flags: 0,
                },
                &note,
            )
            .await
            .unwrap();
        assert!(matches!(
            seller.confirm_offer_outcome(&tc).await.unwrap(),
            Some(crate::market::SellOfferOutcome::Rested { order_id: 103 })
        ));
        let rows = seller.raw_resting_sell_orders_for_tc(&tc).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].order_id, rows[0].price_per_tick, rows[0].ticks),
            (103, PRICE_STEP, 100)
        );
        assert!(rows[0].deadline > local_unix_secs().unwrap());
        assert_eq!(rows[0].token_contract.as_deref(), Some(tc.as_str()));
        assert_eq!(state.lock().unwrap().posts.len(), 1);
        assert_eq!(
            state.lock().unwrap().roots_read,
            roots,
            "pruned predecessor must not be read again"
        );
        assert!(seller
            .reconcile_pending_offer(&issue_2115_address('c'), &book)
            .await
            .unwrap());
        server.abort();
    }
}

#[tokio::test]
async fn issue_2115_unknown_trace_cannot_be_attributed_to_a_visible_row_or_empty_book() {
    let (seller, state, server, directory, book, _) = issue_2115_resting_backend().await;
    let tc = issue_2115_address('c');
    state.lock().unwrap().unknown = true;
    assert_eq!(
        seller
            .raw_resting_sell_orders_for_tc(&tc.with_workchain())
            .await
            .unwrap()
            .len(),
        1
    );
    let path = directory.path().join(format!("{}.sell.json", tc.bare()));
    assert!(SellerPlacement::load(&path)
        .unwrap()
        .unwrap()
        .terminal
        .is_none());
    state.lock().unwrap().order = None;
    assert!(matches!(
        seller.reconcile_pending_offer(&tc, &book).await,
        Err(crate::ChainError::AmbiguousSubmit(_))
    ));
    assert!(SellerPlacement::load(&path)
        .unwrap()
        .unwrap()
        .terminal
        .is_none());
    assert!(state.lock().unwrap().posts.is_empty());
    server.abort();
}
