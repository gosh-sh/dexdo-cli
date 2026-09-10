//! Gosh.ai onboarding happens where the deployment says it happens, or it does not happen.

//! asks for two things: the real subscription URL instead of the `https://gosh.ai`
//! placeholder, and a refusal on a deployment where Gosh.ai issues no wallets -- before any URL or
//! QR reaches the screen, because a link that cannot work is worse than no link when the next thing
//! the command asks for is a recovery phrase.

//! **Written as "the manifest declares it", not as a comparison against a chain's name.** The issue
//! was filed on 24 August and merged on the 29th, deleting the client's own idea of which
//! chains exist;

//! `commands.rs` closes the "a test is an exception" loophole in advance. So the fixtures here are
//! written inline and labelled neutrally: what is under test is the DECISION, and the decision
//! never sees a name it recognises. The assertion that the manifest this repository publishes
//! carries the exact URL the issue names lives in `tests/`, which is deleted before the public tree
//! is built and may therefore name the file it reads.

//! What the manifest-driven shape buys, beyond the rule: a third deployment gets the right
//! behaviour for free, the product URL stops being compiled into the binary, and changing it is a
//! manifest edit rather than a release.

use super::wallet_goshai::{goshai_invitation_url, render_goshai_invitation};

/// A deployment described by exactly the fields `Deployed` requires, plus whatever this test adds.

/// Inline rather than `include_str!` of a shipped manifest, for two reasons that both bite. A
/// network name in `crates/**/*.rs` is the thing removed; and this module compiles inside the
/// PUBLISHED tree, which carries one manifest only -- a test that reads the other by name stops the
/// public tree from compiling, which `ci/release/common/check_public_tree_tests.sh` exists to catch.
fn deployment(declared: Option<&str>) -> dexdo_core::Deployed {
    let hex = "0".repeat(64);
    let mut document = serde_json::json!({
        "network": "net-a",
        "superroot": format!("0:{hex}"),
        "dapp_config": format!("0:{hex}"),
        "dapp_id": hex,
    });
    // Inserted as a JSON VALUE, never spliced into the text: several of the links under test carry
    // carriage returns and escape sequences, which is the whole point of them, and hand-built JSON
    // would fail to parse instead of exercising the check.
    if let Some(url) = declared {
        document["goshai_onboarding_url"] = serde_json::Value::String(url.to_string());
    }
    serde_json::from_value(document).expect("the fixture parses as a deployment manifest")
}

/// A deployment whose operator can get a Gosh.ai wallet.
fn with_goshai(url: &str) -> dexdo_core::Deployed {
    deployment(Some(url))
}

/// The link a deployment declares is the link that is offered, unchanged.
#[test]
fn the_declared_link_is_the_one_offered() {
    let declared = "https://gosh.ai/subscription/login/?utm_source=dexdo";
    let manifest = with_goshai(declared);

    assert_eq!(
        goshai_invitation_url(&manifest).expect("this deployment declares onboarding"),
        declared,
        "the client must offer what the deployment declared, not a link of its own"
    );
}

/// A deployment that declares no Gosh.ai onboarding refuses, and the refusal is usable.

/// Usable means it says what is unavailable and where the operator is, rather than failing with
/// something they have to translate. The deployment is named from the manifest's own label -- a
/// string it declared about itself, not a name the client keeps a list of.
#[test]
fn a_deployment_without_goshai_refuses_and_says_where_it_is() {
    let manifest = deployment(None);
    let refusal = goshai_invitation_url(&manifest)
        .expect_err("this deployment declares no Gosh.ai onboarding")
        .to_string();

    assert!(
        refusal.contains(manifest.network.trim()),
        "the refusal must say which deployment it is talking about: {refusal}"
    );
    assert!(
        refusal.to_lowercase().contains("gosh.ai"),
        "and which provider is unavailable: {refusal}"
    );

    // An empty or blank declaration is not a declaration. Without this, `"": ""` in a manifest
    // would render `export`-ready nothing and encode a QR of the empty string.
    for blank in ["", "   "] {
        assert!(
            goshai_invitation_url(&with_goshai(blank)).is_err(),
            "a blank declaration must not count as one: {blank:?}"
        );
    }
}

/// The refusal carries no link of any kind.

/// This is the acceptance criterion that costs money to get wrong. An operator shown any URL at
/// this moment is one who opens it, gets a wallet the client cannot use, and pastes a recovery
/// phrase into a flow that was doomed before it printed anything. "No Gosh.ai, stage, or
/// placeholder URL" is checked as "no URL", because a URL nobody meant to print is exactly the one
/// that slips in.
#[test]
fn no_refusal_shows_a_url_at_all() {
    // All THREE refusals, not just the absent-declaration one. The two validation refusals are the
    // ones where echoing would hurt most: they would print the very escape or override they just
    // rejected, onto the screen that precedes a recovery-phrase prompt. The code does not echo --
    // this is what holds it there.
    let refusals = [
        goshai_invitation_url(&deployment(None)).expect_err("no declaration"),
        goshai_invitation_url(&with_goshai("http://gosh.ai/subscription")).expect_err("not https"),
        goshai_invitation_url(&with_goshai("https://gosh.ai/\u{202e}elpmaxe.live"))
            .expect_err("unprintable"),
    ];

    for refusal in refusals {
        let said = refusal.to_string();
        let lowered = said.to_lowercase();
        for forbidden in ["http://", "https://", "www.", "stage", "placeholder"] {
            assert!(
                !lowered.contains(forbidden),
                "a refusal carries {forbidden:?}, which is a link the operator will follow: {said}"
            );
        }
        assert!(
            said.chars()
                .all(|c| !c.is_control() && c != '\u{202e}' && c != '\u{200b}'),
            "a refusal echoed a character it refused the link for: {said:?}"
        );
    }
}

/// A hostile network label does not reach the screen as itself either.

/// The label comes out of the same downloaded document as the link, and `Deployed::load` validates
/// no fields. These refusals print it immediately before the recovery-phrase prompt, so it goes
/// through the same filter -- replaced, not deleted, because a label rendered `net?a` still tells
/// the operator which deployment answered.
#[test]
fn a_hostile_network_label_is_not_echoed_as_itself() {
    let hex = "0".repeat(64);
    let mut document = serde_json::json!({
        "network": "net\u{202e}a\u{7}",
        "superroot": format!("0:{hex}"),
        "dapp_config": format!("0:{hex}"),
        "dapp_id": hex,
    });
    document["network"] = serde_json::Value::String("net\u{202e}a\u{7}".to_string());
    let manifest: dexdo_core::Deployed =
        serde_json::from_value(document).expect("the fixture parses");

    let said = goshai_invitation_url(&manifest)
        .expect_err("this deployment declares no onboarding")
        .to_string();

    assert!(
        !said.contains('\u{202e}') && !said.contains('\u{7}'),
        "the refusal echoed the label verbatim: {said:?}"
    );
    assert!(
        said.contains("net?a?"),
        "the label must still identify the deployment that answered: {said}"
    );
}

/// A declared link that cannot be shown safely is refused, not shown.

/// The value moved out of a compiled constant and into a file the operator DOWNLOADS, and the
/// screen it lands on is the one that asks for a recovery phrase. The control-character case is
/// the reason this test exists rather than the scheme case: `\r` or an escape sequence makes the
/// terminal rewrite the line, so what is READ stops being what the QR ENCODES -- and no byte
/// comparison of a captured buffer can see that, because the divergence happens in the terminal.
#[test]
fn a_link_that_cannot_be_shown_safely_is_refused() {
    let unsafe_links = [
        "http://gosh.ai/subscription",
        "ftp://gosh.ai/subscription",
        "gosh.ai/subscription",
        "https://gosh.ai/a\rhttps://elsewhere.example",
        "https://gosh.ai/a\u{1b}[2Khttps://elsewhere.example",
        "https://gosh.ai /subscription",
        "https://gosh.ai/a\nb",
        // Unicode FORMAT characters, category Cf. `is_control()` is Cc and `is_whitespace()` is
        // White_Space; neither covers these, and the first version of the check used exactly that
        // pair -- measured, all three passed it. A right-to-left override is the textbook way to
        // make a terminal render something other than its bytes, which is the failure the check
        // exists to prevent, so it is the case worth writing down.
        "https://gosh.ai/\u{202e}elpmaxe.live",
        "https://gosh.ai/\u{200b}subscription",
        "https://gosh.ai/\u{202d}subscription",
        // A scheme and nothing else: an invitation to nowhere, printed and encoded all the same.
        "https://",
    ];

    for link in unsafe_links {
        let manifest = with_goshai(link);
        assert!(
            goshai_invitation_url(&manifest).is_err(),
            "a link the operator would follow with a phone must be refused when it cannot be \
             shown honestly: {link:?}"
        );
    }

    // And the ordinary one still passes, so the checks above are a boundary and not a wall.
    assert!(goshai_invitation_url(&with_goshai(
        "https://gosh.ai/subscription/login/?utm_source=dexdo"
    ))
    .is_ok());
}

/// What the eye reads and what the camera reads are the same string.

/// They are produced from one value on purpose. A QR whose payload differs from the text beside it
/// is trusted without being read -- that is what printing a code is for.
#[test]
fn the_printed_url_and_the_encoded_url_are_one_string() {
    let manifest = with_goshai("https://gosh.ai/subscription/login/?utm_source=dexdo");
    let url = goshai_invitation_url(&manifest).expect("an invitation to render");

    let mut shown = Vec::new();
    render_goshai_invitation(&mut shown, url).expect("the invitation renders");
    let shown_text = String::from_utf8_lossy(&shown).to_string();

    assert!(
        shown_text.contains(url),
        "the URL must be readable as text, for a terminal with no camera in front of it"
    );

    // The payload, compared by rendering: by the time it is written it is half-blocks, so the
    // comparison is against a code built from the same string and drawn the same way.
    let expected = qrcode::QrCode::new(url.as_bytes()).expect("the URL fits a QR code");
    let mut again = Vec::new();
    crate::cli::qr_display::write_qr(&mut again, &expected).expect("draw the same code");
    assert!(
        shown
            .windows(again.len())
            .any(|window| window == again.as_slice()),
        "the code printed does not encode the URL printed beside it"
    );

    // And a different URL draws a different code -- otherwise the check above would pass on any
    // two strings that happen to render the same number of rows.
    let other = qrcode::QrCode::new(b"https://example.invalid").expect("fits");
    let mut different = Vec::new();
    crate::cli::qr_display::write_qr(&mut different, &other).expect("draw");
    assert_ne!(
        again, different,
        "two different URLs must not draw the same code, or this test proves nothing"
    );
}

/// Where the decision SITS, not just what it decides.

/// Driving `goshai_invitation_url` on its own proves the decision and says nothing about its
/// position. Review measured the gap: hoist the call back above the resume fork -- which is where
/// the first version of this change had it, and where it refused a RESUMING operator with "nothing
/// was written" while their recovery phrase was already owner-only on disk -- and every behavioural
/// test here stays green.

/// Held by shape, the way `onboard_endpoint_source_1839_tests` holds the endpoint call one line
/// above, and for the same reason: the command needs an interactive terminal, so there is no
/// process-level test that could reach this ordering. Comments are stripped first, or prose naming
/// these calls would satisfy the guard.
#[test]
fn the_invitation_is_resolved_inside_the_branch_that_prints_it() {
    let body = crate::cli::source_probe::code_of(
        include_str!("wallet_goshai.rs"),
        "pub(crate) async fn run_wallet_onboard_goshai(",
    );

    let resume_fork = body
        .find("resume_onboarding(")
        .expect("the resume fork is still where the flow branches");
    let decision = body
        .find("goshai_invitation_url(")
        .expect("the flow still asks whether this deployment has Gosh.ai onboarding");
    let render = body
        .find("render_goshai_invitation(")
        .expect("the flow still prints an invitation");

    assert!(
        decision > resume_fork,
        "the invitation is resolved ABOVE the resume fork, so a resuming operator is refused with          `nothing was written` while their recovery phrase is already on disk"
    );
    assert!(
        decision < render,
        "the invitation is printed before the deployment is asked whether it has one"
    );
}
