//! Shared black-box test support: drives the real compiled `harness`
//! binary as a subprocess against an isolated `RUSTY_PRIME_AGENT_HOME`,
//! the same way a real user/TUI would. This is deliberately a black-box
//! test, not a call into internal modules: even though the package now
//! has a `[lib]` target (`rusty_prime_agent`, see `ARCHITECTURE.md`'s
//! "Embeddable SDK" section, and `tests/embedded_session.rs`/`tests/
//! dispatch_one_shot.rs` for the tests that actually exercise it),
//! testing *this* project's own daemon/worker/socket architecture
//! through the actual CLI + real OS processes + real Unix sockets is
//! what the brief's "session recovery" and "worker crash handling"
//! coverage actually needs to be evidence of -- a lib-level call would
//! bypass the exact machinery these tests exist to prove.
//!
//! `mod common;` is included separately by each test binary
//! (`session_lifecycle.rs`/`supervisor_restart_recovery.rs`/
//! `worker_crash_recovery.rs`), and no single one of them calls every
//! helper here -- `cargo clippy --all-targets` sees each test binary as
//! its own crate, so whichever helpers that particular binary doesn't
//! reach get flagged `dead_code` individually. Allowed at the module
//! level rather than per-function: this file's whole purpose is being a
//! shared toolbox, not every tool in it being used by every caller.
#![allow(dead_code)]

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A directory this process owns exclusively, removed best-effort on
/// drop. Hand-rolled instead of pulling in `tempfile`: this project's
/// dependency floor is deliberately narrow, and "unique subdirectory
/// under the OS temp dir, removed on drop" is a dozen lines.
pub struct TempDir(PathBuf);

impl TempDir {
    /// `label` is accepted for readability at call sites but
    /// deliberately **not** included in the actual directory name:
    /// Windows AF_UNIX's `sun_path` is a hard 107-usable-byte budget for
    /// the *entire* socket path (see `paths::worker_socket_path`'s doc
    /// comment), and `std::env::temp_dir()` alone
    /// (`C:\Users\<user>\AppData\Local\Temp` for a real profile) can
    /// already spend 40-plus of those bytes -- a verbose
    /// `rpa-test-<label>-<pid>-<nanos>` component blew through the
    /// remaining budget the moment the flat `sock/<hash>.sock` suffix
    /// was appended (caught by this project's own integration tests
    /// before this fix). A short hash of the same uniqueness inputs
    /// (label, pid, time) keeps every test's state root tiny instead.
    pub fn new(label: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (label, std::process::id(), nanos).hash(&mut hasher);
        let dir = std::env::temp_dir().join(format!("rpa{:x}", hasher.finish() & 0xffff_ffff));
        std::fs::create_dir_all(&dir).expect("create temp state dir");
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harness"))
}

/// Runs one CLI invocation to completion and returns its output.
/// `daemon start`/`session new`/`session prompt`/`session list`/
/// `daemon status`/`daemon shutdown` are all one-shot -- only `session
/// attach` streams, and that goes through [`attach_lines`] instead.
pub fn run(state_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("RUSTY_PRIME_AGENT_HOME", state_dir)
        .output()
        .expect("failed to run harness")
}

pub fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub fn assert_success(label: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{label} failed (status {:?}):\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

pub fn daemon_start(state_dir: &Path) {
    let out = run(state_dir, &["daemon", "start"]);
    if !out.status.success() {
        // `daemon.log` is the supervisor's own stderr (`client::
        // daemon_start`'s redirect, not this CLI process's) -- read it
        // back so a failed startup doesn't just say "timed out" with no
        // clue why the supervisor never got far enough to answer.
        let log = std::fs::read_to_string(state_dir.join("daemon.log"))
            .unwrap_or_else(|e| format!("<could not read daemon.log: {e}>"));
        panic!(
            "daemon start failed (status {:?}):\nstdout: {}\nstderr: {}\ndaemon.log:\n{log}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

pub fn daemon_status(state_dir: &Path) -> String {
    let out = run(state_dir, &["daemon", "status"]);
    assert_success("daemon status", &out);
    stdout_string(&out)
}

pub fn daemon_pid(state_dir: &Path) -> u32 {
    let status = daemon_status(state_dir);
    parse_field(&status, "pid=").expect("daemon status has pid=")
}

pub fn daemon_shutdown(state_dir: &Path) {
    let out = run(state_dir, &["daemon", "shutdown"]);
    assert_success("daemon shutdown", &out);
}

pub fn session_new(state_dir: &Path, name: Option<&str>) -> String {
    session_new_with_model(state_dir, name, None)
}

/// Parity with `prime-agent --model provider/id`. `model` is a
/// `"provider/model"` string routed through the `rp-server` sidecar
/// (see `rp_server`'s own doc comment) -- `None` keeps `EchoProvider`.
pub fn session_new_with_model(state_dir: &Path, name: Option<&str>, model: Option<&str>) -> String {
    let mut args = vec!["session", "new"];
    if let Some(n) = name {
        args.push("--name");
        args.push(n);
    }
    if let Some(m) = model {
        args.push("--model");
        args.push(m);
    }
    let out = run(state_dir, &args);
    assert_success("session new", &out);
    stdout_string(&out)
}

/// Same as [`session_new_with_model`] plus `--thinking low|medium|high`.
/// Kept as its own function rather than widening `session_new_with_model`
/// itself, since only the real-model `#[ignore]`d tests need it.
pub fn session_new_with_model_and_thinking(
    state_dir: &Path,
    name: Option<&str>,
    model: Option<&str>,
    thinking: Option<&str>,
) -> String {
    let mut args = vec!["session", "new"];
    if let Some(n) = name {
        args.push("--name");
        args.push(n);
    }
    if let Some(m) = model {
        args.push("--model");
        args.push(m);
    }
    if let Some(t) = thinking {
        args.push("--thinking");
        args.push(t);
    }
    let out = run(state_dir, &args);
    assert_success("session new", &out);
    stdout_string(&out)
}

/// Same as [`session_new_with_model`] plus `--tools read`. Kept as its
/// own function for the same reason `session_new_with_model_and_thinking`
/// is.
pub fn session_new_with_model_and_tools(
    state_dir: &Path,
    name: Option<&str>,
    model: Option<&str>,
    tools: Option<&str>,
) -> String {
    let mut args = vec!["session", "new"];
    if let Some(n) = name {
        args.push("--name");
        args.push(n);
    }
    if let Some(m) = model {
        args.push("--model");
        args.push(m);
    }
    if let Some(t) = tools {
        args.push("--tools");
        args.push(t);
    }
    let out = run(state_dir, &args);
    assert_success("session new", &out);
    stdout_string(&out)
}

/// Same as [`session_new_with_model`] plus `--runtime ipython`. Kept as
/// its own function for the same reason `session_new_with_model_and_tools`
/// is.
pub fn session_new_with_runtime(
    state_dir: &Path,
    name: Option<&str>,
    model: Option<&str>,
    runtime: Option<&str>,
) -> String {
    let mut args = vec!["session", "new"];
    if let Some(n) = name {
        args.push("--name");
        args.push(n);
    }
    if let Some(m) = model {
        args.push("--model");
        args.push(m);
    }
    if let Some(r) = runtime {
        args.push("--runtime");
        args.push(r);
    }
    let out = run(state_dir, &args);
    assert_success("session new", &out);
    stdout_string(&out)
}

pub fn session_prompt(state_dir: &Path, session_id: &str, text: &str) -> String {
    let out = run(state_dir, &["session", "prompt", session_id, text]);
    assert_success("session prompt", &out);
    stdout_string(&out)
}

pub fn session_list(state_dir: &Path) -> String {
    let out = run(state_dir, &["session", "list"]);
    assert_success("session list", &out);
    stdout_string(&out)
}

fn parse_field(text: &str, key: &str) -> Option<u32> {
    let idx = text.find(key)?;
    let rest = &text[idx + key.len()..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Reads the recorded worker pid straight from `state.json` -- the same
/// pointer file `crate::catalog`/`crate::daemon` read, used here so a
/// test can target a worker for a simulated crash without needing an
/// internal API.
pub fn worker_pid(state_dir: &Path, session_id: &str) -> u32 {
    let path = state_dir
        .join("sessions")
        .join(session_id)
        .join("state.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let value: serde_json::Value = serde_json::from_str(&text).expect("state.json is valid JSON");
    value["worker_pid"]
        .as_u64()
        .expect("state.json has worker_pid") as u32
}

pub fn session_stop(state_dir: &Path, session_id: &str) -> String {
    let out = run(state_dir, &["session", "stop", session_id]);
    assert_success("session stop", &out);
    stdout_string(&out)
}

pub fn session_status(state_dir: &Path, session_id: &str) -> String {
    let text = std::fs::read_to_string(
        state_dir
            .join("sessions")
            .join(session_id)
            .join("state.json"),
    )
    .expect("read state.json");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    value["status"].as_str().expect("status field").to_string()
}

/// Simulates an external crash/kill of an arbitrary already-running pid
/// -- deliberately not going through this project's own detach/spawn
/// machinery, since the whole point is to act like an outside force
/// (OOM killer, `kill -9` from a shell, Task Manager) rather than this
/// project's own graceful shutdown. Duplicated here rather than reused
/// from `src/procutil.rs`: this package has no `[lib]` target (see
/// `ARCHITECTURE.md`), so an integration test can only drive the
/// compiled binary as a subprocess, never call its internal functions
/// directly -- the same reason every other helper in this module shells
/// out to `harness` instead of calling into it.
#[cfg(windows)]
pub fn force_kill(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // SAFETY: plain Win32 calls on a caller-supplied pid; the handle is
    // checked before use and closed on every path that opened one.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        assert!(
            !handle.is_null(),
            "OpenProcess({pid}) for force_kill failed: {:?}",
            std::io::Error::last_os_error()
        );
        let ok = TerminateProcess(handle, 1);
        let err = std::io::Error::last_os_error();
        CloseHandle(handle);
        assert!(
            ok != 0,
            "TerminateProcess({pid}) for force_kill failed: {err:?}"
        );
    }
}

#[cfg(unix)]
pub fn force_kill(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("run `kill -9`");
    assert!(status.success(), "kill -9 {pid} failed");
}

/// Spawns `session attach <id>` and collects up to `max_lines` of stdout
/// (or until `timeout` elapses), then kills the attach process. Used
/// instead of `run` because attach is a long-lived stream, not a
/// one-shot command.
pub fn attach_lines(
    state_dir: &Path,
    session_id: &str,
    max_lines: usize,
    timeout: Duration,
) -> Vec<String> {
    attach_lines_with_args(
        state_dir,
        &["session", "attach", session_id],
        max_lines,
        timeout,
    )
}

/// As [`attach_lines`], but with the full argv under the caller's
/// control -- used by the `--mode json` test to prefix the global
/// `--mode json` flag ahead of `session attach`, which `attach_lines`
/// itself has no way to express.
pub fn attach_lines_with_args(
    state_dir: &Path,
    args: &[&str],
    max_lines: usize,
    timeout: Duration,
) -> Vec<String> {
    let mut child = Command::new(bin())
        .args(args)
        .env("RUSTY_PRIME_AGENT_HOME", state_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn harness");
    let stdout = child.stdout.take().expect("piped stdout");

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut lines = Vec::new();
    let deadline = Instant::now() + timeout;
    while lines.len() < max_lines {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => lines.push(line),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    lines
}

pub fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
