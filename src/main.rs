// nenwa: 自分のエージェント同士（Claude Code、Codex、Cursor、Grok Bot）がメッセージを送り合えるようにする。
// ここはコマンドの振り分けだけ。
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

// 招待で入る 1 行。実行した人のパソコンと同じ OS の形で出す（Mac と Linux は bash、Windows は PowerShell）。
fn join_line(code: &str) -> String {
    if cfg!(windows) {
        format!("& ([scriptblock]::Create((irm {DOWNLOAD}/install.ps1))) join {code}")
    } else {
        format!("curl -fsSL {DOWNLOAD}/install.sh | bash -s -- join {code}")
    }
}

const USAGE: &str = "使い方（はじめては install.sh の 1 行で、導入から登録まで済む）:
  nenwa install                                  これだけで使える: 自動起動と、Claude Code・Codex・Cursor への登録
                                                   （初回はこのパソコンの中だけのネットワークを作る）
  nenwa grok <webhook の URL>                    Grok Bot をつなぐ（鍵は画面に出さずに入れる。Grok Bot に入れる値を表示）
  nenwa who                                      つながっているエージェントの一覧
  nenwa invite                                   自分の別のパソコンをつなぐ行を出す（ここで P2P をオンにする）
  nenwa join <招待> [--person 名前] [--machine 名前]
                                                   別のパソコンで: そのネットワークに入って install まで行う
  nenwa init [--person 名前] [--machine 名前]   ネットワークを作り直す（ふつうは install が自動で作る）
  nenwa send [--as 受信箱] [--reply-to id] <人>/<セッション>@<マシン> <本文>
                                                   送る（--as なしならターミナルとして）
  nenwa inbox [--as 受信箱] [--wait 秒]           受信箱を読む（既定は terminal）
  nenwa wake <受信箱> <webhook の URL>            受信箱に届いたら webhook で起こす（鍵は画面に出さずに入れる）
  nenwa wake <受信箱> --import <JSON>             { url, key } の JSON から設定する
  nenwa wake <受信箱> --test                      試しに 1 回起こす
  nenwa muse [--hours 時間] [--key-out ファイル]  Muse をつなぐ: 受信箱 muse と鍵（クリップボードへ）を作り、
                                                   HTTP の入り口を Tailscale Funnel で開く（既定は 2 時間で閉じる。0 なら閉じない）
  nenwa muse --off                               Muse の入り口と Funnel を閉じ、鍵を消す（受信箱の中身は残す）
  nenwa daemon                                   デーモンを前で動かす（ふつうは自動で起動）
  nenwa mcp [--as 受信箱 | --agent <名前>]        MCP サーバー（各エージェントが起動する。名前は codex、cursor、gemini、qwen、opencode、copilot）
  nenwa hook <名前> <イベント>                    フック（Codex、Cursor、Gemini CLI、Qwen Code、OpenCode のプラグインが起動する）
  nenwa setup [--remove]                         入っているエージェント（Codex、Cursor、OpenCode、Gemini CLI、Qwen Code、Copilot CLI）への登録だけをやり直す（外す）
  nenwa uninstall                                自動起動と各エージェントへの登録を外す（チームと受信箱は残す）";

// --名前 値 を取り出す。値がなければそのまま残す。
fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| *a == format!("--{name}"))?;
    let v = args.get(i + 1).filter(|v| !v.is_empty())?.clone();
    args.drain(i..i + 2);
    Some(v)
}

// network: false のあいだは P2P につながず、このパソコンの中だけで動く。
pub fn new_config(args: &mut Vec<String>, team_key: String, network: bool) -> Result<Config> {
    let old = load_config();
    let person = clean(&flag(args, "person").unwrap_or_else(os::user_name));
    let host = os::host_name();
    let machine = clean(&flag(args, "machine").unwrap_or_else(|| host.split('.').next().unwrap_or("").to_string()));
    // マシンの鍵（seed）は作り直さない。ネットワークを変えても同じパソコンとして見える。
    let seed = old.map(|c| c.seed).unwrap_or_else(|| state::hex(&state::random_bytes::<32>()));
    let c = Config { person, machine, team_key, seed, network: Some(network) };
    save_config(&c)?;
    Ok(c)
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

// 送る項目のうち、値のあるものだけを入れる（JSON.stringify が undefined を落とすのと同じ）
fn req(pairs: &[(&str, Option<Value>)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            m.insert((*k).into(), v.clone());
        }
    }
    Value::Object(m)
}

// フックはエージェントの動きを止めないことが一番大事。デーモンがなければ何も出さずに終わる（起動もしない）。
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
    match cmd.as_str() {
        "init" => {
            let c = new_config(&mut args, state::hex(&state::random_bytes::<32>()), false)?;
            install::restart_daemon();
            println!("ネットワークを作りました（{}@{}、このパソコンの中だけ）", c.person, c.machine);
        }
        "invite" => {
            let Some(mut c) = load_config() else { bail!("まだ nenwa が入っていません。nenwa install を先に") };
            if c.network == Some(false) {
                c.network = Some(true);
                save_config(&c)?;
                install::restart_daemon();
                println!("P2P をオンにしました（このパソコンもほかのパソコンを探し始めます）\n");
            }
            let addr: net::PeerAddr = serde_json::from_value(ask(json!({ "op": "addr" }))?)?;
            let key: [u8; 32] = state::unhex(&c.team_key)
                .and_then(|k| k.try_into().ok())
                .ok_or_else(|| anyhow::anyhow!("config.json の teamKey が正しくありません"))?;
            println!(
                "自分の別のパソコンで、次の行を実行してください（合言葉が入っているので公開しない）:\n\n  {}",
                join_line(&net::encode_invite(&key, &addr))
            );
        }
        "join" => {
            let code = if args.is_empty() { String::new() } else { args.remove(0) };
            let Some((key, inviter)) = net::decode_invite(&code) else { bail!("招待の文字列が正しくありません") };
            let c = new_config(&mut args, state::hex(&key), true)?;
            // 招いた側の住所を覚えておく。つながったら名前と新しい住所で書き直す。
            let mut peers = net::load_peers();
            peers.insert(
                inviter.id.clone(),
                net::Known { relay: inviter.relay, addrs: inviter.addrs, seen: state::now_iso(), ..Default::default() },
            );
            net::save_peers(&peers);
            println!("ネットワークに入りました（{}@{}）", c.person, c.machine);
            // テストでは本物の自動起動や Claude Code の登録に触らない
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
                // 2 つ目は正常に終わる。自動起動は失敗したときだけ起こし直すので、繰り返し起動されない。
                if e.downcast_ref::<daemon::AlreadyRunning>().is_none() {
                    return Err(e);
                }
                eprintln!("{e}");
            }
        }
        "mcp" => {
            // Claude Code 以外（Grok Bot など）は --as <受信箱> か環境変数 NENWA_AS で名前を付ける
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
