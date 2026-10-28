//! Real OpenAI-compatible upstream: Chat Completions and Responses requests are relayed
//! to the provider as received, and its response returns as produced.

//! **Billing authority:** money is authorized only by the provider's
//! terminal usage record ([`super::meter`]). SSE event boundaries and delta text length are never a
//! token count.

//! The key is taken **from the environment at runtime** ([`api_key`]) and is never stored/logged.
//! Without a key the adapter does not start -- the stream closes with `Status::failed_precondition`.

use super::forward::{forward, ProviderCall};
use super::{
    probe_body, send_event, ProviderRequest, StartupCapabilityRequirements, UpstreamResult,
};
use crate::api_format::{parse_frame, ApiFormat, Frame, SseFrames};
use crate::seller::models::ModelConfig;
use tokio::sync::mpsc;
use tonic::Status;

/// Name of the env variable holding the upstream key (Groq OpenAI-compatible API).
pub const API_KEY_ENV: &str = "GROQ_API_KEY";

/// Default base of the Groq OpenAI-compatible API.
pub const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Default model id -- Qwen 32B on Groq (canonical id from the Groq model list).
pub const DEFAULT_MODEL: &str = "qwen/qwen3-32b";

/// Real upstream configuration. Carries **only** non-confidential parameters (URL/model id/
/// env-key name). The key itself is NOT stored here -- it is read from the environment at request
/// time via [`OpenAiConfig::api_key_env`]. Operationally built from a model config entry
/// `Default` is the built-in demo default (Groq/qwen)
/// for tests and `live_groq`.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    /// Base URL of the OpenAI-compatible API (without the operation path).
    pub base_url: String,
    /// Upstream model id (`served_model`): sent to the provider in place of the deal model.
    pub model: String,
    /// **Canonical market id** (`producer--model--version`, e.g. `qwen--qwen3--32b`) -- the model the
    /// buyer paid for (B2) and the model a request must name.
    pub frame_model: String,
    /// Name of the env variable holding the key -- **per-model/provider**, not a single global one.
    pub api_key_env: String,
    /// The operator's declared extra spellings for this SAME model --
    /// e.g. `Qwen/Qwen3-32B` for the slug `qwen/qwen3-32b`. The seller's served-model check reads
    /// them, so an operator who declared the provider's own spelling is not refused for it.
    pub identity_aliases: Vec<String>,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            frame_model: DEFAULT_MODEL.to_string(),
            api_key_env: API_KEY_ENV.to_string(),
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
            frame_model: registry_frame_model.unwrap_or(&m.frame_model).to_string(),
            api_key_env: m.api_key_env.clone(),
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

fn endpoint(cfg: &OpenAiConfig, format: ApiFormat) -> String {
    let operation = match format {
        ApiFormat::Responses => "/responses",
        ApiFormat::ChatCompletions | ApiFormat::Messages => "/chat/completions",
    };
    format!("{}{operation}", cfg.base_url.trim_end_matches('/'))
}

/// Relay one request. `market` is the identity of the market this call is being made for, when the
/// caller knows one (seller readiness). `None` is "no market is in question" -- buyer traffic and the
/// bare provider-health probe. It selects which question the served-model check answers; see
/// [`offered_model_aliases`].
pub(super) async fn run_for_market(
    cfg: &OpenAiConfig,
    market: Option<&str>,
    request: &ProviderRequest,
    tx: mpsc::Sender<UpstreamResult>,
) {
    let Some(key) = api_key(&cfg.api_key_env) else {
        let _ = send_event(
            &tx,
            Err(Status::failed_precondition(format!(
                "real upstream key absent ({})",
                cfg.api_key_env
            ))),
        )
        .await;
        return;
    };
    let offered = offered_model_aliases(cfg, market);
    let check = |reported: &str| {
        (!offered.contains(&crate::registry::model_id_alias(reported)))
            .then(|| served_model_mismatch(cfg, market, reported))
    };
    let call = ProviderCall {
        url: endpoint(cfg, request.format),
        credentials: vec![("authorization", format!("Bearer {key}"))],
        request,
        served_model: Some(&check),
    };
    if let Err(status) = forward(call, &tx).await {
        let _ = send_event(&tx, Err(status)).await;
    }
}

/// HOW FAR the served-model check goes, so nobody reads more into it than it carries: a dishonest
/// seller can rewrite the field before the buyer sees anything. What this catches is a MISCONFIGURED
/// seller -- the wrong `served_model`, a typo, a provider that silently substituted a replacement. The
/// defence against deliberate substitution is the buyer's own check of the forwarded response and its
/// content spot-check (B7/B8), where the buyer obtains the reference itself.
fn served_model_mismatch(cfg: &OpenAiConfig, market: Option<&str>, reported: &str) -> String {
    match market {
        // Seller readiness: the offer may not rest, because the model that answered is not the model
        // this market sells.
        Some(market_model) => format!(
            "upstream served model \"{reported}\", but this market sells \"{market_model}\": the \
             provider answering this seller is not the model the buyer would be paying for -- point \
             --model at the model this market was provisioned for, or declare the provider's own \
             spelling in identity_aliases for this model in the models config (models.json)"
        ),
        // Provider health: the provider did not serve a model this seller's own config names.
        None => format!(
            "upstream served model \"{reported}\", not the offered \"{}\" (market model \"{}\"): \
             correct served_model/frame_model/identity_aliases for this model in the models config \
             (models.json), or the provider substituted the model",
            cfg.model, cfg.frame_model
        ),
    }
}

/// Every spelling that may name the model that just answered -- **for the question the caller is asking**.

/// * `market == None` -- **provider health** and buyer traffic: "did my provider serve a model my own
/// config names?" The set is the seller's own declared spellings: the slug it sends
/// (`served_model`), the id it sells under (`frame_model`) and `identity_aliases`.
/// * `market == Some(id)` -- **market readiness**: "is the model that answered the model THIS MARKET
/// sells?" The set is the market id and `identity_aliases` -- nothing else. `served_model` is absent
/// because a provider echoes the model it was asked for: with the slug in the set the check would
/// compare our own request against itself.

/// [`crate::registry::model_id_alias`] is the repo's one normalization between the spellings
/// (`qwen/qwen3-32b`, `qwen--qwen3--32b`, `Qwen/Qwen3-32B`), so "same model, other spelling" can never
/// read as a substitution.
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

const CAPABILITY_TOOL_NAME: &str = "dexdo_capability_probe";

/// The seller's own readiness or capability request: a streamed Chat Completions call.
pub(super) fn probe_request(
    cfg: &OpenAiConfig,
    prompt: &str,
    max_tokens: Option<u32>,
    requirements: Option<StartupCapabilityRequirements>,
) -> ProviderRequest {
    let mut body = serde_json::json!({
        "model": cfg.model,
        "messages": [{ "role": "user", "content": prompt }],
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = max_tokens.into();
    }
    if requirements.is_some_and(|requirements| requirements.tools) {
        body["tools"] = serde_json::json!([{
            "type": "function",
            "function": {
                "name": CAPABILITY_TOOL_NAME,
                "description": "Return an empty object to prove tool-call support.",
                "parameters": { "type": "object", "properties": {}, "additionalProperties": false },
            },
        }]);
        body["tool_choice"] = serde_json::json!({
            "type": "function",
            "function": { "name": CAPABILITY_TOOL_NAME },
        });
    }
    if requirements.is_some_and(|requirements| requirements.think) {
        body["reasoning"] = serde_json::json!({ "enabled": true, "exclude": false });
    }
    ProviderRequest {
        format: ApiFormat::ChatCompletions,
        body: probe_body(body),
        headers: Vec::new(),
    }
}

/// What a startup capability probe's response proved.
pub(super) struct CapabilityObservation {
    requirements: StartupCapabilityRequirements,
    frames: SseFrames,
    saw_content: bool,
    saw_reasoning: bool,
    saw_tool_call: bool,
    reasoning_tokens: Option<u64>,
}

impl CapabilityObservation {
    pub(super) fn new(requirements: StartupCapabilityRequirements) -> Self {
        Self {
            requirements,
            frames: SseFrames::default(),
            saw_content: false,
            saw_reasoning: false,
            saw_tool_call: false,
            reasoning_tokens: None,
        }
    }

    /// Read the forwarded response bytes.
    #[allow(clippy::result_large_err)]
    pub(super) fn observe(&mut self, bytes: &[u8]) -> Result<(), Status> {
        for frame in self.frames.push(bytes).map_err(Status::data_loss)? {
            let Frame::Event { value, facts } =
                parse_frame(ApiFormat::ChatCompletions, &frame.text).map_err(Status::data_loss)?
            else {
                continue;
            };
            self.saw_content |= !facts.text.trim().is_empty();
            self.saw_reasoning |= !facts.reasoning.trim().is_empty();
            if self.requirements.tools {
                self.saw_tool_call |= frame_has_tool_call(&value);
            }
            if self.requirements.think {
                if let Some(reported) = frame_reasoning_usage(&value)? {
                    if self
                        .reasoning_tokens
                        .is_some_and(|recorded| recorded != reported)
                    {
                        return Err(Status::data_loss(
                            "OpenAI-compatible stream reported contradictory reasoning-token usage totals",
                        ));
                    }
                    self.reasoning_tokens = Some(reported);
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::result_large_err)]
    pub(super) fn validate(&self, completion_tokens: u64) -> Result<(), Status> {
        let requirements = self.requirements;
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
    let Some(container) = container.filter(|container| !container.is_null()) else {
        return Ok(None);
    };
    let Some(object) = container.as_object() else {
        return Err(Status::data_loss(
            "OpenAI-compatible usage is not an object",
        ));
    };
    let Some(details) = object
        .get("completion_tokens_details")
        .filter(|details| !details.is_null())
    else {
        return Ok(None);
    };
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

/// A VERBATIM capture of a live Groq `qwen/qwen3-32b` stream, recorded by sending exactly the request
/// this adapter builds for the readiness probe (`stream: true`,
/// `stream_options.include_usage: true`, `temperature: 0`, `max_tokens: 1`). The capture predates the
/// log-probability retirement (`ef6a8611`), so the recorded frames still carry the `logprobs` payload
/// the probe asked for then; the readiness probe sends no such key now. Nothing is trimmed, reordered or
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
    use crate::api_format::RequestBody;
    use crate::seller::upstream::{UpstreamEvent, UpstreamResult};
    use dexdo_proto::{BillingUsage, Header};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Scripted {
        status: &'static str,
        content_type: &'static str,
        /// Body bytes, written as separate reads.
        writes: Vec<Vec<u8>>,
    }

    fn sse(body: &str) -> Scripted {
        Scripted {
            status: "200 OK",
            content_type: "text/event-stream",
            writes: vec![body.as_bytes().to_vec()],
        }
    }

    /// One HTTP provider per answer; returns its base URL and the raw requests it read.
    async fn provider(answers: Vec<Scripted>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for answer in answers {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut next = [0_u8; 4096];
                loop {
                    let read = socket.read(&mut next).await.unwrap();
                    assert_ne!(read, 0, "the provider received a truncated request");
                    request.extend_from_slice(&next[..read]);
                    let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = std::str::from_utf8(&request[..end]).unwrap();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let length: usize = answer.writes.iter().map(Vec::len).sum();
                let head = format!(
                    "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n",
                    answer.status, answer.content_type
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                for write in answer.writes {
                    socket.write_all(&write).await.unwrap();
                    socket.flush().await.unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
            requests
        });
        (format!("http://{address}"), task)
    }

    fn config(base_url: String) -> OpenAiConfig {
        OpenAiConfig {
            base_url,
            model: "qwen/qwen3-32b".to_string(),
            frame_model: "qwen--qwen3--32b".to_string(),
            // `PATH` is always set and non-empty, so the adapter reaches the provider without
            // mutating process-global environment while other tests run.
            api_key_env: "PATH".to_string(),
            identity_aliases: Vec::new(),
        }
    }

    fn request(format: ApiFormat, body: serde_json::Value) -> ProviderRequest {
        let mut body = RequestBody::parse(&serde_json::to_vec(&body).unwrap()).unwrap();
        body.set_model("qwen/qwen3-32b");
        if format == ApiFormat::ChatCompletions && body.stream() {
            body.require_stream_usage();
        }
        ProviderRequest {
            format,
            body,
            headers: Vec::new(),
        }
    }

    fn chat(stream: bool) -> ProviderRequest {
        request(
            ApiFormat::ChatCompletions,
            serde_json::json!({
                "messages": [{"role": "user", "content": "hello"}],
                "stream": stream,
            }),
        )
    }

    async fn relay_with(
        answers: Vec<Scripted>,
        market: Option<&str>,
        request: ProviderRequest,
    ) -> (Vec<UpstreamResult>, Vec<String>) {
        relay_served("qwen/qwen3-32b", answers, market, request).await
    }

    async fn relay_served(
        served: &str,
        answers: Vec<Scripted>,
        market: Option<&str>,
        request: ProviderRequest,
    ) -> (Vec<UpstreamResult>, Vec<String>) {
        let (base_url, provider) = provider(answers).await;
        let (tx, mut rx) = mpsc::channel(256);
        let cfg = OpenAiConfig {
            model: served.to_string(),
            ..config(base_url)
        };
        run_for_market(&cfg, market, &request, tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (events, provider.await.unwrap())
    }

    async fn relay(body: &str) -> Vec<UpstreamResult> {
        relay_with(vec![sse(body)], None, chat(true)).await.0
    }

    fn forwarded(events: &[UpstreamResult]) -> Vec<u8> {
        events
            .iter()
            .filter_map(|event| match event {
                Ok(UpstreamEvent::Data { bytes, .. }) => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn outputs(events: &[UpstreamResult]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, Ok(UpstreamEvent::Data { output: true, .. })))
            .count()
    }

    fn billed(events: &[UpstreamResult]) -> Option<BillingUsage> {
        events.iter().find_map(|event| match event {
            Ok(UpstreamEvent::Usage(usage)) => Some(*usage),
            _ => None,
        })
    }

    fn error(events: &[UpstreamResult]) -> Option<&Status> {
        events.iter().find_map(|event| event.as_ref().err())
    }

    /// The key the test provider is called with (`config` reads it from `PATH`).
    fn seller_key() -> String {
        std::env::var("PATH").unwrap()
    }

    /// a provider that answers `200` with a bare JSON error object instead of a stream has
    /// answered. Its reason reaches the operator, nothing is relayed or billed, and the error is
    /// not one to retry on the same deal.
    #[tokio::test]
    async fn a_bare_json_error_body_under_two_hundred_is_an_answer_not_a_retryable_outage() {
        let events = relay(r#"{"error":{"message":"model decommissioned"}}"#).await;
        let status = error(&events).expect("an error object is not a served request");
        assert!(
            status.message().contains("model decommissioned"),
            "{status:?}"
        );
        assert_ne!(status.code(), tonic::Code::Unavailable, "{status:?}");
        assert!(!dexdo_proto::is_retryable_upstream(status), "{status:?}");
        assert!(forwarded(&events).is_empty());
        assert!(billed(&events).is_none());
        // Anything that is not a stated error stays the retryable transport non-answer.
        for body in [r#"{"choices":[]}"#, "not json", r#"{"error":null}"#] {
            let events = relay(body).await;
            let status = error(&events).expect("no stream is no answer");
            assert!(
                dexdo_proto::is_retryable_upstream(status),
                "{body}: {status:?}"
            );
        }
    }

    /// An answer that names the seller's key is never relayed, wherever the key is: a response
    /// header, a frame of a streamed answer, or a whole body. The error is final.
    #[tokio::test]
    async fn an_answer_naming_the_sellers_key_is_never_relayed() {
        let key = seller_key();
        let leaky_frame = format!("{}{}", content("ok"), content(&format!("key {key}")));
        let events = relay(&leaky_frame).await;
        let status = error(&events).expect("a key-naming frame ends the answer");
        assert!(
            status.message().contains("named the seller's key"),
            "{status:?}"
        );
        assert!(!dexdo_proto::is_retryable_upstream(status));
        let relayed = String::from_utf8(forwarded(&events)).unwrap();
        assert!(!relayed.contains(&key), "{relayed}");

        let header: &'static str =
            Box::leak(format!("text/event-stream\r\nx-echo: {key}").into_boxed_str());
        let answer = Scripted {
            status: "200 OK",
            content_type: header,
            writes: vec![format!("{}{}", content("ok"), usage_frame(1, 1)).into_bytes()],
        };
        let (events, _) = relay_with(vec![answer], None, chat(true)).await;
        assert!(error(&events)
            .is_some_and(|status| status.message().contains("named the seller's key")));
        assert!(forwarded(&events).is_empty());

        let body = serde_json::json!({
            "model": "qwen/qwen3-32b",
            "choices": [{"message": {"content": format!("echo {key}")}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })
        .to_string();
        let whole = Scripted {
            status: "200 OK",
            content_type: "application/json",
            writes: vec![body.into_bytes()],
        };
        let (events, _) = relay_with(vec![whole], None, chat(false)).await;
        assert!(error(&events)
            .is_some_and(|status| status.message().contains("named the seller's key")));
        assert!(forwarded(&events).is_empty());
        assert!(billed(&events).is_none());
    }

    fn content(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[{"delta":{"content":text}}]})
        )
    }

    fn usage_frame(input: u64, output: u64) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[],"usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output}})
        )
    }

    #[tokio::test]
    async fn requests_reach_their_operation_path_with_the_seller_key_and_client_headers() {
        for (format, operation) in [
            (ApiFormat::ChatCompletions, "/chat/completions"),
            (ApiFormat::Responses, "/responses"),
        ] {
            let mut provider_request = request(
                format,
                serde_json::json!({"input": "hi", "messages": [], "stream": false}),
            );
            provider_request.headers = vec![Header {
                name: "session_id".into(),
                value: b"agent-session".to_vec(),
            }];
            let answer = Scripted {
                status: "200 OK",
                content_type: "application/json",
                writes: vec![br#"{"output":[],"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"input_tokens":0,"output_tokens":0}}"#.to_vec()],
            };
            let (_, requests) = relay_with(vec![answer], None, provider_request).await;
            let request = requests[0].to_ascii_lowercase();
            assert!(
                request.starts_with(&format!("post {operation} ")),
                "{format:?}: {request}"
            );
            assert!(request.contains("authorization: bearer "), "{request}");
            assert!(request.contains("session_id: agent-session"), "{request}");
            assert!(
                request.contains("content-type: application/json"),
                "{request}"
            );
            assert!(!request.contains("anthropic-version"), "{request}");
        }
    }

    #[tokio::test]
    async fn the_provider_status_and_headers_arrive_first_and_unchanged() {
        let body = format!("{}{}data: [DONE]\n\n", content("hi"), usage_frame(1, 1));
        let events = relay(&body).await;
        let Ok(UpstreamEvent::Head(head)) = &events[0] else {
            panic!("the first event is the provider head: {:?}", events.len());
        };
        assert_eq!(head.status, 200);
        let names: Vec<&str> = head.headers.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"content-type"));
        assert!(!names.contains(&"content-length"));
        assert!(!names.contains(&"connection"));
        assert_eq!(forwarded(&events), body.as_bytes());
        assert_eq!(billed(&events).unwrap().total_tokens, 2);
    }

    /// The seller does not ask again: a throttled request is relayed to the buyer with the
    /// provider's own status and body, once.
    #[tokio::test]
    async fn a_throttled_request_is_relayed_once_and_not_repeated() {
        let answer = Scripted {
            status: "429 Too Many Requests",
            content_type: "application/json",
            writes: vec![br#"{"error":{"message":"slow down"}}"#.to_vec()],
        };
        let (events, requests) = relay_with(vec![answer], None, chat(true)).await;
        assert_eq!(requests.len(), 1);
        let Ok(UpstreamEvent::Head(head)) = &events[0] else {
            panic!("throttle head is relayed");
        };
        assert_eq!(head.status, 429);
        assert_eq!(forwarded(&events), br#"{"error":{"message":"slow down"}}"#);
        assert!(error(&events).is_none());
        assert!(billed(&events).is_none());
    }

    #[tokio::test]
    async fn a_refused_seller_key_is_the_sellers_error_without_the_provider_body() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let answer = Scripted {
                status,
                content_type: "application/json",
                writes: vec![
                    br#"{"error":{"message":"Incorrect API key provided: sk-abc***xyz"}}"#.to_vec(),
                ],
            };
            let (events, _) = relay_with(vec![answer], None, chat(true)).await;
            assert_eq!(events.len(), 1, "{status}: no head, no body");
            let status_error = error(&events).unwrap();
            assert_eq!(status_error.message(), format!("upstream HTTP {status}"));
            assert!(!format!("{status_error:?}").contains("sk-abc"));
        }
    }

    #[tokio::test]
    async fn multibyte_character_split_across_reads_reaches_the_buyer_intact() {
        // Written as escapes so the source stays ASCII; these are ordinary two-byte letters.
        let text = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}";
        let body = format!("{}{}data: [DONE]\n\n", content(text), usage_frame(1, 6));
        let bytes = body.clone().into_bytes();
        let splits = (1..bytes.len())
            .filter(|at| !body.is_char_boundary(*at))
            .collect::<Vec<_>>();
        assert_eq!(
            splits.len(),
            6,
            "the fixture must contain split-able characters"
        );
        for split in splits {
            let answer = Scripted {
                status: "200 OK",
                content_type: "text/event-stream",
                writes: vec![bytes[..split].to_vec(), bytes[split..].to_vec()],
            };
            let (events, _) = relay_with(vec![answer], None, chat(true)).await;
            assert!(
                error(&events).is_none(),
                "split={split}: {:?}",
                error(&events)
            );
            assert_eq!(
                forwarded(&events),
                bytes,
                "a read boundary inside a character changed the buyer's bytes (split={split})"
            );
            assert_eq!(billed(&events).unwrap().total_tokens, 7);
        }
    }

    /// Bytes that are not valid text inside a COMPLETE frame are refused explicitly instead of being
    /// forwarded as if they were the provider's answer.
    #[tokio::test]
    async fn invalid_utf8_inside_a_complete_frame_is_refused() {
        let mut body =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"bad\xffbytes\"}}]}\n\n".to_vec();
        body.extend_from_slice(usage_frame(1, 1).as_bytes());
        body.extend_from_slice(b"data: [DONE]\n\n");
        let answer = Scripted {
            status: "200 OK",
            content_type: "text/event-stream",
            writes: vec![body],
        };
        let (events, _) = relay_with(vec![answer], None, chat(true)).await;
        assert!(error(&events)
            .unwrap()
            .message()
            .contains("not valid UTF-8"));
        assert_eq!(outputs(&events), 0);
        assert!(billed(&events).is_none());
    }

    #[test]
    fn drain_keeps_partial_frame() {
        let mut frames = crate::api_format::SseFrames::default();
        assert!(
            frames
                .push(b"data: {\"a\":1}\n\ndata: {\"b\"")
                .unwrap()
                .len()
                == 1
        );
        let rest = frames.push(b":2}\n\n").unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].text, "data: {\"b\":2}");
        assert_eq!(rest[0].bytes, b"data: {\"b\":2}\n\n");
        assert!(frames.take_rest().is_empty());
    }

    #[test]
    fn an_unfinished_frame_above_the_cap_is_refused() {
        let mut frames = crate::api_format::SseFrames::default();
        let huge = vec![b'x'; dexdo_core::params::UPSTREAM_SSE_FRAME_MAX_BYTES + 1];
        assert_eq!(
            frames.push(&huge).err(),
            Some("provider SSE frame exceeds buffer cap")
        );
    }

    /// E2E-ROW: E2E-UPS-41/L0
    #[tokio::test]
    async fn content_after_the_done_terminator_is_not_delivered_and_does_not_move_the_bill() {
        let complete = format!("{}{}data: [DONE]\n\n", content("paid"), usage_frame(2, 1));
        let events = relay(&format!("{complete}{}", content("tail"))).await;
        assert!(error(&events).is_none());
        assert_eq!(forwarded(&events), complete.as_bytes());
        assert_eq!(billed(&events).unwrap().total_tokens, 3);
    }

    /// E2E-ROW: E2E-UPS-42/L0
    #[tokio::test]
    async fn a_terminal_usage_record_after_the_done_terminator_authorizes_nothing() {
        let events = relay(&format!(
            "{}data: [DONE]\n\n{}",
            content("paid"),
            usage_frame(2, 1)
        ))
        .await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
    }

    /// E2E-ROW: E2E-UPS-43/L0
    #[tokio::test]
    async fn an_empty_done_only_stream_without_usage_fails_closed() {
        let events = relay("data: [DONE]\n\n").await;
        let error = error(&events).unwrap();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert!(error.message().contains("without terminal billing usage"));
        assert!(billed(&events).is_none());
    }

    /// E2E-ROW: E2E-UPS-44/L0
    #[tokio::test]
    async fn a_tool_call_only_answer_is_delivered_and_billed() {
        let call = format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"lookup","arguments":"{}"}}]}}]})
        );
        let events = relay(&format!("{call}{}data: [DONE]\n\n", usage_frame(4, 3))).await;
        assert!(error(&events).is_none());
        assert_eq!(outputs(&events), 1, "a tool call is output");
        assert_eq!(billed(&events).unwrap().total_tokens, 7);
    }

    /// E2E-ROW: E2E-UPS-45/L0
    #[tokio::test]
    async fn served_model_identity_is_settled_by_the_first_frame_that_states_one() {
        let named = |model: &str, text: &str| {
            format!(
                "data: {}\n\n",
                serde_json::json!({"model": model, "choices":[{"delta":{"content":text}}]})
            )
        };
        let body = format!(
            "{}{}{}data: [DONE]\n\n",
            named("qwen/qwen3-32b", "one "),
            named("meta-llama/llama-3.3-70b", "two"),
            usage_frame(1, 2)
        );
        let events = relay(&body).await;
        assert!(error(&events).is_none());
        assert_eq!(forwarded(&events), body.as_bytes());
        assert_eq!(billed(&events).unwrap().total_tokens, 3);
    }

    #[tokio::test]
    async fn a_served_model_mismatch_before_output_is_refused() {
        let body = format!(
            "data: {}\n\n{}data: [DONE]\n\n",
            serde_json::json!({"model": "meta-llama/llama-3.3-70b", "choices":[{"delta":{"content":"x"}}]}),
            usage_frame(1, 1)
        );
        let events = relay(&body).await;
        let error = error(&events).unwrap();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("meta-llama/llama-3.3-70b"));
        assert_eq!(
            outputs(&events),
            0,
            "nothing of the foreign answer is forwarded"
        );
        assert!(billed(&events).is_none());
    }

    /// E2E-ROW: E2E-UPS-46/L0
    #[tokio::test]
    async fn a_served_model_mismatch_found_after_delivery_is_carried_to_its_terminal() {
        let body = format!(
            "{}data: {}\n\n{}data: [DONE]\n\n",
            content("first "),
            serde_json::json!({"model": "meta-llama/llama-3.3-70b", "choices":[{"delta":{"content":"second"}}]}),
            usage_frame(1, 2)
        );
        let events = relay(&body).await;
        assert!(error(&events).is_none());
        assert_eq!(outputs(&events), 2);
        assert_eq!(billed(&events).unwrap().total_tokens, 3);
    }

    /// E2E-ROW: E2E-UPS-48/L0
    #[tokio::test]
    async fn sse_comments_are_forwarded_and_carry_no_output() {
        let body = format!(
            ": keep-alive\n\n{}: keep-alive\n\n{}{}data: [DONE]\n\n",
            content("one"),
            content("two"),
            usage_frame(1, 2)
        );
        let events = relay(&body).await;
        assert!(error(&events).is_none());
        assert_eq!(forwarded(&events), body.as_bytes());
        assert_eq!(outputs(&events), 2);
        assert_eq!(billed(&events).unwrap().total_tokens, 3);
    }

    /// E2E-ROW: E2E-UPS-47/L0
    #[test]
    fn a_bare_error_object_with_no_data_prefix_is_not_a_frame() {
        assert!(matches!(
            parse_frame(
                ApiFormat::ChatCompletions,
                r#"{"error":{"message":"overloaded"}}"#
            ),
            Ok(Frame::Empty)
        ));
    }

    /// E2E-ROW: E2E-UPS-47/L0
    #[test]
    fn an_event_line_without_any_data_line_produces_no_frame() {
        assert!(matches!(
            parse_frame(ApiFormat::ChatCompletions, "event: error"),
            Ok(Frame::Empty)
        ));
    }

    /// E2E-ROW: E2E-UPS-49/L0
    #[tokio::test]
    async fn a_total_stated_only_in_the_groq_usage_mirror_is_authoritative() {
        let mirror = format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[],"x_groq":{"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}})
        );
        let events = relay(&format!("{}{mirror}data: [DONE]\n\n", content("hi"))).await;
        assert!(error(&events).is_none());
        assert_eq!(billed(&events).unwrap().total_tokens, 7);
    }

    #[tokio::test]
    async fn disagreeing_usage_mirrors_bill_nothing() {
        let both = format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[],
                "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7},
                "x_groq":{"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}})
        );
        let events = relay(&format!("{}{both}data: [DONE]\n\n", content("hi"))).await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
    }

    #[tokio::test]
    async fn usage_on_an_output_delta_or_output_after_usage_bills_nothing() {
        let usage_on_delta = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"delta":{"content":"x"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
        );
        let output_after_usage = format!(
            "{}{}{}data: [DONE]\n\n",
            content("x"),
            usage_frame(1, 1),
            content("late")
        );
        for body in [usage_on_delta, output_after_usage] {
            let events = relay(&body).await;
            assert_eq!(
                error(&events).unwrap().code(),
                tonic::Code::DataLoss,
                "{body}"
            );
            assert!(billed(&events).is_none(), "{body}");
        }
    }

    /// A live Groq stream states the same total twice; the identical restatement is one record.
    #[tokio::test]
    async fn an_identical_restatement_bills_once_and_a_different_one_bills_nothing() {
        let events = relay(LIVE_GROQ_READINESS_CAPTURE).await;
        assert!(error(&events).is_none(), "{:?}", error(&events));
        assert_eq!(billed(&events).unwrap().total_tokens, 13);

        let events = relay(&format!(
            "{}{}{}data: [DONE]\n\n",
            content("x"),
            usage_frame(1, 1),
            usage_frame(1, 2)
        ))
        .await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
    }

    /// UPS-28 against a live capture: a positive bill with nothing delivered is refused.
    #[tokio::test]
    async fn a_positive_bill_without_delivered_output_is_refused() {
        let gpt_oss = |body: &'static str| {
            relay_served("openai/gpt-oss-20b", vec![sse(body)], None, chat(true))
        };
        let (events, _) = gpt_oss(LIVE_GROQ_GPT_OSS_ONE_TOKEN_CAPTURE).await;
        assert!(error(&events)
            .unwrap()
            .message()
            .contains("billable tokens without delivered output"));
        assert!(billed(&events).is_none());

        let (events, _) = gpt_oss(LIVE_GROQ_GPT_OSS_READINESS_CAPTURE).await;
        assert!(error(&events).is_none(), "reasoning is delivered output");
        assert_eq!(billed(&events).unwrap().output_tokens, 16);
    }

    #[tokio::test]
    async fn a_stream_that_ends_before_its_final_event_is_retryable_only_without_an_answer() {
        let events = relay("").await;
        let silent = error(&events).unwrap();
        assert!(dexdo_proto::is_retryable_upstream(silent));

        let events = relay(&content("partial")).await;
        let partial = error(&events).unwrap();
        assert_eq!(partial.code(), tonic::Code::DataLoss);
        assert!(!dexdo_proto::is_retryable_upstream(partial));

        let stated = format!(
            "data: {}\n\n",
            serde_json::json!({"error":{"message":"model overloaded"}})
        );
        let events = relay(&stated).await;
        assert_eq!(
            forwarded(&events),
            stated.as_bytes(),
            "the provider's words reach the buyer"
        );
        assert!(!dexdo_proto::is_retryable_upstream(error(&events).unwrap()));
    }

    /// The last frame of a body that ends without its blank line is closed and judged like any
    /// other: a final `[DONE]` still ends the answer, and output in it is classified before it is
    /// forwarded, never relayed as unjudged bytes.
    #[tokio::test]
    async fn an_unterminated_last_frame_is_judged_before_it_is_forwarded() {
        let body = format!("{}{}data: [DONE]", content("whole"), usage_frame(3, 1));
        let events = relay(&body).await;
        assert!(error(&events).is_none(), "{:?}", error(&events));
        assert_eq!(billed(&events).unwrap().total_tokens, 4);
        assert_eq!(forwarded(&events), format!("{body}\n\n").as_bytes());

        let tail = format!(
            "data: {}",
            serde_json::json!({"choices":[{"delta":{"content":"tail"}}]})
        );
        let events = relay(&tail).await;
        assert_eq!(outputs(&events), 1, "the tail is classified as output");
        assert!(billed(&events).is_none());
        let failure = error(&events).unwrap();
        assert_eq!(failure.code(), tonic::Code::DataLoss);
        assert!(
            !dexdo_proto::is_retryable_upstream(failure),
            "an answer already reached the buyer"
        );

        let foreign = format!(
            "data: {}",
            serde_json::json!({"model":"cheap/other","choices":[{"delta":{"content":"tail"}}]})
        );
        let events = relay(&foreign).await;
        assert_eq!(
            outputs(&events),
            0,
            "the served-model check runs on the tail too"
        );
        assert_eq!(
            error(&events).unwrap().code(),
            tonic::Code::FailedPrecondition
        );
    }

    #[tokio::test]
    async fn an_unstreamed_answer_is_relayed_whole_and_billed() {
        let body = serde_json::json!({
            "model": "qwen/qwen3-32b",
            "choices": [{"message": {"role": "assistant", "content": "whole"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
        })
        .to_string();
        let answer = Scripted {
            status: "200 OK",
            content_type: "application/json",
            writes: vec![body.as_bytes().to_vec()],
        };
        let (events, requests) = relay_with(vec![answer], None, chat(false)).await;
        assert!(!requests[0].contains("include_usage"), "{}", requests[0]);
        assert!(error(&events).is_none());
        assert_eq!(forwarded(&events), body.as_bytes());
        assert_eq!(outputs(&events), 1);
        assert_eq!(billed(&events).unwrap().total_tokens, 5);
    }

    #[tokio::test]
    async fn a_responses_stream_bills_its_completed_record() {
        let body = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"qwen/qwen3-32b\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"qwen/qwen3-32b\",\"status\":\"completed\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1,\"total_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n",
        );
        let (events, _) = relay_with(
            vec![sse(body)],
            None,
            request(
                ApiFormat::Responses,
                serde_json::json!({"input": "hello", "stream": true}),
            ),
        )
        .await;
        assert!(error(&events).is_none(), "{:?}", error(&events));
        assert_eq!(forwarded(&events), body.as_bytes());
        let usage = billed(&events).unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (4, 1));
        assert_eq!(usage.cache_read_tokens, 2);

        let unbilled = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        let (events, _) = relay_with(
            vec![sse(unbilled)],
            None,
            request(
                ApiFormat::Responses,
                serde_json::json!({"input": "hello", "stream": true}),
            ),
        )
        .await;
        assert_eq!(error(&events).unwrap().code(), tonic::Code::DataLoss);
        assert!(billed(&events).is_none());
    }

    #[test]
    fn capability_probe_observation_requires_what_the_flag_declares() {
        let tools = StartupCapabilityRequirements {
            tools: true,
            think: false,
        };
        let mut observation = CapabilityObservation::new(tools);
        observation
            .observe(format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[{"function":{"name":"other"}}]}}]})
            ).as_bytes())
            .unwrap();
        assert!(
            observation.validate(1).is_err(),
            "another tool proves nothing"
        );
        observation
            .observe(format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"tool_calls":[{"function":{"name":CAPABILITY_TOOL_NAME}}]}}]})
            ).as_bytes())
            .unwrap();
        observation.validate(1).unwrap();

        let think = StartupCapabilityRequirements {
            tools: false,
            think: true,
        };
        let mut observation = CapabilityObservation::new(think);
        observation
            .observe(b"data: {\"choices\":[{\"delta\":{\"reasoning\":\"thinking\"}}]}\n\n")
            .unwrap();
        assert!(
            observation.validate(1).is_err(),
            "reasoning without its usage count"
        );
        observation
            .observe(format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"completion_tokens_details":{"reasoning_tokens":2}}})
            ).as_bytes())
            .unwrap();
        observation.validate(2).unwrap();
        assert!(observation
            .observe(format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[],"usage":{"completion_tokens_details":{"reasoning_tokens":3}}})
            ).as_bytes())
            .is_err(), "contradictory reasoning totals");
    }
}
