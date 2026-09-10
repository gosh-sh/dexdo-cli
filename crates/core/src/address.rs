//! Canonical Acki Nacki addresses: `<dapp_id>::<account_id>`.

//! The legacy TVM form `0:<account_id>` names only the workchain and carries no DApp identity. It remains
//! accepted by the shared parser and in files written by older versions, while callers that need complete
//! identity evidence may require the DApp-qualified form.

//! Two directions, deliberately separate:
//! - **out** ([`display`], [`to_canonical`]) - the public form, `<dapp_id>::<account_id>`;
//! - **in** ([`CanonicalAddress::parse`]) - accepts both forms and keeps the parsed halves;
//! - **chain boundary** ([`to_chain_param`]) - yields the account-only workchain form required by chain
//! clients and address-valued contract parameters, as a `String` with nothing else attached;
//! - **chain boundary, DApp kept** (`parse_chain_address` -> `ChainAddress`) - the same account-only
//! chain address for the client, with the DApp id the input named carried beside it.

//! Two entry boundaries mirror each other, so that whatever the client PRINTS it also READS back:
//! - a **file** field arrives through [`serde_canonical`] and friends;
//! - a **command-line argument** arrives through [`arg_to_chain_param`].

//! Both accept either spelling and keep the workchain form in memory, so the rest of the client is
//! unaffected by which spelling the operator or the file used.

//! A supplied DApp id survives on [`CanonicalAddress`] and on `ChainAddress` until a caller explicitly
//! crosses that boundary by taking the chain half alone.

use std::fmt;

/// The shared dexdo system DApp id in canonical 64-hex form.

/// RootPN/PrivateNote/PMP/order-book and RootOracle/Oracle contracts use DApp `4`, as pinned by
/// the committed manifest and the deployed `ROOT_PN_DAPP_ID`/`ORACLE_DAPP_ID` constants.
/// Per-deal TokenContracts do not: they are self-DApp accounts and must be rendered with
/// [`display_self_dapp`]. This constant is the compatibility default only when a legacy
/// `0:<account_id>` carries no DApp of its own.
pub const DEXDO_DAPP_ID: &str = "0000000000000000000000000000000000000000000000000000000000000004";

/// A blockchain address in canonical form: a DApp id plus an account id, both 256-bit.

/// Built by [`CanonicalAddress::parse`], which accepts the canonical `<dapp_id>::<account_id>` and the
/// legacy `0:<account_id>`. Renders canonical through [`fmt::Display`]; [`CanonicalAddress::legacy`]
/// renders the workchain form the chain client and contract parameters still take.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalAddress {
    dapp_id: String,
    account_id: String,
}

impl CanonicalAddress {
    /// Parse either accepted form, fail-loud on anything else.

    /// - `<dapp_id>::<account_id>` - both halves exactly 64 hex chars; the DApp id is kept.
    /// - `0:<account_id>` - the legacy form; the account must be exactly 64 hex chars and the DApp is
    /// taken to be [`DEXDO_DAPP_ID`]. Callers rendering a self-DApp account must instead use the
    /// account-aware [`display_self_dapp`] seam.

    /// Both halves are lowercased. A short/over-long half, a bare hex without a prefix, non-hex, and
    /// extra `::` are rejected rather than being coerced into an address that would move money elsewhere.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some((dapp_id, account_id)) = s.split_once("::") {
            let (dapp_id, account_id) = (dapp_id.trim(), account_id.trim());
            if !is_hex64(dapp_id) || !is_hex64(account_id) {
                return Err(format!(
                    "invalid address `{s}`: canonical `<dapp_id>::<account_id>` takes two 64-hex \
                     (256-bit) halves"
                ));
            }
            return Ok(Self {
                dapp_id: dapp_id.to_ascii_lowercase(),
                account_id: account_id.to_ascii_lowercase(),
            });
        }
        if let Some(account_id) = s.strip_prefix("0:") {
            if !is_hex64(account_id) {
                return Err(format!(
                    "invalid address `{s}`: the legacy `0:<hex>` account must be exactly 64 hex chars"
                ));
            }
            return Ok(Self {
                dapp_id: DEXDO_DAPP_ID.to_string(),
                account_id: account_id.to_ascii_lowercase(),
            });
        }
        Err(format!(
            "invalid address `{s}`: expected canonical `<dapp_id>::<account_id>` (64-hex halves) or \
             legacy `0:<64 hex>`"
        ))
    }

    /// The DApp the account belongs to, 64-hex lowercase.
    pub fn dapp_id(&self) -> &str {
        &self.dapp_id
    }

    /// The account id, 64-hex lowercase, with no prefix.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// The legacy workchain form `0:<account_id>` - what the chain client and contract address
    /// parameters take. The DApp id is not part of it, which is exactly why it is not the public form.
    pub fn legacy(&self) -> String {
        format!("0:{}", self.account_id)
    }
}

impl fmt::Display for CanonicalAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.dapp_id, self.account_id)
    }
}

/// Normalize any accepted form to the canonical public form, fail-loud on anything else.
pub fn to_canonical(s: &str) -> Result<String, String> {
    Ok(CanonicalAddress::parse(s)?.to_string())
}

/// Normalize any accepted form to the workchain form `0:<account_id>` for the chain client and contract
/// parameters, fail-loud on anything else. The canonical form is accepted here so a user may paste it
/// wherever an address is taken.
pub fn to_chain_param(s: &str) -> Result<String, String> {
    Ok(CanonicalAddress::parse(s)?.legacy())
}

/// The command-line boundary: normalize an address a person typed or pasted into the workchain form the
/// rest of the client carries, accepting every spelling the client itself emits.

/// This is the argument-side mirror of [`serde_canonical`], which does exactly this for an address read
/// out of a file. Without it the product's two halves disagree: `note deploy` prints, and its recovery
/// file records, the canonical `<dapp_id>::<account_id>`, and the operator's next move -- pasting that
/// address into `note balance` -- died on the SDK's `Address::parse`, which splits on the FIRST `:` and
/// reads the DApp half as a workchain. The address is the identity of a note holding money, so
/// an operator who cannot round-trip it through our own commands re-types 130 hex characters by hand.

/// It is [`to_chain_param`] and not `parse_chain_address` for the reason `pinned_schema_address`
/// records: the latter is `#[cfg(feature = "net-a")]`, and an argument is parsed in every build.

/// Only a `::`-bearing input is rewritten. `0:<hex>`, `0x<hex>` and bare hex are handed on untouched,
/// because those are the SDK's own accepted spellings and this boundary must not narrow what already
/// works. A `::` input that is not a well-formed canonical address is refused here rather than being
/// passed on to be misdiagnosed downstream.

/// One colon where two belong is called by its name. `<64 hex>:<account_id>` is a DApp id joined with
/// the wrong separator, and the SDK's answer for it -- `unsupported address workchain "<64 hex>"` --
/// names the wrong half of the address as the fault, which is what sent's reporter looking for a
/// workchain that was never in the input. The address the operator meant is shown only when the second
/// half is itself an account id, because a refusal that hands back a form this same boundary would
/// refuse in its turn leaves the operator exactly where it found them; when it is not, both faults are
/// named instead.

/// Both halves are judged and shown TRIMMED, because a pasted address carries whatever whitespace
/// the line it was copied from left around the colon. Whitespace before the colon must not hide the
/// DApp id from this recognition, and whitespace after it must not ride into an advised address,
/// which would then no longer be the one argument the operator is being told to paste.
pub fn arg_to_chain_param(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.contains("::") {
        return to_chain_param(raw);
    }
    if let Some((first, rest)) = raw.split_once(':') {
        let (first, rest) = (first.trim(), rest.trim());
        if is_hex64(first) {
            if !is_hex64(rest) {
                return Err(format!(
                    "invalid address `{raw}`: `{first}` is a DApp id, not a workchain, and `{rest}` is \
                     not a 64-hex account id; the canonical form joins two 64-hex halves with `::`"
                ));
            }
            return Err(format!(
                "invalid address `{raw}`: `{first}` is a DApp id, not a workchain; the canonical form \
                 joins the two halves with `::`, so this address is `{first}::{rest}`"
            ));
        }
    }
    Ok(raw.to_string())
}

/// Render a DApp id as the `uint256` argument the contract ABIs declare it to be.

/// `dapp_id` is a `uint256` input of `UpdateCustodianMultisigWallet_v2.submitTransaction` and of
/// `PrivateNote.withdrawTokens`, and the SDK's tokenizer reads a STRING argument as decimal unless it
/// carries the `0x` prefix. A DApp id is 64 hex characters, so handing it over bare either fails to
/// encode at all - `can not parse number from string`, raised before anything reaches the chain - or,
/// for the DApp id whose characters all happen to be decimal digits, silently encodes a different
/// number. The prefix is what makes the argument name the DApp it was read from.

/// An already-prefixed id passes through unchanged, so this is safe to apply at any boundary.
pub fn to_dapp_id_param(dapp_id: &str) -> String {
    format!("0x{}", dapp_id.trim_start_matches("0x"))
}

/// Render an address for output in the canonical public form.

/// This is the display seam: it upgrades a stored/legacy `0:<account_id>` to `<dapp_id>::<account_id>`
/// and leaves an already-canonical address alone. A value that is not an address at all (a placeholder,
/// a `-`, a name) is passed through trimmed rather than being hidden behind an error - output must never
/// be less informative than the value it has.
pub fn display(s: &str) -> String {
    match CanonicalAddress::parse(s) {
        Ok(addr) => addr.to_string(),
        Err(_) => s.trim().to_string(),
    }
}

/// Render a self-DApp account, whose DApp id is its own account id.

/// A supplied canonical address is authoritative and survives unchanged. For a legacy
/// `0:<account_id>`, the account's protocol-defined self-DApp identity is reconstructed as
/// `<account_id>::<account_id>`. Non-address placeholders remain visible unchanged, matching [`display`].
pub fn display_self_dapp(s: &str) -> String {
    let raw = s.trim();
    match CanonicalAddress::parse(raw) {
        Ok(addr) if raw.contains("::") => addr.to_string(),
        Ok(addr) => format!("{}::{}", addr.account_id(), addr.account_id()),
        Err(_) => raw.to_string(),
    }
}

/// [`display`] over an optional address, with `placeholder` for `None` (the `-` a table column shows).
pub fn display_opt(s: Option<&str>, placeholder: &str) -> String {
    s.map_or_else(|| placeholder.to_string(), display)
}

/// [`display_self_dapp`] over an optional address, with `placeholder` for `None`.
pub fn display_self_dapp_opt(s: Option<&str>, placeholder: &str) -> String {
    s.map_or_else(|| placeholder.to_string(), display_self_dapp)
}

/// An address parsed at the chain boundary: the account-only chain [`Address`](crate::Address) that the
/// chain client and address-valued contract parameters take, together with the DApp id the input named.

/// [`parse_chain_address`] used to return the chain half alone. The SDK's `Address` is a single bare
/// 64-hex account id with derived `PartialEq`/`Hash`, so `<dapp_a>::<account>` and `<dapp_b>::<account>`
/// came back equal and every consumer had to re-derive the DApp from surrounding context. That is not
/// derivable from an account id: a `PrivateNote`, a `RootModel` and the order book live in the shared
/// [`DEXDO_DAPP_ID`], while a per-deal `TokenContract` and an operator multisig are self-DApp accounts
/// whose DApp id **is** their own account id. Carrying the pair is how the SDK itself models an
/// account, and it is what makes two DApps holding one account id stop comparing equal.

/// [`ChainAddress::dapp_id`] is `None` when the input was the legacy `0:<account_id>`, which names no
/// DApp at all. An absent DApp is recorded as absent rather than guessed, so a self-DApp account read
/// from an older file is never re-labelled as a shared-DApp one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainAddress {
    dapp_id: Option<String>,
    chain: crate::Address,
}

impl ChainAddress {
    /// The DApp id the input named, 64-hex lowercase; `None` for a legacy `0:<account_id>`.
    pub fn dapp_id(&self) -> Option<&str> {
        self.dapp_id.as_deref()
    }

    /// The account id, 64-hex lowercase, with no prefix.
    pub fn account_id(&self) -> &str {
        self.chain.bare()
    }

    /// The chain half, borrowed - what the chain client and contract address parameters take.
    pub fn chain(&self) -> &crate::Address {
        &self.chain
    }

    /// The chain half, owned, for a caller that stores or forwards it by value.
    pub fn into_chain(self) -> crate::Address {
        self.chain
    }
}

/// Renders the DApp-qualified `<dapp_id>::<account_id>` when the input named a DApp, and the legacy
/// `0:<account_id>` when it did not. The DApp id a caller supplied survives the round trip in both the
/// shared and the self-DApp case, instead of being replaced by whichever one the renderer assumed.
impl fmt::Display for ChainAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.dapp_id {
            Some(dapp_id) => write!(f, "{dapp_id}::{}", self.account_id()),
            None => write!(f, "0:{}", self.account_id()),
        }
    }
}

/// The chain half is the whole of what this function used to return, so every existing chain call -
/// `get_account(&addr)`, `.with_workchain()`, a contract parameter - keeps its exact previous meaning
/// without a conversion at the call site. The DApp id is deliberately NOT reachable through this seam:
/// it is an addition beside the chain address, never a silent substitute for it.
impl std::ops::Deref for ChainAddress {
    type Target = crate::Address;

    fn deref(&self) -> &Self::Target {
        &self.chain
    }
}

/// Parse any accepted form into a [`ChainAddress`] - the chain address plus the DApp id it named.

/// A strict widening of `Address::parse`: everything the SDK already accepts (`0:<hex>`, `0x<hex>`, bare
/// hex) still parses, and the canonical `<dapp_id>::<account_id>` - which the SDK reads as workchain
/// `<dapp_id>` and rejects - is accepted too. Use this wherever an address arrives from a person or a
/// file (a CLI argument, a manifest, a deal handle) instead of `Address::parse`.
pub fn parse_chain_address(s: &str) -> anyhow::Result<ChainAddress> {
    let s = s.trim();
    if s.contains("::") {
        let canonical = CanonicalAddress::parse(s).map_err(|e| anyhow::anyhow!(e))?;
        return Ok(ChainAddress {
            dapp_id: Some(canonical.dapp_id().to_string()),
            chain: crate::Address::parse(&canonical.legacy())?,
        });
    }
    Ok(ChainAddress {
        dapp_id: None,
        chain: crate::Address::parse(s)?,
    })
}

/// Serde for an address field that is **written** canonically and **read** in either form.

/// Serialization emits `<dapp_id>::<account_id>`, so every newly written file carries the public form.
/// Deserialization accepts a file written either way and yields the workchain form the rest of the
/// client passes to the chain, so an existing `0:<account_id>` file keeps working unchanged. A value
/// that is not an address is carried through untouched, so a hand-written or future manifest never
/// becomes unreadable or unwritable because of this field.
pub mod serde_canonical {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Emit the canonical public form.
    pub fn serialize<S: Serializer>(value: &str, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::display(value))
    }

    /// Accept either form; keep the workchain form in memory.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(super::CanonicalAddress::parse(&raw)
            .map(|addr| addr.legacy())
            .unwrap_or(raw))
    }
}

/// [`serde_canonical`] for an optional address field (`Option<String>`); `null`/absent is untouched.
pub mod serde_canonical_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Emit the canonical public form, or `null`.
    pub fn serialize<S: Serializer>(
        value: &Option<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&super::display(value)),
            None => serializer.serialize_none(),
        }
    }

    /// Accept either form; keep the workchain form in memory.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.map(|raw| {
            super::CanonicalAddress::parse(&raw)
                .map(|addr| addr.legacy())
                .unwrap_or(raw)
        }))
    }
}

/// Serde for a self-DApp address such as a per-deal TokenContract.

/// New writes carry `<account_id>::<account_id>`. Reads remain migration-compatible and keep the
/// account-only chain form in memory, just like [`serde_canonical`].
pub mod serde_self_dapp {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Emit the canonical self-DApp public form.
    pub fn serialize<S: Serializer>(value: &str, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::display_self_dapp(value))
    }

    /// Accept either form; keep the workchain form in memory.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(super::CanonicalAddress::parse(&raw)
            .map(|addr| addr.legacy())
            .unwrap_or(raw))
    }
}

/// [`serde_self_dapp`] for an optional address field; `null`/absent is untouched.
pub mod serde_self_dapp_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Emit the canonical self-DApp public form, or `null`.
    pub fn serialize<S: Serializer>(
        value: &Option<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&super::display_self_dapp(value)),
            None => serializer.serialize_none(),
        }
    }

    /// Accept either form; keep the workchain form in memory.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.map(|raw| {
            super::CanonicalAddress::parse(&raw)
                .map(|addr| addr.legacy())
                .unwrap_or(raw)
        }))
    }
}

/// [`serde_self_dapp`] for a list of TokenContract addresses.
pub mod serde_self_dapp_vec {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// Emit every self-DApp address canonically.
    pub fn serialize<S: Serializer>(values: &[String], serializer: S) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(|value| super::display_self_dapp(value))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    /// Accept either representation and keep account-only chain forms in memory.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<String>, D::Error> {
        Ok(Vec::<String>::deserialize(deserializer)?
            .into_iter()
            .map(|raw| {
                super::CanonicalAddress::parse(&raw)
                    .map(|addr| addr.legacy())
                    .unwrap_or(raw)
            })
            .collect())
    }
}

/// Exactly 64 hex chars - a 256-bit DApp id or account id.
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h64(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    /// The DApp id is protocol-defined, not a guess: it is the `dapp_id` the deployed-contracts manifest
    /// pins, in the 64-hex form the canonical address uses. If the chain moves dexdo to another DApp this
    /// test fails instead of the client quietly printing a wrong identity.
    #[test]
    fn dapp_id_matches_the_deployed_contracts_manifest() {
        let path = crate::params::committed_manifest_for_tests();
        let deployed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read the committed manifest"))
                .expect("the committed manifest is JSON");
        assert_eq!(deployed["dapp_id"].as_str(), Some(DEXDO_DAPP_ID));
        assert!(is_hex64(DEXDO_DAPP_ID), "{DEXDO_DAPP_ID}");
    }

    /// Canonical in, canonical out: parsing then rendering is the identity on a canonical address, and
    /// the supplied DApp id survives it instead of being replaced by the dexdo default.
    #[test]
    fn canonical_round_trips_and_keeps_the_supplied_dapp_id() {
        let dapp = h64('7');
        let account = h64('b');
        let canonical = format!("{dapp}::{account}");

        let addr = CanonicalAddress::parse(&canonical).unwrap();
        assert_eq!(addr.dapp_id(), dapp);
        assert_eq!(addr.account_id(), account);
        assert_eq!(addr.to_string(), canonical);
        assert_eq!(to_canonical(&canonical).unwrap(), canonical);
        assert_eq!(display(&canonical), canonical);
        // Re-parsing our own output is stable.
        assert_eq!(CanonicalAddress::parse(&addr.to_string()).unwrap(), addr);
        // A DApp that is not dexdo's is carried, not rewritten.
        assert_ne!(addr.dapp_id(), DEXDO_DAPP_ID);
    }

    /// Legacy in, canonical out: a `0:<account>` address is upgraded to the dexdo DApp and keeps its
    /// account id, and going back to the chain form returns exactly the input.
    #[test]
    fn legacy_round_trips_through_canonical() {
        let account = h64('a');
        let legacy = format!("0:{account}");
        let canonical = format!("{DEXDO_DAPP_ID}::{account}");

        assert_eq!(to_canonical(&legacy).unwrap(), canonical);
        assert_eq!(display(&legacy), canonical);
        let addr = CanonicalAddress::parse(&legacy).unwrap();
        assert_eq!(addr.dapp_id(), DEXDO_DAPP_ID);
        assert_eq!(addr.account_id(), account);
        assert_eq!(addr.legacy(), legacy);
        // canonical -> chain form -> canonical is lossless for a dexdo-DApp address.
        assert_eq!(to_chain_param(&canonical).unwrap(), legacy);
        assert_eq!(
            to_canonical(&to_chain_param(&canonical).unwrap()).unwrap(),
            canonical
        );
    }

    /// Case and surrounding whitespace are normalized, so the same address never appears twice in two
    /// spellings in output or in a file.
    #[test]
    fn parse_normalizes_case_and_whitespace() {
        let account_upper = format!("ABCD{}", "0".repeat(60));
        let dapp_upper = format!("EF{}", "1".repeat(62));
        let canonical = format!(
            "{}::{}",
            dapp_upper.to_ascii_lowercase(),
            account_upper.to_ascii_lowercase()
        );
        assert_eq!(
            to_canonical(&format!("  {dapp_upper} :: {account_upper}  ")).unwrap(),
            canonical
        );
        assert_eq!(
            to_canonical(&format!("  0:{account_upper} ")).unwrap(),
            format!("{DEXDO_DAPP_ID}::{}", account_upper.to_ascii_lowercase())
        );
    }

    /// Fail-loud on anything that is not one of the two accepted forms - a truncated half, a bare hex, a
    /// non-zero workchain, extra `::`. Coercing any of these would name a different account.
    #[test]
    fn rejects_malformed_addresses() {
        let h = h64('a');
        for bad in [
            "",
            "0:",
            "0:dead",
            "0:nothex",
            "dead",
            &h64('a'),
            "xyz",
            "a::b::c",
            "aaaa::bbbb",
            "1:0000",
        ] {
            assert!(
                CanonicalAddress::parse(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
        assert!(CanonicalAddress::parse(&format!("{h}::beef")).is_err());
        assert!(CanonicalAddress::parse(&format!("beef::{h}")).is_err());
        assert!(CanonicalAddress::parse(&format!("0:{h}ff")).is_err());
        assert!(CanonicalAddress::parse(&format!("{h}::{h}ff")).is_err());
    }

    /// `display` is a rendering helper, not a validator: a placeholder or a name that is not an address
    /// must survive it, or a table column would lose the only value it has.
    #[test]
    fn display_passes_non_addresses_through() {
        assert_eq!(display("-"), "-");
        assert_eq!(display("  none  "), "none");
        assert_eq!(display("qwen--qwen3--32b"), "qwen--qwen3--32b");
        assert_eq!(display_opt(None, "-"), "-");
        assert_eq!(
            display_opt(Some(&format!("0:{}", h64('c'))), "-"),
            format!("{DEXDO_DAPP_ID}::{}", h64('c'))
        );
    }

    /// The serde seam writes canonical and reads both forms, so a file written by an older version keeps
    /// loading and a file written now carries the DApp identity.
    #[test]
    fn serde_writes_canonical_and_reads_both_forms() {
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
        struct Holder {
            #[serde(with = "serde_canonical")]
            address: String,
        }

        let account = h64('d');
        let legacy = format!("0:{account}");
        let canonical = format!("{DEXDO_DAPP_ID}::{account}");

        // Written canonically, whichever form is held in memory.
        let held_legacy = Holder {
            address: legacy.clone(),
        };
        let json = serde_json::to_string(&held_legacy).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["address"],
            serde_json::Value::String(canonical.clone())
        );

        // Both forms read back to the chain form the rest of the client uses.
        for stored in [&legacy, &canonical] {
            let read: Holder =
                serde_json::from_str(&format!("{{\"address\":\"{stored}\"}}")).unwrap();
            assert_eq!(read.address, legacy);
        }

        // A non-address value is neither rejected nor rewritten.
        let odd: Holder = serde_json::from_str("{\"address\":\"pending\"}").unwrap();
        assert_eq!(odd.address, "pending");
        assert_eq!(
            serde_json::to_string(&odd).unwrap(),
            "{\"address\":\"pending\"}"
        );
    }

    /// Issues ** /** -- a stored foreign-dapp address is silently REWRITTEN to dexdo's.

    /// This row deliberately does NOT assert that `to_chain_param` or `parse_chain_address`
    /// preserve the dapp id. Both are documented to yield the workchain form `0:<account_id>`, so
    /// asserting otherwise would be asserting against a conversion the code intends. The consumer
    /// where the collapsed identity DECIDES something is rowed separately, at
    /// `validate_resting_offer` (`crates/dexdo/src/seller/mod.rs`).

    /// What is asserted here is a violation of this module's OWN stated contract, at the top of
    /// this file: *"A supplied DApp id is never discarded: it is carried on [`CanonicalAddress`]
    /// and re-emitted on output."* [`serde_canonical::deserialize`] (`:162-167`) stores `.legacy()`
    /// in memory and `serialize` (`:157-159`) re-emits it through [`display`], which re-attaches
    /// [`DEXDO_DAPP_ID`]. So loading and re-saving a record rewrites a foreign dapp id to dexdo's
    /// -- silently, with no error and no diagnostic. The record afterwards names a different account
    /// from the one that was stored, and every deal handle and pool record goes through this seam.

    /// RED on this head.
    #[test]
    #[ignore = "issues : the serde seam rewrites a stored foreign-dapp address to \
                dexdo's, so a persisted record silently names a different account. RED until the \
                code PR carries the dapp id through. UN-IGNORE there."]
    fn a_stored_foreign_dapp_address_is_not_rewritten_to_ours() {
        let account = h64('9');
        let foreign_dapp = h64('3');
        assert_ne!(
            foreign_dapp, DEXDO_DAPP_ID,
            "the fixture must be a FOREIGN dapp"
        );
        let ours = format!("{DEXDO_DAPP_ID}::{account}");
        let theirs = format!("{foreign_dapp}::{account}");

        // The two are distinct addresses, and `CanonicalAddress` itself agrees.
        assert_ne!(
            CanonicalAddress::parse(&ours).unwrap(),
            CanonicalAddress::parse(&theirs).unwrap()
        );

        // Persistence. A stored foreign-dapp address must come back as itself.
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
        struct Holder {
            #[serde(with = "serde_canonical")]
            address: String,
        }
        let stored = format!("{{\"address\":\"{theirs}\"}}");
        let read: Holder = serde_json::from_str(&stored).unwrap();
        let rewritten = serde_json::to_string(&read).unwrap();
        assert_eq!(
            rewritten, stored,
            "loading and re-saving a foreign-dapp address rewrote it to the dexdo dapp; the \
             record now names a different account than the one that was stored"
        );
    }

    /// The optional variant behaves the same and leaves an absent address absent.
    #[test]
    fn serde_opt_writes_canonical_and_keeps_none() {
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
        struct Holder {
            #[serde(with = "serde_canonical_opt")]
            address: Option<String>,
        }

        let account = h64('f');
        let legacy = format!("0:{account}");
        let canonical = format!("{DEXDO_DAPP_ID}::{account}");

        let some = Holder {
            address: Some(legacy.clone()),
        };
        assert_eq!(
            serde_json::to_string(&some).unwrap(),
            format!("{{\"address\":\"{canonical}\"}}")
        );
        for stored in [&legacy, &canonical] {
            let read: Holder =
                serde_json::from_str(&format!("{{\"address\":\"{stored}\"}}")).unwrap();
            assert_eq!(read.address.as_deref(), Some(legacy.as_str()));
        }

        let none = Holder { address: None };
        assert_eq!(serde_json::to_string(&none).unwrap(), "{\"address\":null}");
        assert_eq!(
            serde_json::from_str::<Holder>("{\"address\":null}").unwrap(),
            none
        );
    }
}

/// Issue **** - the DApp id must survive [`parse_chain_address`], for BOTH address classes.

/// Which DApp an account belongs to is not derivable from its account id, and both classes are real
/// here:
/// - **shared** - a `PrivateNote`, a `RootModel`, the order book: DApp [`DEXDO_DAPP_ID`];
/// - **self-DApp** - a per-deal `TokenContract`, an operator multisig: DApp id **is** its account id.

/// Before this change `parse_chain_address` returned the SDK `Address`, a single bare account id with
/// derived `PartialEq`/`Hash`. Both classes came back byte-identical, and the DApp a caller supplied
/// was replaced by whichever one the next renderer assumed - `display` re-attaches [`DEXDO_DAPP_ID`]
/// to every account, so a self-DApp `TokenContract` was rendered as a shared-DApp account.

/// Every assertion below is written against API that existed BEFORE the change as well, so on the
/// pre-fix code it fails as an assertion rather than as a compile error.
#[cfg(test)]
mod parse_chain_address_keeps_the_dapp {
    use super::{parse_chain_address, DEXDO_DAPP_ID};

    fn h64(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    /// The shared class round-trips: a `PrivateNote` address goes in as `<DEXDO_DAPP_ID>::<account>`
    /// and comes back naming that same DApp.

    /// Separate from the self-DApp case below on purpose: one test asserting both classes stops at
    /// whichever fails first, which would leave the other class unproven.
    #[test]
    fn a_shared_dapp_address_survives_a_round_trip() {
        let note_account = h64('9');
        let shared = format!("{DEXDO_DAPP_ID}::{note_account}");

        assert_eq!(
            parse_chain_address(&shared).unwrap().to_string(),
            shared,
            "a shared-DApp address lost its DApp id crossing the chain boundary"
        );
    }

    /// The self-DApp class round-trips: a per-deal `TokenContract` goes in as `<account>::<account>`
    /// and comes back naming its own account id as its DApp, not the shared dexdo one.
    #[test]
    fn a_self_dapp_address_survives_a_round_trip() {
        let token_contract_account = h64('c');
        let self_dapp = format!("{token_contract_account}::{token_contract_account}");

        assert_eq!(
            parse_chain_address(&self_dapp).unwrap().to_string(),
            self_dapp,
            "a self-DApp address came back naming a different DApp than the one it was given"
        );
    }

    /// Two DApps holding one account id are two different accounts, and must not compare equal.

    /// This is the comparison the defect turns into a false match: the SDK `Address` derives equality
    /// over the account id alone, so before this change these two were the same value.
    #[test]
    fn a_foreign_dapp_address_does_not_compare_equal_to_ours() {
        let account = h64('9');
        let foreign_dapp = h64('3');
        assert_ne!(
            foreign_dapp, DEXDO_DAPP_ID,
            "the fixture must name a FOREIGN dapp"
        );

        let ours = parse_chain_address(&format!("{DEXDO_DAPP_ID}::{account}")).unwrap();
        let theirs = parse_chain_address(&format!("{foreign_dapp}::{account}")).unwrap();

        assert_ne!(
            ours, theirs,
            "an address in a foreign DApp compared equal to ours; they are different accounts, \
             holding different money, and only the DApp half distinguishes them"
        );
    }

    /// The chain half is exactly what it was, so this fix moves no chain call.

    /// Every accepted input form still yields the account-only workchain address the chain client and
    /// address-valued contract parameters take, and a legacy `0:<account_id>` - which names no DApp -
    /// is not handed one it never carried.
    #[test]
    fn the_chain_half_and_the_legacy_form_are_unchanged() {
        let account = h64('9');
        let legacy = format!("0:{account}");

        for form in [
            legacy.clone(),
            account.clone(),
            format!("0x{account}"),
            format!("{DEXDO_DAPP_ID}::{account}"),
            format!("{account}::{account}"),
        ] {
            assert_eq!(
                parse_chain_address(&form).unwrap().with_workchain(),
                legacy,
                "chain half changed for input `{form}`"
            );
        }

        assert_eq!(
            parse_chain_address(&legacy).unwrap().to_string(),
            legacy,
            "a legacy address names no DApp and must not be given one"
        );
    }

    /// The retained DApp is readable, and an absent one reads as absent rather than as dexdo's.

    /// This is the only test here that names API introduced by the fix, so unlike the three above it
    /// cannot be run against the pre-fix code; it covers the accessors rather than the defect.
    #[test]
    fn the_retained_dapp_id_is_readable_and_absent_when_unstated() {
        let account = h64('9');
        let foreign_dapp = h64('3');

        let shared = parse_chain_address(&format!("{DEXDO_DAPP_ID}::{account}")).unwrap();
        assert_eq!(shared.dapp_id(), Some(DEXDO_DAPP_ID));
        assert_eq!(shared.account_id(), account);

        let self_dapp = parse_chain_address(&format!("{account}::{account}")).unwrap();
        assert_eq!(self_dapp.dapp_id(), Some(account.as_str()));

        let foreign = parse_chain_address(&format!("{foreign_dapp}::{account}")).unwrap();
        assert_eq!(foreign.dapp_id(), Some(foreign_dapp.as_str()));

        // A legacy address states no DApp. Reporting `None` is what stops a self-DApp account read
        // from an older file being re-labelled with the shared dexdo DApp.
        let legacy = parse_chain_address(&format!("0:{account}")).unwrap();
        assert_eq!(legacy.dapp_id(), None);
        assert_eq!(legacy.account_id(), account);

        // The chain half stays reachable both borrowed and owned.
        assert_eq!(shared.chain().bare(), account);
        assert_eq!(shared.into_chain().bare(), account);
    }
}

/// The argument boundary's own refusals. The round trip that proves the boundary works at
/// all is `crates/dexdo/tests/canonical_address_round_trip_1422.rs`, which feeds each command the
/// string another command printed; what is checked here is only what the boundary SAYS when it says
/// no, because no printed address produces a malformed input to round-trip.
#[cfg(test)]
mod arg_boundary_refusal_tests {
    use super::{arg_to_chain_param, DEXDO_DAPP_ID};

    const ACCOUNT: &str = "851a3cb6388e1bf815898fc5977743ce31b004139fe546dafe0f6af5d837fa76";

    /// One colon where two belong is a DApp id joined with the wrong separator. The SDK answers it
    /// with `unsupported address workchain "<dapp id>"`, which names the wrong half of the address
    /// as the fault; the reporter of went looking for a workchain that was never in the input.
    #[test]
    fn a_dapp_id_joined_with_one_colon_is_named_a_dapp_id() {
        let error = arg_to_chain_param(&format!("{DEXDO_DAPP_ID}:{ACCOUNT}"))
            .expect_err("one colon is not an address");
        assert!(
            error.contains("is a DApp id, not a workchain"),
            "the refusal does not say what is actually wrong: {error}"
        );
        assert!(
            error.contains(&format!("{DEXDO_DAPP_ID}::{ACCOUNT}")),
            "the refusal does not show the address the operator meant: {error}"
        );
        assert!(
            !error.contains("workchain \""),
            "the refusal still reads the DApp half as a workchain: {error}"
        );
    }

    /// A real workchain is still called a workchain: this boundary must not relabel every prefix.
    #[test]
    fn a_short_prefix_is_left_to_the_sdk_to_judge() {
        assert_eq!(
            arg_to_chain_param(&format!("1:{ACCOUNT}")).expect("passed through untouched"),
            format!("1:{ACCOUNT}"),
            "a workchain-shaped prefix must reach the SDK, which is the authority on workchains"
        );
    }

    /// Everything the SDK already accepts still arrives at the SDK unchanged. Narrowing the set of
    /// spellings that work would trade one refused paste for another.
    #[test]
    fn the_sdk_spellings_are_not_narrowed() {
        for spelling in [
            format!("0:{ACCOUNT}"),
            format!("0x{ACCOUNT}"),
            ACCOUNT.to_string(),
        ] {
            assert_eq!(
                arg_to_chain_param(&spelling).expect("an SDK spelling"),
                spelling,
                "the argument boundary rewrote a spelling the SDK already parses"
            );
        }
    }

    /// The address the refusal shows is one this same boundary accepts. A refusal that answers a
    /// paste with a form the next command refuses in its turn has not told the operator what to do.
    #[test]
    fn the_address_the_refusal_shows_is_accepted_by_this_boundary() {
        let error = arg_to_chain_param(&format!("{DEXDO_DAPP_ID}:{ACCOUNT}"))
            .expect_err("one colon is not an address");
        let shown = error
            .rsplit('`')
            .nth(1)
            .expect("the refusal shows the address the operator meant");
        assert_eq!(shown, format!("{DEXDO_DAPP_ID}::{ACCOUNT}"));
        assert_eq!(
            arg_to_chain_param(shown)
                .expect("the address the refusal shows is refused in its turn"),
            format!("0:{ACCOUNT}"),
            "the refusal advised an address this same boundary does not accept"
        );
    }

    /// With a second half that is no account id, joining it with `::` would only earn the next
    /// refusal, so none is advised and both faults are named in the one answer.
    #[test]
    fn a_second_half_that_is_no_account_is_named_instead_of_advised() {
        let error = arg_to_chain_param(&format!("{DEXDO_DAPP_ID}:abc"))
            .expect_err("neither half is an address");
        assert!(
            error.contains("is a DApp id, not a workchain"),
            "the refusal no longer names the first half: {error}"
        );
        assert!(
            error.contains("`abc` is not a 64-hex account id"),
            "the refusal does not name the second half, which is the half that is also wrong: {error}"
        );
        assert!(
            arg_to_chain_param(&format!("{DEXDO_DAPP_ID}::abc")).is_err(),
            "the joined form is accepted after all, so withholding it would be the wrong answer"
        );
        assert!(
            !error.contains(&format!("{DEXDO_DAPP_ID}::")),
            "the refusal still advises an address this same boundary refuses: {error}"
        );
    }

    /// A malformed canonical address is refused here rather than passed on to be misdiagnosed.
    #[test]
    fn a_malformed_canonical_address_is_refused_at_the_boundary() {
        let error = arg_to_chain_param(&format!("{DEXDO_DAPP_ID}::not-hex"))
            .expect_err("a short account half is not an address");
        assert!(
            error.contains("two 64-hex"),
            "the refusal does not name the malformed half: {error}"
        );
    }
}
