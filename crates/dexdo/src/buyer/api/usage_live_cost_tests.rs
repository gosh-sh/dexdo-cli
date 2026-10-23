use super::*;
use axum::body::Body;
use dexdo_core::params::{SHELL_UNIT, TICK_SIZE};
use dexdo_proto::BillingUsage;
use tower::ServiceExt;

#[tokio::test]
async fn accepted_answers_advance_http_cost_and_quota_without_a_payment_or_new_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
    store.register_deal("deal", "model").unwrap();
    store.open_deal_quota("deal", 9, 12 * SHELL_UNIT).unwrap();
    let state = ApiState::lazy(
        Arc::new(crate::buyer::Buyer::generate()),
        "model".into(),
        Arc::new(|| Box::pin(async { panic!("usage must not buy") })),
        dexdo_core::params::MATCH_OPEN_TIMEOUT,
    )
    .with_usage(store.clone());
    let app = super::super::router(state, true);
    for (id, tokens, expected, remaining) in [
        ("first", TICK_SIZE / 2, 6, 102),
        ("second", TICK_SIZE, 18, 90),
    ] {
        store.start_request(id, "model", "openai", false).unwrap();
        store.link_deal(id, "deal").unwrap();
        store
            .record_call_usage(
                id,
                "answer",
                "answer",
                BillingUsage::new(tokens as u64, 0, None).unwrap(),
                true,
            )
            .unwrap();
        store.finish_request(id, 200, Some("stop"), None).unwrap();
        let response = app
            .clone()
            .oneshot(http::Request::get("/v1/usage").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 8);
        assert_eq!(value["usage"]["total"]["cost"], expected);
        assert_eq!(value["usage"]["today"]["cost"], expected);
        assert_eq!(value["model_stats"][0]["cost"], expected);
        assert_eq!(
            value["daily_usage"].as_array().unwrap().last().unwrap()["cost"],
            expected
        );
        assert_eq!(value["usage"]["total"]["overhead_cost"], 0);
        assert_eq!(value["quota"]["used"], expected);
        assert_eq!(value["quota"]["remaining"], remaining);
        assert_eq!(value["remaining"], value["quota"]["remaining"]);
        assert_eq!(value["unit"], value["quota"]["unit"]);
        assert!(value["usage"]["total"].get("confirmed_expense").is_none());
    }
}
