//! Client-side model verification, on the buyer's side -- the protocol
//! does not guarantee authenticity off-chain; the client provides it.

//! Layers in increasing order of cost: the **model check** of the forwarded response
//! ([`ModelCheck`], every response), the reference spot-check (B7) and behavioral probes (B8, once
//! per deal), per-seller score (B10).

use crate::registry::{model_id_alias, registry_identity_candidates};
use crate::seller::{ModelConfig, ModelsConfig, SampleAlgorithm};
pub use dexdo_core::params::DEFAULT_SPOTCHECK_THRESHOLD;

/// Stream verification verdict: continue or bail (refuse/bail to the next
/// eligible seller B3 -- loss <= 2 ticks, without completing the deal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Signals consistent on the available subset -- continue accepting.
    Pass,
    /// Substitution/inconsistency caught -- bail. Carries the reason (for the log/dispute).
    Bail(String),
}

/// The model check of a forwarded response: every model the provider says produced
/// the answer must be the deal's model in one of its spellings -- the canonical market id
/// (`qwen--qwen3--32b`), the registry's (`Qwen/Qwen3-32B`), the provider slug (`qwen/qwen3-32b`),
/// the producer-free name (`qwen3-32b`), or the `served_model` or an `identity_aliases` entry of the
/// buyer's own models config for that model.
/// A response that names no model gives no signal; inventing a verdict from silence would refuse
/// honest sellers.
#[derive(Debug, Clone)]
pub struct ModelCheck {
    expected: String,
    accepted: Vec<String>,
    /// The deal model names no producer (`Qwen3.8-27B`), so a provider slug that prefixes one
    /// (`qwen/qwen3.8-27b`) is compared without it.
    producer_free: bool,
}

impl ModelCheck {
    pub fn new(expected: &str, models: Option<&ModelsConfig>) -> Self {
        let mut accepted: Vec<String> = registry_identity_candidates(expected)
            .iter()
            .map(|candidate| model_id_alias(candidate))
            .collect();
        if let Some(entry) = models.and_then(|models| resolve_model_cfg(expected, models)) {
            accepted.extend(
                [&entry.frame_model, &entry.served_model]
                    .into_iter()
                    .chain(&entry.identity_aliases)
                    .map(|alias| model_id_alias(alias)),
            );
        }
        accepted.sort();
        accepted.dedup();
        let expected = expected.trim();
        Self {
            expected: expected.to_string(),
            accepted,
            producer_free: !expected.contains('/') && !expected.contains("--"),
        }
    }

    pub fn verify(&self, reported: &str) -> Verdict {
        let reported_alias = model_id_alias(reported);
        // Every path segment before the model name is dropped: `qwen/...` and a provider's
        // `accounts/<account>/models/...` alike.
        let without_producer = reported_alias
            .rsplit_once('/')
            .map(|(_, model)| model.to_string())
            .filter(|_| self.producer_free);
        if self.accepted.contains(&reported_alias)
            || without_producer.is_some_and(|model| self.accepted.contains(&model))
        {
            Verdict::Pass
        } else {
            Verdict::Bail(format!(
                "model-check (): the provider answered as model '{reported}', not the deal's model '{}' -- substitution",
                self.expected
            ))
        }
    }
}

/// Behavioral fingerprint of an exact/reference model: a deterministic probe-prompt + a quirk that
/// the declared model characteristically emits (format/tokenization/refusals). Built from per-model config
/// ([`crate::seller::FingerprintCfg`]) -- owned `String` so it can be constructed at runtime, not a hardcoded set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub probe_prompt: String,
    /// A quirk marker in the response, characteristic of this exact/reference model.
    pub expected_contains: String,
    /// Some providers expose thinking out-of-band instead of embedding this marker in `content`.
    pub accepts_reasoning_side_channel: bool,
}

/// Resolve the config entry that governs `model_id` for **fingerprint / vocab** purposes. Matches by
/// config key or `frame_model` (via `ModelsConfig::get`), then exact `served_model`, then the served-form
/// alias (`model_id_alias`) across `frame_model` / `served_model` / `identity_aliases` -- so the registry
/// (`Qwen/Qwen3-32B`) and canonical (`qwen--qwen3--32b`) spellings both resolve to the same entry.
fn resolve_model_cfg<'a>(model_id: &str, models: &'a ModelsConfig) -> Option<&'a ModelConfig> {
    let id = model_id.trim();
    if let Ok(m) = models.get(id) {
        return Some(m);
    }
    if let Some(m) = models.models.values().find(|m| m.served_model == id) {
        return Some(m);
    }
    let want = model_id_alias(id);
    models.models.values().find(|m| {
        model_id_alias(&m.frame_model) == want
            || m.served_model.to_ascii_lowercase() == want
            || m.identity_aliases.iter().any(|a| model_id_alias(a) == want)
    })
}

fn fingerprints_for(model_id: &str, models: &ModelsConfig) -> Vec<Fingerprint> {
    resolve_model_cfg(model_id, models)
        .map(|m| {
            m.fingerprints
                .iter()
                .map(|f| Fingerprint {
                    probe_prompt: f.probe_prompt.clone(),
                    expected_contains: f.expected_contains.clone(),
                    accepts_reasoning_side_channel: f.accepts_reasoning_side_channel,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The exact model reference for the B7 spot-check, **derived** from the configured upstream
/// (`base_url` + `served_model` + `api_key_env`). Resolves ONLY by config key / `frame_model` /
/// exact `served_model` -- NOT `identity_aliases`: a provider-neutral registry name (e.g. `Qwen/Qwen3-32B`,
/// which may be served elsewhere) has no reference here, so we do not compare it against the configured
/// provider's greedy output. A profile using `sample_algorithm: NONE` has no comparable reference and
/// therefore also returns `None` -- **degradation** (R3): reliance on the model check and behavioral probes.
pub fn reference_endpoint_for(model_id: &str, models: &ModelsConfig) -> Option<ReferenceEndpoint> {
    let id = model_id.trim();
    let cfg = models
        .get(id)
        .ok()
        .or_else(|| models.models.values().find(|m| m.served_model == id))?;
    if cfg.capabilities.sample_algorithm == SampleAlgorithm::None {
        return None;
    }
    Some(cfg.reference_endpoint())
}

/// Resolve an exact buyer-controlled reference binding. Registry identities are accepted only when
/// explicitly listed in `identity_aliases`; fuzzy model-family matching is not used for money admission.
pub fn executable_reference_model_for<'a>(
    model_id: &str,
    models: &'a ModelsConfig,
) -> Option<&'a str> {
    let id = model_id.trim();
    let cfg = models
        .get(id)
        .ok()
        .or_else(|| {
            models
                .models
                .values()
                .find(|model| model.served_model.trim() == id)
        })
        .or_else(|| {
            models.models.values().find(|model| {
                model
                    .identity_aliases
                    .iter()
                    .any(|alias| alias.trim() == id)
            })
        })?;
    cfg.require_executable_reference().ok()?;
    Some(cfg.frame_model.as_str())
}

/// Default probe-prompt of an exact/reference model (B8) -- the first configured fingerprint. `None` -- no fingerprint
/// (degradation R3: B8 does not apply, reliance on the model check and B7).
pub fn default_probe(model_id: &str, models: &ModelsConfig) -> Option<String> {
    fingerprints_for(model_id, models)
        .into_iter()
        .next()
        .map(|f| f.probe_prompt)
}

/// Behavioral probe: the declared model's response to the probe-prompt must carry its quirk.
/// A mismatch -> the model is not the one declared -> `Bail`. A prompt not in the registry / no fingerprint ->
/// degradation (`Pass` at this layer).
pub fn behavioral_check(
    model_id: &str,
    probe_prompt: &str,
    response: &str,
    models: &ModelsConfig,
) -> Verdict {
    behavioral_check_with_reasoning(model_id, probe_prompt, response, "", models)
}

/// Behavioral probe with provider-separated reasoning. OpenRouter can return qwen thinking in
/// reasoning/reasoning_details while `content` carries only the final answer; that is the same
/// exact-model signal as the Groq `<think>` content marker for this fingerprint. Whether the reasoning
/// side channel counts is **data-driven** from the matched `Fingerprint.accepts_reasoning_side_channel`.
pub fn behavioral_check_with_reasoning(
    model_id: &str,
    probe_prompt: &str,
    response: &str,
    reasoning: &str,
    models: &ModelsConfig,
) -> Verdict {
    for fp in fingerprints_for(model_id, models) {
        if fp.probe_prompt == probe_prompt {
            let has_content_quirk = response.contains(&fp.expected_contains);
            let has_reasoning = fp.accepts_reasoning_side_channel && !reasoning.trim().is_empty();
            return if has_content_quirk || has_reasoning {
                Verdict::Pass
            } else {
                Verdict::Bail(format!(
                    "behavioral-probe (): the probe response does not carry model '{model_id}' quirk ('{}') -- model is not the declared one",
                    fp.expected_contains
                ))
            };
        }
    }
    Verdict::Pass
}

// ---- B7 full spot-check: greedy comparison against the declared model's official endpoint ----

/// The declared model's **official** reference endpoint: the buyer compares the
/// seller's greedy output against it. **Data-driven** (owned `String`) -- built from a model's configured
/// upstream. The key is read from env at runtime (`api_key_env`) and is NOT stored here (masked in logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceEndpoint {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    /// The same provider-declared greedy control used by the seller leg of B7.
    pub sample_algorithm: SampleAlgorithm,
}

/// Coarse tokenizer family by model id (`qwen`/`llama`/`gpt`/...). This is for diagnostics/tokenizer profiles only;
/// content-identity fingerprints/references are keyed by exact model id or explicit aliases.
pub fn family_of(model: &str) -> String {
    let m = model.to_ascii_lowercase();
    for fam in ["qwen", "llama", "gpt", "mixtral", "gemma"] {
        if m.contains(fam) {
            return fam.to_string();
        }
    }
    String::new()
}

/// Text normalization for comparison: lowercase, words of alphanumerics (punctuation stripped).
fn normalize_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut current_is_digit = None;
    for ch in s.chars().flat_map(char::to_lowercase) {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
                current_is_digit = None;
            }
            continue;
        }
        let is_digit = ch.is_ascii_digit();
        if current_is_digit.is_some_and(|prev| prev != is_digit) && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch);
        current_is_digit = Some(is_digit);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Agreement fraction over the **leading word prefix** (B7 spot-check), normalized by the full
/// reference length so a seller cannot raise its score by answering less. With matching declared
/// greedy controls, the same model is expected to keep a long prefix; a different model should
/// diverge early. Returns `0.0` if there is nothing to compare (empty response on either side).
pub fn prefix_agreement(seller: &str, reference: &str) -> f64 {
    let a = normalize_words(seller);
    let b = normalize_words(reference);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let matched = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    matched as f64 / b.len() as f64
}

/// B7 spot-check verdict: agreement with the reference >= threshold -> `Pass` (model confirmed);
/// otherwise -> `Bail` (greedy output diverged from the official endpoint -> not the declared model).
pub fn spotcheck_verdict(agreement: f64, threshold: f64) -> Verdict {
    if agreement >= threshold {
        Verdict::Pass
    } else {
        Verdict::Bail(format!(
            "reference-spotcheck (): agreement with the reference {agreement:.2} < threshold {threshold:.2} -- greedy output did not match the official model"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- B7: the model the provider says answered vs the deal's model ----

    #[test]
    fn every_spelling_of_the_deal_model_passes() {
        for (deal, reported) in [
            ("qwen--qwen3--32b", "qwen/qwen3-32b"),
            ("qwen--qwen3--32b", "Qwen/Qwen3-32B"),
            ("qwen--qwen3--32b", "qwen3-32b"),
            ("Qwen/Qwen3-32B", "qwen/qwen3-32b"),
            ("Qwen/Qwen3-32B", " QWEN/QWEN3-32B "),
            ("openai--gpt-oss--120b", "openai/gpt-oss-120b"),
            ("dexdo-mock", "dexdo-mock"),
        ] {
            assert_eq!(
                ModelCheck::new(deal, None).verify(reported),
                Verdict::Pass,
                "{deal} answered as {reported}"
            );
        }
    }

    #[test]
    fn another_model_bails() {
        for (deal, reported) in [
            ("qwen--qwen3--32b", "meta-llama/llama-3.3-70b-versatile"),
            ("qwen--qwen3--32b", "qwen/qwen3-235b"),
            ("Qwen/Qwen3-32B", "substituted/cheap-model"),
            ("dexdo-mock", "mock"),
        ] {
            assert!(
                matches!(
                    ModelCheck::new(deal, None).verify(reported),
                    Verdict::Bail(_)
                ),
                "{deal} answered as {reported}"
            );
        }
    }

    #[test]
    fn an_identity_alias_declared_in_the_buyers_config_passes() {
        let models = ModelsConfig::from_json(
            r#"{"models":{"llama":{"frame_model":"meta--llama-3.3--70b","base_url":"http://x",
              "served_model":"llama-3.3-70b-versatile","api_key_env":"K","tokenizer_family":"llama",
              "price_per_tick":1,"identity_aliases":["llama-3.3-70b-versatile"]}}}"#,
        )
        .unwrap();
        let check = ModelCheck::new("meta--llama-3.3--70b", Some(&models));
        assert_eq!(check.verify("llama-3.3-70b-versatile"), Verdict::Pass);
        assert!(matches!(
            ModelCheck::new("meta--llama-3.3--70b", None).verify("llama-3.3-70b-versatile"),
            Verdict::Bail(_)
        ));
    }

    /// A deal model that names no producer matches the provider's slug that prefixes one, and only
    /// that model.
    #[test]
    fn a_producer_free_deal_model_matches_the_providers_prefixed_slug() {
        let check = ModelCheck::new("Qwen3.8-27B", None);
        assert_eq!(check.verify("qwen/qwen3.8-27b"), Verdict::Pass);
        assert_eq!(
            check.verify("accounts/fireworks/models/qwen3.8-27b"),
            Verdict::Pass
        );
        assert_eq!(check.verify("Qwen3.8-27B"), Verdict::Pass);
        assert!(matches!(
            check.verify("accounts/fireworks/models/qwen3-32b"),
            Verdict::Bail(_)
        ));
        assert!(matches!(check.verify("qwen/qwen3-32b"), Verdict::Bail(_)));
        let prefixed = ModelCheck::new("qwen/qwen3-32b", None);
        assert!(matches!(
            prefixed.verify("other/qwen3-32b"),
            Verdict::Bail(_)
        ));
    }

    /// The provider's own id for the model, as the buyer's config records it, is a spelling of the
    /// deal model: a dated Anthropic id must not read as a substitution.
    #[test]
    fn the_served_model_in_the_buyers_config_passes() {
        let models = ModelsConfig::from_json(
            r#"{"models":{"claude-sonnet":{"frame_model":"claude-sonnet-4-5","base_url":"https://api.anthropic.com",
              "served_model":"claude-sonnet-4-5-20250929","api_key_env":"K","tokenizer_family":"claude",
              "price_per_tick":1}}}"#,
        )
        .unwrap();
        let check = ModelCheck::new("claude-sonnet-4-5", Some(&models));
        assert_eq!(check.verify("claude-sonnet-4-5-20250929"), Verdict::Pass);
        assert!(matches!(
            check.verify("claude-haiku-4-5-20251001"),
            Verdict::Bail(_)
        ));
        assert!(matches!(
            ModelCheck::new("claude-sonnet-4-5", None).verify("claude-sonnet-4-5-20250929"),
            Verdict::Bail(_)
        ));
    }

    // ---- B8: behavioral probes, data-driven from config ----

    /// The qwen verification data as config -- reproduces the pre-config hardcoded qwen fingerprint/reference
    /// so the qwen behavior is preserved byte-for-byte through the new data-driven path.
    fn qwen_models() -> ModelsConfig {
        ModelsConfig::from_json(
            r#"{ "models": { "qwen": {
                "frame_model": "qwen--qwen3--32b",
                "base_url": "https://api.groq.com/openai/v1",
                "served_model": "qwen/qwen3-32b",
                "api_key_env": "GROQ_API_KEY",
                "tokenizer_family": "qwen",
                "price_per_tick": 1000,
                "identity_aliases": ["Qwen/Qwen3-32B"],
                "vocab_size": 152064,
                "fingerprints": [ { "probe_prompt": "What is 17*23? Think step by step.", "expected_contains": "<think>", "accepts_reasoning_side_channel": true } ],
                "capabilities": { "sample_algorithm": "SEED" }
            } } }"#,
        )
        .expect("qwen config")
    }

    /// A TWO-model config (qwen + gpt-oss-20b), both served by Groq -- proves the verification mechanism is
    /// general: BOTH yield real fingerprints + reference purely from config, no code change. gpt-oss's
    /// fingerprint sets `accepts_reasoning_side_channel = false` (it is not a reasoning model).
    fn two_models() -> ModelsConfig {
        ModelsConfig::from_json(
            r#"{ "models": {
              "qwen": {
                "frame_model": "qwen--qwen3--32b",
                "base_url": "https://api.groq.com/openai/v1",
                "served_model": "qwen/qwen3-32b",
                "api_key_env": "GROQ_API_KEY",
                "tokenizer_family": "qwen",
                "price_per_tick": 1000,
                "identity_aliases": ["Qwen/Qwen3-32B"],
                "vocab_size": 152064,
                "fingerprints": [ { "probe_prompt": "What is 17*23? Think step by step.", "expected_contains": "<think>", "accepts_reasoning_side_channel": true } ],
                "capabilities": { "sample_algorithm": "SEED" }
              },
              "gpt-oss-20b": {
                "frame_model": "openai--gpt-oss--20b",
                "base_url": "https://api.groq.com/openai/v1",
                "served_model": "openai/gpt-oss-20b",
                "api_key_env": "GROQ_API_KEY",
                "tokenizer_family": "gpt",
                "price_per_tick": 500,
                "identity_aliases": ["openai/gpt-oss-20b"],
                "vocab_size": 100352,
                "fingerprints": [ { "probe_prompt": "Reply with exactly: OSSMARK", "expected_contains": "OSSMARK" } ],
                "capabilities": { "sample_algorithm": "SEED" }
              }
            } }"#,
        )
        .expect("two-model config")
    }

    #[test]
    fn behavioral_probe_matches_fingerprint_passes() {
        // qwen3-32b's response to the probe carries the <think> quirk -> Pass.
        let cfg = qwen_models();
        let probe = default_probe("qwen--qwen3--32b", &cfg).unwrap();
        assert_eq!(
            behavioral_check(
                "qwen--qwen3--32b",
                &probe,
                "<think>\nOkay...</think> 391",
                &cfg
            ),
            Verdict::Pass
        );
    }

    #[test]
    fn behavioral_probe_missing_quirk_bails() {
        // qwen3-32b declared, but the response lacks <think> (substitute model, not reasoning) -> Bail.
        let cfg = qwen_models();
        let probe = default_probe("qwen--qwen3--32b", &cfg).unwrap();
        assert!(matches!(
            behavioral_check("qwen--qwen3--32b", &probe, "391", &cfg),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn behavioral_probe_openrouter_reasoning_passes_without_content_think() {
        let cfg = qwen_models();
        let probe = default_probe("Qwen/Qwen3-32B", &cfg).unwrap();
        assert_eq!(
            behavioral_check_with_reasoning(
                "Qwen/Qwen3-32B",
                &probe,
                "391",
                "We need compute 17 * 23 step by step.",
                &cfg
            ),
            Verdict::Pass
        );
    }

    #[test]
    fn behavioral_probe_plain_answer_without_reasoning_still_bails() {
        let cfg = qwen_models();
        let probe = default_probe("Qwen/Qwen3-32B", &cfg).unwrap();
        assert!(matches!(
            behavioral_check_with_reasoning("Qwen/Qwen3-32B", &probe, "391", "", &cfg),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn reasoning_side_channel_comes_from_fingerprint_flag_not_registry_identity() {
        // The reasoning side channel is now DATA-DRIVEN from `Fingerprint.accepts_reasoning_side_channel`
        // (not a hardcoded `is_registry_qwen3_32b` special-case). qwen's fingerprint sets it true, so the
        // served form ALSO accepts provider-separated reasoning; gpt-oss's fingerprint leaves it false, so a
        // reasoning-only response is NOT accepted for gpt-oss. This is the intended generalization.
        let cfg = two_models();
        let qwen_probe = default_probe("qwen/qwen3-32b", &cfg).unwrap();
        assert_eq!(
            behavioral_check_with_reasoning(
                "qwen/qwen3-32b",
                &qwen_probe,
                "391",
                "provider-separated reasoning",
                &cfg
            ),
            Verdict::Pass,
            "flag=true -> reasoning side channel accepted for any spelling that resolves to the entry"
        );
        let oss_probe = default_probe("openai--gpt-oss--20b", &cfg).unwrap();
        assert!(
            matches!(
                behavioral_check_with_reasoning(
                    "openai--gpt-oss--20b",
                    &oss_probe,
                    "wrong answer, no mark",
                    "some reasoning text",
                    &cfg
                ),
                Verdict::Bail(_)
            ),
            "flag=false -> reasoning side channel does NOT rescue a missing content quirk"
        );
    }

    #[test]
    fn behavioral_probe_exact_qwen_aliases_preserve_fingerprint() {
        let cfg = qwen_models();
        let canonical_probe = default_probe("qwen--qwen3--32b", &cfg).unwrap();
        let served_probe = default_probe("qwen/qwen3-32b", &cfg).unwrap();
        let registry_probe = default_probe("Qwen/Qwen3-32B", &cfg).unwrap();
        assert_eq!(canonical_probe, served_probe);
        assert_eq!(canonical_probe, registry_probe);
        assert!(matches!(
            behavioral_check("qwen/qwen3-32b", &served_probe, "391", &cfg),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn unknown_qwen_variant_does_not_inherit_qwen3_fingerprint() {
        let cfg = qwen_models();
        let qwen3_probe = default_probe("qwen--qwen3--32b", &cfg).unwrap();
        assert!(default_probe("qwen--qwen3.6--27b", &cfg).is_none());
        assert_eq!(
            behavioral_check("qwen--qwen3.6--27b", &qwen3_probe, "391", &cfg),
            Verdict::Pass
        );
    }

    #[test]
    fn behavioral_probe_unknown_family_degrades() {
        // No exact-model fingerprint -> degradation (Pass), we do not fabricate.
        let cfg = qwen_models();
        assert!(default_probe("unknown", &cfg).is_none());
        assert_eq!(behavioral_check("unknown", "x", "y", &cfg), Verdict::Pass);
    }

    #[test]
    fn behavioral_probe_non_registered_prompt_skips() {
        // A prompt not in the probe registry -> the layer does not apply (Pass).
        let cfg = qwen_models();
        assert_eq!(
            behavioral_check("qwen", "random prompt", "no think here", &cfg),
            Verdict::Pass
        );
    }

    #[test]
    fn second_model_gpt_oss_is_fully_verifiable_from_config() {
        // Mandatory (Track 2): a config with TWO models yields real fingerprints + reference for BOTH,
        // and behavioral_check bails a wrong-content response for gpt-oss just like qwen -- purely from data.
        let cfg = two_models();

        // Fingerprints resolve for BOTH (canonical + served spellings).
        let qwen_probe = default_probe("qwen--qwen3--32b", &cfg).expect("qwen fingerprint");
        let oss_probe = default_probe("openai--gpt-oss--20b", &cfg).expect("gpt-oss fingerprint");
        assert_eq!(
            default_probe("openai/gpt-oss-20b", &cfg).as_deref(),
            Some(oss_probe.as_str())
        );
        assert_ne!(qwen_probe, oss_probe, "each model has its own probe");

        // References resolve for BOTH (Groq base_url, per-model served id).
        let rq = reference_endpoint_for("qwen--qwen3--32b", &cfg).expect("qwen reference");
        assert_eq!(rq.model, "qwen/qwen3-32b");
        let ro = reference_endpoint_for("openai--gpt-oss--20b", &cfg).expect("gpt-oss reference");
        assert_eq!(ro.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(ro.model, "openai/gpt-oss-20b");
        assert_eq!(ro.api_key_env, "GROQ_API_KEY");
        assert!(reference_endpoint_for("openai/gpt-oss-20b", &cfg).is_some());

        // Behavioral check bails a wrong-content gpt-oss response, and passes the correct quirk.
        assert!(
            matches!(
                behavioral_check(
                    "openai--gpt-oss--20b",
                    &oss_probe,
                    "totally different",
                    &cfg
                ),
                Verdict::Bail(_)
            ),
            "wrong content for gpt-oss -> Bail (same as qwen)"
        );
        assert_eq!(
            behavioral_check(
                "openai--gpt-oss--20b",
                &oss_probe,
                "the mark is OSSMARK here",
                &cfg
            ),
            Verdict::Pass,
            "correct gpt-oss quirk -> Pass"
        );
    }

    #[test]
    fn executable_reference_requires_exact_bound_identity_and_key() {
        let mut cfg = qwen_models();
        cfg.models.get_mut("qwen").unwrap().api_key_env = "PATH".into();

        for model_id in [
            "qwen",
            "qwen--qwen3--32b",
            "qwen/qwen3-32b",
            "Qwen/Qwen3-32B",
        ] {
            assert_eq!(
                executable_reference_model_for(model_id, &cfg),
                Some("qwen--qwen3--32b"),
                "{model_id}"
            );
        }
        assert_eq!(
            executable_reference_model_for("Qwen/Qwen3-32B-unlisted", &cfg),
            None
        );

        cfg.models.get_mut("qwen").unwrap().api_key_env =
            "DEXDO_TEST_REFERENCE_KEY_MISSING_520".into();
        assert_eq!(
            executable_reference_model_for("qwen--qwen3--32b", &cfg),
            None
        );
    }

    // ---- B7 full spot-check ----

    #[test]
    fn reference_endpoint_registry() {
        let cfg = qwen_models();
        let r =
            reference_endpoint_for("qwen--qwen3--32b", &cfg).expect("qwen3-32b has a reference");
        assert_eq!(r.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(r.model, "qwen/qwen3-32b");
        assert_eq!(r.api_key_env, "GROQ_API_KEY");
        assert!(reference_endpoint_for("qwen/qwen3-32b", &cfg).is_some());
        // The live ModelRegistry name is provider-neutral (identity alias only); do not compare its
        // output against the configured Groq reference here.
        assert!(reference_endpoint_for("Qwen/Qwen3-32B", &cfg).is_none());
        // No reference -> degradation (R3).
        assert!(reference_endpoint_for("qwen--qwen3.6--27b", &cfg).is_none());
        assert!(reference_endpoint_for("llama", &cfg).is_none());
        assert!(reference_endpoint_for("mock", &cfg).is_none());
    }

    #[test]
    fn issue_1951_none_sample_algorithm_degrades_b7_before_network() {
        let cfg = ModelsConfig::from_json(
            r#"{"models":{"m":{"frame_model":"vendor--model--v1",
              "base_url":"https://provider.example/v1","served_model":"provider/model",
              "api_key_env":"PROVIDER_KEY","tokenizer_family":"vendor","price_per_tick":1,
              "capabilities":{"sample_algorithm":"NONE"}}}}"#,
        )
        .expect("NONE profile parses");

        assert!(
            reference_endpoint_for("m", &cfg).is_none(),
            "NONE has no comparable B7 reference and must degrade before any request"
        );
    }

    #[test]
    fn prefix_agreement_identical_and_subset() {
        // greedy of one model: identical output -> 1.0.
        assert_eq!(
            prefix_agreement("The answer is 42.", "the answer is 42"),
            1.0
        );
        // A seller answer that is a PREFIX of the reference is the attack is about: under the
        // old `min(a,b)` denominator it scored a perfect 1.0, so answering one word was the cheapest
        // way to pass any threshold. Normalised by the reference, two matching words out of four are
        // worth exactly half. This line asserted the defect; it now asserts the guarantee.
        assert_eq!(prefix_agreement("the answer", "the answer is 42"), 0.5);
        // Punctuation/case do not matter (normalization).
        assert_eq!(prefix_agreement("Hello,   World!", "hello world"), 1.0);
        // Streaming providers may emit no whitespace at token boundaries around numbers/operators.
        assert_eq!(prefix_agreement("compute17 *23", "compute 17 * 23"), 1.0);
    }

    #[test]
    fn prefix_agreement_divergent_and_empty() {
        // A different model: divergence from the first word -> 0.0.
        assert_eq!(prefix_agreement("foo bar baz", "qux bar baz"), 0.0);
        // Partial agreement: 2 of 3 leading words.
        let a = prefix_agreement("the cat sat", "the cat ran");
        assert!((a - 2.0 / 3.0).abs() < 1e-9, "got {a}");
        // Empty response -> nothing to confirm -> 0.0.
        assert_eq!(prefix_agreement("", "something"), 0.0);
        assert_eq!(prefix_agreement("something", ""), 0.0);
    }

    #[test]
    fn spotcheck_verdict_threshold() {
        // High agreement (>= threshold) -> Pass; below -> Bail. The boundary is inclusive.
        assert_eq!(
            spotcheck_verdict(1.0, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
        assert_eq!(
            spotcheck_verdict(DEFAULT_SPOTCHECK_THRESHOLD, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
        assert!(matches!(
            spotcheck_verdict(0.1, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Bail(_)
        ));
    }

    /// the threshold is calibrated against measured run-to-run agreement, NOT against an
    /// assumed determinism. Both figures come from the real [`DEFAULT_SPOTCHECK_PROBE`] at
    /// `CONTENT_PROBE_MAX_TOKENS`, greedy, against Groq: an honest seller (the same model on both
    /// legs) floors at 0.41 (`qwen/qwen3-32b` branches at word 21 of 51), a substituting seller
    /// (6 different models) tops out at 0.02 (branch at word 0/1). Both populations must stay on
    /// their own side of the threshold, or the gate refuses honest sellers again.
    const _: () =
        assert!(DEFAULT_SPOTCHECK_THRESHOLD > 0.02 && DEFAULT_SPOTCHECK_THRESHOLD <= 0.41);

    #[test]
    fn spotcheck_threshold_separates_measured_populations() {
        // Honest floor measured live -- must PASS.
        assert_eq!(
            spotcheck_verdict(0.41, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
        // Substitution ceiling measured live over 6 models -- must BAIL.
        assert!(matches!(
            spotcheck_verdict(0.02, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn spotcheck_catches_substitution_end_to_end() {
        // End to end across the functions: reference greedy output vs a substituting seller's diverging output -> Bail.
        let reference = "To compute 17 times 23 we multiply step by step";
        let scammer = "Sure here is a totally different cheaper answer";
        let agreement = prefix_agreement(scammer, reference);
        assert!(
            matches!(
                spotcheck_verdict(agreement, DEFAULT_SPOTCHECK_THRESHOLD),
                Verdict::Bail(_)
            ),
            "model substitution is caught by the spot-check (agreement={agreement})"
        );
        // Honest seller, the same greedy output -> Pass.
        let honest = "To compute 17 times 23 we multiply";
        let agreement_ok = prefix_agreement(honest, reference);
        assert_eq!(
            spotcheck_verdict(agreement_ok, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
    }
}

#[cfg(test)]
mod issue_939_scoring_tests {
    use super::*;

    const REFERENCE: &str = "one two three four five six seven eight nine ten";

    fn assert_score_and_bail(seller: &str, expected: f64) {
        let agreement = prefix_agreement(seller, REFERENCE);
        assert!(
            (agreement - expected).abs() < 1e-12,
            "expected agreement {expected}, got {agreement}"
        );
        assert!(matches!(
            spotcheck_verdict(agreement, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn issue_939_one_matching_word_uses_reference_denominator() {
        assert_score_and_bail("one", 1.0 / 10.0);
    }

    #[test]
    fn issue_939_four_words_with_one_match_use_reference_denominator() {
        assert_score_and_bail("one wrong three four", 1.0 / 10.0);
    }

    #[test]
    fn issue_939_eight_words_with_two_matches_use_reference_denominator() {
        assert_score_and_bail("one two wrong four five six seven eight", 2.0 / 10.0);
    }

    #[test]
    fn issue_939_full_length_substitution_still_bails() {
        assert_score_and_bail("wrong two three four five six seven eight nine ten", 0.0);
    }

    #[test]
    fn issue_939_full_length_match_still_passes() {
        let agreement = prefix_agreement(REFERENCE, REFERENCE);
        assert_eq!(agreement, 1.0);
        assert_eq!(
            spotcheck_verdict(agreement, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
    }
}
