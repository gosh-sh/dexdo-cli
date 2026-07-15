use anyhow::{anyhow, Result};
use base64::Engine as _;
use gosh_ackinacki::sdk::{Address, KeyPair};
use tvm_block::{Deserializable, Serializable, StateInit};

pub(super) const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
/// #70 gas-health floor for active RootModel/TokenContract pokes. Balances are native vmshell
/// nanotokens (`Account.balance`), after `fundDeployShell` flag:16 converts note SHELL/ECC[2].
pub(super) const GAS_HEALTH_MIN: u128 = 5_000_000_000;
/// Top active contracts back up to the accepted right-sized deploy level when they reach the floor.
pub(super) const GAS_HEALTH_TARGET: u128 = 10_000_000_000;
pub(super) fn gas_health_top_up_amount(balance: u128, min: u128, target: u128) -> Option<u128> {
    debug_assert!(target >= min);
    if balance <= min {
        let amount = target.saturating_sub(balance);
        (amount > 0).then_some(amount)
    } else {
        None
    }
}

/// ABI of the deployed contracts (`contracts/compiled_0.79.3`), embedded for on-chain getters.
pub(super) const SUPERROOT_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/airegistry/SuperRoot.abi.json");
pub(super) const ROOTPN_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/dex/RootPN.abi.json");
pub(super) const ROOTORACLE_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/dex/RootOracle.abi.json");
pub(super) const ORACLE_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/dex/Oracle.abi.json");
pub(super) const ORACLEEVENTLIST_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/dex/OracleEventList.abi.json");
pub(super) const PMP_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/dex/PMP.abi.json");
pub(super) const ROOTMODEL_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/airegistry/RootModel.abi.json");
pub(super) const TOKENCONTRACT_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/airegistry/TokenContract.abi.json");
/// `PrivateNote` (zk-note) — ABI of the deal's owner methods (`deployInferenceOrderBook`,
/// `postSellOffer`, `placeInferenceBuy`, `streamStop`, getter `getInferenceOrderBookAddress`).
/// Minted via RootPN (gosh-dexdo `mint_pn_pool`); the signatures of these 5 methods match the
/// deployed code byte-for-byte — the note accepts our calls (see the live test).
pub(super) const PRIVATENOTE_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/dex/PrivateNote.abi.json");
/// `PrivateNote` StateInit (`.tvc`) — for the diagnostic comparison of the embedded image's code-hash
/// against the `Account.code_hash` value of the minted note (live cross-check of the inference variant; test-only).
pub(super) const PRIVATENOTE_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/dex/PrivateNote.tvc");
pub(super) const SUPERROOT_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/SuperRoot.tvc");
/// Keep the standard-sold v2 image available for offline diagnostics; it is not the live
/// shellnet RootPN self-hash expectation.
#[allow(dead_code)]
pub(super) const ROOTPN_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/dex/RootPN.tvc");
/// Shellnet RootPN is compiled with the old halo2-sold (v1 ext-out) so the voucher prover
/// (tvm_block 2.24.20) can parse VoucherGenerated; its code hash is 44faea57 (v1), NOT the
/// standard-sold v2 23c8fc1b. RootPN LOGIC is identical 4.0.27.
pub(super) const SHELLNET_ROOTPN_V1_CODE_HASH: &str =
    "44faea57e048ec3eec9a570a91dc9592b25d2d9021a173ca8870ba82fca8b3f6";
pub(super) const ROOTORACLE_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/dex/RootOracle.tvc");
/// `TokenContract` StateInit (`.tvc`) — deployed via `build_deploy` (step 2: the seller provisions
/// the per-deal TC). Its code-hash == the `RootModel.TOKEN_CONTRACT_CODE_HASH` pin (offline guard), so
/// the derived address matches `RootModel.getTokenContractAddress` and registration is accepted.
pub(super) const TOKENCONTRACT_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/TokenContract.tvc");
/// `RootModel` StateInit (`.tvc`) — the seller (model owner) deploys their own RootModel under
/// SuperRoot themselves (self-register: the ctor calls `SuperRoot.registerRoot`). Its code-hash == the
/// `SuperRoot.ROOT_MODEL_CODE_HASH` pin (offline guard), otherwise SuperRoot rejects the registration.
pub(super) const ROOTMODEL_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/RootModel.tvc");
/// The `TOKEN_CONTRACT_CODE_HASH` pin from `contracts/airegistry/RootModel.sol` — against it RootModel
/// checks the TC code when registering a deal. The embedded `TokenContract.tvc` must yield this hash.
pub(super) const ROOTMODEL_PINNED_TC_CODE_HASH: &str =
    "029dbf013ebcb356be93f5c1f594833014c4191445a021f1c773ca364249c240";
/// The `ROOT_MODEL_CODE_HASH` pin from `contracts/airegistry/SuperRoot.sol` — against it SuperRoot
/// checks the RootModel code at `registerRoot`. The embedded `RootModel.tvc` must yield this hash.
pub(super) const SUPERROOT_PINNED_RM_CODE_HASH: &str =
    "b88723892c07ad3b12eee921b98c3b79d1b853b8af41749474f3536ec1060de8";
/// The deployed `PrivateNote` code-hash (`deployed.shellnet.json` `_note`). The #83 orphaned-note
/// guard (`assert_seller_note_current`) requires the seller note's on-chain `code_hash` to equal this; the
/// `private_note_code_hash_matches_deployed_pin` test cross-checks it against the embedded `PRIVATENOTE_TVC`
/// (test-only). Update on every PrivateNote redeploy (same cadence as `deployed.shellnet.json`).
pub(super) const PRIVATENOTE_PINNED_CODE_HASH: &str =
    "2894e9c978a9554f22ddd6d502d8580d847f67acf1af3fc4bae2496deb117c24";

/// #117 (pure, offline-testable): the seller note must carry the CURRENT pinned `PrivateNote` code. A note
/// minted before a contract redeploy is orphaned (stale code_hash) and its owner-methods (`confirmDeal`,
/// provision) throw a raw `TVM_ERROR`; reject fail-closed with an actionable re-mint message instead. The async
/// [`RealChainBackend::assert_seller_note_current`] wraps this with the on-chain existence + Active checks.
pub(super) fn note_code_hash_current(note: &Address, code_hash: Option<&str>) -> Result<()> {
    match code_hash {
        Some(h) if h == PRIVATENOTE_PINNED_CODE_HASH => Ok(()),
        other => Err(anyhow!(
            "seller note {note} code_hash {} != the current PrivateNote code {PRIVATENOTE_PINNED_CODE_HASH} \
             — the pn_pool predates a contract redeploy (orphaned). Re-mint against the current contracts \
             (`mint_pn_pool`) and point DEXDO_PN_POOL at the fresh pool.",
            other.unwrap_or("<none>")
        )),
    }
}

/// Fund-safety guard for `note withdraw` (public dexdo-cli#37): pure code-hash generation check.
/// A note whose on-chain `code_hash` is not the current `PRIVATENOTE_PINNED_CODE_HASH` was deployed
/// by a previous contract generation; the current-generation `withdrawTokens` zeroes it without
/// crediting the destination, so the SHELL is lost. Refuse before any on-chain write.
pub(super) fn note_withdraw_generation_ok(note: &Address, code_hash: Option<&str>) -> Result<()> {
    match code_hash {
        Some(h) if h == PRIVATENOTE_PINNED_CODE_HASH => Ok(()),
        other => Err(anyhow!(
            "REFUSING to withdraw from note {note}: it was deployed by a PREVIOUS contract generation \
             (code_hash {}, current is {PRIVATENOTE_PINNED_CODE_HASH}). Withdrawing from a \
             previous-generation note with this CLI zeroes the note WITHOUT crediting the destination \
             — the SHELL is lost (dexdo-cli#37). This CLI will not submit the withdraw.",
            other.unwrap_or("<none>")
        )),
    }
}

#[cfg(test)]
mod withdraw_generation_tests {
    use super::*;

    fn any_note() -> Address {
        Address::parse(&format!("0:{}", "1".repeat(64))).unwrap()
    }

    #[test]
    fn withdraw_allows_current_generation_note() {
        assert!(
            note_withdraw_generation_ok(&any_note(), Some(PRIVATENOTE_PINNED_CODE_HASH)).is_ok()
        );
    }

    #[test]
    fn withdraw_refuses_previous_generation_note() {
        // The two previous-generation hashes from dexdo-cli#37 that zeroed notes without crediting.
        for stale in [
            "210add370000000000000000000000000000000000000000000000000000000a",
            "76acd39200000000000000000000000000000000000000000000000000000007",
        ] {
            let err = note_withdraw_generation_ok(&any_note(), Some(stale))
                .unwrap_err()
                .to_string();
            assert!(err.contains("REFUSING to withdraw"), "message: {err}");
            assert!(err.contains(stale), "must name the stale hash: {err}");
            assert!(
                err.contains(PRIVATENOTE_PINNED_CODE_HASH),
                "must name the current hash: {err}"
            );
        }
    }

    #[test]
    fn withdraw_refuses_note_with_no_code_hash() {
        assert!(note_withdraw_generation_ok(&any_note(), None).is_err());
    }
}

/// `InferenceOrderBook` — ABI of the on-chain offer/order book (per-model, §3.1.2).
pub(super) const INFERENCE_ORDERBOOK_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/airegistry/InferenceOrderBook.abi.json");
/// `InferenceOrderBook` StateInit (`.tvc`) — the **code-cell** is extracted from it, which the note
/// passes to `deployInferenceOrderBook(code, …)` (the book address is deterministic from code+params).
pub(super) const INFERENCE_ORDERBOOK_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/InferenceOrderBook.tvc");
pub(super) const ROOTPN_ADDR: &str =
    "0:1010101010101010101010101010101010101010101010101010101010101010";
pub(super) const ROOTORACLE_ADDR: &str =
    "0:1515151515151515151515151515151515151515151515151515151515151515";
/// Decode a hex string (TVM ABI `bytes` output) without an external dependency.
pub(super) fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(anyhow!("odd hex length"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

/// Derive a note's ed25519 public key (`[u8; 32]`) from its owner [`KeyPair`] — the same derivation
/// `RealNote::pubkey` uses (`KeyPair::public_hex` → bytes). Used by `dexdo recover` (#85) to verify the
/// recover note is the deal's recorded buyer (`getBuyerPubkey`) before signing STOP.
pub fn keypair_ed_pubkey(keys: &KeyPair) -> Result<[u8; 32]> {
    let bytes = decode_hex(keys.public_hex().trim_start_matches("0x"))?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "ed25519 public key: expected 32 bytes, got {}",
            bytes.len()
        ));
    }
    let mut ed = [0u8; 32];
    ed.copy_from_slice(&bytes);
    Ok(ed)
}

/// Encode bytes to hex (a TVM ABI `bytes` argument, e.g. `endpointCipher`; and code-hash comparison).
/// `write!` directly into the buffer (review #7) — without allocating a `String` per byte.
pub(super) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Extract the **code-cell** from a `.tvc` (StateInit BOC) — the same logic as
/// `airegistry::abi::Contract::code_cell` in the SDK: `read_single_root_boc` → `StateInit` → `.code`.
pub(super) fn code_cell(tvc: &[u8]) -> Result<tvm_types::Cell> {
    let cell = tvm_types::read_single_root_boc(tvc).map_err(|e| anyhow!("read tvc BOC: {e}"))?;
    let state_init =
        StateInit::construct_from_cell(cell).map_err(|e| anyhow!("parse StateInit: {e}"))?;
    state_init
        .code
        .ok_or_else(|| anyhow!("no code-cell in StateInit"))
}

/// The `.tvc` code-cell as base64-BOC — the encoding of a `cell` argument in TVM ABI (`call`/`run_getter`).
pub(super) fn code_boc_b64(tvc: &[u8]) -> Result<String> {
    let boc = tvm_types::write_boc(&code_cell(tvc)?).map_err(|e| anyhow!("write code BOC: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(boc))
}

/// Hex `tvm.hash` of a `.tvc` code-cell — compared against `Account.code_hash` by `dexdo doctor`.
pub(super) fn code_hash(tvc: &[u8]) -> Result<String> {
    Ok(encode_hex(code_cell(tvc)?.repr_hash().as_slice()))
}

/// Derive the canonical per-model order-book address from the pinned TVC and model hash.
pub(super) fn inference_orderbook_address_from_model_hash(model_hash: &str) -> Result<Address> {
    let hash = model_hash
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if hash.len() != 64 || !hash.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(anyhow!("model hash must be exactly 32 bytes of hex"));
    }
    let root = tvm_types::read_single_root_boc(INFERENCE_ORDERBOOK_TVC)
        .map_err(|error| anyhow!("read InferenceOrderBook TVC: {error}"))?;
    let mut state_init = StateInit::construct_from_cell(root)
        .map_err(|error| anyhow!("parse InferenceOrderBook StateInit: {error}"))?;
    let fields = serde_json::json!({
        "_pubkey": "0x0",
        "_modelHash": format!("0x{hash}"),
    });
    let data = tvm_abi::json_abi::encode_storage_fields(
        INFERENCE_ORDERBOOK_ABI,
        Some(&fields.to_string()),
    )
    .map_err(|error| anyhow!("encode InferenceOrderBook static fields: {error}"))?
    .into_cell()
    .map_err(|error| anyhow!("build InferenceOrderBook data cell: {error}"))?;
    state_init.data = Some(data);
    let state_init = state_init
        .serialize()
        .map_err(|error| anyhow!("serialize InferenceOrderBook StateInit: {error}"))?;
    Address::parse(&format!(
        "0:{}",
        encode_hex(state_init.repr_hash().as_slice())
    ))
}
