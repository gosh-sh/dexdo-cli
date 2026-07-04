//! Buyer client: read the enc endpoint from a file, decrypt it with the note,
//! connect, sign the challenge(B18), receive the incremental stream, account ticks, STOP.

pub mod api;
pub mod continuity;
pub mod render;
pub mod routing;
pub mod tls;
pub mod verify;

use crate::seller::auth::challenge_bytes;
use anyhow::{anyhow, Result};
use dexdo_core::{ChainBackend, Handover, LocalNote, Note, TokenContract};
use dexdo_proto::{
    CanonChunk, CanonRequest, ChallengeRequest, ChatMessage, GatewayClient, SamplingParams,
    StreamRequest,
};
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

/// Outcome of the buyer receiving a stream.
#[derive(Debug)]
pub struct StreamOutcome {
    /// Received fake tokens(text concatenated).
    pub tokens: Vec<String>,
    /// Provider-side reasoning/thinking deltas, when separated from visible content.
    pub reasoning: Vec<String>,
    /// How many chunks were received.
    pub received: u64,
}

/// Buyer client with a note(private key -- for decryption and signing).
pub struct Buyer {
    /// Buyer's note -- **polymorphic**: `LocalNote`(mock) OR `RealNote`(real shellnet).
    /// Its ed25519 is registered on-chain(`placeInferenceBuy`); the seller reconstructs the
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
    /// The note is polymorphic(`Arc<dyn Note>`): the mock path yields `LocalNote`, the real
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
        let enc = chain
            .read_handover(token_contract)
            .await
            .map_err(|e| anyhow!(e))?
            .ok_or_else(|| anyhow!("no handover for {token_contract}"))?;
        let plain = self.note.decrypt(&enc).map_err(|e| {
            // DX guard: a real-shellnet handover is encrypted to the buyer's x25519 RECONSTRUCTED from
            // its on-chain ed25519(Montgomery form, `x25519_pub_from_ed25519_pub`). A mock/HKDF identity
            // (`--mock-chain` / `LocalNote::from_seed`) derives a DIFFERENT x25519(note.rs "do not mix"), so it
            // can NEVER decrypt a real seller's ciphertext. Fail closed with an actionable message -- the cause is
            // a mock/real mix, not a protocol bug -- instead of the opaque "decrypt failed".
            let pk = self.note.pubkey();
            if dexdo_core::note::x25519_pub_from_ed25519_pub(&pk.ed) != Some(pk.x) {
                anyhow!(
                    "handover decrypt failed: this note's x25519 is not the chain-reconstructible Montgomery form \
                     (it is a mock/HKDF identity -- e.g. `--mock-chain` or a `from_seed` note). A real-shellnet \
                     handover needs a real RealNote; you are mixing a mock note/endpoints-file with a real seller."
                )
            } else {
                anyhow!("handover decrypt failed: {e}")
            }
        })?;
        Handover::from_bytes(&plain).map_err(|e| anyhow!("malformed handover: {e}"))
    }

    /// Connect to the gateway over TLS, complete the
    /// challenge-response(B18) and receive the stream incrementally. On a fingerprint
    /// mismatch the connection does not come up -- the stream is not received(fail-closed).
    /// `max_tokens` -- how many chunks to receive.
    pub async fn connect_and_stream(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        max_tokens: u64,
    ) -> Result<StreamOutcome> {
        self.connect_and_stream_request(handover, token_contract, max_tokens, None)
            .await
    }

    /// Like [`connect_and_stream`], but carries a **canonical request**
    /// in the opening gRPC call alongside authorization(R1). Used by the consumer interface
    /// (B19/B20). `request = None` -- the path(neutral fake tokens).
    pub async fn connect_and_stream_request(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        max_tokens: u64,
        request: Option<CanonRequest>,
    ) -> Result<StreamOutcome> {
        let mut client = self.connect(handover).await?;
        let signed = self.authorize(&mut client, token_contract, request).await?;

        let mut stream = client.open_stream(signed).await?.into_inner();
        let mut tokens = Vec::new();
        let mut reasoning = Vec::new();
        let mut received = 0u64;
        while let Some(item) = stream.next().await {
            let chunk = item?;
            if !chunk.text.is_empty() {
                tokens.push(chunk.text);
            }
            if !chunk.reasoning.is_empty() {
                reasoning.push(chunk.reasoning);
            }
            received += 1;
            // Tick accounting is done on-chain via advance_tick(called by the orchestrator);
            // here the buyer keeps a count of received tokens and may break off(STOP).
            if received >= max_tokens {
                break; // buyer stops receiving -> STOP is submitted as a separate on-chain action
            }
        }
        Ok(StreamOutcome {
            tokens,
            reasoning,
            received,
        })
    }

    /// Open an authorized canonical stream to the gateway with a canonical
    /// request and return the incremental `CanonChunk` stream for re-rendering to
    /// SSE by the consumer interface(B19/B20). Tick accounting/verification happen on this
    /// channel BEFORE re-rendering (in verification is a no-op, ticks are kept
    /// on-chain).
    pub async fn open_canon_stream(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        request: CanonRequest,
    ) -> Result<tonic::Streaming<CanonChunk>> {
        let mut client = self.connect(handover).await?;
        let signed = self
            .authorize(&mut client, token_contract, Some(request))
            .await?;
        Ok(client.open_stream(signed).await?.into_inner())
    }

    /// **Behavioral probe:** the buyer sends a deterministic probe prompt for the
    /// claimed exact/reference model (indistinguishable from normal traffic -- it's an ordinary canonical
    /// request), collects the response and checks it against that model's fingerprint
    /// ([`verify::behavioral_check`]). Mismatch -> the model is not the one claimed -> `Bail`.
    /// No exact-model fingerprint -> degradation(`Pass`). The probe is a separate request (spends the
    /// tick budget), sent as an ordinary completion.
    pub async fn behavioral_probe(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        model_id: &str,
        max_tokens: u64,
        models: &crate::seller::ModelsConfig,
    ) -> Result<crate::buyer::verify::Verdict> {
        use crate::buyer::verify;
        let Some(probe_prompt) = verify::default_probe(model_id, models) else {
            return Ok(verify::Verdict::Pass); // no exact-model fingerprint -> degradation(R3)
        };
        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: probe_prompt.clone(),
            }],
            params: None,
        };
        let outcome = self
            .connect_and_stream_request(handover, token_contract, max_tokens, Some(request))
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

    /// **B7 full spot-check:** the buyer sends a deterministic **greedy** probe(temp=0)
    /// SIMULTANEOUSLY to the seller AND to the **official reference endpoint** of the claimed exact/reference
    /// model, and compares them by prefix agreement
    /// ([`verify::prefix_agreement`]). Divergence above the threshold -> the model is not the one
    /// claimed -> `Bail`. No exact-model reference / no key in env / reference unavailable ->
    /// **degradation** (`Pass`, R3 -- we don't penalize the seller for the absence/failure of our
    /// reference). The probe is a separate request(spends the tick budget), sent as an ordinary
    /// completion(indistinguishable to the seller). The cheap B7(claimed-vs-frame) stays
    /// separate; this is the strong sampled cross-check(1-5%).
    pub async fn reference_spotcheck(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        model_id: &str,
        max_tokens: u64,
        models: &crate::seller::ModelsConfig,
    ) -> Result<crate::buyer::verify::Verdict> {
        use crate::buyer::verify::{
            self, prefix_agreement, reference_endpoint_for, spotcheck_verdict,
            DEFAULT_SPOTCHECK_THRESHOLD,
        };
        let Some(endpoint) = reference_endpoint_for(model_id, models) else {
            return Ok(verify::Verdict::Pass); // no exact-model reference -> degradation(R3)
        };
        let api_key = match std::env::var(&endpoint.api_key_env) {
            Ok(k) if !k.is_empty() => k,
            _ => return Ok(verify::Verdict::Pass), // no reference key -> degradation(R3)
        };

        // Deterministic probe: the claimed model's greedy output is characteristic and reproducible.
        let probe = SPOTCHECK_PROBE;
        let request = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: probe.to_string(),
            }],
            params: Some(SamplingParams {
                temperature: 0.0,
                max_tokens: max_tokens as u32,
                stop: Vec::new(),
                greedy: true, // forced temp=0 at the seller(deterministic cross-check)
            }),
        };
        let outcome = self
            .connect_and_stream_request(handover, token_contract, max_tokens, Some(request))
            .await?;
        let seller_content = outcome.tokens.join("");
        let seller_reasoning = outcome.reasoning.join("");
        let seller_response = content_or_reasoning(&seller_content, &seller_reasoning);

        // Greedy probe to the reference. Network/endpoint failure -> degradation(our fault, not the seller's).
        let reference_response =
            match reference_completion(&endpoint, &api_key, probe, max_tokens as u32).await {
                Ok(t) => t,
                Err(_) => return Ok(verify::Verdict::Pass),
            };

        let agreement = prefix_agreement(&seller_response, &reference_response);
        Ok(spotcheck_verdict(agreement, DEFAULT_SPOTCHECK_THRESHOLD))
    }

    /// Open a gRPC channel to the gateway over TLS, pinning the fingerprint from the handover.
    async fn connect(&self, handover: &Handover) -> Result<GatewayClient<Channel>> {
        let channel = tls::connect_pinned(&handover.endpoint, &handover.tls_fingerprint).await?;
        Ok(GatewayClient::new(channel))
    }

    /// Challenge-response: get a nonce, sign `challenge_bytes` with the note key.
    /// The canonical request(`request`) travels in the opening call alongside the signature(R1).
    async fn authorize(
        &self,
        client: &mut GatewayClient<Channel>,
        token_contract: &TokenContract,
        request: Option<CanonRequest>,
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
        })
    }
}

/// Deterministic B7 spot-check probe: a reasoning prompt yields, under greedy, a reproducible
/// model-characteristic output(a different model diverges in the prefix early).
const SPOTCHECK_PROBE: &str = "What is 17 times 23? Show your step-by-step reasoning.";

/// Greedy(temp=0) request to the official reference endpoint:
/// non-streaming `chat/completions`, return the comparable text. Some reasoning models return provider-side
/// reasoning first and visible content only after enough tokens; that reasoning is still a model identity signal
/// and matches the gateway's `CanonChunk.reasoning` side channel. The key goes only into the Authorization
/// header and is NOT logged(secret masking).
async fn reference_completion(
    endpoint: &crate::buyer::verify::ReferenceEndpoint,
    api_key: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String> {
    let url = format!("{}/chat/completions", endpoint.base_url);
    let body = serde_json::json!({
        "model": endpoint.model.clone(),
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": 0.0,
        "max_tokens": max_tokens,
        "stream": false,
        "seed": 0,
    });
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
    use super::{content_or_reasoning, reference_reasoning_text};

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
