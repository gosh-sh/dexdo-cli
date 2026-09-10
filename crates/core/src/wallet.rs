//! Wallet-address handling. A single parse/normalize for the operator wallet address, so the
//! canonical display form is not re-parsed at every call site and is rejected fail-loud on bad input.

//! The two accepted forms and the parsing itself live in [`crate::address`]; this stays the
//! wallet-flavoured entry point that yields the contract-parameter form.

use crate::address::CanonicalAddress;

/// Normalize a wallet address to the contract-parameter form `0:<account>`. Accepts:
/// - the canonical `<dapp_id>::<account_id>` display form -- **two 64-hex (256-bit) halves**; the account
/// is the **second** half, so `<dapp_id>::<account_id>` -> `0:<account_id>`. The DApp id is not
/// part of a contract address parameter and is dropped **here only**; output that a user reads goes
/// through [`crate::address::display`], which puts it back;
/// - the legacy `0:<hex>` where the account is **exactly 64 hex chars**.

/// Anything else -- a short/over-long account (not a valid TVM `0:<account>`), a bare hex without a `0:`
/// prefix, non-hex, missing/extra halves -- is a **fail-loud** error (the repo convention). The output is
/// always lowercase `0:<64 hex>`, ready to drop into `dest`/address contract parameters; a malformed address
/// is rejected here at the shared boundary rather than reaching money-path JSON.
pub fn normalize_wallet_address(s: &str) -> Result<String, String> {
    CanonicalAddress::parse(s)
        .map(|addr| addr.legacy())
        .map_err(|e| e.replacen("invalid address", "invalid wallet address", 1))
}

/// Normalize a multisig custodian public key to the comparable 64-hex lowercase form.

/// `getCustodians` renders `owner_pubkey` as an unsigned 256-bit integer, so a key whose leading
/// bytes are zero comes back short and with an `0x` prefix, while a key derived locally is bare and
/// already 64 chars. Comparing the two textually without this is how a custodian that IS ours reads
/// as "not a custodian". The answer decides whether a wallet we cannot sign for gets recorded.

/// `None` = not a public key at all (empty, over-long, non-hex), which callers must treat as a
/// mismatch rather than coerce.
pub fn normalize_multisig_pubkey(pubkey: &str) -> Option<String> {
    let pubkey = pubkey.trim();
    let pubkey = pubkey
        .strip_prefix("0x")
        .or_else(|| pubkey.strip_prefix("0X"))
        .unwrap_or(pubkey);
    if pubkey.is_empty()
        || pubkey.len() > 64
        || !pubkey.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(format!("{pubkey:0>64}").to_ascii_lowercase())
}

#[cfg(test)]
mod multisig_pubkey_tests {
    use super::normalize_multisig_pubkey;

    /// The three renderings of one key -- bare, `0x`-prefixed, and left-truncated by the getter's
    /// integer rendering -- must all compare equal.
    #[test]
    fn one_key_normalizes_to_one_form() {
        let expected = format!("{}cafe", "0".repeat(60));
        for rendering in [
            "0xcafe",
            "0XCAFE",
            "cafe",
            "  cafe  ",
            &expected.to_uppercase(),
        ] {
            assert_eq!(
                normalize_multisig_pubkey(rendering).as_deref(),
                Some(expected.as_str()),
                "rendering {rendering} must normalize to the comparable form"
            );
        }
    }

    /// Anything that is not a public key stays `None`: a mismatch must never be manufactured by
    /// padding garbage out to 64 chars.
    #[test]
    fn non_keys_are_refused_rather_than_padded() {
        for bad in ["", "0x", "  ", "nothex", &"a".repeat(65), "0x?"] {
            assert_eq!(
                normalize_multisig_pubkey(bad),
                None,
                "{bad} is not a public key"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h64(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    /// `<dapp_id>::<account_id>` with full 64-hex halves -> `0:<account_id>` (the second half), lowercased;
    /// `::`-spaces trimmed.
    #[test]
    fn half1_half2_takes_second_half_lowercased() {
        let h1 = h64('1');
        let h2_up = format!("ABCD{}", "0".repeat(60)); // 64, mixed case
        let h2_lo = h2_up.to_ascii_lowercase();
        assert_eq!(
            normalize_wallet_address(&format!("{h1}::{h2_up}")).unwrap(),
            format!("0:{h2_lo}")
        );
        assert_eq!(
            normalize_wallet_address(&format!("  {h1} :: {h2_up}  ")).unwrap(),
            format!("0:{h2_lo}")
        );
    }

    /// A canonical `0:<64 hex>` passes through, lowercased.
    #[test]
    fn passes_canonical_64hex_form() {
        let acct_up = format!("DEAD{}", "0".repeat(60)); // 64
        assert_eq!(
            normalize_wallet_address(&format!("0:{acct_up}")).unwrap(),
            format!("0:{}", acct_up.to_ascii_lowercase())
        );
        assert_eq!(
            normalize_wallet_address(&format!("  0:{}  ", h64('a'))).unwrap(),
            format!("0:{}", h64('a'))
        );
    }

    /// Fail loud -- including **short forms** (not a 64-hex account is NOT a valid `0:<account>`): bare hex,
    /// non-hex, empty, `0:` without account, `a::b::c`, and wrong-length halves/accounts.
    #[test]
    fn garbage_and_short_forms_fail_loud() {
        let h = h64('a');
        for bad in [
            "",
            "aaaa::bbbb", // short halves (4 hex) -- not valid addresses
            "aaaa :: BEEF",
            "0:dead", // short account
            "0:BeEf",
            "0:", // empty account
            "0:nothex",
            "dead", // bare hex, no prefix
            "xyz", // non-hex
            "a::b::c", // extra `::`
        ] {
            assert!(
                normalize_wallet_address(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
        // a valid 64-hex partner does not rescue a wrong-length half/account -- both must be 64.
        assert!(
            normalize_wallet_address(&format!("{h}::beef")).is_err(),
            "short half2"
        );
        assert!(
            normalize_wallet_address(&format!("beef::{h}")).is_err(),
            "short half1"
        );
        assert!(
            normalize_wallet_address(&format!("0:{h}ff")).is_err(),
            "66-hex account"
        );
    }

    /// A canonical `<dapp_id>::<account_id>` is accepted wherever a wallet address is, and the DApp id it
    /// carries is not lost: dropping it is a contract-parameter detail, and the display seam restores it.
    #[test]
    fn canonical_address_is_accepted_and_its_dapp_id_survives_display() {
        let dapp = h64('9');
        let account = h64('e');
        let canonical = format!("{dapp}::{account}");
        assert_eq!(
            normalize_wallet_address(&canonical).unwrap(),
            format!("0:{account}")
        );
        assert_eq!(crate::address::display(&canonical), canonical);
        assert_eq!(
            crate::address::CanonicalAddress::parse(&canonical)
                .unwrap()
                .dapp_id(),
            dapp
        );
    }
}
