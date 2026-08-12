//! Bounded, non-Python parity with `prime-agent`'s prompt templates --
//! see `crate::prompt_template`'s own module doc comment for why real
//! `prime-agent` "skills" (Python packages) are a separate, out-of-scope
//! concept from these plain Markdown-plus-frontmatter templates.

mod common;

use std::path::Path;
use std::process::Command;

fn write_template(dir: &Path, filename: &str, content: &str) {
    std::fs::create_dir_all(dir).expect("create prompts dir");
    std::fs::write(dir.join(filename), content).expect("write template");
}

fn global_prompts_dir(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("prompts")
}

/// Runs the harness binary with `cwd` set explicitly, unlike
/// `common::run` (which inherits the test binary's own cwd) -- needed
/// for the project-local prompt-template discovery tier, which is
/// resolved against the current working directory rather than
/// `RUSTY_PRIME_AGENT_HOME`.
fn run_in(state_dir: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(common::bin())
        .args(args)
        .current_dir(cwd)
        .env("RUSTY_PRIME_AGENT_HOME", state_dir)
        .output()
        .expect("failed to run harness")
}

#[test]
fn prompt_template_list_reports_none_when_empty() {
    let state_dir = common::TempDir::new("prompt-template-empty");

    // No daemon needed at all -- list/render are pure local operations.
    let out = common::run(state_dir.path(), &["prompt-template", "list"]);
    common::assert_success("prompt-template list", &out);
    assert_eq!(common::stdout_string(&out), "no prompt templates");
}

#[test]
fn prompt_template_list_and_render_discover_a_global_template() {
    let state_dir = common::TempDir::new("prompt-template-global");
    write_template(
        &global_prompts_dir(state_dir.path()),
        "greet.md",
        "---\ndescription: Greets someone\nargument-hint: <name>\n---\nHello, $1! All args: $ARGUMENTS\n",
    );

    let out = common::run(state_dir.path(), &["prompt-template", "list"]);
    common::assert_success("prompt-template list", &out);
    assert_eq!(common::stdout_string(&out), "greet\tGreets someone");

    let out = common::run(
        state_dir.path(),
        &["prompt-template", "render", "greet", "World"],
    );
    common::assert_success("prompt-template render", &out);
    assert_eq!(common::stdout_string(&out), "Hello, World! All args: World");

    let out = common::run(
        state_dir.path(),
        &[
            "prompt-template",
            "render",
            "greet",
            "World",
            "and",
            "friends",
        ],
    );
    common::assert_success("prompt-template render (multiple args)", &out);
    assert_eq!(
        common::stdout_string(&out),
        "Hello, World! All args: World and friends"
    );
}

#[test]
fn project_local_template_overrides_a_global_template_of_the_same_name() {
    let state_dir = common::TempDir::new("prompt-template-override-state");
    let project_dir = common::TempDir::new("prompt-template-override-project");

    write_template(
        &global_prompts_dir(state_dir.path()),
        "note.md",
        "global body\n",
    );
    write_template(
        &project_dir
            .path()
            .join(".rusty-prime-agent")
            .join("prompts"),
        "note.md",
        "project body\n",
    );

    let out = run_in(
        state_dir.path(),
        project_dir.path(),
        &["prompt-template", "render", "note"],
    );
    common::assert_success("prompt-template render", &out);
    assert_eq!(common::stdout_string(&out), "project body");
}

#[test]
fn unknown_template_is_reported_as_a_conflict() {
    let state_dir = common::TempDir::new("prompt-template-unknown");

    let out = common::run(
        state_dir.path(),
        &["prompt-template", "render", "does-not-exist"],
    );
    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(3),
        "an unknown template is a conflict, not a usage error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no such prompt template: does-not-exist"),
        "got: {stderr}"
    );
}

#[test]
fn slice_argument_substitution_forms() {
    let state_dir = common::TempDir::new("prompt-template-slices");
    write_template(
        &global_prompts_dir(state_dir.path()),
        "slice.md",
        "from-2=${@:2} from-2-len-1=${@:2:1}\n",
    );

    let out = common::run(
        state_dir.path(),
        &["prompt-template", "render", "slice", "a", "b", "c"],
    );
    common::assert_success("prompt-template render", &out);
    assert_eq!(common::stdout_string(&out), "from-2=b c from-2-len-1=b");
}

#[test]
fn session_prompt_template_expands_and_sends_it_to_the_session() {
    let state_dir = common::TempDir::new("prompt-template-session");
    write_template(
        &global_prompts_dir(state_dir.path()),
        "greet.md",
        "Hello, $1!\n",
    );
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let out = common::run(
        state_dir.path(),
        &["session", "prompt-template", &session_id, "greet", "World"],
    );
    common::assert_success("session prompt-template", &out);
    assert!(
        common::stdout_string(&out).contains("echo: Hello, World!"),
        "got: {}",
        common::stdout_string(&out)
    );

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=2"),
        "one prompt-template turn should produce one user+assistant pair, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}
