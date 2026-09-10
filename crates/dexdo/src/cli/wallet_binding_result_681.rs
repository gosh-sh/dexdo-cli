//! what a committed wallet binding tells a PERSON, and where everything else goes.

//! This file used to assert the opposite of the directive, and it is worth saying how.

//! The regression it was written for was real: `feat(681)` (3d86f302, PR1440) turned both
//! `println!`s at the end of `run_selected` into `tracing::info!`, and `wallet rebind manual` was
//! left naming neither the binding it created nor the Hot it now spends from. A log level cannot be
//! how something is shown to an operator -- the filter is global and lets every other crate's lines
//! through with ours.

//! The fix pinned the wrong thing. It searched the source for the literal
//! `"active wallet binding: "` and demanded it sit inside a `println!` -- a line that BEGINS with an
//! absolute path. 681 forbids exactly that: "paths to files the client manages itself do not
//! reach the result: it finds them without the operator". So the test held the forbidden shape in
//! place: while the result started with a path it was green, and correcting it turned it red.

//! Measured on mainnet. Binding one wallet printed three absolute paths and no heading, and the one
//! fact the operator came for -- the Hot address -- was in the middle of the second line's
//! parentheses.

//! Why the wrong shape survived: the paths were printed BECAUSE tests read stdout. There was no
//! machine contract to read instead, so the operator's result grew fields that existed for the test
//! harness. settles that both ways -- `--json` is where a runtime reads, and
//! a test that needs a path either asks for `--json` or runs with `RUST_LOG` and reads the log.

//! So the rules asserted here are the directive's, not one command's wording:

//! 1. the two facts unrecoverable without the client -- the Hot address and the binding id -- are
//! in the RESULT, printed, never only logged;
//! 2. the result carries no filesystem paths;
//! 3. a caller that asked for `--json` gets the object and nothing else.

/// The production half of `wallet.rs`, with its unit-test module cut off.
fn production_source() -> &'static str {
    let source = include_str!("wallet.rs");
    source
        .split_once("#[cfg(test)]")
        .map(|(before, _)| before)
        .unwrap_or(source)
}

/// The body of `run_selected`, which is where an onboard or a rebind reports what it committed.

/// `body_of`, not a slice ending at the next `async fn`. That end marker is a NEIGHBOUR: make
/// `run_selected` the last async function in the file and `unwrap_or(body.len())` silently makes
/// "the body" the whole remainder, where the guarded statements are found whether or not this


/// COMMENTS ARE KEPT here on purpose, and that is the other half of the same directive. This body
/// is used two ways: `human_result_block` navigates by a landmark that IS a comment -- the
/// directive reference marking where the human rendering starts -- while `printed_lines` asserts
/// about CALLS and must not be satisfiable by one. So the first takes the body, the second takes
/// `code_of` of the same function.
fn run_selected_body() -> &'static str {
    crate::cli::source_probe::body_of(production_source(), "async fn run_selected")
}

/// The statements that reach stdout, with comments dropped.

/// Comments are dropped because this file's own history proves what happens otherwise: a rule about
/// code that a comment can satisfy is a rule about nothing.
fn printed_lines() -> Vec<String> {
    crate::cli::source_probe::code_of(production_source(), "async fn run_selected")
        .lines()
        .map(str::trim_start)
        .filter(|line| line.contains("println!(") || line.contains("print!("))
        .map(str::to_string)
        .collect()
}

/// The Hot address and the binding id reach the operator, not just the log.

/// These two are the result under 681's own test for the layer: "if the operator cannot find it
/// again without the client, it is in the result". The binding id names the secrets directory that
/// signs for this wallet; the Hot is the account the client is about to spend from.
#[test]
fn the_two_facts_only_the_client_knows_are_printed_not_logged_681() {
    let printed = printed_lines().join("\n");
    assert!(
        printed.contains("result"),
        "run_selected prints no assembled result at all; the operator is told nothing about the \
         wallet that was just bound"
    );

    // Only the human block. Looking at the whole function is what let the first version of this
    // test pass a sabotage: `hot_address` also appears in the `--json` object and in the
    // `tracing::info!`, so "the address is somewhere in this function" is true even when the
    // operator's result no longer shows it.
    let human = human_result_block();
    for (fragment, what) in [
        ("hot_address", "the Hot address"),
        ("draft.id()", "the binding id"),
    ] {
        assert!(
            human.contains(fragment),
            "{what} is not in the operator's result. It may still be in the log and in --json, \
             which is exactly the regression this file exists for: 681 rules out a log level as \
             the way something is shown, because the filter is global and carries every other \
             crate's lines with ours"
        );
    }
}

/// The block that renders the result for a person: from its directive marker to where the log
/// begins. The `--json` branch returns before it, so it is not in here.
fn human_result_block() -> String {
    run_selected_body()
        .split("681, the shape it spells out")
        .nth(1)
        .expect("the human result block is marked by its directive reference")
        .split("tracing::info!")
        .next()
        .expect("the human block ends where the log begins")
        .to_string()
}

/// The human result names the secrets path, and no other path.

/// 681 pulls in two directions here, and the first version of this test read only one of them.
/// "paths to files the client manages itself do not reach the result: it finds them without
/// the operator" -- measured cost of ignoring that: three absolute paths above the one address the
/// operator wanted, on a mainnet binding. The artifact list is the other direction, and it names
/// this exact command: a wallet binding is stated as "the Vault and Hot addresses, the network,
/// and where the material that signs for this binding lives".

/// What decides between them is the rule the list is derived from: if the operator cannot find it
/// again without the client, it is in the result. That splits this command's two paths, and it
/// splits them the other way round from "no paths at all":

/// * the binding RECORD (`wallet/active/<network>.json`) -- the client wrote it and the client
/// reads it, so keeps it out;
/// * the SECRETS directory -- the operator must back it up and must not lose it, and an id alone
/// does not locate it without knowing which data directory this instance used.

/// Asserting "no paths" flatly is what dropped the secrets path, and the regression got as far as
/// a live run: `wallet show` in the same file prints it, and onboarding stopped printing it.
#[test]
fn the_human_result_names_the_secrets_path_and_no_other_681() {
    let human = human_result_block();

    assert!(
        human.contains("draft.dir()"),
        "the operator's result does not name where the material that signs for this binding \
         lives. 681's artifact list requires it for a wallet binding, under the rule that if the \
         operator cannot find it again without the client it is in the result -- and an id is a \
         name, not a place"
    );

    assert!(
        !human.contains("binding_path("),
        "the human result names the binding RECORD file. 681: the client finds its own files \
         without the operator -- put that path in the --json object or in the log, both of which \
         have a reader who wants it"
    );
}

/// `--json` is one object and nothing else.

/// A runtime parsing stdout must not have to skip a heading and four styled fields first, and a
/// person must not get a JSON blob. The two contracts never share a stream.
#[test]
fn the_machine_contract_replaces_the_human_result_681() {
    let body = run_selected_body();
    let branch = body
        .find("if json {")
        .expect("run_selected honours --json at all");
    let rest = &body[branch..];
    let ends = rest
        .find("return Ok(());")
        .expect("the --json branch returns rather than falling through to the human result");
    let block = &rest[..ends];

    assert!(
        block.contains("println!("),
        "--json prints nothing; a caller that asked for the machine contract gets silence"
    );
    assert!(
        !block.contains("style::"),
        "--json is styled; a JSON document with colour escapes in it is not parseable, and 203 \
         exists because runtimes cannot supervise dexdo by reading human output"
    );
    for field in [
        "\"hot\"",
        "\"binding_id\"",
        "\"binding_file\"",
        "\"secrets_dir\"",
    ] {
        assert!(
            block.contains(field),
            "the --json object omits {field}; the paths in particular are the reason it exists -- \
             an automated caller does need them, and this is where they belong instead of the \
             operator's result"
        );
    }
}
