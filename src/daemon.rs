// パソコンに 1 つ常駐するデーモン。
// - 外側: iroh（QUIC で暗号化した P2P）。net.rs を見る。
// - 内側: 手元の通信（unix ソケット）。CLI と MCP サーバーがここに who / send を聞きに来る。
// - 配送: 宛先のセッションの受信ソケットに書く（claude.rs の post）。相手がオフラインなら outbox に置いて、つながったら送る。
// - 受信箱: Claude Code 以外（Grok Bot など）は、受信箱に貯めて check_inbox で読ませ、webhook で起こす。
// - HTTP の入り口: nenwa muse で開く（http.rs）。期限を過ぎたらここで閉じる。
use anyhow::{Result, anyhow, bail};
use interprocess::local_socket::tokio::{Stream as LocalStream, prelude::*};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::claude::{self, claude_sessions};
use crate::message::{MAX_BODY, Message, frame, is_mailbox_name, js_len, parse_address, slug};
use crate::net;
use crate::state::{self, now_iso, now_ms, read_json, write_json_private};
use crate::{http, muse, os, wake};

const MAX_DEPTH: u32 = 10; // 人の入力なしにエージェント同士が続けて返事できる回数
const RATE_PER_MIN: usize = 20; // 1 人の送り主から 1 分に受け取る数
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WAIT: f64 = 50.0; // check_inbox が待てる秒数（MCP の呼び出しが切れないうちに返す）
const WAKE_RETRY: u64 = 10 * 60_000; // 起こしてもまだ読まれていない間は、この時間がたつまで起こし直さない
const DAY: u64 = 24 * 3_600_000;

macro_rules! log {
    ($($a:tt)*) => { eprintln!("{} {}", $crate::state::now_iso(), format!($($a)*)) };
}
pub(crate) use log;

// 2 つ目のデーモンは正常に終わる
#[derive(Debug)]
pub struct AlreadyRunning;
impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("デーモンはもう動いています")
    }
}
impl std::error::Error for AlreadyRunning {}

// 覚えておく数に上限のある表。あふれたら古いものから忘れる。
pub struct Remember<T> {
    map: HashMap<String, T>,
    order: VecDeque<String>,
}
impl<T> Remember<T> {
    fn new() -> Self {
        Remember { map: HashMap::new(), order: VecDeque::new() }
    }
    pub fn set(&mut self, k: &str, v: T) {
        if self.map.insert(k.to_string(), v).is_none() {
            self.order.push_back(k.to_string());
        }
        if self.map.len() > 5000
            && let Some(old) = self.order.pop_front()
        {
            self.map.remove(&old);
        }
    }
}

// フックでつながるエージェント（Codex、Cursor、Gemini CLI、Qwen Code、OpenCode、Copilot CLI、Pi、Antigravity CLI）の会話
#[derive(Serialize, Deserialize, Clone)]
pub struct Agent {
    pub agent: String,
    pub id: String,
    pub cwd: String,
    pub label: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub seen: u64,
    // エージェント本体のプロセス（0 はまだ分からない）。消えたら、その会話は閉じたものとして扱う。
    #[serde(default)]
    pub pid: i64,
    // 直前のフックの親の並び。次のフックと共通する最初のものが本体（間のシェルは毎回変わる）。
    #[serde(default)]
    pub anc: Vec<i64>,
    // OpenCode の手元のサーバー（http://127.0.0.1:<ポート>）。ここに送ると会話に入る。
    #[serde(default)]
    pub url: String,
}

// 自分のパソコンの中の http だけを受け付ける（フックの入力で、よそへ送らされないように）
fn local_url(u: &str) -> Option<String> {
    let rest = u.strip_prefix("http://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    let (name, port) = host.rsplit_once(':')?;
    (matches!(name, "127.0.0.1" | "localhost" | "[::1]") && port.parse::<u16>().is_ok()).then(|| format!("http://{host}"))
}

fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn pids(v: &Value) -> Vec<i64> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_i64).filter(|p| *p > 1).take(8).collect()).unwrap_or_default()
}

// つながった（つながっていた）ほかのパソコン
pub struct Peer {
    pub id: String,
    pub person: String,
    pub machine: String,
    pub online: bool,
    pub last_seen: u64,
    pub sessions: Vec<Value>,
    pub tx: Option<mpsc::UnboundedSender<String>>,
    pub conn: u64,      // いま使っているつながりの番号
    pub dialer: String, // そのつながりをかけた側の鍵
    pub handle: Option<iroh::endpoint::Connection>,
}

pub struct St {
    pub peers: Vec<Peer>,
    pub known: net::Peers, // peers.json
    pub dialing: HashSet<String>,
    pub next_conn: u64,
    pub pending: HashMap<String, oneshot::Sender<String>>, // メッセージ id → 返ってきた ack の state
    depths: Remember<u32>,                                 // メッセージ id → 何往復目か（暴走よけ）
    seen: Remember<u64>,                                   // 受け取ったメッセージ id → 時刻（再送の重複よけ）
    rate: HashMap<String, Vec<u64>>,                       // 送り主 → 直近 1 分の受信時刻
    outbox: Vec<Message>,
    mailboxes: Map<String, Value>,
    waiters: HashMap<String, Vec<oneshot::Sender<()>>>, // 受信箱 → check_inbox で待っている人
    wake_state: HashMap<String, (u64, bool)>,           // 受信箱 → (最後に起こした時刻, 読まれたか)
    agents: Vec<(String, Agent)>,
    pub last_presence: Vec<Value>,
}

pub struct Daemon {
    pub person: String,
    pub machine: String,
    pub team_key: [u8; 32],
    pub network: bool,
    pub ep: OnceLock<iroh::Endpoint>,
    codex_bin: String,
    st: Mutex<St>,
    http: tokio::sync::Mutex<Option<http::Running>>,
}

// 問い合わせの項目のうち、空でない文字列だけを取り出す
fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty())
}

// JavaScript の Number(x) || 0
fn num(v: &Value) -> f64 {
    let n = match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(t) if t.trim().is_empty() => 0.0,
        Value::String(t) => t.trim().parse().unwrap_or(0.0),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    };
    if n.is_nan() { 0.0 } else { n }
}

// JavaScript の String(x || "")
fn js_str(v: &Value) -> String {
    match v {
        Value::String(t) => t.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "true".into(),
        _ => String::new(),
    }
}

fn basename(p: &str) -> String {
    Path::new(p).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn inbox_file(name: &str) -> std::path::PathBuf {
    state::inbox_dir().join(format!("{name}.json"))
}
fn read_inbox(name: &str) -> Vec<Message> {
    read_json(&inbox_file(name)).unwrap_or_default()
}
fn write_inbox(name: &str, list: &[Message]) {
    let _ = os::make_private_dir(&state::inbox_dir());
    write_json_private(&inbox_file(name), &list);
}

// { t: "msg", ...msg } の形の 1 行
pub fn wire_msg(msg: &Message) -> String {
    let mut m = Map::new();
    m.insert("t".into(), json!("msg"));
    if let Value::Object(body) = serde_json::to_value(msg).unwrap_or_default() {
        m.extend(body);
    }
    Value::Object(m).to_string() + "\n"
}

impl Daemon {
    pub fn lock(&self) -> MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn own(&self, label: &str) -> String {
        format!("{}/{label}@{}", self.person, self.machine)
    }

    // ---- 受信箱（Claude Code 以外の相手。terminal はいつもある） ----
    // 受信箱を作れるのはこのパソコンの中からだけ（register / send --as / check_inbox）。外からは作れない。
    fn register_mailbox(&self, st: &mut St, name: &str) -> Result<()> {
        if !is_mailbox_name(name) {
            bail!("受信箱の名前は英小文字・数字・. _ - で 40 文字まで（例: grok）");
        }
        if st.mailboxes.contains_key(name) {
            return Ok(());
        }
        st.mailboxes.insert(name.into(), json!({ "created": now_iso() }));
        write_json_private(&state::mailboxes_file(), &st.mailboxes);
        log!("mailbox {name}");
        Ok(())
    }

    fn store_in_inbox(&self, st: &mut St, name: &str, msg: Message) {
        let mut list = read_inbox(name);
        list.push(msg);
        let start = list.len().saturating_sub(500);
        write_inbox(name, &list[start..]);
        for w in st.waiters.remove(name).unwrap_or_default() {
            let _ = w.send(());
        }
        self.maybe_wake(st, name, &list);
    }

    fn maybe_wake(&self, st: &mut St, name: &str, list: &[Message]) {
        let now = now_ms();
        if let Some(&(at, read)) = st.wake_state.get(name)
            && !read
            && now - at < WAKE_RETRY
        {
            return;
        }
        st.wake_state.insert(name.into(), (now, false));
        let address = self.own(name);
        let mut from: Vec<&str> = vec![];
        for m in list {
            if !from.contains(&m.from.as_str()) {
                from.push(&m.from);
            }
        }
        let exe = state::exe().display().to_string();
        let payload = json!({
            "source": "nenwa",
            "mailbox": address,
            "unread": list.len(),
            "from": from,
            "text": format!("nenwa の受信箱（{address}）に新しいメッセージが {} 件あります。\
                nenwa の check_inbox で読み、返事が要るものには send_message で reply_to を付けて返してください。\
                MCP が使えないときは、下の commands をこのパソコンのローカル実行で使ってください。", list.len()),
            "commands": {
                "read": format!("{exe} inbox --as {name}"),
                "reply": format!("{exe} send --as {name} --reply-to <id> <宛先> \"<返事>\""),
            },
            "sent_at": state::iso(now),
        });
        let name = name.to_string();
        tokio::task::spawn_blocking(move || match wake::wake(&name, &payload) {
            Ok(Some(status)) => log!("wake {name} {status}"),
            Ok(None) => {}
            Err(e) => log!("wake failed {name} {e}"),
        });
    }

    async fn take_inbox(&self, name: &str, wait: f64, register: bool) -> Result<Vec<Message>> {
        let secs = wait.clamp(0.0, MAX_WAIT);
        let rx = {
            let mut st = self.lock();
            if register {
                self.register_mailbox(&mut st, name)?;
            }
            (read_inbox(name).is_empty() && secs > 0.0).then(|| {
                let (tx, rx) = oneshot::channel();
                st.waiters.entry(name.into()).or_default().push(tx);
                rx
            })
        };
        if let Some(rx) = rx {
            let _ = tokio::time::timeout(Duration::from_secs_f64(secs), rx).await;
        }
        let mut st = self.lock();
        let list = read_inbox(name);
        if !list.is_empty() {
            write_inbox(name, &[]);
        }
        if let Some(w) = st.wake_state.get_mut(name) {
            w.1 = true;
        }
        for m in &list {
            st.depths.set(&m.id, m.depth);
        }
        Ok(list)
    }

    // ---- フックでつながるエージェント（Codex、Cursor） ----
    // フックが呼ばれるたびにセッションを覚える。1 日見えなければ忘れる。
    fn live_agents<'a>(&self, st: &'a St) -> impl Iterator<Item = &'a Agent> {
        let now = now_ms();
        st.agents.iter().map(|(_, a)| a).filter(move |a| now.saturating_sub(a.seen) < DAY && (a.pid == 0 || os::alive(a.pid)))
    }

    fn save_agents(st: &St) {
        let m: Map<String, Value> = st.agents.iter().map(|(k, a)| (k.clone(), serde_json::to_value(a).unwrap_or_default())).collect();
        write_json_private(&state::agents_file(), &m);
    }

    fn touch_agent(&self, st: &mut St, agent: &str, input: &Value, status: &str, anc: Vec<i64>) -> Option<String> {
        let mut id = js_str(&input["session_id"]);
        for k in ["conversation_id", "sessionId", "conversationId"] {
            if id.is_empty() {
                id = js_str(&input[k]);
            }
        }
        if id.is_empty() {
            return None;
        }
        let key = format!("{agent}:{id}");
        let cwd = s(input, "cwd")
            .or_else(|| input["workspace_roots"].get(0).and_then(Value::as_str).filter(|x| !x.is_empty()))
            .or_else(|| input["workspacePaths"].get(0).and_then(Value::as_str).filter(|x| !x.is_empty()))
            .unwrap_or("")
            .to_string();
        // Codex の ID は先頭が時刻なので、ばらつく末尾の 4 文字を使う。
        let alnum: Vec<char> = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let tail: String = alnum[alnum.len().saturating_sub(4)..].iter().collect::<String>().to_lowercase();
        if !st.agents.iter().any(|(k, _)| *k == key) {
            let label = format!("{}-{tail}", slug(&basename(&cwd)));
            st.agents.push((
                key.clone(),
                Agent {
                    agent: agent.into(),
                    id,
                    cwd: cwd.clone(),
                    label,
                    status: String::new(),
                    seen: 0,
                    pid: 0,
                    anc: vec![],
                    url: String::new(),
                },
            ));
        }
        let now = now_ms();
        let a = &mut st.agents.iter_mut().find(|(k, _)| *k == key)?.1;
        // 空なら今の状態のまま（待ち受けの呼び出しは、作業中かどうかを変えない）
        if !status.is_empty() || a.status.is_empty() {
            a.status = if status.is_empty() { "idle" } else { status }.into();
        }
        a.seen = now;
        // 本体が入れ替わった（同じ会話を開き直した）ら、調べ直す
        if a.pid != 0 && !anc.contains(&a.pid) && !os::alive(a.pid) {
            a.pid = 0;
        }
        if a.pid == 0
            && let Some(p) = anc.iter().find(|p| a.anc.contains(p))
        {
            a.pid = *p;
        }
        if !anc.is_empty() {
            a.anc = anc;
        }
        if let Some(u) = s(input, "server_url").and_then(local_url) {
            a.url = u;
        }
        if a.cwd.is_empty() && !cwd.is_empty() {
            a.cwd = cwd;
        }
        let label = a.label.clone();
        st.agents.retain(|(_, a)| now.saturating_sub(a.seen) <= DAY);
        Self::save_agents(st);
        Some(label)
    }

    // MCP サーバーはどの会話から呼ばれたかを知らない。作業中（ツールを呼べるのは作業中）で、フォルダが合い、最近動いたものを選ぶ。
    // 呼んだ MCP サーバーの親に本体がいる会話があれば、それに決まる。
    fn resolve_agent(&self, st: &St, agent: &str, cwd: Option<&str>, anc: &[i64]) -> Result<Agent> {
        let score = |a: &Agent| {
            (if a.pid != 0 && anc.contains(&a.pid) { 4 } else { 0 })
                + (if a.status == "busy" { 2 } else { 0 })
                + (if cwd.is_some_and(|c| a.cwd == c) { 1 } else { 0 })
        };
        let mut list: Vec<&Agent> = self.live_agents(st).filter(|a| a.agent == agent && a.status != "offline").collect();
        list.sort_by(|x, y| score(y).cmp(&score(x)).then(y.seen.cmp(&x.seen)));
        list.first().map(|a| (*a).clone()).ok_or_else(|| {
            anyhow!("この {agent} のセッションがまだ nenwa に見えていません。nenwa install のあとに開いたセッションか、フックが信頼済みか（Codex は /hooks）を確かめてください")
        })
    }

    async fn deliver_to_agent(&self, a: &Agent, msg: Message) -> &'static str {
        if a.agent == "codex" {
            // 待機中でも作業中でも、codex queue なら会話に入る（待機中ならそこで動き出す）。だめならフックで渡す。
            let mut cmd = tokio::process::Command::new(&self.codex_bin);
            cmd.args(["queue", "--thread", &a.id, "--message", &frame(&msg)]);
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let ok = match cmd.spawn() {
                Ok(mut child) => match tokio::time::timeout(Duration::from_secs(20), child.wait()).await {
                    Ok(Ok(s)) if s.success() => true,
                    other => {
                        log!("codex queue failed {}: {other:?}", a.label);
                        false
                    }
                },
                Err(e) => {
                    log!("codex queue failed {}: {e}", a.label);
                    false
                }
            };
            if ok {
                return "delivered";
            }
        }
        if a.agent == "opencode" && !a.url.is_empty() {
            // OpenCode は会話ごとに手元のサーバーを持つ。待機中でも作業中でも、ここに送れば会話に入る。
            let url = format!("{}/session/{}/prompt_async?directory={}", a.url, percent(&a.id).replace('/', "%2F"), percent(&a.cwd));
            let body = json!({ "parts": [{ "type": "text", "text": frame(&msg) }] }).to_string();
            let sent = tokio::task::spawn_blocking(move || {
                let agent: ureq::Agent =
                    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).http_status_as_error(false).build().into();
                agent.post(&url).header("Content-Type", "application/json").send(&body).map(|r| r.status().as_u16())
            })
            .await;
            match sent {
                Ok(Ok(code)) if (200..300).contains(&code) => return "delivered",
                other => log!("opencode prompt failed {}: {other:?}", a.label),
            }
        }
        let mut st = self.lock();
        self.store_in_inbox(&mut st, &a.label, msg);
        "inbox"
    }

    // フックの出力。届いているメッセージがあれば、そのエージェントのやり方で会話に差し込む。
    async fn hook_event(&self, req: &Value) -> Result<Value> {
        let agent = req["agent"].as_str().unwrap_or("undefined");
        if !matches!(agent, "codex" | "cursor" | "gemini" | "qwen" | "opencode" | "copilot" | "pi" | "antigravity") {
            bail!("unknown agent: {agent}");
        }
        let event = req["event"].as_str().unwrap_or("");
        let status = match event {
            "session-start" | "stop" => "idle",
            "session-end" => "offline",
            "wait" => "",
            _ => "busy",
        };
        let Some(label) = self.touch_agent(&mut self.lock(), agent, &req["input"], status, pids(&req["ancestors"])) else {
            return Ok(Value::Null);
        };
        if event == "session-end" {
            return Ok(Value::Null);
        }
        // Cursor の beforeSubmitPrompt は会話に足せないので、メッセージは次の postToolUse か stop まで取っておく。
        // OpenCode のプラグインと Copilot CLI も、入力のときは会話に足せない。Pi は待ち受けで受け取る。
        let prompt = matches!(agent, "cursor" | "opencode" | "copilot" | "pi") && event == "user-prompt-submit";
        // Pi の拡張と OpenCode のプラグインは、会話が開いている間ここで届くのを待つ（待機中でも、届いたらそこで動き出す）。
        // OpenCode の TUI は外から叩けるサーバーを持たないので、直接は届けられない。
        let wait = if matches!(agent, "pi" | "opencode") && event == "wait" { 25.0 } else { 0.0 };
        let mail = if prompt { vec![] } else { self.take_inbox(&label, wait, false).await? };
        // OpenCode は始まったときに文脈を足す口がない（足すと、それだけで応答が始まってしまう）
        let intro = if event == "session-start" && agent != "opencode" {
            format!("nenwa: このセッションの宛先は {} です。", self.own(&label))
        } else {
            String::new()
        };
        let parts: Vec<String> = std::iter::once(intro).chain(mail.iter().map(frame)).filter(|t| !t.is_empty()).collect();
        let text = parts.join("\n\n");
        if !mail.is_empty() {
            log!("hook {agent} {event} {label} {}", mail.len());
        }
        if event == "stop" && !mail.is_empty() {
            let mut st = self.lock();
            if let Some((_, a)) = st.agents.iter_mut().find(|(_, a)| a.agent == agent && a.label == label) {
                a.status = "busy".into();
            }
        }
        if text.is_empty() {
            return Ok(if prompt && agent == "cursor" { json!({ "continue": true }) } else { Value::Null });
        }
        if matches!(agent, "opencode" | "pi") {
            return Ok(json!({ "text": text }));
        }
        if agent == "antigravity" {
            return Ok(if event == "stop" {
                json!({ "decision": "continue", "reason": text })
            } else {
                json!({ "injectSteps": [{ "ephemeralMessage": text }] })
            });
        }
        if agent == "copilot" {
            return Ok(if event == "stop" { json!({ "decision": "block", "reason": text }) } else { json!({ "additionalContext": text }) });
        }
        // Codex、Qwen Code、Gemini CLI は Claude Code と同じ形。Gemini だけイベントの名前と、続けさせるときの言葉が違う。
        if agent != "cursor" {
            let gemini = agent == "gemini";
            if event == "stop" {
                return Ok(json!({ "decision": if gemini { "deny" } else { "block" }, "reason": text }));
            }
            let mut out = Map::new();
            let name = match event {
                "session-start" => Some("SessionStart"),
                "user-prompt-submit" => Some(if gemini { "BeforeAgent" } else { "UserPromptSubmit" }),
                "post-tool-use" => Some(if gemini { "AfterTool" } else { "PostToolUse" }),
                _ => None,
            };
            if let Some(n) = name {
                out.insert("hookEventName".into(), json!(n));
            }
            out.insert("additionalContext".into(), json!(text));
            return Ok(json!({ "hookSpecificOutput": out }));
        }
        if event == "stop" {
            return Ok(json!({ "followup_message": text }));
        }
        Ok(json!({ "additional_context": text }))
    }

    // ---- 自分のセッション ----
    pub fn presence_of(&self, st: &St) -> Vec<Value> {
        let mut list: Vec<Value> =
            claude_sessions().into_iter().map(|s| json!({ "address": self.own(&s.label), "title": s.name, "status": s.status })).collect();
        list.extend(
            self.live_agents(st)
                .filter(|a| a.status != "offline")
                .map(|a| json!({ "address": self.own(&a.label), "title": a.agent, "status": a.status })),
        );
        list.extend(
            st.mailboxes
                .keys()
                .filter(|n| *n != "terminal")
                .map(|n| json!({ "address": self.own(n), "title": "受信箱", "status": "inbox" })),
        );
        list
    }

    // 配送の順: Claude Code のセッション → フックのエージェント → 受信箱
    pub async fn deliver_local(&self, msg: Message) -> &'static str {
        // msg.to はいつも正しい形（受け取るときと送るときに確かめてある）
        let Some(to) = parse_address(&msg.to) else { return "no_session" };
        if let Some(s) = claude::find_session(&to.label) {
            return match claude::post(&s.socket, &frame(&msg)).await {
                Ok(()) => "delivered",
                Err(e) => {
                    log!("deliver failed {} {e}", msg.to);
                    "no_session"
                }
            };
        }
        let agent = {
            let st = self.lock();
            self.live_agents(&st).find(|a| a.label == to.label).cloned()
        };
        if let Some(a) = agent {
            return self.deliver_to_agent(&a, msg).await;
        }
        let mut st = self.lock();
        if !st.mailboxes.contains_key(&to.label) {
            return "no_session";
        }
        self.store_in_inbox(&mut st, &to.label, msg);
        "inbox"
    }

    // ---- 仲間とのやり取り ----
    pub fn peer_for(&self, person: &str, machine: &str) -> Option<String> {
        self.lock().peers.iter().find(|p| p.online && p.person == person && p.machine == machine).map(|p| p.id.clone())
    }

    pub fn send_line(&self, peer_id: &str, line: String) {
        if let Some(tx) = self.lock().peers.iter().find(|p| p.id == peer_id).and_then(|p| p.tx.as_ref()) {
            let _ = tx.send(line);
        }
    }

    // 送って ack を待つ。返事がなければ None
    pub async fn transmit(&self, peer_id: &str, msg: &Message) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.lock().pending.insert(msg.id.clone(), tx);
        self.send_line(peer_id, wire_msg(msg));
        match tokio::time::timeout(ACK_TIMEOUT, rx).await {
            Ok(Ok(state)) => Some(state),
            _ => {
                self.lock().pending.remove(&msg.id);
                None
            }
        }
    }

    fn save_outbox(st: &St) {
        write_json_private(&state::outbox_file(), &st.outbox);
    }

    pub async fn flush_outbox(&self, peer_id: &str) {
        let items: Vec<Message> = {
            let st = self.lock();
            let Some(peer) = st.peers.iter().find(|p| p.id == peer_id) else { return };
            st.outbox
                .iter()
                .filter(|m| parse_address(&m.to).is_some_and(|to| to.person == peer.person && to.machine == peer.machine))
                .cloned()
                .collect()
        };
        for item in items {
            // 返事がなければ次につながったときにまた送る
            let Some(state) = self.transmit(peer_id, &item).await else { return };
            let mut st = self.lock();
            st.outbox.retain(|x| x.id != item.id);
            Self::save_outbox(&st);
            log!("outbox {} → {} {state}", item.id, item.to);
        }
    }

    fn allowed(st: &mut St, from: &str) -> bool {
        let now = now_ms();
        let recent = st.rate.entry(from.into()).or_default();
        recent.retain(|t| now - t < 60_000);
        recent.push(now);
        recent.len() <= RATE_PER_MIN
    }

    // 仲間から届く 1 行。中身は信用できないので、使う前に形を確かめる。
    pub fn on_peer_line(self: &Arc<Self>, peer_id: &str, m: Value) {
        match m["t"].as_str().unwrap_or("") {
            "presence" => {
                let sessions = m["sessions"].as_array().map(|a| a.iter().take(100).cloned().collect()).unwrap_or_default();
                if let Some(p) = self.lock().peers.iter_mut().find(|p| p.id == peer_id) {
                    p.sessions = sessions;
                }
            }
            "ack" => {
                let tx = m["id"].as_str().and_then(|id| self.lock().pending.remove(id));
                if let Some(tx) = tx {
                    let _ = tx.send(m["state"].as_str().unwrap_or("").to_string());
                }
            }
            "peers" => net::on_peers(self, &m["peers"]),
            "msg" => {
                let (d, peer_id) = (self.clone(), peer_id.to_string());
                tokio::spawn(async move { d.on_peer_msg(&peer_id, m).await });
            }
            _ => {}
        }
    }

    async fn on_peer_msg(&self, peer_id: &str, m: Value) {
        let ack = |state: &str| self.send_line(peer_id, json!({ "t": "ack", "id": m["id"], "state": state }).to_string() + "\n");
        let (from_s, to_s) = (js_str(&m["from"]), js_str(&m["to"]));
        let (from, to) = (parse_address(&from_s), parse_address(&to_s));
        let peer = {
            let st = self.lock();
            st.peers.iter().find(|p| p.id == peer_id).map(|p| (p.person.clone(), p.machine.clone()))
        };
        let Some((pp, pm)) = peer else { return };
        // パソコンは自分の人の名前でしか送れない。
        let (Some(from), Some(to)) = (from, to) else { return ack("refused") };
        if from.person != pp || from.machine != pm || to.person != self.person || to.machine != self.machine {
            return ack("refused");
        }
        let Some(body) = m["body"].as_str().filter(|b| js_len(b) <= MAX_BODY) else { return ack("refused") };
        let id = js_str(&m["id"]);
        {
            let mut st = self.lock();
            if st.seen.map.contains_key(&id) {
                return ack("delivered");
            }
            if !Self::allowed(&mut st, &from_s) {
                return ack("rate_limited");
            }
        }
        let depth = num(&m["depth"]).max(0.0) as u32;
        let msg = Message {
            id: id.clone(),
            from: from_s.clone(),
            to: to_s.clone(),
            reply_to: s(&m, "reply_to").map(String::from),
            body: body.into(),
            sent_at: js_str(&m["sent_at"]),
            depth,
        };
        let state = self.deliver_local(msg).await;
        if matches!(state, "delivered" | "inbox") {
            let mut st = self.lock();
            st.seen.set(&id, now_ms());
            st.depths.set(&id, depth);
        }
        log!("in {id} {from_s} → {to_s} {state}");
        ack(state);
    }

    // ---- 内側の API ----
    async fn send(&self, req: &Value) -> Result<Value> {
        let Some(target) = parse_address(&js_str(&req["to"])) else {
            bail!("宛先は <人>/<セッション>@<マシン> の形で（例: alice/api@alice-mbp）。nenwa who で確認できます");
        };
        let body = match req["body"].as_str() {
            Some(b) if !b.trim().is_empty() => b.to_string(),
            _ => bail!("本文が空です"),
        };
        if js_len(&body) > MAX_BODY {
            bail!("本文は {MAX_BODY} 文字まで");
        }
        let reply_to = s(req, "reply_to").map(String::from);
        let msg = {
            let mut st = self.lock();
            // 送り主: Codex / Cursor のセッション > 受信箱の名前（Grok Bot など）> Claude Code のセッション > ターミナル
            let hooked = match s(req, "agent") {
                Some(kind) => Some(self.resolve_agent(&st, kind, s(req, "cwd"), &pids(&req["ancestors"]))?),
                None => None,
            };
            let as_ = s(req, "as");
            if let (Some(name), None) = (as_, &hooked) {
                self.register_mailbox(&mut st, name)?;
            }
            let own = match (&hooked, as_, s(req, "fromSession")) {
                (None, None, Some(fs)) => claude_sessions().into_iter().find(|x| x.session_id == fs).map(|x| x.label),
                _ => None,
            };
            let (from, agent) = match (&hooked, as_, &own) {
                (Some(h), _, _) => (self.own(&h.label), true),
                (None, Some(name), _) => (self.own(name), name != "terminal"),
                (None, None, Some(label)) => (self.own(label), true),
                _ => (self.own("terminal"), false),
            };
            // 人（ターミナル）から送ると数え直し。エージェントの返事は 1 つ増える。
            let depth = if agent { reply_to.as_ref().and_then(|r| st.depths.map.get(r)).map_or(0, |d| d + 1) } else { 0 };
            if depth >= MAX_DEPTH {
                bail!("人の入力なしのやり取りが {MAX_DEPTH} 回続いたので止めました。続けるならユーザーに確認してください");
            }
            let msg = Message { id: state::uuid(), from, to: target.to_string(), reply_to, body, sent_at: now_iso(), depth };
            st.depths.set(&msg.id, depth);
            msg
        };
        let (id, from) = (msg.id.clone(), msg.from.clone());
        if target.person == self.person && target.machine == self.machine {
            let state = self.deliver_local(msg).await;
            return Ok(json!({ "id": id, "from": from, "state": state }));
        }
        if let Some(peer) = self.peer_for(&target.person, &target.machine)
            && let Some(state) = self.transmit(&peer, &msg).await
        {
            return Ok(json!({ "id": id, "from": from, "state": state }));
        }
        let mut st = self.lock();
        st.outbox.push(msg);
        Self::save_outbox(&st);
        Ok(
            json!({ "id": id, "from": from, "state": "queued", "note": format!("{}@{} がつながったら送ります", target.person, target.machine) }),
        )
    }

    fn who(&self) -> Value {
        let st = self.lock();
        let peers: Vec<Value> = st
            .peers
            .iter()
            .map(|p| json!({ "person": p.person, "machine": p.machine, "online": p.online, "lastSeen": state::iso(p.last_seen), "sessions": if p.online { p.sessions.clone() } else { vec![] } }))
            .collect();
        json!({
            "me": { "person": self.person, "machine": self.machine },
            "network": self.network,
            "local": st.last_presence,
            "peers": peers,
            "outbox": st.outbox.len(),
        })
    }

    async fn handle(self: &Arc<Self>, req: &Value) -> Result<Value> {
        let op = req["op"].as_str().unwrap_or("");
        Ok(match op {
            "who" => self.who(),
            "send" => self.send(req).await?,
            "register" => {
                let name = s(req, "name").unwrap_or("");
                self.register_mailbox(&mut self.lock(), name)?;
                json!({ "address": self.own(name) })
            }
            "inbox" if s(req, "agent").is_some() => {
                let a = self.resolve_agent(&self.lock(), s(req, "agent").unwrap_or(""), s(req, "cwd"), &pids(&req["ancestors"]))?;
                json!({ "address": self.own(&a.label), "messages": self.take_inbox(&a.label, num(&req["wait"]), false).await? })
            }
            "inbox" => {
                let name = s(req, "name").unwrap_or("");
                json!({ "address": self.own(name), "messages": self.take_inbox(name, num(&req["wait"]), true).await? })
            }
            "hook" => self.hook_event(req).await?,
            "http" => self.http_op(req).await?,
            // 招待に入れる自分の住所
            "addr" => match self.ep.get() {
                Some(ep) => serde_json::to_value(net::my_addr(ep).await)?,
                None => bail!("P2P がオフです"),
            },
            _ => bail!("unknown op: {op}"),
        })
    }

    // HTTP の入り口を開く（開き直す）か閉じる。開き直すと鍵を読み直し、数えていた回数も消える。
    async fn http_op(&self, req: &Value) -> Result<Value> {
        let mut cur = self.http.lock().await;
        if let Some(r) = cur.take() {
            r.stop().await;
        }
        if req["on"] != true {
            http::clear();
            log!("http off");
            return Ok(json!({ "stopped": true }));
        }
        let port = num(&req["port"]);
        if !(1.0..=65535.0).contains(&port) {
            bail!("port が正しくありません");
        }
        let e = http::Enabled { port: port as u16, expires: req["expires"].as_u64(), funnel: req["funnel"] == true };
        match http::start(&e).await {
            Ok(r) => *cur = Some(r),
            Err(err) => {
                http::clear();
                return Err(err);
            }
        }
        log!("http on 127.0.0.1:{}{}", e.port, e.expires.map_or(String::new(), |ms| format!(" until {}", state::iso(ms))));
        Ok(json!({ "port": e.port }))
    }

    // 期限を過ぎていたら、入り口と鍵と Funnel を閉じる
    async fn http_expire(&self) {
        let mut cur = self.http.lock().await;
        let Some(r) = cur.take_if(|r| r.enabled.expired(now_ms())) else { return };
        let e = r.enabled.clone();
        r.stop().await;
        Self::http_closed(&e);
    }

    fn http_closed(e: &http::Enabled) {
        // 先に書く（ファイルが消えたのを見てすぐログを読む人がいる）
        log!("http expired");
        http::clear();
        if e.funnel {
            let port = e.port;
            tokio::task::spawn_blocking(move || {
                if let Err(err) = muse::funnel_off(port) {
                    log!("funnel off failed {err}");
                }
            });
        }
    }

    async fn serve(self: Arc<Self>, c: LocalStream) {
        let mut r = BufReader::new(c);
        let mut line = String::new();
        loop {
            line.clear();
            match (&mut r).take(1 << 20).read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => break,
            }
        }
        let req: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => return reply(r.get_mut(), json!({ "ok": false, "error": e.to_string() })).await,
        };
        if req["op"] == "shutdown" {
            reply(r.get_mut(), json!({ "ok": true, "result": { "stopping": true } })).await;
            log!("shutdown requested");
            self.stop().await;
            std::process::exit(0);
        }
        let res = match self.handle(&req).await {
            Ok(result) => json!({ "ok": true, "result": result }),
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        };
        reply(r.get_mut(), res).await;
    }

    async fn stop(&self) {
        os::remove_sock(&state::sock());
        if let Some(ep) = self.ep.get() {
            let _ = tokio::time::timeout(Duration::from_secs(3), ep.close()).await;
        }
    }
}

async fn reply(c: &mut LocalStream, v: Value) {
    let _ = c.write_all((v.to_string() + "\n").as_bytes()).await;
    let _ = c.flush().await;
}

fn key32(hex: &str, what: &str) -> Result<[u8; 32]> {
    state::unhex(hex).and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("config.json の {what} が正しくありません"))
}

pub async fn start() -> Result<()> {
    os::prepare_daemon(&state::log_file());
    let config = state::load_config().ok_or_else(|| anyhow!("config.json がありません。nenwa init か join を先に"))?;
    let (team_key, seed) = (key32(&config.team_key, "teamKey")?, key32(&config.seed, "seed")?);
    let sock = state::sock();
    let name = || sock.clone().into_os_string().to_fs_name::<GenericFilePath>();

    // 前のデーモンが残したソケットは、つながらなければ消す。
    if os::sock_exists(&sock) {
        if LocalStream::connect(name()?).await.is_ok() {
            return Err(AlreadyRunning.into());
        }
        os::remove_sock(&sock);
    }

    let mut mailboxes: Map<String, Value> = read_json(&state::mailboxes_file()).unwrap_or_default();
    if !mailboxes.contains_key("terminal") {
        mailboxes.insert("terminal".into(), json!({ "created": now_iso() }));
    }
    let agents: Map<String, Value> = read_json(&state::agents_file()).unwrap_or_default();
    let st = St {
        peers: vec![],
        known: net::load_peers(),
        dialing: HashSet::new(),
        next_conn: 0,
        pending: HashMap::new(),
        depths: Remember::new(),
        seen: Remember::new(),
        rate: HashMap::new(),
        outbox: read_json(&state::outbox_file()).unwrap_or_default(),
        mailboxes,
        waiters: HashMap::new(),
        wake_state: HashMap::new(),
        agents: agents.into_iter().filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?))).collect(),
        last_presence: vec![],
    };
    // network: false（install が最初に作る形）のあいだは P2P につながない。このパソコンの中だけで動く。
    let network = config.network != Some(false);
    let d = Arc::new(Daemon {
        person: config.person,
        machine: config.machine,
        team_key,
        network,
        ep: OnceLock::new(),
        codex_bin: std::env::var("NENWA_CODEX_BIN").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "codex".into()),
        st: Mutex::new(st),
        http: tokio::sync::Mutex::new(None),
    });
    if network {
        let _ = d.ep.set(net::bind(seed).await?);
    }
    {
        let mut st = d.lock();
        st.last_presence = d.presence_of(&st);
    }

    os::make_private_dir(&state::home())?;
    let listener = ListenerOptions::new().name(name()?).create_tokio()?;
    os::set_private(&sock);
    let key = d.ep.get().map(|ep| state::hex(ep.id().as_bytes())[..8].to_string());
    log!("daemon up {}@{} {}", d.person, d.machine, key.map_or("local only".to_string(), |k| format!("p2p key {k}")));

    if d.ep.get().is_some() {
        net::start(&d);
    }

    // 自分のセッションが変わったら仲間に知らせる。
    let dd = d.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut st = dd.lock();
            let now = dd.presence_of(&st);
            if now == st.last_presence {
                continue;
            }
            st.last_presence = now.clone();
            let line = json!({ "t": "presence", "sessions": now }).to_string() + "\n";
            for p in st.peers.iter().filter(|p| p.online) {
                if let Some(tx) = &p.tx {
                    let _ = tx.send(line.clone());
                }
            }
        }
    });

    let dd = d.clone();
    tokio::spawn(async move {
        while let Ok(c) = listener.accept().await {
            tokio::spawn(dd.clone().serve(c));
        }
    });

    // nenwa muse で開いていた HTTP の入り口を開き直す（中で手元の通信を使うので、その待ち受けのあとで）
    if let Some(e) = http::load_enabled() {
        if e.expired(now_ms()) {
            Daemon::http_closed(&e);
        } else {
            match http::start(&e).await {
                Ok(r) => {
                    log!("http on 127.0.0.1:{} (reopened)", e.port);
                    *d.http.lock().await = Some(r);
                }
                Err(err) => log!("http reopen failed {err}"),
            }
        }
    }
    let dd = d.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            dd.http_expire().await;
        }
    });

    os::terminated().await;
    d.stop().await;
    std::process::exit(0);
}

// peers.json の 1 件を、hello で名乗った名前と住所で書き直す
pub fn remember_peer(st: &mut St, id: &str, person: &str, machine: &str, addr: Option<net::PeerAddr>) {
    let k = st.known.entry(id.into()).or_default();
    k.person = person.into();
    k.machine = machine.into();
    if let Some(a) = addr.filter(|a| a.id == id) {
        k.relay = a.relay;
        k.addrs = a.addrs;
    }
    k.seen = now_iso();
    net::save_peers(&st.known);
}
