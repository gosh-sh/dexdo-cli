use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

type ChainResult<T> = std::result::Result<T, dexdo_core::ChainError>;

struct RefusingModelBook {
    unframed_detail: String,
    money_submissions: AtomicUsize,
}

impl RefusingModelBook {
    fn record_unexpected_money_submission(&self) -> ChainResult<()> {
        self.money_submissions.fetch_add(1, Ordering::SeqCst);
        Err(dexdo_core::ChainError::Chain(
            "unexpected buyer money submission after a quote refusal".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl dexdo_core::ChainBackend for RefusingModelBook {
    async fn discover_offers(&self) -> ChainResult<Vec<dexdo_core::OfferListing>> {
        Ok(Vec::new())
    }

    async fn post_offer(
        &self,
        _offer: dexdo_core::SellOffer,
        _note: &dyn dexdo_core::Note,
    ) -> ChainResult<()> {
        unreachable!("the buyer preflight cannot post a seller offer")
    }

    async fn assert_model_buy_matches_executable_quote(
        &self,
        _ticks: u128,
        _max_price_per_tick: u128,
    ) -> ChainResult<()> {
        Err(dexdo_core::ChainError::Chain(self.unframed_detail.clone()))
    }

    async fn place_buy(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _note: &dyn dexdo_core::Note,
    ) -> ChainResult<()> {
        self.record_unexpected_money_submission()
    }

    async fn place_buy_by_model(
        &self,
        _note: &dyn dexdo_core::Note,
        _ticks: u128,
        _max_price_per_tick: u128,
        _escrow: u128,
        _flags: u8,
        _deadline: u64,
    ) -> ChainResult<()> {
        self.record_unexpected_money_submission()
    }

    async fn place_buy_by_model_with_submit_identity(
        &self,
        _note: &dyn dexdo_core::Note,
        _quoted_order: Option<&dexdo_core::OrderBookOrder>,
        _ticks: u128,
        _max_price_per_tick: u128,
        _escrow: u128,
        _cursor: &mut dexdo_core::MatchWatchCursor,
        _before_post: &mut (dyn FnMut(String, dexdo_core::MatchWatchCursor, u128) -> ChainResult<()>
                  + Send),
    ) -> ChainResult<()> {
        self.record_unexpected_money_submission()
    }

    async fn read_match(
        &self,
        _token_contract: &dexdo_core::TokenContract,
    ) -> ChainResult<dexdo_core::Match> {
        unreachable!("the buyer preflight cannot read a matched deal")
    }

    async fn open_stream(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _enc_endpoint: Vec<u8>,
        _note: &dyn dexdo_core::Note,
    ) -> ChainResult<()> {
        unreachable!("the buyer preflight cannot open a seller stream")
    }

    async fn read_handover(
        &self,
        _token_contract: &dexdo_core::TokenContract,
    ) -> ChainResult<Option<Vec<u8>>> {
        unreachable!("the buyer preflight cannot read a deal handover")
    }

    async fn claim_tokens(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _note: &dyn dexdo_core::Note,
        _cumulative_tokens: u128,
    ) -> ChainResult<()> {
        unreachable!("the buyer preflight cannot submit a seller claim")
    }

    async fn stop(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _note: &dyn dexdo_core::Note,
    ) -> ChainResult<dexdo_core::Settlement> {
        unreachable!("the buyer preflight cannot submit STOP")
    }

    async fn snapshot(
        &self,
        _token_contract: &dexdo_core::TokenContract,
    ) -> Option<dexdo_core::StreamSnapshot> {
        None
    }
}

fn write_buyer_policy(path: &std::path::Path) {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "buyer": {
                "on": {
                    "no_handover_after_match": "fail_closed",
                    "malformed_handover": "fail_closed",
                    "dead_gateway": "fail_closed",
                    "empty_stream": "fail_closed",
                    "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                    "bad_output_scam": "stop"
                },
                "failover": {
                    "max_sellers_to_try": 1,
                    "total_spend_cap_shells": 1000000000_u64
                }
            }
        }))
        .expect("serialize buyer policy"),
    )
    .expect("write buyer policy");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn issue_1054_default_proactive_model_only_refusal_is_framed_before_money_submit() {
    let _env_lock = dexdo_pn_pool_env_lock().lock().unwrap();
    let dir = tempfile::tempdir().expect("create isolated buyer preflight directory");
    let note_addr = format!("0:{}", "a".repeat(64));
    let frame_model = "Qwen3-32B";
    let pool_path = dir.path().join("pool.json");
    crate::cli::support::write_owner_only_key_fixture(
        &pool_path,
        &serde_json::to_string_pretty(&serde_json::json!({
            "token_type": dexdo_core::params::SHELL_CURRENCY_ID,
            "notes": [{"address": note_addr, "owner_secret_key_hex": "00"}]
        }))
        .expect("serialize note pool"),
    );
    let _pool = EnvVarGuard::set("DEXDO_PN_POOL", pool_path.as_os_str());

    let policy = dir.path().join("policy.json");
    write_buyer_policy(&policy);
    let models = dir.path().join("models.json");
    let args = BuyerArgs {
        mock: MockFlags {
            mock_model: false,
            mock_chain: false,
        },
        identity: IdentityArgs {
            note_key: None,
            note_index: 0,
            note_addr: Some(note_addr.clone()),
        },
        registry: ModelRegistryValidationArgs::default(),
        endpoints_file: None,
        deals_dir: Some(dir.path().join("deals")),
        token_contract: None,
        resume: false,
        preserve_deal_on_exit: false,
        wait_for_seller: false,
        market: None,
        max_tokens: 1,
        local_listen: Some("127.0.0.1:0".parse().unwrap()),
        continuity_mode: ContinuityModeArg::Proactive,
        json: false,
        anthropic_compat: false,
        frame_model: Some(frame_model.to_string()),
        allow_unverified_model: true,
        models: models.clone(),
        ticks: 2,
        max_price_per_tick: dexdo_core::PRICE_STEP,
        escrow: None,
        policy: Some(policy),
    };
    let unframed_class = dexdo_core::params::EMPTY_MODEL_BOOK_CLASS;
    let unframed_detail = format!(
        "{unframed_class}: no executable matching ask for InferenceOrderBook 0:book at \
         max_price_per_tick {}, requested ticks 2: no resting asks in this model book",
        dexdo_core::PRICE_STEP
    );
    let chain = Arc::new(RefusingModelBook {
        unframed_detail: unframed_detail.clone(),
        money_submissions: AtomicUsize::new(0),
    });
    let backend: Arc<dyn dexdo_core::ChainBackend> = chain.clone();
    let note: Arc<dyn dexdo_core::Note> = Arc::new(dexdo_core::LocalNote::generate());
    let mut machine_events = None;
    let mut machine_context = BuyerMachineErrorContext::default();

    let error = run_buyer_inner(
        args,
        &mut machine_events,
        &mut machine_context,
        BuyerCommandRuntime {
            backend: Some((backend, note)),
            chain_preflight: BuyerChainPreflight::OfflineTest,
            shutdown: Box::pin(std::future::pending()),
        },
    )
    .await
    .expect_err("an unmatchable proactive model book must refuse before escrow submission");
    let rendered = format!("{error:#}");
    let preflight = format!("BUYER_PREFLIGHT matchable=false reason={unframed_class} detail=");
    assert!(
        rendered.contains(&preflight),
        "the default proactive entry must carry the existing class in the shared frame: {rendered}"
    );
    assert!(
        rendered.contains(&unframed_detail),
        "the shared frame must retain the unframed refusal detail: {rendered}"
    );

    let next_command = rendered
        .lines()
        .find_map(|line| line.strip_prefix("next_command="))
        .expect("the framed refusal must carry a next_command remedy");
    let next_argv = crate::cli::support::printed_commands::shell_split(next_command)
        .expect("the next_command remedy must survive a shell");
    assert_eq!(
        next_argv,
        vec![
            "dexdo",
            "executable-book",
            frame_model,
            "--ticks",
            "2",
            "--max-price-per-tick",
            // The remedy is a line to type back, so the ceiling is stated as the argument takes
            // it: one SHELL a tick.
            "1",
            "--note-addr",
            note_addr.as_str(),
            "--models",
            models.to_str().unwrap(),
        ],
        "the remedy must re-read the same model, volume, ceiling, note, and manifests"
    );
    assert_eq!(
        chain.money_submissions.load(Ordering::SeqCst),
        0,
        "the framed refusal must remain a read-only preflight"
    );
}
