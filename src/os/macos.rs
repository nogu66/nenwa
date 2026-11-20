// macOS: autostart is a launchd LaunchAgent. Parent processes are looked up with ps.
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

// Start the daemon at login. Returns where it was registered.
pub fn autostart_install(exe: &Path, nenwa_home: &Path, log: &Path) -> anyhow::Result<String> {
    let (exe, home, log) =
        (xml_escape(&exe.to_string_lossy()), xml_escape(&nenwa_home.to_string_lossy()), xml_escape(&log.to_string_lossy()));
    // The launchd PATH is only /usr/bin:/bin:/usr/sbin:/sbin, so codex (a Homebrew node script) is not found. Pass the PATH from registration time.
    let path = xml_escape(&std::env::var("PATH").unwrap_or_default());
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key><dict><key>NENWA_HOME</key><string>{home}</string><key>PATH</key><string>{path}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#
    );
    let plist = plist();
    let domain = domain();
    // If the content is the same and it is already registered, only restart it.
    // Rewriting and registering again makes macOS show a "Background Items Added" notification every time.
    if std::fs::read_to_string(&plist).is_ok_and(|old| old == xml)
        && launchctl(&["print", &format!("{domain}/{LABEL}")])
        && launchctl(&["kickstart", "-k", &format!("{domain}/{LABEL}")])
    {
        return Ok(plist.to_string_lossy().into_owned());
    }
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(&plist, xml)?;
    launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    // bootout can return before removal finishes, and a bootstrap right after fails with "5: Input/output error". Wait a little and retry a few times.
    let mut last = String::new();
    for _ in 0..10 {
        let out = Command::new("launchctl").args(["bootstrap", &domain, &plist.to_string_lossy()]).output()?;
        if out.status.success() {
            return Ok(plist.to_string_lossy().into_owned());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    anyhow::bail!("launchctl bootstrap failed: {last}")
}

pub fn autostart_remove() {
    launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    let _ = std::fs::remove_file(plist());
}

// Restart only when it is in autostart and its state folder is the current one
// (do not restart the real daemon from tests or another NENWA_HOME).
pub fn autostart_restart(nenwa_home: &Path) -> bool {
    let want = format!("<string>{}</string>", xml_escape(&nenwa_home.to_string_lossy()));
    match std::fs::read_to_string(plist()) {
        Ok(text) if text.contains(&want) => launchctl(&["kickstart", "-k", &format!("{}/{LABEL}", domain())]),
        _ => false,
    }
}
