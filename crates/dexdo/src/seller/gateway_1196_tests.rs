use super::*;
use crate::seller::{drive_advance_with_observer, AdvanceWindows, ClaimStateObserver};
use dexdo_core::{
    order_flags, ChainBackend, ChainError, DealChainState, DealSubscription, LocalNote, Match,
    Note, OfferListing, SellOffer, Settlement, StreamSnapshot, CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX,
    TICK_SIZE,
};
use dexdo_proto::SamplingParams;
use std::sync::Arc;
use std::time::Duration;

const TC: &str = "0:1196";

#[derive(Clone, Copy)]
enum ReadFailure {
    ExhaustedBudget,
    OneAttempt,
}

struct RequiredReadFailure(ReadFailure);

#[async_trait::async_trait]
impl ChainBackend for RequiredReadFailure {
    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
        unimplemented!()
    }

    async fn post_offer(&self, _: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
        unimplemented!()
    }

    async fn place_buy(&self, _: &String, _: &dyn Note) -> Result<(), ChainError> {
        unimplemented!()
    }

    async fn read_match(&self, _: &String) -> Result<Match, ChainError> {
        unimplemented!()
    }

    async fn open_stream(&self, _: &String, _: Vec<u8>, _: &dyn Note) -> Result<(), ChainError> {
        unimplemented!()
    }

    async fn read_handover(&self, _: &String) -> Result<Option<Vec<u8>>, ChainError> {
        unimplemented!()
    }

    async fn accept_probe(&self, _: &String) -> Result<(), ChainError> {
        unimplemented!()
    }

    async fn claim_tokens(&self, _: &String, _: &dyn Note, _: u128) -> Result<(), ChainError> {
        unimplemented!()
    }

    async fn deal_state(&self, _: &String) -> Result<Option<DealChainState>, ChainError> {
        Err(match self.0 {
            ReadFailure::ExhaustedBudget => ChainError::Chain(format!(
                "TokenContract 0:1196: could not obtain a coherent TokenContract snapshot after 3 attempts: attempt 3: bracketed read failed: {}5 attempt(s) in 45s",
                CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX
            )),
            ReadFailure::OneAttempt => {
                ChainError::Transport("chain read exceeded 20s on attempt 1".to_string())
            }
        })
    }

    async fn stop(&self, _: &String, _: &dyn Note) -> Result<Settlement, ChainError> {
        unimplemented!()
    }

    async fn snapshot(&self, _: &String) -> Option<StreamSnapshot> {
        None
    }
}

struct GatewayAvailabilityObserver {
    state: Arc<GatewayState>,
}

impl ClaimStateObserver for GatewayAvailabilityObserver {
    fn observe(&self, _: &String, _: DealChainState) -> Result<(), ChainError> {
        Ok(())
    }

    fn observe_chain_unavailable(&self, token_contract: &String, _: &ChainError) {
        self.state.report_chain_unavailable(token_contract);
    }
}

fn instant_windows() -> AdvanceWindows {
    AdvanceWindows {
        claim_interval: Duration::ZERO,
        seconds_per_tick: Duration::ZERO,
        promote: Duration::ZERO,
        probe: Duration::ZERO,
    }
}

async fn fail_required_read(state: Arc<GatewayState>, failure: ReadFailure) {
    let delivery = state.delivery(TC);
    let result = drive_advance_with_observer(
        &RequiredReadFailure(failure),
        &TC.to_string(),
        &LocalNote::generate(),
        instant_windows(),
        2,
        TICK_SIZE as u64,
        true,
        delivery.count,
        delivery.done,
        &GatewayAvailabilityObserver { state },
    )
    .await;
    assert!(
        result.is_err(),
        "the required read failure still propagates"
    );
}

fn chunk(text: &str, seq: u64) -> UpstreamEvent {
    UpstreamEvent::Chunk(CanonChunk {
        text: text.to_string(),
        seq,
        ..CanonChunk::default()
    })
}

async fn controlled_relay(
    state: &Arc<GatewayState>,
    token_contract: &str,
) -> (
    mpsc::Sender<Result<UpstreamEvent, Status>>,
    mpsc::Receiver<Result<CanonChunk, Status>>,
    tokio::task::JoinHandle<()>,
) {
    let delivery = state.delivery(token_contract);
    let unavailable = state.subscribe_chain_unavailable();
    let (up_tx, up_rx) = mpsc::channel(4);
    let (buyer_tx, buyer_rx) = mpsc::channel(4);
    let relay = tokio::spawn(relay_counting_with_chain_availability(
        up_rx,
        buyer_tx,
        delivery,
        None,
        Some(unavailable),
        u64::MAX,
        u64::MAX,
        false,
    ));
    (up_tx, buyer_rx, relay)
}

#[tokio::test]
async fn exhausted_read_budget_stop_terminates_inflight_stream_as_buyer_error() {
    let state = Arc::new(GatewayState::new());
    let (up_tx, mut buyer_rx, relay) = controlled_relay(&state, TC).await;
    let (other_up_tx, mut other_buyer_rx, other_relay) =
        controlled_relay(&state, "0:other-open-deal").await;
    up_tx.send(Ok(chunk("first", 0))).await.unwrap();
    other_up_tx.send(Ok(chunk("other", 0))).await.unwrap();
    assert_eq!(buyer_rx.recv().await.unwrap().unwrap().text, "first");
    assert_eq!(other_buyer_rx.recv().await.unwrap().unwrap().text, "other");

    fail_required_read(state, ReadFailure::ExhaustedBudget).await;
    drop(up_tx);
    drop(other_up_tx);

    let status = buyer_rx
        .recv()
        .await
        .expect("chain-unavailable stop must be an explicit item, not clean EOF")
        .expect_err("chain-unavailable stop must reach the buyer as an error");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().starts_with("SELLER_CHAIN_UNAVAILABLE"));
    assert_eq!(
        crate::buyer::api::stream_error_policy_action(&status.to_string(), 1),
        crate::buyer::api::StreamErrorPolicyAction::SellerStallsMidStream,
        "the buyer classifies a post-token terminal status as an error, never stop/end_turn"
    );
    assert!(buyer_rx.recv().await.is_none());
    relay.await.unwrap();

    let other_status = other_buyer_rx
        .recv()
        .await
        .expect("stop must terminate every in-flight seller stream")
        .expect_err("another open deal must receive the same explicit status");
    assert!(other_status
        .message()
        .starts_with("SELLER_CHAIN_UNAVAILABLE"));
    assert!(other_buyer_rx.recv().await.is_none());
    other_relay.await.unwrap();
}

#[tokio::test]
async fn one_failed_attempt_inside_the_budget_does_not_stop_serving() {
    let state = Arc::new(GatewayState::new());
    let (up_tx, mut buyer_rx, relay) = controlled_relay(&state, TC).await;
    up_tx.send(Ok(chunk("first", 0))).await.unwrap();
    assert_eq!(buyer_rx.recv().await.unwrap().unwrap().text, "first");

    fail_required_read(state, ReadFailure::OneAttempt).await;
    up_tx.send(Ok(chunk("second", 1))).await.unwrap();
    up_tx
        .send(Ok(UpstreamEvent::Usage(
            BillingUsage::new(0, 2, None).unwrap(),
        )))
        .await
        .unwrap();
    drop(up_tx);

    assert_eq!(buyer_rx.recv().await.unwrap().unwrap().text, "second");
    assert_eq!(
        buyer_rx.recv().await.unwrap().unwrap().usage,
        Some(BillingUsage::new(0, 2, None).unwrap())
    );
    assert!(buyer_rx.recv().await.is_none());
    relay.await.unwrap();
}

#[tokio::test]
async fn keep_serving_preserves_the_existing_clean_relay_behavior() {
    let state = Arc::new(GatewayState::new());
    state.set_chain_unavailable_action(ChainUnavailableAction::KeepServing);
    let (up_tx, mut buyer_rx, relay) = controlled_relay(&state, TC).await;
    up_tx.send(Ok(chunk("first", 0))).await.unwrap();
    assert_eq!(buyer_rx.recv().await.unwrap().unwrap().text, "first");

    fail_required_read(state, ReadFailure::ExhaustedBudget).await;
    up_tx.send(Ok(chunk("second", 1))).await.unwrap();
    up_tx
        .send(Ok(UpstreamEvent::Usage(
            BillingUsage::new(0, 2, None).unwrap(),
        )))
        .await
        .unwrap();
    drop(up_tx);

    assert_eq!(buyer_rx.recv().await.unwrap().unwrap().text, "second");
    assert_eq!(
        buyer_rx.recv().await.unwrap().unwrap().usage,
        Some(BillingUsage::new(0, 2, None).unwrap())
    );
    assert!(buyer_rx.recv().await.is_none());
    relay.await.unwrap();
}

fn authorized_request(
    state: &GatewayState,
    buyer: &LocalNote,
    token_contract: &str,
) -> Request<StreamRequest> {
    let nonce = vec![9; 32];
    state.auth.issue_challenge(token_contract, nonce.clone());
    let signature = buyer.sign(&challenge_bytes(token_contract, &nonce));
    Request::new(StreamRequest {
        token_contract: token_contract.to_string(),
        nonce,
        signature: signature.0.to_vec(),
        request: Some(CanonRequest {
            messages: Vec::new(),
            params: Some(SamplingParams {
                max_tokens: 1,
                ..SamplingParams::default()
            }),
        }),
        billing_grant_tokens: 8,
    })
}

fn open_state(pending: u128) -> DealChainState {
    DealChainState {
        funded: true,
        opened: true,
        probe_accepted: true,
        disputed: false,
        deposit: 1,
        finalized_owed: 0,
        tokens_final: pending,
        tokens_pending: pending,
        probe_tick: 0,
        funded_time: Some(1),
        probe_time: 1,
        last_claim_time: 1,
        dispute_time: 0,
    }
}

fn ordinary_shape() -> DealSubscription {
    DealSubscription {
        deal_flags: 0,
        sub_weeks: 0,
        week_index: 0,
        tokens_per_week: 2 * TICK_SIZE,
        funded_tokens: 2 * TICK_SIZE,
        tokens_paid: 0,
        period_start: 0,
        week_base_tokens: 0,
    }
}

fn exhausted_subscription_shape() -> DealSubscription {
    DealSubscription {
        deal_flags: order_flags::SUBSCRIPTION,
        sub_weeks: 4,
        week_index: 0,
        tokens_per_week: 2 * TICK_SIZE,
        funded_tokens: 8 * TICK_SIZE,
        tokens_paid: 0,
        period_start: 1,
        week_base_tokens: 0,
    }
}

#[tokio::test]
async fn chain_unavailable_refusal_stays_distinct_from_capacity_auth_and_upstream() {
    let state = Arc::new(GatewayState::new());
    let buyer = LocalNote::generate();
    state
        .register_stream(
            TC,
            buyer.pubkey(),
            1,
            open_state(TICK_SIZE),
            ordinary_shape(),
        )
        .unwrap();
    state.report_chain_unavailable(TC);
    state.unregister_stream(TC);
    let service = GatewayService::new(state.clone());

    let chain_status = match service
        .open_stream(authorized_request(&state, &buyer, TC))
        .await
    {
        Err(status) => status,
        Ok(_) => panic!("an unavailable deal must refuse every later authorized request"),
    };
    assert_eq!(chain_status.code(), tonic::Code::FailedPrecondition);
    assert!(
        chain_status
            .message()
            .starts_with("SELLER_CHAIN_UNAVAILABLE"),
        "the operator-visible status is named"
    );

    let mut unauthorized = authorized_request(&state, &buyer, TC);
    unauthorized.get_mut().signature.clear();
    let auth_status = match service.open_stream(unauthorized).await {
        Err(status) => status,
        Ok(_) => panic!("an invalid signature must be refused"),
    };
    assert_eq!(auth_status.code(), tonic::Code::Unauthenticated);

    let capacity_state = Arc::new(GatewayState::new());
    capacity_state
        .register_stream(
            "0:capacity",
            buyer.pubkey(),
            1,
            open_state(2 * TICK_SIZE),
            exhausted_subscription_shape(),
        )
        .unwrap();
    let capacity_status = match GatewayService::new(capacity_state.clone())
        .open_stream(authorized_request(&capacity_state, &buyer, "0:capacity"))
        .await
    {
        Err(status) => status,
        Ok(_) => panic!("exhausted capacity must be refused"),
    };
    assert_eq!(capacity_status.code(), tonic::Code::ResourceExhausted);

    let upstream_state = Arc::new(GatewayState::new());
    let (up_tx, mut buyer_rx, relay) = controlled_relay(&upstream_state, TC).await;
    up_tx
        .send(Err(Status::unavailable("upstream provider unavailable")))
        .await
        .unwrap();
    drop(up_tx);
    let upstream_status = buyer_rx
        .recv()
        .await
        .expect("upstream fault must be an explicit relay item")
        .expect_err("upstream fault must stay an error");
    assert!(buyer_rx.recv().await.is_none());
    relay.await.unwrap();
    assert_eq!(upstream_status.code(), tonic::Code::Unavailable);
    assert_ne!(chain_status.code(), capacity_status.code());
    assert_ne!(chain_status.code(), auth_status.code());
    assert_ne!(chain_status.code(), upstream_status.code());
    assert!(!upstream_status
        .message()
        .starts_with("SELLER_CHAIN_UNAVAILABLE"));
}
