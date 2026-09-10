//! one connection reset ended a purchase whose money had already been POSTed.

//! Measured on head `0e5af1d9`, live shellnet, 10 September 2026 10:56 UTC. The buyer's money POST
//! was accepted at 10:56:17 and the order filled on chain by 10:56:23 -- the acceptance suite read
//! the funded deal contract itself. By 10:56:27 the buyer's own request had died with

//! ```text
//! shellnet ambiguous submit: buyer money POST may have landed but its MatchedFill is not yet
//! provable; journal retained and no resubmit is safe: shellnet: error sending request for url
//! (https://dd-shellnet.ackinacki.org/v2/account?...): client error (SendRequest): connection
//! error: connection reset
//! ```

//! The wait had `DEAL_WAIT_SECS` = 300 seconds and spent under ten of them, because the read that
//! failed was outside the retry policy -- `poll_inference_filled_tcs` called `fetch_dapp_id` and
//! its own GraphQL straight through `http`, while the readers on either side of it in the same file
//! go through `retry_transient_read`. That is the shape removed from the note-deploy receipt
//! poll; the buy path still had it.

//! Wrapping the read is necessary but not sufficient, and this is the second half of the defect: a
//! connection reset in flight is not `is_connect`, not `is_timeout`, not `is_body`, not `is_decode`
//! and carries no status, so no predicate in this client called it transient and the wrapper would
//! have declined it too. `hyper_util` files it under `ErrorKind::SendRequest`, and
//! `reqwest::Error::is_connect` answers `true` only for `ErrorKind::Connect`.

//! The reset is produced here rather than described: a loopback listener accepts the connection and
//! closes it with the request still unread, which the kernel turns into a reset.

use super::{is_transient_read_failure, is_transient_transport_failure, retry_transient_read};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The failure from the run, reproduced: a read whose connection is destroyed before any response.
async fn read_whose_connection_is_reset() -> anyhow::Error {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener");
    let addr = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept one connection");
        // Closing a socket whose receive buffer still holds the request is a reset: the kernel has
        // nothing to do with bytes nobody read.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(stream);
    });
    let error = reqwest::Client::new()
        .get(format!(
            "http://{addr}/v2/account?account_id=note&dapp_id=note"
        ))
        .send()
        .await
        .expect_err("a destroyed connection cannot produce a response");
    server.await.expect("listener task");
    anyhow::Error::new(error).context("shellnet")
}

/// The rendered failure is the one from the run, so the tests below are about that error and not
/// about one composed for the occasion.
#[tokio::test]
async fn the_reproduced_failure_is_the_one_the_run_reported() {
    let rendered = format!("{:#}", read_whose_connection_is_reset().await);
    assert!(
        rendered.contains("error sending request for url"),
        "{rendered}"
    );
    assert!(
        rendered.contains("client error (SendRequest)"),
        "{rendered}"
    );
    // Linux says `connection reset`, macOS `Connection reset by peer (os error 54)`.
    assert!(
        rendered.to_ascii_lowercase().contains("connection reset"),
        "{rendered}"
    );
}

/// The defect, stated as the retry policy sees it.
#[tokio::test]
async fn a_reset_read_is_a_transient_read_failure() {
    let error = read_whose_connection_is_reset().await;
    assert!(
        is_transient_read_failure(&error),
        "a read that got no answer is the plainest repeatable failure there is: {error:#}"
    );
}

/// And the policy therefore repeats it. Without this the wrapper added to the buy poll would have
/// declined the very failure it was added for.
#[tokio::test]
async fn the_read_policy_repeats_a_reset_read() {
    let calls = AtomicUsize::new(0);
    let answer: anyhow::Result<u64> = retry_transient_read(|| async {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(read_whose_connection_is_reset().await);
        }
        Ok(1_789_037_783)
    })
    .await;

    assert_eq!(
        answer.expect("a reset read is repeated, and the repeat answers"),
        1_789_037_783
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "exactly one repeat: the first read got no answer, the second did"
    );
}

/// THE guard on blast radius, the same one left behind. The predicate that gates the money
/// submit retry must not have moved: a submit reset in flight may already have reached the chain,
/// and repeating it spends twice.
#[tokio::test]
async fn the_submit_predicate_is_not_widened() {
    let error = read_whose_connection_is_reset().await;
    assert!(
        !is_transient_transport_failure(&error),
        "is_transient_transport_failure gates the money submit retry and must not see this: \
         {error:#}"
    );
}

/// Where the buy path reads the fill, and where the settlement confirmation reads its events: both
/// are polled after money has moved, and both used to go out unwrapped. Frozen here so a later
/// edit cannot quietly take either back out of the policy.
#[test]
fn both_post_money_readers_go_through_the_read_policy() {
    let source = include_str!("client.rs");
    for (reader, next_item) in [
        (
            "pub async fn poll_inference_filled_tcs(",
            "pub(super) async fn seller_offer_events_since(",
        ),
        (
            "pub async fn token_contract_settlement_receipts(",
            "/// Read current getters when active and immutable ext-out history for one TokenContract.",
        ),
    ] {
        let start = source.find(reader).expect("reader present");
        let end = source[start..]
            .find(next_item)
            .map(|offset| start + offset)
            .expect("reader boundary present");
        let body = &source[start..end];
        assert!(
            body.contains("retry_transient_read"),
            "{reader} reads the chain after money moved and must go through the read policy"
        );
    }
}
