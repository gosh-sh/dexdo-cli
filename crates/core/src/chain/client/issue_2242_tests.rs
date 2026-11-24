fn issue_2242_account_reply(info: Value) -> Value {
    json!({"data":{"blockchain":{"account":{"info":info}}}})
}

#[tokio::test]
async fn issue_2242_buy_cannot_mix_balance_and_bond_across_the_funding_transition() {
    use crate::params::{matched_buy_reserve_raw, DEAL_BURN_FUND_DEAL_RAW};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let tc = issue_2115_address('c');
    let price = crate::params::PRICE_STEP;
    let ticks = 2;
    let funded_threshold = matched_buy_reserve_raw(ticks, true, false);
    let reply = |have, funded| {
        let mut info = issue_2115_account_fixture(
            &tc,
            TOKENCONTRACT_ABI,
            TOKENCONTRACT_TVC,
            json!({"_constructorFlag":true,"_pricePerTick":price.to_string(),"_maxTicks":"100",
                "_sellerBondFunded":funded,"_sellerBond":if funded { (2 * price).to_string() } else { "0".to_string() }}),
        );
        info["balance_other"] =
            json!([{"currency":crate::params::SHELL_CURRENCY_ID,"value":format!("0x{have:x}")}]);
        (200, issue_2242_account_reply(info))
    };
    let before = reply(funded_threshold, false);
    let after = reply(funded_threshold - DEAL_BURN_FUND_DEAL_RAW, true);
    // Each preflight reads getState, then one reserve snapshot. The old preflight
    // read the balance from `before` and kept fetching getters into `after`, where
    // the lower funded threshold would incorrectly accept the earlier balance.
    let (requests, server) =
        issue_2030_producer_server(listener, vec![before.clone(), before, after.clone(), after]);
    let ask = crate::market::OrderBookOrder {
        order_id: 103,
        owner_note: issue_2115_address('a').with_workchain(),
        token_contract: Some(tc.with_workchain()),
        is_buy: false,
        price_per_tick: price,
        ticks: 100,
        escrow: 0,
        deadline: local_unix_secs().unwrap() + crate::params::MAX_SELL_TTL.as_secs(),
        flags: 0,
        timestamp: 1,
    };
    let chain = issue_2030_backend(&endpoint);
    for (have, required) in [
        (funded_threshold, funded_threshold + DEAL_BURN_FUND_DEAL_RAW),
        (funded_threshold - DEAL_BURN_FUND_DEAL_RAW, funded_threshold),
    ] {
        let error = chain
            .check_resting_buy_cross(std::slice::from_ref(&ask), ticks, price, false)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&format!(
            "reserve {have} raw ECC[2] is below {required} raw ECC[2] required for the 2-tick fill"
        )),
            "{message}"
        );
    }
    server.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn issue_2242_subscription_reserve_is_preparation_before_journal_or_post() {
    use crate::market::flags;
    let ticks = u128::from(SUBSCRIPTION_WEEKS);
    let flags = flags::SUBSCRIPTION | flags::AON;
    let reserve = crate::params::matched_buy_reserve_raw(ticks, false, true);
    let price = crate::params::PRICE_STEP;
    let escrow = crate::market::subscription_buy_reserve(ticks, price)
        .unwrap()
        .total_escrow;
    for (have, cross) in [(reserve - 1, true), (reserve, true), (0, false)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let note = issue_2115_address('a');
        let book = issue_2115_address('b');
        let tc = issue_2115_address('c');
        let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
        let model = model_hash_for("fixture--model--v1");
        let deadline = local_unix_secs().unwrap() + crate::params::MAX_SELL_TTL.as_secs();
        let orders = if cross {
            json!({"103":{
                "note":note.with_workchain(),"buyerPubkey":"0","tokenContract":tc.with_workchain(),
                "price":price.to_string(),"amount":ticks.to_string(),"initialAmount":ticks.to_string(),
                "filledAccum":"0","escrow":"0","deadline":deadline.to_string(),
                "clientOrderId":"0","ts":"1","flags":flags.to_string(),"isBuy":false,
                "nextAtPrice":"0","prevAtPrice":"0","nextInOwner":"0","prevInOwner":"0"
            }})
        } else {
            json!({})
        };
        let book_reply = issue_2242_account_reply(issue_2115_account_fixture(
            &book,
            INFERENCE_ORDERBOOK_ABI,
            INFERENCE_ORDERBOOK_TVC,
            json!({"_constructorFlag":true,"_modelHash":model,"_modelName":"fixture--model--v1",
                "_orders":orders,"_orderCount":u8::from(cross).to_string(),"_nextOrderId":"104"}),
        ));
        let mut tc_info = issue_2115_account_fixture(
            &tc,
            TOKENCONTRACT_ABI,
            TOKENCONTRACT_TVC,
            json!({"_constructorFlag":true,"_modelName":"fixture--model--v1","_modelHash":model,
                "_pricePerTick":price.to_string(),"_maxTicks":ticks.to_string()}),
        );
        tc_info["balance_other"] =
            json!([{"currency":crate::params::SHELL_CURRENCY_ID,"value":format!("0x{have:x}")}]);
        let note_reply = issue_2220_note_reply(
            &note,
            crate::params::buyer_order_gas_raw(ticks, true).unwrap(),
        );
        let mut replies = vec![
            (200, note_reply.clone()),
            (200, note_reply),
            (200, book_reply.clone()),
            (200, book_reply),
        ];
        if cross {
            replies.extend((0..2).map(|_| (200, issue_2242_account_reply(tc_info.clone()))));
        }
        let allowed = !cross || have == reserve;
        if allowed {
            replies.extend([
                (200,json!({"dapp_id":crate::address::DEXDO_DAPP_ID})),
                (200,json!({"data":{"blockchain":{"account":{"info":{},
                    "messages":{"edges":[],"pageInfo":{"startCursor":null,"hasPreviousPage":false}}}}}})),
                (200,issue_2115_clock_reply()),
                (200,json!({"result":{}})),
            ]);
        }
        let (requests, server) = issue_2030_producer_server(listener, replies);
        let mut journaled = false;
        let result = issue_2030_backend(&endpoint)
            .place_inference_buy_with_identity_and_cursors(
                &note,
                &keys,
                &book,
                &model,
                price,
                ticks,
                escrow,
                flags,
                deadline,
                &mut MatchWatchCursor::new(0),
                &mut |_, _, _, _| {
                    journaled = true;
                    Ok(())
                },
            )
            .await;
        if allowed {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert!(matches!(
                error.downcast_ref::<MoneySubmitError>(),
                Some(MoneySubmitError::Preparation { .. })
            ));
            let message = format!("{error:#}");
            for fact in [
                "order ".to_string(),
                display_token_contract(&tc),
                format!("{ticks}-tick fill"),
                format!("{have} raw ECC[2]"),
                format!("{reserve} raw ECC[2]"),
            ] {
                assert!(message.contains(&fact), "{fact}: {message}");
            }
        }
        server.await.unwrap();
        assert_eq!(journaled, allowed);
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|body| body.is_array())
                .count(),
            usize::from(allowed)
        );
    }
}
