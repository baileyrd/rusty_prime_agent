//! `---\nkey: value\n...\n---\n<body>` frontmatter parsing, shared by
//! `prompt_template` (`description`/`argument-hint`) and `skills`
//! (`description`) -- hand-rolled rather than a YAML dependency, since
//! both callers only ever read a couple of flat string keys, matching
//! this project's deliberately narrow dependency floor. A file with no
//! leading `---` block is entirely body, no frontmatter.

use std::collections::BTreeMap;

/// Splits `content` into its frontmatter key/value pairs (in file order,
/// via a `BTreeMap` only for ordered, deduplicated *lookup*, not to
/// prescribe a display order the caller doesn't get to choose) and the
/// trimmed body that follows.
pub fn parse(content: &str) -> (BTreeMap<String, String>, &str) {
    let mut fields = BTreeMap::new();
    let mut body = content;

    if let Some(rest) = content.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            let frontmatter = &rest[..end];
            body = &rest[end + "\n---\n".len()..];
            for line in frontmatter.lines() {
                if let Some((key, value)) = line.split_once(':') {
                    fields.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }
    }

    (fields, body.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_fields_and_trims_body() {
        let content = "---\ndescription: does a thing\nargument-hint: <name>\n---\nThe body.\n";
        let (fields, body) = parse(content);
        assert_eq!(
            fields.get("description").map(String::as_str),
            Some("does a thing")
        );
        assert_eq!(
            fields.get("argument-hint").map(String::as_str),
            Some("<name>")
        );
        assert_eq!(body, "The body.");
    }

    #[test]
    fn no_leading_marker_is_entirely_body() {
        let (fields, body) = parse("just a body, no frontmatter\n");
        assert!(fields.is_empty());
        assert_eq!(body, "just a body, no frontmatter");
    }

    #[test]
    fn unterminated_frontmatter_block_is_treated_as_entirely_body() {
        let content = "---\ndescription: oops, no closing marker\nstill going";
        let (fields, body) = parse(content);
        assert!(fields.is_empty());
        assert_eq!(body, content.trim());
    }
}
