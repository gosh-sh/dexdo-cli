//! Gateway upstream token source. Adapters:
//! - [`mock`] -- mock model (`--mock-model`): deterministic fake tokens from the prompt

//! - [`openai`] -- **real OpenAI-compatible upstream**: Groq,
//! streaming SSE -> normalization into `CanonChunk` (R1/R2/R5/R6).
//! - [`anthropic`] -- native Anthropic Messages API, streaming SSE -> the same canon.

//! All branches normalize output into canonical content chunks followed by one validated terminal
//! billing record. Real adapters use provider-native input + output usage; the deterministic mock
//! applies the same two-part accounting contract. Chunk framing is never itself money.

pub mod anthropic;
pub mod mock;
pub mod openai;

use anyhow::{bail, Result};
use dexdo_core::params::{
    SampleAlgorithm, CAPABILITY_PROBE_PROMPT, UPSTREAM_HEALTH_CHANNEL_CAPACITY,
    UPSTREAM_HEALTH_PROBE_MAX_TOKENS, UPSTREAM_HEALTH_PROBE_PROMPT,
};
use dexdo_proto::{BillingUsage, CanonChunk, CanonRequest, ChatMessage, SamplingParams};
use tokio::sync::mpsc;
use tonic::Status;

/// Seller-internal upstream event. Accounting is kept separate from the buyer-facing canon so
/// providers that report authoritative usage without token ids do not have to invent token data.
pub enum UpstreamEvent {
    Chunk(CanonChunk),
    /// One provider-native terminal input/output/total record for the preceding content.
    Usage(BillingUsage),
}

pub type UpstreamResult = Result<UpstreamEvent, Status>;

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
        if status
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

/// Resolve the model's own maximum output length -- the third
/// bound on the outbound generation limit next to the buyer's request and the deal budget.

/// **Fail closed**: an undeclared cap is UNKNOWN, not "unbounded". A deal budget is `ticks * TICK_SIZE`
/// tokens and a request without `max_tokens` used to become `u32::MAX`; both exceed every real provider's
/// output limit, so the provider answered `400` and no delivery ever succeeded. Refusing here happens
/// BEFORE the provider is contacted and names the concrete remediation.
#[allow(clippy::result_large_err)]
pub(crate) fn resolve_model_output_cap(
    declared: Option<u32>,
    frame_model: &str,
    served_model: &str,
) -> Result<u32, Status> {
    match declared {
        Some(cap) if cap > 0 => Ok(cap),
        _ => Err(Status::failed_precondition(format!(
            "model \"{frame_model}\" (served \"{served_model}\") has no known output cap: refusing to \
             send an unbounded max_tokens to the provider; set \"capabilities\": \
             {{ \"max_output_tokens\": <the provider's maximum completion length> }} for this model in \
             the models config (models.json)"
        ))),
    }
}

/// Is this provider HTTP status a **seller-side configuration** fault?

/// The seller constructs the whole upstream request: the market forces the model and the gateway bounds the
/// sampling params, so the buyer cannot shape it. A `4xx` rejection therefore means the seller's own model
/// config/request is wrong, not that the upstream is transiently down. `401/403` keep the dedicated `auth`
/// class and `408/429` are genuinely transient, so both are excluded.
pub(crate) fn is_seller_config_http_status(code: u16) -> bool {
    (400..500).contains(&code) && !matches!(code, 401 | 403 | 408 | 429)
}

/// Annotate a provider `4xx` that is a seller configuration fault with the concrete subject: which model was
/// served, which generation limit was sent, and which optional greedy field was present.
/// The `Status` code and the `upstream HTTP <code>` prefix are preserved verbatim -- stream-error policy and the
/// failure classifier both parse them -- so this only enriches the message the operator (and the relayed buyer
/// error body) actually reads.
pub(crate) fn annotate_seller_config_fault(
    status: Status,
    http_status: u16,
    served_model: &str,
    sent_max_tokens: u32,
    configured_output_cap: u32,
    sample_algorithm: Option<SampleAlgorithm>,
) -> Status {
    if !is_seller_config_http_status(http_status) {
        return status;
    }
    let sample_hint = sample_algorithm.map_or_else(String::new, |algorithm| {
        let field = algorithm
            .wire_field()
            .expect("NONE is never attached to a request");
        format!(
            "; request also sent optional field \"{field}\" selected by \
             capabilities.sample_algorithm={}; choose the provider's supported sample_algorithm \
             if it refuses that field",
            algorithm.profile_value()
        )
    });
    Status::new(
        status.code(),
        format!(
            "{} [seller configuration fault: model \"{served_model}\" sent max_tokens={sent_max_tokens} \
             at capabilities.max_output_tokens={configured_output_cap}; correct this model's \
             max_output_tokens in the models config{sample_hint}]",
            status.message()
        ),
    )
}

/// Gateway upstream choice (`--mock-model` vs the real adapter). Configured at seller startup
/// and **immutable** for the gateway's lifetime. The real branch carries base-url + model id;
/// the key is read from the environment at runtime (see [`openai`]) and is not stored here.
#[derive(Clone)]
pub enum UpstreamConfig {
    /// Mock model: deterministic fake tokens from the prompt.
    Mock,
    /// The same mock model, declaring the exact on-chain registry identity.
    MockWithClaimedModel(String),
    /// Instance scammer: a mock that UNCONDITIONALLY substitutes the model (claims one other than
    /// the frame's) -- a seller that client-side verification (B7) is obligated to catch. For the failover e2e.
    MockScammer,
    /// Real OpenAI-compatible upstream (Groq, etc.): API base + market model id.
    OpenAi(openai::OpenAiConfig),
    /// Native Anthropic Messages API upstream.
    Anthropic(anthropic::AnthropicConfig),
}

impl UpstreamConfig {
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

    fn startup_market_model(&self) -> Option<&str> {
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
        if matches!(
            self,
            Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer
        ) {
            return Ok(());
        }

        // two questions with two shapes. Plain readiness asks "can you deliver at all?"; a
        // capability probe asks the provider to PROVE a declared `--tools`/`--think`. Only the
        // branch carrying `StartupCapabilityRequirements` moves here -- the plain readiness request
        // below is byte-identical to what it has always sent, and
        // `issue_1227_plain_id_uses_the_unchanged_readiness_request_and_no_extra_probe` fails if
        // this leaks into it.

        // The PROMPT is chosen by whether a tool is actually OFFERED, not by whether this is a
        // capability probe. `build_request_with_startup_capabilities` builds `tools`/`tool_choice`
        // from `requirements.tools` alone, so a `--think`-only market sends a body with no tool in
        // it; asking THAT body to call `dexdo_capability_probe` would name a tool the request does
        // not carry -- the same self-contradiction, mirrored. A `--think`-only probe therefore keeps
        // the readiness prompt.

        // The BUDGET is the model's OWN declared output cap, not a number of ours. A capability
        // probe has to fit whatever the model does before it can answer -- under a forced
        // `tool_choice` the provider buffers the generation to parse the call, so a model cut off
        // mid-reasoning is reported as having refused. How long that takes is a property of the
        // model, it varies between samples of the SAME model, and there are more models and more
        // providers than we can measure: any constant we pick here is fitted to whatever we last
        // measured and wrong for the next one. `resolve_model_output_cap` already holds the model's
        // own statement about itself, so we use that and stop guessing.

        // This costs nothing. `max_tokens` is a CEILING, not a spend: a model that calls the tool
        // spends what it needs and stops, and the provider bills the tokens produced, not the
        // ceiling offered. Raising the ceiling buys headroom for free.

        // An unknown cap FAILS CLOSED here, naming the model and the fix, exactly as the serving
        // path does -- never a fallback number, which is the habit this change exists to break.
        let (prompt, max_tokens) = match requirements {
            Some(requirements) => {
                let declared = match self {
                    Self::OpenAi(cfg) => resolve_model_output_cap(
                        cfg.capabilities.max_output_tokens,
                        &cfg.frame_model,
                        &cfg.model,
                    ),
                    Self::Anthropic(cfg) => resolve_model_output_cap(
                        cfg.max_output_tokens,
                        &cfg.frame_model,
                        &cfg.model,
                    ),
                    Self::Mock | Self::MockWithClaimedModel(_) | Self::MockScammer => {
                        unreachable!("capability probes are refused for mock upstreams above")
                    }
                };
                let cap = match declared {
                    Ok(cap) => cap,
                    Err(status) => {
                        let status = requirements.wrap_status(status);
                        let detail = format!(
                            "upstream readiness failed ({:?}): {}",
                            status.code(),
                            status.message()
                        );
                        return Err(anyhow::Error::new(status).context(detail));
                    }
                };
                let prompt = if requirements.tools {
                    CAPABILITY_PROBE_PROMPT
                } else {
                    UPSTREAM_HEALTH_PROBE_PROMPT
                };
                (prompt, cap)
            }
            None => (
                UPSTREAM_HEALTH_PROBE_PROMPT,
                UPSTREAM_HEALTH_PROBE_MAX_TOKENS,
            ),
        };

        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            params: Some(SamplingParams {
                temperature: 0.0,
                max_tokens,
                stop: Vec::new(),
                // Readiness asks only whether the provider can answer. B7's provider-specific greedy
                // control is irrelevant here and may itself be rejected by an otherwise healthy endpoint.
                greedy: false,
            }),
        };
        let (tx, mut rx) = mpsc::channel(UPSTREAM_HEALTH_CHANNEL_CAPACITY);
        let run = self.run_for_market(
            market,
            requirements,
            u64::from(max_tokens),
            Some(request),
            tx,
        );
        tokio::pin!(run);

        loop {
            tokio::select! {
                item = rx.recv() => match item {
                    // visible content is not a complete provider billing record. Read the
                    // stream through its validated terminal usage before declaring readiness.
                    Some(Ok(UpstreamEvent::Chunk(_))) => continue,
                    Some(Ok(UpstreamEvent::Usage(usage))) if usage.output_tokens > 0 => return Ok(()),
                    Some(Ok(_)) => continue,
                    Some(Err(status)) => {
                        let status = match requirements {
                            Some(requirements) => requirements.wrap_status(status),
                            None => status,
                        };
                        // The status CARRIES the remediation -- the missing output cap, the
                        // absent key, the substituted model -- and printing the code alone
                        // threw it away: the operator saw `FailedPrecondition` and nothing to
                        // act on, with the seller refusing to post and no way to tell why.

                        // the message stays IN the context string, because the caller in
                        // `seller::liveness` renders this with `error.to_string()` -- which shows
                        // only the outermost context -- and stores it in a `HealthFailure` with no
                        // source. The `Status` is ALSO attached as the typed cause, so a renderer
                        // that walks the chain can still downcast to it.
                        let detail = format!(
                            "upstream readiness failed ({:?}): {}",
                            status.code(),
                            status.message()
                        );
                        return Err(anyhow::Error::new(status).context(detail));
                    }
                    None => bail!("upstream readiness produced no authoritative model-token usage"),
                },
                _ = &mut run => {
                    while let Ok(item) = rx.try_recv() {
                        match item {
                            Ok(UpstreamEvent::Chunk(_)) => {}
                            Ok(UpstreamEvent::Usage(usage)) if usage.output_tokens > 0 => return Ok(()),
                            Ok(_) => {}
                            Err(status) => {
                                let status = match requirements {
                                    Some(requirements) => requirements.wrap_status(status),
                                    None => status,
                                };
                                // same as above -- the provider's message stays in the
                                // context string (the caller renders only the outermost one) and
                                // the typed `Status` is attached as the cause.
                                let detail = format!(
                                    "upstream readiness failed ({:?}): {}",
                                    status.code(),
                                    status.message()
                                );
                                return Err(anyhow::Error::new(status).context(detail));
                            }
                        }
                    }
                    bail!("upstream readiness produced no authoritative model-token usage");
                }
            }
        }
    }

    /// Run the upstream: normalize its output into `CanonChunk` and send it incrementally into
    /// `tx` (R6). `output_limit` is the provider/model output cap; total billing is carried only by
    /// terminal usage and checked against the separate gateway reservation. `req` is
    /// the buyer's canonical request (R1). Finishes on upstream
    /// exhaustion, on reaching `count`, or when the buyer disconnected (`tx` closed = STOP).
    pub async fn run(
        &self,
        output_limit: u64,
        req: Option<CanonRequest>,
        tx: mpsc::Sender<UpstreamResult>,
    ) {
        self.run_for_market(None, None, output_limit, req, tx).await
    }

    /// [`Self::run`], for a caller that knows which market the call is being made for (seller readiness).
    /// The market only selects which question the adapter's served-model check answers; it changes nothing
    /// about the request sent upstream. Buyer traffic passes `None` -- by the time a buyer streams, the
    /// market verdict was already given at readiness, and refusing mid-stream would strand paid capacity
    /// .
    async fn run_for_market(
        &self,
        market: Option<&str>,
        requirements: Option<StartupCapabilityRequirements>,
        output_limit: u64,
        req: Option<CanonRequest>,
        tx: mpsc::Sender<UpstreamResult>,
    ) {
        match self {
            UpstreamConfig::Mock => mock::run(output_limit, req.as_ref(), tx, false, None).await,
            UpstreamConfig::MockWithClaimedModel(claimed_model) => {
                mock::run(
                    output_limit,
                    req.as_ref(),
                    tx,
                    false,
                    Some(claimed_model.as_str()),
                )
                .await
            }
            UpstreamConfig::MockScammer => {
                mock::run(output_limit, req.as_ref(), tx, true, None).await
            }
            UpstreamConfig::OpenAi(cfg) => match requirements {
                Some(requirements) => {
                    openai::run_startup_probe(cfg, market, requirements, output_limit, req, tx)
                        .await
                }
                None => openai::run(cfg, market, output_limit, req, tx).await,
            },
            UpstreamConfig::Anthropic(cfg) => anthropic::run(cfg, output_limit, req, tx).await,
        }
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
            UPSTREAM_ERROR_BODY_MAX_BYTES, UPSTREAM_ERROR_DETAIL_MAX_BYTES,
            UPSTREAM_ERROR_ECHO_PREFIX_CHARS, UPSTREAM_SSE_FRAME_MAX_BYTES,
        };

        assert_eq!(UPSTREAM_HEALTH_PROBE_PROMPT, "Reply with OK.");
        // the pin moved from 1 to the canonical content-probe budget. What this test owns is
        // that the knob lives in core params and is stated exactly here -- not the number itself, and
        // the number is still pinned exactly, not loosened.
        assert_eq!(
            UPSTREAM_HEALTH_PROBE_MAX_TOKENS,
            dexdo_core::params::CONTENT_PROBE_MAX_TOKENS as u32
        );
        assert_eq!(UPSTREAM_HEALTH_PROBE_MAX_TOKENS, 64);
        assert_eq!(UPSTREAM_HEALTH_CHANNEL_CAPACITY, 4);
        assert_eq!(GATEWAY_UPSTREAM_CHANNEL_CAPACITY, 16);
        assert_eq!(GATEWAY_CLIENT_CHANNEL_CAPACITY, 16);
        assert_eq!(UPSTREAM_ERROR_BODY_MAX_BYTES, 4_096);
        assert_eq!(UPSTREAM_ERROR_DETAIL_MAX_BYTES, 1_024);
        assert_eq!(UPSTREAM_ERROR_ECHO_PREFIX_CHARS, 32);
        assert_eq!(UPSTREAM_SSE_FRAME_MAX_BYTES, 1_048_576);

        let upstream = include_str!("mod.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .expect("upstream unit-test module boundary")
            .0;
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
        for literal in [
            "content: \"Reply with OK.\"",
            "max_tokens: 1",
            "mpsc::channel(4)",
            "self.run(1,",
        ] {
            assert!(
                !upstream.contains(literal),
                "upstream production policy must not copy {literal}"
            );
        }

        let gateway = include_str!("../gateway.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .expect("gateway unit-test module boundary")
            .0;
        assert!(gateway.contains("GATEWAY_UPSTREAM_CHANNEL_CAPACITY"));
        assert!(gateway.contains("GATEWAY_CLIENT_CHANNEL_CAPACITY"));
        assert!(!gateway.contains("mpsc::channel(16)"));

        let openai = include_str!("openai.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .expect("OpenAI unit-test module boundary")
            .0;
        for name in [
            "UPSTREAM_ERROR_BODY_MAX_BYTES",
            "UPSTREAM_ERROR_DETAIL_MAX_BYTES",
            "UPSTREAM_ERROR_ECHO_PREFIX_CHARS",
            "UPSTREAM_SSE_FRAME_MAX_BYTES",
        ] {
            assert!(openai.contains(name), "OpenAI must consume params::{name}");
        }
        for alias in [
            "MAX_UPSTREAM_ERROR_BODY_BYTES",
            "MAX_UPSTREAM_ERROR_DETAIL_BYTES",
            "PARTIAL_ECHO_PREFIX_CHARS",
            "MAX_SSE_FRAME_BYTES",
        ] {
            assert!(
                !openai.contains(alias),
                "OpenAI must not own local alias {alias}"
            );
        }

        let anthropic = include_str!("anthropic.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .expect("Anthropic unit-test module boundary")
            .0;
        assert!(anthropic.contains("UPSTREAM_SSE_FRAME_MAX_BYTES"));
        assert!(!anthropic.contains("MAX_SSE_FRAME_BYTES"));
    }

    fn model(base_url: &str, served_model: &str) -> ModelConfig {
        ModelConfig {
            frame_model: "qwen--qwen3--32b".to_string(),
            base_url: base_url.to_string(),
            served_model: served_model.to_string(),
            api_key_env: "PROVIDER_API_KEY".to_string(),
            tokenizer_family: "qwen".to_string(),
            price_per_tick: 1,
            capabilities: Capabilities {
                max_output_tokens: Some(openai::DEFAULT_MAX_OUTPUT_TOKENS),
                ..Capabilities::default()
            },
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
        assert_eq!(openai.capabilities, openai_model.capabilities);
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

    async fn first_claimed_model(upstream: UpstreamConfig) -> String {
        let (tx, mut rx) = mpsc::channel(2);
        upstream.run(1, None, tx).await;
        match rx.recv().await.unwrap().unwrap() {
            UpstreamEvent::Chunk(chunk) => chunk.manifest.unwrap().claimed_model,
            UpstreamEvent::Usage(_) => panic!("mock must emit a chunk first"),
        }
    }

    #[tokio::test]
    async fn registry_enabled_mock_claims_exact_name_and_disabled_mock_stays_legacy() {
        assert_eq!(
            first_claimed_model(UpstreamConfig::MockWithClaimedModel(
                "Qwen/Qwen3-32B".to_string()
            ))
            .await,
            "Qwen/Qwen3-32B"
        );
        assert_eq!(first_claimed_model(UpstreamConfig::Mock).await, "mock");
    }

    #[test]
    fn unknown_model_output_cap_is_refused_with_an_actionable_message() {
        // unknown != unbounded. The refusal names the model, the missing capability and where to set it.
        for declared in [None, Some(0)] {
            let status = resolve_model_output_cap(declared, "qwen--qwen3--32b", "qwen/qwen3-32b")
                .unwrap_err();
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            let message = status.message();
            assert!(message.contains("qwen--qwen3--32b"), "{message}");
            assert!(message.contains("qwen/qwen3-32b"), "{message}");
            assert!(message.contains("max_output_tokens"), "{message}");
            assert!(message.contains("models.json"), "{message}");
        }
        assert_eq!(
            resolve_model_output_cap(Some(40_960), "frame", "served").unwrap(),
            40_960
        );
    }

    /// the capability probe's budget is the model's own declared output cap, and an
    /// undeclared cap FAILS CLOSED here rather than falling back to a number of ours.

    /// The fallback is the habit this change exists to break: any constant we pick is fitted to the
    /// models we happened to measure and wrong for the next one. The refusal must still name the
    /// model and the fix, and it must happen before the provider is contacted -- the base URL below
    /// is a closed port, so a probe that tried to connect would fail with a transport error instead.
    #[tokio::test]
    async fn issue_1278_capability_probe_refuses_an_undeclared_output_cap_before_contacting_anyone()
    {
        use crate::seller::OpenAiConfig;

        let upstream = UpstreamConfig::OpenAi(OpenAiConfig {
            base_url: "http://127.0.0.1:9".to_string(),
            model: "qwen/qwen3-32b".to_string(),
            frame_model: "qwen--qwen3--32b--tools".to_string(),
            claimed_model_override: None,
            api_key_env: "PATH".to_string(),
            tokenizer_family: "exact".to_string(),
            capabilities: Capabilities {
                max_output_tokens: None,
                ..Capabilities::default()
            },
            identity_aliases: Vec::new(),
        });

        let error = upstream
            .check_startup_market_readiness()
            .await
            .expect_err("an undeclared output cap must refuse the capability probe")
            .to_string();

        assert!(error.contains("qwen--qwen3--32b--tools"), "{error}");
        assert!(error.contains("qwen/qwen3-32b"), "{error}");
        assert!(error.contains("max_output_tokens"), "{error}");
        assert!(error.contains("models.json"), "{error}");
        assert!(
            !error.contains("connect") && !error.contains("transport"),
            "the refusal must precede any provider contact: {error}"
        );
    }

    #[test]
    fn provider_request_rejections_are_seller_configuration_faults() {
        // The seller builds the whole upstream request, so a `4xx` request rejection is its own config fault.
        for code in [400, 404, 413, 422] {
            assert!(is_seller_config_http_status(code), "{code}");
        }
        // `401/403` keep the dedicated auth class; `408/429` and every `5xx` are transient upstream trouble.
        for code in [401, 403, 408, 429, 500, 502, 503] {
            assert!(!is_seller_config_http_status(code), "{code}");
        }
    }

    #[test]
    fn seller_configuration_annotation_keeps_the_parsed_prefix_and_code() {
        let original =
            Status::unavailable("upstream HTTP 400 Bad Request: unsupported field: random_seed");
        let annotated = annotate_seller_config_fault(
            original,
            400,
            "qwen/qwen3-32b",
            40_961,
            40_961,
            Some(SampleAlgorithm::RandomSeed),
        );
        assert_eq!(annotated.code(), tonic::Code::Unavailable);
        let message = annotated.message();
        assert!(
            message.starts_with("upstream HTTP 400 Bad Request"),
            "the classifier and the buyer stream-error policy parse this prefix: {message}"
        );
        assert!(message.contains("seller configuration fault"), "{message}");
        assert!(message.contains("qwen/qwen3-32b"), "{message}");
        assert!(message.contains("sent max_tokens=40961"), "{message}");
        assert!(
            message.contains("capabilities.max_output_tokens=40961"),
            "{message}"
        );
        assert!(
            message.contains("optional field \"random_seed\""),
            "{message}"
        );
        assert!(
            message.contains("capabilities.sample_algorithm=RANDOM_SEED"),
            "{message}"
        );

        // Transient/auth statuses are left byte-for-byte alone.
        for code in [401, 429, 503] {
            let untouched = Status::unavailable("upstream HTTP x");
            assert_eq!(
                annotate_seller_config_fault(untouched, code, "m", 1, 1, None).message(),
                "upstream HTTP x",
                "{code}"
            );
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
            capabilities: Capabilities {
                max_output_tokens: Some(openai::DEFAULT_MAX_OUTPUT_TOKENS),
                ..Capabilities::default()
            },
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
                capabilities: Capabilities {
                    max_output_tokens: Some(openai::DEFAULT_MAX_OUTPUT_TOKENS),
                    ..Capabilities::default()
                },
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
                capabilities: Capabilities {
                    max_output_tokens: Some(openai::DEFAULT_MAX_OUTPUT_TOKENS),
                    ..Capabilities::default()
                },
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
