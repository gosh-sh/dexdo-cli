const FORWARDING_TIMEOUT: &str =
    include_str!("fixtures/block_producer_forwarding_timeout_2064.json");

#[tokio::test]
async fn issue_2064_forwarding_timeout_posts_once_and_preserves_submit_evidence() {
    let account = "1".repeat(64);
    let client = build_money_post_http_client().unwrap();
    let (endpoint, posts, task) =
        serve_counted_money_post_response("200 OK", FORWARDING_TIMEOUT).await;
    let error = retry_buyer_money_submit(&client, &endpoint, "signed-boc", &account, &account)
        .await
        .expect_err("a forwarding timeout does not prove rejection");
    assert!(
        matches!(
            money_submit_stage(&error),
            MoneySubmitError::Ambiguous { .. }
        ),
        "{error:#}"
    );
    assert!(!money_submit_stage(&error).clears_journal());
    assert!(!is_decoded_transient_money_rejection(&error));
    assert!(format!("{error:#}").contains("redirection to the Block Producer"));
    task.await.unwrap();
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "unknown outcome must never resend"
    );
}

#[tokio::test]
async fn issue_2064_unrelated_decoded_errors_remain_rejections() {
    let account = "1".repeat(64);
    let client = build_money_post_http_client().unwrap();
    for body in [
        r#"{"error":{"code":"INTERNAL_ERROR","message":"operation timed out"}}"#,
        r#"{"error":{"code":"TVM_ERROR","message":"operation timed out","data":{"exit_code":151}}}"#,
        r#"{"result":{"exit_code":151}}"#,
    ] {
        let (endpoint, posts, task) = serve_counted_money_post_response("200 OK", body).await;
        let error = retry_buyer_money_submit(&client, &endpoint, "signed-boc", &account, &account)
            .await
            .unwrap_err();
        assert!(
            matches!(
                money_submit_stage(&error),
                MoneySubmitError::Rejected { .. }
            ),
            "{error:#}"
        );
        assert!(money_submit_stage(&error).clears_journal());
        task.await.unwrap();
        assert_eq!(posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn issue_2064_queue_overflow_keeps_controlled_buy_retry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let posts = Arc::new(AtomicUsize::new(0));
    let observed = posts.clone();
    let server = tokio::spawn(async move {
        for body in [
            r#"{"error":"QUEUE_OVERFLOW: message queue is full"}"#,
            r#"{"result":{"accepted":true}}"#,
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            assert!(request[..read].starts_with(b"POST /v2/messages "));
            observed.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let account = "1".repeat(64);
    let response = retry_buyer_money_submit(
        &build_money_post_http_client().unwrap(),
        &endpoint,
        "signed-boc",
        &account,
        &account,
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(response["result"]["accepted"], true);
    assert_eq!(posts.load(Ordering::SeqCst), 2);
}
