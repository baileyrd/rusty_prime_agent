//! Parity with `prime-agent`'s tolerance for addressing a session by a
//! short, unambiguous prefix of its id rather than the full string --
//! see `daemon::Daemon::resolve_session_id`'s own doc comment for the
//! resolution rule (exact match wins outright; otherwise exactly one
//! `catalog::scan` prefix match resolves, zero or more than one both
//! fall through to the existing "unknown session" error path unchanged).
//! Exercised here through `session stop`/`session goal show`/`session
//! schedule list`, three of the seven real session-id-validation
//! entry points -- not all seven, since they all share the same helper
//! and `resolve_worker` alone (proven via `session stop`) already covers
//! the other six.

mod common;

#[test]
fn an_unambiguous_prefix_resolves_the_same_as_the_full_id() {
    let state_dir = common::TempDir::new("partial-id-unambiguous");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    // `sess-<hex nanos>-<hex pid>`: the first chars after `sess-` are
    // already enough entropy to be unique among the handful of sessions
    // a single test creates.
    let prefix = &session_id[..12.min(session_id.len())];

    let out = common::run(state_dir.path(), &["session", "goal", "show", prefix]);
    common::assert_success("session goal show <prefix>", &out);
    assert_eq!(common::stdout_string(&out), "no goal");

    let out = common::run(state_dir.path(), &["session", "schedule", "list", prefix]);
    common::assert_success("session schedule list <prefix>", &out);
    assert_eq!(common::stdout_string(&out), "no schedules");

    let ack = common::session_stop(state_dir.path(), prefix);
    assert!(
        ack.contains("session stopped"),
        "expected 'session stopped', got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn a_full_exact_id_still_works_even_when_it_is_also_a_prefix_of_another_session() {
    let state_dir = common::TempDir::new("partial-id-exact-wins");
    common::daemon_start(state_dir.path());

    let first = common::session_new(state_dir.path(), None);
    let _second = common::session_new(state_dir.path(), None);

    // `first` is a real, exact session id and (in principle) could also
    // be a literal prefix of some other session's id -- the fast exact
    // match must win outright rather than falling into the ambiguous
    // multi-match path.
    let out = common::run(state_dir.path(), &["session", "goal", "show", &first]);
    common::assert_success("session goal show <exact full id>", &out);
    assert_eq!(common::stdout_string(&out), "no goal");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn an_ambiguous_prefix_reports_unknown_session_not_a_crash() {
    let state_dir = common::TempDir::new("partial-id-ambiguous");
    common::daemon_start(state_dir.path());

    let _a = common::session_new(state_dir.path(), None);
    let _b = common::session_new(state_dir.path(), None);

    // Every session id starts with "sess-", so this prefix matches both
    // sessions created above -- must fall through unresolved rather than
    // guessing.
    let out = common::run(state_dir.path(), &["session", "goal", "show", "sess-"]);
    assert!(
        !out.status.success(),
        "an ambiguous prefix must not silently pick a session"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown session"),
        "expected an 'unknown session' message, got: {stderr}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn a_prefix_matching_no_session_reports_unknown_session() {
    let state_dir = common::TempDir::new("partial-id-no-match");
    common::daemon_start(state_dir.path());

    let _session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "show", "sess-does-not-exist"],
    );
    assert!(!out.status.success(), "a prefix matching nothing must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown session"),
        "expected an 'unknown session' message, got: {stderr}"
    );

    common::daemon_shutdown(state_dir.path());
}
