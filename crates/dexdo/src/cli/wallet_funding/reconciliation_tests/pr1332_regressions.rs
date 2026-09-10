use super::*;

/// A balance observation says nothing about a request that is still confirmable in the Vault.
/// A later, different shortfall gets its own request without retiring the earlier queue id.
#[tokio::test]
async fn a_sufficient_balance_does_not_retire_a_still_pending_vault_request() {
    let dir = temp();
    let vault = FakeVault::empty();

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(
        vault.submits.get(),
        1,
        "the first run creates queue transaction 7"
    );
    let first = record_of(dir.path()).expect("run 1 left a record");
    assert_eq!(first.pending_transaction_id.as_deref(), Some("7"));

    // The Hot is funded by some other route while transaction 7 remains live. Make the queue
    // unreadable to prove that the sufficient-balance path probes before it considers retirement.
    *vault.queue_error.borrow_mut() = Some("queue temporarily unavailable".to_string());
    let hot = FakeHot::always(REQUIRED);
    let error = money_command_run(dir.path(), &vault, &hot)
        .await
        .expect_err("an unprobed live queue id must keep the money command from continuing");
    let message = error.to_string();
    assert!(
        message.contains(
            "refusing to retire funding generation 1 while Vault queue transaction 7 may still \
             execute"
        ) && message.contains("pending list"),
        "the refusal must name the transfer the operator has to settle: {message}"
    );
    assert_eq!(vault.submits.get(), 1, "the failed probe submits nothing");

    // Later the Hot has spent part of that direct top-up. Its new shortfall no longer matches the
    // frozen transfer, so it gets request 8 while queue id 7 remains independently confirmable.
    *vault.queue_error.borrow_mut() = None;
    let hot = FakeHot::always(400);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        2,
        "a different amount must create a second request"
    );
    let after = records_of(dir.path());
    assert_eq!(after.len(), 2, "both live amounts remain recorded");
    assert_eq!(after[0].state, FundingState::Submitted);
    assert_eq!(after[0].generation, 1);
    assert_eq!(after[0].pending_transaction_id.as_deref(), Some("7"));
    assert_eq!(after[1].state, FundingState::Submitted);
    assert_eq!(after[1].generation, 2);
    assert_eq!(after[1].pending_transaction_id.as_deref(), Some("8"));
}

/// An unrelated old `TransactionSubmitted` does not duplicate a still-live exact journal request.
#[tokio::test]
async fn an_older_submitted_event_cannot_date_an_indeterminate_generation() {
    let dir = temp();
    let vault = FakeVault::empty();
    vault
        .history
        .borrow_mut()
        .push(submitted_event(101, 0, QUEUED_AT));
    vault.indeterminate.set(true);

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1);
    let first = record_of(dir.path()).expect("the indeterminate submit remains prepared");
    assert_eq!(first.state, FundingState::Prepared);
    assert_eq!(first.generation, 1);
    assert!(first.pending_transaction_id.is_none());
    assert!(
        first.submit_attempted,
        "the crash-safe boundary is durable before an unresolved submit returns"
    );

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;

    assert_eq!(
        vault.submits.get(),
        1,
        "an unrelated event cannot authorize a duplicate of the same live amount"
    );
    let after = record_of(dir.path()).expect("the unresolved generation remains recorded");
    assert_eq!(after.state, FundingState::Prepared);
    assert_eq!(after.generation, 1);

    // Let the balance arrive on a final run so the real entry point returns the provider's reason.
    let hot = FakeHot::then_always(vec![0], REQUIRED);
    let funded = money_command_run_with(dir.path(), &vault, &hot, patient_bounds())
        .await
        .expect("the independently arriving balance lets the command return its refusal");
    let FundingNotice::RequestIndeterminate { reason } = funded.notice else {
        panic!(
            "the unrelated event must leave this generation indeterminate: {:?}",
            funded.notice
        );
    };
    assert!(
        reason.contains("generation 1") && reason.contains("local funding journal until"),
        "the refusal must name the live generation and its deadline: {reason}"
    );
    assert_eq!(vault.submits.get(), 1);
}

/// A receipt-less submit can have reached the Vault even though this client learned no queue id.
/// An independently arriving balance may let the money command continue, but it must not erase the
/// unresolved generation; a later, different shortfall gets a separate request without erasing it.
#[tokio::test]
async fn a_sufficient_balance_keeps_an_indeterminate_no_id_generation_visible() {
    let dir = temp();
    let vault = FakeVault::empty();
    vault.indeterminate.set(true);

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1);
    let first = record_of(dir.path()).expect("the indeterminate submit remains recorded");
    assert_eq!(first.state, FundingState::Prepared);
    assert!(first.pending_transaction_id.is_none());
    assert!(first.evidence.is_none());

    let hot = FakeHot::then_always(vec![0], REQUIRED);
    let funded = money_command_run_with(dir.path(), &vault, &hot, patient_bounds())
        .await
        .expect("an independently funded Hot may let the command continue");
    assert!(matches!(
        funded.notice,
        FundingNotice::RequestIndeterminate { .. }
    ));
    drop(funded);
    let retained = record_of(dir.path()).expect("the unresolved generation stays visible");
    assert_eq!(retained.state, FundingState::Prepared);
    assert!(retained.pending_transaction_id.is_none());
    assert!(retained.evidence.is_none());

    let hot = FakeHot::always(400);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(
        vault.submits.get(),
        2,
        "a later, different shortfall must get its own transfer"
    );
    let records = records_of(dir.path());
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].state, FundingState::Prepared);
    assert_eq!(records[0].generation, 1);
    assert_eq!(records[1].state, FundingState::Prepared);
    assert_eq!(records[1].generation, 2);
}

/// A no-id history fallback is intentionally allowed to find `Sent`, because even a false match
/// forbids a submit. It is not generation evidence, though, so it cannot authorize journal
/// retirement; and once the real queue entry becomes visible its stale evidence must be cleared.
#[tokio::test]
async fn a_fallback_sent_event_never_retires_an_indeterminate_generation() {
    let dir = temp();
    let vault = FakeVault::empty();
    vault.history.borrow_mut().extend([
        submitted_event(101, 0, QUEUED_AT),
        sent_event(101, 0, QUEUED_AT + 1),
    ]);
    vault.indeterminate.set(true);

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1);

    let hot = FakeHot::then_always(vec![0], REQUIRED);
    let funded = money_command_run_with(dir.path(), &vault, &hot, patient_bounds())
        .await
        .expect("the independently funded Hot may continue without retiring the generation");
    assert!(matches!(
        funded.notice,
        FundingNotice::RequestExecuted { .. }
    ));
    drop(funded);
    let fallback = record_of(dir.path()).expect("the no-id generation stays visible");
    assert_eq!(fallback.state, FundingState::Executed);
    assert!(fallback.pending_transaction_id.is_none());
    assert!(fallback.evidence.is_some());

    let hot = FakeHot::always(400);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(
        vault.submits.get(),
        2,
        "fallback evidence for one amount must not block a distinct amount"
    );
    let after_second = records_of(dir.path());
    assert_eq!(after_second.len(), 2);
    assert_eq!(after_second[0].state, FundingState::Executed);
    assert_eq!(after_second[0].generation, 1);
    assert_eq!(after_second[1].state, FundingState::Prepared);
    assert_eq!(after_second[1].generation, 2);

    // The current generation becomes visible after the fallback read. `Present` is authoritative
    // about its liveness and must erase the unrelated execution evidence before close is attempted.
    vault.queue.borrow_mut().push(queued(7, 0, REQUIRED));
    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    let pending = records_of(dir.path());
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].state, FundingState::Submitted);
    assert_eq!(pending[0].pending_transaction_id.as_deref(), Some("7"));
    assert!(pending[0].evidence.is_none());
    assert_eq!(pending[1].state, FundingState::Prepared);
    assert_eq!(vault.submits.get(), 2);
}

/// A persisted queue id that cannot be parsed as the Vault's `uint64` id is not a known id. A
/// generation-invariant history fallback may still conservatively find execution, but that
/// fallback cannot turn the malformed id into proof for this generation or retire its record.
#[tokio::test]
async fn a_malformed_pending_transaction_id_is_refused_rather_than_retired() {
    let dir = temp();
    let vault = FakeVault::empty();
    *vault.reported_pending_id.borrow_mut() = Some("not-a-queue-id".to_string());

    let hot = FakeHot::always(0);
    let _ = money_command_run(dir.path(), &vault, &hot).await;
    assert_eq!(vault.submits.get(), 1);
    let submitted = record_of(dir.path()).expect("the accepted request remains recorded");
    assert_eq!(submitted.state, FundingState::Submitted);
    assert_eq!(
        submitted.pending_transaction_id.as_deref(),
        Some("not-a-queue-id")
    );

    // The real queue id 7 left the queue and has finalized execution evidence. Because the
    // journal's malformed string cannot identify id 7, this is only a conservative fallback match.
    vault.queue.borrow_mut().clear();
    vault
        .history
        .borrow_mut()
        .push(sent_event(7, 0, QUEUED_AT + 1));

    let hot = FakeHot::always(REQUIRED);
    let error = money_command_run(dir.path(), &vault, &hot)
        .await
        .expect_err("a malformed queue id must refuse retirement");
    let message = error.to_string();
    assert!(
        message.contains(
            "refusing to retire funding generation 1 while Vault queue transaction \
             not-a-queue-id may still execute"
        ),
        "the refusal must name the malformed recorded id: {message}"
    );
    assert_eq!(vault.submits.get(), 1, "the refusal submits nothing");

    let retained = record_of(dir.path()).expect("the refused record remains visible");
    assert_eq!(retained.state, FundingState::Executed);
    assert_eq!(
        retained.pending_transaction_id.as_deref(),
        Some("not-a-queue-id")
    );
    assert!(retained.evidence.is_some());
    assert!(retained.satisfied_balances.is_none());
}
