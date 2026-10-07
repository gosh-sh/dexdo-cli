//! the settlement receipt's `current` block reads the `getState()` the COMPILED ABI
//! declares, and the payloads that prove it are keyed from that ABI rather than from literals here.

//! What went wrong is worth stating exactly, because the test that should have caught it was green
//! the whole time. `parse_current` required `prepaid`, `frozen`, `prepaidTime` and `lastAdvance` --
//! the pre-4.0.31 prepaid/frozen buffer. `TokenContract.getState()` has declared none of them since
//! that generation, so on every live deal the parse returned `None`, the receipt pushed
//! `current_getter_shape_invalid`, and a non-empty `consistency_issues` made BOTH `terminal.status`
//! and `withdrawal.status` read `inconsistent` while the whole `current` block was omitted. A money
//! document declared itself untrustworthy on every deal it was ever run against.

//! It survived because the fixture beside it fed the parser the same four invented names, so the
//! test confirmed the PARSER against itself and never once mentioned the contract. That is the
//! defect this module is built to make impossible: every payload below takes its keys from
//! `contracts/compiled/airegistry/TokenContract.abi.json`, so a field the chain renames drops out
//! of the fixture and reddens these tests instead of being quietly agreed with.

//! The four assertions are deliberately different in kind:
//! - the ABI-shaped payload produces a receipt with NO issues (the fix works);
//! - dropping any ONE ABI-declared field produces `inconsistent` (every field is really required,
//! so a parser that stops reading one cannot pass here);
//! - the superseded prepaid/frozen shape is REFUSED;
//! - a refusal NAMES the field it refused on, beside the machine code rather than instead of it
//! -- the decoder always had that sentence and `parse_current` used to discard it, which is why
//! identifying the four dead fields took a hand comparison against the compiled ABI.

use super::*;
use dexdo_core::{
    TokenContractCurrentFacts, TokenContractReceiptChainData, TokenContractSettlementEvent,
    TokenContractSettlementReceipt, TokenContractSettlementReceipts,
};

/// The compiled artifact this workspace embeds -- the same file `dexdo_core` pins its strict
/// `getState` decoder against in `the_deal_state_decoder_matches_the_compiled_getstate`.
const TOKEN_CONTRACT_ABI: &str =
    include_str!("../../../../contracts/compiled/airegistry/TokenContract.abi.json");

const DEAL: &str = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BUYER: &str = "0:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const SELLER: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ROOT_MODEL: &str = "0:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

/// The output field names of one getter, in the order the COMPILED ABI declares them.
pub(super) fn abi_output_names(getter: &str) -> Vec<String> {
    let abi: Value =
        serde_json::from_str(TOKEN_CONTRACT_ABI).expect("parse the compiled TokenContract ABI");
    let function = abi["functions"]
        .as_array()
        .expect("the compiled ABI declares functions")
        .iter()
        .find(|function| function["name"] == getter)
        .unwrap_or_else(|| panic!("the compiled ABI declares {getter}"));
    let names = function["outputs"]
        .as_array()
        .unwrap_or_else(|| panic!("{getter} declares outputs"))
        .iter()
        .map(|output| {
            output["name"]
                .as_str()
                .unwrap_or_else(|| panic!("{getter} output has a name"))
                .to_string()
        })
        .collect::<Vec<_>>();
    // Emptiness is what this whole module reads as evidence, so prove the lookup can find
    // something: a mistyped getter name would otherwise return an empty list and every assertion
    // below would pass against a payload with no fields in it at all.
    assert!(
        !names.is_empty(),
        "{getter} declares at least one output in the compiled ABI"
    );
    names
}

/// A `getState()` payload whose KEYS come from the compiled ABI rather than from literals here.

/// `values` supplies one value per field, keyed by the name the ABI declares. Both directions are
/// hard failures on purpose. A name the ABI declares and this table does not is a field the
/// contract grew that nobody has decided about -- filling it with `null` would let a stale reader
/// look correct. A name in this table the ABI does not declare is itself: a caller still
/// writing a superseded generation's field, which is precisely what must never be absorbed into a
/// fixture that then proves the wrong thing.
pub(super) fn getstate_payload(values: &[(&str, Value)]) -> Value {
    let declared = abi_output_names("getState");
    for (field, _) in values {
        assert!(
            declared.iter().any(|name| name == field),
            "the compiled getState() does not declare {field}"
        );
    }
    let mut object = serde_json::Map::new();
    for name in &declared {
        let (_, value) = values
            .iter()
            .find(|(field, _)| field == name)
            .unwrap_or_else(|| panic!("the compiled getState() field {name} has no fixture value"));
        object.insert(name.clone(), value.clone());
    }
    Value::Object(object)
}

/// A funded, opened, probe-frozen deal: escrow still held, one tick claimed and promoted, one
/// pending claim inside its window. Figures are in the units the contract uses -- SHELL for
/// `deposit`/`probeTick`/`finalizedOwed`, cumulative TOKENS for `tokensFinal`/`tokensPending`.
fn live_state() -> Value {
    getstate_payload(&[
        ("funded", json!(true)),
        ("opened", json!(true)),
        ("probeAccepted", json!(true)),
        ("disputed", json!(false)),
        ("deposit", json!("25")),
        ("probeTick", json!("0")),
        ("finalizedOwed", json!("10")),
        ("tokensFinal", json!("1000000")),
        ("tokensPending", json!("2000000")),
        ("probeTime", json!("100")),
        ("lastClaimTime", json!("101")),
        ("disputeTime", json!("0")),
        ("fundedTime", json!("90")),
    ])
}

fn current_facts(state: Value) -> TokenContractCurrentFacts {
    TokenContractCurrentFacts {
        state,
        fees: json!({
            "feeAccrued": "0",
            "ticksFinalized": "1",
            "everDisputed": false,
            "rebateMaxBps": "5000",
            "rebateSlopeBps": "100",
        }),
        deal: json!({
            "tickSize": u128::from(dexdo_core::DobParams::canonical().tick_size).to_string(),
            "pricePerTick": "10",
            "maxTicks": "1024",
        }),
        parties: json!({ "buyer": BUYER, "sellerNote": SELLER }),
        seller: json!({
            "sellerPubkey": "0x1234",
            "rootModelAddress": ROOT_MODEL,
            "nonce": "42",
        }),
        version: json!({ "value0": "4.0.36", "value1": "TokenContract" }),
    }
}

fn context() -> ReceiptContext {
    ReceiptContext {
        generated_at: 1_787_400_000,
        network: "net-a".to_string(),
        chain_endpoint: "https://net-a.example/graphql".to_string(),
        contracts_generation: Some("4.0.36".to_string()),
        expected_code_hash: Some("ab".repeat(32)),
        token_contract: DEAL.to_string(),
        season: None,
        local_client_stop: None,
    }
}

fn event(id: &str, at: u64, event: TokenContractSettlementEvent) -> TokenContractSettlementReceipt {
    TokenContractSettlementReceipt {
        message_id: id.to_string(),
        created_at: at,
        cursor: format!("cursor-{id}"),
        event,
    }
}

fn chain(state: Value) -> TokenContractReceiptChainData {
    TokenContractReceiptChainData {
        account_id: DEAL.to_string(),
        account_active: true,
        code_hash: Some("ab".repeat(32)),
        current: Some(current_facts(state)),
        receipts: TokenContractSettlementReceipts {
            events: vec![
                event(
                    "funded",
                    1,
                    TokenContractSettlementEvent::StreamFunded {
                        buyer: BUYER.to_string(),
                        deposit: 45,
                    },
                ),
                event(
                    "opened",
                    2,
                    TokenContractSettlementEvent::StreamOpened {
                        buyer: BUYER.to_string(),
                        price_per_tick: 10,
                    },
                ),
            ],
        },
        note_credits: Vec::new(),
        notes_read: Vec::new(),
    }
}

fn as_value(receipt: &SettlementReceiptV1) -> Value {
    serde_json::to_value(receipt).expect("serialize settlement receipt")
}

/// The fix, stated as the symptom: a receipt built on the shape the chain actually returns has
/// nothing to complain about, and says so in all three places broke.
#[test]
fn the_receipt_reads_the_shape_the_compiled_getstate_declares() {
    let receipt = build_receipt(context(), &chain(live_state()));

    assert!(
        receipt.consistency_issues.is_empty(),
        "an ABI-shaped getState must raise nothing: {:?}",
        receipt.consistency_issues
    );
    assert_eq!(receipt.terminal.status, "not_final");
    assert_eq!(receipt.withdrawal.status, "not_applicable");

    // The block was omitted entirely while the parse failed, so its presence is half the fix.
    let state = &as_value(&receipt)["current"]["state"];
    assert_eq!(state["deposit"], "25");
    assert_eq!(state["probe_tick"], "0");
    assert_eq!(state["finalized_owed"], "10");
    assert_eq!(state["tokens_final"], "1000000");
    assert_eq!(state["tokens_pending"], "2000000");
    assert_eq!(state["probe_time"], 100);
    assert_eq!(state["last_claim_time"], 101);
    assert_eq!(state["funded_time"], 90);
    // The buffer that no longer exists is not reported under any name.
    for gone in ["prepaid", "frozen", "prepaid_time", "last_advance"] {
        assert!(
            state.get(gone).is_none(),
            "the pre-4.0.31 buffer field {gone} is gone, not renamed"
        );
    }
}

/// Every field the compiled getter declares is one this receipt actually requires.

/// This is the assertion that reddens when the parser and the ABI part company in either
/// direction, and it never names a field itself: the list is the artifact's. A parser that stopped
/// reading `probeTick`, or started reading `probeTicks`, fails here on that field's own iteration.
#[test]
fn every_field_the_compiled_getstate_declares_is_required() {
    let declared = abi_output_names("getState");
    assert_eq!(
        declared.len(),
        13,
        "the compiled getState() is the thirteen-field generation: {declared:?}"
    );

    for dropped in &declared {
        let mut state = live_state();
        state
            .as_object_mut()
            .expect("getState payload is an object")
            .remove(dropped)
            .unwrap_or_else(|| panic!("{dropped} was in the payload before it was dropped"));

        let receipt = build_receipt(context(), &chain(state));
        assert!(
            receipt
                .consistency_issues
                .contains(&"current_getter_shape_invalid".to_string()),
            "a getState without {dropped} must invalidate the current block: {:?}",
            receipt.consistency_issues
        );
        assert_eq!(receipt.terminal.status, "inconsistent");
        assert_eq!(receipt.withdrawal.status, "inconsistent");
    }
}

/// The refusal names the field it refused on, and still carries the code for the machine.

/// Before this, a refused `getState` produced exactly one line -- `current_getter_shape_invalid` --
/// which says the class and never the member, and that is why identifying the four dead fields
/// needed a hand comparison against the compiled ABI. The decoder had the sentence the whole time;
/// `parse_current` dropped it with `.ok()?`.

/// Both halves are asserted because either alone would be a regression: the bare code without the
/// sentence is the old behaviour, and the sentence replacing the code would break any consumer
/// keying on it. The ordering assertion is the reason the detail is written as an extension of the
/// code rather than as a separate word -- `issues` is sorted, and a prefix sorts immediately before
/// its extensions, so the explanation is always the next line after the code it explains.
#[test]
fn a_refused_getstate_names_the_field_it_refused_on() {
    let mut state = live_state();
    state
        .as_object_mut()
        .expect("getState payload is an object")
        .remove("probeTick")
        .expect("probeTick was in the payload before it was dropped");
    let receipt = build_receipt(context(), &chain(state));

    assert!(
        receipt
            .consistency_issues
            .contains(&"current_getter_shape_invalid".to_string()),
        "the machine-readable code must survive: {:?}",
        receipt.consistency_issues
    );
    let detail = receipt
        .consistency_issues
        .iter()
        .find(|issue| issue.starts_with("current_getter_shape_invalid: "))
        .unwrap_or_else(|| {
            panic!(
                "the refusal must carry the decoder's reason: {:?}",
                receipt.consistency_issues
            )
        });
    assert!(
        detail.contains("probeTick"),
        "the reason must name the field that refused: {detail}"
    );
    assert!(
        detail.contains("getState()"),
        "the reason must name the getter: {detail}"
    );

    let code_at = receipt
        .consistency_issues
        .iter()
        .position(|issue| issue == "current_getter_shape_invalid")
        .expect("the code is in the list");
    assert_eq!(
        receipt.consistency_issues.get(code_at + 1),
        Some(detail),
        "sorted, the reason is the line directly after its code: {:?}",
        receipt.consistency_issues
    );
}

/// as it was, pinned as refused.

/// The four names below are written out ON PURPOSE -- they are the historical artifact, the
/// pre-4.0.31 prepaid/frozen buffer, and the point is that this shape must NOT satisfy the reader.
/// Built by hand rather than through `getstate_payload`, which would refuse them at the fixture.
#[test]
fn the_superseded_prepaid_and_frozen_shape_is_refused() {
    let superseded = json!({
        "funded": true,
        "opened": true,
        "probeAccepted": true,
        "disputed": false,
        "deposit": "25",
        "prepaid": "10",
        "frozen": "10",
        "finalizedOwed": "10",
        "prepaidTime": "100",
        "lastAdvance": "101",
        "disputeTime": "0",
        "fundedTime": "90",
    });
    for gone in ["prepaid", "frozen", "prepaidTime", "lastAdvance"] {
        assert!(
            !abi_output_names("getState").iter().any(|name| name == gone),
            "{gone} is not a field the compiled getState() declares"
        );
    }

    let receipt = build_receipt(context(), &chain(superseded));
    assert!(
        receipt
            .consistency_issues
            .contains(&"current_getter_shape_invalid".to_string()),
        "the pre-4.0.31 shape is not the getter this receipt reads: {:?}",
        receipt.consistency_issues
    );
    assert_eq!(receipt.terminal.status, "inconsistent");
}
