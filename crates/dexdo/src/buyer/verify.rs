//! Client-side model verification. Runs **inline on the canonical
//! stream BEFORE re-rendering**(B5-B9), on the buyer's side -- the protocol does not guarantee
//! authenticity off-chain; the client provides it.
//! Layers in increasing order of cost: **B5 tokenizer-check**(here), then logprob shape
//! (B6), reference spot-check(B7), behavioral probes(B8), per-seller score(B10). Currently B5 is
//! up -- it is mandatory first and cheaply catches a crude model substitution.
//! The verifier is **stateful per stream**: the `SignalManifest` arrives on the first chunk,
//! the verifier captures it and runs the available layers on each chunk. No signals / not declared --
//! we do not fabricate(R4), we degrade(R3) to the next layers.

use dexdo_proto::{CanonChunk, SignalManifest, TokenLogprobs};

/// Stream verification verdict: continue or bail (refuse/bail to the next
/// eligible seller B3 -- loss <= 2 ticks, without completing the deal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Signals consistent on the available subset -- continue accepting.
    Pass,
    /// Substitution/inconsistency caught -- bail. Carries the reason(for the log/dispute).
    Bail(String),
}

/// Tokenizer profile of a family. At minimum -- the upper bound of the vocabulary: a foreign tokenizer with
/// a different vocabulary is exposed by a token-id out of range, instantly and cheaply. `Permissive` -- the mock's fake tokens
/// the tokenizer-check does not apply to them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenizerProfile {
    Permissive,
    /// Real vocabulary: all token-ids must be `< vocab_size`.
    Vocab(u32),
}

/// Registry of family profiles ( -- at least one profile at start; a full buyer profile
/// config is an open question). Returns `None` for an undeclared/unknown family.
fn profile_for(family: &str) -> Option<TokenizerProfile> {
    match family {
        // Mock: tokens are fake by design, nothing to check.
        "mock" => Some(TokenizerProfile::Permissive),
        // Qwen3(the D3 path -- Groq `qwen/qwen3-32b`): vocabulary ~152064.
        "qwen" => Some(TokenizerProfile::Vocab(152_064)),
        // Llama 3.x: vocabulary 128256.
        "llama" => Some(TokenizerProfile::Vocab(128_256)),
        // GPT-4o/cl100k family: ~100352(round up; o200k is outside the start profile).
        "gpt" => Some(TokenizerProfile::Vocab(100_352)),
        _ => None,
    }
}

/// Tokenizer-check: `token_ids` against the profile of the declared `family`. An unknown/empty
/// family -> nothing to check(degradation R3 -- `Pass` at this layer; the next layers/spot-check catch it).
fn tokenizer_check(family: &str, token_ids: &[u32]) -> Verdict {
    match profile_for(family) {
        None | Some(TokenizerProfile::Permissive) => Verdict::Pass,
        Some(TokenizerProfile::Vocab(max)) => match token_ids.iter().find(|&&id| id >= max) {
            Some(&bad) => Verdict::Bail(format!(
                "tokenizer-check: token-id {bad} outside the vocabulary of family '{family}' (vocab {max}) -- foreign tokenizer"
            )),
            None => Verdict::Pass,
        },
    }
}

/// Logprob shape -- at minimum: **distribution correctness**. A real LM returns
/// valid log-probabilities(<= 0), a top-k sorted in descending order, and the chosen token no
/// more probable than the maximum. Forged logprobs(zeros/unsorted/chosen > top) are an instant flag.
/// Comparing entropy/shape against a **specific model's profile**(an equivalent-but-different model) is,
/// horizon(needs a per-model profile); here -- a cheap universal consistency cross-check.
fn logprob_shape_check(logprobs: &[TokenLogprobs]) -> Verdict {
    const EPS: f64 = 1e-6;
    for (i, tl) in logprobs.iter().enumerate() {
        // A real model's log-probability is always FINITE and <= 0. NaN/+/-inf is a "broken" signal
        // (corruption/fabrication); catch it explicitly, otherwise NaN comparisons(always false) silently bypass B6.
        if !tl.logprob.is_finite() {
            return Verdict::Bail(format!(
                "logprob-shape: token {i} logprob is not finite ({}) -- invalid log-probability",
                tl.logprob
            ));
        }
        if tl.logprob > EPS {
            return Verdict::Bail(format!(
                "logprob-shape: token {i} logprob {:.4} > 0 -- invalid log-probability (fabrication)",
                tl.logprob
            ));
        }
        let mut prev = f64::INFINITY;
        for (j, t) in tl.top.iter().enumerate() {
            if !t.logprob.is_finite() {
                return Verdict::Bail(format!(
                    "logprob-shape: token {i} top[{j}] logprob is not finite -- invalid log-probability"
                ));
            }
            if t.logprob > EPS {
                return Verdict::Bail(format!("logprob-shape: token {i} top[{j}] logprob > 0"));
            }
            if t.logprob > prev + EPS {
                return Verdict::Bail(format!(
                    "logprob-shape: token {i} top is not descending -- distribution fabrication"
                ));
            }
            prev = t.logprob;
        }
        if let Some(top0) = tl.top.first() {
            if tl.logprob > top0.logprob + EPS {
                return Verdict::Bail(format!(
                    "logprob-shape: token {i} chosen logprob > top maximum -- inconsistent"
                ));
            }
        }
    }
    Verdict::Pass
}

/// Stateful verifier of a single stream. Captures the `SignalManifest` from the first chunk and
/// runs the available layers inline on each chunk BEFORE re-rendering. A `Bail` verdict is a signal to the buyer
/// to bail(B10).
#[derive(Debug, Default)]
pub struct StreamVerifier {
    manifest: Option<SignalManifest>,
    /// The market frame's model(what the buyer pays for, B2) -- the model declared by the seller
    /// is checked against it. `None` -- the check is disabled.
    expected_model: Option<String>,
}

impl StreamVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verifier that checks the declared model against the frame model(B7).
    pub fn with_expected_model(model: String) -> Self {
        Self {
            manifest: None,
            expected_model: Some(model),
        }
    }

    /// The declared tokenizer family(if the gateway declared it) -- for diagnostics/scoring.
    pub fn claimed_model(&self) -> Option<&str> {
        self.manifest.as_ref().map(|m| m.claimed_model.as_str())
    }

    /// Verify the next canonical chunk. `Pass` -- continue; `Bail(reason)` -- bail.
    pub fn verify(&mut self, chunk: &CanonChunk) -> Verdict {
        // R3: the manifest arrives on the first chunk(seq=0) -- capture it for the whole stream.
        if let Some(m) = &chunk.manifest {
            self.manifest = Some(m.clone());
        }
        let Some(manifest) = &self.manifest else {
            // The gateway did not declare signals(R3/R4) -- on an empty subset there is nothing to check;
            // we do not fabricate. We rely on the next layers(spot-check B7).
            return Verdict::Pass;
        };
        // B5: tokenizer-check -- only if the gateway declared token_ids(otherwise degradation R3).
        if manifest.has_token_ids {
            let v = tokenizer_check(&manifest.tokenizer_family, &chunk.token_ids);
            if v != Verdict::Pass {
                return v;
            }
        }
        // B6: logprob shape -- only if the gateway declared logprobs(otherwise degradation R3).
        if manifest.has_logprobs && !chunk.logprobs.is_empty() {
            let v = logprob_shape_check(&chunk.logprobs);
            if v != Verdict::Pass {
                return v;
            }
        }
        // B7: the declared model must match the frame model (what
        // the buyer pays for, B2). Substitution in the declaration is a flag("do not trust the declaration blindly").
        // Mock is skipped(fake by design). Full comparison against reference X (running the prompt
        // on a trusted endpoint, ~1-5% of requests) is(reference source) horizon, see the report sidecar.
        if let Some(expected) = &self.expected_model {
            let claimed = manifest.claimed_model.as_str();
            let is_mock = matches!(
                profile_for(&manifest.tokenizer_family),
                Some(TokenizerProfile::Permissive)
            );
            if !claimed.is_empty() && !is_mock && claimed != expected {
                return Verdict::Bail(format!(
                    "reference-check (): declared model '{claimed}' != frame model '{expected}' -- substitution"
                ));
            }
        }
        // B8(behavioral probes) - via a separate mechanism: the buyer sends a probe prompt and checks
        // the response against the exact-model fingerprint([`behavioral_check`]); this is not per-chunk.
        Verdict::Pass
    }
}

/// Behavioral fingerprint of an exact/reference model: a deterministic probe-prompt + a quirk that
/// the declared model characteristically emits(format/tokenization/refusals). The registry is small and grows
/// (lead's decision); populated from live verification(temp=0).
pub struct Fingerprint {
    pub probe_prompt: &'static str,
    /// A quirk marker in the response, characteristic of this exact/reference model.
    pub expected_contains: &'static str,
    /// Some providers expose thinking out-of-band instead of embedding this marker in `content`.
    pub accepts_reasoning_side_channel: bool,
}

fn model_id_alias(model_id: &str) -> &str {
    match model_id.trim() {
        "Qwen/Qwen3-32B" => "qwen/qwen3-32b",
        _ => match model_id.trim().to_ascii_lowercase().as_str() {
            "qwen/qwen3-32b" | "qwen--qwen3--32b" => "qwen/qwen3-32b",
            _ => "",
        },
    }
}

fn is_registry_qwen3_32b(model_id: &str) -> bool {
    model_id.trim() == "Qwen/Qwen3-32B"
}

fn is_qwen3_32b_identity(model_id: &str) -> bool {
    matches!(model_id_alias(model_id), "qwen/qwen3-32b")
}

fn qwen3_32b_fingerprints() -> &'static [Fingerprint] {
    &[Fingerprint {
        probe_prompt: "What is 17*23? Think step by step.",
        expected_contains: "<think>",
        accepts_reasoning_side_channel: true,
    }]
}

fn fingerprints_for(model_id: &str) -> &'static [Fingerprint] {
    if is_qwen3_32b_identity(model_id) {
        qwen3_32b_fingerprints()
    } else {
        &[]
    }
}

/// The exact model reference for the B7 spot-check. `None` -- no reference -> **degradation**(R3): the full
/// spot-check does not apply, reliance on the cheap B7(claimed-vs-frame) + B5/B6.
pub fn reference_endpoint_for(model_id: &str) -> Option<ReferenceEndpoint> {
    if is_registry_qwen3_32b(model_id) {
        return None;
    }
    match model_id_alias(model_id) {
        "qwen/qwen3-32b" => Some(ReferenceEndpoint {
            base_url: "https://api.groq.com/openai/v1",
            model: "qwen/qwen3-32b",
            api_key_env: "GROQ_API_KEY",
        }),
        _ => None,
    }
}

/// Default probe-prompt of an exact/reference model(B8) -- the first fingerprint in the registry. `None` -- no fingerprint
/// (degradation R3: B8 does not apply, reliance on B5/B6/B7).
pub fn default_probe(model_id: &str) -> Option<&'static str> {
    fingerprints_for(model_id).first().map(|f| f.probe_prompt)
}

/// Behavioral probe: the declared model's response to the probe-prompt must carry its quirk.
/// A mismatch -> the model is not the one declared -> `Bail`. A prompt not in the registry / no fingerprint ->
/// degradation(`Pass` at this layer).
pub fn behavioral_check(model_id: &str, probe_prompt: &str, response: &str) -> Verdict {
    behavioral_check_with_reasoning(model_id, probe_prompt, response, "")
}

/// Behavioral probe with provider-separated reasoning. OpenRouter can return qwen thinking in
/// reasoning/reasoning_details while `content` carries only the final answer; that is the same
/// exact-model signal as the old Groq `<think>` content marker for this fingerprint.
pub fn behavioral_check_with_reasoning(
    model_id: &str,
    probe_prompt: &str,
    response: &str,
    reasoning: &str,
) -> Verdict {
    for fp in fingerprints_for(model_id) {
        if fp.probe_prompt == probe_prompt {
            let has_content_quirk = response.contains(fp.expected_contains);
            let has_reasoning = fp.accepts_reasoning_side_channel
                && is_registry_qwen3_32b(model_id)
                && !reasoning.trim().is_empty();
            return if has_content_quirk || has_reasoning {
                Verdict::Pass
            } else {
                Verdict::Bail(format!(
                    "behavioral-probe (): the probe response does not carry model '{model_id}' quirk ('{}') -- model is not the declared one",
                    fp.expected_contains
                ))
            };
        }
    }
    Verdict::Pass
}

// ---- B7 full spot-check: greedy comparison against the declared model's official endpoint ----

/// The default B7 spot-check agreement threshold is **high**: greedy(temp=0) of the same model
/// yields an almost identical leading prefix, while a different model diverges early. Tuned by the caller.
pub const DEFAULT_SPOTCHECK_THRESHOLD: f64 = 0.7;

/// The declared model's **official** reference endpoint: the buyer
/// compares the seller's greedy output against it. The registry grows; `qwen` -> Groq. The key is read from env at
/// runtime(`api_key_env`) and is NOT stored here(masked in logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceEndpoint {
    pub base_url: &'static str,
    pub model: &'static str,
    pub api_key_env: &'static str,
}

/// Coarse tokenizer family by model id(`qwen`/`llama`/`gpt`/...). This is for diagnostics/tokenizer profiles only;
/// content-identity fingerprints/references are keyed by exact model id or explicit aliases.
pub fn family_of(model: &str) -> String {
    let m = model.to_ascii_lowercase();
    for fam in ["qwen", "llama", "gpt", "mixtral", "gemma"] {
        if m.contains(fam) {
            return fam.to_string();
        }
    }
    String::new()
}

/// Text normalization for comparison: lowercase, words of alphanumerics(punctuation stripped).
fn normalize_words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Agreement fraction over the **leading word prefix**(B7 spot-check): greedy(temp=0) of the same
/// model gives an identical prefix(-> 1.0); a different model diverges early(-> low). `0.0`, if
/// there is nothing to compare(empty response on either side).
pub fn prefix_agreement(seller: &str, reference: &str) -> f64 {
    let a = normalize_words(seller);
    let b = normalize_words(reference);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let n = a.len().min(b.len());
    let matched = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    matched as f64 / n as f64
}

/// B7 spot-check verdict: agreement with the reference >= threshold -> `Pass`(model confirmed);
/// otherwise -> `Bail`(greedy output diverged from the official endpoint -> not the declared model).
pub fn spotcheck_verdict(agreement: f64, threshold: f64) -> Verdict {
    if agreement >= threshold {
        Verdict::Pass
    } else {
        Verdict::Bail(format!(
            "reference-spotcheck (): agreement with the reference {agreement:.2} < threshold {threshold:.2} -- greedy output did not match the official model"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(family: &str, has_token_ids: bool) -> SignalManifest {
        SignalManifest {
            tokenizer_family: family.to_string(),
            has_token_ids,
            has_logprobs: false,
            claimed_model: "m".to_string(),
        }
    }

    fn chunk(seq: u64, token_ids: Vec<u32>, manifest: Option<SignalManifest>) -> CanonChunk {
        CanonChunk {
            text: "x".to_string(),
            reasoning: String::new(),
            token_ids,
            seq,
            logprobs: Vec::new(),
            manifest,
        }
    }

    #[test]
    fn mock_family_passes_any_token_ids() {
        // Mock tokens -- sequential numbers, Permissive profile -> Pass even on large ids.
        let mut v = StreamVerifier::new();
        assert_eq!(
            v.verify(&chunk(0, vec![0], Some(manifest("mock", true)))),
            Verdict::Pass
        );
        assert_eq!(v.verify(&chunk(1, vec![999_999], None)), Verdict::Pass);
    }

    #[test]
    fn real_family_in_vocab_passes() {
        let mut v = StreamVerifier::new();
        assert_eq!(
            v.verify(&chunk(
                0,
                vec![10, 50_000, 151_000],
                Some(manifest("qwen", true))
            )),
            Verdict::Pass
        );
    }

    #[test]
    fn foreign_vocab_bails() {
        // token-id outside the declared family's vocabulary -> foreign tokenizer -> Bail.
        let mut v = StreamVerifier::new();
        let verdict = v.verify(&chunk(0, vec![10, 500_000], Some(manifest("qwen", true))));
        assert!(matches!(verdict, Verdict::Bail(_)), "out-of-vocab -> Bail");
    }

    #[test]
    fn manifest_captured_on_first_chunk_applies_to_later() {
        // The manifest is only on seq=0; subsequent chunks are checked against it.
        let mut v = StreamVerifier::new();
        assert_eq!(
            v.verify(&chunk(0, vec![1], Some(manifest("qwen", true)))),
            Verdict::Pass
        );
        let verdict = v.verify(&chunk(1, vec![999_999], None));
        assert!(
            matches!(verdict, Verdict::Bail(_)),
            "later chunk still checked"
        );
    }

    #[test]
    fn degrades_when_no_token_ids_or_no_manifest() {
        // has_token_ids=false(e.g. Groq SSE) -> the tokenizer-check does not run(R3), Pass.
        let mut v = StreamVerifier::new();
        assert_eq!(
            v.verify(&chunk(0, vec![], Some(manifest("", false)))),
            Verdict::Pass
        );
        // No manifest at all -> nothing to check, Pass(we do not fabricate).
        let mut v2 = StreamVerifier::new();
        assert_eq!(v2.verify(&chunk(0, vec![5], None)), Verdict::Pass);
    }

    // ---- B6: logprob shape/correctness ----

    fn lp(logprob: f64, top: Vec<(f64, u32)>) -> TokenLogprobs {
        TokenLogprobs {
            logprob,
            top: top
                .into_iter()
                .map(|(l, id)| dexdo_proto::TopLogprob {
                    token: id.to_string(),
                    logprob: l,
                })
                .collect(),
        }
    }

    fn chunk_lp(logprobs: Vec<TokenLogprobs>, manifest: Option<SignalManifest>) -> CanonChunk {
        CanonChunk {
            text: "x".to_string(),
            reasoning: String::new(),
            token_ids: Vec::new(),
            seq: 0,
            logprobs,
            manifest,
        }
    }

    fn manifest_lp(has_logprobs: bool) -> SignalManifest {
        SignalManifest {
            tokenizer_family: String::new(),
            has_token_ids: false,
            has_logprobs,
            claimed_model: "m".to_string(),
        }
    }

    #[test]
    fn wellformed_logprobs_pass() {
        let mut v = StreamVerifier::new();
        let c = chunk_lp(
            vec![lp(-0.5, vec![(-0.5, 10), (-1.2, 20), (-3.0, 30)])],
            Some(manifest_lp(true)),
        );
        assert_eq!(v.verify(&c), Verdict::Pass);
    }

    #[test]
    fn positive_logprob_bails() {
        let mut v = StreamVerifier::new();
        let c = chunk_lp(vec![lp(0.5, vec![])], Some(manifest_lp(true)));
        assert!(
            matches!(v.verify(&c), Verdict::Bail(_)),
            "logprob > 0 -> Bail"
        );
    }

    #[test]
    fn unsorted_top_bails() {
        // top increases(-1.2 then -0.5) -- not descending -> fabrication.
        let mut v = StreamVerifier::new();
        let c = chunk_lp(
            vec![lp(-1.2, vec![(-1.2, 10), (-0.5, 20)])],
            Some(manifest_lp(true)),
        );
        assert!(
            matches!(v.verify(&c), Verdict::Bail(_)),
            "unsorted top -> Bail"
        );
    }

    #[test]
    fn chosen_above_top_bails() {
        // chosen logprob(-0.1) more probable than top maximum(-0.5) -> inconsistent.
        let mut v = StreamVerifier::new();
        let c = chunk_lp(vec![lp(-0.1, vec![(-0.5, 10)])], Some(manifest_lp(true)));
        assert!(
            matches!(v.verify(&c), Verdict::Bail(_)),
            "chosen > top max -> Bail"
        );
    }

    /// (negative, test review items 3/5): a "broken" CanonChunk with a NON-finite logprob(NaN/+/-inf) --
    /// an invalid log-probability(a real model does not send such). Without an explicit check, NaN comparisons
    /// (always false) silently bypass B6 -- we close this: -> Bail, and the verifier does not panic on garbage.
    #[test]
    fn garbage_canon_chunk_bails_on_nonfinite_logprob() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut v = StreamVerifier::new();
            let c = chunk_lp(vec![lp(bad, vec![(-0.5, 10)])], Some(manifest_lp(true)));
            assert!(
                matches!(v.verify(&c), Verdict::Bail(_)),
                "non-finite logprob {bad} -> Bail"
            );
        }
        // A non-finite value inside top is also rejected.
        let mut v = StreamVerifier::new();
        let c = chunk_lp(
            vec![lp(-0.5, vec![(f64::NAN, 10)])],
            Some(manifest_lp(true)),
        );
        assert!(
            matches!(v.verify(&c), Verdict::Bail(_)),
            "non-finite top logprob -> Bail"
        );
    }

    #[test]
    fn logprob_check_skipped_when_has_logprobs_false() {
        // has_logprobs=false -> B6 does not run even with broken logprobs(degradation R3).
        let mut v = StreamVerifier::new();
        let c = chunk_lp(vec![lp(0.5, vec![])], Some(manifest_lp(false)));
        assert_eq!(v.verify(&c), Verdict::Pass);
    }

    // ---- B7: comparing the declared model against the frame model ----

    fn manifest_full(family: &str, claimed: &str) -> SignalManifest {
        SignalManifest {
            tokenizer_family: family.to_string(),
            has_token_ids: false,
            has_logprobs: false,
            claimed_model: claimed.to_string(),
        }
    }

    #[test]
    fn claimed_model_matches_frame_passes() {
        let mut v = StreamVerifier::with_expected_model("qwen/qwen3-32b".to_string());
        let c = chunk_lp(vec![], Some(manifest_full("qwen", "qwen/qwen3-32b")));
        assert_eq!(v.verify(&c), Verdict::Pass);
    }

    #[test]
    fn claimed_model_mismatch_bails() {
        // The seller declares a different(cheap) model than the frame paid for -> substitution -> Bail.
        let mut v = StreamVerifier::with_expected_model("qwen/qwen3-32b".to_string());
        let c = chunk_lp(vec![], Some(manifest_full("llama", "cheap/llama-1b")));
        assert!(
            matches!(v.verify(&c), Verdict::Bail(_)),
            "declared != frame -> Bail"
        );
    }

    #[test]
    fn mock_skips_model_check() {
        // Mock -- the model comparison does not apply(fake by design), even on a mismatch.
        let mut v = StreamVerifier::with_expected_model("dexdo-mock".to_string());
        let c = chunk_lp(vec![], Some(manifest_full("mock", "mock")));
        assert_eq!(v.verify(&c), Verdict::Pass);
    }

    #[test]
    fn no_expected_model_skips_check() {
        // Without a frame model (new()) -- B7 does not run.
        let mut v = StreamVerifier::new();
        let c = chunk_lp(vec![], Some(manifest_full("qwen", "anything")));
        assert_eq!(v.verify(&c), Verdict::Pass);
    }

    // ---- B8: behavioral probes ----

    #[test]
    fn behavioral_probe_matches_fingerprint_passes() {
        // qwen3-32b's response to the probe carries the <think> quirk -> Pass.
        let probe = default_probe("qwen--qwen3--32b").unwrap();
        assert_eq!(
            behavioral_check("qwen--qwen3--32b", probe, "<think>\nOkay...</think> 391"),
            Verdict::Pass
        );
    }

    #[test]
    fn behavioral_probe_missing_quirk_bails() {
        // qwen3-32b declared, but the response lacks <think>(substitute model, not reasoning) -> Bail.
        let probe = default_probe("qwen--qwen3--32b").unwrap();
        assert!(matches!(
            behavioral_check("qwen--qwen3--32b", probe, "391"),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn behavioral_probe_openrouter_reasoning_passes_without_content_think() {
        let probe = default_probe("Qwen/Qwen3-32B").unwrap();
        assert_eq!(
            behavioral_check_with_reasoning(
                "Qwen/Qwen3-32B",
                probe,
                "391",
                "We need compute 17 * 23 step by step."
            ),
            Verdict::Pass
        );
    }

    #[test]
    fn behavioral_probe_plain_answer_without_reasoning_still_bails() {
        let probe = default_probe("Qwen/Qwen3-32B").unwrap();
        assert!(matches!(
            behavioral_check_with_reasoning("Qwen/Qwen3-32B", probe, "391", ""),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn behavioral_probe_reasoning_side_channel_is_registry_identity_only() {
        let probe = default_probe("qwen/qwen3-32b").unwrap();
        assert!(matches!(
            behavioral_check_with_reasoning(
                "qwen/qwen3-32b",
                probe,
                "391",
                "provider-separated reasoning"
            ),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn behavioral_probe_exact_qwen_aliases_preserve_fingerprint() {
        let canonical_probe = default_probe("qwen--qwen3--32b").unwrap();
        let served_probe = default_probe("qwen/qwen3-32b").unwrap();
        let registry_probe = default_probe("Qwen/Qwen3-32B").unwrap();
        assert_eq!(canonical_probe, served_probe);
        assert_eq!(canonical_probe, registry_probe);
        assert!(matches!(
            behavioral_check("qwen/qwen3-32b", served_probe, "391"),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn unknown_qwen_variant_does_not_inherit_qwen3_fingerprint() {
        let qwen3_probe = default_probe("qwen--qwen3--32b").unwrap();
        assert!(default_probe("qwen--qwen3.6--27b").is_none());
        assert_eq!(
            behavioral_check("qwen--qwen3.6--27b", qwen3_probe, "391"),
            Verdict::Pass
        );
    }

    #[test]
    fn behavioral_probe_unknown_family_degrades() {
        // No exact-model fingerprint -> degradation(Pass), we do not fabricate.
        assert!(default_probe("unknown").is_none());
        assert_eq!(behavioral_check("unknown", "x", "y"), Verdict::Pass);
    }

    #[test]
    fn behavioral_probe_non_registered_prompt_skips() {
        // A prompt not in the probe registry -> the layer does not apply(Pass).
        assert_eq!(
            behavioral_check("qwen", "random prompt", "no think here"),
            Verdict::Pass
        );
    }

    // ---- B7 full spot-check ----

    #[test]
    fn reference_endpoint_registry() {
        let r = reference_endpoint_for("qwen--qwen3--32b").expect("qwen3-32b has a reference");
        assert_eq!(r.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(r.model, "qwen/qwen3-32b");
        assert_eq!(r.api_key_env, "GROQ_API_KEY");
        assert!(reference_endpoint_for("qwen/qwen3-32b").is_some());
        // The live ModelRegistry name is provider-neutral; do not compare OpenRouter output to Groq here.
        assert!(reference_endpoint_for("Qwen/Qwen3-32B").is_none());
        // No reference -> degradation(R3).
        assert!(reference_endpoint_for("qwen--qwen3.6--27b").is_none());
        assert!(reference_endpoint_for("llama").is_none());
        assert!(reference_endpoint_for("mock").is_none());
    }

    #[test]
    fn prefix_agreement_identical_and_subset() {
        // greedy of one model: identical output -> 1.0.
        assert_eq!(
            prefix_agreement("The answer is 42.", "the answer is 42"),
            1.0
        );
        // The seller's response is a prefix of the reference(shorter) -> all of it matched -> 1.0.
        assert_eq!(prefix_agreement("the answer", "the answer is 42"), 1.0);
        // Punctuation/case do not matter(normalization).
        assert_eq!(prefix_agreement("Hello,   World!", "hello world"), 1.0);
    }

    #[test]
    fn prefix_agreement_divergent_and_empty() {
        // A different model: divergence from the first word -> 0.0.
        assert_eq!(prefix_agreement("foo bar baz", "qux bar baz"), 0.0);
        // Partial agreement: 2 of 3 leading words.
        let a = prefix_agreement("the cat sat", "the cat ran");
        assert!((a - 2.0 / 3.0).abs() < 1e-9, "got {a}");
        // Empty response -> nothing to confirm -> 0.0.
        assert_eq!(prefix_agreement("", "something"), 0.0);
        assert_eq!(prefix_agreement("something", ""), 0.0);
    }

    #[test]
    fn spotcheck_verdict_threshold() {
        // High agreement(>= threshold) -> Pass; below -> Bail. The boundary is inclusive.
        assert_eq!(
            spotcheck_verdict(1.0, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
        assert_eq!(
            spotcheck_verdict(DEFAULT_SPOTCHECK_THRESHOLD, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
        assert!(matches!(
            spotcheck_verdict(0.3, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Bail(_)
        ));
    }

    #[test]
    fn spotcheck_catches_substitution_end_to_end() {
        // End to end across the functions: reference greedy output vs a substituting seller's diverging output -> Bail.
        let reference = "To compute 17 times 23 we multiply step by step";
        let scammer = "Sure here is a totally different cheaper answer";
        let agreement = prefix_agreement(scammer, reference);
        assert!(
            matches!(
                spotcheck_verdict(agreement, DEFAULT_SPOTCHECK_THRESHOLD),
                Verdict::Bail(_)
            ),
            "model substitution is caught by the spot-check (agreement={agreement})"
        );
        // Honest seller, the same greedy output -> Pass.
        let honest = "To compute 17 times 23 we multiply";
        let agreement_ok = prefix_agreement(honest, reference);
        assert_eq!(
            spotcheck_verdict(agreement_ok, DEFAULT_SPOTCHECK_THRESHOLD),
            Verdict::Pass
        );
    }
}
