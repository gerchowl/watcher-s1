//! The watcher on a real terminal: stdin, stdout and stderr are a PTY that
//! is its controlling terminal, as when a person runs it. This covers the
//! interactive paths (raw-mode stdin forwarding, output translation, the
//! --pipe foreground handoff) that no pipe-based test can.

mod common;
use common::*;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Term {
    master: std::fs::File,
    /// Held so the terminal outlives the watcher on Linux.
    _slave: OwnedFd,
    screen: Arc<Mutex<Vec<u8>>>,
    child: Child,
}

impl Term {
    /// Spawn `watcher-s1 ARGS` as a session leader on a fresh PTY.
    fn spawn(e: &Env, args: &[&str]) -> Term {
        let mut c = Command::new(BIN);
        c.arg("--events").arg(e.path("events.jsonl")).args(args);
        Self::spawn_cmd(e, c)
    }

    /// Spawn any command as a session leader on a fresh PTY.
    fn spawn_cmd(e: &Env, mut c: Command) -> Term {
        let pty = nix::pty::openpty(None::<&nix::pty::Winsize>, None::<&nix::sys::termios::Termios>).unwrap();
        c.env_remove("SYSTEMONE_URL")
            .env_remove("WATCHER_S1_PARENT")
            .env("XDG_CONFIG_HOME", e.path("xdg"))
            .env("WATCHER_S1_STATE_DIR", e.path("state"))
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave.try_clone().unwrap()));
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = c.spawn().unwrap();
        let master = std::fs::File::from(pty.master);
        let screen = Arc::new(Mutex::new(Vec::new()));
        let (mut r, s) = (master.try_clone().unwrap(), screen.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                s.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Term {
            master,
            _slave: pty.slave,
            screen,
            child,
        }
    }

    fn type_keys(&mut self, s: &str) {
        self.master.write_all(s.as_bytes()).unwrap();
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, needle: &str, limit: Duration) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            if self.screen().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Wait for exit; kill and fail the test if it takes longer than `limit`.
    fn wait(&mut self, limit: Duration) -> ExitStatus {
        let t0 = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(100)); // let the reader drain
                return st;
            }
            if t0.elapsed() > limit {
                let _ = self.child.kill();
                panic!("watcher hung on a terminal; screen so far: {:?}", self.screen());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn interactive_run_finishes_without_any_keystroke() {
    let e = Env::new();
    let mut t = Term::spawn(
        &e,
        &["--no-s1", "-q", "--", "sh", "-c", "sleep .3; printf done; exit 4"],
    );
    let st = t.wait(Duration::from_secs(5));
    assert_eq!(st.code(), Some(4));
    assert!(t.screen().contains("done"));
    assert_eq!(last(&e.events())["exit"]["code"], 4);
}

#[test]
fn interactive_output_gets_crlf_on_the_terminal() {
    let e = Env::new();
    let mut t = Term::spawn(&e, &["--no-s1", "-q", "--", "printf", "x\\ny\\n"]);
    t.wait(Duration::from_secs(5));
    let s = t.screen();
    assert!(s.contains("x\r\ny\r\n"), "staircased output: {s:?}");
}

#[test]
fn keystrokes_reach_the_child() {
    let e = Env::new();
    let mut t = Term::spawn(&e, &["--no-s1", "-q", "--", "sh", "-c", "read l; echo got:$l"]);
    std::thread::sleep(Duration::from_millis(300));
    t.type_keys("hello\r");
    let st = t.wait(Duration::from_secs(5));
    assert!(st.success(), "{st:?}");
    assert!(t.screen().contains("got:hello"), "{:?}", t.screen());
}

#[test]
fn sigterm_works_while_waiting_for_input() {
    let e = Env::new();
    let mut t = Term::spawn(&e, &["--no-s1", "-q", "--", "sleep", "30"]);
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(t.child.id() as i32, libc::SIGTERM) };
    let st = t.wait(Duration::from_secs(5));
    assert_eq!(st.signal(), Some(libc::SIGTERM));
}

#[test]
fn terminal_modes_are_restored() {
    // Compare the modes from inside the session: on darwin the terminal is
    // revoked once its session leader exits, so the test cannot ask later.
    // `pendin` is ignored: XNU sets it whenever a tty returns to canonical
    // mode (after any raw-mode program) and clears it on the next read.
    let e = Env::new();
    let modes = "stty -a | tr ' ;' '\\n\\n' | grep -v pendin | grep . | sort | tr '\\n' ' '";
    let script = format!(
        "a=$({modes}); {BIN} --no-s1 -q --events {ev} -- sh -c 'sleep .3'; b=$({modes}); \
         if [ \"$a\" = \"$b\" ]; then echo MODES-SAME; else echo \"MODES-DIFF before=[$a] after=[$b]\"; fi",
        ev = e.path("events.jsonl").display()
    );
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    let mut t = Term::spawn_cmd(&e, c);
    t.wait(Duration::from_secs(5));
    assert!(t.wait_for("MODES-SAME", Duration::from_secs(1)), "{:?}", t.screen());
}

/// A shell (session leader on the PTY) runs the watcher with --pipe, then
/// reports the terminal's foreground group and its own group.
fn pipe_mode_session(e: &Env, inner: &str) -> Term {
    let script = format!(
        "{BIN} --no-s1 -q --pipe --events {ev} -- {inner}; echo rc=$?; echo fg=$(ps -o tpgid= -p $$ | tr -d ' ') me=$(ps -o pgid= -p $$ | tr -d ' ')",
        ev = e.path("events.jsonl").display()
    );
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    Term::spawn_cmd(e, c)
}

fn foreground_is_ours(t: &Term) -> bool {
    let s = t.screen();
    let grab = |k: &str| {
        s.split(k)
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .map(str::to_owned)
    };
    match (grab("fg="), grab("me=")) {
        (Some(fg), Some(me)) => fg == me,
        _ => panic!("no fg/me report: {s:?}"),
    }
}

#[test]
fn pipe_mode_hands_the_terminal_to_the_child_and_back() {
    let e = Env::new();
    let mut t = pipe_mode_session(&e, "sh -c 'read l; echo got:$l'");
    std::thread::sleep(Duration::from_millis(400));
    t.type_keys("hi\n");
    let st = t.wait(Duration::from_secs(5));
    assert!(st.success(), "{st:?} {:?}", t.screen());
    assert!(t.wait_for("got:hi", Duration::from_secs(1)), "{:?}", t.screen());
    assert!(t.screen().contains("rc=0"));
    assert!(foreground_is_ours(&t), "terminal not handed back: {:?}", t.screen());
}

#[test]
fn pipe_mode_reclaims_the_terminal_when_exec_fails() {
    let e = Env::new();
    let mut t = pipe_mode_session(&e, "/nonexistent/x");
    t.wait(Duration::from_secs(5));
    assert!(t.screen().contains("rc=127"), "{:?}", t.screen());
    assert!(foreground_is_ours(&t), "terminal stranded: {:?}", t.screen());
}
