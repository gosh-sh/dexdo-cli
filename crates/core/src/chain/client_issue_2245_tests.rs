//! a contract call that fails is reported once, through the paths the client really takes;
//! one that succeeds, even after a repeat, reports nothing.
use super::super::call_errors::tests::{install, reported_for};
use super::super::call_errors::CallErrorType;
use super::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the node answers a request with: a status line and a JSON body, or nothing at all.
type Answer = Option<(&'static str, String)>;

/// A node on loopback answering each request by its path and body.
async fn node<F>(answer: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: Fn(&str, &str) -> Answer + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let answer = Arc::new(answer);
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            // A client that gave up on an attempt closes its connection unanswered.
            let Some((path, body)) = read_request(&mut socket).await else {
                continue;
            };
            match answer(&path, &body) {
                Some((status, body)) => {
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
                None => held.push(socket),
            }
        }
    });
    (endpoint, task)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = socket.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap().to_string();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap_or(0);
        if bytes.len() >= end + 4 + length {
            let path = headers
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let body = String::from_utf8_lossy(&bytes[end + 4..end + 4 + length]).to_string();
            return Some((path, body));
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn chain_time(at: u64) -> Answer {
    Some((
        "200 OK",
        json!({"data": {"blockchain": {"blocks": {"edges": [{"node": {"gen_utime": at}}]}}}})
            .to_string(),
    ))
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

/// An account of this test alone: no other test in the binary calls it.
fn account(tag: char) -> Address {
    Address::parse(&format!("0:2245{}", tag.to_string().repeat(60))).unwrap()
}

async fn signed_funding(note: &Address) -> String {
    RealChainBackend::encode_signed_call_boc(
        note,
        PRIVATENOTE_ABI,
        "fundDeployShell",
        note_fund_deploy_shell_params(7, 11),
        &KeyPair::from_secret_hex(&"22".repeat(32)).unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_failed_getter_reports_once_and_an_answered_one_reports_nothing() {
    install();
    let (answered, refused) = (account('a'), account('b'));
    let refused_bare = refused.bare().to_string();
    let (endpoint, server) = node(move |_, body| {
        Some(if body.contains(&refused_bare) {
            (
                "200 OK",
                json!({"errors": [{"message": "account query failed"}]}).to_string(),
            )
        } else {
            (
                "200 OK",
                json!({"data": {"blockchain": {"account": null}}}).to_string(),
            )
        })
    })
    .await;
    let chain = backend(&endpoint);
    assert!(chain
        .token_contract_state(&answered)
        .await
        .unwrap()
        .is_none());
    assert!(chain.token_contract_state(&refused).await.is_err());
    server.abort();
    assert!(reported_for(answered.bare()).is_empty());
    assert_eq!(
        reported_for(refused.bare()),
        [(
            "TokenContract",
            "getState".to_string(),
            CallErrorType::NodeRejected
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn a_read_that_runs_out_of_attempts_reports_once_by_its_last_failure() {
    install();
    let silent = account('c');
    let (endpoint, server) = node(|_, _| None).await;
    let error = backend(&endpoint)
        .private_note_details(&silent)
        .await
        .unwrap_err();
    server.abort();
    assert!(
        format!("{error:#}").contains(crate::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX),
        "{error:#}"
    );
    assert_eq!(
        reported_for(silent.bare()),
        [(
            "PrivateNote",
            "getDetails".to_string(),
            CallErrorType::Timeout
        )]
    );
}

#[tokio::test]
async fn a_refused_send_reports_its_contract_function_and_phase() {
    install();
    let note = account('d');
    let boc = signed_funding(&note).await;
    let (endpoint, server) = node(|path, _| {
        assert_eq!(path, "/v2/messages");
        Some((
            "200 OK",
            json!({"result": {"aborted": true, "exit_code": 151}}).to_string(),
        ))
    })
    .await;
    let result = backend(&endpoint)
        .retry_submit_preflighted(
            &boc,
            false,
            Some(crate::address::DEXDO_DAPP_ID),
            SubmitKind::Ordinary,
            &mut false,
        )
        .await;
    server.abort();
    assert!(result.is_err());
    assert_eq!(
        reported_for(note.bare()),
        [(
            "PrivateNote",
            "fundDeployShell".to_string(),
            CallErrorType::TvmCompute
        )]
    );
}

#[tokio::test]
async fn a_send_answered_after_a_repeat_reports_nothing() {
    install();
    let note = account('e');
    let boc = signed_funding(&note).await;
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = sent.clone();
    let (endpoint, server) = node(move |_, _| {
        Some(
            if counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                (
                    "200 OK",
                    json!({"error": {
                        "code": "INTERNAL_ERROR",
                        "message": "Block Producer returned non-success HTTP status 429 Too Many Requests: ",
                        "data": {"message_hash": "", "exit_code": null}
                    }})
                    .to_string(),
                )
            } else {
                ("200 OK", json!({"result": {"exit_code": 0}}).to_string())
            },
        )
    })
    .await;
    backend(&endpoint)
        .retry_submit_preflighted(
            &boc,
            false,
            Some(crate::address::DEXDO_DAPP_ID),
            SubmitKind::Ordinary,
            &mut false,
        )
        .await
        .unwrap();
    server.abort();
    assert_eq!(sent.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(reported_for(note.bare()).is_empty());
}

#[tokio::test]
async fn a_money_send_whose_outcome_is_unknown_is_unconfirmed() {
    install();
    let note = account('f');
    let boc = signed_funding(&note).await;
    let (endpoint, server) =
        node(|_, _| Some(("500 Internal Server Error", "{}".to_string()))).await;
    let error = retry_buyer_money_submit(
        &build_money_post_http_client().unwrap(),
        &endpoint,
        &boc,
        note.bare(),
        crate::address::DEXDO_DAPP_ID,
    )
    .await
    .unwrap_err();
    server.abort();
    assert!(error
        .downcast_ref::<MoneySubmitError>()
        .is_some_and(MoneySubmitError::is_ambiguous));
    assert_eq!(
        reported_for(note.bare()),
        [(
            "PrivateNote",
            "fundDeployShell".to_string(),
            CallErrorType::Unconfirmed
        )]
    );
}

#[tokio::test]
async fn a_clock_the_chain_disagrees_with_refuses_the_send_as_client() {
    install();
    let note = account('9');
    let boc = signed_funding(&note).await;
    let (endpoint, server) = node(|path, body| {
        assert!(
            path == "/graphql" && body.contains("blocks"),
            "nothing is sent past the clock check: {path} {body}"
        );
        chain_time(now() - 100_000)
    })
    .await;
    let error = backend(&endpoint)
        .retry_submit(
            &boc,
            false,
            Some(crate::address::DEXDO_DAPP_ID),
            SubmitKind::Ordinary,
        )
        .await
        .unwrap_err();
    server.abort();
    assert!(
        error.downcast_ref::<ClockSkewRefusal>().is_some(),
        "{error:#}"
    );
    assert_eq!(
        reported_for(note.bare()),
        [(
            "PrivateNote",
            "fundDeployShell".to_string(),
            CallErrorType::Client
        )]
    );
}
