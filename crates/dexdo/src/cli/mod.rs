//! `dexdo` CLI surface, split out of `main.rs` (PR3, move-only / behavior-stable, `refactoring-plan.md`).
//! `main.rs` keeps parse + logging + shutdown signal + dispatch; the subcommand argument structs, helpers,
//! and command handlers live here.

pub(crate) mod args;
pub(crate) mod audit;
pub(crate) mod commands;
pub(crate) mod dashboard;
pub(crate) mod deals;
pub(crate) mod indexer;
pub(crate) mod machine;
pub(crate) mod note;
pub(crate) mod policy;
pub(crate) mod support;
