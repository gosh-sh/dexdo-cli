//! The real chain behind [`VaultChain`].

//! Everything here is a read or a write against the Vault's own canonical multisig contract, using
//! the readers this client already has: `run_getter_retrying` for `getTransactions` and
//! `getParameters`, the ext-out history pager for the finalized queue events, and the same
//! encode/send/observe sequence `note topup` uses for its own `submitTransaction`.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context as _, Result};
use dexdo_core::chain::RetryingReads as _;
use dexdo_core::{Address, CanonicalAddress, KeyPair};
use serde_json::Value;

use super::{
    bounce_argument, send_flag_argument, QueuedTransfer, VaultChain, VaultQueueEvent,
    VaultQueueEventKind,
};
use crate::cli::wallet_funding::{FundingFingerprint, SubmitOutcome};

/// The Vault as this client actually reaches it.
pub(crate) struct RealVaultChain<'a> {
    client: &'a dexdo_core::ChainClient,
    endpoint: String,
    vault: CanonicalAddress,
    vault_address: Address,
    /// The custodian key the agent signs the Vault request with.
    keys: KeyPair,
}

impl<'a> RealVaultChain<'a> {
    pub(crate) fn new(
        client: &'a dexdo_core::ChainClient,
        endpoint: &str,
        vault: CanonicalAddress,
        keys: KeyPair,
    ) -> Result<Self> {
        let vault_address = Address::parse(&vault.legacy())
            .map_err(|e| anyhow!("Vault {vault} is not a chain address: {e:?}"))?;
        Ok(Self {
            client,
            endpoint: endpoint.to_string(),
            vault,
            vault_address,
            keys,
        })
    }

    async fn getter(&self, method: &'static str) -> Result<Value> {
        self.client
            .run_getter_retrying(
                &self.vault_address,
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
                method,
                serde_json::json!({}),
            )
            .await
            .map_err(|e| anyhow!("read Vault {} {method}: {e}", self.vault))?
            .ok_or_else(|| {
                anyhow!(
                    "Vault {} did not answer {method}; it may not be Active",
                    self.vault
                )
            })
    }
}

/// A `uint`-shaped getter output, which the SDK renders as either a JSON number or a string.
fn as_u128(value: &Value, field: &str) -> Result<u128> {
    value
        .as_u64()
        .map(u128::from)
        .or_else(|| value.as_str().and_then(parse_uint))
        .ok_or_else(|| {
            anyhow!("multisig getter field `{field}` is not an unsigned integer: {value}")
        })
}

fn as_u64(value: &Value, field: &str) -> Result<u64> {
    let wide = as_u128(value, field)?;
    u64::try_from(wide).map_err(|_| anyhow!("multisig getter field `{field}` does not fit u64"))
}

/// Decimal, or hex when the SDK rendered the value with an `0x` prefix.
fn parse_uint(text: &str) -> Option<u128> {
    let trimmed = text.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u128::from_str_radix(hex, 16).ok();
    }
    trimmed.parse::<u128>().ok()
}

fn as_string(value: &Value, field: &str) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
        .ok_or_else(|| anyhow!("multisig getter field `{field}` is not a string: {value}"))
}

/// The `cc` map as the queue reports it: currency id -> amount, both as strings or numbers.
fn as_currency_map(value: &Value, field: &str) -> Result<BTreeMap<u32, u128>> {
    let Some(object) = value.as_object() else {
        if value.is_null() {
            return Ok(BTreeMap::new());
        }
        bail!("multisig getter field `{field}` is not a currency map: {value}");
    };
    let mut map = BTreeMap::new();
    for (currency, amount) in object {
        let currency: u32 = currency
            .trim()
            .parse()
            .with_context(|| format!("multisig getter field `{field}` currency id `{currency}`"))?;
        let amount = as_u128(amount, field)?;
        // A zero entry and an absent entry are the same fact about money, and the fingerprint is
        // built from a shortfall map that never carries zeros. Dropping them here keeps the two
        // comparable rather than making a queue entry unrecognisable over a nil currency.
        if amount > 0 {
            map.insert(currency, amount);
        }
    }
    Ok(map)
}

fn queued_transfer_from(entry: &Value) -> Result<QueuedTransfer> {
    let creator_pubkey = entry
        .pointer("/creator/owner_pubkey")
        .filter(|value| !value.is_null())
        .map(|value| as_string(value, "creator.owner_pubkey"))
        .transpose()?;
    Ok(QueuedTransfer {
        id: as_u64(&entry["id"], "id")?,
        creator_pubkey,
        dest: as_string(&entry["dest"], "dest")?,
        value: as_u128(&entry["value"], "value")?,
        cc: as_currency_map(&entry["cc"], "cc")?,
        send_flags: u16::try_from(as_u64(&entry["sendFlags"], "sendFlags")?)
            .map_err(|_| anyhow!("multisig queue sendFlags does not fit u16"))?,
        bounce: entry["bounce"]
            .as_bool()
            .ok_or_else(|| anyhow!("multisig queue entry has no bounce flag"))?,
        dapp_id: as_string(&entry["dapp_id"], "dapp_id")?,
        payload: entry["payload"].as_str().map(str::to_string),
    })
}

fn vault_to_hot_submit_transaction_params(fingerprint: &FundingFingerprint) -> Result<Value> {
    let mut cc = serde_json::Map::new();
    for (currency, amount) in &fingerprint.cc {
        cc.insert(currency.to_string(), serde_json::json!(amount.to_string()));
    }
    // The canonical builder hard-codes `dapp_id = ROOT_PN_DAPP_ID` ("4"), which is right for
    // every dexdo destination and wrong for this one: a Hot is a self-DApp multisig, so the
    // transfer has to be addressed into the Hot's OWN DApp. Forwarding it into DApp 4 would burn
    // the attached vmshell without the money ever reaching the Hot, and the wait would then sit
    // on a balance that is never going to move.
    let mut params = dexdo_core::canonical_multisig::submit_transaction_params(
        dexdo_core::address::to_chain_param(&fingerprint.dest).map_err(anyhow::Error::msg)?,
        fingerprint.value,
        cc,
        bounce_argument(),
        send_flag_argument()?,
        fingerprint.payload_for_wire()?.to_string(),
    );
    params["dapp_id"] =
        serde_json::json!(dexdo_core::address::to_dapp_id_param(&fingerprint.dapp_id));
    Ok(params)
}

#[async_trait::async_trait(?Send)]
impl VaultChain for RealVaultChain<'_> {
    async fn queue(&self) -> Result<Vec<QueuedTransfer>> {
        let output = self.getter("getTransactions").await?;
        let entries = output["transactions"].as_array().ok_or_else(|| {
            anyhow!(
                "Vault {} getTransactions did not return a transactions array: {output}",
                self.vault
            )
        })?;
        entries.iter().map(queued_transfer_from).collect()
    }

    async fn history(&self) -> Result<Vec<VaultQueueEvent>> {
        let http = dexdo_core::chain_http_client()?;
        // this Vault queue reader is NOT on the live seller path, so it is deferred --
        // but the reason recorded here used to be wrong, and the wrong reason is the dangerous part.
        // It said the call site "cannot name a network". It can: `RealVaultChain` has exactly one
        // construction site, wallet_funding.rs:2158, inside `fund_hot_for_money_command`, which
        // already takes `network: &str` and receives the manifest-derived value from both callers.
        // Nothing forbids threading it either -- `ChainRequestCeiling::from_manifest` is the ONE way
        // a ceiling is decided, so using it here would be that same way, not the second way
        // forbids. What is actually true is narrower: this struct does not carry the network
        // today, and giving this provider the backend's own gate is a later batch. So Unlimited is a
        // DEFERRAL stated in the open, not an impossibility -- and mainnet Vault queue history goes
        // out unmetered until that batch lands.
        let records = dexdo_core::chain::read_multisig_queue_history(
            dexdo_core::chain::ChainRequestCeiling::Unlimited,
            &http,
            &self.endpoint,
            self.vault.account_id(),
            self.vault.dapp_id(),
        )
        .await
        .map_err(|e| anyhow!("read Vault {} queue history: {e}", self.vault))?;
        Ok(records
            .into_iter()
            .map(|record| {
                let (kind, transaction_id, dest, value, dapp_id) = match record.event {
                    dexdo_core::chain::MultisigQueueEvent::Submitted {
                        transaction_id,
                        dest,
                        value,
                        dapp_id,
                    } => (
                        VaultQueueEventKind::Submitted,
                        transaction_id,
                        dest,
                        value,
                        dapp_id,
                    ),
                    dexdo_core::chain::MultisigQueueEvent::Sent {
                        transaction_id,
                        dest,
                        value,
                        dapp_id,
                        ..
                    } => (
                        VaultQueueEventKind::Sent,
                        transaction_id,
                        dest,
                        value,
                        dapp_id,
                    ),
                };
                VaultQueueEvent {
                    kind,
                    transaction_id,
                    dest,
                    value,
                    dapp_id,
                    message_id: record.message_id,
                    created_at: record.created_at,
                }
            })
            .collect())
    }

    async fn delivery_message_id(
        &self,
        sent_event_message_id: &str,
        destination: &str,
        destination_dapp_id: &str,
    ) -> Result<Option<String>> {
        // The frozen fingerprint's own destination, parsed by the one address parser this client
        // has. A destination that does not parse is not a destination a receipt can be proven at.
        let destination = CanonicalAddress::parse(destination).map_err(|e| {
            anyhow!("funding destination {destination} is not a chain address: {e}")
        })?;
        let http = dexdo_core::chain_http_client()?;
        dexdo_core::chain::prove_multisig_delivery_message(
            &http,
            &self.endpoint,
            sent_event_message_id,
            destination.account_id(),
            // The DApp the RECORD froze, not the one the address happens to render, so a receipt is
            // only ever accepted in the DApp this generation's transfer was addressed into.
            destination_dapp_id,
        )
        .await
        .map_err(|e| {
            anyhow!(
                "prove the Vault {} -> {destination} delivery behind TransactionSent message \
                 {sent_event_message_id}: {e}",
                self.vault
            )
        })
    }

    async fn submit(&self, fingerprint: &FundingFingerprint) -> Result<SubmitOutcome> {
        use dexdo_core::airegistry::{calls::encode_external_call, deploy::local_context};

        let ctx = local_context()?;
        let params = vault_to_hot_submit_transaction_params(fingerprint)?;

        let boc = encode_external_call(
            &ctx,
            dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
            &self.vault_address.with_workchain(),
            "submitTransaction",
            params,
            self.keys.public_hex(),
            self.keys.secret_hex(),
        )
        .await
        .map_err(|e| {
            anyhow!("encode UpdateCustodianMultisigWallet_v2.submitTransaction -> Hot top-up: {e}")
        })?;

        let http = dexdo_core::chain_http_client()?;
        dexdo_core::chain_clock_skew_preflight(&self.endpoint).await?;
        // Everything from here on is an OUTCOME, never an error: a send whose result cannot be
        // established leaves the journal at `prepared`, and the next run reconciles it against the
        // chain. Turning it into `Err` would lose the distinction between "we know nothing was
        // queued" and "we do not know", and only the first of those may ever be followed by a
        // second transfer.
        if let Err(error) = dexdo_core::ackinacki_wallet::query::send_message_routed(
            &http,
            &self.endpoint,
            &boc,
            self.vault_address.bare(),
            self.vault_address.bare(),
            None,
        )
        .await
        {
            return Ok(SubmitOutcome::Indeterminate {
                reason: format!("the Vault submitTransaction could not be sent: {error}"),
            });
        }

        let receipt = match dexdo_core::chain::observe_note_deploy_wallet_action(
            &http,
            &self.endpoint,
            &boc,
            self.vault_address.bare(),
            self.vault_address.bare(),
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                return Ok(SubmitOutcome::Indeterminate {
                    reason: format!(
                        "the Vault submitTransaction receipt could not be read: {error}"
                    ),
                })
            }
        };
        let Some(receipt) = receipt else {
            return Ok(SubmitOutcome::Indeterminate {
                reason: "the Vault submitTransaction produced no finalized receipt in the receipt \
                         window"
                    .to_string(),
            });
        };
        if receipt.aborted || receipt.action_result_code != 0 {
            return Ok(SubmitOutcome::Indeterminate {
                reason: format!(
                    "the Vault submitTransaction failed (tx {} aborted={} action_result_code={})",
                    receipt.transaction_hash, receipt.aborted, receipt.action_result_code
                ),
            });
        }

        // The queue id is what every later run matches on, so it is read back from the queue rather
        // than assumed: `submitTransaction` returns it on chain, but an external call's receipt does
        // not carry a return value. Failing to read it back is not a failure of the submit - the
        // request is queued either way - so it stays `Accepted` with no id and the next run recovers
        // the id from the wallet's own `TransactionSubmitted`.
        let pending_transaction_id = match self.queue().await {
            Ok(queue) => queue
                .iter()
                .find(|entry| super::queue_entry_matches(entry, fingerprint))
                .map(|entry| entry.id.to_string()),
            Err(_) => None,
        };
        Ok(SubmitOutcome::Accepted {
            transaction_hash: Some(receipt.transaction_hash),
            pending_transaction_id,
        })
    }
}

/// item 1: the same parameters, through the SDK encoder that decides whether they can be sent.
#[cfg(test)]
mod abi_encoding_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::wallet_funding::{payload_hash, VAULT_TO_HOT_PAYLOAD};

    #[test]
    fn vault_submit_params_use_chain_dest_and_keep_hot_dapp() {
        let hot_dapp_id = "a1".repeat(32);
        let hot_account_id = "b2".repeat(32);
        let fingerprint = FundingFingerprint {
            creator: "c3".repeat(32),
            dest: format!("{hot_dapp_id}::{hot_account_id}"),
            dapp_id: hot_dapp_id.clone(),
            value: 10,
            cc: [(2u32, 1_000u128)].into_iter().collect(),
            send_flags: 1,
            bounce: true,
            payload_hash: payload_hash(VAULT_TO_HOT_PAYLOAD),
        };

        let params = vault_to_hot_submit_transaction_params(&fingerprint).unwrap();

        assert_eq!(params["dest"], format!("0:{hot_account_id}"));
        // `0x`-prefixed because the ABI declares `dapp_id` a `uint256` and the SDK reads an
        // unprefixed string as decimal; `abi_encoding_tests` is what proves that through the encoder.
        assert_eq!(params["dapp_id"], format!("0x{hot_dapp_id}"));
    }
}
