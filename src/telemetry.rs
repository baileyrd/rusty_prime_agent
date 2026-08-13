//! Opt-in, local-only telemetry -- bounded parity with a slice of
//! `prime-agent`'s own `telemetry.*` settings key family
//! (`CLAIMS_AUDIT.md`'s own `settings.md` audit previously confirmed
//! `telemetry.*` entirely absent from this project). `prime-agent`'s
//! real telemetry presumably configures *where* usage events get sent --
//! some analytics collector this project has no equivalent of, the same
//! "nothing on the other end" shape `/login`'s missing OAuth backend and
//! `self_update`'s missing release channel both already have (see
//! `PARITY.md`'s own entries for both). This module doesn't invent a
//! fake destination to send anything to; it does the one honest thing
//! that's actually implementable without one: record events to a local
//! file only, when explicitly turned on.
//!
//! **Opt-in, not opt-out**: [`settings::Settings::telemetry_enabled`]
//! defaults to `None`, which [`record`] treats identically to
//! `Some(false)` -- no event is ever written unless a caller has
//! explicitly set `"telemetry_enabled": true` in `settings.json`. This
//! is a deliberate choice for this project's own stub, not a claim about
//! matching whatever `prime-agent`'s own real default is (this project
//! has never verified that against `prime-agent`'s real source).
//!
//! **"Local-only" is structural, not configured**: there is no HTTP
//! client, no collector URL, no network call anywhere in this module --
//! confirmed by its own short size, not merely asserted. Enabling
//! telemetry can only ever grow `<state_root>/telemetry.jsonl`; nothing
//! in this project ever reads that file back out or transmits it
//! anywhere. A caller who wants the data elsewhere copies the file
//! themselves.
//!
//! Two event kinds are recorded today, both from real, load-bearing call
//! sites (not fabricated for this feature): `session_created`
//! ([`AgentSession::create`](crate::session::AgentSession::create), one
//! event per new session) and `prompt`
//! ([`AgentSession::prompt_with_images`](crate::session::AgentSession::prompt_with_images),
//! one event per completed turn, recording whether it succeeded and how
//! many tool-call rounds it took). Deliberately not attempted: any
//! event for `session recover`/`session stop`/tool-level granularity,
//! an anonymous installation ID, or any aggregation/summary view --
//! `telemetry.jsonl` is raw, unprocessed, append-only, same shape as
//! `<session>/transcript.jsonl` already has elsewhere in this project.
//! Failures writing the file are swallowed rather than propagated: a
//! telemetry write is never allowed to turn an otherwise-successful
//! session operation into a failure, the same "don't let a nicety break
//! the real thing" stance `PARITY.md`'s own usage/token-accounting entry
//! already takes ("a malformed sub-field defaulting to `0` rather than
//! failing an otherwise-successful reply over a telemetry nicety").

use std::io::Write;
use std::path::Path;

use serde::Serialize;

/// The file every recorded event is appended to, one JSON object per
/// line -- global, same as every other top-level file under
/// `state_root` (`auth.json`, `settings.json`, `providers.json`).
const TELEMETRY_FILE: &str = "telemetry.jsonl";

#[derive(Serialize)]
struct Event<'a> {
    ts_ms: u64,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(flatten)]
    data: serde_json::Value,
}

/// Records one event to `<state_root>/telemetry.jsonl` if and only if
/// `settings.json` has `"telemetry_enabled": true` -- a no-op otherwise,
/// checked fresh on every call (no caching), same as every other
/// `settings::load` consumer in this project. `data` is any extra
/// per-event JSON object (e.g. `{"tool_rounds": 2}`); pass
/// `serde_json::json!({})` for an event with nothing extra to record.
///
/// Never fails outward: a missing/unwritable state directory, a
/// serialization error, or any other I/O failure is silently swallowed.
/// See this module's own doc comment for why that's a deliberate choice
/// rather than an oversight.
pub fn record(state_root: &Path, event: &str, session_id: Option<&str>, data: serde_json::Value) {
    if !crate::settings::load(state_root)
        .telemetry_enabled
        .unwrap_or(false)
    {
        return;
    }
    let entry = Event {
        ts_ms: crate::paths::now_ms(),
        event,
        session_id,
        data,
    };
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    let path = state_root.join(TELEMETRY_FILE);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-telemetry-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn record_writes_nothing_when_telemetry_is_not_configured() {
        let root = temp_state_root("disabled-default");
        record(&root, "session_created", Some("s1"), serde_json::json!({}));
        assert!(!root.join(TELEMETRY_FILE).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn record_writes_nothing_when_telemetry_is_explicitly_disabled() {
        let root = temp_state_root("disabled-explicit");
        std::fs::write(
            root.join("settings.json"),
            r#"{"telemetry_enabled": false}"#,
        )
        .unwrap();
        record(&root, "session_created", Some("s1"), serde_json::json!({}));
        assert!(!root.join(TELEMETRY_FILE).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn record_appends_one_json_line_per_event_when_enabled() {
        let root = temp_state_root("enabled");
        std::fs::write(root.join("settings.json"), r#"{"telemetry_enabled": true}"#).unwrap();

        record(&root, "session_created", Some("s1"), serde_json::json!({}));
        record(
            &root,
            "prompt",
            Some("s1"),
            serde_json::json!({"tool_rounds": 2, "ok": true}),
        );

        let contents = std::fs::read_to_string(root.join(TELEMETRY_FILE)).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "session_created");
        assert_eq!(first["session_id"], "s1");
        assert!(first["ts_ms"].is_u64());

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "prompt");
        assert_eq!(second["tool_rounds"], 2);
        assert_eq!(second["ok"], true);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn record_omits_session_id_when_none() {
        let root = temp_state_root("no-session-id");
        std::fs::write(root.join("settings.json"), r#"{"telemetry_enabled": true}"#).unwrap();

        record(&root, "session_created", None, serde_json::json!({}));

        let contents = std::fs::read_to_string(root.join(TELEMETRY_FILE)).unwrap();
        let value: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert!(value.get("session_id").is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
