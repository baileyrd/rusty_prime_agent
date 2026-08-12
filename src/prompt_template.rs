//! Bounded, non-Python parity with `prime-agent`'s prompt templates
//! (`packages/coding-agent/docs/prompt-templates.md`): Markdown-plus-
//! frontmatter snippets that expand into a full prompt, discovered from
//! a project-local and a global directory (`paths::project_prompts_dir`/
//! `paths::global_prompts_dir`), invoked by filename (minus `.md`). Real
//! `prime-agent` "skills" (`packages/coding-agent/docs/skills.md`) are a
//! separate, much larger concept -- "importable Python packages" wired
//! to the RLM control environment (`tool_runtime::ToolRuntime`'s
//! deliberately open seam, Phase 1 backs it with `NoopToolRuntime` only)
//! -- and stay out of scope; this covers only the plain-text template
//! half of that surface, which needs no code execution at all. See
//! `PARITY.md`.

use std::path::Path;

use crate::error::{Context, HarnessError, Result};
use crate::paths;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub body: String,
}

impl PromptTemplate {
    pub fn expand(&self, args: &[String]) -> String {
        expand_args(&self.body, args)
    }
}

/// Discovers every `*.md` file under the global directory and the
/// project-local directory, project-local entries winning on a name
/// collision (more specific wins, matching most layered-config
/// conventions). A missing directory contributes nothing rather than
/// erroring -- most invocations have neither set up.
pub fn discover(state_root: &Path, cwd: &Path) -> Result<Vec<PromptTemplate>> {
    let mut by_name = std::collections::BTreeMap::new();
    for dir in [
        paths::global_prompts_dir(state_root),
        paths::project_prompts_dir(cwd),
    ] {
        for template in read_dir(&dir)? {
            by_name.insert(template.name.clone(), template);
        }
    }
    Ok(by_name.into_values().collect())
}

pub fn find(state_root: &Path, cwd: &Path, name: &str) -> Result<Option<PromptTemplate>> {
    Ok(discover(state_root, cwd)?
        .into_iter()
        .find(|t| t.name == name))
}

fn read_dir(dir: &Path) -> Result<Vec<PromptTemplate>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HarnessError::io(Context::Cli, Some(dir.to_path_buf()), e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| HarnessError::io(Context::Cli, Some(dir.to_path_buf()), e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let content = std::fs::read_to_string(&path)
            .map_err(|e| HarnessError::io(Context::Cli, Some(path.clone()), e))?;
        out.push(parse(name.to_string(), &content));
    }
    Ok(out)
}

/// Only two keys (`description`, `argument-hint`) are ever read out of
/// `crate::frontmatter::parse`'s full field map -- see that module's own
/// doc comment for the frontmatter shape itself.
fn parse(name: String, content: &str) -> PromptTemplate {
    let (mut fields, body) = crate::frontmatter::parse(content);
    PromptTemplate {
        name,
        description: fields.remove("description"),
        argument_hint: fields.remove("argument-hint"),
        body: body.to_string(),
    }
}

/// Positional-argument substitution -- `$1`/`$2`/... for individual
/// arguments (missing ones expand to nothing), `$@`/`$ARGUMENTS` for
/// every argument joined by a space, `${@:N}` for arguments from
/// 1-indexed position `N` onward, and `${@:N:L}` for `L` arguments
/// starting at position `N`. Hand-rolled, not a templating-engine
/// dependency -- same reasoning as this module's frontmatter parser.
fn expand_args(body: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let ch_len = body[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&body[i..i + ch_len]);
            i += ch_len;
            continue;
        }

        if body[i..].starts_with("$ARGUMENTS") {
            out.push_str(&args.join(" "));
            i += "$ARGUMENTS".len();
            continue;
        }
        if let Some(rest) = body[i..].strip_prefix("${@:") {
            if let Some(close) = rest.find('}') {
                let inner = &rest[..close];
                let (start, len) = match inner.split_once(':') {
                    Some((n, l)) => (n.parse::<usize>().ok(), l.parse::<usize>().ok()),
                    None => (inner.parse::<usize>().ok(), None),
                };
                if let Some(n) = start {
                    let from = n.saturating_sub(1).min(args.len());
                    let to = match len {
                        Some(l) => (from + l).min(args.len()),
                        None => args.len(),
                    };
                    out.push_str(&args[from..to.max(from)].join(" "));
                    i += "${@:".len() + close + 1;
                    continue;
                }
            }
        }
        if body[i..].starts_with("$@") {
            out.push_str(&args.join(" "));
            i += 2;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if let Ok(n) = body[i + 1..j].parse::<usize>() {
                if n >= 1 {
                    if let Some(a) = args.get(n - 1) {
                        out.push_str(a);
                    }
                }
            }
            i = j;
            continue;
        }

        out.push('$');
        i += 1;
    }
    out
}
