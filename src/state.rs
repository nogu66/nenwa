// 状態ファイルの場所と、デーモンへの問い合わせ（1 行 1 JSON の手元の通信）。
use anyhow::{Result, bail};
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::os;

pub fn home() -> PathBuf {
    match std::env::var_os("NENWA_HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => os::home_dir().join(".nenwa"),
    }
}
pub fn sock() -> PathBuf {
    os::sock_path(&home())
}
pub fn config_file() -> PathBuf {
    home().join("config.json")
}
pub fn outbox_file() -> PathBuf {
    home().join("outbox.json")
}
pub fn log_file() -> PathBuf {
    home().join("daemon.log")
}
pub fn inbox_dir() -> PathBuf {
    home().join("inbox")
}
pub fn mailboxes_file() -> PathBuf {
    home().join("mailboxes.json")
}
pub fn agents_file() -> PathBuf {
    home().join("agents.json")
}
pub fn peers_file() -> PathBuf {
    home().join("peers.json")
}
pub fn wake_dir() -> PathBuf {
    home().join("wake")
}

// 動いている実行ファイルの場所。フック、MCP、自動起動の登録に書く。
pub fn exe() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).unwrap_or_else(|| PathBuf::from("nenwa"))
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub person: String,
    pub machine: String,
    #[serde(rename = "teamKey")]
    pub team_key: String,
    pub seed: String,
    // false のあいだは P2P につながない。古い config.json には無い
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
}

pub fn load_config() -> Option<Config> {
    serde_json::from_str(&std::fs::read_to_string(config_file()).ok()?).ok()
}

pub fn save_config(c: &Config) -> Result<()> {
    os::make_private_dir(&home())?;
    os::write_private(&config_file(), &(serde_json::to_string_pretty(c)? + "\n"))?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

// JSON.stringify(x, null, 2) と同じ形（末尾の改行なし）で、権限 600 で書く
pub fn write_json_private(path: &Path, v: &impl Serialize) {
    if let Ok(text) = serde_json::to_string_pretty(v) {
        let _ = os::write_private(path, &text);
    }
}

// ---- 時刻と ID ----
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// 2026-11-18T02:03:04.567Z の形（Date の toISOString と同じ）
pub fn iso(ms: u64) -> String {
    let (secs, milli) = (ms / 1000, ms % 1000);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // 日数から年月日（Howard Hinnant の civil_from_days）
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn now_iso() -> String {
    iso(now_ms())
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    rand::random()
}

// randomUUID と同じ形（v4）
pub fn uuid() -> String {
    let mut b: [u8; 16] = random_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

// ---- デーモンへの問い合わせ ----
fn once(req: &Value) -> std::io::Result<Value> {
    let name = sock().into_os_string().to_fs_name::<GenericFilePath>()?;
    let mut c = Stream::connect(name)?;
    c.write_all((req.to_string() + "\n").as_bytes())?;
    let mut r = BufReader::new(c);
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "no reply"));
        }
        if !line.trim().is_empty() {
            return serde_json::from_str(line.trim()).map_err(std::io::Error::other);
        }
    }
}

// デーモンが動いていれば聞き、動いていなければ None（起動はしない）。
pub fn try_request(req: &Value) -> Option<Value> {
    once(req).ok()
}

// デーモンが動いていなければ裏で起動してから聞く。
pub fn request(req: &Value) -> Result<Value> {
    match once(req) {
        Ok(v) => return Ok(v),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {}
        Err(e) => return Err(e.into()),
    }
    if load_config().is_none() {
        bail!("まだチームがありません。nenwa init か nenwa join <招待> を先に実行してください");
    }
    spawn_daemon()?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(v) = once(req) {
            return Ok(v);
        }
    }
    bail!("デーモンが起動しませんでした。{} を見てください", log_file().display())
}

fn spawn_daemon() -> Result<()> {
    os::make_private_dir(&home())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_file())?;
    let mut cmd = std::process::Command::new(exe());
    cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    os::detach(&mut cmd);
    cmd.spawn()?;
    Ok(())
}

// 問い合わせて、ok でなければそのエラーにする
pub fn ask(req: Value) -> Result<Value> {
    let res = request(&req)?;
    if res["ok"] != json!(true) {
        bail!("{}", res["error"].as_str().unwrap_or("error"));
    }
    Ok(res.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 時刻の形() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_795_490_584_567), "2026-11-24T03:23:04.567Z");
        assert_eq!(uuid().len(), 36);
    }
}
