#[test]
fn issue_2220_funded_tc_does_not_substitute_for_the_buyers_gas_pocket() {
    let note = issue_2115_address('a');
    let charge = crate::params::DEAL_BURN_TERMINAL_RAW;
    for call in ["streamStop", "streamDispute", "streamCleanup"] {
        let refusal = buyer_note_charge_refusal(call, &note, charge - 1, charge);
        assert!(refusal.is_some(), "{call} must be paid by the buyer's note");
        let refusal = refusal.unwrap();
        assert!(refusal.contains("dexdo note topup"));
        assert!(refusal.contains("Nothing was sent"));
        assert!(!refusal.contains("SELLER"));
        assert!(buyer_note_charge_refusal(call, &note, charge, charge).is_none());
    }
}

proptest::proptest! {
    #[test]
    fn issue_2220_note_payment_boundary_is_exact(note_ecc in proptest::num::u128::ANY) {
        let charge = crate::params::DEAL_BURN_TERMINAL_RAW;
        let note = issue_2115_address('a');
        proptest::prop_assert_eq!(
            buyer_note_charge_refusal("streamStop", &note, note_ecc, charge).is_none(),
            note_ecc >= charge,
        );
    }

    #[test]
    fn issue_2220_buy_budget_covers_each_possible_bond_message(ticks in 2u128..u64::MAX as u128) {
        use crate::params::*;
        let gas = buyer_order_gas_raw(ticks, false).unwrap();
        proptest::prop_assert_eq!(gas - BUY_ORDER_GAS_RAW,
            (ticks / MIN_STREAM_BUY_TICKS) * DEAL_BURN_FUND_DEAL_RAW);
        proptest::prop_assert_eq!(buyer_order_gas_raw(ticks, true),
            Some(BUY_ORDER_GAS_RAW + DEAL_BURN_FUND_DEAL_RAW));
    }
}

#[test]
fn issue_2220_placement_alone_does_not_pay_the_buyer_bond() {
    use crate::params::*;
    let escrow = 10 * SHELL_UNIT;
    assert!(buy_order_shortfall(escrow, BUY_ORDER_GAS_RAW, escrow, 2, false).is_some());
    let required = BUY_ORDER_GAS_RAW + DEAL_BURN_FUND_DEAL_RAW;
    assert!(buy_order_shortfall(escrow, required - 1, escrow, 2, false).is_some());
    assert!(buy_order_shortfall(escrow, required, escrow, 2, false).is_none());
    assert!(buy_order_shortfall(escrow - 1, required, escrow, 2, false)
        .unwrap()
        .contains("escrows"));
    assert!(buyer_order_gas_raw(u128::MAX, false).is_none());
    assert_eq!(buyer_order_gas_raw(u128::MAX, true), Some(required));
}

fn issue_2220_note_reply(note: &Address, ecc: u128) -> Value {
    let mut info = issue_2115_account_fixture(
        note,
        PRIVATENOTE_ABI,
        PRIVATENOTE_TVC,
        json!({"_constructorFlag":true,"_balance":{"2":(100 * crate::params::SHELL_UNIT).to_string()}}),
    );
    info["balance_other"] =
        json!([{"currency":crate::params::SHELL_CURRENCY_ID,"value":format!("0x{ecc:x}")}]);
    json!({"data":{"blockchain":{"account":{"info":info}}}})
}

#[tokio::test]
async fn issue_2220_all_terminal_entry_points_refuse_an_empty_note_without_submission() {
    for call in [
        "streamStop",
        "streamDispute",
        "streamCleanup",
        "automaticStop",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let note = issue_2115_address('a');
        let tc = issue_2115_address('c');
        let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
        let (requests, server) =
            issue_2030_producer_server(listener, vec![(200, issue_2220_note_reply(&note, 0))]);
        let chain = issue_2030_backend(&endpoint);
        let result = match call {
            "streamStop" => chain.stream_stop(&note, &keys, &tc).await.map(|_| ()),
            "streamDispute" => chain.stream_dispute(&note, &keys, &tc).await.map(|_| ()),
            "streamCleanup" => chain.stream_cleanup(&note, &keys, &tc).await.map(|_| ()),
            _ => chain
                .stop_if_heartbeat(&note, &keys, &tc, &mut || true)
                .await
                .map(|_| ()),
        };
        assert!(
            format!("{:#}", result.unwrap_err()).contains("Nothing was sent"),
            "{call}"
        );
        server.await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]["query"].as_str().unwrap().contains(note.bare()));
    }
}

#[tokio::test]
async fn issue_2220_missing_note_does_not_authorize_a_terminal_call() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, server) = issue_2030_producer_server(
        listener,
        vec![(
            200,
            json!({"data":{"blockchain":{"account":{"info":null}}}}),
        )],
    );
    let result = issue_2030_backend(&endpoint)
        .require_buyer_note_charge(
            &issue_2115_address('a'),
            "streamStop",
            crate::params::DEAL_BURN_TERMINAL_RAW,
        )
        .await;
    assert!(result.is_err());
    server.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn issue_2220_all_buy_entry_points_check_bond_gas_before_journaling_or_submission() {
    for entry in 0..3 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let note = issue_2115_address('a');
        let book = issue_2115_address('b');
        let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
        let reply = issue_2220_note_reply(&note, crate::params::BUY_ORDER_GAS_RAW);
        let (requests, server) =
            issue_2030_producer_server(listener, vec![(200, reply.clone()), (200, reply)]);
        let chain = issue_2030_backend(&endpoint);
        let mut cursor = MatchWatchCursor::new(0);
        let result = match entry {
            0 => {
                chain
                    .place_inference_buy(
                        &note,
                        &keys,
                        &"0".repeat(64),
                        crate::params::SHELL_UNIT,
                        2,
                        10 * crate::params::SHELL_UNIT,
                        0,
                        0,
                    )
                    .await
            }
            1 => {
                chain
                    .place_inference_buy_with_submit_identity(
                        &note,
                        &keys,
                        &book,
                        &"0".repeat(64),
                        crate::params::SHELL_UNIT,
                        2,
                        10 * crate::params::SHELL_UNIT,
                        0,
                        0,
                        &mut cursor,
                        &mut |_, _, _| panic!("no journal before gas preflight"),
                    )
                    .await
            }
            _ => {
                chain
                    .place_inference_buy_with_identity_and_cursors(
                        &note,
                        &keys,
                        &book,
                        &"0".repeat(64),
                        crate::params::SHELL_UNIT,
                        2,
                        10 * crate::params::SHELL_UNIT,
                        0,
                        0,
                        &mut cursor,
                        &mut |_, _, _, _| panic!("no journal before gas preflight"),
                    )
                    .await
            }
        };
        assert!(format!("{:#}", result.unwrap_err()).contains("fundBuyerBond"));
        server.await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| !request.is_array()));
    }
}

#[tokio::test]
async fn issue_2220_funded_cleanup_is_sent_through_the_buyer_note() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let note = issue_2115_address('a');
    let (requests, server) = issue_2030_producer_server(
        listener,
        vec![
            (
                200,
                issue_2220_note_reply(&note, crate::params::DEAL_BURN_TERMINAL_RAW),
            ),
            (200, issue_2115_clock_reply()),
            (200, json!({"result":{}})),
        ],
    );
    issue_2030_backend(&endpoint)
        .stream_cleanup(
            &note,
            &KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
            &issue_2115_address('c'),
        )
        .await
        .unwrap();
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        dest_account_id_hex(requests[2][0]["body"].as_str().unwrap()).unwrap(),
        note.bare()
    );
}

#[test]
fn issue_2220_current_abi_limits_are_explicit() {
    for method in [
        "fundBuyerBondNow",
        "streamStop",
        "streamDispute",
        "streamCleanup",
    ] {
        assert!(compiled_abi_inputs(PRIVATENOTE_ABI, method).is_some());
    }
    for method in ["finalize", "settleWeek", "resolveDisputeTimeout"] {
        assert!(compiled_abi_inputs(TOKENCONTRACT_ABI, method).is_some());
        assert!(compiled_abi_inputs(PRIVATENOTE_ABI, method).is_none());
    }
}
