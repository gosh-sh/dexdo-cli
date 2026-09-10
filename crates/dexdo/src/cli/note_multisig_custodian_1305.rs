//! the note path's custodian check and the shared normalization are one rule, not two.

//! `normalize_multisig_pubkey` was defined twice -- once in `dexdo-core`, once here -- and the two
//! copies decided the same money-path question: whether the key dexdo holds is a custodian of the
//! wallet it is about to spend from. They agreed, so no runtime test could ever have separated
//! them; that is exactly why the fix is an import (a second definition is now `error[E0255]`)
//! rather than an assertion.

//! What a test CAN hold is the other half: that this path's answer is the shared function's answer,
//! on the renderings that made two spellings of one key look like two keys in the first place. The
//! cases below drive the real entry points -- `multisig_custodian_pubkeys`, which reads
//! `getCustodians` output, and `ensure_multisig_private_key_is_custodian`, which decides -- against
//! `dexdo_core::normalize_multisig_pubkey` as the oracle. A future copy that drifts on `0X`, on a
//! short key, or on case fails here.

use super::{ensure_multisig_private_key_is_custodian, multisig_custodian_pubkeys};

/// The wallet the refusals name. Its spelling does not matter to any case below.
const FUNDING_WALLET: &str = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// One key, in the comparable form both sides are supposed to reach.
const KEY: &str = "00000000000000000000000000000000000000000000000000000000cafebabe";

/// Every spelling of `KEY` a getter or a local derivation can hand over.

/// `getCustodians` renders `owner_pubkey` as an unsigned 256-bit integer, so a key with leading
/// zero bytes comes back short and `0x`-prefixed; a locally derived one is bare and already 64
/// characters. Both are the same key.
const RENDERINGS: [&str; 8] = [
    KEY,
    "0xcafebabe",
    "0Xcafebabe",
    "0XCAFEBABE",
    "cafebabe",
    "  cafebabe  ",
    "0x00000000000000000000000000000000000000000000000000000000CAFEBABE",
    "\t0X00000000000000000000000000000000000000000000000000000000cafebabe\n",
];

fn custodians(renderings: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "custodians": renderings
            .iter()
            .enumerate()
            .map(|(index, pubkey)| serde_json::json!({
                "index": index.to_string(),
                "owner_pubkey": pubkey,
            }))
            .collect::<Vec<_>>(),
    })
}

/// The reader agrees with the shared function on every rendering, one at a time so a failure names
/// the spelling that drifted rather than a whole vector.
#[test]
fn the_getter_reader_normalizes_through_the_shared_function() {
    for rendering in RENDERINGS {
        let expected = dexdo_core::normalize_multisig_pubkey(rendering)
            .expect("every rendering here is a public key");
        assert_eq!(
            expected, KEY,
            "the corpus itself is wrong if {rendering} is not {KEY}"
        );
        assert_eq!(
            multisig_custodian_pubkeys(&custodians(&[rendering])),
            vec![expected],
            "the note path read {rendering} differently from dexdo_core::normalize_multisig_pubkey"
        );
    }
}

/// The decision the duplication sat on: a key that IS a custodian is accepted however the getter
/// spelled it. A copy that dropped `0X`, or stopped left-padding, would accept one rendering and
/// refuse another -- the same wallet, two answers.
#[test]
fn a_custodian_is_recognized_in_every_rendering() {
    for rendering in RENDERINGS {
        for derived in RENDERINGS {
            ensure_multisig_private_key_is_custodian(
                FUNDING_WALLET,
                derived,
                &custodians(&[rendering]),
            )
            .unwrap_or_else(|error| {
                panic!("custodian {rendering} must accept the same key spelled {derived}: {error}")
            });
        }
    }
}

/// The other direction, which matters more: a key that is NOT a custodian stays refused. A
/// normalization that padded or truncated its way to a match would bind a wallet dexdo cannot sign
/// for, so the wide corpus above must not have widened this.
#[test]
fn a_stranger_key_is_still_refused_in_every_rendering() {
    let stranger = "00000000000000000000000000000000000000000000000000000000deadbeef";
    for rendering in RENDERINGS {
        let error = ensure_multisig_private_key_is_custodian(
            FUNDING_WALLET,
            stranger,
            &custodians(&[rendering]),
        )
        .expect_err("a key that owns nothing must be refused")
        .to_string();
        assert!(
            error.contains("is not a custodian"),
            "unexpected refusal for {rendering}: {error}"
        );
        assert!(
            error.contains("no wallet message was submitted"),
            "the refusal must say nothing was submitted for {rendering}: {error}"
        );
    }
}

/// A rendering that is not a public key is dropped, never padded into one: a getter that returns
/// garbage must reduce the custodian set, not manufacture a member of it.
#[test]
fn renderings_that_are_not_public_keys_are_dropped_rather_than_padded() {
    let not_keys = ["", "0x", "   ", "nothex", "0x?", &"a".repeat(65)];
    for not_a_key in not_keys {
        assert_eq!(
            dexdo_core::normalize_multisig_pubkey(not_a_key),
            None,
            "the corpus itself is wrong if {not_a_key:?} is a public key"
        );
        assert!(
            multisig_custodian_pubkeys(&custodians(&[not_a_key])).is_empty(),
            "the note path turned {not_a_key:?} into a custodian"
        );
    }
    // Mixed with a real one, the real one survives and the garbage does not become a second entry.
    assert_eq!(
        multisig_custodian_pubkeys(&custodians(&["nothex", "0xcafebabe", ""])),
        vec![KEY.to_string()],
        "garbage must reduce the custodian set, not extend it"
    );
}
