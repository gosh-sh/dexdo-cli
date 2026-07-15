//! Narrow `InferenceSubscriptionPlaced` decoder used by durable subscription reconciliation.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use tvm_abi::token::TokenValue;
use tvm_abi::{Contract, Event};
use tvm_types::SliceData;

use crate::chain::InferenceSubscriptionPlacement;

use super::contracts_provision::INFERENCE_ORDERBOOK_ABI;

pub(super) fn decode_subscription_placement(
    body_b64: &str,
) -> Result<Option<InferenceSubscriptionPlacement>> {
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
    let contract = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes())
        .map_err(|error| anyhow!("load InferenceOrderBook ABI: {error}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    if event.name != "InferenceSubscriptionPlaced" {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode InferenceSubscriptionPlaced body: {error}"))?;
    Ok(Some(InferenceSubscriptionPlacement {
        order_id: named_u128(&tokens, "orderId")?,
        buyer_note: named_address(&tokens, "buyerNote")?,
        max_price_per_tick: named_u128(&tokens, "maxPrice")?,
        ticks: named_u128(&tokens, "ticks")?,
        cycle_budget: named_u128(&tokens, "cycleBudget")?,
        auto_renew: named_bool(&tokens, "autoRenew")?,
        created_at: 0,
    }))
}

fn named_u128(tokens: &[tvm_abi::Token], name: &str) -> Result<u128> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Uint(value)) if got == name => value.number.to_string().parse().ok(),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

fn named_bool(tokens: &[tvm_abi::Token], name: &str) -> Result<bool> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Bool(value)) if got == name => Some(*value),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

fn named_address(tokens: &[tvm_abi::Token], name: &str) -> Result<String> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Address(value)) if got == name => Some(format!("{value}")),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_subscription_placement(fields: serde_json::Value) -> String {
        use tvm_abi::token::Tokenizer;
        use tvm_types::{BuilderData, IBitstring as _};

        let contract = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes()).expect("load IOB ABI");
        let event = contract
            .event("InferenceSubscriptionPlaced")
            .expect("subscription event");
        let tokens =
            Tokenizer::tokenize_all_params(&event.inputs, &fields).expect("tokenize event");
        let mut prefix = BuilderData::new();
        prefix.append_u32(event.get_id()).expect("event selector");
        let builder =
            TokenValue::pack_values_into_chain(&tokens, vec![prefix.into()], &event.abi_version)
                .expect("encode event body");
        let cell = builder.into_cell().expect("event cell");
        base64::engine::general_purpose::STANDARD
            .encode(tvm_types::write_boc(&cell).expect("event BOC"))
    }

    #[test]
    fn subscription_placement_decodes_owner_ticks_and_price() {
        let buyer_note = format!("0:{}", "a".repeat(64));
        let body = encode_subscription_placement(serde_json::json!({
            "orderId": "41",
            "buyerNote": buyer_note,
            "maxPrice": "700",
            "ticks": "2",
            "cycleBudget": "350",
            "autoRenew": true
        }));
        let placement = decode_subscription_placement(&body)
            .expect("decode placement")
            .expect("placement event");
        assert_eq!(placement.buyer_note, buyer_note);
        assert_eq!(placement.ticks, 2);
        assert_eq!(placement.max_price_per_tick, 700);
        assert_eq!(placement.cycle_budget, 350);
        assert!(placement.auto_renew);
    }
}
