use crate::buyer::api::accounted_tokens;
use crate::buyer::verify::{StreamVerifier, Verdict};
use dexdo_proto::CanonChunk;
use tokio_stream::StreamExt;

pub(super) enum CanonStreamNext {
    Chunk(CanonChunk),
    End,
    Bailed,
    Errored(String),
}

pub(super) struct CanonStreamDriver {
    upstream: tonic::Streaming<CanonChunk>,
    verifier: StreamVerifier,
    received: u64,
    max_tokens: u64,
    bailed: bool,
}

impl CanonStreamDriver {
    pub(super) fn new(
        upstream: tonic::Streaming<CanonChunk>,
        expected_model: String,
        max_tokens: u64,
    ) -> Self {
        Self {
            upstream,
            verifier: StreamVerifier::with_expected_model(expected_model),
            received: 0,
            max_tokens,
            bailed: false,
        }
    }

    pub(super) async fn next(&mut self) -> CanonStreamNext {
        match self.upstream.next().await {
            Some(Ok(chunk)) => {
                if let Verdict::Bail(reason) = self.verifier.verify(&chunk) {
                    tracing::warn!(%reason, "verify: bail — bailing off the stream (B10)");
                    self.bailed = true;
                    CanonStreamNext::Bailed
                } else {
                    CanonStreamNext::Chunk(chunk)
                }
            }
            Some(Err(e)) => CanonStreamNext::Errored(e.to_string()),
            None => CanonStreamNext::End,
        }
    }

    pub(super) fn account_rendered(&mut self, chunk: &CanonChunk) -> bool {
        self.received = self.received.saturating_add(accounted_tokens(chunk));
        self.received >= self.max_tokens
    }

    pub(super) fn bailed(&self) -> bool {
        self.bailed
    }

    pub(super) fn received(&self) -> u64 {
        self.received
    }
}
