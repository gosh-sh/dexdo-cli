//! `dexdo-proto` — the canonical stream format and the gateway's gRPC service (§3.1.1, §6, R2/R6/R16).
//! In directive 1 the canonical chunk carries fake tokens (mock model).

/// Generated tonic/prost types for the `dexdo.v1` package.
pub mod v1 {
    tonic::include_proto!("dexdo.v1");
}

pub use v1::{
    gateway_client::GatewayClient,
    gateway_server::{Gateway, GatewayServer},
    CanonChunk, CanonRequest, Challenge, ChallengeRequest, ChatMessage, SamplingParams,
    SignalManifest, StreamRequest, TokenLogprobs, TopLogprob,
};
