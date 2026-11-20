// OS differences are contained here. Other files do not care about the OS.
// - Autostart (launchd on macOS, systemd --user on Linux, Task Scheduler on Windows)
// - Whether a process is alive, and parent process IDs
// - Quoting of hook commands
// - Where the local socket is (a unix socket or a named pipe)

// The IDs found walking up from own parent, its parent and so on (nearest first). Hooks and the MCP server use it to tell which agent started them.
pub fn ancestors() -> Vec<i64> {
    let mut list = vec![];
    let mut pid = own_parent_pid();
    while pid > 1 && list.len() < 8 && !list.contains(&pid) {
        list.push(pid);
        let Some(parent) = parent_pid(pid) else { break };
        pid = parent;
    }
    list
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
