//! Which values the operator actually gave, as opposed to the ones clap filled in.

//! A struct field carries a value and says nothing about where it came from: `args.ticks == 2` reads
//! the same whether the operator wrote `--ticks 2` or the default did. That difference is what
//! decides whether a question may be asked. Asking again for something already answered on the
//! command line makes the operator answer twice with no way to know which answer the command used;
//! not asking for something they never answered means a money command proceeds on a value nobody
//! chose.

//! Clap keeps the distinction in `ArgMatches::value_source`, and the parse in `main` used to throw
//! the matches away. It does not any more, and this module is where the answer is looked up.

//! Reading `std::env::args()` instead -- searching argv for the flag's spelling -- is what this
//! replaces, and it is wrong in four ways, each of which produces a false "the operator answered"
//! and therefore a question that is never asked:

//! 1. **A value equal to the flag counts as the flag.** Values here are model ids, addresses and
//! free text; `--frame-model=--ticks` makes a scan believe `--ticks` was given.
//! 2. **Short forms are invisible.** A scan for `--ticks` cannot see `-t 2`.
//! 3. **Everything after `--` counts.** That is exactly where arguments meant for someone else go.
//! 4. **Another subcommand's flag counts.** A scan has no idea which command is running.

//! Every one of the four is measured below against this module, on shapes the argv scan gets wrong.

use std::sync::OnceLock;

/// The parse of this process's command line, kept for the questions that depend on it.
static MATCHES: OnceLock<clap::ArgMatches> = OnceLock::new();

/// Record what this process was actually given. Called once, from `main`, right after parsing.
pub(crate) fn remember(matches: clap::ArgMatches) {
    let _ = MATCHES.set(matches);
}

/// Did the operator answer this argument, rather than clap answering for them?

/// `id` is the argument's clap id -- the field name (`ticks`), not its spelling (`--ticks`) -- so a
/// renamed flag cannot leave this silently answering about nothing.

/// `false` when the command line was never recorded, which is every unit test in this binary: a run
/// that cannot tell must ask rather than assume it was told.
pub(crate) fn was_answered(id: &str) -> bool {
    MATCHES
        .get()
        .is_some_and(|matches| answered_in(matches, id))
}

/// The same question against a given parse, which is what the tests can drive.

/// The lookup walks to the deepest subcommand first: an argument belongs to the command that is
/// running: in `dexdo subscription place --max-price-per-tick 1 --ticks 5` the answer
/// belongs to `place`, not to `subscription`.

/// An environment variable counts as an answer alongside the command line. `--flag`, `DEXDO_FLAG=`
/// and a default are three different things, and only the last one is clap speaking for the
/// operator; a run configured through the environment has been told, and asking it again on a
/// terminal would be asking about a value the operator already set.
pub(crate) fn answered_in(matches: &clap::ArgMatches, id: &str) -> bool {
    let leaf = leaf_of(matches);
    // `value_source` panics on an id the parse does not know -- which is precisely the fourth case,
    // a flag belonging to some other subcommand -- so presence is established first.
    leaf.ids().any(|present| present.as_str() == id)
        && matches!(
            leaf.value_source(id),
            Some(clap::parser::ValueSource::CommandLine | clap::parser::ValueSource::EnvVariable)
        )
}

/// The matches of the command that is actually running.
fn leaf_of(matches: &clap::ArgMatches) -> &clap::ArgMatches {
    let mut current = matches;
    while let Some((_, deeper)) = current.subcommand() {
        current = deeper;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};

    fn parse(argv: &[&str]) -> clap::ArgMatches {
        <crate::Cli as clap::CommandFactory>::command()
            .try_get_matches_from(argv)
            .unwrap_or_else(|error| panic!("{argv:?}: {error}"))
    }

    /// The pair the whole rule rests on: a typed argument is an answer, a defaulted one is not.
    #[test]
    fn a_typed_value_is_an_answer_and_a_default_is_not() {
        let typed = parse(&[
            "dexdo",
            "buyer",
            "--model",
            "qwen",
            "--ticks",
            "5",
            "--max-price-per-tick",
            "1",
        ]);
        assert!(answered_in(&typed, "ticks"), "--ticks 5 is an answer");

        let defaulted = parse(&[
            "dexdo",
            "buyer",
            "--model",
            "qwen",
            "--max-price-per-tick",
            "1",
        ]);
        assert!(
            !answered_in(&defaulted, "ticks"),
            "clap's own default is not something the operator said"
        );
    }

    /// Lie 1: a VALUE whose text is the flag. Model ids, addresses and free text all go through
    /// here, and an argv scan cannot tell a flag from something that merely looks like one.
    #[test]
    fn a_value_that_looks_like_the_flag_is_not_the_flag() {
        let argv = [
            "dexdo",
            "buyer",
            "--model=--ticks",
            "--max-price-per-tick",
            "1",
        ];
        let matches = parse(&argv);

        assert!(
            argv.iter().any(|argument| argument.contains("--ticks")),
            "the argv scan this replaces would see `--ticks` here"
        );
        assert!(
            !answered_in(&matches, "ticks"),
            "the operator named a model, not a tick count: asking them would be right"
        );
    }

    /// Lie 2: the short form. Nothing in today's CLI has one, so this is measured against a command
    /// that does -- the helper is shared and the next argument to need it may well be `-n`.
    #[test]
    fn a_short_form_is_the_same_answer_as_the_long_one() {
        let command = Command::new("dexdo").arg(
            Arg::new("ticks")
                .short('t')
                .long("ticks")
                .action(ArgAction::Set)
                .default_value("1"),
        );
        let short = command
            .clone()
            .try_get_matches_from(["dexdo", "-t", "5"])
            .expect("short form parses");

        assert!(
            answered_in(&short, "ticks"),
            "`-t 5` answers the same question as `--ticks 5`; a scan for the long spelling misses it \
             and asks the operator to repeat themselves"
        );
    }

    /// Lie 3: everything after `--`. That is where arguments meant for another program go, and a
    /// scan counts them as ours.
    #[test]
    fn what_follows_a_double_dash_belongs_to_someone_else() {
        let command = Command::new("dexdo")
            .arg(
                Arg::new("ticks")
                    .long("ticks")
                    .action(ArgAction::Set)
                    .default_value("1"),
            )
            .arg(
                Arg::new("passthrough")
                    .num_args(0..)
                    .last(true)
                    .allow_hyphen_values(true),
            );
        let matches = command
            .try_get_matches_from(["dexdo", "--", "--ticks", "5"])
            .expect("trailing arguments parse");

        assert!(
            !answered_in(&matches, "ticks"),
            "`-- --ticks 5` is an argument for whatever runs next, not an answer to our question"
        );
    }

    /// Lie 4: another command's flag. The answer belongs to the command that is running, and asking
    /// about an argument this parse does not have must be `false` rather than a panic.
    #[test]
    fn an_argument_of_another_command_is_not_an_answer_here() {
        let elsewhere = parse(&["dexdo", "doctor"]);
        assert!(
            !answered_in(&elsewhere, "ticks"),
            "`doctor` has no ticks: nothing was answered, and looking must not panic"
        );

        let nested = parse(&[
            "dexdo",
            "subscription",
            "place",
            "--max-price-per-tick",
            "1",
            "--ticks",
            "112",
        ]);
        assert!(
            answered_in(&nested, "ticks"),
            "the running command is the nested one, and its own flag is what was answered"
        );
    }

    /// Nothing recorded -- every unit test in this binary -- must read as "not answered", so the
    /// caller asks instead of proceeding on a value nobody chose.
    #[test]
    fn an_unrecorded_command_line_answers_nothing() {
        assert!(!was_answered("ticks"));
    }
}
