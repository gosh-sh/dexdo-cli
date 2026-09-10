//! `dexdo oracle` command handlers (provision/state/resolve), extracted from `commands.rs`
//! (move-only / behavior-identical, anti-entropy refactor Track C1).

use crate::cli::args::OracleArgs;
use anyhow::{bail, Result};
use dexdo_core::params::{
    ORACLE_FEE_WITHDRAW_CONFIRM_MAX_READS, ORACLE_FEE_WITHDRAW_CONFIRM_POLL_INTERVAL,
    ORACLE_RESOLUTION_MAX_READS, ORACLE_RESOLUTION_POLL_INTERVAL, PMP_EXIT_CONFIRM_MAX_READS,
    PMP_EXIT_CONFIRM_POLL_INTERVAL,
};
// Reachable in a plain `test` build too: the raw-triple resolver is offline logic and its
// regression must run on the gate CI actually executes -- there are no cargo features left.
// A feature-gated test tree rots without anyone seeing it go red.
use dexdo_core::params::SHELL_CURRENCY_ID;

use crate::cli::args::OraclePmpExitArgs;
use crate::cli::args::{
    OracleAddressArgs, OracleBookArgs, OracleBookCommand, OracleBookOrderArgs,
    OracleBookOrdersArgs, OracleBookStatusArgs, OracleCommand, OracleEventListAddressArgs,
    OracleEventListArgs, OracleEventListCommand, OracleEventListEventsArgs, OraclePmpAddressArgs,
    OraclePmpArgs, OraclePmpCommand, OraclePmpStatusArgs, OracleProvisionArgs, OracleResolveArgs,
    OracleStateArgs, OracleWithdrawFeesArgs,
};
use crate::cli::commands::{chain_doctor_preflight, now_unix_secs};
// the destination reads below are account/getter reads on a money path, so they take the same
// retrying readers every other chain read in this client takes -- a transient endpoint hiccup must
// not read as "this destination cannot be proved" and refuse a correct withdrawal.
use crate::cli::support::{load_market, read_secret_hex, require_note_addr};
use dexdo_core::chain::RetryingReads as _;

#[cfg(test)]
#[path = "oracle_exit_1120_tests.rs"]
mod oracle_exit_1120_tests;

#[cfg(test)]
#[path = "oracle_withdraw_destination_1465_tests.rs"]
mod oracle_withdraw_destination_1465_tests;

#[cfg(test)]
#[path = "oracle_declared_destination_1580_tests.rs"]
mod oracle_declared_destination_1580_tests;

#[cfg(test)]
#[path = "oracle_forfeit_1734_tests.rs"]
mod oracle_forfeit_1734_tests;
#[cfg(test)]
#[path = "oracle_raw_triple_1553_tests.rs"]
mod oracle_raw_triple_1553_tests;

const ORACLE_MIN_RESULT_GAP_SECS: u64 = 120;

fn load_oracle_market_manifest(path: &std::path::Path) -> Result<dexdo_core::OracleMarketManifest> {
    let json = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read --manifest {}: {e}", path.display()))?;
    let manifest = dexdo_core::OracleMarketManifest::from_json(&json)
        .map_err(|e| anyhow::anyhow!("parse --manifest {}: {e}", path.display()))?;
    manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("--manifest {}: {e}", path.display()))?;
    if manifest.token_type != SHELL_CURRENCY_ID {
        bail!(
            "--manifest {}: token_type {} is unsupported; dexdo markets require SHELL currency id {}",
            path.display(),
            manifest.token_type,
            SHELL_CURRENCY_ID
        );
    }
    Ok(manifest)
}

fn pmp_resolved_outcome(details: &serde_json::Value) -> Option<String> {
    let v = &details["resolvedOutcome"];
    if v.is_null() {
        return None;
    }
    v.as_str()
        .map(str::to_string)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| {
            v.as_object()
                .and_then(|o| o.get("value").or_else(|| o.get("0")))
                .and_then(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .or_else(|| x.as_u64().map(|n| n.to_string()))
                })
        })
}

fn validate_oracle_deadline(deadline: u64, now: u64) -> Result<()> {
    let min_deadline = now.saturating_add(ORACLE_MIN_RESULT_GAP_SECS);
    if deadline < min_deadline {
        bail!(
            "oracle provision: --deadline {deadline} must be at least {ORACLE_MIN_RESULT_GAP_SECS}s \
             in the future for OracleEventList.addRangeEvent (now={now}, min={min_deadline})"
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PmpExitAction {
    CancelStake,
    Claim,
    ForfeitStake,
}

impl PmpExitAction {
    fn command(self) -> &'static str {
        match self {
            Self::CancelStake => "oracle cancel-stake",
            Self::Claim => "oracle claim",
            Self::ForfeitStake => "oracle forfeit-stake",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PmpExitObservation {
    stake_present: bool,
    candidate_amount: u128,
    amount_slots: usize,
    open_orders: u32,
    busy_address: Option<String>,
    has_withdrawn: bool,
    note_balance: u128,
    coupons_value: u128,
}

fn parse_pmp_exit_observation(value: &serde_json::Value) -> Result<PmpExitObservation> {
    let required_bool = |field: &str| {
        value[field]
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("PrivateNote exit snapshot exposes no boolean {field}"))
    };
    let required_u128 = |field: &str| {
        oracle_u128(value, field)
            .ok_or_else(|| anyhow::anyhow!("PrivateNote exit snapshot exposes no uint {field}"))
    };
    Ok(PmpExitObservation {
        stake_present: required_bool("stake_present")?,
        candidate_amount: required_u128("candidate_amount")?,
        amount_slots: usize::try_from(required_u128("amount_slots")?)
            .map_err(|_| anyhow::anyhow!("PrivateNote exit snapshot amount_slots exceeds usize"))?,
        open_orders: u32::try_from(required_u128("open_orders")?)
            .map_err(|_| anyhow::anyhow!("PrivateNote exit snapshot open_orders exceeds uint32"))?,
        busy_address: value["busy_address"]
            .as_str()
            .filter(|address| !address.trim().is_empty())
            .map(|address| address.trim().to_string()),
        has_withdrawn: required_bool("has_withdrawn")?,
        note_balance: required_u128("note_balance")?,
        coupons_value: required_u128("coupons_value")?,
    })
}

fn oracle_bool(value: &serde_json::Value, field: &str) -> Option<bool> {
    value[field]
        .as_bool()
        .or_else(|| match value[field].as_str()? {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        })
}

fn validate_pmp_exit_preflight(
    action: PmpExitAction,
    pmp: &serde_json::Value,
    shutdown: &serde_json::Value,
    note: &PmpExitObservation,
    chain_now_secs: u64,
) -> Result<()> {
    let command = action.command();
    if !note.stake_present {
        bail!("{command}: this note has no stake for the manifest PMP tuple");
    }
    if note.has_withdrawn {
        bail!("{command}: the PrivateNote is withdrawn");
    }
    if let Some(busy) = note.busy_address.as_deref() {
        bail!("{command}: the PrivateNote is busy with {busy}");
    }
    if note.open_orders != 0 {
        bail!(
            "{command}: the PrivateNote still has {} open order(s) for this event",
            note.open_orders
        );
    }
    if note.candidate_amount != 0 {
        bail!(
            "{command}: the PrivateNote stake has pending candidate amount {}",
            note.candidate_amount
        );
    }

    let order_book_done = oracle_bool(shutdown, "orderBookDone").ok_or_else(|| {
        anyhow::anyhow!("{command}: PMP getShutdownState exposes no orderBookDone")
    })?;
    match action {
        PmpExitAction::CancelStake => {
            // `PMP.cancelStake` makes `_isCancelled` true INSIDE ITSELF: past the grace period, on
            // an unresolved event, it calls `cancelEvent()` before its own
            // `require(_isCancelled, ERR_NOT_CANCELLED)`. So that require has TWO ways to hold --
            // the event was already cancelled, or this very call cancels it. A preflight that
            // knows only the first refuses the exact call that would produce the second, and the
            // money never moves:, where a 0.02 SHELL stake held 93.905 SHELL for that
            // reason alone.

            // This is not a weakened gate. It is the contract's own condition, so the refusal
            // still lands wherever the call would truly revert -- before the gas, not after it.

            // The deadline is READ FROM THE CHAIN rather than rebuilt from a client-side
            // GRACE_PERIOD: `getDetails` already returns `resultEnd` as `_resultStart +
            // GRACE_PERIOD`, so the client never keeps a second copy of that constant to drift
            // away from the deployed one.
            if oracle_bool(pmp, "isCancelled") != Some(true) {
                let result_end = oracle_u128(pmp, "resultEnd").ok_or_else(|| {
                    anyhow::anyhow!("{command}: PMP getDetails exposes no resultEnd")
                })?;
                if u128::from(chain_now_secs) <= result_end {
                    bail!(
                        "{command}: PMP is not cancelled and its grace period has not passed \
                         (chain time {chain_now_secs}, resultEnd {result_end})"
                    );
                }
                if pmp_resolved_outcome(pmp).is_some() {
                    bail!("{command}: PMP is not cancelled and its outcome is already resolved");
                }
            }
            let frozen = oracle_bool(pmp, "frozen")
                .ok_or_else(|| anyhow::anyhow!("{command}: PMP getDetails exposes no frozen"))?;
            if frozen && !order_book_done {
                bail!("{command}: PMP OrderBook shutdown is not complete");
            }
        }
        // `PMP.forfeitStake` gates on the SENDER and nothing else -- no `_isCancelled`, no
        // `_orderBookDone`, no approval. That absence is the whole reason this command exists, so
        // there is deliberately nothing to check here: adding a lifecycle condition would rebuild
        // the wall `cancel-stake` is stuck behind. The note-side checks above still apply, because
        // `PrivateNote.deleteStake` really does require them.
        PmpExitAction::ForfeitStake => {}
        PmpExitAction::Claim => {
            if oracle_bool(pmp, "approved") != Some(true) {
                bail!("{command}: PMP is not approved");
            }
            if !order_book_done {
                bail!("{command}: PMP OrderBook shutdown is not complete");
            }
            if pmp_resolved_outcome(pmp).is_none() {
                bail!("{command}: PMP exposes no resolved outcome");
            }
            let outcomes = oracle_u128(pmp, "numOutcomes").ok_or_else(|| {
                anyhow::anyhow!("{command}: PMP getDetails exposes no numOutcomes")
            })?;
            if u128::try_from(note.amount_slots).ok() != Some(outcomes) {
                bail!(
                    "{command}: note stake has {} outcome slots but PMP declares {outcomes}",
                    note.amount_slots
                );
            }
        }
    }
    Ok(())
}

fn pmp_exit_postread_confirmed(note: &PmpExitObservation) -> bool {
    !note.stake_present && note.busy_address.is_none()
}

fn oracle_fee_expected_after(before: u128, amount: u128) -> Result<u128> {
    if amount == 0 {
        bail!("oracle withdraw-fees: --amount must be greater than zero");
    }
    before.checked_sub(amount).ok_or_else(|| {
        anyhow::anyhow!(
            "oracle withdraw-fees: --amount {} SHELL exceeds the live Oracle fee balance of {} SHELL",
            dexdo_core::shell_amount(amount),
            dexdo_core::shell_amount(before)
        )
    })
}

fn oracle_fee_postread_confirmed(expected: u128, observed: u128) -> bool {
    observed == expected
}

pub(crate) async fn run_oracle(args: OracleArgs) -> Result<()> {
    match args.command {
        OracleCommand::Address(a) => run_oracle_address(a).await,
        OracleCommand::EventList(e) => run_oracle_event_list(e).await,
        OracleCommand::Pmp(p) => run_oracle_pmp(p).await,
        OracleCommand::Book(b) => run_oracle_book(b).await,
        OracleCommand::Provision(p) => run_oracle_provision(*p).await,
        OracleCommand::State(s) => run_oracle_state(s).await,
        OracleCommand::Resolve(r) => run_oracle_resolve(r).await,
        OracleCommand::Cancel(c) => run_oracle_cancel(c).await,
        OracleCommand::Delete(d) => run_oracle_delete(d).await,
        OracleCommand::CancelStake(c) => run_oracle_pmp_exit(c, PmpExitAction::CancelStake).await,
        OracleCommand::Claim(c) => run_oracle_pmp_exit(c, PmpExitAction::Claim).await,
        OracleCommand::ForfeitStake(f) => run_oracle_forfeit_stake(f).await,
        OracleCommand::WithdrawFees(w) => run_oracle_withdraw_fees(w).await,
    }
}

fn parse_oracle_read_address(flag: &str, raw: &str) -> Result<dexdo_core::Address> {
    dexdo_core::Address::parse(raw).map_err(|e| anyhow::anyhow!("{flag} {raw}: {e}"))
}

fn oracle_read_chain(contracts: &std::path::Path) -> Result<dexdo_core::RealChainBackend> {
    dexdo_core::RealChainBackend::connect(
        contracts
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )
}

fn required_oracle_getter(
    value: Option<serde_json::Value>,
    kind: &str,
    address: &dexdo_core::Address,
    getter: &str,
) -> Result<serde_json::Value> {
    value.ok_or_else(|| {
        anyhow::anyhow!(
            "{kind} {} {getter} unavailable (inactive or missing)",
            dexdo_core::address::display(&address.with_workchain())
        )
    })
}

fn print_oracle_json(value: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

async fn bound_order_book(
    chain: &dexdo_core::RealChainBackend,
    pmp: &dexdo_core::Address,
) -> Result<dexdo_core::Address> {
    let order_book = chain.pmp_order_book_address(pmp).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "PMP {} exposes no bound OrderBook",
            dexdo_core::address::display(&pmp.with_workchain())
        )
    })?;
    if order_book.bare().bytes().all(|byte| byte == b'0') {
        bail!(
            "PMP {} exposes no bound OrderBook",
            dexdo_core::address::display(&pmp.with_workchain())
        );
    }
    chain
        .assert_order_book_read_identity(pmp, &order_book)
        .await?;
    Ok(order_book)
}

async fn run_oracle_address(args: OracleAddressArgs) -> Result<()> {
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    chain.assert_root_oracle_read_identity().await?;
    let oracle = chain.oracle_address(&args.oracle_name).await?;
    print_oracle_json(serde_json::json!({
        "kind": "oracle_address",
        "oracle_name": args.oracle_name,
        "oracle": dexdo_core::address::display(&oracle.with_workchain()),
    }))
}

async fn run_oracle_event_list(args: OracleEventListArgs) -> Result<()> {
    match args.command {
        OracleEventListCommand::Address(a) => run_oracle_event_list_address(a).await,
        OracleEventListCommand::Events(e) => run_oracle_event_list_events(e).await,
    }
}

async fn run_oracle_event_list_address(args: OracleEventListAddressArgs) -> Result<()> {
    let oracle = parse_oracle_read_address("--oracle", &args.oracle)?;
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    chain.assert_oracle_read_identity(&oracle).await?;
    let event_list = chain.oracle_event_list_address(&oracle, args.index).await?;
    print_oracle_json(serde_json::json!({
        "kind": "oracle_event_list_address",
        "oracle": dexdo_core::address::display(&oracle.with_workchain()),
        "index": args.index.to_string(),
        "event_list": dexdo_core::address::display(&event_list.with_workchain()),
    }))
}

async fn run_oracle_event_list_events(args: OracleEventListEventsArgs) -> Result<()> {
    let event_list = parse_oracle_read_address("--event-list", &args.event_list)?;
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    chain
        .assert_oracle_event_list_read_identity(&event_list)
        .await?;
    let raw = required_oracle_getter(
        chain.oracle_event_list_events(&event_list).await?,
        "OracleEventList",
        &event_list,
        "_events",
    )?;
    let events = raw.get("_events").cloned().unwrap_or(raw);
    print_oracle_json(serde_json::json!({
        "kind": "oracle_events",
        "event_list": dexdo_core::address::display(&event_list.with_workchain()),
        "events": events,
    }))
}

async fn run_oracle_pmp(args: OraclePmpArgs) -> Result<()> {
    match args.command {
        OraclePmpCommand::Address(a) => run_oracle_pmp_address(a).await,
        OraclePmpCommand::Status(s) => run_oracle_pmp_status(s).await,
    }
}

async fn run_oracle_pmp_address(args: OraclePmpAddressArgs) -> Result<()> {
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    chain.assert_root_pn_read_identity().await?;
    let pmp = chain
        .pmp_address(&args.event_id, &args.oracle_names, args.token_type)
        .await?;
    print_oracle_json(serde_json::json!({
        "kind": "pmp_address",
        "event_id": args.event_id,
        "oracle_names": args.oracle_names,
        "token_type": args.token_type,
        "pmp": dexdo_core::address::display(&pmp.with_workchain()),
    }))
}

async fn run_oracle_pmp_status(args: OraclePmpStatusArgs) -> Result<()> {
    let pmp = parse_oracle_read_address("--pmp", &args.pmp)?;
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    chain.assert_pmp_read_identity(&pmp).await?;
    let details =
        required_oracle_getter(chain.pmp_details(&pmp).await?, "PMP", &pmp, "getDetails")?;
    let shutdown_state = required_oracle_getter(
        chain.pmp_shutdown_state(&pmp).await?,
        "PMP",
        &pmp,
        "getShutdownState",
    )?;
    let unclaimed = required_oracle_getter(
        chain.pmp_unclaimed_balance(&pmp).await?,
        "PMP",
        &pmp,
        "getUnclaimedBalance",
    )?;
    let unclaimed_balance = unclaimed
        .get("value0")
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_u64().map(|value| value.to_string()))
        })
        .ok_or_else(|| anyhow::anyhow!("PMP getUnclaimedBalance exposes no uint128 value0"))?;
    let version =
        required_oracle_getter(chain.pmp_version(&pmp).await?, "PMP", &pmp, "getVersion")?;
    let order_book = chain
        .pmp_order_book_address(&pmp)
        .await?
        .and_then(|address| {
            (!address.bare().bytes().all(|byte| byte == b'0'))
                .then(|| dexdo_core::address::display(&address.with_workchain()))
        });
    print_oracle_json(serde_json::json!({
        "kind": "pmp_status",
        "pmp": dexdo_core::address::display(&pmp.with_workchain()),
        "details": details,
        "shutdown_state": shutdown_state,
        "unclaimed_balance": unclaimed_balance,
        "version": version,
        "order_book": order_book,
    }))
}

async fn run_oracle_book(args: OracleBookArgs) -> Result<()> {
    match args.command {
        OracleBookCommand::Status(s) => run_oracle_book_status(s).await,
        OracleBookCommand::Order(o) => run_oracle_book_order(o).await,
        OracleBookCommand::Orders(o) => run_oracle_book_orders(o).await,
    }
}

async fn run_oracle_book_status(args: OracleBookStatusArgs) -> Result<()> {
    let pmp = parse_oracle_read_address("--pmp", &args.pmp)?;
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    let order_book = bound_order_book(&chain, &pmp).await?;
    let details = required_oracle_getter(
        chain.order_book_details(&order_book).await?,
        "OrderBook",
        &order_book,
        "getDetails",
    )?;
    let queue_size = required_oracle_getter(
        chain.order_book_queue_size(&order_book).await?,
        "OrderBook",
        &order_book,
        "getQueueSize",
    )?;
    let shutdown_state = required_oracle_getter(
        chain.order_book_shutdown_state(&order_book).await?,
        "OrderBook",
        &order_book,
        "getShutdownState",
    )?;
    let version = required_oracle_getter(
        chain.order_book_version(&order_book).await?,
        "OrderBook",
        &order_book,
        "getVersion",
    )?;
    print_oracle_json(serde_json::json!({
        "kind": "order_book_status",
        "pmp": dexdo_core::address::display(&pmp.with_workchain()),
        "order_book": dexdo_core::address::display(&order_book.with_workchain()),
        "details": details,
        "queue_size": queue_size,
        "shutdown_state": shutdown_state,
        "version": version,
    }))
}

async fn run_oracle_book_order(args: OracleBookOrderArgs) -> Result<()> {
    let pmp = parse_oracle_read_address("--pmp", &args.pmp)?;
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    let order_book = bound_order_book(&chain, &pmp).await?;
    let order = required_oracle_getter(
        chain.order_book_order(&order_book, args.order_id).await?,
        "OrderBook",
        &order_book,
        "getOrder",
    )?;
    print_oracle_json(serde_json::json!({
        "kind": "order_book_order",
        "pmp": dexdo_core::address::display(&pmp.with_workchain()),
        "order_book": dexdo_core::address::display(&order_book.with_workchain()),
        "order_id": args.order_id.to_string(),
        "order": order,
    }))
}

async fn run_oracle_book_orders(args: OracleBookOrdersArgs) -> Result<()> {
    let pmp = parse_oracle_read_address("--pmp", &args.pmp)?;
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    let order_book = bound_order_book(&chain, &pmp).await?;
    let orders = required_oracle_getter(
        chain
            .order_book_orders_by_owner(&order_book, &args.deposit_hash)
            .await?,
        "OrderBook",
        &order_book,
        "getOrdersByOwner",
    )?;
    print_oracle_json(serde_json::json!({
        "kind": "order_book_orders_by_owner",
        "pmp": dexdo_core::address::display(&pmp.with_workchain()),
        "order_book": dexdo_core::address::display(&order_book.with_workchain()),
        "deposit_hash": args.deposit_hash,
        "orders": orders,
    }))
}

async fn run_oracle_provision(args: OracleProvisionArgs) -> Result<()> {
    use dexdo_core::{KeyPair, RealChainBackend};
    if args.outcome_names.len() != args.bounds.len() + 1 {
        bail!(
            "oracle provision: pass exactly bounds.len()+1 --outcome values (got {}, expected {})",
            args.outcome_names.len(),
            args.bounds.len() + 1
        );
    }
    if args.initial_stakes.len() != args.outcome_names.len() {
        bail!(
            "oracle provision: pass exactly one --initial-stake per outcome (got {}, expected {})",
            args.initial_stakes.len(),
            args.outcome_names.len()
        );
    }
    validate_oracle_deadline(args.deadline, now_unix_secs()?)?;
    chain_doctor_preflight(
        &crate::cli::commands::manifest_path()?,
        Some(args.market.as_path()),
    )
    .await?;

    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("oracle provision: --note-addr (PMP deployer PrivateNote) is required")
    })?;
    let note_seed = crate::cli::support::note_owner_secret_for(
        args.identity.note_key.as_deref(),
        &note_addr,
        None,
        "oracle provision",
        "the deployer note's owner key",
    )?;
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let contracts_path = crate::cli::commands::manifest_path()?;
    let contracts = contracts_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            contracts_path.display()
        )
    })?;
    let market = load_market(&args.market)?;
    let oracle_seed = read_secret_hex(&args.oracle_key, "--oracle-key")?;
    let note_keys = KeyPair::from_secret_hex(note_seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let oracle_keys = KeyPair::from_secret_hex(oracle_seed.trim())
        .map_err(|e| anyhow::anyhow!("--oracle-key (SDK secret hex): {e:?}"))?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let chain = RealChainBackend::connect(contracts)?;
    let manifest = chain
        .provision_oracle_market(
            &note_keys,
            &note,
            &oracle_keys,
            &args.oracle_name,
            args.event_list_index,
            &args.event_list_description,
            &args.event_name,
            args.oracle_fee,
            args.deadline,
            &args.describe,
            &args.bounds,
            &args.outcome_names,
            &market,
            args.token_type,
            &args.initial_stakes,
        )
        .await?;
    let json = manifest.to_json()?;
    std::fs::write(&args.output, &json)
        .map_err(|e| anyhow::anyhow!("write --output {}: {e}", args.output.display()))?;
    println!("oracle market provisioned -> {}", args.output.display());
    println!("{json}");
    Ok(())
}

async fn run_oracle_state(args: OracleStateArgs) -> Result<()> {
    use dexdo_core::{Address, RealChainBackend};
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    chain_doctor_preflight(&crate::cli::commands::manifest_path()?, None).await?;
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let contracts_path = crate::cli::commands::manifest_path()?;
    let contracts = contracts_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            contracts_path.display()
        )
    })?;
    let chain = RealChainBackend::connect(contracts)?;
    let oel = Address::parse(&manifest.oracle_event_list)
        .map_err(|e| anyhow::anyhow!("oracle_event_list {}: {e}", manifest.oracle_event_list))?;
    let pmp =
        Address::parse(&manifest.pmp).map_err(|e| anyhow::anyhow!("pmp {}: {e}", manifest.pmp))?;
    let pmp_display = dexdo_core::address::display(&manifest.pmp);
    let inference_ob_display = dexdo_core::address::display(&manifest.inference_order_book);
    let range = chain.oracle_range_data(&oel, &manifest.event_id).await?;
    let details = chain.pmp_details(&pmp).await?;
    let pmp_ob = chain.pmp_order_book_address(&pmp).await?;
    println!(
        "oracle_state event={} pmp={} token_type={} deadline={} frame_model={} inference_ob={}",
        manifest.event_id,
        pmp_display,
        manifest.token_type,
        manifest.deadline,
        manifest.frame_model,
        inference_ob_display
    );
    match range {
        Some(r) => println!("range_data={}", serde_json::to_string(&r)?),
        None => println!("range_data=<inactive-or-missing>"),
    }
    match details {
        Some(d) => {
            let resolved = pmp_resolved_outcome(&d).unwrap_or_else(|| "none".to_string());
            println!(
                "pmp_details approved={} approved_oracles={}/{} resolved_outcome={} raw={}",
                d["approved"].as_bool().unwrap_or(false),
                d["approvedOracleEvents"].as_str().unwrap_or("0"),
                d["numberOfOracleEvents"].as_str().unwrap_or("0"),
                resolved,
                serde_json::to_string(&d)?
            );
        }
        None => println!("pmp_details=<inactive-or-missing>"),
    }
    if let Some(ob) = pmp_ob {
        println!(
            "pmp_order_book={}",
            dexdo_core::address::display(&ob.with_workchain())
        );
    }
    Ok(())
}

async fn run_oracle_resolve(args: OracleResolveArgs) -> Result<()> {
    use dexdo_core::{Address, KeyPair, RealChainBackend};
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    let now = now_unix_secs()?;
    if now < manifest.deadline {
        bail!(
            "oracle resolve: deadline not reached (deadline={}, now={now})",
            manifest.deadline
        );
    }
    chain_doctor_preflight(&crate::cli::commands::manifest_path()?, None).await?;
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let contracts_path = crate::cli::commands::manifest_path()?;
    let contracts = contracts_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            contracts_path.display()
        )
    })?;
    let chain = RealChainBackend::connect(contracts)?;
    let oel = Address::parse(&manifest.oracle_event_list)
        .map_err(|e| anyhow::anyhow!("oracle_event_list {}: {e}", manifest.oracle_event_list))?;
    let pmp =
        Address::parse(&manifest.pmp).map_err(|e| anyhow::anyhow!("pmp {}: {e}", manifest.pmp))?;
    let pmp_display = dexdo_core::address::display(&manifest.pmp);
    let oracle_seed = read_secret_hex(&args.oracle_key, "--oracle-key")?;
    let oracle_keys = KeyPair::from_secret_hex(oracle_seed.trim())
        .map_err(|e| anyhow::anyhow!("--oracle-key (SDK secret hex): {e:?}"))?;
    // Liquidity preflight. `resolveRange` makes the PMP ask its bound InferenceOrderBook for the
    // weekly median, and that book answers through `requestWeeklyMedian`
    // (`contracts/airegistry/InferenceOrderBook.sol:1759`) with `bounce: false`. Its
    // `_weeklyMedian()` reverts with `ERR_NO_LIQUIDITY = 334` (`:1738`, the constant declared at
    // `:116`) while the week's matched volume is below `MIN_LIQUIDITY` (`:237`), and under
    // `bounce: false` that revert never comes back: the resolve is paid for and the PMP stays
    // unresolved. `getWeeklyMedianPrice()` (`:1749`) is the same `_weeklyMedian()` exposed as a
    // public getter, so read it first and send nothing at all when it does not answer.
    let order_book = chain.pmp_order_book_address(&pmp).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "oracle resolve: refusing to submit resolveRange -- PMP {} exposes no bound \
             InferenceOrderBook, so whether it has liquidity for a weekly median cannot be read",
            pmp_display
        )
    })?;
    let order_book_s = dexdo_core::address::display(&order_book.with_workchain());
    validate_oracle_resolve_liquidity(
        &order_book_s,
        chain
            .inference_orderbook_weekly_median_price(&order_book)
            .await,
    )?;
    chain
        .resolve_oracle_range(
            &oel,
            &oracle_keys,
            &manifest.event_id,
            &manifest.oracle_list_hash,
            manifest.token_type,
        )
        .await?;
    println!(
        "resolveRange submitted event={} oracle_list_hash={} pmp={}",
        manifest.event_id, manifest.oracle_list_hash, pmp_display
    );
    let mut last_details_error = None;
    for i in 0..ORACLE_RESOLUTION_MAX_READS {
        match chain.pmp_details(&pmp).await {
            Ok(Some(details)) => {
                if let Some(outcome) = pmp_resolved_outcome(&details) {
                    println!(
                        "pmp resolved event={} outcome={} pmp={}",
                        manifest.event_id, outcome, pmp_display
                    );
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("pmp details poll failed (will retry): {e}");
                last_details_error = Some(e.to_string());
            }
        }
        if i + 1 < ORACLE_RESOLUTION_MAX_READS {
            tokio::time::sleep(ORACLE_RESOLUTION_POLL_INTERVAL).await;
        }
    }
    let resolution_timeout_secs = (ORACLE_RESOLUTION_MAX_READS as u64)
        .saturating_mul(ORACLE_RESOLUTION_POLL_INTERVAL.as_secs());
    let last_details_error = last_details_error
        .map(|e| format!(" Last transient pmp_details error while polling: {e}."))
        .unwrap_or_default();
    bail!(
        "resolveRange was submitted but PMP {} did not expose resolvedOutcome within {resolution_timeout_secs}s. \
         If the bound InferenceOrderBook has no MIN_LIQUIDITY, requestWeeklyMedian reverts under bounce:false \
         and onWeeklyMedian never arrives; this is the  no-liquidity stuck case, not a CLI success.{}",
        pmp_display,
        last_details_error
    )
}

fn oracle_u128(value: &serde_json::Value, field: &str) -> Option<u128> {
    value[field].as_u64().map(u128::from).or_else(|| {
        value[field]
            .as_str()
            .and_then(|raw| raw.parse::<u128>().ok())
    })
}

/// The book's own answer to `getWeeklyMedianPrice`, decided the way the contract decides it: an
/// answer at all means `_weeklyMedian()` cleared `require(totalVol >= MIN_LIQUIDITY,
/// ERR_NO_LIQUIDITY)` (`contracts/airegistry/InferenceOrderBook.sol:1738`), and no answer means it
/// did not. Money path, so anything that is not an answer refuses the resolve rather than paying
/// for one that cannot complete; the getter error is reported verbatim so the exit code the book
/// actually returned is on the operator's screen next to the constant it belongs to.
fn validate_oracle_resolve_liquidity(
    order_book: &str,
    weekly_median: Result<Option<u128>>,
) -> Result<u128> {
    let order_book = dexdo_core::address::display(order_book);
    match weekly_median {
        Ok(Some(price)) => Ok(price),
        Ok(None) => bail!(
            "oracle resolve: refusing to submit resolveRange -- InferenceOrderBook {order_book} is \
             not Active, so no liquidity and no weekly median can be read from it \
             (contracts/airegistry/InferenceOrderBook.sol:1749 getWeeklyMedianPrice)"
        ),
        Err(e) => bail!(
            "oracle resolve: refusing to submit resolveRange -- InferenceOrderBook {order_book} \
             reports no liquidity: getWeeklyMedianPrice \
             (contracts/airegistry/InferenceOrderBook.sol:1749) did not answer, which is how \
             `_weeklyMedian()` reports ERR_NO_LIQUIDITY = 334 (declared at :116, required at \
             :1738 as totalVol >= MIN_LIQUIDITY). resolveRange would make the PMP call \
             requestWeeklyMedian (:1759) under bounce:false, so that revert would never return \
             and the PMP would stay unresolved. Getter error: {e:#}"
        ),
    }
}

fn validate_oracle_cancel_preflight(
    before_pmp: &serde_json::Value,
    before_event: &serde_json::Value,
) -> Result<u128> {
    if before_pmp["approved"].as_bool() != Some(true) {
        bail!("oracle cancel: PMP is not approved");
    }
    if before_pmp["isCancelled"].as_bool() == Some(true) {
        bail!("oracle cancel: PMP is already cancelled");
    }
    if pmp_resolved_outcome(before_pmp).is_some() {
        bail!("oracle cancel: PMP is already resolved");
    }
    let before_count = oracle_u128(before_event, "count")
        .ok_or_else(|| anyhow::anyhow!("oracle cancel: event getter exposes no count"))?;
    if before_count == 0 {
        bail!("oracle cancel: event confirmation count is already zero");
    }
    Ok(before_count)
}

fn validate_oracle_cancel_postread(
    before_count: u128,
    after_pmp: Option<&serde_json::Value>,
    after_event: Option<&serde_json::Value>,
    exact_confirmation_active: bool,
) -> Result<(bool, Option<u128>)> {
    let (Some(after_pmp), Some(after_event)) = (after_pmp, after_event) else {
        return Ok((false, None));
    };
    let cancelled = after_pmp["isCancelled"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("oracle cancel: post-read exposes no isCancelled"))?;
    if !cancelled && pmp_resolved_outcome(after_pmp).is_some() {
        bail!("oracle cancel: contradictory post-read reports a resolved PMP");
    }
    let after_count = oracle_u128(after_event, "count")
        .ok_or_else(|| anyhow::anyhow!("oracle cancel: post-read exposes no event count"))?;
    let expected = before_count
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("oracle cancel: pre-read count was already zero"))?;
    if !(expected..=before_count).contains(&after_count) {
        bail!(
            "oracle cancel: contradictory post-read confirmation count {after_count}; expected {expected}..={before_count}"
        );
    }
    Ok((
        cancelled && after_count == expected && !exact_confirmation_active,
        Some(after_count),
    ))
}

fn validate_oracle_delete_preflight(event: &serde_json::Value, now: u64) -> Result<()> {
    let count = oracle_u128(event, "count")
        .ok_or_else(|| anyhow::anyhow!("oracle delete: event getter exposes no count"))?;
    if count != 0 {
        bail!("oracle delete: event still has {count} active PMP confirmation(s)");
    }
    let deadline = oracle_u128(event, "deadline")
        .ok_or_else(|| anyhow::anyhow!("oracle delete: event getter exposes no deadline"))?;
    if deadline >= u128::from(now) {
        bail!("oracle delete: deadline not passed (deadline={deadline}, now={now})");
    }
    Ok(())
}

fn validate_oracle_delete_postread(
    before_event: &serde_json::Value,
    after_event: Option<&serde_json::Value>,
) -> Result<bool> {
    let Some(after_event) = after_event else {
        return Ok(true);
    };
    if oracle_u128(after_event, "count") != Some(0)
        || oracle_u128(after_event, "deadline") != oracle_u128(before_event, "deadline")
    {
        bail!("oracle delete: contradictory post-read event state");
    }
    Ok(false)
}

async fn submit_oracle_cancel_after_validation(
    preflight: Result<u128>,
    submit: impl std::future::Future<Output = Result<serde_json::Value>>,
) -> Result<u128> {
    let before_count = preflight?;
    submit.await?;
    Ok(before_count)
}

async fn submit_oracle_delete_after_validation(
    preflight: Result<()>,
    submit: impl std::future::Future<Output = Result<serde_json::Value>>,
) -> Result<()> {
    preflight?;
    submit.await?;
    Ok(())
}

fn load_oracle_signer(path: &std::path::Path) -> Result<dexdo_core::KeyPair> {
    let secret = read_secret_hex(path, "--oracle-key")?;
    dexdo_core::KeyPair::from_secret_hex(secret.trim())
        .map_err(|e| anyhow::anyhow!("--oracle-key (SDK secret hex): {e:?}"))
}

async fn run_oracle_cancel(args: OracleResolveArgs) -> Result<()> {
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    chain_doctor_preflight(&crate::cli::commands::manifest_path()?, None).await?;
    let chain = dexdo_core::RealChainBackend::connect(
        crate::cli::commands::manifest_path()?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    let signer = load_oracle_signer(&args.oracle_key)?;
    let (oel, pmp, before_pmp, before_event) = chain
        .assert_oracle_market_identity(&manifest, &signer)
        .await?;
    let before_count = submit_oracle_cancel_after_validation(
        validate_oracle_cancel_preflight(&before_pmp, &before_event),
        chain.submit_pmp_cancel_event(&pmp, &signer),
    )
    .await?;
    let after_pmp = chain.pmp_details(&pmp).await?;
    let after_event = chain.oracle_event_info(&oel, &manifest.event_id).await?;
    let exact_confirmation_active = chain
        .oracle_event_list_has_pmp_confirmation(&oel, &pmp, &manifest.event_id)
        .await?;
    let (confirmed, after_count) = validate_oracle_cancel_postread(
        before_count,
        after_pmp.as_ref(),
        after_event.as_ref(),
        exact_confirmation_active,
    )?;
    let cancelled = after_pmp
        .as_ref()
        .and_then(|details| details["isCancelled"].as_bool())
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let after_count = after_count
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    println!(
        "oracle cancel submitted event={} pmp={} post_read_is_cancelled={} confirmations={before_count}->{after_count} exact_confirmation_active={exact_confirmation_active} status={}",
        manifest.event_id,
        dexdo_core::address::display(&manifest.pmp),
        cancelled,
        if confirmed { "confirmed" } else { "pending" }
    );
    Ok(())
}

async fn run_oracle_delete(args: OracleResolveArgs) -> Result<()> {
    let manifest = load_oracle_market_manifest(&args.manifest)?;
    chain_doctor_preflight(&crate::cli::commands::manifest_path()?, None).await?;
    let chain = dexdo_core::RealChainBackend::connect(
        crate::cli::commands::manifest_path()?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    let signer = load_oracle_signer(&args.oracle_key)?;
    let (oel, event) = chain
        .assert_oracle_event_identity(&manifest, &signer)
        .await?;
    submit_oracle_delete_after_validation(
        validate_oracle_delete_preflight(&event, chain.observed_chain_timestamp().await?),
        chain.delete_oracle_event(&oel, &signer, &manifest.event_id),
    )
    .await?;
    let after_event = chain.oracle_event_info(&oel, &manifest.event_id).await?;
    let confirmed = validate_oracle_delete_postread(&event, after_event.as_ref())?;
    println!(
        "oracle delete submitted event={} oracle_event_list={} post_read_exists={} status={}",
        manifest.event_id,
        dexdo_core::address::display(&manifest.oracle_event_list),
        after_event.is_some(),
        if confirmed { "confirmed" } else { "pending" }
    );
    Ok(())
}

fn is_ambiguous_money_submit(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<dexdo_core::MoneySubmitError>()
            .is_some_and(dexdo_core::MoneySubmitError::is_ambiguous)
    })
}

async fn read_pmp_unclaimed_balance(
    chain: &dexdo_core::RealChainBackend,
    pmp: &dexdo_core::Address,
) -> Result<Option<u128>> {
    chain
        .pmp_unclaimed_balance(pmp)
        .await?
        .map(|value| {
            oracle_u128(&value, "value0")
                .ok_or_else(|| anyhow::anyhow!("PMP getUnclaimedBalance exposes no uint128 value0"))
        })
        .transpose()
}

async fn wait_pmp_exit_postread(
    chain: &dexdo_core::RealChainBackend,
    note: &dexdo_core::Address,
    target: &PmpExitTarget,
    action: PmpExitAction,
) -> Result<PmpExitObservation> {
    let mut last_state = None;
    let mut last_error = None;
    for read in 0..PMP_EXIT_CONFIRM_MAX_READS {
        match chain
            .private_note_pmp_exit_state(
                note,
                &target.event_id,
                &target.oracle_list_hash,
                target.token_type,
            )
            .await
            .and_then(|value| parse_pmp_exit_observation(&value))
        {
            Ok(state) if pmp_exit_postread_confirmed(&state) => return Ok(state),
            Ok(state) => last_state = Some(state),
            Err(error) => last_error = Some(format!("{error:#}")),
        }
        if read + 1 < PMP_EXIT_CONFIRM_MAX_READS {
            tokio::time::sleep(PMP_EXIT_CONFIRM_POLL_INTERVAL).await;
        }
    }
    bail!(
        "{}: callback was not confirmed after {} reads; last_state={last_state:?}; last_read_error={last_error:?}",
        action.command(),
        PMP_EXIT_CONFIRM_MAX_READS
    )
}

/// What an operator reads after a forfeit lands, and it exists because the shared exit line would
/// otherwise mislead here.

/// `cancel-stake` and `claim` both move `note_balance`, so the before/after pair IS their result.
/// A forfeit deliberately moves nothing -- `PMP.forfeitStake` touches only `_forfeited` -- so the
/// same line prints `note_balance=N->N` beside `status=confirmed`, which reads either as "nothing
/// happened" or, worse, as "confirmed" meaning the money is back. Both are wrong: what was
/// confirmed is that the stake record is GONE and the note is unfrozen.
pub(crate) fn render_forfeit_epilogue(
    note_balance_before: u128,
    note_balance_after: u128,
) -> String {
    let moved = note_balance_after != note_balance_before;
    let unchanged = if moved {
        "  the note balance moved, which a forfeit does not cause -- read it again before acting\n"
    } else {
        "  the note balance did NOT move, and that is correct: a forfeit returns nothing now\n"
    };
    format!(
        "oracle forfeit-stake result\n           the stake record is gone and the note is no longer frozen by it\n         {unchanged}           the stake is in the market's forfeited mass; it leaves at the PMP's close, to the          market's deployer\n           NEXT, IN THIS ORDER: wait for the close and check `dexdo note balance`; only after the          credit lands run `dexdo note withdraw`\n           withdrawing first drops the credit permanently -- `PrivateNote.acceptFee` does not credit          a withdrawn note, and no sweep reaches money that was never credited\n"
    )
}

/// The refusal that stands between an operator and an irreversible forfeit.

/// NOT cfg-gated, and deliberately: it is the safety, it must be testable where CI actually runs,
/// and a guard nobody has watched refuse is not a guard.

/// The text names a PRICE and an ORDER, in that order, because those are the two things that decide
/// whether this command recovers the money or loses it. It does not promise the stake back: the
/// close is moved by other parties on their own schedule, and "returns at close, if" is the whole
/// of what we know.
pub(crate) fn forfeit_stake_consent(abandon_the_stake: bool) -> Result<()> {
    if abandon_the_stake {
        return Ok(());
    }
    bail!(
        "oracle forfeit-stake: refused. This ABANDONS the stake -- it is recorded in the market's \
         forfeited mass, is never paid back to you directly, and leaves only when the PMP closes, \
         to the market's deployer. For a stake this client can hold that deployer IS this note, so \
         it can come back -- but only at close, and only if this note has not withdrawn by then: \
         `PrivateNote.acceptFee` drops the credit outright on a withdrawn note.\n\n\
         THE ORDER MATTERS AND IT IS THIS:\n\
           1. forfeit the stake (this command, with --abandon-the-stake)\n\
           2. WAIT for the PMP to close and the credit to land -- check with `dexdo note balance`\n\
           3. only THEN run `dexdo note withdraw`\n\n\
         Withdrawing before the credit lands is how the stake is actually lost. Nothing here can \
         un-lose it afterwards: the money is dropped at the contract boundary, so no sweep reaches \
         it either.\n\n\
         Prefer `dexdo oracle cancel-stake` whenever it works -- it RETURNS the stake instead of \
         abandoning it. Use this only when the market cannot be cancelled and the stake is freezing \
         the whole note. Re-run with --abandon-the-stake to proceed."
    )
}

/// `dexdo oracle forfeit-stake`: the consent gate, then the shared exit machinery.

/// The gate is FIRST, before the manifest is read, before the owner key is looked for, and before
/// any chain connection -- so a run without the flag cannot reach anything that spends or sends.
async fn run_oracle_forfeit_stake(args: crate::cli::args::OracleForfeitStakeArgs) -> Result<()> {
    forfeit_stake_consent(args.abandon_the_stake)?;
    run_oracle_pmp_exit(args.exit, PmpExitAction::ForfeitStake).await
}

/// What a PMP exit is actually addressed by. The manifest is one carrier of it; the raw flags are
/// another. Kept as its own value rather than a half-filled manifest, because a manifest with
/// invented `network`/`oracle`/`bounds` fields would be a placeholder standing where a real value
/// is expected, and the next reader could not tell which fields were real.
#[derive(Debug)]
struct PmpExitTarget {
    pmp: String,
    event_id: String,
    oracle_list_hash: String,
    token_type: u32,
    source: &'static str,
}

fn resolve_pmp_exit_target(args: &OraclePmpExitArgs, command: &str) -> Result<PmpExitTarget> {
    match (args.manifest.as_deref(), args.pmp.as_deref()) {
        (Some(path), None) => {
            let manifest = load_oracle_market_manifest(path)?;
            Ok(PmpExitTarget {
                pmp: manifest.pmp,
                event_id: manifest.event_id,
                oracle_list_hash: manifest.oracle_list_hash,
                token_type: manifest.token_type,
                source: "the manifest",
            })
        }
        (None, Some(pmp)) => {
            let event_id = args
                .event_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("{command}: --pmp needs --event-id"))?;
            let oracle_list_hash = args
                .oracle_list_hash
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("{command}: --pmp needs --oracle-list-hash"))?;
            if args.token_type != SHELL_CURRENCY_ID {
                bail!(
                    "{command}: --token-type {} is unsupported; dexdo markets require SHELL currency id {}",
                    args.token_type,
                    SHELL_CURRENCY_ID
                );
            }
            Ok(PmpExitTarget {
                pmp: pmp.to_string(),
                event_id: event_id.to_string(),
                oracle_list_hash: oracle_list_hash.to_string(),
                token_type: args.token_type,
                source: "the --event-id/--oracle-list-hash/--token-type triple",
            })
        }
        (Some(_), Some(_)) => bail!(
            "{command}: pass either --manifest or --pmp with the triple, not both -- two sources \
             that could disagree is not a stronger check, it is an unanswered question"
        ),
        (None, None) => {
            bail!("{command}: needs --manifest, or --pmp with --event-id and --oracle-list-hash")
        }
    }
}

async fn run_oracle_pmp_exit(args: OraclePmpExitArgs, action: PmpExitAction) -> Result<()> {
    use dexdo_core::{KeyPair, RealChainBackend};

    let target = resolve_pmp_exit_target(&args, action.command())?;
    // gave this exit a RESOLVED target, which accepts either a manifest or the raw `--pmp`
    // plus `--event-id`/`--oracle-list-hash` triple. `oracle forfeit-stake` did not exist
    // when that was written, and it ABANDONS the stake irreversibly. It therefore does not inherit
    // the manifest-less route: that would be a new capability arriving as a side effect of two
    // branches never having seen each other, on the one exit here that cannot be undone.

    // DELIBERATE, NOT AN OVERSIGHT. Today's behaviour is that this command takes a manifest, and
    // this keeps it unchanged to the value: under `--manifest` the three fields the submit arm
    // reads are copied straight off the loaded manifest, so what reaches the chain is what reached
    // it before.

    // Refused HERE and not at the submit arm, so the refusal costs no chain read -- the placement
    // the consent gate already uses. Whether a stake can sit in a PMP that no manifest can address
    // is a real question and an open one; it is named in this change and not answered by it,
    // because answering it is a new capability owing its own measurement and its own tests.
    if matches!(action, PmpExitAction::ForfeitStake) && args.manifest.is_none() {
        bail!(
            "{}: needs --manifest. The raw --pmp/--event-id/--oracle-list-hash route addresses a \
             PMP with no declared network for an endpoint to be checked against, and this command \
             abandons the stake irreversibly, so it is not offered here. \
             `dexdo oracle cancel-stake` does take the raw triple and RETURNS the stake -- prefer \
             it wherever it works.",
            action.command()
        );
    }
    let note_addr = require_note_addr(
        &args.identity,
        action.command(),
        "PMP participant PrivateNote",
    )?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|error| anyhow::anyhow!("--note-addr {note_addr}: {error}"))?;
    let note_secret = crate::cli::support::note_owner_secret_for(
        args.identity.note_key.as_deref(),
        &note_addr,
        None,
        action.command(),
        "the participant note's owner key",
    )?;
    let note_keys = KeyPair::from_secret_hex(note_secret.trim())
        .map_err(|error| anyhow::anyhow!("--note-key (SDK secret hex): {error:?}"))?;
    chain_doctor_preflight(&crate::cli::commands::manifest_path()?, None).await?;
    let chain = RealChainBackend::connect(
        crate::cli::commands::manifest_path()?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    chain
        .assert_note_owner_matches(action.command(), &note, &note_keys)
        .await?;
    let pmp = dexdo_core::address::parse_chain_address(&target.pmp)
        .map_err(|error| anyhow::anyhow!("{}: --pmp {}: {error}", action.command(), target.pmp))?;
    let pmp_details = chain
        .assert_pmp_identity_for_triple(
            &pmp,
            &target.event_id,
            &target.oracle_list_hash,
            target.token_type,
            target.source,
        )
        .await?;
    let shutdown = required_oracle_getter(
        chain.pmp_shutdown_state(&pmp).await?,
        "PMP",
        &pmp,
        "getShutdownState",
    )?;
    let before = parse_pmp_exit_observation(
        &chain
            .private_note_pmp_exit_state(
                &note,
                &target.event_id,
                &target.oracle_list_hash,
                target.token_type,
            )
            .await?,
    )?;
    let chain_now_secs = chain.observed_chain_timestamp().await?;
    validate_pmp_exit_preflight(action, &pmp_details, &shutdown, &before, chain_now_secs)?;
    let before_unclaimed = read_pmp_unclaimed_balance(&chain, &pmp)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("PMP {pmp} getUnclaimedBalance unavailable before submit")
        })?;

    let submit = match action {
        PmpExitAction::CancelStake => {
            chain
                .cancel_pmp_stake(
                    &note,
                    &note_keys,
                    &target.event_id,
                    &target.oracle_list_hash,
                    target.token_type,
                )
                .await
        }
        PmpExitAction::Claim => {
            chain
                .claim_pmp_stake(
                    &note,
                    &note_keys,
                    &target.event_id,
                    &target.oracle_list_hash,
                    target.token_type,
                )
                .await
        }
        PmpExitAction::ForfeitStake => {
            chain
                .forfeit_pmp_stake(
                    &note,
                    &note_keys,
                    &target.event_id,
                    &target.oracle_list_hash,
                    target.token_type,
                )
                .await
        }
    };
    let submit_status = match submit {
        Ok(_) => "accepted",
        Err(error) if is_ambiguous_money_submit(&error) => "ambiguous-reconciled",
        Err(error) => return Err(error),
    };
    let after = wait_pmp_exit_postread(&chain, &note, &target, action).await?;
    let after_unclaimed = read_pmp_unclaimed_balance(&chain, &pmp).await?;
    let after_unclaimed = after_unclaimed
        .map(|value| value.to_string())
        .unwrap_or_else(|| "inactive-or-missing".to_string());
    println!(
        "{} submitted event={} oracle_list_hash={} token_type={} pmp={} note={} \
         pmp_unclaimed={before_unclaimed}->{after_unclaimed} note_balance={}->{} \
         coupons={}->{} submit_status={submit_status} status=confirmed",
        action.command(),
        target.event_id,
        target.oracle_list_hash,
        target.token_type,
        pmp.with_workchain(),
        note.with_workchain(),
        before.note_balance,
        after.note_balance,
        before.coupons_value,
        after.coupons_value,
    );
    if action == PmpExitAction::ForfeitStake {
        print!(
            "{}",
            render_forfeit_epilogue(before.note_balance, after.note_balance)
        );
    }
    Ok(())
}

async fn wait_oracle_fee_balance(
    chain: &dexdo_core::RealChainBackend,
    oracle: &dexdo_core::Address,
    signer: &dexdo_core::KeyPair,
    expected: u128,
) -> Result<u128> {
    let mut last_balance = None;
    let mut last_error = None;
    for read in 0..ORACLE_FEE_WITHDRAW_CONFIRM_MAX_READS {
        match chain.oracle_fee_balance_for_owner(oracle, signer).await {
            Ok(balance) if oracle_fee_postread_confirmed(expected, balance) => return Ok(balance),
            Ok(balance) => last_balance = Some(balance),
            Err(error) => last_error = Some(format!("{error:#}")),
        }
        if read + 1 < ORACLE_FEE_WITHDRAW_CONFIRM_MAX_READS {
            tokio::time::sleep(ORACLE_FEE_WITHDRAW_CONFIRM_POLL_INTERVAL).await;
        }
    }
    bail!(
        "oracle withdraw-fees: account ECC[2] balance did not reach expected {expected} after {} reads; \
         last_balance={last_balance:?}; last_read_error={last_error:?}",
        ORACLE_FEE_WITHDRAW_CONFIRM_MAX_READS
    )
}

/// the destination account, as one read of it answered.

/// `Unreadable` is a THIRD answer and not a flavour of either other one. An endpoint that will not
/// answer for an address is not evidence about that address, and folding it into "fine" or "wrong" is
/// how a complete-looking answer ends up covering less than it appears to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OracleFeeDestinationReading {
    /// The account's RAW ECC[2] pocket (`balance_other`) -- the plane `Oracle.withdrawFees` credits,
    /// since it sends a `currencies` map (`contracts/dex/Oracle.sol:97-99`). A wallet has no
    /// `getDetails` trading record, so there is no second plane here, but the plane is named because
    /// a balance claim that does not name its plane is not a claim.
    Pocket(u128),
    Unreadable(String),
}

/// What the pair of destination reads established. Three outcomes, because there are three.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OracleFeeDestinationOutcome {
    Credited { before: u128, after: u128 },
    NotCredited { before: u128, after: u128 },
    Unread(String),
}

/// Did the destination receive it?

/// The predicate is `after >= before + amount` and deliberately NOT equality. The sender-side check
/// is rightly exact -- an Oracle's fee pocket does not move for other reasons -- but a destination
/// wallet receives other money while this command is polling, and an exact match would report a
/// perfectly good withdrawal as a failure.

/// What that costs, stated so nobody reads more into a `Credited` than it holds: a concurrent DEBIT
/// at the destination can hold the pocket below the threshold and produce `NotCredited` for a
/// withdrawal that did arrive. That is why both numbers are printed rather than only the verdict --
/// the operator can see the pocket and judge, which they cannot do from a bare word.
fn oracle_fee_destination_outcome(
    before: &OracleFeeDestinationReading,
    after: &OracleFeeDestinationReading,
    amount: u128,
) -> OracleFeeDestinationOutcome {
    match (before, after) {
        (
            OracleFeeDestinationReading::Pocket(before),
            OracleFeeDestinationReading::Pocket(after),
        ) => {
            if *after >= before.saturating_add(amount) {
                OracleFeeDestinationOutcome::Credited {
                    before: *before,
                    after: *after,
                }
            } else {
                OracleFeeDestinationOutcome::NotCredited {
                    before: *before,
                    after: *after,
                }
            }
        }
        (OracleFeeDestinationReading::Unreadable(why), _)
        | (_, OracleFeeDestinationReading::Unreadable(why)) => {
            OracleFeeDestinationOutcome::Unread(why.clone())
        }
    }
}

/// The `status=` word this command is entitled to print.

/// `confirmed` is reserved for the case where BOTH sides were read and both agree. Before it
/// was printed whenever the Oracle's pocket fell by `amount`, which is true of a withdrawal that
/// went to the wrong live address just as much as of one that went to the right one.
fn oracle_fee_status_word(outcome: &OracleFeeDestinationOutcome) -> &'static str {
    match outcome {
        OracleFeeDestinationOutcome::Credited { .. } => "confirmed",
        OracleFeeDestinationOutcome::NotCredited { .. } => "sender-only-destination-not-credited",
        OracleFeeDestinationOutcome::Unread(_) => "sender-only-destination-unread",
    }
}

/// What kind of account `--to` is, decided from one account read.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OracleWithdrawDestinationKind {
    NotFound,
    NotActive(String),
    /// A contract the deployed manifest names, carrying that name.
    DeployedContract(String),
    /// A funding-wallet family this build supports.
    SupportedWallet,
    UnknownCode(String),
}

fn normalized_code_hash(raw: &str) -> String {
    let raw = raw.trim();
    raw.strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw)
        .to_ascii_lowercase()
}

/// Classify `--to` from the account snapshot. Facts only; the policy is the next function.
fn classify_oracle_withdraw_destination(
    found: bool,
    status: &str,
    is_active: bool,
    code_hash: Option<&str>,
    deployed_hashes: &[(&str, String)],
) -> OracleWithdrawDestinationKind {
    if !found {
        return OracleWithdrawDestinationKind::NotFound;
    }
    if !is_active {
        return OracleWithdrawDestinationKind::NotActive(status.to_string());
    }
    let Some(code_hash) = code_hash.map(normalized_code_hash) else {
        return OracleWithdrawDestinationKind::NotActive(status.to_string());
    };
    for (name, compiled) in deployed_hashes {
        if normalized_code_hash(compiled) == code_hash {
            return OracleWithdrawDestinationKind::DeployedContract((*name).to_string());
        }
    }
    if dexdo_core::canonical_multisig::is_supported_spending_code_hash(&code_hash) {
        return OracleWithdrawDestinationKind::SupportedWallet;
    }
    OracleWithdrawDestinationKind::UnknownCode(code_hash)
}

/// How `--to` earned admission, so the printed line can say which route it took.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OracleWithdrawDestinationProof {
    /// It is the address the operator declared in their wallet binding for this network.
    Declared(&'static str),
    /// The key that owns this Oracle is a custodian of this wallet.
    Custodian,
}

/// Do a declared address and `--to` name the SAME ACCOUNT, whatever either one is spelled like?

/// This was `address == to_display`, a comparison of SPELLINGS, and it could not match:

/// * `--to` crosses `value_parser = arg_to_chain_param` (`args.rs:2348`), which returns
/// `CanonicalAddress::legacy()` -- `0:<account_id>` -- for a canonical input and hands a legacy
/// one through unchanged. So no `::` ever survives into `args.to`;
/// * `to_display` is then `display_self_dapp(&to.with_workchain())` (`:1477`), and on an input
/// with no `::` that branch renders the account's self-DApp identity `<account>::<account>`;
/// * a binding written by `wallet onboard manual` from a legacy `0:<hex>` multisig address --
/// the spelling that flag's own help advertises (`args.rs:1426`) -- holds
/// `CanonicalAddress::parse(..).to_string()` (`wallet_manual.rs:155-157`), and `parse` fills a
/// legacy input's absent DApp with `DEXDO_DAPP_ID`, so the stored spelling is `0000..0004::<account>`.

/// One account, two spellings, and the DECLARED route -- the only route left to an operator whose
/// oracle key is deliberately not a custodian of their payout wallet -- refused every time.

/// The account id is the right thing to compare and not merely the convenient one. `withdrawFees`
/// takes a TVM `address` (`contracts/dex/Oracle.sol:97-99`), which is workchain plus account id; the
/// DApp half is which DApp an account belongs to, and it is not what the value is routed by. Two
/// spellings with the same account id are the same destination for this transfer, which is why the
/// repo compares account ids after and.

/// An address that does not parse cannot be shown to be the same account, so it does not admit. A
/// malformed declaration therefore falls through to the custodian route or to a refusal, which is
/// the safe direction: this function's job is to ADMIT, never to refuse.
fn declares_the_same_account(declared: &str, to_display: &str) -> bool {
    match (
        dexdo_core::CanonicalAddress::parse(declared),
        dexdo_core::CanonicalAddress::parse(to_display),
    ) {
        (Ok(declared), Ok(to)) => declared.account_id() == to.account_id(),
        _ => false,
    }
}

/// Decide whether fees may move to `--to`.

/// Two routes, and a destination that takes neither is refused naming both:

/// * DECLARED -- `--to` is `hot_address` or `vault_address` of the wallet binding for the network
/// this command is running on. Checked first because it is free: no getter, no second key, and
/// it is the only route available to an operator whose oracle key is deliberately not a
/// custodian of their payout wallet (key separation is good practice, not a corner case).
/// * CUSTODIAN -- `--to` is a supported wallet family AND the key that owns this Oracle is one of
/// its custodians. This is the same proof `note deploy` makes before it spends
/// (`crates/dexdo/src/cli/note_cmd.rs:1590-1608`), with the same reader, in the same order:
/// code hash first, because `getCustodians` is only meaningful on an account already proven to
/// be that wallet family.

/// # What this does NOT close, and why the next reader must not assume it does

/// **A hostile command line defeats every route above, and no file can fix that.** `--data-dir` is
/// declared `global = true` (`crates/dexdo/src/main.rs:26`), so it is reachable from this command:
/// whoever composes the argv can point the wallet store at a directory they own and put any
/// `hot_address` they like in it. The declaration route is then self-satisfied, and the custodian
/// route can be skipped entirely by supplying a destination that satisfies it.

/// So this guards operator ERROR -- the typo, the address pasted from a neighbouring terminal, which
/// is the loss path is about -- and it guards an environment where files are writable but argv
/// is not, because the binding is written under an owner-only directory
/// (`crates/dexdo/src/cli/wallet/store.rs:119-128`, `:798`) while a bare env var is not. It does NOT
/// guard an attacker who writes the command line. The only declaration that would is one stored on
/// chain beside `_oraclePubkey` (`contracts/dex/Oracle.sol:16`), which is a contract change.

/// Written here rather than only in the issue because this is the sentence that goes missing: in six
/// months "the destination is checked against the binding" reads like the hostile case is covered.
fn admit_oracle_withdraw_destination(
    to_display: &str,
    kind: &OracleWithdrawDestinationKind,
    oracle_owner_pubkey: &str,
    custodian_pubkeys: &[String],
    declared: &[(&'static str, String)],
) -> Result<OracleWithdrawDestinationProof> {
    for (field, address) in declared {
        if declares_the_same_account(address, to_display) {
            return Ok(OracleWithdrawDestinationProof::Declared(field));
        }
    }
    match kind {
        OracleWithdrawDestinationKind::NotFound => bail!(
            "oracle withdraw-fees: --to {to_display} names no account on this network. Fees are not \
             sent to an address that cannot be shown to exist; pass the wallet they should land in, \
             or bind one with `dexdo wallet onboard` so it is declared once and checked every time."
        ),
        OracleWithdrawDestinationKind::NotActive(status) => bail!(
            "oracle withdraw-fees: --to {to_display} is not Active (acc_type={status}), so nothing \
             can be shown about what would become of fees sent there. No transaction was submitted."
        ),
        OracleWithdrawDestinationKind::DeployedContract(name) => bail!(
            "oracle withdraw-fees: --to {to_display} is a deployed {name}, not a wallet. A dex \
             contract is not a payout destination -- pass the wallet the fees should land in. \
             Nothing was submitted, and refusing costs nothing that passing the right address does \
             not recover."
        ),
        OracleWithdrawDestinationKind::UnknownCode(code_hash) => bail!(
            "oracle withdraw-fees: --to {to_display} runs code this build does not know \
             (code_hash {code_hash}), so it is neither a supported wallet nor a contract the \
             deployed manifest names, and nothing can be proved about who controls it. Pass a \
             supported wallet, or declare this address with `dexdo wallet onboard` if it is yours."
        ),
        OracleWithdrawDestinationKind::SupportedWallet => {
            let owner = dexdo_core::normalize_multisig_pubkey(oracle_owner_pubkey)
                .unwrap_or_else(|| oracle_owner_pubkey.trim().to_ascii_lowercase());
            if custodian_pubkeys.contains(&owner) {
                return Ok(OracleWithdrawDestinationProof::Custodian);
            }
            bail!(
                "oracle withdraw-fees: --to {to_display} is a supported wallet, but the key that \
                 owns this Oracle is not one of its custodians, so this client cannot show the \
                 destination is yours. Either withdraw to a wallet this Oracle's key is a custodian \
                 of, or bind {to_display} with `dexdo wallet onboard` so it is declared for this \
                 network. Nothing was submitted."
            )
        }
    }
}

/// Read the destination's raw ECC[2] pocket, or say why it could not be read.
async fn read_oracle_fee_destination(
    chain: &dexdo_core::RealChainBackend,
    to: &dexdo_core::Address,
) -> OracleFeeDestinationReading {
    match chain.client().get_account_retrying(to).await {
        Ok(Some(account)) => OracleFeeDestinationReading::Pocket(
            account.ecc_balance(dexdo_core::params::SHELL_CURRENCY_ID),
        ),
        Ok(None) => OracleFeeDestinationReading::Unreadable("account not found".to_string()),
        Err(error) => OracleFeeDestinationReading::Unreadable(format!("{error:#}")),
    }
}

/// The destination addresses the operator declared for THIS network, if they declared any.

/// Read from the wallet binding, which already means exactly this -- `hot_address` is documented as
/// the wallet that funds spends. Nothing new is stored: this is a read of a record the operator
/// already created with `dexdo wallet onboard`, and the record carries no secret, so the check needs
/// no second key (`crates/dexdo/src/cli/wallet.rs:182-186`).

/// The network comes from the deployed manifest and only from there, which is what stops a mainnet
/// withdrawal reading the declaration -- the binding is keyed by network in its PATH
/// (`crates/dexdo/src/cli/wallet/store.rs:109-113`).

/// A missing binding is not an error: an operator who never onboarded a wallet simply has no
/// declared route, and the custodian proof stands on its own.
fn declared_oracle_fee_destinations(contracts: &std::path::Path) -> Vec<(&'static str, String)> {
    let Ok(deployed) = dexdo_core::Deployed::load(contracts) else {
        return Vec::new();
    };
    let Ok(network) = crate::cli::wallet::WalletNetwork::from_manifest_label(&deployed.network)
    else {
        return Vec::new();
    };
    let Ok(store) = crate::cli::wallet::WalletStore::open() else {
        return Vec::new();
    };
    let Ok(Some(binding)) = store.load_active(&network) else {
        return Vec::new();
    };
    let mut declared = vec![("hot_address", binding.hot_address)];
    if let Some(vault) = binding.vault_address {
        declared.push(("vault_address", vault));
    }
    declared
}

/// Everything `--to` has to survive before a single vmshell moves. All of it before the submit.
async fn preflight_oracle_fee_destination(
    chain: &dexdo_core::RealChainBackend,
    contracts: &std::path::Path,
    to: &dexdo_core::Address,
    oracle_owner_pubkey: &str,
) -> Result<OracleWithdrawDestinationProof> {
    let to_display = dexdo_core::address::display_self_dapp(&to.with_workchain());
    let declared = declared_oracle_fee_destinations(contracts);
    let account = chain
        .client()
        .get_account_retrying(to)
        .await
        .map_err(|e| anyhow::anyhow!("oracle withdraw-fees: read --to {to_display}: {e}"))?;
    let deployed_hashes = dexdo_core::chain::compiled_contract_hashes();
    let kind = match &account {
        Some(account) => classify_oracle_withdraw_destination(
            true,
            &account.status.to_string(),
            account.is_active(),
            account.code_hash.as_deref(),
            &deployed_hashes,
        ),
        None => classify_oracle_withdraw_destination(false, "", false, None, &deployed_hashes),
    };
    // Only asked of an account already proven to be a supported wallet family: `getCustodians` is
    // meaningless on anything else, and asking it first would turn a clear refusal into a getter
    // error. Same order as the funding-wallet preflight in `note_cmd.rs:1590-1608`.
    let custodian_pubkeys = if matches!(kind, OracleWithdrawDestinationKind::SupportedWallet) {
        chain
            .client()
            .run_getter_retrying(
                to,
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
                "getCustodians",
                serde_json::json!({}),
            )
            .await
            .ok()
            .flatten()
            .map(|output| crate::cli::note_cmd::multisig_custodian_pubkeys(&output))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    admit_oracle_withdraw_destination(
        &to_display,
        &kind,
        oracle_owner_pubkey,
        &custodian_pubkeys,
        &declared,
    )
}

async fn run_oracle_withdraw_fees(args: OracleWithdrawFeesArgs) -> Result<()> {
    let oracle = parse_oracle_read_address("--oracle", &args.oracle)?;
    let to = parse_oracle_read_address("--to", &args.to)?;
    if oracle.with_workchain() == to.with_workchain() {
        bail!("oracle withdraw-fees: --to must not be the Oracle itself");
    }
    let signer = load_oracle_signer(&args.oracle_key)?;
    chain_doctor_preflight(&crate::cli::commands::manifest_path()?, None).await?;
    let chain = oracle_read_chain(&crate::cli::commands::manifest_path()?)?;
    let before = chain.oracle_fee_balance_for_owner(&oracle, &signer).await?;
    let expected = oracle_fee_expected_after(before, args.amount)?;
    // Before the submit: nothing below this line can un-send a transfer.
    let proof = preflight_oracle_fee_destination(
        &chain,
        &crate::cli::commands::manifest_path()?,
        &to,
        signer.public_hex(),
    )
    .await?;
    let destination_before = read_oracle_fee_destination(&chain, &to).await;
    let submit = chain
        .withdraw_oracle_fees(&oracle, &signer, &to, args.amount)
        .await;
    let submit_status = match submit {
        Ok(_) => "accepted",
        Err(error) if is_ambiguous_money_submit(&error) => "ambiguous-reconciled",
        Err(error) => return Err(error),
    };
    let after = wait_oracle_fee_balance(&chain, &oracle, &signer, expected).await?;
    let destination_after = read_oracle_fee_destination(&chain, &to).await;
    let outcome =
        oracle_fee_destination_outcome(&destination_before, &destination_after, args.amount);
    let admitted = match proof {
        OracleWithdrawDestinationProof::Declared(field) => format!("declared:{field}"),
        OracleWithdrawDestinationProof::Custodian => "custodian".to_string(),
    };
    let destination_line = match &outcome {
        OracleFeeDestinationOutcome::Credited { before, after }
        | OracleFeeDestinationOutcome::NotCredited { before, after } => format!(
            "destination_ecc2={}->{} SHELL",
            dexdo_core::shell_amount(*before),
            dexdo_core::shell_amount(*after)
        ),
        OracleFeeDestinationOutcome::Unread(why) => format!("destination_ecc2=unread ({why})"),
    };
    println!(
        "oracle withdraw-fees submitted oracle={} to={} amount={} SHELL balance={}->{} SHELL \
         {destination_line} admitted={admitted} submit_status={submit_status} status={}",
        oracle.with_workchain(),
        to.with_workchain(),
        dexdo_core::shell_amount(args.amount),
        dexdo_core::shell_amount(before),
        dexdo_core::shell_amount(after),
        oracle_fee_status_word(&outcome),
    );
    // The Oracle's pocket fell and the destination we COULD read did not rise. That is the shape a
    // misdirection has, and it is not something to report under a zero exit code. `Unread` is not
    // this case: an unread destination is an open question, and a question is not a finding.
    if let OracleFeeDestinationOutcome::NotCredited { before, after } = outcome {
        bail!(
            "oracle withdraw-fees: the Oracle's ECC[2] pocket fell by {} SHELL but --to {} went \
             {}->{} SHELL, which is short of the {} it should have received. The submit was \
             accepted, so this is a report and not a rollback: read the destination account before \
             withdrawing again. A concurrent debit at the destination can also produce this.",
            dexdo_core::shell_amount(args.amount),
            to.with_workchain(),
            dexdo_core::shell_amount(before),
            dexdo_core::shell_amount(after),
            dexdo_core::shell_amount(args.amount),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn oracle_manifest(token_type: u32) -> dexdo_core::OracleMarketManifest {
        dexdo_core::OracleMarketManifest {
            network: "net-a".into(),
            root_oracle: "0:root".into(),
            oracle: "0:oracle".into(),
            oracle_event_list: "0:list".into(),
            oracle_list_hash: "1".into(),
            event_id: "2".into(),
            event_name: "event".into(),
            pmp: "0:pmp".into(),
            token_type,
            inference_order_book: "0:book".into(),
            frame_model: "model".into(),
            deadline: 1,
            bounds: vec!["100".into()],
            outcome_names: vec!["below".into(), "above".into()],
        }
    }

    #[test]
    fn oracle_manifest_rejects_non_shell_before_chain_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oracle-market.json");
        std::fs::write(&path, oracle_manifest(1).to_json().unwrap()).unwrap();

        let error = super::load_oracle_market_manifest(&path)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains(&format!(
                "require SHELL currency id {}",
                super::SHELL_CURRENCY_ID
            )),
            "{error}"
        );
    }

    #[tokio::test]
    async fn oracle_state_rejects_non_shell_before_doctor_or_backend() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("oracle-market.json");
        std::fs::write(&manifest, oracle_manifest(1).to_json().unwrap()).unwrap();

        let error = super::run_oracle_state(super::OracleStateArgs { manifest })
            .await
            .unwrap_err()
            .to_string();

        assert!(
            error.contains(&format!(
                "require SHELL currency id {}",
                super::SHELL_CURRENCY_ID
            )),
            "{error}"
        );
        assert!(!error.contains("must-not-read-contracts"), "{error}");
    }

    #[test]
    fn oracle_deadline_enforces_contract_result_gap() {
        let now = 1_900_000_000;
        assert!(super::validate_oracle_deadline(now + 119, now).is_err());
        assert!(super::validate_oracle_deadline(now + 120, now).is_ok());
    }

    #[test]
    fn oracle_resolve_refuses_without_weekly_median_liquidity() {
        assert_eq!(
            super::validate_oracle_resolve_liquidity("0:book", Ok(Some(7))).unwrap(),
            7,
            "an answered getWeeklyMedianPrice is the contract's own proof of liquidity"
        );

        for refusal in [
            super::validate_oracle_resolve_liquidity("0:book", Ok(None)),
            super::validate_oracle_resolve_liquidity(
                "0:book",
                Err(anyhow::anyhow!(
                    "run_tvm getter getWeeklyMedianPrice: Contract execution was terminated with \
                     error: exit code: 334"
                )),
            ),
        ] {
            let error = refusal.unwrap_err().to_string();
            assert!(
                error.to_ascii_lowercase().contains("no liquidity"),
                "the refusal must name the condition in the words the operator reads: {error}"
            );
            assert!(
                error.contains("refusing to submit resolveRange"),
                "the refusal must say that nothing was submitted: {error}"
            );
        }

        let exit_code = super::validate_oracle_resolve_liquidity(
            "0:book",
            Err(anyhow::anyhow!("run_tvm getter getWeeklyMedianPrice")),
        )
        .unwrap_err()
        .to_string();
        assert!(
            exit_code.contains("ERR_NO_LIQUIDITY = 334"),
            "the refusal must carry the exact contract exit code: {exit_code}"
        );
    }

    /// `PMP.cancelStake` cancels the event inside itself once the grace period has passed
    /// on an unresolved event, so the preflight must admit that second route to
    /// `require(_isCancelled)`. It must NOT admit anything else: the two controls below are the
    /// point of the test, because a check that started passing everything would look identical to
    /// a fix from the outside.
    #[test]
    fn cancel_stake_preflight_admits_the_self_cancelling_call_and_nothing_more() {
        const RESULT_END: u64 = 1_787_246_775;
        let note = super::PmpExitObservation {
            stake_present: true,
            candidate_amount: 0,
            amount_slots: 2,
            open_orders: 0,
            busy_address: None,
            has_withdrawn: false,
            note_balance: 93_905_000_000,
            coupons_value: 0,
        };
        let shutdown = serde_json::json!({ "orderBookDone": true });
        let pmp = |cancelled: bool, resolved: serde_json::Value| {
            serde_json::json!({
                "isCancelled": cancelled,
                "resolvedOutcome": resolved,
                "resultEnd": RESULT_END.to_string(),
                "frozen": false,
            })
        };
        let check = |pmp: &serde_json::Value, now: u64| {
            super::validate_pmp_exit_preflight(
                super::PmpExitAction::CancelStake,
                pmp,
                &shutdown,
                &note,
                now,
            )
        };

        // THE FIX. Grace passed, outcome unresolved, not yet cancelled: the contract would cancel
        // it in this very call, so the client must let the call happen.
        check(&pmp(false, serde_json::Value::Null), RESULT_END + 1)
            .expect("past grace and unresolved must be submitted, not refused");

        // CONTROL 1 -- the clock. One second before the deadline the contract would revert with
        // ERR_NOT_CANCELLED, so the refusal must stay.
        let early = check(&pmp(false, serde_json::Value::Null), RESULT_END)
            .expect_err("on-the-deadline must still refuse: the contract wants strictly greater");
        let early = early.to_string();
        assert!(
            early.contains("grace period has not passed") && early.contains("1787246775"),
            "the refusal must name the condition and the deadline it read from chain: {early}"
        );

        // CONTROL 2 -- a resolved event is not cancellable however late it is.
        let resolved = check(&pmp(false, serde_json::json!("1")), RESULT_END + 100_000)
            .expect_err("a resolved outcome must still refuse");
        assert!(
            resolved.to_string().contains("already resolved"),
            "the refusal must name the outcome, not the clock: {resolved}"
        );

        // The route that already worked keeps working, and does not depend on the clock at all.
        check(&pmp(true, serde_json::Value::Null), 0)
            .expect("an already-cancelled PMP was always allowed and must stay allowed");
    }

    #[tokio::test]
    async fn oracle_cancel_validation_checks_direct_pre_and_post_reads() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let before_pmp =
            serde_json::json!({"approved": true, "isCancelled": false, "resolvedOutcome": null});
        let after_pmp =
            serde_json::json!({"approved": true, "isCancelled": true, "resolvedOutcome": null});
        let before_event = serde_json::json!({"count": "2"});
        let after_event = serde_json::json!({"count": "1"});

        let before = super::validate_oracle_cancel_preflight(&before_pmp, &before_event).unwrap();
        let postread = |pmp, event, exact_active| {
            super::validate_oracle_cancel_postread(before, Some(pmp), Some(event), exact_active)
                .unwrap()
        };
        assert_eq!(before, 2);
        assert_eq!(postread(&after_pmp, &after_event, false), (true, Some(1)));
        assert_eq!(postread(&before_pmp, &before_event, true), (false, Some(2)));
        assert_eq!(
            postread(&after_pmp, &after_event, true),
            (false, Some(1)),
            "an unrelated decrement must not confirm this PMP while its exact OEL entry remains"
        );

        assert!(super::validate_oracle_cancel_preflight(
            &serde_json::json!({"approved": true, "isCancelled": true, "resolvedOutcome": null}),
            &before_event
        )
        .is_err());
        assert!(super::validate_oracle_cancel_preflight(
            &before_pmp,
            &serde_json::json!({"count": "0"})
        )
        .is_err());
        assert!(super::validate_oracle_cancel_postread(
            before,
            Some(&serde_json::json!({
                "approved": true,
                "isCancelled": false,
                "resolvedOutcome": "1"
            })),
            Some(&before_event),
            true
        )
        .is_err());
        let cancel_posts = AtomicUsize::new(0);
        let cancel_post = || async {
            cancel_posts.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({}))
        };
        assert!(super::submit_oracle_cancel_after_validation(
            super::validate_oracle_cancel_preflight(
                &before_pmp,
                &serde_json::json!({"count": "0"}),
            ),
            cancel_post(),
        )
        .await
        .is_err());
        assert_eq!(cancel_posts.load(Ordering::SeqCst), 0);

        let count = super::submit_oracle_cancel_after_validation(
            super::validate_oracle_cancel_preflight(&before_pmp, &before_event),
            cancel_post(),
        )
        .await
        .unwrap();
        assert_eq!(cancel_posts.load(Ordering::SeqCst), 1);
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn oracle_delete_validation_requires_zero_count_deadline_and_absence() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let deletable = serde_json::json!({"count": "0", "deadline": "1000"});
        assert!(super::validate_oracle_delete_preflight(&deletable, 1_001).is_ok());
        assert!(super::validate_oracle_delete_preflight(&deletable, 1_000).is_err());
        assert!(super::validate_oracle_delete_preflight(
            &serde_json::json!({"count": "1", "deadline": "1000"}),
            1_001
        )
        .is_err());
        assert!(super::validate_oracle_delete_postread(&deletable, None).unwrap());
        assert!(!super::validate_oracle_delete_postread(&deletable, Some(&deletable)).unwrap());
        assert!(super::validate_oracle_delete_postread(
            &deletable,
            Some(&serde_json::json!({"count": "1", "deadline": "1000"}))
        )
        .is_err());

        let delete_posts = AtomicUsize::new(0);
        let delete_post = || async {
            delete_posts.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({}))
        };
        assert!(super::submit_oracle_delete_after_validation(
            super::validate_oracle_delete_preflight(&deletable, 1_000),
            delete_post(),
        )
        .await
        .is_err());
        assert_eq!(delete_posts.load(Ordering::SeqCst), 0);

        super::submit_oracle_delete_after_validation(
            super::validate_oracle_delete_preflight(&deletable, 1_001),
            delete_post(),
        )
        .await
        .unwrap();
        assert_eq!(delete_posts.load(Ordering::SeqCst), 1);
    }
}
