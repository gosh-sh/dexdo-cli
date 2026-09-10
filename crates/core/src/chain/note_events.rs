//! Decoding the buyer/seller note's **owner-facing** ext-out events (, vendored 4.0.15
//! `PrivateNote`). On a match the `InferenceOrderBook` pushes `onInferenceFilled` into BOTH notes, so
//! each owner reads the matched deal `tokenContract` from JUST its own note's ext-out -- no shared-book
//! index. This module decodes the `InferenceFilledConfirmed(orderBook, tokenContract, orderId, ticks,
//! clearingPrice, isBuy)` event body with `tvm_abi` (same single tvm-sdk source as `gosh.ackinacki`).

use anyhow::{anyhow, Result};
use base64::Engine as _;
use tvm_abi::contract::ABI_VERSION_2_4;
use tvm_abi::token::TokenValue;
use tvm_abi::{Contract, Event};
use tvm_types::SliceData;

use super::contracts_provision::PRIVATENOTE_ABI;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InferencePlaced {
    pub order_book: String,
    pub token_contract: String,
    pub order_id: u128,
    pub is_buy: bool,
}

pub(super) fn decode_inference_placed(body_b64: &str) -> Result<Option<InferencePlaced>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(PRIVATENOTE_ABI.as_bytes())
        .map_err(|e| anyhow!("load PrivateNote ABI: {e}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    if event.name != "InferenceOrderPlacedConfirmed" {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|e| anyhow!("decode InferenceOrderPlacedConfirmed body: {e}"))?;

    let mut order_book = None;
    let mut token_contract = None;
    let mut order_id = None;
    let mut is_buy = None;
    for token in tokens {
        match (token.name.as_str(), &token.value) {
            ("orderBook", TokenValue::Address(address)) => order_book = Some(format!("{address}")),
            ("tokenContract", TokenValue::Address(address)) => {
                token_contract = Some(format!("{address}"))
            }
            ("orderId", TokenValue::Uint(value)) => {
                order_id = value.number.to_string().parse().ok()
            }
            ("isBuy", TokenValue::Bool(value)) => is_buy = Some(*value),
            _ => {}
        }
    }
    match (order_book, token_contract, order_id, is_buy) {
        (Some(order_book), Some(token_contract), Some(order_id), Some(is_buy)) => {
            Ok(Some(InferencePlaced {
                order_book,
                token_contract,
                order_id,
                is_buy,
            }))
        }
        _ => Err(anyhow!(
            "InferenceOrderPlacedConfirmed body missing orderBook/tokenContract/orderId/isBuy -- ABI drift"
        )),
    }
}

/// One decoded `InferenceFilledConfirmed` ext-out from a note (the fields the client needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InferenceFilled {
    /// The per-model `InferenceOrderBook` that emitted the fill (caller filters by the derived book).
    pub order_book: String,
    /// The matched per-deal `TokenContract` (`0:<hex>`) -- what the buyer/seller then reads.
    pub token_contract: String,
    /// This note owner's authoritative order-book id.
    pub order_id: u128,
    /// Number of ticks filled by this match.
    pub ticks: u128,
    /// Clearing price paid per tick.
    pub price_per_tick: u128,
    /// This note's side of the match: `true` = buyer, `false` = seller.
    pub is_buy: bool,
}

/// Decode one ext-out message body (base64 BOC) as `InferenceFilledConfirmed`.

/// Returns `Ok(None)` when the body is a DIFFERENT note event (another event id) -- the caller scans all
/// of a note's ext-out and skips non-matches. Errors only on a body that claims this event id but does not
/// decode (a real ABI/selector drift, which must fail loud, not be silently skipped).
pub(super) fn decode_inference_filled(body_b64: &str) -> Result<Option<InferenceFilled>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        // Not an ABI event body from this note mirror.
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        // Not a TVM event BOC.
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };

    // The first 32 bits of an event body are the event function id.
    let id = match Event::decode_id(slice.clone()) {
        Ok(id) => id,
        // No leading id (not an ABI event body) -- not our event.
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(PRIVATENOTE_ABI.as_bytes())
        .map_err(|e| anyhow!("load PrivateNote ABI: {e}"))?;
    let event = match contract.event_by_id(id) {
        Ok(e) => e,
        // A valid id but not a PrivateNote event we know -- skip.
        Err(_) => return Ok(None),
    };
    if event.name != "InferenceFilledConfirmed" {
        return Ok(None);
    }

    // It IS our event id -- a decode failure now is a real selector/ABI drift: fail loud.
    let tokens = event
        .decode_input(slice, true)
        .map_err(|e| anyhow!("decode InferenceFilledConfirmed body: {e}"))?;

    let mut order_book = None;
    let mut token_contract = None;
    let mut order_id = None;
    let mut ticks = None;
    let mut price_per_tick = None;
    let mut is_buy = None;
    for t in tokens {
        match (t.name.as_str(), &t.value) {
            ("orderBook", TokenValue::Address(a)) => order_book = Some(format!("{a}")),
            ("tokenContract", TokenValue::Address(a)) => token_contract = Some(format!("{a}")),
            ("orderId", TokenValue::Uint(v)) => order_id = v.number.to_string().parse().ok(),
            ("ticks", TokenValue::Uint(v)) => ticks = v.number.to_string().parse().ok(),
            ("clearingPrice", TokenValue::Uint(v)) => {
                price_per_tick = v.number.to_string().parse().ok()
            }
            ("isBuy", TokenValue::Bool(b)) => is_buy = Some(*b),
            _ => {}
        }
    }
    match (
        order_book,
        token_contract,
        order_id,
        ticks,
        price_per_tick,
        is_buy,
    ) {
        (
            Some(order_book),
            Some(token_contract),
            Some(order_id),
            Some(ticks),
            Some(price_per_tick),
            Some(is_buy),
        ) => Ok(Some(InferenceFilled {
            order_book,
            token_contract,
            order_id,
            ticks,
            price_per_tick,
            is_buy,
        })),
        _ => Err(anyhow!(
            "InferenceFilledConfirmed body missing orderBook/tokenContract/orderId/ticks/clearingPrice/isBuy -- ABI drift"
        )),
    }
}

/// One decoded fill with the order id required for durable subscription attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AttributedInferenceFilled {
    pub order_book: String,
    pub token_contract: String,
    pub order_id: u128,
    pub ticks: u128,
    pub price_per_tick: u128,
    pub is_buy: bool,
}

/// Decode a fill for the inert subscription journal, requiring its `orderId` attribution field.
pub(super) fn decode_attributed_inference_filled(
    body_b64: &str,
) -> Result<Option<AttributedInferenceFilled>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(PRIVATENOTE_ABI.as_bytes())
        .map_err(|e| anyhow!("load PrivateNote ABI: {e}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    if event.name != "InferenceFilledConfirmed" {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|e| anyhow!("decode InferenceFilledConfirmed body: {e}"))?;

    let mut order_book = None;
    let mut token_contract = None;
    let mut order_id = None;
    let mut ticks = None;
    let mut price_per_tick = None;
    let mut is_buy = None;
    for token in tokens {
        match (token.name.as_str(), &token.value) {
            ("orderBook", TokenValue::Address(address)) => order_book = Some(format!("{address}")),
            ("tokenContract", TokenValue::Address(address)) => {
                token_contract = Some(format!("{address}"))
            }
            ("orderId", TokenValue::Uint(value)) => {
                order_id = value.number.to_string().parse().ok()
            }
            ("ticks", TokenValue::Uint(value)) => ticks = value.number.to_string().parse().ok(),
            ("clearingPrice", TokenValue::Uint(value)) => {
                price_per_tick = value.number.to_string().parse().ok()
            }
            ("isBuy", TokenValue::Bool(value)) => is_buy = Some(*value),
            _ => {}
        }
    }
    match (
        order_book,
        token_contract,
        order_id,
        ticks,
        price_per_tick,
        is_buy,
    ) {
        (
            Some(order_book),
            Some(token_contract),
            Some(order_id),
            Some(ticks),
            Some(price_per_tick),
            Some(is_buy),
        ) => Ok(Some(AttributedInferenceFilled {
            order_book,
            token_contract,
            order_id,
            ticks,
            price_per_tick,
            is_buy,
        })),
        _ => Err(anyhow!(
            "InferenceFilledConfirmed body missing orderBook/tokenContract/orderId/ticks/clearingPrice/isBuy -- ABI drift"
        )),
    }
}

/// One `onInferencePlaced` / `onInferenceOrderRemoved` the BOOK sent INTO this note.

/// these two inbound calls are the only place `modelHash` ever reaches the note, and
/// `modelHash` is what the owner needs -- `cancelInferenceOrder(uint256 modelHash, uint128 orderId)`
/// and `cancelAllInferenceOrders(uint256 modelHash)` are keyed on it and on nothing else.

/// Every other record loses it. `_restingInf` stores `tvm.hash(abi.encode(book, orderId))`, a
/// one-way key. The note's own ext-out mirrors (`InferenceOrderPlacedConfirmed`,
/// `InferenceOrderRemoved`) re-emit `msg.sender` -- the BOOK -- and a book address is
/// `computeInferenceOrderBookAddress(code, modelHash)`, which is also one way. So an owner holding
/// only the note and its key could see that money was resting and still not name the call that
/// releases it. The inbound body is where the answer survives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InferenceOrderCall {
    /// `0x`-prefixed uint256, exactly the shape `cancelInferenceOrder` takes.
    pub model_hash: String,
    pub order_id: u128,
}

/// Decode one INBOUND internal message body as a named `PrivateNote` call.

/// Internal bodies carry no signed header, so the id is read with `internal = true`. A body that is
/// not a `PrivateNote` call at all is `None` rather than an error: a note's inbound history is
/// mixed, and the walk over it must skip what it does not recognise instead of failing the run.
fn decode_note_call(body_b64: &str) -> Result<Option<(String, Vec<tvm_abi::Token>)>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(PRIVATENOTE_ABI.as_bytes())
        .map_err(|e| anyhow!("load PrivateNote ABI: {e}"))?;
    let id = match tvm_abi::Function::decode_input_id(
        contract.version(),
        slice.clone(),
        contract.header(),
        true,
    ) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let function = match contract.function_by_id(id, true) {
        Ok(function) => function,
        Err(_) => return Ok(None),
    };
    let name = function.name.clone();
    let tokens = function
        .decode_input(slice, true, true)
        .map_err(|e| anyhow!("decode PrivateNote.{name} inbound body: {e}"))?;
    Ok(Some((name, tokens)))
}

/// Pull `modelHash` and `orderId` out of a decoded call, whichever of the two calls it is.
fn inference_order_call_fields(tokens: &[tvm_abi::Token]) -> Option<InferenceOrderCall> {
    let mut model_hash = None;
    let mut order_id = None;
    for token in tokens {
        match (token.name.as_str(), &token.value) {
            ("modelHash", TokenValue::Uint(value)) => {
                model_hash = Some(format!("0x{:064x}", value.number));
            }
            ("orderId", TokenValue::Uint(value)) => {
                order_id = value.number.to_string().parse().ok();
            }
            _ => {}
        }
    }
    Some(InferenceOrderCall {
        model_hash: model_hash?,
        order_id: order_id?,
    })
}

/// `onInferencePlaced(modelHash, tokenContract, orderId, clientOrderId, isBuy, price, ticks)`.
pub(super) fn decode_inference_placed_call(body_b64: &str) -> Result<Option<InferenceOrderCall>> {
    match decode_note_call(body_b64)? {
        Some((name, tokens)) if name == "onInferencePlaced" => inference_order_call_fields(&tokens)
            .map(Some)
            .ok_or_else(|| {
                anyhow!("onInferencePlaced body missing modelHash/orderId -- ABI drift")
            }),
        _ => Ok(None),
    }
}

/// `onInferenceOrderRemoved(modelHash, orderId, cause, refunded)` -- the order stopped resting.

/// Decoding this is not optional. A note that placed five orders and had all five removed shows
/// five `onInferencePlaced` in its history; reporting those as still resting would send the owner
/// to cancel five orders that do not exist, paying gas for each. Measured on the chain note
/// `0:29f4223b...4e`, which carries exactly that 5-and-5 shape.
pub(super) fn decode_inference_order_removed_call(
    body_b64: &str,
) -> Result<Option<InferenceOrderCall>> {
    match decode_note_call(body_b64)? {
        Some((name, tokens)) if name == "onInferenceOrderRemoved" => {
            inference_order_call_fields(&tokens)
                .map(Some)
                .ok_or_else(|| {
                    anyhow!("onInferenceOrderRemoved body missing modelHash/orderId -- ABI drift")
                })
        }
        _ => Ok(None),
    }
}

/// `tvm.hash(abi.encode(book, orderId))` -- the key `PrivateNote._restingInf` is stored under.

/// This is what turns a recovered `(modelHash, orderId)` from a LEAD into a PROOF. The pair comes
/// out of message history, which is a record of what happened, not of what is still true; the note's
/// own `getOutstanding()` is what is still true, but it publishes only these opaque keys. Composing
/// the key from the pair and looking for it in that set joins the two: a hit proves this exact order
/// is resting right now, and the leftover keys are the orders the history could not explain.

/// The encoding is not hand rolled -- `pack_values_into_chain` is the same ABI serializer the
/// compiler used on the contract side, so the layout cannot drift from the one that produced the
/// stored key.
pub(super) fn resting_inference_order_key(book: &str, order_id: u128) -> Result<String> {
    use std::str::FromStr as _;

    let address = tvm_block::MsgAddress::from_str(book)
        .map_err(|e| anyhow!("resting order key: book address {book}: {e}"))?;
    let tokens = vec![
        tvm_abi::Token::new("book", TokenValue::Address(address)),
        tvm_abi::Token::new(
            "orderId",
            TokenValue::Uint(tvm_abi::Uint::new(order_id, 128)),
        ),
    ];
    let builder = TokenValue::pack_values_into_chain(&tokens, Vec::new(), &ABI_VERSION_2_4)
        .map_err(|e| anyhow!("resting order key: pack abi.encode(book, orderId): {e}"))?;
    let cell = builder
        .into_cell()
        .map_err(|e| anyhow!("resting order key: finalize cell: {e}"))?;
    Ok(format!(
        "0x{}",
        super::contracts_provision::encode_hex(cell.repr_hash().as_slice())
    ))
}

#[cfg(test)]
mod tests {
    /// The membership key is what turns a `(modelHash, orderId)` recovered from history into
    /// a statement about what is resting NOW, so it has to be the key the contract actually stored:
    /// `PrivateNote._restingInf` is written under `tvm.hash(abi.encode(msg.sender, orderId))`.

    /// The vector is derived INDEPENDENTLY, from the TVM cell spec rather than from this code:
    /// `addr_std$10` + `anycast:nothing$0` + `workchain:int8` + `address:bits256` + `uint128` is 395
    /// bits, which is one cell with no refs, so `d1=0`, `d2=2*49+1=99`, the data carries the
    /// completion tag, and the representation hash is the sha256 of `d1||d2||data`. Two derivations
    /// that agree are worth more than one that cannot be checked: nothing on chain today rests an
    /// inference order, so this cannot yet be confirmed against a live `_restingInf` key, and a
    /// self-generated expectation would confirm nothing at all.
    #[test]
    fn resting_order_key_matches_the_cell_spec_derivation() {
        let key = super::resting_inference_order_key(
            "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            4242,
        )
        .expect("the key is computable for a well formed book address");
        assert_eq!(
            key, "0x8eed5e92bd3da53e81200adcc8ad58dcc1c51c6a30a000ebf3c9543ba025b581",
            "abi.encode(book, orderId) must serialise exactly as the TVM cell spec lays it out"
        );
    }

    /// The book is inside the key on purpose -- each book runs its own `_nextOrderId`, so order 7 in
    /// one book and order 7 in another are different orders. A key that ignored either half would
    /// merge them, which is the failure the contract's own comment says the composite key exists to
    /// prevent.
    #[test]
    fn resting_order_key_separates_both_halves() {
        let a = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let b = "0:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let key_ab = super::resting_inference_order_key(a, 7).expect("key");
        let key_bb = super::resting_inference_order_key(b, 7).expect("key");
        let key_a9 = super::resting_inference_order_key(a, 9).expect("key");
        assert_ne!(
            key_ab, key_bb,
            "the same order id in two books is two orders"
        );
        assert_ne!(key_ab, key_a9, "two order ids in one book are two orders");
    }

    use super::*;
    use serde_json::Value;

    /// Offline selector guard: the decoder extracts fields BY NAME, so the deployed event must keep this
    /// exact shape. If the vendored `PrivateNote` ABI renames/reorders these, the decoder silently stops
    /// finding `tokenContract` -- pin the layout so that drift fails this test, not a live buy.
    #[test]
    fn inference_filled_confirmed_abi_shape_is_pinned() {
        let abi: Value = serde_json::from_str(PRIVATENOTE_ABI).expect("parse PrivateNote ABI");
        let ev = abi["events"]
            .as_array()
            .expect("events[]")
            .iter()
            .find(|e| e["name"] == "InferenceFilledConfirmed")
            .expect("InferenceFilledConfirmed present in 4.0.15 PrivateNote ABI");
        let inputs: Vec<(&str, &str)> = ev["inputs"]
            .as_array()
            .expect("inputs[]")
            .iter()
            .map(|i| {
                (
                    i["name"].as_str().unwrap_or(""),
                    i["type"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            inputs,
            vec![
                ("orderBook", "address"),
                ("tokenContract", "address"),
                ("orderId", "uint128"),
                ("ticks", "uint128"),
                ("clearingPrice", "uint256"),
                ("isBuy", "bool"),
            ],
            "InferenceFilledConfirmed selector drifted -- the buyer's tokenContract decode depends on it"
        );
    }

    #[test]
    fn inference_placed_4026_callback_and_event_abi_shapes_are_pinned() {
        let abi: Value = serde_json::from_str(PRIVATENOTE_ABI).expect("parse PrivateNote ABI");
        let callback = abi["functions"]
            .as_array()
            .expect("functions[]")
            .iter()
            .find(|function| function["name"] == "onInferencePlaced")
            .expect("onInferencePlaced present in 4.0.26 PrivateNote ABI");
        let callback_inputs: Vec<(&str, &str)> = callback["inputs"]
            .as_array()
            .expect("onInferencePlaced inputs[]")
            .iter()
            .map(|input| {
                (
                    input["name"].as_str().unwrap_or(""),
                    input["type"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            callback_inputs,
            vec![
                ("modelHash", "uint256"),
                ("tokenContract", "address"),
                ("orderId", "uint128"),
                // 4.0.33 threads the caller's own order id back to the note, between the book's
                // `orderId` and `isBuy` -- see `onInferencePlaced` in `contracts/dex/PrivateNote.sol`
                // and the matching `IPrivateNote` declaration in
                // `contracts/airegistry/InferenceOrderBook.sol`. It is a positional insertion, so a
                // pin that omits it does not merely under-specify the shape: every field after it
                // is described at the wrong offset.
                ("clientOrderId", "uint64"),
                ("isBuy", "bool"),
                ("price", "uint256"),
                ("ticks", "uint128"),
            ],
            "onInferencePlaced callback shape drifted from the vendored PrivateNote ABI"
        );

        let ev = abi["events"]
            .as_array()
            .expect("events[]")
            .iter()
            .find(|e| e["name"] == "InferenceOrderPlacedConfirmed")
            .expect("InferenceOrderPlacedConfirmed present in 4.0.26 PrivateNote ABI");
        let inputs: Vec<(&str, &str)> = ev["inputs"]
            .as_array()
            .expect("inputs[]")
            .iter()
            .map(|i| {
                (
                    i["name"].as_str().unwrap_or(""),
                    i["type"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            inputs,
            vec![
                ("orderBook", "address"),
                ("tokenContract", "address"),
                ("orderId", "uint128"),
                ("isBuy", "bool"),
                ("price", "uint256"),
                ("ticks", "uint128"),
            ],
            "4.0.26 placement event shape drifted"
        );
    }

    /// The ABI loads into a `tvm_abi::Contract` and the event resolves both by name and by its derived id --
    /// the two lookups the decoder relies on.
    #[test]
    fn private_note_abi_loads_and_event_resolves() {
        let contract = Contract::load(PRIVATENOTE_ABI.as_bytes()).expect("load PrivateNote ABI");
        let ev = contract
            .event("InferenceFilledConfirmed")
            .expect("event by name");
        let by_id = contract.event_by_id(ev.get_id()).expect("event by id");
        assert_eq!(by_id.name, "InferenceFilledConfirmed");
    }

    /// A body that is not an ABI event (random bytes / empty) is skipped, not an error.
    #[test]
    fn non_event_body_is_skipped() {
        assert_eq!(decode_inference_filled("").unwrap(), None);
        assert_eq!(decode_inference_filled("AA==").unwrap(), None);
    }
}

/// One decoded `DealCredited(deal, amount)` ext-out from a note.

/// THE DEAL DOES NOT REPORT THIS OUTCOME AND CANNOT. `cleanupUnopened` -- the never-opened
/// refund -- emits no settlement event at all: `TokenContract.sol` says so where the event used to be
/// declared ("`StreamReclaimed(address buyer, uint128 refundToBuyer)` stood here and was never
/// emitted -- no `emit`, and no external-address constant to emit it through"). All the deal leaves is
/// `ContractDestroyed`, which proves destruction and nothing about money. The note, receiving the
/// figure, announces it here -- so for that path this event is the ONLY on-chain statement of what
/// happened to the escrow, and a receipt that reads only the deal is reading the side that stayed
/// silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DealCredited {
    /// The TokenContract that sent the credit -- the note authenticates it by re-deriving this
    /// address, so the pairing is the chain's, not ours.
    pub deal: String,
    /// SHELL credited to the note's spendable record, in raw ECC[2] units.
    pub amount: u128,
}

/// Decode one ext-out message body (base64 BOC) as `DealCredited`.

/// `Ok(None)` for a body that is a different note event -- the caller scans a note's whole ext-out.
/// A body that claims this event id and then does not decode is ABI drift and fails loud, exactly as
/// the two decoders above do: a silently skipped money statement is the defect this reader exists to
/// close.
pub(super) fn decode_deal_credited(body_b64: &str) -> Result<Option<DealCredited>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(PRIVATENOTE_ABI.as_bytes())
        .map_err(|e| anyhow!("load PrivateNote ABI: {e}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    if event.name != "DealCredited" {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|e| anyhow!("decode DealCredited body: {e}"))?;

    let mut deal = None;
    let mut amount = None;
    for token in tokens {
        match (token.name.as_str(), &token.value) {
            ("deal", TokenValue::Address(address)) => deal = Some(format!("{address}")),
            ("amount", TokenValue::Uint(value)) => amount = value.number.to_string().parse().ok(),
            _ => {}
        }
    }
    match (deal, amount) {
        (Some(deal), Some(amount)) => Ok(Some(DealCredited { deal, amount })),
        _ => Err(anyhow!(
            "DealCredited body missing deal/amount -- ABI drift"
        )),
    }
}
