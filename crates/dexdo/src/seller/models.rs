//! Seller model config layer: model-agnosticism is achieved through
//! **OpenAI compatibility + config + a local proxy**, without vendor branches in dexdo.
//! Any model reachable over an OpenAI-compatible API (a real provider OR a local
//! proxy in front of dexdo) is described by a **config entry** and selected **by name**.
//! The format is **JSON**(not yaml/toml): `serde_json` is already in the build graph, we don't
//! introduce a new dependency, and it is consistent with the rest of the repo's configs
//! (`deployed.shellnet.json`, `endpoints.json`). Loading/selection is **fail-loud**: a corrupt file,
//! an empty config, an unknown model -> an explicit error, not a silent degradation.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Model capabilities -- what the endpoint actually supports. Consumed by
/// (capability-aware request: don't send `logprobs` to strict endpoints that respond `400`).
/// The default is **conservative**(`logprobs=false`): an unknown/undescribed endpoint does not break.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Whether the upstream supports the `logprobs` field.
    #[serde(default)]
    pub logprobs: bool,
    /// How many top alternatives to request when `logprobs=true`. `None` -> don't send `top_logprobs`.
    #[serde(default)]
    pub top_logprobs: Option<u32>,
}

/// One sellable model entry in the config.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// The canonical market id(R1): the seller **forces** it; the buyer's `model` is not trusted.
    pub frame_model: String,
    /// OpenAI-compatible base URL(a real provider **or** a local proxy), without a trailing path.
    pub base_url: String,
    /// The model id at the upstream -- what to send in the request's `model` field.
    pub served_model: String,
    /// Name of the env variable holding the key -- **per-model/provider**(not one global `GROQ_API_KEY`).
    pub api_key_env: String,
    /// The tokenizer family for `SignalManifest.tokenizer_family` -- instead of
    /// a substring hardcode. The buyer's profile matches by family.
    pub tokenizer_family: String,
    /// Default tick price in SHELL.
    pub price_per_tick: u64,
    /// Upstream capabilities. By default -- conservative(no `logprobs`).
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// The models config file: key(name/alias) -> entry. The key may coincide with `frame_model`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub models: BTreeMap<String, ModelConfig>,
}

impl ModelsConfig {
    /// Load and validate the config -- **fail-loud**(no file / corrupt JSON / empty -> error).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read models config {}", path.display()))?;
        Self::from_json(&text).with_context(|| format!("models config {}", path.display()))
    }

    /// Parse the config from a JSON string -- **fail-loud**.
    pub fn from_json(text: &str) -> Result<Self> {
        let cfg: ModelsConfig = serde_json::from_str(text).context("parse JSON")?;
        if cfg.models.is_empty() {
            bail!("no models in the config");
        }
        Ok(cfg)
    }

    /// Select a model by name(config key **or** `frame_model`) -- **fail-loud**: an unknown
    /// model -> an error with the list of available ones(not a silent default).
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
    /// Check that the key's env variable is set and non-empty -- **fail-loud** (:
    /// "a missing key env variable -> an explicit error"). The key value is neither returned nor
    /// logged -- only the fact of its presence(read at runtime by the adapter via `api_key_env`).
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
            "empty -> error"
        );
        assert!(
            ModelsConfig::from_json("{ not json").is_err(),
            "corrupt -> error"
        );
    }

    #[test]
    fn capabilities_default_is_conservative_off() {
        // An entry without `capabilities` -> the conservative default(no logprobs),.
        let json = r#"{"models":{"m":{"frame_model":"f","base_url":"http://x","served_model":"s",
          "api_key_env":"K","tokenizer_family":"fam","price_per_tick":1}}}"#;
        let cfg = ModelsConfig::from_json(json).unwrap();
        let m = cfg.get("m").unwrap();
        assert!(!m.capabilities.logprobs, "default capability -- off");
        assert_eq!(m.capabilities.top_logprobs, None);
    }

    #[test]
    fn missing_api_key_env_fails_loud() {
        let cfg = ModelsConfig::from_json(SAMPLE).unwrap();
        let m = cfg.get("qwen").unwrap();
        // The env variable is definitely absent in the test environment(the name is unique).
        let mut m2 = m.clone();
        m2.api_key_env = "DEXDO_TEST_NO_SUCH_KEY_ENV_X9".into();
        assert!(m2.require_api_key_present().is_err());
    }
}
