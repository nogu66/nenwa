// Mac: 自動起動は launchd の LaunchAgent。親プロセスは ps で調べる。
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LABEL: &str = "me.nenwa.daemon";

fn plist() -> PathBuf {
    super::home_dir().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

pub fn parent_pid(pid: i64) -> Option<i64> {
    let out = Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ログイン時にデーモンを起動する。登録した場所を返す。
pub fn autostart_install(exe: &Path, nenwa_home: &Path, log: &Path) -> anyhow::Result<String> {
    let (exe, home, log) =
        (xml_escape(&exe.to_string_lossy()), xml_escape(&nenwa_home.to_string_lossy()), xml_escape(&log.to_string_lossy()));
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key><dict><key>NENWA_HOME</key><string>{home}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#
    );
    let plist = plist();
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(&plist, xml)?;
    let domain = domain();
    launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    // bootout は外し終わる前に戻ることがあり、直後の bootstrap が「5: Input/output error」で失敗する。少し待って数回やり直す。
    let mut last = String::new();
    for _ in 0..10 {
        let out = Command::new("launchctl").args(["bootstrap", &domain, &plist.to_string_lossy()]).output()?;
        if out.status.success() {
            return Ok(plist.to_string_lossy().into_owned());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    anyhow::bail!("launchctl bootstrap に失敗しました: {last}")
}

pub fn autostart_remove() {
    launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    let _ = std::fs::remove_file(plist());
}

// 自動起動に入っていて、その状態フォルダが今のものと同じときだけ起動し直す
// （テストや別の NENWA_HOME で本物のデーモンを起こし直さない）。
pub fn autostart_restart(nenwa_home: &Path) -> bool {
    let want = format!("<string>{}</string>", xml_escape(&nenwa_home.to_string_lossy()));
    match std::fs::read_to_string(plist()) {
        Ok(text) if text.contains(&want) => launchctl(&["kickstart", "-k", &format!("{}/{LABEL}", domain())]),
        _ => false,
    }
}
