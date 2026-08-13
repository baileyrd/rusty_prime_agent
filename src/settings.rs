//! `<state_dir>/settings.json` -- a persistent config file layer
//! underneath the CLI flags/env vars every tunable knob in this project
//! already has, parity with `prime-agent`'s own `settings.json`. Global
//! only, same cwd-visibility reason `skills::discover`/`session::
//! read_context_file` are: the worker process has no access to the CLI
//! caller's own cwd, so there's no project-local tier to discover here
//! either.
//!
//! Precedence, highest wins: an env var (where one exists) beats
//! `settings.json`, which beats the hardcoded default -- the same order
//! `session::compact_trigger_tokens`/`compact_keep_recent_tokens`
//! already established for their own env-var overrides, just with one
//! more fallback tier inserted underneath. Malformed or missing JSON
//! reads as "no settings" (every field `None`) rather than a hard
//! error -- the same permissive "an unparseable override falls back to
//! the default" stance the env-var overrides already take for a bad
//! value, not a new stance invented for this file.
//!
//! Field names are `snake_case`, matching this project's own JSON
//! convention throughout (`Request`/`Response`/`TranscriptEntry`, ...)
//! rather than copying `prime-agent`'s own camelCase verbatim -- a
//! deliberate consistency choice, not an oversight.
//!
//! The only fields today are the two compaction thresholds -- the only
//! tunables this project has that make sense as a persistent default
//! rather than a one-off override. `prime-agent`'s own `settings.json`
//! covers real estate this project has no equivalent knob for at all
//! (`enabled`/telemetry, retry policy, ...) and isn't attempted here;
//! more fields can be added the same way these two were, as this
//! project grows more tunables worth persisting.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub compact_trigger_tokens: Option<usize>,
    #[serde(default)]
    pub compact_keep_recent_tokens: Option<usize>,
}

/// Reads and parses `<state_root>/settings.json`. Never fails: a
/// missing file, an unreadable one, or one that isn't valid JSON (or
/// doesn't match [`Settings`]'s shape) all read the same as "no
/// settings" -- see this module's own doc comment for why that's a
/// deliberate stance, not a swallowed error.
pub fn load(state_root: &Path) -> Settings {
    std::fs::read_to_string(state_root.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-settings-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_settings_file_reads_as_all_none() {
        let root = temp_state_root("missing");
        assert_eq!(load(&root), Settings::default());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn malformed_json_reads_as_all_none() {
        let root = temp_state_root("malformed");
        std::fs::write(root.join("settings.json"), "{ not json").unwrap();
        assert_eq!(load(&root), Settings::default());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_both_compaction_fields() {
        let root = temp_state_root("both-fields");
        std::fs::write(
            root.join("settings.json"),
            r#"{"compact_trigger_tokens": 1234, "compact_keep_recent_tokens": 56}"#,
        )
        .unwrap();
        let settings = load(&root);
        assert_eq!(settings.compact_trigger_tokens, Some(1234));
        assert_eq!(settings.compact_keep_recent_tokens, Some(56));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_empty_object_reads_as_all_none() {
        let root = temp_state_root("empty-object");
        std::fs::write(root.join("settings.json"), "{}").unwrap();
        assert_eq!(load(&root), Settings::default());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unknown_fields_are_ignored_rather_than_rejected() {
        let root = temp_state_root("unknown-field");
        std::fs::write(
            root.join("settings.json"),
            r#"{"compact_trigger_tokens": 42, "some_future_setting": true}"#,
        )
        .unwrap();
        assert_eq!(load(&root).compact_trigger_tokens, Some(42));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
