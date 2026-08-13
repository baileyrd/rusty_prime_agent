//! `harness update [--force]` -- bounded, honest parity with
//! `prime-agent update [--force]`. See `PARITY.md`'s own "Self-update"
//! entry for the full story of why the real thing (`prime-agent` is
//! published to npm, so its own `update` presumably checks that registry
//! for a newer release) doesn't translate directly: `Cargo.toml` here
//! says `publish = false`, there's no crates.io release, and no GitHub
//! Releases workflow -- there is no release channel to check *against*.
//!
//! What's real instead: this project is only ever built one way (`cargo
//! build --release` run directly in a git checkout of its own source --
//! see the README's own "Build" section), so a genuine, if narrower,
//! "update" is available for the one case that already covers every
//! real user of this binary -- pull the latest commit into that same
//! checkout and rebuild. [`SOURCE_ROOT`] is that checkout's path,
//! embedded into the binary at compile time via `CARGO_MANIFEST_DIR`
//! (set by Cargo to the directory containing this crate's own
//! `Cargo.toml`) -- accurate for exactly the binary this project ever
//! produces, and if that directory has since moved, been deleted, or
//! this binary was copied somewhere else entirely, [`run`] fails loudly
//! with that path named in the error rather than silently doing nothing
//! or guessing at a different location.
//!
//! `--force` deliberately does **not** mean "discard local changes" --
//! this project's own operating rules (see the top-level `CLAUDE.md`-
//! equivalent this whole codebase was built under) treat discarding
//! uncommitted work as something that needs an explicit, separate ask,
//! never a side effect of an unrelated flag. `git pull` already refuses,
//! loudly, to overwrite uncommitted changes a merge would touch --
//! that's git's own real protection, and nothing here duplicates or
//! second-guesses it. `--force` instead means "rebuild even if `git
//! pull` reports nothing new" -- useful after manually editing a
//! tracked file or switching branches by hand, cases where skipping the
//! rebuild because `git pull` alone saw no upstream change would be
//! wrong.

use std::path::Path;

use rusty_tokio::process::Command;

use crate::error::{Context, HarnessError, Result};

/// The git checkout this binary was built from -- see this module's own
/// doc comment for why that's a meaningful, accurate thing to embed
/// rather than a guess.
const SOURCE_ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Runs the update: `git pull` in [`SOURCE_ROOT`], then (unless `git
/// pull` reported nothing new and `force` is `false`) `cargo build
/// --release` in the same directory. Returns a human-readable summary on
/// success; any failure (missing checkout, `git pull` conflict, build
/// error) is a loud [`HarnessError`], never a silent no-op.
pub async fn run(force: bool) -> Result<String> {
    run_in(Path::new(SOURCE_ROOT), force).await
}

/// The actual implementation, taking `root` as a parameter rather than
/// reading [`SOURCE_ROOT`] directly -- exists so the "checkout not
/// found" error path has a CI-safe unit test (a plain temp directory,
/// no real git/cargo invoked) independent of [`run`]'s own `#[ignore]`d
/// real-checkout coverage, which does invoke both for real.
async fn run_in(root: &Path, force: bool) -> Result<String> {
    if !root.join(".git").exists() {
        return Err(HarnessError::conflict(
            Context::Cli,
            format!(
                "no release channel: source checkout not found at {} (embedded at build time) -- \
                 this project isn't published to any package registry (Cargo.toml says `publish = \
                 false`, no GitHub Releases), so self-update only works from the same git checkout \
                 `cargo build --release` originally ran in",
                root.display()
            ),
        ));
    }

    let pull = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["pull"])
        .output()
        .await
        .map_err(|e| HarnessError::io(Context::Cli, None, e))?;
    if !pull.status.success() {
        return Err(HarnessError::conflict(
            Context::Cli,
            format!(
                "git pull failed:\n{}",
                String::from_utf8_lossy(&pull.stderr).trim()
            ),
        ));
    }
    let pull_summary = String::from_utf8_lossy(&pull.stdout).trim().to_string();
    let already_up_to_date = pull_summary.contains("Already up to date");
    if already_up_to_date && !force {
        return Ok(format!(
            "{pull_summary} -- nothing to rebuild (pass --force to rebuild anyway)"
        ));
    }

    let build = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(root)
        .output()
        .await
        .map_err(|e| HarnessError::io(Context::Cli, None, e))?;
    if !build.status.success() {
        return Err(HarnessError::conflict(
            Context::Cli,
            format!(
                "cargo build --release failed:\n{}",
                String::from_utf8_lossy(&build.stderr).trim()
            ),
        ));
    }

    Ok(format!(
        "{pull_summary} -- rebuilt {}/target/release/harness. Restart the daemon (`daemon shutdown` \
         then `daemon start`) to run the new code.",
        root.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test binary's own `CARGO_MANIFEST_DIR` really is a git
    /// checkout (this crate's own repo) -- confirms [`SOURCE_ROOT`]
    /// resolves to a real, `.git`-containing directory rather than
    /// exercising the "checkout not found" error path, which needs a
    /// binary built somewhere that isn't a checkout to trigger for real
    /// (not reproducible from inside this same checkout's own tests).
    #[test]
    fn source_root_is_a_real_git_checkout() {
        assert!(Path::new(SOURCE_ROOT).join(".git").exists());
    }

    /// A directory that exists but was never `git init`-ed reads as "no
    /// release channel" rather than attempting `git pull` against it --
    /// CI-safe: no real `git`/`cargo` invocation, no network, no mutation
    /// of this project's own checkout.
    #[rusty_tokio::test]
    async fn run_in_reports_no_release_channel_outside_a_git_checkout() {
        let dir = std::env::temp_dir().join(format!("rpa-self-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let err = run_in(&dir, false).await.unwrap_err();
        assert!(err.to_string().contains("no release channel"), "got: {err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A real, slow, network- and toolchain-dependent end-to-end check
    /// against this project's own actual checkout: `git pull` (real
    /// network access to `origin`) then, unless nothing changed,
    /// `cargo build --release` (a real, non-trivial rebuild). `#[ignore]`d
    /// for the same reason every other genuinely-external-state test in
    /// this project is -- CI's own checkout is typically a shallow,
    /// detached-HEAD clone with no tracking branch `git pull` could
    /// resolve, and running a full `cargo build --release` on every test
    /// run would be far too slow to be part of the default suite. Run
    /// manually (`cargo test --lib -- --ignored self_update`) from a
    /// real clone with a tracking branch to verify this for real.
    #[rusty_tokio::test]
    #[ignore]
    async fn run_against_the_real_checkout_pulls_and_rebuilds() {
        let outcome = run(false).await;
        assert!(outcome.is_ok(), "expected success, got: {outcome:?}");
    }
}
