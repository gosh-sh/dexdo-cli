use super::*;
use proptest::prelude::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn bm_refused(account: &str) -> String {
    format!("failed to get account {account}: error sending request to http://block_manager:8600/v2/account?account_id={account}&dapp_id=4: client error (Connect): tcp connect error: Connection refused (os error 111)")
}

fn sdk_error(message: &str) -> anyhow::Error {
    anyhow!("GraphQL errors: {}", json!([{"message": message}]))
}

#[test]
fn issue_2122_graphql_bm_refusal_is_only_a_repeatable_read() {
    let message = bm_refused(&"a".repeat(64));
    for error in [
        sdk_error(&message),
        anyhow!("GraphQL errors: {message}"),
        anyhow::Error::new(GraphQlBodyError::from_errors(
            &json!([{"message": message}]),
        )),
    ] {
        assert!(is_transient_read_failure(&error), "{error:#}");
        assert!(!is_transient_transport_failure(&error));
        assert!(!is_transient_submit_failure(&error));
    }
}

#[test]
fn issue_2122_schema_contract_and_unstructured_prose_stay_permanent() {
    for error in [
        sdk_error("Cannot query field 'info' on type 'Account'"),
        sdk_error("on-chain revert: exit_code=151"),
        sdk_error("Connection refused"),
        sdk_error("failed to get account a: invalid account_id"),
        sdk_error("failed to get account a: error sending request to http://block_manager:8600/v2/messages: client error (Connect): tcp connect error: Connection refused (os error 111)"),
        anyhow!("operator description: {}", bm_refused("a")),
        anyhow!("GraphQL errors: {{broken JSON: {}}}", bm_refused("a")),
    ] {
        assert!(!is_transient_read_failure(&error), "{error:#}");
        assert!(!is_transient_submit_failure(&error));
    }
}

proptest! {
    #[test]
    fn issue_2122_read_classification_never_authorizes_a_money_retry(account in "[0-9a-f]{64}") {
        let error = sdk_error(&bm_refused(&account));
        prop_assert!(is_transient_read_failure(&error));
        prop_assert!(!is_transient_submit_failure(&error));
    }
}

// Drive the pinned SDK, which flattens errors[] into a string, through a real HTTP response.
#[tokio::test]
async fn issue_2122_sdk_account_read_recovers_without_submitting_anything() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let account = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let wanted = account.bare().to_string();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
                let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..end]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse().ok())
                    })
                    .unwrap();
                if request.len() < end + 4 + length {
                    continue;
                }
                let query: Value =
                    serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                let query = query["query"].as_str().unwrap();
                assert!(query.contains("account(account_id:"), "{query}");
                assert!(query.contains(&wanted), "{query}");
                assert!(!query.contains("mutation"));
                break;
            }
            observed.fetch_add(1, Ordering::SeqCst);
            let body = if attempt == 0 {
                json!({"data": null, "errors": [{"message": bm_refused(&wanted)}]})
            } else {
                json!({"data":{"blockchain":{"account":{"info": {
                    "acc_type_name":"Active", "balance":"0x0", "balance_other":[]
                }}}}})
            }
            .to_string();
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
        }
    });
    let client = LimitedChainClient::new(
        ChainClient::connect(&endpoint).unwrap(),
        ChainRequestCeiling::Unlimited,
    );
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_account_retrying(&account),
    )
    .await
    .unwrap();
    assert!(read
        .expect("the same account read must recover")
        .unwrap()
        .is_active());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.await.unwrap();
}
