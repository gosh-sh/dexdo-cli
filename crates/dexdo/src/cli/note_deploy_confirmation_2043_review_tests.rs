//! Review regressions for independent account polling while a receipt read is pending.
use super::*;
use std::cell::Cell;

#[tokio::test(start_paused = true)]
async fn a_stalled_receipt_does_not_stop_polling_after_an_absent_note() {
    let active_reads = Cell::new(0);
    let receipt_reads = Cell::new(0);
    let start = tokio::time::Instant::now();
    note_deploy_wait_confirmation_with(
        "expected-note",
        std::path::Path::new("recovery.json"),
        None,
        async || {
            receipt_reads.set(receipt_reads.get() + 1);
            std::future::pending::<
                anyhow::Result<Option<dexdo_core::chain::NoteDeployRootPnActionObservation>>,
            >()
            .await
        },
        async || {
            active_reads.set(active_reads.get() + 1);
            Ok(active_reads.get() > 1)
        },
    )
    .await
    .expect("later account activation must complete despite a stalled receipt");
    assert_eq!(active_reads.get(), 2);
    assert_eq!(receipt_reads.get(), 1, "keep one receipt request in flight");
    assert_eq!(start.elapsed(), NOTE_DEPLOY_ACTIVE_POLL_INTERVAL);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_receipt_does_not_stop_polling_after_a_note_read_error() {
    let active_reads = Cell::new(0);
    let start = tokio::time::Instant::now();
    note_deploy_wait_confirmation_with(
        "expected-note",
        std::path::Path::new("recovery.json"),
        None,
        async || {
            std::future::pending::<
                anyhow::Result<Option<dexdo_core::chain::NoteDeployRootPnActionObservation>>,
            >()
            .await
        },
        async || {
            active_reads.set(active_reads.get() + 1);
            if active_reads.get() == 1 {
                anyhow::bail!("account read temporarily unavailable");
            }
            Ok(true)
        },
    )
    .await
    .expect("a transient account error must not suppress its next read");
    assert_eq!(active_reads.get(), 2);
    assert_eq!(start.elapsed(), NOTE_DEPLOY_ACTIVE_POLL_INTERVAL);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_receipt_keeps_the_shared_deadline_and_refuses_resubmit() {
    let active_reads = Cell::new(0u64);
    let start = tokio::time::Instant::now();
    let error = note_deploy_wait_confirmation_with(
        "expected-note",
        std::path::Path::new("recovery.json"),
        None,
        async || {
            std::future::pending::<
                anyhow::Result<Option<dexdo_core::chain::NoteDeployRootPnActionObservation>>,
            >()
            .await
        },
        async || {
            active_reads.set(active_reads.get() + 1);
            Ok(false)
        },
    )
    .await
    .expect_err("a stalled receipt and absent note cannot confirm deployment");
    assert_eq!(start.elapsed(), NOTE_DEPLOY_ACTIVE_TIMEOUT);
    assert_eq!(
        active_reads.get(),
        NOTE_DEPLOY_ACTIVE_TIMEOUT.as_secs() / NOTE_DEPLOY_ACTIVE_POLL_INTERVAL.as_secs()
    );
    assert!(
        !error
            .downcast_ref::<NoteDeployUnconfirmed>()
            .unwrap()
            .may_resubmit
    );
    assert!(!note_deploy_has_exact_finalized_rootpn_exit_code(
        &error, 403
    ));
}
