//! Gateway channel confidentiality: a self-signed TLS certificate brought up
//! at gateway startup. There is no PKI -- trust in the certificate comes from the encrypted handover
//! the **fingerprint** (SHA-256 over DER) is placed next to the endpoint, and the buyer pins it.

//! Here -- only cert+key generation and fingerprint computation. The server side mounts
//! `Identity` into `ServerTlsConfig`; pinning on the buyer's side is in `buyer` (custom verifier).

use anyhow::{Context, Result};
use dexdo_core::Handover;
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

/// The gateway's self-signed certificate + its fingerprint for the handover.
#[derive(Clone)]
pub struct GatewayTls {
    /// Certificate in PEM (for `tonic::Identity`).
    pub cert_pem: String,
    /// Private key in PEM (for `tonic::Identity`).
    pub key_pem: String,
    /// Certificate fingerprint: SHA-256 over DER, hex. Placed into the handover.
    pub fingerprint: String,
}

impl GatewayTls {
    /// Generate the gateway's self-signed certificate. The SAN is fixed (`dexdo`) -- the name
    /// in the TLS check carries no trust (that comes from fingerprint pinning), but rustls requires SNI.
    pub fn generate() -> Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec!["dexdo".to_string()])?;
        let fingerprint = fingerprint_der(ck.cert.der().as_ref());
        Ok(Self {
            cert_pem: ck.cert.pem(),
            key_pem: ck.signing_key.serialize_pem(),
            fingerprint,
        })
    }

    pub fn from_pem_bundle(bundle: String) -> Result<Self> {
        let certificate = CertificateDer::from_pem_slice(bundle.as_bytes())
            .context("gateway TLS bundle has no valid certificate")?;
        PrivateKeyDer::from_pem_slice(bundle.as_bytes())
            .context("gateway TLS bundle has no valid private key")?;
        Ok(Self {
            fingerprint: fingerprint_der(certificate.as_ref()),
            cert_pem: bundle.clone(),
            key_pem: bundle,
        })
    }

    pub fn pem_bundle(&self) -> String {
        format!("{}{}", self.cert_pem, self.key_pem)
    }
}

fn handover_path(cursor: &Path, ciphertext: &[u8]) -> PathBuf {
    cursor.with_extension(format!(
        "{}.handover.json",
        hex::encode(Sha256::digest(ciphertext))
    ))
}

/// A rollback can open a deal with a different certificate. Only trust local handover data
/// whose ciphertext was actually accepted by the contract.
pub fn read_handover(cursor: &Path, ciphertext: &[u8]) -> Result<Option<Handover>> {
    let path = handover_path(cursor, ciphertext);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse seller handover {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read seller handover {}", path.display()))
        }
    }
}

/// Save the exact endpoint and pin before submitting the encrypted handover. Keep the data beside
/// the cursor so an older seller can still read its own state after a rollback.
pub fn persist_handover(cursor: &Path, ciphertext: &[u8], handover: &Handover) -> Result<()> {
    let path = handover_path(cursor, ciphertext);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let write = || -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec(handover)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    };
    write().with_context(|| format!("persist seller handover {}", path.display()))
}

/// Pin the process rustls CryptoProvider (`ring`) once. Both providers (ring and aws-lc-rs)
/// are present in the dependency tree, so rustls does not pick a default on its own and
/// panics; we set it explicitly. Idempotent: a repeat call (or a race) is a no-op.
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
