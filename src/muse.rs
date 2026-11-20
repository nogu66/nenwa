// nenwa muse: connect Muse (an assistant that cannot start a command on this computer).
// Creates the muse inbox, makes a key and puts it on the clipboard, opens the daemon's HTTP endpoint (http.rs), and publishes it through Tailscale Funnel.
// By default it closes after 2 hours (the daemon watches the expiry and closes the endpoint, the key and Funnel).
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
// Where to look if it is not on PATH (the PATH of a daemon run from launchd is narrow)
const FALLBACKS: &[&str] =
    &["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale"];

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

// In tests, touch neither Tailscale nor the clipboard
fn no_tailscale() -> bool {
    env("NENWA_TEST_NO_TAILSCALE").is_some()
}

fn port() -> Result<u16> {
    match env("NENWA_HTTP_PORT") {
        None => Ok(http::DEFAULT_PORT),
        Some(p) => p.trim().parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| anyhow::anyhow!("NENWA_HTTP_PORT is not valid")),
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

// Of the Funnel configs, the ones publishing this endpoint (127.0.0.1:<port>): (host name, HTTPS port)
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

// Run Tailscale. If it finishes quickly, its output is not shown (only on failure).
// If it has not finished after 2 seconds, it is waiting for Funnel to be enabled or the like, so from then on the output is shown as it comes.
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
            "tailscale {} failed (see the output above. If Funnel is disabled, enable it at the URL shown and run this again)",
            args.join(" ")
        );
    }
    Ok(())
}

fn funnel_on(bin: &Path, port: u16) -> Result<String> {
    run_tailscale(bin, &["funnel", "--bg", &port.to_string()])?;
    // It can take a moment for the config to show up in status
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
        None => bail!("Could not find the Funnel URL (check with tailscale funnel status)"),
    }
}

// Close only the Funnel that publishes this endpoint (Funnels for other uses are not touched)
pub fn funnel_off(port: u16) -> Result<()> {
    if no_tailscale() {
        return Ok(());
    }
    let Some(bin) = tailscale_bin() else { return Ok(()) };
    let Some((_, https)) = our_funnel(&bin, port) else { return Ok(()) };
    run_tailscale(&bin, &["funnel", &format!("--https={https}"), "off"])
}

// ---- Handing over the key ----
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

// The key is not shown on screen. If a file is given it goes there (mode 600); otherwise to the clipboard.
fn hand_over(key: &str, key_out: Option<&str>) -> Result<String> {
    if let Some(path) = key_out {
        let p = Path::new(path);
        os::write_private(p, &format!("{key}\n"))?;
        os::set_private(p);
        return Ok(format!("written to {path} (mode 600; delete it after pasting)"));
    }
    if no_tailscale() {
        bail!("In tests, give the key location with --key-out or NENWA_KEY_FILE");
    }
    if !copy_to_clipboard(key) {
        bail!("Could not copy the key to the clipboard. Use --key-out <file> to write it to a file");
    }
    Ok("copied to the clipboard (not shown on screen)".into())
}

fn hours_text(ttl_ms: Option<u64>) -> String {
    match ttl_ms {
        None => "it does not close by itself; close it with nenwa muse --off".into(),
        Some(ms) if ms % 3_600_000 == 0 => format!("closes by itself in {} hour(s)", ms / 3_600_000),
        Some(ms) if ms % 60_000 == 0 => format!("closes by itself in {} minute(s)", ms / 60_000),
        Some(ms) => format!("closes by itself in {} second(s)", ms.div_ceil(1000)),
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
    println!("Closed the Muse endpoint (the key is deleted; messages in the {MAILBOX} inbox are kept)");
    Ok(())
}

// Takes out --name value
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
            _ => bail!("--hours must be a number 0 or greater (0 keeps it open)"),
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
            None => bail!("Tailscale was not found. Install it from https://tailscale.com/download, sign in, and run this again"),
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
        "Opened the Muse endpoint ({})

  URL:     {url}
  Key:     {where_key}
  Address: {address}

In Muse, add an MCP connector, enter the URL, and paste the key as the Bearer token into a secure input field.
  - In Muse, set this connector to approve every action.
  - While it is open, do not keep Claude Code conversations running in a mode that skips permission prompts (a request from Muse would run there as it is).
  - Nothing reaches this Mac while it sleeps.
  - Running nenwa muse again makes a new key (the old one stops working at once). To close it: nenwa muse --off",
        hours_text(ttl_ms)
    );
    Ok(())
}
