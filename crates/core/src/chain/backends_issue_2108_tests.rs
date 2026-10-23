use super::*;
use crate::chain::client::{
    fetch_ext_out_page, retry_transient_read, ChainRequestCeiling, RequestGate,
};
use crate::params::{EXT_OUT_PAGE_SIZE, TRANSIENT_READ_ATTEMPTS};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(start_paused = true)]
async fn issue_2108_exhausted_read_wrapper_timeout_is_recoverable_only_for_reads() {
    let error = retry_transient_read(std::future::pending::<Result<()>>)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("chain read exceeded"));
    assert!(!crate::chain::client::is_transient_transport_failure(
        &error
    ));
    let mapped = map_read_err(error);
    assert!(matches!(mapped, ChainError::Transport(_)), "{mapped}");
    let refusal = map_read_err(anyhow!("on-chain submit failed: exit_code=151"));
    assert!(matches!(refusal, ChainError::Contract(_)), "{refusal}");
}

#[tokio::test]
async fn issue_2108_exhausted_pool_reads_remain_recoverable_but_schema_errors_do_not() {
    for (message, transient) in [
        ("pool timed out while waiting for an open connection", true),
        ("Cannot query field messages", false),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let attempts = if transient {
            TRANSIENT_READ_ATTEMPTS
        } else {
            1
        };
        let server = tokio::spawn(async move {
            for _ in 0..attempts {
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
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                captured.fetch_add(1, Ordering::Relaxed);
                let body = json!({"data":{"blockchain":{"account":null}},
                    "errors":[{"message":message}]})
                .to_string();
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
                ).as_bytes()).await.unwrap();
            }
        });
        let gate = RequestGate::new(ChainRequestCeiling::Unlimited);
        let http = reqwest::Client::new();
        let account_id = "a".repeat(64);
        let error = retry_transient_read(|| {
            fetch_ext_out_page(
                &gate,
                &http,
                &endpoint,
                &account_id,
                crate::address::DEXDO_DAPP_ID,
                EXT_OUT_PAGE_SIZE,
                None,
            )
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), attempts);
        server.await.unwrap();
        assert!(
            !crate::chain::client::is_transient_transport_failure(&error),
            "the shared money-submit predicate must remain unchanged"
        );
        let mapped = map_read_err(error);
        assert_eq!(
            matches!(mapped, ChainError::Transport(_)),
            transient,
            "{mapped}"
        );
        assert!(mapped.to_string().contains(message));
        assert!(!mapped.to_string().contains("shape changed"));
    }
}
