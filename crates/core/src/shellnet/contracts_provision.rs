use anyhow::{anyhow, Result};
use base64::Engine as _;
use gosh_ackinacki::sdk::{Address, KeyPair};
use tvm_block::{Deserializable, StateInit};

pub(super) const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
/// gas-health floor for active RootModel/TokenContract pokes. Balances are native vmshell
/// nanotokens(`Account.balance`), after `fundDeployShell` flag:16 converts note SHELL/ECC[2].
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

/// ABI of the deployed contracts(`contracts/compiled_0.79.3`), embedded for on-chain getters.
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
/// `PrivateNote`(zk-note) -- ABI of the deal's owner methods (`deployInferenceOrderBook`,
/// `postSellOffer`, `placeInferenceBuy`, `streamStop`, getter `getInferenceOrderBookAddress`).
/// Minted via RootPN(gosh-dexdo `mint_pn_pool`); the signatures of these 5 methods match the
/// deployed code byte-for-byte -- the note accepts our calls(see the live test).
pub(super) const PRIVATENOTE_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/dex/PrivateNote.abi.json");
/// `PrivateNote` StateInit(`.tvc`) -- for the diagnostic comparison of the embedded image's code-hash
/// against the `Account.code_hash` value of the minted note(live cross-check of the inference variant; test-only).
pub(super) const PRIVATENOTE_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/dex/PrivateNote.tvc");
pub(super) const SUPERROOT_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/SuperRoot.tvc");
pub(super) const ROOTPN_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/dex/RootPN.tvc");
pub(super) const ROOTORACLE_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/dex/RootOracle.tvc");
/// `TokenContract` StateInit(`.tvc`) -- deployed via `build_deploy` (step 2: the seller provisions
/// the per-deal TC). Its code-hash == the `RootModel.TOKEN_CONTRACT_CODE_HASH` pin(offline guard), so
/// the derived address matches `RootModel.getTokenContractAddress` and registration is accepted.
pub(super) const TOKENCONTRACT_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/TokenContract.tvc");
/// `RootModel` StateInit(`.tvc`) -- the seller(model owner) deploys their own RootModel under
/// SuperRoot themselves(self-register: the ctor calls `SuperRoot.registerRoot`). Its code-hash == the
/// `SuperRoot.ROOT_MODEL_CODE_HASH` pin(offline guard), otherwise SuperRoot rejects the registration.
pub(super) const ROOTMODEL_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/RootModel.tvc");
/// The `TOKEN_CONTRACT_CODE_HASH` pin from `contracts/airegistry/RootModel.sol` -- against it RootModel
/// checks the TC code when registering a deal. The embedded `TokenContract.tvc` must yield this hash.
pub(super) const ROOTMODEL_PINNED_TC_CODE_HASH: &str =
    "1fa8c398d5379a5bb31380c3c2b5b8dd36a7c0a1e7af94d89cce5a2e9458eb4b";
/// The `ROOT_MODEL_CODE_HASH` pin from `contracts/airegistry/SuperRoot.sol` -- against it SuperRoot
/// checks the RootModel code at `registerRoot`. The embedded `RootModel.tvc` must yield this hash.
pub(super) const SUPERROOT_PINNED_RM_CODE_HASH: &str =
    "1425d0ce8e82fc19ff23a721b2ef11befdb493c0901a9f63f4f903abca3d5de7";
/// The deployed `PrivateNote` code-hash(`deployed.shellnet.json` `_note`). The orphaned-note
/// guard(`assert_seller_note_current`) requires the seller note's on-chain `code_hash` to equal this; the
/// `private_note_code_hash_matches_deployed_pin` test cross-checks it against the embedded `PRIVATENOTE_TVC`
/// (test-only). Update on every PrivateNote redeploy(same cadence as `deployed.shellnet.json`).
pub(super) const PRIVATENOTE_PINNED_CODE_HASH: &str =
    "1d2fcae0a1a7bc8af4e39992fbf0eda7bc2e7ff3397e44500ff03b57247d732f";

/// (pure, offline-testable): the seller note must carry the CURRENT pinned `PrivateNote` code. A note
/// minted before a contract redeploy is orphaned(stale code_hash) and its owner-methods (`postSellOffer`,
/// provision) throw a raw `TVM_ERROR`; reject fail-closed with an actionable re-mint message instead. The async
/// [`RealChainBackend::assert_seller_note_current`] wraps this with the on-chain existence + Active checks.
pub(super) fn note_code_hash_current(note: &Address, code_hash: Option<&str>) -> Result<()> {
    match code_hash {
        Some(h) if h == PRIVATENOTE_PINNED_CODE_HASH => Ok(()),
        other => Err(anyhow!(
            "seller note {note} code_hash {} != the current PrivateNote code {PRIVATENOTE_PINNED_CODE_HASH} \
             -- the pn_pool predates a contract redeploy (orphaned). Re-mint against the current contracts \
             (`mint_pn_pool`) and point DEXDO_PN_POOL at the fresh pool.",
            other.unwrap_or("<none>")
        )),
    }
}
/// `InferenceOrderBook` -- ABI of the on-chain offer/order book.
pub(super) const INFERENCE_ORDERBOOK_ABI: &str =
    include_str!("../../../../contracts/compiled_0.79.3/airegistry/InferenceOrderBook.abi.json");
/// `InferenceOrderBook` StateInit(`.tvc`) -- the **code-cell** is extracted from it, which the note
/// passes to `deployInferenceOrderBook(code,...)`(the book address is deterministic from code+params).
pub(super) const INFERENCE_ORDERBOOK_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled_0.79.3/airegistry/InferenceOrderBook.tvc");
pub(super) const ROOTPN_ADDR: &str =
    "0:1010101010101010101010101010101010101010101010101010101010101010";
pub(super) const ROOTORACLE_ADDR: &str =
    "0:1515151515151515151515151515151515151515151515151515151515151515";
/// Decode a hex string(TVM ABI `bytes` output) without an external dependency.
pub(super) fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(anyhow!("odd hex length"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

/// Derive a note's ed25519 public key(`[u8; 32]`) from its owner [`KeyPair`] -- the same derivation
/// `RealNote::pubkey` uses(`KeyPair::public_hex` -> bytes). Used by `dexdo recover` to verify the
/// recover note is the deal's recorded buyer(`getBuyerPubkey`) before signing STOP.
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

/// Encode bytes to hex(a TVM ABI `bytes` argument, e.g. `endpointCipher`; and code-hash comparison).
/// `write!` directly into the buffer -- without allocating a `String` per byte.
pub(super) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Extract the **code-cell** from a `.tvc`(StateInit BOC) -- the same logic as
/// `airegistry::abi::Contract::code_cell` in the SDK: `read_single_root_boc` -> `StateInit` -> `.code`.
pub(super) fn code_cell(tvc: &[u8]) -> Result<tvm_types::Cell> {
    let cell = tvm_types::read_single_root_boc(tvc).map_err(|e| anyhow!("read tvc BOC: {e}"))?;
    let state_init =
        StateInit::construct_from_cell(cell).map_err(|e| anyhow!("parse StateInit: {e}"))?;
    state_init
        .code
        .ok_or_else(|| anyhow!("no code-cell in StateInit"))
}

/// The `.tvc` code-cell as base64-BOC -- the encoding of a `cell` argument in TVM ABI(`call`/`run_getter`).
pub(super) fn code_boc_b64(tvc: &[u8]) -> Result<String> {
    let boc = tvm_types::write_boc(&code_cell(tvc)?).map_err(|e| anyhow!("write code BOC: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(boc))
}

/// Hex `tvm.hash` of a `.tvc` code-cell -- compared against `Account.code_hash` by `dexdo doctor`.
pub(super) fn code_hash(tvc: &[u8]) -> Result<String> {
    Ok(encode_hex(code_cell(tvc)?.repr_hash().as_slice()))
}
