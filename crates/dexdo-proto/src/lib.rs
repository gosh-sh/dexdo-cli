//! `dexdo-proto` -- the canonical stream format and the gateway's gRPC service.
//! In the canonical chunk carries fake tokens (mock model).

/// Generated tonic/prost types for the `dexdo.v1` package.

/// `tonic_build` owns these signatures; their large error type is `tonic::Status`, not authored API.
#[allow(clippy::result_large_err)]
pub mod v1 {
    tonic::include_proto!("dexdo.v1");
}

pub use v1::{
    gateway_client::GatewayClient,
    gateway_server::{Gateway, GatewayServer},
    BillingUsage, CanonChunk, CanonRequest, Challenge, ChallengeRequest, ChatMessage,
    SamplingParams, SignalManifest, StreamRequest,
};

impl BillingUsage {
    /// Build one validated provider-native billing record.
    pub fn new(
        input_tokens: u64,
        output_tokens: u64,
        declared_total: Option<u64>,
    ) -> Result<Self, &'static str> {
        let total_tokens = input_tokens
            .checked_add(output_tokens)
            .ok_or("billing usage input + output overflows u64")?;
        if declared_total.is_some_and(|declared| declared != total_tokens) {
            return Err("billing usage total does not equal input + output");
        }
        Ok(Self {
            input_tokens,
            output_tokens,
            total_tokens,
        })
    }

    /// Validate a record decoded from an untrusted peer.
    pub fn validate(&self) -> Result<(), &'static str> {
        Self::new(
            self.input_tokens,
            self.output_tokens,
            Some(self.total_tokens),
        )
        .map(|_| ())
    }
}

impl CanonChunk {
    /// Lower bound on visible output tokens proved by one normalized content chunk.
    /// Structured token IDs are exact; a textual/reasoning delta without IDs proves one token;
    /// metadata-only and terminal-usage frames prove no output tokens.
    pub fn visible_output_tokens(&self) -> u64 {
        if !self.token_ids.is_empty() {
            self.token_ids.len() as u64
        } else if !self.text.is_empty() || !self.reasoning.is_empty() {
            1
        } else {
            0
        }
    }
}

#[cfg(test)]
mod billing_usage_tests {
    use super::BillingUsage;

    #[test]
    fn checked_total_accepts_the_exact_sum_and_rejects_a_false_declaration() {
        assert_eq!(BillingUsage::new(7, 11, Some(18)).unwrap().total_tokens, 18);
        assert_eq!(
            BillingUsage::new(7, 11, Some(17)).unwrap_err(),
            "billing usage total does not equal input + output"
        );
    }

    #[test]
    fn checked_total_rejects_u64_overflow() {
        assert_eq!(
            BillingUsage::new(u64::MAX, 1, None).unwrap_err(),
            "billing usage input + output overflows u64"
        );
    }
}
