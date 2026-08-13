//! Proves this project's *other* embedding layer: `rusty_prime_agent::
//! dispatch_one_shot`, re-exported at the crate root, sends a real typed
//! `Request` to an already-running daemon and hands back a typed
//! `Response` -- no `println!`/stdout parsing anywhere in the loop, the
//! property that makes it usable as a library call rather than a CLI
//! rendering routine. `tests/embedded_session.rs` covers the other
//! ("no daemon at all") layer.
//!
//! Still needs a real daemon process, so `mod common` (spawning the
//! compiled `harness` binary) does the daemon lifecycle -- only the
//! actual `Request`/`Response` round trip goes through the library call
//! this test exists to prove, not a subprocess.

mod common;

use rusty_prime_agent::protocol::{Request, Response};

#[rusty_tokio::test]
async fn dispatch_one_shot_creates_a_session_against_a_real_running_daemon() {
    let state_dir = common::TempDir::new("dispatch-one-shot");
    common::daemon_start(state_dir.path());

    let response = rusty_prime_agent::dispatch_one_shot(
        state_dir.path(),
        Request::SessionNew {
            name: Some("embedded-via-dispatch".to_string()),
            model: None,
            goal: None,
            parent_id: None,
            thinking: None,
            tools: None,
            runtime: None,
        },
    )
    .await
    .expect("dispatch_one_shot should reach the running daemon");

    let session_id = match response {
        Response::SessionNew { session_id } => session_id,
        other => panic!("expected Response::SessionNew, got {other:?}"),
    };
    assert!(!session_id.is_empty());

    let listing = rusty_prime_agent::dispatch_one_shot(state_dir.path(), Request::SessionList)
        .await
        .expect("dispatch_one_shot should list sessions");
    match listing {
        Response::SessionList { sessions } => {
            assert!(
                sessions.iter().any(|s| s.session_id == session_id),
                "expected {session_id} in {sessions:?}"
            );
        }
        other => panic!("expected Response::SessionList, got {other:?}"),
    }

    common::daemon_shutdown(state_dir.path());
}
