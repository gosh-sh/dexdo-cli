// Regression coverage for consuming the initially funded reserve without seller-note refills.
#[test]
fn issue_2199_seller_and_provision_do_not_maintain_the_initial_reserve() {
    let backends = include_str!("../backends.rs");
    let client = include_str!("../client.rs");
    assert!(!backends.contains("ensure_deal_contract_gas("));
    assert!(!backends.contains("ensure_tc_gas("));
    let production_client = client.split("#[cfg(test)]\nmod tests").next().unwrap();
    assert!(!production_client.contains("fn ensure_deal_contract_gas"));
}

#[tokio::test]
async fn issue_2199_seller_accepts_probe_at_and_below_former_floor_without_note_funding() {
    use crate::market::ChainBackend as _;
    let max_ticks = 57_500;
    let floor = crate::params::deal_gas_requirement_raw(max_ticks);
    for reserve in [floor, floor - crate::params::DEAL_BURN_CLAIM_RAW] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let tc = issue_2115_address('c');
        let note = issue_2115_address('a');
        let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
        let mut info = issue_2115_account_fixture(
            &tc,
            TOKENCONTRACT_ABI,
            TOKENCONTRACT_TVC,
            json!({"_constructorFlag":true,"_opened":true,"_probeAccepted":true,
                "_maxTicks":max_ticks.to_string()}),
        );
        info["balance_other"] =
            json!([{"currency":crate::params::SHELL_CURRENCY_ID,"value":format!("0x{reserve:x}")}]);
        let (requests, server) = issue_2030_producer_server(
            listener,
            vec![
                (200, issue_2115_clock_reply()),
                (200, json!({"result":{}})),
                (
                    200,
                    json!({"data":{"blockchain":{"account":{"info":info}}}}),
                ),
            ],
        );
        let seller = super::super::backends::RealSellerBackend::new(
            issue_2030_backend(&endpoint),
            note,
            keys,
            model_hash_for("fixture"),
            "fixture".into(),
            1,
            crate::params::TICK_SIZE,
        );
        seller.accept_probe(&tc.with_workchain()).await.unwrap();
        server.await.unwrap();
        let requests = requests.lock().unwrap();
        let posts: Vec<_> = requests
            .iter()
            .filter(|request| request.is_array())
            .collect();
        assert_eq!(posts.len(), 1, "one TC operation, no fundDeployShell");
        assert_eq!(
            dest_account_id_hex(posts[0][0]["body"].as_str().unwrap()).unwrap(),
            tc.bare()
        );
        assert_eq!(
            requests.len(),
            3,
            "no note or reserve-health preflight reads"
        );
    }
}

fn issue_2199_reserve_receipt(balance: u128) -> CorrelatedActionReceipt {
    CorrelatedActionReceipt {
        message_hash: "abcd".into(),
        transaction_hash: Some("tx2199".into()),
        aborted: Some(true),
        compute_exit_code: Some(0),
        action_success: Some(false),
        result_code: Some(38),
        no_funds: Some(true),
        account_ecc_balances: Some(vec![(crate::params::SHELL_CURRENCY_ID, balance)]),
        ..Default::default()
    }
}

#[test]
fn issue_2199_receipt_currency_ids_accept_integral_floats_without_truncating_bad_ids() {
    let tc = issue_2115_address('c');
    for currency in [
        json!(2),
        json!(2.0),
        json!(2.5),
        json!(-1.0),
        json!(4294967296.0),
    ] {
        let valid = currency == json!(2) || currency == json!(2.0);
        let response = json!({"data":{"blockchain":{
            "message":{"id":"abcd","dst":tc.with_workchain(),
                "dst_transaction":{"id":"tx2199","status":3,"aborted":true,
                    "account_addr":tc.with_workchain(),"compute":{"exit_code":0},
                    "action":{"result_code":38,"success":false,"no_funds":true}}},
            "account":{"info":{"id":tc.bare(),"dapp_id":crate::address::DEXDO_DAPP_ID,
                "balance_other":[{"currency":currency,"value":"0x1"}]}}
        }}});
        let receipt = parse_exact_destination_receipt(
            &response,
            tc.bare(),
            crate::address::DEXDO_DAPP_ID,
            "abcd",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            receipt.account_ecc_balances,
            valid.then_some(vec![(crate::params::SHELL_CURRENCY_ID, 1)])
        );
    }
}

proptest::proptest! {
    #[test]
    fn issue_2199_any_remainder_below_operation_charge_is_reported_as_tc_reserve(
        balance in 0..crate::params::DEAL_BURN_CLAIM_RAW,
    ) {
        let tc = issue_2115_address('c');
        let error = tc_reserve_receipt_error(aborted_submit_error(), &tc, "claimTokens", "abcd",
            Some(&issue_2199_reserve_receipt(balance)));
        let shown = format!("{error:#}");
        let expected_balance = format!("observed_reserve_raw={balance}");
        let expected_charge = format!("required_operation_charge_raw={}", crate::params::DEAL_BURN_CLAIM_RAW);
        proptest::prop_assert!(shown.contains("ECC[2]/SHELL reserve"));
        proptest::prop_assert!(shown.contains("claimTokens"));
        proptest::prop_assert!(shown.contains(&expected_balance));
        proptest::prop_assert!(shown.contains(&expected_charge));
        proptest::prop_assert!(shown.contains("transaction_hash=tx2199"));
        proptest::prop_assert!(shown.contains("settlement is unconfirmed"));
        proptest::prop_assert!(!shown.contains("nothing was sent"));
    }
}

#[test]
fn issue_2199_unproven_or_foreign_receipt_does_not_claim_tc_reserve_exhaustion() {
    let tc = issue_2115_address('c');
    let valid = issue_2199_reserve_receipt(0);
    let mut invalid = vec![None];
    for mutation in 0..9 {
        let mut receipt = valid.clone();
        match mutation {
            0 => receipt.message_hash = "foreign".into(),
            1 => receipt.transaction_hash = None,
            2 => receipt.aborted = Some(false),
            3 => receipt.action_success = Some(true),
            4 => receipt.result_code = Some(37),
            5 => receipt.no_funds = None,
            6 => receipt.no_funds = Some(false),
            7 => receipt.compute_exit_code = Some(101),
            _ => receipt.compute_exit_code = None,
        }
        invalid.push(Some(receipt));
    }
    for receipt in invalid {
        let error = tc_reserve_receipt_error(
            aborted_submit_error(),
            &tc,
            "claimTokens",
            "abcd",
            receipt.as_ref(),
        );
        assert!(!format!("{error:#}").contains("ECC[2]/SHELL reserve"));
    }
}

#[tokio::test]
async fn issue_2199_compute_rejection_is_not_reclassified_or_polled_as_reserve_exhaustion() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(
        listener,
        vec![
            (200, issue_2115_clock_reply()),
            (200, json!({"result":{"aborted":true,"exit_code":332}})),
        ],
    );
    let error = issue_2030_backend(&endpoint)
        .open_stream(
            &issue_2115_address('c'),
            &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            &[1],
        )
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(!format!("{error:#}").contains("ECC[2]/SHELL reserve"));
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn issue_2199_real_submit_resolves_exact_finalized_tc_rejection() {
    for balance in [0, crate::params::DEAL_BURN_CLAIM_RAW - 1] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let tc = issue_2115_address('c');
        let account = tc.clone();
        let server = tokio::spawn(async move {
            let mut hash = String::new();
            for step in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_fixture_http_request(&mut socket).await;
                let input: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                let reply = match step {
                    0 => issue_2115_clock_reply(),
                    1 => {
                        hash = external_message_hash(input[0]["body"].as_str().unwrap()).unwrap();
                        json!({"result":{"aborted":true}})
                    }
                    _ => {
                        assert_eq!(input["variables"]["hash"], hash);
                        json!({"data":{"blockchain":{
                            "message":{"id":hash,"dst":account.with_workchain(),
                                "dst_transaction":{"id":"tx2199","status":3,"aborted":true,
                                    "account_addr":account.with_workchain(),"compute":{"exit_code":0},
                                    "action":{"result_code":38,"success":false,"no_funds":true}}},
                            "account":{"info":{"id":account.bare(),"dapp_id":crate::address::DEXDO_DAPP_ID,
                                "balance_other":[{"currency":f64::from(crate::params::SHELL_CURRENCY_ID),"value":balance.to_string()}]}}
                        }}})
                    }
                };
                let body = reply.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let error = issue_2030_backend(&endpoint)
            .claim_tokens(
                &tc,
                &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
                crate::params::TICK_SIZE,
            )
            .await
            .unwrap_err();
        server.await.unwrap();
        let shown = format!("{error:#}");
        assert!(shown.contains("claimTokens"), "{shown}");
        assert!(shown.contains(tc.bare()), "{shown}");
        assert!(shown.contains("ECC[2]/SHELL reserve"), "{shown}");
        assert!(shown.contains("transaction_hash=tx2199"), "{shown}");
        assert!(
            shown.contains(&format!("observed_reserve_raw={balance}")),
            "{shown}"
        );
    }
}

#[test]
fn issue_2199_unavailable_receipt_retains_known_submission_identity() {
    let tc = issue_2115_address('c');
    let error = tc_reserve_receipt_error(aborted_submit_error(), &tc, "claimTokens", "abcd", None);
    let shown = format!("{error:#}");
    for fact in [
        tc.bare(),
        "claimTokens",
        "message_hash=abcd",
        "was rejected",
        "reserve cause unproven",
        "settlement is unconfirmed",
    ] {
        assert!(shown.contains(fact), "{shown}");
    }
}

fn issue_2199_internal_body(operation: &str) -> String {
    let contract = tvm_abi::Contract::load(TOKENCONTRACT_ABI.as_bytes()).unwrap();
    let function = contract.function(operation).unwrap();
    let args = match operation {
        "fundDeal" => json!({"amount":"2000000000", "endpointCipher":null}),
        "postFromNote" => {
            json!({"iobHash":"0x1", "iobDepth":1, "depositIdentifierHash":"0x1", "flags":0, "deadline":"1"})
        }
        _ => unreachable!(),
    };
    let tokens = tvm_abi::token::Tokenizer::tokenize_all_params(&function.inputs, &args).unwrap();
    let cell = function
        .encode_input(&Default::default(), &tokens, true, None, None)
        .unwrap()
        .into_cell()
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&cell).unwrap())
}

fn issue_2199_exact_receipt(balance: u128, hash: &str) -> Value {
    let tc = issue_2115_address('c');
    json!({"data":{"blockchain":{
        "message":{"id":hash,"dst":tc.with_workchain(),
            "dst_transaction":{"id":"tx2199","status":3,"aborted":true,
                "account_addr":tc.with_workchain(),"compute":{"exit_code":0},
                "action":{"result_code":38,"success":false,"no_funds":true}}},
        "account":{"info":{"id":tc.bare(),"dapp_id":crate::address::DEXDO_DAPP_ID,
            "balance_other":[{"currency":2.0,"value":balance.to_string()}]}}
    }}})
}

#[tokio::test]
async fn issue_2199_aborted_post_receipt_read_failures_keep_known_rejection_without_resubmit() {
    for mode in ["timeout", "unavailable", "foreign"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let tc = issue_2115_address('c');
        let server = tokio::spawn(async move {
            let reads = if mode == "unavailable" {
                crate::params::FINALIZED_DESTINATION_RECEIPT_MAX_ATTEMPTS
            } else {
                1
            };
            let mut hash = String::new();
            let mut posts = 0;
            for step in 0..2 + reads {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_fixture_http_request(&mut socket).await;
                let input: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                let reply = match step {
                    0 => issue_2115_clock_reply(),
                    1 => {
                        posts += 1;
                        hash = external_message_hash(input[0]["body"].as_str().unwrap()).unwrap();
                        json!({"result":{"aborted":true}})
                    },
                    _ => {
                        assert_eq!(input["variables"]["hash"], hash);
                        match mode {
                            "timeout" => json!({"errors":[{"message":"pool timed out while waiting for an open connection"}]}),
                            "foreign" => issue_2199_exact_receipt(0, "foreign"),
                            _ => json!({"data":{"blockchain":{"message":null}}}),
                        }
                    },
                }.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
            }
            assert_eq!(posts, 1);
            hash
        });
        let error = issue_2030_backend(&endpoint)
            .claim_tokens(
                &tc,
                &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
                TICK_SIZE,
            )
            .await
            .unwrap_err();
        let hash = server.await.unwrap();
        let shown = format!("{error:#}");
        for fact in [
            tc.bare(),
            "claimTokens",
            &hash,
            "was rejected",
            "reserve cause unproven",
            "settlement is unconfirmed",
        ] {
            assert!(shown.contains(fact), "{mode}: {shown}");
        }
        assert!(
            !shown.contains("failed because its ECC[2]/SHELL reserve"),
            "{shown}"
        );
    }
}

#[tokio::test]
async fn issue_2199_nominal_two_p_does_not_mask_internal_fund_deal_reserve_failure() {
    for balance in [0, crate::params::DEAL_BURN_FUND_DEAL_RAW - 1] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let tc = issue_2115_address('c');
        let note = issue_2115_address('a');
        let tc_info = issue_2115_account_fixture(
            &tc,
            TOKENCONTRACT_ABI,
            TOKENCONTRACT_TVC,
            json!({"_constructorFlag":true,"_pricePerTick":crate::params::SHELL_UNIT.to_string()}),
        );
        let note_info = issue_2115_account_fixture(
            &note,
            PRIVATENOTE_ABI,
            PRIVATENOTE_TVC,
            json!({"_constructorFlag":true,"_balance":{"2":(2 * crate::params::SHELL_UNIT).to_string()}}),
        );
        let server = tokio::spawn(async move {
            let last_poll = 4 + crate::params::SELLER_BOND_CONFIRM_MAX_READS;
            let mut hash = String::new();
            let mut posts = 0;
            for step in 0..last_poll + 7 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_fixture_http_request(&mut socket).await;
                let input: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                let reply = if step == 3 {
                    issue_2115_clock_reply()
                } else if step == 4 {
                    posts += 1;
                    hash = external_message_hash(input[0]["body"].as_str().unwrap()).unwrap();
                    json!({"result":{}})
                } else if step == last_poll + 4 {
                    assert_eq!(input["variables"]["hash"], hash);
                    let mut root = issue_2115_message('1', None, Some('a'), &['2']);
                    root["data"]["blockchain"]["message"]["id"] = json!(hash);
                    root
                } else if step == last_poll + 5 {
                    let mut child = issue_2115_message('2', Some('a'), Some('c'), &[]);
                    child["data"]["blockchain"]["message"]["body"] =
                        json!(issue_2199_internal_body("fundDeal"));
                    child
                } else if step == last_poll + 6 {
                    assert_eq!(input["variables"]["hash"], "2".repeat(64));
                    issue_2199_exact_receipt(balance, &"2".repeat(64))
                } else {
                    let info = if step == 2 || step == last_poll + 3 {
                        &note_info
                    } else {
                        &tc_info
                    };
                    json!({"data":{"blockchain":{"account":{"info":info}}}})
                }
                .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
            }
            assert_eq!(posts, 1, "no extra submission or reserve refill");
        });
        let error = super::super::backends::post_seller_bond_and_wait(
            &issue_2030_backend(&endpoint),
            &note,
            &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            1,
            &tc.with_workchain(),
            &tc,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("ECC[2]/SHELL reserve"),
            "{error}"
        );
        server.await.unwrap();
        let shown = error.to_string();
        for fact in [
            "fundDeal",
            "ECC[2]/SHELL reserve",
            "note_message_hash=",
            "transaction_hash=tx2199",
            "settlement is unconfirmed",
        ] {
            assert!(shown.contains(fact), "{shown}");
        }
        assert!(
            shown.contains(&format!("observed_reserve_raw={balance}")),
            "{shown}"
        );
        assert!(
            shown.contains(&format!(
                "required_operation_charge_raw={}",
                crate::params::DEAL_BURN_FUND_DEAL_RAW
            )),
            "{shown}"
        );
        assert!(!shown.contains("Re-mint"), "{shown}");
    }
}

#[tokio::test]
async fn issue_2199_internal_receipt_requires_note_parent_source_target_and_operation() {
    for mutation in [
        "none",
        "timeout",
        "unavailable",
        "parent",
        "source",
        "destination",
        "operation",
        "receipt",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut root = issue_2115_message('1', None, Some('a'), &['2']);
        let mut child = issue_2115_message('2', Some('a'), Some('c'), &[]);
        child["data"]["blockchain"]["message"]["body"] =
            json!(issue_2199_internal_body("fundDeal"));
        match mutation {
            "timeout" => {
                root = json!({"errors":[{"message":"pool timed out while waiting for an open connection"}]})
            }
            "unavailable" => root = json!({"data":{"blockchain":{"message":null}}}),
            "parent" => {
                root["data"]["blockchain"]["message"]["dst_transaction"]["status"] = json!(1)
            }
            "source" => {
                child["data"]["blockchain"]["message"]["src"] =
                    json!(issue_2115_address('f').with_workchain())
            }
            "destination" => {
                child["data"]["blockchain"]["message"]["dst"] =
                    json!(issue_2115_address('f').with_workchain())
            }
            "operation" => {
                child["data"]["blockchain"]["message"]["body"] =
                    json!(issue_2199_internal_body("postFromNote"))
            }
            _ => {}
        }
        let mut replies = vec![(200, root)];
        if !matches!(mutation, "parent" | "timeout" | "unavailable") {
            replies.push((200, child));
        }
        if mutation == "none" || mutation == "receipt" {
            let mut receipt = issue_2199_exact_receipt(0, &"2".repeat(64));
            if mutation == "receipt" {
                receipt["data"]["blockchain"]["message"]["dst"] =
                    json!(issue_2115_address('f').with_workchain());
            }
            replies.push((200, receipt));
        }
        let (requests, server) = issue_2030_producer_server(listener, replies);
        let error = issue_2030_backend(&endpoint)
            .diagnose_note_tc_failure(
                &issue_2115_address('a'),
                &issue_2115_address('c'),
                "fundDeal",
                &"1".repeat(64),
                anyhow!("bond not funded"),
            )
            .await;
        server.await.unwrap();
        let shown = format!("{error:#}");
        assert_eq!(
            shown.contains("failed because its ECC[2]/SHELL reserve"),
            mutation == "none",
            "{shown}"
        );
        assert_eq!(
            shown.contains("reserve cause unproven"),
            mutation != "none",
            "{shown}"
        );
        assert!(shown.contains("fundDeal was submitted"), "{shown}");
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !request.is_array())
        );
    }
}

#[tokio::test]
async fn issue_2199_offer_reserve_rejection_keeps_journal_and_does_not_resubmit_on_resume() {
    for balance in [0, crate::params::DEAL_BURN_POST_FROM_NOTE_RAW - 1] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let root = issue_2115_message('1', None, Some('a'), &['2']);
        let mut child = issue_2115_message('2', Some('a'), Some('c'), &[]);
        let node = &mut child["data"]["blockchain"]["message"];
        node["body"] = json!(issue_2199_internal_body("postFromNote"));
        node["dst_transaction"]["aborted"] = json!(true);
        node["dst_transaction"]["action"] = json!({"success":false,"result_code":38});
        let (requests, server) = issue_2030_producer_server(
            listener,
            vec![
                (200, root.clone()),
                (200, child.clone()),
                (200, root),
                (200, child),
                (200, issue_2199_exact_receipt(balance, &"2".repeat(64))),
            ],
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pending.sell.json");
        let mut post = issue_2115_post();
        let chain = issue_2030_backend(&endpoint);
        let error = chain
            .reconcile_seller_placement(&mut post, Some(&path))
            .await
            .unwrap_err();
        let shown = format!("{error:#}");
        for fact in [
            "postFromNote",
            "ECC[2]/SHELL reserve",
            "transaction_hash=tx2199",
            "settlement is unconfirmed",
        ] {
            assert!(shown.contains(fact), "{shown}");
        }
        assert!(
            shown.contains(&format!("observed_reserve_raw={balance}")),
            "{shown}"
        );
        let mut restored = SellerPlacement::load(&path).unwrap().unwrap();
        assert_eq!(restored.message_id, post.message_id);
        let resumed = chain
            .reconcile_seller_placement(&mut restored, Some(&path))
            .await
            .unwrap_err();
        assert_eq!(format!("{resumed:#}"), shown);
        server.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 5);
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !request.is_array())
        );
    }
}

#[tokio::test]
async fn issue_2199_two_p_minus_one_still_refuses_before_any_money_post() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let tc = issue_2115_address('c');
    let note = issue_2115_address('a');
    let tc_info = issue_2115_account_fixture(
        &tc,
        TOKENCONTRACT_ABI,
        TOKENCONTRACT_TVC,
        json!({"_constructorFlag":true,"_pricePerTick":crate::params::SHELL_UNIT.to_string()}),
    );
    let note_info = issue_2115_account_fixture(
        &note,
        PRIVATENOTE_ABI,
        PRIVATENOTE_TVC,
        json!({"_constructorFlag":true,"_balance":{"2":(2 * crate::params::SHELL_UNIT - 1).to_string()}}),
    );
    let (requests, server) = issue_2030_producer_server(
        listener,
        vec![
            (
                200,
                json!({"data":{"blockchain":{"account":{"info":tc_info}}}}),
            ),
            (
                200,
                json!({"data":{"blockchain":{"account":{"info":tc_info}}}}),
            ),
            (
                200,
                json!({"data":{"blockchain":{"account":{"info":note_info}}}}),
            ),
        ],
    );
    let error = super::super::backends::post_seller_bond_and_wait(
        &issue_2030_backend(&endpoint),
        &note,
        &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        1,
        &tc.with_workchain(),
        &tc,
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    let shown = error.to_string();
    assert!(shown.contains("below required seller bond 2P"), "{shown}");
    assert!(shown.contains("before money moves"), "{shown}");
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.is_array())
    );
}

async fn issue_2199_check_sell_diagnostic_deadline(stalled_read: usize) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let root = issue_2115_message('1', None, Some('a'), &['2']);
    let mut child = issue_2115_message('2', Some('a'), Some('c'), &[]);
    let node = &mut child["data"]["blockchain"]["message"];
    node["body"] = json!(issue_2199_internal_body("postFromNote"));
    node["dst_transaction"]["aborted"] = json!(true);
    node["dst_transaction"]["action"] = json!({"success":false,"result_code":38});
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let server = tokio::spawn(async move {
        let replies = [root.clone(), child.clone(), root, child];
        for reply in replies.iter().take(stalled_read) {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let input: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert!(!input.is_array(), "no money POST or refill");
            captured.lock().unwrap().push(input);
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
        let (mut stalled_socket, _) = listener.accept().await.unwrap();
        let request = read_fixture_http_request(&mut stalled_socket).await;
        let input: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            input["variables"]["hash"],
            if stalled_read == 2 {
                "1".repeat(64)
            } else {
                "2".repeat(64)
            }
        );
        captured.lock().unwrap().push(input);
        // Hold every retry too, so the actual outer SELL deadline cancels enrichment.
        let mut stalled_sockets = vec![stalled_socket];
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_fixture_http_request(&mut socket).await;
            let input: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            captured.lock().unwrap().push(input);
            stalled_sockets.push(socket);
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let tc = issue_2115_address('c');
    let book = issue_2115_address('b');
    let path = directory.path().join(format!("{}.sell.json", tc.bare()));
    let chain = issue_2030_backend(&endpoint);
    let mut post = issue_2115_post();
    post.network = chain.network().to_string();
    post.save(Some(&path)).unwrap();
    let seller = super::super::backends::RealSellerBackend::new(
        chain,
        issue_2115_address('a'),
        KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        "model-hash".into(),
        "model".into(),
        1,
        TICK_SIZE,
    )
    .with_offer_journal_dir(directory.path().to_path_buf());
    let error = seller
        .reconcile_pending_offer(&tc, &book)
        .await
        .unwrap_err();
    let shown = error.to_string();
    assert!(
        matches!(error, crate::ChainError::AmbiguousSubmit(_)),
        "{shown}"
    );
    for fact in [
        tc.bare(),
        "postFromNote",
        "was rejected",
        "execution read timed out",
        "message_hash=",
        &"1".repeat(64),
        &"2".repeat(64),
        "transaction-2",
        "reserve cause unproven",
        "unconfirmed",
    ] {
        assert!(shown.contains(fact), "{shown}");
    }
    assert!(
        !shown.contains("failed because its ECC[2]/SHELL reserve"),
        "{shown}"
    );
    assert!(!shown.contains("nothing was sent"), "{shown}");
    assert!(!shown.contains("Re-mint"), "{shown}");
    let saved = SellerPlacement::load(&path).unwrap().unwrap();
    assert_eq!(saved.message_id, post.message_id);
    assert!(
        saved.terminal.is_none(),
        "diagnostic cancellation must not claim terminal settlement"
    );
    let checkpoint: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let rejection = checkpoint["rejection"].as_str().unwrap();
    for fact in [
        "postFromNote",
        "was rejected",
        "transaction-2",
        "reserve cause unproven",
    ] {
        assert!(rejection.contains(fact), "{rejection}");
    }
    let observed_reads = requests.lock().unwrap().len();
    assert!(observed_reads > stalled_read);
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.is_array())
    );
    drop(seller);
    let restarted = super::super::backends::RealSellerBackend::new(
        issue_2030_backend(&endpoint),
        issue_2115_address('a'),
        KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        "model-hash".into(),
        "model".into(),
        1,
        TICK_SIZE,
    )
    .with_offer_journal_dir(directory.path().to_path_buf());
    let resumed = restarted
        .reconcile_pending_offer(&tc, &book)
        .await
        .unwrap_err()
        .to_string();
    assert!(resumed.contains(rejection), "{resumed}");
    assert_eq!(
        SellerPlacement::load(&path).unwrap().unwrap().message_id,
        post.message_id
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        observed_reads,
        "no read, POST or refill on restart"
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn issue_2199_sell_diagnostic_deadline_during_note_reread() {
    issue_2199_check_sell_diagnostic_deadline(2).await;
}

#[tokio::test]
async fn issue_2199_sell_diagnostic_deadline_during_child_reread() {
    issue_2199_check_sell_diagnostic_deadline(3).await;
}

#[tokio::test]
async fn issue_2199_sell_diagnostic_deadline_during_detailed_receipt() {
    issue_2199_check_sell_diagnostic_deadline(4).await;
}
