//! Wallet-address handling (#17). A single parse/normalize for the operator wallet address, so the
//! `half1::half2` display form is not re-parsed at every call site and is rejected fail-loud on bad input.

/// Normalize a wallet address to the contract-parameter form `0:<account>`. Accepts:
/// - the GOSH `half1::half2` display form — **two 64-hex (256-bit) halves**; the account is the **second**
///   half, so `half1::half2` → `0:<half2>` (#17);
/// - an already-canonical `0:<hex>` where the account is **exactly 64 hex chars**.
///
/// Anything else — a short/over-long account (not a valid TVM `0:<account>`), a bare hex without a `0:`
/// prefix, non-hex, missing/extra halves — is a **fail-loud** error (the repo convention). The output is
/// always lowercase `0:<64 hex>`, ready to drop into `dest`/address contract parameters; a malformed address
/// is rejected here at the shared boundary rather than reaching money-path JSON.
pub fn normalize_wallet_address(s: &str) -> Result<String, String> {
    let s = s.trim();
    if let Some((h1, h2)) = s.split_once("::") {
        let (h1, h2) = (h1.trim(), h2.trim());
        if !is_hex64(h1) || !is_hex64(h2) {
            return Err(format!(
                "invalid wallet address `{s}`: expected two 64-hex (256-bit) halves `half1::half2`"
            ));
        }
        return Ok(format!("0:{}", h2.to_ascii_lowercase()));
    }
    if let Some(acct) = s.strip_prefix("0:") {
        if !is_hex64(acct) {
            return Err(format!(
                "invalid wallet address `{s}`: the `0:<hex>` account must be exactly 64 hex chars"
            ));
        }
        return Ok(format!("0:{}", acct.to_ascii_lowercase()));
    }
    Err(format!(
        "invalid wallet address `{s}`: expected `half1::half2` (64-hex halves) or `0:<64 hex>`"
    ))
}

/// Exactly 64 hex chars — a 256-bit TVM account id half / `0:<account>` body.
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h64(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    /// `half1::half2` with full 64-hex halves → `0:<half2>` (the second half), lowercased; `::`-spaces trimmed.
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

    /// Fail loud — including **short forms** (not a 64-hex account is NOT a valid `0:<account>`): bare hex,
    /// non-hex, empty, `0:` without account, `a::b::c`, and wrong-length halves/accounts.
    #[test]
    fn garbage_and_short_forms_fail_loud() {
        let h = h64('a');
        for bad in [
            "",
            "aaaa::bbbb", // short halves (4 hex) — not valid addresses
            "aaaa :: BEEF",
            "0:dead", // short account
            "0:BeEf",
            "0:", // empty account
            "0:nothex",
            "dead",    // bare hex, no prefix
            "xyz",     // non-hex
            "a::b::c", // extra `::`
        ] {
            assert!(
                normalize_wallet_address(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
        // a valid 64-hex partner does not rescue a wrong-length half/account — both must be 64.
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
}
