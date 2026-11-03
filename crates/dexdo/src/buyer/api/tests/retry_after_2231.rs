use super::super::error::{provider_retry_headers, retry_headers};
use super::*;
use proptest::prelude::*;

#[tokio::test]
async fn seller_retry_metadata_reaches_both_error_formats_2231() {
    for anthropic in [false, true] {
        for (busy, onchain, expected) in [
            (true, false, "deal_busy"),
            (false, true, "onchain_deal_limit"),
            (false, false, "deal_capacity_unavailable"),
        ] {
            let mut status =
                tonic::Status::resource_exhausted("diagnostic text is not a discriminator");
            status
                .metadata_mut()
                .insert("dexdo-retry-after-ms", "61001".parse().unwrap());
            if busy {
                status
                    .metadata_mut()
                    .insert("dexdo-error-code", "deal_busy".parse().unwrap());
            }
            if onchain {
                status
                    .metadata_mut()
                    .insert("dexdo-limit", "onchain_deal_limit".parse().unwrap());
            }
            let response = capacity_response(&anyhow::Error::new(status), anthropic);
            assert_eq!(response.headers()["retry-after"], "62");
            assert_eq!(response.headers()["retry-after-ms"], "61001");
            assert_eq!(response.headers().contains_key("dexdo-limit"), onchain);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], expected);
        }
    }
}

#[test]
fn legacy_and_invalid_seller_delays_do_not_invent_headers_2231() {
    for hint in [
        None,
        Some(""),
        Some("0"),
        Some("-1"),
        Some("1.5"),
        Some("18446744073709551616"),
    ] {
        let mut status = tonic::Status::resource_exhausted("onchain_deal_limit deal_busy");
        if let Some(hint) = hint {
            status
                .metadata_mut()
                .insert("dexdo-retry-after-ms", hint.parse().unwrap());
        }
        let response = capacity_response(&anyhow::Error::new(status), false);
        for name in ["retry-after", "retry-after-ms", "dexdo-limit"] {
            assert!(!response.headers().contains_key(name), "{hint:?} {name}");
        }
    }
}

#[test]
fn provider_retry_hints_are_preserved_and_quota_is_not_retried_2231() {
    let default_seconds = dexdo_core::params::CAPACITY_RETRY_DEFAULT
        .as_secs()
        .to_string();
    let default_ms = dexdo_core::params::CAPACITY_RETRY_DEFAULT
        .as_millis()
        .to_string();
    for (seconds, milliseconds, expected_seconds, expected_ms) in [
        (Some("19"), Some("2501"), Some("19"), Some("2501")),
        (
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
            None,
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
            None,
        ),
        (None, Some("2501"), Some("3"), Some("2501")),
        (None, Some("invalid"), None, Some("invalid")),
        (
            None,
            None,
            Some(default_seconds.as_str()),
            Some(default_ms.as_str()),
        ),
    ] {
        let mut headers = http::HeaderMap::new();
        if let Some(value) = seconds {
            headers.insert("retry-after", value.parse().unwrap());
        }
        if let Some(value) = milliseconds {
            headers.insert("retry-after-ms", value.parse().unwrap());
        }
        provider_retry_headers(&mut headers, b"{\"error\":{\"type\":\"rate_limit_error\"}}");
        assert_eq!(
            headers
                .get("retry-after")
                .map(|value| value.to_str().unwrap()),
            expected_seconds
        );
        assert_eq!(
            headers
                .get("retry-after-ms")
                .map(|value| value.to_str().unwrap()),
            expected_ms
        );
    }
    for code in [
        "insufficient_quota",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
        "organization_usage_limit_exceeded",
        "usage_limit_exceeded",
        "billing_error",
    ] {
        for field in ["code", "type"] {
            let body = serde_json::json!({"error":{field:code}}).to_string();
            let mut headers = http::HeaderMap::new();
            provider_retry_headers(&mut headers, body.as_bytes());
            assert!(headers.is_empty(), "{body}");
        }
    }
    let mut headers = http::HeaderMap::new();
    provider_retry_headers(&mut headers, br#"{"error":{"type":"rate_limit_error","message":"You have reached your API usage limits: your organization has crossed its monthly API usage threshold."}}"#);
    assert!(headers.is_empty());
}

proptest! {
    #[test]
    fn retry_seconds_never_round_down_2231(milliseconds in 1u64..=u64::MAX) {
        let mut headers = http::HeaderMap::new();
        retry_headers(&mut headers, milliseconds);
        let seconds: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        prop_assert!(u128::from(seconds) * 1000 >= u128::from(milliseconds));
        prop_assert!(u128::from(seconds - 1) * 1000 < u128::from(milliseconds));
        prop_assert_eq!(headers["retry-after-ms"].to_str().unwrap().parse::<u64>().unwrap(), milliseconds);
    }
}
