use anyhow::{bail, Result};
use std::path::PathBuf;
use super::types::*;

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

fn strip_comment(s: &str) -> &str {
    // Strip `//` that is NOT inside a string literal (good enough for our
    // config format where strings don't contain `//`).
    if let Some(pos) = s.find("//") {
        s[..pos].trim_end()
    } else {
        s
    }
}

/// Pull out a quoted string starting from byte `start`, returning
/// (content, end_index_exclusive).
fn parse_quoted(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'"') && bytes.get(start) != Some(&b'\'') {
        return None;
    }
    let quote = bytes[start];
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'\\' => out.push('\\'),
                b'"' => out.push('"'),
                b'\'' => out.push('\''),
                c => { out.push('\\'); out.push(c as char); }
            }
        } else if bytes[i] == quote {
            return Some((out, i + 1));
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    None // unterminated string — treat as None; caller handles gracefully
}

/// Parse a single non-blank, non-comment line into a `Line` token.
fn lex_line(raw: &str) -> Option<Line<'_>> {
    let s = strip_comment(raw).trim();
    if s.is_empty() {
        return None;
    }

    // Closing brace
    if s.starts_with('}') {
        return Some(Line::SectionClose);
    }

    // Check for section open: ends with `{` (possibly after a quoted arg).
    // Pattern: `name {` or `name "arg" {`
    if s.ends_with('{') {
        let body = s[..s.len() - 1].trim();
        // Does it have a quoted arg?
        if let Some(q_start) = body.find(|c| c == '"' || c == '\'') {
            let name = body[..q_start].trim();
            if let Some((arg, _)) = parse_quoted(body, q_start) {
                return Some(Line::SectionOpen { name, arg: Some(arg) });
            }
        }
        // No quoted arg
        let name = body.trim();
        return Some(Line::SectionOpen { name, arg: None });
    }

    // Key-value: `key = "value"`, `key = value`, `key "value"`, `key value`
    // Also handles `key=value` (no spaces around =).
    let s_eq = s.replace('=', " = ");
    let mut tokens = s_eq.split_whitespace();
    let key = match tokens.next() {
        Some(k) => k.trim_matches('=').to_string(),
        None => return None,
    };

    // Swallow any bare `=` token
    let mut rest_iter = tokens.peekable();
    if rest_iter.peek().map(|t| *t == "=").unwrap_or(false) {
        rest_iter.next();
    }

    // Try to get the value — could be a quoted string or bare token
    let value_start = {
        // Find position of second non-whitespace run in original `s` after the key
        let after_key = s.trim_start();
        // skip the key itself
        let after_key = &after_key[key.len()..];
        let after_key = after_key.trim_start_matches(|c: char| c == '=' || c.is_whitespace());
        after_key
    };

    if value_start.starts_with('"') || value_start.starts_with('\'') {
        if let Some((v, _)) = parse_quoted(value_start, 0) {
            return Some(Line::KeyValue { key, value: v });
        }
    }

    // Bare token value
    if let Some(v) = rest_iter.next() {
        let v = v.trim_matches(|c: char| c == '"' || c == '\'').to_string();
        return Some(Line::KeyValue { key, value: v });
    }

    // No value — bare token / flag
    Some(Line::Bare { token: key })
}

// ---------------------------------------------------------------------------
// High-level config builder
// ---------------------------------------------------------------------------

pub fn parse_kdl_config(input: &str) -> Result<Config> {
    let mut config = Config::default();

    // Section stack: Vec of section names as we nest
    let mut section_stack: Vec<String> = Vec::new();

    // Accumulators for block objects
    let mut cur_source: Option<SourceConfig> = None;
    let mut cur_monitor: Option<MonitorOverride> = None;

    for (lineno, raw) in input.lines().enumerate() {
        let token = match lex_line(raw) {
            Some(t) => t,
            None => continue,
        };

        let section = section_stack.join(".");

        match token {
            Line::SectionOpen { name, arg } => {
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
                let closed = section_stack.pop().unwrap_or_default();
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
                apply_kv(&mut config, &mut cur_source, &mut cur_monitor, &section, &key, &value)
                    .map_err(|e| anyhow::anyhow!("line {}: {}", lineno + 1, e))?;
            }

            Line::Bare { token } => {
                // Bare tokens inside `source.extensions` or similar treated as
                // list items. Otherwise ignored with a best-effort interpretation.
                if section == "source.extensions" {
                    if let Some(ref mut s) = cur_source {
                        s.extensions.push(token);
                    }
                }
                // Bare flags at other levels: ignore gracefully.
            }
        }
    }

    Ok(config)
}

fn parse_bool(v: &str) -> bool {
    matches!(v.to_lowercase().as_str(), "true" | "1" | "yes" | "on")
}

fn parse_u32(v: &str) -> Result<u32> {
    v.parse::<u32>().map_err(|_| anyhow::anyhow!("expected u32, got {:?}", v))
}

fn parse_u64(v: &str) -> Result<u64> {
    v.parse::<u64>().map_err(|_| anyhow::anyhow!("expected u64, got {:?}", v))
}

fn parse_u16(v: &str) -> Result<u16> {
    v.parse::<u16>().map_err(|_| anyhow::anyhow!("expected u16, got {:?}", v))
}

fn parse_usize(v: &str) -> Result<usize> {
    v.parse::<usize>().map_err(|_| anyhow::anyhow!("expected usize, got {:?}", v))
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
            _ => {} // forward-compatible: silently ignore unknown top-level keys
        },

        // ---- source block ----
        "source" => {
            if let Some(ref mut s) = cur_source {
                match key {
                    "path" => s.path = PathBuf::from(value),
                    "recursive" => s.recursive = parse_bool(value),
                    "min-width" | "min_width" => s.min_width = parse_u32(value)?,
                    "min-height" | "min_height" => s.min_height = parse_u32(value)?,
                    "extensions" => {
                        // Comma-separated or space-separated list on one line
                        s.extensions = value
                            .split(|c: char| c == ',' || c == ' ')
                            .map(|t| t.trim().to_lowercase())
                            .filter(|t| !t.is_empty())
                            .collect();
                    }
                    _ => {}
                }
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
                config.schedule.on_workspace_change = parse_bool(value);
            }
            "pause-when-fullscreen" | "pause_when_fullscreen" => {
                config.schedule.pause_when_fullscreen = parse_bool(value);
            }
            "pause-when-idle-secs" | "pause_when_idle_secs" => {
                config.schedule.pause_when_idle_secs = parse_u32(value)?;
            }
            "min-repeat-window" | "min_repeat_window" => {
                config.schedule.min_repeat_window = parse_usize(value)?;
            }
            _ => {}
        },

        // ---- monitor block ----
        "monitor" => {
            if let Some(ref mut m) = cur_monitor {
                match key {
                    "name" => m.name = value.to_string(),
                    "fit" => m.fit = value.to_string(),
                    "tint" => m.tint = value.to_string(),
                    _ => {}
                }
            }
        }

        // ---- transitions block ----
        "transitions" | "transition" => match key {
            "enabled" => config.transitions.enabled = parse_bool(value),
            "duration-ms" | "duration_ms" | "duration" => {
                config.transitions.duration_ms = parse_u32(value)?;
            }
            "style" => config.transitions.style = value.to_string(),
            "renderer" => config.transitions.renderer = value.to_string(),
            _ => {}
        },

        // ---- triggers block ----
        "triggers" => match key {
            "on-startup" | "on_startup" => {
                config.triggers.on_startup.push(value.to_string());
            }
            _ => {}
        },

        // ---- triggers.wiri-workspace entries ----
        "triggers.wiri-workspace" | "triggers.wiri_workspace" => {
            // key is the workspace id (as string), value is the command/folder
            if let Ok(id) = key.parse::<i32>() {
                config.triggers.on_wiri_workspace.push((id, value.to_string()));
            }
        }

        // ---- metrics block ----
        "metrics" => match key {
            "enabled" => config.metrics.enabled = parse_bool(value),
            "port" => config.metrics.port = parse_u16(value)?,
            _ => {}
        },

        // ---- cache block ----
        "cache" => match key {
            "decoded-mb" | "decoded_mb" => config.cache.decoded_mb = parse_u32(value)?,
            "prefetch-count" | "prefetch_count" => {
                config.cache.prefetch_count = parse_usize(value)?;
            }
            _ => {}
        },

        // Unknown sections: silently forward-compatible
        _ => {}
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
        assert_eq!(cfg.schedule.mode, "interval");
        assert!(cfg.transitions.enabled);
    }
}
