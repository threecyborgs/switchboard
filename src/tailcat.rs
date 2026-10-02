//! Everything that touches tailcat: keys, the hub's tunnel (restarted when the allow list changes), the one-time
//! enrollment listener that exists only while an invite is open, and a peer's forward.

use crate::config::user_home;
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The hub's saved tailcat server key: its address stays the same across restarts.
pub const HUB_KEY: &str = "switchboard";
/// A second key for the enrollment listener, which lets any client in but only to spend an invite.
pub const ENROLL_KEY: &str = "switchboard-enroll";

pub fn hub_addr_file(home: &Path) -> PathBuf {
    home.join("hub.addr")
}

pub fn enroll_addr_file(home: &Path) -> PathBuf {
    home.join("enroll.addr")
}

pub fn read_addr(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// tailcat on PATH, or in the usual install folders (a launchd service gets a bare PATH).
pub fn find() -> Option<String> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin", "/home/linuxbrew/.linuxbrew/bin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(user_home().join("go/bin"));
    let name = if cfg!(windows) { "tailcat.exe" } else { "tailcat" };
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

/// Find tailcat, installing it with Homebrew when that is the obvious way.
pub fn find_or_install(quiet: bool) -> Result<String> {
    if let Some(t) = find() {
        return Ok(t);
    }
    let brew = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew", "/home/linuxbrew/.linuxbrew/bin/brew"]
        .into_iter()
        .find(|p| Path::new(p).is_file());
    if let Some(brew) = brew {
        if !quiet {
            println!("  tailcat is not installed; installing it with Homebrew (brew install tailcat)...");
        }
        let ok = Command::new(brew).args(["install", "tailcat"]).status()?.success();
        if ok {
            if let Some(t) = find() {
                return Ok(t);
            }
        }
    }
    bail!("tailcat is not installed. Install it (macOS/Linux: `brew install tailcat`; otherwise \
           `go install github.com/tailscale/tailcat@latest`), then run this again.")
}

fn keys(bin: &str) -> Result<Vec<String>> {
    let out = Command::new(bin).args(["genkey", "--list"]).output().context("running tailcat genkey --list")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect())
}

/// Make sure a saved server key exists. `--fixed-region` bakes the relay region into the address, so it never
/// changes even if a later start would pick a different region.
pub fn ensure_server_key(bin: &str, name: &str) -> Result<()> {
    if keys(bin)?.iter().any(|k| k == name) {
        return Ok(());
    }
    let out = Command::new(bin).args(["genkey", &format!("--key={name}"), "--fixed-region"]).output()?;
    if !out.status.success() {
        bail!("tailcat genkey --key={name} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Make sure this machine has a tailcat client key, and return its public key (`nodekey:...`).
pub fn ensure_client_key(bin: &str) -> Result<String> {
    if !keys(bin)?.iter().any(|k| k == "client-default") {
        let out = Command::new(bin).args(["genkey", "--client", "--key=client-default"]).output()?;
        if !out.status.success() {
            bail!("tailcat genkey --client failed: {}", String::from_utf8_lossy(&out.stderr));
        }
    }
    let out = Command::new(bin).arg("printpub").output()?;
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !key.starts_with("nodekey:") {
        bail!("tailcat printpub gave {key:?}");
    }
    Ok(key)
}

/// Stop a tailcat we started earlier whose pid file outlived us (a crash, a kill -9).
fn kill_stale(pidfile: &Path) {
    let Ok(pid) = std::fs::read_to_string(pidfile) else { return };
    let pid = pid.trim();
    if pid.is_empty() {
        return;
    }
    #[cfg(unix)]
    {
        let cmd = Command::new("ps").args(["-p", pid, "-o", "command="]).output();
        if let Ok(o) = cmd {
            if String::from_utf8_lossy(&o.stdout).contains("tailcat") {
                let _ = Command::new("kill").arg(pid).status();
            }
        }
    }
    let _ = std::fs::remove_file(pidfile);
}

/// Handles the hub keeps to tell the tunnel supervisor something changed.
#[derive(Clone)]
pub struct HubTunnel {
    pub peers_changed: Arc<tokio::sync::Notify>,
    pub invite_made: Arc<tokio::sync::Notify>,
}

/// Run the hub's tailcat servers for as long as the process lives:
/// * main: `tailcat serve --key=switchboard --allow=<enrolled peers> PORT`, restarted when a peer is added;
/// * enrollment: `tailcat serve --key=switchboard-enroll -- switchboard enroll-stdio`, only while an invite is open.
pub fn spawn_hub_tunnel(store: Arc<Store>, home: PathBuf, bin: String, port: u16) -> HubTunnel {
    let t = HubTunnel {
        peers_changed: Arc::new(tokio::sync::Notify::new()),
        invite_made: Arc::new(tokio::sync::Notify::new()),
    };
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "switchboard".into());

    // main listener
    {
        let (store, home, bin, changed) = (store.clone(), home.clone(), bin.clone(), t.peers_changed.clone());
        tokio::spawn(async move {
            let pidfile = home.join("tailcat-hub.pid");
            kill_stale(&pidfile);
            let mut backoff = Duration::from_secs(2);
            loop {
                let allow = store.peers().map(|p| p.into_iter().map(|p| p.key).collect::<Vec<_>>()).unwrap_or_default();
                let allow_arg = if allow.is_empty() { "none".to_string() } else { allow.join(",") };
                let started = Instant::now();
                let child = tokio::process::Command::new(&bin)
                    .args(["serve", &format!("--key={HUB_KEY}"), &format!("--allow={allow_arg}"), &port.to_string()])
                    .env("TAILCAT_ADDR_FILE", hub_addr_file(&home))
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .spawn();
                let mut child = match child {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("switchboard: cannot start tailcat ({bin}): {e}; retrying in 10s");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                };
                if let Some(pid) = child.id() {
                    let _ = std::fs::write(&pidfile, pid.to_string());
                }
                eprintln!("switchboard: tailcat serving port {port} to {} peer(s)", allow.len());
                tokio::select! {
                    st = child.wait() => {
                        eprintln!("switchboard: tailcat exited ({st:?}); restarting in {}s", backoff.as_secs());
                        if started.elapsed() > Duration::from_secs(60) { backoff = Duration::from_secs(2); }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                    _ = changed.notified() => {
                        eprintln!("switchboard: peers changed; restarting tailcat with the new allow list");
                        let _ = child.kill().await;
                    }
                }
            }
        });
    }

    // enrollment listener
    {
        let (home, bin, made) = (home.clone(), bin.clone(), t.invite_made.clone());
        tokio::spawn(async move {
            let pidfile = home.join("tailcat-enroll.pid");
            kill_stale(&pidfile);
            let mut child: Option<tokio::process::Child> = None;
            loop {
                let want = store.pending_invites().unwrap_or(0) > 0;
                if let Some(c) = child.as_mut() {
                    if let Ok(Some(st)) = c.try_wait() {
                        eprintln!("switchboard: enrollment listener exited ({st})");
                        child = None;
                    }
                }
                match (want, child.is_some()) {
                    (true, false) => {
                        let spawned = tokio::process::Command::new(&bin)
                            .args(["serve", &format!("--key={ENROLL_KEY}"), "--", &exe, "enroll-stdio"])
                            .env("TAILCAT_ADDR_FILE", enroll_addr_file(&home))
                            .env("SWITCHBOARD_HOME", &home)
                            .stdin(Stdio::null())
                            .kill_on_drop(true)
                            .spawn();
                        match spawned {
                            Ok(c) => {
                                if let Some(pid) = c.id() {
                                    let _ = std::fs::write(&pidfile, pid.to_string());
                                }
                                eprintln!("switchboard: enrollment listener up (an invite is open)");
                                child = Some(c);
                            }
                            Err(e) => eprintln!("switchboard: cannot start the enrollment listener: {e}"),
                        }
                    }
                    (false, true) => {
                        if let Some(mut c) = child.take() {
                            let _ = c.kill().await;
                        }
                        let _ = std::fs::remove_file(&pidfile);
                        eprintln!("switchboard: enrollment listener down (no open invites)");
                    }
                    _ => {}
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), made.notified()).await;
            }
        });
    }
    t
}

/// A peer's side: forward 127.0.0.1:local to the hub, forever. tailcat's forward does not recover by itself when the
/// hub restarts (its tunnel goes stale while the local port keeps accepting), so we probe the hub through it every
/// 5 s and restart the forward after two failed probes in a row.
pub fn run_forward(bin: &str, home: &Path, addr: &str, local_port: u16, hub_port: u16) -> ! {
    let pidfile = home.join("tailcat-forward.pid");
    kill_stale(&pidfile);
    let probe = ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build();
    let url = format!("http://127.0.0.1:{local_port}/healthz");
    let mut backoff = Duration::from_secs(2);
    loop {
        let started = Instant::now();
        let mut child = match Command::new(bin)
            .args(["forward", addr, &format!("{local_port}:{hub_port}")])
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("switchboard: cannot start tailcat ({bin}): {e}; retrying in 10s");
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        let _ = std::fs::write(&pidfile, child.id().to_string());
        eprintln!("switchboard: forwarding 127.0.0.1:{local_port} to the hub");
        let (mut fails, mut ever_up) = (0, false);
        loop {
            std::thread::sleep(Duration::from_secs(if ever_up { 5 } else { 3 }));
            if let Ok(Some(st)) = child.try_wait() {
                eprintln!("switchboard: tailcat forward exited ({st})");
                break;
            }
            match probe.get(&url).call() {
                Ok(_) => {
                    if !ever_up || fails > 0 {
                        eprintln!("switchboard: hub reachable through the tunnel");
                    }
                    ever_up = true;
                    fails = 0;
                    backoff = Duration::from_secs(2);
                }
                Err(e) => {
                    fails += 1;
                    let limit = if ever_up { 2 } else { 10 };
                    if fails >= limit {
                        eprintln!("switchboard: hub unreachable through the tunnel ({e}); restarting the forward");
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        if started.elapsed() > Duration::from_secs(120) {
            backoff = Duration::from_secs(2);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Spend an invite: connect to the hub's enrollment listener with this machine's client key and trade the secret for
/// a token. The listener learns our key from the tunnel itself, so nobody copies node keys around.
pub fn enroll_remote(bin: &str, enroll_addr: &str, secret: &str) -> Result<Value> {
    let mut child = Command::new(bin)
        .args([enroll_addr, "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting tailcat")?;
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", serde_json::json!({"secret": secret}))?;
    stdin.flush()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let r = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(r);
    });
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut s);
        s
    });
    let got = rx.recv_timeout(Duration::from_secs(90));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let line = match got {
        Ok(Ok(l)) if !l.trim().is_empty() => l,
        _ => {
            let e = errs.join().unwrap_or_default();
            let tail: Vec<&str> = e.lines().rev().take(4).collect();
            bail!("the hub's enrollment listener did not answer. Is the invite still open, and is the hub running? \
                   ({})", tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
        }
    };
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| anyhow!("bad enrollment reply {line:?}: {e}"))?;
    if v["ok"].as_bool() != Some(true) {
        bail!("the hub refused: {}", v["error"].as_str().unwrap_or("unknown error"));
    }
    Ok(v)
}
