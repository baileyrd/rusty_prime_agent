//! Bounded parity with `prime-agent`'s own extension system -- see
//! `PARITY.md`'s "Extensions: manifest/registration format + event-hook
//! system" entry for the full story of what's here and what deliberately
//! isn't. `prime-agent`'s own `docs/extensions.md` documents a default-
//! export factory receiving an `ExtensionAPI`
//! (`pi.registerTool()`/`registerCommand()`/`registerShortcut()`/
//! `registerProvider()`/`on(event, handler)` across roughly 25 named
//! lifecycle events). This project's own user-facing extensibility has
//! always been Python-via-the-kernel, not JavaScript (see `skills.rs`'s
//! own doc comment) -- extensions here follow that same shape rather
//! than copying `prime-agent`'s literal tech choice: a discovered
//! directory is an importable Python package, loaded into a live
//! `--runtime ipython` kernel, that defines a `register(pi)` function
//! called once at session-bootstrap time with a small `pi` object
//! (`worker::bootstrap_kernel`'s own doc comment has the exact Python
//! source). Only two of `prime-agent`'s ~25 named lifecycle events are
//! implemented -- `on("pre_tool_call", handler)` (a real, blocking hook:
//! returning a non-empty string from any registered handler skips the
//! real tool call and substitutes that string as the result, the same
//! "extension can veto" semantics `extensions.md` describes for
//! interception) and `register_command(name, handler, description="")`
//! (a REPL-invocable custom command, `/name <args>`, dispatched to
//! `AgentSession::invoke_extension_command`) -- matching `PARITY.md`'s
//! own scoping of "one blocking pre-tool-call hook plus one custom-
//! command registration point" as the tractable first slice. Every
//! other named event (`registerTool`/`registerShortcut`/
//! `registerProvider`, the ~23 other lifecycle events, a dialog-based
//! user-interaction surface, custom rendering) stays unimplemented,
//! stated honestly rather than silently absorbed into "extensions now
//! supported."
//!
//! A discovered extension is a directory under
//! [`paths::global_extensions_dir`]:
//!
//! ```text
//! <state_dir>/extensions/
//!   greeter/
//!     EXTENSION.md    <- frontmatter: description: ...  (human-facing)
//!     __init__.py      <- the actual importable Python package,
//!                          defining `def register(pi): ...`
//! ```
//!
//! Same discovery shape as [`crate::skills::discover`] (`EXTENSION.md`'s
//! presence is what makes a directory count as an extension at all; the
//! directory name doubles as the Python package name); see that
//! module's own doc comment for the reasoning behind the global-only
//! tier and the "let `import` fail naturally on a malformed package"
//! stance, both identical here.

use std::path::Path;

use crate::error::{Context, HarnessError, Result};
use crate::paths;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Extension {
    pub name: String,
    pub description: Option<String>,
}

/// Scans [`paths::global_extensions_dir`] for subdirectories containing
/// an `EXTENSION.md`, sorted by name -- the exact same shape
/// [`crate::skills::discover`] already uses for skills, down to the
/// "missing directory contributes nothing rather than erroring" and
/// "a directory without the manifest file is silently skipped, not a
/// stray-file error" stances.
pub fn discover(state_root: &Path) -> Result<Vec<Extension>> {
    let dir = paths::global_extensions_dir(state_root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HarnessError::io(Context::Runtime, Some(dir), e)),
    };

    let mut extensions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| HarnessError::io(Context::Runtime, Some(dir.clone()), e))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let manifest_path = path.join("EXTENSION.md");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(HarnessError::io(Context::Runtime, Some(manifest_path), e)),
        };
        let (mut fields, _body) = crate::frontmatter::parse(&content);
        extensions.push(Extension {
            name: name.to_string(),
            description: fields.remove("description"),
        });
    }
    extensions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(extensions)
}

/// What actually got registered by every discovered extension's own
/// `register(pi)` call during `worker::bootstrap_kernel` -- queried back
/// out of the live kernel once (a single marker-prefixed `print`, the
/// same technique `session::HEARTBEAT_MARKER` already established) right
/// after loading, and installed onto the new `AgentSession` via
/// `AgentSession::install_extension_registry`. `commands` maps a
/// registered command name to its (optional) description; `handler`
/// values themselves stay in the kernel's own memory -- a Python
/// function object can't cross the wire, so Rust only ever tracks
/// *that* something is registered, dispatching back into the kernel by
/// name to actually run it (see `AgentSession::invoke_extension_command`/
/// `run_pre_tool_call_hooks`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ExtensionRegistry {
    pub commands: std::collections::HashMap<String, String>,
    pub has_pre_tool_call_hook: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpa-extensions-test-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_extension(state_root: &Path, name: &str, manifest: &str) {
        let dir = paths::global_extensions_dir(state_root).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("EXTENSION.md"))
            .unwrap()
            .write_all(manifest.as_bytes())
            .unwrap();
    }

    #[test]
    fn missing_extensions_dir_returns_empty() {
        let root = temp_state_root("missing");
        assert_eq!(discover(&root).unwrap(), vec![]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_directory_without_extension_md_is_skipped() {
        let root = temp_state_root("no-manifest");
        std::fs::create_dir_all(paths::global_extensions_dir(&root).join("not_an_extension"))
            .unwrap();
        assert_eq!(discover(&root).unwrap(), vec![]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discovers_and_parses_a_real_extension() {
        let root = temp_state_root("real");
        write_extension(
            &root,
            "greeter",
            "---\ndescription: adds a /hello command\n---\nLonger docs go here.\n",
        );
        let extensions = discover(&root).unwrap();
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].name, "greeter");
        assert_eq!(
            extensions[0].description.as_deref(),
            Some("adds a /hello command")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_extension_with_no_description_field_still_discovers_with_none() {
        let root = temp_state_root("no-desc");
        write_extension(&root, "bare", "no frontmatter at all\n");
        let extensions = discover(&root).unwrap();
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].description, None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn results_are_sorted_by_name() {
        let root = temp_state_root("sorted");
        write_extension(&root, "zeta", "---\ndescription: z\n---\n");
        write_extension(&root, "alpha", "---\ndescription: a\n---\n");
        let extensions = discover(&root).unwrap();
        let names: Vec<&str> = extensions.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
