// nenwa: lets your own agents (Claude Code, Codex, Cursor, Grok Bot) send messages to each other.
// This file only dispatches commands.
mod claude;
mod daemon;
mod http;
mod install;
mod mcp;
mod message;
mod muse;
mod net;
mod os;
mod setup;
mod state;
mod wake;

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::time::Duration;

use message::clean;
use state::{Config, ask, load_config, save_config, try_request};

const DOWNLOAD: &str = "https://github.com/nogu66/nenwa/releases/latest/download";

// The one line that joins by invite. Printed in the form for the OS of the computer that ran it (bash on macOS and Linux, PowerShell on Windows).
fn join_line(code: &str) -> String {
    if cfg!(windows) {
        format!("& ([scriptblock]::Create((irm {DOWNLOAD}/install.ps1))) join {code}")
    } else {
        format!("curl -fsSL {DOWNLOAD}/install.sh | bash -s -- join {code}")
    }
}

const USAGE: &str = "Usage (the first time, the one-line install.sh does everything from install to registration):
  nenwa install                                  All you need: autostart, and registration with Claude Code, Codex and Cursor
                                                   (the first run creates a network that stays inside this computer)
  nenwa grok <webhook URL>                       Connect Grok Bot (the key is entered without being shown; prints the values to enter in Grok Bot)
  nenwa who                                      List the connected agents
  nenwa doctor                                   For each agent, check whether it is installed, whether nenwa is registered,
                                                   and whether a conversation is visible
  nenwa invite                                   Print the line that connects another computer of yours (this turns P2P on)
  nenwa join <invite> [--person name] [--machine name]
                                                   On the other computer: join that network and run install
  nenwa init [--person name] [--machine name]    Recreate the network (install normally creates it for you)
  nenwa send [--as inbox] [--reply-to id] <person>/<session>@<machine> <text>
                                                   Send (as the terminal when --as is not given)
  nenwa inbox [--as inbox] [--wait secs]         Read an inbox (terminal by default)
  nenwa wake <inbox> <webhook URL>               Wake by webhook when a message reaches the inbox (the key is entered without being shown)
  nenwa wake <inbox> --import <JSON>             Set it up from a { url, key } JSON file
  nenwa wake <inbox> --test                      Wake it once as a test
  nenwa muse [--hours n] [--key-out file]        Connect Muse: create the muse inbox and a key (copied to the clipboard), and open the HTTP
                                                   endpoint through Tailscale Funnel (closes after 2 hours by default; 0 keeps it open)
  nenwa muse --off                               Close the Muse endpoint and Funnel and delete the key (messages in the inbox are kept)
  nenwa daemon                                   Run the daemon in the foreground (it normally starts by itself)
  nenwa mcp [--as inbox | --agent <name>]        MCP server (started by each agent; the names are codex, cursor, gemini, antigravity,
                                                   qwen, opencode, copilot, pi)
  nenwa hook <name> <event>                      Hook (started by Codex, Cursor, Gemini CLI, Qwen Code and the OpenCode plugin)
  nenwa --version                                Print the version
  nenwa setup [--remove]                         Redo (or remove) only the registration with the installed agents (Codex, Cursor, OpenCode, Pi,
                                                   Gemini CLI, Antigravity CLI, Qwen Code, Copilot CLI)
  nenwa uninstall                                Remove autostart and the registration with each agent (the team and inboxes are kept)";

// Takes out --name value. If there is no value, leaves it as it is.
fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| *a == format!("--{name}"))?;
    let v = args.get(i + 1).filter(|v| !v.is_empty())?.clone();
    args.drain(i..i + 2);
    Some(v)
}

// While network is false, it does not connect to P2P and works only inside this computer.
pub fn new_config(args: &mut Vec<String>, team_key: String, network: bool) -> Result<Config> {
    let old = load_config();
    let person = clean(&flag(args, "person").unwrap_or_else(os::user_name));
    let host = os::host_name();
    let machine = clean(&flag(args, "machine").unwrap_or_else(|| host.split('.').next().unwrap_or("").to_string()));
    // The machine key (seed) is not recreated. Even if the network changes, it is seen as the same computer.
    let seed = old.map(|c| c.seed).unwrap_or_else(|| state::hex(&state::random_bytes::<32>()));
    let c = Config { person, machine, team_key, seed, network: Some(network) };
    save_config(&c)?;
    Ok(c)
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

// Of the fields to send, include only those that have a value (the same as JSON.stringify dropping undefined)
fn req(pairs: &[(&str, Option<Value>)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            m.insert((*k).into(), v.clone());
        }
    }
    Value::Object(m)
}

// The most important thing for a hook is never to stop the agent. Without a daemon it exits without output (and does not start one).
fn hook(agent: Option<&String>, event: Option<&String>) -> ! {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut raw = String::new();
        let _ = std::io::stdin().read_to_string(&mut raw);
        let _ = tx.send(raw);
    });
    let raw = rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default();
    let input: Value = serde_json::from_str(if raw.trim().is_empty() { "{}" } else { &raw }).unwrap_or(json!({}));
    let r = req(&[
        ("op", Some(json!("hook"))),
        ("agent", agent.map(|a| json!(a))),
        ("event", event.map(|e| json!(e))),
        ("input", Some(input)),
        ("ancestors", Some(json!(os::ancestors()))),
    ]);
    if let Some(res) = try_request(&r)
        && res["ok"] == true
        && !res["result"].is_null()
    {
        println!("{}", res["result"]);
    }
    std::process::exit(0)
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if args.is_empty() { String::new() } else { args.remove(0) };
    // When the output closes first, as in `nenwa who | head`, exit quietly (the daemon keeps the default so that it does not die when writing to a peer that has gone)
    #[cfg(unix)]
    if cmd != "daemon" {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match cmd.as_str() {
        "init" => {
            let c = new_config(&mut args, state::hex(&state::random_bytes::<32>()), false)?;
            install::restart_daemon();
            println!("Created a network ({}@{}, inside this computer only)", c.person, c.machine);
        }
        "invite" => {
            let Some(mut c) = load_config() else { bail!("nenwa is not set up yet. Run nenwa install first") };
            if c.network == Some(false) {
                c.network = Some(true);
                save_config(&c)?;
                install::restart_daemon();
                println!("Turned P2P on (this computer now starts looking for other computers)\n");
            }
            let addr: net::PeerAddr = serde_json::from_value(ask(json!({ "op": "addr" }))?)?;
            let key: [u8; 32] = state::unhex(&c.team_key)
                .and_then(|k| k.try_into().ok())
                .ok_or_else(|| anyhow::anyhow!("teamKey in config.json is not valid"))?;
            println!(
                "Run this line on your other computer (it contains the team key, so do not publish it):\n\n  {}",
                join_line(&net::encode_invite(&key, &addr))
            );
        }
        "join" => {
            let code = if args.is_empty() { String::new() } else { args.remove(0) };
            let Some((key, inviter)) = net::decode_invite(&code) else { bail!("The invite string is not valid") };
            let c = new_config(&mut args, state::hex(&key), true)?;
            // Remember the inviter's address. Once connected, it is rewritten with the name and the new address.
            let mut peers = net::load_peers();
            peers.insert(
                inviter.id.clone(),
                net::Known { relay: inviter.relay, addrs: inviter.addrs, seen: state::now_iso(), ..Default::default() },
            );
            net::save_peers(&peers);
            println!("Joined the network ({}@{})", c.person, c.machine);
            // In tests, do not touch the real autostart or the Claude Code registration
            if std::env::var("NENWA_TEST_NO_INSTALL").is_ok_and(|v| !v.is_empty()) {
                install::restart_daemon();
                return Ok(());
            }
            install::install()?;
        }
        "grok" => install::grok(args.first())?,
        "who" => println!("{}", mcp::format_who(&ask(json!({ "op": "who" }))?)),
        "send" => {
            let as_ = flag(&mut args, "as");
            let reply_to = flag(&mut args, "reply-to");
            let to = if args.is_empty() { None } else { Some(args.remove(0)) };
            let body = args.join(" ");
            print(&ask(req(&[
                ("op", Some(json!("send"))),
                ("as", as_.map(Value::from)),
                ("reply_to", reply_to.map(Value::from)),
                ("to", to.map(Value::from)),
                ("body", Some(json!(body))),
            ]))?);
        }
        "inbox" => {
            let name = flag(&mut args, "as").unwrap_or_else(|| "terminal".into());
            let wait = flag(&mut args, "wait").map_or(0.0, |w| w.trim().parse::<f64>().unwrap_or(f64::NAN));
            let wait = if wait.is_nan() { Value::Null } else { json!(wait) };
            print(&ask(json!({ "op": "inbox", "name": name, "wait": wait }))?);
        }
        "wake" => wake::wake_command(&args)?,
        "muse" => muse::muse_command(&mut args)?,
        "daemon" => {
            let rt = tokio::runtime::Runtime::new()?;
            if let Err(e) = rt.block_on(daemon::start()) {
                // A second daemon exits normally. Autostart restarts it only after a failure, so it is not started over and over.
                if e.downcast_ref::<daemon::AlreadyRunning>().is_none() {
                    return Err(e);
                }
                eprintln!("{e}");
            }
        }
        "mcp" => {
            // Peers other than Claude Code (Grok Bot and so on) are named with --as <inbox> or the NENWA_AS environment variable
            let env = |k: &str| std::env::var(k).unwrap_or_default();
            let as_ = flag(&mut args, "as").unwrap_or_else(|| env("NENWA_AS"));
            let agent = flag(&mut args, "agent").unwrap_or_else(|| env("NENWA_AGENT"));
            mcp::run_mcp(as_, agent);
        }
        "hook" => hook(args.first(), args.get(1)),
        "install" => install::install()?,
        "setup" => {
            install::report_setup(args.iter().any(|a| a == "--remove"));
        }
        "uninstall" => install::uninstall(),
        "doctor" => install::doctor(),
        "--version" | "-V" | "version" => println!("nenwa {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(if cmd.is_empty() { 0 } else { 1 });
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
