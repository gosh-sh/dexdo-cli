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

#[tokio::test(start_paused = true)]
async fn issue_2206_open_confirmation_preserves_the_budget_after_bk_exhaustion() {
    let tc = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
    let reads = AtomicUsize::new(0);
    let sdk_reads = AtomicUsize::new(0);
    let max_reads = crate::params::TC_BOOL_CONFIRM_MAX_READS;
    wait_tc_bool_with(&tc, "opened", true, || async {
        let attempt = reads.fetch_add(1, Ordering::Relaxed);
        if attempt == 0 {
            let error = super::super::client::retry_transient_read(|| async {
                sdk_reads.fetch_add(1, Ordering::Relaxed);
                Err::<(), _>(super::backends_issue_2122_tests::bk_error())
            })
            .await
            .unwrap_err();
            return Err(map_read_err(error));
        }
        Ok(Some(DealChainState {
            opened: attempt == max_reads,
            ..pending()
        }))
    })
    .await
    .unwrap();
    assert_eq!(reads.load(Ordering::Relaxed), max_reads + 1);
    assert_eq!(
        sdk_reads.load(Ordering::Relaxed),
        crate::params::TRANSIENT_READ_ATTEMPTS
    );
}
