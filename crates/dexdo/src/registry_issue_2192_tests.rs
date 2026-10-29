//! Provision loads this snapshot before its first deployment. Transient failures must not be cached.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn reader_with_replies(
    replies: Vec<(u16, Value)>,
) -> (
    tempfile::TempDir,
    ChainModelRegistryReader,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let task = tokio::spawn(async move {
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&chunk[..n]);
                let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                if request.len() < end + 4 + length {
                    continue;
                }
                let body: Value =
                    serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                assert!(String::from_utf8_lossy(&request).starts_with("POST /graphql "));
                assert!(body["query"]
                    .as_str()
                    .unwrap()
                    .contains("account(account_id:"));
                assert!(!body.to_string().contains("mutation"));
                break;
            }
            observed.fetch_add(1, Ordering::SeqCst);
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("manifest.json");
    let address = format!("0:{}", "1".repeat(64));
    std::fs::write(
        &manifest,
        json!({"network":"test", "endpoint":endpoint,
        "superroot":address, "dapp_config":"", "dapp_id":"0".repeat(64)})
        .to_string(),
    )
    .unwrap();
    let reader = ChainModelRegistryReader::from_manifest(&manifest, &address).unwrap();
    (dir, reader, count, task)
}

#[tokio::test]
async fn issue_2192_registry_snapshot_retries_502_before_caching_success() {
    let (_dir, reader, count, task) = reader_with_replies(vec![
        (502, json!({"error":"Bad Gateway"})),
        (
            200,
            json!({"data":{"blockchain":{"account":{"info":{
                "acc_type_name":"Active", "balance":"0x0", "balance_other":[], "boc":"fixture-boc"
            }}}}}),
        ),
    ])
    .await;
    reader
        .read_account_once()
        .await
        .expect("one 502 must not poison the provision snapshot");
    assert_eq!(reader.account_boc().await.unwrap(), "fixture-boc");
    task.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn issue_2192_registry_snapshot_stops_at_the_existing_retry_bound() {
    let (_dir, reader, count, task) = reader_with_replies(
        (0..dexdo_core::params::TRANSIENT_READ_ATTEMPTS)
            .map(|_| (502, json!({"error":"Bad Gateway"})))
            .collect(),
    )
    .await;
    let error = reader.read_account_once().await.unwrap_err();
    assert!(
        format!("{error:#}").contains(dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX),
        "{error:#}"
    );
    task.await.unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        dexdo_core::params::TRANSIENT_READ_ATTEMPTS
    );
    assert!(reader.read_account_once().await.is_err());
    assert_eq!(
        count.load(Ordering::SeqCst),
        dexdo_core::params::TRANSIENT_READ_ATTEMPTS,
        "exhaustion must not restart the read for each registry getter"
    );
}

#[tokio::test]
async fn issue_2192_registry_snapshot_does_not_retry_a_permanent_refusal() {
    let (_dir, reader, count, task) =
        reader_with_replies(vec![(400, json!({"error":"invalid query"}))]).await;
    let error = reader.read_account_once().await.unwrap_err();
    assert!(!format!("{error:#}").contains(dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX));
    task.await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[ignore = "read-only live registry evidence; DEXDO_MANIFEST may route through a fault-injecting proxy"]
async fn live_issue_2192_registry_snapshot() {
    let manifest = dexdo_core::params::manifest_path().expect("DEXDO_MANIFEST");
    let registry = default_registry_address(&manifest).unwrap();
    let reader = ChainModelRegistryReader::from_manifest(&manifest, &registry).unwrap();
    reader
        .read_account_once()
        .await
        .expect("read the live registry snapshot");
    let count = reader.declared_model_count().await.unwrap();
    assert!(count > 0, "the live registry must be populated");
    println!("live registry snapshot read successfully; declared model count={count}");
}
