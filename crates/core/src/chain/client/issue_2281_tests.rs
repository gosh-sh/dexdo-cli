fn issue_2281_seller(
    endpoint: &str,
    directory: &Path,
) -> super::super::backends::RealSellerBackend {
    super::super::backends::RealSellerBackend::new(
        issue_2115_seller_chain(endpoint),
        issue_2115_address('a'),
        KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap(),
        model_hash_for("fixture--model--v1"),
        "fixture--model--v1".into(),
        1,
        TICK_SIZE,
    )
    .with_offer_journal_dir(directory.to_owned())
}

fn issue_2281_offer() -> crate::market::SellOffer {
    crate::market::SellOffer {
        token_contract: issue_2115_address('c').with_workchain(),
        price_per_tick: PRICE_STEP.try_into().unwrap(),
        max_ticks: 100,
        flags: 0,
    }
}

#[tokio::test]
async fn issue_2281_quiet_note_recovers_once_after_lost_anchor_ack_and_restart() {
    issue_2281_recovery(false).await;
}

#[tokio::test]
async fn issue_2281_delayed_original_history_never_authorizes_a_duplicate() {
    issue_2281_recovery(true).await;
}

async fn issue_2281_recovery(original_executed: bool) {
    use tvm_block::Serializable;
    let (_initial, state, server, directory, book, endpoint) = issue_2115_resting_backend().await;
    let path = directory
        .path()
        .join(format!("{}.sell.json", issue_2115_address('c').bare()));
    let window = GENERATION_PINS[0].private_note_expiry_window_secs;
    let expiry = u32::try_from(local_unix_secs().unwrap()).unwrap() - 1;
    // The original exact message is deliberately withheld from the message index.
    let original_boc = base64::engine::general_purpose::STANDARD
        .encode(tvm_types::write_boc(&tvm_block::Message::default().serialize().unwrap()).unwrap());
    let original = SellerPlacement::new(
        issue_2115_seller_chain(&endpoint).network(),
        &issue_2115_address('a'),
        &issue_2115_address('c'),
        &book,
        external_message_hash(&original_boc).unwrap(),
        10,
        Some(expiry),
    );
    original.save(Some(&path)).unwrap();
    let anchor = issue_2264_history_tx(10, expiry - window, None, None);
    {
        let mut state = state.lock().unwrap();
        state.order = None;
        state.rejected_hashes.push(original.message_id.clone());
        state.note_history = vec![anchor.clone()];
        state.expiry_anchor_unknown_ack = true;
    }
    let note = crate::note::LocalNote::generate();
    let seller = issue_2281_seller(&endpoint, directory.path());
    let error = seller
        .post_offer(issue_2281_offer(), &note)
        .await
        .unwrap_err();
    let saved = SellerPlacement::load(&path).unwrap().unwrap();
    assert_eq!(saved.message_id, original.message_id);
    assert!(saved.terminal.is_none());
    let saved_json = serde_json::to_value(&saved).unwrap();
    assert!(
        saved_json["expiry_anchor"][0].as_str().is_some(),
        "{error:#}"
    );
    let marker_boc = state.lock().unwrap().expiry_anchors[0].clone();
    let marker_hash = external_message_hash(&marker_boc).unwrap();
    assert_eq!(saved_json["expiry_anchor"][0], marker_hash);
    let decoded = tvm_client::abi::decode_message(
        local_context().unwrap(),
        tvm_client::abi::ParamsOfDecodeMessage {
            abi: tvm_client::abi::Abi::Json(PRIVATENOTE_ABI.into()),
            message: marker_boc.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(decoded.header.unwrap().expire.unwrap() >= expiry + window);
    drop(seller);
    let resumed = issue_2281_seller(&endpoint, directory.path());
    // Restart and repeated reads retain the exact uncertain marker, without another POST.
    assert!(resumed.post_offer(issue_2281_offer(), &note).await.is_err());
    assert_eq!(state.lock().unwrap().expiry_anchors.len(), 1);
    assert!(state.lock().unwrap().posts.is_empty());

    let old = if original_executed {
        Some(issue_2264_history_tx(
            11,
            expiry - 1,
            Some((&anchor, 10)),
            Some(&original_boc),
        ))
    } else {
        None
    };
    let predecessor = old.as_ref().unwrap_or(&anchor);
    let predecessor_lt = if old.is_some() { 11 } else { 10 };
    let marker = issue_2264_history_tx(
        12,
        expiry + 1,
        Some((predecessor, predecessor_lt)),
        Some(&marker_boc),
    );
    // A finalized marker without its predecessor is not evidence of missing execution.
    state.lock().unwrap().note_history = vec![marker.clone()];
    assert!(resumed.post_offer(issue_2281_offer(), &note).await.is_err());
    assert!(state.lock().unwrap().posts.is_empty());
    assert_eq!(state.lock().unwrap().expiry_anchors.len(), 1);
    {
        let mut state = state.lock().unwrap();
        state.note_history = vec![anchor];
        if let Some(old) = old {
            state.note_history.push(old);
        }
        state.note_history.push(marker);
    }
    if original_executed {
        assert!(resumed.post_offer(issue_2281_offer(), &note).await.is_err());
        assert!(state.lock().unwrap().posts.is_empty());
        let saved = SellerPlacement::load(&path).unwrap().unwrap();
        assert!(saved.terminal.is_none());
        // The message index catches up: adopt the original result, no successor.
        let mut state = state.lock().unwrap();
        state.posts.push(saved.message_id);
        state.order = Some(103);
    }
    resumed.post_offer(issue_2281_offer(), &note).await.unwrap();
    assert_eq!(state.lock().unwrap().posts.len(), 1);
    assert_eq!(state.lock().unwrap().expiry_anchors.len(), 1);
    resumed.post_offer(issue_2281_offer(), &note).await.unwrap();
    assert_eq!(state.lock().unwrap().posts.len(), 1);
    server.abort();
}

#[tokio::test]
async fn issue_2281_history_read_failure_and_unexpired_sell_send_no_anchor() {
    let (_seller, state, server, directory, book, endpoint) = issue_2115_resting_backend().await;
    let chain = issue_2115_seller_chain(&endpoint);
    let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
    let mut post = issue_2115_post();
    post.order_book = book.with_workchain();
    let now = u32::try_from(local_unix_secs().unwrap()).unwrap();
    let path = directory.path().join("anchor-negative.sell.json");
    state.lock().unwrap().pruned = true;
    for expiry in [
        now + GENERATION_PINS[0].private_note_expiry_window_secs,
        now - 1,
    ] {
        post.expire_at_unix = Some(expiry);
        state.lock().unwrap().note_history_error = true;
        let _ = chain
            .ensure_seller_expiry_anchor(&mut post, Some(&path), &keys)
            .await;
        assert!(state.lock().unwrap().expiry_anchors.is_empty());
    }
    server.abort();
}

#[tokio::test]
async fn issue_2281_anchor_is_persisted_before_send_and_only_expired_missing_anchor_is_replaced() {
    let (_seller, state, server, directory, _book, endpoint) = issue_2115_resting_backend().await;
    let chain = issue_2115_seller_chain(&endpoint);
    let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
    let now = u32::try_from(local_unix_secs().unwrap()).unwrap();
    let mut post = issue_2115_post();
    post.expire_at_unix = Some(now - 1);
    {
        let mut state = state.lock().unwrap();
        state.pruned = true;
        state.note_history = vec![issue_2264_history_tx(
            10,
            now - GENERATION_PINS[0].private_note_expiry_window_secs,
            None,
            None,
        )];
    }
    let path = directory.path().join("blocked.sell.json");
    std::fs::create_dir(&path).unwrap();
    assert!(chain
        .ensure_seller_expiry_anchor(&mut post, Some(&path), &keys)
        .await
        .is_err());
    assert!(state.lock().unwrap().expiry_anchors.is_empty());
    assert!(serde_json::to_value(&post)
        .unwrap()
        .get("expiry_anchor")
        .is_none());

    let hash = "6".repeat(64);
    let mut value = serde_json::to_value(&post).unwrap();
    value["expiry_anchor"] = json!([hash, now - 1]);
    post = serde_json::from_value(value).unwrap();
    // An indexed anchor may still be waiting for account-history indexing. Do not send another.
    state.lock().unwrap().posts.push(hash.clone());
    chain
        .ensure_seller_expiry_anchor(&mut post, None, &keys)
        .await
        .unwrap();
    assert!(state.lock().unwrap().expiry_anchors.is_empty());
    {
        let mut state = state.lock().unwrap();
        state.posts.clear();
        state.rejected_hashes.push(hash);
    }
    let path = directory.path().join("retry.sell.json");
    chain
        .ensure_seller_expiry_anchor(&mut post, Some(&path), &keys)
        .await
        .unwrap();
    chain
        .ensure_seller_expiry_anchor(&mut post, Some(&path), &keys)
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().expiry_anchors.len(), 1);
    assert_eq!(
        serde_json::to_value(SellerPlacement::load(&path).unwrap().unwrap()).unwrap(),
        serde_json::to_value(post).unwrap()
    );
    server.abort();
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]
    #[test]
    fn issue_2281_compiled_zero_credit_preserves_active_note_and_replays_safely(
        owner in proptest::array::uniform32(proptest::num::u8::ANY),
        balance in proptest::num::u128::ANY, debt in proptest::num::u128::ANY,
        withdrawn in proptest::bool::ANY,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(issue_2281_compiled_zero_credit(owner, balance, debt, withdrawn));
    }
}

async fn issue_2281_compiled_zero_credit(
    owner: [u8; 32],
    balance: u128,
    debt: u128,
    withdrawn: bool,
) {
    use tvm_client::tvm::{AccountForExecutor, ExecutionOptions, ParamsOfRunExecutor};
    let keys = KeyPair::from_secret_hex(&"3a".repeat(32)).unwrap();
    let owner = owner
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let note = issue_2115_address('a');
    let context = local_context().unwrap();
    let now = u32::try_from(local_unix_secs().unwrap()).unwrap();
    let window = GENERATION_PINS[0].private_note_expiry_window_secs;
    let marker = RealChainBackend::seller_expiry_anchor_boc(&note, &keys, now + window)
        .await
        .unwrap();
    let info = issue_2115_account_fixture(
        &note,
        PRIVATENOTE_ABI,
        PRIVATENOTE_TVC,
        json!({
            "_constructorFlag":true,
            "_ephemeralPubkey":format!("0x{}",KeyPair::from_secret_hex(&owner).unwrap().public_hex()),
            "_hasWithdrawn":withdrawn, "_balance":{"2":balance.to_string()},
            "messages":{(now + window - 1).to_string():{"1":true}},
            "_liveDeals":{issue_2115_address('b').with_workchain():true, issue_2115_address('c').with_workchain():true, issue_2115_address('d').with_workchain():true},
            "_restingInf":{"42":true}, "_pendingInf":{"17":"123"}, "_debt":debt.to_string()
        }),
    );
    let mut account = info["boc"].as_str().unwrap().to_owned();
    let before = account_storage_fields(&account, PRIVATENOTE_ABI, "PrivateNote").unwrap();
    let early = tvm_client::tvm::run_executor(
        context.clone(),
        ParamsOfRunExecutor {
            message: marker.clone(),
            account: AccountForExecutor::Account {
                boc: account.clone(),
                unlimited_balance: Some(true),
            },
            execution_options: Some(ExecutionOptions {
                block_time: Some(now),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        early.data()["exit_code"],
        401,
        "the marker must not execute before the SELL expiry"
    );
    for tick in 1..=2 {
        let result = tvm_client::tvm::run_executor(
            context.clone(),
            ParamsOfRunExecutor {
                message: marker.clone(),
                account: AccountForExecutor::Account {
                    boc: account,
                    unlimited_balance: Some(true),
                },
                execution_options: Some(ExecutionOptions {
                    block_time: Some(now + tick),
                    ..Default::default()
                }),
                return_updated_account: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.transaction["aborted"], false);
        assert!(result.out_messages.is_empty());
        account = result.account;
        let mut after = account_storage_fields(&account, PRIVATENOTE_ABI, "PrivateNote").unwrap();
        after["lastMessage"] = before["lastMessage"].clone();
        assert_eq!(
            before, after,
            "zero credit must preserve every application storage field"
        );
    }
}
