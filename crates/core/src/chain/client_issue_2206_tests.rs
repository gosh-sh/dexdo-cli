use super::*;
use proptest::prelude::*;

pub(super) fn bk_unavailable(account: &str) -> String {
    format!("failed to get account {account}: Invalid server response: All BK API endpoints failed; traceparent: 00-00000000000000000000000038f4ca33-0000000038f4ca33-01")
}

pub(super) fn sdk_error(message: &str) -> anyhow::Error {
    anyhow!(
        "GraphQL errors: {}",
        json!([{"message": message, "path": ["blockchain", "account", "info"]}])
    )
}

#[test]
fn issue_2206_exact_bk_account_failure_is_only_a_transient_read() {
    let message =
        bk_unavailable("d446d4b942523bfb20509f4c8450c65dc6e21957f9d626ca7d34cebf2ce3a2ab");
    for error in [
        sdk_error(&message),
        anyhow!("GraphQL errors: {message}"),
        anyhow::Error::new(GraphQlBodyError::from_errors(&json!([{
            "message": message,
            "locations": [{"line": 1, "column": 183}],
            "path": ["blockchain", "account", "info"]
        }]))),
    ] {
        let error = error.context("mainnet account read");
        assert!(is_transient_read_failure(&error), "{error:#}");
        assert!(!is_transient_transport_failure(&error));
        assert!(!is_transient_submit_failure(&error));
    }
}

#[test]
fn issue_2206_similar_prose_schema_and_contract_errors_stay_permanent() {
    let message = bk_unavailable(&"a".repeat(64));
    for error in [
        sdk_error("Cannot query field 'info' on type 'Account'"),
        sdk_error("on-chain revert: All BK API endpoints failed; exit_code=151"),
        sdk_error("All BK API endpoints failed"),
        sdk_error("failed to get account a: invalid account_id; All BK API endpoints failed"),
        sdk_error(&format!("operator description: {message}")),
        sdk_error(&message.replace("Invalid server response:", "contract refused:")),
        sdk_error(&message.replace("endpoints failed;", "endpoints failed validation;")),
        anyhow!("{message}"),
        anyhow!("GraphQL errors: {{broken JSON: {message}}}"),
    ] {
        assert!(!is_transient_read_failure(&error), "{error:#}");
        assert!(!is_transient_submit_failure(&error));
    }
}

proptest! {
    #[test]
    fn issue_2206_wrapped_account_read_never_authorizes_a_money_retry(
        account in "[0-9a-f]{64}",
        trace_id in "[0-9a-f]{32}",
        span_id in "[0-9a-f]{16}",
        contexts in prop::collection::vec("[a-z ]{1,32}", 0..5),
    ) {
        let message = format!("failed to get account {account}: Invalid server response: All BK API endpoints failed; traceparent: 00-{trace_id}-{span_id}-01");
        for mut error in [sdk_error(&message), anyhow::Error::new(
            GraphQlBodyError::from_errors(&json!([{"message": message}]))
        )] {
            for context in &contexts {
                error = error.context(context.clone());
            }
            prop_assert!(is_transient_read_failure(&error));
            prop_assert!(!is_transient_submit_failure(&error));
        }
    }
}

#[tokio::test]
async fn issue_2206_http_200_sdk_account_read_recovers_without_a_submit() {
    client_issue_2122_tests::sdk_account_read_recovers(bk_unavailable).await;
}
