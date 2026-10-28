//! Gateway upstream. Adapters:
//! - [`mock`] -- mock model (`--mock-model`): deterministic fake output from the prompt, in the
//! format of the requested path;
//! - [`openai`] -- a real OpenAI-compatible provider: Chat Completions and Responses;
//! - [`anthropic`] -- the native Anthropic Messages API.

//! Every adapter relays the provider's status, headers and bytes as they are, followed by one
//! validated terminal billing record read from the provider's own usage ([`meter`]).

pub mod anthropic;
mod forward;
mod meter;
pub mod mock;
pub mod openai;

use crate::api_format::{ApiFormat, RequestBody};
use anyhow::{bail, Result};
use dexdo_core::params::{
    CAPABILITY_PROBE_PROMPT, UPSTREAM_HEALTH_CHANNEL_CAPACITY, UPSTREAM_HEALTH_PROBE_MAX_TOKENS,
    UPSTREAM_HEALTH_PROBE_PROMPT,
};
use dexdo_proto::{BillingUsage, Header, ResponseHead};
use tokio::sync::mpsc;
use tonic::Status;

/// Seller-internal upstream event.
pub enum UpstreamEvent {
    /// Provider HTTP status and the response headers that may reach the client.
    Head(ResponseHead),
    /// Provider response bytes; `output` says whether they carry answer text, reasoning or a tool call.
    Data { bytes: Vec<u8>, output: bool },
    /// One provider-native terminal input/output/total record for the preceding output.
    Usage(BillingUsage),
}

pub type UpstreamResult = Result<UpstreamEvent, Status>;

/// One consumer request as the provider receives it.
#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub format: ApiFormat,
    pub body: RequestBody,
    /// Client headers that may reach the provider ([`crate::api_format::forwards_request_header`]).
    pub headers: Vec<Header>,
}

/// Observe the existing send, without extra buffering or changing stream semantics. `Err` means the
/// receiver is gone; callers only test for it, so the undelivered event is not handed back.
pub(crate) async fn send_event(
    tx: &mpsc::Sender<UpstreamResult>,
    event: UpstreamResult,
) -> Result<(), ()> {
    if let Err(status) = &event {
        crate::telemetry::failed(status.code());
    }
    let content = matches!(&event, Ok(UpstreamEvent::Data { output: true, .. }));
    let terminal = matches!(&event, Ok(UpstreamEvent::Usage(_)));
    tx.send(event).await.map_err(|_| ())?;
    if content {
        crate::telemetry::chunk();
    }
    if terminal {
        crate::telemetry::completed();
    }
    Ok(())
}

/// The two canonical model-id flags this startup probe can prove from an upstream response.
/// Other flag slots deliberately never reach this type: their enforcement belongs to other
/// delivery artifacts, and precision is attested rather than enforced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StartupCapabilityRequirements {
    pub(super) tools: bool,
    pub(super) think: bool,
}

impl StartupCapabilityRequirements {
    fn from_market(market: &str) -> Option<Self> {
        let flags = dexdo_core::parse_canonical_model_id(market).ok()?.flags;
        let requirements = Self {
            tools: flags.tools,
            think: flags.think,
        };
        (requirements.tools || requirements.think).then_some(requirements)
    }

    pub(super) fn flags(self) -> &'static str {
        match (self.tools, self.think) {
            (true, true) => "`--tools` and `--think`",
            (true, false) => "`--tools`",
            (false, true) => "`--think`",
            (false, false) => "no capability flag",
        }
    }

    pub(super) fn asked_for(self) -> &'static str {
        match (self.tools, self.think) {
            (true, true) => {
                "a forced call to the supplied `dexdo_capability_probe` tool, reasoning content, and positive reasoning-token usage"
            }
            (true, false) => "a forced call to the supplied `dexdo_capability_probe` tool",
            (false, true) => "reasoning content and positive reasoning-token usage",
            (false, false) => "the ordinary health response",
        }
    }

    fn unsupported(self, adapter: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "startup capability probe for {} asked the upstream for {}; the configured {adapter} returned no supported capability-probe path, so the seller refuses to advertise this flagged market",
            self.flags(),
            self.asked_for()
        )
    }

    fn wrap_status(self, status: Status) -> Status {
        // Startup retries need the original typed throttle and HTTP status message.
        if (dexdo_proto::is_retryable_upstream(&status)
            && status.message().starts_with("upstream HTTP 429 "))
            || status
                .message()
                .starts_with("startup capability probe for ")
        {
            return status;
        }
        Status::new(
            status.code(),
            format!(
                "startup capability probe for {} asked the upstream for {}; the upstream returned an error: {}",
                self.flags(),
                self.asked_for(),
                status.message()
            ),
        )
    }
}

/// Gateway upstream choice (`--mock-model` vs the real adapter). Configured at seller startup
/// and **immutable** for the gateway's lifetime. The real branch carries base-url + model id;
/// the key is read from the environment at runtime (see [`openai`]) and is not stored here.
#[derive(Clone)]
pub enum UpstreamConfig {
    /// Mock model: deterministic fake output from the prompt.
    Mock,
    /// The same mock model, answering as the exact on-chain registry identity.
    MockWithClaimedModel(String),
    /// Instance scammer: a mock that UNCONDITIONALLY answers as another model -- a seller
    /// that client-side verification is obligated to catch. For the failover e2e.
    MockScammer,
    /// Real OpenAI-compatible upstream (Groq, etc.): API base + market model id.
    OpenAi(openai::OpenAiConfig),
    /// Native Anthropic Messages API upstream.
    Anthropic(anthropic::AnthropicConfig),
}

impl UpstreamConfig {
    /// Consumer API paths this upstream serves, as declared in the challenge.
    pub(crate) fn api_formats(&self) -> &'static [ApiFormat] {
        match self {
            Self::OpenAi(_) => &[ApiFormat::ChatCompletions, ApiFormat::Responses],
            Self::Anthropic(_) => &[ApiFormat::Messages],
            Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer => &[
                ApiFormat::ChatCompletions,
                ApiFormat::Responses,
                ApiFormat::Messages,
            ],
        }
    }

    /// The deal model a request must name, and the provider model id sent in its place.
    /// `None` = a mock without a market identity, which serves whatever model is named.
    pub(crate) fn request_model(&self) -> Option<(&str, &str)> {
        match self {
            Self::OpenAi(cfg) => Some((&cfg.frame_model, &cfg.model)),
            Self::Anthropic(cfg) => Some((&cfg.frame_model, &cfg.model)),
            Self::MockWithClaimedModel(model) => Some((model, model)),
            Self::Mock | Self::MockScammer => None,
        }
    }

    pub(crate) fn is_mock(&self) -> bool {
        matches!(
            self,
            Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer
        )
    }

    /// **Provider health.** Prove that the configured endpoint accepts the configured credentials and
    /// exact served model through the same adapter used for buyer traffic. The caller owns the timeout.

    /// This answers "is my provider reachable and working?" and nothing more. It has no business knowing
    /// which market it is being asked about -- that question is [`Self::check_market_readiness`].
    pub async fn check_health(&self) -> Result<()> {
        self.probe(None, None).await
    }

    /// **Market readiness.** Provider health AND: the model that actually answered is the model this
    /// market sells.

    /// Seller readiness asks a strictly larger question than provider health -- "may I sell on THIS
    /// market?" -- and this is the component that answers it. The refusal happens BEFORE `postSellOffer`,
    /// so a market whose provider answers as another model never gets an offer on the book.

    /// The market is the identity the per-deal upstream was built for: the seller CLI overrides the
    /// config's `frame_model` with `market.frame_model` when it builds this config
    /// (`cli::seller::seller_upstream`), so for a per-deal upstream that field IS the market being asked
    /// about. A mock upstream has no market identity to check and is health only.
    pub async fn check_market_readiness(&self) -> Result<()> {
        self.probe(self.market_model(), None).await
    }

    /// The one pre-SELL readiness probe. For a canonical market carrying `--tools` and/or
    /// `--think`, augment the existing authentication/model request and refuse unless the same
    /// response proves the declared capability. Runtime health checks intentionally keep calling
    /// [`Self::check_market_readiness`] so a seller already serving is never torn down by this
    /// startup-only verdict.
    pub async fn check_startup_market_readiness(&self) -> Result<()> {
        let market = self.startup_market_model();
        let requirements = market.and_then(StartupCapabilityRequirements::from_market);
        if let Some(requirements) = requirements {
            match self {
                Self::OpenAi(_) => {}
                Self::Anthropic(_) => {
                    return Err(requirements.unsupported("native Anthropic adapter"));
                }
                Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer => {
                    return Err(requirements.unsupported("fake-text mock upstream"));
                }
            }
        }
        self.probe(market, requirements).await
    }

    pub(crate) fn startup_capability_timeout_detail(&self) -> Option<String> {
        let requirements = self
            .startup_market_model()
            .and_then(StartupCapabilityRequirements::from_market)?;
        Some(format!(
            "startup capability probe for {} asked the upstream for {}; the upstream returned no response before the bounded startup timeout",
            requirements.flags(),
            requirements.asked_for()
        ))
    }

    /// The market identity this upstream was built to serve, when it has one.
    fn market_model(&self) -> Option<&str> {
        match self {
            Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer => None,
            Self::OpenAi(cfg) => Some(&cfg.frame_model),
            Self::Anthropic(cfg) => Some(&cfg.frame_model),
        }
    }

    pub(crate) fn startup_market_model(&self) -> Option<&str> {
        match self {
            Self::MockWithClaimedModel(model) => Some(model),
            _ => self.market_model(),
        }
    }

    async fn probe(
        &self,
        market: Option<&str>,
        requirements: Option<StartupCapabilityRequirements>,
    ) -> Result<()> {
        if self.is_mock() {
            return Ok(());
        }
        // two questions with two shapes. Plain readiness asks "can you deliver at all?" in a
        // bounded probe; a capability probe asks the provider to PROVE a declared `--tools`/`--think`
        // and is left unbounded, so a model that thinks before calling the tool is never cut off and
        // reported as incapable. The PROMPT follows whether a tool is actually OFFERED: a
        // `--think`-only market sends no tool and keeps the readiness prompt.
        let prompt = if requirements.is_some_and(|requirements| requirements.tools) {
            CAPABILITY_PROBE_PROMPT
        } else {
            UPSTREAM_HEALTH_PROBE_PROMPT
        };
        let max_tokens = requirements
            .is_none()
            .then_some(UPSTREAM_HEALTH_PROBE_MAX_TOKENS);
        let request = match self {
            Self::OpenAi(cfg) => openai::probe_request(cfg, prompt, max_tokens, requirements),
            Self::Anthropic(cfg) => anthropic::probe_request(cfg, prompt, max_tokens),
            Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer => {
                unreachable!("mock upstreams return above")
            }
        };
        let (tx, mut rx) = mpsc::channel(UPSTREAM_HEALTH_CHANNEL_CAPACITY);
        let (model, backend) = self.metric_labels();
        let run = crate::telemetry::request(model, "probe", backend, async {
            match self {
                Self::OpenAi(cfg) => openai::run_for_market(cfg, market, &request, tx).await,
                Self::Anthropic(cfg) => anthropic::run(cfg, &request, tx).await,
                Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer => {
                    unreachable!("mock upstreams return above")
                }
            }
        });
        tokio::pin!(run);
        let mut capability = requirements.map(openai::CapabilityObservation::new);
        let mut ran = false;

        loop {
            let item = if ran {
                rx.try_recv().ok()
            } else {
                tokio::select! {
                    item = rx.recv() => item,
                    _ = &mut run => {
                        ran = true;
                        continue;
                    }
                }
            };
            let Some(item) = item else {
                bail!("upstream readiness produced no authoritative model-token usage");
            };
            match item {
                Ok(UpstreamEvent::Data { bytes, .. }) => {
                    if let Some(observation) = capability.as_mut() {
                        if let Err(status) = observation.observe(&bytes) {
                            return Err(readiness_failure(status, requirements));
                        }
                    }
                }
                Ok(UpstreamEvent::Head(head)) if !(200..300).contains(&head.status) => {
                    let status = u16::try_from(head.status)
                        .ok()
                        .and_then(|status| reqwest::StatusCode::from_u16(status).ok())
                        .map_or_else(
                            || Status::unavailable(format!("upstream HTTP {}", head.status)),
                            forward::http_failure,
                        );
                    return Err(readiness_failure(status, requirements));
                }
                Ok(UpstreamEvent::Head(_)) => {}
                // visible content is not a complete provider billing record. Read the
                // stream through its validated terminal usage before declaring readiness.
                Ok(UpstreamEvent::Usage(usage)) if usage.output_tokens > 0 => {
                    if let Some(observation) = capability.as_ref() {
                        observation
                            .validate(usage.output_tokens)
                            .map_err(|status| readiness_failure(status, requirements))?;
                    }
                    return Ok(());
                }
                Ok(UpstreamEvent::Usage(_)) => {}
                Err(status) => return Err(readiness_failure(status, requirements)),
            }
        }
    }

    /// Relay one consumer request to the provider (R6): the response's status, headers and bytes
    /// travel into `tx` as they arrive, followed by its terminal usage. Finishes on upstream
    /// exhaustion or when the buyer disconnected (`tx` closed = STOP). `mock_token_count` bounds the
    /// mock model's output and is ignored by real providers.
    pub async fn run(
        &self,
        request: ProviderRequest,
        mock_token_count: u64,
        tx: mpsc::Sender<UpstreamResult>,
    ) {
        let (model, backend) = self.metric_labels();
        crate::telemetry::request(model, "inference", backend, async {
            match self {
                Self::Mock => mock::run(&request, mock_token_count, None, tx).await,
                Self::MockWithClaimedModel(claimed) => {
                    mock::run(&request, mock_token_count, Some(claimed), tx).await
                }
                Self::MockScammer => {
                    mock::run(
                        &request,
                        mock_token_count,
                        Some(mock::SUBSTITUTED_MODEL),
                        tx,
                    )
                    .await
                }
                Self::OpenAi(cfg) => openai::run_for_market(cfg, None, &request, tx).await,
                Self::Anthropic(cfg) => anthropic::run(cfg, &request, tx).await,
            }
        })
        .await
    }

    /// `model` and `backend` labels for this upstream's request metrics.
    fn metric_labels(&self) -> (&str, &'static str) {
        let backend = match self {
            Self::OpenAi(_) => "openai",
            Self::Anthropic(_) => "anthropic",
            _ => "mock",
        };
        (self.startup_market_model().unwrap_or("mock"), backend)
    }
}

/// the provider's message stays IN the context string, because the caller in
/// `seller::liveness` renders this with `error.to_string()` -- which shows only the outermost
/// context. The `Status` is ALSO attached as the typed cause, so a renderer that walks the chain
/// can still downcast to it.
fn readiness_failure(
    status: Status,
    requirements: Option<StartupCapabilityRequirements>,
) -> anyhow::Error {
    let status = match requirements {
        Some(requirements) => requirements.wrap_status(status),
        None => status,
    };
    let detail = format!(
        "upstream readiness failed ({:?}): {}",
        status.code(),
        status.message()
    );
    anyhow::Error::new(status).context(detail)
}

/// A request body for a seller-built probe.
fn probe_body(fields: serde_json::Value) -> RequestBody {
    match fields {
        serde_json::Value::Object(object) => RequestBody(object),
        _ => unreachable!("probe bodies are JSON objects"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller::models::{Capabilities, ModelConfig};

    #[test]
    fn seller_runtime_policy_is_owned_by_core_params() {
        use dexdo_core::params::{
            GATEWAY_CLIENT_CHANNEL_CAPACITY, GATEWAY_UPSTREAM_CHANNEL_CAPACITY,
            UPSTREAM_RESPONSE_BODY_MAX_BYTES, UPSTREAM_SSE_FRAME_MAX_BYTES,
            UPSTREAM_UNSTREAMED_RESPONSE_TIMEOUT,
        };

        assert_eq!(UPSTREAM_HEALTH_PROBE_PROMPT, "Reply with OK.");
        // the readiness budget is the canonical content-probe budget, pinned exactly.
        assert_eq!(
            UPSTREAM_HEALTH_PROBE_MAX_TOKENS,
            dexdo_core::params::CONTENT_PROBE_MAX_TOKENS as u32
        );
        assert_eq!(UPSTREAM_HEALTH_PROBE_MAX_TOKENS, 64);
        assert_eq!(UPSTREAM_HEALTH_CHANNEL_CAPACITY, 4);
        assert_eq!(GATEWAY_UPSTREAM_CHANNEL_CAPACITY, 16);
        assert_eq!(GATEWAY_CLIENT_CHANNEL_CAPACITY, 16);
        assert_eq!(
            UPSTREAM_SSE_FRAME_MAX_BYTES,
            UPSTREAM_RESPONSE_BODY_MAX_BYTES
        );
        assert_eq!(UPSTREAM_RESPONSE_BODY_MAX_BYTES, 32 * 1024 * 1024);
        assert_eq!(
            UPSTREAM_UNSTREAMED_RESPONSE_TIMEOUT,
            std::time::Duration::from_secs(180)
        );

        let production = |source: &'static str| {
            source
                .split_once("#[cfg(test)]\nmod tests")
                .map_or(source, |(production, _)| production)
        };
        let upstream = production(include_str!("mod.rs"));
        for name in [
            "UPSTREAM_HEALTH_PROBE_PROMPT",
            "UPSTREAM_HEALTH_PROBE_MAX_TOKENS",
            "UPSTREAM_HEALTH_CHANNEL_CAPACITY",
        ] {
            assert!(
                upstream.contains(name),
                "upstream must consume params::{name}"
            );
        }
        for literal in ["\"Reply with OK.\"", "mpsc::channel(4)"] {
            assert!(
                !upstream.contains(literal),
                "upstream production policy must not copy {literal}"
            );
        }
        let gateway = production(include_str!("../gateway.rs"));
        assert!(gateway.contains("GATEWAY_UPSTREAM_CHANNEL_CAPACITY"));
        assert!(gateway.contains("GATEWAY_CLIENT_CHANNEL_CAPACITY"));
        assert!(!gateway.contains("mpsc::channel(16)"));
        let forward = production(include_str!("forward.rs"));
        assert!(forward.contains("UPSTREAM_RESPONSE_BODY_MAX_BYTES"));
        assert!(forward.contains("UPSTREAM_UNSTREAMED_RESPONSE_TIMEOUT"));
        assert!(production(include_str!("../../api_format.rs"))
            .contains("UPSTREAM_SSE_FRAME_MAX_BYTES"));
    }

    fn model(base_url: &str, served_model: &str) -> ModelConfig {
        ModelConfig {
            frame_model: "qwen--qwen3--32b".to_string(),
            base_url: base_url.to_string(),
            served_model: served_model.to_string(),
            api_key_env: "PROVIDER_API_KEY".to_string(),
            tokenizer_family: "qwen".to_string(),
            price_per_tick: 1,
            capabilities: Capabilities::default(),
            identity_aliases: Vec::new(),
            vocab_size: None,
            fingerprints: Vec::new(),
        }
    }

    #[test]
    fn resolved_registry_name_changes_only_real_upstream_protocol_identity() {
        let exact = "Qwen/Qwen3-32B";

        let openai_model = model("https://provider.example/v1", "qwen/qwen3-32b");
        let openai = openai::OpenAiConfig::from_model(&openai_model, Some(exact));
        assert_eq!(openai.frame_model, exact);
        assert_eq!(openai.model, openai_model.served_model);
        assert_eq!(openai.base_url, openai_model.base_url);
        assert_eq!(openai.api_key_env, openai_model.api_key_env);
        assert_eq!(
            openai::OpenAiConfig::from_model(&openai_model, None).frame_model,
            openai_model.frame_model
        );

        let anthropic_model = model("https://api.anthropic.com", "claude-provider-model");
        let anthropic = anthropic::AnthropicConfig::from_model(&anthropic_model, Some(exact));
        assert_eq!(anthropic.frame_model, exact);
        assert_eq!(anthropic.model, anthropic_model.served_model);
        assert_eq!(anthropic.base_url, anthropic_model.base_url);
        assert_eq!(anthropic.api_key_env, anthropic_model.api_key_env);
        assert_eq!(
            anthropic::AnthropicConfig::from_model(&anthropic_model, None).frame_model,
            anthropic_model.frame_model
        );
    }

    async fn answering_model(upstream: UpstreamConfig) -> String {
        let request = ProviderRequest {
            format: ApiFormat::ChatCompletions,
            body: RequestBody::parse(br#"{"model":"requested","messages":[],"stream":true}"#)
                .unwrap(),
            headers: Vec::new(),
        };
        let (tx, mut rx) = mpsc::channel(16);
        upstream.run(request, 1, tx).await;
        while let Some(event) = rx.recv().await {
            if let UpstreamEvent::Data {
                bytes,
                output: true,
            } = event.unwrap()
            {
                let frame = String::from_utf8(bytes).unwrap();
                let crate::api_format::Frame::Event { facts, .. } =
                    crate::api_format::parse_frame(ApiFormat::ChatCompletions, &frame).unwrap()
                else {
                    panic!("an output frame is an event");
                };
                return facts.model.unwrap();
            }
        }
        panic!("mock must emit output");
    }

    #[tokio::test]
    async fn the_mock_answers_as_its_registry_identity_the_request_or_a_substitute() {
        assert_eq!(
            answering_model(UpstreamConfig::MockWithClaimedModel(
                "Qwen/Qwen3-32B".to_string()
            ))
            .await,
            "Qwen/Qwen3-32B"
        );
        assert_eq!(answering_model(UpstreamConfig::Mock).await, "requested");
        assert_eq!(
            answering_model(UpstreamConfig::MockScammer).await,
            mock::SUBSTITUTED_MODEL
        );
    }

    #[test]
    fn issue_2147_capability_context_preserves_the_startup_throttle_verdict() {
        for (tools, think) in [(true, false), (false, true), (true, true)] {
            let requirements = StartupCapabilityRequirements { tools, think };
            let throttle = dexdo_proto::retryable_upstream_status(Status::unavailable(
                "upstream HTTP 429 Too Many Requests",
            ));
            let annotated = requirements.wrap_status(throttle.clone());
            assert!(dexdo_proto::is_retryable_upstream(&annotated));
            assert_eq!(annotated.message(), throttle.message());
            assert_eq!(annotated.code(), throttle.code());

            let rejected = requirements.wrap_status(Status::unauthenticated("bad credentials"));
            assert_eq!(rejected.code(), tonic::Code::Unauthenticated);
            assert!(!dexdo_proto::is_retryable_upstream(&rejected));
            assert!(rejected.message().contains(requirements.flags()));
        }
    }

    /// A seller whose model reports no per-word probabilities passes its own start-up health check
    /// and is therefore allowed to put an offer on sale.

    /// E2E-UPS-01, `tests/e2e/test-specification.md`.

    /// Partial: the start half of the row only; the provider-contact, streaming and settlement
    /// halves are `a_model_without_logprobs_is_sellable_and_reaches_the_provider` in
    /// `crates/dexdo/src/seller/upstream/openai.rs`.
    #[tokio::test]
    async fn a_model_without_logprobs_reaches_readiness() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const KEY_ENV: &str = "DEXDO_861_NO_LOGPROBS_READINESS_KEY";
        // Content, then the provider's own completion count, then the terminator. No per-token
        // logprob records anywhere -- the shape of a model that does not support them.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":1,\"total_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        )
        .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = vec![0_u8; 8192];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        std::env::set_var(KEY_ENV, "fake-provider-secret");
        let upstream = UpstreamConfig::OpenAi(openai::OpenAiConfig {
            base_url: format!("http://{address}"),
            api_key_env: KEY_ENV.into(),
            ..openai::OpenAiConfig::default()
        });
        let readiness =
            tokio::time::timeout(std::time::Duration::from_secs(5), upstream.check_health())
                .await
                .expect("E2E-UPS-01B seller readiness did not complete");
        std::env::remove_var(KEY_ENV);
        provider.abort();

        if readiness.is_err() {
            panic!("E2E-UPS-01B no-logprobs model failed seller readiness");
        }
    }

    /// seller readiness reads the model the provider says actually answered and refuses when it is
    /// not the model this seller committed to serve -- driven through the real readiness entry point
    /// (`UpstreamConfig::check_health`, the `upstream_authentication_and_model` component), not through the
    /// parser in isolation.

    /// The accepted cases are the SAME model in the other spellings this system uses -- the canonical market
    /// id (`producer--model--version`) and the registry's display case -- so the refusal can only fire on a
    /// genuinely different model, never on a re-spelling.
    #[tokio::test]
    async fn readiness_refuses_a_provider_that_served_a_different_model() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // The seller committed to the provider slug `qwen/qwen3-32b`, sold as the canonical market id
        // `qwen--qwen3--32b`.
        const SERVED_MODEL: &str = "qwen/qwen3-32b";
        const FRAME_MODEL: &str = "qwen--qwen3--32b";
        let cases = [
            (SERVED_MODEL, true),
            // The same model in the canonical frame spelling and in the registry's display case.
            (FRAME_MODEL, true),
            ("Qwen/Qwen3-32B", true),
            // A genuinely different model, and a near-miss inside the same vendor.
            ("meta-llama/llama-3.3-70b-versatile", false),
            ("qwen/qwen3-235b", false),
        ];

        for (provider_model, expect_ready) in cases {
            let body = format!(
                "data: {{\"model\":\"{provider_model}\",\"choices\":[{{\"delta\":{{\"content\":\"OK\"}}}}]}}\n\n\
                 data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":0,\"completion_tokens\":1,\"total_tokens\":1}}}}\n\n\
                 data: [DONE]\n\n"
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let provider = tokio::spawn(async move {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = vec![0_u8; 8192];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });

            // `PATH` is always set and non-empty, so the adapter reaches the provider without this test
            // mutating process-global environment while other tests run.
            let upstream = UpstreamConfig::OpenAi(openai::OpenAiConfig {
                base_url: format!("http://{address}"),
                model: SERVED_MODEL.to_string(),
                frame_model: FRAME_MODEL.to_string(),
                api_key_env: "PATH".to_string(),
                ..openai::OpenAiConfig::default()
            });
            let readiness =
                tokio::time::timeout(std::time::Duration::from_secs(5), upstream.check_health())
                    .await
                    .expect(" seller readiness did not complete");
            provider.abort();

            assert_eq!(
                readiness.is_ok(),
                expect_ready,
                " provider answering as \"{provider_model}\" against offered \"{SERVED_MODEL}\" \
                 (market \"{FRAME_MODEL}\"): readiness said {:?}",
                readiness.err().map(|e| e.to_string())
            );
        }
    }

    /// E2E-ADV-02/L2, offline: an HONEST provider serving the model it was asked for must still not let an
    /// offer rest on a market that sells a DIFFERENT model.

    /// This is the case the guard could not see. [`OpenAiConfig::model`] is the slug the seller puts in the
    /// request and an OpenAI-compatible provider echoes it, so while that slug was in the accepted set the
    /// check was satisfied by construction on every honest provider -- it certified without being capable of
    /// firing. The two layers are pinned here together, because the distinction is the whole fix:
    /// [`UpstreamConfig::check_health`] answers "is my provider working?" and must PASS (the provider is
    /// healthy and served exactly what it was asked for), while [`UpstreamConfig::check_market_readiness`]
    /// answers "may I sell on this market?" and must REFUSE.

    /// The config is the live row's own shape: production overrides the config frame with the market id
    /// (`cli::seller::seller_upstream` -> [`OpenAiConfig::from_model`]), so a seller pointing `--model qwen`
    /// at a market provisioned for another identity gets exactly this.
    #[tokio::test]
    async fn an_honest_provider_may_not_rest_an_offer_on_a_market_selling_another_model() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const SERVED_MODEL: &str = "qwen/qwen3-32b";
        // (market id, may this offer rest?)
        let cases = [
            // The live row: a market claiming a foreign identity, served by a real qwen.
            ("adv--real-foreign--1785956361213153088", false),
            // The honest pairing, in each spelling this system uses for the SAME model -- none of these may
            // be refused, or the fix would take honest sellers off the market.
            ("qwen--qwen3--32b", true),
            ("qwen/qwen3-32b", true),
            ("Qwen/Qwen3-32B", true),
        ];

        for (market_model, may_rest) in cases {
            // The provider answers honestly: exactly the slug the seller asked it for.
            let body = format!(
                "data: {{\"model\":\"{SERVED_MODEL}\",\"choices\":[{{\"delta\":{{\"content\":\"OK\"}}}}]}}\n\n\
                 data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":0,\"completion_tokens\":1,\"total_tokens\":1}}}}\n\n\
                 data: [DONE]\n\n"
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            // Two probes are made against this config, so the provider answers twice.
            let provider = tokio::spawn(async move {
                for _ in 0..2 {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let mut request = vec![0_u8; 8192];
                    let _ = socket.read(&mut request).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });

            let upstream = UpstreamConfig::OpenAi(openai::OpenAiConfig {
                base_url: format!("http://{address}"),
                model: SERVED_MODEL.to_string(),
                // Production's own shape: the market id is what this upstream sells under.
                frame_model: market_model.to_string(),
                api_key_env: "PATH".to_string(),
                ..openai::OpenAiConfig::default()
            });

            let health =
                tokio::time::timeout(std::time::Duration::from_secs(5), upstream.check_health())
                    .await
                    .expect("provider-health probe did not complete");
            assert!(
                health.is_ok(),
                "the provider served exactly the model it was asked for, so it is healthy \
                 (market \"{market_model}\"): {:?}",
                health.err().map(|e| e.to_string())
            );

            let readiness = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                upstream.check_market_readiness(),
            )
            .await
            .expect("market-readiness probe did not complete");
            provider.abort();

            assert_eq!(
                readiness.is_ok(),
                may_rest,
                "a market selling \"{market_model}\" whose provider answers as \"{SERVED_MODEL}\": \
                 readiness said {:?}",
                readiness.err().map(|e| e.to_string())
            );
        }
    }
}
