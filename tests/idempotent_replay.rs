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
