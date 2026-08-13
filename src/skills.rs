//! Real, importable Python packages for `session new --runtime ipython`
//! (parity with `prime-agent`'s "skills" -- see `prompt_template.rs`'s
//! own doc comment for why plain-text prompt templates are a *separate*
//! thing from this: prompt templates are the non-Python half of
//! `prime-agent skills.md`'s surface; this module is the Python-package
//! half, tractable now that `tool_runtime::ToolRuntime` is backed by a
//! real kernel (`ipython_runtime::IpythonKernelRuntime`).
//!
//! A skill is a directory under [`paths::global_skills_dir`]:
//!
//! ```text
//! <state_dir>/skills/
//!   weather/
//!     SKILL.md       <- frontmatter: description/license/compatibility/
//!                        disable-model-invocation (model-facing/informational)
//!     __init__.py     <- the actual importable Python package
//!     fetch.py
//! ```
//!
//! `SKILL.md`'s presence is what makes a directory count as a skill at
//! all -- this module never inspects the Python files themselves (a
//! missing/malformed `__init__.py` surfaces as an ordinary `ImportError`
//! the *model* sees when it actually tries `import weather`, the same
//! "let the callee reject malformed input" philosophy `tools::execute`'s
//! own doc comment already establishes, not something worth validating
//! twice). The skill's own directory name is its Python package name
//! (`import weather`, not whatever `SKILL.md`'s own optional `name:`
//! field might separately claim) -- the two have to match for `import`
//! to work at all, so a mismatched `name:` is a lenient warning
//! ([`Skill::warnings`]), not a second "display name" this module
//! reconciles or prefers.
//!
//! Global tier only, deliberately: unlike `prompt_template::discover`
//! (always called client-side, where the real CLI caller's cwd is
//! meaningful), the one place skill *loading* actually needs to run
//! (`worker::run`, so it can call `tool_runtime::ToolRuntime::execute` on
//! a live kernel connection) has no access to that cwd -- a worker
//! process runs with the daemon's own cwd, not the session-creation
//! caller's. Doing project-local skills correctly would mean threading
//! the CLI's cwd through `Request::SessionNew`/`WorkerArgs` on every
//! worker respawn (`thinking`'s "always supplied" pattern, not `goal`'s
//! "New-only" one) -- real, but separate scope, not attempted here.

use std::path::Path;

use crate::error::{Context, HarnessError, Result};
use crate::paths;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    /// `SKILL.md`'s own `license:` frontmatter field, informational
    /// only -- nothing in this project reads or enforces it.
    pub license: Option<String>,
    /// `SKILL.md`'s own `compatibility:` frontmatter field, informational
    /// only -- nothing in this project checks it against anything (no
    /// real "compatible with which harness version" concept exists
    /// here); surfaced purely so `harness skill list` shows what the
    /// skill author claimed.
    pub compatibility: Option<String>,
    /// `SKILL.md`'s own `disable-model-invocation:` frontmatter field
    /// (`"true"`, case-insensitive; anything else, including absent, is
    /// `false`). `true` means `AgentSession::
    /// execute_python_tool_def_with_skills` never advertises this skill
    /// to the model at all -- a human can still reach it explicitly via
    /// `session_repl`'s `/skill:<name>` command (see that command's own
    /// doc comment), the same "the model won't stumble onto it, but a
    /// caller who asks for it by name still can" split
    /// `disable-model-invocation` is named for upstream.
    pub disable_model_invocation: bool,
    /// Lenient, non-fatal validation notes -- currently just a `name:`
    /// frontmatter field that doesn't match the skill's own directory
    /// name. Never turned into a discovery failure: the directory name
    /// is what actually governs `import <name>` (see this module's own
    /// doc comment), so a mismatched `name:` is worth flagging, not
    /// worth refusing to load the skill over.
    pub warnings: Vec<String>,
}

/// Scans [`paths::global_skills_dir`] for subdirectories containing a
/// `SKILL.md`, sorted by name for a stable `skill list`/tool-description
/// ordering. A missing skills directory contributes nothing rather than
/// erroring -- most invocations have none installed, the same tolerance
/// `prompt_template::read_dir` already has for its own directories.
pub fn discover(state_root: &Path) -> Result<Vec<Skill>> {
    let dir = paths::global_skills_dir(state_root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HarnessError::io(Context::Runtime, Some(dir), e)),
    };

    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| HarnessError::io(Context::Runtime, Some(dir.clone()), e))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let manifest_path = path.join("SKILL.md");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(content) => content,
            // No `SKILL.md` -- not a skill directory (e.g. stray files a
            // user dropped alongside real skills), skip rather than error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(HarnessError::io(Context::Runtime, Some(manifest_path), e)),
        };
        let (mut fields, _body) = crate::frontmatter::parse(&content);
        let description = fields.remove("description");
        let license = fields.remove("license");
        let compatibility = fields.remove("compatibility");
        let disable_model_invocation = fields
            .remove("disable-model-invocation")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));

        let mut warnings = Vec::new();
        if let Some(declared_name) = fields.remove("name") {
            if declared_name != name {
                warnings.push(format!(
                    "SKILL.md declares name `{declared_name}`, but the skill's own \
                     directory is `{name}` -- `import {name}` is what actually works, \
                     not this field"
                ));
            }
        }

        skills.push(Skill {
            name: name.to_string(),
            description,
            license,
            compatibility,
            disable_model_invocation,
            warnings,
        });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-skills-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(state_root: &Path, name: &str, manifest: &str) {
        let dir = paths::global_skills_dir(state_root).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("SKILL.md"))
            .unwrap()
            .write_all(manifest.as_bytes())
            .unwrap();
    }

    #[test]
    fn missing_skills_dir_returns_empty() {
        let root = temp_state_root("missing");
        assert_eq!(discover(&root).unwrap(), vec![]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_directory_without_skill_md_is_skipped() {
        let root = temp_state_root("no-manifest");
        std::fs::create_dir_all(paths::global_skills_dir(&root).join("not_a_skill")).unwrap();
        assert_eq!(discover(&root).unwrap(), vec![]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discovers_and_parses_a_real_skill() {
        let root = temp_state_root("real");
        write_skill(
            &root,
            "weather",
            "---\ndescription: fetch weather data\n---\nLonger docs go here.\n",
        );
        let skills = discover(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "weather");
        assert_eq!(skills[0].description.as_deref(), Some("fetch weather data"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_license_compatibility_and_disable_model_invocation() {
        let root = temp_state_root("extra-fields");
        write_skill(
            &root,
            "weather",
            "---\ndescription: fetch weather data\nlicense: MIT\n\
             compatibility: >=1.0\ndisable-model-invocation: true\n---\n",
        );
        let skills = discover(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].license.as_deref(), Some("MIT"));
        assert_eq!(skills[0].compatibility.as_deref(), Some(">=1.0"));
        assert!(skills[0].disable_model_invocation);
        assert!(skills[0].warnings.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn disable_model_invocation_defaults_to_false_when_absent() {
        let root = temp_state_root("disable-absent");
        write_skill(
            &root,
            "weather",
            "---\ndescription: fetch weather data\n---\n",
        );
        let skills = discover(&root).unwrap();
        assert!(!skills[0].disable_model_invocation);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn disable_model_invocation_is_case_insensitive() {
        let root = temp_state_root("disable-case");
        write_skill(
            &root,
            "weather",
            "---\ndescription: fetch weather data\ndisable-model-invocation: TRUE\n---\n",
        );
        let skills = discover(&root).unwrap();
        assert!(skills[0].disable_model_invocation);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_name_field_matching_the_directory_produces_no_warning() {
        let root = temp_state_root("name-matches");
        write_skill(
            &root,
            "weather",
            "---\ndescription: fetch weather data\nname: weather\n---\n",
        );
        let skills = discover(&root).unwrap();
        assert!(skills[0].warnings.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_name_field_mismatching_the_directory_produces_a_warning_not_a_failure() {
        let root = temp_state_root("name-mismatch");
        write_skill(
            &root,
            "weather",
            "---\ndescription: fetch weather data\nname: weather-forecaster\n---\n",
        );
        let skills = discover(&root).unwrap();
        assert_eq!(
            skills.len(),
            1,
            "a mismatched name: must not fail discovery"
        );
        assert_eq!(skills[0].name, "weather", "the directory name still wins");
        assert_eq!(skills[0].warnings.len(), 1);
        assert!(skills[0].warnings[0].contains("weather-forecaster"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_skill_with_no_description_field_still_discovers_with_none() {
        let root = temp_state_root("no-desc");
        write_skill(&root, "bare", "no frontmatter at all\n");
        let skills = discover(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn results_are_sorted_by_name() {
        let root = temp_state_root("sorted");
        write_skill(&root, "zeta", "---\ndescription: z\n---\n");
        write_skill(&root, "alpha", "---\ndescription: a\n---\n");
        let skills = discover(&root).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
