//! Low-rate read admission must preserve progress and the configured ceiling.
use super::{ChainRequestCeiling, RequestGate};
use std::future::Future;
use std::task::Poll;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn an_older_waiter_gets_the_next_slot_before_a_new_read() {
    for ceiling in [1, 2, 3] {
        let gate = RequestGate::new(ChainRequestCeiling::PerSecond(ceiling));
        for _ in 0..ceiling {
            gate.admit().await;
        }
        let waiting = gate.admit();
        tokio::pin!(waiting);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        let newcomer = gate.admit();
        tokio::pin!(newcomer);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(newcomer.as_mut().poll(cx)))
                .await
                .is_pending(),
            "a new read took the slot of an older waiter at ceiling {ceiling}"
        );
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx)))
                .await
                .is_ready()
        );
        newcomer.await;
    }
}

#[tokio::test(start_paused = true)]
async fn admission_wait_does_not_use_the_attempt_or_retry_budget() {
    let gate = RequestGate::new(ChainRequestCeiling::PerSecond(1));
    let held = gate.granted.lock().await;
    let calls = std::cell::Cell::new(0);
    let read = super::retry_chain_read(Some(&gate), || async {
        calls.set(calls.get() + 1);
        Err::<(), _>(anyhow::Error::new(super::GraphQlBodyError::from_errors(
            &serde_json::json!([
                {"message": "pool timed out while waiting for an open connection"}
            ]),
        )))
    });
    tokio::pin!(read);
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    tokio::time::advance(crate::params::TRANSIENT_READ_TOTAL_BUDGET * 2).await;
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert_eq!(
        calls.get(),
        0,
        "a queued read has not sent a network request"
    );
    drop(held);
    assert!(
        read.await.is_err(),
        "the actual network failure still propagates"
    );
    assert_eq!(
        calls.get(),
        crate::params::TRANSIENT_READ_ATTEMPTS,
        "local queue time must not exhaust the network retry budget"
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_the_front_waiter_releases_its_turn() {
    for ceiling in [1, 2, 3] {
        let gate = RequestGate::new(ChainRequestCeiling::PerSecond(ceiling));
        for _ in 0..ceiling {
            gate.admit().await;
        }
        let mut cancelled = Box::pin(gate.admit());
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(cancelled.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let mut next = Box::pin(gate.admit());
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        drop(cancelled);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx)))
                .await
                .is_ready()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_permanent_network_failure_is_not_retried() {
    let gate = RequestGate::new(ChainRequestCeiling::PerSecond(1));
    let calls = std::cell::Cell::new(0);
    let error = super::retry_chain_read(Some(&gate), || async {
        calls.set(calls.get() + 1);
        Err::<(), _>(anyhow::anyhow!("permanent identity failure"))
    })
    .await
    .unwrap_err();
    assert_eq!(calls.get(), 1);
    assert_eq!(error.to_string(), "permanent identity failure");
}

proptest::proptest! {
    #[test]
    fn cancellations_never_exceed_the_configured_sliding_window(
        ceiling in 1u32..=3,
        cancel in proptest::collection::vec(proptest::bool::ANY, 1..40),
    ) {
        tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true)
            .build().unwrap().block_on(async {
                let gate = std::sync::Arc::new(RequestGate::new(ChainRequestCeiling::PerSecond(ceiling)));
                let starts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let mut tasks = Vec::new();
                for _ in &cancel {
                    let gate = gate.clone();
                    let starts = starts.clone();
                    tasks.push(tokio::spawn(async move {
                        gate.admit().await;
                        starts.lock().unwrap().push(tokio::time::Instant::now());
                    }));
                }
                tokio::task::yield_now().await;
                for (task, cancel) in tasks.iter().zip(&cancel) {
                    if *cancel { task.abort(); }
                }
                for task in tasks { let _ = task.await; }
                let mut starts = starts.lock().unwrap().clone();
                starts.sort_unstable();
                for index in ceiling as usize..starts.len() {
                    assert!(starts[index].duration_since(starts[index - ceiling as usize]) >= Duration::from_secs(1));
                }
            });
    }
}
