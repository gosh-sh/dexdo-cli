//! Seller model config layer (Directive 11): model-agnosticism is achieved through
//! **OpenAI compatibility + config + a local proxy**, without vendor branches in dexdo.
//! Any model reachable over an OpenAI-compatible API (a real provider OR a local
//! proxy in front of dexdo) is described by a **config entry** and selected **by name**.
//!
//! The format is **JSON** (not yaml/toml): `serde_json` is already in the build graph, we don't
//! introduce a new dependency (AGENTS.md §1), and it is consistent with the rest of the repo's configs
//! (`deployed.shellnet.json`, `endpoints.json`). Loading/selection is **fail-loud**: a corrupt file,
//! an empty config, an unknown model → an explicit error, not a silent degradation.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Model capabilities — what the endpoint actually supports. Consumed by Directive 12
/// (capability-aware request: don't send `logprobs` to strict endpoints that respond `400`).
/// The default is **conservative** (`logprobs=false`): an unknown/undescribed endpoint does not break.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Whether the upstream supports the `logprobs` field (verification signal B6, §10.1.2).
    #[serde(default)]
    pub logprobs: bool,
    /// How many top alternatives to request when `logprobs=true`. `None` → don't send `top_logprobs`.
    #[serde(default)]
    pub top_logprobs: Option<u32>,
}

/// One behavioral-probe fingerprint (B8/§10.1.4) declared for a model in config: a deterministic
/// probe prompt and a quirk the exact/reference model characteristically emits. Data-driven so the
/// content-identity check generalizes to any model, not just a hardcoded qwen registry.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FingerprintCfg {
    /// The deterministic probe prompt the buyer sends (B8).
    pub probe_prompt: String,
    /// A marker the model's response characteristically contains (e.g. qwen `<think>`).
    pub expected_contains: String,
    /// Some providers expose the thinking out-of-band (reasoning side channel) instead of embedding
    /// the marker in `content`; when true, non-empty provider reasoning is accepted as the same signal.
    #[serde(default)]
    pub accepts_reasoning_side_channel: bool,
}

/// One sellable model entry in the config.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// The canonical market id (R1): the seller **forces** it; the buyer's `model` is not trusted.
    pub frame_model: String,
    /// OpenAI-compatible base URL (a real provider **or** a local proxy), without a trailing path.
    pub base_url: String,
    /// The model id at the upstream — what to send in the request's `model` field.
    pub served_model: String,
    /// Name of the env variable holding the key — **per-model/provider** (not one global `GROQ_API_KEY`).
    pub api_key_env: String,
    /// The tokenizer family for `SignalManifest.tokenizer_family` (§6/§10.1.1) — instead of
    /// a substring hardcode. The buyer's profile matches by family.
    pub tokenizer_family: String,
    /// Default tick price in SHELL.
    pub price_per_tick: u64,
    /// Upstream capabilities (Directive 12). By default — conservative (no `logprobs`).
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Extra content-identity spellings the served model self-reports (e.g. qwen `["Qwen/Qwen3-32B"]`),
    /// used to resolve fingerprints/vocab for the registry/provider name. Empty by default.
    #[serde(default)]
    pub identity_aliases: Vec<String>,
    /// Tokenizer vocabulary size for the B5 tokenizer-check (§10.1.1). `None` → fall back to the
    /// `tokenizer_family` mapping. qwen 152064, llama 128256, gpt 100352.
    #[serde(default)]
    pub vocab_size: Option<u32>,
    /// Behavioral fingerprints (B8/§10.1.4) for the exact model. Empty → no B8 (degradation R3).
    #[serde(default)]
    pub fingerprints: Vec<FingerprintCfg>,
}

impl ModelConfig {
    /// The B7-full reference endpoint (§10.1.3) **derived** from the upstream fields — the reference IS
    /// the configured upstream (`base_url` + `served_model` + `api_key_env`). No dedicated config field.
    /// The key is read from env at runtime by `api_key_env` and is never stored here (masked in logs).
    pub fn reference_endpoint(&self) -> crate::buyer::verify::ReferenceEndpoint {
        crate::buyer::verify::ReferenceEndpoint {
            base_url: self.base_url.clone(),
            model: self.served_model.clone(),
            api_key_env: self.api_key_env.clone(),
        }
    }
}

/// The models config file: key (name/alias) → entry. The key may coincide with `frame_model`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub models: BTreeMap<String, ModelConfig>,
}

impl ModelsConfig {
    /// An empty config (no models). Used by the **buyer** when no `--models` file is present: every model then
    /// has no verification data → the content-identity policy fails closed (unless `--allow-unverified-model`).
    /// Distinct from a present-but-empty config file, which [`from_json`](Self::from_json) still rejects.
    pub fn empty() -> Self {
        Self {
            models: BTreeMap::new(),
        }
    }

    /// Buyer-side lenient load (#281): an ABSENT `--models` path yields an empty config (fail-closed per model),
    /// while a present file must parse and be non-empty (**fail-loud** on corrupt/empty). The seller path uses
    /// the strict [`load`](Self::load) — it must always have a model to serve.
    pub fn load_or_empty(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::empty())
        }
    }

    /// Load and validate the config — **fail-loud** (no file / corrupt JSON / empty → error).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read models config {}", path.display()))?;
        Self::from_json(&text).with_context(|| format!("models config {}", path.display()))
    }

    /// Parse the config from a JSON string — **fail-loud**.
    pub fn from_json(text: &str) -> Result<Self> {
        let cfg: ModelsConfig = serde_json::from_str(text).context("parse JSON")?;
        if cfg.models.is_empty() {
            bail!("no models in the config");
        }
        Ok(cfg)
    }

    /// Select a model by name (config key **or** `frame_model`) — **fail-loud**: an unknown
    /// model → an error with the list of available ones (not a silent default).
    pub fn get(&self, name: &str) -> Result<&ModelConfig> {
        if let Some(m) = self.models.get(name) {
            return Ok(m);
        }
        if let Some(m) = self.models.values().find(|m| m.frame_model == name) {
            return Ok(m);
        }
        let available: Vec<&str> = self.models.keys().map(String::as_str).collect();
        bail!("model \"{name}\" not found in the config; available: {available:?}");
    }
}

impl ModelConfig {
    /// Check that the key's env variable is set and non-empty — **fail-loud** (Directive 11:
    /// "a missing key env variable → an explicit error"). The key value is neither returned nor
    /// logged — only the fact of its presence (read at runtime by the adapter via `api_key_env`).
    pub fn require_api_key_present(&self) -> Result<()> {
        match std::env::var(&self.api_key_env) {
            Ok(v) if !v.is_empty() => Ok(()),
            _ => bail!(
                "the upstream key for model \"{}\" is not set: env variable {} is empty/missing",
                self.frame_model,
                self.api_key_env
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "models": {
        "qwen": {
          "frame_model": "qwen--qwen3--32b",
          "base_url": "https://api.groq.com/openai/v1",
          "served_model": "qwen/qwen3-32b",
          "api_key_env": "GROQ_API_KEY",
          "tokenizer_family": "qwen",
          "price_per_tick": 1000,
          "capabilities": { "logprobs": true, "top_logprobs": 5 }
        }
      }
    }"#;

    #[test]
    fn loads_and_selects_by_key_and_frame_model() {
        let cfg = ModelsConfig::from_json(SAMPLE).expect("parse");
        // By config key.
        let by_key = cfg.get("qwen").expect("by key");
        assert_eq!(by_key.frame_model, "qwen--qwen3--32b");
        assert_eq!(by_key.served_model, "qwen/qwen3-32b");
        assert_eq!(by_key.tokenizer_family, "qwen");
        assert!(by_key.capabilities.logprobs);
        assert_eq!(by_key.capabilities.top_logprobs, Some(5));
        // By the canonical frame_model.
        let by_frame = cfg.get("qwen--qwen3--32b").expect("by frame");
        assert_eq!(by_frame.frame_model, by_key.frame_model);
        assert_eq!(by_frame.served_model, "qwen/qwen3-32b");
    }

    #[test]
    fn unknown_model_fails_loud_with_list() {
        let cfg = ModelsConfig::from_json(SAMPLE).unwrap();
        let err = cfg.get("gpt-4o").unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("qwen"), "list of available: {err}");
    }

    #[test]
    fn empty_and_broken_configs_fail_loud() {
        assert!(
            ModelsConfig::from_json(r#"{"models":{}}"#).is_err(),
            "empty → error"
        );
        assert!(
            ModelsConfig::from_json("{ not json").is_err(),
            "corrupt → error"
        );
    }

    #[test]
    fn capabilities_default_is_conservative_off() {
        // An entry without `capabilities` → the conservative default (no logprobs), Directive 12.
        let json = r#"{"models":{"m":{"frame_model":"f","base_url":"http://x","served_model":"s",
          "api_key_env":"K","tokenizer_family":"fam","price_per_tick":1}}}"#;
        let cfg = ModelsConfig::from_json(json).unwrap();
        let m = cfg.get("m").unwrap();
        assert!(!m.capabilities.logprobs, "default capability — off");
        assert_eq!(m.capabilities.top_logprobs, None);
    }

    #[test]
    fn missing_api_key_env_fails_loud() {
        let cfg = ModelsConfig::from_json(SAMPLE).unwrap();
        let m = cfg.get("qwen").unwrap();
        // The env variable is definitely absent in the test environment (the name is unique).
        let mut m2 = m.clone();
        m2.api_key_env = "DEXDO_TEST_NO_SUCH_KEY_ENV_X9".into();
        assert!(m2.require_api_key_present().is_err());
    }

    #[test]
    fn verification_fields_default_when_absent_backward_compatible() {
        // The SAMPLE (and any pre-existing single-model models.json) has no identity_aliases / vocab_size /
        // fingerprints — it MUST still load, with the new fields defaulting to empty/None (backward-compat).
        let cfg = ModelsConfig::from_json(SAMPLE)
            .expect("legacy config without verification fields loads");
        let m = cfg.get("qwen").unwrap();
        assert!(
            m.identity_aliases.is_empty(),
            "identity_aliases defaults to empty"
        );
        assert_eq!(m.vocab_size, None, "vocab_size defaults to None");
        assert!(m.fingerprints.is_empty(), "fingerprints defaults to empty");
    }

    #[test]
    fn verification_fields_parse_when_present() {
        let json = r#"{
          "models": {
            "qwen": {
              "frame_model": "qwen--qwen3--32b",
              "base_url": "https://api.groq.com/openai/v1",
              "served_model": "qwen/qwen3-32b",
              "api_key_env": "GROQ_API_KEY",
              "tokenizer_family": "qwen",
              "price_per_tick": 1000,
              "identity_aliases": ["Qwen/Qwen3-32B"],
              "vocab_size": 152064,
              "fingerprints": [
                { "probe_prompt": "What is 17*23? Think step by step.", "expected_contains": "<think>", "accepts_reasoning_side_channel": true }
              ]
            }
          }
        }"#;
        let cfg = ModelsConfig::from_json(json).expect("parse");
        let m = cfg.get("qwen").unwrap();
        assert_eq!(m.identity_aliases, vec!["Qwen/Qwen3-32B".to_string()]);
        assert_eq!(m.vocab_size, Some(152_064));
        assert_eq!(m.fingerprints.len(), 1);
        assert_eq!(
            m.fingerprints[0].probe_prompt,
            "What is 17*23? Think step by step."
        );
        assert_eq!(m.fingerprints[0].expected_contains, "<think>");
        assert!(m.fingerprints[0].accepts_reasoning_side_channel);
        // The B7-full reference is derived from the upstream fields (no dedicated field).
        let r = m.reference_endpoint();
        assert_eq!(r.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(r.model, "qwen/qwen3-32b");
        assert_eq!(r.api_key_env, "GROQ_API_KEY");
    }

    #[test]
    fn fingerprint_reasoning_flag_defaults_false() {
        // accepts_reasoning_side_channel is #[serde(default)] — a fingerprint may omit it.
        let json = r#"{
          "models": { "m": {
            "frame_model": "vendor--fam--v1", "base_url": "http://x", "served_model": "vendor/fam-v1",
            "api_key_env": "K", "tokenizer_family": "fam", "price_per_tick": 1,
            "fingerprints": [ { "probe_prompt": "p", "expected_contains": "q" } ]
          } }
        }"#;
        let cfg = ModelsConfig::from_json(json).unwrap();
        let m = cfg.get("m").unwrap();
        assert!(!m.fingerprints[0].accepts_reasoning_side_channel);
    }
}
