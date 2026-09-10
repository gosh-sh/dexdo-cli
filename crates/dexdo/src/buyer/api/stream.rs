use crate::buyer::api::accounted_tokens;
use crate::buyer::verify::{StreamVerifier, Verdict};
use dexdo_proto::{BillingUsage, CanonChunk};
use tokio_stream::StreamExt;

pub(super) enum CanonStreamError {
    Upstream(tonic::Status),
    Local(String),
}

impl CanonStreamError {
    pub(super) fn is_capacity(&self) -> bool {
        matches!(self, Self::Upstream(status) if status.code() == tonic::Code::ResourceExhausted)
    }

    pub(super) fn policy_message(&self) -> &str {
        match self {
            Self::Upstream(status) => status.message(),
            Self::Local(message) => message,
        }
    }
}

impl From<String> for CanonStreamError {
    fn from(message: String) -> Self {
        Self::Local(message)
    }
}

impl std::fmt::Display for CanonStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upstream(status) => status.fmt(formatter),
            Self::Local(message) => formatter.write_str(message),
        }
    }
}

pub(super) enum CanonStreamNext {
    Chunk(CanonChunk),
    Usage(BillingUsage),
    End,
    Bailed,
    Errored(CanonStreamError),
}

pub(super) struct CanonStreamDriver {
    upstream: tonic::Streaming<CanonChunk>,
    session_closed: tokio::sync::watch::Receiver<bool>,
    verifier: StreamVerifier,
    received: u64,
    legacy_received: u64,
    output_limit: u64,
    billing_grant: u64,
    saw_usage: bool,
    mock_stream: bool,
    saw_output: bool,
    next_seq: u64,
    bailed: bool,
}

impl CanonStreamDriver {
    pub(super) fn new(
        upstream: tonic::Streaming<CanonChunk>,
        session_closed: tokio::sync::watch::Receiver<bool>,
        expected_model: String,
        output_limit: u64,
        billing_grant: u64,
        mock_stream: bool,
    ) -> Self {
        Self {
            upstream,
            session_closed,
            verifier: StreamVerifier::with_expected_model(expected_model),
            received: 0,
            legacy_received: 0,
            output_limit,
            billing_grant,
            saw_usage: false,
            mock_stream,
            saw_output: false,
            next_seq: 0,
            bailed: false,
        }
    }

    pub(super) async fn next(&mut self) -> CanonStreamNext {
        if *self.session_closed.borrow() {
            return CanonStreamNext::Errored(
                "deal session closed after accepted-output deadline"
                    .to_string()
                    .into(),
            );
        }
        let next = tokio::select! {
            next = self.upstream.next() => next,
            changed = self.session_closed.changed() => {
                if changed.is_ok() && *self.session_closed.borrow() {
                    return CanonStreamNext::Errored(
                        "deal session closed after accepted-output deadline"
                            .to_string()
                            .into(),
                    );
                }
                self.upstream.next().await
            }
        };
        match next {
            Some(Ok(chunk)) => {
                if self.saw_usage {
                    return CanonStreamNext::Errored(
                        "canonical content followed terminal billing usage"
                            .to_string()
                            .into(),
                    );
                }
                if let Some(usage) = chunk.usage.as_ref() {
                    if !chunk.text.is_empty()
                        || !chunk.reasoning.is_empty()
                        || !chunk.token_ids.is_empty()
                        || chunk.manifest.is_some()
                    {
                        return CanonStreamNext::Errored(
                            "terminal billing usage frame also carries content"
                                .to_string()
                                .into(),
                        );
                    }
                    if chunk.seq != self.next_seq {
                        return CanonStreamNext::Errored(
                            format!(
                                "terminal billing usage sequence {} is not next expected {}",
                                chunk.seq, self.next_seq
                            )
                            .into(),
                        );
                    }
                    if let Err(error) = usage.validate() {
                        return CanonStreamNext::Errored(error.to_string().into());
                    }
                    if usage.output_tokens > self.output_limit {
                        return CanonStreamNext::Errored(
                            format!(
                                "terminal output usage {} exceeds output limit {}",
                                usage.output_tokens, self.output_limit
                            )
                            .into(),
                        );
                    }
                    if usage.total_tokens > self.billing_grant {
                        return CanonStreamNext::Errored(
                            format!(
                                "terminal billable usage {} exceeds billing grant {}",
                                usage.total_tokens, self.billing_grant
                            )
                            .into(),
                        );
                    }
                    let valid_output = if self.mock_stream {
                        usage.output_tokens == self.received
                    } else {
                        usage.output_tokens >= self.received
                    };
                    if !valid_output {
                        let relation = if self.mock_stream { "equal" } else { "cover" };
                        return CanonStreamNext::Errored(
                            format!(
                                "terminal output usage {} does not {relation} visible output lower bound {}",
                                usage.output_tokens, self.received
                            )
                            .into(),
                        );
                    }
                    if !self.saw_output || usage.output_tokens == 0 {
                        return CanonStreamNext::Errored(
                            "terminal billing usage has no preceding delivered output"
                                .to_string()
                                .into(),
                        );
                    }
                    if !self.mock_stream {
                        self.received = usage.output_tokens;
                    }
                    self.saw_usage = true;
                    return CanonStreamNext::Usage(*usage);
                }
                if let Verdict::Bail(reason) = self.verifier.verify(&chunk) {
                    tracing::warn!(%reason, "verify: bail -- bailing off the stream (B10)");
                    self.bailed = true;
                    CanonStreamNext::Bailed
                } else {
                    if chunk.seq != self.next_seq {
                        return CanonStreamNext::Errored(
                            format!(
                                "canonical content sequence {} is not next expected {}",
                                chunk.seq, self.next_seq
                            )
                            .into(),
                        );
                    }
                    self.next_seq = match chunk.seq.checked_add(1) {
                        Some(next) => next,
                        None => {
                            return CanonStreamNext::Errored(
                                "canonical content sequence overflows u64"
                                    .to_string()
                                    .into(),
                            )
                        }
                    };
                    CanonStreamNext::Chunk(chunk)
                }
            }
            Some(Err(e)) => CanonStreamNext::Errored(CanonStreamError::Upstream(e)),
            None if self.saw_usage => CanonStreamNext::End,
            None => CanonStreamNext::Errored(
                "canonical stream ended without terminal billing usage"
                    .to_string()
                    .into(),
            ),
        }
    }

    fn chunk_output_lower_bound(&self, chunk: &CanonChunk) -> u64 {
        if self.mock_stream || !chunk.token_ids.is_empty() {
            chunk.visible_output_tokens()
        } else {
            0
        }
    }

    /// Exact token IDs are capped before rendering. Text-only provider fragments are not token
    /// units, so their output cap is verified from the provider-native terminal usage instead.
    pub(super) fn admits(&self, chunk: &CanonChunk) -> bool {
        self.received
            .saturating_add(self.chunk_output_lower_bound(chunk))
            <= self.output_limit
    }

    pub(super) fn account_rendered(&mut self, chunk: &CanonChunk) -> bool {
        self.received = self
            .received
            .saturating_add(self.chunk_output_lower_bound(chunk));
        self.saw_output |=
            !chunk.text.is_empty() || !chunk.reasoning.is_empty() || !chunk.token_ids.is_empty();
        self.legacy_received = self.legacy_received.saturating_add(accounted_tokens(chunk));
        self.received >= self.output_limit
    }

    pub(super) fn bailed(&self) -> bool {
        self.bailed
    }

    pub(super) fn received(&self) -> u64 {
        self.received.max(u64::from(self.saw_output))
    }

    pub(super) fn legacy_received(&self) -> u64 {
        self.legacy_received
    }
}
