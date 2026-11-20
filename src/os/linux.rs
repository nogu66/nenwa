// Linux: autostart is a systemd --user service. Parent processes are looked up in /proc.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const UNIT: &str = "nenwa.service";

fn unit_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| super::home_dir().join(".config"));
    base.join("systemd/user").join(UNIT)
}

fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl").arg("--user").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

pub fn parent_pid(pid: i64) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The second field (the name) can contain spaces, so count from after the last )
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

// Restart only after a crash (a second daemon exits normally, so it is not started over and over)
fn unit_text(exe: &Path, nenwa_home: &Path, log: &Path) -> String {
    format!(
        "[Unit]\nDescription=nenwa daemon\n\n[Service]\nExecStart=\"{}\" daemon\nEnvironment=\"NENWA_HOME={}\"\nEnvironment=\"PATH={}\"\nRestart=on-failure\nStandardOutput=append:{}\nStandardError=append:{}\n\n[Install]\nWantedBy=default.target\n",
        exe.display(),
        nenwa_home.display(),
        // The systemd --user PATH is minimal, so pass the PATH from registration time so that codex and others are found (% is a systemd specifier, so it is doubled)
        std::env::var("PATH").unwrap_or_default().replace('%', "%%").replace('"', ""),
        log.display(),
        log.display()
    )
}

// Whether the state folder written there is the current one (do not restart the real daemon from tests or another NENWA_HOME)
fn same_home(unit: &str, nenwa_home: &Path) -> bool {
    unit.lines().any(|l| l == format!("Environment=\"NENWA_HOME={}\"", nenwa_home.display()))
}

pub fn autostart_install(exe: &Path, nenwa_home: &Path, log: &Path) -> anyhow::Result<String> {
    let text = unit_text(exe, nenwa_home, log);
    let file = unit_file();
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, text)?;
    systemctl(&["daemon-reload"]);
    if !systemctl(&["enable", "--now", UNIT]) {
        anyhow::bail!("systemctl --user enable failed");
    }
    Ok(file.to_string_lossy().into_owned())
}

pub fn autostart_remove() {
    systemctl(&["disable", "--now", UNIT]);
    let _ = std::fs::remove_file(unit_file());
    systemctl(&["daemon-reload"]);
}

pub fn autostart_restart(nenwa_home: &Path) -> bool {
    match std::fs::read_to_string(unit_file()) {
        Ok(text) if same_home(&text, nenwa_home) => systemctl(&["restart", UNIT]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit() {
        let t = unit_text(Path::new("/home/a/.local/bin/nenwa"), Path::new("/home/a/.nenwa"), Path::new("/home/a/.nenwa/daemon.log"));
        assert!(t.contains("ExecStart=\"/home/a/.local/bin/nenwa\" daemon\n"));
        assert!(t.contains("StandardError=append:/home/a/.nenwa/daemon.log\n"));
        assert!(same_home(&t, Path::new("/home/a/.nenwa")));
        assert!(!same_home(&t, Path::new("/home/a/.nenwa-test")));
    }
}
