// Find Claude Code sessions and push messages into them.
// An open session writes its own receiving socket (messagingSocketPath) to ~/.claude/sessions/<pid>.json.
// Writing one {"type":"user",...} line there delivers it between tool calls, and if the session is idle a turn starts there.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::message::slug;
use crate::os;

pub struct ClaudeSession {
    pub pid: i64,
    pub session_id: String,
    pub socket: String,
    pub name: String,
    pub status: String,
    pub label: String,
}

fn sessions_dir() -> PathBuf {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => os::home_dir().join(".claude"),
    };
    base.join("sessions")
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

fn basename(p: &str) -> String {
    Path::new(p).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

// The name used in the address is "folder name-first 4 characters of the session ID". It does not change when other sessions open or close.
pub fn claude_sessions() -> Vec<ClaudeSession> {
    let Ok(dir) = std::fs::read_dir(sessions_dir()) else { return vec![] };
    let mut list: Vec<ClaudeSession> = dir
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".json").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|e| serde_json::from_str::<Value>(&std::fs::read_to_string(e.path()).ok()?).ok())
        .filter(|s| {
            !text(s, "sessionId").is_empty() && !text(s, "messagingSocketPath").is_empty() && os::alive(s["pid"].as_i64().unwrap_or(0))
        })
        .map(|s| {
            let id = text(&s, "sessionId");
            ClaudeSession {
                pid: s["pid"].as_i64().unwrap_or(0),
                label: format!("{}-{}", slug(&basename(&text(&s, "cwd"))), id.chars().take(4).collect::<String>()),
                session_id: id,
                socket: text(&s, "messagingSocketPath"),
                name: text(&s, "name"),
                status: text(&s, "status"),
            }
        })
        .collect();
    list.sort_by(|a, b| a.label.cmp(&b.label));
    let mut former = FORMER.lock().unwrap_or_else(|e| e.into_inner());
    for s in &list {
        former.insert(s.label.clone(), s.pid);
    }
    former.retain(|_, pid| list.iter().any(|s| s.pid == *pid));
    list
}

// Address seen before → the process of that conversation. /clear changes the conversation ID and so the address,
// so this is remembered to deliver messages sent to an old address (a reply to the previous address, for example) to the current conversation of the same process.
static FORMER: Mutex<BTreeMap<String, i64>> = Mutex::new(BTreeMap::new());

// Find the conversation for an address. If it is not a current address, look for it as one the same process used before.
pub fn find_session(label: &str) -> Option<ClaudeSession> {
    let mut list = claude_sessions();
    if let Some(i) = list.iter().position(|s| s.label == label) {
        return Some(list.swap_remove(i));
    }
    let pid = *FORMER.lock().unwrap_or_else(|e| e.into_inner()).get(label)?;
    let i = list.iter().position(|s| s.pid == pid)?;
    Some(list.swap_remove(i))
}

// The Claude Code conversation that started the MCP server. Walk up the parent processes and read the current conversation ID.
// The environment variable keeps the ID from when the MCP server started and goes stale after /clear, so it is used only when nothing is found.
pub fn caller_session_id() -> Option<String> {
    let mut pid = os::own_parent_pid();
    for _ in 0..6 {
        if pid <= 1 {
            break;
        }
        if let Ok(t) = std::fs::read_to_string(sessions_dir().join(format!("{pid}.json")))
            && let Ok(v) = serde_json::from_str::<Value>(&t)
            && let Some(id) = v["sessionId"].as_str()
        {
            return Some(id.to_string());
        }
        let Some(parent) = os::parent_pid(pid) else { break };
        pid = parent;
    }
    std::env::var("CLAUDE_CODE_SESSION_ID").ok().filter(|id| !id.is_empty())
}

// A path starting with \\.\pipe\ is a named pipe; anything else is a unix socket (both are left to os)
pub async fn post(socket_path: &str, text: &str) -> std::io::Result<()> {
    let line = json!({ "type": "user", "message": { "role": "user", "content": text } }).to_string() + "\n";
    os::post_line(socket_path, &line).await
}
