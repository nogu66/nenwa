// Linux: 自動起動は systemd --user の service。親プロセスは /proc で調べる。
// （進め方の 2 で実機と Actions で確かめる）
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
    // 2 つ目の項目（名前）には空白が入りうるので、最後の ) のあとから数える
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

pub fn autostart_install(exe: &Path, nenwa_home: &Path, log: &Path) -> anyhow::Result<String> {
    let text = format!(
        "[Unit]\nDescription=nenwa daemon\n\n[Service]\nExecStart=\"{}\" daemon\nEnvironment=\"NENWA_HOME={}\"\nRestart=on-failure\nStandardOutput=append:{}\nStandardError=append:{}\n\n[Install]\nWantedBy=default.target\n",
        exe.display(),
        nenwa_home.display(),
        log.display(),
        log.display()
    );
    let file = unit_file();
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, text)?;
    systemctl(&["daemon-reload"]);
    if !systemctl(&["enable", "--now", UNIT]) {
        anyhow::bail!("systemctl --user enable に失敗しました");
    }
    Ok(file.to_string_lossy().into_owned())
}

pub fn autostart_remove() {
    systemctl(&["disable", "--now", UNIT]);
    let _ = std::fs::remove_file(unit_file());
    systemctl(&["daemon-reload"]);
}

pub fn autostart_restart(nenwa_home: &Path) -> bool {
    let want = format!("NENWA_HOME={}\"", nenwa_home.display());
    match std::fs::read_to_string(unit_file()) {
        Ok(text) if text.contains(&want) => systemctl(&["restart", UNIT]),
        _ => false,
    }
}
