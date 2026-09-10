//! at the amount that was actually accepted, and the three properties the verdict rests on.

//! The reported run lost the owner key of a note whose voucher payment the chain had already taken:
//! message `70b25853...` moved `value_other ECC[2] = 0x517da02c00` from the funding wallet to RootPN
//! with `aborted: false`, and the deploy then missed its proof window and deleted the recovery file
//! saying "Nothing is on chain from it".

//! THE 350 IS NOT A NOMINAL, and an earlier version of this file assumed it was. A recovery state
//! holds the NOMINAL in `raw_value` -- production fills it from `nominal.raw_value(..)`
//! (`note_cmd.rs`), and `NoteDeployVoucherCheckpoint::voucher_currency_map` says it outright: the
//! wire figure exists only where the message is built. What the wallet actually attaches is
//! `params::note_deploy_wallet_funding_raw(nominal)` = nominal + `ROOT_PN_GAS_DEPOSIT_RAW`. So the
//! accepted 350 SHELL is an N100 note plus the 250 SHELL collection, and a fixture that stores
//! `N350` with `raw_value = 350e9` describes a deploy that would have put 600 SHELL on the wire --
//! a state production cannot reach, pinning a path that never broke. The figure below is therefore
//! DERIVED from the production constant rather than typed, so it cannot drift away from it.

//! WHAT THE SENTENCE IS ALLOWED TO MEAN. "Nothing is on chain from it" is a claim about the CHAIN
//! made without asking the chain: the decision reads the local recovery file and nothing else. That
//! is sound only because of an ordering the writer guarantees -- `submit_maybe_sent` is persisted
//! BEFORE the wallet POST goes out -- so a state that reads back clean and records no possible spend
//! could not have paid for anything. Every other state must not make the claim, and the two that are
//! easiest to confuse are the two this pins apart: a file that says "nothing was paid" and a file
//! that cannot be read at all. The first is an answer; the second is a failure to obtain one.

use std::path::Path;

use crate::cli::note::{
    load_note_deploy_recovery, write_note_deploy_recovery, NoteDeployRecoveryRequest,
    NoteDeployRecoveryState, NoteDeployVoucherCheckpoint, NoteDeployVoucherEvent,
    NoteDeployVoucherKind,
};

use super::{note_deploy_classify_stale_proof_attempt, NoteDeployRecoveryOutcome};

/// The exact ECC[2] figure the wallet transaction carried, as the report printed it in hex.
const REPORTED_WIRE_RAW: u128 = 0x0051_7da0_2c00;

/// What the deploy was FOR: an N100 note. This is what a recovery state stores, and the reported
/// payment is this plus the collection RootPN takes out of every deposit.
const NOMINAL_LABEL: &str = "N100";
const NOMINAL_RAW: u64 = 100_000_000_000;

const OWNER_SECRET_HEX: &str = "5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b";

/// Where the reported figure comes from, asserted rather than described.

/// `0x517da02c00` is 350 SHELL at the raw ECC[2] scale, and it is what an N100 deploy puts on the
/// wire once RootPN's collection is added. Both halves are pinned: the hex is the sum, and the sum
/// is not the nominal -- so a fixture that stored 350 as `raw_value` would be describing a deploy
/// of a 350 SHELL note, which is a different and larger payment.
#[test]
fn the_reported_payment_is_an_n100_nominal_plus_the_collection() {
    assert_eq!(REPORTED_WIRE_RAW, 350_000_000_000);
    assert_eq!(
        dexdo_core::params::note_deploy_wallet_funding_raw(NOMINAL_RAW),
        REPORTED_WIRE_RAW,
        "the accepted payment is the N100 nominal plus ROOT_PN_GAS_DEPOSIT_RAW"
    );
    assert_ne!(
        u128::from(NOMINAL_RAW),
        REPORTED_WIRE_RAW,
        "the nominal a recovery state stores is not the figure the wallet sent"
    );
}

fn recovery_state() -> NoteDeployRecoveryState {
    let owner_public_key_hex =
        crate::cli::note::derive_owner_pubkey_from_secret_hex(OWNER_SECRET_HEX)
            .expect("owner pubkey derives from the recovery secret");
    NoteDeployRecoveryState::new(
        NoteDeployRecoveryRequest {
            endpoint: "http://127.0.0.1:9",
            nominal: NOMINAL_LABEL,
            token_type: dexdo_core::params::SHELL_CURRENCY_ID,
            raw_value: NOMINAL_RAW,
            funding_multisig_address: &format!("0:{}", "f8".repeat(32)),
        },
        &owner_public_key_hex,
        OWNER_SECRET_HEX,
    )
    .expect("a fresh note deploy recovery state")
}

/// The state as production holds it at the moment of the reported failure: the wallet submit is
/// recorded, the chain accepted it, the `VoucherGenerated` event is persisted, and no PrivateNote
/// exists yet -- deploying it is what the proof that missed its window was for.
fn write_accepted_payment_recovery(path: &Path) -> String {
    let mut state = recovery_state();
    let mut checkpoint = NoteDeployVoucherCheckpoint::new(
        &state.owner_public_key_hex,
        dexdo_core::params::SHELL_CURRENCY_ID,
        NOMINAL_RAW,
        false,
        "7e".repeat(32),
        "8f".repeat(32),
    )
    .expect("voucher checkpoint at the N100 nominal");
    checkpoint.submit_maybe_sent = true;
    checkpoint.event = Some(NoteDeployVoucherEvent {
        id: "70b25853254d102122fdac300a43601248d20f7568543cbde2014e04491180a1".to_string(),
        boc: "voucher-boc".to_string(),
        body: "voucher-body".to_string(),
        dst: format!("0:{}", "10".repeat(32)),
        created_at: 1,
        block_id: Some("voucher-block".to_string()),
    });
    let sk_u_hex = checkpoint.sk_u_hex.to_string();
    state
        .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, checkpoint)
        .expect("a paid voucher checkpoint");
    assert!(
        state.pn_address.is_none(),
        "the reported case is an accepted payment with no note deployed yet"
    );
    write_note_deploy_recovery(path, &state).expect("write the accepted-payment recovery");
    sk_u_hex
}

/// The reported sequence, at the reported figure: 350 SHELL accepted, then the deploy fails.

/// The assertion is on the KEY, not only on the file's existence. What was lost in the report was
/// `owner_secret_key_hex` and the voucher's `sk_u`; a file that survives with either of them altered
/// is the same loss with a different shape.
#[tokio::test(start_paused = true)]
async fn an_accepted_three_hundred_and_fifty_shell_payment_survives_the_deploy_failure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let recovery_path = temp.path().join("pn_pool.json.recovery.json");
    let sk_u_hex = write_accepted_payment_recovery(&recovery_path);

    let error = super::note_deploy_prove_within_history_window(
        std::future::pending::<anyhow::Result<u8>>(),
        "deposit",
        "SDK default plan",
        &recovery_path,
        None,
        Some(std::time::Duration::from_secs(600)),
    )
    .await
    .expect_err("a proof past the bound is still reported");
    let message = error.to_string();

    let state = load_note_deploy_recovery(&recovery_path)
        .expect("the recovery still reads back")
        .expect("the recovery still exists");
    assert_eq!(
        state.owner_secret_key_hex.to_string(),
        OWNER_SECRET_HEX,
        "the note owner key must survive the failure verbatim: {message}"
    );
    // The field rather than the `voucher_checkpoint` reader: that reader is compiled only under
    // a cargo feature this tree no longer declares, and this regression belongs in the default gate -- the guard it
    // covers answers on every build, and a proof of it that only the feature build runs is a proof
    // nobody runs.
    let voucher = state
        .deposit_voucher
        .as_ref()
        .expect("the paid voucher checkpoint survives");
    assert_eq!(
        voucher.sk_u_hex.to_string(),
        sk_u_hex,
        "the voucher secret that reaches the accepted payment must survive: {message}"
    );
    assert_eq!(
        voucher.raw_value, NOMINAL_RAW,
        "the surviving state must still name the note it paid for: {message}"
    );
    assert_eq!(
        dexdo_core::params::note_deploy_wallet_funding_raw(voucher.raw_value),
        REPORTED_WIRE_RAW,
        "and what survives must still derive the 350 SHELL the chain accepted: {message}"
    );
    assert!(
        !message.contains("Nothing is on chain from it"),
        "350 SHELL was accepted by the chain, so this claim is false: {message}"
    );
    assert!(
        !message.contains("has been removed"),
        "an accepted payment is never abandoned: {message}"
    );
    assert!(
        message.contains("is KEPT"),
        "the verdict must say the paid recovery survives: {message}"
    );
}

/// Which states may claim the chain is empty, and which may not.

/// The claim is local, so its licence has to be exact. Only a file that read back as this client's
/// recovery state AND records nothing that can have cost money is entitled to it; the outcome that
/// exists precisely because reading failed is not, and neither is a path that never held a state --
/// there is nothing there to make a claim about.
#[test]
fn only_a_state_that_read_back_clean_may_claim_the_chain_is_empty() {
    fn verdict(outcome: NoteDeployRecoveryOutcome) -> String {
        super::note_deploy_proof_window_missed_message(
            "deposit",
            "SDK default plan",
            std::time::Duration::from_secs(600),
            Path::new("/tmp/pn_pool.json.recovery.json"),
            outcome,
            None,
        )
    }

    assert!(
        verdict(NoteDeployRecoveryOutcome::Discarded).contains("Nothing is on chain from it"),
        "a state that read back clean and records no spend is the one case that may say it"
    );
    for silent in [
        NoteDeployRecoveryOutcome::KeptPaidFor,
        NoteDeployRecoveryOutcome::KeptUnreadable,
        NoteDeployRecoveryOutcome::Absent,
    ] {
        let message = verdict(silent);
        assert!(
            !message.contains("Nothing is on chain from it"),
            "{silent:?} has not established that the chain is empty: {message}"
        );
    }

    // The pair the report confused, named apart. `KeptUnreadable` is a failure to obtain an answer,
    // and it must say so rather than borrow the answer it could not read.
    let unreadable = verdict(NoteDeployRecoveryOutcome::KeptUnreadable);
    assert!(
        unreadable.contains("cannot be read back"),
        "an unreadable state must say that reading is what failed: {unreadable}"
    );
    assert!(
        unreadable.contains("is KEPT"),
        "what cannot be read cannot be ruled out, so it is kept: {unreadable}"
    );
}

/// What a torn file is worth, which is the half of crash-safety this can actually decide.

/// NAMED FOR WHAT IT PROVES. An earlier version of this called itself a crash-safety test and wrote
/// the torn file itself, which tests how corruption is HANDLED, not how a write is INTERRUPTED --
/// strip the temp-write and rename out of the writer and it stayed green. Handling is still worth
/// pinning, because it is the branch the reported loss took: a file that does not read back must be
/// kept, must say that READING is what failed, and must be left exactly as found. Whether the
/// writer can produce such a file at all is the next test's question, not this one's.

/// The tail is the contrast that gives the torn half its meaning, and it is asserted BYTE FOR BYTE
/// rather than as "parses and differs": a completed write over the same path lands the whole
/// document, which is only a statement about this fixture because the fixture is deterministic.
#[test]
fn a_torn_recovery_is_kept_and_left_exactly_as_found() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("pn_pool.json.recovery.json");
    let sk_u_hex = write_accepted_payment_recovery(&path);

    let whole = std::fs::read_to_string(&path).expect("the completed write is readable");
    assert!(
        whole.contains(&sk_u_hex),
        "the completed write carries the voucher secret"
    );

    let torn = &whole[..whole.len() / 2];
    std::fs::write(&path, torn).expect("write the torn file");
    assert!(
        load_note_deploy_recovery(&path).is_err(),
        "a torn file must not read back as a valid state; that is what makes it undecidable"
    );
    assert_eq!(
        note_deploy_classify_stale_proof_attempt(&path, None)
            .expect("classifying a torn file is not itself an error"),
        NoteDeployRecoveryOutcome::KeptUnreadable,
        "a file left half written by an interrupted write still carries whatever was funded"
    );
    assert!(path.exists(), "and it is still there afterwards");
    assert_eq!(
        std::fs::read_to_string(&path).expect("the torn file is still readable as bytes"),
        torn,
        "classifying it must not rewrite or truncate it further"
    );

    // A completed write over the same path lands whole, never mixed with what was there.
    std::fs::remove_file(&path).expect("clear the torn file");
    let restored = write_accepted_payment_recovery(&path);
    assert_eq!(restored, sk_u_hex, "the fixture is deterministic");
    let after = std::fs::read_to_string(&path).expect("the rewritten file is readable");
    assert_eq!(after, whole, "a completed write lands the whole document");
}

/// Crash-safety at the writer, stated as a property a non-atomic writer fails.

/// The guarantee is that an interrupted write leaves the PREVIOUS complete document at the path,
/// never a mixture. What produces it is that the writer never touches the target: it creates a new
/// temp file, fsyncs it, and renames it over the top.

/// The final document cannot show this on its own -- both writers leave the same bytes at the path
/// -- so what is asserted is the FILE the old directory entry pointed at. A second name for that
/// file, taken before the rewrite, still reads the OLD document afterwards, because `rename` rebinds
/// a name and leaves the file alone. A writer that opens the target and writes through keeps ONE
/// file under both names, so the second name reads the new document and there was no instant at
/// which the old bytes were safe. So this fails exactly when the temp-and-rename goes.

/// WHAT IS COVERED WHERE, stated because the previous version of this comment got it wrong. The
/// second name is a hard link, which every target we ship on provides, so the property is asserted
/// on all of them. The inode check is the same statement in the Unix spelling, where the identity
/// being replaced is directly nameable, and it runs only there. What this replaces was a content
/// assertion of the form `!a || b` whose `b` is implied by its `a` -- true for every input, so it
/// could not fail: with the temp-and-rename stripped out and the inode check compiled away, the
/// test measured 6 passed, which is what a claim of non-Unix coverage was resting on.
#[test]
fn the_writer_replaces_the_recovery_file_rather_than_writing_into_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("pn_pool.json.recovery.json");
    write_accepted_payment_recovery(&path);
    let first = std::fs::read_to_string(&path).expect("the first write is readable");

    // A second name for the file the path points at NOW, taken before the rewrite.
    let first_entry = temp.path().join("first_entry_hard_link.json");
    std::fs::hard_link(&path, &first_entry).expect("a second name for the first write");

    #[cfg(unix)]
    let before = {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(&path)
            .expect("stat the first write")
            .ino()
    };

    // The same document again, through the same writer.
    let mut state = recovery_state();
    let mut checkpoint = NoteDeployVoucherCheckpoint::new(
        &state.owner_public_key_hex,
        dexdo_core::params::SHELL_CURRENCY_ID,
        NOMINAL_RAW,
        false,
        "7e".repeat(32),
        "8f".repeat(32),
    )
    .expect("voucher checkpoint at the N100 nominal");
    checkpoint.submit_maybe_sent = true;
    state
        .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, checkpoint)
        .expect("a paid voucher checkpoint");
    write_note_deploy_recovery(&path, &state).expect("the second write lands");

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let after = std::fs::metadata(&path)
            .expect("stat the second write")
            .ino();
        assert_ne!(
            before, after,
            "the writer must rename a new file over the target, not write into it: an in-place \
             write has an instant where the previous complete document is already gone"
        );
    }

    // The same property where there is no inode to name: the rewrite rebound the PATH, so the file
    // the earlier name still points at was never opened for writing.
    assert_eq!(
        std::fs::read_to_string(&first_entry).expect("the first write is still readable"),
        first,
        "the writer must rebind the path to a new file and leave the previous document intact: \
         writing through to it destroys the only complete copy before the replacement is whole"
    );

    // Whole document either way: a rename cannot leave a prefix behind.
    let second = std::fs::read_to_string(&path).expect("the second write is readable");
    assert!(
        serde_json::from_str::<serde_json::Value>(&second).is_ok(),
        "what lands at the path parses as a whole document"
    );
    assert_ne!(
        first, second,
        "the fixtures differ, so the rewrite is observable"
    );

    // And no temp file is left behind to be mistaken for a recovery state.
    let strays: Vec<_> = std::fs::read_dir(temp.path())
        .expect("read the directory back")
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .filter(|name| name.to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        strays.is_empty(),
        "a completed write leaves no temp file: {strays:?}"
    );
}

/// Idempotence, run twice rather than argued once.

/// The transition has two ends and both must be idempotent, for opposite reasons. Repeating the
/// KEEP must not start creating obligations -- no deletion, no rewrite, no change of verdict. And
/// repeating the DISCARD must report the closed state rather than claim a second removal: `Absent`
/// exists so that "has been removed" is never printed about a path that no longer holds anything.
#[test]
fn repeating_the_transition_creates_no_second_obligation() {
    let temp = tempfile::tempdir().expect("temp dir");

    let kept = temp.path().join("kept.recovery.json");
    write_accepted_payment_recovery(&kept);
    let before = std::fs::read_to_string(&kept).expect("the paid state is on disk");
    for round in 1..=2 {
        assert_eq!(
            note_deploy_classify_stale_proof_attempt(&kept, None).expect("classify a paid state"),
            NoteDeployRecoveryOutcome::KeptPaidFor,
            "round {round}: a paid state is kept every time it is asked about"
        );
        assert_eq!(
            std::fs::read_to_string(&kept).expect("the paid state is still on disk"),
            before,
            "round {round}: asking must not modify the file"
        );
    }

    let discarded = temp.path().join("discarded.recovery.json");
    write_note_deploy_recovery(&discarded, &recovery_state())
        .expect("write a state with no voucher under it");
    assert_eq!(
        note_deploy_classify_stale_proof_attempt(&discarded, None).expect("classify"),
        NoteDeployRecoveryOutcome::Discarded,
        "the first pass removes a state that cannot have cost anything"
    );
    assert!(!discarded.exists(), "and the file is gone");
    assert_eq!(
        note_deploy_classify_stale_proof_attempt(&discarded, None).expect("classify again"),
        NoteDeployRecoveryOutcome::Absent,
        "the second pass reports the closed state instead of removing a second time"
    );
    let message = super::note_deploy_proof_window_missed_message(
        "deposit",
        "SDK default plan",
        std::time::Duration::from_secs(600),
        &discarded,
        NoteDeployRecoveryOutcome::Absent,
        None,
    );
    assert!(
        !message.contains("has been removed"),
        "a second pass has removed nothing and must not say it did: {message}"
    );
    assert!(
        message.contains("nothing was removed"),
        "it must name the closed state instead: {message}"
    );
}
