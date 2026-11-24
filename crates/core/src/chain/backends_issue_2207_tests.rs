//! A claim POST reset must reach the existing readback, without repeating the POST.
use super::*;
use proptest::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;

async fn reset_claim_post(posts: Arc<AtomicUsize>) -> anyhow::Error {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(socket.read_u8().await.unwrap());
        }
        let headers = String::from_utf8(headers).unwrap();
        assert!(headers.starts_with("POST /v2/messages HTTP/1.1\r\n"));
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")?
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body[0]["body"], "signed-claim-boc");
        posts.fetch_add(1, Ordering::Relaxed);
        // Zero linger sends RST after the complete POST has been accepted.
        #[allow(deprecated)]
        socket.set_linger(Some(std::time::Duration::ZERO)).unwrap();
    });
    let error = super::super::client::send_message_routed_checked(
        &reqwest::Client::new(),
        &endpoint,
        "signed-claim-boc",
        "0:11",
        "0:22",
        None,
        super::super::client::SubmitKind::Claim,
    )
    .await
    .expect_err("the accepted POST loses its response");
    server.await.unwrap();
    let request = error
        .chain()
        .find_map(|e| e.downcast_ref::<reqwest::Error>())
        .unwrap();
    assert!(request.is_request() && !request.is_connect(), "{error:#}");
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)),
        "{error:#}"
    );
    assert!(!super::super::client::is_transient_transport_failure(
        &error
    ));
    error
}

async fn reconcile_reset(
    target: u128,
    observed: impl IntoIterator<Item = Result<Option<Value>, &'static str>>,
) -> (Result<(), ChainError>, usize, usize, usize) {
    let reads = AtomicUsize::new(0);
    let posts = Arc::new(AtomicUsize::new(0));
    let pauses = AtomicUsize::new(0);
    let mut observed = VecDeque::from_iter(observed);
    let result = submit_claim_confirmed_with(
        "0:tc",
        target,
        || {
            let before_submit = reads.fetch_add(1, Ordering::Relaxed) == 0;
            let state = if before_submit {
                Ok(Some(
                    json!({"tokensPending": (target - TICK_SIZE).to_string()}),
                ))
            } else {
                let next = if observed.len() > 1 {
                    observed.pop_front().unwrap()
                } else {
                    observed.front().unwrap().clone()
                };
                next.map_err(|message| ChainError::Transport(message.into()))
            };
            async { state }
        },
        || async {
            Err(map_claim_submit_err(
                reset_claim_post(Arc::clone(&posts)).await,
            ))
        },
        || async {
            pauses.fetch_add(1, Ordering::Relaxed);
        },
    )
    .await;
    (
        result,
        reads.load(Ordering::Relaxed),
        posts.load(Ordering::Relaxed),
        pauses.load(Ordering::Relaxed),
    )
}

#[tokio::test]
async fn claim_post_reset_confirms_equal_readback_without_resubmit() {
    let target = 2 * TICK_SIZE;
    let (result, reads, posts, pauses) = reconcile_reset(
        target,
        [Ok(Some(json!({"tokensPending": target.to_string()})))],
    )
    .await;
    result.expect("equal readback confirms a claim whose POST response was lost");
    assert_eq!((reads, posts, pauses), (2, 1, 0));
}

#[tokio::test]
async fn claim_post_reset_lower_readback_exhausts_only_the_existing_read_budget() {
    let target = 2 * TICK_SIZE;
    let last_pending = target - 1;
    let (result, reads, posts, pauses) = reconcile_reset(
        target,
        [Ok(Some(json!({"tokensPending": last_pending.to_string()})))],
    )
    .await;
    let error = result.expect_err("a lower cursor never confirms a claim");
    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "claimTokens({target}) was not confirmed within the readback budget after ambiguous submit"
        )),
        "{error}"
    );
    assert!(
        message.contains(&format!(
            "tokensPending remained below target at {last_pending}"
        )),
        "{error}"
    );
    assert!(message.contains("error sending request for url"), "{error}");
    assert!(!message.contains("did not apply"), "{error}");
    let confirmation = ClaimConfirmationParams::canonical();
    assert_eq!(
        (reads, posts, pauses),
        (confirmation.max_reads + 1, 1, confirmation.max_reads - 1)
    );
}

#[tokio::test]
async fn claim_post_reset_keeps_high_water_resync_explicit() {
    let target = 2 * TICK_SIZE;
    let (result, reads, posts, pauses) = reconcile_reset(
        target,
        [Ok(Some(
            json!({"tokensPending": (target + TICK_SIZE).to_string()}),
        ))],
    )
    .await;
    assert!(
        matches!(result, Err(ChainError::ClaimHighWaterResync {attempted, on_chain}) if attempted == target && on_chain == target + TICK_SIZE)
    );
    assert_eq!((reads, posts, pauses), (2, 1, 0));
}

#[tokio::test]
async fn claim_post_reset_missing_malformed_or_failed_read_never_confirms() {
    for observed in [
        Ok(None),
        Ok(Some(json!({}))),
        Ok(Some(json!({"tokensPending":"invalid"}))),
        Err("read unavailable"),
    ] {
        let (result, reads, posts, pauses) = reconcile_reset(2 * TICK_SIZE, [observed]).await;
        result.expect_err("inconclusive readback cannot confirm a claim");
        assert_eq!((reads, posts, pauses), (2, 1, 0));
    }
}

#[tokio::test]
async fn claim_reset_mapping_is_limited_to_the_post_stage() {
    let error = reset_claim_post(Arc::new(AtomicUsize::new(0))).await;
    assert!(
        matches!(map_err(error), ChainError::Chain(_)),
        "other submit mappings stay unchanged"
    );

    // The same reqwest request error from a preflight read has no POST-stage marker.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.read_u8().await.unwrap();
    });
    let error = reqwest::Client::new()
        .get(endpoint)
        .send()
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(error.is_request() && !error.is_connect());
    assert!(matches!(
        map_claim_submit_err(error.into()),
        ChainError::Chain(_)
    ));

    let preparation = reqwest::Client::new()
        .post("invalid URL")
        .build()
        .unwrap_err();
    assert!(preparation.is_builder());
    assert!(matches!(
        map_claim_submit_err(preparation.into()),
        ChainError::Chain(_)
    ));
    assert!(matches!(
        map_claim_submit_err(anyhow!("on-chain submit failed: exit_code=151")),
        ChainError::Contract(_)
    ));
    assert!(matches!(
        map_claim_submit_err(anyhow!("connection reset while preparing claim")),
        ChainError::Chain(_)
    ));
}

proptest! {
    #[test]
    fn claim_post_reset_readback_never_reposts_or_confirms_an_unproven_cursor(
        target in 2 * TICK_SIZE..=10 * TICK_SIZE,
        before_confirmation in 0..ClaimConfirmationParams::canonical().max_reads,
        final_state in 0u8..4,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let behind = Ok(Some(json!({"tokensPending": (target - TICK_SIZE).to_string()})));
        let last = match final_state {
            0 => Ok(Some(json!({"tokensPending": target.to_string()}))),
            1 => behind.clone(),
            2 => Ok(None),
            _ => Ok(Some(json!({"tokensPending":"invalid"}))),
        };
        let observations = std::iter::repeat_n(behind, before_confirmation).chain([last]);
        let (result, reads, posts, pauses) = runtime.block_on(reconcile_reset(target, observations));
        prop_assert_eq!(posts, 1);
        prop_assert_eq!(result.is_ok(), final_state == 0);
        let expected = if final_state == 1 { ClaimConfirmationParams::canonical().max_reads } else { before_confirmation + 1 };
        prop_assert_eq!(reads, expected + 1);
        prop_assert_eq!(pauses, expected - 1);
    }
}
