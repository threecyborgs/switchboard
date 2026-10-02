//! Keep `switchboard daemon` running across logouts and crashes: launchd on macOS, a systemd user unit on Linux, a
//! logon task on Windows.

use crate::config::{home, is_default_home};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The service name. A non-default `SWITCHBOARD_HOME` gets its own, so a test hub never replaces the real one.
pub fn label() -> String {
    if is_default_home() {
        "com.threecyborgs.switchboard".into()
    } else {
        let h = crate::store::hash_secret(&home().to_string_lossy());
        format!("com.threecyborgs.switchboard.{}", &h[..8])
    }
}

pub fn log_path() -> PathBuf {
    home().join("switchboard.log")
}

/// A stable copy of this binary under the home, so the service and agents never point into a build folder.
pub fn install_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = home().join("bin");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(if cfg!(windows) { "switchboard.exe" } else { "switchboard" });
    if exe.canonicalize().ok() != dest.canonicalize().ok() {
        let tmp = dest.with_extension("new");
        std::fs::copy(&exe, &tmp)?;
        std::fs::rename(&tmp, &dest)?;
    }
    // Put it on PATH for people and agents, when there is an obvious place and it is the real home.
    #[cfg(unix)]
    if is_default_home() {
        let local = crate::config::user_home().join(".local/bin");
        if local.is_dir() {
            let link = local.join("switchboard");
            if std::fs::symlink_metadata(&link).map(|m| m.file_type().is_symlink()).unwrap_or(true) {
                let _ = std::fs::remove_file(&link);
                let _ = std::os::unix::fs::symlink(&dest, &link);
            }
        }
    }
    Ok(dest)
}

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    crate::config::user_home().join("Library/LaunchAgents").join(format!("{}.plist", label()))
}

#[cfg(target_os = "macos")]
fn uid() -> String {
    String::from_utf8_lossy(&Command::new("id").arg("-u").output().map(|o| o.stdout).unwrap_or_default())
        .trim()
        .to_string()
}

pub fn install(exe: &Path) -> Result<String> {
    let home = home();
    std::fs::create_dir_all(&home)?;
    #[cfg(target_os = "macos")]
    {
        let plist = plist_path();
        std::fs::create_dir_all(plist.parent().unwrap())?;
        let env_home = if is_default_home() {
            String::new()
        } else {
            format!("<key>SWITCHBOARD_HOME</key><string>{}</string>", xml(&home.to_string_lossy()))
        };
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/opt/local/bin:/usr/bin:/bin</string>
    <key>HOME</key><string>{user_home}</string>
    {env_home}
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            label = label(),
            exe = xml(&exe.to_string_lossy()),
            user_home = xml(&crate::config::user_home().to_string_lossy()),
            log = xml(&log_path().to_string_lossy()),
        );
        std::fs::write(&plist, body)?;
        let target = format!("gui/{}", uid());
        let _ = Command::new("launchctl").args(["bootout", &format!("{target}/{}", label())]).output();
        let out = Command::new("launchctl").args(["bootstrap", &target, &plist.to_string_lossy()]).output()?;
        if !out.status.success() {
            bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        return Ok(format!("launchd agent {} ({})", label(), plist.display()));
    }
    #[cfg(target_os = "linux")]
    {
        let unit_name = format!("{}.service", label().replace("com.threecyborgs.", ""));
        let dir = crate::config::user_home().join(".config/systemd/user");
        std::fs::create_dir_all(&dir)?;
        let env_home = if is_default_home() {
            String::new()
        } else {
            format!("Environment=SWITCHBOARD_HOME={}\n", home.display())
        };
        let body = format!(
            "[Unit]\nDescription=switchboard (agent mail hub/peer)\nAfter=network-online.target\n\n[Service]\n\
             ExecStart={} daemon\n{env_home}Environment=PATH=/home/linuxbrew/.linuxbrew/bin:/usr/local/bin:/usr/bin:\
             /bin:{}/go/bin\nRestart=always\nRestartSec=5\nStandardOutput=append:{}\nStandardError=append:{}\n\n\
             [Install]\nWantedBy=default.target\n",
            exe.display(),
            crate::config::user_home().display(),
            log_path().display(),
            log_path().display()
        );
        std::fs::write(dir.join(&unit_name), body)?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
        let ok = Command::new("systemctl").args(["--user", "enable", "--now", &unit_name]).status()?.success();
        if !ok {
            bail!("systemctl --user enable --now {unit_name} failed (no user systemd session?). Run \
                   `switchboard daemon` yourself, or `loginctl enable-linger $USER` and try again.");
        }
        let _ = Command::new("systemctl").args(["--user", "restart", &unit_name]).status();
        return Ok(format!("systemd user unit {unit_name} (runs after logout once `loginctl enable-linger` is on)"));
    }
    #[cfg(windows)]
    {
        let task = label();
        let tr = format!("\"{}\" daemon", exe.display());
        let ok = Command::new("schtasks")
            .args(["/create", "/tn", &task, "/tr", &tr, "/sc", "onlogon", "/rl", "limited", "/f"])
            .status()?
            .success();
        if !ok {
            bail!("schtasks /create failed");
        }
        let _ = Command::new("schtasks").args(["/run", "/tn", &task]).status();
        return Ok(format!("scheduled task {task} (starts at logon)"));
    }
    #[allow(unreachable_code)]
    {
        let _ = exe;
        bail!("no service manager support on this platform; run `switchboard daemon` yourself")
    }
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{}", uid(), label())]).output();
        let _ = std::fs::remove_file(plist_path());
    }
    #[cfg(target_os = "linux")]
    {
        let unit_name = format!("{}.service", label().replace("com.threecyborgs.", ""));
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", &unit_name]).status();
        let _ = std::fs::remove_file(crate::config::user_home().join(".config/systemd/user").join(unit_name));
    }
    #[cfg(windows)]
    {
        let _ = Command::new("schtasks").args(["/delete", "/tn", &label(), "/f"]).status();
    }
    Ok(())
}

/// (installed, running, detail)
pub fn status() -> (bool, bool, String) {
    #[cfg(target_os = "macos")]
    {
        let installed = plist_path().exists();
        let out = Command::new("launchctl").args(["print", &format!("gui/{}/{}", uid(), label())]).output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let running = text.contains("state = running");
        let pid = text.lines().find(|l| l.trim().starts_with("pid =")).map(|l| l.trim().to_string()).unwrap_or_default();
        return (installed, running, format!("launchd {} {pid}", label()));
    }
    #[cfg(target_os = "linux")]
    {
        let unit_name = format!("{}.service", label().replace("com.threecyborgs.", ""));
        let installed = crate::config::user_home().join(".config/systemd/user").join(&unit_name).exists();
        let running = Command::new("systemctl").args(["--user", "is-active", "--quiet", &unit_name])
            .status().map(|s| s.success()).unwrap_or(false);
        return (installed, running, format!("systemd --user {unit_name}"));
    }
    #[allow(unreachable_code)]
    (false, false, "no service manager".into())
}

#[cfg(target_os = "macos")]
fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
