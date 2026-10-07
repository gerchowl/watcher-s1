use clap::Parser;
use std::sync::Arc;
use watcher_s1::breaker::{Breaker, default_state_dir};
use watcher_s1::cli::{Cli, OnPrompt, S1Args, Sub};
use watcher_s1::config::{self, S1Config};
use watcher_s1::event::Sink;
use watcher_s1::follow;
use watcher_s1::judge;
use watcher_s1::questions::QuestionSet;
use watcher_s1::s1::Client;
use watcher_s1::supervise::{self, Options, log};

/// Resolve the System One client; every problem degrades to "tier off".
fn client(args: &S1Args, quiet: bool) -> Option<Client> {
    let cfg = match config::resolve_from_process(&args.to_cli()) {
        Ok(c) => c,
        Err(e) => {
            log(quiet, &format!("System One config error, tier off: {e}"));
            return None;
        }
    };
    if !cfg.enabled() {
        log(
            quiet,
            "no System One endpoint configured (--s1-url, SYSTEMONE_URL, config.toml): System One tier off",
        );
        return None;
    }
    let questions = match QuestionSet::load(&cfg.questions) {
        Ok(q) => q,
        Err(e) => {
            log(quiet, &format!("System One questions error, tier off: {e}"));
            return None;
        }
    };
    let breaker = Breaker::new(default_state_dir(), cfg.breaker.clone());
    Some(Client::new(cfg, questions, breaker))
}

fn print_config(args: &S1Args) -> i32 {
    let cfg: S1Config = match config::resolve_from_process(&args.to_cli()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("watcher-s1: {e}");
            return 1;
        }
    };
    let questions = QuestionSet::load(&cfg.questions);
    let out = serde_json::json!({
        "enabled": cfg.enabled(),
        "urls": cfg.urls,
        "urls_source": cfg.urls_source.to_string(),
        "timeout_s": cfg.timeout.as_secs_f64(),
        "timeout_source": cfg.timeout_source.to_string(),
        "breaker": {"fails": cfg.breaker.fails, "cooldown_s": cfg.breaker.cooldown.as_secs_f64()},
        "questions": cfg.questions,
        "questions_ok": questions.as_ref().map(|_| true).unwrap_or(false),
        "questions_error": questions.err(),
        "state_dir": default_state_dir(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    0
}

fn main() {
    let cli = Cli::parse();
    match cli.sub {
        Some(Sub::Judge(j)) => {
            // Silent by design: stderr from a hook only reaches a debug log.
            judge::run_posttooluse(client(&j.s1, true))
        }
        Some(Sub::Config(a)) => std::process::exit(print_config(&a)),
        Some(Sub::Guide) => {
            print!("{}", watcher_s1::GUIDE);
            std::process::exit(0)
        }
        Some(Sub::Follow(f)) => match follow::run(&f.file, f.new, f.timeout, &mut std::io::stdout()) {
            Ok(follow::Outcome::Done) => std::process::exit(0),
            Ok(follow::Outcome::TimedOut) => {
                eprintln!(
                    "watcher-s1: follow {}: timed out before the run's final event",
                    f.file.display()
                );
                std::process::exit(3)
            }
            Err(e) => {
                eprintln!("watcher-s1: follow {}: {e}", f.file.display());
                // A path follow cannot read at all is a usage error.
                std::process::exit(if e.kind() == std::io::ErrorKind::InvalidInput {
                    2
                } else {
                    1
                })
            }
        },
        None => {}
    }
    let w = cli.wrap;
    let sink = match (&w.events, w.events_fd) {
        (Some(p), _) => Sink::file(&p.to_string_lossy()),
        (None, Some(fd)) => Sink::fd(fd),
        (None, None) => Ok(Sink::Stderr),
    };
    let sink = sink.unwrap_or_else(|e| {
        log(
            w.quiet,
            &format!("cannot open the event sink ({e}); events go to stderr"),
        );
        Sink::Stderr
    });
    let opts = Options {
        s1: client(&w.s1, w.quiet).map(Arc::new),
        argv: w.cmd,
        pty: !w.pipe,
        silence: (!w.silence.is_zero()).then_some(w.silence),
        timeout: w.timeout.filter(|t| !t.is_zero()),
        kill_grace: w.kill_grace,
        prompt_after: w.prompt_after,
        sample_every: w.sample_every,
        blocked_after: w.blocked_after,
        probe_timeout: w.probe_timeout,
        prompt_cancel: (w.on_prompt == OnPrompt::Cancel).then_some(w.prompt_cancel_after),
        heartbeat: w.heartbeat,
        heartbeat_s1: w.heartbeat_s1,
        evidence_bytes: w.evidence_bytes,
        sink: Arc::new(sink),
        quiet: w.quiet,
    };
    match w.log {
        Some(path) => supervise::exit_like(supervise::run_log(opts, &path)),
        None => supervise::exit_like(supervise::run(opts)),
    }
}
