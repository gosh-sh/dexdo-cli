use super::args::SettlementReceiptArgs;
use anyhow::Result;

use anyhow::Context;
use dexdo_core::{
    buyer_net_result, buyer_total_debit, Deployed, RealChainBackend, TokenContractCurrentFacts,
    TokenContractReceiptChainData, TokenContractSettlementEvent, TokenContractSettlementReceipt,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
#[path = "settlement_receipt_conservation_1417.rs"]
mod settlement_receipt_conservation_1417;
// `#[cfg(test)]` alone. This arrived as `all(test, feature = "...")` naming a cargo feature this tree
// no longer declares, so the module compiled to nothing: the
// money defect it pins -- a conservation verdict reading `conserved` when nothing was
// cross-checked -- was guarded by a test that never ran, in the default gate or anywhere else.
// Found twice independently, here and in, which is the shape of a defect that leaves no
// trace: a `cfg` naming something that does not exist excludes the code without a word.
#[cfg(test)]
#[path = "settlement_receipt_unverified_conservation_1417.rs"]
mod settlement_receipt_unverified_conservation_1417;

#[cfg(test)]
#[path = "settlement_receipt_exit_code_1785.rs"]
mod settlement_receipt_exit_code_1785;

// Also the home of the ABI-keyed `getState` payload builder the fixtures below use, so the
// names this file's tests feed the parser come from `contracts/compiled/airegistry/
// TokenContract.abi.json` and cannot be a contract generation out of date on their own.
#[cfg(test)]
#[path = "settlement_receipt_getstate_shape_1560.rs"]
mod settlement_receipt_getstate_shape_1560;

const RECEIPT_SCHEMA: &str = "dexdo.settlement-receipt.v1";
const PROOF_LEVEL: &str = "chain_event_observed";
const REWARDS_SCHEMA: &str = "dexdo.note-rewards.v1";
const REWARDS_SOURCE: &str = "dexdo-points-rewards";
const REWARDS_SEMANTICS: &str = "note_season_aggregate_not_per_deal";

#[derive(Debug, Serialize)]
struct SettlementReceiptV1 {
    schema: &'static str,
    generated_at: u64,
    proof_level: &'static str,
    network: NetworkReceipt,
    token_contract: TokenContractIdentity,
    parties: PartiesReceipt,
    deal: DealReceipt,
    current: Option<CurrentReceipt>,
    terminal: TerminalReceipt,
    outcome: OutcomeReceipt,
    settlement_sequence: Vec<EventReceipt>,
    /// Omitted entirely -- not emitted as `null` -- when the chain could not be read: this
    /// receipt already distinguishes "read and found nothing" from "could not read", and a money
    /// identity printed as `null` would claim the first while meaning the second.
    #[serde(skip_serializing_if = "Option::is_none")]
    conservation: Option<ConservationReceipt>,
    withdrawal: WithdrawalReceipt,
    rewards: RewardsJoinReceipt,
    consistency_issues: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NetworkReceipt {
    name: String,
    chain_endpoint: String,
    contracts_generation: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenContractIdentity {
    address: String,
    account_status: &'static str,
    contract_version: Option<ContractVersionReceipt>,
    code_identity: CodeIdentityReceipt,
}

#[derive(Debug, Clone, Serialize)]
struct ContractVersionReceipt {
    version: String,
    contract: String,
}

#[derive(Debug, Serialize)]
struct CodeIdentityReceipt {
    actual_code_hash: Option<String>,
    manifest_expected_code_hash: Option<String>,
    matches_manifest: Option<bool>,
}

#[derive(Debug, Serialize)]
struct PartiesReceipt {
    buyer_note: PartyReceipt,
    seller_note: PartyReceipt,
}

#[derive(Debug, Serialize)]
struct PartyReceipt {
    role: &'static str,
    address: Option<String>,
    source: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct DealReceipt {
    terms: Option<DealTermsReceipt>,
    asset: AssetReceipt,
}

#[derive(Debug, Clone, Serialize)]
struct DealTermsReceipt {
    tick_size: String,
    price_per_tick: String,
    max_ticks: String,
}

#[derive(Debug, Serialize)]
struct AssetReceipt {
    symbol: &'static str,
    ecc_id: u8,
}

#[derive(Debug, Serialize)]
struct CurrentReceipt {
    state: CurrentStateReceipt,
    fees: CurrentFeesReceipt,
    seller: CurrentSellerReceipt,
}

/// The deal's own `getState()`, in the order the compiled ABI declares it.

/// The four fields this block used to carry -- `prepaid`, `frozen`, `prepaidTime`, `lastAdvance`
/// -- are the pre-4.0.31 prepaid/frozen buffer, which no longer exists: "There is no prepaid/frozen
/// buffer beyond the probe: a claim earns money by outliving its own window, not by being paid in
/// advance" (`contracts/airegistry/TokenContract.sol`, the contract header). What survived of that
/// buffer is the probe -- `_frozen` at `open()` became `_probeTick`, and `_prepaidTime` became
/// `_probeTime` -- while `_lastAdvance`, the anchor of the last seller-side step, became
/// `_lastClaimTime`. `prepaid` itself has NO successor: the delivered-not-yet-finalized quantity is
/// now a cumulative TOKEN count (`tokensFinal`/`tokensPending`), not a SHELL earmark, so it is
/// reported as what it is rather than restated under a money field's name.
#[derive(Debug, Clone, Serialize)]
struct CurrentStateReceipt {
    funded: bool,
    opened: bool,
    probe_accepted: bool,
    disputed: bool,
    deposit: String,
    /// SHELL held as the unaccepted probe. Owed to nobody until `acceptProbe`.
    probe_tick: String,
    finalized_owed: String,
    /// Promoted cumulative consumption in TOKENS -- the only figure money is computed from.
    tokens_final: String,
    /// The one claimed cumulative consumption still inside its contest window, in TOKENS.
    tokens_pending: String,
    probe_time: u64,
    last_claim_time: u64,
    dispute_time: u64,
    funded_time: u64,
}

#[derive(Debug, Clone, Serialize)]
struct CurrentFeesReceipt {
    fee_accrued: String,
    ticks_finalized: String,
    ever_disputed: bool,
    rebate_max_bps: u64,
    rebate_slope_bps: u64,
}

#[derive(Debug, Clone, Serialize)]
struct CurrentSellerReceipt {
    seller_pubkey: String,
    root_model_address: String,
    nonce: u64,
}

#[derive(Debug)]
struct ParsedCurrent {
    receipt: CurrentReceipt,
    deal: DealTermsReceipt,
    buyer_note: Option<String>,
    seller_note: String,
    version: ContractVersionReceipt,
}

#[derive(Debug, Clone, Serialize)]
struct IndexerOrderReceipt {
    created_at: u64,
    cursor: String,
    message_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct EventReceipt {
    kind: &'static str,
    payload: Value,
    message_id: String,
    created_at: u64,
    indexer_order: IndexerOrderReceipt,
}

#[derive(Debug, Serialize)]
struct TerminalReceipt {
    status: &'static str,
    kind: Option<&'static str>,
    payload: Option<Value>,
    message_id: Option<String>,
    created_at: Option<u64>,
    indexer_order: Option<IndexerOrderReceipt>,
}

/// WHAT HAPPENED TO THE MONEY, and from whose statement.

/// `terminal` above answers a different question -- did the DEAL post a settlement event -- and on one
/// real path the honest answer to that question is "no" while the escrow has nevertheless been
/// returned in full. `cleanupUnopened` emits no settlement event at all; the contract says so where
/// the event used to be declared. All the deal leaves is `ContractDestroyed`, which proves
/// destruction and nothing about money, so a receipt built only from the deal's own events reports
/// `not_final` on a deal that is completely settled. That is not a missing event to be papered over:
/// it is the receipt reading the side that does not report money.

/// So this block reads the side that does. The note announces what it received
/// (`PrivateNote.DealCredited`), and `status` says which statement the figure came from:

/// * `settled` -- the deal posted its own terminal settlement; `amount` is its refund figure;
/// * `returned` -- the deal said nothing, and the note reports being credited for this deal;
/// * `divergent` -- BOTH spoke and they disagree. Reported as a finding, never resolved by
/// choosing: two chain statements about one deal that do not match is exactly
/// what an owner needs told, and a receipt that picked the prettier one would be
/// inventing a third truth;
/// * `undetermined` -- nothing established it. `missing` names what was absent, so the gap is a
/// stated gap rather than a silence.
#[derive(Debug, Serialize)]
struct OutcomeReceipt {
    status: &'static str,
    /// Raw ECC[2] SHELL, as a decimal string like every other figure in this receipt. `None` only
    /// when nothing established one.
    amount: Option<String>,
    /// Which statement `amount` came from: `deal_terminal_event` or `note_deal_credited`.
    source: Option<&'static str>,
    /// The deal's own refund figure, when it posted one -- present alongside `note_amount` on a
    /// divergence so both halves of the disagreement are in the receipt.
    deal_amount: Option<String>,
    /// The total the notes report being credited for this deal.
    note_amount: Option<String>,
    /// Every note credit that fed `note_amount`, with the provenance the deal's own events carry.
    note_credits: Vec<NoteCreditReceipt>,
    /// The note accounts that were read at all. Empty means no counterparty was identified -- a
    /// different answer from "read and found nothing", and the two must not look alike.
    notes_read: Vec<String>,
    /// Machine-readable reasons the outcome is `undetermined`.
    missing: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NoteCreditReceipt {
    note: String,
    amount: String,
    message_id: String,
    created_at: u64,
    indexer_order: IndexerOrderReceipt,
}

#[derive(Debug, Serialize)]
struct WithdrawalReceipt {
    status: &'static str,
    events: Vec<EventReceipt>,
    observed_amount: String,
    finalized_owed: Option<String>,
}

#[derive(Debug, Serialize)]
struct RewardsJoinReceipt {
    schema: &'static str,
    source: &'static str,
    requested_season: Option<u32>,
    semantics: &'static str,
    queries: Vec<RewardsQueryReceipt>,
}

#[derive(Debug, Serialize)]
struct RewardsQueryReceipt {
    role: &'static str,
    participant_note: Option<String>,
    query_path: Option<String>,
}

/// where every unit of this deal's SHELL came from and went.

/// `TokenContract` holds the deal's money as a scalar `_balance` and moves it with exactly three
/// primitives -- `_balance +=` on the three funding entries, `_payShell` to a note, `_burnShell` to
/// RootPN. So the deal's whole life is one identity:

/// ```text
/// escrow + seller bond + buyer bond == credited to notes + written off
/// ```

/// and, because the outflow side has only those two primitives, whatever was funded and did not
/// reach a note **was burned**. That is what makes `written_off` an accounting result rather than a
/// leftover, and it is why this block never reports a remainder it cannot name.
#[derive(Debug, Serialize)]
struct ConservationReceipt {
    /// `conserved`, `unbalanced`, `incomplete` or `unverified`. `incomplete` means a term could not
    /// be read at all, which is a different answer from a mismatch and must not be dressed up as
    /// one; `unverified` means the identity had only ONE account's word for both of its sides, so it
    /// closed by construction and states nothing about the money.
    status: &'static str,
    /// Which money this verdict is about, said out loud so `conserved` is not read as a statement
    /// about everything the deal touched. The deal's SHELL is a scalar `uint128 _balance` inside
    /// `TokenContract`; native `vmshell` gas is a different balance on a different plane and no term
    /// here counts it. A defect that moves ECC[2] into the native pocket -- the 4.0.36 class, where
    /// a transfer succeeds, the intent is right and the reserve never grows -- is invisible to every
    /// figure in this block.
    covers: &'static str,
    funded_in: String,
    /// Every funding message, so each term of `funded_in` is traceable to one transaction.
    funding: Vec<ConservationTermReceipt>,
    credited_to_notes: String,
    written_off: String,
    /// How `written_off` was established: implied by the identity, or declared by the deal itself.
    written_off_basis: &'static str,
    /// The burn the terminal event declared, where it declares one (`ProbeBurned`). Present next to
    /// `written_off` so the two can be compared rather than merged.
    declared_write_off: Option<String>,
    /// The payout the terminal event declared, where it declares both legs.
    declared_payout: Option<String>,
    /// The payout term actually used in the identity.
    payout: Option<String>,
    /// Which statement `payout` came from: `note_deal_credited` or `deal_terminal_event`. The notes
    /// are preferred, so the identity is checked between two different accounts wherever possible.
    payout_source: &'static str,
    /// Raw ECC[2] SHELL that no term accounts for. Zero on a settled deal, and never rounded.
    unexplained: String,
    buyer_position: Option<BuyerPositionReceipt>,
    /// Machine-readable reasons `status` is not `conserved`.
    missing: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ConservationTermReceipt {
    kind: &'static str,
    amount: String,
    message_id: String,
    created_at: u64,
}

/// What the buyer actually paid for one deal -- the figure was missing.

/// `deposit` is the fee-inclusive escrow and NOTHING ELSE. Since 4.0.35 an ordinary deal also debits
/// the buyer's note `BUYER_BOND = 2P` in a separate `fundBuyerBond` message, so `deposit -
/// refund_to_buyer` is not the buyer's result and never was. Reading it as one understated the
/// buyer's outlay by the whole bond and made a correct settlement look short by exactly
/// `2 000 000 000`. `total_debit` and `net` are published so no reader has to derive them.
#[derive(Debug, Serialize)]
struct BuyerPositionReceipt {
    deposit: String,
    bond: String,
    total_debit: String,
    credited_back: String,
    /// Signed, in raw ECC[2] SHELL. Negative is the ordinary case: it is the price of the service.
    net: String,
    /// `deposit - credited_back` -- the figure a reader computes when the bond is invisible. Printed
    /// so the trap is named in the receipt instead of being rediscovered.
    net_excluding_bond: String,
}

struct ReceiptContext {
    generated_at: u64,
    network: String,
    chain_endpoint: String,
    contracts_generation: Option<String>,
    expected_code_hash: Option<String>,
    token_contract: String,
    season: Option<u32>,
}

fn normalized_code_hash(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    (!value.is_empty()).then(|| value.to_ascii_lowercase())
}

fn decimal_u128(value: &Value, field: &str) -> Option<String> {
    let raw = value.get(field)?;
    let parsed = match raw {
        Value::String(value) => value.parse::<u128>().ok()?,
        Value::Number(value) => value.to_string().parse::<u128>().ok()?,
        _ => return None,
    };
    Some(parsed.to_string())
}

fn integer_u64(value: &Value, field: &str) -> Option<u64> {
    let raw = value.get(field)?;
    match raw {
        Value::String(value) => value.parse().ok(),
        Value::Number(value) => value.as_u64(),
        _ => None,
    }
}

fn boolean(value: &Value, field: &str) -> Option<bool> {
    value.get(field)?.as_bool()
}

fn string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalized_address(value: &str) -> Option<String> {
    dexdo_core::Address::parse(value)
        .ok()
        .map(|address| address.with_workchain())
}

fn optional_nonzero_address(value: &Value, field: &str) -> Option<Option<String>> {
    let raw = value.get(field)?.as_str()?.trim();
    if raw.is_empty() {
        return Some(None);
    }
    let address = normalized_address(raw)?;
    let bare = address
        .split_once(':')
        .map(|(_, bare)| bare)
        .unwrap_or(address.as_str());
    Some((!bare.chars().all(|character| character == '0')).then_some(address))
}

/// `getState` is read through the workspace's one strict decoder rather than a second
/// hand-written field list, so `facts` arrives already decoded from `current.state` -- the output of
/// exactly the call that decoder is written for (`ChainClient::token_contract_state`), with its
/// names pinned against the compiled `TokenContract.abi.json` by
/// `the_deal_state_decoder_matches_the_compiled_getstate`. This receipt therefore cannot go a
/// contract generation out of date on its own again, which is how it came to declare every live
/// deal `inconsistent`.
fn parse_current(
    current: &TokenContractCurrentFacts,
    facts: dexdo_core::DealChainState,
) -> Option<ParsedCurrent> {
    let state = CurrentStateReceipt {
        funded: facts.funded,
        opened: facts.opened,
        probe_accepted: facts.probe_accepted,
        disputed: facts.disputed,
        deposit: facts.deposit.to_string(),
        probe_tick: facts.probe_tick.to_string(),
        finalized_owed: facts.finalized_owed.to_string(),
        tokens_final: facts.tokens_final.to_string(),
        tokens_pending: facts.tokens_pending.to_string(),
        probe_time: facts.probe_time,
        last_claim_time: facts.last_claim_time,
        dispute_time: facts.dispute_time,
        // The decoder reports "never funded" as `None`; the getter reports it as `0` and this
        // receipt keeps the getter's own reading.
        funded_time: facts.funded_time.unwrap_or_default(),
    };
    let fees = CurrentFeesReceipt {
        fee_accrued: decimal_u128(&current.fees, "feeAccrued")?,
        ticks_finalized: decimal_u128(&current.fees, "ticksFinalized")?,
        ever_disputed: boolean(&current.fees, "everDisputed")?,
        rebate_max_bps: integer_u64(&current.fees, "rebateMaxBps")?,
        rebate_slope_bps: integer_u64(&current.fees, "rebateSlopeBps")?,
    };
    let deal = DealTermsReceipt {
        tick_size: decimal_u128(&current.deal, "tickSize")?,
        price_per_tick: decimal_u128(&current.deal, "pricePerTick")?,
        max_ticks: decimal_u128(&current.deal, "maxTicks")?,
    };
    let buyer_note = optional_nonzero_address(&current.parties, "buyer")?;
    let seller_note = normalized_address(current.parties.get("sellerNote")?.as_str()?)?;
    let seller = CurrentSellerReceipt {
        seller_pubkey: string(&current.seller, "sellerPubkey")
            .or_else(|| string(&current.seller, "value0"))?,
        root_model_address: normalized_address(current.seller.get("rootModelAddress")?.as_str()?)?,
        nonce: integer_u64(&current.seller, "nonce")?,
    };
    let version = ContractVersionReceipt {
        version: string(&current.version, "value0")?,
        contract: string(&current.version, "value1")?,
    };
    Some(ParsedCurrent {
        receipt: CurrentReceipt {
            state,
            fees,
            seller,
        },
        deal,
        buyer_note,
        seller_note,
        version,
    })
}

fn event_kind_payload(event: &TokenContractSettlementEvent) -> (&'static str, Value) {
    match event {
        TokenContractSettlementEvent::ContractDeployed { token_contract } => {
            ("ContractDeployed", json!({"self": token_contract}))
        }
        TokenContractSettlementEvent::StreamFunded { buyer, deposit } => (
            "StreamFunded",
            json!({"buyer": buyer, "deposit": deposit.to_string()}),
        ),
        TokenContractSettlementEvent::SellerBondFunded { amount } => {
            ("SellerBondFunded", json!({"amount": amount.to_string()}))
        }
        TokenContractSettlementEvent::BuyerBondFunded { amount } => {
            ("BuyerBondFunded", json!({"amount": amount.to_string()}))
        }
        TokenContractSettlementEvent::StreamOpened {
            buyer,
            price_per_tick,
        } => (
            "StreamOpened",
            json!({"buyer": buyer, "pricePerTick": price_per_tick.to_string()}),
        ),
        TokenContractSettlementEvent::ProbeAccepted {
            buyer,
            to_seller,
            bond_returned,
        } => (
            "ProbeAccepted",
            json!({
                "buyer": buyer,
                "toSeller": to_seller.to_string(),
                "bondReturned": bond_returned.to_string(),
            }),
        ),
        TokenContractSettlementEvent::ProbeBurned {
            buyer,
            burned_probe,
            burned_bond,
            refund_to_buyer,
        } => (
            "ProbeBurned",
            json!({
                "buyer": buyer,
                "burnedProbe": burned_probe.to_string(),
                "burnedBond": burned_bond.to_string(),
                "refundToBuyer": refund_to_buyer.to_string(),
            }),
        ),
        TokenContractSettlementEvent::TickFinalized {
            finalized_owed,
            deposit,
        } => (
            "TickFinalized",
            json!({
                "finalizedOwed": finalized_owed.to_string(),
                "deposit": deposit.to_string(),
            }),
        ),
        TokenContractSettlementEvent::TicksClaimed { trusted, claimed } => (
            "TicksClaimed",
            json!({
                "trusted": trusted.to_string(),
                "claimed": claimed.to_string(),
            }),
        ),
        TokenContractSettlementEvent::StreamStopped {
            buyer,
            to_seller,
            refund_to_buyer,
        } => (
            "StreamStopped",
            json!({
                "buyer": buyer,
                "toSeller": to_seller.to_string(),
                "refundToBuyer": refund_to_buyer.to_string(),
            }),
        ),
        TokenContractSettlementEvent::StreamDisputed { buyer, at } => {
            ("StreamDisputed", json!({"buyer": buyer, "at": at}))
        }
        TokenContractSettlementEvent::DisputeResolved {
            to_seller,
            refund_to_buyer,
            released,
        } => (
            "DisputeResolved",
            json!({
                "toSeller": to_seller.to_string(),
                "refundToBuyer": refund_to_buyer.to_string(),
                "released": released,
            }),
        ),
        TokenContractSettlementEvent::StreamReclaimed {
            buyer,
            refund_to_buyer,
        } => (
            "StreamReclaimed",
            json!({
                "buyer": buyer,
                "refundToBuyer": refund_to_buyer.to_string(),
            }),
        ),
        TokenContractSettlementEvent::ShellWithdrawn { recipient, amount } => (
            "ShellWithdrawn",
            json!({"recipient": recipient, "amount": amount.to_string()}),
        ),
        TokenContractSettlementEvent::ContractDestroyed { token_contract } => {
            ("ContractDestroyed", json!({"self": token_contract}))
        }
    }
}

fn rendered_event(receipt: &TokenContractSettlementReceipt) -> EventReceipt {
    let (kind, payload) = event_kind_payload(&receipt.event);
    let indexer_order = IndexerOrderReceipt {
        created_at: receipt.created_at,
        cursor: receipt.cursor.clone(),
        message_id: receipt.message_id.clone(),
    };
    EventReceipt {
        kind,
        payload,
        message_id: receipt.message_id.clone(),
        created_at: receipt.created_at,
        indexer_order,
    }
}

fn is_terminal(event: &TokenContractSettlementEvent) -> bool {
    matches!(
        event,
        TokenContractSettlementEvent::ProbeBurned { .. }
            | TokenContractSettlementEvent::StreamStopped { .. }
            | TokenContractSettlementEvent::DisputeResolved { .. }
            | TokenContractSettlementEvent::StreamReclaimed { .. }
    )
}

/// The refund the DEAL itself posted on its terminal event. Every terminal kind carries one, and it
/// already includes the buyer bond: each of these paths calls `_releaseBuyerBond`, which folds the
/// surviving bond back into the deposit before the figure is computed. That is what makes it
/// comparable, one to one, with what the buyer's note reports being credited.
fn terminal_refund_to_buyer(event: &TokenContractSettlementEvent) -> Option<u128> {
    match event {
        TokenContractSettlementEvent::ProbeBurned {
            refund_to_buyer, ..
        }
        | TokenContractSettlementEvent::StreamStopped {
            refund_to_buyer, ..
        }
        | TokenContractSettlementEvent::DisputeResolved {
            refund_to_buyer, ..
        }
        | TokenContractSettlementEvent::StreamReclaimed {
            refund_to_buyer, ..
        } => Some(*refund_to_buyer),
        _ => None,
    }
}

fn event_buyer(event: &TokenContractSettlementEvent) -> Option<&str> {
    match event {
        TokenContractSettlementEvent::StreamFunded { buyer, .. }
        | TokenContractSettlementEvent::StreamOpened { buyer, .. }
        | TokenContractSettlementEvent::ProbeAccepted { buyer, .. }
        | TokenContractSettlementEvent::ProbeBurned { buyer, .. }
        | TokenContractSettlementEvent::StreamStopped { buyer, .. }
        | TokenContractSettlementEvent::StreamDisputed { buyer, .. }
        | TokenContractSettlementEvent::StreamReclaimed { buyer, .. } => Some(buyer),
        _ => None,
    }
}

fn event_contract(event: &TokenContractSettlementEvent) -> Option<&str> {
    match event {
        TokenContractSettlementEvent::ContractDeployed { token_contract }
        | TokenContractSettlementEvent::ContractDestroyed { token_contract } => {
            Some(token_contract)
        }
        _ => None,
    }
}

fn rewards_query(
    role: &'static str,
    participant_note: Option<String>,
    season: Option<u32>,
) -> RewardsQueryReceipt {
    let query_path = participant_note.as_ref().map(|note| match season {
        Some(season) => format!("/v1/notes/{note}?season={season}"),
        None => format!("/v1/notes/{note}"),
    });
    RewardsQueryReceipt {
        role,
        participant_note,
        query_path,
    }
}

fn unavailable_receipt(context: ReceiptContext) -> SettlementReceiptV1 {
    let buyer_note = PartyReceipt {
        role: "buyer",
        address: None,
        source: None,
    };
    let seller_note = PartyReceipt {
        role: "seller",
        address: None,
        source: None,
    };
    // The chain could not be read at all, so neither money statement was even attempted. Saying
    // that is the whole point of the block: an unread chain must not look like a silent one.
    let outcome = OutcomeReceipt {
        status: "undetermined",
        amount: None,
        source: None,
        deal_amount: None,
        note_amount: None,
        note_credits: Vec::new(),
        notes_read: Vec::new(),
        missing: vec![
            "chain_read_unavailable".to_string(),
            "counterparty_note_unidentified".to_string(),
        ],
    };
    SettlementReceiptV1 {
        schema: RECEIPT_SCHEMA,
        generated_at: context.generated_at,
        proof_level: PROOF_LEVEL,
        network: NetworkReceipt {
            name: context.network,
            chain_endpoint: context.chain_endpoint,
            contracts_generation: context.contracts_generation,
        },
        token_contract: TokenContractIdentity {
            address: context.token_contract,
            account_status: "unavailable",
            contract_version: None,
            code_identity: CodeIdentityReceipt {
                actual_code_hash: None,
                manifest_expected_code_hash: context.expected_code_hash,
                matches_manifest: None,
            },
        },
        parties: PartiesReceipt {
            buyer_note,
            seller_note,
        },
        deal: DealReceipt {
            terms: None,
            asset: AssetReceipt {
                symbol: "SHELL",
                ecc_id: 2,
            },
        },
        current: None,
        terminal: TerminalReceipt {
            status: "unavailable",
            kind: None,
            payload: None,
            message_id: None,
            created_at: None,
            indexer_order: None,
        },
        outcome,
        settlement_sequence: Vec::new(),
        conservation: None,
        withdrawal: WithdrawalReceipt {
            status: "not_applicable",
            events: Vec::new(),
            observed_amount: "0".to_string(),
            finalized_owed: None,
        },
        rewards: RewardsJoinReceipt {
            schema: REWARDS_SCHEMA,
            source: REWARDS_SOURCE,
            requested_season: context.season,
            semantics: REWARDS_SEMANTICS,
            queries: vec![
                rewards_query("buyer", None, context.season),
                rewards_query("seller", None, context.season),
            ],
        },
        consistency_issues: vec!["chain_read_unavailable".to_string()],
    }
}

fn reader_error_is_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
            error.is_connect()
                || error.is_timeout()
                || error.is_body()
                || error.status().is_some_and(|status| {
                    status == reqwest::StatusCode::NOT_FOUND || status.is_server_error()
                })
        })
    })
}

fn receipt_from_chain_result(
    context: ReceiptContext,
    result: Result<TokenContractReceiptChainData>,
) -> Result<SettlementReceiptV1> {
    match result {
        Ok(chain) => Ok(build_receipt(context, &chain)),
        Err(error) if reader_error_is_unavailable(&error) => Ok(unavailable_receipt(context)),
        Err(error) => Err(error.context("read TokenContract settlement receipt")),
    }
}

/// Build the conservation block from figures the receipt already reads.

/// The payout term is taken from the DEAL's own terminal event wherever that event declares it,
/// because the notes party to a destroyed deal cannot always be identified -- the seller's note is a
/// current getter, and a destroyed deal has none. What the notes DO report is published beside it as
/// an independent statement rather than folded into it.
fn conservation_receipt(
    events: &[TokenContractSettlementReceipt],
    terminal: Option<&TokenContractSettlementEvent>,
    chain: &TokenContractReceiptChainData,
    buyer_address: Option<&str>,
) -> ConservationReceipt {
    let mut missing = Vec::<String>::new();
    let mut funding = Vec::<ConservationTermReceipt>::new();
    let mut funded_in = 0u128;
    let mut funded_overflowed = false;
    let mut deposit = None;
    let mut buyer_bond = None;
    for receipt in events {
        let (kind, amount) = match &receipt.event {
            TokenContractSettlementEvent::StreamFunded { deposit: paid, .. } => {
                deposit = Some(*paid);
                ("escrow_funded", *paid)
            }
            TokenContractSettlementEvent::SellerBondFunded { amount } => {
                ("seller_bond_funded", *amount)
            }
            TokenContractSettlementEvent::BuyerBondFunded { amount } => {
                buyer_bond = Some(*amount);
                ("buyer_bond_funded", *amount)
            }
            _ => continue,
        };
        funding.push(ConservationTermReceipt {
            kind,
            amount: amount.to_string(),
            message_id: receipt.message_id.clone(),
            created_at: receipt.created_at,
        });
        match funded_in.checked_add(amount) {
            Some(total) => funded_in = total,
            None => funded_overflowed = true,
        }
    }
    if funded_overflowed {
        missing.push("funding_total_overflow".to_string());
    }
    if funding.is_empty() {
        missing.push("no_funding_event_observed".to_string());
    }

    let mut credited_to_notes = 0u128;
    let mut credit_overflowed = false;
    for credit in &chain.note_credits {
        match credited_to_notes.checked_add(credit.amount) {
            Some(total) => credited_to_notes = total,
            None => credit_overflowed = true,
        }
    }
    if credit_overflowed {
        missing.push("note_credit_total_overflow".to_string());
    }

    // What the deal itself said about the split. `StreamStopped`/`DisputeResolved` declare both
    // payout legs; `ProbeBurned` declares the refund and the BURN instead, so on that path the burn
    // is a chain statement and the payout is what the identity implies.
    let (declared_payout, declared_write_off) = match terminal {
        Some(TokenContractSettlementEvent::StreamStopped {
            to_seller,
            refund_to_buyer,
            ..
        })
        | Some(TokenContractSettlementEvent::DisputeResolved {
            to_seller,
            refund_to_buyer,
            ..
        }) => (to_seller.checked_add(*refund_to_buyer), None),
        Some(TokenContractSettlementEvent::ProbeBurned {
            burned_probe,
            burned_bond,
            ..
        }) => (None, burned_probe.checked_add(*burned_bond)),
        _ => (None, None),
    };

    // Where the payout term comes from. The NOTES are preferred when both parties were read,
    // because they are a statement by different accounts than the deal's own -- using the deal's
    // figure on both sides of the identity would make it balance by construction and check nothing.
    let credits_complete = chain.notes_read.len() >= 2 && !chain.note_credits.is_empty();
    let payout = if credits_complete {
        Some(credited_to_notes)
    } else {
        declared_payout
    };
    let payout_source = if credits_complete {
        "note_deal_credited"
    } else {
        "deal_terminal_event"
    };
    // Named WITH the count, because "nobody cross-checked this" and "two accounts agreed" must not
    // reach the reader as the same silence. One note read is a different fact from two that agreed.
    if !credits_complete {
        missing.push(format!(
            "payout_not_cross_checked_notes_read_{}",
            chain.notes_read.len()
        ));
    }
    let (written_off, written_off_basis) = match (declared_write_off, payout) {
        (Some(burned), _) => (Some(burned), "declared_by_terminal_event"),
        (None, Some(paid)) => (
            Some(funded_in.saturating_sub(paid)),
            "implied_by_conservation",
        ),
        (None, None) => {
            missing.push("terminal_settlement_event_absent".to_string());
            (None, "unestablished")
        }
    };

    // Signed, so a payout the funding cannot cover reads as the shortfall it is rather than
    // saturating to a comforting zero.
    let unexplained = match (written_off, payout) {
        (Some(written_off), Some(paid)) => {
            let signed = |value: u128| i128::try_from(value).unwrap_or(i128::MAX);
            Some(
                signed(funded_in)
                    .saturating_sub(signed(paid))
                    .saturating_sub(signed(written_off)),
            )
        }
        _ => None,
    };

    // The deal's own terminal split against what the notes report. Two independent statements about
    // one settlement: agreement is evidence, disagreement is a finding, and neither is resolved by
    // preferring one of them.
    let mut disagrees = false;
    if let (Some(declared), true) = (declared_payout, credits_complete) {
        if declared != credited_to_notes {
            disagrees = true;
            missing.push("declared_payout_disagrees_with_note_credits".to_string());
        }
    }
    if payout.is_some_and(|paid| paid > funded_in) {
        missing.push("declared_payout_exceeds_funding".to_string());
    }

    let buyer_position = match (deposit, buyer_address) {
        (Some(deposit), Some(buyer)) => {
            let bond = buyer_bond.unwrap_or(0);
            let credited_back = chain
                .note_credits
                .iter()
                .filter(|credit| credit.note == buyer)
                .fold(0u128, |total, credit| total.saturating_add(credit.amount));
            buyer_total_debit(deposit, bond).map(|total_debit| BuyerPositionReceipt {
                deposit: deposit.to_string(),
                bond: bond.to_string(),
                total_debit: total_debit.to_string(),
                credited_back: credited_back.to_string(),
                net: buyer_net_result(credited_back, total_debit).to_string(),
                net_excluding_bond: buyer_net_result(credited_back, deposit).to_string(),
            })
        }
        _ => None,
    };

    let status = match unexplained {
        _ if disagrees => "unbalanced",
        None => "incomplete",
        // follow-up, and it is a money-path defect rather than a wording one. With fewer than
        // two notes read there is no second statement to check the deal's own figure against:
        // `payout` IS `declared_payout`, `written_off` is `funded_in - payout`, and `unexplained`
        // is therefore `funded_in - payout - (funded_in - payout)` -- exactly 0 for every input the
        // chain can produce. Reporting that as `conserved` announces a cross-check that did not
        // happen; the identity closed against itself, and a number compared with itself is not
        // evidence about money.

        // The guard was already written -- the comment above the `payout` selection says taking the
        // deal's figure for both sides "would make it balance by construction and check nothing" --
        // but it only covered the branch that reaches for the notes. This arm covers the branch
        // that cannot.
        Some(_) if !credits_complete => "unverified",
        Some(0) => "conserved",
        Some(_) => "unbalanced",
    };
    ConservationReceipt {
        status,
        covers: "ecc2_traded_asset_only",
        funded_in: funded_in.to_string(),
        funding,
        credited_to_notes: credited_to_notes.to_string(),
        written_off: written_off
            .map(|amount| amount.to_string())
            .unwrap_or_default(),
        written_off_basis,
        declared_write_off: declared_write_off.map(|amount| amount.to_string()),
        declared_payout: declared_payout.map(|amount| amount.to_string()),
        payout: payout.map(|amount| amount.to_string()),
        payout_source,
        unexplained: unexplained
            .map(|amount| amount.to_string())
            .unwrap_or_default(),
        buyer_position,
        missing,
    }
}

fn build_receipt(
    context: ReceiptContext,
    chain: &TokenContractReceiptChainData,
) -> SettlementReceiptV1 {
    let mut issues = Vec::<String>::new();
    if chain.account_id != context.token_contract {
        issues.push("reader_account_mismatch".to_string());
    }

    let expected_code_hash = context.expected_code_hash;
    let actual_code_hash = chain.code_hash.as_deref().and_then(normalized_code_hash);
    let matches_manifest = match (actual_code_hash.as_ref(), expected_code_hash.as_ref()) {
        (Some(actual), Some(expected)) => {
            let matches = actual == expected;
            if !matches {
                issues.push("token_contract_code_hash_mismatch".to_string());
            }
            Some(matches)
        }
        _ => None,
    };
    if expected_code_hash.is_none() {
        issues.push("manifest_token_contract_code_hash_missing".to_string());
    }
    if chain.account_active && actual_code_hash.is_none() {
        issues.push("active_token_contract_code_hash_missing".to_string());
    }

    let parsed_current = match (&chain.current, chain.account_active) {
        (Some(current), true) => {
            // second half. The strict decoder already SAYS which field is wrong -- "missing
            // fields: probeTick" -- and `.ok()?` threw that sentence away, so the receipt could only
            // ever report `current_getter_shape_invalid` and naming the four dead fields cost a
            // manual comparison against the compiled ABI. The code is kept unchanged for the
            // machine and the sentence is added BESIDE it for the reader; the two sort adjacently,
            // the code first, because it is a prefix of the detail line.

            // Decoded here rather than inside `parse_current` so it happens exactly once: this is
            // the only scope that owns `issues`, and a second decode call to recover the reason
            // would be a copy that can drift from the one whose result is used.
            let decoded = match dexdo_core::DealChainState::decode_getter(&current.state) {
                Ok(state) => Some(state),
                Err(reason) => {
                    issues.push(format!("current_getter_shape_invalid: {reason}"));
                    None
                }
            };
            match decoded.and_then(|state| parse_current(current, state)) {
                Some(parsed) => Some(parsed),
                None => {
                    issues.push("current_getter_shape_invalid".to_string());
                    None
                }
            }
        }
        (None, true) => {
            issues.push("active_token_contract_getters_missing".to_string());
            None
        }
        (Some(_), false) => {
            issues.push("inactive_token_contract_has_current_getters".to_string());
            None
        }
        (None, false) => None,
    };
    if parsed_current
        .as_ref()
        .is_some_and(|current| current.version.contract != "TokenContract")
    {
        issues.push("get_version_contract_identity_mismatch".to_string());
    }
    if parsed_current.as_ref().is_some_and(|current| {
        current.deal.tick_size
            != u128::from(dexdo_core::DobParams::canonical().tick_size).to_string()
    }) {
        issues.push("deal_tick_size_mismatch".to_string());
    }

    let events = &chain.receipts.events;
    if !events
        .windows(2)
        .all(|pair| (pair[0].created_at, &pair[0].cursor) <= (pair[1].created_at, &pair[1].cursor))
    {
        issues.push("settlement_sequence_not_ordered".to_string());
    }
    let mut message_ids = BTreeSet::new();
    if events
        .iter()
        .any(|event| !message_ids.insert(event.message_id.as_str()))
    {
        issues.push("duplicate_event_message_id".to_string());
    }

    let mut observed_buyers = BTreeSet::new();
    for event in events {
        if let Some(buyer) = event_buyer(&event.event) {
            match normalized_address(buyer) {
                Some(buyer) => {
                    observed_buyers.insert(buyer);
                }
                None => issues.push("event_buyer_address_invalid".to_string()),
            }
        }
        if let Some(token_contract) = event_contract(&event.event) {
            if normalized_address(token_contract).as_deref()
                != Some(context.token_contract.as_str())
            {
                issues.push("event_token_contract_mismatch".to_string());
            }
        }
    }
    if observed_buyers.len() > 1 {
        issues.push("event_buyer_party_mismatch".to_string());
    }
    let observed_buyer = observed_buyers.first().cloned();
    if let (Some(current), Some(observed)) = (&parsed_current, &observed_buyer) {
        match current.buyer_note.as_ref() {
            Some(current_buyer) if current_buyer != observed => {
                issues.push("event_getter_buyer_mismatch".to_string())
            }
            None => issues.push("current_buyer_missing_for_observed_lifecycle".to_string()),
            _ => {}
        }
    }

    let terminal_events = events
        .iter()
        .filter(|event| is_terminal(&event.event))
        .collect::<Vec<_>>();
    if terminal_events.len() > 1 {
        issues.push("multiple_terminal_events".to_string());
    }
    let destroyed = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            matches!(
                event.event,
                TokenContractSettlementEvent::ContractDestroyed { .. }
            )
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if destroyed.len() > 1 {
        issues.push("multiple_contract_destroyed_events".to_string());
    }
    if destroyed
        .first()
        .is_some_and(|index| *index + 1 != events.len())
    {
        issues.push("event_after_contract_destroyed".to_string());
    }
    if chain.account_active && !destroyed.is_empty() {
        issues.push("destroyed_event_with_active_account".to_string());
    }

    if let (Some(terminal), Some(current)) = (terminal_events.first(), &parsed_current) {
        let state = &current.receipt.state;
        // The money still earmarked inside the deal, and nothing else. `deposit` and `probeTick`
        // are the two escrow earmarks every terminal path zeroes -- the same pair
        // `DealChainState::is_stopped` reads to tell a settled close from a funded-never-opened
        // deal. `tokensFinal`/`tokensPending` are deliberately absent: they are the cumulative
        // DELIVERY record, which a terminal path preserves rather than clears, so requiring them to
        // be zero would report every correctly settled deal as a mismatch.
        if state.opened || state.disputed || state.deposit != "0" || state.probe_tick != "0" {
            issues.push("terminal_event_current_state_mismatch".to_string());
        }
        match terminal.event {
            TokenContractSettlementEvent::ProbeBurned { .. } if state.probe_accepted => {
                issues.push("probe_burned_getter_probe_accepted".to_string())
            }
            TokenContractSettlementEvent::StreamStopped { .. } if !state.probe_accepted => {
                issues.push("stream_stopped_getter_probe_not_accepted".to_string())
            }
            _ => {}
        }
    }

    let withdrawal_receipts = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                TokenContractSettlementEvent::ShellWithdrawn { .. }
            )
        })
        .collect::<Vec<_>>();
    let mut observed_amount = 0u128;
    for event in &withdrawal_receipts {
        let TokenContractSettlementEvent::ShellWithdrawn { amount, .. } = event.event else {
            unreachable!("filtered ShellWithdrawn")
        };
        if amount == 0 {
            issues.push("zero_shell_withdrawal".to_string());
        }
        match observed_amount.checked_add(amount) {
            Some(total) => observed_amount = total,
            None => issues.push("shell_withdrawal_total_overflow".to_string()),
        }
    }

    issues.sort();
    issues.dedup();
    let has_chain_evidence = chain.account_active || !events.is_empty();
    let terminal_status = if !issues.is_empty() {
        "inconsistent"
    } else if terminal_events.len() == 1 {
        "terminal"
    } else if has_chain_evidence {
        "not_final"
    } else {
        "unavailable"
    };
    let terminal = terminal_events.first().map(|event| rendered_event(event));
    let terminal_receipt = TerminalReceipt {
        status: terminal_status,
        kind: terminal.as_ref().map(|event| event.kind),
        payload: terminal.as_ref().map(|event| event.payload.clone()),
        message_id: terminal.as_ref().map(|event| event.message_id.clone()),
        created_at: terminal.as_ref().map(|event| event.created_at),
        indexer_order: terminal.as_ref().map(|event| event.indexer_order.clone()),
    };

    let withdrawal_status = if !issues.is_empty() {
        "inconsistent"
    } else if !withdrawal_receipts.is_empty() {
        "observed"
    } else if terminal_status == "terminal" {
        "not_observed"
    } else {
        "not_applicable"
    };
    let finalized_owed = parsed_current
        .as_ref()
        .map(|current| current.receipt.state.finalized_owed.clone());
    let buyer_address = parsed_current
        .as_ref()
        .and_then(|current| current.buyer_note.clone())
        .or(observed_buyer);
    let buyer_source = if parsed_current
        .as_ref()
        .and_then(|current| current.buyer_note.as_ref())
        .is_some()
    {
        Some("current_getter")
    } else if buyer_address.is_some() {
        Some("chain_event")
    } else {
        None
    };
    let seller_address = parsed_current
        .as_ref()
        .map(|current| current.seller_note.clone());
    let seller_source = seller_address.as_ref().map(|_| "current_getter");

    // the outcome is about the BUYER's escrow, so it is built from the two statements that
    // are about the same money -- the deal's `refundToBuyer` and what the BUYER's note reports being
    // credited. Seller credits are a different figure (bond back, proceeds) and summing them in
    // would manufacture a disagreement out of two correct numbers.
    let buyer_credits = buyer_address
        .as_ref()
        .map(|buyer| {
            chain
                .note_credits
                .iter()
                .filter(|credit| &credit.note == buyer)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut note_total = 0u128;
    let mut note_total_overflowed = false;
    for credit in &buyer_credits {
        match note_total.checked_add(credit.amount) {
            Some(total) => note_total = total,
            None => note_total_overflowed = true,
        }
    }
    if note_total_overflowed {
        issues.push("note_credit_total_overflow".to_string());
    }
    let deal_refund = terminal_events
        .first()
        .and_then(|receipt| terminal_refund_to_buyer(&receipt.event));
    let has_note_statement = !buyer_credits.is_empty() && !note_total_overflowed;
    let outcome_status = match (deal_refund, has_note_statement) {
        (Some(deal), true) if deal != note_total => {
            // Both sides spoke and disagreed. Say so; do not choose.
            issues.push("outcome_amount_divergence".to_string());
            "divergent"
        }
        (Some(_), _) => "settled",
        (None, true) => "returned",
        (None, false) => "undetermined",
    };
    let mut missing = Vec::<String>::new();
    if outcome_status == "undetermined" {
        missing.push(if chain.account_active {
            "deal_still_active".to_string()
        } else {
            "deal_posted_no_terminal_settlement_event".to_string()
        });
        missing.push(if chain.notes_read.is_empty() {
            "counterparty_note_unidentified".to_string()
        } else {
            "note_reported_no_credit_for_this_deal".to_string()
        });
    }
    let outcome = OutcomeReceipt {
        status: outcome_status,
        amount: match outcome_status {
            "settled" | "divergent" => deal_refund.map(|amount| amount.to_string()),
            "returned" => Some(note_total.to_string()),
            _ => None,
        },
        source: match outcome_status {
            "settled" | "divergent" => Some("deal_terminal_event"),
            "returned" => Some("note_deal_credited"),
            _ => None,
        },
        deal_amount: deal_refund.map(|amount| amount.to_string()),
        note_amount: has_note_statement.then(|| note_total.to_string()),
        note_credits: buyer_credits
            .iter()
            .map(|credit| NoteCreditReceipt {
                note: credit.note.clone(),
                amount: credit.amount.to_string(),
                message_id: credit.message_id.clone(),
                created_at: credit.created_at,
                indexer_order: IndexerOrderReceipt {
                    created_at: credit.created_at,
                    cursor: credit.cursor.clone(),
                    message_id: credit.message_id.clone(),
                },
            })
            .collect(),
        notes_read: chain.notes_read.clone(),
        missing,
    };
    let conservation = conservation_receipt(
        events,
        terminal_events.first().map(|receipt| &receipt.event),
        chain,
        buyer_address.as_deref(),
    );
    match conservation.status {
        "unbalanced" => issues.push("deal_money_not_conserved".to_string()),
        // Only once the deal is GONE. A live deal has not settled yet, so having no terminal split
        // to check is its ordinary condition, not an inconsistency -- raising it here would put a
        // permanent finding on every healthy in-flight deal.
        "incomplete" if !chain.account_active => {
            issues.push("deal_money_conservation_incomplete".to_string())
        }
        // Same gate and the same reason: on a live deal the counterparty note may simply not have
        // been read yet. On a deal that is GONE, a settlement nobody could cross-check is a finding
        // about that settlement, not a note about the reader.
        "unverified" if !chain.account_active => {
            issues.push("deal_money_conservation_unverified".to_string())
        }
        _ => {}
    }
    // Pushed after `terminal`/`withdrawal` were decided on purpose: a divergence between the two
    // money statements is a finding about the OUTCOME, and must not silently restate the deal's own
    // terminal event or its withdrawals as inconsistent.
    issues.sort();
    issues.dedup();

    SettlementReceiptV1 {
        schema: RECEIPT_SCHEMA,
        generated_at: context.generated_at,
        proof_level: PROOF_LEVEL,
        network: NetworkReceipt {
            name: context.network,
            chain_endpoint: context.chain_endpoint,
            contracts_generation: context.contracts_generation,
        },
        token_contract: TokenContractIdentity {
            address: context.token_contract,
            account_status: if chain.account_active {
                "active"
            } else {
                "inactive"
            },
            contract_version: parsed_current
                .as_ref()
                .map(|current| current.version.clone()),
            code_identity: CodeIdentityReceipt {
                actual_code_hash,
                manifest_expected_code_hash: expected_code_hash,
                matches_manifest,
            },
        },
        parties: PartiesReceipt {
            buyer_note: PartyReceipt {
                role: "buyer",
                address: buyer_address.clone(),
                source: buyer_source,
            },
            seller_note: PartyReceipt {
                role: "seller",
                address: seller_address.clone(),
                source: seller_source,
            },
        },
        deal: DealReceipt {
            terms: parsed_current.as_ref().map(|current| current.deal.clone()),
            asset: AssetReceipt {
                symbol: "SHELL",
                ecc_id: 2,
            },
        },
        current: parsed_current.map(|current| current.receipt),
        terminal: terminal_receipt,
        outcome,
        settlement_sequence: events.iter().map(rendered_event).collect(),
        conservation: Some(conservation),
        withdrawal: WithdrawalReceipt {
            status: withdrawal_status,
            events: withdrawal_receipts
                .into_iter()
                .map(rendered_event)
                .collect(),
            observed_amount: observed_amount.to_string(),
            finalized_owed,
        },
        rewards: RewardsJoinReceipt {
            schema: REWARDS_SCHEMA,
            source: REWARDS_SOURCE,
            requested_season: context.season,
            semantics: REWARDS_SEMANTICS,
            queries: vec![
                rewards_query("buyer", buyer_address, context.season),
                rewards_query("seller", seller_address, context.season),
            ],
        },
        consistency_issues: issues,
    }
}

/// The machine-readable reasons this receipt gives for not being `conserved`, as one line.

/// Read off the receipt rather than recomputed: `unexplained` and `missing` are what the
/// conservation block already established, so the refusal names the same terms the JSON above it
/// prints and cannot drift from them.
fn conservation_detail(conservation: &ConservationReceipt) -> String {
    let mut parts = Vec::<String>::new();
    parts.push(if conservation.unexplained.is_empty() {
        "unexplained: not established".to_string()
    } else {
        format!("unexplained {} raw ECC[2] SHELL", conservation.unexplained)
    });
    parts.extend(conservation.missing.iter().cloned());
    parts.join("; ")
}

/// what this gate refuses with, as a fact about the condition rather than about the sentence.

/// `classify_error` reads the message text, and these three refusals demonstrated in one place what
/// that costs. Measured: `incomplete` matched "settlement" -- through the receipt's own reason code
/// `terminal_settlement_event_absent` -- and came out `SETTLEMENT_FAILED` with `retryable: true`, so
/// a caller was told to retry a permanent finding forever. `unbalanced` matched "balance", because
/// the word "unbalanced" contains it, and came out `INSUFFICIENT_BALANCE`: money that does not
/// conserve, reported as a note that needs topping up. The absent-block refusal matched no rule at
/// all and came out `INTERNAL`, which tells a machine "this client has a bug".

/// Three siblings of one check, three unrelated codes, none of them right. So the code is CHOSEN
/// here and the envelope is printed here, and the refusal returns `machine::printed_error()`, which
/// `main` exits on without consulting the classifier at all. That is the shape established and
/// `subscription_machine_error` already uses; rewording any sentence below cannot move any code,
/// which is the property the regressions assert.

/// Deliberately NOT fixed by rewording these messages to miss the text rules. That would be a
/// workaround for the classifier rather than a fix for the refusal, and it would break again the
/// next time anyone improved the wording.

/// One code covers every refusal, and it is exact only for `unbalanced`: two records really do
/// contradict each other there. For the rest it is the deliberate safe side, because what decides
/// retryability is a sub-cause INSIDE each verdict that the refusal cannot see -- a transient
/// timeout and a permanent 404 reach the absent-block case as byte-identical receipts, the cause
/// having been dropped before the receipt was built -- and naming the state imprecisely is cheaper
/// than telling a caller to retry a destroyed deal forever.
struct ConservationRefusal {
    code: super::machine::ErrorCode,
    cause: String,
}

/// The refusal this receipt earns under `--require-conserved`, or `None` when conservation is proven.

/// Kept separate from the printing so a regression can assert the CODE a machine consumer reads
/// rather than scraping stdout -- the same separation `machine_error` is split out for.
fn conservation_refusal(receipt: &SettlementReceiptV1) -> Option<ConservationRefusal> {
    use super::machine::ErrorCode;
    let Some(conservation) = receipt.conservation.as_ref() else {
        return Some(ConservationRefusal {
            code: ErrorCode::ContradictoryState,
            cause: "--require-conserved: no conservation block -- the chain read was unavailable, so this receipt carries no money identity to judge".to_string(),
        });
    };
    match conservation.status {
        "conserved" => None,
        "unbalanced" => Some(ConservationRefusal {
            code: ErrorCode::ContradictoryState,
            cause: format!(
                "--require-conserved: conservation unbalanced -- this deal's money does not conserve ({})",
                conservation_detail(conservation)
            ),
        }),
        // PR1787's fourth value. Named here rather than left to the catch-all below: we know this
        // verdict is coming, and a generic sentence for a state we can name is a debt, not a
        // default. Having its own arm also unlinks the two pull requests in both merge orders.
        "unverified" => Some(ConservationRefusal {
            code: ErrorCode::ContradictoryState,
            cause: format!(
                "--require-conserved: conservation unverified -- the identity had only one account's word for both of its sides, so it closed by construction and states nothing about the money ({})",
                conservation_detail(conservation)
            ),
        }),
        "incomplete" => Some(ConservationRefusal {
            code: ErrorCode::ContradictoryState,
            cause: format!(
                "--require-conserved: conservation incomplete -- a term of the money identity could not be read, so conservation was never evaluated ({})",
                conservation_detail(conservation)
            ),
        }),
        other => Some(ConservationRefusal {
            code: ErrorCode::ContradictoryState,
            cause: format!(
                "--require-conserved: conservation status {other} is not conserved"
            ),
        }),
    }
}

/// the command's exit status, decided from the receipt it has just printed.

/// The default is `Ok` on every verdict, and that is a decision rather than an omission. This
/// command emits a stable reporting object whose consumers live outside this tree, and they need
/// that object most when a deal is inconsistent; an exit code that flipped on content would stop
/// those scripts mid-run for a change none of them asked for.

/// `--require-conserved` is how an operator opts into the other rule, and that rule is strict: zero
/// only when conservation was PROVEN. `incomplete` and an absent block fail exactly as `unbalanced`
/// does, because "could not check" is not "checked and fine" -- a gate that passes what it could
/// not verify teaches its operator that a green run means nothing.
fn receipt_exit_status(receipt: &SettlementReceiptV1, require_conserved: bool) -> Result<()> {
    if !require_conserved {
        return Ok(());
    }
    let Some(refusal) = conservation_refusal(receipt) else {
        return Ok(());
    };
    let envelope =
        super::machine::MachineError::new(super::machine::OP_SETTLEMENT_RECEIPT, refusal.code)
            .with_cause(refusal.cause);
    match super::machine::print_json(&envelope) {
        Ok(()) => Err(super::machine::printed_error()),
        Err(error) => Err(error),
    }
}

pub(crate) async fn run_settlement_receipt(args: SettlementReceiptArgs) -> Result<()> {
    debug_assert!(args.json, "--json is required by clap");
    let token_contract = dexdo_core::Address::parse(&args.token_contract)
        .map_err(|error| anyhow::anyhow!("TOKEN_CONTRACT {}: {error}", args.token_contract))?;
    let token_contract_text = token_contract.with_workchain();
    let manifest = crate::cli::commands::manifest_path()?;
    let deployed =
        Deployed::load(&manifest).with_context(|| format!("load {}", manifest.display()))?;
    let endpoint = dexdo_core::resolve_endpoint(None, &deployed)?;
    let expected_code_hash = dexdo_core::chain::compiled_contract_hash("TokenContract")
        .ok()
        .as_deref()
        .and_then(normalized_code_hash);
    let backend = RealChainBackend::connect(&crate::cli::commands::manifest_path()?)?;
    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs();
    let context = ReceiptContext {
        generated_at,
        network: deployed.network,
        chain_endpoint: endpoint,
        contracts_generation: deployed.version,
        expected_code_hash,
        token_contract: token_contract_text,
        season: args.season,
    };
    let receipt = receipt_from_chain_result(
        context,
        backend
            .token_contract_receipt_chain_data(&token_contract)
            .await,
    )?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    // Printed IN FULL first, and only then does the verdict decide the exit code -- the shape
    // `doctor` already uses, where the report is rendered before the run fails on its summary. A
    // caller that asked for the gate still gets the data it came for..
    receipt_exit_status(&receipt, args.require_conserved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{TokenContractSettlementEvent::*, TokenContractSettlementReceipts};

    fn address(character: char) -> String {
        format!("0:{}", character.to_string().repeat(64))
    }

    fn context(token_contract: &str) -> ReceiptContext {
        ReceiptContext {
            generated_at: 1_700_000_000,
            network: "net-a".to_string(),
            chain_endpoint: "https://net-a.example".to_string(),
            contracts_generation: Some("4.0.29".to_string()),
            expected_code_hash: Some("ab".repeat(32)),
            token_contract: token_contract.to_string(),
            season: Some(7),
        }
    }

    #[tokio::test]
    async fn settlement_receipt_command_classifies_not_found_but_propagates_integrity_errors() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let token_contract = address('a');
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 404 fixture");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept 404 request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.expect("read 404 request");
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write 404 response");
        });
        let not_found = reqwest::Client::new()
            .get(endpoint)
            .send()
            .await
            .expect("receive 404 response")
            .error_for_status()
            .expect_err("404 must be an error");
        server.await.expect("404 fixture task");
        let unavailable =
            receipt_from_chain_result(context(&token_contract), Err(not_found.into()))
                .expect("not-found reader result remains unavailable");
        assert_eq!(unavailable.terminal.status, "unavailable");

        let error = receipt_from_chain_result(
            context(&token_contract),
            Err(anyhow::anyhow!(
                "malformed known TokenContract event payload"
            )),
        )
        .expect_err("integrity/decode errors must fail closed");
        let message = format!("{error:#}");
        assert!(
            message.contains("read TokenContract settlement receipt"),
            "{message}"
        );
        assert!(message.contains("malformed known"), "{message}");
    }

    fn current_facts(
        buyer: Option<&str>,
        seller: &str,
        opened: bool,
        probe_accepted: bool,
        disputed: bool,
        finalized_owed: u128,
    ) -> TokenContractCurrentFacts {
        TokenContractCurrentFacts {
            // the KEYS come from `contracts/compiled/airegistry/TokenContract.abi.json`, not
            // from literals here. This fixture used to name the pre-4.0.31 prepaid/frozen buffer --
            // `prepaid`, `frozen`, `prepaidTime`, `lastAdvance` -- none of which the contract has
            // declared for five generations, so it confirmed the parser against itself while every
            // live deal read `inconsistent`. Meaning is unchanged: an open deal still holds an
            // escrow and a tick's worth of money, at the same timestamps. `frozen` is now
            // `probeTick` and `lastAdvance` is now `lastClaimTime`; `prepaid` -- the delivered but
            // not yet finalized tick -- is a cumulative TOKEN count now, so it is stated as one.
            state: super::settlement_receipt_getstate_shape_1560::getstate_payload(&[
                ("funded", json!(buyer.is_some())),
                ("opened", json!(opened)),
                ("probeAccepted", json!(probe_accepted)),
                ("disputed", json!(disputed)),
                ("deposit", json!(if opened { "25" } else { "0" })),
                ("probeTick", json!(if opened { "10" } else { "0" })),
                ("finalizedOwed", json!(finalized_owed.to_string())),
                ("tokensFinal", json!("0")),
                (
                    "tokensPending",
                    json!(if opened {
                        u128::from(dexdo_core::DobParams::canonical().tick_size).to_string()
                    } else {
                        "0".to_string()
                    }),
                ),
                ("probeTime", json!("100")),
                ("lastClaimTime", json!("101")),
                ("disputeTime", json!("0")),
                ("fundedTime", json!("90")),
            ]),
            fees: json!({
                "feeAccrued": "0",
                "ticksFinalized": if probe_accepted { "3" } else { "0" },
                "everDisputed": disputed,
                "rebateMaxBps": "5000",
                "rebateSlopeBps": "100",
            }),
            deal: json!({
                "tickSize": "1000000",
                "pricePerTick": "10",
                "maxTicks": "1024",
            }),
            parties: json!({
                "buyer": buyer.unwrap_or(""),
                "sellerNote": seller,
            }),
            seller: json!({
                "sellerPubkey": "0x1234",
                "rootModelAddress": address('d'),
                "nonce": "42",
            }),
            version: json!({
                "value0": "4.0.29",
                "value1": "TokenContract",
            }),
        }
    }

    fn event(
        message_id: &str,
        created_at: u64,
        event: TokenContractSettlementEvent,
    ) -> TokenContractSettlementReceipt {
        TokenContractSettlementReceipt {
            message_id: message_id.to_string(),
            created_at,
            cursor: format!("{created_at:04}-{message_id}"),
            event,
        }
    }

    fn chain(
        token_contract: &str,
        current: Option<TokenContractCurrentFacts>,
        events: Vec<TokenContractSettlementReceipt>,
    ) -> TokenContractReceiptChainData {
        TokenContractReceiptChainData {
            account_id: token_contract.to_string(),
            account_active: current.is_some(),
            code_hash: current.as_ref().map(|_| "ab".repeat(32)),
            current,
            receipts: TokenContractSettlementReceipts { events },
            // this helper keeps meaning exactly what it meant -- a receipt built with no
            // note-side statement read. The cases that DO read one use `chain_with_note_credits`.
            note_credits: Vec::new(),
            notes_read: Vec::new(),
        }
    }

    /// The same fixture plus what the money-reporting side said. `notes_read` is separate from
    /// `note_credits` on purpose: a note that was read and reported nothing is a different fact from
    /// a note that was never identified, and the receipt has to be able to tell them apart.
    fn chain_with_note_credits(
        token_contract: &str,
        current: Option<TokenContractCurrentFacts>,
        events: Vec<TokenContractSettlementReceipt>,
        notes_read: Vec<String>,
        note_credits: Vec<dexdo_core::NoteDealCreditReceipt>,
    ) -> TokenContractReceiptChainData {
        TokenContractReceiptChainData {
            note_credits,
            notes_read,
            ..chain(token_contract, current, events)
        }
    }

    fn note_credit(
        note: &str,
        deal: &str,
        amount: u128,
        created_at: u64,
    ) -> dexdo_core::NoteDealCreditReceipt {
        dexdo_core::NoteDealCreditReceipt {
            note: note.to_string(),
            deal: deal.to_string(),
            amount,
            message_id: format!("credit-{created_at}"),
            created_at,
            cursor: format!("cursor-{created_at}"),
        }
    }

    fn as_value(receipt: &SettlementReceiptV1) -> Value {
        serde_json::to_value(receipt).expect("serialize receipt")
    }

    #[test]
    fn unopened_and_open_deals_without_terminal_event_are_not_final() {
        let token_contract = address('a');
        let seller = address('b');
        let buyer = address('c');

        let unopened = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                Some(current_facts(None, &seller, false, false, false, 0)),
                vec![event(
                    "deployed",
                    1,
                    ContractDeployed {
                        token_contract: token_contract.clone(),
                    },
                )],
            ),
        );
        assert_eq!(unopened.terminal.status, "not_final");
        assert_eq!(unopened.withdrawal.status, "not_applicable");

        let opened = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                Some(current_facts(Some(&buyer), &seller, true, false, false, 0)),
                vec![
                    event(
                        "funded",
                        1,
                        StreamFunded {
                            buyer: buyer.clone(),
                            deposit: 45,
                        },
                    ),
                    event(
                        "opened",
                        2,
                        StreamOpened {
                            buyer,
                            price_per_tick: 10,
                        },
                    ),
                ],
            ),
        );
        assert_eq!(opened.terminal.status, "not_final");
        assert!(opened.consistency_issues.is_empty());
    }

    #[test]
    fn every_terminal_kind_is_machine_visible_with_exact_payload() {
        let token_contract = address('a');
        let seller = address('b');
        let buyer = address('c');
        let cases = [
            (
                ProbeBurned {
                    buyer: buyer.clone(),
                    burned_probe: 1,
                    burned_bond: 2,
                    refund_to_buyer: 3,
                },
                "ProbeBurned",
                json!({
                    "buyer": buyer,
                    "burnedProbe": "1",
                    "burnedBond": "2",
                    "refundToBuyer": "3",
                }),
                false,
            ),
            (
                StreamStopped {
                    buyer: buyer.clone(),
                    to_seller: 4,
                    refund_to_buyer: 5,
                },
                "StreamStopped",
                json!({"buyer": buyer, "toSeller": "4", "refundToBuyer": "5"}),
                true,
            ),
            (
                DisputeResolved {
                    to_seller: 6,
                    refund_to_buyer: 7,
                    released: true,
                },
                "DisputeResolved",
                json!({"toSeller": "6", "refundToBuyer": "7", "released": true}),
                true,
            ),
            (
                StreamReclaimed {
                    buyer: buyer.clone(),
                    refund_to_buyer: 8,
                },
                "StreamReclaimed",
                json!({"buyer": buyer, "refundToBuyer": "8"}),
                true,
            ),
        ];

        for (terminal, kind, payload, probe_accepted) in cases {
            let receipt = build_receipt(
                context(&token_contract),
                &chain(
                    &token_contract,
                    Some(current_facts(
                        Some(&buyer),
                        &seller,
                        false,
                        probe_accepted,
                        false,
                        9,
                    )),
                    vec![event("terminal", 1, terminal)],
                ),
            );
            assert_eq!(receipt.terminal.status, "terminal", "{kind}");
            assert_eq!(receipt.terminal.kind, Some(kind), "{kind}");
            assert_eq!(receipt.terminal.payload, Some(payload), "{kind}");
            assert_eq!(
                receipt
                    .terminal
                    .indexer_order
                    .as_ref()
                    .map(|order| order.cursor.as_str()),
                Some("0001-terminal")
            );
        }
    }

    #[test]
    fn withdrawal_distinguishes_not_observed_observed_and_partial() {
        let token_contract = address('a');
        let seller = address('b');
        let buyer = address('c');
        let stopped = event(
            "stopped",
            1,
            StreamStopped {
                buyer: buyer.clone(),
                to_seller: 10,
                refund_to_buyer: 0,
            },
        );
        let current = current_facts(Some(&buyer), &seller, false, true, false, 7);
        let not_observed = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                Some(current.clone()),
                vec![stopped.clone()],
            ),
        );
        assert_eq!(not_observed.withdrawal.status, "not_observed");
        assert_eq!(not_observed.withdrawal.observed_amount, "0");
        let pre_withdrawal = as_value(&not_observed);
        assert_eq!(pre_withdrawal["parties"]["buyer_note"]["address"], buyer);
        assert_eq!(pre_withdrawal["parties"]["seller_note"]["address"], seller);
        assert!(pre_withdrawal["deal"]["terms"].is_object());
        let pre_buyer_rewards_path = pre_withdrawal["rewards"]["queries"][0]["query_path"]
            .as_str()
            .expect("pre-withdraw buyer rewards path")
            .to_string();
        let pre_seller_rewards_path = pre_withdrawal["rewards"]["queries"][1]["query_path"]
            .as_str()
            .expect("pre-withdraw seller rewards path")
            .to_string();

        let observed = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                Some(current),
                vec![
                    stopped.clone(),
                    event(
                        "withdrawn",
                        2,
                        ShellWithdrawn {
                            recipient: seller.clone(),
                            amount: 3,
                        },
                    ),
                ],
            ),
        );
        assert_eq!(observed.withdrawal.status, "observed");
        assert_eq!(observed.withdrawal.observed_amount, "3");
        assert_eq!(observed.withdrawal.finalized_owed.as_deref(), Some("7"));
        assert_eq!(observed.withdrawal.events.len(), 1);
        assert!(
            as_value(&observed)["withdrawal"].get("complete").is_none(),
            "an observed partial withdrawal must not be named complete"
        );

        let post_destroy = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                None,
                vec![
                    stopped,
                    event(
                        "withdrawn",
                        2,
                        ShellWithdrawn {
                            recipient: seller,
                            amount: 3,
                        },
                    ),
                    event(
                        "destroyed",
                        3,
                        ContractDestroyed {
                            token_contract: token_contract.clone(),
                        },
                    ),
                ],
            ),
        );
        let post_destroy = as_value(&post_destroy);
        assert_eq!(post_destroy["terminal"]["status"], "terminal");
        assert_eq!(post_destroy["terminal"]["message_id"], "stopped");
        assert_eq!(post_destroy["withdrawal"]["status"], "observed");
        assert_eq!(post_destroy["withdrawal"]["observed_amount"], "3");
        assert!(post_destroy["current"].is_null());
        assert!(post_destroy["deal"]["terms"].is_null());
        assert!(post_destroy["parties"]["seller_note"]["address"].is_null());
        assert!(post_destroy["parties"]["seller_note"]["source"].is_null());
        assert!(post_destroy["rewards"]["queries"][1]["participant_note"].is_null());
        assert!(post_destroy["rewards"]["queries"][1]["query_path"].is_null());
        assert_eq!(
            post_destroy["rewards"]["queries"][0]["query_path"],
            pre_buyer_rewards_path
        );
        assert!(
            pre_seller_rewards_path.contains(&address('b')),
            "the retained pre-withdraw receipt supplies the seller rewards path"
        );
    }

    #[test]
    fn contract_destroyed_without_terminal_event_is_not_settlement() {
        let token_contract = address('a');
        let receipt = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                None,
                vec![event(
                    "destroyed",
                    1,
                    ContractDestroyed {
                        token_contract: token_contract.clone(),
                    },
                )],
            ),
        );
        assert_eq!(receipt.terminal.status, "not_final");
        assert_eq!(receipt.terminal.kind, None);
        assert_eq!(receipt.withdrawal.status, "not_applicable");
        assert!(receipt.current.is_none());
    }

    /// the receipt reads the side that reports the money.

    /// `cleanupUnopened` -- the never-opened refund -- emits NO settlement event; the contract says so
    /// where the event used to be declared. All the deal leaves behind is `ContractDestroyed`, so
    /// `terminal.status` is honestly `not_final` and must stay that way: the deal really did not post
    /// a settlement. What was missing is the answer to the owner's actual question, which the note
    /// does report. These cases pin both ends of that and the disagreement between them.
    mod outcome_reads_the_money_side {
        use super::*;

        /// The live shape of issue, in figures taken from that deal: the book funded 2.05, the
        /// buyer bonded 2.00, the deal was destroyed silently, and the buyer's note announced a
        /// credit of 4.05 -- deposit plus the bond `_releaseBuyerBond` folds back into it.
        #[test]
        fn destroyed_deal_with_a_note_credit_names_the_return_and_its_amount() {
            let token_contract = address('a');
            let buyer = address('c');
            let receipt = build_receipt(
                context(&token_contract),
                &chain_with_note_credits(
                    &token_contract,
                    None,
                    vec![
                        event(
                            "funded",
                            1,
                            StreamFunded {
                                buyer: buyer.clone(),
                                deposit: 2_050_000_000,
                            },
                        ),
                        event(
                            "bond",
                            2,
                            BuyerBondFunded {
                                amount: 2_000_000_000,
                            },
                        ),
                        event(
                            "destroyed",
                            3,
                            ContractDestroyed {
                                token_contract: token_contract.clone(),
                            },
                        ),
                    ],
                    vec![buyer.clone()],
                    vec![note_credit(&buyer, &token_contract, 4_050_000_000, 4)],
                ),
            );

            // The deal genuinely posted no settlement event, and the receipt still says so.
            assert_eq!(receipt.terminal.status, "not_final");
            // What changed is that the receipt no longer stops there.
            assert_eq!(receipt.outcome.status, "returned");
            assert_eq!(receipt.outcome.amount.as_deref(), Some("4050000000"));
            assert_eq!(receipt.outcome.source, Some("note_deal_credited"));
            assert_eq!(receipt.outcome.note_amount.as_deref(), Some("4050000000"));
            assert!(receipt.outcome.deal_amount.is_none());
            assert!(receipt.outcome.missing.is_empty());
            let value = as_value(&receipt);
            assert_eq!(
                value["outcome"]["note_credits"][0]["message_id"],
                "credit-4"
            );
            assert_eq!(value["outcome"]["notes_read"][0], buyer);
            // The buyer bond is now in the sequence: without it a reader sees a 4.05 return against
            // a 2.05 deposit and cannot account for the difference.
            assert_eq!(value["settlement_sequence"][1]["kind"], "BuyerBondFunded");
            assert_eq!(
                value["settlement_sequence"][1]["payload"]["amount"],
                "2000000000"
            );
        }

        /// Nothing was credited and no note was ever identified: the receipt says what is missing
        /// instead of implying an outcome. This is the case the fix must NOT turn into a claim.
        #[test]
        fn destroyed_deal_with_no_note_statement_stays_undetermined_and_says_why() {
            let token_contract = address('a');
            let receipt = build_receipt(
                context(&token_contract),
                &chain(
                    &token_contract,
                    None,
                    vec![event(
                        "destroyed",
                        1,
                        ContractDestroyed {
                            token_contract: token_contract.clone(),
                        },
                    )],
                ),
            );
            assert_eq!(receipt.outcome.status, "undetermined");
            assert!(receipt.outcome.amount.is_none());
            assert_eq!(
                receipt.outcome.missing,
                vec![
                    "deal_posted_no_terminal_settlement_event".to_string(),
                    "counterparty_note_unidentified".to_string(),
                ]
            );
        }

        /// Both sides spoke and disagreed. The receipt reports the disagreement and carries BOTH
        /// figures; it does not pick one. Choosing here would be inventing a third truth.
        #[test]
        fn two_statements_that_disagree_are_reported_as_a_divergence_not_resolved() {
            let token_contract = address('a');
            let buyer = address('c');
            let receipt = build_receipt(
                context(&token_contract),
                &chain_with_note_credits(
                    &token_contract,
                    None,
                    vec![event(
                        "stopped",
                        1,
                        StreamStopped {
                            buyer: buyer.clone(),
                            to_seller: 1_000_000_000,
                            refund_to_buyer: 3_000_000_000,
                        },
                    )],
                    vec![buyer.clone()],
                    vec![note_credit(&buyer, &token_contract, 4_050_000_000, 2)],
                ),
            );
            assert_eq!(receipt.outcome.status, "divergent");
            assert_eq!(receipt.outcome.deal_amount.as_deref(), Some("3000000000"));
            assert_eq!(receipt.outcome.note_amount.as_deref(), Some("4050000000"));
            assert!(receipt
                .consistency_issues
                .contains(&"outcome_amount_divergence".to_string()));
        }

        /// Agreement is not a divergence. The deal posted its settlement and the note confirms the
        /// same figure, so the outcome is `settled` and nothing is flagged.
        #[test]
        fn a_note_credit_that_matches_the_deal_is_settled_and_flags_nothing() {
            let token_contract = address('a');
            let buyer = address('c');
            let receipt = build_receipt(
                context(&token_contract),
                &chain_with_note_credits(
                    &token_contract,
                    None,
                    vec![event(
                        "stopped",
                        1,
                        StreamStopped {
                            buyer: buyer.clone(),
                            to_seller: 1_000_000_000,
                            refund_to_buyer: 4_050_000_000,
                        },
                    )],
                    vec![buyer.clone()],
                    vec![note_credit(&buyer, &token_contract, 4_050_000_000, 2)],
                ),
            );
            assert_eq!(receipt.outcome.status, "settled");
            assert_eq!(receipt.outcome.source, Some("deal_terminal_event"));
            assert!(!receipt
                .consistency_issues
                .contains(&"outcome_amount_divergence".to_string()));
        }
    }

    #[test]
    fn wrong_tc_and_event_getter_contradiction_are_inconsistent() {
        let token_contract = address('a');
        let wrong_contract = address('f');
        let seller = address('b');
        let buyer = address('c');
        let terminal = StreamStopped {
            buyer: buyer.clone(),
            to_seller: 1,
            refund_to_buyer: 2,
        };

        let wrong_reader = build_receipt(
            context(&token_contract),
            &chain(
                &wrong_contract,
                Some(current_facts(Some(&buyer), &seller, false, true, false, 3)),
                vec![event("terminal", 1, terminal.clone())],
            ),
        );
        assert_eq!(wrong_reader.terminal.status, "inconsistent");
        assert!(wrong_reader
            .consistency_issues
            .contains(&"reader_account_mismatch".to_string()));

        let contradictory = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                Some(current_facts(Some(&buyer), &seller, true, true, false, 3)),
                vec![event("terminal", 1, terminal)],
            ),
        );
        assert_eq!(contradictory.terminal.status, "inconsistent");
        assert!(contradictory
            .consistency_issues
            .contains(&"terminal_event_current_state_mismatch".to_string()));
    }

    #[test]
    fn events_from_two_token_contracts_cannot_mix() {
        let first = address('a');
        let second = address('e');
        let first_receipt = build_receipt(
            context(&first),
            &chain(
                &first,
                None,
                vec![event(
                    "first-destroyed",
                    1,
                    ContractDestroyed {
                        token_contract: first.clone(),
                    },
                )],
            ),
        );
        let second_receipt = build_receipt(
            context(&second),
            &chain(
                &second,
                None,
                vec![event(
                    "second-destroyed",
                    1,
                    ContractDestroyed {
                        token_contract: second.clone(),
                    },
                )],
            ),
        );
        assert_eq!(first_receipt.settlement_sequence[0].payload["self"], first);
        assert_eq!(
            second_receipt.settlement_sequence[0].payload["self"],
            second
        );

        let mixed = build_receipt(
            context(&first),
            &chain(
                &first,
                None,
                vec![event(
                    "foreign-destroyed",
                    1,
                    ContractDestroyed {
                        token_contract: second,
                    },
                )],
            ),
        );
        assert_eq!(mixed.terminal.status, "inconsistent");
        assert!(mixed
            .consistency_issues
            .contains(&"event_token_contract_mismatch".to_string()));
    }

    #[test]
    fn receipt_fixture_is_stable_and_contains_no_private_local_material() {
        let receipt = unavailable_receipt(context(&address('a')));
        let actual = as_value(&receipt);
        let expected: Value =
            serde_json::from_str(include_str!("settlement_receipt_v1.fixture.json"))
                .expect("parse settlement receipt fixture");
        assert_eq!(actual, expected);

        let serialized = serde_json::to_string(&receipt).expect("serialize receipt");
        for forbidden in [
            "wallet_key",
            "note_secret",
            "owner_secret",
            "endpoint_cipher",
            "prompt",
            "model_output",
            "deal_history",
        ] {
            assert!(!serialized.contains(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn rewards_join_is_two_note_season_aggregate_and_never_per_deal_points() {
        let token_contract = address('a');
        let seller = address('b');
        let buyer = address('c');
        let receipt = build_receipt(
            context(&token_contract),
            &chain(
                &token_contract,
                Some(current_facts(Some(&buyer), &seller, true, false, false, 0)),
                Vec::new(),
            ),
        );
        let rewards = &as_value(&receipt)["rewards"];
        assert_eq!(rewards["schema"], REWARDS_SCHEMA);
        assert_eq!(rewards["source"], REWARDS_SOURCE);
        assert_eq!(rewards["requested_season"], 7);
        assert_eq!(rewards["semantics"], REWARDS_SEMANTICS);
        assert_eq!(rewards["queries"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            rewards["queries"][0]["query_path"],
            format!("/v1/notes/{buyer}?season=7")
        );
        assert_eq!(
            rewards["queries"][1]["query_path"],
            format!("/v1/notes/{seller}?season=7")
        );
        assert!(rewards.get("points").is_none());
        assert!(rewards.get("token_contract").is_none());
    }
}
