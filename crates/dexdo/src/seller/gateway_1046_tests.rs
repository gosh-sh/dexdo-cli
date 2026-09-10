use super::*;
use crate::seller::upstream::anthropic::AnthropicConfig;
use dexdo_core::{LocalNote, Note, TICK_SIZE};
use dexdo_proto::{ChatMessage, SamplingParams};
use std::panic::{self, AssertUnwindSafe};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::StreamExt;

const UNREGISTERED_MESSAGE: &str = "this deal is not registered on this gateway";

fn opened_state(tokens_pending: u128) -> DealChainState {
    DealChainState {
        funded: true,
        opened: true,
        probe_accepted: true,
        disputed: false,
        deposit: 1,
        finalized_owed: 0,
        tokens_final: tokens_pending,
        tokens_pending,
        probe_tick: 0,
        funded_time: Some(1),
        probe_time: 1,
        last_claim_time: 1,
        dispute_time: 0,
    }
}

fn ordinary_deal(funded_tokens: u128) -> DealSubscription {
    DealSubscription {
        deal_flags: 0,
        sub_weeks: 0,
        week_index: 0,
        tokens_per_week: funded_tokens,
        funded_tokens,
        tokens_paid: 0,
        period_start: 0,
        week_base_tokens: 0,
    }
}

fn authorized_request(
    state: &GatewayState,
    buyer: &LocalNote,
    token_contract: &str,
    max_tokens: u32,
) -> Request<StreamRequest> {
    let nonce = vec![0x46; 32];
    state.auth.issue_challenge(token_contract, nonce.clone());
    let signature = buyer.sign(&challenge_bytes(token_contract, &nonce));
    Request::new(StreamRequest {
        token_contract: token_contract.to_string(),
        nonce,
        signature: signature.0.to_vec(),
        request: Some(CanonRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            params: Some(SamplingParams {
                max_tokens,
                ..SamplingParams::default()
            }),
        }),
        billing_grant_tokens: u64::from(max_tokens).saturating_add(1),
    })
}

async fn anthropic_fixture() -> (AnthropicConfig, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"exposed\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0_u8; 8192];
        let _ = socket.read(&mut request).await.unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            sse.len(),
            sse
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let config = AnthropicConfig {
        base_url: format!("http://{address}"),
        model: "fixture-model".into(),
        frame_model: "fixture-frame".into(),
        api_key_env: "PATH".into(),
        tokenizer_family: "fixture-tokenizer".into(),
        max_output_tokens: Some(8),
    };
    (config, server)
}

#[tokio::test]
async fn old_auth_before_limits_window_is_refused_as_unregistered() {
    let (config, server) = anthropic_fixture().await;
    let state = Arc::new(GatewayState::with_upstream(UpstreamConfig::Anthropic(
        config,
    )));
    let buyer = LocalNote::generate();
    let token_contract = "0:1046-old-window";

    let snapshot = state
        .reconcile_deal_capacity(
            token_contract,
            opened_state(TICK_SIZE),
            ordinary_deal(TICK_SIZE + 4),
        )
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.available().unwrap(), 4);
    state.auth.register(token_contract, buyer.pubkey());
    let request = authorized_request(&state, &buyer, token_contract, 1);

    match GatewayService::new(state).open_stream(request).await {
        Err(status) => {
            server.abort();
            let _ = server.await;
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            assert_eq!(status.message(), UNREGISTERED_MESSAGE);
            assert_ne!(status.message(), "deal delivery capacity is exhausted");
        }
        Ok(response) => {
            let mut stream = response.into_inner();
            let mut observed = Vec::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => observed.push(format!("chunk:{}", chunk.text)),
                    Err(status) => {
                        observed.push(format!("error:{:?}:{}", status.code(), status.message()))
                    }
                }
            }
            server.await.unwrap();
            panic!(
                "unregistered deal reached the unbounded relay instead of being refused: {observed:?}"
            );
        }
    }
}

#[tokio::test]
async fn registered_deal_keeps_reservation_and_delivery_numbers() {
    let state = Arc::new(GatewayState::new());
    let buyer = LocalNote::generate();
    let token_contract = "0:1046-registered";
    state
        .register_stream(
            token_contract,
            buyer.pubkey(),
            3,
            opened_state(TICK_SIZE),
            ordinary_deal(TICK_SIZE + 5),
        )
        .unwrap();

    let before = state.capacity_snapshot(token_contract).unwrap().unwrap();
    assert_eq!(before.local_delivered_after_anchor, 0);
    assert_eq!(before.outstanding_reservation, 0);
    assert_eq!(before.available().unwrap(), 5);

    let mut stream = GatewayService::new(state.clone())
        .open_stream(authorized_request(&state, &buyer, token_contract, 3))
        .await
        .unwrap()
        .into_inner();
    let mut delivered_chunks = 0;
    let mut terminal_usage = None;
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let Some(usage) = chunk.usage {
            terminal_usage = Some(usage);
        } else {
            assert_eq!(chunk.token_ids.len(), 1);
            delivered_chunks += 1;
        }
    }

    assert_eq!(delivered_chunks, 3);
    assert_eq!(terminal_usage, Some(BillingUsage::new(1, 3, None).unwrap()));
    assert_eq!(
        state.delivery(token_contract).count.load(Ordering::Acquire),
        4
    );
    let after = state.capacity_snapshot(token_contract).unwrap().unwrap();
    assert_eq!(after.local_delivered_after_anchor, 4);
    assert_eq!(after.outstanding_reservation, 0);
    assert_eq!(after.available().unwrap(), 1);
}

#[tokio::test]
async fn registered_mock_zero_output_limit_is_rejected_without_a_reservation() {
    let state = Arc::new(GatewayState::new());
    let buyer = LocalNote::generate();
    let token_contract = "0:1046-mock-no-show";
    state
        .register_stream(
            token_contract,
            buyer.pubkey(),
            0,
            opened_state(TICK_SIZE),
            ordinary_deal(TICK_SIZE + 5),
        )
        .unwrap();

    let status = match GatewayService::new(state.clone())
        .open_stream(authorized_request(&state, &buyer, token_contract, 1))
        .await
    {
        Ok(_) => panic!("a registered zero-output model cannot open a request"),
        Err(status) => status,
    };
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "canonical request output limit must be nonzero"
    );
    assert_eq!(
        state.delivery(token_contract).count.load(Ordering::Acquire),
        0
    );
    let snapshot = state.capacity_snapshot(token_contract).unwrap().unwrap();
    assert_eq!(snapshot.local_delivered_after_anchor, 0);
    assert_eq!(snapshot.outstanding_reservation, 0);
    assert_eq!(snapshot.available().unwrap(), 5);
}

#[tokio::test]
async fn failed_limits_publication_never_makes_auth_visible_to_open_stream() {
    let state = Arc::new(GatewayState::new());
    let poison_target = Arc::clone(&state);
    std::thread::spawn(move || {
        poison_target.poison_limits_for_test("issue 1046 limits publication probe");
    })
    .join()
    .expect_err("the limits lock must be poisoned by the fixture");

    let buyer = LocalNote::generate();
    let token_contract = "0:1046-publication-order";
    let registration = panic::catch_unwind(AssertUnwindSafe(|| {
        state.register_stream(
            token_contract,
            buyer.pubkey(),
            1,
            opened_state(TICK_SIZE),
            ordinary_deal(TICK_SIZE + 1),
        )
    }))
    .expect("register_stream must return a named error after lock poison");
    assert!(registration
        .unwrap_err()
        .to_string()
        .contains("seller gateway limits"));

    match GatewayService::new(state.clone())
        .open_stream(authorized_request(&state, &buyer, token_contract, 1))
        .await
    {
        Err(status) => {
            assert_eq!(status.code(), tonic::Code::Unauthenticated);
            assert_eq!(status.message(), "challenge-response failed");
        }
        Ok(_) => {
            panic!("open_stream observed auth even though the preceding limits publication failed")
        }
    }
}
