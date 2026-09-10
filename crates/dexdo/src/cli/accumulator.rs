//! `dexdo accumulator`: exchange SHELL <-> eccUSDC from the operator multisig.

//! The seller earns SHELL and needs a stablecoin; the operator holds eccUSDC and needs SHELL. Both
//! conversions are the Shell Accumulator's, and both are driven here from the SAME operator multisig
//! `note deploy` and `note topup` spend, taking the SAME funding-wallet turn.

//! What the contract makes this client responsible for, and why each command looks the way it does:

//! * **The sell side has no method.** A lot is created by a bare ECC[2] SHELL transfer into the
//! root's `receive()`, sized to exactly one of four denominations. The contract does not split a
//! larger deposit and does not keep the change, so `sell` decomposes the amount here and sends one
//! message per lot.
//! * **The refusal is expensive.** `_processShellDeposit` validates AFTER `tvm.accept()`, so a
//! wrongly sized deposit is not a cheap bounce. Every figure is checked before anything is sent.
//! * **There is no cancel and no timeout.** Once SHELL is deposited the only exit is to be matched
//! by a buyer and then claimed. That is why `sell` states the whole plan before it moves anything
//! and why the lots it creates are reported so a later run can find them.
//! * **A lot is a durable position.** The run that creates it can die. Lots are therefore recovered
//! from the chain by `lots`, not from any local file: the root derives and publishes every lot
//! address, so `(denomination, order id)` is the whole of a lot's identity.

use crate::cli::args::{
    AccumulatorArgs, AccumulatorBuyArgs, AccumulatorClaimArgs, AccumulatorCommand,
    AccumulatorLotsArgs, AccumulatorSellArgs,
};
use anyhow::Result;

pub(crate) async fn run_accumulator(args: AccumulatorArgs) -> Result<()> {
    match args.command {
        AccumulatorCommand::Status(_) => run_status().await,
        AccumulatorCommand::Sell(args) => run_sell(args).await,
        AccumulatorCommand::Buy(args) => run_buy(args).await,
        AccumulatorCommand::Lots(args) => run_lots(args).await,
        AccumulatorCommand::Claim(args) => run_claim(args).await,
    }
}

mod live {
    use super::*;
    use anyhow::{anyhow, bail};
    use dexdo_core::accumulator::{
        whole_usdc_from_raw, LotDetails, LotId, QueueState, RootDetails, ACCUMULATOR_LOT_ABI,
        ACCUMULATOR_ROOT_ABI,
    };
    use dexdo_core::chain::RetryingReads;
    use dexdo_core::params::{
        ACCUMULATOR_DAPP_ID, ACCUMULATOR_DENOMS, ACCUMULATOR_LOT_VERSION, ACCUMULATOR_ROOT_ADDRESS,
        ACCUMULATOR_ROOT_VERSION, ACCUMULATOR_WALLET_MESSAGE_GAS_RAW,
        GAS_BALANCE_CONFIRM_MAX_READS, GAS_BALANCE_CONFIRM_POLL_INTERVAL,
        NOTE_DEPLOY_SUBMIT_NATIVE_VALUE,
    };
    use dexdo_core::{Address, ChainClient, KeyPair, RealChainBackend};

    /// One resolved on-chain lot: its identity, its address, and whether it can be claimed now.
    pub(super) struct LiveLot {
        pub id: LotId,
        pub address: Address,
        pub details: LotDetails,
        pub sold: bool,
    }

    /// The read side: the accumulator root addressed in ITS OWN DApp, not dexdo's.
    pub(super) struct AccumulatorReader {
        client: ChainClient,
        root: Address,
    }

    impl AccumulatorReader {
        pub(super) fn connect(manifest: &std::path::Path, endpoint: Option<&str>) -> Result<Self> {
            let client = RealChainBackend::connect_accumulator_reader(manifest, endpoint)?;
            let root = Address::parse(ACCUMULATOR_ROOT_ADDRESS)
                .map_err(|e| anyhow!("accumulator root address {ACCUMULATOR_ROOT_ADDRESS}: {e}"))?;
            Ok(Self { client, root })
        }

        pub(super) fn root(&self) -> &Address {
            &self.root
        }

        /// Refuse to read or spend until the address answers as an accumulator root.

        /// Fail-closed identity, not a build pin: the two live roots serve DIFFERENT code hashes
        /// under the SAME version string, so this proves we are talking to an accumulator of the
        /// generation whose ABI we carry - and, just as importantly, that the read path reaches it
        /// at all. A silent `None` here would otherwise read downstream as "no lots exist".
        pub(super) async fn assert_root_identity(&self) -> Result<()> {
            let version = self
                .client
                .run_getter_retrying(&self.root, ACCUMULATOR_ROOT_ABI, "getVersion", json_empty())
                .await
                .map_err(|e| anyhow!("read accumulator root {ACCUMULATOR_ROOT_ADDRESS}: {e}"))?
                .ok_or_else(|| {
                    anyhow!(
                        "accumulator root {ACCUMULATOR_ROOT_ADDRESS} is not Active in DApp {} on \
                         this network, so nothing was read and nothing was submitted",
                        dexdo_core::params::ACCUMULATOR_DAPP_ID
                    )
                })?;
            let (want_version, want_name) = ACCUMULATOR_ROOT_VERSION;
            let got_version = version["value0"].as_str().unwrap_or_default();
            let got_name = version["value1"].as_str().unwrap_or_default();
            if got_version != want_version || got_name != want_name {
                bail!(
                    "refusing to use {ACCUMULATOR_ROOT_ADDRESS}: getVersion() answered \
                     ({got_version:?}, {got_name:?}), expected ({want_version:?}, {want_name:?}). \
                     Nothing was submitted."
                );
            }
            Ok(())
        }

        pub(super) async fn queue(&self, denom: u16) -> Result<QueueState> {
            let raw = self
                .client
                .run_getter_retrying(
                    &self.root,
                    ACCUMULATOR_ROOT_ABI,
                    "getQueueState",
                    serde_json::json!({ "D": denom }),
                )
                .await
                .map_err(|e| anyhow!("read accumulator queue D={denom}: {e}"))?
                .ok_or_else(|| {
                    anyhow!("accumulator root did not answer getQueueState D={denom}")
                })?;
            QueueState::decode_getter(&raw)
                .map_err(|e| anyhow!("decode getQueueState D={denom}: {e}"))
        }

        pub(super) async fn details(&self) -> Result<RootDetails> {
            let raw = self
                .client
                .run_getter_retrying(&self.root, ACCUMULATOR_ROOT_ABI, "getDetails", json_empty())
                .await
                .map_err(|e| anyhow!("read accumulator getDetails: {e}"))?
                .ok_or_else(|| anyhow!("accumulator root did not answer getDetails"))?;
            RootDetails::decode_getter(&raw).map_err(|e| anyhow!("decode getDetails: {e}"))
        }

        /// Resolve a lot's address from the root rather than deriving it locally.
        pub(super) async fn lot_address(&self, id: LotId) -> Result<Address> {
            let raw = self
                .client
                .run_getter_retrying(
                    &self.root,
                    ACCUMULATOR_ROOT_ABI,
                    "getSellOrderAddress",
                    serde_json::json!({ "D": id.denom, "orderId": id.order_id.to_string() }),
                )
                .await
                .map_err(|e| anyhow!("resolve lot address D={} #{}: {e}", id.denom, id.order_id))?
                .ok_or_else(|| anyhow!("accumulator root did not answer getSellOrderAddress"))?;
            let addr = raw["sellOrderAddr"]
                .as_str()
                .ok_or_else(|| anyhow!("getSellOrderAddress returned no sellOrderAddr"))?;
            Address::parse(addr).map_err(|e| anyhow!("lot address {addr}: {e}"))
        }

        /// Read a lot if it still exists. A claimed lot self-destructs, so absence is an answer.
        pub(super) async fn lot_details(&self, address: &Address) -> Result<Option<LotDetails>> {
            let dapp = Address::parse(ACCUMULATOR_DAPP_ID)
                .map_err(|e| anyhow!("accumulator DApp {ACCUMULATOR_DAPP_ID}: {e}"))?;
            let account = self
                .client
                .get_account_in_dapp(address, &dapp)
                .await
                .map_err(|e| anyhow!("read lot account: {e}"))?;
            match account {
                Some(account) if account.is_active() => {}
                _ => return Ok(None),
            }
            let raw = self
                .client
                .run_getter_retrying(address, ACCUMULATOR_LOT_ABI, "getDetails", json_empty())
                .await
                .map_err(|e| anyhow!("read lot getDetails: {e}"))?;
            let Some(raw) = raw else {
                return Ok(None);
            };
            LotDetails::decode_getter(&raw)
                .map(Some)
                .map_err(|e| anyhow!("decode lot getDetails: {e}"))
        }

        /// Confirm a lot really is a `ShellSellOrderLot` before sending it a claim.
        pub(super) async fn assert_lot_identity(&self, address: &Address) -> Result<()> {
            let version = self
                .client
                .run_getter_retrying(address, ACCUMULATOR_LOT_ABI, "getVersion", json_empty())
                .await
                .map_err(|e| anyhow!("read lot getVersion: {e}"))?
                .ok_or_else(|| anyhow!("lot is not Active; nothing was submitted"))?;
            let (want_version, want_name) = ACCUMULATOR_LOT_VERSION;
            let got_version = version["value0"].as_str().unwrap_or_default();
            let got_name = version["value1"].as_str().unwrap_or_default();
            if got_version != want_version || got_name != want_name {
                bail!(
                    "refusing to claim: lot getVersion() answered ({got_version:?}, {got_name:?}), \
                     expected ({want_version:?}, {want_name:?}). Nothing was submitted."
                );
            }
            Ok(())
        }

        /// Every live lot owned by `owner`, oldest first, across all four queues.

        /// Recovered from the chain alone. A claimed lot has self-destructed, so what this finds is
        /// exactly the set that still holds a position - which is what a run that died mid-sell
        /// needs in order to learn what it actually created.
        pub(super) async fn lots_owned_by(
            &self,
            owner: &Address,
            from_order_id: u64,
        ) -> Result<Vec<LiveLot>> {
            let floors = ACCUMULATOR_DENOMS
                .into_iter()
                .map(|denom| (denom, from_order_id))
                .collect();
            self.lots_owned_by_from(owner, &floors).await
        }

        /// Every live lot owned by `owner`, with a separate scan floor for each queue.
        pub(super) async fn lots_owned_by_from(
            &self,
            owner: &Address,
            from_order_id: &std::collections::BTreeMap<u16, u64>,
        ) -> Result<Vec<LiveLot>> {
            let owner_bare = owner.bare().to_string();
            let mut found = Vec::new();
            for denom in ACCUMULATOR_DENOMS {
                let queue = self.queue(denom).await?;
                let floor = scan_floor(from_order_id, denom);
                for order_id in queue.issued_order_ids().skip_while(|id| *id < floor) {
                    let id = LotId { denom, order_id };
                    let address = self.lot_address(id).await?;
                    let Some(details) = self.lot_details(&address).await? else {
                        continue;
                    };
                    if Address::parse(&details.owner)
                        .map(|parsed| parsed.bare() == owner_bare)
                        .unwrap_or(false)
                    {
                        found.push(LiveLot {
                            id,
                            address,
                            sold: queue.is_sold(order_id),
                            details,
                        });
                    }
                }
            }
            Ok(found)
        }

        /// Wait for all lots sent by this run to become visible, bounded by the canonical credit
        /// confirmation budget. The first read happens only after one poll interval because both
        /// queue advancement and lot deployment occur after the wallet action being observed.
        pub(super) async fn await_owned_lots(
            &self,
            owner: &Address,
            floors: &std::collections::BTreeMap<u16, u64>,
            expected: usize,
        ) -> Result<Vec<LiveLot>> {
            let mut latest = Vec::new();
            for _ in 0..GAS_BALANCE_CONFIRM_MAX_READS {
                tokio::time::sleep(GAS_BALANCE_CONFIRM_POLL_INTERVAL).await;
                latest = self.lots_owned_by_from(owner, floors).await?;
                if confirmation_complete(latest.len(), expected) {
                    break;
                }
            }
            Ok(latest)
        }
    }

    fn json_empty() -> serde_json::Value {
        serde_json::json!({})
    }

    pub(super) fn expected_credit_landed(
        before_raw: u128,
        expected_raw: u128,
        now_raw: u128,
    ) -> Result<bool> {
        let target_raw = before_raw
            .checked_add(expected_raw)
            .ok_or_else(|| anyhow!("expected ECC credit overflows uint128 balance"))?;
        Ok(now_raw >= target_raw)
    }

    pub(super) fn scan_floor(floors: &std::collections::BTreeMap<u16, u64>, denom: u16) -> u64 {
        floors.get(&denom).copied().unwrap_or(1)
    }

    pub(super) fn confirmation_complete(found: usize, expected: usize) -> bool {
        found >= expected
    }

    pub(super) fn is_claim_candidate(sold: bool, claimed: bool) -> bool {
        sold && !claimed
    }

    pub(super) fn required_native_gas(message_count: usize) -> Result<u128> {
        ACCUMULATOR_WALLET_MESSAGE_GAS_RAW
            .checked_mul(message_count as u128)
            .ok_or_else(|| anyhow!("accumulator native-gas requirement overflows uint128"))
    }

    pub(super) fn require_native_gas(available_raw: u128, message_count: usize) -> Result<()> {
        let required_raw = required_native_gas(message_count)?;
        if available_raw < required_raw {
            bail!(
                "funding wallet has {available_raw} raw native gas, but all {message_count} outgoing \
                 message(s) require {required_raw} raw native gas; nothing was submitted"
            );
        }
        Ok(())
    }

    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "operation", rename_all = "snake_case")]
    pub(super) enum PendingOperation {
        Buy {
            shell_before_raw: String,
            shell_expected_raw: String,
            usdc_before_raw: String,
            usdc_spent_raw: String,
        },
        Sell {
            floors: std::collections::BTreeMap<u16, u64>,
            denoms: Vec<u16>,
        },
        Claim {
            lots: Vec<(u16, u64)>,
        },
    }

    pub(super) fn persist_operation(
        path: &std::path::Path,
        operation: &PendingOperation,
    ) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(operation)?;
        crate::cli::note::write_private_atomic(path, &bytes).map_err(|e| {
            anyhow!(
                "persist accumulator operation before wallet submit {}: {e}",
                path.display()
            )
        })
    }

    pub(super) fn load_operation(path: &std::path::Path) -> Result<Option<PendingOperation>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| anyhow!("read pending accumulator operation {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!(
                "read pending accumulator operation {}: {e}",
                path.display()
            )),
        }
    }

    /// The operator multisig this run spends from, already resolved and unlocked.
    pub(super) struct Spender {
        pub address: Address,
        pub display: String,
        pub keys: KeyPair,
        pub chain: RealChainBackend,
        pending_path: std::path::PathBuf,
    }

    impl Spender {
        /// Resolve the wallet, take its turn, and connect - in that order.

        /// The lock is taken on the RESOLVED wallet and BEFORE any balance is read, because the
        /// decision to spend is made from that reading. This is the same lock under the same key
        /// that `note deploy` and `note topup` take, not a second one beside it.
        pub(super) async fn open(
            manifest: &std::path::Path,
            endpoint: Option<&str>,
            multisig_address: Option<&str>,
            multisig_private_key: &Option<std::path::PathBuf>,
            multisig_seed_file: &Option<std::path::PathBuf>,
        ) -> Result<(Self, crate::cli::note_cmd::FundingWalletLock)> {
            let manifest_str = manifest
                .to_str()
                .ok_or_else(|| anyhow!("DEXDO_MANIFEST: non-printable path"))?
                .to_string();
            let network = dexdo_core::Deployed::load(manifest)
                .map_err(|e| anyhow!("manifest {}: {e}", manifest.display()))?
                .network;
            let wallet_network = crate::cli::wallet::WalletNetwork::from_manifest_label(&network)?;
            let wallet = crate::cli::wallet::resolve_funding_wallet(
                &crate::cli::wallet::WalletStore::open()?,
                &wallet_network,
                multisig_address,
                multisig_private_key,
                multisig_seed_file,
            )?;
            let address = dexdo_core::address::parse_chain_address(&wallet.address)
                .map_err(|e| anyhow!("--multisig-address {}: {e}", wallet.address))?;
            let address = address.into_chain();
            let display = dexdo_core::address::display_self_dapp(&address.with_workchain());

            let lock =
                crate::cli::note_cmd::acquire_funding_wallet_lock(&network, &wallet.address)?;
            let pending_path =
                crate::cli::note_cmd::funding_wallet_lock_path(&network, &wallet.address)?
                    .with_extension("accumulator-pending.json");

            let (source, secret_hex) =
                crate::cli::commands::multisig_secret_hex(&wallet.key, &wallet.seed_file)?;
            let keys = KeyPair::from_secret_hex(secret_hex.trim())
                .map_err(|e| anyhow!("{source} (SDK secret hex): {e:?}"))?;
            let chain = RealChainBackend::connect_with_endpoint(&manifest_str, endpoint)?;
            Ok((
                Self {
                    address,
                    display,
                    keys,
                    chain,
                    pending_path,
                },
                lock,
            ))
        }

        pub(super) async fn native_balance(&self) -> Result<u128> {
            let account = self
                .chain
                .client()
                .get_account_retrying(&self.address)
                .await
                .map_err(|e| anyhow!("read funding wallet {}: {e}", self.display))?
                .ok_or_else(|| anyhow!("funding wallet {} is missing", self.display))?;
            Ok(account.balance)
        }

        pub(super) fn persist_pending(&self, operation: &PendingOperation) -> Result<()> {
            persist_operation(&self.pending_path, operation)
        }

        pub(super) fn load_pending(&self) -> Result<Option<PendingOperation>> {
            load_operation(&self.pending_path)
        }

        pub(super) fn clear_pending(&self) -> Result<()> {
            match std::fs::remove_file(&self.pending_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(anyhow!(
                    "clear completed accumulator operation {}: {e}",
                    self.pending_path.display()
                )),
            }
        }

        /// Raw balance of one ECC currency on the funding wallet, read by fact.
        pub(super) async fn ecc_balance(&self, currency: u32) -> Result<u128> {
            let account = self
                .chain
                .client()
                .get_account_retrying(&self.address)
                .await
                .map_err(|e| anyhow!("read funding wallet {}: {e}", self.display))?;
            Ok(account
                .as_ref()
                .map(|account| account.ecc_balance(currency))
                .unwrap_or_default())
        }

        /// Wait, bounded, for an ECC credit to actually arrive, and answer by fact.

        /// The wallet's own action phase succeeding is NOT the arrival. Both directions are paid by
        /// a SEPARATE internal message the accumulator sends back (`buyer.transfer(...)` in
        /// `_processUsdcDeposit`, `seller.transfer(...)` in `claimUSDC`), which lands a block or
        /// more after the submit returns. Reading the balance straight afterwards therefore reports
        /// a delta of zero on a perfectly good exchange - a receipt that is worse than none,
        /// because it says the money did not arrive when it simply had not arrived YET.

        /// `None` means only "not observed within the read budget", never "lost". The caller turns
        /// that into a refusal rather than a cheerful zero, because an unverified money movement is
        /// exactly what this client must not report as done.

        /// The budget is the one this client already uses to confirm a credit landing on this same
        /// wallet ([`GAS_BALANCE_CONFIRM_MAX_READS`]), not a new timer.
        pub(super) async fn await_ecc_credit(
            &self,
            currency: u32,
            before_raw: u128,
            expected_raw: u128,
        ) -> Result<Option<u128>> {
            for _ in 0..GAS_BALANCE_CONFIRM_MAX_READS {
                tokio::time::sleep(GAS_BALANCE_CONFIRM_POLL_INTERVAL).await;
                let now_raw = self.ecc_balance(currency).await?;
                if expected_credit_landed(before_raw, expected_raw, now_raw)? {
                    return Ok(Some(now_raw));
                }
            }
            Ok(None)
        }

        /// Send one ECC amount to `dest` with an empty body.

        /// An empty payload is what makes this a plain currency transfer into the accumulator's
        /// `receive()`, which is the only entry point the sell side has and the safer of the two the
        /// buy side has. `flag: 1` pays the message fees from the wallet rather than out of the
        /// amount, so the figure that arrives is the figure the plan promised - flag 16 would
        /// convert the ECC into the destination's native gas instead. `bounce: true` because the
        /// message carries money: on any refusal it comes home rather than resting somewhere that
        /// did not take it.
        pub(super) async fn send_ecc(
            &self,
            dest: &Address,
            currency: u32,
            raw: u128,
            dapp_id: &str,
        ) -> Result<()> {
            use dexdo_core::airegistry::{calls::encode_external_call, deploy::local_context};

            let ctx = local_context()?;
            let mut cc = serde_json::Map::new();
            cc.insert(currency.to_string(), serde_json::json!(raw.to_string()));
            let boc = encode_external_call(
                &ctx,
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
                &self.address.with_workchain(),
                "submitTransaction",
                dexdo_core::canonical_multisig::submit_transaction_params_in_dapp(
                    dest.with_workchain(),
                    NOTE_DEPLOY_SUBMIT_NATIVE_VALUE,
                    cc,
                    true,
                    1,
                    String::new(),
                    dapp_id,
                ),
                self.keys.public_hex(),
                self.keys.secret_hex(),
            )
            .await?;
            self.submit(&boc).await
        }

        /// Call a method on `dest` from the wallet, attaching no ECC.
        pub(super) async fn call_method(
            &self,
            dest: &Address,
            abi: &str,
            method: &str,
            args: serde_json::Value,
            dapp_id: &str,
        ) -> Result<()> {
            use dexdo_core::airegistry::{
                calls::{encode_external_call, encode_internal_payload},
                deploy::local_context,
            };

            let ctx = local_context()?;
            let payload = encode_internal_payload(&ctx, abi, method, args).await?;
            let boc = encode_external_call(
                &ctx,
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
                &self.address.with_workchain(),
                "submitTransaction",
                dexdo_core::canonical_multisig::submit_transaction_params_in_dapp(
                    dest.with_workchain(),
                    NOTE_DEPLOY_SUBMIT_NATIVE_VALUE,
                    serde_json::Map::new(),
                    true,
                    1,
                    payload,
                    dapp_id,
                ),
                self.keys.public_hex(),
                self.keys.secret_hex(),
            )
            .await?;
            self.submit(&boc).await
        }

        async fn submit(&self, boc: &str) -> Result<()> {
            let endpoint = self.chain.client().endpoint();
            let http = dexdo_core::chain_http_client()?;
            dexdo_core::chain_clock_skew_preflight(endpoint).await?;
            dexdo_core::ackinacki_wallet::query::send_message_routed(
                &http,
                endpoint,
                boc,
                self.address.bare(),
                self.address.bare(),
                None,
            )
            .await?;
            // The receipt read is best-effort, and deliberately so.

            // `observe_note_deploy_wallet_action` carries a note-deploy contract: it requires the
            // wallet's LATEST transaction to be the one it just observed, and errors with "stale or
            // advanced" otherwise. That holds for a deploy, which performs exactly one wallet action
            // per run. It does NOT hold here: `sell` sends one message per lot, so by the time a
            // later message is observed the wallet has moved on, and the check fails on a transfer
            // that succeeded. Measured live on the chain: `buy` and `claim` both reported failure
            // AFTER the money had moved and the queues had advanced.

            // Failing a money command that in fact succeeded is its own hazard - it invites the
            // operator to send again. So a receipt that CANNOT be identified is inconclusive rather
            // than fatal, and every caller proves the outcome by fact instead: `sell` re-reads the
            // lots it created, `buy` and `claim` wait for the credit. A receipt that IS identified
            // and shows a refusal still fails hard, because that is real evidence.
            match dexdo_core::chain::observe_note_deploy_wallet_action(
                &http,
                endpoint,
                boc,
                self.address.bare(),
                self.address.bare(),
            )
            .await
            {
                Ok(Some(receipt)) if receipt.aborted || receipt.action_result_code != 0 => {
                    bail!(
                        "funding wallet {} refused the transfer: aborted={}, action_result_code={} \
                         (37 = native shortfall, 38 = ECC shortfall). Transaction {}.",
                        self.display,
                        receipt.aborted,
                        receipt.action_result_code,
                        receipt.transaction_hash
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    println!(
                        "  note the wallet receipt for this message could not be identified ({error}); \
                         the outcome is confirmed from chain state below, not from this read."
                    );
                }
            }
            Ok(())
        }
    }

    /// Resolve a durable pre-submit intent from chain state before permitting another spend.
    pub(super) async fn reconcile_pending(
        reader: &AccumulatorReader,
        spender: &Spender,
    ) -> Result<bool> {
        let Some(pending) = spender.load_pending()? else {
            return Ok(false);
        };
        let complete = match &pending {
            PendingOperation::Buy {
                shell_before_raw,
                shell_expected_raw,
                usdc_before_raw,
                usdc_spent_raw,
            } => {
                let before_raw = shell_before_raw.parse::<u128>().map_err(|e| {
                    anyhow!("pending accumulator buy has invalid SHELL baseline: {e}")
                })?;
                let expected_raw = shell_expected_raw.parse::<u128>().map_err(|e| {
                    anyhow!("pending accumulator buy has invalid expected SHELL credit: {e}")
                })?;
                let usdc_before = usdc_before_raw.parse::<u128>().map_err(|e| {
                    anyhow!("pending accumulator buy has invalid eccUSDC baseline: {e}")
                })?;
                let usdc_spent = usdc_spent_raw.parse::<u128>().map_err(|e| {
                    anyhow!("pending accumulator buy has invalid eccUSDC spend: {e}")
                })?;
                let usdc_after = spender
                    .ecc_balance(dexdo_core::params::USDC_CURRENCY_ID)
                    .await?;
                expected_credit_landed(
                    before_raw,
                    expected_raw,
                    spender
                        .ecc_balance(dexdo_core::params::SHELL_CURRENCY_ID)
                        .await?,
                )? && usdc_before
                    .checked_sub(usdc_spent)
                    .is_some_and(|target| usdc_after <= target)
            }
            PendingOperation::Sell { floors, denoms } => {
                let found = reader.lots_owned_by_from(&spender.address, floors).await?;
                ACCUMULATOR_DENOMS.into_iter().all(|denom| {
                    let expected = denoms.iter().filter(|item| **item == denom).count();
                    found.iter().filter(|lot| lot.id.denom == denom).count() >= expected
                })
            }
            PendingOperation::Claim { lots } => {
                let mut complete = true;
                for (denom, order_id) in lots {
                    let address = reader
                        .lot_address(LotId {
                            denom: *denom,
                            order_id: *order_id,
                        })
                        .await?;
                    if reader
                        .lot_details(&address)
                        .await?
                        .is_some_and(|details| !details.claimed)
                    {
                        complete = false;
                    }
                }
                complete
            }
        };
        if complete {
            spender.clear_pending()?;
            println!(
                "accumulator: the pending operation is already complete on chain; no message was submitted"
            );
            return Ok(true);
        }
        bail!(
            "a durable accumulator operation is still pending and is not yet complete on chain; \
             refusing to submit it again because the previous message may already be in flight"
        )
    }

    /// Render one amount of SHELL. The raw figure is gone from the line: it said the same thing in
    /// units nobody spends, and `shell_amount` loses nothing -- `1.5 SHELL` is exactly what
    /// `whole_shell_from_raw` used to print as `1`.
    pub(super) fn shell_units(raw: u128) -> String {
        format!("{} SHELL", dexdo_core::shell_amount(raw))
    }

    /// Render one whole-unit line describing an amount of eccUSDC.
    pub(super) fn usdc_units(raw: u128) -> String {
        format!("{} eccUSDC ({raw} raw ECC[3])", whole_usdc_from_raw(raw))
    }
}

use dexdo_core::accumulator::{BuyPlan, SellPlan};
use dexdo_core::params::{
    ACCUMULATOR_DAPP_ID, ACCUMULATOR_DENOMS, ACCUMULATOR_SHELL_PER_USDC_RAW,
    GAS_BALANCE_CONFIRM_MAX_READS, SHELL_CURRENCY_ID, USDC_CURRENCY_ID, USDC_UNIT,
};
use live::*;

async fn run_status() -> Result<()> {
    let reader = AccumulatorReader::connect(&crate::cli::commands::manifest_path()?, None)?;
    reader.assert_root_identity().await?;
    let details = reader.details().await?;

    println!(
        "accumulator {} (DApp {})",
        reader.root().with_workchain(),
        ACCUMULATOR_DAPP_ID
    );
    println!(
        "  rate            1 eccUSDC = {} (fixed, no fee, both directions)",
        shell_units(ACCUMULATOR_SHELL_PER_USDC_RAW)
    );
    println!(
        "  seller pool     {}",
        shell_units(details.seller_shell_pool_raw)
    );
    println!("  usdc balance    {}", usdc_units(details.usdc_balance_raw));
    println!("  owed to sellers {}", usdc_units(details.owed_total_raw));
    println!(
        "  free reserve    {}",
        usdc_units(details.free_reserve_raw())
    );
    for denom in ACCUMULATOR_DENOMS {
        let queue = reader.queue(denom).await?;
        println!(
            "  queue {denom:>4} eccUSDC  resting={} sold={} unclaimed={} next_order_id={}",
            queue.available, queue.sold_prefix, queue.owed_count, queue.next_id
        );
    }
    Ok(())
}

async fn run_sell(args: AccumulatorSellArgs) -> Result<()> {
    let reader = AccumulatorReader::connect(&crate::cli::commands::manifest_path()?, None)?;
    reader.assert_root_identity().await?;

    let (spender, _lock) = Spender::open(
        &crate::cli::commands::manifest_path()?,
        None,
        args.multisig_address.as_deref(),
        &args.multisig_private_key,
        &args.multisig_seed_file,
    )
    .await?;
    if reconcile_pending(&reader, &spender).await? {
        return Ok(());
    }

    let available_raw = spender.ecc_balance(SHELL_CURRENCY_ID).await?;
    let plan = match args.usdc {
        Some(usdc) => SellPlan::for_whole_usdc(u128::from(usdc)),
        None => SellPlan::for_available_shell(available_raw),
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    plan.require_funded(available_raw)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // What will move, in whole units, before anything moves.
    println!(
        "accumulator sell: wallet {} converts {} into {} across {} lot(s)",
        spender.display,
        shell_units(plan.shell_committed_raw),
        usdc_units(plan.usdc_expected_raw),
        plan.lot_count()
    );
    for (denom, count) in plan.denomination_counts() {
        println!(
            "  {count} x {denom} eccUSDC lot  = {}",
            shell_units(u128::from(denom) * ACCUMULATOR_SHELL_PER_USDC_RAW * count as u128)
        );
    }
    // Two different reasons SHELL can be left behind, and calling them the same thing would be a
    // lie on a money path: dust that no lot size can express, versus a balance this run simply was
    // not asked to convert.
    let left = plan.shell_left_raw(available_raw);
    if left > 0 {
        let still_convertible = dexdo_core::accumulator::max_whole_usdc_from_shell(left);
        if still_convertible == 0 {
            println!(
                "  remainder      {} stays in the wallet: it is below one {} eccUSDC lot, and the \
                 accumulator accepts only exact lot sizes.",
                shell_units(left),
                ACCUMULATOR_DENOMS[ACCUMULATOR_DENOMS.len() - 1]
            );
        } else {
            println!(
                "  not converted  {} stays in the wallet: this run was asked for {} eccUSDC, and \
                 that balance would still cover {} more. `--all` converts everything that divides \
                 into whole lots.",
                shell_units(left),
                plan.usdc_whole,
                still_convertible
            );
        }
    }
    println!(
        "  NOTE a lot cannot be cancelled or withdrawn. Its eccUSDC is released only once a buyer \
         matches it, and then only by `dexdo accumulator claim`."
    );

    // Record where each queue stood BEFORE sending, so a run that dies here still leaves the reader
    // a bounded place to look: the lots this run creates are the ones from these ids upward.
    let mut first_unseen = std::collections::BTreeMap::new();
    for denom in ACCUMULATOR_DENOMS {
        first_unseen.insert(denom, reader.queue(denom).await?.next_id);
    }
    for (denom, next_id) in &first_unseen {
        println!("  queue {denom} eccUSDC: this run's lots start at order id {next_id}");
    }

    let native_raw = spender.native_balance().await?;
    require_native_gas(native_raw, plan.lot_count())?;
    // `require_native_gas` above answers "can this run's lots be sent", from
    // `ACCUMULATOR_WALLET_MESSAGE_GAS_RAW` -- one observed message. It does NOT answer "will this
    // wallet still be able to act afterwards", and that is the question a sell has to ask, because
    // a sell is the command that converts a wallet's spendable SHELL away. A wallet that ends this
    // run below the floor holds eccUSDC and SHELL it cannot move, and the message that would
    // convert SHELL back into gas is one of the messages it can no longer pay for. Stated here, at
    // the last point before anything irreversible is written, and stated with the numbers.
    if let Some(notice) =
        dexdo_core::params::funding_wallet_native_floor_notice(native_raw, available_raw)
    {
        anyhow::bail!(
            "funding wallet {} is below the gas floor its own outgoing messages need: {notice}. \
             No lot was submitted. This is a statement of what messages cost, \
             not a limit on what you may convert -- top the wallet up and re-run the same command.",
            spender.display
        );
    }
    spender.persist_pending(&PendingOperation::Sell {
        floors: first_unseen.clone(),
        denoms: plan.lots.iter().map(|lot| lot.denom).collect(),
    })?;

    for (index, lot) in plan.lots.iter().enumerate() {
        println!(
            "  [{}/{}] sending {} for a {} eccUSDC lot",
            index + 1,
            plan.lot_count(),
            shell_units(lot.shell_raw),
            lot.denom
        );
        spender
            .send_ecc(
                reader.root(),
                SHELL_CURRENCY_ID,
                lot.shell_raw,
                ACCUMULATOR_DAPP_ID,
            )
            .await?;
    }

    // Report what actually landed, read back from the chain rather than assumed from the sends.
    let created = reader
        .await_owned_lots(&spender.address, &first_unseen, plan.lot_count())
        .await?;
    println!(
        "accumulator sell: {} lot(s) now owned by {} on chain",
        created.len(),
        spender.display
    );
    for lot in &created {
        println!(
            "  lot D={} #{} at {} ({})",
            lot.id.denom,
            lot.id.order_id,
            lot.address.with_workchain(),
            if lot.sold { "matched" } else { "waiting" }
        );
    }

    // The lots this run is responsible for are the ones at or after the ids recorded before the
    // first send; anything below that the wallet already owned. Since the wallet receipt is not
    // authoritative here, this count IS the proof that every message landed - so a shortfall fails
    // the command rather than being reported as a smaller number the operator has to notice.
    let landed = created
        .iter()
        .filter(|lot| {
            first_unseen
                .get(&lot.id.denom)
                .is_some_and(|first| lot.id.order_id >= *first)
        })
        .count();
    if landed < plan.lot_count() {
        anyhow::bail!(
            "only {landed} of {} planned lot(s) are on chain for wallet {}. {} deposit(s) cannot be \
             accounted for: re-read with `dexdo accumulator lots` before sending again, because a \
             deposit that DID land cannot be cancelled and re-sending would create a second lot.",
            plan.lot_count(),
            spender.display,
            plan.lot_count() - landed
        );
    }
    spender.clear_pending()?;
    Ok(())
}

async fn run_buy(args: AccumulatorBuyArgs) -> Result<()> {
    let reader = AccumulatorReader::connect(&crate::cli::commands::manifest_path()?, None)?;
    reader.assert_root_identity().await?;

    let (spender, _lock) = Spender::open(
        &crate::cli::commands::manifest_path()?,
        None,
        args.multisig_address.as_deref(),
        &args.multisig_private_key,
        &args.multisig_seed_file,
    )
    .await?;
    if reconcile_pending(&reader, &spender).await? {
        return Ok(());
    }

    let plan =
        BuyPlan::for_whole_usdc(u128::from(args.usdc)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let available_raw = spender.ecc_balance(USDC_CURRENCY_ID).await?;
    plan.require_funded(available_raw)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!(
        "accumulator buy: wallet {} spends {} and receives {}",
        spender.display,
        usdc_units(plan.usdc_raw),
        shell_units(plan.shell_expected_raw)
    );
    println!(
        "  the amount is exact, not a quote: whatever resting lots do not cover, the accumulator \
         mints at the same fixed rate."
    );

    let before_raw = spender.ecc_balance(SHELL_CURRENCY_ID).await?;
    require_native_gas(spender.native_balance().await?, 1)?;
    spender.persist_pending(&PendingOperation::Buy {
        shell_before_raw: before_raw.to_string(),
        shell_expected_raw: plan.shell_expected_raw.to_string(),
        usdc_before_raw: available_raw.to_string(),
        usdc_spent_raw: plan.usdc_raw.to_string(),
    })?;
    spender
        .send_ecc(
            reader.root(),
            USDC_CURRENCY_ID,
            plan.usdc_raw,
            ACCUMULATOR_DAPP_ID,
        )
        .await?;

    let Some(after_raw) = spender
        .await_ecc_credit(SHELL_CURRENCY_ID, before_raw, plan.shell_expected_raw)
        .await?
    else {
        anyhow::bail!(
            "the eccUSDC left wallet {} but the SHELL credit was NOT observed on chain within {} \
             reads. The accumulator pays by a separate message, so this is not proof the exchange \
             failed - but this run cannot confirm it happened, and will not report an unverified \
             money movement as done. Re-read the wallet's ECC[2] SHELL balance before sending \
             anything further.",
            spender.display,
            GAS_BALANCE_CONFIRM_MAX_READS
        );
    };
    println!(
        "accumulator buy: wallet SHELL {} -> {} (credited {})",
        shell_units(before_raw),
        shell_units(after_raw),
        shell_units(after_raw.saturating_sub(before_raw))
    );
    spender.clear_pending()?;
    Ok(())
}

async fn run_lots(args: AccumulatorLotsArgs) -> Result<()> {
    let reader = AccumulatorReader::connect(&crate::cli::commands::manifest_path()?, None)?;
    reader.assert_root_identity().await?;

    let owner = match args.multisig_address.as_deref() {
        Some(address) => dexdo_core::address::parse_chain_address(address)
            .map_err(|e| anyhow::anyhow!("--multisig-address {address}: {e}"))?
            .into_chain(),
        None => {
            let manifest = crate::cli::commands::manifest_path()?;
            let network = dexdo_core::Deployed::load(&manifest)
                .map_err(|e| anyhow::anyhow!("manifest {}: {e}", manifest.display()))?
                .network;
            let wallet_network = crate::cli::wallet::WalletNetwork::from_manifest_label(&network)?;
            let wallet = crate::cli::wallet::resolve_funding_wallet(
                &crate::cli::wallet::WalletStore::open()?,
                &wallet_network,
                None,
                &None,
                &None,
            )?;
            dexdo_core::address::parse_chain_address(&wallet.address)
                .map_err(|e| anyhow::anyhow!("wallet {}: {e}", wallet.address))?
                .into_chain()
        }
    };

    let lots = reader.lots_owned_by(&owner, args.from_order_id).await?;
    println!(
        "accumulator lots owned by {}: {}",
        dexdo_core::address::display_self_dapp(&owner.with_workchain()),
        lots.len()
    );
    let mut claimable = 0usize;
    for lot in &lots {
        let state = if lot.details.claimed {
            "claim in flight"
        } else if lot.sold {
            claimable += 1;
            "matched - claimable"
        } else {
            "waiting for a buyer"
        };
        println!(
            "  D={:<4} #{:<6} {}  {}  payout {}",
            lot.id.denom,
            lot.id.order_id,
            lot.address.with_workchain(),
            state,
            usdc_units(u128::from(lot.id.denom) * USDC_UNIT)
        );
    }
    if claimable > 0 {
        println!("{claimable} lot(s) can be claimed now with `dexdo accumulator claim`.");
    }
    Ok(())
}

async fn run_claim(args: AccumulatorClaimArgs) -> Result<()> {
    let reader = AccumulatorReader::connect(&crate::cli::commands::manifest_path()?, None)?;
    reader.assert_root_identity().await?;

    let (spender, _lock) = Spender::open(
        &crate::cli::commands::manifest_path()?,
        None,
        args.multisig_address.as_deref(),
        &args.multisig_private_key,
        &args.multisig_seed_file,
    )
    .await?;
    if reconcile_pending(&reader, &spender).await? {
        return Ok(());
    }

    let owned = reader
        .lots_owned_by(&spender.address, args.from_order_id)
        .await?;
    let targets: Vec<_> = match (args.denom, args.order_id) {
        (Some(denom), Some(order_id)) => owned
            .into_iter()
            .filter(|lot| {
                lot.id.denom == denom
                    && lot.id.order_id == order_id
                    && is_claim_candidate(lot.sold, lot.details.claimed)
            })
            .collect(),
        _ => owned
            .into_iter()
            .filter(|lot| is_claim_candidate(lot.sold, lot.details.claimed))
            .collect(),
    };

    if targets.is_empty() {
        println!(
            "accumulator claim: wallet {} has no matched lot to claim; nothing was submitted.",
            spender.display
        );
        return Ok(());
    }

    let payout_raw: u128 = targets
        .iter()
        .map(|lot| u128::from(lot.id.denom) * USDC_UNIT)
        .sum();
    println!(
        "accumulator claim: wallet {} claims {} lot(s) worth {}",
        spender.display,
        targets.len(),
        usdc_units(payout_raw)
    );

    let before_raw = spender.ecc_balance(USDC_CURRENCY_ID).await?;
    require_native_gas(spender.native_balance().await?, targets.len())?;
    for lot in &targets {
        reader.assert_lot_identity(&lot.address).await?;
    }
    spender.persist_pending(&PendingOperation::Claim {
        lots: targets
            .iter()
            .map(|lot| (lot.id.denom, lot.id.order_id))
            .collect(),
    })?;
    for lot in &targets {
        if !lot.sold {
            anyhow::bail!(
                "refusing to claim lot D={} #{}: it has not been matched by a buyer yet, and the \
                 accumulator would refuse it (ERR_ORDER_NOT_SOLD, 205). Nothing was submitted.",
                lot.id.denom,
                lot.id.order_id
            );
        }
        println!(
            "  claiming D={} #{} -> {}",
            lot.id.denom,
            lot.id.order_id,
            usdc_units(u128::from(lot.id.denom) * USDC_UNIT)
        );
        spender
            .call_method(
                &lot.address,
                dexdo_core::accumulator::ACCUMULATOR_LOT_ABI,
                "claim",
                serde_json::json!({}),
                ACCUMULATOR_DAPP_ID,
            )
            .await?;
    }

    let Some(after_raw) = spender
        .await_ecc_credit(USDC_CURRENCY_ID, before_raw, payout_raw)
        .await?
    else {
        anyhow::bail!(
            "claim(s) were submitted for wallet {} but the eccUSDC payout was NOT observed on \
             chain within {} reads. A claim on a lot the root has not matched bounces and leaves \
             the lot claimable, so re-run `dexdo accumulator lots` to see the lot's real state \
             before claiming again.",
            spender.display,
            GAS_BALANCE_CONFIRM_MAX_READS
        );
    };
    println!(
        "accumulator claim: wallet eccUSDC {} -> {} (credited {})",
        usdc_units(before_raw),
        usdc_units(after_raw),
        usdc_units(after_raw.saturating_sub(before_raw))
    );
    spender.clear_pending()?;
    Ok(())
}

#[cfg(test)]
mod review_regressions {
    use super::live::{
        confirmation_complete, expected_credit_landed, is_claim_candidate, load_operation,
        persist_operation, require_native_gas, required_native_gas, scan_floor, PendingOperation,
    };
    use std::collections::BTreeMap;

    #[test]
    fn unrelated_partial_credit_does_not_confirm_the_expected_money_movement() {
        assert!(!expected_credit_landed(1_000, 500, 1_001).expect("valid target"));
        assert!(!expected_credit_landed(1_000, 500, 1_499).expect("valid target"));
        assert!(expected_credit_landed(1_000, 500, 1_500).expect("valid target"));
        assert!(expected_credit_landed(1_000, 500, 1_501).expect("valid target"));
    }

    #[test]
    fn an_impossible_expected_balance_fails_closed() {
        assert!(expected_credit_landed(u128::MAX, 1, u128::MAX).is_err());
    }

    #[test]
    fn canonical_wallet_binding_address_reaches_the_chain_account_parser() {
        let account = "ab".repeat(32);
        let parsed = dexdo_core::address::parse_chain_address(&format!("{account}::{account}"))
            .expect("canonical binding address")
            .into_chain();
        assert_eq!(parsed.bare(), account);
    }

    #[test]
    fn sell_confirmation_keeps_each_denominations_own_scan_floor() {
        let floors = BTreeMap::from([(1000, 2), (100, 30), (10, 400), (1, 5_000)]);
        assert_eq!(scan_floor(&floors, 1000), 2);
        assert_eq!(scan_floor(&floors, 1), 5_000);
        assert_eq!(scan_floor(&BTreeMap::new(), 10), 1);
    }

    #[test]
    fn sell_confirmation_waits_for_every_planned_lot() {
        assert!(!confirmation_complete(0, 1));
        assert!(!confirmation_complete(3, 4));
        assert!(confirmation_complete(4, 4));
    }

    #[test]
    fn a_claim_already_in_flight_is_never_submitted_again() {
        assert!(is_claim_candidate(true, false));
        assert!(!is_claim_candidate(true, true));
        assert!(!is_claim_candidate(false, false));
    }

    #[test]
    fn native_gas_preflight_prices_every_message_before_a_multi_send() {
        let one = dexdo_core::params::ACCUMULATOR_WALLET_MESSAGE_GAS_RAW;
        assert_eq!(required_native_gas(3).expect("three messages"), one * 3);
        let refusal = require_native_gas(one * 3 - 1, 3)
            .expect_err("one raw short must refuse the entire operation")
            .to_string();
        assert!(refusal.contains(&(one * 3 - 1).to_string()), "{refusal}");
        assert!(refusal.contains(&(one * 3).to_string()), "{refusal}");
        assert!(refusal.contains("nothing was submitted"), "{refusal}");
    }

    #[test]
    fn pending_buy_intent_is_durable_and_carries_the_chain_reconciliation_baseline() {
        let pending = PendingOperation::Buy {
            shell_before_raw: "41".to_string(),
            shell_expected_raw: "100".to_string(),
            usdc_before_raw: "500".to_string(),
            usdc_spent_raw: "10".to_string(),
        };
        let temp = tempfile::tempdir().expect("temporary operation directory");
        let path = temp.path().join("pending.json");
        persist_operation(&path, &pending).expect("persist before transport");
        let recovered = load_operation(&path)
            .expect("read after process restart")
            .expect("durable intent exists");
        assert_eq!(recovered, pending);
        let PendingOperation::Buy {
            shell_before_raw,
            shell_expected_raw,
            usdc_before_raw,
            usdc_spent_raw,
        } = recovered
        else {
            panic!("buy intent");
        };
        let before_raw = shell_before_raw.parse().unwrap();
        let expected_raw = shell_expected_raw.parse().unwrap();
        assert!(!expected_credit_landed(before_raw, expected_raw, 140).unwrap());
        assert!(expected_credit_landed(before_raw, expected_raw, 141).unwrap());
        assert_eq!(usdc_before_raw, "500");
        assert_eq!(usdc_spent_raw, "10");
    }
}
