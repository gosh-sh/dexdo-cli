//! Real OpenAI-compatible upstream.
//! The gateway connects to an OpenAI-compatible API (by default **Groq**, `qwen/qwen3-32b`),
//! sends the buyer's canonical request (R1), reads the **streaming SSE** and normalizes each
//! delta into a `CanonChunk` incrementally (R6).

//! **Billing authority:** money is authorized only by the provider's
//! terminal `usage.prompt_tokens + usage.completion_tokens`. Provider `total_tokens` is optional,
//! but when present must match; the normalized [`BillingUsage`] always carries the checked sum.
//! SSE event boundaries and delta text length are never a token count.

//! The key is taken **from the environment at runtime** ([`api_key`]) and is never stored/logged
//! . Without a key the adapter does not start -- the stream
//! closes with `Status::failed_precondition`, which yields a clean skip in e2e.

use super::{
    annotate_seller_config_fault, resolve_model_output_cap, StartupCapabilityRequirements,
    UpstreamEvent,
};
use crate::seller::models::{Capabilities, ModelConfig};
use dexdo_core::params::{
    SampleAlgorithm, SellerLivenessParams, REFERENCE_SAMPLE_SEED, REFERENCE_TOP_K,
    UPSTREAM_ERROR_BODY_MAX_BYTES, UPSTREAM_ERROR_DETAIL_MAX_BYTES,
    UPSTREAM_ERROR_ECHO_PREFIX_CHARS, UPSTREAM_SSE_FRAME_MAX_BYTES,
};
use dexdo_proto::{BillingUsage, CanonChunk, CanonRequest, SignalManifest};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tonic::Status;

/// Name of the env variable holding the upstream key (Groq OpenAI-compatible API). Creds come from "seahorse".
pub const API_KEY_ENV: &str = "GROQ_API_KEY";

/// Default base of the Groq OpenAI-compatible API.
pub const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Default model id -- Qwen 32B on Groq (canonical id from the Groq model list).
pub const DEFAULT_MODEL: &str = "qwen/qwen3-32b";

/// Maximum output length of the built-in demo default ([`DEFAULT_MODEL`] on Groq), measured against the live
/// provider: `max_tokens=40960` -> HTTP 200, `max_tokens=40961` -> HTTP 400
/// `` `max_tokens` must be less than or equal to `40960` ``. Only the built-in default carries it;
/// every configured model declares its own `capabilities.max_output_tokens`.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 40_960;

/// Real upstream configuration. Carries **only** non-confidential parameters (URL/model id/
/// env-key name/tokenizer family/capabilities). The key itself is NOT stored here -- it is read from
/// the environment at request time via [`OpenAiConfig::api_key_env`]. Operationally built from a
/// model config entry; `Default` is the built-in demo default
/// (Groq/qwen) for tests and `live_groq`.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    /// Base URL of the OpenAI-compatible API (without the trailing `/chat/completions`).
    pub base_url: String,
    /// Upstream model id (`served_model`; forced by the market R1, the buyer's `model` is not trusted).
    /// Sent to the upstream (Groq); an internal detail, NOT the on-wire declared model.
    pub model: String,
    /// **Canonical market id** (`producer--model--version`, e.g. `qwen--qwen3--32b`) -- the protocol-facing model
    /// identity the buyer paid for (B2) and verifies the declaration against. Declared as
    /// `claimed_model`. It is DISTINCT from [`Self::model`] (the upstream slug like `qwen/qwen3-32b`): the buyer's
    /// frame is canonical, so declaring the served slug here would false-trip the substitution check.
    pub frame_model: String,
    /// **Test-only seam:** declare a DIFFERENT `claimed_model` in the
    /// `SignalManifest` while the real upstream still serves [`Self::model`]. `None` (default / production path,
    /// [`Self::from_model`]) -> `claimed_model == frame_model` (honest declaration of the canonical market id).
    /// `Some(name)` -> emit `name` as the declared model while serving `model` -- reproduces a served!=declared
    /// substitution so the buyer's content gate (B8 + B7-full) can be proven against a REAL divergent upstream.
    pub claimed_model_override: Option<String>,
    /// Name of the env variable holding the key -- **per-model/provider**, not a single global one.
    pub api_key_env: String,
    /// Tokenizer family for `SignalManifest` -- from config, not a substring hardcode.
    pub tokenizer_family: String,
    /// Upstream capabilities.
    pub capabilities: Capabilities,
    /// The operator's declared extra spellings for this SAME model --
    /// e.g. `Qwen/Qwen3-32B` for the slug `qwen/qwen3-32b`. The buyer already reconciles identity across
    /// all three spellings through this field ([`crate::buyer::verify`]); the seller's served-model check
    /// reads the same one, so an operator who declared the provider's own spelling the supported way is
    /// not refused for it.
    pub identity_aliases: Vec<String>,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            frame_model: DEFAULT_MODEL.to_string(),
            claimed_model_override: None,
            api_key_env: API_KEY_ENV.to_string(),
            tokenizer_family: "qwen".to_string(),
            capabilities: Capabilities {
                max_output_tokens: Some(DEFAULT_MAX_OUTPUT_TOKENS),
                // The built-in demo is a known Groq profile, not an unfamiliar config entry.
                sample_algorithm: SampleAlgorithm::Seed,
            },
            identity_aliases: Vec::new(),
        }
    }
}

impl OpenAiConfig {
    /// Build from a model config entry -- the operational CLI path (`--model`).
    pub fn from_model(m: &ModelConfig, registry_frame_model: Option<&str>) -> Self {
        Self {
            base_url: m.base_url.clone(),
            model: m.served_model.clone(),
            // The on-wire declared model is the CANONICAL frame (what the buyer paid for / verifies against),
            // not the upstream served slug -- else the buyer's check false-trips a substitution.
            frame_model: registry_frame_model.unwrap_or(&m.frame_model).to_string(),
            // Production path: honest declaration (`claimed_model == frame_model`). The override is test-only.
            claimed_model_override: None,
            api_key_env: m.api_key_env.clone(),
            tokenizer_family: m.tokenizer_family.clone(),
            capabilities: m.capabilities.clone(),
            identity_aliases: m.identity_aliases.clone(),
        }
    }
}

/// Read the upstream key from the environment (runtime) by the **env-variable name from the model config**
/// . `None` means no key (the live path is unavailable). The value is never
/// logged and never persisted to disk.
pub fn api_key(env_name: &str) -> Option<String> {
    std::env::var(env_name).ok().filter(|k| !k.is_empty())
}

// --- Request/response shape of OpenAI-compatible chat-completions (subset for the adapter) ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    stream: bool,
    /// the terminal `usage` record is the ONLY billing authority, and an OpenAI-compatible endpoint
    /// omits it from a stream unless it is asked for. Always sent -- a seller that does not ask cannot be paid.
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    /// Always sent and always bounded: `min(buyer request, deal budget, model output cap)`.
    /// Never optional -- an absent/unbounded generation limit is exactly what the provider rejects.
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    random_seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    /// Internal annotation context; the provider never sees this field.
    #[serde(skip)]
    sample_algorithm: Option<SampleAlgorithm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningRequest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<CapabilityToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<CapabilityToolChoice>,
}

#[derive(Serialize)]
struct ReasoningRequest {
    enabled: bool,
    exclude: bool,
}

const CAPABILITY_TOOL_NAME: &str = "dexdo_capability_probe";

#[derive(Serialize)]
struct CapabilityToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    function: CapabilityToolFunction,
}

#[derive(Serialize)]
struct CapabilityToolFunction {
    name: &'static str,
    description: &'static str,
    parameters: CapabilityToolParameters,
}

#[derive(Serialize)]
struct CapabilityToolParameters {
    #[serde(rename = "type")]
    kind: &'static str,
    properties: serde_json::Value,
    #[serde(rename = "additionalProperties")]
    additional_properties: bool,
}

#[derive(Serialize)]
struct CapabilityToolChoice {
    #[serde(rename = "type")]
    kind: &'static str,
    function: CapabilityToolChoiceFunction,
}

#[derive(Serialize)]
struct CapabilityToolChoiceFunction {
    name: &'static str,
}

/// `stream_options.include_usage`: ask the endpoint to close the stream with its native usage record.
#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning_details: Option<Vec<ReasoningDetailWire>>,
}

#[derive(Deserialize, Default)]
struct ReasoningDetailWire {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

/// Build the upstream request body from the buyer's canonical request (R1: normalizing the
/// request into the upstream format). `model` is forced by the market from configuration -- the buyer's `model`
/// is absent from `CanonRequest` by design.

/// `model_output_cap` is the model's own maximum output length, already resolved fail-closed by the caller
/// ([`resolve_model_output_cap`]); the outbound generation limit is the minimum of all three bounds.
#[cfg(test)]
fn build_request<'a>(
    cfg: &'a OpenAiConfig,
    req: &CanonRequest,
    count: u64,
    model_output_cap: u32,
) -> ChatRequest<'a> {
    build_request_with_startup_capabilities(cfg, req, count, model_output_cap, None)
}

fn build_request_with_startup_capabilities<'a>(
    cfg: &'a OpenAiConfig,
    req: &CanonRequest,
    count: u64,
    model_output_cap: u32,
    requirements: Option<StartupCapabilityRequirements>,
) -> ChatRequest<'a> {
    let messages = req
        .messages
        .iter()
        .map(|m| WireMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();
    let (temperature, requested_max_tokens, stop, sample_algorithm) = match &req.params {
        Some(p) => (
            // `greedy` (B7 spot-check) forcibly sets temp=0 (distinct from 0="not set").
            if p.greedy {
                Some(0.0)
            } else {
                (p.temperature != 0.0).then_some(p.temperature)
            },
            (p.max_tokens != 0).then_some(p.max_tokens),
            p.stop.clone(),
            // B7 needs a provider-declared greedy control on both legs. A readiness request is not greedy,
            // and NONE deliberately adds nothing so the reference layer degrades before making the call.
            p.greedy
                .then_some(cfg.capabilities.sample_algorithm)
                .filter(|algorithm| *algorithm != SampleAlgorithm::None),
        ),
        None => (None, None, Vec::new(), None),
    };
    // the outbound limit is bounded by ALL THREE of the buyer's request, the deal budget and the
    // model's own output cap. A missing buyer value is not "unbounded" (it used to become `u32::MAX`) and a
    // deal budget is `ticks * TICK_SIZE` tokens, so without the model cap every provider answered `400`.
    let deal_max_tokens = u32::try_from(count).unwrap_or(u32::MAX);
    let max_tokens = requested_max_tokens
        .unwrap_or(u32::MAX)
        .min(deal_max_tokens)
        .min(model_output_cap)
        .max(1);
    let probe_tools = requirements.is_some_and(|requirements| requirements.tools);
    let probe_thinking = requirements.is_some_and(|requirements| requirements.think);
    ChatRequest {
        model: &cfg.model,
        messages,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        temperature,
        max_tokens,
        stop,
        seed: (sample_algorithm == Some(SampleAlgorithm::Seed)).then_some(REFERENCE_SAMPLE_SEED),
        random_seed: (sample_algorithm == Some(SampleAlgorithm::RandomSeed))
            .then_some(REFERENCE_SAMPLE_SEED),
        top_k: (sample_algorithm == Some(SampleAlgorithm::TopK)).then_some(REFERENCE_TOP_K),
        sample_algorithm,
        reasoning: (probe_thinking || openrouter_qwen_reasoning(cfg)).then_some(ReasoningRequest {
            enabled: true,
            exclude: false,
        }),
        tools: if probe_tools {
            vec![CapabilityToolDefinition {
                kind: "function",
                function: CapabilityToolFunction {
                    name: CAPABILITY_TOOL_NAME,
                    description: "Return an empty object to prove tool-call support.",
                    parameters: CapabilityToolParameters {
                        kind: "object",
                        properties: serde_json::json!({}),
                        additional_properties: false,
                    },
                },
            }]
        } else {
            Vec::new()
        },
        tool_choice: probe_tools.then_some(CapabilityToolChoice {
            kind: "function",
            function: CapabilityToolChoiceFunction {
                name: CAPABILITY_TOOL_NAME,
            },
        }),
    }
}

fn openrouter_qwen_reasoning(cfg: &OpenAiConfig) -> bool {
    cfg.base_url.to_ascii_lowercase().contains("openrouter.ai")
        && cfg.model.eq_ignore_ascii_case("qwen/qwen3-32b")
}

/// Run the real upstream: POST `.../chat/completions` with `stream:true`, parse the SSE and
/// normalize deltas into `CanonChunk`, yielding incrementally into `tx` (R6). No more than `count`
/// delivered tokens are requested/forwarded. A canonical request is mandatory
/// . On a missing key/error we
/// close the stream with an error status (the response buffer does not accumulate).

/// `market` is the identity of the market this call is being made for, when the caller knows one
/// (seller readiness). `None` is "no market is in question" -- buyer traffic and the bare provider-health
/// probe. It selects which question the served-model check answers; see [`offered_model_aliases`].
pub async fn run(
    cfg: &OpenAiConfig,
    market: Option<&str>,
    count: u64,
    req: Option<CanonRequest>,
    tx: mpsc::Sender<Result<UpstreamEvent, Status>>,
) {
    run_with_startup_capabilities(cfg, market, None, count, req, tx).await
}

pub(super) async fn run_startup_probe(
    cfg: &OpenAiConfig,
    market: Option<&str>,
    requirements: StartupCapabilityRequirements,
    count: u64,
    req: Option<CanonRequest>,
    tx: mpsc::Sender<Result<UpstreamEvent, Status>>,
) {
    run_with_startup_capabilities(cfg, market, Some(requirements), count, req, tx).await
}

async fn run_with_startup_capabilities(
    cfg: &OpenAiConfig,
    market: Option<&str>,
    requirements: Option<StartupCapabilityRequirements>,
    count: u64,
    req: Option<CanonRequest>,
    tx: mpsc::Sender<Result<UpstreamEvent, Status>>,
) {
    if count == 0 {
        return;
    }
    // capabilities never gate serving. A model without log probabilities is a normal seller model --
    // the optional arrays are a verification signal, and the provider's terminal native usage is what bills.
    // resolve the model's own output cap FIRST -- an unknown cap must fail closed here, before any
    // provider connection, instead of sending an unbounded `max_tokens` and collecting a `400`.
    let model_output_cap = match resolve_model_output_cap(
        cfg.capabilities.max_output_tokens,
        &cfg.frame_model,
        &cfg.model,
    ) {
        Ok(cap) => cap,
        Err(status) => {
            let _ = tx.send(Err(status)).await;
            return;
        }
    };
    let Some(key) = api_key(&cfg.api_key_env) else {
        let _ = tx
            .send(Err(Status::failed_precondition(format!(
                "real upstream key absent ({})",
                cfg.api_key_env
            ))))
            .await;
        return;
    };
    let Some(req) = req else {
        let _ = tx
            .send(Err(Status::invalid_argument(
                "real upstream requires a canonical request",
            )))
            .await;
        return;
    };

    if let Err(status) = stream_upstream_with_startup_capabilities(
        OpenAiStreamContext {
            cfg,
            market,
            requirements,
            count,
            tx: &tx,
        },
        &key,
        &req,
        model_output_cap,
    )
    .await
    {
        // Send the error into the channel (if the buyer is still listening) -- without leaking the key into the text.
        let _ = tx.send(Err(*status)).await;
    }
}

/// Provider statuses that mean **later**, before the provider emitted an answer.

/// `429` is an explicit rate-limit instruction. `502` and `503` mean the provider gateway or
/// service is momentarily unavailable. Those three may be retried before any output crosses to the
/// buyer. Every other non-success status is an answer and is terminal here: `400` is a request
/// rejection, `401`/`403` reject the credentials, and `404` covers an unavailable model/route.
/// Content-level refusals arrive inside a successful response and are likewise never restarted.
fn retryable_provider_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
    )
}

/// A transport that did not produce a response, or whose body decoder died, did not answer.
/// Invalid request construction and redirect policy are local/configuration answers and stay
/// terminal rather than being repeated.
fn retryable_transport_error(error: &reqwest::Error) -> bool {
    !error.is_builder()
        && !error.is_redirect()
        && (error.is_connect()
            || error.is_timeout()
            || error.is_request()
            || error.is_body()
            || error.is_decode())
}

/// Read both legal `Retry-After` forms: delta-seconds and an HTTP date. An invalid header is not an
/// instruction, so the caller falls back to the seller's canonical supervision cadence.
fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    Some(
        retry_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

enum RetryStep {
    Retry,
    BuyerGone,
}

#[derive(Clone, Copy)]
struct OpenAiStreamContext<'a> {
    cfg: &'a OpenAiConfig,
    market: Option<&'a str>,
    requirements: Option<StartupCapabilityRequirements>,
    count: u64,
    tx: &'a mpsc::Sender<Result<UpstreamEvent, Status>>,
}

/// Wait exactly as long as the provider requested, or use the already-canonical seller health
/// cadence when it gave no usable instruction. The whole pre-output retry phase is bounded by the
/// existing seller supervision cycle; the buyer closing its stream is an earlier domain bound.
async fn wait_for_retry(
    failure: Status,
    retry_after: Option<Duration>,
    retry_deadline: tokio::time::Instant,
    tx: &mpsc::Sender<Result<UpstreamEvent, Status>>,
) -> Result<RetryStep, Box<Status>> {
    let timing = SellerLivenessParams::canonical();
    let delay = retry_after.unwrap_or(timing.health_interval);
    let remaining = retry_deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() || delay >= remaining {
        // Never retry before `Retry-After`, and never sleep past the domain's supervision cycle.
        return Err(Box::new(failure));
    }
    tracing::warn!(
        error = %failure,
        retry_delay_ms = delay.as_millis(),
        "seller upstream did not answer; retrying before any output was delivered"
    );
    tokio::select! {
        _ = tx.closed() => Ok(RetryStep::BuyerGone),
        _ = tokio::time::sleep(delay) => Ok(RetryStep::Retry),
    }
}

/// Internal stream loop: connect, parse SSE, normalize. Errors are returned as `Status`
/// without confidential data. The `Authorization` header carries the key only at runtime.
#[cfg(test)]
async fn stream_upstream(
    cfg: &OpenAiConfig,
    market: Option<&str>,
    key: &str,
    count: u64,
    req: &CanonRequest,
    tx: &mpsc::Sender<Result<UpstreamEvent, Status>>,
    model_output_cap: u32,
) -> Result<(), Box<Status>> {
    stream_upstream_with_startup_capabilities(
        OpenAiStreamContext {
            cfg,
            market,
            requirements: None,
            count,
            tx,
        },
        key,
        req,
        model_output_cap,
    )
    .await
}

async fn stream_upstream_with_startup_capabilities(
    context: OpenAiStreamContext<'_>,
    key: &str,
    req: &CanonRequest,
    model_output_cap: u32,
) -> Result<(), Box<Status>> {
    let OpenAiStreamContext {
        cfg,
        requirements,
        count,
        tx,
        ..
    } = context;
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body =
        build_request_with_startup_capabilities(cfg, req, count, model_output_cap, requirements);
    let client = reqwest::Client::new();
    // A matched gateway stream carries no order-book deadline: the match has already consumed that
    // order. Before the first output, reuse the seller's existing supervision-cycle budget instead
    // of inventing a provider timeout. Once output crosses to the buyer, restarting is forbidden and
    // this retry deadline no longer applies; the buyer stream itself owns the remaining lifetime.
    let retry_deadline =
        tokio::time::Instant::now() + SellerLivenessParams::canonical().health_cycle_timeout;

    'attempt: loop {
        let send = client.post(&url).bearer_auth(key).json(&body).send();
        let response = tokio::select! {
            _ = tx.closed() => return Ok(()),
            response = tokio::time::timeout_at(retry_deadline, send) => response,
        };
        let resp = match response {
            Ok(Ok(resp)) => resp,
            Ok(Err(error)) if retryable_transport_error(&error) => {
                let failure = Status::unavailable(format!("upstream connect failed: {error}"));
                match wait_for_retry(failure, None, retry_deadline, tx).await? {
                    RetryStep::Retry => continue 'attempt,
                    RetryStep::BuyerGone => return Ok(()),
                }
            }
            Ok(Err(error)) => {
                return Err(Box::new(Status::unavailable(format!(
                    "upstream connect failed: {error}"
                ))))
            }
            Err(_) => {
                return Err(Box::new(Status::deadline_exceeded(
                    "upstream produced no output before the seller supervision cycle ended",
                )))
            }
        };

        if retryable_provider_status(resp.status()) {
            let failure = Status::unavailable(format!("upstream HTTP {}", resp.status()));
            let retry_after = retry_after_delay(resp.headers());
            match wait_for_retry(failure, retry_after, retry_deadline, tx).await? {
                RetryStep::Retry => continue 'attempt,
                RetryStep::BuyerGone => return Ok(()),
            }
        }

        if !resp.status().is_success() {
            // A terminal `4xx` here rejects a request the SELLER built end to end, so name the served model
            // and exact generation limit instead of relaying an opaque provider line to the buyer.
            let http_status = resp.status().as_u16();
            let sent_max_tokens = body.max_tokens;
            return Err(Box::new(annotate_seller_config_fault(
                upstream_http_error(resp, key, req).await,
                http_status,
                &cfg.model,
                sent_max_tokens,
                model_output_cap,
                body.sample_algorithm,
            )));
        }

        let mut ending = StreamEnding::default();
        let result = stream_response(resp, context, retry_deadline, &mut ending).await;
        match result {
            Err(failure) if ending.retryable_pre_output_non_answer => {
                match wait_for_retry(*failure, None, retry_deadline, tx).await? {
                    RetryStep::Retry => continue 'attempt,
                    RetryStep::BuyerGone => return Ok(()),
                }
            }
            Err(failure) => return Err(Box::new(ending.explain(*failure, key, req))),
            Ok(()) => return Ok(()),
        }
    }
}

/// How one provider response ended, beyond the `Status` itself: the facts the caller acts on and
/// cannot recover from a message.
#[derive(Default)]
struct StreamEnding {
    /// The body disappeared, or its decoder died, before one complete output event crossed to the
    /// buyer. That is transport silence and may be asked again.
    retryable_pre_output_non_answer: bool,
    /// The provider's own words: from an in-band `event: error` frame, or from a whole body
    /// that was a bare JSON error object and never became a frame at all. Raw and untrusted:
    /// redaction and bounding happen once, in [`Self::explain`], where the key and the request that
    /// must not be echoed back are in scope.
    provider_error: Option<String>,
    /// the answer above arrived as the WHOLE body rather than as a frame. The two are the
    /// same verdict -- the provider answered -- but not the same observation, and the operator is
    /// told which one was seen: an in-band error frame means the stream started and then stopped,
    /// while this means there was never a stream, only an error object under a `200`.
    answered_without_framing: bool,
    /// bounded prefix of the raw response body, accumulated only while nothing has been
    /// delivered. The bound is the one this adapter already reads an untrusted provider error body
    /// with ([`UPSTREAM_ERROR_BODY_MAX_BYTES`], [`upstream_http_error`]) -- the same question, asked
    /// of a `200` instead of a `4xx`.
    unframed_body: Vec<u8>,
}

impl StreamEnding {
    /// Keep the bounded body prefix, so a response that turns out to have been an ANSWER rather
    /// than a stream can still be recognized as one once it ends. Past the cap the body is
    /// not an error object this can parse anyway, so there is nothing to gain by growing it.
    fn observe_body(&mut self, bytes: &[u8]) {
        let remaining = UPSTREAM_ERROR_BODY_MAX_BYTES.saturating_sub(self.unframed_body.len());
        if remaining == 0 {
            return;
        }
        self.unframed_body
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }

    /// Transport silence may be retried -- unless the provider ANSWERED. An answer names why the
    /// response stopped, and asking the same question again gets the same answer: it costs the
    /// operator a full supervision cycle before they are told, and on an open deal it spends the
    /// buyer's delivery window.

    /// There are two ways that answer reaches us and this is the one place that decides between
    /// them and silence. An `event: error` frame is already recorded by the time this is called
    /// . A bare JSON error object under a `200` never becomes a frame -- `parse_event` reads
    /// `data:` lines and this body has none, so the stream ends having seen nothing -- and is
    /// recognized here, from the body itself.
    fn transport_non_answer(&mut self) {
        if self.provider_error.is_some() {
            return;
        }
        if let Some(detail) = unframed_provider_error(&self.unframed_body) {
            self.provider_error = Some(detail);
            self.answered_without_framing = true;
            return;
        }
        self.retryable_pre_output_non_answer = true;
    }

    /// Carry the provider's own sentence into OUR refusal.

    /// The class stays ours, first and unchanged: it says what the SELLER did with the stream, and
    /// every caller and test that reads it keeps reading the same thing. What follows is what the
    /// provider said, which is the part that tells an operator whether to fix `tool_choice` or go
    /// and debug the network ( -- reporting our own view of a stream as the whole diagnosis is
    /// the confusion that cost a day in).

    /// The provider's text is attacker-controlled, so it goes through the SAME redaction and the
    /// SAME length bound as every other provider string this adapter surfaces
    /// ([`sanitize_error_detail`] and [`bound_error_detail`], `UPSTREAM_ERROR_ECHO_PREFIX_CHARS` /
    /// `UPSTREAM_ERROR_DETAIL_MAX_BYTES`). One policy, not two.
    fn explain(&self, failure: Status, key: &str, request: &CanonRequest) -> Status {
        let Some(detail) = self.provider_error.as_deref() else {
            return failure;
        };
        let detail = bound_error_detail(sanitize_error_detail(detail, key, request), false);
        if detail.is_empty() {
            return failure;
        }
        Status::new(
            failure.code(),
            format!("{}: provider reported: {detail}", failure.message()),
        )
    }
}

#[derive(Default)]
struct StartupCapabilityObservation {
    saw_content: bool,
    saw_reasoning: bool,
    saw_tool_call: bool,
    reasoning_tokens: Option<u64>,
}

impl StartupCapabilityObservation {
    #[allow(clippy::result_large_err)]
    fn observe_reasoning_tokens(&mut self, reported: Option<u64>) -> Result<(), Status> {
        let Some(reported) = reported else {
            return Ok(());
        };
        if self
            .reasoning_tokens
            .is_some_and(|recorded| recorded != reported)
        {
            return Err(Status::data_loss(
                "OpenAI-compatible stream reported contradictory reasoning-token usage totals",
            ));
        }
        self.reasoning_tokens = Some(reported);
        Ok(())
    }

    #[allow(clippy::result_large_err)]
    fn validate(
        &self,
        requirements: StartupCapabilityRequirements,
        completion_tokens: Option<u64>,
    ) -> Result<(), Status> {
        let tools_missing = requirements.tools && !self.saw_tool_call;
        let reasoning_missing = requirements.think && !self.saw_reasoning;
        let reasoning_usage_missing = requirements.think && self.reasoning_tokens.unwrap_or(0) == 0;
        if !tools_missing && !reasoning_missing && !reasoning_usage_missing {
            return Ok(());
        }

        let mut missing = Vec::new();
        if tools_missing {
            missing.push("tool call");
        }
        if reasoning_missing {
            missing.push("reasoning content");
        }
        if reasoning_usage_missing {
            missing.push("positive reasoning-token usage");
        }
        let completion_tokens = completion_tokens
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "absent".to_string());
        let reasoning_tokens = self
            .reasoning_tokens
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "absent".to_string());
        Err(Status::failed_precondition(format!(
            "startup capability probe for {} asked the upstream for {}; the upstream returned content={}, tool_call={}, reasoning_content={}, completion_tokens={completion_tokens}, reasoning_tokens={reasoning_tokens}; missing {}; remove the unsupported flag from the model id or configure an upstream that returns the declared capability",
            requirements.flags(),
            requirements.asked_for(),
            self.saw_content,
            self.saw_tool_call,
            self.saw_reasoning,
            missing.join(" and ")
        )))
    }
}

#[derive(Clone, Copy, Default)]
struct StartupCapabilityEvent {
    tool_call: bool,
    reasoning_tokens: Option<u64>,
}

#[allow(clippy::result_large_err)]
fn startup_capability_event(
    event: &str,
    requirements: StartupCapabilityRequirements,
) -> Result<StartupCapabilityEvent, Status> {
    let mut data = String::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() || data == "[DONE]" {
        return Ok(StartupCapabilityEvent::default());
    }
    let value = serde_json::from_str::<serde_json::Value>(&data).map_err(|error| {
        Status::data_loss(format!("malformed OpenAI-compatible SSE JSON: {error}"))
    })?;
    Ok(StartupCapabilityEvent {
        tool_call: requirements.tools && frame_has_tool_call(&value),
        reasoning_tokens: if requirements.think {
            frame_reasoning_usage(&value)?
        } else {
            None
        },
    })
}

fn frame_has_tool_call(value: &serde_json::Value) -> bool {
    value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                ["delta", "message"].into_iter().any(|field| {
                    choice.get(field).is_some_and(|container| {
                        container
                            .get("tool_calls")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|calls| calls.iter().any(names_capability_tool))
                            || container
                                .get("function_call")
                                .is_some_and(names_capability_function)
                    })
                })
            })
        })
}

fn names_capability_tool(call: &serde_json::Value) -> bool {
    call.get("function").is_some_and(names_capability_function)
}

fn names_capability_function(function: &serde_json::Value) -> bool {
    function.get("name").and_then(serde_json::Value::as_str) == Some(CAPABILITY_TOOL_NAME)
}

#[allow(clippy::result_large_err)]
fn reasoning_output_total(container: Option<&serde_json::Value>) -> Result<Option<u64>, Status> {
    let Some(container) = container else {
        return Ok(None);
    };
    if container.is_null() {
        return Ok(None);
    }
    let Some(object) = container.as_object() else {
        return Err(Status::data_loss(
            "OpenAI-compatible usage is not an object",
        ));
    };
    let Some(details) = object.get("completion_tokens_details") else {
        return Ok(None);
    };
    if details.is_null() {
        return Ok(None);
    }
    let Some(details) = details.as_object() else {
        return Err(Status::data_loss(
            "OpenAI-compatible usage.completion_tokens_details is not an object",
        ));
    };
    match details.get("reasoning_tokens") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            Status::data_loss(
                "OpenAI-compatible usage.completion_tokens_details.reasoning_tokens is not a token count",
            )
        }),
    }
}

#[allow(clippy::result_large_err)]
fn frame_reasoning_usage(value: &serde_json::Value) -> Result<Option<u64>, Status> {
    let standard = reasoning_output_total(value.get("usage"))?;
    let groq = reasoning_output_total(value.get("x_groq").and_then(|groq| groq.get("usage")))?;
    match (standard, groq) {
        (Some(standard), Some(groq)) if standard != groq => Err(Status::data_loss(
            "OpenAI-compatible reasoning-token usage totals disagree",
        )),
        (Some(total), _) | (None, Some(total)) => Ok(Some(total)),
        (None, None) => Ok(None),
    }
}

/// Parse one successful provider response. `ending` reports how it finished:
/// `retryable_pre_output_non_answer` is set only when the response body disappears or its transport
/// decoder fails before any output crosses to the buyer, and `provider_error` carries the words of
/// an in-band `event: error` frame. Complete invalid frames and semantic refusals stay terminal;
/// output already sent makes every later failure terminal because restarting would duplicate or
/// reorder buyer-visible content.
async fn stream_response(
    resp: reqwest::Response,
    context: OpenAiStreamContext<'_>,
    retry_deadline: tokio::time::Instant,
    ending: &mut StreamEnding,
) -> Result<(), Box<Status>> {
    use futures::StreamExt;

    let OpenAiStreamContext {
        cfg,
        market,
        requirements,
        count,
        tx,
    } = context;

    // Incremental SSE parsing over the body's byte stream (R6): accumulate a buffer, split on
    // `\n\n` boundaries, parse `data:` lines. `data: [DONE]` ends the stream.
    let mut byte_stream = resp.bytes_stream();
    // accumulate BYTES. A network read ends wherever the network chose, which is routinely inside a
    // multi-byte character; decoding each read on its own would silently replace the halves and hand the
    // buyer text the provider never wrote. Bytes are decoded only at a complete `\n\n` frame boundary.
    let mut buf: Vec<u8> = Vec::new();
    let mut seq: u64 = 0;
    // the provider's terminal input-plus-output usage, held locally until the stream
    // terminates consistently so a second, contradictory or post-terminal record cannot leave a
    // partially advanced bill.
    let mut native_usage: Option<BillingUsage> = None;
    // the spellings that may name the model that answers, normalized once for the whole stream.
    // WHICH spellings depends on the question this call is asking -- see `offered_model_aliases`.
    let offered = offered_model_aliases(cfg, market);
    // the identity question is answered ONCE per stream, by the first frame that states one.
    // A second frame repeating the same name says nothing new, so re-normalizing it per frame only burns
    // allocations for the rest of the response.
    let mut model_identity_settled = false;
    let mut capability_observation = requirements.map(|_| StartupCapabilityObservation::default());

    let saw_done = 'provider_stream: loop {
        let item = if seq == 0 {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                item = tokio::time::timeout_at(retry_deadline, byte_stream.next()) => match item {
                    Ok(item) => item,
                    Err(_) => {
                        return Err(Box::new(Status::deadline_exceeded(
                            "upstream produced no output before the seller supervision cycle ended",
                        )))
                    }
                },
            }
        } else {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                item = byte_stream.next() => item,
            }
        };
        let Some(item) = item else {
            break false;
        };
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(error) => {
                if seq == 0 && retryable_transport_error(&error) {
                    ending.transport_non_answer();
                }
                return Err(Box::new(Status::unavailable(format!(
                    "upstream read failed: {error}"
                ))));
            }
        };
        buf.extend_from_slice(&bytes);
        // while nothing has been delivered, this response may still turn out not to be a
        // stream at all. Keep the bounded prefix that lets its ending be classified as an answer.
        if seq == 0 {
            ending.observe_body(&bytes);
        }

        // Flush complete SSE events (separated by `\n\n`); an unfinished frame must not grow
        // the gateway buffer without bound -- a hostile/broken upstream is untrusted (Y3, R6).
        for event in drain_complete_events(&mut buf)? {
            let capability_event = match (requirements, capability_observation.as_mut()) {
                (Some(requirements), Some(observation)) => {
                    let evidence = startup_capability_event(&event, requirements)?;
                    observation.saw_tool_call |= evidence.tool_call;
                    observation.observe_reasoning_tokens(evidence.reasoning_tokens)?;
                    evidence
                }
                _ => StartupCapabilityEvent::default(),
            };
            match parse_event(&event)? {
                ParsedEvent::Done => break 'provider_stream true,
                ParsedEvent::Frame {
                    text,
                    reasoning,
                    usage,
                    model,
                } => {
                    if let Some(observation) = capability_observation.as_mut() {
                        observation.saw_content |= !text.trim().is_empty();
                        observation.saw_reasoning |= !reasoning.trim().is_empty();
                    }
                    // E2E-ADV-02: the provider names the model that actually answered. Compare it
                    // with the model this seller committed to serve and refuse the stream on a mismatch --
                    // in the readiness probe (`UpstreamConfig::check_health`, the
                    // `upstream_authentication_and_model` component) that refusal happens BEFORE
                    // `postSellOffer`, so a foreign model never reaches the book or a buyer.

                    // HOW FAR THIS GOES, so nobody reads more into it than it carries: the seller PROXIES
                    // the provider, so a dishonest seller can rewrite this field before the buyer sees
                    // anything. What this catches is a MISCONFIGURED seller -- the wrong `served_model`, a
                    // typo, a provider that silently substituted a replacement. It is NOT a defence against
                    // deliberate substitution. The only defence against that is the buyer's content
                    // spot-check, where the buyer obtains the reference itself and never
                    // through the seller.

                    // A frame that states no model is not a mismatch: OpenAI-compatible chunks carry
                    // `model`, but an endpoint that omits it gives no identity signal at all, and inventing
                    // a verdict from silence would take honest sellers off the market.

                    // WHY THE REFUSAL IS BOUNDED BY THE FIRST DELIVERED OUTPUT: once a chunk
                    // has been forwarded, an error returned from here reaches `relay_counting` with output
                    // already delivered and its authoritative usage still outstanding, which classifies the
                    // request `AmbiguousUsage` (`seller::gateway`) -> `finish_ambiguous`
                    // (`seller::capacity`), and that terminal deliberately keeps the unresolved remainder
                    // COMMITTED: the buyer loses the capacity it paid for and the seller cannot claim the
                    // tokens it already delivered. Burning both sides' money is not a proportionate answer
                    // to a misconfigured `served_model`, so past that point the divergence is recorded as a
                    // diagnostic and the stream is carried to its honest terminal.

                    // This costs the check nothing it could have had: an OpenAI-compatible provider names
                    // the model in its FIRST frame (Groq states it on the role delta, before any content),
                    // so a real mismatch is always seen at `seq == 0` -- including in the readiness probe
                    // (`check_health`), where E2E-ADV-02 refuses BEFORE `postSellOffer` and no buyer, deal
                    // or reservation exists yet.
                    if !model_identity_settled {
                        if let Some(reported) = model.as_deref() {
                            // Whatever this frame says is the verdict for the stream; there is no second
                            // question to ask of the frames after it.
                            model_identity_settled = true;
                            if !offered.contains(&crate::registry::model_id_alias(reported)) {
                                let mismatch = match market {
                                    // Seller readiness: the offer may not rest, because the model that
                                    // answered is not the model this market sells.
                                    Some(market_model) => format!(
                                        "upstream served model \"{reported}\", but this market sells \
                                         \"{market_model}\": the provider answering this seller is not the \
                                         model the buyer would be paying for -- point --model at the model \
                                         this market was provisioned for, or declare the provider's own \
                                         spelling in identity_aliases for this model in the models config \
                                         (models.json)"
                                    ),
                                    // Provider health: the provider did not serve a model this seller's own
                                    // config names.
                                    None => format!(
                                        "upstream served model \"{reported}\", not the offered \"{}\" \
                                         (market model \"{}\"): correct \
                                         served_model/frame_model/identity_aliases for this model in the \
                                         models config (models.json), or the provider substituted the model",
                                        cfg.model, cfg.frame_model
                                    ),
                                };
                                if seq == 0 {
                                    return Err(Box::new(Status::failed_precondition(mismatch)));
                                }
                                tracing::error!(
                                    "{mismatch}; output was already delivered, so the stream is carried to \
                                     its terminal instead of stranding the buyer's paid capacity"
                                );
                            }
                        }
                    }
                    let has_canon_output = !text.is_empty() || !reasoning.is_empty();
                    let has_output = has_canon_output || capability_event.tool_call;
                    if let Some(reported) = usage {
                        // UPS-29: transport position alone does not make a content-carrying frame terminal.
                        if has_output {
                            return Err(Box::new(Status::data_loss(
                                "OpenAI-compatible usage is attached to an output delta, not a terminal record",
                            )));
                        }
                        if reported.output_tokens > count {
                            return Err(Box::new(Status::data_loss(
                                "OpenAI-compatible completion_tokens exceeds the requested output limit",
                            )));
                        }
                        // UPS-24: one request carries exactly one authoritative aggregate, and it is billed
                        // exactly once. Real OpenAI-compatible endpoints RESTATE that one aggregate rather
                        // than send it once: a live Groq stream carries the total on the `finish_reason`
                        // chunk (as both `usage` and `x_groq.usage`) and again on the dedicated
                        // `stream_options.include_usage` chunk that closes the stream. An identical
                        // restatement is the same number said twice -- it is held once, cannot move the bill,
                        // and refusing it would take a correctly-reported seller off the market for saying
                        // nothing new. Two DIFFERENT totals stay refused: picking one of them would be
                        // inventing the amount.
                        if let Some(recorded) = native_usage {
                            if recorded != reported {
                                return Err(Box::new(Status::data_loss(
                                    "OpenAI-compatible stream reported contradictory terminal usage totals",
                                )));
                            }
                            continue;
                        }
                        native_usage = Some(reported);
                        continue;
                    }
                    if !has_output {
                        continue;
                    }
                    // UPS-30: post-terminal output cannot be ignored while the earlier bill is kept.
                    if native_usage.is_some() {
                        return Err(Box::new(Status::data_loss(
                            "OpenAI-compatible output continued after the terminal usage record",
                        )));
                    }
                    if !has_canon_output {
                        continue;
                    }
                    let chunk = CanonChunk {
                        text,
                        reasoning,
                        // R2/R4: Groq chat-completions does not return token-ids in SSE -- do NOT fabricate.
                        token_ids: Vec::new(),
                        seq,
                        manifest: (seq == 0).then(|| SignalManifest {
                            // Family comes from the model config; the buyer matches the profile.
                            tokenizer_family: cfg.tokenizer_family.clone(),
                            has_token_ids: false,
                            // Declare the CANONICAL frame model (what the buyer paid for / verifies, B2/B7), NOT
                            // the upstream served slug -- declaring the slug false-trips. The test-only
                            // override emits a different declared name to prove a real substitution.
                            claimed_model: cfg
                                .claimed_model_override
                                .clone()
                                .unwrap_or_else(|| cfg.frame_model.clone()),
                        }),
                        usage: None,
                    };
                    seq += 1;
                    if tx.send(Ok(UpstreamEvent::Chunk(chunk))).await.is_err() {
                        return Ok(()); // buyer disconnected (STOP)
                    }
                }
                // the provider ANSWERED, in band, on a `200 OK` stream. It is not a delta and
                // it changes nothing about what this stream delivered -- the refusal below is the
                // same one, reached the same way. What it changes is what the operator is told, and
                // that we do not ask the same question again.
                ParsedEvent::ProviderError(detail) => {
                    // The FIRST stated reason is the one that stopped the response; a later frame
                    // restating it says nothing new.
                    if ending.provider_error.is_none() {
                        ending.provider_error = Some(detail);
                    }
                }
                ParsedEvent::Other => {}
            }
        }
    };
    if saw_done {
        if let (Some(requirements), Some(observation)) = (requirements, &capability_observation) {
            observation.validate(
                requirements,
                native_usage.as_ref().map(|usage| usage.output_tokens),
            )?;
        }
        if seq == 0
            && !capability_observation
                .as_ref()
                .is_some_and(|observation| observation.saw_tool_call)
        {
            let Some(usage) = native_usage else {
                return Err(Box::new(Status::data_loss(
                    "OpenAI-compatible response ended without terminal billing usage",
                )));
            };
            // UPS-28: a positive number alone is not proof of delivered service.
            if usage.total_tokens != 0 {
                return Err(Box::new(Status::data_loss(
                    "OpenAI-compatible usage reports billable tokens without delivered output",
                )));
            }
            return Ok(());
        }
        // UPS-02/UPS-07/UPS-20: delivered output bills exactly the provider's terminal total, and only when
        // that total exists and is positive. Zero or absent is a contradiction, never a fallback to lengths.
        let usage = native_usage
            .filter(|usage| usage.output_tokens > 0)
            .ok_or_else(|| {
                Status::data_loss(
                "OpenAI-compatible output ended without positive terminal usage.completion_tokens",
            )
            })?;
        let _ = tx.send(Ok(UpstreamEvent::Usage(usage))).await;
        return Ok(());
    }
    if seq == 0 {
        // A body that vanished before one complete output event is a transport non-answer. A body
        // that STATED why it stopped is not one, however it then ended -- in band or as the
        // whole body. The verdict is taken BEFORE the message is written, because the
        // message has to name which of the two this was.
        ending.transport_non_answer();
    }
    let failure = if ending.answered_without_framing {
        // saying "ended without [DONE]" here describes our own parser's disappointment and
        // reads as an outage. What happened is that the provider answered, in one shot, with an
        // error object instead of a stream -- and `explain` appends its own words to this.
        Status::data_loss(
            "OpenAI-compatible response was not a stream but a provider error object under HTTP 200; the provider answered, so it was not asked again",
        )
    } else if !buf.is_empty() {
        Status::data_loss("OpenAI-compatible SSE ended with an unfinished frame")
    } else {
        Status::data_loss("OpenAI-compatible SSE ended without [DONE]")
    };
    Err(Box::new(failure))
}

/// Provider errors are untrusted and may echo credentials or request fields. Read only a small
/// prefix and surface either a known JSON error field or a compact text response.
const TRUNCATED_DETAIL_SUFFIX: &str = "... [truncated]";

async fn upstream_http_error(resp: reqwest::Response, key: &str, request: &CanonRequest) -> Status {
    use futures::StreamExt;

    let code = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !is_text_error_body(&content_type) {
        return Status::unavailable(format!(
            "upstream HTTP {code}: non-text response body omitted"
        ));
    }

    let mut body = Vec::new();
    let mut truncated = false;
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(_) => {
                return Status::unavailable(format!(
                    "upstream HTTP {code}: provider error body unreadable"
                ));
            }
        };
        let remaining = UPSTREAM_ERROR_BODY_MAX_BYTES.saturating_sub(body.len());
        if bytes.len() > remaining {
            body.extend_from_slice(&bytes[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&bytes);
    }

    let Some(detail) = safe_error_detail(&body, &content_type, key, request, truncated) else {
        return Status::unavailable(format!("upstream HTTP {code}"));
    };
    Status::unavailable(format!("upstream HTTP {code}: {detail}"))
}

fn is_text_error_body(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    media_type.is_empty()
        || media_type.starts_with("text/")
        || media_type == "application/json"
        || media_type.ends_with("+json")
}

fn safe_error_detail(
    body: &[u8],
    content_type: &str,
    key: &str,
    request: &CanonRequest,
    body_truncated: bool,
) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    if body_truncated {
        return Some(bound_error_detail(
            "provider error body omitted".to_string(),
            true,
        ));
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return Some("non-text response body omitted".to_string());
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let detail = if media_type.ends_with("json")
        || (media_type.is_empty() && (text.starts_with('{') || text.starts_with('[')))
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Some("malformed provider error body omitted".to_string());
        };
        json_error_detail(&value).unwrap_or_else(|| "provider error body omitted".to_string())
    } else {
        text.to_string()
    };

    Some(bound_error_detail(
        sanitize_error_detail(&detail, key, request),
        body_truncated,
    ))
}

/// The error this JSON STATES, if it states one.

/// `serde_json::Value::get` answers "is this key PRESENT", not "does it hold anything": it returns
/// `Some(Value::Null)` for `{"error": null}`. That shape is not exotic -- it is what a nullable
/// member looks like when a gateway serialises a fixed envelope instead of omitting its absent
/// members, and several OpenAI-compatible proxies put it on every chunk they send. So `is_some()` is
/// the wrong question wherever this is asked: the member must be present AND hold something.

/// This is the one place that answers it, so the frame parser, the detail reader and the unframed
/// body reader cannot drift apart on what an error is -- the drift that produced, where two
/// layers asked the same question and only one of them had been taught the answer.
fn stated_error(value: &serde_json::Value) -> Option<&serde_json::Value> {
    // Absent, or present and holding nothing: nothing was stated.
    value.get("error").filter(|error| !error.is_null())
}

fn json_error_detail(value: &serde_json::Value) -> Option<String> {
    // A member holding nothing is not the container to read a reason FROM either: fall back to the
    // value itself, exactly as an absent member already does. Otherwise a frame saying
    // `{"error": null, "message": "rate limited"}` reported no reason at all.
    let error = stated_error(value).unwrap_or(value);
    if let Some(detail) = error.as_str() {
        return Some(detail.to_string());
    }
    ["message", "detail", "code", "type"]
        .into_iter()
        .find_map(|field| error.get(field).and_then(|value| value.as_str()))
        .map(ToString::to_string)
}

/// Is this whole response body the provider ANSWERING with an error, rather than a stream that went
/// quiet?

/// The shape: `200 OK`, `content-type: text/event-stream`, and a body that is a bare JSON error
/// object -- no `data:` prefix, no SSE framing at all. A gateway sends it when it has already
/// committed the status line and only then learns the upstream failed. `parse_event` recognizes a
/// frame by its `data:` lines and nothing else, so this body produces no frame, and the stream used
/// to end at `seq == 0` looking exactly like a body that vanished.

/// **The rule is the one the in-band frame layer already applies**: `parse_event` treats a JSON
/// object carrying an `error` member as the provider stating why it stopped. The only difference is
/// where the bytes were found -- there, inside a `data:` frame; here, as the entire body. Nothing
/// about what a FRAME is changes, and `a_bare_error_object_with_no_data_prefix_is_not_a_frame` still
/// holds: this is read from the body, after the stream has ended, not from the parser.

/// **A present-but-null `error` is not a stated reason.** `serde_json::Value::get` answers
/// "is this key present", not "does it hold anything": it returns `Some(Value::Null)` for
/// `{"error": null}`, a shape several OpenAI-compatible proxies put on every chunk they send. Asking
/// `is_some()` here would read that as the provider having answered -- refusing to retry, and
/// reporting an empty reason to the operator. So the member must be present AND non-null, which is
/// what [`stated_error`] answers for every layer that asks.

/// **Deliberately narrow, and it fails towards today's behaviour.** Only a body that parses whole as
/// JSON and carries an `error` member qualifies. A partial SSE stream does not parse (`data: ` is
/// not JSON), an HTML error page does not parse, a JSON body with no `error` states no reason, and a
/// body longer than [`UPSTREAM_ERROR_BODY_MAX_BYTES`] arrives here truncated and so does not parse
/// either. Every one of those stays the retryable transport non-answer it is today. That asymmetry
/// is the intended one: failing to recognize an answer costs a supervision cycle, while inventing an
/// answer out of a fragment would refuse to retry a deal that a genuine transport blip could still
/// have delivered.
fn unframed_provider_error(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    // Absent, or present and holding nothing: the body stated no reason.
    stated_error(&value)?;
    // An error object that names no reason is still an answer: it will be the same answer next
    // time. The refusal then carries our own class alone, and `explain` adds no empty quotation.
    Some(json_error_detail(&value).unwrap_or_default())
}

fn sanitize_error_detail(detail: &str, key: &str, request: &CanonRequest) -> String {
    fn compact_text(text: &str) -> String {
        text.chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn echoes_request_value(detail: &str, value: &str) -> bool {
        let value = compact_text(value);
        let prefix = value
            .chars()
            .take(UPSTREAM_ERROR_ECHO_PREFIX_CHARS)
            .collect::<String>();
        !detail.is_empty()
            && !value.is_empty()
            && (detail.contains(&value)
                || value.contains(detail)
                || (prefix != value && detail.contains(&prefix)))
    }

    let compact = compact_text(detail);
    let lower = compact.to_ascii_lowercase();
    const SENSITIVE_MARKERS: &[&str] = &[
        "authorization",
        "bearer ",
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "access-token",
        "client_secret",
        "private_key",
        "password",
        "secret",
        "gsk_",
        "sk-",
    ];
    let echoes_request_secret = request
        .messages
        .iter()
        .any(|message| echoes_request_value(&compact, &message.content))
        || request.params.as_ref().is_some_and(|params| {
            params
                .stop
                .iter()
                .any(|stop| echoes_request_value(&compact, stop))
        });
    if echoes_request_secret
        || (!key.is_empty() && compact.contains(key))
        || SENSITIVE_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
    {
        return "sensitive provider error detail redacted".to_string();
    }

    compact
}

fn bound_error_detail(mut detail: String, body_truncated: bool) -> String {
    let truncated = body_truncated || detail.len() > UPSTREAM_ERROR_DETAIL_MAX_BYTES;
    if !truncated {
        return detail;
    }
    let limit = UPSTREAM_ERROR_DETAIL_MAX_BYTES - TRUNCATED_DETAIL_SUFFIX.len();
    let mut end = detail.len().min(limit);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
    detail.push_str(TRUNCATED_DETAIL_SUFFIX);
    detail
}

/// Cap on an unfinished SSE frame (Y3): a hostile/broken upstream sending bytes without
/// a `\n\n` separator must not grow the gateway buffer without bound. Legitimate events (a text
/// delta) are 2-3 orders of magnitude smaller -- 1 MiB does not touch them.
/// Drain complete SSE events (`\n\n`-separated) from the buffer in order. If the REMAINDER
/// (unfinished frame) exceeds the cap -- `resource_exhausted` instead of uncontrolled buffer
/// growth (Y3, R6). Complete events are always drained before the cap check.
// `tonic::Status` is the standard gRPC error type of the whole upstream module; boxing it in a single helper
// would break `?`-propagation into the loop's `Result<_, Status>`. The large Err variant here is deliberate.
#[allow(clippy::result_large_err)]
fn drain_complete_events(buf: &mut Vec<u8>) -> Result<Vec<String>, Status> {
    let mut events = Vec::new();
    while let Some(boundary) = buf.windows(2).position(|window| window == b"\n\n") {
        let frame = buf.drain(..boundary).collect::<Vec<u8>>();
        buf.drain(..2);
        // A COMPLETE frame is where the encoding must already be whole. If it is not, the provider sent
        // bytes that are not text: refuse them. Substituting characters here would forward output the
        // provider never produced, and the buyer cannot tell the difference afterwards.
        events
            .push(String::from_utf8(frame).map_err(|_| {
                Status::data_loss("OpenAI-compatible SSE frame is not valid UTF-8")
            })?);
    }
    if buf.len() > UPSTREAM_SSE_FRAME_MAX_BYTES {
        return Err(Status::resource_exhausted(
            "upstream SSE frame exceeds buffer cap",
        ));
    }
    Ok(events)
}

/// A parsed SSE event.
enum ParsedEvent {
    /// Terminal `data: [DONE]`.
    Done,
    /// `data: {...}`: content/reasoning deltas (possibly empty), the provider's own native input/output
    /// usage when this frame carries one, and the model the provider says actually answered (`model`,
    /// ) when the frame states one.
    Frame {
        text: String,
        reasoning: String,
        usage: Option<BillingUsage>,
        /// Top-level `model` of an OpenAI-compatible chunk -- the provider's own name for what served
        /// this frame. `None` = the frame stated nothing (see [`stream_upstream`]).
        model: Option<String>,
    },
    /// An in-band `event: error` frame on an otherwise successful (`200 OK`) response: the
    /// provider's own statement of why it stopped, carrying its message. The native
    /// Anthropic adapter models the same thing the same way (`anthropic::ParsedEvent::Error`).
    ProviderError(String),
    /// Carries no delta (comment, keep-alive, etc.).
    Other,
}

/// Read one complete native input/output usage record out of a `usage`-shaped container.

/// Untrusted input: a container that is not an object, and a required prompt/completion count that is
/// not a non-negative integer fitting `u64` (string, float, negative, overflowing), are rejected
/// outright (E2E-UPS-18/22). An object declaring none of the billing fields carries no record; the
/// stream then fails at its terminal boundary if no complete record ever arrives (E2E-UPS-07).
#[allow(clippy::result_large_err)]
fn native_billing_usage(
    container: Option<&serde_json::Value>,
) -> Result<Option<BillingUsage>, Status> {
    let Some(container) = container else {
        return Ok(None);
    };
    if container.is_null() {
        return Ok(None);
    }
    let Some(object) = container.as_object() else {
        return Err(Status::data_loss(
            "OpenAI-compatible usage is not an object",
        ));
    };
    let declares_billing = ["prompt_tokens", "completion_tokens", "total_tokens"]
        .iter()
        .any(|field| object.contains_key(*field));
    if !declares_billing {
        return Ok(None);
    }
    let token = |field: &'static str| {
        object
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                Status::data_loss(format!(
                    "OpenAI-compatible usage.{field} is missing or is not a token count"
                ))
            })
    };
    let input_tokens = token("prompt_tokens")?;
    let output_tokens = token("completion_tokens")?;
    let declared_total = object
        .get("total_tokens")
        .map(|_| token("total_tokens"))
        .transpose()?;
    BillingUsage::new(input_tokens, output_tokens, declared_total)
        .map(Some)
        .map_err(Status::data_loss)
}

/// The frame's terminal native total: the standard `usage` and the Groq mirror `x_groq.usage` are BOTH
/// read, and a disagreement between them is rejected instead of resolved (E2E-UPS-19) -- choosing the
/// first, last, smaller or larger value would monetize contradictory provider metadata.
#[allow(clippy::result_large_err)]
fn frame_native_usage(value: &serde_json::Value) -> Result<Option<BillingUsage>, Status> {
    let standard = native_billing_usage(value.get("usage"))?;
    let groq = native_billing_usage(value.get("x_groq").and_then(|groq| groq.get("usage")))?;
    match (standard, groq) {
        (Some(standard), Some(groq)) if standard != groq => Err(Status::data_loss(
            "OpenAI-compatible native usage totals disagree",
        )),
        (Some(total), _) | (None, Some(total)) => Ok(Some(total)),
        (None, None) => Ok(None),
    }
}

/// Parse a single SSE event: join the `data:` lines, recognize `[DONE]` and the provider's own
/// in-band `event: error` frame, otherwise extract `choices[0].delta.content`,
/// provider-separated reasoning and the frame's native input/output usage. A frame
/// without `data:` is `Other`; malformed JSON fails closed.
#[allow(clippy::result_large_err)]
fn parse_event(event: &str) -> Result<ParsedEvent, Status> {
    let mut data = String::new();
    let mut named_error = false;
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            named_error |= rest.trim() == "error";
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(ParsedEvent::Other);
    }
    if data == "[DONE]" {
        return Ok(ParsedEvent::Done);
    }
    let value = serde_json::from_str::<serde_json::Value>(&data)
        .map_err(|e| Status::data_loss(format!("malformed OpenAI-compatible SSE JSON: {e}")))?;
    // The money field is read from the raw frame BEFORE the delta shape, so a provider metadata frame the
    // delta parser does not recognize can neither hide nor invent an aggregate. The identity field
    // is read from the same raw frame for the same reason.
    let usage = frame_native_usage(&value)?;
    let model = frame_served_model(&value);
    // an in-band error frame is the provider's answer -- but ONLY after the money field has
    // been read, and only when this frame carries none. A frame that states the authoritative total
    // is a terminal record first, whatever else it says, so recognizing the error here can neither
    // drop a bill nor create one. Groq's error frame carries no usage; this ordering is what
    // guarantees the rule rather than the observation.

    // the frame must STATE an error, not merely have room for one. `named_error` is the SSE
    // event NAME -- a positive statement made in the event stream's own syntax, which has no null to
    // be blind to -- so it needs no such guard; the JSON member does, and [`stated_error`] is where
    // that question is answered. Asking `get("error").is_some()` here read `{"error": null}` as a
    // failure and threw away the content delta beside it.
    if usage.is_none() && (named_error || stated_error(&value).is_some()) {
        return Ok(ParsedEvent::ProviderError(
            json_error_detail(&value).unwrap_or_default(),
        ));
    }
    match serde_json::from_value::<StreamChunk>(value) {
        Ok(chunk) => {
            let Some(choice) = chunk.choices.into_iter().next() else {
                return Ok(match usage {
                    Some(usage) => ParsedEvent::Frame {
                        text: String::new(),
                        reasoning: String::new(),
                        usage: Some(usage),
                        model,
                    },
                    None => ParsedEvent::Other,
                });
            };
            let Delta {
                content,
                reasoning,
                reasoning_content,
                reasoning_details,
            } = choice.delta;
            let text = content.unwrap_or_default();
            let reasoning = collect_reasoning(reasoning, reasoning_content, reasoning_details);
            Ok(ParsedEvent::Frame {
                text,
                reasoning,
                usage,
                model,
            })
        }
        // A well-formed provider metadata frame that is not a chat delta does not crash the stream -- but a
        // native total it carries is still the authoritative amount and must not be dropped with it.
        Err(_) => Ok(match usage {
            Some(usage) => ParsedEvent::Frame {
                text: String::new(),
                reasoning: String::new(),
                usage: Some(usage),
                model,
            },
            None => ParsedEvent::Other,
        }),
    }
}

/// Read the frame's own `model` -- an OpenAI-compatible `chat.completion.chunk` states the model that
/// actually produced it. A missing/non-string field is "this frame said nothing about identity", which is
/// not by itself an error (see the refusal in [`stream_upstream`]).
fn frame_served_model(value: &serde_json::Value) -> Option<String> {
    let reported = value.get("model")?.as_str()?.trim();
    (!reported.is_empty()).then(|| reported.to_string())
}

/// Every spelling that may name the model that just answered -- **for the question the caller is asking**.

/// Two layers ask two different things of the same response field, and they are not interchangeable:

/// * `market == None` -- **provider health** ([`super::UpstreamConfig::check_health`], and buyer traffic).
/// "Did my provider serve a model my own config names?" The set is the seller's own declared spellings:
/// the slug it sends (`served_model`), the id it sells under (`frame_model`) and `identity_aliases`.
/// This catches a mistyped `served_model` and a provider that quietly routed the request elsewhere.
/// * `market == Some(id)` -- **market readiness** ([`super::UpstreamConfig::check_market_readiness`]).
/// "Is the model that answered the model THIS MARKET sells?" The set is the market id and
/// `identity_aliases` -- nothing else. This is the verdict that decides whether an offer may rest.

/// **Why `served_model` is absent from the market set, and why that is the whole point (,
/// E2E-ADV-02/L2).** [`OpenAiConfig::model`] is the slug this seller PUTS IN the request, and an
/// OpenAI-compatible provider echoes the model it was asked for. While it sat in the only set there was,
/// the check compared our own request against itself: it was satisfied by construction on every honest
/// provider, so it could certify without ever being able to fire. A real Groq `qwen/qwen3-32b` under a
/// market claiming `adv--real-foreign--...` (production's own shape -- the market id overrides the config
/// frame in [`OpenAiConfig::from_model`]) answered honestly, matched the slug we sent, and readiness posted
/// the SELL. The market id is the identity the buyer pays for (B2/B7), so it is the identity a market
/// verdict measures the answer against.

/// The same model is spelled several ways across this system -- the provider slug (`qwen/qwen3-32b`), the
/// canonical market id in the frame (`qwen--qwen3--32b`) and the registry's display case
/// (`Qwen/Qwen3-32B`). [`crate::registry::model_id_alias`] is the repo's one normalization between them
/// (lowercase + `producer--model--version` -> `producer/model-version`); it is reused for both sets rather
/// than duplicated, so "same model, other spelling" can never read as a substitution -- and for an honest
/// entry the market id and the provider slug normalize to the SAME alias.

/// `identity_aliases` is in both sets because it is the config field this repo already gives an operator to
/// declare "the model I sell self-reports under that name": the buyer reconciles identity through exactly
/// this field ([`crate::buyer::verify`]) and the shipped `models.json` uses it. Reading anything less would
/// refuse an operator whose provider spells the model its own way, and the refusal text names this field.
fn offered_model_aliases(cfg: &OpenAiConfig, market: Option<&str>) -> Vec<String> {
    let mut offered = Vec::with_capacity(2 + cfg.identity_aliases.len());
    match market {
        Some(market_model) => offered.push(crate::registry::model_id_alias(market_model)),
        None => {
            offered.push(crate::registry::model_id_alias(&cfg.model));
            offered.push(crate::registry::model_id_alias(&cfg.frame_model));
        }
    }
    offered.extend(
        cfg.identity_aliases
            .iter()
            .map(|alias| crate::registry::model_id_alias(alias)),
    );
    offered
}

fn collect_reasoning(
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<Vec<ReasoningDetailWire>>,
) -> String {
    let mut parts = Vec::new();
    for value in [reasoning, reasoning_content].into_iter().flatten() {
        if !value.trim().is_empty() {
            parts.push(value);
        }
    }
    for detail in reasoning_details.into_iter().flatten() {
        for value in [detail.text, detail.summary].into_iter().flatten() {
            if !value.trim().is_empty() {
                parts.push(value);
            }
        }
    }
    parts.join("\n")
}

/// A VERBATIM capture of a live Groq `qwen/qwen3-32b` stream, recorded by sending exactly the request
/// this adapter builds for the readiness probe (`stream: true`,
/// `stream_options.include_usage: true`, `temperature: 0`, `max_tokens: 1`). The capture predates the
/// log-probability retirement (`ef6a8611`), so the recorded frames still carry the `logprobs` payload
/// the probe asked for then; `ChatRequest` sends no such key now. Nothing is trimmed, reordered or
/// reformatted.

/// It is the fixture because the hand-written ones were the reason a whole campaign of offline rows
/// went green while every live seller on this model failed readiness: the modelled streams close with
/// ONE terminal record, and this provider closes with the same total stated TWICE -- once on the
/// `finish_reason` chunk (mirrored in `usage` and `x_groq.usage`) and once on the dedicated
/// `stream_options.include_usage` chunk that carries `choices: []`. It also shows the two facts
/// depends on for this provider: content frames carry log probabilities and carry no token ids, so
/// the terminal input/output/total usage record is the only authoritative bill there is.
#[cfg(test)]
/// the starvation capture: the SAME model answering the SAME readiness prompt at the OLD
/// one-token budget (live, 2026-08-12). Four frames, `content` present once and empty, `reasoning`
/// absent entirely, terminal `completion_tokens` = 1 -- a positive bill with nothing delivered.
/// This one must stay REFUSED: UPS-28 is what catches a provider billing without delivering, and
/// the fix is the probe budget, not the guard.
pub(crate) const LIVE_GROQ_GPT_OSS_ONE_TOKEN_CAPTURE: &str = r#"data: {"id":"chatcmpl-8087db6b-4c33-4e5e-ba30-a85377c54ccc","object":"chat.completion.chunk","created":1786530508,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],"x_groq":{"id":"req_01kztr8bqme32rx8ttyjtpk0nw","seed":91721220}}

data: {"id":"chatcmpl-8087db6b-4c33-4e5e-ba30-a85377c54ccc","object":"chat.completion.chunk","created":1786530508,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"length"}],"x_groq":{"id":"req_01kztr8bqme32rx8ttyjtpk0nw","usage":{"queue_time":0.017371826,"prompt_tokens":75,"prompt_time":0.003517995,"completion_tokens":1,"completion_time":0.001015726,"total_tokens":76,"total_time":0.004533721}},"usage":{"queue_time":0.017371826,"prompt_tokens":75,"prompt_time":0.003517995,"completion_tokens":1,"completion_time":0.001015726,"total_tokens":76,"total_time":0.004533721}}

data: {"id":"chatcmpl-8087db6b-4c33-4e5e-ba30-a85377c54ccc","object":"chat.completion.chunk","created":1786530508,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[],"usage":{"queue_time":0.017371826,"prompt_tokens":75,"prompt_time":0.003517995,"completion_tokens":1,"completion_time":0.001015726,"total_tokens":76,"total_time":0.004533721},"service_tier":"on_demand"}

data: [DONE]

"#;

/// The EXACT bytes a live Groq `openai/gpt-oss-20b` returned for the readiness probe request
/// seventeen frames, `content` present once and EMPTY, thirteen
/// frames carrying the whole answer in `reasoning`, terminal `completion_tokens` = 16.
#[cfg(test)]
pub(crate) const LIVE_GROQ_GPT_OSS_READINESS_CAPTURE: &str = r#"data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],"x_groq":{"id":"req_01kztqwa7ke0s9kzg8qqda4hs5","seed":917241710}}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"The","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" user","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" says","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" \"","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"Say","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" OK","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"\".","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" The","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" instruction","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":":","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" \"","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"You","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" are","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"length"}],"x_groq":{"id":"req_01kztqwa7ke0s9kzg8qqda4hs5","usage":{"queue_time":0.01628762,"prompt_tokens":73,"prompt_time":0.003557195,"completion_tokens":16,"completion_time":0.016187042,"total_tokens":89,"total_time":0.019744237,"completion_tokens_details":{"reasoning_tokens":14}}},"usage":{"queue_time":0.01628762,"prompt_tokens":73,"prompt_time":0.003557195,"completion_tokens":16,"completion_time":0.016187042,"total_tokens":89,"total_time":0.019744237,"completion_tokens_details":{"reasoning_tokens":14}}}

data: {"id":"chatcmpl-107bf530-0688-4a0e-b2a3-b21025c15124","object":"chat.completion.chunk","created":1786530113,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[],"usage":{"queue_time":0.01628762,"prompt_tokens":73,"prompt_time":0.003557195,"completion_tokens":16,"completion_time":0.016187042,"total_tokens":89,"total_time":0.019744237,"completion_tokens_details":{"reasoning_tokens":14}},"service_tier":"on_demand"}

data: [DONE]

"#;

#[cfg(test)]
pub(crate) const LIVE_GROQ_READINESS_CAPTURE: &str = r#"data: {"id":"chatcmpl-eda3591e-f053-41b8-b720-f6f6a9b3fed2","object":"chat.completion.chunk","created":1785879521,"model":"qwen/qwen3-32b","system_fingerprint":"fp_d58dbe76cd","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],"x_groq":{"id":"req_01kz7bdsx2ewjbpzjmx50syk52","seed":1312675382}}

data: {"id":"chatcmpl-eda3591e-f053-41b8-b720-f6f6a9b3fed2","object":"chat.completion.chunk","created":1785879521,"model":"qwen/qwen3-32b","system_fingerprint":"fp_d58dbe76cd","choices":[{"index":0,"delta":{"content":"\u003cthink\u003e"},"logprobs":{"content":[{"token":"\u003cthink\u003e","logprob":0,"bytes":[60,116,104,105,110,107,62],"top_logprobs":[{"token":"\u003cthink\u003e","logprob":0,"bytes":[60,116,104,105,110,107,62]},{"token":"\u003c/think\u003e","logprob":-14.689622,"bytes":[60,47,116,104,105,110,107,62]},{"token":"Okay","logprob":-15.249238,"bytes":[79,107,97,121]},{"token":"okay","logprob":-15.942385,"bytes":[111,107,97,121]},{"token":"","logprob":-16.635532,"bytes":null}]}]},"finish_reason":null}]}

data: {"id":"chatcmpl-eda3591e-f053-41b8-b720-f6f6a9b3fed2","object":"chat.completion.chunk","created":1785879521,"model":"qwen/qwen3-32b","system_fingerprint":"fp_d58dbe76cd","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"length"}],"x_groq":{"id":"req_01kz7bdsx2ewjbpzjmx50syk52","usage":{"queue_time":0.1910344,"prompt_tokens":12,"prompt_time":0.000266366,"completion_tokens":1,"completion_time":0.004549186,"total_tokens":13,"total_time":0.004815552}},"usage":{"queue_time":0.1910344,"prompt_tokens":12,"prompt_time":0.000266366,"completion_tokens":1,"completion_time":0.004549186,"total_tokens":13,"total_time":0.004815552}}

data: {"id":"chatcmpl-eda3591e-f053-41b8-b720-f6f6a9b3fed2","object":"chat.completion.chunk","created":1785879521,"model":"qwen/qwen3-32b","system_fingerprint":"fp_d58dbe76cd","choices":[],"usage":{"queue_time":0.1910344,"prompt_tokens":12,"prompt_time":0.000266366,"completion_tokens":1,"completion_time":0.004549186,"total_tokens":13,"total_time":0.004815552},"service_tier":"on_demand"}

data: [DONE]

"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CanonRequest {
        CanonRequest {
            messages: vec![dexdo_proto::ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            params: None,
        }
    }

    async fn start_test_server(body: String) -> (String, tokio::task::JoinHandle<String>) {
        start_test_server_with_response(body, "200 OK", "text/event-stream").await
    }

    /// Read one complete HTTP request (headers plus declared body) off the socket.
    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut request = Vec::new();
        let mut next = [0_u8; 4096];
        loop {
            let read = socket.read(&mut next).await.unwrap();
            assert_ne!(read, 0, "fake provider received a truncated HTTP request");
            request.extend_from_slice(&next[..read]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    async fn start_test_server_with_response(
        body: String,
        status_line: &'static str,
        content_type: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            let header = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}"), server)
    }

    /// A provider that hands the SAME body to the network in two separate writes, split at `split`.
    /// The pause between them is what makes the client see two distinct reads.
    async fn start_split_test_server(
        body: Vec<u8>,
        split: usize,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut socket).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&body[..split]).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            socket.write_all(&body[split..]).await.unwrap();
            socket.flush().await.unwrap();
        });
        (format!("http://{address}"), server)
    }

    /// Run one stream against a body delivered in two writes split at `split`.
    async fn run_split_stream(
        body: Vec<u8>,
        split: usize,
        count: u64,
    ) -> (Result<(), Box<Status>>, Vec<UpstreamEvent>) {
        let (base_url, server) = start_split_test_server(body, split).await;
        let cfg = OpenAiConfig {
            base_url,
            capabilities: no_logprobs(),
            ..OpenAiConfig::default()
        };
        let (tx, mut rx) = mpsc::channel(16);
        let result = stream_upstream(
            &cfg,
            None,
            "secret",
            count,
            &request(),
            &tx,
            DEFAULT_MAX_OUTPUT_TOKENS,
        )
        .await;
        drop(tx);
        server.await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        (result, events)
    }

    async fn run_test_stream_with_capabilities(
        body: String,
        count: u64,
        capabilities: Capabilities,
    ) -> (Result<(), Box<Status>>, Vec<UpstreamEvent>, String) {
        let (base_url, server) = start_test_server(body).await;
        let cfg = OpenAiConfig {
            base_url,
            capabilities,
            ..OpenAiConfig::default()
        };
        let (tx, mut rx) = mpsc::channel(16);
        let model_output_cap =
            resolve_model_output_cap(cfg.capabilities.max_output_tokens, "frame", &cfg.model)
                .expect("test capabilities declare an output cap");
        let result = stream_upstream(
            &cfg,
            None,
            "secret",
            count,
            &request(),
            &tx,
            model_output_cap,
        )
        .await;
        drop(tx);
        let provider_request = server.await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        (result, events, provider_request)
    }

    /// Same as [`run_test_stream_with_capabilities`], for a seller whose configured model is not the
    /// default qwen: a capture from another provider family must be read against the identity that
    /// actually produced it, or the model check fires before the behaviour under test.
    async fn run_test_stream_for_model(
        body: String,
        count: u64,
        model: &str,
    ) -> (Result<(), Box<Status>>, Vec<UpstreamEvent>) {
        let (base_url, server) = start_test_server(body).await;
        let cfg = OpenAiConfig {
            base_url,
            model: model.to_string(),
            frame_model: model.to_string(),
            capabilities: no_logprobs(),
            ..OpenAiConfig::default()
        };
        let (tx, mut rx) = mpsc::channel(16);
        let model_output_cap =
            resolve_model_output_cap(cfg.capabilities.max_output_tokens, "frame", &cfg.model)
                .expect("test capabilities declare an output cap");
        let result = stream_upstream(
            &cfg,
            None,
            "secret",
            count,
            &request(),
            &tx,
            model_output_cap,
        )
        .await;
        drop(tx);
        let _ = server.await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        (result, events)
    }

    async fn run_test_stream(
        body: String,
        count: u64,
    ) -> (Result<(), Box<Status>>, Vec<UpstreamEvent>) {
        let (result, events, _) =
            run_test_stream_with_capabilities(body, count, OpenAiConfig::default().capabilities)
                .await;
        (result, events)
    }

    fn no_logprobs() -> Capabilities {
        Capabilities {
            max_output_tokens: Some(DEFAULT_MAX_OUTPUT_TOKENS),
            ..Capabilities::default()
        }
    }

    fn sse_frame(text: &str, tokens: usize) -> String {
        let logprobs = (0..tokens)
            .map(|_| r#"{"token":"x","logprob":-0.1,"top_logprobs":[]}"#)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"logprobs\":{{\"content\":[{logprobs}]}}}}]}}\n\n",
            serde_json::to_string(text).unwrap()
        )
    }

    fn unstructured_sse_frame(text: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
            serde_json::to_string(text).unwrap()
        )
    }

    fn both_usage_frame(tokens: u64) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":0,\"completion_tokens\":{tokens},\"total_tokens\":{tokens}}},\"x_groq\":{{\"usage\":{{\"prompt_tokens\":0,\"completion_tokens\":{tokens},\"total_tokens\":{tokens}}}}}}}\n\n"
        )
    }

    /// The OpenAI-compatible terminal record: no content shape, one native billing record.
    fn usage_frame(tokens: u64) -> String {
        usage_frame_parts(0, tokens)
    }

    fn usage_frame_parts(input_tokens: u64, output_tokens: u64) -> String {
        let total_tokens = input_tokens.checked_add(output_tokens).unwrap();
        format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{input_tokens},\"completion_tokens\":{output_tokens},\"total_tokens\":{total_tokens}}}}}\n\n"
        )
    }

    /// A terminal record whose `usage` container is spelled by the caller (malformed grids, UPS-18).
    fn raw_usage_frame(usage: &str) -> String {
        format!("data: {{\"choices\":[],\"usage\":{usage}}}\n\n")
    }

    fn accounted_total(events: Vec<UpstreamEvent>) -> u64 {
        events
            .into_iter()
            .map(|event| match event {
                UpstreamEvent::Chunk(_) => 0,
                UpstreamEvent::Usage(usage) => usage.total_tokens,
            })
            .sum()
    }

    fn error_response(
        status: reqwest::StatusCode,
        content_type: Option<&str>,
        body: impl Into<Vec<u8>>,
    ) -> reqwest::Response {
        let mut response = http::Response::builder().status(status);
        if let Some(content_type) = content_type {
            response = response.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        response
            .body(body.into())
            .expect("build error response")
            .into()
    }

    async fn error_message(
        status: reqwest::StatusCode,
        content_type: Option<&str>,
        body: impl Into<Vec<u8>>,
        key: &str,
    ) -> String {
        error_message_for_request(
            status,
            content_type,
            body,
            key,
            &CanonRequest {
                messages: vec![],
                params: None,
            },
        )
        .await
    }

    async fn error_message_for_request(
        status: reqwest::StatusCode,
        content_type: Option<&str>,
        body: impl Into<Vec<u8>>,
        key: &str,
        request: &CanonRequest,
    ) -> String {
        upstream_http_error(error_response(status, content_type, body), key, request)
            .await
            .message()
            .to_string()
    }

    // ---------------------------------------------------------------------------------------
    // (E2E-UPS-41..49): the terminator, the identity latch and the shapes with no output

    // The second batch of provider shapes, after UPS-B3..UPS-B12 (PR1277). Each one names the
    // branch of `stream_response`/`parse_event` it exercises and what would break if that branch
    // gave the other answer -- a mock that passes without pinning a behaviour reports coverage
    // that does not exist.

    // Placed here, between the helpers and the first test, rather than at the end of this module:
    // PR1280 appends its own block at the end, and two appends at one anchor is a conflict
    // for whoever merges second. Nothing here depends on file order.
    // ---------------------------------------------------------------------------------------

    /// One content delta that also states the model that produced it. The helpers
    /// above never state one, so a stream built only from them leaves the identity question
    /// unanswered -- which is exactly the state UPS-46 needs and UPS-45 must move out of.
    fn frame_with_model(text: &str, model: &str) -> String {
        format!(
            "data: {{\"model\":{},\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
            serde_json::to_string(model).unwrap(),
            serde_json::to_string(text).unwrap()
        )
    }

    /// A delta carrying ONLY a tool call: no `content`, no reasoning, no usage. The live shape is
    /// `TOOL_CALL_CAPTURE` (PR1277); this is the same delta with the capability tool's name replaced
    /// by an ordinary one, because ordinary buyer traffic asks for no capability tool.
    fn tool_call_only_frame() -> String {
        concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
            "\"type\":\"function\",\"function\":{\"name\":\"get_weather\",",
            "\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}}]}\n\n"
        )
        .to_string()
    }

    const DONE: &str = "data: [DONE]\n\n";

    /// E2E-ROW: E2E-UPS-41/L0

    /// `[DONE]` is the end of the response, and `ParsedEvent::Done` leaves the read loop by
    /// `break 'provider_stream` -- so every byte after it, complete frames included, is never
    /// parsed. This pins that: the post-terminator delta reaches no buyer, and the bill stays the
    /// total the provider stated while the stream was still open.

    /// If the branch instead kept reading, one of two things would happen and both are worse than
    /// the silence. Either the delta is forwarded, and the buyer receives content after the record
    /// that closed its bill -- the precise hazard UPS-30 exists to refuse; or it is refused, and a
    /// complete, correctly-billed, fully-terminated response becomes a seller-side failure because
    /// the provider appended a byte after saying it was finished.

    /// What is NOT claimed here: that the dropped content was free. The provider's total was stated
    /// before `[DONE]`, so the buyer pays that number and receives everything that preceded it; the
    /// discarded tail is output the buyer never paid for. The exposure runs against the seller, not
    /// the buyer, which is why this is pinned as behaviour and not filed as a defect.
    #[tokio::test]
    async fn content_after_the_done_terminator_is_not_delivered_and_does_not_move_the_bill() {
        let mut body = unstructured_sse_frame("delivered");
        body.push_str(&usage_frame(2));
        body.push_str(DONE);
        body.push_str(&unstructured_sse_frame("after the terminator"));
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert!(result.is_ok(), "{:?}", result.unwrap_err());
        assert_eq!(
            forwarded_text(&events),
            vec!["delivered"],
            "nothing after [DONE] may reach the buyer"
        );
        assert_eq!(
            accounted_total(events),
            2,
            "the bill is the total stated while the stream was open, unchanged by the tail"
        );
    }

    /// E2E-ROW: E2E-UPS-42/L0

    /// The same branch from the money side: a terminal usage record that arrives AFTER `[DONE]` is
    /// never read, so the stream reaches its end with `native_usage == None` and delivered output,
    /// which is the UPS-07 refusal. Nothing is billed and the delivered chunk still reached the
    /// buyer.

    /// If the branch read it, the seller would bill on an aggregate that arrived after the
    /// response's own terminator -- authority from transport position, which is exactly what UPS-29
    /// refuses one frame earlier. A provider could then state a small total, close the stream, and
    /// append a larger one.
    #[tokio::test]
    async fn a_terminal_usage_record_after_the_done_terminator_authorizes_nothing() {
        let mut body = unstructured_sse_frame("delivered");
        body.push_str(DONE);
        body.push_str(&usage_frame(2));
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "without positive terminal usage.completion_tokens");
        assert_eq!(
            forwarded_text(&events),
            vec!["delivered"],
            "the chunk that crossed before the terminator still crossed"
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// E2E-ROW: E2E-UPS-43/L0

    /// `[DONE]` is a transport terminator, not an input-usage record. Even with no visible
    /// output, a request that omitted its provider-native input total fails closed explicitly.
    #[tokio::test]
    async fn an_empty_done_only_stream_without_usage_fails_closed() {
        let (result, events, _) =
            run_test_stream_with_capabilities(DONE.to_string(), 8, no_logprobs()).await;
        assert_status(result, "without terminal billing usage");
        assert!(
            forwarded_text(&events).is_empty(),
            "an empty completion delivers no chunk"
        );
        assert!(
            accounted_amounts(&events).is_empty(),
            "an empty completion emits no accounting event at all, not even a zero"
        );
    }

    /// E2E-ROW: E2E-UPS-44/L0

    /// A tool call is delivered service, and UPS-B8 proves the startup capability probe reads it as
    /// such. This is the OTHER caller: ordinary buyer traffic, where `requirements` is `None`, so
    /// `StartupCapabilityEvent::default()` leaves `tool_call` false and the delta carries no
    /// `content` and no reasoning. `has_output` is therefore false, the frame is skipped, and the
    /// positive total meets `seq == 0` at the end of the stream -- the UPS-28 refusal.

    /// This is the current, deliberate answer, not an oversight: `CanonChunk` has no field a tool
    /// call could travel in, which is why E2E-UPS-36 is `blocked` on defining one. Billing it would
    /// charge the buyer for output the wire provably cannot carry. Delivering it is impossible
    /// today; refusing it and billing nothing is the only remaining honest option, and this pins
    /// which one is chosen so that adding a representation later is a visible change to this row
    /// rather than a silent change of who pays.
    #[tokio::test]
    async fn a_tool_call_only_answer_to_ordinary_traffic_delivers_nothing_and_bills_nothing() {
        let mut body = tool_call_only_frame();
        body.push_str(&usage_frame(5));
        body.push_str(DONE);
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "without delivered output");
        assert!(
            forwarded_text(&events).is_empty(),
            "no canon chunk can carry a tool call today"
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// E2E-ROW: E2E-UPS-45/L0

    /// `model_identity_settled` answers the served-model question ONCE, on the first frame that
    /// states a model, and never re-opens it. A stream whose opening frame names the model this
    /// seller sells is served to its end even if a later frame names another.

    /// If the latch were dropped and every frame re-asked, this stream would be refused midway --
    /// after output had already crossed. That refusal reaches `relay_counting` with delivered
    /// output and no authoritative total, which classifies `AmbiguousUsage` and settles through
    /// `finish_ambiguous`, and that terminal keeps the unresolved remainder COMMITTED: the buyer
    /// loses capacity it paid for and the seller cannot claim what it delivered. Burning both sides
    /// over a field a dishonest seller could rewrite anyway is the trade refused to make.
    #[tokio::test]
    async fn served_model_identity_is_settled_by_the_first_frame_that_states_one() {
        let mut body = frame_with_model("first", DEFAULT_MODEL);
        body.push_str(&frame_with_model("second", "openai/gpt-oss-20b"));
        body.push_str(&usage_frame(3));
        body.push_str(DONE);
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert!(result.is_ok(), "{:?}", result.unwrap_err());
        assert_eq!(forwarded_text(&events), vec!["first", "second"]);
        assert_eq!(accounted_total(events), 3);
    }

    /// E2E-ROW: E2E-UPS-46/L0

    /// The other side of the same money rule: a served-model mismatch found once output has
    /// already been delivered is recorded and the stream is carried to its honest terminal, because
    /// the refusal at that point costs the buyer the capacity it paid for.

    /// Reaching that branch needs the identity to still be open at `seq > 0`, so the first frame
    /// states no model at all -- the shape an endpoint that omits `model` sends -- and the foreign
    /// name arrives on the second. A frame stating nothing is not a mismatch, so it neither settles
    /// the question nor refuses.

    /// The bound is the whole protection. If this branch refused like the `seq == 0` one, a typo in
    /// `served_model` would strand paid capacity on every request rather than print a diagnostic;
    /// if the `seq == 0` branch stopped refusing, a foreign model would reach the book and a buyer.
    /// UPS-B11 pins that first half; this pins the second.
    #[tokio::test]
    async fn a_served_model_mismatch_found_after_delivery_is_carried_to_its_terminal() {
        let mut body = unstructured_sse_frame("already delivered");
        body.push_str(&frame_with_model("and more", "openai/gpt-oss-20b"));
        body.push_str(&usage_frame(4));
        body.push_str(DONE);
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert!(
            result.is_ok(),
            "a mismatch found after delivery must not strand the buyer's paid capacity: {:?}",
            result.unwrap_err()
        );
        assert_eq!(
            forwarded_text(&events),
            vec!["already delivered", "and more"]
        );
        assert_eq!(
            accounted_total(events),
            4,
            "the stream is carried to its own terminal total"
        );
    }

    /// A `200 OK` whose body is a bare JSON error object: no `data:` prefix, no SSE framing at all.
    /// The shape a gateway sends when it has already committed the `text/event-stream` status line
    /// and only then learns the upstream failed.
    const BARE_ERROR_BODY: &str =
        "{\"error\":{\"message\":\"model decommissioned\",\"type\":\"invalid_request_error\"}}\n\n";

    /// E2E-ROW: E2E-UPS-47/L0

    /// `parse_event` recognizes a frame by its `data:` lines and nothing else. A body with none --
    /// the bare error object above -- collects no data, returns `Other` before any JSON is even
    /// looked at, and therefore contributes no delta, no total and no model identity.

    /// That is the branch the whole-stream outcome rests on. If such a frame were read as a delta,
    /// a `{"error":...}` object would have to become content or usage, and the seller would forward
    /// or bill an error; UPS-B7 pins the neighbouring rule that an unrecognized field inside a real
    /// delta is ignored, and this is the same discipline one level out.

    /// Scope: this pins what the frame IS. What the seller then DOES with a stream made only of such
    /// frames is measured in the ignored test below, and is not settled behaviour.

    /// WHAT THIS ROW DELIBERATELY DOES NOT ASSERT, and why the omission is the point. It says only
    /// what a body with no `data:` line parses to. It says nothing about what the SAME bytes parse
    /// to once a provider frames them with `data:`, because that is's subject and the answer
    /// is expected to change: before it, a framed `{"error":...}` object is `Other` -- the delta
    /// parser does not recognise it and it carries no total -- and after it, the same bytes are the
    /// provider's answer, carried out of the stream as its stated reason.

    /// An earlier draft of this test did assert the framed case, as
    /// `matches!(.., Frame {.. } | Other)`, meaning to show that framing is what makes a frame.
    /// That assertion was worse than useless: on this base the framed and unframed forms BOTH parse
    /// to `Other`, so it demonstrated no contrast at all and passed on the wrong arm -- while still
    /// being narrow enough to break the moment the parser grew a variant it had not enumerated.
    /// A test that enumerates a closed set over an evolving enum breaks on growth without ever
    /// having proved anything; this one states a positive fact instead.
    #[test]
    fn a_bare_error_object_with_no_data_prefix_is_not_a_frame() {
        assert!(matches!(
            parse_event(BARE_ERROR_BODY.trim_end()),
            Ok(ParsedEvent::Other)
        ));
    }

    /// E2E-ROW: E2E-UPS-47/L0

    /// The other half of the same boundary: an SSE `event:` line alone does not manufacture a frame.
    /// `parse_event` decides on the `data:` lines and nothing else, and returns before any other
    /// line of the event is given meaning.

    /// This is the assertion the framed case above should have been. It is non-vacuous -- it fails
    /// if an `event:` line is ever allowed to produce a parse on its own -- and it is stable across
    /// which reads `event: error` only to classify a body that already carries `data:`.
    /// Naming a provider error in the event line while sending no data is still nothing.
    #[test]
    fn an_event_line_without_any_data_line_produces_no_frame() {
        for event in [
            "event: error",
            "event: error\nid: 42",
            ": a comment and nothing else",
        ] {
            assert!(
                matches!(parse_event(event), Ok(ParsedEvent::Other)),
                "{event:?} carries no data: line and must parse to nothing"
            );
        }
    }

    /// A provider that answers every connection instead of dying after the first. A one-shot server
    /// cannot see a retry at all: the second attempt finds nobody listening, and "connection
    /// refused" is indistinguishable from "never retried".
    async fn start_repeating_test_server(
        body: String,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::AsyncWriteExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = read_http_request(&mut socket).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            }
        });
        (format!("http://{address}"), requests)
    }

    /// DEFECT, reported and not fixed here.

    /// A `200 OK` carrying a bare JSON error object is treated as transport silence and RE-ASKED.
    /// `stream_response` ends it with `seq == 0` and no `[DONE]`, which sets
    /// `retryable_pre_output_non_answer`, and the caller asks the same question again -- for the
    /// whole supervision cycle -- before the operator is told anything.

    /// Measured, not inferred: against the one-shot fixture this shape consumed the body on attempt
    /// one and then hit a closed listener, surfacing `Unavailable: upstream connect failed` after
    /// ~40s instead of the `DataLoss` the first attempt had already established. A second attempt
    /// is the only way that status can exist.

    /// The money outcome is safe -- nothing is delivered and nothing is billed -- so this is an
    /// operational defect, not a loss path: the cost is a wasted supervision cycle and a diagnosis
    /// that points at the network while the provider has plainly said the model is gone. That is
    /// the same complaint makes about the in-band `event: error` frame; PR1280 fixes it for
    /// bodies that carry `data:` lines, and a body with none reaches none of that machinery.

    /// Left `#[ignore]`d rather than adjusted to today's behaviour: an error answer must be asked
    /// exactly once, and asserting the retry instead would pin the defect as the contract. The
    /// reason string deliberately avoids `EXPECTED TO FAIL`, which is the marker
    /// `ci/run_red_by_design_tests.sh` holds in strict bijection with its own registry; adding an
    /// entry there is the lead's call, together with whether to fix this at all.
    #[tokio::test]
    async fn a_two_hundred_response_whose_body_is_a_bare_error_object_is_asked_exactly_once() {
        let (base_url, requests) = start_repeating_test_server(BARE_ERROR_BODY.to_string()).await;
        let cfg = OpenAiConfig {
            base_url,
            capabilities: no_logprobs(),
            ..OpenAiConfig::default()
        };
        let (tx, mut rx) = mpsc::channel(16);
        let result = stream_upstream(
            &cfg,
            None,
            "secret",
            8,
            &request(),
            &tx,
            DEFAULT_MAX_OUTPUT_TOKENS,
        )
        .await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        assert!(result.is_err(), "an error body is not a served request");
        assert!(forwarded_text(&events).is_empty());
        assert_eq!(accounted_total(events), 0);
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the provider answered; asking it again gets the same answer"
        );
    }

    /// E2E-ROW: E2E-UPS-48/L0

    /// SSE comment frames -- the keep-alives every long-lived stream carries -- reach
    /// `ParsedEvent::Other` and are dropped without touching `seq`. That is load-bearing for a
    /// reason that has nothing to do with text: the `SignalManifest` rides on `seq == 0`, and it is
    /// the only place the buyer learns the declared model and the tokenizer family it verifies
    /// against (B2/B7).

    /// A comment that consumed the counter would leave the manifest on a frame that carries no
    /// output, or nowhere at all, and the buyer would have no declaration to check a substitution
    /// against. UPS-B5 pins the same property against an empty content delta; this pins it against
    /// the frame shape that arrives on every idle connection, and additionally that a comment
    /// between two deltas does not renumber or reorder them.
    #[tokio::test]
    async fn sse_comments_carry_no_output_and_never_consume_the_manifest_slot() {
        let mut body = ": keep-alive\n\n".to_string();
        body.push_str(&unstructured_sse_frame("first"));
        body.push_str(": keep-alive\n\n");
        body.push_str(&unstructured_sse_frame("second"));
        body.push_str(&usage_frame(2));
        body.push_str(DONE);
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert!(result.is_ok(), "{:?}", result.unwrap_err());
        assert_eq!(forwarded_text(&events), vec!["first", "second"]);
        assert_eq!(
            chunk_seqs(&events),
            vec![0, 1],
            "a comment must not renumber the deltas around it"
        );
        assert_eq!(
            declared_manifests(&events),
            vec![(0, DEFAULT_MODEL.to_string())],
            "exactly one manifest, on the first frame that delivered output"
        );
        assert_eq!(accounted_total(events), 2);
    }

    /// E2E-ROW: E2E-UPS-49/L0

    /// `frame_native_usage` reads the standard `usage` and the Groq mirror `x_groq.usage` and has a
    /// distinct arm for each combination. UPS-19 pins the arm where both are present and disagree.
    /// This pins the arm where the standard container is absent and only the vendor mirror states
    /// the total: it is authoritative, and the stream bills it.

    /// If that arm returned `None`, a provider that reports its total only under `x_groq` would end
    /// every stream without a total and be refused by UPS-07 -- output delivered, nothing billable,
    /// the model unsellable for a reason no message would name. That is the same class of outage as
    /// reached through the money field instead of the content channel.
    #[tokio::test]
    async fn a_total_stated_only_in_the_groq_usage_mirror_is_authoritative() {
        let mut body = unstructured_sse_frame("delivered");
        body.push_str(
            "data: {\"choices\":[],\"x_groq\":{\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":4,\"total_tokens\":4}}}\n\n",
        );
        body.push_str(DONE);
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert!(result.is_ok(), "{:?}", result.unwrap_err());
        assert_eq!(forwarded_text(&events), vec!["delivered"]);
        assert_eq!(accounted_total(events), 4);
    }

    #[tokio::test]
    async fn surfaces_http_400_json_error_detail() {
        let message = error_message(
            reqwest::StatusCode::BAD_REQUEST,
            Some("application/json"),
            br#"{"error":{"message":"logprobs are not supported for this model","type":"invalid_request_error"}}"#,
            "unused-key",
        )
        .await;
        assert_eq!(
            message,
            "upstream HTTP 400 Bad Request: logprobs are not supported for this model"
        );
    }

    /// Content that carries no optional arrays is ordinary output: it is forwarded, and the provider's
    /// terminal total bills it.
    #[tokio::test]
    async fn content_without_logprobs_is_forwarded_and_billed_by_terminal_usage() {
        let mut body = unstructured_sse_frame("countable");
        body.push_str(&both_usage_frame(3));
        body.push_str("data: [DONE]\n\n");
        let (result, events) = run_test_stream(body, 8).await;
        result.unwrap();
        assert_eq!(forwarded_text(&events), vec!["countable"]);
        assert_eq!(accounted_total(events), 3);
    }

    /// The terminal total is the amount whatever the optional arrays say.
    #[tokio::test]
    async fn terminal_total_sets_the_amount_not_the_logprob_records() {
        let mut body = sse_frame("three records", 3);
        body.push_str(&usage_frame(5));
        body.push_str("data: [DONE]\n\n");
        let (result, events) = run_test_stream(body, 8).await;
        result.unwrap();
        assert_eq!(accounted_total(events), 5);
    }

    #[tokio::test]
    async fn provider_eof_without_done_bills_nothing() {
        let (result, events) = run_test_stream(sse_frame("truncated", 2), 8).await;
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(status.message().contains("without [DONE]"));
        assert_eq!(
            accounted_total(events),
            0,
            "E2E-UPS-07: a truncated stream never reaches an authoritative amount"
        );
    }

    #[tokio::test]
    async fn provider_eof_with_unfinished_remainder_fails_closed() {
        let mut body = sse_frame("forwarded", 1);
        body.push_str("data: {\"choices\":[");
        let (result, events) = run_test_stream(body, 8).await;
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(status.message().contains("unfinished frame"));
        assert_eq!(
            accounted_total(events),
            0,
            "the incomplete remainder must not create guessed accounting"
        );
    }

    /// One logical output bills the same however the provider chopped it into frames -- the terminal total
    /// is a property of the response, not of the transport.
    #[tokio::test]
    async fn identical_output_count_is_invariant_to_sse_partitioning() {
        let partitions = [
            vec![("abcd", 4)],
            vec![("ab", 2), ("cd", 2)],
            vec![("a", 1), ("b", 1), ("c", 1), ("d", 1)],
        ];
        for partition in partitions {
            let mut body = String::new();
            for (text, tokens) in partition {
                body.push_str(&sse_frame(text, tokens));
            }
            body.push_str(&usage_frame(4));
            body.push_str("data: [DONE]\n\n");
            let (result, events) = run_test_stream(body, 8).await;
            result.unwrap();
            assert_eq!(
                accounted_total(events),
                4,
                "one logical four-token output must not depend on frame count"
            );
        }
    }

    /// A seller whose model reports no per-word probabilities still reaches the provider, streams
    /// its output and is charged the provider's own reported count.

    /// E2E-UPS-01, `tests/e2e/test-specification.md`.

    /// Partial: covers the seller upstream path only, not start-to-settlement payment; the
    /// adversary half (corrupted terminal usage) is E2E-UPS-07.
    #[tokio::test]
    async fn a_model_without_logprobs_is_sellable_and_reaches_the_provider() {
        const KEY_ENV: &str = "DEXDO_861_NO_LOGPROBS_SELLABLE_KEY";
        let mut body = unstructured_sse_frame("served without logprobs");
        body.push_str(&both_usage_frame(3));
        body.push_str("data: [DONE]\n\n");
        let (base_url, server) = start_test_server(body).await;
        let cfg = OpenAiConfig {
            base_url,
            api_key_env: KEY_ENV.into(),
            capabilities: no_logprobs(),
            ..OpenAiConfig::default()
        };
        std::env::set_var(KEY_ENV, "fake-provider-secret");
        let (tx, mut rx) = mpsc::channel(16);
        run(&cfg, None, 8, Some(request()), tx).await;
        std::env::remove_var(KEY_ENV);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap_or_else(|_| {
                panic!("E2E-UPS-01A no-logprobs stream was rejected before provider service")
            }));
        }
        let provider_request = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("E2E-UPS-01A provider server did not complete")
            .expect("E2E-UPS-01A provider task failed");
        assert!(
            provider_request.contains("chat/completions"),
            "the provider must actually be contacted: {provider_request}"
        );
        assert!(
            !events.is_empty(),
            "the output of a no-logprobs model must reach the buyer"
        );
        assert_eq!(
            accounted_total(events),
            3,
            "the provider's own usage figure is the accounted number"
        );
    }

    /// Only the provider's terminal native input + output total authorizes what the seller bills for
    /// one OpenAI-compatible response.

    /// E2E-UPS-02, `tests/e2e/test-specification.md`.

    /// Partial: the OpenAI-compatible protocol only; the Anthropic half of the row is proved by
    /// `crates/dexdo/src/seller/upstream/anthropic.rs`.
    #[tokio::test]
    async fn aggregate_provider_usage_is_a_sufficient_authoritative_count() {
        let mut body = unstructured_sse_frame("forwarded on the provider's own count");
        body.push_str(&both_usage_frame(3));
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_test_stream_with_capabilities(body, 8, no_logprobs()),
        )
        .await
        .expect("E2E-UPS-02 provider stream did not complete");
        if result.is_err() {
            panic!("E2E-UPS-02 aggregate provider usage was rejected");
        }
        assert!(
            !events.is_empty(),
            "text backed by the provider's own usage count is deliverable"
        );
        assert_eq!(
            accounted_total(events),
            3,
            "the accounted total is exactly the provider's reported prompt + completion sum"
        );
    }

    /// The terminal record is consumed exactly once, and never in addition to a chunk-level amount.
    #[tokio::test]
    async fn terminal_usage_is_accounted_once_not_twice() {
        let mut body = sse_frame("one", 1);
        body.push_str(&both_usage_frame(1));
        body.push_str("data: [DONE]\n\n");
        let (result, events) = run_test_stream(body, 8).await;
        result.unwrap();
        assert_eq!(accounted_amounts(&events), vec![1]);
        assert_eq!(accounted_total(events), 1);
    }

    // ---: provider-native terminal input-plus-output usage is the sole billing authority ---

    /// A negative row fails on its OWN reason: assert the exact refusal, never merely "some `DataLoss`".
    #[track_caller]
    fn assert_status(result: Result<(), Box<Status>>, expected: &str) {
        let status = result.expect_err("this stream must be refused");
        assert_eq!(status.code(), tonic::Code::DataLoss, "{}", status.message());
        assert!(
            status.message().contains(expected),
            "expected {expected:?}, got {:?}",
            status.message()
        );
    }

    /// Every separate authoritative-usage amount the adapter emitted, in order.
    fn accounted_amounts(events: &[UpstreamEvent]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|event| match event {
                UpstreamEvent::Usage(usage) => Some(usage.total_tokens),
                UpstreamEvent::Chunk(_) => None,
            })
            .collect()
    }

    /// Buyer-visible text of every forwarded chunk, in order.
    fn forwarded_text(events: &[UpstreamEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                UpstreamEvent::Chunk(chunk) => Some(chunk.text.clone()),
                UpstreamEvent::Usage(_) => None,
            })
            .collect()
    }

    /// UPS-01 + UPS-02: a provider that returns no log probabilities streams its output and its terminal
    /// native total authorizes the whole bill. Nothing about the missing optional data blocks service.
    #[tokio::test]
    async fn provider_without_logprobs_streams_and_terminal_usage_authorizes_billing() {
        let mut body = unstructured_sse_frame("no logprobs ");
        body.push_str(&unstructured_sse_frame("here"));
        body.push_str(&usage_frame(4));
        body.push_str("data: [DONE]\n\n");
        let (result, events, provider_request) =
            run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        result.unwrap();
        assert!(provider_request.starts_with("POST "));
        assert_eq!(forwarded_text(&events), vec!["no logprobs ", "here"]);
        assert_eq!(
            accounted_amounts(&events),
            vec![4],
            "the provider's terminal completion total is the whole bill"
        );
        // Chunks never carry their own money on this path: the terminal record does.
        assert!(events
            .iter()
            .all(|event| matches!(event, UpstreamEvent::Chunk(_) | UpstreamEvent::Usage(_))));
    }

    #[tokio::test]
    async fn nonempty_input_is_billed_without_becoming_part_of_the_output_cap() {
        let mut body = unstructured_sse_frame("one output");
        body.push_str(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":1,\"total_tokens\":101}}\n\n",
        );
        body.push_str(DONE);
        let (result, events, _) = run_test_stream_with_capabilities(body, 1, no_logprobs()).await;
        result.expect("input usage is bounded by the billing grant, not the output cap");
        assert_eq!(forwarded_text(&events), vec!["one output"]);
        assert_eq!(accounted_amounts(&events), vec![101]);
    }

    #[test]
    fn declared_total_and_full_groq_mirror_must_match() {
        let bad_total = serde_json::json!({
            "prompt_tokens": 2,
            "completion_tokens": 3,
            "total_tokens": 6
        });
        assert!(native_billing_usage(Some(&bad_total))
            .unwrap_err()
            .message()
            .contains("does not equal"));

        let conflicting_mirror = serde_json::json!({
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5},
            "x_groq": {"usage": {"prompt_tokens": 1, "completion_tokens": 4, "total_tokens": 5}}
        });
        assert!(frame_native_usage(&conflicting_mirror)
            .unwrap_err()
            .message()
            .contains("disagree"));
    }

    /// UPS-01: the seller must reach the provider. A model configured without log probabilities is served,
    /// not refused before contact.
    #[tokio::test]
    async fn no_logprobs_config_contacts_the_provider_and_completes() {
        const KEY_ENV: &str = "DEXDO_861_NO_LOGPROBS_SERVES_KEY";
        let mut body = unstructured_sse_frame("served");
        body.push_str(&usage_frame(2));
        body.push_str("data: [DONE]\n\n");
        let (base_url, server) = start_test_server(body).await;
        let cfg = OpenAiConfig {
            base_url,
            api_key_env: KEY_ENV.into(),
            capabilities: no_logprobs(),
            ..OpenAiConfig::default()
        };
        std::env::set_var(KEY_ENV, "fake-provider-secret");
        let (tx, mut rx) = mpsc::channel(8);
        run(&cfg, None, 8, Some(request()), tx).await;
        std::env::remove_var(KEY_ENV);
        // Bounded: a build that refuses before contact would otherwise wait forever on a listener that never
        // accepts, and a hung test is not a failing test.
        let provider_request = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("a model without log probabilities must still reach the provider")
            .unwrap();
        assert!(
            provider_request.starts_with("POST "),
            "a model without log probabilities must still reach the provider"
        );
        assert!(
            !provider_request.contains("logprobs"),
            "the unsupported field is still not sent: {provider_request}"
        );
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.expect("serving a no-logprobs model is not an error"));
        }
        assert_eq!(forwarded_text(&events), vec!["served"]);
        assert_eq!(accounted_amounts(&events), vec![2]);
    }

    /// UPS-06: optional log-probability data never changes the amount. Seven records against a terminal total
    /// of three bill three.
    #[tokio::test]
    async fn logprob_record_count_never_changes_the_billed_amount() {
        let mut body = sse_frame("seven records", 7);
        body.push_str(&both_usage_frame(3));
        body.push_str("data: [DONE]\n\n");
        let (result, events) = run_test_stream(body, 8).await;
        result.unwrap();
        assert_eq!(accounted_amounts(&events), vec![3]);
        assert_eq!(accounted_total(events), 3);
    }

    /// UPS-07: output that never reaches a terminal native total bills nothing, even with well-formed
    /// optional arrays on every delta.
    #[tokio::test]
    async fn output_without_terminal_usage_bills_nothing() {
        let mut body = sse_frame("delivered but unbilled", 4);
        body.push_str("data: [DONE]\n\n");
        let (result, events) = run_test_stream(body, 8).await;
        assert_status(result, "without positive terminal usage.completion_tokens");
        assert!(
            accounted_amounts(&events).is_empty(),
            "no separate usage amount may be emitted"
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-19: one terminal record whose two native totals disagree authorizes nothing.
    #[tokio::test]
    async fn disagreeing_native_totals_authorize_nothing() {
        let mut body = unstructured_sse_frame("output");
        body.push_str(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":3,\"total_tokens\":3},\"x_groq\":{\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":4,\"total_tokens\":4}}}\n\n",
        );
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "native usage totals disagree");
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-24: two terminal aggregates that DISAGREE reject the whole request. There is no rule for
    /// choosing between two amounts that both claim to be the total, so neither is billed.
    #[tokio::test]
    async fn contradictory_terminal_aggregates_reject_the_request() {
        let mut body = unstructured_sse_frame("output");
        body.push_str(&usage_frame(3));
        body.push_str(&usage_frame(5));
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "contradictory terminal usage totals");
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-24, the other half: a provider that RESTATES the identical total is not contradicting
    /// itself, and is billed that total exactly once. This is not a hypothetical tolerance -- it is the
    /// shape `LIVE_GROQ_READINESS_CAPTURE` shows a live endpoint actually sending, and refusing it
    /// took every seller on that provider off the market.
    #[tokio::test]
    async fn an_identical_restatement_is_billed_once_not_refused() {
        let mut body = unstructured_sse_frame("output");
        body.push_str(&usage_frame(3));
        body.push_str(&usage_frame(3));
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        result.expect("an unchanged restatement of the same total is not a contradiction");
        assert_eq!(
            accounted_amounts(&events),
            vec![3],
            "the restated total is billed once, not twice"
        );
    }

    /// The regression, at the adapter: the EXACT bytes a live Groq `qwen/qwen3-32b` returned for the
    /// readiness probe request are consumed, and they authorize the provider's own count.
    #[tokio::test]
    async fn the_live_groq_capture_is_consumed_and_authorizes_its_own_count() {
        let (result, events, _) = run_test_stream_with_capabilities(
            LIVE_GROQ_READINESS_CAPTURE.to_string(),
            u64::from(dexdo_core::params::UPSTREAM_HEALTH_PROBE_MAX_TOKENS),
            OpenAiConfig::default().capabilities,
        )
        .await;
        result.expect("the live provider stream must be consumable by the production adapter");
        assert_eq!(
            accounted_amounts(&events),
            vec![13],
            "the bill is the provider's own terminal input + output total"
        );
    }

    /// the regression at the adapter: the EXACT bytes a live Groq `openai/gpt-oss-20b`
    /// returned for the readiness probe. The whole answer arrives in `reasoning` and no frame ever
    /// carries `content`; the seller must consume it and bill the provider's own terminal count,
    /// because reasoning IS delivered output -- refusing it took the entire gpt-oss family off the
    /// market with no offer ever posted.
    #[tokio::test]
    async fn the_live_gpt_oss_reasoning_only_capture_is_consumed_and_authorizes_its_own_count() {
        let (result, events) = run_test_stream_for_model(
            LIVE_GROQ_GPT_OSS_READINESS_CAPTURE.to_string(),
            16,
            "openai/gpt-oss-20b",
        )
        .await;
        result.expect(
            "a reasoning-only provider stream must be consumable by the production adapter",
        );
        assert_eq!(
            accounted_amounts(&events),
            vec![89],
            "the bill is the provider's own terminal input + output total"
        );
    }

    /// UPS-B2, the other half of the class: at a starvation budget the same live model
    /// delivers NOTHING while still reporting a positive terminal total, and that must stay a
    /// refusal. Pinning it here is what stops a future "fix" from relaxing UPS-28 instead of the
    /// budget -- the guard is the only thing standing between a seller and a provider that bills for
    /// output it never sent.
    #[tokio::test]
    async fn a_starved_reasoning_probe_that_delivers_nothing_is_still_refused() {
        let (result, events) = run_test_stream_for_model(
            LIVE_GROQ_GPT_OSS_ONE_TOKEN_CAPTURE.to_string(),
            1,
            "openai/gpt-oss-20b",
        )
        .await;
        assert_status(result, "without delivered output");
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-28: a positive total with no delivered output of any kind is rejected, not billed.
    #[tokio::test]
    async fn terminal_usage_without_any_output_is_rejected() {
        let mut body = usage_frame(5);
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "without delivered output");
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-20: nonempty output terminated by a zero native total is an explicit contradiction.
    #[tokio::test]
    async fn zero_terminal_usage_after_output_is_rejected() {
        let mut body = unstructured_sse_frame("visible output");
        body.push_str(&usage_frame(0));
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "without positive terminal usage.completion_tokens");
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-30: output after a valid terminal aggregate fails the request instead of keeping the earlier bill.
    #[tokio::test]
    async fn output_after_the_terminal_aggregate_is_rejected() {
        let mut body = unstructured_sse_frame("first");
        body.push_str(&usage_frame(2));
        body.push_str(&unstructured_sse_frame("after the bill"));
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "continued after the terminal usage record");
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-29: usage attached to a content-carrying frame is not a terminal record.
    #[tokio::test]
    async fn usage_attached_to_an_output_delta_is_not_terminal() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"paid and printed\"}}],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":2,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        )
        .to_string();
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "attached to an output delta");
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-18: wrong type, negative, fractional and overflowing native totals all authorize nothing, and
    /// none of them falls back to a chunk or optional-array length.
    #[tokio::test]
    async fn malformed_native_totals_authorize_nothing() {
        for (usage, expected) in [
            (
                "{\"prompt_tokens\":0,\"completion_tokens\":\"3\"}",
                "is not a token count",
            ),
            (
                "{\"prompt_tokens\":0,\"completion_tokens\":-3}",
                "is not a token count",
            ),
            (
                "{\"prompt_tokens\":0,\"completion_tokens\":3.5}",
                "is not a token count",
            ),
            (
                "{\"prompt_tokens\":0,\"completion_tokens\":18446744073709551616}",
                "is not a token count",
            ),
            // A usage container that carries no total at all is not a terminal record: the request ends
            // with no authoritative amount, which is UPS-07, not a silent chunk-length fallback.
            (
                "{\"prompt_tokens\":0,\"completion_tokens\":null}",
                "is not a token count",
            ),
            ("{}", "without positive terminal usage.completion_tokens"),
            ("3", "usage is not an object"),
            ("[]", "usage is not an object"),
        ] {
            let mut body = sse_frame("output", 3);
            body.push_str(&raw_usage_frame(usage));
            body.push_str("data: [DONE]\n\n");
            let (result, events) = run_test_stream(body, 8).await;
            assert_status(result, expected);
            assert_eq!(accounted_total(events), 0, "usage={usage}");
        }
    }

    /// A terminal total above the request's own token limit is rejected rather than clamped and paid.
    #[tokio::test]
    async fn terminal_usage_above_the_requested_limit_is_rejected() {
        let mut body = unstructured_sse_frame("output");
        body.push_str(&usage_frame(9));
        body.push_str("data: [DONE]\n\n");
        let (result, events, _) = run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        assert_status(result, "exceeds the requested output limit");
        assert_eq!(accounted_total(events), 0);
    }

    // --- / E2E-UPS-38: the provider's text survives the network read boundary ---

    /// A character encoded in more than one byte, cut in half by the read boundary, must reach the buyer
    /// whole. The same response delivered in one read is the reference answer.
    #[tokio::test]
    async fn multibyte_character_split_across_reads_reaches_the_buyer_intact() {
        // Written as escapes so the source stays ASCII; these are ordinary two-byte letters.
        let text = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}";
        let mut body = unstructured_sse_frame(text);
        body.push_str(&usage_frame(6));
        body.push_str("data: [DONE]\n\n");
        let bytes = body.clone().into_bytes();
        // Every byte position that falls INSIDE a character, not between two of them.
        let splits = (1..bytes.len())
            .filter(|at| !body.is_char_boundary(*at))
            .collect::<Vec<_>>();
        assert_eq!(
            splits.len(),
            6,
            "the fixture must contain split-able characters"
        );

        let (whole_result, whole_events, _) =
            run_test_stream_with_capabilities(body, 8, no_logprobs()).await;
        whole_result.unwrap();
        let reference = forwarded_text(&whole_events);
        assert_eq!(reference, vec![text.to_string()]);

        for split in splits {
            let (result, events) = run_split_stream(bytes.clone(), split, 8).await;
            result.unwrap();
            let delivered = forwarded_text(&events);
            assert_eq!(
                delivered, reference,
                "a read boundary inside a character changed the buyer's text (split={split})"
            );
            assert!(
                !delivered[0].contains('\u{fffd}'),
                "the buyer received a substitution character (split={split}): {:?}",
                delivered[0]
            );
            assert_eq!(accounted_total(events), 6);
        }
    }

    /// Bytes that are not valid text inside a COMPLETE frame are refused explicitly instead of being
    /// silently substituted and forwarded as if they were the provider's answer.
    #[tokio::test]
    async fn invalid_utf8_inside_a_complete_frame_is_refused() {
        let mut body =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"bad\xffbytes\"}}]}\n\n".to_vec();
        body.extend_from_slice(usage_frame(1).as_bytes());
        body.extend_from_slice(b"data: [DONE]\n\n");
        let split = body.len();
        let (result, events) = run_split_stream(body, split, 8).await;
        assert_status(result, "not valid UTF-8");
        assert_eq!(accounted_total(events), 0);
    }

    #[tokio::test]
    async fn surfaces_http_400_text_error_detail() {
        let message = error_message(
            reqwest::StatusCode::BAD_REQUEST,
            Some("text/plain; charset=utf-8"),
            " invalid request\n  detail ",
            "unused-key",
        )
        .await;
        assert_eq!(
            message,
            "upstream HTTP 400 Bad Request: invalid request detail"
        );
    }

    #[tokio::test]
    async fn surfaces_http_404_status_and_detail() {
        let message = error_message(
            reqwest::StatusCode::NOT_FOUND,
            Some("application/problem+json"),
            br#"{"error":{"message":"model not found"}}"#,
            "unused-key",
        )
        .await;
        assert_eq!(message, "upstream HTTP 404 Not Found: model not found");
    }

    #[tokio::test]
    async fn empty_error_body_keeps_status_only() {
        let message = error_message(
            reqwest::StatusCode::BAD_REQUEST,
            Some("text/plain"),
            "",
            "unused-key",
        )
        .await;
        assert_eq!(message, "upstream HTTP 400 Bad Request");
    }

    #[tokio::test]
    async fn malformed_error_body_is_not_echoed() {
        let message = error_message(
            reqwest::StatusCode::BAD_REQUEST,
            Some("application/json"),
            br#"{"error":{"message":"unterminated""#,
            "unused-key",
        )
        .await;
        assert_eq!(
            message,
            "upstream HTTP 400 Bad Request: malformed provider error body omitted"
        );
        assert!(!message.contains("unterminated"));
    }

    #[tokio::test]
    async fn oversized_error_body_is_truncated() {
        let body = format!(
            "{}must-not-appear",
            "x".repeat(UPSTREAM_ERROR_BODY_MAX_BYTES + 256)
        );
        let message = error_message(
            reqwest::StatusCode::BAD_REQUEST,
            Some("text/plain"),
            body,
            "unused-key",
        )
        .await;
        assert!(message.ends_with(TRUNCATED_DETAIL_SUFFIX), "{message}");
        assert!(!message.contains("must-not-appear"), "{message}");
        assert!(
            message.len()
                <= "upstream HTTP 400 Bad Request: ".len() + UPSTREAM_ERROR_DETAIL_MAX_BYTES
        );
    }

    #[tokio::test]
    async fn error_detail_redacts_keys_authorization_and_bearer_credentials() {
        const KEY: &str = "gsk_live_GROQ_API_KEY_value";
        for body in [
            format!(r#"{{"error":{{"message":"provider echoed {KEY}"}}}}"#),
            r#"{"error":{"message":"Authorization: basic-credential"}}"#.to_string(),
            r#"{"error":{"message":"Bearer bearer-credential"}}"#.to_string(),
            r#"{"error":{"message":"api_key=other-provider-key"}}"#.to_string(),
        ] {
            let message = error_message(
                reqwest::StatusCode::BAD_REQUEST,
                Some("application/json"),
                body,
                KEY,
            )
            .await;
            assert!(
                message.ends_with("sensitive provider error detail redacted"),
                "{message}"
            );
            for secret in [
                KEY,
                "basic-credential",
                "bearer-credential",
                "other-provider-key",
            ] {
                assert!(!message.contains(secret), "{message}");
            }
        }
    }

    #[tokio::test]
    async fn truncated_and_partial_request_message_echoes_are_redacted() {
        let request_secret = "A".repeat(UPSTREAM_ERROR_BODY_MAX_BYTES + 512);
        let request = CanonRequest {
            messages: vec![dexdo_proto::ChatMessage {
                role: "user".to_string(),
                content: request_secret.clone(),
            }],
            params: None,
        };
        let truncated = error_message_for_request(
            reqwest::StatusCode::BAD_REQUEST,
            Some("text/plain"),
            request_secret.clone(),
            "unused-key",
            &request,
        )
        .await;
        assert_eq!(
            truncated,
            "upstream HTTP 400 Bad Request: provider error body omitted... [truncated]"
        );
        assert!(!truncated.contains("AAAAAAAA"), "{truncated}");

        let partial = request_secret[..64].to_string();
        let message = error_message_for_request(
            reqwest::StatusCode::BAD_REQUEST,
            Some("text/plain"),
            format!("provider echoed {partial}"),
            "unused-key",
            &request,
        )
        .await;
        assert!(
            message.ends_with("sensitive provider error detail redacted"),
            "{message}"
        );
        assert!(!message.contains(&partial), "{message}");
    }

    #[tokio::test]
    async fn stop_sequence_echo_is_redacted() {
        const STOP: &str = "STOP-PRIVATE-VALUE-4c9";
        let request = CanonRequest {
            messages: vec![],
            params: Some(dexdo_proto::SamplingParams {
                temperature: 0.0,
                max_tokens: 0,
                stop: vec![STOP.to_string()],
                greedy: false,
            }),
        };
        let message = error_message_for_request(
            reqwest::StatusCode::BAD_REQUEST,
            Some("application/json"),
            format!(r#"{{"error":{{"message":"provider rejected stop {STOP}"}}}}"#),
            "unused-key",
            &request,
        )
        .await;
        assert!(
            message.ends_with("sensitive provider error detail redacted"),
            "{message}"
        );
        assert!(!message.contains(STOP), "{message}");
    }

    #[tokio::test]
    async fn non_text_error_body_is_omitted() {
        let message = error_message(
            reqwest::StatusCode::BAD_REQUEST,
            Some("application/octet-stream"),
            b"\0\xffGROQ_API_KEY_VALUE".to_vec(),
            "GROQ_API_KEY_VALUE",
        )
        .await;
        assert_eq!(
            message,
            "upstream HTTP 400 Bad Request: non-text response body omitted"
        );
        assert!(!message.contains("GROQ_API_KEY_VALUE"));
    }

    #[test]
    fn parses_delta_done_and_other() {
        let delta = parse_event("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}").unwrap();
        assert!(
            matches!(delta, ParsedEvent::Frame { text, reasoning, .. } if text == "hi" && reasoning.is_empty())
        );
        assert!(matches!(
            parse_event("data: [DONE]").unwrap(),
            ParsedEvent::Done
        ));
        assert!(matches!(
            parse_event(": keep-alive").unwrap(),
            ParsedEvent::Other
        ));
        // A delta without content (role-only first frame) -> empty string, not accounted.
        let empty = parse_event("data: {\"choices\":[{\"delta\":{}}]}").unwrap();
        assert!(
            matches!(empty, ParsedEvent::Frame { text, reasoning, .. } if text.is_empty() && reasoning.is_empty())
        );
    }

    #[test]
    fn parses_openrouter_reasoning_fields() {
        let raw = parse_event(
            "data: {\"choices\":[{\"delta\":{\"content\":\"391\",\"reasoning\":\"raw \",\"reasoning_content\":\"alias \",\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"detail text\"},{\"type\":\"reasoning.summary\",\"summary\":\"summary text\"},{\"type\":\"reasoning.encrypted\",\"data\":\"redacted\"}]}}]}",
        )
        .unwrap();
        match raw {
            ParsedEvent::Frame {
                text, reasoning, ..
            } => {
                assert_eq!(text, "391");
                assert!(reasoning.contains("raw"));
                assert!(reasoning.contains("alias"));
                assert!(reasoning.contains("detail text"));
                assert!(reasoning.contains("summary text"));
                assert!(!reasoning.contains("redacted"));
            }
            _ => panic!("expected OpenRouter reasoning delta"),
        }
    }

    /// Y3 (regression): complete events are drained in order, the unfinished tail is preserved.
    /// the tail stays a BYTE remainder, so a character split across it survives intact.
    #[test]
    fn drain_keeps_partial_frame() {
        let mut buf = b"data: a\n\ndata: b\n\ndata: part".to_vec();
        let events = drain_complete_events(&mut buf).unwrap();
        assert_eq!(events, vec!["data: a".to_string(), "data: b".to_string()]);
        assert_eq!(
            buf, b"data: part",
            "unfinished frame preserved under the cap"
        );

        let text = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}";
        let frame = format!("data: {text}\n\n").into_bytes();
        let inside = frame.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
        let mut buf = frame[..inside].to_vec();
        assert!(
            drain_complete_events(&mut buf).unwrap().is_empty(),
            "half a character is not a complete frame"
        );
        buf.extend_from_slice(&frame[inside..]);
        assert_eq!(
            drain_complete_events(&mut buf).unwrap(),
            vec![format!("data: {text}")]
        );
    }

    /// a complete frame whose bytes are not text is refused, never repaired.
    #[test]
    fn drain_refuses_a_complete_frame_that_is_not_text() {
        let mut buf = b"data: \xff\n\n".to_vec();
        let status = drain_complete_events(&mut buf).unwrap_err();
        assert_eq!(status.code(), tonic::Code::DataLoss);
        assert!(
            status.message().contains("not valid UTF-8"),
            "{}",
            status.message()
        );
    }

    /// Y3 (negative): an upstream without a `\n\n` separator does not grow the gateway buffer without bound --
    /// when the cap is exceeded the stream closes with `resource_exhausted`, not OOM.
    #[test]
    fn frame_without_separator_is_capped() {
        let mut buf = vec![b'x'; UPSTREAM_SSE_FRAME_MAX_BYTES + 1];
        let err = drain_complete_events(&mut buf).unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn builds_request_forces_model_and_carries_messages() {
        let cfg = OpenAiConfig::default();
        let req = CanonRequest {
            messages: vec![dexdo_proto::ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            params: Some(dexdo_proto::SamplingParams {
                temperature: 0.0,
                max_tokens: 0,
                stop: vec![],
                greedy: false,
            }),
        };
        let body = build_request(&cfg, &req, 8, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(body.model, DEFAULT_MODEL);
        assert_eq!(body.messages.len(), 1);
        assert!(body.stream);
        assert!(body.reasoning.is_none());
        // without this the endpoint never sends the record that is the sole billing authority.
        assert!(body.stream_options.include_usage);
        let json = serde_json::to_string(&body).unwrap();
        assert!(
            json.contains("\"stream_options\":{\"include_usage\":true}"),
            "{json}"
        );
        // Zero-valued sampling fields keep their defaults, while generation is always bounded by the deal.
        assert!(body.temperature.is_none());
        assert_eq!(body.max_tokens, 8);
    }

    #[test]
    fn build_request_clamps_generation_to_request_deal_count_and_model_output_cap() {
        // the outbound limit is `min(client request, deal budget, model output cap)` -- the model cap is
        // the bound that used to be missing, which made every real provider answer `400`.
        const CAP: u32 = 40_960;
        let cfg = OpenAiConfig::default();
        let with_request_limit = |max_tokens| CanonRequest {
            messages: vec![],
            params: Some(dexdo_proto::SamplingParams {
                max_tokens,
                ..Default::default()
            }),
        };

        // The deal budget below every other bound wins.
        assert_eq!(
            build_request(&cfg, &with_request_limit(12), 5, CAP).max_tokens,
            5
        );
        // A client value below every other bound passes through unchanged.
        assert_eq!(
            build_request(&cfg, &with_request_limit(3), 5, CAP).max_tokens,
            3
        );
        // No client value, deal below the cap -> the deal budget.
        assert_eq!(build_request(&cfg, &request(), 7, CAP).max_tokens, 7);
        // No client value, a deal budget above the cap -> the model cap, never `u32::MAX`.
        assert_eq!(
            build_request(&cfg, &request(), u64::MAX, CAP).max_tokens,
            CAP
        );
        assert_eq!(
            build_request(&cfg, &request(), 2_000_000, CAP).max_tokens,
            CAP
        );
        // A client value above the cap is clamped to the cap.
        assert_eq!(
            build_request(&cfg, &with_request_limit(100_000), u64::MAX, CAP).max_tokens,
            CAP
        );
        // The cap is the tightest bound only when it is: a smaller cap wins over a smaller deal budget.
        assert_eq!(build_request(&cfg, &request(), 5, CAP).max_tokens, 5);
        assert_eq!(build_request(&cfg, &request(), 5, 2).max_tokens, 2);
        // Never zero (a provider rejects `max_tokens: 0`).
        assert_eq!(build_request(&cfg, &request(), 0, CAP).max_tokens, 1);
    }

    #[tokio::test]
    async fn outbound_request_carries_the_model_capped_generation_limit() {
        // Wire-level: with no client `max_tokens` and a deal budget of a whole tick, the JSON that actually
        // reaches the provider carries the model cap -- not `u32::MAX` and not `ticks * TICK_SIZE`.
        let (_, _, provider_request) = run_test_stream_with_capabilities(
            "data: [DONE]\n\n".to_string(),
            2_000_000,
            OpenAiConfig::default().capabilities,
        )
        .await;
        assert!(
            provider_request.contains(&format!("\"max_tokens\":{DEFAULT_MAX_OUTPUT_TOKENS}")),
            "outbound body must carry the model output cap: {provider_request}"
        );
        assert!(
            !provider_request.contains(&format!("\"max_tokens\":{}", u32::MAX)),
            "an unbounded generation limit must never reach the provider: {provider_request}"
        );
        assert!(
            !provider_request.contains("\"max_tokens\":2000000"),
            "the deal budget must not reach the provider unclamped: {provider_request}"
        );
    }

    #[tokio::test]
    async fn unknown_model_output_cap_refuses_before_contacting_the_provider() {
        // a model whose output cap is unknown must never be served -- and must fail BEFORE any provider
        // connection, so no request is billed, attempted or half-sent.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let unknown_cap = OpenAiConfig {
            base_url: format!("http://{address}"),
            // `PATH` is always set, so the key precondition cannot mask the capability refusal.
            api_key_env: "PATH".to_string(),
            capabilities: Capabilities {
                max_output_tokens: None,
                ..Capabilities::default()
            },
            ..OpenAiConfig::default()
        };
        let (tx, mut rx) = mpsc::channel(4);
        // Bounded: a regression that only refuses AFTER connecting would otherwise block on a listener that
        // never answers, and a hung test is not a failing test.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run(&unknown_cap, None, 8, Some(request()), tx),
        )
        .await
        .expect("an unknown output cap must be refused without waiting on the provider");

        let Some(Err(status)) = rx.recv().await else {
            panic!("an unknown output cap must be reported as a refusal");
        };
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let message = status.message();
        assert!(
            message.contains(DEFAULT_MODEL),
            "names the model: {message}"
        );
        assert!(
            message.contains("max_output_tokens"),
            "names the remediation: {message}"
        );
        assert!(
            message.contains("models config"),
            "names where to set it: {message}"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept())
                .await
                .is_err(),
            "an unknown output cap must not reach the provider at all"
        );

        // Positive control: the very same setup WITH a declared cap does open a provider connection, so the
        // assertion above is not vacuous.
        let known_cap = OpenAiConfig {
            capabilities: Capabilities {
                max_output_tokens: Some(16),
                ..Capabilities::default()
            },
            ..unknown_cap
        };
        let (tx, _rx) = mpsc::channel(4);
        let serving =
            tokio::spawn(async move { run(&known_cap, None, 8, Some(request()), tx).await });
        let accepted =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept()).await;
        serving.abort();
        assert!(
            accepted.is_ok(),
            "a declared output cap must reach the provider"
        );
    }

    #[tokio::test]
    async fn a_models_config_that_omits_the_output_cap_cannot_serve() {
        // follow-up: the refusal above is asserted on a hand-built config. This one starts from a
        // models **config file** whose `capabilities` block omits `max_output_tokens` -- the shape every live
        // `models.json` fixture had -- and drives it through the production loader, `from_model` and the serve
        // entry, so the whole chain "a config that omits the cap -> a model that cannot deliver a token" is a
        // regression instead of a fact rediscovered on a live run.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let json = format!(
            r#"{{"models":{{"qwen":{{"frame_model":"qwen--qwen3--32b","base_url":"http://{address}",
              "served_model":"qwen/qwen3-32b","api_key_env":"PATH","tokenizer_family":"qwen",
              "price_per_tick":1,"capabilities":{{"logprobs":true,"top_logprobs":5}}}}}}}}"#
        );
        let models = crate::seller::models::ModelsConfig::from_json(&json).expect("fixture parses");
        let cfg = OpenAiConfig::from_model(models.get("qwen").expect("model"), None);
        assert_eq!(
            cfg.capabilities.max_output_tokens, None,
            "an omitted cap is UNKNOWN, never a default"
        );

        let (tx, mut rx) = mpsc::channel(4);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run(&cfg, None, 8, Some(request()), tx),
        )
        .await
        .expect("the refusal must not wait on the provider");
        let Some(Err(status)) = rx.recv().await else {
            panic!("a config without an output cap must be refused");
        };
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(
            status.message().contains("max_output_tokens"),
            "{}",
            status.message()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept())
                .await
                .is_err(),
            "nothing may reach the provider"
        );
    }

    #[tokio::test]
    async fn provider_request_rejection_names_the_model_and_the_sent_limit() {
        // the provider rejects a request the SELLER built, so the error the buyer relays must name the
        // served model and the generation limit that was sent instead of a bare relayed provider line.
        let (base_url, server) = start_test_server_with_response(
            r#"{"error":{"message":"max_tokens must be less than or equal to 40960"}}"#.to_string(),
            "400 Bad Request",
            "application/json",
        )
        .await;
        let cfg = OpenAiConfig {
            base_url,
            ..OpenAiConfig::default()
        };
        let (tx, _rx) = mpsc::channel(4);
        let status = stream_upstream(
            &cfg,
            None,
            "secret",
            u64::MAX,
            &request(),
            &tx,
            DEFAULT_MAX_OUTPUT_TOKENS,
        )
        .await
        .expect_err("a provider 400 fails the stream");
        let _ = server.await;

        let message = status.message();
        // The `upstream HTTP <code>` prefix is preserved: relay policy and the failure classifier parse it.
        assert!(
            message.starts_with("upstream HTTP 400 Bad Request"),
            "{message}"
        );
        assert!(
            message.contains("seller configuration fault"),
            "classified as a seller fault: {message}"
        );
        assert!(
            message.contains(DEFAULT_MODEL),
            "names the model: {message}"
        );
        assert!(
            message.contains(&format!("sent max_tokens={DEFAULT_MAX_OUTPUT_TOKENS}")),
            "names the limit that was sent: {message}"
        );
        assert!(
            message.contains(&format!(
                "capabilities.max_output_tokens={DEFAULT_MAX_OUTPUT_TOKENS}"
            )),
            "names the configured cap to correct: {message}"
        );
        assert!(
            message.contains("max_tokens must be less than or equal to 40960"),
            "keeps the provider's own detail: {message}"
        );
    }

    #[test]
    fn build_request_pins_seed_only_for_greedy_spotcheck() {
        let cfg = OpenAiConfig {
            capabilities: Capabilities {
                max_output_tokens: Some(DEFAULT_MAX_OUTPUT_TOKENS),
                sample_algorithm: SampleAlgorithm::Seed,
            },
            ..OpenAiConfig::default()
        };
        let greedy = CanonRequest {
            messages: vec![],
            params: Some(dexdo_proto::SamplingParams {
                temperature: 0.9,
                max_tokens: 16,
                stop: vec![],
                greedy: true,
            }),
        };
        let body = build_request(&cfg, &greedy, 8, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(body.temperature, Some(0.0));
        assert_eq!(body.seed, Some(REFERENCE_SAMPLE_SEED));
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"seed\":0"), "{json}");

        let regular = CanonRequest {
            messages: vec![],
            params: Some(dexdo_proto::SamplingParams {
                temperature: 0.9,
                max_tokens: 16,
                stop: vec![],
                greedy: false,
            }),
        };
        let body = build_request(&cfg, &regular, 32, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(body.temperature, Some(0.9));
        assert_eq!(body.seed, None);
    }

    #[test]
    fn issue_1951_sample_algorithm_selects_exactly_one_greedy_wire_shape() {
        let expected = [
            ("SEED", "seed", serde_json::json!(0)),
            ("RANDOM_SEED", "random_seed", serde_json::json!(0)),
            ("TOP_K", "top_k", serde_json::json!(1)),
        ];
        let control_fields = ["seed", "random_seed", "top_k"];

        for (algorithm, expected_field, expected_value) in expected {
            let models = crate::seller::models::ModelsConfig::from_json(&format!(
                r#"{{"models":{{"m":{{"frame_model":"vendor--model--v1",
                  "base_url":"https://provider.example/v1","served_model":"provider/model",
                  "api_key_env":"PROVIDER_KEY","tokenizer_family":"vendor","price_per_tick":1,
                  "capabilities":{{"max_output_tokens":256,"sample_algorithm":"{algorithm}"}}}}}}}}"#,
            ))
            .expect("declared sample algorithm parses");
            let cfg = OpenAiConfig::from_model(models.get("m").expect("profile"), None);
            let greedy = CanonRequest {
                messages: vec![],
                params: Some(dexdo_proto::SamplingParams {
                    temperature: 0.9,
                    max_tokens: 16,
                    stop: vec![],
                    greedy: true,
                }),
            };
            let body = serde_json::to_value(build_request(&cfg, &greedy, 8, 256)).unwrap();

            assert_eq!(body["temperature"], 0.0, "algorithm {algorithm}");
            assert_eq!(
                body[expected_field], expected_value,
                "algorithm {algorithm}"
            );
            assert_eq!(
                control_fields
                    .iter()
                    .filter(|field| body.get(**field).is_some())
                    .count(),
                1,
                "algorithm {algorithm} must add exactly one greedy control: {body}"
            );
        }

        let models = crate::seller::models::ModelsConfig::from_json(
            r#"{"models":{"m":{"frame_model":"vendor--model--v1",
              "base_url":"https://provider.example/v1","served_model":"provider/model",
              "api_key_env":"PROVIDER_KEY","tokenizer_family":"vendor","price_per_tick":1,
              "capabilities":{"max_output_tokens":256}}}}"#,
        )
        .expect("omitted sample algorithm parses");
        let cfg = OpenAiConfig::from_model(models.get("m").expect("profile"), None);
        let greedy = CanonRequest {
            messages: vec![],
            params: Some(dexdo_proto::SamplingParams {
                temperature: 0.9,
                max_tokens: 16,
                stop: vec![],
                greedy: true,
            }),
        };
        let body = serde_json::to_value(build_request(&cfg, &greedy, 8, 256)).unwrap();
        assert!(
            control_fields
                .iter()
                .all(|field| body.get(*field).is_none()),
            "the conservative NONE default must add no optional field: {body}"
        );
    }

    #[tokio::test]
    async fn issue_1951_declared_field_rejection_names_the_profile_switch() {
        let (base_url, server) = start_test_server_with_response(
            r#"{"error":{"message":"unsupported field: seed"}}"#.to_string(),
            "400 Bad Request",
            "application/json",
        )
        .await;
        let cfg = OpenAiConfig {
            base_url,
            capabilities: Capabilities {
                max_output_tokens: Some(DEFAULT_MAX_OUTPUT_TOKENS),
                sample_algorithm: SampleAlgorithm::Seed,
            },
            ..OpenAiConfig::default()
        };
        let greedy = CanonRequest {
            messages: vec![],
            params: Some(dexdo_proto::SamplingParams {
                temperature: 0.0,
                max_tokens: 16,
                stop: vec![],
                greedy: true,
            }),
        };
        let (tx, _rx) = mpsc::channel(4);

        let status = stream_upstream(
            &cfg,
            None,
            "secret",
            16,
            &greedy,
            &tx,
            DEFAULT_MAX_OUTPUT_TOKENS,
        )
        .await
        .expect_err("the provider refuses a request carrying seed");
        let _ = server.await;

        let message = status.message();
        assert!(message.contains("unsupported field: seed"), "{message}");
        assert!(
            message.contains("optional field \"seed\""),
            "the refusal must name the exact request field: {message}"
        );
        assert!(
            message.contains("capabilities.sample_algorithm=SEED"),
            "the refusal must name the exact profile switch: {message}"
        );
    }

    #[test]
    fn build_request_enables_openrouter_qwen_reasoning_only_for_exact_model() {
        let cfg = OpenAiConfig {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: "qwen/qwen3-32b".to_string(),
            capabilities: no_logprobs(),
            ..Default::default()
        };
        let body = build_request(
            &cfg,
            &CanonRequest {
                messages: vec![],
                params: None,
            },
            8,
            DEFAULT_MAX_OUTPUT_TOKENS,
        );
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"reasoning\":{\"enabled\":true,\"exclude\":false}"));

        let other_model = OpenAiConfig {
            model: "qwen/qwen3.6-27b".to_string(),
            ..cfg
        };
        let body = build_request(
            &other_model,
            &CanonRequest {
                messages: vec![],
                params: None,
            },
            8,
            DEFAULT_MAX_OUTPUT_TOKENS,
        );
        assert!(body.reasoning.is_none());
    }

    // --- (UPS-B3..UPS-B12): the stream shapes real providers actually send ---

    // WHY THIS SECTION EXISTS. Every provider quirk this client has met so far arrived as a
    // production fire: (one aggregate restated across two frames), (per-token log
    // probabilities), and (a model whose whole answer is reasoning). The live acceptance
    // campaign runs on ONE model family, so "a provider that does not behave like qwen" was absent
    // from acceptance entirely. These rows put that class in a mock, where it costs seconds.

    // The fixtures below are VERBATIM captures from the live Groq OpenAI-compatible endpoint,
    // recorded on 2026-08-12 with exactly the request this adapter builds (`stream: true`,
    // `stream_options.include_usage: true`, `temperature: 0`, `seed: 0`, the canonical
    // `UPSTREAM_HEALTH_PROBE_PROMPT`). Nothing is trimmed, reordered or reformatted. A row that
    // needs a shape no live provider would hand us derives it FROM one of these captures and names
    // the single edit it makes, so the frame layout under test is still the provider's own.

    /// The model that produced [`GPT_OSS_REASONING_ONLY_CAPTURE`] and
    /// [`GPT_OSS_TOOL_REFUSAL_CAPTURE`].
    const GPT_OSS_MODEL: &str = "openai/gpt-oss-20b";

    /// The model that produced [`TOOL_CALL_CAPTURE`].
    const TOOL_CALL_MODEL: &str = "llama-3.3-70b-versatile";

    /// Every `reasoning` fragment [`GPT_OSS_REASONING_ONLY_CAPTURE`] delivers, in order. It is the
    /// buyer-visible output of that stream, so every derived row below must reproduce it exactly: a
    /// transform that drops or mangles the provider's text fails here rather than passing quietly.
    const GPT_OSS_REASONING_FRAGMENTS: [&str; 5] = ["The", " user", " says", ":", " \""];

    /// The provider's own terminal output and billable totals for that capture.
    const GPT_OSS_REASONING_ONLY_OUTPUT: u64 = 8;
    const GPT_OSS_REASONING_ONLY_BILLABLE: u64 = 83;

    /// The provider's own terminal output and billable totals for [`TOOL_CALL_CAPTURE`].
    const TOOL_CALL_OUTPUT: u64 = 5;
    const TOOL_CALL_BILLABLE: u64 = 250;

    /// The terminal total [`LIVE_GROQ_READINESS_CAPTURE`] itself reports.

    /// It is a property of the CAPTURE and of nothing else. This row first spelled it
    /// `UPSTREAM_HEALTH_PROBE_MAX_TOKENS`, which was the budget the request ASKED for, and the two
    /// numbers happened to both be 1 -- so the row went green while asserting something it never
    /// meant. then moved that constant to 64 and the row failed for a reason that had nothing
    /// to do with what it tests. What a provider bills is what the provider reported; a test that has
    /// to be edited when an unrelated constant moves was not testing what it claimed.
    const LIVE_GROQ_READINESS_CAPTURE_BILLABLE: u64 = 13;

    /// A live `openai/gpt-oss-20b` answering the readiness prompt at `max_tokens: 8`. Nine events:
    /// `content` appears exactly ONCE, empty, on the role delta and never again; five deltas carry
    /// the whole answer on the `reasoning` channel, each beside an unknown `channel` field; the
    /// terminal total is stated TWICE (on the `finish_reason` chunk, mirrored in `x_groq.usage`, and
    /// again on the dedicated `include_usage` chunk that closes the stream).
    const GPT_OSS_REASONING_ONLY_CAPTURE: &str = r#"data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],"x_groq":{"id":"req_01kzts16b5e659x3dt0s22t2nr","seed":0}}

data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[{"index":0,"delta":{"reasoning":"The","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[{"index":0,"delta":{"reasoning":" user","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[{"index":0,"delta":{"reasoning":" says","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[{"index":0,"delta":{"reasoning":":","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[{"index":0,"delta":{"reasoning":" \"","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"length"}],"x_groq":{"id":"req_01kzts16b5e659x3dt0s22t2nr","usage":{"queue_time":0.015556816,"prompt_tokens":75,"prompt_time":0.003550521,"completion_tokens":8,"completion_time":0.008310333,"total_tokens":83,"total_time":0.011860854,"completion_tokens_details":{"reasoning_tokens":6}}},"usage":{"queue_time":0.015556816,"prompt_tokens":75,"prompt_time":0.003550521,"completion_tokens":8,"completion_time":0.008310333,"total_tokens":83,"total_time":0.011860854,"completion_tokens_details":{"reasoning_tokens":6}}}

data: {"id":"chatcmpl-1410cb1f-c8f2-45df-8065-2673e7927091","object":"chat.completion.chunk","created":1786531322,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_9b8528b477","choices":[],"usage":{"queue_time":0.015556816,"prompt_tokens":75,"prompt_time":0.003550521,"completion_tokens":8,"completion_time":0.008310333,"total_tokens":83,"total_time":0.011860854,"completion_tokens_details":{"reasoning_tokens":6}},"service_tier":"on_demand"}

data: [DONE]

"#;

    /// A live `llama-3.3-70b-versatile` answering the same prompt with the seller's own
    /// `dexdo_capability_probe` tool forced (`tool_choice`) -- the exact request the startup
    /// capability probe of a `--tools` market builds. Five events: the role delta carries
    /// `content: null`, ONE delta carries the tool call and no content of any kind, and the total is
    /// five tokens. This is what "delivered service" looks like when the delivery is a tool call.
    const TOOL_CALL_CAPTURE: &str = r#"data: {"id":"chatcmpl-141c7512-47b1-4753-ab1f-9676f4654653","object":"chat.completion.chunk","created":1786531175,"model":"llama-3.3-70b-versatile","system_fingerprint":"fp_f8b414701e","choices":[{"index":0,"delta":{"role":"assistant","content":null},"logprobs":null,"finish_reason":null}],"x_groq":{"id":"req_01kztrwpzrefc88rs5d8cj9z6x","seed":0}}

data: {"id":"chatcmpl-141c7512-47b1-4753-ab1f-9676f4654653","object":"chat.completion.chunk","created":1786531175,"model":"llama-3.3-70b-versatile","system_fingerprint":"fp_f8b414701e","choices":[{"index":0,"delta":{"tool_calls":[{"id":"9fp90rpt3","type":"function","function":{"name":"dexdo_capability_probe","arguments":"{}"},"index":0}]},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-141c7512-47b1-4753-ab1f-9676f4654653","object":"chat.completion.chunk","created":1786531175,"model":"llama-3.3-70b-versatile","system_fingerprint":"fp_f8b414701e","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"tool_calls"}],"x_groq":{"id":"req_01kztrwpzrefc88rs5d8cj9z6x","usage":{"queue_time":0.052658695,"prompt_tokens":245,"prompt_time":0.012556885,"completion_tokens":5,"completion_time":0.026087911,"total_tokens":250,"total_time":0.038644796}},"usage":{"queue_time":0.052658695,"prompt_tokens":245,"prompt_time":0.012556885,"completion_tokens":5,"completion_time":0.026087911,"total_tokens":250,"total_time":0.038644796}}

data: {"id":"chatcmpl-141c7512-47b1-4753-ab1f-9676f4654653","object":"chat.completion.chunk","created":1786531175,"model":"llama-3.3-70b-versatile","system_fingerprint":"fp_f8b414701e","choices":[],"usage":{"queue_time":0.052658695,"prompt_tokens":245,"prompt_time":0.012556885,"completion_tokens":5,"completion_time":0.026087911,"total_tokens":250,"total_time":0.038644796},"service_tier":"on_demand"}

data: [DONE]

"#;

    /// The same forced-tool request against `openai/gpt-oss-20b`, live. The model answers in
    /// reasoning, then in content, and then Groq ends a `200 OK` stream with an in-band
    /// `event: error` frame and NO `[DONE]`: seventeen deltas crossed to the buyer and no
    /// authoritative total ever arrived. Nobody would have invented this shape.
    const GPT_OSS_TOOL_REFUSAL_CAPTURE: &str = r#"data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],"x_groq":{"id":"req_01kztrwpd4e199n52y597qwjg8","seed":0}}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"The","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" user","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" says","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":":","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" \"","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"Reply","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" with","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" OK","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":".\"","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" So","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" we","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" just","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" reply","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":" \"","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"OK","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"reasoning":"\".","channel":"analysis"},"logprobs":null,"finish_reason":null}]}

data: {"id":"chatcmpl-9bf89eb9-dcd7-4d13-93fb-35a3c52a1f1f","object":"chat.completion.chunk","created":1786531174,"model":"openai/gpt-oss-20b","system_fingerprint":"fp_ef00694abe","choices":[{"index":0,"delta":{"content":"OK"},"logprobs":null,"finish_reason":null}]}

event: error
data: {"error":{"message":"Tool choice is required, but model did not call a tool","type":"invalid_request_error","code":"tool_use_failed","failed_generation":"","status_code":400}}

"#;

    /// The budget a startup capability probe sends (`CONTENT_PROBE_MAX_TOKENS`) -- the same canonical
    /// number the tool captures above were recorded with.
    fn capability_probe_budget() -> u64 {
        dexdo_core::params::CONTENT_PROBE_MAX_TOKENS
    }

    /// The startup capability probe of a `--tools` market.
    fn tools_probe() -> StartupCapabilityRequirements {
        StartupCapabilityRequirements {
            tools: true,
            think: false,
        }
    }

    /// One stream against a captured provider body, for a seller configured to serve `model`.

    /// `market` selects which question the served-model check answers (`None` = provider health and
    /// buyer traffic; `Some` = market readiness). `requirements` is the startup capability probe
    /// (`None` = ordinary traffic). The default config sells qwen, so a capture from another family
    /// has to be read against the identity that actually produced it, or the served-model check
    /// fires before the shape under test is reached.
    async fn run_provider_capture(
        body: String,
        count: u64,
        model: &str,
        market: Option<&str>,
        requirements: Option<StartupCapabilityRequirements>,
    ) -> (Result<(), Box<Status>>, Vec<UpstreamEvent>) {
        let (base_url, server) = start_test_server(body).await;
        let cfg = OpenAiConfig {
            base_url,
            model: model.to_string(),
            frame_model: model.to_string(),
            capabilities: no_logprobs(),
            ..OpenAiConfig::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        let model_output_cap =
            resolve_model_output_cap(cfg.capabilities.max_output_tokens, "frame", &cfg.model)
                .expect("test capabilities declare an output cap");
        let result = stream_upstream_with_startup_capabilities(
            OpenAiStreamContext {
                cfg: &cfg,
                market,
                requirements,
                count,
                tx: &tx,
            },
            "secret",
            &request(),
            model_output_cap,
        )
        .await;
        drop(tx);
        let _ = server.await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        (result, events)
    }

    /// A refusal is asserted by its OWN code and reason: `is_err` alone would pass for any of the
    /// dozen other ways this adapter can fail, and would keep passing after the behaviour moved.
    #[track_caller]
    fn assert_refusal(result: Result<(), Box<Status>>, code: tonic::Code, expected: &[&str]) {
        let status = result.expect_err("this stream must be refused");
        assert_eq!(status.code(), code, "{}", status.message());
        for part in expected {
            assert!(
                status.message().contains(part),
                "expected {part:?}, got {:?}",
                status.message()
            );
        }
    }

    /// Reasoning of every forwarded chunk, in order (the buyer-visible thinking channel).
    fn forwarded_reasoning(events: &[UpstreamEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                UpstreamEvent::Chunk(chunk) => Some(chunk.reasoning.clone()),
                UpstreamEvent::Usage(_) => None,
            })
            .collect()
    }

    /// The sequence number of every forwarded chunk, in order.
    fn chunk_seqs(events: &[UpstreamEvent]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|event| match event {
                UpstreamEvent::Chunk(chunk) => Some(chunk.seq),
                UpstreamEvent::Usage(_) => None,
            })
            .collect()
    }

    /// Every `SignalManifest` the stream declared, as `(chunk seq, claimed model)`.
    fn declared_manifests(events: &[UpstreamEvent]) -> Vec<(u64, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                UpstreamEvent::Chunk(chunk) => chunk
                    .manifest
                    .as_ref()
                    .map(|manifest| (chunk.seq, manifest.claimed_model.clone())),
                UpstreamEvent::Usage(_) => None,
            })
            .collect()
    }

    /// The SSE events of a capture, in order, without the blank-line separator.
    fn capture_events(capture: &str) -> Vec<&str> {
        capture
            .split("\n\n")
            .filter(|event| !event.is_empty())
            .collect()
    }

    /// The JSON of the first captured frame `pick` accepts -- the provider's own bytes, so a derived
    /// shape differs from the live stream in exactly the one edit its row names.
    fn capture_frame(
        capture: &str,
        pick: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        capture_events(capture)
            .into_iter()
            .filter_map(|event| event.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| {
                serde_json::from_str::<serde_json::Value>(data).expect("a captured frame is JSON")
            })
            .find(pick)
            .expect("the capture carries the frame this row needs")
    }

    fn is_reasoning_delta(value: &serde_json::Value) -> bool {
        value["choices"][0]["delta"].get("reasoning").is_some()
    }

    fn is_tool_call_delta(value: &serde_json::Value) -> bool {
        value["choices"][0]["delta"].get("tool_calls").is_some()
    }

    /// One frame back on the wire.
    fn data_frame(value: &serde_json::Value) -> String {
        format!("data: {value}\n\n")
    }

    /// [`GPT_OSS_REASONING_ONLY_CAPTURE`] with every `reasoning` delta re-spelled the way another
    /// OpenAI-compatible provider spells the SAME channel. Exactly one edit: the field name, and for
    /// `reasoning_details` the wrapper object that field is documented to carry. The text, the frame
    /// layout, the usage records and the terminator stay the provider's own bytes.
    fn respell_reasoning(field: &str, wrap: fn(&str) -> serde_json::Value) -> String {
        let mut body = String::new();
        for event in capture_events(GPT_OSS_REASONING_ONLY_CAPTURE) {
            let data = event
                .strip_prefix("data: ")
                .expect("every captured event is a data frame");
            if data == "[DONE]" {
                body.push_str("data: [DONE]\n\n");
                continue;
            }
            let mut value =
                serde_json::from_str::<serde_json::Value>(data).expect("a captured frame is JSON");
            let delta = value
                .get_mut("choices")
                .and_then(|choices| choices.get_mut(0))
                .and_then(|choice| choice.get_mut("delta"))
                .and_then(serde_json::Value::as_object_mut);
            if let Some(delta) = delta {
                if let Some(reasoning) = delta.remove("reasoning") {
                    let reasoning = reasoning.as_str().expect("a reasoning delta is text");
                    delta.insert(field.to_string(), wrap(reasoning));
                }
            }
            body.push_str(&data_frame(&value));
        }
        body
    }

    /// UPS-B3: the whole answer arrives under `reasoning_content` -- the spelling DeepSeek-style and
    /// vLLM-style endpoints use for the channel Groq spells `reasoning`. Reading one spelling and not
    /// the other is exactly what was: the seller sees a positive terminal total with nothing
    /// delivered, takes the UPS-28 branch, and the model family becomes unsellable with no offer
    /// posted and no diagnosis pointing at the provider's field name.
    #[tokio::test]
    async fn the_whole_answer_in_reasoning_content_is_delivered_and_billed() {
        let body = respell_reasoning("reasoning_content", |text| serde_json::json!(text));
        let (result, events) = run_provider_capture(
            body,
            GPT_OSS_REASONING_ONLY_OUTPUT,
            GPT_OSS_MODEL,
            None,
            None,
        )
        .await;
        result.expect("a reasoning_content-only stream is delivered output, not an empty response");
        assert_eq!(forwarded_reasoning(&events), GPT_OSS_REASONING_FRAGMENTS);
        assert_eq!(
            forwarded_text(&events),
            vec![""; GPT_OSS_REASONING_FRAGMENTS.len()],
            "nothing may be invented into the content channel"
        );
        assert_eq!(
            accounted_amounts(&events),
            vec![GPT_OSS_REASONING_ONLY_BILLABLE],
            "the bill is the provider's own terminal input + output total"
        );
    }

    /// UPS-B4: the same answer under OpenRouter's `reasoning_details[]`, in both of the shapes that
    /// carry text (`.text` and `.summary`). Same verdict as UPS-B3: it is delivered output.
    #[tokio::test]
    async fn the_whole_answer_in_reasoning_details_is_delivered_and_billed() {
        type ReasoningDetailsShape = (&'static str, fn(&str) -> serde_json::Value);
        let shapes: [ReasoningDetailsShape; 2] = [
            (
                "reasoning_details[].text",
                |text| serde_json::json!([{"type": "reasoning.text", "text": text}]),
            ),
            (
                "reasoning_details[].summary",
                |text| serde_json::json!([{"type": "reasoning.summary", "summary": text}]),
            ),
        ];
        for (shape, wrap) in shapes {
            let body = respell_reasoning("reasoning_details", wrap);
            let (result, events) = run_provider_capture(
                body,
                GPT_OSS_REASONING_ONLY_OUTPUT,
                GPT_OSS_MODEL,
                None,
                None,
            )
            .await;
            result.unwrap_or_else(|status| {
                panic!("{shape} is delivered output, not an empty response: {status}")
            });
            assert_eq!(
                forwarded_reasoning(&events),
                GPT_OSS_REASONING_FRAGMENTS,
                "{shape}"
            );
            assert_eq!(
                accounted_amounts(&events),
                vec![GPT_OSS_REASONING_ONLY_BILLABLE],
                "{shape}"
            );
        }
    }

    /// UPS-B5: `content` arrives once, EMPTY, on the role delta, and never again -- the first frame
    /// of every OpenAI-compatible provider measured for, and the only `content` a thinking
    /// model sends at a short budget.

    /// Two things must hold here and neither is obvious. The empty delta is not delivered output, so
    /// it may not be forwarded as a chunk; and it may not consume `seq == 0` either, because that is
    /// the slot the `SignalManifest` rides in. A stream that spent its manifest on an empty frame
    /// would leave the buyer with no declared model and no tokenizer family to verify (B2/B7).
    #[tokio::test]
    async fn an_empty_first_content_delta_delivers_nothing_and_keeps_the_manifest() {
        assert!(
            GPT_OSS_REASONING_ONLY_CAPTURE.contains(r#""delta":{"role":"assistant","content":""}"#),
            "this row is about the empty first content delta; the fixture must carry one"
        );
        let (result, events) = run_provider_capture(
            GPT_OSS_REASONING_ONLY_CAPTURE.to_string(),
            GPT_OSS_REASONING_ONLY_OUTPUT,
            GPT_OSS_MODEL,
            None,
            None,
        )
        .await;
        result.expect("a live reasoning-only stream must be consumable by the production adapter");
        assert_eq!(
            forwarded_reasoning(&events),
            GPT_OSS_REASONING_FRAGMENTS,
            "the empty content delta forwards nothing; the five reasoning deltas are the output"
        );
        assert_eq!(
            chunk_seqs(&events),
            (0..GPT_OSS_REASONING_FRAGMENTS.len() as u64).collect::<Vec<_>>(),
            "the empty delta must not consume a sequence number"
        );
        assert_eq!(
            declared_manifests(&events),
            vec![(0, GPT_OSS_MODEL.to_string())],
            "the manifest rides the first DELIVERED chunk, exactly once"
        );
        assert_eq!(
            accounted_amounts(&events),
            vec![GPT_OSS_REASONING_ONLY_BILLABLE]
        );
    }

    /// UPS-B6, the identical half: this provider already states its one total twice, and a third
    /// restatement says nothing new either. It is held once and billed once. Refusing a repeat is
    /// what did, and it took every seller on that provider off the market.
    #[tokio::test]
    async fn a_third_identical_restatement_of_the_live_total_is_billed_once() {
        let body = GPT_OSS_REASONING_ONLY_CAPTURE.replace(
            "data: [DONE]",
            &format!(
                "{}data: [DONE]",
                usage_frame_parts(75, GPT_OSS_REASONING_ONLY_OUTPUT)
            ),
        );
        let (result, events) = run_provider_capture(
            body,
            GPT_OSS_REASONING_ONLY_OUTPUT,
            GPT_OSS_MODEL,
            None,
            None,
        )
        .await;
        result.expect("an unchanged restatement of the same total is not a contradiction");
        assert_eq!(
            accounted_amounts(&events),
            vec![GPT_OSS_REASONING_ONLY_BILLABLE],
            "three statements of one total are one bill"
        );
    }

    /// UPS-B6, the contradicting half: a further total that DISAGREES authorizes nothing. There is no
    /// rule for choosing between two numbers that each claim to be the whole amount, and picking one
    /// would be inventing the bill.
    #[tokio::test]
    async fn a_disagreeing_further_total_after_the_live_terminal_record_is_refused() {
        let body = GPT_OSS_REASONING_ONLY_CAPTURE.replace(
            "data: [DONE]",
            &format!(
                "{}data: [DONE]",
                usage_frame_parts(75, GPT_OSS_REASONING_ONLY_OUTPUT - 1)
            ),
        );
        let (result, events) = run_provider_capture(
            body,
            GPT_OSS_REASONING_ONLY_OUTPUT,
            GPT_OSS_MODEL,
            None,
            None,
        )
        .await;
        assert_refusal(
            result,
            tonic::Code::DataLoss,
            &["contradictory terminal usage totals"],
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-B7: a usage record attached to a frame that also carries REASONING is not a terminal
    /// record. The existing row proves this for a content delta; for a model whose output is entirely
    /// reasoning, an output frame read as the terminal one would bill the request while the delivery
    /// it carried went unaccounted.
    #[tokio::test]
    async fn usage_attached_to_a_reasoning_delta_is_not_terminal() {
        let mut frame = capture_frame(GPT_OSS_REASONING_ONLY_CAPTURE, is_reasoning_delta);
        frame["usage"] = serde_json::json!({
            "prompt_tokens": 75,
            "completion_tokens": GPT_OSS_REASONING_ONLY_OUTPUT,
            "total_tokens": GPT_OSS_REASONING_ONLY_BILLABLE,
        });
        let body = format!("{}data: [DONE]\n\n", data_frame(&frame));
        let (result, events) = run_provider_capture(
            body,
            GPT_OSS_REASONING_ONLY_OUTPUT,
            GPT_OSS_MODEL,
            None,
            None,
        )
        .await;
        assert_refusal(
            result,
            tonic::Code::DataLoss,
            &["attached to an output delta"],
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-B7, the capability-probe half: the delivered output of a `--tools` probe is the tool call
    /// itself, so a usage record riding the tool-call frame is not terminal either.

    /// The contrast is asserted with it, because it is the whole point: the SAME bytes with the probe
    /// off carry no canonical output at all, and are refused for the other reason. Only a row that
    /// shows both can tell "the tool call counted as delivery" from "something was refused".
    #[tokio::test]
    async fn usage_attached_to_a_tool_call_delta_is_not_terminal() {
        let mut frame = capture_frame(TOOL_CALL_CAPTURE, is_tool_call_delta);
        frame["usage"] = serde_json::json!({
            "prompt_tokens": 245,
            "completion_tokens": TOOL_CALL_OUTPUT,
            "total_tokens": TOOL_CALL_BILLABLE,
        });
        let body = format!("{}data: [DONE]\n\n", data_frame(&frame));
        let (result, events) = run_provider_capture(
            body.clone(),
            capability_probe_budget(),
            TOOL_CALL_MODEL,
            None,
            Some(tools_probe()),
        )
        .await;
        assert_refusal(
            result,
            tonic::Code::DataLoss,
            &["attached to an output delta"],
        );
        assert_eq!(accounted_total(events), 0);

        let (result, events) =
            run_provider_capture(body, capability_probe_budget(), TOOL_CALL_MODEL, None, None)
                .await;
        assert_refusal(result, tonic::Code::DataLoss, &["without delivered output"]);
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-B8: output after the terminal record fails the request instead of keeping the earlier
    /// bill. The frame replayed here is the provider's OWN reasoning delta, lifted out of the live
    /// capture: a reasoning-channel continuation must be caught by the same rule that catches a
    /// content one, or a provider could deliver unbilled output after closing its own account.
    #[tokio::test]
    async fn a_reasoning_delta_after_the_live_terminal_record_is_refused() {
        let replayed = capture_frame(GPT_OSS_REASONING_ONLY_CAPTURE, is_reasoning_delta);
        let body = GPT_OSS_REASONING_ONLY_CAPTURE.replace(
            "data: [DONE]",
            &format!("{}data: [DONE]", data_frame(&replayed)),
        );
        let (result, events) = run_provider_capture(
            body,
            GPT_OSS_REASONING_ONLY_OUTPUT,
            GPT_OSS_MODEL,
            None,
            None,
        )
        .await;
        assert_refusal(
            result,
            tonic::Code::DataLoss,
            &["continued after the terminal usage record"],
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-B9, provider health: the provider names the model that answered, and these are a real
    /// `qwen/qwen3-32b` stream's own bytes read by a seller configured for gpt-oss. The refusal comes
    /// BEFORE any output crosses, and it names both models and the config field that fixes it.
    #[tokio::test]
    async fn a_capture_from_another_model_is_refused_before_any_output() {
        let (result, events) = run_provider_capture(
            LIVE_GROQ_READINESS_CAPTURE.to_string(),
            u64::from(dexdo_core::params::UPSTREAM_HEALTH_PROBE_MAX_TOKENS),
            GPT_OSS_MODEL,
            None,
            None,
        )
        .await;
        assert_refusal(
            result,
            tonic::Code::FailedPrecondition,
            &[
                "upstream served model \"qwen/qwen3-32b\"",
                "not the offered \"openai/gpt-oss-20b\"",
                "identity_aliases",
            ],
        );
        assert!(
            chunk_seqs(&events).is_empty(),
            "a foreign model's output must never reach the buyer"
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-B9, market readiness: the same bytes, asked the question that decides whether an offer may
    /// rest. The market id is the identity the buyer pays for, so the answer is measured against THAT
    /// -- and both directions are asserted here, because a check that only ever fires (or only ever
    /// passes) proves nothing about the case it was written for.
    #[tokio::test]
    async fn market_readiness_measures_the_answer_against_the_market_it_sells() {
        let budget = u64::from(dexdo_core::params::UPSTREAM_HEALTH_PROBE_MAX_TOKENS);
        let (honest, events) = run_provider_capture(
            LIVE_GROQ_READINESS_CAPTURE.to_string(),
            budget,
            DEFAULT_MODEL,
            Some("qwen--qwen3--32b"),
            None,
        )
        .await;
        honest.expect("the market's own model answering it is not a substitution");
        assert_eq!(
            accounted_amounts(&events),
            vec![LIVE_GROQ_READINESS_CAPTURE_BILLABLE],
            "the bill is the CAPTURE's own terminal total, not the budget the request asked for"
        );

        let (foreign, events) = run_provider_capture(
            LIVE_GROQ_READINESS_CAPTURE.to_string(),
            budget,
            DEFAULT_MODEL,
            Some("gptoss--gpt-oss--20b"),
            None,
        )
        .await;
        assert_refusal(
            foreign,
            tonic::Code::FailedPrecondition,
            &[
                "upstream served model \"qwen/qwen3-32b\"",
                "this market sells \"gptoss--gpt-oss--20b\"",
            ],
        );
        assert!(
            chunk_seqs(&events).is_empty(),
            "no offer may rest, and no output may cross, on a foreign model"
        );
        assert_eq!(accounted_total(events), 0);
    }

    /// UPS-B10: a live `200 OK` stream that ends in an in-band provider error, with seventeen deltas
    /// already delivered and no `[DONE]` and no usage record ever sent. Delivered output does not
    /// become billable because it was delivered: without the provider's own terminal total there is
    /// no authoritative amount, and the request fails closed at zero.
    #[tokio::test]
    async fn a_live_stream_that_ends_in_a_provider_error_frame_bills_nothing() {
        assert!(
            !GPT_OSS_TOOL_REFUSAL_CAPTURE.contains("[DONE]"),
            "this row is about a stream with no terminator; the fixture must not carry one"
        );
        let (result, events) = run_provider_capture(
            GPT_OSS_TOOL_REFUSAL_CAPTURE.to_string(),
            capability_probe_budget(),
            GPT_OSS_MODEL,
            None,
            Some(tools_probe()),
        )
        .await;
        assert_refusal(result, tonic::Code::DataLoss, &["without [DONE]"]);
        assert_eq!(
            chunk_seqs(&events).len(),
            17,
            "the output the provider did send crossed to the buyer"
        );
        assert_eq!(
            accounted_total(events),
            0,
            "an unterminated stream never reaches an authoritative amount"
        );
    }

    /// UPS-B11: an unknown field in the delta (`channel`, which gpt-oss sends beside every fragment)
    /// is ignored, and its value never reaches the buyer. A parser that concatenated whatever strings
    /// it found in the delta would ship the provider's routing metadata as model output.
    #[test]
    fn an_unknown_delta_field_is_ignored_and_never_reaches_the_buyer() {
        let live = capture_frame(GPT_OSS_REASONING_ONLY_CAPTURE, is_reasoning_delta);
        assert_eq!(
            live["choices"][0]["delta"]["channel"], "analysis",
            "this row is about an unknown delta field; the fixture must carry one"
        );
        match parse_event(&format!("data: {live}")).unwrap() {
            ParsedEvent::Frame {
                text, reasoning, ..
            } => {
                assert_eq!(text, "");
                assert_eq!(reasoning, GPT_OSS_REASONING_FRAGMENTS[0]);
                assert!(!reasoning.contains("analysis"));
            }
            _ => panic!("a reasoning delta is a frame"),
        }

        // The same provider's other channel: an unknown discriminator must not change what a content
        // delta means either.
        let mut final_channel = live;
        final_channel["choices"][0]["delta"] =
            serde_json::json!({"content": "OK", "channel": "final"});
        match parse_event(&format!("data: {final_channel}")).unwrap() {
            ParsedEvent::Frame {
                text, reasoning, ..
            } => {
                assert_eq!(text, "OK");
                assert_eq!(reasoning, "");
            }
            _ => panic!("a content delta is a frame"),
        }
    }

    /// UPS-B12: the whole delivered service is a tool call -- no content and no reasoning on any
    /// frame. A `--tools` market's startup probe is satisfied by it and bills the provider's own
    /// total, while the call itself is NOT forwarded to the buyer as output: it is evidence of a
    /// capability, not text anybody bought.
    #[tokio::test]
    async fn a_tool_call_without_any_content_satisfies_the_tools_probe_and_bills_its_own_total() {
        assert!(
            !TOOL_CALL_CAPTURE.contains(r#""content":""#),
            "this row is about a stream with no content of any kind"
        );
        let (result, events) = run_provider_capture(
            TOOL_CALL_CAPTURE.to_string(),
            capability_probe_budget(),
            TOOL_CALL_MODEL,
            None,
            Some(tools_probe()),
        )
        .await;
        result.expect("a forced tool call is delivered service for a --tools market");
        assert!(
            chunk_seqs(&events).is_empty(),
            "a capability probe's tool call is not buyer-visible output"
        );
        assert_eq!(
            accounted_amounts(&events),
            vec![TOOL_CALL_BILLABLE],
            "the bill is the provider's own terminal input + output total"
        );
    }

    /// UPS-B12, the negative half: the same probe against a provider that answered in reasoning and
    /// never called the tool is refused, and the diagnostic carries the provider's own numbers so the
    /// operator has something to act on instead of a bare code.
    #[tokio::test]
    async fn a_tools_probe_answered_without_a_tool_call_is_refused_with_the_provider_numbers() {
        let (result, events) = run_provider_capture(
            GPT_OSS_REASONING_ONLY_CAPTURE.to_string(),
            GPT_OSS_REASONING_ONLY_OUTPUT,
            GPT_OSS_MODEL,
            None,
            Some(tools_probe()),
        )
        .await;
        assert_refusal(
            result,
            tonic::Code::FailedPrecondition,
            &[
                "missing tool call",
                "tool_call=false",
                "completion_tokens=8",
                "remove the unsupported flag",
            ],
        );
        assert_eq!(
            accounted_total(events),
            0,
            "a probe that proved nothing bills nothing"
        );
    }

    // ---: an in-band `event: error` is the provider's ANSWER, not our view of a broken stream ---

    /// The exact bytes a live Groq `qwen/qwen3-32b` returned for the seller's OWN `--tools`
    /// capability probe (2026-08-12, the request `build_request_with_startup_capabilities` builds):
    /// HTTP `200 OK`, one `event: error` frame, and then nothing. No `[DONE]`, no delta, no usage
    /// record. The provider states why it stopped; that sentence is the only diagnosis that exists.
    const LIVE_GROQ_INBAND_ERROR_CAPTURE: &str = r#"event: error
data: {"error":{"message":"Failed to call a function. Please adjust your prompt. See 'failed_generation' for more details.","type":"invalid_request_error","code":"tool_use_failed","failed_generation":"","status_code":400}}

"#;

    /// The provider's own sentence inside that capture.
    const LIVE_GROQ_INBAND_ERROR_MESSAGE: &str =
        "Failed to call a function. Please adjust your prompt. See 'failed_generation' for more details.";

    /// Our own failure class for a stream that never terminated. It stays first and unchanged: it is
    /// what the seller did, and adds the provider's half after it rather than replacing it.
    const UNTERMINATED_CLASS: &str = "OpenAI-compatible SSE ended without [DONE]";

    /// A provider that answers EVERY connection with the same body and counts how many times it was
    /// asked. A one-shot server cannot see this defect at all: a retry that finds nobody listening
    /// looks exactly like never having retried.
    async fn start_counting_test_server(
        body: String,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = read_http_request(&mut socket).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            }
        });
        (format!("http://{address}"), requests)
    }

    /// One stream through the WHOLE retry ladder, against a provider that never stops answering.
    /// Returns the refusal, what crossed to the buyer, and how many requests the provider received.
    async fn run_counted_stream(
        body: String,
        count: u64,
    ) -> (Result<(), Box<Status>>, Vec<UpstreamEvent>, usize) {
        let (base_url, requests) = start_counting_test_server(body).await;
        let cfg = OpenAiConfig {
            base_url,
            capabilities: no_logprobs(),
            ..OpenAiConfig::default()
        };
        let (tx, mut rx) = mpsc::channel(16);
        let result = stream_upstream(
            &cfg,
            None,
            "secret",
            count,
            &request(),
            &tx,
            DEFAULT_MAX_OUTPUT_TOKENS,
        )
        .await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        (
            result,
            events,
            requests.load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// A refusal is asserted by its OWN code and by every fragment it must carry. `is_err` would pass
    /// for a dozen other endings, and asserting only our half cannot see the provider's half appear
    /// or disappear.
    #[track_caller]
    fn assert_provider_refusal(result: Result<(), Box<Status>>, expected: &[&str]) {
        let status = result.expect_err("this stream must be refused");
        assert_eq!(status.code(), tonic::Code::DataLoss, "{}", status.message());
        for part in expected {
            assert!(
                status.message().contains(part),
                "expected {part:?}, got {:?}",
                status.message()
            );
        }
    }

    /// defect 1: the operator is told what the SELLER did and what the PROVIDER said, in that
    /// order. Before this, the whole diagnosis was our own class, which sends the reader to debug the
    /// network while the provider is plainly naming a `tool_choice` it could not satisfy.

    /// The refusal itself does not move: same code, same class, nothing billed.
    #[tokio::test]
    async fn an_in_band_provider_error_is_refused_in_the_providers_own_words() {
        let (result, events, _) =
            run_counted_stream(LIVE_GROQ_INBAND_ERROR_CAPTURE.to_string(), 64).await;
        assert_provider_refusal(
            result,
            &[
                UNTERMINATED_CLASS,
                "provider reported:",
                LIVE_GROQ_INBAND_ERROR_MESSAGE,
            ],
        );
        assert_eq!(
            accounted_total(events),
            0,
            "an unterminated stream still bills nothing"
        );
    }

    /// defect 2: an in-band error is an ANSWER. Asking the same question again gets the same
    /// answer, so the provider is asked exactly ONCE.

    /// Before this fix the same capture was treated as transport silence: the seller re-sent the
    /// identical request every `health_interval` for the whole `health_cycle_timeout`, and only then
    /// showed the operator the message from defect 1. The count is the assertion because the count is
    /// what the provider sees and what the operator waits for.
    #[tokio::test]
    async fn an_in_band_provider_error_is_asked_exactly_once() {
        let (result, _, requests) =
            run_counted_stream(LIVE_GROQ_INBAND_ERROR_CAPTURE.to_string(), 64).await;
        assert!(result.is_err(), "the stream is still refused");
        assert_eq!(
            requests, 1,
            "an in-band error is an answer; it must not be asked again"
        );
    }

    /// The same shape once output has already crossed to the buyer: the provider's words still reach
    /// the operator, the delivered chunk still reached the buyer, and nothing is billed because no
    /// authoritative total ever arrived.
    #[tokio::test]
    async fn a_provider_error_after_delivered_output_still_names_the_provider() {
        let mut body = unstructured_sse_frame("delivered");
        body.push_str(LIVE_GROQ_INBAND_ERROR_CAPTURE);
        let (result, events) = run_test_stream(body, 8).await;
        assert_provider_refusal(
            result,
            &[UNTERMINATED_CLASS, LIVE_GROQ_INBAND_ERROR_MESSAGE],
        );
        assert_eq!(forwarded_text(&events), vec!["delivered"]);
        assert_eq!(accounted_total(events), 0);
    }

    /// The provider's text is attacker-controlled, so it is bounded by the SAME policy every other
    /// provider string here is bounded by (`UPSTREAM_ERROR_DETAIL_MAX_BYTES`). A refusal is a log
    /// line and an operator-facing message; an upstream must not be able to make it arbitrarily long.
    #[tokio::test]
    async fn an_oversized_provider_error_is_bounded_not_pasted_whole() {
        let oversized = "z".repeat(UPSTREAM_ERROR_DETAIL_MAX_BYTES * 4);
        let body = format!("event: error\ndata: {{\"error\":{{\"message\":\"{oversized}\"}}}}\n\n");
        let (result, _) = run_test_stream(body, 8).await;
        let status = result.expect_err("an unterminated stream is refused");
        assert!(
            status.message().contains(TRUNCATED_DETAIL_SUFFIX),
            "{}",
            status.message()
        );
        assert!(
            status.message().len()
                <= UNTERMINATED_CLASS.len()
                    + ": provider reported: ".len()
                    + UPSTREAM_ERROR_DETAIL_MAX_BYTES,
            "the provider's half must stay inside the one bound, got {} bytes",
            status.message().len()
        );
    }

    /// The same redaction, too. A provider that echoes the buyer's prompt back at us, or names a
    /// credential, must not have that text copied into a seller-side message: this is exactly the
    /// policy `sanitize_error_detail` already carries for HTTP error bodies, reused rather than
    /// re-invented.
    #[tokio::test]
    async fn a_provider_error_that_echoes_a_secret_or_the_request_is_redacted() {
        for (case, message, forbidden) in [
            ("request echo", "rejected your prompt: hello", "hello"),
            (
                "credential",
                "Authorization header rejected",
                "Authorization",
            ),
        ] {
            let body =
                format!("event: error\ndata: {{\"error\":{{\"message\":\"{message}\"}}}}\n\n");
            let (result, _) = run_test_stream(body, 8).await;
            let status = result.expect_err("an unterminated stream is refused");
            assert!(
                status
                    .message()
                    .contains("sensitive provider error detail redacted"),
                "{case}: {}",
                status.message()
            );
            assert!(
                !status.message().contains(forbidden),
                "{case}: the redacted text leaked into {:?}",
                status.message()
            );
        }
    }

    mod issue_1336 {
        use super::*;

        #[test]
        fn an_empty_provider_detail_leaves_the_failure_class_alone() {
            let detail = unframed_provider_error(br#"{"error":{}}"#)
                .expect("an error object is still the provider's answer");
            assert!(detail.is_empty(), "the provider named no reason");

            let ending = StreamEnding {
                provider_error: Some(detail),
                ..StreamEnding::default()
            };
            let failure = Status::data_loss("provider answered without a readable reason");
            let explained = ending.explain(failure, "secret", &request());
            assert_eq!(
                explained.message(),
                "provider answered without a readable reason",
                "an empty detail is not a secret echo and adds no invented redaction claim"
            );

            assert_eq!(
                sanitize_error_detail("rejected your prompt: hello", "secret", &request()),
                "sensitive provider error detail redacted",
                "a genuine request echo must still be withheld"
            );
        }
    }

    /// Money does not move, and the ordering is what guarantees it: the authoritative total is read
    /// from the raw frame BEFORE the shape, so a frame that states one is a terminal record whatever
    /// else it carries. Recognizing an error frame must not give a provider a way to erase a bill it
    /// already reported for output it already delivered.
    #[tokio::test]
    async fn an_error_frame_that_states_the_total_is_still_the_terminal_record() {
        let mut body = unstructured_sse_frame("delivered");
        body.push_str(
            "event: error\ndata: {\"error\":{\"message\":\"late failure\"},\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}\n\n",
        );
        body.push_str("data: [DONE]\n\n");
        let (result, events) = run_test_stream(body, 8).await;
        result.expect("a frame carrying the provider's own total is the terminal record");
        assert_eq!(
            accounted_amounts(&events),
            vec![5],
            "the bill is the provider's own terminal input + output total"
        );
    }

    /// Both signals a provider uses to say "this is an error frame": the SSE event name, and a
    /// top-level `error` object for gateways that send the body without one.
    #[test]
    fn an_in_band_error_frame_parses_as_the_providers_own_statement() {
        let named = LIVE_GROQ_INBAND_ERROR_CAPTURE.trim_end_matches('\n');
        assert!(
            named.starts_with("event: error\n"),
            "the capture must carry the event name this row is about"
        );
        match parse_event(named).unwrap() {
            ParsedEvent::ProviderError(detail) => {
                assert_eq!(detail, LIVE_GROQ_INBAND_ERROR_MESSAGE)
            }
            _ => panic!("an in-band error frame is the provider's statement"),
        }

        let unnamed = "data: {\"error\":{\"message\":\"no event name here\"}}";
        match parse_event(unnamed).unwrap() {
            ParsedEvent::ProviderError(detail) => assert_eq!(detail, "no event name here"),
            _ => panic!("a body-only error frame is the provider's statement too"),
        }
    }

    /// a `200 OK` whose whole body is a bare JSON error object is an ANSWER, and an answer is
    /// not silence.

    /// The defect these cover: such a body carries no `data:` line, so it reaches no frame, the
    /// stream ends at `seq == 0`, and that used to set `retryable_pre_output_non_answer` -- the
    /// classification reserved for a body that vanished. The seller then re-asked a provider that
    /// had already replied, which on an open deal spends the buyer's delivery window, and the
    /// diagnosis the operator finally read described our own second attempt rather than the
    /// provider's stated reason.
    mod issue_1301 {
        use super::*;

        /// Drive the real entry point -- `stream_upstream`, the retry loop and all -- against a
        /// provider that answers EVERY connection, so a retry is a countable fact rather than an
        /// inference. A one-shot server cannot see this: its second attempt finds nobody listening,
        /// and "connection refused" is indistinguishable from "never retried".

        /// Three separable claims, because a fix that merely stopped retrying everything would
        /// satisfy only the first:

        /// 1. the provider is asked exactly ONCE;
        /// 2. it returns promptly -- inside a single health interval, where the un-fixed path spent
        /// the whole supervision cycle sleeping between attempts;
        /// 3. the operator is told the provider ANSWERED and is given its words, not a connection
        /// error. This is the half the issue calls the real cost: a diagnosis the operator cannot
        /// tell apart from an outage is worth little.
        #[tokio::test]
        async fn a_bare_json_error_body_under_two_hundred_is_asked_once_and_reported_as_an_answer()
        {
            let (base_url, requests) =
                start_repeating_test_server(BARE_ERROR_BODY.to_string()).await;
            let cfg = OpenAiConfig {
                base_url,
                capabilities: no_logprobs(),
                ..OpenAiConfig::default()
            };
            let (tx, mut rx) = mpsc::channel(16);
            let started = std::time::Instant::now();
            let result = stream_upstream(
                &cfg,
                None,
                "secret",
                8,
                &request(),
                &tx,
                DEFAULT_MAX_OUTPUT_TOKENS,
            )
            .await;
            let elapsed = started.elapsed();
            drop(tx);
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event.unwrap());
            }

            let status = result.expect_err("an error body is not a served request");
            assert_eq!(
                requests.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "the provider answered; asking it again gets the same answer"
            );
            assert!(
                elapsed < SellerLivenessParams::canonical().health_interval,
                "an answered request must not spend a retry wait: took {elapsed:?}"
            );

            let message = status.message();
            assert!(
                message.contains("provider error object"),
                "the message must name what was actually seen: {message}"
            );
            assert!(
                message.contains("model decommissioned"),
                "the provider's own reason is the part that tells an operator what to do: {message}"
            );
            assert!(
                !message.contains("connect failed") && status.code() != tonic::Code::Unavailable,
                "an answered request must not be reported as an outage: {:?} {message}",
                status.code()
            );

            assert!(forwarded_text(&events).is_empty());
            assert_eq!(accounted_total(events), 0);
        }

        /// The distinction is the fix, so the other side of it is pinned too: everything that is NOT
        /// a stated error answer stays the retryable transport non-answer it has always been.

        /// Asserted through `StreamEnding` itself, which is where the verdict is taken -- so this
        /// fails if the retry gate is ever satisfied by something weaker than the provider saying
        /// why it stopped. A truncated prefix is in the list deliberately: reading half an error
        /// object as an answer would refuse to retry a deal a genuine transport blip could still
        /// have delivered.
        #[test]
        fn only_a_stated_error_body_is_an_answer_and_everything_else_stays_retryable() {
            let answers = [
                BARE_ERROR_BODY,
                "{\"error\":\"flat string reason\"}",
                "  \n{\"error\":{\"code\":\"rate_limited\"}}\n  ",
            ];
            for body in answers {
                let mut ending = StreamEnding::default();
                ending.observe_body(body.as_bytes());
                ending.transport_non_answer();
                assert!(
                    ending.answered_without_framing,
                    "the provider stated a reason: {body:?}"
                );
                assert!(
                    !ending.retryable_pre_output_non_answer,
                    "an answered request must not be re-asked: {body:?}"
                );
            }

            let non_answers = [
                "",
                "   \n\n",
                // A stream that started and then stopped: still transport silence.
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                // A frame-shaped error body IS handled -- one layer up, by `parse_event`.
                "data: {\"error\":{\"message\":\"in band\"}}\n\n",
                // JSON, but stating no reason.
                "{\"choices\":[]}",
                // `Value::get` answers "is the key present", not "does it hold anything",
                // so `is_some()` reads these as an answer. A key holding null states no reason,
                // and several OpenAI-compatible proxies put exactly this on every chunk.
                "{\"error\":null}",
                "{\"error\":null,\"choices\":[]}",
                // Half an error object, as a body cut short by the read cap or the network.
                "{\"error\":{\"message\":\"model decom",
                "<html><body>502 Bad Gateway</body></html>",
            ];
            for body in non_answers {
                let mut ending = StreamEnding::default();
                ending.observe_body(body.as_bytes());
                ending.transport_non_answer();
                assert!(
                    !ending.answered_without_framing,
                    "nothing here states a reason: {body:?}"
                );
                assert!(
                    ending.retryable_pre_output_non_answer,
                    "a body that stated nothing is still transport silence: {body:?}"
                );
            }
        }

        /// The body prefix this adapter is willing to read from an untrusted provider is the one it
        /// already uses for a non-2xx error body, and it is a hard cap: a hostile `200` cannot grow
        /// the gateway's memory by streaming an endless "error object".
        #[test]
        fn the_observed_body_is_bounded_by_the_existing_error_body_cap() {
            let mut ending = StreamEnding::default();
            for _ in 0..16 {
                ending.observe_body(&vec![b'x'; UPSTREAM_ERROR_BODY_MAX_BYTES]);
            }
            assert_eq!(ending.unframed_body.len(), UPSTREAM_ERROR_BODY_MAX_BYTES);
            ending.transport_non_answer();
            assert!(ending.retryable_pre_output_non_answer);
        }
    }

    /// an `error` member that is present and HOLDS NOTHING states no error.

    /// The defect these cover: `serde_json::Value::get` answers "is this key present", not "does it
    /// hold anything", so `value.get("error").is_some()` was true for `{"error": null}` -- the shape
    /// a gateway emits when it serialises a fixed envelope instead of omitting its absent members,
    /// which several OpenAI-compatible proxies put on every chunk they send. A chunk carrying BOTH
    /// `"error": null` and a real content delta became `ProviderError("")`: the delta was discarded
    /// and the stream ended, on the FIRST content chunk of every request, against a provider the
    /// seller was serving correctly. The operator was shown a provider error with no words, which is
    /// exactly what an outage looks like.

    /// Both directions are pinned, because a fix that merely stopped believing `error` members would
    /// satisfy only the first: a null member must deliver its content, and a STATED error must still
    /// terminate the stream in the provider's own words.
    mod issue_1318 {
        use super::*;

        /// A gateway that serialises its envelope rather than omitting absent members: every chunk
        /// carries `"error": null` beside its real delta.
        fn null_error_frame(text: &str) -> String {
            format!(
                "data: {{\"error\":null,\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
                serde_json::to_string(text).unwrap()
            )
        }

        /// The defect itself, at the parser: the content survives.
        #[test]
        fn a_null_error_member_beside_a_delta_is_a_frame_and_keeps_its_content() {
            let frame = "data: {\"error\":null,\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}";
            match parse_event(frame).unwrap() {
                ParsedEvent::Frame { text, usage, .. } => {
                    assert_eq!(text, "hi", "the delta beside a null `error` is the output");
                    assert_eq!(usage, None, "this frame states no total");
                }
                ParsedEvent::ProviderError(detail) => panic!(
                    "a present-but-null `error` was read as a stated error, discarding the delta \
                     (reason given to the operator: {detail:?})"
                ),
                _ => panic!("a chunk carrying a content delta is a frame"),
            }
        }

        /// The opposite direction, at the same site: a member that HOLDS an error still terminates,
        /// and still does so when a delta sits beside it. Recognizing content must not become a way
        /// for a provider to smuggle a stated failure past the stream's ending.
        #[test]
        fn a_stated_error_object_beside_a_delta_is_still_the_providers_statement() {
            let frame = "data: {\"error\":{\"message\":\"real upstream failure\"},\
                         \"choices\":[{\"delta\":{\"content\":\"never delivered\"}}]}";
            match parse_event(frame).unwrap() {
                ParsedEvent::ProviderError(detail) => {
                    assert_eq!(detail, "real upstream failure");
                }
                _ => panic!("an `error` member that holds an object is a stated error"),
            }
        }

        /// The SSE event NAME is a positive statement made in the stream's own syntax -- it has no
        /// null to be blind to -- so it is untouched by this: `event: error` means error whatever the
        /// JSON beside it holds. This direction passed before the fix and must keep passing after it.
        #[test]
        fn an_event_named_error_stays_an_error_even_with_a_null_error_member() {
            let frame =
                "event: error\ndata: {\"error\":null,\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}";
            match parse_event(frame).unwrap() {
                ParsedEvent::ProviderError(_) => {}
                _ => panic!("the SSE event name is the provider naming this frame an error"),
            }
        }

        /// The second site that asked the same question ([`json_error_detail`], reached here through
        /// the non-2xx body reader): a null `error` member is not the container to read the reason
        /// FROM either, so the words beside it are the reason. Before this, `error` holding nothing
        /// made the reader look for a message INSIDE nothing, and the operator was told only that a
        /// body had been omitted.
        #[tokio::test]
        async fn a_null_error_member_does_not_hide_the_reason_stated_beside_it() {
            let message = error_message(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                Some("application/json"),
                br#"{"error":null,"message":"rate limit reached for qwen/qwen3-32b"}"#,
                "unused-key",
            )
            .await;
            assert_eq!(
                message,
                "upstream HTTP 429 Too Many Requests: rate limit reached for qwen/qwen3-32b",
                "a member holding nothing is absent, so the reason is read from the body itself"
            );
        }

        /// What it cost, end to end through the real entry point: a deal the seller was serving
        /// correctly is served to completion, rather than stopping on its first content chunk.
        #[tokio::test]
        async fn a_provider_that_puts_a_null_error_on_every_chunk_is_served_to_completion() {
            let mut body = null_error_frame("hello ");
            body.push_str(&null_error_frame("world"));
            body.push_str(&usage_frame(4));
            body.push_str(DONE);
            let (result, events) = run_test_stream(body, 8).await;
            result.expect("a null `error` member is not a provider failure");
            assert_eq!(
                forwarded_text(&events),
                vec!["hello ", "world"],
                "every delta reaches the buyer"
            );
            assert_eq!(
                accounted_total(events),
                4,
                "the provider's own terminal total still bills the stream"
            );
        }

        /// And the same stream still ENDS when the provider finally states a reason: delivered output
        /// stays delivered, the refusal carries the provider's words, and nothing is billed because
        /// no authoritative total ever arrived.
        #[tokio::test]
        async fn a_stated_error_still_ends_a_stream_whose_chunks_carry_null_error_members() {
            let mut body = null_error_frame("delivered");
            body.push_str("data: {\"error\":{\"message\":\"real upstream failure\"}}\n\n");
            let (result, events) = run_test_stream(body, 8).await;
            assert_provider_refusal(result, &[UNTERMINATED_CLASS, "real upstream failure"]);
            assert_eq!(forwarded_text(&events), vec!["delivered"]);
            assert_eq!(
                accounted_total(events),
                0,
                "an unterminated stream bills nothing"
            );
        }
    }
}
