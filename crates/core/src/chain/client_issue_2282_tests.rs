use super::*;
use proptest::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CLOSED_POOL: &str = "attempted to acquire a connection on a closed pool";

fn errors(message: &str) -> Value {
    json!([{
        "message": message,
        "locations": [{"line": 6, "column": 15}],
        "path": ["blockchain", "account", "messages"]
    }])
}

#[test]
fn issue_2282_closed_pool_is_only_a_retryable_read() {
    for error in [
        anyhow::Error::new(GraphQlBodyError::from_errors(&errors(CLOSED_POOL))),
        anyhow!("GraphQL errors: {}", errors(CLOSED_POOL)),
    ] {
        let error = error.context("mainnet: account seller-note ext-out GraphQL errors");
        assert!(is_transient_read_failure(&error), "{error:#}");
        assert!(!is_transient_transport_failure(&error));
        assert!(!is_transient_submit_failure(&error));
    }
}

#[test]
fn issue_2282_permanent_errors_and_unstructured_closed_pool_text_are_not_retried() {
    for message in [
        "Unauthorized",
        "seller note owner mismatch",
        "Cannot query field 'messages' on type 'Account'",
        "on-chain revert: exit_code=151",
        "connection pool closed",
        &format!("operator description: {CLOSED_POOL}"),
        &format!("on-chain revert: {CLOSED_POOL}; exit_code=151"),
    ] {
        let error = anyhow::Error::new(GraphQlBodyError::from_errors(&errors(message)));
        assert!(!is_transient_read_failure(&error), "{error:#}");
    }
    for error in [
        anyhow!("{CLOSED_POOL}"),
        anyhow!("GraphQL errors: {CLOSED_POOL}"),
        anyhow!("GraphQL errors: {{broken JSON: {CLOSED_POOL}}}"),
    ] {
        assert!(!is_transient_read_failure(&error), "{error:#}");
    }
}

proptest! {
    #[test]
    fn issue_2282_read_context_never_enables_a_money_retry(
        contexts in prop::collection::vec("[a-z ]{1,32}", 0..5),
    ) {
        for mut error in [
            anyhow::Error::new(GraphQlBodyError::from_errors(&errors(CLOSED_POOL))),
            anyhow!("GraphQL errors: {}", errors(CLOSED_POOL)),
        ] {
            for context in &contexts {
                error = error.context(context.clone());
            }
            prop_assert!(is_transient_read_failure(&error));
            prop_assert!(!is_transient_transport_failure(&error));
            prop_assert!(!is_transient_submit_failure(&error));
        }
    }
}

#[tokio::test]
async fn issue_2282_ext_out_http_read_recovers_after_repeated_closed_pool_responses() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let account = "a".repeat(64);
    let expected_account = account.clone();
    let server = tokio::spawn(async move {
        for attempt in 0..3 {
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
                assert!(headers.starts_with("POST /graphql "));
                let query: Value =
                    serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                assert_eq!(query["variables"]["accountId"], expected_account);
                assert_eq!(query["variables"]["before"], Value::Null);
                assert!(!query["query"].as_str().unwrap().contains("mutation"));
                break;
            }
            let body = if attempt < 2 {
                json!({"data": null, "errors": errors(CLOSED_POOL)})
            } else {
                json!({"data":{"blockchain":{"account":{
                    "info":{"id":expected_account},
                    "messages":{"edges":[],"pageInfo":{"hasPreviousPage":false,"startCursor":null}}
                }}}})
            }
            .to_string();
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
        }
    });
    let messages = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fetch_all_ext_out_messages_routed(
            &RequestGate::new(ChainRequestCeiling::Unlimited),
            &reqwest::Client::new(),
            &endpoint,
            &account,
            crate::address::DEXDO_DAPP_ID,
            |message| Ok(Some(message)),
        ),
    )
    .await
    .unwrap()
    .expect("the same ext-out read recovers without a write");
    assert!(messages.is_empty());
    server.await.unwrap();
}
