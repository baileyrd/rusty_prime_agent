//! Parity with `prime-agent schedule <list|add|cancel>` -- see
//! `crate::schedule`'s own module doc comment for the background firing
//! loop this exercises. Uses `EchoProvider` (no real model needed); the
//! only real-time dependency is `daemon::SCHEDULE_POLL_INTERVAL` (5s),
//! which these tests wait out via `common::wait_until` rather than
//! sleeping a fixed amount.

mod common;

use std::time::Duration;

#[test]
fn a_one_shot_schedule_fires_exactly_once() {
    let state_dir = common::TempDir::new("schedule-once");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let at_ms = now_ms + 500;
    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "schedule",
            "add",
            &session_id,
            "--at",
            &at_ms.to_string(),
            "scheduled",
            "hello",
        ],
    );
    common::assert_success("session schedule add", &out);
    let schedule_id = common::stdout_string(&out);
    assert!(!schedule_id.is_empty(), "schedule add must print an id");

    // Fires within one SCHEDULE_POLL_INTERVAL (5s) of its due time --
    // generous upper bound for CI variance.
    assert!(
        common::wait_until(
            || {
                let listing = common::session_list(state_dir.path());
                listing.contains(&session_id) && listing.contains("turns=2")
            },
            Duration::from_secs(15)
        ),
        "scheduled prompt should have fired and produced a reply within 15s"
    );

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=2"),
        "exactly one user+assistant pair from the one fire, got: {listing}"
    );

    // A one-shot entry removes itself after firing -- list must be empty.
    let out = common::run(
        state_dir.path(),
        &["session", "schedule", "list", &session_id],
    );
    common::assert_success("session schedule list", &out);
    assert!(
        common::stdout_string(&out).contains("no schedules"),
        "a fired one-shot schedule should no longer be listed, got: {}",
        common::stdout_string(&out)
    );

    // It doesn't fire again on a later poll -- wait past another full
    // interval and confirm the turn count is still exactly 2.
    std::thread::sleep(Duration::from_secs(6));
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=2"),
        "a one-shot schedule must not fire twice, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn canceling_a_schedule_before_it_fires_prevents_it() {
    let state_dir = common::TempDir::new("schedule-cancel");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let at_ms = now_ms + 30_000; // Far enough out that this test's own cancel wins the race.
    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "schedule",
            "add",
            &session_id,
            "--at",
            &at_ms.to_string(),
            "should",
            "never",
            "fire",
        ],
    );
    common::assert_success("session schedule add", &out);
    let schedule_id = common::stdout_string(&out);

    let out = common::run(
        state_dir.path(),
        &["session", "schedule", "cancel", &session_id, &schedule_id],
    );
    common::assert_success("session schedule cancel", &out);
    assert_eq!(common::stdout_string(&out), "schedule canceled");

    // Canceling something already gone is reported, not an error.
    let out = common::run(
        state_dir.path(),
        &["session", "schedule", "cancel", &session_id, &schedule_id],
    );
    common::assert_success("session schedule cancel (again)", &out);
    assert_eq!(common::stdout_string(&out), "no such schedule");

    let out = common::run(
        state_dir.path(),
        &["session", "schedule", "list", &session_id],
    );
    common::assert_success("session schedule list", &out);
    assert!(common::stdout_string(&out).contains("no schedules"));

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn an_every_schedule_fires_more_than_once() {
    let state_dir = common::TempDir::new("schedule-every");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "schedule",
            "add",
            &session_id,
            "--every",
            "1s",
            "tick",
        ],
    );
    common::assert_success("session schedule add", &out);

    // Two SCHEDULE_POLL_INTERVAL (5s) cycles is enough for a 1s-interval
    // recurring schedule to have fired at least twice.
    assert!(
        common::wait_until(
            || {
                let listing = common::session_list(state_dir.path());
                listing.contains(&session_id)
                    && listing
                        .split('\t')
                        .find_map(|f| f.strip_prefix("turns="))
                        .and_then(|n| n.parse::<u64>().ok())
                        .is_some_and(|turns| turns >= 4)
            },
            Duration::from_secs(15)
        ),
        "a recurring schedule should fire more than once within 15s"
    );

    common::daemon_shutdown(state_dir.path());
}
