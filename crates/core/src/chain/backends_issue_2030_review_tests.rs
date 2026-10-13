use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn pending() -> DealChainState {
    DealChainState {
        funded: true,
        opened: false,
        probe_accepted: false,
        disputed: false,
        deposit: crate::params::PRICE_STEP,
        probe_tick: 0,
        finalized_owed: 0,
        tokens_final: 0,
        tokens_pending: 0,
        funded_time: Some(1),
        probe_time: 0,
        last_claim_time: 0,
        dispute_time: 0,
    }
}

#[tokio::test(start_paused = true)]
async fn accepted_open_confirmation_exhaustion_is_unconfirmed_not_contract_refusal() {
    let tc = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let reads = AtomicUsize::new(0);
    let error = wait_tc_bool_with(&tc, "opened", true, || async {
        reads.fetch_add(1, Ordering::Relaxed);
        Ok(Some(pending()))
    })
    .await
    .unwrap_err();
    assert!(
        matches!(error, ChainError::OpenUnconfirmed(ref actual) if actual == &tc.with_workchain())
    );
    assert_eq!(
        reads.load(Ordering::Relaxed),
        crate::params::TC_BOOL_CONFIRM_MAX_READS
    );
}

#[tokio::test(start_paused = true)]
async fn confirmation_keeps_genuine_errors_and_other_bool_timeouts() {
    let tc = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let error = wait_tc_bool_with(&tc, "opened", true, || async {
        Err(ChainError::Contract("getState contract refusal".into()))
    })
    .await
    .unwrap_err();
    assert!(matches!(error, ChainError::Contract(_)));
    let error = wait_tc_bool_with(&tc, "probeAccepted", true, || async { Ok(Some(pending())) })
        .await
        .unwrap_err();
    assert!(matches!(error, ChainError::Chain(_)));
}
