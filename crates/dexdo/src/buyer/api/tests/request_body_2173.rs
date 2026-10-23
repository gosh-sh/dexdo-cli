use super::*;
use axum::body::to_bytes;
use dexdo_core::params::BUYER_API_REQUEST_BODY_MAX_BYTES;
use http::{Request, StatusCode};
use std::sync::atomic::AtomicUsize;
use tower::ServiceExt;

const PATHS: [&str; 3] = ["/v1/chat/completions", "/v1/responses", "/v1/messages"];

fn lazy_router() -> (axum::Router, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let init_calls = calls.clone();
    let state = ApiState::lazy(
        Arc::new(Buyer::generate()),
        "dexdo-mock".to_string(),
        Arc::new(move || {
            init_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(DealInitError::new("body reached deal initialization")) })
        }),
        MATCH_OPEN_TIMEOUT,
    );
    (router(state, true), calls)
}

fn json_body(path: &str, size: usize) -> Vec<u8> {
    let (prefix, suffix) = if path == "/v1/responses" {
        (r#"{"input":""#, r#"","max_output_tokens":1}"#)
    } else {
        (
            r#"{"messages":[{"role":"user","content":""#,
            r#""}],"max_tokens":1}"#,
        )
    };
    let mut body = Vec::with_capacity(size);
    body.extend_from_slice(prefix.as_bytes());
    body.resize(size - suffix.len(), b'x');
    body.extend_from_slice(suffix.as_bytes());
    assert_eq!(body.len(), size);
    body
}

async fn post(app: axum::Router, path: &str, body: Body, length: Option<usize>) -> Response {
    let mut request = Request::post(path).header("content-type", "application/json");
    if let Some(length) = length {
        request = request.header("content-length", length);
    }
    app.oneshot(request.body(body).unwrap()).await.unwrap()
}

async fn assert_too_large(response: Response, calls: &AtomicUsize, path: &str) {
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{path}");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let error: serde_json::Value = serde_json::from_slice(&body).expect("JSON size refusal");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&BUYER_API_REQUEST_BODY_MAX_BYTES.to_string()),
        "{error}"
    );
    if path == "/v1/messages" {
        assert_eq!(error["type"], "error");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "{path}: must not open a deal"
    );
}

#[tokio::test]
async fn valid_large_json_reaches_initializer_below_and_at_limit() {
    for path in PATHS {
        for size in [
            BUYER_API_REQUEST_BODY_MAX_BYTES - 1,
            BUYER_API_REQUEST_BODY_MAX_BYTES,
        ] {
            let (app, calls) = lazy_router();
            let response = post(app, path, Body::from(json_body(path, size)), Some(size)).await;
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{path}, {size}"
            );
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("body reached deal initialization"));
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{path}, {size}");
        }
    }
}

#[tokio::test]
async fn oversized_json_is_refused_before_deal_initialization() {
    for path in PATHS {
        let size = BUYER_API_REQUEST_BODY_MAX_BYTES + 1;
        let (app, calls) = lazy_router();
        let response = post(app, path, Body::from(json_body(path, size)), Some(size)).await;
        assert_too_large(response, &calls, path).await;
    }
}

#[tokio::test]
async fn oversized_stream_stops_reading_at_the_limit() {
    for path in PATHS {
        let (app, calls) = lazy_router();
        let tail_polls = Arc::new(AtomicUsize::new(0));
        let polled = tail_polls.clone();
        let chunks = futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![b' '; BUYER_API_REQUEST_BODY_MAX_BYTES])),
            Ok(Bytes::from_static(b"!")),
        ])
        .chain(futures::stream::once(async move {
            polled.fetch_add(1, Ordering::SeqCst);
            Ok(Bytes::from_static(b"must not be buffered"))
        }));
        let response = post(app, path, Body::from_stream(chunks), None).await;
        assert_too_large(response, &calls, path).await;
        assert_eq!(tail_polls.load(Ordering::SeqCst), 0, "{path}");
    }
}

#[tokio::test]
async fn malformed_json_never_initializes_a_deal() {
    for path in PATHS {
        for size in [
            BUYER_API_REQUEST_BODY_MAX_BYTES - 1,
            BUYER_API_REQUEST_BODY_MAX_BYTES,
        ] {
            let (app, calls) = lazy_router();
            let mut body = json_body(path, size);
            *body.last_mut().unwrap() = b'!';
            let response = post(app, path, Body::from(body), Some(size)).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}, {size}");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        let (app, calls) = lazy_router();
        let size = BUYER_API_REQUEST_BODY_MAX_BYTES + 1;
        let response = post(app, path, Body::from(vec![b'!'; size]), Some(size)).await;
        assert_too_large(response, &calls, path).await;
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(8))]
    #[test]
    fn streamed_body_limit_is_enforced_without_content_length(
        path_index in 0..PATHS.len(),
        offset in -4i32..=4,
        split in 1usize..BUYER_API_REQUEST_BODY_MAX_BYTES,
    ) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let path = PATHS[path_index];
            let size = BUYER_API_REQUEST_BODY_MAX_BYTES.checked_add_signed(offset as isize).unwrap();
            let bytes = Bytes::from(json_body(path, size));
            let split = split.min(size);
            let chunks = [bytes.slice(..split), bytes.slice(split..)];
            let body = Body::from_stream(futures::stream::iter(
                chunks.into_iter().map(Ok::<_, std::io::Error>),
            ));
            let (app, calls) = lazy_router();
            let response = post(app, path, body, None).await;
            if size > BUYER_API_REQUEST_BODY_MAX_BYTES {
                assert_too_large(response, &calls, path).await;
            } else {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
        });
    }
}
