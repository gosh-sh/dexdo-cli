use super::*;
use proptest::prelude::*;

#[tokio::test]
async fn transient_read_2129_does_not_make_missing_or_invalid_facts_retryable() {
    for invalid in [false, true] {
        let chain = Arc::new(RecordingSettleChain::default());
        chain.fail_deal_state.store(invalid, Ordering::SeqCst);
        let session = SessionSettle::new(
            chain.clone(),
            "tc-2129-invalid".into(),
            Arc::new(dexdo_core::LocalNote::generate()),
        );
        let error = session.ensure_open_for_serving().await.unwrap_err();
        assert_eq!(error.code, ConsumerErrorCode::DealUnavailable);
        assert!(session.is_closed());
        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
        session.disable_drop_backup();
    }
}

proptest! {
    #[test]
    fn transient_read_2129_never_authorizes_an_unreadable_deal(
        failures in prop::collection::vec(any::<bool>(), 1..24)
    ) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let chain = Arc::new(RecordingSettleChain::default());
            chain.set_deal_state(opened_deal_state(TEST_DEPOSIT, unix_now_secs()));
            let session = SessionSettle::new(chain.clone(), "tc-2129".into(),
                Arc::new(dexdo_core::LocalNote::generate()));
            for unavailable in failures {
                chain.transient_deal_state.store(unavailable, Ordering::SeqCst);
                assert_eq!(session.ensure_open_for_serving().await.is_err(), unavailable);
                assert!(!session.is_closed());
                assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
                assert_eq!(chain.recovery_stop_calls.load(Ordering::SeqCst), 0);
                assert_eq!(chain.cleanup_unopened_calls.load(Ordering::SeqCst), 0);
                assert_eq!(chain.dispute_calls.load(Ordering::SeqCst), 0);
            }
            session.disable_drop_backup();
        });
    }
}

#[tokio::test]
async fn transient_read_2129_does_not_reopen_an_independently_closed_session() {
    let chain = Arc::new(RecordingSettleChain::default());
    chain.set_deal_state(opened_deal_state(TEST_DEPOSIT, unix_now_secs()));
    let session = SessionSettle::new(
        chain.clone(),
        "tc-2129-closed".into(),
        Arc::new(dexdo_core::LocalNote::generate()),
    );
    session.close_local_api(ClosureCategory::DeadGateway, Some("fail_closed"));
    chain.transient_deal_state.store(true, Ordering::SeqCst);
    assert!(session.ensure_open_for_serving().await.is_err());
    assert!(session.is_closed());
    assert!(session.closed_rejection("retry", false).is_some());
    session.disable_drop_backup();
}
