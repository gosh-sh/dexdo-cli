use super::*;

#[test]
fn issue_2192_review_retirement_survives_all_later_failures_and_no_op() {
    for stage in [
        "read seller capacity",
        "read order-book state",
        "validate registry policy",
        "check note deposit",
        "wait for deal",
    ] {
        for cause in [
            format!(
                "{}{} attempt(s): connection reset by peer",
                dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX,
                dexdo_core::params::TRANSIENT_READ_ATTEMPTS
            ),
            "insufficient SHELL".to_owned(),
        ] {
            let error = provision_after_offer_retirement(
                Err(anyhow::Error::msg(cause).context(format!("provision step={stage}; {}", if stage == "wait for deal" {
                    "deployDeal submitted with requested reserve=20000000000 raw nanoSHELL ECC[2]; cost unconfirmed"
                } else {
                    "new deployment not submitted"
                }))),
                true,
            )
            .unwrap_err();
            let shown = crate::cli::refusal::for_operator(&error)
                .unwrap()
                .render_with(crate::cli::style::Palette::None);
            assert!(shown.contains("offer retirement attempted"), "{shown}");
            assert!(shown.contains("cost, if any, unconfirmed"), "{shown}");
            assert!(shown.contains(stage), "{shown}");
            assert!(!shown.contains("nothing was sent"), "{shown}");
            assert!(!shown.contains("Top up"), "{shown}");
        }
    }
    // A successful or no-op retirement is an attempt, not evidence of a charge.
    assert!(provision_after_offer_retirement(Ok(()), true).is_ok());
    let plain =
        provision_after_offer_retirement(Err(anyhow::anyhow!("unchanged")), false).unwrap_err();
    assert_eq!(plain.to_string(), "unchanged");
    let ambiguous = provision_after_offer_retirement(
        Err(anyhow::Error::new(
            dexdo_core::MoneySubmitError::Ambiguous {
                source: anyhow::anyhow!("POST response lost"),
            },
        )),
        true,
    )
    .unwrap_err();
    assert!(ambiguous
        .downcast_ref::<dexdo_core::MoneySubmitError>()
        .is_some());
    assert!(!provision_failure_clears_pending(&ambiguous, false));
}

#[test]
fn issue_2192_review_every_stage_after_retirement_is_inside_its_error_boundary() {
    let source = include_str!("admin.rs");
    let body = source
        .split_once("pub(crate) async fn run_provision_with_deal_gas_overhead")
        .unwrap()
        .1
        .split_once("fn provision_after_offer_retirement")
        .unwrap()
        .0;
    let retirement = body.find("retire_superseded_seller_offers(").unwrap();
    let boundary = body.find("let result = async {").unwrap();
    let finish = body
        .find("provision_after_offer_retirement(result, previous_market.is_some())")
        .unwrap();
    assert!(retirement < boundary && boundary < finish);
    for call in [
        "seller_deal_capacity(",
        "order_book_active(",
        "enforce_model_registry_policy_with_endpoint(",
        "prepare_fresh_provision_deposit(",
        "provision_market_with_deal_gas_overhead_and_before_deploy(",
        "std::fs::write(&args.output",
    ] {
        let at = body.find(call).unwrap();
        assert!(
            boundary < at && at < finish,
            "{call} must retain the retirement attempt"
        );
    }
}
