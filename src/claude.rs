// Claude Code のセッションを見つけて、メッセージを押し込む。
// 開いているセッションは ~/.claude/sessions/<pid>.json に自分の受信ソケット（messagingSocketPath）を書いている。
// そこに {"type":"user",...} を 1 行書くと、ツール呼び出しの合間に届き、待機中ならそこでターンが始まる。
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::message::slug;
use crate::os;

pub struct ClaudeSession {
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

// 宛先に使う名前は「フォルダ名-セッション ID の先頭 4 文字」。ほかのセッションが開いても閉じても変わらない。
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
                label: format!("{}-{}", slug(&basename(&text(&s, "cwd"))), id.chars().take(4).collect::<String>()),
                session_id: id,
                socket: text(&s, "messagingSocketPath"),
                name: text(&s, "name"),
                status: text(&s, "status"),
            }
        })
        .collect();
    list.sort_by(|a, b| a.label.cmp(&b.label));
    list
}

// MCP サーバーを起動した Claude Code の会話。環境変数が一番確か。なければ親プロセスをさかのぼる。
pub fn caller_session_id() -> Option<String> {
    if let Ok(id) = std::env::var("CLAUDE_CODE_SESSION_ID")
        && !id.is_empty()
    {
        return Some(id);
    }
    let mut pid = os::own_parent_pid();
    for _ in 0..6 {
        if pid <= 1 {
            break;
        }
        if let Ok(t) = std::fs::read_to_string(sessions_dir().join(format!("{pid}.json")))
            && let Ok(v) = serde_json::from_str::<Value>(&t)
        {
            return v["sessionId"].as_str().map(String::from);
        }
        pid = os::parent_pid(pid)?;
    }
    None
}

// パスが \\.\pipe\ で始まれば名前付きパイプ、それ以外は unix ソケット（どちらも os に任せる）
pub async fn post(socket_path: &str, text: &str) -> std::io::Result<()> {
    let line = json!({ "type": "user", "message": { "role": "user", "content": text } }).to_string() + "\n";
    os::post_line(socket_path, &line).await
}
