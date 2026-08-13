//! Bounded parity with `prime-agent`'s own theme-token system -- see
//! `PARITY.md`'s "Themes: token spec + TUI renderer" entry for the full
//! story, including a real caveat worth stating up front: this
//! sandbox has no checkout of `prime-agent`'s own source, only this
//! project's prior, second-hand characterization of `docs/themes.md`
//! ("51 required color tokens across 6 categories, 4 value formats").
//! A live fetch of that file, attempted while scoping this increment,
//! returned detailed, plausible-looking token lists across repeated
//! calls -- but the calls disagreed with each other on category counts
//! (6, 7, and 8 depending on the attempt) and, added up, totaled 52
//! tokens against the claimed 51 -- signs of an unreliable extraction,
//! not a verified source. [`REQUIRED_TOKENS`] below is the token list
//! that recurred consistently across those attempts, kept as-is (52
//! tokens, 7 categories) rather than silently trimmed to force-fit an
//! unverified round number. Presented honestly as "closely modeled on
//! `prime-agent`'s own token spec," not "byte-for-byte verified
//! parity" -- the same distinction `PARITY.md` already draws elsewhere
//! between "a real gap" and "an unverifiable claim."
//!
//! Deliberately bounded on the *rendering* side too: this project has
//! no markdown renderer, no syntax highlighter, and no diff renderer at
//! all, so the ~35 tokens those three categories exist to color
//! ([`REQUIRED_TOKENS`]'s `md*`/`syntax*`/`toolDiff*` entries) are
//! parsed and validated (a theme must still define all of them --
//! `prime-agent`'s own "no optional colors" rule, kept as-is) but never
//! actually consumed by anything today. Applying a color to a feature
//! that doesn't exist would be cosmetic dishonesty, not parity. Only a
//! handful of tokens (`success`/`error`/`warning`/`muted`) are wired
//! into real output, in `client::session_repl` -- see that function's
//! own use of [`colorize`].

use std::collections::HashMap;

use serde::Deserialize;

/// Every token a valid theme must define -- see this module's own doc
/// comment for why this list is 52 entries across 7 categories rather
/// than the claimed "51 across 6": it's what a live-fetch attempt
/// consistently reconstructed, kept honestly rather than force-trimmed.
pub const REQUIRED_TOKENS: &[&str] = &[
    // Core UI
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    // Backgrounds & Content
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolPanelBg",
    "toolTitle",
    "toolOutput",
    // Markdown
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    // Tool Diffs
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    // Syntax Highlighting
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    // Thinking Level Borders
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    // Bash Mode
    "bashMode",
];

/// The on-disk shape of a custom theme file -- parity with
/// `prime-agent`'s own `{"name", "vars", "colors"}` structure. `vars`
/// is a table of reusable named colors a `colors` entry can reference
/// by name instead of repeating a literal value.
#[derive(Debug, Clone, Deserialize)]
pub struct ThemeFile {
    pub name: String,
    #[serde(default)]
    pub vars: HashMap<String, String>,
    pub colors: HashMap<String, String>,
}

/// A fully resolved theme: every token already parsed into a
/// [`ColorValue`], `vars` references already substituted. Built once
/// per `session repl` run (see `client::session_repl`'s own startup
/// sequence) -- no live reload, the same "read once at startup, no
/// hot-swap" stance `settings::load`'s own two existing fields already
/// have.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    colors: HashMap<String, ColorValue>,
}

/// One resolved token value -- parity with the four raw formats
/// `prime-agent`'s own theme files use: a `#rrggbb` hex literal (24-bit
/// RGB, [`ColorValue::Rgb`]), a bare `0`-`255` xterm palette index
/// ([`ColorValue::Indexed`]), a `vars` name (resolved away before this
/// type is ever constructed -- by the time a raw string becomes a
/// `ColorValue` it's already been looked up), or an empty string
/// (`""`, meaning "leave the terminal's own default color alone" --
/// [`ColorValue::Default`], which emits no ANSI escape at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorValue {
    Rgb(u8, u8, u8),
    Indexed(u8),
    Default,
}

impl ColorValue {
    /// Parses one raw token value, resolving a `vars` reference first
    /// if `raw` matches a key in `vars` -- `vars` entries are
    /// themselves plain hex/indexed/empty values, not further
    /// references, so this is a single lookup, not a recursive walk.
    /// An unrecognized format (neither `#rrggbb`, a valid `0`-`255`
    /// integer, nor empty) falls back to [`ColorValue::Default`] rather
    /// than failing the whole theme load -- the same "an unparseable
    /// value degrades to the default, non-fatal" stance `settings.rs`
    /// already takes for a malformed override.
    fn parse(raw: &str, vars: &HashMap<String, String>) -> ColorValue {
        let raw = vars.get(raw).map(String::as_str).unwrap_or(raw);
        if raw.is_empty() {
            return ColorValue::Default;
        }
        if let Some(hex) = raw.strip_prefix('#') {
            if hex.len() == 6 {
                let parsed = (
                    u8::from_str_radix(&hex[0..2], 16),
                    u8::from_str_radix(&hex[2..4], 16),
                    u8::from_str_radix(&hex[4..6], 16),
                );
                if let (Ok(r), Ok(g), Ok(b)) = parsed {
                    return ColorValue::Rgb(r, g, b);
                }
            }
            return ColorValue::Default;
        }
        match raw.parse::<u8>() {
            Ok(index) => ColorValue::Indexed(index),
            Err(_) => ColorValue::Default,
        }
    }

    /// The ANSI SGR escape that sets this color as the foreground --
    /// `None` for [`ColorValue::Default`], since there's nothing to
    /// emit (the terminal's own default foreground already applies).
    /// 24-bit (`\x1b[38;2;R;G;Bm`) and 256-color (`\x1b[38;5;Nm`) are
    /// both widely supported by real terminal emulators today; no
    /// fallback/downsampling between the two is attempted (a real
    /// `prime-agent` feature this bounded increment doesn't take on).
    fn ansi_prefix(self) -> Option<String> {
        match self {
            ColorValue::Rgb(r, g, b) => Some(format!("\x1b[38;2;{r};{g};{b}m")),
            ColorValue::Indexed(i) => Some(format!("\x1b[38;5;{i}m")),
            ColorValue::Default => None,
        }
    }
}

impl Theme {
    /// Validates every [`REQUIRED_TOKENS`] entry is present (parity
    /// with `prime-agent`'s own "every theme must define all color
    /// tokens; there are no optional colors" rule) and resolves each
    /// one. A theme missing any required token is rejected outright,
    /// not silently padded with defaults -- an incomplete theme file is
    /// a real authoring mistake worth surfacing, not swallowing.
    fn from_file(file: ThemeFile) -> Result<Theme, String> {
        let missing: Vec<&str> = REQUIRED_TOKENS
            .iter()
            .filter(|t| !file.colors.contains_key(**t))
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "theme {:?} is missing required color token(s): {}",
                file.name,
                missing.join(", ")
            ));
        }
        let colors = file
            .colors
            .iter()
            .map(|(k, v)| (k.clone(), ColorValue::parse(v, &file.vars)))
            .collect();
        Ok(Theme {
            name: file.name,
            colors,
        })
    }

    /// The color a real token name resolves to in this theme, if it's
    /// one of [`REQUIRED_TOKENS`] and this theme actually defines it
    /// (always true for a `Theme` built via [`Theme::from_file`],
    /// [`Theme::dark`], or [`Theme::light`] -- all three validate every
    /// required token is present).
    pub fn token(&self, name: &str) -> Option<ColorValue> {
        self.colors.get(name).copied()
    }

    /// Named parity with `prime-agent`'s own built-in `dark` theme --
    /// the one built-in this project actually implements (see this
    /// module's own doc comment for why the rest of the real spec's
    /// surface is parsed but not rendered). A small, deliberately
    /// muted palette -- this project's REPL output is plain text lines,
    /// not boxed panels or markdown, so bright/saturated colors would
    /// look out of place against the handful of tokens actually
    /// applied ([`success`]/[`error`]/[`warning`]/[`muted`] in
    /// `client::session_repl`).
    pub fn dark() -> Theme {
        built_in_theme("dark", |name| match name {
            "success" => ColorValue::Rgb(0x8f, 0xc9, 0x71),
            "error" => ColorValue::Rgb(0xe5, 0x6b, 0x6b),
            "warning" => ColorValue::Rgb(0xe0, 0xc0, 0x6a),
            "muted" | "dim" => ColorValue::Rgb(0x80, 0x80, 0x80),
            "accent" => ColorValue::Rgb(0x6a, 0xa8, 0xe0),
            "text" => ColorValue::Rgb(0xe0, 0xe0, 0xe0),
            _ => ColorValue::Default,
        })
    }

    /// Named parity with `prime-agent`'s own built-in `light` theme --
    /// same shape as [`Theme::dark`], darker-on-light-background
    /// values instead.
    pub fn light() -> Theme {
        built_in_theme("light", |name| match name {
            "success" => ColorValue::Rgb(0x1a, 0x7d, 0x3a),
            "error" => ColorValue::Rgb(0xc4, 0x2b, 0x2b),
            "warning" => ColorValue::Rgb(0x8a, 0x6d, 0x0f),
            "muted" | "dim" => ColorValue::Rgb(0x60, 0x60, 0x60),
            "accent" => ColorValue::Rgb(0x1f, 0x5f, 0x99),
            "text" => ColorValue::Rgb(0x20, 0x20, 0x20),
            _ => ColorValue::Default,
        })
    }

    /// `"dark"`/`"light"` by name, matching `prime-agent`'s own two
    /// built-in theme names (confirmed the one detail every scoping
    /// attempt agreed on -- see this module's own doc comment).
    /// Anything else isn't a built-in.
    pub fn builtin(name: &str) -> Option<Theme> {
        match name {
            "dark" => Some(Theme::dark()),
            "light" => Some(Theme::light()),
            _ => None,
        }
    }
}

/// Every [`REQUIRED_TOKENS`] entry resolves to [`ColorValue::Default`]
/// unless `f` overrides it -- keeps [`Theme::dark`]/[`Theme::light`]
/// honest about which tokens they actually assign a real color to
/// (the handful this increment renders) versus which ones exist only
/// to satisfy the "every theme defines every token" validation rule.
fn built_in_theme(name: &str, f: impl Fn(&str) -> ColorValue) -> Theme {
    let colors = REQUIRED_TOKENS
        .iter()
        .map(|&token| (token.to_string(), f(token)))
        .collect();
    Theme {
        name: name.to_string(),
        colors,
    }
}

/// Resolves `settings.json`'s own `theme` field into an active
/// [`Theme`]. `None` (unset) and the two built-in names both succeed
/// immediately; anything else is treated as a path to a custom theme
/// JSON file. Never fails outward -- a missing file, invalid JSON, or a
/// theme missing required tokens all fall back to the built-in `dark`
/// theme, the same "an unparseable override degrades to the default,
/// non-fatal" stance `settings::load` already takes, with the failure
/// reason returned (not silently dropped) so the caller can report it.
pub fn resolve(theme_setting: Option<&str>) -> (Theme, Option<String>) {
    match theme_setting {
        None => (Theme::dark(), None),
        Some(name) if Theme::builtin(name).is_some() => (Theme::builtin(name).unwrap(), None),
        Some(path) => {
            let loaded = std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|text| {
                    serde_json::from_str::<ThemeFile>(&text).map_err(|e| e.to_string())
                })
                .and_then(Theme::from_file);
            match loaded {
                Ok(theme) => (theme, None),
                Err(e) => (
                    Theme::dark(),
                    Some(format!(
                        "failed to load theme {path:?}: {e} -- falling back to the built-in \
                         dark theme"
                    )),
                ),
            }
        }
    }
}

/// Whether this process should emit ANSI color codes at all: a real
/// interactive terminal (the same `termctl::is_tty()` check raw mode
/// already gates on) that hasn't opted out via `NO_COLOR`
/// (<https://no-color.org>, a widely-honored convention this project
/// didn't have to invent). Every one of this project's own tests pipes
/// stdio, so `is_tty()` reports `false` there and colorized output is
/// never exercised by the automated suite -- see `PARITY.md`'s own
/// entry for how this was verified instead (a real pty pass, the same
/// technique raw mode's own `is_tty()`/`RawModeGuard::enable()` needed).
pub fn colors_enabled() -> bool {
    crate::termctl::is_tty() && std::env::var_os("NO_COLOR").is_none()
}

/// Wraps `text` in `color`'s ANSI SGR prefix plus a trailing reset
/// (`\x1b[0m`), or returns it unchanged for [`ColorValue::Default`]/
/// `None`/`enabled: false`. The one function `client::session_repl`
/// actually calls to color its own output -- everything else in this
/// module is resolving *which* color to pass in.
pub fn colorize(text: &str, color: Option<ColorValue>, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    match color.and_then(ColorValue::ansi_prefix) {
        Some(prefix) => format!("{prefix}{text}\x1b[0m"),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_tokens_has_no_duplicates() {
        let mut sorted = REQUIRED_TOKENS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), REQUIRED_TOKENS.len());
    }

    #[test]
    fn color_value_parses_a_hex_literal() {
        assert_eq!(
            ColorValue::parse("#ff8800", &HashMap::new()),
            ColorValue::Rgb(0xff, 0x88, 0x00)
        );
    }

    #[test]
    fn color_value_parses_a_256_color_index() {
        assert_eq!(
            ColorValue::parse("39", &HashMap::new()),
            ColorValue::Indexed(39)
        );
    }

    #[test]
    fn color_value_parses_an_empty_string_as_default() {
        assert_eq!(ColorValue::parse("", &HashMap::new()), ColorValue::Default);
    }

    #[test]
    fn color_value_resolves_a_vars_reference() {
        let mut vars = HashMap::new();
        vars.insert("primary".to_string(), "#123456".to_string());
        assert_eq!(
            ColorValue::parse("primary", &vars),
            ColorValue::Rgb(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn color_value_falls_back_to_default_for_garbage() {
        assert_eq!(
            ColorValue::parse("not-a-color", &HashMap::new()),
            ColorValue::Default
        );
        assert_eq!(
            ColorValue::parse("#zzzzzz", &HashMap::new()),
            ColorValue::Default
        );
        assert_eq!(
            ColorValue::parse("999", &HashMap::new()),
            ColorValue::Default
        );
    }

    #[test]
    fn ansi_prefix_matches_the_expected_sgr_sequences() {
        assert_eq!(
            ColorValue::Rgb(1, 2, 3).ansi_prefix(),
            Some("\x1b[38;2;1;2;3m".to_string())
        );
        assert_eq!(
            ColorValue::Indexed(9).ansi_prefix(),
            Some("\x1b[38;5;9m".to_string())
        );
        assert_eq!(ColorValue::Default.ansi_prefix(), None);
    }

    #[test]
    fn colorize_wraps_text_when_enabled_with_a_real_color() {
        let out = colorize("hi", Some(ColorValue::Indexed(1)), true);
        assert_eq!(out, "\x1b[38;5;1mhi\x1b[0m");
    }

    #[test]
    fn colorize_leaves_text_untouched_when_disabled() {
        assert_eq!(colorize("hi", Some(ColorValue::Indexed(1)), false), "hi");
    }

    #[test]
    fn colorize_leaves_text_untouched_for_a_default_color() {
        assert_eq!(colorize("hi", Some(ColorValue::Default), true), "hi");
        assert_eq!(colorize("hi", None, true), "hi");
    }

    #[test]
    fn builtin_themes_define_every_required_token() {
        for theme in [Theme::dark(), Theme::light()] {
            for token in REQUIRED_TOKENS {
                assert!(
                    theme.token(token).is_some(),
                    "{} theme missing {token}",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn resolve_with_no_setting_defaults_to_dark() {
        let (theme, warning) = resolve(None);
        assert_eq!(theme.name, "dark");
        assert!(warning.is_none());
    }

    #[test]
    fn resolve_with_a_builtin_name_picks_it() {
        let (theme, warning) = resolve(Some("light"));
        assert_eq!(theme.name, "light");
        assert!(warning.is_none());
    }

    #[test]
    fn resolve_with_an_unreadable_path_falls_back_to_dark_with_a_warning() {
        let (theme, warning) = resolve(Some("/no/such/theme.json"));
        assert_eq!(theme.name, "dark");
        assert!(warning.unwrap().contains("failed to load theme"));
    }

    #[test]
    fn resolve_loads_a_valid_custom_theme_file() {
        let dir = std::env::temp_dir().join(format!("rpa-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("custom.json");
        let mut colors = serde_json::Map::new();
        for token in REQUIRED_TOKENS {
            colors.insert(
                (*token).to_string(),
                serde_json::Value::String(String::new()),
            );
        }
        colors.insert(
            "success".to_string(),
            serde_json::Value::String("primary".to_string()),
        );
        let json = serde_json::json!({
            "name": "custom",
            "vars": { "primary": "#00ff00" },
            "colors": colors,
        });
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let (theme, warning) = resolve(Some(path.to_str().unwrap()));
        assert!(warning.is_none(), "got warning: {warning:?}");
        assert_eq!(theme.name, "custom");
        assert_eq!(theme.token("success"), Some(ColorValue::Rgb(0, 0xff, 0)));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_rejects_a_theme_file_missing_required_tokens() {
        let dir = std::env::temp_dir().join(format!("rpa-theme-incomplete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("incomplete.json");
        std::fs::write(
            &path,
            r##"{"name": "incomplete", "colors": {"accent": "#ffffff"}}"##,
        )
        .unwrap();

        let (theme, warning) = resolve(Some(path.to_str().unwrap()));
        assert_eq!(theme.name, "dark");
        assert!(warning.unwrap().contains("missing required color token"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
