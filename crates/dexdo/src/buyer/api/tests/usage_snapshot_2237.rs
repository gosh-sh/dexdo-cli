//! Reproducible local HTTP measurements, without a cache or request coalescing.
use super::*;
use crate::buyer::usage::UsageStore;
use dexdo_core::params::SHELL_UNIT;
use futures::StreamExt;
use serde_json::json;
use std::time::Instant;

#[derive(Clone, Default)]
struct Timings(Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<f64>>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Timings {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Fields<'a>(&'a Timings);
        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
                if ["sync_us", "db_wait_us", "read_us", "write_us"].contains(&field.name()) {
                    self.0
                         .0
                        .lock()
                        .unwrap()
                        .entry(field.name().into())
                        .or_default()
                        .push(value / 1000.0);
                }
            }
        }
        event.record(&mut Fields(self));
    }
}

impl Timings {
    fn install() -> Self {
        use tracing_subscriber::prelude::*;
        static TIMINGS: std::sync::OnceLock<Timings> = std::sync::OnceLock::new();
        TIMINGS
            .get_or_init(|| {
                let timings = Self::default();
                tracing_subscriber::registry().with(timings.clone()).init();
                timings
            })
            .clone()
    }
    fn take(&self) -> serde_json::Value {
        let fields = std::mem::take(&mut *self.0.lock().unwrap());
        fields
            .into_iter()
            .map(|(name, values)| (name, stats(values)))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}

fn history(count: usize) -> (tempfile::TempDir, Arc<UsageStore>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sqlite3");
    let store = UsageStore::open(&path, "shellnet").unwrap();
    store.open_deal_quota("history", 100, SHELL_UNIT).unwrap();
    drop(store);
    let mut connection = rusqlite::Connection::open(&path).unwrap();
    let transaction = connection.transaction().unwrap();
    let at = chrono::Utc::now().timestamp_millis() - 1000;
    transaction.execute(
        "WITH RECURSIVE ids(i) AS(VALUES(1) UNION ALL SELECT i+1 FROM ids WHERE i<?1)
         INSERT INTO requests(network,request_id,runtime_id,model,protocol,streamed,deal_id,started_at,finished_at,outcome)
         SELECT 'shellnet','history-'||i,'previous','dexdo-mock','openai',0,'history',?2,?2+1,'succeeded' FROM ids",
        rusqlite::params![i64::try_from(count).unwrap(), at]).unwrap();
    transaction.execute_batch(
        "INSERT INTO request_usage(network,request_id,phase,call_id,input_tokens,output_tokens,total_tokens,charged_tokens,accepted,cache_read_tokens,cache_write_tokens,accepted_at,deal_id)
         SELECT network,request_id,'answer','answer','10','20','30','30',1,'0','0',finished_at,deal_id FROM requests;
         DROP TABLE admission_models; DROP TABLE admission_deals; PRAGMA user_version=6;").unwrap();
    transaction.commit().unwrap();
    drop(connection);
    let store = Arc::new(UsageStore::open(&path, "shellnet").unwrap());
    assert_eq!(
        store.admission_snapshot("dexdo-mock").unwrap()["accepted"]["output_tokens"],
        (count * 20).to_string()
    );
    (dir, store)
}

fn stats(mut values: Vec<f64>) -> serde_json::Value {
    values.sort_by(f64::total_cmp);
    let percentile = |p: usize| values[(values.len() * p).div_ceil(100).saturating_sub(1)];
    json!({"n":values.len(),"p50_ms":percentile(50),"p95_ms":percentile(95),"p99_ms":percentile(99)})
}

async fn reads(addr: SocketAddr, path: &str, concurrency: usize) -> serde_json::Value {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let url = format!("http://{addr}{path}");
    let n = concurrency.max(10);
    let started = Instant::now();
    let results: Vec<_> = futures::stream::iter(0..n)
        .map(|_| {
            let client = client.clone();
            let url = url.clone();
            async move {
                let started = Instant::now();
                let response = async {
                    let response = client.get(url).send().await?;
                    let ok = response.status().is_success();
                    response.bytes().await?;
                    Ok::<_, reqwest::Error>(ok)
                }
                .await;
                (
                    started.elapsed().as_secs_f64() * 1000.0,
                    response.as_ref().is_ok_and(|ok| *ok),
                    response.as_ref().is_err_and(|error| error.is_timeout()),
                )
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    let elapsed = started.elapsed().as_secs_f64();
    let mut value = stats(results.iter().map(|result| result.0).collect());
    value["seconds"] = json!(elapsed);
    value["requests_per_second"] = json!(n as f64 / elapsed);
    value["errors"] = json!(results.iter().filter(|result| !result.1).count());
    value["timeouts_30s"] = json!(results.iter().filter(|result| result.2).count());
    value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "local HTTP performance matrix; run explicitly with --ignored --nocapture"]
async fn snapshot_2237_http_benchmark() {
    let timings = Timings::install();
    for count in [1000, 10_000, 100_000] {
        let (_dir, store) = history(count);
        let state = ApiState::lazy(
            Arc::new(Buyer::generate()),
            "dexdo-mock".into(),
            Arc::new(|| Box::pin(async { panic!("read must not purchase") })),
            dexdo_core::params::MATCH_OPEN_TIMEOUT,
        )
        .with_usage(store.clone());
        let (stop, rx) = tokio::sync::oneshot::channel();
        let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), state, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
        for concurrency in [1, 10, 50, 100] {
            for path in ["/v1/usage?days=1", "/v1/usage/admission"] {
                timings.take();
                let mut result = reads(addr, path, concurrency).await;
                result["timing_ms"] = timings.take();
                println!(
                    "2237_HTTP {}",
                    json!({"history":count,"concurrency":concurrency,"path":path,"result":result})
                );
                // Timed-out HTTP reads still finish their blocking query. Drain before the next cell.
                let store = store.clone();
                tokio::task::spawn_blocking(move || {
                    store
                        .query(&Default::default(), chrono::Utc::now())
                        .unwrap()
                })
                .await
                .unwrap();
            }
        }
        stop.send(()).unwrap();
        task.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "local mixed inference/admission HTTP measurements"]
async fn snapshot_2237_mixed_http_benchmark() {
    let timings = Timings::install();
    for readers in [0, 50] {
        let (_dir, store) = history(100_000);
        let harness = weekly_route_harness_with_upstream(0, false, SUB_WEEK_LEN.as_secs(), 1).await;
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let state = direct_state_with_deliveries(&harness, events).with_usage(store.clone());
        let (stop, rx) = tokio::sync::oneshot::channel();
        let (addr, task) = serve("127.0.0.1:0".parse().unwrap(), state, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let mut tasks = Vec::new();
        for _ in 0..readers {
            let running = running.clone();
            tasks.push(tokio::spawn(async move {
                let client = reqwest::Client::new();
                let mut count = 0;
                while running.load(Ordering::SeqCst) {
                    let response = client
                        .get(format!("http://{addr}/v1/usage/admission"))
                        .send()
                        .await
                        .unwrap();
                    assert!(response.status().is_success());
                    response.bytes().await.unwrap();
                    count += 1;
                }
                count
            }));
        }
        let client = reqwest::Client::new();
        timings.take();
        let mut durations = Vec::new();
        for _ in 0..20 {
            let start = Instant::now();
            let response=client.post(format!("http://{addr}/v1/chat/completions"))
                .json(&json!({"model":"dexdo-mock","messages":[{"role":"user","content":""}],"max_tokens":1,"stream":false}))
                .send().await.unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert!(status.is_success(), "{status}: {body}");
            durations.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        running.store(false, Ordering::SeqCst);
        let mut reads = 0;
        for task in tasks {
            reads += task.await.unwrap();
        }
        let snapshot = store.admission_snapshot("dexdo-mock").unwrap();
        assert_eq!(snapshot["accepted"]["output_tokens"], "2000020");
        println!(
            "2237_MIXED {}",
            json!({"history":100000,"readers":readers,"reads":reads,"inference":stats(durations),"timing_ms":timings.take()})
        );
        stop.send(()).unwrap();
        task.await.unwrap();
        harness.shutdown().await;
    }
}
