use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn deal(token_contract: &str, session: Arc<SessionSettle>) -> ApiDeal {
    ApiDeal::new(
        Route {
            handover: Handover {
                endpoint: "https://127.0.0.1:1".to_string(),
                tls_fingerprint: "00".repeat(32),
            },
            token_contract: token_contract.to_string(),
            max_tokens: 1,
        },
        session,
        Arc::new(ContentGate::skip()),
    )
}

fn sessions(chain: Arc<RecordingSettleChain>) -> (Arc<SessionSettle>, Arc<SessionSettle>) {
    let note = Arc::new(dexdo_core::LocalNote::generate());
    (
        Arc::new(SessionSettle::new(
            chain.clone(),
            "tc-previous".to_string(),
            note.clone(),
        )),
        Arc::new(SessionSettle::new(chain, "tc-next".to_string(), note)),
    )
}

#[tokio::test]
async fn zero_delivery_swap_makes_no_settle_call_and_installs_next_route() {
    let chain = Arc::new(RecordingSettleChain::default());
    let (previous_session, next_session) = sessions(chain.clone());
    let routes = RouteManager::new(deal("tc-previous", previous_session.clone()));

    routes
        .replace_active(
            || deal("tc-next", next_session.clone()),
            "continuity-renewal",
        )
        .await
        .expect("a zero-delivery route is dropped without settlement");

    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 0);
    assert!(!previous_session.is_settled());
    assert!(!next_session.is_settled());
    assert_eq!(
        routes.current().await.unwrap().route.token_contract,
        "tc-next"
    );
}

#[tokio::test]
async fn delivering_swap_makes_exactly_one_settle_call_and_installs_next_route() {
    let chain = Arc::new(RecordingSettleChain::default());
    let (previous_session, next_session) = sessions(chain.clone());
    let routes = RouteManager::new(deal("tc-previous", previous_session.clone()));
    deliver_one_request(&routes, 1).await;

    routes
        .replace_active(
            || deal("tc-next", next_session.clone()),
            "continuity-renewal",
        )
        .await
        .expect("a delivering route is settled before replacement");

    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    assert!(previous_session.is_settled());
    assert!(!next_session.is_settled());
    assert_eq!(
        routes.current().await.unwrap().route.token_contract,
        "tc-next"
    );
}

#[tokio::test]
async fn delivering_swap_propagates_settle_failure_and_keeps_previous_route() {
    let chain = Arc::new(RecordingSettleChain::default());
    chain.fail_stop.store(true, Ordering::SeqCst);
    let (previous_session, next_session) = sessions(chain.clone());
    let routes = RouteManager::new(deal("tc-previous", previous_session.clone()));
    deliver_one_request(&routes, 1).await;
    let next_factory_called = Arc::new(AtomicBool::new(false));
    let factory_called = next_factory_called.clone();

    let error = routes
        .replace_active(
            || {
                factory_called.store(true, Ordering::SeqCst);
                deal("tc-next", next_session.clone())
            },
            "continuity-renewal",
        )
        .await
        .expect_err("a failed settlement must prevent route replacement");

    assert!(
        error.to_string().contains("injected stop failure"),
        "{error}"
    );
    assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
    assert!(!previous_session.is_settled());
    assert!(!next_session.is_settled());
    assert!(!next_factory_called.load(Ordering::SeqCst));
    assert_eq!(
        routes.current().await.unwrap().route.token_contract,
        "tc-previous"
    );
}
