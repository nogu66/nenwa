// Add (or remove) nenwa's MCP server and hooks in the configs of Codex, Cursor, Gemini CLI, Qwen Code, OpenCode, Copilot CLI, Pi and Antigravity CLI.
// Before rewriting, keep <file>.bak-nenwa once. Other hooks and MCP servers are not touched.
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

// Do not tell them apart by path. When the folder is moved or nenwa is reinstalled, hooks pointing at the old location would remain and pile up.
// The current form is `…/nenwa' hook codex …` (`nenwa.exe"` on Windows); the old form of the TS version is `…/cli.js' hook codex …`.
fn is_our_command(c: &str) -> bool {
    for agent in ["codex", "cursor", "gemini", "qwen", "antigravity"] {
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

// JavaScript's `x ||= {}`: reinsert when the value is falsy (missing, null, false, 0, "")
fn ensure_obj<'a>(m: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !m.get(key).is_some_and(|v| v.is_object()) {
        m.insert(key.into(), json!({}));
    }
    m.get_mut(key).and_then(Value::as_object_mut).expect("just inserted")
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

// Replace \n?# nenwa:start[\s\S]*?# nenwa:end\n? with "\n"
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

// ---- Gemini CLI and Qwen Code ----
// Both keep mcpServers and hooks (nested the same way as Claude Code) in settings.json. The differences are the event names and the unit of the timeout.
const GEMINI_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start"),
    ("BeforeAgent", "user-prompt-submit"),
    ("AfterTool", "post-tool-use"),
    ("AfterAgent", "stop"),
    ("SessionEnd", "session-end"),
];
const QWEN_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PostToolUse", "post-tool-use"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

fn setup_settings_json(
    agent: &'static str,
    dir: PathBuf,
    events: &[(&str, &str)],
    timeout: u32, // milliseconds for Gemini, seconds for Qwen
    remove: bool,
) -> Option<SetupResult> {
    if !dir.exists() {
        return None;
    }
    let file = dir.join("settings.json");
    // If it cannot be parsed (comments, for example), leave it alone so that nothing is lost
    let mut data = match std::fs::read_to_string(&file) {
        Ok(t) if !t.trim().is_empty() => match serde_json::from_str(&t) {
            Ok(Value::Object(m)) => m,
            _ => {
                eprintln!("{agent}: could not parse {}, left it as it is", file.display());
                return None;
            }
        },
        _ => Map::new(),
    };
    let before = data.clone();
    if remove {
        if let Some(servers) = data.get_mut("mcpServers").and_then(Value::as_object_mut) {
            servers.remove("nenwa");
        }
    } else {
        ensure_obj(&mut data, "mcpServers")
            .insert("nenwa".into(), json!({ "command": exe().to_string_lossy(), "args": ["mcp", "--agent", agent] }));
    }
    if !remove || data.get("hooks").is_some_and(Value::is_object) {
        let hooks = ensure_obj(&mut data, "hooks");
        for (event, name) in events {
            let mut groups: Vec<Value> = hooks
                .get(*event)
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
                groups.push(
                    json!({ "hooks": [{ "type": "command", "name": "nenwa", "command": hook_command(agent, name), "timeout": timeout }] }),
                );
            }
            if groups.is_empty() {
                hooks.remove(*event);
            } else {
                hooks.insert((*event).into(), Value::Array(groups));
            }
        }
    }
    if data != before {
        backup(&file);
        write_json(&file, &data);
    }
    Some(SetupResult { agent, files: vec![file.display().to_string()] })
}

// ---- OpenCode ----
// There are no command hooks, so place a small plugin that does the same job. MCP is added to opencode.json.
const OPENCODE_PLUGIN: &str = include_str!("opencode_plugin.js");

pub fn opencode_plugin_text() -> String {
    OPENCODE_PLUGIN.replace("__NENWA_EXE__", &json!(exe().to_string_lossy()).to_string())
}

fn opencode_dir() -> PathBuf {
    match std::env::var_os("NENWA_OPENCODE_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
            Some(x) => PathBuf::from(x).join("opencode"),
            None => os::home_dir().join(".config").join("opencode"),
        },
    }
}

fn setup_opencode(remove: bool) -> Option<SetupResult> {
    let dir = opencode_dir();
    if !dir.exists() {
        return None;
    }
    // If opencode.jsonc exists, that is the one in use. If it has comments and cannot be parsed, leave it alone so that nothing is lost.
    let file = if dir.join("opencode.jsonc").exists() { dir.join("opencode.jsonc") } else { dir.join("opencode.json") };
    let mut data = match std::fs::read_to_string(&file) {
        Ok(t) if !t.trim().is_empty() => match serde_json::from_str(&t) {
            Ok(Value::Object(m)) => m,
            _ => {
                eprintln!("opencode: could not parse {}, left it as it is", file.display());
                return None;
            }
        },
        _ => Map::new(),
    };
    let before = data.clone();
    if remove {
        if let Some(servers) = data.get_mut("mcp").and_then(Value::as_object_mut) {
            servers.remove("nenwa");
        }
    } else {
        ensure_obj(&mut data, "mcp").insert(
            "nenwa".into(),
            json!({ "type": "local", "command": [exe().to_string_lossy(), "mcp", "--agent", "opencode"], "enabled": true }),
        );
    }
    if data != before {
        backup(&file);
        write_json(&file, &data);
    }
    // The plugin folder is read as either plugins or plugin. Use the one that already exists.
    let plugins = if !dir.join("plugins").exists() && dir.join("plugin").exists() { dir.join("plugin") } else { dir.join("plugins") };
    let plugin = plugins.join("nenwa.js");
    if remove {
        let _ = std::fs::remove_file(&plugin);
    } else {
        let _ = std::fs::create_dir_all(&plugins);
        let _ = std::fs::write(&plugin, opencode_plugin_text());
    }
    Some(SetupResult { agent: "opencode", files: vec![file.display().to_string(), plugin.display().to_string()] })
}

// ---- GitHub Copilot CLI ----
// Every *.json in ~/.copilot/hooks/ is read, so place one file of our own. MCP is added to mcp-config.json.
const COPILOT_EVENTS: [(&str, &str); 5] = [
    ("sessionStart", "session-start"),
    ("userPromptSubmitted", "user-prompt-submit"),
    ("postToolUse", "post-tool-use"),
    ("agentStop", "stop"),
    ("sessionEnd", "session-end"),
];

fn copilot_dir() -> PathBuf {
    match std::env::var_os("NENWA_COPILOT_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => match std::env::var_os("COPILOT_HOME").filter(|d| !d.is_empty()) {
            Some(x) => PathBuf::from(x),
            None => os::home_dir().join(".copilot"),
        },
    }
}

fn setup_copilot(remove: bool) -> Option<SetupResult> {
    let dir = copilot_dir();
    if !dir.exists() {
        return None;
    }
    let mcp_file = dir.join("mcp-config.json");
    let mut mcp = match std::fs::read_to_string(&mcp_file) {
        Ok(t) if !t.trim().is_empty() => match serde_json::from_str(&t) {
            Ok(Value::Object(m)) => m,
            _ => {
                eprintln!("copilot: could not parse {}, left it as it is", mcp_file.display());
                return None;
            }
        },
        _ => Map::new(),
    };
    let before = mcp.clone();
    if remove {
        if let Some(servers) = mcp.get_mut("mcpServers").and_then(Value::as_object_mut) {
            servers.remove("nenwa");
        }
    } else {
        ensure_obj(&mut mcp, "mcpServers").insert(
            "nenwa".into(),
            json!({ "type": "local", "command": exe().to_string_lossy(), "args": ["mcp", "--agent", "copilot"], "tools": ["*"] }),
        );
    }
    if mcp != before {
        backup(&mcp_file);
        write_json(&mcp_file, &mcp);
    }
    let hooks_file = dir.join("hooks").join("nenwa.json");
    if remove {
        let _ = std::fs::remove_file(&hooks_file);
    } else {
        let path = exe().to_string_lossy().into_owned();
        let (sh, ps) = (format!("'{}'", path.replace('\'', "'\\''")), format!("& '{}'", path.replace('\'', "''")));
        let hooks: Map<String, Value> = COPILOT_EVENTS
            .iter()
            .map(|(event, name)| {
                let h = json!({
                    "type": "command",
                    "bash": format!("{sh} hook copilot {name}"),
                    "powershell": format!("{ps} hook copilot {name}"),
                    "timeoutSec": 10
                });
                (event.to_string(), json!([h]))
            })
            .collect();
        let _ = std::fs::create_dir_all(dir.join("hooks"));
        write_json(&hooks_file, &Map::from_iter([("version".to_string(), json!(1)), ("hooks".to_string(), Value::Object(hooks))]));
    }
    Some(SetupResult { agent: "copilot", files: vec![mcp_file.display().to_string(), hooks_file.display().to_string()] })
}

// ---- Pi ----
// Pi has neither MCP nor command hooks, so place one extension that does the job of both the tools and the hooks.
const PI_EXTENSION: &str = include_str!("pi_extension.ts");

fn pi_dir() -> PathBuf {
    match std::env::var_os("NENWA_PI_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => match std::env::var_os("PI_CODING_AGENT_DIR").filter(|d| !d.is_empty()) {
            Some(x) => PathBuf::from(x),
            None => os::home_dir().join(".pi").join("agent"),
        },
    }
}

fn setup_pi(remove: bool) -> Option<SetupResult> {
    let dir = pi_dir();
    if !dir.exists() {
        return None;
    }
    let file = dir.join("extensions").join("nenwa.ts");
    if remove {
        let _ = std::fs::remove_file(&file);
    } else {
        let _ = std::fs::create_dir_all(dir.join("extensions"));
        let _ = std::fs::write(&file, PI_EXTENSION.replace("__NENWA_EXE__", &json!(exe().to_string_lossy()).to_string()));
    }
    Some(SetupResult { agent: "pi", files: vec![file.display().to_string()] })
}

// ---- Antigravity CLI (successor to Gemini CLI. The hook format is different) ----
// The config is mcp_config.json and hooks.json in ~/.gemini/config/. The top level of hooks.json is the "hook name".
// There is no SessionEnd or input-time event, so insert before the model is called (PreInvocation).
const ANTIGRAVITY_EVENTS: [(&str, &str); 3] = [("SessionStart", "session-start"), ("PreInvocation", "post-tool-use"), ("Stop", "stop")];

fn antigravity_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("NENWA_ANTIGRAVITY_DIR").filter(|d| !d.is_empty()) {
        let d = PathBuf::from(d);
        return d.exists().then_some(d);
    }
    // Installing the CLI creates ~/.gemini/antigravity-cli (do not touch it when only the desktop app is installed)
    let gemini = os::home_dir().join(".gemini");
    (gemini.join("antigravity-cli").exists() || gemini.join("config").exists()).then(|| gemini.join("config"))
}

fn setup_antigravity(remove: bool) -> Option<SetupResult> {
    let dir = antigravity_dir()?;
    let read = |file: &Path| -> Option<Map<String, Value>> {
        match std::fs::read_to_string(file) {
            Ok(t) if !t.trim().is_empty() => match serde_json::from_str(&t) {
                Ok(Value::Object(m)) => Some(m),
                _ => {
                    eprintln!("antigravity: could not parse {}, left it as it is", file.display());
                    None
                }
            },
            _ => Some(Map::new()),
        }
    };
    let (mcp_file, hooks_file) = (dir.join("mcp_config.json"), dir.join("hooks.json"));
    let (mut mcp, mut hooks) = (read(&mcp_file)?, read(&hooks_file)?);
    let (mcp_before, hooks_before) = (mcp.clone(), hooks.clone());
    if remove {
        if let Some(servers) = mcp.get_mut("mcpServers").and_then(Value::as_object_mut) {
            servers.remove("nenwa");
        }
        hooks.remove("nenwa");
    } else {
        ensure_obj(&mut mcp, "mcpServers")
            .insert("nenwa".into(), json!({ "command": exe().to_string_lossy(), "args": ["mcp", "--agent", "antigravity"] }));
        let ours: Map<String, Value> = ANTIGRAVITY_EVENTS
            .iter()
            .map(|(event, name)| {
                (event.to_string(), json!([{ "type": "command", "command": hook_command("antigravity", name), "timeout": 10 }]))
            })
            .collect();
        hooks.insert("nenwa".into(), Value::Object(ours));
    }
    let _ = std::fs::create_dir_all(&dir);
    for (file, data, before) in [(&mcp_file, &mcp, &mcp_before), (&hooks_file, &hooks, &hooks_before)] {
        if data != before {
            backup(file);
            write_json(file, data);
        }
    }
    Some(SetupResult { agent: "antigravity", files: vec![mcp_file.display().to_string(), hooks_file.display().to_string()] })
}

// For doctor: whether nenwa is in that agent's config. None if there is no config folder.
pub fn registered(agent: &str) -> Option<bool> {
    let has = |file: PathBuf, needle: &str| std::fs::read_to_string(file).is_ok_and(|t| t.contains(needle));
    // Other tools also have hooks shaped like "hook codex", so count only those that end with the nenwa executable
    let ours = |file: PathBuf, agent: &str| {
        ["nenwa' hook", "nenwa\\\" hook", "nenwa.exe\\\" hook", "nenwa.exe' hook"]
            .iter()
            .any(|q| has(file.clone(), &format!("{q} {agent} ")))
    };
    let hooked = |dir: PathBuf, file: &str, agent: &str| dir.exists().then(|| ours(dir.join(file), agent));
    match agent {
        "codex" => hooked(dir_from("NENWA_CODEX_DIR", ".codex"), "hooks.json", "codex"),
        "cursor" => hooked(dir_from("NENWA_CURSOR_DIR", ".cursor"), "hooks.json", "cursor"),
        "gemini" => hooked(dir_from("NENWA_GEMINI_DIR", ".gemini"), "settings.json", "gemini"),
        "qwen" => hooked(dir_from("NENWA_QWEN_DIR", ".qwen"), "settings.json", "qwen"),
        "antigravity" => antigravity_dir().map(|d| ours(d.join("hooks.json"), "antigravity")),
        "copilot" => Some(copilot_dir()).filter(|d| d.exists()).map(|d| d.join("hooks").join("nenwa.json").exists()),
        "opencode" => Some(opencode_dir())
            .filter(|d| d.exists())
            .map(|d| d.join("plugins").join("nenwa.js").exists() || d.join("plugin").join("nenwa.js").exists()),
        "pi" => Some(pi_dir()).filter(|d| d.exists()).map(|d| d.join("extensions").join("nenwa.ts").exists()),
        _ => None,
    }
}

pub fn setup_agents(remove: bool) -> Vec<SetupResult> {
    [
        setup_codex(remove),
        setup_cursor(remove),
        setup_settings_json("gemini", dir_from("NENWA_GEMINI_DIR", ".gemini"), &GEMINI_EVENTS, 10_000, remove),
        setup_settings_json("qwen", dir_from("NENWA_QWEN_DIR", ".qwen"), &QWEN_EVENTS, 10, remove),
        setup_opencode(remove),
        setup_copilot(remove),
        setup_pi(remove),
        setup_antigravity(remove),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_own_hooks() {
        assert!(is_our_command("'/Users/a/.local/bin/nenwa' hook codex stop"));
        assert!(is_our_command("\"C:\\Users\\a\\nenwa.exe\" hook cursor stop"));
        assert!(is_our_command("'/usr/bin/node' '/old/place/src/cli.mjs' hook cursor stop"));
        assert!(!is_our_command("echo other"));
        assert!(!is_our_command("'/bin/other' hook codex stop"));
        assert!(is_our_command("'/Users/a/.local/bin/nenwa' hook gemini stop"));
        assert!(is_our_command("'/Users/a/.local/bin/nenwa' hook qwen session-start"));
    }

    #[test]
    fn removes_marked_section() {
        let t = "a\n\n# nenwa:start\nx\n# nenwa:end\n";
        assert_eq!(remove_blocks(t), "a\n\n");
    }
}
