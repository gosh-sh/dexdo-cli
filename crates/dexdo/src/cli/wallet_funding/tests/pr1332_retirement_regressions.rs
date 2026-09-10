use super::*;

fn assert_pending_retirement_refusal(error: anyhow::Error) {
    let message = format!("{error:#}");
    assert!(
        message.contains(
            "refusing to retire funding generation 1 while Vault queue transaction pending may \
             still execute"
        ),
        "the refusal must name the generation and live queue id: {message}"
    );
}

async fn accepted_request_then_refusal(
    second_presence: RequestPresence,
) -> (tempfile::TempDir, FakeProvider) {
    let dir = temp();
    let active = binding(WalletProvider::AckinackiWallet);

    let first_chain = FakeChain::always(0);
    let first_provider = FakeProvider::ackinacki(RequestPresence::Absent);
    let first_error = run(
        dir.path(),
        &active,
        &first_chain,
        &first_provider,
        tight_bounds(),
    )
    .await
    .expect_err("the accepted request has not funded the Hot yet");
    assert!(first_error
        .chain()
        .any(|cause| cause.to_string().contains("timed out")));
    let pending = record(dir.path()).expect("the timeout retains the accepted request");
    assert_eq!(pending.state, FundingState::Submitted);
    assert_eq!(pending.pending_transaction_id.as_deref(), Some("pending"));

    let funded_chain = FakeChain::always(1_000);
    let second_provider = FakeProvider::ackinacki(second_presence);
    let error = run(
        dir.path(),
        &active,
        &funded_chain,
        &second_provider,
        patient_bounds(),
    )
    .await
    .expect_err("a sufficient balance cannot retire a request without finalized evidence");
    assert_pending_retirement_refusal(error);
    (dir, second_provider)
}

/// The unsafe fixture formerly embedded in
/// `the_journal_closes_only_on_an_observed_balance_that_meets_the_requirement`: an accepted submit
/// followed by `Absent`, with no finalized verdict. The recorded request may still execute, so the
/// journal stays open and no replacement is submitted.
#[tokio::test]
async fn an_accepted_submit_then_absent_without_evidence_refuses_retirement() {
    let (dir, provider) = accepted_request_then_refusal(RequestPresence::Absent).await;

    assert_eq!(provider.probes.get(), 1);
    assert_eq!(provider.submits.get(), 0);
    let pending = record(dir.path()).expect("the refused retirement retains the request");
    assert_eq!(pending.state, FundingState::Submitted);
    assert_eq!(pending.pending_transaction_id.as_deref(), Some("pending"));
    assert!(pending.evidence.is_none());
}

/// The pending half of `a_timeout_leaves_a_state_that_a_rerun_re_checks`: the rerun does re-check,
/// learns that the timed-out request is still present, and refuses to forget it even though another
/// route has made the Hot balance sufficient.
#[tokio::test]
async fn a_timeout_then_present_without_evidence_refuses_retirement() {
    let (dir, provider) = accepted_request_then_refusal(RequestPresence::Present {
        transaction_hash: None,
        pending_transaction_id: Some("pending".to_string()),
        chain_created_at_unix: None,
    })
    .await;

    assert_eq!(provider.probes.get(), 1);
    assert_eq!(provider.submits.get(), 0);
    let pending = record(dir.path()).expect("the still-present request remains recorded");
    assert_eq!(pending.state, FundingState::Submitted);
    assert_eq!(pending.pending_transaction_id.as_deref(), Some("pending"));
    assert!(pending.evidence.is_none());
}
