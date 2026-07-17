//! `dexdo` CLI: `seller` and `buyer` subcommands, each with first-class flags
//! `--mock-model` and `--mock-chain`.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod cli;
use cli::args::*;
use cli::buyer::{run_buyer, run_subscription};
use cli::commands::*;
use cli::machine;
use cli::policy;

#[derive(Parser)]
#[command(
    name = "dexdo",
    version,
    about = "dexdo -- private inference market: seller and buyer clients"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Seller client: gateway, authorization, stream handover(headless, R12).
    Seller(SellerArgs),
    /// Buyer client: endpoint decryption, challenge signing, stream reception.
    Buyer(BuyerArgs),
    /// Monitor(R14): human-readable state view **from the loaded note** --
    /// own offers, deals, by-fact tokens, exposure. Read-only, moves nothing.
    Monitor(MonitorArgs),
    /// Doctor: read-only shellnet version/pin and market freshness checks. Alias: `health`.
    #[command(alias = "health")]
    Doctor(DoctorArgs),
    /// Provision: bring up the InferenceOrderBook + RootModel + per-deal TokenContract for a
    /// market -- **all note-funded** from the seller note's own ECC[2] (directive, no operator wallet,
    /// no giver in the operate path) -- and write the manifest with the deployed, active TC address.
    Provision(ProvisionArgs),
    /// Deploy-market: deploy the per-model `InferenceOrderBook`(the shared market for a model) if absent --
    /// note-funded, the explicit "list this model" step before a seller posts offers. Idempotent
    /// (the book address is deterministic from `model_hash`; already-deployed -> no-op).
    #[command(name = "deploy-market")]
    DeployMarket(MarketDeployArgs),
    /// Destroy: the seller CLOSES a STOPped deal's per-deal `TokenContract` --
    /// `TokenContract::destroy(payoutAddress)` -> `selfdestruct`. **DESTRUCTIVE / BURNS:** after the 4.0.8
    /// fund-10 sizing, the unrecovered deploy remainder is expected to be negligible; `--acknowledge-burn`
    /// is an explicit operator confirmation, not a fail-closed guard for the old over-funded reserve.
    /// Run after the deal STOPs(`!_opened && !_disputed`); seller-signed.
    Destroy(DestroyArgs),
    /// Recover: the BUYER signs **STOP** on an orphaned OPEN deal (its buyer process died, but the
    /// note/key are intact) -- the normal buyer-STOP split, **without** placing a new buy -- so a stuck
    /// deal can be closed and the seller can then `destroy` it. Buyer-signed; fails closed if the deal is
    /// not OPEN / is disputed / the note is not the deal's buyer. (Seller-vanished mid-stream is instead the
    /// contract's `reclaimOnTimeout`/`STREAM_TIMEOUT`.)
    Recover(RecoverArgs),
    /// Dispute: the BUYER opens an on-chain dispute on an OPEN deal -- `streamDispute` -> `TC.dispute()`
    /// LOCKS both notes until `releaseDispute`/arbitration. The anti-scam lever for an observed
    /// substitution/fraud -- strictly stronger than `recover`'s STOP (which still pays for delivered
    /// ticks). Buyer-signed; fails closed if the deal is not OPEN / already disputed / the note isn't the buyer.
    Dispute(DisputeArgs),
    /// Reclaim: the BUYER reclaims escrow on seller no-show. OPEN abandoned deals use
    /// `streamReclaim` -> `TC.reclaimOnTimeout()` after `STREAM_TIMEOUT`; funded-but-never-opened deals use
    /// `streamCleanup` -> `TC.cleanupUnopened()` after `MATCH_OPEN_TIMEOUT`. Buyer-signed; fails closed locally
    /// on ownership + the timer.
    Reclaim(ReclaimArgs),
    /// ReleaseDispute: the SELLER concedes a disputed deal -- `TokenContract.releaseDispute()` unlocks
    /// both notes and returns the contested tick/deposit to the buyer. Seller-signed; fails closed if the deal
    /// is not disputed or the signing key is not the TC seller.
    ReleaseDispute(ReleaseDisputeArgs),
    /// WithdrawShell: the SELLER withdraws finalized `_finalizedOwed` SHELL from a deal TC. This moves
    /// seller proceeds; `destroy` remains the close/selfdestruct path.
    WithdrawShell(WithdrawShellArgs),
    /// Markets: read-only discovery of active model order books and depth.
    Markets(MarketsArgs),
    /// Market: render ONE model's order book as the human-readable box table(`dexdo market <model>`).
    Market(MarketArgs),
    /// Executable-book: list current buyer-executable asks for one model book.
    #[command(name = "executable-book")]
    ExecutableBook(ExecutableBookArgs),
    /// Quote: compute an executable quote over current order-book depth.
    Quote(QuoteArgs),
    /// Market-data: read-only Dodex indexer discovery/cache for inference model books.
    #[command(name = "market-data", alias = "indexer")]
    MarketData(MarketDataArgs),
    /// Orders: list/show/cancel this note's resting inference orders.
    Orders(OrdersArgs),
    /// Subscription: place/status/cancel recurring inference buy subscriptions.
    Subscription(SubscriptionArgs),
    /// Deals: list durable local deal handles saved by seller/buyer flows.
    Deals(DealsArgs),
    /// History: secret-free local trading history, filterable by note/model.
    History(HistoryArgs),
    /// Dashboard: loopback-only read view of local buyer/seller streams.
    Dashboard(DashboardArgs),
    /// Status: read current state for a local deal handle or raw TokenContract.
    Status(StatusArgs),
    /// Close: role-aware close/recovery action for a local deal handle or raw TokenContract.
    Close(CloseArgs),
    /// Export: secret-free JSON/Markdown evidence for one local deal handle or raw TokenContract.
    Export(ExportArgs),
    /// Note: manage the actor's shellnet `PrivateNote`s. `note deploy` mints a wallet-funded PN
    /// in-process through `gosh.ackinacki` and folds it into a `DEXDO_PN_POOL` the `seller`/`buyer` consume.
    Note(NoteArgs),
    /// Oracle: deploy OracleEventList-backed range PMPs tied to inference order books and resolve them.
    Oracle(OracleArgs),
    /// Persistent failure policy for real buyer/seller startup and runtime recovery choices.
    Policy(PolicyArgs),
}

impl Command {
    fn machine_operation(&self) -> Option<&'static str> {
        match self {
            Command::Markets(args) if args.json => Some(machine::OP_MARKETS),
            Command::Quote(args) if args.json => Some(machine::OP_QUOTE),
            Command::Status(args) if args.json => Some(machine::OP_STATUS),
            Command::Close(args) if args.json => Some(machine::OP_CLOSE),
            Command::Buyer(args) if args.json => Some(machine::OP_BUYER_START),
            _ => None,
        }
    }
}

fn raw_machine_operation(args: &[std::ffi::OsString]) -> Option<&'static str> {
    for (idx, arg) in args.iter().enumerate().skip(1) {
        let op = match arg.to_str()? {
            "markets" => machine::OP_MARKETS,
            "quote" => machine::OP_QUOTE,
            "buyer" => machine::OP_BUYER_START,
            "status" => machine::OP_STATUS,
            "close" => machine::OP_CLOSE,
            _ => continue,
        };
        if args
            .iter()
            .skip(idx + 1)
            .any(|a| a.to_str() == Some("--json"))
        {
            return Some(op);
        }
        return None;
    }
    None
}

/// The operator close signal for `dexdo buyer --local-listen`: SIGINT(Ctrl-C) **and** SIGTERM
/// (systemd/container/operator). `serve()` runs graceful shutdown on it, then awaits `session.settle("shutdown")`
/// -- so a `SIGTERM` does NOT bypass the awaited funds-safety terminal into best-effort `Drop`. Non-Unix: Ctrl-C.
#[cfg(unix)]
pub(crate) async fn operator_shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
pub(crate) async fn operator_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args = std::env::args_os().collect::<Vec<_>>();
    let cli = match Cli::try_parse_from(raw_args.clone()) {
        Ok(cli) => cli,
        Err(err) => {
            if let Some(operation) = raw_machine_operation(&raw_args) {
                if operation == machine::OP_BUYER_START {
                    let mut events = machine::BuyerEventWriter::new();
                    events.error(
                        machine::OP_BUYER_START,
                        machine::ErrorCode::InvalidArgument,
                        serde_json::json!({}),
                    )?;
                } else {
                    machine::print_short_error(operation, machine::ErrorCode::InvalidArgument)?;
                }
                std::process::exit(err.exit_code());
            }
            err.exit();
        }
    };
    let machine_operation = cli.command.machine_operation();
    let env_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if machine_operation.is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let result = match cli.command {
        Command::Seller(args) => run_seller(args).await,
        Command::Buyer(args) => run_buyer(args).await,
        Command::Monitor(args) => run_monitor(args).await,
        Command::Doctor(args) => run_doctor(args).await,
        Command::Provision(args) => run_provision(args).await,
        Command::DeployMarket(args) => run_market_deploy(args).await,
        Command::Destroy(args) => run_destroy(args).await,
        Command::Recover(args) => run_recover(args).await,
        Command::Dispute(args) => run_dispute(args).await,
        Command::Reclaim(args) => run_reclaim(args).await,
        Command::ReleaseDispute(args) => run_release_dispute(args).await,
        Command::WithdrawShell(args) => run_withdraw_shell(args).await,
        Command::Markets(args) => run_markets(args).await,
        Command::Market(args) => run_market(args).await,
        Command::ExecutableBook(args) => run_executable_book(args).await,
        Command::Quote(args) => run_quote(args).await,
        Command::MarketData(args) => run_market_data(args).await,
        Command::Orders(args) => run_orders(args).await,
        Command::Subscription(args) => run_subscription(args).await,
        Command::Deals(args) => run_deals(args).await,
        Command::History(args) => run_history(args).await,
        Command::Dashboard(args) => run_dashboard(args).await,
        Command::Status(args) => run_status(args).await,
        Command::Close(args) => run_close(args).await,
        Command::Export(args) => run_export(args).await,
        Command::Note(args) => match args.command {
            NoteCommand::Balance(b) => run_note_balance(b).await,
            NoteCommand::Deploy(d) => run_note_deploy(d).await,
            NoteCommand::Recover(r) => run_note_recover(r).await,
            NoteCommand::Withdraw(w) => run_note_withdraw(w).await,
            NoteCommand::StreamLocks(s) => run_note_stream_locks(s).await,
        },
        Command::Oracle(args) => run_oracle(args).await,
        Command::Policy(args) => policy::run_policy(args),
    };
    if let Err(err) = result {
        if machine::is_printed_error(&err) {
            std::process::exit(1);
        }
        if let Some(operation) = machine_operation {
            let code = machine::classify_error(operation, &err);
            machine::print_error(operation, code, &err)?;
            std::process::exit(1);
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod buyer_mode_tests {
    use crate::cli::support::oneshot_real_upstream_guard;
    use serde_json::json;

    /// one-shot `dexdo buyer`(no `--local-listen`) is promptless -- it must fail closed
    /// against a real seller(no `--mock-model`) with an actionable error, instead of a deep gateway
    /// `InvalidArgument`. `--local-listen`(consumer API supplies the prompt) and `--mock-model` both pass.
    #[test]
    fn oneshot_real_upstream_rejected_promptless() {
        let err = oneshot_real_upstream_guard(false, false).unwrap_err();
        assert!(err.contains("--local-listen"), "{err}");
        assert!(err.contains(""), "{err}");
        // one-shot + --mock-model -> OK(the mock seller synthesizes tokens for the promptless stream).
        assert!(oneshot_real_upstream_guard(false, true).is_ok());
        // --local-listen(consumer API supplies the prompt per request) -> OK regardless of --mock-model.
        assert!(oneshot_real_upstream_guard(true, false).is_ok());
        assert!(oneshot_real_upstream_guard(true, true).is_ok());
    }

    #[test]
    fn user_visible_onchain_error_contains_numeric_exit_code() {
        let err = dexdo_core::validate_onchain_submit_response(json!({
            "result": {"exit_code": 321, "aborted": true}
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("exit_code=321"), "{err}");
        assert!(err.contains("ERR_ALREADY_OPEN"), "{err}");
    }
}

#[cfg(test)]
mod recovery_cli_tests {
    use super::{Cli, Command};
    use clap::Parser;

    /// `dexdo dispute` and `dexdo reclaim` parse as buyer-signed subcommands, accepting `--market` (the
    /// single TC source, mirroring `recover`) and `--token-contract` as the alternative.
    #[test]
    fn dispute_reclaim_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "dispute",
            "--market",
            "m.json",
            "--note-addr",
            "0:b",
        ])
        .expect("dispute --market parses");
        assert!(matches!(c.command, Command::Dispute(_)));
        let c = Cli::try_parse_from([
            "dexdo",
            "reclaim",
            "--market",
            "m.json",
            "--note-addr",
            "0:b",
        ])
        .expect("reclaim --market parses");
        assert!(matches!(c.command, Command::Reclaim(_)));
        assert!(Cli::try_parse_from(["dexdo", "dispute", "--token-contract", "0:tc"]).is_ok());
        assert!(Cli::try_parse_from(["dexdo", "reclaim", "--token-contract", "0:tc"]).is_ok());
    }

    /// seller-side dispute/payout commands parse with either a market manifest or explicit TC.
    #[test]
    fn seller_dispute_payout_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "release-dispute",
            "--market",
            "m.json",
            "--note-addr",
            "0:s",
        ])
        .expect("release-dispute --market parses");
        assert!(matches!(c.command, Command::ReleaseDispute(_)));
        let c = Cli::try_parse_from([
            "dexdo",
            "withdraw-shell",
            "--token-contract",
            "0:tc",
            "--note-addr",
            "0:s",
            "--amount",
            "100",
        ])
        .expect("withdraw-shell --token-contract parses");
        assert!(matches!(c.command, Command::WithdrawShell(_)));
    }
}

#[cfg(test)]
mod note_cli_tests {
    use super::{Cli, Command};
    use crate::cli::args::NoteCommand;
    #[cfg(feature = "shellnet")]
    use crate::cli::args::{IdentityArgs, NoteWithdrawArgs};
    use clap::Parser;
    use std::path::PathBuf;

    const NOTE: &str = "0:2222222222222222222222222222222222222222222222222222222222222222";
    const DEST_HALF_1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const DEST_HALF_2: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    /// `dexdo note deploy` parses with the required wallet/pool flags and defaults for
    /// nominal/token-type/endpoint.
    #[test]
    fn note_deploy_subcommand_parses() {
        let c = Cli::try_parse_from([
            "dexdo",
            "note",
            "deploy",
            "--multisig-address",
            "0:wallet",
            "--multisig-key",
            "w.keys.json",
            "--pool",
            "pn_pool.json",
        ])
        .expect("note deploy parses with required flags + defaults");
        let Command::Note(n) = c.command else {
            panic!("expected Command::Note");
        };
        let NoteCommand::Deploy(d) = n.command else {
            panic!("expected NoteCommand::Deploy");
        };
        assert_eq!(d.nominal, "N100");
        assert_eq!(d.token_type, "nackl");
        assert_eq!(d.endpoint, "shellnet.ackinacki.org");
        assert_eq!(d.multisig_key, Some(PathBuf::from("w.keys.json")));
        assert_eq!(d.multisig_seed_file, None);
        assert_eq!(d.recovery, None);
        assert!(!d.simulate_interrupt_after_spend_before_pool);
        let c = Cli::try_parse_from([
            "dexdo",
            "note",
            "deploy",
            "--multisig-address",
            "0:wallet",
            "--multisig-seed-file",
            r"C:\Users\operator\wallet.seed",
            "--pool",
            "pn_pool.json",
            "--recovery",
            "pn_pool.json.recovery.json",
        ])
        .expect("note deploy parses seed-file path");
        let Command::Note(n) = c.command else {
            panic!("expected Command::Note");
        };
        let NoteCommand::Deploy(d) = n.command else {
            panic!("expected NoteCommand::Deploy");
        };
        assert_eq!(d.multisig_key, None);
        assert_eq!(
            d.multisig_seed_file,
            Some(PathBuf::from(r"C:\Users\operator\wallet.seed"))
        );
        assert_eq!(
            d.recovery,
            Some(PathBuf::from("pn_pool.json.recovery.json"))
        );
        // The wallet address, one key input, and pool are required -- omitting any fails parse.
        assert!(Cli::try_parse_from(["dexdo", "note", "deploy", "--pool", "p.json"]).is_err());
        assert!(Cli::try_parse_from([
            "dexdo",
            "note",
            "deploy",
            "--multisig-address",
            "0:wallet",
            "--multisig-key",
            "w.keys.json",
            "--multisig-seed-file",
            "wallet.seed",
            "--pool",
            "pn_pool.json",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "dexdo",
            "note",
            "deploy",
            "--multisig-address",
            "0:wallet",
            "--multisig-key",
            "w.keys.json",
            "--pool",
            "pn_pool.json",
            "--onboard-bin",
            "/bin/onboard_user_shellnet",
        ])
        .is_err());
    }

    /// `dexdo note recover` finalizes from the crash-safe state without wallet credentials.
    #[test]
    fn note_recover_subcommand_parses() {
        let c = Cli::try_parse_from([
            "dexdo",
            "note",
            "recover",
            "--recovery",
            "pn_pool.json.recovery.json",
            "--pool",
            "pn_pool.json",
        ])
        .expect("note recover parses");
        let Command::Note(n) = c.command else {
            panic!("expected Command::Note");
        };
        let NoteCommand::Recover(r) = n.command else {
            panic!("expected NoteCommand::Recover");
        };
        assert_eq!(r.recovery, PathBuf::from("pn_pool.json.recovery.json"));
        assert_eq!(r.pool, PathBuf::from("pn_pool.json"));
        assert!(Cli::try_parse_from(["dexdo", "note", "recover", "--pool", "p.json"]).is_err());
        assert!(
            Cli::try_parse_from(["dexdo", "note", "recover", "--recovery", "state.json"]).is_err()
        );
    }

    /// `dexdo note withdraw` is owner-signed money movement, so the parser surface and destination
    /// normalization contract are pinned separately from the live shellnet submit.
    #[test]
    fn note_withdraw_subcommand_parses_and_requires_destination() {
        let to = format!("{DEST_HALF_1}::{DEST_HALF_2}");
        let c = Cli::try_parse_from([
            "dexdo",
            "note",
            "withdraw",
            "--note-addr",
            NOTE,
            "--note-key",
            "note.key",
            "--to",
            &to,
            "--contracts",
            "contracts/custom.json",
        ])
        .expect("note withdraw parses");
        let Command::Note(n) = c.command else {
            panic!("expected Command::Note");
        };
        let NoteCommand::Withdraw(w) = n.command else {
            panic!("expected NoteCommand::Withdraw");
        };
        assert_eq!(w.identity.note_addr.as_deref(), Some(NOTE));
        assert_eq!(w.identity.note_key, Some(PathBuf::from("note.key")));
        assert_eq!(w.to, to);
        assert_eq!(w.contracts, PathBuf::from("contracts/custom.json"));
        assert!(Cli::try_parse_from([
            "dexdo",
            "note",
            "withdraw",
            "--note-addr",
            NOTE,
            "--note-key",
            "note.key",
        ])
        .is_err());

        let normalized =
            dexdo_core::normalize_wallet_address(&format!("{DEST_HALF_1}::{DEST_HALF_2}"))
                .expect("half1::half2 normalizes");
        assert_eq!(normalized, format!("0:{DEST_HALF_2}"));
        assert!(dexdo_core::normalize_wallet_address("not-a-wallet").is_err());
    }

    /// `dexdo note balance` is address-only and read-only at the parser surface.
    #[test]
    fn note_balance_subcommand_parses_and_requires_note_addr() {
        let c = Cli::try_parse_from([
            "dexdo",
            "note",
            "balance",
            "--note-addr",
            NOTE,
            "--contracts",
            "contracts/custom.json",
            "--endpoint",
            "new-shellnet.example",
        ])
        .expect("note balance parses");
        let Command::Note(n) = c.command else {
            panic!("expected Command::Note");
        };
        let NoteCommand::Balance(b) = n.command else {
            panic!("expected NoteCommand::Balance");
        };
        assert_eq!(b.note_addr, NOTE);
        assert_eq!(b.contracts, PathBuf::from("contracts/custom.json"));
        assert_eq!(b.endpoint.as_deref(), Some("new-shellnet.example"));
        assert!(Cli::try_parse_from(["dexdo", "note", "balance"]).is_err());
        assert!(Cli::try_parse_from([
            "dexdo",
            "note",
            "balance",
            "--note-addr",
            NOTE,
            "--note-key",
            "note.key",
        ])
        .is_err());
    }

    #[cfg(feature = "shellnet")]
    #[tokio::test]
    async fn note_withdraw_runtime_guards_fail_before_chain() {
        let err = crate::cli::commands::run_note_withdraw(NoteWithdrawArgs {
            identity: IdentityArgs {
                note_key: Some(PathBuf::from("note.key")),
                note_index: 0,
                note_addr: None,
            },
            to: format!("{DEST_HALF_1}::{DEST_HALF_2}"),
            contracts: PathBuf::from("contracts/deployed.shellnet.json"),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--note-addr"), "{err}");

        let err = crate::cli::commands::run_note_withdraw(NoteWithdrawArgs {
            identity: IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: Some(NOTE.to_string()),
            },
            to: format!("{DEST_HALF_1}::{DEST_HALF_2}"),
            contracts: PathBuf::from("contracts/deployed.shellnet.json"),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--note-key"), "{err}");

        let err = crate::cli::commands::run_note_withdraw(NoteWithdrawArgs {
            identity: IdentityArgs {
                note_key: Some(PathBuf::from("missing-note.key")),
                note_index: 0,
                note_addr: Some(NOTE.to_string()),
            },
            to: "not-a-wallet".to_string(),
            contracts: PathBuf::from("contracts/deployed.shellnet.json"),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--to"), "{err}");
    }
}

#[cfg(test)]
mod doctor_cli_tests {
    use super::{Cli, Command};
    use clap::Parser;

    /// `dexdo doctor` is the read-only shellnet health guard; `health` is kept as an alias.
    #[test]
    fn doctor_subcommand_parses() {
        let c = Cli::try_parse_from(["dexdo", "doctor"]).expect("doctor parses");
        assert!(matches!(c.command, Command::Doctor(_)));
        let c = Cli::try_parse_from(["dexdo", "health", "--market", "m.json"])
            .expect("health alias parses");
        assert!(matches!(c.command, Command::Doctor(_)));
    }

    #[test]
    fn doctor_accepts_non_shellnet_endpoint() {
        let c = Cli::try_parse_from([
            "dexdo",
            "doctor",
            "--network",
            "https://new-shellnet.example/",
        ])
        .expect("doctor accepts an endpoint through --network");
        let Command::Doctor(args) = c.command else {
            panic!("expected Command::Doctor");
        };
        assert_eq!(args.network, "https://new-shellnet.example/");
    }
}

#[cfg(test)]
mod market_orders_cli_tests {
    use super::{Cli, Command};
    use crate::cli::args::{
        MarketDataCommand, MarketDataOutput, OrdersCommand, SubscriptionCommand,
        DEFAULT_CHAIN_READ_TIMEOUT_SECS,
    };
    use clap::Parser;
    use std::path::PathBuf;

    /// market discovery and executable quote commands parse the intended read-only surfaces.
    #[test]
    fn markets_and_quote_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo", "markets", "--market", "m1.json", "--market", "m2.json",
        ])
        .expect("markets with manifests parses");
        let Command::Markets(m) = c.command else {
            panic!("expected Command::Markets");
        };
        assert_eq!(
            m.market,
            vec![PathBuf::from("m1.json"), PathBuf::from("m2.json")]
        );
        assert_eq!(
            m.read_timeout.read_timeout_secs,
            DEFAULT_CHAIN_READ_TIMEOUT_SECS
        );

        let c = Cli::try_parse_from(["dexdo", "quote", "--market", "m.json", "--ticks", "3"])
            .expect("quote by ticks parses");
        let Command::Quote(q) = c.command else {
            panic!("expected Command::Quote");
        };
        assert_eq!(q.market, Some(PathBuf::from("m.json")));
        assert_eq!(q.ticks, Some(3));
        assert_eq!(q.budget, None);
        assert_eq!(
            q.read_timeout.read_timeout_secs,
            DEFAULT_CHAIN_READ_TIMEOUT_SECS
        );

        let c = Cli::try_parse_from([
            "dexdo",
            "quote",
            "--market",
            "m.json",
            "--read-timeout-secs",
            "7",
            "--ticks",
            "3",
            "--model-registry-validation",
            "registry.json",
            "--model-registry-address",
            "0:9999999999999999999999999999999999999999999999999999999999999999",
        ])
        .expect("quote registry validation flags parse");
        let Command::Quote(q) = c.command else {
            panic!("expected Command::Quote");
        };
        assert_eq!(
            q.registry.model_registry_validation,
            Some(PathBuf::from("registry.json"))
        );
        assert_eq!(
            q.registry.model_registry_address.as_deref(),
            Some("0:9999999999999999999999999999999999999999999999999999999999999999")
        );
        assert_eq!(q.read_timeout.read_timeout_secs, 7);

        let c = Cli::try_parse_from([
            "dexdo",
            "quote",
            "--model",
            "qwen",
            "--note-addr",
            "0:note",
            "--budget",
            "100000",
        ])
        .expect("quote by budget parses");
        assert!(matches!(c.command, Command::Quote(_)));

        let c = Cli::try_parse_from([
            "dexdo",
            "market",
            "--read-timeout-secs",
            "9",
            "--note-addr",
            "0:note",
            "qwen",
        ])
        .expect("market read timeout parses");
        let Command::Market(m) = c.command else {
            panic!("expected Command::Market");
        };
        assert_eq!(m.read_timeout.read_timeout_secs, 9);
        assert!(Cli::try_parse_from([
            "dexdo",
            "market",
            "--read-timeout-secs",
            "0",
            "--note-addr",
            "0:note",
            "qwen",
        ])
        .is_err());

        let c = Cli::try_parse_from([
            "dexdo",
            "executable-book",
            "--market",
            "m.json",
            "--ticks",
            "8",
            "--max-price-per-tick",
            "1000",
            "--read-timeout-secs",
            "11",
            "qwen",
        ])
        .expect("executable-book parses");
        let Command::ExecutableBook(b) = c.command else {
            panic!("expected Command::ExecutableBook");
        };
        assert_eq!(b.market, Some(PathBuf::from("m.json")));
        assert_eq!(b.ticks, 8);
        assert_eq!(b.max_price_per_tick, 1000);
        assert_eq!(b.read_timeout.read_timeout_secs, 11);
    }

    /// read-only Dodex indexer discovery parses independently of shellnet signing flags.
    #[test]
    fn market_data_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "market-data",
            "--indexer-url",
            "http://dodex-dev.ackinacki.org:8080",
            "--output",
            "json",
            "--endpoint",
            "new-shellnet.example",
            "list",
            "--producer",
            "qwen",
            "--status",
            "TRADING",
            "--cursor",
            "MTc4Mjg4NDY0MTAwMDAwMDo0",
            "--limit",
            "50",
        ])
        .expect("market-data list parses");
        let Command::MarketData(args) = c.command else {
            panic!("expected Command::MarketData");
        };
        assert_eq!(
            args.indexer_url.as_deref(),
            Some("http://dodex-dev.ackinacki.org:8080")
        );
        assert_eq!(args.output, MarketDataOutput::Json);
        assert_eq!(args.endpoint.as_deref(), Some("new-shellnet.example"));
        assert_eq!(args.timeout_ms, 10_000);
        let MarketDataCommand::List {
            producer,
            status,
            cursor,
            limit,
        } = args.command
        else {
            panic!("expected list");
        };
        assert_eq!(producer.as_deref(), Some("qwen"));
        assert_eq!(status.as_deref(), Some("TRADING"));
        assert_eq!(cursor.as_deref(), Some("MTc4Mjg4NDY0MTAwMDAwMDo0"));
        assert_eq!(limit, Some(50));

        let c = Cli::try_parse_from([
            "dexdo",
            "market-data",
            "list",
            "--output",
            "json",
            "--timeout-ms",
            "10000",
            "--limit",
            "1",
        ])
        .expect("market-data list accepts shared flags after subcommand");
        let Command::MarketData(args) = c.command else {
            panic!("expected Command::MarketData");
        };
        assert_eq!(args.output, MarketDataOutput::Json);
        assert_eq!(args.timeout_ms, 10_000);
        assert!(matches!(
            args.command,
            MarketDataCommand::List { limit: Some(1), .. }
        ));

        let c = Cli::try_parse_from([
            "dexdo",
            "indexer",
            "show",
            "0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76",
            "--output",
            "json",
        ])
        .expect("indexer alias show parses");
        assert!(matches!(
            c.command,
            Command::MarketData(crate::cli::args::MarketDataArgs {
                output: MarketDataOutput::Json,
                command: MarketDataCommand::Show { .. },
                ..
            })
        ));

        let c = Cli::try_parse_from([
            "dexdo",
            "market-data",
            "depth",
            "0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76",
            "--output",
            "json",
            "--limit",
            "5",
        ])
        .expect("market-data depth parses");
        assert!(matches!(
            c.command,
            Command::MarketData(crate::cli::args::MarketDataArgs {
                output: MarketDataOutput::Json,
                command: MarketDataCommand::Depth { limit: Some(5), .. },
                ..
            })
        ));

        assert!(Cli::try_parse_from(["dexdo", "market-data", "list", "--limit", "0"]).is_err());
        assert!(Cli::try_parse_from([
            "dexdo",
            "market-data",
            "depth",
            "0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76",
            "--limit",
            "1001",
        ])
        .is_err());
    }

    /// own-order lifecycle commands parse as one note-scoped surface.
    #[test]
    fn orders_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "orders",
            "--note-addr",
            "0:note",
            "--market",
            "m.json",
            "list",
        ])
        .expect("orders list parses");
        let Command::Orders(o) = c.command else {
            panic!("expected Command::Orders");
        };
        assert!(matches!(o.command, OrdersCommand::List));
        assert_eq!(
            o.read_timeout.read_timeout_secs,
            DEFAULT_CHAIN_READ_TIMEOUT_SECS
        );

        let c = Cli::try_parse_from([
            "dexdo",
            "orders",
            "--note-addr",
            "0:note",
            "--read-timeout-secs",
            "11",
            "--model",
            "qwen",
            "show",
            "7",
        ])
        .expect("orders show parses");
        let Command::Orders(o) = c.command else {
            panic!("expected Command::Orders");
        };
        assert!(matches!(o.command, OrdersCommand::Show { order_id: 7 }));
        assert_eq!(o.read_timeout.read_timeout_secs, 11);

        let c = Cli::try_parse_from([
            "dexdo",
            "orders",
            "--note-addr",
            "0:note",
            "--note-key",
            "note.secret",
            "--market",
            "m.json",
            "cancel",
            "7",
        ])
        .expect("orders cancel parses");
        assert!(matches!(
            c.command,
            Command::Orders(crate::cli::args::OrdersArgs {
                command: OrdersCommand::Cancel { order_id: 7 },
                ..
            })
        ));

        let c = Cli::try_parse_from([
            "dexdo",
            "orders",
            "--note-addr",
            "0:note",
            "--note-key",
            "note.secret",
            "--market",
            "m.json",
            "cancel-all",
        ])
        .expect("orders cancel-all parses");
        assert!(matches!(
            c.command,
            Command::Orders(crate::cli::args::OrdersArgs {
                command: OrdersCommand::CancelAll,
                ..
            })
        ));
    }

    /// subscription lifecycle commands parse the note-scoped inference surface.
    #[test]
    fn subscription_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "subscription",
            "--note-addr",
            "0:note",
            "--note-key",
            "note.secret",
            "--market",
            "m.json",
            "place",
            "--max-price-per-tick",
            "1000",
            "--ticks",
            "4",
            "--auto-renew",
        ])
        .expect("subscription place parses");
        let Command::Subscription(s) = c.command else {
            panic!("expected Command::Subscription");
        };
        let SubscriptionCommand::Place(p) = s.command else {
            panic!("expected subscription place");
        };
        assert_eq!(s.market, Some(PathBuf::from("m.json")));
        assert_eq!(
            s.read_timeout.read_timeout_secs,
            DEFAULT_CHAIN_READ_TIMEOUT_SECS
        );
        assert_eq!(p.max_price_per_tick, 1000);
        assert_eq!(p.ticks, Some(4));
        assert_eq!(p.budget, None);
        assert!(p.auto_renew);

        let c = Cli::try_parse_from([
            "dexdo",
            "subscription",
            "--note-addr",
            "0:note",
            "--market",
            "m.json",
            "place",
            "--note-key",
            "note.secret",
            "--max-price-per-tick",
            "1000",
            "--ticks",
            "4",
        ])
        .expect("subscription place accepts --note-key after place");
        let Command::Subscription(s) = c.command else {
            panic!("expected Command::Subscription");
        };
        let SubscriptionCommand::Place(p) = s.command else {
            panic!("expected subscription place");
        };
        assert_eq!(s.identity.note_key, None);
        assert_eq!(p.note_key, Some(PathBuf::from("note.secret")));

        let c = Cli::try_parse_from([
            "dexdo",
            "subscription",
            "--note-addr",
            "0:note",
            "--read-timeout-secs",
            "12",
            "--model",
            "qwen",
            "status",
            "7",
        ])
        .expect("subscription status parses");
        assert!(matches!(
            c.command,
            Command::Subscription(crate::cli::args::SubscriptionArgs {
                command: SubscriptionCommand::Status { order_id: 7 },
                read_timeout: crate::cli::args::ChainReadTimeoutArgs {
                    read_timeout_secs: 12
                },
                ..
            })
        ));

        let c = Cli::try_parse_from([
            "dexdo",
            "subscription",
            "--note-addr",
            "0:note",
            "--note-key",
            "note.secret",
            "--market",
            "m.json",
            "cancel",
            "7",
        ])
        .expect("subscription cancel parses");
        assert!(matches!(
            c.command,
            Command::Subscription(crate::cli::args::SubscriptionArgs {
                command: SubscriptionCommand::Cancel { order_id: 7 },
                ..
            })
        ));

        assert!(Cli::try_parse_from([
            "dexdo",
            "subscription",
            "--note-addr",
            "0:note",
            "--market",
            "m.json",
            "place",
            "--max-price-per-tick",
            "1000",
            "--ticks",
            "4",
            "--budget",
            "4100",
        ])
        .is_err());
    }
}

#[cfg(test)]
mod deal_handle_cli_tests {
    use super::{Cli, Command};
    use crate::cli::args::{ContinuityModeArg, DealRoleArg, ExportFormatArg};
    use clap::Parser;
    use std::path::PathBuf;

    /// durable local deal-handle commands parse without low-level address reassembly for the handle path,
    /// while raw TokenContract close can still be made explicit with role/note.
    #[test]
    fn deal_handle_subcommands_parse() {
        let c =
            Cli::try_parse_from(["dexdo", "deals", "--deals-dir", "deals"]).expect("deals parses");
        let Command::Deals(d) = c.command else {
            panic!("expected Command::Deals");
        };
        assert_eq!(d.deals_dir, Some(PathBuf::from("deals")));

        let c = Cli::try_parse_from(["dexdo", "status", "deal-0-abc"]).expect("status parses");
        let Command::Status(status) = c.command else {
            panic!("expected Command::Status");
        };
        assert_eq!(status.contracts, None);

        let c = Cli::try_parse_from([
            "dexdo",
            "status",
            "deal-0-abc",
            "--contracts",
            "contracts/custom.json",
        ])
        .expect("status --contracts parses");
        let Command::Status(status) = c.command else {
            panic!("expected Command::Status");
        };
        assert_eq!(
            status.contracts,
            Some(PathBuf::from("contracts/custom.json"))
        );

        let c = Cli::try_parse_from([
            "dexdo",
            "close",
            "0:tc",
            "--role",
            "buyer",
            "--note-addr",
            "0:note",
            "--note-key",
            "note.secret",
        ])
        .expect("close raw token contract parses");
        let Command::Close(close) = c.command else {
            panic!("expected Command::Close");
        };
        assert_eq!(close.role, Some(DealRoleArg::Buyer));
        assert_eq!(close.note_addr.as_deref(), Some("0:note"));
        assert_eq!(close.contracts, None);

        let c = Cli::try_parse_from([
            "dexdo",
            "history",
            "--deals-dir",
            "deals",
            "--note",
            "0:note",
            "--model",
            "qwen/qwen3-32b",
        ])
        .expect("history parses");
        let Command::History(history) = c.command else {
            panic!("expected Command::History");
        };
        assert_eq!(history.deals_dir, Some(PathBuf::from("deals")));
        assert_eq!(history.note.as_deref(), Some("0:note"));
        assert_eq!(history.model.as_deref(), Some("qwen/qwen3-32b"));

        let c = Cli::try_parse_from([
            "dexdo",
            "dashboard",
            "--listen",
            "127.0.0.1:0",
            "--deals-dir",
            "deals",
        ])
        .expect("dashboard parses");
        let Command::Dashboard(dashboard) = c.command else {
            panic!("expected Command::Dashboard");
        };
        assert_eq!(dashboard.listen.to_string(), "127.0.0.1:0");
        assert_eq!(dashboard.deals_dir, Some(PathBuf::from("deals")));

        let c = Cli::try_parse_from([
            "dexdo",
            "export",
            "--deal",
            "deal-0-abc",
            "--format",
            "md",
            "--contracts",
            "contracts/custom.json",
        ])
        .expect("export parses");
        let Command::Export(export) = c.command else {
            panic!("expected Command::Export");
        };
        assert_eq!(export.deal, "deal-0-abc");
        assert_eq!(export.format, ExportFormatArg::Md);
        assert_eq!(
            export.contracts,
            Some(PathBuf::from("contracts/custom.json"))
        );
    }

    /// PR212: explicit `buyer --resume` remains a no-new-buy connect path; model-only resume is covered
    /// by the shellnet resume validation tests.
    #[test]
    fn buyer_resume_explicit_deal_parses() {
        let c = Cli::try_parse_from([
            "dexdo",
            "buyer",
            "--resume",
            "--token-contract",
            "0:tc",
            "--frame-model",
            "qwen--qwen3--32b",
        ])
        .expect("buyer --resume parses with an explicit deal");
        let Command::Buyer(buyer) = c.command else {
            panic!("expected Command::Buyer");
        };
        assert!(buyer.resume);
        assert_eq!(buyer.token_contract.as_deref(), Some("0:tc"));
        assert_eq!(buyer.frame_model.as_deref(), Some("qwen--qwen3--32b"));
    }

    #[test]
    fn buyer_model_alias_and_models_config_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "buyer",
            "--mock-model",
            "--mock-chain",
            "--token-contract",
            "0:tc",
            "--model",
            "qwen--qwen3--32b",
            "--models",
            "custom-models.json",
        ])
        .expect("buyer accepts --model alias plus --models config path");
        let Command::Buyer(buyer) = c.command else {
            panic!("expected Command::Buyer");
        };
        assert_eq!(buyer.frame_model.as_deref(), Some("qwen--qwen3--32b"));
        assert_eq!(buyer.models, PathBuf::from("custom-models.json"));
    }

    #[test]
    fn buyer_continuity_mode_parses_defaults_and_rejects_unknown_values() {
        let c = Cli::try_parse_from([
            "dexdo",
            "buyer",
            "--resume",
            "--token-contract",
            "0:tc",
            "--frame-model",
            "qwen--qwen3--32b",
        ])
        .expect("buyer default continuity mode parses");
        let Command::Buyer(buyer) = c.command else {
            panic!("expected Command::Buyer");
        };
        assert_eq!(buyer.continuity_mode, ContinuityModeArg::Proactive);

        let c = Cli::try_parse_from([
            "dexdo",
            "buyer",
            "--resume",
            "--token-contract",
            "0:tc",
            "--frame-model",
            "qwen--qwen3--32b",
            "--continuity-mode",
            "on-demand",
        ])
        .expect("buyer on-demand continuity mode parses");
        let Command::Buyer(buyer) = c.command else {
            panic!("expected Command::Buyer");
        };
        assert_eq!(buyer.continuity_mode, ContinuityModeArg::OnDemand);

        let c = Cli::try_parse_from([
            "dexdo",
            "buyer",
            "--resume",
            "--token-contract",
            "0:tc",
            "--frame-model",
            "qwen--qwen3--32b",
            "--continuity-mode",
            "proactive",
        ])
        .expect("buyer proactive continuity mode parses");
        let Command::Buyer(buyer) = c.command else {
            panic!("expected Command::Buyer");
        };
        assert_eq!(buyer.continuity_mode, ContinuityModeArg::Proactive);

        assert!(Cli::try_parse_from([
            "dexdo",
            "buyer",
            "--resume",
            "--token-contract",
            "0:tc",
            "--frame-model",
            "qwen--qwen3--32b",
            "--continuity-mode",
            "automatic",
        ])
        .is_err());
    }

    #[test]
    fn seller_gateway_advertise_defaults_to_listen() {
        let c = Cli::try_parse_from([
            "dexdo",
            "seller",
            "--mock-chain",
            "--mock-model",
            "--token-contract",
            "0:tc",
            "--gateway-listen",
            "0.0.0.0:8443",
        ])
        .expect("seller parses with gateway-listen only");
        let Command::Seller(seller) = c.command else {
            panic!("expected Command::Seller");
        };
        assert_eq!(seller.gateway_listen.to_string(), "0.0.0.0:8443");
        assert_eq!(seller.gateway_advertise_addr(), "0.0.0.0:8443");
    }

    #[test]
    fn seller_gateway_advertise_accepts_public_host_port() {
        let c = Cli::try_parse_from([
            "dexdo",
            "seller",
            "--mock-chain",
            "--mock-model",
            "--token-contract",
            "0:tc",
            "--gateway-listen",
            "127.0.0.1:8443",
            "--gateway-advertise",
            "seller.example.net:443",
        ])
        .expect("seller parses public advertise host:port");
        let Command::Seller(seller) = c.command else {
            panic!("expected Command::Seller");
        };
        assert_eq!(seller.gateway_listen.to_string(), "127.0.0.1:8443");
        assert_eq!(seller.gateway_advertise_addr(), "seller.example.net:443");
    }

    #[test]
    fn seller_gateway_advertise_rejects_malformed_host_port() {
        assert!(Cli::try_parse_from([
            "dexdo",
            "seller",
            "--mock-chain",
            "--mock-model",
            "--token-contract",
            "0:tc",
            "--gateway-advertise",
            "seller.example.net",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "dexdo",
            "seller",
            "--mock-chain",
            "--mock-model",
            "--token-contract",
            "0:tc",
            "--gateway-advertise",
            "seller.example.net:notaport",
        ])
        .is_err());
    }
}

#[cfg(test)]
mod oracle_cli_tests {
    use super::{Cli, Command};
    use crate::cli::args::{OracleCommand, OracleProvisionArgs};
    use clap::Parser;
    use std::path::PathBuf;

    /// oracle/PMP lifecycle commands parse as a single shellnet surface.
    #[test]
    fn oracle_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "oracle",
            "provision",
            "--note-key",
            "note.key",
            "--note-addr",
            "0:note",
            "--oracle-key",
            "oracle.key",
            "--oracle-name",
            "weekly-qwen",
            "--market",
            "market.json",
            "--event-name",
            "qwen-weekly-price",
            "--deadline",
            "1900000000",
            "--bound",
            "100",
            "--bound",
            "200",
            "--outcome",
            "below",
            "--outcome",
            "middle",
            "--outcome",
            "above",
            "--initial-stake",
            "10000000",
            "--initial-stake",
            "10000000",
            "--initial-stake",
            "10000000",
            "--output",
            "oracle-market.json",
        ])
        .expect("oracle provision parses");
        let Command::Oracle(args) = c.command else {
            panic!("expected oracle command");
        };
        let OracleCommand::Provision(p) = args.command else {
            panic!("expected oracle provision");
        };
        let OracleProvisionArgs {
            oracle_name,
            bounds,
            outcome_names,
            initial_stakes,
            token_type,
            output,
            ..
        } = *p;
        assert_eq!(oracle_name, "weekly-qwen");
        assert_eq!(bounds, ["100", "200"]);
        assert_eq!(outcome_names, ["below", "middle", "above"]);
        assert_eq!(initial_stakes, [10_000_000, 10_000_000, 10_000_000]);
        assert_eq!(token_type, 1);
        assert_eq!(output, PathBuf::from("oracle-market.json"));

        let c = Cli::try_parse_from(["dexdo", "oracle", "state", "--manifest", "oracle.json"])
            .expect("oracle state parses");
        assert!(matches!(
            c.command,
            Command::Oracle(crate::cli::args::OracleArgs {
                command: OracleCommand::State(_)
            })
        ));

        let c = Cli::try_parse_from([
            "dexdo",
            "oracle",
            "resolve",
            "--manifest",
            "oracle.json",
            "--oracle-key",
            "oracle.key",
        ])
        .expect("oracle resolve parses");
        assert!(matches!(
            c.command,
            Command::Oracle(crate::cli::args::OracleArgs {
                command: OracleCommand::Resolve(_)
            })
        ));
    }
}

#[cfg(test)]
mod deposit_tests {
    use crate::cli::support::{
        deposit_per_deploy, ensure_provision_deposit_covered, DEFAULT_DEPOSIT_SHELLS,
        MIN_DEPLOY_SHELLS, SHELL_UNIT,
    };

    #[test]
    fn default_deposit_clears_the_floor() {
        let pd = deposit_per_deploy(DEFAULT_DEPOSIT_SHELLS).expect("default deposit must be valid");
        assert_eq!(pd, (DEFAULT_DEPOSIT_SHELLS / 2) * SHELL_UNIT);
        assert!(pd >= MIN_DEPLOY_SHELLS * SHELL_UNIT);
    }

    #[test]
    fn below_floor_deposit_is_rejected_fail_closed() {
        // A deposit whose deposit/2 lands below the constant-derived MIN_DEPLOY_SHELLS floor must error, not
        // silently proceed into an under-funded deploy. Asserted relative to the constant so it survives a
        // re-sizing of the floor.
        assert!(
            deposit_per_deploy(MIN_DEPLOY_SHELLS).is_err(),
            "half the floor per deploy -- must be rejected"
        );
        assert!(
            deposit_per_deploy(MIN_DEPLOY_SHELLS * 2 - 2).is_err(),
            "one SHELL/deploy below the floor -- must be rejected"
        );
        // Exactly at the floor(2xMIN_DEPLOY_SHELLS) is the minimum accepted.
        assert!(deposit_per_deploy(MIN_DEPLOY_SHELLS * 2).is_ok());
        assert!(deposit_per_deploy(MIN_DEPLOY_SHELLS * 2 - 1).is_err());
    }

    #[test]
    fn overflow_deposit_errors_not_silently_clamps() {
        assert!(
            deposit_per_deploy(u128::MAX).is_err(),
            "overflow must error, not saturate"
        );
    }

    #[test]
    fn provision_deposit_guard_checks_exact_deploy_amount_without_magic_reserve() {
        let need = DEFAULT_DEPOSIT_SHELLS * SHELL_UNIT;
        assert!(
            ensure_provision_deposit_covered(need, DEFAULT_DEPOSIT_SHELLS, 0).is_ok(),
            "zero-price deals have no seller probe commission"
        );
        assert!(ensure_provision_deposit_covered(need - 1, DEFAULT_DEPOSIT_SHELLS, 0).is_err());
        assert!(ensure_provision_deposit_covered(need + 1, DEFAULT_DEPOSIT_SHELLS, 0).is_ok());
    }

    #[test]
    fn provision_deposit_guard_reserves_contract_probe_commission() {
        let deploy_need = DEFAULT_DEPOSIT_SHELLS * SHELL_UNIT;
        let price_per_tick = 1000;
        let probe_commission = 25;
        let err =
            ensure_provision_deposit_covered(deploy_need, DEFAULT_DEPOSIT_SHELLS, price_per_tick)
                .expect_err("exact deploy allocation leaves no seller probe commission");
        let msg = err.to_string();
        assert!(msg.contains("seller probe commission"), "{msg}");
        assert!(msg.contains("price_per_tick=1000"), "{msg}");
        assert!(ensure_provision_deposit_covered(
            deploy_need + probe_commission,
            DEFAULT_DEPOSIT_SHELLS,
            price_per_tick,
        )
        .is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::support::{
        check_market_model_match, consumer_api_token_budget, default_endpoints_path,
        resolve_endpoints_file, resolve_market_fields,
    };
    use clap::{CommandFactory, Parser};

    fn subcommand_long_help(name: &str) -> String {
        let mut command = Cli::command();
        command
            .find_subcommand_mut(name)
            .expect("subcommand exists")
            .render_long_help()
            .to_string()
    }

    fn nested_subcommand_long_help(path: &[&str]) -> String {
        let mut command = Cli::command();
        let mut current = &mut command;
        for name in path {
            current = current
                .find_subcommand_mut(name)
                .expect("nested subcommand exists");
        }
        current.render_long_help().to_string()
    }

    #[test]
    fn root_version_flag_is_available_for_release_smoke() {
        let err = Cli::command()
            .try_get_matches_from(["dexdo", "--version"])
            .expect_err("--version should render the package version");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn explicit_endpoints_file_used_and_parent_created() {
        // D6: an explicit path is used as is, and a missing parent directory is created
        // (otherwise the mock write of `endpoints`/`*.chainstate.json` would fail on a fresh machine).
        let base = std::env::temp_dir().join(format!("dexdo-eps-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let nested = base.join("sub").join("eps.json");
        let got = resolve_endpoints_file(Some(nested.clone())).expect("resolve explicit");
        assert_eq!(got, nested, "explicit path is not rewritten");
        assert!(
            nested.parent().unwrap().is_dir(),
            "parent directory created"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn policy_subcommands_parse() {
        let c = Cli::try_parse_from([
            "dexdo",
            "policy",
            "init",
            "--role",
            "buyer",
            "--path",
            "policy.json",
        ])
        .expect("policy init parses");
        assert!(matches!(c.command, Command::Policy(_)));
        let c = Cli::try_parse_from(["dexdo", "policy", "show"]).expect("policy show parses");
        assert!(matches!(c.command, Command::Policy(_)));
        let c = Cli::try_parse_from(["dexdo", "policy", "edit"]).expect("policy edit parses");
        assert!(matches!(c.command, Command::Policy(_)));
    }

    #[test]
    fn default_endpoints_path_is_under_platform_app_dir() {
        // Pure function(no directory creation) -- no side effects in the test.
        // ProjectDirs == None only without a home directory; otherwise the path is under dexdo/endpoints.json.
        if let Ok(p) = default_endpoints_path() {
            assert!(
                p.ends_with("endpoints.json"),
                "ends with endpoints.json: {p:?}"
            );
            assert!(
                p.to_string_lossy().to_lowercase().contains("dexdo"),
                "path contains the dexdo app segment: {p:?}"
            );
        }
    }

    #[test]
    fn seller_model_help_matches_real_shellnet_requirement() {
        let help = subcommand_long_help("seller");
        assert!(
            help.contains("Required on real shellnet even with `--mock-model`"),
            "{help}"
        );
        assert!(
            help.contains("optional only for the `--mock-chain --mock-model` demo"),
            "{help}"
        );
        assert!(!help.contains("Not needed with `--mock-model`"), "{help}");
    }

    #[test]
    fn seller_gateway_advertise_help_documents_public_host_port() {
        let help = subcommand_long_help("seller");
        assert!(help.contains("--gateway-advertise <HOST:PORT>"), "{help}");
        assert!(help.contains("Defaults to --gateway-listen"), "{help}");
    }

    #[test]
    fn listen_help_documents_seller_buyer_equivalence() {
        let seller = subcommand_long_help("seller");
        assert!(
            seller.contains("equivalent of buyer --local-listen"),
            "{seller}"
        );
        let buyer = subcommand_long_help("buyer");
        assert!(
            buyer.contains("equivalent of seller --gateway-listen"),
            "{buyer}"
        );
    }

    #[test]
    fn buyer_model_alias_is_visible_in_help() {
        let help = subcommand_long_help("buyer");
        assert!(help.contains("--frame-model <FRAME_MODEL>"), "{help}");
        assert!(help.contains("Alias: --model"), "{help}");
        assert!(help.contains("[aliases: --model]"), "{help}");
    }

    #[test]
    fn note_deploy_token_type_help_lists_values() {
        let help = nested_subcommand_long_help(&["note", "deploy"]);
        assert!(help.contains("--token-type <TOKEN_TYPE>"), "{help}");
        assert!(
            help.contains("[possible values: nackl, shell, usdc]"),
            "{help}"
        );
    }

    #[test]
    fn buyer_continuity_mode_help_documents_operator_tradeoff() {
        let help = subcommand_long_help("buyer");
        assert!(help.contains("--continuity-mode <MODE>"), "{help}");
        assert!(help.contains("[default: proactive]"), "{help}");
        assert!(
            help.contains("[possible values: proactive, on-demand]"),
            "{help}"
        );
        assert!(
            help.contains("proactive keeps a warm next deal ready"),
            "{help}"
        );
        assert!(help.contains("may pre-buy while idle"), "{help}");
        assert!(
            help.contains("on-demand buys only after active/recent consumer traffic"),
            "{help}"
        );
        assert!(help.contains("first request after idle may wait"), "{help}");
    }

    #[test]
    fn provision_deposit_help_is_short_and_unit_explicit() {
        let help = subcommand_long_help("provision");
        assert!(help.contains("whole SHELL"), "{help}");
        assert!(help.contains("1 SHELL = 1e9 raw"), "{help}");
        assert!(help.contains("not raw nano/vmshell"), "{help}");
        assert!(
            help.contains("Unused deploy remainder burns at `destroy`"),
            "{help}"
        );
        assert!(!help.contains("fund-10"), "{help}");
        assert!(!help.contains("MIN_BALANCE"), "{help}");
        assert!(!help.contains("REGISTER_FORWARD_VALUE"), "{help}");
    }

    #[test]
    fn consumer_api_budget_is_ticks_times_canonical_tick_size() {
        assert_eq!(
            consumer_api_token_budget(8),
            8 * dexdo_core::DobParams::canonical().tick_size
        );
        assert_eq!(consumer_api_token_budget(u128::MAX), u64::MAX);
    }

    /// Issue: `--market` feeds `token_contract` + `frame_model` from a provision manifest verbatim
    /// (no hand-editing), and the explicit flags are used when `--market` is absent.
    #[test]
    fn market_loader_resolves_fields() {
        let valid = dexdo_core::MarketManifest {
            network: "shellnet".into(),
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: dexdo_core::model_hash_for("qwen/qwen3-32b"),
            inference_order_book: "0:ob".into(),
            root_model: "0:rm".into(),
            token_contract: "0:tc".into(),
            seller_note: "0:n".into(),
            nonce: 1,
            price_per_tick: 1000,
            max_ticks: 8,
        };
        let dir = std::env::temp_dir().join(format!("dexdo-market-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, m: &dexdo_core::MarketManifest| {
            let p = dir.join(name);
            std::fs::write(&p, m.to_json().unwrap()).unwrap();
            p
        };

        // --market provides token_contract, frame_model AND the deal nonce, verbatim.
        let p = write("ok.json", &valid);
        let (tc, fm, nonce) = resolve_market_fields(Some(&p), None, None).unwrap();
        assert_eq!(tc, "0:tc");
        assert_eq!(fm.as_deref(), Some("qwen/qwen3-32b"));
        assert_eq!(
            nonce,
            Some(1),
            "--market must preserve the manifest's deal nonce for the seller"
        );

        // Flags path: token_contract + optional frame_model(the seller passes None for frame_model).
        // The explicit path carries no nonce -- the seller must supply it via `--nonce`.
        let (tc, fm, nonce) = resolve_market_fields(None, Some("0:flag"), Some("m")).unwrap();
        assert_eq!((tc.as_str(), fm.as_deref()), ("0:flag", Some("m")));
        assert!(
            nonce.is_none(),
            "explicit --token-contract path yields no nonce (the seller needs --nonce)"
        );
        let (tc, fm, _nonce) = resolve_market_fields(None, Some("0:flag"), None).unwrap();
        assert_eq!((tc.as_str(), fm), ("0:flag", None));

        // Neither --market nor --token-contract -> explicit error.
        assert!(resolve_market_fields(None, None, None).is_err());

        // Fail-loud: --market is mutually exclusive with the explicit flags(no silent precedence).
        assert!(resolve_market_fields(Some(&p), Some("0:other"), None).is_err());
        assert!(resolve_market_fields(Some(&p), None, Some("other")).is_err());

        // Corrupt manifest(model_hash inconsistent with frame_model) is rejected by load.
        let mut bad = valid.clone();
        bad.model_hash = "0xdeadbeef".into();
        let pb = write("bad.json", &bad);
        assert!(resolve_market_fields(Some(&pb), None, None).is_err());

        // Empty token_contract is rejected.
        let mut empty = valid.clone();
        empty.token_contract = String::new();
        let pe = write("empty.json", &empty);
        assert!(resolve_market_fields(Some(&pe), None, None).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue(review): the seller fails closed when the `--market` manifest's model does not match
    /// the `--model` it would serve(no posting the manifest's TC into the wrong order book).
    #[test]
    fn market_model_match_fails_closed() {
        // No manifest model(flags path) or a matching one -- OK.
        assert!(check_market_model_match(None, "qwen/qwen3-32b", "qwen").is_ok());
        assert!(check_market_model_match(Some("qwen/qwen3-32b"), "qwen/qwen3-32b", "qwen").is_ok());
        // Mismatch -- fail closed.
        let err = check_market_model_match(Some("qwen/qwen3-32b"), "llama/llama-3", "llama")
            .unwrap_err()
            .to_string();
        assert!(err.contains("wrong model"), "{err}");
    }
}
