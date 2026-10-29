use super::*;

fn operator_text(error: &anyhow::Error) -> String {
    assert!(shown_to_operator(error).is_none());
    for_operator(error)
        .expect("provision failures have operator guidance")
        .render_with(crate::cli::style::Palette::None)
}

#[test]
fn issue_2192_review_operator_keeps_post_order_book_context() {
    let error = anyhow::anyhow!("connection reset by peer").context(
        "provision step=read RootModel; order-book deployment requested=true, cost unconfirmed; deal reserve not submitted"
    );
    let shown = operator_text(&error);
    assert!(shown.contains("provision step=read RootModel"), "{shown}");
    assert!(
        shown.contains("order-book deployment requested=true"),
        "{shown}"
    );
    assert!(shown.contains("cost unconfirmed"), "{shown}");
    assert!(!shown.contains("nothing was sent"), "{shown}");
}

#[test]
fn issue_2192_review_operator_keeps_ambiguous_deploy_context() {
    let error = anyhow::Error::new(dexdo_core::MoneySubmitError::Ambiguous {
        source: anyhow::anyhow!("operation timed out"),
    }).context("provision step=wait for deal; deployDeal submitted with requested reserve=20000000000 raw nanoSHELL ECC[2]; cost unconfirmed; nonce=123");
    let shown = operator_text(&error);
    assert!(shown.contains("provision step=wait for deal"), "{shown}");
    assert!(shown.contains("deployDeal submitted"), "{shown}");
    assert!(shown.contains("requested reserve=20000000000"), "{shown}");
    assert!(!shown.contains("nothing was sent"), "{shown}");
    assert!(!crate::cli::support::provision_failure_clears_pending(
        &error, false
    ));
}

#[test]
fn issue_2192_review_operator_preserves_each_spend_boundary() {
    for stage in [
        "provision step=load registry snapshot; no deployment submitted",
        "provision step=read RootModel; order-book deployment requested=true, cost unconfirmed; deal reserve not submitted",
        "provision step=read prior deal history; order-book deployment requested=true; RootModel deployment requested=true (funded by SuperRoot); actual note cost unconfirmed; deal reserve not submitted",
        "provision step=PrivateNote.deployDeal; actual note cost unconfirmed; requested deal reserve=20000000000 raw nanoSHELL ECC[2]; submission outcome is reported by the cause",
        "provision step=wait for deal; deployDeal submitted with requested reserve=20000000000 raw nanoSHELL ECC[2]; cost unconfirmed; nonce=123",
    ] {
        for cause in [
            format!("{}{} attempts: connection reset by peer", dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX, dexdo_core::params::TRANSIENT_READ_ATTEMPTS),
            "insufficient SHELL".to_owned(),
        ] {
            let error = anyhow::Error::msg(cause.clone()).context(stage);
            let shown = operator_text(&error);
            assert!(shown.contains(stage), "{shown}");
            assert!(shown.contains(&cause), "{shown}");
            assert!(shown.contains("Keep the recovery files and the same deal nonce"), "{shown}");
            assert!(shown.contains("retrying provision"), "{shown}");
            assert!(!shown.contains("nothing was sent"), "{shown}");
            assert!(!shown.contains("Top up"), "{shown}");
        }
    }
}

proptest::proptest! {
    #[test]
    fn issue_2192_review_operator_never_loses_reserve_or_nonce(
        reserve in proptest::prelude::any::<u128>(),
        nonce in proptest::prelude::any::<u64>(),
        cause in proptest::sample::select(vec!["connection reset by peer", "operation timed out", "insufficient SHELL"]),
    ) {
        let stage = format!("provision step=wait for deal; deployDeal submitted with requested reserve={reserve} raw nanoSHELL ECC[2]; cost unconfirmed; nonce={nonce}");
        let error = anyhow::Error::msg(cause).context(stage.clone());
        let shown = operator_text(&error);
        proptest::prop_assert!(shown.contains(&stage));
        proptest::prop_assert!(shown.contains("same deal nonce"));
        proptest::prop_assert!(!shown.contains("nothing was sent"));
        proptest::prop_assert!(!shown.contains("Top up"));
    }
}
