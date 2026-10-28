//! Shared constructors for the claim/delivery measurements added by.

//! The CLI owns stdout, sequencing, and timestamps. Keeping the complete v1 objects here gives the
//! production writers and the regression test one schema path without exposing any output sink to
//! the money-driving tasks.

use crate::buyer::api::BuyerClaimObservation;
use crate::seller::ClaimDeliveryMeasurement;
use serde_json::{json, Value};

pub const SELLER_EVENT_SCHEMA: &str = "dexdo.seller.event.v1";
pub const BUYER_EVENT_SCHEMA: &str = "dexdo.buyer.event.v3";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellerClaimEventKind {
    ProbeDecision,
    ClaimSubmitted,
}

impl SellerClaimEventKind {
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::ProbeDecision => "probe_decision",
            Self::ClaimSubmitted => "claim_submitted",
        }
    }
}

pub fn seller_claim_event(
    seq: u64,
    ts_unix: u64,
    token_contract: &str,
    kind: SellerClaimEventKind,
    measurement: ClaimDeliveryMeasurement,
) -> Value {
    let mut event = json!({
        "schema": SELLER_EVENT_SCHEMA,
        "seq": seq,
        "ts_unix": ts_unix,
        "event": kind.event_name(),
        "role": "seller",
        "token_contract": token_contract,
    });
    if let (Some(event), Some(fields)) = (
        event.as_object_mut(),
        measurement.event_fields().as_object(),
    ) {
        event.extend(fields.clone());
    }
    event
}

pub fn buyer_claim_event(
    seq: u64,
    ts_unix: u64,
    session_id: &str,
    operation: &str,
    deal_handle: &str,
    observation: &BuyerClaimObservation,
) -> Value {
    let mut event = json!({
        "schema": BUYER_EVENT_SCHEMA,
        "seq": seq,
        "ts_unix": ts_unix,
        "session_id": session_id,
        "operation": operation,
        "event": observation.event_name(),
    });
    if let (Some(event), Some(fields)) = (
        event.as_object_mut(),
        observation.event_fields().as_object(),
    ) {
        event.extend(fields.clone());
        event.insert("deal_handle".to_string(), json!(deal_handle));
    }
    event
}
