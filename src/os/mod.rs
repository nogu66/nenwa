// OS ごとの違いをここに閉じ込める。ほかのファイルは OS を気にしない。
// - 自動起動（Mac は launchd、Linux は systemd --user、Windows はタスク スケジューラ）
// - プロセスが生きているか、親プロセスの番号
// - フックのコマンドの囲み
// - 手元の通信の場所（unix ソケットか名前付きパイプ）

// 自分の親、その親…と上へたどった番号の並び（近い順）。フックや MCP サーバーが、どのエージェントから起動されたかを伝えるのに使う。
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
