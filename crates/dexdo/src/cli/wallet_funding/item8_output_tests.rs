use super::*;
use crate::cli::machine::MachineFundingNotice;

const NOTICE: &str = "Hot wallet funding request submitted.";

/// The notice is a call to act, so it is drawn like every other one: amber and bold, and only where
/// colour is allowed at all.

/// The painted form is compared against `style::action` rather than against a literal escape,
/// because which escape it is depends on what the terminal reports -- 24-bit or 256 colours -- and
/// a test that pinned one of them would be pinning the machine it ran on. What IS pinned here is
/// that colour appears only for a terminal that did not refuse it, that the weight is there, and
/// that the sentence itself is never altered.
#[test]
fn the_ackinacki_notice_is_drawn_as_a_call_to_act_only_where_colour_is_allowed() {
    use crate::cli::style::{self, Palette};

    let painted = render_ackinacki_funding_notice(NOTICE, true, false);
    assert_eq!(
        painted,
        style::action(Palette::resolved(true, false), NOTICE)
    );
    assert!(
        painted.contains(NOTICE),
        "the sentence survives the drawing: {painted}"
    );
    assert!(
        painted.contains("\u{1b}[1m"),
        "a call to act carries its weight: {painted}"
    );
    assert_ne!(painted, NOTICE, "a terminal that takes colour gets it");

    assert_eq!(
        render_ackinacki_funding_notice(NOTICE, false, false),
        NOTICE,
        "a pipe gets the sentence and no escapes"
    );
    assert_eq!(
        render_ackinacki_funding_notice(NOTICE, true, true),
        NOTICE,
        "colour refused is colour refused"
    );
}

#[test]
fn every_funding_notice_has_one_stable_secret_free_machine_event() {
    let evidence = FundingEvidence {
        verdict: "executed".to_string(),
        source: "secret-looking-provider-response".to_string(),
        observed_at_unix: Some(7),
        detail: "owner_secret_key_hex=must-not-leak".to_string(),
        delivery_message_id: None,
    };
    let cases = [
        (FundingNotice::AlreadyFunded, "already_funded"),
        (FundingNotice::RequestSubmitted, "request_submitted"),
        (
            FundingNotice::RequestAlreadyPending,
            "request_already_pending",
        ),
        (
            FundingNotice::RequestExecuted { evidence },
            "request_executed",
        ),
        (
            FundingNotice::RequestIndeterminate {
                reason: "private_key=must-not-leak".to_string(),
            },
            "request_indeterminate",
        ),
        (
            FundingNotice::ManualTopUpRequested,
            "manual_top_up_requested",
        ),
    ];

    for (notice, expected) in cases {
        let machine: MachineFundingNotice = notice.machine_notice();
        let encoded = serde_json::to_string(&machine).expect("serialize machine notice");
        assert_eq!(encoded.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
            serde_json::json!({ "event": expected })
        );
        assert!(!encoded.contains("secret") && !encoded.contains("private_key"));
    }
}
