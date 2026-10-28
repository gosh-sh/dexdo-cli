use super::*;

fn assert_refusal(body: &str, code: &str, action: &str, path: &str) {
    let body: serde_json::Value = serde_json::from_str(body).expect("JSON refusal");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert_eq!(body["error"]["recovery_action"], action, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .is_some_and(|m| !m.is_empty()));
    if path == "/v1/messages" {
        assert_eq!(body["type"], "error");
    }
}

#[tokio::test]
async fn budget_causes_reach_each_http_surface() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for stream in [false, true] {
            for (subscription, code, action) in [
                (false, "deal_budget_exhausted", "new_purchase"),
                (true, "subscription_budget_exhausted", "wait_for_next_cycle"),
            ] {
                let harness = weekly_route_harness(WEEK_QUOTA - 2, subscription).await;
                let (status, body) = harness.ask_full(path, 2, "", false).await;
                assert_eq!(status, reqwest::StatusCode::OK, "{body}");
                let (status, body) = harness.ask_full(path, 1, "", stream).await;
                assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
                assert_refusal(&body, code, action, path);
                if !subscription {
                    assert!(
                        !body.contains("renewal"),
                        "a fixed purchase never promises renewal"
                    );
                }
                assert_eq!(harness.delivered().await, 2);
                assert_eq!(harness.chain.foreign_money_calls(), 0);
                harness.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn verification_debt_is_not_reported_as_exhaustion() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for subscription in [false, true] {
            let harness = weekly_route_harness_gated(
                WEEK_QUOTA - 1,
                subscription,
                SUB_WEEK_LEN.as_secs(),
                UNCONSTRAINED_UPSTREAM,
                ContentGate::probe(
                    "dexdo-mock".to_string(),
                    probe_models(
                        "identity probe",
                        "https://reference.invalid/v1",
                        "DEXDO_FIXTURE_ABSENT_KEY",
                    ),
                ),
                crate::seller::UpstreamConfig::Mock,
            )
            .await;
            let (status, body) = harness.ask_full(path, 1, "", false).await;
            assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert_refusal(
                &body,
                "verification_budget_insufficient",
                "operator_action",
                path,
            );
            assert_eq!(harness.delivered().await, 0);
            assert_eq!(harness.remaining().await, 1);
            assert_eq!(harness.chain.foreign_money_calls(), 0);
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn completed_verification_can_consume_the_whole_request_grant() {
    const REFERENCE_KEY: &str = "DEXDO_FIXTURE_1450_REFERENCE_KEY";
    std::env::set_var(REFERENCE_KEY, "test-key");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let reference = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let harness = weekly_route_harness_gated(
            WEEK_QUOTA - u128::from(FULL_VERIFICATION_BILLABLE),
            false,
            SUB_WEEK_LEN.as_secs(),
            UNCONSTRAINED_UPSTREAM,
            ContentGate::probe(
                "dexdo-mock".into(),
                probe_models(
                    "identity probe",
                    &format!("http://{address}"),
                    REFERENCE_KEY,
                ),
            ),
            crate::seller::UpstreamConfig::Mock,
        )
        .await;
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_refusal(&body, "verification_grant_consumed", "retry", path);
        let deal = harness.deals.current().await.unwrap();
        assert_eq!(deal.content_gate.outstanding_verification_tokens(), 0);
        assert_eq!(harness.delivered().await, FULL_VERIFICATION_BILLABLE);
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_refusal(&body, "deal_budget_exhausted", "new_purchase", path);
        assert_eq!(harness.delivered().await, FULL_VERIFICATION_BILLABLE);
        harness.shutdown().await;
    }
    reference.abort();
    let _ = reference.await;
    std::env::remove_var(REFERENCE_KEY);
}

#[tokio::test]
async fn initialization_codes_survive_http_and_message_changes() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for (cause, code, action) in [
            (
                ConsumerErrorCode::OrderUnavailable,
                "order_unavailable",
                "retry",
            ),
            (
                ConsumerErrorCode::DealNotOpened,
                "deal_not_opened",
                "read_only_reconciliation",
            ),
            (
                ConsumerErrorCode::InvalidBuyTicks,
                "invalid_buy_ticks",
                "change_request",
            ),
        ] {
            let initializer: DealInitializer = Arc::new(move || {
                Box::pin(async move {
                    Err(DealInitError::with_code(
                        cause,
                        "changed diagnostic wording",
                    ))
                })
            });
            let state = ApiState::lazy(
                Arc::new(Buyer::generate()),
                "dexdo-mock".to_string(),
                initializer,
                MATCH_OPEN_TIMEOUT,
            );
            let (addr, task) = serve(
                "127.0.0.1:0".parse().unwrap(),
                state,
                std::future::pending(),
            )
            .await
            .unwrap();
            let body = if path == "/v1/responses" {
                serde_json::json!({"model":"dexdo-mock", "input":"hello", "max_output_tokens":1})
            } else {
                serde_json::json!({"model":"dexdo-mock", "messages":[{"role":"user","content":"hello"}], "max_tokens":1})
            };
            let response = reqwest::Client::new()
                .post(format!("http://{addr}{path}"))
                .json(&body)
                .send()
                .await
                .unwrap();
            let expected = if path != "/v1/messages" && cause == ConsumerErrorCode::InvalidBuyTicks
            {
                400
            } else {
                503
            };
            assert_eq!(response.status().as_u16(), expected);
            assert!(response.headers().contains_key(REQUEST_ID_HEADER));
            assert_refusal(&response.text().await.unwrap(), code, action, path);
            task.abort();
            let _ = task.await;
        }
    }
}

/// the background deal observer, not the request, reads the unopened state and closes
/// serving; every request after it is refused before any provider request.
#[tokio::test]
async fn funded_never_opened_is_typed_before_any_provider_request() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let harness = weekly_route_harness(0, false).await;
        harness.chain.snapshot.lock().unwrap().state.opened = false;
        observe_deal(&harness.deals).await;
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::GONE, "{body}");
        assert_refusal(&body, "deal_session_closed", "operator_action", path);
        let diagnostic: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(diagnostic["error"]["closure"]["category"], "deal_not_open");
        assert_eq!(diagnostic["error"]["recovery_state"], "not_attempted");
        assert_eq!(harness.delivered().await, 0);
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        harness.shutdown().await;
    }
}

#[tokio::test]
async fn ambiguous_purchase_keeps_its_reconciliation_anchor() {
    let error = DealInitError::with_reconciliation(
        "changed wording",
        BuyerSubmitReconciliation {
            submit_identity: "pending-submit".into(),
            recovery_anchor: BuyerSubmitRecoveryAnchor {
                order_id: 1,
                token_contract: "tc".into(),
            },
            state: BuyerSubmitReconciliationState::FreshUnresolved,
            origin: BuyerSubmitReconciliationOrigin::FreshSubmit,
        },
    );
    for anthropic in [false, true] {
        let response = error.response(anthropic);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "purchase_reconciliation_required");
        assert_eq!(body["error"]["recovery_action"], "read_only_reconciliation");
        assert_eq!(
            body["error"]["submit_reconciliation"]["recovery_anchor"]["order_id"],
            "1"
        );
    }
}

#[test]
fn backpressure_uses_metadata_instead_of_diagnostic_text() {
    let legacy = anyhow::Error::new(tonic::Status::resource_exhausted("deal_busy"));
    assert_eq!(
        capacity_rejection(&legacy).code,
        ConsumerErrorCode::DealCapacityUnavailable
    );
    let mut busy = tonic::Status::resource_exhausted("unrelated wording");
    busy.metadata_mut()
        .insert("dexdo-error-code", "deal_busy".parse().unwrap());
    assert_eq!(
        capacity_rejection(&anyhow::Error::new(busy)).code,
        ConsumerErrorCode::DealBusy
    );
}

#[tokio::test]
async fn busy_request_does_not_spend_or_close_the_shared_deal() {
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let harness = weekly_route_harness(0, false).await;
        harness
            .seller
            .set_concurrency(std::num::NonZeroU32::new(1).unwrap());
        // Holding the first frame also stalls a response larger than both production channels,
        // so the seller's slot stays occupied without depending on a sleep or elapsed-time check.
        let mut first = harness
            .send_full(path, 256, "DEXDO_FIXTURE_HOLD_SHARED_RESPONSE", true)
            .await;
        assert_eq!(first.status(), reqwest::StatusCode::OK);
        assert!(first.chunk().await.unwrap().is_some());
        let before = harness.remaining().await;
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_refusal(&body, "deal_busy", "retry_with_backoff", path);
        assert_eq!(harness.remaining().await, before);
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        assert!(!harness.deals.current().await.unwrap().session.is_closed());
        harness.seller.release_delayed_eof();
        let body = first.text().await.unwrap();
        assert!(!body.contains("\"finish_reason\":\"error\""), "{body}");
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "capacity is usable again: {body}"
        );
        harness.shutdown().await;
    }
}

#[tokio::test]
async fn renewal_guidance_requires_a_renewal_enabled_route() {
    let harness = weekly_route_harness(WEEK_QUOTA, false).await;
    let active = harness.deals.current().await.unwrap();
    let initializer: DealInitializer = Arc::new(|| {
        Box::pin(async { panic!("rendering a budget refusal must never initiate a BUY") })
    });
    for (mode, lazy_replacement, renewal_enabled) in [
        ("fixed", false, false),
        ("on-demand", true, true),
        ("proactive", false, true),
        ("replacement without renewal authorization", true, false),
    ] {
        let mut state = if lazy_replacement {
            ApiState::recoverable_lazy_with_active(
                harness.buyer.clone(),
                "dexdo-mock".into(),
                active.clone(),
                initializer.clone(),
                MATCH_OPEN_TIMEOUT,
            )
        } else {
            ApiState::single_deal(harness.buyer.clone(), "dexdo-mock".into(), active.clone())
        };
        state.renewal_enabled = renewal_enabled;
        for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
            let (status, body) = ask_router(state.clone(), path).await;
            assert_eq!(
                status,
                http::StatusCode::SERVICE_UNAVAILABLE,
                "{mode}: {body}"
            );
            let (code, action) = if renewal_enabled {
                ("renewal_pending", "retry_with_backoff")
            } else {
                ("deal_budget_exhausted", "new_purchase")
            };
            assert_refusal(&body, code, action, path);
        }
    }
    assert_eq!(harness.chain.foreign_money_calls(), 0);
    harness.shutdown().await;
}

async fn ask_router(state: ApiState, path: &str) -> (http::StatusCode, String) {
    use tower::ServiceExt;
    let body = if path == "/v1/responses" {
        serde_json::json!({"model":"dexdo-mock", "input":"hello", "max_output_tokens":1})
    } else {
        serde_json::json!({"model":"dexdo-mock", "messages":[{"role":"user","content":"hello"}], "max_tokens":1})
    };
    let response = router(state)
        .oneshot(
            http::Request::post(path)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn trial_ceiling_refusal_then_readmission(subscription: bool) {
    let trial = u64::try_from(dexdo_core::TICK_SIZE).unwrap();
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let harness = weekly_route_harness(0, subscription).await;
        let deal = harness.deals.current().await.unwrap();
        let token_contract = deal.route.token_contract.clone();
        let week = harness.chain.week_index();
        let mut request = deal.begin_request(unix_now_secs());
        let RouteBudget::Admitted(reservation) = deal.admit_with_probe_observation(false).await
        else {
            panic!("the canonical trial tick must be available");
        };
        assert_eq!(reservation.granted, trial);
        request.hold(reservation);
        request
            .record_billing(&deal, BillingUsage::new(0, trial, None).unwrap())
            .unwrap();
        drop(request);
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_refusal(
            &body,
            "deal_capacity_unavailable",
            "retry_after_reconciliation",
            path,
        );
        assert_eq!(deal.billable_tokens(), trial);
        assert!(!deal.session.is_closed());
        harness.chain.accepts_probe();
        observe_deal(&harness.deals).await;
        let (status, body) = harness.ask_full(path, 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(deal.billable_tokens() > trial);
        assert_eq!(
            harness.deals.current().await.unwrap().route.token_contract,
            token_contract
        );
        assert_eq!(harness.chain.week_index(), week);
        assert_eq!(harness.chain.bookings(), 0);
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        harness.shutdown().await;
    }
}

#[tokio::test]
async fn ordinary_trial_ceiling_waits_for_acceptance_without_new_purchase() {
    trial_ceiling_refusal_then_readmission(false).await;
}

#[tokio::test]
async fn weekly_trial_ceiling_waits_for_acceptance_without_new_week() {
    trial_ceiling_refusal_then_readmission(true).await;
}

#[tokio::test]
async fn minimum_weekly_quota_is_truly_exhausted_even_before_acceptance() {
    let trial = u64::try_from(dexdo_core::TICK_SIZE).unwrap();
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let harness = weekly_route_harness_before_probe(SUB_WEEK_LEN.as_secs()).await;
        let deal = harness.deals.current().await.unwrap();
        {
            let mut snapshot = harness.chain.snapshot.lock().unwrap();
            snapshot.subscription.tokens_per_week = dexdo_core::TICK_SIZE;
            snapshot.subscription.funded_tokens =
                dexdo_core::TICK_SIZE * u128::from(dexdo_core::SUBSCRIPTION_WEEKS);
        }
        deal.token_ceiling.store(trial, Ordering::SeqCst);
        let mut request = deal.begin_request(unix_now_secs());
        let RouteBudget::Admitted(reservation) = deal.admit_with_probe_observation(false).await
        else {
            panic!("the whole minimum weekly quota is available");
        };
        request.hold(reservation);
        request
            .record_billing(&deal, BillingUsage::new(0, trial, None).unwrap())
            .unwrap();
        drop(request);
        for accepted in [false, true] {
            if accepted {
                harness.chain.accepts_probe();
            }
            let (status, body) = harness.ask_full(path, 1, "", false).await;
            assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert_refusal(
                &body,
                "subscription_budget_exhausted",
                "wait_for_next_cycle",
                path,
            );
            assert_eq!(deal.billable_tokens(), trial);
            assert_eq!(harness.chain.week_index(), 0);
            assert_eq!(harness.chain.bookings(), 0);
            assert_eq!(harness.chain.foreign_money_calls(), 0);
        }
        harness.shutdown().await;
    }
}

#[tokio::test]
async fn terminal_and_unconfirmed_subscription_states_are_not_weekly_exhaustion() {
    for terminal in [false, true] {
        let harness = weekly_route_harness(WEEK_QUOTA, true).await;
        if terminal {
            harness.chain.disputes_while_booking();
        } else {
            harness.chain.chain_crosses_boundaries(1);
            harness.chain.stale_reads.store(1, Ordering::SeqCst);
        }
        let (status, body) = harness.ask_full("/v1/responses", 1, "", false).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let (code, action) = if terminal {
            ("subscription_terminal", "operator_action")
        } else {
            ("subscription_state_unavailable", "read_only_reconciliation")
        };
        assert_refusal(&body, code, action, "/v1/responses");
        assert_eq!(harness.chain.foreign_money_calls(), 0);
        harness.shutdown().await;
    }
}
