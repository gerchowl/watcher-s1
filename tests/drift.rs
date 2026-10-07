//! Keeps the agent guide and README honest: every flag they mention exists
//! in the CLI, and the guide's event table matches `event.schema.json`.

use clap::CommandFactory;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use watcher_s1::cli::Cli;

const GUIDE: &str = include_str!("../docs/agent-guide.md");
const README: &str = include_str!("../README.md");

/// Flags of other tools the docs mention. Anything else must be ours.
const FOREIGN_FLAGS: &[&str] = &[
    "--git",   // cargo install --git
    "--repo",  // gh issue create
    "--label", // gh issue create
    "--title", // gh issue create
    "--body",  // gh issue create
    "--yes",   // the guide's "re-run non-interactively" hint
    "--tag",   // cargo install
    "--check", // cargo fmt --check
    "--pid",   // README: the attach mode watcher-s1 deliberately lacks
];

/// Every `--long` flag of the CLI, subcommands included.
fn real_flags() -> BTreeSet<String> {
    fn walk(c: &clap::Command, out: &mut BTreeSet<String>) {
        out.extend(c.get_arguments().filter_map(|a| a.get_long()).map(|l| format!("--{l}")));
        c.get_subcommands().for_each(|s| walk(s, out));
    }
    let mut out = BTreeSet::new();
    walk(&Cli::command(), &mut out);
    out.extend(["--help", "--version"].map(String::from));
    out
}

/// `--flag` mentions that count: inside backticks anywhere, or on a
/// fenced-code line that runs watcher-s1.
fn mentioned_flags(doc: &str) -> BTreeSet<String> {
    let flag = Regex::new(r"--[a-z][a-z0-9-]*").unwrap();
    let span = Regex::new(r"`([^`\n]+)`").unwrap();
    let mut out = BTreeSet::new();
    let mut in_code = false;
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        let hay: Vec<&str> = if in_code {
            if line.contains("watcher-s1") {
                vec![line]
            } else {
                vec![]
            }
        } else {
            span.captures_iter(line).map(|c| c.get(1).unwrap().as_str()).collect()
        };
        out.extend(
            hay.iter()
                .flat_map(|h| flag.find_iter(h))
                .map(|m| m.as_str().to_string()),
        );
    }
    out
}

fn check_flags(name: &str, doc: &str) {
    let real = real_flags();
    let bad: Vec<_> = mentioned_flags(doc)
        .into_iter()
        .filter(|f| !real.contains(f) && !FOREIGN_FLAGS.contains(&f.as_str()))
        .collect();
    assert!(
        bad.is_empty(),
        "{name} mentions flags that watcher-s1 does not have: {bad:?} (typo, or add to FOREIGN_FLAGS if another tool's)"
    );
}

#[test]
fn guide_flags_are_real() {
    check_flags("docs/agent-guide.md", GUIDE);
}

#[test]
fn readme_flags_are_real() {
    check_flags("README.md", README);
}

#[test]
fn the_allowlist_has_no_dead_entries() {
    let seen: BTreeSet<String> = mentioned_flags(GUIDE)
        .union(&mentioned_flags(README))
        .cloned()
        .collect();
    let dead: Vec<_> = FOREIGN_FLAGS.iter().filter(|f| !seen.contains(**f)).collect();
    assert!(dead.is_empty(), "FOREIGN_FLAGS entries no doc mentions: {dead:?}");
}

#[test]
fn the_scan_sees_subcommand_flags() {
    let real = real_flags();
    for f in ["--new", "--posttooluse", "--s1-url", "--silence", "--events"] {
        assert!(real.contains(f), "{f}");
    }
    let seen = mentioned_flags("`--nope` and\n```bash\nwatcher-s1 --also-nope -- x\ncargo --skipped\n```\n");
    assert_eq!(seen, BTreeSet::from(["--nope".into(), "--also-nope".into()]));
}

fn schema_enum(schema: &Value, key: &str) -> BTreeSet<String> {
    schema["properties"][key]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("schema has no enum for {key}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// Backticked values of the guide's first two table columns (`state`, `reason`).
fn table_values() -> (BTreeSet<String>, BTreeSet<String>) {
    let tick = Regex::new(r"`([a-z_]+)`").unwrap();
    let (mut states, mut reasons) = (BTreeSet::new(), BTreeSet::new());
    for row in GUIDE.lines().filter(|l| l.starts_with('|')) {
        let cells: Vec<&str> = row.split('|').skip(1).collect();
        if cells.len() < 3 || cells[0].trim() == "`state`" {
            continue;
        }
        for (cell, set) in [(cells[0], &mut states), (cells[1], &mut reasons)] {
            set.extend(tick.captures_iter(cell).map(|c| c[1].to_string()));
        }
    }
    (states, reasons)
}

#[test]
fn guide_event_table_matches_the_schema() {
    let schema: Value = serde_json::from_str(watcher_s1::event::SCHEMA_JSON).unwrap();
    let (states, reasons) = table_values();
    assert_eq!(
        states,
        schema_enum(&schema, "state"),
        "guide states vs event.schema.json"
    );
    assert_eq!(
        reasons,
        schema_enum(&schema, "reason"),
        "guide reasons vs event.schema.json"
    );
}
