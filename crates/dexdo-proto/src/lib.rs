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

/// A transient provider failure before output; the gateway is still alive.
/// Preserve the original gRPC code/message and carry the recovery distinction in metadata.
pub fn retryable_upstream_status(mut status: tonic::Status) -> tonic::Status {
    status
        .metadata_mut()
        .insert("dexdo-error-code", "upstream_retryable".parse().unwrap());
    status
}

pub fn is_retryable_upstream(status: &tonic::Status) -> bool {
    status
        .metadata()
        .get("dexdo-error-code")
        .is_some_and(|code| code == "upstream_retryable")
}

/// A provider request failed while the seller gateway remained reachable.
/// Output evidence is independent from retryability and survives gRPC translation.
pub fn upstream_failure_status(mut status: tonic::Status, output_started: bool) -> tonic::Status {
    status.metadata_mut().insert(
        "dexdo-upstream-output",
        if output_started { "started" } else { "none" }
            .parse()
            .unwrap(),
    );
    status
}

pub fn upstream_output_started(status: &tonic::Status) -> Option<bool> {
    match status
        .metadata()
        .get("dexdo-upstream-output")?
        .to_str()
        .ok()?
    {
        "none" => Some(false),
        "started" => Some(true),
        _ => None,
    }
}

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
            charged_tokens: None,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        })
    }

    /// Validate a record decoded from an untrusted peer.
    pub fn validate(&self) -> Result<(), &'static str> {
        Self::new(
            self.input_tokens,
            self.output_tokens,
            Some(self.total_tokens),
        )?;
        if self.billable_tokens() > self.total_tokens {
            return Err("charged tokens exceed provider usage");
        }
        let cached = self
            .cache_read_tokens
            .checked_add(self.cache_write_tokens)
            .ok_or("cache token counts overflow u64")?;
        if cached > self.input_tokens {
            return Err("cache tokens exceed provider input usage");
        }
        Ok(())
    }

    /// Payment accepted from the deal, separately from provider-native work.
    pub fn billable_tokens(&self) -> u64 {
        self.charged_tokens.unwrap_or(self.total_tokens)
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
    #[test]
    fn shared_budget_2091_charge_is_separate_bounded_and_may_be_zero() {
        let mut usage = BillingUsage::new(20, 30, None).unwrap();
        usage.charged_tokens = Some(0);
        usage.validate().unwrap();
        assert_eq!(usage.total_tokens, 50);
        assert_eq!(usage.billable_tokens(), 0);
        usage.charged_tokens = Some(51);
        assert!(usage.validate().is_err());
    }

    #[test]
    fn cache_counts_are_bounded_subsets_and_survive_the_wire() {
        use prost::Message;
        let mut usage = BillingUsage::new(12, 3, None).unwrap();
        usage.cache_read_tokens = 7;
        usage.cache_write_tokens = 2;
        usage.validate().unwrap();
        let decoded = BillingUsage::decode(usage.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, usage);
        assert_eq!(decoded.billable_tokens(), 15);
        usage.cache_write_tokens = 6;
        assert_eq!(
            usage.validate(),
            Err("cache tokens exceed provider input usage")
        );
        usage.cache_read_tokens = u64::MAX;
        assert_eq!(usage.validate(), Err("cache token counts overflow u64"));
    }
}

#[cfg(test)]
mod reasoning_wire_compatibility_tests {
    use super::{Challenge, ChallengeRequest, SamplingParams};
    use prost::Message;

    #[derive(Clone, PartialEq, Message)]
    struct LegacyChallengeRequest {
        #[prost(string, tag = "1")]
        token_contract: String,
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacyChallenge {
        #[prost(bytes = "vec", tag = "1")]
        nonce: Vec<u8>,
        #[prost(string, tag = "2")]
        token_contract: String,
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacySamplingParams {
        #[prost(double, tag = "1")]
        temperature: f64,
        #[prost(uint32, tag = "2")]
        max_tokens: u32,
        #[prost(string, repeated, tag = "3")]
        stop: Vec<String>,
        #[prost(bool, tag = "4")]
        greedy: bool,
    }

    #[test]
    fn capabilities_only_is_an_additive_challenge_request() {
        let old = LegacyChallengeRequest {
            token_contract: "deal".into(),
        };
        let new = ChallengeRequest::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(!new.capabilities_only);
        assert_eq!(new.encode_to_vec(), old.encode_to_vec());
        let query = ChallengeRequest {
            capabilities_only: true,
            ..new
        };
        let ignored = LegacyChallengeRequest::decode(query.encode_to_vec().as_slice()).unwrap();
        assert_eq!(ignored, old);
    }

    #[test]
    fn old_seller_reply_declares_no_reasoning_support() {
        let old = LegacyChallenge {
            nonce: vec![1, 2, 3],
            token_contract: "deal".into(),
        };
        let new = Challenge::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(!new.supports_reasoning_disable);
        assert!(new.supported_reasoning_efforts.is_empty());
        assert_eq!(new.nonce, old.nonce);
        assert_eq!(new.token_contract, old.token_contract);
    }

    #[test]
    fn old_buyer_reads_new_seller_reply() {
        let new = Challenge {
            nonce: vec![1, 2, 3],
            token_contract: "deal".into(),
            supports_reasoning_disable: true,
            supported_reasoning_efforts: vec!["high".into()],
        };
        let old = LegacyChallenge::decode(new.encode_to_vec().as_slice()).unwrap();
        assert_eq!(old.nonce, new.nonce);
        assert_eq!(old.token_contract, new.token_contract);
    }

    #[test]
    fn reasoning_2172_effort_survives_the_wire_without_disabling_reasoning() {
        for effort in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            let params = SamplingParams {
                reasoning_effort: Some(effort.into()),
                ..Default::default()
            };
            let decoded = SamplingParams::decode(params.encode_to_vec().as_slice()).unwrap();
            assert_eq!(decoded.reasoning_effort.as_deref(), Some(effort));
            assert!(!decoded.disable_reasoning);
            let legacy = LegacySamplingParams::decode(params.encode_to_vec().as_slice()).unwrap();
            assert_eq!(legacy, LegacySamplingParams::default());
        }
    }

    #[test]
    fn omitted_reasoning_preserves_the_legacy_request_bytes() {
        let old = LegacySamplingParams {
            temperature: 0.5,
            max_tokens: 8,
            stop: vec!["END".into()],
            greedy: false,
        };
        let new = SamplingParams::decode(old.encode_to_vec().as_slice()).unwrap();
        assert!(!new.disable_reasoning);
        assert!(new.reasoning_effort.is_none());
        assert_eq!(new.encode_to_vec(), old.encode_to_vec());
        let disabled = SamplingParams {
            disable_reasoning: true,
            ..new
        };
        let ignored = LegacySamplingParams::decode(disabled.encode_to_vec().as_slice()).unwrap();
        assert_eq!(
            ignored, old,
            "old sellers ignore the new field: buyer capability gating is essential"
        );
    }
}
