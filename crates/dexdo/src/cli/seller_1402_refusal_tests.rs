// an already deployed deal must not be refused at seller startup.

// Mainnet, 2026-08-16, contracts 4.0.35. `dexdo provision` deployed and funded the per-deal
// `TokenContract` `0ba5349a...` and wrote it into `market.json`. The very next command, `dexdo seller`
// with that manifest, declined it -- `seller startup did not take deal at max_open_deals` -- because
// the one slot the policy allows was held by `5484c260...`, a deal whose account no longer answers
// `getState`. The seller then deployed and funded `140d9cd8...` to carry that settled deal's residual
// instead. The seller's note paid 32 000 000 000 raw ECC[2] -- two 16-SHELL deposits -- for one
// deal, and the buyer was handed a manifest pointing at the contract nobody serves.

// correctly stopped a refusal from spending, but the refusal itself abandoned a funded
// TokenContract. Capacity is now enforced before `provision`; startup serves every contract that
// already exists. This regression drives the production entry (`run_seller`) over the old pre-state
// and asserts both halves: the handed deal is served, and the queued residual does not leap over the
// runtime capacity while that deal holds it.

/// The seller args this regression needs, in the shape `run_seller` reads them. Deliberately its
/// own builder rather than a shared one: the sibling mock-pool tests build theirs behind
/// `#[cfg(feature = "net-a")]`, and a money regression has to run in the default-feature gate CI
/// actually enforces (`cargo test --workspace --locked`), not only under the removed chain feature.
fn refusal_seller_args(
    root: &std::path::Path,
    token_contract: String,
    nonce: u64,
    gateway_listen: std::net::SocketAddr,
    policy: std::path::PathBuf,
) -> crate::cli::args::SellerArgs {
    crate::cli::args::SellerArgs {
        max_concurrent_requests_per_deal:
            dexdo_core::params::SELLER_MAX_CONCURRENT_REQUESTS_PER_DEAL,
        mock: crate::cli::args::MockFlags {
            mock_model: true,
            mock_chain: true,
        },
        identity: crate::cli::args::IdentityArgs {
            note_key: Some(root.join("seller.key")),
            note_index: 0,
            note_addr: None,
        },
        registry: crate::cli::args::ModelRegistryValidationArgs::default(),
        gateway_listen,
        gateway_advertise: None,
        allow_private_advertise: false,
        require_advertise_probe: false,
        endpoints_file: Some(root.join("endpoints.json")),
        deals_dir: Some(root.join("deals")),
        token_contract: Some(token_contract),
        market: None,
        nonce: Some(nonce),
        subscription: false,
        mock_token_count: 4,
        model: None,
        allow_unverified_model: false,
        models: root.join("unused-models.json"),
        policy: Some(policy),
    }
}

/// The exact mainnet pre-state from: one settled parent has a queued residual, while a second
/// paid TokenContract is vacant. `max_open_deals=1` must not choose by directory order and abandon
/// either existing contract.
#[tokio::test]
async fn startup_above_max_serves_the_paid_candidate_before_funding_any_residual() {
    let root = tempfile::tempdir().expect(" regression directory");
    let seller_seed = [0x14; 32];
    crate::cli::support::write_owner_only_key_fixture(
        &root.path().join("seller.key"),
        &hex::encode(seller_seed),
    );
    let seller_note = Arc::new(
        dexdo_core::NoteTree::from_secret_hex(&hex::encode(seller_seed))
            .unwrap()
            .node(0)
            .unwrap(),
    );
    let seller_owner = format!(
        "0:{}",
        seller_note
            .pubkey()
            .ed
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    // `provision` deployed and funded this one, and wrote it into the manifest handed to `seller`.
    let provisioned_tc = format!("0:{}", "1".repeat(64));
    // The settled deal still holding the single slot: its account is gone, so nothing on the mock
    // chain answers for it, exactly as `getState` answered nothing on mainnet.
    let settled_tc = format!("0:{}", "2".repeat(64));
    // Where the successor would land: the mock provisioner is deterministic in
    // `{seller}:{frame_model}:{nonce}`, and the settled deal's own nonce is 20.
    let successor_tc = format!(
        "0:{}",
        dexdo_core::model_hash_for(&format!("{seller_owner}:mock:21")).trim_start_matches("0x")
    );
    let chain = MockChainBackend::new(
        root.path().join("endpoints.json"),
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    );
    let gateway: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    // The canonical single-deal seller policy, the one the mainnet run used.
    let policy_path = root.path().join("seller-policy.json");
    crate::cli::support::write_owner_only_key_fixture(
        &policy_path,
        &serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "seller": {
                "on": {
                    "after_deal_done": "retire",
                    "buyer_no_show": "retire_gateway",
                    "dispute_against_me": "hold"
                },
                "max_open_deals": 1
            }
        }))
        .unwrap(),
    );

    // The settled deal as an earlier run left it: a handle in the deals directory and an
    // authoritative owner-fill lineage with unmatched capacity and no successor linked yet.
    let deals_dir = root.path().join("deals");
    let settled_market = dexdo_core::MarketManifest {
        network: "mock".to_string(),
        frame_model: "mock".to_string(),
        model_hash: dexdo_core::model_hash_for("mock"),
        inference_order_book: "mock".to_string(),
        root_model: "mock".to_string(),
        token_contract: settled_tc.clone(),
        seller_note: seller_owner.clone(),
        nonce: 20,
        price_per_tick: u128::from(dexdo_core::PRICE_STEP as u64),
        max_ticks: 4,
    };
    deals::save_deal_handle(
        &deals_dir,
        &deals::DealHandle {
            version: deals::DEAL_HANDLE_VERSION,
            handle: deals::make_handle_id(&settled_tc, deals::DealHandleRole::Seller),
            role: deals::DealHandleRole::Seller,
            network: "mock".to_string(),
            token_contract: settled_tc.clone(),
            note_addr: seller_owner.clone(),
            frame_model: settled_market.frame_model.clone(),
            model_hash: Some(settled_market.model_hash.clone()),
            order_book: Some(settled_market.inference_order_book.clone()),
            root_model: Some(settled_market.root_model.clone()),
            market: Some(settled_market),
            contracts: root
                .path()
                .join("unused-contracts.json")
                .display()
                .to_string(),
            endpoint: Some(deals::DealEndpointInfo {
                kind: "gateway".to_string(),
                value: gateway.to_string(),
            }),
            created_order_ids: Vec::new(),
            created_at_unix: deals::now_unix().unwrap(),
        },
    )
    .unwrap();
    let cursor_path = super::seller_watch_cursor_path(Some(&deals_dir), &settled_tc).unwrap();
    std::fs::create_dir_all(cursor_path.parent().unwrap()).unwrap();
    std::fs::write(
        &cursor_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "token_contract": settled_tc,
            "source": MatchWatchCursor::new(0),
            "last_polled_unix": null,
            "opened_at_unix": null,
            "fill": dexdo::seller::SellerFillLineage {
                order_id: 1,
                offered_ticks: 4,
                matched_ticks: 2,
                residual_ticks: 2,
                price_per_tick: dexdo_core::PRICE_STEP as u64,
                replacement_nonce: None,
                replacement_token_contract: None,
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let seller = super::run_seller(refusal_seller_args(
        root.path(),
        provisioned_tc.clone(),
        7,
        gateway,
        policy_path,
    ));
    tokio::pin!(seller);
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        tokio::select! {
            result = &mut seller => {
                panic!("seller returned before serving the paid candidate: {result:?}");
            }
            () = async {
                loop {
                    if matches!(
                        chain.confirm_offer_outcome(&provisioned_tc).await,
                        Ok(Some(_))
                    ) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            } => {}
        }
    })
    .await
    .expect("the already paid candidate must be served instead of refused");

    // The runtime cap still orders NEW spending: while the paid candidate holds the sole slot, the
    // settled parent's residual stays queued instead of buying a successor around it.
    assert_eq!(
        chain.confirm_offer_outcome(&successor_tc).await.unwrap(),
        None,
        "the queued residual must wait until runtime capacity is free"
    );
}
