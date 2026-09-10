//! recognising OUR request in a Vault's queue, and the instructions the request-less
//! providers print.

//! Recognition is the half of repeat safety the journal cannot supply on its own. A matcher that is
//! too loose adopts somebody else's transaction and never makes our own; one that is too strict
//! fails to see our own and makes a second. Both are money errors, in opposite directions.

use std::collections::BTreeMap;

use super::*;
use crate::cli::wallet_funding::{
    payload_hash, vault_to_hot_native_value, FundingFingerprint, VAULT_TO_HOT_BOUNCE,
    VAULT_TO_HOT_PAYLOAD, VAULT_TO_HOT_SEND_FLAGS,
};

fn hex64(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn fingerprint() -> FundingFingerprint {
    FundingFingerprint {
        creator: hex64(0xc3),
        dest: format!("{}::{}", hex64(0xa1), hex64(0xa1)),
        dapp_id: hex64(0xa1),
        value: vault_to_hot_native_value(),
        cc: [(2u32, 1_000u128)].into_iter().collect(),
        send_flags: VAULT_TO_HOT_SEND_FLAGS,
        bounce: VAULT_TO_HOT_BOUNCE,
        payload_hash: payload_hash(VAULT_TO_HOT_PAYLOAD),
    }
}

fn matching_entry() -> QueuedTransfer {
    let expected = fingerprint();
    QueuedTransfer {
        id: 7,
        creator_pubkey: Some(expected.creator.clone()),
        dest: expected.dest.clone(),
        value: expected.value,
        cc: expected.cc.clone(),
        send_flags: expected.send_flags,
        bounce: expected.bounce,
        dapp_id: expected.dapp_id.clone(),
        payload: None,
    }
}

#[test]
fn a_queue_entry_that_is_our_transfer_matches() {
    assert!(queue_entry_matches(&matching_entry(), &fingerprint()));
}

/// Every field of the fingerprint is load-bearing: change any one of them and the queue entry is a
/// different transfer, which must not be adopted as ours.
#[test]
fn every_fingerprint_field_is_part_of_the_match() {
    type QueueMutation = (&'static str, Box<dyn Fn(&mut QueuedTransfer)>);
    let expected = fingerprint();
    let mutations: Vec<QueueMutation> = vec![
        (
            "creator",
            Box::new(|entry: &mut QueuedTransfer| entry.creator_pubkey = Some(hex64(0xee))),
        ),
        (
            "dest",
            Box::new(|entry: &mut QueuedTransfer| {
                entry.dest = format!("{}::{}", hex64(0xee), hex64(0xee))
            }),
        ),
        (
            "dapp_id",
            Box::new(|entry: &mut QueuedTransfer| entry.dapp_id = hex64(0xee)),
        ),
        (
            "value",
            Box::new(|entry: &mut QueuedTransfer| entry.value += 1),
        ),
        (
            "cc",
            Box::new(|entry: &mut QueuedTransfer| {
                entry.cc = [(2u32, 999u128)].into_iter().collect()
            }),
        ),
        (
            "sendFlags",
            Box::new(|entry: &mut QueuedTransfer| entry.send_flags = 16),
        ),
        (
            "bounce",
            Box::new(|entry: &mut QueuedTransfer| entry.bounce = !entry.bounce),
        ),
        (
            "payload",
            Box::new(|entry: &mut QueuedTransfer| {
                entry.payload = Some("te6ccgEBAQEAEgAAIAAAAAAAAAAAAAAAAAAAAAA=".to_string())
            }),
        ),
    ];
    for (field, mutate) in mutations {
        let mut entry = matching_entry();
        mutate(&mut entry);
        assert!(
            !queue_entry_matches(&entry, &expected),
            "a queue entry differing in `{field}` is a different transfer and must not match"
        );
    }
}

/// A transaction to the same destination for the same amount, created by a DIFFERENT custodian, is
/// not ours. Adopting it would leave our own request never made.
#[test]
fn another_custodians_identical_transfer_is_not_ours() {
    let mut entry = matching_entry();
    entry.creator_pubkey = Some(hex64(0xdd));
    assert!(!queue_entry_matches(&entry, &fingerprint()));
    // Nor is one with no creator key at all - an address custodian, which our agent never is.
    let mut entry = matching_entry();
    entry.creator_pubkey = None;
    assert!(!queue_entry_matches(&entry, &fingerprint()));
}

/// The queue reports the empty payload either as an absent cell or as the empty cell, and both are
/// the payload every transfer this client creates carries.
#[test]
fn both_spellings_of_the_empty_payload_hash_alike() {
    let empty = payload_hash(VAULT_TO_HOT_PAYLOAD);
    assert_eq!(queue_payload_hash(None), empty);
    assert_eq!(queue_payload_hash(Some("")), empty);
    assert_eq!(queue_payload_hash(Some(EMPTY_PAYLOAD_CELL)), empty);
    assert_ne!(
        queue_payload_hash(Some("te6ccgEBAQEAEgAAIAAAAAAAAAAAAAAAAAAAAAA=")),
        empty,
        "a transaction carrying a body we did not send must not hash as the empty payload"
    );
}

/// One `uint256`, two spellings the chain may answer with. Comparing them unequal would read as
/// "not our request", which is the state that authorizes a second transfer.
#[test]
fn a_uint256_compares_equal_across_the_spellings_the_chain_uses() {
    let mut entry = matching_entry();
    entry.dapp_id = format!("0x{}", hex64(0xa1).to_uppercase());
    assert!(queue_entry_matches(&entry, &fingerprint()));

    let mut entry = matching_entry();
    entry.creator_pubkey = Some(format!("0x{}", hex64(0xc3)));
    assert!(queue_entry_matches(&entry, &fingerprint()));

    // A short key is left-padded, exactly as the custodian reader already pads them.
    assert_eq!(normalized_uint256("0x1f"), format!("{:0>64}", "1f"));
    assert_eq!(normalized_uint256("1F"), format!("{:0>64}", "1f"));
}

/// The destination may come back in either the canonical or the legacy spelling.
#[test]
fn a_destination_compares_equal_across_the_spellings_the_chain_uses() {
    let mut entry = matching_entry();
    entry.dest = format!("0:{}", hex64(0xa1));
    assert!(
        queue_entry_matches(&entry, &fingerprint()),
        "the legacy spelling of the Hot is the same account as the canonical one"
    );
}

// ---------------------------------------------------------------------------------------------
// The providers with no request to create
// ---------------------------------------------------------------------------------------------

fn direct_request(provider: WalletProvider) -> FundingRequest {
    FundingRequest {
        provider,
        network: "net-a".to_string(),
        vault_address: None,
        hot_address: format!("{}::{}", hex64(0xa1), hex64(0xa1)),
        hot_dapp_id: hex64(0xa1),
        creator_pubkey: hex64(0xc3),
        required: [(2u32, 1_000u128)].into_iter().collect(),
        required_native: vault_to_hot_native_value(),
        shortfall: [(2u32, 600u128)].into_iter().collect(),
        native_shortfall: 0,
    }
}

#[test]
fn the_direct_providers_refuse_to_serve_a_provider_that_has_a_vault() {
    assert!(DirectTopUpProvider::new(WalletProvider::AckinackiWallet).is_err());
    assert!(DirectTopUpProvider::new(WalletProvider::GoshAi).is_ok());
    assert!(DirectTopUpProvider::new(WalletProvider::Manual).is_ok());
}

#[test]
fn the_goshai_instruction_names_the_shortfall_and_the_address_and_carries_no_link() {
    let provider = DirectTopUpProvider::new(WalletProvider::GoshAi).expect("provider");
    let request = direct_request(WalletProvider::GoshAi);
    let instruction = provider.manual_instruction(&request);
    assert!(instruction.contains("0.0000006 SHELL"), "{instruction}");
    assert!(instruction.contains(&request.hot_address), "{instruction}");
    // the placeholder link is gone from this sentence -- Gosh.ai's address is the
    // manifest's to state, and a second copy in a second surface is how the two drift apart.
    // No link, in any spelling: a schemeless `gosh.ai/subscription` is a link an operator types
    // into a browser just as readily as one with a scheme, so `http` alone would not hold this.
    for shaped_like_a_link in ["http", "www.", "gosh.ai/"] {
        assert!(
            !instruction.contains(shaped_like_a_link),
            "the top-up instruction carries a link again ({shaped_like_a_link:?}). Where Gosh.ai \
             lives is the manifest's to say, and a second copy in a second surface is how the two \
             come to disagree: {instruction}"
        );
    }
}

#[test]
fn the_manual_instruction_offers_no_link_and_no_vault() {
    let provider = DirectTopUpProvider::new(WalletProvider::Manual).expect("provider");
    let request = direct_request(WalletProvider::Manual);
    let instruction = provider.manual_instruction(&request);
    assert!(instruction.contains("0.0000006 SHELL"), "{instruction}");
    assert!(instruction.contains(&request.hot_address), "{instruction}");
    assert!(
        !instruction.contains("http") && !instruction.to_lowercase().contains("vault"),
        "a manual Hot has neither a service to visit nor a Vault to ask: {instruction}"
    );
}

/// A provider with no queue must never answer `Absent`, which is the answer that authorizes a
/// submit.
#[tokio::test]
async fn a_provider_without_a_queue_never_answers_absent() {
    for provider_kind in [WalletProvider::GoshAi, WalletProvider::Manual] {
        let provider = DirectTopUpProvider::new(provider_kind).expect("provider");
        let presence = provider
            .probe_existing_request(&direct_request(provider_kind))
            .await
            .expect("probe");
        assert!(
            matches!(presence, RequestPresence::Unknown { .. }),
            "{provider_kind:?} has no queue to prove absence from: {presence:?}"
        );
        assert!(
            provider
                .create_request(&direct_request(provider_kind))
                .await
                .is_err(),
            "{provider_kind:?} has no Vault and must not pretend to create a request"
        );
    }
}

/// The flag the wire takes is the flag the fingerprint records.
#[test]
fn the_wire_flag_and_the_recorded_send_flags_are_one_value() {
    assert_eq!(
        u16::from(send_flag_argument().expect("the send flags fit the argument")),
        VAULT_TO_HOT_SEND_FLAGS
    );
    assert_eq!(bounce_argument(), VAULT_TO_HOT_BOUNCE);
}

/// A currency map with a zero entry and one without it describe the same money, and must compare
/// equal - otherwise a queue entry becomes unrecognisable over a currency nobody is sending.
#[test]
fn the_fingerprint_never_carries_a_zero_currency() {
    let expected = fingerprint();
    let zero: BTreeMap<u32, u128> = [(2u32, 1_000u128)].into_iter().collect();
    assert_eq!(expected.cc, zero);
    assert!(
        !expected.cc.values().any(|amount| *amount == 0),
        "a shortfall map never carries a zero, so the fingerprint never does either"
    );
}
