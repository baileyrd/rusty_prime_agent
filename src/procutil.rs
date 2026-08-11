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
        cmd.as_std_mut().creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
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
/// `ESRCH` (no such process). Windows: `OpenProcess` +
/// `GetExitCodeProcess`; `Ok(true)` if the code is `STILL_ACTIVE` or the
/// open itself failed with anything other than "no such process".
pub fn is_alive(pid: u32) -> io::Result<bool> {
    #[cfg(unix)]
    {
        // SAFETY: `kill(pid, 0)` sends no signal; it only probes
        // existence/permission, per POSIX, for any valid pid value.
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return Ok(true);
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
                return Ok(io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER));
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
