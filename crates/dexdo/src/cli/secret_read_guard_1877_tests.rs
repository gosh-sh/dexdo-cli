//! every read of a secret file is checked BEFORE the read, not after and not at all.

//! put `refuse_exposed_secret_file` on the reads that move money -- `--note-key`,
//! `--multisig-private-key`, `--multisig-seed-file`, the gateway key. Five other readers of the same
//! secrets never got it, and they are not the small ones:

//! - `load_pool_json` reads `pn_pool.json`, which carries `owner_secret_key_hex` for EVERY note in
//! the pool -- the most secret-dense file the client has;
//! - the two note-deploy recovery readers read that same secret back out of a file this client
//! wrote at 0600 and never checked again;
//! - the Gosh.ai resume path reads the stored RECOVERY PHRASE back from `hot.seed`;
//! - `load_note_tree` reads `--note-key` on the mock seams.

//! And one was worse than missing. `wallet_manual`'s `resolve_manual_secret` read the operator's
//! secret to CLASSIFY it -- hex key or seed phrase -- before the guarded read further down ever
//! ran. `support.rs` names that exact failure in its own words: "a refusal that has already loaded
//! the secret has done the thing it refuses." That one was a wrong ORDER, not a missing call.

use super::{refuse_exposed_secret_file, refuse_exposed_secret_file_if_present};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// A file at an exact mode. The mode is applied after creation here **on purpose**: this is a test
/// fixture standing in for a file that arrived from a backup, a checkout or another machine, which
/// is precisely the case the guard exists for. Production writes never do this -- they set the mode
/// at creation, and `support.rs` explains why.
fn file_at(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .expect("write the fixture secret");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
        .expect("set the fixture mode");
    path
}

/// Both directions, which is the only way either direction means anything: owner-only passes,
/// anything readable or writable by group or other is refused.
#[test]
fn the_guard_refuses_exposure_and_admits_owner_only() {
    let dir = tempfile::tempdir().expect("create fixture dir");

    for mode in [0o600, 0o400, 0o700] {
        let path = file_at(dir.path(), &format!("ok-{mode:o}"), mode);
        assert!(
            refuse_exposed_secret_file(&path, "the fixture").is_ok(),
            "mode {mode:o} exposes nothing to group or other and must be accepted"
        );
    }

    // Group-readable, group-writable, other-readable, other-writable, and the common 0644.
    for mode in [0o640, 0o620, 0o604, 0o602, 0o644, 0o666] {
        let path = file_at(dir.path(), &format!("bad-{mode:o}"), mode);
        let error = refuse_exposed_secret_file(&path, "the fixture")
            .expect_err(&format!(
                "mode {mode:o} is readable or writable beyond its owner"
            ))
            .to_string();
        assert!(
            error.contains(&format!("{mode:04o}")),
            "the refusal must state the mode it saw, or the operator cannot tell what to fix: \
             {error}"
        );
        assert!(
            error.contains("chmod 600"),
            "the refusal must carry the command that fixes it: {error}"
        );
    }
}

/// The lenient variant, in both directions plus the direction it exists for.

/// It is a second entry point, and a second entry point is a hazard, so what it does differently is
/// asserted rather than described: absent is fine, everything else is judged exactly as the strict
/// one judges it.
#[test]
fn the_if_present_variant_admits_absence_and_nothing_else() {
    let dir = tempfile::tempdir().expect("create fixture dir");

    let absent = dir.path().join("never-written.json");
    assert!(!absent.exists(), "the premise: this file does not exist");
    assert!(
        refuse_exposed_secret_file_if_present(&absent, "the pool").is_ok(),
        "a pool that has not been created yet is an ordinary state, not a refusal"
    );

    let good = file_at(dir.path(), "present-0600.json", 0o600);
    assert!(refuse_exposed_secret_file_if_present(&good, "the pool").is_ok());

    let bad = file_at(dir.path(), "present-0644.json", 0o644);
    assert!(
        refuse_exposed_secret_file_if_present(&bad, "the pool").is_err(),
        "present-and-exposed must refuse here exactly as it does in the strict variant -- \
         tolerating absence is the ONLY difference between the two"
    );
}

/// THE POOL, END TO END, and this is the site that matters most.

/// Not a source assertion: `load_pool_json` is called with a real file at a real mode. The two runs
/// differ only in the mode, and the verdict is which failure they reach -- a permission refusal, or
/// something further in that could only be reached by getting past the guard.
#[test]
fn load_pool_json_refuses_an_exposed_pool_and_reads_an_owner_only_one() {
    let dir = tempfile::tempdir().expect("create fixture dir");
    let body = r#"{"token_type":"shell","notes":[]}"#;

    let exposed = dir.path().join("exposed-pool.json");
    std::fs::write(&exposed, body).expect("write pool");
    std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(0o644))
        .expect("expose the pool");
    let error = crate::cli::commands::load_pool_json(&exposed)
        .expect_err("a world-readable pool holds every note's owner secret and must be refused")
        .to_string();
    assert!(
        error.contains("can be read by users other than its owner"),
        "the pool must be refused for its MODE, not for anything downstream: {error}"
    );

    let owner_only = dir.path().join("owner-only-pool.json");
    std::fs::write(&owner_only, body).expect("write pool");
    std::fs::set_permissions(&owner_only, std::fs::Permissions::from_mode(0o600))
        .expect("restrict the pool");
    let outcome = crate::cli::commands::load_pool_json(&owner_only);
    let rendered = outcome
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        !rendered.contains("can be read by users other than its owner"),
        "an owner-only pool must get PAST the guard; whatever happens after is not this test's \
         subject, but the permission refusal must not be it: {rendered}"
    );
}

/// The recovery file carries the same secret and gets the same treatment, absence included.
#[test]
fn the_note_deploy_recovery_reader_refuses_an_exposed_file() {
    let dir = tempfile::tempdir().expect("create fixture dir");

    let absent = dir.path().join("no-recovery.json");
    assert!(crate::cli::note::load_note_deploy_recovery(&absent)
        .expect("a recovery file that was never written is the ordinary case")
        .is_none());

    let exposed = dir.path().join("recovery.json");
    std::fs::write(&exposed, r#"{"schema":"x"}"#).expect("write recovery");
    std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(0o640))
        .expect("expose the recovery");
    let error = crate::cli::note::load_note_deploy_recovery(&exposed)
        .expect_err("a group-readable recovery file holds the note's owner secret")
        .to_string();
    assert!(
        error.contains("can be read by users other than its owner"),
        "{error}"
    );
}

/// `wallet_manual`: the CHECK BEFORE THE READ, asserted as an order.

/// This one cannot be driven at runtime and the reason is worth stating rather than working around:
/// the classify path is reached only from the interactive branch, where `prompt_line` refuses
/// outright when stdin is not a terminal, so a test can never get past it to the read. The property
/// under test is nevertheless exact -- the guard call precedes the read call in the body -- and this
/// is the instrument the tree already uses for it (`admin.rs`, `market_views.rs`, `buyer.rs`).

/// It is an order assertion over source, not a runtime observation of memory, and it is labelled
/// that way so nobody later cites it as more than it is.
#[test]
fn every_secret_read_is_preceded_by_the_permission_check() {
    for (source, signature, guard, read) in [
        (
            include_str!("wallet_manual.rs"),
            "fn resolve_manual_secret(args: &WalletOnboardManualArgs)",
            "refuse_exposed_secret_file(",
            "read_to_string(",
        ),
        (
            include_str!("commands.rs"),
            "pub(crate) fn load_pool_json(path: &std::path::Path)",
            "refuse_exposed_secret_file(",
            "std::fs::read(",
        ),
        (
            include_str!("note.rs"),
            "pub(crate) fn load_note_deploy_recovery(path: &Path)",
            "refuse_exposed_secret_file_if_present(",
            "std::fs::read(",
        ),
        (
            include_str!("wallet_goshai.rs"),
            "pub(crate) fn resume_onboarding(",
            "refuse_exposed_secret_file(",
            "read_to_string(",
        ),
    ] {
        let body = crate::cli::source_probe::code_of(source, signature);
        let at_guard = body
            .find(guard)
            .unwrap_or_else(|| panic!("`{signature}` does not check permissions at all:\n{body}"));
        let at_read = body
            .find(read)
            .unwrap_or_else(|| panic!("`{signature}` no longer reads with `{read}`"));
        assert!(
            at_guard < at_read,
            "`{signature}` reads the secret at {at_read} and checks its mode only at {at_guard}. A \
             guard that fires after the read has already loaded what it refuses"
        );
    }
}

/// The guard on the guard: comment stripping must actually remove something from those bodies, or
/// the order assertions above could be satisfied by prose. `admin.rs` measured this failure on its
/// own guards, which is why it is repeated here rather than assumed.
#[test]
fn the_order_assertions_read_code_and_not_comments() {
    let body = crate::cli::source_probe::code_of(
        include_str!("wallet_manual.rs"),
        "fn resolve_manual_secret(args: &WalletOnboardManualArgs)",
    );
    assert!(
        body.contains("refuse_exposed_secret_file("),
        "the real call must survive comment stripping: {body}"
    );
    assert!(
        !body.contains("THE CHECK COMES BEFORE THE READ"),
        "comment lines must be gone from the scanned body, or a commented-out call would pass"
    );
}
