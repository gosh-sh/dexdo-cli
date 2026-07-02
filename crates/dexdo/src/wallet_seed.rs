//! TVM-compatible multisig seed phrase derivation for `dexdo note deploy`.

use bip39::{Language, Mnemonic};
use ed25519_dalek::SigningKey;
use hmac::{Hmac, Mac};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::NonZeroScalar;
use pbkdf2::pbkdf2;
use sha2::Sha512;
use zeroize::Zeroizing;

type HmacSha512 = Hmac<Sha512>;

pub const TVM_DEFAULT_DERIVATION_PATH: &str = "m/44'/1331'/0'/0/0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultisigSeedError {
    InvalidPhrase,
    UnsupportedDerivation(String),
}

impl std::fmt::Display for MultisigSeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPhrase => write!(f, "invalid seed phrase"),
            Self::UnsupportedDerivation(reason) => {
                write!(f, "unsupported derivation ({reason})")
            }
        }
    }
}

impl std::error::Error for MultisigSeedError {}

#[derive(Clone, PartialEq, Eq)]
pub struct DerivedMultisigKey {
    public_hex: String,
    secret_hex: String,
}

impl DerivedMultisigKey {
    pub fn public_hex(&self) -> &str {
        &self.public_hex
    }

    pub fn secret_hex(&self) -> &str {
        &self.secret_hex
    }
}

impl std::fmt::Debug for DerivedMultisigKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DerivedMultisigKey")
            .field("public_hex", &self.public_hex)
            .finish_non_exhaustive()
    }
}

pub fn derive_multisig_key_from_seed_phrase(
    phrase: &str,
) -> Result<DerivedMultisigKey, MultisigSeedError> {
    let phrase = normalize_seed_phrase(phrase);
    if phrase.is_empty() || Mnemonic::validate(&phrase, Language::English).is_err() {
        return Err(MultisigSeedError::InvalidPhrase);
    }
    let node = HdNode::from_bip39_phrase(&phrase)?.derive_path(TVM_DEFAULT_DERIVATION_PATH)?;
    let signing = SigningKey::from_bytes(&node.key);
    Ok(DerivedMultisigKey {
        public_hex: hex::encode(signing.verifying_key().as_bytes()),
        secret_hex: hex::encode(node.key),
    })
}

fn normalize_seed_phrase(phrase: &str) -> Zeroizing<String> {
    Zeroizing::new(phrase.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[derive(Clone)]
struct HdNode {
    key: [u8; 32],
    chain: [u8; 32],
}

impl HdNode {
    fn from_bip39_phrase(phrase: &str) -> Result<Self, MultisigSeedError> {
        let mut seed = Zeroizing::new([0u8; 64]);
        pbkdf2::<HmacSha512>(phrase.as_bytes(), b"mnemonic", 2048, &mut *seed)
            .map_err(|e| MultisigSeedError::UnsupportedDerivation(e.to_string()))?;
        let mut hmac = HmacSha512::new_from_slice(b"Bitcoin seed")
            .map_err(|e| MultisigSeedError::UnsupportedDerivation(e.to_string()))?;
        hmac.update(&*seed);
        let bytes = hmac.finalize().into_bytes();
        let mut key = [0u8; 32];
        let mut chain = [0u8; 32];
        key.copy_from_slice(&bytes[..32]);
        chain.copy_from_slice(&bytes[32..]);
        Ok(Self { key, chain })
    }

    fn derive_path(&self, path: &str) -> Result<Self, MultisigSeedError> {
        let mut child = self.clone();
        for step in path.split('/') {
            if step == "m" {
                continue;
            }
            let hardened = step.ends_with('\'');
            let raw_index = if hardened {
                &step[..step.len().saturating_sub(1)]
            } else {
                step
            };
            let index = raw_index.parse::<u32>().map_err(|_| {
                MultisigSeedError::UnsupportedDerivation(format!("invalid derivation path {path}"))
            })?;
            child = child.derive(index, hardened)?;
        }
        Ok(child)
    }

    fn derive(&self, index: u32, hardened: bool) -> Result<Self, MultisigSeedError> {
        let child_number = if hardened { 0x80000000 | index } else { index }.to_be_bytes();
        let mut hmac = HmacSha512::new_from_slice(&self.chain)
            .map_err(|e| MultisigSeedError::UnsupportedDerivation(e.to_string()))?;
        if hardened {
            hmac.update(&[0]);
            hmac.update(&self.key);
        } else {
            hmac.update(&self.compressed_secp256k1_public()?);
        }
        hmac.update(&child_number);
        let bytes = hmac.finalize().into_bytes();
        let mut chain = [0u8; 32];
        chain.copy_from_slice(&bytes[32..]);

        let child_scalar = NonZeroScalar::try_from(&bytes[..32])
            .map_err(|e| MultisigSeedError::UnsupportedDerivation(e.to_string()))?;
        let parent_scalar = NonZeroScalar::try_from(self.key.as_slice())
            .map_err(|e| MultisigSeedError::UnsupportedDerivation(e.to_string()))?;
        let sum = child_scalar.as_ref() + parent_scalar.as_ref();
        let result_scalar = Option::<NonZeroScalar>::from(NonZeroScalar::new(sum))
            .ok_or_else(|| MultisigSeedError::UnsupportedDerivation("zero child key".into()))?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&result_scalar.to_bytes());
        Ok(Self { key, chain })
    }

    fn compressed_secp256k1_public(&self) -> Result<[u8; 33], MultisigSeedError> {
        let secret = k256::SecretKey::from_slice(&self.key)
            .map_err(|e| MultisigSeedError::UnsupportedDerivation(e.to_string()))?;
        let public = secret.public_key();
        let point = public.to_encoded_point(true);
        let mut bytes = [0u8; 33];
        bytes.copy_from_slice(point.as_bytes());
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TVM_TONOS_FIXTURE_WORD_INDICES: [u16; 12] = [
        1636, 1293, 905, 102, 1057, 1956, 1247, 1750, 597, 881, 1302, 3,
    ];

    fn tonos_fixture_phrase() -> String {
        TVM_TONOS_FIXTURE_WORD_INDICES
            .iter()
            .map(|i| Language::English.wordlist().get_word((*i).into()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn seed_phrase_fixture_derives_well_formed_keypair() {
        assert_eq!(TVM_DEFAULT_DERIVATION_PATH, "m/44'/1331'/0'/0/0");
        let key = derive_multisig_key_from_seed_phrase(&tonos_fixture_phrase()).unwrap();
        assert_eq!(key.public_hex().len(), 64);
        assert_eq!(key.secret_hex().len(), 64);
        hex::decode(key.public_hex()).unwrap();
        hex::decode(key.secret_hex()).unwrap();
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn seed_phrase_fixture_matches_pinned_tvm_sdk_default_derivation() {
        let phrase = tonos_fixture_phrase();
        let context = std::sync::Arc::new(
            tvm_client::ClientContext::new(tvm_client::ClientConfig::default()).unwrap(),
        );
        let sdk_key = tvm_client::crypto::mnemonic_derive_sign_keys(
            context,
            tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys {
                phrase: phrase.clone(),
                path: None,
                dictionary: None,
                word_count: None,
            },
        )
        .unwrap();

        assert_eq!(
            tvm_client::crypto::default_hdkey_derivation_path(),
            TVM_DEFAULT_DERIVATION_PATH
        );
        let key = derive_multisig_key_from_seed_phrase(&phrase).unwrap();
        assert_eq!(key.public_hex(), sdk_key.public);
        assert!(
            key.secret_hex() == sdk_key.secret,
            "seed-file derivation does not match pinned TVM SDK default secret"
        );
    }

    #[test]
    fn seed_phrase_whitespace_is_normalized_without_changing_derivation() {
        let phrase = tonos_fixture_phrase();
        let spaced = phrase.replace(' ', "\n  ");
        let expected = derive_multisig_key_from_seed_phrase(&phrase).unwrap();
        assert_eq!(
            derive_multisig_key_from_seed_phrase(&spaced).unwrap(),
            expected
        );
    }

    #[test]
    fn invalid_seed_phrase_error_does_not_echo_input() {
        let bad = "zzzz zzzz zzzz";
        let err = derive_multisig_key_from_seed_phrase(bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid seed phrase"));
        assert!(!err.contains(bad));
    }

    #[test]
    fn derived_key_debug_never_leaks_secret() {
        let key = derive_multisig_key_from_seed_phrase(&tonos_fixture_phrase()).unwrap();
        let dbg = format!("{key:?}");
        assert!(dbg.contains(key.public_hex()));
        assert!(!dbg.contains(key.secret_hex()));
    }
}
