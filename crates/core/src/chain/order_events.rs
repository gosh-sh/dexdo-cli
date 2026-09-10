//! Narrow `InferenceOrderPlaced` decoder used by durable subscription reconciliation.

//! Subscriptions no longer have an event of their own: one is an ordinary BUY order carrying the
//! `SUBSCRIPTION` deal-shape flag, so reconciliation filters the generic placement event by side and flag.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use tvm_abi::token::TokenValue;
use tvm_abi::{Contract, Event};
use tvm_types::SliceData;

use crate::market::{flags, InferenceSubscriptionPlacement};
#[cfg(test)]
use crate::params::SUBSCRIPTION_MAX_TICKS;
use crate::params::SUBSCRIPTION_WEEKS;

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
    if event.name != "InferenceOrderPlaced" {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode InferenceOrderPlaced body: {error}"))?;
    // Keep only the buyer-side subscription placements: an ask carries the same event, and so does every
    // ordinary buy. The flag is what distinguishes a reserved-capacity order from a plain one.
    if !named_bool(&tokens, "isBuy")? {
        return Ok(None);
    }
    let order_flags = u8::try_from(named_u128(&tokens, "flags")?).unwrap_or(0);
    if order_flags != (flags::AON | flags::SUBSCRIPTION) {
        return Ok(None);
    }
    Ok(Some(InferenceSubscriptionPlacement {
        order_id: named_u128(&tokens, "orderId")?,
        buyer_note: named_address(&tokens, "note")?,
        max_price_per_tick: named_u128(&tokens, "price")?,
        ticks: named_u128(&tokens, "ticks")?,
        sub_weeks: SUBSCRIPTION_WEEKS,
        deadline: u64::try_from(named_u128(&tokens, "deadline")?).unwrap_or(u64::MAX),
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

    fn encode_order_placed(fields: serde_json::Value) -> String {
        use tvm_abi::token::Tokenizer;
        use tvm_types::{BuilderData, IBitstring as _};

        let contract = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes()).expect("load IOB ABI");
        let event = contract
            .event("InferenceOrderPlaced")
            .expect("order placed event");
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

    fn placement_fields(is_buy: bool, order_flags: u8) -> serde_json::Value {
        serde_json::json!({
            "orderId": "41",
            "isBuy": is_buy,
            "price": "700",
            "ticks": SUBSCRIPTION_MAX_TICKS.to_string(),
            "note": format!("0:{}", "a".repeat(64)),
            "tokenContract": format!("0:{}", "b".repeat(64)),
            "deadline": "1700000000",
            "flags": order_flags,
        })
    }

    #[test]
    fn subscription_placement_decodes_owner_ticks_price_and_deadline() {
        let body = encode_order_placed(placement_fields(
            true,
            crate::market::flags::AON | crate::market::flags::SUBSCRIPTION,
        ));
        let placement = decode_subscription_placement(&body)
            .expect("decode placement")
            .expect("placement event");
        assert_eq!(placement.buyer_note, format!("0:{}", "a".repeat(64)));
        assert_eq!(placement.ticks, SUBSCRIPTION_MAX_TICKS);
        assert_eq!(placement.max_price_per_tick, 700);
        assert_eq!(placement.deadline, 1_700_000_000);
        assert_eq!(
            placement.sub_weeks, SUBSCRIPTION_WEEKS,
            "the term is fixed protocol-wide, not carried by the order"
        );
    }

    /// A subscription is an ordinary flagged BUY order, so the decoder must reject everything that shares the
    /// same event: plain buys, and asks -- otherwise reconciliation would attribute unrelated orders to a
    /// subscription and match on the wrong one.
    #[test]
    fn only_flagged_buy_orders_are_treated_as_subscriptions() {
        let plain_buy = encode_order_placed(placement_fields(true, 0));
        assert!(
            decode_subscription_placement(&plain_buy)
                .expect("decode")
                .is_none(),
            "an unflagged buy is not a subscription"
        );

        let sell_side = encode_order_placed(placement_fields(
            false,
            crate::market::flags::AON | crate::market::flags::SUBSCRIPTION,
        ));
        assert!(
            decode_subscription_placement(&sell_side)
                .expect("decode")
                .is_none(),
            "the seller side is never a subscription placement"
        );

        let missing_aon =
            encode_order_placed(placement_fields(true, crate::market::flags::SUBSCRIPTION));
        assert!(
            decode_subscription_placement(&missing_aon)
                .expect("decode")
                .is_none(),
            "SUBSCRIPTION without AON is not the accepted single-seller shape"
        );

        let extra_ioc = encode_order_placed(placement_fields(
            true,
            crate::market::flags::AON
                | crate::market::flags::SUBSCRIPTION
                | crate::market::flags::IOC,
        ));
        assert!(
            decode_subscription_placement(&extra_ioc)
                .expect("decode")
                .is_none(),
            "the durable reconciler accepts exactly AON|SUBSCRIPTION"
        );
    }
}
