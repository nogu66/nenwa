// Codex と Cursor の設定に nenwa の MCP サーバーとフックを足す（外す）。
// 書き換える前に一度だけ <ファイル>.bak-nenwa を残す。ほかのフックや MCP サーバーには触らない。
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

use crate::os;
use crate::state::exe;

fn dir_from(env: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(env) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => os::home_dir().join(fallback),
    }
}

fn hook_command(agent: &str, event: &str) -> String {
    format!("{} hook {agent} {event}", os::quote(&exe().to_string_lossy()))
}

// パスでは見分けない。フォルダを移したときや、入れ直したときに、古い場所を指すフックが残って重なってしまうため。
// 今の形は `…/nenwa' hook codex …`（Windows は `nenwa.exe"`）、TS 版の古い形は `…/cli.js' hook codex …`。
fn is_our_command(c: &str) -> bool {
    for agent in ["codex", "cursor"] {
        let needle = format!(" hook {agent} ");
        for (i, _) in c.match_indices(&needle) {
            let before = &c[..i];
            let Some(q) = before.chars().last().filter(|q| *q == '\'' || *q == '"') else { continue };
            let path = &before[..before.len() - 1];
            if path.ends_with("nenwa") || path.ends_with("nenwa.exe") {
                return true;
            }
            let old =
                ["cli.mjs", "cli.js", "cli.ts"].iter().any(|f| path.strip_suffix(f).is_some_and(|p| p.ends_with('/') || p.ends_with('\\')));
            if q == '\'' && old {
                return true;
            }
        }
    }
    false
}

fn is_ours(h: &Value) -> bool {
    h["command"].as_str().is_some_and(is_our_command)
}

pub struct SetupResult {
    pub agent: &'static str,
    pub files: Vec<String>,
}

fn backup(file: &Path) {
    let bak = PathBuf::from(format!("{}.bak-nenwa", file.display()));
    if file.exists() && !bak.exists() {
        let _ = std::fs::copy(file, bak);
    }
}

fn read_obj(file: &Path) -> Option<Map<String, Value>> {
    match serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn write_json(file: &Path, data: &Map<String, Value>) {
    if let Ok(t) = serde_json::to_string_pretty(data) {
        let _ = std::fs::write(file, t + "\n");
    }
}

// JavaScript の `x ||= {}`: 値が偽（なし、null、false、0、""）なら入れ直す
fn ensure_obj<'a>(m: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !m.get(key).is_some_and(|v| v.is_object()) {
        m.insert(key.into(), json!({}));
    }
    m.get_mut(key).and_then(Value::as_object_mut).expect("いま入れた")
}

// ---- Codex ----
const CODEX_EVENTS: [(&str, &str, bool, u32); 5] = [
    ("SessionStart", "session-start", true, 10),
    ("UserPromptSubmit", "user-prompt-submit", false, 10),
    ("PostToolUse", "post-tool-use", true, 10),
    ("Stop", "stop", false, 10),
    ("SessionEnd", "session-end", false, 3),
];
const TOML_START: &str = "# nenwa:start";
const TOML_END: &str = "# nenwa:end";

// \n?# nenwa:start[\s\S]*?# nenwa:end\n? を "\n" に置き換える
fn remove_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(TOML_START) {
        let Some(j) = rest[i..].find(TOML_END) else { break };
        let head = &rest[..i];
        out.push_str(head.strip_suffix('\n').unwrap_or(head));
        out.push('\n');
        let after = &rest[i + j + TOML_END.len()..];
        rest = after.strip_prefix('\n').unwrap_or(after);
    }
    out.push_str(rest);
    out
}

fn setup_codex(remove: bool) -> Option<SetupResult> {
    let dir = dir_from("NENWA_CODEX_DIR", ".codex");
    if !dir.exists() {
        return None;
    }
    let toml = dir.join("config.toml");
    let text = std::fs::read_to_string(&toml).unwrap_or_default();
    let ours = if remove {
        String::new()
    } else {
        format!(
            "\n{TOML_START}\n[mcp_servers.nenwa]\ncommand = {}\nargs = {}\ntool_timeout_sec = 120\n{TOML_END}\n",
            json!(exe().to_string_lossy()),
            json!(["mcp", "--agent", "codex"])
        )
    };
    let next = remove_blocks(&text).trim_end().to_string() + "\n" + &ours;
    if next != text {
        backup(&toml);
        let _ = std::fs::write(&toml, &next);
    }

    let hooks_file = dir.join("hooks.json");
    let mut data = read_obj(&hooks_file).unwrap_or_else(|| Map::from_iter([("hooks".into(), json!({}))]));
    let hooks = ensure_obj(&mut data, "hooks");
    for (event, name, matcher, timeout) in CODEX_EVENTS {
        let mut groups: Vec<Value> = hooks
            .get(event)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|g| {
                let mut g = g.as_object().cloned().unwrap_or_default();
                let list: Vec<Value> =
                    g.get("hooks").and_then(Value::as_array).into_iter().flatten().filter(|h| !is_ours(h)).cloned().collect();
                g.insert("hooks".into(), Value::Array(list));
                Value::Object(g)
            })
            .filter(|g| g["hooks"].as_array().is_some_and(|l| !l.is_empty()))
            .collect();
        if !remove {
            let mut g = Map::new();
            if matcher {
                g.insert("matcher".into(), json!("*"));
            }
            g.insert("hooks".into(), json!([{ "type": "command", "command": hook_command("codex", name), "timeout": timeout }]));
            groups.push(Value::Object(g));
        }
        if groups.is_empty() {
            hooks.remove(event);
        } else {
            hooks.insert(event.into(), Value::Array(groups));
        }
    }
    backup(&hooks_file);
    write_json(&hooks_file, &data);
    Some(SetupResult { agent: "codex", files: vec![toml.display().to_string(), hooks_file.display().to_string()] })
}

// ---- Cursor ----
const CURSOR_EVENTS: [(&str, &str, bool); 5] = [
    ("sessionStart", "session-start", false),
    ("beforeSubmitPrompt", "user-prompt-submit", false),
    ("postToolUse", "post-tool-use", true),
    ("stop", "stop", false),
    ("sessionEnd", "session-end", false),
];

fn setup_cursor(remove: bool) -> Option<SetupResult> {
    let dir = dir_from("NENWA_CURSOR_DIR", ".cursor");
    if !dir.exists() {
        return None;
    }
    let mcp_file = dir.join("mcp.json");
    let mut mcp = read_obj(&mcp_file).unwrap_or_else(|| Map::from_iter([("mcpServers".into(), json!({}))]));
    let servers = ensure_obj(&mut mcp, "mcpServers");
    if remove {
        servers.remove("nenwa");
    } else {
        servers
            .insert("nenwa".into(), json!({ "type": "stdio", "command": exe().to_string_lossy(), "args": ["mcp", "--agent", "cursor"] }));
    }
    backup(&mcp_file);
    write_json(&mcp_file, &mcp);

    let hooks_file = dir.join("hooks.json");
    let mut data = read_obj(&hooks_file).unwrap_or_else(|| Map::from_iter([("version".into(), json!(1)), ("hooks".into(), json!({}))]));
    let falsy = |v: Option<&Value>| match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => true,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    };
    if falsy(data.get("version")) {
        data.insert("version".into(), json!(1));
    }
    let hooks = ensure_obj(&mut data, "hooks");
    for (event, name, matcher) in CURSOR_EVENTS {
        let mut list: Vec<Value> =
            hooks.get(event).and_then(Value::as_array).into_iter().flatten().filter(|h| !is_ours(h)).cloned().collect();
        if !remove {
            let mut h = Map::new();
            h.insert("command".into(), json!(hook_command("cursor", name)));
            h.insert("timeout".into(), json!(10));
            if matcher {
                h.insert("matcher".into(), json!("*"));
            }
            list.push(Value::Object(h));
        }
        if list.is_empty() {
            hooks.remove(event);
        } else {
            hooks.insert(event.into(), Value::Array(list));
        }
    }
    backup(&hooks_file);
    write_json(&hooks_file, &data);
    Some(SetupResult { agent: "cursor", files: vec![mcp_file.display().to_string(), hooks_file.display().to_string()] })
}

pub fn setup_agents(remove: bool) -> Vec<SetupResult> {
    [setup_codex(remove), setup_cursor(remove)].into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 自分のフックの見分け方() {
        assert!(is_our_command("'/Users/a/.local/bin/nenwa' hook codex stop"));
        assert!(is_our_command("\"C:\\Users\\a\\nenwa.exe\" hook cursor stop"));
        assert!(is_our_command("'/usr/bin/node' '/old/place/src/cli.mjs' hook cursor stop"));
        assert!(!is_our_command("echo other"));
        assert!(!is_our_command("'/bin/other' hook codex stop"));
    }

    #[test]
    fn 目印の区間を消す() {
        let t = "a\n\n# nenwa:start\nx\n# nenwa:end\n";
        assert_eq!(remove_blocks(t), "a\n\n");
    }
}
