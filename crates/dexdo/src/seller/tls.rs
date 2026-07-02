//! Gateway channel confidentiality: a self-signed TLS certificate brought up
//! at gateway startup. There is no PKI -- trust in the certificate comes from the encrypted handover
//! the **fingerprint**(SHA-256 over DER) is placed next to the endpoint, and the buyer pins it.
//! Here -- only cert+key generation and fingerprint computation. The server side mounts
//! `Identity` into `ServerTlsConfig`; pinning on the buyer's side is in `buyer`(custom verifier).

use anyhow::Result;
use sha2::{Digest, Sha256};

/// The gateway's self-signed certificate + its fingerprint for the handover.
pub struct GatewayTls {
    /// Certificate in PEM(for `tonic::Identity`).
    pub cert_pem: String,
    /// Private key in PEM(for `tonic::Identity`).
    pub key_pem: String,
    /// Certificate fingerprint: SHA-256 over DER, hex. Placed into the handover.
    pub fingerprint: String,
}

impl GatewayTls {
    /// Generate the gateway's self-signed certificate. The SAN is fixed(`dexdo`) -- the name
    /// in the TLS check carries no trust(that comes from fingerprint pinning), but rustls requires SNI.
    pub fn generate() -> Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec!["dexdo".to_string()])?;
        let fingerprint = fingerprint_der(ck.cert.der().as_ref());
        Ok(Self {
            cert_pem: ck.cert.pem(),
            key_pem: ck.signing_key.serialize_pem(),
            fingerprint,
        })
    }
}

/// Pin the process rustls CryptoProvider(`ring`) once. Both providers(ring and aws-lc-rs)
/// are present in the dependency tree, so rustls does not pick a default on its own and
/// panics; we set it explicitly. Idempotent: a repeat call(or a race) is a no-op.
pub fn ensure_crypto_provider() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

/// SHA-256 over the certificate's DER bytes in lowercase hex -- the canonical fingerprint for pinning.
pub fn fingerprint_der(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}
