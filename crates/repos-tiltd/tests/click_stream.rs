//! The click stream end to end: the real daemon, driven by a fake `tilt` placed
//! first on its PATH, asserted against its own log.
//!
//! These failures live in the seam between the daemon and the `tilt` it shells
//! out to, where a unit test can't reach them.
//!
//! Needs `/bin/sh` for the fake, as the crate's other tests need `git`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// Covers a debug-build startup plus a re-established stream (which waits a
/// second), while still failing a hang rather than sitting on it.
const DEADLINE: Duration = Duration::from_secs(30);

struct Daemon {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
    /// Where the fake appends the arguments of each call it receives.
    records: PathBuf,
    /// Dropping this deletes the workspace, so it is declared after `child`:
    /// fields drop in order, and the daemon has to die first.
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// The first line containing every one of `needles`.
    fn wait_for_line(&mut self, needles: &[&str]) -> String {
        self.read_until(&format!("a line containing {needles:?}"), |seen| {
            seen.iter().any(|l| needles.iter().all(|n| l.contains(n)))
        });
        self.line_with(needles).expect("just waited for it")
    }

    /// Waits until `needle` has appeared on `count` separate lines.
    fn wait_for_count(&mut self, needle: &str, count: usize) {
        self.read_until(&format!("{count} lines containing {needle:?}"), |seen| {
            seen.iter().filter(|l| l.contains(needle)).count() >= count
        });
    }

    /// Panics with everything read so far, so a failure shows what the daemon
    /// did instead — an early exit is as much a failure as a timeout.
    fn read_until(&mut self, wanted: &str, enough: impl Fn(&[String]) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !enough(&self.seen) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
                Err(e) => panic!(
                    "wanted {wanted} within {DEADLINE:?} but {}; the daemon logged:\n{}",
                    match e {
                        RecvTimeoutError::Timeout => "it never arrived",
                        RecvTimeoutError::Disconnected => "the daemon exited first",
                    },
                    self.seen.join("\n")
                ),
            }
        }
    }

    /// Reads the fake's record of its calls until one contains `needle`,
    /// returning the file's contents. Panics with the log on a timeout.
    fn wait_for_record(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let calls = std::fs::read_to_string(&self.records).unwrap_or_default();
            if calls.contains(needle) {
                return calls;
            }
            if Instant::now() >= deadline {
                panic!(
                    "no call containing {needle:?} within {DEADLINE:?}; the fake logged:\n{calls}\nthe daemon logged:\n{}",
                    self.seen.join("\n")
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn line_with(&self, needles: &[&str]) -> Option<String> {
        self.seen
            .iter()
            .find(|l| needles.iter().all(|n| l.contains(n)))
            .cloned()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{}", self.child.id())])
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Every fake `tilt`, written before any test spawns a process.
///
/// Writing a file and exec'ing it is racy once another thread forks: the child
/// inherits the writable fd, and the kernel refuses to exec a file while one is
/// open (ETXTBSY) — so a test writing its fake while its sibling starts a daemon
/// can find its own fake unrunnable. Writing them all before the first fork
/// leaves no such window.
static FAKES: LazyLock<Fakes> = LazyLock::new(Fakes::install);

struct Fakes {
    refusing: PathBuf,
    stream_dies: PathBuf,
    records_args: PathBuf,
    /// A script named `tilt`, since `ps` names a process after the script it
    /// runs: the daemon then sees the ancestry it would under the real thing.
    /// Not a copy of a shell — busybox, which is `/bin/sh` in the Nix sandbox,
    /// dispatches on argv[0] and refuses to run under this name.
    launcher: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fakes {
    fn install() -> Fakes {
        let dir = tempfile::TempDir::new().unwrap();
        Fakes {
            refusing: install_fake(dir.path().join("refusing"), TILT_REFUSING),
            stream_dies: install_fake(dir.path().join("stream-dies"), TILT_STREAM_DIES),
            records_args: install_fake(dir.path().join("records-args"), TILT_RECORDS_ARGS),
            launcher: install_fake(dir.path().join("launcher"), TILT_LAUNCHES_DAEMON).join("tilt"),
            _dir: dir,
        }
    }
}

/// Writes `script` as an executable `tilt` in a fresh `bin`, returning `bin` to
/// put on a PATH. Runs it once: a fake that won't exec — no interpreter, say —
/// reports as ENOENT on the spawn, indistinguishable in a daemon's log from a
/// `tilt` that isn't installed.
fn install_fake(bin: PathBuf, script: &str) -> PathBuf {
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("tilt");
    write_executable(&fake, script);
    if let Err(e) = Command::new(&fake).output() {
        panic!("can't run the fake tilt at {}: {e}", fake.display());
    }
    bin
}

/// Starts the daemon with the fake in `bin` as the `tilt` it finds on PATH.
fn start_daemon(bin: &Path) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_repos-tiltd"));
    cmd.arg("--no-self-update");
    spawn_daemon(cmd, bin)
}

/// Starts the daemon as the child of a process that looks like `tilt up --port
/// <port>`, with `TILT_PORT` set to `stale` — the shape that broke in the field,
/// where the daemon has to believe the flag over the environment.
fn start_daemon_under_tilt(bin: &Path, port: &str, stale: &str) -> Daemon {
    let mut cmd = Command::new(&FAKES.launcher);
    cmd.args(["up", "--port", port])
        .env("REPOS_TILTD", env!("CARGO_BIN_EXE_repos-tiltd"))
        .env("TILT_PORT", stale);
    spawn_daemon(cmd, bin)
}

/// Runs `cmd` — the daemon, or something that starts it — with the environment
/// the daemon needs.
///
/// Registry and XDG state live in one temp dir, and the resource spec is empty —
/// no repo, git call or network, so only the click stream is under test.
fn spawn_daemon(mut cmd: Command, bin: &Path) -> Daemon {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("tilt-devenv.json"), r#"{"repos":[]}"#).unwrap();
    let records = dir.path().join("tilt-calls");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = cmd
        // Its own group, so shutdown reaches a daemon started underneath a
        // launcher as surely as one started directly.
        .process_group(0)
        .env("PATH", path)
        .env("REPOS_ROOT", dir.path())
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("REPOS_TILT_SPEC", "[]")
        .env("FAKE_TILT_STATE", &records)
        .env("RUST_LOG", "info")
        // A `tilt` the daemon runs without piping inherits this; a fake reading
        // stdin would otherwise block on the test runner's terminal.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the daemon");

    let stderr = child.stderr.take().expect("stderr was piped");
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(strip_ansi(&line)).is_err() {
                return;
            }
        }
    });

    Daemon {
        child,
        lines,
        seen: Vec::new(),
        records,
        _dir: dir,
    }
}

/// Drops ANSI colour, which tracing writes between a field's name and its value
/// — so an assertion can look for `port=10399` and not an escape sequence.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn write_executable(path: &Path, contents: &str) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    f.set_permissions(std::fs::Permissions::from_mode(0o755))
        .unwrap();
}

/// A `tilt` that only starts the daemon; its arguments are there to be read back
/// off `ps`, as a real Tilt's are.
const TILT_LAUNCHES_DAEMON: &str = r#"#!/bin/sh
"$REPOS_TILTD" --no-self-update
"#;

/// A `tilt` that appends the arguments of every call it gets, so a test can see
/// which apiserver the daemon aimed at.
const TILT_RECORDS_ARGS: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_TILT_STATE"
case "$*" in
  *--watch-only) : ;;
  *"get uibutton -o json") echo '{"items":[]}' ;;
  *"apply -f -") cat >/dev/null ;;
esac
exit 0
"#;

/// A `tilt` whose apiserver is gone, the way a stale `TILT_PORT` leaves it:
/// every query refused.
const TILT_REFUSING: &str = r#"#!/bin/sh
case "$*" in
  *"apply -f -") cat >/dev/null ;;
esac
echo 'The connection to the server 127.0.0.1:44073 was refused - did you specify the right host or port?' >&2
exit 1
"#;

/// A `tilt` whose click stream delivers one press and then exits, every time —
/// the daemon has to notice and re-establish it to see the second press.
const TILT_STREAM_DIES: &str = r#"#!/bin/sh
case "$*" in
  *"get uibutton -o json")
    echo '{"items":[{"kind":"UIButton","metadata":{"name":"repos-profile"},"status":{"lastClickedAt":null}}]}'
    ;;
  *--watch-only)
    n=$(( $(cat "$FAKE_TILT_STATE" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$FAKE_TILT_STATE"
    if [ "$n" -le 2 ]; then
      printf '{"kind":"UIButton","metadata":{"name":"repos-profile"},"status":{"lastClickedAt":"2026-08-17T00:00:%02dZ"}}\n' "$n"
    fi
    ;;
  *"apply -f -") cat >/dev/null ;;
esac
exit 0
"#;

#[test]
fn should_report_why_it_cannot_watch_for_clicks() {
    let mut daemon = start_daemon(&FAKES.refusing);

    let line = daemon.wait_for_line(&["buttons won't respond"]);

    assert!(
        line.contains("was refused"),
        "the warning has to carry the cause, or it sends you hunting for a \
         rendering bug instead of a dead apiserver: {line}"
    );
}

#[test]
fn should_aim_tilt_at_the_port_its_own_tilt_was_given() {
    let mut daemon = start_daemon_under_tilt(&FAKES.records_args, "10399", "19999");

    daemon.wait_for_line(&["talking to Tilt's apiserver", "port=10399"]);

    let calls = daemon.wait_for_record("--port 10399");
    assert!(
        !calls.contains("19999"),
        "the stale environment port must reach no call: {calls}"
    );
}

#[test]
fn should_keep_answering_clicks_after_the_stream_dies() {
    let mut daemon = start_daemon(&FAKES.stream_dies);

    daemon.wait_for_line(&["the click stream ended"]);
    daemon.wait_for_line(&["re-establishing the click stream"]);

    // One line per click handled: the second can only arrive on a stream the
    // daemon re-established itself.
    daemon.wait_for_count("profile selection cleared", 2);
}
