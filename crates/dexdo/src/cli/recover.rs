//! `dexdo` pool-recovery command handlers(`recover`/`dispute`/`reclaim`/`release-dispute`/`withdraw-shell`),
//! extracted from `commands.rs`(move-only / behavior-identical, anti-entropy refactor Track C2).

use crate::cli::args::{
    DisputeArgs, ReclaimArgs, RecoverArgs, ReleaseDisputeArgs, WithdrawShellArgs,
};
use anyhow::Result;

#[cfg(feature = "shellnet")]
use crate::cli::commands::{persist_pool_recovery_record, resolve_pool_recovery_inputs};
#[cfg(feature = "shellnet")]
use crate::cli::support::{read_secret_hex, resolve_market_fields};
#[cfg(not(feature = "shellnet"))]
use anyhow::bail;
#[cfg(feature = "shellnet")]
use serde_json::Value;

/// recover an orphaned OPEN deal. The buyer process died mid-stream but the buyer note/key are intact,
/// so no one sent STOP and the deal hangs OPEN(the seller cannot `destroy` an `_opened` deal). `recover`
/// signs the **normal buyer-STOP** (`streamStop(tokenContract)` -> `TokenContract.stop()`, standard
/// split) from the buyer note -- it does NOT place a new buy -- after which the seller `destroy`s the TC.
/// Fails closed(before sending STOP) if the deal is not `_opened`, is `_disputed`, or the note is not the
/// deal's recorded buyer; the on-chain `TC.stop()` also enforces `msg.sender == _buyer`.
/// (The "seller vanished mid-stream" case is instead the contract's `reclaimOnTimeout`/`STREAM_TIMEOUT`.)
#[cfg(feature = "shellnet")]
#[async_trait::async_trait]
trait RecoverChain {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<Value>>;
    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>>;
    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>>;
    async fn stop(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<()>;
}

#[cfg(feature = "shellnet")]
#[async_trait::async_trait]
impl RecoverChain for dexdo_core::RealChainBackend {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<Value>> {
        Ok(self.token_contract_state(tc).await?)
    }

    async fn buyer_note(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::Address>> {
        Ok(self.token_contract_buyer_note(tc).await?)
    }

    async fn buyer_pubkey(&self, tc: &dexdo_core::Address) -> Result<Option<[u8; 32]>> {
        Ok(self.token_contract_buyer_pubkey(tc).await?)
    }

    async fn stop(
        &self,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
        tc: &dexdo_core::Address,
    ) -> Result<()> {
        self.stream_stop(note, keys, tc).await?;
        Ok(())
    }
}

#[cfg(feature = "shellnet")]
async fn run_recover_with_chain(args: RecoverArgs, chain: &dyn RecoverChain) -> Result<()> {
    use dexdo_core::{check_recoverable, keypair_ed_pubkey, Address, KeyPair};
    let resolved = resolve_pool_recovery_inputs(
        "recover",
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    )?;
    let pool_record = resolved.pool_record;
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    let state = chain.state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("recover: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let buyer_note = chain.buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_recoverable(
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "recover {tc}: buyer-signed STOP of an OPEN deal (streamStop -> TokenContract.stop(), standard \
         split). No new buy is placed. After this, the seller closes it: `dexdo destroy --token-contract {tc}`."
    );
    chain.stop(&note, &keys, &tc).await?;
    if let Some(record) = pool_record.as_ref() {
        persist_pool_recovery_record(record)?;
    }
    println!(
        "recover submitted -> streamStop(TokenContract {tc}) from buyer note {note}; the deal STOPs (standard \
         split). Next: the seller runs `dexdo destroy` to close (selfdestruct) the TokenContract."
    );
    Ok(())
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_recover(args: RecoverArgs) -> Result<()> {
    use dexdo_core::RealChainBackend;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(manifest)?;
    run_recover_with_chain(args, &chain).await
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_recover(_args: RecoverArgs) -> Result<()> {
    bail!("recover unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_dispute(args: DisputeArgs) -> Result<()> {
    use dexdo_core::{check_disputable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend};
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let resolved = resolve_pool_recovery_inputs(
        "dispute",
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    )?;
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: only an OPEN, undisputed deal owned by THIS buyer note/key can be disputed.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("dispute: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    check_disputable(
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "dispute {tc}: buyer-signed streamDispute -> TokenContract.dispute() () -- LOCKS BOTH notes (yours \
         and the seller's) until releaseDispute/arbitration. Stronger than `recover` (which still pays the \
         seller for delivered ticks); releaseDispute is seller-only."
    );
    chain.stream_dispute(&note, &keys, &tc).await?;
    println!(
        "dispute submitted -> streamDispute(TokenContract {tc}) from buyer note {note}; the deal is DISPUTED \
         and both notes are locked until it resolves (seller releaseDispute, or arbitration)."
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_dispute(_args: DisputeArgs) -> Result<()> {
    bail!("dispute unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_reclaim(args: ReclaimArgs) -> Result<()> {
    use dexdo_core::{
        check_reclaimable, keypair_ed_pubkey, Address, KeyPair, RealChainBackend,
        MATCH_OPEN_TIMEOUT_SECS,
    };
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let resolved = resolve_pool_recovery_inputs(
        "reclaim",
        &args.identity,
        args.market.as_deref(),
        args.token_contract.as_deref(),
        args.pool.as_deref(),
    )?;
    let note_addr = resolved.note_addr;
    let tc_str = resolved.token_contract;
    let seed = resolved.note_secret_hex;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    // Fail-loud pre-flight: owned by THIS buyer + funded + not disputed + the
    // relevant timeout reached. OPEN deals use STREAM_TIMEOUT(streamReclaim); funded-but-never-opened deals use
    // MATCH_OPEN_TIMEOUT from fundedTime(streamCleanup). Reject locally rather than letting the contract revert.
    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("reclaim: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let funded = state["funded"].as_bool().unwrap_or(false);
    let opened = state["opened"].as_bool().unwrap_or(false);
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    let last_advance = state["lastAdvance"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let funded_time = state["fundedTime"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok());
    let buyer_note = chain.token_contract_buyer_note(&tc).await?;
    let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
    let note_s = note.with_workchain();
    let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
    let note_ed = keypair_ed_pubkey(&keys)?;
    // Per-deal dynamic STREAM_TIMEOUT is only needed for OPEN abandoned deals. The never-opened cleanup path
    // gates on fixed MATCH_OPEN_TIMEOUT from getState.fundedTime.
    let stream_timeout = if opened {
        let cfg = chain
            .token_contract_config(&tc)
            .await?
            .ok_or_else(|| anyhow::anyhow!("reclaim: TokenContract {tc} getConfig unavailable"))?;
        Some(
            cfg["streamTimeout"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("reclaim: getConfig exposes no streamTimeout"))?,
        )
    } else {
        None
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs();
    check_reclaimable(
        funded,
        opened,
        disputed,
        buyer_note_s.as_deref(),
        &note_s,
        buyer_pubkey.as_ref(),
        &note_ed,
        now,
        last_advance,
        stream_timeout,
        funded_time,
        MATCH_OPEN_TIMEOUT_SECS,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if opened {
        let stream_timeout = stream_timeout.expect("opened branch parsed streamTimeout");
        eprintln!(
            "reclaim {tc}: buyer-signed streamReclaim -> TokenContract.reclaimOnTimeout() (no burn: probe + \
             deposit back to you, commission to the seller). STREAM_TIMEOUT met: lastAdvance {last_advance} + \
             streamTimeout {stream_timeout} <= now {now}."
        );
        chain.reclaim_on_timeout(&note, &keys, &tc).await?;
        println!(
            "reclaim submitted -> streamReclaim(TokenContract {tc}) from buyer note {note}; the escrow returns \
             to your note and the deal closes (opened=false)."
        );
    } else {
        let funded_time = funded_time.expect("never-opened branch checked fundedTime");
        eprintln!(
            "reclaim {tc}: buyer-signed streamCleanup -> TokenContract.cleanupUnopened() (never-opened refund). \
             MATCH_OPEN_TIMEOUT met: fundedTime {funded_time} + matchOpenTimeout {MATCH_OPEN_TIMEOUT_SECS} <= \
             now {now}."
        );
        chain.stream_cleanup(&note, &keys, &tc).await?;
        println!(
            "reclaim submitted -> streamCleanup(TokenContract {tc}) from buyer note {note}; the never-opened \
             escrow returns to your note and the deal closes."
        );
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_reclaim(_args: ReclaimArgs) -> Result<()> {
    bail!("reclaim unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_release_dispute(args: ReleaseDisputeArgs) -> Result<()> {
    use dexdo_core::{
        check_release_disputable, check_seller_pubkey, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!("release-dispute: --note-addr (seller note) is required")
        })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("release-dispute: --note-key (seller owner key) is required")
    })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note =
        Address::parse(&note_addr).map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;

    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("release-dispute: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let disputed = state["disputed"].as_bool().unwrap_or(false);
    check_release_disputable(disputed).map_err(|e| anyhow::anyhow!(e))?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("release-dispute", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "release-dispute {tc}: seller-signed TokenContract.releaseDispute() from note {note}; concedes the \
         dispute, unlocks both notes, and returns the contested tick/deposit to the buyer."
    );
    chain.release_dispute(&tc, &keys).await?;
    println!(
        "release-dispute submitted -> TokenContract {tc}; both notes unlock after the dispute resolution lands"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_release_dispute(_args: ReleaseDisputeArgs) -> Result<()> {
    bail!("release-dispute unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_withdraw_shell(args: WithdrawShellArgs) -> Result<()> {
    use dexdo_core::{
        check_seller_pubkey, check_withdrawable_shell, Address, KeyPair, RealChainBackend,
    };
    let note_addr =
        args.identity.note_addr.clone().ok_or_else(|| {
            anyhow::anyhow!("withdraw-shell: --note-addr (seller note) is required")
        })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("withdraw-shell: --note-key (seller owner key) is required")
    })?;
    let recipient_addr = args.recipient.clone().unwrap_or_else(|| note_addr.clone());
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = read_secret_hex(note_key, "--note-key")?;
    let chain = RealChainBackend::connect(manifest)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let recipient = Address::parse(&recipient_addr)
        .map_err(|e| anyhow::anyhow!("--recipient/--note-addr {recipient_addr}: {e}"))?;

    let state = chain.token_contract_state(&tc).await?.ok_or_else(|| {
        anyhow::anyhow!("withdraw-shell: TokenContract {tc} is not active (undeployed/closed)")
    })?;
    let finalized_owed = state["finalizedOwed"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .ok_or_else(|| anyhow::anyhow!("withdraw-shell: getState exposes no finalizedOwed"))?;
    let amount =
        check_withdrawable_shell(finalized_owed, args.amount).map_err(|e| anyhow::anyhow!(e))?;
    let seller = chain.token_contract_seller_pubkey(&tc).await?;
    check_seller_pubkey("withdraw-shell", seller.as_deref(), keys.public_hex())
        .map_err(|e| anyhow::anyhow!(e))?;

    eprintln!(
        "withdraw-shell {tc}: seller-signed TokenContract.withdrawShell(amount={amount}, recipient={recipient}). \
         This withdraws finalized seller proceeds only; use `destroy` later to close/selfdestruct the TC."
    );
    chain.withdraw_shell(&tc, amount, &recipient, &keys).await?;
    println!(
        "withdraw-shell submitted -> {amount} finalized SHELL from TokenContract {tc} to {recipient}"
    );
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_withdraw_shell(_args: WithdrawShellArgs) -> Result<()> {
    bail!("withdraw-shell unavailable: build with `--features shellnet`")
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "shellnet")]
    use crate::cli::args::{IdentityArgs, RecoverArgs};

    #[cfg(feature = "shellnet")]
    struct TempDirCleanup(std::path::PathBuf);

    #[cfg(feature = "shellnet")]
    impl Drop for TempDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(feature = "shellnet")]
    struct PoolRecoverChain {
        buyer_note: dexdo_core::Address,
        buyer_pubkey: [u8; 32],
        stop_calls: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "shellnet")]
    #[async_trait::async_trait]
    impl super::RecoverChain for PoolRecoverChain {
        async fn state(
            &self,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<serde_json::Value>> {
            Ok(Some(serde_json::json!({
                "opened": true,
                "disputed": false
            })))
        }

        async fn buyer_note(
            &self,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<dexdo_core::Address>> {
            Ok(Some(self.buyer_note.clone()))
        }

        async fn buyer_pubkey(
            &self,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<Option<[u8; 32]>> {
            Ok(Some(self.buyer_pubkey))
        }

        async fn stop(
            &self,
            note: &dexdo_core::Address,
            _keys: &dexdo_core::KeyPair,
            _tc: &dexdo_core::Address,
        ) -> anyhow::Result<()> {
            assert_eq!(note.with_workchain(), self.buyer_note.with_workchain());
            self.stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// primary regression: the production recover flow must atomically write the selected pool-only buyer
    /// record after STOP, so a fresh pool load observes it as a durable buyer recovery record.
    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn run_recover_persists_pool_only_record_across_reload() {
        use std::sync::atomic::Ordering;

        let dir = std::env::temp_dir().join(format!(
            "dexdo-run-recover-persist-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = TempDirCleanup(dir.clone());
        let pool_path = dir.join("pn_pool.json");
        let note_addr = format!("0:{}", "1".repeat(64));
        let token_contract = format!("0:{}", "2".repeat(64));
        let seller_tc = format!("0:{}", "3".repeat(64));
        let secret = "2a".repeat(32);
        std::fs::write(
            &pool_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "notes": [
                    {
                        "address": note_addr,
                        "owner_secret_key_hex": secret,
                        "token_contract": seller_tc,
                        "token_contract_role": "seller",
                        "token_contract_updated_at_unix": 7
                    },
                    {
                        "address": note_addr,
                        "owner_secret_key_hex": secret,
                        "token_contract": token_contract
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let keys = dexdo_core::KeyPair::from_secret_hex(&secret).unwrap();
        let chain = PoolRecoverChain {
            buyer_note: dexdo_core::Address::parse(&note_addr).unwrap(),
            buyer_pubkey: dexdo_core::keypair_ed_pubkey(&keys).unwrap(),
            stop_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        super::run_recover_with_chain(
            RecoverArgs {
                identity: IdentityArgs {
                    note_key: None,
                    note_index: 0,
                    note_addr: None,
                },
                token_contract: None,
                market: None,
                pool: Some(pool_path.clone()),
                contracts: dir.join("unused-contracts.json"),
            },
            &chain,
        )
        .await
        .unwrap();

        assert_eq!(chain.stop_calls.load(Ordering::SeqCst), 1);
        let reloaded = crate::cli::commands::load_pool_json(&pool_path).unwrap();
        let notes = reloaded["notes"].as_array().unwrap();
        let seller = notes
            .iter()
            .find(|note| note["token_contract"] == seller_tc)
            .expect("different seller record must remain present");
        assert_eq!(seller["token_contract_role"], "seller");
        assert_eq!(seller["token_contract_updated_at_unix"], 7);
        let recovered = notes
            .iter()
            .find(|note| note["token_contract"] == token_contract)
            .expect("recovered buyer record must survive pool reload");
        assert_eq!(recovered["owner_secret_key_hex"], secret);
        assert_eq!(recovered["token_contract_role"], "buyer");
        assert!(recovered["token_contract_updated_at_unix"]
            .as_u64()
            .is_some());
    }
}
