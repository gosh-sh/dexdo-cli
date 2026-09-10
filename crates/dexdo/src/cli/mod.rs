//! `dexdo` CLI surface, split out of `main.rs` (PR3, move-only / behavior-stable, `refactoring-plan.md`).
//! `main.rs` keeps parse + logging + shutdown signal + dispatch; the subcommand argument structs, helpers,
//! and command handlers live here.

/// Set by `--no-color`, read together with the environment variable below.

/// A flag and a variable, because neither reaches everywhere: the variable is not exported in a
/// one-off run or a copied runbook line, and the flag cannot be set by an environment that wraps
/// this client. Whichever says "no colour" wins, and neither can turn colour back on.
static NO_COLOR_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record `--no-color` for the rest of the process. Called once, from `main`.
pub(crate) fn set_no_color(requested: bool) {
    if requested {
        NO_COLOR_FLAG.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The standard `NO_COLOR` override, or this run's `--no-color`: either disables terminal styling.
pub(crate) fn no_color_requested() -> bool {
    NO_COLOR_FLAG.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

pub(crate) mod accumulator;
pub(crate) mod admin;
pub(crate) mod args;
pub(crate) mod audit;
pub(crate) mod buyer;
// Pick one row with the arrow keys, for the questions whose answer the client already holds.
pub(crate) mod choose;
pub(crate) mod close;
pub(crate) mod commands;
pub(crate) mod dashboard;
pub(crate) mod data_dir;
pub(crate) mod deals;
// the SECOND atomic writer. `deals.rs` holds the same replace-rather-than-overwrite property
// as `note.rs`, by its own separate `rename`, and PR1605's guard covers only the first. Declared
// here rather than inside `deals`, so that a revert of the file this guards cannot take the guard
// with it.
#[cfg(test)]
mod deals_write_atomicity_1810;
pub(crate) mod indexer;
// Whether this run may ask the operator anything: one answer for the whole process.
// Which values the operator gave and which clap supplied, for every command that has to decide
// whether a question is already answered.
pub(crate) mod command_line;
pub(crate) mod interaction;
// Names in the output that are worth clicking, rendered as links only where that means something.
pub(crate) mod link;
pub(crate) mod machine;
pub(crate) mod market_views;
pub(crate) mod markets;
pub(crate) mod model_registry;
pub(crate) mod monitor;
// One snapshot of chain figures through every renderer a person reads, checked for the unit.
pub(crate) mod note;
pub(crate) mod note_cmd;
#[cfg(test)]
mod one_unit_everywhere;
// Which note a command spends from, when the operator did not say: the rows, and the refusal for a
// run that cannot ask.
pub(crate) mod note_pick;
// Reading a function's own text in a test that guards call ORDER, ending the body at its closing
// brace instead of at whichever neighbouring item happened to follow it.

// The file sits at the crate root because BOTH targets need it: `cli` is the binary's, `registry`
// is the library's, and a binary and a library are separate crates that cannot see each other's
// modules. One file, declared twice.
#[cfg(test)]
#[path = "../source_probe.rs"]
pub(crate) mod source_probe;
// how far the declared-network check reaches, frozen by name. Declared here rather than
// inside any one of the files it counts, because it counts SEVERAL and would go uncollected the
// moment that file was reverted.
#[cfg(test)]
mod network_check_reach_1613;

/// Gosh.ai onboarding happens where the deployment says it happens, or it refuses before it
/// prints a link the operator would follow into a flow that cannot end.
#[cfg(test)]
mod goshai_onboarding_is_where_the_manifest_says_1639;
pub(crate) mod oracle;
pub(crate) mod orders;
pub(crate) mod policy;
// The rules of engagement asked as situations rather than as dotted field paths: same file, same
// values, wording an operator can answer without reading the source.
pub(crate) mod policy_questions;
// What a long command is doing, in place of a scrolling log: a checklist of its declared steps and
// one live line under it.
pub(crate) mod progress;
// The prover writes its phase timings to stderr and takes no verbosity setting; this folds them
// into the status line rather than letting them scroll.
pub(crate) mod progress_capture;
// The cursor arithmetic behind the display, kept apart from the API commands call.
pub(crate) mod progress_draw;
// The checklist model: declared steps, ticked as they are passed. Pure.
pub(crate) mod progress_plan;
// a display belongs to the command that built it, not to the process. Declared here so a
// revert of `progress` cannot take the regression with it.
#[cfg(test)]
mod progress_one_display_per_thread_1695;
// shared provenance vocabulary for chain-backed book views and indexer depth.
pub(crate) mod fold_completeness;
pub(crate) mod provenance;
// QR-as-image: the probe-reply parsing, the protocol decision, the encoders and the terminal I/O.
// `qr_compact` has no pure half -- every one of its functions takes or returns a `qrcode::QrCode`.
// That used to matter, because `qrcode` was an optional dependency and this module could not be
// compiled without the feature that pulled it in. There are no optional dependencies now.
pub(crate) mod qr_compact;
pub(crate) mod qr_display;
// What a refusal says to the operator, above what it says to a machine.

pub(crate) mod recover;
pub(crate) mod refusal;
pub(crate) mod reports;
// Where a key lives: the operating system's store where the machine has one that keeps a secret
// until it is deleted, and an owner-only file where it does not. Both branches are ordinary -- a
// seller runs headless, and a headless server has no keychain.
pub(crate) mod secret_store;
pub(crate) mod seller;
pub(crate) mod seller_policy;
pub(crate) mod settlement_receipt;
pub(crate) mod style;
pub(crate) mod support;
// the wallet provider model, the durable active binding, and the fail-fast.
pub(crate) mod wallet;
// the shared Hot check-and-fund mechanism and its durable journal. Its callers are the two
// commands that spend a Hot: `note deploy` and `note topup`.
pub(crate) mod wallet_funding;
// re-audit items 4 and 8: how the funding wait treats a read that got no answer, and what its
// failure hands a machine consumer. Declared beside the two modules it drives - `wallet_funding`
// and `machine` - rather than inside either, so reverting one of them leaves the regression
// compiled and failing instead of silently uncollected.
#[cfg(test)]
mod wallet_funding_wait_failures_334;
// Gosh.ai onboarding, reached from `wallet onboard gosh-ai` through `wallet::provider_flow`.
// The `allow(dead_code)` that used to sit here is gone with the wiring it was waiting for.
pub(crate) mod wallet_goshai;
// the manual provider, reached from `wallet onboard manual` through `wallet::provider_flow`.
// The funding half still has no caller, until the shared provider-aware funding step is wired into
// `note deploy` / `note topup`; it is covered by this module's own tests.
#[allow(dead_code)]
pub(crate) mod wallet_manual;
// (rymkapro, 2026-08-17): `wallet remove-archived` deletes the only local keys to a Hot, so it
// must hold the funding wallet's turn across BOTH observations the deletion rests on. Declared here
// rather than inside `wallet`, because it pins a property shared by `wallet` and `note_cmd` - the
// lock's key - and a regression living in one of them would go uncollected the moment that module
// was reverted.
#[cfg(test)]
mod wallet_remove_archived_lock_334;
// the gas floor a funding wallet's own outgoing messages need, and the rule that the client
// states it before it commits a spend. Declared here rather than inside one of the modules it pins
// because it pins THREE - `wallet_funding`'s floor, `accumulator`'s sell and `note_cmd`'s top-up
// preflight - and a regression living in any one of them would be uncollected the moment that
// module was reverted.
#[cfg(test)]
mod wallet_gas_floor_1392;
// 681: the binding id and the Hot address a wallet onboard/rebind committed are the result of that
// command, not journal entries. Declared here rather than inside `wallet`, so that a revert of
// `wallet.rs` cannot take the regression with it -- which is exactly how the demotion in PR1440
// reached a live gate unnoticed.
#[cfg(test)]
mod wallet_binding_result_681;
// (PR715): the authenticated bee-session onboarding the `ackinacki-wallet` provider runs.
pub(crate) mod wallet_onboarding;
#[cfg(windows)]
pub(crate) mod windows_secret_file;
