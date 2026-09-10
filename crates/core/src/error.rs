//! Structured user-facing errors: a stable code, a coarse kind, one human sentence, and the
//! **preserved** `std::error::Error` source chain.

//! The debugging session in lost hours because `advertised_gateway failed: transport error`
//! was the whole error: the probed address was not named, and the real cause (`io:...` under a
//! `tonic::transport::Error` whose `Display` is the literal string `transport error`) had already
//! been thrown away by an `error.to_string()` at the boundary. The rule this module enforces is
//! that a cause chain is never flattened into a string early -- [`DexdoError`] keeps the source as
//! a live `dyn Error` and only walks it when it renders.

//! Rendered shape (one line per element, deterministic, greppable):

//! ```text
//! error[E_ADVERTISE_UNREACHABLE] (network): advertised gateway 94.156.178.14:8443 did not complete the pinned-TLS (h2) self-probe (stage: tls_handshake)
//! cause: transport error
//! cause: io: connection reset by peer
//! secondary (pool owner-fill audit): error[E_POOL_UNKNOWN_OWNER_FILL] (pool):...
//! hint: the advertised address must be reachable from this host
//! ```

//! `Display` renders the WHOLE shape (headline + causes + secondary notes + hint), so a renderer
//! must not additionally print the `anyhow` cause chain on top of it or every cause appears twice.
//! `source()` still returns the live chain, so programmatic inspection (`downcast_ref`,
//! `anyhow::Error::chain`) keeps working.

//! Every code lives in [`codes::TABLE`] and has a row in `error-codes.md`
//! (code -> meaning -> likely fix). A code with no table entry is a defect, and
//! `code_table_matches_the_documented_table` fails the build for it.

use std::error::Error as StdError;
use std::fmt;

/// The erased error type used for preserved sources. `Send + Sync` so a [`DexdoError`] can travel
/// through `anyhow::Error` and across tasks.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// Coarse category of a user-facing failure, for grouping and for `grep`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorKind {
    /// Operator input: flags, config files, manifests.
    Config,
    /// Reachability/transport: TCP, DNS, HTTP/2, timeouts.
    Network,
    /// TLS specifically, including certificate pinning (a wrong endpoint, not a flaky one).
    Tls,
    /// The seller's own deal pool: capacity, handles, lineage.
    Pool,
}

impl ErrorKind {
    /// The lowercase token rendered inside `(...)` on the headline.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Network => "network",
            Self::Tls => "tls",
            Self::Pool => "pool",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One documented row of the error-code table: the stable identifier plus the two things an
/// operator needs when they see it (what it means, what to do about it).

/// A [`DexdoError`] can only be built from an `ErrorCode`, so a code that is not declared in
/// [`codes`] cannot be emitted, and every declared code is checked against `error-codes.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorCode {
    code: &'static str,
    kind: ErrorKind,
    meaning: &'static str,
    fix: &'static str,
}

impl ErrorCode {
    /// Declare a code. Private to this module: codes are the closed set in [`codes`].
    const fn new(
        code: &'static str,
        kind: ErrorKind,
        meaning: &'static str,
        fix: &'static str,
    ) -> Self {
        Self {
            code,
            kind,
            meaning,
            fix,
        }
    }

    /// The stable, greppable identifier (`E_...`).
    pub const fn code(self) -> &'static str {
        self.code
    }

    /// The coarse category rendered on the headline.
    pub const fn kind(self) -> ErrorKind {
        self.kind
    }

    /// What the code means, for the documented table.
    pub const fn meaning(self) -> &'static str {
        self.meaning
    }

    /// The likely fix, for the documented table.
    pub const fn fix(self) -> &'static str {
        self.fix
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

/// The closed set of user-facing error codes. Each one has a row in `error-codes.md`.
pub mod codes {
    use super::{ErrorCode, ErrorKind};

    /// the advertised gateway host cannot be dialled by a remote buyer.
    pub const E_ADVERTISE_NOT_PUBLIC: ErrorCode = ErrorCode::new(
        "E_ADVERTISE_NOT_PUBLIC",
        ErrorKind::Config,
        "--gateway-advertise names an address no remote buyer can dial (bind-all, loopback, \
         RFC1918/ULA, link-local, CGNAT, or a reserved local name)",
        "pass a public host:port reachable from the internet, or run on a public host; for \
         local/LAN testing only, use --allow-private-advertise",
    );

    /// the advertised address did not answer the pinned-TLS (h2) self-probe.
    pub const E_ADVERTISE_UNREACHABLE: ErrorCode = ErrorCode::new(
        "E_ADVERTISE_UNREACHABLE",
        ErrorKind::Network,
        "the pinned-TLS (h2) self-probe of the advertised gateway failed at the transport level; \
         the failing stage and the underlying cause are on the cause lines",
        "check that the advertised address is reachable from this host and forwards to this \
         gateway; a NAT/VPN/reverse-tunnel hairpin can fail the self-probe while a remote buyer \
         connects fine ()",
    );

    /// something answered on the advertised address, but it is not this gateway.
    pub const E_ADVERTISE_WRONG_GATEWAY: ErrorCode = ErrorCode::new(
        "E_ADVERTISE_WRONG_GATEWAY",
        ErrorKind::Tls,
        "the advertised address answered the self-probe but is provably not this gateway \
         (pinned-certificate mismatch, or a foreign service on that port)",
        "point --gateway-advertise at this gateway's own address, or free the port from the other \
         service; never relax the certificate pin",
    );

    /// the buyer could not dial the seller gateway named in its handover.
    pub const E_GATEWAY_UNREACHABLE: ErrorCode = ErrorCode::new(
        "E_GATEWAY_UNREACHABLE",
        ErrorKind::Network,
        "the pinned-TLS (h2) dial of the seller gateway failed at the transport level; the \
         failing stage and the underlying cause are on the cause lines",
        "check that the address on the headline is reachable from this host and that the seller \
         gateway is still serving it; the seller advertises this address, so a wrong advertise or \
         a closed port shows up here",
    );

    /// something answered the buyer's dial, but it is not the seller's gateway.
    pub const E_GATEWAY_WRONG_ENDPOINT: ErrorCode = ErrorCode::new(
        "E_GATEWAY_WRONG_ENDPOINT",
        ErrorKind::Tls,
        "the address in the handover answered, but the certificate it presented does not match \
         the fingerprint the handover pinned, so it is provably not the seller's gateway",
        "do not relax the pin: get a fresh handover from the seller, or have the seller advertise \
         its own address instead of one served by another service",
    );

    /// an owner fill was observed with no same-note deal handle to account it against.
    pub const E_POOL_UNKNOWN_OWNER_FILL: ErrorCode = ErrorCode::new(
        "E_POOL_UNKNOWN_OWNER_FILL",
        ErrorKind::Pool,
        "the seller note was matched on an order (an 'owner fill') whose TokenContract has no \
         deal handle or market manifest in this pool, so the delivered capacity cannot be \
         accounted; the pool refuses to silently discard it",
        "run the seller from the directory holding that deal's handle/market.json, or close the \
         orphaned deal (`dexdo deals`, then `destroy`/`recover`); when it appears as a secondary \
         note under another error, fix that primary error first",
    );

    /// the seller pool could not bring its deals up; carries the primary cause and any
    /// secondary (cascade) notes that were demoted so they cannot masquerade as the root cause.
    pub const E_SELLER_POOL_FAILED: ErrorCode = ErrorCode::new(
        "E_SELLER_POOL_FAILED",
        ErrorKind::Pool,
        "the seller pool failed; the headline is the primary (root) failure and any \
         `secondary` lines are consequences of it, not independent problems",
        "fix the primary failure on the headline and its cause lines; re-run and only then look \
         at the secondary notes",
    );

    /// a command that spends from the funding (Hot) wallet ran with no active wallet binding.
    pub const E_WALLET_NOT_CONFIGURED: ErrorCode = ErrorCode::new(
        "E_WALLET_NOT_CONFIGURED",
        ErrorKind::Config,
        "the command needs a funding (Hot) wallet, and this instance has no active wallet binding; \
         the CLI does not start onboarding on its own and never guesses a provider, because the \
         provider is recorded when the wallet is bound and cannot be recovered from an address, a \
         code hash or an on-chain parameter afterwards",
        "bind a wallet once with `dexdo wallet onboard` followed by one of the providers \
         `ackinacki-wallet`, `gosh-ai` or `manual`; on a headless host with no TTY, no browser and \
         no camera use `dexdo wallet onboard manual`, whose whole input is a plain printed address",
    );

    /// Every declared code. `error-codes.md` is checked against this table.
    pub const TABLE: &[ErrorCode] = &[
        E_ADVERTISE_NOT_PUBLIC,
        E_ADVERTISE_UNREACHABLE,
        E_ADVERTISE_WRONG_GATEWAY,
        E_GATEWAY_UNREACHABLE,
        E_GATEWAY_WRONG_ENDPOINT,
        E_POOL_UNKNOWN_OWNER_FILL,
        E_SELLER_POOL_FAILED,
        E_WALLET_NOT_CONFIGURED,
    ];
}

/// A failure that happened *because of* another failure, kept attached to the primary instead of
/// being reported in its place.
#[derive(Debug)]
struct Secondary {
    label: &'static str,
    error: BoxError,
}

/// A user-facing error: stable code, coarse kind, one human sentence, and the preserved source
/// chain.

/// Build it with [`DexdoError::new`] and the `with_*` modifiers; render it with `Display`.
#[derive(Debug)]
pub struct DexdoError {
    code: &'static str,
    kind: ErrorKind,
    message: String,
    /// Appended to the headline as ` (stage:...)` -- which step of a multi-step operation failed.
    stage: Option<&'static str>,
    hint: Option<Box<str>>,
    source: Option<BoxError>,
    /// When the source was *adopted* (its `Display` became the message), the first cause line
    /// would repeat the headline verbatim; skip it instead of flattening the chain away.
    skip_first_cause: bool,
    secondary: Vec<Secondary>,
}

impl DexdoError {
    /// A user-facing error with a stable code and one human sentence. The sentence must name the
    /// concrete subject (address, TokenContract, file, order id).
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            kind: code.kind(),
            message: message.into(),
            stage: None,
            hint: None,
            source: None,
            skip_first_cause: false,
            secondary: Vec::new(),
        }
    }

    /// Adopt an existing error as a structured primary: its `Display` becomes the message and the
    /// error itself is kept as the source, so its own `source()` chain is preserved (the renderer
    /// skips the duplicated first cause line). Used where a failure that is already the root cause
    /// has to carry attached [`with_secondary`](Self::with_secondary) context.
    pub fn adopt(code: ErrorCode, error: impl Into<BoxError>) -> Self {
        let error = error.into();
        let message = error.to_string();
        Self {
            code: code.code(),
            kind: code.kind(),
            message,
            stage: None,
            hint: None,
            source: Some(error),
            skip_first_cause: true,
            secondary: Vec::new(),
        }
    }

    /// Preserve the underlying error as the source. NEVER stringify it at the call site -- the
    /// whole point of is that `cause:` lines can still be walked at render time.
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        self.source = Some(source.into());
        self.skip_first_cause = false;
        self
    }

    /// Which step of a multi-step operation failed (`tcp_connect`, `tls_handshake`,...). Rendered
    /// as ` (stage:...)` at the end of the headline.
    pub fn with_stage(mut self, stage: &'static str) -> Self {
        self.stage = Some(stage);
        self
    }

    /// One actionable `hint:` line, in the style of `status... is ambiguous: pass --role
    /// buyer|seller` -- it must state the fix, not restate the problem. A multi-line hint is
    /// rendered verbatim, so the caller controls its continuation indent.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into().into_boxed_str());
        self
    }

    /// Attach a failure that is a *consequence* of this one. It renders under `secondary (label):`
    /// and never replaces the primary as the reported error.
    pub fn with_secondary(mut self, label: &'static str, error: impl Into<BoxError>) -> Self {
        self.secondary.push(Secondary {
            label,
            error: error.into(),
        });
        self
    }

    /// The stable code, for `grep` and for the documented table.
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// The coarse category.
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human sentence, without the code/kind prefix and without the stage suffix.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The failing stage, when the operation has stages.
    pub const fn stage(&self) -> Option<&'static str> {
        self.stage
    }

    /// The actionable hint, when there is one.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// `true` when at least one consequence failure is attached.
    pub fn has_secondary(&self) -> bool {
        !self.secondary.is_empty()
    }

    /// The first line only: `error[CODE] (kind): message [(stage:...)]`.
    pub fn headline(&self) -> String {
        match self.stage {
            Some(stage) => format!(
                "error[{}] ({}): {} (stage: {stage})",
                self.code, self.kind, self.message
            ),
            None => format!("error[{}] ({}): {}", self.code, self.kind, self.message),
        }
    }

    /// Walk the preserved source chain, deepest last. Empty when there is no source (or when the
    /// source was adopted and only repeats the headline).
    pub fn causes(&self) -> impl Iterator<Item = &(dyn StdError + 'static)> {
        let mut next = self.source.as_deref().map(|error| error as &dyn StdError);
        if self.skip_first_cause {
            next = next.and_then(StdError::source);
        }
        std::iter::from_fn(move || {
            let current = next?;
            next = current.source();
            Some(current)
        })
    }
}

/// Indent the continuation lines of an already-rendered block so a nested render stays readable.
fn indent_continuations(rendered: &str, indent: &str) -> String {
    rendered.replace('\n', &format!("\n{indent}"))
}

impl fmt::Display for DexdoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.headline())?;
        for cause in self.causes() {
            write!(
                formatter,
                "\n  cause: {}",
                indent_continuations(&cause.to_string(), "  ")
            )?;
        }
        for secondary in &self.secondary {
            write!(
                formatter,
                "\n  secondary ({}): {}",
                secondary.label,
                indent_continuations(&secondary.error.to_string(), "  ")
            )?;
        }
        if let Some(hint) = &self.hint {
            write!(formatter, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

impl StdError for DexdoError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source.as_deref().map(|error| error as &dyn StdError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A three-level chain built out of real `std::error::Error` implementors, so the test proves
    /// the chain is walked rather than re-printed from a pre-formatted string.
    #[derive(Debug)]
    struct Layer {
        message: &'static str,
        source: Option<Box<Layer>>,
    }

    impl fmt::Display for Layer {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl StdError for Layer {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn StdError + 'static))
        }
    }

    fn deep_chain() -> Layer {
        Layer {
            message: "transport error",
            source: Some(Box::new(Layer {
                message: "http2 handshake failed",
                source: Some(Box::new(Layer {
                    message: "io: connection reset by peer",
                    source: None,
                })),
            })),
        }
    }

    #[test]
    fn renders_the_documented_shape() {
        let error = DexdoError::new(
            codes::E_ADVERTISE_UNREACHABLE,
            "advertised gateway 94.156.178.14:8443 did not complete the pinned-TLS (h2) self-probe",
        )
        .with_stage("tls_handshake")
        .with_source(deep_chain())
        .with_hint("the advertised address must be reachable AND serve this gateway's cert");
        assert_eq!(
            error.to_string(),
            "error[E_ADVERTISE_UNREACHABLE] (network): advertised gateway 94.156.178.14:8443 did \
             not complete the pinned-TLS (h2) self-probe (stage: tls_handshake)\n  \
             cause: transport error\n  \
             cause: http2 handshake failed\n  \
             cause: io: connection reset by peer\n  \
             hint: the advertised address must be reachable AND serve this gateway's cert"
        );
        assert_eq!(error.code(), "E_ADVERTISE_UNREACHABLE");
        assert_eq!(error.kind(), ErrorKind::Network);
        assert_eq!(error.stage(), Some("tls_handshake"));
    }

    #[test]
    fn a_source_less_error_renders_only_the_headline() {
        let error = DexdoError::new(codes::E_ADVERTISE_NOT_PUBLIC, "--gateway-advertise 0.0.0.0:8443 is not reachable by remote buyers (bind-all wildcard)");
        assert_eq!(
            error.to_string(),
            "error[E_ADVERTISE_NOT_PUBLIC] (config): --gateway-advertise 0.0.0.0:8443 is not \
             reachable by remote buyers (bind-all wildcard)"
        );
        assert!(error.causes().next().is_none());
        assert!(error.hint().is_none());
        assert!(!error.has_secondary());
    }

    /// 's central requirement: the chain is preserved, not flattened into a string at the
    /// boundary. The deepest cause -- the one `advertised_gateway failed: transport error` threw
    /// away in -- must still be reachable and rendered.
    #[test]
    fn the_source_chain_is_not_flattened_into_a_string() {
        let error = DexdoError::new(codes::E_ADVERTISE_UNREACHABLE, "probe failed")
            .with_source(deep_chain());
        let causes: Vec<String> = error.causes().map(ToString::to_string).collect();
        assert_eq!(
            causes,
            vec![
                "transport error".to_string(),
                "http2 handshake failed".to_string(),
                "io: connection reset by peer".to_string(),
            ]
        );
        // The deep cause survives BOTH the programmatic walk and the render.
        assert!(error
            .to_string()
            .contains("cause: io: connection reset by peer"));
        // And `std::error::Error::source` still exposes the live chain, so a caller can downcast
        // instead of string-matching.
        let mut depth = 0;
        let mut current = StdError::source(&error);
        while let Some(source) = current {
            assert!(source.downcast_ref::<Layer>().is_some());
            depth += 1;
            current = source.source();
        }
        assert_eq!(depth, 3);
    }

    /// The cascade rule (issue example 2): the readiness failure is the reported error and the
    /// pool teardown finding hangs off it, marked `secondary`.
    #[test]
    fn a_cascade_reports_the_primary_and_attaches_the_secondary() {
        let primary = Layer {
            message: "seller readiness failed before SELL: advertised_gateway failed",
            source: Some(Box::new(Layer {
                message: "io: connection reset by peer",
                source: None,
            })),
        };
        let secondary = DexdoError::new(
            codes::E_POOL_UNKNOWN_OWNER_FILL,
            "seller owner fill for TokenContract 0:958f77 has no same-note deal handle/manifest",
        )
        .with_hint("this is a consequence of the primary failure above");
        let error = DexdoError::adopt(codes::E_SELLER_POOL_FAILED, primary)
            .with_secondary("pool owner-fill audit", secondary);
        let rendered = error.to_string();
        assert_eq!(
            rendered,
            "error[E_SELLER_POOL_FAILED] (pool): seller readiness failed before SELL: \
             advertised_gateway failed\n  \
             cause: io: connection reset by peer\n  \
             secondary (pool owner-fill audit): error[E_POOL_UNKNOWN_OWNER_FILL] (pool): seller \
             owner fill for TokenContract 0:958f77 has no same-note deal handle/manifest\n    \
             hint: this is a consequence of the primary failure above"
        );
        // The primary is the headline; the secondary never sits on the first line.
        assert!(rendered.starts_with("error[E_SELLER_POOL_FAILED]"));
        let first_line = rendered.lines().next().unwrap();
        assert!(!first_line.contains("owner fill"), "{first_line}");
        assert!(error.has_secondary());
    }

    /// `adopt` must not flatten: the adopted error's own sources still render, only the duplicated
    /// first line is suppressed.
    #[test]
    fn adopt_keeps_the_adopted_chain_and_drops_only_the_duplicate_line() {
        let error = DexdoError::adopt(codes::E_SELLER_POOL_FAILED, deep_chain());
        let causes: Vec<String> = error.causes().map(ToString::to_string).collect();
        assert_eq!(
            causes,
            vec![
                "http2 handshake failed".to_string(),
                "io: connection reset by peer".to_string(),
            ]
        );
        assert_eq!(
            error.to_string(),
            "error[E_SELLER_POOL_FAILED] (pool): transport error\n  \
             cause: http2 handshake failed\n  \
             cause: io: connection reset by peer"
        );
        // The whole adopted error, including the line used as the message, is still reachable.
        assert!(StdError::source(&error).is_some());
    }

    #[test]
    fn a_multi_line_hint_renders_verbatim() {
        let error = DexdoError::new(codes::E_ADVERTISE_NOT_PUBLIC, "not public").with_hint(
            "pass a public host:port reachable from the internet, or run on a public host;\
             \n        for local/LAN testing only, use --allow-private-advertise",
        );
        assert_eq!(
            error.to_string(),
            "error[E_ADVERTISE_NOT_PUBLIC] (config): not public\n  \
             hint: pass a public host:port reachable from the internet, or run on a public host;\n        \
             for local/LAN testing only, use --allow-private-advertise"
        );
    }

    #[test]
    fn codes_are_unique_well_formed_and_carry_a_meaning_and_a_fix() {
        let mut seen = BTreeSet::new();
        for entry in codes::TABLE {
            assert!(
                seen.insert(entry.code()),
                "duplicate error code {}",
                entry.code()
            );
            assert!(
                entry.code().starts_with("E_"),
                "{} must start with E_",
                entry.code()
            );
            assert!(
                entry
                    .code()
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "{} must be SCREAMING_SNAKE ASCII",
                entry.code()
            );
            assert!(
                !entry.meaning().is_empty(),
                "{} has no meaning",
                entry.code()
            );
            assert!(!entry.fix().is_empty(), "{} has no fix", entry.code());
        }
    }

    /// Parse the `| code | kind | meaning | likely fix |` rows out of the documented table.
    fn documented_rows() -> Vec<(String, String)> {
        include_str!("../../../error-codes.md")
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("| `E_"))
            .map(|line| {
                let mut cells = line.trim_matches('|').split('|').map(str::trim);
                let code = cells
                    .next()
                    .unwrap_or_default()
                    .trim_matches('`')
                    .to_string();
                let kind = cells
                    .next()
                    .unwrap_or_default()
                    .trim_matches('`')
                    .to_string();
                (code, kind)
            })
            .collect()
    }

    /// "a code with no table entry is a defect". Both directions -- no undocumented code, and
    /// no stale documented code that no longer exists.
    #[test]
    fn code_table_matches_the_documented_table() {
        let documented = documented_rows();
        assert!(
            !documented.is_empty(),
            "error-codes.md has no `| `E_...`` rows"
        );
        let documented_codes: BTreeSet<&str> =
            documented.iter().map(|(code, _)| code.as_str()).collect();
        for entry in codes::TABLE {
            assert!(
                documented_codes.contains(entry.code()),
                "{} is constructible but has no row in error-codes.md",
                entry.code()
            );
        }
        let declared: BTreeSet<&str> = codes::TABLE.iter().map(|entry| entry.code()).collect();
        for code in &documented_codes {
            assert!(
                declared.contains(code),
                "error-codes.md documents {code}, which no longer exists in codes::TABLE"
            );
        }
        // The documented kind must be the kind actually rendered, or the table lies about `grep`.
        for (code, kind) in &documented {
            let entry = codes::TABLE
                .iter()
                .find(|entry| entry.code() == code)
                .expect("checked above");
            assert_eq!(
                entry.kind().as_str(),
                kind,
                "error-codes.md lists {code} as ({kind}) but it renders as ({})",
                entry.kind()
            );
        }
    }

    /// Any `error[CODE]` literal anywhere in the workspace must name a table code -- this catches a
    /// hand-rolled renderer that invents a code without documenting it.
    #[test]
    fn every_rendered_code_literal_is_a_table_code() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let crates = root.join("crates");
        if !crates.is_dir() {
            return; // packaged crate, not the repository checkout
        }
        let declared: BTreeSet<&str> = codes::TABLE.iter().map(|entry| entry.code()).collect();
        let mut stack = vec![crates];
        let mut checked = 0usize;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)
                .expect("readable directory")
                .flatten()
            {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name == "target") {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).expect("readable source");
                    for (index, _) in text.match_indices("error[") {
                        let rest = &text[index + "error[".len()..];
                        let Some(end) = rest.find(']') else { continue };
                        let literal = &rest[..end];
                        // `{E_...}` is the format-string form of the same literal.
                        let code = literal.trim_matches(|c| c == '{' || c == '}');
                        if code.is_empty() || !code.starts_with("E_") {
                            continue;
                        }
                        checked += 1;
                        assert!(
                            declared.contains(code),
                            "{} renders error[{code}], which has no row in codes::TABLE",
                            path.display()
                        );
                    }
                }
            }
        }
        assert!(checked > 0, "the scanner found no error[CODE] literals");
    }

    /// Guard against a code declared in `codes` but left out of `TABLE` -- it would be emittable
    /// with no documented row, which defines as a defect.
    #[test]
    fn every_declared_code_constant_is_in_the_table() {
        let source = include_str!("error.rs");
        let declared: BTreeSet<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub const E_"))
            .filter_map(|rest| rest.split(':').next())
            .collect();
        assert!(
            declared.len() >= codes::TABLE.len(),
            "the scanner found {} declarations for {} table rows",
            declared.len(),
            codes::TABLE.len()
        );
        let tabled: BTreeSet<&str> = codes::TABLE
            .iter()
            .map(|entry| entry.code().trim_start_matches("E_"))
            .collect();
        for name in &declared {
            assert!(
                tabled.contains(name),
                "E_{name} is declared but missing from codes::TABLE"
            );
        }
    }
}
