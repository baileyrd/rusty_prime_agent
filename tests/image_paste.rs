//! `harness session prompt <id> --image <path> [text...]` -- parity with
//! a bounded slice of `prime-agent`'s image-paste feature, see
//! `PARITY.md`'s own "Interactive TUI: image paste support" entry. See
//! `tests/repl.rs` for the REPL-side `/file`/`@`-reference coverage of
//! the same underlying mechanism.

mod common;

#[test]
fn session_prompt_with_an_image_attaches_it_out_of_band() {
    let state_dir = common::TempDir::new("prompt-image");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let image = state_dir.path().join("photo.png");
    std::fs::write(&image, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
    let image_str = image.to_str().unwrap();

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--image",
            image_str,
            "describe this",
        ],
    );
    common::assert_success("session prompt --image", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("echo: describe this [+1 image]"),
        "got: {stdout}"
    );

    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &session_id],
        2,
        std::time::Duration::from_secs(5),
    );
    let snapshot = lines.join("\n");
    assert!(
        snapshot.contains("data:image/png;base64,"),
        "expected the user entry to carry the base64 image, got: {snapshot}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_prompt_with_multiple_images_attaches_all_of_them() {
    let state_dir = common::TempDir::new("prompt-multi-image");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let image_a = state_dir.path().join("a.png");
    let image_b = state_dir.path().join("b.jpg");
    std::fs::write(&image_a, [1, 2, 3]).unwrap();
    std::fs::write(&image_b, [4, 5, 6]).unwrap();

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--image",
            image_a.to_str().unwrap(),
            "--image",
            image_b.to_str().unwrap(),
            "compare these",
        ],
    );
    common::assert_success("session prompt --image (x2)", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("echo: compare these [+2 images]"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_prompt_with_no_text_and_only_an_image_is_accepted() {
    let state_dir = common::TempDir::new("prompt-image-only");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let image = state_dir.path().join("only.png");
    std::fs::write(&image, [1, 2, 3]).unwrap();

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--image",
            image.to_str().unwrap(),
        ],
    );
    common::assert_success("session prompt --image with no text", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("echo:  [+1 image]"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_prompt_with_an_unreadable_image_path_fails_loudly() {
    let state_dir = common::TempDir::new("prompt-image-missing");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--image",
            "/nonexistent/path/photo.png",
            "hello",
        ],
    );
    assert!(
        !out.status.success(),
        "expected a failure exit code, got success: {}",
        common::stdout_string(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("/nonexistent/path/photo.png"),
        "got: {stderr}"
    );

    // A failed prompt must not have appended anything -- the session
    // should still show zero turns.
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_prompt_with_a_non_image_extension_fails_loudly() {
    let state_dir = common::TempDir::new("prompt-image-wrong-ext");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let not_an_image = state_dir.path().join("notes.txt");
    std::fs::write(&not_an_image, "just text").unwrap();

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--image",
            not_an_image.to_str().unwrap(),
            "hello",
        ],
    );
    assert!(
        !out.status.success(),
        "expected a failure exit code, got success: {}",
        common::stdout_string(&out)
    );

    common::daemon_shutdown(state_dir.path());
}
