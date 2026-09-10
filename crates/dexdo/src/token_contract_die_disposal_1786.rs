//! every path to `selfdestruct` disposes of the deal's `_balance` before taking it.

//! `TokenContract` holds the deal's SHELL as a `uint128 _balance`. A variable does not travel:
//! destroying the contract with a non-zero `_balance` pays nobody and annihilates the figure --
//! "silently, with no failed call and nothing in a log", as the contract itself puts it
//! (`TokenContract.sol:407-427`). There is ONE `selfdestruct`, inside `_die`, and every function
//! that reaches it disposes of `_balance` on its own line first. Today that invariant is held by
//! everyone having remembered. A seventh path added without the line would destroy money and no
//! test, no compiler and no reviewer diff would say so.

//! # WHAT THIS GUARD HOLDS

//! The forgotten line. A new exit that reaches `selfdestruct` without disposing of `_balance` is
//! refused, in six shapes -- a direct `_die` call, a `selfdestruct` that bypasses `_die` entirely,
//! a new wrapper, a disposal deleted from an existing exit, a disposal moved to after the call,
//! and a disposal commented out. Each of those is a test below, and each names the function.

//! # WHAT THIS GUARD DOES NOT HOLD, AND THE NEXT READER MUST NOT ASSUME IT DOES

//! **A disposal that is present but skippable.** This reads TEXT ORDER, not control flow:

//! ```solidity
//! if (_offerPosted) { if (_balance > 0) { _payShell(_sellerNote, _balance); } }
//! _die(_sellerNote);
//! ```

//! passes -- and annihilates the money on every path where `_offerPosted` is false. A disposal
//! conditioned on anything other than `_balance` itself is invisible here. That is a DIFFERENT
//! mistake from the one this guard exists for, it cannot be caught by any textual form, and closing
//! it needs the compiler or a control-flow tool. This guard is not a proof of the invariant; it is
//! a refusal of the one way the invariant has actually been at risk.

//! # TWO LIMITS OF THE READING, BOTH DELIBERATE

//! **A disposal moved into a helper is refused, and that is the contract's own rule, not a defect
//! here.** `TokenContract.sol:407` states it: "EVERY `selfdestruct` IN THIS FILE DISPOSES OF
//! `_balance` ON ITS OWN LINE, in the function that destructs. There was a `_sweepBalance(to)`
//! helper here and it is gone on purpose". Reintroducing the helper reddens this guard, which is
//! the conversation the doctrine is owed rather than a silent pass.

//! **Functions are enumerated by `function ` at the start of a line.** Constructors, modifiers,
//! `receive` and `fallback` are therefore not swept. No such member reaches `_die` today -- that is
//! a fact about today, not about the rule -- so a `selfdestruct` placed inside a modifier would sit
//! outside rule 1 as written. Bodyless declarations (`function f() external;`) are skipped; this
//! file has none, and the skip is there so an interface added later cannot hand the scanner a
//! neighbour's body.

//! # WHY THE BODY IS READ THROUGH `code_of` AND NOT AS TEXT

//! `code_of` returns the body with comments removed. This file mentions `selfdestruct` or `_die` in
//! ten comment lines, so a raw-text guard is fooled at the first attempt -- measured: with the
//! disposal in `_payOwedAndDie` commented out, a raw-text form of these same two rules stays GREEN
//! while this one goes RED. Both agree on the shipped contract.

//! `code_of` strips comments but keeps string literals verbatim -- its own doc says it will "leave
//! string and char literals alone". A literal naming `_die(` would therefore read as an exit, so
//! this guard drops literals on its own side. Solidity has no lifetimes, so unlike the Rust scanner
//! a bare `'` here is always a string.

use crate::source_probe::code_of;

/// The shipped contract, read from the tree rather than pasted, so the guard cannot drift from it.
const TOKEN_CONTRACT: &str = include_str!("../../../contracts/airegistry/TokenContract.sol");

const DIE: &str = "function _die(address payoutAddress) private";

/// The body with string and char literals blanked, so a literal that merely NAMES `_die(` or
/// `selfdestruct(` is not read as a call.
fn without_literals(code: &str) -> String {
    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'"' || bytes[index] == b'\'' {
            let quote = bytes[index];
            index += 1;
            while index < bytes.len() && bytes[index] != quote {
                index += if bytes[index] == b'\\' { 2 } else { 1 };
            }
            index += 1;
            out.push_str("\"\"");
            continue;
        }
        let character = code[index..]
            .chars()
            .next()
            .expect("the scanner only stops on a character boundary");
        out.push(character);
        index += character.len_utf8();
    }
    out
}

/// The offset of the earliest statement that DISPOSES of `_balance`, or `None`.

/// Disposal means the figure is handed somewhere -- passed as an argument (`, _balance)`) or
/// assigned (`_balance =`, `_balance -=`). A COMPARISON is not a disposal: `if (_balance > 0)` on
/// its own leaves the money exactly where it was, and a guard that accepted it would be satisfied
/// by an exit that only ever LOOKS at the figure before destroying it.
fn disposal_offset(code: &str) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    for (at, _) in code.match_indices("_balance") {
        let after = code[at + "_balance".len()..].trim_start();
        let before = code[..at].trim_end();
        let disposes = (before.ends_with(',') && after.starts_with(')'))
            || (after.starts_with('=') && !after.starts_with("=="))
            || after.starts_with("-=");
        if disposes {
            earliest = Some(earliest.map_or(at, |seen: usize| seen.min(at)));
        }
    }
    earliest
}

/// Every function declaration that starts a line, as `code_of` wants to be given it.
fn functions(sol: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in sol.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("function ") {
            continue;
        }
        if trimmed.trim_end().ends_with(';') {
            continue; // a declaration with no body: its `{` belongs to somebody else
        }
        let signature = trimmed.split('{').next().unwrap_or_default().trim_end();
        if !signature.is_empty() {
            out.push(signature.to_string());
        }
    }
    out
}

/// The comment-free, literal-free code of one function.
fn code(sol: &str, signature: &str) -> String {
    without_literals(&code_of(sol, signature))
}

/// Every way this source breaks the invariant, named. Empty is the passing state.
fn findings(sol: &str) -> Vec<String> {
    let mut findings = Vec::new();
    let all = functions(sol);

    // RULE 1: the contract ceases to exist in exactly one place.
    let in_die = code(sol, DIE).matches("selfdestruct(").count();
    let anywhere: usize = all
        .iter()
        .map(|signature| code(sol, signature).matches("selfdestruct(").count())
        .sum();
    if in_die != 1 || anywhere != 1 {
        findings.push(format!(
            "rule 1: `selfdestruct` must appear exactly once and inside `_die` \
             (found {in_die} in `_die`, {anywhere} across all functions)"
        ));
    }

    // RULE 2: whoever reaches `_die` has already disposed of the figure.

    // A new WRAPPER needs no call graph to be covered: it would contain `_die(` itself, so it is
    // checked on its own. Only a wrapper's CALLERS are exempt, and they are exempt correctly --
    // `_payOwedAndDie` disposes before it dies, so `stop`, `_closeClean` and
    // `_settleTrustedAndClose` inherit a disposal that has already happened.
    for signature in &all {
        let body = code(sol, signature);
        let Some(dies_at) = body.find("_die(") else {
            continue;
        };
        match disposal_offset(&body) {
            Some(at) if at < dies_at => {}
            Some(_) => findings.push(format!(
                "rule 2: `{signature}` disposes of `_balance` AFTER it calls `_die(`"
            )),
            None => findings.push(format!(
                "rule 2: `{signature}` reaches `_die(` and never disposes of `_balance`"
            )),
        }
    }
    findings
}

/// The shipped contract, which is the control for every sabotage below: a matrix in which the
/// correct input is not also green proves nothing about the sabotage that is red.
#[test]
fn the_shipped_contract_disposes_on_every_path_to_selfdestruct() {
    assert_eq!(findings(TOKEN_CONTRACT), Vec::<String>::new());
}

/// The exits are a call graph, not a list, and the guard's reach depends on its shape. Frozen here
/// so a later refactor that hides `_die` behind another wrapper has to come past this line.
#[test]
fn the_exits_are_the_four_direct_callers_and_the_one_wrapper() {
    let direct: Vec<String> = functions(TOKEN_CONTRACT)
        .into_iter()
        .filter(|signature| code(TOKEN_CONTRACT, signature).contains("_die("))
        .collect();
    assert_eq!(direct.len(), 4, "{direct:#?}");
    assert!(
        direct.iter().any(|s| s.contains("_payOwedAndDie")),
        "{direct:#?}"
    );
    assert_eq!(TOKEN_CONTRACT.matches("_payOwedAndDie();").count(), 3);
}

// ---------------------------------------------------------------------------------------------
// THE SABOTAGE MATRIX. Six shapes of's defect, each of which MUST be refused, and three
// legitimate wind-downs which MUST NOT be. A guard that only ever goes red is no better than one
// that only ever goes green, so both halves live here together.

// The mutations are applied to a COPY of the source in memory. Nothing is written to any contract.
// ---------------------------------------------------------------------------------------------

const ANCHOR: &str = "    function _die(address payoutAddress) private {";
const DISPOSAL: &str = "if (_balance > 0) { _payShell(_sellerNote, _balance); }";
const WIND_DOWN_TAIL: &str = "        if (_balance > 0) { _payShell(_sellerNote, _balance); }\n        _die(_sellerNote);\n    }\n\n    function _die";

/// A new function spliced in ahead of `_die`, as a new exit would arrive in review.
fn with_extra(body: &str) -> String {
    let mutated = TOKEN_CONTRACT.replacen(ANCHOR, &format!("{body}\n{ANCHOR}"), 1);
    assert_ne!(mutated, TOKEN_CONTRACT, "the mutation did not apply");
    mutated
}

fn refused(sol: &str, naming: &str) {
    let found = findings(sol);
    assert!(
        found.iter().any(|f| f.contains(naming)),
        "expected a finding naming `{naming}`, got {found:#?}"
    );
}

fn accepted(sol: &str) {
    assert_eq!(findings(sol), Vec::<String>::new());
}

#[test]
fn a_sixth_exit_that_never_disposes_is_refused() {
    refused(
        &with_extra(
            "    function abandonDeal() public onlyOwnerPubkey(_sellerPubkey) accept {\n\
             \x20       _returnBond();\n\
             \x20       _die(_sellerNote);\n\
             \x20   }\n",
        ),
        "abandonDeal",
    );
}

#[test]
fn a_sixth_exit_that_bypasses_die_and_selfdestructs_itself_is_refused() {
    refused(
        &with_extra(
            "    function abandonDeal() public onlyOwnerPubkey(_sellerPubkey) accept {\n\
             \x20       selfdestruct(_sellerNote);\n\
             \x20   }\n",
        ),
        "rule 1",
    );
}

#[test]
fn a_new_wrapper_that_dies_without_disposing_is_refused() {
    refused(
        &with_extra("    function _quietDie() private {\n        _die(_sellerNote);\n    }\n"),
        "_quietDie",
    );
}

#[test]
fn deleting_the_disposal_from_an_existing_exit_is_refused() {
    let at = TOKEN_CONTRACT
        .find("        _returnBond();")
        .expect("close() still returns the bond");
    let mutated = format!(
        "{}{}",
        &TOKEN_CONTRACT[..at],
        TOKEN_CONTRACT[at..].replacen(DISPOSAL, "", 1)
    );
    assert_ne!(mutated, TOKEN_CONTRACT, "the mutation did not apply");
    refused(&mutated, "function close()");
}

#[test]
fn moving_the_disposal_after_the_die_call_is_refused() {
    let mutated = TOKEN_CONTRACT.replacen(
        WIND_DOWN_TAIL,
        "        _die(_sellerNote);\n        if (_balance > 0) { _payShell(_sellerNote, _balance); }\n    }\n\n    function _die",
        1,
    );
    assert_ne!(mutated, TOKEN_CONTRACT, "the mutation did not apply");
    refused(&mutated, "AFTER");
}

/// The case `code_of` is here for. A raw-text form of these same two rules stays GREEN on this
/// input; commenting a line out must not be a way to remove it.
#[test]
fn commenting_the_disposal_out_is_refused() {
    let mutated = TOKEN_CONTRACT.replacen(
        WIND_DOWN_TAIL,
        " // if (_balance > 0) { _payShell(_sellerNote, _balance); }\n _die(_sellerNote);\n }\n\n function _die",
        1,
    );
    assert_ne!(mutated, TOKEN_CONTRACT, "the mutation did not apply");
    refused(&mutated, "_payOwedAndDie");
}

/// A false red on a correct contract is worse than no guard: it teaches the next reader to delete
/// the guard rather than to read it. These three are correct wind-downs and must pass.
#[test]
fn an_exit_that_pays_the_residual_to_the_buyer_is_accepted() {
    accepted(&with_extra(
        "    function refundAndDie() public onlyOwnerPubkey(_sellerPubkey) accept {\n\
         \x20       if (_balance > 0) { _payShell(_buyer, _balance); }\n\
         \x20       _die(_buyer);\n\
         \x20   }\n",
    ));
}

#[test]
fn an_exit_that_zeroes_the_figure_explicitly_is_accepted() {
    accepted(&with_extra(
        "    function burnAndDie() public onlyOwnerPubkey(_sellerPubkey) accept {\n\
         \x20       uint128 residue = _balance;\n\
         \x20       _balance = 0;\n\
         \x20       if (residue > 0) { gosh.burnecc(uint64(residue), SHELL_ECC_ID); }\n\
         \x20       _die(_sellerNote);\n\
         \x20   }\n",
    ));
}

/// `cleanupUnopened` pays two parties and says at length why; a future exit shaped like it must not
/// be refused for splitting the residual.
#[test]
fn an_exit_that_splits_the_residual_between_two_parties_is_accepted() {
    accepted(&with_extra(
        "    function splitAndDie() public onlyOwnerPubkey(_sellerPubkey) accept {\n\
         \x20       uint128 half = _balance / 2;\n\
         \x20       if (half > 0) { _payShell(_buyer, half); }\n\
         \x20       if (_balance > 0) { _payShell(_sellerNote, _balance); }\n\
         \x20       _die(_sellerNote);\n\
         \x20   }\n",
    ));
}

/// A literal is not a call. `code_of` keeps literals, so without `without_literals` this reads as a
/// seventh exit and the guard false-reds on correct code.
#[test]
fn a_string_literal_naming_die_is_not_an_exit() {
    accepted(&with_extra(
        "    function explain() public pure returns (string) {\n\
         \x20       return \"call _die( to end the deal\";\n\
         \x20   }\n",
    ));
}

/// The ceiling, asserted rather than described, so nobody has to trust the header. A disposal
/// behind an unrelated condition PASSES: the line is there and the money still dies whenever the
/// condition is false. When this test starts failing, the guard has become stronger than its own
/// documentation and the header must be rewritten.
#[test]
fn a_disposal_behind_an_unrelated_condition_is_not_caught_and_this_is_the_known_ceiling() {
    accepted(&with_extra(
        "    function abandonDeal() public onlyOwnerPubkey(_sellerPubkey) accept {\n\
         \x20       if (_offerPosted) { if (_balance > 0) { _payShell(_sellerNote, _balance); } }\n\
         \x20       _die(_sellerNote);\n\
         \x20   }\n",
    ));
}
