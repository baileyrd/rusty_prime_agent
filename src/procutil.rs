//! Small, direct OS primitives this project needs that
//! `rusty_tokio::process::Command`/`Child` don't cover, because they're
//! about a pid this process did not itself spawn (crash-detection
//! liveness checks on a worker pid recorded in `state.json`, possibly
//! long after the process that spawned it exited) or about spawn-time
//! behavior `rusty_tokio`'s wrapper doesn't expose a builder method for
//! (real session-leader detachment).
//!
//! Everything else -- spawn, piped stdio, wait, single-child kill --
//! goes through `rusty_tokio::process::Command`/`Child` directly; see
//! `ARCHITECTURE.md` "Dependency Stack" for why this project no longer
//! wraps `rustils` itself for process management.

use std::io;

/// Marks `cmd` to survive this process exiting (including crashing) and
/// its terminal closing -- Required Behavior: "client disconnect does
/// not stop the worker" / a worker outliving a crashed supervisor.
///
/// - Windows: `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`, the same
///   flags `rustils`' own `Command::detach` sets, applied here via
///   `std::os::windows::process::CommandExt::creation_flags` (stable,
///   safe) through `rusty_tokio::process::Command::as_std_mut`'s escape
///   hatch.
/// - Unix: `rusty_tokio::process::Command` exposes `process_group`
///   (`setpgid`-before-`execve`, its own doc comment) but no
///   `setsid`/"new session" builtin -- `process_group(0)` alone would
///   place the child in a fresh *group* but leave it in the *same
///   session* as this process, still reachable by e.g. a `SIGHUP` this
///   process's own controlling terminal (if any) delivers to the whole
///   session on hangup. A `pre_exec` hook calling `libc::setsid()`
///   directly (via the same `as_std_mut` escape hatch) gets the real,
///   session-leader detachment `rustils`' `POSIX_SPAWN_SETSID` gave.
pub fn prepare_detached(cmd: &mut rusty_tokio::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.as_std_mut()
            .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure only calls `libc::setsid()` -- one
        // async-signal-safe POSIX call -- and always returns `Ok`,
        // never panics, allocates, or touches this (the parent's)
        // memory: the exact restricted-operation contract `pre_exec`'s
        // own safety documentation requires for the post-fork,
        // pre-exec window it runs in.
        unsafe {
            cmd.as_std_mut().pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
}

/// Is `pid` currently alive? Not tied to any child this process itself
/// spawned -- works for any pid, including a `prepare_detached`ed
/// worker's pid recorded in `state.json` long after the process that
/// spawned it exited.
///
/// Unix: `kill(pid, 0)` -- `Ok(true)` on success *or* `EPERM` (the pid
/// exists, just isn't signalable by this process), `Ok(false)` on
/// `ESRCH` (no such process) -- combined with [`is_zombie`], since a
/// process that has exited but not yet been reaped answers `kill(pid,
/// 0)` successfully and is emphatically not serving anything. Windows:
/// `OpenProcess` + `GetExitCodeProcess`; `Ok(true)` if the code is
/// `STILL_ACTIVE` or the open itself failed with anything other than
/// "no such process".
///
/// This answers "is *a* live process using this number", which is not
/// the same question as "is this still the process that recorded
/// itself" -- see [`is_same_process`] for that one.
pub fn is_alive(pid: u32) -> io::Result<bool> {
    #[cfg(unix)]
    {
        // SAFETY: `kill(pid, 0)` sends no signal; it only probes
        // existence/permission, per POSIX, for any valid pid value.
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            // Existence is not liveness: a zombie has already exited and
            // answers this successfully. See [`is_zombie`] for the real
            // session-wedging failure that omitting this caused.
            return Ok(!is_zombie(pid));
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(io::Error::last_os_error()),
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        const ERROR_INVALID_PARAMETER: i32 = 87;
        // SAFETY: `OpenProcess`/`GetExitCodeProcess`/`CloseHandle` are
        // plain Win32 calls on a caller-supplied pid / a handle this
        // function itself just opened; the handle is checked before use
        // and closed on every path that successfully opened one.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                // "No such process" -> false; any other failure (e.g.
                // access denied on a pid owned by another user) still
                // means the pid exists.
                return Ok(
                    io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER)
                );
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(exit_code == STILL_ACTIVE)
        }
    }
}

/// One whitespace-separated field of `/proc/<pid>/stat`, indexed from
/// the field *after* the executable-name field.
///
/// Split after the **last** `)` rather than by whitespace from the
/// beginning: field 2 is the executable name in parentheses and may
/// itself contain spaces and parentheses, which is the classic way a
/// naive `split_whitespace().nth(n)` silently reads the wrong field for
/// some processes. Index 0 here is therefore field 3 (`state`).
///
/// `Ok(None)` for a pid with no `/proc` entry (already reaped) or a
/// `stat` line that does not parse.
#[cfg(target_os = "linux")]
fn proc_stat_field(pid: u32, index: usize) -> io::Result<Option<String>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return Ok(None);
    };
    Ok(after_comm.split_whitespace().nth(index).map(str::to_string))
}

/// Has `pid` already exited but not yet been reaped by its parent?
///
/// Parity with `prime-agent`'s `R-PROC-03`: "liveness is `kill(pid, 0)`
/// combined, on POSIX, with a zombie check -- a zombie has already
/// exited and would otherwise count as alive to a naive probe." A
/// zombie answers `kill(pid, 0)` successfully, so without this a worker
/// that has exited reads as healthy for the whole window before its
/// parent reaps it, and the supervisor declines to respawn it.
///
/// That window is not theoretical here. It is exactly what wedged a
/// session while this function was being written: `daemon shutdown`
/// exits the supervisor and its worker at nearly the same moment, so the
/// worker sat unreaped, `is_alive` said "yes", and the next request got
/// `Connection refused` from a socket nobody was listening on any more.
///
/// A start-time fingerprint cannot substitute for this: a zombie has
/// both the same pid *and* the same start time as the process it is the
/// remains of, so it matches on every field there is to compare.
///
/// Unix-only, and gated rather than stubbed: Windows has no zombie
/// concept at all -- a terminated process's handle reports its exit code,
/// which `is_alive`'s own `GetExitCodeProcess` check already
/// distinguishes -- so there is nothing for this to do there and a
/// `false`-returning Windows arm would just be dead code. Matches
/// `prime-agent`'s own `R-PROC-04` ("the zombie check always returns
/// false on win32"), reached by not having the concept rather than by
/// answering it.
#[cfg(unix)]
fn is_zombie(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(proc_stat_field(pid, 0).ok().flatten().as_deref(), Some("Z"))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // `ps -o state=` reports `Z` for a zombie on macOS/BSD too; the
        // column can carry trailing flag characters (`Z+`), so this
        // checks the leading character rather than the whole field.
        std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('Z'))
            .unwrap_or(false)
    }
}

/// An opaque, per-platform fingerprint of *when* `pid` started, used to
/// tell a still-running process apart from an unrelated one that has
/// since been handed the same pid number.
///
/// Parity with `prime-agent`'s own lease-owner liveness check
/// (`R-WRK-14`/`R-WRK-15` in this project's spec-tree extraction of its
/// docs): "lease-owner liveness combines a raw process-existence check
/// with a process-start-time fingerprint match, so a reused PID from a
/// dead owner is never mistaken for the same live process."
/// [`is_alive`] alone cannot make that distinction -- it answers "does
/// *a* process with this number exist", not "is this still the process
/// that wrote `state.json`".
///
/// `Ok(None)` means "this platform could not tell us", which callers
/// treat as *no evidence of a mismatch* rather than as a mismatch --
/// see [`is_same_process`] for why that direction is the safe one.
///
/// The value is opaque and only ever compared for equality with an
/// earlier reading for the same pid. Nothing parses it, so the differing
/// per-platform formats below (clock ticks, a date string, a 64-bit
/// FILETIME) never need reconciling.
///
/// **Granularity, and why it is enough.** These clocks are coarse:
/// Linux's `starttime` counts clock ticks (typically 10ms) and macOS's
/// `ps -o lstart=` resolves only to the second, so two processes started
/// within the same tick genuinely do share a fingerprint (this project's
/// own unit test hit exactly that before it was made to space the two
/// spawns apart). That costs nothing here: a reused pid necessarily
/// arrives after the pid space has wrapped around, which is orders of
/// magnitude longer than a second, so the interval where two processes
/// are indistinguishable never overlaps the interval where pid reuse is
/// possible.
pub fn start_fingerprint(pid: u32) -> io::Result<Option<String>> {
    #[cfg(target_os = "linux")]
    {
        // Field 22 (`starttime`) of `/proc/<pid>/stat`: the process's
        // start time in clock ticks since boot. Read by splitting after
        // the *last* `)` rather than by whitespace from the beginning --
        // field 2 is the executable name in parentheses and may itself
        // contain spaces and parentheses, which is the classic way a
        // naive `split_whitespace().nth(21)` silently reads the wrong
        // field for some processes.
        // `proc_stat_fields` begins at field 3 (`state`), so field 22 is
        // index 22 - 3 = 19 within it.
        proc_stat_field(pid, 19)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // macOS/BSD have no `/proc`. `ps -o lstart=` is the portable
        // read of a process's start time here, and is what `prime-agent`
        // itself uses on these platforms (`R-WRK-15`). One-second
        // granularity, so a pid reused within the same second as its
        // predecessor's start is not distinguished -- an accepted limit,
        // since this narrows the window rather than claiming to close
        // it, and the alternative is hand-declaring `kinfo_proc` for a
        // `sysctl` call.
        let out = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()?;
        if !out.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if text.is_empty() { None } else { Some(text) })
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut ignored = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 3];
        // SAFETY: plain Win32 calls on a caller-supplied pid; the handle
        // is checked before use and closed on every path that opened
        // one, and all four `FILETIME` out-params are live locals.
        //
        // `GetProcessTimes` rather than shelling out to PowerShell for
        // `Process.StartTime.Ticks` the way `prime-agent` does
        // (`R-WRK-15`) -- same information, one syscall instead of
        // starting a PowerShell process per liveness check.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return Ok(None);
            }
            let ok = GetProcessTimes(
                handle,
                &mut creation,
                &mut ignored[0],
                &mut ignored[1],
                &mut ignored[2],
            );
            CloseHandle(handle);
            if ok == 0 {
                return Ok(None);
            }
        }
        let ticks = ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64);
        Ok(Some(ticks.to_string()))
    }
}

/// Is `pid` alive *and* still the same process that recorded
/// `expected_fingerprint`?
///
/// The PID-reuse-safe replacement for a bare [`is_alive`] call on a pid
/// read back off disk. A recovering supervisor that trusts `is_alive`
/// alone will decline to respawn a genuinely dead worker whose pid an
/// unrelated process now holds, leaving that session wedged with no live
/// worker and nothing to notice.
///
/// **Ambiguity resolves toward "alive", deliberately.** If
/// `expected_fingerprint` is `None` (a `state.json` written before this
/// field existed) or the current fingerprint cannot be read on this
/// platform, this reduces to [`is_alive`]. Being wrong in that direction
/// costs the narrow PID-reuse case this exists to catch; being wrong in
/// the other direction would declare a *live* worker dead and respawn a
/// second one onto the same session, which is a far worse failure than
/// the one being fixed.
pub fn is_same_process(pid: u32, expected_fingerprint: Option<&str>) -> io::Result<bool> {
    if !is_alive(pid)? {
        return Ok(false);
    }
    let Some(expected) = expected_fingerprint else {
        return Ok(true);
    };
    match start_fingerprint(pid) {
        Ok(Some(current)) => Ok(current == expected),
        // No reading available: see the doc comment above -- no evidence
        // of a mismatch is not evidence of one.
        Ok(None) | Err(_) => Ok(true),
    }
}

/// Terminates an arbitrary pid this process did not itself spawn --
/// `rp_server::shutdown`'s counterpart to `is_alive` above, for tearing
/// down the `rp-server` sidecar on `daemon shutdown`. Unix: `SIGTERM`
/// (graceful, not `SIGKILL` -- unlike `tests/common::force_kill`'s
/// deliberate hard-kill, which exists specifically to *simulate* a crash;
/// this is an ordinary, cooperative shutdown request). Windows has no
/// signal-delivery equivalent to ask a process to shut down
/// cooperatively, so `TerminateProcess` is the only primitive available
/// either way.
pub fn kill(pid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: `kill(pid, SIGTERM)` on a caller-supplied pid is a
        // plain, well-defined POSIX call.
        let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        // SAFETY: plain Win32 calls on a caller-supplied pid; the handle
        // is checked before use and closed on every path that opened one.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let ok = TerminateProcess(handle, 1);
            let err = io::Error::last_os_error();
            CloseHandle(handle);
            if ok != 0 {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_process_fingerprints_stably() {
        let me = std::process::id();
        let a = start_fingerprint(me).expect("fingerprint call should not error");
        assert!(
            a.is_some(),
            "every platform this project targets should be able to fingerprint its own pid"
        );
        let b = start_fingerprint(me).expect("fingerprint call should not error");
        assert_eq!(
            a, b,
            "a process's start time must not change between two readings"
        );
        assert!(!a.as_deref().unwrap().is_empty());
    }

    #[test]
    fn processes_started_far_enough_apart_fingerprint_differently() {
        // The property the whole mechanism rests on: a *later* process
        // holding the same pid number must not read as the earlier one.
        //
        // The sleep is load-bearing and sized to the coarsest granularity
        // this project supports. Linux's `starttime` is in clock ticks
        // (typically 10ms) and macOS's `ps -o lstart=` resolves only to
        // the second -- so without a gap wider than that, a parent and a
        // child it spawns immediately genuinely do share a fingerprint.
        // An earlier version of this test spawned the child straight away
        // and failed on exactly that: both read `23559` ticks.
        //
        // That granularity is not a problem for what this guards. A
        // reused pid necessarily arrives after the pid space has wrapped
        // -- far more than a second later -- so the window where two
        // processes are indistinguishable is nowhere near the window
        // where pid reuse is possible.
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["/C", "timeout", "/T", "30"]
            } else {
                vec!["30"]
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a short-lived child");

        let mine = start_fingerprint(std::process::id()).unwrap();
        let theirs = start_fingerprint(child.id()).unwrap();
        let _ = child.kill();
        let _ = child.wait();

        assert_ne!(
            mine, theirs,
            "a child spawned well after its parent must not fingerprint identically to it"
        );
    }

    #[test]
    fn is_same_process_rejects_a_mismatched_fingerprint() {
        let me = std::process::id();
        // Alive, and the fingerprint matches: the ordinary healthy case.
        let real = start_fingerprint(me).unwrap();
        assert!(is_same_process(me, real.as_deref()).unwrap());

        // Alive, but recorded by something else: exactly the reused-pid
        // case this exists to catch.
        assert!(
            !is_same_process(me, Some("definitely-not-this-processes-start-time")).unwrap(),
            "a live pid whose start time does not match the recording must not read as the same process"
        );

        // No recording at all (a pre-upgrade `state.json`): falls back to
        // the bare liveness check rather than declaring a live process
        // dead. See `is_same_process`'s own doc comment.
        assert!(is_same_process(me, None).unwrap());
    }

    #[test]
    fn an_unreaped_child_does_not_read_as_alive() {
        // The zombie case, and the one that actually wedged a session
        // during development: a child that has exited but whose parent
        // (this test) has not yet `wait`ed on it still answers
        // `kill(pid, 0)` successfully. It is serving nothing and must not
        // read as alive.
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                vec!["/C", "exit"]
            } else {
                vec![]
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a child that exits immediately");
        let pid = child.id();

        // Deliberately no `try_wait`/`wait` before the assertion:
        // both *reap* the child, which is exactly what makes the pid
        // disappear and would turn this into the easy already-gone case
        // rather than the zombie one. Polling `is_alive` is the only
        // wait here, so if the zombie check were missing this would spin
        // to the timeout and then fail on the assertion below.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while is_alive(pid).unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        assert!(
            !is_alive(pid).unwrap(),
            "an exited-but-unreaped process must not read as alive"
        );
        assert!(
            !is_same_process(pid, None).unwrap(),
            "a zombie must be dead to `is_same_process` too, fingerprint or not"
        );

        child.wait().expect("reap the child");
    }

    #[test]
    fn a_dead_pid_is_never_the_same_process() {
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                vec!["/C", "exit"]
            } else {
                vec![]
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a child that exits immediately");
        let pid = child.id();
        let fingerprint = start_fingerprint(pid).ok().flatten();
        child.wait().expect("reap the child");

        // Reaped, so the pid is genuinely gone rather than a zombie.
        assert!(!is_same_process(pid, fingerprint.as_deref()).unwrap());
        assert!(!is_same_process(pid, None).unwrap());
    }
}
