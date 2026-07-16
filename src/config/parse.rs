use super::types::*;
use anyhow::{bail, Result};
use std::path::PathBuf;

use crate::apply::WallpaperFit;
use crate::transition::{Backend, TransitionStyle};

// ---------------------------------------------------------------------------
// Tiny hand-rolled KDL parser
//
// Grammar subset supported:
//   - `//` line comments (and inline after a space)
//   - block sections:  `section-name { ... }`
//   - block sections with a quoted arg: `section-name "arg" { ... }`
//   - key-value pairs: `key value`  or  `key "value"`  or  `key=value`
//   - bare values (section lines without `{`) treated as key=true
// ---------------------------------------------------------------------------

/// A parsed token from one non-blank, non-comment line.
#[derive(Debug)]
enum Line<'a> {
    SectionOpen { name: &'a str, arg: Option<String> },
    SectionClose,
    KeyValue { key: String, value: String },
    Bare { token: String },
}

fn strip_comment(s: &str) -> Result<&str> {
    let mut chars = s.char_indices().peekable();
    let mut quote = None;
    let mut escaped = false;

    while let Some((index, ch)) = chars.next() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == '/' && chars.peek().is_some_and(|(_, next)| *next == '/') {
            return Ok(s[..index].trim_end());
        }
    }

    if quote.is_some() {
        bail!("unterminated quoted string");
    }

    Ok(s)
}

/// Pull out a quoted string starting from byte `start`, returning
/// (content, end_index_exclusive).
fn parse_quoted(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'"') && bytes.get(start) != Some(&b'\'') {
        return None;
    }
    let quote = bytes[start] as char;
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        let ch = s[i..].chars().next()?;
        if ch == '\\' && i + 1 < bytes.len() {
            i += 1;
            let escaped = s[i..].chars().next()?;
            match escaped {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            i += escaped.len_utf8();
        } else if ch == quote {
            return Some((out, i + ch.len_utf8()));
        } else {
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    None // strip_comment rejects unterminated strings before tokenization
}

fn parse_value_tokens(mut s: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();

    loop {
        s = s.trim_start_matches(|ch: char| ch.is_whitespace() || ch == ',');
        if s.is_empty() {
            break;
        }

        if s.starts_with('"') || s.starts_with('\'') {
            let (value, end) =
                parse_quoted(s, 0).ok_or_else(|| anyhow::anyhow!("unterminated quoted string"))?;
            if s[end..]
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_whitespace() && ch != ',')
            {
                bail!("unexpected text after quoted value");
            }
            values.push(value);
            s = &s[end..];
            continue;
        }

        let end = s
            .find(|ch: char| ch.is_whitespace() || ch == ',')
            .unwrap_or(s.len());
        let value = &s[..end];
        if value.contains(['"', '\'']) {
            bail!("malformed quoted value");
        }
        values.push(value.to_string());
        s = &s[end..];
    }

    Ok(values)
}

/// Parse a single non-blank, non-comment line into a `Line` token.
fn lex_line(raw: &str) -> Result<Option<Line<'_>>> {
    let s = strip_comment(raw)?.trim();
    if s.is_empty() {
        return Ok(None);
    }

    // Closing brace
    if s == "}" {
        return Ok(Some(Line::SectionClose));
    }

    // Check for section open: ends with `{` (possibly after a quoted arg).
    // Pattern: `name {` or `name "arg" {`
    if let Some(body) = s.strip_suffix('{') {
        let body = body.trim();
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let name = &body[..name_end];
        if name.is_empty() || name.contains(['"', '\'', '=', '{', '}']) {
            bail!("invalid section declaration");
        }
        let rest = body[name_end..].trim();
        let arg = if rest.is_empty() {
            None
        } else {
            let (arg, end) = parse_quoted(rest, 0)
                .ok_or_else(|| anyhow::anyhow!("expected quoted section argument"))?;
            if !rest[end..].trim().is_empty() {
                bail!("unexpected text after section argument");
            }
            Some(arg)
        };
        return Ok(Some(Line::SectionOpen { name, arg }));
    }

    // Key-value: `key = "value"`, `key = value`, `key "value"`, `key value`
    // Also handles `key=value` (no spaces around =).
    let key_end = s
        .find(|ch: char| ch.is_whitespace() || ch == '=')
        .unwrap_or(s.len());
    let key = &s[..key_end];
    if key.is_empty() || key.contains(['"', '\'', '{', '}']) {
        bail!("invalid key");
    }

    // Try to get the value — could be a quoted string or bare token
    let mut value_start = s[key_end..].trim_start();
    if let Some(rest) = value_start.strip_prefix('=') {
        value_start = rest.trim_start();
    }

    if key == "extensions" {
        let values = parse_value_tokens(value_start)?;
        if !values.is_empty() {
            return Ok(Some(Line::KeyValue {
                key: key.to_string(),
                value: values.join(","),
            }));
        }
    }

    let values = parse_value_tokens(value_start)?;
    if values.len() > 1 {
        bail!("expected one value for key {:?}", key);
    }

    if let Some(value) = values.into_iter().next() {
        return Ok(Some(Line::KeyValue {
            key: key.to_string(),
            value,
        }));
    }

    // No value — bare token / flag
    Ok(Some(Line::Bare {
        token: key.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// High-level config builder
// ---------------------------------------------------------------------------

pub fn parse_kdl_config(input: &str) -> Result<Config> {
    // Strip leading UTF-8 BOM if present (PowerShell 5.1's Out-File -Encoding utf8
    // writes one and silently breaks our hand-rolled parser).
    let input = input.strip_prefix('\u{FEFF}').unwrap_or(input);

    let mut config = Config::default();

    // Section stack: Vec of section names as we nest
    let mut section_stack: Vec<String> = Vec::new();

    // Accumulators for block objects
    let mut cur_source: Option<SourceConfig> = None;
    let mut cur_monitor: Option<MonitorOverride> = None;

    for (lineno, raw) in input.lines().enumerate() {
        let token = match lex_line(raw)
            .map_err(|error| anyhow::anyhow!("line {}: {}", lineno + 1, error))?
        {
            Some(t) => t,
            None => continue,
        };

        let section = section_stack.join(".");

        match token {
            Line::SectionOpen { name, arg } => {
                let known = matches!(
                    (section.as_str(), name),
                    (
                        "",
                        "source"
                            | "monitor"
                            | "schedule"
                            | "transitions"
                            | "transition"
                            | "metrics"
                            | "cache"
                    ) | ("source", "extensions")
                );
                if !known {
                    let name = if section.is_empty() {
                        name.to_string()
                    } else {
                        format!("{section}.{name}")
                    };
                    bail!("line {}: unknown section {:?}", lineno + 1, name);
                }
                if arg.is_some() && !matches!((section.as_str(), name), ("", "source" | "monitor"))
                {
                    bail!(
                        "line {}: section {:?} does not accept an argument",
                        lineno + 1,
                        name
                    );
                }

                // Start accumulator for known sections
                match (section.as_str(), name) {
                    ("", "source") => {
                        let mut s = SourceConfig::default();
                        if let Some(p) = arg {
                            s.path = PathBuf::from(p);
                        }
                        cur_source = Some(s);
                    }
                    ("", "monitor") => {
                        let mut m = MonitorOverride::default();
                        if let Some(n) = arg {
                            m.name = n;
                        }
                        cur_monitor = Some(m);
                    }
                    _ => {}
                }
                section_stack.push(name.to_string());
            }

            Line::SectionClose => {
                let closed = section_stack.pop().ok_or_else(|| {
                    anyhow::anyhow!("line {}: unmatched closing brace", lineno + 1)
                })?;
                let parent = section_stack.join(".");
                match (parent.as_str(), closed.as_str()) {
                    ("", "source") => {
                        if let Some(s) = cur_source.take() {
                            config.sources.push(s);
                        }
                    }
                    ("", "monitor") => {
                        if let Some(m) = cur_monitor.take() {
                            config.monitors.push(m);
                        }
                    }
                    _ => {}
                }
            }

            Line::KeyValue { key, value } => {
                apply_kv(
                    &mut config,
                    &mut cur_source,
                    &mut cur_monitor,
                    &section,
                    &key,
                    &value,
                )
                .map_err(|e| anyhow::anyhow!("line {}: {}", lineno + 1, e))?;
            }

            Line::Bare { token } => {
                if section == "source.extensions" {
                    if let Some(ref mut s) = cur_source {
                        s.extensions.push(token.to_lowercase());
                    }
                } else {
                    apply_kv(
                        &mut config,
                        &mut cur_source,
                        &mut cur_monitor,
                        &section,
                        &token,
                        "true",
                    )
                    .map_err(|error| anyhow::anyhow!("line {}: {}", lineno + 1, error))?;
                }
            }
        }
    }

    if !section_stack.is_empty() {
        bail!("unclosed section {:?}", section_stack.join("."));
    }

    validate_schedule(&mut config.schedule)?;
    Ok(config)
}

fn validate_schedule(schedule: &mut ScheduleConfig) -> Result<()> {
    schedule.mode = schedule.mode.trim().to_ascii_lowercase();
    match schedule.mode.as_str() {
        "interval" | "at" => {}
        "random" => bail!("schedule mode \"random\" is not supported; use \"interval\" or \"at\""),
        mode => bail!("unsupported schedule mode {mode:?}; expected \"interval\" or \"at\""),
    }

    for at in &schedule.at_times {
        crate::scheduler::parse_hhmm(at)
            .map_err(|error| anyhow::anyhow!("invalid schedule time {at:?}: {error}"))?;
    }
    if schedule.mode == "at" && schedule.at_times.is_empty() {
        bail!("schedule mode \"at\" requires at least one `at \"HH:MM\"` entry");
    }
    Ok(())
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => bail!("expected boolean, got {:?}", v),
    }
}

fn parse_u32(v: &str) -> Result<u32> {
    v.parse::<u32>()
        .map_err(|_| anyhow::anyhow!("expected u32, got {:?}", v))
}

fn parse_u64(v: &str) -> Result<u64> {
    v.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("expected u64, got {:?}", v))
}

fn parse_u16(v: &str) -> Result<u16> {
    v.parse::<u16>()
        .map_err(|_| anyhow::anyhow!("expected u16, got {:?}", v))
}

fn parse_usize(v: &str) -> Result<usize> {
    v.parse::<usize>()
        .map_err(|_| anyhow::anyhow!("expected usize, got {:?}", v))
}

fn validate_wallpaper_fit(value: &str) -> Result<()> {
    if matches!(WallpaperFit::parse(value), WallpaperFit::Fill)
        && !value.eq_ignore_ascii_case("fill")
    {
        bail!("unsupported wallpaper fit {value:?}");
    }
    Ok(())
}

fn validate_transition_style(value: &str) -> Result<()> {
    if matches!(TransitionStyle::parse(value), TransitionStyle::None)
        && !value.eq_ignore_ascii_case("none")
    {
        bail!("unsupported transition style {value:?}");
    }
    Ok(())
}

fn validate_transition_renderer(value: &str) -> Result<()> {
    if matches!(Backend::parse(value), Backend::Auto) && !value.eq_ignore_ascii_case("auto") {
        bail!("unsupported transition renderer {value:?}");
    }
    Ok(())
}

fn apply_kv(
    config: &mut Config,
    cur_source: &mut Option<SourceConfig>,
    cur_monitor: &mut Option<MonitorOverride>,
    section: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    match section {
        // ---- Top-level scalars ----
        "" => match key {
            "log-level" | "log_level" => config.log_level = value.to_string(),
            _ => bail!("unknown top-level key {:?}", key),
        },

        // ---- source block ----
        "source" => {
            let s = cur_source
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("source key outside source block"))?;
            match key {
                "path" => s.path = PathBuf::from(value),
                "recursive" => s.recursive = parse_bool(value)?,
                "min-width" | "min_width" => s.min_width = parse_u32(value)?,
                "min-height" | "min_height" => s.min_height = parse_u32(value)?,
                "extensions" => {
                    // Comma-separated or space-separated list on one line
                    s.extensions = value
                        .split([',', ' '])
                        .map(|t| t.trim().to_lowercase())
                        .filter(|t| !t.is_empty())
                        .collect();
                }
                _ => bail!("unknown key {:?} in source", key),
            }
        }

        // ---- schedule block ----
        "schedule" => match key {
            "mode" => config.schedule.mode = value.to_string(),
            "interval-secs" | "interval_secs" | "interval" => {
                config.schedule.interval_secs = parse_u64(value)?;
            }
            "at" => {
                config.schedule.at_times.push(value.to_string());
            }
            "on-workspace-change" | "on_workspace_change" => {
                config.schedule.on_workspace_change = parse_bool(value)?;
            }
            "pause-when-fullscreen" | "pause_when_fullscreen" => {
                config.schedule.pause_when_fullscreen = parse_bool(value)?;
            }
            "pause-when-idle-secs" | "pause_when_idle_secs" => {
                config.schedule.pause_when_idle_secs = parse_u32(value)?;
            }
            "min-repeat-window" | "min_repeat_window" => {
                config.schedule.min_repeat_window = parse_usize(value)?;
            }
            _ => bail!("unknown key {:?} in schedule", key),
        },

        // ---- monitor block ----
        "monitor" => {
            let m = cur_monitor
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("monitor key outside monitor block"))?;
            match key {
                "name" => m.name = value.to_string(),
                "fit" => {
                    validate_wallpaper_fit(value)?;
                    m.fit = value.to_string();
                }
                _ => bail!("unknown key {:?} in monitor", key),
            }
        }

        // ---- transitions block ----
        "transitions" | "transition" => match key {
            "enabled" => config.transitions.enabled = parse_bool(value)?,
            "duration-ms" | "duration_ms" | "duration" => {
                let duration_ms = parse_u32(value)?;
                if duration_ms > MAX_TRANSITION_DURATION_MS {
                    bail!(
                        "transition duration {duration_ms} ms exceeds the {MAX_TRANSITION_DURATION_MS} ms maximum"
                    );
                }
                config.transitions.duration_ms = duration_ms;
            }
            "style" => {
                validate_transition_style(value)?;
                config.transitions.style = value.to_string();
            }
            "renderer" => {
                validate_transition_renderer(value)?;
                config.transitions.renderer = value.to_string();
            }
            _ => bail!("unknown key {:?} in transitions", key),
        },

        // ---- metrics block ----
        "metrics" => match key {
            "enabled" => config.metrics.enabled = parse_bool(value)?,
            "port" => config.metrics.port = parse_u16(value)?,
            _ => bail!("unknown key {:?} in metrics", key),
        },

        // ---- cache block ----
        "cache" => match key {
            "decoded-mb" | "decoded_mb" => config.cache.decoded_mb = parse_u32(value)?,
            "prefetch-count" | "prefetch_count" => {
                config.cache.deprecated_prefetch_count = Some(parse_usize(value)?);
            }
            _ => bail!("unknown key {:?} in cache", key),
        },

        _ => bail!("unknown section {:?}", section),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Default config path helper
// ---------------------------------------------------------------------------

pub fn default_config_path() -> std::path::PathBuf {
    let mut p = dirs_or_appdata();
    p.push("aurora");
    p.push("config.kdl");
    p
}

fn dirs_or_appdata() -> std::path::PathBuf {
    // %APPDATA%  e.g. C:\Users\kalli\AppData\Roaming
    if let Some(p) = std::env::var_os("APPDATA") {
        return std::path::PathBuf::from(p);
    }
    // Fallback: home dir
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return std::path::PathBuf::from(p).join("AppData").join("Roaming");
    }
    std::path::PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_parses() {
        let src = include_str!("../../resources/default_config.kdl");
        let cfg = parse_kdl_config(src).expect("default_config.kdl should parse without error");
        assert!(!cfg.sources.is_empty(), "should have at least one source");
        assert_eq!(
            cfg.sources[0].extensions,
            DEFAULT_IMAGE_EXTENSIONS
                .iter()
                .map(|extension| (*extension).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(cfg.schedule.mode, "interval");
        assert!(!cfg.transitions.enabled);
        assert!(cfg.cache.deprecated_prefetch_count.is_none());
        tracing_subscriber::EnvFilter::try_new(&cfg.log_level)
            .expect("default log-level must be a valid filter");

        for stale_claim in [
            "watches this file",
            "\"random\"",
            "prefetch-count",
            "tint ",
            "triggers {",
        ] {
            assert!(
                !src.contains(stale_claim),
                "stale default claim: {stale_claim}"
            );
        }

        let readme = include_str!("../../README.md");
        for stale_claim in ["docs/PLAN.md", "on-idle", "prefetch"] {
            assert!(
                !readme.contains(stale_claim),
                "stale README claim: {stale_claim}"
            );
        }
    }

    #[test]
    fn validates_schedule_modes_and_times() {
        for mode in ["random", "on-idle", "unknown"] {
            let input = format!("schedule {{\nmode \"{mode}\"\n}}");
            assert!(
                parse_kdl_config(&input)
                    .unwrap_err()
                    .to_string()
                    .contains("schedule mode"),
                "mode {mode:?} did not fail clearly"
            );
        }

        assert!(parse_kdl_config("schedule {\nmode \"at\"\n}")
            .unwrap_err()
            .to_string()
            .contains("requires at least one"));
        assert!(parse_kdl_config("schedule {\nmode \"at\"\nat \"25:00\"\n}")
            .unwrap_err()
            .to_string()
            .contains("invalid schedule time"));
        assert_eq!(
            parse_kdl_config("schedule {\nmode \"AT\"\nat \"09:30\"\n}")
                .unwrap()
                .schedule
                .mode,
            "at"
        );
    }

    #[test]
    fn accepts_deprecated_prefetch_but_rejects_dead_config() {
        let config = parse_kdl_config("cache {\nprefetch-count 2\n}").unwrap();
        assert_eq!(config.cache.deprecated_prefetch_count, Some(2));

        for input in [
            "monitor {\ntint \"none\"\n}",
            "triggers {\non-startup \"script.cmd\"\n}",
        ] {
            assert!(
                parse_kdl_config(input).is_err(),
                "accepted dead config {input:?}"
            );
        }
    }

    #[test]
    fn test_parse_strips_bom() {
        // PowerShell 5.1 Out-File -Encoding utf8 prepends a UTF-8 BOM (U+FEFF).
        // Verify the parser silently strips it and returns Ok.
        let bom_src = "\u{FEFF}source \"X\" {\n}\n";
        let result = parse_kdl_config(bom_src);
        assert!(
            result.is_ok(),
            "BOM-prefixed config should parse without error: {:?}",
            result.err()
        );
        let cfg = result.unwrap();
        // The source path should have been captured as "X".
        assert_eq!(
            cfg.sources.len(),
            1,
            "expected 1 source, got {}: {:?}",
            cfg.sources.len(),
            cfg.sources
        );
        assert_eq!(cfg.sources[0].path.to_str().unwrap(), "X");
    }

    #[test]
    fn test_source_extensions_accept_multiple_quoted_values() {
        let src = r#"
source "C:\Users\kalli\Documents\custom_wallpapers" {
    recursive true
    extensions "jpg" "jpeg" "png" "heic" "webp"
}
"#;
        let cfg = parse_kdl_config(src).expect("config should parse");
        assert_eq!(
            cfg.sources[0].extensions,
            vec!["jpg", "jpeg", "png", "heic", "webp"]
        );
    }

    #[test]
    fn test_preserves_unicode_and_quoted_slashes() {
        let cfg = parse_kdl_config(
            r#"
source "//server/share/壁紙" { // quoted slashes are not a comment
    recursive true
}
"#,
        )
        .expect("quoted slashes and Unicode should parse");

        assert_eq!(cfg.sources[0].path, PathBuf::from("//server/share/壁紙"));
    }

    #[test]
    fn test_rejects_invalid_boolean() {
        let error = parse_kdl_config("source {\nrecursive maybe\n}")
            .expect_err("invalid boolean should fail");
        assert!(error.to_string().contains("expected boolean"));
    }

    #[test]
    fn test_rejects_unknown_keys_and_sections() {
        for input in ["schedule {\nunknown-key 1\n}", "unknown-section {\n}"] {
            assert!(parse_kdl_config(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn test_rejects_unclosed_blocks() {
        for input in ["source {\npath \"C:/wallpapers\"", "monitor {\nfit fill"] {
            assert!(parse_kdl_config(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn test_rejects_unmatched_braces_and_unterminated_strings() {
        for input in ["}", r#"source "unterminated {"#] {
            assert!(parse_kdl_config(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn validates_transition_duration_boundaries() {
        for duration_ms in [0, MAX_TRANSITION_DURATION_MS] {
            let input = format!("transitions {{\nduration-ms {duration_ms}\n}}");
            assert_eq!(
                parse_kdl_config(&input).unwrap().transitions.duration_ms,
                duration_ms
            );
        }

        let input = format!(
            "transitions {{\nduration-ms {}\n}}",
            MAX_TRANSITION_DURATION_MS + 1
        );
        let error = parse_kdl_config(&input).unwrap_err().to_string();
        assert!(error.contains("60000 ms maximum"), "{error}");
    }

    #[test]
    fn rejects_unknown_runtime_enum_values() {
        for (input, expected) in [
            ("monitor {\nfit crop\n}", "wallpaper fit"),
            ("transitions {\nstyle spin\n}", "transition style"),
            (
                "transitions {\nrenderer software-ish\n}",
                "transition renderer",
            ),
        ] {
            let error = parse_kdl_config(input).unwrap_err().to_string();
            assert!(error.contains(expected), "{input:?}: {error}");
        }
    }

    #[test]
    fn accepts_existing_runtime_enum_aliases_case_insensitively() {
        let config = parse_kdl_config(
            "monitor {\nfit CENTRE\n}\ntransitions {\nstyle SLIDE_LEFT\nrenderer GPU\n}",
        )
        .unwrap();

        assert_eq!(config.monitors[0].fit, "CENTRE");
        assert_eq!(config.transitions.style, "SLIDE_LEFT");
        assert_eq!(config.transitions.renderer, "GPU");
    }
}
