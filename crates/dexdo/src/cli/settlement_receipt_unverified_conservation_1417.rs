//! The conservation verdict must not be `conserved` when nothing was cross-checked.

//! # The defect this pins

//! `conservation_receipt` prefers the NOTES for the payout term, because they are a statement by
//! different accounts than the deal's own. When fewer than two notes were read it falls back to the
//! deal's own `declared_payout` -- and from there the identity is self-referential:

//! ```text
//! payout = declared_payout (the deal's own figure)
//! written_off = funded_in - payout ("implied_by_conservation")
//! unexplained = funded_in - payout - (funded_in - payout) == 0, for every input
//! ```

//! So the receipt printed `conserved` -- "the money adds up" -- having compared one number with
//! itself. On the real deal the figures are `funded_in = 12 200 000 000`, declared payout
//! `11 175 400 000`, implied write-off `1 024 600 000`, and `unexplained = 0` whether or not a
//! single note was ever read. The fallback branch had no test: the sibling file's fixture hardcodes
//! `notes_read: vec![BUYER, SELLER]`, so the degenerate path was never executed.

//! # Both directions, deliberately

//! A test that only proved the refusal would leave an always-red verdict, which is the worse
//! failure: a status that is never `conserved` carries no information either. So the first case
//! below drives two notes that agree and requires `conserved` to still be reachable.

use super::*;
use dexdo_core::{
    NoteDealCreditReceipt, TokenContractReceiptChainData, TokenContractSettlementEvent,
    TokenContractSettlementReceipt, TokenContractSettlementReceipts,
};

const DEAL: &str = "0:9b81f701f6e94a12d2772607f3874d1ccde9459c46f7688cbceb244a4fe098bd";
const BUYER: &str = "0:977936df3527a524516a796c619bfe4a40238a7bef76378d4f1aaba8016db438";
const SELLER: &str = "0:851a3cb6388e1bf815898fc5977743ce31b004139fe546dafe0f6af5d837fa76";

/// The real deal's terms. `8.2` escrow + `2` buyer bond + `2` seller bond funded in; the terminal
/// splits `3.0004` to the seller and `8.175` back to the buyer, leaving `1.0246` written off.
const FUNDED_IN: u128 = 12_200_000_000;
const TO_SELLER: u128 = 3_000_400_000;
const REFUND_TO_BUYER: u128 = 8_175_000_000;
const DECLARED_PAYOUT: u128 = TO_SELLER + REFUND_TO_BUYER;
const IMPLIED_WRITE_OFF: u128 = FUNDED_IN - DECLARED_PAYOUT;

fn event(id: &str, at: u64, event: TokenContractSettlementEvent) -> TokenContractSettlementReceipt {
    TokenContractSettlementReceipt {
        message_id: id.to_string(),
        created_at: at,
        cursor: format!("cursor-{id}"),
        event,
    }
}

fn credit(note: &str, amount: u128, id: &str) -> NoteDealCreditReceipt {
    NoteDealCreditReceipt {
        note: note.to_string(),
        deal: DEAL.to_string(),
        amount,
        message_id: id.to_string(),
        created_at: 1_786_929_867,
        cursor: format!("cursor-{id}"),
    }
}

fn settled_events() -> Vec<TokenContractSettlementReceipt> {
    vec![
        event(
            "d16efc22830bc94e",
            1_786_929_000,
            TokenContractSettlementEvent::StreamFunded {
                buyer: BUYER.to_string(),
                deposit: 8_200_000_000,
            },
        ),
        event(
            "a0d31cbe7ff4bba5",
            1_786_929_010,
            TokenContractSettlementEvent::BuyerBondFunded {
                amount: 2_000_000_000,
            },
        ),
        event(
            "ba5c11eef0c69240",
            1_786_929_020,
            TokenContractSettlementEvent::SellerBondFunded {
                amount: 2_000_000_000,
            },
        ),
        event(
            "fe3c30e277b9ff51",
            1_786_929_867,
            TokenContractSettlementEvent::StreamStopped {
                buyer: BUYER.to_string(),
                to_seller: TO_SELLER,
                refund_to_buyer: REFUND_TO_BUYER,
            },
        ),
    ]
}

/// A destroyed deal -- the state in which a settlement verdict is final and therefore matters.
fn chain_read(
    notes_read: Vec<String>,
    note_credits: Vec<NoteDealCreditReceipt>,
) -> TokenContractReceiptChainData {
    TokenContractReceiptChainData {
        account_id: DEAL.to_string(),
        account_active: false,
        code_hash: None,
        current: None,
        receipts: TokenContractSettlementReceipts {
            events: settled_events(),
        },
        note_credits,
        notes_read,
    }
}

fn receipt(chain: &TokenContractReceiptChainData) -> serde_json::Value {
    let built = build_receipt(
        ReceiptContext {
            generated_at: 1_786_929_900,
            network: "mainnet".to_string(),
            chain_endpoint: "https://net-b.example/graphql".to_string(),
            contracts_generation: Some("4.0.35".to_string()),
            expected_code_hash: None,
            token_contract: DEAL.to_string(),
            season: None,
            local_client_stop: None,
        },
        chain,
    );
    serde_json::to_value(&built).expect("serialize receipt")
}

/// DIRECTION ONE. Two notes, and they confirm the deal's own split. The verdict must still be
/// reachable: a conservation status that can never say `conserved` is as useless as one that always
/// does, and this is the half that keeps the fix from becoming an always-red gate.
#[test]
fn two_notes_that_confirm_the_split_still_report_conserved() {
    let value = receipt(&chain_read(
        vec![BUYER.to_string(), SELLER.to_string()],
        vec![
            credit(BUYER, REFUND_TO_BUYER, "c1"),
            credit(SELLER, TO_SELLER, "c2"),
        ],
    ));
    let block = &value["conservation"];
    assert_eq!(block["status"], "conserved");
    assert_eq!(block["payout_source"], "note_deal_credited");
    assert_eq!(block["unexplained"], "0");
    assert_eq!(block["funded_in"], FUNDED_IN.to_string());
    assert_eq!(block["written_off"], IMPLIED_WRITE_OFF.to_string());
    assert!(
        !value["consistency_issues"]
            .as_array()
            .expect("issues array")
            .iter()
            .any(|issue| issue == "deal_money_conservation_unverified"),
        "a cross-checked settlement must not be flagged unverified: {value}"
    );
}

/// DIRECTION TWO, and the defect. One note read, no credits: there is no second account's statement,
/// so the identity has only the deal's own figure on both sides. It closes -- `unexplained` is `0` --
/// and that zero is arithmetic, not evidence. The status must say so.
#[test]
fn one_note_read_is_not_a_conserved_verdict() {
    let value = receipt(&chain_read(vec![BUYER.to_string()], Vec::new()));
    let block = &value["conservation"];

    // The identity DID close. This is the line that shows why the old status was so convincing.
    assert_eq!(block["unexplained"], "0");
    assert_eq!(block["payout_source"], "deal_terminal_event");
    assert_eq!(block["payout"], DECLARED_PAYOUT.to_string());

    // And it means nothing, so it must not be called conserved.
    assert_ne!(
        block["status"], "conserved",
        "a payout compared with itself is not a conserved settlement: {value}"
    );
    assert_eq!(block["status"], "unverified");

    // The reason carries the COUNT, so "nobody checked" cannot be mistaken for "two accounts agreed".
    assert!(
        block["missing"]
            .as_array()
            .expect("missing array")
            .iter()
            .any(|reason| reason == "payout_not_cross_checked_notes_read_1"),
        "the receipt must name how many notes it managed to read: {value}"
    );
    assert!(
        value["consistency_issues"]
            .as_array()
            .expect("issues array")
            .iter()
            .any(|issue| issue == "deal_money_conservation_unverified"),
        "a settled, destroyed deal nobody could cross-check is a finding: {value}"
    );
}

/// No notes at all is the same defect one step further along, and the count in the reason has to
/// move with it rather than being a fixed string that happens to read well at one.
#[test]
fn no_notes_read_is_not_a_conserved_verdict_either() {
    let value = receipt(&chain_read(Vec::new(), Vec::new()));
    let block = &value["conservation"];
    assert_ne!(block["status"], "conserved");
    assert_eq!(block["status"], "unverified");
    assert!(block["missing"]
        .as_array()
        .expect("missing array")
        .iter()
        .any(|reason| reason == "payout_not_cross_checked_notes_read_0"));
}

/// Two notes READ but neither reporting a credit is still not a cross-check: `credits_complete`
/// requires both, and the fallback is the same self-referential identity.
#[test]
fn two_notes_read_but_no_credits_reported_is_not_conserved() {
    let value = receipt(&chain_read(
        vec![BUYER.to_string(), SELLER.to_string()],
        Vec::new(),
    ));
    assert_eq!(value["conservation"]["status"], "unverified");
}

/// The verdict states which money it is about. `conserved` here is a claim about the traded asset
/// only: the deal's SHELL is a scalar inside `TokenContract`, and native `vmshell` gas is a
/// different balance that no term in the block counts.
#[test]
fn the_verdict_names_the_plane_it_speaks_about() {
    let value = receipt(&chain_read(
        vec![BUYER.to_string(), SELLER.to_string()],
        vec![
            credit(BUYER, REFUND_TO_BUYER, "c1"),
            credit(SELLER, TO_SELLER, "c2"),
        ],
    ));
    assert_eq!(value["conservation"]["covers"], "ecc2_traded_asset_only");
}
