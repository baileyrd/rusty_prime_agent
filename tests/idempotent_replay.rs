//! Parity with a bounded first slice of idempotent replay protection --
//! see `protocol::Request::SessionPrompt::request_id`'s own doc comment
//! for the design (in-memory, per-worker dedup, not a durable
//! `daemon.md`-style `clientId + commandId` journal). Uses `EchoProvider`
//! throughout.

mod common;

#[test]
fn a_repeated_request_id_returns_the_same_reply_without_a_second_turn() {
    let state_dir = common::TempDir::new("idempotent-repeat");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--request-id",
            "req-1",
            "first",
            "attempt",
        ],
    );
    common::assert_success("session prompt --request-id (first)", &out);
    let first_reply = common::stdout_string(&out);
    assert!(
        first_reply.contains("echo: first attempt"),
        "got: {first_reply}"
    );

    // Same request id, deliberately different text -- a real retry would
    // resend the identical request, but sending different text here
    // proves the dedup check short-circuits before the provider is ever
    // asked again, rather than just happening to produce the same
    // answer.
    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--request-id",
            "req-1",
            "a",
            "totally",
            "different",
            "prompt",
        ],
    );
    common::assert_success("session prompt --request-id (retry)", &out);
    let retry_reply = common::stdout_string(&out);
    assert_eq!(
        retry_reply, first_reply,
        "a duplicate request id must return the exact cached reply"
    );

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=2"),
        "a duplicate request id must not have enqueued a second prompt, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn distinct_request_ids_both_send_real_prompts() {
    let state_dir = common::TempDir::new("idempotent-distinct");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--request-id",
            "req-a",
            "one",
        ],
    );
    common::assert_success("session prompt --request-id req-a", &out);
    let first = common::stdout_string(&out);
    assert!(first.contains("echo: one"), "got: {first}");

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--request-id",
            "req-b",
            "two",
        ],
    );
    common::assert_success("session prompt --request-id req-b", &out);
    let second = common::stdout_string(&out);
    assert!(second.contains("echo: two"), "got: {second}");

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=4"),
        "two distinct request ids should both be real prompts, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn prompting_without_a_request_id_is_unaffected() {
    let state_dir = common::TempDir::new("idempotent-no-request-id");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let ack = common::session_prompt(state_dir.path(), &session_id, "one");
    assert!(ack.contains("echo: one"), "got: {ack}");
    let ack = common::session_prompt(state_dir.path(), &session_id, "two");
    assert!(ack.contains("echo: two"), "got: {ack}");

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=4"),
        "no request id at all must behave exactly like before this feature existed, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// The session's transcript, read straight off disk. Direct rather than
/// via `session attach`: this only ever asks "did this text land", which
/// the file answers deterministically and a stream answers on a timeout.
fn transcript(state_dir: &std::path::Path, session_id: &str) -> String {
    std::fs::read_to_string(
        state_dir
            .join("sessions")
            .join(session_id)
            .join("transcript.jsonl"),
    )
    .unwrap_or_default()
}

/// The property the in-memory cache could not provide, and the reason
/// `COMPARISON.md` §5 flagged it: a client retrying after a dropped
/// connection is most likely retrying *because* the worker died, which
/// was precisely when the old cache was empty and the retry double-sent.
#[test]
fn a_completed_request_id_still_dedupes_after_the_worker_crashes() {
    let state_dir = common::TempDir::new("idempotent-durable");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let first = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--request-id",
            "req-durable",
            "the original prompt",
        ],
    );
    common::assert_success("first --request-id prompt", &first);

    // Kill the worker outright: no graceful shutdown, nothing flushed on
    // the way out beyond what was already durable.
    common::force_kill(common::worker_pid(state_dir.path(), &session_id));

    // Same id, different text -- so a re-execution would be visible as a
    // new turn rather than coincidentally identical output.
    let retry = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--request-id",
            "req-durable",
            "a completely different prompt",
        ],
    );
    common::assert_success("retry after crash", &retry);
    assert_eq!(
        common::stdout_string(&first).trim(),
        common::stdout_string(&retry).trim(),
        "a retry after a worker crash must replay the original reply, not run again"
    );

    assert!(
        !transcript(state_dir.path(), &session_id).contains("a completely different prompt"),
        "the retried text must never have reached the transcript"
    );

    common::daemon_shutdown(state_dir.path());
}

/// The other half: a request journaled as dispatched but never completed
/// is reported *uncertain* and never silently re-run -- parity with
/// `daemon.md`'s `R-PROTO-03`. Forged by writing the `begin` record a
/// worker killed mid-prompt would have left behind.
#[test]
fn a_dispatched_but_unfinished_request_is_reported_uncertain() {
    let state_dir = common::TempDir::new("idempotent-uncertain");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "something first");

    // Stop gracefully so the worker is not holding the session, then
    // plant exactly what a crash between dispatch and completion leaves.
    common::daemon_shutdown(state_dir.path());
    let journal = state_dir
        .path()
        .join("sessions")
        .join(&session_id)
        .join("request-journal.jsonl");
    std::fs::write(
        &journal,
        "{\"op\":\"begin\",\"v\":1,\"request_id\":\"req-interrupted\",\"at\":0}\n",
    )
    .unwrap();

    common::daemon_start(state_dir.path());
    let output = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--request-id",
            "req-interrupted",
            "retrying after the crash",
        ],
    );
    assert!(
        !output.status.success(),
        "an uncertain request must not report success; got: {}",
        common::stdout_string(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("never recorded") || stderr.contains("unknown"),
        "the caller should be told the outcome is unknown, not given a generic error: {stderr}"
    );

    assert!(
        !transcript(state_dir.path(), &session_id).contains("retrying after the crash"),
        "reporting uncertain must never re-execute the prompt"
    );

    common::daemon_shutdown(state_dir.path());
}
