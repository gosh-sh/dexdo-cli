//! Gateway mock model: fake tokens instead of calling a real model
//! . A standard debug mode in production code, not a
//! `#[cfg(test)]` crutch. Retained even after the real upstream appears.

use dexdo_proto::{CanonChunk, CanonRequest, SignalManifest, TokenLogprobs};
use tokio::sync::mpsc;
use tonic::Status;

/// Run the mock upstream: build up to `count` deterministic fake tokens **from the prompt**
/// of the canonical request(R1) and send them incrementally into `tx`(R6, token-by-token). Each mock
/// chunk carries one fake token id, so gateway accounting sees one delivered token. Both sides know the tokens are fake (`--mock-model` on
/// both,). When there is no request -- neutral `mock-token-*`.
pub async fn run(
    count: u64,
    req: Option<&CanonRequest>,
    tx: mpsc::Sender<Result<CanonChunk, Status>>,
    scammer: bool,
) {
    // claims a DIFFERENT(real) model than the market's frame model -> the buyer's verification(B7) rejects it.
    // `scammer` makes the substitution UNCONDITIONAL -- a seller instance that always
    // serves the wrong model regardless of the prompt(for the multi-seller failover e2e).
    let substitute = scammer
        || req
            .map(last_user_message)
            .map(|p| p.contains("DEXDO_FIXTURE_SUBSTITUTE"))
            .unwrap_or(false);
    // log-probability(>0) -> the buyer's verification(B6, logprobs shape) -> Bail.
    let bad_logprobs = req
        .map(last_user_message)
        .map(|p| p.contains("DEXDO_FIXTURE_BADLOGPROBS"))
        .unwrap_or(false);
    // token_ids OUTSIDE its vocabulary -> the buyer's verification(B5, tokenizer check) -> Bail.
    let foreign = req
        .map(last_user_message)
        .map(|p| p.contains("DEXDO_FIXTURE_FOREIGN"))
        .unwrap_or(false);
    let tokens = match req {
        Some(req) => derive_tokens(req, count),
        None => (0..count).map(|i| format!("mock-token-{i} ")).collect(),
    };
    for (seq, text) in tokens.into_iter().enumerate() {
        let chunk = CanonChunk {
            text,
            reasoning: String::new(),
            // Fake token-ids: by seq; in "foreign tokenizer" -- outside the qwen vocabulary.
            token_ids: if foreign {
                vec![999_999]
            } else {
                vec![seq as u32]
            },
            seq: seq as u64,
            // The mock yields no logprobs; in the "bad logprobs" fixture -- invalid(>0) for B6.
            logprobs: if bad_logprobs {
                vec![TokenLogprobs {
                    logprob: 1.0,
                    top: Vec::new(),
                }]
            } else {
                Vec::new()
            },
            // R3: the gateway declares the available signals on the first chunk. The mock yields(fake)
            // token_ids, no logprobs; the tokenizer family is the mock profile.
            manifest: (seq == 0).then(|| {
                if substitute {
                    // Fixture: real family + a foreign model(!= frame) -> buyer's B7 -> Bail.
                    SignalManifest {
                        tokenizer_family: "qwen".to_string(),
                        has_token_ids: true,
                        has_logprobs: false,
                        claimed_model: "substituted/cheap-model".to_string(),
                    }
                } else if foreign {
                    // Claims the qwen tokenizer, but token_ids are outside the qwen vocabulary -> B5 -> Bail.
                    // claimed_model is empty(R4, don't fabricate) -> B7 doesn't run; B5 is what catches it.
                    SignalManifest {
                        tokenizer_family: "qwen".to_string(),
                        has_token_ids: true,
                        has_logprobs: false,
                        claimed_model: String::new(),
                    }
                } else {
                    // Mock(Permissive -- B5/B7 pass). In the bad-logprobs fixture we declare
                    // has_logprobs so that B6 runs and rejects the invalid shape.
                    SignalManifest {
                        tokenizer_family: "mock".to_string(),
                        has_token_ids: true,
                        has_logprobs: bad_logprobs,
                        claimed_model: "mock".to_string(),
                    }
                }
            }),
        };
        if tx.send(Ok(chunk)).await.is_err() {
            break; // buyer disconnected(STOP)
        }
    }
}

/// The last `user`-role message in the canonical request(the prompt the output is built from).
fn last_user_message(req: &CanonRequest) -> &str {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .unwrap_or("")
}

/// Deterministic fake output from the prompt: a prefix marker + echo of the words of the last
/// user message, one delta token per word, truncated to `count`(mock model).
fn derive_tokens(req: &CanonRequest, count: u64) -> Vec<String> {
    let prompt = last_user_message(req);
    let mut out: Vec<String> = Vec::new();
    // Mock marker + echo of the prompt token-by-token(R6: incrementality is preserved).
    out.push("mock-reply: ".to_string());
    for word in prompt.split_whitespace() {
        out.push(format!("{word} "));
    }
    if out.len() == 1 {
        // Empty prompt -- still yield something deterministic.
        out.push("(empty) ".to_string());
    }
    out.truncate(count as usize);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_proto::ChatMessage;

    #[tokio::test]
    async fn mock_echoes_prompt_incrementally() {
        let req = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "ping pong".into(),
            }],
            params: None,
        };
        let (tx, mut rx) = mpsc::channel(16);
        run(8, Some(&req), tx, false).await;
        let mut chunks = Vec::new();
        while let Some(item) = rx.recv().await {
            chunks.push(item.unwrap());
        }
        // seq monotonic from 0; marker + echo of the prompt words.
        assert_eq!(chunks[0].seq, 0);
        let text: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert!(text.contains("mock-reply"));
        assert!(text.contains("ping") && text.contains("pong"));
    }
}
