// What macOS and Linux share.
use std::ffi::CStr;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

// Where the daemon's local socket is
pub fn sock_path(nenwa_home: &Path) -> PathBuf {
    nenwa_home.join("sock")
}

pub fn alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // Signal 0 only checks without sending. If it is only a permission error, the process is alive.
    let ok = unsafe { libc::kill(pid as i32, 0) } == 0;
    ok || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn own_parent_pid() -> i64 {
    std::os::unix::process::parent_id() as i64
}

// Quoting of hook commands
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

// Write with mode 600 (applies only on creation. Same as mode in Node's writeFileSync)
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

// Detach a background process from the caller's terminal
pub fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

// Wait until SIGINT or SIGTERM arrives
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

// Write one line to a Claude Code session's receiving socket and close. Wait until the other side closes.
pub async fn post_line(socket_path: &str, line: &str) -> std::io::Result<()> {
    let mut c = tokio::net::UnixStream::connect(socket_path).await?;
    c.write_all(line.as_bytes()).await?;
    c.shutdown().await?;
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut rest)).await;
    Ok(())
}

// Whether the local socket exists (a unix socket stays as a file)
pub fn sock_exists(path: &Path) -> bool {
    path.exists()
}

pub fn remove_sock(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// Where the running executable is
pub fn exe_path() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()
}

// An old daemon that does not know shutdown is stopped by sending SIGTERM to the processes holding the socket open.
pub fn stop_socket_holders(sock: &Path) {
    let Ok(out) = std::process::Command::new("lsof").arg("-t").arg(sock).output() else { return };
    for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if let Ok(pid) = pid.parse::<i32>() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

// Preparation before the daemon starts (nothing on macOS and Linux. The autostart config points the log at daemon.log)
pub fn prepare_daemon(_log: &Path) {}

#[cfg(test)]
mod tests {
    #[test]
    fn parent_process_ids() {
        assert_eq!(super::super::parent_pid(std::process::id() as i64), Some(super::own_parent_pid()));
        assert!(super::alive(std::process::id() as i64));
        assert_eq!(super::quote("a'b"), "'a'\\''b'");
    }
}
