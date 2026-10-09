//! Regression coverage for unconfirmed RootPN deployment, independent of wallet spending.
use super::*;

#[test]
fn duplicate_without_receipt_is_not_an_internal_invariant_failure() {
    let error = note_deploy_rootpn_action_result(
        "deployPrivateNote",
        Some(anyhow::anyhow!(
            "block manager rejected message [DUPLICATE_MESSAGE]: Unknown error"
        )),
        None,
    )
    .expect_err("an absent receipt cannot prove deployment");
    let code = crate::cli::machine::classify_error(crate::cli::machine::OP_NOTE_DEPLOY, &error);
    assert_eq!(code.as_str(), "NOTE_DEPLOY_UNCONFIRMED");
    assert!(code.retryable(), "resuming the same paid recovery is safe");
    assert!(format!("{error:#}").contains("DUPLICATE_MESSAGE"));
    assert!(!note_deploy_has_exact_finalized_rootpn_exit_code(
        &error, 403
    ));
}

use dexdo_core::chain::NoteDeployRootPnActionObservation;
use dexdo_core::params::{NOTE_DEPLOY_ACTIVE_POLL_INTERVAL, NOTE_DEPLOY_ACTIVE_TIMEOUT};
use std::cell::Cell;

fn expected_note() -> String {
    format!("{}::{}", "0".repeat(63) + "4", "a".repeat(64))
}
fn submission() -> crate::cli::note::NoteDeploySubmission {
    crate::cli::note::NoteDeploySubmission {
        message_hash: "b".repeat(64),
        root_pn_address: format!("{}::{}", "0".repeat(63) + "4", "10".repeat(32)),
        expected_note_address: expected_note(),
        time_ms: 0,
        expire_at_unix: 1,
        last_submit_error: Some(
            "block manager rejected message [DUPLICATE_MESSAGE]: Unknown error".into(),
        ),
    }
}
fn receipt(exit: i64) -> NoteDeployRootPnActionObservation {
    NoteDeployRootPnActionObservation {
        transaction_hash: "c".repeat(64),
        compute_exit_code: exit,
        aborted: exit != 0,
        action_result_code: Some(0),
    }
}

#[tokio::test(start_paused = true)]
async fn a_delayed_note_completes_even_without_a_receipt() {
    let active_reads = Cell::new(0);
    let start = tokio::time::Instant::now();
    note_deploy_wait_confirmation_with(
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&submission()),
        async || Ok(None),
        async || {
            active_reads.set(active_reads.get() + 1);
            Ok(active_reads.get() >= 3)
        },
    )
    .await
    .expect("a later account activation completes the same submit");
    assert_eq!(active_reads.get(), 3);
    assert_eq!(start.elapsed(), NOTE_DEPLOY_ACTIVE_POLL_INTERVAL * 2);
}

#[tokio::test(start_paused = true)]
async fn an_active_note_does_not_wait_for_an_unavailable_receipt_reader() {
    let start = tokio::time::Instant::now();
    note_deploy_wait_confirmation_with(
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&submission()),
        async || {
            std::future::pending::<anyhow::Result<Option<NoteDeployRootPnActionObservation>>>()
                .await
        },
        async || {
            tokio::time::sleep(NOTE_DEPLOY_ACTIVE_POLL_INTERVAL).await;
            Ok(true)
        },
    )
    .await
    .expect("confirmed note must win over a stalled receipt read");
    assert_eq!(start.elapsed(), NOTE_DEPLOY_ACTIVE_POLL_INTERVAL);
}

#[tokio::test(start_paused = true)]
async fn absent_receipt_and_note_share_one_canonical_wait_budget() {
    let reads = Cell::new(0u64);
    let start = tokio::time::Instant::now();
    let saved = submission();
    let error = note_deploy_wait_confirmation_with(
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&saved),
        async || {
            reads.set(reads.get() + 1);
            Ok(None)
        },
        async || Ok(false),
    )
    .await
    .expect_err("absence cannot be relabeled as success");
    assert_eq!(start.elapsed(), NOTE_DEPLOY_ACTIVE_TIMEOUT);
    assert_eq!(
        reads.get(),
        NOTE_DEPLOY_ACTIVE_TIMEOUT.as_secs() / NOTE_DEPLOY_ACTIVE_POLL_INTERVAL.as_secs()
    );
    assert!(
        error
            .downcast_ref::<NoteDeployUnconfirmed>()
            .unwrap()
            .may_resubmit
    );
    for value in [
        &saved.message_hash,
        &saved.expected_note_address,
        "DUPLICATE_MESSAGE",
        "recovery.json",
    ] {
        assert!(
            error.to_string().contains(value),
            "missing public recovery diagnostic"
        );
    }
    assert!(!note_deploy_has_exact_finalized_rootpn_exit_code(
        &error, 403
    ));
}

#[tokio::test(start_paused = true)]
async fn a_hanging_read_is_cancelled_at_the_same_deadline() {
    let start = tokio::time::Instant::now();
    let error = note_deploy_wait_confirmation_with(
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&submission()),
        async || {
            std::future::pending::<anyhow::Result<Option<NoteDeployRootPnActionObservation>>>()
                .await
        },
        async || Ok(false),
    )
    .await
    .expect_err("reads cannot extend the confirmation budget");
    assert_eq!(start.elapsed(), NOTE_DEPLOY_ACTIVE_TIMEOUT);
    assert!(
        !error
            .downcast_ref::<NoteDeployUnconfirmed>()
            .unwrap()
            .may_resubmit
    );
}

#[tokio::test(start_paused = true)]
async fn finalized_403_requires_a_successful_absence_read() {
    let error = note_deploy_wait_confirmation_with(
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&submission()),
        async || Ok(Some(receipt(403))),
        async || Ok(false),
    )
    .await
    .expect_err("exact finalized rejection must reach the existing reproof guard");
    assert!(note_deploy_has_exact_finalized_rootpn_exit_code(
        &error, 403
    ));
    let error = note_deploy_wait_confirmation_with(
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&submission()),
        async || Ok(Some(receipt(403))),
        async || anyhow::bail!("note observer unavailable"),
    )
    .await
    .expect_err("unknown effect must not authorize reproof");
    assert!(!note_deploy_has_exact_finalized_rootpn_exit_code(
        &error, 403
    ));
    assert!(
        !error
            .downcast_ref::<NoteDeployUnconfirmed>()
            .unwrap()
            .may_resubmit
    );
}

#[tokio::test(start_paused = true)]
async fn a_successful_root_receipt_does_not_claim_an_absent_note_exists() {
    let error = note_deploy_wait_confirmation_with(
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&submission()),
        async || Ok(Some(receipt(0))),
        async || Ok(false),
    )
    .await
    .expect_err("the expected note still has to materialize");
    assert!(
        !error
            .downcast_ref::<NoteDeployUnconfirmed>()
            .unwrap()
            .may_resubmit
    );
}

fn paid_recovery() -> crate::cli::note::NoteDeployRecoveryState {
    use crate::cli::note::*;
    let owner = dexdo_core::KeyPair::from_secret_hex(&"2a".repeat(32)).unwrap();
    let mut state = NoteDeployRecoveryState::new(
        NoteDeployRecoveryRequest {
            endpoint: "http://127.0.0.1:9",
            nominal: "N100",
            token_type: dexdo_core::params::SHELL_CURRENCY_ID,
            raw_value: 100_000_000_000,
            funding_multisig_address: &format!("0:{}", "d".repeat(64)),
        },
        owner.public_hex(),
        owner.secret_hex(),
    )
    .unwrap();
    let mut voucher = NoteDeployVoucherCheckpoint::new(
        owner.public_hex(),
        state.token_type,
        state.raw_value,
        false,
        "8".repeat(64),
        "9".repeat(64),
    )
    .unwrap();
    voucher.submit_maybe_sent = true;
    voucher.proof = Some(NoteDeployVoucherProof {
        proof: "abcd".into(),
        deposit_identifier_hash_hex: "a".repeat(64),
        final_layer_historical_hash_root_hex: "1".repeat(64),
        voucher_nominal_fr_hex: "2".repeat(64),
        token_type_fr_hex: "3".repeat(64),
        ephemeral_pubkey_hex: owner.public_hex().into(),
        voucher_value: state.raw_value,
        voucher_token_type: state.token_type,
        layer_number: 1,
        sk_u_hex: "8".repeat(64).into(),
        sk_u_commit_hex: "9".repeat(64),
    });
    state
        .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
        .unwrap();
    state
}

#[test]
fn older_paid_recovery_files_are_readable_without_a_message_hash() {
    let mut old = serde_json::to_value(paid_recovery()).unwrap();
    old.as_object_mut().unwrap().remove("deploy_submission");
    let loaded: crate::cli::note::NoteDeployRecoveryState = serde_json::from_value(old).unwrap();
    loaded.validate().unwrap();
    assert!(loaded.deploy_submission.is_none());
    assert!(loaded.deposit_voucher.as_ref().unwrap().proof.is_some());
}

#[test]
fn pending_identity_roundtrips_without_claiming_a_deployed_note() {
    let mut state = paid_recovery();
    state.deploy_submission = Some(submission());
    state.validate().unwrap();
    let loaded: crate::cli::note::NoteDeployRecoveryState =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    loaded.validate().unwrap();
    assert_eq!(loaded.deploy_submission, state.deploy_submission);
    assert_eq!(loaded.deposit_voucher, state.deposit_voucher);
    assert!(loaded.pn_address.is_none());
    assert!(loaded.ensure_ready_for_pool().is_err());
}

proptest::proptest! {
    #[test]
    fn corrupt_message_hash_is_rejected_before_any_resume(hash in ".{0,63}") {
        let mut state=paid_recovery(); let mut saved=submission(); saved.message_hash=hash;
        state.deploy_submission=Some(saved);
        proptest::prop_assert!(state.validate().is_err());
        proptest::prop_assert!(state.deposit_voucher.as_ref().unwrap().proof.is_some());
    }
    #[test]
    fn a_pending_submit_cannot_exist_without_the_paid_proof(time_ms in 0u64..1000) {
        let mut state=paid_recovery(); let mut saved=submission(); saved.time_ms=time_ms;
        state.deploy_submission=Some(saved);state.deposit_voucher.as_mut().unwrap().proof=None;
        proptest::prop_assert!(state.validate().is_err());
    }
}

async fn serve_json(
    handler: impl Fn(&str, serde_json::Value) -> serde_json::Value + Send + Sync + 'static,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handler = std::sync::Arc::new(handler);
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let (end, length) = loop {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    if let Some(end) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&raw[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while raw.len() < end + length {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                }
                let path = String::from_utf8_lossy(&raw[..end])
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let input = serde_json::from_slice(&raw[end..end + length])
                    .unwrap_or(serde_json::Value::Null);
                let body = handler(&path, input);
                if body.is_null() {
                    std::future::pending::<()>().await;
                }
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (endpoint, task)
}

fn harmless_read(path: &str) -> Option<serde_json::Value> {
    if path.starts_with("/v2/account?") {
        return Some(serde_json::json!({"dapp_id":dexdo_core::address::DEXDO_DAPP_ID}));
    }
    None
}
fn chain_time() -> serde_json::Value {
    serde_json::json!({"data":{"blockchain":{"blocks":{"edges":[{"node":{"gen_utime":note_deploy_now_unix().unwrap()}}]}}}})
}

#[tokio::test]
async fn production_submit_saves_exact_identity_before_post_and_waits_for_delayed_note() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let temp = tempfile::tempdir().unwrap();
    let recovery_path = temp.path().join("recovery.json");
    let captured_path = recovery_path.clone();
    let posts = Arc::new(AtomicUsize::new(0));
    let captured_posts = posts.clone();
    let active_reads = AtomicUsize::new(0);
    let (endpoint, server) = serve_json(move |path, input| {
        if let Some(reply) = harmless_read(path) { return reply; }
        if path == "/v2/messages" {
            captured_posts.fetch_add(1, Ordering::SeqCst);
            let durable = crate::cli::note::load_note_deploy_recovery(&captured_path).unwrap().unwrap();
            let saved = durable.deploy_submission.as_ref().unwrap();
            assert_eq!(saved.message_hash, dexdo_core::chain::external_message_hash(input[0]["body"].as_str().unwrap()).unwrap());
            assert!(saved.last_submit_error.is_none());
            assert_eq!(input[0]["account_id"], "10".repeat(32));
            return serde_json::json!({"error":{"code":"DUPLICATE_MESSAGE","message":"Unknown error"}});
        }
        if input["query"].as_str().unwrap().contains("gen_utime") { return chain_time(); }
        if input["variables"]["hash"].is_string() { return serde_json::json!({"data":{"blockchain":{"message":null}}}); }
        let active = active_reads.fetch_add(1, Ordering::SeqCst) > 0;
        serde_json::json!({"data":{"blockchain":{"account":{"info":if active { serde_json::json!({"acc_type_name":"Active","balance":"0","balance_other":[]}) } else { serde_json::Value::Null }}}}})
    }).await;
    let mut recovery = paid_recovery();
    recovery.endpoint = endpoint.clone();
    crate::cli::note::write_note_deploy_recovery(&recovery_path, &recovery).unwrap();
    let keys = dexdo_core::KeyPair::from_secret_hex(&recovery.owner_secret_key_hex).unwrap();
    let proof = recovery
        .deposit_voucher
        .as_ref()
        .unwrap()
        .proof
        .as_ref()
        .unwrap()
        .to_halo2();
    let client = dexdo_core::ChainClient::connect(&endpoint).unwrap();
    let root =
        dexdo_core::Address::parse(dexdo_core::private_note::artifacts::ROOT_PN_ADDRESS).unwrap();
    note_deploy_submit_private_note(
        &client,
        &root,
        &keys,
        &proof,
        "1",
        &recovery_path,
        &expected_note(),
    )
    .await
    .unwrap();
    let loaded = crate::cli::note::load_note_deploy_recovery(&recovery_path)
        .unwrap()
        .unwrap();
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "only one RootPN POST, no wallet POST"
    );
    assert_eq!(loaded.deposit_voucher, recovery.deposit_voucher);
    assert!(loaded
        .deploy_submission
        .as_ref()
        .unwrap()
        .last_submit_error
        .as_ref()
        .unwrap()
        .contains("DUPLICATE_MESSAGE"));
    assert!(
        loaded.pn_address.is_none(),
        "confirmation itself does not finalize pool custody"
    );
    server.abort();
}

#[tokio::test]
async fn cancelling_the_production_post_leaves_a_readable_submit_checkpoint() {
    use std::sync::{Arc, Mutex};
    let temp = tempfile::tempdir().unwrap();
    let recovery_path = temp.path().join("recovery.json");
    let (posted, received) = tokio::sync::oneshot::channel();
    let posted = Arc::new(Mutex::new(Some(posted)));
    let (endpoint, server) = serve_json(move |path, input| {
        if let Some(reply) = harmless_read(path) {
            return reply;
        }
        if path == "/v2/messages" {
            posted.lock().unwrap().take().unwrap().send(input).unwrap();
            return serde_json::Value::Null;
        }
        chain_time()
    })
    .await;
    let mut recovery = paid_recovery();
    recovery.endpoint = endpoint.clone();
    crate::cli::note::write_note_deploy_recovery(&recovery_path, &recovery).unwrap();
    let keys = dexdo_core::KeyPair::from_secret_hex(&recovery.owner_secret_key_hex).unwrap();
    let proof = recovery
        .deposit_voucher
        .as_ref()
        .unwrap()
        .proof
        .as_ref()
        .unwrap()
        .to_halo2();
    let client = dexdo_core::ChainClient::connect(&endpoint).unwrap();
    let root =
        dexdo_core::Address::parse(dexdo_core::private_note::artifacts::ROOT_PN_ADDRESS).unwrap();
    let expected = expected_note();
    let input = {
        let pending = note_deploy_submit_private_note(
            &client,
            &root,
            &keys,
            &proof,
            "1",
            &recovery_path,
            &expected,
        );
        tokio::pin!(pending);
        tokio::select! {
            input = received => input.unwrap(),
            result = &mut pending => panic!("POST must remain pending, got {result:?}"),
        }
    }; // Drop the command future, like an interrupted CLI.
    let saved = crate::cli::note::load_note_deploy_recovery(&recovery_path)
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.deploy_submission.as_ref().unwrap().message_hash,
        dexdo_core::chain::external_message_hash(input[0]["body"].as_str().unwrap()).unwrap()
    );
    assert_eq!(saved.deposit_voucher, recovery.deposit_voucher);
    assert!(saved.pn_address.is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&recovery_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    server.abort();
}

#[tokio::test]
async fn restart_receipt_reader_rejects_a_foreign_hash_destination_or_dapp() {
    for field in ["hash", "destination", "dapp"] {
        let saved = submission();
        let response = serde_json::json!({"data":{"blockchain":{
            "message":{"id":if field=="hash" { "e".repeat(64) } else { saved.message_hash.clone() },"dst":if field=="destination" { "e".repeat(64) } else { "10".repeat(32) },"dst_transaction":{"id":"c".repeat(64),"status":3,"account_addr":"10".repeat(32),"aborted":true,"compute":{"exit_code":403},"action":{"success":false,"result_code":0,"no_funds":false}}},
            "account":{"info":{"id":"10".repeat(32),"dapp_id":if field=="dapp" { "e".repeat(64) } else { dexdo_core::address::DEXDO_DAPP_ID.to_string() }}}
        }}});
        let (endpoint, server) = serve_json(move |_, _| response.clone()).await;
        let error = dexdo_core::chain::read_note_deploy_rootpn_action_by_hash(
            &dexdo_core::chain_http_client().unwrap(),
            &endpoint,
            &saved.message_hash,
            &"10".repeat(32),
            dexdo_core::address::DEXDO_DAPP_ID,
        )
        .await
        .unwrap_err();
        assert!(
            !note_deploy_has_exact_finalized_rootpn_exit_code(&error, 403),
            "foreign {field} must never authorize another proof"
        );
        server.abort();
    }
}

#[test]
fn the_unconfirmed_machine_error_preserves_public_diagnostics_and_safe_resume() {
    let error = anyhow::Error::new(NoteDeployUnconfirmed::new(
        format!("DUPLICATE_MESSAGE; expected note {}; message hash {}; recovery /tmp/note.recovery.json",expected_note(),submission().message_hash),false,
    ));
    let error = note_deploy_resume_error("0:wallet", error);
    let operation = crate::cli::machine::OP_NOTE_DEPLOY;
    let machine = crate::cli::machine::machine_error(
        operation,
        crate::cli::machine::classify_error(operation, &error),
        &error,
    );
    let json = serde_json::to_value(machine).unwrap();
    assert_eq!(json["code"], "NOTE_DEPLOY_UNCONFIRMED");
    assert_eq!(json["retryable"], true);
    assert!(json["message"]
        .as_str()
        .unwrap()
        .contains("same recovery file"));
    let cause = json["cause"].as_str().unwrap();
    assert!(cause.contains(&submission().message_hash));
    assert!(cause.contains("DUPLICATE_MESSAGE"));
    assert!(cause.contains(&expected_note()));
    assert!(!cause.contains("/tmp/"));
    assert_ne!(
        crate::cli::machine::classify_error(crate::cli::machine::OP_BUYER_RUNTIME, &error).as_str(),
        "NOTE_DEPLOY_UNCONFIRMED"
    );
}

#[tokio::test]
async fn an_unrelated_expected_note_is_refused_before_any_recovery_read() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let (endpoint, server) = serve_json(move |_, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        serde_json::json!({})
    })
    .await;
    let client = dexdo_core::ChainClient::connect(&endpoint).unwrap();
    let mut saved = submission();
    saved.expected_note_address = expected_note().replace(&"a".repeat(64), &"d".repeat(64));
    let error = note_deploy_wait_confirmation(
        &client,
        &expected_note(),
        std::path::Path::new("recovery.json"),
        Some(&saved),
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("differs from the current paid proof"));
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    server.abort();
}
