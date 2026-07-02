//! `dexdo note deploy`: deploy a wallet-funded `PrivateNote` on shellnet in-process through
//! `gosh.ackinacki`, then fold the CLI-compatible result into a `DEXDO_PN_POOL` pool the `seller`/`buyer`
//! already consume. The chain call lives in `commands.rs::run_note_deploy`; the pure schema adapters
//! live here.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write as _;

const UNIT_SCALE: u128 = 1_000_000_000;
const SHELL_ECC_ID: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteAccountSnapshot {
    pub(crate) address: String,
    pub(crate) status: String,
    pub(crate) native_raw: u128,
    pub(crate) ecc: Vec<(u32, u128)>,
    pub(crate) code_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoteBalanceMap {
    Known(Vec<(u32, u128)>),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteGetterBalanceMaps {
    pub(crate) balance: NoteBalanceMap,
    pub(crate) locked_in_orders: NoteBalanceMap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteBalanceView {
    pub(crate) account: NoteAccountSnapshot,
    pub(crate) getters: NoteGetterBalanceMaps,
}

pub(crate) fn build_note_balance_view(
    note_addr: &str,
    account: Option<NoteAccountSnapshot>,
    getters: NoteGetterBalanceMaps,
) -> Result<NoteBalanceView> {
    let account = account.ok_or_else(|| {
        anyhow!(
            "cannot read PrivateNote account {note_addr}: account not found/null; refusing to report zero balance"
        )
    })?;
    Ok(NoteBalanceView { account, getters })
}

pub(crate) fn note_getter_balance_maps(details: Option<&Value>) -> NoteGetterBalanceMaps {
    let Some(details) = details else {
        return NoteGetterBalanceMaps {
            balance: NoteBalanceMap::Unknown("getDetails returned no data".to_string()),
            locked_in_orders: NoteBalanceMap::Unknown("getDetails returned no data".to_string()),
        };
    };
    NoteGetterBalanceMaps {
        balance: parse_balance_map(&details["balance"], "balance"),
        locked_in_orders: parse_balance_map(&details["lockedInOrders"], "lockedInOrders"),
    }
}

pub(crate) fn unknown_note_getter_balance_maps(reason: impl Into<String>) -> NoteGetterBalanceMaps {
    let reason = reason.into();
    NoteGetterBalanceMaps {
        balance: NoteBalanceMap::Unknown(reason.clone()),
        locked_in_orders: NoteBalanceMap::Unknown(reason),
    }
}

pub(crate) fn render_note_balance(view: &NoteBalanceView) -> String {
    let mut out = String::new();
    let account = &view.account;
    writeln!(&mut out, "PrivateNote {}", account.address).unwrap();
    writeln!(&mut out, "status: {}", account.status).unwrap();
    if let Some(code_hash) = account.code_hash.as_deref() {
        writeln!(&mut out, "code_hash: {code_hash}").unwrap();
    } else {
        writeln!(&mut out, "code_hash: unknown").unwrap();
    }
    writeln!(
        &mut out,
        "SHELL ECC[2]: {} SHELL (raw {})",
        decimal_units(account.ecc_value(SHELL_ECC_ID)),
        account.ecc_value(SHELL_ECC_ID)
    )
    .unwrap();
    writeln!(
        &mut out,
        "VMSHELL native gas: {} vmshell (raw {})",
        decimal_units(account.native_raw),
        account.native_raw
    )
    .unwrap();
    render_ecc_map(
        &mut out,
        "account ECC balances",
        &NoteBalanceMap::Known(account.ecc.clone()),
    );
    render_ecc_map(
        &mut out,
        "PrivateNote.getDetails balance",
        &view.getters.balance,
    );
    render_ecc_map(
        &mut out,
        "PrivateNote.getDetails lockedInOrders",
        &view.getters.locked_in_orders,
    );
    out
}

impl NoteAccountSnapshot {
    fn ecc_value(&self, id: u32) -> u128 {
        self.ecc
            .iter()
            .find(|(currency, _)| *currency == id)
            .map(|(_, value)| *value)
            .unwrap_or(0)
    }
}

fn render_ecc_map(out: &mut String, title: &str, map: &NoteBalanceMap) {
    writeln!(out, "{title}:").unwrap();
    match map {
        NoteBalanceMap::Known(entries) if entries.is_empty() => {
            writeln!(out, "  none reported").unwrap();
        }
        NoteBalanceMap::Known(entries) => {
            let mut entries = entries.clone();
            entries.sort_by_key(|(id, _)| *id);
            for (id, value) in entries {
                if id == SHELL_ECC_ID {
                    writeln!(
                        out,
                        "  ECC[2] SHELL: {} SHELL (raw {value})",
                        decimal_units(value)
                    )
                    .unwrap();
                } else {
                    writeln!(out, "  ECC[{id}]: raw {value}").unwrap();
                }
            }
        }
        NoteBalanceMap::Unknown(reason) => {
            writeln!(out, "  unknown ({reason})").unwrap();
        }
    }
}

fn decimal_units(raw: u128) -> String {
    format!("{}.{:09}", raw / UNIT_SCALE, raw % UNIT_SCALE)
}

fn parse_balance_map(value: &Value, name: &str) -> NoteBalanceMap {
    if value.is_null() {
        return NoteBalanceMap::Unknown(format!("{name} field unavailable"));
    }
    if let Some(object) = value.as_object() {
        let mut out = Vec::new();
        for (id, amount) in object {
            let Some(id) = parse_u32_key(id) else {
                return NoteBalanceMap::Unknown(format!("{name} contains non-numeric currency id"));
            };
            let Some(amount) = parse_u128_value(amount) else {
                return NoteBalanceMap::Unknown(format!("{name}[{id}] is not a u128"));
            };
            out.push((id, amount));
        }
        return NoteBalanceMap::Known(out);
    }
    if let Some(array) = value.as_array() {
        let mut out = Vec::new();
        for item in array {
            let Some(id) = item
                .get("currency")
                .or_else(|| item.get("id"))
                .and_then(parse_u32_value)
            else {
                return NoteBalanceMap::Unknown(format!("{name} array entry missing currency id"));
            };
            let Some(amount) = item
                .get("value")
                .or_else(|| item.get("amount"))
                .and_then(parse_u128_value)
            else {
                return NoteBalanceMap::Unknown(format!("{name}[{id}] is not a u128"));
            };
            out.push((id, amount));
        }
        return NoteBalanceMap::Known(out);
    }
    NoteBalanceMap::Unknown(format!("{name} has unexpected JSON shape"))
}

fn parse_u32_key(raw: &str) -> Option<u32> {
    raw.parse::<u32>().ok().or_else(|| {
        raw.strip_prefix("0x")
            .or_else(|| raw.strip_prefix("0X"))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
    })
}

fn parse_u32_value(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| value.as_str().and_then(parse_u32_key))
}

fn parse_u128_value(value: &Value) -> Option<u128> {
    value.as_u64().map(u128::from).or_else(|| {
        let raw = value.as_str()?.trim();
        raw.parse::<u128>().ok().or_else(|| {
            raw.strip_prefix("0x")
                .or_else(|| raw.strip_prefix("0X"))
                .and_then(|hex| u128::from_str_radix(hex, 16).ok())
        })
    })
}

/// CLI-compatible note deploy state. A subset of its fields -- exactly those the pool needs. **Carries the owner
/// secret key** -- never log it.
/// `allow(dead_code)` off `shellnet`: the only non-test consumer(`run_note_deploy`) is shellnet-gated, and the
/// `cfg(test)` suite does not save these from clippy's non-test `-D warnings` pass on the default bin.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OnboardPnState {
    pub endpoint: String,
    pub nominal: String,
    pub token_type: u32,
    pub raw_value: u64,
    pub ecc_shell_deposit: u64,
    pub pn_address: Option<String>,
    pub deposit_identifier_hash: Option<String>,
    pub owner_public_key_hex: Option<String>,
    pub owner_secret_key_hex: Option<String>,
    pub deployed_at_unix: Option<u64>,
    pub shell_funded: bool,
    pub sanity_checked: bool,
}

#[cfg(feature = "shellnet")]
impl From<dexdo_core::private_note::DeployPrivateNoteResult> for OnboardPnState {
    fn from(s: dexdo_core::private_note::DeployPrivateNoteResult) -> Self {
        Self {
            endpoint: s.endpoint,
            nominal: s.nominal,
            token_type: s.token_type,
            raw_value: s.raw_value,
            ecc_shell_deposit: s.ecc_shell_deposit,
            pn_address: Some(s.pn_address),
            deposit_identifier_hash: Some(s.deposit_identifier_hash),
            owner_public_key_hex: Some(s.owner_public_key_hex),
            owner_secret_key_hex: Some(s.owner_secret_key_hex),
            deployed_at_unix: Some(s.deployed_at_unix),
            shell_funded: s.shell_funded,
            sanity_checked: s.sanity_checked,
        }
    }
}

/// output adapter: build a single DEXDO_PN_POOL **note** object from a fully deployed note state. Fails
/// loud if deploy did not complete(missing `pn_address`/keys, or not `shell_funded`/`sanity_checked`) -- folding a
/// half-deployed note into the pool would later strand the `seller`/`buyer` on an unusable note.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn pn_state_to_pool_note(s: &OnboardPnState) -> Result<Value> {
    let address = s.pn_address.as_deref().ok_or_else(|| {
        anyhow!("pn_state has no pn_address -- note deploy did not reach deployPrivateNote (step 1)")
    })?;
    let dih = s.deposit_identifier_hash.as_deref().ok_or_else(|| {
        anyhow!("pn_state has no deposit_identifier_hash -- incomplete note deploy")
    })?;
    let pubkey = s
        .owner_public_key_hex
        .as_deref()
        .ok_or_else(|| anyhow!("pn_state has no owner_public_key_hex -- incomplete note deploy"))?;
    let seckey = s
        .owner_secret_key_hex
        .as_deref()
        .ok_or_else(|| anyhow!("pn_state has no owner_secret_key_hex -- incomplete note deploy"))?;
    if !s.shell_funded || !s.sanity_checked {
        bail!(
            "note deploy state not fully deployed (shell_funded={}, sanity_checked={}) -- the PN has no gas / failed its \
             getDetails check; re-run `dexdo note deploy` (idempotent at the step boundary) before pooling it.",
            s.shell_funded,
            s.sanity_checked
        );
    }
    Ok(json!({
        "address": address,
        "deposit_identifier_hash": dih,
        "owner_public_key_hex": pubkey,
        "owner_secret_key_hex": seckey,
        "deployed_at_unix": s.deployed_at_unix.unwrap_or(0),
        "shell_funded": s.shell_funded,
        "native_funded": s.sanity_checked,
    }))
}

/// output adapter: append `note` to a `DEXDO_PN_POOL` JSON, creating the pool with the pool-level fields
/// from the deploy state(endpoint/nominal/token_type/raw_value/ecc) when it does not yet exist, or appending to an
/// existing matching pool. Refuses to mix nominals/token-types in one pool (the consumers assume a homogeneous
/// pool), and refuses to add a duplicate note `address`. Pure(takes the existing pool JSON, returns the new one).
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn pool_with_note_added(
    existing: Option<Value>,
    s: &OnboardPnState,
    note: Value,
    created_at_unix: u64,
    funding_multisig_address: &str,
) -> Result<Value> {
    let funding_multisig_address = dexdo_core::normalize_wallet_address(funding_multisig_address)
        .map_err(|e| anyhow!("{e}"))?;
    let mut pool = match existing {
        Some(p) => p,
        None => json!({
            "endpoint": s.endpoint,
            "created_at_unix": created_at_unix,
            "nominal": s.nominal,
            "token_type": s.token_type,
            "raw_value_per_pn": s.raw_value,
            "ecc_shell_deposit_per_pn": s.ecc_shell_deposit,
            "funding_multisig_address": funding_multisig_address,
            "notes": [],
        }),
    };
    // Homogeneity: a pool is one nominal + token_type(the seller/buyer pick any note assuming uniform value).
    if pool["nominal"] != json!(s.nominal) || pool["token_type"] != json!(s.token_type) {
        bail!(
            "pool nominal/token_type ({}/{}) != this note's ({}/{}): the consumers assume a homogeneous pool -- \
             use a separate --pool file per nominal/token-type.",
            pool["nominal"],
            pool["token_type"],
            s.nominal,
            s.token_type
        );
    }
    match pool.get("funding_multisig_address").and_then(Value::as_str) {
        Some(existing) => {
            let existing = dexdo_core::normalize_wallet_address(existing).map_err(|e| {
                anyhow!("--pool: malformed funding_multisig_address `{existing}`: {e}")
            })?;
            if existing != funding_multisig_address {
                bail!(
                    "pool funding_multisig_address {existing} != this note's {funding_multisig_address}: \
                     rewards provenance must not mix PrivateNotes funded by different multisigs. Use a separate \
                     --pool file for each funding multisig."
                );
            }
            pool["funding_multisig_address"] = json!(existing);
        }
        None => {
            let has_existing_notes = pool["notes"]
                .as_array()
                .map(|notes| !notes.is_empty())
                .unwrap_or(false);
            if has_existing_notes {
                bail!(
                    "--pool has existing notes but no funding_multisig_address: refusing to attach new rewards \
                     provenance to older notes of unknown origin. Create a fresh --pool or migrate the old pool \
                     explicitly after verifying its funding multisig."
                );
            }
            pool["funding_multisig_address"] = json!(funding_multisig_address);
        }
    }
    let notes = pool["notes"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("--pool: malformed (\"notes\" is not an array)"))?;
    let new_addr = note["address"].as_str().unwrap_or_default();
    if notes
        .iter()
        .any(|n| n["address"].as_str() == Some(new_addr))
    {
        bail!("note {new_addr} is already in the pool -- refusing to add a duplicate");
    }
    notes.push(note);
    Ok(pool)
}

#[cfg(test)]
mod note_deploy_tests {
    use super::*;

    fn complete_state() -> OnboardPnState {
        OnboardPnState {
            endpoint: "shellnet.ackinacki.org".into(),
            nominal: "N100".into(),
            token_type: 1,
            raw_value: 100_000_000_000,
            ecc_shell_deposit: 100_000_000_000,
            pn_address: Some("0:abc".into()),
            deposit_identifier_hash: Some("123".into()),
            owner_public_key_hex: Some("pub".into()),
            owner_secret_key_hex: Some("sec".into()),
            deployed_at_unix: Some(1000),
            shell_funded: true,
            sanity_checked: true,
        }
    }

    fn tvm_tonos_fixture_phrase() -> String {
        const WORD_INDICES: [u16; 12] = [
            1636, 1293, 905, 102, 1057, 1956, 1247, 1750, 597, 881, 1302, 3,
        ];
        WORD_INDICES
            .iter()
            .map(|i| bip39::Language::English.wordlist().get_word((*i).into()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// account-reader data formats SHELL/ECC[2] and native gas in readable units plus raw units.
    #[test]
    fn note_balance_formats_shell_and_native_balances() {
        let view = build_note_balance_view(
            "0:abc",
            Some(NoteAccountSnapshot {
                address: "0:abc".into(),
                status: "Active".into(),
                native_raw: 5_000_000_123,
                ecc: vec![(7, 42), (2, 1_234_567_890)],
                code_hash: Some("cafe".into()),
            }),
            NoteGetterBalanceMaps {
                balance: NoteBalanceMap::Known(vec![(2, 2_000_000_001), (1, 10)]),
                locked_in_orders: NoteBalanceMap::Unknown("getter unavailable".into()),
            },
        )
        .unwrap();
        let out = render_note_balance(&view);
        assert!(out.contains("PrivateNote 0:abc"), "{out}");
        assert!(
            out.contains("SHELL ECC[2]: 1.234567890 SHELL (raw 1234567890)"),
            "{out}"
        );
        assert!(
            out.contains("VMSHELL native gas: 5.000000123 vmshell (raw 5000000123)"),
            "{out}"
        );
        assert!(
            out.contains("ECC[2] SHELL: 2.000000001 SHELL (raw 2000000001)"),
            "{out}"
        );
        assert!(out.contains("ECC[7]: raw 42"), "{out}");
        assert!(out.contains("unknown (getter unavailable)"), "{out}");
    }

    /// negative: a null/unreadable account is not rendered as zero.
    #[test]
    fn note_balance_null_account_fails_loud() {
        let err = build_note_balance_view(
            "0:missing",
            None,
            unknown_note_getter_balance_maps("not queried"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cannot read PrivateNote account"), "{err}");
        assert!(err.contains("refusing to report zero"), "{err}");
    }

    /// `getDetails` maps preserve unknown vs empty and parse known token maps.
    #[test]
    fn note_balance_getter_maps_preserve_unknown() {
        let maps = note_getter_balance_maps(Some(&json!({
            "balance": {"2": "3000000000", "7": "9"},
            "lockedInOrders": null
        })));
        assert_eq!(
            maps.balance,
            NoteBalanceMap::Known(vec![(2, 3_000_000_000), (7, 9)])
        );
        assert!(matches!(maps.locked_in_orders, NoteBalanceMap::Unknown(_)));

        let maps = note_getter_balance_maps(Some(&json!({
            "balance": {},
            "lockedInOrders": {}
        })));
        assert_eq!(maps.balance, NoteBalanceMap::Known(vec![]));
        assert_eq!(maps.locked_in_orders, NoteBalanceMap::Known(vec![]));
    }

    /// the command body is read-only and address-only: no key read and no signed/write helper.
    #[test]
    fn note_balance_command_path_is_read_only() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_note_balance")
            .expect("run_note_balance present");
        let end = source[start..]
            .find("/// `dexdo note withdraw`")
            .map(|offset| start + offset)
            .expect("run_note_balance end marker present");
        let body = &source[start..end];
        assert!(body.contains(".get_account("), "{body}");
        assert!(body.contains(".private_note_details("), "{body}");
        for forbidden in [
            "read_secret_hex",
            "note_key",
            "KeyPair",
            ".submit(",
            ".call(",
            "withdraw_note_tokens",
        ] {
            assert!(
                !body.contains(forbidden),
                "run_note_balance contains forbidden write/key path {forbidden}: {body}"
            );
        }
    }

    /// a fully deployed note state maps to the exact pool note schema the seller/buyer consume.
    #[test]
    fn pn_state_to_note_exact_schema() {
        let n = pn_state_to_pool_note(&complete_state()).unwrap();
        assert_eq!(n["address"], "0:abc");
        assert_eq!(n["deposit_identifier_hash"], "123");
        assert_eq!(n["owner_public_key_hex"], "pub");
        assert_eq!(n["owner_secret_key_hex"], "sec");
        assert_eq!(n["deployed_at_unix"], 1000);
        assert_eq!(n["shell_funded"], true);
        assert_eq!(n["native_funded"], true);
    }

    /// (negatives): an incomplete deploy state fails loud -- never pooled.
    #[test]
    fn incomplete_onboard_fails_loud() {
        let mut s = complete_state();
        s.pn_address = None;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("pn_address"));
        let mut s = complete_state();
        s.shell_funded = false;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("not fully deployed"));
        let mut s = complete_state();
        s.sanity_checked = false;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("not fully deployed"));
    }

    /// a fresh pool is created from the pn_state pool-level fields; a second note appends.
    #[test]
    fn pool_create_then_append() {
        let s = complete_state();
        let n1 = pn_state_to_pool_note(&s).unwrap();
        let wallet = format!("0:{}", "a".repeat(64));
        let pool = pool_with_note_added(None, &s, n1, 42, &wallet).unwrap();
        assert_eq!(pool["nominal"], "N100");
        assert_eq!(pool["raw_value_per_pn"], 100_000_000_000u64);
        assert_eq!(pool["funding_multisig_address"], wallet);
        assert_eq!(pool["notes"].as_array().unwrap().len(), 1);

        let mut s2 = complete_state();
        s2.pn_address = Some("0:def".into());
        let n2 = pn_state_to_pool_note(&s2).unwrap();
        let pool = pool_with_note_added(Some(pool), &s2, n2, 43, &wallet).unwrap();
        assert_eq!(pool["funding_multisig_address"], wallet);
        assert_eq!(pool["notes"].as_array().unwrap().len(), 2);
    }

    /// (negatives): duplicate address + mixed nominal are refused.
    #[test]
    fn pool_refuses_duplicate_and_mixed() {
        let s = complete_state();
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        // duplicate address
        let dup = pn_state_to_pool_note(&s).unwrap();
        assert!(
            pool_with_note_added(Some(pool.clone()), &s, dup, 2, &wallet)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        // mixed nominal
        let mut s2 = complete_state();
        s2.nominal = "N1000".into();
        s2.pn_address = Some("0:xyz".into());
        let n2 = pn_state_to_pool_note(&s2).unwrap();
        assert!(pool_with_note_added(Some(pool), &s2, n2, 3, &wallet)
            .unwrap_err()
            .to_string()
            .contains("homogeneous pool"));
    }

    /// rewards provenance is root-level and cannot silently mix funding multisigs or backfill legacy pools.
    #[test]
    fn pool_records_and_guards_funding_multisig_provenance() {
        let s = complete_state();
        let h1 = "1".repeat(64);
        let h2 = "B".repeat(64);
        let wallet_half_form = format!("{h1}::{h2}");
        let wallet = format!("0:{}", h2.to_ascii_lowercase());
        let other_wallet = format!("0:{}", "c".repeat(64));

        let pool = pool_with_note_added(
            None,
            &s,
            pn_state_to_pool_note(&s).unwrap(),
            1,
            &wallet_half_form,
        )
        .unwrap();
        assert_eq!(pool["funding_multisig_address"], wallet);

        let mut s2 = complete_state();
        s2.pn_address = Some("0:def".into());
        let err = pool_with_note_added(
            Some(pool.clone()),
            &s2,
            pn_state_to_pool_note(&s2).unwrap(),
            2,
            &other_wallet,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("funding_multisig_address"), "{err}");

        let mut legacy = pool;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("funding_multisig_address");
        let err = pool_with_note_added(
            Some(legacy),
            &s2,
            pn_state_to_pool_note(&s2).unwrap(),
            3,
            &wallet,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown origin"), "{err}");
    }

    /// the funding wallet seed phrase is an input-only credential. The pool stores only the deployed note
    /// material the runtime consumes; seed words must not appear in serialized pool output.
    #[test]
    fn pool_output_does_not_contain_seed_words() {
        let phrase = tvm_tonos_fixture_phrase();
        let derived = dexdo::wallet_seed::derive_multisig_key_from_seed_phrase(&phrase).unwrap();
        let mut state = complete_state();
        state.owner_public_key_hex = Some(derived.public_hex().to_string());
        state.owner_secret_key_hex = Some(derived.secret_hex().to_string());
        let wallet = format!("0:{}", "a".repeat(64));
        let pool = pool_with_note_added(
            None,
            &state,
            pn_state_to_pool_note(&state).unwrap(),
            1,
            &wallet,
        )
        .unwrap();
        let json = serde_json::to_string(&pool).unwrap();
        for word in phrase.split_whitespace() {
            assert!(!json.contains(word), "pool output contains a seed word");
        }
    }
}
