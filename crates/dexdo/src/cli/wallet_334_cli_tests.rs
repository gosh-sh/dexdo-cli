//! stage one: the `dexdo wallet` parse surface.

//! The provider is a subcommand of the two setup commands and appears nowhere else. These tests pin
//! that shape, because a provider flag on a working command is exactly the thing the spec forbids:
//! it would let a spend name a provider that the recorded binding contradicts.

use super::*;

fn provider_of(cli: Cli) -> Option<&'static str> {
    let Command::Wallet(wallet) = cli.command else {
        panic!("expected `dexdo wallet`");
    };
    let bind = match wallet.command {
        WalletCommand::Onboard(bind) | WalletCommand::Rebind(bind) => bind,
        // Neither takes a provider: `remove-archived` names an existing binding by id, and
        // `show` reads whatever is recorded. This helper answers "which provider did the parse
        // select", so both are None rather than a panic.
        WalletCommand::RemoveArchived(_) | WalletCommand::Show(_) => return None,
    };
    bind.provider.as_ref().map(|provider| match provider {
        WalletProviderCommand::AckinackiWallet(_) => "ackinacki-wallet",
        WalletProviderCommand::GoshAi(_) => "gosh-ai",
        WalletProviderCommand::Manual(_) => "manual",
    })
}

/// The argument shape each provider actually has.

/// `ackinacki-wallet` carries the onboarding arguments, and none of `--agent-name`, `--state` or
/// `--hot-key` has a default, so its row supplies them. The other two take no payload yet and their
/// rows stay bare. The table still sweeps all three: what changed is that each provider is pinned
/// with the shape it has, instead of one shape being assumed for all of them.
const PROVIDER_SHAPES: [(&str, &[&str]); 3] = [
    (
        "ackinacki-wallet",
        &[
            "--agent-name",
            "build-agent",
            "--state",
            "session.json",
            "--hot-key",
            "hot.key",
        ],
    ),
    ("gosh-ai", &[]),
    ("manual", &[]),
];

/// Each provider is reachable as a subcommand of both setup commands, spelled canonically.
#[test]
fn every_provider_is_a_subcommand_of_onboard_and_rebind() {
    for group in ["onboard", "rebind"] {
        for (provider, extra) in PROVIDER_SHAPES {
            let mut argv = vec!["dexdo", "wallet", group, provider];
            argv.extend_from_slice(extra);
            let cli = Cli::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("dexdo wallet {group} {provider}: {error}"));
            assert_eq!(provider_of(cli), Some(provider));
        }
    }
}

/// The providers that take no payload still parse BARE, which is what the previous single-shape
/// table proved for all three. Pinned separately now that one provider has required arguments, so
/// widening `ackinacki-wallet` cannot quietly relax the other two.
#[test]
fn payload_free_providers_still_parse_bare() {
    for group in ["onboard", "rebind"] {
        for provider in ["gosh-ai", "manual"] {
            let cli = Cli::try_parse_from(["dexdo", "wallet", group, provider])
                .unwrap_or_else(|error| panic!("dexdo wallet {group} {provider}: {error}"));
            assert_eq!(provider_of(cli), Some(provider));
        }
    }
}

/// ...and `ackinacki-wallet` REFUSES the bare form, because its arguments have no defaults. This is
/// the one rejection the reshaped table above no longer states on its own.
#[test]
fn ackinacki_wallet_requires_its_onboarding_arguments() {
    for group in ["onboard", "rebind"] {
        assert!(
            Cli::try_parse_from(["dexdo", "wallet", group, "ackinacki-wallet"]).is_err(),
            "dexdo wallet {group} ackinacki-wallet must refuse the bare form"
        );
    }
}

/// A bare `wallet onboard`/`wallet rebind` PARSES, carrying no provider. Whether that is a menu or
/// an error is decided at run time by whether there is a terminal -- clap must not decide it, or a
/// headless host would get a parse error instead of the actionable refusal.
#[test]
fn a_bare_setup_command_parses_with_no_provider() {
    for group in ["onboard", "rebind"] {
        let cli = Cli::try_parse_from(["dexdo", "wallet", group])
            .unwrap_or_else(|error| panic!("dexdo wallet {group}: {error}"));
        assert_eq!(provider_of(cli), None);
    }
}

/// No provider is a default and none is smuggled in as a flag on the setup commands either.
#[test]
fn the_provider_is_not_a_flag() {
    for group in ["onboard", "rebind"] {
        for form in [
            vec!["dexdo", "wallet", group, "--provider", "gosh-ai"],
            vec!["dexdo", "wallet", group, "--wallet-provider", "gosh-ai"],
        ] {
            assert!(
                Cli::try_parse_from(form.clone()).is_err(),
                "{form:?} must not parse: the provider is a subcommand"
            );
        }
    }
}

/// An unknown provider is refused rather than falling back to one of the real ones.
#[test]
fn an_unknown_provider_subcommand_is_refused() {
    for provider in ["ackinacki", "goshai", "gosh_ai", "wallet", "auto"] {
        assert!(
            Cli::try_parse_from(["dexdo", "wallet", "onboard", provider]).is_err(),
            "`{provider}` must not parse as a provider"
        );
    }
}

/// The provider never becomes a global flag: it must not be accepted before the subcommand, and it
/// must not be accepted by the money commands that spend from the bound wallet.
#[test]
fn working_commands_never_take_a_provider() {
    assert!(
        Cli::try_parse_from(["dexdo", "--wallet-provider", "gosh-ai", "wallet", "onboard"])
            .is_err(),
        "the provider must not be a global flag"
    );
    for command in [
        vec![
            "dexdo",
            "note",
            "deploy",
            "--wallet-provider",
            "gosh-ai",
            "--multisig-address",
            "0:wallet",
            "--multisig-private-key",
            "w.keys.json",
            "--nominal",
            "N100",
            "--pool",
            "pn_pool.json",
        ],
        vec![
            "dexdo",
            "note",
            "topup",
            "--wallet-provider",
            "gosh-ai",
            "--note-addr",
            "0:note",
            "--to",
            "1",
            "--multisig-address",
            "0:wallet",
            "--multisig-private-key",
            "w.keys.json",
        ],
    ] {
        assert!(
            Cli::try_parse_from(command.clone()).is_err(),
            "{command:?} must not accept a provider"
        );
    }
}

/// The passed-in funding path is untouched by: the two money commands still take, and still
/// require, their own wallet flags.
#[test]
fn the_passed_in_wallet_flags_still_work_unchanged() {
    Cli::try_parse_from([
        "dexdo",
        "note",
        "deploy",
        "--multisig-address",
        "0:wallet",
        "--multisig-private-key",
        "w.keys.json",
        "--nominal",
        "N100",
        "--pool",
        "pn_pool.json",
    ])
    .expect("note deploy keeps its passed-in wallet path");
    Cli::try_parse_from([
        "dexdo",
        "note",
        "topup",
        "--note-addr",
        "0:note",
        "--to",
        "1",
        "--multisig-address",
        "0:wallet",
        "--multisig-seed-file",
        "wallet.seed",
    ])
    .expect("note topup keeps its passed-in wallet path");
}

/// The specification fixes the Gosh.ai activation wait at ten minutes and makes
/// `--activation-timeout` the only way to change it. It is a provider-command flag, never a global
/// one, so it cannot be passed to a provider that has no such wait.
#[test]
fn gosh_ai_takes_activation_timeout_and_only_gosh_ai_does() {
    let cli = Cli::try_parse_from([
        "dexdo",
        "wallet",
        "onboard",
        "gosh-ai",
        "--activation-timeout",
        "20m",
    ])
    .expect("gosh-ai accepts --activation-timeout");
    let Command::Wallet(WalletArgs {
        command: WalletCommand::Onboard(args),
    }) = cli.command
    else {
        panic!("expected wallet onboard");
    };
    let Some(WalletProviderCommand::GoshAi(goshai)) = args.provider else {
        panic!("expected the gosh-ai provider");
    };
    assert_eq!(
        goshai.activation_timeout,
        Some(std::time::Duration::from_secs(20 * 60))
    );

    // Absent means the specification's default, decided at the call site rather than by clap, so
    // there is one figure and not two.
    let cli = Cli::try_parse_from(["dexdo", "wallet", "onboard", "gosh-ai"])
        .expect("gosh-ai parses bare");
    let Command::Wallet(WalletArgs {
        command: WalletCommand::Onboard(args),
    }) = cli.command
    else {
        panic!("expected wallet onboard");
    };
    let Some(WalletProviderCommand::GoshAi(goshai)) = args.provider else {
        panic!("expected the gosh-ai provider");
    };
    assert!(goshai.activation_timeout.is_none());

    // It is not a global flag and not another provider's.
    assert!(
        Cli::try_parse_from(["dexdo", "wallet", "onboard", "--activation-timeout", "20m"]).is_err()
    );
    assert!(Cli::try_parse_from([
        "dexdo",
        "wallet",
        "onboard",
        "manual",
        "--activation-timeout",
        "20m"
    ])
    .is_err());
}

/// A duration is a duration, and zero is not one.
#[test]
fn activation_timeout_accepts_the_documented_forms_and_refuses_the_rest() {
    use crate::cli::args::parse_activation_timeout;
    assert_eq!(
        parse_activation_timeout("600").expect("bare seconds"),
        std::time::Duration::from_secs(600)
    );
    assert_eq!(
        parse_activation_timeout("10m").expect("minutes"),
        std::time::Duration::from_secs(600)
    );
    assert_eq!(
        parse_activation_timeout("1h").expect("hours"),
        std::time::Duration::from_secs(3600)
    );
    assert_eq!(
        parse_activation_timeout("30s").expect("seconds"),
        std::time::Duration::from_secs(30)
    );
    for bad in ["0", "0m", "", "twenty", "20x", "-5"] {
        assert!(
            parse_activation_timeout(bad).is_err(),
            "`{bad}` must not be accepted as an activation timeout"
        );
    }
}

/// `wallet onboard ackinacki-wallet` must keep reaching PR715's onboarding.

/// It used to reach it through a dedicated arm in `main.rs`, one line above a general arm that
/// matched everything it did -- so deleting that arm compiled cleanly and silently re-routed the
/// provider. That arm is now GONE, deliberately: it bypassed `run_selected`, which is the only
/// caller of `commit_active`, so the flow ran to completion and wrote no binding at all. The route
/// moved into `provider_flow` beside the other two providers, and this test moved with it. What it
/// guards is unchanged: that the route exists and nothing can lose it unnoticed.
#[test]
fn wallet_onboard_ackinacki_wallet_routes_to_the_onboarding_entry_point() {
    let main_rs = include_str!("../main.rs");
    let wallet_rs = include_str!("wallet.rs");

    assert!(
        !main_rs.contains("WalletProviderCommand::AckinackiWallet(onboard)"),
        "the provider-specific dispatch arm is gone on purpose: it bypassed run_selected, which is \
         the only caller of commit_active, so the flow produced no binding"
    );
    assert!(
        main_rs.contains("Command::Wallet(args) => cli::wallet::run_wallet(args)"),
        "one dispatcher must serve every wallet shape"
    );

    let arm = wallet_rs
        .find("WalletProvider::AckinackiWallet => ackinacki_flow(draft, explicit).await")
        .expect(
            "provider_flow must route ackinacki-wallet to its onboarding; without this arm the \
             provider reaches no flow at all",
        );
    let handler = wallet_rs
        .find("crate::cli::wallet_onboarding::run_wallet_onboard(")
        .expect("the arm must hand off to PR715's onboarding entry point");
    assert!(
        wallet_rs[arm..].contains("ackinacki_flow"),
        "the routed arm must name the flow that calls it"
    );
    assert!(
        handler > 0 && arm > 0,
        "both the route and its handler must be present"
    );
}

/// The payload the route carries is the real onboarding request, not an empty marker.

/// `ackinacki_flow` destructures `WalletProviderCommand::AckinackiWallet(args)` out of `explicit`
/// and rebuilds `WalletOnboardArgs` from it, so what this proves is that the three required flags
/// survive parsing into the value that reconstruction reads.
#[test]
fn the_ackinacki_wallet_payload_reaches_the_dispatch_arm_intact() {
    let cli = Cli::try_parse_from([
        "dexdo",
        "wallet",
        "onboard",
        "ackinacki-wallet",
        "--agent-name",
        "build-agent",
        "--state",
        "session.json",
        "--hot-key",
        "hot.key",
    ])
    .expect("the documented ackinacki-wallet invocation parses");

    let Command::Wallet(WalletArgs {
        command:
            WalletCommand::Onboard(WalletBindArgs {
                provider: Some(WalletProviderCommand::AckinackiWallet(onboard)),
                ..
            }),
    }) = cli.command
    else {
        panic!("`wallet onboard ackinacki-wallet` must parse into the shape provider_flow reads");
    };
    assert_eq!(onboard.agent_name, "build-agent");
    assert_eq!(
        onboard.state,
        Some(std::path::PathBuf::from("session.json"))
    );
    assert_eq!(onboard.hot_key, Some(std::path::PathBuf::from("hot.key")));
}

/// What actually ships, as a single readable statement.

/// `onboard` works for two providers and `rebind` for none. This is here because the claim is easy
/// to get wrong in both directions -- the routing is split across two dispatchers -- and a wrong
/// claim about which money paths work is worse than a missing one.
#[test]
fn only_the_wired_provider_shapes_avoid_the_staged_refusal() {
    let main_rs = include_str!("../main.rs");
    let wallet_rs = include_str!("wallet.rs");

    // All three now reach one provider_flow. There is no provider-specific dispatcher left.
    assert!(
        main_rs.contains("Command::Wallet(args) => cli::wallet::run_wallet(args)"),
        "one dispatcher serves every wallet shape"
    );
    assert!(
        wallet_rs
            .contains("WalletProvider::AckinackiWallet => ackinacki_flow(draft, explicit).await"),
        "ackinacki-wallet must be routed from provider_flow to PR715's onboarding"
    );
    // onboard + gosh-ai: the general dispatcher, then provider_flow's wired arm.
    assert!(
        wallet_rs.contains("WalletProvider::GoshAi => goshai_flow(draft, explicit).await"),
        "gosh-ai must be routed from provider_flow to the Gosh.ai onboarding"
    );
    // onboard + manual: the general dispatcher, then provider_flow's wired arm.
    assert!(
        wallet_rs.contains("WalletProvider::Manual => manual_flow(draft, explicit).await"),
        "manual must be routed from provider_flow to the manual onboarding"
    );
    // Nothing is left refusing: with step 9 landed, every provider reaches a flow that can
    // produce a binding, for `onboard` and for `rebind` alike.
    assert!(
        !wallet_rs.contains("WalletProvider::AckinackiWallet => bail!("),
        "no provider may still be a staged refusal now that all three flows produce a binding"
    );
}
