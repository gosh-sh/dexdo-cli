//! TVM-compatible multisig seed phrase derivation for `dexdo note deploy`.

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

#[cfg(feature = "shellnet")]
pub fn derive_multisig_key_from_seed_phrase(
    phrase: &str,
) -> Result<DerivedMultisigKey, MultisigSeedError> {
    let phrase = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
    let word_count = u8::try_from(phrase.split_whitespace().count()).map_err(|_| {
        MultisigSeedError::UnsupportedDerivation("seed phrase word count exceeds u8".into())
    })?;
    if !matches!(word_count, 12 | 15 | 18 | 21 | 24) {
        return Err(MultisigSeedError::UnsupportedDerivation(format!(
            "TVM SDK supports 12, 15, 18, 21, or 24 words; got {word_count}"
        )));
    }
    let context = std::sync::Arc::new(
        tvm_client::ClientContext::new(tvm_client::ClientConfig::default()).map_err(|_| {
            MultisigSeedError::UnsupportedDerivation(
                "cannot initialize pinned TVM SDK crypto context".into(),
            )
        })?,
    );
    let keys = tvm_client::crypto::mnemonic_derive_sign_keys(
        context,
        tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys {
            phrase,
            path: None,
            dictionary: None,
            word_count: Some(word_count),
        },
    )
    .map_err(|_| MultisigSeedError::InvalidPhrase)?;
    Ok(DerivedMultisigKey {
        public_hex: keys.public.clone(),
        secret_hex: keys.secret.clone(),
    })
}

#[cfg(not(feature = "shellnet"))]
pub fn derive_multisig_key_from_seed_phrase(
    _phrase: &str,
) -> Result<DerivedMultisigKey, MultisigSeedError> {
    Err(MultisigSeedError::UnsupportedDerivation(
        "build dexdo with --features shellnet".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "shellnet")]
    const TVM_TONOS_FIXTURE_WORD_INDICES: [u16; 12] = [
        1636, 1293, 905, 102, 1057, 1956, 1247, 1750, 597, 881, 1302, 3,
    ];

    #[cfg(feature = "shellnet")]
    fn phrase_12() -> String {
        TVM_TONOS_FIXTURE_WORD_INDICES
            .iter()
            .map(|i| bip39::Language::English.wordlist().get_word((*i).into()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(feature = "shellnet")]
    fn phrase_24() -> String {
        bip39::Mnemonic::from_entropy(&[0u8; 32], bip39::Language::English)
            .unwrap()
            .phrase()
            .to_string()
    }

    #[cfg(feature = "shellnet")]
    fn sdk_derive(phrase: &str, word_count: u8) -> tvm_client::crypto::KeyPair {
        let context = std::sync::Arc::new(
            tvm_client::ClientContext::new(tvm_client::ClientConfig::default()).unwrap(),
        );
        tvm_client::crypto::mnemonic_derive_sign_keys(
            context,
            tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys {
                phrase: phrase.to_string(),
                path: None,
                dictionary: None,
                word_count: Some(word_count),
            },
        )
        .unwrap()
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn derivation_matches_sdk_for_12_word_phrase() {
        let phrase = phrase_12();
        let expected = sdk_derive(&phrase, 12);
        let derived = derive_multisig_key_from_seed_phrase(&phrase).unwrap();
        assert_eq!(derived.public_hex(), expected.public);
        assert_eq!(derived.secret_hex(), expected.secret);
        assert_eq!(
            tvm_client::crypto::default_hdkey_derivation_path(),
            TVM_DEFAULT_DERIVATION_PATH
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn derivation_matches_sdk_for_24_word_phrase() {
        let phrase = phrase_24();
        let expected = sdk_derive(&phrase, 24);
        let derived = derive_multisig_key_from_seed_phrase(&phrase).unwrap();
        assert_eq!(derived.public_hex(), expected.public);
        assert_eq!(derived.secret_hex(), expected.secret);
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn unsupported_word_count_is_rejected_precisely() {
        let err = derive_multisig_key_from_seed_phrase(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "unsupported derivation (TVM SDK supports 12, 15, 18, 21, or 24 words; got 13)"
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn seed_phrase_whitespace_is_normalized_without_changing_derivation() {
        let phrase = phrase_12();
        let spaced = phrase.replace(' ', "\n  ");
        assert_eq!(
            derive_multisig_key_from_seed_phrase(&spaced).unwrap(),
            derive_multisig_key_from_seed_phrase(&phrase).unwrap()
        );
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn invalid_seed_phrase_error_does_not_echo_input() {
        let bad = "zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz";
        let err = derive_multisig_key_from_seed_phrase(bad)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "invalid seed phrase");
        assert!(!err.contains(bad));
    }

    #[cfg(feature = "shellnet")]
    #[test]
    fn derived_key_debug_never_leaks_secret() {
        let key = derive_multisig_key_from_seed_phrase(&phrase_12()).unwrap();
        let dbg = format!("{key:?}");
        assert!(dbg.contains(key.public_hex()));
        assert!(!dbg.contains(key.secret_hex()));
    }

    #[cfg(not(feature = "shellnet"))]
    #[test]
    fn seed_derivation_requires_shellnet_sdk_feature() {
        let err = derive_multisig_key_from_seed_phrase("not read")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--features shellnet"));
    }
}
