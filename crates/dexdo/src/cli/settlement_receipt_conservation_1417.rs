//! at the receipt surface: the settlement receipt must state, per transaction, where every
//! unit of a deal's SHELL came from and went -- and must refuse to call a reading balanced when it
//! is not.

//! The figures are the real mainnet deal the issue was opened about,
//! `0:9b81f701f6e94a12d2772607f3874d1ccde9459c46f7688cbceb244a4fe098bd` (cut `c95e2fea`, contracts
//! 4.0.35), and the live deal
//! `0:a71399a3606cb32292628d37518d7983c430420febd0b57585eabd9ca1a3a83a` read from mainnet
//! 2026-08-20, whose two `reportDealWriteOff` messages confirm the derived burn to the unit.

use super::*;
use dexdo_core::{
    NoteDealCreditReceipt, TokenContractReceiptChainData, TokenContractSettlementEvent,
    TokenContractSettlementReceipt, TokenContractSettlementReceipts,
};

const DEAL: &str = "0:9b81f701f6e94a12d2772607f3874d1ccde9459c46f7688cbceb244a4fe098bd";
const BUYER: &str = "0:977936df3527a524516a796c619bfe4a40238a7bef76378d4f1aaba8016db438";
const SELLER: &str = "0:851a3cb6388e1bf815898fc5977743ce31b004139fe546dafe0f6af5d837fa76";

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

fn chain(
    events: Vec<TokenContractSettlementReceipt>,
    note_credits: Vec<NoteDealCreditReceipt>,
) -> TokenContractReceiptChainData {
    TokenContractReceiptChainData {
        account_id: DEAL.to_string(),
        account_active: false,
        code_hash: None,
        current: None,
        receipts: TokenContractSettlementReceipts { events },
        note_credits,
        notes_read: vec![BUYER.to_string(), SELLER.to_string()],
    }
}

/// The deal, exactly as it settled. Every term is named, and nothing is left over.
fn deal_1417_events() -> Vec<TokenContractSettlementReceipt> {
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
                to_seller: 3_000_400_000,
                refund_to_buyer: 8_175_000_000,
            },
        ),
    ]
}

fn conservation(chain: &TokenContractReceiptChainData) -> serde_json::Value {
    let receipt = build_receipt(
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
    serde_json::to_value(&receipt).expect("serialize receipt")
}

#[test]
fn the_1417_deal_reports_conserved_with_nothing_unexplained() {
    let value = conservation(&chain(
        deal_1417_events(),
        vec![
            credit(BUYER, 8_175_000_000, "af88fb62ab13cc04"),
            credit(SELLER, 3_000_400_000, "8b39816c6951d9cc"),
        ],
    ));
    let block = &value["conservation"];

    assert_eq!(block["status"], "conserved");
    assert_eq!(block["funded_in"], "12200000000");
    assert_eq!(block["declared_payout"], "11175400000");
    assert_eq!(block["credited_to_notes"], "11175400000");
    // The identity is checked between two different accounts: the payout term is what the NOTES
    // reported, not what the deal said about itself.
    assert_eq!(block["payout"], "11175400000");
    assert_eq!(block["payout_source"], "note_deal_credited");
    // The 1.0246 SHELL could not place, now a named term with a stated basis.
    assert_eq!(block["written_off"], "1024600000");
    assert_eq!(block["written_off_basis"], "implied_by_conservation");
    assert_eq!(block["unexplained"], "0");

    // Every funding term cites the transaction it came from -- the issue's acceptance condition.
    let funding = block["funding"].as_array().expect("funding terms");
    assert_eq!(funding.len(), 3);
    assert_eq!(funding[0]["kind"], "escrow_funded");
    assert_eq!(funding[0]["message_id"], "d16efc22830bc94e");
    assert_eq!(funding[1]["kind"], "buyer_bond_funded");
    assert_eq!(funding[1]["amount"], "2000000000");
    assert_eq!(funding[1]["message_id"], "a0d31cbe7ff4bba5");
    assert_eq!(funding[2]["kind"], "seller_bond_funded");
}

/// The 2.025 SHELL. `deposit - refund` is published as `net_excluding_bond` and named, beside the
/// real figure, so the reading that opened this issue cannot be made silently again.
#[test]
fn the_receipt_states_the_buyers_whole_debit_not_only_the_escrow() {
    let value = conservation(&chain(
        deal_1417_events(),
        vec![
            credit(BUYER, 8_175_000_000, "af88fb62ab13cc04"),
            credit(SELLER, 3_000_400_000, "8b39816c6951d9cc"),
        ],
    ));
    let buyer = &value["conservation"]["buyer_position"];

    assert_eq!(buyer["deposit"], "8200000000");
    assert_eq!(buyer["bond"], "2000000000");
    assert_eq!(buyer["total_debit"], "10200000000");
    assert_eq!(buyer["credited_back"], "8175000000");
    assert_eq!(buyer["net"], "-2025000000");
    assert_eq!(buyer["net_excluding_bond"], "-25000000");
}

/// The live `ProbeBurned` deal: there the DEAL declares the burn, so the receipt must say so rather
/// than presenting the same figure as something it inferred.
#[test]
fn a_probe_burned_deal_reports_the_burn_the_deal_itself_declared() {
    let value = conservation(&chain(
        vec![
            event(
                "f00559045af5aa4f",
                1_787_204_877,
                TokenContractSettlementEvent::StreamFunded {
                    buyer: BUYER.to_string(),
                    deposit: 2_050_000_000,
                },
            ),
            event(
                "d17dfc886ceaaf4d",
                1_787_204_878,
                TokenContractSettlementEvent::BuyerBondFunded {
                    amount: 2_000_000_000,
                },
            ),
            event(
                "484eca25873cc04c",
                1_787_204_892,
                TokenContractSettlementEvent::SellerBondFunded {
                    amount: 2_000_000_000,
                },
            ),
            event(
                "ccf3418211ba3835",
                1_787_204_975,
                TokenContractSettlementEvent::ProbeBurned {
                    buyer: BUYER.to_string(),
                    burned_probe: 1_000_000_000,
                    burned_bond: 1_000_000_000,
                    refund_to_buyer: 3_050_000_000,
                },
            ),
        ],
        vec![
            credit(BUYER, 3_050_000_000, "c65ea8c6579cfded"),
            credit(SELLER, 1_000_000_000, "ea415515fc493d57"),
        ],
    ));
    let block = &value["conservation"];

    assert_eq!(block["status"], "conserved");
    assert_eq!(block["funded_in"], "6050000000");
    // `1911746c0788e464...` on mainnet reported exactly this as one `reportDealWriteOff`.
    assert_eq!(block["written_off"], "2000000000");
    assert_eq!(block["declared_write_off"], "2000000000");
    assert_eq!(block["written_off_basis"], "declared_by_terminal_event");
    assert_eq!(block["credited_to_notes"], "4050000000");
    assert_eq!(block["unexplained"], "0");
}

// -------------------------------------------------------------------------
// Negative controls.
// -------------------------------------------------------------------------

/// THE DEFECT ITSELF. At cut `c95e2fea` the decoder did not read `BuyerBondFunded`, so the
/// inflow side was short by the whole bond. With that one event removed the receipt must NOT say
/// `conserved` -- it must report the deal as unbalanced by exactly the bond and raise the issue.
#[test]
fn a_receipt_blind_to_the_buyer_bond_is_reported_unbalanced() {
    let events: Vec<_> = deal_1417_events()
        .into_iter()
        .filter(|receipt| {
            !matches!(
                receipt.event,
                TokenContractSettlementEvent::BuyerBondFunded { .. }
            )
        })
        .collect();
    let value = conservation(&chain(
        events,
        vec![
            credit(BUYER, 8_175_000_000, "af88fb62ab13cc04"),
            credit(SELLER, 3_000_400_000, "8b39816c6951d9cc"),
        ],
    ));
    let block = &value["conservation"];

    assert_eq!(block["status"], "unbalanced");
    assert_eq!(block["funded_in"], "10200000000");
    // The notes were credited more than the deal, so read, was ever funded. No burn can reconcile
    // that, and the shortfall is reported signed instead of saturating to a comforting zero.
    assert_eq!(block["written_off"], "0");
    assert_eq!(block["unexplained"], "-975400000");
    assert!(block["missing"]
        .as_array()
        .expect("missing")
        .iter()
        .any(|reason| reason == "declared_payout_exceeds_funding"));
    assert!(value["consistency_issues"]
        .as_array()
        .expect("issues")
        .iter()
        .any(|issue| issue == "deal_money_not_conserved"));
}

/// A deal with no terminal settlement event has no money statement to check. That must read as
/// `incomplete` and say why -- never as `conserved`, which would be a claim nothing supports.
#[test]
fn a_deal_with_no_terminal_event_is_incomplete_not_conserved() {
    let events: Vec<_> = deal_1417_events()
        .into_iter()
        .filter(|receipt| {
            !matches!(
                receipt.event,
                TokenContractSettlementEvent::StreamStopped { .. }
            )
        })
        .collect();
    let value = conservation(&chain(events, Vec::new()));
    let block = &value["conservation"];

    assert_eq!(block["status"], "incomplete");
    assert_eq!(block["written_off_basis"], "unestablished");
    assert!(block["missing"]
        .as_array()
        .expect("missing")
        .iter()
        .any(|reason| reason == "terminal_settlement_event_absent"));
    assert!(value["consistency_issues"]
        .as_array()
        .expect("issues")
        .iter()
        .any(|issue| issue == "deal_money_conservation_incomplete"));
}

/// The deal's own terminal split and the notes' credits are independent statements. When they
/// disagree by even one unit that is a finding, reported with both figures -- never resolved by
/// preferring the prettier one, and never rounded away as dust.
#[test]
fn a_terminal_split_the_notes_do_not_confirm_is_reported_unbalanced() {
    let value = conservation(&chain(
        deal_1417_events(),
        vec![
            credit(BUYER, 8_175_000_000, "af88fb62ab13cc04"),
            credit(SELLER, 3_000_399_999, "8b39816c6951d9cc"),
        ],
    ));
    let block = &value["conservation"];

    assert_eq!(block["status"], "unbalanced");
    assert_eq!(block["declared_payout"], "11175400000");
    assert_eq!(block["credited_to_notes"], "11175399999");
    assert!(block["missing"]
        .as_array()
        .expect("missing")
        .iter()
        .any(|reason| reason == "declared_payout_disagrees_with_note_credits"));
    assert!(value["consistency_issues"]
        .as_array()
        .expect("issues")
        .iter()
        .any(|issue| issue == "deal_money_not_conserved"));
}
