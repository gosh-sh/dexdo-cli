//! what `note deploy` and `note topup` do about the wallet, before they touch the chain.

//! The resolver takes the store rather than reading the process-wide data directory, so every case
//! below is decided by a directory this test created -- never by whatever the machine running the
//! suite happens to have bound.

use super::{
    resolve_funding_wallet, resolve_funding_wallet_identity, FundingWallet, WalletBinding,
    WalletBindingCannotSign, WalletProvider, WalletStore, BINDING_VERSION,
};
use crate::Cli;
use clap::Parser as _;
use std::path::PathBuf;

const HOT: &str = "dd::11";
const BOUND_KEY: &str = "/secrets/bound-hot.key";

fn bound(store: &WalletStore, mutate: impl FnOnce(&mut WalletBinding)) {
    let mut binding = WalletBinding {
        network: crate::cli::wallet::test_network_a(),
        version: BINDING_VERSION,
        id: "0123456789abcdef0123456789abcdef".to_string(),
        provider: WalletProvider::Manual,
        hot_address: HOT.to_string(),
        vault_address: None,
        hot_key_file: Some(PathBuf::from(BOUND_KEY)),
        vault_key_file: None,
        hot_seed_file: None,
        push_profile_address: None,
    };
    mutate(&mut binding);
    // The secrets directory the id names. A real binding always has one -- `open_draft` creates it
    // before any key exists -- and since the reader refuses a record whose id names nothing,
    // so a fixture without it stands for state no onboarding can produce.
    std::fs::create_dir_all(store.bindings_dir().join(&binding.id))
        .expect("create the binding's secrets directory");
    store.commit_active(&binding).expect("commit the binding");
}

fn is_wallet_not_configured(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<dexdo_core::DexdoError>()
            .is_some_and(|dexdo| {
                dexdo.code() == dexdo_core::error_codes::E_WALLET_NOT_CONFIGURED.code()
            })
    })
}

/// The fail-fast itself. No wallet was passed and none is bound, so there is nothing to spend from
/// and the command must say so with the stable code rather than as a missing argument.
#[test]
fn no_flags_and_no_binding_is_e_wallet_not_configured() {
    let dir = tempfile::tempdir().unwrap();
    let store = WalletStore::at(dir.path().join("wallet"));
    let error = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect_err("a command that spends the Hot cannot proceed without one");
    assert!(
        is_wallet_not_configured(&error),
        "the refusal must carry E_WALLET_NOT_CONFIGURED: {error:#}"
    );
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("dexdo wallet onboard"),
        "the refusal must name the setup command, not just the problem: {rendered}"
    );
}

/// Without flags, the bound Hot and its recorded secret file are what the command spends from.
#[test]
fn without_flags_the_active_binding_supplies_the_wallet() {
    let dir = tempfile::tempdir().unwrap();
    let store = WalletStore::at(dir.path().join("wallet"));
    bound(&store, |_| {});
    let resolved = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect("the binding answers");
    assert_eq!(
        resolved,
        FundingWallet {
            address: HOT.to_string(),
            key: Some(PathBuf::from(BOUND_KEY)),
            seed_file: None,
        }
    );
}

/// The case the re-pointed parse test cannot cover, and the one that keeps every existing script
/// working: a passed-in wallet WINS. Binding a wallet must never silently move where an explicit
/// command spends from.
#[test]
fn an_explicit_multisig_address_wins_over_the_active_binding() {
    let dir = tempfile::tempdir().unwrap();
    let store = WalletStore::at(dir.path().join("wallet"));
    bound(&store, |_| {});
    let resolved = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        Some("0:explicit"),
        &Some(PathBuf::from("explicit.key")),
        &None,
    )
    .expect("an explicit wallet needs no binding");
    assert_eq!(
        resolved,
        FundingWallet {
            address: "0:explicit".to_string(),
            key: Some(PathBuf::from("explicit.key")),
            seed_file: None,
        },
        "neither the bound address nor the bound key file may leak into an explicit run"
    );
    assert_ne!(resolved.address, HOT);
    assert_ne!(resolved.key, Some(PathBuf::from(BOUND_KEY)));
}

/// An explicit wallet must not even READ the binding, so a corrupt one cannot break a run that
/// never needed it.
#[test]
fn an_explicit_wallet_is_unaffected_by_an_unreadable_binding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wallet");
    let store = WalletStore::at(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("binding.json"), b"{ this is not json").unwrap();
    let resolved = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        Some("0:explicit"),
        &Some(PathBuf::from("explicit.key")),
        &None,
    )
    .expect("an explicit wallet does not consult the binding at all");
    assert_eq!(resolved.address, "0:explicit");
}

/// A binding that exists but cannot be read is NOT "no wallet". Reporting it as the fail-fast would
/// send the operator into onboarding while a real Hot, possibly holding funds, is already bound --
/// and `wallet onboard` refuses when a binding exists, so they would be stuck between two refusals.
#[test]
fn an_unparseable_binding_is_an_error_and_not_a_silent_no_wallet() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wallet");
    let store = WalletStore::at(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("binding.json"), b"{ this is not json").unwrap();

    let error = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect_err("a binding that cannot be read must not be treated as absent");
    assert!(
        !is_wallet_not_configured(&error),
        "an unreadable binding is a different problem from an absent one: {error:#}"
    );
    assert!(
        format!("{error:#}").contains("parse wallet binding"),
        "the error must say the binding could not be read: {error:#}"
    );
}

/// The same rule for a binding from a version this build does not read: refused, not ignored.
#[test]
fn a_future_version_binding_is_refused_rather_than_treated_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wallet");
    let store = WalletStore::at(&root);
    bound(&store, |_| {});
    let path = store.binding_path(&crate::cli::wallet::test_network_a());
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["version"] = serde_json::json!(BINDING_VERSION + 1);
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let error = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect_err("a newer binding must not be read with fields this build ignores");
    assert!(!is_wallet_not_configured(&error), "{error:#}");
}

/// A binding with no local secret cannot sign. That is its own refusal: the operator's next move is
/// to re-bind or pass a key for this run, not to onboard from scratch.
#[test]
fn a_binding_with_no_local_secret_is_refused_as_itself() {
    let dir = tempfile::tempdir().unwrap();
    let store = WalletStore::at(dir.path().join("wallet"));
    bound(&store, |binding| {
        binding.hot_key_file = None;
        binding.hot_seed_file = None;
    });
    let identity = resolve_funding_wallet_identity(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect("recovery may resolve the public Hot without a signing credential");
    assert_eq!(identity.address, HOT);
    assert_eq!(identity.key, None);
    assert_eq!(identity.seed_file, None);
    let error = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect_err("a binding with no secret cannot fund a spend");
    assert!(!is_wallet_not_configured(&error), "{error:#}");
    assert!(error.downcast_ref::<WalletBindingCannotSign>().is_some());
    assert!(
        format!("{error:#}").contains("cannot sign a spend"),
        "{error:#}"
    );
}

/// A seed-file binding resolves to the seed input, not the key input.
#[test]
fn a_seed_file_binding_resolves_to_the_seed_input() {
    let dir = tempfile::tempdir().unwrap();
    let store = WalletStore::at(dir.path().join("wallet"));
    bound(&store, |binding| {
        binding.hot_key_file = None;
        binding.hot_seed_file = Some(PathBuf::from("/secrets/hot.seed"));
    });
    let resolved = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .unwrap();
    assert_eq!(resolved.key, None);
    assert_eq!(resolved.seed_file, Some(PathBuf::from("/secrets/hot.seed")));
}

/// The parser half of the same change: omitting the wallet is a run-time question for the binding,
/// and either `note deploy` flag half may be completed by its env equivalent. Those shapes must
/// reach the post-fallback pair validator instead of being rejected by clap. `note topup` has no
/// such env contract, so its existing pair rejections stay at the parser.
#[test]
fn note_deploy_reaches_binding_or_env_fallback_while_topup_pairing_still_rejects() {
    for command in [
        vec![
            "dexdo",
            "note",
            "deploy",
            "--nominal",
            "N100",
            "--pool",
            "p.json",
        ],
        vec![
            "dexdo",
            "note",
            "topup",
            "--note-addr",
            "0:note",
            "--to",
            "1",
        ],
        vec![
            "dexdo",
            "note",
            "deploy",
            "--multisig-private-key",
            "w.keys.json",
            "--nominal",
            "N100",
            "--pool",
            "p.json",
        ],
        vec![
            "dexdo",
            "note",
            "deploy",
            "--multisig-address",
            "0:wallet",
            "--nominal",
            "N100",
            "--pool",
            "p.json",
        ],
    ] {
        Cli::try_parse_from(command.clone())
            .unwrap_or_else(|error| panic!("{command:?} must reach binding/env fallback: {error}"));
    }
    for command in [
        vec![
            "dexdo",
            "note",
            "topup",
            "--note-addr",
            "0:note",
            "--to",
            "1",
            "--multisig-private-key",
            "w.keys.json",
        ],
        vec![
            "dexdo",
            "note",
            "topup",
            "--note-addr",
            "0:note",
            "--to",
            "1",
            "--multisig-address",
            "0:wallet",
        ],
    ] {
        assert!(
            Cli::try_parse_from(command.clone()).is_err(),
            "{command:?} must still be rejected by the parser"
        );
    }
}

/// Deploying a note or posting an order now onboards a wallet instead of dead-ending on the absence
/// of one -- but only for the absence, and only where an operator can answer. These pin the two
/// conditions the decision turns on, without running an onboarding: what counts as "nothing is
/// bound", and what a session with nobody on the other end does instead.
mod onboarding_on_demand {
    use super::*;
    use crate::cli::wallet::{is_wallet_not_configured, resolve_funding_wallet};

    /// The only refusal that may start an onboarding.
    #[test]
    fn an_absent_binding_is_recognised_as_the_one_refusal_onboarding_answers() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = WalletStore::at(temp.path().join("wallet"));

        let refusal = resolve_funding_wallet(
            &store,
            &crate::cli::wallet::test_network_a(),
            None,
            &None,
            &None,
        )
        .expect_err("nothing is bound");

        assert!(is_wallet_not_configured(&refusal), "{refusal:#}");
    }

    /// Every other way a wallet can be unusable must NOT start one. A binding that exists but
    /// records no local key is the operator's own wallet, half set up; onboarding over it would
    /// bind a second wallet and leave the first paying for nothing.
    #[test]
    fn a_keyless_binding_is_a_different_problem_and_must_not_onboard() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = WalletStore::at(temp.path().join("wallet"));
        bound(&store, |binding| {
            binding.hot_key_file = None;
            binding.hot_seed_file = None;
        });

        let refusal = resolve_funding_wallet(
            &store,
            &crate::cli::wallet::test_network_a(),
            None,
            &None,
            &None,
        )
        .expect_err("a binding with no key cannot sign");

        assert!(
            !is_wallet_not_configured(&refusal),
            "a half-configured binding must keep its own refusal: {refusal:#}"
        );
    }

    /// A corrupt binding file is not an absent one either: onboarding there would run while a
    /// funded Hot may well be bound, and the file is what has to be looked at.
    #[test]
    fn a_corrupt_binding_is_a_different_problem_and_must_not_onboard() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = WalletStore::at(temp.path().join("wallet"));
        let path = store.binding_path(&crate::cli::wallet::test_network_a());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"{ not json").expect("write");

        let refusal = resolve_funding_wallet(
            &store,
            &crate::cli::wallet::test_network_a(),
            None,
            &None,
            &None,
        )
        .expect_err("a corrupt binding cannot be read");

        assert!(!is_wallet_not_configured(&refusal), "{refusal:#}");
    }

    /// Under `cargo test` there is no terminal, which is exactly the state a script or a machine
    /// consumer is in: onboarding draws a code to scan and then waits, so it must not be started.
    #[test]
    fn a_session_with_nobody_on_the_other_end_does_not_start_an_onboarding() {
        assert!(!crate::cli::wallet::onboarding_can_be_run());
    }
}
