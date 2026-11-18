// 1 台の上で 2 人分（alice と bob）のデーモンを立て、偽の Claude Code セッションに届くまでを確かめる。
// テストは前のテストの状態を使うので、書いた順に 1 つずつ動かす（自前の小さなランナー。Cargo.toml の harness = false）。
// 本物のホームには何も書かない: NENWA_HOME、HOME（Windows は USERPROFILE）、CLAUDE_CONFIG_DIR をすべて一時フォルダに向け、
// P2P は NENWA_RELAY=disabled。偽の Claude Code セッションは、Mac と Linux は unix ソケット、Windows は名前付きパイプ。
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream as LocalStream, ToFsName, prelude::*};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CLI: &str = env!("CARGO_BIN_EXE_nenwa");

fn sleep(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

fn rand_hex(n: usize) -> String {
    (0..n).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}

// デーモンの手元の通信の場所（src/os と同じ決め方）
fn daemon_sock(h: &Home) -> String {
    if cfg!(windows) {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(h.get("NENWA_HOME").as_bytes());
        let hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!(r"\\.\pipe\nenwa-{}-{hex}", std::env::var("USERNAME").unwrap_or_default())
    } else {
        h.home.join("sock").display().to_string()
    }
}

fn local_connect(path: &str) -> std::io::Result<LocalStream> {
    LocalStream::connect(path.to_fs_name::<GenericFilePath>()?)
}

// デーモンに shutdown を頼む（自動で起動したものは pid が分からないので、こちらで止める）
fn shutdown(h: &Home) {
    if let Ok(mut s) = local_connect(&daemon_sock(h)) {
        let _ = s.write_all(b"{\"op\":\"shutdown\"}\n");
        let _ = s.read_to_string(&mut String::new());
    }
}

#[derive(Clone)]
struct Home {
    home: PathBuf,
    claude: PathBuf,
    env: Vec<(String, String)>,
}

impl Home {
    fn with(&self, extra: &[(&str, &str)]) -> Home {
        let mut h = self.clone();
        h.env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        h
    }
    fn get(&self, key: &str) -> String {
        self.env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(CLI);
        c.args(args);
        for k in ["NENWA_AS", "NENWA_AGENT", "NENWA_WAKE_KEY", "NENWA_CODEX_DIR", "NENWA_CURSOR_DIR", "XDG_CONFIG_HOME"] {
            c.env_remove(k);
        }
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }
}

type Test = (&'static str, fn(&Ctx));

struct Ctx {
    root: PathBuf,
    team_key: String,
    fake_codex: PathBuf,
    alice: Home,
    bob: Home,
    bob_session: Session,
    daemons: Mutex<HashMap<&'static str, Child>>,
}

impl Ctx {
    fn make_home(&self, name: &str, config: bool) -> Home {
        make_home(&self.root, &self.fake_codex, &self.team_key, name, config)
    }
}

fn make_home(root: &Path, fake_codex: &Path, team_key: &str, name: &str, config: bool) -> Home {
    let home = root.join(name);
    let claude = home.join("claude");
    std::fs::create_dir_all(claude.join("sessions")).unwrap();
    std::fs::create_dir_all(home.join("fake-home")).unwrap();
    if config {
        let c = json!({ "person": name, "machine": format!("{name}-mac"), "teamKey": team_key, "seed": rand_hex(32) });
        std::fs::write(home.join("config.json"), c.to_string()).unwrap();
    }
    let p = |x: &Path| x.display().to_string();
    let env = vec![
        ("NENWA_HOME", p(&home)),
        ("HOME", p(&home.join("fake-home"))),
        ("USERPROFILE", p(&home.join("fake-home"))),
        ("CLAUDE_CONFIG_DIR", p(&claude)),
        ("CLAUDE_CODE_SESSION_ID", String::new()),
        ("NENWA_RELAY", "disabled".into()),
        ("NENWA_CODEX_BIN", p(fake_codex)),
        ("FAKE_CODEX_LOG", p(&home.join("codex-queue.log"))),
        ("FAKE_CODEX_FAIL", p(&home.join("codex-fail"))),
    ];
    Home { home, claude, env: env.into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
}

fn cli(h: &Home, args: &[&str]) -> Result<String, String> {
    let out = h.command(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn cli_json(h: &Home, args: &[&str]) -> Value {
    serde_json::from_str(&cli(h, args).unwrap()).unwrap()
}

// フックを起動して stdin に JSON を渡し、stdout の JSON（なければ None）を返す。
fn hook_call(h: &Home, agent: &str, event: &str, input: Value) -> Option<Value> {
    let mut p = h.command(&["hook", agent, event]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    p.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = p.wait_with_output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status.code());
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then(|| serde_json::from_str(&text).unwrap())
}

// MCP サーバーを起動して JSON-RPC を流し、返ってきた分を集める。
fn mcp_call(h: &Home, requests: &[Value]) -> Vec<Value> {
    let mut p = h.command(&["mcp"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = p.stdin.take().unwrap();
    for r in requests {
        writeln!(stdin, "{r}").unwrap();
    }
    let mut out = vec![];
    for line in BufReader::new(p.stdout.take().unwrap()).lines() {
        out.push(serde_json::from_str(&line.unwrap()).unwrap());
        if out.len() == requests.len() {
            break;
        }
    }
    let _ = p.kill();
    let _ = p.wait();
    out
}

fn by_id(out: &[Value], id: i64) -> Value {
    out.iter().find(|r| r["id"] == id).cloned().unwrap()
}

// f が Some を返すまで待ち、その値を返す
fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let ms = std::env::var("NENWA_TEST_WAIT").ok().and_then(|v| v.parse().ok()).unwrap_or(60_000);
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timeout");
        sleep(200);
    }
}

fn who_has(h: &Home, text: &str) -> bool {
    cli(h, &["who"]).is_ok_and(|o| o.contains(text))
}

// 偽のセッション: 受信ソケットを開き、届いた行をためる。pid はこのテスト自身（生きているので一覧に出る）。
#[derive(Clone)]
struct Session {
    got: Arc<Mutex<Vec<Value>>>,
}

impl Session {
    fn len(&self) -> usize {
        self.got.lock().unwrap().len()
    }
    fn last(&self) -> Option<Value> {
        self.got.lock().unwrap().last().cloned()
    }
    fn after(&self, before: usize) -> Option<Value> {
        (self.len() > before).then(|| self.last()).flatten()
    }
}

fn fake_session(h: &Home, folder: &str, session_id: &str) -> Session {
    let sock = if cfg!(windows) {
        format!(r"\\.\pipe\nenwa-test-{}-{folder}", rand_hex(6))
    } else {
        h.home.join(format!("{folder}.sock")).display().to_string()
    };
    let listener = ListenerOptions::new().name(sock.as_str().to_fs_name::<GenericFilePath>().unwrap()).create_sync().unwrap();
    let got = Arc::new(Mutex::new(vec![]));
    let g = got.clone();
    std::thread::spawn(move || {
        for mut c in listener.incoming().flatten() {
            // 相手が閉じるまで読む（Windows のパイプは閉じるとエラーで返る）
            let (mut buf, mut chunk) = (vec![], [0u8; 4096]);
            while let Ok(n @ 1..) = c.read(&mut chunk) {
                buf.extend_from_slice(&chunk[..n]);
            }
            g.lock().unwrap().push(serde_json::from_slice(&buf).unwrap());
        }
    });
    let pid = std::process::id();
    let s = json!({ "pid": pid, "sessionId": session_id, "cwd": format!("/work/{folder}"), "name": format!("{folder} の作業"), "status": "idle", "messagingSocketPath": sock });
    std::fs::write(h.claude.join("sessions").join(format!("{pid}.json")), s.to_string()).unwrap();
    Session { got }
}

fn start_daemon(h: &Home) -> Child {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(h.home.join("test-daemon.log")).unwrap();
    h.command(&["daemon"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log).spawn().unwrap()
}

// SIGTERM で止める（デーモンは仲間とのつながりを閉じてから終わる）。Windows は shutdown を頼む。
fn stop_daemon(mut p: Child, h: &Home) {
    if cfg!(windows) {
        shutdown(h);
    } else {
        let _ = Command::new("kill").arg("-TERM").arg(p.id().to_string()).status();
    }
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if let Ok(Some(_)) = p.try_wait() {
            return;
        }
        sleep(50);
    }
    let _ = p.kill();
    let _ = p.wait();
}

// 偽の webhook: 届いた要求のヘッダーと本文をためる
type Hooks = Arc<Mutex<Vec<(HashMap<String, String>, Value)>>>;
fn webhook_server() -> (String, Hooks) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/hook", listener.local_addr().unwrap().port());
    let hooks: Hooks = Arc::default();
    let h = hooks.clone();
    std::thread::spawn(move || {
        for c in listener.incoming().flatten() {
            let mut r = BufReader::new(c);
            let mut headers = HashMap::new();
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = line.trim().split_once(':') {
                    headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                }
            }
            let n: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; n];
            let _ = r.read_exact(&mut body);
            h.lock().unwrap().push((headers, serde_json::from_slice(&body).unwrap_or(Value::Null)));
            let _ = r.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    (url, hooks)
}

fn before() -> Ctx {
    let root = std::env::temp_dir().join(format!("nenwa-e2e-{}-{}", std::process::id(), rand_hex(3)));
    std::fs::create_dir_all(&root).unwrap();
    let team_key = rand_hex(32);
    // 偽の codex はこのテストの実行ファイル自身（main の先頭を見る）
    let fake_codex = std::env::current_exe().unwrap();

    // alice がネットワークを持ち、bob は alice の招待で入る（install は飛ばす）
    let alice = make_home(&root, &fake_codex, &team_key, "alice", true);
    let bob = make_home(&root, &fake_codex, &team_key, "bob", false);
    let mut daemons = HashMap::new();
    daemons.insert("alice", start_daemon(&alice));
    let invite = until(|| cli(&alice, &["invite"]).ok());
    let code = invite.split_whitespace().last().unwrap().to_string();
    cli(&bob.with(&[("NENWA_TEST_NO_INSTALL", "1")]), &["join", &code, "--person", "bob", "--machine", "bob-mac"]).unwrap();
    let bob_session = fake_session(&bob, "api", "bbbb1111-0000-0000-0000-000000000000");
    daemons.insert("bob", start_daemon(&bob));
    Ctx { root, team_key, fake_codex, alice, bob, bob_session, daemons: Mutex::new(daemons) }
}

fn after(ctx: Ctx) {
    for (name, p) in ctx.daemons.lock().unwrap().drain() {
        stop_daemon(p, if name == "alice" { &ctx.alice } else { &ctx.bob });
    }
    if std::env::var("NENWA_TEST_KEEP").is_err() {
        let _ = std::fs::remove_dir_all(&ctx.root);
    }
}

fn 相手のセッションが一覧に出る(c: &Ctx) {
    let out = until(|| cli(&c.alice, &["who"]).ok().filter(|o| o.contains("bob/api-bbbb@bob-mac")));
    assert!(re(r"bob@bob-mac\s+online").is_match(&out), "{out}");
    assert!(out.contains("api の作業"));
}

fn ターミナルから送ると相手のセッションに枠つきで届く(c: &Ctx) {
    let res = cli_json(&c.alice, &["send", "bob/api-bbbb@bob-mac", "トークンの形式を変えた？"]);
    assert_eq!(res["state"], "delivered");
    assert_eq!(res["from"], "alice/terminal@alice-mac");
    let line = until(|| c.bob_session.last());
    assert_eq!(line["type"], "user");
    assert_eq!(line["message"]["role"], "user");
    let content = line["message"]["content"].as_str().unwrap();
    assert!(
        re(r#"<nenwa-message from="alice/terminal@alice-mac" id="[0-9a-f-]+">\nトークンの形式を変えた？\n</nenwa-message>"#)
            .is_match(content),
        "{content}"
    );
}

fn いないセッション宛てはno_session(c: &Ctx) {
    let res = cli_json(&c.alice, &["send", "bob/nope@bob-mac", "hi"]);
    assert_eq!(res["state"], "no_session");
}

// ---- 受信箱（Grok Bot など Claude Code 以外の相手） ----

fn 受信箱に届くとwebhookで起こし_check_inbox相当で読める(c: &Ctx) {
    let (url, hooks) = webhook_server();
    cli(&c.alice.with(&[("NENWA_WAKE_KEY", "test-key-123")]), &["wake", "grok", &url]).unwrap();

    // bob から見ても宛先として一覧に出る
    until(|| who_has(&c.bob, "alice/grok@alice-mac").then_some(()));

    let sent = cli_json(&c.bob, &["send", "alice/grok@alice-mac", "このテストを直して"]);
    assert_eq!(sent["state"], "inbox");
    let (headers, body) = until(|| hooks.lock().unwrap().first().cloned());
    assert_eq!(headers["authorization"], "Bearer test-key-123");
    assert_eq!(body["source"], "nenwa");
    assert_eq!(body["mailbox"], "alice/grok@alice-mac");
    assert_eq!(body["unread"], 1);
    assert_eq!(body["from"], json!(["bob/terminal@bob-mac"]));
    assert!(!body.to_string().contains("このテストを直して"), "本文は webhook に載せない");

    // まだ読まれていない間は起こし直さない
    cli(&c.bob, &["send", "alice/grok@alice-mac", "もう 1 件"]).unwrap();
    sleep(500);
    assert_eq!(hooks.lock().unwrap().len(), 1);

    let bx = cli_json(&c.alice, &["inbox", "--as", "grok"]);
    let bodies: Vec<&str> = bx["messages"].as_array().unwrap().iter().map(|m| m["body"].as_str().unwrap()).collect();
    assert_eq!(bodies, ["このテストを直して", "もう 1 件"]);
    assert_eq!(cli_json(&c.alice, &["inbox", "--as", "grok"])["messages"].as_array().unwrap().len(), 0, "読んだ分は消える");

    // 読んだあとに届いたら、また起こす
    cli(&c.bob, &["send", "alice/grok@alice-mac", "3 件目"]).unwrap();
    until(|| (hooks.lock().unwrap().len() == 2).then_some(()));
    cli(&c.alice, &["inbox", "--as", "grok"]).unwrap();
}

fn 空の受信箱は_waitで届くまで待つ(c: &Ctx) {
    let waiting = c.alice.command(&["inbox", "--as", "grok", "--wait", "20"]).stdout(Stdio::piped()).spawn().unwrap();
    sleep(500);
    cli(&c.bob, &["send", "alice/grok@alice-mac", "待っていた分"]).unwrap();
    let out = waiting.wait_with_output().unwrap();
    let bx: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(bx["messages"][0]["body"], "待っていた分");
}

fn 受信箱の名前で返事をすると_相手のセッションに届く(c: &Ctx) {
    let sent = cli_json(&c.bob, &["send", "alice/grok@alice-mac", "質問です"]);
    let bx = cli_json(&c.alice, &["inbox", "--as", "grok"]);
    assert_eq!(bx["messages"][0]["id"], sent["id"]);
    let before = c.bob_session.len();
    let id = sent["id"].as_str().unwrap();
    let res = cli_json(&c.alice, &["send", "--as", "grok", "--reply-to", id, "bob/api-bbbb@bob-mac", "答えです"]);
    assert_eq!(res["from"], "alice/grok@alice-mac");
    assert_eq!(res["state"], "delivered");
    let line = until(|| c.bob_session.after(before));
    let content = line["message"]["content"].as_str().unwrap();
    assert!(re(&format!(r#"from="alice/grok@alice-mac" id="[0-9a-f-]+" reply_to="{id}""#)).is_match(content), "{content}");
}

fn ターミナルからの送信への返事はterminalの受信箱に入る(c: &Ctx) {
    let sent = cli_json(&c.alice, &["send", "--as", "grok", "bob/terminal@bob-mac", "ターミナルさんへ"]);
    assert_eq!(sent["state"], "inbox");
    let bx = cli_json(&c.bob, &["inbox"]);
    assert_eq!(bx["messages"].as_array().unwrap().last().unwrap()["body"], "ターミナルさんへ");
}

fn mcp_nenwa_asを付けたときだけcheck_inboxが出る(c: &Ctx) {
    let tools = |h: &Home| -> Vec<String> {
        let out = mcp_call(
            h,
            &[
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            ],
        );
        by_id(&out, 2)["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(tools(&c.alice), ["list_team", "send_message"]);
    assert_eq!(tools(&c.alice.with(&[("NENWA_AS", "grok")])), ["list_team", "send_message", "check_inbox"]);

    cli(&c.bob, &["send", "alice/grok@alice-mac", "MCP で読んで"]).unwrap();
    let out = mcp_call(
        &c.alice.with(&[("NENWA_AS", "grok")]),
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "check_inbox", "arguments": {} } }),
        ],
    );
    let text = by_id(&out, 3)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\nMCP で読んで\n</nenwa-message>"#).is_match(&text), "{text}");
}

// ---- フックでつながるエージェント（Cursor、Codex） ----

const CURSOR_ID: &str = "c0ffee00-1111-2222-3333-44445555beef";
const CURSOR_ADDR: &str = "alice/web-beef@alice-mac";

fn cursor_session_startで宛先が決まり_post_tool_useとstopで届く(c: &Ctx) {
    let start = hook_call(
        &c.alice,
        "cursor",
        "session-start",
        json!({ "conversation_id": CURSOR_ID, "session_id": CURSOR_ID, "workspace_roots": ["/work/web"] }),
    )
    .unwrap();
    assert!(start["additional_context"].as_str().unwrap().contains(&format!("宛先は {CURSOR_ADDR}")));
    until(|| who_has(&c.bob, CURSOR_ADDR).then_some(()));

    let sent = cli_json(&c.bob, &["send", CURSOR_ADDR, "画面のバグを見て"]);
    assert_eq!(sent["state"], "inbox");
    // beforeSubmitPrompt は会話に足せないので取っておく
    assert_eq!(
        hook_call(&c.alice, "cursor", "user-prompt-submit", json!({ "conversation_id": CURSOR_ID, "prompt": "x" })),
        Some(json!({ "continue": true }))
    );
    let post = hook_call(&c.alice, "cursor", "post-tool-use", json!({ "conversation_id": CURSOR_ID, "tool_name": "Shell" })).unwrap();
    assert!(
        re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\n画面のバグを見て"#).is_match(post["additional_context"].as_str().unwrap())
    );
    assert_eq!(hook_call(&c.alice, "cursor", "post-tool-use", json!({ "conversation_id": CURSOR_ID })), None, "同じものは二度出さない");

    cli(&c.bob, &["send", CURSOR_ADDR, "終わったら返事を"]).unwrap();
    let stop =
        hook_call(&c.alice, "cursor", "stop", json!({ "conversation_id": CURSOR_ID, "status": "completed", "loop_count": 0 })).unwrap();
    assert!(stop["followup_message"].as_str().unwrap().contains("終わったら返事を"));
    assert_eq!(
        hook_call(&c.alice, "cursor", "stop", json!({ "conversation_id": CURSOR_ID, "status": "completed", "loop_count": 1 })),
        None
    );
}

fn cursor_mcp_agent_cursorから送ると_そのセッションが送り主になる(c: &Ctx) {
    hook_call(&c.alice, "cursor", "post-tool-use", json!({ "conversation_id": CURSOR_ID })); // 作業中にする
    let before = c.bob_session.len();
    let out = mcp_call(
        &c.alice.with(&[("NENWA_AGENT", "cursor")]),
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "send_message", "arguments": { "to": "bob/api-bbbb@bob-mac", "message": "Cursor から" } } }),
        ],
    );
    let res: Value = serde_json::from_str(by_id(&out, 2)["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(res["from"], CURSOR_ADDR);
    assert_eq!(res["state"], "delivered");
    let line = until(|| c.bob_session.after(before));
    assert!(line["message"]["content"].as_str().unwrap().contains(&format!("from=\"{CURSOR_ADDR}\"")));
}

const CODEX_ID: &str = "019a2b3c-0000-7000-8000-00000000c0de";
const CODEX_ADDR: &str = "alice/api-c0de@alice-mac";

fn codex_codex_queueで会話に入れる_失敗したらフックで差し込む(c: &Ctx) {
    let start = hook_call(
        &c.alice,
        "codex",
        "session-start",
        json!({ "session_id": CODEX_ID, "cwd": "/work/api", "hook_event_name": "SessionStart", "source": "startup" }),
    )
    .unwrap();
    assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(start["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains(&format!("宛先は {CODEX_ADDR}")));

    let sent = cli_json(&c.bob, &["send", CODEX_ADDR, "API の型を教えて"]);
    assert_eq!(sent["state"], "delivered");
    let logged = std::fs::read_to_string(c.alice.get("FAKE_CODEX_LOG")).unwrap();
    let lines: Vec<&str> = logged.split('\n').collect();
    assert_eq!(lines[..4], ["queue", "--thread", CODEX_ID, "--message"]);
    assert!(re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\nAPI の型を教えて"#).is_match(&lines[4..].join("\n")));

    // codex queue が失敗したとき
    let fail = c.alice.get("FAKE_CODEX_FAIL");
    std::fs::write(&fail, "").unwrap();
    assert_eq!(cli_json(&c.bob, &["send", CODEX_ADDR, "フックで受けて"])["state"], "inbox");
    let post = hook_call(&c.alice, "codex", "post-tool-use", json!({ "session_id": CODEX_ID, "hook_event_name": "PostToolUse" })).unwrap();
    assert_eq!(post["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(post["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("フックで受けて"));
    cli(&c.bob, &["send", CODEX_ADDR, "止まる前に読んで"]).unwrap();
    let stop =
        hook_call(&c.alice, "codex", "stop", json!({ "session_id": CODEX_ID, "hook_event_name": "Stop", "stop_hook_active": false }))
            .unwrap();
    assert_eq!(stop["decision"], "block");
    assert!(stop["reason"].as_str().unwrap().contains("止まる前に読んで"));
    assert_eq!(hook_call(&c.alice, "codex", "stop", json!({ "session_id": CODEX_ID, "stop_hook_active": true })), None);
    std::fs::remove_file(&fail).unwrap();

    hook_call(&c.alice, "codex", "session-end", json!({ "session_id": CODEX_ID }));
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(CODEX_ADDR)));
}

fn フック_デーモンがなくても何も出さずに正常終了する(c: &Ctx) {
    let lonely = c.alice.with(&[("NENWA_HOME", &c.root.join("nobody").display().to_string())]);
    assert_eq!(hook_call(&lonely, "codex", "post-tool-use", json!({ "session_id": CODEX_ID })), None);
}

fn setup_codexとcursorにほかの設定を残したまま足し_removeで元に戻す(c: &Ctx) {
    let codex = c.root.join("codex-home");
    let cursor = c.root.join("cursor-home");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::create_dir_all(&cursor).unwrap();
    let toml = "[mcp_servers.other]\ncommand = \"other\"\n";
    std::fs::write(codex.join("config.toml"), toml).unwrap();
    let other_hook = json!({ "hooks": [{ "type": "command", "command": "echo other" }], "matcher": "*" });
    std::fs::write(codex.join("hooks.json"), json!({ "hooks": { "PostToolUse": [other_hook] } }).to_string()).unwrap();
    std::fs::write(cursor.join("mcp.json"), json!({ "mcpServers": { "other": { "command": "other" } } }).to_string()).unwrap();
    // 古い場所（フォルダを移す前や TS 版）を指す nenwa のフックは、置き換わって重ならない
    let stale_ts = json!({ "command": "'/usr/bin/node' '/old/place/src/cli.mjs' hook cursor stop" });
    let stale = json!({ "command": "'/old/bin/nenwa' hook cursor stop" });
    std::fs::write(
        cursor.join("hooks.json"),
        json!({ "version": 1, "hooks": { "stop": [{ "command": "echo other" }, stale_ts, stale] } }).to_string(),
    )
    .unwrap();
    let h = c.alice.with(&[("NENWA_CODEX_DIR", &codex.display().to_string()), ("NENWA_CURSOR_DIR", &cursor.display().to_string())]);
    let read = |p: PathBuf| -> Value { serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap() };

    cli(&h, &["setup"]).unwrap();
    cli(&h, &["setup"]).unwrap(); // 2 回やっても重ならない
    let t = std::fs::read_to_string(codex.join("config.toml")).unwrap();
    assert_eq!(t.matches("[mcp_servers.nenwa]").count(), 1);
    assert!(re(r#"command = ".*nenwa(\.exe)?"\nargs = \["mcp","--agent","codex"\]"#).is_match(&t), "{t}");
    assert!(t.starts_with(toml));
    let ch = read(codex.join("hooks.json"))["hooks"].clone();
    let mut keys: Vec<&String> = ch.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["PostToolUse", "SessionEnd", "SessionStart", "Stop", "UserPromptSubmit"]);
    assert_eq!(ch["PostToolUse"].as_array().unwrap().len(), 2);
    assert_eq!(ch["PostToolUse"][0], other_hook);
    assert!(re(r#"nenwa(\.exe)?['"] hook codex stop$"#).is_match(ch["Stop"][0]["hooks"][0]["command"].as_str().unwrap()));
    let cm = read(cursor.join("mcp.json"))["mcpServers"].clone();
    assert_eq!(cm["nenwa"]["args"], json!(["mcp", "--agent", "cursor"]));
    let cu = read(cursor.join("hooks.json"));
    assert_eq!(cu["version"], 1);
    assert_eq!(cu["hooks"]["stop"].as_array().unwrap().len(), 2);
    assert_eq!(cu["hooks"]["postToolUse"][0]["matcher"], "*");
    assert!(codex.join("config.toml.bak-nenwa").exists());

    cli(&h, &["setup", "--remove"]).unwrap();
    assert_eq!(std::fs::read_to_string(codex.join("config.toml")).unwrap().trim(), toml.trim());
    assert_eq!(read(codex.join("hooks.json"))["hooks"], json!({ "PostToolUse": [other_hook] }));
    assert_eq!(read(cursor.join("mcp.json"))["mcpServers"], json!({ "other": { "command": "other" } }));
    assert_eq!(read(cursor.join("hooks.json"))["hooks"], json!({ "stop": [{ "command": "echo other" }] }));
}

fn 相手がオフラインならoutboxに置き_戻ったら届く(c: &Ctx) {
    stop_daemon(c.daemons.lock().unwrap().remove("bob").unwrap(), &c.bob);
    until(|| who_has(&c.alice, "offline").then_some(()));
    let before = c.bob_session.len();
    let res = cli_json(&c.alice, &["send", "bob/api-bbbb@bob-mac", "戻ったら読んで"]);
    assert_eq!(res["state"], "queued");
    c.daemons.lock().unwrap().insert("bob", start_daemon(&c.bob));
    let line = until(|| c.bob_session.after(before));
    assert!(line["message"]["content"].as_str().unwrap().contains("戻ったら読んで"));
    until(|| cli(&c.alice, &["who"]).ok().filter(|o| !o.contains("未送信")));
}

fn p2pオフ_install_が最初に作る形_でも_このパソコンの中では届く_inviteでオンになる(c: &Ctx) {
    let carol = c.make_home("carol", true);
    let cfg_file = carol.home.join("config.json");
    let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&cfg_file).unwrap()).unwrap();
    cfg["network"] = json!(false);
    std::fs::write(&cfg_file, cfg.to_string()).unwrap();
    let session = fake_session(&carol, "notes", "cccc2222-0000-0000-0000-000000000000");
    let run = || {
        let out = cli(&carol, &["who"]).unwrap(); // デーモンは自動で起動する
        assert!(out.contains("このパソコンの中だけ"), "{out}");
        assert!(out.contains("carol/notes-cccc@carol-mac"));
        assert!(std::fs::read_to_string(carol.home.join("daemon.log")).unwrap().contains("local only"));
        let res = cli_json(&carol, &["send", "carol/notes-cccc@carol-mac", "中だけで届く"]);
        assert_eq!(res["state"], "delivered");
        assert!(until(|| session.last())["message"]["content"].as_str().unwrap().contains("中だけで届く"));

        let inv = cli(&carol, &["invite"]).unwrap();
        assert!(
            re(if cfg!(windows) {
                r"& \(\[scriptblock\]::Create\(\(irm https://github\.com/nogu66/nenwa/releases/latest/download/install\.ps1\)\)\) join [A-Za-z0-9_-]{88,}"
            } else {
                r"curl -fsSL https://github\.com/nogu66/nenwa/releases/latest/download/install\.sh \| bash -s -- join [A-Za-z0-9_-]{88,}"
            })
            .is_match(&inv),
            "{inv}"
        );
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&cfg_file).unwrap()).unwrap();
        assert_eq!(cfg["network"], true);
        until(|| cli(&carol, &["who"]).ok().filter(|o| !o.contains("このパソコンの中だけ")));
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    // 自動で起動したデーモンを止める
    shutdown(&carol);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// 偽の codex として呼ばれたとき: queue に渡された引数を記録する。codex-fail があれば失敗する。
fn fake_codex(log: &str) {
    if std::env::var("FAKE_CODEX_FAIL").is_ok_and(|f| Path::new(&f).exists()) {
        eprintln!("no rollout found");
        std::process::exit(1);
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    for a in std::env::args().skip(1) {
        writeln!(f, "{a}").unwrap();
    }
    writeln!(f, "---").unwrap();
}

fn main() {
    if let Ok(log) = std::env::var("FAKE_CODEX_LOG") {
        return fake_codex(&log);
    }
    let tests: Vec<Test> = vec![
        ("相手のセッションが一覧に出る", 相手のセッションが一覧に出る),
        ("ターミナルから送ると相手のセッションに枠つきで届く", ターミナルから送ると相手のセッションに枠つきで届く),
        ("いないセッション宛ては no_session", いないセッション宛てはno_session),
        ("受信箱に届くと webhook で起こし、check_inbox 相当で読める", 受信箱に届くとwebhookで起こし_check_inbox相当で読める),
        ("空の受信箱は --wait で届くまで待つ", 空の受信箱は_waitで届くまで待つ),
        ("受信箱の名前で返事をすると、相手のセッションに届く", 受信箱の名前で返事をすると_相手のセッションに届く),
        ("ターミナルからの送信への返事は terminal の受信箱に入る", ターミナルからの送信への返事はterminalの受信箱に入る),
        ("MCP: NENWA_AS を付けたときだけ check_inbox が出る", mcp_nenwa_asを付けたときだけcheck_inboxが出る),
        ("Cursor: sessionStart で宛先が決まり、postToolUse と stop で届く", cursor_session_startで宛先が決まり_post_tool_useとstopで届く),
        (
            "Cursor: MCP（--agent cursor）から送ると、そのセッションが送り主になる",
            cursor_mcp_agent_cursorから送ると_そのセッションが送り主になる,
        ),
        ("Codex: codex queue で会話に入れる。失敗したらフックで差し込む", codex_codex_queueで会話に入れる_失敗したらフックで差し込む),
        ("フック: デーモンがなくても何も出さずに正常終了する", フック_デーモンがなくても何も出さずに正常終了する),
        (
            "setup: Codex と Cursor にほかの設定を残したまま足し、--remove で元に戻す",
            setup_codexとcursorにほかの設定を残したまま足し_removeで元に戻す,
        ),
        ("相手がオフラインなら outbox に置き、戻ったら届く", 相手がオフラインならoutboxに置き_戻ったら届く),
        (
            "P2P オフ（install が最初に作る形）でも、このパソコンの中では届く。invite でオンになる",
            p2pオフ_install_が最初に作る形_でも_このパソコンの中では届く_inviteでオンになる,
        ),
    ];
    // cargo test -- <文字列> で、名前にその文字列を含むものだけを動かす（--list などの旗は無視）
    let filter: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with('-')).collect();
    let selected: Vec<_> = tests.into_iter().filter(|(n, _)| filter.is_empty() || filter.iter().any(|f| n.contains(f.as_str()))).collect();
    println!("\nrunning {} tests", selected.len());
    if selected.is_empty() {
        println!("\ntest result: ok. 0 passed; 0 failed\n");
        return;
    }
    let ctx = before();
    let (mut passed, mut failed) = (0, vec![]);
    let started = Instant::now();
    for (name, f) in selected {
        let t = Instant::now();
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&ctx))).is_ok();
        println!("test {name} ... {} ({:.1}s)", if ok { "ok" } else { "FAILED" }, t.elapsed().as_secs_f64());
        if ok {
            passed += 1;
        } else {
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        eprintln!("ログは NENWA_TEST_KEEP=1 で {} に残せます", ctx.root.display());
    }
    after(ctx);
    println!(
        "\ntest result: {}. {passed} passed; {} failed; finished in {:.1}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len(),
        started.elapsed().as_secs_f64()
    );
    if !failed.is_empty() {
        for n in failed {
            eprintln!("    {n}");
        }
        std::process::exit(1);
    }
}
