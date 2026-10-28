//! Buyer client: read the enc endpoint from a file, decrypt it with the note,
//! connect, sign the challenge (B18), receive the incremental stream, account ticks, STOP.

pub mod api;
pub mod continuity;
pub mod routing;
pub mod tls;
pub mod usage;
pub mod verify;

use crate::api_format::{inspect_body, parse_frame, ApiFormat, Frame, SseFrames};
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
    BillingUsage, ChallengeRequest, GatewayClient, Header, StreamFrame, StreamRequest,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_stream::StreamExt;
use tonic::transport::Channel;

/// Outcome of the buyer receiving a whole response.
#[derive(Debug)]
pub struct StreamOutcome {
    /// Received answer text, as delivered.
    pub tokens: Vec<String>,
    /// Provider-side reasoning/thinking text, when separated from visible content.
    pub reasoning: Vec<String>,
    /// How many output-carrying frames were received.
    pub received: u64,
    /// Validated provider-native terminal usage accepted by both peers.
    pub usage: BillingUsage,
}

/// The seller cannot serve a request on this path. Request-scoped: the deal is not touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellerApiMismatch {
    /// The seller declares no forwarded paths: it predates request forwarding.
    Outdated,
    /// The seller's upstream serves other paths only.
    PathUnsupported { path: String, served: Vec<String> },
}

impl std::fmt::Display for SellerApiMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outdated => formatter.write_str(
                "the seller runs an older dexdo that cannot forward this request; it must update",
            ),
            Self::PathUnsupported { path, served } => write!(
                formatter,
                "this deal's seller does not serve {path}; it serves {}",
                served.join(", ")
            ),
        }
    }
}

impl std::error::Error for SellerApiMismatch {}

/// Buyer client with a note (private key -- for decryption and signing).
pub struct Buyer {
    /// Buyer's note -- **polymorphic**: `LocalNote` (mock) OR `RealNote` (a real chain).
    /// Its ed25519 is registered on-chain (`placeInferenceBuy`); the seller reconstructs the
    /// x25519 handover from it. `decrypt`/`sign` go through the `Note` trait, the note type is
    /// transparent to the scenario.
    pub note: Arc<dyn Note>,
    /// API paths each deal's seller declared; a seller that declares none is asked again.
    api_paths: Mutex<HashMap<TokenContract, Vec<String>>>,
}

impl Buyer {
    /// Generate a buyer with a fresh **ephemeral** note -- only for the mock fixture of a single
    /// e2e run; the production path is `from_note` with a persistent identity.
    pub fn generate() -> Self {
        Self::from_note(Arc::new(LocalNote::generate()))
    }

    /// Buyer with a **loaded persistent** note: identity from
    /// `--note-key`/wallet, reused across runs -- its orders/deals are visible again.
    /// The note is polymorphic (`Arc<dyn Note>`): the mock path yields `LocalNote`, the real
    /// path `RealNote`.
    pub fn from_note(note: Arc<dyn Note>) -> Self {
        Self {
            note,
            api_paths: Mutex::new(HashMap::new()),
        }
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
    /// challenge-response (B18) and receive one promptless mock answer naming `model`. On a
    /// fingerprint mismatch the connection does not come up -- the stream is not received
    /// (fail-closed). `max_tokens` is both the answer's limit and the billing grant.
    pub async fn connect_and_stream(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        model: &str,
        max_tokens: u64,
    ) -> Result<StreamOutcome> {
        let body = serde_json::json!({
            "model": model,
            "messages": [],
            "max_tokens": max_tokens,
            "stream": true,
        });
        self.exchange(
            handover,
            token_contract,
            ApiFormat::ChatCompletions,
            serde_json::to_vec(&body).expect("a JSON object serializes"),
            max_tokens,
            None,
            None,
        )
        .await
    }

    /// Send one request the buyer itself wrote and read its whole answer: the stream must carry a
    /// successful response head, output, and exactly one validated terminal usage within
    /// `billing_grant_tokens`. `account_output` is told about each output-carrying frame, and
    /// `account_tokens` receives the accepted usage.
    #[allow(clippy::too_many_arguments)]
    pub async fn exchange(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        format: ApiFormat,
        body: Vec<u8>,
        billing_grant_tokens: u64,
        mut account_tokens: Option<&mut (dyn FnMut(BillingUsage) -> Result<(), String> + Send)>,
        mut account_output: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
    ) -> Result<StreamOutcome> {
        api::observe_verification_usage(None);
        let streamed = crate::api_format::RequestBody::parse(&body)
            .map_err(anyhow::Error::msg)?
            .stream();
        let mut stream = self
            .open_request_stream(
                handover,
                token_contract,
                format.path(),
                body,
                crate::api_format::own_request_headers(format),
                billing_grant_tokens,
            )
            .await?;
        let mut head_status = None;
        let mut frames = SseFrames::default();
        let mut whole = Vec::new();
        let mut tokens = Vec::new();
        let mut reasoning = Vec::new();
        let mut received = 0u64;
        let mut terminal_usage = None;
        // A whole answer is inspected when its first frame arrives (an honest seller sends it as
        // one) and, failing that, once before its usage is accepted.
        let mut whole_frames = 0usize;
        let mut whole_inspected = false;
        while let Some(item) = stream.next().await {
            let frame: StreamFrame = item?;
            if terminal_usage.is_some() {
                anyhow::bail!("response content followed terminal billing usage");
            }
            if let Some(head) = frame.head {
                if head_status.replace(head.status).is_some() {
                    anyhow::bail!("the seller stated a second response head");
                }
                continue;
            }
            if let Some(usage) = frame.usage {
                let success = head_status.is_some_and(|status| (200..300).contains(&status));
                if !streamed && success && !whole_inspected {
                    whole_inspected = true;
                    if let Some(stated) = whole_output(format, &whole) {
                        if let Some(record) = account_output.as_mut() {
                            (*record)(1).map_err(anyhow::Error::msg)?;
                        }
                        push_output(&mut tokens, &mut reasoning, stated);
                        received += 1;
                    }
                }
                usage.validate().map_err(anyhow::Error::msg)?;
                if usage.billable_tokens() > billing_grant_tokens {
                    anyhow::bail!(
                        "terminal billable usage {} exceeds billing grant {}",
                        usage.billable_tokens(),
                        billing_grant_tokens
                    );
                }
                if received == 0 || usage.output_tokens == 0 {
                    anyhow::bail!("terminal billing usage has no preceding delivered output");
                }
                api::observe_verification_usage(Some(usage));
                terminal_usage = Some(usage);
                continue;
            }
            let Some(status) = head_status else {
                anyhow::bail!("response content arrived before its response head");
            };
            if !(200..300).contains(&status) || !streamed {
                whole.extend_from_slice(&frame.data);
                if whole.len() > dexdo_core::params::UPSTREAM_RESPONSE_BODY_MAX_BYTES {
                    anyhow::bail!("the seller's answer exceeds the buyer's response size limit");
                }
                whole_frames += 1;
                if (200..300).contains(&status) && whole_frames == 1 {
                    if let Some(stated) = whole_output(format, &whole) {
                        whole_inspected = true;
                        if let Some(record) = account_output.as_mut() {
                            (*record)(1).map_err(anyhow::Error::msg)?;
                        }
                        push_output(&mut tokens, &mut reasoning, stated);
                        received += 1;
                    }
                }
                continue;
            }
            let mut facts = Vec::new();
            for complete in frames.push(&frame.data).map_err(anyhow::Error::msg)? {
                if let Frame::Event { facts: stated, .. } =
                    parse_frame(format, &complete.text).map_err(anyhow::Error::msg)?
                {
                    facts.push(stated);
                }
            }
            for stated in facts.into_iter().filter(|stated| stated.has_output()) {
                if let Some(record) = account_output.as_mut() {
                    (*record)(1).map_err(anyhow::Error::msg)?;
                }
                push_output(&mut tokens, &mut reasoning, stated);
                received += 1;
            }
        }
        if let Some(status) = head_status.filter(|status| !(200..300).contains(status)) {
            anyhow::bail!(
                "the provider answered HTTP {status}: {}",
                String::from_utf8_lossy(&whole)
            );
        }
        let usage = terminal_usage
            .ok_or_else(|| anyhow!("response stream ended without terminal billing usage"))?;
        if let Some(charge) = account_tokens.as_mut() {
            (*charge)(usage).map_err(anyhow::Error::msg)?;
        }
        Ok(StreamOutcome {
            tokens,
            reasoning,
            received,
            usage,
        })
    }

    /// Open an authorized stream to the gateway carrying the consumer's path, body and
    /// headers, and return the seller's frames: the provider's head, its bytes and the terminal
    /// usage. Monetary accounting accepts the checked terminal usage only after clean EOF.
    pub async fn open_request_stream(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        path: &str,
        body: Vec<u8>,
        headers: Vec<Header>,
        billing_grant_tokens: u64,
    ) -> Result<tonic::Streaming<StreamFrame>> {
        let mut client = self.connect(handover).await?;
        let signed = self
            .authorize(
                &mut client,
                token_contract,
                path,
                body,
                headers,
                billing_grant_tokens,
            )
            .await?;
        Ok(client.open_stream(signed).await?.into_inner())
    }

    /// The format a buyer-written probe uses: Chat Completions when the seller serves it, else the
    /// first path the seller declares.
    async fn probe_format(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
    ) -> Result<ApiFormat> {
        let paths = self.seller_api_paths(handover, token_contract).await?;
        let formats = paths
            .iter()
            .filter_map(|path| ApiFormat::from_path(path))
            .collect::<Vec<_>>();
        formats
            .iter()
            .copied()
            .find(|format| *format == ApiFormat::ChatCompletions)
            .or_else(|| formats.first().copied())
            .ok_or_else(|| SellerApiMismatch::Outdated.into())
    }

    /// **Behavioral probe:** the buyer sends a deterministic probe prompt for the
    /// claimed exact/reference model (an ordinary request in the seller's format, naming
    /// `wire_model`), collects the response and checks it against that model's fingerprint
    /// ([`verify::behavioral_check`]). Mismatch -> the model is not the one claimed -> `Bail`.
    /// No exact-model fingerprint -> degradation (`Pass`). The probe is a separate request (spends the
    /// tick budget), sent as an ordinary completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn behavioral_probe(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        model_id: &str,
        wire_model: &str,
        output_limit_tokens: u64,
        billing_grant_tokens: u64,
        models: &crate::seller::ModelsConfig,
        account_tokens: Option<&mut (dyn FnMut(BillingUsage) -> Result<(), String> + Send)>,
        account_output: Option<&mut (dyn FnMut(u64) -> Result<(), String> + Send)>,
    ) -> Result<crate::buyer::verify::Verdict> {
        use crate::buyer::verify;
        let token_contract_display = dexdo_core::address::display_self_dapp(token_contract);
        let Some(probe_prompt) = verify::default_probe(model_id, models) else {
            // Nothing was asked of the seller, so nothing was consumed.
            return Ok(verify::Verdict::Pass);
        };
        // the caller's grant is what bounds this probe. Zero means the reservation cannot pay
        // for it, and issuing anyway would be worse than refusing.
        if billing_grant_tokens == 0 {
            anyhow::bail!(
                "deal {token_contract_display}: the admitted grant cannot pay for the identity verification \
                 this deal still owes; refusing rather than probing on credit"
            );
        }
        let format = self.probe_format(handover, token_contract).await?;
        let body = probe_body(format, wire_model, &probe_prompt, output_limit_tokens, None);
        let outcome = self
            .exchange(
                handover,
                token_contract,
                format,
                body,
                billing_grant_tokens,
                account_tokens,
                account_output,
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
    /// means that the model is not the one claimed -> `Bail`. No exact-model reference / no key in env
    /// reference unavailable -> **degradation** (`Pass`, R3 -- we don't penalize the seller for the
    /// absence/failure of our reference). The probe is a separate request (spends the tick budget),
    /// sent over the ordinary completion path -- no audit-only API, no second channel.

    /// It is NOT, however, indistinguishable to the seller: the probe carries the profile's
    /// greedy controls (`temperature: 0` plus its reproducibility field) and the probe prompt itself is
    /// a fixed public constant ([`DEFAULT_SPOTCHECK_PROBE`]). A seller that answers this one request
    /// honestly and serves anything cheaper afterwards is not caught by this check. Read the gate as
    /// what it is -- a sampled detector of a substituting-by-default seller, not a defence against one
    /// that watches for it; making the audit unidentifiable is tracked in and is a design decision.
    #[allow(clippy::too_many_arguments)]
    pub async fn reference_spotcheck(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        model_id: &str,
        wire_model: &str,
        output_limit_tokens: u64,
        billing_grant_tokens: u64,
        models: &crate::seller::ModelsConfig,
        account_tokens: Option<&mut (dyn FnMut(BillingUsage) -> Result<(), String> + Send)>,
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
        // for it, and issuing anyway would be worse than refusing.
        if billing_grant_tokens == 0 {
            anyhow::bail!(
                "deal {token_contract_display}: the admitted grant cannot pay for the identity verification \
                 this deal still owes; refusing rather than probing on credit"
            );
        }
        // Both legs use the same profile-declared reproducibility control. Profiles without a
        // comparable control degrade before this request is built.
        let probe = DEFAULT_SPOTCHECK_PROBE;
        let format = self.probe_format(handover, token_contract).await?;
        let body = probe_body(
            format,
            wire_model,
            probe,
            output_limit_tokens,
            Some(endpoint.sample_algorithm),
        );
        let outcome = self
            .exchange(
                handover,
                token_contract,
                format,
                body,
                billing_grant_tokens,
                account_tokens,
                account_output,
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
        Ok(GatewayClient::new(channel)
            .max_decoding_message_size(dexdo_core::params::GATEWAY_RESPONSE_MAX_BYTES))
    }

    /// Challenge-response: get a nonce, sign `challenge_bytes` with the note key.
    /// The consumer request travels in the opening call alongside the signature.
    async fn authorize(
        &self,
        client: &mut GatewayClient<Channel>,
        token_contract: &TokenContract,
        path: &str,
        body: Vec<u8>,
        headers: Vec<Header>,
        billing_grant_tokens: u64,
    ) -> Result<StreamRequest> {
        let challenge = client
            .get_challenge(ChallengeRequest {
                token_contract: token_contract.clone(),
                capabilities_only: false,
            })
            .await?
            .into_inner();
        check_api_path(path, &challenge.api_paths)?;
        let msg = challenge_bytes(token_contract, &challenge.nonce);
        let sig = self.note.sign(&msg);
        Ok(StreamRequest {
            shared_budget: true,
            token_contract: token_contract.clone(),
            nonce: challenge.nonce,
            signature: sig.0.to_vec(),
            billing_grant_tokens,
            path: path.to_string(),
            body,
            headers,
        })
    }

    /// The API paths this deal's seller serves: a money-free capability query, made before the
    /// consumer's billable identity probes and remembered for the deal once the seller declares any.
    pub(crate) async fn seller_api_paths(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
    ) -> Result<Vec<String>> {
        if let Some(paths) = self
            .api_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(token_contract)
        {
            return Ok(paths.clone());
        }
        let paths = self
            .connect(handover)
            .await?
            .get_challenge(ChallengeRequest {
                token_contract: token_contract.clone(),
                capabilities_only: true,
            })
            .await?
            .into_inner()
            .api_paths;
        if !paths.is_empty() {
            self.api_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(token_contract.clone(), paths.clone());
        }
        Ok(paths)
    }

    /// Refuse, before any money moves, a request the deal's seller cannot serve on `path`.
    pub(crate) async fn check_seller_serves(
        &self,
        handover: &Handover,
        token_contract: &TokenContract,
        path: &str,
    ) -> Result<()> {
        let paths = self.seller_api_paths(handover, token_contract).await?;
        check_api_path(path, &paths).map_err(Into::into)
    }
}

fn check_api_path(path: &str, served: &[String]) -> Result<(), SellerApiMismatch> {
    if served.is_empty() {
        return Err(SellerApiMismatch::Outdated);
    }
    if !served.iter().any(|served| served == path) {
        return Err(SellerApiMismatch::PathUnsupported {
            path: path.to_string(),
            served: served.to_vec(),
        });
    }
    Ok(())
}

/// A buyer-written probe in the seller's `format`, naming the deal's `model` and bounded by
/// `max_tokens`. `greedy` adds `temperature: 0` and the profile's one reproducibility field (B7).
/// The output a complete whole answer states, or `None` while it is not a JSON value or states no
/// output.
fn whole_output(format: ApiFormat, body: &[u8]) -> Option<crate::api_format::ResponseFacts> {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    Some(inspect_body(format, &value)).filter(|stated| stated.has_output())
}

fn push_output(
    tokens: &mut Vec<String>,
    reasoning: &mut Vec<String>,
    stated: crate::api_format::ResponseFacts,
) {
    if !stated.text.is_empty() {
        tokens.push(stated.text);
    }
    if !stated.reasoning.is_empty() {
        reasoning.push(stated.reasoning);
    }
}

fn probe_body(
    format: ApiFormat,
    model: &str,
    prompt: &str,
    max_tokens: u64,
    greedy: Option<SampleAlgorithm>,
) -> Vec<u8> {
    let mut body = match format {
        ApiFormat::Responses => serde_json::json!({
            "model": model,
            "input": prompt,
            "max_output_tokens": max_tokens,
            "stream": false,
        }),
        ApiFormat::ChatCompletions | ApiFormat::Messages => serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": prompt }],
            "max_tokens": max_tokens,
            "stream": false,
        }),
    };
    if let Some(algorithm) = greedy {
        body["temperature"] = 0.0.into();
        sample_field(&mut body, algorithm);
    }
    serde_json::to_vec(&body).expect("a JSON object serializes")
}

fn sample_field(body: &mut serde_json::Value, algorithm: SampleAlgorithm) {
    match algorithm {
        SampleAlgorithm::Seed => body["seed"] = serde_json::json!(REFERENCE_SAMPLE_SEED),
        SampleAlgorithm::RandomSeed => {
            body["random_seed"] = serde_json::json!(REFERENCE_SAMPLE_SEED)
        }
        SampleAlgorithm::TopK => body["top_k"] = serde_json::json!(REFERENCE_TOP_K),
        SampleAlgorithm::None => {}
    }
}

/// Greedy (temp=0) request to the official reference endpoint:
/// non-streaming `chat/completions`, return the comparable text. Some reasoning models return provider-side
/// reasoning first and visible content only after enough tokens; that reasoning is still a model identity signal
/// and matches the reasoning the seller's answer carries. The key goes only into the Authorization
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
    sample_field(&mut body, endpoint.sample_algorithm);
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

    /// the greedy B7 probe the buyer sends the seller is `temperature: 0` with exactly the
    /// one reproducibility field its algorithm names, in every format; B8 probes carry none.
    #[test]
    fn issue_1951_the_greedy_seller_probe_carries_exactly_one_reproducibility_field() {
        use crate::api_format::ApiFormat;
        const FIELDS: [&str; 3] = ["seed", "random_seed", "top_k"];
        for format in [
            ApiFormat::ChatCompletions,
            ApiFormat::Responses,
            ApiFormat::Messages,
        ] {
            for (algorithm, field) in [
                (SampleAlgorithm::Seed, Some("seed")),
                (SampleAlgorithm::RandomSeed, Some("random_seed")),
                (SampleAlgorithm::TopK, Some("top_k")),
                (SampleAlgorithm::None, None),
            ] {
                let body: serde_json::Value = serde_json::from_slice(&super::probe_body(
                    format,
                    "model",
                    "probe",
                    8,
                    Some(algorithm),
                ))
                .unwrap();
                assert_eq!(body["temperature"], 0.0, "{format:?} {algorithm:?}");
                let present: Vec<&str> = FIELDS
                    .into_iter()
                    .filter(|name| body.get(*name).is_some())
                    .collect();
                assert_eq!(present, field.into_iter().collect::<Vec<_>>(), "{body}");
            }
            let behavioral: serde_json::Value =
                serde_json::from_slice(&super::probe_body(format, "model", "probe", 8, None))
                    .unwrap();
            assert!(behavioral.get("temperature").is_none(), "{behavioral}");
            assert!(
                FIELDS.iter().all(|name| behavioral.get(*name).is_none()),
                "{behavioral}"
            );
        }
    }

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
              "capabilities":{"sample_algorithm":"NONE"}}}}"#,
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
                "vendor--model--v1",
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
