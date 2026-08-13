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
//! Fields today: the two compaction thresholds, `compaction_enabled`,
//! `theme` (see `theme`'s own module doc comment), and
//! `telemetry_enabled` (see `telemetry`'s own module doc comment).
//! `prime-agent`'s own `settings.json` still covers real estate this
//! project has no equivalent knob for at all (retry policy,
//! `branchSummary.*`, ...) and isn't attempted here; more fields can be
//! added the same way these were, as this project grows more tunables
//! worth persisting.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub compact_trigger_tokens: Option<usize>,
    #[serde(default)]
    pub compact_keep_recent_tokens: Option<usize>,
    /// Parity with `prime-agent`'s own `compaction.enabled` settings
    /// key. `None`/`Some(true)` (the default) leaves automatic
    /// compaction on; only an explicit `Some(false)` suppresses the
    /// trigger `AgentSession::maybe_compact` checks every prompt round.
    /// Manual compaction (`session compact`/`/compact`) is unaffected --
    /// a caller explicitly asking for it should still get it regardless
    /// of whether the automatic trigger is disabled. Before this field
    /// existed, the only way to suppress automatic compaction at all was
    /// to never configure a real `--model` in the first place.
    #[serde(default)]
    pub compaction_enabled: Option<bool>,
    /// `"dark"`/`"light"` (this project's two built-in themes) or a
    /// path to a custom theme JSON file -- parity with `prime-agent`'s
    /// own `theme` settings key. See `theme::resolve`'s own doc
    /// comment for exactly how this is interpreted, and `PARITY.md`'s
    /// "Themes: token spec + TUI renderer" entry for what's rendered
    /// with it and what isn't. Read once at `session repl` startup, the
    /// same "no live reload" stance the two fields above already have.
    #[serde(default)]
    pub theme: Option<String>,
    /// Opt-in switch for the local-only telemetry stub -- parity with a
    /// bounded slice of `prime-agent`'s own `telemetry.*` settings key
    /// family. `None`/`Some(false)` (the default -- unset reads as off,
    /// same "absent means not configured" stance every other `Option`
    /// field here already has) means `telemetry::record` never writes
    /// anything; only an explicit `Some(true)` turns it on. See
    /// `telemetry`'s own module doc comment for what "local-only" means
    /// structurally, not just as a configuration choice.
    #[serde(default)]
    pub telemetry_enabled: Option<bool>,
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
    fn parses_the_theme_field() {
        let root = temp_state_root("theme-field");
        std::fs::write(root.join("settings.json"), r#"{"theme": "light"}"#).unwrap();
        assert_eq!(load(&root).theme, Some("light".to_string()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_the_telemetry_enabled_field() {
        let root = temp_state_root("telemetry-field");
        std::fs::write(root.join("settings.json"), r#"{"telemetry_enabled": true}"#).unwrap();
        assert_eq!(load(&root).telemetry_enabled, Some(true));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn telemetry_enabled_defaults_to_none_when_absent() {
        let root = temp_state_root("telemetry-field-absent");
        std::fs::write(root.join("settings.json"), "{}").unwrap();
        assert_eq!(load(&root).telemetry_enabled, None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_the_compaction_enabled_field() {
        let root = temp_state_root("compaction-enabled-field");
        std::fs::write(
            root.join("settings.json"),
            r#"{"compaction_enabled": false}"#,
        )
        .unwrap();
        assert_eq!(load(&root).compaction_enabled, Some(false));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn compaction_enabled_defaults_to_none_when_absent() {
        let root = temp_state_root("compaction-enabled-field-absent");
        std::fs::write(root.join("settings.json"), "{}").unwrap();
        assert_eq!(load(&root).compaction_enabled, None);
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
