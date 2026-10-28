//! `dexdo-proto` -- the gateway's gRPC service: the consumer request travels to the
//! seller as received, and the provider response returns to the buyer as produced.

/// Generated tonic/prost types for the `dexdo.v1` package.

/// `tonic_build` owns these signatures; their large error type is `tonic::Status`, not authored API.
#[allow(clippy::result_large_err)]
pub mod v1 {
    tonic::include_proto!("dexdo.v1");
}

pub use v1::{
    gateway_client::GatewayClient,
    gateway_server::{Gateway, GatewayServer},
    BillingUsage, Challenge, ChallengeRequest, Header, ResponseHead, StreamFrame, StreamRequest,
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
mod version_mismatch_wire_tests {
    use super::{Challenge, ChallengeRequest, StreamRequest};
    use prost::Message;

    /// The request an older buyer sends: its prompt travels in field 4, which this schema leaves unused.
    #[derive(Clone, PartialEq, Message)]
    struct OlderBuyerStreamRequest {
        #[prost(string, tag = "1")]
        token_contract: String,
        #[prost(bytes = "vec", tag = "2")]
        nonce: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        signature: Vec<u8>,
        #[prost(bytes = "vec", tag = "4")]
        request: Vec<u8>,
        #[prost(uint64, tag = "5")]
        billing_grant_tokens: u64,
        #[prost(bool, tag = "6")]
        shared_budget: bool,
    }

    /// The challenge an older seller answers with: fields 3 and 4 are unused here.
    #[derive(Clone, PartialEq, Message)]
    struct OlderSellerChallenge {
        #[prost(bytes = "vec", tag = "1")]
        nonce: Vec<u8>,
        #[prost(string, tag = "2")]
        token_contract: String,
        #[prost(bool, tag = "3")]
        flag: bool,
        #[prost(string, repeated, tag = "4")]
        levels: Vec<String>,
    }

    #[test]
    fn an_older_buyer_request_decodes_without_a_path_or_body() {
        let older = OlderBuyerStreamRequest {
            token_contract: "deal".into(),
            nonce: vec![1, 2],
            signature: vec![3; 64],
            request: vec![10, 3, 10, 1, 120],
            billing_grant_tokens: 64,
            shared_budget: true,
        };
        let decoded = StreamRequest::decode(older.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.token_contract, "deal");
        assert_eq!(decoded.billing_grant_tokens, 64);
        assert!(decoded.shared_budget);
        assert!(decoded.path.is_empty());
        assert!(decoded.body.is_empty());
        assert!(decoded.headers.is_empty());
    }

    /// The challenge an older buyer reads: only the nonce and the deal.
    #[derive(Clone, PartialEq, Message)]
    struct OlderBuyerChallenge {
        #[prost(bytes = "vec", tag = "1")]
        nonce: Vec<u8>,
        #[prost(string, tag = "2")]
        token_contract: String,
    }

    /// The challenge request an older buyer sends: only the deal.
    #[derive(Clone, PartialEq, Message)]
    struct OlderBuyerChallengeRequest {
        #[prost(string, tag = "1")]
        token_contract: String,
    }

    /// An older buyer still authorizes against a new seller: the declared paths are a field it
    /// skips, and the nonce and deal read as before. Its later request is what the seller refuses.
    #[test]
    fn an_older_buyer_reads_a_new_seller_challenge() {
        let new = Challenge {
            nonce: vec![1, 2, 3],
            token_contract: "deal".into(),
            api_paths: vec!["/v1/chat/completions".into(), "/v1/responses".into()],
        };
        let older = OlderBuyerChallenge::decode(new.encode_to_vec().as_slice()).unwrap();
        assert_eq!(older.nonce, new.nonce);
        assert_eq!(older.token_contract, new.token_contract);
    }

    /// `capabilities_only` is additive: an older request decodes as an ordinary challenge with
    /// unchanged bytes, and an older seller reads a capability query as an ordinary one.
    #[test]
    fn capabilities_only_is_an_additive_challenge_request() {
        let older = OlderBuyerChallengeRequest {
            token_contract: "deal".into(),
        };
        let new = ChallengeRequest::decode(older.encode_to_vec().as_slice()).unwrap();
        assert!(!new.capabilities_only);
        assert_eq!(new.encode_to_vec(), older.encode_to_vec());
        let query = ChallengeRequest {
            capabilities_only: true,
            ..new
        };
        let read_by_older =
            OlderBuyerChallengeRequest::decode(query.encode_to_vec().as_slice()).unwrap();
        assert_eq!(read_by_older, older);
    }

    #[test]
    fn an_older_seller_challenge_declares_no_api_paths() {
        let older = OlderSellerChallenge {
            nonce: vec![7],
            token_contract: "deal".into(),
            flag: true,
            levels: vec!["high".into()],
        };
        let decoded = Challenge::decode(older.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.nonce, vec![7]);
        assert!(decoded.api_paths.is_empty());
    }
}
