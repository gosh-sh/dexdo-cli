//! Paid-voucher recovery and public-input reconciliation.
use super::*;
use dexdo_core::private_note::halo2::live::Halo2Proof;

fn fr_hex(value: u64) -> String {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&value.to_le_bytes());
    hex::encode(bytes)
}

fn proof() -> Halo2Proof {
    let value = crate::cli::note::NoteNominal::N100
        .raw_value(dexdo_core::private_note::proof::TokenType::Shell.decimals());
    Halo2Proof {
        proof: "fixture-proof".into(),
        deposit_identifier_hash_hex: "1".repeat(64),
        final_layer_historical_hash_root_hex: "2".repeat(64),
        voucher_nominal_fr_hex: fr_hex(value),
        token_type_fr_hex: fr_hex(u64::from(SHELL_CURRENCY_ID)),
        ephemeral_pubkey_hex: dexdo_core::KeyPair::generate().public_hex().into(),
        voucher_value: value,
        voucher_token_type: SHELL_CURRENCY_ID,
        layer_number: 1,
        sk_u_hex: "3".repeat(64),
        sk_u_commit_hex: "4".repeat(64),
    }
}

#[test]
fn attached_amount_in_public_inputs_is_rejected_before_deploy() {
    let mut proof = proof();
    proof.voucher_nominal_fr_hex =
        fr_hex(proof.voucher_value + crate::cli::note::contract_gas_deposit_raw());
    let error = note_deploy_private_note_params(&proof.ephemeral_pubkey_hex, &proof, "1")
        .expect_err("the wallet-attached amount must not pass as the proven nominal");
    assert!(error.to_string().contains("voucher nominal"));
    assert_eq!(
        crate::cli::machine::classify_error(crate::cli::machine::OP_NOTE_DEPLOY, &error).as_str(),
        "INVALID_ARGUMENT",
    );
}

#[test]
fn inconsistent_token_public_input_is_rejected_before_deploy() {
    let mut proof = proof();
    proof.token_type_fr_hex = fr_hex(u64::from(proof.voucher_token_type) + 1);
    let error = note_deploy_private_note_params(&proof.ephemeral_pubkey_hex, &proof, "1")
        .expect_err("a different proven currency must never be submitted");
    assert!(error.to_string().contains("token type"));
    assert_eq!(
        crate::cli::machine::classify_error(crate::cli::machine::OP_NOTE_DEPLOY, &error).as_str(),
        "INVALID_ARGUMENT",
    );
}

#[test]
fn finalized_137_does_not_claim_a_proven_mismatch_or_a_consumed_voucher() {
    let error = note_deploy_rootpn_action_result(
        "deployPrivateNote",
        None,
        Some(dexdo_core::chain::NoteDeployRootPnActionObservation {
            transaction_hash: "a".repeat(64),
            compute_exit_code: 137,
            aborted: true,
            action_result_code: Some(0),
        }),
    )
    .unwrap_err();
    let message = note_deploy_error("0:wallet", error).to_string();
    assert!(message.contains("exit 137"));
    assert!(!message.contains("do_not_retry_this_voucher"), "{message}");
    assert!(!message.contains("reverts identically"), "{message}");
}

pub(super) fn paid_recovery() -> crate::cli::note::NoteDeployRecoveryState {
    use crate::cli::note::*;
    let keys = dexdo_core::KeyPair::generate();
    let mut proof = proof();
    proof.ephemeral_pubkey_hex = keys.public_hex().into();
    let mut state = NoteDeployRecoveryState::new(
        NoteDeployRecoveryRequest {
            endpoint: "http://127.0.0.1:9",
            nominal: "N100",
            token_type: proof.voucher_token_type,
            raw_value: proof.voucher_value,
            funding_multisig_address: &format!("0:{}", "5".repeat(64)),
        },
        keys.public_hex(),
        keys.secret_hex(),
    )
    .unwrap();
    let mut voucher = NoteDeployVoucherCheckpoint::new(
        keys.public_hex(),
        proof.voucher_token_type,
        proof.voucher_value,
        false,
        proof.sk_u_hex.clone(),
        proof.sk_u_commit_hex.clone(),
    )
    .unwrap();
    voucher.submit_maybe_sent = true;
    voucher.event = Some(NoteDeployVoucherEvent {
        id: "paid-event".into(),
        boc: "saved-event-boc".into(),
        body: "saved-event-body".into(),
        dst: "saved-event-dst".into(),
        created_at: 1,
        block_id: Some("paid-block".into()),
    });
    voucher.proof = Some(NoteDeployVoucherProof::from_halo2(&proof));
    state
        .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
        .unwrap();
    state.deploy_submission = Some(NoteDeploySubmission {
        message_hash: "6".repeat(64),
        root_pn_address: dexdo_core::params::ROOT_PN_ADDRESS.to_string(),
        expected_note_address: format!("{}::{}", "0".repeat(63) + "4", "7".repeat(64)),
        time_ms: 0,
        expire_at_unix: 1,
        last_submit_error: None,
    });
    state.validate().unwrap();
    state
}

pub(super) fn finalized(exit: i64) -> anyhow::Error {
    note_deploy_rootpn_action_result(
        "deployPrivateNote",
        None,
        Some(dexdo_core::chain::NoteDeployRootPnActionObservation {
            transaction_hash: "a".repeat(64),
            compute_exit_code: exit,
            aborted: true,
            action_result_code: Some(0),
        }),
    )
    .unwrap_err()
}

#[test]
fn retired_reprove_flag_is_rejected_and_incomplete_reproof_is_refused() {
    use clap::Parser;
    #[derive(Parser)]
    struct Parse {
        #[command(flatten)]
        args: NoteDeployArgs,
    }
    assert!(Parse::try_parse_from(["deploy", "--nominal", "N100"]).is_ok());
    let error = Parse::try_parse_from(["deploy", "--nominal", "N100", "--reprove-paid-voucher"])
        .err()
        .expect("the public flag was removed");
    assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    let mut state = paid_recovery();
    note_deploy_require_paid_reproof(&state).unwrap();
    let mut inconsistent = paid_recovery();
    inconsistent.pn_address = Some(
        inconsistent
            .deploy_submission
            .as_ref()
            .unwrap()
            .expected_note_address
            .clone(),
    );
    assert!(note_deploy_require_paid_reproof(&inconsistent).is_err());
    let mut wrong_owner = paid_recovery();
    wrong_owner.owner_secret_key_hex = dexdo_core::KeyPair::generate()
        .secret_hex()
        .to_owned()
        .into();
    assert!(note_deploy_require_paid_reproof(&wrong_owner).is_err());
    state.deposit_voucher.as_mut().unwrap().proof = None;
    assert!(note_deploy_require_paid_reproof(&state).is_err());
    state.deposit_voucher = None;
    state.deploy_submission = None;
    assert!(note_deploy_require_paid_reproof(&state).is_err());
}

#[tokio::test]
async fn finalized_137_rebuilds_once_and_preserves_the_same_paid_voucher() {
    use crate::cli::note::{write_note_deploy_recovery, NoteDeployVoucherProof};
    use std::cell::Cell;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("paid.recovery.json");
    let mut state = paid_recovery();
    state
        .deposit_voucher
        .as_mut()
        .unwrap()
        .proof
        .as_mut()
        .unwrap()
        .voucher_nominal_fr_hex =
        fr_hex(state.raw_value + crate::cli::note::contract_gas_deposit_raw());
    let original = state.deposit_voucher.clone().unwrap();
    let owner = state.owner_public_key_hex.clone();
    let rebuilt = Cell::new(0);
    write_note_deploy_recovery(&path, &state).unwrap();
    note_deploy_reprove_paid_voucher_with(
        &mut state,
        async |_| Err(finalized(137)),
        async |state| {
            rebuilt.set(rebuilt.get() + 1);
            assert_eq!(
                note_deploy_funding_route(state),
                NoteDeployFundingRoute::VoucherRecoveryRequired
            );
            let voucher = state.deposit_voucher.as_mut().unwrap();
            assert!(voucher.proof.is_none());
            assert!(voucher.submit_maybe_sent);
            assert_eq!(voucher.event, original.event);
            assert_eq!(voucher.sk_u_commit_hex, original.sk_u_commit_hex);
            assert_eq!(voucher.sk_u_hex, original.sk_u_hex);
            assert_eq!(state.owner_public_key_hex, owner);
            assert!(state.deploy_submission.is_none());
            let mut replacement = original.proof.as_ref().unwrap().to_halo2();
            replacement.proof = "rebuilt-proof".into();
            replacement.voucher_nominal_fr_hex = fr_hex(state.raw_value);
            note_deploy_validate_public_inputs(&replacement)?;
            voucher.proof = Some(NoteDeployVoucherProof::from_halo2(&replacement));
            write_note_deploy_recovery(&path, state)
        },
    )
    .await
    .unwrap();
    assert_eq!(rebuilt.get(), 1);
    let saved = crate::cli::note::load_note_deploy_recovery(&path)
        .unwrap()
        .unwrap();
    assert!(saved.deploy_submission.is_none());
    assert_eq!(
        saved.deposit_voucher.unwrap().proof.unwrap().proof,
        "rebuilt-proof"
    );
    // Restart before the replacement's POST must resume it without rebuilding or buying anything.
    note_deploy_reprove_paid_voucher_with(
        &mut state,
        async |_| panic!("no previous replacement POST to reconcile"),
        async |_| panic!("the persisted replacement must not be rebuilt twice"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_reproof_retains_the_original_proof_submission_and_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("paid.recovery.json");
    let mut state = paid_recovery();
    crate::cli::note::write_note_deploy_recovery(&path, &state).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let original = state.deposit_voucher.clone();
    let submission = state.deploy_submission.clone();
    let error = note_deploy_reprove_paid_voucher_with(
        &mut state,
        async |_| Err(finalized(137)),
        async |_| anyhow::bail!("prover unavailable"),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("prover unavailable"));
    assert_eq!(state.deposit_voucher, original);
    assert_eq!(state.deploy_submission, submission);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn active_note_and_other_failures_never_rebuild_a_proof() {
    for exit in [101, 4030, 1370] {
        let mut state = paid_recovery();
        let original = state.deposit_voucher.clone();
        note_deploy_reprove_paid_voucher_with(
            &mut state,
            async |_| Err(finalized(exit)),
            async |_| panic!("only finalized 137 is eligible"),
        )
        .await
        .unwrap_err();
        assert_eq!(state.deposit_voucher, original);
        assert!(state.deploy_submission.is_some());
    }
    let mut state = paid_recovery();
    for exit in [137, 403] {
        note_deploy_reprove_paid_voucher_with(
            &mut state,
            async |_| Err(anyhow::anyhow!("RootPN.deployPrivateNote exit_code={exit}")),
            async |_| panic!("untyped error text is not a finalized receipt"),
        )
        .await
        .unwrap_err();
    }
    assert!(state.deploy_submission.is_some());
    note_deploy_reprove_paid_voucher_with(
        &mut state,
        async |_| Ok(()),
        async |_| panic!("an active note must be finalized without a second proof"),
    )
    .await
    .unwrap();
    assert!(state.deposit_voucher.as_ref().unwrap().proof.is_some());
    assert!(state.deploy_submission.is_some());
}

#[tokio::test(start_paused = true)]
async fn absent_receipt_or_failed_absence_read_preserves_the_paid_proof() {
    for (exit, missing_receipt, absence_read_fails) in [
        (137, true, false),
        (137, false, true),
        (403, true, false),
        (403, false, true),
    ] {
        let mut state = paid_recovery();
        let original = state.deposit_voucher.clone();
        let error = note_deploy_reprove_paid_voucher_with(
            &mut state,
            async |state| {
                note_deploy_wait_confirmation_with(
                    &state
                        .deploy_submission
                        .as_ref()
                        .unwrap()
                        .expected_note_address,
                    std::path::Path::new("paid.recovery.json"),
                    state.deploy_submission.as_ref(),
                    async || {
                        Ok(if missing_receipt {
                            None
                        } else {
                            Some(dexdo_core::chain::NoteDeployRootPnActionObservation {
                                transaction_hash: "a".repeat(64),
                                compute_exit_code: exit,
                                aborted: true,
                                action_result_code: Some(0),
                            })
                        })
                    },
                    async || {
                        if absence_read_fails {
                            anyhow::bail!("account reader unavailable")
                        } else {
                            Ok(false)
                        }
                    },
                )
                .await
            },
            async |_| panic!("an ambiguous result must never replace the paid proof"),
        )
        .await
        .unwrap_err();
        assert!(!note_deploy_has_exact_finalized_rootpn_exit_code(
            &error, exit
        ));
        assert_eq!(state.deposit_voucher, original);
        assert!(state.deploy_submission.is_some());
    }
}

#[tokio::test]
async fn restart_recorded_note_preserves_owner_and_finalization() {
    use crate::cli::note::{load_note_deploy_recovery, write_note_deploy_recovery};
    use clap::Parser;
    #[derive(Parser)]
    struct Parse {
        #[command(flatten)]
        args: NoteDeployArgs,
    }

    for chain_complete in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("paid.recovery.json");
        let pool = temp.path().join("pool.json");
        let mut state = paid_recovery();
        let note = state
            .deploy_submission
            .as_ref()
            .unwrap()
            .expected_note_address
            .clone();
        let dih = dexdo_core::private_note::proof::hex_u256_to_dec(
            &state
                .deposit_voucher
                .as_ref()
                .unwrap()
                .proof
                .as_ref()
                .unwrap()
                .deposit_identifier_hash_hex,
        )
        .unwrap();
        state
            .mark_private_note_deployed(note.clone(), dih, 1)
            .unwrap();
        if chain_complete {
            state.mark_shell_funded_and_checked().unwrap();
        }
        write_note_deploy_recovery(&path, &state).unwrap();
        let mut resumed = load_note_deploy_recovery(&path).unwrap().unwrap();
        let original = serde_json::to_vec(&resumed).unwrap();

        // Validate the original paid recovery before resuming.
        note_deploy_require_paid_reproof(&resumed).unwrap();
        note_deploy_reprove_paid_voucher_with(
            &mut resumed,
            async |_| panic!("a durable note record must bypass proof confirmation"),
            async |_| panic!("a durable note record must never rebuild its proof"),
        )
        .await
        .unwrap();
        assert!(serde_json::to_vec(&resumed).unwrap() == original);
        assert_eq!(
            note_deploy_funding_route(&resumed),
            NoteDeployFundingRoute::ResumeWithoutFunding
        );
        assert!(!note_deploy_recovery_needs_new_proof(&resumed));

        if chain_complete {
            // Real production dispatch must not call RootPN or load a funding key on this restart.
            let args = Parse::try_parse_from(["deploy", "--nominal", "N100"])
                .unwrap()
                .args;
            let client = dexdo_core::ChainClient::connect(&resumed.endpoint).unwrap();
            let wallet = dexdo_core::CanonicalAddress::parse(
                resumed.funding_multisig_address.as_deref().unwrap(),
            )
            .unwrap()
            .legacy();
            let request = crate::cli::note::NoteDeployRecoveryRequest {
                endpoint: &resumed.endpoint,
                nominal: &resumed.nominal,
                token_type: resumed.token_type,
                raw_value: resumed.raw_value,
                funding_multisig_address: &wallet,
            };
            let paths = dexdo_core::private_note::Halo2Paths::from_env();
            let mut ops = NoteDeployProductionOps {
                args: &args,
                client: &client,
                recovery_path: &path,
                pool_path: &pool,
                funding_multisig_address: &wallet,
                recovery_request: request,
                funding_binding: None,
                resolved_hot_address: &wallet,
                funding_network: "test",
                nominal: crate::cli::note::NoteNominal::N100,
                pn_keys: Some(
                    dexdo_core::KeyPair::from_secret_hex(&resumed.owner_secret_key_hex).unwrap(),
                ),
                halo2_paths: &paths,
                voucher_failpoints: Default::default(),
                funding_notice: crate::cli::machine::MachineFundingNotice::AlreadyFunded,
            };
            let mut chain = load_note_deploy_recovery(&path).unwrap().unwrap();
            let onboard = ops.resume_chain(&mut chain).await.unwrap();
            assert_eq!(
                dexdo_core::CanonicalAddress::parse(onboard.pn_address.as_deref().unwrap())
                    .unwrap(),
                dexdo_core::CanonicalAddress::parse(&note).unwrap(),
            );
            assert!(onboard.owner_secret_key_hex.as_ref() == Some(&resumed.owner_secret_key_hex));
        } else {
            assert!(!resumed.shell_funded && !resumed.sanity_checked);
            assert!(resumed.ensure_ready_for_pool().is_err());
            // Model a successful ordinary getDetails check during finalization.
            resumed.mark_shell_funded_and_checked().unwrap();
        }
        let onboard = resumed.to_onboard_state().unwrap();
        note_deploy_fold_state_into_pool(&pool, &onboard).unwrap();
        let pooled: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&pool).unwrap()).unwrap();
        assert_eq!(pooled["notes"].as_array().unwrap().len(), 1);
        assert_eq!(pooled["notes"][0]["address"], note);
        assert!(
            pooled["notes"][0]["owner_secret_key_hex"].as_str()
                == Some(resumed.owner_secret_key_hex.as_str())
        );
        assert!(resumed.deposit_voucher.as_ref().unwrap().submit_maybe_sent);
    }
}

#[tokio::test]
async fn restart_replacement_403_uses_history_recovery() {
    use crate::cli::note::{
        load_note_deploy_recovery, write_note_deploy_recovery, NoteDeployVoucherKind,
        NoteDeployVoucherProof,
    };
    use std::cell::{Cell, RefCell};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("paid.recovery.json");
    let mut state = paid_recovery();
    // The replacement proof and its POST identity were saved; confirmation was interrupted.
    state
        .deposit_voucher
        .as_mut()
        .unwrap()
        .proof
        .as_mut()
        .unwrap()
        .proof = "replacement-after-137".into();
    write_note_deploy_recovery(&path, &state).unwrap();
    let original = state.deposit_voucher.clone().unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut resumed = load_note_deploy_recovery(&path).unwrap().unwrap();
    note_deploy_reprove_paid_voucher_with(
        &mut resumed,
        async |saved| {
            note_deploy_wait_confirmation_with(
                &saved
                    .deploy_submission
                    .as_ref()
                    .unwrap()
                    .expected_note_address,
                &path,
                saved.deploy_submission.as_ref(),
                async || {
                    Ok(Some(dexdo_core::chain::NoteDeployRootPnActionObservation {
                        transaction_hash: "a".repeat(64),
                        compute_exit_code: 403,
                        aborted: true,
                        action_result_code: Some(0),
                    }))
                },
                async || Ok(false),
            )
            .await
        },
        async |_| panic!("403 must use history recovery, not invalid-proof rebuilding"),
    )
    .await
    .unwrap();
    assert_eq!(resumed.deposit_voucher.as_ref(), Some(&original));
    assert!(resumed.deploy_submission.is_some());
    assert!(std::fs::read(&path).unwrap() == before);

    let mut previous_failure = Some(finalized(403));
    let submitted_layers = RefCell::new(Vec::new());
    let history_reproofs = Cell::new(0);
    let accepted = note_deploy_run_reproof_loop(
        resumed
            .deposit_voucher
            .as_ref()
            .unwrap()
            .proof
            .as_ref()
            .unwrap()
            .to_halo2(),
        async |proof| {
            note_deploy_submit_proof_once(
                proof,
                async |_| Ok(false),
                async |proof| {
                    if let Some(error) = previous_failure.take() {
                        return Err(error);
                    }
                    submitted_layers.borrow_mut().push(proof.layer_number);
                    Ok(())
                },
            )
            .await
        },
        async |rejected| {
            history_reproofs.set(history_reproofs.get() + 1);
            assert_eq!(
                note_deploy_funding_route(&resumed),
                NoteDeployFundingRoute::VoucherRecoveryRequired
            );
            note_deploy_persist_rejected_proof(
                &path,
                &mut resumed,
                NoteDeployVoucherKind::Deposit,
                rejected,
            )?;
            let voucher = resumed.deposit_voucher.as_mut().unwrap();
            assert!(voucher.submit_maybe_sent);
            assert_eq!(voucher.event, original.event);
            let mut replacement = rejected.clone();
            replacement.proof = "replacement-on-next-history-layer".into();
            replacement.layer_number += 1;
            voucher.replace_rejected_proof(NoteDeployVoucherProof::from_halo2(&replacement))?;
            write_note_deploy_recovery(&path, &resumed)?;
            Ok(replacement)
        },
    )
    .await
    .unwrap();
    assert_eq!(history_reproofs.get(), 1);
    assert_eq!(*submitted_layers.borrow(), [2]);
    assert_eq!(accepted.layer_number, 2);
    let saved = load_note_deploy_recovery(&path).unwrap().unwrap();
    let voucher = saved.deposit_voucher.unwrap();
    assert!(voucher.submit_maybe_sent);
    assert_eq!(voucher.event, original.event);
    assert!(voucher.sk_u_hex == original.sk_u_hex);
    assert_eq!(voucher.sk_u_commit_hex, original.sk_u_commit_hex);
    assert!(saved.owner_secret_key_hex == state.owner_secret_key_hex);
    assert_eq!(voucher.last_rejected_proof_layer, Some(1));
    assert_eq!(voucher.proof.unwrap().layer_number, 2);
}

proptest::proptest! {
    #[test]
    fn public_input_binding_rejects_every_changed_nominal_or_currency(
        value in 1u64..=u64::MAX, token in proptest::prelude::any::<u32>(),
        index in 0usize..32, mask in 1u8..=u8::MAX, nominal in proptest::prelude::any::<bool>(),
    ) {
        let mut proof = proof();
        proof.voucher_value = value;
        proof.voucher_token_type = token;
        proof.voucher_nominal_fr_hex = fr_hex(value);
        proof.token_type_fr_hex = fr_hex(u64::from(token));
        proptest::prop_assert!(note_deploy_validate_public_inputs(&proof).is_ok());
        let encoded = if nominal {
            &mut proof.voucher_nominal_fr_hex
        } else { &mut proof.token_type_fr_hex };
        let mut bytes = hex::decode(&*encoded).unwrap();
        bytes[index] ^= mask;
        *encoded = hex::encode(bytes);
        proptest::prop_assert!(note_deploy_validate_public_inputs(&proof).is_err());
    }
}
