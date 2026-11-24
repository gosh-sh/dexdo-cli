//! Opt-in OTLP metrics of `dexdo seller` and `dexdo buyer`. No network I/O occurs on the inference
//! path.
use anyhow::{bail, Context, Result};
use dexdo_core::params::{
    SellerLivenessParams, BUYER_METRICS_SERVICE_NAME, SELLER_METRICS_DEFAULT_ENDPOINT,
    SELLER_METRICS_EXPORT_INTERVAL, SELLER_METRICS_EXPORT_TIMEOUT, SELLER_METRICS_FRESHNESS,
    SELLER_METRICS_SERVICE_NAME, SELLER_UPTIME_PROBE_INTERVAL,
};
use opentelemetry::{
    metrics::{Counter, Histogram, MeterProvider},
    KeyValue,
};
use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::{
    metrics::{PeriodicReader, SdkMeterProvider},
    runtime, Resource,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub mod buyer;

static METRICS: OnceLock<Arc<Metrics>> = OnceLock::new();
/// `dexdo.<role>.contract.call.errors`: the session's role and its counter.
static CONTRACT_CALL_ERRORS: OnceLock<(Role, Counter<u64>)> = OnceLock::new();
/// The model this seller process was started for: the `model` of a failed contract call that is
/// not a call to one of its deals' TokenContracts.
static SELLER_MODEL: OnceLock<String> = OnceLock::new();

/// Name the model this seller process sells, once it is fixed.
pub fn set_model(model: &str) {
    let _ = SELLER_MODEL.set(model.to_owned());
}

/// The account part of an address, lowercase: the same key whichever form the address came in.
fn account_key(address: &str) -> String {
    address
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Count one failed contract call. A call to a seller deal's TokenContract carries that deal's
/// model; any other call carries the model the process was started for.
fn contract_call_failed(call: &dexdo_core::chain::call_errors::FailedCall<'_>) {
    let Some((role, counter)) = CONTRACT_CALL_ERRORS.get() else {
        return;
    };
    let model = match role {
        Role::Buyer => buyer::model(),
        Role::Seller => METRICS
            .get()
            .and_then(|metrics| {
                lock(&metrics.observations)
                    .deal_models
                    .get(&account_key(call.account))
                    .cloned()
            })
            .or_else(|| SELLER_MODEL.get().cloned()),
    };
    let mut attributes: Vec<KeyValue> = model
        .map(|model| KeyValue::new("model", model))
        .into_iter()
        .collect();
    attributes.extend([
        KeyValue::new("error.type", call.error_type.as_str()),
        KeyValue::new("contract", call.contract),
        KeyValue::new("method", call.method.to_owned()),
    ]);
    counter.add(1, &attributes);
}
pub(crate) fn seller_claim_failed(
    token_contract: &str,
    attempted_tokens: u128,
    error: &dexdo_core::ChainError,
) {
    use dexdo_core::ChainError;
    let error_type = match error {
        ChainError::AmbiguousSubmit(_) | ChainError::ClaimReplayRecovery { .. } => "unconfirmed",
        ChainError::Transport(_) => "transport",
        ChainError::Contract(_) | ChainError::ClaimWindowRefused(_) | ChainError::Limit(_) => {
            "contract_refused"
        }
        ChainError::UnsafeClaimState(_) => "invalid_claim_state",
        _ => "claim_failed",
    };
    tracing::event!(
        name: "dexdo.seller.claim.failed", target: module_path!(), tracing::Level::WARN, {
        "error.type" = error_type,
        "dexdo.contract.method" = "claimTokens",
        token_contract = %dexdo_core::address::display_self_dapp(token_contract),
        attempted_tokens,
        %error,
        }, "claim failed; waiting for authoritative claim state"
    );
    if let Some(metrics) = METRICS.get() {
        let model = lock(&metrics.observations)
            .deal_models
            .get(&account_key(token_contract))
            .cloned()
            .or_else(|| SELLER_MODEL.get().cloned());
        let mut attributes = model.as_deref().map(attrs).unwrap_or_default();
        attributes.push(KeyValue::new("error.type", error_type));
        metrics.claim_errors.add(1, &attributes);
    }
}

pub(crate) fn grpc_status_name(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Ok => "OK",
        tonic::Code::Cancelled => "CANCELLED",
        tonic::Code::Unknown => "UNKNOWN",
        tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
        tonic::Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        tonic::Code::NotFound => "NOT_FOUND",
        tonic::Code::AlreadyExists => "ALREADY_EXISTS",
        tonic::Code::PermissionDenied => "PERMISSION_DENIED",
        tonic::Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        tonic::Code::FailedPrecondition => "FAILED_PRECONDITION",
        tonic::Code::Aborted => "ABORTED",
        tonic::Code::OutOfRange => "OUT_OF_RANGE",
        tonic::Code::Unimplemented => "UNIMPLEMENTED",
        tonic::Code::Internal => "INTERNAL",
        tonic::Code::Unavailable => "UNAVAILABLE",
        tonic::Code::DataLoss => "DATA_LOSS",
        tonic::Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

pub fn enabled() -> bool {
    METRICS.get().is_some()
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}
fn attrs(model: &str) -> Vec<KeyValue> {
    vec![KeyValue::new("model", model.to_owned())]
}

/// The command a session exports for: its meter, its `role` resource attribute and its default
/// `service.name`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Seller,
    Buyer,
}
impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Seller => "seller",
            Self::Buyer => "buyer",
        }
    }
    fn meter(self) -> &'static str {
        match self {
            Self::Seller => "dexdo.seller",
            Self::Buyer => "dexdo.buyer",
        }
    }
    fn service_name(self) -> &'static str {
        match self {
            Self::Seller => SELLER_METRICS_SERVICE_NAME,
            Self::Buyer => BUYER_METRICS_SERVICE_NAME,
        }
    }
    fn initialized(self) -> bool {
        match self {
            Self::Seller => METRICS.get().is_some(),
            Self::Buyer => buyer::METRICS.get().is_some(),
        }
    }
}
impl std::fmt::Display for Role {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Parsed without printing any supplied endpoint, headers or resource values in diagnostics.
#[derive(Clone)]
pub struct Config {
    role: Role,
    endpoint: String,
    interval: Duration,
    timeout: Duration,
    metadata: tonic::metadata::MetadataMap,
    resource: Vec<KeyValue>,
}
impl Config {
    /// The explicit mock-chain mode must not claim production chain provenance.
    pub fn with_mock_network(mut self) -> Self {
        self.resource
            .retain(|attribute| attribute.key.as_str() != "network");
        self.resource.push(KeyValue::new("network", "mock"));
        self
    }
    /// The seller's configuration.
    pub fn from_env() -> Result<Option<Self>> {
        Self::parse(|key| std::env::var(key).ok())
    }
    pub fn from_env_as(role: Role) -> Result<Option<Self>> {
        Self::parse_as(role, |key| std::env::var(key).ok())
    }
    fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        Self::parse_as(Role::Seller, get)
    }
    fn parse_as(role: Role, get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        match get("OTEL_METRICS_EXPORTER").as_deref().unwrap_or("none") {
            "none" | "off" | "" => return Ok(None),
            "otlp" => (),
            _ => bail!("{role} metrics: OTEL_METRICS_EXPORTER must be none or otlp"),
        }
        let signal = |suffix: &str| {
            get(&format!("OTEL_EXPORTER_OTLP_METRICS_{suffix}"))
                .or_else(|| get(&format!("OTEL_EXPORTER_OTLP_{suffix}")))
        };
        if signal("PROTOCOL").as_deref().unwrap_or("grpc") != "grpc" {
            bail!("{role} metrics: only grpc protocol is supported");
        }
        let endpoint = signal("ENDPOINT").unwrap_or_else(|| SELLER_METRICS_DEFAULT_ENDPOINT.into());
        let url = reqwest::Url::parse(&endpoint)
            .map_err(|_| anyhow::anyhow!("{role} metrics: invalid endpoint"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            bail!("{role} metrics: endpoint must be an http(s) origin without credentials, query or path");
        }
        let duration = |name: &str, value: Option<String>, default: Duration| -> Result<Duration> {
            match value {
                None => Ok(default),
                Some(value) => {
                    let millis: u64 = value.parse().ok().filter(|v| *v > 0).with_context(|| {
                        format!("{role} metrics: {name} must be positive milliseconds")
                    })?;
                    let duration = Duration::from_millis(millis);
                    if Instant::now().checked_add(duration).is_none() {
                        bail!("{role} metrics: {name} exceeds platform duration range");
                    }
                    Ok(duration)
                }
            }
        };
        let interval = duration(
            "OTEL_METRIC_EXPORT_INTERVAL",
            get("OTEL_METRIC_EXPORT_INTERVAL"),
            SELLER_METRICS_EXPORT_INTERVAL,
        )?;
        let timeout = duration(
            "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
            signal("TIMEOUT"),
            SELLER_METRICS_EXPORT_TIMEOUT,
        )?;
        let mut metadata = tonic::metadata::MetadataMap::new();
        if let Some(headers) = signal("HEADERS") {
            for item in headers.split(',').filter(|s| !s.trim().is_empty()) {
                let (name, value) = item
                    .split_once('=')
                    .with_context(|| format!("{role} metrics: invalid header syntax"))?;
                let name: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = name
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{role} metrics: invalid header name"))?;
                let value = decode(role, value.trim())?;
                let value = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{role} metrics: invalid header value"))?;
                metadata.insert(name, value);
            }
        }
        let mut resource = vec![];
        if let Some(values) = get("OTEL_RESOURCE_ATTRIBUTES") {
            for item in values.split(',').filter(|s| !s.trim().is_empty()) {
                let (key, value) = item.split_once('=').with_context(|| {
                    format!("{role} metrics: invalid resource attribute syntax")
                })?;
                if matches!(key.trim(), "role" | "network" | "service.name") {
                    continue;
                }
                resource.push(KeyValue::new(
                    key.trim().to_owned(),
                    decode(role, value.trim())?,
                ));
            }
        }
        resource.push(KeyValue::new(
            "service.name",
            get("OTEL_SERVICE_NAME").unwrap_or_else(|| role.service_name().into()),
        ));
        resource.push(KeyValue::new("role", role.name()));
        resource.push(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));
        resource.push(KeyValue::new(
            "network",
            dexdo_core::params::current_network().to_owned(),
        ));
        Ok(Some(Self {
            role,
            endpoint,
            interval,
            timeout,
            metadata,
            resource,
        }))
    }
}
fn decode(role: Role, value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .with_context(|| format!("{role} metrics: invalid percent encoding"))?;
            let hex = std::str::from_utf8(hex)
                .with_context(|| format!("{role} metrics: invalid percent encoding"))?;
            decoded.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| anyhow::anyhow!("{role} metrics: invalid percent encoding"))?,
            );
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| anyhow::anyhow!("{role} metrics: invalid UTF-8 encoding"))
}

#[derive(Default, Debug)]
struct ExportState {
    failures: std::sync::atomic::AtomicU64,
    failing: std::sync::atomic::AtomicBool,
}
#[derive(Debug)]
struct ObservedExporter {
    inner: opentelemetry_otlp::MetricExporter,
    state: Arc<ExportState>,
    role: Role,
}
#[async_trait::async_trait]
impl opentelemetry_sdk::metrics::exporter::PushMetricExporter for ObservedExporter {
    async fn export(
        &self,
        metrics: &mut opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> opentelemetry_sdk::metrics::MetricResult<()> {
        struct Attempt<'a> {
            state: &'a ExportState,
            role: Role,
            success: bool,
        }
        impl Drop for Attempt<'_> {
            fn drop(&mut self) {
                use std::sync::atomic::Ordering::Relaxed;
                let role = self.role;
                if self.success {
                    if self.state.failing.swap(false, Relaxed) {
                        eprintln!("{role} metrics: collector export recovered");
                    }
                } else {
                    self.state.failures.fetch_add(1, Relaxed);
                    if !self.state.failing.swap(true, Relaxed) {
                        eprintln!("{role} metrics: collector export failed; trading continues");
                    }
                }
            }
        }
        let mut attempt = Attempt {
            state: &self.state,
            role: self.role,
            success: false,
        };
        let result = self.inner.export(metrics).await;
        attempt.success = result.is_ok();
        result
    }
    async fn force_flush(&self) -> opentelemetry_sdk::metrics::MetricResult<()> {
        self.inner.force_flush().await
    }
    fn shutdown(&self) -> opentelemetry_sdk::metrics::MetricResult<()> {
        self.inner.shutdown()
    }
    fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
        self.inner.temporality()
    }
}

pub struct Session {
    provider: SdkMeterProvider,
    timeout: Duration,
    role: Role,
}
impl Session {
    pub fn start(config: Option<Config>) -> Result<Option<Self>> {
        let Some(config) = config else {
            return Ok(None);
        };
        if config.role.initialized() {
            bail!("{} metrics: provider is already initialized", config.role);
        }
        let session = Self::build(config)?;
        // One seller or buyer per process. Never replace a provider while request instruments use it.
        Ok(Some(session))
    }
    fn build(config: Config) -> Result<Self> {
        let role = config.role;
        let mut channel = tonic::transport::Endpoint::from_shared(config.endpoint.clone())
            .map_err(|_| anyhow::anyhow!("{role} metrics: invalid endpoint"))?
            .timeout(config.timeout);
        if config.endpoint.starts_with("https:") {
            channel = channel
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .map_err(|_| anyhow::anyhow!("{role} metrics: TLS configuration failed"))?;
        }
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_channel(channel.connect_lazy())
            .with_metadata(config.metadata)
            .with_timeout(config.timeout)
            .build()
            .map_err(|_| anyhow::anyhow!("{role} metrics: exporter configuration failed"))?;
        let export_state = Arc::new(ExportState::default());
        let exporter = ObservedExporter {
            inner: exporter,
            state: export_state.clone(),
            role,
        };
        let reader = PeriodicReader::builder(exporter, runtime::Tokio)
            .with_interval(config.interval)
            .with_timeout(config.timeout)
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(Resource::new(config.resource))
            .with_reader(reader)
            .build();
        match role {
            Role::Seller => {
                let meter = provider.meter(role.meter());
                meter
                    .u64_observable_counter("dexdo.seller.telemetry.export_failures")
                    .with_callback(move |observer| {
                        observer.observe(
                            export_state
                                .failures
                                .load(std::sync::atomic::Ordering::Relaxed),
                            &[],
                        )
                    })
                    .build();
                let _ = METRICS.set(Arc::new(Metrics::new(&provider)));
            }
            Role::Buyer => buyer::install(&provider.meter(role.meter())),
        }
        let counter = provider
            .meter(role.meter())
            .u64_counter(format!("{}.contract.call.errors", role.meter()))
            .build();
        if CONTRACT_CALL_ERRORS.set((role, counter)).is_ok() {
            dexdo_core::chain::call_errors::observe(contract_call_failed);
        }
        Ok(Self {
            provider,
            timeout: config.timeout,
            role,
        })
    }
    pub async fn shutdown(self) {
        let provider = self.provider;
        let role = self.role;
        // A detached OS thread avoids Tokio's unbounded wait for spawn_blocking tasks at
        // runtime teardown. The SDK owns its bounded reader channel and export timeout.
        let (sent, received) = tokio::sync::oneshot::channel();
        let _ = std::thread::Builder::new()
            .name(format!("{role}-metrics-shutdown"))
            .spawn(move || {
                let _ = sent.send(provider.shutdown().is_ok());
            });
        if !matches!(
            tokio::time::timeout(self.timeout, received).await,
            Ok(Ok(true))
        ) {
            // Do not print exporter responses: they may contain authentication metadata.
            eprintln!("{role} metrics: shutdown incomplete; collector unavailable or timed out");
        }
    }
}

#[derive(Clone)]
struct Health {
    model: String,
    component: &'static str,
    outcome: &'static str,
    checked: f64,
    success: f64,
    /// The last watched deal of this model ended: the value is final and does not age.
    settled: bool,
}
#[derive(Clone)]
struct Uptime {
    model: String,
    component: &'static str,
    outcome: &'static str,
    checked: f64,
    success: f64,
}
#[derive(Clone)]
struct Offer {
    model: String,
    observed: f64,
    deadline: u64,
    ticks: u128,
    listed: bool,
    known: bool,
    live: bool,
    recovering: Option<f64>,
    isolated: bool,
    /// Set by `terminal`: an authoritative end state, exempt from the freshness window.
    settled: bool,
}
#[derive(Default)]
struct Observations {
    health: HashMap<(String, &'static str), Health>,
    uptime: HashMap<(String, &'static str), Uptime>,
    active_requests: HashMap<(String, &'static str), u64>,
    offers: HashMap<String, Offer>,
    in_progress_deals: HashMap<String, String>,
    // A bounded per-configured-model fallback survives watcher retirement, without
    // retaining every historical TokenContract or poisoning active successor observations.
    summaries: HashMap<String, Offer>,
    health_summaries: HashMap<(String, &'static str), Health>,
    /// The model of every deal this process registered, by its TokenContract's account key.
    deal_models: HashMap<String, String>,
    /// SHELL on each note with an active deal, by (`note`, `pocket`). NaN is a failed read, or a
    /// note whose last active deal ended: an exported series cannot be withdrawn.
    note_balances: HashMap<(String, &'static str), f64>,
}
impl Observations {
    fn health_values(&self) -> Vec<&Health> {
        let mut result: Vec<_> = self.health.values().collect();
        result.extend(
            self.health_summaries
                .iter()
                .filter(|((model, component), _)| {
                    !self
                        .health
                        .values()
                        .any(|h| h.model == *model && h.component == *component)
                })
                .map(|(_, h)| h),
        );
        result
    }
    fn offer_values(&mut self, time: f64) -> Vec<&Offer> {
        let removed: Vec<_> = self
            .offers
            .iter()
            .filter(|(_, offer)| {
                offer.isolated
                    && !(offer.live && time < offer.deadline as f64)
                    && time - offer.observed > SELLER_METRICS_FRESHNESS.as_secs_f64()
            })
            .map(|(tc, _)| tc.clone())
            .collect();
        for tc in removed {
            if let Some(mut offer) = self.offers.remove(&tc) {
                offer.known = false;
                self.summaries.insert(offer.model.clone(), offer);
            }
        }
        let mut result: Vec<_> = self.offers.values().collect();
        result.extend(
            self.summaries
                .iter()
                .filter(|(model, _)| !self.offers.values().any(|o| o.model == **model))
                .map(|(_, o)| o),
        );
        result
    }
}
struct Metrics {
    claim_errors: Counter<u64>,
    checks: Counter<u64>,
    check_duration: Histogram<f64>,
    events: Counter<u64>,
    requests: Counter<u64>,
    attempts: Counter<u64>,
    responses: Counter<u64>,
    input_tokens: Counter<u64>,
    output_tokens: Counter<u64>,
    billable_ticks: Counter<f64>,
    duration: Histogram<f64>,
    ttft: Histogram<f64>,
    tpot: Histogram<f64>,
    observations: Arc<Mutex<Observations>>,
}
impl Metrics {
    fn new(provider: &SdkMeterProvider) -> Self {
        let meter = provider.meter("dexdo.seller");
        let observations = Arc::new(Mutex::new(Observations::default()));
        let state = observations.clone();
        meter
            .f64_observable_gauge("dexdo.seller.heartbeat")
            .with_unit("s")
            .with_callback(move |observer| observer.observe(now(), &[]))
            .build();
        let state_health = state.clone();
        meter
            .f64_observable_gauge("dexdo.seller.health.last_check")
            .with_unit("s")
            .with_callback(move |o| {
                let state = lock(&state_health);
                let mut values: HashMap<(&str, &str), f64> = HashMap::new();
                for h in state.health_values() {
                    let value = values.entry((&h.model, h.component)).or_insert(h.checked);
                    *value = value.min(h.checked);
                }
                for ((model, component), value) in values {
                    let mut a = attrs(model);
                    a.push(KeyValue::new("component", component.to_owned()));
                    o.observe(value, &a)
                }
            })
            .build();
        let state_health = state.clone();
        meter
            .f64_observable_gauge("dexdo.seller.health.last_success")
            .with_unit("s")
            .with_callback(move |o| {
                let state = lock(&state_health);
                let mut values: HashMap<(&str, &str), f64> = HashMap::new();
                for h in state.health_values() {
                    let value = values.entry((&h.model, h.component)).or_insert(h.success);
                    *value = value.min(h.success);
                }
                for ((model, component), value) in values {
                    let mut a = attrs(model);
                    a.push(KeyValue::new("component", component.to_owned()));
                    o.observe(value, &a)
                }
            })
            .build();
        let state_health = state.clone();
        meter
            .u64_observable_gauge("dexdo.seller.health.status")
            .with_callback(move |o| {
                // Aggregate all watched deals for a model/component. One failure must not be hidden by another success.
                let state = lock(&state_health);
                let mut aggregate: HashMap<(&str, &str), (bool, bool)> = HashMap::new();
                for h in state.health_values() {
                    let entry = aggregate
                        .entry((&h.model, h.component))
                        .or_insert((true, true));
                    entry.0 &=
                        h.settled || now() - h.checked <= SELLER_METRICS_FRESHNESS.as_secs_f64();
                    entry.1 &= h.outcome == "pass";
                }
                for ((model, component), (fresh, healthy)) in aggregate {
                    let mut a = attrs(model);
                    a.push(KeyValue::new("component", component.to_owned()));
                    let current = if !fresh {
                        "unknown"
                    } else if healthy {
                        "pass"
                    } else {
                        "fail"
                    };
                    for state in ["unknown", "pass", "fail"] {
                        let mut a = a.clone();
                        a.push(KeyValue::new("state", state));
                        o.observe(u64::from(current == state), &a)
                    }
                }
            })
            .build();
        let state_uptime = state.clone();
        meter
            .u64_observable_gauge("dexdo.seller.uptime.status")
            .with_callback(move |o| {
                let state = lock(&state_uptime);
                for uptime in state.uptime.values() {
                    let current =
                        uptime_status(uptime.component, uptime.outcome, uptime.checked, now());
                    let mut a = attrs(&uptime.model);
                    a.push(KeyValue::new("component", uptime.component.to_owned()));
                    for status in ["unknown", "pass", "fail", "timeout", "throttled"] {
                        let mut point_attrs = a.clone();
                        point_attrs.push(KeyValue::new("state", status));
                        o.observe(u64::from(current == status), &point_attrs);
                    }
                }
            })
            .build();
        for (name, success) in [
            ("dexdo.seller.uptime.last_check", false),
            ("dexdo.seller.uptime.last_success", true),
        ] {
            let state = state.clone();
            meter
                .f64_observable_gauge(name)
                .with_unit("s")
                .with_callback(move |o| {
                    for uptime in lock(&state).uptime.values() {
                        let mut a = attrs(&uptime.model);
                        a.push(KeyValue::new("component", uptime.component.to_owned()));
                        o.observe(
                            if success {
                                uptime.success
                            } else {
                                uptime.checked
                            },
                            &a,
                        );
                    }
                })
                .build();
        }
        for (name, kind) in [
            ("dexdo.seller.offer.executable", 0),
            ("dexdo.seller.offer.remaining_ticks", 1),
            ("dexdo.seller.offer.unknown", 2),
            ("dexdo.seller.offer.recovery_age", 3),
            ("dexdo.seller.offer.isolated", 4),
            ("dexdo.seller.offer.listed", 5),
        ] {
            let state = state.clone();
            meter
                .f64_observable_gauge(name)
                .with_unit(if kind == 3 { "s" } else { "1" })
                .with_callback(move |o| {
                    let mut state = lock(&state);
                    let time = now();
                    let offers = state.offer_values(time);
                    let mut aggregate: HashMap<&str, f64> = HashMap::new();
                    for offer in &offers {
                        let fresh = offer.settled
                            || time - offer.observed <= SELLER_METRICS_FRESHNESS.as_secs_f64();
                        let live =
                            offer.known && fresh && offer.live && time < (offer.deadline as f64);
                        let value = match kind {
                            0 => u8::from(live) as f64,
                            1 => {
                                if live {
                                    offer.ticks as f64
                                } else {
                                    0.
                                }
                            }
                            2 => u8::from(!offer.known || !fresh) as f64,
                            3 => offer.recovering.map(|v| time - v).unwrap_or_default(),
                            5 => u8::from(offer.known && fresh && offer.listed) as f64,
                            _ => u8::from(offer.isolated) as f64,
                        };
                        let total = aggregate.entry(&offer.model).or_default();
                        if kind == 3 || kind == 5 {
                            *total = total.max(value)
                        } else {
                            *total += value
                        }
                    }
                    for (model, value) in aggregate {
                        if (kind <= 1 || (kind == 5 && value == 0.))
                            && offers.iter().any(|offer| {
                                offer.model == model
                                    && (!offer.known
                                        || (!offer.settled
                                            && time - offer.observed
                                                > SELLER_METRICS_FRESHNESS.as_secs_f64()))
                            })
                        {
                            // The cumulative SDK retains an omitted last-value series. NaN is
                            // an explicit unknown sample, preventing stale success or fake zero.
                            o.observe(f64::NAN, &attrs(model));
                            continue;
                        }
                        o.observe(value, &attrs(model))
                    }
                })
                .build();
        }
        let active_requests = state.clone();
        meter
            .u64_observable_gauge("dexdo.seller.upstream.requests.active")
            .with_callback(move |observer| {
                for ((model, purpose), count) in &lock(&active_requests).active_requests {
                    let mut attributes = attrs(model);
                    attributes.push(KeyValue::new("purpose", *purpose));
                    observer.observe(*count, &attributes);
                }
            })
            .build();
        let state_deals = state.clone();
        meter
            .u64_observable_gauge("dexdo.seller.deals.in_progress")
            .with_callback(move |o| {
                let state = lock(&state_deals);
                let mut counts: HashMap<&str, u64> = HashMap::new();
                for offer in state.offers.values() {
                    counts.entry(&offer.model).or_default();
                }
                for offer in state.summaries.values() {
                    counts.entry(&offer.model).or_default();
                }
                for health in state.health_values() {
                    counts.entry(&health.model).or_default();
                }
                for model in state.in_progress_deals.values() {
                    let count = counts.entry(model).or_default();
                    *count = count.saturating_add(1);
                }
                for (model, count) in counts {
                    o.observe(count, &attrs(model));
                }
            })
            .build();
        let state_notes = state.clone();
        meter
            .f64_observable_gauge("dexdo.seller.note.balance")
            .with_unit("{SHELL}")
            .with_callback(move |o| {
                for ((note, pocket), shell) in &lock(&state_notes).note_balances {
                    o.observe(
                        *shell,
                        &[
                            KeyValue::new("note", note.clone()),
                            KeyValue::new("pocket", *pocket),
                        ],
                    );
                }
            })
            .build();
        Self {
            claim_errors: meter.u64_counter("dexdo.seller.claim.errors").build(),
            checks: meter.u64_counter("dexdo.seller.health.checks").build(),
            check_duration: meter
                .f64_histogram("dexdo.seller.health.duration")
                .with_unit("s")
                .build(),
            events: meter.u64_counter("dexdo.seller.offer.events").build(),
            requests: meter.u64_counter("dexdo.seller.upstream.requests").build(),
            attempts: meter.u64_counter("dexdo.seller.upstream.attempts").build(),
            responses: meter.u64_counter("dexdo.seller.upstream.responses").build(),
            input_tokens: meter
                .u64_counter("dexdo.seller.upstream.input_tokens")
                .build(),
            output_tokens: meter
                .u64_counter("dexdo.seller.upstream.output_tokens")
                .build(),
            billable_ticks: meter.f64_counter("dexdo.seller.billable_ticks").build(),
            duration: meter
                .f64_histogram("dexdo.seller.upstream.duration")
                .with_unit("s")
                .build(),
            ttft: meter
                .f64_histogram("dexdo.seller.upstream.time_to_first_token")
                .with_unit("s")
                .build(),
            tpot: meter
                .f64_histogram("dexdo.seller.upstream.time_per_output_token")
                .with_unit("s")
                .build(),
            observations,
        }
    }
}

pub fn register(tc: &str, model: &str) {
    if let Some(m) = METRICS.get() {
        let mut observations = lock(&m.observations);
        observations
            .deal_models
            .insert(account_key(tc), model.into());
        observations
            .offers
            .entry(tc.into())
            .or_insert_with(|| Offer {
                model: model.into(),
                observed: now(),
                deadline: 0,
                ticks: 0,
                listed: false,
                known: false,
                live: false,
                recovering: None,
                isolated: false,
                settled: false,
            });
        // A restored, already-running deal may not execute readiness before it starts serving.
        // Keep readiness series present and explicitly unknown until a real check records a
        // result; registering a deal is not itself a health check and must not increment checks.
        for component in ["gateway_task", "advertised_gateway"] {
            observations
                .health
                .entry((tc.into(), component))
                .or_insert_with(|| Health {
                    model: model.into(),
                    component,
                    outcome: "unknown",
                    checked: 0.,
                    success: 0.,
                    settled: false,
                });
        }
        for component in ["gateway", "upstream"] {
            observations
                .uptime
                .entry((model.into(), component))
                .or_insert_with(|| Uptime {
                    model: model.into(),
                    component,
                    outcome: "unknown",
                    checked: 0.,
                    success: 0.,
                });
        }
        for purpose in ["inference", "probe"] {
            observations
                .active_requests
                .entry((model.into(), purpose))
                .or_default();
        }
    }
}
fn active_request(model: &str, purpose: &'static str, started: bool) {
    if let Some(metrics) = METRICS.get() {
        let mut observations = lock(&metrics.observations);
        let count = observations
            .active_requests
            .entry((model.into(), purpose))
            .or_default();
        if started {
            *count = count.saturating_add(1);
        } else {
            *count = count.saturating_sub(1);
        }
    }
}
/// Run the inference probe used by the seller's per-model uptime task.
pub async fn probe_upstream(
    upstream: &crate::seller::UpstreamConfig,
    timeout: Duration,
) -> &'static str {
    match tokio::time::timeout(timeout, upstream.check_market_readiness()).await {
        Ok(Ok(())) => "pass",
        Ok(Err(error)) => upstream_uptime_outcome(&error),
        Err(_) => "timeout",
    }
}

pub(crate) async fn run_upstream_uptime(model: String, upstream: crate::seller::UpstreamConfig) {
    let timeout = SellerLivenessParams::canonical().health_check_timeout;
    let interval = SELLER_UPTIME_PROBE_INTERVAL;
    let delay = upstream_probe_delay(&model);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    loop {
        let outcome = probe_upstream(&upstream, timeout).await;
        uptime(&model, "upstream", outcome);
        tokio::time::sleep(interval).await;
    }
}

/// Classify an upstream readiness error for uptime metrics. A provider HTTP 429 proves
/// connectivity and is reported as throttled rather than unavailable.
fn upstream_uptime_outcome(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<tonic::Status>()
        .filter(|status| {
            dexdo_proto::is_retryable_upstream(status)
                && status.message().starts_with("upstream HTTP 429 ")
        })
        .map_or("fail", |_| "throttled")
}

fn uptime_status(component: &str, outcome: &'static str, checked: f64, time: f64) -> &'static str {
    let freshness = if component == "upstream" {
        SELLER_UPTIME_PROBE_INTERVAL + SellerLivenessParams::canonical().health_check_timeout
    } else {
        SELLER_METRICS_FRESHNESS
    };
    if checked == 0. || time - checked > freshness.as_secs_f64() {
        "unknown"
    } else {
        outcome
    }
}
fn upstream_probe_delay_at(checked: Option<f64>, time: f64) -> Duration {
    let interval = SELLER_UPTIME_PROBE_INTERVAL.as_secs_f64();
    let remaining = checked.map_or(0., |checked| {
        (interval - (time - checked)).clamp(0., interval)
    });
    Duration::from_secs_f64(remaining)
}
/// Record an independent process/provider uptime probe, separate from offer readiness decisions.
pub fn uptime(model: &str, component: &'static str, outcome: &'static str) {
    if let Some(m) = METRICS.get() {
        let mut state = lock(&m.observations);
        let key = (model.into(), component);
        let time = now();
        let success = if outcome == "pass" {
            time
        } else {
            state
                .uptime
                .get(&key)
                .map(|value| value.success)
                .unwrap_or_default()
        };
        state.uptime.insert(
            key,
            Uptime {
                model: model.into(),
                component,
                outcome,
                checked: time,
                success,
            },
        );
    }
}
fn upstream_probe_delay(model: &str) -> Duration {
    let checked = METRICS.get().and_then(|metrics| {
        lock(&metrics.observations)
            .uptime
            .get(&(model.to_owned(), "upstream"))
            .map(|uptime| uptime.checked)
    });
    upstream_probe_delay_at(checked, now())
}
/// Record a chain-confirmed matched deal as currently being served.
pub fn deal_in_progress(tc: &str, model: &str) {
    if let Some(m) = METRICS.get() {
        lock(&m.observations)
            .in_progress_deals
            .insert(tc.into(), model.into());
    }
}
/// Clear an in-progress deal only after an authoritative terminal settlement fact.
pub fn deal_settled(tc: &str) {
    if let Some(m) = METRICS.get() {
        lock(&m.observations).in_progress_deals.remove(tc);
    }
}
/// The TokenContracts of the deals this seller is working now: an offer it watches, or a matched
/// deal it serves until its terminal settlement.
pub fn active_deals() -> Vec<String> {
    let Some(m) = METRICS.get() else {
        return Vec::new();
    };
    let state = lock(&m.observations);
    let mut deals: Vec<String> = state
        .offers
        .keys()
        .chain(state.in_progress_deals.keys())
        .cloned()
        .collect();
    deals.sort();
    deals.dedup();
    deals
}
/// The balances of one note, by its number in the note pool. `None` is a pocket whose read failed.
pub struct NoteBalance {
    pub note: usize,
    pub trading_raw: Option<u128>,
    pub gas_raw: Option<u128>,
}
/// Replace the balances of the notes with active deals. A note missing from `balances` that was
/// written before reads NaN from now on.
pub fn note_balances(balances: &[NoteBalance]) {
    if let Some(m) = METRICS.get() {
        let mut state = lock(&m.observations);
        for shell in state.note_balances.values_mut() {
            *shell = f64::NAN;
        }
        let shell = |raw: Option<u128>| {
            raw.map_or(f64::NAN, |raw| {
                raw as f64 / dexdo_core::params::SHELL_UNIT as f64
            })
        };
        for balance in balances {
            let note = balance.note.to_string();
            state
                .note_balances
                .insert((note.clone(), "trading"), shell(balance.trading_raw));
            state
                .note_balances
                .insert((note, "gas"), shell(balance.gas_raw));
        }
    }
}
pub fn observe_offer(tc: &str, deadline: u64, ticks: u128, live: bool, listed: bool) {
    if let Some(m) = METRICS.get() {
        let model = lock(&m.observations)
            .offers
            .get(tc)
            .map(|o| o.model.clone());
        if let Some(model) = model {
            offer(tc, &model, deadline, ticks, live, listed)
        }
    }
}
/// Preserve an authoritative no-resting-offer result for a model until a later seller event
/// replaces it. This is used when a restored/funded deal proves its parent SELL was consumed;
/// unlike an unavailable read, that state is a known zero and must not age into a fabricated
/// unknown merely because the deal remains open for a long time.
pub fn confirmed_offer_absent(tc: &str, model: &str) {
    if let Some(m) = METRICS.get() {
        let mut state = lock(&m.observations);
        let time = now();
        state.offers.remove(tc);
        state.summaries.insert(
            model.into(),
            Offer {
                model: model.into(),
                observed: time,
                deadline: 0,
                ticks: 0,
                listed: false,
                known: true,
                live: false,
                recovering: None,
                isolated: false,
                settled: true,
            },
        );
    }
}
pub fn offer_event(tc: &str, outcome: &'static str) {
    if let Some(m) = METRICS.get() {
        let model = lock(&m.observations)
            .offers
            .get(tc)
            .map(|o| o.model.clone());
        if let Some(model) = model {
            event(tc, &model, outcome)
        }
    }
}
pub fn health(
    tc: &str,
    model: &str,
    component: &'static str,
    outcome: &'static str,
    elapsed: Option<Duration>,
) {
    if let Some(m) = METRICS.get() {
        let mut a = attrs(model);
        a.extend([
            KeyValue::new("component", component),
            KeyValue::new("outcome", outcome),
        ]);
        m.checks.add(1, &a);
        if let Some(elapsed) = elapsed {
            m.check_duration.record(elapsed.as_secs_f64(), &a);
        }
        let mut state = lock(&m.observations);
        let key = (tc.to_owned(), component);
        let time = now();
        let success = if outcome == "pass" {
            time
        } else {
            state
                .health
                .get(&key)
                .map(|h| h.success)
                .unwrap_or_default()
        };
        let summary = Health {
            model: model.into(),
            component,
            outcome,
            checked: time,
            success,
            settled: false,
        };
        state
            .health_summaries
            .insert((model.into(), component), summary);
        state.health.insert(
            key,
            Health {
                model: model.into(),
                component,
                outcome,
                checked: time,
                success,
                settled: false,
            },
        );
    }
}
pub fn offer(tc: &str, model: &str, deadline: u64, ticks: u128, live: bool, listed: bool) {
    if let Some(m) = METRICS.get() {
        let mut s = lock(&m.observations);
        let old = s.offers.remove(tc);
        s.offers.insert(
            tc.into(),
            Offer {
                model: model.into(),
                observed: now(),
                deadline,
                ticks,
                listed,
                known: true,
                live,
                recovering: old.and_then(|v| v.recovering).filter(|_| !live),
                isolated: false,
                settled: false,
            },
        );
    }
}
pub fn unknown(tc: &str) {
    if let Some(m) = METRICS.get() {
        if let Some(offer) = lock(&m.observations).offers.get_mut(tc) {
            offer.known = false;
            offer.observed = now()
        }
    }
}
pub fn event(tc: &str, model: &str, outcome: &'static str) {
    if let Some(m) = METRICS.get() {
        let mut a = attrs(model);
        a.push(KeyValue::new("outcome", outcome));
        m.events.add(1, &a);
        if let Some(o) = lock(&m.observations).offers.get_mut(tc) {
            if outcome == "recovering" {
                o.known = false;
                o.recovering.get_or_insert(now());
            }
            if outcome == "isolated" {
                o.isolated = true;
            }
        }
    }
}
/// Authoritative match/absence retires supervision without leaving stale paid-TC health.
pub fn terminal(tc: &str) {
    if let Some(m) = METRICS.get() {
        let mut state = lock(&m.observations);
        state.health.retain(|(key, _), _| key != tc);
        if let Some(mut offer) = state.offers.remove(tc) {
            offer.known = true;
            offer.live = false;
            offer.listed = false;
            offer.observed = now();
            offer.recovering = None;
            offer.isolated = false;
            offer.settled = true;
            // With no watched deal left for the model, its last health results are final too.
            let watched = state.health.values().any(|h| h.model == offer.model);
            if !watched {
                for health in state
                    .health_summaries
                    .values_mut()
                    .filter(|h| h.model == offer.model)
                {
                    health.settled = true;
                }
            }
            state.summaries.insert(offer.model.clone(), offer);
        }
    }
}
pub fn forget(tc: &str) {
    if let Some(m) = METRICS.get() {
        let mut state = lock(&m.observations);
        state.health.retain(|(key, _), _| key != tc);
        if let Some(offer) = state.offers.get_mut(tc) {
            offer.isolated = true;
        }
    }
}

/// Task-local request state never crosses into wire payloads or adds a channel.
struct Request {
    model: String,
    purpose: &'static str,
    backend: &'static str,
    started: Instant,
    first: bool,
    first_output_at: Option<Instant>,
    outcome: &'static str,
    error: &'static str,
}
tokio::task_local! {static REQUEST: std::cell::RefCell<Request>;}
pub async fn request<F: std::future::Future<Output = ()>>(
    model: &str,
    purpose: &'static str,
    backend: &'static str,
    future: F,
) {
    // Boxed before either branch: the upstream future is large, and holding it by value here (and
    // again inside the scope below) overflowed 2 MiB test stacks with metrics off as well as on.
    let future = Box::pin(future);
    if METRICS.get().is_none() {
        future.await;
        return;
    }
    REQUEST
        .scope(
            std::cell::RefCell::new(Request {
                model: model.into(),
                purpose,
                backend,
                started: Instant::now(),
                first: false,
                first_output_at: None,
                outcome: "cancelled",
                error: "none",
            }),
            async {
                active_request(model, purpose, true);
                struct Finish;
                impl Drop for Finish {
                    fn drop(&mut self) {
                        let _ = REQUEST.try_with(|r| {
                            let r = r.borrow();
                            if let Some(m) = METRICS.get() {
                                let mut a = attrs(&r.model);
                                a.extend([
                                    KeyValue::new("purpose", r.purpose),
                                    KeyValue::new("backend", r.backend),
                                    KeyValue::new("outcome", r.outcome),
                                    KeyValue::new("error_class", r.error),
                                ]);
                                m.requests.add(1, &a);
                                m.duration.record(r.started.elapsed().as_secs_f64(), &a);
                            }
                            active_request(&r.model, r.purpose, false);
                        });
                    }
                }
                let _finish = Finish;
                future.await
            },
        )
        .await;
}
pub fn attempt(status: Option<u16>) {
    let _ = REQUEST.try_with(|r| {
        let r = r.borrow();
        if let Some(m) = METRICS.get() {
            let mut a = attrs(&r.model);
            a.extend([
                KeyValue::new("purpose", r.purpose),
                KeyValue::new("backend", r.backend),
                KeyValue::new(
                    "status_class",
                    match status {
                        None => "started",
                        Some(200..=299) => "2xx",
                        Some(400..=499) => "4xx",
                        Some(500..=599) => "5xx",
                        _ => "other",
                    },
                ),
            ]);
            if let Some(status) = status {
                a.push(KeyValue::new("status_code", i64::from(status)));
                m.responses.add(1, &a)
            } else {
                m.attempts.add(1, &a)
            }
        }
    });
}
pub fn chunk() {
    let _ = REQUEST.try_with(|r| {
        let mut r = r.borrow_mut();
        if !r.first {
            r.first = true;
            r.first_output_at = Some(Instant::now());
            if let Some(m) = METRICS.get() {
                let mut a = attrs(&r.model);
                a.extend([
                    KeyValue::new("purpose", r.purpose),
                    KeyValue::new("backend", r.backend),
                ]);
                m.ttft.record(r.started.elapsed().as_secs_f64(), &a)
            }
        }
    });
}
pub fn usage(input_tokens: u64, output_tokens: u64) {
    let _ = REQUEST.try_with(|r| {
        let r = r.borrow();
        if r.purpose == "inference" {
            if let Some(m) = METRICS.get() {
                let mut a = attrs(&r.model);
                a.extend([
                    KeyValue::new("purpose", r.purpose),
                    KeyValue::new("backend", r.backend),
                ]);
                m.input_tokens.add(input_tokens, &a);
                m.output_tokens.add(output_tokens, &a);
            }
        }
    });
}
pub fn completed(output_tokens: u64) {
    let _ = REQUEST.try_with(|r| {
        let mut r = r.borrow_mut();
        r.outcome = "success";
        if output_tokens > 1 {
            if let (Some(first_output_at), Some(m)) = (r.first_output_at, METRICS.get()) {
                let mut a = attrs(&r.model);
                a.extend([
                    KeyValue::new("purpose", r.purpose),
                    KeyValue::new("backend", r.backend),
                ]);
                m.tpot.record(
                    first_output_at.elapsed().as_secs_f64() / (output_tokens - 1) as f64,
                    &a,
                );
            }
        }
    });
}
/// Observe only usage accepted by the gateway's authoritative billing recorder.
pub(crate) fn billed(model: &str, backend: &'static str, charged_tokens: u64) {
    if let Some(m) = METRICS.get() {
        let mut a = attrs(model);
        a.extend([
            KeyValue::new("purpose", "inference"),
            KeyValue::new("backend", backend),
        ]);
        m.billable_ticks
            .add(charged_tokens as f64 / dexdo_core::TICK_SIZE as f64, &a);
    }
}
pub fn failed(code: tonic::Code) {
    let _ = REQUEST.try_with(|r| {
        let mut r = r.borrow_mut();
        r.outcome = "error";
        r.error = match code {
            tonic::Code::DeadlineExceeded => "timeout",
            tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => "authentication",
            tonic::Code::ResourceExhausted => "rate_limit",
            tonic::Code::DataLoss => "stream_protocol",
            tonic::Code::Unavailable => "unavailable",
            tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => "configuration",
            _ => "other",
        };
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::{
        collector::metrics::v1::{
            metrics_service_server::{MetricsService, MetricsServiceServer},
            ExportMetricsServiceRequest, ExportMetricsServiceResponse,
        },
        metrics::v1::{metric, number_data_point},
    };
    use tokio_stream::wrappers::TcpListenerStream;
    fn config(values: &[(&str, &str)]) -> Result<Option<Config>> {
        Config::parse(|key| {
            values
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
    }
    #[test]
    fn uptime_probe_classifies_retryable_429_as_throttled_only() {
        let rate_limited = dexdo_proto::retryable_upstream_status(tonic::Status::unavailable(
            "upstream HTTP 429 Too Many Requests",
        ));
        assert_eq!(
            super::upstream_uptime_outcome(&anyhow::Error::new(rate_limited)),
            "throttled"
        );

        let other_retryable = dexdo_proto::retryable_upstream_status(tonic::Status::unavailable(
            "upstream HTTP 503 Service Unavailable",
        ));
        assert_eq!(
            super::upstream_uptime_outcome(&anyhow::Error::new(other_retryable)),
            "fail"
        );

        let unclassified_429 = tonic::Status::unavailable("upstream HTTP 429 Too Many Requests");
        assert_eq!(
            super::upstream_uptime_outcome(&anyhow::Error::new(unclassified_429)),
            "fail"
        );
    }

    #[test]
    fn upstream_probe_delay_uses_the_canonical_five_minute_interval() {
        assert_eq!(SELLER_UPTIME_PROBE_INTERVAL, Duration::from_secs(300));
        assert!(upstream_probe_delay_at(None, 100.).is_zero());
        assert_eq!(
            upstream_probe_delay_at(Some(100.), 399.),
            Duration::from_secs(1)
        );
        assert!(upstream_probe_delay_at(Some(100.), 400.).is_zero());
    }
    #[test]
    fn uptime_status_preserves_throttling_and_expires_stale_observations() {
        for outcome in ["pass", "fail", "timeout", "throttled", "unknown"] {
            assert_eq!(uptime_status("upstream", outcome, 100.0, 399.0), outcome);
        }
        assert_eq!(uptime_status("upstream", "pass", 0.0, 101.0), "unknown");
        assert_eq!(
            uptime_status(
                "gateway",
                "pass",
                100.0,
                100.0 + SELLER_METRICS_FRESHNESS.as_secs_f64() + 0.001
            ),
            "unknown"
        );
    }
    #[test]
    fn retired_deal_does_not_poison_fresh_successor_and_orphan_remains_unknown() {
        let mut state = Observations::default();
        let time = now();
        let old = Offer {
            model: "model".into(),
            observed: time - SELLER_METRICS_FRESHNESS.as_secs_f64() - 1.,
            deadline: 0,
            ticks: 0,
            listed: false,
            known: true,
            live: false,
            recovering: None,
            isolated: true,
            settled: false,
        };
        state.offers.insert("old".into(), old);
        assert!(!state.offer_values(time)[0].known);
        assert!(state.offers.is_empty());
        state.offers.insert(
            "new".into(),
            Offer {
                model: "model".into(),
                observed: time,
                deadline: u64::MAX,
                ticks: 98,
                listed: true,
                known: true,
                live: true,
                recovering: None,
                isolated: false,
                settled: false,
            },
        );
        let values = state.offer_values(time);
        assert_eq!(values.len(), 1);
        assert!(values[0].known);
        assert_eq!(values[0].ticks, 98);
    }
    #[test]
    fn disabled_exporter_ignores_collector_configuration() {
        assert!(config(&[]).unwrap().is_none());
        assert!(config(&[
            ("OTEL_METRICS_EXPORTER", "none"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "secret://CANARY")
        ])
        .unwrap()
        .is_none());
    }
    #[test]
    fn enabled_configuration_is_validated_and_redacted() {
        for value in [
            "http://name:CANARY@localhost:4317",
            "http://localhost:4317/?token=CANARY",
            "CANARY",
            "http://localhost:4317/CANARY",
        ] {
            let error = config(&[
                ("OTEL_METRICS_EXPORTER", "otlp"),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", value),
            ])
            .err()
            .unwrap()
            .to_string();
            assert!(!error.contains("CANARY"));
        }
        assert!(config(&[
            ("OTEL_METRICS_EXPORTER", "otlp"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
        ])
        .is_err());
        assert!(config(&[
            ("OTEL_METRICS_EXPORTER", "otlp"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "0")
        ])
        .is_err());
        let parsed = config(&[
            ("OTEL_METRICS_EXPORTER", "otlp"),
            (
                "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
                "authorization=Bearer%20CANARY",
            ),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed.metadata.get("authorization").unwrap(),
            "Bearer CANARY"
        );
    }
    #[derive(Clone)]
    struct Collector {
        received: tokio::sync::mpsc::Sender<ExportMetricsServiceRequest>,
    }
    #[tonic::async_trait]
    impl MetricsService for Collector {
        async fn export(
            &self,
            request: tonic::Request<ExportMetricsServiceRequest>,
        ) -> std::result::Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status>
        {
            assert_eq!(
                request.metadata().get("authorization").unwrap(),
                "Bearer CANARY"
            );
            self.received.send(request.into_inner()).await.unwrap();
            Ok(tonic::Response::new(ExportMetricsServiceResponse {
                partial_success: None,
            }))
        }
    }
    fn points(
        request: &ExportMetricsServiceRequest,
        name: &str,
    ) -> Vec<(Vec<opentelemetry_proto::tonic::common::v1::KeyValue>, f64)> {
        points_for(request, name, WIRE_MODEL)
    }
    fn points_for(
        request: &ExportMetricsServiceRequest,
        name: &str,
        model: &str,
    ) -> Vec<(Vec<opentelemetry_proto::tonic::common::v1::KeyValue>, f64)> {
        let mut values = vec![];
        for resource in &request.resource_metrics {
            for scope in &resource.scope_metrics {
                for metric in &scope.metrics {
                    if metric.name != name {
                        continue;
                    }
                    if let Some(metric::Data::Gauge(g)) = &metric.data {
                        for point in g.data_points.iter().filter(|p| has(p, "model", model)) {
                            let value = match point.value {
                                Some(number_data_point::Value::AsDouble(v)) => v,
                                Some(number_data_point::Value::AsInt(v)) => v as f64,
                                None => continue,
                            };
                            values.push((point.attributes.clone(), value))
                        }
                    }
                }
            }
        }
        values
    }
    fn gauge_sum_for(
        request: &ExportMetricsServiceRequest,
        name: &str,
        model: &str,
        key: &str,
        value: &str,
    ) -> f64 {
        points_for(request, name, model)
            .iter()
            .filter(|(attributes, _)| attributes.iter().any(|attribute| {
                attribute.key == key
                    && attribute.value.as_ref().and_then(|value| value.value.as_ref()).is_some_and(|actual| {
                        matches!(actual, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(actual) if actual == value)
                    })
            }))
            .map(|(_, value)| value)
            .sum()
    }
    async fn collect(
        provider: &SdkMeterProvider,
        received: &mut tokio::sync::mpsc::Receiver<ExportMetricsServiceRequest>,
    ) -> ExportMetricsServiceRequest {
        let provider = provider.clone();
        tokio::task::spawn_blocking(move || provider.force_flush())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(SELLER_METRICS_EXPORT_TIMEOUT, received.recv())
            .await
            .unwrap()
            .unwrap()
    }
    /// The provider is process-global, so other tests running alongside this one record into the
    /// same counters. Only points carrying this test's own `model` are summed.
    const WIRE_MODEL: &str = "telemetry-wire-test-model";
    fn has(
        point: &opentelemetry_proto::tonic::metrics::v1::NumberDataPoint,
        key: &str,
        value: &str,
    ) -> bool {
        point.attributes.iter().any(|a| {
            a.key == key
                && a.value.as_ref().and_then(|v| v.value.as_ref()).is_some_and(|v| {
                    matches!(v, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) if s == value)
                })
        })
    }
    fn sum(request: &ExportMetricsServiceRequest, name: &str, key: &str, value: &str) -> i64 {
        request
            .resource_metrics
            .iter()
            .flat_map(|r| &r.scope_metrics)
            .flat_map(|s| &s.metrics)
            .filter(|m| m.name == name)
            .filter_map(|m| match &m.data {
                Some(metric::Data::Sum(sum)) => Some(sum),
                _ => None,
            })
            .flat_map(|s| &s.data_points)
            .filter(|p| has(p, "model", WIRE_MODEL) && has(p, key, value))
            .map(|p| match p.value {
                Some(number_data_point::Value::AsInt(v)) => v,
                _ => 0,
            })
            .sum()
    }
    fn sum_f64(request: &ExportMetricsServiceRequest, name: &str, key: &str, value: &str) -> f64 {
        sum_f64_for(request, name, WIRE_MODEL, key, value)
    }
    fn sum_f64_for(
        request: &ExportMetricsServiceRequest,
        name: &str,
        model: &str,
        key: &str,
        value: &str,
    ) -> f64 {
        request
            .resource_metrics
            .iter()
            .flat_map(|r| &r.scope_metrics)
            .flat_map(|s| &s.metrics)
            .filter(|m| m.name == name)
            .filter_map(|m| match &m.data {
                Some(metric::Data::Sum(sum)) => Some(sum),
                _ => None,
            })
            .flat_map(|sum| &sum.data_points)
            .filter(|point| has(point, "model", model) && has(point, key, value))
            .map(|point| match point.value {
                Some(number_data_point::Value::AsDouble(value)) => value,
                Some(number_data_point::Value::AsInt(value)) => value as f64,
                None => 0.,
            })
            .sum()
    }
    async fn exercise_openai_adapter() {
        use crate::seller::upstream::{openai::OpenAiConfig, UpstreamConfig};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app=axum::Router::new().route("/chat/completions",axum::routing::post(move||{let calls=calls.clone();async move{
            let call=calls.fetch_add(1,Ordering::Relaxed);
            if call==0 {return (axum::http::StatusCode::BAD_GATEWAY,[("content-type","text/plain")],"temporary error".to_owned())}
            let mut body=String::from("data: {\"model\":\"telemetry-wire-test-model\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n");
            if call==1 {body.push_str("data: [DONE]\n\n")}
            (axum::http::StatusCode::OK,[("content-type","text/event-stream")],body)
        }}));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        std::env::set_var("DEXDO_METRICS_TEST_ONLY_KEY", "PRIVATE_PROVIDER_CANARY");
        let upstream = UpstreamConfig::OpenAi(OpenAiConfig {
            base_url: format!("http://{address}"),
            model: WIRE_MODEL.into(),
            frame_model: WIRE_MODEL.into(),
            api_key_env: "DEXDO_METRICS_TEST_ONLY_KEY".into(),
            ..Default::default()
        });
        let request = crate::seller::upstream::ProviderRequest {
            format: crate::api_format::ApiFormat::ChatCompletions,
            body: crate::api_format::RequestBody::parse(
                br#"{"model":"telemetry-wire-test-model","messages":[{"role":"user","content":"PRIVATE_PROMPT_CANARY"}],"stream":true}"#,
            )
            .unwrap(),
            headers: Vec::new(),
        };
        // A provider error answer is relayed, a complete stream bills, a truncated one fails.
        for (fails, billed) in [(false, false), (false, true), (true, false)] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(16);
            upstream.run(request.clone(), 0, tx).await;
            let mut failed = false;
            let mut usage = false;
            while let Some(event) = rx.recv().await {
                match event {
                    Err(_) => failed = true,
                    Ok(crate::seller::upstream::UpstreamEvent::Usage(_)) => usage = true,
                    _ => (),
                }
            }
            assert_eq!(failed, fails);
            assert_eq!(usage, billed);
        }
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        upstream.run(request, 0, tx).await;
        server.abort();
        let _ = server.await;
        std::env::remove_var("DEXDO_METRICS_TEST_ONLY_KEY");
    }
    async fn exercise_shared_uptime_cycle(
        provider: &SdkMeterProvider,
        received: &mut tokio::sync::mpsc::Receiver<ExportMetricsServiceRequest>,
    ) {
        use crate::seller::{self, liveness, OpenAiConfig, SellerConfig, UpstreamConfig};
        use dexdo_core::{ChainBackend, DobParams, MockChainBackend, ProtocolConsts};

        const MODEL: &str = "telemetry-wire-shared-upstream";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let (requests, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post({
                let release = release.clone();
                let index = index.clone();
                move || {
                    let release = release.clone();
                    let requests = requests.clone();
                    let index = index.clone();
                    async move {
                        use axum::{http::StatusCode, response::IntoResponse};
                        let call = index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        requests.send(call).unwrap();
                        match call {
                            0 => {
                                release.notified().await;
                                StatusCode::TOO_MANY_REQUESTS.into_response()
                            }
                            1 => ([("content-type", "text/event-stream")], "data: {}\n\n").into_response(),
                            2 => std::future::pending().await,
                            _ => ([
                                ("content-type", "text/event-stream"),
                            ], concat!(
                                "data: {\"model\":\"telemetry-wire-shared-upstream\",\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n",
                                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                                "data: [DONE]\n\n",
                            )).into_response(),
                        }
                    }
                }
            }),
        );
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let upstream = UpstreamConfig::OpenAi(OpenAiConfig {
            base_url: format!("http://{address}"),
            model: MODEL.into(),
            frame_model: MODEL.into(),
            api_key_env: "PATH".into(),
            ..Default::default()
        });
        let seller = Arc::new(
            seller::start_gateway_with("127.0.0.1:0".parse().unwrap(), upstream.clone())
                .await
                .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let chain = Arc::new(MockChainBackend::new(
            dir.path().join("book.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        ));
        let cfg = SellerConfig {
            token_contract: format!("0:{}", "6".repeat(64)),
            price_per_tick: dexdo_core::PRICE_STEP.try_into().unwrap(),
            max_ticks: 4,
            subscription: false,
            all_or_none: false,
            gateway_advertise: seller.listen_addr.to_string(),
            mock_token_count: 4,
        };
        seller::post_offer(&seller, chain.as_ref(), &cfg)
            .await
            .unwrap();
        let row = chain
            .raw_resting_sell_orders_for_tc(&cfg.token_contract)
            .await
            .unwrap()
            .remove(0);
        let order_id = row.order_id;
        let tc = cfg.token_contract.clone();
        let watch = seller::SellerMatchWatchConfig {
            cursor_path: dir.path().join("watch.json"),
            poll_interval: dexdo_core::params::DEFAULT_MATCH_POLL_INTERVAL,
        };
        let mut supervision = tokio::spawn({
            let seller = seller.clone();
            let chain = chain.clone();
            async move {
                let identity = liveness::RestingOfferIdentity {
                    owner_note: row.owner_note,
                    token_contract: cfg.token_contract.clone(),
                    order_id,
                };
                liveness::supervise_resting_offer(
                    &seller,
                    chain.as_ref(),
                    &cfg,
                    &watch,
                    &identity,
                    std::future::pending(),
                    false,
                    liveness::AdvertiseProbePolicy::Required,
                )
                .await
            }
        });

        // Only the uptime task runs on the clock we advance. The gateway, supervisor
        // and OTLP collector keep their real deadlines while this task sleeps.
        let probe_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let probe_handle = probe_runtime.handle().clone();
        let (clock, mut ticks) =
            tokio::sync::mpsc::channel::<(Duration, tokio::sync::oneshot::Sender<()>)>(1);
        let probe_driver = tokio::task::spawn_blocking(move || {
            probe_runtime.block_on(async {
                while let Some((elapsed, done)) = ticks.recv().await {
                    tokio::time::pause();
                    tokio::time::advance(elapsed).await;
                    tokio::time::resume();
                    let _ = done.send(());
                }
            });
        });
        let advance_probe_clock = async |elapsed| {
            let (done, advanced) = tokio::sync::oneshot::channel();
            clock.send((elapsed, done)).await.unwrap();
            advanced.await.unwrap();
        };
        let watch_uptime = || {
            let _probe_runtime = probe_handle.enter();
            seller.state.watch_model_uptime(MODEL, &upstream);
        };

        register(&tc, MODEL);
        watch_uptime();
        assert_eq!(
            tokio::time::timeout(SELLER_METRICS_EXPORT_TIMEOUT, observed.recv())
                .await
                .unwrap(),
            Some(0)
        );

        // The second pool group registers the same model while the first request is in flight.
        register("second-pool-group", MODEL);
        watch_uptime();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), observed.recv())
                .await
                .is_err()
        );
        release.notify_one();

        for (call, expected) in ["throttled", "fail", "timeout", "pass"]
            .into_iter()
            .enumerate()
        {
            if call > 0 {
                advance_probe_clock(SELLER_UPTIME_PROBE_INTERVAL).await;
                assert_eq!(
                    tokio::time::timeout(SELLER_METRICS_EXPORT_TIMEOUT, observed.recv())
                        .await
                        .unwrap(),
                    Some(call),
                );
            }
            if expected == "timeout" {
                advance_probe_clock(SellerLivenessParams::canonical().health_check_timeout).await;
            }
            tokio::time::timeout(SELLER_METRICS_EXPORT_TIMEOUT, async {
                loop {
                    let current = lock(&METRICS.get().unwrap().observations)
                        .uptime
                        .get(&(MODEL.into(), "upstream"))
                        .map(|value| value.outcome);
                    if current == Some(expected) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("upstream probe {call} did not record {expected}"));
            let export = collect(provider, received).await;
            assert_eq!(
                gauge_sum_for(
                    &export,
                    "dexdo.seller.uptime.status",
                    MODEL,
                    "state",
                    expected
                ),
                1.,
                "exported probe outcome {expected}",
            );
            let orders: Vec<_> = chain
                .raw_resting_sell_orders_for_tc(&tc)
                .await
                .unwrap()
                .iter()
                .map(|order| order.order_id)
                .collect();
            if orders != [order_id] || supervision.is_finished() {
                let outcome =
                    tokio::time::timeout(SELLER_METRICS_EXPORT_TIMEOUT, &mut supervision).await;
                panic!("probe {expected}: expected resting SELL {order_id}, got {orders:?}, supervisor outcome {outcome:?}");
            }
        }
        assert!(observed.try_recv().is_err());
        supervision.abort();
        seller.server_task.abort();
        http.abort();
        drop(clock);
        probe_driver.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_otlp_wire_recovers_from_errors_without_sensitive_or_unbounded_labels() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, mut received) = tokio::sync::mpsc::channel(16);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(MetricsServiceServer::new(Collector { received: sent }))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = stopped.await;
                }),
        );
        let endpoint = format!("http://{address}");
        let cfg = config(&[
            ("OTEL_METRICS_EXPORTER", "otlp"),
            ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", &endpoint),
            (
                "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
                "authorization=Bearer%20CANARY",
            ),
            ("OTEL_METRIC_EXPORT_INTERVAL", "60000"),
        ])
        .unwrap()
        .unwrap();
        let session = Session::start(Some(cfg)).unwrap().unwrap();
        let claim_tc = "test-2266";
        let claim_model = "telemetry-claim-retry";
        register(claim_tc, claim_model);
        crate::seller::advance::exercise_claim_retry_metrics(claim_tc).await;
        crate::seller::gateway::exercise_billing_metrics().await;
        let billing = collect(&session.provider, &mut received).await;
        for error_type in ["transport", "unconfirmed"] {
            assert_eq!(
                sum_f64_for(
                    &billing,
                    "dexdo.seller.claim.errors",
                    claim_model,
                    "error.type",
                    error_type
                ),
                1.0,
            );
        }
        for (model, charged, input, output) in [
            ("telemetry-billing-clamp", 2, 2, 1),
            ("telemetry-billing-shared", 2, 4, 2),
            ("telemetry-billing-rejected", 0, 2, 1),
            ("telemetry-billing-disconnected", 0, 2, 1),
        ] {
            let total = sum_f64_for(
                &billing,
                "dexdo.seller.billable_ticks",
                model,
                "purpose",
                "inference",
            );
            assert!(
                (total - charged as f64 / dexdo_core::TICK_SIZE as f64).abs() < 1e-12,
                "{model}: {total}"
            );
            assert_eq!(
                sum_f64_for(
                    &billing,
                    "dexdo.seller.upstream.input_tokens",
                    model,
                    "purpose",
                    "inference"
                ),
                input as f64,
                "{model}"
            );
            assert_eq!(
                sum_f64_for(
                    &billing,
                    "dexdo.seller.upstream.output_tokens",
                    model,
                    "purpose",
                    "inference"
                ),
                output as f64,
                "{model}"
            );
            println!("billing exporter evidence: model={model} charged_tokens={charged} billable_ticks={total} input_tokens={input} output_tokens={output}");
        }
        register("tc-SECRET", WIRE_MODEL);
        health(
            "tc-SECRET",
            WIRE_MODEL,
            "advertised_gateway",
            "pass",
            Some(Duration::ZERO),
        );
        observe_offer("tc-SECRET", u64::MAX, 98, true, true);
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(points(&request, "dexdo.seller.offer.executable")[0].1, 1.);
        let (started_first, wait_first) = tokio::sync::oneshot::channel();
        let (started_second, wait_second) = tokio::sync::oneshot::channel();
        let first = tokio::spawn(super::request(
            WIRE_MODEL,
            "inference",
            "mock",
            async move {
                let _ = started_first.send(());
                std::future::pending::<()>().await;
            },
        ));
        let second = tokio::spawn(super::request(
            WIRE_MODEL,
            "inference",
            "mock",
            async move {
                let _ = started_second.send(());
                std::future::pending::<()>().await;
            },
        ));
        wait_first.await.unwrap();
        wait_second.await.unwrap();
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(
            gauge_sum_for(
                &request,
                "dexdo.seller.upstream.requests.active",
                WIRE_MODEL,
                "purpose",
                "inference"
            ),
            2.
        );
        first.abort();
        second.abort();
        let _ = first.await;
        let _ = second.await;
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(
            gauge_sum_for(
                &request,
                "dexdo.seller.upstream.requests.active",
                WIRE_MODEL,
                "purpose",
                "inference"
            ),
            0.
        );
        const RESTORED: &str = "telemetry-wire-test-restored-model";
        // A restarted pool with a restored funded deal and no resting SELL has an authoritative
        // zero, not a missing series. The settled zero remains known for the open deal's lifetime.
        confirmed_offer_absent("tc-restored", RESTORED);
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.executable", RESTORED)[0].1,
            0.
        );
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.remaining_ticks", RESTORED)[0].1,
            0.
        );
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.unknown", RESTORED)[0].1,
            0.
        );
        {
            let mut state = lock(&METRICS.get().unwrap().observations);
            assert!(!state.offers.contains_key("tc-restored"));
            assert!(state.summaries.contains_key(RESTORED));
            for offer in state
                .summaries
                .values_mut()
                .filter(|offer| offer.model == RESTORED)
            {
                offer.observed -= SELLER_METRICS_FRESHNESS.as_secs_f64() + 1.;
            }
        }
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.executable", RESTORED)[0].1,
            0.
        );
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.remaining_ticks", RESTORED)[0].1,
            0.
        );
        for id in 0..100 {
            confirmed_offer_absent(&format!("tc-restored-{id}"), RESTORED);
        }
        {
            let state = lock(&METRICS.get().unwrap().observations);
            assert_eq!(
                state
                    .summaries
                    .values()
                    .filter(|offer| offer.model == RESTORED)
                    .count(),
                1
            );
            assert!(!state.offers.values().any(|offer| offer.model == RESTORED));
        }
        // A read failure means unknown, never authoritative absence.
        unknown("tc-SECRET");
        offer_event("tc-SECRET", "recovering");
        health(
            "tc-SECRET",
            WIRE_MODEL,
            "advertised_gateway",
            "timeout",
            Some(Duration::ZERO),
        );
        let request = collect(&session.provider, &mut received).await;
        assert!(points(&request, "dexdo.seller.offer.executable")[0]
            .1
            .is_nan());
        assert_eq!(points(&request, "dexdo.seller.offer.unknown")[0].1, 1.);
        let status = points(&request, "dexdo.seller.health.status");
        assert_eq!(status.len(), 6);
        for component in ["gateway_task", "advertised_gateway"] {
            let component_status: Vec<_> = status
                .iter()
                .filter(|(attributes, _)| {
                    attributes.iter().any(|attribute| {
                        attribute.key == "component"
                            && attribute.value.as_ref().and_then(|value| value.value.as_ref()).is_some_and(|value| {
                                matches!(value, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(value) if value == component)
                            })
                    })
                })
                .collect();
            assert_eq!(component_status.len(), 3, "{component}");
            assert_eq!(
                component_status.iter().map(|(_, value)| value).sum::<f64>(),
                1.,
                "{component}"
            );
        }
        // Repeated reconciliation preserves age; accepted authoritative successor resets it.
        offer_event("tc-SECRET", "recovering");
        observe_offer("tc-SECRET", u64::MAX, 98, true, true);
        health(
            "tc-SECRET",
            WIRE_MODEL,
            "advertised_gateway",
            "pass",
            Some(Duration::ZERO),
        );
        super::request(WIRE_MODEL, "inference", "mock", async {
            attempt(None);
            attempt(Some(502));
            attempt(Some(429));
            attempt(None);
            attempt(Some(200));
            chunk();
            usage(2, 2);
            completed(2);
            billed(WIRE_MODEL, "mock", 2);
        })
        .await;
        super::request(WIRE_MODEL, "inference", "mock", async {
            failed(tonic::Code::DataLoss)
        })
        .await;
        super::request(WIRE_MODEL, "inference", "mock", async {}).await; // drop/consumer STOP cannot become success
        super::request(WIRE_MODEL, "probe", "mock", async {
            usage(10, 10);
            completed(10);
        })
        .await;
        exercise_openai_adapter().await;
        let request = collect(&session.provider, &mut received).await;
        for name in [
            "dexdo.seller.upstream.input_tokens",
            "dexdo.seller.upstream.output_tokens",
            "dexdo.seller.billable_ticks",
        ] {
            assert_eq!(
                sum_f64(&request, name, "purpose", "probe"),
                0.,
                "probe traffic: {name}"
            );
        }
        assert_eq!(
            sum(
                &request,
                "dexdo.seller.upstream.requests",
                "backend",
                "openai"
            ),
            4
        );
        assert_eq!(
            sum(
                &request,
                "dexdo.seller.upstream.responses",
                "status_class",
                "5xx"
            ),
            2
        );
        assert_eq!(
            sum(
                &request,
                "dexdo.seller.upstream.input_tokens",
                "purpose",
                "inference"
            ),
            3
        );
        assert_eq!(
            sum(
                &request,
                "dexdo.seller.upstream.output_tokens",
                "purpose",
                "inference"
            ),
            3
        );
        assert_eq!(
            sum(
                &request,
                "dexdo.seller.upstream.responses",
                "status_class",
                "4xx"
            ),
            1
        );
        assert!(
            (sum_f64(
                &request,
                "dexdo.seller.billable_ticks",
                "purpose",
                "inference"
            ) - 2. / dexdo_core::TICK_SIZE as f64)
                .abs()
                < 1e-12
        );
        assert_eq!(
            sum(
                &request,
                "dexdo.seller.upstream.responses",
                "status_class",
                "2xx"
            ),
            3
        );
        assert_eq!(
            sum(
                &request,
                "dexdo.seller.upstream.requests",
                "error_class",
                "stream_protocol"
            ),
            2
        );
        assert!(request.resource_metrics.iter().flat_map(|r|&r.scope_metrics).flat_map(|s|&s.metrics).any(|m| m.name=="dexdo.seller.upstream.duration" && matches!(&m.data,Some(metric::Data::Histogram(h)) if h.data_points.iter().map(|p|p.count).sum::<u64>()>=3)));
        assert!(request.resource_metrics.iter().flat_map(|r|&r.scope_metrics).flat_map(|s|&s.metrics).any(|m| m.name=="dexdo.seller.upstream.time_per_output_token" && matches!(&m.data,Some(metric::Data::Histogram(h)) if h.data_points.iter().map(|p|p.count).sum::<u64>()>=1)));
        assert_eq!(points(&request, "dexdo.seller.offer.executable")[0].1, 1.);
        assert_eq!(points(&request, "dexdo.seller.offer.listed")[0].1, 1.);
        assert_eq!(points(&request, "dexdo.seller.offer.recovery_age")[0].1, 0.);
        let debug = format!("{request:?}");
        assert!(!debug.contains("CANARY"));
        assert!(!debug.contains("tc-SECRET"));
        for id in 0..1000 {
            register(&format!("tc-{id}"), WIRE_MODEL);
            observe_offer(&format!("tc-{id}"), u64::MAX, 1, true, true);
        }
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(points(&request, "dexdo.seller.offer.executable").len(), 1);
        assert_eq!(
            points(&request, "dexdo.seller.offer.executable")[0].1,
            1001.
        );
        // Disappearing local watcher doesn't claim an accepted ask was cancelled.
        forget("tc-SECRET");
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(
            points(&request, "dexdo.seller.offer.executable")[0].1,
            1001.
        );
        // Selling the last offer of a model is an authoritative end state. Its series keep the
        // final values past the freshness window; nothing failed, so nothing turns unknown.
        const SOLD: &str = "telemetry-wire-test-sold-model";
        register("tc-sold", SOLD);
        health(
            "tc-sold",
            SOLD,
            "advertised_gateway",
            "pass",
            Some(Duration::ZERO),
        );
        // A flag read has no duration: counted as a check, absent from the duration histogram.
        health("tc-sold", SOLD, "gateway_task", "pass", None);
        observe_offer("tc-sold", u64::MAX, 5, true, true);
        observe_offer("tc-sold", 0, 0, false, false);
        offer_event("tc-sold", "matched");
        terminal("tc-sold");
        // Normal pool match: terminal supervision precedes admission of the funded deal.
        // Admission is idempotent and must not revive an unknown offer/readiness result.
        deal_in_progress("tc-sold", SOLD);
        deal_in_progress("tc-sold", SOLD);
        let matched = collect(&session.provider, &mut received).await;
        assert_eq!(
            points_for(&matched, "dexdo.seller.deals.in_progress", SOLD)[0].1,
            1.
        );
        for name in ["executable", "remaining_ticks", "listed", "unknown"] {
            assert_eq!(
                points_for(&matched, &format!("dexdo.seller.offer.{name}"), SOLD)[0].1,
                0.,
                "matched offer must remain a confirmed zero: {name}"
            );
        }
        assert_eq!(
            gauge_sum_for(
                &matched,
                "dexdo.seller.health.status",
                SOLD,
                "state",
                "unknown"
            ),
            0.
        );
        deal_settled("tc-sold");
        let settled = collect(&session.provider, &mut received).await;
        assert_eq!(
            points_for(&settled, "dexdo.seller.deals.in_progress", SOLD)[0].1,
            0.
        );
        {
            let stale = SELLER_METRICS_FRESHNESS.as_secs_f64() + 1.;
            let mut state = lock(&METRICS.get().unwrap().observations);
            for offer in state.summaries.values_mut().filter(|o| o.model == SOLD) {
                offer.observed -= stale;
            }
            for health in state
                .health_summaries
                .values_mut()
                .filter(|h| h.model == SOLD)
            {
                health.checked -= stale;
            }
        }
        let request = collect(&session.provider, &mut received).await;
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.executable", SOLD)[0].1,
            0.
        );
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.remaining_ticks", SOLD)[0].1,
            0.
        );
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.listed", SOLD)[0].1,
            0.
        );
        assert_eq!(
            points_for(&request, "dexdo.seller.offer.unknown", SOLD)[0].1,
            0.
        );
        let state_of = |state: &str| {
            points_for(&request, "dexdo.seller.health.status", SOLD)
                .into_iter()
                .find(|(attributes, _)| {
                    attributes.iter().any(|a| {
                        a.key == "state"
                            && a.value.as_ref().and_then(|v| v.value.as_ref()).is_some_and(|v| {
                                matches!(v, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) if s == state)
                            })
                    })
                })
                .map(|(_, value)| value)
        };
        assert_eq!(state_of("unknown"), Some(0.));
        assert_eq!(state_of("pass"), Some(1.));
        // A normal match also leaves another live offer of the model numeric and executable.
        register("tc-successor", SOLD);
        observe_offer("tc-successor", u64::MAX, 7, true, true);
        deal_in_progress("tc-sold", SOLD);
        let successor = collect(&session.provider, &mut received).await;
        assert_eq!(
            points_for(&successor, "dexdo.seller.offer.executable", SOLD)[0].1,
            1.
        );
        assert_eq!(
            points_for(&successor, "dexdo.seller.offer.remaining_ticks", SOLD)[0].1,
            7.
        );
        deal_settled("tc-sold");

        let sold_gateway = |attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue]| {
            let is = |key: &str, value: &str| {
                attributes.iter().any(|a| {
                    a.key == key
                        && a.value.as_ref().and_then(|v| v.value.as_ref()).is_some_and(|v| {
                            matches!(v, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) if s == value)
                        })
                })
            };
            is("model", SOLD) && is("component", "gateway_task")
        };
        let metrics: Vec<_> = request
            .resource_metrics
            .iter()
            .flat_map(|r| &r.scope_metrics)
            .flat_map(|s| &s.metrics)
            .collect();
        assert!(metrics.iter().any(|m| m.name == "dexdo.seller.health.checks"
            && matches!(&m.data, Some(metric::Data::Sum(sum)) if sum.data_points.iter().any(|p| sold_gateway(&p.attributes)))));
        assert!(!metrics.iter().any(|m| m.name == "dexdo.seller.health.duration"
            && matches!(&m.data, Some(metric::Data::Histogram(h)) if h.data_points.iter().any(|p| sold_gateway(&p.attributes)))));
        exercise_shared_uptime_cycle(&session.provider, &mut received).await;
        session.shutdown().await;
        let _ = stop.send(());
        server.await.unwrap().unwrap();
        // A collector which disappeared is nonfatal and shutdown has a hard application deadline.
        let mut cfg = config(&[
            ("OTEL_METRICS_EXPORTER", "otlp"),
            ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", &endpoint),
        ])
        .unwrap()
        .unwrap();
        cfg.timeout = Duration::from_millis(50);
        let unavailable = Session::build(cfg).unwrap();
        let started = Instant::now();
        unavailable.shutdown().await;
        assert!(started.elapsed() < SELLER_METRICS_EXPORT_TIMEOUT);
    }
}
