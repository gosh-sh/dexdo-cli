//! Deal-close command handlers(Track C7, move-only).

use crate::cli::args::{CloseArgs, DealRoleArg};
#[cfg(feature = "shellnet")]
use crate::cli::commands::{
    close_hint, deal_contracts_path, load_deal_target, shellnet_doctor_preflight_market,
};
use crate::cli::commands::{mock_chain_for_machine, resolve_mock_deal_target, role_arg_str};
#[cfg(feature = "shellnet")]
use crate::cli::deals;
use crate::cli::machine;
#[cfg(feature = "shellnet")]
use crate::cli::support::read_secret_hex;
use anyhow::{bail, Result};
use dexdo_core::ChainBackend;

#[allow(clippy::too_many_arguments)]
fn close_response(
    network: &str,
    handle: Option<String>,
    role: &str,
    token_contract: String,
    action: &str,
    submitted: bool,
    terminal: bool,
    reason: Option<&str>,
    state_before: &str,
    state_after: &str,
) -> Result<machine::CloseResponse> {
    Ok(machine::CloseResponse {
        schema: machine::CLOSE_SCHEMA,
        network: network.to_string(),
        generated_at_unix: machine::now_unix()?,
        handle,
        role: role.to_string(),
        token_contract,
        action: action.to_string(),
        submitted,
        terminal,
        reason: reason.map(str::to_string),
        state_before: state_before.to_string(),
        state_after: state_after.to_string(),
        tx: None,
    })
}

async fn run_close_mock(args: CloseArgs) -> Result<()> {
    let target = resolve_mock_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let role = target.role.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --role buyer|seller with a raw TokenContract",
            args.deal
        )
    })?;
    if target.note_addr.is_none() {
        bail!(
            "close: `{}` is not a local handle; pass --note-addr with a raw TokenContract",
            args.deal
        );
    }
    let role_s = role_arg_str(role);
    let handle = target.handle.as_ref().map(|h| h.handle.clone());
    let chain = mock_chain_for_machine(args.endpoints_file)?;
    let snapshot = chain.snapshot(&target.token_contract).await;
    match snapshot {
        None => {
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_closed"),
                "closed",
                "closed",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: TokenContract {} is inactive/closed",
                response.token_contract
            );
            Ok(())
        }
        Some(snapshot) if snapshot.closed => {
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_stopped"),
                "stopped",
                "stopped",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close noop: {} side already STOPped for {}",
                role_s, response.token_contract
            );
            Ok(())
        }
        Some(snapshot) => {
            if role != DealRoleArg::Buyer {
                bail!(
                    "close: seller cannot destroy opened deal {}. Buyer must STOP/recover/reclaim first.",
                    target.token_contract
                );
            }
            let state_before = if snapshot.seller_received > 0 {
                "streaming"
            } else {
                "probe"
            };
            let note = dexdo_core::LocalNote::generate();
            chain.stop(&target.token_contract, &note).await?;
            let response = close_response(
                "mock",
                handle,
                role_s,
                target.token_contract,
                "streamStop",
                true,
                false,
                None,
                state_before,
                "stopped",
            )?;
            if args.json {
                return machine::print_json(&response);
            }
            println!(
                "close submitted role=buyer action=streamStop token_contract={}",
                response.token_contract
            );
            Ok(())
        }
    }
}

#[cfg(feature = "shellnet")]
pub(crate) async fn run_close(args: CloseArgs) -> Result<()> {
    if args.mock_chain {
        return run_close_mock(args).await;
    }
    use dexdo_core::{
        check_reclaimable, check_recoverable, keypair_ed_pubkey, Address, KeyPair,
        RealChainBackend, MATCH_OPEN_TIMEOUT_SECS,
    };
    let target = load_deal_target(
        &args.deal,
        args.deals_dir.as_deref(),
        args.role,
        args.note_addr.clone(),
    )?;
    let role = target.role.ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --role buyer|seller with a raw TokenContract",
            args.deal
        )
    })?;
    let note_addr = target.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "close: `{}` is not a local handle; pass --note-addr with a raw TokenContract",
            args.deal
        )
    })?;
    if let (Some(handle), Some(arg_note)) = (&target.handle, args.note_addr.as_deref()) {
        if deals::normalize_addr(&handle.note_addr) != deals::normalize_addr(arg_note) {
            bail!(
                "close: --note-addr {arg_note} does not match handle {} note {}",
                handle.handle,
                handle.note_addr
            );
        }
    }
    let contracts_path = deal_contracts_path(args.contracts.as_deref(), &target);
    shellnet_doctor_preflight_market(&contracts_path, target.market.as_ref()).await?;
    let contracts = args
        .contracts
        .as_deref()
        .unwrap_or(&contracts_path)
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let tc = Address::parse(&target.token_contract)
        .map_err(|e| anyhow::anyhow!("token_contract {}: {e}", target.token_contract))?;
    let Some(state) = chain.token_contract_state(&tc).await? else {
        if args.json {
            return machine::print_json(&close_response(
                "shellnet",
                target.handle.as_ref().map(|h| h.handle.clone()),
                role.as_str(),
                target.token_contract,
                "noop",
                false,
                false,
                Some("already_closed"),
                "closed",
                "closed",
            )?);
        }
        println!(
            "close noop: TokenContract {} is inactive/closed",
            target.token_contract
        );
        return Ok(());
    };
    let s = deals::classify_deal_state(&state);
    match role {
        deals::DealHandleRole::Seller => {
            if s.disputed {
                bail!(
                    "close: seller deal {} is disputed; seller-side release is tracked by . Next command \
                     once exposed: `dexdo release-dispute {}`.",
                    target.token_contract,
                    target
                        .handle
                        .as_ref()
                        .map(|h| h.handle.as_str())
                        .unwrap_or(&target.token_contract)
                );
            }
            if s.opened {
                bail!(
                    "close: seller cannot destroy opened deal {}. {}",
                    target.token_contract,
                    close_hint(&target, &s)
                );
            }
            if s.kind != deals::DealStateKind::Stopped {
                bail!("{}", close_hint(&target, &s));
            }
            let note_key = args.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!("close seller requires --note-key to sign destroy")
            })?;
            let keys = KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            let note = Address::parse(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            chain.destroy_token_contract(&tc, &note, &keys).await?;
            if args.json {
                return machine::print_json(&close_response(
                    "shellnet",
                    target.handle.as_ref().map(|h| h.handle.clone()),
                    role.as_str(),
                    target.token_contract.clone(),
                    "destroy",
                    true,
                    true,
                    None,
                    s.kind.as_str(),
                    "closed",
                )?);
            }
            println!(
                "close submitted role=seller action=destroy token_contract={} note={}",
                target.token_contract, note
            );
        }
        deals::DealHandleRole::Buyer => {
            if s.disputed {
                bail!(
                    "close: buyer deal {} is disputed; wait for seller release/arbitration (), then re-run status.",
                    target.token_contract
                );
            }
            if s.kind == deals::DealStateKind::Stopped {
                if args.json {
                    return machine::print_json(&close_response(
                        "shellnet",
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "noop",
                        false,
                        false,
                        Some("already_stopped"),
                        "stopped",
                        "stopped",
                    )?);
                }
                println!(
                    "close noop: buyer side already STOPped for {}. Next: seller runs `dexdo close <seller-handle> --note-key <seller-key>`.",
                    target.token_contract
                );
                return Ok(());
            }
            let note_key = args.note_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!("close buyer requires --note-key to sign note owner method")
            })?;
            let keys = KeyPair::from_secret_hex(read_secret_hex(note_key, "--note-key")?.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            let note = Address::parse(&note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let buyer_note = chain.token_contract_buyer_note(&tc).await?;
            let buyer_note_s = buyer_note.as_ref().map(|a| a.with_workchain());
            let buyer_pubkey = chain.token_contract_buyer_pubkey(&tc).await?;
            let note_ed = keypair_ed_pubkey(&keys)?;
            if s.opened {
                let cfg = chain.token_contract_config(&tc).await?.ok_or_else(|| {
                    anyhow::anyhow!("close: TokenContract {} getConfig unavailable", tc)
                })?;
                let stream_timeout = cfg["streamTimeout"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| anyhow::anyhow!("close: getConfig exposes no streamTimeout"))?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
                    .as_secs();
                match buyer_opened_close_action(now, s.last_advance, stream_timeout) {
                    BuyerOpenedCloseAction::StreamReclaim => {
                        check_reclaimable(
                            s.funded,
                            s.opened,
                            s.disputed,
                            buyer_note_s.as_deref(),
                            &note.with_workchain(),
                            buyer_pubkey.as_ref(),
                            &note_ed,
                            now,
                            s.last_advance,
                            Some(stream_timeout),
                            s.funded_time,
                            MATCH_OPEN_TIMEOUT_SECS,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                        chain.reclaim_on_timeout(&note, &keys, &tc).await?;
                        if args.json {
                            return machine::print_json(&close_response(
                                "shellnet",
                                target.handle.as_ref().map(|h| h.handle.clone()),
                                role.as_str(),
                                target.token_contract.clone(),
                                "streamReclaim",
                                true,
                                false,
                                None,
                                s.kind.as_str(),
                                "stopped",
                            )?);
                        }
                        println!(
                            "close submitted role=buyer action=streamReclaim token_contract={} note={}",
                            target.token_contract, note
                        );
                    }
                    BuyerOpenedCloseAction::StreamStop => {
                        check_recoverable(
                            s.opened,
                            s.disputed,
                            buyer_note_s.as_deref(),
                            &note.with_workchain(),
                            buyer_pubkey.as_ref(),
                            &note_ed,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                        chain.stream_stop(&note, &keys, &tc).await?;
                        if args.json {
                            return machine::print_json(&close_response(
                                "shellnet",
                                target.handle.as_ref().map(|h| h.handle.clone()),
                                role.as_str(),
                                target.token_contract.clone(),
                                "streamStop",
                                true,
                                false,
                                None,
                                s.kind.as_str(),
                                "stopped",
                            )?);
                        }
                        println!(
                            "close submitted role=buyer action=streamStop token_contract={} note={}",
                            target.token_contract, note
                        );
                    }
                }
                return Ok(());
            }
            if s.funded && !s.probe_accepted {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
                    .as_secs();
                check_reclaimable(
                    s.funded,
                    s.opened,
                    s.disputed,
                    buyer_note_s.as_deref(),
                    &note.with_workchain(),
                    buyer_pubkey.as_ref(),
                    &note_ed,
                    now,
                    s.last_advance,
                    None,
                    s.funded_time,
                    MATCH_OPEN_TIMEOUT_SECS,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{e}. Next: re-run `dexdo close {}` after MATCH_OPEN_TIMEOUT, or inspect with `dexdo status {}`.",
                        args.deal,
                        args.deal
                    )
                })?;
                chain.stream_cleanup(&note, &keys, &tc).await?;
                if args.json {
                    return machine::print_json(&close_response(
                        "shellnet",
                        target.handle.as_ref().map(|h| h.handle.clone()),
                        role.as_str(),
                        target.token_contract.clone(),
                        "streamCleanup",
                        true,
                        false,
                        None,
                        s.kind.as_str(),
                        "stopped",
                    )?);
                }
                println!(
                    "close submitted role=buyer action=streamCleanup token_contract={} note={}",
                    target.token_contract, note
                );
                return Ok(());
            }
            bail!("{}", close_hint(&target, &s));
        }
    }
    Ok(())
}

#[cfg(not(feature = "shellnet"))]
pub(crate) async fn run_close(args: CloseArgs) -> Result<()> {
    if args.mock_chain {
        return run_close_mock(args).await;
    }
    bail!("close unavailable: build with `--features shellnet`")
}

#[cfg(feature = "shellnet")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuyerOpenedCloseAction {
    StreamStop,
    StreamReclaim,
}

#[cfg(feature = "shellnet")]
fn buyer_opened_close_action(
    now: u64,
    last_advance: u64,
    stream_timeout: u64,
) -> BuyerOpenedCloseAction {
    if now >= last_advance.saturating_add(stream_timeout) {
        BuyerOpenedCloseAction::StreamReclaim
    } else {
        BuyerOpenedCloseAction::StreamStop
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "shellnet")]
    #[test]
    fn buyer_close_reclaims_opened_deal_after_stream_timeout() {
        assert_eq!(
            super::buyer_opened_close_action(699, 100, 600),
            super::BuyerOpenedCloseAction::StreamStop
        );
        assert_eq!(
            super::buyer_opened_close_action(700, 100, 600),
            super::BuyerOpenedCloseAction::StreamReclaim
        );
        assert_eq!(
            super::buyer_opened_close_action(u64::MAX - 1, u64::MAX - 10, 600),
            super::BuyerOpenedCloseAction::StreamStop
        );
    }
}
