// Exercise recovery through the production seller backend and compiled contract getters.
fn issue_2230_body(abi: &str, method: &str, args: Value, internal: bool) -> String {
    let contract = tvm_abi::Contract::load(abi.as_bytes()).unwrap();
    let function = contract.function(method).unwrap();
    let tokens = tvm_abi::token::Tokenizer::tokenize_all_params(&function.inputs, &args).unwrap();
    let cell = function
        .encode_input(&Default::default(), &tokens, internal, None, None)
        .unwrap()
        .into_cell()
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&cell).unwrap())
}

#[derive(Default)]
struct Issue2230State {
    reserve: u128,
    order: bool,
    funded: bool,
    latched: bool,
    unknown: bool,
    mutation: Option<&'static str>,
    posts: Vec<String>,
    hold_post: bool,
    post_seen: Arc<tokio::sync::Notify>,
    fail_clock: bool,
    clock_reads: usize,
}

async fn issue_2230_backend() -> (
    super::super::backends::RealSellerBackend,
    Arc<Mutex<Issue2230State>>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
    String,
) {
    let model = model_hash_for("fixture--model--v1");
    let book = RealChainBackend::canonical_inference_orderbook_address(&model).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(Issue2230State::default()));
    let server_state = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server_path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let root_boc = RealChainBackend::encode_signed_call_boc(
        &issue_2115_address('a'), PRIVATENOTE_ABI, "postSellOffer",
        json!({"flags":crate::market::flags::AON,"nonce":"1","ttl":crate::params::MAX_SELL_TTL.as_secs().to_string()}),
        &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
    ).await.unwrap();
    let root_hash = external_message_hash(&root_boc).unwrap();
    let saved_root_hash = root_hash.clone();
    let foreign_boc = RealChainBackend::encode_signed_call_boc(
        &issue_2115_address('f'), PRIVATENOTE_ABI, "postSellOffer",
        json!({"flags":crate::market::flags::AON,"nonce":"1","ttl":crate::params::MAX_SELL_TTL.as_secs().to_string()}),
        &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
    ).await.unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let input: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let reply = {
                let mut state = server_state.lock().unwrap();
                let note = issue_2115_address('a');
                let tc = issue_2115_address('c');
                if input.is_array() {
                    let hash = external_message_hash(input[0]["body"].as_str().unwrap()).unwrap();
                    assert!(decode_external_abi_message_boc(
                        input[0]["body"].as_str().unwrap(),
                        PRIVATENOTE_ABI,
                        true
                    )
                    .is_some_and(|call| call.function_name == "postSellOffer"
                        && decoded_u128(&call.tokens, "flags")
                            == Some(u128::from(crate::market::flags::AON))));
                    assert_eq!(
                        SellerPlacement::load(&server_path)
                            .unwrap()
                            .unwrap()
                            .message_id,
                        hash,
                        "new identity must be durable before POST"
                    );
                    let archived = SellerPlacement::load(
                        &server_path.with_extension(format!("{root_hash}.rejected.json")),
                    )
                    .unwrap()
                    .unwrap();
                    assert_eq!(archived.message_id, root_hash);
                    assert!(archived
                        .tc_rejection()
                        .unwrap()
                        .contains("historical observed_reserve_raw=0"));
                    assert!(archived
                        .tc_rejection()
                        .unwrap()
                        .contains("no_funds=true out_msgs=[]"));
                    state.posts.push(hash);
                    state.order = !state.hold_post;
                    json!({"result":{}})
                } else if let Some(hash) = input["variables"]["hash"].as_str() {
                    if state.unknown {
                        json!({"data":{"blockchain":{"message":null}}})
                    } else {
                        let mut reply = if state.posts.iter().any(|id| id == hash) {
                            let mut root = issue_2115_message('1', None, Some('a'), &['3']);
                            root["data"]["blockchain"]["message"]["id"] = json!(hash);
                            root
                        } else if hash == "3".repeat(64) {
                            issue_2115_message('3', Some('a'), Some('c'), &['4'])
                        } else if hash == "4".repeat(64) {
                            issue_2115_message('4', Some('c'), Some('b'), &['5'])
                        } else if hash == "5".repeat(64) {
                            issue_2115_placement_event_for(103)
                        } else if hash == root_hash {
                            let mut root = issue_2115_message('1', None, Some('a'), &['2']);
                            root["data"]["blockchain"]["message"]["id"] = json!(root_hash);
                            root["data"]["blockchain"]["message"]["body"] = json!(issue_2230_body(
                                PRIVATENOTE_ABI,
                                "postSellOffer",
                                json!({"flags":crate::market::flags::AON,"nonce":"1","ttl":crate::params::MAX_SELL_TTL.as_secs().to_string()}),
                                false
                            ));
                            root
                        } else {
                            assert_eq!(hash, "2".repeat(64));
                            let mut child = issue_2115_message('2', Some('a'), Some('c'), &[]);
                            let node = &mut child["data"]["blockchain"]["message"];
                            node["body"] = json!(issue_2230_body(
                                TOKENCONTRACT_ABI,
                                "postFromNote",
                                json!({"iobHash":"0x1","iobDepth":1,"depositIdentifierHash":"0x1","flags":crate::market::flags::AON,"deadline":"1"}),
                                true
                            ));
                            node["dst_transaction"]["aborted"] = json!(true);
                            node["dst_transaction"]["action"] =
                                json!({"success":false,"result_code":38,"no_funds":true});
                            child
                        };
                        let node = &mut reply["data"]["blockchain"]["message"];
                        for field in ["src", "dst"] {
                            if node[field].as_str()
                                == Some(issue_2115_address('b').with_workchain().as_str())
                            {
                                node[field] = json!(book.with_workchain());
                            }
                        }
                        if hash == root_hash {
                            match state.mutation {
                                Some("root_boc") => {
                                    node["body"] = Value::Null;
                                    node["boc"] = json!(root_boc);
                                }
                                Some("bad_boc") => {
                                    node["body"] = Value::Null;
                                    node["boc"] = json!("broken");
                                }
                                Some("foreign_boc") => {
                                    node["body"] = Value::Null;
                                    node["boc"] = json!(foreign_boc);
                                }
                                Some("root_unfinalized") => {
                                    node["dst_transaction"]["status"] = json!(1)
                                }
                                Some("root_dispatch") => {
                                    node["dst_transaction"]["out_msgs"] =
                                        json!(["2".repeat(64), "3".repeat(64)])
                                }
                                Some("root_nonce") => {
                                    node["body"] = json!(issue_2230_body(
                                        PRIVATENOTE_ABI,
                                        "postSellOffer",
                                        json!({"flags":crate::market::flags::AON,"nonce":"2","ttl":crate::params::MAX_SELL_TTL.as_secs().to_string()}),
                                        false
                                    ))
                                }
                                _ => {}
                            }
                        }
                        if hash == "2".repeat(64) {
                            match state.mutation {
                                Some("unfinalized") => node["dst_transaction"]["status"] = json!(1),
                                Some("foreign") => {
                                    node["src"] = json!(issue_2115_address('f').with_workchain())
                                }
                                Some("compute") => {
                                    node["dst_transaction"]["compute"]["exit_code"] = json!(101)
                                }
                                Some("no_funds") => {
                                    node["dst_transaction"]["action"]["no_funds"] = Value::Null
                                }
                                Some("dispatch") => {
                                    node["dst_transaction"]["out_msgs"] = json!(["3".repeat(64)])
                                }
                                Some("body") => node["body"] = json!("broken"),
                                Some("success") => {
                                    node["dst_transaction"]["action"]["success"] = json!(true)
                                }
                                Some("aborted") => {
                                    node["dst_transaction"]["aborted"] = json!(false)
                                }
                                Some("transaction") => node["dst_transaction"]["id"] = Value::Null,
                                _ => {}
                            }
                        }
                        reply
                    }
                } else if input["query"].as_str().unwrap().contains("blocks") {
                    state.clock_reads += 1;
                    let mut reply = issue_2115_clock_reply();
                    if state.fail_clock {
                        reply["data"]["blockchain"]["blocks"]["edges"][0]["node"]["gen_utime"] =
                            json!(0);
                    }
                    reply
                } else {
                    let query = input["query"].as_str().unwrap();
                    let info = if query.contains(note.bare()) {
                        issue_2115_account_fixture(
                            &note,
                            PRIVATENOTE_ABI,
                            PRIVATENOTE_TVC,
                            json!({
                                "_constructorFlag":true,"_ephemeralPubkey":format!("0x{}", KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap().public_hex()),
                                "_inferenceOrderBookCode":RealChainBackend::inference_orderbook_code_b64().unwrap()
                            }),
                        )
                    } else if query.contains(tc.bare()) {
                        let mut info = issue_2115_account_fixture(
                            &tc,
                            TOKENCONTRACT_ABI,
                            TOKENCONTRACT_TVC,
                            json!({
                                "_constructorFlag":true,"_modelName":"fixture--model--v1","_modelHash":model,
                                "_pricePerTick":PRICE_STEP.to_string(),"_maxTicks":"100","_sellerNote":note.with_workchain(),
                                "_offerPosted":state.latched || state.order,"_funded":state.funded
                            }),
                        );
                        info["balance_other"] = json!([{"currency":crate::params::SHELL_CURRENCY_ID,"value":format!("0x{:x}", state.reserve)}]);
                        info
                    } else {
                        assert!(query.contains(book.bare()), "unexpected query: {query}");
                        let orders = if state.order {
                            json!({"103":{
                                "note":note.with_workchain(),"buyerPubkey":"0","tokenContract":tc.with_workchain(),
                                "price":PRICE_STEP.to_string(),"amount":"100","initialAmount":"100","filledAccum":"0","escrow":"0",
                                "deadline":(local_unix_secs().unwrap()+crate::params::MAX_SELL_TTL.as_secs()).to_string(),
                                "clientOrderId":"0","ts":"1","flags":crate::market::flags::AON.to_string(),"isBuy":false,
                                "nextAtPrice":"0","prevAtPrice":"0","nextInOwner":"0","prevInOwner":"0"
                            }})
                        } else {
                            json!({})
                        };
                        issue_2115_account_fixture(
                            &book,
                            INFERENCE_ORDERBOOK_ABI,
                            INFERENCE_ORDERBOOK_TVC,
                            json!({
                                "_constructorFlag":true,"_modelHash":model,"_modelName":"fixture--model--v1","_orders":orders,
                                "_orderCount":u8::from(state.order).to_string(),"_nextOrderId":"104"
                            }),
                        )
                    };
                    json!({"data":{"blockchain":{"account":{"info":info}}}})
                }
            };
            if input.is_array() && server_state.lock().unwrap().hold_post {
                server_state.lock().unwrap().post_seen.notify_one();
                tokio::spawn(async move {
                    std::future::pending::<()>().await;
                    drop(socket);
                });
                continue;
            }
            let body = reply.to_string();
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
    let mut post = serde_json::to_value(issue_2115_post()).unwrap();
    post["network"] = json!(chain.network());
    post["message_id"] = json!(saved_root_hash);
    post["order_book"] = json!(RealChainBackend::canonical_inference_orderbook_address(
        &model_hash_for("fixture--model--v1")
    )
    .unwrap()
    .with_workchain());
    post["rejection"] = json!("postFromNote was rejected; historical observed_reserve_raw=0");
    post["terminal"] = json!("finalized rejection");
    post["pending"] = json!([]);
    serde_json::from_value::<SellerPlacement>(post)
        .unwrap()
        .save(Some(&directory.path().join(format!(
            "{}.sell.json",
            issue_2115_address('c').bare()
        ))))
        .unwrap();
    let seller = issue_2230_restarted(&endpoint, directory.path());
    (seller, state, server, directory, endpoint)
}

fn issue_2230_restarted(
    endpoint: &str,
    directory: &Path,
) -> super::super::backends::RealSellerBackend {
    super::super::backends::RealSellerBackend::new(
        issue_2030_backend(endpoint),
        issue_2115_address('a'),
        KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        model_hash_for("fixture--model--v1"),
        "fixture--model--v1".into(),
        1,
        TICK_SIZE,
    )
    .with_offer_journal_dir(directory.to_path_buf())
}

fn issue_2230_offer() -> crate::market::SellOffer {
    crate::market::SellOffer {
        token_contract: issue_2115_address('c').with_workchain(),
        price_per_tick: PRICE_STEP.try_into().unwrap(),
        max_ticks: 100,
        flags: crate::market::flags::AON,
    }
}

#[tokio::test]
async fn issue_2230_replenished_rejection_recovers_once_through_normal_post_and_restart() {
    let (seller, state, server, directory, endpoint) = issue_2230_backend().await;
    // Live GraphQL external calls require the full message BOC fallback.
    state.lock().unwrap().mutation = Some("root_boc");
    let path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let original_hash = SellerPlacement::load(&path).unwrap().unwrap().message_id;
    let note = crate::note::LocalNote::generate();
    assert!(seller.post_offer(issue_2230_offer(), &note).await.is_err());
    assert!(state.lock().unwrap().posts.is_empty());
    state.lock().unwrap().reserve = crate::params::DEAL_BURN_POST_FROM_NOTE_RAW;
    seller.post_offer(issue_2230_offer(), &note).await.unwrap();
    assert_eq!(state.lock().unwrap().posts.len(), 1);
    let rows = seller
        .raw_resting_sell_orders_for_tc(&issue_2115_address('c').with_workchain())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].price_per_tick, rows[0].ticks, rows[0].flags),
        (PRICE_STEP, 100, crate::market::flags::AON)
    );
    let path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let saved = SellerPlacement::load(&path).unwrap().unwrap();
    assert_eq!(saved.message_id, state.lock().unwrap().posts[0]);
    assert_ne!(saved.message_id, original_hash);
    assert!(path
        .with_extension(format!("{original_hash}.rejected.json"))
        .exists());
    drop(seller);
    let restarted = issue_2230_restarted(&endpoint, directory.path());
    restarted
        .post_offer(issue_2230_offer(), &note)
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().posts.len(), 1);
    server.abort();
}

#[tokio::test]
async fn issue_2230_refuses_unproven_receipts_existing_orders_matches_and_insufficient_reserve() {
    for case in [
        "unfinalized",
        "foreign",
        "compute",
        "no_funds",
        "dispatch",
        "body",
        "success",
        "aborted",
        "transaction",
        "root_unfinalized",
        "root_dispatch",
        "root_nonce",
        "unknown",
        "order",
        "funded",
        "latched",
        "underfunded",
        "flags",
        "bad_boc",
        "foreign_boc",
    ] {
        let (seller, state, server, directory, _) = issue_2230_backend().await;
        {
            let mut state = state.lock().unwrap();
            state.reserve = crate::params::DEAL_BURN_POST_FROM_NOTE_RAW;
            match case {
                "unknown" => state.unknown = true,
                "order" => state.order = true,
                "funded" => state.funded = true,
                "latched" => state.latched = true,
                "underfunded" => state.reserve -= 1,
                "flags" => {}
                case => state.mutation = Some(case),
            }
        }
        let path = directory
            .path()
            .join(format!("{}.sell.json", issue_2115_address('c').bare()));
        let before = std::fs::read(&path).unwrap();
        let mut offer = issue_2230_offer();
        if case == "flags" {
            offer.flags = 0;
        }
        let error = seller
            .post_offer(offer, &crate::note::LocalNote::generate())
            .await
            .unwrap_err();
        if case == "underfunded" {
            assert!(error.to_string().contains(&format!(
                "current_reserve_raw={}",
                crate::params::DEAL_BURN_POST_FROM_NOTE_RAW - 1
            )));
            assert!(error
                .to_string()
                .contains("historical observed_reserve_raw=0"));
        }
        assert!(state.lock().unwrap().posts.is_empty(), "{case}: {error}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{case}: original journal must survive"
        );
        server.abort();
    }
}

#[tokio::test]
async fn issue_2230_preparation_failure_retains_original_journal_and_flags_after_restart() {
    let (seller, state, server, directory, endpoint) = issue_2230_backend().await;
    {
        let mut state = state.lock().unwrap();
        state.reserve = crate::params::DEAL_BURN_POST_FROM_NOTE_RAW;
        state.fail_clock = true;
    }
    let path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let before = std::fs::read(&path).unwrap();
    let original_hash = SellerPlacement::load(&path).unwrap().unwrap().message_id;
    let note = crate::note::LocalNote::generate();
    let error = seller
        .post_offer(issue_2230_offer(), &note)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("before any message POST"),
        "{error}"
    );
    assert_eq!(
        state.lock().unwrap().clock_reads,
        1,
        "recovery must reach preparation"
    );
    assert!(state.lock().unwrap().posts.is_empty());
    assert!(path
        .with_extension(format!("{original_hash}.rejected.json"))
        .exists());
    assert_eq!(
        std::fs::read(&path).expect("preparation failure must retain the original active journal"),
        before
    );

    drop(seller);
    let restarted = issue_2230_restarted(&endpoint, directory.path());
    let mut changed = issue_2230_offer();
    changed.flags = 0;
    let error = restarted.post_offer(changed, &note).await.unwrap_err();
    assert!(
        error.to_string().contains("unchanged flags/nonce"),
        "{error}"
    );
    assert_eq!(
        state.lock().unwrap().clock_reads,
        1,
        "changed flags must fail before preparation"
    );
    assert!(state.lock().unwrap().posts.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);

    state.lock().unwrap().fail_clock = false;
    restarted
        .post_offer(issue_2230_offer(), &note)
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().posts.len(), 1);
    let rows = restarted
        .raw_resting_sell_orders_for_tc(&issue_2115_address('c').with_workchain())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].price_per_tick, rows[0].ticks, rows[0].flags),
        (PRICE_STEP, 100, crate::market::flags::AON)
    );
    server.abort();
}

#[tokio::test]
async fn issue_2230_failed_successor_checkpoint_keeps_old_attempt_recoverable() {
    let (seller, state, server, directory, endpoint) = issue_2230_backend().await;
    state.lock().unwrap().reserve = crate::params::DEAL_BURN_POST_FROM_NOTE_RAW;
    let path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let before = std::fs::read(&path).unwrap();
    let blocked = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::create_dir(&blocked).unwrap();
    assert!(seller
        .post_offer(issue_2230_offer(), &crate::note::LocalNote::generate())
        .await
        .is_err());
    assert!(state.lock().unwrap().posts.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::remove_dir(blocked).unwrap();
    drop(seller);
    let restarted = issue_2230_restarted(&endpoint, directory.path());
    restarted
        .post_offer(issue_2230_offer(), &crate::note::LocalNote::generate())
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().posts.len(), 1);
    server.abort();
}

#[tokio::test]
async fn issue_2230_failed_rejection_archive_does_not_authorize_a_successor() {
    let (seller, state, server, directory, _) = issue_2230_backend().await;
    state.lock().unwrap().reserve = crate::params::DEAL_BURN_POST_FROM_NOTE_RAW;
    let path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let before = std::fs::read(&path).unwrap();
    let original_hash = SellerPlacement::load(&path).unwrap().unwrap().message_id;
    std::fs::create_dir(path.with_extension(format!("{original_hash}.rejected.json"))).unwrap();
    assert!(seller
        .post_offer(issue_2230_offer(), &crate::note::LocalNote::generate())
        .await
        .is_err());
    assert!(state.lock().unwrap().posts.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    server.abort();
}

#[tokio::test]
async fn issue_2230_interrupted_successor_is_not_resubmitted_on_restart() {
    let (seller, state, server, directory, endpoint) = issue_2230_backend().await;
    let observed = {
        let mut state = state.lock().unwrap();
        state.reserve = crate::params::DEAL_BURN_POST_FROM_NOTE_RAW;
        state.hold_post = true;
        state.post_seen.clone()
    };
    let note = crate::note::LocalNote::generate();
    {
        let attempt = seller.post_offer(issue_2230_offer(), &note);
        tokio::pin!(attempt);
        tokio::select! {
            _ = observed.notified() => {},
            result = &mut attempt => panic!("POST must still be unacknowledged: {result:?}"),
        }
    }
    state.lock().unwrap().unknown = true;
    let path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let successor = SellerPlacement::load(&path).unwrap().unwrap().message_id;
    assert_eq!(successor, state.lock().unwrap().posts[0]);
    drop(seller);
    let restarted = issue_2230_restarted(&endpoint, directory.path());
    assert!(restarted
        .post_offer(issue_2230_offer(), &note)
        .await
        .is_err());
    assert_eq!(state.lock().unwrap().posts.len(), 1);
    assert_eq!(
        SellerPlacement::load(&path).unwrap().unwrap().message_id,
        successor
    );
    server.abort();
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]
    #[test]
    fn issue_2230_unknown_execution_never_resubmits_after_topups_and_restarts(
        reserve in proptest::num::u64::ANY,
        retries in 1usize..4,
    ) {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (seller, state, server, directory, endpoint) = issue_2230_backend().await;
            drop(seller);
            let path = directory.path().join(format!("{}.sell.json", issue_2115_address('c').bare()));
            let mut original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            original["rejection"] = Value::Null;
            original["terminal"] = Value::Null;
            let original_hash = original["message_id"].as_str().unwrap().to_string();
            original["pending"] = json!([[original_hash, null]]);
            serde_json::from_value::<SellerPlacement>(original).unwrap().save(Some(&path)).unwrap();
            state.lock().unwrap().unknown = true;
            for attempt in 0..retries {
                state.lock().unwrap().reserve = u128::from(reserve) + attempt as u128;
                let restarted = issue_2230_restarted(&endpoint, directory.path());
                assert!(restarted.post_offer(issue_2230_offer(), &crate::note::LocalNote::generate()).await.is_err());
                assert!(state.lock().unwrap().posts.is_empty());
                assert_eq!(SellerPlacement::load(&path).unwrap().unwrap().message_id, original_hash);
            }
            server.abort();
        });
    }
}
