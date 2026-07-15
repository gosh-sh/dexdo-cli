//! Gateway upstream token source (§10.5 A, R1–R6). Two adapters:
//!  - [`mock`] — mock model (`--mock-model`): deterministic fake tokens from the prompt
//!    (a standard debug mode, AGENTS.md §2);
//!  - [`openai`] — **real OpenAI-compatible upstream** (Directive 3, first step): Groq,
//!    streaming SSE → normalization into `CanonChunk` (R1/R2/R5/R6).
//!
//! Both branches normalize the upstream output into a single canonical stream (R1). Accounting is
//! done by the gateway from structured token signals (`token_ids`/logprobs) and converted to ticks
//! using the canonical `TICK_SIZE`; `CanonChunk` is only a streaming container.

pub mod mock;
pub mod openai;

use dexdo_proto::{CanonChunk, CanonRequest};
use tokio::sync::mpsc;
use tonic::Status;

/// Gateway upstream choice (`--mock-model` vs the real adapter). Configured at seller startup
/// and **immutable** for the gateway's lifetime. The real branch carries base-url + model id;
/// the key is read from the environment at runtime (see [`openai`]) and is not stored here.
#[derive(Clone)]
pub enum UpstreamConfig {
    /// Mock model (Directive 1): deterministic fake tokens from the prompt.
    Mock,
    /// Instance scammer (Directive 5): a mock that UNCONDITIONALLY substitutes the model (claims one other than
    /// the frame's) — a seller that client-side verification (B7) is obligated to catch. For the failover e2e.
    MockScammer,
    /// Real OpenAI-compatible upstream (Groq, etc.): API base + market model id.
    OpenAi(openai::OpenAiConfig),
}

impl UpstreamConfig {
    /// Run the upstream: normalize its output into `CanonChunk` and send it incrementally into
    /// `tx` (R6). `count` is the stream's token budget: no more than `count` delivered tokens. `req` is
    /// the buyer's canonical request (R1). Finishes on upstream
    /// exhaustion, on reaching `count`, or when the buyer disconnected (`tx` closed = STOP).
    pub async fn run(
        &self,
        count: u64,
        req: Option<CanonRequest>,
        tx: mpsc::Sender<Result<CanonChunk, Status>>,
    ) {
        match self {
            UpstreamConfig::Mock => mock::run(count, req.as_ref(), tx, false).await,
            UpstreamConfig::MockScammer => mock::run(count, req.as_ref(), tx, true).await,
            UpstreamConfig::OpenAi(cfg) => openai::run(cfg, count, req, tx).await,
        }
    }
}
