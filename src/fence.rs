//! Generation-fenced per-worker tokens: the authority mechanism that
//! decides which supervisor process is allowed to command a given
//! worker.
//!
//! Parity with `prime-agent`'s `daemon.md`: "Private worker connections
//! authenticate with a per-worker token fenced to the current supervisor
//! generation, preventing a stale replacement supervisor from commanding
//! an adopted worker." See `COMPARISON.md` §4 for why this was the
//! highest-leverage idea in that reference design this project hadn't
//! adopted.
//!
//! # The problem it actually solves
//!
//! `transport::Listener::bind_with_retry` answers "is the process that
//! owns this stale socket file genuinely dead, or merely wedged?"
//! *empirically* -- a real `Ping`/`Pong` round trip, a 20-second budget,
//! and a long doc comment recording the Windows `AF_UNIX` reclaim race
//! that made a bare `connect()` verdict untrustworthy. That check is
//! still needed and still correct (a live listener really does own its
//! path), but it is answering a question that fencing makes *unaskable*:
//! even if the classification is wrong and an old supervisor is still
//! running, a fenced worker rejects its commands outright rather than
//! serving two masters.
//!
//! # The mechanism
//!
//! Each worker has a [`WorkerFence`] on disk next to its `state.json`,
//! written by whichever supervisor spawned it:
//!
//! - `worker_token` -- a per-worker 128-bit random secret, minted once at
//!   spawn and stable for that worker process's whole life. Presenting it
//!   is what authorizes *changing* the fence (adoption), not ordinary
//!   traffic.
//! - `supervisor` -- the [`SupervisorIdentity`] currently authorized to
//!   command this worker.
//!
//! Every private supervisor->worker connection opens with
//! `Request::WorkerAuth { supervisor }`; the worker requires exact
//! equality against its current fence before serving anything. A
//! replacement supervisor adopting a still-live worker it did not spawn
//! sends `Request::WorkerAdopt { worker_token, supervisor }` instead,
//! which advances the fence.
//!
//! # Why ordering, when upstream uses pure identity-equality
//!
//! `prime-agent`'s generation is a random UUID per supervisor instance
//! and comparison there is *never* ordering (`R-PROTO-18` in this
//! project's spec-tree extraction of its docs). It can afford that
//! because a separate atomic launch lease (`R-SUP-03`) already
//! guarantees exactly one legitimate supervisor at a time, so "is this
//! adopter legitimate?" is answered before the token ever comes up.
//!
//! This project has no such lease -- `COMPARISON.md` §3.3 records that
//! `Supervisor::spawn_lock` is in-process memory standing in for one. So
//! identity-equality alone would be unsound here: a stale supervisor can
//! read `worker_token` off disk (it is the same OS user; this is not a
//! privilege boundary and never claims to be) and simply re-adopt the
//! worker back, which would make the whole fence a no-op.
//!
//! [`SupervisorIdentity`] therefore carries *both* halves:
//!
//! - `counter` -- the monotonic `u64` already persisted in `daemon.pid`
//!   and bumped by every supervisor at startup (`daemon::
//!   record_daemon_pid`). Adoption requires **strictly greater**, which
//!   is what makes a stale supervisor unable to re-adopt: its counter is
//!   by construction lower than the replacement's.
//! - `instance` -- 128 random bits, fresh per supervisor process.
//!   Ordinary traffic requires exact equality on the whole pair, so a
//!   counter value alone is not a forgeable credential, and two
//!   supervisors that raced to the same counter (see below) still cannot
//!   be mistaken for each other.
//!
//! **Known bounded weakness, stated rather than hidden:** two supervisors
//! starting concurrently can both read the same `previous_generation` and
//! both write `counter = N + 1`. Neither can then adopt the other's
//! workers, because strictly-greater fails in both directions. That
//! degrades to "adoption fails and says so on stderr" -- a visible,
//! recoverable outcome -- rather than to "both supervisors command the
//! same worker", which is the outcome this module exists to prevent. It
//! is the correct failure direction, but it is a failure, and closing it
//! properly needs the on-disk supervisor launch lease `COMPARISON.md` §14
//! item 4 tracks separately, not a change here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Context, HarnessError, Result};

/// Which supervisor process a worker is currently fenced to.
///
/// See this module's own doc comment for why this is a `(counter,
/// instance)` pair rather than upstream's bare per-instance UUID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorIdentity {
    /// The monotonic generation from `daemon.pid`, bumped once per
    /// supervisor startup. Compared with `>` on adoption.
    pub counter: u64,
    /// 128 random bits identifying this specific supervisor *process*.
    /// Compared with `==` on every ordinary request.
    pub instance: String,
}

impl SupervisorIdentity {
    /// Mints the identity for the currently-starting supervisor process.
    /// `counter` comes from `daemon::record_daemon_pid`'s return value --
    /// this function deliberately does not read `daemon.pid` itself, so
    /// the bump-and-record stays in one place.
    pub(crate) fn new(counter: u64) -> Result<Self> {
        Ok(SupervisorIdentity {
            counter,
            instance: random_hex_128(Context::Daemon)?,
        })
    }

    /// Whether `self` may take over a worker currently fenced to
    /// `current`. Strictly-greater on the counter: see this module's doc
    /// comment for why equality is not enough and why the tie case
    /// (concurrent startup) deliberately fails closed.
    pub(crate) fn may_adopt_from(&self, current: &SupervisorIdentity) -> bool {
        self.counter > current.counter
    }
}

impl std::fmt::Display for SupervisorIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Truncated `instance` -- the full 32 hex chars carry no extra
        // diagnostic value in a log line, and this string ends up in
        // user-visible `Conflict` errors.
        write!(f, "gen {}/{}", self.counter, &self.instance[..8])
    }
}

/// The on-disk fence for one worker, at [`crate::paths::worker_fence_path`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerFence {
    /// Per-worker secret, minted at spawn and never rotated for that
    /// worker process's lifetime. Required to *advance* the fence.
    pub worker_token: String,
    /// The supervisor currently authorized to command this worker.
    pub supervisor: SupervisorIdentity,
}

impl WorkerFence {
    /// Mints a fresh fence for a worker `supervisor` is about to spawn.
    pub(crate) fn mint(supervisor: SupervisorIdentity) -> Result<Self> {
        Ok(WorkerFence {
            worker_token: random_hex_128(Context::Daemon)?,
            supervisor,
        })
    }

    /// Writes the fence to `path`, owner-only.
    ///
    /// Not atomic-rename: a torn fence file is not a correctness hazard
    /// the way a torn `state.json` would be. The worker holds the
    /// authoritative copy in memory for its whole life and only ever
    /// *re-reads* this file at startup, at which point a torn file means
    /// the spawn itself failed and there is no worker to fence. The
    /// supervisor re-reads it only to learn `worker_token` for adoption,
    /// where a parse failure correctly means "cannot adopt".
    pub(crate) fn write(&self, context: Context, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| HarnessError::json(context, Some(path.to_path_buf()), e))?;
        std::fs::write(path, json)
            .map_err(|e| HarnessError::io(context, Some(path.to_path_buf()), e))?;
        restrict_to_owner(context, path)
    }

    /// Reads the fence back. A missing file is `Ok(None)` -- see
    /// [`crate::worker`]'s startup path for why an unfenced worker is a
    /// real, supported state rather than an error (in-process/SDK and
    /// test callers never spawn through the daemon at all).
    pub(crate) fn read(context: Context, path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(HarnessError::io(context, Some(path.to_path_buf()), e)),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| HarnessError::json(context, Some(path.to_path_buf()), e))
    }
}

/// Best-effort `0600` on Unix; a no-op on Windows.
///
/// Mirrors `daemon.md`'s "worker descriptors, auth tokens, active-session
/// IDs, paths, and recovery journals are written with owner-only
/// permissions" (`R-WRK-03`). On Windows the containing tree is already
/// `%LOCALAPPDATA%`, which carries a per-user ACL by default, and this
/// project has no ACL-manipulation code anywhere else to be consistent
/// with -- adding one for this file alone would be the only such call in
/// the codebase.
fn restrict_to_owner(context: Context, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| HarnessError::io(context, Some(path.to_path_buf()), e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (context, path);
    }
    Ok(())
}

/// 128 bits of OS randomness, lowercase hex.
///
/// Straight to the OS on both platforms rather than through a `rand`-
/// shaped dependency, matching this project's existing posture
/// (`ARCHITECTURE.md` "Dependency stack": `zmtp.rs`/`sha256.rs` hand-roll
/// a wire protocol and its HMAC rather than take a crate that would drag
/// a second async runtime in). Both calls below are the platform's own
/// documented CSPRNG entry point, not a hand-rolled generator.
pub(crate) fn random_hex_128(context: Context) -> Result<String> {
    let mut bytes = [0u8; 16];
    fill_random(context, &mut bytes)?;
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}

#[cfg(unix)]
fn fill_random(context: Context, buf: &mut [u8]) -> Result<()> {
    use std::io::Read as _;
    let path = PathBuf::from("/dev/urandom");
    let mut f =
        std::fs::File::open(&path).map_err(|e| HarnessError::io(context, Some(path.clone()), e))?;
    f.read_exact(buf)
        .map_err(|e| HarnessError::io(context, Some(path), e))
}

#[cfg(windows)]
fn fill_random(context: Context, buf: &mut [u8]) -> Result<()> {
    // `BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` and a null
    // algorithm handle -- the documented way to get system randomness
    // without opening (and having to close) an algorithm provider first.
    let status = unsafe {
        windows_sys::Win32::Security::Cryptography::BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            windows_sys::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(HarnessError::io(
            context,
            None,
            std::io::Error::other(format!("BCryptGenRandom failed: NTSTATUS 0x{status:08x}")),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_128_is_32_hex_chars_and_not_constant() {
        let a = random_hex_128(Context::Daemon).expect("randomness available");
        let b = random_hex_128(Context::Daemon).expect("randomness available");
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Not a statistical test -- just proof the source isn't a
        // constant/zeroed buffer, which is the realistic way a
        // platform arm could be wrong without failing outright.
        assert_ne!(a, b);
        assert_ne!(a, "0".repeat(32));
    }

    #[test]
    fn adoption_requires_a_strictly_greater_counter() {
        let old = SupervisorIdentity {
            counter: 4,
            instance: "a".repeat(32),
        };
        let replacement = SupervisorIdentity {
            counter: 5,
            instance: "b".repeat(32),
        };
        assert!(replacement.may_adopt_from(&old));
        // The stale supervisor cannot take it back -- the whole point.
        assert!(!old.may_adopt_from(&replacement));
        // Concurrent-startup tie: fails closed in both directions.
        let tie = SupervisorIdentity {
            counter: 5,
            instance: "c".repeat(32),
        };
        assert!(!tie.may_adopt_from(&replacement));
        assert!(!replacement.may_adopt_from(&tie));
    }

    #[test]
    fn fence_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "rpa-fence-{}-{}",
            std::process::id(),
            random_hex_128(Context::Daemon).unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("worker-fence.json");

        assert!(WorkerFence::read(Context::Worker, &path).unwrap().is_none());

        let fence = WorkerFence::mint(SupervisorIdentity::new(7).unwrap()).unwrap();
        fence.write(Context::Daemon, &path).unwrap();
        let read_back = WorkerFence::read(Context::Worker, &path).unwrap().unwrap();
        assert_eq!(read_back, fence);
        assert_eq!(read_back.supervisor.counter, 7);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "fence file must be owner-only");
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
