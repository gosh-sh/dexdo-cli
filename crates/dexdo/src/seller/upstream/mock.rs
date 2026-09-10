//! Gateway mock model: fake tokens instead of calling a real model
//! . A standard debug mode in production code, not a
//! `#[cfg(test)]` crutch. Retained even after the real upstream appears.

use super::UpstreamEvent;
use dexdo_proto::{BillingUsage, CanonChunk, CanonRequest, SignalManifest};
use tokio::sync::mpsc;
use tonic::Status;

/// Run the mock upstream: build up to `count` deterministic fake tokens **from the prompt**
/// of the canonical request (R1) and send them incrementally into `tx` (R6, token-by-token), followed
/// by the same terminal input/output/total record used by real adapters. Both sides know the tokens
/// are fake. When there is no request -- neutral `mock-token-*`.
/// Token ids per chunk in the `DEXDO_FIXTURE_FATCHUNK` fixture.
pub const FAT_CHUNK_TOKENS: u32 = 4;

pub async fn run(
    count: u64,
    req: Option<&CanonRequest>,
    tx: mpsc::Sender<Result<UpstreamEvent, Status>>,
    scammer: bool,
    claimed_model: Option<&str>,
) {

    // claims a DIFFERENT (real) model than the market's frame model -> the buyer's verification (B7) rejects it.
    // `scammer` makes the substitution UNCONDITIONAL -- a seller instance that always
    // serves the wrong model regardless of the prompt (for the multi-seller failover e2e).
    let substitute = scammer
        || req
            .map(last_user_message)
            .map(|p| p.contains("DEXDO_FIXTURE_SUBSTITUTE"))
            .unwrap_or(false);

    // token_ids OUTSIDE its vocabulary -> the buyer's verification (B5, tokenizer check) -> Bail.
    let foreign = req
        .map(last_user_message)
        .map(|p| p.contains("DEXDO_FIXTURE_FOREIGN"))
        .unwrap_or(false);

    // one. A seller chooses how many tokens a chunk holds, so the buyer's per-request grant has to
    // hold against a chunk that overshoots what is left of it rather than against a convenient
    // one-token-per-chunk stream.
    let fat_chunk = req
        .map(last_user_message)
        .map(|p| p.contains("DEXDO_FIXTURE_FATCHUNK"))
        .unwrap_or(false);

    // overshoots it by chunking around it: one token, then two. A buyer whose grant is two must
    // refuse the second chunk BEFORE rendering it, not notice afterwards that it delivered three.
    let straddle = req
        .map(last_user_message)
        .map(|p| p.contains("DEXDO_FIXTURE_STRADDLE"))
        .unwrap_or(false);

    // actually given, so a test can read off the wire what the buyer SENT rather than infer it from
    // how much came back. `count` is the seller's resolved limit, which for an unconstrained mock is
    // exactly the request's `params.max_tokens`.
    let echo_limit = req
        .map(last_user_message)
        .map(|p| p.contains("DEXDO_FIXTURE_ECHOLIMIT"))
        .unwrap_or(false);
    let mut tokens = match req {
        Some(req) => derive_tokens(req, count),
        None => (0..count).map(|i| format!("mock-token-{i} ")).collect(),
    };
    if echo_limit {
        if let Some(first) = tokens.first_mut() {
            *first = format!("limit={count} ");
        }
    }
    let input_tokens = match mock_input_tokens(req) {
        Ok(tokens) => tokens,
        Err(error) => {
            let _ = tx.send(Err(Status::data_loss(error))).await;
            return;
        }
    };
    let mut output_tokens = 0u64;
    for (seq, text) in tokens.into_iter().enumerate() {
        let chunk = CanonChunk {
            text,
            reasoning: String::new(),
            // Fake token-ids: by seq; in "foreign tokenizer" -- outside the qwen vocabulary.
            token_ids: if foreign {
                vec![999_999]
            } else if fat_chunk {
                let base = (seq as u32).saturating_mul(FAT_CHUNK_TOKENS);
                (0..FAT_CHUNK_TOKENS).map(|offset| base + offset).collect()
            } else if straddle {
                let base = (seq as u32).saturating_mul(2);
                if seq == 0 {
                    vec![base]
                } else {
                    vec![base, base + 1]
                }
            } else {
                vec![seq as u32]
            },
            seq: seq as u64,
            // R3: the gateway declares the available signals on the first chunk. The mock yields (fake)
            // token_ids; the tokenizer family is the mock profile.
            manifest: (seq == 0).then(|| {
                if substitute {
                    // Fixture: real family + a foreign model (!= frame) -> buyer's B7 -> Bail.
                    SignalManifest {
                        tokenizer_family: "qwen".to_string(),
                        has_token_ids: true,
                        claimed_model: "substituted/cheap-model".to_string(),
                    }
                } else if foreign {
                    // Claims the qwen tokenizer, but token_ids are outside the qwen vocabulary -> B5 -> Bail.
                    // claimed_model is empty (R4, don't fabricate) -> B7 doesn't run; B5 is what catches it.
                    SignalManifest {
                        tokenizer_family: "qwen".to_string(),
                        has_token_ids: true,
                        claimed_model: String::new(),
                    }
                } else {
                    // Mock (Permissive -- B5/B7 pass).
                    SignalManifest {
                        tokenizer_family: "mock".to_string(),
                        has_token_ids: true,
                        claimed_model: claimed_model.unwrap_or("mock").to_string(),
                    }
                }
            }),
            usage: None,
        };
        let chunk_tokens = match u64::try_from(chunk.token_ids.len()) {
            Ok(tokens) => tokens,
            Err(_) => {
                let _ = tx
                    .send(Err(Status::data_loss(
                        "mock token-id count does not fit u64",
                    )))
                    .await;
                return;
            }
        };
        output_tokens = match output_tokens.checked_add(chunk_tokens) {
            Some(tokens) => tokens,
            None => {
                let _ = tx
                    .send(Err(Status::data_loss(
                        "mock output-token total overflows u64",
                    )))
                    .await;
                return;
            }
        };
        if tx.send(Ok(UpstreamEvent::Chunk(chunk))).await.is_err() {
            return; // buyer disconnected before terminal usage
        }
    }
    let usage = match BillingUsage::new(input_tokens, output_tokens, None) {
        Ok(usage) => usage,
        Err(error) => {
            let _ = tx.send(Err(Status::data_loss(error))).await;
            return;
        }
    };
    let _ = tx.send(Ok(UpstreamEvent::Usage(usage))).await;
}

fn mock_input_tokens(req: Option<&CanonRequest>) -> Result<u64, &'static str> {
    req.into_iter()
        .flat_map(|request| &request.messages)
        .try_fold(0u64, |total, message| {
            let count = u64::try_from(message.content.split_whitespace().count())
                .map_err(|_| "mock input-token count does not fit u64")?;
            total
                .checked_add(count)
                .ok_or("mock input-token total overflows u64")
        })
}

/// The last `user`-role message in the canonical request (the prompt the output is built from).
fn last_user_message(req: &CanonRequest) -> &str {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .unwrap_or("")
}

/// Deterministic fake output from the prompt: a prefix marker + echo of the words of the last
/// user message, one delta token per word, truncated to `count` (mock model).
fn derive_tokens(req: &CanonRequest, count: u64) -> Vec<String> {
    let prompt = last_user_message(req);
    let mut seed: Vec<String> = Vec::new();
    // Mock marker + echo of the prompt token-by-token (R6: incrementality is preserved).
    seed.push("mock-reply: ".to_string());
    for word in prompt.split_whitespace() {
        seed.push(format!("{word} "));
    }
    if seed.len() == 1 {
        // Empty prompt -- still yield something deterministic.
        seed.push("(empty) ".to_string());
    }
    (0..count)
        .map(|index| seed[(index % seed.len() as u64) as usize].clone())
        .collect()
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
        run(8, Some(&req), tx, false, None).await;
        let mut chunks = Vec::new();
        let mut usage = None;
        while let Some(item) = rx.recv().await {
            match item.unwrap() {
                UpstreamEvent::Chunk(chunk) => chunks.push(chunk),
                UpstreamEvent::Usage(record) => usage = Some(record),
            }
        }
        // seq monotonic from 0; marker + echo of the prompt words.
        assert_eq!(chunks[0].seq, 0);
        let text: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert!(text.contains("mock-reply"));
        assert!(text.contains("ping") && text.contains("pong"));
        assert_eq!(
            usage,
            Some(BillingUsage::new(2, 8, None).unwrap()),
            "mock bills all message input plus the exact emitted fake token-id count"
        );
    }

    #[tokio::test]
    async fn mock_counts_all_message_content_and_keeps_output_cap_independent() {
        let req = CanonRequest {
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: "one two three".into(),
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: String::new(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: "four five".into(),
                },
            ],
            params: None,
        };
        let (tx, mut rx) = mpsc::channel(8);
        run(2, Some(&req), tx, false, None).await;
        let mut chunks = 0;
        let mut usage = None;
        while let Some(event) = rx.recv().await {
            match event.unwrap() {
                UpstreamEvent::Chunk(_) => chunks += 1,
                UpstreamEvent::Usage(record) => usage = Some(record),
            }
        }
        assert_eq!(chunks, 2, "output alone is capped");
        assert_eq!(usage, Some(BillingUsage::new(5, 2, None).unwrap()));
    }

    #[test]
    fn mock_token_count_is_exact_even_when_prompt_is_short() {
        let req = CanonRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "ping".into(),
            }],
            params: None,
        };
        assert_eq!(derive_tokens(&req, 5).len(), 5);
    }
}
