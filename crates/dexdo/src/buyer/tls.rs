//! Pinning of the gateway's TLS certificate on the buyer side (§3.1.3).
//!
//! There is no PKI. Trust in the gateway's self-signed certificate comes from the **fingerprint**
//! that arrived in the note-encrypted handover (§3.1). The buyer connects over TLS and accepts
//! the connection **only if** the SHA-256 of the presented leaf certificate matches the pinned
//! fingerprint; otherwise it tears down **before** receiving the stream (fail-closed). An active
//! MITM with a foreign certificate is repelled this way.
//!
//! The implementation reuses the rustls stack that tonic already pulls in (tokio-rustls/hyper-util),
//! without a separate TLS stack: a custom `ServerCertVerifier` checks the fingerprint and delegates
//! handshake signature verification to the standard rustls webpki provider.

use anyhow::{anyhow, Result};
use http::Uri;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    verify_tls12_signature, verify_tls13_signature, CryptoProvider,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
};
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

use crate::seller::tls::fingerprint_der;

/// rustls verifier that pins the gateway's certificate fingerprint (§3.1.3).
///
/// `verify_server_cert` accepts the certificate **only** when SHA-256(DER) matches the pinned
/// fingerprint. The TLS handshake signature is verified by the standard webpki provider — so
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
            // Fail-closed: foreign certificate (MITM) — rejection BEFORE receiving any stream.
            Err(RustlsError::General(format!(
                "pinned TLS fingerprint mismatch: expected {}, got {presented}",
                self.expected
            )))
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

/// Open a gRPC channel to the gateway over TLS, pinning the certificate fingerprint from the
/// handover (§3.1.3).
///
/// `endpoint` is `https://host:port` from the decrypted handover; `fingerprint` is the pinned
/// fingerprint from the same place. If the presented certificate does not match, the connection
/// does not come up.
pub async fn connect_pinned(endpoint: &str, fingerprint: &str) -> Result<Channel> {
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
    let channel = endpoint_cfg
        .connect_with_connector(service_fn(move |uri: Uri| {
            let tls = tls.clone();
            async move {
                let host_port = format!(
                    "{}:{}",
                    uri.host().unwrap_or("127.0.0.1"),
                    uri.port_u16().unwrap_or(443)
                );
                let tcp = tokio::net::TcpStream::connect(host_port).await?;
                // The gateway certificate's SAN is fixed (`dexdo`) — trust comes from fingerprint
                // pinning, not the name; we pass a fixed server_name for the SNI handshake.
                let dns = ServerName::try_from("dexdo")
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                let stream = tls.connect(dns, tcp).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(channel)
}
