//! Decoding the buyer/seller note's **owner-facing** ext-out events (, vendored 4.0.15
//! `PrivateNote`). On a match the `InferenceOrderBook` pushes `onInferenceFilled` into BOTH notes, so
//! each owner reads the matched deal `tokenContract` from JUST its own note's ext-out -- no shared-book
//! index. This module decodes the `InferenceFilledConfirmed(orderBook, tokenContract, orderId, ticks,
//! clearingPrice, isBuy)` event body with `tvm_abi`(same single tvm-sdk source as `gosh.ackinacki`).

use anyhow::{anyhow, Result};
use base64::Engine as _;
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

/// One decoded `InferenceFilledConfirmed` ext-out from a note(the fields the client needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InferenceFilled {
    /// The per-model `InferenceOrderBook` that emitted the fill(caller filters by the derived book).
    pub order_book: String,
    /// The matched per-deal `TokenContract`(`0:<hex>`) -- what the buyer/seller then reads.
    pub token_contract: String,
    /// Number of ticks filled by this match.
    pub ticks: u128,
    /// Clearing price paid per tick.
    pub price_per_tick: u128,
    /// This note's side of the match: `true` = buyer, `false` = seller.
    pub is_buy: bool,
}

/// Decode one ext-out message body(base64 BOC) as `InferenceFilledConfirmed`.
/// Returns `Ok(None)` when the body is a DIFFERENT note event(another event id) -- the caller scans all
/// of a note's ext-out and skips non-matches. Errors only on a body that claims this event id but does not
/// decode(a real ABI/selector drift, which must fail loud, not be silently skipped).
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
        // No leading id(not an ABI event body) -- not our event.
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
    let mut ticks = None;
    let mut price_per_tick = None;
    let mut is_buy = None;
    for t in tokens {
        match (t.name.as_str(), &t.value) {
            ("orderBook", TokenValue::Address(a)) => order_book = Some(format!("{a}")),
            ("tokenContract", TokenValue::Address(a)) => token_contract = Some(format!("{a}")),
            ("ticks", TokenValue::Uint(v)) => ticks = v.number.to_string().parse().ok(),
            ("clearingPrice", TokenValue::Uint(v)) => {
                price_per_tick = v.number.to_string().parse().ok()
            }
            ("isBuy", TokenValue::Bool(b)) => is_buy = Some(*b),
            _ => {}
        }
    }
    match (order_book, token_contract, ticks, price_per_tick, is_buy) {
        (
            Some(order_book),
            Some(token_contract),
            Some(ticks),
            Some(price_per_tick),
            Some(is_buy),
        ) => Ok(Some(InferenceFilled {
            order_book,
            token_contract,
            ticks,
            price_per_tick,
            is_buy,
        })),
        _ => Err(anyhow!(
            "InferenceFilledConfirmed body missing orderBook/tokenContract/ticks/clearingPrice/isBuy -- ABI drift"
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

#[cfg(test)]
mod tests {
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
                ("isBuy", "bool"),
                ("price", "uint256"),
                ("ticks", "uint128"),
            ],
            "4.0.26 onInferencePlaced callback shape drifted"
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

    /// A body that is not an ABI event(random bytes / empty) is skipped, not an error.
    #[test]
    fn non_event_body_is_skipped() {
        assert_eq!(decode_inference_filled("").unwrap(), None);
        assert_eq!(decode_inference_filled("AA==").unwrap(), None);
    }
}
