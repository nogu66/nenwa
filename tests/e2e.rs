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
        for k in [
            "NENWA_AS",
            "NENWA_AGENT",
            "NENWA_WAKE_KEY",
            "NENWA_CODEX_DIR",
            "NENWA_CURSOR_DIR",
            "NENWA_GEMINI_DIR",
            "NENWA_QWEN_DIR",
            "XDG_CONFIG_HOME",
            "NENWA_HTTP_PORT",
            "NENWA_HTTP_TTL_SECS",
            "NENWA_KEY_FILE",
        ] {
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
    muse: Mutex<Option<Muse>>,
}

// nenwa muse で開いた HTTP の入り口
#[derive(Clone)]
struct Muse {
    env: Home,
    url: String,
    key: String,
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
    let file = h.home.join("test-daemon.log");
    let ups = || std::fs::read_to_string(&file).unwrap_or_default().matches(" daemon up ").count();
    let before = ups();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&file).unwrap();
    let mut p = h.command(&["daemon"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log).spawn().unwrap();
    // 立ち上がる前に CLI を呼ぶと、CLI が自分でもう 1 つ起動してしまい、こちらが負けて終わる。起動のログを待つ。
    until(|| (ups() > before || p.try_wait().is_ok_and(|s| s.is_some())).then_some(()));
    assert!(p.try_wait().unwrap().is_none(), "デーモンが起動しませんでした: {}", std::fs::read_to_string(&file).unwrap_or_default());
    p
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
    Ctx { root, team_key, fake_codex, alice, bob, bob_session, daemons: Mutex::new(daemons), muse: Mutex::new(None) }
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

// /clear すると会話の ID が変わり、宛先（ID の先頭 4 文字）も変わる。古い宛先への返事と、送り主の名前がずれないこと。
fn clearで会話のidが変わっても_古い宛先に届き_送り主は今の名前になる(c: &Ctx) {
    let file = c.bob.claude.join("sessions").join(format!("{}.json", std::process::id()));
    let before = std::fs::read_to_string(&file).unwrap();
    let mut s: Value = serde_json::from_str(&before).unwrap();
    let old_id = s["sessionId"].as_str().unwrap().to_string();
    s["sessionId"] = json!("cccc0000-0000-4000-8000-000000000000");
    std::fs::write(&file, s.to_string()).unwrap();

    let n = c.bob_session.len();
    let res = cli_json(&c.alice, &["send", "bob/api-bbbb@bob-mac", "clear のあとの返事"]);
    assert_eq!(res["state"], "delivered", "{res}");
    until(|| (c.bob_session.len() > n).then_some(()));

    // MCP サーバーの環境変数は、起動したときの古い ID のまま
    let mut p =
        c.bob.command(&["mcp"]).env("CLAUDE_CODE_SESSION_ID", &old_id).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "send_message", "arguments": { "to": "alice/terminal@alice-mac", "message": "今の名前で送る" } } });
    writeln!(p.stdin.take().unwrap(), "{req}").unwrap();
    let mut line = String::new();
    BufReader::new(p.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let _ = p.kill();
    let _ = p.wait();
    std::fs::write(&file, before).unwrap();
    assert!(line.contains("bob/api-cccc@bob-mac"), "{line}");
    let _ = cli_json(&c.alice, &["inbox"]);
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

// ---- HTTP の入り口（nenwa muse）。Tailscale とクリップボードは使わず、鍵はファイルで受け取る ----

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

// nenwa muse を実行して、表示された URL と書き出された鍵を返す
fn muse_on(c: &Ctx, extra: &[(&str, &str)]) -> Muse {
    let key_file = c.root.join(format!("muse-key-{}", rand_hex(3)));
    let port = free_port().to_string();
    let k = key_file.display().to_string();
    let mut vars = vec![("NENWA_TEST_NO_TAILSCALE", "1"), ("NENWA_HTTP_PORT", port.as_str()), ("NENWA_KEY_FILE", k.as_str())];
    vars.extend_from_slice(extra);
    let env = c.alice.with(&vars);
    let out = cli(&env, &["muse"]).unwrap();
    let url = re(r"http://127\.0\.0\.1:\d+/mcp").find(&out).unwrap().as_str().to_string();
    let key = std::fs::read_to_string(&key_file).unwrap().trim().to_string();
    std::fs::remove_file(&key_file).unwrap();
    assert!(!out.contains(&key), "鍵は画面に出さない");
    assert!(out.contains("alice/muse@alice-mac"), "{out}");
    let m = Muse { env, url, key };
    *c.muse.lock().unwrap() = Some(m.clone());
    m
}

fn muse(c: &Ctx) -> Muse {
    c.muse.lock().unwrap().clone().unwrap()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(40))).build().into()
}

// POST して (状態, 本文)。つながらなければ Err
fn post(url: &str, key: Option<&str>, body: &str) -> Result<(u16, String), String> {
    let mut r = agent().post(url).header("Content-Type", "application/json").header("Accept", "application/json, text/event-stream");
    if let Some(k) = key {
        r = r.header("Authorization", &format!("Bearer {k}"));
    }
    let mut res = r.send(body).map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    Ok((status, res.body_mut().read_to_string().unwrap_or_default()))
}

fn rpc(m: &Muse, id: i64, method: &str, params: Value) -> Value {
    let (status, body) =
        post(&m.url, Some(&m.key), &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()).unwrap();
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], id);
    v
}

fn tool(m: &Muse, id: i64, name: &str, args: Value) -> String {
    let v = rpc(m, id, "tools/call", json!({ "name": name, "arguments": args }));
    v["result"]["content"][0]["text"].as_str().unwrap().to_string()
}

fn http_muse_鍵なしと違う鍵は401_鍵はハッシュだけ残す(c: &Ctx) {
    until(|| who_has(&c.alice, "bob/api-bbbb@bob-mac").then_some(())); // 絞り込んで動かしたときのため
    let m = muse_on(c, &[]);
    let keys = c.alice.home.join("http").join("keys.json");
    let saved = std::fs::read_to_string(&keys).unwrap();
    assert!(!saved.contains(&m.key), "鍵そのものは残さない");
    assert!(saved.contains("sha256"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&keys).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert!(c.alice.home.join("http").join("enabled").exists());
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    assert_eq!(post(&m.url, None, &ping).unwrap().0, 401);
    assert_eq!(post(&m.url, Some("wrong-key"), &ping).unwrap().0, 401);
    assert_eq!(agent().get(&m.url).call().unwrap().status().as_u16(), 405);
    assert_eq!(post(&m.url.replace("/mcp", "/other"), Some(&m.key), &ping).unwrap().0, 404);
}

fn http_initializeとtools_listで3つのツール_通知は202(c: &Ctx) {
    let m = muse(c);
    let init = rpc(&m, 1, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test" } }));
    assert_eq!(init["result"]["serverInfo"]["name"], "nenwa");
    let (status, body) =
        post(&m.url, Some(&m.key), &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string()).unwrap();
    assert_eq!((status, body.as_str()), (202, ""));
    let tools = rpc(&m, 2, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["list_team", "send_message", "check_inbox"]);
    assert!(tool(&m, 3, "list_team", json!({})).contains("bob/api-bbbb@bob-mac"));
}

fn http_museからのsend_messageは_送り主がmuseで届く(c: &Ctx) {
    let m = muse(c);
    let before = c.bob_session.len();
    // 別の名前を名乗ろうとしても muse のまま
    let text = tool(&m, 4, "send_message", json!({ "to": "bob/api-bbbb@bob-mac", "message": "Muse から", "as": "grok" }));
    let res: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(res["from"], "alice/muse@alice-mac");
    assert_eq!(res["state"], "delivered");
    let line = until(|| c.bob_session.after(before));
    let content = line["message"]["content"].as_str().unwrap();
    assert!(
        re(r#"<nenwa-message from="alice/muse@alice-mac" id="[0-9a-f-]+">\nMuse から\n</nenwa-message>"#).is_match(content),
        "{content}"
    );
}

fn http_返事をcheck_inboxで読める(c: &Ctx) {
    let m = muse(c);
    let sent = cli_json(&c.bob, &["send", "alice/muse@alice-mac", "Muse さんへの返事"]);
    assert_eq!(sent["state"], "inbox");
    let text = tool(&m, 5, "check_inbox", json!({ "wait_secs": 5 }));
    assert!(re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\nMuse さんへの返事\n</nenwa-message>"#).is_match(&text), "{text}");
    // 空のときは待って、届いたら返す（上限は 25 秒）
    let mm = m.clone();
    let waiting = std::thread::spawn(move || tool(&mm, 6, "check_inbox", json!({ "wait_secs": 999 })));
    sleep(500);
    cli(&c.bob, &["send", "alice/muse@alice-mac", "待っていた分"]).unwrap();
    assert!(waiting.join().unwrap().contains("待っていた分"));
}

fn http_本文が大きすぎると413_続けて送ると429(c: &Ctx) {
    let m = muse(c);
    let big = json!({ "jsonrpc": "2.0", "id": 7, "method": "ping", "params": { "pad": "x".repeat(70 * 1024) } }).to_string();
    assert_eq!(post(&m.url, Some(&m.key), &big).unwrap().0, 413);
    let ping = json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" }).to_string();
    let statuses: Vec<u16> = (0..31).map(|_| post(&m.url, Some(&m.key), &ping).unwrap().0).collect();
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
}

fn http_鍵の合わない要求が続くと503(c: &Ctx) {
    let m = muse(c);
    let ping = json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" }).to_string();
    let statuses: Vec<u16> = (0..62).map(|_| post(&m.url, Some("guess"), &ping).unwrap().0).collect();
    assert_eq!(statuses.last(), Some(&503), "{statuses:?}");
    assert_eq!(post(&m.url, Some(&m.key), &ping).unwrap().0, 503, "正しい鍵でも止まっている");
}

fn http_offのあとは使えない_受信箱の中身は残る(c: &Ctx) {
    let m = muse(c);
    cli(&c.bob, &["send", "alice/muse@alice-mac", "閉じても残る"]).unwrap();
    let out = cli(&m.env, &["muse", "--off"]).unwrap();
    assert!(out.contains("閉じました"), "{out}");
    let ping = json!({ "jsonrpc": "2.0", "id": 10, "method": "ping" }).to_string();
    // 待ち受けは閉じている（前のつながりが残っていれば 401）
    match post(&m.url, Some(&m.key), &ping) {
        Err(_) => {}
        Ok((status, _)) => assert_eq!(status, 401),
    }
    assert!(!c.alice.home.join("http").join("keys.json").exists());
    assert!(!c.alice.home.join("http").join("enabled").exists());
    let bx = cli_json(&c.alice, &["inbox", "--as", "muse"]);
    assert_eq!(bx["messages"][0]["body"], "閉じても残る");
}

fn http_期限を過ぎると閉じる(c: &Ctx) {
    let m = muse_on(c, &[("NENWA_HTTP_TTL_SECS", "3")]);
    let ping = json!({ "jsonrpc": "2.0", "id": 11, "method": "ping" }).to_string();
    assert_eq!(post(&m.url, Some(&m.key), &ping).unwrap().0, 200, "開き直すと数えていた回数は消える");
    until(|| post(&m.url, Some(&m.key), &ping).map_or(Some(()), |(s, _)| (s == 401).then_some(())));
    until(|| (!c.alice.home.join("http").join("enabled").exists()).then_some(()));
    assert!(!c.alice.home.join("http").join("keys.json").exists());
    assert!(std::fs::read_to_string(c.alice.home.join("test-daemon.log")).unwrap().contains("http expired"));
}

fn http_デーモンが再起動しても開き直す(c: &Ctx) {
    let m = muse_on(c, &[]);
    stop_daemon(c.daemons.lock().unwrap().remove("alice").unwrap(), &c.alice);
    c.daemons.lock().unwrap().insert("alice", start_daemon(&c.alice));
    let ping = json!({ "jsonrpc": "2.0", "id": 12, "method": "ping" }).to_string();
    until(|| post(&m.url, Some(&m.key), &ping).ok().filter(|(s, _)| *s == 200));
    cli(&m.env, &["muse", "--off"]).unwrap();
    let log = std::fs::read_to_string(c.alice.home.join("test-daemon.log")).unwrap();
    assert!(log.contains("(reopened)"));
    assert!(!log.contains(&m.key), "鍵はログにも出さない");
    until(|| who_has(&c.bob, "alice@alice-mac  online").then_some(()));
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

// 偽の Codex 本体（sh）。フックと MCP サーバーをその子として動かし、stdin が閉じたら終わる。
struct FakeAgent {
    child: std::process::Child,
    lines: std::io::Lines<BufReader<std::process::ChildStdout>>,
    out: PathBuf,
}

impl FakeAgent {
    fn start(c: &Ctx, id: &str, to: &str) -> FakeAgent {
        let dir = c.root.join(format!("agent-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        let (req, out) = (dir.join("req"), dir.join("out"));
        let calls = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "send_message", "arguments": { "to": to, "message": id } } }),
        ];
        std::fs::write(&req, calls.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
        let script = r#"printf '%s' "$2" | "$1" hook codex session-start >/dev/null
printf '%s' "$2" | "$1" hook codex post-tool-use >/dev/null
echo ready
read _ || exit 0
NENWA_AGENT=codex "$1" mcp < "$3" > "$4"
echo sent
read _"#;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script, "sh", CLI, &json!({ "session_id": id, "cwd": "/work/web" }).to_string()]).arg(&req).arg(&out);
        for (k, v) in &c.alice.env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "ready");
        FakeAgent { child, lines, out }
    }
    // MCP から 1 通送り、デーモンが決めた送り主を返す
    fn send(&mut self) -> String {
        writeln!(self.child.stdin.as_mut().unwrap()).unwrap();
        assert_eq!(self.lines.next().unwrap().unwrap(), "sent");
        let text = std::fs::read_to_string(&self.out).unwrap();
        let replies: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let res: Value = serde_json::from_str(by_id(&replies, 2)["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        res["from"].as_str().unwrap().to_string()
    }
    fn close(mut self) {
        drop(self.child.stdin.take());
        self.child.wait().unwrap();
    }
}

fn 同じフォルダの会話が2つあっても_送り主は呼んだ本体で決まり_本体が消えた会話は一覧から外れる(
    c: &Ctx,
) {
    if cfg!(windows) {
        return; // sh を使うので unix だけ
    }
    let (first, second) = ("alice/web-aaa1@alice-mac", "alice/web-aaa2@alice-mac");
    let mut a = FakeAgent::start(c, "019a2b3c-0000-7000-8000-00000000aaa1", "bob/api-bbbb@bob-mac");
    // あとから開いた会話。フォルダも状態も同じなので、本体を見なければこちらが選ばれる。
    let b = FakeAgent::start(c, "019a2b3c-0000-7000-8000-00000000aaa2", "bob/api-bbbb@bob-mac");
    assert_eq!(a.send(), first);

    a.close();
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(first) && o.contains(second)));
    assert_eq!(cli_json(&c.bob, &["send", first, "もういない"])["state"], "no_session");
    b.close();
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(second)));
}

const GEMINI_ID: &str = "7d1e2f3a-aaaa-bbbb-cccc-00000000f00d";
const GEMINI_ADDR: &str = "alice/docs-f00d@alice-mac";

// Gemini CLI はイベントの名前が違い、応答の終わりで続けさせる言葉が deny。Qwen Code は Codex と同じ形。
fn gemini_cliとqwen_code_フックで宛先が決まり_それぞれの形で届く(c: &Ctx) {
    let input = |event: &str| json!({ "session_id": GEMINI_ID, "cwd": "/work/docs", "hook_event_name": event });
    let start = hook_call(&c.alice, "gemini", "session-start", input("SessionStart")).unwrap();
    assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(start["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains(&format!("宛先は {GEMINI_ADDR}")));
    assert!(who_has(&c.bob, GEMINI_ADDR) || until(|| who_has(&c.bob, GEMINI_ADDR).then_some(true)));

    // 外から会話に入れる口はないので、受信箱に置いて次のフックで渡す
    assert_eq!(cli_json(&c.bob, &["send", GEMINI_ADDR, "入力の前に"])["state"], "inbox");
    let prompt = hook_call(&c.alice, "gemini", "user-prompt-submit", input("BeforeAgent")).unwrap();
    assert_eq!(prompt["hookSpecificOutput"]["hookEventName"], "BeforeAgent");
    assert!(prompt["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("入力の前に"));
    cli(&c.bob, &["send", GEMINI_ADDR, "ツールのあとに"]).unwrap();
    let tool = hook_call(&c.alice, "gemini", "post-tool-use", input("AfterTool")).unwrap();
    assert_eq!(tool["hookSpecificOutput"]["hookEventName"], "AfterTool");
    assert!(tool["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("ツールのあとに"));
    cli(&c.bob, &["send", GEMINI_ADDR, "止まる前に"]).unwrap();
    let stop = hook_call(&c.alice, "gemini", "stop", input("AfterAgent")).unwrap();
    assert_eq!(stop["decision"], "deny");
    assert!(stop["reason"].as_str().unwrap().contains("止まる前に"));
    assert_eq!(hook_call(&c.alice, "gemini", "stop", input("AfterAgent")), None);

    // MCP（--agent gemini）から送ると、その会話が送り主になる
    let out = mcp_call(
        &c.alice.with(&[("NENWA_AGENT", "gemini")]),
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "send_message", "arguments": { "to": "bob/api-bbbb@bob-mac", "message": "Gemini から" } } }),
        ],
    );
    let res: Value = serde_json::from_str(by_id(&out, 2)["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(res["from"], GEMINI_ADDR);
    hook_call(&c.alice, "gemini", "session-end", input("SessionEnd"));
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(GEMINI_ADDR)));

    let qwen = "alice/docs-beef@alice-mac";
    let input = json!({ "session_id": "11112222-3333-4444-5555-66667777beef", "cwd": "/work/docs" });
    hook_call(&c.alice, "qwen", "session-start", input.clone()).unwrap();
    cli(&c.bob, &["send", qwen, "Qwen へ"]).unwrap();
    let post = hook_call(&c.alice, "qwen", "post-tool-use", input.clone()).unwrap();
    assert_eq!(post["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(post["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("Qwen へ"));
    cli(&c.bob, &["send", qwen, "止まる前に"]).unwrap();
    assert_eq!(hook_call(&c.alice, "qwen", "stop", input.clone()).unwrap()["decision"], "block");
    hook_call(&c.alice, "qwen", "session-end", input);
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
    // Gemini CLI: ほかの設定とフックを残す。Qwen Code: 読めない設定（コメント入り）には触らない。
    let (gemini, qwen) = (c.root.join("gemini-home"), c.root.join("qwen-home"));
    std::fs::create_dir_all(&gemini).unwrap();
    std::fs::create_dir_all(&qwen).unwrap();
    let gemini_before = json!({
        "theme": "Default",
        "mcpServers": { "other": { "command": "other" } },
        "hooks": { "SessionStart": [{ "matcher": "", "hooks": [{ "type": "command", "command": "echo other" }] }] }
    });
    std::fs::write(gemini.join("settings.json"), gemini_before.to_string()).unwrap();
    let qwen_text = "{\n  // コメント\n  \"theme\": \"Default\"\n}\n";
    std::fs::write(qwen.join("settings.json"), qwen_text).unwrap();
    let h = c.alice.with(&[
        ("NENWA_CODEX_DIR", &codex.display().to_string()),
        ("NENWA_CURSOR_DIR", &cursor.display().to_string()),
        ("NENWA_GEMINI_DIR", &gemini.display().to_string()),
        ("NENWA_QWEN_DIR", &qwen.display().to_string()),
    ]);
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
    let g = read(gemini.join("settings.json"));
    assert_eq!(g["theme"], "Default");
    assert_eq!(g["mcpServers"]["other"]["command"], "other");
    assert_eq!(g["mcpServers"]["nenwa"]["args"], json!(["mcp", "--agent", "gemini"]));
    let mut keys: Vec<&String> = g["hooks"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["AfterAgent", "AfterTool", "BeforeAgent", "SessionEnd", "SessionStart"]);
    assert_eq!(g["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    assert_eq!(g["hooks"]["AfterAgent"].as_array().unwrap().len(), 1);
    assert!(re(r#"nenwa(\.exe)?['"] hook gemini stop$"#).is_match(g["hooks"]["AfterAgent"][0]["hooks"][0]["command"].as_str().unwrap()));
    assert_eq!(g["hooks"]["AfterAgent"][0]["hooks"][0]["timeout"], 10_000);
    assert_eq!(std::fs::read_to_string(qwen.join("settings.json")).unwrap(), qwen_text);

    cli(&h, &["setup", "--remove"]).unwrap();
    assert_eq!(read(gemini.join("settings.json")), gemini_before);
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
        (
            "clearで会話のidが変わっても_古い宛先に届き_送り主は今の名前になる",
            clearで会話のidが変わっても_古い宛先に届き_送り主は今の名前になる,
        ),
        ("いないセッション宛ては no_session", いないセッション宛てはno_session),
        ("受信箱に届くと webhook で起こし、check_inbox 相当で読める", 受信箱に届くとwebhookで起こし_check_inbox相当で読める),
        ("空の受信箱は --wait で届くまで待つ", 空の受信箱は_waitで届くまで待つ),
        ("受信箱の名前で返事をすると、相手のセッションに届く", 受信箱の名前で返事をすると_相手のセッションに届く),
        ("ターミナルからの送信への返事は terminal の受信箱に入る", ターミナルからの送信への返事はterminalの受信箱に入る),
        ("MCP: NENWA_AS を付けたときだけ check_inbox が出る", mcp_nenwa_asを付けたときだけcheck_inboxが出る),
        ("HTTP（nenwa muse）: 鍵なしと違う鍵は 401。鍵はハッシュだけ残す", http_muse_鍵なしと違う鍵は401_鍵はハッシュだけ残す),
        ("HTTP: initialize と tools/list で 3 つのツール。通知は 202", http_initializeとtools_listで3つのツール_通知は202),
        ("HTTP: muse からの send_message は、送り主が muse で届く", http_museからのsend_messageは_送り主がmuseで届く),
        ("HTTP: 返事を check_inbox で読める", http_返事をcheck_inboxで読める),
        ("HTTP: 本文が大きすぎると 413、続けて送ると 429", http_本文が大きすぎると413_続けて送ると429),
        ("HTTP: 鍵の合わない要求が続くと 503", http_鍵の合わない要求が続くと503),
        ("HTTP: --off のあとは使えない。受信箱の中身は残る", http_offのあとは使えない_受信箱の中身は残る),
        ("HTTP: 期限を過ぎると閉じる", http_期限を過ぎると閉じる),
        ("HTTP: デーモンが再起動しても開き直す", http_デーモンが再起動しても開き直す),
        ("Cursor: sessionStart で宛先が決まり、postToolUse と stop で届く", cursor_session_startで宛先が決まり_post_tool_useとstopで届く),
        (
            "Cursor: MCP（--agent cursor）から送ると、そのセッションが送り主になる",
            cursor_mcp_agent_cursorから送ると_そのセッションが送り主になる,
        ),
        ("Codex: codex queue で会話に入れる。失敗したらフックで差し込む", codex_codex_queueで会話に入れる_失敗したらフックで差し込む),
        (
            "Codex: 同じフォルダの会話が 2 つあっても、送り主は呼んだ本体で決まる。本体が消えた会話は一覧から外れる",
            同じフォルダの会話が2つあっても_送り主は呼んだ本体で決まり_本体が消えた会話は一覧から外れる,
        ),
        (
            "Gemini CLI と Qwen Code: フックで宛先が決まり、それぞれの形で届く",
            gemini_cliとqwen_code_フックで宛先が決まり_それぞれの形で届く,
        ),
        ("フック: デーモンがなくても何も出さずに正常終了する", フック_デーモンがなくても何も出さずに正常終了する),
        (
            "setup: 各エージェントにほかの設定を残したまま足し、--remove で元に戻す",
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
