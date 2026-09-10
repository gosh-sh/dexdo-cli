//! Local deal handles: small public JSON records that let operators run
//! `deals`/`status`/`close` without reassembling low-level addresses.

use anyhow::{bail, Result};
use dexdo_core::DealBuyerBond;
use dexdo_core::{DealChainState, MarketManifest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Durable deal-record schema written by this runtime. An absent field is the understood
/// pre-versioning schema 0; this value changes only when the record meaning changes.
pub(crate) const DEAL_HANDLE_VERSION: u32 = 2;

const LAST_OBSERVED_PROMOTION_FIELD: &str = "last_observed_promotion";

#[derive(Debug, Deserialize)]
struct DealHandleVersionProbe {
    #[serde(default)]
    version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DealHandleSchemaTooNew {
    handle: String,
    record_version: u32,
    max_supported_version: u32,
}

impl DealHandleSchemaTooNew {
    pub(crate) fn handle(&self) -> &str {
        &self.handle
    }

    pub(crate) fn record_version(&self) -> u32 {
        self.record_version
    }

    pub(crate) fn max_supported_version(&self) -> u32 {
        self.max_supported_version
    }
}

impl std::fmt::Display for DealHandleSchemaTooNew {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "deal handle {} carries schema version {}, but this runtime understands through {}; \
             keep the older runtime pinned until that deal terminates",
            self.handle, self.record_version, self.max_supported_version
        )
    }
}

impl std::error::Error for DealHandleSchemaTooNew {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DealHandleRole {
    Buyer,
    Seller,
}

impl DealHandleRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Buyer => "buyer",
            Self::Seller => "seller",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DealEndpointInfo {
    pub(crate) kind: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DealHandle {
    #[serde(default)]
    pub(crate) version: u32,
    pub(crate) handle: String,
    pub(crate) role: DealHandleRole,
    pub(crate) network: String,
    // issue: addresses are written canonically and read in either form, so a handle file written
    // by an older version keeps loading while a new one carries the DApp identity.
    #[serde(with = "dexdo_core::address::serde_self_dapp")]
    pub(crate) token_contract: String,
    #[serde(with = "dexdo_core::address::serde_canonical")]
    pub(crate) note_addr: String,
    pub(crate) frame_model: String,
    pub(crate) model_hash: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) order_book: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) root_model: Option<String>,
    pub(crate) market: Option<MarketManifest>,
    pub(crate) contracts: String,
    pub(crate) endpoint: Option<DealEndpointInfo>,
    pub(crate) created_order_ids: Vec<u128>,
    pub(crate) created_at_unix: u64,
}

/// The last claim-pipeline values read from a live `TokenContract.getState()`.

/// `last_claim_time` is deliberately named after the getter field. It is the chain timestamp that
/// accompanied the pair, not proof that the pair was still current when a later close executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LastObservedPromotion {
    #[serde(with = "decimal_u128")]
    pub(crate) tokens_final: u128,
    #[serde(with = "decimal_u128")]
    pub(crate) tokens_pending: u128,
    pub(crate) last_claim_time: u64,
}

impl From<DealChainState> for LastObservedPromotion {
    fn from(state: DealChainState) -> Self {
        Self {
            tokens_final: state.tokens_final,
            tokens_pending: state.tokens_pending,
            last_claim_time: state.last_claim_time,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DealRecord {
    pub(crate) handle: DealHandle,
    pub(crate) last_observed_promotion: Option<LastObservedPromotion>,
}

// `Placed`/`FundedButNeverOpened`/`Disputed` are produced only by the chain-state summariser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DealStateKind {
    Placed,
    FundedButNeverOpened,
    Probe,
    Streaming,
    Stopped,
    Disputed,
}

impl DealStateKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Placed => "placed",
            Self::FundedButNeverOpened => "funded-but-never-opened",
            Self::Probe => "probe",
            Self::Streaming => "streaming",
            Self::Stopped => "stopped",
            Self::Disputed => "disputed",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DealStateSummary {
    pub(crate) kind: DealStateKind,
    pub(crate) funded: bool,
    pub(crate) opened: bool,
    pub(crate) disputed: bool,
    pub(crate) probe_accepted: bool,
    pub(crate) deposit: u128,
    pub(crate) probe_tick: u128,
    pub(crate) buyer_bond: u128,
    pub(crate) buyer_bond_required: u128,
    pub(crate) finalized_owed: u128,
    pub(crate) tokens_final: u128,
    pub(crate) tokens_pending: u128,
    pub(crate) funded_time: Option<u64>,
    pub(crate) probe_time: u64,
    pub(crate) last_claim_time: u64,
    pub(crate) dispute_time: u64,
}

impl DealStateSummary {
    pub(crate) fn buyer_locked(&self) -> Result<u128> {
        self.deposit
            .checked_add(self.probe_tick)
            .and_then(|total| total.checked_add(self.buyer_bond))
            .ok_or_else(|| anyhow::anyhow!("buyer locked amount overflows uint128"))
    }
}

#[cfg(test)]
pub(crate) fn classify_deal_state(
    state: &serde_json::Value,
    buyer_bond: DealBuyerBond,
) -> Result<DealStateSummary> {
    let state = DealChainState::decode_getter(state).map_err(anyhow::Error::msg)?;
    Ok(summarize_chain_state(state, buyer_bond))
}

pub(crate) fn summarize_deal_snapshot(
    snapshot: &dexdo_core::DealChainSnapshot,
) -> DealStateSummary {
    summarize_chain_state(snapshot.state, snapshot.buyer_bond)
}

fn summarize_chain_state(state: DealChainState, buyer_bond: DealBuyerBond) -> DealStateSummary {
    let kind = if state.disputed {
        DealStateKind::Disputed
    } else if state.opened && state.probe_accepted {
        DealStateKind::Streaming
    } else if state.opened {
        DealStateKind::Probe
    } else if state.is_stopped() {
        DealStateKind::Stopped
    } else if state.funded {
        DealStateKind::FundedButNeverOpened
    } else {
        DealStateKind::Placed
    };
    DealStateSummary {
        kind,
        funded: state.funded,
        opened: state.opened,
        disputed: state.disputed,
        probe_accepted: state.probe_accepted,
        deposit: state.deposit,
        probe_tick: state.probe_tick,
        buyer_bond: buyer_bond.bond_held,
        buyer_bond_required: buyer_bond.bond_required,
        finalized_owed: state.finalized_owed,
        tokens_final: state.tokens_final,
        tokens_pending: state.tokens_pending,
        funded_time: state.funded_time,
        probe_time: state.probe_time,
        last_claim_time: state.last_claim_time,
        dispute_time: state.dispute_time,
    }
}

pub(crate) fn deal_state_getter_json(state: DealChainState) -> serde_json::Value {
    serde_json::json!({
        "funded": state.funded,
        "opened": state.opened,
        "probeAccepted": state.probe_accepted,
        "disputed": state.disputed,
        "deposit": state.deposit.to_string(),
        "probeTick": state.probe_tick.to_string(),
        "finalizedOwed": state.finalized_owed.to_string(),
        "tokensFinal": state.tokens_final.to_string(),
        "tokensPending": state.tokens_pending.to_string(),
        "probeTime": state.probe_time.to_string(),
        "lastClaimTime": state.last_claim_time.to_string(),
        "disputeTime": state.dispute_time.to_string(),
        "fundedTime": state.funded_time.unwrap_or(0).to_string(),
    })
}

pub(crate) fn default_deals_dir() -> Result<PathBuf> {
    crate::cli::data_dir::automatic("deals")
}

pub(crate) fn resolve_deals_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    Ok(match explicit {
        Some(p) => p.to_path_buf(),
        None => default_deals_dir()?,
    })
}

pub(crate) fn make_token_contract_id(token_contract: &str) -> String {
    let clean = token_contract
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("deal-{clean}")
}

pub(crate) fn make_handle_id(token_contract: &str, role: DealHandleRole) -> String {
    format!(
        "{}-{}",
        make_token_contract_id(token_contract),
        role.as_str()
    )
}

pub(crate) fn handle_path(dir: &Path, handle: &str) -> PathBuf {
    dir.join(format!("{handle}.json"))
}

pub(crate) fn save_deal_handle(dir: &Path, handle: &DealHandle) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("create deals dir {}: {e}", dir.display()))?;
    let path = handle_path(dir, &handle.handle);
    // A runtime restart rewrites the same durable identity. Retain a valid observation already in
    // that record rather than erasing the only non-circular audit fact before a later settlement.
    let last_observed_promotion = if path.exists() {
        let record = load_deal_record(&path)?;
        same_deal_side(&record.handle, handle)
            .then_some(record.last_observed_promotion)
            .flatten()
    } else {
        None
    };
    let bytes = serialize_deal_record(handle, last_observed_promotion)?;
    write_private_atomic(&path, &bytes)?;
    Ok(path)
}

pub(crate) fn load_deal_handle(path: &Path) -> Result<DealHandle> {
    Ok(load_deal_record(path)?.handle)
}

pub(crate) fn load_deal_record(path: &Path) -> Result<DealRecord> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read deal handle {}: {e}", path.display()))?;
    let probe: DealHandleVersionProbe = serde_json::from_str(&s)
        .map_err(|e| anyhow::anyhow!("parse deal handle {}: {e}", path.display()))?;
    let handle = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    validate_deal_handle_schema_version(&handle, probe.version)?;
    let mut value: serde_json::Value = serde_json::from_str(&s)
        .map_err(|e| anyhow::anyhow!("parse deal handle {}: {e}", path.display()))?;
    let object = value.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "parse deal handle {}: expected a JSON object",
            path.display()
        )
    })?;
    let last_observed_promotion = object
        .remove(LAST_OBSERVED_PROMOTION_FIELD)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| {
            anyhow::anyhow!(
                "parse deal handle {} field {LAST_OBSERVED_PROMOTION_FIELD}: {e}",
                path.display()
            )
        })?
        .flatten();
    let h: DealHandle = serde_json::from_value(value)
        .map_err(|e| anyhow::anyhow!("parse deal handle {}: {e}", path.display()))?;
    validate_deal_handle(&h)?;
    Ok(DealRecord {
        handle: h,
        last_observed_promotion,
    })
}

pub(crate) fn persist_last_observed_promotion(
    path: &Path,
    observation: LastObservedPromotion,
) -> Result<()> {
    let mut record = load_deal_record(path)?;
    if record.handle.version == DEAL_HANDLE_VERSION
        && record.last_observed_promotion == Some(observation)
    {
        return Ok(());
    }
    record.handle.version = DEAL_HANDLE_VERSION;
    let bytes = serialize_deal_record(&record.handle, Some(observation))?;
    write_private_atomic(path, &bytes)
}

/// Does this refusal carry the one class the sweep may NOT skip?

/// `load_deal_record` already tells its refusals apart, and has since: the schema check
/// returns a typed [`DealHandleSchemaTooNew`], while every other failure is a plain
/// `parse deal handle {path}:...`. Nothing new is introduced here -- this reads the distinction
/// that already exists, the same way the regressions read it (`downcast_ref` over the cause
/// chain). Kept as a named predicate so the sweep's arm states which class it is holding back
/// rather than open-coding a downcast in a match guard.
fn deal_handle_schema_is_too_new(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<DealHandleSchemaTooNew>().is_some())
}

/// The line a skipped handle prints, as ONE fact with one owner.

/// The path is the whole point: a sweep that silently dropped a file would turn a stale record into
/// an invisible one, and the operator would have no way to find what to remove. Kept as a function
/// so the naming is asserted directly instead of through a captured stderr.
fn skipped_handle_warning(path: &Path, error: &anyhow::Error) -> String {
    format!(
        "warning: skipping unreadable deal handle {}: {error}",
        path.display()
    )
}

pub(crate) fn list_deal_handles(dir: &Path) -> Result<Vec<(PathBuf, DealHandle)>> {
    let mut out = Vec::new();
    // the reason the sweep skipped something, kept in case it ends up with nothing to
    // return. The FIRST is kept rather than the last: `read_dir` order is arbitrary, so "the last
    // one that happened to be read" is not a property worth reporting, while "the first thing that
    // went wrong" at least reads the same way twice on the same directory.
    let mut first_skip: Option<anyhow::Error> = None;
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("read deals dir {}: {e}", dir.display()))?
    {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json")
            || !p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.starts_with("deal-"))
        {
            continue;
        }
        match load_deal_handle(&p) {
            Ok(handle) => out.push((p.clone(), handle)),
            // A handle this scan merely CAME ACROSS is not a reason to kill the command. The deals
            // directory accumulates across generations, and one file written by an older client --
            // a market whose prices predate whole-SHELL quoting, say -- used to abort every command
            // that enumerates handles, including `dexdo seller` startup, for a deal it had nothing
            // to do with.

            // This deliberately does NOT soften an explicitly named handle: `resolve_deal_ref`
            // loads a direct path and a by-id path through `load_deal_handle` before it ever gets
            // here, so asking for a broken handle BY NAME still fails loudly. Only the incidental
            // sweep skips, and it says which file it skipped so the cause is never invisible.

            // EXCEPT a record written by a NEWER runtime, which is not "unreadable" and is
            // not this client's to skip. Every other refusal here says the file is beyond us --
            // garbage, or a value we will not read (the live case was a market price quoted
            // in the units of an older generation). A schema-too-new record says the OPPOSITE: the
            // file is fine and WE are behind it, and the only safe answer is to stop and say so.
            // Skipping it turns "keep the older runtime pinned until that deal terminates" into a
            // deal that silently is not there, on the money path, which is what exists to
            // prevent. It refuses whether or not a healthy handle sits beside it: a neighbour
            // cannot tell the operator what this record needs them to know.
            Err(error) if deal_handle_schema_is_too_new(&error) => return Err(error),
            Err(error) => {
                tracing::warn!(
                    path = %p.display(),
                    error = %error,
                    "skipping unreadable deal handle"
                );
                eprintln!("{}", skipped_handle_warning(&p, &error));
                if first_skip.is_none() {
                    first_skip = Some(error);
                }
            }
        }
    }
    // an empty result is only an empty result when the directory really had nothing for us.

    // The split is not by error CLASS but by whether anything survived. While a readable handle
    // remains, a stale neighbour is noise and skipping it is what is for. When every candidate
    // was skipped there is nobody left to carry the news, and returning `Ok(vec![])` tells the
    // caller "no deals" about a directory that in fact holds one it could not read -- which on the
    // buyer's resume path is a deal that silently is not there.

    // So the caller gets the reason instead, with its own class intact: a schema-too-new keeps its
    // typed cause and classifies as DEAL_RECORD_SCHEMA_TOO_NEW, a malformed record stays malformed,
    // a read error stays a read error. A directory that genuinely holds no handles still returns an
    // empty list -- nothing was skipped, so nothing is being hidden, and that is not a refusal.
    if out.is_empty() {
        if let Some(error) = first_skip {
            return Err(error);
        }
    }
    out.sort_by_key(|entry| entry.1.created_at_unix);
    Ok(out)
}

/// Every handle in `dir`, and the FIRST unreadable one as an error instead of a warning.

/// The difference from [`list_deal_handles`] is who is asking, not what is on disk.

/// That sweep is lenient on purpose: a deals directory accumulates across generations, and
/// one handle written by an older client used to abort every command that merely enumerates
/// handles -- measured live, a handle from 1 August took the 4.0.36 seller gate down for a deal it
/// had nothing to do with. Listing is not deciding, so listing skips and says what it skipped.

/// A buyer resume IS deciding, and it decides about money. It asks "is there a record of a deal I
/// already have?", and a skipped record answers "no" -- whereupon the buyer places a NEW order
/// beside the one it could not read. That is the second order the durable handle exists to
/// prevent, and it is why the three ways a record can be unusable have to stay distinct
/// and loud: a schema from a future client, a record that cannot be read, and a record that does
/// not parse are three different things an operator does three different things about.
pub(crate) fn list_deal_handles_strict(
    dir: &std::path::Path,
) -> Result<Vec<(std::path::PathBuf, DealHandle)>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("read deals dir {}: {e}", dir.display()))?
    {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json")
            || !p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.starts_with("deal-"))
        {
            continue;
        }
        out.push((p.clone(), load_deal_handle(&p)?));
    }
    out.sort_by_key(|entry| entry.1.created_at_unix);
    Ok(out)
}

pub(crate) fn resolve_deal_ref(
    input: &str,
    dir: &Path,
    explicit_role: Option<DealHandleRole>,
    explicit_note_addr: Option<&str>,
) -> Result<Option<(PathBuf, DealHandle)>> {
    let direct = Path::new(input);
    if direct.exists() {
        let handle = load_deal_handle(direct)?;
        validate_explicit_handle_args(&handle, explicit_role, explicit_note_addr)?;
        return Ok(Some((direct.to_path_buf(), handle)));
    }
    let by_handle = handle_path(dir, input);
    if by_handle.exists() {
        let handle = load_deal_handle(&by_handle)?;
        validate_explicit_handle_args(&handle, explicit_role, explicit_note_addr)?;
        return Ok(Some((by_handle, handle)));
    }

    let handles = list_deal_handles(dir)?;
    let mut by_id = handles.iter().filter(|(_, handle)| handle.handle == input);
    if let Some((path, handle)) = by_id.next() {
        if by_id.next().is_some() {
            bail!("deal handle id `{input}` is ambiguous; pass an exact handle path");
        }
        validate_explicit_handle_args(handle, explicit_role, explicit_note_addr)?;
        return Ok(Some((path.clone(), handle.clone())));
    }

    let wanted = normalize_addr(input);
    let matches = handles
        .into_iter()
        .filter(|(_, handle)| normalize_addr(&handle.token_contract) == wanted)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Ok(None);
    }

    let Some(role) = explicit_role else {
        if matches.len() > 1 {
            bail!(
                "deal reference `{input}` is ambiguous: multiple local handles match this TokenContract; pass --role buyer|seller"
            );
        }
        let (path, handle) = matches.into_iter().next().expect("one handle");
        validate_explicit_handle_args(&handle, None, explicit_note_addr)?;
        return Ok(Some((path, handle)));
    };

    let role_matches = matches
        .into_iter()
        .filter(|(_, handle)| handle.role == role)
        .collect::<Vec<_>>();
    if role_matches.is_empty() {
        return Ok(None);
    }
    let matching = match explicit_note_addr {
        Some(note_addr) => role_matches
            .iter()
            .filter(|(_, handle)| normalize_addr(&handle.note_addr) == normalize_addr(note_addr))
            .cloned()
            .collect::<Vec<_>>(),
        None => role_matches.clone(),
    };
    if matching.is_empty() {
        let (_, handle) = &role_matches[0];
        bail!(
            "--note-addr {} does not match handle {} note {}",
            explicit_note_addr.expect("note filter"),
            handle.handle,
            handle.note_addr
        );
    }
    if matching.len() > 1 {
        let (_, first) = &matching[0];
        let same_logical_side = matching.iter().all(|(_, handle)| {
            normalize_addr(&handle.token_contract) == normalize_addr(&first.token_contract)
                && handle.role == first.role
                && normalize_addr(&handle.note_addr) == normalize_addr(&first.note_addr)
        });
        if same_logical_side {
            if let Some((path, handle)) = matching.iter().find(|(_, handle)| {
                handle.handle == make_handle_id(&handle.token_contract, handle.role)
            }) {
                return Ok(Some((path.clone(), handle.clone())));
            }
        }
        bail!(
            "deal reference `{input}` is ambiguous for role {}; pass an exact handle path",
            role.as_str()
        );
    }
    Ok(matching.into_iter().next())
}

fn validate_explicit_handle_args(
    handle: &DealHandle,
    explicit_role: Option<DealHandleRole>,
    explicit_note_addr: Option<&str>,
) -> Result<()> {
    if explicit_role.is_some_and(|role| role != handle.role) {
        bail!(
            "--role {} does not match handle {} role {}",
            explicit_role.expect("mismatched role").as_str(),
            handle.handle,
            handle.role.as_str()
        );
    }
    if explicit_note_addr
        .is_some_and(|note_addr| normalize_addr(note_addr) != normalize_addr(&handle.note_addr))
    {
        bail!(
            "--note-addr {} does not match handle {} note {}",
            explicit_note_addr.expect("mismatched note"),
            handle.handle,
            handle.note_addr
        );
    }
    Ok(())
}

fn same_deal_side(left: &DealHandle, right: &DealHandle) -> bool {
    left.role == right.role
        && normalize_addr(&left.token_contract) == normalize_addr(&right.token_contract)
        && normalize_addr(&left.note_addr) == normalize_addr(&right.note_addr)
}

fn serialize_deal_record(
    handle: &DealHandle,
    last_observed_promotion: Option<LastObservedPromotion>,
) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(handle)?;
    value
        .as_object_mut()
        .expect("DealHandle serializes as a JSON object")
        .insert(
            LAST_OBSERVED_PROMOTION_FIELD.to_string(),
            serde_json::to_value(last_observed_promotion)?,
        );
    Ok(serde_json::to_vec_pretty(&value)?)
}

pub(crate) fn now_unix() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

pub(crate) fn normalize_addr(s: &str) -> String {
    dexdo_core::normalize_wallet_address(s).unwrap_or_else(|_| s.trim().to_ascii_lowercase())
}

pub(crate) fn validate_deal_handle(h: &DealHandle) -> Result<()> {
    validate_deal_handle_schema_version(&h.handle, h.version)?;
    if h.handle.trim().is_empty() {
        bail!("deal handle has empty handle id");
    }
    if h.token_contract.trim().is_empty() {
        bail!("deal handle {} has empty token_contract", h.handle);
    }
    if h.note_addr.trim().is_empty() {
        bail!("deal handle {} has empty note_addr", h.handle);
    }
    if let Some(market) = &h.market {
        market
            .validate()
            .map_err(|e| anyhow::anyhow!("deal handle {} market: {e}", h.handle))?;
        if normalize_addr(&market.token_contract) != normalize_addr(&h.token_contract) {
            bail!(
                "deal handle {} market token_contract {} != handle token_contract {}",
                h.handle,
                dexdo_core::address::display_self_dapp(&market.token_contract),
                dexdo_core::address::display_self_dapp(&h.token_contract)
            );
        }
    }
    let json = serde_json::to_value(h)?;
    if let Some(field) = first_secret_field_name(&json, "") {
        bail!(
            "deal handle {} contains forbidden secret-bearing field `{field}`",
            h.handle
        );
    }
    Ok(())
}

fn validate_deal_handle_schema_version(handle: &str, version: u32) -> Result<()> {
    if version > DEAL_HANDLE_VERSION {
        return Err(anyhow::Error::new(DealHandleSchemaTooNew {
            handle: handle.to_string(),
            record_version: version,
            max_supported_version: DEAL_HANDLE_VERSION,
        }));
    }
    Ok(())
}

fn first_secret_field_name(value: &serde_json::Value, path: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let field = if path.is_empty() {
                    k.to_string()
                } else {
                    format!("{path}.{k}")
                };
                if is_secret_field_name(k) {
                    return Some(field);
                }
                if let Some(found) = first_secret_field_name(v, &field) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| first_secret_field_name(v, &format!("{path}[{i}]"))),
        _ => None,
    }
}

fn is_secret_field_name(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    matches!(
        key.as_str(),
        "secret"
            | "seed"
            | "mnemonic"
            | "owner_key"
            | "note_key"
            | "private_key"
            | "priv_key"
            | "multisig_private_key"
    ) || key.contains("secret")
        || key.ends_with("_seed")
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("deal.json");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_nanos();
    let tmp = dir.join(format!(".{name}.tmp.{}.{nanos}", std::process::id()));
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, READ_CONTROL, WRITE_DAC};
        opts.access_mode(GENERIC_WRITE | READ_CONTROL | WRITE_DAC);
        opts.share_mode(FILE_SHARE_DELETE);
    }
    let mut f = opts
        .open(&tmp)
        .map_err(|e| anyhow::anyhow!("create temp handle {}: {e}", tmp.display()))?;
    #[cfg(windows)]
    if let Err(error) = crate::cli::windows_secret_file::protect_owner_only(&f, &tmp) {
        drop(f);
        return match std::fs::remove_file(&tmp) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(anyhow::anyhow!(
                "{error}; remove empty temp handle {} after ACL failure: {cleanup_error}",
                tmp.display()
            )),
        };
    }
    if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("write temp handle {}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("rename {} -> {}: {e}", tmp.display(), path.display())
    })
}

mod decimal_u128 {
    use serde::{de::Error as _, Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_market() -> MarketManifest {
        MarketManifest {
            network: "net-a".into(),
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: dexdo_core::model_hash_for("qwen/qwen3-32b"),
            inference_order_book: "0:11".into(),
            root_model: "0:22".into(),
            token_contract: "0:33".into(),
            seller_note: "0:44".into(),
            nonce: 7,
            // Whole SHELL a tick: the manifest carries prices in SHELL, and the book holds no
            // other kind.
            price_per_tick: 1000 * dexdo_core::PRICE_STEP,
            max_ticks: 1024,
        }
    }

    fn sample_handle() -> DealHandle {
        DealHandle {
            version: DEAL_HANDLE_VERSION,
            handle: make_handle_id("0:33", DealHandleRole::Seller),
            role: DealHandleRole::Seller,
            network: "net-a".into(),
            token_contract: "0:33".into(),
            note_addr: "0:44".into(),
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
            order_book: Some("0:11".into()),
            root_model: Some("0:22".into()),
            market: Some(sample_market()),
            contracts: "manifest/deployed.manifest.json".into(),
            endpoint: Some(DealEndpointInfo {
                kind: "gateway".into(),
                value: "127.0.0.1:8443".into(),
            }),
            created_order_ids: vec![],
            created_at_unix: 1,
        }
    }

    /// Issue: a deal handle is written with canonical `<dapp_id>::<account_id>` addresses and is
    /// read back from either that or a legacy `0:<account_id>` file, so an existing deals dir keeps
    /// resolving to the same handle after the upgrade.

    /// The DApp half is role-specific. A per-deal `TokenContract` is a self-DApp account - its own
    /// `info.dapp_id` IS its account id - so it is written `<account_id>::<account_id>`. `note_addr`
    /// and `order_book` are system contracts of the shared dexdo DApp.
    #[test]
    fn deal_handle_writes_canonical_addresses_and_reads_legacy_ones() {
        let account = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
        let mut handle = sample_handle();
        handle.token_contract = format!("0:{}", account('3'));
        handle.note_addr = format!("0:{}", account('4'));
        handle.order_book = Some(format!("0:{}", account('1')));
        handle.root_model = None;

        let json = serde_json::to_string(&handle).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for (field, dapp, c) in [
            ("token_contract", account('3'), '3'),
            ("note_addr", dexdo_core::DEXDO_DAPP_ID.to_string(), '4'),
            ("order_book", dexdo_core::DEXDO_DAPP_ID.to_string(), '1'),
        ] {
            assert_eq!(
                v[field].as_str().unwrap(),
                format!("{dapp}::{}", account(c)),
                "{field} was not written canonically"
            );
        }
        assert!(
            v["root_model"].is_null(),
            "an absent address became a value"
        );

        // The canonical file and the legacy file it replaces load to the same handle. Both DApp
        // halves are stripped, so the fixture is the pre- file rather than a half-migrated one.
        let legacy_json = json
            .replace(&format!("{}::", dexdo_core::DEXDO_DAPP_ID), "0:")
            .replace(&format!("{}::", account('3')), "0:");
        assert!(
            !legacy_json.contains("::"),
            "the legacy fixture still carries a DApp half: {legacy_json}"
        );
        assert_ne!(legacy_json, json);
        assert_eq!(
            serde_json::from_str::<DealHandle>(&legacy_json).unwrap(),
            handle
        );
        assert_eq!(serde_json::from_str::<DealHandle>(&json).unwrap(), handle);
    }

    #[test]
    fn deal_handle_roundtrip_carries_no_secret_markers() {
        let h = sample_handle();
        validate_deal_handle(&h).unwrap();
        let json = serde_json::to_string(&h).unwrap();
        assert!(!json.contains("note_key"), "{json}");
        assert!(!json.to_ascii_lowercase().contains("secret"), "{json}");
        let parsed: DealHandle = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, h);
    }

    #[cfg(unix)]
    #[test]
    fn deal_handle_file_remains_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = save_deal_handle(temp.path(), &sample_handle()).unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "deal handle must remain 0600");
    }

    #[test]
    fn deal_handle_allows_public_paths_with_private_words() {
        let mut h = sample_handle();
        h.contracts = "/tmp/private-inference/manifest/deployed.manifest.json".into();
        validate_deal_handle(&h).unwrap();
    }

    #[test]
    fn deal_handle_rejects_unknown_secret_fields() {
        let mut v = serde_json::to_value(sample_handle()).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("note_key".into(), serde_json::json!("/tmp/note.key"));
        let err = serde_json::from_value::<DealHandle>(v).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn deal_state_classification_distinguishes_lifecycle_states() {
        let ordinary_bond = dexdo_core::DealBuyerBond {
            bond_held: 0,
            bond_required: 0,
        };
        fn state(
            funded: bool,
            opened: bool,
            probe_accepted: bool,
            disputed: bool,
            deposit: u128,
            probe_tick: u128,
        ) -> serde_json::Value {
            serde_json::json!({
                "funded": funded,
                "opened": opened,
                "probeAccepted": probe_accepted,
                "disputed": disputed,
                "deposit": deposit.to_string(),
                "probeTick": probe_tick.to_string(),
                "finalizedOwed": "0",
                "tokensFinal": "0",
                "tokensPending": "0",
                "probeTime": "0",
                "lastClaimTime": "0",
                "disputeTime": "0",
                "fundedTime": "0"
            })
        }

        let st = state(false, false, false, false, 0, 0);
        assert_eq!(
            classify_deal_state(&st, ordinary_bond).unwrap().kind,
            DealStateKind::Placed
        );
        let st = state(true, false, false, false, 10, 0);
        assert_eq!(
            classify_deal_state(&st, ordinary_bond).unwrap().kind,
            DealStateKind::FundedButNeverOpened
        );
        let st = state(true, true, false, false, 10, 1);
        assert_eq!(
            classify_deal_state(&st, ordinary_bond).unwrap().kind,
            DealStateKind::Probe
        );
        let st = state(true, true, true, false, 10, 0);
        assert_eq!(
            classify_deal_state(&st, ordinary_bond).unwrap().kind,
            DealStateKind::Streaming
        );
        let st = state(true, false, true, false, 0, 0);
        assert_eq!(
            classify_deal_state(&st, ordinary_bond).unwrap().kind,
            DealStateKind::Stopped,
            "a post-STOP deal with returned escrow is terminal, not never-opened"
        );
        let st = state(true, false, true, true, 10, 0);
        assert_eq!(
            classify_deal_state(&st, ordinary_bond).unwrap().kind,
            DealStateKind::Disputed
        );

        let mut incomplete = state(true, true, true, false, 10, 0);
        incomplete.as_object_mut().unwrap().remove("tokensPending");
        assert!(
            classify_deal_state(&incomplete, ordinary_bond)
                .unwrap_err()
                .to_string()
                .contains("tokensPending"),
            "an incomplete state must fail closed"
        );

        let overflow = state(true, true, true, false, u128::MAX, 1);
        let summary = classify_deal_state(
            &overflow,
            dexdo_core::DealBuyerBond {
                bond_held: 1,
                bond_required: 1,
            },
        )
        .unwrap();
        assert!(
            summary.buyer_locked().is_err(),
            "deposit + probeTick + buyerBond must use checked uint128 arithmetic"
        );
    }

    #[test]
    fn deal_ref_resolves_by_handle_and_token_contract() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        let h = sample_handle();
        let p = save_deal_handle(base, &h).unwrap();
        assert_eq!(
            resolve_deal_ref(&h.handle, base, None, None)
                .unwrap()
                .unwrap()
                .1,
            h
        );
        assert_eq!(
            resolve_deal_ref("0:33", base, None, None)
                .unwrap()
                .unwrap()
                .0,
            p
        );
    }

    fn buyer_handle() -> DealHandle {
        let mut h = sample_handle();
        h.handle = make_handle_id(&h.token_contract, DealHandleRole::Buyer);
        h.role = DealHandleRole::Buyer;
        h.note_addr = "0:55".into();
        h
    }

    #[test]
    fn buyer_and_seller_handles_for_one_token_contract_coexist() {
        let temp = tempfile::tempdir().unwrap();
        let buyer = buyer_handle();
        let seller = sample_handle();

        let buyer_path = save_deal_handle(temp.path(), &buyer).unwrap();
        let seller_path = save_deal_handle(temp.path(), &seller).unwrap();

        assert_eq!(
            buyer_path.file_name().and_then(|name| name.to_str()),
            Some("deal-0-33-buyer.json")
        );
        assert_eq!(
            seller_path.file_name().and_then(|name| name.to_str()),
            Some("deal-0-33-seller.json")
        );
        assert_eq!(load_deal_handle(&buyer_path).unwrap(), buyer);
        assert_eq!(load_deal_handle(&seller_path).unwrap(), seller);
    }

    #[test]
    fn raw_token_contract_with_explicit_buyer_selects_buyer_after_seller_write() {
        let temp = tempfile::tempdir().unwrap();
        let buyer = buyer_handle();
        save_deal_handle(temp.path(), &buyer).unwrap();
        save_deal_handle(temp.path(), &sample_handle()).unwrap();

        let (_, selected) = resolve_deal_ref(
            "0:33",
            temp.path(),
            Some(DealHandleRole::Buyer),
            Some("0:55"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected, buyer);
    }

    #[test]
    fn explicit_owner_prefers_scoped_duplicate_over_legacy_handle() {
        let temp = tempfile::tempdir().unwrap();
        let scoped = buyer_handle();
        let mut legacy = scoped.clone();
        legacy.token_contract = " 0:33 ".into();
        legacy.note_addr = " 0:55 ".into();
        legacy.handle = make_token_contract_id(&legacy.token_contract);
        save_deal_handle(temp.path(), &legacy).unwrap();
        let scoped_path = save_deal_handle(temp.path(), &scoped).unwrap();

        let (selected_path, selected) = resolve_deal_ref(
            "0:33",
            temp.path(),
            Some(DealHandleRole::Buyer),
            Some("0:55"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(selected_path, scoped_path);
        assert_eq!(selected, scoped);
    }

    #[test]
    fn other_role_handle_does_not_block_explicit_raw_owner() {
        let temp = tempfile::tempdir().unwrap();
        save_deal_handle(temp.path(), &sample_handle()).unwrap();

        assert!(resolve_deal_ref(
            "0:33",
            temp.path(),
            Some(DealHandleRole::Buyer),
            Some("0:55"),
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn raw_token_contract_with_two_roles_is_ambiguous_without_role() {
        let temp = tempfile::tempdir().unwrap();
        save_deal_handle(temp.path(), &buyer_handle()).unwrap();
        save_deal_handle(temp.path(), &sample_handle()).unwrap();

        let err = resolve_deal_ref("0:33", temp.path(), None, None).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err:#}");
        assert!(err.to_string().contains("--role"), "{err:#}");
    }

    #[test]
    fn exact_handle_rejects_conflicting_role_and_note() {
        let temp = tempfile::tempdir().unwrap();
        let seller = sample_handle();
        let path = save_deal_handle(temp.path(), &seller).unwrap();

        let err = resolve_deal_ref(
            path.to_str().unwrap(),
            temp.path(),
            Some(DealHandleRole::Buyer),
            Some(&seller.note_addr),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--role buyer"), "{err:#}");

        let err = resolve_deal_ref(
            &seller.handle,
            temp.path(),
            Some(DealHandleRole::Seller),
            Some("0:66"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--note-addr 0:66"), "{err:#}");
    }

    #[test]
    fn legacy_unscoped_handle_remains_readable() {
        let temp = tempfile::tempdir().unwrap();
        let mut legacy = sample_handle();
        legacy.handle = make_token_contract_id(&legacy.token_contract);
        let path = save_deal_handle(temp.path(), &legacy).unwrap();

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("deal-0-33.json")
        );
        assert_eq!(
            resolve_deal_ref(&legacy.handle, temp.path(), None, None)
                .unwrap()
                .unwrap()
                .1,
            legacy
        );
        assert_eq!(
            resolve_deal_ref("0:33", temp.path(), None, None)
                .unwrap()
                .unwrap()
                .0,
            path
        );
    }

    #[test]
    fn last_observed_promotion_is_persisted_in_the_existing_deal_handle() {
        let temp = tempfile::tempdir().unwrap();
        let path = save_deal_handle(temp.path(), &sample_handle()).unwrap();
        let observed = LastObservedPromotion {
            tokens_final: 2 * dexdo_core::TICK_SIZE,
            tokens_pending: 3 * dexdo_core::TICK_SIZE,
            last_claim_time: 1_754_006_400,
        };

        persist_last_observed_promotion(&path, observed).unwrap();

        let record = load_deal_record(&path).unwrap();
        assert_eq!(record.last_observed_promotion, Some(observed));
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            json["last_observed_promotion"]["tokens_final"],
            serde_json::json!((2 * dexdo_core::TICK_SIZE).to_string())
        );
        assert_eq!(
            json["last_observed_promotion"]["tokens_pending"],
            serde_json::json!((3 * dexdo_core::TICK_SIZE).to_string())
        );
        assert_eq!(
            json["last_observed_promotion"]["last_claim_time"],
            serde_json::json!(1_754_006_400_u64)
        );
    }

    #[test]
    fn absent_observation_and_an_observed_zero_pair_are_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let path = save_deal_handle(temp.path(), &sample_handle()).unwrap();

        let absent = load_deal_record(&path).unwrap();
        assert_eq!(absent.last_observed_promotion, None);
        let absent_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(absent_json["last_observed_promotion"].is_null());

        let zero = LastObservedPromotion {
            tokens_final: 0,
            tokens_pending: 0,
            last_claim_time: 1_754_006_401,
        };
        persist_last_observed_promotion(&path, zero).unwrap();
        let observed = load_deal_record(&path).unwrap();
        assert_eq!(observed.last_observed_promotion, Some(zero));
        assert_ne!(observed.last_observed_promotion, None);
    }
}

#[cfg(test)]
#[path = "deals_stale_handle_1697_tests.rs"]
mod deals_stale_handle_1697_tests;

#[cfg(test)]
#[path = "deals_empty_sweep_1716_tests.rs"]
mod deals_empty_sweep_1716_tests;
