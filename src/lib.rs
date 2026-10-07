//! watcher-s1: a truthful command wrapper that watches for stalls, prompts
//! and masked failures, and reports them on a JSON sideband.

/// The agent guide, printed by `watcher-s1 guide`.
pub const GUIDE: &str = include_str!("../docs/agent-guide.md");

pub mod breaker;
pub mod cli;
pub mod config;
pub mod control;
pub mod detect;
pub mod diag;
pub mod event;
pub mod follow;
pub mod http;
pub mod judge;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod outbox;
pub mod probe;
pub mod questions;
pub mod ring;
pub mod s1;
pub mod supervise;
