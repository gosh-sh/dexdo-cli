//! `note deploy` and `note topup` spend from ONE funding multisig, so they take ONE lock
//! under ONE key.

//! Before this, `note topup` took no wallet lock at all and `note deploy`'s was keyed on the wallet
//! address alone. Two concurrent commands could therefore draw on the same multisig at the same
//! time, which is the shape that produces a spend the operator cannot account for. The lock is now
//! keyed on `(network, wallet)` and both commands take it before anything they do can lead to a
//! spend.

//! The first test drives the REAL `run_note_topup` entry point -- not a helper, and with no
//! end state fabricated -- against a funding wallet whose turn is already taken exactly the way
//! `run_note_deploy` takes it. Without the fix `note topup` holds no lock, walks straight past the
//! held turn and fails at the chain read instead; with it, the command refuses before it connects.

/// The DApp every dexdo contract lives in, in the canonical 64-hex form `CanonicalAddress` takes.
const DEXDO_DAPP_ID: &str = "0000000000000000000000000000000000000000000000000000000000000004";

/// A funding wallet nothing else on this machine can be using.

/// Unique per process AND per run: the lock deliberately lives in one machine-wide directory, so a
/// fixed address here would make two concurrent test binaries -- `--lib` and `--bins` are two --
/// contend for one real lock file and turn this test into a race.
fn unique_wallet_account_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!("{:032x}{:032x}", std::process::id(), nanos)
}

/// The smallest manifest `Deployed::load` accepts, so the network in the key comes from the same
/// field on both sides rather than from a constant this test made up.
fn write_manifest(dir: &std::path::Path, network: &str) -> std::path::PathBuf {
    let path = dir.join(format!("deployed.{network}.json"));
    let account = "1".repeat(64);
    std::fs::write(
        &path,
        format!(
            r#"{{"network":"{network}","superroot":"0:{account}","dapp_config":"0:{account}","dapp_id":"{DEXDO_DAPP_ID}"}}"#
        ),
    )
    .expect("write deployed manifest");
    path
}

/// The manifest is no longer an argument: `run_note_topup` reads `DEXDO_MANIFEST`, which is
/// what says which chain the run is on and therefore what keys the funding-wallet lock. A test that
/// only passed a path left the command reading whatever manifest the tree falls back to, so the
/// lock it took and the lock the command looked for were keyed on two different networks -- and the
/// command sailed past the turn this test is about and failed at a chain read instead.
fn topup_args(wallet_address: &str) -> crate::cli::args::NoteTopupArgs {
    crate::cli::args::NoteTopupArgs {
        note_addr: format!("0:{}", "1291face".repeat(8)),
        to_raw: 350_000_000_000,
        // Explicit, so this test proves the lock without depending on a durable binding: with
        // the argument is optional and an absent one would send `run_note_topup` to the wallet
        // store, which is a different path from the one under proof here.
        multisig_address: Some(wallet_address.to_string()),
        multisig_private_key: None,
        multisig_seed_file: None,
        // A dead endpoint on purpose: if the command ever gets past the wallet's turn, it fails at
        // the chain read with a connection error, which is a different message from the refusal
        // this test requires. The two outcomes are therefore never confusable.
        // The documented default, which is what this fixture always stood for: the flag is new
        // and none of this test's assertions are about it.
        funding_timeout: None,
    }
}

/// The canonical bounded wait is an hour, which is the right answer for a real deploy and the wrong
/// one for a test. This is the operator's own documented knob, not a seam added for the test.
const LOCK_TIMEOUT_VAR: &str = "DEXDO_NOTE_DEPLOY_LOCK_TIMEOUT_SECS";

/// The regression: the real `note topup` command refuses while the funding wallet's turn is held.

/// The turn is taken here the way `run_note_deploy` takes it -- same helper, same
/// `(network, --multisig-address)` pair -- and with the wallet written in its canonical
/// DApp-qualified form while `note topup` is handed the legacy form of the SAME wallet. A key that
/// did not collapse the two forms would put the two commands on two lock files that never see each
/// other, so this also proves they land on one.
#[tokio::test]
async fn note_topup_refuses_while_note_deploy_holds_the_funding_wallet_1291() {
    let temp = tempfile::tempdir().expect("temp dir");
    let manifest = write_manifest(temp.path(), "net-a");
    let account_id = unique_wallet_account_id();
    let canonical_wallet = format!("{DEXDO_DAPP_ID}::{account_id}");
    let legacy_wallet = format!("0:{account_id}");

    let held = super::acquire_funding_wallet_lock_with_timeout(
        "net-a",
        &canonical_wallet,
        std::time::Duration::from_secs(5),
    )
    .expect("the first spender takes the funding wallet's turn");

    let previous = std::env::var(LOCK_TIMEOUT_VAR).ok();
    std::env::set_var(LOCK_TIMEOUT_VAR, "1");
    // Thread-local, not `set_var`: the environment is process-wide and these tests run in parallel,
    // so a fixture set here was read by unrelated tests as the manifest of their own run.
    let _manifest_here = crate::cli::commands::manifest_for_this_thread(&manifest);
    let outcome = super::run_note_topup(topup_args(&legacy_wallet)).await;
    drop(_manifest_here);
    match previous {
        Some(value) => std::env::set_var(LOCK_TIMEOUT_VAR, value),
        None => std::env::remove_var(LOCK_TIMEOUT_VAR),
    }

    let error = outcome.expect_err(
        "note topup must not spend from a wallet another dexdo command is spending from",
    );
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("funding wallet busy"),
        "note topup must refuse on the funding wallet's turn, got: {rendered}"
    );
    // An operator multisig is a SELF-DApp account -- its DApp id is its own account id -- which is
    // how the chain carries it and how `dexdo` prints it. `{DEXDO_DAPP_ID}::<acct>`
    // is the shared-DApp spelling and belongs to notes and the order book, not to this wallet, so
    // accepting it here would have let the refusal name an address that does not exist. The lock
    // KEY is unaffected: it is built from the legacy form, which is why the holder's spelling and
    // topup's spelling still land on one lock.
    let self_dapp_wallet = format!("{account_id}::{account_id}");
    assert!(
        rendered.contains(&self_dapp_wallet),
        "the refusal must name the wallet it waited for, in the form dexdo prints it, got: {rendered}"
    );
    // The turn is taken BEFORE the level is read. A refusal that arrived after the chain read would
    // mean the decision to spend had already been made from a reading the other command can change.
    assert!(
        !rendered.contains("read PrivateNote account"),
        "note topup must refuse before it reads the note, got: {rendered}"
    );

    drop(held);
    let path = super::funding_wallet_lock_path("net-a", &legacy_wallet).expect("lock path");
    let _ = std::fs::remove_file(&path);
}

/// The turn is released when the holder goes away, and the next command gets it.
#[test]
fn funding_wallet_turn_is_released_when_the_holder_drops_1291() {
    let account_id = unique_wallet_account_id();
    let wallet = format!("0:{account_id}");

    let held = super::acquire_funding_wallet_lock_with_timeout(
        "net-a",
        &wallet,
        std::time::Duration::from_secs(5),
    )
    .expect("first holder");
    let contender = std::thread::spawn({
        let wallet = wallet.clone();
        move || {
            super::acquire_funding_wallet_lock_with_timeout(
                "net-a",
                &wallet,
                std::time::Duration::from_secs(1),
            )
            .expect_err("a second spender must not get the wallet while it is held")
        }
    });
    let error = contender.join().expect("contender thread").to_string();
    assert!(error.contains("funding wallet busy: waited 1s"), "{error}");

    drop(held);
    let regained = super::acquire_funding_wallet_lock_with_timeout(
        "net-a",
        &wallet,
        std::time::Duration::from_secs(5),
    )
    .expect("the turn is free once the holder drops");
    drop(regained);
    let path = super::funding_wallet_lock_path("net-a", &wallet).expect("lock path");
    let _ = std::fs::remove_file(&path);
}

/// The key carries the network, and collapses the two accepted spellings of one wallet.
#[test]
fn funding_wallet_lock_key_separates_networks_and_joins_address_forms_1291() {
    let account_id = "c0de1291".repeat(8);
    let legacy = format!("0:{account_id}");
    let canonical = format!("{DEXDO_DAPP_ID}::{account_id}");

    let net_a_legacy = super::funding_wallet_lock_path("net-a", &legacy).expect("legacy");
    let net_a_canonical = super::funding_wallet_lock_path("net-a", &canonical).expect("canonical");
    let net_b_legacy = super::funding_wallet_lock_path("mainnet", &legacy).expect("mainnet");

    assert_eq!(
        net_a_legacy, net_a_canonical,
        "one wallet written two ways must take ONE turn, or the two commands never see each other"
    );
    assert_ne!(
        net_a_legacy, net_b_legacy,
        "the same address on two networks is different money and must not serialise ()"
    );
    assert!(
        super::funding_wallet_lock_path("", &legacy).is_err(),
        "a manifest with no network cannot tell one chain's wallet from another's"
    );
    assert!(
        super::funding_wallet_lock_path("net-a", "not-an-address").is_err(),
        "an unparseable wallet must fail rather than hash into some other wallet's turn"
    );
}

/// Both spenders take the turn, in production, before they can spend.

/// `run_note_deploy` reaches the chain (a clock-skew preflight) before it reaches the lock, so it
/// cannot be driven offline the way `run_note_topup` can. Its call site is pinned here in the same
/// way this file already pins the machine-wide prover turn.
#[test]
fn both_wallet_spenders_take_the_funding_wallet_turn_in_production_1291() {
    let source = include_str!("note_cmd.rs");
    let production = source
        .split_once("#[cfg(test)]\nmod tests")
        .expect("note_cmd unit-test module boundary")
        .0;
    // Keyed on the call and its network argument, not on the wallet expression. The two spenders
    // name the resolved wallet differently -- `note deploy` has it as `funding_multisig_address`,
    // `note topup` as `funding_wallet.address` -- and NEITHER may use `args.multisig_address`,
    // which with is an `Option` that is empty exactly when the binding supplied the wallet.
    assert_eq!(
        production
            .matches("acquire_funding_wallet_lock(&funding_network,")
            .count(),
        2,
        "note deploy AND note topup must both take the funding wallet's turn under the same key"
    );
    assert_eq!(
        production
            .matches("acquire_funding_wallet_lock(&funding_network, &args.multisig_address)")
            .count(),
        0,
        "the turn must be taken on the RESOLVED wallet; `args.multisig_address` can be None while a \
         wallet from the durable binding is about to be spent, and a lock on an absent argument \
         guards nothing"
    );
    for entry in ["run_note_deploy", "run_note_topup"] {
        let start = production
            .find(&format!("pub(crate) async fn {entry}"))
            .unwrap_or_else(|| panic!("{entry} present"));
        assert!(
            production[start..].contains("acquire_funding_wallet_lock("),
            "{entry} must take the funding wallet's turn"
        );
    }
}
