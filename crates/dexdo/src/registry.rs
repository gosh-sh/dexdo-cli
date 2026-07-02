//! Client-side ModelRegistry policy for issue.
//! The registry is an on-chain authority. This module keeps the local pieces
//! reusable and testable: strict operator config, read-only registry facts, and
//! role-neutral validation against dexdo's own model hash/book derivation.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[cfg(feature = "shellnet")]
use serde_json::json;

pub const MODEL_REGISTRY_VALIDATION_SCHEMA: &str = "dexdo.model_registry_validation.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryRole {
    Seller,
    Buyer,
}

impl RegistryRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seller => "seller",
            Self::Buyer => "buyer",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryValidationInput {
    pub config_path: Option<PathBuf>,
    pub address_override: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryValidationPolicy {
    pub network: String,
    pub registry_address: Option<String>,
    pub seller_check_model_registry: bool,
    pub seller_deploy_missing_order_book: bool,
    pub buyer_check_model_registry: bool,
    pub source: Option<PathBuf>,
    pub address_overridden: bool,
}

impl RegistryValidationPolicy {
    pub fn load(input: &RegistryValidationInput, contracts: &Path) -> Result<Self> {
        if input.config_path.is_none() && input.address_override.is_none() {
            return Ok(Self::disabled());
        }
        let Some(path) = input.config_path.as_deref() else {
            bail!("--model-registry-address requires --model-registry-validation <config.json>");
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read --model-registry-validation {}", path.display()))?;
        let raw = RawRegistryValidationConfig::from_json(&text)
            .with_context(|| format!("--model-registry-validation {}", path.display()))?;
        let mut address = raw.registry.address;
        let mut overridden = false;
        if let Some(override_addr) = input.address_override.as_deref() {
            address = Some(
                validate_registry_address(override_addr)
                    .with_context(|| format!("--model-registry-address {override_addr}"))?,
            );
            overridden = true;
        }
        if address.is_none() && (raw.seller.check_model_registry || raw.buyer.check_model_registry)
        {
            address = Some(default_registry_address(contracts).with_context(|| {
                format!(
                    "read default ModelRegistry address from {}",
                    contracts.display()
                )
            })?);
        }
        Ok(Self {
            network: raw.registry.network,
            registry_address: address,
            seller_check_model_registry: raw.seller.check_model_registry,
            seller_deploy_missing_order_book: raw.seller.deploy_missing_order_book,
            buyer_check_model_registry: raw.buyer.check_model_registry,
            source: Some(path.to_path_buf()),
            address_overridden: overridden,
        })
    }

    pub fn disabled() -> Self {
        Self {
            network: "shellnet".to_string(),
            registry_address: None,
            seller_check_model_registry: false,
            seller_deploy_missing_order_book: false,
            buyer_check_model_registry: false,
            source: None,
            address_overridden: false,
        }
    }

    pub fn check_enabled(&self, role: RegistryRole) -> bool {
        match role {
            RegistryRole::Seller => self.seller_check_model_registry,
            RegistryRole::Buyer => self.buyer_check_model_registry,
        }
    }

    pub fn required_address(&self, role: RegistryRole) -> Result<&str> {
        self.registry_address.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "{} model registry check enabled but no ModelRegistry address is configured",
                role.as_str()
            )
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistryValidationConfig {
    schema: String,
    registry: RawRegistrySection,
    seller: RawSellerSection,
    buyer: RawBuyerSection,
}

impl RawRegistryValidationConfig {
    fn from_json(text: &str) -> Result<Self> {
        let mut cfg: RawRegistryValidationConfig =
            serde_json::from_str(text).context("parse JSON")?;
        if cfg.schema != MODEL_REGISTRY_VALIDATION_SCHEMA {
            bail!(
                "schema must be `{MODEL_REGISTRY_VALIDATION_SCHEMA}`, got `{}`",
                cfg.schema
            );
        }
        if cfg.registry.network != "shellnet" {
            bail!(
                "registry.network `{}` is unsupported (only `shellnet` is supported)",
                cfg.registry.network
            );
        }
        if let Some(addr) = cfg.registry.address.as_deref() {
            cfg.registry.address = Some(validate_registry_address(addr)?);
        }
        Ok(cfg)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistrySection {
    #[serde(default)]
    address: Option<String>,
    network: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSellerSection {
    check_model_registry: bool,
    deploy_missing_order_book: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuyerSection {
    check_model_registry: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRegistryEntry {
    pub exists: bool,
    pub model_hash: String,
    pub order_book: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRegistryFacts {
    pub frame_model: String,
    pub model_hash: String,
    pub order_book: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModelIdentity {
    pub requested_model: String,
    pub registry_model: String,
    pub model_hash: String,
    pub order_book: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryBookAction {
    UseActive,
    SellerMayDeployMissing,
    BuyerHideMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuyerMissingBookPolicy {
    Reject,
    HideFromAvailableList,
}

#[async_trait]
pub trait ModelRegistryReader: Send + Sync {
    async fn model(&self, frame_model: &str) -> Result<Option<ModelRegistryEntry>>;
}

pub async fn validate_registered_model(
    reader: &(dyn ModelRegistryReader + Send + Sync),
    role: RegistryRole,
    registry_address: &str,
    frame_model: &str,
    expected_order_book: &str,
) -> Result<ModelRegistryFacts> {
    let expected_model_hash = dexdo_core::model_hash_for(frame_model);
    let expected_order_book = validate_registry_address(expected_order_book)
        .with_context(|| format!("expected orderBook for frame_model {frame_model}"))?;
    let Some(entry) = reader.model(frame_model).await? else {
        bail!(
            "{} model registry check failed: frame_model {} is not registered in ModelRegistry {}",
            role.as_str(),
            frame_model,
            registry_address
        );
    };
    if !entry.exists {
        bail!(
            "{} model registry check failed: frame_model {} is not registered in ModelRegistry {}",
            role.as_str(),
            frame_model,
            registry_address
        );
    }
    if normalize_hash(&entry.model_hash) != normalize_hash(&expected_model_hash) {
        bail!(
            "{} model registry check failed: frame_model {} ModelRegistry {} modelHash {} != sha256(frame_model) {}",
            role.as_str(),
            frame_model,
            registry_address,
            entry.model_hash,
            expected_model_hash
        );
    }
    if let Some(registry_order_book) =
        nonzero_registry_order_book(&entry.order_book).with_context(|| {
            format!(
                "{} model registry check failed: frame_model {} ModelRegistry {} returned malformed orderBook",
                role.as_str(),
                frame_model,
                registry_address
            )
        })?
    {
        if registry_order_book != expected_order_book {
            bail!(
                "{} model registry check failed: frame_model {} ModelRegistry {} orderBook {} != dexdo derived orderBook {}",
                role.as_str(),
                frame_model,
                registry_address,
                registry_order_book,
                expected_order_book
            );
        }
    }
    Ok(ModelRegistryFacts {
        frame_model: frame_model.to_string(),
        model_hash: expected_model_hash,
        order_book: expected_order_book,
    })
}

pub async fn resolve_registered_model_identity(
    reader: &(dyn ModelRegistryReader + Send + Sync),
    role: RegistryRole,
    registry_address: &str,
    claimed_model: &str,
) -> Result<ResolvedModelIdentity> {
    let candidates = registry_identity_candidates(claimed_model);
    let mut misses = Vec::new();
    for candidate in &candidates {
        match reader.model(candidate).await? {
            Some(entry) if entry.exists => {
                let expected_model_hash = dexdo_core::model_hash_for(candidate);
                if normalize_hash(&entry.model_hash) != normalize_hash(&expected_model_hash) {
                    bail!(
                        "{} content identity registry check failed: claimed model {} resolved to ModelRegistry {} entry {} but modelHash {} != sha256(entry) {}",
                        role.as_str(),
                        claimed_model,
                        registry_address,
                        candidate,
                        entry.model_hash,
                        expected_model_hash
                    );
                }
                let order_book =
                    nonzero_registry_order_book(&entry.order_book).with_context(|| {
                        format!(
                            "{} content identity registry check failed: ModelRegistry {} entry {} returned malformed orderBook",
                            role.as_str(),
                            registry_address,
                            candidate
                        )
                    })?;
                return Ok(ResolvedModelIdentity {
                    requested_model: claimed_model.to_string(),
                    registry_model: candidate.clone(),
                    model_hash: expected_model_hash,
                    order_book: order_book.unwrap_or_default(),
                });
            }
            _ => misses.push(candidate.clone()),
        }
    }
    bail!(
        "{} content identity registry check failed: claimed model {} does not resolve to a registered ModelRegistry {} identity; tried {:?}",
        role.as_str(),
        claimed_model,
        registry_address,
        misses
    )
}

pub fn registry_identity_candidates(model_id: &str) -> Vec<String> {
    let trimmed = model_id.trim();
    let normalized = trimmed.to_ascii_lowercase();
    let mut out = Vec::new();
    match normalized.as_str() {
        "qwen--qwen3--32b" | "qwen/qwen3-32b" => {
            out.push("Qwen/Qwen3-32B".to_string());
            out.push("qwen--qwen3--32b".to_string());
            out.push("qwen/qwen3-32b".to_string());
        }
        "openai--gpt-oss--20b" | "openai/gpt-oss-20b" => {
            out.push("openai/gpt-oss-20b".to_string());
            out.push("openai--gpt-oss--20b".to_string());
        }
        _ => out.push(trimmed.to_string()),
    }
    out.dedup();
    out
}

pub async fn enforce_model_registry_policy(
    reader: &(dyn ModelRegistryReader + Send + Sync),
    role: RegistryRole,
    policy: &RegistryValidationPolicy,
    frame_model: &str,
    expected_order_book: &str,
    order_book_active: bool,
    buyer_missing_book_policy: BuyerMissingBookPolicy,
) -> Result<RegistryBookAction> {
    let registry_address = policy.required_address(role)?;
    validate_registered_model(
        reader,
        role,
        registry_address,
        frame_model,
        expected_order_book,
    )
    .await?;
    order_book_availability(
        role,
        registry_address,
        frame_model,
        expected_order_book,
        order_book_active,
        policy.seller_deploy_missing_order_book,
        buyer_missing_book_policy,
    )
}

pub fn validate_order_book_availability(
    role: RegistryRole,
    registry_address: &str,
    frame_model: &str,
    order_book: &str,
    active: bool,
    seller_deploy_missing_order_book: bool,
) -> Result<RegistryBookAction> {
    order_book_availability(
        role,
        registry_address,
        frame_model,
        order_book,
        active,
        seller_deploy_missing_order_book,
        BuyerMissingBookPolicy::Reject,
    )
}

pub fn order_book_availability(
    role: RegistryRole,
    registry_address: &str,
    frame_model: &str,
    order_book: &str,
    active: bool,
    seller_deploy_missing_order_book: bool,
    buyer_missing_book_policy: BuyerMissingBookPolicy,
) -> Result<RegistryBookAction> {
    if active {
        return Ok(RegistryBookAction::UseActive);
    }
    match role {
        RegistryRole::Seller if seller_deploy_missing_order_book => {
            Ok(RegistryBookAction::SellerMayDeployMissing)
        }
        RegistryRole::Seller => bail!(
            "seller model registry check failed: frame_model {frame_model} canonical order book {order_book} from ModelRegistry {registry_address} is not deployed and seller.deploy_missing_order_book=false"
        ),
        RegistryRole::Buyer
            if buyer_missing_book_policy == BuyerMissingBookPolicy::HideFromAvailableList =>
        {
            Ok(RegistryBookAction::BuyerHideMissing)
        }
        RegistryRole::Buyer => bail!(
            "buyer model registry check failed: frame_model {frame_model} canonical order book {order_book} from ModelRegistry {registry_address} is not deployed; not available to buy now"
        ),
    }
}

#[derive(Clone, Debug)]
pub struct UnavailableModelRegistryReader {
    reason: String,
}

impl UnavailableModelRegistryReader {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl ModelRegistryReader for UnavailableModelRegistryReader {
    async fn model(&self, _frame_model: &str) -> Result<Option<ModelRegistryEntry>> {
        bail!("{}", self.reason)
    }
}

#[cfg(feature = "shellnet")]
pub struct ShellnetModelRegistryReader {
    chain: dexdo_core::RealChainBackend,
    registry_address: dexdo_core::Address,
    abi_json: String,
}

#[cfg(feature = "shellnet")]
impl ShellnetModelRegistryReader {
    pub fn from_manifest(
        contracts: &Path,
        registry_address: &str,
        abi_path: &Path,
    ) -> Result<Self> {
        let abi_json = std::fs::read_to_string(abi_path)
            .with_context(|| format!("read ModelRegistry ABI {}", abi_path.display()))?;
        validate_model_registry_abi_getters(&abi_json)
            .with_context(|| format!("ModelRegistry ABI {}", abi_path.display()))?;
        let registry_address = dexdo_core::Address::parse(registry_address)
            .map_err(|e| anyhow::anyhow!("ModelRegistry address {registry_address}: {e}"))?;
        let chain = dexdo_core::RealChainBackend::connect(contracts)
            .with_context(|| format!("connect shellnet using {}", contracts.display()))?;
        Ok(Self {
            chain,
            registry_address,
            abi_json,
        })
    }

    async fn getter(&self, method: &str, frame_model: &str) -> Result<Value> {
        self.chain
            .client()
            .run_getter(
                &self.registry_address,
                &self.abi_json,
                method,
                json!({ "canonicalName": frame_model }),
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "ModelRegistry {} is not active; cannot read {method}({frame_model})",
                    self.registry_address.with_workchain()
                )
            })
    }
}

#[cfg(feature = "shellnet")]
#[async_trait]
impl ModelRegistryReader for ShellnetModelRegistryReader {
    async fn model(&self, frame_model: &str) -> Result<Option<ModelRegistryEntry>> {
        let has = self
            .getter("has", frame_model)
            .await
            .with_context(|| format!("ModelRegistry has({frame_model})"))?;
        if !getter_bool(&has, &["value0"]).context("ModelRegistry.has returned no bool")? {
            return Ok(None);
        }

        let model_hash_of = self
            .getter("modelHashOf", frame_model)
            .await
            .with_context(|| format!("ModelRegistry modelHashOf({frame_model})"))?;
        let model_hash = getter_hash(&model_hash_of, &["value0"])
            .context("modelHashOf returned no modelHash")?;

        let order_book_of = self
            .getter("orderBookOf", frame_model)
            .await
            .with_context(|| format!("ModelRegistry orderBookOf({frame_model})"))?;
        let raw_order_book = getter_address(&order_book_of, &["value0"])
            .context("orderBookOf returned no address")?;
        let order_book = nonzero_registry_order_book(&raw_order_book)
            .context("orderBookOf returned malformed address")?
            .ok_or_else(|| anyhow::anyhow!("orderBookOf returned zero address"))?;

        Ok(Some(ModelRegistryEntry {
            exists: true,
            model_hash,
            order_book,
        }))
    }
}

pub fn validate_model_registry_abi_getters(abi_json: &str) -> Result<()> {
    let abi: Value = serde_json::from_str(abi_json).context("parse JSON")?;
    let functions = abi
        .get("functions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("ABI has no functions array"))?;
    for required in [
        "has",
        "modelHashOf",
        "orderBookOf",
        "count",
        "inferenceOrderBookCode",
    ] {
        let found = functions
            .iter()
            .any(|f| f.get("name").and_then(|v| v.as_str()) == Some(required));
        if !found {
            bail!("ABI is missing required getter `{required}`");
        }
    }
    Ok(())
}

fn validate_registry_address(address: &str) -> Result<String> {
    dexdo_core::normalize_wallet_address(address)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("malformed ModelRegistry address `{address}`"))
}

fn nonzero_registry_order_book(raw: &str) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || is_zero_address_like(trimmed) {
        return Ok(None);
    }
    validate_registry_address(trimmed).map(Some)
}

fn is_zero_address_like(raw: &str) -> bool {
    let bare = raw
        .trim()
        .strip_prefix("0:")
        .or_else(|| raw.trim().strip_prefix("0x"))
        .or_else(|| raw.trim().strip_prefix("0X"))
        .unwrap_or(raw.trim());
    !bare.is_empty() && bare.bytes().all(|b| b == b'0')
}

fn normalize_hash(hash: &str) -> String {
    hash.trim()
        .strip_prefix("0x")
        .or_else(|| hash.trim().strip_prefix("0X"))
        .unwrap_or(hash.trim())
        .to_ascii_lowercase()
}

fn default_registry_address(contracts: &Path) -> Result<String> {
    let text = std::fs::read_to_string(contracts)?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let raw = json
        .get("model_registry")
        .or_else(|| json.get("modelRegistry"))
        .or_else(|| json.get("ModelRegistry"))
        .and_then(address_from_value)
        .or_else(|| {
            json.get("registry")
                .and_then(|v| v.get("model_registry").or_else(|| v.get("modelRegistry")))
                .and_then(address_from_value)
        })
        .ok_or_else(|| {
            anyhow::anyhow!("contracts manifest has no `model_registry` / `modelRegistry` address")
        })?;
    validate_registry_address(raw)
}

pub fn default_model_registry_address(contracts: &Path) -> Result<String> {
    default_registry_address(contracts)
}

fn address_from_value(value: &serde_json::Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("address").and_then(|v| v.as_str()))
}

#[cfg(feature = "shellnet")]
fn getter_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| {
        let v = value.get(*key)?;
        v.as_bool().or_else(|| match v.as_str()?.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    })
}

#[cfg(feature = "shellnet")]
fn getter_hash(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let v = value.get(*key)?;
        if let Some(s) = v.as_str() {
            let s = s.trim();
            if s.is_empty() {
                None
            } else if s.starts_with("0x") || s.starts_with("0X") {
                Some(format!("0x{}", normalize_hash(s)))
            } else if s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                Some(format!("0x{}", s.to_ascii_lowercase()))
            } else {
                None
            }
        } else {
            v.as_u64().map(|n| format!("0x{n:064x}"))
        }
    })
}

#[cfg(feature = "shellnet")]
fn getter_address(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let s = value.get(*key)?.as_str()?.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    const ADDR1: &str = "0:1111111111111111111111111111111111111111111111111111111111111111";
    const ADDR2: &str = "0:2222222222222222222222222222222222222222222222222222222222222222";
    const REG: &str = "0:9999999999999999999999999999999999999999999999999999999999999999";
    const ZERO_ADDR: &str = "0:0000000000000000000000000000000000000000000000000000000000000000";

    fn config(seller: bool, buyer: bool) -> String {
        format!(
            r#"{{
              "schema": "{MODEL_REGISTRY_VALIDATION_SCHEMA}",
              "registry": {{ "address": "{REG}", "network": "shellnet" }},
              "seller": {{ "check_model_registry": {seller}, "deploy_missing_order_book": false }},
              "buyer": {{ "check_model_registry": {buyer} }}
            }}"#
        )
    }

    #[test]
    fn parser_accepts_explicit_seller_buyer_booleans() {
        let dir = temp_dir("registry-config-ok");
        let contracts = write_contracts(&dir, ADDR1);
        let cfg_path = dir.join("registry.json");
        std::fs::write(&cfg_path, config(true, false)).unwrap();
        let policy = RegistryValidationPolicy::load(
            &RegistryValidationInput {
                config_path: Some(cfg_path),
                address_override: None,
            },
            &contracts,
        )
        .unwrap();
        assert!(policy.check_enabled(RegistryRole::Seller));
        assert!(!policy.check_enabled(RegistryRole::Buyer));
        assert!(!policy.seller_deploy_missing_order_book);
        assert_eq!(policy.required_address(RegistryRole::Seller).unwrap(), REG);
    }

    #[test]
    fn parser_accepts_seller_deploy_missing_book_independently() {
        let dir = temp_dir("registry-config-deploy-missing");
        let contracts = write_contracts(&dir, ADDR1);
        let cfg_path = dir.join("registry.json");
        std::fs::write(
            &cfg_path,
            config(false, true).replace(
                r#""deploy_missing_order_book": false"#,
                r#""deploy_missing_order_book": true"#,
            ),
        )
        .unwrap();
        let policy = RegistryValidationPolicy::load(
            &RegistryValidationInput {
                config_path: Some(cfg_path),
                address_override: None,
            },
            &contracts,
        )
        .unwrap();
        assert!(!policy.check_enabled(RegistryRole::Seller));
        assert!(policy.check_enabled(RegistryRole::Buyer));
        assert!(policy.seller_deploy_missing_order_book);
    }

    #[test]
    fn parser_rejects_malformed_config() {
        let bad_configs = vec![
            r#"{"registry":{"address":"0:aaaa","network":"shellnet"},"seller":{"check_model_registry":true},"buyer":{"check_model_registry":true}}"#.to_string(),
            config(true, true).replace(r#""buyer": {"#, r#""extra": 1, "buyer": {"#),
            config(true, true).replace(REG, "0:dead"),
            config(true, true)
                .replace(r#""check_model_registry": true"#, r#""check_model_registry": "yes""#),
            config(true, true).replace(r#""network": "shellnet""#, r#""network": "mainnet""#),
        ];
        for bad in bad_configs {
            assert!(
                RawRegistryValidationConfig::from_json(&bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn default_address_reads_contracts_manifest_when_enabled() {
        let dir = temp_dir("registry-config-default");
        let contracts = write_contracts(&dir, ADDR2);
        let cfg_path = dir.join("registry.json");
        std::fs::write(
            &cfg_path,
            format!(
                r#"{{
                  "schema": "{MODEL_REGISTRY_VALIDATION_SCHEMA}",
                  "registry": {{ "network": "shellnet" }},
                  "seller": {{ "check_model_registry": true, "deploy_missing_order_book": false }},
                  "buyer": {{ "check_model_registry": false }}
                }}"#
            ),
        )
        .unwrap();
        let policy = RegistryValidationPolicy::load(
            &RegistryValidationInput {
                config_path: Some(cfg_path),
                address_override: None,
            },
            &contracts,
        )
        .unwrap();
        assert_eq!(
            policy.required_address(RegistryRole::Seller).unwrap(),
            ADDR2
        );
    }

    #[test]
    fn default_address_reads_committed_shellnet_manifest() {
        let contracts = repo_path("contracts/deployed.shellnet.json");
        let address = default_registry_address(&contracts).unwrap();
        assert_eq!(
            address,
            "0:19a88a103f949df6ce5532a91e935a04acd708872d4bd4e2e5e446d0d78140b9"
        );
    }

    #[derive(Default)]
    struct FakeReader {
        entries: Mutex<BTreeMap<String, Option<ModelRegistryEntry>>>,
    }

    #[async_trait]
    impl ModelRegistryReader for FakeReader {
        async fn model(&self, frame_model: &str) -> Result<Option<ModelRegistryEntry>> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .get(frame_model)
                .cloned()
                .unwrap_or(None))
        }
    }

    impl FakeReader {
        fn with(self, frame_model: &str, entry: Option<ModelRegistryEntry>) -> Self {
            self.entries
                .lock()
                .unwrap()
                .insert(frame_model.to_string(), entry);
            self
        }
    }

    fn registered_entry(frame_model: &str, order_book: &str) -> ModelRegistryEntry {
        ModelRegistryEntry {
            exists: true,
            model_hash: dexdo_core::model_hash_for(frame_model),
            order_book: order_book.to_string(),
        }
    }

    fn policy(seller_deploy_missing_order_book: bool) -> RegistryValidationPolicy {
        RegistryValidationPolicy {
            network: "shellnet".to_string(),
            registry_address: Some(REG.to_string()),
            seller_check_model_registry: true,
            seller_deploy_missing_order_book,
            buyer_check_model_registry: true,
            source: None,
            address_overridden: false,
        }
    }

    #[test]
    fn model_registry_abi_getter_shape_matches_4_0_18() {
        let abi_path = repo_path("contracts/compiled_0.79.3/airegistry/ModelRegistry.abi.json");
        let abi = std::fs::read_to_string(abi_path).unwrap();
        validate_model_registry_abi_getters(&abi).unwrap();

        let parsed: Value = serde_json::from_str(&abi).unwrap();
        let functions = parsed["functions"].as_array().unwrap();
        for removed in ["getModel", "getAll"] {
            assert!(
                !functions
                    .iter()
                    .any(|f| f.get("name").and_then(|v| v.as_str()) == Some(removed)),
                "4.0.18 ModelRegistry ABI must not expose removed getter {removed}"
            );
        }

        let missing = r#"{"functions":[{"name":"has"},{"name":"orderBookOf"},{"name":"count"},{"name":"inferenceOrderBookCode"}]}"#;
        let err = validate_model_registry_abi_getters(missing)
            .unwrap_err()
            .to_string();
        assert!(err.contains("modelHashOf"), "{err}");
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    #[ignore = "read-only live shellnet evidence; requires current deployed.shellnet.json endpoint"]
    async fn live_shellnet_model_registry_reader_reads_seeded_model() {
        let contracts = repo_path("contracts/deployed.shellnet.json");
        let abi_path = repo_path("contracts/compiled_0.79.3/airegistry/ModelRegistry.abi.json");
        let abi = std::fs::read_to_string(&abi_path).unwrap();
        let registry = default_registry_address(&contracts).unwrap();
        let registry_addr = dexdo_core::Address::parse(&registry).unwrap();
        let chain = dexdo_core::RealChainBackend::connect(&contracts).unwrap();
        let count = chain
            .client()
            .run_getter(&registry_addr, &abi, "count", json!({}))
            .await
            .expect("read live ModelRegistry count")
            .expect("ModelRegistry account active for count");
        let count_n = count
            .get("n")
            .or_else(|| count.get("value0"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            .expect("ModelRegistry count returned no n/value0");
        assert!(count_n > 0, "live ModelRegistry count must be nonzero");
        let code = chain
            .client()
            .run_getter(&registry_addr, &abi, "inferenceOrderBookCode", json!({}))
            .await
            .expect("read live ModelRegistry inferenceOrderBookCode")
            .expect("ModelRegistry account active for inferenceOrderBookCode");
        println!(
            "live ModelRegistry snapshot registry={} count={} inferenceOrderBookCode={}",
            registry, count_n, code
        );
        // These are live ModelRegistry seed names. They may differ from indexer
        // display refs such as normalized producer--model--version strings.
        let frame_models = ["Qwen/Qwen3-32B", "openai/gpt-oss-20b"];

        let reader = ShellnetModelRegistryReader::from_manifest(&contracts, &registry, &abi_path)
            .expect("shellnet ModelRegistry reader");
        let mut found = None;
        for frame_model in frame_models {
            if let Some(entry) = reader
                .model(&frame_model)
                .await
                .unwrap_or_else(|e| panic!("read live ModelRegistry {frame_model}: {e}"))
            {
                found = Some((frame_model.to_string(), entry));
                break;
            }
        }
        let (frame_model, entry) =
            found.unwrap_or_else(|| panic!("no live seeded ModelRegistry entry found"));
        assert!(entry.exists);
        assert_eq!(
            normalize_hash(&entry.model_hash),
            normalize_hash(&dexdo_core::model_hash_for(&frame_model))
        );
        let order_book = nonzero_registry_order_book(&entry.order_book)
            .unwrap()
            .expect("seeded model exposes a derived orderBook");
        println!(
            "live ModelRegistry evidence registry={} frame_model={} model_hash={} order_book={}",
            registry, frame_model, entry.model_hash, order_book
        );
    }

    #[tokio::test]
    async fn validator_accepts_registered_matching_model() {
        let frame = "qwen--qwen3--32b";
        let reader = FakeReader::default().with(
            frame,
            Some(ModelRegistryEntry {
                exists: true,
                model_hash: dexdo_core::model_hash_for(frame),
                order_book: ADDR1.to_string(),
            }),
        );
        let facts = validate_registered_model(&reader, RegistryRole::Buyer, REG, frame, ADDR1)
            .await
            .unwrap();
        assert_eq!(facts.model_hash, dexdo_core::model_hash_for(frame));
        assert_eq!(facts.order_book, ADDR1);
    }

    #[tokio::test]
    async fn content_identity_resolves_qwen_frame_to_live_registry_name() {
        let reader = FakeReader::default().with(
            "Qwen/Qwen3-32B",
            Some(registered_entry("Qwen/Qwen3-32B", ADDR1)),
        );
        let identity = resolve_registered_model_identity(
            &reader,
            RegistryRole::Buyer,
            REG,
            "qwen--qwen3--32b",
        )
        .await
        .unwrap();
        assert_eq!(identity.requested_model, "qwen--qwen3--32b");
        assert_eq!(identity.registry_model, "Qwen/Qwen3-32B");
        assert_eq!(
            identity.model_hash,
            dexdo_core::model_hash_for("Qwen/Qwen3-32B")
        );
    }

    #[tokio::test]
    async fn content_identity_rejects_unregistered_qwen_variant_without_family_fallback() {
        let reader = FakeReader::default().with(
            "Qwen/Qwen3-32B",
            Some(registered_entry("Qwen/Qwen3-32B", ADDR1)),
        );
        let err = resolve_registered_model_identity(
            &reader,
            RegistryRole::Buyer,
            REG,
            "qwen--qwen3.6--27b",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not resolve"), "{err}");
        assert!(
            err.contains("qwen--qwen3.6--27b"),
            "specific variant is reported: {err}"
        );
    }

    #[tokio::test]
    async fn content_identity_resolves_openai_gpt_oss_seed_exactly() {
        let reader = FakeReader::default().with(
            "openai/gpt-oss-20b",
            Some(registered_entry("openai/gpt-oss-20b", ADDR1)),
        );
        let identity = resolve_registered_model_identity(
            &reader,
            RegistryRole::Buyer,
            REG,
            "openai--gpt-oss--20b",
        )
        .await
        .unwrap();
        assert_eq!(identity.registry_model, "openai/gpt-oss-20b");
    }

    #[tokio::test]
    async fn validator_accepts_registered_name_hash_without_deployed_book_metadata() {
        let frame = "qwen--qwen3--32b";
        for registry_order_book in ["", ZERO_ADDR] {
            let reader = FakeReader::default()
                .with(frame, Some(registered_entry(frame, registry_order_book)));
            let facts = validate_registered_model(&reader, RegistryRole::Seller, REG, frame, ADDR1)
                .await
                .unwrap();
            assert_eq!(facts.model_hash, dexdo_core::model_hash_for(frame));
            assert_eq!(facts.order_book, ADDR1);
        }
    }

    #[tokio::test]
    async fn enforce_registry_policy_accepts_registered_active_book() {
        let frame = "qwen--qwen3--32b";
        let reader = FakeReader::default().with(
            frame,
            Some(ModelRegistryEntry {
                exists: true,
                model_hash: dexdo_core::model_hash_for(frame),
                order_book: ADDR1.to_string(),
            }),
        );
        let action = enforce_model_registry_policy(
            &reader,
            RegistryRole::Buyer,
            &policy(false),
            frame,
            ADDR1,
            true,
            BuyerMissingBookPolicy::Reject,
        )
        .await
        .unwrap();
        assert_eq!(action, RegistryBookAction::UseActive);
    }

    #[tokio::test]
    async fn seller_registered_missing_book_metadata_deploy_true_may_deploy_canonical_book() {
        let frame = "qwen--qwen3--32b";
        let reader = FakeReader::default().with(frame, Some(registered_entry(frame, "")));
        let action = enforce_model_registry_policy(
            &reader,
            RegistryRole::Seller,
            &policy(true),
            frame,
            ADDR1,
            false,
            BuyerMissingBookPolicy::Reject,
        )
        .await
        .unwrap();
        assert_eq!(action, RegistryBookAction::SellerMayDeployMissing);
    }

    #[tokio::test]
    async fn seller_registered_missing_book_metadata_deploy_false_fails_closed() {
        let frame = "qwen--qwen3--32b";
        let reader = FakeReader::default().with(frame, Some(registered_entry(frame, "")));
        let err = enforce_model_registry_policy(
            &reader,
            RegistryRole::Seller,
            &policy(false),
            frame,
            ADDR1,
            false,
            BuyerMissingBookPolicy::Reject,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("deploy_missing_order_book=false"), "{err}");
    }

    #[tokio::test]
    async fn buyer_registered_missing_book_metadata_hides_and_rejects_before_escrow() {
        let frame = "qwen--qwen3--32b";
        let hidden_reader = FakeReader::default().with(frame, Some(registered_entry(frame, "")));
        let action = enforce_model_registry_policy(
            &hidden_reader,
            RegistryRole::Buyer,
            &policy(false),
            frame,
            ADDR1,
            false,
            BuyerMissingBookPolicy::HideFromAvailableList,
        )
        .await
        .unwrap();
        assert_eq!(action, RegistryBookAction::BuyerHideMissing);

        let rejected_reader =
            FakeReader::default().with(frame, Some(registered_entry(frame, ZERO_ADDR)));
        let err = enforce_model_registry_policy(
            &rejected_reader,
            RegistryRole::Buyer,
            &policy(false),
            frame,
            ADDR1,
            false,
            BuyerMissingBookPolicy::Reject,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("not available to buy now"), "{err}");
    }

    #[tokio::test]
    async fn enforce_registry_policy_rejects_bad_registry_facts_before_money_move() {
        let frame = "qwen--qwen3--32b";
        let unregistered = FakeReader::default();
        let err = enforce_model_registry_policy(
            &unregistered,
            RegistryRole::Seller,
            &policy(true),
            frame,
            ADDR1,
            true,
            BuyerMissingBookPolicy::Reject,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("not registered"), "{err}");

        let bad_hash = FakeReader::default().with(
            frame,
            Some(ModelRegistryEntry {
                exists: true,
                model_hash: dexdo_core::model_hash_for("qwen--wrong--v1"),
                order_book: ADDR1.to_string(),
            }),
        );
        let err = enforce_model_registry_policy(
            &bad_hash,
            RegistryRole::Buyer,
            &policy(false),
            frame,
            ADDR1,
            true,
            BuyerMissingBookPolicy::Reject,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("modelHash"), "{err}");

        let bad_book = FakeReader::default().with(
            frame,
            Some(ModelRegistryEntry {
                exists: true,
                model_hash: dexdo_core::model_hash_for(frame),
                order_book: ADDR2.to_string(),
            }),
        );
        let err = enforce_model_registry_policy(
            &bad_book,
            RegistryRole::Buyer,
            &policy(false),
            frame,
            ADDR1,
            true,
            BuyerMissingBookPolicy::Reject,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("orderBook"), "{err}");
    }

    #[tokio::test]
    async fn validator_rejects_unregistered_before_money_move() {
        let err = validate_registered_model(
            &FakeReader::default(),
            RegistryRole::Seller,
            REG,
            "qwen--typo--v1",
            ADDR1,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("seller model registry check failed"), "{err}");
        assert!(err.contains("not registered"), "{err}");
    }

    #[tokio::test]
    async fn validator_rejects_exists_false_hash_and_book_mismatch() {
        let frame = "qwen--qwen3--32b";
        let exists_false = FakeReader::default().with(
            frame,
            Some(ModelRegistryEntry {
                exists: false,
                model_hash: dexdo_core::model_hash_for(frame),
                order_book: ADDR1.to_string(),
            }),
        );
        assert!(
            validate_registered_model(&exists_false, RegistryRole::Buyer, REG, frame, ADDR1)
                .await
                .unwrap_err()
                .to_string()
                .contains("not registered")
        );

        let bad_hash = FakeReader::default().with(
            frame,
            Some(ModelRegistryEntry {
                exists: true,
                model_hash: dexdo_core::model_hash_for("llama--llama3--8b"),
                order_book: ADDR1.to_string(),
            }),
        );
        assert!(
            validate_registered_model(&bad_hash, RegistryRole::Buyer, REG, frame, ADDR1)
                .await
                .unwrap_err()
                .to_string()
                .contains("modelHash")
        );

        let bad_book = FakeReader::default().with(
            frame,
            Some(ModelRegistryEntry {
                exists: true,
                model_hash: dexdo_core::model_hash_for(frame),
                order_book: ADDR2.to_string(),
            }),
        );
        assert!(
            validate_registered_model(&bad_book, RegistryRole::Buyer, REG, frame, ADDR1)
                .await
                .unwrap_err()
                .to_string()
                .contains("orderBook")
        );
    }

    #[test]
    fn active_canonical_book_is_available_to_seller_and_buyer() {
        assert_eq!(
            validate_order_book_availability(
                RegistryRole::Seller,
                REG,
                "qwen--qwen3--32b",
                ADDR1,
                true,
                false
            )
            .unwrap(),
            RegistryBookAction::UseActive
        );
        assert_eq!(
            validate_order_book_availability(
                RegistryRole::Buyer,
                REG,
                "qwen--qwen3--32b",
                ADDR1,
                true,
                false
            )
            .unwrap(),
            RegistryBookAction::UseActive
        );
    }

    #[test]
    fn seller_missing_book_with_deploy_true_may_deploy_canonical_book() {
        assert_eq!(
            validate_order_book_availability(
                RegistryRole::Seller,
                REG,
                "qwen--qwen3--32b",
                ADDR1,
                false,
                true
            )
            .unwrap(),
            RegistryBookAction::SellerMayDeployMissing
        );
    }

    #[test]
    fn seller_missing_book_with_deploy_false_fails_closed() {
        let err = validate_order_book_availability(
            RegistryRole::Seller,
            REG,
            "qwen--qwen3--32b",
            ADDR1,
            false,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("deploy_missing_order_book=false"), "{err}");
    }

    #[test]
    fn buyer_missing_book_rejects_for_verified_operations() {
        let err = validate_order_book_availability(
            RegistryRole::Buyer,
            REG,
            "qwen--qwen3--32b",
            ADDR1,
            false,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not available to buy now"), "{err}");
    }

    #[test]
    fn buyer_missing_book_hides_from_available_market_list() {
        assert_eq!(
            order_book_availability(
                RegistryRole::Buyer,
                REG,
                "qwen--qwen3--32b",
                ADDR1,
                false,
                true,
                BuyerMissingBookPolicy::HideFromAvailableList,
            )
            .unwrap(),
            RegistryBookAction::BuyerHideMissing
        );
    }

    #[tokio::test]
    async fn disabled_checks_preserve_old_behavior() {
        let policy = RegistryValidationPolicy::disabled();
        assert!(!policy.check_enabled(RegistryRole::Seller));
        assert!(!policy.check_enabled(RegistryRole::Buyer));
    }

    fn write_contracts(dir: &Path, address: &str) -> PathBuf {
        let path = dir.join("deployed.shellnet.json");
        std::fs::write(
            &path,
            format!(r#"{{"network":"shellnet","model_registry":"{address}"}}"#),
        )
        .unwrap();
        path
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dexdo-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    fn repo_path(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative)
    }
}
