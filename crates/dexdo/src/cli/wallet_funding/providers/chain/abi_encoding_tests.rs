//! item 1: the Vault -> Hot `submitTransaction` parameters, put through the SDK encoder.

//! Reading the built `Value` back and comparing it to what the builder put there proves only that
//! the builder is self-consistent. It cannot see the one thing that decides whether the transfer
//! ever leaves this process: `dapp_id` is declared `uint256` in
//! `UpdateCustodianMultisigWallet_v2.submitTransaction`, and the SDK tokenizer reads a string
//! argument as DECIMAL unless it carries the `0x` prefix. A bare 64-hex DApp id is therefore refused
//! with `can not parse number from string` before any message is built - a refusal a string-to-string
//! assertion is blind to, because the string it compares is exactly the one the encoder rejects.

//! So this goes through [`encode_external_call`] against the vendored canonical ABI - the same
//! function, ABI and parameters `RealVaultChain::submit` uses - and then decodes the message the
//! chain would receive. Encoding is the red/green oracle; the decode makes the assertion about the
//! DApp the argument NAMES rather than about the absence of an error.

use dexdo_core::airegistry::{calls::encode_external_call, deploy::local_context};
use dexdo_core::KeyPair;

use super::vault_to_hot_submit_transaction_params;
use crate::cli::wallet_funding::{payload_hash, FundingFingerprint, VAULT_TO_HOT_PAYLOAD};

/// A Hot's DApp id carries hex letters, as every real self-DApp multisig id derived from an account
/// id does. That is precisely the shape a decimal parse cannot read.
fn hot_dapp_id() -> String {
    "a1".repeat(32)
}

fn vault_address() -> String {
    format!("0:{}", "d4".repeat(32))
}

fn vault_to_hot_fingerprint() -> FundingFingerprint {
    let hot_dapp_id = hot_dapp_id();
    FundingFingerprint {
        creator: "c3".repeat(32),
        dest: format!("{hot_dapp_id}::{}", "b2".repeat(32)),
        dapp_id: hot_dapp_id,
        value: 0,
        cc: [(2u32, 350_000_000_000u128)].into_iter().collect(),
        send_flags: 1,
        bounce: true,
        payload_hash: payload_hash(VAULT_TO_HOT_PAYLOAD),
    }
}

/// The parameters the production submit builds encode against the real ABI, and the encoded message
/// names the Hot's own DApp.
#[tokio::test]
async fn vault_to_hot_submit_transaction_params_encode_against_the_canonical_multisig_abi() {
    let fingerprint = vault_to_hot_fingerprint();
    let params = vault_to_hot_submit_transaction_params(&fingerprint)
        .expect("build Vault -> Hot parameters");
    let context = local_context().expect("local SDK context");
    let keys = KeyPair::generate();

    let message = encode_external_call(
        &context,
        dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
        &vault_address(),
        "submitTransaction",
        params,
        keys.public_hex(),
        keys.secret_hex(),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "the Vault -> Hot parameters must encode against \
             UpdateCustodianMultisigWallet_v2.submitTransaction: {error}"
        )
    });

    let decoded = tvm_client::abi::decode_message(
        context,
        tvm_client::abi::ParamsOfDecodeMessage {
            abi: tvm_client::abi::Abi::Json(
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON.to_string(),
            ),
            message,
            allow_partial: false,
            function_name: None,
            data_layout: None,
        },
    )
    .expect("decode the encoded Vault -> Hot message");
    assert_eq!(decoded.name, "submitTransaction");
    let inputs = decoded.value.expect("decoded submitTransaction inputs");

    // The detokenizer renders a `uint256` as `0x` + 64 zero-padded lowercase hex digits, so this is
    // the Hot's own DApp id and not, say, the decimal reading of the same characters.
    assert_eq!(
        inputs["dapp_id"],
        serde_json::Value::String(format!("0x{}", hot_dapp_id())),
        "the encoded transfer must name the Hot's own DApp"
    );
    assert_eq!(
        inputs["dest"],
        serde_json::Value::String(format!("0:{}", "b2".repeat(32))),
        "the encoded transfer must address the Hot's account"
    );
}
