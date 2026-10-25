//! Durable Vault execution identity must outlive the node's history retention window.

use super::*;

fn legacy_record(queue_id: Option<u64>, evidence_id: u64) -> FundingJournalRecord {
    let requirements = requirements();
    let request = FundingRequest {
        provider: WalletProvider::AckinackiWallet,
        network: "net-a".into(),
        vault_address: Some(vault_address()),
        hot_address: hot_address(),
        hot_dapp_id: hex64(0xa1),
        creator_pubkey: creator(),
        required: requirements.required.clone(),
        required_native: requirements.required_native,
        shortfall: requirements.required.clone(),
        native_shortfall: 0,
    };
    let mut record = FundingJournalRecord::open_generation(&request, QUEUED_AT, 5);
    record.state = FundingState::Executed;
    record.submit_attempted = true;
    record.pending_transaction_id = queue_id.map(|id| id.to_string());
    record.evidence = Some(FundingEvidence {
        queue_transaction_id: None,
        verdict: "executed".into(),
        source: format!("finalized ext-out message {}", hex64(0x8a)),
        observed_at_unix: Some(QUEUED_AT + 1),
        detail: format!(
            "the Vault emitted TransactionSent for queue transaction {evidence_id} to {} at chain time {}",
            hot_address(), QUEUED_AT + 1
        ),
        delivery_message_id: None,
    });
    record
}

async fn unrelated_unbound_candidate_yields_to_actual_execution(production_fallback: bool) {
    let dir = temp();
    let vault = FakeVault::empty();
    vault.delivery_available.set(false);
    let mut unrelated_sent = sent_event(99, 0, QUEUED_AT + 3);
    unrelated_sent.message_id = hex64(0x99);
    if production_fallback {
        let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
        let mut submitted = record_of(dir.path()).unwrap();
        submitted.pending_transaction_id = None;
        unrelated_sent.value = submitted.fingerprint.value;
        store_funding_journal(dir.path(), &submitted).unwrap();
        vault.queue.borrow_mut().clear();
        *vault.history.borrow_mut() = vec![
            submitted_event(7, submitted.fingerprint.value, QUEUED_AT),
            submitted_event(99, submitted.fingerprint.value, QUEUED_AT + 2),
            unrelated_sent.clone(),
        ];
        *vault.submit_identity_error.borrow_mut() = Some("temporarily unavailable".into());
        let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
        *vault.submit_identity_error.borrow_mut() = None;
    } else {
        let mut legacy = legacy_record(None, 99);
        legacy.transaction_hash = Some("tx-7".into());
        store_funding_journal(dir.path(), &legacy).unwrap();
    }
    let fallback = record_of(dir.path()).unwrap();
    let old_evidence = fallback.evidence.as_ref().unwrap();
    assert_eq!(fallback.state, FundingState::Executed);
    assert!(fallback.pending_transaction_id.is_none());
    assert!(old_evidence.queue_transaction_id.is_none());
    assert!(old_evidence.detail.contains("queue transaction 99 to "));
    let submits = vault.submits.get();

    // F1: retain our independently proven ID even while only admission is visible.
    let admission = submitted_event(7, fallback.fingerprint.value, QUEUED_AT);
    *vault.history.borrow_mut() = vec![admission.clone()];
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let identified = record_of(dir.path()).unwrap();
    assert_eq!(identified.pending_transaction_id.as_deref(), Some("7"));
    assert_eq!(identified.state, FundingState::Executed);
    assert_eq!(identified.evidence, fallback.evidence);
    assert_eq!(identified.expires_at_unix, fallback.expires_at_unix);

    // Another request's execution and a sufficient balance cannot close this generation.
    *vault.history.borrow_mut() = vec![admission.clone(), unrelated_sent];
    assert!(
        money_command_run(dir.path(), &vault, &FakeHot::always(REQUIRED))
            .await
            .is_err()
    );
    let still_unbound = record_of(dir.path()).unwrap();
    assert_eq!(still_unbound.evidence, fallback.evidence);
    assert_eq!(still_unbound.pending_transaction_id.as_deref(), Some("7"));
    assert_eq!(still_unbound.state, FundingState::Executed);
    assert_eq!(vault.submits.get(), submits);

    // F2: the old candidate must not veto fresh execution proof for the confirmed ID.
    let mut actual_sent = sent_event(7, fallback.fingerprint.value, QUEUED_AT + 1);
    actual_sent.message_id = hex64(0x07);
    *vault.history.borrow_mut() = vec![admission, actual_sent.clone()];
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let executed = record_of(dir.path()).unwrap();
    let evidence = executed.evidence.as_ref().unwrap();
    assert_eq!(evidence.queue_transaction_id, Some(7));
    assert_eq!(
        evidence.source,
        format!("finalized ext-out message {}", actual_sent.message_id)
    );
    assert!(evidence.detail.contains("queue transaction 7 to "));
    assert!(evidence.delivery_message_id.is_none());
    assert_eq!(executed.pending_transaction_id.as_deref(), Some("7"));
    assert_eq!(vault.submits.get(), submits);
    drop(
        money_command_run(dir.path(), &vault, &FakeHot::always(REQUIRED))
            .await
            .unwrap(),
    );
    assert_eq!(record_of(dir.path()).unwrap().state, FundingState::Executed);
    assert_eq!(
        vault.submits.get(),
        submits,
        "execution alone is not Hot delivery"
    );

    // Delivery is independent, and only a fresh post-receipt balance can close the generation.
    vault.history.borrow_mut().clear();
    *vault.history_error.borrow_mut() = Some("pruned".into());
    vault.delivery_available.set(true);
    let hot = FakeHot::then_always(vec![0], REQUIRED);
    drop(money_command_run(dir.path(), &vault, &hot).await.unwrap());
    assert!(hot.reads.get() >= 2);
    let satisfied = record_of(dir.path()).unwrap();
    assert_eq!(satisfied.state, FundingState::Satisfied);
    assert_eq!(
        satisfied.evidence.as_ref().unwrap().queue_transaction_id,
        Some(7)
    );
    assert_eq!(
        satisfied.evidence.unwrap().delivery_message_id,
        Some(format!("delivery-of-{}", actual_sent.message_id))
    );
    assert_eq!(vault.submits.get(), submits);

    let larger = FundingRequirements::new([(SHELL, REQUIRED * 2)]);
    let _ = money_command_run_with_requirements(
        dir.path(),
        &vault,
        &FakeHot::always(REQUIRED),
        &larger,
        bounds(),
    )
    .await;
    assert_eq!(vault.submits.get(), submits + 1);
    assert_eq!(
        vault.submitted.borrow().last().unwrap().cc.get(&SHELL),
        Some(&REQUIRED)
    );
}

#[tokio::test]
async fn legacy_unrelated_unbound_candidate_yields_to_actual_execution() {
    unrelated_unbound_candidate_yields_to_actual_execution(false).await;
}

#[tokio::test]
async fn production_unrelated_unbound_candidate_yields_to_actual_execution() {
    unrelated_unbound_candidate_yields_to_actual_execution(true).await;
}

async fn admission_identity_survives_separate_execution_and_delivery(legacy_executed: bool) {
    let dir = temp();
    let vault = FakeVault::empty();
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let mut record = record_of(dir.path()).unwrap();
    if legacy_executed {
        let generation = record.generation;
        let transaction_hash = record.transaction_hash.clone();
        record = legacy_record(None, 7);
        record.generation = generation;
        record.transaction_hash = transaction_hash;
        assert!(record.is_expired_at(unix_now_secs()));
    }
    record.pending_transaction_id = None;
    store_funding_journal(dir.path(), &record).unwrap();
    vault.queue.borrow_mut().clear();
    vault.delivery_available.set(false);
    // Only admission is visible. An equal-amount admission from another submit is not ours.
    *vault.history.borrow_mut() = vec![
        submitted_event(7, record.fingerprint.value, QUEUED_AT),
        submitted_event(99, record.fingerprint.value, QUEUED_AT + 2),
    ];
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let identified = record_of(dir.path()).unwrap();
    assert_eq!(identified.pending_transaction_id.as_deref(), Some("7"));
    let mut expected = record;
    expected.pending_transaction_id = Some("7".into());
    expected.last_checked_at_unix = identified.last_checked_at_unix;
    assert_eq!(
        identified, expected,
        "admission changes neither state nor execution evidence nor deadline"
    );
    assert_eq!(vault.submits.get(), 1);

    // Restart after the admission is pruned. Execution becomes visible, but delivery is still absent.
    *vault.history.borrow_mut() = vec![sent_event(7, identified.fingerprint.value, QUEUED_AT + 1)];
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let executed = record_of(dir.path()).unwrap();
    assert_eq!(executed.pending_transaction_id.as_deref(), Some("7"));
    assert_eq!(executed.state, FundingState::Executed);
    let evidence = executed.evidence.as_ref().unwrap();
    assert_eq!(evidence.queue_transaction_id, Some(7));
    assert!(evidence.delivery_message_id.is_none());
    assert_eq!(vault.submits.get(), 1);

    vault.history.borrow_mut().clear();
    *vault.history_error.borrow_mut() = Some("history pruned".into());
    *vault.queue_error.borrow_mut() = Some("queue unavailable".into());
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    assert_eq!(record_of(dir.path()).unwrap().state, FundingState::Executed);
    assert_eq!(
        vault.submits.get(),
        1,
        "missing delivery never authorizes a repeat"
    );

    // Delivery arrives independently; the balance must be read again after its receipt.
    vault.delivery_available.set(true);
    let hot = FakeHot::then_always(vec![0], REQUIRED);
    drop(money_command_run(dir.path(), &vault, &hot).await.unwrap());
    assert!(hot.reads.get() >= 2);
    let satisfied = record_of(dir.path()).unwrap();
    assert_eq!(satisfied.state, FundingState::Satisfied);
    assert!(satisfied.evidence.unwrap().delivery_message_id.is_some());
    assert_eq!(vault.submits.get(), 1);

    *vault.queue_error.borrow_mut() = None;
    let larger = FundingRequirements::new([(SHELL, REQUIRED * 2)]);
    let _ = money_command_run_with_requirements(
        dir.path(),
        &vault,
        &FakeHot::always(REQUIRED),
        &larger,
        bounds(),
    )
    .await;
    assert_eq!(vault.submits.get(), 2);
    assert_eq!(vault.submitted.borrow()[1].cc.get(&SHELL), Some(&REQUIRED));
}

#[tokio::test]
async fn submitted_admission_identity_survives_separate_execution_and_delivery() {
    admission_identity_survives_separate_execution_and_delivery(false).await;
}

#[tokio::test]
async fn legacy_executed_admission_identity_survives_separate_execution_and_delivery() {
    admission_identity_survives_separate_execution_and_delivery(true).await;
}

#[tokio::test]
async fn unproven_admissions_without_execution_never_persist_a_queue_identity() {
    for proof in [
        "wrong receipt",
        "unreadable receipt",
        "conflicting ids",
        "fingerprint only",
    ] {
        let dir = temp();
        let vault = FakeVault::empty();
        let mut record = legacy_record(None, 7);
        record.state = FundingState::Submitted;
        record.evidence = None;
        record.created_at_unix = unix_now_secs();
        record.expires_at_unix = funding_request_deadline(record.created_at_unix);
        record.transaction_hash = Some("tx-7".into());
        let mut admission = submitted_event(7, record.fingerprint.value, QUEUED_AT);
        match proof {
            "wrong receipt" => admission.message_id = "msg-submitted-99".into(),
            "unreadable receipt" => {
                *vault.submit_identity_error.borrow_mut() = Some("unavailable".into())
            }
            "conflicting ids" => {
                let mut conflicting = admission.clone();
                conflicting.transaction_id = 8;
                vault.history.borrow_mut().push(conflicting);
            }
            "fingerprint only" => record.transaction_hash = None,
            _ => unreachable!(),
        }
        vault.history.borrow_mut().push(admission);
        store_funding_journal(dir.path(), &record).unwrap();
        let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
        assert_eq!(record_of(dir.path()).unwrap(), record, "{proof}");
        assert_eq!(vault.submits.get(), 0, "{proof}");
    }
}

#[tokio::test]
async fn recovered_id_and_delivery_survive_a_second_restart_after_history_pruning() {
    let dir = temp();
    let vault = FakeVault::empty();
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let mut submitted = record_of(dir.path()).unwrap();
    // A successful submit receipt survived, but the subsequent queue lookup did not return an ID.
    submitted.pending_transaction_id = None;
    let native = submitted.fingerprint.value;
    store_funding_journal(dir.path(), &submitted).unwrap();
    vault.queue.borrow_mut().clear();
    vault
        .history
        .borrow_mut()
        .push(sent_event(7, native, QUEUED_AT + 1));
    // A later equal-amount transfer must not hide the admission tied to our exact submit receipt.
    vault.history.borrow_mut().extend([
        submitted_event(99, native, QUEUED_AT + 2),
        sent_event(99, native, QUEUED_AT + 3),
    ]);
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let mut executed = record_of(dir.path()).unwrap();
    assert_eq!(executed.pending_transaction_id.as_deref(), Some("7"));
    let evidence = executed.evidence.as_ref().unwrap();
    assert_eq!(evidence.queue_transaction_id, Some(7));
    assert_eq!(
        evidence.delivery_message_id.as_deref(),
        Some("delivery-of-msg-sent-7")
    );
    executed.created_at_unix = QUEUED_AT;
    executed.expires_at_unix = funding_request_deadline(QUEUED_AT);
    store_funding_journal(dir.path(), &executed).unwrap();
    vault.history.borrow_mut().clear();
    *vault.history_error.borrow_mut() = Some("history pruned".into());
    *vault.queue_error.borrow_mut() = Some("queue unavailable".into());
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    assert_eq!(record_of(dir.path()).unwrap().evidence, executed.evidence);
    assert_eq!(
        vault.submits.get(),
        1,
        "no repeat funding after history pruning"
    );
    drop(
        money_command_run(dir.path(), &vault, &FakeHot::always(REQUIRED))
            .await
            .unwrap(),
    );
    assert_eq!(
        record_of(dir.path()).unwrap().state,
        FundingState::Satisfied
    );
}

#[tokio::test]
async fn legacy_text_with_an_independent_queue_id_restores_a_saved_delivery() {
    let dir = temp();
    let vault = FakeVault::empty();
    let mut record = legacy_record(Some(7), 7);
    let delivery = hex64(0xdd);
    record.evidence.as_mut().unwrap().detail.push_str(&format!(
        ", delivered to the Hot by internal message {delivery}, whose destination receipt is finalized"
    ));
    store_funding_journal(dir.path(), &record).unwrap();
    *vault.queue_error.borrow_mut() = Some("unavailable".into());
    *vault.history_error.borrow_mut() = Some("pruned".into());
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    let migrated = record_of(dir.path()).unwrap();
    let evidence = migrated.evidence.unwrap();
    assert_eq!(evidence.queue_transaction_id, Some(7));
    assert_eq!(
        evidence.delivery_message_id.as_deref(),
        Some(delivery.as_str())
    );
    assert_eq!(vault.submits.get(), 0);
    drop(
        money_command_run(dir.path(), &vault, &FakeHot::always(REQUIRED))
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn the_incident_shaped_no_id_journal_remains_protected_after_its_deadline() {
    let dir = temp();
    let vault = FakeVault::empty();
    let record = legacy_record(None, 3_401_253_548_762_108_931);
    assert!(record.is_expired_at(unix_now_secs()));
    store_funding_journal(dir.path(), &record).unwrap();
    let provider = AckinackiVaultProvider::new(&vault, Some(record.recorded_request()));
    let presence = provider
        .probe_existing_request(&record.recorded_funding_request(hex64(0xa1)))
        .await
        .unwrap();
    let RequestPresence::Unknown {
        reason: diagnostic, ..
    } = presence
    else {
        panic!("ambiguous old evidence must not authorize another transfer: {presence:?}");
    };
    assert!(
        diagnostic.contains("confirmed queue identity"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("Hot delivery"), "{diagnostic}");
    assert!(money_command_run(dir.path(), &vault, &FakeHot::always(0))
        .await
        .is_err());
    let after = record_of(dir.path()).unwrap();
    assert_eq!(
        after, record,
        "ambiguous evidence is preserved without invented identifiers"
    );
    assert_eq!(vault.submits.get(), 0);
}

#[tokio::test]
async fn saved_execution_without_delivery_keeps_waiting_and_later_persists_the_receipt() {
    let dir = temp();
    let vault = FakeVault::empty();
    let mut record = legacy_record(Some(7), 7);
    record.evidence.as_mut().unwrap().queue_transaction_id = Some(7);
    store_funding_journal(dir.path(), &record).unwrap();
    vault.delivery_available.set(false);
    *vault.history_error.borrow_mut() = Some("pruned".into());
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    assert_eq!(vault.submits.get(), 0);
    assert!(record_of(dir.path())
        .unwrap()
        .evidence
        .unwrap()
        .delivery_message_id
        .is_none());
    vault.delivery_available.set(true);
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    assert!(record_of(dir.path())
        .unwrap()
        .evidence
        .unwrap()
        .delivery_message_id
        .is_some());
    assert_eq!(vault.submits.get(), 0);
}

#[tokio::test]
async fn recovered_execution_sizes_a_later_request_from_the_current_balance() {
    let dir = temp();
    let vault = FakeVault::empty();
    let mut record = legacy_record(Some(7), 7);
    let evidence = record.evidence.as_mut().unwrap();
    evidence.queue_transaction_id = Some(7);
    evidence.delivery_message_id = Some(hex64(0xdd));
    store_funding_journal(dir.path(), &record).unwrap();
    *vault.history_error.borrow_mut() = Some("pruned".into());
    drop(
        money_command_run(dir.path(), &vault, &FakeHot::always(REQUIRED))
            .await
            .unwrap(),
    );
    assert_eq!(
        record_of(dir.path()).unwrap().state,
        FundingState::Satisfied
    );
    let larger = FundingRequirements::new([(SHELL, REQUIRED * 2)]);
    let _ = money_command_run_with_requirements(
        dir.path(),
        &vault,
        &FakeHot::always(REQUIRED),
        &larger,
        bounds(),
    )
    .await;
    assert_eq!(vault.submits.get(), 1);
    assert_eq!(vault.submitted.borrow()[0].cc.get(&SHELL), Some(&REQUIRED));
}

#[tokio::test]
async fn an_unrelated_sufficient_balance_does_not_forget_an_undelivered_transfer() {
    let dir = temp();
    let vault = FakeVault::empty();
    let mut record = legacy_record(Some(7), 7);
    record.evidence.as_mut().unwrap().queue_transaction_id = Some(7);
    store_funding_journal(dir.path(), &record).unwrap();
    vault.delivery_available.set(false);
    drop(
        money_command_run(dir.path(), &vault, &FakeHot::always(REQUIRED))
            .await
            .unwrap(),
    );
    assert_eq!(record_of(dir.path()).unwrap().state, FundingState::Executed);
    // Spending the unrelated incoming transfer must not authorize the same funding a second time.
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    assert_eq!(vault.submits.get(), 0);
    assert_eq!(record_of(dir.path()).unwrap().state, FundingState::Executed);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

    #[test]
    fn legacy_text_cannot_bind_an_unknown_or_conflicting_queue_id(id in 1u64..u64::MAX) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for known in [None, Some(id - 1)] {
            let dir = temp();
            let vault = FakeVault::empty();
            let mut record = legacy_record(known, id);
            record.evidence.as_mut().unwrap().detail.push_str(&format!(
                ", delivered to the Hot by internal message {}, whose destination receipt is finalized",
                hex64(0xdd)
            ));
            store_funding_journal(dir.path(), &record).unwrap();
            let _ = runtime.block_on(money_command_run(dir.path(), &vault, &FakeHot::always(0)));
            proptest::prop_assert_eq!(vault.submits.get(), 0);
            let mut after = record_of(dir.path()).unwrap();
            // A chain probe may refresh its check time, but must not promote the candidate or
            // change the generation's identity, state, evidence, amounts or deadline.
            after.last_checked_at_unix = record.last_checked_at_unix;
            proptest::prop_assert_eq!(after, record);
        }
    }
}

#[tokio::test]
async fn a_conflicting_structured_execution_id_never_overwrites_the_recorded_id() {
    let dir = temp();
    let vault = FakeVault::empty();
    let mut record = legacy_record(Some(7), 7);
    let evidence = record.evidence.as_mut().unwrap();
    evidence.queue_transaction_id = Some(8);
    evidence.delivery_message_id = Some(hex64(0xdd));
    store_funding_journal(dir.path(), &record).unwrap();
    // Fresh history must not override a conflict between two confirmed identities.
    *vault.history.borrow_mut() = vec![
        submitted_event(7, record.fingerprint.value, QUEUED_AT),
        sent_event(7, record.fingerprint.value, QUEUED_AT + 1),
    ];
    for balance in [0, REQUIRED] {
        assert!(
            money_command_run(dir.path(), &vault, &FakeHot::always(balance))
                .await
                .is_err()
        );
        assert_eq!(record_of(dir.path()).unwrap(), record);
        assert_eq!(vault.submits.get(), 0);
    }
}

#[tokio::test]
async fn confirmed_structured_evidence_restores_the_redundant_queue_field_without_history() {
    let dir = temp();
    let vault = FakeVault::empty();
    let mut record = legacy_record(None, 7);
    let evidence = record.evidence.as_mut().unwrap();
    evidence.queue_transaction_id = Some(7);
    evidence.delivery_message_id = Some(hex64(0xdd));
    store_funding_journal(dir.path(), &record).unwrap();
    *vault.queue_error.borrow_mut() = Some("unavailable".into());
    *vault.history_error.borrow_mut() = Some("pruned".into());
    let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
    assert_eq!(
        record_of(dir.path())
            .unwrap()
            .pending_transaction_id
            .as_deref(),
        Some("7")
    );
    assert_eq!(vault.submits.get(), 0);
}

#[tokio::test]
async fn malformed_legacy_evidence_is_not_promoted_to_a_confirmed_execution() {
    let original = legacy_record(Some(7), 7);
    let mut cases = vec![original.clone(); 5];
    cases[0].evidence.as_mut().unwrap().source = "finalized ext-out message missing".into();
    cases[1].evidence.as_mut().unwrap().observed_at_unix = None;
    cases[2].evidence.as_mut().unwrap().detail = original
        .evidence
        .as_ref()
        .unwrap()
        .detail
        .replace(&hot_address(), &vault_address());
    cases[3].evidence.as_mut().unwrap().detail.push_str(", delivered to the Hot by internal message missing, whose destination receipt is finalized");
    cases[4].evidence.as_mut().unwrap().observed_at_unix = Some(QUEUED_AT);
    let vault = FakeVault::empty();
    *vault.queue_error.borrow_mut() = Some("unavailable".into());
    for record in cases {
        let provider = AckinackiVaultProvider::new(&vault, Some(record.recorded_request()));
        let verdict = provider
            .probe_existing_request(&record.recorded_funding_request(hex64(0xa1)))
            .await
            .unwrap();
        assert!(
            matches!(verdict, RequestPresence::Unknown { .. }),
            "{verdict:?}"
        );
    }
}

#[tokio::test]
async fn failed_or_conflicting_receipt_binding_retains_conservative_execution_past_expiry() {
    for conflicting in [false, true] {
        let dir = temp();
        let vault = FakeVault::empty();
        let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
        let mut record = record_of(dir.path()).unwrap();
        record.pending_transaction_id = None;
        let native = record.fingerprint.value;
        store_funding_journal(dir.path(), &record).unwrap();
        vault.queue.borrow_mut().clear();
        vault
            .history
            .borrow_mut()
            .push(sent_event(7, native, QUEUED_AT + 1));
        if conflicting {
            let mut admission = submitted_event(8, native, QUEUED_AT);
            admission.message_id = "msg-submitted-7".into();
            vault.history.borrow_mut().push(admission);
        } else {
            *vault.submit_identity_error.borrow_mut() = Some("receipt index unavailable".into());
        }
        let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
        let mut retained = record_of(dir.path()).unwrap();
        assert_eq!(retained.state, FundingState::Executed);
        assert!(retained.pending_transaction_id.is_none());
        assert!(retained
            .evidence
            .as_ref()
            .unwrap()
            .queue_transaction_id
            .is_none());
        retained.created_at_unix = QUEUED_AT;
        retained.expires_at_unix = funding_request_deadline(QUEUED_AT);
        store_funding_journal(dir.path(), &retained).unwrap();
        vault.history.borrow_mut().clear();
        let _ = money_command_run(dir.path(), &vault, &FakeHot::always(0)).await;
        assert_eq!(vault.submits.get(), 1);
        assert_eq!(record_of(dir.path()).unwrap(), retained);
    }
}
