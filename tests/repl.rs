//! Minimal, non-Python parity with `prime-agent`'s interactive TUI --
//! see `client::session_repl`'s own doc comment for exactly what it does
//! and doesn't cover. Uses `EchoProvider` throughout.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

/// Runs `session repl <id>` with `input` piped to stdin and closed
/// (EOF) once written -- exercises the REPL's own "loop until EOF"
/// termination path without needing `/exit`.
fn run_repl(state_dir: &std::path::Path, session_id: &str, input: &str) -> std::process::Output {
    let mut child = Command::new(common::bin())
        .args(["session", "repl", session_id])
        .env("RUSTY_PRIME_AGENT_HOME", state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn harness session repl");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write repl input");
    child.wait_with_output().expect("wait for repl to exit")
}

#[test]
fn repl_sends_each_line_as_a_prompt_and_exits_on_eof() {
    let state_dir = common::TempDir::new("repl-eof");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hello\nworld\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("echo: hello"), "got: {stdout}");
    assert!(stdout.contains("echo: world"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=4"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_queues_lines_typed_while_a_reply_is_still_in_flight() {
    // Parity with a bounded slice of `prime-agent`'s "steering vs.
    // follow-up queuing" -- see `PARITY.md`'s own "Interactive TUI:
    // steering vs. follow-up message queue" entry. All four lines are
    // already sitting in the pipe before the REPL even starts reading,
    // so the background reader can (and in practice reliably does) pull
    // ahead of the first prompt's own daemon round trip -- a real
    // socket hop plus transcript persistence -- while `session_repl`
    // used to be fully synchronous (read one line, `.await` its whole
    // reply, only then read the next) and could never even see a second
    // line until the first was completely done.
    let state_dir = common::TempDir::new("repl-followup-queue");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "one\ntwo\nthree\nfour\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for word in ["one", "two", "three", "four"] {
        assert!(stdout.contains(&format!("echo: {word}")), "got: {stdout}");
    }
    // The reader really does pull ahead of the in-flight prompt in
    // practice (confirmed manually and across repeated runs in this
    // project's own sandbox) -- at least one of the three follow-up
    // lines should have been queued rather than processed synchronously.
    assert!(
        stdout.contains("(queued -- will run once the current reply finishes)"),
        "expected at least one line to be queued while a reply was in flight, got: {stdout}"
    );
    // Replies land in the same order they were typed -- the queue is
    // FIFO, not reordered by whichever daemon round trip happens to
    // finish first.
    let one = stdout.find("echo: one").expect("echo: one");
    let two = stdout.find("echo: two").expect("echo: two");
    let three = stdout.find("echo: three").expect("echo: three");
    let four = stdout.find("echo: four").expect("echo: four");
    assert!(one < two && two < three && three < four, "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=8"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_name_command_renames_the_current_session() {
    let state_dir = common::TempDir::new("repl-name");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/name Renamed\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("renamed to Renamed"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("Renamed"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_refine_command_adds_a_harness_note() {
    let state_dir = common::TempDir::new("repl-refine");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hello\n/refine\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("refine: added memory note"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_session_command_lists_every_session() {
    let state_dir = common::TempDir::new("repl-session-list");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    let other_id = common::session_new(state_dir.path(), Some("other"));

    let out = run_repl(state_dir.path(), &session_id, "/session\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&session_id), "got: {stdout}");
    assert!(stdout.contains(&other_id), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_model_command_lists_configured_providers() {
    let state_dir = common::TempDir::new("repl-model-list");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/model\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ollama"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_reload_command_explains_context_files_are_already_fresh() {
    let state_dir = common::TempDir::new("repl-reload");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/reload\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("already re-read fresh on every turn"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_new_command_switches_this_repl_to_a_brand_new_session() {
    let state_dir = common::TempDir::new("repl-new");
    common::daemon_start(state_dir.path());
    let original_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &original_id, "/new second\nhello\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("switched to new session"), "got: {stdout}");
    assert!(stdout.contains("echo: hello"), "got: {stdout}");

    // "hello" must have landed on the *new* session, not the original one.
    let original_listing = common::run(state_dir.path(), &["session", "list"]);
    let original_stdout = common::stdout_string(&original_listing);
    let original_line = original_stdout
        .lines()
        .find(|l| l.starts_with(&original_id))
        .unwrap_or("");
    assert!(original_line.contains("turns=0"), "got: {original_stdout}");
    assert!(
        original_stdout.contains("second") && original_stdout.contains("turns=2"),
        "got: {original_stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_resume_command_switches_this_repl_to_an_existing_session() {
    let state_dir = common::TempDir::new("repl-resume");
    common::daemon_start(state_dir.path());
    let session_a = common::session_new(state_dir.path(), None);
    let session_b = common::session_new(state_dir.path(), None);

    // Two separate REPL runs, not one: `/resume` and the follow-up
    // prompt sent together in a single burst can race the same
    // "queued behind an in-flight prompt" window
    // `repl_new_refuses_to_switch_while_a_message_is_still_queued_
    // behind_it` deliberately exercises -- an earlier version of this
    // test hit that guard by accident when "hello a" was piped in the
    // same burst as `/resume`. Each run below starts idle, so its own
    // first line dispatches immediately with nothing queued behind it.
    let out = run_repl(state_dir.path(), &session_a, "hello a\n");
    common::assert_success("session repl", &out);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("echo: hello a"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let input = format!("/resume {session_b}\nhello b\n");
    let out = run_repl(state_dir.path(), &session_a, &input);
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("resumed session"), "got: {stdout}");
    assert!(stdout.contains("echo: hello b"), "got: {stdout}");

    let listing = common::run(state_dir.path(), &["session", "list"]);
    let listing_stdout = common::stdout_string(&listing);
    let a_line = listing_stdout
        .lines()
        .find(|l| l.starts_with(&session_a))
        .unwrap_or("");
    let b_line = listing_stdout
        .lines()
        .find(|l| l.starts_with(&session_b))
        .unwrap_or("");
    assert!(a_line.contains("turns=2"), "got: {listing_stdout}");
    assert!(b_line.contains("turns=2"), "got: {listing_stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_resume_command_with_an_unknown_id_reports_a_conflict_and_stays_put() {
    let state_dir = common::TempDir::new("repl-resume-unknown");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/resume sess-bogus\nhello\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("failed to resume"), "got: {stdout}");
    // Never switched -- "hello" still landed on the original session.
    assert!(stdout.contains("echo: hello"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_new_refuses_to_switch_while_a_message_is_still_queued_behind_it() {
    // "hello" starts a real in-flight prompt; "/new" and "world" both
    // arrive while it's still generating and get queued behind it (the
    // same reliable-in-practice race `repl_queues_lines_typed_while_a_
    // reply_is_still_in_flight` already exercises). Once "hello"
    // finishes, "/new" is dequeued and dispatched -- but "world" is
    // still sitting in the queue behind it, so `/new` must refuse to
    // switch sessions rather than silently strand "world".
    let state_dir = common::TempDir::new("repl-new-guard");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hello\n/new second\nworld\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("let those finish before switching sessions"),
        "got: {stdout}"
    );
    assert!(!stdout.contains("switched to new session"), "got: {stdout}");
    // Both prompts landed on the *original* session.
    assert!(stdout.contains("echo: hello"), "got: {stdout}");
    assert!(stdout.contains("echo: world"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=4"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_stops_at_an_explicit_exit_line_without_reaching_eof() {
    let state_dir = common::TempDir::new("repl-exit");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    // A line after `/exit` must never be sent.
    let out = run_repl(state_dir.path(), &session_id, "first\n/exit\nnever sent\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("echo: first"), "got: {stdout}");
    assert!(!stdout.contains("never sent"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

/// Parity with `prime-agent`'s `/heartbeat` -- see `client::session_repl`'s
/// own doc comment for why it's an immediate `send_prompt`, not routed
/// through `session schedule` the way its kernel-callable sibling
/// (`rlm_heartbeat()`) has to be.
#[test]
fn repl_heartbeat_with_no_active_goal_sends_nothing() {
    let state_dir = common::TempDir::new("repl-heartbeat-no-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/heartbeat\n/exit\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no active goal"),
        "expected an explanation, got: {stdout}"
    );

    // Nothing was sent -- the transcript is still empty.
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_heartbeat_with_an_active_goal_sends_a_continuation_prompt() {
    let state_dir = common::TempDir::new("repl-heartbeat-active-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "write", "a", "haiku"],
    );
    common::assert_success("session goal set", &out);

    let out = run_repl(state_dir.path(), &session_id, "/heartbeat\n/exit\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("echo: Continue working toward the goal: write a haiku"),
        "got: {stdout}"
    );

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=2"),
        "one heartbeat should produce one user+assistant pair, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Parity with `prime-agent /heartbeat every <duration>` -- unlike plain
/// `/heartbeat` above, this registers a real recurring `session
/// schedule` entry rather than sending anything immediately (see
/// `client::session_repl`'s own doc comment for why).
#[test]
fn repl_heartbeat_every_with_no_active_goal_sends_nothing() {
    let state_dir = common::TempDir::new("repl-heartbeat-every-no-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(
        state_dir.path(),
        &session_id,
        "/heartbeat every 10m\n/exit\n",
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no active goal"),
        "expected an explanation, got: {stdout}"
    );

    let listing = common::run(
        state_dir.path(),
        &["session", "schedule", "list", &session_id],
    );
    common::assert_success("session schedule list", &listing);
    assert_eq!(common::stdout_string(&listing), "no schedules");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_heartbeat_every_with_an_active_goal_creates_a_recurring_schedule() {
    let state_dir = common::TempDir::new("repl-heartbeat-every-active-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "write", "a", "haiku"],
    );
    common::assert_success("session goal set", &out);

    let out = run_repl(
        state_dir.path(),
        &session_id,
        "/heartbeat every 1h\n/exit\n",
    );
    common::assert_success("session repl", &out);
    let schedule_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!schedule_id.is_empty(), "expected a printed schedule id");

    // Nothing sent immediately -- it's a standing recurring schedule,
    // not a one-shot send.
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    let schedule_listing = common::run(
        state_dir.path(),
        &["session", "schedule", "list", &session_id],
    );
    common::assert_success("session schedule list", &schedule_listing);
    let schedule_listing = common::stdout_string(&schedule_listing);
    assert!(
        schedule_listing.contains(&schedule_id),
        "got: {schedule_listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_replays_prior_transcript_before_reading_new_input() {
    let state_dir = common::TempDir::new("repl-replay");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "already said");

    let out = run_repl(state_dir.path(), &session_id, "");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("already said") && stdout.contains("echo: already said"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Bounded parity with `prime-agent`'s TUI-side file-reference feature
/// -- see `session_repl`'s own `pending_file_content` doc comment.
#[test]
fn repl_file_command_queues_content_into_the_next_prompt() {
    let state_dir = common::TempDir::new("repl-file");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let file_path = state_dir.path().join("notes.txt");
    std::fs::write(&file_path, "the secret ingredient is basil").unwrap();

    let out = run_repl(
        state_dir.path(),
        &session_id,
        &format!("/file {}\nwhat's the secret?\n", file_path.display()),
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("queued"), "got: {stdout}");
    assert!(
        stdout.contains("the secret ingredient is basil") && stdout.contains("what's the secret?"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_file_command_on_a_missing_file_reports_an_error_and_sends_nothing() {
    let state_dir = common::TempDir::new("repl-file-missing");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/file does-not-exist.txt\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("failed to read"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

/// Bounded parity with `prime-agent`'s TUI-side `/fork` -- wires the
/// already-existing `session fork` client call into the REPL loop.
#[test]
fn repl_fork_command_creates_a_new_session_from_the_current_one() {
    let state_dir = common::TempDir::new("repl-fork");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    let mut child = Command::new(common::bin())
        .args(["--mode", "json", "session", "repl", &session_id])
        .env("RUSTY_PRIME_AGENT_HOME", state_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn harness session repl");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"/fork --name my-fork\n")
        .expect("write repl input");
    let out = child.wait_with_output().expect("wait for repl to exit");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let forked_line = stdout
        .lines()
        .find(|l| l.contains("\"type\":\"session_new\""))
        .unwrap_or_else(|| panic!("expected a session_new line, got: {stdout}"));
    let value: serde_json::Value = serde_json::from_str(forked_line).unwrap();
    let forked_id = value["session_id"].as_str().unwrap().to_string();
    assert_ne!(forked_id, session_id);

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&format!("{forked_id}\tactive\tmy-fork")),
        "got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Bounded parity with `prime-agent`'s TUI-side `/export` -- writes the
/// current transcript to a local file as pretty-printed JSON.
#[test]
fn repl_export_command_writes_the_transcript_to_a_file() {
    let state_dir = common::TempDir::new("repl-export");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    let export_path = state_dir.path().join("exported.json");
    let out = run_repl(
        state_dir.path(),
        &session_id,
        &format!("/export {}\n", export_path.display()),
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("exported 2 turn(s)"), "got: {stdout}");

    let exported = std::fs::read_to_string(&export_path).expect("exported file exists");
    let value: serde_json::Value = serde_json::from_str(&exported).expect("valid JSON");
    let entries = value.as_array().expect("exported transcript is an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["text"], "hello");
    assert_eq!(entries[1]["text"], "echo: hello");

    common::daemon_shutdown(state_dir.path());
}

/// Bounded parity with `prime-agent`'s TUI-side "`@` fuzzy search" file
/// reference -- see `client::expand_at_references`'s own doc comment.
/// Applies regardless of whether the line came from a real raw-mode
/// terminal or (as here) piped/cooked-mode input, so it's CI-safe: an
/// `@<path>` token in a submitted line is expanded into that file's own
/// content before the prompt is sent.
#[test]
fn repl_expands_an_at_reference_to_the_referenced_files_content() {
    let state_dir = common::TempDir::new("repl-at-reference");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let referenced = state_dir.path().join("notes.txt");
    std::fs::write(&referenced, "the referenced file's content").unwrap();
    let referenced_str = referenced.to_str().unwrap();

    let out = run_repl(
        state_dir.path(),
        &session_id,
        &format!("please read @{referenced_str}\n"),
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("the referenced file's content"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// An `@<path>` token whose path doesn't resolve to a real file is left
/// exactly as typed -- most likely an ordinary `@`-mention, not a
/// botched file reference.
#[test]
fn repl_leaves_an_at_token_untouched_when_the_path_does_not_exist() {
    let state_dir = common::TempDir::new("repl-at-reference-missing");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hey @nonexistent-file-xyz\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("echo: hey @nonexistent-file-xyz"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Parity with a bounded slice of `prime-agent`'s image-paste feature --
/// see `PARITY.md`'s own "Interactive TUI: image paste support" entry.
/// An `@<path>` token naming a real image file is left in the text as
/// written (unlike a text-file reference, which gets inlined) and its
/// content is instead carried out-of-band to the provider -- `EchoProvider`
/// mentions the image count it actually received, proving the image
/// reached `build_turns`/the provider call, not just that the REPL
/// recognized the extension.
#[test]
fn repl_at_image_reference_is_attached_out_of_band_not_inlined() {
    let state_dir = common::TempDir::new("repl-at-image");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let image = state_dir.path().join("photo.png");
    std::fs::write(&image, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
    let image_str = image.to_str().unwrap();

    let out = run_repl(
        state_dir.path(),
        &session_id,
        &format!("what's in @{image_str} please\n"),
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("echo: what's in @{image_str} please [+1 image]")),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// `/file <path>` on an image path queues it the same out-of-band way
/// `@<path>` does, instead of `/file`'s own ordinary text-inlining
/// behavior -- see `pending_images`'s own doc comment.
#[test]
fn repl_file_command_on_an_image_path_queues_it_as_an_image() {
    let state_dir = common::TempDir::new("repl-file-image");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let image = state_dir.path().join("shot.jpg");
    std::fs::write(&image, [1, 2, 3, 4]).unwrap();
    let image_str = image.to_str().unwrap();

    let out = run_repl(
        state_dir.path(),
        &session_id,
        &format!("/file {image_str}\ndescribe it\n"),
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("queued {image_str} as an image")),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("echo: describe it [+1 image]"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Parity with a bounded slice of `prime-agent`'s theme system -- see
/// `PARITY.md`'s "Themes: token spec + TUI renderer" entry. Every one
/// of this project's own tests pipes stdio, so `termctl::is_tty()`
/// reports `false` and no ANSI escape ever appears in captured output
/// (confirmed separately with a real pty pass, per that same entry) --
/// what *is* observable here, piped or not, is `settings.json`'s
/// `theme` field being read at all and a bad value degrading
/// gracefully rather than crashing the REPL.
#[test]
fn repl_falls_back_to_the_dark_theme_and_warns_when_the_configured_theme_path_is_unreadable() {
    let state_dir = common::TempDir::new("repl-theme-missing");
    std::fs::write(
        state_dir.path().join("settings.json"),
        r#"{"theme": "/no/such/theme.json"}"#,
    )
    .unwrap();
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hello\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("failed to load theme") && stdout.contains("falling back"),
        "got: {stdout}"
    );
    // The REPL still works normally after falling back.
    assert!(stdout.contains("echo: hello"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_falls_back_and_warns_when_a_custom_theme_file_is_missing_required_tokens() {
    let state_dir = common::TempDir::new("repl-theme-incomplete");
    let theme_path = state_dir.path().join("incomplete-theme.json");
    std::fs::write(
        &theme_path,
        r##"{"name": "incomplete", "colors": {"accent": "#ffffff"}}"##,
    )
    .unwrap();
    std::fs::write(
        state_dir.path().join("settings.json"),
        format!(r#"{{"theme": {:?}}}"#, theme_path.to_str().unwrap()),
    )
    .unwrap();
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hello\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("missing required color token"),
        "got: {stdout}"
    );
    assert!(stdout.contains("echo: hello"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

/// A valid theme selection (a built-in name) produces no warning at
/// all -- the REPL just proceeds normally, the same as with no `theme`
/// setting configured.
#[test]
fn repl_accepts_a_builtin_theme_name_with_no_warning() {
    let state_dir = common::TempDir::new("repl-theme-builtin");
    std::fs::write(
        state_dir.path().join("settings.json"),
        r#"{"theme": "light"}"#,
    )
    .unwrap();
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hello\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("failed to load theme"), "got: {stdout}");
    assert!(
        !stdout.contains("missing required color token"),
        "got: {stdout}"
    );
    assert!(stdout.contains("echo: hello"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}
