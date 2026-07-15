//! Handover payload (§3.1, §3.1.3) — a stable format that is encrypted to the
//! buyer's pubkey and placed in the endpoints file (directive 1) / `token_contract`
//! (directive 2). It is the same blob; only the source that fills the file changes.
//!
//! Carries the **gateway endpoint** and the **TLS certificate fingerprint** (§3.1.3): the
//! buyer, after decrypting with the note, pins the fingerprint on the TLS connection — a
//! MITM with a foreign certificate is rejected, because the genuine fingerprint arrived over
//! the channel encrypted to the note.

use serde::{Deserialize, Serialize};

/// Decrypted handover payload. Serialized to JSON, then encrypted to the buyer's
/// pubkey (`Note::encrypt_to`). The format is stable between directives 1 and 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    /// Seller's gateway endpoint (points at the gateway, not the upstream; R15).
    pub endpoint: String,
    /// Fingerprint of the gateway's self-signed TLS certificate: SHA-256 over DER, hex (§3.1.3).
    pub tls_fingerprint: String,
}

impl Handover {
    /// Serialize to bytes for encryption (`encrypt_to`).
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("handover serializes")
    }

    /// Parse decrypted bytes back into the payload.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}
