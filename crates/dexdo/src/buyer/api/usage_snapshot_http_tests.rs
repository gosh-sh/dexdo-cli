use super::*;
use tower::ServiceExt;

fn state(store: Option<Arc<UsageStore>>) -> ApiState {
    let state = ApiState::lazy(
        Arc::new(crate::buyer::Buyer::generate()),
        "m".into(),
        Arc::new(|| Box::pin(async { panic!("GET admission must not buy") })),
        dexdo_core::params::MATCH_OPEN_TIMEOUT,
    );
    match store {
        Some(store) => state.with_usage(store),
        None => state,
    }
}

async fn get(state: ApiState) -> (StatusCode, Value) {
    let response = super::super::router(state)
        .oneshot(
            http::Request::get("/v1/usage/admission")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn snapshot_2237_http_returns_only_current_model_totals_and_quota() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(UsageStore::open(dir.path().join("usage.sqlite3"), "shellnet").unwrap());
    store
        .start_request("other", "other-model", "openai", false)
        .unwrap();
    store
        .record_call_usage(
            "other",
            "answer",
            "answer",
            dexdo_proto::BillingUsage::new(5, 7, None).unwrap(),
            true,
        )
        .unwrap();
    let (status, value) = get(state(Some(store.clone()))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["model"], "m");
    assert_eq!(value["accepted"]["input_tokens"], "0");
    assert_eq!(value.as_object().unwrap().len(), 5);
    assert_eq!(
        value["quota"],
        store.admission_snapshot("m").unwrap()["quota"]
    );
    store.start_request("new", "m", "openai", false).unwrap();
    let (status, value) = get(state(Some(store.clone()))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["complete"], false);
    store.begin_deal_quota("unresolved").unwrap();
    let (status, value) = get(state(Some(store))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(value["error"]["type"], "usage_unavailable");
    let (status, _) = get(state(None)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
