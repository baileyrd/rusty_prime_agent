//! Proves the embeddable-SDK layer `lib.rs`'s own doc comment describes:
//! `rusty_prime_agent::session::AgentSession` can be constructed and
//! driven directly, in-process, by an external crate -- no daemon, no
//! worker process, no Unix/AF_UNIX socket anywhere in the loop. This is
//! the first test in this project not shaped as `std::process::Command::
//! new(common::bin())` (see `tests/common/mod.rs`'s own doc comment,
//! written back when the package had no `[lib]` target for a test like
//! this to link against at all).
//!
//! `tests/dispatch_one_shot.rs` covers this project's *other* embedding
//! layer -- driving an already-running daemon over its socket via the
//! same crate-root-exported primitive -- since that one genuinely needs
//! a real daemon process, unlike this file.

use rusty_prime_agent::provider::EchoProvider;
use rusty_prime_agent::session::{AgentSession, NewSessionMeta};
use rusty_prime_agent::tool_runtime::NoopToolRuntime;

fn temp_state_root(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpa-embedded-session-test-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[rusty_tokio::test]
async fn an_embedded_session_can_be_created_and_prompted_with_no_daemon_involved() {
    let root = temp_state_root("create-and-prompt");
    let mut session = AgentSession::create(
        &root,
        "embedded-sess-1".to_string(),
        NewSessionMeta::default(),
        Box::new(EchoProvider),
        Box::new(NoopToolRuntime),
    )
    .await
    .expect("embedded AgentSession::create should succeed with no daemon running");

    let entry = session
        .prompt("hello from an embedding host program".to_string())
        .await
        .expect("prompting an embedded session should succeed");
    assert_eq!(entry.text, "echo: hello from an embedding host program");

    // Real durable state under the caller-supplied state_root, the same
    // "not a pure in-memory session" property `lib.rs`'s own doc comment
    // calls out -- an embedder gets real persistence for free, without
    // ever starting a daemon to get it.
    assert!(root
        .join("sessions")
        .join("embedded-sess-1")
        .join("transcript.jsonl")
        .exists());

    std::fs::remove_dir_all(&root).unwrap();
}

#[rusty_tokio::test]
async fn an_embedded_session_survives_recover_from_the_same_state_root() {
    let root = temp_state_root("recover");
    {
        let mut session = AgentSession::create(
            &root,
            "embedded-sess-2".to_string(),
            NewSessionMeta::default(),
            Box::new(EchoProvider),
            Box::new(NoopToolRuntime),
        )
        .await
        .unwrap();
        session.prompt("first turn".to_string()).await.unwrap();
    }

    // A fresh `AgentSession` value, reconstructed purely from what
    // `create`'s first instance persisted to disk -- proving an
    // embedding host can drop and later reload a session the same way
    // this project's own worker process does after a crash, without any
    // in-memory state surviving between the two.
    let mut recovered = AgentSession::recover(
        &root,
        "embedded-sess-2",
        Box::new(EchoProvider),
        Box::new(NoopToolRuntime),
    )
    .await
    .expect("recovering an embedded session from disk should succeed");
    let entry = recovered.prompt("second turn".to_string()).await.unwrap();
    assert_eq!(entry.text, "echo: second turn");

    std::fs::remove_dir_all(&root).unwrap();
}
