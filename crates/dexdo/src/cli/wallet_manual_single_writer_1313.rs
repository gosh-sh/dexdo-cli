//! the binding writer has one call site, counted so a space cannot walk past it.

//! The rule itself is the compiler's now. `persist::save_active_binding` takes an
//! `EmptyBindingSlot`, whose field is private to a module with no descendants, so a second writer
//! that skips the already-bound refusal has no way to build the argument and does not type-check.
//! Nothing in this file is what stops that one; `cargo build` is.

//! What is left for a test is the second writer that DOES type-check -- one that makes the refusal
//! and then writes anyway, bypassing whatever the single writer is later given to do. The count
//! that was supposed to catch it read `save_active_binding` immediately followed by `(`, so a call
//! written `save_active_binding (`, with one space, contributed zero matches: the count stayed at 2
//! while three places wrote the binding. Counting the name wherever it is APPLIED, whatever sits
//! between it and its arguments, closes that spelling and closes `save_active_binding\n(` and
//! `save_active_binding\t(` with it.

//! What this deliberately does not do is guess. A comment wedged between the name and its arguments
//! (`save_active_binding /* */ (`) is not whitespace and is not normalized away here, because the
//! count is no longer the guarantee -- it is the regression for the spelling that was reported, and
//! widening it into "anything that looks like a call" would trade a defeatable guard for an
//! unpredictable one.

/// This module's source, read at compile time so the check cannot miss a file it was not pointed at.
const SOURCE: &str = include_str!("wallet_manual.rs");

/// Everything before the module's own unit tests.

/// The test halves are excluded on purpose: a test may name the writer as often as it likes, and
/// this file itself would otherwise be counted through the string literals below.
fn production() -> &'static str {
    SOURCE
        .split_once("\n#[cfg(test)]\nmod tests {")
        .expect("unit-test module boundary")
        .0
}

/// How many times `name` appears as a whole identifier applied to an argument list.

/// "Applied" rather than "followed by `(`": Rust puts no meaning on the whitespace between a
/// function's name and its arguments, so neither does this. A longer identifier that merely
/// contains `name` is not `name`, and a mention that is never applied -- a doc link, prose -- is
/// not a call.
fn applications_of(source: &str, name: &str) -> usize {
    let bytes = source.as_bytes();
    let mut count = 0;
    let mut cursor = 0;
    while let Some(offset) = source[cursor..].find(name) {
        let start = cursor + offset;
        let end = start + name.len();
        cursor = end;
        let continues_a_longer_identifier =
            start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        if continues_a_longer_identifier {
            continue;
        }
        if source[end..].trim_start().starts_with('(') {
            count += 1;
        }
    }
    count
}

/// Source with every whitespace character removed, for reading a signature that may be wrapped
/// across lines however a formatter happens to want it.
fn without_whitespace(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// The counting itself, on a fixture, because the defect this file exists for was in the counting
/// and not in the thing counted. Every spelling below is the same call.
#[test]
fn the_count_is_blind_to_what_sits_between_a_name_and_its_arguments() {
    assert_eq!(
        applications_of("write(a); write (a); write\n(a);", "write"),
        3
    );
    assert_eq!(applications_of("write\t(a);\r\n write  (a);", "write"), 2);
    assert_eq!(
        applications_of("rewrite(a); write_twice(a); writer(a);", "write"),
        0,
        "a longer identifier that contains the name is a different function"
    );
    assert_eq!(
        applications_of("/// see [`write`] for the rule\nlet write = 1;", "write"),
        0,
        "a mention that is never applied is not a call"
    );
}

/// One definition and one call, whatever spacing either is written with.

/// A second writer inside `persist` that made the refusal and then wrote would land here, which is
/// the case the compiler cannot refuse: it can insist the refusal ran, not that the write happened
/// once. Both places live in `persist`; if this fails, read it as "the binding now has more than
/// one writer" and not as "the count needs adjusting".
#[test]
fn the_binding_writer_is_defined_once_and_applied_once() {
    assert_eq!(
        applications_of(production(), "save_active_binding"),
        2,
        "the binding writer must be defined once and called once, both inside `persist`"
    );
}

/// The writer's argument is the proof that the refusal ran.

/// This is what makes the writer's visibility stop mattering, and therefore what a refactor could
/// quietly undo: widen the parameter back to a plain `&Path` and the write becomes reachable from
/// anywhere that can name it again.
#[test]
fn the_writer_takes_the_proof_that_the_refusal_ran() {
    let packed = without_whitespace(production());
    let parameters = packed
        .split_once("fnsave_active_binding(")
        .expect("the binding writer is defined in the production half")
        .1
        .split_once(')')
        .expect("the writer's parameter list ends")
        .0;
    assert!(
        parameters.contains("EmptyBindingSlot"),
        "the writer must take the empty-slot proof, or the already-bound refusal becomes a step a \
         second writer can forget; its parameters are `{parameters}`"
    );
}
