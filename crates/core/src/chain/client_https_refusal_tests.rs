//! the refusal an operator meets when the HTTPS client cannot be assembled.

//! These assert a PROPERTY, not a sentence. The wording belongs to whoever writes it next; what must
//! survive any rewording is that the operator is handed causes they can go and check, that more than
//! one is offered, and that the evidence which reached us is not thrown away.

//! Pinning the phrase instead would be the mistake this file exists to avoid: three separate defects
//! in one day came from a test that asserted an exact string and so could not notice that the string
//! had stopped being true.

use super::{https_client_refusal, is_https_client_build_failure};

/// The underlying sentence, as it actually arrives after crossing `reqwest` -> `tvm_client` ->
/// `gosh-ackinacki`. Used as an opaque input: nothing here asserts its contents.
const UNDERLYING: &str =
    "create local tvm client context: Can not create http client: builder error";

/// Handles an operator can act on, recognised by SHAPE so that any rewording still counts:
/// an environment variable they can echo, an absolute path they can list, or a backticked token
/// they can type.

/// A claim about the world -- "minimal images often ship none" -- is true and is NOT a handle: the
/// operator cannot check it on their own machine. Only what they can go and look at counts here, and
/// that is the whole point of the property.
fn checkable_handles(text: &str) -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    for token in text.split_whitespace() {
        let path = token.trim_end_matches([',', '.', ';']);
        if path.starts_with('/') && path.len() >= 5 && path[1..].contains('/') {
            found.insert(path.to_string());
        }
    }
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        let shaped = token.len() >= 5
            && token.contains('_')
            && token
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && token.chars().any(|c| c.is_ascii_uppercase());
        if shaped {
            found.insert(token.to_string());
        }
    }
    let mut rest = text.split('`');
    let _ = rest.next();
    while let Some(quoted) = rest.next() {
        if !quoted.trim().is_empty() {
            found.insert(quoted.trim().to_string());
        }
        if rest.next().is_none() {
            break;
        }
    }
    found
}

#[test]
fn the_refusal_offers_more_than_one_cause_the_operator_can_check() {
    let refusal = https_client_refusal(UNDERLYING).to_string();
    let handles = checkable_handles(&refusal);
    // More than one, and that bound is the finding, not a preference: four distinct causes produce
    // the identical underlying sentence and three of them had a complete certificate store, so a
    // refusal naming a single cause would be wrong more often than right.
    assert!(
        handles.len() >= 2,
        "the refusal must offer at least two checkable causes; found {handles:?} in: {refusal}"
    );
}

#[test]
fn the_refusal_keeps_the_evidence_that_reached_it() {
    let refusal = https_client_refusal(UNDERLYING).to_string();
    assert!(
        refusal.contains(UNDERLYING),
        "the underlying error is the only evidence that survived two dependencies and must not be \
         replaced: {refusal}"
    );
}

#[test]
fn the_property_detector_can_fail() {
    // A check that has only ever been seen to succeed is untested in the direction that matters.
    // A bare restatement of the underlying error offers nothing to check, and must not pass.
    let handles = checkable_handles(UNDERLYING);
    assert!(
        handles.len() < 2,
        "the detector must reject a refusal that adds nothing checkable, but it accepted \
         {handles:?}"
    );
}

#[test]
fn guidance_is_attached_to_a_client_build_failure_and_to_nothing_else() {
    // The negative control, and it is shaped like the class from outside: a real failure of the very
    // same connector, at the very same call, that has nothing to do with TLS. It must pass through
    // untouched -- attaching certificate advice to it would be the same defect this change removes,
    // pointed the other way.
    // The fixture text arrived naming the endpoint-versus-declared-network contradiction, a refusal
    // removed along with the `--endpoint` flag that made it possible. Any real failure of this
    // connector serves as the control; this one is a manifest that says nothing about where to dial.
    let unrelated =
        anyhow::anyhow!("the manifest carries no `endpoint`, so nothing says where to dial");
    assert!(
        !is_https_client_build_failure(&unrelated),
        "an unrelated connector failure must not be given HTTPS-client guidance"
    );

    let real = anyhow::anyhow!("{UNDERLYING}");
    assert!(
        is_https_client_build_failure(&real),
        "the failure this change exists for must be recognised: {real}"
    );
}
