//! Oracle/PMP prediction-market provisioning manifest: the dex-side artifact a `dexdo`
//! oracle/PMP provisioning run produces -- the addresses + range-event parameters of a per-event
//! prediction market(`OracleEventList` + range event + `PMP`) tied to an inference `InferenceOrderBook`.
//! Pure data(no chain, no feature gate); the output/parsing contract for, consumed by tests and
//! later CLI flows without hand-editing. `validate()` mirrors the on-chain
//! `OracleEventList.addRangeEvent` invariants (>=1 strictly-increasing bound; outcomes == bounds + 1;
//! < 20 outcomes), so a bad range config is rejected offline before any deploy.

use serde::{Deserialize, Serialize};

/// `uint256::MAX`(2^256 - 1) as a 78-digit decimal string -- for an offline range check, no bigint dep.
const UINT256_MAX_DEC: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";

/// Compare two non-negative decimal integer strings as numbers(no bigint dep): strip leading zeros, then
/// a longer string is the larger number; equal lengths compare lexicographically(decimal digits ordered).
fn cmp_uint_dec(a: &str, b: &str) -> std::cmp::Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// `s` is a decimal `uint256`: non-empty, all ASCII digits, and `<= uint256::MAX`(the contract type).
fn is_uint256_dec(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|c| c.is_ascii_digit())
        && cmp_uint_dec(s, UINT256_MAX_DEC) != std::cmp::Ordering::Greater
}

/// `s` is a `uint256` hash in `0x`-hex: `0x` + 1..=64 hex nibbles(<= 256 bits).
fn is_uint256_hex(s: &str) -> bool {
    let Some(hex) = s.strip_prefix("0x") else {
        return false;
    };
    (1..=64).contains(&hex.len()) && hex.bytes().all(|c| c.is_ascii_hexdigit())
}

/// A provisioned dex prediction-market. `bounds` are the range-event boundaries (uint256
/// prices, kept as decimal strings); `outcome_names` label the `bounds.len() + 1` ranges. Addresses are
/// `workchain:hex`. No secrets -- public/derivable only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleMarketManifest {
    /// Network the market is deployed on(e.g. `shellnet`).
    pub network: String,
    /// `RootOracle` address(the dex root that deploys oracles).
    pub root_oracle: String,
    /// Per-owner `Oracle` address(deploys `OracleEventList`s).
    pub oracle: String,
    /// Per-oracle `OracleEventList` address(holds the range events).
    pub oracle_event_list: String,
    /// Hash of the PMP oracle list(`oracleListHash`).
    pub oracle_list_hash: String,
    /// Event identifier hash(`eventId`).
    pub event_id: String,
    /// Human-readable event name.
    pub event_name: String,
    /// Per-event `PMP`(prediction-market pool) address.
    pub pmp: String,
    /// PMP token type.
    pub token_type: u32,
    /// The inference `InferenceOrderBook` the range event resolves against(the price source).
    pub inference_order_book: String,
    /// The OB's model identity(`frame_model`), so the market's price source is unambiguous.
    pub frame_model: String,
    /// Resolution deadline(`deadline`, doubles as the PMP result-start).
    pub deadline: u64,
    /// Range-event boundaries(uint256 prices as decimal strings), strictly increasing.
    pub bounds: Vec<String>,
    /// Outcome labels -- exactly `bounds.len() + 1`(one per range).
    pub outcome_names: Vec<String>,
}

impl OracleMarketManifest {
    /// Serialize to pretty JSON(the on-disk output format).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse from JSON.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Integrity check -- mirrors the on-chain `OracleEventList.addRangeEvent` requires so a
    /// bad range config is rejected offline, before a real-money deploy:
    /// - every address/identity field is non-empty;
    /// - `bounds` has >=1 entry, each a valid `uint256` decimal price, strictly increasing;
    /// - `outcome_names` has exactly `bounds.len() + 1` entries, and fewer than 20 total.
    pub fn validate(&self) -> Result<(), String> {
        for (field, val) in [
            ("root_oracle", &self.root_oracle),
            ("oracle", &self.oracle),
            ("oracle_event_list", &self.oracle_event_list),
            ("pmp", &self.pmp),
            ("inference_order_book", &self.inference_order_book),
            ("event_name", &self.event_name),
            ("frame_model", &self.frame_model),
        ] {
            if val.trim().is_empty() {
                return Err(format!("{field} is empty"));
            }
        }
        // `event_id` / `oracle_list_hash` are `uint256` identifiers that drive `confirmEvent`,
        // `resolveRange`, and the PMP address derivation. TVM getters in the two live SDK stacks have
        // emitted both `0x`-hex and decimal strings; accept both notations but reject malformed values.
        for (field, val) in [
            ("event_id", &self.event_id),
            ("oracle_list_hash", &self.oracle_list_hash),
        ] {
            if val.trim().is_empty() {
                return Err(format!("{field} is empty"));
            }
            if !is_uint256_hex(val) && !is_uint256_dec(val) {
                return Err(format!(
                    "{field} `{val}` is not a uint256 (0x-hex or decimal)"
                ));
            }
        }
        let n = self.bounds.len();
        if n < 1 {
            return Err("bounds must have at least one boundary (>= 2 outcomes)".to_string());
        }
        // `bounds` is the contract's `uint256[]` -- validate + compare as uint256(decimal), NOT u128, so a
        // valid bound above u128::MAX is accepted and one above uint256::MAX is rejected(mirror the chain).
        let mut prev: Option<&str> = None;
        for (i, b) in self.bounds.iter().enumerate() {
            if !is_uint256_dec(b) {
                return Err(format!(
                    "bounds[{i}] `{b}` is not a uint256 decimal (<= uint256::MAX)"
                ));
            }
            if let Some(p) = prev {
                if cmp_uint_dec(b, p) != std::cmp::Ordering::Greater {
                    return Err(format!(
                        "bounds must be strictly increasing: bounds[{i}] {b} <= bounds[{}] {p}",
                        i - 1
                    ));
                }
            }
            prev = Some(b);
        }
        let outcomes = self.outcome_names.len();
        if outcomes != n + 1 {
            return Err(format!(
                "outcome_names must be exactly bounds.len()+1 = {} (got {outcomes})",
                n + 1
            ));
        }
        if outcomes >= 20 {
            return Err(format!(
                "too many outcomes ({outcomes}); the contract caps it below 20"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OracleMarketManifest {
        OracleMarketManifest {
            network: "shellnet".to_string(),
            root_oracle: "0:ro".to_string(),
            oracle: "0:or".to_string(),
            oracle_event_list: "0:oel".to_string(),
            oracle_list_hash: "0xabc123".to_string(),
            event_id: "0xdeadbeef".to_string(),
            event_name: "qwen-price-week".to_string(),
            pmp: "0:pmp".to_string(),
            token_type: 2,
            inference_order_book: "0:ob".to_string(),
            frame_model: "qwen/qwen3-32b".to_string(),
            deadline: 1_900_000_000,
            bounds: vec!["100".to_string(), "200".to_string(), "300".to_string()],
            outcome_names: vec![
                "<100".to_string(),
                "100-200".to_string(),
                "200-300".to_string(),
                ">300".to_string(),
            ],
        }
    }

    /// Output/parsing contract: round-trips losslessly.
    #[test]
    fn oracle_manifest_roundtrips() {
        let m = sample();
        let json = m.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["pmp"], "0:pmp");
        assert_eq!(v["bounds"].as_array().unwrap().len(), 3);
        assert_eq!(OracleMarketManifest::from_json(&json).unwrap(), m);
    }

    /// Privacy: no secret/seed/owner key may appear in the manifest.
    #[test]
    fn oracle_manifest_carries_no_secret_fields() {
        let j = sample().to_json().unwrap().to_lowercase();
        for bad in ["secret", "seed", "owner_key", "private", "priv_"] {
            assert!(!j.contains(bad), "manifest leaked `{bad}`");
        }
    }

    /// Integrity: `validate()` accepts a consistent range config and rejects the bad cases
    /// the contract's `addRangeEvent` rejects(non-increasing bounds, wrong outcome count, empty fields).
    #[test]
    fn oracle_manifest_validate_mirrors_contract() {
        assert!(sample().validate().is_ok());

        // Strictly-increasing bounds enforced.
        let mut not_incr = sample();
        not_incr.bounds = vec!["100".into(), "100".into()];
        not_incr.outcome_names = vec!["a".into(), "b".into(), "c".into()];
        assert!(not_incr
            .validate()
            .unwrap_err()
            .contains("strictly increasing"));

        // outcomes must be bounds+1.
        let mut wrong_outcomes = sample();
        wrong_outcomes.outcome_names.pop();
        assert!(wrong_outcomes
            .validate()
            .unwrap_err()
            .contains("outcome_names"));

        // empty bounds.
        let mut empty_bounds = sample();
        empty_bounds.bounds.clear();
        empty_bounds.outcome_names = vec!["only".into()];
        assert!(empty_bounds.validate().is_err());

        // bound not a uint256 decimal.
        let mut bad_bound = sample();
        bad_bound.bounds[0] = "notanumber".into();
        assert!(bad_bound.validate().unwrap_err().contains("uint256"));

        // empty address field.
        let mut empty_pmp = sample();
        empty_pmp.pmp = "  ".into();
        assert!(empty_pmp.validate().unwrap_err().contains("pmp"));

        // < 20 outcomes cap.
        let mut too_many = sample();
        too_many.bounds = (0..20).map(|i| i.to_string()).collect();
        too_many.outcome_names = (0..21).map(|i| format!("o{i}")).collect();
        assert!(too_many.validate().unwrap_err().contains("too many"));
    }

    /// Issue(review): `event_id`/`oracle_list_hash` are uint256 identifiers,
    /// and `bounds` are uint256(NOT u128) -- a bound above u128::MAX is accepted, above uint256::MAX rejected.
    #[test]
    fn oracle_manifest_validates_uint256_identifiers() {
        let mut decimal_ids = sample();
        decimal_ids.event_id = "123456789".into();
        decimal_ids.oracle_list_hash = "987654321".into();
        assert!(decimal_ids.validate().is_ok());

        // event_id / oracle_list_hash: empty + malformed are rejected.
        let mut empty_evt = sample();
        empty_evt.event_id = String::new();
        assert!(empty_evt.validate().unwrap_err().contains("event_id"));
        let mut bad_evt = sample();
        bad_evt.event_id = "0xnothex".into(); // 'n','o','t','x' are not hex nibbles
        assert!(bad_evt.validate().unwrap_err().contains("event_id"));
        let mut no_prefix = sample();
        no_prefix.oracle_list_hash = "deadbeef".into(); // missing 0x
        assert!(no_prefix
            .validate()
            .unwrap_err()
            .contains("oracle_list_hash"));
        let mut empty_list = sample();
        empty_list.oracle_list_hash = "  ".into();
        assert!(empty_list
            .validate()
            .unwrap_err()
            .contains("oracle_list_hash"));

        // bounds are uint256, not u128: a value above u128::MAX is ACCEPTED(when ordered).
        let above_u128 = "340282366920938463463374607431768211456"; // u128::MAX + 1
        let mut big = sample();
        big.bounds = vec!["1".into(), above_u128.into()];
        big.outcome_names = vec!["a".into(), "b".into(), "c".into()];
        assert!(
            big.validate().is_ok(),
            "a bound above u128::MAX must be accepted"
        );

        // non-increasing huge(uint256) values are rejected.
        let mut not_incr_big = sample();
        not_incr_big.bounds = vec![above_u128.into(), above_u128.into()];
        not_incr_big.outcome_names = vec!["a".into(), "b".into(), "c".into()];
        assert!(not_incr_big
            .validate()
            .unwrap_err()
            .contains("strictly increasing"));

        // a value above uint256::MAX is rejected.
        let above_u256 =
            "115792089237316195423570985008687907853269984665640564039457584007913129639936"; // uint256::MAX + 1
        let mut overflow = sample();
        overflow.bounds = vec![above_u256.into()];
        overflow.outcome_names = vec!["a".into(), "b".into()];
        assert!(overflow.validate().unwrap_err().contains("uint256"));
    }
}
