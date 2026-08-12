//! Built-in tools offered to a model when a session opts in via
//! `session new --tools read` (see `PARITY.md`'s "real tool-calling
//! loop" entry). Read-only first: `read_file`/`list_dir`, plain
//! `std::fs` calls, no path sandboxing -- consistent with this
//! project's existing single-local-user trust model, the same reasoning
//! `session_autonomous --quality-gate`'s unsandboxed shell execution
//! already established. `--tools shell`/write-capable tools are a
//! natural v2 extension of the same flag, not built now.

use crate::provider::ToolDef;

/// The tool set offered when a session's `state.tools == Some("read")`.
pub fn read_only_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file".to_string(),
            description: "Read the full contents of a file at the given path.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read.",
                    },
                },
                "required": ["path"],
            }),
        },
        ToolDef {
            name: "list_dir".to_string(),
            description: "List the entries of a directory at the given path.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the directory to list.",
                    },
                },
                "required": ["path"],
            }),
        },
    ]
}

/// Executes one built-in tool call by name, returning the text to send
/// back as the `Role::Tool` transcript entry's own text. An unknown tool
/// name or malformed arguments produce an `error: ...`-prefixed result
/// string rather than a `HarnessError` -- a model hallucinating a tool
/// name or a bad argument is normal, expected model behavior to hand
/// back as data for it to see and recover from, not a protocol-level
/// failure that should tear down the session.
pub fn execute(name: &str, arguments: &str) -> String {
    match name {
        "read_file" => read_file(arguments),
        "list_dir" => list_dir(arguments),
        other => format!("error: unknown tool `{other}`"),
    }
}

fn parse_path_arg(arguments: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|e| format!("error: invalid arguments JSON: {e}"))?;
    value["path"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "error: missing required `path` argument".to_string())
}

fn read_file(arguments: &str) -> String {
    let path = match parse_path_arg(arguments) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) => format!("error: could not read {path}: {e}"),
    }
}

fn list_dir(arguments: &str) -> String {
    let path = match parse_path_arg(arguments) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::read_dir(&path) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names.join("\n")
        }
        Err(e) => format!("error: could not list {path}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn read_only_tool_defs_offers_read_file_and_list_dir() {
        let defs = read_only_tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["read_file", "list_dir"]);
    }

    #[test]
    fn read_file_returns_contents_of_a_real_file() {
        let dir = std::env::temp_dir().join(format!("rpa-tools-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("hello.txt");
        std::fs::File::create(&file_path)
            .unwrap()
            .write_all(b"hello from a tool")
            .unwrap();

        let arguments = serde_json::json!({ "path": file_path.to_string_lossy() }).to_string();
        assert_eq!(execute("read_file", &arguments), "hello from a tool");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_file_reports_a_missing_file_as_an_error_string_not_a_panic() {
        let arguments = serde_json::json!({ "path": "/definitely/not/a/real/path" }).to_string();
        let result = execute("read_file", &arguments);
        assert!(result.starts_with("error:"), "got: {result}");
    }

    #[test]
    fn list_dir_lists_entries_sorted() {
        let dir = std::env::temp_dir().join(format!("rpa-tools-test-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("b.txt")).unwrap();
        std::fs::File::create(dir.join("a.txt")).unwrap();

        let arguments = serde_json::json!({ "path": dir.to_string_lossy() }).to_string();
        assert_eq!(execute("list_dir", &arguments), "a.txt\nb.txt");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_tool_name_returns_an_error_string() {
        let result = execute("delete_everything", "{}");
        assert_eq!(result, "error: unknown tool `delete_everything`");
    }

    #[test]
    fn missing_path_argument_returns_an_error_string() {
        let result = execute("read_file", "{}");
        assert_eq!(result, "error: missing required `path` argument");
    }
}
