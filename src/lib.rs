//! watcher-s1: a truthful command wrapper that watches for stalls, prompts
//! and masked failures, and reports them on a JSON sideband.

pub mod breaker;
pub mod cli;
pub mod config;
pub mod detect;
pub mod event;
pub mod http;
pub mod probe;
pub mod questions;
pub mod ring;
pub mod s1;
pub mod supervise;
