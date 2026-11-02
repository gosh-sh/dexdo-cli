use super::*;
use dexdo_core::{
    DealChainState, Match, Note, OfferListing, SellOffer, Settlement, StreamSnapshot, TokenContract,
};
use opentelemetry_proto::tonic::{
    collector::metrics::v1::{
        metrics_service_server::{MetricsService, MetricsServiceServer},
        ExportMetricsServiceRequest, ExportMetricsServiceResponse,
    },
    common::v1::any_value,
    metrics::v1::{metric, number_data_point},
};
use tokio_stream::wrappers::TcpListenerStream;

struct DisputeChain {
    disputed: bool,
    release_ok: bool,
}

#[async_trait::async_trait]
impl ChainBackend for DisputeChain {
    async fn deal_state(&self, _: &TokenContract) -> Result<Option<DealChainState>, ChainError> {
        Ok(Some(DealChainState {
            funded: true,
            opened: true,
            probe_accepted: true,
            disputed: self.disputed,
            deposit: 0,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 1,
        }))
    }
    async fn release_dispute(&self, _: &TokenContract) -> Result<Settlement, ChainError> {
        if self.release_ok {
            Ok(Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            })
        } else {
            Err(ChainError::Transport("release is unconfirmed".into()))
        }
    }
    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
        unreachable!()
    }
    async fn post_offer(&self, _: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
        unreachable!()
    }
    async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
        unreachable!()
    }
    async fn read_match(&self, _: &TokenContract) -> Result<Match, ChainError> {
        unreachable!()
    }
    async fn open_stream(
        &self,
        _: &TokenContract,
        _: Vec<u8>,
        _: &dyn Note,
    ) -> Result<(), ChainError> {
        unreachable!()
    }
    async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
        unreachable!()
    }
    async fn claim_tokens(
        &self,
        _: &TokenContract,
        _: &dyn Note,
        _: u128,
    ) -> Result<(), ChainError> {
        unreachable!()
    }
    async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
        unreachable!()
    }
    async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
        unreachable!()
    }
}

struct Collector(tokio::sync::mpsc::UnboundedSender<ExportMetricsServiceRequest>);
#[tonic::async_trait]
impl MetricsService for Collector {
    async fn export(
        &self,
        request: tonic::Request<ExportMetricsServiceRequest>,
    ) -> Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
        let _ = self.0.send(request.into_inner());
        Ok(tonic::Response::new(ExportMetricsServiceResponse {
            partial_success: None,
        }))
    }
}

async fn exported_deals(
    received: &mut tokio::sync::mpsc::UnboundedReceiver<ExportMetricsServiceRequest>,
    model: &str,
    expected: i64,
) {
    let deadline = tokio::time::Instant::now() + dexdo_core::params::SELLER_METRICS_EXPORT_TIMEOUT;
    // Queue cleanup cannot discard an in-flight export. Wait for two consecutive matches;
    // late snapshots reset the streak without extending the overall deadline.
    while received.try_recv().is_ok() {}
    let mut consecutive = 0;
    loop {
        let snapshot = tokio::time::timeout_at(deadline, received.recv())
            .await
            .unwrap_or_else(|_| {
                panic!("model={model}: gauge did not converge to {expected} before export deadline")
            })
            .expect("OTLP collector closed before gauge convergence");
        let values: Vec<_> = snapshot
            .resource_metrics
            .iter()
            .flat_map(|r| &r.scope_metrics)
            .flat_map(|s| &s.metrics)
            .filter(|m| m.name == "dexdo.seller.deals.in_progress")
            .filter_map(|m| match &m.data {
                Some(metric::Data::Gauge(g)) => Some(g),
                _ => None,
            })
            .flat_map(|g| &g.data_points)
            .filter(|p| {
                p.attributes.iter().any(|a| {
                    a.key == "model"
                        && matches!(
                a.value.as_ref().and_then(|v| v.value.as_ref()),
                Some(any_value::Value::StringValue(actual)) if actual == model)
                })
            })
            .map(|p| p.value.as_ref().unwrap())
            .collect();
        if values == vec![&number_data_point::Value::AsInt(expected)] {
            consecutive += 1;
            if consecutive == 2 {
                break;
            }
        } else {
            consecutive = 0;
        }
    }
    println!("dispute exporter evidence: model={model} deals.in_progress={expected}");
}

fn deals_snapshot(model: &str, count: i64) -> ExportMetricsServiceRequest {
    use opentelemetry_proto::tonic::{
        common::v1::{AnyValue, KeyValue},
        metrics::v1::{Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics},
    };
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: "dexdo.seller.deals.in_progress".into(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            attributes: vec![KeyValue {
                                key: "model".into(),
                                value: Some(AnyValue {
                                    value: Some(any_value::Value::StringValue(model.into())),
                                }),
                            }],
                            value: Some(number_data_point::Value::AsInt(count)),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

#[tokio::test]
async fn exported_deals_ignores_late_snapshots_in_both_directions() {
    for (old, expected) in [(1, 0), (0, 1)] {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let waiting = exported_deals(&mut received, "delayed-model", expected);
        tokio::pin!(waiting);
        // Poll through queue cleanup before the collector delivers its in-flight old snapshot.
        assert!(futures::poll!(&mut waiting).is_pending());
        for count in [old, expected, old, expected] {
            sent.send(deals_snapshot("delayed-model", count)).unwrap();
            assert!(futures::poll!(&mut waiting).is_pending());
        }
        // The intervening old snapshot reset the streak: only these final two may finish it.
        sent.send(deals_snapshot("delayed-model", expected))
            .unwrap();
        waiting.await;
    }
}

#[tokio::test(start_paused = true)]
async fn exported_deals_wrong_snapshots_cannot_extend_the_deadline() {
    let timeout = dexdo_core::params::SELLER_METRICS_EXPORT_TIMEOUT;
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let started = tokio::time::Instant::now();
    let waiting =
        tokio::spawn(async move { exported_deals(&mut received, "wrong-model", 0).await });
    tokio::task::yield_now().await;
    for _ in 0..2 {
        sent.send(deals_snapshot("wrong-model", 1)).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "a late snapshot must not fail immediately"
        );
        tokio::time::advance(timeout / 2).await;
    }
    assert!(waiting
        .await
        .expect_err("wrong gauge must fail by the deadline")
        .is_panic());
    assert_eq!(
        started.elapsed(),
        timeout,
        "one deadline must bound all snapshots"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_dispute_release_clears_only_its_deal_in_both_paths() {
    // Metrics are process-global. Keep this provider out of unrelated CLI tests' seller lifecycles.
    const CHILD: &str = "DEXDO_DISPUTE_METRICS_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::seller_policy::metrics_tests::confirmed_dispute_release_clears_only_its_deal_in_both_paths",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        print!("{}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(MetricsServiceServer::new(Collector(sent)))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            }),
    );
    let settings = [
        ("OTEL_METRICS_EXPORTER", "otlp"),
        ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", endpoint.as_str()),
        ("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL", "grpc"),
        ("OTEL_METRIC_EXPORT_INTERVAL", "10"),
    ];
    let previous: Vec<_> = settings
        .iter()
        .map(|(k, v)| {
            let old = std::env::var_os(k);
            std::env::set_var(k, v);
            (*k, old)
        })
        .collect();
    let config = dexdo::telemetry::Config::from_env();
    for (key, old) in previous {
        match old {
            Some(old) => std::env::set_var(key, old),
            None => std::env::remove_var(key),
        }
    }
    let session = dexdo::telemetry::Session::start(config.unwrap())
        .unwrap()
        .unwrap();
    let mut policy = policy::SellerRuntimePolicy {
        after_deal_done: policy::SellerAfterDealDoneAction::Retire,
        buyer_no_show: policy::SellerBuyerNoShowAction::RetireGateway,
        dispute_against_me: policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        max_open_deals: 1,
    };
    for reason in ["pre-advance", "advance-error"] {
        let model = format!("dispute-metrics-{reason}");
        let tc = format!("tc-{reason}");
        let other = format!("tc-other-{reason}");
        dexdo::telemetry::register(&tc, &model);
        dexdo::telemetry::deal_in_progress(&tc, &model);
        exported_deals(&mut received, &model, 1).await;
        let failed = DisputeChain {
            disputed: true,
            release_ok: false,
        };
        assert!(apply_seller_dispute_policy(&failed, &tc, &policy, reason)
            .await
            .is_err());
        exported_deals(&mut received, &model, 1).await;
        let clean = DisputeChain {
            disputed: true,
            release_ok: true,
        };
        policy.dispute_against_me = policy::SellerDisputeAgainstMeAction::Hold;
        assert!(apply_seller_dispute_policy(&clean, &tc, &policy, reason)
            .await
            .is_err());
        exported_deals(&mut received, &model, 1).await;
        policy.dispute_against_me = policy::SellerDisputeAgainstMeAction::ReleaseIfClean;
        assert!(!apply_seller_dispute_policy(
            &DisputeChain {
                disputed: false,
                release_ok: true
            },
            &tc,
            &policy,
            reason
        )
        .await
        .unwrap());
        exported_deals(&mut received, &model, 1).await;
        assert!(apply_seller_dispute_policy(&clean, &tc, &policy, reason)
            .await
            .unwrap());
        exported_deals(&mut received, &model, 0).await;
        dexdo::telemetry::deal_in_progress(&other, &model);
        assert!(apply_seller_dispute_policy(&clean, &tc, &policy, reason)
            .await
            .unwrap());
        exported_deals(&mut received, &model, 1).await;
        dexdo::telemetry::deal_settled(&other);
        exported_deals(&mut received, &model, 0).await;
    }
    session.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
