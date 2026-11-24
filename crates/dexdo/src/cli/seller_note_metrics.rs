//! `dexdo.seller.note.balance`: the trading and the gas SHELL of every note that has a deal this
//! seller is working now, read every `SELLER_METRICS_NOTE_BALANCE_INTERVAL`.

//! The deals are the ones the running seller watches or serves. Each deal's note is the
//! `note_addr` its record in `<data-dir>/deals/` names, and a note is labelled by its number in the
//! note pool, never by its address. A note of the pool with no active deal is not written.
use crate::cli::deals;
use anyhow::{anyhow, Result};
use dexdo::telemetry::{self, NoteBalance};
use dexdo_core::RealChainBackend;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Stops the reads when dropped.
pub(crate) struct NoteBalanceReader(tokio::task::JoinHandle<()>);

impl Drop for NoteBalanceReader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Start reading, when seller metrics are on.
pub(crate) fn start(manifest: PathBuf, deals_dir: Option<PathBuf>) -> Option<NoteBalanceReader> {
    if !telemetry::enabled() {
        return None;
    }
    Some(NoteBalanceReader(tokio::spawn(async move {
        let chain = match RealChainBackend::connect(&manifest) {
            Ok(chain) => chain,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "seller note balances are not read");
                return;
            }
        };
        let mut every =
            tokio::time::interval(dexdo_core::params::SELLER_METRICS_NOTE_BALANCE_INTERVAL);
        loop {
            every.tick().await;
            let balances = read(&chain, deals_dir.as_deref())
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(error = %format!("{error:#}"), "seller note balances: this read failed");
                    Vec::new()
                });
            telemetry::note_balances(&balances);
        }
    })))
}

/// The account part of an address, lowercase, whichever form it came in.
fn account_key(address: &str) -> String {
    address
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The notes of the deals `active` names, each with its number in the pool at `pool_path`.
fn active_notes(
    active: &[String],
    deals_dir: &Path,
    pool_path: Option<&Path>,
) -> Result<Vec<(usize, String)>> {
    let active: HashSet<String> = active
        .iter()
        .map(|token_contract| account_key(token_contract))
        .collect();
    if active.is_empty() {
        return Ok(Vec::new());
    }
    let mut notes = BTreeSet::new();
    for (_, handle) in deals::list_deal_handles(deals_dir)? {
        if handle.role == deals::DealHandleRole::Seller
            && active.contains(&account_key(&handle.token_contract))
        {
            notes.insert(deals::normalize_addr(&handle.note_addr));
        }
    }
    if notes.is_empty() {
        return Ok(Vec::new());
    }
    let pool_path = pool_path.ok_or_else(|| anyhow!("no note pool to number the notes by"))?;
    let pool = crate::cli::commands::load_pool_json(pool_path)?;
    let pool: Vec<String> = pool["notes"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    entry["address"]
                        .as_str()
                        .map(deals::normalize_addr)
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(notes
        .into_iter()
        .filter_map(|note| match pool.iter().position(|entry| *entry == note) {
            Some(number) => Some((number, note)),
            None => {
                tracing::warn!(
                    %note,
                    "a seller deal's note is not in the note pool; its balance is not written"
                );
                None
            }
        })
        .collect())
}

async fn read(chain: &RealChainBackend, deals_dir: Option<&Path>) -> Result<Vec<NoteBalance>> {
    let notes = active_notes(
        &telemetry::active_deals(),
        &deals::resolve_deals_dir(deals_dir)?,
        crate::cli::commands::note_pool_path(None).as_deref(),
    )?;
    let mut balances = Vec::new();
    for (number, note) in notes {
        let address = dexdo_core::address::parse_chain_address(&note)?;
        let trading_raw = chain.private_note_shell_balance(&address).await.ok();
        let gas_raw = chain
            .get_system_account_retrying(&address)
            .await
            .ok()
            .flatten()
            .map(|account| account.ecc_balance(dexdo_core::params::SHELL_CURRENCY_ID));
        balances.push(NoteBalance {
            note: number,
            trading_raw,
            gas_raw,
        });
    }
    Ok(balances)
}

#[cfg(test)]
#[path = "seller_note_metrics_2245_tests.rs"]
mod tests;
