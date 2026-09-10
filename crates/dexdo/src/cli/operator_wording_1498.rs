//! what an operator reads about funding a wallet names the multisig and the next move.

//! Asserted against the ACTION, never against length or prose: a test that says "contains this
//! sentence" pins the thing that must stay free to improve. What must not change is that each
//! refusal sends the operator somewhere specific, that two refusals needing different actions do
//! not share one text, and that no help line sends them to something this client has no command
//! for.

/// The word this file exists to keep out of everything an operator reads, spelled by construction
/// rather than written out: the acceptance for this work is that a search of `crates/dexdo/src`
/// finds it nowhere, and a check that names what it forbids flags itself. `ci/check_no_cyrillic.sh`
/// escapes the range it rejects for exactly this reason.
const RETIRED_TERM: &str = concat!("gi", "ver");

fn address() -> dexdo_core::CanonicalAddress {
    dexdo_core::CanonicalAddress::parse(
        "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .expect("canonical test address")
}

fn refusals() -> Vec<(&'static str, String)> {
    vec![
        (
            "unreadable",
            super::operator_wallet_unreadable_refusal(&address(), &"pool timed out"),
        ),
        (
            "shortfall",
            super::operator_wallet_shortfall_refusal(&address(), "1", "10001", "10000"),
        ),
        (
            "undeployable",
            super::operator_wallet_undeployable_account_refusal(&address(), "Frozen"),
        ),
    ]
}

/// Every way `dexdo note wallet` can stop before it has a deployable wallet leaves the operator
/// something to run from the same shell.

/// A bare mention of `dexdo` is deliberately not accepted as an action: "dexdo cannot fund it" names
/// the client and nothing to do, and that sentence is exactly what this file was opened over.
#[test]
fn every_operator_wallet_refusal_names_something_to_do() {
    for (class, refusal) in refusals() {
        assert!(
            refusal.contains("--")
                || refusal.contains("run this command again")
                || refusal.contains("rerun this command"),
            "{class}: no flag and no repeat to act on: {refusal}"
        );
    }
}

/// And none of them asks the operator to understand something this client has no command for.
/// A refusal that has to be decoded before it can be acted on is a log line in the wrong stream.
#[test]
fn no_operator_wallet_refusal_names_a_concept_the_client_has_no_command_for() {
    for (class, refusal) in refusals() {
        assert!(
            !refusal.to_lowercase().contains(RETIRED_TERM),
            "{class}: the operator has to learn a word before they can read the refusal: {refusal}"
        );
    }
}

/// And it says what did not happen, in the operator's units, rather than what the client is
/// internally unwilling to do.
#[test]
fn every_operator_wallet_refusal_says_what_did_not_happen() {
    for (class, refusal) in refusals() {
        assert!(
            refusal.contains("no chain write")
                || refusal.contains("deployed nothing")
                || refusal.contains("did not deploy"),
            "{class}: the operator is not told what failed to happen: {refusal}"
        );
    }
}

/// Three problems, three answers: an endpoint that did not respond, a wallet that is short, and an
/// address that is occupied. None of them may arrive under another one's sentence.
#[test]
fn refusals_needing_different_actions_do_not_share_a_text() {
    let all = refusals();
    for (i, (left_class, left)) in all.iter().enumerate() {
        for (right_class, right) in all.iter().skip(i + 1) {
            assert_ne!(
                left, right,
                "{left_class} and {right_class} need different actions and share one text"
            );
        }
    }

    let unreadable = &all[0].1;
    // It used to require the literal `--endpoint`. That flag is gone, and the assertion
    // outlived it: it kept passing while the refusal sent the operator to a flag the binary would
    // reject, which is worse than naming nothing -- they would have run it and been told the flag
    // does not exist, with the unreadable endpoint still unexplained. What has to be named is
    // whatever actually selects the endpoint now, and that is the manifest variable.
    assert!(
        unreadable.contains(dexdo_core::params::MANIFEST_PATH_VAR),
        "a read that never answered is retried against another endpoint, and the only thing that \
         chooses one is unnamed: {unreadable}"
    );
    assert!(
        !unreadable.contains("--endpoint"),
        "the refusal sends the operator to a flag this build does not accept: {unreadable}"
    );

    let shortfall = &all[1].1;
    assert!(
        shortfall.contains("send at least") && shortfall.contains("rerun this command"),
        "a deployed wallet that is short is topped up and the command rerun: {shortfall}"
    );
    assert!(
        !shortfall.contains("--note-key"),
        "a different key does not fill a wallet that is merely short: {shortfall}"
    );
}

/// The negative half, and the one that matters most here: an address already occupied by an
/// account the canonical wallet cannot be written over is not a money problem. Sending SHELL at it
/// buys the operator a second refusal, so the refusal must not ask for any.
#[test]
fn an_occupied_address_does_not_send_the_operator_to_spend() {
    let refusal = super::operator_wallet_undeployable_account_refusal(&address(), "Frozen");
    assert!(
        !refusal.contains("SHELL") && !refusal.contains("send"),
        "money does not change an account state: {refusal}"
    );
    assert!(
        refusal.contains("--note-key"),
        "the address comes from the key, so a free address means another key: {refusal}"
    );
}

/// The staged funding block is an instruction, and it holds the same two halves: nothing moved,
/// and here is who moves it and what to rerun afterwards.
#[test]
fn the_funding_block_names_who_moves_the_shell_and_what_to_rerun() {
    let rendered =
        super::render_operator_wallet_funding(&address(), crate::cli::note::NoteNominal::N10000);
    assert!(
        rendered.contains("Dexdo sent nothing to it"),
        "what did not happen: {rendered}"
    );
    assert!(
        rendered.contains("rerunning this command"),
        "what to do, runnable from this shell: {rendered}"
    );
}

/// Every help text the operator can reach, at every depth of the command tree.
fn help_texts(command: &mut clap::Command) -> Vec<String> {
    let mut rendered = vec![command.render_long_help().to_string()];
    let names: Vec<String> = command
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        let sub = command
            .find_subcommand_mut(&name)
            .expect("declared subcommand");
        rendered.extend(help_texts(sub));
    }
    rendered
}

/// Help may not spend the operator's attention on something the client has no command for. The
/// funding flags used to qualify the multisig against an alternative that does not exist, which
/// obliged the reader to learn a word before they could read the line.
#[test]
fn no_help_line_names_something_the_client_has_no_command_for() {
    use clap::CommandFactory;
    let mut command = crate::Cli::command();
    for help in help_texts(&mut command) {
        let lowered = help.to_lowercase();
        assert!(
            !lowered.contains(RETIRED_TERM),
            "help names something no command of this client can reach:\n{help}"
        );
    }
}

/// And the removal took the qualifier, not the meaning: both funding flags still say that a
/// deployed multisig address is what pays.
#[test]
fn both_funding_flags_still_say_a_multisig_pays() {
    use clap::CommandFactory;
    let mut command = crate::Cli::command();
    let all = help_texts(&mut command).join("\n");
    for surface in ["funds the note", "funds the top-up"] {
        assert!(
            all.contains(surface),
            "the funding flag no longer says what pays: {surface}"
        );
    }
    assert!(
        all.contains("--multisig-address"),
        "the flag that names the paying wallet is gone from help"
    );
}
