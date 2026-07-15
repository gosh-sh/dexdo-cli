//! Market-provisioning output manifest (issue #24): the addresses a `dexdo provision` run produces.
//! One JSON file per per-deal market (`InferenceOrderBook` + `RootModel` + the deployed `TokenContract`).
//!
//! Pure data (no chain, no feature gate); this is the output/parsing contract, covered by a
//! deterministic offline guard.
//!
//! **Note-funded (directive #58):** `dexdo provision` brings up the OB + RootModel + per-deal `TokenContract`
//! ALL from the seller note's own ECC[2] — no operator multisig. The note pre-funds the RootModel/TC uninit
//! deploy addresses via `PrivateNote.fundDeployShell` and the external seller-signed deploys activate them, so
//! `token_contract` here is the **deployed, active** per-deal TC (not a derived placeholder). Giver is the
//! one-time mint faucet only (D13); zero giver in the operate path.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `sha256(frame_model)` as `0x<hex>` — the canonical on-chain modelHash (D10): the seller
/// (`--model`→`frame_model`) and the buyer (`--frame-model`) derive it from the SAME `frame_model` to
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

/// Validate that a model identifier is the **canonical `producer--model--version`** form — exactly three
/// non-empty `--`-separated parts (e.g. `openai--gpt--4.1`, `qwen--qwen3--32b`). This is the string
/// hashed into the on-chain `modelHash` (and stored as the book's `getModelName`), and it is what the model
/// indexer parses (`split("--")`, three parts) to show a human-readable market name; a non-canonical name
/// (e.g. the OpenAI slug `qwen/qwen3-32b`) deploys a book the indexer can only key by its raw hash. Fail-loud
/// so a mis-set `frame_model` is caught **before** a book/offer is created, not after.
pub fn validate_canonical_model_id(name: &str) -> Result<(), String> {
    let parts: Vec<&str> = name.split("--").collect();
    if parts.len() == 3 && parts.iter().all(|p| !p.trim().is_empty()) {
        return Ok(());
    }
    Err(format!(
        "model id `{name}` is not canonical `producer--model--version` (exactly three non-empty `--`-separated \
         parts, e.g. `qwen--qwen3--32b`). This string is the on-chain model name/hash the indexer \
         parses — an OpenAI slug like `qwen/qwen3-32b` belongs in `served_model`, not the market model id."
    ))
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

/// Resolve a `modelHash` back to a configured model name (issue #23) — the inverse of [`model_hash_for`].
/// This is an **integrity/fallback** helper: the 4.0.6 `TokenContract` exposes `getModelName()` directly (the
/// authoritative display name), so the real reader uses that name and calls this to **cross-check** it against
/// the operator's configured set (or to recover the name when only the hash — `getModelHash` — is on hand). It
/// matches by hashing each configured name (normalized for the optional `0x`/case). Returns the first match, or
/// `None` if the hash is absent or unknown to the configured set — then the caller shows the raw hash rather
/// than guessing. The configured set is the operator's `models.json` / market manifest(s), never an unbounded
/// preimage search.
pub fn resolve_model_name(model_hash: Option<&str>, known_models: &[String]) -> Option<String> {
    let want = normalize_hash(model_hash?);
    known_models
        .iter()
        .find(|name| normalize_hash(&model_hash_for(name)) == want)
        .cloned()
}

/// A provisioned per-deal market (issue #24). `token_contract` is what `dexdo seller`/`buyer`
/// take as `--token-contract`; the rest is the surrounding market identity (for transparency and
/// `dexdo monitor`). Addresses are `workchain:hex` strings. No secrets — public/derivable only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketManifest {
    /// Network the market is deployed on (e.g. `shellnet`).
    pub network: String,
    /// Configured frame model id (the model the seller serves; the buyer's `--frame-model`).
    pub frame_model: String,
    /// `sha256(frame_model)` — the on-chain `modelHash` keying the order book.
    pub model_hash: String,
    /// Per-model `InferenceOrderBook` address.
    pub inference_order_book: String,
    /// Per-owner `RootModel` address.
    pub root_model: String,
    /// Per-deal `TokenContract` — the **deployed, active** address (note-funded via `fundDeployShell`, #58).
    pub token_contract: String,
    /// The seller's provisioned `PrivateNote` (the market owner's note).
    pub seller_note: String,
    /// Deal nonce (disambiguates multiple `TokenContract`s under one `RootModel`).
    pub nonce: u64,
    /// Tick price P (SHELL) the `TokenContract` was deployed with.
    pub price_per_tick: u128,
    /// Max ticks the `TokenContract` bounds the deal to.
    pub max_ticks: u128,
}

impl MarketManifest {
    /// Serialize to pretty JSON (the on-disk `--output` format).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse from JSON (what `dexdo seller`/`buyer` load to resolve the market).
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Integrity check (issue #24): a corrupt/hand-edited manifest must not silently drive a real-money
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
                "model_hash {} does not match sha256(frame_model `{}`) = {} — inconsistent/corrupt manifest",
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
    /// model-name invariant fixed in #24/PR#44 — guard it offline (deterministic, 32-byte, distinct per frame).
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

    /// `resolve_model_name` is the inverse of `model_hash_for` over the configured set (issue #23): a deal's
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

    fn sample() -> MarketManifest {
        MarketManifest {
            network: "shellnet".to_string(),
            frame_model: "qwen/qwen3-32b".to_string(),
            model_hash: model_hash_for("qwen/qwen3-32b"),
            inference_order_book: "0:11".to_string(),
            root_model: "0:22".to_string(),
            token_contract: "0:33".to_string(),
            seller_note: "0:44".to_string(),
            nonce: 7,
            price_per_tick: 1000,
            max_ticks: 1024,
        }
    }

    /// The output/parsing contract (issue #24): round-trips losslessly and carries the fields the
    /// `--market` loader feeds to `dexdo seller`/`buyer` (`token_contract`, `frame_model`).
    #[test]
    fn manifest_roundtrips_and_exposes_consumable_fields() {
        let m = sample();
        let json = m.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["token_contract"], "0:33");
        assert_eq!(v["frame_model"], "qwen/qwen3-32b");
        assert_eq!(MarketManifest::from_json(&json).unwrap(), m);
    }

    /// Privacy (issue #24): the manifest must never carry a secret/seed/owner key.
    #[test]
    fn manifest_carries_no_secret_fields() {
        let j = sample().to_json().unwrap().to_lowercase();
        for bad in ["secret", "seed", "owner_key", "private", "priv_"] {
            assert!(!j.contains(bad), "manifest leaked `{bad}`");
        }
    }

    /// Integrity (issue #24, review): `validate()` accepts a consistent manifest and rejects empty
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

        // Wrong model_hash for the frame_model — corrupt/hand-edited.
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
