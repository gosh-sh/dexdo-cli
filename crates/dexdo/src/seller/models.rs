//! Seller model config layer: model-agnosticism is achieved through
//! provider adapters + config. Models are described by a **config entry** and selected **by name**.

//! The format is **JSON** (not yaml/toml): `serde_json` is already in the build graph, we don't
//! introduce a new dependency, and it is consistent with the rest of the repo's configs
//! (the deployment manifest, `endpoints.json`). Loading/selection is **fail-loud**: a corrupt file,
//! an empty config, an unknown model -> an explicit error, not a silent degradation.

use anyhow::{bail, Context, Result};
pub use dexdo_core::params::SampleAlgorithm;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Model capabilities -- what the endpoint actually supports.

/// Unlike the rest of the config this struct does NOT deny unknown fields: the retired `logprobs` /
/// `top_logprobs` keys are still present in already-deployed `models.json` files, and rejecting them
/// would take every such seller off the market on upgrade. They are ignored, not honoured.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Capabilities {
    /// The model's **own** maximum output length (completion tokens) at this endpoint. The outbound
    /// generation limit is clamped to it in addition to the deal budget: a deal budget is
    /// `ticks * TICK_SIZE` tokens, which every real provider rejects with `400` (Groq answers
    /// `` `max_tokens` must be less than or equal to `40960` ``), so a deal-only clamp made every
    /// delivery fail. `None` = **unknown** -> serving that model fails closed with an explicit
    /// error BEFORE the provider is contacted, rather than sending an unbounded value.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Which one provider-specific field controls B7 reproducibility. A non-`NONE` declaration
    /// asserts that the exact endpoint was found reproducible with that wire shape. `NONE` is the
    /// conservative default: send no optional field and degrade B7 rather than compare stochastic
    /// generations.
    #[serde(default)]
    pub sample_algorithm: SampleAlgorithm,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            max_output_tokens: None,
            sample_algorithm: dexdo_core::params::OPENAI_COMPATIBLE_SAMPLE_ALGORITHM_DEFAULT,
        }
    }
}

/// One behavioral-probe fingerprint declared for a model in config: a deterministic
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
    /// Provider API base URL, without the operation path.
    pub base_url: String,
    /// The model id at the upstream -- what to send in the request's `model` field.
    pub served_model: String,
    /// Name of the env variable holding the key -- **per-model/provider** (not one global `GROQ_API_KEY`).
    pub api_key_env: String,
    /// The tokenizer family for `SignalManifest.tokenizer_family` -- instead of
    /// a substring hardcode. The buyer's profile matches by family.
    pub tokenizer_family: String,
    /// Default tick price in whole SHELL: `3` is three SHELL a tick, the same figure
    /// `--price-per-tick` takes. No runtime path reads it -- the price a seller offers at comes
    /// from `--price-per-tick` or from the market manifest -- so it is a declaration in the file
    /// and nothing more.
    pub price_per_tick: u64,
    /// Upstream capabilities.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Extra content-identity spellings the served model self-reports (e.g. qwen `["Qwen/Qwen3-32B"]`),
    /// used to resolve fingerprints/vocab for the registry/provider name. Empty by default.
    #[serde(default)]
    pub identity_aliases: Vec<String>,
    /// Tokenizer vocabulary size for the B5 tokenizer-check. `None` -> fall back to the
    /// `tokenizer_family` mapping. qwen 152064, llama 128256, gpt 100352.
    #[serde(default)]
    pub vocab_size: Option<u32>,
    /// Behavioral fingerprints for the exact model. Empty -> no B8 (degradation R3).
    #[serde(default)]
    pub fingerprints: Vec<FingerprintCfg>,
}

impl ModelConfig {
    /// The B7-full reference endpoint **derived** from the upstream fields -- the reference IS
    /// the configured upstream (`base_url` + `served_model` + `api_key_env`). No dedicated config field.
    /// The key is read from env at runtime by `api_key_env` and is never stored here (masked in logs).
    pub fn reference_endpoint(&self) -> crate::buyer::verify::ReferenceEndpoint {
        crate::buyer::verify::ReferenceEndpoint {
            base_url: self.base_url.clone(),
            model: self.served_model.clone(),
            api_key_env: self.api_key_env.clone(),
            sample_algorithm: self.capabilities.sample_algorithm,
        }
    }
}

/// The models config file: key (name/alias) -> entry. The key may coincide with `frame_model`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub models: BTreeMap<String, ModelConfig>,
}

impl ModelsConfig {
    /// An empty config (no models). Used by the **buyer** when no `--models` file is present: every model then
    /// has no verification data -> the content-identity policy fails closed (unless `--allow-unverified-model`).
    /// Distinct from a present-but-empty config file, which [`from_json`](Self::from_json) still rejects.
    pub fn empty() -> Self {
        Self {
            models: BTreeMap::new(),
        }
    }

    /// Buyer-side lenient load: an ABSENT `--models` path yields an empty config (fail-closed per model),
    /// while a present file must parse and be non-empty (**fail-loud** on corrupt/empty). The seller path uses
    /// the strict [`load`](Self::load) -- it must always have a model to serve.
    pub fn load_or_empty(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::empty())
        }
    }

    /// Load and validate the config -- **fail-loud** (no file / corrupt JSON / empty -> error).
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

    /// Select a model by name (config key **or** `frame_model`) -- **fail-loud**: an unknown
    /// model -> an error with the list of available ones (not a silent default).
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
    /// logged -- only the fact of its presence (read at runtime by the adapter via `api_key_env`).
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

    /// Validate that this entry can be used as the buyer's exact-model reference.
    pub fn require_executable_reference(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.base_url)
            .with_context(|| format!("invalid reference endpoint for \"{}\"", self.frame_model))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!(
                "reference endpoint for \"{}\" must be an absolute http(s) URL",
                self.frame_model
            );
        }
        if self.served_model.trim().is_empty() {
            bail!(
                "reference served_model for \"{}\" is empty",
                self.frame_model
            );
        }
        self.require_api_key_present()
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
          "price_per_tick": 1,
          "capabilities": { "logprobs": true, "top_logprobs": 5, "max_output_tokens": 40960 }
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
        assert_eq!(by_key.capabilities.max_output_tokens, Some(40_960));
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
    fn capabilities_default_leaves_the_output_cap_unknown() {
        let json = r#"{"models":{"m":{"frame_model":"f","base_url":"http://x","served_model":"s",
          "api_key_env":"K","tokenizer_family":"fam","price_per_tick":1}}}"#;
        let cfg = ModelsConfig::from_json(json).unwrap();
        let m = cfg.get("m").unwrap();
        // an undeclared output cap is UNKNOWN, never "unbounded" -- the seller fails closed.
        assert_eq!(m.capabilities.max_output_tokens, None);
        assert!(
            m.capabilities.sample_algorithm == SampleAlgorithm::None,
            "an omitted sample_algorithm uses the conservative NONE default"
        );
    }

    #[test]
    fn sample_algorithm_enum_is_loaded_and_reaches_the_reference_binding() {
        for (value, expected) in [
            ("SEED", SampleAlgorithm::Seed),
            ("RANDOM_SEED", SampleAlgorithm::RandomSeed),
            ("TOP_K", SampleAlgorithm::TopK),
            ("NONE", SampleAlgorithm::None),
        ] {
            let json = format!(
                r#"{{"models":{{"m":{{"frame_model":"f","base_url":"http://x","served_model":"s",
                  "api_key_env":"K","tokenizer_family":"fam","price_per_tick":1,
                  "capabilities":{{"max_output_tokens":16,"sample_algorithm":"{value}"}}}}}}}}"#,
            );
            let cfg = ModelsConfig::from_json(&json).unwrap();
            let model = cfg.get("m").unwrap();
            assert_eq!(model.capabilities.sample_algorithm, expected);
            assert_eq!(model.reference_endpoint().sample_algorithm, expected);
        }

        let invalid = r#"{"models":{"m":{"frame_model":"f","base_url":"http://x","served_model":"s",
          "api_key_env":"K","tokenizer_family":"fam","price_per_tick":1,
          "capabilities":{"max_output_tokens":16,"sample_algorithm":"BOTH"}}}}"#;
        assert!(
            ModelsConfig::from_json(invalid).is_err(),
            "a profile must select exactly one known sampling algorithm"
        );

        let retired_do_sample = r#"{"models":{"m":{"frame_model":"f","base_url":"http://x","served_model":"s",
          "api_key_env":"K","tokenizer_family":"fam","price_per_tick":1,
          "capabilities":{"max_output_tokens":16,"sample_algorithm":"DO_SAMPLE"}}}}"#;
        assert!(
            ModelsConfig::from_json(retired_do_sample).is_err(),
            "DO_SAMPLE is not a supported B7 reproducibility declaration; GLM profiles use NONE"
        );
    }

    #[test]
    fn the_shipped_models_config_declares_an_output_cap_for_every_model() {
        // a model whose `capabilities.max_output_tokens` is undeclared has an UNKNOWN output cap, and the
        // seller refuses to serve it BEFORE the provider is contacted. A shipped config entry without the cap is
        // therefore a model that cannot deliver a single token -- guard the repo's own deployment artifact so a
        // release can never ship one, the way the live fixtures shipped one.

        // re-pointed this at `models.example.json`, and the subject is unchanged: the shipped
        // artifact. `models.json` stopped being one -- it is our own working config now, and what the
        // user's archive carries is the example. An example teaching a config with no output cap
        // teaches a seller a model that never serves a token, which is exactly what this guards.
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../models.example.json"
        ));
        let cfg = ModelsConfig::load(path).expect("the repo's models.example.json loads");
        for (name, model) in &cfg.models {
            assert!(
                model
                    .capabilities
                    .max_output_tokens
                    .is_some_and(|cap| cap > 0),
                "shipped model \"{name}\" ({}) declares no capabilities.max_output_tokens: the seller fails \
                 closed on an unknown output cap and never serves it",
                model.frame_model
            );
        }
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
    fn executable_reference_requires_url_model_and_key() {
        let cfg = ModelsConfig::from_json(SAMPLE).unwrap();
        let mut model = cfg.get("qwen").unwrap().clone();
        model.api_key_env = "PATH".into();
        model
            .require_executable_reference()
            .expect("valid configured reference");

        model.base_url = "file:///tmp/reference".into();
        assert!(model.require_executable_reference().is_err());
        model.base_url = "https://api.groq.com/openai/v1".into();
        model.served_model.clear();
        assert!(model.require_executable_reference().is_err());
    }

    #[test]
    fn verification_fields_default_when_absent_backward_compatible() {
        // The SAMPLE (and any pre-existing single-model models.json) has no identity_aliases / vocab_size /
        // fingerprints -- it MUST still load, with the new fields defaulting to empty/None (backward-compat).
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
              "price_per_tick": 1,
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
        // accepts_reasoning_side_channel is #[serde(default)] -- a fingerprint may omit it.
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
