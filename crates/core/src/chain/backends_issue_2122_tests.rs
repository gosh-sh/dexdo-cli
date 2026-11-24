use super::*;
use crate::chain::client::retry_transient_read;
use crate::params::TRANSIENT_READ_ATTEMPTS;
use std::sync::atomic::{AtomicUsize, Ordering};

fn bm_error() -> anyhow::Error {
    anyhow!(
        "GraphQL errors: {}",
        json!([{"message": "failed to get account a: error sending request to http://block_manager:8600/v2/account?account_id=a&dapp_id=4: client error (Connect): tcp connect error: Connection refused (os error 111)"}])
    )
}

#[tokio::test(start_paused = true)]
async fn issue_2282_closed_pool_exhaustion_preserves_read_recovery_and_submit_policy() {
    let closed_pool = || {
        anyhow!(
            "GraphQL errors: {}",
            json!([{"message": "attempted to acquire a connection on a closed pool",
                "path": ["blockchain", "account", "messages"]}])
        )
    };
    let calls = AtomicUsize::new(0);
    let error = retry_transient_read(|| async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(closed_pool())
    })
    .await
    .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), TRANSIENT_READ_ATTEMPTS);
    let mapped = map_read_err(error.context("seller note ext-out"));
    assert!(matches!(mapped, ChainError::Transport(_)), "{mapped}");
    for mapped in [
        map_err(closed_pool()),
        map_claim_submit_err(closed_pool()),
        map_sell_offer_submit_err(closed_pool()),
    ] {
        assert!(matches!(mapped, ChainError::Chain(_)), "{mapped}");
    }
}

pub(super) fn bk_error() -> anyhow::Error {
    anyhow!(
        "GraphQL errors: {}",
        json!([{"message": "failed to get account d446d4b942523bfb20509f4c8450c65dc6e21957f9d626ca7d34cebf2ce3a2ab: Invalid server response: All BK API endpoints failed; traceparent: 00-00000000000000000000000038f4ca33-0000000038f4ca33-01"}])
    )
}

#[tokio::test(start_paused = true)]
async fn issue_2206_bk_outage_survives_read_exhaustion_without_enabling_submit_retry() {
    let calls = AtomicUsize::new(0);
    let error = retry_transient_read(|| async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(bk_error())
    })
    .await
    .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), TRANSIENT_READ_ATTEMPTS);
    let mapped = map_read_err(error.context("TokenContract coherent snapshot"));
    assert!(matches!(mapped, ChainError::Transport(_)), "{mapped}");
    assert!(matches!(map_err(bk_error()), ChainError::Chain(_)));
    assert!(matches!(
        map_claim_submit_err(bk_error()),
        ChainError::Chain(_)
    ));
    assert!(matches!(
        map_sell_offer_submit_err(bk_error()),
        ChainError::Chain(_)
    ));

    calls.store(0, Ordering::SeqCst);
    let order_id = retry_seller_read("same resting SELL", || async {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(map_read_err(bk_error()))
        } else {
            Ok(257_u128)
        }
    })
    .await
    .unwrap();
    assert_eq!(order_id, 257);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn issue_2122_exhausting_read_retries_still_leaves_seller_recovery_possible() {
    let calls = AtomicUsize::new(0);
    let error = retry_transient_read(|| async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(bm_error())
    })
    .await
    .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), TRANSIENT_READ_ATTEMPTS);
    assert!(!crate::chain::client::is_transient_transport_failure(
        &error
    ));
    let mapped = map_read_err(error);
    assert!(matches!(mapped, ChainError::Transport(_)), "{mapped}");
    assert!(mapped.to_string().contains("Connection refused"));
    // A read-specific change must not turn an arbitrary write error into a retry.
    assert!(matches!(map_err(bm_error()), ChainError::Chain(_)));
}

#[tokio::test(start_paused = true)]
async fn issue_2122_seller_read_retry_recovers_but_contract_refusal_is_not_repeated() {
    let calls = AtomicUsize::new(0);
    let result = retry_seller_read("exact resting offer", || async {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(map_read_err(bm_error()))
        } else {
            Ok(49_u128)
        }
    })
    .await
    .unwrap();
    assert_eq!(result, 49);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    calls.store(0, Ordering::SeqCst);
    let error = retry_seller_read("exact resting offer", || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(map_read_err(anyhow!("on-chain revert: exit_code=151")))
    })
    .await
    .unwrap_err();
    assert!(matches!(error, ChainError::Contract(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
