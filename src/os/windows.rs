// Windows: 手元の通信は名前付きパイプ、自動起動はタスク スケジューラ（ログオン時）。
// プロセスは OpenProcess、親プロセスは Toolhelp で調べる。
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, GetLastError, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, STILL_ACTIVE, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    FreeConsole, GetConsoleProcessList, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

const TASK: &str = "nenwa";

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default())
}

pub fn user_name() -> String {
    std::env::var("USERNAME").unwrap_or_default()
}

pub fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

fn default_home() -> PathBuf {
    home_dir().join(".nenwa")
}

// 名前付きパイプ \\.\pipe\nenwa-<ユーザー>。NENWA_HOME がふつうの場所でなければ、その場所ごとに別のパイプにする。
pub fn sock_path(nenwa_home: &Path) -> PathBuf {
    let mut name = format!(r"\\.\pipe\nenwa-{}", user_name());
    if nenwa_home != default_home() {
        let h = Sha256::digest(nenwa_home.to_string_lossy().as_bytes());
        name += &format!("-{}", h.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>());
    }
    PathBuf::from(name)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// パイプはサーバーがいる間だけある（ファイルのように残らない）。つながずに確かめる。
pub fn sock_exists(path: &Path) -> bool {
    unsafe { WaitNamedPipeW(wide(&path.to_string_lossy()).as_ptr(), 1) != 0 }
}

pub fn remove_sock(_path: &Path) {}

pub fn alive(pid: i64) -> bool {
    if pid <= 0 || pid > u32::MAX as i64 {
        return false;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            // 権限がないだけなら生きている
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
}

pub fn parent_pid(pid: i64) -> Option<i64> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more {
            if e.th32ProcessID as i64 == pid {
                found = Some(e.th32ParentProcessID as i64);
                break;
            }
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
    }
}

pub fn own_parent_pid() -> i64 {
    parent_pid(unsafe { GetCurrentProcessId() } as i64).unwrap_or(0)
}

// フックのコマンドの囲み（Windows のパスに " は入らない）
pub fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

// ユーザーのフォルダの中は、ほかのユーザーからは見えない（権限はそのまま）
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
}

pub fn make_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

pub fn set_private(_path: &Path) {}

pub fn exe_path() -> Option<PathBuf> {
    // canonicalize は \\?\ を付けてしまうので使わない
    std::env::current_exe().ok()
}

// Windows には lsof がない。shutdown を知らない古いデーモンはいない（Windows 版はこの版から）。
pub fn stop_socket_holders(_sock: &Path) {}

// 裏で動かすプロセスを、呼んだ側の端末から切り離す（窓も出さない）
pub fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    // 子は、引き継げるハンドルをすべて受け取る。自分の標準入出力がパイプだと、デーモンがそれを握り続け、
    // 出力を待っている呼び出し元（フックを起動した Codex など）がいつまでも終わらない。引き継ぎを外しておく。
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let h = GetStdHandle(id);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

// タスク スケジューラから起動されると、自分だけの窓ができる。その窓を閉じて、ログは daemon.log に書く。
pub fn prepare_daemon(log: &Path) {
    let mut list = [0u32; 4];
    if unsafe { GetConsoleProcessList(list.as_mut_ptr(), list.len() as u32) } != 1 {
        return;
    }
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(log) else { return };
    use std::os::windows::io::IntoRawHandle;
    let h = f.into_raw_handle();
    unsafe {
        FreeConsole();
        SetStdHandle(STD_ERROR_HANDLE, h);
        SetStdHandle(STD_OUTPUT_HANDLE, h);
    }
}

// Ctrl+C、窓を閉じた、ログオフ、シャットダウンのどれかが来るまで待つ
pub async fn terminated() {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
    let (Ok(mut a), Ok(mut b), Ok(mut c), Ok(mut d)) = (ctrl_c(), ctrl_break(), ctrl_close(), ctrl_shutdown()) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = a.recv() => {}
        _ = b.recv() => {}
        _ = c.recv() => {}
        _ = d.recv() => {}
    }
}

// Claude Code のセッションの受信口に 1 行書いて閉じる。Windows では名前付きパイプ（\\.\pipe\…）。
pub async fn post_line(socket_path: &str, line: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ClientOptions;
    if !(socket_path.starts_with(r"\\.\pipe\") || socket_path.starts_with(r"\\?\pipe\")) {
        return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, format!("名前付きパイプではありません: {socket_path}")));
    }
    // ほかの人が書いている間はパイプが空かない。少し待ってやり直す。
    let mut tries = 0;
    let mut c = loop {
        match ClientOptions::new().open(socket_path) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(231) && tries < 50 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    };
    c.write_all(line.as_bytes()).await?;
    c.flush().await?;
    Ok(())
}

// ---- 自動起動（タスク スケジューラ、ログオン時） ----
fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("schtasks").args(args).stdin(Stdio::null()).output()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// タスクの XML。時間の上限なし、電池でも止めない、落ちたら 1 分後に起こし直す。
fn task_xml(exe: &Path, user: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>nenwa daemon</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author"><Exec><Command>{}</Command><Arguments>daemon</Arguments></Exec></Actions>
</Task>
"#,
        xml_escape(&exe.to_string_lossy()),
        user = xml_escape(user)
    )
}

pub fn autostart_install(exe: &Path, nenwa_home: &Path, _log: &Path) -> anyhow::Result<String> {
    // タスクには環境変数を渡せないので、ふつうの場所（%USERPROFILE%\.nenwa）だけにする
    if nenwa_home != default_home() {
        anyhow::bail!("Windows の自動起動は、NENWA_HOME がふつうの場所（{}）のときだけ使えます", default_home().display());
    }
    let user = match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{}", user_name()),
        _ => user_name(),
    };
    // schtasks の /XML は UTF-16（BOM つき）で読む
    let file = std::env::temp_dir().join("nenwa-task.xml");
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(task_xml(exe, &user).encode_utf16().flat_map(|u| u.to_le_bytes()));
    std::fs::write(&file, bytes)?;
    let out = schtasks(&["/Create", "/TN", TASK, "/XML", &file.to_string_lossy(), "/F"])?;
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        anyhow::bail!("タスク スケジューラへの登録に失敗しました: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let _ = schtasks(&["/Run", "/TN", TASK]);
    Ok(format!("タスク スケジューラの {TASK}"))
}

pub fn autostart_remove() {
    let _ = schtasks(&["/End", "/TN", TASK]);
    let _ = schtasks(&["/Delete", "/TN", TASK, "/F"]);
}

// タスクに入っていて、ふつうの場所を使っているときだけ、止めてからタスクで起動し直す。
pub fn autostart_restart(nenwa_home: &Path) -> bool {
    if nenwa_home != default_home() || !schtasks(&["/Query", "/TN", TASK]).is_ok_and(|o| o.status.success()) {
        return false;
    }
    if crate::state::try_request(&serde_json::json!({ "op": "shutdown" })).is_some() {
        for _ in 0..30 {
            if !sock_exists(&sock_path(nenwa_home)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    schtasks(&["/Run", "/TN", TASK]).is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 親プロセスとパイプの名前() {
        assert!(alive(unsafe { GetCurrentProcessId() } as i64));
        assert!(own_parent_pid() > 0);
        let p = sock_path(Path::new(r"C:\tmp\x")).to_string_lossy().into_owned();
        assert!(p.starts_with(r"\\.\pipe\nenwa-") && p.len() > r"\\.\pipe\nenwa-".len() + 16, "{p}");
        assert!(task_xml(Path::new(r"C:\a&b\nenwa.exe"), "D\\u").contains(r"<Command>C:\a&amp;b\nenwa.exe</Command>"));
    }
}
