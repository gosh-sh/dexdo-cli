//! Producer 429 replies from live_acceptance 7791/7792 arrive inside an HTTP-200
//! INTERNAL_ERROR envelope. Exercise the actual POST/retry paths over loopback,
//! including signed message identity and the one-shot/ambiguous money boundaries.

use super::*;
use proptest::prelude::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn producer_429() -> Value {
    json!({"error": {
        "code": "INTERNAL_ERROR",
        "message": "Block Producer returned non-success HTTP status 429 Too Many Requests: ",
        "data": {"message_hash": "", "exit_code": null}
    }})
}

type Posts = Arc<Mutex<Vec<Value>>>;

async fn scripted_posts(
    responses: Vec<(&'static str, String)>,
) -> (String, Posts, tokio::task::JoinHandle<()>) {
    assert!(!responses.is_empty());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let posts = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&posts);
    let task = tokio::spawn(async move {
        let mut index = 0;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_post(&mut socket).await;
            captured.lock().unwrap().push(request);
            let (status, body) = &responses[index.min(responses.len() - 1)];
            index += 1;
            write_response(&mut socket, status, body).await;
        }
    });
    (endpoint, posts, task)
}

async fn read_post(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0, "request closed before its complete body");
        bytes.extend_from_slice(&chunk[..count]);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
        assert!(headers.starts_with("POST /v2/messages HTTP/1.1"));
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .expect("POST content length");
        if bytes.len() >= end + 4 + length {
            return serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
        }
    }
}

async fn write_response(socket: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}

fn backend(endpoint: &str) -> RealChainBackend {
    let deployed: Deployed = serde_json::from_value(json!({
        "network": "fixture", "endpoint": endpoint,
        "superroot": format!("0:{}", "0".repeat(64)),
        "dapp_config": format!("0:{}", "0".repeat(64)),
        "dapp_id": crate::address::DEXDO_DAPP_ID,
    }))
    .unwrap();
    RealChainBackend {
        client: LimitedChainClient::new(
            ChainClient::connect(endpoint).unwrap(),
            ChainRequestCeiling::Unlimited,
        ),
        http: chain_http_client().unwrap(),
        money_post_http: build_money_post_http_client().unwrap(),
        superroot: Address::parse(&deployed.superroot).unwrap(),
        deployed,
    }
}

async fn signed_funding() -> String {
    RealChainBackend::encode_signed_call_boc(
        &Address::parse(&format!("0:{}", "1".repeat(64))).unwrap(),
        PRIVATENOTE_ABI,
        "fundDeployShell",
        note_fund_deploy_shell_params(7, 11),
        &KeyPair::from_secret_hex(&"22".repeat(32)).unwrap(),
    )
    .await
    .unwrap()
}

fn assert_same_message(posts: &Posts, boc: &str, count: usize) {
    let posts = posts.lock().unwrap();
    assert_eq!(posts.len(), count);
    let hash = external_message_hash(boc).unwrap();
    let account = dest_account_id_hex(boc).unwrap();
    for post in posts.iter() {
        assert_eq!(post.as_array().unwrap().len(), 1);
        assert_eq!(
            post[0]["body"], boc,
            "a retry must not re-sign or create another operation"
        );
        assert_eq!(
            external_message_hash(post[0]["body"].as_str().unwrap()).unwrap(),
            hash
        );
        assert_eq!(post[0]["account_id"], account);
        assert_eq!(post[0]["dapp_id"], crate::address::DEXDO_DAPP_ID);
    }
}

#[tokio::test]
async fn producer_429_retries_identical_signed_provision_message() {
    let boc = signed_funding().await;
    let (endpoint, posts, server) = scripted_posts(vec![
        ("200 OK", producer_429().to_string()),
        ("200 OK", producer_429().to_string()),
        ("200 OK", json!({"result": {"exit_code": 0}}).to_string()),
    ])
    .await;
    let mut possibly_dispatched = false;
    let result = backend(&endpoint)
        .retry_submit_preflighted(
            &boc,
            false,
            Some(crate::address::DEXDO_DAPP_ID),
            &mut possibly_dispatched,
        )
        .await;
    server.abort();
    result.expect("the producer's rate limit must back off and recover");
    assert_same_message(&posts, &boc, 3);
}

#[tokio::test]
async fn producer_429_buyer_keeps_the_recovery_journal_without_retry() {
    let boc = signed_funding().await;
    let account = dest_account_id_hex(&boc).unwrap();
    let (endpoint, posts, server) = scripted_posts(vec![
        ("200 OK", producer_429().to_string()),
        ("200 OK", json!({"result": {"exit_code": 0}}).to_string()),
    ])
    .await;
    let result = retry_buyer_money_submit(
        &build_money_post_http_client().unwrap(),
        &endpoint,
        &boc,
        &account,
        crate::address::DEXDO_DAPP_ID,
    )
    .await;
    server.abort();
    let error = result.expect_err("the manager's final refusal hides earlier forwarding outcomes");
    let stage = error.downcast_ref::<MoneySubmitError>().unwrap();
    assert!(stage.is_ambiguous());
    assert!(!stage.clears_journal());
    assert_same_message(&posts, &boc, 1);
}

#[tokio::test]
async fn manager_429_after_an_ambiguous_forward_preserves_buyer_recovery_without_a_second_post() {
    let boc = signed_funding().await;
    let account = dest_account_id_hex(&boc).unwrap();
    // The manager retries forwarding internally, exposing only the last response.
    // Its first 504 may hide an accepted BOC, even though later attempts get 429.
    // Upstream behavior: ackinacki/ackinacki@9768b6279107b60a76dce003767fffe165d2de8b,
    // message-router/src/process_ext_messages.rs:79-133,272-311.
    let (producer, producer_posts, producer_server) = scripted_posts(vec![
        (
            "504 Gateway Timeout",
            "forwarded response was lost".to_owned(),
        ),
        ("429 Too Many Requests", String::new()),
    ])
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let manager_posts: Posts = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&manager_posts);
    let manager_server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_post(&mut socket).await;
            captured.lock().unwrap().push(request.clone());
            let first_post = captured.lock().unwrap().len() == 1;
            let response = if first_post {
                let client = chain_http_client().unwrap();
                let mut last_body = String::new();
                for expected_status in [504, 429, 429] {
                    let forwarded = client
                        .post(format!("{producer}/v2/messages"))
                        .json(&request)
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(forwarded.status().as_u16(), expected_status);
                    last_body = forwarded.text().await.unwrap();
                }
                let mut refusal = producer_429();
                refusal["error"]["message"] = json!(format!(
                    "Block Producer returned non-success HTTP status 429 Too Many Requests: {last_body}"
                ));
                refusal
            } else {
                // A second client POST must not turn the unknown first outcome into
                // a conclusive replay rejection that clears the recovery journal.
                json!({"result": {"exit_code": 400}})
            };
            write_response(&mut socket, "200 OK", &response.to_string()).await;
        }
    });
    let result = retry_buyer_money_submit(
        &build_money_post_http_client().unwrap(),
        &endpoint,
        &boc,
        &account,
        crate::address::DEXDO_DAPP_ID,
    )
    .await;
    manager_server.abort();
    producer_server.abort();
    let error = result.expect_err("the manager's last 429 cannot confirm a money rejection");
    assert_same_message(&producer_posts, &boc, 3);
    assert_same_message(&manager_posts, &boc, 1);
    let stage = error.downcast_ref::<MoneySubmitError>().unwrap();
    assert!(stage.is_ambiguous());
    assert!(
        !stage.clears_journal(),
        "retain the original identity for reconciliation"
    );
}

#[tokio::test]
async fn producer_429_retries_stop_at_the_existing_attempt_budget() {
    let boc = signed_funding().await;
    let (endpoint, posts, server) =
        scripted_posts(vec![("200 OK", producer_429().to_string())]).await;
    let mut possibly_dispatched = false;
    let result = backend(&endpoint)
        .retry_submit_preflighted(
            &boc,
            false,
            Some(crate::address::DEXDO_DAPP_ID),
            &mut possibly_dispatched,
        )
        .await;
    server.abort();
    result.expect_err("persistent throttling must terminate");
    assert_same_message(
        &posts,
        &boc,
        crate::params::TRANSIENT_SUBMIT_RETRIES_BEFORE_FINAL as usize + 1,
    );
}

#[tokio::test]
async fn ambiguous_money_outcomes_never_retry_even_if_the_response_mentions_429() {
    let boc = signed_funding().await;
    let account = dest_account_id_hex(&boc).unwrap();
    for (status, body) in [
        ("429 Too Many Requests", producer_429().to_string()),
        ("200 OK", "not-json 429 Too Many Requests".to_owned()),
        ("200 OK", json!({"error": {"code": "INTERNAL_ERROR", "message": "The message redirection to the Block Producer has failed: (source: operation timed out)"}}).to_string()),
    ] {
        let (endpoint, posts, server) = scripted_posts(vec![(status, body)]).await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), retry_buyer_money_submit(
            &build_money_post_http_client().unwrap(), &endpoint, &boc, &account, crate::address::DEXDO_DAPP_ID,
        )).await;
        server.abort();
        let error = result.expect("ambiguous money outcome must return without backoff").expect_err("not a confirmed submit");
        let stage = error.chain().find_map(|cause| cause.downcast_ref::<MoneySubmitError>()).unwrap();
        assert!(stage.is_ambiguous(), "{error:#}");
        assert!(!stage.clears_journal(), "the recovery record must survive an unknown outcome");
        assert_same_message(&posts, &boc, 1);
    }
}

#[tokio::test]
async fn producer_429_does_not_add_a_retry_to_explicit_stop() {
    let boc = signed_funding().await;
    let account = dest_account_id_hex(&boc).unwrap();
    let (endpoint, posts, server) =
        scripted_posts(vec![("200 OK", producer_429().to_string())]).await;
    let result = send_explicit_stop_money_once(
        &build_money_post_http_client().unwrap(),
        &endpoint,
        &boc,
        &account,
        crate::address::DEXDO_DAPP_ID,
    )
    .await;
    server.abort();
    let error = result.expect_err("the one-shot STOP preserves the unknown outcome for its caller");
    let stage = error.downcast_ref::<MoneySubmitError>().unwrap();
    assert!(stage.is_ambiguous());
    assert!(!stage.clears_journal());
    assert_same_message(&posts, &boc, 1);
}

#[test]
fn an_ambiguous_money_stage_overrides_even_a_nested_producer_429() {
    let error = anyhow::Error::new(MoneySubmitError::Ambiguous {
        source: checked_submit_response(producer_429()).unwrap_err(),
    });
    assert!(!is_decoded_transient_money_rejection(&error));
    let stage = error.downcast_ref::<MoneySubmitError>().unwrap();
    assert!(!stage.clears_journal());
}

#[test]
fn only_the_typed_producer_rate_limit_refusal_is_transient() {
    let refusal = producer_429();
    assert!(is_transient_submit_failure(
        &checked_submit_response(refusal).unwrap_err()
    ));
    for refusal in [
        json!({"error": {"code": "TVM_ERROR", "message": producer_429()["error"]["message"]}}),
        json!({"error": {"code": "INTERNAL_ERROR", "message": "request failed: 429 Too Many Requests"}}),
        json!({"error": {"code": "INTERNAL_ERROR", "message": "Block Producer returned non-success HTTP status 4290 Too Many Requests: "}}),
        json!({"result": {"exit_code": 151}}),
    ] {
        assert!(!is_transient_submit_failure(
            &checked_submit_response(refusal).unwrap_err()
        ));
    }
    assert!(!is_transient_submit_failure(&anyhow!(
        "Block Producer returned non-success HTTP status 429 Too Many Requests: "
    )));
}

proptest! {
    #[test]
    fn other_producer_statuses_do_not_gain_money_retries(status in 400u16..600u16) {
        prop_assume!(status != 429);
        let error = checked_submit_response(json!({"error": {
            "code": "INTERNAL_ERROR",
            "message": format!("Block Producer returned non-success HTTP status {status} Too Many Requests: ")
        }})).unwrap_err();
        prop_assert!(!is_transient_submit_failure(&error));
    }
}
