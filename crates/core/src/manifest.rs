//! Market-provisioning output manifest: the addresses a `dexdo provision` run produces.
//! One JSON file per per-deal market (`InferenceOrderBook` + `RootModel` + the deployed `TokenContract`).

//! Pure data (no chain, no feature gate); this is the output/parsing contract, covered by a
//! deterministic offline guard.

//! **Note-funded:** `dexdo provision` brings up the OB + per-deal `TokenContract` from the
//! seller note's own ECC[2] -- no operator multisig. The note pre-funds the TC's uninit deploy address via
//! `PrivateNote.fundDeployShell` and the external seller-signed deploy activates it, so `token_contract` here
//! is the **deployed, active** per-deal TC (not a derived placeholder). The `RootModel` is NOT note-funded:
//! since 4.0.34 `SuperRoot.deployRootModel` deploys it with its own value, and an external deploy of it is
//! refused outright. Giver is the one-time mint faucet only; zero giver in the operate path.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `sha256(frame_model)` as `0x<hex>` -- the canonical on-chain modelHash: the seller
/// (`--model`->`frame_model`) and the buyer (`--frame-model`) derive it from the SAME `frame_model` to
/// converge on a single order-book address. Also used to validate a manifest's `model_hash`.
pub fn model_hash_for(frame_model: &str) -> String {
    let digest = Sha256::digest(frame_model.as_bytes());
    let mut s = String::with_capacity(2 + digest.len() * 2);
    s.push_str("0x");
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Precision is a seller attestation carried by the model id; delivery does not enforce it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttestedModelPrecision {
    pub value: String,
    pub status: &'static str,
    pub enforced: bool,
}

/// Optional canonical-id flag slots. `None`/`false` means the grammar's default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CanonicalModelFlags {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub tools: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub think: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precision: Option<AttestedModelPrecision>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl CanonicalModelFlags {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Compact human rendering used beside every displayed book, offer, and quote.
    pub fn render_human(&self) -> String {
        let mut flags = Vec::new();
        if let Some(unit) = &self.unit {
            flags.push(format!("unit={unit}"));
        }
        if let Some(window) = &self.window {
            flags.push(format!("window={window}"));
        }
        if let Some(resolution) = &self.resolution {
            flags.push(format!("resolution={resolution}"));
        }
        if self.tools {
            flags.push("tools".to_string());
        }
        if self.think {
            flags.push("think".to_string());
        }
        if let Some(precision) = &self.precision {
            flags.push(format!(
                "precision={}(attested,not-enforced)",
                precision.value
            ));
        }
        flags.join(",")
    }
}

/// Parsed canonical model id. The raw id remains the only input to [`model_hash_for`]; this value is
/// descriptive and is never used to rebuild, reorder, or otherwise normalize the hash preimage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalModelId {
    pub producer: String,
    pub model: String,
    pub version: String,
    pub flags: CanonicalModelFlags,
}

fn parse_model_flag(token: &str) -> Result<(usize, &'static str), String> {
    let slot = match token {
        "sec" | "min" | "img" | "frame" => (0, "unit"),
        "480p" | "720p" | "1080p" | "2160p" => (2, "resolution"),
        "tools" => (3, "tools"),
        "think" => (4, "think"),
        "fp32" | "bf16" | "fp16" | "fp8" | "int8" | "int4" => (5, "precision"),
        "gptq" | "awq" | "gguf" | "Q4_K_M" => {
            return Err(format!(
                "packing format `{token}` is not a canonical model-id flag; packing spellings are deliberately not admitted"
            ));
        }
        _ if token.starts_with('w') => {
            let digits = token
                .strip_prefix('w')
                .and_then(|value| value.strip_suffix('k'))
                .unwrap_or_default();
            let thousands = digits.parse::<u64>().ok();
            if digits.is_empty()
                || !digits.bytes().all(|byte| byte.is_ascii_digit())
                || thousands == Some(0)
                || thousands.is_none()
                || thousands.is_some_and(|value| value.to_string() != digits)
            {
                return Err(format!(
                    "malformed window flag `{token}`; expected `w<N>k` with a positive canonical integer N, e.g. `w8k`"
                ));
            }
            (1, "window")
        }
        _ => return Err(format!("unknown flag token `{token}` in model id")),
    };
    Ok(slot)
}

/// Parse and validate the injective canonical-id grammar:
/// `producer--model--version[--unit][--w<N>k][--<N>p][--tools][--think][--<precision>]`.

/// Flag order is fixed, each slot may occur at most once, and the parser rejects rather than
/// normalizing. In particular, callers must keep hashing the original raw bytes with
/// [`model_hash_for`], never reconstruct an id from this parsed value.
pub fn parse_canonical_model_id(name: &str) -> Result<CanonicalModelId, String> {
    const CONTRACT_MODEL_NAME_MAX_BYTES: usize = 127;

    let byte_len = name.len();
    if byte_len > CONTRACT_MODEL_NAME_MAX_BYTES {
        return Err(format!(
            "model id is {byte_len} bytes; the contract model-name limit is {CONTRACT_MODEL_NAME_MAX_BYTES} bytes"
        ));
    }

    let mut parts = name.split("--");
    let (Some(producer), Some(model), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!(
            "model id `{name}` is not canonical `producer--model--version` (three non-empty base parts are required)"
        ));
    };
    if producer.trim().is_empty() || model.trim().is_empty() || version.trim().is_empty() {
        return Err(format!(
            "model id `{name}` is not canonical `producer--model--version` (three non-empty base parts are required)"
        ));
    }

    let mut flags = CanonicalModelFlags::default();
    let mut seen = [false; 6];
    let mut last_slot = None;
    let mut last_name = None;
    for token in parts {
        let (slot, slot_name) = parse_model_flag(token)?;
        if seen[slot] {
            return Err(format!("duplicate flag `{token}` in model id `{name}`"));
        }
        if last_slot.is_some_and(|last| slot < last) {
            return Err(format!(
                "flag `{token}` is in the wrong order in model id `{name}`; `{slot_name}` must precede `{}`",
                last_name.unwrap_or("the previous flag")
            ));
        }
        seen[slot] = true;
        last_slot = Some(slot);
        last_name = Some(slot_name);
        match slot {
            0 => flags.unit = Some(token.to_string()),
            1 => flags.window = Some(token.to_string()),
            2 => flags.resolution = Some(token.to_string()),
            3 => flags.tools = true,
            4 => flags.think = true,
            5 => {
                flags.precision = Some(AttestedModelPrecision {
                    value: token.to_string(),
                    status: "attested",
                    enforced: false,
                });
            }
            _ => unreachable!("model flag slot is closed"),
        }
    }

    Ok(CanonicalModelId {
        producer: producer.to_string(),
        model: model.to_string(),
        version: version.to_string(),
        flags,
    })
}

/// Validate a canonical model id while preserving the original validator's unit return type.
pub fn validate_canonical_model_id(name: &str) -> Result<(), String> {
    parse_canonical_model_id(name).map(|_| ())
}

/// Normalize a model hash for comparison: drop an optional `0x` prefix and lowercase. The on-chain
/// `getModelHash` getter and [`model_hash_for`] may differ only by the prefix/case, so both sides are
/// normalized before matching.
fn normalize_hash(h: &str) -> String {
    let t = h.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    t.to_ascii_lowercase()
}

/// Resolve a `modelHash` back to a configured model name -- the inverse of [`model_hash_for`].
/// This is an **integrity/fallback** helper: the 4.0.6 `TokenContract` exposes `getModelName()` directly (the
/// authoritative display name), so the real reader uses that name and calls this to **cross-check** it against
/// the operator's configured set (or to recover the name when only the hash -- `getModelHash` -- is on hand). It
/// matches by hashing each configured name (normalized for the optional `0x`/case). Returns the first match, or
/// `None` if the hash is absent or unknown to the configured set -- then the caller shows the raw hash rather
/// than guessing. The configured set is the operator's `models.json` / market manifest(s), never an unbounded
/// preimage search.
pub fn resolve_model_name(model_hash: Option<&str>, known_models: &[String]) -> Option<String> {
    let want = normalize_hash(model_hash?);
    known_models
        .iter()
        .find(|name| normalize_hash(&model_hash_for(name)) == want)
        .cloned()
}

/// A provisioned per-deal market. `token_contract` is what `dexdo seller`/`buyer`
/// take as `--token-contract`; the rest is the surrounding market identity (for transparency and
/// `dexdo monitor`). No secrets -- public/derivable only.

/// **Addresses:** written as canonical `<dapp_id>::<account_id>`, read in either that or the
/// legacy `0:<account_id>` form, and held in memory in the workchain form the chain client takes. A
/// manifest written by an older version keeps loading unchanged; one written now carries the DApp
/// identity. Root/market/note accounts use the shared dexdo DApp; each TokenContract uses its own
/// account id as its DApp id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketManifest {
    /// Network the market is deployed on (e.g. the chain build).
    pub network: String,
    /// Configured frame model id (the model the seller serves; the buyer's `--frame-model`).
    pub frame_model: String,
    /// `sha256(frame_model)` -- the on-chain `modelHash` keying the order book.
    pub model_hash: String,
    /// Per-model `InferenceOrderBook` address.
    #[serde(with = "crate::address::serde_canonical")]
    pub inference_order_book: String,
    /// Per-owner `RootModel` address.
    #[serde(with = "crate::address::serde_canonical")]
    pub root_model: String,
    /// Per-deal `TokenContract` -- the **deployed, active** address.
    #[serde(with = "crate::address::serde_self_dapp")]
    pub token_contract: String,
    /// The seller's provisioned `PrivateNote` (the market owner's note).
    #[serde(with = "crate::address::serde_canonical")]
    pub seller_note: String,
    /// Deal nonce (disambiguates multiple `TokenContract`s under one `RootModel`).
    pub nonce: u64,
    /// Tick price P in whole SHELL, the price the `TokenContract` was deployed with. Raw ECC[2]
    /// in memory, whole SHELL on disk: the file is read by an operator, and the book quotes prices
    /// in whole SHELL.
    #[serde(with = "crate::params::serde_price_shell")]
    pub price_per_tick: u128,
    /// Max ticks the `TokenContract` bounds the deal to.
    pub max_ticks: u128,
}

/// A live or durable market identity was read successfully and proved not to be the requested one.
/// Unlike an RPC/read failure, retrying cannot make this immutable mismatch converge.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct MarketIdentityMismatch {
    message: String,
}

impl MarketIdentityMismatch {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl MarketManifest {
    /// Prove that an already-active TokenContract is the exact deal described by this manifest.
    pub fn ensure_live_identity(
        &self,
        deal_terms: (u128, u128, u128),
        model_name: &str,
        model_hash: &str,
        seller_note: &str,
    ) -> Result<(), MarketIdentityMismatch> {
        let expected_terms = (crate::TICK_SIZE, self.price_per_tick, self.max_ticks);
        let same_note = crate::normalize_wallet_address(seller_note)
            .map_err(MarketIdentityMismatch::new)?
            == crate::normalize_wallet_address(&self.seller_note)
                .map_err(MarketIdentityMismatch::new)?;
        if deal_terms != expected_terms
            || model_name != self.frame_model
            || !model_hash.eq_ignore_ascii_case(&self.model_hash)
            || !same_note
        {
            return Err(MarketIdentityMismatch::new(format!(
                "active TokenContract {} does not match the requested existing deal: expected \
                 model={} model_hash={} tick_size={} price_per_tick={} max_ticks={} seller_note={}, \
                 got model={} model_hash={} tick_size={} price_per_tick={} max_ticks={} seller_note={}; \
                 refusing to reuse its manifest",
                crate::address::display_self_dapp(&self.token_contract),
                self.frame_model,
                self.model_hash,
                crate::TICK_SIZE,
                self.price_per_tick,
                self.max_ticks,
                self.seller_note,
                model_name,
                model_hash,
                deal_terms.0,
                deal_terms.1,
                deal_terms.2,
                seller_note,
            )));
        }
        Ok(())
    }

    /// Serialize to pretty JSON (the on-disk `--output` format).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse from JSON (what `dexdo seller`/`buyer` load to resolve the market).
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Integrity check: a corrupt/hand-edited manifest must not silently drive a real-money
    /// CLI. Rejects an empty `token_contract`/`frame_model` and a `model_hash` that is inconsistent with
    /// `sha256(frame_model)`. Returns a human-readable reason on failure.
    pub fn validate(&self) -> Result<(), String> {
        if self.token_contract.trim().is_empty() {
            return Err("token_contract is empty".to_string());
        }
        if self.frame_model.trim().is_empty() {
            return Err("frame_model is empty".to_string());
        }
        let expected = model_hash_for(&self.frame_model);
        if self.model_hash != expected {
            return Err(format!(
                "model_hash {} does not match sha256(frame_model `{}`) = {} -- inconsistent/corrupt manifest",
                self.model_hash, self.frame_model, expected
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The on-chain model id must be canonical `producer--model--version` (what the indexer parses): exactly
    /// three non-empty `--`-parts. An OpenAI slug (`qwen/qwen3-32b`) or a 2/4-part name is rejected fail-loud.
    #[test]
    fn validate_canonical_model_id_requires_three_parts() {
        assert!(validate_canonical_model_id("openai--gpt--4.1").is_ok());
        assert!(validate_canonical_model_id("qwen--qwen3--32b").is_ok());
        // Non-canonical: OpenAI slug, too few / too many parts, empty part.
        assert!(validate_canonical_model_id("qwen/qwen3-32b").is_err());
        assert!(validate_canonical_model_id("dexdo-mock").is_err());
        assert!(validate_canonical_model_id("a--b").is_err());
        assert!(validate_canonical_model_id("a--b--c--d").is_err());
        assert!(validate_canonical_model_id("a----c").is_err());
    }

    /// `model_hash_for` is the on-chain model key = `0x` + lowercase hex of `sha256(frame_model)`. On 4.0.6
    /// the IOB/TokenContract ctors require `sha256(modelName) == modelHash`, so this derivation IS the
    /// model-name invariant fixed inPR -- guard it offline (deterministic, 32-byte, distinct per frame).
    #[test]
    fn model_hash_for_is_sha256_hex() {
        // Known SHA-256 vector: sha256("abc").
        assert_eq!(
            model_hash_for("abc"),
            "0xba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let h = model_hash_for("qwen/qwen3-32b");
        assert!(h.starts_with("0x") && h.len() == 66, "{h}");
        assert!(
            h[2..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{h}"
        );
        assert_eq!(
            model_hash_for("qwen/qwen3-32b"),
            model_hash_for("qwen/qwen3-32b")
        );
        assert_ne!(
            model_hash_for("qwen/qwen3-32b"),
            model_hash_for("qwen/qwen3-32b-v2")
        );
    }

    /// `resolve_model_name` is the inverse of `model_hash_for` over the configured set: a deal's
    /// on-chain `modelHash` maps back to the configured model name by matching the hash of each known name.
    #[test]
    fn resolve_model_name_round_trips_model_hash_for() {
        let known = vec![
            "qwen/qwen3-32b".to_string(),
            "meta/llama-3.1-8b".to_string(),
        ];
        let h = model_hash_for("meta/llama-3.1-8b");
        assert_eq!(
            resolve_model_name(Some(&h), &known).as_deref(),
            Some("meta/llama-3.1-8b")
        );
        let h2 = model_hash_for("qwen/qwen3-32b");
        assert_eq!(
            resolve_model_name(Some(&h2), &known).as_deref(),
            Some("qwen/qwen3-32b")
        );
    }

    /// The match normalizes the optional `0x` prefix and case, so it works whether `getModelHash` returns the
    /// hash bare or `0x`-prefixed, upper or lower.
    #[test]
    fn resolve_model_name_normalizes_prefix_and_case() {
        let known = vec!["qwen/qwen3-32b".to_string()];
        let full = model_hash_for("qwen/qwen3-32b"); // 0x + lowercase
        let bare_upper = full[2..].to_ascii_uppercase(); // no prefix, uppercase
        assert_eq!(
            resolve_model_name(Some(&bare_upper), &known).as_deref(),
            Some("qwen/qwen3-32b")
        );
        let prefixed_upper = format!("0X{bare_upper}");
        assert_eq!(
            resolve_model_name(Some(&prefixed_upper), &known).as_deref(),
            Some("qwen/qwen3-32b")
        );
    }

    /// A hash outside the configured set, an absent hash, and an empty configured set all return `None` (the
    /// accounting view then shows the raw hash rather than guessing a name).
    #[test]
    fn resolve_model_name_unknown_absent_or_empty_is_none() {
        let known = vec!["qwen/qwen3-32b".to_string()];
        assert_eq!(
            resolve_model_name(Some(&model_hash_for("some/other-model")), &known),
            None
        );
        assert_eq!(resolve_model_name(None, &known), None);
        assert_eq!(
            resolve_model_name(Some(&model_hash_for("qwen/qwen3-32b")), &[]),
            None
        );
    }

    fn addr(c: char) -> String {
        format!("0:{}", std::iter::repeat_n(c, 64).collect::<String>())
    }

    fn sample() -> MarketManifest {
        MarketManifest {
            network: "net-a".to_string(),
            frame_model: "qwen/qwen3-32b".to_string(),
            model_hash: model_hash_for("qwen/qwen3-32b"),
            inference_order_book: addr('1'),
            root_model: addr('2'),
            token_contract: addr('3'),
            seller_note: addr('4'),
            nonce: 7,
            // A thousand SHELL a tick. The file carries whole SHELL, and a raw 1000 is not a price
            // the book can hold.
            price_per_tick: 1000 * crate::params::PRICE_STEP,
            max_ticks: 1024,
        }
    }

    /// A market file written before prices were quoted in whole SHELL is refused, not read as a
    /// price a billion times its own size.

    /// `dexdo provision` wrote `price_per_tick: 3000000000` for three SHELL a tick while this field
    /// held raw ECC[2] units. Read as SHELL that is three billion a tick -- above the largest note
    /// that exists, so no note could pay for one tick at it. The refusal names that, and names the
    /// command that writes a current file.
    #[test]
    fn a_market_file_from_before_the_unit_change_is_refused_by_name() {
        let mut file = serde_json::to_value(sample()).expect("sample serializes");
        file["price_per_tick"] = serde_json::json!(3_000_000_000u64);

        let error = MarketManifest::from_json(&file.to_string())
            .expect_err("a raw-unit price must not be read as SHELL");
        let text = error.to_string();
        assert!(
            text.contains("above the largest note that exists"),
            "the refusal must say why the figure cannot be a price: {text}"
        );
        assert!(
            text.contains("dexdo provision"),
            "the refusal must name what writes a current file: {text}"
        );

        // The largest note that does exist is still a price this reads.
        let mut top = serde_json::to_value(sample()).expect("sample serializes");
        top["price_per_tick"] = serde_json::json!(1_000_000u64);
        let read = MarketManifest::from_json(&top.to_string()).expect("a price at the bound reads");
        assert_eq!(read.price_per_tick, 1_000_000 * crate::params::PRICE_STEP);
    }

    /// The output/parsing contract: round-trips losslessly and carries the fields the
    /// `--market` loader feeds to `dexdo seller`/`buyer` (`token_contract`, `frame_model`).
    #[test]
    fn manifest_roundtrips_and_exposes_consumable_fields() {
        let m = sample();
        let json = m.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // a per-deal TokenContract is a self-DApp account, written `<account>::<account>`.
        let tc = std::iter::repeat_n('3', 64).collect::<String>();
        assert_eq!(v["token_contract"], format!("{tc}::{tc}"));
        assert_eq!(v["frame_model"], "qwen/qwen3-32b");
        assert_eq!(MarketManifest::from_json(&json).unwrap(), m);
    }

    /// Issue: a manifest is **written** with canonical `<dapp_id>::<account_id>` addresses, and is
    /// **read** from either that or a legacy `0:<account_id>` file written by an older version. Both load
    /// to the same manifest, so an existing market.json keeps driving the same deal.

    /// The DApp half is role-specific. A per-deal `TokenContract` is a self-DApp account - its own
    /// `info.dapp_id` IS its account id - so it is written `<account_id>::<account_id>`. The book, the
    /// `RootModel` and the seller's `PrivateNote` are system contracts of the shared dexdo DApp.
    #[test]
    fn manifest_writes_canonical_addresses_and_reads_legacy_ones() {
        let m = sample();
        let json = m.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let self_dapp = addr('3').strip_prefix("0:").unwrap().to_string();
        for (field, legacy, dapp) in [
            (
                "inference_order_book",
                addr('1'),
                crate::address::DEXDO_DAPP_ID.to_string(),
            ),
            (
                "root_model",
                addr('2'),
                crate::address::DEXDO_DAPP_ID.to_string(),
            ),
            ("token_contract", addr('3'), self_dapp.clone()),
            (
                "seller_note",
                addr('4'),
                crate::address::DEXDO_DAPP_ID.to_string(),
            ),
        ] {
            let written = v[field].as_str().unwrap();
            assert_eq!(
                written,
                format!("{dapp}::{}", legacy.strip_prefix("0:").unwrap()),
                "{field} was not written canonically"
            );
            assert!(!written.starts_with("0:"), "{field} kept the legacy form");
        }

        // A legacy file loads to the same manifest a canonical one does. Both DApp halves are
        // stripped, so the fixture is the pre- file rather than a half-migrated one.
        let legacy_json = json
            .replace(&format!("{}::", crate::address::DEXDO_DAPP_ID), "0:")
            .replace(&format!("{self_dapp}::"), "0:");
        assert!(
            !legacy_json.contains("::"),
            "the legacy fixture still carries a DApp half: {legacy_json}"
        );
        assert_eq!(MarketManifest::from_json(&legacy_json).unwrap(), m);
        assert_eq!(MarketManifest::from_json(&json).unwrap(), m);
    }

    /// Privacy: the manifest must never carry a secret/seed/owner key.
    #[test]
    fn manifest_carries_no_secret_fields() {
        let j = sample().to_json().unwrap().to_lowercase();
        for bad in ["secret", "seed", "owner_key", "private", "priv_"] {
            assert!(!j.contains(bad), "manifest leaked `{bad}`");
        }
    }

    /// Integrity: `validate()` accepts a consistent manifest and rejects empty
    /// addresses/model + a `model_hash` that does not match `sha256(frame_model)`.
    #[test]
    fn manifest_validate_rejects_inconsistent() {
        assert!(sample().validate().is_ok());
        // model_hash matches frame_model by construction.
        assert_eq!(sample().model_hash, model_hash_for(&sample().frame_model));

        let mut empty_tc = sample();
        empty_tc.token_contract = "  ".to_string();
        assert!(empty_tc.validate().is_err());

        let mut empty_fm = sample();
        empty_fm.frame_model = String::new();
        assert!(empty_fm.validate().is_err());

        // Wrong model_hash for the frame_model -- corrupt/hand-edited.
        let mut bad_hash = sample();
        bad_hash.model_hash = "0xdeadbeef".to_string();
        let err = bad_hash.validate().unwrap_err();
        assert!(err.contains("model_hash"), "{err}");

        // Changing frame_model without updating model_hash is also caught.
        let mut drifted = sample();
        drifted.frame_model = "llama/llama-3".to_string();
        assert!(drifted.validate().is_err());
    }
}
