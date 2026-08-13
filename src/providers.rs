//! `<state_dir>/providers.json` -- registers a custom/arbitrary
//! OpenAI-compatible provider (a self-hosted vLLM server, LM Studio, a
//! company-internal proxy, ...) that `rp_server`'s own compiled-in
//! `OPTIONAL_PROVIDERS` list has no entry for. Global only, same
//! cwd-visibility reason `settings.json`/`auth.json` are.
//!
//! Confirmed against `rusty_provider`'s real router source
//! (`crates/router/src/config.rs`/`lib.rs`), not guessed: a provider's
//! *name* is an arbitrary TOML table key (`HashMap<String,
//! ProviderConfig>`), routed on by splitting an incoming `"name/model"`
//! string on the first `/` -- `kind` is the only closed enum
//! (`openai`/`anthropic`/`gemini`), and any wire-compatible
//! self-hosted endpoint registers as `kind = "openai"`, exactly the
//! upstream project's own shipped `config.example.toml` pattern
//! (`groq`/`together`/`fireworks`, three arbitrary names all
//! `kind = "openai"`). So a registered custom provider needs nothing
//! from `provider.rs`/`cli.rs`/`client.rs`: `--model <name>/<model>`
//! was always an opaque, unvalidated string forwarded straight through
//! to `rp-server`, which is the only thing that ever rejects an unknown
//! name (a 4xx from `/v1/chat/completions`).
//!
//! `rp_server::all_providers` merges this file's entries with
//! `OPTIONAL_PROVIDERS` into one list `write_config`/`known_providers`/
//! `resolve_auth_env` all iterate instead of the bare const -- see that
//! function's own doc comment for the merge/precedence rules (a custom
//! entry reusing a reserved name is silently dropped, not an error, the
//! same permissive stance every config file in this project already
//! takes). A registered provider's API key is supplied the exact same
//! way a built-in one's is: an env var, or an `<state_dir>/auth.json`
//! entry keyed by the same provider name (`auth.rs` needed no changes
//! at all -- it was already a plain name-keyed map).

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

fn default_kind() -> String {
    "openai".to_string()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CustomProvider {
    pub base_url: String,
    /// One of `rp-server`'s closed `ProviderKind` variants
    /// (`"openai"`/`"anthropic"`/`"gemini"`) -- see this module's own
    /// doc comment. Defaults to `"openai"`, the only kind that makes
    /// sense for an arbitrary self-hosted "OpenAI-compatible" endpoint,
    /// the case this feature exists for; not validated here (`rp-server`
    /// itself rejects an unrecognized `kind` at its own config-parse
    /// time, not something this module pre-checks).
    #[serde(default = "default_kind")]
    pub kind: String,
}

pub type CustomProviders = HashMap<String, CustomProvider>;

/// Reads and parses `<state_root>/providers.json`. Never fails: a
/// missing file, an unreadable one, or one that isn't valid JSON all
/// read the same as "no custom providers registered" -- the same
/// permissive stance `settings::load`/`auth::load` already take.
pub fn load(state_root: &Path) -> CustomProviders {
    std::fs::read_to_string(state_root.join("providers.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-providers-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_providers_file_reads_as_no_entries() {
        let root = temp_state_root("missing");
        assert!(load(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn malformed_json_reads_as_no_entries() {
        let root = temp_state_root("malformed");
        std::fs::write(root.join("providers.json"), "{ not json").unwrap();
        assert!(load(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_a_custom_provider_and_defaults_kind_to_openai() {
        let root = temp_state_root("default-kind");
        std::fs::write(
            root.join("providers.json"),
            r#"{"my-vllm": {"base_url": "http://127.0.0.1:8000/v1"}}"#,
        )
        .unwrap();
        let providers = load(&root);
        let entry = providers.get("my-vllm").unwrap();
        assert_eq!(entry.base_url, "http://127.0.0.1:8000/v1");
        assert_eq!(entry.kind, "openai");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_explicit_kind_overrides_the_default() {
        let root = temp_state_root("explicit-kind");
        std::fs::write(
            root.join("providers.json"),
            r#"{"custom-anthropic-proxy": {"base_url": "https://proxy.example.com", "kind": "anthropic"}}"#,
        )
        .unwrap();
        let providers = load(&root);
        assert_eq!(
            providers.get("custom-anthropic-proxy").unwrap().kind,
            "anthropic"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
