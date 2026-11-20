// Start daemons for two people (alice and bob) on one machine and check that a message reaches a fake Claude Code session.
// Each test uses the state left by the one before, so they run one at a time in the order written (a small runner of our own. harness = false in Cargo.toml).
// Nothing is written to the real home: NENWA_HOME, HOME (USERPROFILE on Windows) and CLAUDE_CONFIG_DIR all point at a temporary folder,
// and P2P uses NENWA_RELAY=disabled. The fake Claude Code session is a unix socket on macOS and Linux and a named pipe on Windows.
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

// Where the daemon's local socket is (decided the same way as src/os)
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

// Ask the daemon to shut down (one started automatically has an unknown pid, so stop it here)
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
            "NENWA_OPENCODE_DIR",
            "NENWA_COPILOT_DIR",
            "COPILOT_HOME",
            "NENWA_PI_DIR",
            "NENWA_ANTIGRAVITY_DIR",
            "PI_CODING_AGENT_DIR",
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

// The HTTP endpoint opened by nenwa muse
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

// Run a hook, pass JSON on stdin, and return the JSON from stdout (None if there is none).
fn hook_call(h: &Home, agent: &str, event: &str, input: Value) -> Option<Value> {
    let mut p = h.command(&["hook", agent, event]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    p.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = p.wait_with_output().unwrap();
    assert!(out.status.success(), "exit {:?}", out.status.code());
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then(|| serde_json::from_str(&text).unwrap())
}

// Start the MCP server, feed it JSON-RPC, and collect what comes back.
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

// Wait until f returns Some, and return that value
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

// A fake session: opens a receiving socket and collects the lines that arrive. The pid is this test itself (it is alive, so it appears in the list).
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
            // Read until the other side closes (a Windows pipe returns an error on close)
            let (mut buf, mut chunk) = (vec![], [0u8; 4096]);
            while let Ok(n @ 1..) = c.read(&mut chunk) {
                buf.extend_from_slice(&chunk[..n]);
            }
            g.lock().unwrap().push(serde_json::from_slice(&buf).unwrap());
        }
    });
    let pid = std::process::id();
    let s = json!({ "pid": pid, "sessionId": session_id, "cwd": format!("/work/{folder}"), "name": format!("{folder} work"), "status": "idle", "messagingSocketPath": sock });
    std::fs::write(h.claude.join("sessions").join(format!("{pid}.json")), s.to_string()).unwrap();
    Session { got }
}

fn start_daemon(h: &Home) -> Child {
    let file = h.home.join("test-daemon.log");
    let ups = || std::fs::read_to_string(&file).unwrap_or_default().matches(" daemon up ").count();
    let before = ups();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&file).unwrap();
    let mut p = h.command(&["daemon"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log).spawn().unwrap();
    // Calling the CLI before it is up makes the CLI start another one itself, and ours loses and exits. Wait for the startup log.
    until(|| (ups() > before || p.try_wait().is_ok_and(|s| s.is_some())).then_some(()));
    assert!(p.try_wait().unwrap().is_none(), "the daemon did not start: {}", std::fs::read_to_string(&file).unwrap_or_default());
    p
}

// Stop with SIGTERM (the daemon closes its peer connections before exiting). On Windows, ask it to shut down.
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

// A fake webhook: collects the headers and bodies of the requests that arrive
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
            headers.insert(":path".to_string(), line.split_whitespace().nth(1).unwrap_or("").to_string());
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
    // The fake codex is this test executable itself (see the top of main)
    let fake_codex = std::env::current_exe().unwrap();

    // alice owns the network and bob joins with alice's invite (install is skipped)
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

fn peer_session_appears_in_the_list(c: &Ctx) {
    let out = until(|| cli(&c.alice, &["who"]).ok().filter(|o| o.contains("bob/api-bbbb@bob-mac")));
    assert!(re(r"bob@bob-mac\s+online").is_match(&out), "{out}");
    assert!(out.contains("api work"));
}

fn send_from_terminal_arrives_framed_in_peer_session(c: &Ctx) {
    let res = cli_json(&c.alice, &["send", "bob/api-bbbb@bob-mac", "Did you change the token format"]);
    assert_eq!(res["state"], "delivered");
    assert_eq!(res["from"], "alice/terminal@alice-mac");
    let line = until(|| c.bob_session.last());
    assert_eq!(line["type"], "user");
    assert_eq!(line["message"]["role"], "user");
    let content = line["message"]["content"].as_str().unwrap();
    assert!(
        re(r#"<nenwa-message from="alice/terminal@alice-mac" id="[0-9a-f-]+">\nDid you change the token format\n</nenwa-message>"#)
            .is_match(content),
        "{content}"
    );
}

// /clear changes the conversation ID and so the address (first 4 characters of the ID). Replies to the old address and the sender's name must not drift.
fn after_clear_old_address_still_delivers_and_sender_uses_current_name(c: &Ctx) {
    let file = c.bob.claude.join("sessions").join(format!("{}.json", std::process::id()));
    let before = std::fs::read_to_string(&file).unwrap();
    let mut s: Value = serde_json::from_str(&before).unwrap();
    let old_id = s["sessionId"].as_str().unwrap().to_string();
    s["sessionId"] = json!("cccc0000-0000-4000-8000-000000000000");
    std::fs::write(&file, s.to_string()).unwrap();

    let n = c.bob_session.len();
    let res = cli_json(&c.alice, &["send", "bob/api-bbbb@bob-mac", "a reply after clear"]);
    assert_eq!(res["state"], "delivered", "{res}");
    until(|| (c.bob_session.len() > n).then_some(()));

    // The MCP server's environment variable keeps the old ID from when it started
    let mut p =
        c.bob.command(&["mcp"]).env("CLAUDE_CODE_SESSION_ID", &old_id).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "send_message", "arguments": { "to": "alice/terminal@alice-mac", "message": "sent under the current name" } } });
    writeln!(p.stdin.take().unwrap(), "{req}").unwrap();
    let mut line = String::new();
    BufReader::new(p.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let _ = p.kill();
    let _ = p.wait();
    std::fs::write(&file, before).unwrap();
    assert!(line.contains("bob/api-cccc@bob-mac"), "{line}");
    let _ = cli_json(&c.alice, &["inbox"]);
}

fn missing_session_gives_no_session(c: &Ctx) {
    let res = cli_json(&c.alice, &["send", "bob/nope@bob-mac", "hi"]);
    assert_eq!(res["state"], "no_session");
}

// ---- Inboxes (peers other than Claude Code, such as Grok Bot) ----

fn inbox_arrival_wakes_by_webhook_and_reads_like_check_inbox(c: &Ctx) {
    let (url, hooks) = webhook_server();
    cli(&c.alice.with(&[("NENWA_WAKE_KEY", "test-key-123")]), &["wake", "grok", &url]).unwrap();

    // It appears in the list as an address from bob's side too
    until(|| who_has(&c.bob, "alice/grok@alice-mac").then_some(()));

    let sent = cli_json(&c.bob, &["send", "alice/grok@alice-mac", "fix this test"]);
    assert_eq!(sent["state"], "inbox");
    let (headers, body) = until(|| hooks.lock().unwrap().first().cloned());
    assert_eq!(headers["authorization"], "Bearer test-key-123");
    assert_eq!(body["source"], "nenwa");
    assert_eq!(body["mailbox"], "alice/grok@alice-mac");
    assert_eq!(body["unread"], 1);
    assert_eq!(body["from"], json!(["bob/terminal@bob-mac"]));
    assert!(!body.to_string().contains("fix this test"), "the message text is not put in the webhook");

    // While it is still unread, it is not woken again
    cli(&c.bob, &["send", "alice/grok@alice-mac", "one more"]).unwrap();
    sleep(500);
    assert_eq!(hooks.lock().unwrap().len(), 1);

    let bx = cli_json(&c.alice, &["inbox", "--as", "grok"]);
    let bodies: Vec<&str> = bx["messages"].as_array().unwrap().iter().map(|m| m["body"].as_str().unwrap()).collect();
    assert_eq!(bodies, ["fix this test", "one more"]);
    assert_eq!(
        cli_json(&c.alice, &["inbox", "--as", "grok"])["messages"].as_array().unwrap().len(),
        0,
        "messages that were read are removed"
    );

    // A message that arrives after the read wakes it again
    cli(&c.bob, &["send", "alice/grok@alice-mac", "the third one"]).unwrap();
    until(|| (hooks.lock().unwrap().len() == 2).then_some(()));
    cli(&c.alice, &["inbox", "--as", "grok"]).unwrap();
}

fn empty_inbox_waits_with_wait_until_arrival(c: &Ctx) {
    let waiting = c.alice.command(&["inbox", "--as", "grok", "--wait", "20"]).stdout(Stdio::piped()).spawn().unwrap();
    sleep(500);
    cli(&c.bob, &["send", "alice/grok@alice-mac", "the one waited for"]).unwrap();
    let out = waiting.wait_with_output().unwrap();
    let bx: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(bx["messages"][0]["body"], "the one waited for");
}

fn reply_under_inbox_name_reaches_peer_session(c: &Ctx) {
    let sent = cli_json(&c.bob, &["send", "alice/grok@alice-mac", "a question"]);
    let bx = cli_json(&c.alice, &["inbox", "--as", "grok"]);
    assert_eq!(bx["messages"][0]["id"], sent["id"]);
    let before = c.bob_session.len();
    let id = sent["id"].as_str().unwrap();
    let res = cli_json(&c.alice, &["send", "--as", "grok", "--reply-to", id, "bob/api-bbbb@bob-mac", "an answer"]);
    assert_eq!(res["from"], "alice/grok@alice-mac");
    assert_eq!(res["state"], "delivered");
    let line = until(|| c.bob_session.after(before));
    let content = line["message"]["content"].as_str().unwrap();
    assert!(re(&format!(r#"from="alice/grok@alice-mac" id="[0-9a-f-]+" reply_to="{id}""#)).is_match(content), "{content}");
}

fn reply_to_terminal_send_goes_to_terminal_inbox(c: &Ctx) {
    let sent = cli_json(&c.alice, &["send", "--as", "grok", "bob/terminal@bob-mac", "to the terminal"]);
    assert_eq!(sent["state"], "inbox");
    let bx = cli_json(&c.bob, &["inbox"]);
    assert_eq!(bx["messages"].as_array().unwrap().last().unwrap()["body"], "to the terminal");
}

fn mcp_check_inbox_appears_only_with_nenwa_as(c: &Ctx) {
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

    cli(&c.bob, &["send", "alice/grok@alice-mac", "read this over MCP"]).unwrap();
    let out = mcp_call(
        &c.alice.with(&[("NENWA_AS", "grok")]),
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "check_inbox", "arguments": {} } }),
        ],
    );
    let text = by_id(&out, 3)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\nread this over MCP\n</nenwa-message>"#).is_match(&text), "{text}");
}

// ---- HTTP endpoint (nenwa muse). Tailscale and the clipboard are not used; the key is received in a file ----

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

// Run nenwa muse and return the URL it printed and the key it wrote
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
    assert!(!out.contains(&key), "the key is not shown on screen");
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

// POST and return (status, body). Err if it cannot connect
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

fn http_muse_no_key_or_wrong_key_gives_401_and_only_the_hash_is_stored(c: &Ctx) {
    until(|| who_has(&c.alice, "bob/api-bbbb@bob-mac").then_some(())); // for when the tests are run with a filter
    let m = muse_on(c, &[]);
    let keys = c.alice.home.join("http").join("keys.json");
    let saved = std::fs::read_to_string(&keys).unwrap();
    assert!(!saved.contains(&m.key), "the key itself is not stored");
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

fn http_initialize_and_tools_list_give_3_tools_and_notifications_get_202(c: &Ctx) {
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

fn http_send_message_from_muse_arrives_with_muse_as_sender(c: &Ctx) {
    let m = muse(c);
    let before = c.bob_session.len();
    // Even when it tries to use another name, it stays muse
    let text = tool(&m, 4, "send_message", json!({ "to": "bob/api-bbbb@bob-mac", "message": "from Muse", "as": "grok" }));
    let res: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(res["from"], "alice/muse@alice-mac");
    assert_eq!(res["state"], "delivered");
    let line = until(|| c.bob_session.after(before));
    let content = line["message"]["content"].as_str().unwrap();
    assert!(
        re(r#"<nenwa-message from="alice/muse@alice-mac" id="[0-9a-f-]+">\nfrom Muse\n</nenwa-message>"#).is_match(content),
        "{content}"
    );
}

fn http_reply_can_be_read_with_check_inbox(c: &Ctx) {
    let m = muse(c);
    let sent = cli_json(&c.bob, &["send", "alice/muse@alice-mac", "a reply to Muse"]);
    assert_eq!(sent["state"], "inbox");
    let text = tool(&m, 5, "check_inbox", json!({ "wait_secs": 5 }));
    assert!(re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\na reply to Muse\n</nenwa-message>"#).is_match(&text), "{text}");
    // When empty it waits, and returns once one arrives (the limit is 25 seconds)
    let mm = m.clone();
    let waiting = std::thread::spawn(move || tool(&mm, 6, "check_inbox", json!({ "wait_secs": 999 })));
    sleep(500);
    cli(&c.bob, &["send", "alice/muse@alice-mac", "the one waited for"]).unwrap();
    assert!(waiting.join().unwrap().contains("the one waited for"));
}

fn http_oversized_body_gives_413_and_rapid_requests_give_429(c: &Ctx) {
    let m = muse(c);
    let big = json!({ "jsonrpc": "2.0", "id": 7, "method": "ping", "params": { "pad": "x".repeat(70 * 1024) } }).to_string();
    assert_eq!(post(&m.url, Some(&m.key), &big).unwrap().0, 413);
    let ping = json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" }).to_string();
    let statuses: Vec<u16> = (0..31).map(|_| post(&m.url, Some(&m.key), &ping).unwrap().0).collect();
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
}

fn http_repeated_wrong_key_requests_give_503(c: &Ctx) {
    let m = muse(c);
    let ping = json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" }).to_string();
    let statuses: Vec<u16> = (0..62).map(|_| post(&m.url, Some("guess"), &ping).unwrap().0).collect();
    assert_eq!(statuses.last(), Some(&503), "{statuses:?}");
    assert_eq!(post(&m.url, Some(&m.key), &ping).unwrap().0, 503, "blocked even with the right key");
}

fn http_unusable_after_off_and_inbox_messages_are_kept(c: &Ctx) {
    let m = muse(c);
    cli(&c.bob, &["send", "alice/muse@alice-mac", "kept after closing"]).unwrap();
    let out = cli(&m.env, &["muse", "--off"]).unwrap();
    assert!(out.contains("Closed the Muse endpoint"), "{out}");
    let ping = json!({ "jsonrpc": "2.0", "id": 10, "method": "ping" }).to_string();
    // The listener is closed (401 if an earlier connection is still open)
    match post(&m.url, Some(&m.key), &ping) {
        Err(_) => {}
        Ok((status, _)) => assert_eq!(status, 401),
    }
    assert!(!c.alice.home.join("http").join("keys.json").exists());
    assert!(!c.alice.home.join("http").join("enabled").exists());
    let bx = cli_json(&c.alice, &["inbox", "--as", "muse"]);
    assert_eq!(bx["messages"][0]["body"], "kept after closing");
}

fn http_closes_after_expiry(c: &Ctx) {
    let m = muse_on(c, &[("NENWA_HTTP_TTL_SECS", "3")]);
    let ping = json!({ "jsonrpc": "2.0", "id": 11, "method": "ping" }).to_string();
    assert_eq!(post(&m.url, Some(&m.key), &ping).unwrap().0, 200, "reopening clears the counts");
    until(|| post(&m.url, Some(&m.key), &ping).map_or(Some(()), |(s, _)| (s == 401).then_some(())));
    until(|| (!c.alice.home.join("http").join("enabled").exists()).then_some(()));
    assert!(!c.alice.home.join("http").join("keys.json").exists());
    assert!(std::fs::read_to_string(c.alice.home.join("test-daemon.log")).unwrap().contains("http expired"));
}

fn http_reopens_after_daemon_restart(c: &Ctx) {
    let m = muse_on(c, &[]);
    stop_daemon(c.daemons.lock().unwrap().remove("alice").unwrap(), &c.alice);
    c.daemons.lock().unwrap().insert("alice", start_daemon(&c.alice));
    let ping = json!({ "jsonrpc": "2.0", "id": 12, "method": "ping" }).to_string();
    until(|| post(&m.url, Some(&m.key), &ping).ok().filter(|(s, _)| *s == 200));
    cli(&m.env, &["muse", "--off"]).unwrap();
    let log = std::fs::read_to_string(c.alice.home.join("test-daemon.log")).unwrap();
    assert!(log.contains("(reopened)"));
    assert!(!log.contains(&m.key), "the key is not written to the log either");
    until(|| who_has(&c.bob, "alice@alice-mac  online").then_some(()));
}

// ---- Agents connected through hooks (Cursor, Codex) ----

const CURSOR_ID: &str = "c0ffee00-1111-2222-3333-44445555beef";
const CURSOR_ADDR: &str = "alice/web-beef@alice-mac";

fn cursor_session_start_sets_address_and_post_tool_use_and_stop_deliver(c: &Ctx) {
    let start = hook_call(
        &c.alice,
        "cursor",
        "session-start",
        json!({ "conversation_id": CURSOR_ID, "session_id": CURSOR_ID, "workspace_roots": ["/work/web"] }),
    )
    .unwrap();
    assert!(start["additional_context"].as_str().unwrap().contains(&format!("address is {CURSOR_ADDR}")));
    until(|| who_has(&c.bob, CURSOR_ADDR).then_some(()));

    let sent = cli_json(&c.bob, &["send", CURSOR_ADDR, "look at the screen bug"]);
    assert_eq!(sent["state"], "inbox");
    // beforeSubmitPrompt cannot add to the conversation, so it is held
    assert_eq!(
        hook_call(&c.alice, "cursor", "user-prompt-submit", json!({ "conversation_id": CURSOR_ID, "prompt": "x" })),
        Some(json!({ "continue": true }))
    );
    let post = hook_call(&c.alice, "cursor", "post-tool-use", json!({ "conversation_id": CURSOR_ID, "tool_name": "Shell" })).unwrap();
    assert!(
        re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\nlook at the screen bug"#)
            .is_match(post["additional_context"].as_str().unwrap())
    );
    assert_eq!(
        hook_call(&c.alice, "cursor", "post-tool-use", json!({ "conversation_id": CURSOR_ID })),
        None,
        "the same one is not given twice"
    );

    cli(&c.bob, &["send", CURSOR_ADDR, "reply when you finish"]).unwrap();
    let stop =
        hook_call(&c.alice, "cursor", "stop", json!({ "conversation_id": CURSOR_ID, "status": "completed", "loop_count": 0 })).unwrap();
    assert!(stop["followup_message"].as_str().unwrap().contains("reply when you finish"));
    assert_eq!(
        hook_call(&c.alice, "cursor", "stop", json!({ "conversation_id": CURSOR_ID, "status": "completed", "loop_count": 1 })),
        None
    );
}

fn cursor_mcp_agent_cursor_send_makes_that_session_the_sender(c: &Ctx) {
    hook_call(&c.alice, "cursor", "post-tool-use", json!({ "conversation_id": CURSOR_ID })); // make it working
    let before = c.bob_session.len();
    let out = mcp_call(
        &c.alice.with(&[("NENWA_AGENT", "cursor")]),
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "send_message", "arguments": { "to": "bob/api-bbbb@bob-mac", "message": "from Cursor" } } }),
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

fn codex_queue_puts_into_conversation_and_hooks_inject_on_failure(c: &Ctx) {
    let start = hook_call(
        &c.alice,
        "codex",
        "session-start",
        json!({ "session_id": CODEX_ID, "cwd": "/work/api", "hook_event_name": "SessionStart", "source": "startup" }),
    )
    .unwrap();
    assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(start["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains(&format!("address is {CODEX_ADDR}")));

    let sent = cli_json(&c.bob, &["send", CODEX_ADDR, "tell me the API type"]);
    assert_eq!(sent["state"], "delivered");
    let logged = std::fs::read_to_string(c.alice.get("FAKE_CODEX_LOG")).unwrap();
    let lines: Vec<&str> = logged.split('\n').collect();
    assert_eq!(lines[..4], ["queue", "--thread", CODEX_ID, "--message"]);
    assert!(re(r#"<nenwa-message from="bob/terminal@bob-mac"[^>]*>\ntell me the API type"#).is_match(&lines[4..].join("\n")));

    // When codex queue fails
    let fail = c.alice.get("FAKE_CODEX_FAIL");
    std::fs::write(&fail, "").unwrap();
    assert_eq!(cli_json(&c.bob, &["send", CODEX_ADDR, "take this through the hook"])["state"], "inbox");
    let post = hook_call(&c.alice, "codex", "post-tool-use", json!({ "session_id": CODEX_ID, "hook_event_name": "PostToolUse" })).unwrap();
    assert_eq!(post["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(post["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("take this through the hook"));
    cli(&c.bob, &["send", CODEX_ADDR, "read this before stopping"]).unwrap();
    let stop =
        hook_call(&c.alice, "codex", "stop", json!({ "session_id": CODEX_ID, "hook_event_name": "Stop", "stop_hook_active": false }))
            .unwrap();
    assert_eq!(stop["decision"], "block");
    assert!(stop["reason"].as_str().unwrap().contains("read this before stopping"));
    assert_eq!(hook_call(&c.alice, "codex", "stop", json!({ "session_id": CODEX_ID, "stop_hook_active": true })), None);
    std::fs::remove_file(&fail).unwrap();

    hook_call(&c.alice, "codex", "session-end", json!({ "session_id": CODEX_ID }));
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(CODEX_ADDR)));
}

// A fake Codex process (sh). Runs the hook and the MCP server as its children, and exits when stdin closes.
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
    // Send one message through MCP and return the sender the daemon decided
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

fn with_two_conversations_in_one_folder_sender_is_the_calling_process_and_dead_ones_leave_the_list(c: &Ctx) {
    if cfg!(windows) {
        return; // uses sh, so unix only
    }
    let (first, second) = ("alice/web-aaa1@alice-mac", "alice/web-aaa2@alice-mac");
    let mut a = FakeAgent::start(c, "019a2b3c-0000-7000-8000-00000000aaa1", "bob/api-bbbb@bob-mac");
    // A conversation opened later. The folder and status are the same, so without looking at the agent process this one would be picked.
    let b = FakeAgent::start(c, "019a2b3c-0000-7000-8000-00000000aaa2", "bob/api-bbbb@bob-mac");
    assert_eq!(a.send(), first);

    a.close();
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(first) && o.contains(second)));
    assert_eq!(cli_json(&c.bob, &["send", first, "already gone"])["state"], "no_session");
    b.close();
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(second)));
}

// OpenCode: the plugin reports the conversation, and the daemon sends to OpenCode's local server to put the message into the conversation.
fn opencode_sends_to_local_server_and_falls_back_to_plugin_hooks(c: &Ctx) {
    let (url, got) = webhook_server();
    let base = url.trim_end_matches("/hook").to_string();
    let addr = "alice/site-9z8y@alice-mac";
    let input = json!({ "session_id": "ses_4f0a1b2c3d9Z8Y", "cwd": "/work/my site/site", "server_url": base });
    // Nothing is added at start (adding would start a response by itself)
    assert_eq!(hook_call(&c.alice, "opencode", "session-start", input.clone()), None);
    until(|| who_has(&c.bob, addr).then_some(()));

    let before = got.lock().unwrap().len();
    assert_eq!(cli_json(&c.bob, &["send", addr, "arrives even when idle"])["state"], "delivered");
    let (headers, body) = got.lock().unwrap()[before].clone();
    assert_eq!(headers[":path"], "/session/ses_4f0a1b2c3d9Z8Y/prompt_async?directory=/work/my%20site/site");
    assert_eq!(body["parts"][0]["type"], "text");
    assert!(body["parts"][0]["text"].as_str().unwrap().contains("arrives even when idle"));

    // Never send to a server elsewhere. It cannot be delivered, so it goes to the inbox and the plugin's hook hands it over.
    let mut away = input.clone();
    away["session_id"] = json!("ses_0000000000aw4y");
    away["server_url"] = json!("http://example.com:80");
    hook_call(&c.alice, "opencode", "user-prompt-submit", away.clone());
    let n = got.lock().unwrap().len();
    assert_eq!(cli_json(&c.bob, &["send", "alice/site-aw4y@alice-mac", "take this through the hook"])["state"], "inbox");
    assert_eq!(got.lock().unwrap().len(), n);
    assert_eq!(hook_call(&c.alice, "opencode", "user-prompt-submit", away.clone()), None, "not taken out at input time");
    let tool = hook_call(&c.alice, "opencode", "post-tool-use", away.clone()).unwrap();
    assert!(tool["text"].as_str().unwrap().contains("take this through the hook"));
    cli(&c.bob, &["send", "alice/site-aw4y@alice-mac", "before stopping"]).unwrap();
    assert!(hook_call(&c.alice, "opencode", "stop", away.clone()).unwrap()["text"].as_str().unwrap().contains("before stopping"));
    assert_eq!(hook_call(&c.alice, "opencode", "stop", away.clone()), None);

    // In a TUI without a server, the plugin receives through its wait call (even when idle, it returns as soon as one arrives)
    let (h, waiting_input) = (c.alice.clone(), away.clone());
    let waiter = std::thread::spawn(move || hook_call(&h, "opencode", "wait", waiting_input));
    sleep(700);
    let sent = Instant::now();
    cli(&c.bob, &["send", "alice/site-aw4y@alice-mac", "while waiting"]).unwrap();
    assert!(waiter.join().unwrap().unwrap()["text"].as_str().unwrap().contains("while waiting"));
    assert!(sent.elapsed() < Duration::from_secs(5), "returns as soon as one arrives");

    hook_call(&c.alice, "opencode", "session-end", away);
    hook_call(&c.alice, "opencode", "session-end", input);
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains("alice/site-")));
}

// Pi: the extension waits for arrivals with "wait". If one arrives while waiting, it returns at once.
fn pi_extension_wait_gets_arrivals_at_once(c: &Ctx) {
    let addr = "alice/tool-91ab@alice-mac";
    let input = json!({ "session_id": "0c1d2e3f-4a5b-4c6d-8e7f-0000000091ab", "cwd": "/work/tool" });
    let start = hook_call(&c.alice, "pi", "session-start", input.clone()).unwrap();
    assert!(start["text"].as_str().unwrap().contains(&format!("address is {addr}")));
    hook_call(&c.alice, "pi", "user-prompt-submit", input.clone());
    until(|| who_has(&c.bob, &format!("{addr}  [busy]")).then_some(()));

    let h = c.alice.clone();
    let waiting_input = input.clone();
    let waiter = std::thread::spawn(move || (Instant::now(), hook_call(&h, "pi", "wait", waiting_input)));
    sleep(700);
    assert!(who_has(&c.bob, &format!("{addr}  [busy]")), "a wait call does not change the status");
    let sent = Instant::now();
    cli(&c.bob, &["send", addr, "while waiting"]).unwrap();
    let (_, got) = waiter.join().unwrap();
    assert!(got.unwrap()["text"].as_str().unwrap().contains("while waiting"));
    assert!(sent.elapsed() < Duration::from_secs(5), "returns as soon as one arrives");

    assert_eq!(hook_call(&c.alice, "pi", "stop", input.clone()), None);
    hook_call(&c.alice, "pi", "session-end", input);
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(addr)));
}

// Antigravity CLI: the conversation ID is conversationId. Added with injectSteps before the model is called, and Stop makes it continue with continue.
fn antigravity_cli_delivers_before_model_call_and_at_end_of_response(c: &Ctx) {
    let addr = "alice/lab-7e57@alice-mac";
    let input = json!({ "conversationId": "b1b2b3b4-0000-4000-8000-000000007e57", "workspacePaths": ["/work/lab"], "hookEventName": "SessionStart" });
    let start = hook_call(&c.alice, "antigravity", "session-start", input.clone()).unwrap();
    assert!(start["injectSteps"][0]["ephemeralMessage"].as_str().unwrap().contains(&format!("address is {addr}")));
    assert_eq!(cli_json(&c.bob, &["send", addr, "before the model"])["state"], "inbox");
    let pre = hook_call(&c.alice, "antigravity", "post-tool-use", input.clone()).unwrap();
    assert!(pre["injectSteps"][0]["ephemeralMessage"].as_str().unwrap().contains("before the model"));
    assert_eq!(hook_call(&c.alice, "antigravity", "post-tool-use", input.clone()), None);
    cli(&c.bob, &["send", addr, "before stopping"]).unwrap();
    let stop = hook_call(&c.alice, "antigravity", "stop", input.clone()).unwrap();
    assert_eq!(stop["decision"], "continue");
    assert!(stop["reason"].as_str().unwrap().contains("before stopping"));
    hook_call(&c.alice, "antigravity", "session-end", input);
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(addr)));
}

// Copilot CLI: the conversation ID is sessionId. Context goes in additionalContext at the top level. Nothing can be added at input time.
fn copilot_cli_hooks_set_address_and_deliver_after_tools_and_at_end_of_response(c: &Ctx) {
    let addr = "alice/app-c0p1@alice-mac";
    let input = json!({ "sessionId": "5a6b7c8d-0000-4000-8000-00000000c0p1", "cwd": "/work/app", "timestamp": 1795000000000u64 });
    let start = hook_call(&c.alice, "copilot", "session-start", input.clone()).unwrap();
    assert!(start["additionalContext"].as_str().unwrap().contains(&format!("address is {addr}")));
    assert_eq!(cli_json(&c.bob, &["send", addr, "after the tool"])["state"], "inbox");
    assert_eq!(hook_call(&c.alice, "copilot", "user-prompt-submit", input.clone()), None);
    let tool = hook_call(&c.alice, "copilot", "post-tool-use", input.clone()).unwrap();
    assert!(tool["additionalContext"].as_str().unwrap().contains("after the tool"));
    cli(&c.bob, &["send", addr, "before stopping"]).unwrap();
    let stop = hook_call(&c.alice, "copilot", "stop", input.clone()).unwrap();
    assert_eq!(stop["decision"], "block");
    assert!(stop["reason"].as_str().unwrap().contains("before stopping"));
    assert_eq!(hook_call(&c.alice, "copilot", "stop", input.clone()), None);
    hook_call(&c.alice, "copilot", "session-end", input);
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(addr)));
}

const GEMINI_ID: &str = "7d1e2f3a-aaaa-bbbb-cccc-00000000f00d";
const GEMINI_ADDR: &str = "alice/docs-f00d@alice-mac";

// Gemini CLI has different event names, and the word that makes it continue at the end of a response is deny. Qwen Code has the same form as Codex.
fn gemini_cli_and_qwen_code_hooks_set_address_and_deliver_in_each_form(c: &Ctx) {
    let input = |event: &str| json!({ "session_id": GEMINI_ID, "cwd": "/work/docs", "hook_event_name": event });
    let start = hook_call(&c.alice, "gemini", "session-start", input("SessionStart")).unwrap();
    assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(start["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains(&format!("address is {GEMINI_ADDR}")));
    assert!(who_has(&c.bob, GEMINI_ADDR) || until(|| who_has(&c.bob, GEMINI_ADDR).then_some(true)));

    // There is no way into the conversation from outside, so it goes to the inbox and the next hook hands it over
    assert_eq!(cli_json(&c.bob, &["send", GEMINI_ADDR, "before the input"])["state"], "inbox");
    let prompt = hook_call(&c.alice, "gemini", "user-prompt-submit", input("BeforeAgent")).unwrap();
    assert_eq!(prompt["hookSpecificOutput"]["hookEventName"], "BeforeAgent");
    assert!(prompt["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("before the input"));
    cli(&c.bob, &["send", GEMINI_ADDR, "after the tool"]).unwrap();
    let tool = hook_call(&c.alice, "gemini", "post-tool-use", input("AfterTool")).unwrap();
    assert_eq!(tool["hookSpecificOutput"]["hookEventName"], "AfterTool");
    assert!(tool["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("after the tool"));
    cli(&c.bob, &["send", GEMINI_ADDR, "before stopping"]).unwrap();
    let stop = hook_call(&c.alice, "gemini", "stop", input("AfterAgent")).unwrap();
    assert_eq!(stop["decision"], "deny");
    assert!(stop["reason"].as_str().unwrap().contains("before stopping"));
    assert_eq!(hook_call(&c.alice, "gemini", "stop", input("AfterAgent")), None);

    // Sending through MCP (--agent gemini) makes that conversation the sender
    let out = mcp_call(
        &c.alice.with(&[("NENWA_AGENT", "gemini")]),
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "send_message", "arguments": { "to": "bob/api-bbbb@bob-mac", "message": "from Gemini" } } }),
        ],
    );
    let res: Value = serde_json::from_str(by_id(&out, 2)["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(res["from"], GEMINI_ADDR);
    hook_call(&c.alice, "gemini", "session-end", input("SessionEnd"));
    until(|| cli(&c.bob, &["who"]).ok().filter(|o| !o.contains(GEMINI_ADDR)));

    let qwen = "alice/docs-beef@alice-mac";
    let input = json!({ "session_id": "11112222-3333-4444-5555-66667777beef", "cwd": "/work/docs" });
    hook_call(&c.alice, "qwen", "session-start", input.clone()).unwrap();
    cli(&c.bob, &["send", qwen, "to Qwen"]).unwrap();
    let post = hook_call(&c.alice, "qwen", "post-tool-use", input.clone()).unwrap();
    assert_eq!(post["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(post["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("to Qwen"));
    cli(&c.bob, &["send", qwen, "before stopping"]).unwrap();
    assert_eq!(hook_call(&c.alice, "qwen", "stop", input.clone()).unwrap()["decision"], "block");
    hook_call(&c.alice, "qwen", "session-end", input);
}

fn hook_exits_cleanly_without_output_when_no_daemon(c: &Ctx) {
    let lonely = c.alice.with(&[("NENWA_HOME", &c.root.join("nobody").display().to_string())]);
    assert_eq!(hook_call(&lonely, "codex", "post-tool-use", json!({ "session_id": CODEX_ID })), None);
}

fn setup_adds_while_keeping_other_settings_and_remove_restores(c: &Ctx) {
    let codex = c.root.join("codex-home");
    let cursor = c.root.join("cursor-home");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::create_dir_all(&cursor).unwrap();
    let toml = "[mcp_servers.other]\ncommand = \"other\"\n";
    std::fs::write(codex.join("config.toml"), toml).unwrap();
    let other_hook = json!({ "hooks": [{ "type": "command", "command": "echo other" }], "matcher": "*" });
    std::fs::write(codex.join("hooks.json"), json!({ "hooks": { "PostToolUse": [other_hook] } }).to_string()).unwrap();
    std::fs::write(cursor.join("mcp.json"), json!({ "mcpServers": { "other": { "command": "other" } } }).to_string()).unwrap();
    // nenwa hooks pointing at an old location (before the folder moved, or the TS version) are replaced and do not pile up
    let stale_ts = json!({ "command": "'/usr/bin/node' '/old/place/src/cli.mjs' hook cursor stop" });
    let stale = json!({ "command": "'/old/bin/nenwa' hook cursor stop" });
    std::fs::write(
        cursor.join("hooks.json"),
        json!({ "version": 1, "hooks": { "stop": [{ "command": "echo other" }, stale_ts, stale] } }).to_string(),
    )
    .unwrap();
    // Gemini CLI: keeps other settings and hooks. Qwen Code: a config that cannot be parsed (comments) is not touched.
    let (gemini, qwen) = (c.root.join("gemini-home"), c.root.join("qwen-home"));
    std::fs::create_dir_all(&gemini).unwrap();
    std::fs::create_dir_all(&qwen).unwrap();
    let gemini_before = json!({
        "theme": "Default",
        "mcpServers": { "other": { "command": "other" } },
        "hooks": { "SessionStart": [{ "matcher": "", "hooks": [{ "type": "command", "command": "echo other" }] }] }
    });
    std::fs::write(gemini.join("settings.json"), gemini_before.to_string()).unwrap();
    let qwen_text = "{\n  // comment\n  \"theme\": \"Default\"\n}\n";
    std::fs::write(qwen.join("settings.json"), qwen_text).unwrap();
    let opencode = c.root.join("opencode-home");
    std::fs::create_dir_all(opencode.join("plugins")).unwrap();
    std::fs::write(opencode.join("plugins").join("other.js"), "// other").unwrap();
    let opencode_before = json!({ "mcp": { "other": { "type": "local", "command": ["other"] } } });
    std::fs::write(opencode.join("opencode.json"), opencode_before.to_string()).unwrap();
    let copilot = c.root.join("copilot-home");
    std::fs::create_dir_all(copilot.join("hooks")).unwrap();
    std::fs::write(copilot.join("hooks").join("other.json"), "{}").unwrap();
    let copilot_before = json!({ "mcpServers": { "other": { "type": "local", "command": "other", "tools": ["*"] } } });
    std::fs::write(copilot.join("mcp-config.json"), copilot_before.to_string()).unwrap();
    let pi = c.root.join("pi-home");
    std::fs::create_dir_all(&pi).unwrap();
    let agy = c.root.join("antigravity-config");
    std::fs::create_dir_all(&agy).unwrap();
    let agy_hooks = json!({ "other": { "Stop": [{ "type": "command", "command": "echo other" }] } });
    std::fs::write(agy.join("hooks.json"), agy_hooks.to_string()).unwrap();
    let h = c.alice.with(&[
        ("NENWA_ANTIGRAVITY_DIR", &agy.display().to_string()),
        ("NENWA_PI_DIR", &pi.display().to_string()),
        ("NENWA_COPILOT_DIR", &copilot.display().to_string()),
        ("NENWA_OPENCODE_DIR", &opencode.display().to_string()),
        ("NENWA_CODEX_DIR", &codex.display().to_string()),
        ("NENWA_CURSOR_DIR", &cursor.display().to_string()),
        ("NENWA_GEMINI_DIR", &gemini.display().to_string()),
        ("NENWA_QWEN_DIR", &qwen.display().to_string()),
    ]);
    let read = |p: PathBuf| -> Value { serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap() };

    cli(&h, &["setup"]).unwrap();
    cli(&h, &["setup"]).unwrap(); // running it twice does not duplicate anything
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
    let o = read(opencode.join("opencode.json"));
    assert_eq!(o["mcp"]["other"]["command"], json!(["other"]));
    assert_eq!(o["mcp"]["nenwa"]["type"], "local");
    assert_eq!(o["mcp"]["nenwa"]["command"].as_array().unwrap()[1..], [json!("mcp"), json!("--agent"), json!("opencode")]);
    let plugin = std::fs::read_to_string(opencode.join("plugins").join("nenwa.js")).unwrap();
    assert!(re(r#"const NENWA = ".*nenwa(\.exe)?";"#).is_match(&plugin), "the executable's location is filled in");
    assert!(!plugin.contains("__NENWA_EXE__"));
    let ext = std::fs::read_to_string(pi.join("extensions").join("nenwa.ts")).unwrap();
    assert!(re(r#"const NENWA = ".*nenwa(\.exe)?";"#).is_match(&ext));
    let ah = read(agy.join("hooks.json"));
    assert_eq!(ah["other"], agy_hooks["other"]);
    let mut keys: Vec<&String> = ah["nenwa"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["PreInvocation", "SessionStart", "Stop"]);
    assert!(re(r#"nenwa(\.exe)?['"] hook antigravity stop$"#).is_match(ah["nenwa"]["Stop"][0]["command"].as_str().unwrap()));
    assert_eq!(read(agy.join("mcp_config.json"))["mcpServers"]["nenwa"]["args"], json!(["mcp", "--agent", "antigravity"]));
    let cp = read(copilot.join("mcp-config.json"))["mcpServers"].clone();
    assert_eq!(cp["other"]["command"], "other");
    assert_eq!(
        cp["nenwa"],
        json!({ "type": "local", "command": cp["nenwa"]["command"], "args": ["mcp", "--agent", "copilot"], "tools": ["*"] })
    );
    let ch = read(copilot.join("hooks").join("nenwa.json"));
    assert_eq!(ch["version"], 1);
    let mut keys: Vec<&String> = ch["hooks"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["agentStop", "postToolUse", "sessionEnd", "sessionStart", "userPromptSubmitted"]);
    assert!(re(r"^'.*nenwa(\.exe)?' hook copilot stop$").is_match(ch["hooks"]["agentStop"][0]["bash"].as_str().unwrap()));
    assert!(re(r"^& '.*nenwa(\.exe)?' hook copilot stop$").is_match(ch["hooks"]["agentStop"][0]["powershell"].as_str().unwrap()));

    let doc = cli(&h, &["doctor"]).unwrap();
    assert!(re(r"Codex\s+\S+\s+done").is_match(&doc), "{doc}");
    assert!(re(r"Qwen Code\s+\S+\s+not yet").is_match(&doc), "a config that could not be parsed stays unregistered: {doc}");
    assert!(doc.contains("Daemon: running"));

    cli(&h, &["setup", "--remove"]).unwrap();
    assert_eq!(read(gemini.join("settings.json")), gemini_before);
    assert_eq!(read(opencode.join("opencode.json")), opencode_before);
    assert!(!opencode.join("plugins").join("nenwa.js").exists());
    assert!(opencode.join("plugins").join("other.js").exists());
    assert_eq!(read(copilot.join("mcp-config.json")), copilot_before);
    assert!(!pi.join("extensions").join("nenwa.ts").exists());
    assert_eq!(read(agy.join("hooks.json")), agy_hooks);
    assert_eq!(read(agy.join("mcp_config.json"))["mcpServers"], json!({}));
    assert!(!copilot.join("hooks").join("nenwa.json").exists());
    assert!(copilot.join("hooks").join("other.json").exists());
    assert_eq!(std::fs::read_to_string(codex.join("config.toml")).unwrap().trim(), toml.trim());
    assert_eq!(read(codex.join("hooks.json"))["hooks"], json!({ "PostToolUse": [other_hook] }));
    assert_eq!(read(cursor.join("mcp.json"))["mcpServers"], json!({ "other": { "command": "other" } }));
    assert_eq!(read(cursor.join("hooks.json"))["hooks"], json!({ "stop": [{ "command": "echo other" }] }));
}

fn offline_peer_goes_to_outbox_and_delivers_on_return(c: &Ctx) {
    stop_daemon(c.daemons.lock().unwrap().remove("bob").unwrap(), &c.bob);
    until(|| who_has(&c.alice, "offline").then_some(()));
    let before = c.bob_session.len();
    let res = cli_json(&c.alice, &["send", "bob/api-bbbb@bob-mac", "read this when you are back"]);
    assert_eq!(res["state"], "queued");
    c.daemons.lock().unwrap().insert("bob", start_daemon(&c.bob));
    let line = until(|| c.bob_session.after(before));
    assert!(line["message"]["content"].as_str().unwrap().contains("read this when you are back"));
    until(|| cli(&c.alice, &["who"]).ok().filter(|o| !o.contains("Unsent")));
}

fn p2p_off_still_delivers_inside_this_computer_and_invite_turns_it_on(c: &Ctx) {
    let carol = c.make_home("carol", true);
    let cfg_file = carol.home.join("config.json");
    let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&cfg_file).unwrap()).unwrap();
    cfg["network"] = json!(false);
    std::fs::write(&cfg_file, cfg.to_string()).unwrap();
    let session = fake_session(&carol, "notes", "cccc2222-0000-0000-0000-000000000000");
    let run = || {
        let out = cli(&carol, &["who"]).unwrap(); // the daemon starts automatically
        assert!(out.contains("inside this computer only"), "{out}");
        assert!(out.contains("carol/notes-cccc@carol-mac"));
        assert!(std::fs::read_to_string(carol.home.join("daemon.log")).unwrap().contains("local only"));
        let res = cli_json(&carol, &["send", "carol/notes-cccc@carol-mac", "delivered inside only"]);
        assert_eq!(res["state"], "delivered");
        assert!(until(|| session.last())["message"]["content"].as_str().unwrap().contains("delivered inside only"));

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
        until(|| cli(&carol, &["who"]).ok().filter(|o| !o.contains("inside this computer only")));
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    // Stop the daemon that was started automatically
    shutdown(&carol);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// When called as the fake codex: record the arguments passed to queue. Fail if codex-fail exists.
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
        ("the peer's session appears in the list", peer_session_appears_in_the_list),
        ("sending from the terminal arrives framed in the peer's session", send_from_terminal_arrives_framed_in_peer_session),
        (
            "after /clear changes the conversation ID, the old address still delivers and the sender uses the current name",
            after_clear_old_address_still_delivers_and_sender_uses_current_name,
        ),
        ("sending to a session that is not there gives no_session", missing_session_gives_no_session),
        (
            "an arrival in an inbox wakes by webhook and can be read as with check_inbox",
            inbox_arrival_wakes_by_webhook_and_reads_like_check_inbox,
        ),
        ("an empty inbox waits with --wait until a message arrives", empty_inbox_waits_with_wait_until_arrival),
        ("a reply under the inbox name reaches the peer's session", reply_under_inbox_name_reaches_peer_session),
        ("a reply to a message sent from the terminal goes to the terminal inbox", reply_to_terminal_send_goes_to_terminal_inbox),
        ("MCP: check_inbox appears only with NENWA_AS", mcp_check_inbox_appears_only_with_nenwa_as),
        (
            "HTTP (nenwa muse): no key or a wrong key gives 401. Only the hash of the key is stored",
            http_muse_no_key_or_wrong_key_gives_401_and_only_the_hash_is_stored,
        ),
        (
            "HTTP: initialize and tools/list give 3 tools. Notifications get 202",
            http_initialize_and_tools_list_give_3_tools_and_notifications_get_202,
        ),
        ("HTTP: send_message from muse arrives with muse as the sender", http_send_message_from_muse_arrives_with_muse_as_sender),
        ("HTTP: a reply can be read with check_inbox", http_reply_can_be_read_with_check_inbox),
        ("HTTP: an oversized body gives 413, rapid requests give 429", http_oversized_body_gives_413_and_rapid_requests_give_429),
        ("HTTP: repeated wrong-key requests give 503", http_repeated_wrong_key_requests_give_503),
        ("HTTP: unusable after --off. Messages in the inbox are kept", http_unusable_after_off_and_inbox_messages_are_kept),
        ("HTTP: closes after it expires", http_closes_after_expiry),
        ("HTTP: reopens after the daemon restarts", http_reopens_after_daemon_restart),
        (
            "Cursor: sessionStart sets the address, postToolUse and stop deliver",
            cursor_session_start_sets_address_and_post_tool_use_and_stop_deliver,
        ),
        (
            "Cursor: sending through MCP (--agent cursor) makes that session the sender",
            cursor_mcp_agent_cursor_send_makes_that_session_the_sender,
        ),
        (
            "Codex: codex queue puts it into the conversation. On failure the hooks inject it",
            codex_queue_puts_into_conversation_and_hooks_inject_on_failure,
        ),
        (
            "Codex: with two conversations in the same folder, the sender is decided by the calling process. A conversation whose process is gone leaves the list",
            with_two_conversations_in_one_folder_sender_is_the_calling_process_and_dead_ones_leave_the_list,
        ),
        (
            "OpenCode: sends to the local server to put it into the conversation. If that fails, the plugin's hooks hand it over",
            opencode_sends_to_local_server_and_falls_back_to_plugin_hooks,
        ),
        ("Pi: the extension's wait call gets an arrival at once", pi_extension_wait_gets_arrivals_at_once),
        (
            "Antigravity CLI: delivers before the model is called and at the end of a response",
            antigravity_cli_delivers_before_model_call_and_at_end_of_response,
        ),
        (
            "Copilot CLI: the hooks set the address, and messages arrive after a tool and at the end of a response",
            copilot_cli_hooks_set_address_and_deliver_after_tools_and_at_end_of_response,
        ),
        (
            "Gemini CLI and Qwen Code: the hooks set the address, and messages arrive in each one's form",
            gemini_cli_and_qwen_code_hooks_set_address_and_deliver_in_each_form,
        ),
        ("hook: exits normally without output even when there is no daemon", hook_exits_cleanly_without_output_when_no_daemon),
        (
            "setup: adds to each agent while keeping other settings, and --remove restores them",
            setup_adds_while_keeping_other_settings_and_remove_restores,
        ),
        (
            "when the peer is offline the message goes to the outbox and is delivered when it returns",
            offline_peer_goes_to_outbox_and_delivers_on_return,
        ),
        (
            "with P2P off (what install creates first) messages still arrive inside this computer. invite turns it on",
            p2p_off_still_delivers_inside_this_computer_and_invite_turns_it_on,
        ),
    ];
    // cargo test -- <string> runs only the tests whose name contains that string (flags such as --list are ignored)
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
        eprintln!("Set NENWA_TEST_KEEP=1 to keep the logs in {}", ctx.root.display());
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
