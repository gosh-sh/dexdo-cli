//! Note trait and crypto handover.
//! In the `LocalNote` implementation is **real local cryptography**:
//! x25519 for `encrypt_to`/`decrypt`, ed25519 for `sign`/verify, ChaCha20-Poly1305 as AEAD.
//! Only the key source is mocked(local generation); the chain-backed Note arrives in.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

/// Note public key(anonymous). Carries the x25519 pubkey(for endpoint encryption)
/// and the ed25519 pubkey(for verifying the challenge signature). In reality this is one note
/// pubkey; here there are two independent pairs bound by a single note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotePubkey {
    /// x25519 public for `encrypt_to`.
    pub x: [u8; 32],
    /// ed25519 verifying key for signature verification.
    pub ed: [u8; 32],
}

/// Signature of an action/challenge with the note's private key(ed25519).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature(pub [u8; 64]);

/// Note crypto operation errors.
#[derive(Debug, thiserror::Error)]
pub enum NoteError {
    /// Decryption failed(wrong key / corrupted ciphertext).
    #[error("decrypt failed")]
    Decrypt,
    /// Malformed ciphertext.
    #[error("malformed ciphertext")]
    Malformed,
    /// Invalid note key.
    #[error("invalid note key")]
    BadKey,
}

/// Note -- a shielded wallet with handover cryptography.
/// `encrypt_to`/`decrypt` -- x25519 + AEAD; `sign`/verify -- ed25519.
/// The key source in is local generation; in it is `gosh.ackinacki`.
pub trait Note: Send + Sync {
    /// Note public key(anonymous).
    fn pubkey(&self) -> NotePubkey;
    /// Encrypt `msg` to the `peer` pubkey.
    fn encrypt_to(&self, peer: &NotePubkey, msg: &[u8]) -> Vec<u8>;
    /// Decrypt ciphertext with this note's private key.
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, NoteError>;
    /// Sign a message with the note's private key.
    fn sign(&self, msg: &[u8]) -> Signature;
}

/// Verify a signature against the note pubkey(ed25519). Gateway side.
pub fn verify(pubkey: &NotePubkey, msg: &[u8], sig: &Signature) -> bool {
    let vk = match VerifyingKey::from_bytes(&pubkey.ed) {
        Ok(vk) => vk,
        Err(_) => return false,
    };
    let s = ed25519_dalek::Signature::from_bytes(&sig.0);
    vk.verify(msg, &s).is_ok()
}

/// Derive the x25519 handover pubkey from the note's ed25519 pubkey: one note key sets
/// both the signature(ed25519) and the handover encryption (x25519 = Montgomery form of ed25519, the
/// birational equivalence Edwards<->Montgomery). This is how the seller **reconstructs the buyer's x25519
/// from on-chain `getBuyerPubkey`(ed25519)** -- the counterparty's pubkey is read from the chain, no
/// separate x25519 is needed. `None` -- an invalid ed25519 point.
pub fn x25519_pub_from_ed25519_pub(ed_pub: &[u8; 32]) -> Option<[u8; 32]> {
    VerifyingKey::from_bytes(ed_pub)
        .ok()
        .map(|vk| vk.to_montgomery().to_bytes())
}

/// Local note with real keys.
/// If the note is built deterministically from a key(`from_seed`/`from_secret_hex`), it carries
/// its `seed` in memory -- this is the **tree root**: child(sub)notes are
/// derived by index via [`LocalNote::derive`]. The ephemeral `generate()` and the child notes
/// themselves carry no seed(flat tree of depth 1). The seed is never written to disk.
pub struct LocalNote {
    x_secret: StaticSecret,
    ed_signing: SigningKey,
    /// Root seed(only on a deterministically built root note) -- for deriving children.
    seed: Option<[u8; 32]>,
}

impl LocalNote {
    /// Generate a new note from the system RNG.: this is an **ephemeral** note --
    /// admissible only as a one-off mock fixture of a single e2e run, not as the identity of production
    /// subcommands(for those use `from_seed`/`from_secret_hex`, with continuity across runs).
    /// An ephemeral note without a seed cannot be a tree root(`derive` returns `None`).
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        let x_secret = StaticSecret::random_from_rng(rng);
        let ed_signing = SigningKey::generate(&mut rng);
        Self {
            x_secret,
            ed_signing,
            seed: None,
        }
    }

    /// Build a note **deterministically** from a 32-byte root secret:
    /// the same key -> the same note(persistent identity), without writing to disk. ed25519 is taken
    /// directly from the seed; x25519 is an HKDF derivative of the seed (domain separation
    /// `dexdo/note/x25519/v1`), so that one root key sets both parts of the note.
    /// This is the **tree root**(HD-style): child(sub)notes are derived from this seed by index
    /// via [`LocalNote::derive`] / enumerated by [`NoteTree`]. The seed is held only in memory.
    /// **NOTE: x25519 here is an HKDF derivative, != [`LocalNote::from_ed25519_signing`]**
    /// (which takes the Montgomery form of the ed25519 scalar). For the same 32 bytes the two paths yield
    /// **different** x25519 -- intentionally: `from_seed` is HD-identity(D7), `from_ed25519_signing` is the
    /// chain-reconstructible `RealNote` handover. Do not mix them.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let ed_signing = SigningKey::from_bytes(seed);
        let hk = Hkdf::<Sha256>::new(None, seed);
        let mut x_bytes = [0u8; 32];
        hk.expand(b"dexdo/note/x25519/v1", &mut x_bytes)
            .expect("HKDF-SHA256 expand of 32 bytes never fails");
        let x_secret = StaticSecret::from(x_bytes);
        Self {
            x_secret,
            ed_signing,
            seed: Some(*seed),
        }
    }

    /// **Deterministic(sub)note derivation by index**. From the root
    /// seed a child seed is derived with domain-separated HKDF (`info = "dexdo/note/derive/v1" ||
    /// index_be`), then the child note is built with the same [`from_seed`]. Guarantees:
    /// - **Reproducibility:** the same `(root_seed, index)` always yields the same note
    /// (pubkey + keys), without any randomness -- restarting with the same key restores
    /// the whole tree(regression test on determinism).
    /// - **Enumerability:** the index is a `u32`; the tree's notes are enumerated by [`NoteTree::nodes`].
    /// `None` for a note without a seed (ephemeral `generate()` or a child note itself -- the tree is
    /// flat, depth 1: the single D7 axis is the index under the root; nested paths are out of scope).
    pub fn derive(&self, index: u32) -> Option<Self> {
        let seed = self.seed.as_ref()?;
        let hk = Hkdf::<Sha256>::new(None, seed);
        let mut info = Vec::with_capacity(DERIVE_INFO.len() + 4);
        info.extend_from_slice(DERIVE_INFO);
        info.extend_from_slice(&index.to_be_bytes());
        let mut child = [0u8; 32];
        hk.expand(&info, &mut child)
            .expect("HKDF-SHA256 expand of 32 bytes never fails");
        // A child note is a leaf: its seed is an HKDF of the root, the root cannot be recovered from it,
        // so the tree stays flat(`derive` does not continue from a child). The root seed lives at the root.
        let mut note = Self::from_seed(&child);
        note.seed = None;
        Some(note)
    }

    /// The note's root seed, if it was built deterministically from a key; `None` for the ephemeral
    /// `generate()` and for child notes. dexdo holds the seed only in memory and never writes it to disk.
    pub fn seed(&self) -> Option<&[u8; 32]> {
        self.seed.as_ref()
    }

    /// Load a note from a hex secret(32 bytes, optional `0x` prefix) -- the `--note-key` key format
    /// . dexdo only **reads** the key, never writes or rotates it: custody of the root
    /// is an external module/wallet. A wrong key -> an explicit error, not silent generation.
    pub fn from_secret_hex(hex: &str) -> Result<Self, NoteError> {
        let s = hex.trim();
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.len() != 64 {
            return Err(NoteError::BadKey);
        }
        let mut seed = [0u8; 32];
        for i in 0..32 {
            seed[i] =
                u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| NoteError::BadKey)?;
        }
        Ok(Self::from_seed(&seed))
    }

    /// A note whose x25519 handover is **derived from ed25519** (the Montgomery form of the signing
    /// scalar), rather than from an independent HKDF/random key. Then
    /// `pubkey().x == x25519_pub_from_ed25519_pub(pubkey().ed)`, and the counterparty reconstructs the
    /// pubkey from on-chain ed25519 -- no separate x25519 channel is needed. Used by `RealNote` (, one
    /// note key on the chain). The x25519 secret = the ed25519 signing scalar(x25519 clamps idempotently).
    /// **NOTE: != [`LocalNote::from_seed`].** `from_seed` derives x25519 via **HKDF**
    /// (the D7 HD-path), this one via the **Montgomery form** of the signing scalar. For the same 32 bytes
    /// they yield **DIFFERENT** x25519 secrets. The derivations are intentionally distinct -- do not mix paths.
    pub fn from_ed25519_signing(ed_signing: SigningKey) -> Self {
        let x_secret = StaticSecret::from(ed_signing.to_scalar_bytes());
        Self {
            x_secret,
            ed_signing,
            seed: None,
        }
    }
}

/// A note tree of a single identity: a root key + **enumerable** child
/// (sub)notes by deterministic index. A single deal/order lives on a specific(sub)note, but
/// all of them are derived from one key -- so the identity = the key AND the whole tree under it, not one note.
/// dexdo only **reads** the root key and derives notes **in memory** as needed
/// (`node`/`nodes`): nothing is written to disk. Enumerating
/// the window `0..width` is enough for acceptance(the routing pool B4/D5 is separate behavior, not active here).
pub struct NoteTree {
    root: LocalNote,
}

impl NoteTree {
    /// Build a tree from a root key(32-byte hex secret `--note-key`). Equivalent to
    /// `LocalNote::from_secret_hex` + child enumeration; a wrong key -> `NoteError::BadKey`.
    pub fn from_secret_hex(hex: &str) -> Result<Self, NoteError> {
        Ok(Self {
            root: LocalNote::from_secret_hex(hex)?,
        })
    }

    /// Build a tree from a ready root note(from `--note-key`/wallet). A note without a seed
    /// (ephemeral) gives a degenerate tree: only the note itself as index 0, no children.
    pub fn new(root: LocalNote) -> Self {
        Self { root }
    }

    /// The tree's root note.
    pub fn root(&self) -> &LocalNote {
        &self.root
    }

    /// (Sub)note by index -- **reproducibly**(the same root+index -> the same note). For a note with
    /// a seed this is `root.derive(index)`; for an ephemeral root without a seed index `0` = the root itself
    /// (no children -- the tree is degenerate). `None` for index > 0 on a seedless root.
    pub fn node(&self, index: u32) -> Option<LocalNote> {
        match self.root.derive(index) {
            Some(n) => Some(n),
            None if index == 0 => Some(LocalNote {
                x_secret: self.root.x_secret.clone(),
                ed_signing: self.root.ed_signing.clone(),
                seed: None,
            }),
            None => None,
        }
    }

    /// Enumerate the first `width`(sub)notes of the tree(`index = 0..width`) -- the window over which
    /// the monitor aggregates the state of the whole identity. Reproducible: repeating with the same key gives the same notes.
    pub fn nodes(&self, width: u32) -> impl Iterator<Item = LocalNote> + '_ {
        (0..width).filter_map(move |i| self.node(i))
    }

    /// Anonymous pubkeys of the first `width`(sub)notes -- the input for the monitor's aggregated snapshot
    /// . "From whom" = the note pubkey.
    pub fn node_pubkeys(&self, width: u32) -> Vec<NotePubkey> {
        self.nodes(width).map(|n| n.pubkey()).collect()
    }
}

// Ciphertext format: ephemeral x25519 pub(32) || nonce(12) || AEAD ciphertext.
const EPK_LEN: usize = 32;
const NONCE_LEN: usize = 12;
/// Handover KDF domain string -- fixes the scheme/version in the key output.
const HANDOVER_INFO: &[u8] = b"dexdo/handover/x25519-chacha20poly1305/v1";
/// Domain string of the(sub)note-by-index derivation KDF -- separates the child
/// seed output from the x25519 derivative(`dexdo/note/x25519/v1`), so the axes do not collide.
const DERIVE_INFO: &[u8] = b"dexdo/note/derive/v1";

/// The canonical handover ECIES-KDF(security review O1): the raw x25519 DH output is non-uniform
/// (lies in a subgroup, has cofactor structure) and is unsuitable as a direct symmetric key.
/// We run `shared` through HKDF-SHA256 binding `epk` and the recipient's pubkey into `info` -- the key
/// is deterministic but bound to this ephemeral pair and recipient. `epk` additionally goes in as
/// AAD under AEAD, so the ephemeral pubkey is authenticated by the tag(tampering -> decryption error).
fn handover_key(shared: &[u8; 32], epk: &[u8; 32], recipient_x_pub: &[u8; 32]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, shared);
    let mut info = Vec::with_capacity(HANDOVER_INFO.len() + EPK_LEN + 32);
    info.extend_from_slice(HANDOVER_INFO);
    info.extend_from_slice(epk);
    info.extend_from_slice(recipient_x_pub);
    let mut okm = [0u8; 32];
    hk.expand(&info, &mut okm)
        .expect("HKDF-SHA256 expand of 32 bytes never fails");
    okm
}

impl Note for LocalNote {
    fn pubkey(&self) -> NotePubkey {
        NotePubkey {
            x: *XPublicKey::from(&self.x_secret).as_bytes(),
            ed: self.ed_signing.verifying_key().to_bytes(),
        }
    }

    fn encrypt_to(&self, peer: &NotePubkey, msg: &[u8]) -> Vec<u8> {
        // Sender's ephemeral x25519 key -> ECDH with the recipient's pubkey -> HKDF -> AEAD key(O1).
        let mut rng = rand::rngs::OsRng;
        let eph_secret = StaticSecret::random_from_rng(rng);
        let eph_pub = XPublicKey::from(&eph_secret);
        let peer_pub = XPublicKey::from(peer.x);
        let shared = eph_secret.diffie_hellman(&peer_pub);

        let epk = *eph_pub.as_bytes();
        let key = handover_key(shared.as_bytes(), &epk, &peer.x);
        let cipher = ChaCha20Poly1305::new((&key).into());
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        // AAD = epk: the ephemeral pubkey is authenticated by the tag.
        let ct = cipher
            .encrypt(nonce, Payload { msg, aad: &epk })
            .expect("aead encrypt is infallible for valid key");

        let mut out = Vec::with_capacity(EPK_LEN + NONCE_LEN + ct.len());
        out.extend_from_slice(&epk);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        out
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, NoteError> {
        if ciphertext.len() < EPK_LEN + NONCE_LEN {
            return Err(NoteError::Malformed);
        }
        let mut epk = [0u8; EPK_LEN];
        epk.copy_from_slice(&ciphertext[..EPK_LEN]);
        let nonce_bytes = &ciphertext[EPK_LEN..EPK_LEN + NONCE_LEN];
        let ct = &ciphertext[EPK_LEN + NONCE_LEN..];

        let eph_pub = XPublicKey::from(epk);
        let shared = self.x_secret.diffie_hellman(&eph_pub);
        // The recipient is us: the same x25519 pubkey the sender bound into the KDF `info`.
        let recipient_x_pub = *XPublicKey::from(&self.x_secret).as_bytes();
        let key = handover_key(shared.as_bytes(), &epk, &recipient_x_pub);
        let cipher = ChaCha20Poly1305::new((&key).into());
        let nonce = Nonce::from_slice(nonce_bytes);
        cipher
            .decrypt(nonce, Payload { msg: ct, aad: &epk })
            .map_err(|_| NoteError::Decrypt)
    }

    fn sign(&self, msg: &[u8]) -> Signature {
        Signature(self.ed_signing.sign(msg).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_seed_is_deterministic_and_distinct() {
        // the same root key -> the same note(persistent identity across runs).
        let seed = [7u8; 32];
        let a = LocalNote::from_seed(&seed);
        let b = LocalNote::from_seed(&seed);
        assert_eq!(a.pubkey(), b.pubkey(), "same seed -> same note");
        assert_ne!(
            a.pubkey(),
            LocalNote::from_seed(&[8u8; 32]).pubkey(),
            "different seed -> different note"
        );
        assert_ne!(
            a.pubkey(),
            LocalNote::generate().pubkey(),
            "ephemeral generate() is almost certainly different"
        );
    }

    /// regression: the two `LocalNote` x25519 derivations are NON-INTEROPERABLE. `from_seed`
    /// (`from_secret_hex`) derives x25519 via **HKDF**(the mock / HD identity); the chain-reconstructible
    /// handover uses the **Montgomery** form (`from_ed25519_signing`, and the seller's
    /// `x25519_pub_from_ed25519_pub` from the on-chain ed25519). For the SAME key they differ -- so a mock/HKDF
    /// note can NEVER decrypt a real seller's ciphertext. The buyer DX
    /// guard(`Buyer::resolve_endpoint`) detects exactly this (`pubkey().x != x25519_pub_from_ed25519_pub(ed)`).
    #[test]
    fn from_seed_x25519_is_not_the_montgomery_handover_form() {
        let seed = [42u8; 32];
        // Mock identity: from_seed -> HKDF x25519.
        let mock = LocalNote::from_seed(&seed);
        let mpk = mock.pubkey();
        let montgomery = x25519_pub_from_ed25519_pub(&mpk.ed).expect("valid ed point");
        assert_ne!(
            mpk.x, montgomery,
            "from_seed (HKDF) x25519 must differ from Montgomery(ed) -- the non-interop invariant ( 'do not mix')"
        );
        // Real handover identity: from_ed25519_signing -> x25519 IS the Montgomery form(the consistent real path).
        let real = LocalNote::from_ed25519_signing(SigningKey::from_bytes(&seed));
        let rpk = real.pubkey();
        assert_eq!(
            rpk.x,
            x25519_pub_from_ed25519_pub(&rpk.ed).expect("valid ed point"),
            "from_ed25519_signing x25519 == Montgomery(ed) -- the chain-reconstructible real handover"
        );
    }

    #[test]
    fn derive_is_deterministic_reproducible_and_distinct_per_index() {
        // note(pubkey+keys), without randomness. Different indices -> different notes; child note != root.
        let root = LocalNote::from_seed(&[7u8; 32]);
        let a0 = root.derive(0).expect("root has seed");
        let b0 = LocalNote::from_seed(&[7u8; 32]).derive(0).expect("seed");
        assert_eq!(a0.pubkey(), b0.pubkey(), "same (root, index) -> same note");
        // Determinism regression: the same index again on the same root.
        assert_eq!(root.derive(0).unwrap().pubkey(), a0.pubkey());
        // Different indices -> different notes.
        let a1 = root.derive(1).unwrap();
        assert_ne!(a0.pubkey(), a1.pubkey(), "different index -> different note");
        // The child note is not equal to the root(derivation, not identity).
        assert_ne!(root.pubkey(), a0.pubkey(), "child note != root");
        // The child note is fully functional(signature + handover) -- it is a real note.
        let sig = a0.sign(b"challenge");
        assert!(verify(&a0.pubkey(), b"challenge", &sig));
        // A different root -> a different tree.
        let other = LocalNote::from_seed(&[8u8; 32]);
        assert_ne!(
            other.derive(0).unwrap().pubkey(),
            a0.pubkey(),
            "different root -> different (sub)note at the same index"
        );
    }

    #[test]
    fn ephemeral_and_child_notes_have_no_seed_so_tree_is_flat() {
        // Derivation is available only from a root with a seed. The ephemeral generate() and the child
        // notes themselves carry no seed -> the tree is flat, depth 1(nested paths are out of scope, not needed for D7).
        assert!(LocalNote::generate().seed().is_none());
        assert!(LocalNote::generate().derive(0).is_none());
        let child = LocalNote::from_seed(&[5u8; 32]).derive(3).unwrap();
        assert!(child.seed().is_none(), "child note carries no seed");
        assert!(
            child.derive(0).is_none(),
            "derivation does not continue from a child"
        );
    }

    #[test]
    fn note_tree_enumerates_distinct_reproducible_nodes() {
        // the tree is enumerable and reproducible. From one key -- several DIFFERENT notes;
        // repeating with the same key gives the same pubkeys(the same tree after a restart).
        let tree = NoteTree::from_secret_hex(&"2a".repeat(32)).unwrap();
        let pks = tree.node_pubkeys(3);
        assert_eq!(pks.len(), 3);
        assert_ne!(pks[0], pks[1]);
        assert_ne!(pks[1], pks[2]);
        assert_ne!(pks[0], pks[2]);
        // node(i) matches the i-th enumerated pubkey.
        assert_eq!(tree.node(1).unwrap().pubkey(), pks[1]);
        // Restart(a new tree from the same key) -> the same notes.
        let again = NoteTree::from_secret_hex(&"2a".repeat(32)).unwrap();
        assert_eq!(again.node_pubkeys(3), pks, "same key -> same tree");
        // The tree root = from_secret_hex(the "root itself" index note differs from the children).
        assert_eq!(
            tree.root().pubkey(),
            LocalNote::from_secret_hex(&"2a".repeat(32))
                .unwrap()
                .pubkey()
        );
    }

    #[test]
    fn ephemeral_tree_is_degenerate_root_only() {
        // Ephemeral root without a seed: index 0 = the root itself, no children(degenerate tree).
        let tree = NoteTree::new(LocalNote::generate());
        let root_pk = tree.root().pubkey();
        assert_eq!(tree.node(0).unwrap().pubkey(), root_pk);
        assert!(tree.node(1).is_none(), "an ephemeral tree has no children");
        assert_eq!(tree.node_pubkeys(5), vec![root_pk]);
    }

    #[test]
    fn from_secret_hex_matches_seed_and_rejects_bad() {
        let hex = "11".repeat(32);
        let from_hex = LocalNote::from_secret_hex(&hex).expect("valid hex");
        assert_eq!(
            from_hex.pubkey(),
            LocalNote::from_seed(&[0x11u8; 32]).pubkey()
        );
        assert_eq!(
            LocalNote::from_secret_hex(&format!("0x{hex}"))
                .unwrap()
                .pubkey(),
            from_hex.pubkey(),
            "the 0x prefix is allowed"
        );
        // A wrong key -> an explicit rejection, not silent generation.
        assert!(matches!(
            LocalNote::from_secret_hex("zz"),
            Err(NoteError::BadKey)
        ));
        assert!(matches!(
            LocalNote::from_secret_hex("1234"),
            Err(NoteError::BadKey)
        ));
        assert!(matches!(
            LocalNote::from_secret_hex(""),
            Err(NoteError::BadKey)
        ));
    }

    #[test]
    fn loaded_note_is_fully_functional() {
        // A persistent note is fully functional: the signature verifies, encryption to it decrypts
        // .
        let n = LocalNote::from_seed(&[3u8; 32]);
        let sig = n.sign(b"challenge");
        assert!(verify(&n.pubkey(), b"challenge", &sig));
        let peer = LocalNote::from_seed(&[4u8; 32]);
        let ct = peer.encrypt_to(&n.pubkey(), b"endpoint|fp");
        assert_eq!(n.decrypt(&ct).unwrap(), b"endpoint|fp");
    }

    #[test]
    fn roundtrip_encrypt_to_recipient() {
        let alice = LocalNote::generate();
        let bob = LocalNote::generate();
        let msg = b"https://gateway.example:8443";
        let ct = alice.encrypt_to(&bob.pubkey(), msg);
        assert_eq!(bob.decrypt(&ct).unwrap(), msg);
    }

    #[test]
    fn wrong_key_cannot_decrypt() {
        let alice = LocalNote::generate();
        let bob = LocalNote::generate();
        let eve = LocalNote::generate();
        let ct = alice.encrypt_to(&bob.pubkey(), b"secret endpoint");
        assert!(eve.decrypt(&ct).is_err());
    }

    /// O1(negative): the ephemeral pubkey `epk` is bound both in the KDF(`info`) and as AAD -- tampering
    /// with any of its bytes breaks decryption(rather than panicking), closing the tamper case under the
    /// "home-grown crypto" red flag.
    #[test]
    fn tampered_epk_is_rejected() {
        let alice = LocalNote::generate();
        let bob = LocalNote::generate();
        let base = alice.encrypt_to(&bob.pubkey(), b"https://gateway.example:8443|fp");
        // Sanity check: clean ciphertext decrypts.
        assert!(bob.decrypt(&base).is_ok());
        // Any corrupted byte of the epk region(first 32) -> a decryption error.
        for i in 0..EPK_LEN {
            let mut ct = base.clone();
            ct[i] ^= 0xFF;
            assert!(
                bob.decrypt(&ct).is_err(),
                "tampering with byte epk[{i}] must be rejected"
            );
        }
    }

    #[test]
    fn sign_and_verify() {
        let n = LocalNote::generate();
        let challenge = b"nonce|token_contract";
        let sig = n.sign(challenge);
        assert!(verify(&n.pubkey(), challenge, &sig));
    }

    #[test]
    fn other_note_signature_rejected() {
        let n = LocalNote::generate();
        let other = LocalNote::generate();
        let challenge = b"nonce|token_contract";
        let sig = other.sign(challenge);
        assert!(!verify(&n.pubkey(), challenge, &sig));
    }
}
