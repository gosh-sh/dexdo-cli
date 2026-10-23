//! Exercise the recover command's production orchestration and shared paid resume policy.
//! Chain/prover responses are scripted; journal and pool writes use production persistence.
use super::note_deploy_paid_voucher_2088_tests::{finalized, paid_recovery};
use super::*;
use crate::cli::note::{
    self, NoteDeployRecoveryState, NoteDeployVoucherKind, NoteDeployVoucherProof,
};
use std::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    InvalidProof,
    ExpiredHistory,
    ExpiredSubmission,
    Ambiguous,
    ReproveUnavailable,
    HistoryUnavailable,
    InvalidReplacement,
    ForeignOwner,
    RejectedAgain,
}

struct RecoverOps {
    path: std::path::PathBuf,
    pool: std::path::PathBuf,
    outcome: Outcome,
    checks: Cell<usize>,
    confirms: Cell<usize>,
    reproves: Cell<usize>,
    resumes: Cell<usize>,
    finalizations: Cell<usize>,
    observed_route: Cell<Option<NoteDeployFundingRoute>>,
}

impl RecoverOps {
    fn new(dir: &std::path::Path, state: &NoteDeployRecoveryState, outcome: Outcome) -> Self {
        let path = dir.join("paid.recovery.json");
        note::write_note_deploy_recovery(&path, state).unwrap();
        Self {
            path,
            pool: dir.join("pool.json"),
            outcome,
            checks: Cell::new(0),
            confirms: Cell::new(0),
            reproves: Cell::new(0),
            resumes: Cell::new(0),
            finalizations: Cell::new(0),
            observed_route: Cell::new(None),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl NoteDeployResolvedOps for RecoverOps {
    async fn load_recovery(&mut self) -> Result<NoteDeployRecoveryState> {
        Ok(note::load_note_deploy_recovery(&self.path)?.unwrap())
    }
    async fn preflight_doctor(&mut self) -> Result<()> {
        self.checks.set(self.checks.get() + 1);
        Ok(())
    }
    async fn prepare_funding(&mut self, route: NoteDeployFundingRoute) -> Result<()> {
        // A regression in orchestration must fail before any mocked chain work.
        assert_ne!(route, NoteDeployFundingRoute::FreshSpend);
        self.observed_route.set(Some(route));
        Ok(())
    }
    async fn preflight_prover(&mut self) -> Result<()> {
        Ok(())
    }
    async fn resume_chain(
        &mut self,
        recovery: &mut NoteDeployRecoveryState,
    ) -> Result<note::OnboardPnState> {
        let outcome = self.outcome;
        let original = recovery.deposit_voucher.clone();
        let path = &self.path;
        let confirms = &self.confirms;
        let reproves = &self.reproves;
        let resumes = &self.resumes;
        // This is the same automatic dispatch called by production deploy AND recover.
        note_deploy_resume_paid_with(recovery,
            async |_| {
                confirms.set(confirms.get() + 1);
                match outcome {
                    Outcome::InvalidProof | Outcome::ReproveUnavailable | Outcome::RejectedAgain
                        | Outcome::InvalidReplacement => Err(finalized(137)),
                    Outcome::ExpiredHistory => Err(finalized(403)),
                    Outcome::Ambiguous => Err(NoteDeployUnconfirmed::new("pending or unreadable receipt".into(), false).into()),
                    Outcome::ExpiredSubmission => Err(NoteDeployUnconfirmed::new("expired submission with absent note".into(), true).into()),
                    _ => Ok(()),
                }
            },
            async |state| {
                reproves.set(reproves.get() + 1);
                assert!(state.deploy_submission.is_none());
                let voucher = state.deposit_voucher.as_mut().unwrap();
                let old = original.as_ref().unwrap();
                assert!(voucher.submit_maybe_sent);
                assert_eq!(voucher.event, old.event);
                assert!(voucher.sk_u_hex == old.sk_u_hex);
                assert_eq!(voucher.sk_u_commit_hex, old.sk_u_commit_hex);
                assert!(voucher.proof.is_none());
                if outcome == Outcome::ReproveUnavailable { bail!("prover unavailable; preserve paid recovery"); }
                let mut replacement = old.proof.as_ref().unwrap().to_halo2();
                replacement.proof = "replacement-for-the-same-voucher".into();
                if outcome == Outcome::InvalidReplacement {
                    replacement.voucher_nominal_fr_hex = "f".repeat(64);
                }
                note_deploy_validate_public_inputs(&replacement)?;
                voucher.proof = Some(NoteDeployVoucherProof::from_halo2(&replacement));
                note::write_note_deploy_recovery(path, state)
            },
            async |state| {
                resumes.set(resumes.get() + 1);
                if outcome == Outcome::HistoryUnavailable {
                    bail!("original event block unavailable; preserve recovery and obtain its historical data; no new payment");
                }
                if outcome == Outcome::RejectedAgain { return Err(finalized(137)); }
                if outcome == Outcome::ExpiredSubmission {
                    assert_eq!(state.deposit_voucher, original);
                    assert!(state.deploy_submission.is_some());
                }
                // A 403 retains the saved proof and submission until the existing bounded loop
                // receives its exact rejection, rather than entering the invalid-proof path.
                if outcome == Outcome::ExpiredHistory {
                    let mut rejection = Some(finalized(403));
                    let proof = state.deposit_voucher.as_ref().unwrap().proof.as_ref().unwrap().to_halo2();
                    note_deploy_run_reproof_loop(proof,
                        async |proof| {
                            note_deploy_submit_proof_once(
                                proof,
                                async |_| Ok(false),
                                async |_| if let Some(error) = rejection.take() { Err(error) } else { Ok(()) },
                            ).await
                        },
                        async |rejected| {
                            note_deploy_persist_rejected_proof(path, state, NoteDeployVoucherKind::Deposit, rejected)?;
                            let mut next = rejected.clone();
                            next.layer_number += 1;
                            next.proof = "same-voucher-next-history-layer".into();
                            state.deposit_voucher.as_mut().unwrap().replace_rejected_proof(NoteDeployVoucherProof::from_halo2(&next))?;
                            note::write_note_deploy_recovery(path, state)?;
                            Ok(next)
                        }).await?;
                }
                if state.pn_address.is_none() {
                    // Chain mock supplies the completion of the same paid operation. No wallet
                    // reader, BOC builder or funding key is part of this recovery seam.
                    let address = state.deploy_submission.as_ref().map(|s| s.expected_note_address.clone())
                        .unwrap_or_else(|| format!("{}::{}", "0".repeat(63) + "4", "7".repeat(64)));
                    state.mark_private_note_deployed(address, "1".repeat(64), 1)?;
                }
                state.mark_shell_funded_and_checked()?;
                note::write_note_deploy_recovery(path, state)?;
                state.to_onboard_state()
            }).await
    }
    async fn finalize_pool(
        &mut self,
        recovery: &NoteDeployRecoveryState,
        state: &note::OnboardPnState,
    ) -> Result<()> {
        self.finalizations.set(self.finalizations.get() + 1);
        let owner = if self.outcome == Outcome::ForeignOwner {
            dexdo_core::KeyPair::generate().public_hex().to_owned()
        } else {
            recovery.owner_public_key_hex.clone()
        };
        note::ensure_recovery_owner_matches_target_note(&self.path, recovery, Some(&owner))?;
        note_recover_finalize_pool(&self.path, &self.pool, recovery, state)?;
        Ok(())
    }
}

fn recorded(mut state: NoteDeployRecoveryState, complete: bool) -> NoteDeployRecoveryState {
    let address = state
        .deploy_submission
        .as_ref()
        .unwrap()
        .expected_note_address
        .clone();
    state
        .mark_private_note_deployed(address, "1".repeat(64), 1)
        .unwrap();
    if complete {
        state.mark_shell_funded_and_checked().unwrap();
    }
    state
}

#[tokio::test]
async fn recover_orchestration_resumes_all_paid_stages_without_fresh_funding() {
    for stage in 0..7 {
        let dir = tempfile::tempdir().unwrap();
        let mut state = paid_recovery();
        match stage {
            0 => {
                state.deploy_submission = None;
                let v = state.deposit_voucher.as_mut().unwrap();
                v.proof = None;
                v.event = None;
            }
            1 => {
                state.deploy_submission = None;
                state.deposit_voucher.as_mut().unwrap().proof = None;
            }
            2 => {
                state.deploy_submission = None;
            } // Saved replacement before its first POST.
            3 => {} // Submitted proof, active note found by confirmation.
            4 => {
                state = recorded(state, false);
            }
            5 => {
                state = recorded(state, true);
            }
            6 => {
                state.deposit_voucher.as_mut().unwrap().event = None;
            }
            _ => unreachable!(),
        }
        let owner = state.owner_public_key_hex.clone();
        let original = state.deposit_voucher.clone().unwrap();
        let mut ops = RecoverOps::new(dir.path(), &state, Outcome::Success);
        run_note_recover_resolved(&mut ops).await.unwrap();
        assert_eq!(ops.reproves.get(), 0, "stage {stage}");
        assert_eq!(ops.confirms.get(), usize::from(stage == 3));
        assert!(!ops.path.exists());
        let pooled: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ops.pool).unwrap()).unwrap();
        assert_eq!(pooled["notes"].as_array().unwrap().len(), 1);
        assert_eq!(pooled["notes"][0]["owner_public_key_hex"], owner);
        assert!(original.submit_maybe_sent);
        assert_ne!(
            ops.observed_route.get(),
            Some(NoteDeployFundingRoute::FreshSpend)
        );
    }
}

#[tokio::test]
async fn recover_orchestration_automatically_reproves_137_once_and_403_uses_history_loop() {
    for outcome in [
        Outcome::InvalidProof,
        Outcome::ExpiredHistory,
        Outcome::ExpiredSubmission,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut ops = RecoverOps::new(dir.path(), &paid_recovery(), outcome);
        run_note_recover_resolved(&mut ops).await.unwrap();
        assert_eq!(ops.confirms.get(), 1);
        assert_eq!(
            ops.reproves.get(),
            usize::from(outcome == Outcome::InvalidProof)
        );
        assert_eq!(ops.resumes.get(), 1);
        assert_eq!(ops.finalizations.get(), 1);
        assert!(!ops.path.exists());
    }
}

#[tokio::test]
async fn recover_orchestration_preserves_ambiguous_failed_or_unusable_paid_evidence() {
    for outcome in [
        Outcome::Ambiguous,
        Outcome::ReproveUnavailable,
        Outcome::HistoryUnavailable,
        Outcome::InvalidReplacement,
        Outcome::RejectedAgain,
        Outcome::ForeignOwner,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut ops = RecoverOps::new(dir.path(), &paid_recovery(), outcome);
        let before = std::fs::read(&ops.path).unwrap();
        let error = run_note_recover_resolved(&mut ops).await.unwrap_err();
        assert!(ops.path.exists(), "{error:#}");
        assert!(!ops.pool.exists());
        if [
            Outcome::Ambiguous,
            Outcome::ReproveUnavailable,
            Outcome::HistoryUnavailable,
            Outcome::InvalidReplacement,
        ]
        .contains(&outcome)
        {
            assert_eq!(std::fs::read(&ops.path).unwrap(), before);
        }
        assert!(ops.reproves.get() <= 1);
        if outcome == Outcome::Ambiguous {
            assert_eq!(ops.resumes.get(), 0);
        }
        if outcome == Outcome::RejectedAgain {
            assert_eq!(ops.reproves.get(), 1);
        }
    }
}

#[tokio::test]
async fn recover_orchestration_refuses_unspent_and_invalid_owner_before_network() {
    for invalid in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let mut state = paid_recovery();
        if invalid < 2 {
            state.deploy_submission = None;
            state.deposit_voucher = None;
            if invalid == 1 {
                state.funding_multisig_address = None;
            }
        }
        let mut ops = RecoverOps::new(dir.path(), &state, Outcome::Success);
        if invalid == 2 {
            let mut json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&ops.path).unwrap()).unwrap();
            json["owner_secret_key_hex"] = dexdo_core::KeyPair::generate().secret_hex().into();
            // Corrupt an existing private file without changing its mode.
            std::fs::write(&ops.path, serde_json::to_vec(&json).unwrap()).unwrap();
        }
        let before = std::fs::read(&ops.path).unwrap();
        run_note_recover_resolved(&mut ops).await.unwrap_err();
        assert_eq!(ops.checks.get(), 0);
        assert_eq!(ops.resumes.get(), 0);
        assert_eq!(ops.observed_route.get(), None);
        assert_eq!(std::fs::read(&ops.path).unwrap(), before);
    }
}

#[tokio::test]
async fn recover_restarts_after_pool_write_without_appending_a_second_note() {
    let dir = tempfile::tempdir().unwrap();
    let state = recorded(paid_recovery(), true);
    let mut ops = RecoverOps::new(dir.path(), &state, Outcome::Success);
    note_deploy_fold_state_into_pool(&ops.pool, &state.to_onboard_state().unwrap()).unwrap();
    let before = std::fs::read(&ops.pool).unwrap();
    run_note_recover_resolved(&mut ops).await.unwrap();
    assert_eq!(std::fs::read(&ops.pool).unwrap(), before);
    assert!(!ops.path.exists());
}

#[tokio::test]
async fn recover_retains_journal_when_pool_write_fails_or_has_foreign_key() {
    for foreign in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let state = recorded(paid_recovery(), true);
        let mut ops = RecoverOps::new(dir.path(), &state, Outcome::Success);
        if foreign {
            let mut onboard = state.to_onboard_state().unwrap();
            let other = dexdo_core::KeyPair::generate();
            onboard.owner_public_key_hex = Some(other.public_hex().into());
            onboard.owner_secret_key_hex = Some(other.secret_hex().to_owned().into());
            note_deploy_fold_state_into_pool(&ops.pool, &onboard).unwrap();
        } else {
            crate::cli::support::write_owner_only_key_fixture(&ops.pool, "invalid JSON");
        }
        let pool = std::fs::read(&ops.pool).unwrap();
        run_note_recover_resolved(&mut ops).await.unwrap_err();
        assert!(ops.path.exists());
        assert_eq!(std::fs::read(&ops.pool).unwrap(), pool);
    }
}

#[test]
fn recover_endpoint_mismatch_and_funding_key_access_are_refused() {
    let state = paid_recovery();
    assert!(note_recover_require_endpoint(&state, "http://other-network.example").is_err());
    assert!(note_recover_require_endpoint(&state, &state.endpoint).is_ok());
    assert!(NoteRecoverNoFunding.load_funding_wallet_keys().is_err());
}

proptest::proptest! {
    #[test]
    fn every_recoverable_stage_excludes_the_fresh_spend_route(
        submitted in proptest::prelude::any::<bool>(), event in proptest::prelude::any::<bool>(),
        proof in proptest::prelude::any::<bool>(), recorded_note in proptest::prelude::any::<bool>(),
    ) {
        let mut state = paid_recovery();
        state.deploy_submission = None;
        if !proof { state.deposit_voucher.as_mut().unwrap().proof = None; }
        if !event { state.deposit_voucher.as_mut().unwrap().event = None; }
        state.deposit_voucher.as_mut().unwrap().submit_maybe_sent = submitted;
        if recorded_note { state.mark_private_note_deployed(format!("0:{}", "7".repeat(64)), "1".repeat(64), 1).ok(); }
        if note_recover_require_paid(&state).is_ok() {
            proptest::prop_assert_ne!(note_deploy_funding_route(&state), NoteDeployFundingRoute::FreshSpend);
            proptest::prop_assert!(state.pn_address.is_some() || state.deposit_voucher.as_ref().unwrap().submit_maybe_sent);
        }
    }
}

#[tokio::test]
async fn recover_shared_paid_stage_enters_event_wait_without_wallet_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paid.recovery.json");
    let mut state = paid_recovery();
    state.deploy_submission = None;
    let voucher = state.deposit_voucher.as_mut().unwrap();
    voucher.event = None;
    voucher.proof = None;
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let before = std::fs::read(&path).unwrap();
    let client = dexdo_core::ChainClient::connect(&state.endpoint).unwrap();
    let owner = dexdo_core::KeyPair::from_secret_hex(&state.owner_secret_key_hex).unwrap();
    let wallet = state.funding_multisig_address.clone().unwrap();
    let paths = dexdo_core::private_note::Halo2Paths::from_env();
    let error = resume_note_deploy_chain(
        &client,
        &path,
        &mut state,
        &wallet,
        &NoteRecoverNoFunding,
        &owner,
        &paths,
        NoteDeployVoucherFailpoints {
            before_voucher_event_wait: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("simulated interruption before voucher event wait"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn recover_production_completed_stage_uses_no_wallet_or_prover() {
    let dir = tempfile::tempdir().unwrap();
    let state = recorded(paid_recovery(), true);
    let path = dir.path().join("complete.recovery.json");
    let pool = dir.path().join("pool.json");
    note::write_note_deploy_recovery(&path, &state).unwrap();
    let client = dexdo_core::ChainClient::connect(&state.endpoint).unwrap();
    let paths = dexdo_core::private_note::Halo2Paths::from_env();
    let mut ops = NoteRecoverProductionOps {
        client: &client,
        manifest: None,
        recovery_path: &path,
        pool_path: &pool,
        halo2_paths: &paths,
    };
    let mut loaded = ops.load_recovery().await.unwrap();
    let before = std::fs::read(&path).unwrap();
    let onboard = ops.resume_chain(&mut loaded).await.unwrap();
    assert_eq!(
        dexdo_core::CanonicalAddress::parse(onboard.pn_address.as_deref().unwrap()).unwrap(),
        dexdo_core::CanonicalAddress::parse(state.pn_address.as_deref().unwrap()).unwrap()
    );
    assert_eq!(
        onboard.owner_public_key_hex.as_deref(),
        Some(state.owner_public_key_hex.as_str())
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!pool.exists());
}
