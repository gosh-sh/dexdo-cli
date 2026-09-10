//! (rymkapro, 2026-08-17): `wallet remove-archived` deletes the ONLY local keys to a Hot, so
//! it must take the funding wallet's turn -- the SAME one `note deploy` and `note topup` take.

//! Without it the command decided from two observations it did not own, and a money command fitted
//! entirely into the gap between them: it loads the binding and its keys, a parallel rebind archives
//! that binding, `remove-archived` reads a still-empty journal and a still-zero Hot, the money
//! command writes `prepared` and sends its Vault -> Hot request, `remove-archived` deletes the keys,
//! and the confirmed request later credits a Hot nobody can spend from.

//! Two properties make the fix real rather than decorative, and each is proven separately here.

//! **It is the same lock.** A turn taken under a different key is worse than no turn at all -- it
//! serialises the command against itself while leaving the wallet raced, which is what the lock's
//! own documentation warns about. The binding records a Hot in whatever spelling it was written
//! with, and a money command resolves its wallet independently; if the two hashed to different lock
//! files the fix would be a no-op that reads as a fix.

//! **It covers the whole window.** A lock taken after the journal check would leave step 3 outside
//! it and close nothing.

//! The command cannot be driven end to end offline: `run_remove_archived` reads its store through
//! `data_dir::effective()`, whose `EXPLICIT_DATA_DIR` is a process-wide `OnceLock` that no test can
//! reset, and it reaches the chain for the balance. So the placement is proven the way the sibling
//! wiring proofs for the same lock prove theirs -- against the source of the function itself.

/// The production half of `wallet.rs`, with its unit-test module cut off.
fn production_source() -> &'static str {
    let source = include_str!("wallet.rs");
    source
        .split_once("#[cfg(test)]")
        .map(|(before, _)| before)
        .unwrap_or(source)
}

/// The shared seam, not a hand-rolled slice.

/// This one ended at the next sibling `async fn`, with `unwrap_or(body.len())` behind it, so the
/// absence of a next sibling was silent and "the body" became the whole rest of `wallet.rs`. It
/// also kept comments, so a commented-out call read as a call. `code_of` bounds by brace depth and

fn body_of(entry: &str) -> String {
    crate::cli::source_probe::code_of(production_source(), &format!("async fn {entry}"))
}

/// The turn is taken, and it is taken before EITHER observation the deletion rests on.
#[test]
fn remove_archived_holds_the_funding_turn_across_both_checks_334() {
    let body = body_of("run_remove_archived");

    let lock = body
        .find("acquire_funding_wallet_lock(")
        .expect("remove-archived takes the funding wallet's turn");
    let journal = body
        .find("refuse_removal_while_funding_may_still_arrive(")
        .expect("remove-archived checks the funding journal");
    let balance = body
        .find("remove_archived_binding_after_balance_check(")
        .expect("remove-archived checks the Hot balance before deleting");

    assert!(
        lock < journal,
        "the funding journal is read OUTSIDE the wallet's turn: a money command can write \
         `prepared` between this read and the deletion"
    );
    assert!(
        lock < balance,
        "the Hot balance is read OUTSIDE the wallet's turn: the reading the deletion rests on can \
         go stale before the keys are gone"
    );
}

/// The lock is keyed on the Hot that is about to lose its keys, not on something else that happens
/// to be in scope.
#[test]
fn remove_archived_locks_on_the_hot_it_is_about_to_forget_334() {
    let body = body_of("run_remove_archived");
    let call = body
        .find("acquire_funding_wallet_lock(")
        .expect("remove-archived takes the funding wallet's turn");
    let tail = &body[call..];
    let end = tail.find(")?").expect("the call is closed");
    let arguments = &tail[..end];
    assert!(
        arguments.contains("target.binding.hot_address"),
        "the turn is keyed on something other than the Hot whose keys are about to be deleted: \
         {arguments}"
    );
}

/// The two spellings of one wallet must land on ONE lock file. This is the property that makes the
/// turn shared with the spenders instead of private to this command.
#[test]
fn the_binding_spelling_and_the_spender_spelling_take_one_turn_334() {
    let account_id = "5".repeat(64);
    let canonical =
        format!("0000000000000000000000000000000000000000000000000000000000000004::{account_id}");
    let legacy = format!("0:{account_id}");

    let from_spender = crate::cli::note_cmd::funding_wallet_lock_path("net-a", &canonical)
        .expect("a spender resolves its wallet's lock path");
    let from_binding = crate::cli::note_cmd::funding_wallet_lock_path("net-a", &legacy)
        .expect("the binding's recorded Hot resolves its lock path");

    assert_eq!(
        from_spender, from_binding,
        "the canonical and legacy spellings of one wallet hash to different lock files, so \
         `remove-archived` and the money commands would each hold a turn the other cannot see"
    );
}

/// A different wallet must NOT share the turn: a lock that collides for everyone serialises the
/// whole client and would be its own defect.
#[test]
fn two_different_wallets_do_not_share_one_turn_334() {
    let one =
        crate::cli::note_cmd::funding_wallet_lock_path("net-a", &format!("0:{}", "5".repeat(64)))
            .expect("first wallet");
    let two =
        crate::cli::note_cmd::funding_wallet_lock_path("net-a", &format!("0:{}", "6".repeat(64)))
            .expect("second wallet");
    assert_ne!(
        one, two,
        "two different wallets share one lock file, which would serialise unrelated spends"
    );
}
