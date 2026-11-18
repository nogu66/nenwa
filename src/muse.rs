// nenwa muse: Muse（パソコンの上でコマンドを起動できないアシスタント）をつなぐ。
// 受信箱 muse を作り、鍵を作ってクリップボードに入れ、デーモンの HTTP の入り口（http.rs）を開き、Tailscale Funnel で外に出す。
// 既定では 2 時間で閉じる（デーモンが期限を見て、入り口と鍵と Funnel を閉じる）。
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::http;
use crate::os;
use crate::state::{ask, now_ms};

const MAILBOX: &str = "muse";
// PATH になければ探す場所（launchd から動くデーモンの PATH は狭い）
const FALLBACKS: &[&str] =
    &["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale"];

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

// テストでは Tailscale にもクリップボードにも触らない
fn no_tailscale() -> bool {
    env("NENWA_TEST_NO_TAILSCALE").is_some()
}

fn port() -> Result<u16> {
    match env("NENWA_HTTP_PORT") {
        None => Ok(http::DEFAULT_PORT),
        Some(p) => p.trim().parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| anyhow::anyhow!("NENWA_HTTP_PORT が正しくありません")),
    }
}

// ---- Tailscale ----
pub fn tailscale_bin() -> Option<PathBuf> {
    let name = if cfg!(windows) { "tailscale.exe" } else { "tailscale" };
    let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|f| f.is_file()));
    on_path.or_else(|| FALLBACKS.iter().map(PathBuf::from).find(|p| p.is_file()))
}

fn status_json(bin: &Path, args: &[&str]) -> Option<Value> {
    let out = Command::new(bin).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

// Funnel の設定のうち、この入り口（127.0.0.1:<port>）を外に出しているもの: (ホスト名, HTTPS のポート)
fn our_funnel(bin: &Path, port: u16) -> Option<(String, String)> {
    let st = status_json(bin, &["funnel", "status", "--json"])?;
    let targets = [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")];
    st["Web"].as_object()?.iter().find_map(|(host_port, web)| {
        let ours = web["Handlers"].as_object()?.values().any(|h| {
            let proxy = h["Proxy"].as_str().unwrap_or("").trim_end_matches('/');
            targets.iter().any(|t| proxy == t)
        });
        if !ours || st["AllowFunnel"][host_port] != true {
            return None;
        }
        let (host, p) = host_port.rsplit_once(':')?;
        Some((host.to_string(), p.to_string()))
    })
}

// Tailscale を動かす。すぐ終わればその出力は見せない（失敗したときだけ見せる）。
// 2 秒たっても終わらないときは、Funnel の有効化を待っているなどなので、そこからは出力をそのまま見せる。
fn run_tailscale(bin: &Path, args: &[&str]) -> Result<()> {
    let mut child = Command::new(bin).args(args).stdin(Stdio::inherit()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<_> = [child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>), child.stderr.take().map(|e| Box::new(e) as _)]
        .into_iter()
        .flatten()
        .map(|r| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(r).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            })
        })
        .collect();
    drop(tx);
    let started = Instant::now();
    let (mut held, mut live) = (vec![], false);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if live => eprintln!("  {line}"),
            Ok(line) => held.push(line),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !live && started.elapsed() > Duration::from_secs(2) {
            live = true;
            for line in held.drain(..) {
                eprintln!("  {line}");
            }
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait()?;
    if !status.success() {
        for line in held {
            eprintln!("  {line}");
        }
        bail!(
            "tailscale {} が失敗しました（上の表示を見てください。Funnel が無効なら、表示された URL で有効にしてからもう一度）",
            args.join(" ")
        );
    }
    Ok(())
}

fn funnel_on(bin: &Path, port: u16) -> Result<String> {
    run_tailscale(bin, &["funnel", "--bg", &port.to_string()])?;
    // 設定が status に出るまで少し待つことがある
    for _ in 0..20 {
        if let Some((host, p)) = our_funnel(bin, port) {
            return Ok(if p == "443" { format!("https://{host}/mcp") } else { format!("https://{host}:{p}/mcp") });
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let dns =
        status_json(bin, &["status", "--json"]).and_then(|s| s["Self"]["DNSName"].as_str().map(|d| d.trim_end_matches('.').to_string()));
    match dns.filter(|d| !d.is_empty()) {
        Some(d) => Ok(format!("https://{d}/mcp")),
        None => bail!("Funnel の URL が分かりませんでした（tailscale funnel status で確かめてください）"),
    }
}

// この入り口を出している Funnel だけを閉じる（ほかの用途の Funnel には触らない）
pub fn funnel_off(port: u16) -> Result<()> {
    if no_tailscale() {
        return Ok(());
    }
    let Some(bin) = tailscale_bin() else { return Ok(()) };
    let Some((_, https)) = our_funnel(&bin, port) else { return Ok(()) };
    run_tailscale(&bin, &["funnel", &format!("--https={https}"), "off"])
}

// ---- 鍵の受け渡し ----
fn copy_to_clipboard(text: &str) -> bool {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    candidates.iter().any(|(cmd, args)| {
        let Ok(mut p) = Command::new(cmd).args(*args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
            return false;
        };
        let wrote = p.stdin.take().is_some_and(|mut i| i.write_all(text.as_bytes()).is_ok());
        p.wait().is_ok_and(|s| s.success()) && wrote
    })
}

// 鍵は画面に出さない。ファイルを指定したときはそこへ（権限 600）、なければクリップボードへ。
fn hand_over(key: &str, key_out: Option<&str>) -> Result<String> {
    if let Some(path) = key_out {
        let p = Path::new(path);
        os::write_private(p, &format!("{key}\n"))?;
        os::set_private(p);
        return Ok(format!("{path} に書きました（権限 600。貼ったら消してください）"));
    }
    if no_tailscale() {
        bail!("テストでは --key-out か NENWA_KEY_FILE で鍵の置き場を指定してください");
    }
    if !copy_to_clipboard(key) {
        bail!("鍵をクリップボードに入れられませんでした。--key-out <ファイル> で書き出せます");
    }
    Ok("クリップボードに入れました（画面には出しません）".into())
}

fn hours_text(ttl_ms: Option<u64>) -> String {
    match ttl_ms {
        None => "時間では閉じません。nenwa muse --off で閉じてください".into(),
        Some(ms) if ms % 3_600_000 == 0 => format!("{} 時間後に自動で閉じます", ms / 3_600_000),
        Some(ms) if ms % 60_000 == 0 => format!("{} 分後に自動で閉じます", ms / 60_000),
        Some(ms) => format!("{} 秒後に自動で閉じます", ms.div_ceil(1000)),
    }
}

fn off() -> Result<()> {
    let port = http::load_enabled().map_or(port()?, |e| e.port);
    if let Err(e) = funnel_off(port) {
        eprintln!("{e}");
    }
    let res = ask(json!({ "op": "http", "on": false }));
    http::clear();
    res?;
    println!("Muse の入り口を閉じました（鍵は消しました。受信箱 {MAILBOX} の中身は残っています）");
    Ok(())
}

// --名前 値 を取り出す
fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| *a == format!("--{name}"))?;
    let v = args.get(i + 1)?.clone();
    args.drain(i..i + 2);
    Some(v)
}

pub fn muse_command(args: &mut Vec<String>) -> Result<()> {
    if args.iter().any(|a| a == "--off") {
        return off();
    }
    let hours = match flag(args, "hours") {
        None => 2.0,
        Some(h) => match h.trim().parse::<f64>() {
            Ok(h) if h.is_finite() && h >= 0.0 => h,
            _ => bail!("--hours は 0 以上の数で（0 なら時間では閉じない）"),
        },
    };
    let key_out = flag(args, "key-out").or_else(|| env("NENWA_KEY_FILE"));
    let ttl_ms = match env("NENWA_HTTP_TTL_SECS").and_then(|s| s.trim().parse::<u64>().ok()) {
        Some(secs) => Some(secs * 1000),
        None => Some((hours * 3_600_000.0).round() as u64).filter(|ms| *ms > 0),
    };
    let port = port()?;
    let funnel = !no_tailscale();
    let bin = if funnel {
        match tailscale_bin() {
            Some(b) => Some(b),
            None => bail!("Tailscale が見つかりません。https://tailscale.com/download から入れてサインインしてから、もう一度"),
        }
    } else {
        None
    };

    let address = ask(json!({ "op": "register", "name": MAILBOX }))?["address"].as_str().unwrap_or("").to_string();
    let key = http::new_key();
    let where_key = hand_over(&key, key_out.as_deref())?;
    http::save_key(MAILBOX, &key)?;
    drop(key);
    let expires = ttl_ms.map(|ms| now_ms() + ms);
    ask(json!({ "op": "http", "on": true, "port": port, "expires": expires, "funnel": funnel }))?;

    let url = match &bin {
        None => format!("http://127.0.0.1:{port}/mcp"),
        Some(bin) => match funnel_on(bin, port) {
            Ok(u) => u,
            Err(e) => {
                let _ = funnel_off(port);
                let _ = ask(json!({ "op": "http", "on": false }));
                http::clear();
                return Err(e);
            }
        },
    };

    println!(
        "Muse の入り口を開きました（{}）

  URL:  {url}
  鍵:   {where_key}
  宛先: {address}

Muse でコネクタ（MCP）を足し、URL を入れて、鍵を Bearer トークンとして安全な入力欄に貼ってください。
  - Muse 側で、このコネクタは「操作のたびに承認」にしてください。
  - 開いている間は、権限確認を飛ばすモードの Claude Code の会話を開かないでください（Muse からの頼みがそのまま動きます）。
  - この Mac が寝ている間はつながりません。
  - もう一度 nenwa muse を実行すると鍵を作り直します（古い鍵はすぐ使えなくなります）。閉じるときは nenwa muse --off",
        hours_text(ttl_ms)
    );
    Ok(())
}
