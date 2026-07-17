//! `dexdo` CLI surface, split out of `main.rs`(PR3, move-only / behavior-stable, `refactoring-plan.md`).
//! `main.rs` keeps parse + logging + shutdown signal + dispatch; the subcommand argument structs, helpers,
//! and command handlers live here.

pub(crate) mod admin;
pub(crate) mod args;
pub(crate) mod audit;
pub(crate) mod buyer;
pub(crate) mod close;
pub(crate) mod commands;
pub(crate) mod dashboard;
pub(crate) mod deals;
pub(crate) mod indexer;
pub(crate) mod machine;
pub(crate) mod market_views;
pub(crate) mod markets;
pub(crate) mod monitor;
pub(crate) mod note;
pub(crate) mod note_cmd;
pub(crate) mod oracle;
pub(crate) mod orders;
pub(crate) mod policy;
pub(crate) mod recover;
pub(crate) mod reports;
pub(crate) mod seller;
pub(crate) mod seller_policy;
pub(crate) mod support;
