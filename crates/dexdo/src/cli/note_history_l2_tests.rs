//! Paid-event proof selection exercises production journal writes without funding credentials.
use super::*;
use crate::cli::note::{self, NoteDeployVoucherKind};
use dexdo_core::private_note::halo2::live::{ExpiredVoucherAnchor, Halo2Proof};
use std::cell::RefCell;

fn event_without_proof() -> (note::NoteDeployRecoveryState, Halo2Proof) {
    let mut state = super::note_deploy_paid_voucher_2088_tests::paid_recovery();
    let proof = state
        .deposit_voucher
        .as_mut()
        .unwrap()
        .proof
        .take()
        .unwrap()
        .to_halo2();
    state.deploy_submission = None;
    (state, proof)
}

fn expiry(layer: u8) -> anyhow::Error {
    ExpiredVoucherAnchor {
        layer_number: layer,
        expires_at_height: 16_512,
        head_height: 16_512,
    }
    .into()
}

#[tokio::test]
async fn fresh_paid_event_starts_at_l1_and_persists_selection_before_proving() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paid.json");
    let (mut state, proof) = event_without_proof();
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let mut checkpoint = state.deposit_voucher.clone().unwrap();
    let original_event = checkpoint.event.clone();
    let result = note_deploy_prove_paid_layers(
        &path,
        &mut state,
        NoteDeployVoucherKind::Deposit,
        &mut checkpoint,
        async |layer| {
            assert_eq!(layer, dexdo_core::params::NOTE_DEPLOY_PROOF_LAYER_MIN);
            let saved = note::load_note_deploy_recovery(&path)?.unwrap();
            assert_eq!(
                saved.deposit_voucher.unwrap().selected_proof_layer,
                Some(layer)
            );
            Ok(proof.clone())
        },
    )
    .await
    .unwrap();
    assert_eq!(result.layer_number, 1);
    assert_eq!(checkpoint.event, original_event);
    assert!(checkpoint.submit_maybe_sent);
}

#[tokio::test]
async fn expired_l1_rebuilds_same_paid_event_on_l2_without_an_old_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paid.json");
    let (mut state, mut proof) = event_without_proof();
    let original_owner = state.owner_public_key_hex.clone();
    let original = state.deposit_voucher.clone().unwrap();
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let mut checkpoint = state.deposit_voucher.clone().unwrap();
    let calls = RefCell::new(Vec::new());
    let result = note_deploy_prove_paid_layers(
        &path,
        &mut state,
        NoteDeployVoucherKind::Deposit,
        &mut checkpoint,
        async |layer| {
            calls.borrow_mut().push(layer);
            let saved = note::load_note_deploy_recovery(&path)?.unwrap();
            assert_eq!(
                saved.deposit_voucher.as_ref().unwrap().selected_proof_layer,
                Some(layer)
            );
            if layer == 1 {
                return Err(expiry(layer));
            }
            proof.layer_number = layer;
            Ok(proof.clone())
        },
    )
    .await
    .unwrap();
    assert_eq!(*calls.borrow(), [1, 2]);
    assert_eq!(result.layer_number, 2);
    assert_eq!(state.owner_public_key_hex, original_owner);
    let before = original;
    assert!(checkpoint.sk_u_hex == before.sk_u_hex);
    assert_eq!(checkpoint.sk_u_commit_hex, before.sk_u_commit_hex);
    assert_eq!(checkpoint.event, before.event);
    assert_eq!(checkpoint.raw_value, before.raw_value);
    assert_eq!(checkpoint.token_type, before.token_type);
    assert!(checkpoint.submit_maybe_sent);
}

#[tokio::test]
async fn interrupted_l2_selection_survives_restart_and_does_not_retry_l1() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paid.json");
    let (mut state, mut proof) = event_without_proof();
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let mut checkpoint = state.deposit_voucher.clone().unwrap();
    let error = note_deploy_prove_paid_layers(
        &path,
        &mut state,
        NoteDeployVoucherKind::Deposit,
        &mut checkpoint,
        async |layer| {
            if layer == 1 {
                Err(expiry(layer))
            } else {
                bail!("temporary BM read failure")
            }
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("temporary BM read failure"));
    let mut resumed = note::load_note_deploy_recovery(&path).unwrap().unwrap();
    let mut checkpoint = resumed.deposit_voucher.clone().unwrap();
    assert_eq!(checkpoint.selected_proof_layer, Some(2));
    assert!(checkpoint.proof.is_none());
    note_deploy_prove_paid_layers(
        &path,
        &mut resumed,
        NoteDeployVoucherKind::Deposit,
        &mut checkpoint,
        async |layer| {
            assert_eq!(layer, 2);
            proof.layer_number = layer;
            Ok(proof.clone())
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn unavailable_history_and_numeric_error_text_do_not_authorize_a_layer_change() {
    for cause in [
        "BM returned no event block",
        "request timeout",
        "403",
        "expired L1",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paid.json");
        let (mut state, _) = event_without_proof();
        note::write_note_deploy_recovery(&path, &state).unwrap();
        let mut checkpoint = state.deposit_voucher.clone().unwrap();
        let error = note_deploy_prove_paid_layers(
            &path,
            &mut state,
            NoteDeployVoucherKind::Deposit,
            &mut checkpoint,
            async |layer| {
                assert_eq!(layer, 1);
                Err(anyhow::anyhow!(cause))
            },
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        let wrapped = anyhow::anyhow!("halo2 deposit voucher: {error}");
        let displayed = note_deploy_resume_error("0:wallet", wrapped).to_string();
        assert!(
            displayed.contains(cause),
            "actual CLI formatter lost the cause: {displayed}"
        );
        assert!(message.contains(cause));
        assert!(message.contains("KEPT") && message.contains("Do not fund a new voucher"));
        let saved = note::load_note_deploy_recovery(&path).unwrap().unwrap();
        assert_eq!(saved.deposit_voucher.unwrap().selected_proof_layer, Some(1));
    }
}

#[tokio::test]
async fn rejected_proof_can_skip_a_second_anchor_only_after_observed_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paid.json");
    let mut state = super::note_deploy_paid_voucher_2088_tests::paid_recovery();
    state.deploy_submission = None;
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let mut checkpoint = state.deposit_voucher.clone().unwrap();
    let mut proof = checkpoint.proof.as_ref().unwrap().to_halo2();
    checkpoint.reject_current_proof().unwrap();
    note_deploy_persist_voucher_checkpoint(
        &path,
        &mut state,
        NoteDeployVoucherKind::Deposit,
        checkpoint.clone(),
    )
    .unwrap();
    let result = note_deploy_prove_paid_layers(
        &path,
        &mut state,
        NoteDeployVoucherKind::Deposit,
        &mut checkpoint,
        async |layer| {
            assert!(layer > 1);
            if layer == 2 {
                return Err(expiry(layer));
            }
            proof.layer_number = layer;
            Ok(proof.clone())
        },
    )
    .await
    .unwrap();
    assert_eq!(
        result.layer_number,
        dexdo_core::params::NOTE_DEPLOY_PROOF_LAYER_MAX
    );
    checkpoint
        .replace_rejected_proof(note::NoteDeployVoucherProof::from_halo2(&result))
        .unwrap();
    checkpoint.validate("accepted raised proof").unwrap();
}

#[tokio::test]
async fn a_prover_layer_mismatch_preserves_the_paid_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paid.json");
    let (mut state, mut proof) = event_without_proof();
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let mut checkpoint = state.deposit_voucher.clone().unwrap();
    proof.layer_number = 2;
    let error = note_deploy_prove_paid_layers(
        &path,
        &mut state,
        NoteDeployVoucherKind::Deposit,
        &mut checkpoint,
        async |_| Ok(proof.clone()),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("returned L2 for selected L1"));
    assert!(note::load_note_deploy_recovery(&path)
        .unwrap()
        .unwrap()
        .deposit_voucher
        .unwrap()
        .proof
        .is_none());
}

proptest::proptest! {
    #[test]
    fn selected_paid_layer_roundtrips_without_changing_event_or_funding(
        layer in dexdo_core::params::NOTE_DEPLOY_PROOF_LAYER_MIN..=dexdo_core::params::NOTE_DEPLOY_PROOF_LAYER_MAX,
    ) {
        let (state, _) = event_without_proof();
        let mut checkpoint = state.deposit_voucher.unwrap();
        let original = checkpoint.clone();
        for selected in dexdo_core::params::NOTE_DEPLOY_PROOF_LAYER_MIN..=layer {
            checkpoint.select_proof_layer(selected).unwrap();
        }
        let decoded: note::NoteDeployVoucherCheckpoint = serde_json::from_str(&serde_json::to_string(&checkpoint).unwrap()).unwrap();
        proptest::prop_assert_eq!(decoded.next_anchor_layer(), Some(layer));
        proptest::prop_assert!(decoded.sk_u_hex == original.sk_u_hex);
        proptest::prop_assert_eq!(decoded.event, original.event);
        proptest::prop_assert_eq!(decoded.sk_u_commit_hex, original.sk_u_commit_hex);
        proptest::prop_assert!(decoded.submit_maybe_sent);
    }
}

#[tokio::test]
async fn failed_137_rebuild_preserves_durable_proof_and_submission_during_every_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paid.json");
    let mut state = super::note_deploy_paid_voucher_2088_tests::paid_recovery();
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let original_proof = state.deposit_voucher.as_ref().unwrap().proof.clone();
    let original_submission = state.deploy_submission.clone();
    let error = note_deploy_reprove_paid_voucher_with(
        &mut state,
        async |_| Err(super::note_deploy_paid_voucher_2088_tests::finalized(137)),
        async |state| {
            assert!(state.deploy_submission.is_none());
            let mut checkpoint = state.deposit_voucher.clone().unwrap();
            assert!(checkpoint.proof.is_none());
            note_deploy_prove_paid_layers(
                &path,
                state,
                NoteDeployVoucherKind::Deposit,
                &mut checkpoint,
                async |layer| {
                    // A crash HERE must leave the original evidence on disk,
                    // not depend on an error handler restoring it afterwards.
                    let saved = note::load_note_deploy_recovery(&path)?.unwrap();
                    assert_eq!(saved.deposit_voucher.unwrap().proof, original_proof);
                    assert_eq!(saved.deploy_submission, original_submission);
                    if layer == 1 {
                        Err(expiry(layer))
                    } else {
                        bail!("prover unavailable")
                    }
                },
            )
            .await
            .map(|_| ())
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("prover unavailable"));
    let saved = note::load_note_deploy_recovery(&path).unwrap().unwrap();
    assert_eq!(saved.deploy_submission, original_submission);
    let voucher = saved.deposit_voucher.unwrap();
    assert_eq!(voucher.proof, original_proof);
    assert_eq!(voucher.selected_proof_layer, Some(2));
}

#[test]
fn malformed_journal_cannot_select_a_layer_below_a_recorded_rejection() {
    let (mut state, proof) = event_without_proof();
    let voucher = state.deposit_voucher.as_mut().unwrap();
    voucher.proof = Some(note::NoteDeployVoucherProof::from_halo2(&proof));
    voucher.last_rejected_proof_layer = Some(2);
    voucher.selected_proof_layer = Some(1);
    assert!(state.validate().is_err());
    let encoded = serde_json::to_vec(&state).unwrap();
    let decoded: note::NoteDeployRecoveryState = serde_json::from_slice(&encoded).unwrap();
    assert!(decoded.validate().is_err());
}

#[test]
fn a_cpu_budget_timeout_does_not_claim_observed_root_expiry() {
    let message = note_deploy_proof_window_missed_message(
        "deposit",
        "L1",
        dexdo_core::params::NOTE_DEPLOY_PROOF_TIMEOUT,
        std::path::Path::new("paid.recovery.json"),
        NoteDeployRecoveryOutcome::KeptPaidFor,
        None,
    );
    assert!(message.contains("does not establish"));
    assert!(!message.contains("layer 0 is gone"));
    assert!(message.contains("KEPT"));
}
