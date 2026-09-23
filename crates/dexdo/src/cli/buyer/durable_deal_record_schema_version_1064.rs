use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const FRAME_MODEL: &str = "qwen--qwen3--32b";
const ORDER_ID: u128 = 4_242;

fn note_addr() -> String {
    format!("0:{}", "a".repeat(64))
}

fn token_contract() -> String {
    format!("0:{}", "b".repeat(64))
}

fn write_fixture(dir: &std::path::Path, schema_version: Option<u32>) -> String {
    let token_contract = token_contract();
    let handle = deals::make_handle_id(&token_contract, deals::DealHandleRole::Buyer);
    // The record names the network the fixture's OWN backend reports. `resolve_buyer_spot_resume`
    // filters durable deals by `chain.network()` (`buyer.rs:8323`), which is what keeps one chain's
    // deal from being resumed on another, and this fixture's backend is a mock -- so the record is a
    // mock's record. It used to say the test chain's name because the trait default did; a mock
    // reporting itself as being on a real chain is the confusion removes.
    let mut record = serde_json::json!({
        "handle": handle,
        "role": "buyer",
        "network": ResumeSelectorChain::new().network(),
        "token_contract": token_contract,
        "note_addr": note_addr(),
        "frame_model": FRAME_MODEL,
        "model_hash": dexdo_core::model_hash_for(FRAME_MODEL),
        "order_book": null,
        "root_model": null,
        "market": null,
        "contracts": "offline-contracts.json",
        "endpoint": {
            "kind": "local-listen",
            "value": "127.0.0.1:0"
        },
        "created_order_ids": [],
        "created_at_unix": 1_700_000_000_u64
    });
    if let Some(schema_version) = schema_version {
        record
            .as_object_mut()
            .expect("fixture is an object")
            .insert("version".to_string(), serde_json::json!(schema_version));
    }
    std::fs::write(
        deals::handle_path(dir, &handle),
        serde_json::to_vec_pretty(&record).expect("serialize deal fixture"),
    )
    .expect("write deal fixture");
    handle
}

struct ResumeSelectorChain {
    token_contract: String,
    liveness_checks: AtomicUsize,
    attribution_reads: AtomicUsize,
    money_posts: AtomicUsize,
}

impl ResumeSelectorChain {
    fn new() -> Self {
        Self {
            token_contract: token_contract(),
            liveness_checks: AtomicUsize::new(0),
            attribution_reads: AtomicUsize::new(0),
            money_posts: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl dexdo_core::ChainBackend for ResumeSelectorChain {
    async fn claim_tokens(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _note: &dyn dexdo_core::Note,
        _cumulative_tokens: u128,
    ) -> Result<(), dexdo_core::ChainError> {
        panic!("buyer resume must not claim seller tokens")
    }

    async fn discover_offers(
        &self,
    ) -> Result<Vec<dexdo_core::OfferListing>, dexdo_core::ChainError> {
        panic!("resume must not discover a new offer")
    }

    async fn post_offer(
        &self,
        _offer: dexdo_core::SellOffer,
        _note: &dyn dexdo_core::Note,
    ) -> Result<(), dexdo_core::ChainError> {
        panic!("buyer resume must not post an offer")
    }

    async fn place_buy(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _note: &dyn dexdo_core::Note,
    ) -> Result<(), dexdo_core::ChainError> {
        self.money_posts.fetch_add(1, Ordering::SeqCst);
        panic!("resume must not submit a second BUY")
    }

    fn model_buy_order_book_identity(&self) -> Option<String> {
        Some(format!("0:{}", "c".repeat(64)))
    }

    async fn poll_attributed_model_buys_for_order_book(
        &self,
        _order_book: &str,
        _cursor: &mut dexdo_core::MatchWatchCursor,
    ) -> Result<Vec<(u128, dexdo_core::MatchedFill)>, dexdo_core::ChainError> {
        self.attribution_reads.fetch_add(1, Ordering::SeqCst);
        Ok(vec![(
            ORDER_ID,
            dexdo_core::MatchedFill {
                order_id: ORDER_ID,
                token_contract: self.token_contract.clone(),
                ticks: 7,
                price_per_tick: 3 * dexdo_core::PRICE_STEP,
            },
        )])
    }

    async fn assert_model_only_resume_target(
        &self,
        token_contract: &dexdo_core::TokenContract,
    ) -> Result<(), dexdo_core::ChainError> {
        self.liveness_checks.fetch_add(1, Ordering::SeqCst);
        assert_eq!(token_contract, &self.token_contract);
        Ok(())
    }

    async fn read_match(
        &self,
        _token_contract: &dexdo_core::TokenContract,
    ) -> Result<dexdo_core::Match, dexdo_core::ChainError> {
        panic!("resume selector must not reconstruct a match")
    }

    async fn open_stream(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _enc_endpoint: Vec<u8>,
        _note: &dyn dexdo_core::Note,
    ) -> Result<(), dexdo_core::ChainError> {
        panic!("buyer resume must not open the seller stream")
    }

    async fn read_handover(
        &self,
        _token_contract: &dexdo_core::TokenContract,
    ) -> Result<Option<Vec<u8>>, dexdo_core::ChainError> {
        panic!("the selector stops before reading the handover")
    }

    async fn stop(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _note: &dyn dexdo_core::Note,
    ) -> Result<dexdo_core::Settlement, dexdo_core::ChainError> {
        panic!("the selector must not settle the deal")
    }

    async fn snapshot(
        &self,
        _token_contract: &dexdo_core::TokenContract,
    ) -> Option<dexdo_core::StreamSnapshot> {
        None
    }
}

async fn selected_fixture(
    schema_version: Option<u32>,
) -> (BuyerSpotResumeSelection, usize, usize, usize) {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let handle = write_fixture(dir.path(), schema_version);
    let path = deals::handle_path(dir.path(), &handle);
    let before = std::fs::read(&path).expect("read fixture before resume");
    let chain = ResumeSelectorChain::new();
    let resume = resolve_buyer_spot_resume(&chain, Some(dir.path()), &note_addr(), FRAME_MODEL)
        .await
        .expect("fixture reaches the production durable resume selector");
    let BuyerSpotResume::Live(selection) = resume else {
        panic!("fixture must select its live durable deal: {resume:?}");
    };
    assert_eq!(
        std::fs::read(path).expect("read fixture after resume"),
        before,
        "resume must not rewrite a known record"
    );
    (
        selection,
        chain.liveness_checks.load(Ordering::SeqCst),
        chain.attribution_reads.load(Ordering::SeqCst),
        chain.money_posts.load(Ordering::SeqCst),
    )
}

#[tokio::test]
async fn versionless_and_known_records_resume_with_identical_behavior_and_numbers() {
    let legacy = selected_fixture(None).await;
    let explicit_pre_versioning = selected_fixture(Some(0)).await;
    let prior = selected_fixture(Some(deals::DEAL_HANDLE_VERSION - 1)).await;
    let current = selected_fixture(Some(deals::DEAL_HANDLE_VERSION)).await;

    assert_eq!(legacy, explicit_pre_versioning);
    assert_eq!(legacy, prior);
    assert_eq!(legacy, current);
    assert_eq!(legacy.0.token_contract, token_contract());
    assert_eq!(
        legacy.0.deal_handle,
        deals::make_handle_id(&token_contract(), deals::DealHandleRole::Buyer)
    );
    assert_eq!(legacy.0.order_id, ORDER_ID);
    assert_eq!(legacy.1, 1, "the recorded deal is proved live once");
    assert_eq!(legacy.2, 1, "the book attribution is read once");
    assert_eq!(legacy.3, 0, "resume submits no second BUY");
}

#[test]
fn newer_schema_refusal_is_structured_named_actionable_and_distinct() {
    let newer_dir = tempfile::tempdir().expect("newer fixture tempdir");
    let handle = write_fixture(newer_dir.path(), Some(deals::DEAL_HANDLE_VERSION + 1));
    let newer = buyer_spot_resume_candidates(Some(newer_dir.path()), &note_addr(), FRAME_MODEL)
        .expect_err("a newer schema must be refused before chain resume");
    let newer_code = machine::classify_error(machine::OP_BUYER_START, &newer);
    assert_eq!(newer_code.as_str(), "DEAL_RECORD_SCHEMA_TOO_NEW");
    assert!(!newer_code.retryable());

    let mut context = BuyerMachineErrorContext::default();
    context.enrich_from_error(&newer);
    let (mut writer, captured) = machine::BuyerEventWriter::capturing();
    writer
        .error_with_cause(
            machine::OP_BUYER_START,
            newer_code,
            &newer,
            context.fields(),
        )
        .expect("emit production buyer error envelope");
    let error = captured.lock().expect("event capture lock")[0].clone();
    assert_eq!(error["schema"], "dexdo.error.v1");
    assert_eq!(error["event"], "error");
    assert_eq!(error["operation"], machine::OP_BUYER_START);
    assert_eq!(error["code"], "DEAL_RECORD_SCHEMA_TOO_NEW");
    assert_eq!(error["retryable"], false);
    assert_eq!(error["deal_handle"], handle);
    assert_eq!(
        error["record_schema_version"],
        deals::DEAL_HANDLE_VERSION + 1
    );
    assert_eq!(
        error["max_supported_schema_version"],
        deals::DEAL_HANDLE_VERSION
    );
    assert_eq!(
        error["operator_action"],
        "keep_older_runtime_pinned_until_deal_terminates"
    );
    let cause = error["cause"].as_str().expect("structured refusal cause");
    assert!(cause.contains(&handle), "{cause}");
    assert!(
        cause.contains("keep the older runtime pinned until that deal terminates"),
        "{cause}"
    );

    let missing = buyer_spot_resume_refusal(
        FRAME_MODEL,
        &dexdo_core::ChainError::Chain("no such deal".to_string()),
        &BuyerSpotResume::NoCandidate,
    );
    let missing_code = machine::classify_error(machine::OP_BUYER_START, &missing);

    let io_dir = tempfile::tempdir().expect("I/O fixture tempdir");
    std::fs::create_dir(io_dir.path().join("deal-read-error.json"))
        .expect("create unreadable-as-file fixture");
    let io = buyer_spot_resume_candidates(Some(io_dir.path()), &note_addr(), FRAME_MODEL)
        .expect_err("a directory in place of a record is a read error");
    let io_code = machine::classify_error(machine::OP_BUYER_START, &io);

    assert_ne!(newer_code, missing_code, "newer schema != no such deal");
    assert_ne!(newer_code, io_code, "newer schema != record read error");
}

#[test]
fn freshly_written_runtime_record_carries_current_schema_version() {
    let dir = tempfile::tempdir().expect("writer fixture tempdir");
    let token_contract = token_contract();
    let note_addr = note_addr();
    // The network is passed, not resolved: production takes it from the manifest this run is on
    // (`save_runtime_deal_handle_for_network`), and a test that let it resolve from an unset
    // environment was asserting the fallback label rather than the record's contract.
    let written = crate::cli::commands::save_runtime_deal_handle_for_network(
        crate::cli::commands::RuntimeDealHandleInput {
            role: deals::DealHandleRole::Buyer,
            deals_dir: Some(dir.path()),
            token_contract: &token_contract,
            note_addr: &note_addr,
            frame_model: FRAME_MODEL,
            market: None,
            market_path: None,
            contracts: std::path::Path::new("offline-contracts.json"),
            endpoint: Some(deals::DealEndpointInfo {
                kind: "local-listen".to_string(),
                value: "127.0.0.1:0".to_string(),
            }),
            created_order_ids: vec![ORDER_ID],
        },
        "net-a",
        false,
    )
    .expect("production runtime handle writer succeeds");
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(deals::handle_path(dir.path(), &written.handle))
            .expect("read freshly written handle"),
    )
    .expect("fresh handle is JSON");
    assert_eq!(record["version"], deals::DEAL_HANDLE_VERSION);
    assert_eq!(record["network"], "net-a");
}

#[test]
fn unknown_field_on_newer_schema_produces_structured_too_new_refusal() {
    let dir = tempfile::tempdir().expect("newer fixture tempdir");
    let handle = write_fixture(dir.path(), Some(deals::DEAL_HANDLE_VERSION + 1));
    let path = deals::handle_path(dir.path(), &handle);
    let mut record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).expect("read newer fixture before adding its field"),
    )
    .expect("newer fixture is JSON");
    record
        .as_object_mut()
        .expect("newer fixture is an object")
        .insert(
            "future_record_meaning".to_string(),
            serde_json::json!("introduced-by-schema-v2"),
        );
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&record).expect("serialize newer fixture"),
    )
    .expect("write newer fixture with its field");

    let refusal = buyer_spot_resume_candidates(Some(dir.path()), &note_addr(), FRAME_MODEL)
        .expect_err("a realistic newer schema must be refused before full-shape parsing");
    let code = machine::classify_error(machine::OP_BUYER_START, &refusal);
    assert_eq!(code.as_str(), "DEAL_RECORD_SCHEMA_TOO_NEW");
    assert!(!code.retryable());

    let mut context = BuyerMachineErrorContext::default();
    context.enrich_from_error(&refusal);
    let (mut writer, captured) = machine::BuyerEventWriter::capturing();
    writer
        .error_with_cause(machine::OP_BUYER_START, code, &refusal, context.fields())
        .expect("emit production buyer error envelope");
    let error = captured.lock().expect("event capture lock")[0].clone();
    assert_eq!(error["code"], "DEAL_RECORD_SCHEMA_TOO_NEW");
    assert_eq!(error["deal_handle"], handle);
    assert_eq!(
        error["record_schema_version"],
        deals::DEAL_HANDLE_VERSION + 1
    );
    assert_eq!(
        error["max_supported_schema_version"],
        deals::DEAL_HANDLE_VERSION
    );
    assert_eq!(
        error["operator_action"],
        "keep_older_runtime_pinned_until_deal_terminates"
    );
}

#[test]
fn unknown_field_on_supported_schema_remains_malformed() {
    let dir = tempfile::tempdir().expect("supported fixture tempdir");
    let handle = write_fixture(dir.path(), Some(deals::DEAL_HANDLE_VERSION));
    let path = deals::handle_path(dir.path(), &handle);
    let mut record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).expect("read supported fixture before adding the typo"),
    )
    .expect("supported fixture is JSON");
    record
        .as_object_mut()
        .expect("supported fixture is an object")
        .insert("unknown_typo".to_string(), serde_json::json!(true));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&record).expect("serialize malformed supported fixture"),
    )
    .expect("write malformed supported fixture");

    let malformed = buyer_spot_resume_candidates(Some(dir.path()), &note_addr(), FRAME_MODEL)
        .expect_err("an unknown field in a supported schema must remain malformed");
    let rendered = format!("{malformed:#}");
    assert!(rendered.contains("parse deal handle"), "{rendered}");
    assert!(
        rendered.contains("unknown field `unknown_typo`"),
        "{rendered}"
    );
    assert!(
        malformed.chain().all(|cause| cause
            .downcast_ref::<deals::DealHandleSchemaTooNew>()
            .is_none()),
        "a malformed supported record must not be classified as too new: {rendered}"
    );
    assert_ne!(
        machine::classify_error(machine::OP_BUYER_START, &malformed).as_str(),
        "DEAL_RECORD_SCHEMA_TOO_NEW"
    );
}
