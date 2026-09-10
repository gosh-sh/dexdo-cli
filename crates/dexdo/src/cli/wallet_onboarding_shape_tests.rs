/// The agreed Acki Nacki wallet shape, enforced case by case.

/// The specification is the answer recorded on (2026-08-12): owners are
/// `[K0, K1, matching_agent_key]`, Vault/Hot transaction confirms `2`/`1`, data confirms `2` on
/// both -- restated for the Vault as "the exact form: exactly three pubkey custodians including
/// the local Vault key, `requiredTxnConfirms=2` and `requiredDataConfirms=2`".

/// Each invariant gets its own case, and each case asserts WHY the wallet was refused, not merely
/// that some error came back: a count check that happened to fail on membership would pass an
/// `is_err()` assertion while leaving the count unenforced. The accepting case is here for the same
/// reason -- a validator that refuses everything satisfies every refusal test.
#[cfg(test)]
mod agreed_wallet_shape_tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;

    /// The two human custodian keys the wallet deploys with, and the agent's own two keys.
    const HUMAN_ONE: char = 'a';
    const HUMAN_TWO: char = 'b';
    const LOCAL_HOT: char = '1';
    const LOCAL_VAULT: char = '2';
    /// A key the operator never agreed to: a fourth custodian, or a set that replaced ours.
    const STRANGER: char = '9';

    fn public(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    #[derive(Clone)]
    struct ShapeWallet {
        custodians: Vec<String>,
        txn_confirms: u8,
        data_confirms: Option<u8>,
        /// Raw override for the custodians getter, for the entries a normal builder cannot express.
        raw_custodians: Option<Value>,
    }

    impl ShapeWallet {
        fn new(local: char, txn_confirms: u8) -> Self {
            Self {
                custodians: vec![
                    public(HUMAN_ONE),
                    public(HUMAN_TWO),
                    public(local),
                ],
                txn_confirms,
                data_confirms: Some(2),
                raw_custodians: None,
            }
        }

        fn omit_data_confirms(mut self) -> Self {
            self.data_confirms = None;
            self
        }

        fn custodians(mut self, keys: &[char]) -> Self {
            self.custodians = keys.iter().copied().map(public).collect();
            self
        }

        fn txn_confirms(mut self, confirms: u8) -> Self {
            self.txn_confirms = confirms;
            self
        }

        fn data_confirms(mut self, confirms: u8) -> Self {
            self.data_confirms = Some(confirms);
            self
        }

        fn raw_custodians(mut self, custodians: Value) -> Self {
            self.raw_custodians = Some(custodians);
            self
        }

        fn custodians_value(&self) -> Value {
            if let Some(raw) = &self.raw_custodians {
                return raw.clone();
            }
            let entries = self
                .custodians
                .iter()
                .enumerate()
                .map(|(index, key)| serde_json::json!({"index": index, "owner_pubkey": format!("0x{key}")}))
                .collect::<Vec<_>>();
            serde_json::json!({"custodians": entries})
        }
    }

    struct ShapeChain {
        wallets: HashMap<String, ShapeWallet>,
    }

    #[async_trait(?Send)]
    impl WalletChainReader for ShapeChain {
        async fn account(&self, address: &Address) -> Result<Option<WalletAccountFact>> {
            Ok(self
                .wallets
                .get(&address.with_workchain())
                .map(|_| WalletAccountFact {
                    status: "Active".to_string(),
                    code_hash: Some(dexdo_core::canonical_multisig::CODE_HASH.to_string()),
                }))
        }

        async fn getter(&self, address: &Address, method: &'static str) -> Result<Option<Value>> {
            let wallet = self
                .wallets
                .get(&address.with_workchain())
                .ok_or_else(|| anyhow!("missing fixture wallet"))?;
            Ok(Some(match method {
                "getVersion" => serde_json::json!({
                    "value0": dexdo_core::canonical_multisig::VERSION,
                    "value1": dexdo_core::canonical_multisig::CONTRACT_NAME,
                }),
                "getCustodians" => wallet.custodians_value(),
                "getParameters" => {
                    let mut parameters =
                        serde_json::json!({"requiredTxnConfirms": wallet.txn_confirms});
                    if let Some(confirms) = wallet.data_confirms {
                        parameters["requiredDataConfirms"] = serde_json::json!(confirms);
                    }
                    parameters
                }
                other => bail!("unexpected getter {other}"),
            }))
        }
    }

    /// One wallet-root address exactly as `parse_scoped_address` yields it for `<id>::<id>`.
    fn scoped(byte: char) -> Value {
        let id = public(byte);
        serde_json::json!({
            "canonical": format!("{id}::{id}"),
            "dapp_id": id,
            "account_address": format!("0:{id}"),
        })
    }

    /// Built through the wire shape rather than through the crate's parser: naming
    /// `dexdo_wallet_onboarding` outside `wallet_onboarding.rs` is what `ci/check_single_sdk.sh`
    /// exists to forbid, and this file is included beside it rather than being it.
    fn response() -> AgentWalletsResponse {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "network": "net-a",
            "vault": scoped('c'),
            "hot": scoped('d'),
        }))
        .expect("the fixture is a canonical wallets response")
    }

    /// The pair exactly as the wallet team agreed to deploy it.
    fn agreed() -> (ShapeWallet, ShapeWallet) {
        (
            ShapeWallet::new(LOCAL_VAULT, 2),
            ShapeWallet::new(LOCAL_HOT, 1),
        )
    }

    fn chain(vault: ShapeWallet, hot: ShapeWallet) -> (ShapeChain, AgentWalletsResponse) {
        let response = response();
        let chain = ShapeChain {
            wallets: HashMap::from([
                (response.vault.account_address.clone(), vault),
                (response.hot.account_address.clone(), hot),
            ]),
        };
        (chain, response)
    }

    async fn refusal(vault: ShapeWallet, hot: ShapeWallet) -> String {
        let (chain, response) = chain(vault, hot);
        let error =
            validate_wallet_pair(&chain, &response, &public(LOCAL_HOT), &public(LOCAL_VAULT))
                .await
                .expect_err("the agreed shape is a money-safety invariant and must be refused");
        format!("{error:#}")
    }

    #[tokio::test]
    async fn the_agreed_pair_is_accepted() {
        let (vault, hot) = agreed();
        let (chain, response) = chain(vault, hot);
        let validated =
            validate_wallet_pair(&chain, &response, &public(LOCAL_HOT), &public(LOCAL_VAULT))
                .await
                .expect("three custodians, 2/1 transaction confirms and 2 data confirms is the agreed pair");
        assert_eq!(validated.hot_scoped_address, response.hot.canonical);
        assert_eq!(validated.vault_scoped_address, response.vault.canonical);
    }

    #[tokio::test]
    async fn two_custodians_are_refused_on_either_half() {
        let (vault, hot) = agreed();
        let error = refusal(
            vault.clone().custodians(&[HUMAN_ONE, LOCAL_VAULT]),
            hot.clone(),
        )
        .await;
        assert!(
            error.contains("Vault") && error.contains("2 pubkey custodians"),
            "{error}"
        );

        let error = refusal(vault, hot.custodians(&[HUMAN_ONE, LOCAL_HOT])).await;
        assert!(
            error.contains("Hot") && error.contains("2 pubkey custodians"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn four_custodians_are_refused_on_either_half() {
        let (vault, hot) = agreed();
        let error = refusal(
            vault
                .clone()
                .custodians(&[HUMAN_ONE, HUMAN_TWO, LOCAL_VAULT, STRANGER]),
            hot.clone(),
        )
        .await;
        assert!(
            error.contains("Vault") && error.contains("4 pubkey custodians"),
            "{error}"
        );

        // The Hot is the half that spends on one signature, so a custodian nobody intended here is
        // a key that can drain it alone. Membership of our own key is satisfied in this case: only
        // the count refuses it.
        let error = refusal(
            vault,
            hot.custodians(&[HUMAN_ONE, HUMAN_TWO, LOCAL_HOT, STRANGER]),
        )
        .await;
        assert!(
            error.contains("Hot") && error.contains("4 pubkey custodians"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_custodian_that_cannot_be_read_is_refused_rather_than_skipped() {
        // Four entries, one of them unreadable. Dropping it silently would leave three and pass.
        let (vault, hot) = agreed();
        let error = refusal(
            vault,
            hot.raw_custodians(serde_json::json!({
                "custodians": [
                    {"index": 0, "owner_pubkey": format!("0x{}", public(HUMAN_ONE))},
                    {"index": 1, "owner_pubkey": format!("0x{}", public(HUMAN_TWO))},
                    {"index": 2, "owner_pubkey": format!("0x{}", public(LOCAL_HOT))},
                    {"index": 3, "note": "no readable owner_pubkey"},
                ],
            })),
        )
        .await;
        assert!(
            error.contains("Hot") && error.contains("owner_pubkey"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_repeated_custodian_key_is_refused() {
        // Three entries, two distinct keys: the agreed set is three DISTINCT custodians, and a
        // duplicate would otherwise let a two-key wallet present itself as a three-key one.
        let (vault, hot) = agreed();
        let error = refusal(
            vault,
            hot.custodians(&[HUMAN_ONE, HUMAN_ONE, LOCAL_HOT]),
        )
        .await;
        assert!(
            error.contains("Hot") && error.contains("more than once"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_custodian_set_without_the_local_key_is_refused() {
        let (vault, hot) = agreed();
        let error = refusal(
            vault.clone().custodians(&[HUMAN_ONE, HUMAN_TWO, STRANGER]),
            hot.clone(),
        )
        .await;
        assert!(
            error.contains("local Vault public key is not in wallet"),
            "{error}"
        );

        let error = refusal(vault, hot.custodians(&[HUMAN_ONE, HUMAN_TWO, STRANGER])).await;
        assert!(
            error.contains("local Hot public key is not in wallet"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_wrong_required_txn_confirms_is_refused_on_either_half() {
        let (vault, hot) = agreed();
        // A Vault that executes on one signature is not custody: the human never confirms.
        let error = refusal(vault.clone().txn_confirms(1), hot.clone()).await;
        assert!(
            error.contains("Vault") && error.contains("1 transaction confirmations"),
            "{error}"
        );

        let error = refusal(vault.clone().txn_confirms(3), hot.clone()).await;
        assert!(
            error.contains("Vault") && error.contains("3 transaction confirmations"),
            "{error}"
        );

        let error = refusal(vault, hot.txn_confirms(2)).await;
        assert!(
            error.contains("Hot") && error.contains("2 transaction confirmations"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_wrong_required_data_confirms_is_refused_on_either_half() {
        // Data confirms guard custodian rotation: at 1, a single custodian can rotate our key out.
        let (vault, hot) = agreed();
        let error = refusal(vault.clone().data_confirms(1), hot.clone()).await;
        assert!(
            error.contains("Vault") && error.contains("1 data confirmations"),
            "{error}"
        );

        let error = refusal(vault, hot.data_confirms(3)).await;
        assert!(
            error.contains("Hot") && error.contains("3 data confirmations"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_wallet_that_does_not_report_data_confirms_is_refused() {
        // Silence is not agreement: a getter that omits the field proves nothing about it.
        let (vault, hot) = agreed();
        let error = refusal(vault, hot.omit_data_confirms()).await;
        assert!(
            error.contains("Hot") && error.contains("requiredDataConfirms"),
            "{error}"
        );
    }

    fn validated() -> ValidatedWalletPair {
        ValidatedWalletPair {
            network: "net-a".to_string(),
            vault_scoped_address: format!("{0}::{0}", public('c')),
            hot_scoped_address: format!("{0}::{0}", public('d')),
        }
    }

    #[test]
    fn the_binding_retains_the_vault_key_file() {
        let validated = validated();
        let hot_key = Path::new("/agent/keys/hot.key");
        let vault_key = Path::new("/agent/keys/vault.key");

        let separate = binding_of(
        crate::cli::wallet::test_network_a(),"binding-id", &validated, hot_key, Some(vault_key), None);
        assert_eq!(separate.hot_key_file.as_deref(), Some(hot_key));
        assert_eq!(
            separate.vault_key_file.as_deref(),
            Some(vault_key),
            "a separately generated --vault-key is the only key that can sign the Vault request"
        );

        // Without --vault-key the Hot key IS the Vault custodian, which is what `run` validates
        // the Vault against, so the binding records that same file rather than nothing.
        let shared = binding_of(
        crate::cli::wallet::test_network_a(),"binding-id", &validated, hot_key, None, None);
        assert_eq!(shared.vault_key_file.as_deref(), Some(hot_key));
    }

    #[test]
    fn the_binding_retains_the_multifactor_wallet_address() {
        let validated = validated();
        let hot_key = Path::new("/agent/keys/hot.key");
        let wallet_address = format!("0:{}", public('e'));

        let binding = binding_of(
            crate::cli::wallet::test_network_a(),
            "binding-id",
            &validated,
            hot_key,
            None,
            Some(wallet_address.as_str()),
        );
        assert_eq!(
            binding.push_profile_address.as_deref(),
            Some(wallet_address.as_str()),
            "the reserved metadata the authenticated hello proved must not be lost on completion"
        );

        // Optional by specification: an onboarding that is otherwise proved is not failed over it,
        // and a blank address is recorded as absent rather than as an empty string.
        for absent in [None, Some(""), Some("   ")] {
            let binding = binding_of(
        crate::cli::wallet::test_network_a(),"binding-id", &validated, hot_key, None, absent);
            assert_eq!(binding.push_profile_address, None, "{absent:?}");
        }
    }
}
