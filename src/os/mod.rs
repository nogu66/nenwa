// OS ごとの違いをここに閉じ込める。ほかのファイルは OS を気にしない。
// - 自動起動（Mac は launchd、Linux は systemd --user、Windows はタスク スケジューラ）
// - プロセスが生きているか、親プロセスの番号
// - フックのコマンドの囲み
// - 手元の通信の場所（unix ソケットか名前付きパイプ）

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
