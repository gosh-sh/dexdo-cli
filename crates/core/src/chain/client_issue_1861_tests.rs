//! one `502` on the receipt poll ended a money command AFTER the wallet had been spent.

//! Measured live on builds 271 and 272: `dexdo note deploy` submitted the multisig transfer, the
//! edge answered the following receipt read with `502 Bad Gateway`, and the run exited 1 with the
//! recovery file left at `submit_maybe_sent` -- a spend that had to be reconciled by hand. The
//! issue counted 34 of 120 probes answering `502` inside a 38-second window, so this is not rare.

//! The defect was never the retry policy's decision. `read_failure_is_transient` already accepts
//! any `is_server_error()` (`client.rs`), and a `502` is pinned as transient by
//! `issue_1185_single_predicate_classifies_all_transient_shapes`. What happened is that the receipt
//! read was outside the wrapper: both money-path observers called
//! `query_exact_destination_receipt` straight through `http.post`, while the same file routes every
//! other chain read through `retry_transient_read`.

//! These tests drive the two callers over a real socket, so the assertions run through the real
//! `reqwest` status handling, the real predicate and the real policy rather than a hand-made error.

use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Everything the scripted edge recorded: how many requests arrived and what each one asked for.
struct EdgeLog {
    requests: AtomicUsize,
    bodies: Mutex<Vec<String>>,
}

impl EdgeLog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: AtomicUsize::new(0),
            bodies: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("edge log").clone()
    }
}

/// Read one whole HTTP request, headers and declared body.

/// A single `read` is not enough and the difference is not cosmetic: `reqwest` can put the headers
/// and the JSON body in separate segments, and answering before the body has been taken off the
/// socket lets the close race the client's write. Draining to `Content-Length` removes the race.
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket
            .read(&mut chunk)
            .await
            .expect("read the receipt POST");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(head_end) = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|at| at + 4)
        else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let declared = head
            .lines()
            .find_map(|line| line.trim().strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() >= head_end + declared {
            break;
        }
    }
    String::from_utf8_lossy(&buffer).to_string()
}

async fn answer(socket: &mut tokio::net::TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("write the scripted edge response");
    let _ = socket.shutdown().await;
}

/// An endpoint that answers a scripted sequence, one response per connection.

/// `Connection: close` on every response is what makes the sequence deterministic: the client
/// cannot keep a connection alive and take two scripted answers out of order.
async fn serve_scripted_edge(
    responses: Vec<(&'static str, &'static str, String)>,
) -> (String, Arc<EdgeLog>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the scripted chain edge");
    let endpoint = format!("http://{}", listener.local_addr().expect("edge address"));
    let log = EdgeLog::new();
    let task_log = Arc::clone(&log);
    let task = tokio::spawn(async move {
        for (status, content_type, body) in responses {
            let (mut socket, _) = listener.accept().await.expect("accept a chain read");
            let request = read_request(&mut socket).await;
            task_log.requests.fetch_add(1, Ordering::SeqCst);
            task_log.bodies.lock().expect("edge log").push(request);
            answer(&mut socket, status, content_type, &body).await;
        }
    });
    (endpoint, log, task)
}

/// An endpoint that answers the same thing forever, for measuring where the policy stops.
async fn serve_unending_edge(
    status: &'static str,
    content_type: &'static str,
    body: &'static str,
) -> (String, Arc<EdgeLog>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the unending chain edge");
    let endpoint = format!("http://{}", listener.local_addr().expect("edge address"));
    let log = EdgeLog::new();
    let task_log = Arc::clone(&log);
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.expect("accept a chain read");
            let request = read_request(&mut socket).await;
            task_log.requests.fetch_add(1, Ordering::SeqCst);
            task_log.bodies.lock().expect("edge log").push(request);
            answer(&mut socket, status, content_type, body).await;
        }
    });
    (endpoint, log, task)
}

/// What Cloudflare puts in front of a `502`. The body is never parsed -- `error_for_status` decides
/// on the status alone -- but sending HTML keeps the fixture honest about what arrives.
const GATEWAY_HTML: &str = "<html><body><h1>502 Bad Gateway</h1></body></html>";

fn account() -> String {
    "a".repeat(64)
}

fn dapp() -> String {
    format!("{:0>64}", "4")
}

fn message_hash() -> String {
    "b".repeat(64)
}

fn transaction_hash() -> String {
    "c".repeat(64)
}

/// The finalized destination receipt, in the shape `parse_exact_destination_receipt` demands.
fn finalized_receipt_body() -> String {
    serde_json::json!({
        "data": {
            "blockchain": {
                "message": {
                    "id": message_hash(),
                    "dst": format!("0:{}", account()),
                    "dst_transaction": {
                        "id": transaction_hash(),
                        "status": 3,
                        "aborted": false,
                        "account_addr": format!("0:{}", account()),
                        "outmsg_cnt": 1,
                        "compute": { "exit_code": 0, "success": true },
                        "action": { "result_code": 0, "success": true, "no_funds": false },
                    },
                },
                "account": {
                    "info": {
                        "id": account(),
                        "dapp_id": dapp(),
                        "balance_other": [{ "currency": 2, "value": "1000" }],
                    },
                    "transactions": { "edges": [{ "node": { "id": transaction_hash() } }] },
                },
            }
        }
    })
    .to_string()
}

/// THE regression. Two `502`s, then the receipt -- and the observation the money path is waiting on
/// has to arrive. Before the fix the first `502` left `query_exact_destination_receipt` through `?`
/// and this poll returned `Err` on request one.
#[tokio::test]
async fn issue_1861_a_502_on_the_note_deploy_receipt_poll_is_survived() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) = serve_scripted_edge(vec![
        ("502 Bad Gateway", "text/html", GATEWAY_HTML.to_string()),
        ("502 Bad Gateway", "text/html", GATEWAY_HTML.to_string()),
        ("200 OK", "application/json", finalized_receipt_body()),
    ])
    .await;

    let observed = poll_finalized_destination_receipt(
        &client,
        &endpoint,
        &account(),
        &dapp(),
        &message_hash(),
    )
    .await
    .expect("a 502 is a read that got no answer, so the poll must repeat it and finish");

    task.await.expect("scripted edge task");
    let receipt = observed.expect("the third answer carries the finalized receipt");
    assert_eq!(receipt.transaction_hash, Some(transaction_hash()));
    assert_eq!(receipt.aborted, Some(false));
    assert_eq!(
        log.requests(),
        3,
        "two refused reads and the one that answered"
    );
    for body in log.bodies() {
        assert!(
            body.contains("dst_transaction"),
            "every repeat must be the same receipt read, not some other query"
        );
    }
}

fn event_id() -> String {
    "e".repeat(64)
}

fn delivery_id() -> String {
    "d".repeat(64)
}

/// Read 1 of `prove_multisig_delivery_message`: the emitting transaction and its out-messages.

/// Two out-messages, which is the shape the function's own doc describes -- the queued path performs
/// `txn.dest.transfer(..)` and emits `TransactionSent(..)` in ONE transaction -- so the sibling loop
/// below runs exactly once.
fn multisig_anchor_body() -> String {
    serde_json::json!({
        "data": { "blockchain": { "message": {
            "id": event_id(),
            "src_transaction": {
                "id": transaction_hash(),
                "out_msgs": [event_id(), delivery_id()],
            },
        } } }
    })
    .to_string()
}

/// Read 2: the sibling's destination, by exact hash.
fn multisig_sibling_body() -> String {
    serde_json::json!({
        "data": { "blockchain": { "message": {
            "id": delivery_id(),
            "dst": format!("0:{}", account()),
        } } }
    })
    .to_string()
}

/// Read 3: the delivery's own finalized destination receipt.
fn multisig_receipt_body() -> String {
    serde_json::json!({
        "data": {
            "blockchain": {
                "message": {
                    "id": delivery_id(),
                    "dst": format!("0:{}", account()),
                    "dst_transaction": {
                        "id": transaction_hash(),
                        "status": 3,
                        "aborted": false,
                        "account_addr": format!("0:{}", account()),
                        "outmsg_cnt": 1,
                        "compute": { "exit_code": 0, "success": true },
                        "action": { "result_code": 0, "success": true, "no_funds": false },
                    },
                },
                "account": {
                    "info": { "id": account(), "dapp_id": dapp(), "balance_other": [] },
                    "transactions": { "edges": [] },
                },
            }
        }
    })
    .to_string()
}

fn ok(body: String) -> (&'static str, &'static str, String) {
    ("200 OK", "application/json", body)
}

fn gateway_502() -> (&'static str, &'static str, String) {
    ("502 Bad Gateway", "text/html", GATEWAY_HTML.to_string())
}

/// The second caller named in, driven the same way: the multisig delivery proof reaches its
/// receipt read after two reads of its own, and a `502` there must not end it either.
#[tokio::test]
async fn issue_1861_a_502_on_the_multisig_delivery_receipt_is_survived() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) = serve_scripted_edge(vec![
        ok(multisig_anchor_body()),
        ok(multisig_sibling_body()),
        gateway_502(),
        ok(multisig_receipt_body()),
    ])
    .await;

    let proven =
        prove_multisig_delivery_message(&client, &endpoint, &event_id(), &account(), &dapp())
            .await
            .expect("a 502 on the delivery receipt must be repeated, not returned to the caller");

    task.await.expect("scripted edge task");
    assert_eq!(proven, Some(delivery_id()));
    assert_eq!(
        log.requests(),
        4,
        "anchor, sibling, refused receipt, receipt"
    );
}

/// Read 1 of the same proof, and the first one the money path reaches. Wrapping only the receipt
/// read would have left this `502` ending `prove_multisig_delivery_message` one step earlier, which
/// is exactly the half-truth this test exists to refuse.
#[tokio::test]
async fn issue_1861_a_502_on_the_multisig_anchor_read_is_survived() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) = serve_scripted_edge(vec![
        gateway_502(),
        ok(multisig_anchor_body()),
        ok(multisig_sibling_body()),
        ok(multisig_receipt_body()),
    ])
    .await;

    let proven =
        prove_multisig_delivery_message(&client, &endpoint, &event_id(), &account(), &dapp())
            .await
            .expect("a 502 on the anchor read must be repeated, not returned to the caller");

    task.await.expect("scripted edge task");
    assert_eq!(proven, Some(delivery_id()));
    assert_eq!(
        log.requests(),
        4,
        "refused anchor, anchor, sibling, receipt"
    );
}

/// Read 2, inside the loop over the anchor's out-messages. Same read function, different call site,
/// and the one whose worst case had to be counted before the retry went on it.
#[tokio::test]
async fn issue_1861_a_502_on_a_multisig_sibling_read_is_survived() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) = serve_scripted_edge(vec![
        ok(multisig_anchor_body()),
        gateway_502(),
        ok(multisig_sibling_body()),
        ok(multisig_receipt_body()),
    ])
    .await;

    let proven =
        prove_multisig_delivery_message(&client, &endpoint, &event_id(), &account(), &dapp())
            .await
            .expect("a 502 on a sibling read must be repeated, not returned to the caller");

    task.await.expect("scripted edge task");
    assert_eq!(proven, Some(delivery_id()));
    assert_eq!(
        log.requests(),
        4,
        "anchor, refused sibling, sibling, receipt"
    );
}

/// The negative control for the read the two tests above cover. `post_message_query` must still
/// hand a real refusal straight back: one request, no repeat, no exhaustion message.

/// It also fixes the loop's worst case at ONE exhausted read rather than one per sibling. A read
/// that runs out of attempts leaves `prove_multisig_delivery_message` through `?` on the sibling
/// where it happened, so the bound does not multiply by the number of out-messages.
#[tokio::test]
async fn issue_1861_a_permanent_refusal_on_the_anchor_still_ends_the_proof_at_once() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) = serve_scripted_edge(vec![(
        "400 Bad Request",
        "application/json",
        r#"{"error":"malformed query"}"#.to_string(),
    )])
    .await;

    let started = std::time::Instant::now();
    let error =
        prove_multisig_delivery_message(&client, &endpoint, &event_id(), &account(), &dapp())
            .await
            .expect_err("a 400 is a real answer and stays terminal");
    let elapsed = started.elapsed();

    task.await.expect("scripted edge task");
    assert_eq!(
        log.requests(),
        1,
        "a terminal refusal must not be asked again, and must not reach the sibling loop"
    );
    assert!(
        !format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX),
        "a terminal refusal must not be reported as an exhausted retry: {error:#}"
    );
    assert!(
        elapsed < crate::params::TRANSIENT_READ_INITIAL_BACKOFF,
        "a terminal refusal must return before the policy would even have waited once, took {elapsed:?}"
    );
}

/// The bound on the sibling loop, measured rather than argued.

/// An edge that answers `502` forever must stop the whole proof at `TRANSIENT_READ_ATTEMPTS`
/// requests -- not at that many PER out-message. The anchor read exhausts first and leaves through
/// `?`, so the count is the policy's ceiling and the sibling loop is never entered.
#[tokio::test]
async fn issue_1861_an_unending_502_does_not_multiply_across_the_sibling_loop() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) =
        serve_unending_edge("502 Bad Gateway", "text/html", GATEWAY_HTML).await;

    let started = std::time::Instant::now();
    let error =
        prove_multisig_delivery_message(&client, &endpoint, &event_id(), &account(), &dapp())
            .await
            .expect_err("an edge that never answers must be reported, not retried forever");
    let elapsed = started.elapsed();

    task.abort();
    assert_eq!(
        log.requests(),
        crate::params::TRANSIENT_READ_ATTEMPTS,
        "the proof spends one read's worth of attempts in total, not one per out-message"
    );
    assert!(
        format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX),
        "an exhausted read says so: {error:#}"
    );
    assert!(
        elapsed < crate::params::TRANSIENT_READ_TOTAL_BUDGET,
        "one read's budget bounds the whole proof, took {elapsed:?}"
    );
}

/// The negative control, and the reason it is not optional: a wrapper that repeated everything
/// would turn a deliberate refusal into a loop. A `400` is the server's answer, not the absence of
/// one, so it must end the poll on the spot -- one request, no repeat, no exhaustion message.
#[tokio::test]
async fn issue_1861_a_permanent_refusal_still_ends_the_poll_at_once() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) = serve_scripted_edge(vec![(
        "400 Bad Request",
        "application/json",
        r#"{"error":"malformed query"}"#.to_string(),
    )])
    .await;

    let started = std::time::Instant::now();
    let error = poll_finalized_destination_receipt(
        &client,
        &endpoint,
        &account(),
        &dapp(),
        &message_hash(),
    )
    .await
    .expect_err("a 400 is a real answer and stays terminal");
    let elapsed = started.elapsed();

    task.await.expect("scripted edge task");
    assert_eq!(
        log.requests(),
        1,
        "a terminal refusal must not be asked again"
    );
    assert!(
        !format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX),
        "a terminal refusal must not be reported as an exhausted retry: {error:#}"
    );
    assert!(
        elapsed < crate::params::TRANSIENT_READ_INITIAL_BACKOFF,
        "a terminal refusal must return before the policy would even have waited once, took {elapsed:?}"
    );
}

/// The bound. An edge that answers `502` forever must stop the read at the policy's ceiling and
/// report it as exhausted -- the count is `TRANSIENT_READ_ATTEMPTS`, and the poll loop around it
/// does NOT get to multiply that by its own twelve attempts.
#[tokio::test]
async fn issue_1861_an_unending_502_stops_at_the_policy_bound() {
    let client = chain_http_client().expect("chain http client");
    let (endpoint, log, task) =
        serve_unending_edge("502 Bad Gateway", "text/html", GATEWAY_HTML).await;

    let started = std::time::Instant::now();
    let error = poll_finalized_destination_receipt(
        &client,
        &endpoint,
        &account(),
        &dapp(),
        &message_hash(),
    )
    .await
    .expect_err("an edge that never answers must eventually be reported, not retried forever");
    let elapsed = started.elapsed();

    task.abort();
    assert_eq!(
        log.requests(),
        crate::params::TRANSIENT_READ_ATTEMPTS,
        "the read is bounded by TRANSIENT_READ_ATTEMPTS, and the twelve-attempt poll around it \
         must not restart the read once it is exhausted"
    );
    assert!(
        format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX),
        "an exhausted read says so: {error:#}"
    );
    assert!(
        elapsed < crate::params::TRANSIENT_READ_TOTAL_BUDGET,
        "the whole read is bounded by TRANSIENT_READ_TOTAL_BUDGET, took {elapsed:?}"
    );
}
