//! Issue: every production call of `ChainClient::connect(`, frozen by name, with the two
//! facts that decide what each one costs.

//! **What membership actually is.** A SPELLING: the literal `ChainClient::connect(` outside
//! `#[cfg(test)]`, anywhere under `crates/dexdo/src`. That is what the walk below can see, and it
//! is all it can see. It is emphatically NOT the property "reaches a chain with nothing to hold
//! the endpoint against", and the rows prove the difference three ways: `run_note_deploy` proves
//! its manifest's `network` against the endpoint before it connects, `note_pick::balance_reader`
//! takes its endpoint from a manifest but never holds it against that manifest's label, and
//! `run_note_recover` has no second opinion to hold anything against at all. Three situations, one
//! spelling. Reading the list as the set of unchecked paths overstates it, in the direction that
//! sends a reader to the wrong site.

//! **Why the spelling is still worth freezing.** `verify_declared_network_matches_endpoint`
//! runs inside `connect_client_from_manifest_with`, and the comment there once claimed
//! every path went through it. These calls do not: each takes an endpoint and nothing else, so on
//! each one a declared network has to be supplied and compared deliberately, or it is not compared
//! at all.

//! **What this file is NOT evidence of, which its own wording used to imply.** The issue cites two
//! paid incidents: a withdrawal route proven on one chain while being read as another, and
//! eight notes holding 743.412 SHELL searched for on the wrong chain. Measured against the rows:
//! no site here is a withdrawal route, and the notes incident IS `note_pick::balance_reader` --
//! repaired by `69fa58a4` on 2026-08-17, which is why that row resolves through a manifest today
//! and why its own comment names the symptom, "reading that field alone is what left every balance
//! unread". So the two incidents are a bug already fixed and a bug none of these paths can cause.
//! They are why the question was asked. They are not a measurement of what this list risks now,
//! and quoting them as one is how a reader ends up budgeting money-risk against a headline member
//! that is already safe. The measurement is the two facts per row, below.

//! **What decides severity, and why it lives in the table.** Two facts, per site: `DeclaredNetwork`
//! -- whether a declared network exists on the path at all, and if so whether anything compares it
//! to the endpoint before the client is used -- and whether the path `spends`. A new row cannot be
//! added without answering both, and neither answer is taken on trust: the tests below check them
//! against the files they are claimed about. The combination that costs money is
//! `spends` with an unproved network, and the tests refuse it.

//! Routing the remaining reads through a manifest, or giving each an explicitly declared network,
//! is the repair and it is not this file. This file stops the set from growing while that is
//! decided: a new call fails here rather than arriving unnoticed.

/// Whether a site can tell which chain it is dialling, and whether anyone checked.

/// Three states rather than a yes/no, because the two `false` halves have different repairs and
/// collapsing them is how this set got read as uniform. A path with nothing to compare needs a
/// declared network invented for it; a path that has one and ignores it needs one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeclaredNetwork {
    /// Nothing on this path names a chain. `note recover` takes its endpoint from the recovery
    /// file and has no second opinion to hold it against.
    None,
    /// A declared network is on hand -- a `--network` flag, a binding's own field, a manifest --
    /// and nothing compares it to the endpoint being dialled. An explicit `--endpoint` naming the
    /// other chain is accepted in silence.
    Unproved,
    /// The endpoint and the network label come out of ONE manifest, so there is no second source
    /// for them to contradict.

    /// This used to be `Proved` and meant a call to `verify_declared_network_matches_endpoint` --
    /// a check comparing the declared label against the host actually dialled. That check is gone
    /// with the flag it existed for: `--endpoint` was the only way the two could disagree,
    /// and with it removed there is nothing to compare, because there is only one source. The
    /// protection is stronger than the check was, and structural rather than asserted.
    OneSource,
}

/// One production call of [`DIRECT_CONNECT`], named by the function it sits in.
struct Site {
    /// Path under `crates/dexdo/src`, spelled as the walk below renders it.
    file: &'static str,
    /// The enclosing function, so a row that stopped describing anything says so out loud.
    function: &'static str,
    /// Which chain this call can prove it is dialling.
    declared: DeclaredNetwork,
    /// Whether money moves on this path -- the operator's, ours, or an operator's transfer this
    /// command asks for and then waits on. Reads and local file work are not spending.
    spends: bool,
}

/// Every site, and the two facts about each.

/// By name rather than by a single total: a bare count tells the next reader that something moved
/// and not what, and the whole point of a ratchet is that the diff explains itself.
const PRODUCTION_SITES: &[Site] = &[
    Site {
        // `note recover` reads a getter off a note whose endpoint came out of the recovery file.
        // The one site with no declared network anywhere on it.
        file: "cli/note_cmd.rs",
        function: "run_note_recover",
        declared: DeclaredNetwork::None,
        spends: false,
    },
    Site {
        // The command that mints a note and spends the funding wallet -- and the one site that was
        // already proving its manifest label before it connected, at the last free point ahead of
        // the wallet lock and the first spend.
        file: "cli/note_cmd.rs",
        function: "run_note_deploy",
        declared: DeclaredNetwork::OneSource,
        spends: true,
    },
    Site {
        // Paints SHELL balances beside the notes an operator is choosing between. It loads the
        // manifest and resolves through it, so an endpoint arrives with a label attached -- but an
        // explicit `--endpoint` outranks both and is never held against that label.
        file: "cli/note_pick.rs",
        function: "balance_reader",
        declared: DeclaredNetwork::Unproved,
        spends: false,
    },
    Site {
        // Proves an archived Hot holds zero on both balances before its local record is deleted.
        // The label is the archived binding's own `network`.
        file: "cli/wallet.rs",
        function: "run_remove_archived",
        declared: DeclaredNetwork::Unproved,
        spends: false,
    },
    Site {
        // Paints what the bound Hot holds beside the recorded binding. It dials only when the
        // manifest's own `network` equals the binding's, so the pair is compared -- but by string
        // equality against the FILE, not by `DECLARED_NETWORK_CHECK` against the endpoint, which is
        // what `Proved` means. A disagreement here reports "not read" and never a balance, so the
        // gap this leaves is a missing figure rather than a figure off the wrong chain.
        file: "cli/wallet.rs",
        function: "hot_balance_for",
        declared: DeclaredNetwork::Unproved,
        spends: false,
    },
    Site {
        // Three read-only account queries proving a pasted Gosh.ai Hot, under `--network`.
        file: "cli/wallet_goshai.rs",
        function: "run_wallet_onboard_goshai",
        declared: DeclaredNetwork::Unproved,
        spends: false,
    },
    Site {
        // `wallet onboard ackinacki-wallet`: the first read decides the branch, and on the branch
        // where the wallet is already there no manifest is ever loaded -- a binding recording
        // `--network` is written from facts read at `--endpoint`. It had `--network` on hand and
        // compared nothing; it now proves the pair before the connect (,
        // `wallet_manual_network_1613_tests.rs`).
        file: "cli/wallet_manual.rs",
        function: "run",
        declared: DeclaredNetwork::OneSource,
        spends: true,
    },
    Site {
        // Validates the Vault/Hot pair the wallet app returned, under `--network`.
        file: "cli/wallet_onboarding.rs",
        function: "run",
        declared: DeclaredNetwork::Unproved,
        spends: false,
    },
];

/// The call this file counts. It takes an endpoint and nothing else; what put that endpoint there
/// is the per-site fact above, and not something this spelling can see.
const DIRECT_CONNECT: &str = "ChainClient::connect(";

/// The one predicate that decides what a declared network and a dialled host mean together
/// . A site claiming [`DeclaredNetwork::Proved`] is claiming a call to this.
const DECLARED_NETWORK_CHECK: &str = "verify_declared_network_matches_endpoint(";

/// Remove every `#[cfg(test)]` item, by matching braces rather than by looking for a marker.

/// A substring split on `"#[cfg(test)]\nmod tests"` would be wrong twice over: several of these
/// files carry more than one test module, and some carry production code AFTER them. Counting
/// braces is what makes "production" mean production.
fn production_half(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        // From the attribute, find the item's opening brace, then its match.
        let Some(open) = after.find('{') else {
            // A `#[cfg(test)]` with no block after it (a `use`, a `mod foo;`): drop the line only.
            let end = after.find('\n').map_or(after.len(), |n| n + 1);
            rest = &after[end..];
            continue;
        };
        let mut depth = 0_i32;
        let mut close = None;
        for (offset, byte) in after[open..].bytes().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + offset + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match close {
            Some(end) => rest = &after[end..],
            // Unbalanced: keep the remainder rather than silently swallowing the file.
            None => {
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn source_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// A file that exists only under `#[cfg(test)]` because its PARENT declares it that way -- the
/// repository spells those `tests.rs` and `*_tests.rs` throughout. Brace-matching cannot see that
/// from inside the file, so the naming convention is what tells them apart, and it is stated here
/// rather than left to be inferred from a surprising number.
fn is_test_only_file(path: &std::path::Path) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    name == "tests.rs" || name.ends_with("_tests.rs") || name == RATCHET_FILE
}

/// This file names the call it counts, so counting itself would make the ratchet a site.
const RATCHET_FILE: &str = "network_check_reach_1613.rs";

fn rust_files(dir: &std::path::Path, into: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            rust_files(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") && !is_test_only_file(&path) {
            into.push(path);
        }
    }
}

/// Walked rather than listed: a new bypass in a NEW file is exactly the case a fixed list of files
/// would miss, and it is the case that needs catching most.
#[test]
fn the_set_of_paths_that_reach_a_chain_without_a_declared_network_is_frozen() {
    let root = source_root();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    files.sort();
    assert!(files.len() > 20, "the source walk found almost nothing");

    let mut found: Vec<(String, usize)> = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("read source file");
        let count = production_half(&source).matches(DIRECT_CONNECT).count();
        if count > 0 {
            let relative = path
                .strip_prefix(&root)
                .expect("source file is under the source root")
                .to_string_lossy()
                .replace('\\', "/");
            found.push((relative, count));
        }
    }
    found.sort();

    // The per-file counts the walk is compared against are DERIVED from the rows rather than
    // typed beside them: one row per call means the table cannot disagree with its own total, and
    // a row added without a call -- or a call added without a row -- still lands on the assertion
    // below.
    let mut expected: Vec<(String, usize)> = Vec::new();
    for site in PRODUCTION_SITES {
        match expected.iter_mut().find(|(file, _)| file == site.file) {
            Some((_, count)) => *count += 1,
            None => expected.push((site.file.to_string(), 1)),
        }
    }
    expected.sort();

    assert_eq!(
        found, expected,
        "the set of production calls of `ChainClient::connect(` changed.\n\
         Adding one is a decision, not an accident: this call takes an endpoint and nothing else, \
         so whatever decides which chain it dials has to be supplied and compared deliberately. \
         Route it through a manifest, or give it an explicitly declared network and call \
         `verify_declared_network_matches_endpoint` itself. If it genuinely belongs here, add a \
         row to PRODUCTION_SITES -- which means answering both facts, `DeclaredNetwork` and \
         `spends` -- and say in the pull request which of the two it is ()."
    );

    // --- and what each row CLAIMS about its site, measured against that site. --------------------

    // Two facts per row are worth nothing if they are only ever read. `Proved` is the one that
    // decides whether a crossed endpoint costs anything, so it is checked against the file it is
    // claimed about, in both directions: a row promoted to `Proved` without the call fails, and a
    // check quietly removed from a file that still claims one fails too. The second direction is
    // the one a passing test usually never exercises.
    for site in PRODUCTION_SITES {
        let source = std::fs::read_to_string(root.join(site.file)).expect("read a listed site");
        let production = production_half(&source);

        assert!(
            production.contains(&format!("fn {}(", site.function)),
            "`{}` lists `{}`, which is no longer a function in its production half. A row that \
             stopped describing anything is worse than no row: it is a name the next reader will \
             go looking for ().",
            site.file,
            site.function,
        );

        assert!(
            !production.contains(DECLARED_NETWORK_CHECK),
            "`{}` calls `{DECLARED_NETWORK_CHECK}`, which no longer exists (). It compared a \
             declared label against the host being dialled, and it existed because `--endpoint` \
             could name a different one. With that flag gone, the endpoint and the label come out \
             of one file -- so a site reintroducing the check is a second source of truth arriving \
             with it.",
            site.file,
        );
    }

    // --- and the combination that costs money. --------------------------------------------------

    // A site that spends while it cannot say which chain it is on is the shape was opened
    // about, and it is the shape that arrives by accident: an endpoint flag added to a read path
    // that later grows a spend. Naming it here means the next one is a decision.
    for site in PRODUCTION_SITES {
        assert!(
            !(site.spends && site.declared != DeclaredNetwork::OneSource),
            "`{}::{}` spends and its endpoint source is `{:?}`. A spending path must take its \
             endpoint and its network label from ONE manifest, the way `run_note_deploy` does, so \
             the two cannot disagree -- and if this path genuinely cannot, that is a call for the \
             lead and not an edit to this row ().",
            site.file,
            site.function,
            site.declared,
        );
    }
}

/// The removed check stays removed, across the whole workspace -- not only in the listed files.

/// What stood here read the comment above the call site and asserted it did not claim to cover
/// every path, because it did not: the wording at `client.rs` said every path went through the
/// verified seam, so the tree was read through the comment instead of the other way round.

/// The call is gone along with `--endpoint`, the one thing that could put a second endpoint
/// beside the manifest's. So the statement worth freezing is the stronger one: nothing calls it,
/// because there is no longer anything for it to compare.
#[test]
fn the_declared_network_check_is_gone_from_the_whole_tree() {
    let mut files = Vec::new();
    rust_files(&source_root(), &mut files);
    let callers = files
        .iter()
        .filter(|file| {
            std::fs::read_to_string(file)
                .expect("read a workspace source")
                .contains(DECLARED_NETWORK_CHECK)
        })
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>();

    assert!(
        callers.is_empty(),
        "`{DECLARED_NETWORK_CHECK}` is called again, in: {}. The endpoint and the network label \
         come out of one manifest now, so a check comparing them compares a value against itself \
         -- and the flag that made them two values is gone.",
        callers.join(", ")
    );
}
