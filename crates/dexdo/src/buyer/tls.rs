//! Pinning of the gateway's TLS certificate on the buyer side.

//! There is no PKI. Trust in the gateway's self-signed certificate comes from the **fingerprint**
//! that arrived in the note-encrypted handover. The buyer connects over TLS and accepts
//! the connection **only if** the SHA-256 of the presented leaf certificate matches the pinned
//! fingerprint; otherwise it tears down **before** receiving the stream (fail-closed). An active
//! MITM with a foreign certificate is repelled this way.

//! The implementation reuses the rustls stack that tonic already pulls in (tokio-rustls/hyper-util),
//! without a separate TLS stack: a custom `ServerCertVerifier` checks the fingerprint and delegates
//! handshake signature verification to the standard rustls webpki provider.

use anyhow::{anyhow, Result};
use dexdo_core::params::BUYER_GATEWAY_DIAL_TIMEOUT;
use http::Uri;
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    verify_tls12_signature, verify_tls13_signature, CryptoProvider,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
};
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

use crate::seller::tls::fingerprint_der;

/// rustls verifier that pins the gateway's certificate fingerprint.

/// `verify_server_cert` accepts the certificate **only** when SHA-256(DER) matches the pinned
/// fingerprint. The TLS handshake signature is verified by the standard webpki provider -- so
/// fingerprint pinning complements (does not replace) the cryptographic proof of key ownership.
#[derive(Debug)]
struct PinnedFingerprintVerifier {
    expected: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        let presented = fingerprint_der(end_entity.as_ref());
        if presented == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            // Fail-closed: foreign certificate (MITM) -- rejection BEFORE receiving any stream.
            Err(RustlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Which step of the pinned dial failed, kept in the error chain so the stage survives tonic's
/// opaque `transport error`.

/// `tonic::transport::Error`'s whole `Display` is the literal string `transport error`, and the
/// connector's own error is only reachable through `source()`. Tagging the step here is what lets
/// [`dial_stage`] report `tcp_connect` versus `tls_handshake` without dialling a second time and
/// without sniffing the rendered strings.
#[derive(Debug)]
pub struct DialStageError {
    stage: &'static str,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl DialStageError {
    fn new(
        stage: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            stage,
            source: source.into(),
        }
    }

    /// The step that failed during channel setup.
    pub const fn stage(&self) -> &'static str {
        self.stage
    }
}

impl std::fmt::Display for DialStageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "pinned dial failed at {}", self.stage)
    }
}

impl std::error::Error for DialStageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The step of the pinned dial that a [`connect_pinned`] failure actually reached.

/// The connector tags TCP and TLS failures. Untagged errors during channel setup come from
/// tonic's h2 handshake.
pub fn dial_stage(error: &anyhow::Error) -> &'static str {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<DialStageError>())
        .map_or("http2_handshake", DialStageError::stage)
}

/// The dial failure as the operator needs to see it: the stage it reached and every cause under it.

/// A `tonic` transport error renders as the bare words `transport error`, and its Display walks no
/// sources -- so the connect refusal, the TLS alert or the h2 rejection that actually happened never
/// reaches the message the buyer returns. was diagnosed by hand for that reason alone.
pub fn dial_failure_detail(error: &anyhow::Error) -> String {
    // A dial that went through `gateway_dial_error` is ALREADY the finished report:
    // `DexdoError`'s `Display` walks the whole cause chain itself and closes with the hint, and
    // its headline already carries the stage. Walking it a second time here printed every cause
    // twice -- appended after the `hint:` line, so it read as part of the hint -- and stamped a
    // second stage on top of the first. On a pin mismatch that second stage CONTRADICTED the
    // classification: `gateway_dial_error` deliberately reports `tls_certificate_pin`, and the
    // re-derived tag overwrote it with the generic `tls_handshake` the structured error exists
    // to replace.
    if error.downcast_ref::<dexdo_core::DexdoError>().is_some() {
        return error.to_string();
    }
    // Anything that did NOT come from the dial -- a `tonic::Status` from `authorize` or
    // `open_stream` -- renders only its own line, so the causes under it are added here.
    let mut causes = error
        .chain()
        .skip(1)
        .map(|cause| cause.to_string())
        .peekable();
    let stage = dial_stage(error);
    if causes.peek().is_none() {
        return format!("{error} (stage: {stage})");
    }
    let chain = causes.collect::<Vec<_>>().join("; caused by: ");
    format!("{error} (stage: {stage}); caused by: {chain}")
}

/// `true` when the peer answered but presented a certificate that is not the pinned one.

/// This is the exact `rustls` variant [`PinnedFingerprintVerifier`] returns on a fingerprint
/// mismatch, matched as a type rather than sniffed out of a rendered string -- so a wrong endpoint
/// is never confused with a flaky one, and vice versa.
pub fn dial_reached_wrong_endpoint(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        matches!(
            source
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .and_then(|inner| inner.downcast_ref::<RustlsError>()),
            Some(RustlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure
            ))
        )
    })
}

/// The structured form of a failed buyer dial of a seller gateway: the address that was
/// dialled, the stage it reached, and the preserved cause chain.

/// The buyer path used to report the whole failure as `error=transport error` with the address
/// nowhere in the log, at any level up to `RUST_LOG=trace` -- there was nothing to tell an
/// unreachable host from a wrong advertised address from a closed port.
pub fn gateway_dial_error(endpoint: &str, error: anyhow::Error) -> anyhow::Error {
    let dialled = endpoint
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let structured = if dial_reached_wrong_endpoint(&error) {
        dexdo_core::DexdoError::new(
            dexdo_core::error_codes::E_GATEWAY_WRONG_ENDPOINT,
            format!(
                "seller gateway {dialled} answered, but its certificate does not match the \
                 fingerprint pinned by the handover"
            ),
        )
        .with_stage("tls_certificate_pin")
        .with_hint(
            "the pin is never relaxed: get a fresh handover from the seller, or have the seller \
             advertise its own address instead of one served by another service",
        )
    } else {
        let stage = dial_stage(&error);
        dexdo_core::DexdoError::new(
            dexdo_core::error_codes::E_GATEWAY_UNREACHABLE,
            format!("seller gateway {dialled} did not complete the pinned-TLS (h2) dial"),
        )
        .with_stage(stage)
        .with_hint(format!(
            "verify the address answers from this host: `curl -k https://{dialled}/`. \
             tcp_connect means the host/port is not reachable at all (down, firewalled, or the \
             seller advertised an address that does not route to it); tls_handshake and \
             http2_handshake mean something answered but did not complete the pinned h2 handshake"
        ))
    };
    anyhow::Error::new(structured.with_source(error))
}

const HTTP2_CLIENT_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Tonic hands back the channel before h2 processes server SETTINGS. Check the first incoming
/// frame and wait for h2 to validate it and flush its SETTINGS ACK.
struct ServerPrefaceIo {
    stream: TlsStream<TcpStream>,
    preface: Option<ServerPreface>,
}

struct ServerPreface {
    server_header: Http2FrameHeader,
    client_ack: ClientSettingsAck,
    ready: oneshot::Sender<io::Result<()>>,
}

#[derive(Default)]
struct Http2FrameHeader {
    bytes: [u8; 9],
    used: usize,
}

impl Http2FrameHeader {
    fn read_from(&mut self, input: &mut &[u8]) -> bool {
        let copied = input.len().min(self.bytes.len() - self.used);
        self.bytes[self.used..self.used + copied].copy_from_slice(&input[..copied]);
        self.used += copied;
        *input = &input[copied..];
        self.used == self.bytes.len()
    }

    fn is_server_settings(&self) -> bool {
        // RFC 9113: the server preface starts with a SETTINGS frame on stream zero, without ACK.
        let [_, _, _, frame_type, flags, a, b, c, d] = self.bytes;
        let stream = u32::from_be_bytes([a, b, c, d]) & 0x7fff_ffff;
        frame_type == 0x04 && flags & 0x01 == 0 && stream == 0
    }

    fn payload_len(&self) -> usize {
        u32::from_be_bytes([0, self.bytes[0], self.bytes[1], self.bytes[2]]) as usize
    }
}

struct ClientSettingsAck {
    header: Http2FrameHeader,
    skip: usize,
    written: bool,
}

impl ClientSettingsAck {
    fn new() -> Self {
        Self {
            header: Http2FrameHeader::default(),
            skip: HTTP2_CLIENT_PREFACE.len(),
            written: false,
        }
    }

    fn observe(&mut self, mut bytes: &[u8]) {
        while !self.written && !bytes.is_empty() {
            // Skip the client preface and frame payloads. ACK must be its own frame.
            let skipped = bytes.len().min(self.skip);
            self.skip -= skipped;
            bytes = &bytes[skipped..];
            if self.header.read_from(&mut bytes) {
                self.written = self.header.bytes == [0, 0, 0, 0x04, 0x01, 0, 0, 0, 0];
                self.skip = self.header.payload_len();
                self.header = Http2FrameHeader::default();
            }
        }
    }
}

impl ServerPrefaceIo {
    fn finish_preface(&mut self, result: io::Result<()>) {
        if let Some(preface) = self.preface.take() {
            let _ = preface.ready.send(result);
        }
    }
}

impl AsyncRead for ServerPrefaceIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if let (Poll::Ready(Ok(())), Some(preface)) = (&result, &mut self.preface) {
            let mut bytes = &buf.filled()[filled..];
            if preface.server_header.read_from(&mut bytes)
                && !preface.server_header.is_server_settings()
            {
                self.finish_preface(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected a server HTTP/2 SETTINGS frame",
                )));
            }
        }
        result
    }
}

impl AsyncWrite for ServerPrefaceIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        if let (Poll::Ready(Ok(written)), Some(preface)) = (&result, &mut self.preface) {
            preface.client_ack.observe(&buf[..*written]);
        }
        result
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        if let (Poll::Ready(Ok(written)), Some(preface)) = (&result, &mut self.preface) {
            let mut remaining = *written;
            for buf in bufs {
                if remaining == 0 || preface.client_ack.written {
                    break;
                }
                let observed = buf.len().min(remaining);
                preface.client_ack.observe(&buf[..observed]);
                remaining -= observed;
            }
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(())))
            && self.preface.as_ref().is_some_and(|p| p.client_ack.written)
        {
            self.finish_preface(Ok(()));
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Open a gRPC channel to the gateway over TLS, pinning the certificate fingerprint from the
/// handover.

/// `endpoint` is `https://host:port` from the decrypted handover; `fingerprint` is the pinned
/// fingerprint from the same place. If the presented certificate does not match, the connection
/// does not come up.

/// The returned error carries the failing step as a [`DialStageError`] in its chain; a caller that
/// reports it to an operator turns it into the structured form with [`gateway_dial_error`].
pub async fn connect_pinned(endpoint: &str, fingerprint: &str) -> Result<Channel> {
    connect_pinned_with_tcp(
        endpoint,
        fingerprint,
        Some(BUYER_GATEWAY_DIAL_TIMEOUT),
        TcpStream::connect,
    )
    .await
}

/// The seller's readiness probe owns its deadline across the dial and the gRPC challenge.
pub(crate) async fn connect_pinned_for_probe(endpoint: &str, fingerprint: &str) -> Result<Channel> {
    connect_pinned_with_tcp(endpoint, fingerprint, None, TcpStream::connect).await
}

async fn connect_pinned_with_tcp<F>(
    endpoint: &str,
    fingerprint: &str,
    timeout: Option<Duration>,
    connect_tcp: impl Fn(String) -> F + Send + 'static,
) -> Result<Channel>
where
    F: Future<Output = std::io::Result<TcpStream>> + Send,
{
    crate::seller::tls::ensure_crypto_provider();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());

    let mut config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("rustls protocol versions: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedFingerprintVerifier {
            expected: fingerprint.to_string(),
            provider,
        }))
        .with_no_client_auth();
    // gRPC over HTTP/2: the handshake must negotiate ALPN h2.
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tls = TlsConnector::from(Arc::new(config));

    // Endpoint carries the address for our connector; TLS is done by the connector itself (custom
    // verifier), so we hand tonic the `http` scheme (otherwise tonic would require built-in TLS and
    // refuse). The handover's real scheme is `https`; here we rewrite it only for the tonic Endpoint.
    let uri: Uri = endpoint.parse()?;
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow!("handover endpoint has no host:port: {endpoint}"))?
        .clone();
    let inner_uri = Uri::builder()
        .scheme("http")
        .authority(authority)
        .path_and_query("/")
        .build()?;
    let endpoint_cfg = Endpoint::from(inner_uri);
    let stage = Arc::new(Mutex::new("tcp_connect"));
    let connector_stage = stage.clone();
    let buyer_dial = timeout.is_some();
    let (mut preface, ready) = if buyer_dial {
        let (ready, received) = oneshot::channel();
        (
            Some(ServerPreface {
                server_header: Http2FrameHeader::default(),
                client_ack: ClientSettingsAck::new(),
                ready,
            }),
            Some(received),
        )
    } else {
        (None, None)
    };
    let dial = endpoint_cfg.connect_with_connector(service_fn(move |uri: Uri| {
        let tls = tls.clone();
        let stage = connector_stage.clone();
        let preface = preface.take();
        // A buyer Channel gets one socket. Any new dial belongs to the buyer's retry policy.
        let tcp = (!buyer_dial || preface.is_some()).then(|| {
            connect_tcp(format!(
                "{}:{}",
                uri.host().unwrap_or("127.0.0.1"),
                uri.port_u16().unwrap_or(443)
            ))
        });
        async move {
            let tcp = tcp.ok_or_else(|| {
                DialStageError::new(
                    "tcp_connect",
                    io::Error::new(io::ErrorKind::NotConnected, "buyer gateway channel closed"),
                )
            })?;
            // Tag failures before tonic wraps them, and record the active stage for the
            // deadline around the whole dial.
            *stage.lock().unwrap() = "tcp_connect";
            let tcp = tcp
                .await
                .map_err(|e| DialStageError::new("tcp_connect", e))?;
            *stage.lock().unwrap() = "tls_handshake";
            // The gateway certificate's SAN is fixed (`dexdo`) -- trust comes from fingerprint
            // pinning, not the name, so the handshake name is free to be whatever travels best.

            // A handover endpoint is an ADDRESS far more often than a domain, and RFC 6066
            // forbids sending a literal address in SNI. Sending `dexdo` for such an endpoint is
            // both off-spec and load-bearing in the wrong direction: middleboxes that route or
            // filter on SNI -- VPN proxies above all -- see a name that resolves nowhere and drop
            // the connection. The buyer then reports a bare `transport error` for a gateway that
            // is up and reachable: `openssl s_client` to the same address completes the handshake
            // without SNI and is dropped with `-servername dexdo`.

            // So: an address endpoint dials with `ServerName::IpAddress`, which rustls sends
            // WITHOUT an SNI extension; a real hostname keeps its own name. Pinning is unchanged --
            // the verifier ignores the name and compares the certificate fingerprint.
            // The name presented in the ClientHello. Trust is the pinned fingerprint --
            // `PinnedFingerprintVerifier` ignores the name outright -- so the name has one job:
            // to travel. RFC 6066 forbids a literal address in SNI, and a name that resolves
            // nowhere is what SNI-routing middleboxes (VPN proxies above all) drop, which reaches
            // the operator as a bare `transport error` for a gateway that is up and listening.

            // Taken from the socket we JUST connected on, not from the endpoint spelling: the
            // address is the one fact that is always available and always well-formed by then.
            // That leaves no unparseable case to handle -- no bracketed IPv6, no short-form IPv4
            // the system resolver accepts and `IpAddr` rejects, no fallback name -- and rustls
            // sends `IpAddress` WITHOUT the extension, so no SNI ever leaves this client.
            let peer = tcp
                .peer_addr()
                .map_err(|e| DialStageError::new("tls_handshake", e))?;
            let dns = ServerName::IpAddress(peer.ip().into());
            let stream = tls
                .connect(dns, tcp)
                .await
                .map_err(|e| DialStageError::new("tls_handshake", e))?;
            *stage.lock().unwrap() = "http2_handshake";
            Ok::<_, DialStageError>(TokioIo::new(ServerPrefaceIo { stream, preface }))
        }
    }));
    let dial = async {
        let channel = dial.await?;
        if let Some(ready) = ready {
            ready
                .await
                .map_err(|error| DialStageError::new("http2_handshake", error))?
                .map_err(|error| DialStageError::new("http2_handshake", error))?;
        }
        Ok::<_, anyhow::Error>(channel)
    };
    // Bound the whole dial, including the peer's HTTP/2 preface.
    let channel = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, dial)
            .await
            .map_err(|elapsed| DialStageError::new(*stage.lock().unwrap(), elapsed))??,
        None => dial.await?,
    };
    Ok(channel)
}

#[cfg(test)]
pub(super) fn test_tls_acceptor(tls: &crate::seller::tls::GatewayTls) -> tokio_rustls::TlsAcceptor {
    use tokio_rustls::rustls::{pki_types::pem::PemObject, pki_types::PrivateKeyDer, ServerConfig};

    crate::seller::tls::ensure_crypto_provider();
    let cert = CertificateDer::from_pem_slice(tls.cert_pem.as_bytes()).unwrap();
    let key = PrivateKeyDer::from_pem_slice(tls.key_pem.as_bytes()).unwrap();
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec()];
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

#[cfg(test)]
pub(super) const EMPTY_SERVER_SETTINGS: [u8; 9] = [0, 0, 0, 4, 0, 0, 0, 0, 0];

#[cfg(test)]
pub(super) async fn wait_for_client_settings_ack(stream: &mut (impl AsyncRead + Unpin)) {
    use tokio::io::AsyncReadExt;

    let mut ack = ClientSettingsAck::new();
    let mut bytes = [0; 64];
    while !ack.written {
        let read = stream.read(&mut bytes).await.unwrap();
        assert_ne!(read, 0, "the client closed before sending SETTINGS ACK");
        ack.observe(&bytes[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller::{start_gateway_with_note, UpstreamConfig};
    use dexdo_core::LocalNote;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(start_paused = true)]
    async fn issue_2275_stalled_tcp_dial_times_out_at_tcp_connect() {
        let endpoint = "https://127.0.0.1:1";
        let started = tokio::time::Instant::now();
        let raw = tokio::time::timeout(
            BUYER_GATEWAY_DIAL_TIMEOUT + BUYER_GATEWAY_DIAL_TIMEOUT / 10,
            connect_pinned_with_tcp(
                endpoint,
                &"00".repeat(32),
                Some(BUYER_GATEWAY_DIAL_TIMEOUT),
                |_| std::future::pending(),
            ),
        )
        .await
        .expect("the buyer must time out a pending TCP connection")
        .expect_err("TCP never finished");
        assert!(started.elapsed() >= BUYER_GATEWAY_DIAL_TIMEOUT);
        let structured = gateway_dial_error(endpoint, raw);
        let error = structured.downcast_ref::<dexdo_core::DexdoError>().unwrap();
        assert_eq!(error.code(), "E_GATEWAY_UNREACHABLE");
        assert_eq!(error.stage(), Some("tcp_connect"));
    }

    #[tokio::test]
    async fn issue_2275_stalled_tls_uses_the_remaining_dial_time() {
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", silent.local_addr().unwrap());
        let tcp_delay = BUYER_GATEWAY_DIAL_TIMEOUT / 2;
        let started = tokio::time::Instant::now();
        let raw = tokio::time::timeout(
            BUYER_GATEWAY_DIAL_TIMEOUT + tcp_delay / 2,
            connect_pinned_with_tcp(
                &endpoint,
                &"00".repeat(32),
                Some(BUYER_GATEWAY_DIAL_TIMEOUT),
                move |address| async move {
                    tokio::time::sleep(tcp_delay).await;
                    TcpStream::connect(address).await
                },
            ),
        )
        .await
        .expect("TLS must use the remaining deadline")
        .expect_err("the listener never answers the TLS hello");
        assert!(started.elapsed() >= BUYER_GATEWAY_DIAL_TIMEOUT);
        let structured = gateway_dial_error(&endpoint, raw);
        let error = structured.downcast_ref::<dexdo_core::DexdoError>().unwrap();
        assert_eq!(error.code(), "E_GATEWAY_UNREACHABLE");
        assert_eq!(error.stage(), Some("tls_handshake"));
    }

    #[tokio::test]
    async fn issue_2275_incomplete_server_settings_uses_the_remaining_dial_time() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let tls = crate::seller::tls::GatewayTls::generate().unwrap();
        let acceptor = test_tls_acceptor(&tls);
        let tls_delay = BUYER_GATEWAY_DIAL_TIMEOUT / 2;
        let peer = async {
            let (tcp, _) = listener.accept().await.unwrap();
            tokio::time::sleep(tls_delay).await;
            let mut stream = acceptor.accept(tcp).await.unwrap();
            assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            let mut preface = [0; HTTP2_CLIENT_PREFACE.len()];
            stream.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, HTTP2_CLIENT_PREFACE);
            // Advertise a six-byte SETTINGS payload and leave it incomplete.
            let mut settings = EMPTY_SERVER_SETTINGS;
            settings[2] = 6;
            stream.write_all(&settings).await.unwrap();
            stream.flush().await.unwrap();
            stream
        };
        let started = tokio::time::Instant::now();
        let (dial, _peer) =
            tokio::time::timeout(BUYER_GATEWAY_DIAL_TIMEOUT + tls_delay / 2, async {
                tokio::join!(connect_pinned(&endpoint, &tls.fingerprint), peer)
            })
            .await
            .expect("server SETTINGS must use the remaining deadline");
        let raw = dial.expect_err("incomplete SETTINGS cannot open the channel");
        assert!(started.elapsed() >= BUYER_GATEWAY_DIAL_TIMEOUT);
        let structured = gateway_dial_error(&endpoint, raw);
        let error = structured.downcast_ref::<dexdo_core::DexdoError>().unwrap();
        assert_eq!(error.code(), "E_GATEWAY_UNREACHABLE");
        assert_eq!(error.stage(), Some("http2_handshake"));
    }

    #[tokio::test]
    async fn issue_2275_invalid_server_settings_fail_the_dial() {
        let tls = crate::seller::tls::GatewayTls::generate().unwrap();
        let acceptor = test_tls_acceptor(&tls);
        // A partial setting entry and a complete SETTINGS_MAX_FRAME_SIZE=0 entry.
        for payload in [&[0][..], &[0, 5, 0, 0, 0, 0][..]] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("https://{}", listener.local_addr().unwrap());
            let peer = async {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(tcp).await.unwrap();
                assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                let mut header = EMPTY_SERVER_SETTINGS;
                header[2] = u8::try_from(payload.len()).unwrap();
                stream.write_all(&header).await.unwrap();
                stream.write_all(payload).await.unwrap();
                stream.flush().await.unwrap();
                stream
            };
            let (dial, _peer) = tokio::join!(connect_pinned(&endpoint, &tls.fingerprint), peer);
            let raw = dial.expect_err("invalid server SETTINGS must fail the dial");
            assert!(
                !raw.chain()
                    .any(|error| error.is::<tokio::time::error::Elapsed>()),
                "h2 must reject the frame before the deadline: {raw:#}"
            );
            let structured = gateway_dial_error(&endpoint, raw);
            let error = structured.downcast_ref::<dexdo_core::DexdoError>().unwrap();
            assert_eq!(error.code(), "E_GATEWAY_UNREACHABLE");
            assert_eq!(error.stage(), Some("http2_handshake"));
        }
    }

    #[tokio::test]
    async fn issue_2275_closed_channel_cannot_redial_before_rpc() {
        use dexdo_proto::{ChallengeRequest, GatewayClient};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let tls = crate::seller::tls::GatewayTls::generate().unwrap();
        let acceptor = test_tls_acceptor(&tls);
        let calls = Arc::new(AtomicUsize::new(0));
        let connect_calls = calls.clone();
        let dial = connect_pinned_with_tcp(
            &endpoint,
            &tls.fingerprint,
            Some(BUYER_GATEWAY_DIAL_TIMEOUT),
            move |address| {
                connect_calls.fetch_add(1, Ordering::Relaxed);
                TcpStream::connect(address)
            },
        );
        let (channel, mut peer) = tokio::join!(dial, async {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            stream.write_all(&EMPTY_SERVER_SETTINGS).await.unwrap();
            stream.flush().await.unwrap();
            stream
        });
        let mut client = GatewayClient::new(channel.unwrap());
        peer.shutdown().await.unwrap();
        // Read to EOF before issuing any RPC so Tonic has already noticed the closed socket.
        let _ = tokio::time::timeout(
            BUYER_GATEWAY_DIAL_TIMEOUT,
            peer.read_to_end(&mut Vec::new()),
        )
        .await
        .expect("the h2 connection closes");
        let failure = tokio::time::timeout(
            BUYER_GATEWAY_DIAL_TIMEOUT,
            client.get_challenge(ChallengeRequest::default()),
        )
        .await
        .expect("the RPC must fail without hanging on another connection");
        assert!(failure.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    /// An endpoint spelling that the system resolver accepts and `IpAddr::from_str` rejects must
    /// still dial, because the handshake name is taken from the connected socket rather than from
    /// the spelling.

    /// `127.1` is the short form; it is the spelling that made the endpoint-parsing revision of
    /// this fix hard-fail before any packet left. The assertion is the completed dial through
    /// `connect_pinned` -- the production path -- rather than a `ServerName` the test built itself,
    /// which would hold with the name computation reverted.
    #[tokio::test]
    async fn a_short_form_address_endpoint_still_dials() {
        let seller = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            std::sync::Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let port = seller.listen_addr.port();

        // The contract this row states is "a spelling the SYSTEM RESOLVER accepts and
        // `IpAddr::from_str` rejects", so the spelling has to come from what the running system
        // actually accepts. Measured on `windows-latest`: `getaddrinfo("127.1")` fails with 11001,
        // as do `127.0.1`, `127.000.000.001` and `0x7f000001` -- Windows takes no abbreviated IPv4
        // at all. `localhost` satisfies both halves of the contract on every platform, and `127.1`
        // is kept everywhere it resolves because it is the exact spelling that hard-failed before
        // the fix.
        let spellings: &[&str] = if cfg!(windows) {
            &["localhost"]
        } else {
            &["127.1", "localhost"]
        };
        for host in spellings {
            connect_pinned(&format!("https://{host}:{port}"), &seller.tls_fingerprint)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "the endpoint spelling `{host}` must complete the pinned dial: {error:?}"
                    )
                });
        }

        seller.server_task.abort();
    }

    /// the buyer half of the complaint: a dial that never reaches the seller must name the
    /// address it dialled, say how far it got, and keep the underlying `io` error.

    /// What this replaces is the entire user-visible output of the failure:
    /// `consumer API: upstream open failed; error=transport error`. That string is
    /// `tonic::transport::Error`'s whole `Display`; the address appeared nowhere in the log, not
    /// even under `RUST_LOG=trace`, so a down host, a wrong advertised address and a closed port
    /// were indistinguishable.
    #[tokio::test]
    async fn a_dial_that_cannot_connect_names_the_address_the_stage_and_the_real_cause() {
        // A port that is bound and then released is refused, not filtered: the failure is
        // deterministic and it is the connector's own step that produces it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = listener.local_addr().unwrap();
        drop(listener);

        let raw = connect_pinned(&format!("https://{closed}"), "00".repeat(32).as_str())
            .await
            .expect_err("a released port cannot accept the dial");
        // The precondition for the whole issue: tonic really does collapse this to two words.
        assert_eq!(
            raw.to_string(),
            "transport error",
            "fixture guard: the opaque error this issue is about must still be what tonic returns"
        );

        let structured = gateway_dial_error(&format!("https://{closed}"), raw);
        let rendered = structured.to_string();

        let error = structured
            .downcast_ref::<dexdo_core::DexdoError>()
            .expect("the reported error is the structured one, not the opaque transport error");
        assert_eq!(error.code(), "E_GATEWAY_UNREACHABLE");
        assert_eq!(error.kind(), dexdo_core::ErrorKind::Network);
        // The step that actually failed, tagged by the connector rather than guessed from strings.
        assert_eq!(error.stage(), Some("tcp_connect"));
        // The address is on the headline, which is what was missing entirely.
        assert!(
            rendered
                .lines()
                .next()
                .unwrap()
                .contains(&closed.to_string()),
            "the dialled address must be on the headline: {rendered}"
        );
        // The cause chain survived to the bottom: the operator can tell refused from timed out.
        let refused = error.causes().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
        });
        assert!(refused, "the io cause must survive the render: {rendered}");
        assert!(
            rendered.contains("\n  cause: "),
            "the preserved chain must render: {rendered}"
        );
        assert!(
            rendered.contains("\n  hint: "),
            "the failure must state the fix: {rendered}"
        );
    }

    /// The other half of "distinguish the failure modes": something answering on the address with
    /// a certificate that is not the pinned one is a DIFFERENT problem with a different fix, and
    /// must not be reported as an unreachable host.

    /// The mismatch is produced by a real second gateway with its own TLS identity, so detection
    /// runs through `connect_pinned` and the rustls verifier rather than a hand-built chain.
    #[tokio::test]
    async fn a_foreign_certificate_is_a_wrong_endpoint_not_an_unreachable_one() {
        let seller = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            std::sync::Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let foreign = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            std::sync::Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        assert_ne!(
            seller.tls_fingerprint, foreign.tls_fingerprint,
            "fixture guard: the pin mismatch must be real"
        );

        // Dial the foreign gateway while pinning the seller's fingerprint.
        let endpoint = format!("https://{}", foreign.listen_addr);
        let raw = connect_pinned(&endpoint, &seller.tls_fingerprint)
            .await
            .expect_err("a foreign certificate must not be accepted");
        let structured = gateway_dial_error(&endpoint, raw);
        let error = structured
            .downcast_ref::<dexdo_core::DexdoError>()
            .expect("the reported error is the structured one");
        assert_eq!(error.code(), "E_GATEWAY_WRONG_ENDPOINT");
        assert_eq!(error.kind(), dexdo_core::ErrorKind::Tls);
        assert!(
            error.to_string().contains(&foreign.listen_addr.to_string()),
            "the dialled address must be named: {error}"
        );
    }

    /// The stage is read off a tag the connector attached, not off the rendered text, so a change
    /// to any message cannot silently move a failure into another category.
    #[test]
    fn the_stage_comes_from_the_tag_and_not_from_the_message() {
        let tagged = anyhow::Error::new(DialStageError::new(
            "tls_handshake",
            std::io::Error::other("io: connection reset by peer"),
        ));
        assert_eq!(dial_stage(&tagged), "tls_handshake");
        // A failure after the connector returned its stream is tonic's own h2 handshake.
        assert_eq!(
            dial_stage(&anyhow::anyhow!("tcp_connect refused")),
            "http2_handshake",
            "an untagged error must not be classified by what its text happens to say"
        );
    }

    /// The structured dial report must reach the operator EXACTLY once.

    /// `dial_failure_detail` used to walk the chain a second time on top of `DexdoError`'s own
    /// Display, so every cause was printed twice and a second stage was appended after the hint.
    /// On a pin mismatch the appended stage disagreed with the headline -- `tls_handshake` over
    /// the `tls_certificate_pin` that classifies exactly this failure -- which is the one thing
    /// the structured error exists to prevent.
    #[tokio::test]
    async fn the_structured_dial_report_is_not_rendered_twice() {
        let seller = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            std::sync::Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let foreign = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            std::sync::Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let endpoint = format!("https://{}", foreign.listen_addr);
        let raw = connect_pinned(&endpoint, &seller.tls_fingerprint)
            .await
            .expect_err("a foreign certificate must not be accepted");
        let structured = gateway_dial_error(&endpoint, raw);
        let detail = dial_failure_detail(&structured);

        // The one classification the operator acts on, and it must not be contradicted.
        assert!(
            detail.contains("(stage: tls_certificate_pin)"),
            "the pin classification must survive: {detail}"
        );
        assert!(
            !detail.contains("(stage: tls_handshake)"),
            "the generic transport stage must not overwrite the pin classification: {detail}"
        );
        // Rendered once: the chain the `Display` already walked is not repeated after it.
        assert!(
            !detail.contains("; caused by: "),
            "the cause chain must not be appended a second time: {detail}"
        );
        assert_eq!(
            detail,
            structured.to_string(),
            "an already-structured dial failure is reported verbatim"
        );

        seller.server_task.abort();
        foreign.server_task.abort();
    }

    /// `true` when the ClientHello carries the `server_name` extension (RFC 6066, type 0).

    /// The extension list is walked by type rather than the bytes being searched for a needle: a
    /// hostname can appear inside another extension (ALPN, ECH) and a substring match would call
    /// that SNI.
    fn client_hello_has_sni(flight: &[u8]) -> bool {
        assert_eq!(
            flight[0], 0x16,
            "the first flight must be a handshake record"
        );
        assert_eq!(flight[5], 0x01, "the first message must be a ClientHello");
        // handshake header(4) + legacy_version(2) + random(32), from the record body at offset 5.
        let mut i = 5 + 4 + 2 + 32;
        i += 1 + flight[i] as usize; // legacy_session_id
        i += 2 + u16::from_be_bytes([flight[i], flight[i + 1]]) as usize; // cipher_suites
        i += 1 + flight[i] as usize; // legacy_compression_methods
        let end = i + 2 + u16::from_be_bytes([flight[i], flight[i + 1]]) as usize;
        i += 2;
        while i + 4 <= end {
            let kind = u16::from_be_bytes([flight[i], flight[i + 1]]);
            let len = u16::from_be_bytes([flight[i + 2], flight[i + 3]]) as usize;
            if kind == 0x0000 {
                return true;
            }
            i += 4 + len;
        }
        false
    }

    /// Accept one connection and return the client's first flight. Nothing terminates TLS: the
    /// socket is dropped once the record is read, so the dial fails right after -- which is all
    /// this observer needs, because the ClientHello is already on the wire by then.
    async fn capture_first_flight(listener: tokio::net::TcpListener) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let (mut socket, _) = listener.accept().await.expect("the dial arrives");
        let mut header = [0_u8; 5];
        socket.read_exact(&mut header).await.expect("record header");
        let mut flight = header.to_vec();
        let length = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut body = vec![0_u8; length];
        socket.read_exact(&mut body).await.expect("record body");
        flight.extend_from_slice(&body);
        flight
    }

    /// The one property that distinguishes this fix from what it replaced, asserted where it is
    /// observable: the `server_name` extension is ABSENT from the ClientHello.

    /// Nothing downstream of the handshake can see this. `PinnedFingerprintVerifier` ignores the
    /// server name, so on loopback the constant `dexdo` and the socket-derived address complete
    /// the dial identically -- every other test in this module passes with the name computation
    /// reverted. Only the first flight tells them apart.

    /// The second half is a control on the OBSERVER, not on the buyer: the same rustls stack is
    /// pointed at the same parser with an explicit `DnsName`, so a parser that can only ever
    /// answer "absent" -- or a build that stopped sending extensions at all -- fails here instead
    /// of passing the assertion above for the wrong reason. It cannot be driven through
    /// `connect_pinned`, because this fix deliberately sends no SNI for ANY endpoint spelling.

    /// E2E-ROW: E2E-BUY-10/L0
    #[tokio::test]
    async fn the_client_hello_sends_no_server_name_for_an_address_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the observer");
        let observed = listener.local_addr().expect("observer address");
        let dialled = tokio::spawn(capture_first_flight(listener));

        // The dial cannot complete -- the observer speaks no TLS -- and it does not need to.
        let _ = connect_pinned(&format!("https://{observed}"), &"00".repeat(32)).await;

        let flight = dialled
            .await
            .expect("the observer captured the first flight");
        assert!(
            !client_hello_has_sni(&flight),
            "an address endpoint must send no server_name extension (RFC 6066)"
        );

        // ---- control: the same parser, the same stack, an explicit hostname ----
        let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the control observer");
        let control_addr = control_listener.local_addr().expect("control address");
        let control = tokio::spawn(capture_first_flight(control_listener));

        crate::seller::tls::ensure_crypto_provider();
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("rustls protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedFingerprintVerifier {
                expected: "00".repeat(32),
                provider,
            }))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let tcp = tokio::net::TcpStream::connect(control_addr)
            .await
            .expect("connect the control");
        let named = ServerName::try_from("gateway.example.net").expect("a valid DNS name");
        let _ = TlsConnector::from(Arc::new(config))
            .connect(named, tcp)
            .await;

        let control_flight = control
            .await
            .expect("the control captured its first flight");
        assert!(
            client_hello_has_sni(&control_flight),
            "control: a DnsName must produce a server_name extension, or this parser is blind"
        );
    }
}
