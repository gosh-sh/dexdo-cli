//! Gateway stream-session authorization (§3.1.1, R16/B18).
//!
//! Challenge-response: the gateway issues a `nonce` bound to the `token_contract`; the buyer
//! signs it with the note's private key; the gateway verifies the signature against the buyer's pubkey
//! recorded in the `token_contract` at match time, and only then forwards the stream.

use dexdo_core::note::{verify, NotePubkey, Signature};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// The challenge bytes the buyer signs: a nonce hard-bound to the
/// `token_contract` (§3.1.1) — an intercepted signature cannot be replayed on another deal.
pub fn challenge_bytes(token_contract: &str, nonce: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(token_contract.len() + 1 + nonce.len());
    b.extend_from_slice(token_contract.as_bytes());
    b.push(b'|');
    b.extend_from_slice(nonce);
    b
}

/// Authorization registry: buyer pubkey per contract + issued nonces.
pub struct AuthRegistry {
    /// `token_contract` → buyer pubkey (from the contract at match time, §2.3).
    buyer_pubkeys: Mutex<HashMap<String, NotePubkey>>,
    /// `token_contract` → outstanding issued nonces. A deal/session may have concurrent stream opens.
    issued: Mutex<HashMap<String, HashSet<Vec<u8>>>>,
}

impl AuthRegistry {
    pub fn new() -> Self {
        Self {
            buyer_pubkeys: Mutex::new(HashMap::new()),
            issued: Mutex::new(HashMap::new()),
        }
    }

    /// Register the buyer's pubkey for a contract (the seller learned it from the match).
    pub fn register(&self, token_contract: &str, buyer_pubkey: NotePubkey) {
        self.buyer_pubkeys
            .lock()
            .unwrap()
            .insert(token_contract.to_string(), buyer_pubkey);
    }

    /// Issue a challenge nonce bound to the contract.
    pub fn issue_challenge(&self, token_contract: &str, nonce: Vec<u8>) {
        self.issued
            .lock()
            .unwrap()
            .entry(token_contract.to_string())
            .or_default()
            .insert(nonce);
    }

    /// Verify the buyer's response: the nonce must match the issued one, and the signature must pass
    /// against the recorded buyer pubkey (§3.1.1).
    ///
    /// **Consume-on-success** (review Y1/#1): the nonce is consumed ONLY on a successful signature.
    /// Otherwise anyone who knows the `token_contract` + intercepted the issued nonce could, with a
    /// garbage-signature call, "burn" an honest buyer's challenge (a trivial DoS). This does not open
    /// up replay: a signature over the same nonce is valid exactly until the first success, after which the nonce
    /// is removed and a repeat won't pass the nonce check.
    pub fn verify_response(
        &self,
        token_contract: &str,
        nonce: &[u8],
        signature: &Signature,
    ) -> bool {
        let pubkey = match self.buyer_pubkeys.lock().unwrap().get(token_contract) {
            Some(pk) => pk.clone(),
            None => return false,
        };
        // Confirm this exact nonce is outstanding, but do NOT consume it before verifying the signature.
        match self.issued.lock().unwrap().get(token_contract) {
            Some(outstanding) if outstanding.contains(nonce) => {}
            _ => return false,
        }
        let msg = challenge_bytes(token_contract, nonce);
        if !verify(&pubkey, &msg, signature) {
            // Broken signature: the nonce is NOT touched — the honest buyer will resend a valid response.
            return false;
        }
        // Success: consume this nonce (single-use against replay). If another concurrent verifier already consumed
        // the same nonce, this call fails closed; independent outstanding nonces for the same deal remain valid.
        let mut issued = self.issued.lock().unwrap();
        let empty = {
            let Some(outstanding) = issued.get_mut(token_contract) else {
                return false;
            };
            if !outstanding.remove(nonce) {
                return false;
            }
            outstanding.is_empty()
        };
        if empty {
            issued.remove(token_contract);
        }
        true
    }
}

impl Default for AuthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{LocalNote, Note};

    fn sign(note: &LocalNote, tc: &str, nonce: &[u8]) -> Signature {
        note.sign(&challenge_bytes(tc, nonce))
    }

    /// Y1/#1 (negative/regression): a garbage signature with an intercepted nonce must NOT consume
    /// an honest buyer's challenge (otherwise a trivial DoS).
    #[test]
    fn broken_signature_does_not_burn_honest_challenge() {
        let reg = AuthRegistry::new();
        let buyer = LocalNote::generate();
        reg.register("tc1", buyer.pubkey());
        reg.issue_challenge("tc1", b"nonce-abc".to_vec());

        // The attacker knows the nonce but not the key — sends a signature with someone else's key.
        let attacker = LocalNote::generate();
        let garbage = sign(&attacker, "tc1", b"nonce-abc");
        assert!(
            !reg.verify_response("tc1", b"nonce-abc", &garbage),
            "broken signature rejected"
        );

        // The honest buyer can STILL authorize with the same nonce — the DoS did not succeed.
        let honest = sign(&buyer, "tc1", b"nonce-abc");
        assert!(
            reg.verify_response("tc1", b"nonce-abc", &honest),
            "honest nonce survived the DoS attempt (consume-on-success)"
        );
    }

    /// Replay: a signature consumes the nonce on success → a repeat of the same signature is rejected.
    #[test]
    fn nonce_consumed_on_success_blocks_replay() {
        let reg = AuthRegistry::new();
        let buyer = LocalNote::generate();
        reg.register("tc1", buyer.pubkey());
        reg.issue_challenge("tc1", b"nonce-abc".to_vec());
        let sig = sign(&buyer, "tc1", b"nonce-abc");
        assert!(
            reg.verify_response("tc1", b"nonce-abc", &sig),
            "first time — ok"
        );
        assert!(
            !reg.verify_response("tc1", b"nonce-abc", &sig),
            "replay of the same signature rejected (nonce consumed)"
        );
    }

    /// Issue #243 regression: concurrent local API requests can open more than one stream for the same
    /// deal/session. Their challenges must not overwrite each other before the corresponding responses arrive.
    #[test]
    fn multiple_outstanding_challenges_for_one_deal_all_authorize_once() {
        let reg = AuthRegistry::new();
        let buyer = LocalNote::generate();
        reg.register("tc1", buyer.pubkey());
        reg.issue_challenge("tc1", b"nonce-a".to_vec());
        reg.issue_challenge("tc1", b"nonce-b".to_vec());

        let sig_a = sign(&buyer, "tc1", b"nonce-a");
        let sig_b = sign(&buyer, "tc1", b"nonce-b");
        assert!(
            reg.verify_response("tc1", b"nonce-a", &sig_a),
            "first outstanding challenge still authorizes after a later challenge was issued"
        );
        assert!(
            reg.verify_response("tc1", b"nonce-b", &sig_b),
            "second outstanding challenge authorizes independently"
        );
        assert!(
            !reg.verify_response("tc1", b"nonce-a", &sig_a),
            "each challenge remains single-use"
        );
    }

    /// Replay on a DIFFERENT deal: the challenge is bound to the `token_contract` — an intercepted signature
    /// for tc1 is not valid on tc2, even if the nonce matched (worst case).
    #[test]
    fn signature_cannot_be_replayed_on_another_deal() {
        let reg = AuthRegistry::new();
        let buyer = LocalNote::generate();
        reg.register("tc1", buyer.pubkey());
        reg.register("tc2", buyer.pubkey());
        reg.issue_challenge("tc1", b"nonce-x".to_vec());
        reg.issue_challenge("tc2", b"nonce-x".to_vec());

        let sig_tc1 = sign(&buyer, "tc1", b"nonce-x");
        assert!(
            !reg.verify_response("tc2", b"nonce-x", &sig_tc1),
            "signature over the tc1 challenge is not valid on tc2"
        );
        // But on its own deal — it passes (sanity).
        assert!(
            reg.verify_response("tc1", b"nonce-x", &sig_tc1),
            "on its own deal the signature is valid"
        );
    }
}
