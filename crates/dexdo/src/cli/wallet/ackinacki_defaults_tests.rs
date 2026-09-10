//! follow-up item 6: `wallet onboard ackinacki-wallet` has owner-only default paths.

//! # The defect

//! `--state` and `--hot-key` had no defaults, so choosing `ackinacki-wallet` from the interactive
//! provider menu could not run at all: a menu carries no command line, and the flow answered with an
//! error saying the menu could not supply its arguments. The operator picked a provider from a list
//! the CLI itself offered and was told to start again.

//! # Why the default is not simply "the working directory"

//! `--hot-key` names a generated PRIVATE KEY. `--contracts` and `--models` may default to a relative
//! path and land wherever the operator is standing; a secret may not. So the canonical default is
//! resolved inside the owner-only binding draft reserved for this exact attempt. Two onboarding
//! attempts therefore cannot alias or overwrite one another. Anything the operator passes is used
//! exactly as given.

//! # How the menu path is proved here

//! By calling `ackinacki_onboard_args`, the pure function `ackinacki_flow` actually uses, and
//! asserting the request it returns. This module previously asserted that certain refusal phrases
//! were ABSENT from `wallet.rs`. That shape cannot fail usefully: rewording the refusal, respacing
//! it, or moving it one module over all satisfy it, and none of them mean the operator can onboard.

use clap::Parser as _;

/// The menu path must REACH onboarding, with a request that can actually run.

/// The interactive menu carries no command line, so `provider_flow` hands this provider
/// `explicit: None`. That arm used to be a dead end that only told the operator to start again; it
/// must now produce exactly the request the bare subcommand makes.

/// Every field is asserted, not just the two that gained defaults. An argument whose value nothing
/// pins is free to drift, and this is the one code path where no human is present to notice that
/// the request went out naming the wrong network or a stray endpoint.
#[test]
fn the_interactive_menu_no_longer_dead_ends_on_the_arguments_error() {
    let binding_dir = std::path::Path::new("/data/wallet/bindings/binding-a");
    let from_the_menu = super::ackinacki_onboard_args(None, binding_dir);

    assert_eq!(
        from_the_menu.agent_name,
        dexdo_core::params::WALLET_ONBOARD_DEFAULT_AGENT_NAME,
        "the menu cannot ask for an agent name, so it must supply the constant the durable bee \
         session can be resumed under"
    );
    assert_eq!(
        from_the_menu.state,
        Some(binding_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH)),
        "the durable session must be private to this binding attempt"
    );
    assert_eq!(
        from_the_menu.hot_key,
        Some(binding_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH)),
        "the generated private key must be private to this binding attempt"
    );
    // The endpoint assertion that stood here is gone with the field. It said "the menu
    // must not invent an endpoint; the network selects it", and there is now no endpoint for the
    // menu to invent: `--endpoint` was removed from every command, so the manifest is the only
    // thing that names one. The property is held by the type, which is the stronger place for it.
    assert!(
        from_the_menu.vault_key.is_none(),
        "a distinct Vault key is opt-in, and the menu never opts in on the operator's behalf"
    );
}

/// The other half of the same function: an explicit command line is carried through UNCHANGED.

/// Without this, `ackinacki_onboard_args` could return the canonical defaults for every input and
/// the menu assertion above would still pass -- while every operator who typed `--hot-key` silently
/// had their private key written somewhere else. The command line is driven through clap rather than
/// hand-built, so what is proved is the real parse-to-request path.
#[test]
fn an_explicit_command_line_is_not_replaced_by_the_menu_defaults() {
    let parsed = crate::Cli::try_parse_from([
        "dexdo",
        "wallet",
        "onboard",
        "ackinacki-wallet",
        "--agent-name",
        "build-agent",
        "--state",
        "chosen-session.json",
        "--hot-key",
        "chosen-hot.key",
    ])
    .expect("the documented ackinacki-wallet invocation parses");

    let Some(crate::cli::args::WalletCommand::Onboard(onboard)) = wallet_command(&parsed) else {
        panic!("expected `wallet onboard`");
    };
    let provider = onboard
        .provider
        .as_ref()
        .expect("the provider subcommand is present");

    let requested = super::ackinacki_onboard_args(
        Some(provider),
        std::path::Path::new("/data/wallet/bindings/must-not-be-used"),
    );

    assert_eq!(requested.agent_name, "build-agent");
    assert_eq!(
        requested.state,
        Some(std::path::PathBuf::from("chosen-session.json")),
        "an operator-supplied session path must survive into the request"
    );
    assert_eq!(
        requested.hot_key,
        Some(std::path::PathBuf::from("chosen-hot.key")),
        "an operator-supplied private-key path must never be replaced by the default"
    );
}

/// Provenance, not spelling, decides whether a path is resolved under the binding draft.
#[test]
fn explicit_paths_equal_to_the_default_filenames_remain_exact() {
    let parsed = crate::Cli::try_parse_from([
        "dexdo",
        "wallet",
        "onboard",
        "ackinacki-wallet",
        "--agent-name",
        "build-agent",
        "--state",
        dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH,
        "--hot-key",
        dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH,
    ])
    .unwrap();
    let Some(crate::cli::args::WalletCommand::Onboard(onboard)) = wallet_command(&parsed) else {
        panic!("expected `wallet onboard`");
    };
    let requested = super::ackinacki_onboard_args(
        onboard.provider.as_ref(),
        std::path::Path::new("/data/wallet/bindings/must-not-be-used"),
    );

    assert_eq!(
        requested.state,
        Some(std::path::PathBuf::from(
            dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH
        ))
    );
    assert_eq!(
        requested.hot_key,
        Some(std::path::PathBuf::from(
            dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH
        ))
    );
}

/// Rebind reserves a fresh binding id. Its defaults must therefore name fresh files rather than
/// overwriting the session and Hot key retained by the archived binding.
#[test]
fn two_binding_ids_never_alias_or_overwrite_their_default_secrets() {
    let first_dir = std::path::Path::new("/data/wallet/bindings/binding-a");
    let second_dir = std::path::Path::new("/data/wallet/bindings/binding-b");
    let first = super::ackinacki_onboard_args(None, first_dir);
    let second = super::ackinacki_onboard_args(None, second_dir);

    assert_eq!(
        first.state,
        Some(first_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH))
    );
    assert_eq!(
        first.hot_key,
        Some(first_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH))
    );
    assert_eq!(
        second.state,
        Some(second_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH))
    );
    assert_eq!(
        second.hot_key,
        Some(second_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH))
    );
    assert_ne!(first.state, second.state, "durable sessions must not alias");
    assert_ne!(first.hot_key, second.hot_key, "Hot secrets must not alias");
}

/// The two flags now parse without being given, which is what makes the menu path possible.

/// `--agent-name` deliberately stays required on the SUBCOMMAND: it is the label a human reads in
/// the wallet app when approving, so on a command line the operator chooses it. Only the menu, which
/// has no command line, falls back to the canonical default.
#[test]
fn state_and_hot_key_are_no_longer_required_on_the_command_line() {
    let parsed = crate::Cli::try_parse_from([
        "dexdo",
        "wallet",
        "onboard",
        "ackinacki-wallet",
        "--agent-name",
        "laptop",
    ])
    .expect("--state and --hot-key must have defaults");

    let Some(crate::cli::args::WalletCommand::Onboard(onboard)) = wallet_command(&parsed) else {
        panic!("expected `wallet onboard`");
    };
    let Some(crate::cli::args::WalletProviderCommand::AckinackiWallet(args)) =
        onboard.provider.as_ref()
    else {
        panic!("expected the ackinacki-wallet provider");
    };

    assert_eq!(args.state, None);
    assert_eq!(args.hot_key, None);
}

/// The canonical defaults are distinct paths, and the key is not written beside a non-secret by
/// accident. A single shared default would put the durable session and a private key in one file.
#[test]
fn the_two_canonical_defaults_are_distinct() {
    assert_ne!(
        dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH,
        dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH
    );
}

fn wallet_command(cli: &crate::Cli) -> Option<&crate::cli::args::WalletCommand> {
    match &cli.command {
        crate::Command::Wallet(args) => Some(&args.command),
        _ => None,
    }
}
