//! Buyer client: read the enc endpoint from a file, decrypt it with the note,
//! connect, sign the challenge (B18), receive the incremental stream, account ticks, STOP.

pub mod api;
pub mod continuity;
pub mod render;
pub mod routing;
pub mod tls;
pub mod verify;

use crate::seller::auth::challenge_bytes;
use anyhow::{anyhow, Result};
use dexdo_core::{
    params::{
        SampleAlgorithm, DEFAULT_SPOTCHECK_PROBE, DEFAULT_SPOTCHECK_THRESHOLD,
        REFERENCE_SAMPLE_SEED, REFERENCE_TOP_K,
    },
    ChainBackend, Handover, LocalNote, Note, TokenContract,
};
use dexdo_proto::{
    BillingUsage, CanonChunk, CanonRequest, ChallengeRequest, ChatMessage, GatewayClient,
    SamplingParams, StreamRequest,
};
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

/// Outcome of the buyer receiving a stream.
#[derive(Debug)]
pub struct StreamOutcome {
    /// Received fake tokens (text concatenated).
    pub tokens: Vec<String>,
    /// Provider-side reasoning/thinking deltas, when separated from visible content.
    pub reasoning: Vec<String>,
    /// How many chunks were received.
    pub received: u64,
    /// Validated provider-native terminal usage accepted by both peers.
    pub usage: BillingUsage,
}

/// Buyer client with a note (private key -- for decryption and signing).
pub struct Buyer {
    /// Buyer's note -- **polymorphic**: `LocalNote` (mock) OR `RealNote` (a real chain).
    /// Its ed25519 is registered on-chain (`placeInferenceBuy`); the seller reconstructs the
    /// x25519 handover from it. `decrypt`/`sign` go through the `Note` trait, the note type is
    /// transparent to the scenario.
    pub note: Arc<dyn Note>,
}

impl Buyer {
    /// Generate a buyer with a fresh **ephemeral** note -- only for the mock fixture of a single
    /// e2e run; the production path is `from_note` with a persistent identity.
    pub fn generate() -> Self {
        Self {
            note: Arc::new(LocalNote::generate()),
        }
    }

    /// Buyer with a **loaded persistent** note: identity from
    /// `--note-key`/wallet, reused across runs -- its orders/deals are visible again.
    /// The note is polymorphic (`Arc<dyn Note>`): the mock path yields `LocalNote`, the real
    /// path `RealNote`.
    pub fn from_note(note: Arc<dyn Note>) -> Self {
        Self { note }
    }

    /// Buyer step: place a buy order -- the note's public key is written into
    /// `token_contract`.
    pub async fn place_buy(
        &self,
        chain: &dyn ChainBackend,
        token_contract: &TokenContract,
    ) -> Result<()> {
        chain
            .place_buy(token_contract, self.note.as_ref())
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Read the handover from the endpoints file and decrypt it with the note key.
    /// Returns {endpoint, TLS fingerprint}. The same path in -- only the
    /// file contents change.
    pub async fn resolve_endpoint(
        &self,
        chain: &dyn ChainBackend,
        token_contract: &TokenContract,
    ) -> Result<Handover> {
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        let enc = chain
            .read_handover(token_contract)
            .await
            .map_err(|e| anyhow!(e))?
            .ok_or_else(|| anyhow!("no handover for {token_contract_display}"))?;
        let plain = self.note.decrypt(&enc).map_err(|e| {
            // DX guard: a real-chain handover is encrypted to the buyer's x25519 RECONSTRUCTED from
            // its on-chain ed25519 (Montgomery form, `x25519_pub_from_ed25519_pub`). A mock/HKDF identity
            // (`--mock-chain` / `LocalNote::from_seed`) derives a DIFFERENT x25519 (note.rs "do not mix"), so it
            // can NEVER decrypt a real seller's ciphertext. Fail closed with an actionable message -- the cause is
            // a mock/real mix, not a protocol bug -- instead of the opaque "decrypt failed".
            let pk = self.note.pubkey();
            if dexdo_core::note::x25519_pub_from_ed25519_pub(&pk.ed) != Some(pk.x) {
                anyhow!(
                    "handover decrypt failed: this note's x25519 is not the chain-reconstructible Montgomery form \
                     (it is a mock/HKDF identity -- e.g. `--mock-chain` or a `from_seed` note). A real-chain \
                     handover needs a real RealNote; you are mixing a mock note/endpoints-file with a real seller."
                )
            } else {
                anyhow!("handover decrypt failed: {e}")
            }
        })?;
        // E2E-OPEN-07 attribution: accept the payload only as THIS deal's handover. Decryption
        // proves the matched buyer, never the deal -- the same buyer's other deals are encrypted to
        // the same key -- so a replayed ciphertext would otherwise resolve into an endpoint and put
        // the deal on the money path. Fail closed here, on the existing malformed-handover error.
        let handover = Handover::from_deal_bytes(&plain, token_contract)
            .map_err(|e| anyhow!("malformed handover: {e}"))?;
        tracing::info!(
            token_contract = %token_contract_display,
            seller_gateway = %handover.endpoint,
            tls_fingerprint = %handover.tls_fingerprint,
            "buyer decrypted seller gateway handover"
        );
        Ok(handover)
    }

    /// Connect to the gateway over TLS, complete the
    /// challenge-response (B18) and receive the stream incrementally. On a fingerprint
    /// mismatch the connection does not come up -- the stream is not received (fail-closed).
    /// `max_tokens` -- both the output limit and total billing grant for this legacy helper. Because
    /// this API carries no locally trusted mock-mode fact, terminal output uses the real-provider
    /// lower-bound check; explicit mock consumer routes enable exact checking in their local config.
    pub async fn connect_and_stream(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        max_tokens: u64,
    ) -> Result<StreamOutcome> {
        self.connect_and_stream_request(
            handover,
            token_contract,
            max_tokens,
            max_tokens,
            None,
            None,
            None,
            false,
        )
        .await
    }

    /// Like [`connect_and_stream`], but carries a **canonical request**
    /// in the opening gRPC call alongside authorization (R1). Used by the consumer interface
    /// (B19/B20). `request = None` -- the path (neutral fake tokens).
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_and_stream_request(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        output_limit_tokens: u64,
        billing_grant_tokens: u64,
        request: Option<CanonRequest>,
        mut account_tokens: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
        mut account_output: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
        exact_mock_output: bool,
    ) -> Result<StreamOutcome> {
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        let mut client = self.connect(handover).await?;
        let signed = self
            .authorize(&mut client, token_contract, request, billing_grant_tokens)
            .await?;

        let mut stream = client.open_stream(signed).await?.into_inner();
        let mut tokens = Vec::new();
        let mut reasoning = Vec::new();
        let mut received = 0u64;
        let mut output_lower_bound = 0u64;
        let mut saw_output = false;
        let mut next_seq = 0u64;
        let mut terminal_usage = None;
        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(error) => return Err(error.into()),
            };
            if terminal_usage.is_some() {
                anyhow::bail!("canonical content followed terminal billing usage");
            }
            if let Some(usage) = chunk.usage {
                if !chunk.text.is_empty()
                    || !chunk.reasoning.is_empty()
                    || !chunk.token_ids.is_empty()
                    || chunk.manifest.is_some()
                {
                    anyhow::bail!("terminal billing usage frame also carries content");
                }
                if chunk.seq != next_seq {
                    anyhow::bail!(
                        "terminal billing usage sequence {} is not next expected {}",
                        chunk.seq,
                        next_seq
                    );
                }
                usage.validate().map_err(anyhow::Error::msg)?;
                if usage.output_tokens > output_limit_tokens {
                    anyhow::bail!(
                        "terminal output usage {} exceeds output limit {}",
                        usage.output_tokens,
                        output_limit_tokens
                    );
                }
                if usage.total_tokens > billing_grant_tokens {
                    anyhow::bail!(
                        "terminal billable usage {} exceeds billing grant {}",
                        usage.total_tokens,
                        billing_grant_tokens
                    );
                }
                let valid_output = if exact_mock_output {
                    usage.output_tokens == output_lower_bound
                } else {
                    usage.output_tokens >= output_lower_bound
                };
                if !valid_output {
                    anyhow::bail!(
                        "terminal output usage {} does not match visible output {}",
                        usage.output_tokens,
                        output_lower_bound
                    );
                }
                if !saw_output || usage.output_tokens == 0 {
                    anyhow::bail!("terminal billing usage has no preceding delivered output");
                }
                terminal_usage = Some(usage);
                continue;
            }
            if chunk.seq != next_seq {
                anyhow::bail!(
                    "canonical content sequence {} is not next expected {}",
                    chunk.seq,
                    next_seq
                );
            }
            next_seq = next_seq
                .checked_add(1)
                .ok_or_else(|| anyhow!("canonical content sequence overflows u64"))?;
            // Counted before the text/reasoning fields are moved out of the chunk below.
            let has_output = !chunk.text.is_empty()
                || !chunk.reasoning.is_empty()
                || !chunk.token_ids.is_empty();
            let accounted = if exact_mock_output || !chunk.token_ids.is_empty() {
                chunk.visible_output_tokens()
            } else {
                0
            };
            let legacy_accounted = crate::buyer::api::accounted_tokens(&chunk);
            let next_accounted = output_lower_bound
                .checked_add(accounted)
                .filter(|total| *total <= output_limit_tokens)
                .ok_or_else(|| {
                    anyhow!(
                        "deal {token_contract_display}: probe chunk carries {accounted} tokens with \
                         {output_lower_bound} of the {output_limit_tokens}-token output limit already accepted; \
                         refusing the noncompliant chunk before it crosses the output limit"
                    )
                })?;
            saw_output |= has_output;
            if let Some(record) = account_output.as_mut() {
                (*record)(legacy_accounted).map_err(anyhow::Error::msg)?;
            }
            if !chunk.text.is_empty() {
                tokens.push(chunk.text);
            }
            if !chunk.reasoning.is_empty() {
                reasoning.push(chunk.reasoning);
            }
            received += 1;
            output_lower_bound = next_accounted;
        }
        let usage = terminal_usage
            .ok_or_else(|| anyhow!("canonical stream ended without terminal billing usage"))?;
        if let Some(charge) = account_tokens.as_mut() {
            (*charge)(usage.total_tokens).map_err(anyhow::Error::msg)?;
        }
        Ok(StreamOutcome {
            tokens,
            reasoning,
            received,
            usage,
        })
    }

    /// Open an authorized canonical stream to the gateway with a canonical
    /// request and return the incremental `CanonChunk` stream for re-rendering to
    /// SSE by the consumer interface (B19/B20). Output verification happens on this channel before
    /// re-rendering; monetary accounting accepts the checked terminal usage only after clean EOF.
    pub async fn open_canon_stream(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        request: CanonRequest,
        billing_grant_tokens: u64,
    ) -> Result<tonic::Streaming<CanonChunk>> {
        let mut client = self.connect(handover).await?;
        let signed = self
            .authorize(
                &mut client,
                token_contract,
                Some(request),
                billing_grant_tokens,
            )
            .await?;
        Ok(client.open_stream(signed).await?.into_inner())
    }

    /// **Behavioral probe:** the buyer sends a deterministic probe prompt for the
    /// claimed exact/reference model (indistinguishable from normal traffic -- it's an ordinary canonical
    /// request), collects the response and checks it against that model's fingerprint
    /// ([`verify::behavioral_check`]). Mismatch -> the model is not the one claimed -> `Bail`.
    /// No exact-model fingerprint -> degradation (`Pass`). The probe is a separate request (spends the
    /// tick budget), sent as an ordinary completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn behavioral_probe(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        model_id: &str,
        output_limit_tokens: u64,
        billing_grant_tokens: u64,
        models: &crate::seller::ModelsConfig,
        account_tokens: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
        account_output: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
    ) -> Result<crate::buyer::verify::Verdict> {
        use crate::buyer::verify;
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        let Some(probe_prompt) = verify::default_probe(model_id, models) else {
            // Nothing was asked of the seller, so nothing was consumed.
            return Ok(verify::Verdict::Pass);
        };
        // the caller's grant is what bounds this probe. Zero means the reservation cannot pay
        // for it, and issuing anyway would be worse than refusing: `max_tokens == 0` is "unset" on
        // the wire and asks the seller for an unbounded generation.
        if billing_grant_tokens == 0 {
            anyhow::bail!(
                "deal {token_contract_display}: the admitted grant cannot pay for the identity verification \
                 this deal still owes; refusing rather than probing on credit"
            );
        }
        // The seller derives its upstream token limit from `params.max_tokens`, so `None` here used
        // to ask for an UNBOUNDED generation clipped only by the deal's whole remaining capacity.
        // B7 has always set this; B8 must too. The receiver independently enforces the same
        // figure against visible output tokens, even when a noncompliant seller chunks around it.
        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: probe_prompt.clone(),
            }],
            params: Some(SamplingParams {
                temperature: 0.0,
                max_tokens: u32::try_from(output_limit_tokens).unwrap_or(u32::MAX),
                stop: Vec::new(),
                greedy: false,
            }),
        };
        let outcome = self
            .connect_and_stream_request(
                handover,
                token_contract,
                output_limit_tokens,
                billing_grant_tokens,
                Some(request),
                account_tokens,
                account_output,
                false,
            )
            .await?;
        let response = outcome.tokens.join("");
        let reasoning = outcome.reasoning.join("");
        Ok(verify::behavioral_check_with_reasoning(
            model_id,
            &probe_prompt,
            &response,
            &reasoning,
            models,
        ))
    }

    /// **B7 full spot-check:** the buyer sends a **greedy** probe simultaneously to the
    /// seller and to the **official reference endpoint** of the claimed exact/reference model, and
    /// compares their normalized leading-word prefix ([`verify::prefix_agreement`]). The common
    /// prefix must cover at least the canonical fraction of the full reference; agreement below it
    /// means that the model is not the one
    /// claimed -> `Bail`. No exact-model reference / no key in env / reference unavailable ->
    /// **degradation** (`Pass`, R3 -- we don't penalize the seller for the absence/failure of our
    /// reference). The probe is a separate request (spends the tick budget), sent over the ordinary
    /// completion path -- no audit-only API, no second channel.

    /// It is NOT, however, indistinguishable to the seller: this probe sets `greedy: true` while
    /// every ordinary buyer request sets `greedy: false`, so a single boolean on the wire names the audit
    /// with no false positives, and the probe prompt itself is a fixed public constant
    /// ([`DEFAULT_SPOTCHECK_PROBE`]). A seller that answers this one request honestly and serves anything
    /// cheaper afterwards is not caught by this check. Read the gate as what it is -- a sampled detector of
    /// a substituting-by-default seller, not a defence against one that watches for the flag; making the
    /// audit unidentifiable is tracked in and is a design decision, not a wording fix.

    /// The cheap B7 (claimed-vs-frame) stays separate; this is the strong sampled cross-check (1-5%).
    #[allow(clippy::too_many_arguments)]
    pub async fn reference_spotcheck(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        model_id: &str,
        output_limit_tokens: u64,
        billing_grant_tokens: u64,
        models: &crate::seller::ModelsConfig,
        account_tokens: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
        account_output: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
    ) -> Result<crate::buyer::verify::Verdict> {
        use crate::buyer::verify::{
            self, prefix_agreement, reference_endpoint_for, spotcheck_verdict,
        };
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        let Some(endpoint) = reference_endpoint_for(model_id, models) else {
            return Ok(verify::Verdict::Pass); // no exact-model reference -> degradation (R3)
        };
        let api_key = match std::env::var(&endpoint.api_key_env) {
            Ok(k) if !k.is_empty() => k,
            _ => return Ok(verify::Verdict::Pass), // no reference key -> degradation (R3)
        };

        // the caller's grant is what bounds this probe. Zero means the reservation cannot pay
        // for it, and issuing anyway would be worse than refusing: `max_tokens == 0` is "unset" on
        // the wire and asks the seller for an unbounded generation.
        if billing_grant_tokens == 0 {
            anyhow::bail!(
                "deal {token_contract_display}: the admitted grant cannot pay for the identity verification \
                 this deal still owes; refusing rather than probing on credit"
            );
        }
        // Both legs use the same profile-declared reproducibility control. Profiles without a
        // comparable control degrade before this request is built.
        let probe = DEFAULT_SPOTCHECK_PROBE;
        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: probe.to_string(),
            }],
            params: Some(SamplingParams {
                temperature: 0.0,
                max_tokens: u32::try_from(output_limit_tokens).unwrap_or(u32::MAX),
                stop: Vec::new(),
                greedy: true, // temp=0 plus the profile-declared provider control on both legs
            }),
        };
        let outcome = self
            .connect_and_stream_request(
                handover,
                token_contract,
                output_limit_tokens,
                billing_grant_tokens,
                Some(request),
                account_tokens,
                account_output,
                false,
            )
            .await?;
        let seller_content = outcome.tokens.join("");
        let seller_reasoning = outcome.reasoning.join("");
        let seller_response = content_or_reasoning(&seller_content, &seller_reasoning);
        // Greedy probe to the reference. Network/endpoint failure -> degradation (our fault, not the seller's).
        let reference_response = match reference_completion(
            &endpoint,
            &api_key,
            probe,
            u32::try_from(output_limit_tokens).unwrap_or(u32::MAX),
        )
        .await
        {
            Ok(t) => t,
            Err(_) => return Ok(verify::Verdict::Pass),
        };

        let agreement = prefix_agreement(&seller_response, &reference_response);
        Ok(spotcheck_verdict(agreement, DEFAULT_SPOTCHECK_THRESHOLD))
    }

    /// Open a gRPC channel to the gateway over TLS, pinning the fingerprint from the handover.
    async fn connect(&self, handover: &Handover) -> Result<GatewayClient<Channel>> {
        // the address being dialled and the real cause are attached HERE, where the endpoint
        // is known. Reporting `?` straight through gave the operator `transport error` and nothing
        // else, with the address absent from the log even at `RUST_LOG=trace`.
        let channel = tls::connect_pinned(&handover.endpoint, &handover.tls_fingerprint)
            .await
            .map_err(|error| tls::gateway_dial_error(&handover.endpoint, error))?;
        Ok(GatewayClient::new(channel))
    }

    /// Challenge-response: get a nonce, sign `challenge_bytes` with the note key.
    /// The canonical request (`request`) travels in the opening call alongside the signature (R1).
    async fn authorize(
        &self,
        client: &mut GatewayClient<Channel>,
        token_contract: &TokenContract,
        request: Option<CanonRequest>,
        billing_grant_tokens: u64,
    ) -> Result<StreamRequest> {
        let challenge = client
            .get_challenge(ChallengeRequest {
                token_contract: token_contract.clone(),
            })
            .await?
            .into_inner();
        let msg = challenge_bytes(token_contract, &challenge.nonce);
        let sig = self.note.sign(&msg);
        Ok(StreamRequest {
            token_contract: token_contract.clone(),
            nonce: challenge.nonce,
            signature: sig.0.to_vec(),
            request,
            billing_grant_tokens,
        })
    }
}

/// Greedy (temp=0) request to the official reference endpoint:
/// non-streaming `chat/completions`, return the comparable text. Some reasoning models return provider-side
/// reasoning first and visible content only after enough tokens; that reasoning is still a model identity signal
/// and matches the gateway's `CanonChunk.reasoning` side channel. The key goes only into the Authorization
/// header and is NOT logged (secret masking).
async fn reference_completion(
    endpoint: &crate::buyer::verify::ReferenceEndpoint,
    api_key: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String> {
    let url = format!("{}/chat/completions", endpoint.base_url);
    let body = reference_request_body(endpoint, prompt, max_tokens);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!("reference endpoint HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await?;
    let message = &v["choices"][0]["message"];
    let content = message["content"].as_str().unwrap_or("");
    let reasoning = reference_reasoning_text(message);
    Ok(content_or_reasoning(content, &reasoning))
}

fn reference_request_body(
    endpoint: &crate::buyer::verify::ReferenceEndpoint,
    prompt: &str,
    max_tokens: u32,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": endpoint.model.clone(),
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": 0.0,
        "max_tokens": max_tokens,
        "stream": false,
    });
    match endpoint.sample_algorithm {
        SampleAlgorithm::Seed => body["seed"] = serde_json::json!(REFERENCE_SAMPLE_SEED),
        SampleAlgorithm::RandomSeed => {
            body["random_seed"] = serde_json::json!(REFERENCE_SAMPLE_SEED)
        }
        SampleAlgorithm::TopK => body["top_k"] = serde_json::json!(REFERENCE_TOP_K),
        SampleAlgorithm::None => {}
    }
    body
}

fn content_or_reasoning(content: &str, reasoning: &str) -> String {
    if content.trim().is_empty() && !reasoning.trim().is_empty() {
        reasoning.to_string()
    } else {
        content.to_string()
    }
}

fn reference_reasoning_text(message: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    for field in ["reasoning", "reasoning_content"] {
        if let Some(value) = message[field].as_str().filter(|v| !v.trim().is_empty()) {
            parts.push(value.to_string());
        }
    }
    if let Some(details) = message["reasoning_details"].as_array() {
        for detail in details {
            for field in ["text", "summary"] {
                if let Some(value) = detail[field].as_str().filter(|v| !v.trim().is_empty()) {
                    parts.push(value.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{content_or_reasoning, reference_reasoning_text, reference_request_body, Buyer};
    use crate::buyer::verify::{ReferenceEndpoint, Verdict};
    use crate::seller::ModelsConfig;
    use dexdo_core::params::SampleAlgorithm;
    use dexdo_core::Handover;

    #[test]
    fn issue_1951_reference_probe_uses_the_declared_sample_algorithm() {
        let mut endpoint = ReferenceEndpoint {
            base_url: "https://provider.example/v1".to_string(),
            model: "provider/model".to_string(),
            api_key_env: "PROVIDER_KEY".to_string(),
            sample_algorithm: SampleAlgorithm::None,
        };
        let control_fields = ["seed", "random_seed", "top_k"];
        for (algorithm, expected_field, expected_value) in [
            (SampleAlgorithm::Seed, "seed", serde_json::json!(0)),
            (
                SampleAlgorithm::RandomSeed,
                "random_seed",
                serde_json::json!(0),
            ),
            (SampleAlgorithm::TopK, "top_k", serde_json::json!(1)),
        ] {
            endpoint.sample_algorithm = algorithm;
            let body = reference_request_body(&endpoint, "probe", 64);
            assert_eq!(body[expected_field], expected_value);
            assert_eq!(
                control_fields
                    .iter()
                    .filter(|field| body.get(**field).is_some())
                    .count(),
                1,
                "{algorithm:?} must add exactly one field: {body}"
            );
        }

        endpoint.sample_algorithm = SampleAlgorithm::None;
        let body = reference_request_body(&endpoint, "probe", 64);
        assert!(
            control_fields
                .iter()
                .all(|field| body.get(*field).is_none()),
            "NONE must add no greedy control: {body}"
        );
    }

    #[tokio::test]
    // regression: independently generated answers from a profile that declares no comparable
    // reproducibility control must never reach the prefix comparator or contact either endpoint.
    async fn issue_1951_none_degrades_reference_spotcheck_without_network() {
        let models = ModelsConfig::from_json(
            r#"{"models":{"m":{"frame_model":"vendor--model--v1",
              "base_url":"https://must-not-be-contacted.invalid/v1","served_model":"provider/model",
              "api_key_env":"PATH","tokenizer_family":"vendor","price_per_tick":1,
              "capabilities":{"max_output_tokens":256,"sample_algorithm":"NONE"}}}}"#,
        )
        .expect("NONE profile parses");
        let verdict = Buyer::generate()
            .reference_spotcheck(
                &Handover {
                    endpoint: "must-not-be-contacted.invalid:1".to_string(),
                    tls_fingerprint: "invalid".to_string(),
                },
                &"not-a-token-contract".to_string(),
                "m",
                64,
                64,
                &models,
                None,
                None,
            )
            .await
            .expect("NONE degrades instead of surfacing a network/502 failure");
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn reference_text_prefers_visible_content() {
        assert_eq!(content_or_reasoning("answer", "thinking"), "answer");
    }

    #[test]
    fn reference_text_falls_back_to_reasoning() {
        assert_eq!(content_or_reasoning("", "thinking"), "thinking");
    }

    #[test]
    fn reference_reasoning_collects_provider_fields() {
        let message = serde_json::json!({
            "content": "",
            "reasoning": "raw",
            "reasoning_content": "alias",
            "reasoning_details": [
                { "text": "detail" },
                { "summary": "summary" },
                { "data": "ignored" }
            ]
        });
        let reasoning = reference_reasoning_text(&message);
        assert!(reasoning.contains("raw"));
        assert!(reasoning.contains("alias"));
        assert!(reasoning.contains("detail"));
        assert!(reasoning.contains("summary"));
        assert!(!reasoning.contains("ignored"));
    }
}
