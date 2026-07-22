//! Real OpenAI-compatible upstream.
//! The gateway connects to an OpenAI-compatible API(by default **Groq**, `qwen/qwen3-32b`),
//! sends the buyer's canonical request(R1), reads the **streaming SSE** and normalizes each
//! delta into a `CanonChunk` incrementally(R6). Accounting is done after normalization from token-level
//! signals, then converted to ticks with canonical `TICK_SIZE`; an SSE event is not a tick.
//! The key is taken **from the environment at runtime**([`api_key`]) and is never stored/logged
//! . Without a key the adapter does not start -- the stream
//! closes with `Status::failed_precondition`, which yields a clean skip in e2e.

use super::{chunk_with_structured_accounting, UpstreamEvent};
use crate::seller::models::{Capabilities, ModelConfig};
use dexdo_proto::{CanonChunk, CanonRequest, SignalManifest, TokenLogprobs, TopLogprob};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tonic::Status;

/// Name of the env variable holding the upstream key(Groq OpenAI-compatible API). Creds come from "seahorse".
pub const API_KEY_ENV: &str = "GROQ_API_KEY";

/// Default base of the Groq OpenAI-compatible API.
pub const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Default model id -- Qwen 32B on Groq(canonical id from the Groq model list).
pub const DEFAULT_MODEL: &str = "qwen/qwen3-32b";

/// Real upstream configuration. Carries **only** non-confidential parameters (URL/model id/
/// env-key name/tokenizer family/capabilities). The key itself is NOT stored here -- it is read from
/// the environment at request time via [`OpenAiConfig::api_key_env`]. Operationally built from a
/// model config entry; `Default` is the built-in demo default
/// (Groq/qwen) for tests and `live_groq`.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    /// Base URL of the OpenAI-compatible API(without the trailing `/chat/completions`).
    pub base_url: String,
    /// Upstream model id(`served_model`; forced by the market R1, the buyer's `model` is not trusted).
    /// Sent to the upstream(Groq); an internal detail, NOT the on-wire declared model.
    pub model: String,
    /// **Canonical market id**(`producer--model--version`, e.g. `qwen--qwen3--32b`) -- the protocol-facing model
    /// identity the buyer paid for(B2) and verifies the declaration against. Declared as
    /// `claimed_model`. It is DISTINCT from [`Self::model`](the upstream slug like `qwen/qwen3-32b`): the buyer's
    /// frame is canonical, so declaring the served slug here would false-trip the substitution check.
    pub frame_model: String,
    /// **Test-only seam:** declare a DIFFERENT `claimed_model` in the
    /// `SignalManifest` while the real upstream still serves [`Self::model`]. `None` (default / production path,
    /// [`Self::from_model`]) -> `claimed_model == frame_model`(honest declaration of the canonical market id).
    /// `Some(name)` -> emit `name` as the declared model while serving `model` -- reproduces a served!=declared
    /// substitution so the buyer's content gate(B8 + B7-full) can be proven against a REAL divergent upstream.
    pub claimed_model_override: Option<String>,
    /// Name of the env variable holding the key -- **per-model/provider**, not a single global one.
    pub api_key_env: String,
    /// Tokenizer family for `SignalManifest` -- from config, not a substring hardcode.
    pub tokenizer_family: String,
    /// Upstream capabilities.
    pub capabilities: Capabilities,
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
                logprobs: true,
                top_logprobs: Some(5),
            },
        }
    }
}

impl OpenAiConfig {
    /// Build from a model config entry -- the operational CLI path(`--model`).
    pub fn from_model(m: &ModelConfig) -> Self {
        Self {
            base_url: m.base_url.clone(),
            model: m.served_model.clone(),
            // The on-wire declared model is the CANONICAL frame(what the buyer paid for / verifies against),
            // not the upstream served slug -- else the buyer's check false-trips a substitution.
            frame_model: m.frame_model.clone(),
            // Production path: honest declaration(`claimed_model == frame_model`). The override is test-only.
            claimed_model_override: None,
            api_key_env: m.api_key_env.clone(),
            tokenizer_family: m.tokenizer_family.clone(),
            capabilities: m.capabilities.clone(),
        }
    }
}

/// Read the upstream key from the environment(runtime) by the **env-variable name from the model config**
/// . `None` means no key(the live path is unavailable). The value is never
/// logged and never persisted to disk.
pub fn api_key(env_name: &str) -> Option<String> {
    std::env::var(env_name).ok().filter(|k| !k.is_empty())
}

// --- Request/response shape of OpenAI-compatible chat-completions(subset for the adapter) ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    // R2 +: logprobs are requested ONLY if the model supports them
    // . Otherwise the field is NOT sent(`None` -> skip): strict
    // OpenAI-compatible endpoints answer `400` on an unsupported field and drop the stream. Default is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    logprobs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_logprobs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningRequest>,
}

#[derive(Serialize)]
struct ReasoningRequest {
    enabled: bool,
    exclude: bool,
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
    // Delta logprobs(OpenAI/Groq format): `content[]` with the chosen token + top-k alternatives.
    #[serde(default)]
    logprobs: Option<ChoiceLogprobs>,
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

#[derive(Deserialize, Default)]
struct ChoiceLogprobs {
    #[serde(default)]
    content: Vec<ContentLogprob>,
}

#[derive(Deserialize)]
struct ContentLogprob {
    logprob: f64,
    #[serde(default)]
    top_logprobs: Vec<TopLogprobWire>,
}

#[derive(Deserialize)]
struct TopLogprobWire {
    #[serde(default)]
    token: String,
    logprob: f64,
}

/// Build the upstream request body from the buyer's canonical request (R1: normalizing the
/// request into the upstream format). `model` is forced by the market from configuration -- the buyer's `model`
/// is absent from `CanonRequest` by design.
fn build_request<'a>(cfg: &'a OpenAiConfig, req: &CanonRequest) -> ChatRequest<'a> {
    let messages = req
        .messages
        .iter()
        .map(|m| WireMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();
    let (temperature, max_tokens, stop, seed) = match &req.params {
        Some(p) => (
            // `greedy`(B7 spot-check) forcibly sets temp=0(distinct from 0="not set").
            if p.greedy {
                Some(0.0)
            } else {
                (p.temperature != 0.0).then_some(p.temperature)
            },
            (p.max_tokens != 0).then_some(p.max_tokens),
            p.stop.clone(),
            // Groq exposes a random seed even at temperature=0 for some models(notably gpt-oss). Pin the
            // sampled B7 greedy probe so the seller stream and the reference endpoint compare the same run.
            p.greedy.then_some(0),
        ),
        None => (None, None, Vec::new(), None),
    };
    // request logprobs only if the model config declared support(capability-aware) --
    // otherwise don't send the field at all(a strict endpoint must not fail with `400`). Don't fabricate(R3/R4):
    // absence of logprobs -> lower verification weight at the buyer, not invented values.
    let (logprobs, top_logprobs) = if cfg.capabilities.logprobs {
        (Some(true), cfg.capabilities.top_logprobs)
    } else {
        (None, None)
    };
    ChatRequest {
        model: &cfg.model,
        messages,
        stream: true,
        temperature,
        max_tokens,
        stop,
        logprobs,
        top_logprobs,
        seed,
        reasoning: openrouter_qwen_reasoning(cfg).then_some(ReasoningRequest {
            enabled: true,
            exclude: false,
        }),
    }
}

fn openrouter_qwen_reasoning(cfg: &OpenAiConfig) -> bool {
    cfg.base_url.to_ascii_lowercase().contains("openrouter.ai")
        && cfg.model.eq_ignore_ascii_case("qwen/qwen3-32b")
}

/// Run the real upstream: POST `.../chat/completions` with `stream:true`, parse the SSE and
/// normalize deltas into `CanonChunk`, yielding incrementally into `tx`(R6). No more than `count`
/// delivered tokens are requested/forwarded. A canonical request is mandatory
/// . On a missing key/error we
/// close the stream with an error status(the response buffer does not accumulate).
pub async fn run(
    cfg: &OpenAiConfig,
    count: u64,
    req: Option<CanonRequest>,
    tx: mpsc::Sender<Result<UpstreamEvent, Status>>,
) {
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

    if let Err(status) = stream_upstream(cfg, &key, count, &req, &tx).await {
        // Send the error into the channel(if the buyer is still listening) -- without leaking the key into the text.
        let _ = tx.send(Err(status)).await;
    }
}

/// Internal stream loop: connect, parse SSE, normalize. Errors are returned as `Status`
/// without confidential data. The `Authorization` header carries the key only at runtime.
async fn stream_upstream(
    cfg: &OpenAiConfig,
    key: &str,
    count: u64,
    req: &CanonRequest,
    tx: &mpsc::Sender<Result<UpstreamEvent, Status>>,
) -> Result<(), Status> {
    use futures::StreamExt;

    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = build_request(cfg, req);

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| Status::unavailable(format!("upstream connect failed: {e}")))?;

    if !resp.status().is_success() {
        let code = resp.status();
        // The body may carry an upstream error detail, but not our key -- safe to surface the code.
        return Err(Status::unavailable(format!("upstream HTTP {code}")));
    }

    // Incremental SSE parsing over the body's byte stream(R6): accumulate a buffer, split on
    // `\n\n` boundaries, parse `data:` lines. `data: [DONE]` ends the stream.
    let mut byte_stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut seq: u64 = 0;
    let mut sent_tokens: u64 = 0;

    while let Some(item) = byte_stream.next().await {
        let bytes = item.map_err(|e| Status::unavailable(format!("upstream read failed: {e}")))?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // Flush complete SSE events(separated by `\n\n`); an unfinished frame must not grow
        // the gateway buffer without bound -- a hostile/broken upstream is untrusted(Y3, R6).
        for event in drain_complete_events(&mut buf)? {
            match parse_event(&event) {
                ParsedEvent::Done => return Ok(()),
                ParsedEvent::Delta(text, reasoning, logprobs)
                    if !text.is_empty() || !reasoning.is_empty() =>
                {
                    // R3: HONEST declaration -- has_logprobs based on the actual presence of logprobs in the first
                    // content delta(if the upstream honored `logprobs:true`). Don't fabricate(R4).
                    let has_lp = !logprobs.is_empty();
                    let delivered_tokens = (logprobs.len() as u64).max(1);
                    let chunk = CanonChunk {
                        text,
                        reasoning,
                        // R2/R4: Groq chat-completions does not return token-ids in SSE -- do NOT fabricate.
                        token_ids: Vec::new(),
                        seq,
                        // R2: delta logprobs(chosen + top-k) -- normalized without loss.
                        logprobs,
                        manifest: (seq == 0).then(|| SignalManifest {
                            // Family comes from the model config; the buyer matches the profile.
                            tokenizer_family: cfg.tokenizer_family.clone(),
                            has_token_ids: false,
                            has_logprobs: has_lp,
                            // Declare the CANONICAL frame model(what the buyer paid for / verifies, B2/B7), NOT
                            // the upstream served slug -- declaring the slug false-trips. The test-only
                            // override emits a different declared name to prove a real substitution.
                            claimed_model: cfg
                                .claimed_model_override
                                .clone()
                                .unwrap_or_else(|| cfg.frame_model.clone()),
                        }),
                    };
                    seq += 1;
                    if tx
                        .send(Ok(chunk_with_structured_accounting(chunk)))
                        .await
                        .is_err()
                    {
                        return Ok(()); // buyer disconnected(STOP)
                    }
                    sent_tokens = sent_tokens.saturating_add(delivered_tokens);
                    if sent_tokens >= count {
                        return Ok(()); // budget exhausted
                    }
                }
                ParsedEvent::Delta(..) | ParsedEvent::Other => {}
            }
        }
    }
    Ok(())
}

/// Cap on an unfinished SSE frame(Y3): a hostile/broken upstream sending bytes without
/// a `\n\n` separator must not grow the gateway buffer without bound. Legitimate events (a text
/// delta + top-k logprobs) are 2-3 orders of magnitude smaller -- 1 MiB does not touch them.
const MAX_SSE_FRAME_BYTES: usize = 1 << 20;

/// Drain complete SSE events(`\n\n`-separated) from the buffer in order. If the REMAINDER
/// (unfinished frame) exceeds the cap -- `resource_exhausted` instead of uncontrolled buffer
/// growth(Y3, R6). Complete events are always drained before the cap check.
// `tonic::Status` is the standard gRPC error type of the whole upstream module; boxing it in a single helper
// would break `?`-propagation into the loop's `Result<_, Status>`. The large Err variant here is deliberate.
#[allow(clippy::result_large_err)]
fn drain_complete_events(buf: &mut String) -> Result<Vec<String>, Status> {
    let mut events = Vec::new();
    while let Some(idx) = buf.find("\n\n") {
        events.push(buf[..idx].to_string());
        buf.drain(..idx + 2);
    }
    if buf.len() > MAX_SSE_FRAME_BYTES {
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
    /// `data: {...}` with content/reasoning deltas(possibly empty) and the delta's token logprobs.
    Delta(String, String, Vec<TokenLogprobs>),
    /// Carries no delta(comment, keep-alive, etc.).
    Other,
}

/// Parse a single SSE event: join the `data:` lines, recognize `[DONE]`, otherwise extract
/// `choices[0].delta.content`, provider-separated reasoning, and `choices[0].logprobs.content[]`
/// . A frame without `data:` or unparsed JSON is
/// `Other`(we don't break the stream, R6).
fn parse_event(event: &str) -> ParsedEvent {
    let mut data = String::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() {
        return ParsedEvent::Other;
    }
    if data == "[DONE]" {
        return ParsedEvent::Done;
    }
    match serde_json::from_str::<StreamChunk>(&data) {
        Ok(chunk) => {
            let Some(choice) = chunk.choices.into_iter().next() else {
                return ParsedEvent::Other;
            };
            let Delta {
                content,
                reasoning,
                reasoning_content,
                reasoning_details,
            } = choice.delta;
            let text = content.unwrap_or_default();
            let reasoning = collect_reasoning(reasoning, reasoning_content, reasoning_details);
            let logprobs = choice
                .logprobs
                .map(|lp| {
                    lp.content
                        .into_iter()
                        .map(|c| TokenLogprobs {
                            logprob: c.logprob,
                            top: c
                                .top_logprobs
                                .into_iter()
                                .map(|t| TopLogprob {
                                    token: t.token,
                                    logprob: t.logprob,
                                })
                                .collect(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            ParsedEvent::Delta(text, reasoning, logprobs)
        }
        // An unparsed frame does not crash the stream -- we skip it(R6: incremental robustness).
        Err(_) => ParsedEvent::Other,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_done_and_other() {
        let delta = parse_event("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}");
        assert!(matches!(delta, ParsedEvent::Delta(t, r, _) if t == "hi" && r.is_empty()));
        assert!(matches!(parse_event("data: [DONE]"), ParsedEvent::Done));
        assert!(matches!(parse_event(": keep-alive"), ParsedEvent::Other));
        // A delta without content(role-only first frame) -> empty string, not accounted.
        let empty = parse_event("data: {\"choices\":[{\"delta\":{}}]}");
        assert!(matches!(empty, ParsedEvent::Delta(t, r, _) if t.is_empty() && r.is_empty()));
    }

    #[test]
    fn parses_openrouter_reasoning_fields() {
        let raw = parse_event(
            "data: {\"choices\":[{\"delta\":{\"content\":\"391\",\"reasoning\":\"raw \",\"reasoning_content\":\"alias \",\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"detail text\"},{\"type\":\"reasoning.summary\",\"summary\":\"summary text\"},{\"type\":\"reasoning.encrypted\",\"data\":\"redacted\"}]}}]}",
        );
        match raw {
            ParsedEvent::Delta(text, reasoning, lp) => {
                assert_eq!(text, "391");
                assert!(reasoning.contains("raw"));
                assert!(reasoning.contains("alias"));
                assert!(reasoning.contains("detail text"));
                assert!(reasoning.contains("summary text"));
                assert!(!reasoning.contains("redacted"));
                assert!(lp.is_empty());
            }
            _ => panic!("expected OpenRouter reasoning delta"),
        }
    }

    /// Y3(regression): complete events are drained in order, the unfinished tail is preserved.
    #[test]
    fn drain_keeps_partial_frame() {
        let mut buf = String::from("data: a\n\ndata: b\n\ndata: part");
        let events = drain_complete_events(&mut buf).unwrap();
        assert_eq!(events, vec!["data: a".to_string(), "data: b".to_string()]);
        assert_eq!(
            buf, "data: part",
            "unfinished frame preserved under the cap"
        );
    }

    /// Y3(negative): an upstream without a `\n\n` separator does not grow the gateway buffer without bound --
    /// when the cap is exceeded the stream closes with `resource_exhausted`, not OOM.
    #[test]
    fn frame_without_separator_is_capped() {
        let mut buf = "x".repeat(MAX_SSE_FRAME_BYTES + 1);
        let err = drain_complete_events(&mut buf).unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn parses_logprobs_into_canon() {
        // R2: choices[0].logprobs.content[] -> TokenLogprobs(chosen + top-k without loss).
        let ev = parse_event(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"logprobs\":{\"content\":[{\"token\":\"x\",\"logprob\":-0.4,\"top_logprobs\":[{\"token\":\"x\",\"logprob\":-0.4},{\"token\":\"y\",\"logprob\":-1.5}]}]}}]}",
        );
        match ev {
            ParsedEvent::Delta(t, r, lp) => {
                assert_eq!(t, "x");
                assert!(r.is_empty());
                assert_eq!(lp.len(), 1);
                assert!((lp[0].logprob - (-0.4)).abs() < 1e-9);
                assert_eq!(lp[0].top.len(), 2);
                assert_eq!(lp[0].top[1].token, "y");
            }
            _ => panic!("expected Delta with logprobs"),
        }
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
        let body = build_request(&cfg, &req);
        assert_eq!(body.model, DEFAULT_MODEL);
        assert_eq!(body.messages.len(), 1);
        assert!(body.stream);
        assert!(body.reasoning.is_none());
        // Zero-valued parameters are not serialized(the upstream default is used).
        assert!(body.temperature.is_none());
        assert!(body.max_tokens.is_none());
    }

    #[test]
    fn build_request_omits_logprobs_when_capability_off() {
        // a model without logprobs -> the field is NOT in the request body(a strict endpoint won't get a 400).
        let cfg = OpenAiConfig {
            capabilities: Capabilities {
                logprobs: false,
                top_logprobs: None,
            },
            ..Default::default()
        };
        let req = CanonRequest {
            messages: vec![],
            params: None,
        };
        let body = build_request(&cfg, &req);
        assert!(body.logprobs.is_none());
        assert!(body.top_logprobs.is_none());
        let json = serde_json::to_string(&body).unwrap();
        assert!(
            !json.contains("logprobs"),
            "logprobs not serialized: {json}"
        );
    }

    #[test]
    fn build_request_sends_logprobs_when_capability_on() {
        // a model with logprobs(default caps Groq/qwen) -> the field is present(B6 signals are collected).
        let cfg = OpenAiConfig::default();
        let req = CanonRequest {
            messages: vec![],
            params: None,
        };
        let body = build_request(&cfg, &req);
        assert_eq!(body.logprobs, Some(true));
        assert_eq!(body.top_logprobs, Some(5));
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"logprobs\":true"), "{json}");
    }

    #[test]
    fn build_request_pins_seed_only_for_greedy_spotcheck() {
        let cfg = OpenAiConfig::default();
        let greedy = CanonRequest {
            messages: vec![],
            params: Some(dexdo_proto::SamplingParams {
                temperature: 0.9,
                max_tokens: 16,
                stop: vec![],
                greedy: true,
            }),
        };
        let body = build_request(&cfg, &greedy);
        assert_eq!(body.temperature, Some(0.0));
        assert_eq!(body.seed, Some(0));
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
        let body = build_request(&cfg, &regular);
        assert_eq!(body.temperature, Some(0.9));
        assert_eq!(body.seed, None);
    }

    #[test]
    fn build_request_enables_openrouter_qwen_reasoning_only_for_exact_model() {
        let cfg = OpenAiConfig {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: "qwen/qwen3-32b".to_string(),
            capabilities: Capabilities {
                logprobs: false,
                top_logprobs: None,
            },
            ..Default::default()
        };
        let body = build_request(
            &cfg,
            &CanonRequest {
                messages: vec![],
                params: None,
            },
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
        );
        assert!(body.reasoning.is_none());
    }
}
