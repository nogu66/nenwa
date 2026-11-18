// Mac と Linux で同じところ。
use std::ffi::CStr;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

// デーモンの手元の通信の場所
pub fn sock_path(nenwa_home: &Path) -> PathBuf {
    nenwa_home.join("sock")
}

pub fn alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // シグナル 0 は送らずに確かめるだけ。権限がないだけなら生きている。
    let ok = unsafe { libc::kill(pid as i32, 0) } == 0;
    ok || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn own_parent_pid() -> i64 {
    std::os::unix::process::parent_id() as i64
}

// フックのコマンドの囲み
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn user_name() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_name.is_null() {
            return CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        }
    }
    std::env::var("USER").unwrap_or_default()
}

pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

// 権限 600 で書く（作るときだけ効く。Node の writeFileSync の mode と同じ）
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(text.as_bytes())
}

pub fn make_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

pub fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

// 裏で動かすプロセスを、呼んだ側の端末から切り離す
pub fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

// SIGINT か SIGTERM が来るまで待つ
pub async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = int.recv() => {}
        _ = term.recv() => {}
    }
}

// Claude Code のセッションの受信ソケットに 1 行書いて閉じる。相手が閉じるまで待つ。
pub async fn post_line(socket_path: &str, line: &str) -> std::io::Result<()> {
    let mut c = tokio::net::UnixStream::connect(socket_path).await?;
    c.write_all(line.as_bytes()).await?;
    c.shutdown().await?;
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut rest)).await;
    Ok(())
}
