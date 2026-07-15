use super::*;
use gosh_ackinacki::airegistry::calls::encode_internal_payload;
use gosh_ackinacki::airegistry::deploy::DeployMessage;
use gosh_ackinacki::config::AiRegistryConfig;
use gosh_ackinacki::wallet::contracts::{MULTISIG_ABI_JSON, MULTISIG_TVC};
use gosh_ackinacki::wallet::deploy::{prepare_deploy, DeployParams};
use gosh_ackinacki::wallet::giver::GiverClient;

const OPERATIONAL_WALLET_REQ_CONFIRMS: u8 = 1;

impl RealChainBackend {
    /// Provision an operational multisig wallet (1-of-1) for a key: deterministic deploy address
    /// → giver fund → submit → `Active`. Needed to send INTERNAL calls with ECC — e.g.
    /// `fundProbeCommission` requires SHELL in `msg.currencies` (an external message cannot attach currency).
    pub async fn deploy_multisig(&self, keys: &KeyPair) -> Result<Address> {
        let prepared = prepare_operational_multisig_deploy(keys).await?;
        self.fund_deploy_wait(&prepared.address, &prepared.message_boc_b64)
            .await
    }

    /// The **deterministic** address of the owner's operational multisig for `keys` — WITHOUT a deploy
    /// (`prepare_deploy` computes the address from the key+code). D10: one `--note-key` seed controls both the note
    /// and the operational wallet, so the wallet address is derived from the seed — no separate `--wallet-addr` is needed.
    pub fn multisig_address(keys: &KeyPair) -> Result<Address> {
        let params = DeployParams {
            agent_pubkey: keys.public_hex().to_string(),
            controller_pubkey: keys.public_hex().to_string(),
            owner_pubkey: keys.public_hex().to_string(),
            initial_value: 0,
        };
        let prepared = prepare_deploy(&params, keys.secret_hex())?;
        let derived = Address::parse(&prepared.address)?;
        // Directive 17 (#17): accept an explicit operator wallet address via env `DEXDO_WALLET_ADDRESS`,
        // possibly in the GOSH `half1::half2` display form. Normalize through the single
        // `normalize_wallet_address` and fail loud if it disagrees with the seed-derived wallet — one
        // `--note-key` seed controls both the note and the wallet, so a user-supplied address must match.
        if let Ok(env_addr) = std::env::var("DEXDO_WALLET_ADDRESS") {
            let want =
                crate::wallet::normalize_wallet_address(&env_addr).map_err(|e| anyhow!(e))?;
            if want != derived.with_workchain().to_ascii_lowercase() {
                return Err(anyhow!(
                    "DEXDO_WALLET_ADDRESS {want} does not match the seed-derived operator wallet {} \
                     (one --note-key seed controls both the note and the wallet — the addresses must agree)",
                    derived.with_workchain()
                ));
            }
        }
        Ok(derived)
    }

    /// The seller posts the probe-commission (§3.1.2): the wallet sends an INTERNAL `fundProbeCommission()`
    /// to the TC with `shell_ecc` SHELL (ECC[2]). Via `sendTransaction` (NOT `submitTransaction`):
    /// `sendTransaction` works for both the historical 2-of-3 test wallets and the current autonomous
    /// operational wallets; `submitTransaction` would queue forever on the former. Excess SHELL over the
    /// commission is returned.
    pub async fn fund_probe_commission(
        &self,
        wallet: &Address,
        wallet_keys: &KeyPair,
        tc: &Address,
        shell_ecc: u128,
    ) -> Result<Value> {
        let ctx = local_context()?;
        let payload =
            encode_internal_payload(&ctx, TOKENCONTRACT_ABI, "fundProbeCommission", json!({}))
                .await?;
        let mut cc = serde_json::Map::new();
        cc.insert("2".to_string(), json!(shell_ecc.to_string()));
        let msg = encode_external_call(
            &ctx,
            MULTISIG_ABI_JSON,
            &wallet.with_workchain(),
            "sendTransaction",
            json!({
                "dest": tc.with_workchain(),
                "value": "1000000000", // 1 vmshell forward gas
                "cc": Value::Object(cc),
                "bounce": false,
                "flags": 1,
                "payload": payload,
            }),
            wallet_keys.public_hex(),
            wallet_keys.secret_hex(),
        )
        .await?;
        self.send_with_retry(&msg).await
    }

    /// Fund a deploy address from the shellnet giver (test SHELL) — self-provisioning of
    /// deal contracts (directive: "the executor provisions gas/keys ITSELF"). The same giver
    /// (`AiRegistryConfig::shellnet`) and browser-UA path as in wallet self-provisioning.
    pub async fn giver_fund(&self, address: &str, amount: u128) -> Result<()> {
        self.giver_client()?
            .fund_deploy_address(address, amount)
            .await
    }

    /// Send an active account additional **ECC[2] SHELL** from the giver (flag 1). `fund_deploy_address` gives
    /// native gas to an uninit address, but NOT ECC[2]; a wallet that sends SHELL in internal calls
    /// (e.g. `fundProbeCommission`) needs ECC[2] sent separately, after activation.
    pub async fn giver_send_shell(&self, address: &str, amount: u128) -> Result<()> {
        self.giver_client()?.send_shell(address, amount).await
    }

    /// Construct the shellnet giver's `GiverClient` (keys from `AiRegistryConfig::shellnet`),
    /// on top of the backend's browser-UA http client.
    fn giver_client(&self) -> Result<GiverClient> {
        let ctx = local_context()?;
        let cfg = AiRegistryConfig::shellnet();
        Ok(GiverClient::new(
            ctx,
            cfg.giver_address
                .as_deref()
                .ok_or_else(|| anyhow!("no giver_address in config"))?,
            cfg.giver_pubkey
                .as_deref()
                .ok_or_else(|| anyhow!("no giver_pubkey"))?,
            cfg.giver_secret
                .as_deref()
                .ok_or_else(|| anyhow!("no giver_secret"))?,
            self.client.endpoint(),
            self.http.clone(),
        ))
    }

    /// The seller provisions a per-deal `TokenContract`: `build_deploy` (varInit
    /// `{_sellerPubkey,_rootModelAddress,_nonce}` + ctor `{modelName,modelHash,pricePerTick,maxTicks,
    /// sellerNote}`, signed with the note's owner key) → giver-fund the address → submit → wait for `Active`. The address
    /// is deterministic and matches `RootModel.getTokenContractAddress(sellerPubkey,nonce)`; in its ctor the TC
    /// registers itself in RootModel. Returns the address of the active `TokenContract`.
    #[allow(clippy::too_many_arguments)]
    pub async fn deploy_token_contract(
        &self,
        seller: &KeyPair,
        root_model: &Address,
        nonce: u64,
        model_name: &str,
        _tick_size: u128,
        price_per_tick: u128,
        max_ticks: u128,
        seller_note: &Address,
    ) -> Result<Address> {
        let ctx = local_context()?;
        let init_data = json!({
            "_sellerPubkey": format!("0x{}", seller.public_hex()),
            "_rootModelAddress": root_model.with_workchain(),
            "_nonce": nonce.to_string(),
        });
        let ctor = json!({
            "modelName": model_name,
            "modelHash": model_hash_for(model_name),
            "pricePerTick": price_per_tick.to_string(),
            "maxTicks": max_ticks.to_string(),
            "sellerNote": seller_note.with_workchain(),
        });
        let msg = build_deploy(
            &ctx,
            TOKENCONTRACT_ABI,
            TOKENCONTRACT_TVC,
            init_data,
            ctor,
            seller.public_hex(),
            seller.secret_hex(),
        )
        .await?;
        self.fund_deploy_wait(&msg.address, &msg.message_boc_b64)
            .await
    }

    /// Fund a deploy address from the giver, send the deploy message and wait for `Active`.
    /// The common tail of deal-contract provisioning (RootModel/TokenContract).
    async fn fund_deploy_wait(&self, address: &str, message_boc_b64: &str) -> Result<Address> {
        let addr = Address::parse(address)?;
        self.giver_fund(address, 200_000_000_000).await?;
        // Deploy-message send (#65/#68): tolerate the funded-uninit `/v2/account` 404.
        self.send_deploy_with_retry(message_boc_b64).await?;
        for _ in 0..40 {
            if let Some(a) = self.client.get_account(&addr).await? {
                if a.is_active() {
                    return Ok(addr);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Err(anyhow!(
            "deploy {addr} did not activate within the allotted time"
        ))
    }

    /// Operator-path **ECC[2]-funded** deploy (issue #24): funds an uninit deploy address with **ECC[2]
    /// SHELL** from the operator wallet, NOT native gas. This is the fix for the cross-dapp per-deal
    /// `TokenContract`: native funding of an uninit **cross-dapp** address is privileged (only the giver
    /// can — the prior `404`), but ECC[2] (the deal currency the note already moves into escrow, §1) is
    /// permission-free. Mirrors the SDK giver `fund_deploy_address` (`sendCurrencyWithFlag` flag 16 then
    /// 2, attaching `ecc:{2:amount}`) but from the operator multisig via `sendTransaction` carrying
    /// `cc:{2: shell_ecc}` (`sendTransaction`, not `submitTransaction`: the deploy multisig is 2-of-3, so
    /// `submitTransaction` would only queue). Then send the deploy message and wait for `Active`.
    async fn fund_deploy_from_wallet_ecc(
        &self,
        wallet: &Address,
        wallet_keys: &KeyPair,
        address: &str,
        message_boc_b64: &str,
        shell_ecc: u128,
    ) -> Result<Address> {
        let ctx = local_context()?;
        let mut cc = serde_json::Map::new();
        cc.insert("2".to_string(), json!(shell_ecc.to_string()));
        let cc = Value::Object(cc);
        // Mirror the giver `fund_deploy_address`: two ECC[2] sends to the uninit address, flags 16 then 2.
        for flags in [16u8, 2u8] {
            let fund = encode_external_call(
                &ctx,
                MULTISIG_ABI_JSON,
                &wallet.with_workchain(),
                "sendTransaction",
                json!({
                    "dest": address,
                    "value": shell_ecc.to_string(),
                    "cc": cc.clone(),
                    "bounce": false,
                    "flags": flags,
                    "payload": "",
                }),
                wallet_keys.public_hex(),
                wallet_keys.secret_hex(),
            )
            .await?;
            self.send_with_retry(&fund).await?;
        }
        // Deploy-message send (#65/#68): tolerate the funded-uninit `/v2/account` 404.
        self.send_deploy_with_retry(message_boc_b64).await?;
        let addr = Address::parse(address)?;
        for _ in 0..40 {
            if let Some(a) = self.client.get_account(&addr).await? {
                if a.is_active() {
                    return Ok(addr);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Err(anyhow!(
            "deploy {addr} did not activate within the allotted time (ECC-funded)"
        ))
    }

    /// Operator-path `RootModel` deploy (issue #24): same message as [`deploy_root_model`](Self::deploy_root_model)
    /// but funded by the operator multisig (`wallet`), not the giver.
    pub async fn deploy_root_model_from_wallet(
        &self,
        owner: &KeyPair,
        wallet: &Address,
        wallet_keys: &KeyPair,
        gas: u128,
    ) -> Result<Address> {
        let ctx = local_context()?;
        let tc_code = code_boc_b64(TOKENCONTRACT_TVC)?;
        let init_data = json!({
            "_ownerPubkey": format!("0x{}", owner.public_hex()),
            "_superRootAddress": self.superroot.with_workchain(),
        });
        let ctor = json!({ "tokenContractCode": tc_code });
        let msg = build_deploy(
            &ctx,
            ROOTMODEL_ABI,
            ROOTMODEL_TVC,
            init_data,
            ctor,
            owner.public_hex(),
            owner.secret_hex(),
        )
        .await?;
        // #24 (4.0.5): RootModel is a self-dapp contract (`dapp_id == address`); native funding of its
        // uninit cross-dapp address needs the privileged giver (the 404). Fund with ECC[2] SHELL from the
        // operator wallet instead — same as the per-deal TC. `gas` carries the ECC[2] amount here.
        self.fund_deploy_from_wallet_ecc(
            wallet,
            wallet_keys,
            &msg.address,
            &msg.message_boc_b64,
            gas,
        )
        .await
    }

    /// Operator-path per-deal `TokenContract` deploy (issue #24): same message as
    /// [`deploy_token_contract`](Self::deploy_token_contract) but funded by the operator multisig.
    ///
    /// **Known limitation (live-verified):** the per-deal `TokenContract` is a *self-dapp* contract, and
    /// a multisig `sendTransaction` is dapp-bound — it funds same-dapp contracts (e.g. `RootModel`) but
    /// NOT the cross-dapp TC, so this path does not yet activate the TC. The giver works only because it
    /// is privileged (`fund_deploy_address` routes cross-dapp). Operator-funded TC deploy is pending a
    /// cross-dapp funding mechanism; the same funding pattern is otherwise verified by
    /// [`deploy_root_model_from_wallet`](Self::deploy_root_model_from_wallet).
    #[allow(clippy::too_many_arguments)]
    pub async fn deploy_token_contract_from_wallet(
        &self,
        seller: &KeyPair,
        root_model: &Address,
        nonce: u64,
        model_name: &str,
        _tick_size: u128,
        price_per_tick: u128,
        max_ticks: u128,
        seller_note: &Address,
        wallet: &Address,
        wallet_keys: &KeyPair,
        shell_ecc: u128,
    ) -> Result<Address> {
        let ctx = local_context()?;
        let init_data = json!({
            "_sellerPubkey": format!("0x{}", seller.public_hex()),
            "_rootModelAddress": root_model.with_workchain(),
            "_nonce": nonce.to_string(),
        });
        let ctor = json!({
            "modelName": model_name,
            "modelHash": model_hash_for(model_name),
            "pricePerTick": price_per_tick.to_string(),
            "maxTicks": max_ticks.to_string(),
            "sellerNote": seller_note.with_workchain(),
        });
        let msg = build_deploy(
            &ctx,
            TOKENCONTRACT_ABI,
            TOKENCONTRACT_TVC,
            init_data,
            ctor,
            seller.public_hex(),
            seller.secret_hex(),
        )
        .await?;
        // #24 fix (lead): the per-deal TC is cross-dapp — fund its deploy with ECC[2] SHELL from the
        // operator wallet, not native gas (native to an uninit cross-dapp address needs the giver).
        self.fund_deploy_from_wallet_ecc(
            wallet,
            wallet_keys,
            &msg.address,
            &msg.message_boc_b64,
            shell_ecc,
        )
        .await
    }

    /// Operator-path multisig deploy (issue #24): the wallet address is funded by the operator
    /// **externally** (production has no giver), so we just send the prepared deploy message and
    /// wait for `Active` — the pre-funded balance pays. The idempotent caller checks `is_active` first.
    pub async fn deploy_multisig_self_funded(&self, keys: &KeyPair) -> Result<Address> {
        let prepared = prepare_operational_multisig_deploy(keys).await?;
        let addr = Address::parse(&prepared.address)?;
        self.send_deploy_with_retry(&prepared.message_boc_b64)
            .await?;
        for _ in 0..40 {
            if let Some(a) = self.client.get_account(&addr).await? {
                if a.is_active() {
                    return Ok(addr);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        Err(anyhow!(
            "multisig {addr} did not activate — is it funded? (operator funds the wallet externally)"
        ))
    }

    /// The seller (model owner) provisions their `RootModel` under SuperRoot: `build_deploy`
    /// (varInit `{_ownerPubkey,_superRootAddress}` + ctor `{tokenContractCode}`, signed with the owner key)
    /// → giver-fund → submit → `Active`. The address = `getRootModelAddress(ownerPubkey)`; in its ctor the RootModel
    /// registers itself in SuperRoot (`registerRoot`, msg.sender == derived). `tokenContractCode` is the
    /// `TokenContract` code-cell (RootModel verifies its hash against `TOKEN_CONTRACT_CODE_HASH`).
    pub async fn deploy_root_model(&self, owner: &KeyPair) -> Result<Address> {
        let ctx = local_context()?;
        let tc_code = code_boc_b64(TOKENCONTRACT_TVC)?;
        let init_data = json!({
            "_ownerPubkey": format!("0x{}", owner.public_hex()),
            "_superRootAddress": self.superroot.with_workchain(),
        });
        let ctor = json!({ "tokenContractCode": tc_code });
        let msg = build_deploy(
            &ctx,
            ROOTMODEL_ABI,
            ROOTMODEL_TVC,
            init_data,
            ctor,
            owner.public_hex(),
            owner.secret_hex(),
        )
        .await?;
        self.fund_deploy_wait(&msg.address, &msg.message_boc_b64)
            .await
    }
}

/// Test-giver operational wallet deploy: same embedded multisig code/stateInit as the SDK helper,
/// but constructor `reqConfirms=1`, because shellnet onboard forwards vouchers through
/// `submitTransaction` and expects first-signature execution.
async fn prepare_operational_multisig_deploy(keys: &KeyPair) -> Result<DeployMessage> {
    let ctx = local_context()?;
    let owner = format!("0x{}", keys.public_hex());
    build_deploy(
        &ctx,
        MULTISIG_ABI_JSON,
        MULTISIG_TVC,
        json!({}),
        json!({
            "owners_pubkey": [owner.clone(), owner.clone(), owner],
            "owners_address": [],
            "reqConfirms": OPERATIONAL_WALLET_REQ_CONFIRMS,
            "reqConfirmsData": OPERATIONAL_WALLET_REQ_CONFIRMS,
            "value": "0",
        }),
        keys.public_hex(),
        keys.secret_hex(),
    )
    .await
}

#[cfg(test)]
mod operational_multisig_deploy_tests {
    use super::*;

    #[tokio::test]
    async fn operational_multisig_keeps_sdk_deterministic_address() {
        let keys = KeyPair::generate();
        let params = DeployParams {
            agent_pubkey: keys.public_hex().to_string(),
            controller_pubkey: keys.public_hex().to_string(),
            owner_pubkey: keys.public_hex().to_string(),
            initial_value: 0,
        };
        let sdk = prepare_deploy(&params, keys.secret_hex()).expect("sdk deploy");
        let operational = prepare_operational_multisig_deploy(&keys)
            .await
            .expect("operational deploy");

        assert_eq!(operational.address, sdk.address);
        assert!(!operational.message_boc_b64.is_empty());
    }
}
