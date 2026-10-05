//! System One endpoint configuration and its precedence.
//!
//! No endpoint is compiled in. Each field resolves independently, highest
//! first:
//!
//! 1. CLI: `--s1-url` (repeatable), `--s1-timeout`, `--config FILE`
//! 2. env `SYSTEMONE_URL` (URLs only; comma or space separated)
//! 3. the first existing file of `$XDG_CONFIG_HOME/watcher-s1/config.toml`
//!    (default `~/.config/...`) and `/etc/watcher-s1/config.toml`
//! 4. none: the System One tier is off and everything else still works.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ENV_URL: &str = "SYSTEMONE_URL";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
pub const DEFAULT_BREAKER_FAILS: u32 = 3;
pub const DEFAULT_BREAKER_COOLDOWN: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq)]
pub struct BreakerConfig {
    pub fails: u32,
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            fails: DEFAULT_BREAKER_FAILS,
            cooldown: DEFAULT_BREAKER_COOLDOWN,
        }
    }
}

/// Where a resolved value came from, for `watcher-s1 config`.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Cli,
    Env,
    File(PathBuf),
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Cli => write!(f, "cli"),
            Source::Env => write!(f, "env {ENV_URL}"),
            Source::File(p) => write!(f, "file {}", p.display()),
            Source::Default => write!(f, "default"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct S1Config {
    /// Ordered; the first healthy endpoint wins. Empty = tier off.
    pub urls: Vec<String>,
    pub urls_source: Source,
    pub timeout: Duration,
    pub timeout_source: Source,
    pub breaker: BreakerConfig,
    /// `builtin` or a path (already resolved against the config file's dir).
    pub questions: String,
}

impl S1Config {
    pub fn enabled(&self) -> bool {
        !self.urls.is_empty()
    }
}

/// CLI-level overrides.
#[derive(Debug, Clone, Default)]
pub struct CliS1 {
    pub urls: Vec<String>,
    pub timeout_s: Option<f64>,
    pub config: Option<PathBuf>,
    pub disabled: bool,
}

#[derive(Debug, Default, Deserialize)]
struct FileRoot {
    #[serde(default)]
    systemone: Option<FileS1>,
}

#[derive(Debug, Default, Deserialize)]
struct FileS1 {
    #[serde(default)]
    urls: Option<Vec<String>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    timeout_s: Option<f64>,
    #[serde(default)]
    breaker: Option<FileBreaker>,
    #[serde(default)]
    questions: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileBreaker {
    fails: Option<u32>,
    cooldown_s: Option<f64>,
}

/// The config files searched, in order, given an env lookup.
pub fn search_paths(env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let xdg = env("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| Path::new(&h).join(".config"))
        });
    if let Some(x) = xdg {
        out.push(x.join("watcher-s1/config.toml"));
    }
    out.push(PathBuf::from("/etc/watcher-s1/config.toml"));
    out
}

fn split_urls(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
        .collect()
}

fn secs(v: f64, what: &str) -> Result<Duration, String> {
    if v.is_finite() && v > 0.0 {
        Ok(Duration::from_secs_f64(v))
    } else {
        Err(format!("{what} must be a positive number of seconds, got {v}"))
    }
}

/// Resolve the configuration. `env` is injected so tests need no process env.
/// An unreadable or invalid file is an error (the caller logs it and runs
/// with the tier off); a missing file is simply skipped.
pub fn resolve(cli: &CliS1, env: &dyn Fn(&str) -> Option<String>, search: &[PathBuf]) -> Result<S1Config, String> {
    let mut cfg = S1Config {
        urls: Vec::new(),
        urls_source: Source::Default,
        timeout: DEFAULT_TIMEOUT,
        timeout_source: Source::Default,
        breaker: BreakerConfig::default(),
        questions: "builtin".into(),
    };
    if cli.disabled {
        return Ok(cfg);
    }

    let file = match &cli.config {
        Some(p) => Some(read_file(p)?.ok_or_else(|| format!("config {}: not found", p.display()))?),
        None => {
            let mut found = None;
            for p in search {
                if let Some(f) = read_file(p)? {
                    found = Some(f);
                    break;
                }
            }
            found
        }
    };

    if let Some((path, f)) = &file {
        let src = Source::File(path.clone());
        let mut urls = f.urls.clone().unwrap_or_default();
        urls.extend(f.url.clone());
        if !urls.is_empty() {
            cfg.urls = urls;
            cfg.urls_source = src.clone();
        }
        if let Some(t) = f.timeout_s {
            cfg.timeout = secs(t, "timeout_s")?;
            cfg.timeout_source = src;
        }
        if let Some(b) = &f.breaker {
            if let Some(n) = b.fails {
                if n == 0 {
                    return Err("breaker.fails must be >= 1".into());
                }
                cfg.breaker.fails = n;
            }
            if let Some(c) = b.cooldown_s {
                cfg.breaker.cooldown = secs(c, "breaker.cooldown_s")?;
            }
        }
        if let Some(q) = &f.questions {
            cfg.questions = if q == "builtin" || Path::new(q).is_absolute() {
                q.clone()
            } else {
                let dir = path.parent().unwrap_or(Path::new("."));
                dir.join(q).to_string_lossy().into_owned()
            };
        }
    }

    if let Some(v) = env(ENV_URL) {
        let urls = split_urls(&v);
        if !urls.is_empty() {
            cfg.urls = urls;
            cfg.urls_source = Source::Env;
        }
    }

    if !cli.urls.is_empty() {
        cfg.urls = cli.urls.iter().flat_map(|u| split_urls(u)).collect();
        cfg.urls_source = Source::Cli;
    }
    if let Some(t) = cli.timeout_s {
        cfg.timeout = secs(t, "--s1-timeout")?;
        cfg.timeout_source = Source::Cli;
    }
    Ok(cfg)
}

type FileFound = Option<(PathBuf, FileS1)>;

fn read_file(p: &Path) -> Result<FileFound, String> {
    match std::fs::read_to_string(p) {
        Ok(text) => {
            let root: FileRoot = toml::from_str(&text).map_err(|e| format!("config {}: {e}", p.display()))?;
            Ok(Some((p.to_path_buf(), root.systemone.unwrap_or_default())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("config {}: {e}", p.display())),
    }
}

/// Resolve against the real process environment and the standard paths.
pub fn resolve_from_process(cli: &CliS1) -> Result<S1Config, String> {
    let env = |k: &str| std::env::var(k).ok();
    resolve(cli, &env, &search_paths(&env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }

    const XDG_FILE: &str = r#"
        [systemone]
        urls = ["http://xdg-a:1/v1/systemone", "http://xdg-b:1/v1/systemone"]
        timeout_s = 2.5
        breaker = { fails = 5, cooldown_s = 60 }
        questions = "q.toml"
    "#;

    #[test]
    fn nothing_configured_means_tier_off() {
        let t = tempfile::tempdir().unwrap();
        let env = env_of(&[("XDG_CONFIG_HOME", t.path().to_str().unwrap())]);
        let cfg = resolve(&CliS1::default(), &env, &[t.path().join("none.toml")]).unwrap();
        assert!(!cfg.enabled());
        assert_eq!(cfg.timeout, DEFAULT_TIMEOUT);
        assert_eq!(cfg.breaker, BreakerConfig::default());
        assert_eq!(cfg.questions, "builtin");
    }

    #[test]
    fn xdg_file_beats_etc_file() {
        let t = tempfile::tempdir().unwrap();
        let xdg = write(t.path(), "xdg/watcher-s1/config.toml", XDG_FILE);
        let etc = write(t.path(), "etc/config.toml", "[systemone]\nurls=[\"http://etc:1/x\"]\n");
        let cfg = resolve(&CliS1::default(), &env_of(&[]), &[xdg.clone(), etc]).unwrap();
        assert_eq!(cfg.urls, ["http://xdg-a:1/v1/systemone", "http://xdg-b:1/v1/systemone"]);
        assert_eq!(cfg.urls_source, Source::File(xdg.clone()));
        assert_eq!(cfg.timeout, Duration::from_secs_f64(2.5));
        assert_eq!(
            cfg.breaker,
            BreakerConfig {
                fails: 5,
                cooldown: Duration::from_secs(60)
            }
        );
        // A relative questions path resolves against the config file's dir.
        assert_eq!(PathBuf::from(&cfg.questions), xdg.parent().unwrap().join("q.toml"));
    }

    #[test]
    fn etc_file_used_when_xdg_missing() {
        let t = tempfile::tempdir().unwrap();
        let etc = write(t.path(), "etc/config.toml", "[systemone]\nurl=\"http://etc:1/x\"\n");
        let cfg = resolve(&CliS1::default(), &env_of(&[]), &[t.path().join("missing.toml"), etc]).unwrap();
        assert_eq!(cfg.urls, ["http://etc:1/x"]);
    }

    #[test]
    fn env_beats_file_for_urls_only() {
        let t = tempfile::tempdir().unwrap();
        let xdg = write(t.path(), "c.toml", XDG_FILE);
        let env = env_of(&[(ENV_URL, "http://env-a:1/x, http://env-b:1/x")]);
        let cfg = resolve(&CliS1::default(), &env, std::slice::from_ref(&xdg)).unwrap();
        assert_eq!(cfg.urls, ["http://env-a:1/x", "http://env-b:1/x"]);
        assert_eq!(cfg.urls_source, Source::Env);
        // Timeout and breaker still come from the file.
        assert_eq!(cfg.timeout_source, Source::File(xdg));
        assert_eq!(cfg.breaker.fails, 5);
    }

    #[test]
    fn empty_env_is_unset() {
        let t = tempfile::tempdir().unwrap();
        let xdg = write(t.path(), "c.toml", XDG_FILE);
        let cfg = resolve(&CliS1::default(), &env_of(&[(ENV_URL, " ")]), &[xdg]).unwrap();
        assert_eq!(cfg.urls[0], "http://xdg-a:1/v1/systemone");
    }

    #[test]
    fn cli_beats_env_and_file() {
        let t = tempfile::tempdir().unwrap();
        let xdg = write(t.path(), "c.toml", XDG_FILE);
        let env = env_of(&[(ENV_URL, "http://env:1/x")]);
        let cli = CliS1 {
            urls: vec!["http://cli:1/x".into()],
            timeout_s: Some(1.0),
            ..Default::default()
        };
        let cfg = resolve(&cli, &env, &[xdg]).unwrap();
        assert_eq!(cfg.urls, ["http://cli:1/x"]);
        assert_eq!(cfg.urls_source, Source::Cli);
        assert_eq!(cfg.timeout, Duration::from_secs(1));
        assert_eq!(cfg.timeout_source, Source::Cli);
    }

    #[test]
    fn explicit_config_replaces_the_search() {
        let t = tempfile::tempdir().unwrap();
        let xdg = write(t.path(), "c.toml", XDG_FILE);
        let mine = write(t.path(), "mine.toml", "[systemone]\nurls=[\"http://mine:1/x\"]\n");
        let cli = CliS1 {
            config: Some(mine),
            ..Default::default()
        };
        let cfg = resolve(&cli, &env_of(&[]), &[xdg]).unwrap();
        assert_eq!(cfg.urls, ["http://mine:1/x"]);
        let cli = CliS1 {
            config: Some(t.path().join("nope.toml")),
            ..Default::default()
        };
        assert!(resolve(&cli, &env_of(&[]), &[]).unwrap_err().contains("not found"));
    }

    #[test]
    fn disabled_wins_over_everything() {
        let env = env_of(&[(ENV_URL, "http://env:1/x")]);
        let cli = CliS1 {
            urls: vec!["http://cli:1/x".into()],
            disabled: true,
            ..Default::default()
        };
        assert!(!resolve(&cli, &env, &[]).unwrap().enabled());
    }

    #[test]
    fn invalid_values_are_errors() {
        let t = tempfile::tempdir().unwrap();
        let bad = write(t.path(), "bad.toml", "[systemone]\ntimeout_s = -1\n");
        assert!(resolve(&CliS1::default(), &env_of(&[]), &[bad]).is_err());
        let garbage = write(t.path(), "garbage.toml", "this is = = not toml");
        assert!(resolve(&CliS1::default(), &env_of(&[]), &[garbage]).is_err());
    }

    #[test]
    fn search_paths_follow_xdg() {
        let p = search_paths(&env_of(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")]));
        assert_eq!(
            p,
            [
                PathBuf::from("/x/watcher-s1/config.toml"),
                "/etc/watcher-s1/config.toml".into()
            ]
        );
        let p = search_paths(&env_of(&[("HOME", "/h")]));
        assert_eq!(p[0], PathBuf::from("/h/.config/watcher-s1/config.toml"));
    }
}
