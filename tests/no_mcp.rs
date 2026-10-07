//! The build without the `mcp` feature still parses `watcher-s1 mcp` (the
//! CLI and docs stay one surface) and refuses it clearly.
#![cfg(not(feature = "mcp"))]

mod common;
use common::BIN;
use std::process::Command;

#[test]
fn mcp_without_the_feature_exits_2_with_a_reason() {
    let out = Command::new(BIN).arg("mcp").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout must stay clean");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no MCP server"));
}
