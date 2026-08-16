//! Generation-fenced per-worker tokens (`src/fence.rs`) -- parity with
//! `prime-agent`'s `daemon.md`: "Private worker connections authenticate
//! with a per-worker token fenced to the current supervisor generation,
//! preventing a stale replacement supervisor from commanding an adopted
//! worker."
//!
//! Companion to `tests/supervisor_restart_recovery.rs` rather than a
//! replacement for it: that file proves a replacement supervisor *can*
//! still drive a still-live worker (which now silently exercises the
//! whole adopt-then-authenticate path -- if fencing were broken in the
//! permissive direction, every test in that file would fail). This file
//! proves the other half, the half a passing happy path can never show:
//! that the fence actually *refuses* a supervisor it shouldn't serve.
//!
//! Everything here is black-box, through the real CLI and the real
//! on-disk fence file. The worker holds its fence in memory for its whole
//! life (`worker::run` reads it once at startup), so editing the file
//! under a running worker is exactly how a test forges "a supervisor that
//! cannot prove it should be trusted" without needing to run two real
//! supervisors at once.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

fn fence_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("sessions")
        .join(session_id)
        .join("worker-fence.json")
}

fn read_fence(state_dir: &Path, session_id: &str) -> serde_json::Value {
    let path = fence_path(state_dir, session_id);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text).expect("worker-fence.json is valid JSON")
}

/// `daemon status` prints `generation=N` -- the same monotonic counter
/// the fence's `supervisor.counter` half is built from.
fn daemon_generation(state_dir: &Path) -> u64 {
    let status = common::daemon_status(state_dir);
    status
        .split_whitespace()
        .find_map(|field| field.strip_prefix("generation="))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no generation= field in daemon status: {status}"))
}

#[test]
fn a_spawned_worker_is_fenced_to_the_supervisor_that_spawned_it() {
    let state_dir = common::TempDir::new("fence-written");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let fence = read_fence(state_dir.path(), &session_id);
    let token = fence["worker_token"]
        .as_str()
        .expect("worker_token is a string");
    assert_eq!(token.len(), 32, "worker token should be 128 bits of hex");
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "worker token should be hex, got {token}"
    );
    assert_ne!(
        token,
        "0".repeat(32),
        "a zeroed token would mean the platform randomness arm silently did nothing"
    );

    assert_eq!(
        fence["supervisor"]["counter"].as_u64(),
        Some(daemon_generation(state_dir.path())),
        "the fence must name the supervisor generation that actually spawned this worker"
    );
    let instance = fence["supervisor"]["instance"]
        .as_str()
        .expect("instance is a string");
    assert_eq!(instance.len(), 32, "instance should be 128 bits of hex");

    // Two sessions under one supervisor share that supervisor's identity
    // but must never share a worker token -- the token is per *worker*,
    // and reusing one would let an adoption of either worker authorize
    // an adoption of the other.
    let second_id = common::session_new(state_dir.path(), None);
    let second = read_fence(state_dir.path(), &second_id);
    assert_eq!(
        second["supervisor"], fence["supervisor"],
        "both workers are fenced to the same supervisor"
    );
    assert_ne!(
        second["worker_token"], fence["worker_token"],
        "each worker must get its own token"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fence_path(state_dir.path(), &session_id))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the fence file holds an auth token -- it must be owner-only \
             (daemon.md's own R-WRK-03)"
        );
    }

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn a_replacement_supervisor_adopts_the_fence_and_keeps_serving() {
    let state_dir = common::TempDir::new("fence-adoption");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before the restart");

    let before = read_fence(state_dir.path(), &session_id);
    let worker_pid_before = common::worker_pid(state_dir.path(), &session_id);
    let token_before = before["worker_token"].as_str().unwrap().to_string();

    common::force_kill(common::daemon_pid(state_dir.path()));
    assert!(
        common::wait_until(
            || common::run(state_dir.path(), &["daemon", "status"])
                .status
                .code()
                != Some(0),
            Duration::from_secs(5)
        ),
        "the killed supervisor's socket should stop answering"
    );
    common::daemon_start(state_dir.path());

    // The worker is detached, so it survived -- this is an adoption, not
    // a respawn. If it had been respawned, the assertions below would be
    // testing the trivial fresh-spawn path instead of the real one.
    assert_eq!(
        worker_pid_before,
        common::worker_pid(state_dir.path(), &session_id),
        "the still-live worker must be adopted, not replaced"
    );

    let after = read_fence(state_dir.path(), &session_id);
    assert_eq!(
        after["worker_token"].as_str(),
        Some(token_before.as_str()),
        "adoption transfers the fence; it does not rotate the worker's own token"
    );
    assert!(
        after["supervisor"]["counter"].as_u64() > before["supervisor"]["counter"].as_u64(),
        "the adopting supervisor's counter must strictly supersede the displaced one \
         (that is what a stale supervisor cannot produce)"
    );
    assert_ne!(
        after["supervisor"]["instance"], before["supervisor"]["instance"],
        "a new supervisor process means a new instance id"
    );

    // The real proof that the adoption took: the new supervisor can
    // still command the worker it took over.
    let reply = common::session_prompt(state_dir.path(), &session_id, "after the restart");
    assert!(
        !reply.trim().is_empty(),
        "an adopted worker must keep serving prompts, got {reply:?}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn a_supervisor_that_cannot_prove_the_worker_token_is_refused() {
    let state_dir = common::TempDir::new("fence-refusal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before the restart");
    let worker_pid = common::worker_pid(state_dir.path(), &session_id);

    common::force_kill(common::daemon_pid(state_dir.path()));
    assert!(
        common::wait_until(
            || common::run(state_dir.path(), &["daemon", "status"])
                .status
                .code()
                != Some(0),
            Duration::from_secs(5)
        ),
        "the killed supervisor's socket should stop answering"
    );

    // Forge the failure. The live worker still holds the *real* token in
    // memory, so a replacement supervisor reading this file now presents
    // a token the worker will not recognise -- the same rejection an
    // impostor would get, reachable without running a second supervisor.
    let path = fence_path(state_dir.path(), &session_id);
    let mut fence = read_fence(state_dir.path(), &session_id);
    fence["worker_token"] = serde_json::Value::String("f".repeat(32));
    std::fs::write(&path, serde_json::to_string_pretty(&fence).unwrap()).unwrap();

    common::daemon_start(state_dir.path());

    // Startup adoption failed (non-fatally, by design -- one session's
    // failure must not stop the supervisor coming up), so the worker is
    // still fenced to the dead supervisor and refuses this one.
    assert!(
        common::run(state_dir.path(), &["daemon", "status"])
            .status
            .success(),
        "a failed adoption must not stop the supervisor from serving everything else"
    );
    assert_eq!(
        worker_pid,
        common::worker_pid(state_dir.path(), &session_id),
        "the worker is still alive -- it is refusing the supervisor, not gone"
    );

    let output = common::run(
        state_dir.path(),
        &["session", "prompt", &session_id, "should be refused"],
    );
    assert!(
        !output.status.success(),
        "a fenced-out supervisor must not be able to prompt the worker; got success: {}",
        common::stdout_string(&output)
    );
    let combined = format!(
        "{}{}",
        common::stdout_string(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("fenced"),
        "the refusal should say what happened rather than surfacing as a dropped \
         connection; got: {combined}"
    );

    // A brand-new session under the same supervisor is unaffected -- the
    // fence is per worker, so one refused worker is not a daemon-wide
    // outage.
    let fresh_id = common::session_new(state_dir.path(), None);
    assert!(
        !common::session_prompt(state_dir.path(), &fresh_id, "unaffected")
            .trim()
            .is_empty(),
        "a worker this supervisor spawned itself must still work"
    );

    common::daemon_shutdown(state_dir.path());
}

/// A `worker-fence.json` that goes missing under a worker that is
/// genuinely fenced (deleted by hand, a partial state-dir restore) must
/// fail *closed*: the replacement supervisor has no token to present, the
/// worker refuses it, and the session becomes unreachable rather than
/// silently changing hands.
///
/// Documents the operator escape hatch too, since "fail closed" is only
/// an acceptable answer if there is a way back: killing the worker drops
/// the session onto the ordinary crash-recovery path, which respawns it
/// with a fresh fence.
#[test]
fn a_missing_fence_file_under_a_fenced_worker_fails_closed() {
    let state_dir = common::TempDir::new("fence-missing");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before");
    let worker_pid = common::worker_pid(state_dir.path(), &session_id);

    common::force_kill(common::daemon_pid(state_dir.path()));
    assert!(
        common::wait_until(
            || common::run(state_dir.path(), &["daemon", "status"])
                .status
                .code()
                != Some(0),
            Duration::from_secs(5)
        ),
        "the killed supervisor's socket should stop answering"
    );
    std::fs::remove_file(fence_path(state_dir.path(), &session_id)).unwrap();

    common::daemon_start(state_dir.path());

    // Refused, not taken over. The worker holds the real fence in memory
    // for its whole life, so deleting the file on disk gives an adopter
    // nothing to prove itself with.
    let output = common::run(
        state_dir.path(),
        &["session", "prompt", &session_id, "should be refused"],
    );
    assert!(
        !output.status.success(),
        "a supervisor with no token must not be able to command the worker; got: {}",
        common::stdout_string(&output)
    );
    assert_eq!(
        worker_pid,
        common::worker_pid(state_dir.path(), &session_id),
        "failing closed means refusing, not killing the worker out from under itself"
    );

    // The way back: the worker dying drops this onto the ordinary
    // worker-crash path, which respawns with a freshly minted fence.
    common::force_kill(worker_pid);
    // A single blocking attempt, no retry window. This used to need one:
    // the worker's spawning supervisor was force-killed earlier in this
    // test, so the freshly killed worker sat unreaped, `procutil::is_alive`
    // was a bare `kill(pid, 0)` that reads a zombie as alive, and the
    // supervisor skipped the respawn until the kernel got around to
    // reaping. `is_alive` now excludes zombies (`procutil::is_zombie`), so
    // the very first attempt sees a dead worker and respawns.
    let recovered = common::run(
        state_dir.path(),
        &["session", "prompt", &session_id, "after recovery"],
    );
    assert!(
        recovered.status.success(),
        "killing the refusing worker must let the session recover normally; got: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let fence = read_fence(state_dir.path(), &session_id);
    assert_eq!(
        fence["supervisor"]["counter"].as_u64(),
        Some(daemon_generation(state_dir.path())),
        "the respawned worker is fenced to the supervisor that respawned it"
    );

    common::daemon_shutdown(state_dir.path());
}

/// The in-place upgrade path: a worker whose state directory predates the
/// fence mechanism reads no fence at startup and is therefore *unfenced*
/// -- it serves requests with no preamble. The next supervisor restart
/// must converge it onto a real fence rather than leaving it unfenced
/// forever.
///
/// Reproduced by starting a worker directly (`harness __worker-main`)
/// with no fence file on disk, which is exactly the state a pre-fence
/// binary's worker would be in. This is a regression test for a real bug,
/// not a hypothetical one: `adopt_worker` originally minted a placeholder
/// fence for the missing-file case and then hit its own "already ours"
/// short-circuit on it, so the adoption round trip never happened and a
/// pre-fence session would have stayed unfenced permanently.
#[test]
fn an_unfenced_worker_is_converged_onto_a_fence_by_the_next_supervisor() {
    let state_dir = common::TempDir::new("fence-upgrade");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before the upgrade");

    // Tear everything down, then rebuild the pre-fence world: no fence
    // file, and a worker that started without one.
    common::force_kill(common::daemon_pid(state_dir.path()));
    common::force_kill(common::worker_pid(state_dir.path(), &session_id));
    assert!(
        common::wait_until(
            || common::run(state_dir.path(), &["daemon", "status"])
                .status
                .code()
                != Some(0),
            Duration::from_secs(5)
        ),
        "the killed supervisor's socket should stop answering"
    );
    std::fs::remove_file(fence_path(state_dir.path(), &session_id)).unwrap();

    let mut unfenced = std::process::Command::new(common::bin())
        .args(["__worker-main", "--session-id", &session_id, "--state-root"])
        .arg(state_dir.path())
        .args(["--mode", "resume"])
        .env("RUSTY_PRIME_AGENT_HOME", state_dir.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn an unfenced worker directly");
    let unfenced_pid = unfenced.id();
    assert!(
        common::wait_until(
            || common::worker_pid(state_dir.path(), &session_id) == unfenced_pid,
            Duration::from_secs(20)
        ),
        "the hand-started worker should record itself in state.json"
    );
    assert!(
        !fence_path(state_dir.path(), &session_id).exists(),
        "the point of this test is a worker running with no fence at all"
    );

    common::daemon_start(state_dir.path());

    assert_eq!(
        unfenced_pid,
        common::worker_pid(state_dir.path(), &session_id),
        "this must be an adoption of the unfenced worker, not a respawn -- \
         otherwise the test proves nothing about the upgrade path"
    );
    let fence = read_fence(state_dir.path(), &session_id);
    assert_eq!(
        fence["supervisor"]["counter"].as_u64(),
        Some(daemon_generation(state_dir.path())),
        "convergence must leave the worker fenced to the supervisor that adopted it"
    );
    assert_eq!(
        fence["worker_token"].as_str().map(str::len),
        Some(32),
        "convergence must mint a real token, not an empty placeholder"
    );

    assert!(
        !common::session_prompt(state_dir.path(), &session_id, "after the upgrade")
            .trim()
            .is_empty(),
        "converging a worker onto a fence must not break it"
    );

    common::daemon_shutdown(state_dir.path());
    // Killed *and* reaped: this test is the only place that spawns a
    // worker itself rather than letting the daemon do it, so it is also
    // the only place responsible for not leaving a zombie behind (the
    // same reasoning `worker::spawn`'s own reaper task documents).
    let _ = unfenced.kill();
    let _ = unfenced.wait();
}
