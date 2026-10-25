mod issue_2162 {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    struct AccountServer {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for AccountServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    // The final response repeats, so exhaustion exercises the production retry budget.
    async fn account_server(replies: Vec<Option<(u16, String)>>) -> AccountServer {
        account_server_with_retry_after(replies, None).await
    }

    async fn account_server_with_retry_after(
        replies: Vec<Option<(u16, String)>>,
        retry_after: Option<&str>,
    ) -> AccountServer {
        let retry_after = retry_after.map(str::to_owned);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            let mut replies = VecDeque::from(replies);
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut buffer = [0; 4096];
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                let reply = if replies.len() > 1 {
                    replies.pop_front().unwrap()
                } else {
                    replies.front().unwrap().clone()
                };
                if let Some((status, body)) = reply {
                    let header = if status == 429 || status == 503 {
                        retry_after
                            .as_ref()
                            .map(|value| format!("Retry-After: {value}\r\n"))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let response = format!(
                        "HTTP/1.1 {status} response\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            }
        });
        AccountServer {
            endpoint,
            requests,
            task,
        }
    }

    fn owner() -> String {
        format!("0:{}", "a".repeat(64))
    }

    fn tc(digit: char) -> String {
        format!("0:{}", digit.to_string().repeat(64))
    }

    fn found(digit: char) -> Option<(u16, String)> {
        Some((
            200,
            json!({
                "account_id": digit.to_string().repeat(64),
                "dapp_id": digit.to_string().repeat(64),
                "boc": "existing-account-snapshot",
                "state_timestamp": 1,
            })
            .to_string(),
        ))
    }

    fn contracts(root: &std::path::Path, endpoint: &str) -> std::path::PathBuf {
        let path = root.join("contracts.json");
        std::fs::write(
            &path,
            json!({
                "network": "net-a", "endpoint": endpoint,
                "superroot": tc('c'), "dapp_config": "",
                "dapp_id": dexdo_core::DEXDO_DAPP_ID,
            })
            .to_string(),
        )
        .unwrap();
        path
    }

    fn pending(root: &std::path::Path, digit: char) -> std::path::PathBuf {
        pending_contract(root, &tc(digit), "net-a")
    }

    fn pending_contract(
        root: &std::path::Path,
        token_contract: &str,
        network: &str,
    ) -> std::path::PathBuf {
        persist_provision_deal_handle(
            Some(root),
            &dexdo_core::MarketManifest {
                network: network.to_string(),
                frame_model: "test-model".to_string(),
                model_hash: dexdo_core::model_hash_for("test-model"),
                inference_order_book: tc('d'),
                root_model: tc('e'),
                token_contract: token_contract.to_string(),
                seller_note: owner(),
                nonce: 1,
                price_per_tick: dexdo_core::PRICE_STEP,
                max_ticks: 2,
            },
            &root.join("contracts.json"),
            true,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn found_and_exact_missing_accounts_clear_every_pending_file() {
        for missing in [
            "Can't find account in shard state",
            "Original error: Can't find account in shard state\n",
        ] {
            let server = account_server(vec![found('1'), Some((404, missing.to_string()))]).await;
            let root = tempfile::tempdir().unwrap();
            let manifest = contracts(root.path(), &server.endpoint);
            let first = pending(root.path(), '1');
            let second = pending(root.path(), '2');
            // Keep scan order explicit even on file systems with arbitrary directory order.
            let mut handle = deals::load_deal_handle(&second).unwrap();
            handle.created_at_unix += 1;
            deals::save_deal_handle(root.path(), &handle).unwrap();
            reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest)
                .await
                .unwrap();
            for path in [first, second] {
                assert!(!deals::load_deal_record(&path).unwrap().provision_pending);
            }
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for (request, digit) in requests.iter().zip(['1', '2']) {
                let account = digit.to_string().repeat(64);
                assert!(request.starts_with(&format!(
                    "GET /v2/account?account_id={account}&dapp_id={account} "
                )));
            }
        }
    }

    #[tokio::test]
    async fn unrelated_404_retries_then_leaves_all_files_unchanged() {
        let server = account_server(vec![
            found('1'),
            Some((404, "proxy route missing".to_string())),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let manifest = contracts(root.path(), &server.endpoint);
        let first = pending(root.path(), '1');
        let second = pending(root.path(), '2');
        let mut handle = deals::load_deal_handle(&second).unwrap();
        handle.created_at_unix += 1;
        deals::save_deal_handle(root.path(), &handle).unwrap();
        let before = [
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap(),
        ];
        let error = reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest)
            .await
            .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains(second.to_str().unwrap()), "{rendered}");
        assert!(rendered.contains(&"2".repeat(64)), "{rendered}");
        assert!(rendered.contains("proxy route missing"), "{rendered}");
        assert_eq!(
            server.requests.lock().unwrap().len(),
            1 + dexdo_core::params::TRANSIENT_READ_ATTEMPTS
        );
        assert_eq!(
            [
                std::fs::read(first).unwrap(),
                std::fs::read(second).unwrap()
            ],
            before
        );
    }

    #[tokio::test]
    async fn network_failure_retries_and_keeps_the_reservation() {
        let server = account_server(vec![None]).await;
        let root = tempfile::tempdir().unwrap();
        let manifest = contracts(root.path(), &server.endpoint);
        let path = pending(root.path(), '1');
        let before = std::fs::read(&path).unwrap();
        let error = reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest)
            .await
            .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains(path.to_str().unwrap()), "{rendered}");
        assert!(rendered.contains(&"1".repeat(64)), "{rendered}");
        assert!(rendered.contains("error sending request"), "{rendered}");
        assert_eq!(
            server.requests.lock().unwrap().len(),
            dexdo_core::params::TRANSIENT_READ_ATTEMPTS
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[tokio::test]
    async fn transient_response_recovers_without_leaving_pending() {
        for reply in [
            Some((500, "temporary backend failure".to_string())),
            Some((
                404,
                "Original error: Can't find account in shard state; try later".to_string(),
            )),
            Some((
                500,
                "Original error: Can't find account in shard state".to_string(),
            )),
            Some((200, "not an account".to_string())),
            Some((200, json!({"account_id": "1".repeat(64)}).to_string())),
            found('2'),
            None,
        ] {
            let server = account_server(vec![reply, found('1')]).await;
            let root = tempfile::tempdir().unwrap();
            let manifest = contracts(root.path(), &server.endpoint);
            let path = pending(root.path(), '1');
            reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest)
                .await
                .unwrap();
            assert_eq!(server.requests.lock().unwrap().len(), 2);
            assert!(!deals::load_deal_record(&path).unwrap().provision_pending);
        }
    }

    #[tokio::test]
    async fn other_notes_buyers_and_nonpending_files_are_untouched() {
        let server = account_server(vec![found('1')]).await;
        let root = tempfile::tempdir().unwrap();
        let manifest = contracts(root.path(), &server.endpoint);
        let selected = pending(root.path(), '1');
        let foreign = pending(root.path(), '2');
        let mut handle = deals::load_deal_handle(&foreign).unwrap();
        handle.note_addr = tc('b');
        handle.market.as_mut().unwrap().seller_note = tc('b');
        deals::save_deal_handle(root.path(), &handle).unwrap();
        let not_pending = pending(root.path(), '3');
        deals::persist_provision_pending(&not_pending, false).unwrap();
        let buyer_seller = pending(root.path(), '4');
        let mut handle = deals::load_deal_handle(&buyer_seller).unwrap();
        std::fs::remove_file(&buyer_seller).unwrap();
        handle.role = deals::DealHandleRole::Buyer;
        handle.handle = deals::make_handle_id(&handle.token_contract, handle.role);
        let buyer = deals::save_deal_handle(root.path(), &handle).unwrap();
        deals::persist_provision_pending(&buyer, true).unwrap();
        let untouched = [foreign, not_pending, buyer];
        let before = untouched
            .each_ref()
            .map(|path| std::fs::read(path).unwrap());
        reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest)
            .await
            .unwrap();
        assert!(
            !deals::load_deal_record(&selected)
                .unwrap()
                .provision_pending
        );
        assert_eq!(
            untouched
                .each_ref()
                .map(|path| std::fs::read(path).unwrap()),
            before
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_legacy_files_are_all_reconciled() {
        let server = account_server(vec![found('1')]).await;
        let root = tempfile::tempdir().unwrap();
        let manifest = contracts(root.path(), &server.endpoint);
        let canonical = pending(root.path(), '1');
        let legacy = root.path().join("deal-legacy-seller.json");
        std::fs::copy(&canonical, &legacy).unwrap();
        reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest)
            .await
            .unwrap();
        assert!(
            !deals::load_deal_record(&canonical)
                .unwrap()
                .provision_pending
        );
        assert!(!deals::load_deal_record(&legacy).unwrap().provision_pending);
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    fn startup_args(root: &std::path::Path) -> SellerArgs {
        std::fs::write(root.join("policy.json"), json!({
            "version": 1, "seller": {
                "on": {"after_deal_done": "retire", "buyer_no_show": "retire_gateway", "dispute_against_me": "hold"},
                "max_open_deals": 1,
            }
        }).to_string()).unwrap();
        let mut args = parsed_seller_args(&["--allow-private-advertise"]);
        args.identity.note_addr = Some(owner());
        args.token_contract = Some(tc('1'));
        args.deals_dir = Some(root.to_path_buf());
        args.models = root.join("missing-models.json");
        args.policy = Some(root.join("policy.json"));
        args
    }

    #[tokio::test]
    async fn startup_processes_all_pending_before_later_setup() {
        for success in [true, false] {
            let last = if success {
                found('2')
            } else {
                Some((404, "startup unavailable".to_string()))
            };
            let server = account_server(vec![found('1'), last]).await;
            let root = tempfile::tempdir().unwrap();
            let manifest = contracts(root.path(), &server.endpoint);
            let _manifest = crate::cli::commands::manifest_for_this_thread(manifest);
            let first = pending(root.path(), '1');
            let second = pending(root.path(), '2');
            let mut handle = deals::load_deal_handle(&second).unwrap();
            handle.created_at_unix += 1;
            deals::save_deal_handle(root.path(), &handle).unwrap();
            let before = [
                std::fs::read(&first).unwrap(),
                std::fs::read(&second).unwrap(),
            ];
            let args = startup_args(root.path());
            let error = run_seller(args).await.unwrap_err();
            let rendered = format!("{error:#}");
            if success {
                assert!(rendered.contains("missing-models.json"), "{rendered}");
                for path in [first, second] {
                    assert!(!deals::load_deal_record(&path).unwrap().provision_pending);
                }
            } else {
                assert!(
                    rendered.contains("reconcile pending seller deal file"),
                    "{rendered}"
                );
                assert!(rendered.contains("startup unavailable"), "{rendered}");
                assert_eq!(
                    [
                        std::fs::read(first).unwrap(),
                        std::fs::read(second).unwrap()
                    ],
                    before
                );
            }
            assert_eq!(
                server.requests.lock().unwrap().len(),
                if success {
                    2
                } else {
                    1 + dexdo_core::params::TRANSIENT_READ_ATTEMPTS
                }
            );
        }
    }

    #[tokio::test]
    async fn review_regression_honors_retry_after() {
        for status in [429, 503] {
            let server = account_server_with_retry_after(
                vec![Some((status, "busy".to_string())), found('1')],
                Some("2"),
            )
            .await;
            let root = tempfile::tempdir().unwrap();
            let manifest = contracts(root.path(), &server.endpoint);
            let path = pending(root.path(), '1');
            let started = tokio::time::Instant::now();
            reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest)
                .await
                .unwrap();
            assert!(
                started.elapsed() >= std::time::Duration::from_secs(2),
                "HTTP {status}: Retry-After: 2 ignored; completed after {:?}",
                started.elapsed()
            );
            assert!(!deals::load_deal_record(&path).unwrap().provision_pending);
            assert_eq!(server.requests.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn invalid_or_excessive_retry_after_keeps_existing_policy() {
        let excessive =
            (dexdo_core::params::TRANSIENT_READ_MAX_RETRY_AFTER.as_secs() + 1).to_string();
        for header in ["invalid", excessive.as_str()] {
            let server = account_server_with_retry_after(
                vec![Some((429, "busy".to_string())), found('1')],
                Some(header),
            )
            .await;
            let root = tempfile::tempdir().unwrap();
            let manifest = contracts(root.path(), &server.endpoint);
            let path = pending(root.path(), '1');
            tokio::time::timeout(
                dexdo_core::params::TRANSIENT_READ_ATTEMPT_TIMEOUT,
                reconcile_seller_pending_provisions(Some(root.path()), &owner(), &manifest),
            )
            .await
            .expect("invalid or excessive delay uses the existing fallback")
            .unwrap();
            assert!(!deals::load_deal_record(&path).unwrap().provision_pending);
            assert_eq!(server.requests.lock().unwrap().len(), 2);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "subprocess entry for the pending startup shutdown regression"]
    async fn pending_shutdown_child() {
        let root =
            std::path::PathBuf::from(std::env::var_os("DEXDO_TEST_2162_SHUTDOWN_ROOT").unwrap());
        let _manifest = crate::cli::commands::manifest_for_this_thread(root.join("contracts.json"));
        let error = run_seller(startup_args(&root)).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("missing-models.json"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn review_regression_shutdown_interrupts_pending_read() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if matches!(self.0.try_wait(), Ok(None)) {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let server = account_server(vec![
                found('1'),
                Some((503, "temporarily unavailable".to_string())),
            ])
            .await;
            let root = tempfile::tempdir().unwrap();
            contracts(root.path(), &server.endpoint);
            let first = pending(root.path(), '1');
            let second = pending(root.path(), '2');
            let mut handle = deals::load_deal_handle(&second).unwrap();
            handle.created_at_unix += 1;
            deals::save_deal_handle(root.path(), &handle).unwrap();
            let before = [
                std::fs::read(&first).unwrap(),
                std::fs::read(&second).unwrap(),
            ];
            let mut child = ChildGuard(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "cli::seller::tests::issue_2162::pending_shutdown_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("DEXDO_TEST_2162_SHUTDOWN_ROOT", root.path())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            tokio::time::timeout(dexdo_core::params::TRANSIENT_READ_ATTEMPT_TIMEOUT, async {
                while server.requests.lock().unwrap().len() < 2 {
                    assert!(
                        matches!(child.0.try_wait(), Ok(None)),
                        "child exited before the second account read"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the child reaches the pending read after a successful read");
            assert!(matches!(child.0.try_wait(), Ok(None)));
            // The PID is the exact child test executable spawned above, never an agent process.
            assert_eq!(unsafe { libc::kill(child.0.id() as i32, signal) }, 0);
            let status = tokio::time::timeout(
                dexdo_core::params::TRANSIENT_READ_INITIAL_BACKOFF * 4,
                async {
                    loop {
                        if let Some(status) = child.0.try_wait().unwrap() {
                            break status;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                },
            )
            .await
            .expect("shutdown must interrupt the pending reconciliation read");
            let mut output = String::new();
            std::io::Read::read_to_string(&mut child.0.stdout.take().unwrap(), &mut output)
                .unwrap();
            std::io::Read::read_to_string(&mut child.0.stderr.take().unwrap(), &mut output)
                .unwrap();
            assert!(status.success(), "child failed: {status}; {output}");
            assert_eq!(
                [
                    std::fs::read(first).unwrap(),
                    std::fs::read(second).unwrap()
                ],
                before
            );
            assert!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.starts_with("GET /v2/account?")),
                "shutdown must not introduce a submit"
            );
        }
    }

    #[tokio::test]
    #[ignore = "read-only shellnet startup with durable pending files"]
    async fn live_shellnet_startup_reconciles_pending_files() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../manifest/shellnet.manifest.json");
        let deployed = dexdo_core::chain::Deployed::load(&manifest).unwrap();
        assert_eq!(deployed.network, "shellnet");
        let root = tempfile::tempdir().unwrap();
        std::fs::copy(&manifest, root.path().join("contracts.json")).unwrap();
        let _manifest =
            crate::cli::commands::manifest_for_this_thread(root.path().join("contracts.json"));
        let missing = "0:6ffb43777f90a80346515d11bfa5dedb60b90b31f3990b689a0ef569f961a8f4";
        let files = [
            pending_contract(root.path(), &deployed.superroot, &deployed.network),
            pending_contract(root.path(), missing, &deployed.network),
        ];
        for path in &files {
            let record = deals::load_deal_record(path).unwrap();
            assert!(record.provision_pending);
            println!(
                "before file={} contract={} provision_pending=true",
                path.display(),
                record.handle.token_contract
            );
        }
        let mut args = startup_args(root.path());
        args.token_contract = Some(missing.to_string());
        let error = run_seller(args).await.unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("missing-models.json"), "{rendered}");
        for path in &files {
            let record = deals::load_deal_record(path).unwrap();
            assert!(!record.provision_pending);
            println!(
                "after file={} contract={} provision_pending=false",
                path.display(),
                record.handle.token_contract
            );
        }
        println!("startup terminal: reconciliation completed; later model setup refused the deliberately absent models file; pool/gateway/provision not started; {rendered}");
    }

    #[tokio::test]
    #[ignore = "read-only shellnet account lookups"]
    async fn live_shellnet_account_outcomes() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../manifest/shellnet.manifest.json");
        let deployed = dexdo_core::chain::Deployed::load(&manifest).unwrap();
        let chain = dexdo_core::RealChainBackend::connect(&manifest).unwrap();
        let present = dexdo_core::address::parse_chain_address(&deployed.superroot)
            .unwrap()
            .into_chain();
        assert!(chain.provision_account_exists(&present).await.unwrap());
        for account in [
            "6ffb43777f90a80346515d11bfa5dedb60b90b31f3990b689a0ef569f961a8f4",
            "1234123412341234123412341234123412341234123412341234123412341234",
        ] {
            let address = dexdo_core::Address::parse(&format!("0:{account}")).unwrap();
            assert!(!chain.provision_account_exists(&address).await.unwrap());
        }
        println!("read-only shellnet: existing account found; incident and never-deployed accounts absent");
    }
}
