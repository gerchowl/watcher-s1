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

/// Which flag set a mention is checked against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    /// Backticked prose: any flag of any subcommand will do.
    Any,
    /// A plain `watcher-s1 ...` wrap invocation.
    Wrap,
    /// `watcher-s1 <sub> ...`.
    Sub(String),
}

type Mention = (String, Scope);

const SUBCOMMANDS: &[&str] = &["follow", "guide", "judge", "config"];

/// The scope in effect after `watcher-s1` at byte `at` of `line`.
fn scope_after(line: &str, at: usize) -> Scope {
    let rest = line[at + "watcher-s1".len()..].trim_start();
    let word = rest.split_whitespace().next().unwrap_or("");
    match SUBCOMMANDS.iter().find(|s| **s == word) {
        Some(s) => Scope::Sub((*s).into()),
        None => Scope::Wrap,
    }
}

/// Flags of one fenced-code line. `ctx` is the (scope, past `--`) state
/// carried in from a continued line; returns the state to carry out. A
/// `watcher-s1` that is not a command word (a repo name, a JSON string)
/// leaves flags checked against the union.
fn code_line_mentions(
    line: &str,
    flag: &Regex,
    carried: Option<(Scope, bool)>,
    out: &mut Vec<Mention>,
) -> Option<(Scope, bool)> {
    let starts: Vec<usize> = Regex::new(r"(?:^|\s)watcher-s1(?:\s|$)")
        .unwrap()
        .find_iter(line)
        .map(|m| m.start() + m.as_str().find('w').unwrap())
        .collect();
    let mut ctx = carried;
    let mut applied: Option<usize> = None;
    for m in flag.find_iter(line) {
        let nearest = starts.iter().rev().find(|&&i| i < m.start()).copied();
        if let Some(i) = nearest
            && nearest != applied
        {
            ctx = Some((scope_after(line, i), false));
            applied = nearest;
        }
        let Some((scope, past)) = ctx.as_mut() else {
            out.push((m.as_str().to_string(), Scope::Any));
            continue;
        };
        // Flags after a bare ` -- ` belong to the wrapped command.
        let from = applied.unwrap_or(0);
        if *past || line[from..m.start()].split_whitespace().any(|t| t == "--") {
            *past = true;
            continue;
        }
        out.push((m.as_str().to_string(), scope.clone()));
    }
    // A command with no flags still sets what a continuation line means.
    if let Some(&i) = starts.last()
        && applied != Some(i)
    {
        ctx = Some((scope_after(line, i), false));
    }
    if line.trim_end().ends_with('\\') { ctx } else { None }
}

/// `--flag` mentions that count: inside backticks anywhere (checked against
/// the union of all flags), or on fenced-code lines that run watcher-s1,
/// including `\`-continued lines (checked against the invoked subcommand).
fn mentioned(doc: &str) -> Vec<Mention> {
    let flag = Regex::new(r"--[a-z][a-z0-9-]*").unwrap();
    let span = Regex::new(r"`([^`\n]+)`").unwrap();
    let mut out = Vec::new();
    let mut in_code = false;
    let mut carry: Option<(Scope, bool)> = None;
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            carry = None;
            continue;
        }
        if in_code {
            if line.contains("watcher-s1") || carry.is_some() {
                carry = code_line_mentions(line, &flag, carry.take(), &mut out);
            }
        } else {
            for c in span.captures_iter(line) {
                out.extend(
                    flag.find_iter(c.get(1).unwrap().as_str())
                        .map(|m| (m.as_str().to_string(), Scope::Any)),
                );
            }
        }
    }
    out
}

fn mentioned_flags(doc: &str) -> BTreeSet<String> {
    mentioned(doc).into_iter().map(|(f, _)| f).collect()
}

/// Long flags of one command (not its subcommands), plus clap's built-ins.
fn own_flags(c: &clap::Command) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = c
        .get_arguments()
        .filter_map(|a| a.get_long())
        .map(|l| format!("--{l}"))
        .collect();
    out.extend(["--help", "--version"].map(String::from));
    out
}

fn scope_flags(scope: &Scope) -> BTreeSet<String> {
    match scope {
        Scope::Any => real_flags(),
        Scope::Wrap => own_flags(&Cli::command()),
        Scope::Sub(name) => own_flags(
            Cli::command()
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("no subcommand {name}")),
        ),
    }
}

fn check_flags(name: &str, doc: &str) {
    let bad: Vec<_> = mentioned(doc)
        .into_iter()
        .filter(|(f, scope)| !scope_flags(scope).contains(f) && !FOREIGN_FLAGS.contains(&f.as_str()))
        .collect();
    assert!(
        bad.is_empty(),
        "{name} mentions flags that their command does not have: {bad:?} (typo, wrong subcommand, or add to FOREIGN_FLAGS if another tool's)"
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

fn scoped(doc: &str) -> Vec<(String, String)> {
    mentioned(doc).into_iter().map(|(f, s)| (f, format!("{s:?}"))).collect()
}

#[test]
fn flags_are_scoped_per_subcommand() {
    let sub = |n: &str| format!("{:?}", Scope::Sub(n.into()));
    let wrap = format!("{:?}", Scope::Wrap);
    let doc = "```bash\n\
        watcher-s1 --events E -- job &\n\
        watcher-s1 follow --new E\n\
        watcher-s1 judge --posttooluse\n\
        ```\n";
    assert_eq!(
        scoped(doc),
        [
            ("--events", &wrap),
            ("--new", &sub("follow")),
            ("--posttooluse", &sub("judge"))
        ]
        .map(|(f, s)| (f.to_string(), s.clone()))
    );
    // Continuation lines inherit the command's scope; a new command resets it.
    let doc = "```bash\nwatcher-s1 follow \\\n  --timeout 3h E\nwatcher-s1 --silence 1s -- x\n  --not-ours\n```\n";
    assert_eq!(
        scoped(doc),
        [("--timeout", sub("follow")), ("--silence", wrap.clone())].map(|(f, s)| (f.to_string(), s))
    );
    // Wrong subcommand for a flag is caught.
    assert!(!scope_flags(&Scope::Sub("follow".into())).contains("--silence"));
    assert!(scope_flags(&Scope::Sub("follow".into())).contains("--timeout"));
    assert!(!scope_flags(&Scope::Wrap).contains("--new"));
    // A nested invocation after `--` re-scopes.
    let seen = scoped("```\nwatcher-s1 -- watcher-s1 follow --new E\n```\n");
    assert_eq!(seen, [("--new".to_string(), sub("follow"))]);
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
